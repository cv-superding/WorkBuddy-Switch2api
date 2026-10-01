//! 家目录缓存目录跨盘迁移（Windows NTFS 目录联接）。
//!
//! ## 为什么需要它
//!
//! WorkBuddy 把全部运行数据放在家目录下：国内版 `~/.workbuddy`、国际版
//! `~/.workbuddy-ai`。实测国内版能涨到 17 GB（21 万个文件），全落在系统盘。
//! 客户端**没有**「改缓存目录」的入口，社区做法统一是目录联接：
//! robocopy 把数据整份搬到别的盘，原路径 `mklink /J` 指回去，应用零感知。
//!
//! ## 为什么不用环境变量
//!
//! app.asar 里的 `resolveWorkbuddyConfigDir()` 确实认 `WORKBUDDY_CONFIG_DIR`
//! （回退 `CODEBUDDY_CONFIG_DIR`，再回退 `homedir/<product.json 的 dataFolderName>`），
//! 但**一台机器上可能同时装着国内版和国际版**，两版共用这套解析逻辑。
//! 一旦设了用户级 `WORKBUDDY_CONFIG_DIR`，两个应用会指向同一个目录、数据互相覆盖。
//! 所以这里只做联接，不碰任何环境变量。
//!
//! ## 安全设计
//!
//! ```text
//! [1/5] 统计源目录（文件数 + 总字节）
//! [2/5] robocopy /E /COPY:DAT /DCOPY:DAT /MT:16 /XJ
//! [3/5] 复核目标副本 —— 文件数与字节数必须完全一致，否则中止且不动源目录
//! [4/5] 原目录改名 <名称>.moved-YYYYmmdd-HHMMSS（同盘改名，瞬间完成，不删数据）
//! [5/5] mklink /J 建联接，验证重解析点 + 目录可读
//! ```
//!
//! 任何一步失败都会回滚（删联接、改名还原）。备份目录保留到用户显式清理，
//! 清理只接受「家目录直接子项 + 名字含 `.moved-` + 不是联接」的路径。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::modules::{edition::Edition, process};

/// 不属于任何一个 Edition、但应用会读的家目录目录。
const EXTRA_DIRS: [(&str, &str); 1] = [(".workbuddy-key-fallback", "凭据回退目录")];

/// 备份目录名后缀标记。
const BACKUP_MARK: &str = ".moved-";

/// 迁移期间必须退出的进程（这两个才是这些目录的持有者）。
const BLOCKING_IMAGES: [&str; 2] = ["WorkBuddy.exe", "WorkBuddyAI.exe"];

/// 不阻塞迁移、但会在界面上提示的进程（CodeBuddy IDE 可能间接占用运行时目录）。
const WARN_IMAGES: [&str; 1] = ["CodeBuddy.exe"];

/// 目标根目录的默认名字（拼在盘符后面）。
const DEST_FOLDER: &str = "WorkBuddyData";

// ---------------------------------------------------------------- 数据结构

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheDir {
    /// 家目录下的目录名，如 `.workbuddy`。
    pub name: String,
    /// 界面展示名，如「国内版 WorkBuddy」。
    pub label: String,
    pub path: String,
    pub exists: bool,
    /// 是否已是联接。
    pub is_link: bool,
    /// 联接指向（`is_link` 为真时）。
    pub link_target: Option<String>,
    /// 遗留的备份目录（`<名称>.moved-*`），有就说明迁过但还没清理。
    pub backup: Option<String>,
    pub files: u64,
    pub bytes: u64,
    /// 是否参与「迁移全部」。
    pub movable: bool,
    /// 前端展示用的体积字符串。
    pub size_text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    pub letter: String,
    pub free: u64,
    pub total: u64,
    /// 是否是系统盘（默认不往这里迁）。
    pub system: bool,
    pub free_text: String,
    pub total_text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub supported: bool,
    pub platform_note: Option<String>,
    pub home: String,
    pub dest_default: String,
    pub dirs: Vec<CacheDir>,
    pub total_files: u64,
    pub total_bytes: u64,
    pub total_text: String,
    /// 需要退出的进程；非空时 `can_run` 为 false。
    pub blocking: Vec<String>,
    /// 建议退出的进程（不阻塞）。
    pub warnings: Vec<String>,
    pub can_run: bool,
    pub blocked_reason: Option<String>,
    pub drives: Vec<Drive>,
    /// 至少有一个可用的非系统盘。
    pub has_dest: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepLog {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

/// 一条迁移目标：要迁哪个目录 + 它自己的目标根目录。
///
/// 国内版与国际版**可以迁到不同的盘/文件夹**（默认建议同一个根目录，
/// 但每个目录都能单独覆盖，不互相牵制）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveTarget {
    /// 家目录下的目录名，如 `.workbuddy`。
    pub name: String,
    /// 目标根目录；最终落点 = `<dest>\<name>`。
    pub dest: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    /// scan / copy / verify / link / rollback / done
    pub phase: String,
    pub detail: String,
    /// 当前条目序号 / 总条目数（1 起）。
    pub index: u32,
    pub total: u32,
    /// 0-100，整体进度。
    pub percent: u32,
}

// ---------------------------------------------------------------- 平台辅助

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// 是否是目录联接 / 符号链接（Windows 上判断重解析点）。
fn is_reparse(p: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        return std::fs::symlink_metadata(p)
            .map(|m| m.file_attributes() & 0x400 != 0)
            .unwrap_or(false);
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::fs::symlink_metadata(p)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
    }
}

/// 读联接指向；Windows 的 `read_link` 会带 `\\?\` 前缀，这里剥掉。
fn link_target(p: &Path) -> Option<String> {
    let raw = std::fs::read_link(p).ok()?;
    let s = raw.to_string_lossy().to_string();
    Some(match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.starts_with("UNC\\") => format!(r"\\{}", &rest[4..]),
        Some(rest) => rest.to_string(),
        None => s,
    })
}

/// 统计目录（文件数, 总字节）。**不跟随联接**，避免把目标盘再算一遍。
fn scan(p: &Path) -> (u64, u64) {
    if !p.is_dir() {
        return (0, 0);
    }
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![p.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                // 只对目录做一次 stat：目录数远少于文件数
                if is_reparse(&path) {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file() {
                if let Ok(meta) = entry.metadata() {
                    bytes += meta.len();
                }
                files += 1;
            }
        }
    }
    (files, bytes)
}

/// 目录是否非空（迁移后的可读性探测）。
fn dir_has_entries(p: &Path) -> bool {
    std::fs::read_dir(p)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// 读盘符可用空间。Windows 走一次 PowerShell 拿全部逻辑盘，其它平台返回空。
fn list_drives() -> Vec<Drive> {
    #[cfg(target_os = "windows")]
    {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        let script = "Get-CimInstance Win32_LogicalDisk -Filter \"DriveType=3\" | \
             ForEach-Object { '{0}|{1}|{2}' -f $_.DeviceID, $_.FreeSpace, $_.Size }";
        let mut out = Vec::new();
        if let Some(stdout) = process::ps_output(script, 8) {
            for line in stdout.lines() {
                let parts: Vec<&str> = line.trim().split('|').collect();
                if parts.len() != 3 {
                    continue;
                }
                let letter = parts[0].trim().to_string();
                let Ok(free) = parts[1].trim().parse::<u64>() else {
                    continue;
                };
                let total = parts[2].trim().parse::<u64>().unwrap_or(0);
                out.push(Drive {
                    system: letter.eq_ignore_ascii_case(&system),
                    letter,
                    free,
                    total,
                    free_text: human(free),
                    total_text: human(total),
                });
            }
        }
        out.sort_by(|a, b| b.free.cmp(&a.free));
        return out;
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
    }
}

/// 当前在跑的、与迁移相关的进程 → (必须退出的, 建议退出的)。
fn running_procs() -> (Vec<String>, Vec<String>) {
    #[cfg(target_os = "windows")]
    {
        let mut blocking = Vec::new();
        for img in BLOCKING_IMAGES {
            if !process::windows_tasklist_image_rows(img).is_empty() {
                blocking.push(img.to_string());
            }
        }
        let mut warnings = Vec::new();
        for img in WARN_IMAGES {
            if !process::windows_tasklist_image_rows(img).is_empty() {
                warnings.push(img.to_string());
            }
        }
        return (blocking, warnings);
    }
    #[cfg(not(target_os = "windows"))]
    {
        // 两个常量在非 Windows 上也要用上：CI 开着 `-D warnings`，
        // 只要有一个分支没引用它们，mac/Linux 的构建就会因为
        // 「constant is never used」直接失败（v1.0.4 就是这么挂的）。
        let probe = |img: &str| {
            let name = img.strip_suffix(".exe").unwrap_or(img);
            process::cmd_builder("pgrep")
                .args(["-x", name])
                .output()
                .map(|o| o.status.success() && !o.stdout.is_empty())
                .unwrap_or(false)
        };
        let blocking: Vec<String> = BLOCKING_IMAGES
            .iter()
            .filter(|img| probe(img))
            .map(|img| img.to_string())
            .collect();
        let warnings: Vec<String> = WARN_IMAGES
            .iter()
            .filter(|img| probe(img))
            .map(|img| img.to_string())
            .collect();
        (blocking, warnings)
    }
}

// ---------------------------------------------------------------- 条目 / 计划

/// 参与迁移的条目：(目录名, 展示名, 是否参与「迁移全部」)。
fn entries() -> Vec<(String, String, bool)> {
    let mut v = Vec::new();
    for e in Edition::ALL {
        v.push((
            e.data_dir_name().to_string(),
            format!("{} WorkBuddy", e.label()),
            true,
        ));
    }
    for (n, l) in EXTRA_DIRS {
        v.push((n.to_string(), l.to_string(), true));
    }
    v
}

/// 找一个还没被占用的备份名。
fn backup_path_for(src: &Path) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let name = src
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    src.with_file_name(format!("{name}{BACKUP_MARK}{stamp}"))
}

/// 列出某个源目录当前挂着的备份（取名字最大的一个）。
fn find_backup(src: &Path) -> Option<PathBuf> {
    let dir = src.parent()?;
    let name = src.file_name()?.to_string_lossy().to_string();
    let prefix = format!("{name}{BACKUP_MARK}");
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with(&prefix))
                .unwrap_or(false)
        })
        .collect();
    found.sort();
    found.pop()
}

fn dir_status(name: &str, label: &str, movable: bool, with_size: bool) -> CacheDir {
    let path = home_dir().join(name);
    let exists = path.exists();
    let is_link = exists && is_reparse(&path);
    let (files, bytes) = if exists && with_size {
        scan(&path)
    } else {
        (0, 0)
    };
    CacheDir {
        name: name.to_string(),
        label: label.to_string(),
        path: path.to_string_lossy().to_string(),
        exists,
        is_link,
        link_target: if is_link { link_target(&path) } else { None },
        backup: find_backup(&path).map(|p| p.to_string_lossy().to_string()),
        files,
        bytes,
        movable,
        size_text: human(bytes),
    }
}

/// 只读体检。`dest` 只用于覆盖目标目录，其余全是探测。
pub fn plan(dest: Option<String>) -> Plan {
    let home = home_dir();
    let supported = cfg!(target_os = "windows");
    let platform_note = if supported {
        None
    } else {
        Some("缓存迁移目前只支持 Windows：依赖 NTFS 目录联接（mklink /J）。".to_string())
    };

    let dirs: Vec<CacheDir> = entries()
        .into_iter()
        .map(|(n, l, m)| dir_status(&n, &l, m, true))
        .collect();

    let total_files = dirs.iter().map(|d| d.files).sum();
    let total_bytes = dirs.iter().map(|d| d.bytes).sum();

    let drives = list_drives();
    let dest_default = dest
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| {
            let pick = drives
                .iter()
                .find(|d| !d.system && d.free > total_bytes)
                .or_else(|| drives.iter().find(|d| !d.system))
                .or_else(|| drives.first());
            match pick {
                Some(d) => format!("{}\\{}", d.letter, DEST_FOLDER),
                None => format!(r"D:\{DEST_FOLDER}"),
            }
        });

    let (blocking, warnings) = running_procs();
    let has_dest = drives.iter().any(|d| !d.system);
    let blocked_reason = if !supported {
        platform_note.clone()
    } else if !blocking.is_empty() {
        Some(format!(
            "{} 正在运行。窗口关掉不够，要在托盘图标上右键 → 退出，两个应用都退。",
            blocking.join(" / ")
        ))
    } else if !has_dest {
        Some("没有可用的非系统盘。".to_string())
    } else {
        None
    };

    Plan {
        supported,
        platform_note,
        home: home.to_string_lossy().to_string(),
        dest_default,
        dirs,
        total_files,
        total_bytes,
        total_text: human(total_bytes),
        blocking,
        warnings,
        can_run: blocked_reason.is_none(),
        blocked_reason,
        drives,
        has_dest,
    }
}

/// 校验并规整用户给的目标目录。
fn normalize_dest(dest: &str) -> Result<PathBuf, String> {
    let t = dest.trim();
    if t.is_empty() {
        return Err("请先选目标目录".to_string());
    }
    let p = PathBuf::from(t);
    if !p.is_absolute() {
        return Err(format!("目标必须是绝对路径：{t}"));
    }
    let home = home_dir();
    // 目标不能落在任何一个源目录里面，否则 robocopy 会自我递归。
    for (n, _, _) in entries() {
        let src = home.join(&n);
        if p.starts_with(&src) || src.starts_with(&p) {
            return Err(format!(
                "目标目录与 {} 互相嵌套，会自我递归，请换一个位置。",
                src.to_string_lossy()
            ));
        }
    }
    // 也不能落在应用安装目录里（客户端会拒绝这种布局）。
    if p.to_string_lossy().to_lowercase().contains(r"\adobeall\workbuddy") {
        return Err("目标不能放在应用安装目录内部。".to_string());
    }
    Ok(p)
}

/// 取路径的盘符（`D:` 形式）。
fn drive_of(p: &Path) -> String {
    p.components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- 联接操作

#[cfg(target_os = "windows")]
fn make_junction(link: &Path, target: &Path) -> Result<(), String> {
    let out = process::cmd_builder("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .map_err(|e| format!("调用 mklink 失败：{e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "mklink 失败（退出码 {}）：{}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).trim()
        ))
    }
}

#[cfg(not(target_os = "windows"))]
fn make_junction(_link: &Path, _target: &Path) -> Result<(), String> {
    Err("当前平台不支持目录联接".to_string())
}

/// 只删联接本身；`remove_dir` 对重解析点等价于删链接，不会动目标数据。
fn remove_junction(link: &Path) {
    if std::fs::remove_dir(link).is_err() {
        let _ = process::cmd_builder("cmd")
            .args(["/c", "rmdir"])
            .arg(link)
            .output();
    }
}

/// robocopy 复制；返回退出码（<8 视为成功）。
#[cfg(target_os = "windows")]
fn robocopy(src: &Path, dst: &Path) -> Result<i32, String> {
    let out = process::cmd_builder("robocopy")
        .arg(src)
        .arg(dst)
        .args([
            "/E", "/COPY:DAT", "/DCOPY:DAT", "/R:1", "/W:1", "/MT:16", "/XJ", "/NFL", "/NDL",
            "/NJH", "/NJS",
        ])
        .output()
        .map_err(|e| format!("调用 robocopy 失败：{e}"))?;
    let code = out.status.code().unwrap_or(-1);
    if code >= 8 {
        return Err(format!(
            "robocopy 返回 {code}（≥8 表示有文件没能复制）。源目录未改动。"
        ));
    }
    Ok(code)
}

#[cfg(not(target_os = "windows"))]
fn robocopy(_src: &Path, _dst: &Path) -> Result<i32, String> {
    Err("当前平台不支持 robocopy".to_string())
}

// ---------------------------------------------------------------- 迁移

/// 人类可读的体积。
pub fn human(n: u64) -> String {
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

fn push_log(logs: &mut Vec<StepLog>, name: &str, ok: bool, message: String) {
    logs.push(StepLog {
        name: name.to_string(),
        ok,
        message,
    });
}

fn emit(
    progress: Option<&dyn Fn(Progress)>,
    phase: &str,
    detail: &str,
    idx: u32,
    total: u32,
    pct: u32,
) {
    if let Some(f) = progress {
        f(Progress {
            phase: phase.to_string(),
            detail: detail.to_string(),
            index: idx,
            total,
            percent: pct.min(100),
        });
    }
}

/// 迁移单个目录。返回 (是否成功, 日志, 备份路径)。
fn migrate_one(
    name: &str,
    label: &str,
    dest_root: &Path,
    idx: u32,
    total: u32,
    progress: Option<&dyn Fn(Progress)>,
) -> (bool, Vec<StepLog>, Option<PathBuf>) {
    let src = home_dir().join(name);
    let dst = dest_root.join(name);
    let mut logs: Vec<StepLog> = Vec::new();

    // 每个条目占 1/total 的进度，内部再切 5 段。
    let base = ((idx - 1) as f32 / total as f32 * 100.0) as u32;
    let span = (100.0 / total as f32) as u32;
    let at = |k: u32| base + span * k / 5;

    if !src.exists() {
        push_log(&mut logs, label, true, "目录不存在，跳过".to_string());
        return (true, logs, None);
    }
    if is_reparse(&src) {
        let t = link_target(&src).unwrap_or_else(|| "?".to_string());
        push_log(&mut logs, label, true, format!("已经是联接 → {t}，跳过"));
        return (true, logs, None);
    }

    emit(progress, "scan", &format!("统计 {name}"), idx, total, at(1));
    let (nf, nb) = scan(&src);
    if nf == 0 && nb == 0 {
        push_log(&mut logs, label, false, "目录为空，跳过以免误操作".to_string());
        return (false, logs, None);
    }

    emit(
        progress,
        "copy",
        &format!("复制 {name}（{nf} 个文件 / {}）", human(nb)),
        idx,
        total,
        at(2),
    );
    if let Err(e) = robocopy(&src, &dst) {
        push_log(&mut logs, label, false, e);
        return (false, logs, None);
    }

    emit(progress, "verify", &format!("校验 {name}"), idx, total, at(3));
    let (df, db) = scan(&dst);
    if (df, db) != (nf, nb) {
        push_log(
            &mut logs,
            label,
            false,
            format!("副本校验不一致（文件 {nf}→{df}，字节 {nb}→{db}）。源目录未改动。"),
        );
        return (false, logs, None);
    }

    let backup = backup_path_for(&src);
    emit(
        progress,
        "link",
        &format!("改名并建立联接 {name}"),
        idx,
        total,
        at(4),
    );
    if let Err(e) = std::fs::rename(&src, &backup) {
        push_log(
            &mut logs,
            label,
            false,
            format!("改名失败（多半是文件被占用）：{e}。请确认客户端已完全退出后重试。"),
        );
        return (false, logs, None);
    }
    if let Err(e) = make_junction(&src, &dst) {
        let _ = std::fs::rename(&backup, &src);
        push_log(&mut logs, label, false, format!("{e}；已回滚，数据仍在原处。"));
        return (false, logs, None);
    }
    if !is_reparse(&src) || !dir_has_entries(&src) {
        remove_junction(&src);
        let _ = std::fs::rename(&backup, &src);
        push_log(&mut logs, label, false, "联接已建立但读不到内容；已回滚。".to_string());
        return (false, logs, None);
    }

    push_log(
        &mut logs,
        label,
        true,
        format!(
            "已迁移 → {}（{nf} 个文件 / {}）",
            dst.to_string_lossy(),
            human(nb)
        ),
    );
    emit(progress, "link", &format!("{name} 完成"), idx, total, at(5));
    (true, logs, Some(backup))
}

/// 执行迁移。`only` 为空表示全部参与条目。
pub fn run(targets: Vec<MoveTarget>, progress: Option<&dyn Fn(Progress)>) -> Result<Value, String> {
    if !cfg!(target_os = "windows") {
        return Err("缓存迁移目前只支持 Windows（依赖 NTFS 目录联接）。".to_string());
    }
    let (blocking, _) = running_procs();
    if !blocking.is_empty() {
        return Err(format!(
            "{} 正在运行。请先完全退出（托盘图标右键 → 退出，两个应用都退）再迁移。",
            blocking.join(" / ")
        ));
    }
    if targets.is_empty() {
        return Err("没有选中任何要迁移的目录。".to_string());
    }

    let home = home_dir();
    let all = entries();

    // 每条目标的最终落点、体积都单算 —— 两个版本可以落在不同的盘。
    struct Job {
        name: String,
        label: String,
        dest_root: PathBuf,
        dst: PathBuf,
        bytes: u64,
        files: u64,
    }
    let mut jobs: Vec<Job> = Vec::new();
    for t in &targets {
        let Some((_, label, movable)) = all.iter().find(|(n, _, _)| n == &t.name) else {
            return Err(format!("未知目录：{}", t.name));
        };
        if !movable {
            return Err(format!("{} 不支持迁移。", t.name));
        }
        let src = home.join(&t.name);
        if !src.exists() {
            continue;
        }
        if is_reparse(&src) {
            // 已经是联接，跳过（前端一般不会把它传上来）
            continue;
        }
        let dest_root = normalize_dest(&t.dest)?;
        let dst = dest_root.join(&t.name);
        let (files, bytes) = scan(&src);
        if files == 0 && bytes == 0 {
            return Err(format!("{} 是空目录，已跳过以免误操作。", t.name));
        }
        jobs.push(Job {
            name: t.name.clone(),
            label: label.clone(),
            dest_root,
            dst,
            bytes,
            files,
        });
    }
    if jobs.is_empty() {
        return Err("选中的目录都已经是联接或不存在，无需迁移。".to_string());
    }

    // 空间检查按**盘符**汇总：两个目录可以落在不同的盘，各算各的。
    let drives = list_drives();
    let mut per_drive: BTreeMap<String, u64> = BTreeMap::new();
    for j in &jobs {
        *per_drive.entry(drive_of(&j.dest_root)).or_insert(0) += j.bytes;
    }
    for (letter, need) in &per_drive {
        if let Some(d) = drives.iter().find(|d| d.letter.eq_ignore_ascii_case(letter)) {
            if d.free < need + need / 20 {
                return Err(format!(
                    "{} 只剩 {} 可用，装不下（这个盘上要放 {}，建议留 5% 余量）。",
                    d.letter,
                    human(d.free),
                    human(*need)
                ));
            }
        }
    }

    let total = jobs.len() as u32;
    let mut logs: Vec<StepLog> = Vec::new();
    let mut moved: Vec<String> = Vec::new();
    let mut backups: Vec<String> = Vec::new();
    let mut placed: Vec<Value> = Vec::new();
    let mut ok = true;

    for (i, job) in jobs.iter().enumerate() {
        let (one_ok, mut one_logs, backup) = migrate_one(
            &job.name,
            &job.label,
            &job.dest_root,
            i as u32 + 1,
            total,
            progress,
        );
        logs.append(&mut one_logs);
        if !one_ok {
            ok = false;
            break;
        }
        // 跳过（本来就是联接 / 不存在）也算成功，但不计入「本次迁移」。
        if !is_reparse(&home.join(&job.name)) {
            continue;
        }
        moved.push(job.name.clone());
        placed.push(json!({
            "name": job.name,
            "from": home.join(&job.name).to_string_lossy(),
            "to": job.dst.to_string_lossy(),
            "files": job.files,
            "bytes": job.bytes,
        }));
        if let Some(b) = backup {
            backups.push(b.to_string_lossy().to_string());
        }
    }

    emit(progress, "done", "完成", total, total, 100);
    Ok(json!({
        "ok": ok,
        "moved": moved,
        "backups": backups,
        "logs": logs,
        "placed": placed,
    }))
}

// ---------------------------------------------------------------- 回滚 / 清理 / 验证

/// 回滚：删联接 + 把最近的 `.moved-*` 改名回来。
pub fn rollback(progress: Option<&dyn Fn(Progress)>) -> Result<Value, String> {
    if !cfg!(target_os = "windows") {
        return Err("缓存迁移目前只支持 Windows。".to_string());
    }
    let (blocking, _) = running_procs();
    if !blocking.is_empty() {
        return Err(format!(
            "{} 正在运行，请先完全退出再回滚。",
            blocking.join(" / ")
        ));
    }

    let home = home_dir();
    let list = entries();
    let total = list.len() as u32;
    let mut logs: Vec<StepLog> = Vec::new();
    let mut restored: Vec<String> = Vec::new();

    for (i, (name, label, _)) in list.iter().enumerate() {
        let src = home.join(name);
        emit(
            progress,
            "rollback",
            label,
            i as u32 + 1,
            total,
            (i as u32 * 100) / total,
        );
        if !is_reparse(&src) {
            push_log(&mut logs, label, true, "不是联接，无需回滚".to_string());
            continue;
        }
        match find_backup(&src) {
            None => push_log(
                &mut logs,
                label,
                false,
                "是联接但找不到 .moved-* 备份，跳过（需人工处理）".to_string(),
            ),
            Some(backup) => {
                remove_junction(&src);
                match std::fs::rename(&backup, &src) {
                    Ok(()) => {
                        restored.push(name.clone());
                        push_log(
                            &mut logs,
                            label,
                            true,
                            format!("已恢复 {}", backup.to_string_lossy()),
                        );
                    }
                    Err(e) => push_log(
                        &mut logs,
                        label,
                        false,
                        format!("删了联接但恢复备份失败：{e}"),
                    ),
                }
            }
        }
    }

    emit(progress, "done", "回滚完成", total, total, 100);
    Ok(json!({ "ok": true, "restored": restored, "logs": logs }))
}

// ---------------------------------------------------------------- 回收站

/// `SHFILEOPSTRUCTW`。`#[repr(C)]` 在 x64/x86 上都对得上 Win32 的布局。
#[cfg(windows)]
#[repr(C)]
struct ShFileOpStructW {
    hwnd: *mut core::ffi::c_void,
    w_func: u32,
    p_from: *const u16,
    p_to: *const u16,
    f_flags: u16,
    f_any_operations_aborted: i32,
    h_name_mappings: *mut core::ffi::c_void,
    lpsz_progress_title: *const u16,
}

#[cfg(windows)]
#[link(name = "shell32")]
extern "system" {
    fn SHFileOperationW(lp_file_op: *mut ShFileOpStructW) -> i32;
}

/// 把整棵目录丢进回收站。
///
/// **为什么要绕这一下**：实测这台机器删一个文件要 **21.6 ms**（而遍历一个条目只要
/// 0.47 ms）—— 25 万个的备份按条删要**一个半小时**，用户看着就是卡死了。
/// `SHFileOperation` 把整个目录当一个对象处理，同卷内就是一次移动，毫秒级完成；
/// 顺带还给了「删错了能从回收站还原」的退路。
/// 代价是空间要**清空回收站之后**才真正释放，所以调用方必须把这句话告诉用户。
#[cfg(windows)]
fn move_to_recycle_bin(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    const FO_DELETE: u32 = 3;
    const FOF_SILENT: u16 = 0x0004;
    const FOF_NOCONFIRMATION: u16 = 0x0010;
    const FOF_ALLOWUNDO: u16 = 0x0040;
    const FOF_NOCONFIRMMKDIR: u16 = 0x0200;
    const FOF_NOERRORUI: u16 = 0x0400;

    // pFrom 要的是「\0 分隔 + 末尾再来一个 \0 收尾」的宽字符串；单条路径就是双 \0 结尾。
    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.push(0);
    from.push(0);

    let mut op = ShFileOpStructW {
        hwnd: std::ptr::null_mut(),
        w_func: FO_DELETE,
        p_from: from.as_ptr(),
        p_to: std::ptr::null(),
        f_flags: FOF_SILENT
            | FOF_NOCONFIRMATION
            | FOF_ALLOWUNDO
            | FOF_NOCONFIRMMKDIR
            | FOF_NOERRORUI,
        f_any_operations_aborted: 0,
        h_name_mappings: std::ptr::null_mut(),
        lpsz_progress_title: std::ptr::null(),
    };

    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(format!("移入回收站失败（SHFileOperation 返回 {rc}）"));
    }
    if op.f_any_operations_aborted != 0 {
        return Err("移入回收站被中断".to_string());
    }
    Ok(())
}

/// 清掉只读属性。
///
/// Windows 上删除只读文件直接返回 `ERROR_ACCESS_DENIED`（页面上那句
/// 「拒绝访问。(os error 5)」），所以删之前必须先把只读摘掉。
/// WorkBuddy 自己写的 `memory\*_memory.md` 就是只读的，备份目录里一定有一批。
fn clear_readonly(p: &Path) {
    let Ok(md) = std::fs::metadata(p) else {
        return;
    };
    let mut perm = md.permissions();
    if perm.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        let _ = std::fs::set_permissions(p, perm);
    }
}

/// 删一个文件/目录，失败就重试几次（摘掉只读、等一下再试）。
///
/// 失败的常见原因是**瞬时占用**（杀软在扫、应用刚写的日志还在句柄上），
/// 等一下基本就过去了；也可能是只读属性，所以每次重试前都再摘一遍。
fn remove_with_retry(p: &Path, is_dir: bool) -> std::io::Result<()> {
    let mut last: Option<std::io::Error> = None;
    for attempt in 0..3u32 {
        let r = if is_dir {
            std::fs::remove_dir(p)
        } else {
            std::fs::remove_file(p)
        };
        match r {
            Ok(()) => return Ok(()),
            Err(e) => {
                // 已经被别人删掉了 = 也算成功
                if !p.exists() {
                    return Ok(());
                }
                last = Some(e);
                clear_readonly(p);
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("未知错误")))
}

/// 递归删除目录树，顺带统计「多少个文件 / 多少字节」。
///
/// 刻意不用 `std::fs::remove_dir_all`：它撞到**任何一个**不肯走的条目就整棵放弃，
/// 而且错误里只有「拒绝访问」、不告诉你是哪个条目 —— 用户看到的
/// 「删除失败：拒绝访问。(os error 5)」正是这么来的，连查都没法查
/// （实测那棵树既没有只读目录、也没有重解析点和 ACL 问题）。
///
/// 这里改成：
/// 1. 一次遍历同时把体积算出来（省掉 `scan()` 那一趟）；
/// 2. 先自底向上摘掉只读属性（Windows 上只读目录/文件都可能直接拒删）；
/// 3. 逐条 `remove_file` / `remove_dir`，**单条失败不中断整棵**，
///    重试 3 次，仍失败就记下来继续删别的；
/// 4. 最后把「删不掉的到底是哪几个」报出来。
fn force_remove_dir_all(root: &Path, on_tick: &dyn Fn(u64, u64)) -> Result<(u64, u64), String> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut bytes: u64 = 0;
    let mut stack = vec![root.to_path_buf()];

    while let Some(d) = stack.pop() {
        dirs.push(d.clone());
        let rd = std::fs::read_dir(&d).map_err(|e| format!("读取 {} 失败：{e}", d.display()))?;
        for ent in rd {
            let ent = ent.map_err(|e| format!("读取 {} 的条目失败：{e}", d.display()))?;
            let p = ent.path();
            if ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(p);
            } else {
                if let Ok(md) = ent.metadata() {
                    bytes += md.len();
                }
                files.push(p);
            }
        }
    }

    let dbg = std::env::var("WB_CM_DEBUG").is_ok();
    let t_walk = std::time::Instant::now();

    // 1) 先摘属性（文件 + 目录都摘）
    for p in files.iter().chain(dirs.iter()) {
        clear_readonly(p);
    }
    if dbg {
        eprintln!("[dbg] 遍历 {} 个目录 + {} 个文件：{:?}", dirs.len(), files.len(), t_walk.elapsed());
    }
    let t_attr = std::time::Instant::now();

    if dbg {
        eprintln!("[dbg] 摘只读属性：{:?}", t_attr.elapsed());
    }
    let t_del = std::time::Instant::now();

    // 2) 删文件
    let all = files.len() as u64;
    let mut failed: u64 = 0;
    let mut samples: Vec<String> = Vec::new();
    for (i, p) in files.iter().enumerate() {
        if let Err(e) = remove_with_retry(p, false) {
            failed += 1;
            if samples.len() < 3 {
                samples.push(format!("{}（{e}）", p.display()));
            }
        }
        if i % 1000 == 0 {
            on_tick(i as u64, all);
        }
    }
    on_tick(all, all);

    if dbg {
        eprintln!(
            "[dbg] 删 {} 个文件：{:?}（{:.2} ms/文件）",
            all,
            t_del.elapsed(),
            t_del.elapsed().as_secs_f64() * 1000.0 / all.max(1) as f64
        );
    }
    let t_dir = std::time::Instant::now();

    // 3) 删目录：子目录必须先于父目录（按路径层数从深到浅）
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for d in &dirs {
        if let Err(e) = remove_with_retry(d, true) {
            if d.exists() {
                failed += 1;
                if samples.len() < 3 {
                    samples.push(format!("{}（{e}）", d.display()));
                }
            }
        }
    }

    if dbg {
        eprintln!("[dbg] 删 {} 个目录：{:?}", dirs.len(), t_dir.elapsed());
    }

    if root.exists() {
        return Err(if samples.is_empty() {
            format!("{} 没能删干净，可能有进程正占着它", root.display())
        } else {
            format!("有 {failed} 个条目删不掉，例如：{}", samples.join("；"))
        });
    }
    Ok((all, bytes))
}

/// 校验路径是「家目录直接子项 + 名字含 .moved- + 不是联接」。清理的门槛。
fn safe_backup_path(p: &Path) -> Result<(), String> {
    let home = home_dir();
    if p.parent() != Some(home.as_path()) {
        return Err(format!("拒绝：{} 不是家目录的直接子项", p.to_string_lossy()));
    }
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if !name.contains(BACKUP_MARK) {
        return Err(format!("拒绝：{name} 不含 {BACKUP_MARK} 标记"));
    }
    if !p.is_dir() {
        return Err(format!("拒绝：{} 不是目录", p.to_string_lossy()));
    }
    if is_reparse(p) {
        return Err(format!("拒绝：{} 是联接，不能删", p.to_string_lossy()));
    }
    Ok(())
}

/// 在文件管理器里打开一个**目录**。
///
/// 迁移之后「设置页」和资源管理器看到的仍然是原来的家目录路径（联接对程序透明），
/// 所以页面上要有个口子能把人带到数据真正所在的地方 —— 这个函数就是那个口子。
/// 只接受「存在且确实是目录」的绝对路径，不接 URL、不接文件。
pub fn open_path(path: &str) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("路径为空".to_string());
    }
    let p = PathBuf::from(trimmed);
    if !p.is_absolute() {
        return Err(format!("不是绝对路径：{}", p.display()));
    }
    if !p.is_dir() {
        return Err(format!("目录不存在或不是文件夹：{}", p.display()));
    }

    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("explorer.exe").arg(&p).spawn();
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(&p).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(&p).spawn();

    spawned.map(|_| ()).map_err(|e| format!("打不开 {}：{e}", p.display()))
}

/// 列出当前可清理的备份（带体积）。
pub fn list_backups() -> Vec<Value> {
    let home = home_dir();
    let Ok(rd) = std::fs::read_dir(&home) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().contains(BACKUP_MARK))
                .unwrap_or(false)
        })
        .map(|p| {
            let (files, bytes) = scan(&p);
            json!({
                "path": p.to_string_lossy(),
                "name": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                "files": files,
                "bytes": bytes,
                "sizeText": human(bytes),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b["bytes"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["bytes"].as_u64().unwrap_or(0))
    });
    out
}

/// 删除备份目录。只接受显式传入的、通过安全校验的路径。
pub fn cleanup(paths: &[String], progress: Option<&dyn Fn(Progress)>) -> Result<Value, String> {
    if paths.is_empty() {
        return Err("没有选中要清理的备份。".to_string());
    }
    let total = paths.len() as u32;
    let mut removed = Vec::new();
    let mut logs: Vec<StepLog> = Vec::new();

    for (i, raw) in paths.iter().enumerate() {
        let p = PathBuf::from(raw);
        let idx = i as u32 + 1;
        let label = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| raw.clone());
        if let Err(e) = safe_backup_path(&p) {
            push_log(&mut logs, raw, false, e);
            continue;
        }

        // 快路径：整棵目录丢进回收站。
        // 这台机器删 1 个文件要 20 ms 上下，25 万个按条删要一个半小时 —— 不能走那条路。
        let fast: Result<(), String> = {
            #[cfg(windows)]
            {
                move_to_recycle_bin(&p)
            }
            #[cfg(not(windows))]
            {
                Err("非 Windows 平台不支持回收站".to_string())
            }
        };

        match fast {
            Ok(()) => {
                removed.push(raw.clone());
                push_log(
                    &mut logs,
                    raw,
                    true,
                    "已移入回收站（可以在回收站里还原；清空回收站之后空间才真正释放）".to_string(),
                );
            }
            Err(why) => {
                // 回退：逐条删。慢，但有进度、单条失败也不会把整棵带停。
                let base = (i as f64) / (total as f64) * 100.0;
                let span = 100.0 / (total as f64);
                let tick = |done: u64, all: u64| {
                    emit(
                        progress,
                        "cleanup",
                        &format!("{label} · 已处理 {done} 个文件"),
                        idx,
                        total,
                        (base + span * if all == 0 { 0.0 } else { done as f64 / all as f64 })
                            .round()
                            .clamp(0.0, 100.0) as u32,
                    );
                };
                tick(0, 0);

                match force_remove_dir_all(&p, &tick) {
                    Ok((files, bytes)) => {
                        removed.push(raw.clone());
                        push_log(
                            &mut logs,
                            raw,
                            true,
                            format!("已彻底删除（{} 个文件 / {}）", files, human(bytes)),
                        );
                    }
                    // 这里已经是「具体哪个路径失败」的完整原因
                    Err(e) => push_log(&mut logs, raw, false, format!("{why}；逐条删除也没成功：{e}")),
                }
            }
        }
    }
    emit(progress, "done", "清理完成", total, total, 100);
    Ok(json!({ "ok": !removed.is_empty(), "removed": removed, "logs": logs }))
}

/// 只读验证当前联接状态。
pub fn verify() -> Value {
    let list: Vec<CacheDir> = entries()
        .into_iter()
        .map(|(n, l, m)| dir_status(&n, &l, m, true))
        .collect();
    let (blocking, warnings) = running_procs();
    json!({
        "dirs": list,
        "blocking": blocking,
        "warnings": warnings,
        "backups": list_backups(),
    })
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一棵「像真备份那样」的树：深层嵌套 + 只读文件 + 隐藏文件。
    fn make_tree(root: &Path) {
        let deep = root.join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join("plain.txt"), b"x").unwrap();
        std::fs::write(deep.join("inner.txt"), b"y").unwrap();

        // 只读文件 —— 真机上 WorkBuddy 的 memory\*_memory.md 就长这样
        let ro = deep.join("memory.md");
        std::fs::write(&ro, b"read-only").unwrap();
        let mut perm = std::fs::metadata(&ro).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&ro, perm).unwrap();
    }

    /// 整棵删干净，并且「多少个文件 / 多少字节」要在同一次遍历里算对。
    #[test]
    fn force_remove_dir_all_removes_everything() {
        let root = std::env::temp_dir().join(format!("wb-cm-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        make_tree(&root);

        let (files, bytes) = force_remove_dir_all(&root, &|_, _| {}).unwrap();
        assert_eq!(files, 3, "应当统计到 3 个文件");
        assert!(bytes > 0, "字节数要算出来");
        assert!(!root.exists(), "整棵树都不该剩下");
    }

    /// 保险丝：路径不存在时不该 panic，错误信息里要带上路径。
    #[test]
    fn force_remove_dir_all_reports_missing_path() {
        let missing = std::env::temp_dir().join("wb-cm-not-a-dir-9f8a7b6c");
        let err = force_remove_dir_all(&missing, &|_, _| {}).unwrap_err();
        assert!(err.contains("wb-cm-not-a-dir-9f8a7b6c"), "错误里要能看出是哪条路径：{err}");
    }

    /// 只读属性摘掉之后就是普通文件了（清理能过，备份回滚也不受影响）。
    #[test]
    fn clear_readonly_works() {
        let f = std::env::temp_dir().join(format!("wb-cm-ro-{}.txt", std::process::id()));
        std::fs::write(&f, b"x").unwrap();
        let mut perm = std::fs::metadata(&f).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&f, perm).unwrap();
        assert!(std::fs::metadata(&f).unwrap().permissions().readonly());

        clear_readonly(&f);
        assert!(!std::fs::metadata(&f).unwrap().permissions().readonly());
        let _ = std::fs::remove_file(&f);
    }

    /// 真机排查用：删掉 `WB_CM_DELETE_DIR` 指向的目录树，并报出耗时。
    ///
    /// 只在手动执行时跑，且必须显式给环境变量：
    /// ```bash
    /// WB_CM_DELETE_DIR='F://tmp//copy' cargo test -p wb-switch-core --lib \
    ///     cache_move::tests::delete_tree_from_env -- --ignored --nocapture
    /// ```
    /// 用途：备份删不掉时，先拿**副本**验证删除逻辑，再决定要不要动真的那份。
    #[test]
    #[ignore = "手动执行：需要环境变量 WB_CM_DELETE_DIR"]
    fn delete_tree_from_env() {
        let raw = std::env::var("WB_CM_DELETE_DIR").expect("需要环境变量 WB_CM_DELETE_DIR");
        let root = PathBuf::from(raw);
        assert!(root.is_dir(), "{} 不是目录", root.display());

        let t0 = std::time::Instant::now();
        let (files, bytes) = force_remove_dir_all(&root, &|_, _| {}).expect("删除失败");
        println!(
            "✓ 删掉 {files} 个文件 / {}，用时 {:?}  —— {}",
            human(bytes),
            t0.elapsed(),
            root.display()
        );
        assert!(!root.exists(), "应当删干净");
    }

    /// 真机验证回收站快路径：`WB_CM_DELETE_DIR` 指的目录应当被**秒级**移入回收站。
    /// 移进去的东西可以在回收站里还原，不会真的丢。
    #[cfg(windows)]
    #[test]
    #[ignore = "手动执行：需要环境变量 WB_CM_DELETE_DIR"]
    fn recycle_tree_from_env() {
        let raw = std::env::var("WB_CM_DELETE_DIR").expect("需要环境变量 WB_CM_DELETE_DIR");
        let root = PathBuf::from(raw);
        assert!(root.is_dir(), "{} 不是目录", root.display());

        let t0 = std::time::Instant::now();
        move_to_recycle_bin(&root).expect("移入回收站失败");
        println!("✓ 移入回收站用时 {:?} —— {}", t0.elapsed(), root.display());
        assert!(!root.exists(), "原路径应当已经消失");
    }

    /// 反证用：老写法 `std::fs::remove_dir_all` 在同一棵树上到底行不行。
    ///
    /// 只在手动执行时跑：
    /// ```bash
    /// WB_CM_DELETE_DIR='F://tmp//copy2' cargo test -p wb-switch-core --lib \
    ///     cache_move::tests::delete_tree_from_env_legacy -- --ignored --nocapture
    /// ```
    /// 它存在的意义：证明「逐条删 + 重试」不是多此一举，而是必需的 ——
    /// 老写法一旦失败，连是哪条路径都问不出来。
    #[test]
    #[ignore = "手动执行：需要环境变量 WB_CM_DELETE_DIR"]
    fn delete_tree_from_env_legacy() {
        let raw = std::env::var("WB_CM_DELETE_DIR").expect("需要环境变量 WB_CM_DELETE_DIR");
        let root = PathBuf::from(raw);
        assert!(root.is_dir(), "{} 不是目录", root.display());

        let t0 = std::time::Instant::now();
        match std::fs::remove_dir_all(&root) {
            Ok(()) => println!("老写法成功了（用时 {:?}）", t0.elapsed()),
            Err(e) => println!(
                "老写法失败：{e}  raw_os_error={:?}（用时 {:?}），残留={}",
                e.raw_os_error(),
                t0.elapsed(),
                root.exists()
            ),
        }
    }

    /// `open_path` 只接受「存在且确实是目录」的绝对路径；这些输入必须被挡下来，
    /// 顺带确保测试本身不会真的去拉起文件管理器。
    #[test]
    fn open_path_rejects_bad_input() {
        assert!(open_path("").is_err(), "空路径");
        assert!(open_path("   ").is_err(), "全空格");
        assert!(open_path("relative\\path").is_err(), "相对路径");
        if cfg!(target_os = "windows") {
            assert!(
                open_path(r"C:\__no_such_dir_9f8a7b6c__").is_err(),
                "不存在的目录"
            );
        }
    }

    #[test]
    fn human_units() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(512), "512 B");
        assert_eq!(human(2048), "2.0 KB");
        assert_eq!(human(17 * 1024 * 1024 * 1024), "17.0 GB");
    }

    #[test]
    fn entries_cover_both_editions() {
        let names: Vec<String> = entries().into_iter().map(|(n, _, _)| n).collect();
        assert!(names.contains(&".workbuddy".to_string()));
        assert!(names.contains(&".workbuddy-ai".to_string()));
        assert!(names.contains(&".workbuddy-key-fallback".to_string()));
    }

    #[test]
    fn dest_rejects_nesting_and_install_dir() {
        let home = home_dir();
        let nested = home.join(".workbuddy").join("inbox");
        assert!(normalize_dest(&nested.to_string_lossy()).is_err());
        assert!(normalize_dest(r"F:\AdobeAll\WorkBuddy\x").is_err());
        assert!(normalize_dest("relative/path").is_err());
        assert!(normalize_dest("").is_err());
    }

    #[test]
    fn dest_accepts_plain_absolute_path() {
        let ok = if cfg!(target_os = "windows") {
            r"E:\WorkBuddyData"
        } else {
            "/tmp/WorkBuddyData"
        };
        assert!(normalize_dest(ok).is_ok());
    }

    #[test]
    fn backup_path_has_marker() {
        let src = home_dir().join(".workbuddy");
        let b = backup_path_for(&src);
        let name = b.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with(".workbuddy"));
        assert!(name.contains(BACKUP_MARK));
        assert_eq!(b.parent(), src.parent());
    }

    #[test]
    fn safe_backup_rejects_foreign_paths() {
        let home = home_dir();
        assert!(safe_backup_path(&home.join("Documents")).is_err());
        assert!(safe_backup_path(&home.join(".workbuddy")).is_err());
        assert!(safe_backup_path(&home.join("sub").join(".x.moved-1")).is_err());
        // 父目录对、有标记，但目录不存在 → 拒绝
        assert!(safe_backup_path(&home.join(".workbuddy.moved-19700101-000000")).is_err());
    }

    #[test]
    fn drive_of_extracts_letter() {
        if cfg!(target_os = "windows") {
            assert_eq!(drive_of(Path::new(r"E:\WorkBuddyData")), "E:");
        }
    }

    /// 在真实家目录上跑一次只读体检（Windows 上会启动一次 PowerShell 列盘符）。
    /// 需要几十万个文件的目录扫描，所以默认 `#[ignore]`，手动跑：
    /// `cargo test -p wb-switch-core --lib cache_move -- --ignored --nocapture`
    #[test]
    #[ignore = "扫描真实家目录，手动执行"]
    fn plan_smoke_on_real_home() {
        let p = plan(None);
        println!("home        = {}", p.home);
        println!("total       = {} / {} 个文件", p.total_text, p.total_files);
        println!("dest_default= {}", p.dest_default);
        println!("can_run     = {} ({:?})", p.can_run, p.blocked_reason);
        for d in &p.dirs {
            println!(
                "  {:<20} exists={:<5} link={:<5} {:>10} {}",
                d.label, d.exists, d.is_link, d.size_text, d.link_target.clone().unwrap_or_default()
            );
        }
        for dr in &p.drives {
            println!("  drive {:<4} 可用 {:<10} 共 {}", dr.letter, dr.free_text, dr.total_text);
        }
        assert!(!p.dirs.is_empty(), "至少要列出三个待迁移目录");
        assert!(
            p.dirs.iter().any(|d| d.name == ".workbuddy"),
            "必须包含 ~/.workbuddy"
        );
    }
}
