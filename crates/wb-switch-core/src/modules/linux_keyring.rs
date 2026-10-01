//! Linux Secret Service（`org.freedesktop.secrets`）直连读取：取 Chromium / Electron
//! Safe Storage 存在系统密钥环里的密码。
//!
//! 为什么不再依赖 `secret-tool`：它来自 libsecret-tools，Ubuntu / Debian 默认不安装。
//! 缺失时 Electron 明明已经把密码写进了 gnome-keyring，我们也读不到，切换账号只会报
//! 「无法加载 Linux secret storage key（v11）」（issue #80）。这里直接走 D-Bus 协议，
//! `zbus` 本就在 tauri 依赖图里，不引入新的下载。
//!
//! 只取「已解锁」条目：对锁定条目取值会拉起解锁弹窗并可能长时间阻塞，宁可让上层给出
//! 明确错误，也不能把调用线程挂住。

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// 一次密钥环查询的总超时。D-Bus / 密钥环守护进程异常时不能挂住调用线程，
/// 与 `process` 模块给 `secret-tool` 做超时兜底是同一个考虑。
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

const SECRET_SERVICE_NAME: &str = "org.freedesktop.secrets";
const SECRET_SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SECRET_SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
const SECRET_ITEM_IFACE: &str = "org.freedesktop.Secret.Item";

/// Chromium / Electron 写入 Safe Storage 密码时使用的 Secret Service 属性名。
const APPLICATION_ATTRIBUTE: &str = "application";

/// 按 `application` 属性逐个候选名查密码（明文），返回第一个命中的。
///
/// 任何一步失败（没有会话总线、密钥环未实现、条目不存在等）都返回 `None`，
/// 由调用方决定如何降级或报错。
pub(crate) fn find_password(applications: &[&str]) -> Option<String> {
    let applications: Vec<String> = applications.iter().map(|app| (*app).to_string()).collect();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(find_password_blocking(&applications));
    });
    receiver.recv_timeout(LOOKUP_TIMEOUT).ok().flatten()
}

fn find_password_blocking(applications: &[String]) -> Option<String> {
    let connection = Connection::session().ok()?;
    let session = open_session(&connection)?;
    applications
        .iter()
        .find_map(|application| lookup_application(&connection, &session, application))
}

/// `Service.OpenSession("plain", "")`：明文会话，拿到后续取密钥要用的会话路径。
fn open_session(connection: &Connection) -> Option<OwnedObjectPath> {
    let reply = connection
        .call_method(
            Some(SECRET_SERVICE_NAME),
            SECRET_SERVICE_PATH,
            Some(SECRET_SERVICE_IFACE),
            "OpenSession",
            &("plain", Value::from("")),
        )
        .ok()?;
    let (_output, session): (OwnedValue, OwnedObjectPath) = reply.body().deserialize().ok()?;
    Some(session)
}

/// `Service.SearchItems({"application": <name>})`：按属性检索，取已解锁条目再逐个取值。
fn lookup_application(
    connection: &Connection,
    session: &OwnedObjectPath,
    application: &str,
) -> Option<String> {
    let attributes: HashMap<&str, &str> =
        [(APPLICATION_ATTRIBUTE, application)].into_iter().collect();
    let reply = connection
        .call_method(
            Some(SECRET_SERVICE_NAME),
            SECRET_SERVICE_PATH,
            Some(SECRET_SERVICE_IFACE),
            "SearchItems",
            &(attributes,),
        )
        .ok()?;
    let (unlocked, _locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
        reply.body().deserialize().ok()?;
    unlocked
        .iter()
        .find_map(|item| item_password(connection, item, session))
}

/// `Item.GetSecret(session)`：返回 `(session, parameters, value, content_type)`。
fn item_password(
    connection: &Connection,
    item: &OwnedObjectPath,
    session: &OwnedObjectPath,
) -> Option<String> {
    let reply = connection
        .call_method(
            Some(SECRET_SERVICE_NAME),
            item.as_str(),
            Some(SECRET_ITEM_IFACE),
            "GetSecret",
            &(session,),
        )
        .ok()?;
    let (_session, _parameters, value, _content_type): (OwnedObjectPath, Vec<u8>, Vec<u8>, String) =
        reply.body().deserialize().ok()?;
    String::from_utf8(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_application_returns_none() {
        // 不存在的候选名：有密钥环的环境返回 None（查不到），没有会话总线 / 没有
        // 密钥环的环境同样返回 None。无论哪种情况都不该 panic 或挂住。
        let found = find_password(&["workbuddy-switch-no-such-keyring-entry"]);
        assert_eq!(found, None);
    }
}
