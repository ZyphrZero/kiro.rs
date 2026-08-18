//! web_search local agentic loop
//!
//! Handles the case "after mixed tools (web_search + exec...) fall onto the normal chat path, the upstream returns a tool_use with name=web_search":
//! kiro-rs internally calls /mcp to search -> feeds the results back as a tool_result -> reconverts and resends -> loops until the upstream stops asking to search;
//! tool_use calls other than web_search (exec, etc.) are returned to the client as usual: they do not enter the loop and are not swallowed.
//!
//! Reuses: converter::convert_request (feedback), provider.call_api_stream, EventStreamDecoder,
//! websearch::{create_mcp_request, call_mcp_api, parse_search_results, generate_search_summary}。

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use axum::{
    body::{Body, to_bytes},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{FutureExt, StreamExt, stream};
use serde_json::{Value, json};
use tokio::{
    sync::mpsc,
    time::{Duration, Instant, interval_at},
};
use uuid::Uuid;

use crate::admin::trace_db::outcome;
use crate::kiro::model::events::{Event, MeteringEvent, TokenUsage};
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::KiroProvider;
use crate::token;

use super::converter::{ConversionError, convert_request_with_mode, get_context_window_size};
use super::handlers::{
    RequestTracer, TraceUsage, UsageRecordHook, last_attempt_outcome, map_provider_error,
};
use super::stream::{CompletedToolUse, SseEvent};
use super::types::{ErrorResponse, Message, MessagesRequest};
use super::websearch::{self, WebSearchResults};
use crate::model::config::ToolCompatibilityMode;

/// Maximum number of search rounds, to prevent an infinite loop if the upstream keeps asking to search
const MAX_WEB_SEARCH_ROUNDS: usize = 5;

/// A valid assistant turn after a tool result must contain either visible text or
/// another client tool call. Kiro occasionally closes a successful upstream stream
/// without either, which used to be serialized as `end_turn` and made Codex mark an
/// unfinished task complete. Retry once before surfacing an upstream error.
const MAX_EMPTY_TOOL_RESULT_RETRIES: usize = 1;

/// Bounded progress queue for the streamed agentic loop. Search result blocks
/// are small, so this is enough to provide backpressure without buffering an
/// unbounded response when the client is slow.
const WEB_SEARCH_PROGRESS_CAPACITY: usize = 32;

/// Keep a streamed response alive while an upstream model round or MCP call is
/// still running. The first `message_start` is emitted immediately; pings cover
/// any subsequent long-running operation.
const WEB_SEARCH_PING_INTERVAL_SECS: u64 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyToolResultDisposition {
    Accept,
    Retry,
    Fail,
}

/// Result of buffer-decoding one round of the upstream response
struct RoundOutcome {
    /// Accumulated assistant text
    text: String,
    /// Accumulated thinking / reasoning text (Kiro reasoningContentEvent).
    /// Surfaced out-of-band via render_json's `kiro_thinking` so Anthropic
    /// clients never see (and never replay) an unsigned thinking block.
    thinking: String,
    /// The complete tool_use for this round (name already restored via tool_name_map)
    tool_uses: Vec<CompletedToolUse>,
    /// Actual input tokens computed from contextUsageEvent
    context_input_tokens: Option<i32>,
    /// metadataEvent.tokenUsage 的单轮精确最终快照。
    provider_token_usage: Option<TokenUsage>,
    /// Cumulative credits from meteringEvent (sum of usage across rounds)
    credits: f64,
    /// 最近一次 meteringEvent 完整 payload（含 unit / unit_plural / usage）。
    /// 在 run_web_search_loop 出口处透传到响应 usage 字段；如果上游多次下发
    /// 则取最后一次（与 /v1/messages 非流 / 流式路径一致）。
    last_metering: Option<MeteringEvent>,
    /// stop_reason override (max_tokens / model_context_window_exceeded)
    stop_reason_override: Option<String>,
    /// Upstream body-read error, if any. Content decoded before this error is
    /// partial and must not be treated as a successful round.
    stream_error: Option<String>,
    /// Tool names declared to the upstream this round (original + shortened),
    /// taken from `ConversionResult::known_tool_names`. Used by the shared
    /// `<invoke>` text-leak fault tolerance so a leaked `<invoke name=...>` is only
    /// reclaimed when its name is a real declared tool.
    known_tool_names: std::collections::HashSet<String>,
    /// Short-name -> original-name map for this round, taken from
    /// `ConversionResult::tool_name_map`. Used to restore the original tool name when a
    /// leaked `<invoke>` carries a shortened (>63 char) tool name.
    tool_name_map: std::collections::HashMap<String, String>,
}

impl RoundOutcome {
    /// 解析本次 provider 调用的 token 用量；精确 metadata 缺失时只回退本轮。
    fn resolved_token_usage(&self, fallback_input_tokens: i32) -> TokenUsage {
        if let Some(usage) = self.provider_token_usage {
            return usage.sanitized();
        }

        let mut output = Vec::new();
        if !self.thinking.is_empty() {
            output.push(json!({"type": "thinking", "thinking": self.thinking}));
        }
        if !self.text.is_empty() {
            output.push(json!({"type": "text", "text": self.text}));
        }
        output.extend(
            self.tool_uses
                .iter()
                .map(CompletedToolUse::to_anthropic_block),
        );

        TokenUsage {
            uncached_input_tokens: self
                .context_input_tokens
                .unwrap_or(fallback_input_tokens)
                .max(0),
            output_tokens: token::estimate_output_tokens(&output),
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
        }
    }
}

/// Normalize model-produced Web Search input into one non-empty query.
///
/// Codex-compatible providers can emit `query`, `search_query`, `q`, a
/// `queries` array, or wrap the text in `text`/`value`. Kiro's MCP
/// endpoint accepts only one string in `arguments.query`.
fn normalized_query_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let query = s.trim();
            (!query.is_empty()).then(|| query.to_string())
        }
        Value::Array(values) => values.iter().find_map(normalized_query_value),
        Value::Object(object) => ["query", "search_query", "q", "text", "value"]
            .iter()
            .find_map(|key| object.get(*key).and_then(normalized_query_value)),
        _ => None,
    }
}

/// Extract a usable Web Search query from a model tool-use input.
fn tool_query(tu: &CompletedToolUse) -> Option<String> {
    ["query", "search_query", "q", "queries"]
        .iter()
        .find_map(|key| tu.input.get(*key).and_then(normalized_query_value))
        .or_else(|| normalized_query_value(&tu.input))
}

fn log_invalid_web_search_input(tu: &CompletedToolUse) {
    let (input_kind, input_details) = match &tu.input {
        Value::Object(object) => {
            let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
            keys.sort_unstable();
            ("object", keys.join(","))
        }
        Value::Array(values) => ("array", format!("len={}", values.len())),
        Value::String(value) => ("string", format!("len={}", value.chars().count())),
        Value::Number(_) => ("number", String::new()),
        Value::Bool(_) => ("bool", String::new()),
        Value::Null => ("null", String::new()),
    };
    tracing::warn!(
        tool_use_id = %tu.id,
        input_kind,
        input_details = %input_details,
        "web_search tool input has no usable non-empty query; returning an empty result without calling MCP"
    );
}

fn log_normalized_web_search_query(tu: &CompletedToolUse, query: &str) {
    tracing::info!(
        tool_use_id = %tu.id,
        query_chars = query.chars().count(),
        "web_search normalized a non-empty query before calling Kiro MCP"
    );
}

/// Decides whether this round should keep searching (enter the next loop round)
///
/// Continue condition: every tool_use this round is web_search (at least one) and the round limit has not been reached.
/// As soon as a client tool such as exec is mixed in, there is no tool_use at all, or the limit is reached, it stops and flushes (exec is never swallowed).
fn should_search_round(round_idx: usize, tool_uses: &[CompletedToolUse]) -> bool {
    let only_web_search = !tool_uses.is_empty() && tool_uses.iter().all(|t| t.name == "web_search");
    only_web_search && round_idx < MAX_WEB_SEARCH_ROUNDS
}

/// Whether the request is the continuation immediately following a tool result.
fn last_message_has_tool_result(payload: &MessagesRequest) -> bool {
    let Some(last) = payload.messages.last() else {
        return false;
    };
    if last.role != "user" {
        return false;
    }
    last.content.as_array().is_some_and(|blocks| {
        blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
    })
}

/// Decide how to handle a successful upstream round after tool output. Reasoning
/// by itself is intentionally not enough: Codex needs either assistant text (a
/// real final answer) or a client tool call to keep the task lifecycle sound.
fn empty_tool_result_disposition(
    payload: &MessagesRequest,
    round: &RoundOutcome,
    retries: usize,
) -> EmptyToolResultDisposition {
    let is_invalid_empty_continuation = last_message_has_tool_result(payload)
        && round.text.trim().is_empty()
        && round.tool_uses.is_empty()
        && round.stop_reason_override.is_none();
    if !is_invalid_empty_continuation {
        EmptyToolResultDisposition::Accept
    } else if retries < MAX_EMPTY_TOOL_RESULT_RETRIES {
        EmptyToolResultDisposition::Retry
    } else {
        EmptyToolResultDisposition::Fail
    }
}

/// Buffer-decode one round of the upstream streaming response
async fn decode_round(
    response: reqwest::Response,
    model: &str,
    tool_name_map: &std::collections::HashMap<String, String>,
    tracer: &RequestTracer,
) -> RoundOutcome {
    let mut body_stream = response.bytes_stream();
    let mut decoder = EventStreamDecoder::new();

    let mut text = String::new();
    let mut thinking = String::new();
    // id -> (name, json_buffer), preserving the order of appearance
    let mut buffers: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut tool_uses: Vec<CompletedToolUse> = Vec::new();
    let mut context_input_tokens: Option<i32> = None;
    let mut provider_token_usage: Option<TokenUsage> = None;
    let mut credits = 0.0;
    let mut last_metering: Option<MeteringEvent> = None;
    let mut stop_reason_override: Option<String> = None;
    let mut stream_error = None;

    while let Some(chunk) = body_stream.next().await {
        let chunk = match chunk {
            Ok(c) => {
                tracer.mark_first_token();
                c
            }
            Err(e) => {
                tracing::error!("web_search loop failed to read the response stream: {}", e);
                stream_error = Some(e.to_string());
                break;
            }
        };
        if let Err(e) = decoder.feed(&chunk) {
            tracing::warn!("buffer overflow: {}", e);
        }
        for result in decoder.decode_iter() {
            let frame = match result {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("failed to decode event: {}", e);
                    continue;
                }
            };
            let event = match Event::from_frame(frame) {
                Ok(ev) => ev,
                Err(_) => continue,
            };
            match event {
                Event::AssistantResponse(resp) => text.push_str(&resp.content),
                Event::ReasoningContent(r) => {
                    if let Some(t) = &r.text {
                        thinking.push_str(t);
                    }
                }
                Event::ToolUse(tu) => {
                    let entry = buffers.entry(tu.tool_use_id.clone()).or_insert_with(|| {
                        order.push(tu.tool_use_id.clone());
                        (String::new(), String::new())
                    });
                    if entry.0.is_empty() {
                        entry.0 = tu.name.clone();
                    }
                    entry.1.push_str(&tu.input);
                }
                Event::Metadata(metadata) => {
                    if let Some(usage) = metadata.token_usage {
                        // 单条流内重复 metadata 是快照，取最后一份。
                        provider_token_usage = Some(usage.sanitized());
                    }
                }
                Event::ContextUsage(cu) => {
                    let window = get_context_window_size(model);
                    let actual = (cu.context_usage_percentage * (window as f64) / 100.0) as i32;
                    context_input_tokens = Some(actual);
                    if cu.context_usage_percentage >= 100.0 {
                        stop_reason_override = Some("model_context_window_exceeded".to_string());
                    }
                }
                Event::Metering(m) => {
                    credits += m.usage;
                    last_metering = Some(m.clone());
                }
                Event::Exception { exception_type, .. } => {
                    if exception_type == "ContentLengthExceededException" {
                        stop_reason_override = Some("max_tokens".to_string());
                    }
                }
                _ => {}
            }
        }
    }

    // Assemble the complete tool_use in order of appearance (restoring the tool_name_map short name)
    for id in order {
        if let Some((name, buf)) = buffers.remove(&id) {
            let input: Value = if buf.is_empty() {
                json!({})
            } else {
                serde_json::from_str(&buf).unwrap_or_else(|e| {
                    tracing::warn!("failed to parse tool input JSON: {}", e);
                    json!({})
                })
            };
            // 统一还原入口（名字 + 入参），与流式 / 非流式路径同口径。
            tool_uses.push(CompletedToolUse::from_kiro(id, &name, input, tool_name_map));
        }
    }

    // 剥离混入文本的字面 <tool_use> XML 泄漏（与非流式同口径）。
    let text = crate::kiro::model::events::strip_tool_use_xml_leaks(&text);

    RoundOutcome {
        text,
        thinking,
        tool_uses,
        context_input_tokens,
        provider_token_usage,
        credits,
        last_metering,
        stop_reason_override,
        stream_error,
        // Populated by the caller (run_round), which holds ConversionResult::known_tool_names.
        known_tool_names: std::collections::HashSet::new(),
        // Populated by the caller (run_round), which holds ConversionResult::tool_name_map.
        tool_name_map: std::collections::HashMap::new(),
    }
}

/// A failed round plus any usage that can still be attributed to its provider call.
struct RoundFailure {
    response: Response,
    error_type: &'static str,
    error_message: String,
    credential_id: u64,
    token_usage: Option<TokenUsage>,
    credits: f64,
}

/// Run one upstream round (convert + streaming request + buffer decode).
///
/// Usage recording belongs to the outer loop so every terminal path writes exactly one
/// aggregate. A failure after a provider call carries the usage already observed in that call.
async fn run_round(
    provider: &Arc<KiroProvider>,
    payload: &MessagesRequest,
    fallback_input_tokens: i32,
    tracer: &RequestTracer,
    group: Option<&str>,
    tool_compatibility_mode: ToolCompatibilityMode,
) -> Result<(RoundOutcome, u64), RoundFailure> {
    let conversion = match convert_request_with_mode(payload, tool_compatibility_mode) {
        Ok(c) => c,
        Err(e) => {
            let (et, msg) = match &e {
                ConversionError::InvalidModel(reason) => (
                    "invalid_request_error",
                    format!("invalid model id: {}", reason),
                ),
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "message list is empty".to_string())
                }
                ConversionError::UnsupportedToolMapping(reason) => (
                    "invalid_request_error",
                    format!("unsupported tool mapping: {}", reason),
                ),
            };
            let error_message = msg.clone();
            return Err(RoundFailure {
                response: (StatusCode::BAD_REQUEST, Json(ErrorResponse::new(et, msg)))
                    .into_response(),
                error_type: outcome::BAD_REQUEST,
                error_message,
                credential_id: 0,
                token_usage: None,
                credits: 0.0,
            });
        }
    };

    let kiro_request = KiroRequest {
        conversation_state: conversion.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion.additional_model_request_fields,
    };
    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(b) => b,
        Err(e) => {
            let error_message = format!("failed to serialize request: {}", e);
            return Err(RoundFailure {
                response: (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse::new("internal_error", error_message.clone())),
                )
                    .into_response(),
                error_type: outcome::UNKNOWN,
                error_message,
                credential_id: 0,
                token_usage: None,
                credits: 0.0,
            });
        }
    };

    let call_result = match provider
        .call_api_stream(&request_body, Some(tracer), group)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let error_type = last_attempt_outcome(tracer).unwrap_or(outcome::UNKNOWN);
            let error_message = e.to_string();
            return Err(RoundFailure {
                response: map_provider_error(e),
                error_type,
                error_message,
                credential_id: 0,
                token_usage: Some(TokenUsage {
                    uncached_input_tokens: fallback_input_tokens.max(0),
                    ..TokenUsage::default()
                }),
                credits: 0.0,
            });
        }
    };
    let credential_id = call_result.credential_id;
    let mut outcome = decode_round(
        call_result.response,
        &payload.model,
        &conversion.tool_name_map,
        tracer,
    )
    .await;
    // Carry the declared tool names (original + shortened) so the flush step can run the
    // shared `<invoke>` text-leak fault tolerance with a correct tool-table guard.
    outcome.known_tool_names = conversion.known_tool_names;
    // Carry the short->original tool name map so reclaimed <invoke> names get restored.
    outcome.tool_name_map = conversion.tool_name_map;
    if let Some(error_message) = outcome.stream_error.take() {
        // The stream is partial and cannot re-enter the search loop, but any final metadata
        // snapshot/credits observed before the cut still belong to this real provider call.
        let token_usage = outcome.resolved_token_usage(fallback_input_tokens);
        return Err(RoundFailure {
            response: (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "upstream_error",
                    "Upstream response stream ended unexpectedly during the web_search loop."
                        .to_string(),
                )),
            )
                .into_response(),
            error_type: outcome::STREAM_INTERRUPTED,
            error_message,
            credential_id,
            token_usage: Some(token_usage),
            credits: outcome.credits,
        });
    }
    Ok((outcome, credential_id))
}

/// Feeds one round of assistant(text + web_search tool_use) + user(tool_result) back into payload.messages,
/// and appends server_tool_use + web_search_tool_result blocks (Contract A fields) to the presentation.
///
/// `searched` corresponds one-to-one (same order) to `round.tool_uses`; the search has already been completed.
fn append_search_round(
    payload: &mut MessagesRequest,
    round: &RoundOutcome,
    searched: &[Option<WebSearchResults>],
    presentation: &mut Vec<Value>,
) {
    // assistant: text + this round's web_search tool_use (Kiro history requires tool_use<->tool_result pairing)
    let mut assistant_content: Vec<Value> = Vec::new();
    if !round.text.is_empty() {
        assistant_content.push(json!({"type": "text", "text": round.text}));
    }
    for tu in &round.tool_uses {
        assistant_content.push(tu.to_anthropic_block());
    }
    payload.messages.push(Message {
        role: "assistant".to_string(),
        content: Value::Array(assistant_content),
    });

    // user: each web_search tool_use is paired with a tool_result (content = search summary, shown to the upstream)
    let mut user_content: Vec<Value> = Vec::new();
    for (tu, results) in round.tool_uses.iter().zip(searched.iter()) {
        let query = tool_query(tu).unwrap_or_default();
        let summary = websearch::generate_search_summary(&query, results);
        user_content.push(json!({
            "type": "tool_result", "tool_use_id": tu.id, "content": summary
        }));

        // Client presentation: server_tool_use + web_search_tool_result (Contract A)
        let (srv_id, _mcp) = websearch::create_mcp_request(&query);
        presentation.push(json!({
            "type": "server_tool_use", "id": srv_id, "name": "web_search",
            "input": {"query": query}
        }));
        // Contract A: web_search_tool_result has only type + content (no tool_use_id), consistent with generate_websearch_events
        presentation.push(json!({
            "type": "web_search_tool_result",
            "content": build_result_block(results)
        }));
    }
    payload.messages.push(Message {
        role: "user".to_string(),
        content: Value::Array(user_content),
    });
}

/// Converts search results into an array of web_search_result blocks (Contract A fields)
fn build_result_block(results: &Option<WebSearchResults>) -> Vec<Value> {
    match results {
        Some(r) => r
            .results
            .iter()
            .map(|item| {
                let page_age = item.published_date.and_then(|ms| {
                    chrono::DateTime::from_timestamp_millis(ms)
                        .map(|dt| dt.format("%B %-d, %Y").to_string())
                });
                json!({
                    "type": "web_search_result",
                    "title": item.title,
                    "url": item.url,
                    "encrypted_content": item.snippet.clone().unwrap_or_default(),
                    "page_age": page_age
                })
            })
            .collect(),
        None => vec![],
    }
}

/// Splits a round's tool_uses into (web_search calls, client tool calls),
/// preserving order within each group. This is the structural core of the
/// invariant "web_search is always handled internally and never leaves kiro-rs
/// as a raw tool_use": every flush path partitions first, then handles each
/// group differently (web_search -> presentation blocks, client tools -> raw).
fn partition_tool_uses(
    tool_uses: &[CompletedToolUse],
) -> (Vec<&CompletedToolUse>, Vec<&CompletedToolUse>) {
    let mut web = Vec::new();
    let mut client = Vec::new();
    for tu in tool_uses {
        if tu.name == "web_search" {
            web.push(tu);
        } else {
            client.push(tu);
        }
    }
    (web, client)
}

/// Resolves the final `stop_reason` for a flushed web_search-loop response.
///
/// Inputs:
/// - `override_reason`: an upstream-forced terminal reason (max_tokens /
///   model_context_window_exceeded). When present it always wins.
/// - `client_uses_empty`: whether the round had NO structured client tool_use.
/// - `content`: the FINAL flushed content (after the `<invoke>` fault tolerance may have
///   reclaimed a structured tool_use out of the assistant text).
///
/// Rules:
/// 1. An upstream override always wins (verbatim).
/// 2. Otherwise, if the final content contains a real (non-web_search) `tool_use` block,
///    the reason MUST be `tool_use` — this covers BOTH the structured case and the
///    reclaimed-from-text case (the common leak: model emits the call as text, so
///    `client_uses_empty` is true but a tool_use was reclaimed into `content`).
/// 3. Otherwise fall back to the structured signal: `tool_use` if the round had a client
///    tool_use, else `end_turn` (web_search-only rounds end as end_turn).
fn resolve_flush_stop_reason(
    override_reason: Option<&str>,
    client_uses_empty: bool,
    content: &[Value],
) -> String {
    if let Some(r) = override_reason {
        return r.to_string();
    }
    let has_client_tool_use = content
        .iter()
        .any(|c| c["type"] == "tool_use" && c["name"] != "web_search");
    if has_client_tool_use || !client_uses_empty {
        "tool_use".to_string()
    } else {
        "end_turn".to_string()
    }
}

/// Builds the final flush content with the web_search invariant baked in:
/// - any web_search tool_use becomes a `server_tool_use` + `web_search_tool_result`
///   presentation pair (NEVER a raw `tool_use`, which the Codex host rejects);
/// - client tools (exec, get_time, ...) are returned verbatim as raw `tool_use`.
///
/// `searched` corresponds one-to-one (same order) to `tool_uses`; entries for
/// web_search carry the already-completed search results, client-tool entries
/// are ignored (typically None).
///
/// `known_tool_names` is the set of tool names declared by the current request
/// (client short/long names). It is used to run the SAME `<invoke>` text-leak fault
/// tolerance as the streaming path (`stream.rs`): when the upstream model degrades
/// and emits a literal `<invoke name="...">...</invoke>` inside its assistant TEXT,
/// we reclaim it into a structured `tool_use` instead of passing the raw XML through.
/// The web_search loop builds its own SSE/content and historically bypassed that
/// fault tolerance entirely — this is the fix.
/// Canonical, order-independent key for a tool_use `input` JSON value, used to
/// detect that a reclaimed-from-text tool_use is identical to a structured one.
/// `serde_json::Value`'s `Map` is a BTreeMap (or preserves order when the
/// `preserve_order` feature is on); to be robust we serialize via a BTreeMap so
/// key order never affects equality.
fn canonical_input_key(input: &Value) -> String {
    match input {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            serde_json::to_string(&sorted).unwrap_or_else(|_| input.to_string())
        }
        _ => input.to_string(),
    }
}

fn build_flush_content(
    presentation: Vec<Value>,
    text: &str,
    tool_uses: &[CompletedToolUse],
    searched: &[Option<WebSearchResults>],
    known_tool_names: &std::collections::HashSet<String>,
    tool_name_map: &std::collections::HashMap<String, String>,
) -> Vec<Value> {
    let mut content: Vec<Value> = presentation;
    if !text.is_empty() {
        // Run the shared one-shot `<invoke>` sniffer: splits `text` into a sequence of
        // text blocks + reclaimed structured tool_use blocks (same safety gates as the
        // streaming fault tolerance). For clean text with no leaked `<invoke>`, this
        // returns a single text block identical to the old behavior.
        //
        // INVARIANT GUARD: `web_search` must NEVER be reclaimed as a raw client `tool_use`
        // — the Codex host has no web_search executor and rejects it with
        // "unsupported call: web_search". `known_tool_names` is copied verbatim from
        // req.tools and (since we are in the web_search loop) always contains "web_search",
        // so we strip it from the reclamation tool-table here. A leaked
        // `<invoke name="web_search">` then fails the tool-table gate and stays as plain
        // text (ugly but protocol-safe), instead of being upgraded into a raw tool_use that
        // breaks the loop's core invariant.
        let reclaim_tools: std::collections::HashSet<String> = known_tool_names
            .iter()
            .filter(|n| n.as_str() != "web_search")
            .cloned()
            .collect();
        // DEDUP GUARD: a degraded model can emit BOTH a leaked literal `<invoke>` in the
        // text AND the matching structured tool_use in `tool_uses`. Emitting both would
        // make the host execute the same command twice. Suppress any reclaimed-from-text
        // tool_use whose (name + canonical input) already appears in the structured
        // `tool_uses` for this round. Text blocks (and distinct tool_uses) are kept as-is.
        let structured_keys: std::collections::HashSet<(String, String)> = tool_uses
            .iter()
            .filter(|t| t.name != "web_search")
            .map(|t| (t.name.clone(), canonical_input_key(&t.input)))
            .collect();
        for block in
            super::stream::extract_invoke_content_blocks(text, &reclaim_tools, tool_name_map)
        {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let key = (
                    name.to_string(),
                    block
                        .get("input")
                        .map(canonical_input_key)
                        .unwrap_or_default(),
                );
                if structured_keys.contains(&key) {
                    // identical to a structured tool_use already emitted below -> drop the
                    // reclaimed duplicate (avoid double execution).
                    continue;
                }
            }
            content.push(block);
        }
    }
    for (idx, tu) in tool_uses.iter().enumerate() {
        if tu.name == "web_search" {
            // INVARIANT: present as server_tool_use + web_search_tool_result,
            // never as a raw tool_use.
            let query = tool_query(tu).unwrap_or_default();
            let (srv_id, _mcp) = websearch::create_mcp_request(&query);
            content.push(json!({
                "type": "server_tool_use", "id": srv_id, "name": "web_search",
                "input": {"query": query}
            }));
            let results: &Option<WebSearchResults> = searched.get(idx).unwrap_or(&None);
            content.push(json!({
                "type": "web_search_tool_result",
                "content": build_result_block(results)
            }));
        } else {
            // Client tool (exec, get_time, ...): returned to the client verbatim.
            content.push(tu.to_anthropic_block());
        }
    }
    content
}

fn record_aggregated_usage(
    hook: &UsageRecordHook,
    credential_id: u64,
    usage: TokenUsage,
    credits: f64,
    status: &str,
) {
    let usage = usage.sanitized();
    hook.record(
        credential_id,
        usage.uncached_input_tokens,
        usage.output_tokens,
        usage.cache_write_input_tokens,
        usage.cache_read_input_tokens,
        credits,
        status,
    );
}

/// Cancellation-safe, exactly-once accounting for the multi-round search loop.
/// The snapshot is updated after every completed provider round, so cancelling
/// a later MCP/provider await still records all usage and trace attempts already
/// observed.
struct WebSearchUsageSettlement {
    hook: UsageRecordHook,
    tracer: Option<Arc<RequestTracer>>,
    credential_id: u64,
    usage: TokenUsage,
    credits: f64,
    settled: bool,
}

impl WebSearchUsageSettlement {
    fn new(hook: UsageRecordHook, tracer: Arc<RequestTracer>) -> Self {
        Self {
            hook,
            tracer: Some(tracer),
            credential_id: 0,
            usage: TokenUsage::default(),
            credits: 0.0,
            settled: false,
        }
    }

    #[cfg(test)]
    fn without_trace(hook: UsageRecordHook) -> Self {
        Self {
            hook,
            tracer: None,
            credential_id: 0,
            usage: TokenUsage::default(),
            credits: 0.0,
            settled: false,
        }
    }

    fn add(&mut self, credential_id: u64, usage: TokenUsage, credits: f64) {
        if credential_id != 0 {
            self.credential_id = credential_id;
        }
        self.usage = self.usage.saturating_add(usage);
        if credits.is_finite() && credits > 0.0 {
            self.credits += credits;
        }
    }

    fn usage(&self) -> TokenUsage {
        self.usage.sanitized()
    }

    fn finish(
        &mut self,
        usage_status: &str,
        trace_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
    ) {
        if self.settled {
            return;
        }
        record_aggregated_usage(
            &self.hook,
            self.credential_id,
            self.usage,
            self.credits,
            usage_status,
        );
        if let Some(tracer) = &self.tracer {
            finalize_aggregated_trace(
                tracer,
                trace_status,
                error_type,
                error_message,
                self.usage,
                self.credits,
            );
        }
        self.settled = true;
    }
}

impl Drop for WebSearchUsageSettlement {
    fn drop(&mut self) {
        if !self.settled {
            self.finish(
                "error",
                "interrupted",
                Some(outcome::STREAM_INTERRUPTED),
                Some("web_search loop was cancelled before completion"),
            );
        }
    }
}

#[derive(Debug)]
struct PendingWebSearch {
    block_index: i32,
}

/// Emits one coherent Anthropic SSE message while the agentic loop runs in a
/// background task. Content block indexes are allocated once across all search
/// rounds and the final assistant content, so downstream Responses translation
/// sees a single ordered stream instead of several synthetic messages.
struct WebSearchSseEmitter {
    sender: mpsc::Sender<Bytes>,
    next_block_index: i32,
    active_blocks: BTreeSet<i32>,
    terminal: bool,
}

impl WebSearchSseEmitter {
    fn new(sender: mpsc::Sender<Bytes>) -> Self {
        Self {
            sender,
            next_block_index: 0,
            active_blocks: BTreeSet::new(),
            terminal: false,
        }
    }

    async fn send(&self, event: SseEvent) {
        if self
            .sender
            .send(Bytes::from(event.to_sse_string()))
            .await
            .is_err()
        {
            tracing::debug!("web_search SSE receiver disconnected");
        }
    }

    async fn begin_search(&mut self, query: &str) -> PendingWebSearch {
        let block_index = self.next_block_index;
        self.next_block_index += 1;
        self.active_blocks.insert(block_index);
        let (tool_use_id, _) = websearch::create_mcp_request(query);
        self.send(SseEvent::new(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "id": tool_use_id,
                    "type": "server_tool_use",
                    "name": "web_search",
                    "input": {"query": query}
                }
            }),
        ))
        .await;
        PendingWebSearch { block_index }
    }

    /// Closes a search block that was opened via `begin_search` but must not be
    /// presented as a successful result — the MCP call itself failed. Sends only
    /// the matching `content_block_stop` (no `web_search_tool_result`), keeping
    /// every `content_block_start` paired before the caller's terminal `error`
    /// event, exactly like `stream.rs::generate_final_events` closes open blocks
    /// before a mid-stream tool error.
    async fn abort_search(&mut self, pending: PendingWebSearch) {
        self.active_blocks.remove(&pending.block_index);
        self.send(SseEvent::new(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": pending.block_index
            }),
        ))
        .await;
    }

    async fn complete_search(
        &mut self,
        pending: PendingWebSearch,
        results: &Option<WebSearchResults>,
    ) {
        self.active_blocks.remove(&pending.block_index);
        self.send(SseEvent::new(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": pending.block_index
            }),
        ))
        .await;

        let result_index = self.next_block_index;
        self.next_block_index += 1;
        self.active_blocks.insert(result_index);
        self.send(SseEvent::new(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": result_index,
                "content_block": {
                    "type": "web_search_tool_result",
                    "content": build_result_block(results)
                }
            }),
        ))
        .await;
        self.active_blocks.remove(&result_index);
        self.send(SseEvent::new(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": result_index
            }),
        ))
        .await;
    }

    async fn finish(
        &mut self,
        content: Vec<Value>,
        stop_reason: &str,
        token_usage: TokenUsage,
        thinking: &str,
        metering: Option<&MeteringEvent>,
    ) {
        let content = without_web_search_presentation(content);
        let (mut events, next_index) = build_sse_content_events(&content, self.next_block_index);
        self.next_block_index = next_index;
        // Keep the extension on a standard Anthropic event so ordinary Messages
        // clients can ignore the unknown field. Responses consumes it before
        // translating that event, which puts reasoning ahead of final answer/tools.
        let reasoning_attached_to_content = if thinking.is_empty() {
            false
        } else if let Some(first) = events.first_mut() {
            first.data["kiro_thinking"] = json!(thinking);
            true
        } else {
            false
        };
        for event in events {
            self.send(event).await;
        }

        let token_usage = token_usage.sanitized();
        let mut usage = json!({
            "input_tokens": token_usage.uncached_input_tokens,
            "output_tokens": token_usage.output_tokens,
            "cache_creation_input_tokens": token_usage.cache_write_input_tokens,
            "cache_read_input_tokens": token_usage.cache_read_input_tokens
        });
        if let Some(metering) = metering {
            usage["credit_usage"] = json!(metering.usage);
            usage["credit_unit"] = json!(metering.unit);
            usage["credit_unit_plural"] = json!(metering.unit_plural);
        }
        let mut message_delta = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason},
            "usage": usage
        });
        if !reasoning_attached_to_content && !thinking.is_empty() {
            message_delta["kiro_thinking"] = json!(thinking);
        }
        self.send(SseEvent::new("message_delta", message_delta))
            .await;
        self.send(SseEvent::new(
            "message_stop",
            json!({"type": "message_stop"}),
        ))
        .await;
        self.terminal = true;
    }

    async fn fail(&mut self, error_type: &str, message: &str) {
        if self.terminal {
            return;
        }
        let active_blocks = std::mem::take(&mut self.active_blocks);
        for index in active_blocks {
            self.send(SseEvent::new(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
            ))
            .await;
        }
        self.terminal = true;
        self.send(SseEvent::new(
            "error",
            json!({
                "type": "error",
                "error": {"type": error_type, "message": message}
            }),
        ))
        .await;
    }
}

/// Run a streamed web-search task only while its response receiver is alive.
/// Dropping the future cancels an in-flight provider or MCP await, preventing
/// detached work and additional metering after the client disconnects.
async fn while_receiver_open<F>(sender: &mpsc::Sender<Bytes>, future: F) -> Option<F::Output>
where
    F: Future,
{
    // Keep the future in an owned pin so the cancellation branch can explicitly
    // drop it before returning. This is important for cancellation-safe usage
    // settlement: callers may inspect accounting immediately after this helper
    // resolves.
    let mut future = Box::pin(future);
    let result = tokio::select! {
        biased;
        output = &mut future => Some(output),
        _ = sender.closed() => None,
    };
    drop(future);
    result
}

fn initial_stream_event(model: &str, input_tokens: i32) -> SseEvent {
    let message_id = format!("msg_{}", &Uuid::new_v4().to_string().replace('-', "")[..24]);
    SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": input_tokens.max(0),
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        }),
    )
}

fn render_channel_sse(initial_event: SseEvent, receiver: mpsc::Receiver<Bytes>) -> Response {
    let initial = stream::iter([Ok::<Bytes, Infallible>(Bytes::from(
        initial_event.to_sse_string(),
    ))]);
    let keepalive = interval_at(
        Instant::now() + Duration::from_secs(WEB_SEARCH_PING_INTERVAL_SECS),
        Duration::from_secs(WEB_SEARCH_PING_INTERVAL_SECS),
    );
    let updates = stream::unfold(
        (receiver, keepalive),
        |(mut receiver, mut keepalive)| async move {
            tokio::select! {
                item = receiver.recv() => item.map(|bytes| {
                    (Ok::<Bytes, Infallible>(bytes), (receiver, keepalive))
                }),
                _ = keepalive.tick() => Some((
                    Ok::<Bytes, Infallible>(Bytes::from(
                        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
                    )),
                    (receiver, keepalive),
                )),
            }
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(initial.chain(updates)))
        .unwrap()
}

async fn response_error_details(response: Response) -> (String, String) {
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let error_type = value
        .pointer("/error/type")
        .and_then(Value::as_str)
        .unwrap_or("api_error")
        .to_string();
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("web_search agentic loop failed with HTTP {status}"));
    (error_type, message)
}

fn without_web_search_presentation(content: Vec<Value>) -> Vec<Value> {
    content
        .into_iter()
        .filter(|block| {
            !matches!(
                block.get("type").and_then(Value::as_str),
                Some("server_tool_use" | "web_search_tool_result")
            )
        })
        .collect()
}

/// Best-effort string extraction from a `catch_unwind` payload (`Box<dyn Any + Send>`).
/// Panics almost always carry `&str` or `String` (the `panic!`/`unwrap`/`expect` message);
/// anything else is reported as a fixed placeholder rather than failing to log at all.
fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

async fn execute_web_search(
    provider: &Arc<KiroProvider>,
    tool_use: &CompletedToolUse,
    tracer: &RequestTracer,
    group: Option<&str>,
    final_round: bool,
    emitter: &mut Option<&mut WebSearchSseEmitter>,
) -> anyhow::Result<Option<WebSearchResults>> {
    let query = tool_query(tool_use);
    let pending = if let Some(emitter) = emitter.as_deref_mut() {
        Some(emitter.begin_search(query.as_deref().unwrap_or("")).await)
    } else {
        None
    };

    let result = if let Some(query) = query {
        log_normalized_web_search_query(tool_use, &query);
        let (_, mcp_request) = websearch::create_mcp_request(&query);
        match websearch::call_mcp_api(provider, &mcp_request, Some(tracer), group).await {
            Ok(response) => websearch::parse_search_results(&response),
            Err(error) if websearch::is_no_results_mcp_error(&error) => {
                tracing::warn!(
                    final_round,
                    "web_search MCP returned no results; continuing with an empty result"
                );
                None
            }
            Err(error) => {
                // The MCP call itself failed: close the `server_tool_use` block
                // opened above instead of leaving it dangling for the caller's
                // terminal `error` event (every content_block_start must pair
                // with a content_block_stop before a stream ends).
                if let (Some(emitter), Some(pending)) = (emitter.as_deref_mut(), pending) {
                    emitter.abort_search(pending).await;
                }
                return Err(error);
            }
        }
    } else {
        log_invalid_web_search_input(tool_use);
        None
    };

    if let (Some(emitter), Some(pending)) = (emitter.as_deref_mut(), pending) {
        emitter.complete_search(pending, &result).await;
    }
    Ok(result)
}

fn aggregated_trace_usage(usage: TokenUsage, credits: f64) -> TraceUsage {
    let usage = usage.sanitized();
    TraceUsage {
        input_tokens: usage.uncached_input_tokens as u64,
        output_tokens: usage.output_tokens as u64,
        cache_creation_tokens: usage.cache_write_input_tokens as u64,
        cache_read_tokens: usage.cache_read_input_tokens as u64,
        credits: if credits.is_finite() && credits > 0.0 {
            credits
        } else {
            0.0
        },
    }
}

fn finalize_aggregated_trace(
    tracer: &RequestTracer,
    status: &str,
    error_type: Option<&str>,
    error_message: Option<&str>,
    usage: TokenUsage,
    credits: f64,
) {
    tracer.finalize(
        status,
        error_type,
        error_message,
        None,
        aggregated_trace_usage(usage, credits),
    );
}

/// web_search loop entry point
///
/// `stream_client`: whether the client wants SSE (true) or a single JSON response (false).
pub(super) async fn run_web_search_loop(
    provider: Arc<KiroProvider>,
    payload: MessagesRequest,
    hook: UsageRecordHook,
    tracer: Arc<RequestTracer>,
    stream_client: bool,
    group: Option<String>,
    tool_compatibility_mode: ToolCompatibilityMode,
) -> Response {
    if !stream_client {
        return run_web_search_loop_inner(
            provider,
            payload,
            hook,
            tracer,
            group,
            tool_compatibility_mode,
            None,
        )
        .await;
    }

    let initial_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;
    let initial_event = initial_stream_event(&payload.model, initial_input_tokens);
    let (sender, receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
    tokio::spawn(async move {
        let receiver_guard = sender.clone();
        let mut emitter = WebSearchSseEmitter::new(sender);
        // Guard against a panic anywhere in the agentic loop (upstream decode bug,
        // MCP response parsing, etc.): without this, unwinding drops `emitter`
        // (and its `sender`) with no terminal frame, so the client's SSE stream
        // would just end silently instead of surfacing response.failed/error.
        let Some(outcome) = while_receiver_open(
            &receiver_guard,
            AssertUnwindSafe(run_web_search_loop_inner(
                provider,
                payload,
                hook,
                tracer,
                group,
                tool_compatibility_mode,
                Some(&mut emitter),
            ))
            .catch_unwind(),
        )
        .await
        else {
            tracing::debug!("web_search SSE receiver disconnected; cancelling agentic loop");
            return;
        };
        match outcome {
            Ok(response) => {
                if !emitter.terminal {
                    let (error_type, message) = response_error_details(response).await;
                    emitter.fail(&error_type, &message).await;
                }
            }
            Err(panic) => {
                tracing::error!(
                    panic = %panic_message(&panic),
                    "web_search agentic loop panicked"
                );
                if !emitter.terminal {
                    emitter
                        .fail("internal_error", "web_search agentic loop panicked")
                        .await;
                }
            }
        }
    });

    render_channel_sse(initial_event, receiver)
}

async fn run_web_search_loop_inner(
    provider: Arc<KiroProvider>,
    mut payload: MessagesRequest,
    hook: UsageRecordHook,
    tracer: Arc<RequestTracer>,
    group: Option<String>,
    tool_compatibility_mode: ToolCompatibilityMode,
    mut emitter: Option<&mut WebSearchSseEmitter>,
) -> Response {
    let mut presentation: Vec<Value> = Vec::new();
    let mut settlement = WebSearchUsageSettlement::new(hook, tracer.clone());
    let mut latest_metering: Option<MeteringEvent> = None;
    let mut all_thinking = String::new();

    for round_idx in 0..=MAX_WEB_SEARCH_ROUNDS {
        let mut empty_retries = 0usize;
        let round = loop {
            let round_fallback_input_tokens = token::count_all_tokens(
                payload.model.clone(),
                payload.system.clone(),
                payload.messages.clone(),
                payload.tools.clone(),
            ) as i32;
            let (round, credential_id) = match run_round(
                &provider,
                &payload,
                round_fallback_input_tokens,
                tracer.as_ref(),
                group.as_deref(),
                tool_compatibility_mode,
            )
            .await
            {
                Ok(v) => v,
                Err(failure) => {
                    if let Some(usage) = failure.token_usage {
                        settlement.add(failure.credential_id, usage, failure.credits);
                    } else {
                        settlement.add(
                            failure.credential_id,
                            TokenUsage::default(),
                            failure.credits,
                        );
                    }
                    settlement.finish(
                        "error",
                        "error",
                        Some(failure.error_type),
                        Some(&failure.error_message),
                    );
                    return failure.response;
                }
            };
            settlement.add(
                credential_id,
                round.resolved_token_usage(round_fallback_input_tokens),
                round.credits,
            );
            // 跨 round 保留最近一次 meteringEvent，多 round 时取最后一次
            // (clone 以避免与 empty_tool_result_disposition 后续对 round 的借用冲突)。
            if let Some(ref m) = round.last_metering {
                latest_metering = Some(m.clone());
            }

            match empty_tool_result_disposition(&payload, &round, empty_retries) {
                EmptyToolResultDisposition::Accept => {}
                EmptyToolResultDisposition::Retry => {
                    empty_retries += 1;
                    tracing::warn!(
                        round = round_idx,
                        retry = empty_retries,
                        "upstream returned an empty assistant turn after tool_result; retrying"
                    );
                    continue;
                }
                EmptyToolResultDisposition::Fail => {
                    settlement.finish(
                        "error",
                        "error",
                        Some(outcome::UNKNOWN),
                        Some(
                            "Upstream returned no assistant text or tool call after a tool result.",
                        ),
                    );
                    tracing::error!(
                        round = round_idx,
                        "upstream repeated an empty assistant turn after tool_result"
                    );
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(ErrorResponse::new(
                            "upstream_error",
                            "Upstream returned no assistant text or tool call after a tool result."
                                .to_string(),
                        )),
                    )
                        .into_response();
                }
            }

            // Only surface reasoning from the accepted attempt. An empty attempt is
            // discarded and retried, so replaying its hidden reasoning would duplicate
            // or contradict the successful attempt's summary.
            if !round.thinking.is_empty() {
                if !all_thinking.is_empty() {
                    all_thinking.push_str("\n\n");
                }
                all_thinking.push_str(&round.thinking);
            }

            break round;
        };

        if should_search_round(round_idx, &round.tool_uses) {
            // Real search: if any one fails -> propagate the error, never silently turn it into "No results found"
            let mut searched: Vec<Option<WebSearchResults>> =
                Vec::with_capacity(round.tool_uses.len());
            for tu in &round.tool_uses {
                match execute_web_search(
                    &provider,
                    tu,
                    tracer.as_ref(),
                    group.as_deref(),
                    false,
                    &mut emitter,
                )
                .await
                {
                    Ok(result) => searched.push(result),
                    Err(e) => {
                        tracing::warn!("web_search MCP call failed: {}", e);
                        let error_message = e.to_string();
                        settlement.finish(
                            "error",
                            "error",
                            last_attempt_outcome(tracer.as_ref()),
                            Some(&error_message),
                        );
                        return map_provider_error(e);
                    }
                }
            }
            append_search_round(&mut payload, &round, &searched, &mut presentation);
            continue;
        }

        // Terminate: this round is not "pure web_search", or the limit has been reached -> flush to the client.
        // stop_reason must reflect CLIENT tools only: web_search is handled internally
        // (presented as server_tool_use, not a pending tool_use), so a round with only
        // web_search must end as "end_turn", not "tool_use" (otherwise the host would
        // wait for a client tool call that is never emitted).
        let (_web_uses, client_uses) = partition_tool_uses(&round.tool_uses);
        // INVARIANT: web_search is ALWAYS executed internally and is NEVER flushed
        // as a raw tool_use (the Codex host has no executor for it and rejects it
        // with "unsupported call: web_search"). This covers the mixed-round case
        // (web_search + exec) and the round-limit case: search every web_search call
        // in this final round here, then build the flushed content with web_search
        // presented as server_tool_use + web_search_tool_result while client tools
        // (exec, etc.) are returned verbatim.
        let mut searched: Vec<Option<WebSearchResults>> = Vec::with_capacity(round.tool_uses.len());
        for tu in &round.tool_uses {
            if tu.name == "web_search" {
                match execute_web_search(
                    &provider,
                    tu,
                    tracer.as_ref(),
                    group.as_deref(),
                    true,
                    &mut emitter,
                )
                .await
                {
                    Ok(result) => searched.push(result),
                    Err(e) => {
                        tracing::warn!("web_search MCP call (final round) failed: {}", e);
                        let error_message = e.to_string();
                        settlement.finish(
                            "error",
                            "error",
                            last_attempt_outcome(tracer.as_ref()),
                            Some(&error_message),
                        );
                        return map_provider_error(e);
                    }
                }
            } else {
                searched.push(None);
            }
        }
        let content = build_flush_content(
            presentation.clone(),
            &round.text,
            &round.tool_uses,
            &searched,
            &round.known_tool_names,
            &round.tool_name_map,
        );
        // stop_reason must be computed from the FINAL flushed content, not just
        // round.tool_uses: the <invoke> fault tolerance can reclaim a structured tool_use
        // out of the assistant text (the common leak case where the model emits the call as
        // text and round.tool_uses is empty). See resolve_flush_stop_reason for the rules.
        let stop_reason = resolve_flush_stop_reason(
            round.stop_reason_override.as_deref(),
            client_uses.is_empty(),
            &content,
        );

        let final_usage = settlement.usage();
        settlement.finish("success", "success", None, None);

        return if let Some(emitter) = emitter.as_deref_mut() {
            emitter
                .finish(
                    content,
                    &stop_reason,
                    final_usage,
                    &all_thinking,
                    latest_metering.as_ref(),
                )
                .await;
            StatusCode::OK.into_response()
        } else {
            render_json(
                &payload.model,
                content,
                &stop_reason,
                final_usage,
                &all_thinking,
                latest_metering.as_ref(),
            )
        };
    }

    // Theoretically unreachable (the loop always returns)
    settlement.finish(
        "error",
        "error",
        Some(outcome::UNKNOWN),
        Some("web_search loop exited unexpectedly"),
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(
            "internal_error",
            "web_search loop exited unexpectedly",
        )),
    )
        .into_response()
}

/// Single JSON response (non-streaming)
///
/// `thinking`: optional out-of-band reasoning text. Emitted as a TOP-LEVEL
/// `kiro_thinking` field (NOT a content block): Anthropic clients ignore
/// unknown top-level fields and thus never replay an unsigned thinking block
/// upstream, while the Responses translator picks it up for codex's
/// reasoning-summary display.
pub(crate) fn render_json(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    token_usage: TokenUsage,
    thinking: &str,
    metering: Option<&MeteringEvent>,
) -> Response {
    let token_usage = token_usage.sanitized();
    let mut usage = json!({
        "input_tokens": token_usage.uncached_input_tokens,
        "output_tokens": token_usage.output_tokens,
        "cache_creation_input_tokens": token_usage.cache_write_input_tokens,
        "cache_read_input_tokens": token_usage.cache_read_input_tokens
    });
    // 透传上游 meteringEvent 的 credit_* 字段，让客户端拿到与 Kiro 后端口径
    // 一致的计费元数据；只在收到过 meteringEvent 时才追加。
    if let Some(m) = metering {
        usage["credit_usage"] = json!(m.usage);
        usage["credit_unit"] = json!(m.unit);
        usage["credit_unit_plural"] = json!(m.unit_plural);
    }
    let mut body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage
    });
    if !thinking.is_empty() {
        body["kiro_thinking"] = json!(thinking);
    }
    (StatusCode::OK, Json(body)).into_response()
}

/// Renders the final content array into a sequence of SSE events
#[cfg(test)]
fn build_sse_events(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    token_usage: TokenUsage,
    metering: Option<&MeteringEvent>,
) -> Vec<SseEvent> {
    let token_usage = token_usage.sanitized();
    let mut events = Vec::new();
    let message_id = format!("msg_{}", &Uuid::new_v4().to_string().replace('-', "")[..24]);

    events.push(SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": token_usage.uncached_input_tokens,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": token_usage.cache_write_input_tokens,
                    "cache_read_input_tokens": token_usage.cache_read_input_tokens
                }
            }
        }),
    ));

    let (content_events, _) = build_sse_content_events(&content, 0);
    events.extend(content_events);

    let mut message_delta_usage = json!({ "output_tokens": token_usage.output_tokens });
    // 透传上游 meteringEvent 的 credit_* 字段（仅在拿到 meteringEvent 时）。
    if let Some(m) = metering {
        message_delta_usage["credit_usage"] = json!(m.usage);
        message_delta_usage["credit_unit"] = json!(m.unit);
        message_delta_usage["credit_unit_plural"] = json!(m.unit_plural);
    }
    events.push(SseEvent::new(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason},
            "usage": message_delta_usage
        }),
    ));
    events.push(SseEvent::new(
        "message_stop",
        json!({"type": "message_stop"}),
    ));

    events
}

fn build_sse_content_events(content: &[Value], start_index: i32) -> (Vec<SseEvent>, i32) {
    let mut events = Vec::new();
    let mut next_index = start_index;
    for block in content {
        let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(
            btype,
            "text" | "tool_use" | "server_tool_use" | "web_search_tool_result"
        ) {
            continue;
        }
        let index = next_index;
        next_index += 1;
        match btype {
            "text" => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop", "index": index
                    }),
                ));
            }
            "tool_use" => {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let partial = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta", "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": partial}
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop", "index": index
                    }),
                ));
            }
            "server_tool_use" | "web_search_tool_result" => {
                events.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": index,
                        "content_block": block
                    }),
                ));
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop", "index": index
                    }),
                ));
            }
            _ => {}
        }
    }
    (events, next_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::websearch::{WebSearchResult, WebSearchResults};

    fn decode_sse(bytes: Bytes) -> (String, Value) {
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let event = text
            .lines()
            .find_map(|line| line.strip_prefix("event: "))
            .unwrap()
            .to_string();
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        (event, serde_json::from_str(data).unwrap())
    }

    fn drain_sse(receiver: &mut mpsc::Receiver<Bytes>) -> Vec<(String, Value)> {
        let mut events = Vec::new();
        while let Ok(bytes) = receiver.try_recv() {
            events.push(decode_sse(bytes));
        }
        events
    }

    fn tu(name: &str) -> CompletedToolUse {
        CompletedToolUse {
            id: format!("toolu_{}", name),
            name: name.to_string(),
            input: json!({"query": "rust 2026"}),
        }
    }

    fn tu_with_input(input: Value) -> CompletedToolUse {
        CompletedToolUse {
            id: "toolu_web_search".to_string(),
            name: "web_search".to_string(),
            input,
        }
    }

    #[test]
    fn tool_query_normalizes_supported_input_shapes() {
        assert_eq!(
            tool_query(&tu_with_input(json!({"query": "  rust 2026  "}))),
            Some("rust 2026".to_string())
        );
        assert_eq!(
            tool_query(&tu_with_input(json!({"search_query": "南京演唱会"}))),
            Some("南京演唱会".to_string())
        );
        assert_eq!(
            tool_query(&tu_with_input(json!({"queries": ["", "上海天气"]}))),
            Some("上海天气".to_string())
        );
        assert_eq!(
            tool_query(&tu_with_input(json!({"query": {"text": "Paris weather"}}))),
            Some("Paris weather".to_string())
        );
    }

    #[test]
    fn tool_query_rejects_missing_or_non_string_input() {
        assert_eq!(tool_query(&tu_with_input(json!({"query": "   "}))), None);
        assert_eq!(tool_query(&tu_with_input(json!({"query": 42}))), None);
        assert_eq!(tool_query(&tu_with_input(json!({"other": true}))), None);
    }

    #[test]
    fn no_results_mcp_error_is_nonfatal() {
        assert!(websearch::is_no_results_mcp_error(&anyhow::anyhow!(
            "MCP error: -32602 - Tool returned no results"
        )));
        assert!(!websearch::is_no_results_mcp_error(&anyhow::anyhow!(
            "MCP error: -32602 - Invalid tool parameters provided"
        )));
    }

    #[tokio::test]
    async fn channel_response_exposes_message_start_before_background_progress() {
        let (sender, receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
        let response = render_channel_sse(initial_stream_event("gpt-5.6-sol", 17), receiver);
        let mut body = response.into_body().into_data_stream();

        let first = tokio::time::timeout(Duration::from_millis(100), body.next())
            .await
            .expect("the initial SSE event must not wait for the background loop")
            .expect("the response must contain an initial event")
            .unwrap();
        let (event, data) = decode_sse(first);
        assert_eq!(event, "message_start");
        assert_eq!(data["message"]["usage"]["input_tokens"], json!(17));

        // Keep the sender alive through the assertion: the event came from the
        // response prefix rather than from progress-channel completion.
        drop(sender);
    }

    #[tokio::test]
    async fn streaming_emitter_orders_progress_and_deduplicates_final_search() {
        let (sender, mut receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
        let mut emitter = WebSearchSseEmitter::new(sender);

        let pending = emitter.begin_search("Rust 2026").await;
        let started = drain_sse(&mut receiver);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, "content_block_start");
        assert_eq!(started[0].1["index"], json!(0));
        assert_eq!(
            started[0].1["content_block"]["type"],
            json!("server_tool_use")
        );
        assert_eq!(
            started[0].1["content_block"]["input"]["query"],
            json!("Rust 2026")
        );
        assert!(receiver.try_recv().is_err(), "search remains in progress");

        let results = fake_results("Rust 2026");
        emitter.complete_search(pending, &results).await;
        let mut events = started;
        events.extend(drain_sse(&mut receiver));

        let metering = metering_event(0.75);
        emitter
            .finish(
                vec![
                    json!({
                        "type": "server_tool_use", "id": "duplicate",
                        "name": "web_search", "input": {"query": "Rust 2026"}
                    }),
                    json!({
                        "type": "web_search_tool_result",
                        "content": [{"type": "web_search_result"}]
                    }),
                    json!({"type": "text", "text": "final answer"}),
                    json!({
                        "type": "tool_use", "id": "toolu_exec",
                        "name": "exec", "input": {"cmd": "pwd"}
                    }),
                ],
                "tool_use",
                TokenUsage {
                    uncached_input_tokens: 3,
                    output_tokens: 5,
                    cache_read_input_tokens: 7,
                    cache_write_input_tokens: 4,
                },
                "search reasoning",
                Some(&metering),
            )
            .await;
        events.extend(drain_sse(&mut receiver));

        assert_eq!(
            1 + events
                .iter()
                .filter(|(event, _)| event == "message_start")
                .count(),
            1,
            "message_start is emitted only by the channel response prefix"
        );

        let starts: Vec<&Value> = events
            .iter()
            .filter(|(event, _)| event == "content_block_start")
            .map(|(_, data)| data)
            .collect();
        let start_indexes: Vec<i64> = starts
            .iter()
            .map(|data| data["index"].as_i64().unwrap())
            .collect();
        assert_eq!(start_indexes, vec![0, 1, 2, 3]);
        assert_eq!(
            starts
                .iter()
                .filter(|data| data["content_block"]["type"] == "server_tool_use")
                .count(),
            1,
            "the final flush must not repeat an already streamed search"
        );
        assert_eq!(
            starts
                .iter()
                .filter(|data| data["content_block"]["type"] == "web_search_tool_result")
                .count(),
            1,
            "the final flush must not repeat an already streamed result"
        );

        let stops: Vec<i64> = events
            .iter()
            .filter(|(event, _)| event == "content_block_stop")
            .map(|(_, data)| data["index"].as_i64().unwrap())
            .collect();
        assert_eq!(stops, vec![0, 1, 2, 3]);

        let delta = events
            .iter()
            .find(|(event, _)| event == "message_delta")
            .map(|(_, data)| data)
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], json!("tool_use"));
        assert_eq!(delta["usage"]["input_tokens"], json!(3));
        assert_eq!(delta["usage"]["output_tokens"], json!(5));
        assert_eq!(delta["usage"]["cache_creation_input_tokens"], json!(4));
        assert_eq!(delta["usage"]["cache_read_input_tokens"], json!(7));
        assert_eq!(delta["usage"]["credit_usage"], json!(0.75));
        let reasoning_carrier = events
            .iter()
            .find(|(_, data)| data["kiro_thinking"] == "search reasoning")
            .expect("reasoning must be attached before final visible content");
        let reasoning_position = events
            .iter()
            .position(|event| std::ptr::eq(event, reasoning_carrier))
            .unwrap();
        let final_text_position = events
            .iter()
            .position(|(event, data)| {
                event == "content_block_delta"
                    && data["delta"]["type"] == "text_delta"
                    && data["delta"]["text"] == "final answer"
            })
            .unwrap();
        assert!(reasoning_position < final_text_position);
        assert_eq!(events.last().unwrap().0, "message_stop");
        assert!(emitter.terminal);
    }

    #[tokio::test]
    async fn receiver_disconnect_cancels_in_flight_work() {
        struct CancellationProbe(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for CancellationProbe {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let (sender, receiver) = mpsc::channel::<Bytes>(1);
        let (cancelled_sender, cancelled_receiver) = tokio::sync::oneshot::channel();
        let probe = CancellationProbe(Some(cancelled_sender));
        let in_flight = async move {
            let _probe = probe;
            futures::future::pending::<()>().await;
        };

        drop(receiver);
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            while_receiver_open(&sender, in_flight),
        )
        .await
        .expect("closed receiver must cancel the in-flight future");
        assert!(result.is_none());
        tokio::time::timeout(Duration::from_millis(100), cancelled_receiver)
            .await
            .expect("in-flight future must be dropped")
            .expect("cancellation probe must be notified");
    }

    #[tokio::test]
    async fn receiver_disconnect_settles_accumulated_usage_once() {
        let aggregator = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
        let hook = UsageRecordHook {
            recorder: None,
            aggregator: Some(aggregator.clone()),
            client_keys: None,
            key_id: 0,
            model: "test-model".to_string(),
            started_at: std::time::Instant::now(),
        };
        let (sender, receiver) = mpsc::channel::<Bytes>(1);
        let in_flight = async move {
            let mut settlement = WebSearchUsageSettlement::without_trace(hook);
            settlement.add(
                7,
                TokenUsage {
                    uncached_input_tokens: 3,
                    output_tokens: 5,
                    cache_read_input_tokens: 7,
                    cache_write_input_tokens: 4,
                },
                0.75,
            );
            futures::future::pending::<()>().await;
        };

        drop(receiver);
        assert!(while_receiver_open(&sender, in_flight).await.is_none());

        let overview = aggregator.overview();
        assert_eq!(overview.today_calls, 1);
        assert_eq!(overview.today_errors, 1);
        assert_eq!(overview.today_input_tokens, 3);
        assert_eq!(overview.today_output_tokens, 5);
        assert_eq!(overview.today_credits, 0.75);
    }

    #[tokio::test]
    async fn streaming_emitter_reports_background_failure_as_anthropic_error() {
        let (sender, mut receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
        let mut emitter = WebSearchSseEmitter::new(sender);
        emitter
            .fail("upstream_error", "MCP connection failed")
            .await;

        let events = drain_sse(&mut receiver);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "error");
        assert_eq!(events[0].1["error"]["type"], json!("upstream_error"));
        assert_eq!(
            events[0].1["error"]["message"],
            json!("MCP connection failed")
        );
        assert!(emitter.terminal);

        emitter.fail("api_error", "must not duplicate").await;
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn abort_search_closes_the_dangling_block_before_a_terminal_error() {
        // Mirrors execute_web_search's error path: begin_search opens a
        // server_tool_use block, the MCP call fails, abort_search must close
        // it (content_block_stop only, no web_search_tool_result) BEFORE the
        // caller's terminal `error` event — every content_block_start must be
        // paired, exactly like stream.rs::generate_final_events closes open
        // blocks ahead of a mid-stream tool error.
        let (sender, mut receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
        let mut emitter = WebSearchSseEmitter::new(sender);

        let pending = emitter.begin_search("Rust 2026").await;
        let started = drain_sse(&mut receiver);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, "content_block_start");
        let opened_index = started[0].1["index"].clone();

        emitter.abort_search(pending).await;
        emitter
            .fail("upstream_error", "MCP connection failed")
            .await;

        let events = drain_sse(&mut receiver);
        assert_eq!(
            events.len(),
            2,
            "expected a stop for the opened block, then error"
        );
        assert_eq!(events[0].0, "content_block_stop");
        assert_eq!(events[0].1["index"], opened_index);
        assert_eq!(events[1].0, "error");
        assert!(emitter.terminal);
    }

    #[tokio::test]
    async fn background_panic_closes_open_block_before_terminal_error() {
        // Regression for the tokio::spawn body: if the agentic loop panics,
        // unwinding must not drop `emitter` (and its `sender`) with no
        // terminal frame. Exercise the same catch_unwind + fail() shape used
        // in run_web_search_loop's spawned task, with a future that panics
        // standing in for a buggy loop body.
        let (sender, mut receiver) = mpsc::channel(WEB_SEARCH_PROGRESS_CAPACITY);
        tokio::spawn(async move {
            let mut emitter = WebSearchSseEmitter::new(sender);
            let outcome = std::panic::AssertUnwindSafe(async {
                let _pending = emitter.begin_search("Rust 2026").await;
                panic!("boom: simulated agentic loop bug");
            })
            .catch_unwind()
            .await;
            let Err(panic) = outcome;
            if !emitter.terminal {
                emitter.fail("internal_error", &panic_message(&panic)).await;
            }
        })
        .await
        .expect("the spawned task itself must not panic (catch_unwind absorbs it)");

        let events = drain_sse(&mut receiver);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].0, "content_block_start");
        assert_eq!(events[1].0, "content_block_stop");
        assert_eq!(events[0].1["index"], events[1].1["index"]);
        assert_eq!(events[2].0, "error");
        assert_eq!(events[2].1["error"]["type"], json!("internal_error"));
        assert_eq!(
            events[2].1["error"]["message"],
            json!("boom: simulated agentic loop bug")
        );
    }

    /// Build a known-tool-names set for build_flush_content tests.
    fn names(ns: &[&str]) -> std::collections::HashSet<String> {
        ns.iter().map(|s| s.to_string()).collect()
    }

    /// Empty short->original tool name map for build_flush_content tests.
    fn nomap() -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }

    // ---- should_search_round: hit / skip / limit reached ----

    #[test]
    fn round_with_only_web_search_continues() {
        // Hit: this round is all web_search and the limit is not reached -> keep searching
        let tools = vec![tu("web_search"), tu("web_search")];
        assert!(should_search_round(0, &tools));
        assert!(should_search_round(MAX_WEB_SEARCH_ROUNDS - 1, &tools));
    }

    #[test]
    fn round_with_exec_does_not_enter_loop() {
        // Skip: exec mixed in (not web_search) -> terminate, exec returned to the client as-is
        let mixed = vec![tu("web_search"), tu("exec")];
        assert!(!should_search_round(0, &mixed));
        // Same for exec-only
        let exec_only = vec![tu("exec")];
        assert!(!should_search_round(0, &exec_only));
    }

    #[test]
    fn round_with_no_tool_use_does_not_enter_loop() {
        // Skip: no tool_use at all (plain-text answer) -> terminate
        let empty: Vec<CompletedToolUse> = vec![];
        assert!(!should_search_round(0, &empty));
    }

    fn round_outcome(text: &str, tool_uses: Vec<CompletedToolUse>) -> RoundOutcome {
        RoundOutcome {
            text: text.to_string(),
            thinking: String::new(),
            tool_uses,
            context_input_tokens: None,
            provider_token_usage: None,
            credits: 0.0,
            last_metering: None,
            stop_reason_override: None,
            stream_error: None,
            known_tool_names: std::collections::HashSet::new(),
            tool_name_map: std::collections::HashMap::new(),
        }
    }

    fn payload_with_last_block(block: Value) -> MessagesRequest {
        MessagesRequest {
            model: "gpt-5.6-terra".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([block]),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    #[test]
    fn empty_round_after_tool_result_retries_once_then_fails() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![]), 0),
            EmptyToolResultDisposition::Retry
        );
        assert_eq!(
            empty_tool_result_disposition(
                &payload,
                &round_outcome("", vec![]),
                MAX_EMPTY_TOOL_RESULT_RETRIES,
            ),
            EmptyToolResultDisposition::Fail
        );
    }

    #[test]
    fn text_or_tool_call_after_tool_result_is_not_retried() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("finished", vec![]), 0),
            EmptyToolResultDisposition::Accept
        );
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![tu("exec")]), 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn empty_initial_round_is_not_misclassified_as_tool_continuation() {
        let payload = payload_with_last_block(json!({"type": "text", "text": "hello"}));
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![]), 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn whitespace_and_reasoning_only_after_tool_result_is_retried() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        let mut round = round_outcome(" \n\t", vec![]);
        round.thinking = "hidden reasoning without a client-visible continuation".to_string();
        assert_eq!(
            empty_tool_result_disposition(&payload, &round, 0),
            EmptyToolResultDisposition::Retry
        );
    }

    #[test]
    fn terminal_limit_reason_after_tool_result_is_not_retried() {
        let payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        let mut round = round_outcome("", vec![]);
        round.stop_reason_override = Some("max_tokens".to_string());
        assert_eq!(
            empty_tool_result_disposition(&payload, &round, 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn only_the_last_message_determines_tool_continuation() {
        let mut payload = payload_with_last_block(json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done"
        }));
        payload.messages.push(Message {
            role: "user".to_string(),
            content: json!([{"type": "text", "text": "new user turn"}]),
        });
        assert_eq!(
            empty_tool_result_disposition(&payload, &round_outcome("", vec![]), 0),
            EmptyToolResultDisposition::Accept
        );
    }

    #[test]
    fn round_at_limit_stops_even_if_web_search() {
        // Limit reached: even if this round is all web_search, hitting the limit must stop (prevents an infinite loop)
        let tools = vec![tu("web_search")];
        assert!(!should_search_round(MAX_WEB_SEARCH_ROUNDS, &tools));
        assert!(!should_search_round(MAX_WEB_SEARCH_ROUNDS + 1, &tools));
    }

    // ---- build_result_block: search results -> Contract A web_search_result fields ----

    #[test]
    fn result_block_maps_contract_a_fields() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Rust 1.99".to_string(),
                url: "https://example.com/rust".to_string(),
                snippet: Some("Rust 1.99 released".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("rust".to_string()),
            error: None,
        };
        let block = build_result_block(&Some(results));
        assert_eq!(block.len(), 1);
        assert_eq!(block[0]["type"], "web_search_result");
        assert_eq!(block[0]["title"], "Rust 1.99");
        assert_eq!(block[0]["url"], "https://example.com/rust");
        assert_eq!(block[0]["encrypted_content"], "Rust 1.99 released");
    }

    #[test]
    fn result_block_none_is_empty() {
        // No results -> empty block (does not fabricate content)
        assert!(build_result_block(&None).is_empty());
    }

    // ---- search-failure pass-through: an Err from the MCP call must map to an error response, never silently become a 200 "No results found" ----

    #[test]
    fn mcp_failure_maps_to_error_response_not_silent_success() {
        // When the loop gets Err from call_mcp_api it directly `return map_provider_error(e)`,
        // before any generate_search_summary, so a search failure can never turn into a successful summary response.
        // This verifies that map_provider_error returns a non-2xx (BAD_GATEWAY) for a generic MCP error,
        // rather than 200, proving the pass-through path cannot produce a false green.
        let err = anyhow::anyhow!("MCP error: -1 - upstream unavailable");
        let resp = map_provider_error(err);
        assert!(
            !resp.status().is_success(),
            "a failed MCP search must return an error status and must not silently succeed"
        );
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    // ---- build_sse_events: present server_tool_use + result, and the exec tool_use is not swallowed ----

    #[test]
    fn sse_events_render_search_presentation_and_keep_exec() {
        let content = vec![
            json!({"type": "server_tool_use", "id": "srvtoolu_x", "name": "web_search", "input": {"query": "q"}}),
            json!({"type": "web_search_tool_result", "content": []}),
            json!({"type": "text", "text": "done"}),
            json!({"type": "tool_use", "id": "toolu_exec", "name": "exec", "input": {"cmd": "ls"}}),
        ];
        let events = build_sse_events(
            "claude-sonnet-4-8",
            content,
            "tool_use",
            token_usage(10, 5),
            None,
        );

        // Must contain message_start / message_delta(stop_reason) / message_stop
        assert_eq!(events.first().unwrap().event, "message_start");
        assert_eq!(events.last().unwrap().event, "message_stop");
        let delta = events.iter().find(|e| e.event == "message_delta").unwrap();
        assert_eq!(delta.data["delta"]["stop_reason"], "tool_use");

        // the server_tool_use block is placed into content_block_start as-is
        let has_server_tool = events.iter().any(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "server_tool_use"
        });
        assert!(
            has_server_tool,
            "the server_tool_use block should be presented"
        );

        // the web_search_tool_result block is presented
        let has_result = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "web_search_tool_result"
        });
        assert!(
            has_result,
            "the web_search_tool_result block should be presented"
        );

        // exec tool_use is not swallowed: name=exec appears in start
        let has_exec = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "tool_use"
                && e.data["content_block"]["name"] == "exec"
        });
        assert!(
            has_exec,
            "the exec tool_use must be returned to the client as-is and not swallowed"
        );
    }
    // ---- INVARIANT: web_search must NEVER leave kiro-rs as a raw tool_use ----
    // Regression for the "mixed-round leak": when the final round mixes web_search
    // with a client tool (exec/get_time), the flush content must present web_search
    // as server_tool_use + web_search_tool_result (never raw tool_use), while the
    // client tool is returned verbatim. Previously the flush loop emitted
    // {"type":"tool_use","name":"web_search"} which the Codex host rejected with
    // "unsupported call: web_search".

    fn fake_results(q: &str) -> Option<WebSearchResults> {
        Some(WebSearchResults {
            results: vec![WebSearchResult {
                title: "T".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("snip".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some(q.to_string()),
            error: None,
        })
    }

    #[test]
    fn flush_content_mixed_round_never_emits_raw_web_search() {
        let tool_uses = vec![tu("web_search"), tu("exec")];
        let searched = vec![fake_results("rust 2026"), None];
        let content = build_flush_content(
            Vec::new(),
            "answer",
            &tool_uses,
            &searched,
            &names(&["exec"]),
            &nomap(),
        );

        let raw_web_search = content
            .iter()
            .any(|c| c["type"] == "tool_use" && c["name"] == "web_search");
        assert!(
            !raw_web_search,
            "web_search must never be flushed as a raw tool_use (host rejects it). content={:?}",
            content
        );

        assert!(
            content
                .iter()
                .any(|c| c["type"] == "server_tool_use" && c["name"] == "web_search"),
            "web_search must be presented as server_tool_use"
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "web_search_tool_result"),
            "web_search must carry a web_search_tool_result block"
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "exec"),
            "the exec client tool must be returned to the client as-is"
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "text" && c["text"] == "answer"),
            "assistant text must be preserved"
        );
    }

    #[test]
    fn flush_content_client_tools_only_passthrough() {
        let tool_uses = vec![tu("exec")];
        let searched: Vec<Option<WebSearchResults>> = vec![None];
        let content = build_flush_content(
            Vec::new(),
            "",
            &tool_uses,
            &searched,
            &names(&["exec"]),
            &nomap(),
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "exec")
        );
        assert!(!content.iter().any(|c| c["type"] == "server_tool_use"));
    }

    // ---- FIX: web_search loop must run the same <invoke> text-leak fault tolerance ----
    // Root cause: the web_search agentic loop builds its own SSE/content and historically
    // never ran the `<invoke>` fault tolerance that lives in stream.rs. When the upstream
    // model (Kiro Opus, long-context degradation) emits a literal
    // `<invoke name="exec_command">...</invoke>` as assistant TEXT, build_flush_content used
    // to pass it through verbatim as a {"type":"text"} block (the leak). Now it reclaims it.
    fn leaks_literal_invoke(content: &[Value]) -> bool {
        content.iter().any(|c| {
            c["type"] == "text"
                && c["text"]
                    .as_str()
                    .map(|t| t.contains("<invoke name="))
                    .unwrap_or(false)
        })
    }

    #[test]
    fn flush_content_reclaims_leaked_invoke_into_tool_use() {
        // A clean, line-start, closed <invoke> with a known tool name MUST be reclaimed
        // into a structured tool_use and NOT leaked as literal text.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(
            !leaks_literal_invoke(&content),
            "literal <invoke> must not leak as text. content={:?}",
            content
        );
        let reclaimed = content.iter().find(|c| c["type"] == "tool_use");
        assert!(
            reclaimed.is_some(),
            "must reclaim a structured tool_use. content={:?}",
            content
        );
        let tu = reclaimed.unwrap();
        assert_eq!(tu["name"], "exec_command");
        assert_eq!(
            tu["input"]["cmd"], "echo hi",
            "parameter must be parsed into input"
        );
        // the stray `call` line in front of the invoke must be stripped, not leaked
        assert!(
            !content
                .iter()
                .any(|c| c["type"] == "text" && c["text"].as_str() == Some("call\n")),
            "stray token line must be stripped"
        );
    }

    #[test]
    fn flush_content_keeps_real_text_before_leaked_invoke() {
        // Narrative text before the leaked invoke must be preserved as a text block,
        // and the invoke still reclaimed.
        let leaked = "Here is the result.\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(!leaks_literal_invoke(&content));
        assert!(
            content.iter().any(|c| c["type"] == "text"
                && c["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Here is the result.")),
            "narrative text must be preserved. content={:?}",
            content
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "exec_command")
        );
    }

    // ---- SAFETY GATES: must NOT reclaim (would risk executing discussed commands) ----

    #[test]
    fn flush_content_does_not_reclaim_invoke_inside_code_fence() {
        // An <invoke> shown inside a ``` code fence is a DISPLAY/discussion, not a real call.
        // It must stay as text, never become a tool_use.
        let text = "Look at this example:\n```\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf /</parameter>\n</invoke>\n```";
        let content = build_flush_content(
            Vec::new(),
            text,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "fenced <invoke> must NOT be reclaimed (it's a display). content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_does_not_reclaim_invoke_mid_sentence() {
        // <invoke> embedded mid-sentence (not at line start) is discussion text, not a call.
        let text = "the tag <invoke name=\"exec_command\"><parameter name=\"cmd\">x</parameter></invoke> means a call";
        let content = build_flush_content(
            Vec::new(),
            text,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "mid-sentence <invoke> must NOT be reclaimed. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_does_not_reclaim_unknown_tool_name() {
        // Tool-table guard: a clean line-start <invoke> whose name is NOT a declared tool
        // must NOT be reclaimed (never synthesize a call for an unknown tool).
        let leaked = "call\n<invoke name=\"definitely_not_a_tool\">\n<parameter name=\"x\">y</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "unknown tool name must NOT be reclaimed. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_never_reclaims_web_search_as_raw_tool_use() {
        // Reviewer (v2) #3 — the loop's core invariant: a leaked `<invoke name="web_search">`
        // in the assistant TEXT must NEVER be reclaimed into a raw tool_use, even though
        // known_tool_names contains "web_search" (it's always declared on the request that
        // enters this loop). The host has no web_search executor and rejects raw
        // web_search tool_use with "unsupported call: web_search". It must stay as text.
        let leaked = "let me search\n<invoke name=\"web_search\">\n<parameter name=\"query\">latest news</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            // known_tool_names DELIBERATELY contains web_search (mirrors the real request).
            &names(&["web_search", "exec_command"]),
            &nomap(),
        );
        assert!(
            !content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "web_search"),
            "leaked <invoke name=web_search> must NEVER become a raw tool_use. content={:?}",
            content
        );
        // It also must not be mis-presented as a server_tool_use from the text path
        // (only real structured web_search tool_uses become server_tool_use). Staying as
        // text is the protocol-safe outcome here.
        assert!(
            !content.iter().any(|c| c["type"] == "server_tool_use"),
            "text-leaked web_search must not be upgraded to server_tool_use either. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_web_search_guard_does_not_block_other_tools() {
        // Reviewer (v3) #2: stripping web_search from the reclamation table must NOT hurt
        // other tools. A text with BOTH a leaked exec_command and a leaked web_search:
        // exec_command MUST be reclaimed; web_search MUST stay text (never raw tool_use).
        let leaked = "<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>\n<invoke name=\"web_search\">\n<parameter name=\"query\">news</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["web_search", "exec_command"]),
            &nomap(),
        );
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "exec_command"),
            "exec_command must still be reclaimed. content={:?}",
            content
        );
        assert!(
            !content
                .iter()
                .any(|c| c["type"] == "tool_use" && c["name"] == "web_search"),
            "web_search must NOT be reclaimed as raw tool_use. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_clean_text_is_single_text_block() {
        // No <invoke> at all -> behavior identical to before: one text block, unchanged.
        let content = build_flush_content(
            Vec::new(),
            "just a normal answer",
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "just a normal answer");
    }

    #[test]
    fn flush_content_reclaims_two_burst_invokes() {
        // Two consecutive leaked invokes must both be reclaimed and not bleed into each other.
        let leaked = "<invoke name=\"exec_command\">\n<parameter name=\"cmd\">a</parameter>\n</invoke>\n<invoke name=\"get_time\">\n<parameter name=\"tz\">utc</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command", "get_time"]),
            &nomap(),
        );
        assert!(!leaks_literal_invoke(&content));
        let tus: Vec<&Value> = content.iter().filter(|c| c["type"] == "tool_use").collect();
        assert_eq!(
            tus.len(),
            2,
            "both invokes reclaimed. content={:?}",
            content
        );
        assert_eq!(tus[0]["name"], "exec_command");
        assert_eq!(tus[0]["input"]["cmd"], "a");
        assert_eq!(tus[1]["name"], "get_time");
        assert_eq!(tus[1]["input"]["tz"], "utc");
    }

    #[test]
    fn flush_content_unclosed_invoke_stays_text() {
        // An <invoke> with no closing tag in the complete text is not a clean call -> keep as text.
        let text = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi";
        let content = build_flush_content(
            Vec::new(),
            text,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        assert!(
            !content.iter().any(|c| c["type"] == "tool_use"),
            "unclosed <invoke> must NOT be reclaimed. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_restores_shortened_tool_name() {
        // Reviewer #2: long tool names (>63) are shortened before being sent upstream, so the
        // model leaks the SHORT name. known_tool_names contains the short name (so it's reclaimed),
        // but the reclaimed tool_use MUST carry the ORIGINAL name (host matches on original).
        let short = "mcp__codex_apps__x___list_projects_a1b2c3d4";
        let original = "mcp__codex_apps__sites___list_projects_with_a_very_long_suffix";
        let leaked = format!(
            "call\n<invoke name=\"{}\">\n<parameter name=\"q\">x</parameter>\n</invoke>",
            short
        );
        let mut map = std::collections::HashMap::new();
        map.insert(short.to_string(), original.to_string());
        let content = build_flush_content(Vec::new(), &leaked, &[], &[], &names(&[short]), &map);
        let tu = content
            .iter()
            .find(|c| c["type"] == "tool_use")
            .expect("must reclaim a tool_use");
        assert_eq!(
            tu["name"], original,
            "reclaimed tool name must be restored to the original (not the shortened) name"
        );
    }

    #[test]
    fn flush_content_yields_tool_use_so_caller_sets_tool_use_stop_reason() {
        // Reviewer #1: the common leak case is the model emitting the call as TEXT with NO
        // structured tool_use, so round.tool_uses is empty and the caller's pre-flush
        // stop_reason would be "end_turn". The fix relies on build_flush_content surfacing a
        // reclaimed (non-web_search) tool_use block, which the caller then keys off to force
        // stop_reason="tool_use". This test pins that contract: a leaked invoke with an empty
        // tool_uses list still yields a client tool_use block in the content.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi</parameter>\n</invoke>";
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &[],
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        let has_client_tool_use = content
            .iter()
            .any(|c| c["type"] == "tool_use" && c["name"] != "web_search");
        assert!(
            has_client_tool_use,
            "a reclaimed leak must surface a client tool_use so the caller sets stop_reason=tool_use. content={:?}",
            content
        );
    }

    // ---- resolve_flush_stop_reason: the protocol-consistency core of the fix ----

    #[test]
    fn stop_reason_reclaimed_text_invoke_is_tool_use_not_end_turn() {
        // Reviewer #1 main scenario: model degrades, emits the call as TEXT, so the round had
        // NO structured client tool_use (client_uses_empty = true). After the fault tolerance
        // reclaims a tool_use into content, the reason MUST be tool_use (not end_turn).
        let content = vec![json!({"type":"tool_use","id":"t","name":"exec_command","input":{}})];
        assert_eq!(
            resolve_flush_stop_reason(None, true, &content),
            "tool_use",
            "a reclaimed tool_use must flip stop_reason to tool_use"
        );
    }

    #[test]
    fn stop_reason_web_search_only_stays_end_turn() {
        // A web_search-only flush (presented as server_tool_use) has no client tool_use ->
        // must stay end_turn so the host doesn't wait for a client call that never comes.
        let content = vec![
            json!({"type":"text","text":"answer"}),
            json!({"type":"server_tool_use","id":"s","name":"web_search","input":{"query":"q"}}),
            json!({"type":"web_search_tool_result","content":[]}),
        ];
        assert_eq!(resolve_flush_stop_reason(None, true, &content), "end_turn");
    }

    #[test]
    fn stop_reason_structured_client_tool_use_is_tool_use() {
        // Classic structured case: round had a client tool_use -> tool_use.
        let content = vec![json!({"type":"tool_use","id":"t","name":"exec","input":{}})];
        assert_eq!(resolve_flush_stop_reason(None, false, &content), "tool_use");
    }

    #[test]
    fn stop_reason_upstream_override_always_wins() {
        // max_tokens / context_window_exceeded override must win verbatim even if a tool_use
        // was reclaimed.
        let content = vec![json!({"type":"tool_use","id":"t","name":"exec_command","input":{}})];
        assert_eq!(
            resolve_flush_stop_reason(Some("max_tokens"), true, &content),
            "max_tokens"
        );
    }

    #[test]
    fn partition_separates_web_search_from_client_tools() {
        let tool_uses = vec![tu("web_search"), tu("exec"), tu("web_search")];
        let (web, client) = partition_tool_uses(&tool_uses);
        assert_eq!(web.len(), 2, "two web_search calls");
        assert_eq!(client.len(), 1, "one client tool");
        assert_eq!(client[0].name, "exec");
    }

    #[test]
    fn flush_content_only_web_search_has_no_client_tool() {
        // A final round that is only web_search (e.g. round limit hit) must present
        // the search and emit NO raw tool_use at all -> the caller derives end_turn.
        let tool_uses = vec![tu("web_search")];
        let searched = vec![fake_results("q")];
        let content =
            build_flush_content(Vec::new(), "", &tool_uses, &searched, &names(&[]), &nomap());
        assert!(!content.iter().any(|c| c["type"] == "tool_use"));
        assert!(
            content
                .iter()
                .any(|c| c["type"] == "server_tool_use" && c["name"] == "web_search")
        );
        // client-tool partition is empty -> caller will choose end_turn
        let (_web, client) = partition_tool_uses(&tool_uses);
        assert!(client.is_empty());
    }

    #[test]
    fn flush_content_dedups_reclaimed_against_structured_tool_use() {
        // Degraded models can emit BOTH a leaked literal `<invoke>` in the assistant
        // text AND a structured tool_use for the SAME action. Without dedup the host
        // would receive two identical tool_use blocks and execute the command twice.
        // The reclaimed-from-text tool_use must be suppressed when an identical
        // (name + canonical input) structured tool_use already exists in this round.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf build</parameter>\n</invoke>";
        let structured = vec![CompletedToolUse {
            id: "toolu_dup".to_string(),
            name: "exec_command".to_string(),
            input: json!({"cmd": "rm -rf build"}),
        }];
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &structured,
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        let exec_calls = content
            .iter()
            .filter(|c| c["type"] == "tool_use" && c["name"] == "exec_command")
            .count();
        assert_eq!(
            exec_calls, 1,
            "duplicate tool_use (reclaimed + structured) must be de-duped to one. content={:?}",
            content
        );
    }

    #[test]
    fn flush_content_keeps_distinct_reclaimed_and_structured() {
        // Dedup must only collapse TRUE duplicates: a reclaimed tool_use with a
        // different input than the structured one is a distinct action and must be kept.
        let leaked = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>";
        let structured = vec![CompletedToolUse {
            id: "toolu_other".to_string(),
            name: "exec_command".to_string(),
            input: json!({"cmd": "pwd"}),
        }];
        let content = build_flush_content(
            Vec::new(),
            leaked,
            &structured,
            &[],
            &names(&["exec_command"]),
            &nomap(),
        );
        let exec_calls = content
            .iter()
            .filter(|c| c["type"] == "tool_use" && c["name"] == "exec_command")
            .count();
        assert_eq!(
            exec_calls, 2,
            "distinct inputs must both be kept. content={:?}",
            content
        );
    }

    #[test]
    fn round_usage_prefers_provider_and_mixed_rounds_accumulate_each_category() {
        let mut provider_round = round_outcome("ignored fallback output", vec![]);
        provider_round.context_input_tokens = Some(999);
        provider_round.provider_token_usage = Some(TokenUsage {
            uncached_input_tokens: 3,
            output_tokens: 5,
            cache_read_input_tokens: 7,
            cache_write_input_tokens: 4,
        });
        let provider_usage = provider_round.resolved_token_usage(888);
        assert_eq!(
            provider_usage,
            TokenUsage {
                uncached_input_tokens: 3,
                output_tokens: 5,
                cache_read_input_tokens: 7,
                cache_write_input_tokens: 4,
            }
        );

        let mut fallback_round = round_outcome("fallback output", vec![]);
        fallback_round.context_input_tokens = Some(20);
        let fallback_usage = fallback_round.resolved_token_usage(500);
        assert_eq!(fallback_usage.uncached_input_tokens, 20);
        assert_eq!(fallback_usage.cache_write_input_tokens, 0);
        assert_eq!(fallback_usage.cache_read_input_tokens, 0);
        assert!(fallback_usage.output_tokens > 0);

        let total = provider_usage.saturating_add(fallback_usage);
        assert_eq!(total.uncached_input_tokens, 23);
        assert_eq!(total.output_tokens, 5 + fallback_usage.output_tokens);
        assert_eq!(total.cache_write_input_tokens, 4);
        assert_eq!(total.cache_read_input_tokens, 7);
    }

    #[test]
    fn aggregated_trace_usage_matches_websearch_usage_and_sanitizes_values() {
        let trace = aggregated_trace_usage(
            TokenUsage {
                uncached_input_tokens: 3,
                output_tokens: 5,
                cache_read_input_tokens: 7,
                cache_write_input_tokens: 4,
            },
            0.125,
        );
        assert_eq!(trace.input_tokens, 3);
        assert_eq!(trace.output_tokens, 5);
        assert_eq!(trace.cache_creation_tokens, 4);
        assert_eq!(trace.cache_read_tokens, 7);
        assert_eq!(trace.credits, 0.125);

        let sanitized = aggregated_trace_usage(
            TokenUsage {
                uncached_input_tokens: -1,
                output_tokens: -2,
                cache_read_input_tokens: -3,
                cache_write_input_tokens: -4,
            },
            f64::NAN,
        );
        assert_eq!(sanitized.input_tokens, 0);
        assert_eq!(sanitized.output_tokens, 0);
        assert_eq!(sanitized.cache_creation_tokens, 0);
        assert_eq!(sanitized.cache_read_tokens, 0);
        assert_eq!(sanitized.credits, 0.0);
    }

    #[test]
    fn json_and_sse_render_the_same_four_part_usage() {
        let expected = TokenUsage {
            uncached_input_tokens: 3,
            output_tokens: 5,
            cache_read_input_tokens: 7,
            cache_write_input_tokens: 4,
        };
        let content = vec![json!({"type": "text", "text": "ok"})];
        let response = render_json(
            "claude-opus-4-7",
            content.clone(),
            "end_turn",
            expected,
            "",
            None,
        );
        let bytes = futures::executor::block_on(async {
            axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap()
        });
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["usage"]["input_tokens"], json!(3));
        assert_eq!(body["usage"]["output_tokens"], json!(5));
        assert_eq!(body["usage"]["cache_creation_input_tokens"], json!(4));
        assert_eq!(body["usage"]["cache_read_input_tokens"], json!(7));

        let events = build_sse_events("claude-opus-4-7", content, "end_turn", expected, None);
        let start_usage = &events
            .iter()
            .find(|event| event.event == "message_start")
            .unwrap()
            .data["message"]["usage"];
        assert_eq!(start_usage["input_tokens"], json!(3));
        assert_eq!(start_usage["cache_creation_input_tokens"], json!(4));
        assert_eq!(start_usage["cache_read_input_tokens"], json!(7));
        let delta_usage = &events
            .iter()
            .find(|event| event.event == "message_delta")
            .unwrap()
            .data["usage"];
        assert_eq!(delta_usage["output_tokens"], json!(5));
    }

    // ---- credit_usage 透传：run_web_search_loop 路径 ----

    fn metering_event(usage: f64) -> MeteringEvent {
        MeteringEvent {
            unit: "credit".to_string(),
            unit_plural: "credits".to_string(),
            usage,
        }
    }

    fn token_usage(input_tokens: i32, output_tokens: i32) -> TokenUsage {
        TokenUsage {
            uncached_input_tokens: input_tokens,
            output_tokens,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
        }
    }

    #[test]
    fn render_json_carries_credit_fields_when_metering_present() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let metering = metering_event(0.42);
        let resp = render_json(
            "claude-opus-4-7",
            content,
            "end_turn",
            token_usage(10, 5),
            "",
            Some(&metering),
        );
        // 把 Response 的 body 序列化为 JSON 再断言。
        let body = resp.into_body();
        let bytes = futures::executor::block_on(async {
            axum::body::to_bytes(body, 64 * 1024).await.unwrap()
        });
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let usage = &v["usage"];
        assert_eq!(usage["credit_usage"], json!(0.42));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
        // 原有字段保持原样
        assert_eq!(usage["input_tokens"], json!(10));
        assert_eq!(usage["output_tokens"], json!(5));
    }

    #[test]
    fn render_json_omits_credit_fields_without_metering() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let resp = render_json(
            "claude-opus-4-7",
            content,
            "end_turn",
            token_usage(10, 5),
            "",
            None,
        );
        let body = resp.into_body();
        let bytes = futures::executor::block_on(async {
            axum::body::to_bytes(body, 64 * 1024).await.unwrap()
        });
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let usage = &v["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
    }

    #[test]
    fn build_sse_events_carries_credit_fields_in_message_delta() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let metering = metering_event(0.99);
        let events = build_sse_events(
            "claude-opus-4-7",
            content,
            "end_turn",
            token_usage(10, 5),
            Some(&metering),
        );
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert_eq!(usage["credit_usage"], json!(0.99));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
        // 原有字段保持原样
        assert_eq!(usage["output_tokens"], json!(5));
    }

    #[test]
    fn build_sse_events_omits_credit_fields_without_metering() {
        let content = vec![json!({"type": "text", "text": "ok"})];
        let events = build_sse_events(
            "claude-opus-4-7",
            content,
            "end_turn",
            token_usage(10, 5),
            None,
        );
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
    }
}
