//! 会话列表与按需复制（路径 B：生成新 id，云端可正常同步）。
//!
//! 对照 server.py `current_user_uid` / `list_sessions_for_user` /
//! `_find_project_jsonl` / `copy_session_to_user` / `_register_edge_sync_mapping` /
//! `copy_sessions_for_switch` / `backup_workbuddy_db` / `workbuddy_db_path`。
//!
//! WorkBuddy 5.x 数据三件套（缺一不可）：
//!   1) 正文：`~/.workbuddy/projects/{workspace}/{cid}.jsonl`（JSONL 含 sessionId 字段）
//!   2) 元数据：`~/.workbuddy/workbuddy.db` sessions 表（id = conversation id = UUID）
//!   3) 云端映射：`~/.workbuddy/edge-sync-mapping.db` edge_sync_mapping
//!      （session_id=conversation_id，msg_channel=convmsg:{uid} 决定云端归属）
//!
//! ## 档位（国内版 / 国际版）
//!
//! **两个档位都有完整的三件套**，只是落在各自的数据目录，且映射库文件名带版本后缀：
//!
//! | 项 | 国内版 | 国际版 |
//! |---|---|---|
//! | 元数据 | `~/.workbuddy/workbuddy.db` | `~/.workbuddy-ai/workbuddy.db` |
//! | 正文 | `~/.workbuddy/projects/` | `~/.workbuddy-ai/projects/` |
//! | 云端映射 | `edge-sync-mapping.db` | `edge-sync-mapping-v3.db` |
//!
//! 所以路径全部按 `Edition` 取，映射库走目录探测（见 `edge_sync_db_path_for`）。

use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::modules::auth_file;
use crate::modules::config::{backup_dir, now_ms, now_secs, utc_iso};
use crate::modules::edition::Edition;

/// 打开数据库并设置 busy_timeout（对照 Python `sqlite3.connect(timeout=5)`）。
fn open_db(path: &Path, read_only: bool) -> Option<Connection> {
    let conn = if read_only {
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?
    } else {
        Connection::open(path).ok()?
    };
    let _ = conn.busy_timeout(Duration::from_secs(5));
    Some(conn)
}

/// 会话元数据库路径（国内版；保留旧签名）。
pub fn workbuddy_db_path() -> PathBuf {
    workbuddy_db_path_for(Edition::Domestic)
}

/// 指定档位的会话元数据库路径。
pub fn workbuddy_db_path_for(edition: Edition) -> PathBuf {
    edition.db_path()
}

/// 云端映射库路径（国内版；保留旧签名）。
pub fn edge_sync_db_path() -> PathBuf {
    edge_sync_db_path_for(Edition::Domestic)
}

/// 指定档位的云端映射库路径。
///
/// ⚠️ 文件名带版本后缀且会随客户端升级变化（国内实测 `edge-sync-mapping.db`、
/// 国际 `edge-sync-mapping-v3.db`），所以**扫描数据目录探测**：在名字匹配
/// `edge-sync-mapping*.db` 且**确实含 `edge_sync_mapping` 表**的候选中，
/// 取修改时间最新的一个。探测不到就回落到该档位默认名（文件不存在时
/// 注册逻辑会静默跳过，不报错）。
///
/// 顺带修掉一个老 bug：之前硬编码 `-v2`，而实际的库叫 `edge-sync-mapping.db`，
/// 导致 `register_edge_sync_mapping` 一直返回 false（云端归属从未写成功）。
pub fn edge_sync_db_path_for(edition: Edition) -> PathBuf {
    let dir = edition.data_dir();
    let fallback = edition.edge_sync_db_path();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return fallback;
    };
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // 只认主库，跳过 -wal / -shm
        if !name.starts_with("edge-sync-mapping") || !name.ends_with(".db") {
            continue;
        }
        if !path.is_file() || !has_edge_sync_table(&path) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p).unwrap_or(fallback)
}

fn has_edge_sync_table(path: &Path) -> bool {
    let Some(conn) = open_db(path, true) else {
        return false;
    };
    table_exists(&conn, "edge_sync_mapping")
}

/// 当前认证账号的 uid（国内版；保留旧签名）。
pub fn current_user_uid() -> Option<String> {
    current_user_uid_for(Edition::Domestic)
}

/// 指定档位当前登录账号的 uid（认证文件 `account.uid`）。
pub fn current_user_uid_for(edition: Edition) -> Option<String> {
    let auth = auth_file::read_auth_file_for(edition)?;
    auth.get("account")
        .and_then(|a| a.get("uid"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// 该档位是否具备会话能力（按磁盘实况探测）。
pub fn supports_session_sharing_for(edition: Edition) -> bool {
    edition.supports_session_sharing()
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        == 1
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let Ok(iter) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    let names: Vec<String> = iter.flatten().collect();
    names.iter().any(|name| name == column)
}

fn nonempty_text(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 规范化工作目录：反斜杠转正斜杠、去尾斜杠、盘符大写。
///
/// 客户端所有路径比对都是字符串精确匹配，写法不一致会被当成两个不同目录。
fn normalize_workspace_path(raw: &str) -> String {
    let mut s = raw.trim().replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= 2 && chars[1] == ':' && chars[0].is_ascii_lowercase() {
        let mut owned = chars;
        owned[0] = owned[0].to_ascii_uppercase();
        s = owned.into_iter().collect();
    }
    s
}

/// 分组短名：普通目录取末段；一次性时间戳工作区命名为 `临时 MM-DD HH:mm`。
fn workspace_short_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let prefix = path.strip_suffix(base).unwrap_or("");
    let shape_ok = base.len() == 19
        && base.as_bytes().iter().enumerate().all(|(i, c)| match i {
            4 | 7 | 10 | 13 | 16 => *c == b'-',
            _ => c.is_ascii_digit(),
        });
    if shape_ok && prefix.to_ascii_lowercase().ends_with("/workbuddy/") {
        return format!(
            "临时 {} {}:{}",
            &base[5..10],
            &base[11..13],
            &base[14..16]
        );
    }
    base.to_string()
}

/// 补齐 `workspace-display-names.json`：把会话里出现过、但映射表缺失的工作目录补进去。
///
/// 客户端取名顺序是 `spaces.name` → 本文件 → 完整 `cwd`。分组走「按目录聚合」后，
/// 新目录若不在本文件里，侧栏会直接显示 `C:/Users/.../xxx` 这类完整路径。
/// 随切换增量补齐，不改动已有条目。
///
/// 只处理两种确定安全的输入：文件不存在（按当前格式新建），或已是 `{workspaces:{...}}`
/// 结构（增量追加）。其余情况一律跳过不写，避免改坏旧版本或其他工具写入的格式。
fn sync_workspace_display_names(conn: &Connection, db: &Path) -> Result<Value, String> {
    let Some(dir) = db.parent() else {
        return Ok(json!({ "added": 0, "total": 0 }));
    };
    let file = dir.join("workspace-display-names.json");

    // 结构不认识时直接跳过，一个字节都不写，避免遮蔽原有映射。
    let mut root: Value = if file.exists() {
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Ok(json!({ "added": 0, "total": 0, "skipped": "unreadable" }));
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
            return Ok(json!({ "added": 0, "total": 0, "skipped": "unparsable" }));
        };
        if !parsed.get("workspaces").is_some_and(Value::is_object) {
            return Ok(json!({ "added": 0, "total": 0, "skipped": "unknown-shape" }));
        }
        parsed
    } else {
        json!({ "version": 1, "workspaces": {} })
    };

    let obj = root.as_object_mut().ok_or("display names root not object")?;
    obj.entry("version").or_insert(json!(1));
    let workspaces = obj
        .get_mut("workspaces")
        .and_then(Value::as_object_mut)
        .ok_or("display names workspaces not object")?;
    let before = workspaces.len();

    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT cwd FROM sessions \
             WHERE COALESCE(deleted_at, -1) < 0 AND cwd IS NOT NULL AND cwd <> ''",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;

    let now = now_ms();
    for raw in rows.flatten() {
        let path = normalize_workspace_path(&raw);
        if path.is_empty() || workspaces.contains_key(&path) {
            continue;
        }
        workspaces.insert(
            path.clone(),
            json!({
                "id": &path,
                "path": &path,
                "displayName": workspace_short_name(&path),
                "updatedAt": now,
            }),
        );
    }

    let total = workspaces.len();
    let added = total - before;
    if added > 0 {
        let body = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
        std::fs::write(&file, body).map_err(|e| e.to_string())?;
    }
    Ok(json!({ "added": added, "total": total }))
}

/// WorkBuddy 侧栏展示名：优先 custom_title（用户改名 / 定时任务名），否则 title。
fn session_display_title(title: Option<String>, custom_title: Option<String>) -> String {
    nonempty_text(custom_title)
        .or_else(|| nonempty_text(title))
        .unwrap_or_else(|| "(无标题)".to_string())
}

/// Claw 是账号绑定的 IM 渠道工作区，复制会话行不够，目标账号也用不了。
fn is_claw_workspace(cwd: &str) -> bool {
    cwd.trim()
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("claw"))
}

/// 列出某账号未删除的会话（国内版；保留旧签名）。
pub fn list_sessions_for_user(uid: &str) -> Value {
    list_sessions_for_user_for(Edition::Domestic, uid)
}

/// 会话可见性条件 —— **必须与客户端完全一致**。
///
/// 两个客户端源码（`app.asar`）里都确认过：
/// - 国际版 SQL 原文：`AND (user_id = ? OR user_id = '')`
/// - 国内版注释原文：重建 projects 记录时 `userId` 设为空字符串，
///   「上层 getSessions 通过 `user_id IS NULL OR user_id = ''` 条件保证这些记录对当前用户可见」
///
/// 🔴 走完「共享会话」（把 `user_id` 置空）之后，如果这里只按 `user_id = ?1` 查，
/// 界面会显示"当前账号暂无会话" —— 客户端里明明看得到，我们却列不出来。
/// 用 `?1` 占位符编号，调用方按顺序绑定 uid。
const SQL_VISIBLE_TO_USER: &str = "(user_id = ?1 OR user_id = '' OR user_id IS NULL)";

/// 定位「源会话」时的匹配条件（`?1` = 会话 id，`?2` = 源 uid）。
///
/// 与 [`SQL_VISIBLE_TO_USER`] 同理：已共享的会话 `user_id` 为空，
/// 若按 `id = ?1 AND user_id = ?2` 精确匹配，会「找不到源行」而**静默不复制**。
const SQL_SOURCE_MATCH: &str = "id = ?1 AND (user_id = ?2 OR user_id = '' OR user_id IS NULL)";

/// 列出某账号未删除的会话（workbuddy.db sessions 表，db 为准）。
///
/// `title` 为 WorkBuddy 侧栏同款展示名；`isPlayground` 对应侧栏「任务」，
/// 其余按 `cwd` 最后一段归入「空间」。
///
/// 过滤条件见 [`SQL_VISIBLE_TO_USER`]：**跟随客户端的可见性规则**，
/// 而不是「只属于当前 uid」。
pub fn list_sessions_for_user_for(edition: Edition, uid: &str) -> Value {
    list_sessions_in(&workbuddy_db_path_for(edition), uid)
}

/// `list_sessions_for_user_for` 的实现（可传入 db 路径，便于单测）。
fn list_sessions_in(db: &Path, uid: &str) -> Value {
    if !db.is_file() {
        return json!([]);
    }
    let Some(conn) = open_db(db, true) else {
        return json!([]);
    };
    if !table_exists(&conn, "sessions") {
        return json!([]);
    }
    let has_custom = column_exists(&conn, "sessions", "custom_title");
    let has_playground = column_exists(&conn, "sessions", "is_playground");
    let custom_col = if has_custom { "custom_title" } else { "NULL" };
    let playground_col = if has_playground { "is_playground" } else { "0" };
    let sql = format!(
        "SELECT id, cwd, title, {custom_col}, updated_at, {playground_col} FROM sessions \
         WHERE deleted_at IS NULL AND {SQL_VISIBLE_TO_USER} ORDER BY updated_at DESC"
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return json!([]),
    };
    let rows = stmt.query_map([uid], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
        ))
    });

    let mut sessions: Vec<Value> = Vec::new();
    if let Ok(iter) = rows {
        for r in iter.flatten() {
            let (cid, cwd, title, custom_title, updated_at, is_playground) = r;
            let cid = cid.unwrap_or_default();
            let cwd = cwd.unwrap_or_default();
            if is_claw_workspace(&cwd) {
                continue;
            }
            sessions.push(json!({
                "id": cid,
                "title": session_display_title(title, custom_title),
                "cwd": cwd,
                "updatedAt": updated_at.unwrap_or(0),
                // 正文目录固定在库文件同级（数据目录）下
                "hasHistory": find_project_jsonl_in(data_dir_of(db), &cid).is_some(),
                "isPlayground": is_playground.unwrap_or(0) != 0,
            }));
        }
    }
    json!(sessions)
}

/// 会话库所在的数据目录（`workbuddy.db` 的父目录）。
fn data_dir_of(db: &Path) -> &Path {
    db.parent().unwrap_or_else(|| Path::new("."))
}

/// 在指定档位的 `projects/{workspace}/{cid}.jsonl` 定位会话正文。
fn find_project_jsonl_for(edition: Edition, cid: &str) -> Option<PathBuf> {
    find_project_jsonl_in(&edition.projects_dir(), cid)
}

/// 在 `projects/{workspace}/{cid}.jsonl` 里定位会话正文（可传入目录，便于单测）。
fn find_project_jsonl_in(projects: &Path, cid: &str) -> Option<PathBuf> {
    if !projects.is_dir() {
        return None;
    }
    let direct = projects.join(format!("{cid}.jsonl"));
    if direct.is_file() {
        return Some(direct);
    }
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let p = entry.path().join(format!("{cid}.jsonl"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 备份指定档位的 workbuddy.db（含 -wal/-shm），返回主库备份路径。
fn backup_workbuddy_db_for(edition: Edition, backup_root: &Path) -> Option<PathBuf> {
    let db = workbuddy_db_path_for(edition);
    if !db.is_file() {
        return None;
    }
    std::fs::create_dir_all(backup_root).ok()?;
    for suffix in ["", "-wal", "-shm"] {
        let src = PathBuf::from(format!("{}{}", db.to_string_lossy(), suffix));
        if src.is_file() {
            let _ = std::fs::copy(&src, backup_root.join(format!("workbuddy.db{suffix}")));
        }
    }
    Some(backup_root.join("workbuddy.db"))
}

/// 把 source_uid 的一个会话复制为 target_uid 的新会话（国内版；保留旧签名）。
pub fn copy_session_to_user(
    cid: &str,
    source_uid: &str,
    target_uid: &str,
) -> Result<Value, String> {
    copy_session_to_user_for(Edition::Domestic, cid, source_uid, target_uid)
}

/// 把 source_uid 的一个会话复制为 target_uid 的新会话（路径 B：生成新 id）。
///
/// 全部按「新 id」复制一份给目标账号，源账号数据完全不动。
/// 新 id 必须用带连字符的 UUID 格式（`Uuid::new_v4().to_string()`），与官方一致；
/// 32 位无连字符形式会导致 WorkBuddy 无法识别新会话。
pub fn copy_session_to_user_for(
    edition: Edition,
    cid: &str,
    source_uid: &str,
    target_uid: &str,
) -> Result<Value, String> {
    let new_cid = uuid::Uuid::new_v4().to_string();
    let db = workbuddy_db_path_for(edition);
    if let Some(conn) = open_db(&db, true) {
        // 源会话可能是「已共享」的（user_id 为空），所以不能只按 user_id 精确匹配
        let cwd: Option<String> = conn
            .query_row(
                &format!("SELECT cwd FROM sessions WHERE {SQL_SOURCE_MATCH}"),
                rusqlite::params![cid, source_uid],
                |r| r.get(0),
            )
            .ok();
        if cwd.as_deref().is_some_and(is_claw_workspace) {
            return Err("Claw 工作区绑定当前账号渠道，不支持复制".into());
        }
    }

    // 1) 复制正文 jsonl：{projects}/{ws}/{cid}.jsonl → {projects}/{ws}/{new_cid}.jsonl
    let mut jsonl_copied = false;
    let mut project_dir: Option<PathBuf> = None;
    if let Some(src_jsonl) = find_project_jsonl_for(edition, cid) {
        project_dir = src_jsonl.parent().map(|p| p.to_path_buf());
        let dst_jsonl = src_jsonl.with_file_name(format!("{new_cid}.jsonl"));
        if let Ok(text) = std::fs::read_to_string(&src_jsonl) {
            let text = text.replace(cid, &new_cid); // 替换 sessionId 等旧 id 引用
            if std::fs::write(&dst_jsonl, text).is_ok() {
                jsonl_copied = true;
            }
        }
    }

    // 2) 附属数据：工具输出外溢目录 / 产物索引 / 历史任务 / 文件改动历史。
    //
    // 🔴 这些数据全都**按会话 id 命名**。只搬正文的话，新会话指向的是一片空白 ——
    // 目标账号能看到对话，但工具输出、产物索引、任务历史全丢。
    //
    // 附件**不用搬**：`blobs/` 是内容寻址的全局仓库（`blobs/{sha256 前两位}/{sha256}`），
    // 不分账号，新会话引用同样的哈希、文件本来就在原地（详见 transfer 里的注释）。
    let side = crate::modules::transfer::copy_session_side_data(
        edition,
        cid,
        &new_cid,
        project_dir.as_deref(),
    );

    // 3) 备份 db（复制前），再 INSERT 新 sessions 行
    let backup_root = backup_dir().join("sessions").join(utc_iso());
    backup_workbuddy_db_for(edition, &backup_root);
    insert_session_copy(&db, &new_cid, cid, source_uid, target_uid)?;

    // 3) 注册云端映射：新会话归属目标账号（msg_channel=convmsg:{target_uid}）
    let mapping_written = register_edge_sync_mapping_for(edition, &new_cid, target_uid);

    Ok(json!({
        "id": cid,
        "newId": new_cid,
        "jsonlCopied": jsonl_copied,
        // 附属数据的搬运情况（sideFiles / blobsReferenced / blobsMissing / …）
        "side": side,
        "mappingWritten": mapping_written,
        "backup": backup_root.to_string_lossy().to_string(),
    }))
}

/// 在 workbuddy.db 中把源会话行复制为新 id（动态列，覆盖 id/user_id/时间戳）。
///
/// db 不存在或 sessions 表不存在时静默成功（对应 Python 版跳过）。源行不存在则无操作。
fn insert_session_copy(
    db_path: &Path,
    new_cid: &str,
    cid: &str,
    source_uid: &str,
    target_uid: &str,
) -> Result<(), String> {
    if !db_path.is_file() {
        return Ok(());
    }
    let Some(conn) = open_db(db_path, false) else {
        return Ok(());
    };
    if !table_exists(&conn, "sessions") {
        return Ok(());
    }
    let mut src_stmt = conn
        .prepare(&format!("SELECT * FROM sessions WHERE {SQL_SOURCE_MATCH}"))
        .map_err(|e| e.to_string())?;
    let cols: Vec<String> = src_stmt
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut rows = src_stmt
        .query(rusqlite::params![cid, source_uid])
        .map_err(|e| e.to_string())?;
    if let Ok(Some(row)) = rows.next() {
        let mut vals: Vec<rusqlite::types::Value> = Vec::with_capacity(cols.len());
        for (i, col) in cols.iter().enumerate() {
            let v = row
                .get::<_, rusqlite::types::Value>(i)
                .unwrap_or(rusqlite::types::Value::Null);
            if col == "cwd" {
                if let rusqlite::types::Value::Text(ref path) = v {
                    if is_claw_workspace(path) {
                        return Err("Claw 工作区绑定当前账号渠道，不支持复制".into());
                    }
                }
            }
            match col.as_str() {
                "id" => vals.push(rusqlite::types::Value::Text(new_cid.to_string())),
                "user_id" => vals.push(rusqlite::types::Value::Text(target_uid.to_string())),
                "created_at" | "updated_at" => vals.push(rusqlite::types::Value::Integer(now_ms())),
                "deleted_at" => vals.push(rusqlite::types::Value::Null),
                _ => vals.push(v),
            }
        }
        drop(rows);
        drop(src_stmt);

        let placeholders = cols.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let colnames = cols.join(", ");
        let sql = format!("INSERT OR REPLACE INTO sessions ({colnames}) VALUES ({placeholders})");
        let params: Vec<&rusqlite::types::Value> = vals.iter().collect();
        conn.execute(&sql, rusqlite::params_from_iter(params))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 把新会话注册进 edge_sync_mapping（云端归属关键）。失败不致命，返回 False。
fn register_edge_sync_mapping_for(edition: Edition, new_cid: &str, target_uid: &str) -> bool {
    insert_edge_sync_mapping(&edge_sync_db_path_for(edition), new_cid, target_uid)
}

fn insert_edge_sync_mapping(db_path: &Path, new_cid: &str, target_uid: &str) -> bool {
    if !db_path.is_file() {
        return false;
    }
    let Some(conn) = open_db(db_path, false) else {
        return false;
    };
    if !table_exists(&conn, "edge_sync_mapping") {
        return false;
    }
    let created_at = now_secs();
    let r = conn.execute(
        "INSERT OR REPLACE INTO edge_sync_mapping \
         (session_id, conversation_id, msg_channel, created_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            new_cid,
            new_cid,
            format!("convmsg:{target_uid}"),
            created_at
        ],
    );
    match r {
        Ok(_) => true,
        Err(_) => false,
    }
}

/// 切换前把勾选的会话复制到目标账号（国内版；保留旧签名）。
pub fn copy_sessions_for_switch(target_acc: &Value, session_ids: &[String]) -> Option<Value> {
    copy_sessions_for_switch_for(Edition::Domestic, target_acc, session_ids)
}

/// 切换前把勾选的会话复制到目标账号（路径 B）。返回复制报告。
///
/// `edition` 决定读写哪个档位的会话库 —— 源 uid 也从**该档位的认证文件**读，
/// 否则国际版会拿国内版的 uid 去匹配，永远找不到源会话。
pub fn copy_sessions_for_switch_for(
    edition: Edition,
    target_acc: &Value,
    session_ids: &[String],
) -> Option<Value> {
    let target_uid = target_acc
        .get("uid")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if target_uid.is_empty() {
        return None;
    }
    let source_uid = current_user_uid_for(edition)?;
    if source_uid == target_uid {
        return None;
    }

    let mut report = json!({
        "sourceUid": source_uid,
        "targetUid": target_uid,
        "copied": [],
    });
    let mut errors: Vec<Value> = Vec::new();
    for cid in session_ids {
        match copy_session_to_user_for(edition, cid, &source_uid, &target_uid) {
            Ok(r) => report["copied"].as_array_mut().unwrap().push(r),
            Err(e) => errors.push(json!({"id": cid, "error": e})),
        }
    }
    if !errors.is_empty() {
        report["errors"] = json!(errors);
    }
    Some(report)
}

/// Claw 工作区判定（SQL 片段用）：Claw 绑定 IM 渠道账号，不参与共享。
const SQL_NOT_CLAW: &str = "(LOWER(cwd) NOT LIKE '%\\claw' AND LOWER(cwd) NOT LIKE '%/claw' \
                            AND LOWER(cwd) <> 'claw')";
const SQL_IS_CLAW: &str = "(LOWER(cwd) LIKE '%\\claw' OR LOWER(cwd) LIKE '%/claw' \
                           OR LOWER(cwd) = 'claw')";

/// 把所有会话改成「多账号共享」（路径 C）。
///
/// 原理：WorkBuddy 侧栏的过滤条件是
///     `user_id = 当前账号 OR user_id 为空`
/// 把 `user_id` 置空后，**任何一个账号登录都能看到同一份会话**，
/// 磁盘上仍然只有一份正文，不产生副本。
///
/// 与 `copy_sessions_for_switch`（复制成新 id，越切越多）不同，本路径**不新增任何行**，
/// 因此可以放心地在每次切换时执行。
///
/// 同时把「分组」的 `user_id` 对齐到目标账号 —— 新版客户端按账号隔离分组，
/// 不对齐会导致切号后分组整体不可见。会话本身仍保持无主，不产生副本。
///
/// Claw 工作区绑定 IM 渠道账号，跳过不动。
pub fn share_sessions_for_switch() -> Result<Value, String> {
    share_sessions_for_switch_for(Edition::Domestic, None)
}

/// 指定档位的「多账号共享」（两个档位都有独立会话库，各自处理）。
///
/// `target_acc` 是切换的目标账号。切换流程里认证文件尚未写入，
/// 此时 `current_user_uid_for` 拿到的还是**旧账号**，所以必须由调用方显式传入。
/// 传 `None` 时回退到读认证文件（用于切换完成之后再对齐的场景）。
pub fn share_sessions_for_switch_for(
    edition: Edition,
    target_acc: Option<&Value>,
) -> Result<Value, String> {
    let uid = target_acc
        .and_then(|v| v.get("uid"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| current_user_uid_for(edition))
        .ok_or_else(|| "无法确定目标账号 uid".to_string())?;
    share_sessions_in(&workbuddy_db_path_for(edition), &uid)
}

/// `share_sessions_for_switch` 的实现（可传入 db 路径与目标 uid，便于单测）。
fn share_sessions_in(db: &Path, target_uid: &str) -> Result<Value, String> {
    if !db.is_file() {
        return Err(format!("找不到 {db:?}"));
    }
    let Some(conn) = open_db(db, false) else {
        return Err("无法打开 workbuddy.db".to_string());
    };
    if !table_exists(&conn, "sessions") {
        return Err("sessions 表不存在".to_string());
    }

    let alive: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let sql_share = format!(
        "UPDATE sessions SET user_id = '' \
         WHERE deleted_at IS NULL AND user_id <> '' AND {SQL_NOT_CLAW}"
    );
    let shared = conn.execute(&sql_share, []).map_err(|e| e.to_string())?;

    // 分组（spaces）归属对齐。
    //
    // 新版客户端把分组独立成表，列表查询按账号隔离：
    //     WHERE user_id = <当前账号> AND status = 'active' AND deleted_at < 0
    // 切号后必须把 user_id 跟到目标账号，否则分组整体不可见。
    //
    // 不要清空 sessions.space_id，也不要软删分组 —— 分组详情页的条件是
    //     space_id = ? AND is_playground = 0 AND (user_id = <当前账号> OR user_id = '')
    // 无主会话仍能进分组，只有侧栏计数走严格 JOIN（user_id 相等）会得到 0。
    //
    // 旧版客户端没有 spaces 表，跳过。
    let spaces_reassigned = if table_exists(&conn, "spaces") {
        let now = now_ms();
        conn.execute(
            "UPDATE spaces SET user_id = ?1, updated_at = ?2 \
             WHERE deleted_at < 0 AND user_id <> ?1",
            rusqlite::params![target_uid, now],
        )
        .unwrap_or(0) as i64
    } else {
        0
    };

    // 新目录若不在显示名映射里，分组标题会退化成完整路径。
    let display_names = sync_workspace_display_names(&conn, db)
        .unwrap_or_else(|e| json!({ "error": e }));

    let sql_claw = format!(
        "SELECT COUNT(*) FROM sessions WHERE deleted_at IS NULL AND user_id <> '' AND {SQL_IS_CLAW}"
    );
    let skipped_claw: i64 = conn.query_row(&sql_claw, [], |r| r.get(0)).unwrap_or(0);

    Ok(json!({
        "total": alive,
        "shared": shared as i64,
        "skippedClaw": skipped_claw,
        "spacesReassigned": spaces_reassigned,
        "displayNames": display_names,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn db_paths_are_resolved_per_edition() {
        // 用 Path 比较而不是字符串后缀，避免 Windows 反斜杠导致误判
        let cn = workbuddy_db_path_for(Edition::Domestic);
        assert!(cn.starts_with(Edition::Domestic.data_dir()));
        assert_eq!(cn.file_name().and_then(|s| s.to_str()), Some("workbuddy.db"));

        let ai = workbuddy_db_path_for(Edition::International);
        assert!(ai.starts_with(Edition::International.data_dir()));
        assert_ne!(cn, ai, "两个档位的会话库不能是同一个文件");
    }

    #[test]
    fn edge_sync_db_is_discovered_from_data_dir() {
        // 探测到的路径必须落在该档位数据目录里，且文件名以 edge-sync-mapping 开头
        for e in Edition::ALL {
            let p = edge_sync_db_path_for(e);
            assert!(
                p.starts_with(e.data_dir()),
                "{} 的映射库跑到了别的目录: {p:?}",
                e.label()
            );
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            assert!(name.starts_with("edge-sync-mapping"), "遇到非预期文件名 {name}");
            assert!(name.ends_with(".db") && !name.ends_with("-wal") && !name.ends_with("-shm"));
        }
    }

    #[test]
    fn shared_sessions_are_listed_for_every_account() {
        // 客户端侧栏条件是 `user_id = 我 OR user_id = '' OR user_id IS NULL`；
        // 我们的列表必须一致，否则走完「共享会话」后界面会显示"当前账号暂无会话"。
        let db = temp_db("list_shared");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, user_id TEXT, cwd TEXT, title TEXT,
                created_at INTEGER, updated_at INTEGER, deleted_at INTEGER);
             INSERT INTO sessions VALUES
                ('s-shared', '',    'C:/proj/a', '共享会话',   1, 10, NULL),
                ('s-mine',   'u-1', 'C:/proj/a', '我的会话',   1, 20, NULL),
                ('s-other',  'u-2', 'C:/proj/b', '别人的会话', 1, 30, NULL),
                ('s-null',   NULL,  'C:/proj/c', '空归属会话', 1, 35, NULL),
                ('s-gone',   '',    'C:/proj/a', '已删会话',   1, 40, 99);",
        )
        .unwrap();
        drop(conn);

        let listed = list_sessions_in(&db, "u-1");
        let titles: Vec<String> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["title"].as_str().unwrap_or("").to_string())
            .collect();
        for expect in ["共享会话", "我的会话", "空归属会话"] {
            assert!(titles.contains(&expect.to_string()), "{expect} 没被列出: {titles:?}");
        }
        assert!(!titles.contains(&"别人的会话".to_string()), "串了别人的会话: {titles:?}");
        assert!(!titles.contains(&"已删会话".to_string()), "已删会话不该列出: {titles:?}");

        let _ = std::fs::remove_file(&db);
    }

    fn temp_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "wb_switch_test_{}_{name}.db",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn share_sessions_clears_user_id_and_skips_claw() {
        let db = temp_db("share");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, user_id TEXT, cwd TEXT, title TEXT,
                created_at INTEGER, updated_at INTEGER, deleted_at INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('s1','uid-a','F:\\Code\\api','t',1,1,NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('s2','uid-b','C:\\Users\\x\\WorkBuddy\\Claw','c',1,1,NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('s3','uid-a','F:\\Code\\api','deleted',1,1,999)",
            [],
        )
        .unwrap();
        drop(conn);

        let report = share_sessions_in(&db, "uid-b").unwrap();
        assert_eq!(report["shared"], 1, "只应共享未删除的非 Claw 会话: {report}");
        assert_eq!(report["skippedClaw"], 1, "Claw 应被跳过: {report}");

        let conn = Connection::open(&db).unwrap();
        let (s1, s2, s3): (String, String, String) = (
            conn.query_row("SELECT user_id FROM sessions WHERE id='s1'", [], |r| r.get(0))
                .unwrap(),
            conn.query_row("SELECT user_id FROM sessions WHERE id='s2'", [], |r| r.get(0))
                .unwrap(),
            conn.query_row("SELECT user_id FROM sessions WHERE id='s3'", [], |r| r.get(0))
                .unwrap(),
        );
        assert_eq!(s1, "", "普通会话应被置空");
        assert_eq!(s2, "uid-b", "Claw 会话归属应保持不动");
        assert_eq!(s3, "uid-a", "已删除的会话不该被动");
    }

    #[test]
    fn share_sessions_missing_db_is_err_not_panic() {
        let db = temp_db("share-missing");
        assert!(share_sessions_in(&db, "uid-b").is_err());
    }

    /// 切号后分组归属应跟到目标账号，但会话保持无主、绑定关系不得丢。
    /// 回归点：早期实现软删分组并清空 space_id，会导致侧栏分组整体消失。
    #[test]
    fn share_sessions_reassigns_space_owner_keeping_bindings() {
        let db = temp_db("share-spaces");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, user_id TEXT, cwd TEXT, title TEXT,
                created_at INTEGER, updated_at INTEGER, deleted_at INTEGER,
                space_id TEXT);
             CREATE TABLE spaces (
                id TEXT PRIMARY KEY, user_id TEXT NOT NULL, device_id TEXT NOT NULL,
                name TEXT NOT NULL, primary_folder_path TEXT, create_folder_path TEXT,
                pinned INTEGER NOT NULL DEFAULT 0, sort_order INTEGER,
                status TEXT NOT NULL DEFAULT 'active',
                transport TEXT NOT NULL DEFAULT 'local',
                conversation_origin TEXT, source_kind TEXT,
                deleted_at INTEGER NOT NULL DEFAULT -1,
                created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
             INSERT INTO spaces VALUES
                ('sp1','uid-old','dev','proj',NULL,NULL,0,NULL,'active','local',NULL,'user',-1,1,1);
             INSERT INTO sessions VALUES ('s1','uid-old','C:/proj','t',1,1,NULL,'sp1');",
        )
        .unwrap();
        drop(conn);

        let report = share_sessions_in(&db, "uid-new").unwrap();
        assert_eq!(report["spacesReassigned"], 1, "分组应被改归属: {report}");

        let conn = Connection::open(&db).unwrap();
        let sess_uid: String = conn
            .query_row("SELECT user_id FROM sessions WHERE id='s1'", [], |r| r.get(0))
            .unwrap();
        let sess_space: String = conn
            .query_row("SELECT space_id FROM sessions WHERE id='s1'", [], |r| r.get(0))
            .unwrap();
        let space_uid: String = conn
            .query_row("SELECT user_id FROM spaces WHERE id='sp1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sess_uid, "", "会话应保持无主，才能跨账号共享");
        assert_eq!(sess_space, "sp1", "分组绑定不能被清掉");
        assert_eq!(space_uid, "uid-new", "分组归属应跟到目标账号");
    }

    #[test]
    fn insert_session_copy_duplicates_row_with_target_uid() {
        let db = temp_db("sessions");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                title TEXT,
                cwd TEXT,
                created_at INTEGER,
                updated_at INTEGER,
                deleted_at INTEGER,
                payload BLOB
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, user_id, title, cwd, created_at, updated_at, deleted_at, payload)
             VALUES ('src-1', 'uid-a', '旧标题', '/ws', 1000, 2000, NULL, x'DEADBEEF')",
            [],
        )
        .unwrap();

        insert_session_copy(&db, "new-uuid-1", "src-1", "uid-a", "uid-b").unwrap();

        let (id, user_id, title, deleted_at): (String, String, String, Option<i64>) = conn
            .query_row(
                "SELECT id, user_id, title, deleted_at FROM sessions WHERE id = 'new-uuid-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(id, "new-uuid-1");
        assert_eq!(user_id, "uid-b");
        assert_eq!(title, "旧标题"); // 普通列原样保留
        assert_eq!(deleted_at, None); // deleted_at 置空

        // 源行保持不变
        let src_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = 'src-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(src_count, 1);
    }

    #[test]
    fn insert_session_copy_missing_source_is_noop() {
        let db = temp_db("noop");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, user_id TEXT, title TEXT, created_at INTEGER, updated_at INTEGER, deleted_at INTEGER);",
        )
        .unwrap();
        insert_session_copy(&db, "new-1", "missing", "uid-a", "uid-b").unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn insert_session_copy_missing_db_is_ok() {
        let db = temp_db("missing");
        // 不创建文件
        assert!(insert_session_copy(&db, "new-1", "src-1", "a", "b").is_ok());
    }

    #[test]
    fn insert_edge_sync_mapping_registers_channel() {
        let db = temp_db("edge");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE edge_sync_mapping (
                session_id TEXT,
                conversation_id TEXT,
                msg_channel TEXT,
                created_at INTEGER
            );",
        )
        .unwrap();
        assert!(insert_edge_sync_mapping(&db, "new-1", "uid-b"));
        let (sid, cid, channel): (String, String, String) = conn
            .query_row(
                "SELECT session_id, conversation_id, msg_channel FROM edge_sync_mapping",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(sid, "new-1");
        assert_eq!(cid, "new-1");
        assert_eq!(channel, "convmsg:uid-b");
    }

    #[test]
    fn insert_edge_sync_mapping_missing_table_false() {
        let db = temp_db("edge-no-table");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE other (x INTEGER);")
            .unwrap();
        assert!(!insert_edge_sync_mapping(&db, "new-1", "uid-b"));
    }

    #[test]
    fn session_display_title_prefers_custom_title() {
        assert_eq!(
            session_display_title(Some("自动标题".into()), Some("美团每日自动领券".into())),
            "美团每日自动领券"
        );
        assert_eq!(
            session_display_title(None, Some("美团每日自动领券".into())),
            "美团每日自动领券"
        );
        assert_eq!(
            session_display_title(Some("汉字详情页".into()), None),
            "汉字详情页"
        );
        assert_eq!(session_display_title(None, None), "(无标题)");
        assert_eq!(
            session_display_title(Some("  ".into()), Some("".into())),
            "(无标题)"
        );
    }

    #[test]
    fn claw_workspace_detected_by_folder_name() {
        assert!(is_claw_workspace("/Users/apple/WorkBuddy/Claw"));
        assert!(is_claw_workspace("/Users/apple/WorkBuddy/claw/"));
        assert!(is_claw_workspace(r"C:\Users\me\WorkBuddy\Claw"));
        assert!(!is_claw_workspace("/Users/apple/WorkBuddy/ClawBot"));
        assert!(!is_claw_workspace(
            "/Users/apple/Documents/AI-PROJECT/LetterTotTown"
        ));
    }
}
