//! 按版本控制 WorkBuddy / WorkBuddyAI 桌面客户端进程。
//!
//! **为什么不改 `process.rs`**：那边整套逻辑（窗口枚举、内嵌 PowerShell 脚本、
//! exe 路径缓存、macOS bundle id / osascript）都深度绑定「国内版 WorkBuddy」这一个名字，
//! `"WorkBuddy.exe"` 散落在十几处。全量参数化收益不大、回归风险高。
//!
//! 这里只实现国际版真正需要的四件事：找 exe / 查是否在跑 / 关 / 开。
//! 自包含，不改动任何现有函数。

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::modules::edition::Edition;

/// 各版本的客户端可执行文件名（精确匹配用，不能用子串）。
pub fn exe_name(edition: Edition) -> &'static str {
    match edition {
        Edition::Domestic => "WorkBuddy.exe",
        Edition::International => "WorkBuddyAI.exe",
    }
}

/// 无控制台窗口标志。本 App 是 GUI 程序，调 tasklist / taskkill 时别闪黑框。
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(target_os = "windows")]
fn hidden(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

/// 在候选安装目录里找客户端 exe。
///
/// 实测本机两个客户端都在 `F:\AdobeAll\<WorkBuddy|WorkBuddyAI>\`，
/// 也覆盖 `%LOCALAPPDATA%\Programs\` 这种常见装法。
pub fn find_client_exe(edition: Edition) -> Option<PathBuf> {
    let name = exe_name(edition);
    let mut dirs: Vec<PathBuf> = Vec::new();

    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let base = PathBuf::from(&local);
        dirs.push(base.join("Programs").join(edition.process_name()));
        dirs.push(base.join(edition.process_name()));
    }
    // 本机实际情况：装在 F:\AdobeAll 下；其它盘符也扫一遍。
    for letter in ["C", "D", "E", "F", "G"] {
        let root = PathBuf::from(format!("{letter}:\\"));
        if !root.exists() {
            continue;
        }
        dirs.push(root.join("AdobeAll").join(edition.process_name()));
        dirs.push(root.join(edition.process_name()));
    }

    for dir in dirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 客户端是否在运行。
///
/// 用 `tasklist` 的镜像名**精确**比对：`WorkBuddyAI.exe` 的前缀含 `WorkBuddy`，
/// 用子串匹配会把国际版误判成国内版。
pub fn is_running(edition: Edition) -> bool {
    #[cfg(target_os = "windows")]
    {
        let name = exe_name(edition);
        let mut cmd = Command::new("tasklist");
        cmd.args(["/FI", &format!("IMAGENAME eq {name}"), "/FO", "CSV", "/NH"]);
        let out = match hidden(&mut cmd).output() {
            Ok(o) => o,
            // 查不到就认为没在跑，避免因为查询失败而把用户挡在门外。
            Err(_) => return false,
        };
        let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
        // CSV 每行形如 `"WorkBuddyAI.exe","1234",...`；精确比对第一列。
        text.lines().any(|line| {
            line.split("\",\"")
                .next()
                .map(|first| first.trim_start_matches('"').eq_ignore_ascii_case(name))
                .unwrap_or(false)
        })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = edition;
        false
    }
}

/// 关闭客户端：先不带 `/F` 让窗口收到关闭信号，超时再强杀。
///
/// 与 `process.rs` 的国内版流程相比这是简化版（不做窗口消息枚举），
/// 对国际版够用；失败时会返回明确错误让用户手动关闭。
pub fn close(edition: Edition, timeout_secs: i64) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let name = exe_name(edition);
        if !is_running(edition) {
            return Ok(());
        }

        // 1) 先试温和关闭
        let mut graceful = Command::new("taskkill");
        graceful.args(["/IM", name, "/T"]);
        let _ = hidden(&mut graceful).output();

        if wait_gone(edition, timeout_secs as f64) {
            return Ok(());
        }

        // 2) 超时则强杀
        eprintln!("[client-ctl] {name} 温和关闭超时，强制结束");
        let mut force = Command::new("taskkill");
        force.args(["/IM", name, "/T", "/F"]);
        let _ = hidden(&mut force).output();

        if wait_gone(edition, timeout_secs as f64) {
            Ok(())
        } else {
            Err(format!(
                "无法关闭 {}，请手动完全退出后重试",
                edition.process_name()
            ))
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (edition, timeout_secs);
        Ok(())
    }
}

fn wait_gone(edition: Edition, timeout_secs: f64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout_secs);
    while std::time::Instant::now() < deadline {
        if !is_running(edition) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    !is_running(edition)
}

/// 启动客户端（后台脱离，不阻塞）。
pub fn launch(edition: Edition) -> Result<(), String> {
    let exe = find_client_exe(edition).ok_or_else(|| {
        format!(
            "未找到 {} 程序。请先手动打开一次，或确认已安装。",
            edition.process_name()
        )
    })?;

    #[cfg(target_os = "windows")]
    {
        hidden(&mut Command::new(&exe))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("启动 {} 失败: {e}", exe.display()))?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        Command::new(&exe)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("启动 {} 失败: {e}", exe.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_names_are_distinct_and_exact() {
        assert_eq!(exe_name(Edition::Domestic), "WorkBuddy.exe");
        assert_eq!(exe_name(Edition::International), "WorkBuddyAI.exe");
        // 关键：国际版镜像名不能被子串匹配误判成国内版
        assert!(!exe_name(Edition::International).contains(exe_name(Edition::Domestic)));
    }

    #[test]
    fn find_client_exe_returns_exe_or_none() {
        // 本机两个客户端都装了，但换机器不该失败，所以只断言"找到就是 exe"。
        for e in Edition::ALL {
            if let Some(p) = find_client_exe(e) {
                assert!(p.is_file(), "{p:?} 不存在");
                assert_eq!(
                    p.file_name().unwrap().to_string_lossy().to_ascii_lowercase(),
                    exe_name(e).to_ascii_lowercase()
                );
            }
        }
    }

    #[test]
    fn is_running_does_not_panic() {
        for e in Edition::ALL {
            let _ = is_running(e);
        }
    }
}
