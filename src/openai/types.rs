//! OpenAI `/v1/chat/completions` 协议类型
//!
//! 仅定义请求侧需要主动解构的字段；`messages` / `tools` 内层形态多变（content 可为
//! string 或 part 数组、tool 可为嵌套或扁平形式），故以 `serde_json::Value` 宽松接收，
//! 在 [`super::convert`] 中按需解构，避免脆弱的强类型反序列化。
//! 响应侧类型由 [`super::response`] 直接以 `serde_json::json!` 构造，不在此声明。

use serde::Deserialize;

/// OpenAI Chat Completions 请求体
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionRequest {
    /// 模型名（gpt-* 别名或 claude-* 透传；`-thinking` 后缀开启思考）
    pub model: String,
    /// 对话消息（role + content[+tool_calls/tool_call_id]）
    pub messages: Vec<serde_json::Value>,
    /// 是否流式
    #[serde(default)]
    pub stream: bool,
    /// 最大输出 token；兼容新字段 `max_completion_tokens`
    #[serde(default)]
    pub max_tokens: Option<i32>,
    #[serde(default)]
    pub max_completion_tokens: Option<i32>,
    /// 工具定义（function calling）
    #[serde(default)]
    pub tools: Option<Vec<serde_json::Value>>,
    /// 是否在流式末尾追加 usage（`stream_options.include_usage`）。
    /// 我们恒发送 usage，故此字段仅作兼容解析。
    #[serde(default)]
    #[allow(dead_code)]
    pub stream_options: Option<serde_json::Value>,
}

impl ChatCompletionRequest {
    /// 解析出的最大输出 token（两个字段取其一，缺省 4096）
    pub fn resolved_max_tokens(&self) -> i32 {
        self.max_tokens
            .or(self.max_completion_tokens)
            .unwrap_or(4096)
            .max(1)
    }
}
