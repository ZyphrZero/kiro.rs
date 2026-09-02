//! OpenAI Chat Completions 兼容端点
//!
//! 把 OpenAI `POST /v1/chat/completions` 请求翻译成内部的 Anthropic
//! [`MessagesRequest`]，复用 [`super::handlers::post_messages`] 的完整链路
//! （模型映射、多凭据故障转移、用量计量、工具映射……），再把 Anthropic 响应
//! 翻译回 OpenAI 格式。
//!
//! 这样只会说 OpenAI 协议的客户端（如 Codex CLI，`wire_api = "chat"`）也能
//! 直接走 Kiro 后端，无需额外的翻译代理进程。
//!
//! 说明：内部调用始终以非流式方式执行，`stream: true` 的请求在拿到完整结果后
//! 合成为 OpenAI 的 `chat.completion.chunk` SSE 序列。对 Codex 这类"拿到结果再
//! 展示"的客户端，语义与逐 token 流式一致；正确性（含工具调用）完全保留。

use std::collections::BTreeMap;

use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Extension, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::handlers::post_messages;
use super::middleware::{AppState, KeyContext};
use super::types::{Message, MessagesRequest, Metadata, OutputConfig, SystemMessage, Tool};

/// 读取内部响应体时的上限（64MB，与请求体上限对齐）
const MAX_INNER_BODY: usize = 64 * 1024 * 1024;

/// 未显式给出 max_tokens 时的默认输出上限
const DEFAULT_MAX_TOKENS: i32 = 32000;

// ============================ 请求类型 ============================

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_tokens: Option<i32>,
    #[serde(default)]
    pub max_completion_tokens: Option<i32>,
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
}

/// 从 OpenAI 请求体或会话亲和请求头中提取并规范化 Kiro 会话 UUID。
pub(super) fn resolve_session_metadata(
    prompt_cache_key: Option<&str>,
    headers: &HeaderMap,
) -> Option<Metadata> {
    let candidates = [
        prompt_cache_key,
        headers
            .get("x-session-affinity")
            .and_then(|value| value.to_str().ok()),
        headers
            .get("x-client-request-id")
            .and_then(|value| value.to_str().ok()),
        headers
            .get("session_id")
            .and_then(|value| value.to_str().ok()),
    ];

    candidates.into_iter().flatten().find_map(|candidate| {
        let raw_uuid = candidate.strip_prefix("session_").unwrap_or(candidate);
        let uuid = Uuid::parse_str(raw_uuid).ok()?;
        Some(Metadata {
            user_id: Some(format!("session_{uuid}")),
        })
    })
}

// ============================ Handler ============================

/// `POST /v1/chat/completions`
pub async fn post_chat_completions(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    headers: HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> Response {
    let want_stream = req.stream;
    let model = req.model.clone();
    let metadata = resolve_session_metadata(req.prompt_cache_key.as_deref(), &headers);

    tracing::info!(
        model = %model,
        stream = %want_stream,
        message_count = %req.messages.len(),
        "Received POST /v1/chat/completions request"
    );

    // 1. OpenAI -> Anthropic 请求翻译
    let anthropic_req = match openai_to_anthropic(req, metadata) {
        Ok(r) => r,
        Err(msg) => {
            return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &msg);
        }
    };

    // 2. 复用 Anthropic 全链路（内部强制非流式）
    let inner = post_messages(State(state), Extension(key_ctx), Json(anthropic_req)).await;

    let status = inner.status();
    let body_bytes = match to_bytes(inner.into_body(), MAX_INNER_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("failed to read upstream response: {e}"),
            );
        }
    };

    // 上游非 2xx：原样透传（Anthropic 错误体已是 {"error":{type,message}} 形状）
    if !status.is_success() {
        return Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body_bytes))
            .unwrap();
    }

    let anthropic: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("failed to parse upstream response: {e}"),
            );
        }
    };

    // 3. Anthropic -> OpenAI 响应翻译
    let parsed = parse_anthropic_message(&anthropic, &model);

    if want_stream {
        let sse = build_stream_sse(&parsed);
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(sse))
            .unwrap()
    } else {
        let body = build_completion_json(&parsed);
        (StatusCode::OK, Json(body)).into_response()
    }
}

// ============================ 请求翻译 ============================

fn openai_to_anthropic(
    req: ChatCompletionRequest,
    metadata: Option<Metadata>,
) -> Result<MessagesRequest, String> {
    let max_tokens = req
        .max_tokens
        .or(req.max_completion_tokens)
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS);

    let mut system: Vec<SystemMessage> = Vec::new();
    // 合并后的对话消息：(role, content blocks)
    let mut merged: Vec<(String, Vec<Value>)> = Vec::new();

    for m in &req.messages {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        match role {
            "system" | "developer" => {
                for text in collect_text_strings(m.get("content")) {
                    system.push(SystemMessage {
                        text,
                        cache_control: None,
                    });
                }
            }
            "user" => {
                let blocks = content_blocks(m.get("content"));
                push_merged(&mut merged, "user", blocks);
            }
            "assistant" => {
                let mut blocks = content_blocks(m.get("content"));
                if let Some(calls) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    for call in calls {
                        let id = call
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let func = call.get("function");
                        let name = func
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args_str = func
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");
                        let input: Value =
                            serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                }
                push_merged(&mut merged, "assistant", blocks);
            }
            "tool" => {
                let tool_use_id = m
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = collect_text_strings(m.get("content")).join("\n");
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                });
                // Anthropic 里 tool_result 属于 user 轮
                push_merged(&mut merged, "user", vec![block]);
            }
            _ => {}
        }
    }

    // 丢弃空内容轮，Anthropic 不接受空 content
    let messages: Vec<Message> = merged
        .into_iter()
        .filter(|(_, blocks)| !blocks.is_empty())
        .map(|(role, blocks)| Message {
            role,
            content: Value::Array(blocks),
        })
        .collect();

    if messages.is_empty() {
        return Err("messages must contain at least one user/assistant message".to_string());
    }

    let tools = req.tools.as_ref().map(|ts| convert_tools(ts));
    let tool_choice = req.tool_choice.as_ref().and_then(convert_tool_choice);
    let output_config = req
        .reasoning_effort
        .filter(|e| !e.trim().is_empty())
        .map(|effort| OutputConfig { effort });

    Ok(MessagesRequest {
        model: req.model,
        max_tokens,
        messages,
        stream: false, // 内部始终非流式
        system: if system.is_empty() {
            None
        } else {
            Some(system)
        },
        tools,
        tool_choice,
        thinking: None,
        output_config,
        metadata,
    })
}

/// 追加到 merged，若与上一轮 role 相同则合并 content blocks
pub(super) fn push_merged(merged: &mut Vec<(String, Vec<Value>)>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = merged.last_mut() {
        if last.0 == role {
            last.1.extend(blocks);
            return;
        }
    }
    merged.push((role.to_string(), blocks));
}

/// 把 OpenAI message.content（字符串或数组）转成 Anthropic content blocks
fn content_blocks(content: Option<&Value>) -> Vec<Value> {
    let mut out = Vec::new();
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                out.push(json!({"type": "text", "text": s}));
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                let ty = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ty {
                    "text" | "input_text" => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            out.push(json!({"type": "text", "text": t}));
                        }
                    }
                    "image_url" => {
                        if let Some(block) = image_block(part) {
                            out.push(block);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// 仅收集纯文本（system / tool 内容用）
pub(super) fn collect_text_strings(content: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                out.push(s.clone());
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    if !t.is_empty() {
                        out.push(t.to_string());
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// 把 OpenAI image_url（仅支持 data: URL）转成 Anthropic image block
fn image_block(part: &Value) -> Option<Value> {
    let url = part
        .get("image_url")
        .and_then(|iu| iu.get("url"))
        .and_then(|v| v.as_str())?;
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some(json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media_type,
            "data": data,
        }
    }))
}

pub(super) fn convert_tools(tools: &[Value]) -> Vec<Tool> {
    let mut out = Vec::new();
    for t in tools {
        // OpenAI: {type:"function", function:{name, description, parameters}}
        let func = t.get("function").unwrap_or(t);
        let name = func
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let description = func
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mut input_schema: BTreeMap<String, Value> = BTreeMap::new();
        if let Some(Value::Object(params)) = func.get("parameters") {
            for (k, v) in params {
                input_schema.insert(k.clone(), v.clone());
            }
        }
        out.push(Tool {
            tool_type: None,
            name,
            description,
            input_schema,
            max_uses: None,
            cache_control: None,
        });
    }
    out
}

fn convert_tool_choice(tc: &Value) -> Option<Value> {
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "required" => Some(json!({"type": "any"})),
            "none" => None,
            _ => Some(json!({"type": "auto"})),
        },
        Value::Object(_) => {
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str());
            name.map(|n| json!({"type": "tool", "name": n}))
        }
        _ => None,
    }
}

// ============================ 响应翻译 ============================

pub(super) struct ParsedResponse {
    pub(super) model: String,
    pub(super) text: String,
    pub(super) tool_calls: Vec<Value>, // OpenAI tool_calls
    /// Anthropic/Kiro 的原始停止原因。`finish_reason` 是面向 OpenAI
    /// completion 兼容层的归一化值，不能用于区分输出耗尽和上下文溢出。
    pub(super) upstream_stop_reason: String,
    pub(super) finish_reason: String,
    pub(super) prompt_tokens: i64,
    pub(super) cached_tokens: i64,
    pub(super) completion_tokens: i64,
    /// 思考文本（content 里的 thinking 块 + web_search loop 的顶层
    /// `kiro_thinking` 带外字段）。chat/completions 路径不消费，
    /// Responses 路径渲染为 reasoning summary item。
    pub(super) thinking: String,
    /// 内部代答的 web_search 展示（server_tool_use 块）：(id, query)。
    /// Responses 路径渲染为 web_search_call item。
    pub(super) web_searches: Vec<(String, String)>,
    /// 上游 meteringEvent 透传的 credit_usage，未下发时为 None。
    /// 与 kiro-rs /v1/chat/completions 行为对齐：仅在拿到 meteringEvent 时
    /// 才把 credit_usage / credit_unit / credit_unit_plural 写入响应 usage。
    pub(super) credit_usage: Option<f64>,
    pub(super) credit_unit: Option<String>,
    pub(super) credit_unit_plural: Option<String>,
}

pub(super) fn parse_anthropic_message(anthropic: &Value, model: &str) -> ParsedResponse {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut thinking = String::new();
    let mut web_searches = Vec::new();

    if let Some(blocks) = anthropic.get("content").and_then(|v| v.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                        thinking.push_str(t);
                    }
                }
                Some("server_tool_use") => {
                    // 内部代答的 web_search 展示块（websearch_loop Contract A）
                    if block.get("name").and_then(|v| v.as_str()) == Some("web_search") {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let query = block
                            .pointer("/input/query")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        web_searches.push((id, query));
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let arguments = block
                        .get("input")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments },
                    }));
                }
                _ => {} // web_search_tool_result / 其它块对 OpenAI 客户端无意义，忽略
            }
        }
    }

    // web_search loop 的带外思考文本（不进 content，避免 Anthropic 客户端回放）
    if let Some(t) = anthropic.get("kiro_thinking").and_then(|v| v.as_str()) {
        if !t.is_empty() {
            if !thinking.is_empty() {
                thinking.push_str("\n\n");
            }
            thinking.push_str(t);
        }
    }

    let stop_reason = anthropic
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");
    let finish_reason = map_finish_reason(stop_reason, !tool_calls.is_empty()).to_string();

    let usage = anthropic.get("usage");
    let uncached_input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let cache_creation_tokens = usage
        .and_then(|u| u.get("cache_creation_input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let cached_tokens = usage
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let prompt_tokens = uncached_input_tokens
        .saturating_add(cache_creation_tokens)
        .saturating_add(cached_tokens);
    let completion_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);

    let credit_usage = usage
        .and_then(|u| u.get("credit_usage"))
        .and_then(|v| v.as_f64());
    let credit_unit = usage
        .and_then(|u| u.get("credit_unit"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let credit_unit_plural = usage
        .and_then(|u| u.get("credit_unit_plural"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    ParsedResponse {
        model: model.to_string(),
        text,
        tool_calls,
        upstream_stop_reason: stop_reason.to_string(),
        finish_reason,
        prompt_tokens,
        cached_tokens,
        completion_tokens,
        thinking,
        web_searches,
        credit_usage,
        credit_unit,
        credit_unit_plural,
    }
}

fn map_finish_reason(stop_reason: &str, has_tool_calls: bool) -> &'static str {
    match stop_reason {
        "tool_use" => "tool_calls",
        "max_tokens" | "model_context_window_exceeded" => "length",
        _ if has_tool_calls => "tool_calls",
        _ => "stop",
    }
}

pub(super) fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

fn new_id() -> String {
    format!("chatcmpl-{}", Uuid::new_v4().to_string().replace('-', ""))
}

fn build_completion_json(p: &ParsedResponse) -> Value {
    let content: Value = if p.text.is_empty() && !p.tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(p.text.clone())
    };

    let mut message = json!({ "role": "assistant", "content": content });
    if !p.tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(p.tool_calls.clone());
    }

    json!({
        "id": new_id(),
        "object": "chat.completion",
        "created": now_ts(),
        "model": p.model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": p.finish_reason,
        }],
        "usage": build_usage_json(p),
    })
}

/// 把完整结果合成为 OpenAI chat.completion.chunk SSE 序列
fn build_stream_sse(p: &ParsedResponse) -> String {
    let id = new_id();
    let created = now_ts();
    let mut out = String::new();

    let mut push_chunk = |delta: Value, finish: Option<&str>| {
        let chunk = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": p.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish,
            }],
        });
        out.push_str("data: ");
        out.push_str(&chunk.to_string());
        out.push_str("\n\n");
    };

    // 角色帧
    push_chunk(json!({ "role": "assistant" }), None);

    // 文本帧
    if !p.text.is_empty() {
        push_chunk(json!({ "content": p.text }), None);
    }

    // 工具调用帧
    for (i, tc) in p.tool_calls.iter().enumerate() {
        let delta = json!({
            "tool_calls": [{
                "index": i,
                "id": tc.get("id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": tc.get("function").cloned().unwrap_or(json!({})),
            }]
        });
        push_chunk(delta, None);
    }

    // 结束帧（带 usage）
    let final_chunk = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": p.model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": p.finish_reason,
        }],
        "usage": build_usage_json(p),
    });
    out.push_str("data: ");
    out.push_str(&final_chunk.to_string());
    out.push_str("\n\n");
    out.push_str("data: [DONE]\n\n");

    out
}

/// 构造 OpenAI usage 对象，并按需透传 upstream meteringEvent 写入的
/// credit_usage / credit_unit / credit_unit_plural 字段。
fn build_usage_json(p: &ParsedResponse) -> Value {
    let mut usage = json!({
        "prompt_tokens": p.prompt_tokens,
        "completion_tokens": p.completion_tokens,
        "total_tokens": p.prompt_tokens + p.completion_tokens,
    });
    if let Some(credit_usage) = p.credit_usage {
        usage["credit_usage"] = json!(credit_usage);
    }
    if let Some(credit_unit) = &p.credit_unit {
        usage["credit_unit"] = json!(credit_unit);
    }
    if let Some(credit_unit_plural) = &p.credit_unit_plural {
        usage["credit_unit_plural"] = json!(credit_unit_plural);
    }
    usage
}

fn openai_error(status: StatusCode, err_type: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "message": message,
            "type": err_type,
        }
    });
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID_A: &str = "550e8400-e29b-41d4-a716-446655440000";
    const UUID_B: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";
    const UUID_C: &str = "123e4567-e89b-12d3-a456-426614174000";
    const UUID_D: &str = "123e4567-e89b-12d3-a456-426614174001";

    fn metadata_user_id(metadata: Option<Metadata>) -> Option<String> {
        metadata.and_then(|value| value.user_id)
    }

    fn chat_request(prompt_cache_key: Option<&str>) -> ChatCompletionRequest {
        let mut value = json!({
            "model": "gpt-5.6-sol",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "low",
            "max_completion_tokens": 12
        });
        if let Some(key) = prompt_cache_key {
            value["prompt_cache_key"] = json!(key);
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn session_metadata_accepts_and_normalizes_uuid_forms() {
        let headers = HeaderMap::new();
        assert_eq!(
            metadata_user_id(resolve_session_metadata(
                Some("550E8400-E29B-41D4-A716-446655440000"),
                &headers,
            ))
            .as_deref(),
            Some("session_550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            metadata_user_id(resolve_session_metadata(
                Some("session_67e55044-10b1-426f-9247-bb680e5fe0c8"),
                &headers,
            ))
            .as_deref(),
            Some("session_67e55044-10b1-426f-9247-bb680e5fe0c8")
        );
    }

    #[test]
    fn session_metadata_uses_body_then_header_priority_with_invalid_fallback() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-session-affinity",
            format!("session_{UUID_B}").parse().unwrap(),
        );
        headers.insert("x-client-request-id", UUID_C.parse().unwrap());
        headers.insert("session_id", UUID_D.parse().unwrap());

        assert_eq!(
            metadata_user_id(resolve_session_metadata(Some(UUID_A), &headers)).as_deref(),
            Some("session_550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            metadata_user_id(resolve_session_metadata(Some("invalid"), &headers)).as_deref(),
            Some("session_67e55044-10b1-426f-9247-bb680e5fe0c8")
        );

        headers.insert("x-session-affinity", "invalid".parse().unwrap());
        assert_eq!(
            metadata_user_id(resolve_session_metadata(None, &headers)).as_deref(),
            Some("session_123e4567-e89b-12d3-a456-426614174000")
        );

        headers.insert("x-client-request-id", "invalid".parse().unwrap());
        assert_eq!(
            metadata_user_id(resolve_session_metadata(None, &headers)).as_deref(),
            Some("session_123e4567-e89b-12d3-a456-426614174001")
        );
    }

    #[test]
    fn session_metadata_skips_non_utf8_and_invalid_candidates() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-session-affinity",
            axum::http::HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        headers.insert("x-client-request-id", "not-a-uuid".parse().unwrap());
        headers.insert("session_id", "session_not-a-uuid".parse().unwrap());

        assert!(resolve_session_metadata(Some("invalid"), &headers).is_none());
        assert!(resolve_session_metadata(None, &HeaderMap::new()).is_none());
    }

    #[test]
    fn chat_conversion_forwards_resolved_metadata_without_other_regressions() {
        let req = chat_request(Some(UUID_A));
        let metadata = resolve_session_metadata(req.prompt_cache_key.as_deref(), &HeaderMap::new());
        let anthropic = openai_to_anthropic(req, metadata).unwrap();

        assert_eq!(
            anthropic
                .metadata
                .and_then(|value| value.user_id)
                .as_deref(),
            Some("session_550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(anthropic.model, "gpt-5.6-sol");
        assert_eq!(anthropic.max_tokens, 12);
        assert_eq!(anthropic.messages.len(), 1);
        assert_eq!(
            anthropic
                .output_config
                .as_ref()
                .map(|config| config.effort.as_str()),
            Some("low")
        );

        let anthropic = openai_to_anthropic(chat_request(None), None).unwrap();
        assert!(anthropic.metadata.is_none());
    }

    fn base_parsed() -> ParsedResponse {
        ParsedResponse {
            model: "gpt-5.6-sol".to_string(),
            text: "hi".to_string(),
            tool_calls: Vec::new(),
            upstream_stop_reason: "end_turn".to_string(),
            finish_reason: "stop".to_string(),
            prompt_tokens: 7,
            cached_tokens: 0,
            completion_tokens: 11,
            thinking: String::new(),
            web_searches: Vec::new(),
            credit_usage: None,
            credit_unit: None,
            credit_unit_plural: None,
        }
    }

    #[test]
    fn build_completion_omits_credit_fields_without_metering() {
        let p = base_parsed();
        let out = build_completion_json(&p);
        let usage = &out["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
        // 原有字段保持原样
        assert_eq!(usage["prompt_tokens"], json!(7));
        assert_eq!(usage["completion_tokens"], json!(11));
        assert_eq!(usage["total_tokens"], json!(18));
    }

    #[test]
    fn build_completion_carries_credit_fields_when_metering_present() {
        let mut p = base_parsed();
        p.credit_usage = Some(0.5);
        p.credit_unit = Some("credit".to_string());
        p.credit_unit_plural = Some("credits".to_string());
        let out = build_completion_json(&p);
        let usage = &out["usage"];
        assert_eq!(usage["credit_usage"], json!(0.5));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
    }

    #[test]
    fn build_stream_sse_carries_credit_fields_in_final_chunk() {
        let mut p = base_parsed();
        p.credit_usage = Some(0.33);
        p.credit_unit = Some("credit".to_string());
        p.credit_unit_plural = Some("credits".to_string());
        let sse = build_stream_sse(&p);
        assert!(sse.contains("\"credit_usage\":0.33"));
        assert!(sse.contains("\"credit_unit\":\"credit\""));
        assert!(sse.contains("\"credit_unit_plural\":\"credits\""));
        // 结束帧的 usage 必须出现在最后一个 data 块里
        assert!(sse.contains("\"usage\""));
    }

    #[test]
    fn build_stream_sse_omits_credit_fields_without_metering() {
        let p = base_parsed();
        let sse = build_stream_sse(&p);
        assert!(!sse.contains("credit_usage"));
        assert!(!sse.contains("credit_unit"));
        assert!(!sse.contains("credit_unit_plural"));
    }

    #[test]
    fn parse_anthropic_message_extracts_credit_fields() {
        let anthropic = json!({
            "id": "msg_x",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "claude-opus-4-7",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 3,
                "output_tokens": 5,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
                "credit_usage": 0.6,
                "credit_unit": "credit",
                "credit_unit_plural": "credits",
            }
        });
        let p = parse_anthropic_message(&anthropic, "claude-opus-4-7");
        assert_eq!(p.prompt_tokens, 3);
        assert_eq!(p.completion_tokens, 5);
        assert_eq!(p.credit_usage, Some(0.6));
        assert_eq!(p.credit_unit.as_deref(), Some("credit"));
        assert_eq!(p.credit_unit_plural.as_deref(), Some("credits"));
    }

    #[test]
    fn parse_anthropic_message_combines_all_input_categories() {
        let anthropic = json!({
            "content": [],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 3,
                "cache_creation_input_tokens": 4,
                "cache_read_input_tokens": 7,
                "output_tokens": 5
            }
        });
        let p = parse_anthropic_message(&anthropic, "gpt-5.6-sol");
        assert_eq!(p.prompt_tokens, 14);
        assert_eq!(p.cached_tokens, 7);
        assert_eq!(p.completion_tokens, 5);
    }

    #[test]
    fn parse_anthropic_message_preserves_upstream_stop_reason() {
        for reason in ["max_tokens", "model_context_window_exceeded"] {
            let anthropic = json!({
                "content": [{"type": "text", "text": "partial"}],
                "stop_reason": reason,
                "usage": {"input_tokens": 3, "output_tokens": 5}
            });
            let parsed = parse_anthropic_message(&anthropic, "gpt-5.6-sol");
            assert_eq!(parsed.upstream_stop_reason, reason);
            assert_eq!(parsed.finish_reason, "length");
        }
    }

    #[test]
    fn parse_anthropic_message_sanitizes_negative_and_missing_usage() {
        let anthropic = json!({
            "content": [],
            "usage": {
                "input_tokens": -3,
                "cache_read_input_tokens": -7,
                "output_tokens": -5
            }
        });
        let p = parse_anthropic_message(&anthropic, "gpt-5.6-sol");
        assert_eq!(p.prompt_tokens, 0);
        assert_eq!(p.cached_tokens, 0);
        assert_eq!(p.completion_tokens, 0);
    }

    #[test]
    fn parse_anthropic_message_without_credit_fields_leaves_them_none() {
        let anthropic = json!({
            "id": "msg_x",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "claude-opus-4-7",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 3,
                "output_tokens": 5,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
            }
        });
        let p = parse_anthropic_message(&anthropic, "claude-opus-4-7");
        assert!(p.credit_usage.is_none());
        assert!(p.credit_unit.is_none());
        assert!(p.credit_unit_plural.is_none());
    }
}
