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

/// 🚨 **更新包会被暂存到两个地方，两个都要管**。
///
/// 2026-10-08 实测：只盯第 1 个是不够的 —— 真正被启动安装的是第 2 个里的 NSIS 包。
/// 日志原文（`~/.workbuddy/logs/update/update-<date>.log`）：
///
/// ```text
/// [win32] Update downloaded:  ...\AppData\Local\Temp\workbuddy-update-x64\WorkBuddy-Setup-5.7.6.40409493.exe
/// [win32] Restored downloaded update from cache: version=5.7.6.40409493
/// [win32] Launching installer: ...\Temp\workbuddy-update-x64\WorkBuddy-Setup-5.7.6.40409493.exe
/// [win32] Using NSIS silent mode (/S --updated /UPDATE=1 /D=<安装目录>)
/// ```
const CACHE_DIR_NAME: &str = "@genieworkbuddy-desktop-updater";

/// `%TEMP%` 下自定义 updater 的暂存目录前缀（实测 `workbuddy-update-x64`）。
const TEMP_STAGING_PREFIX: &str = "workbuddy-update";

/// 🔴 **`product.json` 里控制「启动时静默强制更新」的开关** ——
/// 就是今晚把 5.7.6 装上去的那条路径。日志原文：
/// `[early-silent-startup-update] enabled=true source=product-base`
///
/// 它是**每份安装各自**的配置，所以这里能做到真正的「按版本」禁用，
/// 而不像环境变量那样只能全局。默认值是 `true`。
const STARTUP_UPDATE_KEY: &str = "startupForceAutoUpdate";

/// 隔离已下载包时加的后缀。
const QUARANTINE_MARK: &str = ".disabled-";

/// 产品身份文件的相对路径。
const PRODUCT_JSON_REL: &str = "resources/app.asar.unpacked/cli/product.json";

// ---------------------------------------------------------------- 数据结构

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheFile {
    /// 所在暂存目录（两个位置里的哪一个）。
    pub dir: String,
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
    /// **最终生效值**：`settings.json` 的用户设置优先，其次 `product.json`。
    /// `false` 才是「已关掉启动静默更新」。
    pub startup_update: Option<bool>,
    /// 单独把 `settings.json`（最高优先级那层）的值带出来，便于排查「改了没生效」。
    pub user_flag: Option<bool>,
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
    /// 两个暂存目录（都列出来，别让用户以为只有一个）。
    pub cache_dirs: Vec<String>,
    pub cache_files: Vec<CacheFile>,
    pub cache_bytes: u64,
    pub cache_text: String,
    /// 暂存目录里还有「会被应用」的包（未隔离的安装包）。
    pub cache_active: bool,
    /// 暂存目录是不是已被冻结（显式 DENY 写入）。
    pub cache_frozen: bool,
    pub installs: Vec<InstallInfo>,
    /// 还没关掉「启动静默更新」的安装目录数。
    pub installs_needing_patch: u32,
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

/// 所有会暂存更新包的位置。**两个都要**，缺一个就等于没防住。
fn staging_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        if !local.trim().is_empty() {
            let local = PathBuf::from(local);
            // ① electron-updater 的缓存
            out.push(local.join(CACHE_DIR_NAME));
            // ② 自定义 updater 真正启动的那个 NSIS 包（`%TEMP%` 就在 LOCALAPPDATA 下）
            let temp = local.join("Temp");
            if let Ok(rd) = std::fs::read_dir(&temp) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                    if name.starts_with(TEMP_STAGING_PREFIX) {
                        out.push(e.path());
                    }
                }
            } else {
                out.push(temp.join("workbuddy-update-x64"));
            }
        }
    }
    out
}

/// 列出所有暂存目录里的文件。
fn read_cache() -> (Vec<CacheFile>, u64) {
    let mut files: Vec<CacheFile> = Vec::new();
    let mut bytes = 0u64;
    for dir in staging_dirs() {
        if !dir.is_dir() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            bytes += meta.len();
            files.push(CacheFile {
                dir: dir.to_string_lossy().to_string(),
                name: e.file_name().to_string_lossy().to_string(),
                bytes: meta.len(),
                size_text: human(meta.len()),
                modified: mtime_text(&p),
            });
        }
    }
    files.sort_by(|a, b| (a.dir.clone(), a.name.clone()).cmp(&(b.dir.clone(), b.name.clone())));
    (files, bytes)
}

/// 缓存里的这个文件**会不会被应用**：未隔离的安装包才算。
///
/// 抽成函数是为了让 `status()`、`quarantine_cache()` 和单测共用同一条判据 ——
/// 三处各写一遍迟早会走偏。
fn is_active_package(name: &str) -> bool {
    !name.contains(QUARANTINE_MARK) && name.to_ascii_lowercase().ends_with(".exe")
}

/// 把所有暂存目录里**未隔离**的包改名隔离掉（不动已经隔离过的）。
fn quarantine_cache() -> Result<(Vec<String>, u64), String> {
    // 改名 = 在父目录里新建一个目录项，会被我们自己的「拒绝写入」挡住
    // （WD 里有 FILE_ADD_FILE）。所以冻结状态下先临时解冻，干完再冻回去。
    let was_frozen = cache_is_frozen();
    if was_frozen {
        let _ = set_dirs_frozen(false);
    }
    let result = quarantine_cache_inner();
    if was_frozen {
        let _ = set_dirs_frozen(true);
    }
    result
}

fn quarantine_cache_inner() -> Result<(Vec<String>, u64), String> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut moved = Vec::new();
    let mut freed = 0u64;
    for dir in staging_dirs() {
        if !dir.is_dir() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
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
                moved.push(format!("{}（{}）", name, human(meta.len())));
            }
        }
    }
    Ok((moved, freed))
}

// ---------------------------------------------------------------- 冻结暂存目录

/// 冻结/解冻暂存目录：对**当前用户**显式 DENY 掉写入、建子目录、删除。
///
/// 🔴 **为什么这一层才是关键**（2026-10-08 从 asar 里挖出来的机制）：
/// Windows 的「已下载就绪」判定**没有状态文件** —— 它就是
/// **扫缓存目录里有没有 `WorkBuddy-Setup-<version>.exe`**：
/// ```text
/// 冷启动恢复「已下载就绪」态（Issue #99875）
/// 扫缓存目录里的 WorkBuddy-Setup-<version>.exe，取版本最高且有效（>1MB）的一份：
///   当前运行版本 >= 该版本 → 删该 exe，不恢复
///   否则 → setState('ready')，冷启动直接安装
/// ```
/// 所以**只要让那个 exe 写不进缓存目录，装包这条路就彻底死了** ——
/// 挡下载、挡装包，而且不碰任何 CDN（不像 hosts 屏蔽会连带搞坏技能市场/头像）。
///
/// 还顺带解决了「环境变量对已在运行的进程无效」这个死角。
///
/// 权限位怎么选（踩过坑）：
/// - `WD`(写数据/建文件) + `AD`(追加/建子目录)，带 `(OI)(CI)` 继承 → **挡住往里放新包**，
///   这是核心。
/// - `DE`(删除) **只加在目录本身、不继承** → 挡住「把目录整个删掉重建」。
///   🔴 **不能把 `DE` 也设成继承** —— 那会连**我们自己**给已下载包改名隔离
///   （改名 = 对源文件要 DELETE 权限）都做不了，按钮会直接失败。
/// 解冻用 `/remove:d`（只摘该账号的 DENY，不动其它 ACE）。
fn set_dirs_frozen(freeze: bool) -> Result<Vec<String>, String> {
    #[cfg(not(windows))]
    {
        let _ = freeze;
        Err("冻结暂存目录目前只支持 Windows。".to_string())
    }
    #[cfg(windows)]
    {
        let acct = current_account().ok_or("读不到当前用户名，没法设目录权限")?;
        let mut done = Vec::new();
        for dir in staging_dirs() {
            // 目录不存在就先建个空的：否则应用自己 mkdir 时拿到的是一个「可写的新目录」。
            if !dir.is_dir() {
                let _ = std::fs::create_dir_all(&dir);
            }
            if !dir.is_dir() {
                continue;
            }
            // 先摘掉旧的 DENY，避免反复点造成 ACE 叠加、也避免残留的继承 DE 卡住改名。
            let _ = run_icacls(&[
                &dir,
                Path::new("/remove:d"),
                Path::new(&acct),
                Path::new("/T"),
                Path::new("/C"),
                Path::new("/Q"),
            ]);
            if freeze {
                run_icacls(&[
                    &dir,
                    Path::new("/deny"),
                    Path::new(&format!("{acct}:(OI)(CI)(WD,AD)")),
                    Path::new("/T"),
                    Path::new("/C"),
                    Path::new("/Q"),
                ])?;
                // 目录自身的删除权（不继承）
                run_icacls(&[
                    &dir,
                    Path::new("/deny"),
                    Path::new(&format!("{acct}:(DE)")),
                    Path::new("/C"),
                    Path::new("/Q"),
                ])?;
            }
            done.push(dir.to_string_lossy().to_string());
        }
        Ok(done)
    }
}

#[cfg(windows)]
fn run_icacls(args: &[&Path]) -> Result<String, String> {
    use std::process::Command;
    let mut c = Command::new("icacls");
    for a in args {
        c.arg(a);
    }
    let out = c
        .output()
        .map_err(|e| format!("调用 icacls 失败：{e}"))?;
    // 控制台是 GBK，非 ASCII 会乱码 —— 但我们要判断的 "(DENY)" 是 ASCII，
    // 所以用 lossy 就够，别为了中文提示去引编码库。
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        return Err(format!(
            "icacls 返回 {}：{}",
            out.status.code().unwrap_or(-1),
            text.trim()
        ));
    }
    Ok(text)
}

/// 当前账号 `域\用户名`，给 icacls 用（用账号名而不是 SID，省一层转换）。
#[cfg(windows)]
fn current_account() -> Option<String> {
    let user = std::env::var("USERNAME").ok()?;
    if user.trim().is_empty() {
        return None;
    }
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    Some(if domain.trim().is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    })
}

/// 暂存目录当前是不是已被冻结（含 DENY 项）。
fn cache_is_frozen() -> bool {
    #[cfg(not(windows))]
    {
        false
    }
    #[cfg(windows)]
    {
        let Some(acct) = current_account() else {
            return false;
        };
        let dirs: Vec<_> = staging_dirs().into_iter().filter(|d| d.is_dir()).collect();
        if dirs.is_empty() {
            return false;
        }
        dirs.iter().all(|d| {
            run_icacls(&[d, Path::new("/T"), Path::new("/C"), Path::new("/Q")])
                .map(|t| t.to_uppercase().contains("(DENY)") && t.contains(&acct.to_uppercase()))
                .unwrap_or(false)
        })
    }
}

// ---------------------------------------------------------------- 改 product.json

/// 就地改 `updates.startupForceAutoUpdate`，返回改好的文本。
///
/// 🔴 **只做定点文本替换，不重新序列化整个文件** —— `product.json` 是程序的身份文件，
/// 用 serde 读进来再写出去会打乱键序、抹掉原有格式，没必要冒这个险。
///
/// 🔴 **国际版实测根本没声明这个键**（`updates` 里只有 `apiVersion` / `download` / `checkVersion`），
/// 所以「找不到就跳过」是不够的 —— 缺省行为不可控，得**显式补进去**才算关掉。
/// 反过来，要「恢复更新」时若键本来就不存在，就什么都不做（别擅自塞一个 `true`，
/// 那等于改了厂商默认行为）。
fn patch_startup_flag(text: &str, disable_update: bool) -> Option<String> {
    if let Some(out) = flip_existing_flag(text, disable_update) {
        return Some(out);
    }
    if !disable_update {
        return Some(text.to_string()); // 恢复更新 + 本来就没这个键 → 无需改动
    }
    insert_flag_into_updates(text)
}

/// 键存在时：把值改成想要的那个。
fn flip_existing_flag(text: &str, disable_update: bool) -> Option<String> {
    let key = format!("\"{STARTUP_UPDATE_KEY}\"");
    let at = text.find(&key)?;
    let after_key = &text[at + key.len()..];
    let colon = after_key.find(':')?;
    let after_colon = &after_key[colon + 1..];
    let value_at = after_colon.find(|c: char| !c.is_whitespace())?;
    let rest = &after_colon[value_at..];

    let (from, to) = if rest.starts_with("false") {
        ("false", "true")
    } else if rest.starts_with("true") {
        ("true", "false")
    } else {
        return None;
    };
    // `disable_update == true` 想看到的是 false。
    let currently_false = from == "false";
    if disable_update == currently_false {
        return Some(text.to_string()); // 已经是想要的值，原样返回
    }

    let abs = at + key.len() + colon + 1 + value_at;
    let mut out = String::with_capacity(text.len() + 1);
    out.push_str(&text[..abs]);
    out.push_str(to);
    out.push_str(&text[abs + from.len()..]);
    Some(out)
}

/// 键不存在时：插到 `"updates": {` 的第一项。JSON 键序无所谓，放最前面最省事。
fn insert_flag_into_updates(text: &str) -> Option<String> {
    let key = "\"updates\"";
    let at = text.find(key)?;
    let after = &text[at + key.len()..];
    let brace = after.find('{')?;
    let pos = at + key.len() + brace + 1;
    // 空对象 `{}` 不能插成 `{...,}`（尾逗号不是合法 JSON）—— 真遇到了就不插逗号。
    let empty = text[pos..].trim_start().starts_with('}');
    let mut out = String::with_capacity(text.len() + STARTUP_UPDATE_KEY.len() + 12);
    out.push_str(&text[..pos]);
    if empty {
        out.push_str(&format!("\"{STARTUP_UPDATE_KEY}\": false"));
    } else {
        out.push_str(&format!("\"{STARTUP_UPDATE_KEY}\": false,"));
    }
    out.push_str(&text[pos..]);
    // 插完必须仍是合法 JSON，否则宁可不改。
    serde_json::from_str::<Value>(&out).ok()?;
    Some(out)
}

// ---------------------------------------------------------------- 用户设置（最高优先级）

/// 用户设置那一层：`<数据目录>/settings.json` 顶层的 `startupForceAutoUpdate`。
///
/// 🔴🔴 **只改 `product.json` 是不够的。** asar 里写明的优先级链（自高到低）：
///
/// ```text
/// 1. settings.json 里用户手动设置的 startupForceAutoUpdate   ← 本函数写这一层
/// 2. 远程实时下发的 productFeature `StartupForceAutoUpdateDefault`（服务端能压回来）
/// 3. settings.json 里缓存的远程运营默认值 `startupForceAutoUpdateSystemDefault`
/// 4. product.json 的 updates.startupForceAutoUpdate
/// 5. 硬编码首次实装兜底 = true
/// ```
///
/// 2026-10-08 实测：`~/.workbuddy/cache/acc-product-config-v3.json`（服务端下发缓存）
/// 里躺着 `updates.startupForceAutoUpdate: true` —— 只改第 4 层会被它盖掉，
/// 所以必须把第 1 层也写掉。好在 settings.json 按**数据目录**分
/// （`~/.workbuddy` vs `~/.workbuddy-ai`），正好对上「按版本」。
fn user_settings_path(data_dir_name: &str) -> Option<PathBuf> {
    let name = data_dir_name.trim();
    if name.is_empty() {
        return None;
    }
    Some(config::home_dir().join(name).join("settings.json"))
}

/// 顶层插入/翻转 `startupForceAutoUpdate`。同样是定点文本改，不重新序列化。
fn patch_user_setting(text: &str, disable_update: bool) -> Option<String> {
    if let Some(out) = flip_existing_flag(text, disable_update) {
        return Some(out);
    }
    if !disable_update {
        return Some(text.to_string()); // 本来就没这个键 → 恢复时什么都不做
    }
    let brace = text.find('{')?;
    let pos = brace + 1;
    let empty = text[pos..].trim_start().starts_with('}');
    let mut out = String::with_capacity(text.len() + STARTUP_UPDATE_KEY.len() + 8);
    out.push_str(&text[..pos]);
    if empty {
        out.push_str(&format!("\"{STARTUP_UPDATE_KEY}\": false"));
    } else {
        out.push_str(&format!("\n  \"{STARTUP_UPDATE_KEY}\": false,"));
    }
    out.push_str(&text[pos..]);
    serde_json::from_str::<Value>(&out).ok()?;
    Some(out)
}

/// 把一层配置文件改成想要的值，成功返回「是否真的动了」。
fn apply_flag_to_file(path: &Path, disable_update: bool, what: &str) -> Result<bool, String> {
    if !path.is_file() {
        return Err(format!("找不到 {what}：{}", path.to_string_lossy()));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读不了 {}：{e}", path.to_string_lossy()))?;
    let patched = patch_user_setting(&text, disable_update).ok_or_else(|| {
        format!("{} 里没有 {} 这个键，也没法安全插入（可能版本变了）", path.to_string_lossy(), STARTUP_UPDATE_KEY)
    })?;
    if patched == text {
        return Ok(false);
    }
    let bak = append_suffix(path, ".bak-switch");
    if !bak.exists() {
        let _ = std::fs::write(&bak, &text);
    }
    std::fs::write(path, &patched).map_err(|e| format!("写不了 {}：{e}", path.to_string_lossy()))?;
    Ok(true)
}

fn append_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
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

    // 先把身份字段读出来 —— 下面算「用户设置那一层」要用 dataFolderName 定位数据目录。
    let is_oversea = v.get("isOversea").and_then(|x| x.as_bool());
    let data_dir_name = v
        .get("dataFolderName")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());

    // `updates.startupForceAutoUpdate` —— 启动静默强制更新的开关（默认 true）。
    // 这是**第 4 层**；第 1 层在数据目录的 settings.json 里（下面读）。
    let product_flag = v
        .get("updates")
        .and_then(|u| u.get(STARTUP_UPDATE_KEY))
        .and_then(|x| x.as_bool());

    // 第 1 层（最高优先级）：`<数据目录>/settings.json` 顶层。
    let user_flag = data_dir_name
        .as_deref()
        .and_then(user_settings_path)
        .filter(|p| p.is_file())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|j| j.get(STARTUP_UPDATE_KEY).and_then(|x| x.as_bool()));

    // 生效值：用户设置优先。
    let startup_update = user_flag.or(product_flag);

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
        startup_update,
        user_flag,
        warning,
    })
}

/// 把一份安装的「启动静默更新」关掉（或恢复）。
///
/// **两层都要写**：`product.json`（第 4 层）+ 数据目录的 `settings.json`（第 1 层）。
/// 只写前者会被服务端下发的配置盖掉，只写后者在 settings.json 缺失时会漏。
/// 返回「是否真的动过」。
fn apply_startup_flag(
    dir: &Path,
    data_dir_name: Option<&str>,
    disable_update: bool,
) -> Result<bool, String> {
    let mut changed = false;

    // 第 4 层：product.json base
    let pj = dir.join(PRODUCT_JSON_REL);
    if pj.is_file() {
        let text = std::fs::read_to_string(&pj)
            .map_err(|e| format!("读不了 {}：{e}", pj.to_string_lossy()))?;
        let patched = patch_startup_flag(&text, disable_update).ok_or_else(|| {
            format!(
                "{} 里没法安全改写 {}（可能版本变了）",
                pj.to_string_lossy(),
                STARTUP_UPDATE_KEY
            )
        })?;
        if patched != text {
            let bak = append_suffix(&pj, ".bak-switch");
            if !bak.exists() {
                let _ = std::fs::write(&bak, &text);
            }
            std::fs::write(&pj, &patched)
                .map_err(|e| format!("写不了 {}：{e}", pj.to_string_lossy()))?;
            changed = true;
        }
    }

    // 第 1 层：数据目录的 settings.json（用户设置，优先级最高）
    // 这一层没有就**跳过而不是报错** —— 用户可能还没启动过这个版本，settings.json 还没生成。
    if let Some(name) = data_dir_name {
        if let Some(sp) = user_settings_path(name) {
            if sp.is_file() {
                if apply_flag_to_file(&sp, disable_update, "用户设置").unwrap_or(false) {
                    changed = true;
                }
            }
        }
    }

    Ok(changed)
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
    let env_disabled = env_value
        .as_deref()
        .map(|v| v.trim() == BLACKHOLE)
        .unwrap_or(false);

    let (cache_files, cache_bytes) = read_cache();
    let cache_dirs = staging_dirs()
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let cache_active = cache_files.iter().any(|f| is_active_package(&f.name));
    let cache_frozen = cache_is_frozen();
    let running = running_clients();
    let installs = discover_installs();
    // 还开着「启动静默更新」的安装数。这个是**真正**决定会不会被静默升级的开关
    // （环境变量只挡得住新进程发起的检查，挡不住已下载的包在启动时被应用）。
    let installs_needing_patch = installs
        .iter()
        .filter(|i| i.startup_update != Some(false))
        .count() as u32;
    // 三件都满足才算「防护生效」：更新源黑洞 + 没有安装还开着启动静默更新
    // + 暂存目录已冻结（挡住「把新下载的包装进去」这条路）。
    let disabled =
        env_disabled && installs_needing_patch == 0 && cache_frozen && !installs.is_empty();

    GuardStatus {
        supported,
        platform_note,
        disabled,
        env_name: UPDATE_URL_ENV.to_string(),
        env_value,
        blackhole: BLACKHOLE.to_string(),
        cache_dirs,
        cache_files,
        cache_bytes,
        cache_text: human(cache_bytes),
        cache_active,
        cache_frozen,
        installs,
        installs_needing_patch,
        running,
    }
}

/// 开/关更新防护。
///
/// 「禁用」做三件事，**少一件都挡不住**（2026-10-08 实测教训）：
///
/// 1. 把 `WORKBUDDY_UPDATE_URL` 指到黑洞 —— 拦住*新进程*发起的更新检查。
///    ⚠️ 对**已经在跑**的进程无效（进程环境在创建时就固定了），所以单靠它挡不住。
/// 2. 把每份安装的 `updates.startupForceAutoUpdate` 改成 `false` ——
///    关掉「启动时静默安装已下载的包」这条路径（今晚 5.7.6 就是走这条装上去的）。
///    这个是**按安装目录**的，所以是真正的按版本生效。
/// 3. 隔离两个暂存目录里已下载的包。
pub fn set_disabled(disabled: bool, clear_cache: bool) -> Result<Value, String> {
    if !cfg!(target_os = "windows") {
        return Err("更新防护目前只支持 Windows。".to_string());
    }
    let mut quarantined: Vec<String> = Vec::new();
    let mut freed = 0u64;
    let mut patched: Vec<String> = Vec::new();
    let mut patch_errors: Vec<String> = Vec::new();

    // 先改配置文件：这是真正管住「启动静默更新」的那一环，失败了要如实报出来。
    for info in discover_installs() {
        let dir = PathBuf::from(&info.dir);
        match apply_startup_flag(&dir, info.data_dir_name.as_deref(), disabled) {
            Ok(true) => patched.push(info.label.clone()),
            Ok(false) => {}
            Err(e) => patch_errors.push(e),
        }
    }

    if disabled {
        winenv::set(UPDATE_URL_ENV, BLACKHOLE)?;
        winenv::broadcast();
        if clear_cache {
            let (moved, bytes) = quarantine_cache()?;
            quarantined = moved;
            freed = bytes;
        }
        // 最后再冻结：先把已经躺着的包清掉，再锁门（反过来的话改名会被自己挡住）。
        if let Err(e) = set_dirs_frozen(true) {
            patch_errors.push(format!("冻结暂存目录失败：{e}"));
        }
    } else {
        winenv::remove(UPDATE_URL_ENV)?;
        winenv::broadcast();
        // 关掉防护就把门打开，别留着看不懂的 ACL。
        if let Err(e) = set_dirs_frozen(false) {
            patch_errors.push(format!("解冻暂存目录失败：{e}"));
        }
    }

    Ok(json!({
        "ok": true,
        "disabled": disabled,
        "quarantined": quarantined,
        "freed": freed,
        "freedText": human(freed),
        "patched": patched,
        "patchErrors": patch_errors,
        "status": status(),
    }))
}

/// 只隔离暂存目录里已下载的包，不动开关。
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

/// 单独冻结/解冻暂存目录（不碰环境变量和配置层）。
///
/// 这是**与版本无关**的那一层：不管 WorkBuddy 哪个版本，只要它写不进
/// `WorkBuddy-Setup-<version>.exe`，「冷启动装掉已下载的包」这条路就成立不了。
pub fn set_cache_frozen(freeze: bool) -> Result<Value, String> {
    let dirs = set_dirs_frozen(freeze)?;
    Ok(json!({
        "ok": true,
        "frozen": freeze,
        "dirs": dirs,
        "status": status(),
    }))
}

/// **自愈**：如果用户之前开过防护（更新源还指在黑洞上），就把该补的层补一遍。
///
/// 为什么需要它：**重装/升级会覆盖 `resources/`**，把 `product.json` 那一层抹掉
/// （2026-10-08 实测：用户重装 5.6.2 后国内版被还原成 `true`、国际版的键直接消失）。
/// 靠「开关只点一次」迟早会被抹掉，所以每次启动 Switch 都静默补一次 ——
/// **幂等**，没被动过就什么都不写。
///
/// 🔴 判据是**环境变量还在黑洞上**（用户开着防护的标记）。没开防护就绝不插手，
/// 免得把用户手动打开的更新又关掉。
///
/// 返回补了什么（空 = 无需补），供调用方记日志或提示；失败静默忽略，
/// 不能因为这个影响启动。
pub fn repair_if_enabled() -> Vec<String> {
    if !cfg!(target_os = "windows") {
        return Vec::new();
    }
    let enabled = winenv::get(UPDATE_URL_ENV)
        .ok()
        .flatten()
        .map(|v| v.trim() == BLACKHOLE)
        .unwrap_or(false);
    if !enabled {
        return Vec::new();
    }

    let mut done = Vec::new();
    for info in discover_installs() {
        let dir = PathBuf::from(&info.dir);
        if let Ok(true) = apply_startup_flag(&dir, info.data_dir_name.as_deref(), true) {
            done.push(format!("{}：补上了启动静默更新开关", info.label));
        }
    }
    if let Ok((moved, _)) = quarantine_cache() {
        if !moved.is_empty() {
            done.push(format!("隔离了 {} 个已下载的更新包", moved.len()));
        }
    }
    // 冻结那层也有可能被外力改掉（改权限、换目录），一并补回来。
    if !cache_is_frozen() && set_dirs_frozen(true).is_ok() {
        done.push("重新冻结了更新暂存目录".to_string());
    }
    done
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
        assert!(!s.cache_dirs.is_empty());
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

    /// 🔴 这是本功能唯一真正管住「启动静默更新」的地方，必须只动那一个布尔值
    /// —— `product.json` 是程序的身份文件，多改一个字节都可能出事。
    #[test]
    fn patch_startup_flag_is_surgical() {
        let src = "{\n  \"applicationName\": \"workbuddy\",\n  \
                   \"updates\": { \"apiVersion\": \"v2\", \"startupForceAutoUpdate\": true },\n  \
                   \"dataFolderName\": \".workbuddy\"\n}";

        let off = patch_startup_flag(src, true).expect("应当找得到这个键");
        assert!(off.contains("\"startupForceAutoUpdate\": false"),
                "应当被改成 false：{off}");
        // 除那一个词之外，其余内容必须逐字节相同
        assert_eq!(off.replace("false", "true"), src, "别的字节不许动");
        assert_eq!(off.len(), src.len() + 1, "true→false 正好长 1");

        // 已经是想要的值 → 原样返回（不算改动）
        assert_eq!(patch_startup_flag(&off, true).as_deref(), Some(off.as_str()));

        // 还原
        assert_eq!(patch_startup_flag(&off, false).as_deref(), Some(src));

        // 键不存在（版本变了）→ None，上层要如实报错，不能假装成功
        assert!(patch_startup_flag("{}", true).is_none());
        // 但 `updates` 在、只是里面没有这个键 → 补进去（见 patch_inserts_flag_when_absent）
        assert!(patch_startup_flag("{\"updates\":{}}", true).is_some());
    }

    /// 🔴 国际版实测**没有**声明这个键（`updates` 里只有 apiVersion/download/checkVersion）。
    /// 缺省值不可控，所以必须显式补进去才算关掉；反过来恢复时不能擅自塞 `true`。
    #[test]
    fn patch_inserts_flag_when_absent() {
        let src = "{\"updates\": {\"apiVersion\": \"v2\"}}";

        let off = patch_startup_flag(src, true).expect("应当能补进去");
        let v: Value = serde_json::from_str(&off).expect("补完必须仍是合法 JSON");
        assert_eq!(
            v.get("updates")
                .and_then(|u| u.get(STARTUP_UPDATE_KEY))
                .and_then(|x| x.as_bool()),
            Some(false),
            "补进去的值必须是 false：{off}"
        );

        // 恢复更新 + 本来就没这个键 → 原样返回，别擅自写一个 true
        assert_eq!(patch_startup_flag(src, false).as_deref(), Some(src));

        // 连 updates 都没有 → 放弃，由上层如实报错
        assert!(patch_startup_flag("{}", true).is_none());
        assert!(patch_startup_flag("{\"a\":1}", true).is_none());
    }

    /// 空的 `updates` 对象也不能插出尾逗号 —— 那会让整个 product.json 变成非法 JSON。
    #[test]
    fn insert_handles_empty_updates_object() {
        let out = patch_startup_flag("{\"updates\": {}}", true).expect("空对象也要能插");
        let v: Value = serde_json::from_str(&out).expect("必须仍是合法 JSON");
        assert_eq!(v["updates"][STARTUP_UPDATE_KEY].as_bool(), Some(false));
    }

    /// 用户设置那一层（`settings.json`，**最高优先级**）：顶层插入 / 翻转。
    /// 这是唯一能压住「服务端下发把开关打回 true」的位置，必须稳。
    #[test]
    fn patch_user_setting_handles_missing_and_existing() {
        let src = "{\n  \"autoLaunchDesired\": true\n}";

        let off = patch_user_setting(src, true).expect("应当能插入");
        let v: Value = serde_json::from_str(&off).expect("必须仍是合法 JSON");
        assert_eq!(v.get(STARTUP_UPDATE_KEY).and_then(|x| x.as_bool()), Some(false));
        assert_eq!(
            v.get("autoLaunchDesired").and_then(|x| x.as_bool()),
            Some(true),
            "别的键一个都不许动"
        );

        // 已有这个键 → 原值翻转
        let on = patch_user_setting(&off, false).expect("应当能翻转回来");
        let v2: Value = serde_json::from_str(&on).expect("仍须合法");
        assert_eq!(v2.get(STARTUP_UPDATE_KEY).and_then(|x| x.as_bool()), Some(true));

        // 空对象：插进去也不能有尾逗号
        let empty = patch_user_setting("{}", true).expect("空对象也要能插");
        assert_eq!(
            serde_json::from_str::<Value>(&empty).unwrap()[STARTUP_UPDATE_KEY].as_bool(),
            Some(false)
        );

        // 恢复 + 本来就没有这个键 → 原样返回，别擅自写 true
        assert_eq!(patch_user_setting(src, false).as_deref(), Some(src));

        // 不是 JSON → 放弃，交给上层报错
        assert!(patch_user_setting("not json", true).is_none());
    }

    /// 两个暂存目录都要覆盖到：只盯 `@genieworkbuddy-desktop-updater` 是不够的，
    /// 真正被启动安装的 NSIS 包在 `%TEMP%\workbuddy-update-*` 里。
    #[test]
    fn staging_dirs_cover_both_locations() {
        let dirs = staging_dirs();
        if std::env::var("LOCALAPPDATA").is_ok() {
            assert!(!dirs.is_empty(), "至少要能算出一个暂存目录");
            assert!(
                dirs.iter().any(|d| d.to_string_lossy().contains(CACHE_DIR_NAME)),
                "必须包含 electron-updater 的缓存目录"
            );
        }
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
