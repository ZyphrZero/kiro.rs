//! 计费事件
//!
//! Kiro 上游 meteringEvent payload 形如 `{"unit":"credit","unitPlural":"credits","usage":<f64>}`，
//! `usage` 是本次请求消耗的 credit 数。中转层据此累计每个时间窗的 credit 总量，
//! 并把最近一次事件原样透传到 Anthropic / OpenAI 响应的 usage 字段（`credit_usage` /
//! `credit_unit` / `credit_unit_plural`），让客户端拿到与 Kiro 后端口径一致的计费元数据。
//!
//! 上游 **不下发** token / cache 字段（实测确认），所以这里**只**解析 unit / unitPlural /
//! usage，不做任何字段名候选兼容；解析失败直接由 ParseError 上抛。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 计费事件 payload
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteringEvent {
    /// 计费单位，当前固定为 `credit`
    #[serde(default)]
    pub unit: String,
    /// 计费单位复数，当前固定为 `credits`
    #[serde(default)]
    pub unit_plural: String,
    /// 本次请求消耗的 credit 数（与计费单位一致，浮点）
    #[serde(default)]
    pub usage: f64,
}

impl EventPayload for MeteringEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_real_payload_shape() {
        // 来自真实抓包：仅含 unit / unitPlural / usage
        let v: MeteringEvent = serde_json::from_str(
            r#"{"unit":"credit","unitPlural":"credits","usage":0.0169543708291874}"#,
        )
        .unwrap();
        assert_eq!(v.unit, "credit");
        assert_eq!(v.unit_plural, "credits");
        assert!((v.usage - 0.0169543708291874).abs() < 1e-12);
    }

    #[test]
    fn missing_usage_is_zero() {
        let v: MeteringEvent =
            serde_json::from_str(r#"{"unit":"credit"}"#).unwrap();
        assert_eq!(v.unit, "credit");
        assert_eq!(v.unit_plural, "");
        assert_eq!(v.usage, 0.0);
    }

    #[test]
    fn empty_payload_defaults_all_fields() {
        let v: MeteringEvent = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(v.unit, "");
        assert_eq!(v.unit_plural, "");
        assert_eq!(v.usage, 0.0);
    }
}
