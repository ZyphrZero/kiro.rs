//! Kiro 端点抽象
//!
//! 不同 Kiro 端点（如 `ide` / `cli`）在 URL、请求头、请求体上存在差异，
//! 但共享凭据池、Token 刷新、重试逻辑和 AWS event-stream 响应解码。
//!
//! [`KiroEndpoint`] 抽象了请求侧的差异点；`KiroProvider` 持有一个 endpoint 注册表，
//! 按凭据的 `endpoint` 字段选择对应实现。

use reqwest::RequestBuilder;

use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

pub mod amazonq;
pub mod cli;
pub mod codewhisperer;
pub mod ide;

pub use amazonq::AmazonqEndpoint;
pub use cli::CliEndpoint;
pub use codewhisperer::CodewhispererEndpoint;
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
    /// Provider 已经设置好 URL、content-type、Connection 和 body；
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

    /// 判断响应体是否表示"账号级临时风控"（429 + suspicious activity）
    ///
    /// 与普通 429（high traffic）区分：账号级风控只针对当前凭据生效，
    /// 故障转移到其它凭据后可立即恢复；普通 429 是上游全局过载，切换无意义。
    fn is_account_throttled(&self, body: &str) -> bool {
        default_is_account_throttled(body)
    }

    /// 判断响应体是否表示"单账号请求速率超限"（429 + USER_REQUEST_RATE_EXCEEDED）
    ///
    /// 与账号级风控（suspicious activity）区分：这是对**当前账号**的 per-user 速率限制，
    /// 短时间内对同一账号发请求过快即触发。普通的瞬态重试会反复命中同一账号（重试时
    /// 排序按在途数，速率超限的账号未被排除，常被重新选中），白白浪费重试预算与槽位。
    /// 识别后应对该账号施加**短冷却**并立即切换到其它账号，让速率窗口自然恢复。
    fn is_user_rate_limited(&self, body: &str) -> bool {
        default_is_user_rate_limited(body)
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
}

/// 触发"额度耗尽 → 禁用并切换"的 reason 取值集合
///
/// - `MONTHLY_REQUEST_COUNT`: 月度请求额度用尽
/// - `OVERAGE_REQUEST_LIMIT_EXCEEDED`: 超额（overage）额度也耗尽
///
/// 两类语义都是「该凭据当前计费周期内不能再用」，处理方式一致：
/// 立刻禁用凭据并故障转移到下一个可用凭据。
const QUOTA_EXHAUSTED_REASONS: &[&str] =
    &["MONTHLY_REQUEST_COUNT", "OVERAGE_REQUEST_LIMIT_EXCEEDED"];

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

/// 默认的账号级风控判断逻辑
///
/// 上游 Kiro/Q-Developer 风控有两种已知措辞，都属于"账号级临时限制、会自动恢复"：
///
/// 1. **429 限速型**：
///    `Due to suspicious activity, we are imposing temporary limits on how
///    frequently your account (d-...) can send a request to Kiro while we investigate.`
///    （"suspicious activity" + "temporary limits"）
///
/// 2. **403 临时封锁型**：
///    `Your user ID is temporarily suspended. We detected unusual user activity
///    and locked it as a security precaution. To restore...`
///    （"temporarily suspended" + "unusual user activity"）
///
/// 两者都是**临时**的（注意 temporary / temporarily），应按长冷却处理、到期自动回池，
/// 而非计入连续失败被自愈反复复活、持续捅一个正在被风控的账号。
pub fn default_is_account_throttled(body: &str) -> bool {
    (body.contains("suspicious activity") && body.contains("temporary limits"))
        || (body.contains("temporarily suspended") && body.contains("unusual user activity"))
}

/// 默认的"单账号请求速率超限"判断逻辑（429 + USER_REQUEST_RATE_EXCEEDED）。
///
/// 上游对 per-user 速率限制返回的 reason 为 `USER_REQUEST_RATE_EXCEEDED`。
/// 用精确 reason 子串匹配即可（该取值不会出现在正常响应里）。
pub fn default_is_user_rate_limited(body: &str) -> bool {
    body.contains("USER_REQUEST_RATE_EXCEEDED")
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
const CLIENT_VALIDATION_REASONS: &[&str] = &["TOOL_USE_RESULT_MISMATCH"];

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
        let body = r#"{"message":"some text MONTHLY_REQUEST_COUNT-like phrase","reason":"OTHER"}"#;
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
    fn test_default_is_account_throttled() {
        let body = r#"{"message":"Due to suspicious activity, we are imposing temporary limits on how frequently your account (d-9067c98495.84f894a8) can send a request to Kiro while we investigate.","reason":null}"#;
        assert!(default_is_account_throttled(body));
        // 普通 429 不应被识别为账号风控
        assert!(!default_is_account_throttled(
            "{\"message\":\"Too many requests\"}"
        ));
        // 仅有一半关键词时也不命中
        assert!(!default_is_account_throttled(
            "suspicious activity detected"
        ));
        // 403 临时封锁型措辞应被识别为账号级风控（会自动恢复，走长冷却而非禁用）
        let suspended = r#"{"message":"Your user ID is temporarily suspended. We detected unusual user activity and locked it as a security precaution. To restore access, please wait."}"#;
        assert!(default_is_account_throttled(suspended));
        // 仅半边措辞不命中
        assert!(!default_is_account_throttled("temporarily suspended"));
        assert!(!default_is_account_throttled("unusual user activity"));
    }

    #[test]
    fn test_default_is_user_rate_limited() {
        let body = r#"{"message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#;
        assert!(default_is_user_rate_limited(body));
        // 账号级风控不应被识别为速率超限
        assert!(!default_is_user_rate_limited(
            r#"{"message":"suspicious activity ... temporary limits"}"#
        ));
        // 普通 429 也不命中
        assert!(!default_is_user_rate_limited(
            r#"{"message":"Too many requests"}"#
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
        // message 级特异短语（纯文本，无结构化 reason）
        assert!(default_is_client_validation_error(
            "Expected toolResult blocks but found none"
        ));

        // 普通上游错误不应被误判（否则会跳过应有的重试）
        assert!(!default_is_client_validation_error(
            r#"{"message":"Internal server error"}"#
        ));
        assert!(!default_is_client_validation_error(
            "connection reset by peer"
        ));
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
