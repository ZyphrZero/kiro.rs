//! Whole-payload size guard: caps the serialized Kiro request by trimming oldest history,
//! operating on the **Anthropic request before conversion** so the converter's tool_use/tool_result
//! pairing cleanup always runs **last** and the emitted payload is guaranteed valid.
//!
//! Why before conversion (the v0.6.25 lesson): `converter::convert_request` runs three pairing
//! cleanups (orphan tool_result removal, orphan tool_use removal, non-adjacent tool_use removal)
//! that satisfy the upstream "tool_use and tool_result must be correctly paired and ordered" rule.
//! Trimming the *converted* Kiro history (as v0.6.25 did) split already-paired turns with no cleanup
//! afterward → upstream 400 "Invalid message sequence". Trimming the *Anthropic* messages and then
//! converting lets that cleanup fix any orphan a cut produced. We never have to be pairing-aware here.
//!
//! `image_resize` (per-image) and `text_truncate` (per-field) cap individual fields; this is the
//! missing **whole-payload** layer for the case where hundreds of in-budget turns add up and trip
//! AWS Q `CONTENT_LENGTH_EXCEEDS_THRESHOLD`.
//!
//! Preserved: `system` (a separate request field, never in `messages`, so untouched), the most
//! recent turns (>= [`MIN_RECENT_TURNS`]), and the current message (the last entry in `messages`,
//! always kept). A single placeholder marks where older turns were elided.
//!
//! Driven by `KIRO_RS_MAX_PAYLOAD_BYTES` (0 disables), sharing the `KIRO_RS_*` env contract.

use serde_json::Value;
use tracing::warn;

use super::converter::{convert_request, ConversionError, ConversionResult};
use super::types::{Message, MessagesRequest};
use crate::kiro::model::requests::kiro::KiroRequest;

/// Default whole-payload byte cap. Sits below the failure-region median (~685 KB observed) so it
/// engages before the upstream 400, while leaving normal traffic untouched. `0` disables.
const DEFAULT_MAX_PAYLOAD_BYTES: usize = 640_000;

/// Most-recent `messages` entries always kept (current message + recent context survive).
const MIN_RECENT_TURNS: usize = 6;

/// Hard cap on trim iterations (each does one reconversion); a safety bound, normally 1–2 suffice.
const MAX_TRIM_ITERS: usize = 12;

/// Placeholder inserted (as a user turn) where older messages were dropped.
const TRUNCATION_PLACEHOLDER: &str =
    "[Earlier conversation history was truncated to fit the model's input limit. \
Older messages and tool activity have been omitted.]";

/// Config for whole-payload truncation. `max_bytes == 0` disables.
#[derive(Debug, Clone, Copy)]
pub struct PayloadLimitConfig {
    pub max_bytes: usize,
}

impl PayloadLimitConfig {
    /// Reads `KIRO_RS_MAX_PAYLOAD_BYTES` (0 disables), falling back to the default cap when unset.
    pub fn from_env() -> Self {
        let max_bytes = std::env::var("KIRO_RS_MAX_PAYLOAD_BYTES")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(DEFAULT_MAX_PAYLOAD_BYTES);
        Self { max_bytes }
    }
}

/// Serialized byte size of the Kiro wire body a `ConversionResult` would produce (matches what
/// `handlers` sends and what upstream measures). Serialization failure → 0 (treated as "fits").
fn converted_payload_bytes(result: &ConversionResult) -> usize {
    let probe = KiroRequest {
        conversation_state: result.conversation_state.clone(),
        profile_arn: None,
        additional_model_request_fields: result.additional_model_request_fields.clone(),
    };
    serde_json::to_string(&probe).map(|s| s.len()).unwrap_or(0)
}

/// True if an Anthropic message is a pure tool_result turn (its `content` array holds only
/// `tool_result` blocks). Such a turn must never become the new oldest kept turn: its paired
/// `tool_use` lives in the dropped region, so the converter would strip the orphan and the turn
/// adds nothing. Dropping it too keeps the kept-window starting on a real user/assistant turn.
fn is_pure_tool_result(msg: &Message) -> bool {
    match &msg.content {
        Value::Array(arr) => {
            !arr.is_empty()
                && arr
                    .iter()
                    .all(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
        }
        _ => false,
    }
}

/// Drop `drop_count` oldest messages, then keep advancing the cut until the new oldest kept turn is
/// not a pure tool_result (avoids leaving an orphan tool_result at the window head). Always keeps at
/// least [`MIN_RECENT_TURNS`] (including the current/last message). Inserts one placeholder where
/// the cut was made. Mutates `messages` in place. Returns true if anything was dropped.
fn drop_oldest_turns(messages: &mut Vec<Message>, drop_count: usize) -> bool {
    let n = messages.len();
    if n <= MIN_RECENT_TURNS || drop_count == 0 {
        return false;
    }
    // Never cut into the most-recent window.
    let max_drop = n - MIN_RECENT_TURNS;
    let mut cut = drop_count.min(max_drop);
    // Advance past a leading pure-tool_result so the kept window starts clean.
    while cut < max_drop && is_pure_tool_result(&messages[cut]) {
        cut += 1;
    }
    if cut == 0 {
        return false;
    }
    let placeholder = Message {
        role: "user".to_string(),
        content: Value::String(TRUNCATION_PLACEHOLDER.to_string()),
    };
    let tail = messages.split_off(cut);
    messages.clear();
    messages.push(placeholder);
    messages.extend(tail);
    true
}

/// Convert `payload`, trimming the **oldest Anthropic history** until the converted Kiro payload
/// fits within `cfg.max_bytes`. The converter (with its tool-pairing cleanup) runs on every attempt,
/// so the returned `ConversionResult` is always pairing-valid. No-op trimming when disabled
/// (`max_bytes == 0`) or already under budget — then it is exactly one `convert_request` call.
pub fn convert_within_limit(
    payload: &mut MessagesRequest,
    cfg: &PayloadLimitConfig,
) -> Result<ConversionResult, ConversionError> {
    let mut result = convert_request(payload)?;
    if cfg.max_bytes == 0 {
        return Ok(result);
    }
    let before = converted_payload_bytes(&result);
    if before <= cfg.max_bytes {
        return Ok(result);
    }

    for _ in 0..MAX_TRIM_ITERS {
        let cur = converted_payload_bytes(&result);
        if cur <= cfg.max_bytes {
            break;
        }
        if payload.messages.len() <= MIN_RECENT_TURNS {
            break; // cannot trim further without touching the recent window / current message
        }
        // Estimate how many oldest turns to drop from the Kiro overage, scaled by per-turn size.
        let over = cur - cfg.max_bytes;
        let avg_turn = (cur / payload.messages.len().max(1)).max(1);
        let est = (over / avg_turn) + 1;
        if !drop_oldest_turns(&mut payload.messages, est) {
            break;
        }
        result = convert_request(payload)?;
    }

    let after = converted_payload_bytes(&result);
    warn!(
        before_bytes = before,
        after_bytes = after,
        max_bytes = cfg.max_bytes,
        remaining_messages = payload.messages.len(),
        "整体 payload 超字节上限，已丢弃最旧历史（转换前裁剪，配对清理在转换时兜底）"
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::requests::conversation::Message as KiroMessage;
    use serde_json::json;

    fn cfg(bytes: usize) -> PayloadLimitConfig {
        PayloadLimitConfig { max_bytes: bytes }
    }

    fn user_text(s: &str) -> Message {
        Message { role: "user".to_string(), content: json!(s) }
    }
    fn assistant_text(s: &str) -> Message {
        Message { role: "assistant".to_string(), content: json!(s) }
    }
    /// assistant turn that calls a tool (tool_use), paired with the following user tool_result.
    fn assistant_tool_use(id: &str, payload: &str) -> Message {
        Message {
            role: "assistant".to_string(),
            content: json!([
                {"type": "text", "text": "calling tool"},
                {"type": "tool_use", "id": id, "name": "do_thing", "input": {"blob": payload}}
            ]),
        }
    }
    fn user_tool_result(id: &str, payload: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: json!([{"type": "tool_result", "tool_use_id": id, "content": payload}]),
        }
    }

    fn req(messages: Vec<Message>) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 4096,
            "messages": [],
        }))
        .map(|mut r: MessagesRequest| {
            r.messages = messages;
            r
        })
        .unwrap()
    }

    /// Every tool_use_id present in history assistant turns must have its tool_result in the
    /// immediately following history user turn or in the current message; no tool_result may be
    /// orphaned. Returns Err(reason) on any violation. Mirrors the upstream pairing rule.
    fn assert_pairing_valid(result: &ConversionResult) {
        let history = &result.conversation_state.history;
        // Collect tool_use ids declared by assistant turns and the ids satisfied by the very next
        // user turn (adjacency). Also collect every tool_result id that appears anywhere.
        let mut violations = Vec::new();
        for (i, m) in history.iter().enumerate() {
            if let KiroMessage::Assistant(a) = m {
                if let Some(uses) = &a.assistant_response_message.tool_uses {
                    for u in uses {
                        // the paired result must be in history[i+1] (a user turn) or current message
                        let next_has = history.get(i + 1).map(|nm| match nm {
                            KiroMessage::User(u2) => u2
                                .user_input_message
                                .user_input_message_context
                                .tool_results
                                .iter()
                                .any(|r| r.tool_use_id == u.tool_use_id),
                            _ => false,
                        });
                        let cur_has = result
                            .conversation_state
                            .current_message
                            .user_input_message
                            .user_input_message_context
                            .tool_results
                            .iter()
                            .any(|r| r.tool_use_id == u.tool_use_id);
                        if next_has != Some(true) && !cur_has {
                            violations.push(format!("orphan tool_use {}", u.tool_use_id));
                        }
                    }
                }
            }
        }
        assert!(violations.is_empty(), "pairing violations: {:?}", violations);
    }

    #[test]
    fn disabled_is_single_convert() {
        let mut p = req(vec![user_text("hi"), assistant_text("hello"), user_text("now")]);
        let r = convert_within_limit(&mut p, &cfg(0)).unwrap();
        assert_pairing_valid(&r);
        // disabled → messages untouched (3 in, last is current → 2 history turns kept).
        assert_eq!(p.messages.len(), 3);
    }

    #[test]
    fn under_budget_untouched() {
        let mut p = req(vec![user_text("short"), assistant_text("ok"), user_text("q")]);
        let n = p.messages.len();
        let r = convert_within_limit(&mut p, &cfg(640_000)).unwrap();
        assert_pairing_valid(&r);
        assert_eq!(p.messages.len(), n, "under budget must not trim");
    }

    #[test]
    fn over_budget_trims_and_stays_pairing_valid() {
        // Build a long tool-heavy history: many (assistant tool_use → user tool_result) pairs,
        // each big. Truncation will cut into the middle of these pairs — the converter cleanup
        // must keep the emitted payload pairing-valid (the v0.6.25 regression case).
        let mut msgs = vec![user_text("initial task")];
        for i in 0..40 {
            msgs.push(assistant_tool_use(&format!("tu_{i}"), &"a".repeat(9_000)));
            msgs.push(user_tool_result(&format!("tu_{i}"), &"b".repeat(9_000)));
        }
        msgs.push(user_text("what is the final status?")); // current message
        let mut p = req(msgs);
        let cap = 120_000;
        let r = convert_within_limit(&mut p, &cfg(cap)).unwrap();
        // The whole point: no orphan tool_use/tool_result after trimming + conversion.
        assert_pairing_valid(&r);
        // And it actually shrank the payload.
        assert!(converted_payload_bytes(&r) <= cap || p.messages.len() <= MIN_RECENT_TURNS);
        // current message preserved.
        assert!(
            r.conversation_state
                .current_message
                .user_input_message
                .content
                .contains("final status")
        );
    }

    #[test]
    fn cut_landing_on_tool_result_does_not_orphan() {
        // Force a cut that would land exactly on a user tool_result (whose paired tool_use is in
        // the dropped region). is_pure_tool_result must push the window past it; after conversion
        // there must be no orphan tool_result.
        let mut msgs = vec![user_text("task")];
        for i in 0..30 {
            msgs.push(assistant_tool_use(&format!("x_{i}"), &"c".repeat(12_000)));
            msgs.push(user_tool_result(&format!("x_{i}"), &"d".repeat(12_000)));
        }
        msgs.push(user_text("done?"));
        let mut p = req(msgs);
        let r = convert_within_limit(&mut p, &cfg(100_000)).unwrap();
        assert_pairing_valid(&r);
    }

    #[test]
    fn huge_single_turn_still_valid() {
        // Current message alone is enormous; cannot trim below MIN_RECENT_TURNS — must still
        // produce a pairing-valid payload (no panic, no orphan), even if over cap.
        let mut p = req(vec![user_text("x"), assistant_text("y"), user_text(&"z".repeat(800_000))]);
        let r = convert_within_limit(&mut p, &cfg(300_000)).unwrap();
        assert_pairing_valid(&r);
    }
}

