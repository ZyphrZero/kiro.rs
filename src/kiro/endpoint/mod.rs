//! Kiro 端点抽象
//!
//! 不同 Kiro 端点（如 `ide` / `cli`）在 URL、请求头、请求体上存在差异，
//! 但共享凭据池、Token 刷新、重试逻辑和 AWS event-stream 响应解码。
//!
//! [`KiroEndpoint`] 抽象了请求侧的差异点；`KiroProvider` 持有一个 endpoint 注册表，
//! 按凭据的 `endpoint` 字段选择对应实现。

use reqwest::RequestBuilder;

use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::{ApiDomain, Config};

pub mod cli;
pub mod ide;

pub use cli::CliEndpoint;
pub use ide::IdeEndpoint;

/// Kiro 端点
///
/// 同一个 `KiroProvider` 可持有多个 endpoint 实现，按凭据级字段切换。
pub trait KiroEndpoint: Send + Sync {
    /// 端点名称（对应 credentials.endpoint / config.defaultEndpoint 的取值）
    fn name(&self) -> &'static str;

    /// API 请求的 Content-Type（默认 application/json）
    fn content_type(&self) -> &'static str {
        "application/json"
    }

    /// API endpoint URL
    fn api_url(&self, ctx: &RequestContext<'_>) -> String;

    /// MCP endpoint URL
    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String;

    /// 装饰 API 请求的端点特有 header
    ///
    /// Provider 已经设置好 URL、content-type 和 body；
    /// 实现负责追加 Authorization、host、user-agent 等端点相关头。
    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder;

    /// 装饰 MCP 请求的端点特有 header
    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder;

    /// 对已序列化的 API 请求体做端点特有加工（如注入 profileArn）
    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String;

    /// 对已序列化的 MCP 请求体做端点特有加工（默认不变）
    fn transform_mcp_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
        body.to_string()
    }

    /// 判断响应体是否表示"月度配额用尽"（禁用凭据并转移）
    fn is_monthly_request_limit(&self, body: &str) -> bool {
        default_is_monthly_request_limit(body)
    }

    /// 判断响应体是否表示"上游 bearer token 失效"（触发强制刷新）
    fn is_bearer_token_invalid(&self, body: &str) -> bool {
        default_is_bearer_token_invalid(body)
    }

    /// 判断响应体是否表示账号级 429 限流。
    ///
    /// 账号级限流可通过冷却当前凭据并故障转移恢复；模型容量不足等普通 429
    /// 与凭据无关，应留在当前凭据上退避重试。
    fn is_account_rate_limited(&self, body: &str) -> bool {
        default_is_account_rate_limited(body)
    }

    /// 判断响应体是否表示"客户端请求格式错误"（messages 数组本身违反协议）
    ///
    /// 这类错误（tool_use↔tool_result 不配对、消息序列非法等）的根因是调用方的
    /// 请求体，而非上游故障。无论上游以 4xx 还是 5xx 返回，重试都不可能成功；
    /// 尤其当上游以 5xx 返回时，若按瞬态错误重试，会把一个永不可能成功的坏请求
    /// 放大成多次 503（503 风暴）并无谓占用重试预算。识别后应立即终止，
    /// 不重试、不切换凭据。
    fn is_client_validation_error(&self, body: &str) -> bool {
        default_is_client_validation_error(body)
    }

    /// 判断响应体是否表示上游网关超时。
    ///
    /// 524 通常来自 Cloudflare/边缘层，继续在同一次客户端调用里重试会把等待时间
    /// 放大到客户端自己的重试上限；让调用方快速失败更利于下一次请求重新建连。
    fn is_gateway_timeout(&self, body: &str) -> bool {
        default_is_gateway_timeout(body)
    }

    /// 判断响应体是否表示"账号被封禁/停用"（403 + 明确封禁文案）。
    ///
    /// 与普通 403（权限/WAF/区域抖动）区分：账号封禁是不可自动恢复的终态，
    /// 需人工联系客服核实。识别后立即禁用该凭据且**不参与自愈**，避免死循环。
    fn is_account_suspended(&self, body: &str) -> bool {
        default_is_account_suspended(body)
    }
}

/// 装饰请求时可用的上下文
///
/// 包含单次调用已确定的所有运行时信息。引用形式避免无谓 clone。
pub struct RequestContext<'a> {
    /// 当前凭据
    pub credentials: &'a KiroCredentials,
    /// 有效的 access token（API Key 凭据下即 kiroApiKey）
    pub token: &'a str,
    /// 当前凭据对应的 machineId
    pub machine_id: &'a str,
    /// 全局配置
    pub config: &'a Config,
    /// 对话 / MCP 上游域名（运行时值，Admin 可热修改，不读 `config.api_domain`）
    pub api_domain: ApiDomain,
}

impl RequestContext<'_> {
    /// 对话 / MCP 请求的上游 host
    pub fn runtime_host(&self) -> String {
        self.api_domain
            .runtime_host(self.credentials.effective_api_region(self.config))
    }
}

/// 触发"额度耗尽 → 禁用并切换"的 reason 取值集合
///
/// - `MONTHLY_REQUEST_COUNT`: 月度请求额度用尽
/// - `OVERAGE_REQUEST_LIMIT_EXCEEDED`: 超额（overage）额度也耗尽
///
/// 两类语义都是「该凭据当前计费周期内不能再用」，处理方式一致：
/// 立刻禁用凭据并故障转移到下一个可用凭据。
const QUOTA_EXHAUSTED_REASONS: &[&str] = &[
    "MONTHLY_REQUEST_COUNT",
    "OVERAGE_REQUEST_LIMIT_EXCEEDED",
];

/// 默认的"请求额度耗尽"判断逻辑
///
/// 同时识别顶层 `reason` 字段和嵌套 `error.reason` 字段。
/// 任一已知额度耗尽 reason 命中即返回 true。
pub fn default_is_monthly_request_limit(body: &str) -> bool {
    // 先快速字符串扫描，避免对 99% 不命中的响应体做 JSON 解析
    if QUOTA_EXHAUSTED_REASONS.iter().any(|r| body.contains(r)) {
        // 进一步用 JSON 解析确认 reason 字段而非偶然出现的子串
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
            let top = value.get("reason").and_then(|v| v.as_str());
            let nested = value.pointer("/error/reason").and_then(|v| v.as_str());
            return [top, nested]
                .into_iter()
                .flatten()
                .any(|r| QUOTA_EXHAUSTED_REASONS.contains(&r));
        }
        // body 是非 JSON 但包含关键词（兼容简单文本响应）
        return true;
    }
    false
}

/// 默认的 bearer token 失效判断逻辑
pub fn default_is_bearer_token_invalid(body: &str) -> bool {
    body.contains("The bearer token included in the request is invalid")
}

/// 默认的账号级 429 限流判断逻辑。
///
/// 优先按上游 JSON 的 `reason` 分类；老版本无 reason 的响应再按高特异文案兼容。
/// `INSUFFICIENT_MODEL_CAPACITY` 等容量类错误不会命中。
pub fn default_is_account_rate_limited(body: &str) -> bool {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let reason = parsed
        .as_ref()
        .and_then(|value| value.get("reason"))
        .and_then(|value| value.as_str());
    if reason.is_some_and(|reason| {
        matches!(
            reason,
            "USER_REQUEST_RATE_EXCEEDED"
                | "CREDIT_CONSUMPTION_RATE_EXCEEDED"
                | "SERVICE_REQUEST_RATE_EXCEEDED"
        )
    }) {
        return true;
    }

    let message = parsed
        .as_ref()
        .and_then(|value| value.get("message"))
        .and_then(|value| value.as_str())
        .unwrap_or(body)
        .trim();
    let message_lower = message.to_ascii_lowercase();

    (message_lower.contains("suspicious activity")
        && message_lower.contains("temporary limits"))
        || message_lower.contains("5-minute credit limit exceeded")
        || message_lower.trim_end_matches('.')
            == "too many requests, please wait before trying again"
}

/// 默认的"账号被封禁/停用"判断逻辑
///
/// 上游对被封账号返回 403 + 类似：
/// `Your User ID (...) temporarily is suspended. We've locked your account as a
/// security precaution. To restore access, please contact our support team ...`
///
/// 与普通 403（权限不足 / WAF / 区域抖动）的关键差异：同时出现 "suspended" 与
/// "locked your account" 两个高特异短语。大小写不敏感匹配，兼容文案微调。
/// 两个短语都命中才判定，避免把偶发 403 误判为封禁。
pub fn default_is_account_suspended(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("suspended") && lower.contains("locked your account")
}

/// 默认的上游网关超时判断逻辑。
pub fn default_is_gateway_timeout(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    body.contains("524")
        && (lower.contains("status code")
            || lower.contains("gateway timeout")
            || lower.contains("server-side issue"))
}

/// 触发"客户端请求格式错误 → 立即终止、不重试"的精确 reason 取值集合
///
/// 这些都是上游对 messages 数组本身的协议校验失败（根因在调用方请求体，
/// 而非上游故障）。仅收录**精确 reason 值**，不收录 `ValidationException`
/// 这类宽泛异常类型——后者语义过宽，裸子串匹配会把恰好携带该词的真实上游
/// 瞬态故障误判为"不可重试"，反而杀掉本可重试恢复的请求。
const CLIENT_VALIDATION_REASONS: &[&str] = &["TOOL_USE_RESULT_MISMATCH", "TOOL_SCHEMA_INVALID"];

/// 触发同类判定的 message 级特征短语（用于无结构化 reason、仅文本报文的场景）
///
/// 例如 Bedrock 的 "Expected toolResult blocks ..." 纯文本错误。短语需具备
/// 足够特异性，不会与正常响应内容冲突。
const CLIENT_VALIDATION_MESSAGE_MARKERS: &[&str] = &["Expected toolResult blocks"];

/// 默认的"客户端请求格式错误"判断逻辑
///
/// 与 [`default_is_monthly_request_limit`] 同构：先做廉价子串快扫，命中后再用
/// JSON 解析确认 `reason`（顶层与嵌套 `error.reason`）字段，避免把偶然出现在
/// 普通字段里的关键词误判。结构化确认失败时，回退到 message 级特异短语匹配，
/// 以覆盖非 JSON 的纯文本错误报文。
pub fn default_is_client_validation_error(body: &str) -> bool {
    let reason_hit = CLIENT_VALIDATION_REASONS.iter().any(|r| body.contains(r));
    if reason_hit {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
            let top = value.get("reason").and_then(|v| v.as_str());
            let nested = value.pointer("/error/reason").and_then(|v| v.as_str());
            if [top, nested]
                .into_iter()
                .flatten()
                .any(|r| CLIENT_VALIDATION_REASONS.contains(&r))
            {
                return true;
            }
        } else {
            // 非 JSON 但含精确 reason 关键词（兼容简单文本响应）
            return true;
        }
    }
    // message 级兜底：纯文本错误报文（无结构化 reason）
    CLIENT_VALIDATION_MESSAGE_MARKERS
        .iter()
        .any(|m| body.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_api_region_controls_api_and_mcp_requests() {
        let client = reqwest::Client::new();
        let endpoints: [&dyn KiroEndpoint; 2] = [&IdeEndpoint::new(), &CliEndpoint::new()];
        let cases = [
            (
                r#"{"region":"eu-central-1"}"#,
                None,
                "us-east-1",
                "eu-central-1",
            ),
            (
                r#"{"region":"eu-central-1"}"#,
                Some("us-east-1"),
                "us-west-2",
                "eu-central-1",
            ),
            (
                r#"{"region":"eu-central-1","apiRegion":"us-east-1"}"#,
                Some("eu-central-1"),
                "us-west-2",
                "us-east-1",
            ),
            ("{}", Some("eu-central-1"), "us-east-1", "eu-central-1"),
            ("{}", None, "eu-central-1", "eu-central-1"),
            ("{}", None, "us-east-1", "us-east-1"),
            (
                r#"{"authRegion":"eu-central-1"}"#,
                None,
                "us-east-1",
                "us-east-1",
            ),
            (
                r#"{"region":"eu-central-1","authRegion":"us-east-1"}"#,
                None,
                "us-east-1",
                "eu-central-1",
            ),
        ];

        for (json, global_api_region, global_region, expected_region) in cases {
            let credentials: KiroCredentials = serde_json::from_str(json).unwrap();
            let mut config = Config::default();
            config.region = global_region.to_string();
            config.api_region = global_api_region.map(str::to_string);
            let ctx = RequestContext {
                credentials: &credentials,
                token: "test-token",
                machine_id: "test-machine",
                config: &config,
                api_domain: config.api_domain,
            };
            let host = format!("q.{expected_region}.amazonaws.com");

            for endpoint in endpoints {
                let api = endpoint
                    .decorate_api(client.post(endpoint.api_url(&ctx)), &ctx)
                    .build()
                    .unwrap();
                let api_path = match endpoint.name() {
                    "ide" => "/generateAssistantResponse",
                    "cli" => "/",
                    name => panic!("unexpected endpoint: {name}"),
                };
                assert_eq!(
                    api.url().as_str(),
                    format!("https://{host}{api_path}"),
                    "{} API with credentials {json}",
                    endpoint.name(),
                );
                assert_eq!(api.headers()["host"].to_str().unwrap(), host);

                let mcp = endpoint
                    .decorate_mcp(client.post(endpoint.mcp_url(&ctx)), &ctx)
                    .build()
                    .unwrap();
                assert_eq!(mcp.url().as_str(), format!("https://{host}/mcp"));
                assert_eq!(mcp.headers()["host"].to_str().unwrap(), host);
            }
        }
    }

    #[test]
    fn api_domain_account_matrix_preserves_protocol_and_profile_arn() {
        use crate::kiro::model::credentials::{BUILDER_ID_PROFILE_ARN, SOCIAL_PROFILE_ARN};
        let endpoints: [&dyn KiroEndpoint; 2] = [&IdeEndpoint::new(), &CliEndpoint::new()];
        let client = reqwest::Client::new();
        let real_arn = "arn:aws:codewhisperer:eu-central-1:123456789012:profile/test";
        let cases = [
            (
                serde_json::json!({"authMethod":"idc"}),
                Some(BUILDER_ID_PROFILE_ARN),
                "us-east-1",
            ),
            (
                serde_json::json!({"authMethod":"social"}),
                Some(SOCIAL_PROFILE_ARN),
                "us-east-1",
            ),
            (
                serde_json::json!({"authMethod":"idc","profileArn":real_arn}),
                Some(real_arn),
                "eu-central-1",
            ),
            (
                serde_json::json!({"kiroApiKey":"ksk_test"}),
                None,
                "us-east-1",
            ),
        ];
        for domain in [ApiDomain::Amazonaws, ApiDomain::Kiro] {
            for (value, expected_arn, region) in &cases {
                let credentials: KiroCredentials = serde_json::from_value(value.clone()).unwrap();
                let config = Config::default();
                let ctx = RequestContext {
                    credentials: &credentials,
                    token: "test-token",
                    machine_id: "test-machine",
                    config: &config,
                    api_domain: domain,
                };
                for endpoint in endpoints {
                    let body = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"origin":"AI_EDITOR","content":"hello"}},"agentContinuationId":"test","history":[{"userInputMessage":{"modelId":"test","origin":"AI_EDITOR"}}]}}"#;
                    let transformed: serde_json::Value =
                        serde_json::from_str(&endpoint.transform_api_body(body, &ctx)).unwrap();
                    assert_eq!(
                        transformed["profileArn"].as_str(),
                        *expected_arn,
                        "{} {value}",
                        endpoint.name()
                    );
                    let state = &transformed["conversationState"];
                    let expected_origin = if endpoint.name() == "cli" {
                        "KIRO_CLI"
                    } else {
                        "AI_EDITOR"
                    };
                    assert_eq!(
                        state["currentMessage"]["userInputMessage"]["origin"],
                        expected_origin
                    );
                    if endpoint.name() == "cli" {
                        assert!(state.get("agentContinuationId").is_none());
                        assert!(
                            state["history"][0]["userInputMessage"]
                                .get("modelId")
                                .is_none()
                        );
                    }
                    let host = domain.runtime_host(region);
                    let mcp = endpoint
                        .decorate_mcp(client.post(endpoint.mcp_url(&ctx)), &ctx)
                        .build()
                        .unwrap();
                    assert_eq!(mcp.url().host_str(), Some(host.as_str()));
                    assert_eq!(
                        mcp.headers()
                            .get("x-amzn-kiro-profile-arn")
                            .map(|v| v.to_str().unwrap()),
                        *expected_arn
                    );
                    let api = endpoint
                        .decorate_api(client.post(endpoint.api_url(&ctx)), &ctx)
                        .build()
                        .unwrap();
                    assert_eq!(api.url().host_str(), Some(host.as_str()));
                    assert_eq!(api.headers()["authorization"], "Bearer test-token");
                    assert_eq!(api.headers()["host"], host);
                    if endpoint.name() == "cli" {
                        assert_eq!(api.url().path(), "/");
                        assert_eq!(
                            api.headers()["x-amz-target"],
                            "AmazonCodeWhispererStreamingService.GenerateAssistantResponse"
                        );
                    } else {
                        assert_eq!(api.url().path(), "/generateAssistantResponse");
                    }
                }
            }
        }
    }

    #[test]
    fn test_api_domain_kiro_switches_api_and_mcp_host() {
        let client = reqwest::Client::new();
        let endpoints: [&dyn KiroEndpoint; 2] = [&IdeEndpoint::new(), &CliEndpoint::new()];
        let credentials: KiroCredentials = serde_json::from_str(r#"{"region":"eu-central-1"}"#).unwrap();
        let config: Config = serde_json::from_str(r#"{"apiDomain":"kiro"}"#).unwrap();
        assert_eq!(config.api_domain, crate::model::config::ApiDomain::Kiro);
        let ctx = RequestContext {
            credentials: &credentials,
            token: "test-token",
            machine_id: "test-machine",
            config: &config,
            api_domain: config.api_domain,
        };
        let host = "runtime.eu-central-1.kiro.dev";

        for endpoint in endpoints {
            let api = endpoint
                .decorate_api(client.post(endpoint.api_url(&ctx)), &ctx)
                .build()
                .unwrap();
            assert!(api.url().as_str().starts_with(&format!("https://{host}/")));
            assert_eq!(api.headers()["host"].to_str().unwrap(), host);

            let mcp = endpoint
                .decorate_mcp(client.post(endpoint.mcp_url(&ctx)), &ctx)
                .build()
                .unwrap();
            assert_eq!(mcp.url().as_str(), format!("https://{host}/mcp"));
            assert_eq!(mcp.headers()["host"].to_str().unwrap(), host);
        }
    }

    #[test]
    fn test_api_domain_defaults_to_amazonaws() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.api_domain, crate::model::config::ApiDomain::Amazonaws);
        assert_eq!(
            serde_json::to_value(&config).unwrap()["apiDomain"],
            serde_json::json!("amazonaws")
        );
    }

    #[test]
    fn test_mcp_sends_placeholder_profile_arn_header() {
        let client = reqwest::Client::new();
        let endpoints: [&dyn KiroEndpoint; 2] = [&IdeEndpoint::new(), &CliEndpoint::new()];
        let placeholder = crate::kiro::model::credentials::BUILDER_ID_PROFILE_ARN;
        let config = Config::default();
        let cases = [
            (format!(r#"{{"authMethod":"idc","profileArn":"{placeholder}"}}"#), Some(placeholder)),
            (r#"{"authMethod":"idc"}"#.to_string(), Some(placeholder)),
            (r#"{"kiroApiKey":"k"}"#.to_string(), None),
        ];

        for (json, expected) in &cases {
            let credentials: KiroCredentials = serde_json::from_str(json).unwrap();
            let ctx = RequestContext {
                credentials: &credentials,
                token: "test-token",
                machine_id: "test-machine",
                config: &config,
                api_domain: config.api_domain,
            };
            for endpoint in endpoints {
                let mcp = endpoint
                    .decorate_mcp(client.post(endpoint.mcp_url(&ctx)), &ctx)
                    .build()
                    .unwrap();
                let header = mcp
                    .headers()
                    .get("x-amzn-kiro-profile-arn")
                    .map(|v| v.to_str().unwrap());
                assert_eq!(header, *expected, "{} {json}", endpoint.name());
            }
        }
    }

    #[test]
    fn test_default_monthly_request_limit_detects_reason() {
        let body = r#"{"message":"You have reached the limit.","reason":"MONTHLY_REQUEST_COUNT"}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_monthly_request_limit_nested_reason() {
        let body = r#"{"error":{"reason":"MONTHLY_REQUEST_COUNT"}}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_monthly_request_limit_false() {
        let body = r#"{"message":"nope","reason":"DAILY_REQUEST_COUNT"}"#;
        assert!(!default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_quota_exhausted_overage() {
        let body = r#"{"message":"You have reached the limit for overages.","reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_quota_exhausted_overage_nested() {
        let body = r#"{"error":{"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}}"#;
        assert!(default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_quota_exhausted_substring_does_not_false_match() {
        // 关键字出现在普通字段而非 reason 字段：仍然命中（向后兼容旧行为）
        // 但 reason 字段是其他值时应严格不命中
        let body =
            r#"{"message":"some text MONTHLY_REQUEST_COUNT-like phrase","reason":"OTHER"}"#;
        assert!(!default_is_monthly_request_limit(body));
    }

    #[test]
    fn test_default_bearer_token_invalid() {
        assert!(default_is_bearer_token_invalid(
            "The bearer token included in the request is invalid"
        ));
        assert!(!default_is_bearer_token_invalid("unrelated error"));
    }

    #[test]
    fn test_default_is_account_suspended() {
        let body = r#"{"message":"Your User ID (736048611274) temporarily is suspended. We've locked your account as a security precaution. To restore access, please contact our support team to verify your identity: https://aws.amazon.com/contact-us/","reason":null}"#;
        assert!(default_is_account_suspended(body));

        // 大小写不敏感
        assert!(default_is_account_suspended(
            "Account SUSPENDED. We've LOCKED YOUR ACCOUNT."
        ));

        // 普通 403 权限错误不应命中
        assert!(!default_is_account_suspended(
            r#"{"message":"User is not authorized to perform this action","reason":null}"#
        ));
        // 仅命中一个短语时不判定为封禁
        assert!(!default_is_account_suspended("your account is suspended"));
        assert!(!default_is_account_suspended(
            "we have locked your account temporarily"
        ));
    }

    #[test]
    fn test_default_is_account_rate_limited() {
        let body = r#"{"message":"Due to suspicious activity, we are imposing temporary limits on how frequently your account (d-9067c98495.84f894a8) can send a request to Kiro while we investigate.","reason":null}"#;
        assert!(default_is_account_rate_limited(body));
        assert!(default_is_account_rate_limited(
            r#"{"message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#
        ));
        assert!(default_is_account_rate_limited(
            r#"{"message":"Too many requests, please wait before trying again.","reason":"CREDIT_CONSUMPTION_RATE_EXCEEDED"}"#
        ));
        assert!(default_is_account_rate_limited(
            r#"{"message":"Too many requests","reason":"SERVICE_REQUEST_RATE_EXCEEDED"}"#
        ));
        assert!(default_is_account_rate_limited(
            r#"{"message":"5-minute credit limit exceeded","reason":null}"#
        ));
        assert!(default_is_account_rate_limited(
            r#"{"message":"Too many requests, please wait before trying again.","reason":null}"#
        ));

        // 模型容量类 429 不应触发换号。
        assert!(!default_is_account_rate_limited(
            r#"{"message":"I am experiencing high traffic, please try again shortly.","reason":"INSUFFICIENT_MODEL_CAPACITY"}"#
        ));
        assert!(!default_is_account_rate_limited(
            r#"{"message":"Encountered unexpectedly high load when processing the request, please try again.","reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#
        ));
        assert!(!default_is_account_rate_limited(
            "{\"message\":\"Too many requests\"}"
        ));
        // 仅有一半关键词时也不命中
        assert!(!default_is_account_rate_limited(
            "suspicious activity detected"
        ));
    }

    #[test]
    fn test_default_is_gateway_timeout() {
        assert!(default_is_gateway_timeout(
            "API Error: 524 status code (no body). This is a server-side issue"
        ));
        assert!(default_is_gateway_timeout("524 Gateway Timeout"));
        assert!(!default_is_gateway_timeout(
            r#"{"message":"some unrelated field mentions 524 tokens"}"#
        ));
    }

    #[test]
    fn test_default_is_client_validation_error() {
        // 顶层 reason 命中（结构化确认）
        assert!(default_is_client_validation_error(
            r#"{"reason":"TOOL_USE_RESULT_MISMATCH"}"#
        ));
        // 嵌套 error.reason 命中
        assert!(default_is_client_validation_error(
            r#"{"error":{"reason":"TOOL_USE_RESULT_MISMATCH"}}"#
        ));
        // 非 JSON 但含精确 reason 关键词
        assert!(default_is_client_validation_error(
            "upstream error: TOOL_USE_RESULT_MISMATCH"
        ));
        // TOOL_SCHEMA_INVALID：工具 inputSchema 不合规（如顶层 oneOf / 非 object），
        // 根因在请求体，重试/换号不会好，应立即终止。
        assert!(default_is_client_validation_error(
            r#"{"__type":"ValidationException","message":"input_schema does not support oneOf, allOf, or anyOf at the top level","reason":"TOOL_SCHEMA_INVALID"}"#
        ));
        // message 级特异短语（纯文本，无结构化 reason）
        assert!(default_is_client_validation_error(
            "Expected toolResult blocks but found none"
        ));

        // 普通上游错误不应被误判（否则会跳过应有的重试）
        assert!(!default_is_client_validation_error(
            r#"{"message":"Internal server error"}"#
        ));
        assert!(!default_is_client_validation_error("connection reset by peer"));
        // 关键回归：reason 关键词偶然出现在普通字段，但真实 reason 是别的值 —— 不应命中
        // （否则会把一个本可重试恢复的真实上游故障误杀）
        assert!(!default_is_client_validation_error(
            r#"{"message":"trace mentions TOOL_USE_RESULT_MISMATCH internally","reason":"INTERNAL_SERVER_ERROR"}"#
        ));
        // 宽泛的 ValidationException 不再单独命中（无精确 reason / 无特异短语时）
        assert!(!default_is_client_validation_error(
            r#"{"__type":"ValidationException","message":"some other validation"}"#
        ));
    }
}
