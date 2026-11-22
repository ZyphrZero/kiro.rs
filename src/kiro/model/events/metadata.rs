//! 上游元数据事件
//!
//! Kiro 在 `metadataEvent.tokenUsage` 中返回本次模型调用的精确 token 用量。
//! 四个字段是单次调用的最终快照，不是增量事件；调用方应在同一条流内保留最后一份快照。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 单次 Kiro 模型调用的精确 token 用量。
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// 未命中缓存、也未写入缓存的输入 token。
    #[serde(default)]
    pub uncached_input_tokens: i32,
    /// 模型输出 token。
    #[serde(default)]
    pub output_tokens: i32,
    /// 从服务端 prompt cache 读取的输入 token。
    #[serde(default)]
    pub cache_read_input_tokens: i32,
    /// 本次写入服务端 prompt cache 的输入 token。
    #[serde(default)]
    pub cache_write_input_tokens: i32,
}

impl TokenUsage {
    /// 清理不可信上游值，确保所有计数非负。
    pub fn sanitized(self) -> Self {
        Self {
            uncached_input_tokens: self.uncached_input_tokens.max(0),
            output_tokens: self.output_tokens.max(0),
            cache_read_input_tokens: self.cache_read_input_tokens.max(0),
            cache_write_input_tokens: self.cache_write_input_tokens.max(0),
        }
    }

    #[cfg(test)]
    /// OpenAI 口径的总输入 token（缓存读取是其中的子集）。
    pub fn total_input_tokens(self) -> i32 {
        let usage = self.sanitized();
        usage
            .uncached_input_tokens
            .saturating_add(usage.cache_write_input_tokens)
            .saturating_add(usage.cache_read_input_tokens)
    }

    /// 合并多次真实 provider 调用的用量。
    pub fn saturating_add(self, other: Self) -> Self {
        let left = self.sanitized();
        let right = other.sanitized();
        Self {
            uncached_input_tokens: left
                .uncached_input_tokens
                .saturating_add(right.uncached_input_tokens),
            output_tokens: left.output_tokens.saturating_add(right.output_tokens),
            cache_read_input_tokens: left
                .cache_read_input_tokens
                .saturating_add(right.cache_read_input_tokens),
            cache_write_input_tokens: left
                .cache_write_input_tokens
                .saturating_add(right.cache_write_input_tokens),
        }
    }
}

/// 上游内容过滤拦截时 `metadataEvent.stopReason` 的取值。
pub const CONTENT_FILTERED_STOP_REASON: &str = "CONTENT_FILTERED";

/// `metadataEvent.stopDetails.refusal`：上游拒答的分类与说明。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Refusal {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
}

/// `metadataEvent.stopDetails`。
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StopDetails {
    #[serde(default)]
    pub refusal: Option<Refusal>,
}

/// `metadataEvent` payload。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    /// 有些 metadataEvent 只携带 stopReason，因此 tokenUsage 必须保持可选。
    #[serde(default)]
    pub token_usage: Option<TokenUsage>,
    /// 上游停止原因，如 `CONTENT_FILTERED`。
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_details: Option<StopDetails>,
}

impl MetadataEvent {
    /// 上游内容过滤拦截时返回可读的拒答说明（`category: explanation`），否则 `None`。
    ///
    /// 拦截后上游只下发 reasoning、不下发正文和 meteringEvent；不识别这一信号会把
    /// 拒答伪装成「成功的空响应」，客户端只能盲目重试同一段被拦截的历史。
    #[cfg(test)]
    pub fn content_filter_refusal(&self) -> Option<String> {
        let refusal = self.content_filter_refusal_info()?;
        Some(Self::format_refusal_detail(&refusal))
    }

    /// 返回上游内容过滤拒答的结构化信息（仅保留标准 `category` / `explanation` 字段，
    /// 绝不透传上游私有字段）。非 `CONTENT_FILTERED` 返回 `None`。
    pub fn content_filter_refusal_info(&self) -> Option<Refusal> {
        if self.stop_reason.as_deref() != Some(CONTENT_FILTERED_STOP_REASON) {
            return None;
        }
        Some(
            self.stop_details
                .as_ref()
                .and_then(|d| d.refusal.clone())
                .unwrap_or_default(),
        )
    }

    /// 将 [`Refusal`] 格式化为日志与 trace 使用的可读说明串。
    pub fn format_refusal_detail(refusal: &Refusal) -> String {
        let category = refusal.category.as_deref().filter(|s| !s.trim().is_empty());
        let explanation = refusal
            .explanation
            .as_deref()
            .filter(|s| !s.trim().is_empty());
        match (category, explanation) {
            (Some(c), Some(e)) => format!("{c}: {e}"),
            (Some(c), None) => c.to_string(),
            (None, Some(e)) => e.to_string(),
            (None, None) => CONTENT_FILTERED_STOP_REASON.to_string(),
        }
    }

    /// 构造符合 Anthropic Messages API 规范的 `stop_details` 对象。
    /// 仅当上游返回了标准 `category` 或 `explanation` 时返回 `Some`，避免泄露私有字段。
    pub fn anthropic_stop_details(refusal: &Refusal) -> Option<serde_json::Value> {
        let category = refusal.category.as_deref().filter(|s| !s.trim().is_empty());
        let explanation = refusal
            .explanation
            .as_deref()
            .filter(|s| !s.trim().is_empty());
        if category.is_none() && explanation.is_none() {
            return None;
        }
        let mut obj = serde_json::Map::new();
        obj.insert("type".to_string(), serde_json::json!("refusal"));
        if let Some(c) = category {
            obj.insert(
                "category".to_string(),
                serde_json::json!(c.to_ascii_lowercase()),
            );
        }
        if let Some(e) = explanation {
            obj.insert("explanation".to_string(), serde_json::json!(e));
        }
        Some(serde_json::Value::Object(obj))
    }

    /// 上游 `stopReason` 中表示生成被截断的部分，映射到 Anthropic `stop_reason`。
    /// `END_TURN` / `TOOL_USE` 返回 `None`：是否为 `tool_use` 以网关实际产出的
    /// tool_use 块为准（工具调用可能被过滤，也可能在流末才从文本中回收）。
    /// CONTENT_FILTERED 由 `content_filter_refusal` 单独处理。
    pub fn truncation_stop_reason(&self) -> Option<&'static str> {
        match self.stop_reason.as_deref()?.to_ascii_lowercase().as_str() {
            "max_tokens" => Some("max_tokens"),
            "stop_sequence" => Some("stop_sequence"),
            "model_context_window_exceeded" => Some("model_context_window_exceeded"),
            _ => None,
        }
    }
}

impl EventPayload for MetadataEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_stop_reason_without_token_usage() {
        for (raw, mapped) in [
            ("END_TURN", None),
            ("TOOL_USE", None),
            ("MAX_TOKENS", Some("max_tokens")),
            ("STOP_SEQUENCE", Some("stop_sequence")),
            ("MODEL_CONTEXT_WINDOW_EXCEEDED", Some("model_context_window_exceeded")),
        ] {
            let event: MetadataEvent =
                serde_json::from_value(serde_json::json!({"stopReason":raw})).unwrap();
            assert_eq!(event.truncation_stop_reason(), mapped, "{raw}");
            assert!(event.content_filter_refusal().is_none());
            assert!(event.token_usage.is_none());
        }
        let filtered: MetadataEvent = serde_json::from_value(serde_json::json!({"stopReason":"CONTENT_FILTERED","stopDetails":{"private_provider_detail":"not forwarded"}})).unwrap();
        assert_eq!(
            filtered.content_filter_refusal().as_deref(),
            Some(CONTENT_FILTERED_STOP_REASON)
        );
        assert_eq!(filtered.truncation_stop_reason(), None);
    }

    #[test]
    fn parses_official_token_usage_shape() {
        let event: MetadataEvent = serde_json::from_str(
            r#"{
                "tokenUsage": {
                    "uncachedInputTokens": 101,
                    "outputTokens": 23,
                    "cacheReadInputTokens": 300,
                    "cacheWriteInputTokens": 40
                },
                "stopReason": "end_turn"
            }"#,
        )
        .unwrap();

        let usage = event.token_usage.unwrap();
        assert_eq!(usage.uncached_input_tokens, 101);
        assert_eq!(usage.output_tokens, 23);
        assert_eq!(usage.cache_read_input_tokens, 300);
        assert_eq!(usage.cache_write_input_tokens, 40);
        assert_eq!(usage.total_input_tokens(), 441);
    }

    #[test]
    fn metadata_without_token_usage_is_not_treated_as_zero_truth() {
        let event: MetadataEvent = serde_json::from_str(r#"{"stopReason":"end_turn"}"#).unwrap();
        assert!(event.token_usage.is_none());
        assert_eq!(event.content_filter_refusal(), None);
    }

    #[test]
    fn content_filtered_stop_reason_is_a_refusal_with_upstream_explanation() {
        let event: MetadataEvent = serde_json::from_str(
            r#"{"stopReason":"CONTENT_FILTERED","stopDetails":{"refusal":{"category":"CYBER","explanation":"cannot continue"}}}"#,
        )
        .unwrap();
        assert_eq!(
            event.content_filter_refusal().as_deref(),
            Some("CYBER: cannot continue")
        );
        let refusal = event.content_filter_refusal_info().unwrap();
        assert_eq!(
            MetadataEvent::anthropic_stop_details(&refusal),
            Some(serde_json::json!({
                "type": "refusal",
                "category": "cyber",
                "explanation": "cannot continue"
            }))
        );
    }

    #[test]
    fn anthropic_stop_details_omits_bare_and_private_fields() {
        let bare: MetadataEvent = serde_json::from_str(
            r#"{"stopReason":"CONTENT_FILTERED","stopDetails":{"privateDetail":"secret"}}"#,
        )
        .unwrap();
        let refusal = bare.content_filter_refusal_info().unwrap();
        assert_eq!(MetadataEvent::anthropic_stop_details(&refusal), None);

        let category_only: MetadataEvent = serde_json::from_str(
            r#"{"stopReason":"CONTENT_FILTERED","stopDetails":{"refusal":{"category":"BIO"}}}"#,
        )
        .unwrap();
        let refusal = category_only.content_filter_refusal_info().unwrap();
        assert_eq!(
            MetadataEvent::anthropic_stop_details(&refusal),
            Some(serde_json::json!({
                "type": "refusal",
                "category": "bio"
            }))
        );
    }

    #[test]
    fn token_usage_with_missing_fields_defaults_only_missing_fields_to_zero() {
        let event: MetadataEvent =
            serde_json::from_str(r#"{"tokenUsage":{"outputTokens":9}}"#).unwrap();

        assert_eq!(
            event.token_usage,
            Some(TokenUsage {
                uncached_input_tokens: 0,
                output_tokens: 9,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
            })
        );
    }

    #[test]
    fn sanitizes_negative_values() {
        let usage = TokenUsage {
            uncached_input_tokens: -1,
            output_tokens: -2,
            cache_read_input_tokens: -3,
            cache_write_input_tokens: -4,
        }
        .sanitized();

        assert_eq!(usage, TokenUsage::default());
    }

    #[test]
    fn adds_multiple_provider_calls_without_overflowing() {
        let first = TokenUsage {
            uncached_input_tokens: i32::MAX,
            output_tokens: 3,
            cache_read_input_tokens: 20,
            cache_write_input_tokens: 4,
        };
        let second = TokenUsage {
            uncached_input_tokens: 7,
            output_tokens: 5,
            cache_read_input_tokens: 11,
            cache_write_input_tokens: 2,
        };

        assert_eq!(
            first.saturating_add(second),
            TokenUsage {
                uncached_input_tokens: i32::MAX,
                output_tokens: 8,
                cache_read_input_tokens: 31,
                cache_write_input_tokens: 6,
            }
        );
    }
}
