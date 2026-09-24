//! 官方认证文件 `workbuddy-desktop.info` 的路径与读写（四段 JSON）。
//!
//! 对照 server.py `auth_file_path` / `workbuddy_app_path` / `read_auth_file` /
//! `import_from_auth_file`。切换写入（build_account_obj / build_auth_obj /
//! write_account_to_auth_file）在阶段 2 随 switch.rs 落地。
//!
//! **版本支持（2026-09-24 加）**：国内版文件是同目录下的 `workbuddy-desktop.info`，
//! 国际版是 `workbuddy-desktop-ai.info`。所有 `*_for(Edition)` 函数按版本选文件名；
//! 不带 `_for` 的旧函数等价于国内版，保持调用点不变。

use serde_json::{json, Map, Value};
use std::path::PathBuf;

use crate::modules::account::get_str;
use crate::modules::config::{atomic_write, backup_dir, now_ms, utc_iso};
use crate::modules::edition::{edition_of, Edition};

/// 国内版认证文件路径（等价于 `auth_file_path_for(Edition::Domestic)`）。
pub fn auth_file_path() -> PathBuf {
    auth_file_path_for(Edition::Domestic)
}

/// 指定版本的官方认证文件路径。
pub fn auth_file_path_for(edition: Edition) -> PathBuf {
    edition.auth_file_path()
}

/// WorkBuddy 应用路径。
pub fn workbuddy_app_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    return crate::modules::process::macos_workbuddy_app_path();

    #[cfg(target_os = "windows")]
    {
        // 探测顺序：运行进程 Path → 缓存 → 注册表 → 环境变量/盘符扫描。
        // 都找不到时返回 LOCALAPPDATA 默认路径，供启动失败文案写出尝试路径。
        if let Some(exe) = crate::modules::process::windows_workbuddy_exe_path() {
            return exe;
        }
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        return std::path::Path::new(&local)
            .join("Programs")
            .join("WorkBuddy")
            .join("WorkBuddy.exe");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return PathBuf::from("/usr/bin/workbuddy");
}

/// 读取认证文件 JSON；不存在或解析失败返回 None。
pub fn read_auth_file() -> Option<Value> {
    read_auth_file_for(Edition::Domestic)
}

/// 读取指定版本的认证文件 JSON。
pub fn read_auth_file_for(edition: Edition) -> Option<Value> {
    let path = auth_file_path_for(edition);
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 切换前备份当前认证文件，返回备份路径。对照 server.py `backup_auth_file`。
pub fn backup_auth_file() -> Option<PathBuf> {
    backup_auth_file_for(Edition::Domestic)
}

/// 备份指定版本的认证文件。备份名带版本前缀，两个版本互不覆盖。
pub fn backup_auth_file_for(edition: Edition) -> Option<PathBuf> {
    let path = auth_file_path_for(edition);
    if !path.exists() {
        return None;
    }
    let dir = backup_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let ts = utc_iso();
    let dest = dir.join(format!("{}.{ts}.info", edition.backup_prefix()));
    std::fs::copy(&path, &dest).ok()?;
    Some(dest)
}

/// 从账号库记录构造官方 account 字段。对照 server.py `build_account_obj`。
pub fn build_account_obj(acc: &Value) -> Value {
    let mut obj: Map<String, Value> = match acc.get("profile_raw") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    obj.insert(
        "uid".to_string(),
        acc.get("uid").cloned().unwrap_or_else(|| json!("")),
    );
    obj.insert(
        "nickname".to_string(),
        acc.get("nickname").cloned().unwrap_or_else(|| json!("")),
    );
    setdefault(&mut obj, "type", json!("personal"));
    setdefault(&mut obj, "accountType", json!(""));
    setdefault(&mut obj, "idp", json!(""));
    setdefault(&mut obj, "oneidAccountId", json!(""));
    setdefault(&mut obj, "areaInfoComplete", json!(false));
    setdefault(&mut obj, "isCurrentOneIdEnterprise", json!(false));
    setdefault(&mut obj, "isCurrentOneIdPersonal", json!(false));
    setdefault(&mut obj, "isFirstLogin", json!(false));
    setdefault(&mut obj, "isCreator", json!(false));
    setdefault(&mut obj, "isAdmin", json!(false));
    setdefault(&mut obj, "uin", json!(""));
    setdefault(&mut obj, "phoneNumber", json!(""));
    setdefault(&mut obj, "lastLogin", json!(true));
    setdefault(&mut obj, "pluginEnabled", json!(true));
    setdefault(
        &mut obj,
        "deployStatus",
        json!({"statusCode": 0, "statusMsg": "", "detailMsg": ""}),
    );
    setdefault(
        &mut obj,
        "sso",
        json!({"domain": "", "domainModifiedTimes": 0}),
    );
    Value::Object(obj)
}

/// 从账号库记录构造官方 auth 字段。对照 server.py `build_auth_obj`。
pub fn build_auth_obj(acc: &Value) -> Value {
    let mut obj: Map<String, Value> = Map::new();
    let raw = acc.get("auth_raw");
    if let Some(Value::Object(m)) = raw {
        let inner = match m.get("auth") {
            Some(Value::Object(im)) => im.clone(),
            _ => m.clone(),
        };
        obj.extend(inner);
    }
    let token_type = acc
        .get("token_type")
        .and_then(|v| v.as_str())
        .unwrap_or("Bearer")
        .to_string();
    let expires_at = acc.get("expiresAt").and_then(|v| v.as_i64());
    let now = now_ms();

    obj.insert(
        "accessToken".to_string(),
        get_str(acc, "access_token").unwrap_or_default().into(),
    );
    obj.insert(
        "refreshToken".to_string(),
        get_str(acc, "refresh_token").unwrap_or_default().into(),
    );
    obj.insert("tokenType".to_string(), token_type.into());
    // domain 决定客户端连哪个后端（国内 www.codebuddy.cn / 国际 www.workbuddy.ai）。
    // 账号记录里缺 domain 时按版本兜底，避免写出空域名把客户端指到错误后端。
    let domain = get_str(acc, "domain")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| edition_of(acc).api_domain().to_string());
    obj.insert("domain".to_string(), domain.into());
    obj.insert("lastRefreshTime".to_string(), json!(now));
    setdefault(
        &mut obj,
        "scope",
        json!("openid profile offline_access email"),
    );

    if let Some(expires_at) = expires_at {
        obj.insert("expiresAt".to_string(), json!(expires_at));
        obj.insert(
            "expiresIn".to_string(),
            json!(((expires_at - now) / 1000).max(0)),
        );
        let refresh_exp = raw
            .and_then(|r| r.get("refreshExpiresAt"))
            .and_then(|v| v.as_i64())
            .unwrap_or(expires_at);
        if !obj.contains_key("refreshExpiresAt") {
            obj.insert("refreshExpiresAt".to_string(), json!(refresh_exp));
        }
        obj.insert(
            "refreshExpiresIn".to_string(),
            json!(((refresh_exp - now) / 1000).max(0)),
        );
    } else {
        setdefault(&mut obj, "expiresIn", json!(0));
        setdefault(&mut obj, "refreshExpiresIn", json!(0));
    }
    setdefault(&mut obj, "notBeforePolicy", json!(0));
    setdefault(&mut obj, "sessionState", json!(""));
    Value::Object(obj)
}

/// 把账号写入官方认证文件（原子写 + 写后校验）。对照 server.py `write_account_to_auth_file`。
pub fn write_account_to_auth_file(acc: &Value) -> Result<(), String> {
    write_account_to_auth_file_for(Edition::Domestic, acc)
}

/// 把账号写入指定版本的认证文件。
///
/// 两个版本的 `auth.domain` 不同（`www.codebuddy.cn` / `www.workbuddy.ai`），
/// 由 `build_auth_obj` 按账号自身的 `edition` 决定；这里只负责选对文件。
pub fn write_account_to_auth_file_for(edition: Edition, acc: &Value) -> Result<(), String> {
    let path = auth_file_path_for(edition);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let existing = read_auth_file_for(edition).unwrap_or_else(|| json!({}));
    eprintln!(
        "[auth] write_account: existing is_object={} allAccounts_len={}",
        existing.is_object(),
        existing
            .get("allAccounts")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    );
    let all_accounts = existing
        .get("allAccounts")
        .cloned()
        .or_else(|| existing.get("accounts").cloned())
        .filter(|v| v.is_array())
        .unwrap_or_else(|| json!([]));
    let account_obj = build_account_obj(acc);
    let auth_obj = build_auth_obj(acc);

    // 把目标账号并入 allAccounts（去重：按 uid 或 id）
    let target_uid = get_str(acc, "uid").unwrap_or_default();
    let mut all: Vec<Value> = all_accounts.as_array().cloned().unwrap_or_default();
    all.retain(|a| {
        let primary = a
            .get("uid")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                a.get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("");
        primary != target_uid
    });
    all.push(account_obj.clone());
    eprintln!("[auth] write_account: merged allAccounts len={}", all.len());

    let session = json!({
        "account": &account_obj,
        "auth": &auth_obj,
        "accounts": &all,
        "allAccounts": &all,
    });
    let content = serde_json::to_string_pretty(&session).map_err(|e| e.to_string())?;
    if let Err(e) = atomic_write(&path, &content) {
        eprintln!("[auth] atomic_write FAILED: {e}");
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            return Err(
                "无权限写入认证文件：请打开 系统设置→隐私与安全性→App 管理，允许本 App 控制 WorkBuddy 的数据（或为其开启『完全磁盘访问』后重试）"
                    .to_string(),
            );
        }
        return Err(e.to_string());
    }

    // 写后校验
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let written_token = written
        .get("auth")
        .and_then(|a| a.get("accessToken"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let expect_token = get_str(acc, "access_token").unwrap_or_default();
    if written_token != expect_token {
        return Err("认证文件写后校验失败，未写入目标账号".to_string());
    }
    Ok(())
}

fn setdefault(map: &mut Map<String, Value>, key: &str, value: Value) {
    if !map.contains_key(key) {
        map.insert(key.to_string(), value);
    }
}

/// 从当前 WorkBuddy 登录态导入账号。对照 server.py `import_from_auth_file`。
pub fn import_from_auth_file() -> Option<Value> {
    import_from_auth_file_for(Edition::Domestic)
}

/// 从指定版本的登录态导入账号，并在记录上打 `edition` 标记。
pub fn import_from_auth_file_for(edition: Edition) -> Option<Value> {
    let mut acc = imported_account_from_root(read_auth_file_for(edition)?)?;
    acc["edition"] = json!(edition.key());
    Some(acc)
}

/// 更新客户端自己记的「当前账号」快照（`<数据目录>/storage/skeleton/account-snapshot.json`）。
///
/// 机制移植自 <https://github.com/Harvey-Will/workbuddy-tools>（MIT License，
/// Copyright (c) Harvey-Will）的 `core/accounts.py::switch_account`：
/// 只改 `primary.{uid,nickname,savedAt}`，写前备份、原子写。
///
/// ⚠️ 本机实测：**国际版有该文件，国内版当前版本没有**（`~/.workbuddy` 下连
/// `storage/` 目录都不存在）。文件不存在时返回 `Ok(false)` 且**不创建**，
/// 避免给国内版凭空造一个客户端不认识的文件。
///
/// 返回 `Ok(true)` 表示确实改写了。
pub fn write_account_snapshot(edition: Edition, uid: &str, nickname: &str) -> Result<bool, String> {
    let path = edition.account_snapshot_path();
    if !path.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut data: Value = serde_json::from_str(&text).map_err(|e| {
        format!("account-snapshot.json 无法解析，已中止以保护原文件: {e}")
    })?;
    if !data.is_object() {
        return Err("account-snapshot.json 结构异常，已中止".to_string());
    }

    let dir = backup_dir();
    std::fs::create_dir_all(&dir).ok();
    let dest = dir.join(format!(
        "account-snapshot.{}.{}.json",
        edition.key(),
        utc_iso()
    ));
    let _ = std::fs::copy(&path, &dest);

    if !data.get("primary").map(Value::is_object).unwrap_or(false) {
        data["primary"] = json!({});
    }
    if let Some(primary) = data.get_mut("primary").and_then(|v| v.as_object_mut()) {
        primary.insert("uid".to_string(), json!(uid));
        // 不把空昵称写进客户端，否则界面会显示空白账号名。
        if !nickname.trim().is_empty() {
            primary.insert("nickname".to_string(), json!(nickname.trim()));
        }
        primary.insert("savedAt".to_string(), json!(now_ms()));
    } else {
        return Err("account-snapshot.json primary 结构异常，已中止".to_string());
    }

    let content = serde_json::to_string_pretty(&data).map_err(|e| e.to_string())?;
    atomic_write(&path, &content).map_err(|e| e.to_string())?;
    Ok(true)
}

fn imported_account_from_root(root: Value) -> Option<Value> {
    let account_obj = root
        .get("account")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let auth_obj = root
        .get("auth")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));

    let uid = get_str(&root, "uid").or_else(|| get_str(&account_obj, "uid"));
    let uid = uid.or_else(|| get_str(&account_obj, "id"));
    let nickname = get_str(&root, "nickname")
        .or_else(|| get_str(&root, "name"))
        .or_else(|| get_str(&account_obj, "nickname"))
        .or_else(|| get_str(&account_obj, "label"));
    let email = get_str(&root, "email")
        .or_else(|| get_str(&account_obj, "email"))
        .or_else(|| get_str(&auth_obj, "email"));
    let access_token = get_str(&auth_obj, "accessToken")
        .or_else(|| get_str(&auth_obj, "access_token"))
        .or_else(|| get_str(&root, "accessToken"))
        .or_else(|| get_str(&root, "access_token"));
    let refresh_token = get_str(&auth_obj, "refreshToken")
        .or_else(|| get_str(&auth_obj, "refresh_token"))
        .or_else(|| get_str(&root, "refreshToken"))
        .or_else(|| get_str(&root, "refresh_token"));
    let token_type = get_str(&auth_obj, "tokenType")
        .or_else(|| get_str(&auth_obj, "token_type"))
        .unwrap_or_else(|| "Bearer".to_string());
    let domain = get_str(&root, "domain").or_else(|| get_str(&auth_obj, "domain"));
    let expires_at = parse_ts(root.get("expiresAt").or_else(|| auth_obj.get("expiresAt")));
    let refresh_expires_at = parse_ts(
        root.get("refreshExpiresAt")
            .or_else(|| auth_obj.get("refreshExpiresAt")),
    );

    if access_token.is_none() {
        return None;
    }

    Some(json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "uid": uid,
        "nickname": nickname,
        "email": email,
        "enterpriseName": get_str(&root, "enterpriseName")
            .or_else(|| get_str(&root, "enterprise_name"))
            .or_else(|| get_str(&account_obj, "enterpriseName"))
            .or_else(|| get_str(&account_obj, "enterprise_name")),
        "enterpriseId": get_str(&root, "enterpriseId")
            .or_else(|| get_str(&root, "enterprise_id"))
            .or_else(|| get_str(&account_obj, "enterpriseId"))
            .or_else(|| get_str(&account_obj, "enterprise_id")),
        "access_token": access_token,
        "refresh_token": refresh_token,
        "token_type": token_type,
        "domain": domain,
        "expiresAt": expires_at,
        "refreshExpiresAt": refresh_expires_at,
        "auth_raw": root,
        "profile_raw": account_obj,
        "createdAt": now_ms(),
    }))
}

/// 字符串时间戳转 i64（数字原样保留，不做秒/毫秒换算）。
/// 对照 server.py `import_from_auth_file` 的 str→int 逻辑。
fn parse_ts(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok().map(|f| f as i64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auth_file_path_is_expected_location() {
        let p = auth_file_path();
        let s = p.to_string_lossy();
        assert!(
            s.contains("CodeBuddyExtension"),
            "路径应包含 CodeBuddyExtension: {s}"
        );
        assert!(
            s.ends_with("workbuddy-desktop.info"),
            "文件名应为 workbuddy-desktop.info: {s}"
        );
    }

    #[test]
    fn import_from_auth_file_extracts_fields() {
        let root = json!({
            "account": {"uid": "u-1", "nickname": "小明", "email": "a@b.c"},
            "auth": {
                "accessToken": "AT-1",
                "refreshToken": "RT-1",
                "tokenType": "Bearer",
                "domain": "www.codebuddy.cn",
                "expiresAt": "1791912333558",
            },
            "domain": "www.codebuddy.cn",
        });
        // import_from_auth_file 从真实认证文件读取，此处直接测 parse_ts 与字段提取逻辑
        assert_eq!(parse_ts(root["auth"].get("expiresAt")), Some(1791912333558));
        assert_eq!(parse_ts(root["auth"].get("refreshToken")), None);
        assert_eq!(parse_ts(Some(&json!("1786728333"))), Some(1786728333));
    }

    #[test]
    fn import_without_email_does_not_synthesize_one() {
        let account = imported_account_from_root(json!({
            "account": {"uid": "u-1", "nickname": "同名用户"},
            "auth": {"accessToken": "test-token"}
        }))
        .expect("auth payload should import");

        assert_eq!(account["uid"], "u-1");
        assert_eq!(account["nickname"], "同名用户");
        assert!(account["email"].is_null());
    }
}
