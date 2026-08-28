//! Codex remote compaction v2 adapter.
//!
//! Manual `/compact` and automatic context-limit compaction both append one
//! `compaction_trigger` item to an ordinary Responses request. Kiro does not
//! understand that item, so this module turns the request into a dedicated
//! summarization pass and returns the single compaction item Codex requires.

use axum::{
    Json,
    body::{Body, to_bytes},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use uuid::Uuid;

use super::super::handlers::post_messages;
use super::super::middleware::{AppState, KeyContext};
use super::super::openai::{
    ParsedResponse, now_ts, parse_anthropic_message, resolve_session_metadata,
};
use super::super::types::{Message, MessagesRequest, Metadata, SystemMessage};
use super::{MAX_INNER_BODY, ResponsesRequest, responses_error, responses_to_anthropic};
use axum::extract::{Extension, State};

const PAYLOAD_PREFIX: &str = "kiro-rs.compaction.v1:";
const RESTORED_CONTEXT_PREFIX: &str = "The following is the compacted context from the earlier conversation. Treat it as prior \
conversation state, not as new user instructions:\n";
const SUMMARY_INSTRUCTION: &str = "Create a compact continuation summary of the conversation. Preserve current progress, \
decisions, constraints, relevant file and system state, user requirements, unresolved issues, \
and concrete next steps. Do not answer the last user request, call tools, or add conversational \
framing. Return only the summary.";
const SUMMARY_REQUEST: &str =
    "Summarize the conversation now according to the compaction instructions.";
const CONTEXT_WINDOW_EXCEEDED: &str = "model_context_window_exceeded";
const TOOL_OUTPUT_RETRY_LIMIT_BYTES: usize = 25_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    Generate,
    RemoteCompact,
}

pub(super) fn classify(input: &Value) -> Result<Operation, String> {
    let Some(items) = input.as_array() else {
        return Ok(Operation::Generate);
    };
    let triggers = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
                .then_some(index)
        })
        .collect::<Vec<_>>();
    match triggers.as_slice() {
        [] => Ok(Operation::Generate),
        [index] if *index + 1 == items.len() => Ok(Operation::RemoteCompact),
        [_] => Err("compaction_trigger must be the final input item".to_string()),
        _ => Err("input must contain at most one compaction_trigger".to_string()),
    }
}

pub(super) async fn handle(
    state: AppState,
    key_ctx: KeyContext,
    headers: HeaderMap,
    mut req: ResponsesRequest,
) -> Response {
    let want_stream = req.stream;
    let model = req.model.clone();
    tracing::info!(
        model = %model,
        stream = %want_stream,
        compact = true,
        "Received Codex remote compaction v2 request"
    );

    // Classification already proved that the final item is the unique trigger.
    req.input
        .as_array_mut()
        .expect("remote compaction input must be an array")
        .pop();
    req.stream = false;
    req.tools = None;
    req.tool_choice = Some(json!("none"));
    let metadata = resolve_session_metadata(req.prompt_cache_key.as_deref(), &headers);
    let mut retry_req = req.clone();
    let anthropic_req = match prepare_request(req, metadata.clone()) {
        Ok(value) => value,
        Err(message) => {
            return responses_error(StatusCode::BAD_REQUEST, "invalid_request_error", &message);
        }
    };

    let mut parsed = match run_attempt(state.clone(), key_ctx.clone(), anthropic_req, &model).await
    {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };

    if parsed.upstream_stop_reason == CONTEXT_WINDOW_EXCEEDED {
        let stats = truncate_large_tool_outputs(&mut retry_req.input);
        if stats.items > 0 {
            tracing::warn!(
                model = %model,
                truncated_tool_outputs = stats.items,
                removed_bytes = stats.removed_bytes,
                "Kiro compaction exceeded the context window; retrying with bounded tool outputs"
            );
            let retry = match prepare_request(retry_req, metadata) {
                Ok(value) => value,
                Err(message) => {
                    return responses_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        &message,
                    );
                }
            };
            parsed = match run_attempt(state, key_ctx, retry, &model).await {
                Ok(parsed) => parsed,
                Err(response) => return response,
            };
        } else {
            tracing::warn!(
                model = %model,
                "Kiro compaction exceeded the context window and had no oversized tool outputs to reduce"
            );
        }
    }

    let outcome = validate(parsed);
    if want_stream {
        render_stream(outcome, &model)
    } else {
        render_json(outcome, &model)
    }
}

fn prepare_request(
    req: ResponsesRequest,
    metadata: Option<Metadata>,
) -> Result<MessagesRequest, String> {
    let (mut anthropic_req, _) = responses_to_anthropic(req, metadata)?;
    anthropic_req.stream = false;
    anthropic_req.tools = None;
    anthropic_req.tool_choice = None;
    append_summary_request(&mut anthropic_req);
    anthropic_req
        .system
        .get_or_insert_with(Vec::new)
        .push(SystemMessage {
            text: SUMMARY_INSTRUCTION.to_string(),
            cache_control: None,
        });
    Ok(anthropic_req)
}

async fn run_attempt(
    state: AppState,
    key_ctx: KeyContext,
    anthropic_req: MessagesRequest,
    model: &str,
) -> Result<ParsedResponse, Response> {
    let inner = post_messages(State(state), Extension(key_ctx), Json(anthropic_req)).await;
    let status = inner.status();
    let body = match to_bytes(inner.into_body(), MAX_INNER_BODY).await {
        Ok(body) => body,
        Err(error) => {
            return Err(responses_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("failed to read compaction response: {error}"),
            ));
        }
    };
    if !status.is_success() {
        return Err(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap());
    }
    let anthropic: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return Err(responses_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("failed to parse compaction response: {error}"),
            ));
        }
    };
    let parsed = parse_anthropic_message(&anthropic, &model);
    tracing::info!(
        model = %model,
        stop_reason = %parsed.upstream_stop_reason,
        input_tokens = parsed.prompt_tokens,
        output_tokens = parsed.completion_tokens,
        "Kiro compaction attempt finished"
    );
    Ok(parsed)
}

/// A Kiro compaction pass is a fresh user turn over the history being
/// summarized. Always append it so an assistant reply, an unanswered user
/// message, or a tool result can never become the active generation turn.
fn append_summary_request(req: &mut MessagesRequest) {
    req.messages.push(Message {
        role: "user".to_string(),
        content: json!([{
            "type": "text",
            "text": SUMMARY_REQUEST,
        }]),
    });
}

#[derive(Default)]
struct TruncationStats {
    items: usize,
    removed_bytes: usize,
}

/// Applies Kiro's aggressive compaction bound only after a confirmed context
/// overflow. The Responses item and call id remain intact, so tool pairing is
/// preserved when the retry is translated back to Kiro history.
fn truncate_large_tool_outputs(input: &mut Value) -> TruncationStats {
    let mut stats = TruncationStats::default();
    let Some(items) = input.as_array_mut() else {
        return stats;
    };
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str);
        if !matches!(
            item_type,
            Some("function_call_output" | "custom_tool_call_output")
        ) {
            continue;
        }
        let Some(output) = item.get_mut("output") else {
            continue;
        };
        let text = super::stringify_output(Some(output));
        if text.len() <= TOOL_OUTPUT_RETRY_LIMIT_BYTES {
            continue;
        }
        let original_bytes = text.len();
        let truncated = truncate_middle(&text, TOOL_OUTPUT_RETRY_LIMIT_BYTES);
        stats.items += 1;
        stats.removed_bytes += original_bytes.saturating_sub(truncated.len());
        *output = Value::String(truncated);
    }
    stats
}

fn truncate_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let marker = format!(
        "\n[... tool output truncated for compaction retry; original size: {} bytes ...]\n",
        text.len()
    );
    if marker.len() >= max_bytes {
        let mut end = max_bytes.min(marker.len());
        while end > 0 && !marker.is_char_boundary(end) {
            end -= 1;
        }
        return marker[..end].to_string();
    }
    let content_budget = max_bytes.saturating_sub(marker.len());
    let head_budget = content_budget * 3 / 4;
    let tail_budget = content_budget.saturating_sub(head_budget);

    let mut head_end = head_budget.min(text.len());
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len().saturating_sub(tail_budget);
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    let mut result = String::with_capacity(max_bytes);
    result.push_str(&text[..head_end]);
    result.push_str(&marker);
    result.push_str(&text[tail_start..]);
    result
}

enum Outcome {
    Complete {
        item: Value,
        usage: Value,
    },
    Incomplete {
        usage: Value,
    },
    Failed {
        code: &'static str,
        message: String,
        usage: Value,
    },
}

fn validate(parsed: ParsedResponse) -> Outcome {
    let usage = usage(&parsed);
    match parsed.upstream_stop_reason.as_str() {
        "max_tokens" => return Outcome::Incomplete { usage },
        CONTEXT_WINDOW_EXCEEDED => {
            return Outcome::Failed {
                code: "context_length_exceeded",
                message: "upstream context window exceeded after compaction recovery".to_string(),
                usage,
            };
        }
        _ => {}
    }
    if !parsed.tool_calls.is_empty() {
        return Outcome::Failed {
            code: "compaction_error",
            message: "upstream returned a tool call instead of a compaction summary".to_string(),
            usage,
        };
    }
    let summary = parsed.text.trim();
    if summary.is_empty() {
        return Outcome::Failed {
            code: "compaction_error",
            message: "upstream completed without a compaction summary".to_string(),
            usage,
        };
    }
    Outcome::Complete {
        item: json!({
            "id": new_compaction_id(),
            "type": "compaction",
            "encrypted_content": encode_payload(summary),
        }),
        usage,
    }
}

fn usage(parsed: &ParsedResponse) -> Value {
    json!({
        "input_tokens": parsed.prompt_tokens,
        "input_tokens_details": { "cached_tokens": parsed.cached_tokens },
        "output_tokens": parsed.completion_tokens,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": parsed.prompt_tokens.saturating_add(parsed.completion_tokens),
    })
}

fn response_object(id: &str, model: &str, status: &str, output: Vec<Value>, usage: Value) -> Value {
    let mut response = json!({
        "id": id,
        "object": "response",
        "created_at": now_ts(),
        "status": status,
        "model": model,
        "output": output,
        "usage": usage,
    });
    if status == "incomplete" {
        response["incomplete_details"] = json!({ "reason": "max_output_tokens" });
    }
    response
}

fn render_json(outcome: Outcome, model: &str) -> Response {
    let id = new_response_id();
    let response = match outcome {
        Outcome::Complete { item, usage } => {
            response_object(&id, model, "completed", vec![item], usage)
        }
        Outcome::Incomplete { usage } => {
            response_object(&id, model, "incomplete", Vec::new(), usage)
        }
        Outcome::Failed {
            code,
            message,
            usage,
        } => {
            let mut response = response_object(&id, model, "failed", Vec::new(), usage);
            response["error"] = json!({ "code": code, "message": message });
            response
        }
    };
    (StatusCode::OK, Json(response)).into_response()
}

fn render_stream(outcome: Outcome, model: &str) -> Response {
    let id = new_response_id();
    let created_at = now_ts();
    let mut sequence = 0_i64;
    let mut body = String::new();
    let mut emit = |event: &str, mut payload: Value| {
        payload["type"] = json!(event);
        payload["sequence_number"] = json!(sequence);
        sequence += 1;
        body.push_str(&format!("event: {event}\ndata: {payload}\n\n"));
    };
    let initial = json!({
        "id": id, "object": "response", "created_at": created_at,
        "status": "in_progress", "model": model, "output": [],
    });
    emit("response.created", json!({ "response": initial.clone() }));
    emit("response.in_progress", json!({ "response": initial }));
    match outcome {
        Outcome::Complete { item, usage } => {
            emit(
                "response.output_item.added",
                json!({ "output_index": 0, "item": item.clone() }),
            );
            emit(
                "response.output_item.done",
                json!({ "output_index": 0, "item": item.clone() }),
            );
            emit(
                "response.completed",
                json!({ "response": response_object(&id, model, "completed", vec![item], usage) }),
            );
        }
        Outcome::Incomplete { usage } => emit(
            "response.incomplete",
            json!({ "response": response_object(&id, model, "incomplete", Vec::new(), usage) }),
        ),
        Outcome::Failed {
            code,
            message,
            usage,
        } => {
            let mut response = response_object(&id, model, "failed", Vec::new(), usage);
            response["error"] = json!({ "code": code, "message": message });
            emit("response.failed", json!({ "response": response }));
        }
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from(body))
        .unwrap()
}

fn new_response_id() -> String {
    format!("resp_{}", Uuid::new_v4().simple())
}

fn new_compaction_id() -> String {
    format!("cmp_{}", Uuid::new_v4().simple())
}

pub(super) fn encode_payload(summary: &str) -> String {
    format!("{PAYLOAD_PREFIX}{summary}")
}

pub(super) fn decode_payload(payload: &str) -> Option<String> {
    payload
        .strip_prefix(PAYLOAD_PREFIX)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

pub(super) fn restored_context(summary: &str) -> String {
    format!("{RESTORED_CONTEXT_PREFIX}{summary}")
}

#[cfg(test)]
mod tests {
    use super::super::super::converter::convert_request;
    use super::*;

    fn parsed(text: &str, upstream_stop_reason: &str) -> ParsedResponse {
        let finish_reason = match upstream_stop_reason {
            "max_tokens" | CONTEXT_WINDOW_EXCEEDED => "length",
            "tool_use" => "tool_calls",
            _ => "stop",
        };
        ParsedResponse {
            model: "gpt-5.6-sol".to_string(),
            text: text.to_string(),
            tool_calls: Vec::new(),
            upstream_stop_reason: upstream_stop_reason.to_string(),
            finish_reason: finish_reason.to_string(),
            prompt_tokens: 10,
            cached_tokens: 2,
            completion_tokens: 4,
            thinking: String::new(),
            web_searches: Vec::new(),
            credit_usage: None,
            credit_unit: None,
            credit_unit_plural: None,
        }
    }

    #[test]
    fn trigger_must_be_unique_and_final() {
        assert_eq!(
            classify(&json!([{ "type": "compaction_trigger" }])).unwrap(),
            Operation::RemoteCompact
        );
        assert!(classify(&json!([{ "type": "compaction_trigger" }, { "role": "user" }])).is_err());
        assert!(
            classify(&json!([{ "type": "compaction_trigger" }, { "type": "compaction_trigger" }]))
                .is_err()
        );
        assert_eq!(
            classify(&json!([{ "role": "user" }])).unwrap(),
            Operation::Generate
        );
    }

    #[test]
    fn checkpoint_marker_without_trigger_stays_on_message_path() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "instructions": "You are performing a CONTEXT CHECKPOINT COMPACTION.",
            "input": [{ "role": "user", "content": "history" }]
        }))
        .unwrap();

        assert_eq!(classify(&request.input).unwrap(), Operation::Generate);
        let (anthropic, _) = responses_to_anthropic(request, None).unwrap();
        assert!(
            anthropic
                .system
                .unwrap()
                .iter()
                .any(|part| part.text.contains("CONTEXT CHECKPOINT COMPACTION"))
        );
    }

    fn compact_request(input: Value) -> ResponsesRequest {
        serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "input": input,
        }))
        .unwrap()
    }

    fn translate_compact_input(mut request: ResponsesRequest) -> MessagesRequest {
        assert_eq!(classify(&request.input).unwrap(), Operation::RemoteCompact);
        request.input.as_array_mut().unwrap().pop();
        responses_to_anthropic(request, None).unwrap().0
    }

    #[test]
    fn assistant_ended_history_is_preserved_behind_summary_turn() {
        const FINAL_ASSISTANT_MARKER: &str = "FINAL_ASSISTANT_STATE_9f13";
        let mut request = translate_compact_input(compact_request(json!([
            { "type": "message", "role": "user", "content": "do the work" },
            {
                "type": "message",
                "role": "assistant",
                "content": FINAL_ASSISTANT_MARKER
            },
            { "type": "compaction_trigger" }
        ])));

        append_summary_request(&mut request);
        let converted = convert_request(&request).unwrap();

        assert_eq!(
            converted
                .conversation_state
                .current_message
                .user_input_message
                .content,
            SUMMARY_REQUEST
        );
        assert!(converted.conversation_state.history.iter().any(|message| {
            matches!(
                message,
                crate::kiro::model::requests::conversation::Message::Assistant(assistant)
                    if assistant.assistant_response_message.content.contains(FINAL_ASSISTANT_MARKER)
            )
        }));
    }

    #[test]
    fn user_ended_history_is_preserved_behind_summary_turn() {
        const FINAL_USER_MARKER: &str = "FINAL_USER_REQUEST_61b8";
        let mut request = translate_compact_input(compact_request(json!([
            { "type": "message", "role": "user", "content": "initial request" },
            { "type": "message", "role": "assistant", "content": "previous reply" },
            {
                "type": "message",
                "role": "user",
                "content": FINAL_USER_MARKER
            },
            { "type": "compaction_trigger" }
        ])));

        append_summary_request(&mut request);
        let converted = convert_request(&request).unwrap();
        assert_eq!(
            converted
                .conversation_state
                .current_message
                .user_input_message
                .content,
            SUMMARY_REQUEST
        );
        assert!(converted.conversation_state.history.iter().any(|message| {
            matches!(
                message,
                crate::kiro::model::requests::conversation::Message::User(user)
                    if user.user_input_message.content.contains(FINAL_USER_MARKER)
            )
        }));
    }

    #[test]
    fn tool_result_ended_history_is_not_the_active_compaction_turn() {
        const TOOL_RESULT_MARKER: &str = "TOOL_RESULT_STATE_70c4";
        let mut request = translate_compact_input(compact_request(json!([
            { "type": "message", "role": "user", "content": "run the command" },
            {
                "type": "function_call",
                "call_id": "call_70c4",
                "name": "shell",
                "arguments": "{\"command\":\"cargo test\"}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_70c4",
                "output": TOOL_RESULT_MARKER
            },
            { "type": "compaction_trigger" }
        ])));

        append_summary_request(&mut request);
        let converted = convert_request(&request).unwrap();
        let current = &converted
            .conversation_state
            .current_message
            .user_input_message;

        assert_eq!(current.content, SUMMARY_REQUEST);
        assert!(
            current.user_input_message_context.tool_results.is_empty(),
            "historical tool results must not drive the compaction turn"
        );
        assert!(converted.conversation_state.history.iter().any(|message| {
            matches!(
                message,
                crate::kiro::model::requests::conversation::Message::User(user)
                    if user
                        .user_input_message
                        .user_input_message_context
                        .tool_results
                        .iter()
                        .any(|result| {
                            result.tool_use_id == "call_70c4"
                                && result.content.iter().any(|content| {
                                    content.get("text").and_then(Value::as_str)
                                        == Some(TOOL_RESULT_MARKER)
                                })
                        })
            )
        }));
    }

    #[test]
    fn completed_summary_emits_exactly_one_compaction_item() {
        let response = render_stream(validate(parsed("handoff", "end_turn")), "gpt-5.6-sol");
        let body =
            futures::executor::block_on(to_bytes(response.into_body(), 1024 * 1024)).unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(body.matches("event: response.output_item.done").count(), 1);
        assert!(body.contains("\"type\":\"compaction\""));
        assert!(!body.contains("\"type\":\"message\""));
        assert!(body.contains("event: response.completed"));
    }

    #[test]
    fn truncated_or_empty_summary_never_emits_compaction_item() {
        let incomplete = render_stream(validate(parsed("partial", "max_tokens")), "gpt-5.6-sol");
        let incomplete =
            futures::executor::block_on(to_bytes(incomplete.into_body(), 1024 * 1024)).unwrap();
        let incomplete = String::from_utf8(incomplete.to_vec()).unwrap();
        assert!(incomplete.contains("event: response.incomplete"));
        assert!(!incomplete.contains("\"type\":\"compaction\""));

        let failed = render_stream(validate(parsed("   ", "end_turn")), "gpt-5.6-sol");
        let failed =
            futures::executor::block_on(to_bytes(failed.into_body(), 1024 * 1024)).unwrap();
        let failed = String::from_utf8(failed.to_vec()).unwrap();
        assert!(failed.contains("event: response.failed"));
        assert!(!failed.contains("\"type\":\"compaction\""));
    }

    #[test]
    fn context_overflow_is_not_reported_as_max_output_tokens() {
        let response = render_stream(
            validate(parsed("partial", CONTEXT_WINDOW_EXCEEDED)),
            "gpt-5.6-sol",
        );
        let body =
            futures::executor::block_on(to_bytes(response.into_body(), 1024 * 1024)).unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.contains("event: response.failed"));
        assert!(body.contains("\"code\":\"context_length_exceeded\""));
        assert!(!body.contains("event: response.incomplete"));
        assert!(!body.contains("max_output_tokens"));
        assert!(!body.contains("\"type\":\"compaction\""));
    }

    #[test]
    fn retry_truncation_preserves_tool_item_pairing_and_summary_turn() {
        let large_output = format!(
            "BEGIN:{}:END",
            "x".repeat(TOOL_OUTPUT_RETRY_LIMIT_BYTES + 1000)
        );
        let mut request = compact_request(json!([
            { "type": "message", "role": "user", "content": "run it" },
            {
                "type": "function_call",
                "call_id": "call_large",
                "name": "shell",
                "arguments": "{\"command\":\"build\"}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_large",
                "output": large_output
            },
            { "type": "message", "role": "user", "content": "small history stays intact" },
            { "type": "compaction_trigger" }
        ]));
        request.input.as_array_mut().unwrap().pop();

        let stats = truncate_large_tool_outputs(&mut request.input);
        assert_eq!(stats.items, 1);
        assert!(stats.removed_bytes > 0);
        let items = request.input.as_array().unwrap();
        let output_item = &items[2];
        assert_eq!(output_item["type"], "function_call_output");
        assert_eq!(output_item["call_id"], "call_large");
        let output = output_item["output"].as_str().unwrap();
        assert!(output.len() <= TOOL_OUTPUT_RETRY_LIMIT_BYTES);
        assert!(output.starts_with("BEGIN:"));
        assert!(output.ends_with(":END"));
        assert!(output.contains("tool output truncated for compaction retry"));
        assert_eq!(items[3]["content"], "small history stays intact");

        let mut anthropic = responses_to_anthropic(request, None).unwrap().0;
        append_summary_request(&mut anthropic);
        let converted = convert_request(&anthropic).unwrap();
        assert_eq!(
            converted
                .conversation_state
                .current_message
                .user_input_message
                .content,
            SUMMARY_REQUEST
        );
        assert!(converted.conversation_state.history.iter().any(|message| {
            matches!(
                message,
                crate::kiro::model::requests::conversation::Message::User(user)
                    if user
                        .user_input_message
                        .user_input_message_context
                        .tool_results
                        .iter()
                        .any(|result| result.tool_use_id == "call_large")
            )
        }));
    }

    #[test]
    fn retry_truncation_is_utf8_safe() {
        let text = "测".repeat(10_000);
        let truncated = truncate_middle(&text, 1000);
        assert!(truncated.len() <= 1000);
        assert!(truncated.contains("tool output truncated for compaction retry"));
    }

    #[test]
    fn payload_round_trips() {
        let payload = encode_payload("summary");
        assert_eq!(decode_payload(&payload).as_deref(), Some("summary"));
    }
}
