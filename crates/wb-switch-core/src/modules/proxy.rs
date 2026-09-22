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
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::account::{load_accounts, upsert_account};
use super::config::{home_dir, now_ms, WORKBUDDY_API_ENDPOINT};
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
    failed_at: Option<Instant>,
}

struct Pool {
    leases: Vec<std::sync::Mutex<AccountLease>>,
    cursor: AtomicUsize,
}

impl Pool {
    fn new(uids: Vec<String>) -> Self {
        Self {
            leases: uids
                .into_iter()
                .map(|uid| {
                    std::sync::Mutex::new(AccountLease {
                        uid,
                        failed_at: None,
                    })
                })
                .collect(),
            cursor: AtomicUsize::new(0),
        }
    }

    /// 轮转取一个「当前可用」的 uid：跳过冷却中的号；全被冷却时返回 None。
    fn next_uid(&self) -> Option<String> {
        if self.leases.is_empty() {
            return None;
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        for i in 0..self.leases.len() {
            let idx = (start + i) % self.leases.len();
            let Ok(lease) = self.leases[idx].lock() else {
                continue;
            };
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
async fn take_account(pool: &Pool, _cfg: &ProxyConfig) -> Option<Value> {
    let uid = pool.next_uid()?;
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

fn chat_url() -> String {
    format!("{WORKBUDDY_API_ENDPOINT}/v2/chat/completions")
}

fn models_url() -> String {
    format!("{WORKBUDDY_API_ENDPOINT}/v2/enterprises/personal/models")
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
    h.insert("Origin".into(), WORKBUDDY_API_ENDPOINT.into());
    h.insert("Referer".into(), format!("{WORKBUDDY_API_ENDPOINT}/"));
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

/// 把 OpenAI 入站体改写成上游形态（强制流式、去掉上游不认识的字段）。
fn build_upstream_body(incoming: &Value) -> Value {
    let model = incoming
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("deepseek-v4-flash")
        .to_string();
    let messages = incoming.get("messages").cloned().unwrap_or_else(|| json!([]));
    let mut body = json!({
        "model": model,
        "messages": messages,
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

async fn list_models(State(st): State<AppState>) -> Response {
    let Some(acc) = take_account(&st.pool, &st.cfg).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "no available account"}})),
        )
            .into_response();
    };
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let mut req = client.get(models_url());
    for (k, v) in chat_headers(&acc) {
        req = req.header(k, v);
    }
    match req.send().await {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(v) => {
                // 上游结构：{ code, msg, data: { agents: [ { name, models: [...] } ] } }
                // 也可能直接是 { data: [...] } 或裸数组，这里都兜住。
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
                if ids.is_empty() {
                    // 拉不到就给一个可用的兜底，避免客户端因为空列表直接报错。
                    for s in [
                        "auto",
                        "hy4-preview",
                        "deepseek-v4-pro",
                        "deepseek-v4.1-flash",
                        "glm-5.3",
                        "kimi-k3-1",
                        "minimax-m3",
                    ] {
                        push_unique(&mut ids, s);
                    }
                }
                let data: Vec<Value> = ids
                    .into_iter()
                    .map(|id| json!({"id": id, "object": "model", "owned_by": "workbuddy"}))
                    .collect();
                Json(json!({"object": "list", "data": data})).into_response()
            }
            Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
        },
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
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

    let Some(acc) = take_account(&st.pool, &st.cfg).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": "no available account (all cooling down or needs relogin)"}})),
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
    let mut req = client.post(chat_url());
    for (k, v) in chat_headers(&acc) {
        req = req.header(k, v);
    }
    let resp = match req.json(&build_upstream_body(&incoming)).send().await {
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
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream; charset=utf-8")
            .header("Cache-Control", "no-cache")
            .header("X-Accel-Buffering", "no")
            .body(axum::body::Body::from_stream(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    } else {
        match aggregate(Box::pin(resp.bytes_stream()), &model).await {
            Ok(json) => {
                let usage = json.get("usage").cloned();
                record_usage(&meta, usage.as_ref(), true);
                Json(json).into_response()
            }
            Err(e) => {
                record_usage(&meta, None, false);
                (StatusCode::BAD_GATEWAY, e).into_response()
            }
        }
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
    let uids = eligible_uids(&cfg);
    if uids.is_empty() {
        return Err("没有可用账号：账号库为空或全部 needs_relogin".to_string());
    }
    let st = AppState {
        pool: Arc::new(Pool::new(uids)),
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
    let uids = eligible_uids(&cfg);
    if uids.is_empty() {
        let msg = "没有可用账号：账号库为空或全部 needs_relogin".to_string();
        let _ = ready.send(Err(msg.clone()));
        return Err(msg);
    }
    let st = AppState {
        pool: Arc::new(Pool::new(uids)),
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
fn eligible_uids(cfg: &ProxyConfig) -> Vec<String> {
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
        .filter_map(|a| a.get("uid").and_then(Value::as_str).map(str::to_string))
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
