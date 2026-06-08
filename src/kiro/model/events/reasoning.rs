//! 推理内容事件
//!
//! 处理 reasoningContentEvent 类型的事件。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// Kiro 原生 thinking / reasoning 事件。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningContentEvent {
    /// 明文思考内容片段。
    #[serde(default)]
    pub text: Option<String>,
    /// 思考块签名，Anthropic 客户端下一轮会原样回传。
    #[serde(default)]
    pub signature: Option<String>,
    /// 上游返回的加密思考内容。
    #[serde(default)]
    pub redacted_content: Option<String>,
}

impl EventPayload for ReasoningContentEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_signature_payload() {
        let v: ReasoningContentEvent =
            serde_json::from_str(r#"{"text":"abc","signature":"sig"}"#).unwrap();
        assert_eq!(v.text.as_deref(), Some("abc"));
        assert_eq!(v.signature.as_deref(), Some("sig"));
    }

    #[test]
    fn parse_redacted_payload() {
        let v: ReasoningContentEvent =
            serde_json::from_str(r#"{"redactedContent":"encrypted"}"#).unwrap();
        assert_eq!(v.redacted_content.as_deref(), Some("encrypted"));
    }
}
