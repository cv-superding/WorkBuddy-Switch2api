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
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
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

use chrono::{FixedOffset, Local, NaiveDateTime, TimeZone};

use super::account::{is_envelope, load_accounts, upsert_account};
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

/// 账号被上游**风控限制**（`11140`）后的**起始**冷却时长：3 小时。
///
/// 🔴🔴 这里曾经写死 6 小时，后来按「腾讯云开发者社区说停 2~4 小时」改成 3 小时 ——
/// **社区那个口径对本案不成立**。2026-10-29 实测：
/// - 同一账号换出口 IP（走代理）仍然 403 ⇒ 与 IP 无关，绑的是账号
/// - 用户反馈已等 **2 天**仍未恢复 ⇒ 不是「几小时自解」那类内容安全临时抖动
/// - 8 个国际号里 5 个可测的**全部** 403 ⇒ 更像批量标记，而非单次误触
///
/// 所以改成**递增冷却**：3h → 6h → 12h → 24h（封顶）。
/// 好处是既保留「万一真自愈了」的自动回池能力，又不会在长时间受限的号上
/// 每 3 小时白撞一次（反复重试被认为会加重风控评分）。
/// 一旦该号成功出字，计数清零，下次再被限重新从 3 小时起步。
const RESTRICTED_COOLDOWN_BASE: Duration = Duration::from_secs(3 * 60 * 60);
/// 递增冷却的上限：24 小时。
const RESTRICTED_COOLDOWN_MAX: Duration = Duration::from_secs(24 * 60 * 60);

// ---------------------------------------------------- 池策略（对齐社区实现）
//
// 参数取自开源实现 `dsh-workbuddy2api` 的池策略（2026-10 版本），
// 它那套值是在真实风控下趟出来的，不是拍脑袋：
//
// | 项 | 值 |
// |---|---|
// | 会话粘性 | 同一会话固定一个号，TTL 30 分钟滚动续期 |
// | 并发上限 | 单号 3 / 国际版号 2 / 整池 8 |
// | 限流 429 | 600s 基数指数退避，封顶 2h，**已在冷却中不翻倍** |
// | 5xx | 连续 3 次熔断，30m 起翻倍封顶 6h |
//
// 我们原来的选号只有一个 cursor 轮转：多账号在同一 IP 上高频互换，
// 正是风控判定「账号共享/批量使用」最典型的形状 —— 2026-10-29 国际版
// 5/5 号被批量限制（11140）大概率与之相关。这一组参数就是针对它。

/// 会话粘性 TTL：同一会话在这段时间内固定用同一个账号，到期滚动续期。
///
/// 为什么要粘：同一段对话在多个账号之间跳，等于「一个人一会儿在上海一会儿在纽约」，
/// 既是风控的异常信号，也会让上游的会话态/缓存失效。
const STICKY_TTL: Duration = Duration::from_secs(30 * 60);

/// 单账号在途请求上限（国际版号更保守：号少、风控更敏感）。
const MAX_INFLIGHT_DOMESTIC: u32 = 3;
const MAX_INFLIGHT_INTERNATIONAL: u32 = 2;
/// 整池在途上限。
const MAX_INFLIGHT_POOL: u32 = 8;

/// 429 软冷却基数与封顶。
const RATE_LIMIT_BASE: Duration = Duration::from_secs(600);
const RATE_LIMIT_MAX: Duration = Duration::from_secs(2 * 60 * 60);

/// 5xx 熔断：连续失败达到阈值即断开，时长翻倍、封顶 6 小时。
const BREAKER_THRESHOLD: u32 = 3;
const BREAKER_BASE: Duration = Duration::from_secs(30 * 60);
const BREAKER_MAX: Duration = Duration::from_secs(6 * 60 * 60);

/// 单个请求内**换号重试**的最大次数。
///
/// 只在上游返回「可换号解决」的错误时用（429 / 5xx）；
/// 参数错误、内容被拦这类客户端错误直接透传，换号只会浪费别的账号。
const MAX_ACCOUNT_RETRY: usize = 2;

/// 上游「渠道校验」（`11128`）的最大重试次数。
///
/// 这个码**不是账号的问题**（同号读接口 200、打 chat 端点 11128），也**不一定**是
/// 请求内容的问题。历史上它表现为间歇软拦截 —— 上游自己的文案就在邀请重发
/// （`displayMsg.zh = "请求被拦截，请重新发送"`），所以值得重试一两次。
///
/// 🔴 但 2026-10-17 起国际版实测是**持续**拦截（5/5 号、4 种 UA、合规最小请求全挂），
/// 重试只是把延迟拉长。所以重试次数保持很小、且**重试耗尽后绝不冷却账号**
/// （理由与判据见 4xx 分支里 `is_channel_block` 那段注释）。
const MAX_CHANNEL_RETRY: usize = 2;

/// 上游**连接层**失败（DNS 解析不了 / 超时 / 连接被拒绝 / TLS 重置）的最大重试次数。
///
/// 🔴 连接层失败**不是账号的问题**，绝不能拿它去冷却账号。
/// 以前这里无条件 `mark_failed`，于是网络一抖就把整池冻 120 秒：
/// 国际版只有 3 个号，几下就冻空 ⇒ 客户端收到
/// `503 国际版的账号当前都不可用（冷却中，或需要重新登录）`，过两分钟又好了，
/// 然后网络再抖 → 再冻 —— 表现为「一会儿报错停下来、一会儿又能用」反复横跳。
/// （2026-10-11 与 `refresh.rs` 里 `code=-1` 那个坑同源。）
///
/// 2026-10-26 从 **1 次** 提到 3 次：用户看到客户端打出
/// `502: error sending request for url (https://www.workbuddy.ai/v2/chat/completions)`
/// 连续 5 次，以为账号被封了 —— 其实只是直连线路在抖（当时 socket 探测 191ms 正常，
/// 过几分钟自行恢复）。反代**走直连、不读系统代理**，线路一抖就只有这一条路可走，
/// 只重试 1 次太浅，容易把瞬时抖动放大成「全线故障」的观感。
const MAX_TRANSPORT_RETRY: usize = 3;

/// 连接层失败的退避梯度：400ms → 1s → 2s。
///
/// 不取更长：这是在**等网络**，不是等上游处理；客户端（ARTEX / zcode / DSH）
/// 自己还有外层重试，反代这边拖太久只会让用户觉得「卡住了」。
const TRANSPORT_RETRY_BACKOFF_MS: [u64; MAX_TRANSPORT_RETRY] = [400, 1_000, 2_000];

/// 入站「思考强度」相关字段名，原样透传给上游。
///
/// 🔴 上游真正认的是 `reasoning_effort`（实测：加它之后 `deepseek-v4-pro` /
/// `deepseek-v4.1-flash` 立即开始返回 `reasoning_content` 与非零 `reasoning_tokens`；
/// 而 `thinking`（含 `budget_tokens`）与 `enable_thinking` 上游一律忽略）。
/// 其余几个是 pi-ai 按 `compat.thinkingFormat` 可能发出的其它拼写，一并透传，
/// 免得换个客户端又踩一遍「参数被吃掉」。
const REASONING_PASSTHROUGH: &[&str] = &[
    "reasoning_effort",
    "thinking",
    "reasoning",
    "enable_thinking",
    "chat_template_kwargs",
];

/// 标准采样 / 输出控制参数，**原样透传**。
///
/// 🔴 这些以前全被丢掉（`build_upstream_body` 只转 model/messages/stream/
/// temperature/top_p/max_tokens/tools），后果是「客户端设了等于没设」：
/// JSON 模式不生效、stop 序列不生效、`seed` 不可复现、Agent 无法禁止并行工具调用。
///
/// 实测（2026-10-21，国内版 `codebuddy.cn`，假模型 oracle：
/// 过校验 → `11102 model not found`）：下面每一个上游都接受，无一报 11101。
/// 所以丢弃它们没有收益，只是静默降级。
const PASSTHROUGH_PARAMS: &[&str] = &[
    "temperature",
    "top_p",
    "max_tokens",
    "stop",
    "frequency_penalty",
    "presence_penalty",
    "response_format",
    "parallel_tool_calls",
    "seed",
    "stream_options",
    "user",
    "logprobs",
    "top_logprobs",
];

/// 需要从 system prompt 里抹掉的**第三方 agent 身份声明**。
///
/// 社区实测（2026-10-21，群友反馈）：腾讯风控会识别系统提示词里固定的
/// 「DeepSeek Harness」身份声明，命中就把整条请求判成 `11128 unapproved channel`；
/// DSH 自带 `includeHarnessIdentity` 开关，关掉它（或把声明从提示词里删掉）即可恢复。
///
/// 🔴 2026-10-28 起**国内版**也按「指令文本指纹」拦 11128
/// （同为开源实现 `dsh-workbuddy2api` 的实测结论，与本机现象一致）：
/// `system` / `developer` / **`assistant` 历史** 里出现 `DeepSeek Harness`
/// 就整条拒；`user` / `tool` 消息里的品牌名不触发。
///
/// 因此清洗**不能只在 11128 自愈时做**（那样每个新会话都要先撞一次 400），
/// 必须在默认路径对国内版主动执行 —— 见 [`neutralize_domestic_fingerprints`]。
///
/// 替换词取 **`DeepSeek`** 而不是整段删掉或换成 `Assistant`：
/// 它就是原短语的前缀，「powered by DeepSeek Harness」→「powered by DeepSeek」
/// 语义零损失；换成 `Assistant` 会出现「powered by Assistant」这种病句。
const IDENTITY_DECLARATIONS: &[&str] = &[
    "DeepSeek Harness",
    "deepseek-harness",
    "DeepSeek-Harness",
];

/// 把 system 文本里的第三方身份声明替换成中性称呼（ASCII 大小写不敏感）。
///
/// 返回 `None` = 没有命中，调用方不必改动原串。
///
/// 实现按**字节**逐位比较、按 `char` 步进：既不破坏 UTF-8 边界，
/// 也不需要为大小写转换预先做整串 lower（那样会改变字节偏移）。
fn strip_identity_declarations(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    let mut hit = false;
    'outer: while i < bytes.len() {
        for phrase in IDENTITY_DECLARATIONS {
            let p = phrase.as_bytes();
            if i + p.len() <= bytes.len()
                && bytes[i..i + p.len()]
                    .iter()
                    .zip(p.iter())
                    .all(|(a, b)| a.eq_ignore_ascii_case(b))
            {
                out.push_str("DeepSeek");
                i += p.len();
                hit = true;
                continue 'outer;
            }
        }
        // 未命中：按完整字符推进一步（保证落在 UTF-8 边界上）
        let ch = text[i..].chars().next()?;
        out.push(ch);
        i += ch.len_utf8();
    }
    if hit {
        Some(out)
    } else {
        None
    }
}

/// 把**被空格包围的裸 `@`** 换成全角 `＠`，返回 `None` 表示没命中。
///
/// 国内版 11128 的第二道判据（2026-10-28，见 [`IDENTITY_DECLARATIONS`]）：
/// 同一条消息里出现**裸 `@` 提及**（两侧都是空格）且同时出现
/// 「`@` 前缀是路径」那段说明 → 拒。
///
/// 只动 ` @ ` 这一种形态，因此：
/// - `@file`、`@scope/pkg`（@ 在词首，前面不是空格）→ 不动
/// - `someone@example.com`（两侧无空格）→ 不动
/// - `@"..."`（后面不是空格）→ 不动
///
/// 全角 `＠` 在语义上与 `@` 等价，模型读到不会误解，但不再命中该指纹。
fn neutralize_bare_at(text: &str) -> Option<String> {
    if !text.contains(" @ ") {
        return None;
    }
    Some(text.replace(" @ ", " ＠ "))
}

/// **国内版**出站前的主动清洗：抹掉渠道校验会认的「指令文本指纹」。
///
/// 为什么必须主动做（而不是等 11128 再自愈）：
/// 自愈是在**已经撞墙之后**才发生，每个新会话都要多花一次 400 + 一次重发；
/// 而且自愈只对 `unapproved channel` 变体生效，对「文本指纹」这一道命中不了。
///
/// 清洗范围严格对齐上游实测判据：
/// - 角色：`system` / `developer` / `assistant`（**assistant 历史也被扫描**，
///   否则「上一轮提到过品牌名」会让后续每一轮都复发）
/// - 载体：字符串 content、multipart 的 text 块、以及 tool 的 description
/// - `user` / `tool` 内容不动（实测不触发，改了反而有损用户内容）
fn neutralize_domestic_fingerprints(messages: &mut Value, degraded: &mut Vec<String>) {
    let Some(arr) = messages.as_array_mut() else {
        return;
    };
    let mut hits = 0usize;
    for item in arr.iter_mut() {
        let role = item.get("role").and_then(Value::as_str).unwrap_or("");
        if !matches!(role, "system" | "developer" | "assistant") {
            continue;
        }
        let Some(content) = item.get_mut("content") else {
            continue;
        };
        match content {
            Value::String(s) => {
                let mut next = s.clone();
                if let Some(v) = strip_identity_declarations(&next) {
                    next = v;
                    hits += 1;
                }
                if let Some(v) = neutralize_bare_at(&next) {
                    next = v;
                    hits += 1;
                }
                *s = next;
            }
            Value::Array(parts) => {
                for part in parts.iter_mut() {
                    // 先拷成 owned String 再写回：避免同时持有只读借用与可变借用。
                    let Some(mut next) = part
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                    else {
                        continue;
                    };
                    if let Some(v) = strip_identity_declarations(&next) {
                        next = v;
                        hits += 1;
                    }
                    if let Some(v) = neutralize_bare_at(&next) {
                        next = v;
                        hits += 1;
                    }
                    part["text"] = Value::String(next);
                }
            }
            _ => {}
        }
    }
    if hits > 0 {
        degraded.push(format!("国内版渠道指纹清洗 {hits} 处"));
    }
}

/// 工具描述同样在扫描范围内（`tools[].function.description`）。
fn neutralize_domestic_tool_descriptions(tools: &mut Value, degraded: &mut Vec<String>) {
    let Some(arr) = tools.as_array_mut() else {
        return;
    };
    let mut hits = 0usize;
    for tool in arr.iter_mut() {
        let Some(mut next) = tool
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        if let Some(v) = strip_identity_declarations(&next) {
            next = v;
            hits += 1;
        }
        if let Some(v) = neutralize_bare_at(&next) {
            next = v;
            hits += 1;
        }
        tool["function"]["description"] = Value::String(next);
    }
    if hits > 0 {
        degraded.push(format!("国内版工具描述清洗 {hits} 处"));
    }
}

/// 最大强度归一化（11128 自愈重试用，见 [`build_upstream_body_ex`]）。
///
/// 上游对「负载是不是官方客户端发的」有一套**不公开**的规则，且会随策略变动：
/// 2026-10-09 判环境注入块（`gitStatus:`）、2026-10-17 判 `developer` 角色，
/// 2026-10-21 再测前者已放行、只剩后者。既然规则会漂，就留一条「全部收敛」的退路：
/// 收到 `unapproved channel` 时用这个函数重造一份最保守的请求再试一次。
///
/// 三件事：
/// 1. **角色收敛** —— 只保留 `system` / `user` / `assistant` / `tool`，
///    其它一律按 `system` 处理（`developer` 及其它未知别名）。
/// 2. **去掉 `name`** —— 部分客户端在这里写 agent 名，属于身份声明。
/// 3. **内容块拍平** —— 数组 content 合并成一个字符串，避开多块解析差异。
fn sanitize_messages_aggressive(messages: &mut Value, degraded: &mut Vec<String>) {
    let Some(arr) = messages.as_array_mut() else {
        return;
    };
    let mut touched = false;
    for m in arr.iter_mut() {
        // 1. 角色收敛
        let role = m.get("role").and_then(Value::as_str).unwrap_or("").to_string();
        let mapped = match role.as_str() {
            "system" | "user" | "assistant" | "tool" => None,
            // developer 及其它未知角色 → system（语义最接近，且上游认识）
            _ => Some("system"),
        };
        if let Some(new_role) = mapped {
            m["role"] = Value::String(new_role.to_string());
            touched = true;
        }
        // 2. 去掉 name
        if m.as_object().map(|o| o.contains_key("name")).unwrap_or(false) {
            if let Some(o) = m.as_object_mut() {
                o.remove("name");
            }
            touched = true;
        }
        // 3. 内容块拍平成字符串
        if let Some(parts) = m.get("content").and_then(Value::as_array) {
            let flat: String = parts
                .iter()
                .filter_map(|p| {
                    p.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| p.as_str())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
                .join("\n");
            m["content"] = Value::String(flat);
            touched = true;
        }
        // 4. 抹掉第三方 agent 身份声明（见 IDENTITY_DECLARATIONS）。
        //    只对 system 生效 —— 用户消息里的同名词是正常内容，不该动。
        if m.get("role").and_then(Value::as_str) == Some("system") {
            if let Some(text) = m.get("content").and_then(Value::as_str) {
                if let Some(cleaned) = strip_identity_declarations(text) {
                    m["content"] = Value::String(cleaned);
                    touched = true;
                }
            }
        }
    }
    if touched {
        degraded.push("aggressive-normalize".to_string());
    }
}

/// 把 `tool_choice` 归一成上游认识的形式（**只吃字符串**）。
///
/// 🔴 OpenAI 规范里 `tool_choice` 允许对象：
/// `{"type":"function","function":{"name":"f"}}`。
/// 上游对这个对象形式直接 400：
/// `11101 Unmarshal chat params failed with error: json: cannot unmarshal object …`
/// （2026-10-21 实测）。而**函数名字符串 `"f"` 上游是接受的**，
/// 所以对象形式可以无损降级成「函数名」——强制调用语义完整保留。
///
/// 返回 `None` = 无法安全转换，调用方应**丢弃**该字段（发过去只会 400）。
fn normalize_tool_choice(v: &Value) -> Option<Value> {
    match v {
        // 字符串（"auto" / "none" / "required" / 函数名）上游原样接受。
        Value::String(s) if !s.trim().is_empty() => Some(v.clone()),
        Value::Object(map) => {
            let ty = map.get("type").and_then(Value::as_str).unwrap_or("");
            match ty {
                // 指定函数 → 用函数名（上游接受的字符串形式，语义最接近）。
                "function" => Some(
                    map.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(|name| json!(name))
                        .unwrap_or_else(|| json!("required")),
                ),
                "auto" | "none" | "required" => Some(json!(ty)),
                // Anthropic 的 {"type":"any"} / {"type":"tool"} 语义
                "any" => Some(json!("required")),
                "tool" => map
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|n| json!(n))
                    .or_else(|| Some(json!("required"))),
                // allowed_tools 之类：退化成 auto，别把请求打死。
                "allowed" | "allowed_tools" => Some(json!("auto")),
                _ => None,
            }
        }
        _ => None,
    }
}
/// 单次请求体上限。
///
/// axum 默认只给 2 MB（`DEFAULT_LIMIT`），长上下文会话（几十万 token）序列化后
/// 很容易超过，会被直接 413 掉、且报错信息里看不到原因。这里显式放宽到 64 MB：
/// 足以容纳百万级 token 的上下文，同时仍留一个防止内存被打爆的上界。
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------- 配置

/// 一个版本的接入点：**监听地址、鉴权 key、开关三者各自独立**。
///
/// 为什么必须分开：国内版（`.workbuddy` / `www.codebuddy.cn`）与国际版
/// （`.workbuddy-ai` / `www.workbuddy.ai`）的**凭证与模型列表完全不互通** ——
/// 上游会校验 token 的 issuer，拿国内版的号去打国际版域名必然被拒。
/// 两边各给一个入口之后：
/// - 客户端按**端口**区分连哪一版，不再依赖「用模型名猜版本」；
/// - 两边各用一把 key，便于分别授权 / 撤销；
/// - `GET /v1/models` 只列本版本的模型，不会被另一版的模型误导。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    #[serde(default)]
    pub enabled: bool,
    /// `host:port`。留空时由 [`ProxyConfig::resolved`] 补成该版本的默认端口。
    #[serde(default)]
    pub listen: String,
    /// 留空 = 不鉴权（仅在 `127.0.0.1` 这类本机监听时才建议留空）。
    #[serde(default)]
    pub api_key: String,
    /// 参与**这个入口**反代的 uid；留空 = 该版本的全部账号。
    ///
    /// 两个入口各存一份：国内版用哪几个号和国际版用哪几个号本来就是两个独立决定，
    /// 共用一个名单只会让人在两处改同一件事。
    #[serde(default)]
    pub accounts: Vec<String>,
    /// **额外放行**的模型：上游列表里没有、但本入口实测可用的。
    ///
    /// 🔴 为什么必须有它：上游的 `/v2/enterprises/personal/models` 是**给客户端 UI 看的展示列表**，
    /// 里面混着 `default-model` / `fast-model` / `primary-model` / `deep-model` 这类「模式名」，
    /// **不等于可用模型全集**。实测（2026-10-11）：`deepseek-v4.1-flash` 在国际版上游
    /// `/v2/chat/completions` 上是 200 成功的，却**完全不在国际版的列表里**
    /// （那 29 KB JSON 里 `"deepseek"` 出现 0 次）；反过来列表里的 `deep-model` 直发反而 11102。
    ///
    /// 版本防火墙是按列表判定归属的，于是这类「能用但没列出」的模型会被误判成另一版专属、
    /// 直接 400 掉。填进这里的模型：
    /// ① 会并入本入口 `GET /v1/models` 的返回；
    /// ② **跳过版本防火墙**（用户显式声明优先于自动推断）。
    #[serde(default)]
    pub extra_models: Vec<String>,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: String::new(),
            api_key: String::new(),
            accounts: Vec::new(),
            extra_models: Vec::new(),
        }
    }
}

/// 国内版入口的默认监听地址（沿用 v1 的默认值，老用户升级后地址不变）。
pub const DEFAULT_LISTEN_DOMESTIC: &str = "127.0.0.1:7863";
/// 国际版入口的默认监听地址。
pub const DEFAULT_LISTEN_INTERNATIONAL: &str = "127.0.0.1:7864";

/// 反代配置：**两个入口彼此独立** —— 各有开关、监听地址、API Key、账号名单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// 国内版入口。
    #[serde(default)]
    pub domestic: Endpoint,
    /// 国际版入口。
    #[serde(default)]
    pub international: Endpoint,
    /// 反代**出网**代理：反代请求上游时走的代理，形如 `http://127.0.0.1:7897`。
    ///
    /// 留空 = **直连**（并且会显式关掉 reqwest 的“跟随环境变量代理”，
    /// 避免 GUI 从 shell 继承了 `HTTP_PROXY` 却没人知道）。
    /// `direct` / `none` 也视为直连，方便临时关掉。
    ///
    /// 为什么需要它：反代原来硬编码直连，直连线路一抖就整片 502
    /// （`error sending request for url (...)`），而用户本机往往已经有可用的
    /// 梯子。社区实现（buddy-proxy / workbuddy2api-hub）都做了「按号/按池固定出口」，
    /// 这里先提供**全局出口**这一档。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_proxy: Option<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            domestic: Endpoint::default(),
            international: Endpoint::default(),
            upstream_proxy: None,
        }
    }
}

impl ProxyConfig {
    /// 实际生效的出网代理地址（`None` = 直连）。
    ///
    /// 优先级：配置文件 → 环境变量 `WB_UPSTREAM_PROXY`。
    /// `""` / `direct` / `none` 都按直连处理，方便临时关掉而不用删字段。
    pub fn effective_upstream_proxy(&self) -> Option<String> {
        let raw = self
            .upstream_proxy
            .clone()
            .or_else(|| std::env::var("WB_UPSTREAM_PROXY").ok())?;
        let v = raw.trim().to_string();
        if v.is_empty() || v.eq_ignore_ascii_case("direct") || v.eq_ignore_ascii_case("none") {
            None
        } else {
            Some(v)
        }
    }

    /// 给上游请求用的 client builder 套上出网策略。
    ///
    /// 🔴 未配置时必须显式 `.no_proxy()`：reqwest 默认会读 `HTTP_PROXY` /
    /// `HTTPS_PROXY` / `ALL_PROXY` 环境变量。GUI 应用从 shell 启动时会继承这些变量，
    /// 于是「我明明没配代理」却实际走了代理，行为不可预期、排查极难。
    /// 显式声明直连，配置说了算。
    pub fn apply_upstream_proxy(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        match self.effective_upstream_proxy() {
            Some(url) => match reqwest::Proxy::all(&url) {
                Ok(p) => builder.proxy(p),
                // 地址写错不该让整个入口起不来：退回直连并留一条日志。
                Err(e) => {
                    eprintln!("[反代] 出网代理地址无效，已退回直连：{url} ({e})");
                    builder.no_proxy()
                }
            },
            None => builder.no_proxy(),
        }
    }

    /// 取某个版本的接入点。
    pub fn endpoint(&self, e: Edition) -> &Endpoint {
        match e {
            Edition::Domestic => &self.domestic,
            Edition::International => &self.international,
        }
    }

    /// 同上，可变引用。
    pub fn endpoint_mut(&mut self, e: Edition) -> &mut Endpoint {
        match e {
            Edition::Domestic => &mut self.domestic,
            Edition::International => &mut self.international,
        }
    }

    /// 把空着的监听地址补成该版本的默认端口。**读配置后必调** ——
    /// 配置文件里写 `"domestic": {"enabled": true}` 也是合法的，端口取默认值。
    pub fn resolved(mut self) -> Self {
        for e in Edition::ALL {
            let d = default_listen_of(e);
            let ep = self.endpoint_mut(e);
            if ep.listen.trim().is_empty() {
                ep.listen = d.to_string();
            }
        }
        self
    }

    /// 有没有任何一个入口被启用。
    pub fn any_enabled(&self) -> bool {
        Edition::ALL.iter().any(|&e| self.endpoint(e).enabled)
    }
}

/// 某版本的默认监听地址。
pub fn default_listen_of(e: Edition) -> &'static str {
    match e {
        Edition::Domestic => DEFAULT_LISTEN_DOMESTIC,
        Edition::International => DEFAULT_LISTEN_INTERNATIONAL,
    }
}

/// v1 时代的**扁平**配置：`{ enabled, listen, api_key, accounts }`。
///
/// 只用于读 —— 读到就迁移并立刻落盘（见 [`load_proxy_config`]）。
#[derive(Deserialize)]
struct LegacyConfig {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    listen: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    accounts: Vec<String>,
}

/// v2 的过渡结构：已有两个入口，但账号名单还是**两边共用**的一份。
#[derive(Deserialize, Default)]
struct V2Endpoint {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    listen: String,
    #[serde(default)]
    api_key: String,
}

#[derive(Deserialize, Default)]
struct V2Config {
    #[serde(default)]
    domestic: V2Endpoint,
    #[serde(default)]
    international: V2Endpoint,
    #[serde(default)]
    accounts: Vec<String>,
}

impl ProxyConfig {
    /// v1 扁平配置 → 现行结构。
    ///
    /// 旧的 `listen` / `api_key` / `enabled` / `accounts` 归到**国内版**（那是 v1
    /// 时代唯一存在的一档），国际版保持默认关闭 —— 不擅自替用户开启一个以前
    /// 不存在的监听端口。
    fn from_legacy(old: LegacyConfig) -> Self {
        ProxyConfig {
            domestic: Endpoint {
                enabled: old.enabled.unwrap_or(false),
                listen: old.listen.unwrap_or_default(),
                api_key: old.api_key.unwrap_or_default(),
                accounts: old.accounts,
                ..Endpoint::default()
            },
            international: Endpoint::default(),
            // 旧配置没有出网代理概念 → 保持直连，不擅自改网络行为。
            upstream_proxy: None,
        }
        .resolved()
    }

    /// v2（两边共用一份名单）→ 现行结构（各持一份）。
    ///
    /// 把那份名单**同时**发给两个入口即可 —— 行为完全等价，因为每个入口本来就
    /// 只能用到属于自己版本的号，不会因为多拿到另一版的 uid 而越界。
    fn from_v2(old: V2Config) -> Self {
        ProxyConfig {
            domestic: Endpoint {
                enabled: old.domestic.enabled,
                listen: old.domestic.listen,
                api_key: old.domestic.api_key,
                accounts: old.accounts.clone(),
                ..Endpoint::default()
            },
            international: Endpoint {
                enabled: old.international.enabled,
                listen: old.international.listen,
                api_key: old.international.api_key,
                accounts: old.accounts,
                ..Endpoint::default()
            },
            // v2 也没有出网代理字段 → 直连。
            upstream_proxy: None,
        }
        .resolved()
    }
}

fn config_path() -> std::path::PathBuf {
    home_dir().join(".wb-switch").join(CONFIG_FILE)
}

/// 读配置。**能识别并自动迁移 v1 的扁平格式**。
pub fn load_proxy_config() -> ProxyConfig {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return ProxyConfig::default().resolved();
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("[反代] {path:?} 解析失败({err})，使用默认配置");
            return ProxyConfig::default().resolved();
        }
    };

    let has_endpoints = value.get("domestic").is_some() || value.get("international").is_some();

    // 现行结构：两个入口，各自的 `accounts` 在入口内部（顶层没有 `accounts`）。
    if has_endpoints && value.get("accounts").is_none() {
        return match serde_json::from_value::<ProxyConfig>(value) {
            Ok(cfg) => cfg.resolved(),
            Err(err) => {
                eprintln!("[反代] {path:?} 结构不对({err})，使用默认配置");
                ProxyConfig::default().resolved()
            }
        };
    }

    // 需要迁移：v2（两个入口 + 顶层共用名单）或 v1（完全扁平）。
    let migrated = if has_endpoints {
        serde_json::from_value::<V2Config>(value)
            .map(ProxyConfig::from_v2)
            .map_err(|e| e.to_string())
    } else {
        serde_json::from_value::<LegacyConfig>(value)
            .map(ProxyConfig::from_legacy)
            .map_err(|e| e.to_string())
    };

    match migrated {
        Ok(cfg) => {
            eprintln!("[反代] 检测到旧版配置，已迁移为「国内版 / 国际版」两个独立入口");
            if let Err(e) = save_proxy_config(&cfg) {
                eprintln!("[反代] 迁移结果写回失败：{e}");
            }
            cfg
        }
        Err(err) => {
            eprintln!("[反代] {path:?} 无法识别({err})，使用默认配置");
            ProxyConfig::default().resolved()
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
    /// 被上游**风控限制**（`11140 request illegal`）的截止时刻。
    ///
    /// 🔴 这类限制是**账号级且长时**的，和 120 秒的抖动冷却完全不同：
    /// 2026-10-29 实测国际版 5/5 可用号全部 403
    /// `{"code":11140,"msg":"request illegal","displayMsg":{"en":"Your account has
    /// been restricted due to unusual activity"}}`，而**同一个 token** 读
    /// `/v2/enterprises/personal/models` 仍 200 —— 号没被注销，是被限制对话。
    /// 国内版同在一条网络下 200 正常，所以既不是 IP 也不是网络。
    ///
    /// 用 120 秒冷却去等它，只会每两分钟再撞一次 403：既刷屏，又可能让风控评分更差。
    restricted_until: Option<Instant>,
    /// 累计被风控限制的次数，用于**递增冷却**（见 `RESTRICTED_COOLDOWN_BASE`）。
    /// 成功出字后清零。
    restrict_strikes: u32,
    /// 429 软冷却截止时刻 + 连续次数（指数退避，见 `RATE_LIMIT_BASE`）。
    rate_until: Option<Instant>,
    rate_strikes: u32,
    /// 5xx 熔断：连续失败计数与断开截止时刻（见 `BREAKER_BASE`）。
    consecutive_5xx: u32,
    open_until: Option<Instant>,
    /// 在途请求数（并发上限用）。由 `Lease` 守卫在响应结束时归还。
    inflight: u32,
    /// **按模型**的长冷却：`model -> 恢复时刻`。
    ///
    /// 60004 只代表「这个模型的额度用完了」，同一账号换模型仍可用（上游文案
    /// 自己就写着「您也可以切换其他模型继续使用」）。账号级长冷却会把还能
    /// 用的模型一起冻掉，所以这类失败记在模型键上；账号仍留在池子里服务
    /// 其他模型。
    model_cooldowns: HashMap<String, Instant>,
}

/// 该租约现在是否**健康可用**（只看状态，不含粘性与并发上限）。
///
/// 抽成独立函数是为了让「轮转选号」和「粘性命中」共用同一套判据 ——
/// 两处判据一旦漂移，就会出现「粘性认为可用、轮转认为不可用」的矛盾。
fn lease_usable(lease: &AccountLease, want: Option<Edition>, model: Option<&str>) -> bool {
    if let Some(e) = want {
        if lease.edition != e {
            return false;
        }
    }
    let now = Instant::now();
    if lease.failed_at.map(|t| t.elapsed() < COOLDOWN).unwrap_or(false) {
        return false;
    }
    // 被风控限制（11140）的号直接跳过，别拿它去撞 403。
    if lease.restricted_until.map(|t| now < t).unwrap_or(false) {
        return false;
    }
    // 429 软冷却中
    if lease.rate_until.map(|t| now < t).unwrap_or(false) {
        return false;
    }
    // 5xx 熔断中
    if lease.open_until.map(|t| now < t).unwrap_or(false) {
        return false;
    }
    if let Some(m) = model {
        if lease
            .model_cooldowns
            .get(m)
            .map(|t| *t > now)
            .unwrap_or(false)
        {
            return false;
        }
    }
    true
}

/// 借用守卫：drop 时把在途计数还给池子。
///
/// 必须用 RAII 而不是手动 release —— 这个函数里有多条提前 return 的分支
/// （鉴权失败、模型不匹配、上游报错早退），手写释放漏一条就会永久占着一个并发名额，
/// 池子越用越窄，最后表现为「莫名其妙 503」。
struct LeaseGuard {
    pool: Arc<Pool>,
    uid: String,
}

impl LeaseGuard {
    fn uid(&self) -> &str {
        &self.uid
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.pool.release(&self.uid);
    }
}

struct Pool {
    leases: Vec<std::sync::Mutex<AccountLease>>,
    cursor: AtomicUsize,
    /// 会话 → (账号, 绑定到期时刻)。滚动续期，见 [`STICKY_TTL`]。
    sticky: std::sync::Mutex<HashMap<String, (String, Instant)>>,
    /// 整池在途总数（各 lease 的 inflight 之和，单独存一份免得到处加锁求和）。
    inflight_total: std::sync::atomic::AtomicU32,
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
                        restricted_until: None,
                        restrict_strikes: 0,
                        rate_until: None,
                        rate_strikes: 0,
                        consecutive_5xx: 0,
                        open_until: None,
                        inflight: 0,
                        model_cooldowns: HashMap::new(),
                    })
                })
                .collect(),
            cursor: AtomicUsize::new(0),
            sticky: std::sync::Mutex::new(HashMap::new()),
            inflight_total: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// 同上，但可以只在某一档里取号（`want = None` 表示不限版本）。
    ///
    /// `model` 给出时，还会跳过「该模型正在长冷却」的号 —— 60004 只冻那个模型，
    /// 同一账号的其他模型仍可服务。
    ///
    /// 一个版本都没有可用号时**不会**悄悄换另一个版本 —— 那样只会把
    /// 「模型不存在」的 400 换个地方报出来，不如直接说清楚。
    /// 仅供单测：纯健康轮转、不计并发也不写粘性。
    ///
    /// 生产路径一律走 [`Pool::pick`]（粘性 + 并发上限）。留这个口子是因为
    /// 早期测试都是按「给一个池子问下一个号」写的，用它断言健康判据最直接。
    #[cfg(test)]
    fn next_uid_of(&self, want: Option<Edition>, model: Option<&str>) -> Option<String> {
        if self.leases.is_empty() {
            return None;
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        for i in 0..self.leases.len() {
            let idx = (start + i) % self.leases.len();
            let Ok(lease) = self.leases[idx].lock() else {
                continue;
            };
            if lease_usable(&lease, want, model) {
                return Some(lease.uid.clone());
            }
        }
        None
    }

    /// 带**会话粘性 + 并发上限**的选号；返回的守卫在 drop 时归还在途计数。
    ///
    /// 顺序对齐社区实现：粘性优先 → 健康过滤 → 轮转兜底。
    /// 粘住的号只要健康就一直用它（TTL 内滚动续期），避免同一段对话
    /// 在多个账号之间跳 —— 那是风控眼里最典型的「账号共享」形状。
    fn pick(
        self: &Arc<Self>,
        want: Option<Edition>,
        model: Option<&str>,
        session: Option<&str>,
    ) -> Option<LeaseGuard> {
        if self.leases.is_empty() {
            return None;
        }
        // 整池背压：超过上限就不再放行，由调用方回 503。
        if self.inflight_total.load(Ordering::Relaxed) >= MAX_INFLIGHT_POOL {
            return None;
        }

        // ① 粘性：该会话已绑定的号若仍健康，直接续期使用。
        if let Some(key) = session {
            let bound = self
                .sticky
                .lock()
                .ok()
                .and_then(|m| m.get(key).map(|(uid, until)| (uid.clone(), *until)));
            if let Some((uid, until)) = bound {
                if until > Instant::now() && self.try_acquire(&uid, want, model) {
                    if let Ok(mut m) = self.sticky.lock() {
                        m.insert(key.to_string(), (uid.clone(), Instant::now() + STICKY_TTL));
                    }
                    return Some(LeaseGuard {
                        pool: Arc::clone(self),
                        uid,
                    });
                }
                // 绑定的号已失效 → 解绑，走下面的重新分配
                if let Ok(mut m) = self.sticky.lock() {
                    m.remove(key);
                }
            }
        }

        // ② 健康过滤 + 轮转
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        for i in 0..self.leases.len() {
            let idx = (start + i) % self.leases.len();
            let uid = {
                let Ok(lease) = self.leases[idx].lock() else {
                    continue;
                };
                if lease_usable(&lease, want, model) {
                    lease.uid.clone()
                } else {
                    continue;
                }
            };
            if !self.try_acquire(&uid, want, model) {
                continue;
            }
            if let Some(key) = session {
                if let Ok(mut m) = self.sticky.lock() {
                    m.insert(key.to_string(), (uid.clone(), Instant::now() + STICKY_TTL));
                }
            }
            return Some(LeaseGuard {
                pool: Arc::clone(self),
                uid,
            });
        }
        None
    }

    /// 在并发上限内占用一个号；成功则 inflight +1。
    fn try_acquire(&self, uid: &str, want: Option<Edition>, model: Option<&str>) -> bool {
        for slot in &self.leases {
            let Ok(mut lease) = slot.lock() else { continue };
            if lease.uid != uid {
                continue;
            }
            if !lease_usable(&lease, want, model) {
                return false;
            }
            let cap = match lease.edition {
                Edition::International => MAX_INFLIGHT_INTERNATIONAL,
                Edition::Domestic => MAX_INFLIGHT_DOMESTIC,
            };
            if lease.inflight >= cap {
                return false;
            }
            lease.inflight += 1;
            self.inflight_total.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// 归还在途计数（由 [`LeaseGuard`] 的 Drop 调用）。
    fn release(&self, uid: &str) {
        for slot in &self.leases {
            let Ok(mut lease) = slot.lock() else { continue };
            if lease.uid == uid {
                lease.inflight = lease.inflight.saturating_sub(1);
                self.inflight_total.fetch_sub(1, Ordering::Relaxed);
                return;
            }
        }
    }

    /// 成功出字：清掉 5xx 连败计数（熔断靠它复位）。
    fn mark_ok(&self, uid: &str) {
        for slot in &self.leases {
            let Ok(mut lease) = slot.lock() else { continue };
            if lease.uid == uid {
                lease.consecutive_5xx = 0;
                lease.open_until = None;
                return;
            }
        }
    }

    /// 429：软冷却 600s 基数指数退避、封顶 2h；**已在冷却中不翻倍**
    /// （否则用户狂点重试会把冷却越堆越厚）。返回本次剩余冷却。
    fn mark_rate_limited(&self, uid: &str) -> Duration {
        for slot in &self.leases {
            let Ok(mut lease) = slot.lock() else { continue };
            if lease.uid != uid {
                continue;
            }
            let now = Instant::now();
            if let Some(until) = lease.rate_until {
                if until > now {
                    // 已在冷却中：保持原截止，不翻倍
                    return until - now;
                }
            }
            lease.rate_strikes = lease.rate_strikes.saturating_add(1);
            let factor = 1u64 << lease.rate_strikes.saturating_sub(1).min(4);
            let secs = (RATE_LIMIT_BASE.as_secs() * factor).min(RATE_LIMIT_MAX.as_secs());
            let d = Duration::from_secs(secs);
            lease.rate_until = Some(now + d);
            return d;
        }
        RATE_LIMIT_BASE
    }

    /// 5xx：连续 3 次熔断，时长 30m 起翻倍、封顶 6h。返回断开时长（未断则 None）。
    fn mark_server_error(&self, uid: &str) -> Option<Duration> {
        for slot in &self.leases {
            let Ok(mut lease) = slot.lock() else { continue };
            if lease.uid != uid {
                continue;
            }
            lease.consecutive_5xx = lease.consecutive_5xx.saturating_add(1);
            if lease.consecutive_5xx < BREAKER_THRESHOLD {
                return None;
            }
            let over = lease.consecutive_5xx - BREAKER_THRESHOLD; // 0,1,2…
            let factor = 1u64 << over.min(4);
            let secs = (BREAKER_BASE.as_secs() * factor).min(BREAKER_MAX.as_secs());
            let d = Duration::from_secs(secs);
            lease.open_until = Some(Instant::now() + d);
            lease.consecutive_5xx = 0;
            return Some(d);
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

    /// 标记账号被上游**风控限制**，按**递增**时长不再选它，并返回本次的冷却时长。
    ///
    /// 与 [`mark_failed`](Self::mark_failed) 的区别见 `AccountLease::restricted_until`：
    /// 那个是 120 秒的抖动冷却，这个是「账号级、时长未知」的限制。
    ///
    /// 递增序列：3h → 6h → 12h → 24h（封顶）。理由见 `RESTRICTED_COOLDOWN_BASE`：
    /// 实测等 2 天仍不恢复，固定 3 小时只会让人每小时白撞一次。
    fn mark_restricted(&self, uid: &str) -> Duration {
        let mut applied = RESTRICTED_COOLDOWN_BASE;
        for slot in &self.leases {
            let Ok(mut lease) = slot.lock() else { continue };
            if lease.uid == uid {
                lease.restrict_strikes = lease.restrict_strikes.saturating_add(1);
                let factor = 1u32 << lease.restrict_strikes.saturating_sub(1).min(4);
                let secs = RESTRICTED_COOLDOWN_BASE.as_secs().saturating_mul(factor as u64);
                applied = Duration::from_secs(secs.min(RESTRICTED_COOLDOWN_MAX.as_secs()));
                lease.restricted_until = Some(Instant::now() + applied);
                break;
            }
        }
        applied
    }

    /// 该号成功出字 → 清掉风控计数，下次再被限从最短档重新开始。
    fn clear_restrict(&self, uid: &str) {
        for slot in &self.leases {
            let Ok(mut lease) = slot.lock() else { continue };
            if lease.uid == uid {
                lease.restrict_strikes = 0;
                lease.restricted_until = None;
                break;
            }
        }
    }

    /// 只冻「某账号的某个模型」到指定时刻 —— 账号的其他模型继续服务。
    ///
    /// 固定的 120 秒短冷却对付频率超限不够：120 秒后号回池立刻又 429，循环空转。
    /// `until_ms` 是上游声明的重置时刻（毫秒）；已过期则视为不冷却。
    fn mark_model_failed_until(&self, uid: &str, model: &str, until_ms: i64) {
        let now = now_ms_i64();
        if until_ms <= now {
            return;
        }
        let until = Instant::now() + Duration::from_millis((until_ms - now) as u64);
        for slot in &self.leases {
            let Ok(mut lease) = slot.lock() else { continue };
            if lease.uid == uid {
                lease.model_cooldowns.insert(model.to_string(), until);
            }
        }
    }

    /// 该账号剩余冷却毫秒数：短冷却与**所有模型**长冷却里最晚的那个。
    /// 不在冷却中返回 None。
    fn cooling_for(&self, uid: &str) -> Option<i64> {
        let now = Instant::now();
        for slot in &self.leases {
            let Ok(lease) = slot.lock() else { continue };
            if lease.uid != uid {
                continue;
            }
            let short = lease
                .failed_at
                .map(|t| COOLDOWN.as_millis() as i64 - t.elapsed().as_millis() as i64)
                .filter(|m| *m > 0);
            let long = lease
                .model_cooldowns
                .values()
                .map(|t| t.saturating_duration_since(now).as_millis() as i64)
                .filter(|m| *m > 0)
                .max();
            return long.max(short);
        }
        None
    }
}

fn now_ms_i64() -> i64 {
    now_ms() as i64
}

/// 由请求体算一个**跨轮次稳定**的会话键，用于会话粘性。
///
/// 取「首条 system + 首条 user」的哈希：同一段对话多轮里这两条是不变的
/// （历史只会往后面追加），所以键稳定；不同对话立刻分开。
///
/// 不直接用「整段 messages」的哈希 —— 那样每加一轮就换一个键，粘性等于没有。
fn session_key(incoming: &Value) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let msgs = incoming.get("messages")?.as_array()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut filled = 0u8;
    for m in msgs {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        let want_bit = match role {
            "system" => 1u8,
            "user" => 2u8,
            _ => continue,
        };
        if filled & want_bit != 0 {
            continue;
        }
        if let Some(c) = m.get("content").and_then(Value::as_str) {
            role.hash(&mut h);
            c.hash(&mut h);
            filled |= want_bit;
            if filled == 3 {
                break;
            }
        }
    }
    if filled == 0 {
        None
    } else {
        Some(format!("{:016x}", h.finish()))
    }
}

/// 取一个可用的账号（必要时先刷新 token），同时占住一个并发名额。
///
/// `model` 用于跳过「该模型正在长冷却」的号 —— 60004 只冻模型，不冻账号。
/// `session` 是会话键（见 [`session_key`]），用于会话粘性。
async fn take_account(
    pool: &Arc<Pool>,
    _cfg: &ProxyConfig,
    want: Option<Edition>,
    model: Option<&str>,
    session: Option<&str>,
) -> Option<(Value, LeaseGuard)> {
    let guard = pool.pick(want, model, session)?;
    let uid = guard.uid().to_string();
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
    Some((acc, guard))
}

// ---------------------------------------------------------------- 上游

fn chat_url(acc: &Value) -> String {
    format!("{}/v2/chat/completions", upstream_endpoint(acc))
}

fn models_url(acc: &Value) -> String {
    format!("{}/v2/enterprises/personal/models", upstream_endpoint(acc))
}

/// 位掩码：某个模型出现在**哪几档**的模型列表里。
const MASK_DOMESTIC: u8 = 1 << 0;
const MASK_INTERNATIONAL: u8 = 1 << 1;

fn edition_mask(e: Edition) -> u8 {
    match e {
        Edition::Domestic => MASK_DOMESTIC,
        Edition::International => MASK_INTERNATIONAL,
    }
}

/// 模型 id → **出现在哪几档的列表里**（位掩码），由「拉模型列表」这条路径**累加**写入。
///
/// 反代池里国内版和国际版的号是混着的，而两版的模型列表不一样：拿只属于另一版的 model
/// 去打本版网关会被上游拒（`11102 only available for authorized users`）。所以请求进来先按
/// model 查这张表，**确实只属于另一版**时才拦。
///
/// 🔴 **必须是累加，不能是覆盖**：两版列表有大量重叠（`glm-5.3` / `hy3` / `lite` …），
/// 覆盖会让重叠模型被记成「后拉的那一档」，于是**另一档的入口连自己列表里的模型都 400 掉**。
/// 实测踩过：国际版列表后拉之后，`7863` 发 `glm-5.3` 直接报「属于国际版」。
fn model_routes() -> &'static Mutex<HashMap<String, u8>> {
    static ROUTES: std::sync::OnceLock<Mutex<HashMap<String, u8>>> = std::sync::OnceLock::new();
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 记下「这些模型**也**出现在这一档的列表里」。拉列表成功时调用（取并集，不覆盖）。
fn remember_model_routes(edition: Edition, ids: &[String]) {
    if let Ok(mut m) = model_routes().lock() {
        let bit = edition_mask(edition);
        for id in ids {
            *m.entry(id.clone()).or_insert(0) |= bit;
        }
    }
}

/// 某个 model **只**属于哪一档。
///
/// 只在「恰好出现在一档的列表里」时返回 `Some`；**两档都有、或谁都没见过 → `None`** ——
/// 这两种情况都不该拦，交给上游判定。（`deepseek-v4.1-flash` 这类「上游能用但没列出」的
/// 模型，可用 [`Endpoint::extra_models`] 显式放行。）
pub fn model_edition(model: &str) -> Option<Edition> {
    let mask = model_routes()
        .lock()
        .ok()
        .and_then(|m| m.get(model).copied())
        .unwrap_or(0);
    match mask {
        MASK_DOMESTIC => Some(Edition::Domestic),
        MASK_INTERNATIONAL => Some(Edition::International),
        _ => None,
    }
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
    // 🔴🔴 `X-Conversation-ID` 是国际版渠道校验的**必需头**，缺它必被 11128 打回。
    //
    // 2026-10-26 对照实验（国际版 `www.workbuddy.ai`，同一个号、同一套其余头、
    // 同一个 payload，只有头数不同）：
    //
    // | 请求 | 结果 |
    // |---|---|
    // | `/v2/chat/completions` + 原有 2 个会话头 | **11128 `request illegal`** |
    // | `/v2/chat/completions` + 补上 `X-Conversation-ID` | **200 ✅** |
    //
    // 这正是 10-17~10-26 期间「国际版 5/5 号全挂、换 4 种 UA 全挂、同 token 读
    // 模型清单却 200」的原因 —— 当时据此判定「上游按调用方身份封禁、本地无解」，
    // **结论是错的**：它只是一道「头存在性」检查，补上就过。
    // 上游只查**存在性**，不校验值，所以复用同一个会话主键即可。
    //
    // （另一说国际版 chat 路由已迁到 `/console/chat/completions`；实测旧 `/v2`
    //  补齐三个头同样 200，故不切换路由，免得撞上 console 侧的另一道限流闸门。）
    h.insert("X-Conversation-ID".into(), conv_req.clone());
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
        // 🔴 `role: "developer"` 必须先改写成 `system`。
        //
        // 上游两道网关（国内 `codebuddy.cn` / 国际 `workbuddy.ai`）都把 `developer`
        // 判成「非官方渠道」，整条请求被 **400 `11128 Illegal API invocation from an
        // unapproved channel`** 打回。实测单变量对照（两版各 3/3 稳定复现）：
        //
        // | messages | 结果 |
        // |---|---|
        // | `[system, developer(长正文), user]` | `11128` |
        // | `[system, system(同一正文), user]` | `200` |
        // | `[system, user(同一正文), user]` | `200` |
        //
        // DSH 这类客户端会把 system prompt 用 `developer` 角色发出去（OpenAI 自 o1 起
        // 引入的 system 别名，语义等价）。改写它**不丢任何内容**，只是换回上游认的角色。
        // ⚠️ 必须在下面「system 环境注入净化」**之前**做，这样开发者消息也能吃到净化。
        if m.get("role").and_then(Value::as_str) == Some("developer") {
            m["role"] = Value::String("system".to_string());
            degraded.push("developer-role".to_string());
        }
        // system 消息先过一遍「环境注入」净化，见 strip_env_injection 的说明。
        if m.get("role").and_then(Value::as_str) == Some("system") {
            if let Some(cleaned) = m
                .get("content")
                .and_then(Value::as_str)
                .and_then(strip_env_injection)
            {
                degraded.push("system-env".to_string());
                m["content"] = Value::String(cleaned);
            }
        }
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

/// 摘掉 system prompt 末尾的**环境上下文注入块**。
///
/// 上游在 2026-10-09 起上线了一道**渠道校验**：它会识别 system prompt 里模仿
/// Claude Code 的环境注入（`gitStatus:` / `Current branch:` / `Main branch` /
/// `Git user:` / `Recent commits:` …），命中就把整条请求判成「非官方客户端」：
///
/// ```text
/// {"code":11128,"msg":"Illegal API invocation from an unapproved channel",
///  "displayMsg":{"zh":"请求被拦截，请重新发送"},
///  "displayTips":{"zh":"这次调用没通过渠道校验，可能不是官方客户端发出的。"}}
/// ```
///
/// 🔴 这**不是**出站请求头伪装不够——同一套头、同一个号，只去掉这段就能过。
/// 也**不是**单个关键词：把这段单独拎出来测是通过的，「长前缀 + 这段」的组合
/// 才必然命中，说明上游认的是累积出来的整体模式。
///
/// 实测（**假 model 名，零额度消耗**，靠返回码区分）：
///
/// | 请求 | 结果 |
/// |---|---|
/// | 前 10132 字 | `11102` 过了渠道校验，只是 model 不存在 |
/// | 前 10132 字 + `"x"*39`（同长度换内容） | `11102` 仍通过 |
/// | 前 10132 字 + 真实触发段 | `11128` 命中 |
/// | 去掉 `gitStatus:` 之后整块 | `11102` 恢复 |
/// | 把 system 改成 user | `11102` 恢复 |
///
/// 处理方式：从**最后一个** `gitStatus:` 起截断。ZCode 把这段环境快照追加在
/// system prompt 末尾，丢掉它只是让模型看不到 git 状态——而 ZCode 每轮都会重新
/// 注入，代价远小于整条请求被打回重试。
///
/// 返回 `None` = 没有可摘的块（原样透传，例如没注入过的短 system prompt）。
fn strip_env_injection(text: &str) -> Option<String> {
    let idx = text.rfind("gitStatus:")?;
    let cleaned = text[..idx].trim_end();
    // 摘完什么都不剩就别动，免得把 system prompt 整条清空。
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.to_string())
}

/// 上游失败体里是否带某个业务码（形如 `"code":11133` 或 `"code": 11133`）。
///
/// 🔴 不要直接 `text.contains("11128")`：`requestId` 是 32 位十六进制，
/// 天然可能撞出 `11128` / `11133` 这样的子串 —— 那样会把正常失败误判成特定错误码。
/// 锚在 `"code":` 上才可靠。
fn has_upstream_code(text: &str, code: &str) -> bool {
    text.contains(&format!("\"code\":{code}")) || text.contains(&format!("\"code\": {code}"))
}

/// 上游「模型侧瞬时故障」——官方文档把 `11133` / `11134` / `14003` 都归为
/// 「多与模型侧状态有关」，处理建议是「新开会话或切换模型重试」。
///
/// 🔴 这一类**不是**参数错误：2026-10-26 用 `glm-5.3-flash` 逐个试过
/// `stop` / `response_format` / `logprobs` / `seed` / `reasoning_effort` /
/// `stream_options` / `parallel_tool_calls` / 图片输入 / 80 个 tools /
/// 7.5 万字 system / `tool_call` 配对断裂，**全部 200**，单参数复现不出来。
/// 对照开源实现 `opencode-codebuddy-oauth`：它把 11133 当「服务端侧故障窗口」，
/// 按 1s→4s→10s→25s 退避幂等重发，同构请求稍后即成功。
///
/// ⇒ 正确处置是**重试**，而不是冷却账号 —— 账号和请求都没问题。
fn is_transient_model_error(text: &str) -> bool {
    transient_code(text).is_some()
}

/// 命中时返回具体是哪个业务码（用于给用户看「是哪个码」）。
fn transient_code(text: &str) -> Option<&'static str> {
    ["11133", "11134", "14003"]
        .into_iter()
        .find(|code| has_upstream_code(text, code))
}

/// 从上游失败体里抠出 `requestId`（官方叫 **Trace ID**，申诉时必填）。
///
/// 形如 `"requestId":"9d5bc5fd-3e04-4752-8e13-a8d4cf28767e"`。
/// 官方渠道明确要求把它写进工单/邮件标题，否则客服定位不到具体请求。
fn upstream_request_id(text: &str) -> Option<String> {
    let key = "\"requestId\":\"";
    let start = text.find(key)? + key.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    let id = &rest[..end];
    (!id.is_empty() && id.len() <= 64).then(|| id.to_string())
}

/// 上游是否把该账号判成**风控限制**（`11140`）。
///
/// 判据来自 2026-10-29 实测（国际版 5/5 可用号）：
///
/// ```text
/// 403 {"code":11140,"msg":"request illegal",
///      "displayMsg":{"en":"Your account has been restricted due to unusual activity"}}
/// ```
///
/// 关键鉴别：**同一个 token 读 `/v2/enterprises/personal/models` 仍 200**。
/// 所以它不是 token 失效（那会 401）、不是渠道校验（11128）、也不是 IP 风控
/// （国内版在同一条网络下 200 正常）——是**这个账号的对话能力被限制**。
///
/// 🔴 与 `11128 request illegal` 的区别：两者 `msg` 相同，**只能靠 `code` 分辨**。
/// 11128 改负载还有救；11140 改什么都没用，只能等或申诉。
fn is_account_restricted(text: &str) -> bool {
    has_upstream_code(text, "11140")
}

/// 模型侧瞬时故障的重试次数（退避见 [`MODEL_RETRY_BACKOFF_MS`]）。
const MAX_MODEL_RETRY: usize = 3;
/// 退避梯度：1s → 3s → 8s（合计 12s）。
///
/// 参考实现用的是 1/4/10/25s（总预算 ≤40s）。这里取更短的一档：
/// 反代是**同步阻塞**客户端的，压到 12s 以免客户端先超时断开，
/// 而实测这类故障窗口通常几秒内就恢复。
const MODEL_RETRY_BACKOFF_MS: [u64; MAX_MODEL_RETRY] = [1_000, 3_000, 8_000];

/// 上游的失败响应体是不是「渠道校验」拦截（`11128`）。
///
/// 只看响应体里有没有这个码 / 这句 msg —— 上游把它包在 `message` 字符串里再套一层
/// JSON，所以不能按结构化字段取，直接做子串匹配最稳。
///
/// 判定成它之后**绝不能冷却账号** —— 号是好的（读接口照样 200），
/// 冻它只会把整个账号池拖空。理由见 4xx 分支里的实测记录。
fn is_channel_block(text: &str) -> bool {
    // 🔴 必须先排除 11140：它的 msg 也是 `request illegal`（与 11128 一字不差），
    // 只按 msg 判会把「账号被风控限制」误当成「渠道校验」，于是去重试 + 自愈，
    // 在一个注定 403 的号上反复送请求。
    if is_account_restricted(text) {
        return false;
    }
    // 锚 `"code":` 而不是裸子串 —— `requestId` 是十六进制，可能撞出 11128。
    has_upstream_code(text, "11128")
        || text.contains("unapproved channel")
        || text.contains("request illegal")
}

/// 在上游原文里扫第一个 `YYYY-MM-DD HH:MM:SS`（按 UTC+8 解释）。
///
/// 🔴 **不用中文关键词定位**：上游的 JSON 会把中文转义成 `\uXXXX`，
/// `contains("将在")` / `find("频率限制")` 这类判断在转义体上全线失效
/// （v1.6.15 的解析就是这么失败的）。日期时间串是纯 ASCII，
/// 在任何转义形式下都保持原样。
fn find_reset_datetime(text: &str) -> Option<i64> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 19 {
        return None;
    }
    let offset = FixedOffset::east_opt(8 * 3600)?;
    for start in 0..=(chars.len() - 19) {
        let candidate: String = chars[start..start + 19].iter().collect();
        let b = candidate.as_bytes();
        if b[4] != b'-' || b[7] != b'-' || b[10] != b' ' || b[13] != b':' || b[16] != b':' {
            continue;
        }
        if let Ok(naive) = NaiveDateTime::parse_from_str(&candidate, "%Y-%m-%d %H:%M:%S") {
            if let Some(dt) = offset.from_local_datetime(&naive).single() {
                return Some(dt.timestamp_millis());
            }
        }
    }
    None
}

/// 解析上游「使用频率超限」响应，返回重置时刻（毫秒）。
///
/// 判定关键词保持 ASCII 优先：`60004` 是稳定错误码，数字不会被转义；
/// 中文 `频率限制` 只作为补充（未转义时才有用）。
fn rate_limit_reset_ms(text: &str) -> Option<i64> {
    if !(text.contains("60004") || text.contains("频率限制")) {
        return None;
    }
    find_reset_datetime(text)
}

/// 调试落盘开关：`~/.wb-switch/DEBUG_DUMP` **存在**时，把发往上游的真实请求体
/// 与失败响应写到 `~/.wb-switch/dump/`。
///
/// 🔴 用「开关文件」而不是环境变量：这样**不必改启动方式**（照常双击即可，
/// 免得从终端启动时继承到 `HTTP_PROXY` 之类变量），排查完删掉开关文件就停。
/// 关掉时这里只做一次 `Path::exists()`，开销可忽略。
///
/// 用途：某些 11128 只在真实客户端（DSH 等）的完整报文下复现，
/// 而我们无法从会话存储里看到实际发出的字段 —— 只能把它拦下来看。
fn debug_dump_enabled() -> bool {
    home_dir().join(".wb-switch").join("DEBUG_DUMP").exists()
}

fn debug_dump(name: &str, payload: &str) {
    if !debug_dump_enabled() {
        return;
    }
    let dir = home_dir().join(".wb-switch").join("dump");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(name), payload);
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
fn build_upstream_body(incoming: &Value, edition: Edition, degraded: &mut Vec<String>) -> Value {
    build_upstream_body_ex(incoming, edition, degraded, false)
}

/// `aggressive = true` 时启用**最大强度归一化**，用于 11128 自愈重试。
///
/// 只在上游以 `unapproved channel`（负载变体）打回后触发一次：
/// 把角色收敛到上游认识的四类、去掉可能泄露第三方 agent 身份的 `name` 字段、
/// 把内容块拍平成字符串。这些改动**都会改变请求形状**，所以不做默认行为。
fn build_upstream_body_ex(
    incoming: &Value,
    edition: Edition,
    degraded: &mut Vec<String>,
    aggressive: bool,
) -> Value {
    let model = incoming
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("deepseek-v4-flash")
        .to_string();
    let messages = incoming.get("messages").cloned().unwrap_or_else(|| json!([]));
    let mut sanitized = sanitize_messages(&messages, degraded);
    // 🔴 国内版必须**主动**清洗渠道指纹（2026-10-28 起按文本内容判）。
    // 放在 aggressive 之前：默认路径就要干净，而不是等撞了 11128 再补救。
    if edition == Edition::Domestic {
        neutralize_domestic_fingerprints(&mut sanitized, degraded);
    }
    if aggressive {
        sanitize_messages_aggressive(&mut sanitized, degraded);
    }
    ensure_leading_system(edition, &mut sanitized, degraded);
    let mut body = json!({
        "model": model,
        "messages": sanitized,
        "stream": true,
    });
    // 标准采样 / 输出控制参数：上游全接受，原样透传（见 PASSTHROUGH_PARAMS）。
    for key in PASSTHROUGH_PARAMS {
        if let Some(v) = incoming.get(*key) {
            body[*key] = v.clone();
        }
    }
    if let Some(t) = incoming.get("tools") {
        let mut tools = t.clone();
        if edition == Edition::Domestic {
            neutralize_domestic_tool_descriptions(&mut tools, degraded);
        }
        body["tools"] = tools;
    }
    // 🔴 `tool_choice` 必须**归一化**后再发：上游只吃字符串，
    // 收到 OpenAI 规范的对象形式会直接 11101（见 normalize_tool_choice）。
    if let Some(tc) = incoming.get("tool_choice") {
        match normalize_tool_choice(tc) {
            Some(norm) => {
                if norm != *tc {
                    degraded.push("tool-choice".to_string());
                }
                body["tool_choice"] = norm;
            }
            None => degraded.push("tool-choice-dropped".to_string()),
        }
    }
    // 🔴 思考强度：以前这里只转 model/messages/stream/temperature/top_p/max_tokens/tools，
    // 思考类字段**全被丢掉**，表现为「客户端调了推理等级、上游毫无反应、思考内容为空」。
    //
    // 各客户端拼写不同，反代不做翻译、原样透传（只有客户端真的发了才转，
    // 所以对不发这些字段的客户端零影响）：
    // - `reasoning_effort`（字符串）—— 上游真正认的就是它，实测见 API-PROXY.md
    // - `thinking`（对象或字符串，pi-ai 的 deepseek / string-thinking 格式都用它）
    // - `reasoning`（对象，openrouter / together / ant-ling 格式）
    // - `enable_thinking`（布尔，qwen 格式）
    // - `chat_template_kwargs`（对象，chat-template 格式）
    for key in REASONING_PASSTHROUGH {
        if let Some(v) = incoming.get(*key) {
            body[*key] = v.clone();
        }
    }
    body
}

/// 国际版上游要求**第一条消息必须是 `system`**，否则整条请求被拦。
///
/// 实测（2026-10-11，7864 → www.workbuddy.ai）：
/// - 首条是 `user` → `HTTP 400 {"code":11128,"msg":"first message is not system prompt"}`
/// - 同一个请求前面补一条 system → **200 正常返回**
///
/// 国内版没有这个要求（7863 上只发一条 user 是通的），所以只在国际版兜。
///
/// 客户端不一定带 system（探针、简单脚本、部分 OpenAI 兼容客户端都不带），
/// 兜一条默认的即可 —— 不带会被**整个**拦掉，比「多一句无害的 system」糟糕得多。
fn ensure_leading_system(edition: Edition, messages: &mut Value, degraded: &mut Vec<String>) {
    if edition != Edition::International {
        return;
    }
    let Some(arr) = messages.as_array_mut() else {
        return;
    };
    let already = arr
        .first()
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        .map(|r| r == "system")
        .unwrap_or(false);
    if already {
        return;
    }
    arr.insert(
        0,
        json!({"role": "system", "content": "You are a helpful assistant."}),
    );
    degraded.push("leading-system".to_string());
}

// ---------------------------------------------------------------- 处理

#[derive(Clone)]
struct AppState {
    /// 全局配置（主要是 `accounts` 白名单）。
    cfg: Arc<ProxyConfig>,
    /// 🔴 **两个入口共享同一个账号池** —— 否则同一个号会在两处各维护一份失败
    /// 冷却状态，出现「这边还在冷却、那边照用」的错乱。
    pool: Arc<Pool>,
    /// 🔴 本实例只服务这一个版本。
    edition: Edition,
    /// 本实例的接入点（监听地址 + **自己的** API Key）。
    endpoint: Arc<Endpoint>,
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": "invalid api key"}}))).into_response()
}

/// 校验请求头里的 key。`api_key` 是**本入口自己的**那把，不是全局的。
fn check_auth(headers: &HeaderMap, api_key: &str) -> bool {
    let want = api_key.trim();
    if want.is_empty() {
        return true;
    }
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .trim_start_matches("Bearer ")
        .trim();
    presented == want
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

/// `GET /v1/models` —— **只列本入口那一版的模型**。
///
/// 分开入口之后这里不再合并两档：客户端连的是国内版端口，就不该在列表里看到
/// 国际版专有的模型（否则它挑一个发过来，只会换来一个看不懂的 400）。
async fn list_models(State(st): State<AppState>) -> Response {
    match fetch_edition_models(&st.cfg, st.edition).await {
        Ok((ids, _, _)) => {
            let all = merge_extra_models(ids, &st.endpoint.extra_models);
            let data: Vec<Value> = all
                .iter()
                .map(|id| {
                    json!({
                        "id": id,
                        "object": "model",
                        "owned_by": owner_tag(st.edition),
                    })
                })
                .collect();
            Json(json!({"object": "list", "data": data})).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": {
                    "message": format!("没有拉到{}的模型列表 —— {e}", st.edition.label()),
                }
            })),
        )
            .into_response(),
    }
}

/// `/v1/models` 的最终清单 = **上游列表 + 本入口声明的 `extra_models`**。
///
/// 上游在前、额外的按声明顺序追加，去重（上游已经列过的不再重复）。
/// 为什么要这份补充，见 [`Endpoint::extra_models`] 的说明 —— 上游那个列表不完整。
fn merge_extra_models(mut upstream: Vec<String>, extra: &[String]) -> Vec<String> {
    for m in extra {
        let m = m.trim();
        if !m.is_empty() && !upstream.iter().any(|x| x == m) {
            upstream.push(m.to_string());
        }
    }
    upstream
}

// ---------------------------------------------------------------- 取模型列表（桌面端按钮用）

/// 从账号库里挑一个可用账号（必要时先刷新 token）。不参与轮转、不动失败冷却。
async fn pick_account_of(cfg: &ProxyConfig, edition: Option<Edition>) -> Option<Value> {
    // 名单取自**对应入口** —— 和 `eligible_accounts` 用同一套判据，别各写一份。
    let want = edition
        .map(|e| cfg.endpoint(e).accounts.clone())
        .unwrap_or_default();
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
    let client = cfg
        .apply_upstream_proxy(reqwest::Client::builder().timeout(Duration::from_secs(20)))
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
                // 和 `GET /v1/models` 保持同一口径：并上本入口声明的 `extra_models`。
                // 少了这一步，界面上「获取模型ID」显示的集合会和客户端实际拿到的不一致
                // （上游那个列表不完整，见 `Endpoint::extra_models`）。
                let ids = merge_extra_models(ids, &cfg.endpoint(*e).extra_models);
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
    /// 🔴 统计必须**按版本分开**：国内版入口只算国内版的量，国际版只算国际版的。
    /// 之前没有这个字段，两版混在同一个 `total` 里，界面上国际版页会显示国内号的用量。
    edition: Edition,
}

const USAGE_FILE: &str = "proxy-usage.json";
/// recent 列表最多保留多少条（**每版各一份**）。
const USAGE_RECENT_MAX: usize = 100;

fn usage_path() -> std::path::PathBuf {
    home_dir().join(".wb-switch").join(USAGE_FILE)
}

static USAGE_LOCK: Mutex<()> = Mutex::new(());

/// `Edition` → 存储里的键名。
fn edition_key(e: Edition) -> &'static str {
    match e {
        Edition::Domestic => "domestic",
        Edition::International => "international",
    }
}

/// 单个版本的用量文档（旧格式的扁平结构原样保留，前端不用大改）。
fn empty_edition_usage() -> Value {
    json!({
        "total": {"requests": 0, "errors": 0, "promptTokens": 0,
                  "completionTokens": 0, "credit": 0},
        "byAccount": {},
        "byModel": {},
        "recent": [],
    })
}

fn empty_usage() -> Value {
    json!({
        "updatedAt": 0,
        "byEdition": {
            "domestic": empty_edition_usage(),
            "international": empty_edition_usage(),
        },
    })
}

/// 把**旧格式**（两版混在一起）迁移成按版本分开。
///
/// 旧文件把两版混在一个 `total` / `byAccount` / `byModel` / `recent` 里，能拆的都拆：
///
/// - `byAccount` 的**键就是 uid** ⇒ 按账号档位精确归位；
/// - `recent` 每条也带 `uid` ⇒ 同样精确归位；
/// - `total` 不猜，直接由**该档的 byAccount 求和**得出（比估算准）；
/// - ⚠️ `byModel` 不含账号信息、**拆不开** ⇒ 只能丢弃（纯展示项，之后重新累积）。
///
/// 之后每次 `record_usage` 只往自己那一档写，`byEdition` 已存在时本函数直接返回。
fn migrate_legacy_usage(doc: Value) -> Value {
    if doc.get("byEdition").is_some() {
        return doc;
    }
    if doc.get("total").is_none() && doc.get("byAccount").is_none() {
        return empty_usage();
    }

    // uid → 档位（按当前账号库判定）
    let mut ed_of: HashMap<String, Edition> = HashMap::new();
    for a in load_accounts() {
        if let Some(uid) = a.get("uid").and_then(Value::as_str) {
            ed_of.insert(uid.to_string(), edition_of(&a));
        }
    }
    migrate_legacy_usage_with(doc, &ed_of)
}

/// [`migrate_legacy_usage`] 的实现体：`uid → 档位` 由调用方给，便于单测。
fn migrate_legacy_usage_with(doc: Value, ed_of: &HashMap<String, Edition>) -> Value {
    if doc.get("byEdition").is_some() {
        return doc;
    }
    if doc.get("total").is_none() && doc.get("byAccount").is_none() {
        return empty_usage();
    }

    let mut out = empty_usage();
    if let Some(t) = doc.get("updatedAt") {
        out["updatedAt"] = t.clone();
    }

    // ① byAccount 精确拆分
    if let Some(m) = doc.get("byAccount").and_then(Value::as_object) {
        for (uid, bucket) in m {
            let key = edition_key(ed_of.get(uid).copied().unwrap_or(Edition::Domestic));
            if let Some(dst) = out["byEdition"][key]["byAccount"].as_object_mut() {
                dst.insert(uid.clone(), bucket.clone());
            }
        }
    }

    // ② 每档 total = 该档 byAccount 求和
    for edition in Edition::ALL {
        let key = edition_key(edition);
        let mut acc = json!({"requests": 0i64, "errors": 0i64, "promptTokens": 0i64,
                             "completionTokens": 0i64, "credit": 0.0f64});
        if let Some(m) = out["byEdition"][key]["byAccount"].as_object() {
            for bucket in m.values() {
                for k in ["requests", "errors", "promptTokens", "completionTokens"] {
                    let v = acc[k].as_i64().unwrap_or(0)
                        + bucket.get(k).and_then(Value::as_i64).unwrap_or(0);
                    acc[k] = json!(v);
                }
                let c = acc["credit"].as_f64().unwrap_or(0.0)
                    + bucket.get("credit").and_then(Value::as_f64).unwrap_or(0.0);
                acc["credit"] = json!(c);
            }
        }
        out["byEdition"][key]["total"] = acc;
    }

    // ③ recent 按 uid 拆分（各自截断）
    if let Some(arr) = doc.get("recent").and_then(Value::as_array) {
        for item in arr {
            let uid = item.get("uid").and_then(Value::as_str).unwrap_or("");
            let key = edition_key(ed_of.get(uid).copied().unwrap_or(Edition::Domestic));
            if let Some(dst) = out["byEdition"][key]["recent"].as_array_mut() {
                if dst.len() < USAGE_RECENT_MAX {
                    dst.push(item.clone());
                }
            }
        }
    }

    out
}

/// 读统计文件；损坏或不存在就返回空结构。旧格式会被就地迁移。
pub fn load_usage() -> Value {
    let p = usage_path();
    if !p.is_file() {
        return empty_usage();
    }
    match std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        Some(v) => migrate_legacy_usage(v),
        None => empty_usage(),
    }
}

/// 取**某一版**的用量，形状与旧格式一致（`total` / `byAccount` / `byModel` / `recent`）。
pub fn load_usage_for(edition: Edition) -> Value {
    let doc = load_usage();
    let key = edition_key(edition);
    let mut out = doc
        .get("byEdition")
        .and_then(|v| v.get(key))
        .cloned()
        .unwrap_or_else(empty_edition_usage);
    out["updatedAt"] = doc.get("updatedAt").cloned().unwrap_or(json!(0));
    out
}

/// 清空统计。`edition` 为 `None` 时清**全部**（两版都清）。
pub fn reset_usage(edition: Option<Edition>) -> Result<(), String> {
    let _g = USAGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut doc = match edition {
        None => empty_usage(),
        Some(e) => {
            let mut d = load_usage();
            let key = edition_key(e);
            d["byEdition"][key] = empty_edition_usage();
            d
        }
    };
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
    // 🔴 只写自己那一档：国内版入口的量不会跑到国际版页面上（反之亦然）。
    let key = edition_key(meta.edition);
    if doc["byEdition"].get(key).is_none() {
        doc["byEdition"][key] = empty_edition_usage();
    }

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

    bump(&mut doc["byEdition"][key]["total"], prompt, completion, credit, ok);

    if let Some(m) = doc["byEdition"][key]["byAccount"].as_object_mut() {
        let e = m.entry(meta.uid.clone()).or_insert_with(|| {
            json!({"nickname": meta.nickname, "requests": 0, "errors": 0,
                   "promptTokens": 0, "completionTokens": 0, "credit": 0})
        });
        e["nickname"] = json!(meta.nickname);
        bump(e, prompt, completion, credit, ok);
    }
    if let Some(m) = doc["byEdition"][key]["byModel"].as_object_mut() {
        let e = m
            .entry(meta.model.clone())
            .or_insert_with(|| {
                json!({"requests": 0, "errors": 0, "promptTokens": 0,
                       "completionTokens": 0, "credit": 0})
            });
        bump(e, prompt, completion, credit, ok);
    }

    if let Some(arr) = doc["byEdition"][key]["recent"].as_array_mut() {
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
    if !check_auth(&headers, &st.endpoint.api_key) {
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

    // 🔴 版本防火墙：本入口只服务一个版本。模型名能**唯一确定**属于另一版时，直接拦下并
    // 说清楚该连哪个端口 —— 放它过去只会拿错版本的号去打上游，换回来一句
    // `11102 only available for authorized users`（实测）。
    //
    // 三种情况**不拦**，交给上游判定：
    // ① 谁都没见过的模型（新模型、还没拉过列表）；
    // ② 两版列表里**都**有的模型 —— 那说明两版都能用（`glm-5.3` / `hy3` / `lite` …）；
    // ③ 用户在本入口 `extra_models` 里**显式声明**的 —— 上游那个列表不完整，
    //    「没列出」≠「不能用」（`deepseek-v4.1-flash` 正是如此）。显式声明优先于自动推断。
    let declared_here = st.endpoint.extra_models.iter().any(|m| m.trim() == model);
    if !declared_here {
        if let Some(want) = model_edition(&model) {
            if want != st.edition {
                let msg = format!(
                    "模型 `{model}` 属于{}，而当前入口是{}（{}）。请改用{}入口（{}）访问。",
                    want.label(),
                    st.edition.label(),
                    st.endpoint.listen,
                    want.label(),
                    st.cfg.endpoint(want).listen,
                );
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": {"message": msg}})),
                )
                    .into_response();
            }
        }
    }

    let session = session_key(&incoming);
    let Some((mut acc, mut lease)) = take_account(
        &st.pool,
        &st.cfg,
        Some(st.edition),
        Some(model.as_str()),
        session.as_deref(),
    )
    .await
    else {
        let message = if st.pool.has_edition(st.edition) {
            format!(
                "{}的账号当前都不可用（冷却中，或需要重新登录）",
                st.edition.label()
            )
        } else {
            format!("参与轮转的账号里没有{}的号", st.edition.label())
        };
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": {"message": message}})),
        )
            .into_response();
    };
    let mut uid = acc
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let nickname = acc
        .get("nickname")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut meta = UsageMeta {
        uid: uid.clone(),
        nickname,
        model: model.clone(),
        stream: want_stream,
        edition: st.edition,
    };

    let client = match st.cfg.apply_upstream_proxy(reqwest::Client::builder()).build() {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    // 上游不认识的内容块（zcode 等客户端发的 file / document）在这里降级成文本，
    // 否则整条请求会被上游 400：`unsupported content type at index 0: file`。
    let mut degraded: Vec<String> = Vec::new();
    let upstream_body = build_upstream_body(&incoming, st.edition, &mut degraded);
    if !degraded.is_empty() {
        eprintln!(
            "[反代] 内容块降级 {} (model={model})",
            degraded_summary(&degraded)
        );
    }
    // 调试落盘（开关文件控制）：真实客户端报文与我们的复现差在哪，看这个。
    debug_dump(
        "last-request.json",
        &serde_json::to_string_pretty(&json!({
            "edition": st.edition.label(),
            "uid": &uid,
            "model": model,
            "degraded": degraded,
            "body": upstream_body,
        }))
        .unwrap_or_else(|_| "{}".to_string()),
    );

    // 🔴 发请求 + 「渠道校验软拦截」重试，见 MAX_CHANNEL_RETRY。
    // 重试时**重新生成**会话头族（chat_headers 里的 id 是每次调用现算的），
    // 免得同一个 request id 被上游当成重复包直接判掉。
    let mut channel_retry = 0usize;
    let mut transport_retry = 0usize;
    let mut model_retry = 0usize;
    // 换号重试次数：只在「换号可能解决」的错误上消耗（429 / 5xx）。
    let mut account_retry = 0usize;
    // 版本名在多处文案里要用（传输失败 / 渠道校验），提前取一次。
    let edition_label = st.edition.label();
    // 11128 自愈：第一次撞上 `unapproved channel`（负载变体）时，
    // 用最大强度归一化重造一份请求再试 —— 见 sanitize_messages_aggressive。
    let mut self_healed = false;
    let mut send_body = upstream_body.clone();
    let resp = loop {
        let mut req = client.post(chat_url(&acc));
        for (k, v) in chat_headers(&acc) {
            req = req.header(k, v);
        }
        let r = match req.json(&send_body).send().await {
            Ok(r) => r,
            Err(e) => {
                // 🔴 连接层失败：**不冷却账号**（原因见 MAX_TRANSPORT_RETRY）。
                // 退避重试几次应对瞬时抖动；仍失败就回 **502 + 可读原因**，
                // 由客户端自己决定要不要再发 —— 号本身没坏，别把它冻掉。
                if transport_retry < MAX_TRANSPORT_RETRY {
                    let wait = TRANSPORT_RETRY_BACKOFF_MS[transport_retry];
                    transport_retry += 1;
                    eprintln!(
                        "[反代] 上游连接失败，第 {transport_retry}/{MAX_TRANSPORT_RETRY} 次重试，\
                         等待 {wait}ms (model={model}, uid={uid}): {e}"
                    );
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    continue;
                }
                debug_dump(
                    "last-failure.txt",
                    &format!(
                        "transport-error\nuid={uid}\nmodel={model}\nedition={}\nretries={transport_retry}\n\n{e}",
                        st.edition.label(),
                    ),
                );
                record_usage(&meta, None, false);
                // 🔴 别把裸的 reqwest 原文回给客户端。
                // 原文形如 `error sending request for url (https://www.workbuddy.ai/...)`，
                // 客户端会原样打给用户看，而用户看到「workbuddy.ai + 502」第一反应就是
                // 「我的号是不是被封了」（2026-10-26 真实发生过）。
                // 这里明确写清：连不上 ≠ 被拒，账号没坏，怎么处理。
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": {
                            "message": format!(
                                "连不上上游（{edition_label}），已重试 {transport_retry} 次。\
                                 这是**网络层**失败：DNS、超时、TLS 重置或线路抖动，\
                                 不是账号被封、不是鉴权失败、也不是频率超限 —— 账号本身是好的。\n\
                                 反代走**直连**（不读系统代理），线路一抖就会落到这里。\
                                 处理办法：① 稍等几秒重发；② 若长期频繁出现，检查本机到 www.workbuddy.ai 的网络；\
                                 ③ 换国内版入口可绕开这条国际线路。\n\
                                 原始错误：{e}"
                            ),
                            "type": "upstream_unreachable",
                            "edition": edition_label,
                            "retries": transport_retry,
                        }
                    })),
                )
                    .into_response();
            }
        };
        if r.status().is_success() {
            // 出字即视为恢复正常：清掉风控计数与 5xx 连败，下次再被限从最短档重新起步。
            st.pool.clear_restrict(&uid);
            st.pool.mark_ok(&uid);
            break r;
        }
        let raw_status = r.status();
        let text = r.text().await.unwrap_or_default();

        // 🔴 429 / 5xx：**换号重试**（社区实现同样最多 3 次）。
        // 这两类都是「这个号这一刻不行」，换一个号往往立刻就好；
        // 参数错/内容被拦则相反 —— 换号只会把同一个 400 在别的号上再犯一次，
        // 所以那类不在这里处理，走下面的透传路径。
        let is_rate = raw_status.as_u16() == 429;
        let is_5xx = raw_status.is_server_error();
        if (is_rate || is_5xx) && account_retry < MAX_ACCOUNT_RETRY {
            account_retry += 1;
            if is_rate {
                let cool = st.pool.mark_rate_limited(&uid);
                eprintln!(
                    "[反代] 429 限流，账号冷却 {}s，换号重试 {}/{} (uid={uid})",
                    cool.as_secs(),
                    account_retry,
                    MAX_ACCOUNT_RETRY
                );
            } else if let Some(open) = st.pool.mark_server_error(&uid) {
                eprintln!(
                    "[反代] 5xx 连败达阈值，熔断 {}s，换号重试 {}/{} (uid={uid})",
                    open.as_secs(),
                    account_retry,
                    MAX_ACCOUNT_RETRY
                );
            }
            // 🔴 先拿到新号、再归还旧号：反过来写的话，一旦换号失败，
            // `lease` 已被 drop，后面的 streaming 分支就没有名额可归还了。
            // 先占后换最坏也只是短暂多占一个名额（在池子上限之内）。
            if let Some((next_acc, next_lease)) = take_account(
                &st.pool,
                &st.cfg,
                Some(st.edition),
                Some(model.as_str()),
                session.as_deref(),
            )
            .await
            {
                drop(lease);
                acc = next_acc;
                lease = next_lease;
                uid = lease.uid().to_string();
                meta.uid = uid.clone();
                meta.nickname = acc
                    .get("nickname")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                continue;
            }
            eprintln!("[反代] 429/5xx 后没有其他可用账号，直接返回上游错误 (uid={uid})");
        }
        // 🔴 模型侧瞬时故障（11133/11134/14003）：官方归类「模型侧状态」，
        // 重发即可能成功。**绝不能冷却账号** —— 见 is_transient_model_error。
        if is_transient_model_error(&text) && model_retry < MAX_MODEL_RETRY {
            let wait = MODEL_RETRY_BACKOFF_MS[model_retry];
            model_retry += 1;
            eprintln!(
                "[反代] 上游模型侧瞬时故障，第 {model_retry}/{MAX_MODEL_RETRY} 次重试，\
                 等待 {wait}ms (model={model}, uid={uid})"
            );
            tokio::time::sleep(Duration::from_millis(wait)).await;
            continue;
        }
        if is_channel_block(&text) {
            // 🔴 先试「自愈」：`unapproved channel` 是**负载**被判成非官方客户端，
            // 用最大强度归一化重造请求往往能直接过（`request illegal` 是客户端身份
            // 变体，改负载没用，所以只对前者做）。只做一次，避免死循环。
            if !self_healed && text.contains("unapproved channel") {
                self_healed = true;
                let mut heal_degraded: Vec<String> = Vec::new();
                let healed = build_upstream_body_ex(&incoming, st.edition, &mut heal_degraded, true);
                if healed != send_body {
                    eprintln!(
                        "[反代] 上游判负载不合规（11128 unapproved channel），启用最大强度\
                         归一化重试 (model={model}, uid={uid}): {}",
                        degraded_summary(&heal_degraded)
                    );
                    send_body = healed;
                    degraded.extend(heal_degraded);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            }
            if channel_retry < MAX_CHANNEL_RETRY {
                channel_retry += 1;
                // 退避：按次数递增，给上游的间歇判定留出恢复窗口。
                tokio::time::sleep(Duration::from_millis(300 * channel_retry as u64)).await;
                eprintln!(
                    "[反代] 上游渠道校验拦下，第 {channel_retry}/{MAX_CHANNEL_RETRY} 次重试 \
                     (model={model}, uid={uid})"
                );
                continue;
            }
        }
        // 重试仍被拦、或本来就不是渠道校验的失败 → 才当账号/上游故障处理。
        debug_dump(
            "last-failure.txt",
            &format!(
                "status={}\nuid={uid}\nmodel={model}\nedition={}\nretries={channel_retry}\n\n{text}",
                raw_status.as_u16(),
                st.edition.label(),
            ),
        );
        // 🔴 只对**账号级**失败冷却号（4xx：鉴权 / 额度 / 渠道校验）。
        // 5xx 是上游自己的问题（502 / 503 / 504 …），拿它去冻号只会把好号冻光 ——
        // 与连接层失败是同一类误判，会让整池无谓地空掉。
        let status = StatusCode::from_u16(raw_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let account_name = meta.nickname.clone();
        // edition_label 已在循环外取好，这里不再重复定义。
        // 🔴 60004 频率超限：按上游声明的重置时刻把**整个账号**暂停出池 ——
        // 120 秒的短冷却只会让它回池后立刻再 429，循环空转。同时给客户端一句
        // 人话：哪个账号、哪个模型、什么时候恢复、怎么继续用，
        // 替代之前那坨双层转义的原始 JSON。
        // 429 是限流的通用状态码，不同上游的错误码不一定都是 60004；
        // 「429 + 能解析出重置时间」同样按频率超限处理。
        let rate_limited_until = rate_limit_reset_ms(&text).or_else(|| {
            if raw_status.as_u16() == 429 {
                find_reset_datetime(&text)
            } else {
                None
            }
        });
        if let Some(until_ms) = rate_limited_until {
            // 长冷却记在**模型**键上：账号的其他模型仍可服务（上游自己也是
            // 这个口径 ——「您也可以切换其他模型继续使用」）。另外给一次账号级
            // 短冷却，把并发请求从「立刻又撞同一个失败号」里拉出来。
            st.pool.mark_model_failed_until(&uid, &model, until_ms);
            st.pool.mark_failed(&uid);
            let reset_at = Local
                .timestamp_millis_opt(until_ms)
                .single()
                .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|| "稍后".to_string());
            eprintln!(
                "[反代] 账号「{account_name}」的模型 {model} 频率超限，已停止对该账号使用该模型至 {reset_at} (uid={uid})"
            );
            record_usage(&meta, None, false);
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "error": {
                        "message": format!(
                            "账号「{account_name}」的模型 {model} 使用频率已超限，已自动停止对该账号使用这个模型，将于 {reset_at} 自动恢复。该账号的其他模型仍可继续使用 —— 换个模型即可，无需重新登录。"
                        ),
                        "type": "rate_limited",
                        "account": account_name,
                        "model": model,
                        "resetAt": reset_at,
                    }
                })),
            )
                .into_response();
        }
        // 本次实际生效的风控冷却时长，声明在分支外供下面的错误文案复用
        // （递增序列 3h/6h/12h/24h，每次不同）。
        let mut restricted_cool = Duration::from_secs(0);
        if raw_status.is_client_error() {
            // 🔴 11128 渠道校验**不能冷却账号**。
            //
            // 2026-10-17 实测（国际版 5 个号 + 国内版 3 个号，同 token 同 payload）：
            // - 同一个号 `GET /v2/enterprises/personal/models` → 200（token 有效）
            // - 同一个号 `POST /v2/chat/completions`         → 400 11128（每次都挂）
            // - 换 4 种 User-Agent（含官方 WorkBuddy/5.6.2）→ 全部 11128
            // - 首条补 system 的完全合规最小请求            → 依旧 11128
            // - 5 个国际版号 5/5 全挂；国内版 3/3 全 200
            // ⇒ 这是**上游在 chat 端点上的策略拦截**，与号、与请求形状、与 UA 都无关。
            //   冻号不会让它变好，只会把本来能读接口的号也从池里摘掉：
            //   以前一次坏请求就能把 5 个国际版号全冻 120 秒，
            //   客户端随即收到「国际版的账号当前都不可用」，而这 5 个号其实一个都没坏。
            let blocked = is_channel_block(&text);
            let transient = is_transient_model_error(&text);
            let restricted = is_account_restricted(&text);
            if restricted {
                // 🔴 风控限制：递增冷却 + 不再重试。用 120 秒冷却去等它，
                // 等于每两分钟再撞一次 403，既刷屏也可能加重风控评分。
                restricted_cool = st.pool.mark_restricted(&uid);
                eprintln!(
                    "[反代] 账号被上游风控限制（11140 unusual activity），\
                     本次冷却 {} 分钟不再选用 (model={model}, uid={uid})",
                    restricted_cool.as_secs() / 60
                );
            } else if blocked {
                eprintln!(
                    "[反代] 上游 chat 端点拒绝该请求（11128 渠道校验），**不冷却账号** \
                     —— 这是端点级策略拦截，账号本身有效 (model={model}, uid={uid})"
                );
            } else if transient {
                // 模型侧状态问题：重试 {MAX_MODEL_RETRY} 次仍失败，号还是好的，
                // 冻它只会让下一次请求换到一个同样会撞故障窗口的号。
                eprintln!(
                    "[反代] 上游模型侧故障（11133/11134/14003）重试 {model_retry} 次仍未恢复，\
                     **不冷却账号** (model={model}, uid={uid})"
                );
            } else {
                st.pool.mark_failed(&uid);
            }
        } else {
            eprintln!(
                "[反代] 上游 {} 失败（上游侧问题，不冷却账号）(model={model}, uid={uid})",
                raw_status.as_u16()
            );
        }
        record_usage(&meta, None, false);
        // 429 但解析不出重置时刻时，也别把原始 JSON 糊给客户端 —— 给一句能读的话。
        // 渠道校验（11128）同理：重试耗尽后原样透传只会让用户看到一坨双层转义的
        // JSON，根本不知道自己该做什么。这里给一句可执行的话。
        let message = if raw_status.as_u16() == 429 {
            format!(
                "账号「{account_name}」请求过于频繁（HTTP 429），已临时冷却该账号，请稍后重试。"
            )
        } else if is_transient_model_error(&text) {
            // 官方文档对 11133/11134/14003 的处置建议就是「新开会话或切换模型重试」。
            // 我们已经退避重试过 {model_retry} 次，仍未恢复。
            let code_label = transient_code(&text).unwrap_or("11133");
            format!(
                "上游模型侧暂时异常（code {code_label}），已自动退避重试 {model_retry} 次仍未恢复。\
                 这不是你的参数写错，也不是账号失效 —— 官方把这类码归为「模型侧状态」。\
                 建议：① 换个模型再发（最有效）；② 新开一个会话；③ 稍等十几秒重试。\
                 当前会话若已连续失败，换模型往往立刻恢复。"
            )
        } else if is_account_restricted(&text) {
            // 结论要说得直白：这是账号被限制，不是我们发得不对，重发也没用。
            // 恢复窗口与申诉渠道取自腾讯云开发者社区对 11140 的官方口径。
            let trace = upstream_request_id(&text)
                .map(|id| format!("\nTrace ID：{id}   ← 申诉时务必附上，客服靠它定位具体请求"))
                .unwrap_or_default();
            format!(
                "账号「{account_name}」被{edition_label}上游**风控限制**了\
                 （HTTP 403 / code 11140，上游原文：Your account has been restricted \
                 due to unusual activity）。{trace}\n\
                 判定依据：同一个 token 读模型清单仍返回 200（说明登录态有效、账号没被注销），\
                 只有对话接口被拦——这是账号级的使用限制，不是网络、不是参数、也不是频率超限。\n\
                 已把该账号冷却 {cooldown_h} 小时不再自动选用，到期会再试一次。\n\
                 实证补充（2026-10-29 本机）：换出口 IP 仍然 403，说明限制绑的是**账号、不是 IP**；\
                 且已有账号等待 **2 天** 仍未恢复 —— 所以别指望「等几小时就好」。\n\
                 能做的只有申诉：客户端「设置 → 帮助与反馈」提交工单，或发邮件到 \
                 workbuddy_ai@tencent.com，**标题和正文都要写上上面的 Trace ID**。\n\
                 期间建议先用国内版入口（国内版账号目前正常）。",
                cooldown_h = restricted_cool.as_secs() / 3600,
            )
        } else if is_channel_block(&text) {
            // 两种 11128 变体成因不同，提示也要分开 —— 混在一起用户无从下手：
            // - `unapproved channel`：**负载**被判成非官方客户端（角色/身份声明）
            // - `request illegal`   ：**调用方**被判成非官方客户端（客户端身份）
            if text.contains("unapproved channel") {
                format!(
                    "上游拒绝了这次请求：{edition_label}网关判定「负载不合规」\
                     （11128 Illegal API invocation from an unapproved channel）。\
                     已自动做最大强度归一化（角色收敛 + 去 name + 抹身份声明）重试仍被拦。\
                     这不是账号失效、不是频率超限，换账号也不会解决。\n\
                     这是腾讯风控在识别系统提示词里的**第三方 agent 身份声明**（典型是 \
                     `DeepSeek Harness`）。DSH 用户可关掉原生开关 `includeHarnessIdentity`，\
                     或把系统提示词里那段身份声明删掉，即可恢复。\n\
                     若已关掉仍报，请开启 `~/.wb-switch/DEBUG_DUMP` 后复现一次，\
                     把 `~/.wb-switch/dump/last-request.json` 发给作者定位。"
                )
            } else {
                format!(
                    "上游拒绝了这次调用：{edition_label}网关判定「调用方不是官方客户端」\
                     （11128 request illegal），已自动重试 {channel_retry} 次仍被拦。\
                     这不是账号失效，也不是频率超限 —— 该账号的只读接口仍然正常，换账号也不会解决。\
                     这是上游对非官方客户端的策略拦截，本地无法绕过。可以稍后重试；\
                     国内版入口通常比国际版宽松，必要时換用国内版。"
                )
            }
        } else {
            text
        };
        return (
            status,
            Json(json!({"error": {"message": message, "type": "upstream_error"}})),
        )
            .into_response();
    };

    if want_stream {
        // 不再原样透传：先过一层 SSE 重组器（见 sse_pump 的说明），
        // 否则上游「一两个词一帧」的粒度会让客户端把思考渲染成一片小方块。
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        // bytes_stream() 返回的 impl Stream 不保证 Unpin，必须 Box::pin 一下
        let upstream = Box::pin(resp.bytes_stream());
        // 🔴 把并发名额的所有权搬进任务体：流式响应会一直产出到 [DONE]，
        // 名额必须活到那一刻才归还。放在外层的话，函数一 return 名额就还了，
        // 并发上限等于没生效。
        tokio::spawn(async move {
            sse_pump(upstream, tx, meta).await;
            drop(lease);
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

/// `/status` —— 报告**本入口**的情况：它是哪一版、监听在哪、这一版有哪些号。
async fn proxy_status(State(st): State<AppState>) -> impl IntoResponse {
    let allowed = &st.endpoint.accounts;
    let items: Vec<Value> = load_accounts()
        .into_iter()
        .filter(|a| {
            // 只列属于本入口这一版的号 —— 另一版的号在这个端口上用不了。
            edition_of(a) == st.edition
                && (allowed.is_empty()
                    || a.get("uid")
                        .and_then(Value::as_str)
                        .map(|u| allowed.iter().any(|x| x == u))
                        .unwrap_or(false))
        })
        .map(|a| {
            let uid = a
                .get("uid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let cooling_ms = st.pool.cooling_for(&uid);
            json!({
                "uid": a.get("uid").and_then(Value::as_str),
                "nickname": a.get("nickname").and_then(Value::as_str),
                "needs_relogin": a.get("needs_relogin").and_then(Value::as_bool).unwrap_or(false),
                "expiresAt": a.get("expiresAt").and_then(Value::as_i64),
                // >0 表示该账号正在冷却（如频率超限被暂停），前端显示剩余时间
                "coolingMs": cooling_ms,
            })
        })
        .collect();
    Json(json!({
        "edition": st.edition.key(),
        "editionLabel": st.edition.label(),
        "enabled": st.endpoint.enabled,
        "listen": st.endpoint.listen,
        "hasApiKey": !st.endpoint.api_key.trim().is_empty(),
        "accounts": items
    }))
}

/// 本入口自己的用量统计（**只含本版**，不会把另一版的量混进来）。
async fn usage_stats(State(st): State<AppState>) -> Response {
    Json(load_usage_for(st.edition)).into_response()
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

/// 启动一个入口（阻塞，直到监听失败或进程退出）。
///
/// 每个入口一条独立线程 + 独立 Tokio 运行时，所以国内版与国际版互不干扰：
/// 一边起不来不会拖累另一边。但**账号池是共享的**（由调用方传入）。
async fn run_endpoint_until(
    edition: Edition,
    endpoint: Endpoint,
    pool: Arc<Pool>,
    cfg: Arc<ProxyConfig>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let addr: SocketAddr = endpoint
        .listen
        .parse()
        .map_err(|_| format!("监听地址无效: {}", endpoint.listen))?;
    // 这一版一个号都没有就别起了 —— 起来了也只会每次请求都 503。
    if !pool.has_edition(edition) {
        let msg = format!(
            "没有可用的{}账号：账号库为空、都需重新登录，或不在参与名单里",
            edition.label()
        );
        let _ = ready.send(Err(msg.clone()));
        return Err(msg);
    }
    let st = AppState {
        cfg,
        pool,
        edition,
        endpoint: Arc::new(endpoint),
    };
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            let msg = format!("监听 {addr} 失败: {e}");
            let _ = ready.send(Err(msg.clone()));
            return Err(msg);
        }
    };
    println!(
        "[反代] {} 已启动 http://{addr}  (POST /v1/chat/completions)",
        edition.label()
    );
    let _ = ready.send(Ok(()));
    axum::serve(listener, router(st))
        .with_graceful_shutdown(async {
            let _ = shutdown.await;
        })
        .await
        .map_err(|e| e.to_string())
}

/// 阻塞式启动**国内版**入口（留给「只用一次、不接 UI」的调用方）。
///
/// 桌面端请走 [`spawn_from_config`] —— 它会按配置把两个入口都起起来。
pub async fn run_proxy_server(cfg: ProxyConfig) -> Result<(), String> {
    let edition = Edition::Domestic;
    let endpoint = cfg.endpoint(edition).clone();
    let pool = Arc::new(Pool::new(eligible_accounts(&cfg)));
    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (ready_tx, _ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    run_endpoint_until(edition, endpoint, pool, Arc::new(cfg), shutdown_rx, ready_tx).await
}

/// 一个账号能不能进反代池。返回 `None` 表示可用，`Some(原因)` 表示不可用。
///
/// 🔴 **加密信封凭据必须排除**：WorkBuddy 5.6+ 会把 token 存成
/// `{$wbEncrypted, envelope}`。这种账号**只能用于切换客户端** —— 要出站就得拿明文
/// token，而信封解不开。放进来只会让反代在运行时返回「API 密钥无效」，
/// 而界面上它看起来是勾选可用状态（`d1c60311` 就踩过这一条）。
fn proxy_ineligible_reason(edition: Edition, a: &Value, allowed: &[String]) -> Option<&'static str> {
    if a.get("needs_relogin").and_then(Value::as_bool) == Some(true) {
        return Some("需要重新登录");
    }
    if edition_of(a) != edition {
        // 🔴 版本不匹配是最容易「勾了却不生效」的一条：勾选框里能看到它，
        // 但 `edition_of` 按 domain/edition 字段把它判到另一档，
        // 于是被静默剔除。国际版入口曾同时勾着两个国内版号（domain
        // `copilot.tencent.com`），实际只有 5 个号在跑，界面却显示 7 个。
        return Some("版本不匹配");
    }
    let Some(uid) = a.get("uid").and_then(Value::as_str) else {
        return Some("缺少 uid");
    };
    if !allowed.is_empty() && !allowed.iter().any(|x| x == uid) {
        return Some("未在本入口勾选");
    }
    if is_envelope(a, "access_token") {
        return Some("凭据加密存储，无法用于反代");
    }
    None
}

/// 按配置挑出参与反代的 uid（两个入口的**并集**，供两者共享的账号池使用）。
///
/// 每个版本的号只由**它自己那个入口**的名单决定 —— 国内版入口勾了谁就是谁，
/// 不会受国际版入口勾选的影响。
fn eligible_accounts(cfg: &ProxyConfig) -> Vec<(String, Edition)> {
    let all = load_accounts();
    let mut out: Vec<(String, Edition)> = Vec::new();
    for edition in Edition::ALL {
        let allowed = &cfg.endpoint(edition).accounts;
        for a in &all {
            if proxy_ineligible_reason(edition, a, allowed).is_some() {
                continue;
            }
            let Some(uid) = a.get("uid").and_then(Value::as_str) else {
                continue;
            };
            out.push((uid.to_string(), edition));
        }
    }
    out
}

/// 供桌面端调用：读配置，把**已启用**的入口都在后台起起来。
pub fn spawn_from_config() {
    let cfg = load_proxy_config();
    if !cfg.any_enabled() {
        return;
    }
    for (edition, outcome) in apply_config(&cfg) {
        if let Err(msg) = outcome {
            eprintln!("[反代] {} 入口启动失败: {msg}", edition.label());
        }
    }
}

// ---------------------------------------------------------------- 生命周期
//
// 说明：Tauri 的 setup() 与同步 command 都跑在**主线程**，那里没有 Tokio 运行时，
// 直接 `tokio::spawn` 会 panic。所以这里**每个入口自己开一条带独立运行时的线程**，
// 无论从启动流程还是从 command 调用都安全。
//
// 国内版与国际版各占一条，互不干扰；账号池由两者共享。

/// 一个入口的运行态。
struct EndpointRuntime {
    /// 记下来是为了状态查询 / 日志能说出「哪个地址在跑」。
    listen: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn runtimes() -> &'static Mutex<HashMap<Edition, EndpointRuntime>> {
    static R: OnceLock<Mutex<HashMap<Edition, EndpointRuntime>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 某个入口是否正在监听。
pub fn is_running(edition: Edition) -> bool {
    runtimes()
        .lock()
        .map(|m| m.contains_key(&edition))
        .unwrap_or(false)
}

/// 有没有**任何**入口在监听。
pub fn proxy_running() -> bool {
    runtimes().lock().map(|m| !m.is_empty()).unwrap_or(false)
}

/// 正在监听的入口 → 它的监听地址。界面用它展示「哪几个在跑」。
pub fn running_endpoints() -> Vec<(Edition, String)> {
    let mut v: Vec<(Edition, String)> = runtimes()
        .lock()
        .map(|m| m.iter().map(|(e, r)| (*e, r.listen.clone())).collect())
        .unwrap_or_default();
    v.sort_by_key(|(e, _)| e.key());
    v
}

/// 起一个入口（非阻塞）。
fn start_endpoint(
    edition: Edition,
    endpoint: Endpoint,
    pool: Arc<Pool>,
    cfg: Arc<ProxyConfig>,
) -> Result<(), String> {
    if is_running(edition) {
        return Err(format!("{}入口已经在运行", edition.label()));
    }
    // 先探地址合法性，避免后台线程里默默失败。
    endpoint
        .listen
        .parse::<SocketAddr>()
        .map_err(|_| format!("监听地址无效: {}", endpoint.listen))?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let listen = endpoint.listen.clone();

    let thread = std::thread::Builder::new()
        .name(format!("wb-api-proxy-{}", edition.key()))
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("创建运行时失败: {e}")));
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(e) =
                    run_endpoint_until(edition, endpoint, pool, cfg, shutdown_rx, ready_tx).await
                {
                    eprintln!("[反代] {} 运行结束: {e}", edition.label());
                }
            });
        })
        .map_err(|e| format!("无法启动反代线程: {e}"))?;

    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => {
            if let Ok(mut m) = runtimes().lock() {
                m.insert(
                    edition,
                    EndpointRuntime {
                        listen,
                        shutdown: Some(shutdown_tx),
                        thread: Some(thread),
                    },
                );
            }
            Ok(())
        }
        Ok(Err(e)) => {
            let _ = shutdown_tx.send(());
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = shutdown_tx.send(());
            Err("启动超时：10 秒内没有就绪".to_string())
        }
    }
}

/// 停掉全部入口（阻塞到线程退出）。
pub fn stop_proxy_server() {
    let taken: Vec<(Edition, EndpointRuntime)> = match runtimes().lock() {
        Ok(mut m) => m.drain().collect(),
        Err(_) => return,
    };
    for (edition, mut rt) in taken {
        if let Some(tx) = rt.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(th) = rt.thread.take() {
            let _ = th.join();
        }
        println!("[反代] {} 已停止", edition.label());
    }
}

/// 按配置重建两个入口。返回**每个已启用版本**的启动结果。
///
/// 刻意做成「逐版本返回」而不是一个总的 Result：国际版没账号属于常态，
/// 不该因此让国内版也起不来，更不该让「保存配置」这个动作整体失败。
pub fn apply_config(cfg: &ProxyConfig) -> Vec<(Edition, Result<(), String>)> {
    stop_proxy_server();
    let pool = Arc::new(Pool::new(eligible_accounts(cfg)));
    let cfg = Arc::new(cfg.clone());
    let mut out = Vec::new();
    for edition in Edition::ALL {
        let endpoint = cfg.endpoint(edition).clone();
        if !endpoint.enabled {
            continue; // 未启用 = 不起，也不算失败
        }
        let r = start_endpoint(edition, endpoint, pool.clone(), cfg.clone());
        out.push((edition, r));
    }
    out
}

/// 兼容旧调用名：按当前配置重启。
///
/// 任一入口失败都会汇总进 Err（但**不影响另一个**已经起来的）。
/// 调用方若要逐版本判断，直接用 [`apply_config`]。
pub fn restart_proxy_server() -> Result<(), String> {
    let cfg = load_proxy_config();
    let errs: Vec<String> = apply_config(&cfg)
        .into_iter()
        .filter_map(|(e, r)| r.err().map(|m| format!("{}：{m}", e.label())))
        .collect();
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("；"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 60004 的双层转义 JSON 必须能解析出重置时刻 —— 这是「按重置时间
    /// 暂停账号」而不是「120 秒后回池再 429」的依据。
    #[test]
    fn parses_rate_limit_reset_from_nested_json() {
        let body = r#"{"message":"{\"code\":60004,\"msg\":\"您的使用量已超出频率限制，将在 2026-10-17 11:46:56 UTC+8 重置，您也可以切换其他模型继续使用。\",\"requestId\":\"00000000000000463270d2d3db8ba1\"}","type":"upstream_error"}"#;
        let until = rate_limit_reset_ms(body).expect("60004 应解析出重置时刻");
        let expected = chrono::FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .from_local_datetime(
                &chrono::NaiveDateTime::parse_from_str(
                    "2026-10-17 11:46:56",
                    "%Y-%m-%d %H:%M:%S",
                )
                .unwrap(),
            )
            .unwrap()
            .timestamp_millis();
        assert_eq!(until, expected);
    }

    /// 非频率超限的上游体不得误判 —— 否则普通 4xx 会被错误地长时间冷却。
    #[test]
    fn non_rate_limit_body_returns_none() {
        assert!(rate_limit_reset_ms(r#"{"message":"{\"code\":11128}"}"#).is_none());
        assert!(rate_limit_reset_ms("plain text").is_none());
        assert!(rate_limit_reset_ms("").is_none());
        // 有 60004 关键字但没有任何时间串的残缺体也要兜住。
        assert!(rate_limit_reset_ms(r#"{"message":"{\"code\":60004,\"msg\":\"?\"}"}"#).is_none());
    }

    /// 🔴 中文被转义成 `\uXXXX` 时（上游实际就是这么发的），解析仍必须成功。
    /// v1.6.15 用中文关键词 `频率限制` 定位，在转义体上必然失败 ⇒ 原始 JSON 透传给客户端。
    #[test]
    fn parses_reset_time_even_when_chinese_is_escaped() {
        let body = r#"{"code":60004,"msg":"\u60a8\u7684\u4f7f\u7528\u91cf\u5df2\u8d85\u51fa\u9891\u7387\u9650\u5236\uff0c\u5c06\u5728 2026-10-17 11:46:56 UTC+8 \u91cd\u7f6e"}"#;
        let until = rate_limit_reset_ms(body).expect("转义体也必须能解析出重置时刻");
        let expected = FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .from_local_datetime(
                &NaiveDateTime::parse_from_str("2026-10-17 11:46:56", "%Y-%m-%d %H:%M:%S")
                    .unwrap(),
            )
            .unwrap()
            .timestamp_millis();
        assert_eq!(until, expected);
    }

    /// 时间串扫描本身要稳：残破日期不算，正常日期能取到。
    #[test]
    fn find_reset_datetime_validates_shape() {
        assert!(find_reset_datetime("no time here").is_none());
        assert!(find_reset_datetime("2026-13-40 99:99:99").is_none());
        assert!(find_reset_datetime("重置时间 2026-10-17 11:46:56 UTC+8").is_some());
    }

    /// system prompt 末尾的环境注入块会被摘掉——上游的渠道校验（11128）正是靠
    /// 识别这段判定「非官方客户端」。见 `strip_env_injection`。
    #[test]
    fn strips_git_env_injection_from_system_prompt() {
        let raw = "You are ZCode.\n\n# Rules\nBe brief.\n\n\
                   gitStatus: clean\n\nCurrent branch: main\n\nGit user: someone\n";
        let cleaned = strip_env_injection(raw).expect("应当能摘出环境块");
        assert_eq!(cleaned, "You are ZCode.\n\n# Rules\nBe brief.");
        assert!(!cleaned.contains("gitStatus:"), "gitStatus 块必须整块消失");
        assert!(!cleaned.contains("Git user:"), "紧跟其后的环境行要一起摘掉");
    }

    /// 没有环境块时原样放行；整条都是环境块时也不动（不能把 system 清空）。
    #[test]
    fn env_strip_is_noop_when_nothing_to_strip() {
        assert!(strip_env_injection("You are WorkBuddy. Be helpful.").is_none());
        assert!(
            strip_env_injection("gitStatus: clean").is_none(),
            "全是要摘的内容就干脆别动，免得 system 变空"
        );
    }

    /// 旧格式（两版混在一起）迁移后必须**按版本分开**：
    /// byAccount / recent 按 uid 精确归位，每档 total 由该档 byAccount 求和。
    #[test]
    fn legacy_usage_splits_by_edition() {
        let mut ed_of = HashMap::new();
        ed_of.insert("u-cn".to_string(), Edition::Domestic);
        ed_of.insert("u-ai".to_string(), Edition::International);

        let legacy = json!({
            "updatedAt": 111,
            // 旧字段（混合），迁移时应被丢弃/不用
            "total": {"requests": 999, "errors": 9, "promptTokens": 9, "completionTokens": 9, "credit": 9.0},
            "byAccount": {
                "u-cn": {"nickname": "国内号", "requests": 3, "errors": 1,
                         "promptTokens": 100, "completionTokens": 10, "credit": 1.5},
                "u-ai": {"nickname": "国际号", "requests": 7, "errors": 2,
                         "promptTokens": 200, "completionTokens": 20, "credit": 2.5},
            },
            "byModel": {"m": {"requests": 10}},
            "recent": [
                {"ts": 2, "uid": "u-ai", "model": "m", "ok": true},
                {"ts": 1, "uid": "u-cn", "model": "m", "ok": false},
            ],
        });

        let out = migrate_legacy_usage_with(legacy, &ed_of);
        assert_eq!(out["updatedAt"], 111);

        let cn = &out["byEdition"]["domestic"];
        let ai = &out["byEdition"]["international"];

        // byAccount 精确归位
        assert_eq!(cn["byAccount"]["u-cn"]["requests"], 3);
        assert!(cn["byAccount"].get("u-ai").is_none(), "国际号不能落到国内档");
        assert_eq!(ai["byAccount"]["u-ai"]["requests"], 7);

        // total 由各自 byAccount 求和，而不是照抄旧的混合 total
        assert_eq!(cn["total"]["requests"], 3);
        assert_eq!(cn["total"]["promptTokens"], 100);
        assert_eq!(cn["total"]["credit"], 1.5);
        assert_eq!(ai["total"]["requests"], 7);
        assert_eq!(ai["total"]["promptTokens"], 200);
        assert_eq!(ai["total"]["credit"], 2.5);

        // recent 按 uid 拆开
        assert_eq!(cn["recent"].as_array().unwrap().len(), 1);
        assert_eq!(cn["recent"][0]["uid"], "u-cn");
        assert_eq!(ai["recent"].as_array().unwrap().len(), 1);
        assert_eq!(ai["recent"][0]["uid"], "u-ai");

        // 拆不开的 byModel 不保留（纯展示项）
        assert!(cn["byModel"].as_object().unwrap().is_empty());
        assert!(ai["byModel"].as_object().unwrap().is_empty());
    }

    /// 已经是新格式时迁移必须是空操作（幂等），且不误判损坏文件。
    #[test]
    fn usage_migration_is_idempotent_and_safe() {
        let mut ed_of = HashMap::new();
        ed_of.insert("u-cn".to_string(), Edition::Domestic);

        let already = json!({
            "updatedAt": 5,
            "byEdition": {
                "domestic": {"total": {"requests": 1}, "byAccount": {}, "byModel": {}, "recent": []},
                "international": {"total": {"requests": 2}, "byAccount": {}, "byModel": {}, "recent": []},
            },
        });
        let out = migrate_legacy_usage_with(already, &ed_of);
        assert_eq!(out["byEdition"]["international"]["total"]["requests"], 2);

        // 空文件 / 损坏文件 → 空结构，不 panic
        let empty = migrate_legacy_usage_with(json!({}), &ed_of);
        assert_eq!(empty["byEdition"]["domestic"]["total"]["requests"], 0);
    }

    /// 渠道校验软拦截（11128）要能被认出来 —— 它是**可重试**的，认错就会白冻一个号。
    ///
    /// 真实响应体长这样（上游把内层 JSON 当字符串塞进 `message`）：
    /// `400: {"message":"{\"code\":11128,\"msg\":\"Illegal API invocation from an
    /// unapproved channel\",...}","type":"upstream_error"}`
    /// 🔴 业务码必须锚在 `"code":` 上。
    ///
    /// `requestId` 是 32 位十六进制，天然可能撞出 `11128` / `11133` 之类的子串；
    /// 用裸 `contains` 会把别的错误误判成渠道校验 / 模型侧故障。
    #[test]
    fn upstream_code_must_be_anchored_not_substring() {
        // requestId 里含 11128，但真实业务码是 11133 → 不是渠道校验
        let sneaky = r#"{"code":11133,"msg":"x","requestId":"111280000000000000046302613c7e69"}"#;
        assert!(has_upstream_code(sneaky, "11133"));
        assert!(!has_upstream_code(sneaky, "11128"), "不能因为 requestId 里有 11128 就误判");
        assert!(!is_channel_block(sneaky), "11133 不应被当成渠道校验");

        // 带空格 / 带不带引号的常见写法都要认
        assert!(has_upstream_code(r#"{"code": 11133}"#, "11133"));
        assert!(has_upstream_code(r#"{"code":11133}"#, "11133"));
        assert!(!has_upstream_code(r#"{"msg":"11133"}"#, "11133"), "只是 msg 里出现数字不算");
    }

    /// 11133 / 11134 / 14003 属于「模型侧瞬时故障」，靠重试解决，不是账号问题。
    #[test]
    fn transient_model_errors_are_recognized() {
        let real = r#"{"code":11133,"msg":"the request parameters were rejected by the model provider","extError":{"code":"1210"}}"#;
        assert!(is_transient_model_error(real));
        assert_eq!(transient_code(real), Some("11133"));
        assert!(is_transient_model_error(r#"{"code":11134}"#));
        assert!(is_transient_model_error(r#"{"code":14003}"#));
        // 别的码不能落进这一类
        assert!(!is_transient_model_error(r#"{"code":11128,"msg":"request illegal"}"#));
        assert!(!is_transient_model_error(r#"{"code":60004,"msg":"频率限制"}"#));
        assert!(!is_transient_model_error(r#"{"code":11102}"#));
        assert_eq!(transient_code(r#"{"code":11102}"#), None);
    }

    /// 🔴🔴 出站头必须同时带齐三个会话 ID。
    ///
    /// 国际版渠道校验只查**存在性**：缺 `X-Conversation-ID` 时，同一个号、同一套
    /// 其余头、同一个 payload 会被稳定判成 `11128 request illegal`；
    /// 补上它立刻 200（2026-10-26 实测，见 chat_headers 里的对照表）。
    ///
    /// 这条测试就是为了防止将来「精简头族」时又把它删掉 —— 这个缺失曾让
    /// 国际版整整 9 天全线不可用，而且现场表现极易被误读成「上游封号」。
    #[test]
    fn chat_headers_carry_all_three_conversation_ids() {
        let acc = json!({
            "access_token": "t",
            "uid": "u",
            "domain": "www.workbuddy.ai",
            "edition": "international"
        });
        let h = chat_headers(&acc);
        for key in [
            "X-Conversation-ID",
            "X-Conversation-Request-ID",
            "X-Conversation-Message-ID",
        ] {
            let v = h.get(key).unwrap_or_else(|| panic!("{key} 必须存在"));
            assert!(!v.is_empty(), "{key} 不能为空");
        }
        // 会话键与消息键语义不同，不该是同一个值
        assert_ne!(
            h["X-Conversation-Request-ID"], h["X-Conversation-Message-ID"],
            "会话请求 ID 与消息 ID 不应相同"
        );
    }

    /// 🔴 `11140` 与 `11128` 的 `msg` **完全一样**（都是 `request illegal`），
    /// 只能靠 `code` 分辨，且两者的处置相反：
    /// - 11128：改负载可能过（渠道校验）
    /// - 11140：改什么都没用（账号被风控限制），必须长冷却
    #[test]
    fn restricted_11140_is_distinguished_from_channel_11128() {
        let restricted = r#"{"code":11140,"msg":"request illegal","displayMsg":{"en":"Your account has been restricted due to unusual activity"}}"#;
        let channel = r#"{"code":11128,"msg":"request illegal","displayMsg":{"zh":"这次调用没通过渠道校验"}}"#;

        assert!(is_account_restricted(restricted), "11140 必须识别为风控限制");
        assert!(!is_account_restricted(channel), "11128 不能被误判成风控限制");

        // 反向：11140 不属于渠道校验，也不属于模型侧瞬时故障
        assert!(!is_channel_block(restricted), "11140 不该被当成 11128");
        assert!(!is_transient_model_error(restricted), "11140 不该被当成重试类");

        // 别的码都不算
        assert!(!is_account_restricted(r#"{"code":11133}"#));
        assert!(!is_account_restricted(r#"{"code":401,"msg":"unauthorized"}"#));
        assert!(!is_account_restricted(""));
    }

    /// 风控冷却必须**递增**且封顶，成功出字后清零重来。
    ///
    /// 为什么这条值得测：固定 3 小时的做法已被实测证伪（等 2 天不恢复），
    /// 若有人把它改回定值，或者递增算错（比如永远 3h），都会让受限账号被反复白撞。
    #[test]
    fn restricted_cooldown_escalates_then_caps() {
        let pool = Pool::new(vec![("u1".to_string(), Edition::International)]);
        let h = |d: Duration| d.as_secs() / 3600;

        assert_eq!(h(pool.mark_restricted("u1")), 3, "第 1 次 3 小时");
        assert_eq!(h(pool.mark_restricted("u1")), 6, "第 2 次 6 小时");
        assert_eq!(h(pool.mark_restricted("u1")), 12, "第 3 次 12 小时");
        assert_eq!(h(pool.mark_restricted("u1")), 24, "第 4 次 24 小时");
        assert_eq!(h(pool.mark_restricted("u1")), 24, "第 5 次仍是 24 小时（封顶）");

        // 受限期间不选它
        assert!(pool.next_uid_of(Some(Edition::International), None).is_none());

        // 恢复后清零：再被限从头（3 小时）开始
        pool.clear_restrict("u1");
        assert_eq!(
            pool.next_uid_of(Some(Edition::International), None).as_deref(),
            Some("u1"),
            "清零后应重新可选"
        );
        assert_eq!(h(pool.mark_restricted("u1")), 3, "清零后回到最短档");
    }

    /// 申诉要 Trace ID：上游的 `requestId` 就是它，必须能从失败体里抠出来。
    #[test]
    fn extracts_upstream_trace_id_for_appeal() {
        let real = r#"{"code":11140,"msg":"request illegal","requestId":"9d5bc5fd-3e04-4752-8e13-a8d4cf28767e","displayMsg":{"en":"..."}}"#;
        assert_eq!(
            upstream_request_id(real).as_deref(),
            Some("9d5bc5fd-3e04-4752-8e13-a8d4cf28767e"),
            "必须原样取出 requestId（Trace ID）"
        );
        // 没有该字段 / 空体 → None，不能 panic
        assert_eq!(upstream_request_id(r#"{"code":11128}"#), None);
        assert_eq!(upstream_request_id(""), None);
        // 值为空串也不算有效
        assert_eq!(upstream_request_id(r#"{"requestId":""}"#), None);
    }

    /// 国内版渠道指纹必须**主动**清洗，且边界严格对齐上游实测判据。
    ///
    /// 上游（2026-10-28 起）扫的是 `system` / `developer` / `assistant` 与工具描述；
    /// `user` / `tool` 里的同名词**不触发**，所以一个字都不能动 —— 改了就是篡改用户内容。
    #[test]
    fn domestic_neutralizes_fingerprints_but_leaves_user_content_alone() {
        let incoming = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "You are an AI agent powered by DeepSeek Harness. Mention @ here."},
                {"role": "user", "content": "请解释 @ 的用法"},
                {"role": "assistant", "content": "按 DeepSeek Harness 文档"},
                {"role": "tool", "content": "result mentions DeepSeek Harness"}
            ],
            "tools": [{
                "type": "function",
                "function": {"name": "f", "description": "built on DeepSeek Harness"}
            }]
        });

        let mut degraded = Vec::new();
        let out = build_upstream_body(&incoming, Edition::Domestic, &mut degraded);
        let msg = |i: usize| out["messages"][i]["content"].as_str().unwrap_or("").to_string();

        // system：品牌名缩成 DeepSeek、裸 @ 换全角
        let sys = msg(0);
        assert!(!sys.contains("DeepSeek Harness"), "system 品牌名应缩掉: {sys}");
        assert!(sys.contains("powered by DeepSeek."), "应缩成 DeepSeek 而非删掉: {sys}");
        assert!(sys.contains("＠"), "system 里的裸 @ 应换全角: {sys}");

        // assistant 历史同样被扫（否则下一轮复发）
        let asst = msg(2);
        assert!(!asst.contains("DeepSeek Harness"), "assistant 历史也要清: {asst}");

        // 工具描述：也清
        let desc = out["tools"][0]["function"]["description"].as_str().unwrap();
        assert!(!desc.contains("DeepSeek Harness"), "工具描述要清: {desc}");

        // 🔴 user / tool 一个字都不能动
        assert_eq!(msg(1), "请解释 @ 的用法", "user 内容必须原样");
        assert_eq!(msg(3), "result mentions DeepSeek Harness", "tool 内容必须原样");

        // 国际版不做这套清洗（上游判据不同，无谓改动）
        let mut d2 = Vec::new();
        let intl = build_upstream_body(&incoming, Edition::International, &mut d2);
        let intl_sys = intl["messages"][0]["content"].as_str().unwrap();
        assert!(
            intl_sys.contains("DeepSeek Harness"),
            "国际版不该被国内版的规则清洗"
        );
    }

    /// 会话粘性：同一会话必须固定同一个号；换会话才重新分配。
    ///
    /// 这条是风控的核心防线之一 —— 同一段对话在多号之间跳，等于告诉上游
    /// 「这个账号在多个地点被共享使用」。
    #[test]
    fn session_sticks_to_one_account() {
        let pool = Arc::new(Pool::new(vec![
            ("u1".to_string(), Edition::Domestic),
            ("u2".to_string(), Edition::Domestic),
        ]));

        let g1 = pool.pick(Some(Edition::Domestic), None, Some("sess-A")).unwrap();
        let first = g1.uid().to_string();
        drop(g1);

        // 同一会话再来两次：必须还是同一个号（cursor 已经转过也不许变）
        for _ in 0..2 {
            let g = pool.pick(Some(Edition::Domestic), None, Some("sess-A")).unwrap();
            assert_eq!(g.uid(), first, "同一会话应粘住同一个账号");
        }

        // 换一个会话：应分到另一个号（两号都在池里）
        let gb = pool.pick(Some(Edition::Domestic), None, Some("sess-B")).unwrap();
        assert_ne!(gb.uid(), first, "不同会话应分散到不同账号");
    }

    /// 并发上限：单账号占满后不再放行，归还后恢复。
    #[test]
    fn inflight_cap_blocks_then_recovers() {
        let pool = Arc::new(Pool::new(vec![(
            "u1".to_string(),
            Edition::International,
        )]));
        let a = pool.pick(Some(Edition::International), None, None);
        let b = pool.pick(Some(Edition::International), None, None);
        assert!(a.is_some() && b.is_some(), "国际版上限 2，前两个应放行");
        assert!(
            pool.pick(Some(Edition::International), None, None).is_none(),
            "第三个必须被并发上限挡下"
        );
        drop(a);
        assert!(
            pool.pick(Some(Edition::International), None, None).is_some(),
            "归还名额后应恢复"
        );
    }

    /// 429 冷却：首次数 600s；**已在冷却中不翻倍**（防用户狂点把冷却堆厚）。
    #[test]
    fn rate_limit_backoff_does_not_stack_while_cooling() {
        let pool = Pool::new(vec![("u1".to_string(), Edition::Domestic)]);
        let d1 = pool.mark_rate_limited("u1");
        assert_eq!(d1.as_secs(), 600, "首次 600s");
        let d2 = pool.mark_rate_limited("u1");
        assert!(
            d2 <= d1,
            "冷却期内再次 429 不能翻倍（否则越点越久）: {d2:?} > {d1:?}"
        );
        assert!(d2.as_secs() > 0, "剩余冷却应大于 0");
    }

    /// 5xx 熔断：连 3 次才断，断开时长 30 分钟，成功后复位。
    #[test]
    fn five_xx_opens_breaker_after_three() {
        let pool = Pool::new(vec![("u1".to_string(), Edition::Domestic)]);
        assert!(pool.mark_server_error("u1").is_none(), "第 1 次不断");
        assert!(pool.mark_server_error("u1").is_none(), "第 2 次不断");
        let open = pool.mark_server_error("u1").expect("第 3 次应熔断");
        assert_eq!(open.as_secs(), 30 * 60);
        assert!(
            pool.next_uid_of(Some(Edition::Domestic), None).is_none(),
            "熔断期间不选它"
        );
        pool.mark_ok("u1");
        assert_eq!(
            pool.next_uid_of(Some(Edition::Domestic), None).as_deref(),
            Some("u1"),
            "成功出字后应复位"
        );
    }

    /// 出网代理的取值口径：空 / direct / none 一律视为直连。
    ///
    /// 这条不能想当然：如果 `""` 被当成「有一个空的代理地址」交给 reqwest，
    /// 要么建 client 失败，要么静默退回环境变量代理 —— 两种都很难查。
    #[test]
    fn upstream_proxy_blank_means_direct() {
        // 环境变量优先级低于配置；这里只测配置本身的解析口径。
        let mut cfg = ProxyConfig::default();
        assert_eq!(cfg.effective_upstream_proxy(), None, "默认必须是直连");

        for blank in ["", "   ", "direct", "NONE"] {
            cfg.upstream_proxy = Some(blank.to_string());
            assert_eq!(
                cfg.effective_upstream_proxy(),
                None,
                "{blank:?} 应视为直连"
            );
        }

        cfg.upstream_proxy = Some("  http://127.0.0.1:7897  ".to_string());
        assert_eq!(
            cfg.effective_upstream_proxy().as_deref(),
            Some("http://127.0.0.1:7897"),
            "正常地址应去掉首尾空白后返回"
        );
    }

    #[test]
    fn detects_channel_block_from_upstream_body() {
        let real = r#"400: {"message":"{\"code\":11128,\"msg\":\"Illegal API invocation from an unapproved channel\",\"requestId\":\"000000000000000046302613c7e69e4d\"}","type":"upstream_error"}"#;
        assert!(is_channel_block(real), "真实响应体必须命中");

        // 只带码、或只带 msg 的变体也要认。
        assert!(is_channel_block(r#"{"code":11128}"#));
        assert!(is_channel_block("Illegal API invocation from an unapproved channel"));

        // 🔴 不能误伤：别的错误码（含恰好包含 11128 之外的数字串）都算普通失败。
        assert!(!is_channel_block(r#"{"code":11102,"msg":"service info not found"}"#));
        assert!(!is_channel_block(r#"{"code":11101,"msg":"unsupported content type"}"#));
        assert!(!is_channel_block(""));
    }

    /// 净化只作用于 system 消息，user / assistant 的内容一个字都不能少。
    #[test]
    fn env_strip_only_touches_system_role() {
        let mut degraded = Vec::new();
        let msgs = json!([
            { "role": "system", "content": "Rules.\n\ngitStatus: clean\n\nGit user: x\n" },
            { "role": "user", "content": "gitStatus: 用户自己写的，必须原样保留" },
            { "role": "assistant", "content": "ok" },
        ]);
        let out = sanitize_messages(&msgs, &mut degraded);
        assert_eq!(out[0]["content"], "Rules.");
        assert_eq!(out[1]["content"], "gitStatus: 用户自己写的，必须原样保留");
        assert_eq!(out[2]["content"], "ok");
        assert_eq!(degraded, vec!["system-env".to_string()]);
    }

    /// 🔴 `role: "developer"` 必须改写成 `system`，否则上游整条 400
    /// `11128 Illegal API invocation from an unapproved channel`。
    ///
    /// 实测单变量对照（国内版 / 国际版各 3/3 稳定）：
    /// `[system, developer, user]` → 11128；把 developer 换成 system → 200。
    /// DSH 这类客户端就是这么发 system prompt 的。
    #[test]
    fn developer_role_is_rewritten_to_system() {
        let mut degraded = Vec::new();
        let msgs = json!([
            { "role": "developer", "content": "You are an AI agent powered by DeepSeek Harness." },
            { "role": "user", "content": "hi" },
        ]);
        let out = sanitize_messages(&msgs, &mut degraded);
        assert_eq!(out[0]["role"], "system", "developer 必须被改成 system");
        assert_eq!(out[0]["content"], "You are an AI agent powered by DeepSeek Harness.",
                   "内容一个字都不能动");
        assert_eq!(out[1]["role"], "user", "user 消息不受影响");
        assert_eq!(degraded, vec!["developer-role".to_string()]);

        // 改写后还要能吃到 system 的环境注入净化（顺序：先改 role 再净化）。
        let mut d2 = Vec::new();
        let msgs2 = json!([
            { "role": "developer", "content": "Rules.\n\ngitStatus: clean\n\nGit user: x\n" },
            { "role": "user", "content": "hi" },
        ]);
        let out2 = sanitize_messages(&msgs2, &mut d2);
        assert_eq!(out2[0]["role"], "system");
        assert_eq!(out2[0]["content"], "Rules.");
        assert_eq!(d2, vec!["developer-role".to_string(), "system-env".to_string()]);

        // 没有 developer 时不能凭空加标记。
        let mut d3 = Vec::new();
        sanitize_messages(&json!([{ "role": "system", "content": "ok" }]), &mut d3);
        assert!(d3.is_empty());
    }

    /// v1 的扁平配置能自动迁移成双入口：旧的 `listen` / `api_key` / `enabled`
    /// 归国内版，国际版保持默认关闭 —— 不擅自替用户开一个以前不存在的监听端口。
    #[test]
    fn legacy_flat_config_migrates_to_two_endpoints() {
        let old: LegacyConfig = serde_json::from_str(
            r#"{"enabled":true,"listen":"127.0.0.1:9999","api_key":"sk-old","accounts":["u1"]}"#,
        )
        .expect("v1 扁平配置应当能解析");
        let cfg = ProxyConfig::from_legacy(old);

        assert!(cfg.domestic.enabled);
        assert_eq!(cfg.domestic.listen, "127.0.0.1:9999");
        assert_eq!(cfg.domestic.api_key, "sk-old");
        assert_eq!(cfg.domestic.accounts, vec!["u1".to_string()]);

        assert!(!cfg.international.enabled, "国际版不该被自动开启");
        assert_eq!(cfg.international.listen, DEFAULT_LISTEN_INTERNATIONAL);
        assert!(cfg.international.api_key.is_empty());
        assert!(cfg.international.accounts.is_empty());

        assert!(cfg.any_enabled());
    }

    /// v2（两个入口 + 顶层共用名单）→ 现行结构：名单同时发给两边，行为等价。
    #[test]
    fn v2_shared_account_list_is_split_to_both() {
        let old: V2Config = serde_json::from_str(
            r#"{"domestic":{"enabled":true,"listen":"127.0.0.1:7863","api_key":"a"},
                "international":{"enabled":true,"listen":"127.0.0.1:7864","api_key":"b"},
                "accounts":["u1","u2"]}"#,
        )
        .expect("v2 配置应当能解析");
        let cfg = ProxyConfig::from_v2(old);

        assert_eq!(cfg.domestic.accounts, vec!["u1".to_string(), "u2".to_string()]);
        assert_eq!(cfg.international.accounts, vec!["u1".to_string(), "u2".to_string()]);
        assert_eq!(cfg.domestic.api_key, "a");
        assert_eq!(cfg.international.api_key, "b");
    }

    /// 监听地址留空时补该版本的默认端口（国内 7863 / 国际 7864）。
    #[test]
    fn empty_listen_falls_back_to_edition_default() {
        let cfg = serde_json::from_str::<ProxyConfig>(r#"{"domestic":{"enabled":true}}"#)
            .expect("新格式应当能解析")
            .resolved();

        assert_eq!(cfg.domestic.listen, DEFAULT_LISTEN_DOMESTIC);
        assert_eq!(cfg.international.listen, DEFAULT_LISTEN_INTERNATIONAL);
        assert!(cfg.endpoint(Edition::Domestic).enabled);
        assert!(!cfg.endpoint(Edition::International).enabled);
    }

    /// 两个入口的 key 与账号名单都互相独立；写回后必须仍被识别为**现行格式**
    /// —— 否则下次读会被当旧格式再迁移一遍（迁移循环）。
    #[test]
    fn two_endpoints_keep_independent_key_and_accounts() {
        let cfg = serde_json::from_str::<ProxyConfig>(
            r#"{"domestic":{"enabled":true,"listen":"127.0.0.1:7863","api_key":"sk-cn","accounts":["u-cn"]},
                "international":{"enabled":true,"listen":"127.0.0.1:7864","api_key":"sk-intl","accounts":["u-intl"]}}"#,
        )
        .expect("现行格式应当能解析")
        .resolved();

        assert_eq!(cfg.domestic.api_key, "sk-cn");
        assert_eq!(cfg.international.api_key, "sk-intl");
        assert_ne!(cfg.domestic.api_key, cfg.international.api_key);
        assert_eq!(cfg.domestic.accounts, vec!["u-cn".to_string()]);
        assert_eq!(cfg.international.accounts, vec!["u-intl".to_string()]);
        assert!(cfg.any_enabled());

        let v = serde_json::to_value(&cfg).expect("应当能序列化");
        assert!(
            v.get("domestic").is_some() && v.get("international").is_some(),
            "写回的必须是现行格式"
        );
        assert!(v.get("listen").is_none(), "不该再写出 v1 的顶层 listen");
        assert!(
            v.get("accounts").is_none(),
            "顶层不该再有 accounts —— 有的话下次读会被当 v2 再迁移一次"
        );
    }

    /// 鉴权只看**本入口**那把 key。
    #[test]
    fn auth_uses_this_endpoint_key_only() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer sk-cn"));

        assert!(check_auth(&h, ""), "留空 = 不鉴权");
        assert!(check_auth(&h, "sk-cn"));
        assert!(check_auth(&h, "  sk-cn  "), "容忍两侧空白");
        assert!(!check_auth(&h, "sk-intl"), "另一入口的 key 不该放行");
    }

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

    /// 模型归属标签：每个入口只列自己那一版，`owned_by` 要能看出是哪一版。
    ///
    /// （`/v1/models` 以前是「两档合并 + 同名标 workbuddy」，改成按入口分端口之后
    /// 不再合并 —— 连国内版端口就不该在列表里看到国际版专有的模型。）
    #[test]
    fn owner_tag_marks_edition() {
        assert_eq!(owner_tag(Edition::Domestic), "workbuddy-cn");
        assert_eq!(owner_tag(Edition::International), "workbuddy-intl");
    }

    /// 只有一档有的模型 → 判给那一档；**两档都有 → 两类都不判**（否则会互相误拦）。
    #[test]
    fn model_routes_union_not_overwrite() {
        remember_model_routes(Edition::International, &["intl-only-model".to_string()]);
        assert_eq!(
            model_edition("intl-only-model"),
            Some(Edition::International)
        );
        assert_eq!(model_edition("never-seen-model"), None);

        // 🔴 同名模型被两档各记一次后，必须变成「都能用」，而不是被后写的那档覆盖。
        // 实测踩过：国际版列表后拉之后，7863 发 `glm-5.3` 会被误报「属于国际版」。
        remember_model_routes(Edition::Domestic, &["shared-model".to_string()]);
        remember_model_routes(Edition::International, &["shared-model".to_string()]);
        assert_eq!(
            model_edition("shared-model"),
            None,
            "两档都列出过的模型不该被判成某一档，否则另一档的入口会被误拦"
        );
        // 顺序反过来也必须一样
        remember_model_routes(Edition::International, &["shared2".to_string()]);
        remember_model_routes(Edition::Domestic, &["shared2".to_string()]);
        assert_eq!(model_edition("shared2"), None);
    }

    /// `extra_models` 并入 `/v1/models`：追加在末尾、去重、去空白、忽略空项。
    #[test]
    fn extra_models_merge_into_model_list() {
        let up = vec!["a".to_string(), "b".to_string()];
        assert_eq!(merge_extra_models(up.clone(), &[]), up, "没声明时原样返回");

        let out = merge_extra_models(
            up,
            &[
                " deepseek-v4.1-flash ".to_string(),
                "a".to_string(),
                "   ".to_string(),
            ],
        );
        assert_eq!(
            out,
            vec!["a", "b", "deepseek-v4.1-flash"],
            "上游在前；`a` 已存在不重复；空白/空项忽略"
        );
    }

    /// 账号池按版本取号：只要国内版时不该给出国际版的号。
    /// 🔴 11128 渠道校验**不得冷却账号**。
    ///
    /// 2026-10-17 实测：同一个号的只读接口 200、chat 端点稳定 11128，
    /// 换 UA / 补 system / 换模型都无效 —— 是端点级策略拦截，不是号坏了。
    /// 以前一次坏请求会把池里 5 个号全冻 120 秒，客户端随即收到
    /// 「国际版的账号当前都不可用」，而这些号其实一个都没坏。
    #[test]
    fn channel_block_must_not_poison_the_pool() {
        let pool = Pool::new(vec![("u1".to_string(), Edition::International)]);
        // 模拟「被 11128 拦过」：新逻辑下这条路径不该调 mark_failed。
        // 反向验证判据：只要 is_channel_block 命中，调用方就跳过 mark_failed。
        assert!(is_channel_block(r#"{"code":11128,"msg":"request illegal"}"#));
        assert_eq!(
            pool.next_uid_of(Some(Edition::International), None).as_deref(),
            Some("u1"),
            "号没被冷却时应仍可选"
        );
        // 对照：真正需要冷却的失败（鉴权类）仍然生效 —— 别把这条护栏理解成「永不冷却」
        pool.mark_failed("u1");
        assert_eq!(pool.next_uid_of(Some(Edition::International), None), None);
    }

    /// 加密信封凭据的账号**不能进反代池**，否则运行时必报「API 密钥无效」。
    #[test]
    fn envelope_credentials_are_not_proxy_eligible() {
        let intl = Edition::International;
        let plain = serde_json::json!({
            "uid": "u1", "edition": "international",
            "access_token": "eyJhbGciOi.payload.sig"
        });
        assert_eq!(proxy_ineligible_reason(intl, &plain, &[]), None);

        let envelope = serde_json::json!({
            "uid": "u2", "edition": "international",
            "access_token": {"$wbEncrypted": 1, "envelope": "AAAA"}
        });
        assert_eq!(
            proxy_ineligible_reason(intl, &envelope, &[]),
            Some("凭据加密存储，无法用于反代")
        );
    }

    /// 版本不匹配的账号不能静默混进入口 —— 勾了却不生效是最难排查的一类。
    #[test]
    fn edition_mismatch_is_reported_not_silently_dropped() {
        let domestic_acc = serde_json::json!({
            "uid": "u-cn", "domain": "copilot.tencent.com", "access_token": "t"
        });
        assert_eq!(
            proxy_ineligible_reason(Edition::International, &domestic_acc, &[]),
            Some("版本不匹配"),
            "国内版号不能出现在国际版入口的可用池里"
        );
        // 同一账号在国际版入口下判为不匹配；换成国内版入口就正常
        assert_eq!(proxy_ineligible_reason(Edition::Domestic, &domestic_acc, &[]), None);
    }

    /// 未勾选的账号、以及标了 needs_relogin 的账号也要被明确剔除。
    #[test]
    fn unselected_or_stale_accounts_are_excluded() {
        let acc = serde_json::json!({"uid": "u1", "edition": "international", "access_token": "t"});
        let allowed = vec!["other".to_string()];
        assert_eq!(
            proxy_ineligible_reason(Edition::International, &acc, &allowed),
            Some("未在本入口勾选")
        );
        let stale = serde_json::json!({
            "uid": "u1", "edition": "international",
            "access_token": "t", "needs_relogin": true
        });
        assert_eq!(
            proxy_ineligible_reason(Edition::International, &stale, &[]),
            Some("需要重新登录")
        );
    }

    #[test]
    fn pool_filters_by_edition() {
        let pool = Pool::new(vec![
            ("u-intl".to_string(), Edition::International),
            ("u-dom".to_string(), Edition::Domestic),
        ]);
        assert_eq!(pool.next_uid_of(Some(Edition::Domestic), None).as_deref(), Some("u-dom"));
        assert_eq!(pool.next_uid_of(Some(Edition::International), None).as_deref(), Some("u-intl"));
        assert!(pool.has_edition(Edition::Domestic));
        assert!(pool.has_edition(Edition::International));

        let only_intl = Pool::new(vec![("u-intl".to_string(), Edition::International)]);
        assert_eq!(only_intl.next_uid_of(Some(Edition::Domestic), None), None);
        assert!(!only_intl.has_edition(Edition::Domestic));
        // 不限版本时照常能拿到
        assert_eq!(only_intl.next_uid_of(None, None).as_deref(), Some("u-intl"));
    }

    /// 🔴 60004 只冻**模型**：同账号的其他模型仍能被选中，冷却到期后该模型自动回归。
    /// 账号级长冷却会把还能用的模型一起冻掉，与上游「可切换其他模型继续使用」矛盾。
    #[test]
    fn model_cooldown_only_blocks_that_model() {
        let pool = Pool::new(vec![("u1".to_string(), Edition::Domestic)]);
        pool.mark_model_failed_until("u1", "model-a", now_ms_i64() + 3_600_000);

        assert_eq!(
            pool.next_uid_of(Some(Edition::Domestic), Some("model-a")),
            None,
            "被冻的模型不应再选中该号"
        );
        assert_eq!(
            pool.next_uid_of(Some(Edition::Domestic), Some("model-b")).as_deref(),
            Some("u1"),
            "同账号的其他模型必须照常可用"
        );
        // 不关心模型时（model = None）不受模型冷却限制
        assert_eq!(
            pool.next_uid_of(Some(Edition::Domestic), None).as_deref(),
            Some("u1")
        );

        // 已过期的冷确不应生效（mark_model_failed_until 会直接忽略过去时刻）
        let fresh = Pool::new(vec![("u1".to_string(), Edition::Domestic)]);
        fresh.mark_model_failed_until("u1", "model-a", now_ms_i64() - 1_000);
        assert_eq!(
            fresh
                .next_uid_of(Some(Edition::Domestic), Some("model-a"))
                .as_deref(),
            Some("u1")
        );
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

    /// 🔴 `tool_choice` 对象形式必须归一成字符串。
    ///
    /// 上游只吃字符串，收到 OpenAI 规范的 `{"type":"function","function":{"name":"f"}}`
    /// 直接 `11101 Unmarshal chat params failed`（2026-10-21 实测）。
    /// 而函数名字符串 `"f"` 上游接受 —— 所以这个转换是**无损**的。
    #[test]
    fn tool_choice_object_is_normalized_to_string() {
        // 指定函数 → 函数名（保留强制调用语义）
        assert_eq!(
            normalize_tool_choice(&json!({"type":"function","function":{"name":"read_file"}})),
            Some(json!("read_file"))
        );
        // 只有 type 的几种
        assert_eq!(normalize_tool_choice(&json!({"type":"auto"})), Some(json!("auto")));
        assert_eq!(normalize_tool_choice(&json!({"type":"none"})), Some(json!("none")));
        assert_eq!(normalize_tool_choice(&json!({"type":"required"})), Some(json!("required")));
        // Anthropic 风格
        assert_eq!(normalize_tool_choice(&json!({"type":"any"})), Some(json!("required")));
        assert_eq!(
            normalize_tool_choice(&json!({"type":"tool","name":"grep"})),
            Some(json!("grep"))
        );
        // 字符串原样保留
        assert_eq!(normalize_tool_choice(&json!("required")), Some(json!("required")));
        // 认不出来的形态返回 None（调用方丢弃，别把 400 送给上游）
        assert_eq!(normalize_tool_choice(&json!({"type":"weird"})), None);
        assert_eq!(normalize_tool_choice(&json!({"no_type":1})), None);
        assert_eq!(normalize_tool_choice(&json!(123)), None);
    }

    /// 标准采样参数不能丢 —— 以前只转 model/messages/stream/temperature/top_p/max_tokens/tools，
    /// 其余全被静默吞掉，表现为「客户端设了等于没设」。
    #[test]
    fn upstream_body_passes_through_standard_params() {
        let incoming = json!({
            "model": "glm-5.3",
            "messages": [{"role":"user","content":"hi"}],
            "temperature": 0.2,
            "top_p": 0.9,
            "max_tokens": 512,
            "stop": ["END"],
            "frequency_penalty": 0.1,
            "presence_penalty": 0.2,
            "response_format": {"type":"json_object"},
            "parallel_tool_calls": false,
            "seed": 7,
            "user": "u-1",
            "tools": [{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],
            "tool_choice": {"type":"function","function":{"name":"f"}},
        });
        let mut degraded = Vec::new();
        let body = build_upstream_body(&incoming, Edition::Domestic, &mut degraded);
        for key in ["stop", "frequency_penalty", "presence_penalty", "response_format",
                    "parallel_tool_calls", "seed", "user"] {
            assert!(body.get(key).is_some(), "{key} 必须透传");
        }
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["tool_choice"], "f", "对象形式要归一成函数名字符串");
        assert!(degraded.iter().any(|d| d == "tool-choice"));
    }

    /// 11128 自愈：最大强度归一化要把非标准角色、name、数组 content 全部收敛。
    #[test]
    fn aggressive_normalize_collapses_payload_shape() {
        let mut msgs = json!([
            {"role":"developer","content":"sys","name":"dsh-agent"},
            {"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]},
            {"role":"assistant","content":"ok"},
        ]);
        let mut degraded = Vec::new();
        sanitize_messages_aggressive(&mut msgs, &mut degraded);
        assert_eq!(msgs[0]["role"], "system", "developer 要收敛成 system");
        assert!(msgs[0].get("name").is_none(), "name 必须摘掉");
        assert_eq!(msgs[1]["content"], "a\nb", "多块 content 要拍平成字符串");
        assert_eq!(msgs[2]["content"], "ok", "字符串 content 原样保留");
        assert!(degraded.iter().any(|d| d == "aggressive-normalize"));
    }

    /// 身份声明清洗：抹掉 `DeepSeek Harness`（含大小写变体），未命中时不动原串。
    #[test]
    fn strips_third_party_identity_declarations() {
        assert_eq!(
            strip_identity_declarations("You are DeepSeek Harness, an agent.").as_deref(),
            Some("You are DeepSeek, an agent.")
        );
        // 大小写变体
        assert_eq!(
            strip_identity_declarations("powered by deepseek harness").as_deref(),
            Some("powered by DeepSeek")
        );
        // 连字符写法
        assert!(strip_identity_declarations("via deepseek-harness bridge").is_some());
        // 中文混排不破坏 UTF-8
        let mixed = "你是DeepSeek Harness，一个助手。";
        let out = strip_identity_declarations(mixed).unwrap();
        assert_eq!(out, "你是DeepSeek，一个助手。");
        assert!(out.is_char_boundary(out.len()));
        // 未命中 → None（调用方不该改写用户提示词）
        assert_eq!(strip_identity_declarations("You are a helpful assistant."), None);
    }

    /// 自愈路径必须把身份声明一并抹掉，但**不能**动 user 消息里的同名词。
    #[test]
    fn aggressive_normalize_scrubs_identity_in_system_only() {
        let mut msgs = json!([
            {"role":"system","content":"You are DeepSeek Harness. Be brief."},
            {"role":"user","content":"DeepSeek Harness 是什么？"},
        ]);
        let mut degraded = Vec::new();
        sanitize_messages_aggressive(&mut msgs, &mut degraded);
        assert_eq!(msgs[0]["content"], "You are DeepSeek. Be brief.");
        assert_eq!(msgs[1]["content"], "DeepSeek Harness 是什么？", "用户内容不该被改");
        assert!(degraded.iter().any(|d| d == "aggressive-normalize"));
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
        // 用国内版：国际版会额外兜一条 system（见下面的专门测试），断言就对不上了。
        let body = build_upstream_body(&incoming, Edition::Domestic, &mut degraded);
        assert_eq!(body["model"], "glm-5.3-flash");
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0.3);
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert!(body["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("f_123"));
    }

    /// 思考强度类字段必须原样透传 —— 以前被整段丢掉，表现为「客户端调了推理等级、
    /// 上游毫无反应」。同时确认：客户端没发时反代**不臆造**任何思考字段。
    #[test]
    fn upstream_body_passes_through_reasoning_fields() {
        let base = json!({
            "model": "deepseek-v4-pro",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let body = build_upstream_body(&base, Edition::Domestic, &mut Vec::new());
        for key in REASONING_PASSTHROUGH {
            assert!(
                body.get(*key).is_none(),
                "客户端没发 {key}，反代不该补：{body}"
            );
        }

        let incoming = json!({
            "model": "deepseek-v4-pro",
            "reasoning_effort": "high",
            "thinking": { "type": "enabled" },
            "enable_thinking": true,
            "chat_template_kwargs": { "enable_thinking": true },
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let body = build_upstream_body(&incoming, Edition::Domestic, &mut Vec::new());
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    }

    /// 国际版必须给「首条不是 system」的请求补一条 —— 不补会被上游整个拦掉
    /// （实测 `11128 first message is not system prompt`）。国内版不补、也不该补。
    #[test]
    fn international_gets_leading_system() {
        let incoming = json!({
            "model": "deepseek-v4.1-flash",
            "messages": [{ "role": "user", "content": "hi" }]
        });

        let mut degraded = Vec::new();
        let intl = build_upstream_body(&incoming, Edition::International, &mut degraded);
        assert_eq!(intl["messages"][0]["role"], "system", "国际版要兜一条 system");
        assert_eq!(intl["messages"][1]["role"], "user", "原来的消息要原样跟在后头");
        assert_eq!(intl["messages"].as_array().unwrap().len(), 2);
        assert!(degraded.contains(&"leading-system".to_string()), "要留痕");

        // 已经有 system 就不要再插
        let with_sys = json!({
            "model": "deepseek-v4.1-flash",
            "messages": [
                { "role": "system", "content": "s" },
                { "role": "user", "content": "hi" }
            ]
        });
        let body2 = build_upstream_body(&with_sys, Edition::International, &mut Vec::new());
        assert_eq!(body2["messages"].as_array().unwrap().len(), 2, "不该重复插");
        assert_eq!(body2["messages"][0]["content"], "s", "原有的 system 要保留");

        // 国内版：一行都不许改
        let dom = build_upstream_body(&incoming, Edition::Domestic, &mut Vec::new());
        assert_eq!(dom["messages"][0]["role"], "user");
        assert_eq!(dom["messages"].as_array().unwrap().len(), 1);
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
