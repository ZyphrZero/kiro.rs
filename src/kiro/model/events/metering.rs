//! 计费事件
//!
//! Kiro 上游 meteringEvent payload 形如 `{"unit":"credit","unitPlural":"credits","usage":<f64>}`，
//! `usage` 是本次请求消耗的 credit 数。中转层据此累计每个时间窗的 credit 总量。
//!
//! 上游 **不下发** token / cache 字段（实测确认），所以这里**只**解析 `usage`，
//! 不做任何字段名候选兼容；解析失败直接由 ParseError 上抛。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 计费事件 payload
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteringEvent {
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
        assert!((v.usage - 0.0169543708291874).abs() < 1e-12);
    }

    #[test]
    fn missing_usage_is_zero() {
        let v: MeteringEvent = serde_json::from_str(r#"{"unit":"credit"}"#).unwrap();
        assert_eq!(v.usage, 0.0);
    }
}
