//! 更新防护：关掉 WorkBuddy 自己的自动更新。
//!
//! ## 为什么需要它
//!
//! 本机同时装着国内版和国际版，而 WorkBuddy 的**更新缓存是按用户放的、不分产品**：
//!
//! ```text
//! %LOCALAPPDATA%\@genieworkbuddy-desktop-updater\installer.exe
//! ```
//!
//! 任一版本下载的安装包都躺在这个**共用**目录里，另一个版本启动时看到「版本号更高」
//! 就直接套用，**不校验包属于哪个产品** —— 于是 A 版的程序目录被 B 版覆盖。
//! 实测已发生两次：2026-10-01 国际版被国内包覆盖；2026-10-08 两个独立安装目录
//! 被合并成一个，国内版身份彻底消失（快捷方式全指向同一目录，且那份是国际版身份）。
//!
//! ## 机制（从 app.asar 里读出来的，不是猜的）
//!
//! 更新源地址的三级优先级（`AbstractUpdateService.buildUpdateFeedUrl`）：
//!
//! ```text
//! process.env.WORKBUDDY_UPDATE_URL          ← 最高优先级，本模块用的就是这个
//!   || updateBaseUrlProvider.getUpdateBaseUrl()   （读运行时租户 endpoint）
//!   || "https://copilot.tencent.com"              （硬编码兜底）
//! ```
//!
//! 拼出来的地址形如 `<base>/v2/update?platform=workbuddy-{os}-{arch}&version={ver}&...`。
//! 把环境变量指到一个**黑洞地址**，更新源就永远拉不到东西，自然不会再下载新包。
//!
//! ## 为什么只设环境变量还不够
//!
//! 已经下载到缓存里的包**不看 URL**，启动时照样会被应用。所以本模块做两件事：
//!
//! 1. 设/删用户级 `WORKBUDDY_UPDATE_URL`（拦住后续下载）
//! 2. 把缓存里已下好的 `installer.exe*` 改名隔离（拦住已下载的包被应用）
//!
//! ## 作用域说明
//!
//! 环境变量是**用户级**的，两个版本读的是同一个变量名 —— 所以这是一个**总开关**，
//! 开了就是两个版本都不更新。这也正是想要的：风险本来就来自两者共用缓存。
//! 界面上的「国内版 / 国际版」两张卡片是**只读状态展示**，用来暴露安装目录与身份是否相符。

use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::modules::{config, edition::Edition};
// ⚠️ 只在 Windows 上用得到。写成条件导入是必须的 —— 非 Windows 上多一个未使用的
// import，CI 的 `-D warnings` 就会直接把 mac/linux 构建打挂（v1.0.4 栽过一次）。
#[cfg(target_os = "windows")]
use crate::modules::process;

/// 控制更新源的用户级环境变量（asar 里明确写了它优先级最高）。
pub const UPDATE_URL_ENV: &str = "WORKBUDDY_UPDATE_URL";

/// 黑洞地址：连不上、也永远不会返回更新信息。
pub const BLACKHOLE: &str = "http://127.0.0.1:1";

/// 共用的更新缓存目录名（Electron updater 按 appId 生成，实测就是这个名字）。
const CACHE_DIR_NAME: &str = "@genieworkbuddy-desktop-updater";

/// 隔离已下载包时加的后缀。
const QUARANTINE_MARK: &str = ".disabled-";

/// 产品身份文件的相对路径。
const PRODUCT_JSON_REL: &str = "resources/app.asar.unpacked/cli/product.json";

// ---------------------------------------------------------------- 数据结构

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheFile {
    pub name: String,
    pub bytes: u64,
    pub size_text: String,
    pub modified: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallInfo {
    pub dir: String,
    /// 从 product.json 读到的档位；读不到就是 None。
    pub edition: Option<String>,
    pub label: String,
    /// `dataFolderName`，如 `.workbuddy-ai`。
    pub data_dir_name: Option<String>,
    pub endpoint: Option<String>,
    pub is_oversea: Option<bool>,
    pub version: Option<String>,
    /// 目录里存在哪些启动器 exe。
    pub exes: Vec<String>,
    /// 身份与「国内版」这个档位不符时的提醒（非空即值得注意）。
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardStatus {
    pub supported: bool,
    pub platform_note: Option<String>,
    /// 总开关当前是否处于「已禁用更新」。
    pub disabled: bool,
    pub env_name: String,
    pub env_value: Option<String>,
    /// 期望值（禁用时应等于它）。
    pub blackhole: String,
    pub cache_dir: String,
    pub cache_exists: bool,
    pub cache_files: Vec<CacheFile>,
    pub cache_bytes: u64,
    pub cache_text: String,
    /// 缓存里还有「会被应用」的包（未隔离的 installer）。
    pub cache_active: bool,
    pub installs: Vec<InstallInfo>,
    pub running: Vec<String>,
}

// ---------------------------------------------------------------- 体积格式化

fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn mtime_text(p: &Path) -> String {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .map(|t| {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        })
        .unwrap_or_else(|_| "—".to_string())
}

// ---------------------------------------------------------------- 更新缓存

fn cache_dir() -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    if local.trim().is_empty() {
        return None;
    }
    Some(PathBuf::from(local).join(CACHE_DIR_NAME))
}

/// 列出缓存里的文件。`active` 判定：名字里不含隔离后缀。
fn read_cache() -> (bool, Vec<CacheFile>, u64) {
    let Some(dir) = cache_dir() else {
        return (false, Vec::new(), 0);
    };
    if !dir.is_dir() {
        return (false, Vec::new(), 0);
    }
    let mut files: Vec<CacheFile> = Vec::new();
    let mut bytes = 0u64;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            bytes += meta.len();
            files.push(CacheFile {
                name,
                bytes: meta.len(),
                size_text: human(meta.len()),
                modified: mtime_text(&p),
            });
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    (true, files, bytes)
}

/// 缓存里的这个文件**会不会被应用**：未隔离的安装包才算。
///
/// 抽成函数是为了让 `status()`、`quarantine_cache()` 和单测共用同一条判据 ——
/// 三处各写一遍迟早会走偏。
fn is_active_package(name: &str) -> bool {
    !name.contains(QUARANTINE_MARK) && name.to_ascii_lowercase().ends_with(".exe")
}

/// 把缓存里**未隔离**的包改名隔离掉（不动已经隔离过的）。
fn quarantine_cache() -> Result<(Vec<String>, u64), String> {
    let Some(dir) = cache_dir() else {
        return Err("读不到 LOCALAPPDATA，无法定位更新缓存目录。".to_string());
    };
    if !dir.is_dir() {
        return Ok((Vec::new(), 0));
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut moved = Vec::new();
    let mut freed = 0u64;
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("读取缓存目录失败：{e}"))?;
    for e in rd.flatten() {
        let p = e.path();
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name.contains(QUARANTINE_MARK) {
            continue;
        }
        let dst = p.with_file_name(format!("{name}{QUARANTINE_MARK}{stamp}"));
        if std::fs::rename(&p, &dst).is_ok() {
            freed += meta.len();
            moved.push(name);
        }
    }
    Ok((moved, freed))
}

// ---------------------------------------------------------------- 安装目录发现

/// 读一个安装目录的产品身份。
fn read_install(dir: &Path) -> Option<InstallInfo> {
    let pj = dir.join(PRODUCT_JSON_REL);
    if !pj.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&pj).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;

    let is_oversea = v.get("isOversea").and_then(|x| x.as_bool());
    let data_dir_name = v
        .get("dataFolderName")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let endpoint = v.get("endpoint").and_then(|x| x.as_str()).map(|s| s.to_string());
    let auth_id = v
        .get("authentication")
        .and_then(|a| a.get("id"))
        .and_then(|x| x.as_str())
        .unwrap_or("");

    // 身份判定：isOversea 优先，其次看 dataFolderName / authentication.id。
    let overseas = match is_oversea {
        Some(b) => b,
        None => data_dir_name
            .as_deref()
            .map(|d| d.ends_with("-ai"))
            .unwrap_or_else(|| auth_id.ends_with("-ai")),
    };
    let edition = if overseas { Edition::International } else { Edition::Domestic };

    let version = std::fs::read_to_string(dir.join("version"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut exes = Vec::new();
    for name in ["WorkBuddy.exe", "WorkBuddyAI.exe"] {
        if dir.join(name).is_file() {
            exes.push(name.to_string());
        }
    }

    // 🔴 这就是 10-08 那次事故的指纹：目录里两个启动器都在，但身份只有一个。
    // 用户点「WorkBuddy」（国内版）的快捷方式，实际跑起来的是国际版。
    let warning = if exes.len() > 1 {
        Some(format!(
            "这个目录里 {} 都在，但身份是{} —— 用另一个名字的 exe 启动也会按{}跑。",
            exes.join(" / "),
            edition.label(),
            edition.label()
        ))
    } else {
        None
    };

    Some(InstallInfo {
        dir: dir.to_string_lossy().to_string(),
        edition: Some(edition.key().to_string()),
        label: edition.label().to_string(),
        data_dir_name,
        endpoint,
        is_oversea: Some(overseas),
        version,
        exes,
        warning,
    })
}

/// 候选父目录 → 逐个看一级子目录里有没有产品身份文件。
///
/// 只做**一级**扫描（`<父>/<名字>/resources/...`），不做递归 —— 既能覆盖
/// `F:\AdobeAll\<名字>` 和 `%LOCALAPPDATA%\Programs\<名字>` 这两种实测布局，
/// 又不会为了找一个文件把整块盘走一遍。
fn discover_installs() -> Vec<InstallInfo> {
    let mut parents: Vec<PathBuf> = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        if !local.trim().is_empty() {
            parents.push(PathBuf::from(local).join("Programs"));
        }
    }
    for drive in ["C:", "D:", "E:", "F:", "G:"] {
        let p = PathBuf::from(format!("{drive}/AdobeAll"));
        if p.is_dir() {
            parents.push(p);
        }
    }
    // 上次成功启动过的 exe 所在目录也算一个候选（装在别处时兜底）。
    if let Some(exe) = config::load_workbuddy_exe_cache() {
        if let Some(d) = exe.parent() {
            parents.push(d.to_path_buf());
        }
    }

    let mut out: Vec<InstallInfo> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for parent in parents {
        let Ok(rd) = std::fs::read_dir(&parent) else { continue };
        for e in rd.flatten() {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            let key = dir.to_string_lossy().to_ascii_lowercase();
            if seen.contains(&key) {
                continue;
            }
            if let Some(info) = read_install(&dir) {
                seen.push(key);
                out.push(info);
            }
        }
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

// ---------------------------------------------------------------- 环境变量（用户级）

#[cfg(windows)]
mod winenv {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(
            hkey: isize,
            sub_key: *const u16,
            options: u32,
            sam: u32,
            result: *mut isize,
        ) -> i32;
        fn RegQueryValueExW(
            hkey: isize,
            value: *const u16,
            reserved: *mut u32,
            ty: *mut u32,
            data: *mut u8,
            cb: *mut u32,
        ) -> i32;
        fn RegSetValueExW(
            hkey: isize,
            value: *const u16,
            reserved: u32,
            ty: u32,
            data: *const u8,
            cb: u32,
        ) -> i32;
        fn RegDeleteValueW(hkey: isize, value: *const u16) -> i32;
        fn RegCloseKey(hkey: isize) -> i32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn SendMessageTimeoutW(
            hwnd: isize,
            msg: u32,
            wparam: usize,
            lparam: isize,
            flags: u32,
            timeout: u32,
            result: *mut usize,
        ) -> isize;
    }

    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as isize;
    const KEY_READ: u32 = 0x2_0019;
    const KEY_SET_VALUE: u32 = 0x0002;
    const REG_SZ: u32 = 1;
    const ERROR_SUCCESS: i32 = 0;
    const ERROR_FILE_NOT_FOUND: i32 = 2;

    const HWND_BROADCAST: isize = 0xFFFF;
    const WM_SETTINGCHANGE: u32 = 0x001A;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    /// 打开 `HKCU\Environment`。`write` 为真时要求写权限。
    fn open_env(write: bool) -> Result<isize, String> {
        let sub = wide("Environment");
        let sam = if write { KEY_READ | KEY_SET_VALUE } else { KEY_READ };
        let mut hkey: isize = 0;
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, sam, &mut hkey) };
        if rc != ERROR_SUCCESS {
            return Err(format!("打不开 HKCU\\Environment（错误码 {rc}）"));
        }
        Ok(hkey)
    }

    pub fn get(name: &str) -> Result<Option<String>, String> {
        let hkey = open_env(false)?;
        let value = wide(name);
        let mut ty: u32 = 0;
        let mut cb: u32 = 0;
        let rc = unsafe {
            RegQueryValueExW(hkey, value.as_ptr(), std::ptr::null_mut(), &mut ty, std::ptr::null_mut(), &mut cb)
        };
        if rc == ERROR_FILE_NOT_FOUND {
            unsafe { RegCloseKey(hkey) };
            return Ok(None);
        }
        if rc != ERROR_SUCCESS || cb == 0 {
            unsafe { RegCloseKey(hkey) };
            return Ok(None);
        }
        let mut buf = vec![0u8; cb as usize];
        let rc = unsafe {
            RegQueryValueExW(hkey, value.as_ptr(), std::ptr::null_mut(), &mut ty, buf.as_mut_ptr(), &mut cb)
        };
        unsafe { RegCloseKey(hkey) };
        if rc != ERROR_SUCCESS {
            return Err(format!("读取 {name} 失败（错误码 {rc}）"));
        }
        // REG_SZ 是 UTF-16，去掉结尾的 NUL。
        let mut units: Vec<u16> = Vec::with_capacity(buf.len() / 2);
        for c in buf.chunks_exact(2) {
            units.push(u16::from_le_bytes([c[0], c[1]]));
        }
        while units.last() == Some(&0) {
            units.pop();
        }
        Ok(Some(String::from_utf16_lossy(&units)))
    }

    pub fn set(name: &str, value: &str) -> Result<(), String> {
        let hkey = open_env(true)?;
        let key = wide(name);
        let data: Vec<u16> = std::ffi::OsStr::new(value).encode_wide().chain(Some(0)).collect();
        let bytes: Vec<u8> = data.iter().flat_map(|u| u.to_le_bytes()).collect();
        let rc = unsafe {
            RegSetValueExW(hkey, key.as_ptr(), 0, REG_SZ, bytes.as_ptr(), bytes.len() as u32)
        };
        unsafe { RegCloseKey(hkey) };
        if rc != ERROR_SUCCESS {
            return Err(format!("写入 {name} 失败（错误码 {rc}）"));
        }
        Ok(())
    }

    pub fn remove(name: &str) -> Result<(), String> {
        let hkey = open_env(true)?;
        let key = wide(name);
        let rc = unsafe { RegDeleteValueW(hkey, key.as_ptr()) };
        unsafe { RegCloseKey(hkey) };
        // 本来就没有也算成功。
        if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
            return Err(format!("删除 {name} 失败（错误码 {rc}）"));
        }
        Ok(())
    }

    /// 广播 WM_SETTINGCHANGE，让资源管理器重新读环境变量 ——
    /// 否则从快捷方式启动的新进程拿到的还是旧环境。失败不影响主流程
    /// （最坏情况要注销重登一次）。
    pub fn broadcast() {
        let param = wide("Environment");
        let mut result: usize = 0;
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                param.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                3000,
                &mut result,
            );
        }
    }
}

#[cfg(not(windows))]
mod winenv {
    pub fn get(_name: &str) -> Result<Option<String>, String> {
        Ok(std::env::var(_name).ok())
    }
    pub fn set(_name: &str, _value: &str) -> Result<(), String> {
        Err("当前平台不支持修改用户级环境变量".to_string())
    }
    pub fn remove(_name: &str) -> Result<(), String> {
        Err("当前平台不支持修改用户级环境变量".to_string())
    }
    pub fn broadcast() {}
}

// ---------------------------------------------------------------- 对外接口

/// 当前在跑的客户端进程名。
///
/// 非 Windows 上不探测：这一页本身只在 Windows 上有意义（要写注册表），
/// 而 `process::windows_tasklist_image_rows` 是 Windows 专属的 ——
/// 无条件调用会让 mac/linux 构建直接编译失败（v1.0.17 首推时就是这么挂的）。
fn running_clients() -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        let mut out = Vec::new();
        for img in ["WorkBuddy.exe", "WorkBuddyAI.exe"] {
            if !process::windows_tasklist_image_rows(img).is_empty() {
                out.push(img.to_string());
            }
        }
        out
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
    }
}

/// 只读体检。
pub fn status() -> GuardStatus {
    let supported = cfg!(target_os = "windows");
    let platform_note = if supported {
        None
    } else {
        Some("更新防护目前只支持 Windows：需要写用户级环境变量（HKCU\\Environment）。".to_string())
    };

    let env_value = winenv::get(UPDATE_URL_ENV).ok().flatten();
    let disabled = env_value
        .as_deref()
        .map(|v| v.trim() == BLACKHOLE)
        .unwrap_or(false);

    let (cache_exists, cache_files, cache_bytes) = read_cache();
    let cache_dir = cache_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "（读不到 LOCALAPPDATA）".to_string());
    let cache_active = cache_files.iter().any(|f| is_active_package(&f.name));
    let running = running_clients();

    GuardStatus {
        supported,
        platform_note,
        disabled,
        env_name: UPDATE_URL_ENV.to_string(),
        env_value,
        blackhole: BLACKHOLE.to_string(),
        cache_dir,
        cache_exists,
        cache_files,
        cache_bytes,
        cache_text: human(cache_bytes),
        cache_active,
        installs: discover_installs(),
        running,
    }
}

/// 开/关更新防护。`clear_cache` 为真时顺带隔离已下载的包。
pub fn set_disabled(disabled: bool, clear_cache: bool) -> Result<Value, String> {
    if !cfg!(target_os = "windows") {
        return Err("更新防护目前只支持 Windows。".to_string());
    }
    let mut quarantined: Vec<String> = Vec::new();
    let mut freed = 0u64;

    if disabled {
        winenv::set(UPDATE_URL_ENV, BLACKHOLE)?;
        winenv::broadcast();
        if clear_cache {
            let (moved, bytes) = quarantine_cache()?;
            quarantined = moved;
            freed = bytes;
        }
    } else {
        winenv::remove(UPDATE_URL_ENV)?;
        winenv::broadcast();
    }

    Ok(json!({
        "ok": true,
        "disabled": disabled,
        "quarantined": quarantined,
        "freed": freed,
        "freedText": human(freed),
        "status": status(),
    }))
}

/// 只隔离缓存里已下载的包，不动环境变量。
pub fn clear_cache() -> Result<Value, String> {
    let (moved, freed) = quarantine_cache()?;
    Ok(json!({
        "ok": true,
        "quarantined": moved,
        "freed": freed,
        "freedText": human(freed),
        "status": status(),
    }))
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blackhole_is_loopback_and_unreachable_port() {
        // 必须是个「连不上」的地址：127.0.0.1:1 上不会有服务。
        assert!(BLACKHOLE.starts_with("http://127.0.0.1:"));
        assert!(BLACKHOLE.ends_with(":1"));
    }

    #[test]
    fn status_does_not_panic() {
        let s = status();
        // 关键字段都要能算出来，不依赖机器上装没装 WorkBuddy。
        assert_eq!(s.env_name, UPDATE_URL_ENV);
        assert_eq!(s.blackhole, BLACKHOLE);
        assert!(!s.cache_dir.is_empty());
        // 未启用时 disabled 必须是 false（除非用户自己设过黑洞地址）。
        if s.env_value.is_none() {
            assert!(!s.disabled);
        }
    }

    /// 缓存文件的「是否会被应用」判定：隔离过的包不算 active。
    #[test]
    fn quarantine_mark_excludes_file_from_active() {
        assert!(is_active_package("installer.exe"));
        assert!(!is_active_package("installer.exe.disabled-20261008-160000"));
        assert!(!is_active_package("installer.exe.quarantine-20261001-231621"));
        assert!(!is_active_package("something.txt"));
    }

    /// 身份判定：`isOversea` 优先，缺失时回落到 dataFolderName / auth id。
    #[test]
    fn edition_detection_falls_back_to_names() {
        let pick = |is_oversea: Option<bool>, data: Option<&str>, auth: &str| {
            match is_oversea {
                Some(b) => b,
                None => data.map(|d| d.ends_with("-ai")).unwrap_or_else(|| auth.ends_with("-ai")),
            }
        };
        assert!(pick(Some(true), Some(".workbuddy"), "x")); // 显式字段优先
        assert!(!pick(Some(false), Some(".workbuddy-ai"), "x"));
        assert!(pick(None, Some(".workbuddy-ai"), ""));
        assert!(!pick(None, Some(".workbuddy"), ""));
        assert!(pick(None, None, "workbuddy-desktop-ai"));
        assert!(!pick(None, None, "workbuddy-desktop"));
    }

    #[test]
    fn human_formats_bytes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1024), "1.0 KB");
        assert_eq!(human(1024 * 1024), "1.0 MB");
    }

    /// 注册表 FFI 的往返验证：写 → 读回 → 删。
    ///
    /// 用**独立的临时变量名**（带 pid），绝不碰真实的 `WORKBUDDY_UPDATE_URL` ——
    /// 否则跑一次测试就把用户的更新源改掉了。这条能挡住「FFI 写错了但界面看着正常」
    /// 这类静默失败：环境变量写不进去，防护就是形同虚设。
    #[cfg(windows)]
    #[test]
    fn env_var_roundtrip_uses_temp_name() {
        let name = format!("WB_SWITCH_ENV_PROBE_{}", std::process::id());
        // 上次跑到一半崩了可能留残值，先清干净再断言初始态。
        let _ = winenv::remove(&name);
        assert_eq!(winenv::get(&name).unwrap(), None, "起始应当不存在");

        winenv::set(&name, BLACKHOLE).unwrap();
        assert_eq!(
            winenv::get(&name).unwrap().as_deref(),
            Some(BLACKHOLE),
            "写入后要能原样读回"
        );

        // 非 ASCII 也要能往返：值里出现中文/盘符路径是可能的。
        winenv::set(&name, "测试-值 F:\\x").unwrap();
        assert_eq!(winenv::get(&name).unwrap().as_deref(), Some("测试-值 F:\\x"));

        winenv::remove(&name).unwrap();
        assert_eq!(winenv::get(&name).unwrap(), None, "删除后应当读不到");
    }
}
