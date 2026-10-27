//! Anthropic API 类型定义

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// === 错误响应 ===

/// API 错误响应
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

/// 错误详情
#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

impl ErrorResponse {
    /// 创建新的错误响应
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ErrorDetail {
                error_type: error_type.into(),
                message: message.into(),
            },
        }
    }

    /// 创建认证错误响应
    pub fn authentication_error() -> Self {
        Self::new("authentication_error", "Invalid API key")
    }
}

// === Models 端点类型 ===

/// 模型信息
#[derive(Debug, Clone, Serialize)]
pub struct Model {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,
    pub display_name: String,
    #[serde(rename = "type")]
    pub model_type: String,
    /// 上下文窗口（输入 token 上限），对应上游 `maxInputTokens`。
    pub context_window: i32,
    pub max_tokens: i32,
}

/// 模型列表响应
#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<Model>,
}

// === Messages 端点类型 ===

/// 最大思考预算 tokens
const MAX_BUDGET_TOKENS: i32 = 24576;

/// Thinking 配置
#[derive(Debug, Deserialize, Clone)]
pub struct Thinking {
    #[serde(rename = "type")]
    pub thinking_type: String,
    #[serde(
        default = "default_budget_tokens",
        deserialize_with = "deserialize_budget_tokens"
    )]
    pub budget_tokens: i32,
}

impl Thinking {
    /// 是否启用了 thinking（enabled 或 adaptive）
    pub fn is_enabled(&self) -> bool {
        self.thinking_type == "enabled" || self.thinking_type == "adaptive"
    }
}

fn default_budget_tokens() -> i32 {
    20000
}
fn deserialize_budget_tokens<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = i32::deserialize(deserializer)?;
    Ok(value.min(MAX_BUDGET_TOKENS))
}

/// OutputConfig 配置
#[derive(Debug, Clone)]
pub struct OutputConfig {
    /// 思考强度。未给出 `effort` 时恒为 `"high"`（与历史行为一致）：
    /// 只要请求带了 `output_config`，无论是否只带 `format`，都按 `effort: "high"` 处理。
    pub effort: String,
    /// `output_config.format`（结构化输出）。Kiro 上游没有对应字段，不会透传。
    pub format: Option<OutputFormat>,
}

/// `output_config.format`，目前只关心 `{"type":"json_schema","schema":{...}}`。
#[derive(Debug, Clone, Deserialize)]
pub struct OutputFormat {
    #[serde(rename = "type", default)]
    pub format_type: String,
    #[serde(default)]
    pub schema: Option<serde_json::Value>,
}

impl OutputFormat {
    /// 仅当 `type == "json_schema"` 且 `schema` 是对象时返回 schema。
    pub fn json_schema(&self) -> Option<&serde_json::Value> {
        (self.format_type == "json_schema")
            .then_some(self.schema.as_ref())
            .flatten()
            .filter(|schema| schema.is_object())
    }
}

impl OutputConfig {
    /// 请求携带的 JSON Schema 结构化输出约束。
    pub fn json_schema(&self) -> Option<&serde_json::Value> {
        self.format.as_ref().and_then(OutputFormat::json_schema)
    }
}

impl<'de> Deserialize<'de> for OutputConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            effort: Option<String>,
            // 形状不对的 format 视为未提供，而不是让整个请求 400。
            #[serde(default)]
            format: Option<serde_json::Value>,
        }
        let mut raw = Raw::deserialize(deserializer)?;
        let format = raw
            .format
            .take()
            .and_then(|value| serde_json::from_value::<OutputFormat>(value).ok());
        // 与 master 的 `#[serde(default = "default_effort")]` 等价：缺省 effort 恒为 "high",
        // 只带 `format` 的 output_config 也不会退化成空串（否则会改变 reasoning 行为）。
        let effort = raw.effort.unwrap_or_else(default_effort);
        Ok(Self { effort, format })
    }
}

fn default_effort() -> String {
    "high".to_string()
}

/// Claude Code 请求中的 metadata
#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    /// 用户 ID，格式如: user_xxx_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705
    pub user_id: Option<String>,
}

/// Messages 请求体
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MessagesRequest {
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: i32,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, deserialize_with = "deserialize_system")]
    pub system: Option<Vec<SystemMessage>>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<serde_json::Value>,
    pub thinking: Option<Thinking>,
    pub output_config: Option<OutputConfig>,
    /// Claude Code 请求中的 metadata，包含 session 信息
    pub metadata: Option<Metadata>,
    /// 顶层 cache_control（开启自动提示词缓存）
    #[serde(default)]
    pub cache_control: Option<CacheControl>,
}

fn default_max_tokens() -> i32 {
    32_000
}

/// 反序列化 system 字段，支持字符串或数组格式
fn deserialize_system<'de, D>(deserializer: D) -> Result<Option<Vec<SystemMessage>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // 创建一个 visitor 来处理 string 或 array
    struct SystemVisitor;

    impl<'de> serde::de::Visitor<'de> for SystemVisitor {
        type Value = Option<Vec<SystemMessage>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string or an array of system messages")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Some(vec![SystemMessage {
                text: value.to_string(),
                cache_control: None,
            }]))
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut messages = Vec::new();
            while let Some(msg) = seq.next_element()? {
                messages.push(msg);
            }
            Ok(if messages.is_empty() {
                None
            } else {
                Some(messages)
            })
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            serde::de::Deserialize::deserialize(deserializer)
        }
    }

    deserializer.deserialize_any(SystemVisitor)
}

/// 消息
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub role: String,
    /// 可以是 string 或 ContentBlock 数组
    pub content: serde_json::Value,
}

/// 系统消息
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemMessage {
    pub text: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// cache_control 配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub cache_type: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

/// 工具定义
///
/// 支持两种格式：
/// 1. 普通工具：{ name, description, input_schema }
/// 2. WebSearch 工具：{ type: "web_search_20250305", name: "web_search", max_uses: 8 }
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tool {
    /// 工具类型，如 "web_search_20250305"（可选，仅 WebSearch 工具）
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub tool_type: Option<String>,
    /// 工具名称
    #[serde(default)]
    pub name: String,
    /// 工具描述（普通工具必需，WebSearch 工具可选）
    #[serde(default)]
    pub description: String,
    /// 输入参数 schema（普通工具必需，WebSearch 工具无此字段）
    ///
    /// 使用 `BTreeMap` 而非 `HashMap`：key 按字典序稳定迭代，保证序列化
    /// 输出可复现。这对 prompt cache 至关重要——tool 签名参与缓存前缀
    /// 指纹，若顶层 key 顺序抖动会导致后续 system/messages 断点连锁失效。
    #[serde(default)]
    pub input_schema: BTreeMap<String, serde_json::Value>,
    /// 最大使用次数（仅 WebSearch 工具）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<i32>,
    /// 缓存控制
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// 内容块
#[derive(Debug, Deserialize, Serialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ImageSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// 图片数据源
#[derive(Debug, Deserialize, Serialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

// === Count Tokens 端点类型 ===

/// Token 计数请求
#[derive(Debug, Serialize, Deserialize)]
pub struct CountTokensRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_system"
    )]
    pub system: Option<Vec<SystemMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}

/// Token 计数响应
#[derive(Debug, Serialize, Deserialize)]
pub struct CountTokensResponse {
    pub input_tokens: i32,
}

#[cfg(test)]
mod tests {
    use super::{MessagesRequest, OutputConfig};

    fn request_json(max_tokens: Option<i32>) -> serde_json::Value {
        let mut value = serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [{"role": "user", "content": "hello"}]
        });
        if let Some(max_tokens) = max_tokens {
            value["max_tokens"] = serde_json::json!(max_tokens);
        }
        value
    }

    #[test]
    fn messages_request_defaults_missing_max_tokens() {
        let request: MessagesRequest = serde_json::from_value(request_json(None)).unwrap();
        assert_eq!(request.max_tokens, 32_000);
    }

    #[test]
    fn messages_request_preserves_explicit_max_tokens() {
        let request: MessagesRequest = serde_json::from_value(request_json(Some(4096))).unwrap();
        assert_eq!(request.max_tokens, 4096);
    }

    #[test]
    fn messages_request_deserializes_top_level_cache_control() {
        let json = serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [{"role": "user", "content": "hello"}],
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        });
        let request: MessagesRequest = serde_json::from_value(json).unwrap();
        assert!(request.cache_control.is_some());
        let cc = request.cache_control.unwrap();
        assert_eq!(cc.cache_type, "ephemeral");
        assert_eq!(cc.ttl.as_deref(), Some("1h"));
    }

    fn output_config(value: serde_json::Value) -> OutputConfig {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn output_config_effort_parsing_is_unchanged() {
        assert_eq!(output_config(serde_json::json!({"effort": "max"})).effort, "max");
        assert_eq!(output_config(serde_json::json!({})).effort, "high");
        let effort_only = output_config(serde_json::json!({"effort": "low"}));
        assert!(effort_only.format.is_none());
        assert!(effort_only.json_schema().is_none());
    }

    #[test]
    fn output_config_parses_json_schema_format() {
        let schema = serde_json::json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
        let config = output_config(serde_json::json!({
            "effort": "low",
            "format": {"type": "json_schema", "schema": schema}
        }));
        assert_eq!(config.effort, "low");
        assert_eq!(config.json_schema(), Some(&schema));

        // 仅 format：effort 仍按 master 的默认值走 "high"。
        let format_only = output_config(serde_json::json!({
            "format": {"type": "json_schema", "schema": schema}
        }));
        assert_eq!(format_only.effort, "high");
        assert_eq!(format_only.json_schema(), Some(&schema));
        // 未知字段（如只有 format 的真实请求）同样落到默认 effort。
        assert_eq!(
            output_config(serde_json::json!({"unknown": 1})).effort,
            "high"
        );
    }

    #[test]
    fn output_config_ignores_non_json_schema_formats() {
        for format in [
            serde_json::json!({"type": "text"}),
            serde_json::json!({"type": "json_schema"}),
            serde_json::json!({"type": "json_schema", "schema": "nope"}),
            serde_json::json!({"schema": {"type": "object"}}),
        ] {
            let config = output_config(serde_json::json!({"format": format}));
            assert!(config.json_schema().is_none(), "{format}");
        }
    }

    #[test]
    fn output_config_tolerates_malformed_format() {
        for format in [
            serde_json::json!("json_schema"),
            serde_json::json!({"type": null}),
            serde_json::json!(42),
        ] {
            let config = output_config(serde_json::json!({"effort": "low", "format": format}));
            assert_eq!(config.effort, "low");
            assert!(config.json_schema().is_none());
        }
    }
}
