//! web_search local agentic loop
//!
//! Handles the case "after mixed tools (web_search + exec...) fall onto the normal chat path, the upstream returns a tool_use with name=web_search":
//! kiro-rs internally calls /mcp to search -> feeds the results back as a tool_result -> reconverts and resends -> loops until the upstream stops asking to search;
//! tool_use calls other than web_search (exec, etc.) are returned to the client as usual: they do not enter the loop and are not swallowed.
//!
//! Reuses: converter::convert_request (feedback), provider.call_api_stream, EventStreamDecoder,
//! websearch::{create_mcp_request, call_mcp_api, parse_search_results, generate_search_summary}。

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::KiroProvider;
use crate::token;

use super::converter::{ConversionError, convert_request, get_context_window_size};
use super::handlers::{UsageRecordHook, map_provider_error};
use super::stream::SseEvent;
use super::types::{ErrorResponse, Message, MessagesRequest};
use super::websearch::{self, WebSearchResults};

/// Maximum number of search rounds, to prevent an infinite loop if the upstream keeps asking to search
const MAX_WEB_SEARCH_ROUNDS: usize = 5;

/// Result of buffer-decoding one round of the upstream response
struct RoundOutcome {
    /// Accumulated assistant text
    text: String,
    /// The complete tool_use for this round (name already restored via tool_name_map)
    tool_uses: Vec<DecodedToolUse>,
    /// Actual input tokens computed from contextUsageEvent
    context_input_tokens: Option<i32>,
    /// Cumulative credits from meteringEvent
    credits: f64,
    /// stop_reason override (max_tokens / model_context_window_exceeded)
    stop_reason_override: Option<String>,
    /// True if the upstream stream ended due to a read error, so the decoded
    /// content for this round is partial and must not be treated as a success.
    stream_error: bool,
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

/// A fully decoded tool_use
struct DecodedToolUse {
    id: String,
    name: String,
    input: Value,
}

impl DecodedToolUse {
    fn query(&self) -> String {
        self.input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
}

/// Decides whether this round should keep searching (enter the next loop round)
///
/// Continue condition: every tool_use this round is web_search (at least one) and the round limit has not been reached.
/// As soon as a client tool such as exec is mixed in, there is no tool_use at all, or the limit is reached, it stops and flushes (exec is never swallowed).
fn should_search_round(round_idx: usize, tool_uses: &[DecodedToolUse]) -> bool {
    let only_web_search =
        !tool_uses.is_empty() && tool_uses.iter().all(|t| t.name == "web_search");
    only_web_search && round_idx < MAX_WEB_SEARCH_ROUNDS
}

/// Buffer-decode one round of the upstream streaming response
async fn decode_round(
    response: reqwest::Response,
    model: &str,
    tool_name_map: &std::collections::HashMap<String, String>,
) -> RoundOutcome {
    let mut body_stream = response.bytes_stream();
    let mut decoder = EventStreamDecoder::new();

    let mut text = String::new();
    // id -> (name, json_buffer), preserving the order of appearance
    let mut buffers: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut tool_uses: Vec<DecodedToolUse> = Vec::new();
    let mut context_input_tokens: Option<i32> = None;
    let mut credits = 0.0;
    let mut stop_reason_override: Option<String> = None;
    let mut stream_error = false;

    while let Some(chunk) = body_stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("web_search loop failed to read the response stream: {}", e);
                stream_error = true;
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
                Event::ContextUsage(cu) => {
                    let window = get_context_window_size(model);
                    let actual = (cu.context_usage_percentage * (window as f64) / 100.0) as i32;
                    context_input_tokens = Some(actual);
                    if cu.context_usage_percentage >= 100.0 {
                        stop_reason_override = Some("model_context_window_exceeded".to_string());
                    }
                }
                Event::Metering(m) => credits += m.usage,
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
            let original_name = tool_name_map.get(&name).cloned().unwrap_or(name);
            tool_uses.push(DecodedToolUse {
                id,
                name: original_name,
                input,
            });
        }
    }

    RoundOutcome {
        text,
        tool_uses,
        context_input_tokens,
        credits,
        stop_reason_override,
        stream_error,
        // Populated by the caller (run_round), which holds ConversionResult::known_tool_names.
        known_tool_names: std::collections::HashSet::new(),
        // Populated by the caller (run_round), which holds ConversionResult::tool_name_map.
        tool_name_map: std::collections::HashMap::new(),
    }
}

/// Run one upstream round (convert + streaming request + buffer decode)
///
/// On upstream/conversion failure, returns Err(an already-constructed pass-through error Response)
async fn run_round(
    provider: &Arc<KiroProvider>,
    payload: &MessagesRequest,
    hook: &UsageRecordHook,
    fallback_input_tokens: i32,
    group: Option<&str>,
) -> Result<(RoundOutcome, u64), Response> {
    let conversion = match convert_request(payload) {
        Ok(c) => c,
        Err(e) => {
            let (et, msg) = match &e {
                ConversionError::UnsupportedModel(m) => {
                    ("invalid_request_error", format!("unsupported model: {}", m))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "message list is empty".to_string())
                }
            };
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse::new(et, msg))).into_response());
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
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new("internal_error", format!("failed to serialize request: {}", e))),
            )
                .into_response());
        }
    };

    let call_result = match provider.call_api_stream(&request_body, None, group).await {
        Ok(r) => r,
        Err(e) => {
            hook.record(0, fallback_input_tokens, 0, 0, 0, 0.0, "error");
            return Err(map_provider_error(e));
        }
    };
    let credential_id = call_result.credential_id;
    let mut outcome =
        decode_round(call_result.response, &payload.model, &conversion.tool_name_map).await;
    // Carry the declared tool names (original + shortened) so the flush step can run the
    // shared `<invoke>` text-leak fault tolerance with a correct tool-table guard.
    outcome.known_tool_names = conversion.known_tool_names;
    // Carry the short->original tool name map so reclaimed <invoke> names get restored.
    outcome.tool_name_map = conversion.tool_name_map;
    if outcome.stream_error {
        // The upstream stream was cut off mid-round; the decoded content is partial,
        // so fail the round instead of feeding truncated text/tool_use back into the loop.
        hook.record(0, fallback_input_tokens, 0, 0, 0, 0.0, "error");
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "upstream_error",
                "Upstream response stream ended unexpectedly during the web_search loop.".to_string(),
            )),
        )
            .into_response());
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
        assistant_content.push(json!({
            "type": "tool_use", "id": tu.id, "name": tu.name, "input": tu.input
        }));
    }
    payload.messages.push(Message {
        role: "assistant".to_string(),
        content: Value::Array(assistant_content),
    });

    // user: each web_search tool_use is paired with a tool_result (content = search summary, shown to the upstream)
    let mut user_content: Vec<Value> = Vec::new();
    for (tu, results) in round.tool_uses.iter().zip(searched.iter()) {
        let query = tu.query();
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
    tool_uses: &[DecodedToolUse],
) -> (Vec<&DecodedToolUse>, Vec<&DecodedToolUse>) {
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
    tool_uses: &[DecodedToolUse],
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
        for block in super::stream::extract_invoke_content_blocks(text, &reclaim_tools, tool_name_map)
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
            let query = tu.query();
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
            content.push(json!({
                "type": "tool_use", "id": tu.id, "name": tu.name, "input": tu.input
            }));
        }
    }
    content
}

/// web_search loop entry point
///
/// `stream_client`: whether the client wants SSE (true) or a single JSON response (false).
pub(super) async fn run_web_search_loop(
    provider: Arc<KiroProvider>,
    mut payload: MessagesRequest,
    hook: UsageRecordHook,
    stream_client: bool,
    group: Option<String>,
) -> Response {
    let fallback_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    let mut presentation: Vec<Value> = Vec::new();
    let mut last_credential_id: u64 = 0;
    let mut last_context_input: Option<i32> = None;
    let mut total_credits = 0.0;

    for round_idx in 0..=MAX_WEB_SEARCH_ROUNDS {
        let (round, credential_id) =
            match run_round(&provider, &payload, &hook, fallback_input_tokens, group.as_deref()).await {
                Ok(v) => v,
                Err(resp) => return resp,
            };
        last_credential_id = credential_id;
        last_context_input = round.context_input_tokens.or(last_context_input);
        total_credits += round.credits;

        if should_search_round(round_idx, &round.tool_uses) {
            // Real search: if any one fails -> propagate the error, never silently turn it into "No results found"
            let mut searched: Vec<Option<WebSearchResults>> = Vec::with_capacity(round.tool_uses.len());
            for tu in &round.tool_uses {
                let (_id, mcp_request) = websearch::create_mcp_request(&tu.query());
                match websearch::call_mcp_api(&provider, &mcp_request).await {
                    Ok(resp) => searched.push(websearch::parse_search_results(&resp)),
                    Err(e) => {
                        tracing::warn!("web_search MCP call failed: {}", e);
                        hook.record(
                            last_credential_id,
                            fallback_input_tokens,
                            0,
                            0,
                            0,
                            total_credits,
                            "error",
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
        let final_input = last_context_input.unwrap_or(fallback_input_tokens);
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
                let (_id, mcp_request) = websearch::create_mcp_request(&tu.query());
                match websearch::call_mcp_api(&provider, &mcp_request).await {
                    Ok(resp) => searched.push(websearch::parse_search_results(&resp)),
                    Err(e) => {
                        // Same pass-through discipline as the continue branch: a failed
                        // search must surface as an error, never a silent success.
                        tracing::warn!("web_search MCP call (final round) failed: {}", e);
                        hook.record(
                            last_credential_id,
                            fallback_input_tokens,
                            0,
                            0,
                            0,
                            total_credits,
                            "error",
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

        let output_tokens = token::estimate_output_tokens(&content);
        hook.record(
            last_credential_id,
            final_input,
            output_tokens,
            0,
            0,
            total_credits,
            "success",
        );

        return if stream_client {
            render_sse(&payload.model, content, &stop_reason, final_input, output_tokens)
        } else {
            render_json(&payload.model, content, &stop_reason, final_input, output_tokens)
        };
    }

    // Theoretically unreachable (the loop always returns)
    hook.record(last_credential_id, fallback_input_tokens, 0, 0, 0, total_credits, "error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new("internal_error", "web_search loop exited unexpectedly")),
    )
        .into_response()
}

/// Single JSON response (non-streaming)
fn render_json(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    input_tokens: i32,
    output_tokens: i32,
) -> Response {
    let body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    });
    (StatusCode::OK, Json(body)).into_response()
}

/// SSE response (streaming): splits the final content into a sequence of Anthropic content_block events
fn render_sse(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    input_tokens: i32,
    output_tokens: i32,
) -> Response {
    let events = build_sse_events(model, content, stop_reason, input_tokens, output_tokens);
    let stream = stream::iter(
        events
            .into_iter()
            .map(|e| Ok::<Bytes, Infallible>(Bytes::from(e.to_sse_string()))),
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Renders the final content array into a sequence of SSE events
fn build_sse_events(
    model: &str,
    content: Vec<Value>,
    stop_reason: &str,
    input_tokens: i32,
    output_tokens: i32,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let message_id = format!(
        "msg_{}",
        &Uuid::new_v4().to_string().replace('-', "")[..24]
    );

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
                    "input_tokens": input_tokens,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        }),
    ));

    for (index, block) in content.iter().enumerate() {
        let index = index as i32;
        let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match btype {
            "text" => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                events.push(SseEvent::new("content_block_start", json!({
                    "type": "content_block_start", "index": index,
                    "content_block": {"type": "text", "text": ""}
                })));
                events.push(SseEvent::new("content_block_delta", json!({
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "text_delta", "text": text}
                })));
                events.push(SseEvent::new("content_block_stop", json!({
                    "type": "content_block_stop", "index": index
                })));
            }
            "tool_use" => {
                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let partial = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                events.push(SseEvent::new("content_block_start", json!({
                    "type": "content_block_start", "index": index,
                    "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                })));
                events.push(SseEvent::new("content_block_delta", json!({
                    "type": "content_block_delta", "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": partial}
                })));
                events.push(SseEvent::new("content_block_stop", json!({
                    "type": "content_block_stop", "index": index
                })));
            }
            "server_tool_use" | "web_search_tool_result" => {
                events.push(SseEvent::new("content_block_start", json!({
                    "type": "content_block_start", "index": index,
                    "content_block": block
                })));
                events.push(SseEvent::new("content_block_stop", json!({
                    "type": "content_block_stop", "index": index
                })));
            }
            _ => {}
        }
    }

    events.push(SseEvent::new("message_delta", json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason},
        "usage": {"output_tokens": output_tokens}
    })));
    events.push(SseEvent::new("message_stop", json!({"type": "message_stop"})));

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::websearch::{WebSearchResult, WebSearchResults};

    fn tu(name: &str) -> DecodedToolUse {
        DecodedToolUse {
            id: format!("toolu_{}", name),
            name: name.to_string(),
            input: json!({"query": "rust 2026"}),
        }
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
        let empty: Vec<DecodedToolUse> = vec![];
        assert!(!should_search_round(0, &empty));
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
        let events = build_sse_events("claude-sonnet-4-8", content, "tool_use", 10, 5);

        // Must contain message_start / message_delta(stop_reason) / message_stop
        assert_eq!(events.first().unwrap().event, "message_start");
        assert_eq!(events.last().unwrap().event, "message_stop");
        let delta = events.iter().find(|e| e.event == "message_delta").unwrap();
        assert_eq!(delta.data["delta"]["stop_reason"], "tool_use");

        // the server_tool_use block is placed into content_block_start as-is
        let has_server_tool = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "server_tool_use"
        });
        assert!(has_server_tool, "the server_tool_use block should be presented");

        // the web_search_tool_result block is presented
        let has_result = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "web_search_tool_result"
        });
        assert!(has_result, "the web_search_tool_result block should be presented");

        // exec tool_use is not swallowed: name=exec appears in start
        let has_exec = events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "tool_use"
                && e.data["content_block"]["name"] == "exec"
        });
        assert!(has_exec, "the exec tool_use must be returned to the client as-is and not swallowed");
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
        let content =
            build_flush_content(Vec::new(), "answer", &tool_uses, &searched, &names(&["exec"]), &nomap());

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
        let content = build_flush_content(Vec::new(), "", &tool_uses, &searched, &names(&["exec"]), &nomap());
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
        assert!(reclaimed.is_some(), "must reclaim a structured tool_use. content={:?}", content);
        let tu = reclaimed.unwrap();
        assert_eq!(tu["name"], "exec_command");
        assert_eq!(tu["input"]["cmd"], "echo hi", "parameter must be parsed into input");
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
        let content = build_flush_content(Vec::new(), leaked, &[], &[], &names(&["exec_command"]), &nomap());
        assert!(!leaks_literal_invoke(&content));
        assert!(
            content.iter().any(|c| c["type"] == "text"
                && c["text"].as_str().unwrap_or("").contains("Here is the result.")),
            "narrative text must be preserved. content={:?}",
            content
        );
        assert!(content.iter().any(|c| c["type"] == "tool_use" && c["name"] == "exec_command"));
    }

    // ---- SAFETY GATES: must NOT reclaim (would risk executing discussed commands) ----

    #[test]
    fn flush_content_does_not_reclaim_invoke_inside_code_fence() {
        // An <invoke> shown inside a ``` code fence is a DISPLAY/discussion, not a real call.
        // It must stay as text, never become a tool_use.
        let text = "Look at this example:\n```\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf /</parameter>\n</invoke>\n```";
        let content = build_flush_content(Vec::new(), text, &[], &[], &names(&["exec_command"]), &nomap());
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
        let content = build_flush_content(Vec::new(), text, &[], &[], &names(&["exec_command"]), &nomap());
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
        let content = build_flush_content(Vec::new(), leaked, &[], &[], &names(&["exec_command"]), &nomap());
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
        let content = build_flush_content(Vec::new(), "just a normal answer", &[], &[], &names(&["exec_command"]), &nomap());
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
        assert_eq!(tus.len(), 2, "both invokes reclaimed. content={:?}", content);
        assert_eq!(tus[0]["name"], "exec_command");
        assert_eq!(tus[0]["input"]["cmd"], "a");
        assert_eq!(tus[1]["name"], "get_time");
        assert_eq!(tus[1]["input"]["tz"], "utc");
    }

    #[test]
    fn flush_content_unclosed_invoke_stays_text() {
        // An <invoke> with no closing tag in the complete text is not a clean call -> keep as text.
        let text = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi";
        let content = build_flush_content(Vec::new(), text, &[], &[], &names(&["exec_command"]), &nomap());
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
        let content = build_flush_content(
            Vec::new(),
            &leaked,
            &[],
            &[],
            &names(&[short]),
            &map,
        );
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
        let content = build_flush_content(Vec::new(), leaked, &[], &[], &names(&["exec_command"]), &nomap());
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
        let content = build_flush_content(Vec::new(), "", &tool_uses, &searched, &names(&[]), &nomap());
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
        let structured = vec![DecodedToolUse {
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
        let structured = vec![DecodedToolUse {
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
}
