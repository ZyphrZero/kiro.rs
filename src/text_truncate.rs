//! Inbound per-field text truncation
//!
//! Truncates oversized text fields **locally on CPU, before the request leaves the converter** so the
//! AWS Q (`q.us-east-1.amazonaws.com`) backend never sees a field large enough to reject with
//! `CONTENT_LENGTH_EXCEEDS_THRESHOLD` (400 Bad Request). Why this step exists:
//!
//! 1. AWS Q enforces a hard **per-field** size limit. A single oversized field — most commonly
//!    `toolResult.content[0].text` (a large file read / command output / pasted blob, ~700 KB is enough)
//!    — trips `CONTENT_LENGTH_EXCEEDS_THRESHOLD` and the whole request 400s. See [`crate::image_resize`]
//!    for the image-field equivalent of the same backend limit.
//! 2. The truncation runs during request conversion, which is **before** the provider acquires any
//!    account concurrency permit. So this guard costs zero concurrency: it neither holds a slot nor
//!    triggers credential failover — it simply shrinks the field so the first attempt succeeds.
//!
//! Design principles (shared contract with the `KIRO_RS_IMAGE_*` family):
//! - Fields already under the cap pass through untouched (no realloc, the owned `String` is returned as-is).
//! - Oversized fields are cut in the **middle**, keeping a head + tail (head carries more signal than the
//!   middle, tail preserves the most recent / closing content) with a visible marker in between.
//! - Truncation is UTF-8 safe: cut points are snapped to char boundaries so we never split a codepoint.
//! - Every truncation emits one `warn!` recording the field label and the before/after byte counts.
//! - Everything is driven by `KIRO_RS_TEXT_*` env vars; disabling it restores the previous behaviour.

use tracing::warn;

/// Default per-field byte cap. Tuned for **maximum fidelity**: it sits just below the documented
/// ~700 KB AWS Q per-field trigger, so truncation effectively never fires on normal traffic and only
/// engages at the very edge where the request would otherwise 400. Lower it (e.g. 500000) if you want a
/// wider safety margin; raise it only if you have confirmed a higher real limit.
const DEFAULT_MAX_FIELD_BYTES: usize = 680_000;
/// Fraction of the budget kept from the head (the rest, minus the marker, is kept from the tail).
const HEAD_RATIO: f64 = 0.7;

/// Inbound text-field truncation configuration
#[derive(Debug, Clone, Copy)]
pub struct TextLimitConfig {
    pub enabled: bool,
    pub max_field_bytes: usize,
}

impl TextLimitConfig {
    /// Reads from `KIRO_RS_TEXT_*` env vars, falling back to defaults when unset.
    ///
    /// - `KIRO_RS_TEXT_TRUNCATE` — `0/false/no/off` disables truncation (default: enabled)
    /// - `KIRO_RS_TEXT_MAX_FIELD_BYTES` — per-field byte cap (default: 680000)
    pub fn from_env() -> Self {
        let enabled = !matches!(
            std::env::var("KIRO_RS_TEXT_TRUNCATE")
                .unwrap_or_else(|_| "1".to_string())
                .to_ascii_lowercase()
                .as_str(),
            "0" | "false" | "no" | "off"
        );
        let max_field_bytes = std::env::var("KIRO_RS_TEXT_MAX_FIELD_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_FIELD_BYTES);
        Self {
            enabled,
            max_field_bytes,
        }
    }
}

/// Largest char-boundary index `<= idx` (never splits a UTF-8 codepoint).
fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Smallest char-boundary index `>= idx` (never splits a UTF-8 codepoint).
fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Truncates one text field to `cfg.max_field_bytes`, keeping head + tail with a marker in between.
///
/// Takes ownership and returns the same `String` untouched when truncation is disabled or the field is
/// already within the cap (no allocation on the fast path). `label` only feeds the warning log so the
/// operator can see which field was cut.
pub fn truncate_field(cfg: &TextLimitConfig, label: &str, text: String) -> String {
    if !cfg.enabled || text.len() <= cfg.max_field_bytes {
        return text;
    }

    let original_bytes = text.len();
    let max = cfg.max_field_bytes;

    // Reserve room for the marker so the final output still fits under `max`.
    let head_budget = (max as f64 * HEAD_RATIO) as usize;
    let head_end = floor_char_boundary(&text, head_budget);

    let removed_estimate = original_bytes.saturating_sub(max);
    let marker = format!("\n\n…[kiro-rs truncated ~{} bytes]…\n\n", removed_estimate);

    // Tail budget is whatever is left of the cap after the head and the marker.
    let tail_budget = max.saturating_sub(head_end).saturating_sub(marker.len());
    let tail_start = ceil_char_boundary(&text, original_bytes.saturating_sub(tail_budget));
    // Guard against the (degenerate) case where tail_start lands before head_end.
    let tail_start = tail_start.max(head_end);

    let mut out = String::with_capacity(head_end + marker.len() + (original_bytes - tail_start));
    out.push_str(&text[..head_end]);
    out.push_str(&marker);
    out.push_str(&text[tail_start..]);

    warn!(
        target: "kiro_rs::text_truncate",
        field = label,
        original_bytes = original_bytes,
        final_bytes = out.len(),
        max_field_bytes = max,
        "text field exceeded per-field cap; truncated middle to avoid CONTENT_LENGTH_EXCEEDS_THRESHOLD"
    );

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max: usize) -> TextLimitConfig {
        TextLimitConfig {
            enabled: true,
            max_field_bytes: max,
        }
    }

    #[test]
    fn passthrough_when_under_cap() {
        let s = "hello world".to_string();
        let out = truncate_field(&cfg(1000), "t", s.clone());
        assert_eq!(out, s);
    }

    #[test]
    fn passthrough_when_disabled() {
        let c = TextLimitConfig {
            enabled: false,
            max_field_bytes: 10,
        };
        let s = "a".repeat(1000);
        assert_eq!(truncate_field(&c, "t", s.clone()), s);
    }

    #[test]
    fn truncates_and_stays_within_cap() {
        let s = "a".repeat(1_000_000);
        let out = truncate_field(&cfg(500_000), "t", s);
        assert!(out.len() <= 500_000, "len was {}", out.len());
        assert!(out.contains("kiro-rs truncated"));
        assert!(out.starts_with('a'));
        assert!(out.ends_with('a'));
    }

    #[test]
    fn never_splits_utf8_codepoint() {
        // Multi-byte chars (3 bytes each) so naive byte slicing would panic.
        let s = "中".repeat(400_000); // ~1.2 MB
        let out = truncate_field(&cfg(300_000), "t", s);
        assert!(out.len() <= 300_000);
        // If any boundary were mis-cut, indexing/printing would have panicked already.
        assert!(out.contains('中'));
    }
}
