//! Whole-payload truncation (overall byte cap on the serialized Kiro request)
//!
//! `image_resize` and `text_truncate` cap **individual fields**. But a request can stay under
//! every per-field cap and still trip the AWS Q (`q.us-east-1.amazonaws.com`)
//! `CONTENT_LENGTH_EXCEEDS_THRESHOLD` (400 Bad Request) because *hundreds of history turns add up*.
//! This module is the missing layer: it caps the **entire serialized payload** and, when over,
//! drops the oldest conversation history turns until it fits — mirroring kiro-go's
//! `truncatePayloadToLimit`.
//!
//! What is preserved (in order):
//!   - the system-priming pair at the front of history (a `User` carrying the system prompt +
//!     its paired `Assistant("I will follow these instructions.")`), if present,
//!   - the most recent turns (at least `MIN_RECENT_HISTORY_TURNS`),
//!   - the current message (it lives in `current_message`, never in history, so it is never
//!     dropped here — only hard-truncated as a last resort).
//!
//! A single placeholder note is inserted where older turns were elided so the model knows context
//! was cut. Runs during request conversion, **before** the provider acquires any account permit, so
//! it costs zero concurrency.
//!
//! Driven by `KIRO_RS_MAX_PAYLOAD_BYTES` (0 disables), sharing the `KIRO_RS_*` env contract.

use tracing::warn;

use crate::kiro::model::requests::conversation::{HistoryUserMessage, Message};
use crate::kiro::model::requests::kiro::KiroRequest;

/// Default whole-payload byte cap. Sits below the failure-region median (~685 KB observed) so it
/// engages before the upstream 400, while leaving normal traffic untouched. `0` disables.
const DEFAULT_MAX_PAYLOAD_BYTES: usize = 640_000;

/// The assistant half of the system-priming pair (see `converter::build_history`). Used to detect
/// whether history begins with a priming pair that must be preserved.
const PRIMING_ASSISTANT_MARKER: &str = "I will follow these instructions.";

/// Most-recent history entries always kept (so recent context survives even when over budget).
const MIN_RECENT_HISTORY_TURNS: usize = 4;

/// Placeholder inserted where older turns were dropped.
const TRUNCATION_PLACEHOLDER: &str =
    "[Earlier conversation history was truncated to fit the model's input limit. \
Older messages and tool activity have been omitted.]";

/// Minimal fallback when even the current message must be emptied.
const MINIMAL_FALLBACK_CONTENT: &str = "[content truncated]";

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

/// Serialized byte size of the whole request (the exact wire body the upstream measures).
/// Serialization failure returns 0 (treated as "fits"); the real send path surfaces such errors.
fn payload_byte_size(req: &KiroRequest) -> usize {
    serde_json::to_string(req).map(|s| s.len()).unwrap_or(0)
}

/// Serialized byte size of one history entry, plus 1 for the JSON array `,` delimiter.
fn entry_byte_size(entry: &Message) -> usize {
    serde_json::to_string(entry).map(|s| s.len() + 1).unwrap_or(0)
}

/// True if the entry is an assistant turn (used to avoid a tail starting with an orphan assistant).
fn is_assistant(entry: &Message) -> bool {
    matches!(entry, Message::Assistant(_))
}

/// Largest byte index `<= max` that lands on a UTF-8 char boundary of `s`.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Number of leading history entries that form the system-priming pair (0 or 2).
///
/// The pair is a `User` (system prompt) immediately followed by an `Assistant` whose content is
/// exactly [`PRIMING_ASSISTANT_MARKER`] — the fixed reply `converter::build_history` injects. These
/// two carry the system prompt and must survive truncation.
fn priming_count(history: &[Message]) -> usize {
    if history.len() >= 2 {
        if let (Message::User(_), Message::Assistant(a)) = (&history[0], &history[1]) {
            if a.assistant_response_message.content == PRIMING_ASSISTANT_MARKER {
                return 2;
            }
        }
    }
    0
}

/// Hard-truncate the current message's content as a last resort (priming + retained tail + current
/// still exceed the cap). Cuts at a UTF-8 boundary; if no budget remains, replaces with a marker.
fn truncate_current_message(req: &mut KiroRequest, max_bytes: usize) {
    let content_len = req
        .conversation_state
        .current_message
        .user_input_message
        .content
        .len();
    let overhead = payload_byte_size(req).saturating_sub(content_len);
    let budget = max_bytes.saturating_sub(overhead);
    let content = &mut req
        .conversation_state
        .current_message
        .user_input_message
        .content;
    if content.len() > budget {
        if budget == 0 {
            *content = MINIMAL_FALLBACK_CONTENT.to_string();
        } else {
            let cut = floor_char_boundary(content, budget);
            content.truncate(cut);
        }
    }
}

/// Drop the oldest conversation history turns until the serialized payload fits within
/// `cfg.max_bytes`. No-op when disabled (`max_bytes == 0`) or already under budget.
///
/// Preserves the system-priming pair, the most recent turns (>= [`MIN_RECENT_HISTORY_TURNS`]), and
/// the current message; inserts a single [`TRUNCATION_PLACEHOLDER`] where older turns were elided.
pub fn truncate_payload_to_limit(req: &mut KiroRequest, cfg: &PayloadLimitConfig) {
    if cfg.max_bytes == 0 {
        return;
    }
    let before = payload_byte_size(req);
    if before <= cfg.max_bytes {
        return;
    }

    let history = std::mem::take(&mut req.conversation_state.history);
    let pc = priming_count(&history);
    let priming: Vec<Message> = history[..pc].to_vec();
    let conversation: Vec<Message> = history[pc..].to_vec();

    let model_id = req
        .conversation_state
        .current_message
        .user_input_message
        .model_id
        .clone();
    let placeholder = Message::User(HistoryUserMessage::new(TRUNCATION_PLACEHOLDER, &model_id));

    // Precompute each conversation entry's serialized size once (O(n)).
    let entry_sizes: Vec<usize> = conversation.iter().map(entry_byte_size).collect();

    // Base size = payload carrying only priming, plus the placeholder we will insert.
    req.conversation_state.history = priming.clone();
    let base_size = payload_byte_size(req) + entry_byte_size(&placeholder);

    // Keep the largest recent suffix that fits, but never fewer than MIN_RECENT_HISTORY_TURNS.
    let mut keep_from = conversation.len();
    let mut running = base_size;
    for i in (0..conversation.len()).rev() {
        running += entry_sizes[i];
        let kept = conversation.len() - i;
        if running > cfg.max_bytes && kept > MIN_RECENT_HISTORY_TURNS {
            break;
        }
        keep_from = i;
    }

    // Tail must not start with an orphan assistant (its paired user turn was dropped).
    let mut tail: Vec<Message> = conversation[keep_from..].to_vec();
    while !tail.is_empty() && is_assistant(&tail[0]) {
        tail.remove(0);
    }

    let mut rebuilt: Vec<Message> = Vec::with_capacity(priming.len() + 1 + tail.len());
    rebuilt.extend(priming);
    if keep_from > 0 {
        // Older turns were dropped → note the elision so the model knows context was cut.
        rebuilt.push(placeholder);
    }
    rebuilt.extend(tail);
    req.conversation_state.history = rebuilt;

    // Current message (or retained tail) alone may still exceed the cap → shrink current message.
    if payload_byte_size(req) > cfg.max_bytes {
        truncate_current_message(req, cfg.max_bytes);
    }

    let after = payload_byte_size(req);
    warn!(
        before_bytes = before,
        after_bytes = after,
        max_bytes = cfg.max_bytes,
        "整体 payload 超字节上限，已丢弃最旧历史以适配（防 CONTENT_LENGTH_EXCEEDS_THRESHOLD）"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::requests::conversation::{
        ConversationState, CurrentMessage, HistoryAssistantMessage, UserInputMessage,
    };

    fn req_with(history: Vec<Message>, current: &str) -> KiroRequest {
        let state = ConversationState::new("conv-test")
            .with_history(history)
            .with_current_message(CurrentMessage::new(UserInputMessage::new(
                current, "claude-opus-4.8",
            )));
        KiroRequest {
            conversation_state: state,
            profile_arn: None,
            additional_model_request_fields: None,
        }
    }

    fn user(content: &str) -> Message {
        Message::User(HistoryUserMessage::new(content, "claude-opus-4.8"))
    }
    fn assistant(content: &str) -> Message {
        Message::Assistant(HistoryAssistantMessage::new(content))
    }
    fn priming() -> Vec<Message> {
        vec![user("SYSTEM PROMPT"), assistant(PRIMING_ASSISTANT_MARKER)]
    }

    #[test]
    fn disabled_is_noop() {
        let mut req = req_with(vec![user(&"x".repeat(50_000)); 40], "now");
        let before = payload_byte_size(&req);
        truncate_payload_to_limit(&mut req, &PayloadLimitConfig { max_bytes: 0 });
        assert_eq!(payload_byte_size(&req), before, "max_bytes=0 必须零改动");
    }

    #[test]
    fn under_budget_is_noop() {
        let mut req = req_with(vec![user("short"), assistant("ok")], "now");
        let n = req.conversation_state.history.len();
        truncate_payload_to_limit(&mut req, &PayloadLimitConfig { max_bytes: 640_000 });
        assert_eq!(req.conversation_state.history.len(), n, "未超预算不应裁剪");
    }

    #[test]
    fn over_budget_fits_and_keeps_priming_and_recent() {
        // 大量历史撑爆；保留 priming(2) + 最近若干 + 占位，结果必须 ≤ 上限。
        let mut history = priming();
        for i in 0..60 {
            history.push(user(&format!("user turn {i} {}", "a".repeat(8_000))));
            history.push(assistant(&format!("assistant turn {i} {}", "b".repeat(8_000))));
        }
        let mut req = req_with(history, "current question");
        let cap = 200_000;
        truncate_payload_to_limit(&mut req, &PayloadLimitConfig { max_bytes: cap });
        assert!(payload_byte_size(&req) <= cap, "裁剪后必须 ≤ 上限");
        // priming 保留在最前。
        let h = &req.conversation_state.history;
        assert!(matches!(&h[0], Message::User(_)));
        assert!(
            matches!(&h[1], Message::Assistant(a) if a.assistant_response_message.content == PRIMING_ASSISTANT_MARKER),
            "priming 配对必须保留"
        );
        // 占位说明已插入（有更旧轮次被丢）。
        assert!(
            h.iter().any(|m| matches!(m, Message::User(u) if u.user_input_message.content == TRUNCATION_PLACEHOLDER)),
            "应插入截断占位"
        );
        // 当前消息不被丢。
        assert_eq!(
            req.conversation_state.current_message.user_input_message.content,
            "current question"
        );
    }

    #[test]
    fn huge_current_message_hard_truncated() {
        // 无历史，仅当前消息就超限 → 兜底硬截当前消息。
        let mut req = req_with(vec![], &"z".repeat(900_000));
        let cap = 300_000;
        truncate_payload_to_limit(&mut req, &PayloadLimitConfig { max_bytes: cap });
        assert!(payload_byte_size(&req) <= cap, "兜底后必须 ≤ 上限");
    }

    #[test]
    fn tail_does_not_start_with_orphan_assistant() {
        let mut history = priming();
        for i in 0..50 {
            history.push(user(&format!("u{i} {}", "a".repeat(9_000))));
            history.push(assistant(&format!("a{i} {}", "b".repeat(9_000))));
        }
        let mut req = req_with(history, "q");
        truncate_payload_to_limit(&mut req, &PayloadLimitConfig { max_bytes: 150_000 });
        // priming 之后、占位之后的第一条非占位 tail 不应是 assistant。
        let h = &req.conversation_state.history;
        let first_tail = h.iter().position(|m| {
            matches!(m, Message::User(u) if u.user_input_message.content == TRUNCATION_PLACEHOLDER)
        });
        if let Some(idx) = first_tail {
            if let Some(next) = h.get(idx + 1) {
                assert!(!is_assistant(next), "tail 不应以孤立 assistant 开头");
            }
        }
    }
}
