//! 账号切换：备份 → 关进程 → 共享会话（可选）→ 复制会话（可选）→ 写认证 → 启动。
//!
//! 对照 server.py `switch_account`。切换过程中通过进度回调向前端推送实时进度，
//! 避免界面长时间无反馈被误认为卡死。core 不依赖 Tauri，进度回调由宿主适配
//! （桌面端转发为 `switch-progress` 事件，HTTP 端写入轮询/SSE）。
//!
//! **版本支持（2026-09-24）**：国内版走原有 `process.rs` 全链路；
//! 国际版走 `client_ctl` 的简化流程（关/开 WorkBuddyAI + 写 `-ai.info` + 写客户端快照）。
//!
//! **会话复制 / 共享两个档位都支持**。早先以为国际版没有会话库所以跳过，那是照抄上游
//! `variant.rs` 的结论，**本机实测是错的**：`~/.workbuddy-ai` 下有完整的
//! `workbuddy.db`(sessions 表) + `projects/{workspace}/{cid}.jsonl` + `edge-sync-mapping-v3.db`。
//! 现在改成按磁盘实况探测（`Edition::supports_session_sharing`），探测到才做、探测不到才跳过。

use serde_json::{json, Value};

use crate::modules::account;
use crate::modules::auth_file;
use crate::modules::client_ctl;
use crate::modules::edition::{edition_of, Edition};
use crate::modules::process::{close_workbuddy, launch_workbuddy};
use crate::modules::session;

/// 切换进度回调（宿主注入，如 Tauri `app.emit` 或 HTTP 进度缓存）。
pub type ProgressFn = Box<dyn Fn(&str) + Send + Sync>;

/// 切换账号（国内版）。保留旧签名，调用点无需改动。
pub fn switch_account(
    progress_fn: Option<&ProgressFn>,
    account_id: &str,
    restart: bool,
    share_sessions: bool,
    copy_session_ids: &[String],
) -> Result<Value, String> {
    switch_account_in_edition(
        progress_fn,
        account_id,
        restart,
        share_sessions,
        copy_session_ids,
        Edition::Domestic,
    )
}

/// 切换账号到指定版本的客户端。
///
/// `copy_session_ids` 非空时按路径 B 复制勾选会话；`share_sessions` 为真时按路径 C
/// 把会话归属清空。两者都**按目标档位操作该档位自己的会话库**，
/// 且只在 `restart = true` 时执行（客户端运行中不宜写库）。
pub fn switch_account_in_edition(
    progress_fn: Option<&ProgressFn>,
    account_id: &str,
    restart: bool,
    share_sessions: bool,
    copy_session_ids: &[String],
    edition: Edition,
) -> Result<Value, String> {
    let progress = |message: &str| {
        eprintln!("[switch] progress: {message}");
        if let Some(p) = progress_fn {
            p(message);
        }
    };

    progress("开始切换账号…");
    let acc =
        account::find_account(account_id).ok_or_else(|| format!("账号不存在: {account_id}"))?;

    // 账号自带版本标记；缺省视为国内版（向后兼容老账号库）。
    let acc_edition = edition_of(&acc);
    if acc_edition != edition {
        return Err(format!(
            "该账号属于「{}」，不能切到「{}」客户端",
            acc_edition.label(),
            edition.label()
        ));
    }

    let backup = auth_file::backup_auth_file_for(edition);

    let mut copy_report: Option<Value> = None;
    let mut session_report: Option<Value> = None;

    // 会话能力按磁盘实况探测：两个档位现在都有会话库，但仍保留探测以便客户端改版后优雅降级
    let session_capable = session::supports_session_sharing_for(edition);

    if restart {
        // 客户端运行中不宜写会话库，所以只在重启场景关闭进程
        if edition == Edition::Domestic {
            progress("正在关闭 WorkBuddy…");
            close_workbuddy(20)?;
        } else {
            progress("正在关闭 WorkBuddyAI…");
            client_ctl::close(edition, 20)?;
        }
        if !copy_session_ids.is_empty() {
            if session_capable {
                progress("正在复制会话到目标账号…");
                copy_report =
                    session::copy_sessions_for_switch_for(edition, &acc, copy_session_ids);
            } else {
                progress("该版本没有可用的会话库，已跳过会话复制");
            }
        }
        if share_sessions {
            if session_capable {
                // 路径 C：把 user_id 置空 → 任何账号登录都能看到同一份，不产生副本。
                progress("正在把会话设为多账号共享…");
                session_report = Some(
                    match session::share_sessions_for_switch_for(edition) {
                        Ok(r) => r,
                        Err(e) => json!({"error": e}),
                    },
                );
            } else {
                progress("该版本没有可用的会话库，已跳过会话共享");
            }
        }
    } else if edition == Edition::International && client_ctl::is_running(edition) {
        // 不重启就没法安全改认证文件，国际版先要求用户手动退出
        return Err("WorkBuddyAI 正在运行，请先完全退出后再切换".to_string());
    }

    progress("正在写入认证文件…");
    auth_file::write_account_to_auth_file_for(edition, &acc)?;

    // 客户端自己记的「当前账号」快照：国际版有该文件、国内版当前版本没有。
    // 失败不影响主流程（认证文件才是决定登录身份的东西）。
    let uid = account::get_str(&acc, "uid").unwrap_or_default();
    let nickname = account::account_display_name(&acc);
    match auth_file::write_account_snapshot(edition, &uid, &nickname) {
        Ok(true) => progress("已同步客户端账号快照"),
        Ok(false) => {}
        Err(e) => eprintln!("[switch] 账号快照写入失败（不影响认证文件）: {e}"),
    }

    if restart {
        progress("正在启动客户端…");
        if edition == Edition::Domestic {
            launch_workbuddy(Some(&progress))?;
        } else {
            client_ctl::launch(edition)?;
        }
    }
    progress("切换完成");

    let mut result = json!({
        "ok": true,
        "account": account::account_display_name(&acc),
        "edition": edition.key(),
        "backup": backup.map(|p| p.to_string_lossy().to_string()),
    });
    if let Some(c) = copy_report {
        result["sessionCopy"] = c;
    }
    if let Some(s) = session_report {
        result["sessionShare"] = s;
    }
    Ok(result)
}
