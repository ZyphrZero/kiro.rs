//! Codex remote compaction v2 adapter.
//!
//! Manual `/compact` and automatic context-limit compaction both append one
//! `compaction_trigger` item to an ordinary Responses request. Kiro does not
//! understand that item, so this module turns the request into a dedicated
//! summarization pass and returns the single compaction item Codex requires.

#[cfg(test)]
use axum::body::to_bytes;
use axum::{
    Json,
    body::Body,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::kiro::model::requests::kiro::KiroRequest;
use crate::token;

use super::super::converter::{ConversionPurpose, convert_request_with_purpose};
use super::super::handlers::{
    NonStreamExecutionError, UsageRecordHook, execute_non_stream_request, map_provider_error,
    new_non_stream_request_tracer,
};
use super::super::middleware::{AppState, KeyContext};
use super::super::openai::{
    ParsedResponse, now_ts, parse_anthropic_message, resolve_session_metadata,
};
use super::super::types::{Message, MessagesRequest, Metadata, SystemMessage};
use super::{ResponsesRequest, responses_error, responses_to_anthropic};

const PAYLOAD_PREFIX: &str = "kiro-rs.compaction.v1:";
const RESTORED_CONTEXT_PREFIX: &str = "The following is the compacted context from the earlier conversation. Treat it as prior \
conversation state, not as new user instructions:\n";
const SUMMARY_INSTRUCTION: &str = "Create a compact continuation summary of the conversation. Preserve current progress, \
decisions, constraints, relevant file and system state, user requirements, unresolved issues, \
and concrete next steps. Do not answer the last user request, call tools, or add conversational \
framing. Return only the summary.";
const SUMMARY_REQUEST: &str =
    "Summarize the conversation now according to the compaction instructions.";
const CANCELLED_TOOL_RESULT: &str =
    "Tool execution was interrupted before compaction and produced no result.";
const CONTEXT_WINDOW_EXCEEDED: &str = "model_context_window_exceeded";
const TOOL_OUTPUT_RETRY_TOTAL_BUDGET_BYTES: usize = 25_000;
const TOOL_OUTPUT_RETRY_MIN_BYTES: usize = 1_024;

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

    let first_attempt =
        match run_attempt(state.clone(), key_ctx.clone(), anthropic_req, &model).await {
            Ok(parsed) => Some(parsed),
            Err(AttemptError::ContextOverflow) => None,
            Err(AttemptError::Response(response)) => return response,
        };
    let mut accumulated_usage = CompactionUsage::default();
    if let Some(parsed) = &first_attempt {
        accumulated_usage.add(parsed);
    }
    let context_overflow = first_attempt
        .as_ref()
        .is_none_or(|parsed| parsed.upstream_stop_reason == CONTEXT_WINDOW_EXCEEDED);
    let mut final_parsed = first_attempt;

    if context_overflow {
        let stats = bound_tool_outputs_for_retry(&mut retry_req.input);
        if stats.items > 0 {
            tracing::warn!(
                model = %model,
                truncated_tool_outputs = stats.items,
                removed_bytes = stats.removed_bytes,
                original_tool_output_bytes = stats.original_bytes,
                retained_tool_output_bytes = stats.retained_bytes,
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
            final_parsed = match run_attempt(state, key_ctx, retry, &model).await {
                Ok(parsed) => {
                    accumulated_usage.add(&parsed);
                    Some(parsed)
                }
                Err(AttemptError::ContextOverflow) => None,
                Err(AttemptError::Response(response)) => return response,
            };
        } else {
            tracing::warn!(
                model = %model,
                "Kiro compaction exceeded the context window and had no oversized tool outputs to reduce"
            );
        }
    }

    let usage = accumulated_usage.into_json();
    let outcome = match final_parsed {
        Some(parsed) => validate(parsed, usage),
        None => context_overflow_outcome(usage),
    };
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
    prepare_summary_turn(&mut anthropic_req)?;
    anthropic_req
        .system
        .get_or_insert_with(Vec::new)
        .push(SystemMessage {
            text: SUMMARY_INSTRUCTION.to_string(),
            cache_control: None,
        });
    Ok(anthropic_req)
}

enum AttemptError {
    ContextOverflow,
    Response(Response),
}

async fn run_attempt(
    state: AppState,
    key_ctx: KeyContext,
    anthropic_req: MessagesRequest,
    model: &str,
) -> Result<ParsedResponse, AttemptError> {
    let provider = match &state.kiro_provider {
        Some(provider) => provider.clone(),
        None => {
            return Err(AttemptError::Response(responses_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "Kiro API provider not configured",
            )));
        }
    };

    let conversion = convert_request_with_purpose(
        &anthropic_req,
        state.tool_compatibility_mode,
        ConversionPurpose::Compact,
    )
    .map_err(|error| {
        AttemptError::Response(responses_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &error.to_string(),
        ))
    })?;
    let kiro_request = KiroRequest {
        conversation_state: conversion.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion.additional_model_request_fields,
    };
    let request_body = serde_json::to_string(&kiro_request).map_err(|error| {
        AttemptError::Response(responses_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("failed to serialize compaction request: {error}"),
        ))
    })?;
    tracing::debug!("Kiro compaction request body: {}", request_body);

    let input_tokens = token::count_all_tokens(
        anthropic_req.model.clone(),
        anthropic_req.system.clone(),
        anthropic_req.messages.clone(),
        anthropic_req.tools.clone(),
    ) as i32;
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, model.to_string());
    let cache_usage = state
        .cache_meter
        .as_ref()
        .map(|cache| {
            super::super::cache_metering::compute_cache_usage(cache, &anthropic_req, key_ctx.key_id)
        })
        .unwrap_or_default();
    let tracer = new_non_stream_request_tracer(&state, key_ctx.clone(), model.to_string());
    let anthropic = execute_non_stream_request(
        provider,
        &request_body,
        model,
        input_tokens,
        false,
        conversion.tool_name_map,
        hook,
        cache_usage,
        tracer,
        key_ctx.group.clone(),
    )
    .await
    .map_err(|error| match error {
        NonStreamExecutionError::Provider(error)
            if error
                .downcast_ref::<crate::kiro::error::UpstreamContextOverflowError>()
                .is_some() =>
        {
            AttemptError::ContextOverflow
        }
        NonStreamExecutionError::Provider(error) => {
            AttemptError::Response(map_provider_error(error))
        }
        NonStreamExecutionError::Response(response) => AttemptError::Response(response),
    })?;

    let parsed = parse_anthropic_message(&anthropic, model);
    tracing::info!(
        model = %model,
        stop_reason = %parsed.upstream_stop_reason,
        input_tokens = parsed.prompt_tokens,
        output_tokens = parsed.completion_tokens,
        "Kiro compaction attempt finished"
    );
    Ok(parsed)
}

/// Keep the last real turn as Kiro's current message whenever it is already a
/// user turn. Only assistant-ended history needs a synthetic user turn because
/// Kiro cannot generate from an assistant prefill.
fn prepare_summary_turn(req: &mut MessagesRequest) -> Result<(), String> {
    let tool_names = historical_tool_names(&req.messages);
    let last = req
        .messages
        .last_mut()
        .ok_or_else(|| "compaction input must contain at least one message".to_string())?;

    match last.role.as_str() {
        "user" => append_text_block(&mut last.content, SUMMARY_REQUEST)?,
        "assistant" => {
            let cancelled = terminal_tool_uses(&last.content)
                .into_iter()
                .map(|(id, _)| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": CANCELLED_TOOL_RESULT,
                        "is_error": true,
                    })
                })
                .chain(std::iter::once(json!({
                    "type": "text",
                    "text": SUMMARY_REQUEST,
                })))
                .collect::<Vec<_>>();
            req.messages.push(Message {
                role: "user".to_string(),
                content: Value::Array(cancelled),
            });
        }
        role => return Err(format!("unsupported final compaction message role: {role}")),
    }

    if !tool_names.is_empty() {
        let mapping = serde_json::to_string(&tool_names)
            .map_err(|error| format!("failed to serialize historical tool mapping: {error}"))?;
        req.system
            .get_or_insert_with(Vec::new)
            .push(SystemMessage {
                text: format!(
                    "Historical tool calls are represented by an inert placeholder in the upstream request. The original call_id to tool-name mapping is: {mapping}"
                ),
                cache_control: None,
            });
    }
    Ok(())
}

fn append_text_block(content: &mut Value, text: &str) -> Result<(), String> {
    match content {
        Value::Array(blocks) => blocks.push(json!({ "type": "text", "text": text })),
        Value::String(existing) => {
            let existing = std::mem::take(existing);
            *content = json!([
                { "type": "text", "text": existing },
                { "type": "text", "text": text }
            ]);
        }
        _ => return Err("final user message has unsupported content".to_string()),
    }
    Ok(())
}

fn terminal_tool_uses(content: &Value) -> Vec<(String, String)> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| {
            Some((
                block.get("id")?.as_str()?.to_string(),
                block.get("name")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

fn historical_tool_names(messages: &[Message]) -> std::collections::BTreeMap<String, String> {
    messages
        .iter()
        .filter(|message| message.role == "assistant")
        .flat_map(|message| terminal_tool_uses(&message.content))
        .collect()
}

#[derive(Default)]
struct TruncationStats {
    items: usize,
    removed_bytes: usize,
    original_bytes: usize,
    retained_bytes: usize,
}

/// Applies Kiro's aggressive compaction bound only after a confirmed context
/// overflow. The Responses item and call id remain intact, so tool pairing is
/// preserved when the retry is translated back to Kiro history.
fn bound_tool_outputs_for_retry(input: &mut Value) -> TruncationStats {
    let mut stats = TruncationStats::default();
    let Some(items) = input.as_array_mut() else {
        return stats;
    };

    let outputs = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let item_type = item.get("type").and_then(Value::as_str);
            if !matches!(
                item_type,
                Some("function_call_output" | "custom_tool_call_output")
            ) {
                return None;
            }
            let output = item.get("output")?;
            let text = match output {
                Value::String(text) => text.clone(),
                structured => structured.to_string(),
            };
            Some((index, text))
        })
        .collect::<Vec<_>>();

    stats.original_bytes = outputs.iter().map(|(_, text)| text.len()).sum();
    stats.retained_bytes = stats.original_bytes;
    if stats.original_bytes <= TOOL_OUTPUT_RETRY_TOTAL_BUDGET_BYTES || outputs.is_empty() {
        return stats;
    }

    // Every result retains at least an equal floor. Any remaining excess is
    // removed oldest-first, which leaves the newest results intact whenever
    // the aggregate budget allows it.
    let per_item_floor =
        TOOL_OUTPUT_RETRY_MIN_BYTES.min(TOOL_OUTPUT_RETRY_TOTAL_BUDGET_BYTES / outputs.len());
    let mut excess = stats
        .original_bytes
        .saturating_sub(TOOL_OUTPUT_RETRY_TOTAL_BUDGET_BYTES);

    for (index, text) in outputs {
        if excess == 0 {
            break;
        }
        let minimum = per_item_floor.min(text.len());
        let removed = excess.min(text.len().saturating_sub(minimum));
        if removed == 0 {
            continue;
        }
        let target = text.len().saturating_sub(removed);
        let truncated = truncate_middle(&text, target);
        let actual_removed = text.len().saturating_sub(truncated.len());
        if actual_removed == 0 {
            continue;
        }
        if let Some(output) = items[index].get_mut("output") {
            *output = Value::String(truncated);
        }
        stats.items += 1;
        stats.removed_bytes = stats.removed_bytes.saturating_add(actual_removed);
        stats.retained_bytes = stats.retained_bytes.saturating_sub(actual_removed);
        excess = excess.saturating_sub(actual_removed);
    }

    // `truncate_middle` never exceeds its target, so this can only fail if a
    // future output representation stops being mutable in place.
    debug_assert!(stats.retained_bytes <= TOOL_OUTPUT_RETRY_TOTAL_BUDGET_BYTES);
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

#[derive(Default)]
struct CompactionUsage {
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
    credit_usage: Option<f64>,
    credit_unit: Option<String>,
    credit_unit_plural: Option<String>,
    credit_units_consistent: bool,
}

impl CompactionUsage {
    fn add(&mut self, parsed: &ParsedResponse) {
        self.input_tokens = self
            .input_tokens
            .saturating_add(parsed.prompt_tokens.max(0));
        self.cached_tokens = self
            .cached_tokens
            .saturating_add(parsed.cached_tokens.max(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(parsed.completion_tokens.max(0));

        let Some(credit_usage) = parsed.credit_usage else {
            return;
        };
        if self.credit_usage.is_none() {
            self.credit_units_consistent = true;
            self.credit_unit = parsed.credit_unit.clone();
            self.credit_unit_plural = parsed.credit_unit_plural.clone();
        } else if option_strings_conflict(&self.credit_unit, &parsed.credit_unit)
            || option_strings_conflict(&self.credit_unit_plural, &parsed.credit_unit_plural)
        {
            self.credit_units_consistent = false;
            tracing::warn!(
                previous_unit = ?self.credit_unit,
                retry_unit = ?parsed.credit_unit,
                "compaction retry returned inconsistent credit units; omitting credit metadata"
            );
        } else {
            if self.credit_unit.is_none() {
                self.credit_unit = parsed.credit_unit.clone();
            }
            if self.credit_unit_plural.is_none() {
                self.credit_unit_plural = parsed.credit_unit_plural.clone();
            }
        }
        self.credit_usage = Some(self.credit_usage.unwrap_or(0.0).max(0.0) + credit_usage.max(0.0));
    }

    fn into_json(self) -> Value {
        let mut usage = json!({
            "input_tokens": self.input_tokens,
            "input_tokens_details": { "cached_tokens": self.cached_tokens },
            "output_tokens": self.output_tokens,
            "output_tokens_details": { "reasoning_tokens": 0 },
            "total_tokens": self.input_tokens.saturating_add(self.output_tokens),
        });
        if self.credit_units_consistent {
            if let Some(credit_usage) = self.credit_usage {
                usage["credit_usage"] = json!(credit_usage);
            }
            if let Some(credit_unit) = self.credit_unit {
                usage["credit_unit"] = json!(credit_unit);
            }
            if let Some(credit_unit_plural) = self.credit_unit_plural {
                usage["credit_unit_plural"] = json!(credit_unit_plural);
            }
        }
        usage
    }
}

fn option_strings_conflict(left: &Option<String>, right: &Option<String>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}

fn validate(parsed: ParsedResponse, usage: Value) -> Outcome {
    match parsed.upstream_stop_reason.as_str() {
        "max_tokens" => return Outcome::Incomplete { usage },
        CONTEXT_WINDOW_EXCEEDED => return context_overflow_outcome(usage),
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

fn context_overflow_outcome(usage: Value) -> Outcome {
    Outcome::Failed {
        code: "context_length_exceeded",
        message: "upstream context window exceeded after compaction recovery".to_string(),
        usage,
    }
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
    use super::*;

    fn convert_compact(
        request: &MessagesRequest,
    ) -> Result<
        super::super::super::converter::ConversionResult,
        super::super::super::converter::ConversionError,
    > {
        convert_request_with_purpose(
            request,
            crate::model::config::ToolCompatibilityMode::ClaudeCode,
            ConversionPurpose::Compact,
        )
    }

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

    fn validated(parsed: ParsedResponse) -> Outcome {
        let mut usage = CompactionUsage::default();
        usage.add(&parsed);
        validate(parsed, usage.into_json())
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

        prepare_summary_turn(&mut request).unwrap();
        let converted = convert_compact(&request).unwrap();

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

        prepare_summary_turn(&mut request).unwrap();
        let converted = convert_compact(&request).unwrap();
        let current = &converted
            .conversation_state
            .current_message
            .user_input_message;
        assert!(current.content.contains(FINAL_USER_MARKER));
        assert!(current.content.contains(SUMMARY_REQUEST));
        assert!(!converted.conversation_state.history.iter().any(|message| {
            matches!(
                message,
                crate::kiro::model::requests::conversation::Message::User(user)
                    if user.user_input_message.content.contains(FINAL_USER_MARKER)
            )
        }));
        assert!(!converted.conversation_state.history.iter().any(|message| {
            matches!(
                message,
                crate::kiro::model::requests::conversation::Message::Assistant(assistant)
                    if assistant.assistant_response_message.content == "OK"
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

        prepare_summary_turn(&mut request).unwrap();
        let converted = convert_compact(&request).unwrap();
        let current = &converted
            .conversation_state
            .current_message
            .user_input_message;

        assert!(current.content.contains(SUMMARY_REQUEST));
        assert!(
            current
                .user_input_message_context
                .tool_results
                .iter()
                .any(|result| {
                    result.tool_use_id == "call_70c4"
                        && result.content.iter().any(|content| {
                            content.get("text").and_then(Value::as_str) == Some(TOOL_RESULT_MARKER)
                        })
                })
        );
        assert!(!converted.conversation_state.history.iter().any(|message| {
            matches!(
                message,
                crate::kiro::model::requests::conversation::Message::User(user)
                    if user.user_input_message.user_input_message_context.tool_results.iter()
                        .any(|result| result.tool_use_id == "call_70c4")
            )
        }));
        let assistant_tool = converted
            .conversation_state
            .history
            .iter()
            .find_map(|message| match message {
                crate::kiro::model::requests::conversation::Message::Assistant(assistant) => {
                    assistant.assistant_response_message.tool_uses.as_ref()
                }
                _ => None,
            })
            .and_then(|uses| uses.first())
            .unwrap();
        assert_eq!(assistant_tool.name, "kiro_compaction_history_tool");
        let tools = &current.user_input_message_context.tools;
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].tool_specification.name,
            "kiro_compaction_history_tool"
        );
        assert_ne!(tools[0].tool_specification.name, "shell");
    }

    #[test]
    fn unfinished_terminal_tool_call_gets_explicit_cancelled_result() {
        let mut request = translate_compact_input(compact_request(json!([
            { "type": "message", "role": "user", "content": "run it" },
            {
                "type": "function_call",
                "call_id": "call_pending",
                "name": "exec",
                "arguments": "{\"command\":\"pwd\",\"timeout\":1000}"
            },
            { "type": "compaction_trigger" }
        ])));

        prepare_summary_turn(&mut request).unwrap();
        assert!(
            request
                .system
                .as_ref()
                .unwrap()
                .iter()
                .any(|part| { part.text.contains("call_pending") && part.text.contains("exec") })
        );

        let converted = convert_compact(&request).unwrap();
        let current = &converted
            .conversation_state
            .current_message
            .user_input_message;
        let cancelled = current
            .user_input_message_context
            .tool_results
            .iter()
            .find(|result| result.tool_use_id == "call_pending")
            .unwrap();
        assert_eq!(cancelled.status.as_deref(), Some("error"));
        assert!(cancelled.content.iter().any(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("interrupted before compaction"))
        }));

        let historical_use = converted
            .conversation_state
            .history
            .iter()
            .find_map(|message| match message {
                crate::kiro::model::requests::conversation::Message::Assistant(assistant) => {
                    assistant.assistant_response_message.tool_uses.as_ref()
                }
                _ => None,
            })
            .and_then(|uses| uses.first())
            .unwrap();
        assert_eq!(historical_use.tool_use_id, "call_pending");
        assert_eq!(historical_use.name, "kiro_compaction_history_tool");
        assert_eq!(historical_use.input["command"], "pwd");
        assert_eq!(historical_use.input["timeout"], 1000);
    }

    #[test]
    fn compact_rejects_orphaned_current_tool_result() {
        let request = MessagesRequest {
            model: "gpt-5.6-sol".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([
                    {
                        "type": "tool_result",
                        "tool_use_id": "missing_call",
                        "content": "output"
                    },
                    { "type": "text", "text": SUMMARY_REQUEST }
                ]),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            metadata: None,
            thinking: None,
            output_config: None,
        };
        assert!(matches!(
            convert_compact(&request),
            Err(super::super::super::converter::ConversionError::InvalidMessageSequence(_))
        ));
    }

    #[test]
    fn completed_summary_emits_exactly_one_compaction_item() {
        let response = render_stream(validated(parsed("handoff", "end_turn")), "gpt-5.6-sol");
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
        let incomplete = render_stream(validated(parsed("partial", "max_tokens")), "gpt-5.6-sol");
        let incomplete =
            futures::executor::block_on(to_bytes(incomplete.into_body(), 1024 * 1024)).unwrap();
        let incomplete = String::from_utf8(incomplete.to_vec()).unwrap();
        assert!(incomplete.contains("event: response.incomplete"));
        assert!(!incomplete.contains("\"type\":\"compaction\""));

        let failed = render_stream(validated(parsed("   ", "end_turn")), "gpt-5.6-sol");
        let failed =
            futures::executor::block_on(to_bytes(failed.into_body(), 1024 * 1024)).unwrap();
        let failed = String::from_utf8(failed.to_vec()).unwrap();
        assert!(failed.contains("event: response.failed"));
        assert!(!failed.contains("\"type\":\"compaction\""));
    }

    #[test]
    fn upstream_tool_call_never_becomes_a_compaction_item() {
        let mut response = parsed("", "tool_use");
        response.tool_calls.push(json!({
            "id": "call_dummy",
            "type": "function",
            "function": {
                "name": "kiro_compaction_history_tool",
                "arguments": "{}"
            }
        }));
        let response = render_stream(validated(response), "gpt-5.6-sol");
        let body =
            futures::executor::block_on(to_bytes(response.into_body(), 1024 * 1024)).unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("event: response.failed"));
        assert!(body.contains("upstream returned a tool call"));
        assert!(!body.contains("\"type\":\"compaction\""));
    }

    #[test]
    fn context_overflow_is_not_reported_as_max_output_tokens() {
        let response = render_stream(
            validated(parsed("partial", CONTEXT_WINDOW_EXCEEDED)),
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
            "x".repeat(TOOL_OUTPUT_RETRY_TOTAL_BUDGET_BYTES + 1000)
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

        let stats = bound_tool_outputs_for_retry(&mut request.input);
        assert_eq!(stats.items, 1);
        assert!(stats.removed_bytes > 0);
        let items = request.input.as_array().unwrap();
        let output_item = &items[2];
        assert_eq!(output_item["type"], "function_call_output");
        assert_eq!(output_item["call_id"], "call_large");
        let output = output_item["output"].as_str().unwrap();
        assert!(output.len() <= TOOL_OUTPUT_RETRY_TOTAL_BUDGET_BYTES);
        assert!(output.starts_with("BEGIN:"));
        assert!(output.ends_with(":END"));
        assert!(output.contains("tool output truncated for compaction retry"));
        assert_eq!(items[3]["content"], "small history stays intact");

        let mut anthropic = responses_to_anthropic(request, None).unwrap().0;
        prepare_summary_turn(&mut anthropic).unwrap();
        let converted = convert_compact(&anthropic).unwrap();
        let current = &converted
            .conversation_state
            .current_message
            .user_input_message;
        assert!(current.content.contains("small history stays intact"));
        assert!(current.content.contains(SUMMARY_REQUEST));
        assert!(
            current
                .user_input_message_context
                .tool_results
                .iter()
                .any(|result| result.tool_use_id == "call_large")
        );
    }

    #[test]
    fn retry_truncation_is_utf8_safe() {
        let text = "测".repeat(10_000);
        let truncated = truncate_middle(&text, 1000);
        assert!(truncated.len() <= 1000);
        assert!(truncated.contains("tool output truncated for compaction retry"));
    }

    #[test]
    fn retry_budget_bounds_many_individually_small_outputs_and_keeps_newest() {
        let newest = format!("NEWEST:{}", "n".repeat(9_990));
        let mut input = Value::Array(
            (0..4)
                .map(|index| {
                    json!({
                        "type": "function_call_output",
                        "call_id": format!("call_{index}"),
                        "output": if index == 3 {
                            newest.clone()
                        } else {
                            format!("OLD_{index}:{}", "o".repeat(9_993))
                        }
                    })
                })
                .collect(),
        );

        let stats = bound_tool_outputs_for_retry(&mut input);
        assert!(stats.items > 0);
        assert!(stats.original_bytes > TOOL_OUTPUT_RETRY_TOTAL_BUDGET_BYTES);
        assert!(stats.retained_bytes <= TOOL_OUTPUT_RETRY_TOTAL_BUDGET_BYTES);
        let items = input.as_array().unwrap();
        assert_eq!(items[3]["output"].as_str(), Some(newest.as_str()));
        assert!(
            items[0]["output"]
                .as_str()
                .unwrap()
                .contains("tool output truncated for compaction retry")
        );
    }

    #[test]
    fn retry_usage_accumulates_tokens_and_credit_metadata() {
        let mut first = parsed("partial", CONTEXT_WINDOW_EXCEEDED);
        first.credit_usage = Some(0.25);
        first.credit_unit = Some("credit".to_string());
        first.credit_unit_plural = Some("credits".to_string());
        let mut second = parsed("summary", "end_turn");
        second.prompt_tokens = 20;
        second.cached_tokens = 3;
        second.completion_tokens = 6;
        second.credit_usage = Some(0.5);
        second.credit_unit = Some("credit".to_string());
        second.credit_unit_plural = Some("credits".to_string());

        let mut usage = CompactionUsage::default();
        usage.add(&first);
        usage.add(&second);
        let usage = usage.into_json();
        assert_eq!(usage["input_tokens"], 30);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 5);
        assert_eq!(usage["output_tokens"], 10);
        assert_eq!(usage["total_tokens"], 40);
        assert_eq!(usage["credit_usage"], 0.75);
        assert_eq!(usage["credit_unit"], "credit");
        assert_eq!(usage["credit_unit_plural"], "credits");
    }

    #[test]
    fn generate_wrapper_keeps_the_existing_conversion_behavior() {
        let request = MessagesRequest {
            model: "gpt-5.6-sol".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([{ "type": "text", "text": "hello" }]),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            metadata: None,
            thinking: None,
            output_config: None,
        };
        let wrapper = super::super::super::converter::convert_request_with_mode(
            &request,
            crate::model::config::ToolCompatibilityMode::ClaudeCode,
        )
        .unwrap();
        let explicit = convert_request_with_purpose(
            &request,
            crate::model::config::ToolCompatibilityMode::ClaudeCode,
            ConversionPurpose::Generate,
        )
        .unwrap();
        let normalize = |state| {
            let mut value = serde_json::to_value(state).unwrap();
            let object = value.as_object_mut().unwrap();
            object.remove("conversationId");
            object.remove("agentContinuationId");
            value
        };
        assert_eq!(
            normalize(wrapper.conversation_state),
            normalize(explicit.conversation_state)
        );
        assert_eq!(wrapper.tool_name_map, explicit.tool_name_map);
        assert_eq!(wrapper.known_tool_names, explicit.known_tool_names);
    }

    #[test]
    fn payload_round_trips() {
        let payload = encode_payload("summary");
        assert_eq!(decode_payload(&payload).as_deref(), Some("summary"));
    }
}
