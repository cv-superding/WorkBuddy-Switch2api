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
//! 2. 看门狗等不到这个信号 ⇒ 判定这次启动失败了，先 `reload()` 一次；
//!    再等不到就写下标记文件并**重启进程**（[`MARKER`]）。
//! 3. 下次启动时 [`preflight`] 看到标记 ⇒ 把坏 profile 挪走，让 WebView2 重新建一个。
//!
//! 于是"永久白屏"降级为"最多白一次，下次启动自动恢复"；
//! 另外托盘『重载界面』给一个手动兜底（白屏时托盘菜单仍然可用）。
//!
//! ⚠️ 防重启风暴：标记只在"本次启动失败"时写，重置一次后会留下 [`COOLDOWN`]，
//! 短时间内不会因为同样的原因反复重启。

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
    let marker = dir.join(MARKER);
    if !marker.is_file() {
        return;
    }

    eprintln!("[webview-guard] 上次启动界面没起来，重置 WebView2 用户数据目录");
    if let Some(profile) = profile_dir(identifier) {
        if profile.exists() {
            let aside = profile.with_file_name(format!("EBWebView-broken-{}", now_secs()));
            match std::fs::rename(&profile, &aside) {
                Ok(()) => eprintln!("[webview-guard] 已挪走坏 profile -> {}", aside.display()),
                Err(error) => eprintln!(
                    "[webview-guard] 挪 profile 失败（可能仍被占用，下次启动会再试）: {error}"
                ),
            }
        }
    }

    // 清标记、留冷却，避免"重置也修不好"时无限重启。
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::write(dir.join(COOLDOWN), now_secs().to_string());
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

/// 启动看门狗：等不到前端握手就自愈。
pub fn spawn_watchdog<R: Runtime>(app: AppHandle<R>) {
    let identifier = app.config().identifier.clone();
    tauri::async_runtime::spawn(async move {
        if wait_ready(FIRST_WAIT).await {
            return;
        }
        eprintln!(
            "[webview-guard] 主界面 {}s 内没有就绪信号，先尝试重载",
            FIRST_WAIT.as_secs()
        );
        reload_main(&app);

        if wait_ready(SECOND_WAIT).await {
            eprintln!("[webview-guard] 重载后已恢复");
            return;
        }

        eprintln!("[webview-guard] 重载无效 ⇒ 判定 WebView2 用户数据目录已损坏");
        write_marker(&identifier);
        if cooldown_active(&identifier) {
            eprintln!(
                "[webview-guard] 最近已重置过 profile 仍未恢复，不再重启（避免重启风暴）；\
                 可托盘『重载界面』或手动重开"
            );
            return;
        }
        eprintln!("[webview-guard] 重启进程，下次启动会自动重建 profile");
        app.restart();
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
