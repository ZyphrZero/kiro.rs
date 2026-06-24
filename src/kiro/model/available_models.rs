//! 可用模型查询数据模型
//!
//! 包含 ListAvailableModels API 的响应类型定义。
//!
//! 上游接口：`GET https://q.{api_region}.amazonaws.com/ListAvailableModels?origin=AI_EDITOR`
//! 返回该凭据（按订阅等级）当前真实可用的模型列表。

use serde::Deserialize;

/// ListAvailableModels API 响应
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListAvailableModelsResponse {
    /// 可用模型列表
    #[serde(default)]
    pub models: Vec<UpstreamModel>,
}

/// 单个上游模型
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamModel {
    /// 模型 ID（如 "claude-sonnet-4.5"）
    pub model_id: String,

    /// 模型展示名（可能不存在）
    #[serde(default)]
    pub model_name: Option<String>,

    /// 模型描述（可能不存在）
    #[serde(default)]
    pub description: Option<String>,

    /// Token 限额信息（可能不存在）
    #[serde(default)]
    pub token_limits: Option<TokenLimits>,
}

/// 模型 Token 限额
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenLimits {
    /// 最大输入 Token 数
    #[serde(default)]
    pub max_input_tokens: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_full_response() {
        let json = r#"{
            "models": [
                {
                    "modelId": "claude-sonnet-4.5",
                    "modelName": "Claude Sonnet 4.5",
                    "description": "balanced model",
                    "tokenLimits": { "maxInputTokens": 200000 }
                },
                {
                    "modelId": "claude-opus-4.6"
                }
            ]
        }"#;
        let resp: ListAvailableModelsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.models.len(), 2);

        let first = &resp.models[0];
        assert_eq!(first.model_id, "claude-sonnet-4.5");
        assert_eq!(first.model_name.as_deref(), Some("Claude Sonnet 4.5"));
        assert_eq!(
            first.token_limits.as_ref().unwrap().max_input_tokens,
            Some(200000)
        );

        // 仅 modelId 的最小对象：其余字段缺省为 None
        let second = &resp.models[1];
        assert_eq!(second.model_id, "claude-opus-4.6");
        assert!(second.model_name.is_none());
        assert!(second.token_limits.is_none());
    }

    #[test]
    fn test_deserialize_empty_models() {
        let resp: ListAvailableModelsResponse = serde_json::from_str(r#"{"models":[]}"#).unwrap();
        assert!(resp.models.is_empty());
    }

    #[test]
    fn test_deserialize_missing_models_field() {
        // 缺少 models 字段时回退为空数组
        let resp: ListAvailableModelsResponse = serde_json::from_str(r#"{}"#).unwrap();
        assert!(resp.models.is_empty());
    }
}
