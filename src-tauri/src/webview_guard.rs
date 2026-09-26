//! WebView2 白屏自愈。
//!
//! # 现象
//!
//! 主窗口只有原生标题栏，内容区空白 —— **前端压根没跑起来**，
//! 所以 `PageErrorBoundary` 也没机会显示（它得先有 JS 在跑）。
//!
//! # 根因（2026-09-26 实测确认）
//!
//! WebView2 的**用户数据目录坏了**：`%LOCALAPPDATA%\<identifier>\EBWebView`。
//! 决定性对照实验：
//!
//! | 条件 | 前端是否就绪 |
//! |---|---|
//! | 原样启动（沿用旧 profile，114 MB） | ✗ 22 秒都没就绪 |
//! | 把 `EBWebView` 改名挪走，重新启动 | ✓ 4 秒内就绪 |
//!
//! 进程被强杀（任务管理器 / `taskkill /F` / 崩溃）时 profile 可能被写坏，
//! 此后**每次启动都白屏**，重启机器也救不回来 —— 用户只能干瞪眼。
//! （清 `msedgewebview2.exe` 残留进程没用：坏的是磁盘上的目录，不是进程。）
//!
//! # 对策
//!
//! 不猜"什么时候会坏"，而是**用结果反推**：
//!
//! 1. 前端挂载成功后调 `ui_ready`（`commands::ui_ready`）→ [`mark_ready`]。
//! 2. 看门狗 12s 等不到握手 ⇒ 立刻写下标记（[`MARKER`]）并 `reload()` 一次；
//!    再 15s 等不到就重启进程。
//! 3. 下次启动时 [`preflight`] 看到标记 ⇒ 清掉残留 WebView2 子进程、把坏 profile
//!    挪走，让 WebView2 重新建一个。
//!
//! 于是"永久白屏"降级为"最多白一次，下次启动自动恢复"；
//! 另外托盘『重载界面』给一个手动兜底（白屏时托盘菜单仍然可用）。
//!
//! # 2026-09-26 修的两个致命缺陷
//!
//! 原实现有两个洞，实测会让"永久白屏"真的变成永久：
//!
//! 1. **`preflight` 挪 profile 失败也照样清标记** ⇒ 一旦 profile 被残留的
//!    `msedgewebview2.exe` 占着（rename 失败），标记就被吃掉，下次启动不再重试。
//!    现在：只有**确认挪走**（或本来就不存在）才清标记，否则保留给下次；
//!    并且挪之前先杀掉属于本应用的残留 WebView2 子进程。
//! 2. **标记要等 27 秒（12+15）才写** ⇒ 用户在这之前手动关掉进程，标记就丢了。
//!    现在：第一次超时（12s）就写标记，语义改为「这次启动界面上不来」；
//!    reload 后若恢复，`mark_ready()` 会把标记清掉。
//!
//! ⚠️ 防重启风暴：重启进程仍受 [`COOLDOWN`] 限制，但**重置 profile 不受限**
//! （重置是幂等的、几乎总是有效，限制它只会让白屏卡死）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager, Runtime};

/// 前端挂载成功后置位。
static UI_READY: AtomicBool = AtomicBool::new(false);

/// 应用标识（= `%LOCALAPPDATA%` 下的目录名 / WebView2 profile 的父目录）。
/// 由 [`preflight`] 在最早时机写入，之后的 [`mark_ready`] 直接复用。
static IDENTIFIER: OnceLock<String> = OnceLock::new();

/// 上次启动界面没起来 —— 存在就说明该重置 profile 了。
const MARKER: &str = ".webview-broken";
/// 上次重置 profile 的时刻（epoch 秒），用于防重启风暴。
const COOLDOWN: &str = ".webview-reset-at";
/// 冷却期内不再重置/重启，避免"重置也修不好"时无限重启。
const COOLDOWN_SECS: u64 = 600;

/// 等前端握手的上限。冷启动 + 首屏拉数据 2~5 秒，12 秒足够宽松。
const FIRST_WAIT: Duration = Duration::from_secs(12);
/// reload 之后再等多久。
const SECOND_WAIT: Duration = Duration::from_secs(15);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 状态文件所在目录（与 WebView2 profile 同级，都在 `%LOCALAPPDATA%\<identifier>`）。
fn state_dir(identifier: &str) -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join(identifier))
}

/// 默认 profile 目录。
fn profile_dir(identifier: &str) -> Option<PathBuf> {
    state_dir(identifier).map(|dir| dir.join("EBWebView"))
}

/// **必须在 `tauri::Builder::build()` 之前调用。**
///
/// 上一次启动如果没能让前端就绪，就把坏掉的 WebView2 用户数据目录挪到一边，
/// WebView2 下次会自己建一个干净的。
///
/// 只挪不改名内容，且用带时间戳的新名字，出问题还能翻回去看。
pub fn preflight(identifier: &str) {
    let _ = IDENTIFIER.set(identifier.to_string());
    let Some(dir) = state_dir(identifier) else {
        return;
    };

    // ① 每次都做：清掉上一轮被强杀留下的 WebView2 孤儿。
    //    它们占着 profile 目录，新实例的 WebView2 初始化不了 ⇒ 空窗口。
    //    这一步独立于 MARKER —— 大多数"白屏"到这一步就好了，不需要重置 profile。
    let orphans = kill_orphan_webviews(identifier);
    if orphans > 0 {
        eprintln!("[webview-guard] 清掉 {orphans} 个残留 WebView2 子进程");
        std::thread::sleep(std::time::Duration::from_millis(600));
    }

    // ② 只有上次启动确实没起来，才动 profile。
    let marker = dir.join(MARKER);
    if !marker.is_file() {
        return;
    }

    eprintln!("[webview-guard] 上次启动界面没起来，重置 WebView2 用户数据目录");

    let mut moved = false;
    match profile_dir(identifier) {
        Some(profile) if profile.exists() => {
            let aside = profile.with_file_name(format!("EBWebView-broken-{}", now_secs()));
            match std::fs::rename(&profile, &aside) {
                Ok(()) => {
                    moved = true;
                    eprintln!("[webview-guard] 已挪走 profile -> {}", aside.display());
                }
                Err(error) => {
                    eprintln!("[webview-guard] 挪 profile 失败（仍被占用？）: {error}")
                }
            }
        }
        // profile 本来就不在 —— 上次已经挪走了，这次 WebView2 会新建一个干净的。
        _ => moved = true,
    }

    // 🔴 标记只在**确认 profile 已挪走**时才清。挪失败就留着，下次启动继续试。
    // （之前无条件清标记 ⇒ rename 一旦被占用失败就永远不再重试，白屏变成永久的。）
    if moved {
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::write(dir.join(COOLDOWN), now_secs().to_string());
    } else {
        eprintln!("[webview-guard] 保留失败标记，下次启动继续尝试重置");
    }
}

/// 杀掉**孤儿** `msedgewebview2.exe`（父进程已退出）—— 只动属于本应用的那些。
///
/// # 为什么需要（2026-09-26 实测修正）
///
/// 之前把白屏归因为"profile 损坏"，进一步实测发现 **profile 往往是好的**：
/// 把同一个 profile 在下次启动时放回去，3 秒就正常渲染。
/// 真正卡住的是**上一轮被强杀的 WebView2 子进程还占着 profile 目录**，
/// 新实例的 WebView2 初始化不了，于是只剩一个空窗口。
///
/// 所以要**每次启动都清一次孤儿**，而不是等看门狗判定失败。
/// 只杀父进程已退出的：app 正常运行时它的 WebView2 孤儿判定不成立，不会误伤
/// （也就不会出现"开第二个实例把第一个的 webview 打死"）。
#[cfg(target_os = "windows")]
fn kill_orphan_webviews(identifier: &str) -> usize {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    /// CREATE_NO_WINDOW —— 避免启动 powershell 时闪黑框。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\" | \
         Where-Object {{ \
            \"$($_.CommandLine)\" -like '*{identifier}*' -and \
            -not (Get-Process -Id $_.ParentProcessId -ErrorAction SilentlyContinue) \
         }} | \
         ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue; \
         Write-Output $_.ProcessId }}"
    );
    match Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter(|s| s.parse::<u32>().is_ok())
            .count(),
        Err(_) => 0,
    }
}

#[cfg(not(target_os = "windows"))]
fn kill_orphan_webviews(_identifier: &str) -> usize {
    0
}

/// 由 `commands::ui_ready` 调用；同时清掉失败标记。
pub fn mark_ready() {
    if !UI_READY.swap(true, Ordering::AcqRel) {
        eprintln!("[webview-guard] 前端已就绪");
    }
    // 这次起来了，把"上次失败"的痕迹擦掉（写标记的是上一次进程）。
    if let Some(identifier) = IDENTIFIER.get() {
        if let Some(dir) = state_dir(identifier) {
            let _ = std::fs::remove_file(dir.join(MARKER));
        }
    }
}

fn write_marker(identifier: &str) {
    if let Some(dir) = state_dir(identifier) {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(MARKER), now_secs().to_string());
    }
}

fn cooldown_active(identifier: &str) -> bool {
    let Some(dir) = state_dir(identifier) else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(dir.join(COOLDOWN)) else {
        return false;
    };
    let Ok(at) = text.trim().parse::<u64>() else {
        return false;
    };
    now_secs().saturating_sub(at) < COOLDOWN_SECS
}

/// 手动重载主界面（托盘菜单用）。
pub fn reload_main<R: Runtime>(app: &AppHandle<R>) {
    match app.get_webview_window(crate::tray::MAIN_WINDOW_LABEL) {
        Some(window) => {
            eprintln!("[webview-guard] 手动重载主界面");
            let _ = window.show();
            let _ = window.reload();
        }
        None => eprintln!("[webview-guard] 主窗口不存在，无法重载"),
    }
}

// ---------------------------------------------------------------------------
// 单实例保护（2026-09-26 实测：两个实例会争抢同一个 WebView2 profile，双双黑屏）
// ---------------------------------------------------------------------------

/// 第二个实例留给第一个实例的「请把窗口显示出来」请求文件。
const SHOW_REQUEST: &str = ".show-request";

/// 是否已有**另一个**本应用实例在运行。
///
/// # 为什么要挡
///
/// 两个实例共用 `%LOCALAPPDATA%\<identifier>\EBWebView`。实测（2026-09-26）：
/// 双实例并存时**两个窗口都只有原生标题栏、内容区全黑**，日志里还伴随
/// `监听 127.0.0.1:7863 失败: os error 10048`（后启动的那个抢不到反代端口）。
/// 用户对这种情况的描述就是"点开是白屏"。
///
/// 用 `tasklist` 查同名进程并按 PID 排除自己 —— 不引入新依赖，
/// 也不依赖文件锁（文件锁在进程被强杀时会留下假锁）。
#[cfg(target_os = "windows")]
pub fn another_instance_running() -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let Ok(output) = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq wb-switch-rust.exe", "/NH", "/FO", "CSV"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let me = std::process::id().to_string();
    text.lines()
        .filter(|line| line.contains("wb-switch-rust.exe"))
        .filter(|line| {
            // CSV: "wb-switch-rust.exe","12345","Console",...
            let parts: Vec<&str> = line.split("\",\"").collect();
            !(parts.len() > 1 && parts[1].trim_matches('"') == me)
        })
        .count()
        > 0
}

#[cfg(not(target_os = "windows"))]
pub fn another_instance_running() -> bool {
    false
}

/// 请求已经在跑的那个实例把主窗口显示出来（我们随后就退出）。
pub fn request_show(identifier: &str) {
    if let Some(dir) = state_dir(identifier) {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(SHOW_REQUEST), now_secs().to_string());
    }
}

/// 消费一次「显示窗口」请求；有请求返回 true（并删掉文件）。
fn take_show_request(identifier: &str) -> bool {
    let Some(dir) = state_dir(identifier) else {
        return false;
    };
    let file = dir.join(SHOW_REQUEST);
    if file.is_file() {
        let _ = std::fs::remove_file(&file);
        true
    } else {
        false
    }
}

/// 启动看门狗：等不到前端握手就自愈。
pub fn spawn_watchdog<R: Runtime>(app: AppHandle<R>) {
    let identifier = app.config().identifier.clone();
    tauri::async_runtime::spawn(async move {
        if !wait_ready(FIRST_WAIT).await {
            eprintln!(
                "[webview-guard] 主界面 {}s 内没有就绪信号，先尝试重载",
                FIRST_WAIT.as_secs()
            );
            // 🔴 第一次超时就落标记：标记的语义是「这次启动界面上不来」，
            // 而不是「重载也没救」。这样即使用户在这之后手动关掉进程，
            // 下次启动 preflight 仍能看到标记并重置 profile。
            // 若 reload 后恢复正常，mark_ready() 会把标记清掉。
            write_marker(&identifier);
            reload_main(&app);

            if wait_ready(SECOND_WAIT).await {
                eprintln!("[webview-guard] 重载后已恢复");
            } else {
                eprintln!("[webview-guard] 重载无效 ⇒ 判定 WebView2 用户数据目录已损坏");
                if cooldown_active(&identifier) {
                    eprintln!(
                        "[webview-guard] 最近已重置过 profile 仍未恢复，不再重启（避免重启风暴）；\
                         可托盘『重载界面』或手动重开"
                    );
                } else {
                    eprintln!("[webview-guard] 重启进程，下次启动会自动重建 profile");
                    // `restart()` 返回 `!`，这里之后不会再往下走。
                    app.restart();
                }
            }
        }

        // 就绪后常驻：第二个实例会写 `.show-request`，请我们把窗口显示出来。
        // 这样用户重复点图标不再产生第二个黑窗口，而是把已有窗口唤到前面。
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if take_show_request(&identifier) {
                eprintln!("[webview-guard] 收到显示窗口请求");
                crate::tray::show_main_window(&app);
            }
        }
    });
}

/// 轮询等待前端置位；已就绪立即返回 true。
async fn wait_ready(limit: Duration) -> bool {
    let step = Duration::from_millis(250);
    let mut waited = Duration::ZERO;
    while waited < limit {
        if UI_READY.load(Ordering::Acquire) {
            return true;
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
    UI_READY.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_secs_is_not_zero() {
        // 冷却必须存在，否则"重置也修不好"会变成无限重启。
        assert!(COOLDOWN_SECS >= 60);
    }

    #[test]
    fn first_wait_is_generous_enough_for_cold_start() {
        // 冷启动 + 首屏拉数据实测 2~5 秒，别把阈值调得太激进导致误判。
        assert!(FIRST_WAIT >= Duration::from_secs(8));
    }
}
