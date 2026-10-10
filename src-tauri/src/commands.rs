//! Tauri commands：前端调用的薄包装，对应 Python 版 HTTP API。
//!
//! 阶段 1 覆盖：get_status / get_accounts / delete_account / oauth_start /
//! oauth_status / import_local。

use std::path::PathBuf;

use serde::Serialize;
use serde_json::{json, Value};

use tauri::Emitter;
use wb_switch_core::modules::{
    account, auth_file, cache_move, checkin, client_ctl, codebuddy_cli, codebuddy_cn_ide,
    credit_usage, credits,
    edition::{edition_of, parse_lenient, Edition}, export_import, oauth,
    process, proxy, refresh, rotate, session, switch, token_stats, transfer, travel, update,
    update_guard,
};

#[derive(Serialize)]
pub struct AppStatus {
    running: bool,
    /// 显式 camelCase：前端读的是 `authFile` / `appPath`。
    /// 之前只靠默认的蛇形命名，桌面端这两个字段一直是 undefined（webui 通道却是驼峰）。
    #[serde(rename = "authFile")]
    auth_file: String,
    /// 国内版那份鉴权文件里的当前账号。
    current: Option<Value>,
    /// 国际版那份鉴权文件里的当前账号。界面要按账号所属版本取，
    /// 只看 `current` 的话国际版卡片永远显示「设为当前」。
    #[serde(rename = "currentInternational")]
    current_international: Option<Value>,
    #[serde(rename = "appPath")]
    app_path: String,
    version: String,
}

/// 前端挂载成功后调用一次，告诉 Rust 侧「界面真的起来了」。
///
/// 这是白屏自愈的判据：`webview_guard` 的看门狗等不到这个信号，
/// 就认为 WebView2 没把页面跑起来（状态脏），先 reload、再用干净 profile 重建。
/// 纯置位操作，不做任何 IO，允许重复调用。
#[tauri::command]
pub fn ui_ready() {
    crate::webview_guard::mark_ready();
}

/// GET /api/status —— WorkBuddy 运行状态 + 当前账号。
#[tauri::command]
pub async fn get_status() -> Result<AppStatus, String> {
    // Windows 的运行状态检测会启动 tasklist 子进程。同步 command 默认在
    // Tauri 主线程执行，标题栏拖拽期间一旦焦点事件触发状态刷新，就会阻塞
    // 原生窗口消息循环。放入 blocking 线程，保持窗口移动与 IPC 查询解耦。
    tauri::async_runtime::spawn_blocking(build_app_status)
        .await
        .map_err(|error| format!("查询应用状态失败: {error}"))
}

fn build_app_status() -> AppStatus {
    // 两个版本各读各的鉴权文件：界面上国际版账号也要能显示「已设为当前」。
    // 展示字段的口径（加密信封收敛成字符串/null）只在 core 里定义一份。
    AppStatus {
        running: process::is_workbuddy_running(),
        auth_file: auth_file::auth_file_path().to_string_lossy().to_string(),
        current: auth_file::current_account_summary_for(Edition::Domestic),
        current_international: auth_file::current_account_summary_for(Edition::International),
        app_path: auth_file::workbuddy_app_path()
            .to_string_lossy()
            .to_string(),
        version: update::APP_VERSION.to_string(),
    }
}

/// GET /api/accounts —— 账号列表（account_meta，不含 token）。
#[tauri::command]
pub fn get_accounts() -> Value {
    let metas: Vec<Value> = account::load_accounts()
        .iter()
        .map(account::account_meta)
        .collect();
    json!({ "accounts": metas })
}

/// GET /api/codebuddy-cli/status —— CodeBuddy CLI helper 轮换状态（不含 token）。
///
/// async + spawn_blocking：状态检测可能执行 ps / helper 定位等子进程，
/// 避免在账号页挂载刷新时阻塞主线程造成页面卡顿。
#[tauri::command]
pub async fn get_codebuddy_cli_status() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(codebuddy_cli::status)
        .await
        .map_err(|error| format!("查询 CodeBuddy CLI 状态失败: {error}"))
}

/// POST /api/codebuddy-cli/install-helper —— 显式安装/升级 CLI helper。
#[tauri::command]
pub async fn install_codebuddy_cli_helper() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(codebuddy_cli::install_helper)
        .await
        .map_err(|e| e.to_string())?
}

/// POST /api/codebuddy-cli/switch —— 只切换 CodeBuddy CLI，不重启 WorkBuddy。
///
/// async + spawn_blocking：切换会用登录 shell 定位 node 并执行 apiKeyHelper
/// 校验账号（子进程无超时），同步 command 会阻塞主线程造成 UI 卡顿。
#[tauri::command(rename_all = "camelCase")]
pub async fn switch_codebuddy_cli_account(account_id: String) -> Result<Value, String> {
    if account_id.trim().is_empty() {
        return Err("缺少 accountId".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || codebuddy_cli::set_active_account(&account_id))
        .await
        .map_err(|e| e.to_string())?
}

/// GET /api/codebuddy-cn-ide/status —— CodeBuddy IDE 安装/运行/当前账号。
///
/// async + spawn_blocking：状态检测会跑 ps / mdfind 等子进程（mdfind 可能
/// 耗时数秒），账号页每次挂载都会刷新，若在主线程执行会造成页面卡顿。
#[tauri::command]
pub async fn get_codebuddy_cn_ide_status() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(codebuddy_cn_ide::status)
        .await
        .map_err(|error| format!("查询 CodeBuddy IDE 状态失败: {error}"))
}

/// POST /api/codebuddy-cn-ide/switch —— 注入凭证并可选重启 CodeBuddy CN IDE。
///
/// async + spawn_blocking：切换会关闭并重启 CodeBuddy CN，可能阻塞数十秒，
/// 与 WorkBuddy 切换同理，若在同步 command（主线程）执行会卡死整个 UI。
#[tauri::command(rename_all = "camelCase")]
pub async fn switch_codebuddy_cn_ide_account(
    account_id: String,
    restart: Option<bool>,
) -> Result<Value, String> {
    if account_id.trim().is_empty() {
        return Err("缺少 accountId".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || {
        codebuddy_cn_ide::switch_account(&account_id, restart.unwrap_or(true))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// POST /api/codebuddy-cn-ide/detect —— 读取本机 CN IDE 当前登录并尝试匹配账号库。
///
/// async + spawn_blocking：会通过 Keychain/secret 读取子进程，避免阻塞主线程。
#[tauri::command]
pub async fn detect_codebuddy_cn_ide_account() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(codebuddy_cn_ide::detect_current_account)
        .await
        .map_err(|e| e.to_string())?
}


/// DELETE /api/delete —— 删除账号。
#[tauri::command]
pub fn delete_account(account_id: String) -> Result<Value, String> {
    let mut accounts = account::load_accounts();
    let before = accounts.len();
    accounts.retain(|a| a.get("id").and_then(|v| v.as_str()) != Some(account_id.as_str()));
    if accounts.len() == before {
        return Err("账号不存在".to_string());
    }
    account::save_accounts(&accounts).map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

/// POST /api/oauth/start —— 发起 OAuth 扫码登录。
///
/// `edition` 缺省 = 国内版。**国际版必须显式传 `international`**：
/// 两档位走不同域名与 `platform` 参数（`workbuddy` / `workbuddy-ai`）。
#[tauri::command]
pub async fn oauth_start(edition: Option<String>) -> Result<Value, String> {
    let edition = edition.as_deref().map(parse_lenient).unwrap_or_default();
    oauth::oauth_start_for(edition).await
}

/// GET /api/oauth/status —— 轮询采集结果。
#[tauri::command]
pub async fn oauth_status(login_id: String) -> Value {
    oauth::oauth_poll(&login_id).await
}

/// POST /api/import-local —— 导入本机当前账号。
///
/// `edition` 缺省 = 国内版；传 `international` 则从 `workbuddy-desktop-ai.info` 导入。
#[tauri::command]
pub fn import_local(edition: Option<String>) -> Result<Value, String> {
    let edition = edition.as_deref().map(parse_lenient).unwrap_or_default();
    account::import_local_for(edition)
        .map(|acc| json!({ "ok": true, "account": acc, "edition": edition.key() }))
}

/// GET /api/editions —— 两个版本（国内版 / 国际版）的客户端状态。
///
/// 供前端版本切换器展示：是否安装、是否正在运行、认证文件是否存在、当前登录 uid。
#[tauri::command]
pub fn get_editions() -> Value {
    let editions: Vec<Value> = wb_switch_core::modules::edition::Edition::ALL
        .iter()
        .map(|e| {
            let auth = auth_file::read_auth_file_for(*e);
            let current_uid = auth
                .as_ref()
                .and_then(|a| a.get("account"))
                .and_then(|a| a.get("uid"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            json!({
                "key": e.key(),
                "label": e.label(),
                "processName": e.process_name(),
                "authFile": e.auth_file_path().to_string_lossy(),
                "authFileExists": auth.is_some(),
                "installed": client_ctl::find_client_exe(*e).is_some(),
                "running": client_ctl::is_running(*e),
                "currentUid": current_uid,
                "snapshotExists": e.account_snapshot_path().exists(),
            })
        })
        .collect();
    json!({ "editions": editions })
}

// ---------------------------------------------------------------------------
// 导出 / 导入账号
// ---------------------------------------------------------------------------

/// POST /api/export-accounts —— 按账号 id 列表导出完整记录（含 token）。
#[tauri::command]
pub fn export_accounts(account_ids: Vec<String>) -> Result<Value, String> {
    export_import::export_accounts(&account_ids)
        .map(|records| json!({ "ok": true, "accounts": records }))
}

/// POST /api/export-accounts-to-path —— 把勾选账号的完整记录写入用户选择的路径（保存对话框产物）。
#[tauri::command]
pub fn export_accounts_to_path(account_ids: Vec<String>, path: String) -> Result<Value, String> {
    export_import::export_accounts_to_path(&account_ids, &path)
        .map(|path| json!({ "ok": true, "path": path }))
}

/// POST /api/import/preview —— 解析导入文件并返回脱敏预览（含文件内索引）。
#[tauri::command]
pub fn preview_import_accounts(file_text: String) -> Result<Value, String> {
    export_import::preview_accounts(&file_text)
}

/// POST /api/import —— 按选中索引把账号导入账号库，返回导入/跳过/覆盖计数。
#[tauri::command]
pub fn import_accounts(file_text: String, indexes: Vec<usize>) -> Result<Value, String> {
    let result = export_import::import_accounts(&file_text, &indexes)?;
    Ok(json!({
        "ok": true,
        "imported": result.imported,
        "skipped": result.skipped,
        "overwritten": result.overwritten,
        "encrypted": result.encrypted,
    }))
}

/// 打开系统设置授权面板。默认「完全磁盘访问」（该 anchor 各版本均有效）；
/// 传 `target="app_management"` 尝试「App 管理」（macOS 15+，部分版本不支持深链）。
///
/// 使用 macOS 13+ 深链接格式（`com.apple.settings.PrivacySecurity.extension?Privacy_*`）。
#[tauri::command]
pub fn open_permission_settings(target: Option<String>) -> Result<(), String> {
    let t = target.unwrap_or_else(|| "all_files".to_string());
    let url = match t.as_str() {
        "app_management" => {
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AppManagement"
        }
        _ => {
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles"
        }
    };
    let _ = std::process::Command::new("open").arg(url).spawn();
    Ok(())
}

/// 权限自检：尝试在认证文件目录写/删探针文件，确认完全磁盘访问等授权是否生效。
#[tauri::command]
pub fn check_auth_permission() -> Value {
    let path = auth_file::auth_file_path();
    let probe = path.with_file_name("workbuddy-desktop.info.probe");
    match std::fs::write(&probe, "probe") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            json!({ "ok": true, "message": "认证目录可写，权限正常" })
        }
        Err(e) => json!({
            "ok": false,
            "error": e.to_string(),
            "dir": path.parent().map(|p| p.to_string_lossy().to_string()),
            "hint": "请在 系统设置→隐私与安全性 中授权：优先「App 管理」开启 wb-switch，若没有则去「完全磁盘访问」把 wb-switch 拖进去；授权后需重启 App 生效",
        }),
    }
}

/// 在 Finder 中显示当前 App（便于拖拽到「完全磁盘访问」授权框）。
#[tauri::command]
pub fn reveal_app_in_finder() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(&exe)
        .spawn();
    Ok(())
}

/// POST /api/switch —— 切换账号（备份 → 关进程 → 复制会话 → 写认证 → 重启）。
///
/// async + spawn_blocking：切换中关闭/启动 WorkBuddy 会阻塞数十秒，
/// 若在同步 command（主线程）执行会卡死整个 UI（loading 遮罩无法渲染）。
#[tauri::command(rename_all = "camelCase")]
pub async fn switch_account(
    app: tauri::AppHandle,
    account_id: String,
    restart: Option<bool>,
    share_sessions: Option<bool>,
    copy_session_ids: Option<Vec<String>>,
    edition: Option<String>,
) -> Result<Value, String> {
    if account_id.trim().is_empty() {
        return Err("缺少 accountId".to_string());
    }
    let restart = restart.unwrap_or(true);
    let share_sessions = share_sessions.unwrap_or(false);
    let copy_ids = copy_session_ids.unwrap_or_default();
    // 缺省 = 国内版，老前端不传该字段时行为不变。
    let edition = edition.as_deref().map(parse_lenient).unwrap_or_default();
    let progress: switch::ProgressFn = Box::new(move |message| {
        let _ = app.emit("switch-progress", json!({ "message": message }));
    });
    tauri::async_runtime::spawn_blocking(move || {
        switch::switch_account_in_edition(
            Some(&progress),
            &account_id,
            restart,
            share_sessions,
            &copy_ids,
            edition,
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

/// GET /api/sessions —— 指定档位当前账号的会话列表。
///
/// `edition` 缺省 = 国内版。国际版有自己的 `workbuddy.db`，必须显式传，
/// 否则会拿国内版的库和 uid 去查，列表永远是空的。
#[tauri::command]
pub fn list_sessions(edition: Option<String>) -> Value {
    let edition = edition.as_deref().map(parse_lenient).unwrap_or_default();
    match session::current_user_uid_for(edition) {
        Some(uid) => json!({
            "sessions": session::list_sessions_for_user_for(edition, &uid),
            "current": uid,
            "edition": edition.key(),
        }),
        None => json!({"sessions": [], "current": Value::Null, "edition": edition.key()}),
    }
}

/// POST /api/sessions/copy —— 把勾选会话复制到指定账号（路径 B）。
///
/// 档位取自**目标账号自身**的 `edition`，保证读写的是该档位的会话库。
#[tauri::command(rename_all = "camelCase")]
pub async fn copy_sessions(
    target_account_id: String,
    session_ids: Vec<String>,
) -> Result<Value, String> {
    if target_account_id.trim().is_empty() {
        return Err("缺少 targetAccountId".to_string());
    }
    if session_ids.is_empty() {
        return Err("缺少 sessionIds".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let target = account::find_account(&target_account_id).ok_or("目标账号不存在")?;
        let edition = edition_of(&target);
        Ok(session::copy_sessions_for_switch_for(edition, &target, &session_ids)
            .unwrap_or_else(|| json!({})))
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// 阶段 3：签到 + token 刷新
// ---------------------------------------------------------------------------

/// GET /api/checkin/status —— 查询单账号签到状态。
#[tauri::command]
pub async fn get_checkin_status(account_id: String) -> Result<Value, String> {
    let acc = account::find_account(&account_id).ok_or("账号不存在")?;
    Ok(checkin::get_checkin_status(&acc).await)
}

/// POST /api/credits —— 查询单账号积分资源及到期时间。
#[tauri::command]
pub async fn get_credit_expiry(account_id: String) -> Result<Value, String> {
    let acc = account::find_account(&account_id).ok_or("账号不存在")?;
    Ok(credits::get_credit_expiry(&acc).await)
}

/// GET /api/credits/stats —— 本地快照与官方请求用量统计。
/// `refresh = true` 时才重新请求官方用量；默认读缓存。
#[tauri::command]
pub async fn get_credit_statistics(refresh: Option<bool>) -> Value {
    credit_usage::get_statistics(refresh.unwrap_or(false)).await
}

#[tauri::command]
pub async fn get_token_statistics(days: Option<i64>) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || token_stats::get_statistics(days))
        .await
        .map_err(|error| format!("扫描 Token 统计失败: {error}"))
}

/// POST /api/checkin —— 单账号立即签到。
#[tauri::command]
pub async fn checkin(account_id: String) -> Result<Value, String> {
    let acc = account::find_account(&account_id).ok_or("账号不存在")?;
    Ok(checkin::checkin_account(&acc).await)
}

/// POST /api/checkin/all —— 全部账号立即签到。
#[tauri::command]
pub async fn checkin_all() -> Value {
    checkin::run_checkin_all().await
}

/// GET /api/checkin/config —— 自动签到配置。
#[tauri::command]
pub fn get_auto_checkin_config() -> Value {
    crate::modules::config::load_checkin_config()
}

/// POST /api/checkin/config —— 保存自动签到配置。
#[tauri::command]
pub fn save_auto_checkin_config(config: Value) -> Result<Value, String> {
    crate::modules::config::save_checkin_config(&config).map_err(|e| e.to_string())?;
    Ok(crate::modules::config::load_checkin_config())
}

/// GET /api/checkin/logs —— 签到日志。
#[tauri::command]
pub fn get_checkin_logs() -> Value {
    json!({ "logs": crate::modules::config::load_checkin_logs() })
}

// ---------------------------------------------------------------------------
// 派猫猫旅行
// ---------------------------------------------------------------------------

/// GET /api/travel/status —— 查询单账号今日旅行状态标签。
#[tauri::command]
pub async fn get_travel_status(account_id: String) -> Result<Value, String> {
    account::find_account(&account_id).ok_or("账号不存在")?;
    travel::reconcile_due_travel(Some(account_id.as_str())).await;
    Ok(travel::travel_display(&account_id))
}

/// GET /api/travel/config —— 自动旅行配置。
#[tauri::command]
pub fn get_auto_travel_config() -> Value {
    crate::modules::config::load_travel_config()
}

/// POST /api/travel/config —— 保存自动旅行配置。开启时立刻跑一轮派发/领取。
#[tauri::command]
pub fn save_auto_travel_config(config: Value) -> Result<Value, String> {
    crate::modules::config::save_travel_config(&config).map_err(|e| e.to_string())?;
    let saved = crate::modules::config::load_travel_config();
    if saved.get("enabled").and_then(Value::as_bool) == Some(true) {
        tauri::async_runtime::spawn(async {
            let _ = travel::run_travel_cycle().await;
            let _ = travel::run_travel_claim_cycle().await;
        });
    }
    Ok(saved)
}

// ---------------------------------------------------------------------------
// 自动轮换（CodeBuddy CLI）
// ---------------------------------------------------------------------------

/// GET /api/rotate/config —— 自动轮换配置。
#[tauri::command]
pub fn get_auto_rotate_config() -> Value {
    crate::modules::config::load_auto_rotate_config()
}

/// POST /api/rotate/config —— 保存自动轮换配置。
#[tauri::command]
pub fn save_auto_rotate_config(config: Value) -> Result<Value, String> {
    crate::modules::config::save_auto_rotate_config(&config).map_err(|e| e.to_string())?;
    Ok(crate::modules::config::load_auto_rotate_config())
}

/// GET /api/rotate/status —— 轮换状态（配置 + 上次检查/切换）。
#[tauri::command]
pub fn rotate_status() -> Value {
    rotate::rotate_status()
}

/// POST /api/rotate/run —— 手动触发一次轮换检查。
#[tauri::command]
pub async fn run_rotate() -> Value {
    rotate::run_rotate_cycle().await
}

/// GET /api/rotate/logs —— 最近轮换日志。
#[tauri::command]
pub fn get_rotate_logs() -> Value {
    json!({ "logs": rotate::rotate_logs() })
}

/// POST /api/refresh-token —— 单账号刷新 token。
#[tauri::command]
pub async fn refresh_account_token(account_id: String) -> Result<Value, String> {
    let acc = account::find_account(&account_id).ok_or("账号不存在")?;
    let fresh = refresh::refresh_account_token(acc).await;
    Ok(account::account_meta(&fresh))
}

// ---------------------------------------------------------------------------
// 阶段 4：自动更新
// ---------------------------------------------------------------------------

/// GET /api/update/config —— 更新源配置（owner/repo/token）。
#[tauri::command]
pub fn get_github_config() -> Value {
    update::load_github_config()
}

/// POST /api/update/config —— 保存更新源配置。
#[tauri::command]
pub fn save_github_config(config: Value) -> Result<Value, String> {
    update::save_github_config(&config).map_err(|e| e.to_string())?;
    Ok(update::load_github_config())
}

/// GET /api/update/check —— 检查 GitHub Releases 是否有新版本。
/// force=true 时绕过缓存强制刷新（设置页手动检查）。
#[tauri::command]
pub async fn check_update(proxy: Option<String>, force: Option<bool>) -> Value {
    update::update_check(proxy.as_deref(), force.unwrap_or(false)).await
}

/// 启动当前应用的新进程并退出旧进程，用于更新安装完成后的立即重启。
#[tauri::command]
pub fn relaunch_app() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|e| format!("无法定位应用程序: {e}"))?;
    // 更新重启是普通启动路径；不要把系统自启专用参数带给新进程。
    let args = std::env::args_os().skip(1).filter(|arg| {
        #[cfg(desktop)]
        {
            should_forward_relaunch_arg(arg.as_os_str())
        }
        #[cfg(not(desktop))]
        {
            true
        }
    });
    std::process::Command::new(executable)
        .args(args)
        .spawn()
        .map_err(|e| format!("启动应用失败: {e}"))?;
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// 开机自启（仅桌面端；webui 不提供同名接口）
// ---------------------------------------------------------------------------

/// GET /api/launch-at-login —— 查询系统当前的开机自启注册状态。
///
/// 以 tauri-plugin-autostart 的 OS 状态为唯一事实来源，不另存本地布尔值。
#[tauri::command]
pub fn get_launch_at_login_enabled(_app: tauri::AppHandle) -> Result<bool, String> {
    #[cfg(desktop)]
    {
        use tauri_plugin_autostart::ManagerExt;
        return _app
            .autolaunch()
            .is_enabled()
            .map_err(|e| format!("查询开机自启状态失败：{e}"));
    }
    #[cfg(not(desktop))]
    {
        Err("当前平台不支持开机自启".to_string())
    }
}

#[cfg(desktop)]
fn should_forward_relaunch_arg(arg: &std::ffi::OsStr) -> bool {
    arg != std::ffi::OsStr::new(crate::tray::SILENT_STARTUP_ARG)
}

#[cfg(all(test, desktop))]
mod relaunch_tests {
    use super::should_forward_relaunch_arg;
    use std::ffi::OsStr;

    #[test]
    fn update_relaunch_drops_only_the_exact_silent_startup_arg() {
        assert!(!should_forward_relaunch_arg(OsStr::new("--hidden")));
        assert!(should_forward_relaunch_arg(OsStr::new("--hidden-x")));
        assert!(should_forward_relaunch_arg(OsStr::new("x--hidden")));
        assert!(should_forward_relaunch_arg(OsStr::new("--debug")));
    }
}

/// POST /api/launch-at-login —— 注册 / 移除系统开机自启，并回读权威状态。
///
/// 回读结果与请求值不一致时按失败处理并返回当前真实状态，避免假装设置成功。
#[tauri::command]
pub fn set_launch_at_login_enabled(_app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    #[cfg(desktop)]
    {
        use tauri_plugin_autostart::ManagerExt;
        let autostart = _app.autolaunch();
        let action = if enabled { "开启" } else { "关闭" };
        let result = if enabled {
            autostart.enable()
        } else {
            autostart.disable()
        };
        if let Err(e) = result {
            return Err(format!("{action}开机自启失败：{e}"));
        }
        let authoritative = autostart
            .is_enabled()
            .map_err(|e| format!("开机自启设置后回读状态失败：{e}"))?;
        if authoritative != enabled {
            return Err(format!(
                "{action}开机自启未生效（系统当前状态：{}），请稍后重试",
                if authoritative {
                    "已开启"
                } else {
                    "未开启"
                }
            ));
        }
        Ok(authoritative)
    }
    #[cfg(not(desktop))]
    {
        let _ = enabled;
        Err("当前平台不支持开机自启".to_string())
    }
}

// ------------------------------------------------------------------ API 反代

/// 读取反代配置（国内版 / 国际版双入口 + 账号白名单）。
#[tauri::command]
pub fn get_proxy_config() -> Result<Value, String> {
    serde_json::to_value(proxy::load_proxy_config()).map_err(|e| e.to_string())
}

/// 保存反代配置并立即生效（启用/停用、改地址、改 key 都在这里完成）。
///
/// 返回**逐版本**的启动结果：某个入口起不来（最常见的是「那一版没有账号」）
/// 不该让整个保存动作失败，更不该连累另一个入口。
#[tauri::command]
pub fn save_proxy_config(config: Value) -> Result<Value, String> {
    let cfg: proxy::ProxyConfig =
        serde_json::from_value(config).map_err(|e| format!("配置格式不正确：{e}"))?;
    proxy::save_proxy_config(&cfg)?;
    let outcomes: Vec<Value> = proxy::apply_config(&cfg)
        .iter()
        .map(|(edition, r)| {
            json!({
                "edition": edition.key(),
                "label": edition.label(),
                "ok": r.is_ok(),
                "error": r.as_ref().err(),
            })
        })
        .collect();
    Ok(json!({ "ok": true, "running": proxy::proxy_running(), "endpoints": outcomes }))
}

/// 反代运行状态：哪几个入口在监听、各自生效的配置是什么。
#[tauri::command]
pub fn get_proxy_status() -> Value {
    let cfg = proxy::load_proxy_config();
    let endpoints: Vec<Value> = wb_switch_core::modules::edition::Edition::ALL
        .iter()
        .map(|&e| {
            let ep = cfg.endpoint(e);
            json!({
                "edition": e.key(),
                "label": e.label(),
                "enabled": ep.enabled,
                "listen": ep.listen,
                "hasApiKey": !ep.api_key.trim().is_empty(),
                // 该入口自己的账号数（0 = 该版本的全部账号都参与）
                "accountCount": ep.accounts.len(),
                "running": proxy::is_running(e),
            })
        })
        .collect();
    json!({
        "running": proxy::proxy_running(),
        "endpoints": endpoints,
    })
}

/// 设置账号分组："desktop"=桌面端 / "proxy"=反代API / ""=未分组。
#[tauri::command]
pub fn set_account_group(account_id: String, group: String) -> Result<Value, String> {
    let mut accounts = account::load_accounts();
    let pos = accounts
        .iter()
        .position(|a| a.get("id").and_then(Value::as_str) == Some(account_id.as_str()))
        .ok_or_else(|| "账号不存在".to_string())?;

    let g = group.trim().to_string();
    if !g.is_empty() && g != "desktop" && g != "proxy" {
        return Err(format!("未知分组：{g}"));
    }
    if let Some(obj) = accounts[pos].as_object_mut() {
        if g.is_empty() {
            obj.remove("group");
        } else {
            obj.insert("group".to_string(), json!(g));
        }
    }
    account::save_accounts(&accounts).map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true, "id": account_id, "group": g }))
}

/// 反代用量统计（即使服务没在跑也能读历史数据）。
#[tauri::command]
pub fn get_proxy_usage() -> Value {
    proxy::load_usage()
}

/// 清空反代用量统计。
#[tauri::command]
pub fn reset_proxy_usage() -> Result<Value, String> {
    proxy::reset_usage()?;
    Ok(json!({ "ok": true }))
}

/// 「获取模型ID」：先问本机反代（同一份数据、不额外占号），拿不到再直连上游。
///
/// async：要走两次可能很慢的网络请求（本地 HTTP + 上游），不能占主线程。
#[tauri::command]
pub async fn get_proxy_models(edition: Option<String>) -> Result<Value, String> {
    // 不传 edition = 国内版 + 国际版各拉一次再分档返回；
    // 传 "domestic" / "international" 只拉那一档。两版模型列表不一样，必须分开看。
    proxy::fetch_models(edition).await
}

// ---------------------------------------------------------------------------
// 跨机器迁移包（扫描 / 导出 / 预览 / 导入）
// ---------------------------------------------------------------------------

/// 从 JS 传来的选项对象里读一个布尔值（camelCase 与 snake_case 都认）。
fn opt_bool(v: &Value, keys: &[&str], default: bool) -> bool {
    for k in keys {
        if let Some(b) = v.get(*k).and_then(|x| x.as_bool()) {
            return b;
        }
    }
    default
}

/// 同上，读字符串数组（空串会被剔掉）。
fn opt_str_list(v: &Value, keys: &[&str]) -> Vec<String> {
    for k in keys {
        if let Some(a) = v.get(*k).and_then(|x| x.as_array()) {
            return a
                .iter()
                .filter_map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
        }
    }
    Vec::new()
}

/// GET /api/transfer/scan —— 列出可以导出的工作区、会话数与各类数据的体积。
#[tauri::command]
pub fn transfer_scan(edition: Option<String>) -> Value {
    let edition = edition.as_deref().map(parse_lenient).unwrap_or_default();
    transfer::scan_exportable(edition)
}

// ---------------------------------------------------------------- 缓存迁移
//
// 把家目录里的 `~/.workbuddy` / `~/.workbuddy-ai` 挪到别的盘，原路径改建
// NTFS 目录联接。会扫几十万个文件、复制十几 GB，全部挪到阻塞线程池。

/// GET /api/cache-move/plan —— 只读体检（扫体积，重 IO）。
#[tauri::command]
pub async fn cache_move_plan(dest: Option<String>) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        serde_json::to_value(cache_move::plan(dest))
            .unwrap_or_else(|e| json!({ "error": e.to_string() }))
    })
    .await
    .map_err(|e| format!("体检任务异常：{e}"))
}

/// GET /api/cache-move/verify —— 只读：当前联接状态 + 遗留备份。
#[tauri::command]
pub async fn cache_move_verify() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(cache_move::verify)
        .await
        .map_err(|e| format!("验证任务异常：{e}"))
}

/// POST /api/cache-move/run —— 执行迁移，边跑边推 `cache-move-progress`。
///
/// `targets` 是逐目录的目标：国内版与国际版可以放在不同的盘/文件夹，
/// 也可以都指向同一个根目录（默认就是这样）。
#[tauri::command]
pub async fn cache_move_run(
    app: tauri::AppHandle,
    targets: Vec<cache_move::MoveTarget>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let progress = |p: cache_move::Progress| {
            let payload = serde_json::to_value(&p).unwrap_or_else(|_| json!({}));
            let _ = app.emit("cache-move-progress", payload);
        };
        cache_move::run(targets, Some(&progress))
    })
    .await
    .map_err(|e| format!("迁移任务异常：{e}"))?
}

/// POST /api/cache-move/rollback —— 删联接、把备份改名回来。
#[tauri::command]
pub async fn cache_move_rollback(app: tauri::AppHandle) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let progress = |p: cache_move::Progress| {
            let payload = serde_json::to_value(&p).unwrap_or_else(|_| json!({}));
            let _ = app.emit("cache-move-progress", payload);
        };
        cache_move::rollback(Some(&progress))
    })
    .await
    .map_err(|e| format!("回滚任务异常：{e}"))?
}

/// GET /api/cache-move/backups —— 列出可清理的 `.moved-*` 备份。
#[tauri::command]
pub async fn cache_move_backups() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| json!({ "backups": cache_move::list_backups() }))
        .await
        .map_err(|e| format!("任务异常：{e}"))
}

/// POST /api/cache-move/cleanup —— 删除选中的备份目录。
///
/// 几十万个小文件，删起来要几十秒 —— 边删边推 `cache-move-progress`，别让界面看着像卡死。
#[tauri::command]
pub async fn cache_move_cleanup(
    app: tauri::AppHandle,
    paths: Vec<String>,
    permanent: Option<bool>,
) -> Result<Value, String> {
    // permanent = true 时不走回收站，直接永久删除（用户明确要的那条路）。
    let permanent = permanent.unwrap_or(false);
    tauri::async_runtime::spawn_blocking(move || {
        let progress = |p: cache_move::Progress| {
            let payload = serde_json::to_value(&p).unwrap_or_else(|_| json!({}));
            let _ = app.emit("cache-move-progress", payload);
        };
        cache_move::cleanup(&paths, permanent, Some(&progress))
    })
    .await
    .map_err(|e| format!("清理任务异常：{e}"))?
}

/// POST /api/cache-move/open —— 在文件管理器里打开一个目录。
///
/// 迁移之后应用看到的仍是原家目录路径（联接对程序透明），这个口子用来把人
/// 直接带到数据真正所在的目标盘。
#[tauri::command]
pub async fn cache_move_open(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || cache_move::open_path(&path))
        .await
        .map_err(|e| format!("打开目录异常：{e}"))?
}

// ---------------------------------------------------------------------------
// 更新防护
//
// WorkBuddy 的更新缓存按用户共用、不分产品，A 版下载的包会被 B 版套用，
// 把对方程序目录覆盖掉（2026-10-01 / 10-08 各发生一次）。这里把更新源指到黑洞
// 并隔离已下载的包。要读写注册表 + 扫安装目录，挪到阻塞线程池。

/// GET /api/update-guard/status —— 只读：开关状态 + 缓存内容 + 两个安装目录的身份。
#[tauri::command]
pub async fn update_guard_status() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        serde_json::to_value(update_guard::status())
            .unwrap_or_else(|e| json!({ "error": e.to_string() }))
    })
    .await
    .map_err(|e| format!("读取更新防护状态异常：{e}"))
}

/// POST /api/update-guard/set —— 开/关更新防护；开启时可顺带隔离已下载的包。
#[tauri::command]
pub async fn update_guard_set(disabled: bool, clear_cache: Option<bool>) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        update_guard::set_disabled(disabled, clear_cache.unwrap_or(true))
    })
    .await
    .map_err(|e| format!("设置更新防护异常：{e}"))?
}

/// POST /api/update-guard/clear-cache —— 只隔离缓存里已下载的包，不动开关。
#[tauri::command]
pub async fn update_guard_clear_cache() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(update_guard::clear_cache)
        .await
        .map_err(|e| format!("清理更新缓存异常：{e}"))?
}

/// POST /api/update-guard/set-frozen —— 单独冻结/解冻更新暂存目录。
#[tauri::command]
pub async fn update_guard_set_frozen(freeze: bool) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || update_guard::set_cache_frozen(freeze))
        .await
        .map_err(|e| format!("冻结暂存目录异常：{e}"))?
}

/// POST /api/transfer/export —— 把选中的工作区 + 配置打成一个 zip。
///
/// 打包是重 IO，可能几秒到几十秒 —— 挪到阻塞线程池，别占着 async runtime。
#[tauri::command]
pub async fn transfer_export(
    edition: Option<String>,
    options: Option<Value>,
    output: String,
) -> Result<Value, String> {
    let edition = edition.as_deref().map(parse_lenient).unwrap_or_default();
    if output.trim().is_empty() {
        return Err("缺少导出路径".to_string());
    }
    let v = options.unwrap_or_else(|| json!({}));
    let out = PathBuf::from(output);
    tauri::async_runtime::spawn_blocking(move || {
        let opts = transfer::ExportOptions {
            slugs: opt_str_list(&v, &["slugs"]),
            include_config: opt_bool(&v, &["includeConfig", "include_config"], true),
            include_plugins: opt_bool(&v, &["includePlugins", "include_plugins"], false),
            include_file_history: opt_bool(
                &v,
                &["includeFileHistory", "include_file_history"],
                false,
            ),
            include_workspace_snapshots: opt_bool(
                &v,
                &["includeWorkspaceSnapshots", "include_workspace_snapshots"],
                false,
            ),
            include_credentials: opt_bool(
                &v,
                &["includeCredentials", "include_credentials"],
                false,
            ),
        };
        transfer::export_bundle(edition, &opts, &out)
    })
    .await
    .map_err(|e| format!("导出任务异常：{e}"))?
}

/// POST /api/transfer/preview —— 只读预览：包里有什么、本地缺什么、会发生什么。
#[tauri::command]
pub async fn transfer_preview(edition: Option<String>, path: String) -> Result<Value, String> {
    let edition = edition.as_deref().map(parse_lenient).unwrap_or_default();
    if path.trim().is_empty() {
        return Err("缺少包路径".to_string());
    }
    let p = PathBuf::from(path);
    tauri::async_runtime::spawn_blocking(move || transfer::preview_bundle(edition, &p))
        .await
        .map_err(|e| format!("预览任务异常：{e}"))?
}

/// POST /api/transfer/import —— 按选项增量合并进来。
#[tauri::command]
pub async fn transfer_import(
    edition: Option<String>,
    path: String,
    options: Option<Value>,
) -> Result<Value, String> {
    let edition = edition.as_deref().map(parse_lenient).unwrap_or_default();
    if path.trim().is_empty() {
        return Err("缺少包路径".to_string());
    }
    let v = options.unwrap_or_else(|| json!({}));
    let p = PathBuf::from(path);
    tauri::async_runtime::spawn_blocking(move || {
        let opts = transfer::ImportOptions {
            session_ids: opt_str_list(&v, &["sessionIds", "session_ids"]),
            apply_sessions: opt_bool(&v, &["applySessions", "apply_sessions"], true),
            apply_blobs: opt_bool(&v, &["applyBlobs", "apply_blobs"], true),
            apply_config: opt_bool(&v, &["applyConfig", "apply_config"], true),
            config_keys: opt_str_list(&v, &["configKeys", "config_keys"]),
            apply_db: opt_bool(&v, &["applyDb", "apply_db"], true),
            apply_credentials: opt_bool(&v, &["applyCredentials", "apply_credentials"], false),
            overwrite: opt_bool(&v, &["overwrite"], false),
            // 默认置空 user_id ⇒ 变成「共享会话」，换机器/换账号都看得见。
            share_sessions: opt_bool(&v, &["shareSessions", "share_sessions"], true),
        };
        transfer::import_bundle(edition, &p, &opts)
    })
    .await
    .map_err(|e| format!("导入任务异常：{e}"))?
}
