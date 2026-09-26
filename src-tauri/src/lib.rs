// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
mod commands;
#[cfg(desktop)]
mod tray;
mod webview_guard;

use std::time::Duration;
use wb_switch_core::modules;

const SCREENSHOT_DEMO_ENV: &str = "WB_SWITCH_SCREENSHOT_DEMO";

pub(crate) fn is_screenshot_demo() -> bool {
    std::env::var(SCREENSHOT_DEMO_ENV).as_deref() == Ok("1")
}

/// 后台循环：自动签到启动即核验、每 30 分钟补签；自动轮换每 30 秒检查；每天一次保活。
fn spawn_background_loops() {
    // OpenAI 兼容反代：proxy.json 里 enabled=true 时才监听（默认关闭）。
    wb_switch_core::modules::proxy::spawn_from_config();

    tauri::async_runtime::spawn(async move {
        if let Err(error) = modules::config::compact_checkin_logs() {
            eprintln!("[签到] 历史日志整理失败: {error}");
        }
        let _ =
            modules::checkin::run_checkin_cycle(modules::checkin::CheckinCycleMode::StartupVerify)
                .await;
        loop {
            tokio::time::sleep(modules::checkin::CHECKIN_RECOVERY_INTERVAL).await;
            let _ = modules::checkin::run_checkin_cycle(
                modules::checkin::CheckinCycleMode::PeriodicRecovery,
            )
            .await;
        }
    });

    // 派猫猫旅行：启动即派发，之后周期性补派（并重试 no-buddy / 瞬时错误）。
    tauri::async_runtime::spawn(async move {
        let _ = modules::travel::run_travel_cycle().await;
        loop {
            tokio::time::sleep(modules::travel::TRAVEL_RETRY_INTERVAL).await;
            let _ = modules::travel::run_travel_cycle().await;
        }
    });

    // 旅行领取：启动立刻查一轮（避免重启后空等 15 分钟漏领），之后按周期检查。
    tauri::async_runtime::spawn(async move {
        let _ = modules::travel::run_travel_claim_cycle().await;
        loop {
            tokio::time::sleep(modules::travel::TRAVEL_CLAIM_INTERVAL).await;
            let _ = modules::travel::run_travel_claim_cycle().await;
        }
    });

    tauri::async_runtime::spawn(async move {
        let mut last_keepalive_day = String::new();
        let mut last_rotate_at: i64 = 0;
        loop {
            // 自动轮换（CodeBuddy CLI）：按配置间隔执行
            let rotate_cfg = modules::config::load_auto_rotate_config();
            if rotate_cfg.get("enabled").and_then(|v| v.as_bool()) == Some(true) {
                let interval_minutes = rotate_cfg
                    .get("check_interval_minutes")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(5)
                    .max(1);
                let now = modules::config::now_ms();
                if now - last_rotate_at >= interval_minutes * 60_000 {
                    last_rotate_at = now;
                    let _ = modules::rotate::run_rotate_cycle().await;
                }
            }
            let today = modules::checkin::date_str(None);
            if today != last_keepalive_day {
                last_keepalive_day = today;
                let _ = modules::refresh::run_keepalive_cycle().await;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = tauri::generate_context!();
    let identifier = context.config().identifier.clone();

    // 已有实例在跑 ⇒ 不启动第二个 UI。
    // 两个实例共用同一个 WebView2 profile（`%LOCALAPPDATA%\<identifier>\EBWebView`），
    // 实测（2026-09-26）会**双双黑屏**（两边都只剩原生标题栏），后启动的那个还会
    // 抢不到反代端口（os error 10048）。用户看到的就是"点开是白屏"。
    // 所以：请已有实例把窗口显示出来，然后自己退出。
    if webview_guard::another_instance_running() {
        webview_guard::request_show(&identifier);
        eprintln!("[instance] 已有实例在运行，已请求它显示主窗口，本次退出");
        return;
    }

    // 必须在 build() 之前：上次启动若界面没起来，这里会把坏掉的 WebView2
    // 用户数据目录挪走，让 WebView2 重新建一个干净的（白屏自愈的关键一步）。
    webview_guard::preflight(&identifier);

    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init());

    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![tray::SILENT_STARTUP_ARG]),
        ));
        builder = builder.on_window_event(tray::on_window_event);
    }

    let app = builder
        .setup(|app| {
            #[cfg(desktop)]
            {
                tray::setup(app)?;
                // 主窗口由配置创建为不可见；在事件循环呈现前决定本次启动是否静默。
                // 仅系统自启（精确 `--hidden` 参数）进入静默托盘，普通启动立即显示主窗口。
                tray::setup_startup_visibility(
                    app.handle(),
                    tray::is_silent_startup(std::env::args()),
                );
            }
            // README 截图模式只渲染前端虚构数据，禁止读取账号后执行签到、轮换或保活。
            if !is_screenshot_demo() {
                spawn_background_loops();
            }
            // 白屏自愈：前端挂载后会调 ui_ready，等不到就 reload → 再不行就用干净 profile 重建。
            webview_guard::spawn_watchdog(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::ui_ready,
            commands::get_status,
            commands::get_accounts,
            commands::get_codebuddy_cli_status,
            commands::install_codebuddy_cli_helper,
            commands::switch_codebuddy_cli_account,
            commands::get_codebuddy_cn_ide_status,
            commands::switch_codebuddy_cn_ide_account,
            commands::detect_codebuddy_cn_ide_account,
            commands::delete_account,
            commands::oauth_start,
            commands::oauth_status,
            commands::import_local,
            commands::get_editions,
            commands::export_accounts,
            commands::export_accounts_to_path,
            commands::preview_import_accounts,
            commands::import_accounts,
            commands::switch_account,
            commands::list_sessions,
            commands::copy_sessions,
            commands::open_permission_settings,
            commands::check_auth_permission,
            commands::reveal_app_in_finder,
            commands::get_checkin_status,
            commands::get_credit_expiry,
            commands::get_credit_statistics,
            commands::get_token_statistics,
            commands::checkin,
            commands::checkin_all,
            commands::get_auto_checkin_config,
            commands::save_auto_checkin_config,
            commands::get_checkin_logs,
            commands::get_travel_status,
            commands::get_auto_travel_config,
            commands::save_auto_travel_config,
            commands::refresh_account_token,
            commands::get_auto_rotate_config,
            commands::save_auto_rotate_config,
            commands::rotate_status,
            commands::run_rotate,
            commands::get_rotate_logs,
            commands::get_github_config,
            commands::save_github_config,
            commands::check_update,
            commands::relaunch_app,
            commands::get_launch_at_login_enabled,
            commands::set_launch_at_login_enabled,
            commands::get_proxy_config,
            commands::save_proxy_config,
            commands::get_proxy_status,
            commands::set_account_group,
            commands::get_proxy_usage,
            commands::reset_proxy_usage,
        ])
        .build(context)
        .expect("error while building tauri application");

    app.run(|_app_handle, event| {
        #[cfg(desktop)]
        tray::on_run_event(event);
    });
}
