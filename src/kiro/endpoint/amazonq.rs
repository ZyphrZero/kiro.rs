//! Kiro AmazonQ 端点（移植自 Kiro-Go 的 "AmazonQ" route）
//!
//! 与 `ide`/`codewhisperer` 同为 KiroIDE 风格流式端点，使用相同的
//! path (`/generateAssistantResponse`)、origin (`AI_EDITOR`)、content-type (`application/json`)、
//! profileArn 注入与 KiroIDE 风格 User-Agent。
//!
//! 区别于 `ide`/`codewhisperer` 的地方：
//! - **`x-amz-target`**: `AmazonQDeveloperStreamingService.SendMessage`
//! - **主机族**: 始终 `q.{region}.amazonaws.com`（不走 `codewhisperer` 主机）

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};
use crate::kiro::kiro_version;

pub const AMAZONQ_ENDPOINT_NAME: &str = "amazonq";
const X_AMZ_TARGET: &str = "AmazonQDeveloperStreamingService.SendMessage";

pub struct AmazonqEndpoint;

impl AmazonqEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        if ctx.credentials.is_external_idp() {
            ctx.credentials.data_plane_region()
        } else {
            ctx.credentials.effective_api_region(ctx.config)
        }
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("q.{}.amazonaws.com", self.api_region(ctx))
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
            kiro_version::effective(&ctx.config.kiro_version),
            ctx.machine_id
        )
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            ctx.config.system_version,
            ctx.config.node_version,
            kiro_version::effective(&ctx.config.kiro_version),
            ctx.machine_id
        )
    }
}

impl Default for AmazonqEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for AmazonqEndpoint {
    fn name(&self) -> &'static str {
        AMAZONQ_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/generateAssistantResponse", self.host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/mcp", self.host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amz-target", X_AMZ_TARGET)
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        } else if ctx.credentials.is_external_idp() {
            req = req.header("TokenType", "EXTERNAL_IDP");
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

        if let Some(arn) = ctx.credentials.effective_profile_arn() {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        } else if ctx.credentials.is_external_idp() {
            req = req.header("TokenType", "EXTERNAL_IDP");
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        inject_profile_arn(body, ctx.credentials.streaming_profile_arn().as_deref())
    }
}

fn inject_profile_arn(request_body: &str, profile_arn: Option<&str>) -> String {
    if let Some(arn) = profile_arn {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body) {
            json["profileArn"] = serde_json::Value::String(arn.to_string());
            if let Ok(body) = serde_json::to_string(&json) {
                return body;
            }
        }
    }
    request_body.to_string()
}

#[cfg(test)]
mod tests {
    use super::{inject_profile_arn, AmazonqEndpoint, KiroEndpoint, RequestContext, X_AMZ_TARGET};
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;
    use serde_json::Value;

    fn external_idp_cred(profile_arn: Option<&str>) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.auth_method = Some("external_idp".to_string());
        c.provider = Some("ExternalIdp".to_string());
        c.profile_arn = profile_arn.map(|s| s.to_string());
        c
    }

    #[test]
    fn test_name() {
        assert_eq!(AmazonqEndpoint::new().name(), "amazonq");
    }

    #[test]
    fn test_us_east_1_q_host() {
        let ep = AmazonqEndpoint::new();
        let cred = external_idp_cred(None);
        let config = Config::default();
        let ctx = RequestContext { credentials: &cred, token: "t", machine_id: "m", config: &config };
        assert_eq!(ep.api_url(&ctx), "https://q.us-east-1.amazonaws.com/generateAssistantResponse");
        assert_eq!(ep.mcp_url(&ctx), "https://q.us-east-1.amazonaws.com/mcp");
        assert_eq!(ep.host(&ctx), "q.us-east-1.amazonaws.com");
    }

    #[test]
    fn test_external_idp_profile_region_q_host() {
        let ep = AmazonqEndpoint::new();
        let cred = external_idp_cred(Some("arn:aws:codewhisperer:eu-central-1:123:profile/REAL"));
        let config = Config::default();
        let ctx = RequestContext { credentials: &cred, token: "t", machine_id: "m", config: &config };
        assert_eq!(ep.api_url(&ctx), "https://q.eu-central-1.amazonaws.com/generateAssistantResponse");
        assert_eq!(ep.host(&ctx), "q.eu-central-1.amazonaws.com");
    }

    #[test]
    fn test_x_amz_target_matches_kiro_go() {
        assert_eq!(X_AMZ_TARGET, "AmazonQDeveloperStreamingService.SendMessage");
    }

    #[test]
    fn test_inject_profile_arn_with_some() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let result = inject_profile_arn(body, Some("arn:test"));
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "arn:test");
    }

    #[test]
    fn test_inject_profile_arn_with_none() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let result = inject_profile_arn(body, None);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
    }
}
