//! OpenAI 兼容的 API 反代（把 Switch 管理的账号包装成 `/v1/chat/completions`）。
//!
//! 设计要点：
//! - 与 Switch 共用同一份账号库（`accounts.json`）与同一套刷新逻辑，
//!   所以不存在「两个进程各自刷新同一个 refresh token」的问题；
//! - 出站强制 `stream: true`（与上游一致），客户端要非流式时在本地聚合；
//! - 账号选择：健康账号轮转，失败的号冷却一段时间后自动回到池子。

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::account::{load_accounts, upsert_account};
use super::config::{home_dir, now_ms};
use super::edition::{edition_of, Edition};
use super::refresh::refresh_account_token;

const CONFIG_FILE: &str = "proxy.json";
const DEFAULT_CLIENT_VERSION: &str = "5.5.4";
const DEFAULT_CLI_VERSION: &str = "2.137.1";
/// token 剩余有效期小于该值时先刷新再出站。
const REFRESH_MARGIN_MS: i64 = 2 * 60 * 60 * 1000;
/// 单账号失败后的冷却时间。
const COOLDOWN: Duration = Duration::from_secs(120);
/// 单次请求体上限。
///
/// axum 默认只给 2 MB（`DEFAULT_LIMIT`），长上下文会话（几十万 token）序列化后
/// 很容易超过，会被直接 413 掉、且报错信息里看不到原因。这里显式放宽到 64 MB：
/// 足以容纳百万级 token 的上下文，同时仍留一个防止内存被打爆的上界。
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------- 配置

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_listen")]
    pub listen: String,
    /// 留空 = 不鉴权（仅本机监听时才建议留空）。
    #[serde(default)]
    pub api_key: String,
    /// 允许出站的 uid；留空 = 全部账号。
    #[serde(default)]
    pub accounts: Vec<String>,
}

fn default_listen() -> String {
    "127.0.0.1:7863".to_string()
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: default_listen(),
            api_key: String::new(),
            accounts: Vec::new(),
        }
    }
}

fn config_path() -> std::path::PathBuf {
    home_dir().join(".wb-switch").join(CONFIG_FILE)
}

pub fn load_proxy_config() -> ProxyConfig {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return ProxyConfig::default();
    };
    match serde_json::from_str::<ProxyConfig>(&text) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("[反代] {path:?} 解析失败({err})，使用默认配置");
            ProxyConfig::default()
        }
    }
}

pub fn save_proxy_config(cfg: &ProxyConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- 账号池

struct AccountLease {
    uid: String,
    /// 这个号属于哪一档。国内版和国际版的模型列表不同，
    /// 拿国际版专属的 model 去打国内版网关会被拒，所以选号必须认版本。
    edition: Edition,
    failed_at: Option<Instant>,
}

struct Pool {
    leases: Vec<std::sync::Mutex<AccountLease>>,
    cursor: AtomicUsize,
}

impl Pool {
    fn new(entries: Vec<(String, Edition)>) -> Self {
        Self {
            leases: entries
                .into_iter()
                .map(|(uid, edition)| {
                    std::sync::Mutex::new(AccountLease {
                        uid,
                        edition,
                        failed_at: None,
                    })
                })
                .collect(),
            cursor: AtomicUsize::new(0),
        }
    }

    /// 同上，但可以只在某一档里取号（`want = None` 表示不限版本）。
    ///
    /// 一个版本都没有可用号时**不会**悄悄换另一个版本 —— 那样只会把
    /// 「模型不存在」的 400 换个地方报出来，不如直接说清楚。
    fn next_uid_of(&self, want: Option<Edition>) -> Option<String> {
        if self.leases.is_empty() {
            return None;
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        for i in 0..self.leases.len() {
            let idx = (start + i) % self.leases.len();
            let Ok(lease) = self.leases[idx].lock() else {
                continue;
            };
            if let Some(e) = want {
                if lease.edition != e {
                    continue;
                }
            }
            if lease
                .failed_at
                .map(|t| t.elapsed() < COOLDOWN)
                .unwrap_or(false)
            {
                continue;
            }
            return Some(lease.uid.clone());
        }
        None
    }

    /// 某一档里还有没有号（用于给出更准确的报错）。
    fn has_edition(&self, edition: Edition) -> bool {
        self.leases.iter().any(|slot| {
            slot.lock()
                .map(|l| l.edition == edition)
                .unwrap_or(false)
        })
    }

    fn mark_failed(&self, uid: &str) {
        for slot in &self.leases {
            let Ok(mut lease) = slot.lock() else { continue };
            if lease.uid == uid {
                lease.failed_at = Some(Instant::now());
            }
        }
    }
}

fn now_ms_i64() -> i64 {
    now_ms() as i64
}

/// 取一个可用的账号（必要时先刷新 token）。
async fn take_account(pool: &Pool, _cfg: &ProxyConfig, want: Option<Edition>) -> Option<Value> {
    let uid = pool.next_uid_of(want)?;
    let mut acc = load_accounts().into_iter().find(|a| {
        a.get("uid").and_then(Value::as_str) == Some(uid.as_str())
            && a.get("needs_relogin").and_then(Value::as_bool) != Some(true)
    })?;

    let exp = acc.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    if exp - now_ms_i64() < REFRESH_MARGIN_MS {
        match tokio::time::timeout(
            Duration::from_secs(30),
            refresh_account_token(acc.clone()),
        )
        .await
        {
            Ok(refreshed) => {
                if refreshed.get("needs_relogin").and_then(Value::as_bool) == Some(true) {
                    pool.mark_failed(&uid);
                    return None;
                }
                acc = refreshed;
            }
            Err(_) => {
                eprintln!("[反代] 刷新超时 uid={uid}");
                pool.mark_failed(&uid);
                return None;
            }
        }
    }
    let _ = upsert_account(&acc);
    Some(acc)
}

// ---------------------------------------------------------------- 上游

fn chat_url(acc: &Value) -> String {
    format!("{}/v2/chat/completions", upstream_endpoint(acc))
}

fn models_url(acc: &Value) -> String {
    format!("{}/v2/enterprises/personal/models", upstream_endpoint(acc))
}

/// 模型 id → 属于哪一档。由「拉模型列表」这条路径写入。
///
/// 反代池里国内版和国际版的号是混着的，而**两版的模型列表不一样**：
/// 拿国际版专属的 model 去打国内版网关会被拒。所以请求进来先按 model 查这张表，
/// 命中就只在该版本的号里选；没命中（还没拉过列表）就退回原来的轮转行为。
fn model_routes() -> &'static Mutex<HashMap<String, Edition>> {
    static ROUTES: std::sync::OnceLock<Mutex<HashMap<String, Edition>>> =
        std::sync::OnceLock::new();
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 记下「这些模型属于这一档」。拉列表成功时调用。
fn remember_model_routes(edition: Edition, ids: &[String]) {
    if let Ok(mut m) = model_routes().lock() {
        for id in ids {
            m.insert(id.clone(), edition);
        }
    }
}

/// 查某个 model 属于哪一档。
pub fn model_edition(model: &str) -> Option<Edition> {
    model_routes().lock().ok().and_then(|m| m.get(model).copied())
}

/// 解析版本标识（前端传 `domestic` / `international`）。
pub fn parse_edition_key(s: &str) -> Option<Edition> {
    match s.trim().to_ascii_lowercase().as_str() {
        "domestic" | "cn" | "国内版" => Some(Edition::Domestic),
        "international" | "intl" | "ai" | "国际版" => Some(Edition::International),
        _ => None,
    }
}

/// `/v1/models` 里 `owned_by` 的取值：让客户端一眼看出这个模型是哪一档的。
fn owner_tag(edition: Edition) -> &'static str {
    match edition {
        Edition::Domestic => "workbuddy-cn",
        Edition::International => "workbuddy-intl",
    }
}

/// 该账号所在档位的上游基址。
///
/// **国际版必须用 `https://www.workbuddy.ai`**——把国际版 token 发到
/// `codebuddy.cn` 会被网关拒掉。对照上游 `WbVariant::api_endpoint`。
fn upstream_endpoint(acc: &Value) -> &'static str {
    edition_of(acc).api_endpoint()
}

fn hex_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:032x}", nanos ^ 0x5eed_5eed_5eed_5eedu128)
}

/// 组装出站头族（对齐官方桌面端，缺字段用 X-No-* 约定）。
fn chat_headers(acc: &Value) -> HashMap<String, String> {
    let mut h = HashMap::new();
    let uid = acc.get("uid").and_then(Value::as_str).unwrap_or("");
    let at = acc.get("access_token").and_then(Value::as_str).unwrap_or("");
    let domain = acc.get("domain").and_then(Value::as_str).unwrap_or("");

    h.insert("Content-Type".into(), "application/json".into());
    h.insert(
        "Accept".into(),
        "application/json, text/event-stream".into(),
    );
    h.insert("X-Requested-With".into(), "XMLHttpRequest".into());
    let endpoint = upstream_endpoint(acc);
    h.insert("Origin".into(), endpoint.into());
    h.insert("Referer".into(), format!("{endpoint}/"));
    h.insert(
        "User-Agent".into(),
        format!(
            "WorkBuddy/{DEFAULT_CLIENT_VERSION} WorkBuddy/{DEFAULT_CLIENT_VERSION} CLI/{DEFAULT_CLI_VERSION}"
        ),
    );
    h.insert("X-CodeBuddy-Request".into(), "1".into());
    h.insert("Accept-Language".into(), "zh-CN".into());

    if !at.is_empty() {
        h.insert("Authorization".into(), format!("Bearer {at}"));
    } else {
        h.insert("X-No-Authorization".into(), "1".into());
    }
    if !uid.is_empty() {
        h.insert("X-User-Id".into(), uid.into());
    } else {
        h.insert("X-No-User-Id".into(), "1".into());
    }
    if !domain.is_empty() {
        h.insert("X-Domain".into(), domain.into());
    } else {
        h.insert("X-No-Department-Info".into(), "1".into());
    }
    match acc.get("enterpriseId").and_then(Value::as_str) {
        Some(v) if !v.is_empty() => {
            h.insert("X-Enterprise-Id".into(), v.into());
        }
        _ => {
            h.insert("X-No-Enterprise-Id".into(), "1".into());
        }
    }
    // 用量归属：伪装成桌面端会话，避免上游统计里 client 为空。
    h.insert("X-Agent-Purpose".into(), "conversation".into());
    h.insert("X-IDE-Name".into(), "WorkBuddy".into());
    h.insert("X-IDE-Type".into(), "desktop".into());
    h.insert("X-Product".into(), "WorkBuddy".into());

    // 会话头族（一次对话轮复用同一个聚合主键）。
    let conv_req = hex_id();
    let msg = hex_id();
    h.insert("X-Conversation-Request-ID".into(), conv_req.clone());
    h.insert("X-Conversation-Message-ID".into(), msg.clone());
    h.insert("X-Request-ID".into(), msg.clone());
    h.insert("X-Root-Request-ID".into(), conv_req.clone());
    h.insert("X-Trace-ID".into(), conv_req.clone());
    h.insert("X-B3-TraceId".into(), conv_req);
    h.insert("X-B3-SpanId".into(), msg[..16].to_string());
    h.insert("X-B3-Sampled".into(), "1".into());
    h
}

/// 上游网关只认少数几种内容块类型。客户端（zcode / Cursor / 各类 Agent）常发
/// OpenAI 较新的 `type:"file"`、Responses 的 `input_file`、Anthropic 的 `document`，
/// 原样透传会被上游直接 400，且报错看不出是哪来的：
/// `{"code":11101,"msg":"Parse message failed: unsupported content type at index 0: file"}`
///
/// 这里把它们降级成文本：能解出文本就内联，否则留一行占位说明。
/// 降级记录进 `degraded`，最后随响应头 `x-wb-switch-degraded` 回到客户端。
fn sanitize_messages(messages: &Value, degraded: &mut Vec<String>) -> Value {
    let Some(arr) = messages.as_array() else {
        return messages.clone();
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let mut m = item.clone();
        let Some(parts) = m.get("content").and_then(Value::as_array) else {
            // content 是字符串（最常见）或压根没有：原样放行。
            out.push(m);
            continue;
        };
        let mut kept: Vec<Value> = Vec::with_capacity(parts.len());
        for p in parts {
            match content_part(p, degraded) {
                Some(text) => kept.push(json!({ "type": "text", "text": text })),
                None => kept.push(p.clone()),
            }
        }
        if kept.is_empty() {
            kept.push(json!({ "type": "text", "text": "[内容块已省略：上游不支持该类型]" }));
        }
        m["content"] = Value::Array(kept);
        out.push(m);
    }
    Value::Array(out)
}

/// 单个内容块：`None` = 原样保留，`Some(text)` = 降级成文本块。
fn content_part(part: &Value, degraded: &mut Vec<String>) -> Option<String> {
    let ty = part.get("type").and_then(Value::as_str).unwrap_or("text");
    match ty {
        // 上游认识的原生块。
        "text" | "input_text" | "image_url" | "input_image" => None,
        // 附件类：能解出文本就内联，否则占位。
        "file" | "input_file" | "file_url" | "document" | "image_file" | "input_audio"
        | "audio_url" => {
            degraded.push(ty.to_string());
            Some(attachment_note(ty, part))
        }
        // 其余未知类型一律降级：宁可少一块内容，也不要整条请求被 400。
        _ => {
            degraded.push(ty.to_string());
            Some(format!("[{ty} 内容块已省略：上游不支持该类型]"))
        }
    }
}

/// 附件块的占位/内联文本。
fn attachment_note(ty: &str, part: &Value) -> String {
    let obj = part
        .get("file")
        .or_else(|| part.get("document"))
        .or_else(|| part.get("image_file"))
        .or_else(|| part.get("input_file"))
        .unwrap_or(part);
    let name = obj
        .get("filename")
        .or_else(|| obj.get("name"))
        .or_else(|| obj.get("file_id"))
        .and_then(Value::as_str)
        .unwrap_or("未命名附件");
    let data = obj
        .get("file_data")
        .or_else(|| obj.get("data"))
        .or_else(|| obj.get("base64"))
        .and_then(Value::as_str);
    match data.and_then(decode_text_data) {
        Some(text) => format!("[附件 {name}]\n{text}"),
        None => format!("[{ty} 附件 {name} 已省略：上游不支持该内容类型，仅文本类附件会内联]"),
    }
}

/// 只在能确定是文本时才解码：二进制（PDF/图片/音频）一律不解 ——
/// 把几十 KB 的 base64 灌进上下文比丢掉这块内容更糟。
fn decode_text_data(data: &str) -> Option<String> {
    use base64::Engine;
    /// 单个附件最多内联这么多字符。
    const MAX: usize = 64 * 1024;

    let Some(rest) = data.strip_prefix("data:") else {
        // 没有 data URL 前缀 = 客户端已经给了明文。
        return Some(clamp(data));
    };
    let (meta, b64) = rest.split_once(";base64,")?;
    let mime = meta.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let textual = mime.starts_with("text/")
        || mime.contains("json")
        || mime.contains("xml")
        || mime.contains("javascript")
        || mime.contains("yaml")
        || mime.contains("csv")
        || mime.contains("markdown");
    if !textual {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    if bytes.len() <= MAX {
        return Some(String::from_utf8_lossy(&bytes).to_string());
    }
    let mut t = String::from_utf8_lossy(&bytes[..MAX]).to_string();
    t.push_str("\n…（附件过长，已截断）");
    Some(t)
}

fn clamp(s: &str) -> String {
    const MAX: usize = 64 * 1024;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut t = s[..cut_at(s, MAX)].to_string();
    t.push_str("\n…（附件过长，已截断）");
    t
}

/// 按 UTF-8 边界往回退，避免切碎多字节字符。
fn cut_at(s: &str, max: usize) -> usize {
    let mut i = max.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// 降级记录的摘要，用作响应头（必须 ASCII）。
fn degraded_summary(degraded: &[String]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for t in degraded {
        match counts.iter_mut().find(|(k, _)| k == t) {
            Some((_, n)) => *n += 1,
            None => counts.push((t.clone(), 1)),
        }
    }
    counts
        .iter()
        .map(|(k, n)| format!("{k}={n}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// 给响应挂上降级摘要头（没降级就不挂）。
fn with_degraded_header(mut resp: Response, degraded: &[String]) -> Response {
    if degraded.is_empty() {
        return resp;
    }
    if let Ok(v) = HeaderValue::from_str(&degraded_summary(degraded)) {
        resp.headers_mut().insert("x-wb-switch-degraded", v);
    }
    resp
}

/// 把 OpenAI 入站体改写成上游形态（强制流式、去掉上游不认识的字段）。
fn build_upstream_body(incoming: &Value, degraded: &mut Vec<String>) -> Value {
    let model = incoming
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("deepseek-v4-flash")
        .to_string();
    let messages = incoming.get("messages").cloned().unwrap_or_else(|| json!([]));
    let mut body = json!({
        "model": model,
        "messages": sanitize_messages(&messages, degraded),
        "stream": true,
    });
    if let Some(t) = incoming.get("temperature") {
        body["temperature"] = t.clone();
    }
    if let Some(t) = incoming.get("top_p") {
        body["top_p"] = t.clone();
    }
    if let Some(m) = incoming.get("max_tokens") {
        body["max_tokens"] = m.clone();
    }
    if let Some(t) = incoming.get("tools") {
        body["tools"] = t.clone();
    }
    body
}

// ---------------------------------------------------------------- 处理

#[derive(Clone)]
struct AppState {
    cfg: Arc<ProxyConfig>,
    pool: Arc<Pool>,
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": "invalid api key"}}))).into_response()
}

fn check_auth(headers: &HeaderMap, cfg: &ProxyConfig) -> bool {
    if cfg.api_key.trim().is_empty() {
        return true;
    }
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .trim_start_matches("Bearer ")
        .trim();
    presented == cfg.api_key
}

async fn healthz(State(st): State<AppState>) -> impl IntoResponse {
    let total = st.pool.leases.len();
    let healthy = load_accounts()
        .iter()
        .filter(|a| a.get("needs_relogin").and_then(Value::as_bool) != Some(true))
        .count();
    Json(json!({"ok": true, "service": "wb-switch-proxy", "healthy": healthy, "total": total}))
}

/// 去重后追加到模型 id 列表。
fn push_unique(ids: &mut Vec<String>, id: &str) {
    if !id.is_empty() && !ids.iter().any(|x| x == id) {
        ids.push(id.to_string());
    }
}

/// 上游模型列表的兜底（拉不到时给一组可用的，免得客户端因为空列表直接报错）。
/// 从上游响应里抠出模型 id。
/// 上游结构：`{ code, msg, data: { agents: [ { name, models: [...] } ] } }`，
/// 也可能直接是 `{ data: [...] }` 或裸数组，这里都兜住。
fn parse_model_ids_raw(v: &Value) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    if let Some(agents) = v.pointer("/data/agents").and_then(Value::as_array) {
        for a in agents {
            if let Some(models) = a.get("models").and_then(Value::as_array) {
                for m in models {
                    if let Some(s) = m.as_str() {
                        push_unique(&mut ids, s);
                    }
                }
            }
        }
    }
    if ids.is_empty() {
        let items = match v.get("data").cloned() {
            Some(Value::Array(arr)) => arr,
            _ => v.as_array().cloned().unwrap_or_default(),
        };
        for m in items {
            let id = m
                .get("id")
                .or_else(|| m.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            push_unique(&mut ids, id);
        }
    }
    ids
}

fn cut_text(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 把两档的模型列表合成 OpenAI 形状：两档都有的标 `workbuddy`，
/// 只在国内版有的标 `workbuddy-cn`，只在国际版有的标 `workbuddy-intl`。
fn merge_edition_models(dom: &[String], intl: &[String]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for id in dom {
        let both = intl.iter().any(|x| x == id);
        out.push(json!({
            "id": id,
            "object": "model",
            "owned_by": if both { "workbuddy" } else { owner_tag(Edition::Domestic) },
        }));
    }
    for id in intl {
        if dom.iter().any(|x| x == id) {
            continue;
        }
        out.push(json!({
            "id": id,
            "object": "model",
            "owned_by": owner_tag(Edition::International),
        }));
    }
    out
}

/// `GET /v1/models` —— **两档各拉一次再合并**。
///
/// 以前只从轮转到的那个号上拉，于是列表是哪一档全看运气；现在按档分别拉，
/// `owned_by` 会写清楚来源（`workbuddy-cn` / `workbuddy-intl` / 两档都有 = `workbuddy`）。
async fn list_models(State(st): State<AppState>) -> Response {
    let (dom, intl) = tokio::join!(
        fetch_edition_models(&st.cfg, Edition::Domestic),
        fetch_edition_models(&st.cfg, Edition::International),
    );

    let mut errors: Vec<String> = Vec::new();
    let mut dom_ids: Vec<String> = Vec::new();
    let mut intl_ids: Vec<String> = Vec::new();
    match dom {
        Ok((ids, _, _)) => dom_ids = ids,
        Err(e) => errors.push(format!("{}：{e}", Edition::Domestic.label())),
    }
    match intl {
        Ok((ids, _, _)) => intl_ids = ids,
        Err(e) => errors.push(format!("{}：{e}", Edition::International.label())),
    }

    if dom_ids.is_empty() && intl_ids.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {"message": format!("两档都没拉到模型列表 —— {}", errors.join("；"))}})),
        )
            .into_response();
    }

    Json(json!({"object": "list", "data": merge_edition_models(&dom_ids, &intl_ids)})).into_response()
}

// ---------------------------------------------------------------- 取模型列表（桌面端按钮用）

/// 从账号库里挑一个可用账号（必要时先刷新 token）。不参与轮转、不动失败冷却。
async fn pick_account_of(cfg: &ProxyConfig, edition: Option<Edition>) -> Option<Value> {
    let want = cfg.accounts.clone();
    let acc = load_accounts().into_iter().find(|a| {
        a.get("needs_relogin").and_then(Value::as_bool) != Some(true)
            && edition.map(|e| edition_of(a) == e).unwrap_or(true)
            && (want.is_empty()
                || a.get("uid")
                    .and_then(Value::as_str)
                    .map(|u| want.iter().any(|w| w == u))
                    .unwrap_or(false))
    })?;
    let exp = acc.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    if exp - now_ms_i64() >= REFRESH_MARGIN_MS {
        return Some(acc);
    }
    match tokio::time::timeout(Duration::from_secs(30), refresh_account_token(acc.clone())).await {
        Ok(refreshed) if refreshed.get("needs_relogin").and_then(Value::as_bool) != Some(true) => {
            let _ = upsert_account(&refreshed);
            Some(refreshed)
        }
        Ok(_) => None,
        Err(_) => {
            eprintln!("[反代] 取模型列表前刷新超时");
            None
        }
    }
}

/// 拉某一档的模型列表（用**该档自己的账号**）。
///
/// 返回 `(ids, 来源说明, 来源 URL)`；成功时顺手把「模型 → 版本」写进路由表，
/// 之后 `/v1/chat/completions` 就能按 model 挑对版本。
async fn fetch_edition_models(
    cfg: &ProxyConfig,
    edition: Edition,
) -> Result<(Vec<String>, String, String), String> {
    let acc = pick_account_of(cfg, Some(edition)).await.ok_or_else(|| {
        format!(
            "没有可用的{}账号（需要重新登录、不在参与名单里，或账号库里就是没有这一档）",
            edition.label()
        )
    })?;
    let url = models_url(&acc);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.get(&url);
    for (k, v) in chat_headers(&acc) {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| format!("请求上游失败：{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("上游返回 {status}：{}", cut_text(&text, 160)));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("上游返回不是 JSON：{e}"))?;
    let ids = parse_model_ids_raw(&v);
    if ids.is_empty() {
        return Err("上游没有返回任何模型".to_string());
    }
    remember_model_routes(edition, &ids);
    Ok((ids, "上游".to_string(), url))
}

/// 「获取模型ID」：按版本分别拉。
///
/// `edition` 传 `"domestic"` / `"international"` 只拉那一档；不传则**两档都拉**。
/// 返回值里 `editions[]` 是分档明细（含各自的失败原因），
/// 另外保留 `models` / `source` / `sourceUrl` 三个扁平字段（= 第一档成功的那份），
/// 老前端不改也能用。
pub async fn fetch_models(edition: Option<String>) -> Result<Value, String> {
    let wanted: Option<Edition> = match edition.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => Some(parse_edition_key(s).ok_or_else(|| {
            format!("未知版本：{s}（可选 domestic / international）")
        })?),
    };

    let cfg = load_proxy_config();
    let list: Vec<Edition> = match wanted {
        Some(e) => vec![e],
        None => Edition::ALL.to_vec(),
    };

    // 两档并行拉，别串行等。
    let results = futures::future::join_all(
        list.iter()
            .map(|e| fetch_edition_models(&cfg, *e))
            .collect::<Vec<_>>(),
    )
    .await;

    let mut editions: Vec<Value> = Vec::new();
    let mut flat_models: Vec<String> = Vec::new();
    let mut flat_source = String::new();
    let mut flat_url = String::new();

    for (e, res) in list.iter().zip(results) {
        match res {
            Ok((ids, source, url)) => {
                if flat_models.is_empty() {
                    flat_models = ids.clone();
                    flat_source = source.clone();
                    flat_url = url.clone();
                }
                editions.push(json!({
                    "edition": e.key(),
                    "label": e.label(),
                    "models": ids,
                    "source": source,
                    "sourceUrl": url,
                    "error": Value::Null,
                }));
            }
            Err(err) => editions.push(json!({
                "edition": e.key(),
                "label": e.label(),
                "models": [],
                "source": "",
                "sourceUrl": "",
                "error": err,
            })),
        }
    }

    if flat_models.is_empty() {
        let why = editions
            .iter()
            .filter_map(|x| x["error"].as_str())
            .collect::<Vec<_>>()
            .join("；");
        return Err(format!("两档都没拉到模型列表 —— {why}"));
    }

    Ok(json!({
        "editions": editions,
        "models": flat_models,
        "source": flat_source,
        "sourceUrl": flat_url,
    }))
}

// ---------------------------------------------------------------- 用量统计

/// 一次出站请求的元信息（用于统计归类）。
#[derive(Clone)]
struct UsageMeta {
    uid: String,
    nickname: String,
    model: String,
    stream: bool,
}

const USAGE_FILE: &str = "proxy-usage.json";
/// recent 列表最多保留多少条。
const USAGE_RECENT_MAX: usize = 100;

fn usage_path() -> std::path::PathBuf {
    home_dir().join(".wb-switch").join(USAGE_FILE)
}

static USAGE_LOCK: Mutex<()> = Mutex::new(());

fn empty_usage() -> Value {
    json!({
        "updatedAt": 0,
        "total": {"requests": 0, "errors": 0, "promptTokens": 0,
                  "completionTokens": 0, "credit": 0},
        "byAccount": {},
        "byModel": {},
        "recent": [],
    })
}

/// 读统计文件；损坏或不存在就返回空结构。
pub fn load_usage() -> Value {
    let p = usage_path();
    if !p.is_file() {
        return empty_usage();
    }
    match std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        Some(v) if v.get("total").is_some() => v,
        _ => empty_usage(),
    }
}

/// 清空统计。
pub fn reset_usage() -> Result<(), String> {
    let _g = USAGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut doc = empty_usage();
    doc["updatedAt"] = json!(now_ms());
    std::fs::write(
        usage_path(),
        serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn usage_num(usage: Option<&Value>, key: &str) -> i64 {
    usage
        .and_then(|u| u.get(key))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

fn usage_credit(usage: Option<&Value>) -> f64 {
    usage
        .and_then(|u| u.get("credit"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

/// 从上游 SSE 原文里取最后一个 usage 块。
fn usage_from_sse(text: &str) -> Option<Value> {
    let mut last: Option<Value> = None;
    for line in text.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if let Some(u) = v.get("usage") {
            if !u.is_null() {
                last = Some(u.clone());
            }
        }
    }
    last
}

/// 累加一次请求到统计文件（请求量很小，直接读改写即可）。
fn record_usage(meta: &UsageMeta, usage: Option<&Value>, ok: bool) {
    let prompt = usage_num(usage, "prompt_tokens");
    let completion = usage_num(usage, "completion_tokens");
    let credit = usage_credit(usage);

    let _g = USAGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut doc = load_usage();

    let bump = |obj: &mut Value, prompt: i64, completion: i64, credit: f64, ok: bool| {
        if !obj.is_object() {
            *obj = json!({});
        }
        let o = obj.as_object_mut().unwrap();
        *o.entry("requests").or_insert(json!(0)) =
            json!(o.get("requests").and_then(Value::as_i64).unwrap_or(0) + 1);
        if !ok {
            *o.entry("errors").or_insert(json!(0)) =
                json!(o.get("errors").and_then(Value::as_i64).unwrap_or(0) + 1);
        }
        *o.entry("promptTokens").or_insert(json!(0)) =
            json!(o.get("promptTokens").and_then(Value::as_i64).unwrap_or(0) + prompt);
        *o.entry("completionTokens").or_insert(json!(0)) =
            json!(o.get("completionTokens").and_then(Value::as_i64).unwrap_or(0) + completion);
        *o.entry("credit").or_insert(json!(0.0)) =
            json!(o.get("credit").and_then(Value::as_f64).unwrap_or(0.0) + credit);
    };

    bump(&mut doc["total"], prompt, completion, credit, ok);

    if let Some(m) = doc["byAccount"].as_object_mut() {
        let e = m.entry(meta.uid.clone()).or_insert_with(|| {
            json!({"nickname": meta.nickname, "requests": 0, "errors": 0,
                   "promptTokens": 0, "completionTokens": 0, "credit": 0})
        });
        e["nickname"] = json!(meta.nickname);
        bump(e, prompt, completion, credit, ok);
    }
    if let Some(m) = doc["byModel"].as_object_mut() {
        let e = m
            .entry(meta.model.clone())
            .or_insert_with(|| {
                json!({"requests": 0, "errors": 0, "promptTokens": 0,
                       "completionTokens": 0, "credit": 0})
            });
        bump(e, prompt, completion, credit, ok);
    }

    if let Some(arr) = doc["recent"].as_array_mut() {
        arr.insert(
            0,
            json!({
                "ts": now_ms(),
                "uid": meta.uid,
                "nickname": meta.nickname,
                "model": meta.model,
                "stream": meta.stream,
                "ok": ok,
                "promptTokens": prompt,
                "completionTokens": completion,
                "credit": credit,
            }),
        );
        arr.truncate(USAGE_RECENT_MAX);
    }

    doc["updatedAt"] = json!(now_ms());
    if let Ok(text) = serde_json::to_string_pretty(&doc) {
        let _ = std::fs::write(usage_path(), text);
    }
}

/// 边转发边累积 SSE 原文，流结束时把用量记进统计。
// ---------------------------------------------------------------- 流式重组
//
// 上游会把思考与正文拆成**非常碎**的小帧：实测 `glm-5.3-flash` 一段思考 750 帧、
// 每帧 1~5 个字符（形如 `'The'` `' user'` `' is'`），`kimi-k3-1` 同量级。
//
// 而部分客户端会把**每一帧都当成一个独立的思考段**（ZCode 就是如此，它内部用
// Vercel AI SDK），于是界面上出现几十上百个「思考」小块、每块只有一两个词；
// 正文却正常，因为正文走的是另一套会累积合并的逻辑。
//
// 所以这里不再原样透传，改成有状态的重组：
//   * 同一字段累积到阈值才发一帧；字段切换、流结束、收到末帧时强制 flush；
//   * 顺手规范化上游不合规的地方：中间帧 finish_reason 用 null（上游给的是 `""`）、
//     重组帧不带空字符串字段，role 只在首帧出现；
//   * 🔴 **工具调用帧（`tool_calls` / `function_call` 非空）不参与合并，原样透传** ——
//     它们的 delta 里既没有 reasoning 也没有 content，走合并分支会被整帧吞掉，
//     客户端就永远收不到工具调用（agent 功能全废）。
// 用量统计仍按原有方式在流结束时解析整段文本。
//
// 阈值为什么两个不一样：拿真实抓包跑过（glm-5.3-flash 一段思考 1082 帧、思考合计 2933 字），
// 阈值 24 时思考仍会拆成 112 帧 —— 客户端按帧开块的话就是 112 个方块，等于没修。
// 实测（思考阈值 → 思考帧数）：24→112、96→30、160→19、240→13、400→8。
// 正文必须保持小阈值以维持逐字出字的观感；思考块在被折叠的情况下大一点反而更好读。
// 若你的客户端仍碎得厉害，把 MERGE_MIN_REASONING 调大即可（调到 usize::MAX 就是整段一次出）。
const MERGE_MIN_REASONING: usize = 256;
const MERGE_MIN_CONTENT: usize = 32;

/// 取 `choices[0].delta.<key>` 的字符串（缺失或 null 都当空串）。
fn delta_str(frame: &Value, key: &str) -> String {
    frame
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 取 `choices[0].finish_reason`（缺失或 null 都当空串）。
fn finish_str(frame: &Value) -> String {
    frame
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// `delta` 里是否带**实际内容**的工具调用（`tool_calls` 非空数组，或 `function_call` 非空对象）。
///
/// 🔴 这类帧必须**原样透传**，绝不能进重组逻辑：它们的 delta 里既没有 `reasoning_content`
/// 也没有 `content`，会被"只处理这两种字段"的合并分支整帧吞掉 ——
/// 结果是客户端收到 `finish_reason: "tool_calls"` 却拿不到任何工具数据，agent 功能全废。
fn has_tool_delta(frame: &Value) -> bool {
    let Some(delta) = frame
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("delta"))
    else {
        return false;
    };
    if let Some(tc) = delta.get("tool_calls") {
        match tc {
            Value::Array(a) => return !a.is_empty(),
            Value::Null => {}
            _ => return true,
        }
    }
    if let Some(fc) = delta.get("function_call") {
        match fc {
            Value::Object(o) => return !o.is_empty(),
            Value::Null => {}
            _ => return true,
        }
    }
    false
}

/// 上游会把中间帧的 `finish_reason` 填成空字符串 `""`（实测 1081/1082 帧），规范成 `null`。
/// 只在**透传帧**上用；重组出来的帧由 `make_chunk` 直接写 `null`。
fn normalize_finish(frame: &mut Value) {
    let Some(choice) = frame
        .get_mut("choices")
        .and_then(|c| c.as_array_mut())
        .and_then(|a| a.first_mut())
        .and_then(|c| c.as_object_mut())
    else {
        return;
    };
    if matches!(choice.get("finish_reason"), Some(Value::String(s)) if s.is_empty()) {
        choice.insert("finish_reason".to_string(), Value::Null);
    }
}

/// 以上游首帧为模板，重造一帧规范的 OpenAI 流式 chunk。
fn make_chunk(template: &Value, reasoning: &str, content: &str, with_role: bool) -> Value {
    let mut frame = template.clone();
    if let Some(obj) = frame.as_object_mut() {
        obj.insert("usage".to_string(), Value::Null);
        if !obj.contains_key("choices") {
            obj.insert("choices".to_string(), json!([{}]));
        }
        if let Some(cv) = obj.get_mut("choices") {
            if let Some(choices) = cv.as_array_mut() {
                if choices.is_empty() {
                    choices.push(json!({}));
                }
                if let Some(choice) = choices[0].as_object_mut() {
                    choice.insert("finish_reason".to_string(), Value::Null);
                    choice.insert("logprobs".to_string(), Value::Null);
                    let mut delta = serde_json::Map::new();
                    if with_role {
                        delta.insert("role".to_string(), json!("assistant"));
                    }
                    // 只在非空时插入字段：某些客户端按「delta 里出现了 reasoning_content 键」
                    // 来判断是否开启新的思考块，若正文帧也携带空串，会凭空多出一堆空思考块。
                    if !reasoning.is_empty() {
                        delta.insert("reasoning_content".to_string(), json!(reasoning));
                    }
                    if !content.is_empty() {
                        delta.insert("content".to_string(), json!(content));
                    }
                    choice.insert("delta".to_string(), Value::Object(delta));
                }
            }
        }
    }
    frame
}

fn sse_bytes(frame: &Value) -> Option<bytes::Bytes> {
    let text = serde_json::to_string(frame).ok()?;
    Some(bytes::Bytes::from(format!("data: {text}\n\n")))
}

/// 把累积内容合成一帧发出去；返回 None 表示当前没有可发的累积。
fn take_flush(
    template: Option<&Value>,
    field: &str,
    acc: &mut String,
    role_done: &mut bool,
) -> Option<bytes::Bytes> {
    if acc.is_empty() {
        return None;
    }
    let t = template?;
    let (reasoning, content) = if field == "reasoning_content" {
        (acc.as_str(), "")
    } else {
        ("", acc.as_str())
    };
    let out = sse_bytes(&make_chunk(t, reasoning, content, !*role_done));
    *role_done = true;
    acc.clear();
    out
}

/// 读完整条上游流，边重组边推给客户端；结束时补末帧与 `[DONE]`，并记录用量。
async fn sse_pump<S>(
    mut upstream: S,
    tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    meta: UsageMeta,
) where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    let mut raw = String::new();
    let mut tail = String::new();
    let mut template: Option<Value> = None;
    let mut last_frame: Option<Value> = None;
    let mut role_done = false;
    let mut field = String::new();
    let mut acc = String::new();

    'outer: while let Some(item) = upstream.next().await {
        match item {
            Ok(chunk) => {
                let text = String::from_utf8_lossy(&chunk).to_string();
                raw.push_str(&text);
                tail.push_str(&text);
            }
            Err(e) => {
                record_usage(&meta, None, false);
                let _ = tx
                    .send(Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        e.to_string(),
                    )))
                    .await;
                return;
            }
        }

        // 按空行切出完整事件；不足一个事件的留在 tail 里等下一块
        while let Some(idx) = tail.find("\n\n") {
            let event = tail[..idx].to_string();
            tail.drain(..idx + 2);

            // SSE 规范允许一个事件里有多行 data:，拼接后才是完整 JSON
            let mut data = String::new();
            for line in event.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    data.push_str(rest.trim_start());
                }
            }
            if data.is_empty() || data.trim() == "[DONE]" {
                continue;
            }
            let Ok(frame) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            if template.is_none() {
                template = Some(frame.clone());
            }

            // 🔴 工具调用帧优先原样透传（必须在合并分支之前判断）。
            // 它们的 delta 里没有 reasoning_content / content，走下面的分支会被整帧吞掉，
            // 客户端只能收到 finish_reason="tool_calls" 却拿不到工具数据。
            if has_tool_delta(&frame) {
                if let Some(b) = take_flush(template.as_ref(), &field, &mut acc, &mut role_done) {
                    if tx.send(Ok(b)).await.is_err() {
                        break 'outer;
                    }
                }
                field.clear();
                let mut fwd = frame.clone();
                normalize_finish(&mut fwd);
                if let Some(b) = sse_bytes(&fwd) {
                    if tx.send(Ok(b)).await.is_err() {
                        break 'outer;
                    }
                }
                // 这帧已带着自己的 finish_reason 原样发出，不能再进 last_frame
                // （那里会把 delta 清空，工具调用就没了）。
                continue;
            }

            let reasoning = delta_str(&frame, "reasoning_content");
            let content = delta_str(&frame, "content");
            if !reasoning.is_empty() || !content.is_empty() {
                let (next_field, piece) = if !reasoning.is_empty() {
                    ("reasoning_content", reasoning)
                } else {
                    ("content", content)
                };
                if field != next_field {
                    if let Some(b) = take_flush(template.as_ref(), &field, &mut acc, &mut role_done) {
                        if tx.send(Ok(b)).await.is_err() {
                            break 'outer;
                        }
                    }
                    field = next_field.to_string();
                }
                acc.push_str(&piece);
                let limit = if field == "reasoning_content" {
                    MERGE_MIN_REASONING
                } else {
                    MERGE_MIN_CONTENT
                };
                if acc.chars().count() >= limit {
                    if let Some(b) = take_flush(template.as_ref(), &field, &mut acc, &mut role_done) {
                        if tx.send(Ok(b)).await.is_err() {
                            break 'outer;
                        }
                    }
                }
            }

            // 末帧要留着最后发：先把它手里的 delta 并入累积，避免丢结尾
            if !finish_str(&frame).is_empty() {
                last_frame = Some(frame);
            }
        }
    }

    if let Some(b) = take_flush(template.as_ref(), &field, &mut acc, &mut role_done) {
        let _ = tx.send(Ok(b)).await;
    }
    if let Some(mut frame) = last_frame {
        normalize_finish(&mut frame);
        // 末帧的 delta 已经并入上面的累积了，这里清空以免重复输出。
        // ⚠️ 但如果这帧本身带工具调用，就不能清 —— 那是唯一的数据来源。
        if !has_tool_delta(&frame) {
            if let Some(obj) = frame.as_object_mut() {
                if let Some(cv) = obj.get_mut("choices") {
                    if let Some(choices) = cv.as_array_mut() {
                        if let Some(choice) = choices.first_mut().and_then(|c| c.as_object_mut()) {
                            choice.insert("delta".to_string(), json!({}));
                        }
                    }
                }
            }
        }
        if let Some(b) = sse_bytes(&frame) {
            let _ = tx.send(Ok(b)).await;
        }
    }
    let _ = tx
        .send(Ok(bytes::Bytes::from_static(b"data: [DONE]\n\n")))
        .await;

    let usage = usage_from_sse(&raw);
    record_usage(&meta, usage.as_ref(), true);
}

async fn chat_completions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(incoming): Json<Value>,
) -> Response {
    if !check_auth(&headers, &st.cfg) {
        return unauthorized();
    }
    let want_stream = incoming
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let model = incoming
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("deepseek-v4-flash")
        .to_string();

    // 按 model 认版本：国内版和国际版的模型列表不一样，把国际版专属的 model
    // 打到国内版网关上会被拒。拉过一次模型列表之后这里就能命中。
    let route = model_edition(&model);
    let Some(acc) = take_account(&st.pool, &st.cfg, route).await else {
        let message = match route {
            Some(e) if st.pool.has_edition(e) => format!(
                "模型 `{model}` 属于{}，但该版本的账号当前都不可用（冷却中或需要重新登录）",
                e.label()
            ),
            Some(e) => format!(
                "模型 `{model}` 属于{}，但参与轮转的账号里没有{}的号",
                e.label(),
                e.label()
            ),
            None => "no available account (all cooling down or needs relogin)".to_string(),
        };
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": message}})),
        )
            .into_response();
    };
    let uid = acc
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let nickname = acc
        .get("nickname")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let meta = UsageMeta {
        uid: uid.clone(),
        nickname,
        model: model.clone(),
        stream: want_stream,
    };

    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let mut req = client.post(chat_url(&acc));
    for (k, v) in chat_headers(&acc) {
        req = req.header(k, v);
    }
    // 上游不认识的内容块（zcode 等客户端发的 file / document）在这里降级成文本，
    // 否则整条请求会被上游 400：`unsupported content type at index 0: file`。
    let mut degraded: Vec<String> = Vec::new();
    let upstream_body = build_upstream_body(&incoming, &mut degraded);
    if !degraded.is_empty() {
        eprintln!(
            "[反代] 内容块降级 {} (model={model})",
            degraded_summary(&degraded)
        );
    }
    let resp = match req.json(&upstream_body).send().await {
        Ok(r) => r,
        Err(e) => {
            st.pool.mark_failed(&uid);
            return (StatusCode::BAD_GATEWAY, e.to_string()).into_response();
        }
    };
    if !resp.status().is_success() {
        st.pool.mark_failed(&uid);
        record_usage(&meta, None, false);
        let status = StatusCode::from_u16(resp.status().as_u16())
            .unwrap_or(StatusCode::BAD_GATEWAY);
        let text = resp.text().await.unwrap_or_default();
        return (
            status,
            Json(json!({"error": {"message": text, "type": "upstream_error"}})),
        )
            .into_response();
    }

    if want_stream {
        // 不再原样透传：先过一层 SSE 重组器（见 sse_pump 的说明），
        // 否则上游「一两个词一帧」的粒度会让客户端把思考渲染成一片小方块。
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        // bytes_stream() 返回的 impl Stream 不保证 Unpin，必须 Box::pin 一下
        let upstream = Box::pin(resp.bytes_stream());
        tokio::spawn(async move {
            sse_pump(upstream, tx, meta).await;
        });
        let body = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream; charset=utf-8")
            .header("Cache-Control", "no-cache")
            .header("X-Accel-Buffering", "no")
            .body(axum::body::Body::from_stream(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        with_degraded_header(resp, &degraded)
    } else {
        let resp = match aggregate(Box::pin(resp.bytes_stream()), &model).await {
            Ok(json) => {
                let usage = json.get("usage").cloned();
                record_usage(&meta, usage.as_ref(), true);
                Json(json).into_response()
            }
            Err(e) => {
                record_usage(&meta, None, false);
                (StatusCode::BAD_GATEWAY, e).into_response()
            }
        };
        with_degraded_header(resp, &degraded)
    }
}

/// 把上游 SSE 聚合为一条非流式 OpenAI 响应。
async fn aggregate<S>(mut stream: S, model: &str) -> Result<Value, String>
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut id = String::new();
    let mut usage: Option<Value> = None;

    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim_end_matches('\r').to_string();
            buf = buf[pos + 1..].to_string();
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            if id.is_empty() {
                if let Some(s) = v.get("id").and_then(Value::as_str) {
                    id = s.to_string();
                }
            }
            if let Some(u) = v.get("usage") {
                usage = Some(u.clone());
            }
            if let Some(choices) = v.get("choices").and_then(Value::as_array) {
                if let Some(delta) = choices.first().and_then(|c| c.get("delta")) {
                    if let Some(s) = delta.get("content").and_then(Value::as_str) {
                        content.push_str(s);
                    }
                    if let Some(s) = delta.get("reasoning_content").and_then(Value::as_str) {
                        reasoning.push_str(s);
                    }
                }
            }
        }
    }

    let created = now_ms() / 1000;
    let id = if id.is_empty() { hex_id() } else { id };
    Ok(json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
                "reasoning_content": reasoning
            },
            "finish_reason": "stop"
        }],
        "usage": usage.unwrap_or_else(|| json!(null))
    }))
}

async fn proxy_status(State(st): State<AppState>) -> impl IntoResponse {
    let allowed = &st.cfg.accounts;
    let items: Vec<Value> = load_accounts()
        .into_iter()
        .filter(|a| {
            allowed.is_empty()
                || a.get("uid")
                    .and_then(Value::as_str)
                    .map(|u| allowed.iter().any(|x| x == u))
                    .unwrap_or(false)
        })
        .map(|a| {
            json!({
                "uid": a.get("uid").and_then(Value::as_str),
                "nickname": a.get("nickname").and_then(Value::as_str),
                "needs_relogin": a.get("needs_relogin").and_then(Value::as_bool).unwrap_or(false),
                "expiresAt": a.get("expiresAt").and_then(Value::as_i64),
            })
        })
        .collect();
    Json(json!({
        "enabled": st.cfg.enabled,
        "listen": st.cfg.listen,
        "accounts": items
    }))
}

async fn usage_stats() -> Response {
    Json(load_usage()).into_response()
}

// ---------------------------------------------------------------- 启动

fn router(st: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/status", get(proxy_status))
        .route("/usage-stats", get(usage_stats))
        // 必须在 with_state 之前挂：layer 只对「它之前注册的路由」生效。
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY))
        .with_state(st)
}

/// 启动反代服务（阻塞，直到监听失败或进程退出）。
pub async fn run_proxy_server(cfg: ProxyConfig) -> Result<(), String> {
    let addr: SocketAddr = cfg.listen.parse().map_err(|_| format!("监听地址无效: {}", cfg.listen))?;
    let accounts = eligible_accounts(&cfg);
    if accounts.is_empty() {
        return Err("没有可用账号：账号库为空或全部 needs_relogin".to_string());
    }
    let st = AppState {
        pool: Arc::new(Pool::new(accounts)),
        cfg: Arc::new(cfg),
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("监听 {addr} 失败: {e}"))?;
    println!("[反代] 已启动 http://{addr}  (POST /v1/chat/completions)");
    axum::serve(listener, router(st))
        .await
        .map_err(|e| e.to_string())
}

/// 同上，但收到 shutdown 信号后优雅退出。
async fn run_proxy_until(
    cfg: ProxyConfig,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let addr: SocketAddr = cfg.listen.parse().map_err(|_| format!("监听地址无效: {}", cfg.listen))?;
    let accounts = eligible_accounts(&cfg);
    if accounts.is_empty() {
        let msg = "没有可用账号：账号库为空或全部 needs_relogin".to_string();
        let _ = ready.send(Err(msg.clone()));
        return Err(msg);
    }
    let st = AppState {
        pool: Arc::new(Pool::new(accounts)),
        cfg: Arc::new(cfg),
    };
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            let msg = format!("监听 {addr} 失败: {e}");
            let _ = ready.send(Err(msg.clone()));
            return Err(msg);
        }
    };
    println!("[反代] 已启动 http://{addr}  (POST /v1/chat/completions)");
    let _ = ready.send(Ok(()));
    axum::serve(listener, router(st))
        .with_graceful_shutdown(async {
            let _ = shutdown.await;
        })
        .await
        .map_err(|e| e.to_string())
}

/// 按配置挑出参与反代的 uid。
fn eligible_accounts(cfg: &ProxyConfig) -> Vec<(String, Edition)> {
    load_accounts()
        .into_iter()
        .filter(|a| a.get("needs_relogin").and_then(Value::as_bool) != Some(true))
        .filter(|a| {
            cfg.accounts.is_empty()
                || a.get("uid")
                    .and_then(Value::as_str)
                    .map(|u| cfg.accounts.iter().any(|x| x == u))
                    .unwrap_or(false)
        })
        .filter_map(|a| {
            a.get("uid")
                .and_then(Value::as_str)
                .map(|u| (u.to_string(), edition_of(&a)))
        })
        .collect()
}

/// 供桌面端调用：读配置，enabled 则在后台起服务。
pub fn spawn_from_config() {
    let cfg = load_proxy_config();
    if !cfg.enabled {
        return;
    }
    if let Err(e) = start_proxy_server(cfg) {
        eprintln!("[反代] 启动失败: {e}");
    }
}

// ---------------------------------------------------------------- 生命周期
//
// 说明：Tauri 的 setup() 与同步 command 都跑在**主线程**，那里没有 Tokio 运行时，
// 直接 `tokio::spawn` 会 panic。所以这里自己开一条带独立运行时的线程，
// 无论从启动流程还是从 command 调用都安全。

static RUNNING: AtomicBool = AtomicBool::new(false);
static HANDLE: Mutex<Option<ServerHandle>> = Mutex::new(None);

struct ServerHandle {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// 反代是否正在监听。
pub fn proxy_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// 启动反代（非阻塞）。返回 Err 说明已在运行或地址无效。
pub fn start_proxy_server(cfg: ProxyConfig) -> Result<(), String> {
    if proxy_running() {
        return Err("反代已经在运行".to_string());
    }
    // 先探一次地址合法性，避免后台线程里默默失败。
    cfg.listen
        .parse::<SocketAddr>()
        .map_err(|_| format!("监听地址无效: {}", cfg.listen))?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let thread = std::thread::Builder::new()
        .name("wb-api-proxy".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("创建运行时失败: {e}")));
                    RUNNING.store(false, Ordering::SeqCst);
                    return;
                }
            };
            rt.block_on(async move {
                let outcome = run_proxy_until(cfg, shutdown_rx, ready_tx).await;
                if let Err(e) = &outcome {
                    eprintln!("[反代] 运行结束: {e}");
                }
            });
            RUNNING.store(false, Ordering::SeqCst);
            println!("[反代] 已停止");
        })
        .map_err(|e| format!("无法启动反代线程: {e}"))?;

    // 等一小会儿，确认是「已监听」还是「起不来」。
    let started = ready_rx.recv_timeout(Duration::from_secs(5));
    match started {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let _ = thread.join();
            RUNNING.store(false, Ordering::SeqCst);
            return Err(e);
        }
        Err(_) => {
            // 5 秒内没回音 = 正常监听中（run_proxy_until 不会提前返回）。
        }
    }

    RUNNING.store(true, Ordering::SeqCst);
    if let Ok(mut g) = HANDLE.lock() {
        *g = Some(ServerHandle {
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        });
    }
    Ok(())
}

/// 停止反代。
pub fn stop_proxy_server() {
    let handle = match HANDLE.lock() {
        Ok(mut g) => g.take(),
        Err(_) => None,
    };
    if let Some(mut h) = handle {
        if let Some(tx) = h.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(th) = h.thread.take() {
            let _ = th.join();
        }
    }
    RUNNING.store(false, Ordering::SeqCst);
}

/// 按当前配置重启（设置页保存时用）。
pub fn restart_proxy_server() -> Result<(), String> {
    stop_proxy_server();
    let cfg = load_proxy_config();
    if !cfg.enabled {
        return Ok(());
    }
    start_proxy_server(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版本标识解析：前端传的是 key，也容忍几个常见别名。
    #[test]
    fn edition_key_parsing() {
        assert_eq!(parse_edition_key("domestic"), Some(Edition::Domestic));
        assert_eq!(parse_edition_key(" INTERNATIONAL "), Some(Edition::International));
        assert_eq!(parse_edition_key("intl"), Some(Edition::International));
        assert_eq!(parse_edition_key("国内版"), Some(Edition::Domestic));
        assert_eq!(parse_edition_key("国际版"), Some(Edition::International));
        assert_eq!(parse_edition_key("nope"), None);
        assert_eq!(parse_edition_key(""), None);
    }

    /// 合并两档：两档都有 → workbuddy；只有一档有 → 各自的 tag。
    #[test]
    fn merging_two_editions_tags_ownership() {
        let dom = vec!["auto".to_string(), "deepseek-v4-pro".to_string()];
        let intl = vec!["auto".to_string(), "claude-sonnet".to_string()];
        let merged = merge_edition_models(&dom, &intl);
        let by: std::collections::HashMap<&str, &str> = merged
            .iter()
            .map(|m| {
                (
                    m["id"].as_str().unwrap(),
                    m["owned_by"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(by.get("auto"), Some(&"workbuddy"), "两档都有应标成通用的");
        assert_eq!(by.get("deepseek-v4-pro"), Some(&"workbuddy-cn"));
        assert_eq!(by.get("claude-sonnet"), Some(&"workbuddy-intl"));
        assert_eq!(merged.len(), 3, "同名的不能重复出现");
        // 国内版在前，保持稳定顺序
        assert_eq!(merged[0]["id"], "auto");
    }

    /// 只有一档有号时，另一档的模型不该被路由过去。
    #[test]
    fn model_routes_record_and_lookup() {
        remember_model_routes(
            Edition::International,
            &["intl-only-model".to_string()],
        );
        assert_eq!(
            model_edition("intl-only-model"),
            Some(Edition::International)
        );
        assert_eq!(model_edition("never-seen-model"), None);
    }

    /// 账号池按版本取号：只要国内版时不该给出国际版的号。
    #[test]
    fn pool_filters_by_edition() {
        let pool = Pool::new(vec![
            ("u-intl".to_string(), Edition::International),
            ("u-dom".to_string(), Edition::Domestic),
        ]);
        assert_eq!(pool.next_uid_of(Some(Edition::Domestic)).as_deref(), Some("u-dom"));
        assert_eq!(pool.next_uid_of(Some(Edition::International)).as_deref(), Some("u-intl"));
        assert!(pool.has_edition(Edition::Domestic));
        assert!(pool.has_edition(Edition::International));

        let only_intl = Pool::new(vec![("u-intl".to_string(), Edition::International)]);
        assert_eq!(only_intl.next_uid_of(Some(Edition::Domestic)), None);
        assert!(!only_intl.has_edition(Edition::Domestic));
        // 不限版本时照常能拿到
        assert_eq!(only_intl.next_uid_of(None).as_deref(), Some("u-intl"));
    }

    /// 复现线上的 400：客户端把附件当 `{"type":"file"}` 发过来，
    /// 上游回 `Parse message failed: unsupported content type at index 0: file`。
    #[test]
    fn file_block_is_downgraded_to_text() {
        let msgs = json!([{
            "role": "user",
            "content": [
                { "type": "file", "file": { "filename": "a.pdf", "file_data": "data:application/pdf;base64,JVBERi0=" } },
                { "type": "text", "text": "看看这个" }
            ]
        }]);
        let mut degraded = Vec::new();
        let out = sanitize_messages(&msgs, &mut degraded);

        let parts = out[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        let note = parts[0]["text"].as_str().unwrap();
        assert!(note.contains("a.pdf"), "{note}");
        assert!(note.contains("已省略"), "{note}");
        // 文本块原地保留（位置不动，索引不变）。
        assert_eq!(parts[1]["text"], "看看这个");
        assert_eq!(degraded_summary(&degraded), "file=1");
    }

    #[test]
    fn plain_string_content_is_untouched() {
        let msgs = json!([{ "role": "user", "content": "你好" }]);
        let mut degraded = Vec::new();
        let out = sanitize_messages(&msgs, &mut degraded);
        assert_eq!(out, msgs);
        assert!(degraded.is_empty());
    }

    #[test]
    fn textual_attachment_is_inlined() {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode("hello 世界".as_bytes());
        let msgs = json!([{
            "role": "user",
            "content": [{ "type": "file", "file": {
                "filename": "note.md",
                "file_data": format!("data:text/markdown;base64,{b64}")
            }}]
        }]);
        let mut degraded = Vec::new();
        let out = sanitize_messages(&msgs, &mut degraded);
        let text = out[0]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("hello 世界"), "{text}");
    }

    #[test]
    fn unknown_type_never_survives() {
        // 未知块必须被换掉，否则又是一个 400。
        let msgs = json!([{
            "role": "user",
            "content": [{ "type": "weird_new_thing", "foo": 1 }, { "type": "text", "text": "x" }]
        }]);
        let mut degraded = Vec::new();
        let out = sanitize_messages(&msgs, &mut degraded);
        let parts = out[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|p| p["type"] == "text"));
        assert_eq!(degraded_summary(&degraded), "weird_new_thing=1");
    }

    #[test]
    fn image_and_text_blocks_survive() {
        let msgs = json!([{
            "role": "user",
            "content": [
                { "type": "text", "text": "看图" },
                { "type": "image_url", "image_url": { "url": "https://x/y.png" } }
            ]
        }]);
        let mut degraded = Vec::new();
        let out = sanitize_messages(&msgs, &mut degraded);
        assert_eq!(out, msgs);
        assert!(degraded.is_empty());
    }

    #[test]
    fn upstream_body_keeps_params_and_sanitizes() {
        let incoming = json!({
            "model": "glm-5.3-flash",
            "temperature": 0.3,
            "messages": [{
                "role": "user",
                "content": [{ "type": "file", "file": { "file_id": "f_123" } }]
            }]
        });
        let mut degraded = Vec::new();
        let body = build_upstream_body(&incoming, &mut degraded);
        assert_eq!(body["model"], "glm-5.3-flash");
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0.3);
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert!(body["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("f_123"));
    }

    /// 上游 `data.agents[].models[]` 形态。
    #[test]
    fn parse_model_ids_reads_agents() {
        let v = json!({
            "code": 0,
            "data": { "agents": [
                { "name": "a", "models": ["glm-5.3", "kimi-k3-1"] },
                { "name": "b", "models": ["glm-5.3", "deepseek-v4-pro"] }
            ]}
        });
        // 去重且保序
        assert_eq!(parse_model_ids_raw(&v), vec!["glm-5.3", "kimi-k3-1", "deepseek-v4-pro"]);
    }

    /// 上游的另一种形态：`{data:[{id}]}`（OpenAI 风格）。
    #[test]
    fn parse_model_ids_reads_openai_list() {
        let v = json!({ "object": "list", "data": [ {"id": "auto"}, {"id": "glm-5.3"} ] });
        assert_eq!(parse_model_ids_raw(&v), vec!["auto", "glm-5.3"]);
    }

    /// 拉不到就是空 —— 不再退回内置兜底（那会让客户端以为有这些模型，
    /// 实际发过去照样 404，不如让调用方把失败原因报出来）。
    #[test]
    fn parse_model_ids_raw_returns_empty_when_nothing() {
        assert!(parse_model_ids_raw(&json!({})).is_empty());
        assert!(parse_model_ids_raw(&json!({"data": {"agents": []}})).is_empty());
    }
}
