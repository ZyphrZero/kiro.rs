//! Kiro CLI 端点（Amazon Q for CLI）
//!
//! 对应 Kiro CLI / Amazon Q for CLI 使用的 AWS JSON 协议端点：
//! - URL: `https://{host}/`（根路径 + x-amz-target 头），`host` 由 `config.apiDomain` 决定
//! - Content-Type: `application/x-amz-json-1.0`
//! - User-Agent: aws-sdk-rust 格式
//! - 请求体 origin: `KIRO_CLI`
//!
//! 适用于使用 `ksk_` 前缀 API Key 的凭据。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};

pub const CLI_ENDPOINT_NAME: &str = "cli";

pub struct CliEndpoint;

impl CliEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        ctx.runtime_host()
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererstreaming/0.1.14474 os/{} lang/rust/1.92.0 md/appVersion-{} app/AmazonQ-For-CLI",
            ctx.config.system_version,
            ctx.config.kiro_version,
        )
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererstreaming/0.1.14474 os/{} lang/rust/1.92.0 m/F app/AmazonQ-For-CLI",
            ctx.config.system_version,
        )
    }
}

impl Default for CliEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for CliEndpoint {
    fn name(&self) -> &'static str {
        CLI_ENDPOINT_NAME
    }

    fn content_type(&self) -> &'static str {
        "application/x-amz-json-1.0"
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/", self.host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/mcp", self.host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header(
                "x-amz-target",
                "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
            )
            .header("x-amzn-codewhisperer-optout", "false")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(token_type) = ctx.credentials.token_type_header() {
            req = req.header("tokentype", token_type);
        }
        req
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        // 上游 MCP 同样强制要求 profileArn（缺失 → 400 profileArn is required），
        // 与对话请求一致：BuilderID 占位符账号也要带上占位符
        if let Some(arn) = ctx.credentials.streaming_profile_arn() {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        if let Some(token_type) = ctx.credentials.token_type_header() {
            req = req.header("tokentype", token_type);
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        // OAuth 凭据走 CLI 协议时上游要求 profileArn；API Key 凭据 streaming_profile_arn() 为 None，不注入
        super::ide::inject_profile_arn(
            &set_origin_kiro_cli(body),
            ctx.credentials.streaming_profile_arn().as_deref(),
        )
    }
}

/// 将请求体转换为 KIRO_CLI 格式：
/// 1. 所有 "AI_EDITOR" origin 替换为 "KIRO_CLI"
/// 2. 移除 conversationState.agentContinuationId（Kiro CLI 不发送此字段）
/// 3. 移除 history 中用户消息的 modelId（Kiro CLI 不在历史消息里发送此字段）
fn set_origin_kiro_cli(body: &str) -> String {
    let body = body.replace("\"origin\":\"AI_EDITOR\"", "\"origin\":\"KIRO_CLI\"");

    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&body) else {
        return body;
    };

    if let Some(state) = json.get_mut("conversationState").and_then(|v| v.as_object_mut()) {
        state.remove("agentContinuationId");

        if let Some(history) = state.get_mut("history").and_then(|v| v.as_array_mut()) {
            for msg in history.iter_mut() {
                if let Some(user_input) = msg
                    .get_mut("userInputMessage")
                    .and_then(|v| v.as_object_mut())
                {
                    user_input.remove("modelId");
                }
            }
        }
    }

    serde_json::to_string(&json).unwrap_or(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_origin_kiro_cli_current_message() {
        let body = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"content":"hi","origin":"AI_EDITOR"}}}}"#;
        let result = set_origin_kiro_cli(body);
        assert!(result.contains("\"origin\":\"KIRO_CLI\""));
        assert!(!result.contains("\"origin\":\"AI_EDITOR\""));
    }

    #[test]
    fn test_set_origin_kiro_cli_history() {
        let body = r#"{"conversationState":{"history":[{"userInputMessage":{"content":"hi","origin":"AI_EDITOR"}},{"userInputMessage":{"content":"hello","origin":"AI_EDITOR"}}],"currentMessage":{"userInputMessage":{"origin":"AI_EDITOR"}}}}"#;
        let result = set_origin_kiro_cli(body);
        assert!(!result.contains("\"origin\":\"AI_EDITOR\""));
        assert_eq!(result.matches("\"origin\":\"KIRO_CLI\"").count(), 3);
    }

    #[test]
    fn test_set_origin_kiro_cli_no_origin() {
        let body = r#"{"conversationState":{}}"#;
        assert_eq!(set_origin_kiro_cli(body), r#"{"conversationState":{}}"#);
    }
}
