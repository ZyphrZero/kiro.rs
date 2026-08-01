//! Token 管理模块
//!
//! 负责 Token 过期检测和刷新，支持 Social 和 IdC 认证方式
//! 支持多凭据 (MultiTokenManager) 管理

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as TokioMutex, Semaphore};

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration as StdDuration, Instant};

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::error::UpstreamRateLimitError;
use crate::kiro::kiro_version::USAGE_API_KIRO_VERSION;
use crate::kiro::machine_id;
use crate::kiro::model::available_models::{ListAvailableModelsResponse, UpstreamModel};
use crate::kiro::model::available_profiles::ListAvailableProfilesResponse;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::token_refresh::{
    ExternalIdpTokenResponse, IdcRefreshRequest, IdcRefreshResponse, RefreshRequest,
    RefreshResponse,
};
use crate::kiro::model::usage_limits::UsageLimitsResponse;
use crate::model::config::Config;

/// 检查 Token 是否在指定时间内过期
pub(crate) fn is_token_expiring_within(
    credentials: &KiroCredentials,
    minutes: i64,
) -> Option<bool> {
    credentials
        .expires_at
        .as_ref()
        .and_then(|expires_at| DateTime::parse_from_rfc3339(expires_at).ok())
        .map(|expires| expires <= Utc::now() + Duration::minutes(minutes))
}

/// 检查 Token 是否已过期（提前 5 分钟判断）
pub(crate) fn is_token_expired(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 5).unwrap_or(true)
}

/// 检查 Token 是否即将过期（10分钟内）
pub(crate) fn is_token_expiring_soon(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 10).unwrap_or(false)
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)
}

/// 生成 API Key 脱敏展示(前 4 + ... + 后 4,长度不足或非 ASCII 回退 ***)
fn mask_api_key(key: &str) -> String {
    if key.is_ascii() && key.len() > 16 {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    } else {
        "***".to_string()
    }
}

/// 验证 refreshToken 的基本有效性
pub(crate) fn validate_refresh_token(credentials: &KiroCredentials) -> anyhow::Result<()> {
    let refresh_token = credentials
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;

    validate_refresh_token_str(refresh_token)
}

/// 验证 refreshToken 字符串本身（调用方已确认 Token 存在）
pub(crate) fn validate_refresh_token_str(refresh_token: &str) -> anyhow::Result<()> {
    if refresh_token.is_empty() {
        bail!("refreshToken 为空");
    }

    if refresh_token.len() < 100 || refresh_token.ends_with("...") || refresh_token.contains("...")
    {
        bail!(
            "refreshToken 已被截断（长度: {} 字符）。\n\
             这通常是 Kiro IDE 为了防止凭证被第三方工具使用而故意截断的。",
            refresh_token.len()
        );
    }

    Ok(())
}

/// Refresh Token 永久失效错误
///
/// 当服务端返回 400 + `invalid_grant` 时，表示 refreshToken 已被撤销或过期，
/// 不应重试，需立即禁用对应凭据。
#[derive(Debug)]
pub(crate) struct RefreshTokenInvalidError {
    pub message: String,
}

impl fmt::Display for RefreshTokenInvalidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RefreshTokenInvalidError {}

/// 刷新 Token
pub(crate) async fn refresh_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    // API Key 凭据不支持 Token 刷新：底层契约级拦截
    // 其他调用点（try_ensure_token / 活跃路径 / add_credential）在调用前已显式分流 API Key；
    // 仅 force_refresh_token_for 未分流，此处 bail 让错误自然传播为 400 BAD_REQUEST。
    if credentials.is_api_key_credential() {
        bail!("API Key 凭据不支持刷新 Token");
    }

    validate_refresh_token(credentials)?;

    // 企业 SSO (external_idp) 走 IdP token 端点刷新（refresh_token grant，public client），
    // 而非 AWS SSO OIDC / Social 端点。必须在下面的 idc/social 自动判断之前分流：
    // external_idp 有 clientId 但无 clientSecret，落到自动判断会被误判为 social。
    if credentials.is_external_idp_credential() {
        return refresh_external_idp_token(credentials, config, proxy).await;
    }

    // 根据 auth_method 选择刷新方式
    // 如果未指定 auth_method，根据是否有 clientId/clientSecret 自动判断
    let auth_method = credentials.auth_method.as_deref().unwrap_or_else(|| {
        if credentials.client_id.is_some() && credentials.client_secret.is_some() {
            "idc"
        } else {
            "social"
        }
    });

    if auth_method.eq_ignore_ascii_case("idc")
        || auth_method.eq_ignore_ascii_case("builder-id")
        || auth_method.eq_ignore_ascii_case("iam")
    {
        refresh_idc_token(credentials, config, proxy).await
    } else {
        refresh_social_token(credentials, config, proxy).await
    }
}

/// 刷新 Social Token
async fn refresh_social_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 Social Token...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);

    let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);
    let refresh_domain = format!("prod.{}.auth.desktop.kiro.dev", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = crate::kiro::kiro_version::effective(&config.kiro_version);

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = RefreshRequest {
        refresh_token: refresh_token.to_string(),
    };

    let response = client
        .post(&refresh_url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            format!("KiroIDE-{}-{}", kiro_version, machine_id),
        )
        .header("Accept-Encoding", "gzip, compress, deflate, br")
        .header("host", &refresh_domain)
        .header("Connection", "close")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    let rate_limit_error =
        (status.as_u16() == 429).then(|| UpstreamRateLimitError::from_headers(response.headers()));
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();

        if let Some(error) = rate_limit_error {
            return Err(error.into());
        }

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("Social refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "OAuth 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OAuth 服务暂时不可用",
            _ => "Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: RefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    Ok(new_credentials)
}

/// 刷新 IdC Token (AWS SSO OIDC)
async fn refresh_idc_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 IdC Token...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    let client_id = credentials
        .client_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientId"))?;
    let client_secret = credentials
        .client_secret
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientSecret"))?;

    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);
    let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    let x_amz_user_agent = "aws-sdk-js/3.980.0 KiroIDE";
    let user_agent = format!(
        "aws-sdk-js/3.980.0 ua/2.1 os/{} lang/js md/nodejs#{} api/sso-oidc#3.980.0 m/E KiroIDE",
        os_name, node_version
    );

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = IdcRefreshRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        refresh_token: refresh_token.to_string(),
        grant_type: "refresh_token".to_string(),
    };

    let response = client
        .post(&refresh_url)
        .header("content-type", "application/json")
        .header("x-amz-user-agent", x_amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=4")
        .header("Connection", "close")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    let rate_limit_error =
        (status.as_u16() == 429).then(|| UpstreamRateLimitError::from_headers(response.headers()));
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();

        if let Some(error) = rate_limit_error {
            return Err(error.into());
        }

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("IdC refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "IdC 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OIDC 服务暂时不可用",
            _ => "IdC Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: IdcRefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    // 同步更新 profile_arn（如果 IdC 响应中包含）
    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    Ok(new_credentials)
}

/// 刷新企业 SSO (external_idp, 如 Azure AD) Token
///
/// 通过 IdP 的 OAuth2 token 端点以 refresh_token grant 刷新（public client，无
/// client_secret）。IdP 不返回 profileArn（由 `list_available_profiles` 用
/// EXTERNAL_IDP token type 另行解析并回填）。
async fn refresh_external_idp_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 External IdP (企业 SSO) Token...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    let client_id = credentials
        .client_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("External IdP 刷新需要 clientId"))?;
    let token_endpoint = credentials
        .token_endpoint
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("External IdP 刷新需要 tokenEndpoint"))?;

    // 纵深防御：外发 refreshToken 前再次校验端点在允许列表内，避免持久化的
    // tokenEndpoint 被带外写入（备份还原 / 外部改文件）后把 refreshToken 送到非法主机。
    crate::kiro::model::credentials::validate_external_idp_endpoint(token_endpoint)
        .map_err(|e| anyhow::anyhow!("External IdP tokenEndpoint 被拒绝: {}", e))?;

    let client = build_client(proxy, 60, config.tls_backend)?;

    // 表单编码 refresh_token grant；scope 中的 offline_access 是拿到（轮换后）
    // refresh_token 的前提。reqwest 的 .form() 会自动设 Content-Type。
    let mut form: Vec<(&str, &str)> = vec![
        ("client_id", client_id),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];
    if let Some(scopes) = credentials.scopes.as_deref().filter(|s| !s.is_empty()) {
        form.push(("scope", scopes));
    }

    let response = client
        .post(token_endpoint)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await?;

    let status = response.status();
    let rate_limit_error =
        (status.as_u16() == 429).then(|| UpstreamRateLimitError::from_headers(response.headers()));
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();

        if let Some(error) = rate_limit_error {
            return Err(error.into());
        }

        // invalid_grant → refreshToken 永久失效（Azure 返回
        // {"error":"invalid_grant","error_description":"..."}）
        if status.as_u16() == 400 && body_text.contains("invalid_grant") {
            return Err(RefreshTokenInvalidError {
                message: format!(
                    "External IdP refreshToken 已失效 (invalid_grant): {}",
                    body_text
                ),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "企业 SSO 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，IdP token 端点暂时不可用",
            _ => "External IdP Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: ExternalIdpTokenResponse = response.json().await?;
    if data.access_token.is_empty() {
        bail!("External IdP Token 刷新失败: 响应缺少 access_token");
    }

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    // 部分 IdP（Azure AD）轮换 refresh_token，部分刷新时不下发；未下发时保留旧的。
    if let Some(new_refresh_token) = data.refresh_token.filter(|t| !t.is_empty()) {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    // 不改动 profile_arn：external_idp 不返回，由 resolve_profile_arn_for 解析回填。
    Ok(new_credentials)
}

/// 官方 Kiro 用量 / 模型 REST 接口（getUsageLimits / ListAvailableModels /
/// setUserPreference）仅在 `us-east-1` 与 `eu-central-1` 两个端点提供服务。
///
/// 依据凭据的 SSO 区域选择主端点，并返回另一个端点作为 403 回退候选：
/// - `eu-central-1` 或任何 `eu-*` 区域 → 主端点 `eu-central-1`
/// - 其余区域 → 主端点 `us-east-1`
///
/// 这样导入的 Enterprise / IAM Identity Center (IdC) 账号即使 SSO 区域不是
/// `us-east-1`，也能命中正确的端点，避免 `403 {"message":"Invalid token"}`。
fn rest_api_region_candidates(sso_region: &str) -> [&'static str; 2] {
    let primary_eu = sso_region == "eu-central-1" || sso_region.starts_with("eu-");
    if primary_eu {
        ["eu-central-1", "us-east-1"]
    } else {
        ["us-east-1", "eu-central-1"]
    }
}

fn profile_arn_query(profile_arn: Option<&str>) -> String {
    match profile_arn {
        Some(arn) => format!("&profileArn={}", urlencoding::encode(arn)),
        None => String::new(),
    }
}

/// 用量类接口的 403 回退候选：每个区域端点先带真实 profileArn 试，再退回不带。
///
/// Enterprise / IdC 账号缺 profileArn 会被上游拒（`403 User is not authorized
/// to make this call.`）；BuilderID 占位符已由 `effective_profile_arn` 过滤，
/// 这类账号只有「不带」一种形态，行为与加此参数前一致。
fn usage_api_attempts<'a>(
    credentials: &'a KiroCredentials,
    candidates: &[&'static str],
) -> Vec<(&'static str, Option<&'a str>)> {
    let mut attempts = Vec::with_capacity(candidates.len() * 2);
    for region in candidates {
        if let Some(arn) = credentials.effective_profile_arn() {
            attempts.push((*region, Some(arn)));
        }
        attempts.push((*region, None));
    }
    attempts
}

fn usage_limits_url(host: &str, profile_arn: Option<&str>) -> String {
    format!(
        "https://{}/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST&isEmailRequired=true{}",
        host,
        profile_arn_query(profile_arn)
    )
}

fn available_models_url(host: &str, profile_arn: Option<&str>) -> String {
    format!(
        "https://{}/ListAvailableModels?origin=AI_EDITOR{}",
        host,
        profile_arn_query(profile_arn)
    )
}

fn set_user_preference_body(
    credentials: &KiroCredentials,
    overage_status: &str,
) -> serde_json::Value {
    if let Some(profile_arn) = credentials.effective_profile_arn() {
        serde_json::json!({
            "overageConfiguration": { "overageStatus": overage_status },
            "profileArn": profile_arn,
        })
    } else {
        serde_json::json!({
            "overageConfiguration": { "overageStatus": overage_status },
        })
    }
}

/// 获取使用额度信息
pub(crate) async fn get_usage_limits(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<UsageLimitsResponse> {
    tracing::debug!("正在获取使用额度信息...");

    // getUsageLimits 仅在 us-east-1 / eu-central-1 提供服务，
    // 依据凭据 SSO 区域选择主端点，403 时回退到另一个端点。
    let sso_region = credentials.effective_auth_region(config);
    let candidates = rest_api_region_candidates(sso_region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    // 用量类接口固定用 USAGE_API_KIRO_VERSION：新版 IDE 会强制要求 profileArn，
    // 对 Enterprise/IdC 账号失败；该版本无需 profileArn。
    let kiro_version = USAGE_API_KIRO_VERSION;
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    // 构建 User-Agent headers
    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
        os_name, node_version, kiro_version, machine_id
    );
    let amz_user_agent = format!("aws-sdk-js/1.0.0 KiroIDE-{}-{}", kiro_version, machine_id);

    let client = build_client(proxy, 60, config.tls_backend)?;

    let attempts = usage_api_attempts(credentials, &candidates);

    let mut last_error: Option<String> = None;
    for (idx, (region, profile_arn)) in attempts.iter().enumerate() {
        let host = format!("q.{}.amazonaws.com", region);
        let url = usage_limits_url(&host, *profile_arn);

        let mut request = client
            .get(&url)
            .header("x-amz-user-agent", &amz_user_agent)
            .header("user-agent", &user_agent)
            .header("host", &host)
            .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .header("Authorization", format!("Bearer {}", token))
            .header("Connection", "close");

        if let Some(token_type) = credentials.token_type_header() {
            request = request.header("tokentype", token_type);
        }

        let response = request.send().await?;

        let status = response.status();
        let rate_limit_error = (status.as_u16() == 429)
            .then(|| UpstreamRateLimitError::from_headers(response.headers()));
        if status.is_success() {
            let data: UsageLimitsResponse = response.json().await?;
            return Ok(data);
        }

        let body_text = response.text().await.unwrap_or_default();
        if let Some(error) = rate_limit_error {
            return Err(error.into());
        }

        // 403 时依次回退：带 profileArn → 不带 → 备用区域端点
        if status.as_u16() == 403 && idx + 1 < attempts.len() {
            tracing::debug!(
                "getUsageLimits 在 {} 返回 403（profileArn={}），尝试下一候选",
                region,
                profile_arn.is_some()
            );
            last_error = Some(format!("{} {}", status, body_text));
            continue;
        }

        let error_msg = match status.as_u16() {
            401 => "认证失败，Token 无效或已过期",
            403 => "权限不足，无法获取使用额度",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "获取使用额度失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    // 所有候选端点均失败（理论上循环内已 return / bail，此处为兜底）
    bail!(
        "权限不足，无法获取使用额度: {}",
        last_error.unwrap_or_else(|| "无可用端点".to_string())
    );
}

/// 获取该凭据当前可用的模型列表
///
/// 上游接口：`GET https://q.{api_region}.amazonaws.com/ListAvailableModels?origin=AI_EDITOR`
/// 返回值随订阅等级不同而不同（如 FREE 账号不含 Opus）。
/// 请求头与构造方式与 [`get_usage_limits`] 完全一致。
pub(crate) async fn get_available_models(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<ListAvailableModelsResponse> {
    tracing::debug!("正在获取可用模型列表...");

    // ListAvailableModels 仅在 us-east-1 / eu-central-1 提供服务，
    // 依据凭据 SSO 区域选择主端点，403 时回退到另一个端点。
    let sso_region = credentials.effective_auth_region(config);
    let candidates = rest_api_region_candidates(sso_region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = USAGE_API_KIRO_VERSION;
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    // 构建 User-Agent headers（与 get_usage_limits 保持一致）
    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
        os_name, node_version, kiro_version, machine_id
    );
    let amz_user_agent = format!("aws-sdk-js/1.0.0 KiroIDE-{}-{}", kiro_version, machine_id);

    let client = build_client(proxy, 60, config.tls_backend)?;

    let attempts = usage_api_attempts(credentials, &candidates);

    let mut last_error: Option<String> = None;
    for (idx, (region, profile_arn)) in attempts.iter().enumerate() {
        let host = format!("q.{}.amazonaws.com", region);
        let url = available_models_url(&host, *profile_arn);

        let mut request = client
            .get(&url)
            .header("x-amz-user-agent", &amz_user_agent)
            .header("user-agent", &user_agent)
            .header("host", &host)
            .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .header("Authorization", format!("Bearer {}", token))
            .header("Connection", "close");

        if let Some(token_type) = credentials.token_type_header() {
            request = request.header("tokentype", token_type);
        }

        let response = request.send().await?;

        let status = response.status();
        let rate_limit_error = (status.as_u16() == 429)
            .then(|| UpstreamRateLimitError::from_headers(response.headers()));
        if status.is_success() {
            let data: ListAvailableModelsResponse = response.json().await?;
            return Ok(data);
        }

        let body_text = response.text().await.unwrap_or_default();
        if let Some(error) = rate_limit_error {
            return Err(error.into());
        }

        // 403 时依次回退：带 profileArn → 不带 → 备用区域端点
        if status.as_u16() == 403 && idx + 1 < attempts.len() {
            tracing::debug!(
                "ListAvailableModels 在 {} 返回 403（profileArn={}），尝试下一候选",
                region,
                profile_arn.is_some()
            );
            last_error = Some(format!("{} {}", status, body_text));
            continue;
        }

        let error_msg = match status.as_u16() {
            401 => "认证失败，Token 无效或已过期",
            403 => "权限不足，无法获取可用模型",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "获取可用模型失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    // 所有候选端点均失败（理论上循环内已 return / bail，此处为兜底）
    bail!(
        "权限不足，无法获取可用模型: {}",
        last_error.unwrap_or_else(|| "无可用端点".to_string())
    );
}

/// 获取该凭据可用的真实 profileArn 列表（`ListAvailableProfiles`）。
///
/// Enterprise / IAM Identity Center (IdC) 账号必须用真实 profileArn 调用流式端点；
/// 该 ARN 既不是 BuilderID 占位符，也不在 OIDC 刷新响应里返回，只能通过本接口获取。
///
/// 上游接口（AWS JSON 1.0，**与用量类的 REST GET 不同**）：
/// `POST https://q.{region}.amazonaws.com/`，请求头
/// `x-amz-target: AmazonCodeWhispererService.ListAvailableProfiles`，
/// `Content-Type: application/x-amz-json-1.0`，Body `{"maxResults":N}`。
///
/// 与 [`get_usage_limits`] 一样仅在 `us-east-1` / `eu-central-1` 提供服务，
/// 依据凭据 SSO 区域选择主端点，主端点未返回 profile 时回退到另一个端点。
pub(crate) async fn list_available_profiles(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<ListAvailableProfilesResponse> {
    tracing::debug!("正在获取可用 profile 列表...");

    let sso_region = credentials.effective_auth_region(config);
    let candidates = rest_api_region_candidates(sso_region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = USAGE_API_KIRO_VERSION;
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
        os_name, node_version, kiro_version, machine_id
    );
    let amz_user_agent = format!("aws-sdk-js/1.0.0 KiroIDE-{}-{}", kiro_version, machine_id);

    let client = build_client(proxy, 60, config.tls_backend)?;

    let mut last_error: Option<String> = None;
    let mut empty_seen = false;
    for region in candidates.iter() {
        let host = format!("q.{}.amazonaws.com", region);
        let url = format!("https://{}/", host);

        let mut request = client
            .post(&url)
            .header("content-type", "application/x-amz-json-1.0")
            .header(
                "x-amz-target",
                "AmazonCodeWhispererService.ListAvailableProfiles",
            )
            .header("x-amz-user-agent", &amz_user_agent)
            .header("user-agent", &user_agent)
            .header("host", &host)
            .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .header("Authorization", format!("Bearer {}", token))
            .header("Connection", "close")
            .body(r#"{"maxResults":10}"#);

        if let Some(token_type) = credentials.token_type_header() {
            request = request.header("tokentype", token_type);
        }

        let response = request.send().await?;
        let status = response.status();
        let rate_limit_error = (status.as_u16() == 429)
            .then(|| UpstreamRateLimitError::from_headers(response.headers()));

        if status.is_success() {
            let data: ListAvailableProfilesResponse = response.json().await?;
            // 该区域无 profile 时尝试另一个区域端点（账号可能在 eu-central-1）
            if data.first_arn().is_none() {
                empty_seen = true;
                continue;
            }
            return Ok(data);
        }

        let body_text = response.text().await.unwrap_or_default();
        if let Some(error) = rate_limit_error {
            return Err(error.into());
        }
        last_error = Some(format!("{} {}", status, body_text));
        // 403 等错误继续尝试下一个候选端点
    }

    // 没有任何端点返回 profile：若至少有一次成功但为空，视为"该账号无 Enterprise profile"
    // （BuilderID 等），返回空结果让调用方回退到占位符逻辑。
    if empty_seen {
        return Ok(ListAvailableProfilesResponse::default());
    }

    bail!(
        "获取可用 profile 失败: {}",
        last_error.unwrap_or_else(|| "无可用端点".to_string())
    );
}

/// 设置用户偏好（开启/关闭超额）
///
/// 上游接口：`POST https://q.{region}.amazonaws.com/setUserPreference`
/// Body: `{ "overageConfiguration": { "overageStatus": "ENABLED" | "DISABLED" }, "profileArn": "..." }`
pub(crate) async fn set_user_preference(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    overage_status: &str, // "ENABLED" or "DISABLED"
) -> anyhow::Result<()> {
    tracing::debug!("正在设置用户偏好 overageStatus={}", overage_status);

    // setUserPreference 仅在 us-east-1 / eu-central-1 提供服务，
    // 依据凭据 SSO 区域选择主端点，403 时回退到另一个端点。
    let sso_region = credentials.effective_auth_region(config);
    let candidates = rest_api_region_candidates(sso_region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = USAGE_API_KIRO_VERSION;
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
        os_name, node_version, kiro_version, machine_id
    );
    let amz_user_agent = format!("aws-sdk-js/1.0.0 KiroIDE-{}-{}", kiro_version, machine_id);

    let client = build_client(proxy, 60, config.tls_backend)?;

    // 构建 body：仅发送真实 profileArn，跳过 BuilderID 占位符
    let body = set_user_preference_body(credentials, overage_status);

    let mut last_error: Option<String> = None;
    for (idx, region) in candidates.iter().enumerate() {
        let host = format!("q.{}.amazonaws.com", region);
        let url = format!("https://{}/setUserPreference", host);

        let mut request = client
            .post(&url)
            .header("x-amz-user-agent", &amz_user_agent)
            .header("user-agent", &user_agent)
            .header("host", &host)
            .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .header("Authorization", format!("Bearer {}", token))
            .header("content-type", "application/json")
            .header("Connection", "close")
            .json(&body);

        if let Some(token_type) = credentials.token_type_header() {
            request = request.header("tokentype", token_type);
        }

        let response = request.send().await?;

        let status = response.status();
        let rate_limit_error = (status.as_u16() == 429)
            .then(|| UpstreamRateLimitError::from_headers(response.headers()));
        if status.is_success() {
            return Ok(());
        }

        let body_text = response.text().await.unwrap_or_default();
        if let Some(error) = rate_limit_error {
            return Err(error.into());
        }

        // 403 且仍有备用端点时，尝试下一个区域端点（Enterprise/IdC 跨区兼容）
        if status.as_u16() == 403 && idx + 1 < candidates.len() {
            tracing::debug!(
                "setUserPreference 在 {} 返回 403，尝试备用端点 {}",
                region,
                candidates[idx + 1]
            );
            last_error = Some(format!("{} {}", status, body_text));
            continue;
        }

        let error_msg = match status.as_u16() {
            400 => "请求参数错误，账号可能不支持超额",
            401 => "认证失败，Token 无效或已过期",
            403 => "权限不足，无法设置用户偏好",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "设置用户偏好失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    // 所有候选端点均失败（理论上循环内已 return / bail，此处为兜底）
    bail!(
        "权限不足，无法设置用户偏好: {}",
        last_error.unwrap_or_else(|| "无可用端点".to_string())
    );
}

// ============================================================================
// 多凭据 Token 管理器
// ============================================================================

/// 单个凭据条目的状态
struct CredentialEntry {
    /// 凭据唯一 ID
    id: u64,
    /// 凭据信息
    credentials: KiroCredentials,
    /// API 调用连续失败次数
    failure_count: u32,
    /// API 调用累计失败次数（含所有失败类型：鉴权/额度/风控/瞬态/网络）。
    /// 只增不减，成功不清零，仅手动重置失败计数时归零。仅用于展示与排查。
    total_failure_count: u64,
    /// Token 刷新连续失败次数
    refresh_failure_count: u32,
    /// 是否已禁用
    disabled: bool,
    /// 禁用原因（用于区分手动禁用 vs 自动禁用，便于自愈）
    disabled_reason: Option<DisabledReason>,
    /// API 调用成功次数
    success_count: u64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    last_used_at: Option<String>,
    /// 临时冷却到期时间（账号级 429 风控触发后短期跳过该凭据）
    /// `Some(t)` 且 `t > now()` 时视为不可用；`t <= now()` 时自动恢复。
    /// 不持久化，进程重启后清空。
    throttled_until: Option<Instant>,
    /// RPM 主动限流的滑动窗口：最近 60 秒内被选中发起请求的时间戳队列。
    /// 队列长度达到 `account_rpm_limit` 时该凭据本窗口内被排除出候选。
    /// 不持久化，进程重启后清空；限流关闭时始终为空。
    rpm_window: VecDeque<Instant>,
    /// 当前凭据连续执行自愈的轮数。同一凭据成功后清零。
    self_heal_consecutive_rounds: u32,
    /// 当前凭据累计被自愈恢复的次数。
    self_heal_total_count: u64,
    /// 最近一次自愈时间。使用绝对时间以支持跨进程重启的冷却判断。
    last_self_heal_at: Option<DateTime<Utc>>,
    /// 当前连续自愈轮次对应的模型；None 表示 MCP/无模型请求。
    self_heal_model: Option<String>,
}

impl CredentialEntry {
    /// 清空失败计数与禁用/冷却状态，让凭据重新参与调度
    fn reset_health(&mut self) {
        self.failure_count = 0;
        self.total_failure_count = 0;
        self.refresh_failure_count = 0;
        self.disabled = false;
        self.disabled_reason = None;
        self.throttled_until = None;
        self.rpm_window.clear();
        self.clear_self_heal_streak();
    }
}

/// 判断 `entries` 中除 `skip_idx` 外是否已存在相同的 refreshToken
fn refresh_token_duplicate_exists(
    entries: &[CredentialEntry],
    refresh_token: &str,
    skip_idx: Option<usize>,
) -> bool {
    entries.iter().enumerate().any(|(idx, entry)| {
        Some(idx) != skip_idx && entry.credentials.refresh_token.as_deref() == Some(refresh_token)
    })
}

/// 禁用原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisabledReason {
    /// Admin API 手动禁用
    Manual,
    /// 连续失败达到阈值后自动禁用
    TooManyFailures,
    /// 上游明确返回账号封禁/停用（403 + 封禁文案）后立即禁用。
    /// 不可自动恢复、**不参与自愈**，需人工联系客服核实后手动重置。
    Suspended,
    /// Token 刷新连续失败达到阈值后自动禁用
    TooManyRefreshFailures,
    /// 额度已用尽（如 MONTHLY_REQUEST_COUNT）
    QuotaExceeded,
    /// Refresh Token 永久失效（服务端返回 invalid_grant）
    InvalidRefreshToken,
    /// 凭据配置无效（如 authMethod=api_key 但缺少 kiroApiKey）
    InvalidConfig,
}

impl DisabledReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "Manual",
            Self::TooManyFailures => "TooManyFailures",
            Self::Suspended => "Suspended",
            Self::TooManyRefreshFailures => "TooManyRefreshFailures",
            Self::QuotaExceeded => "QuotaExceeded",
            Self::InvalidRefreshToken => "InvalidRefreshToken",
            Self::InvalidConfig => "InvalidConfig",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "Manual" => Some(Self::Manual),
            "TooManyFailures" => Some(Self::TooManyFailures),
            "Suspended" => Some(Self::Suspended),
            "TooManyRefreshFailures" => Some(Self::TooManyRefreshFailures),
            "QuotaExceeded" => Some(Self::QuotaExceeded),
            "InvalidRefreshToken" => Some(Self::InvalidRefreshToken),
            "InvalidConfig" => Some(Self::InvalidConfig),
            _ => None,
        }
    }
}

impl CredentialEntry {
    fn clear_self_heal_streak(&mut self) {
        self.self_heal_consecutive_rounds = 0;
        self.last_self_heal_at = None;
        self.self_heal_model = None;
    }

    fn credentials_snapshot(&self) -> KiroCredentials {
        let mut credentials = self.credentials.clone();
        credentials.canonicalize_auth_method();
        credentials.id = Some(self.id);
        credentials.disabled = self.disabled;
        credentials.disabled_reason = self
            .disabled_reason
            .map(|reason| reason.as_str().to_string());
        credentials.self_heal_consecutive_rounds = self.self_heal_consecutive_rounds;
        credentials.self_heal_total_count = self.self_heal_total_count;
        credentials.last_self_heal_at = self.last_self_heal_at.map(|value| value.to_rfc3339());
        credentials.self_heal_model = self.self_heal_model.clone();
        credentials
    }
}

/// 统计数据持久化条目
#[derive(Serialize, Deserialize)]
struct StatsEntry {
    success_count: u64,
    #[serde(default)]
    total_failure_count: u64,
    last_used_at: Option<String>,
}

// ============================================================================
// Admin API 公开结构
// ============================================================================

/// 凭据条目快照（用于 Admin API 读取）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEntrySnapshot {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级
    pub priority: u32,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 累计失败次数（所有失败类型，只增不减，仅手动重置归零）
    pub total_failure_count: u64,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 身份提供商（BuilderId / Enterprise / Github / Google / IAM_SSO）
    pub provider: Option<String>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// Token 过期时间
    pub expires_at: Option<String>,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据，用于前端去重）
    pub api_key_hash: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据，用于前端显示）
    pub masked_api_key: Option<String>,
    /// 用户邮箱（用于前端显示）
    pub email: Option<String>,
    /// 最近一次查询到的 Kiro 订阅等级；凭据禁用后也应保留展示。
    pub subscription_title: Option<String>,
    /// API 调用成功次数
    pub success_count: u64,
    /// 最后一次 API 调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 是否配置了凭据级代理
    pub has_proxy: bool,
    /// 代理 URL（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// Token 刷新连续失败次数
    pub refresh_failure_count: u32,
    /// 禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 临时冷却剩余秒数（账号级 429 风控）；冷却中且 `> 0` 才返回
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throttled_remaining_secs: Option<u64>,
    /// 端点名称（未显式配置时返回 None，由 Admin 层回退到默认值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// 账号所属分组（可属于多个分组）
    #[serde(default)]
    pub groups: Vec<String>,
    /// 账号来源渠道（纯备注）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_channel: Option<String>,
    /// 凭据扩展元数据
    pub metadata: crate::kiro::model::credentials::CredentialMetadata,
    /// 凭据添加（创建）时间（RFC3339 格式）；旧凭据缺失时为 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// 凭据管理器状态快照
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerSnapshot {
    /// 凭据条目列表
    pub entries: Vec<CredentialEntrySnapshot>,
    /// 内部调度指针；balanced 模式不代表唯一活跃凭据
    pub current_id: u64,
    /// 总凭据数量
    pub total: usize,
    /// 可用凭据数量
    pub available: usize,
}

#[derive(Clone)]
struct ModelCacheEntry {
    response: ListAvailableModelsResponse,
    refreshed_at: Instant,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelDiscoveryError {
    #[error("没有符合当前客户端分组的可用凭据")]
    NoAvailableCredentials,
    #[error("所有 {credential_count} 个凭据的模型列表首次加载均失败")]
    ColdStartFailed { credential_count: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CachedModelSupport {
    Confirmed,
    Unknown,
    Unsupported,
}

/// 多凭据 Token 管理器
///
/// 支持多个凭据的管理，实现固定优先级 + 故障转移策略
/// 故障统计基于 API 调用结果，而非 Token 刷新结果
pub struct MultiTokenManager {
    config: Config,
    /// 全局代理（运行时可修改）
    proxy: Mutex<Option<ProxyConfig>>,
    /// 凭据条目列表
    entries: Mutex<Vec<CredentialEntry>>,
    /// 当前活动凭据 ID
    current_id: Mutex<u64>,
    /// 下一个待分配凭据 ID。进程内单调递增，避免删除账号后新账号复用旧 ID，
    /// 从而继承旧账号按 credential_id 聚合的 trace/usage 历史。
    next_id: AtomicU64,
    /// Token 刷新锁，确保同一时间只有一个刷新操作
    refresh_lock: TokioMutex<()>,
    /// 凭据文件路径（用于回写）
    credentials_path: Option<PathBuf>,
    /// 凭据文件写入锁。`persist_credentials` 用整文件覆写，并发调用会互相踩踏，
    /// 故用此锁串行化所有写盘操作（批量导入等场景会并发触发）。
    persist_lock: Mutex<()>,
    /// config.json 的读改写锁。所有运行时配置入口共享，避免并发部分更新丢字段。
    config_write_lock: Mutex<()>,
    /// 多字段运行时配置的更新锁，保证“读取旧值 → 合并 patch → 应用 → 持久化”原子化。
    runtime_config_update_lock: Mutex<()>,
    /// 是否为多凭据格式（数组格式才回写；通过 add_credential 动态升级为 true）
    is_multiple_format: AtomicBool,
    /// 负载均衡模式（运行时可修改）
    load_balancing_mode: Mutex<String>,
    /// 账号级 429 风控故障转移开关（运行时可修改）
    account_throttle_failover: AtomicBool,
    /// 账号级风控冷却时长（秒，运行时可修改）
    account_throttle_cooldown_secs: AtomicU64,
    /// 单账号 RPM 主动限流开关（运行时可修改）
    account_rpm_limit_enabled: AtomicBool,
    /// 单账号每分钟请求次数上限（运行时可修改）
    account_rpm_limit: AtomicU32,
    /// 是否识别 403 封禁文案并立即禁用（运行时可修改）
    suspended_detection_enabled: AtomicBool,
    /// 全账号自愈总开关（运行时可修改）
    self_heal_enabled: AtomicBool,
    /// 两次自愈的最小冷却间隔（秒，运行时可修改）
    self_heal_min_interval_secs: AtomicU64,
    /// 连续自愈最大轮数（0=不限，运行时可修改）
    self_heal_max_consecutive_rounds: AtomicU32,
    /// 最近一次统计持久化时间（用于 debounce）
    last_stats_save_at: Mutex<Option<Instant>>,
    /// 统计数据是否有未落盘更新
    stats_dirty: AtomicBool,
    /// 每个凭据最后一次成功加载的完整模型列表。
    model_cache: Mutex<HashMap<u64, ModelCacheEntry>>,
    /// 每凭据单飞锁，避免并发模型列表请求重复访问上游。
    model_refresh_locks: Mutex<HashMap<u64, Arc<TokioMutex<()>>>>,
    /// 限制同时访问 ListAvailableModels 的凭据数。
    model_refresh_semaphore: Semaphore,
    /// 凭据级缓存代数；凭据信息变化时递增，阻止在途旧请求回填缓存。
    model_cache_generations: Mutex<HashMap<u64, u64>>,
    /// 全局代理变化时递增，阻止所有在途旧请求回填缓存。
    model_cache_epoch: AtomicU64,
}

/// 每个凭据最大 API 调用失败次数
const MAX_FAILURES_PER_CREDENTIAL: u32 = 3;

/// 单账号 RPM 限流的滑动窗口长度（秒）。固定 60 秒 = 每分钟。
const RPM_WINDOW_SECS: u64 = 60;
/// 统计数据持久化防抖间隔
const STATS_SAVE_DEBOUNCE: StdDuration = StdDuration::from_secs(30);

/// API 调用上下文
///
/// 绑定特定凭据的调用上下文，确保 token、credentials 和 id 的一致性
/// 用于解决并发调用时 current_id 竞态问题
#[derive(Clone)]
pub struct CallContext {
    /// 凭据 ID（用于 report_success/report_failure）
    pub id: u64,
    /// 凭据信息（用于构建请求头）
    pub credentials: KiroCredentials,
    /// 访问 Token
    pub token: String,
}

pub struct IdcReloginCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: Option<String>,
    pub client_id: String,
    pub client_secret: String,
    pub region: String,
    pub start_url: String,
    pub provider: String,
}

/// 判断某账号的分组集合是否匹配请求所属分组（严格隔离）
///
/// - `group = None`：Key 未绑定分组（含 master apiKey），匹配所有账号。
/// - `group = Some(g)`：仅匹配 `cred_groups` 包含 `g` 的账号。
fn group_matches(cred_groups: &[String], group: Option<&str>) -> bool {
    match group {
        None => true,
        Some(g) => cred_groups.iter().any(|cg| cg == g),
    }
}

fn credential_matches_request(
    credentials: &KiroCredentials,
    model: Option<&str>,
    group: Option<&str>,
) -> bool {
    let is_opus = model
        .map(|m| m.to_ascii_lowercase().contains("opus"))
        .unwrap_or(false);

    if is_opus && !credentials.supports_opus() {
        return false;
    }

    group_matches(&credentials.groups, group)
}

fn normalize_self_heal_model(model: Option<&str>) -> Option<String> {
    model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
}

impl MultiTokenManager {
    /// 创建多凭据 Token 管理器
    ///
    /// # Arguments
    /// * `config` - 应用配置
    /// * `credentials` - 凭据列表
    /// * `proxy` - 可选的代理配置
    /// * `credentials_path` - 凭据文件路径（用于回写）
    /// * `is_multiple_format` - 是否为多凭据格式（数组格式才回写）
    pub fn new(
        config: Config,
        credentials: Vec<KiroCredentials>,
        proxy: Option<ProxyConfig>,
        credentials_path: Option<PathBuf>,
        is_multiple_format: bool,
    ) -> anyhow::Result<Self> {
        // 计算当前最大 ID，为没有 ID 的凭据分配新 ID
        let max_existing_id = credentials.iter().filter_map(|c| c.id).max().unwrap_or(0);
        let mut next_id = max_existing_id + 1;
        let mut has_new_ids = false;
        let mut has_new_machine_ids = false;
        let config_ref = &config;

        let entries: Vec<CredentialEntry> = credentials
            .into_iter()
            .map(|mut cred| {
                cred.canonicalize_auth_method();
                let id = cred.id.unwrap_or_else(|| {
                    let id = next_id;
                    next_id += 1;
                    cred.id = Some(id);
                    has_new_ids = true;
                    id
                });
                if cred.fill_default_profile_arn() {
                    has_new_ids = true;
                }
                if cred.machine_id.is_none() {
                    cred.machine_id =
                        Some(machine_id::generate_from_credentials(&cred, config_ref));
                    has_new_machine_ids = true;
                }
                let disabled_reason = if cred.disabled {
                    match cred
                        .disabled_reason
                        .as_deref()
                        .and_then(DisabledReason::from_str)
                    {
                        Some(reason) => Some(reason),
                        None => {
                            if let Some(reason) = cred.disabled_reason.as_deref() {
                                tracing::warn!(
                                    "凭据 #{} 的禁用原因 `{}` 无法识别，按 Manual 处理",
                                    id,
                                    reason
                                );
                            }
                            Some(DisabledReason::Manual)
                        }
                    }
                } else {
                    None
                };
                let last_self_heal_at = cred
                    .last_self_heal_at
                    .as_deref()
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .map(|value| value.with_timezone(&Utc));
                CredentialEntry {
                    id,
                    credentials: cred.clone(),
                    failure_count: 0,
                    total_failure_count: 0,
                    refresh_failure_count: 0,
                    disabled: cred.disabled, // 从配置文件读取 disabled 状态
                    disabled_reason,
                    success_count: 0,
                    last_used_at: None,
                    throttled_until: None,
                    rpm_window: VecDeque::new(),
                    self_heal_consecutive_rounds: cred.self_heal_consecutive_rounds,
                    self_heal_total_count: cred.self_heal_total_count,
                    last_self_heal_at,
                    self_heal_model: cred.self_heal_model.clone(),
                }
            })
            .collect();

        // 校验 API Key 凭据配置完整性：authMethod=api_key 时必须提供 kiroApiKey
        let mut entries = entries;
        for entry in &mut entries {
            if entry.credentials.kiro_api_key.is_none()
                && entry
                    .credentials
                    .auth_method
                    .as_deref()
                    .map(|m| m.eq_ignore_ascii_case("api_key") || m.eq_ignore_ascii_case("apikey"))
                    .unwrap_or(false)
            {
                tracing::warn!(
                    "凭据 #{} 配置了 authMethod=api_key 但缺少 kiroApiKey 字段，已自动禁用",
                    entry.id
                );
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::InvalidConfig);
            }
        }

        // 检测重复 ID
        let mut seen_ids = std::collections::HashSet::new();
        let mut duplicate_ids = Vec::new();
        for entry in &entries {
            if !seen_ids.insert(entry.id) {
                duplicate_ids.push(entry.id);
            }
        }
        if !duplicate_ids.is_empty() {
            anyhow::bail!("检测到重复的凭据 ID: {:?}", duplicate_ids);
        }

        // 选择初始凭据：优先级最高（priority 最小）的可用凭据，无可用凭据时为 0
        let initial_id = entries
            .iter()
            .filter(|e| !e.disabled)
            .min_by_key(|e| (e.credentials.priority, e.id))
            .map(|e| e.id)
            .unwrap_or(0);

        let load_balancing_mode = config.load_balancing_mode.clone();
        let throttle_failover = config.account_throttle_failover;
        let throttle_cooldown_secs = config.account_throttle_cooldown_secs;
        let rpm_limit_enabled = config.account_rpm_limit_enabled;
        let rpm_limit = config.account_rpm_limit;
        let suspended_detection_enabled = config.suspended_detection_enabled;
        let self_heal_enabled = config.self_heal_enabled;
        let self_heal_min_interval_secs = config.self_heal_min_interval_secs;
        let self_heal_max_consecutive_rounds = config.self_heal_max_consecutive_rounds;
        let manager = Self {
            config,
            proxy: Mutex::new(proxy),
            entries: Mutex::new(entries),
            current_id: Mutex::new(initial_id),
            next_id: AtomicU64::new(next_id),
            refresh_lock: TokioMutex::new(()),
            credentials_path,
            persist_lock: Mutex::new(()),
            config_write_lock: Mutex::new(()),
            runtime_config_update_lock: Mutex::new(()),
            is_multiple_format: AtomicBool::new(is_multiple_format),
            load_balancing_mode: Mutex::new(load_balancing_mode),
            account_throttle_failover: AtomicBool::new(throttle_failover),
            account_throttle_cooldown_secs: AtomicU64::new(throttle_cooldown_secs),
            account_rpm_limit_enabled: AtomicBool::new(rpm_limit_enabled),
            account_rpm_limit: AtomicU32::new(rpm_limit),
            suspended_detection_enabled: AtomicBool::new(suspended_detection_enabled),
            self_heal_enabled: AtomicBool::new(self_heal_enabled),
            self_heal_min_interval_secs: AtomicU64::new(self_heal_min_interval_secs),
            self_heal_max_consecutive_rounds: AtomicU32::new(self_heal_max_consecutive_rounds),
            last_stats_save_at: Mutex::new(None),
            stats_dirty: AtomicBool::new(false),
            model_cache: Mutex::new(HashMap::new()),
            model_refresh_locks: Mutex::new(HashMap::new()),
            model_refresh_semaphore: Semaphore::new(4),
            model_cache_generations: Mutex::new(HashMap::new()),
            model_cache_epoch: AtomicU64::new(0),
        };

        // 单凭据格式自动迁移：升级为数组格式，确保 token rotation 能写盘
        // 触发条件：原文件是单对象格式 && 存在凭据 && 有文件路径
        if !is_multiple_format
            && !manager.entries.lock().is_empty()
            && manager.credentials_path.is_some()
        {
            manager.is_multiple_format.store(true, Ordering::Relaxed);
            if let Err(e) = manager.persist_credentials() {
                tracing::warn!("单凭据格式迁移到数组格式失败: {}", e);
            } else {
                tracing::info!(
                    "已将凭据文件从单对象格式迁移到数组格式，token rotation 将正确持久化"
                );
            }
        }

        // 如果有新分配的 ID 或新生成的 machineId，立即持久化到配置文件
        if has_new_ids || has_new_machine_ids {
            if let Err(e) = manager.persist_credentials() {
                tracing::warn!("补全凭据 ID/machineId 后持久化失败: {}", e);
            } else {
                tracing::info!("已补全凭据 ID/machineId 并写回配置文件");
            }
        }

        // 加载持久化的统计数据（success_count, last_used_at）
        manager.load_stats();

        Ok(manager)
    }

    /// 获取配置的引用
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 串行执行一次 config.json 读改写，供所有 Admin 运行时配置入口复用。
    pub fn update_config_file(&self, updater: impl FnOnce(&mut Config)) -> anyhow::Result<()> {
        use anyhow::Context;

        let Some(path) = self.config.config_path() else {
            tracing::warn!("配置文件路径未知，配置更新仅在当前进程生效");
            return Ok(());
        };
        let _guard = self.config_write_lock.lock();
        let mut fresh =
            Config::load(path).with_context(|| format!("重新加载配置失败: {}", path.display()))?;
        updater(&mut fresh);
        fresh
            .save()
            .with_context(|| format!("持久化配置失败: {}", path.display()))
    }

    /// 获取全局代理配置的克隆（可安全跨锁使用）
    pub fn proxy(&self) -> Option<ProxyConfig> {
        self.proxy.lock().clone()
    }

    /// 设置全局代理配置（运行时修改，可传 None 清除）
    pub fn set_global_proxy(&self, proxy: Option<ProxyConfig>) {
        *self.proxy.lock() = proxy;
        self.invalidate_all_model_caches();
    }

    fn model_cache_ttl(&self) -> StdDuration {
        StdDuration::from_secs(self.config.model_cache_ttl_secs)
    }

    fn cached_model_response(
        &self,
        id: u64,
        require_fresh: bool,
    ) -> Option<ListAvailableModelsResponse> {
        self.model_cache.lock().get(&id).and_then(|entry| {
            if require_fresh && entry.refreshed_at.elapsed() >= self.model_cache_ttl() {
                None
            } else {
                Some(entry.response.clone())
            }
        })
    }

    fn model_cache_generation(&self, id: u64) -> u64 {
        self.model_cache_generations
            .lock()
            .get(&id)
            .copied()
            .unwrap_or(0)
    }

    fn invalidate_model_cache(&self, id: u64) {
        let mut generations = self.model_cache_generations.lock();
        *generations.entry(id).or_insert(0) += 1;
        self.model_cache.lock().remove(&id);
    }

    fn remove_model_cache(&self, id: u64) {
        self.invalidate_model_cache(id);
        self.model_refresh_locks.lock().remove(&id);
    }

    pub fn invalidate_all_model_caches(&self) {
        self.model_cache_epoch.fetch_add(1, Ordering::Relaxed);
        self.model_cache.lock().clear();
    }

    fn model_refresh_lock(&self, id: u64) -> Arc<TokioMutex<()>> {
        self.model_refresh_locks
            .lock()
            .entry(id)
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    }

    fn cached_model_support(&self, id: u64, model: Option<&str>) -> CachedModelSupport {
        let Some(model) = model else {
            return CachedModelSupport::Unknown;
        };
        let cache = self.model_cache.lock();
        let Some(entry) = cache.get(&id) else {
            return CachedModelSupport::Unknown;
        };
        if entry
            .response
            .models
            .iter()
            .any(|available| available.model_id.eq_ignore_ascii_case(model))
        {
            CachedModelSupport::Confirmed
        } else {
            CachedModelSupport::Unsupported
        }
    }

    async fn refresh_model_cache_for(
        &self,
        id: u64,
        force: bool,
    ) -> anyhow::Result<ListAvailableModelsResponse> {
        let refresh_requested_at = Instant::now();
        if !force && let Some(response) = self.cached_model_response(id, true) {
            return Ok(response);
        }

        let refresh_lock = self.model_refresh_lock(id);
        let _refresh_guard = refresh_lock.lock().await;
        if let Some(entry) = self.model_cache.lock().get(&id) {
            let refreshed_by_concurrent_request =
                force && entry.refreshed_at >= refresh_requested_at;
            let fresh_ttl_hit = !force && entry.refreshed_at.elapsed() < self.model_cache_ttl();
            if refreshed_by_concurrent_request || fresh_ttl_hit {
                return Ok(entry.response.clone());
            }
        }

        let generation = self.model_cache_generation(id);
        let epoch = self.model_cache_epoch.load(Ordering::Relaxed);
        let _permit = self.model_refresh_semaphore.acquire().await?;
        let (token, mut credentials) = self.prepare_request_token(id).await?;

        // 同 get_usage_limits_for：先解析回填真实 profileArn。
        match self.resolve_profile_arn_for(id, &token).await {
            Ok(Some(arn)) => credentials.profile_arn = Some(arn),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!("凭据 #{} 查询模型列表前解析 profileArn 失败: {}", id, error)
            }
        }

        let global_proxy = self.proxy.lock().clone();
        let effective_proxy = credentials.effective_proxy(global_proxy.as_ref());
        let response =
            get_available_models(&credentials, &self.config, &token, effective_proxy.as_ref())
                .await?;

        let generations = self.model_cache_generations.lock();
        if generation == generations.get(&id).copied().unwrap_or(0) {
            let mut cache = self.model_cache.lock();
            if epoch == self.model_cache_epoch.load(Ordering::Relaxed) {
                cache.insert(
                    id,
                    ModelCacheEntry {
                        response: response.clone(),
                        refreshed_at: Instant::now(),
                    },
                );
            }
        }
        Ok(response)
    }

    async fn cached_or_refresh_models_for(
        &self,
        id: u64,
    ) -> anyhow::Result<ListAvailableModelsResponse> {
        match self.refresh_model_cache_for(id, false).await {
            Ok(response) => Ok(response),
            Err(error) => {
                if let Some(stale) = self.cached_model_response(id, false) {
                    tracing::warn!(
                        "凭据 #{} 模型列表刷新失败，继续使用最后一次成功缓存: {}",
                        id,
                        error
                    );
                    Ok(stale)
                } else {
                    Err(error)
                }
            }
        }
    }

    fn available_model_credential_ids(&self, group: Option<&str>) -> Vec<u64> {
        let now = Instant::now();
        self.entries
            .lock()
            .iter()
            .filter(|entry| {
                !entry.disabled
                    && !entry
                        .throttled_until
                        .map(|until| until > now)
                        .unwrap_or(false)
                    && group_matches(&entry.credentials.groups, group)
            })
            .map(|entry| entry.id)
            .collect()
    }

    /// 返回当前客户端分组可访问凭据的动态模型并集原始项。
    pub async fn discover_models_for_group(
        &self,
        group: Option<&str>,
    ) -> Result<Vec<UpstreamModel>, ModelDiscoveryError> {
        let ids = self.available_model_credential_ids(group);
        if ids.is_empty() {
            return Err(ModelDiscoveryError::NoAvailableCredentials);
        }

        let results = futures::future::join_all(
            ids.iter()
                .copied()
                .map(|id| async move { (id, self.cached_or_refresh_models_for(id).await) }),
        )
        .await;

        let mut models = Vec::new();
        let mut successful_credentials = 0usize;
        for (id, result) in results {
            match result {
                Ok(response) => {
                    successful_credentials += 1;
                    models.extend(response.models);
                }
                Err(error) => {
                    tracing::warn!("凭据 #{} 首次加载模型列表失败: {}", id, error);
                }
            }
        }

        if successful_credentials == 0 {
            Err(ModelDiscoveryError::ColdStartFailed {
                credential_count: ids.len(),
            })
        } else {
            Ok(models)
        }
    }

    /// 服务启动后异步预热所有当前启用凭据的模型缓存。
    pub fn start_model_cache_warmer(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            match manager.discover_models_for_group(None).await {
                Ok(models) => {
                    tracing::info!("模型缓存预热完成，共加载 {} 个模型条目", models.len())
                }
                Err(ModelDiscoveryError::NoAvailableCredentials) => {
                    tracing::debug!("没有可用于模型缓存预热的凭据")
                }
                Err(error) => tracing::warn!("模型缓存预热失败: {}", error),
            }
        });
    }

    /// 获取指定分组的凭据总数（group=None 时返回全部凭据数）
    ///
    /// 用于按分组计算 failover 重试预算，避免小分组按全局账号数获得过多无效重试。
    pub fn total_count_in_group(&self, group: Option<&str>) -> usize {
        self.entries
            .lock()
            .iter()
            .filter(|e| group_matches(&e.credentials.groups, group))
            .count()
    }

    /// 获取可用凭据数量
    pub fn available_count(&self) -> usize {
        let now = Instant::now();
        self.entries
            .lock()
            .iter()
            .filter(|e| !e.disabled && !e.throttled_until.map(|t| t > now).unwrap_or(false))
            .count()
    }

    fn entry_available_for_request(
        &self,
        entry: &CredentialEntry,
        model: Option<&str>,
        group: Option<&str>,
        now: Instant,
    ) -> bool {
        !entry.disabled
            && !entry
                .throttled_until
                .map(|until| until > now)
                .unwrap_or(false)
            && !self.rpm_exceeded(entry, now)
            && credential_matches_request(&entry.credentials, model, group)
            && self.cached_model_support(entry.id, model) != CachedModelSupport::Unsupported
    }

    /// 判断凭据在当前 60 秒滑动窗口内是否已达到 RPM 上限。
    ///
    /// 限流未开启时恒为 `false`（不参与调度判断）。只读判断，不修改窗口；
    /// 过期时间戳的实际清理发生在 [`Self::record_request`]。
    fn rpm_exceeded(&self, entry: &CredentialEntry, now: Instant) -> bool {
        if !self.account_rpm_limit_enabled.load(Ordering::Relaxed) {
            return false;
        }
        let limit = self.account_rpm_limit.load(Ordering::Relaxed);
        if limit == 0 {
            return false;
        }
        let window = StdDuration::from_secs(RPM_WINDOW_SECS);
        let fresh = entry
            .rpm_window
            .iter()
            .filter(|&&ts| now.duration_since(ts) < window)
            .count();
        fresh as u32 >= limit
    }

    /// 当所有其它条件均满足的候选都耗尽 RPM 额度时，返回最早可重试秒数。
    fn rpm_retry_after_secs(
        &self,
        entries: &[CredentialEntry],
        model: Option<&str>,
        group: Option<&str>,
        now: Instant,
    ) -> Option<u64> {
        if !self.account_rpm_limit_enabled.load(Ordering::Relaxed) {
            return None;
        }
        let limit = self.account_rpm_limit.load(Ordering::Relaxed) as usize;
        if limit == 0 {
            return None;
        }

        let window = StdDuration::from_secs(RPM_WINDOW_SECS);
        let mut earliest_retry_after = None;

        for entry in entries.iter().filter(|entry| {
            !entry.disabled
                && !entry
                    .throttled_until
                    .map(|until| until > now)
                    .unwrap_or(false)
                && credential_matches_request(&entry.credentials, model, group)
                && self.cached_model_support(entry.id, model) != CachedModelSupport::Unsupported
        }) {
            let fresh_count = entry
                .rpm_window
                .iter()
                .filter(|&&ts| now.duration_since(ts) < window)
                .count();
            if fresh_count < limit {
                return None;
            }

            // 窗口可能因运行时下调 limit 而暂时多于上限；需要等到
            // fresh_count - limit + 1 个时间戳过期后才重新有额度。
            let release_index = fresh_count - limit;
            let release_at = entry
                .rpm_window
                .iter()
                .filter(|&&ts| now.duration_since(ts) < window)
                .nth(release_index)
                .copied()
                .expect("fresh_count 与窗口迭代结果应一致")
                + window;
            let remaining = release_at.saturating_duration_since(now);
            let retry_after = remaining
                .as_secs()
                .saturating_add(u64::from(remaining.subsec_nanos() > 0))
                .max(1);
            earliest_retry_after = Some(
                earliest_retry_after
                    .map(|current: u64| current.min(retry_after))
                    .unwrap_or(retry_after),
            );
        }

        earliest_retry_after
    }

    /// 尝试为一次真实业务请求预留 RPM 额度。
    ///
    /// 在同一把 `entries` 锁内完成过期清理、上限检查和记账，避免多个并发请求
    /// 在选择阶段同时通过检查后全部写入窗口。返回 `false` 表示额度已被其它请求
    /// 抢先占用，调用方应重新选择凭据。
    fn record_request(&self, id: u64) -> bool {
        let now = Instant::now();
        let window = StdDuration::from_secs(RPM_WINDOW_SECS);
        let mut entries = self.entries.lock();
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            return false;
        };
        if !self.account_rpm_limit_enabled.load(Ordering::Relaxed) {
            if !entry.rpm_window.is_empty() {
                entry.rpm_window.clear();
            }
            return true;
        }
        let limit = self.account_rpm_limit.load(Ordering::Relaxed);
        if limit == 0 {
            entry.rpm_window.clear();
            return true;
        }
        while let Some(&front) = entry.rpm_window.front() {
            if now.duration_since(front) >= window {
                entry.rpm_window.pop_front();
            } else {
                break;
            }
        }
        if entry.rpm_window.len() >= limit as usize {
            return false;
        }
        entry.rpm_window.push_back(now);
        true
    }

    fn has_available_for_request(
        &self,
        entries: &[CredentialEntry],
        model: Option<&str>,
        group: Option<&str>,
    ) -> bool {
        let now = Instant::now();
        entries
            .iter()
            .any(|entry| self.entry_available_for_request(entry, model, group, now))
    }

    /// 根据负载均衡模式选择下一个凭据
    ///
    /// - priority 模式：选择优先级最高（priority 最小）的可用凭据
    /// - balanced 模式：均衡选择可用凭据
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    fn select_next_credential(
        &self,
        model: Option<&str>,
        group: Option<&str>,
    ) -> Option<(u64, KiroCredentials)> {
        let entries = self.entries.lock();
        let now = Instant::now();

        // 过滤可用凭据
        let available: Vec<_> = entries
            .iter()
            .filter_map(|e| {
                if !self.entry_available_for_request(e, model, group, now) {
                    return None;
                }
                let model_support = self.cached_model_support(e.id, model);
                Some((e, model_support))
            })
            .collect();

        if available.is_empty() {
            return None;
        }

        let mode = self.load_balancing_mode.lock().clone();
        let mode = mode.as_str();

        match mode {
            "balanced" => {
                // Least-Used 策略：选择成功次数最少的凭据
                // 平局时按优先级排序（数字越小优先级越高）
                let (entry, _) = available.iter().min_by_key(|(e, support)| {
                    let discovery_rank = usize::from(*support != CachedModelSupport::Confirmed);
                    (
                        discovery_rank,
                        e.success_count,
                        e.credentials.priority,
                        e.id,
                    )
                })?;

                Some((entry.id, entry.credentials.clone()))
            }
            _ => {
                // priority 模式（默认）：严格选择数字最小的有效凭据。
                // 同优先级按 ID 升序固定顺序，保证后端调度与前端预览一致。
                let (entry, _) = available
                    .iter()
                    .min_by_key(|(e, _)| (e.credentials.priority, e.id))?;
                Some((entry.id, entry.credentials.clone()))
            }
        }
    }

    /// 获取 API 调用上下文
    ///
    /// 返回绑定了 id、credentials 和 token 的调用上下文
    /// 确保整个 API 调用过程中使用一致的凭据信息
    ///
    /// 如果 Token 过期或即将过期，会自动刷新
    /// Token 刷新失败会累计到当前凭据，达到阈值后禁用并切换
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    pub async fn acquire_context(
        &self,
        model: Option<&str>,
        group: Option<&str>,
    ) -> anyhow::Result<CallContext> {
        self.acquire_context_impl(model, group, true)
            .await
            .map(|(context, _)| context)
    }

    /// 获取 API 调用上下文，并返回本次选择是否使用了 balanced 模式。
    ///
    /// `update_current` 仅应在真实业务请求中开启。Admin 模型发现需要复用同一套
    /// 凭据选择和 Token 刷新规则，但不应因只读查询改变调度状态。
    async fn acquire_context_impl(
        &self,
        model: Option<&str>,
        group: Option<&str>,
        update_current: bool,
    ) -> anyhow::Result<(CallContext, bool)> {
        let total = self.total_count_in_group(group);
        let max_attempts = (total * MAX_FAILURES_PER_CREDENTIAL as usize).max(1);
        let mut attempt_count = 0;

        loop {
            if attempt_count >= max_attempts {
                anyhow::bail!(
                    "所有凭据均无法获取有效 Token（可用: {}/{}）",
                    self.available_count(),
                    total
                );
            }

            let (id, credentials, is_balanced) = {
                let is_balanced = self.load_balancing_mode.lock().as_str() == "balanced";

                // 两种模式都按当前请求重新选择。priority 模式不能复用 current_id，
                // 否则高优先级凭据从 RPM/冷却恢复后无法在下一次请求立即回切。
                let mut best = self.select_next_credential(model, group);

                // 没有可用凭据：如果是"自动禁用导致全灭"，做一次受控自愈
                // （受冷却间隔与连续轮数上限约束，避免持续 403 死循环）。
                if best.is_none() && self.try_self_heal(model, group) {
                    best = self.select_next_credential(model, group);
                }

                let (id, credentials) = if let Some((new_id, new_creds)) = best {
                    if update_current {
                        let mut current_id = self.current_id.lock();
                        *current_id = new_id;
                    }
                    (new_id, new_creds)
                } else {
                    let entries = self.entries.lock();
                    // RPM 打满（而非全部禁用）时回 429 并带 Retry-After，
                    // 让调用方知道这是限流而不是凭据耗尽。
                    if let Some(retry_after) =
                        self.rpm_retry_after_secs(&entries, model, group, Instant::now())
                    {
                        return Err(
                            UpstreamRateLimitError::new(Some(retry_after.to_string())).into()
                        );
                    }
                    // 注意：必须在 bail! 之前计算 available_count，
                    // 因为 available_count() 会尝试获取 entries 锁，
                    // 而此时我们已经持有该锁，会导致死锁
                    let available = entries.iter().filter(|e| !e.disabled).count();
                    anyhow::bail!("所有凭据均已禁用（{}/{}）", available, total);
                };

                (id, credentials, is_balanced)
            };

            // 尝试获取/刷新 Token
            match self.try_ensure_token(id, &credentials).await {
                Ok(ctx) => {
                    // 仅真实业务请求计入 RPM 窗口；Admin 只读模型发现不消耗额度。
                    if update_current && !self.record_request(id) {
                        // Token 获取期间额度可能被其它并发请求抢先占用；重新选号。
                        continue;
                    }
                    return Ok((ctx, is_balanced));
                }
                Err(e) => {
                    let Some(has_available) = self.handle_token_refresh_error(id, e)? else {
                        // 从源文件加载到了轮换后的 Token，不计失败次数，直接重试。
                        continue;
                    };
                    attempt_count += 1;
                    if !has_available {
                        anyhow::bail!("所有凭据均已禁用（0/{}）", total);
                    }
                }
            }
        }
    }

    /// 分类并记录一次 Token 刷新错误。
    ///
    /// 上游 429 是临时流控，不代表凭据失效，因此直接保留类型化错误返回，绝不增加
    /// `refresh_failure_count`。`Ok(None)` 表示已从源文件加载到轮换后的 Token，调用方
    /// 应使用同一凭据立即重试；`Ok(Some(_))` 返回记录失败后是否仍有可用凭据。
    fn handle_token_refresh_error(
        &self,
        id: u64,
        error: anyhow::Error,
    ) -> anyhow::Result<Option<bool>> {
        if error.downcast_ref::<UpstreamRateLimitError>().is_some() {
            tracing::warn!("凭据 #{} Token 刷新被上游限流", id);
            return Err(error);
        }

        if error.downcast_ref::<RefreshTokenInvalidError>().is_some() {
            // 先尝试从源文件重新加载（适用于 IDE 退出后 token rotation 导致失效的场景）。
            if self.try_reload_credential_from_file(id) {
                return Ok(None);
            }
            tracing::warn!("凭据 #{} refreshToken 永久失效: {}", id, error);
            Ok(Some(self.report_refresh_token_invalid(id)))
        } else {
            tracing::warn!("凭据 #{} Token 刷新失败: {}", id, error);
            Ok(Some(self.report_refresh_failure(id)))
        }
    }

    /// 选择优先级最高的未禁用凭据作为当前凭据（内部方法）
    ///
    /// 纯粹按优先级选择，不排除当前凭据，用于优先级变更后立即生效
    fn select_highest_priority(&self) {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（不排除当前凭据）
        if let Some(best) = entries
            .iter()
            .filter(|e| !e.disabled)
            .min_by_key(|e| (e.credentials.priority, e.id))
        {
            if best.id != *current_id {
                tracing::info!(
                    "优先级变更后切换凭据: #{} -> #{}（优先级 {}）",
                    *current_id,
                    best.id,
                    best.credentials.priority
                );
                *current_id = best.id;
            }
        }
    }

    /// 尝试使用指定凭据获取有效 Token
    ///
    /// 使用双重检查锁定模式，确保同一时间只有一个刷新操作
    ///
    /// # Arguments
    /// * `id` - 凭据 ID，用于更新正确的条目
    /// * `credentials` - 凭据信息
    async fn try_ensure_token(
        &self,
        id: u64,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<CallContext> {
        // API Key 凭据直接使用 kiro_api_key 作为 Bearer Token，无需刷新
        if credentials.is_api_key_credential() {
            let token = credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            return Ok(CallContext {
                id,
                credentials: credentials.clone(),
                token,
            });
        }

        // 第一次检查（无锁）：快速判断是否需要刷新
        let needs_refresh = is_token_expired(credentials) || is_token_expiring_soon(credentials);

        let creds = if needs_refresh {
            // 获取刷新锁，确保同一时间只有一个刷新操作
            let _guard = self.refresh_lock.lock().await;

            // 第二次检查：获取锁后重新读取凭据，因为其他请求可能已经完成刷新
            let current_creds = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据 #{} 不存在", id))?
            };

            if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                // 确实需要刷新
                let global_proxy = self.proxy.lock().clone();
                let effective_proxy = current_creds.effective_proxy(global_proxy.as_ref());
                let new_creds =
                    refresh_token(&current_creds, &self.config, effective_proxy.as_ref()).await?;

                if is_token_expired(&new_creds) {
                    anyhow::bail!("刷新后的 Token 仍然无效或已过期");
                }

                // 更新凭据
                {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials = new_creds.clone();
                    }
                }

                // 回写凭据到文件（仅多凭据格式），失败只记录警告
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                }

                new_creds
            } else {
                // 其他请求已经完成刷新，直接使用新凭据
                tracing::debug!("Token 已被其他请求刷新，跳过刷新");
                current_creds
            }
        } else {
            credentials.clone()
        };

        let token = creds
            .access_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("没有可用的 accessToken"))?;

        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.refresh_failure_count = 0;
            }
        }

        Ok(CallContext {
            id,
            credentials: creds,
            token,
        })
    }

    /// 将凭据列表回写到源文件
    ///
    /// 仅在以下条件满足时回写：
    /// - 源文件是多凭据格式（数组）
    /// - credentials_path 已设置
    ///
    /// # Returns
    /// - `Ok(true)` - 成功写入文件
    /// - `Ok(false)` - 跳过写入（非多凭据格式或无路径配置）
    /// - `Err(_)` - 写入失败
    fn persist_credentials(&self) -> anyhow::Result<bool> {
        use anyhow::Context;

        // 仅多凭据格式才回写
        if !self.is_multiple_format.load(Ordering::Relaxed) {
            return Ok(false);
        }

        let path = match &self.credentials_path {
            Some(p) => p,
            None => return Ok(false),
        };

        // 持 persist_lock 覆盖「快照 + 序列化 + 写盘」整个临界区：并发 persist 严格串行，
        // 最后写盘者必在其临界区内重新快照到最新内存，杜绝陈旧快照覆盖已轮换的 token
        // （issue #23 根因）。entries.lock 仅在快照期短暂持有、不跨磁盘 I/O，故不阻塞请求路由。
        // 注：persist_lock 全仓仅此一处获取，且顺序恒为 persist_lock → entries.lock，无死锁。
        let _write_guard = self.persist_lock.lock();

        // 收集所有凭据（在 persist_lock 保护下拍快照，保证与随后的写盘原子）
        let credentials: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(CredentialEntry::credentials_snapshot)
                .collect()
        };

        // 序列化为 pretty JSON
        let json = serde_json::to_string_pretty(&credentials).context("序列化凭据失败")?;

        // 原子落盘：先写临时文件再 rename（同目录 rename 原子），避免崩溃 / 并发导致半截文件。
        let tmp = path.with_extension("json.tmp");
        let write_atomic = || -> std::io::Result<()> {
            std::fs::write(&tmp, &json)?;
            std::fs::rename(&tmp, path)
        };
        let write_result = if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(write_atomic)
        } else {
            write_atomic()
        };
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp); // 清理可能残留的临时文件
            return Err(e).with_context(|| format!("回写凭据文件失败: {:?}", path));
        }

        tracing::debug!("已回写凭据到文件: {:?}", path);
        Ok(true)
    }

    /// 尝试从凭据文件重新加载指定凭据的 Token
    ///
    /// 当 refreshToken 失效 (invalid_grant) 时，检查源文件是否已被其他客户端更新
    /// （例如本地 IDE 退出时刷新了 Token，导致 token rotation）。
    /// 如果文件中存在不同的 refreshToken，更新内存凭据并返回 true。
    ///
    /// # 匹配规则（按优先级）
    /// 1. 文件中与内存凭据 `id` 相同的条目
    /// 2. 文件中与内存凭据 `email` 相同的条目
    /// 3. 文件与内存均只有一个凭据时，直接匹配
    ///
    /// # 更新范围
    /// 仅更新 token 相关字段（refreshToken / accessToken / expiresAt），
    /// 保留代理、region、machineId 等配置不变。
    fn try_reload_credential_from_file(&self, id: u64) -> bool {
        use crate::kiro::model::credentials::CredentialsConfig;

        let path = match self.credentials_path.as_ref() {
            Some(p) => p.clone(),
            None => return false,
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return false,
        };

        let file_config: CredentialsConfig = match serde_json::from_str(&content) {
            Ok(c) => c,
            Err(_) => return false,
        };

        let file_creds = file_config.into_sorted_credentials();
        if file_creds.is_empty() {
            return false;
        }

        // 先读取当前凭据的身份信息（不持有锁，避免死锁）
        let (current_cred_id, current_email, current_refresh_token, entries_len) = {
            let entries = self.entries.lock();
            match entries.iter().find(|e| e.id == id) {
                Some(entry) => (
                    entry.credentials.id,
                    entry.credentials.email.clone(),
                    entry.credentials.refresh_token.clone(),
                    entries.len(),
                ),
                None => return false,
            }
        };

        // 从文件中查找对应凭据
        let matched = file_creds
            .iter()
            .find(|fc| {
                if fc.id.is_some() && fc.id == current_cred_id {
                    return true;
                }
                if fc.email.is_some() && fc.email == current_email {
                    return true;
                }
                false
            })
            .or_else(|| {
                if file_creds.len() == 1 && entries_len == 1 {
                    file_creds.first()
                } else {
                    None
                }
            });

        let file_cred = match matched {
            Some(c) => c,
            None => return false,
        };

        // 文件中的 refreshToken 必须存在且与当前不同，才值得更新
        if file_cred.refresh_token.is_none() || file_cred.refresh_token == current_refresh_token {
            return false;
        }

        let new_refresh_token = file_cred.refresh_token.clone();
        let new_access_token = file_cred.access_token.clone();
        let new_expires_at = file_cred.expires_at.clone();

        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials.refresh_token = new_refresh_token;
                entry.credentials.access_token = new_access_token;
                entry.credentials.expires_at = new_expires_at;
                entry.disabled = false;
                entry.disabled_reason = None;
                entry.refresh_failure_count = 0;
                entry.failure_count = 0;
            }
        }

        self.invalidate_model_cache(id);

        tracing::info!(
            "凭据 #{} 从文件检测到新 refreshToken（疑似 IDE token rotation），已自动恢复，将重试",
            id
        );
        true
    }

    /// 获取缓存目录（凭据文件所在目录）
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.credentials_path.as_ref().and_then(|p| {
            p.parent().map(|d| {
                // 当传入相对路径如 "credentials.json"（无目录前缀）时 parent 为空串，
                // 直接 join 出来的子路径会落到 CWD，且 read_dir("") 会报错导致历史日志重建为 0。
                // 这里归一化为 "."，保证 join / read_dir 行为正确。
                if d.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    d.to_path_buf()
                }
            })
        })
    }

    /// 统计数据文件路径
    fn stats_path(&self) -> Option<PathBuf> {
        self.cache_dir().map(|d| d.join("kiro_stats.json"))
    }

    /// 从磁盘加载统计数据并应用到当前条目
    fn load_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return, // 首次运行时文件不存在
        };

        let stats: HashMap<String, StatsEntry> = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("解析统计缓存失败，将忽略: {}", e);
                return;
            }
        };

        let mut entries = self.entries.lock();
        for entry in entries.iter_mut() {
            if let Some(s) = stats.get(&entry.id.to_string()) {
                entry.success_count = s.success_count;
                entry.total_failure_count = s.total_failure_count;
                entry.last_used_at = s.last_used_at.clone();
            }
        }
        *self.last_stats_save_at.lock() = Some(Instant::now());
        self.stats_dirty.store(false, Ordering::Relaxed);
        tracing::info!("已从缓存加载 {} 条统计数据", stats.len());
    }

    /// 将当前统计数据持久化到磁盘
    fn save_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let stats: HashMap<String, StatsEntry> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    (
                        e.id.to_string(),
                        StatsEntry {
                            success_count: e.success_count,
                            total_failure_count: e.total_failure_count,
                            last_used_at: e.last_used_at.clone(),
                        },
                    )
                })
                .collect()
        };

        match serde_json::to_string_pretty(&stats) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!("保存统计缓存失败: {}", e);
                } else {
                    *self.last_stats_save_at.lock() = Some(Instant::now());
                    self.stats_dirty.store(false, Ordering::Relaxed);
                }
            }
            Err(e) => tracing::warn!("序列化统计数据失败: {}", e),
        }
    }

    /// 标记统计数据已更新，并按 debounce 策略决定是否立即落盘
    fn save_stats_debounced(&self) {
        self.stats_dirty.store(true, Ordering::Relaxed);

        let should_flush = {
            let last = *self.last_stats_save_at.lock();
            match last {
                Some(last_saved_at) => last_saved_at.elapsed() >= STATS_SAVE_DEBOUNCE,
                None => true,
            }
        };

        if should_flush {
            self.save_stats();
        }
    }

    /// 报告指定凭据 API 调用成功
    ///
    /// 重置该凭据的失败计数
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    #[cfg(test)]
    pub fn report_success(&self, id: u64) {
        self.report_success_for_request(id, None);
    }

    /// 报告指定请求模型上的成功。只清零同一凭据、同一模型的连续自愈轮数。
    pub fn report_success_for_request(&self, id: u64, model: Option<&str>) {
        let requested_model = normalize_self_heal_model(model);
        let reset_persisted_self_heal = {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.success_count += 1;
                entry.last_used_at = Some(Utc::now().to_rfc3339());
                // 成功 = 风控已解除，提前结束冷却
                entry.throttled_until = None;
                tracing::debug!(
                    "凭据 #{} API 调用成功（累计 {} 次）",
                    id,
                    entry.success_count
                );
                if entry.self_heal_consecutive_rounds > 0
                    && entry.self_heal_model == requested_model
                {
                    entry.clear_self_heal_streak();
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if reset_persisted_self_heal {
            if let Err(error) = self.persist_credentials() {
                tracing::warn!("凭据 #{} 成功后持久化自愈状态失败: {}", id, error);
            }
        }
        self.save_stats_debounced();
    }

    /// 报告指定凭据 API 调用失败
    ///
    /// 增加失败计数，达到阈值时禁用凭据并切换到优先级最高的可用凭据
    /// 返回是否还有可用凭据可以重试
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    #[cfg(test)]
    pub fn report_failure(&self, id: u64) -> bool {
        self.report_failure_for_request(id, None, None)
    }

    /// 报告指定请求作用域内的凭据失败，并返回该作用域是否还有可用凭据。
    pub fn report_failure_for_request(
        &self,
        id: u64,
        model: Option<&str>,
        group: Option<&str>,
    ) -> bool {
        let (result, newly_disabled) = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let Some(entry_index) = entries.iter().position(|entry| entry.id == id) else {
                return self.has_available_for_request(&entries, model, group);
            };

            if entries[entry_index].disabled {
                return self.has_available_for_request(&entries, model, group);
            }

            entries[entry_index].failure_count += 1;
            entries[entry_index].total_failure_count += 1;
            entries[entry_index].last_used_at = Some(Utc::now().to_rfc3339());
            let failure_count = entries[entry_index].failure_count;

            tracing::warn!(
                "凭据 #{} API 调用失败（{}/{}）",
                id,
                failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            let mut disabled_now = false;
            if failure_count >= MAX_FAILURES_PER_CREDENTIAL {
                entries[entry_index].disabled = true;
                entries[entry_index].disabled_reason = Some(DisabledReason::TooManyFailures);
                disabled_now = true;
                tracing::error!("凭据 #{} 已连续失败 {} 次，已被禁用", id, failure_count);

                if let Some(next) = entries
                    .iter()
                    .filter(|entry| {
                        self.entry_available_for_request(entry, model, group, Instant::now())
                    })
                    .min_by_key(|e| (e.credentials.priority, e.id))
                {
                    *current_id = next.id;
                    tracing::info!(
                        "已切换到凭据 #{}（优先级 {}）",
                        next.id,
                        next.credentials.priority
                    );
                } else {
                    tracing::error!("所有凭据均已禁用！");
                }
            }

            (
                self.has_available_for_request(&entries, model, group),
                disabled_now,
            )
        };
        if newly_disabled {
            if let Err(error) = self.persist_credentials() {
                tracing::warn!("凭据 #{} 自动禁用状态持久化失败: {}", id, error);
            }
        }
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据被上游封禁/停用（403 + 明确封禁文案）。
    ///
    /// 立即禁用并标记 [`DisabledReason::Suspended`]，**不累计、不参与自愈**——
    /// 账号封禁是不可自动恢复的终态，无脑自愈复活只会立刻再次 403 形成死循环
    /// （issue #51）。误判可经 Admin API 手动重置。切换到下一个可用凭据。
    /// 返回是否还有可用凭据可以重试。
    #[cfg(test)]
    pub fn report_suspended(&self, id: u64) -> bool {
        self.report_suspended_for_request(id, None, None)
    }

    pub fn report_suspended_for_request(
        &self,
        id: u64,
        model: Option<&str>,
        group: Option<&str>,
    ) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let Some(entry_index) = entries.iter().position(|entry| entry.id == id) else {
                return self.has_available_for_request(&entries, model, group);
            };

            if entries[entry_index].disabled {
                return self.has_available_for_request(&entries, model, group);
            }

            let entry = &mut entries[entry_index];
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::Suspended);
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.clear_self_heal_streak();
            // 设为阈值，便于在管理面板中直观看到该凭据已不可用
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;
            entry.total_failure_count += 1;

            tracing::error!(
                "凭据 #{} 被上游封禁/停用（账号 suspended），已禁用且不参与自愈，请人工联系客服核实后在管理面板手动重置",
                id
            );

            if let Some(next) = entries
                .iter()
                .filter(|entry| {
                    self.entry_available_for_request(entry, model, group, Instant::now())
                })
                .min_by_key(|e| (e.credentials.priority, e.id))
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        if let Err(error) = self.persist_credentials() {
            tracing::warn!("凭据 #{} 封禁状态持久化失败: {}", id, error);
        }
        self.save_stats_debounced();
        result
    }

    /// 是否启用 403 封禁文案识别（provider 调用，决定 403 是否走 report_suspended）。
    pub fn get_suspended_detection_enabled(&self) -> bool {
        self.suspended_detection_enabled.load(Ordering::Relaxed)
    }

    /// 受控的凭据自愈。
    ///
    /// 当前请求的 model/group 作用域没有可用凭据时，在以下约束下恢复该作用域内因
    /// [`DisabledReason::TooManyFailures`] 被禁用的凭据：
    /// - `self_heal_enabled` 关闭时不自愈；
    /// - 每个凭据独立计算连续轮数和冷却，状态跨重启持久化；
    /// - 只有同一凭据、同一模型的成功才清零连续轮数；
    /// - 不存在的分组、明确不支持的模型和无关 429 冷却不会触碰其它凭据。
    ///
    /// 仅复活 [`DisabledReason::TooManyFailures`]；手动禁用、额度用尽、token 失效等
    /// 其它原因禁用的凭据不受影响。
    /// 返回本次是否实际执行了自愈（调用方据此决定是否重新选取凭据）。
    fn try_self_heal(&self, model: Option<&str>, group: Option<&str>) -> bool {
        if !self.self_heal_enabled.load(Ordering::Relaxed) {
            tracing::debug!("当前请求没有可用凭据，但自愈已关闭");
            return false;
        }

        let max_rounds = self
            .self_heal_max_consecutive_rounds
            .load(Ordering::Relaxed);
        let min_interval = self.self_heal_min_interval_secs.load(Ordering::Relaxed);
        let requested_model = normalize_self_heal_model(model);
        let now = Utc::now();
        let mut recovered = Vec::new();
        let mut max_blocked = Vec::new();

        {
            let mut entries = self.entries.lock();
            for entry in entries.iter_mut() {
                if !entry.disabled
                    || entry.disabled_reason != Some(DisabledReason::TooManyFailures)
                    || !credential_matches_request(&entry.credentials, model, group)
                    || self.cached_model_support(entry.id, model) == CachedModelSupport::Unsupported
                {
                    continue;
                }

                if entry.self_heal_consecutive_rounds > 0
                    && entry.self_heal_model != requested_model
                {
                    continue;
                }

                if max_rounds > 0 && entry.self_heal_consecutive_rounds >= max_rounds {
                    max_blocked.push((entry.id, entry.self_heal_consecutive_rounds));
                    continue;
                }

                if let Some(previous) = entry.last_self_heal_at {
                    let elapsed = now.signed_duration_since(previous).num_seconds();
                    if elapsed < min_interval as i64 {
                        continue;
                    }
                }

                entry.self_heal_consecutive_rounds =
                    entry.self_heal_consecutive_rounds.saturating_add(1);
                entry.self_heal_total_count = entry.self_heal_total_count.saturating_add(1);
                entry.last_self_heal_at = Some(now);
                entry.self_heal_model = requested_model.clone();
                entry.disabled = false;
                entry.disabled_reason = None;
                entry.failure_count = 0;
                recovered.push((entry.id, entry.self_heal_consecutive_rounds));
            }
        }

        for (id, rounds) in max_blocked {
            tracing::error!(
                "凭据 #{} 已连续自愈 {} 轮仍无成功调用（上限 {}），保持禁用并等待人工处理",
                id,
                rounds,
                max_rounds
            );
        }

        if recovered.is_empty() {
            return false;
        }

        tracing::warn!(
            model = model.unwrap_or("<none>"),
            group = group.unwrap_or("<all>"),
            recovered_count = recovered.len(),
            recovered = ?recovered,
            "当前请求作用域无可用凭据，执行受控自愈"
        );
        if let Err(error) = self.persist_credentials() {
            tracing::warn!("自愈状态持久化失败: {}", error);
        }
        true
    }

    /// 报告指定凭据额度已用尽
    ///
    /// 用于处理 402 Payment Required 且 reason 为 `MONTHLY_REQUEST_COUNT` 的场景：
    /// - 立即禁用该凭据（不等待连续失败阈值）
    /// - 切换到下一个可用凭据继续重试
    /// - 返回是否还有可用凭据
    #[cfg(test)]
    pub fn report_quota_exhausted(&self, id: u64) -> bool {
        self.report_quota_exhausted_for_request(id, None, None)
    }

    pub fn report_quota_exhausted_for_request(
        &self,
        id: u64,
        model: Option<&str>,
        group: Option<&str>,
    ) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let Some(entry_index) = entries.iter().position(|entry| entry.id == id) else {
                return self.has_available_for_request(&entries, model, group);
            };

            if entries[entry_index].disabled {
                return self.has_available_for_request(&entries, model, group);
            }

            let entry = &mut entries[entry_index];
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::QuotaExceeded);
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.clear_self_heal_streak();
            // 设为阈值，便于在管理面板中直观看到该凭据已不可用
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;
            entry.total_failure_count += 1;

            tracing::error!(
                "凭据 #{} 额度已用尽（MONTHLY_REQUEST_COUNT 或 OVERAGE_REQUEST_LIMIT_EXCEEDED），已被禁用",
                id
            );

            if let Some(next) = entries
                .iter()
                .filter(|entry| {
                    self.entry_available_for_request(entry, model, group, Instant::now())
                })
                .min_by_key(|e| (e.credentials.priority, e.id))
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        if let Err(error) = self.persist_credentials() {
            tracing::warn!("凭据 #{} 额度禁用状态持久化失败: {}", id, error);
        }
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据刷新 Token 失败。
    ///
    /// 连续刷新失败达到阈值后禁用凭据并切换，阈值内保持当前凭据不切换，
    /// 与 API 401/403 的累计失败策略保持一致。
    pub fn report_refresh_failure(&self, id: u64) -> bool {
        let (result, newly_disabled) = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.refresh_failure_count += 1;
            let refresh_failure_count = entry.refresh_failure_count;

            tracing::warn!(
                "凭据 #{} Token 刷新失败（{}/{}）",
                id,
                refresh_failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if refresh_failure_count < MAX_FAILURES_PER_CREDENTIAL {
                (entries.iter().any(|e| !e.disabled), false)
            } else {
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::TooManyRefreshFailures);
                entry.clear_self_heal_streak();

                tracing::error!(
                    "凭据 #{} Token 已连续刷新失败 {} 次，已被禁用",
                    id,
                    refresh_failure_count
                );

                let has_available = if let Some(next) = entries
                    .iter()
                    .filter(|e| !e.disabled)
                    .min_by_key(|e| (e.credentials.priority, e.id))
                {
                    *current_id = next.id;
                    tracing::info!(
                        "已切换到凭据 #{}（优先级 {}）",
                        next.id,
                        next.credentials.priority
                    );
                    true
                } else {
                    tracing::error!("所有凭据均已禁用！");
                    false
                };
                (has_available, true)
            }
        };
        if newly_disabled {
            if let Err(error) = self.persist_credentials() {
                tracing::warn!("凭据 #{} Token 刷新禁用状态持久化失败: {}", id, error);
            }
        }
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据的 refreshToken 永久失效（invalid_grant）。
    ///
    /// 立即禁用凭据，不累计、不重试。
    /// 返回是否还有可用凭据。
    pub fn report_refresh_token_invalid(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::InvalidRefreshToken);
            entry.clear_self_heal_streak();

            tracing::error!(
                "凭据 #{} refreshToken 已失效 (invalid_grant)，已立即禁用",
                id
            );

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| (e.credentials.priority, e.id))
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        if let Err(error) = self.persist_credentials() {
            tracing::warn!("凭据 #{} 无效 Token 禁用状态持久化失败: {}", id, error);
        }
        self.save_stats_debounced();
        result
    }

    /// 切换到优先级最高的可用凭据
    ///
    /// 返回是否成功切换
    pub fn switch_to_next(&self) -> bool {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（排除当前凭据）
        if let Some(next) = entries
            .iter()
            .filter(|e| !e.disabled && e.id != *current_id)
            .min_by_key(|e| (e.credentials.priority, e.id))
        {
            *current_id = next.id;
            tracing::info!(
                "已切换到凭据 #{}（优先级 {}）",
                next.id,
                next.credentials.priority
            );
            true
        } else {
            // 没有其他可用凭据，检查当前凭据是否可用
            entries.iter().any(|e| e.id == *current_id && !e.disabled)
        }
    }

    // ========================================================================
    // Admin API 方法
    // ========================================================================

    /// 克隆全部凭据（含敏感字段：refreshToken、accessToken、clientSecret 等）
    ///
    /// 仅用于 Admin API 导出场景，调用方需自行保证脱敏与权限控制。
    /// 返回值按调用时的顺序克隆，未做排序。
    pub fn clone_all_credentials(&self) -> Vec<KiroCredentials> {
        let entries = self.entries.lock();
        entries
            .iter()
            .map(CredentialEntry::credentials_snapshot)
            .collect()
    }

    /// 克隆单个凭据（含敏感字段），不存在时返回 `None`
    ///
    /// 需要读取某个凭据完整配置时用它，避免 `clone_all_credentials` 的全量克隆。
    pub fn clone_credential(&self, id: u64) -> Option<KiroCredentials> {
        let entries = self.entries.lock();
        entries
            .iter()
            .find(|entry| entry.id == id)
            .map(CredentialEntry::credentials_snapshot)
    }

    /// 获取管理器状态快照（用于 Admin API）
    pub fn snapshot(&self) -> ManagerSnapshot {
        let entries = self.entries.lock();
        let current_id = *self.current_id.lock();
        let now = Instant::now();
        let available = entries
            .iter()
            .filter(|e| !e.disabled && !e.throttled_until.map(|t| t > now).unwrap_or(false))
            .count();

        ManagerSnapshot {
            entries: entries
                .iter()
                .map(|e| CredentialEntrySnapshot {
                    id: e.id,
                    priority: e.credentials.priority,
                    disabled: e.disabled,
                    failure_count: e.failure_count,
                    total_failure_count: e.total_failure_count,
                    auth_method: if e.credentials.is_api_key_credential() {
                        Some("api_key".to_string())
                    } else {
                        e.credentials.auth_method.as_deref().map(|m| {
                            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam")
                            {
                                "idc".to_string()
                            } else {
                                m.to_string()
                            }
                        })
                    },
                    provider: if e.credentials.is_api_key_credential() {
                        None
                    } else {
                        e.credentials.provider.clone()
                    },
                    has_profile_arn: e.credentials.profile_arn.is_some(),
                    expires_at: if e.credentials.is_api_key_credential() {
                        None // API Key 凭据本地不维护过期时间（服务端策略未知）
                    } else {
                        e.credentials.expires_at.clone()
                    },
                    refresh_token_hash: if e.credentials.is_api_key_credential() {
                        None
                    } else {
                        e.credentials.refresh_token.as_deref().map(sha256_hex)
                    },
                    api_key_hash: if e.credentials.is_api_key_credential() {
                        e.credentials.kiro_api_key.as_deref().map(sha256_hex)
                    } else {
                        None
                    },
                    masked_api_key: if e.credentials.is_api_key_credential() {
                        e.credentials.kiro_api_key.as_deref().map(mask_api_key)
                    } else {
                        None
                    },
                    email: e.credentials.email.clone(),
                    subscription_title: e.credentials.subscription_title.clone(),
                    success_count: e.success_count,
                    last_used_at: e.last_used_at.clone(),
                    has_proxy: e.credentials.proxy_url.is_some(),
                    proxy_url: e.credentials.proxy_url.clone(),
                    refresh_failure_count: e.refresh_failure_count,
                    disabled_reason: e.disabled_reason.map(|reason| reason.as_str().to_string()),
                    throttled_remaining_secs: e
                        .throttled_until
                        .and_then(|t| t.checked_duration_since(now))
                        .map(|d| d.as_secs())
                        .filter(|s| *s > 0),
                    endpoint: e.credentials.endpoint.clone(),
                    groups: e.credentials.groups.clone(),
                    source_channel: e.credentials.source_channel.clone(),
                    metadata: e.credentials.metadata.clone(),
                    created_at: e.credentials.created_at.clone(),
                })
                .collect(),
            current_id,
            total: entries.len(),
            available,
        }
    }

    /// 设置凭据禁用状态（Admin API）
    pub fn set_disabled(&self, id: u64, disabled: bool) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.disabled = disabled;
            if !disabled {
                // 启用时重置失败计数
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.disabled_reason = None;
                entry.throttled_until = None;
                entry.clear_self_heal_streak();
            } else {
                entry.disabled_reason = Some(DisabledReason::Manual);
                entry.clear_self_heal_streak();
            }
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 标记凭据进入临时冷却期（账号级 429 风控触发）
    ///
    /// 与 `report_failure` 不同：不计入永久禁用，到期自动恢复，可用于"`suspicious activity` 429"
    /// 这种短期账号级风控——当前凭据先冷却 N 分钟，故障转移到其它凭据。
    ///
    /// 标记凭据冷却，并在同一锁临界区内返回当前请求范围的剩余凭据数。
    pub fn report_account_throttled_for_request(
        &self,
        id: u64,
        cooldown: StdDuration,
        model: Option<&str>,
        group: Option<&str>,
    ) -> usize {
        let now = Instant::now();
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                let until = now + cooldown;
                // 取较晚的到期时间（多次触发时延长冷却）
                entry.throttled_until = Some(match entry.throttled_until {
                    Some(prev) if prev > until => prev,
                    _ => until,
                });
                // 计入累计失败（账号风控不动连续 failure_count，避免冷却结束后误禁用）
                entry.total_failure_count += 1;
                tracing::warn!(
                    "凭据 #{} 触发账号级风控，冷却 {} 秒",
                    id,
                    cooldown.as_secs()
                );
            }

            let throttled_now = Instant::now();
            entries
                .iter()
                .filter(|e| {
                    !e.disabled
                        && !e
                            .throttled_until
                            .map(|t| t > throttled_now)
                            .unwrap_or(false)
                        && credential_matches_request(&e.credentials, model, group)
                })
                .count()
        }
    }

    /// 手动解除指定凭据的临时冷却（Admin API）
    ///
    /// 即使冷却尚未到期也立即清除，让该凭据重新参与调度。
    pub fn clear_throttle(&self, id: u64) -> anyhow::Result<()> {
        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
        entry.throttled_until = None;
        tracing::info!("凭据 #{} 风控冷却已被手动解除", id);
        Ok(())
    }

    /// 以"额度已用尽"为原因禁用凭据（Admin 一键超额功能）
    ///
    /// 与手动禁用不同，原因记录为 `QuotaExceeded`，便于自愈逻辑识别。
    pub fn disable_quota_exceeded(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::QuotaExceeded);
            entry.clear_self_heal_streak();
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据优先级（Admin API）
    ///
    /// 修改优先级后会立即按新优先级重新选择当前凭据。
    /// 即使持久化失败，内存中的优先级和当前凭据选择也会生效。
    pub fn set_priority(&self, id: u64, priority: u32) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.priority = priority;
        }
        // 立即按新优先级重新选择当前凭据（无论持久化是否成功）
        self.select_highest_priority();
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 重置凭据失败计数并重新启用（Admin API）
    pub fn reset_and_enable(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            if entry.disabled_reason == Some(DisabledReason::InvalidConfig) {
                anyhow::bail!("凭据 #{} 因配置无效被禁用，请修正配置后重启服务", id);
            }
            entry.reset_health();
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    pub fn reset_success_count(&self, id: Option<u64>) -> anyhow::Result<u32> {
        let mut count = 0u32;
        {
            let mut entries = self.entries.lock();
            match id {
                Some(target_id) => {
                    let entry = entries
                        .iter_mut()
                        .find(|e| e.id == target_id)
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", target_id))?;
                    entry.success_count = 0;
                    count = 1;
                }
                None => {
                    for entry in entries.iter_mut() {
                        entry.success_count = 0;
                        count += 1;
                    }
                }
            }
        }
        self.save_stats();
        Ok(count)
    }

    /// 解析并回填 Enterprise / IdC 账号的真实 profileArn。
    ///
    /// 流式端点（`generateAssistantResponse`）强制要求 profileArn：不带 → 400
    /// `profileArn is required`。Enterprise / IdC 账号若带 BuilderID 占位符会因
    /// token 身份不匹配触发 403，真实 profileArn 只能通过 `ListAvailableProfiles` 获取。
    ///
    /// 行为：
    /// - API Key 凭据 / 已有真实（非占位符）profileArn → 直接返回，不发起网络请求；
    /// - 否则调用上游 `ListAvailableProfiles`，命中真实 ARN 时写回凭据并持久化；
    /// - 上游无 profile（如纯 BuilderID 账号）→ 返回 `None`，由调用方回退到占位符。
    ///
    /// 返回应当用于本次请求的 profileArn（`Some` 表示真实 ARN）。
    pub async fn resolve_profile_arn_for(
        &self,
        id: u64,
        token: &str,
    ) -> anyhow::Result<Option<String>> {
        use crate::kiro::model::credentials::is_placeholder_profile_arn;

        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据没有 profileArn 概念
        if credentials.is_api_key_credential() {
            return Ok(None);
        }

        // 已有真实 ARN（含 Social 共享 ARN）→ 直接用，无需查询
        if let Some(arn) = credentials.profile_arn.as_deref() {
            if !is_placeholder_profile_arn(arn) {
                return Ok(Some(arn.to_string()));
            }
        }

        let global_proxy = self.proxy.lock().clone();
        let effective_proxy = credentials.effective_proxy(global_proxy.as_ref());
        let profiles =
            list_available_profiles(&credentials, &self.config, token, effective_proxy.as_ref())
                .await?;

        let Some(arn) = profiles.first_arn().map(|s| s.to_string()) else {
            // 无 Enterprise profile（如纯 BuilderID 账号）：保持占位符回退逻辑
            return Ok(None);
        };

        // 写回真实 ARN 并持久化
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials.profile_arn = Some(arn.clone());
            }
        }
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("profileArn 回填后持久化失败（不影响本次请求）: {}", e);
        }
        tracing::info!("凭据 #{} 已解析并回填真实 profileArn: {}", id, arn);

        Ok(Some(arn))
    }

    /// 获取指定凭据的使用额度（Admin API）
    pub async fn get_usage_limits_for(&self, id: u64) -> anyhow::Result<UsageLimitsResponse> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据直接使用 kiro_api_key，无需刷新
        let token = if credentials.is_api_key_credential() {
            credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?
        } else {
            // 检查是否需要刷新 token
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);

            if needs_refresh {
                let _guard = self.refresh_lock.lock().await;
                let current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };

                if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                    let global_proxy = self.proxy.lock().clone();
                    let effective_proxy = current_creds.effective_proxy(global_proxy.as_ref());
                    let new_creds =
                        refresh_token(&current_creds, &self.config, effective_proxy.as_ref())
                            .await?;
                    {
                        let mut entries = self.entries.lock();
                        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                            entry.credentials = new_creds.clone();
                        }
                    }
                    // 持久化失败只记录警告，不影响本次请求
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                    }
                    new_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
                } else {
                    current_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
                }
            } else {
                credentials
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        };

        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // Enterprise / IdC 账号必须带真实 profileArn，先解析回填；
        // 失败不阻断查询，由 get_usage_limits 内部按候选表回退。
        match self.resolve_profile_arn_for(id, &token).await {
            Ok(Some(arn)) => credentials.profile_arn = Some(arn),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!("凭据 #{} 查询用量前解析 profileArn 失败: {}", id, error)
            }
        }

        let global_proxy = self.proxy.lock().clone();
        let effective_proxy = credentials.effective_proxy(global_proxy.as_ref());
        let usage_limits =
            get_usage_limits(&credentials, &self.config, &token, effective_proxy.as_ref()).await?;

        // 更新订阅等级到凭据（仅在发生变化时持久化）
        if let Some(subscription_title) = usage_limits.subscription_title() {
            let changed = {
                let mut entries = self.entries.lock();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    let old_title = entry.credentials.subscription_title.clone();
                    if old_title.as_deref() != Some(subscription_title) {
                        entry.credentials.subscription_title = Some(subscription_title.to_string());
                        tracing::info!(
                            "凭据 #{} 订阅等级已更新: {:?} -> {}",
                            id,
                            old_title,
                            subscription_title
                        );
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if changed {
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("订阅等级更新后持久化失败（不影响本次请求）: {}", e);
                }
            }
        }

        // 回填邮箱：仅在凭据尚无邮箱、且上游返回了邮箱时写入
        if let Some(email) = usage_limits.email() {
            let changed = {
                let mut entries = self.entries.lock();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    let is_empty = entry
                        .credentials
                        .email
                        .as_deref()
                        .map(|s| s.is_empty())
                        .unwrap_or(true);
                    if is_empty {
                        entry.credentials.email = Some(email.to_string());
                        tracing::info!("凭据 #{} 邮箱已回填: {}", id, email);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if changed {
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("邮箱回填后持久化失败（不影响本次请求）: {}", e);
                }
            }
        }

        Ok(usage_limits)
    }

    /// 为只读型上游查询准备有效 token 与最新凭据快照
    ///
    /// 复用 [`Self::get_usage_limits_for`] 的 token 准备流程：API Key 凭据直接用
    /// kiroApiKey；OAuth 凭据按需在 `refresh_lock` 内刷新并持久化。返回的凭据是
    /// 刷新后重新读取的最新快照，调用方据此构造请求。
    async fn prepare_request_token(&self, id: u64) -> anyhow::Result<(String, KiroCredentials)> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据直接使用 kiro_api_key，无需刷新
        let token = if credentials.is_api_key_credential() {
            credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?
        } else if is_token_expired(&credentials) || is_token_expiring_soon(&credentials) {
            let _guard = self.refresh_lock.lock().await;
            let current_creds = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
            };

            if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                let global_proxy = self.proxy.lock().clone();
                let effective_proxy = current_creds.effective_proxy(global_proxy.as_ref());
                let new_creds =
                    refresh_token(&current_creds, &self.config, effective_proxy.as_ref()).await?;
                {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials = new_creds.clone();
                    }
                }
                // 持久化失败只记录警告，不影响本次请求
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                }
                new_creds
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
            } else {
                current_creds
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        } else {
            credentials
                .access_token
                .clone()
                .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
        };

        // 重新读取最新凭据（刷新可能改写了 access_token 之外的字段）
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        Ok((token, credentials))
    }

    /// 获取指定凭据当前可用的模型列表（Admin API）。
    ///
    /// Admin 查询保持实时语义；成功结果同时更新该凭据的共享缓存。
    pub async fn get_available_models_for(
        &self,
        id: u64,
    ) -> anyhow::Result<ListAvailableModelsResponse> {
        self.refresh_model_cache_for(id, true).await
    }

    /// 使用账号池当前选中的可用凭据实时查询模型列表（Admin 全局模型视图）。
    ///
    /// 凭据选择复用正常请求的账号池规则：priority 模式优先当前凭据，balanced
    /// 模式按均衡策略选择；失效 Token 会在查询前刷新。返回实际命中的凭据 ID，
    /// 供管理前端明确展示数据来源。
    pub async fn get_available_models_for_current(
        &self,
    ) -> anyhow::Result<(u64, ListAvailableModelsResponse, bool)> {
        let (context, is_balanced) = self.acquire_context_impl(None, None, false).await?;
        let id = context.id;
        let response = self.refresh_model_cache_for(id, true).await?;
        Ok((id, response, is_balanced))
    }

    /// 设置用户偏好（开启/关闭超额）— Admin API
    ///
    /// 与 `get_usage_limits_for` 类似的 token 准备流程，最后调用上游
    /// `setUserPreference` 接口写入新的 `overageStatus`。
    pub async fn set_user_preference_for(
        &self,
        id: u64,
        overage_status: &str,
    ) -> anyhow::Result<()> {
        // 仅接受 "ENABLED" / "DISABLED"，其它值早 fail
        if overage_status != "ENABLED" && overage_status != "DISABLED" {
            anyhow::bail!("overageStatus 必须是 ENABLED 或 DISABLED");
        }

        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据：直接当 Bearer 用
        let token = if credentials.is_api_key_credential() {
            credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?
        } else {
            // 复用与 get_usage_limits_for 完全相同的过期检查与刷新逻辑
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);

            if needs_refresh {
                let _guard = self.refresh_lock.lock().await;
                let current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };

                if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                    let global_proxy = self.proxy.lock().clone();
                    let effective_proxy = current_creds.effective_proxy(global_proxy.as_ref());
                    let new_creds =
                        refresh_token(&current_creds, &self.config, effective_proxy.as_ref())
                            .await?;
                    {
                        let mut entries = self.entries.lock();
                        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                            entry.credentials = new_creds.clone();
                        }
                    }
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                    }
                    new_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
                } else {
                    current_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
                }
            } else {
                credentials
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        };

        // 重新读取最新的凭据快照（refresh 可能已修改 access_token 之外的字段）
        let mut credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // Enterprise / IdC 账号必须带真实 profileArn，先解析回填；
        // 解析失败不阻断请求，保留无 ARN 的 BuilderID 兼容路径。
        match self.resolve_profile_arn_for(id, &token).await {
            Ok(Some(arn)) => credentials.profile_arn = Some(arn),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!("凭据 #{} 设置用户偏好前解析 profileArn 失败: {}", id, error)
            }
        }

        let global_proxy = self.proxy.lock().clone();
        let effective_proxy = credentials.effective_proxy(global_proxy.as_ref());
        set_user_preference(
            &credentials,
            &self.config,
            &token,
            effective_proxy.as_ref(),
            overage_status,
        )
        .await
    }

    /// 添加新凭据（Admin API）
    ///
    /// # 流程
    /// 1. 验证凭据基本字段（API Key: kiroApiKey 不为空; OAuth: refreshToken 不为空）
    /// 2. 基于 kiroApiKey 或 refreshToken 的 SHA-256 哈希检测重复
    /// 3. OAuth: 尝试刷新 Token 验证凭据有效性; API Key: 跳过
    /// 4. 分配新 ID（当前最大 ID + 1）
    /// 5. 添加到 entries 列表
    /// 6. 持久化到配置文件
    ///
    /// # 返回
    /// - `Ok(u64)` - 新凭据 ID
    /// - `Err(_)` - 验证失败或添加失败
    pub async fn add_credential(&self, new_cred: KiroCredentials) -> anyhow::Result<u64> {
        // 1. 基本验证
        if new_cred.is_api_key_credential() {
            let api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            if api_key.is_empty() {
                anyhow::bail!("kiroApiKey 为空");
            }
        } else {
            validate_refresh_token(&new_cred)?;
        }

        // 2. 基于哈希检测重复
        if new_cred.is_api_key_credential() {
            let new_api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 kiroApiKey"))?;
            let new_api_key_hash = sha256_hex(new_api_key);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .kiro_api_key
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_api_key_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（kiroApiKey 重复）");
            }
        } else {
            let new_refresh_token = new_cred
                .refresh_token
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;
            let new_refresh_token_hash = sha256_hex(new_refresh_token);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .refresh_token
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_refresh_token_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（refreshToken 重复）");
            }
        }

        // 3. 验证凭据有效性（API Key 无需网络刷新）
        let mut validated_cred = if new_cred.is_api_key_credential() {
            new_cred.clone()
        } else {
            let global_proxy = self.proxy.lock().clone();
            let effective_proxy = new_cred.effective_proxy(global_proxy.as_ref());
            refresh_token(&new_cred, &self.config, effective_proxy.as_ref()).await?
        };

        // 捕获原始输入的去重指纹。刷新可能轮换 refreshToken，且下方 step 5 会把
        // new_cred 的字段 move 走，故必须在此处（字段尚完整时）取指纹，
        // 供插入临界区的权威去重重检使用。
        let dedup_is_api_key = new_cred.is_api_key_credential();
        let dedup_hash: Option<String> = if dedup_is_api_key {
            new_cred
                .kiro_api_key
                .as_deref()
                .filter(|k| !k.is_empty())
                .map(sha256_hex)
        } else {
            new_cred.refresh_token.as_deref().map(sha256_hex)
        };

        // 4. 分配新 ID。必须使用单调计数器，不能按当前 entries 最大值重算；
        // 否则删除最后一个账号后再添加会复用旧 ID，导致 trace/usage/kiro_stats
        // 这类按 credential_id 聚合的历史被新账号继承。
        let new_id = self.next_id.fetch_add(1, Ordering::Relaxed);

        // 5. 设置 ID 并保留用户输入的元数据
        validated_cred.id = Some(new_id);
        validated_cred.priority = new_cred.priority;
        validated_cred.auth_method = new_cred.auth_method.as_deref().map(|m| {
            crate::kiro::model::credentials::canonicalize_auth_method_value(m).to_string()
        });
        if new_cred.profile_arn.is_some() {
            validated_cred.profile_arn = new_cred.profile_arn;
        }
        validated_cred.provider = new_cred.provider;
        validated_cred.fill_default_profile_arn();
        validated_cred.client_id = new_cred.client_id;
        validated_cred.client_secret = new_cred.client_secret;
        validated_cred.token_endpoint = new_cred.token_endpoint;
        validated_cred.issuer_url = new_cred.issuer_url;
        validated_cred.scopes = new_cred.scopes;
        validated_cred.region = new_cred.region;
        validated_cred.auth_region = new_cred.auth_region;
        validated_cred.api_region = new_cred.api_region;
        validated_cred.machine_id = new_cred.machine_id;
        validated_cred.email = new_cred.email;
        validated_cred.proxy_url = new_cred.proxy_url;
        validated_cred.proxy_username = new_cred.proxy_username;
        validated_cred.proxy_password = new_cred.proxy_password;
        validated_cred.kiro_api_key = new_cred.kiro_api_key;
        validated_cred.metadata = new_cred.metadata;
        // 记录添加时间：保留导入时携带的原值（如 KAM 迁移），否则以当前时间入库。
        // 此处为所有添加路径（单条添加 / 批量导入 / 登录回调）的唯一收口。
        if validated_cred.created_at.is_none() {
            validated_cred.created_at = new_cred
                .created_at
                .or_else(|| Some(Utc::now().to_rfc3339()));
        }

        {
            let mut entries = self.entries.lock();
            // 并发安全：token 刷新（网络）在锁外完成，期间可能有其它并发的
            // add_credential 通过了步骤 2 的预去重并已插入同一凭据。故在持锁的
            // 插入点用原始输入指纹再做一次权威去重，关闭 TOCTOU（如命中则 bail，
            // next_id 即便已自增也只是跳号，无副作用）。
            if let Some(hash) = &dedup_hash {
                let dup = entries.iter().any(|e| {
                    let entry_hash = if dedup_is_api_key {
                        e.credentials.kiro_api_key.as_deref().map(sha256_hex)
                    } else {
                        e.credentials.refresh_token.as_deref().map(sha256_hex)
                    };
                    entry_hash.as_deref() == Some(hash.as_str())
                });
                if dup {
                    let msg = if dedup_is_api_key {
                        "凭据已存在（kiroApiKey 重复）"
                    } else {
                        "凭据已存在（refreshToken 重复）"
                    };
                    anyhow::bail!(msg);
                }
            }
            entries.push(CredentialEntry {
                id: new_id,
                credentials: validated_cred,
                failure_count: 0,
                total_failure_count: 0,
                refresh_failure_count: 0,
                disabled: false,
                disabled_reason: None,
                success_count: 0,
                last_used_at: None,
                throttled_until: None,
                rpm_window: VecDeque::new(),
                self_heal_consecutive_rounds: 0,
                self_heal_total_count: 0,
                last_self_heal_at: None,
                self_heal_model: None,
            });
        }

        // 新凭据可能比当前凭据优先级更高，入库后立即重选，确保 priority 模式
        // 不会继续粘滞在旧的低优先级凭据上。即使后续持久化失败，内存中的
        // entries 与 current_id 也应保持一致。
        self.select_highest_priority();

        // 6. 升级为多凭据格式（确保后续 token rotation 能写盘）并持久化
        self.is_multiple_format.store(true, Ordering::Relaxed);
        self.persist_credentials()?;

        tracing::info!("成功添加凭据 #{}", new_id);
        Ok(new_id)
    }

    /// 更新凭据的可编辑字段（Admin API）
    ///
    /// 支持更新 email、proxy_url、proxy_username、proxy_password。
    /// 传 `None` 表示不修改该字段，传 `Some("")` 表示清除该字段。
    pub fn update_credential(
        &self,
        id: u64,
        email: Option<Option<String>>,
        proxy_url: Option<Option<String>>,
        proxy_username: Option<Option<String>>,
        proxy_password: Option<Option<String>>,
        groups: Option<Vec<String>>,
        source_channel: Option<Option<String>>,
        metadata: Option<crate::kiro::model::credentials::CredentialMetadata>,
    ) -> anyhow::Result<()> {
        let invalidate_models =
            proxy_url.is_some() || proxy_username.is_some() || proxy_password.is_some();
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;

            if let Some(v) = email {
                entry.credentials.email = v.filter(|s| !s.is_empty());
            }
            if let Some(v) = proxy_url {
                entry.credentials.proxy_url = v.filter(|s| !s.is_empty());
            }
            if let Some(v) = proxy_username {
                entry.credentials.proxy_username = v.filter(|s| !s.is_empty());
            }
            if let Some(v) = proxy_password {
                entry.credentials.proxy_password = v.filter(|s| !s.is_empty());
            }
            if let Some(g) = groups {
                entry.credentials.groups = g
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            if let Some(v) = source_channel {
                entry.credentials.source_channel =
                    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            }
            if let Some(v) = metadata {
                entry.credentials.metadata = v;
            }
        }
        if invalidate_models {
            self.invalidate_model_cache(id);
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 列出所有凭据当前引用的分组名（去重排序）。
    /// 用于启动迁移到 GroupManager 注册表，以及前端的引用计数显示。
    pub fn list_credential_groups(&self) -> Vec<String> {
        let entries = self.entries.lock();
        let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in entries.iter() {
            for g in &e.credentials.groups {
                if !g.is_empty() {
                    set.insert(g.clone());
                }
            }
        }
        let mut list: Vec<String> = set.into_iter().collect();
        list.sort();
        list
    }

    /// 统计指定分组被多少个凭据引用（用于分组管理页 / 删除前提示）。
    pub fn count_credentials_with_group(&self, group: &str) -> usize {
        let entries = self.entries.lock();
        entries
            .iter()
            .filter(|e| e.credentials.groups.iter().any(|g| g == group))
            .count()
    }

    /// 把所有凭据 `groups` 字段中等于 `old` 的元素改为 `new`（分组改名级联用）。
    /// 已经显式带 `new` 的凭据不会重复添加。返回受影响的凭据数。
    pub fn rename_credential_group(&self, old: &str, new: &str) -> anyhow::Result<usize> {
        let mut affected = 0usize;
        {
            let mut entries = self.entries.lock();
            for entry in entries.iter_mut() {
                let groups = &mut entry.credentials.groups;
                let mut hit = false;
                let mut already_has_new = false;
                for g in groups.iter() {
                    if g == old {
                        hit = true;
                    }
                    if g == new {
                        already_has_new = true;
                    }
                }
                if hit {
                    if already_has_new {
                        // old 和 new 共存：只去掉 old，避免重复
                        groups.retain(|g| g != old);
                    } else {
                        for g in groups.iter_mut() {
                            if g == old {
                                *g = new.to_string();
                            }
                        }
                    }
                    affected += 1;
                }
            }
        }
        if affected > 0 {
            self.persist_credentials()?;
        }
        Ok(affected)
    }

    /// 把 `name` 这个分组从所有凭据的 `groups` 字段中移除（强删分组级联用）。
    /// 返回受影响的凭据数。
    pub fn remove_credential_group(&self, name: &str) -> anyhow::Result<usize> {
        let mut affected = 0usize;
        {
            let mut entries = self.entries.lock();
            for entry in entries.iter_mut() {
                let before = entry.credentials.groups.len();
                entry.credentials.groups.retain(|g| g != name);
                if entry.credentials.groups.len() != before {
                    affected += 1;
                }
            }
        }
        if affected > 0 {
            self.persist_credentials()?;
        }
        Ok(affected)
    }

    /// 删除凭据（Admin API）
    ///
    /// # 前置条件
    /// - 凭据必须已禁用（disabled = true）
    ///
    /// # 行为
    /// 1. 验证凭据存在
    /// 2. 验证凭据已禁用
    /// 3. 从 entries 移除
    /// 4. 如果删除的是当前凭据，切换到优先级最高的可用凭据
    /// 5. 如果删除后没有凭据，将 current_id 重置为 0
    /// 6. 持久化到文件
    ///
    /// # 返回
    /// - `Ok(())` - 删除成功
    /// - `Err(_)` - 凭据不存在或持久化失败
    pub fn delete_credential(&self, id: u64) -> anyhow::Result<()> {
        let was_current = {
            let mut entries = self.entries.lock();

            // 查找凭据
            let _entry = entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;

            // 记录是否是当前凭据
            let current_id = *self.current_id.lock();
            let was_current = current_id == id;

            // 删除凭据
            entries.retain(|e| e.id != id);

            was_current
        };

        // 如果删除的是当前凭据，切换到优先级最高的可用凭据
        if was_current {
            self.select_highest_priority();
        }

        // 如果删除后没有任何凭据，将 current_id 重置为 0（与初始化行为保持一致）
        {
            let entries = self.entries.lock();
            if entries.is_empty() {
                let mut current_id = self.current_id.lock();
                *current_id = 0;
                tracing::info!("所有凭据已删除，current_id 已重置为 0");
            }
        }

        self.remove_model_cache(id);

        // 持久化更改
        self.persist_credentials()?;

        // 立即回写统计数据，清除已删除凭据的残留条目
        self.save_stats();

        tracing::info!("已删除凭据 #{}", id);
        Ok(())
    }

    /// 更新指定凭据的 refreshToken（Admin API）
    ///
    /// # 前置条件
    /// - 凭据必须已禁用（disabled = true），防止意外覆盖正在使用的 Token
    ///
    /// # 行为
    /// 1. 验证凭据存在且已禁用
    /// 2. 验证新 refreshToken 格式
    /// 3. 更新 refreshToken
    /// 4. 重置 refresh_failure_count（保持 disabled 状态，让用户手动启用）
    /// 5. 持久化到文件
    pub fn update_refresh_token(
        &self,
        id: u64,
        new_refresh_token: String,
        new_access_token: Option<String>,
        new_expires_at: Option<String>,
    ) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();

            // 用索引定位，避免两次线性扫描和后续 unwrap
            let idx = entries
                .iter()
                .position(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;

            if !entries[idx].disabled {
                anyhow::bail!(
                    "只能为已禁用的凭据更新 refreshToken（请先禁用凭据 #{}）",
                    id
                );
            }

            // 验证新 refreshToken 格式
            validate_refresh_token_str(&new_refresh_token)?;

            // 检查是否与现有其他凭据重复
            if refresh_token_duplicate_exists(&entries, &new_refresh_token, Some(idx)) {
                anyhow::bail!("refreshToken 与其他凭据重复");
            }

            let entry = &mut entries[idx];
            entry.credentials.refresh_token = Some(new_refresh_token);
            // 若调用方提供了 accessToken（来自导入/导出），则直接保留，无需立即调认证服务器
            // 否则清空，下次使用时系统会自动刷新
            entry.credentials.access_token = new_access_token;
            entry.credentials.expires_at = new_expires_at;
            entry.refresh_failure_count = 0;
        }
        self.invalidate_model_cache(id);
        self.persist_credentials()?;
        tracing::info!("凭据 #{} refreshToken 已更新", id);
        Ok(())
    }

    /// Replaces an existing account with the complete credential set from one AWS IdC login.
    pub async fn replace_idc_relogin_credentials(
        &self,
        id: u64,
        update: IdcReloginCredentials,
    ) -> anyhow::Result<()> {
        validate_refresh_token_str(&update.refresh_token)?;

        if update.access_token.is_empty()
            || update.client_id.is_empty()
            || update.client_secret.is_empty()
            || update.region.is_empty()
            || update.start_url.is_empty()
        {
            anyhow::bail!("IdC 重新登录返回的凭据不完整");
        }

        {
            // The refresh token is bound to its OIDC client registration. Serialize the
            // mutation with refreshes so an in-flight old refresh cannot overwrite this login.
            // Released before persisting so disk I/O does not stall every other refresh.
            let _guard = self.refresh_lock.lock().await;
            let mut entries = self.entries.lock();
            let idx = entries
                .iter()
                .position(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;

            if refresh_token_duplicate_exists(&entries, &update.refresh_token, Some(idx)) {
                anyhow::bail!("refreshToken 与其他凭据重复");
            }

            let entry = &mut entries[idx];
            entry.credentials.access_token = Some(update.access_token);
            entry.credentials.refresh_token = Some(update.refresh_token);
            entry.credentials.expires_at = update.expires_at;
            entry.credentials.auth_method = Some("idc".to_string());
            entry.credentials.provider = Some(update.provider);
            entry.credentials.client_id = Some(update.client_id);
            entry.credentials.client_secret = Some(update.client_secret);
            entry.credentials.region = Some(update.region.clone());
            entry.credentials.auth_region = Some(update.region);
            entry.credentials.start_url = Some(update.start_url);
            entry.credentials.profile_arn = None;
            entry.credentials.token_endpoint = None;
            entry.credentials.issuer_url = None;
            entry.credentials.scopes = None;
            entry.credentials.kiro_api_key = None;

            entry.reset_health();
        }

        self.invalidate_model_cache(id);
        self.persist_credentials()?;
        self.select_highest_priority();
        tracing::info!("凭据 #{} IdC 登录凭据已完整更新", id);
        Ok(())
    }

    /// 强制刷新指定凭据的 Token（Admin API）
    ///
    /// 无条件调用上游 API 重新获取 access token，不检查是否过期。
    /// 适用于排查问题、Token 异常但未过期、主动更新凭据状态等场景。
    pub async fn force_refresh_token_for(&self, id: u64) -> anyhow::Result<()> {
        {
            // Read after locking so token rotation or relogin cannot leave this refresh with a
            // stale refresh token or OIDC client registration snapshot.
            let _guard = self.refresh_lock.lock().await;
            let credentials = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
            };

            // 无条件调用 refresh_token
            let global_proxy = self.proxy.lock().clone();
            let effective_proxy = credentials.effective_proxy(global_proxy.as_ref());
            let new_creds =
                refresh_token(&credentials, &self.config, effective_proxy.as_ref()).await?;

            // 更新 entries 中对应凭据
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials = new_creds;
                entry.refresh_failure_count = 0;
            }
        }
        self.invalidate_model_cache(id);

        // 持久化
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("强制刷新 Token 后持久化失败: {}", e);
        }

        tracing::info!("凭据 #{} Token 已强制刷新", id);
        Ok(())
    }

    /// 获取负载均衡模式（Admin API）
    pub fn get_load_balancing_mode(&self) -> String {
        self.load_balancing_mode.lock().clone()
    }

    fn persist_load_balancing_mode(&self, mode: &str) -> anyhow::Result<()> {
        let mode = mode.to_string();
        self.update_config_file(move |config| config.load_balancing_mode = mode)
    }

    /// 设置负载均衡模式（Admin API）
    pub fn set_load_balancing_mode(&self, mode: String) -> anyhow::Result<()> {
        // 验证模式值
        if mode != "priority" && mode != "balanced" {
            anyhow::bail!("无效的负载均衡模式: {}", mode);
        }

        let previous_mode = self.get_load_balancing_mode();
        if previous_mode == mode {
            return Ok(());
        }

        *self.load_balancing_mode.lock() = mode.clone();

        if let Err(err) = self.persist_load_balancing_mode(&mode) {
            *self.load_balancing_mode.lock() = previous_mode;
            return Err(err);
        }

        if mode == "priority" {
            self.select_highest_priority();
        }

        tracing::info!("负载均衡模式已设置为: {}", mode);
        Ok(())
    }

    /// 获取账号级风控故障转移配置（Admin API）
    pub fn get_account_throttle_failover(&self) -> bool {
        self.account_throttle_failover.load(Ordering::Relaxed)
    }

    /// 获取账号级风控冷却时长秒数（Admin API）
    pub fn get_account_throttle_cooldown_secs(&self) -> u64 {
        self.account_throttle_cooldown_secs.load(Ordering::Relaxed)
    }

    /// 设置账号级风控故障转移配置（Admin API）
    ///
    /// 任一参数传 `None` 表示不修改该字段。
    pub fn set_account_throttle_config(
        &self,
        failover: Option<bool>,
        cooldown_secs: Option<u64>,
    ) -> anyhow::Result<()> {
        if let Some(secs) = cooldown_secs {
            // 限定一个合理范围：1 秒到 24 小时
            if !(1..=86_400).contains(&secs) {
                anyhow::bail!("冷却时长必须在 1..=86400 秒内: {}", secs);
            }
        }

        let _update_guard = self.runtime_config_update_lock.lock();

        let prev_failover = self.get_account_throttle_failover();
        let prev_cooldown = self.get_account_throttle_cooldown_secs();
        let new_failover = failover.unwrap_or(prev_failover);
        let new_cooldown = cooldown_secs.unwrap_or(prev_cooldown);

        if new_failover == prev_failover && new_cooldown == prev_cooldown {
            return Ok(());
        }

        self.account_throttle_failover
            .store(new_failover, Ordering::Relaxed);
        self.account_throttle_cooldown_secs
            .store(new_cooldown, Ordering::Relaxed);

        if let Err(err) = self.persist_account_throttle_config(new_failover, new_cooldown) {
            // 回滚内存值
            self.account_throttle_failover
                .store(prev_failover, Ordering::Relaxed);
            self.account_throttle_cooldown_secs
                .store(prev_cooldown, Ordering::Relaxed);
            return Err(err);
        }

        tracing::info!(
            "账号级风控配置已更新: failover={}, cooldown_secs={}",
            new_failover,
            new_cooldown
        );
        Ok(())
    }

    fn persist_account_throttle_config(
        &self,
        failover: bool,
        cooldown_secs: u64,
    ) -> anyhow::Result<()> {
        self.update_config_file(move |config| {
            config.account_throttle_failover = failover;
            config.account_throttle_cooldown_secs = cooldown_secs;
        })
    }

    /// 获取单账号 RPM 限流配置（Admin API）。返回：(是否启用, 每分钟上限)。
    pub fn get_account_rpm_limit_config(&self) -> (bool, u32) {
        (
            self.account_rpm_limit_enabled.load(Ordering::Relaxed),
            self.account_rpm_limit.load(Ordering::Relaxed),
        )
    }

    /// 设置单账号 RPM 限流配置（Admin API）。
    ///
    /// 任一参数传 `None` 表示不修改该字段。关闭限流时会清空所有凭据的窗口计数，
    /// 避免下次开启时残留旧时间戳造成误判。
    pub fn set_account_rpm_limit_config(
        &self,
        enabled: Option<bool>,
        limit: Option<u32>,
    ) -> anyhow::Result<()> {
        if let Some(value) = limit {
            // 限定合理范围：1..=100000。0 会被视为"不限"，故不接受，避免与关闭开关语义混淆。
            if !(1..=100_000).contains(&value) {
                anyhow::bail!("RPM 上限必须在 1..=100000 内: {}", value);
            }
        }

        let _update_guard = self.runtime_config_update_lock.lock();

        let (prev_enabled, prev_limit) = self.get_account_rpm_limit_config();
        let new_enabled = enabled.unwrap_or(prev_enabled);
        let new_limit = limit.unwrap_or(prev_limit);

        if new_enabled == prev_enabled && new_limit == prev_limit {
            return Ok(());
        }

        self.account_rpm_limit_enabled
            .store(new_enabled, Ordering::Relaxed);
        self.account_rpm_limit.store(new_limit, Ordering::Relaxed);

        if let Err(err) = self.persist_account_rpm_limit_config(new_enabled, new_limit) {
            // 回滚内存值
            self.account_rpm_limit_enabled
                .store(prev_enabled, Ordering::Relaxed);
            self.account_rpm_limit.store(prev_limit, Ordering::Relaxed);
            return Err(err);
        }

        // 关闭限流时清空窗口，避免重新开启后残留旧计数误判。
        if !new_enabled {
            for entry in self.entries.lock().iter_mut() {
                entry.rpm_window.clear();
            }
        }

        tracing::info!(
            "单账号 RPM 限流配置已更新: enabled={}, limit={}",
            new_enabled,
            new_limit
        );
        Ok(())
    }

    fn persist_account_rpm_limit_config(&self, enabled: bool, limit: u32) -> anyhow::Result<()> {
        self.update_config_file(move |config| {
            config.account_rpm_limit_enabled = enabled;
            config.account_rpm_limit = limit;
        })
    }

    /// 获取自愈治理配置（Admin API）。
    ///
    /// 返回：(封禁识别开关, 自愈开关, 自愈冷却秒, 连续自愈上限, 当前连续自愈轮数,
    /// 累计自愈次数)。后两项为只读观测值。
    pub fn get_self_heal_config(&self) -> (bool, bool, u64, u32, u32, u64) {
        let (consecutive_rounds, total_count) = {
            let entries = self.entries.lock();
            (
                entries
                    .iter()
                    .map(|entry| entry.self_heal_consecutive_rounds)
                    .max()
                    .unwrap_or(0),
                entries
                    .iter()
                    .map(|entry| entry.self_heal_total_count)
                    .fold(0_u64, u64::saturating_add),
            )
        };
        (
            self.suspended_detection_enabled.load(Ordering::Relaxed),
            self.self_heal_enabled.load(Ordering::Relaxed),
            self.self_heal_min_interval_secs.load(Ordering::Relaxed),
            self.self_heal_max_consecutive_rounds
                .load(Ordering::Relaxed),
            consecutive_rounds,
            total_count,
        )
    }

    /// 更新自愈治理配置（Admin API）。任一参数传 `None` 表示不修改该字段。
    ///
    /// 运行时立即生效并持久化到 config.json。持久化失败时回滚内存值。
    pub fn set_self_heal_config(
        &self,
        suspended_detection_enabled: Option<bool>,
        self_heal_enabled: Option<bool>,
        self_heal_min_interval_secs: Option<u64>,
        self_heal_max_consecutive_rounds: Option<u32>,
    ) -> anyhow::Result<()> {
        if let Some(secs) = self_heal_min_interval_secs {
            // 0 秒到 24 小时
            if secs > 86_400 {
                anyhow::bail!("自愈冷却间隔必须在 0..=86400 秒内: {}", secs);
            }
        }
        if let Some(r) = self_heal_max_consecutive_rounds {
            // 0 表示不限，上限 1000 防误配
            if r > 1000 {
                anyhow::bail!("连续自愈上限必须在 0..=1000 内（0=不限）: {}", r);
            }
        }

        let _update_guard = self.runtime_config_update_lock.lock();

        let prev = self.get_self_heal_config();
        let new_suspend_detect = suspended_detection_enabled.unwrap_or(prev.0);
        let new_enabled = self_heal_enabled.unwrap_or(prev.1);
        let new_interval = self_heal_min_interval_secs.unwrap_or(prev.2);
        let new_max_rounds = self_heal_max_consecutive_rounds.unwrap_or(prev.3);

        if new_suspend_detect == prev.0
            && new_enabled == prev.1
            && new_interval == prev.2
            && new_max_rounds == prev.3
        {
            return Ok(());
        }

        self.suspended_detection_enabled
            .store(new_suspend_detect, Ordering::Relaxed);
        self.self_heal_enabled.store(new_enabled, Ordering::Relaxed);
        self.self_heal_min_interval_secs
            .store(new_interval, Ordering::Relaxed);
        self.self_heal_max_consecutive_rounds
            .store(new_max_rounds, Ordering::Relaxed);

        if let Err(err) = self.persist_self_heal_config(
            new_suspend_detect,
            new_enabled,
            new_interval,
            new_max_rounds,
        ) {
            // 回滚内存值
            self.suspended_detection_enabled
                .store(prev.0, Ordering::Relaxed);
            self.self_heal_enabled.store(prev.1, Ordering::Relaxed);
            self.self_heal_min_interval_secs
                .store(prev.2, Ordering::Relaxed);
            self.self_heal_max_consecutive_rounds
                .store(prev.3, Ordering::Relaxed);
            return Err(err);
        }

        tracing::info!(
            "自愈治理配置已更新: suspended_detection_enabled={}, self_heal_enabled={}, min_interval_secs={}, max_rounds={}",
            new_suspend_detect,
            new_enabled,
            new_interval,
            new_max_rounds
        );
        Ok(())
    }

    fn persist_self_heal_config(
        &self,
        suspended_detection_enabled: bool,
        self_heal_enabled: bool,
        self_heal_min_interval_secs: u64,
        self_heal_max_consecutive_rounds: u32,
    ) -> anyhow::Result<()> {
        self.update_config_file(move |config| {
            config.suspended_detection_enabled = suspended_detection_enabled;
            config.self_heal_enabled = self_heal_enabled;
            config.self_heal_min_interval_secs = self_heal_min_interval_secs;
            config.self_heal_max_consecutive_rounds = self_heal_max_consecutive_rounds;
        })
    }
}

impl Drop for MultiTokenManager {
    fn drop(&mut self) {
        if self.stats_dirty.load(Ordering::Relaxed) {
            self.save_stats();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// 构造一个仅含单凭据、可配置 RPM 限流的测试用 manager。
    fn rpm_test_manager(enabled: bool, limit: u32) -> MultiTokenManager {
        let mut config = Config::default();
        config.account_rpm_limit_enabled = enabled;
        config.account_rpm_limit = limit;
        let cred = KiroCredentials {
            id: Some(1),
            refresh_token: Some("refresh-token-".repeat(4)),
            access_token: Some("access-token".to_string()),
            ..KiroCredentials::default()
        };
        MultiTokenManager::new(config, vec![cred], None, None, true).unwrap()
    }

    #[test]
    fn rpm_disabled_never_exceeds() {
        let mgr = rpm_test_manager(false, 1);
        // 即使反复记录也不应触发限流（关闭时 record 直接清空窗口）
        for _ in 0..10 {
            mgr.record_request(1);
        }
        let entries = mgr.entries.lock();
        let entry = &entries[0];
        assert!(!mgr.rpm_exceeded(entry, Instant::now()));
        assert!(entry.rpm_window.is_empty());
    }

    #[test]
    fn rpm_blocks_at_limit_and_recovers_after_window() {
        let limit = 3;
        let mgr = rpm_test_manager(true, limit);

        // 恰好未达上限：limit-1 次后仍可用
        for _ in 0..(limit - 1) {
            mgr.record_request(1);
        }
        {
            let entries = mgr.entries.lock();
            assert!(!mgr.rpm_exceeded(&entries[0], Instant::now()));
        }

        // 第 limit 次后达到上限：应拦截
        mgr.record_request(1);
        {
            let entries = mgr.entries.lock();
            assert!(mgr.rpm_exceeded(&entries[0], Instant::now()));
        }

        // 手动把窗口内时间戳伪造成 61 秒前，模拟窗口滑出 → 恢复可用
        {
            let mut entries = mgr.entries.lock();
            let old = Instant::now() - StdDuration::from_secs(RPM_WINDOW_SECS + 1);
            for ts in entries[0].rpm_window.iter_mut() {
                *ts = old;
            }
            assert!(!mgr.rpm_exceeded(&entries[0], Instant::now()));
        }
    }

    #[test]
    fn rpm_record_prunes_expired_timestamps() {
        let mgr = rpm_test_manager(true, 100);
        // 注入一个过期时间戳，record 时应被剔除，只留新压入的一个
        {
            let mut entries = mgr.entries.lock();
            entries[0]
                .rpm_window
                .push_back(Instant::now() - StdDuration::from_secs(RPM_WINDOW_SECS + 5));
        }
        mgr.record_request(1);
        let entries = mgr.entries.lock();
        assert_eq!(entries[0].rpm_window.len(), 1);
    }

    #[test]
    fn rpm_record_never_reserves_beyond_limit() {
        let mgr = rpm_test_manager(true, 2);

        // 多个请求可能在任一请求记账前都已通过选择阶段。最终记账必须再次校验
        // 上限，确保这些并发预选请求中最多只有 limit 个获得额度。
        for _ in 0..8 {
            mgr.record_request(1);
        }

        let entries = mgr.entries.lock();
        assert_eq!(entries[0].rpm_window.len(), 2);
    }

    #[tokio::test]
    async fn rpm_exhaustion_returns_typed_rate_limit_error() {
        let mgr = rpm_test_manager(true, 1);
        mgr.record_request(1);

        let error = match mgr.acquire_context(None, None).await {
            Ok(_) => panic!("RPM 已耗尽时不应返回调用上下文"),
            Err(error) => error,
        };
        let rate_limit = error
            .downcast_ref::<UpstreamRateLimitError>()
            .expect("RPM 已耗尽应返回类型化限流错误，以便 HTTP 层映射为 429");
        assert!(rate_limit.retry_after().is_some());
    }

    #[test]
    fn test_is_token_expired_with_expired_token() {
        let mut credentials = KiroCredentials::default();
        credentials.expires_at = Some("2020-01-01T00:00:00Z".to_string());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_with_valid_token() {
        let mut credentials = KiroCredentials::default();
        let future = Utc::now() + Duration::hours(1);
        credentials.expires_at = Some(future.to_rfc3339());
        assert!(!is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_within_5_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(3);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_no_expires_at() {
        let credentials = KiroCredentials::default();
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_within_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(8);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_beyond_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(15);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(!is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_validate_refresh_token_missing() {
        let credentials = KiroCredentials::default();
        let result = validate_refresh_token(&credentials);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_refresh_token_valid() {
        let mut credentials = KiroCredentials::default();
        credentials.refresh_token = Some("a".repeat(150));
        let result = validate_refresh_token(&credentials);
        assert!(result.is_ok());
    }

    #[test]
    fn test_sha256_hex() {
        let result = sha256_hex("test");
        assert_eq!(
            result,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    #[tokio::test]
    async fn test_refresh_token_rejects_api_key_credential() {
        let config = Config::default();
        let mut credentials = KiroCredentials::default();
        credentials.kiro_api_key = Some("ksk_test_key_123".to_string());
        credentials.auth_method = Some("api_key".to_string());

        let result = refresh_token(&credentials, &config, None).await;

        assert!(result.is_err(), "API Key 凭据应被 refresh_token 拒绝");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("API Key 凭据不支持刷新"),
            "期望错误消息包含 'API Key 凭据不支持刷新'，实际: {}",
            err_msg
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn idc_relogin_replaces_complete_client_bound_credentials() {
        let path = std::env::temp_dir().join(format!(
            "kiro_test_idc_relogin_{}_{}.json",
            std::process::id(),
            fastrand::u64(..)
        ));
        std::fs::write(&path, "[]").unwrap();

        let existing = KiroCredentials {
            id: Some(7),
            profile_arn: Some(
                "arn:aws:codewhisperer:us-east-1:123456789012:profile/REAL".to_string(),
            ),
            access_token: Some("old-access-token".to_string()),
            refresh_token: Some("old-refresh-token-".repeat(10)),
            expires_at: Some("2025-01-01T00:00:00Z".to_string()),
            auth_method: Some("idc".to_string()),
            provider: Some("Enterprise".to_string()),
            client_id: Some("old-client-id".to_string()),
            client_secret: Some("old-client-secret".to_string()),
            start_url: Some("https://old.awsapps.com/start".to_string()),
            token_endpoint: Some(
                "https://login.microsoftonline.com/tenant/oauth2/v2.0/token".to_string(),
            ),
            issuer_url: Some("https://login.microsoftonline.com/tenant/v2.0".to_string()),
            scopes: Some("openid offline_access".to_string()),
            region: Some("us-west-2".to_string()),
            auth_region: Some("us-west-2".to_string()),
            api_region: Some("eu-central-1".to_string()),
            email: Some("user@example.com".to_string()),
            disabled: true,
            disabled_reason: Some("TooManyFailures".to_string()),
            self_heal_consecutive_rounds: 3,
            self_heal_total_count: 9,
            last_self_heal_at: Some("2026-07-28T03:00:00Z".to_string()),
            self_heal_model: Some("claude-sonnet-4".to_string()),
            kiro_api_key: Some("ksk_old".to_string()),
            groups: vec!["team-a".to_string()],
            ..KiroCredentials::default()
        };

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![existing],
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();
        let new_refresh_token = "new-refresh-token-".repeat(10);
        let new_expires_at = "2026-07-28T04:00:00Z".to_string();

        manager
            .replace_idc_relogin_credentials(
                7,
                IdcReloginCredentials {
                    access_token: "new-access-token".to_string(),
                    refresh_token: new_refresh_token.clone(),
                    expires_at: Some(new_expires_at.clone()),
                    client_id: "new-client-id".to_string(),
                    client_secret: "new-client-secret".to_string(),
                    region: "ap-southeast-1".to_string(),
                    start_url: "https://view.awsapps.com/start".to_string(),
                    provider: "BuilderId".to_string(),
                },
            )
            .await
            .unwrap();

        let stored = manager.clone_all_credentials().remove(0);
        assert_eq!(stored.access_token.as_deref(), Some("new-access-token"));
        assert_eq!(
            stored.refresh_token.as_deref(),
            Some(new_refresh_token.as_str())
        );
        assert_eq!(stored.expires_at.as_deref(), Some(new_expires_at.as_str()));
        assert_eq!(stored.client_id.as_deref(), Some("new-client-id"));
        assert_eq!(stored.client_secret.as_deref(), Some("new-client-secret"));
        assert_eq!(stored.auth_method.as_deref(), Some("idc"));
        assert_eq!(stored.provider.as_deref(), Some("BuilderId"));
        assert_eq!(stored.region.as_deref(), Some("ap-southeast-1"));
        assert_eq!(stored.auth_region.as_deref(), Some("ap-southeast-1"));
        assert_eq!(
            stored.start_url.as_deref(),
            Some("https://view.awsapps.com/start")
        );
        assert!(stored.token_endpoint.is_none());
        assert!(stored.issuer_url.is_none());
        assert!(stored.scopes.is_none());
        assert!(stored.kiro_api_key.is_none());
        assert!(!stored.disabled);
        assert!(stored.disabled_reason.is_none());
        assert_eq!(stored.self_heal_consecutive_rounds, 0);
        assert_eq!(stored.self_heal_total_count, 9);
        assert!(stored.last_self_heal_at.is_none());
        assert!(stored.self_heal_model.is_none());

        assert_eq!(stored.email.as_deref(), Some("user@example.com"));
        assert_eq!(stored.groups, vec!["team-a"]);
        assert_eq!(stored.api_region.as_deref(), Some("eu-central-1"));
        // profileArn belongs to the authenticated identity. A relogin can select a
        // different account or provider, so the old ARN must be resolved again.
        assert!(stored.profile_arn.is_none());

        // 落盘同样是完整替换（内存态已在上面逐字段断言，这里只验证持久化本身）
        let persisted: Vec<KiroCredentials> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            persisted[0].refresh_token.as_deref(),
            Some(new_refresh_token.as_str())
        );
        assert_eq!(persisted[0].client_id.as_deref(), Some("new-client-id"));
        assert!(persisted[0].profile_arn.is_none());
        assert_eq!(persisted[0].self_heal_consecutive_rounds, 0);
        assert_eq!(persisted[0].self_heal_total_count, 9);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_refresh_token() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.refresh_token = Some("a".repeat(150));

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("凭据已存在"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_success() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_test_key_123".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        let id = result.unwrap();
        assert!(id > 0);
        assert_eq!(manager.snapshot().total, 1);
        assert_eq!(manager.available_count(), 1);
        assert_eq!(manager.snapshot().current_id, id);
    }

    #[tokio::test]
    async fn test_add_higher_priority_credential_becomes_current() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.kiro_api_key = Some("ksk_existing_lower_priority".to_string());
        existing.auth_method = Some("api_key".to_string());
        existing.priority = 1000;

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();
        let original_id = manager.snapshot().current_id;

        let mut higher_priority = KiroCredentials::default();
        higher_priority.kiro_api_key = Some("ksk_new_higher_priority".to_string());
        higher_priority.auth_method = Some("api_key".to_string());
        higher_priority.priority = 0;

        let new_id = manager.add_credential(higher_priority).await.unwrap();

        assert_ne!(new_id, original_id);
        assert_eq!(manager.snapshot().current_id, new_id);
    }

    #[tokio::test]
    async fn test_add_lower_priority_credential_keeps_current() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.kiro_api_key = Some("ksk_existing_higher_priority".to_string());
        existing.auth_method = Some("api_key".to_string());
        existing.priority = 0;

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();
        let original_id = manager.snapshot().current_id;

        let mut lower_priority = KiroCredentials::default();
        lower_priority.kiro_api_key = Some("ksk_new_lower_priority".to_string());
        lower_priority.auth_method = Some("api_key".to_string());
        lower_priority.priority = 1000;

        let new_id = manager.add_credential(lower_priority).await.unwrap();

        assert_ne!(new_id, original_id);
        assert_eq!(manager.snapshot().current_id, original_id);
    }

    /// add_credential 应在入库时为新凭据写入 created_at（RFC3339），
    /// 且值可被解析。旧凭据（未携带该字段）由调用方决定是否补齐。
    #[tokio::test]
    async fn test_add_credential_sets_created_at() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_created_at_probe".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        manager.add_credential(api_key_cred).await.unwrap();

        let snapshot = manager.snapshot();
        let entry = snapshot.entries.first().expect("凭据应已入库");
        let created_at = entry
            .created_at
            .as_deref()
            .expect("新凭据应写入 created_at");
        assert!(
            DateTime::parse_from_rfc3339(created_at).is_ok(),
            "created_at 应为合法 RFC3339: {created_at}"
        );
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_api_key() {
        let config = Config::default();

        let mut existing = KiroCredentials::default();
        existing.kiro_api_key = Some("ksk_existing_key".to_string());
        existing.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let mut duplicate = KiroCredentials::default();
        duplicate.kiro_api_key = Some("ksk_existing_key".to_string());
        duplicate.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("kiroApiKey 重复")
        );
    }

    #[tokio::test]
    async fn test_add_credential_api_key_empty_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some(String::new());
        cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("kiroApiKey 为空")
        );
    }

    #[tokio::test]
    async fn test_add_credential_api_key_missing_key_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        // kiro_api_key is None

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("缺少 kiroApiKey")
        );
    }

    #[tokio::test]
    async fn test_add_credential_api_key_and_oauth_coexist() {
        let config = Config::default();

        let mut oauth_cred = KiroCredentials::default();
        oauth_cred.refresh_token = Some("a".repeat(150));

        let manager = MultiTokenManager::new(config, vec![oauth_cred], None, None, false).unwrap();

        let mut api_key_cred = KiroCredentials::default();
        api_key_cred.kiro_api_key = Some("ksk_new_key".to_string());
        api_key_cred.auth_method = Some("api_key".to_string());

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        assert_eq!(manager.snapshot().total, 2);
        assert_eq!(manager.available_count(), 2);
    }

    // MultiTokenManager 测试

    #[test]
    fn test_multi_token_manager_new() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.priority = 0;
        let mut cred2 = KiroCredentials::default();
        cred2.priority = 1;

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();
        assert_eq!(manager.snapshot().total, 2);
        assert_eq!(manager.available_count(), 2);
    }

    #[test]
    fn test_multi_token_manager_empty_credentials() {
        let config = Config::default();
        let result = MultiTokenManager::new(config, vec![], None, None, false);
        // 支持 0 个凭据启动（可通过管理面板添加）
        assert!(result.is_ok());
        let manager = result.unwrap();
        assert_eq!(manager.snapshot().total, 0);
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_duplicate_ids() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.id = Some(1);
        let mut cred2 = KiroCredentials::default();
        cred2.id = Some(1); // 重复 ID

        let result = MultiTokenManager::new(config, vec![cred1, cred2], None, None, false);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("重复的凭据 ID"),
            "错误消息应包含 '重复的凭据 ID'，实际: {}",
            err_msg
        );
    }

    #[test]
    fn test_multi_token_manager_api_key_missing_kiro_api_key_auto_disabled() {
        let config = Config::default();

        // auth_method=api_key 但缺少 kiro_api_key → 应被自动禁用
        let mut bad_cred = KiroCredentials::default();
        bad_cred.auth_method = Some("api_key".to_string());
        // kiro_api_key 保持 None

        let mut good_cred = KiroCredentials::default();
        good_cred.refresh_token = Some("valid_token".to_string());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();
        assert_eq!(manager.snapshot().total, 2);
        assert_eq!(manager.available_count(), 1); // bad_cred 被禁用，只剩 1 个可用
    }

    #[test]
    fn test_multi_token_manager_api_key_with_kiro_api_key_not_disabled() {
        let config = Config::default();

        // auth_method=api_key 且有 kiro_api_key → 不应被禁用
        let mut cred = KiroCredentials::default();
        cred.auth_method = Some("api_key".to_string());
        cred.kiro_api_key = Some("ksk_test123".to_string());

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();
        assert_eq!(manager.snapshot().total, 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_multi_token_manager_report_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        // 前两次失败不会禁用（使用 ID 1）
        assert!(manager.report_failure(1));
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 2);

        // 第三次失败会禁用第一个凭据
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 1);

        // 继续失败第二个凭据（使用 ID 2）
        assert!(manager.report_failure(2));
        assert!(manager.report_failure(2));
        assert!(!manager.report_failure(2)); // 所有凭据都禁用了
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_report_success() {
        let config = Config::default();
        let cred = KiroCredentials::default();

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        // 失败两次（使用 ID 1）
        manager.report_failure(1);
        manager.report_failure(1);

        // 成功后重置计数（使用 ID 1）
        manager.report_success(1);

        // 再失败两次不会禁用
        manager.report_failure(1);
        manager.report_failure(1);
        assert_eq!(manager.available_count(), 1);
    }

    /// 把所有凭据打到 TooManyFailures 禁用（全灭）
    fn disable_all_via_failures(manager: &MultiTokenManager, ids: &[u64]) {
        for &id in ids {
            for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
                manager.report_failure(id);
            }
        }
        assert_eq!(manager.available_count(), 0, "预期全部凭据已禁用");
    }

    fn issue51_credentials_path(name: &str) -> (PathBuf, PathBuf) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kiro_issue51_{}_{}_{}",
            name,
            std::process::id(),
            nonce
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        (dir, path)
    }

    #[test]
    fn self_heal_recovers_disabled_credentials() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials::default(), KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        disable_all_via_failures(&manager, &[1, 2]);
        assert!(manager.try_self_heal(None, None), "全灭且启用时应执行自愈");
        assert_eq!(manager.available_count(), 2, "自愈后应恢复全部凭据");

        let (_, _, _, _, consecutive, total) = manager.get_self_heal_config();
        assert_eq!(consecutive, 1);
        assert_eq!(total, 2, "累计值按恢复的凭据次数统计");
    }

    #[test]
    fn self_heal_respects_cooldown() {
        let mut config = Config::default();
        config.self_heal_min_interval_secs = 3600; // 1 小时冷却，测试内不会到期
        let manager = MultiTokenManager::new(
            config,
            vec![KiroCredentials::default(), KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        disable_all_via_failures(&manager, &[1, 2]);
        assert!(manager.try_self_heal(None, None), "首次自愈应成功");

        // 再次全灭，但仍在冷却窗口内 → 不应再次自愈
        disable_all_via_failures(&manager, &[1, 2]);
        assert!(!manager.try_self_heal(None, None), "冷却窗口内不应再次自愈");
        assert_eq!(manager.available_count(), 0);

        let (_, _, _, _, consecutive, total) = manager.get_self_heal_config();
        assert_eq!(consecutive, 1, "冷却拦截不增加轮数");
        assert_eq!(total, 2, "首次恢复了两个凭据");
    }

    #[test]
    fn self_heal_stops_after_max_rounds() {
        let mut config = Config::default();
        config.self_heal_min_interval_secs = 0; // 无冷却，专注测上限
        config.self_heal_max_consecutive_rounds = 2;
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        // 无任何成功，连续自愈到达上限后停止
        disable_all_via_failures(&manager, &[1]);
        assert!(manager.try_self_heal(None, None), "第 1 轮自愈");
        disable_all_via_failures(&manager, &[1]);
        assert!(manager.try_self_heal(None, None), "第 2 轮自愈");
        disable_all_via_failures(&manager, &[1]);
        assert!(!manager.try_self_heal(None, None), "达上限后应停止自愈");
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn self_heal_resets_consecutive_rounds_on_success() {
        let mut config = Config::default();
        config.self_heal_min_interval_secs = 0;
        config.self_heal_max_consecutive_rounds = 2;
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        disable_all_via_failures(&manager, &[1]);
        assert!(manager.try_self_heal(None, None), "第 1 轮自愈");
        disable_all_via_failures(&manager, &[1]);
        assert!(manager.try_self_heal(None, None), "第 2 轮自愈");

        // 一次成功清零连续计数
        manager.report_success(1);
        let (_, _, _, _, consecutive, _) = manager.get_self_heal_config();
        assert_eq!(consecutive, 0, "成功后连续轮数应清零");

        // 清零后应能重新自愈（不受之前上限影响）
        disable_all_via_failures(&manager, &[1]);
        assert!(manager.try_self_heal(None, None), "成功清零后应可再次自愈");
    }

    #[test]
    fn report_suspended_disables_immediately_and_excluded_from_self_heal() {
        let mut config = Config::default();
        config.self_heal_min_interval_secs = 0;
        let manager = MultiTokenManager::new(
            config,
            vec![KiroCredentials::default(), KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        // 凭据 #1 被封禁：立即禁用（无需累计），切换到 #2 仍可用
        assert!(manager.report_suspended(1), "封禁 #1 后 #2 仍可用");
        assert_eq!(manager.available_count(), 1);
        {
            let snapshot = manager.snapshot();
            let e1 = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
            assert!(e1.disabled);
            assert_eq!(e1.disabled_reason.as_deref(), Some("Suspended"));
        }

        // 凭据 #2 也被封禁 → 全灭
        assert!(!manager.report_suspended(2), "封禁 #2 后应全灭");
        assert_eq!(manager.available_count(), 0);

        // 自愈不应复活 Suspended 凭据
        assert!(
            !manager.try_self_heal(None, None),
            "Suspended 凭据不参与自愈"
        );
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn suspended_credential_recovers_via_manual_reset() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        manager.report_suspended(1);
        assert_eq!(manager.available_count(), 0);

        // 手动重置可恢复（误判逃生途径）
        manager.reset_and_enable(1).unwrap();
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn self_heal_disabled_does_not_recover() {
        let mut config = Config::default();
        config.self_heal_enabled = false;
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        disable_all_via_failures(&manager, &[1]);
        assert!(!manager.try_self_heal(None, None), "自愈关闭时不应恢复");
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn self_heal_counters_saturate_at_numeric_limits() {
        let mut config = Config::default();
        config.self_heal_min_interval_secs = 0;
        config.self_heal_max_consecutive_rounds = 0;
        let mut first = grouped_cred("max-counter-1", &[]);
        first.disabled = true;
        first.disabled_reason = Some("TooManyFailures".to_string());
        first.self_heal_consecutive_rounds = u32::MAX;
        first.self_heal_total_count = u64::MAX;
        let mut second = grouped_cred("max-counter-2", &[]);
        second.self_heal_total_count = 1;
        let manager =
            MultiTokenManager::new(config, vec![first, second], None, None, false).unwrap();

        assert!(manager.try_self_heal(None, None));
        let (_, _, _, _, consecutive, total) = manager.get_self_heal_config();
        assert_eq!(consecutive, u32::MAX);
        assert_eq!(total, u64::MAX);
    }

    #[tokio::test]
    async fn self_heal_only_recovers_credentials_in_request_group() {
        let mut config = Config::default();
        config.self_heal_min_interval_secs = 0;
        let manager = MultiTokenManager::new(
            config,
            vec![
                grouped_cred("g1-token", &["g1"]),
                grouped_cred("g2-token", &["g2"]),
            ],
            None,
            None,
            false,
        )
        .unwrap();

        disable_all_via_failures(&manager, &[1, 2]);
        let context = manager
            .acquire_context(None, Some("g1"))
            .await
            .expect("g1 应恢复自己的凭据");
        assert_eq!(context.id, 1);

        let snapshot = manager.snapshot();
        let g2 = snapshot.entries.iter().find(|entry| entry.id == 2).unwrap();
        assert!(g2.disabled, "g1 的自愈不得复活 g2 凭据");
    }

    #[tokio::test]
    async fn success_in_other_group_does_not_reset_recovery_limit() {
        let mut config = Config::default();
        config.self_heal_min_interval_secs = 0;
        config.self_heal_max_consecutive_rounds = 1;
        let manager = MultiTokenManager::new(
            config,
            vec![
                grouped_cred("g1-token", &["g1"]),
                grouped_cred("g2-token", &["g2"]),
            ],
            None,
            None,
            false,
        )
        .unwrap();

        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(1);
        }
        manager
            .acquire_context(None, Some("g1"))
            .await
            .expect("g1 首轮自愈应成功");
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(1);
        }

        manager.report_success(2);
        assert!(
            manager.acquire_context(None, Some("g1")).await.is_err(),
            "g2 成功不能解除 g1 已达到的自愈上限"
        );
        let g1 = manager
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.id == 1)
            .unwrap();
        assert!(g1.disabled);
    }

    #[tokio::test]
    async fn success_on_other_model_does_not_reset_recovery_limit() {
        let mut config = Config::default();
        config.self_heal_min_interval_secs = 0;
        config.self_heal_max_consecutive_rounds = 1;
        let manager = MultiTokenManager::new(
            config,
            vec![grouped_cred("model-token", &[])],
            None,
            None,
            false,
        )
        .unwrap();
        seed_model_cache(&manager, 1, &["model-a", "model-b"]);

        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure_for_request(1, Some("model-a"), None);
        }
        manager
            .acquire_context(Some("model-a"), None)
            .await
            .expect("model-a 首轮自愈应成功");
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure_for_request(1, Some("model-a"), None);
        }

        manager.report_success_for_request(1, Some("model-b"));
        assert!(
            manager
                .acquire_context(Some("model-a"), None)
                .await
                .is_err(),
            "model-b 成功不能解除 model-a 已达到的自愈上限"
        );
    }

    #[tokio::test]
    async fn missing_group_does_not_mutate_self_heal_state() {
        let mut config = Config::default();
        config.self_heal_min_interval_secs = 0;
        let manager = MultiTokenManager::new(
            config,
            vec![grouped_cred("g1-token", &["g1"])],
            None,
            None,
            false,
        )
        .unwrap();
        disable_all_via_failures(&manager, &[1]);

        assert!(
            manager
                .acquire_context(None, Some("missing"))
                .await
                .is_err()
        );
        let snapshot = manager.snapshot();
        assert!(snapshot.entries[0].disabled);
        let (_, _, _, _, consecutive, total) = manager.get_self_heal_config();
        assert_eq!(consecutive, 0);
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn unsupported_model_does_not_mutate_self_heal_state() {
        let mut config = Config::default();
        config.self_heal_min_interval_secs = 0;
        let manager = MultiTokenManager::new(
            config,
            vec![grouped_cred("model-token", &[])],
            None,
            None,
            false,
        )
        .unwrap();
        seed_model_cache(&manager, 1, &["glm-5"]);
        disable_all_via_failures(&manager, &[1]);

        assert!(
            manager
                .acquire_context(Some("deepseek-3.2"), None)
                .await
                .is_err()
        );
        let snapshot = manager.snapshot();
        assert!(snapshot.entries[0].disabled);
        let (_, _, _, _, consecutive, total) = manager.get_self_heal_config();
        assert_eq!(consecutive, 0);
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn throttled_group_does_not_recover_unrelated_disabled_credentials() {
        let mut config = Config::default();
        config.self_heal_min_interval_secs = 0;
        let manager = MultiTokenManager::new(
            config,
            vec![
                grouped_cred("g1-token", &["g1"]),
                grouped_cred("g2-token", &["g2"]),
            ],
            None,
            None,
            false,
        )
        .unwrap();

        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(2);
        }
        assert_eq!(
            manager.report_account_throttled_for_request(
                1,
                StdDuration::from_secs(3600),
                None,
                Some("g1"),
            ),
            0
        );
        assert!(manager.acquire_context(None, Some("g1")).await.is_err());
        let g2 = manager
            .snapshot()
            .entries
            .into_iter()
            .find(|entry| entry.id == 2)
            .unwrap();
        assert!(g2.disabled, "g1 风控冷却不得复活 g2 凭据");
    }

    #[test]
    fn suspended_reason_survives_restart() {
        use crate::kiro::model::credentials::CredentialsConfig;

        let (dir, path) = issue51_credentials_path("suspended_restart");
        let mut credential = grouped_cred("suspended-token", &[]);
        credential.id = Some(1);
        std::fs::write(&path, serde_json::to_vec_pretty(&[&credential]).unwrap()).unwrap();

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![credential],
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();
        manager.report_suspended(1);
        drop(manager);

        let loaded = CredentialsConfig::load(&path)
            .unwrap()
            .into_sorted_credentials();
        let restarted =
            MultiTokenManager::new(Config::default(), loaded, None, Some(path.clone()), true)
                .unwrap();
        let entry = &restarted.snapshot().entries[0];
        assert!(entry.disabled);
        assert_eq!(entry.disabled_reason.as_deref(), Some("Suspended"));
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recovery_limit_survives_restart() {
        use crate::kiro::model::credentials::CredentialsConfig;

        let (dir, path) = issue51_credentials_path("recovery_restart");
        let mut credential = grouped_cred("recovery-token", &[]);
        credential.id = Some(1);
        std::fs::write(&path, serde_json::to_vec_pretty(&[&credential]).unwrap()).unwrap();

        let mut config = Config::default();
        config.self_heal_min_interval_secs = 0;
        config.self_heal_max_consecutive_rounds = 1;
        let manager = MultiTokenManager::new(
            config.clone(),
            vec![credential],
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();
        disable_all_via_failures(&manager, &[1]);
        manager
            .acquire_context(None, None)
            .await
            .expect("首轮自愈应成功");
        disable_all_via_failures(&manager, &[1]);
        drop(manager);

        let loaded = CredentialsConfig::load(&path)
            .unwrap()
            .into_sorted_credentials();
        let restarted =
            MultiTokenManager::new(config, loaded, None, Some(path.clone()), true).unwrap();
        assert!(restarted.acquire_context(None, None).await.is_err());
        let entry = &restarted.snapshot().entries[0];
        assert!(entry.disabled);
        assert_eq!(entry.disabled_reason.as_deref(), Some("TooManyFailures"));
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_self_heal_partial_updates_preserve_all_fields() {
        let (dir, config_path) = issue51_credentials_path("concurrent_config");
        let config = Config::default();
        std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
        let config = Config::load(&config_path).unwrap();
        let manager = Arc::new(MultiTokenManager::new(config, vec![], None, None, false).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let enabled_manager = Arc::clone(&manager);
        let enabled_barrier = Arc::clone(&barrier);
        let enabled = std::thread::spawn(move || {
            enabled_barrier.wait();
            enabled_manager
                .set_self_heal_config(None, Some(false), None, None)
                .unwrap();
        });
        let interval_manager = Arc::clone(&manager);
        let interval_barrier = Arc::clone(&barrier);
        let interval = std::thread::spawn(move || {
            interval_barrier.wait();
            interval_manager
                .set_self_heal_config(None, None, Some(123), None)
                .unwrap();
        });
        barrier.wait();
        enabled.join().unwrap();
        interval.join().unwrap();

        let (_, runtime_enabled, runtime_interval, _, _, _) = manager.get_self_heal_config();
        assert!(!runtime_enabled);
        assert_eq!(runtime_interval, 123);
        let persisted = Config::load(&config_path).unwrap();
        assert!(!persisted.self_heal_enabled);
        assert_eq!(persisted.self_heal_min_interval_secs, 123);

        drop(manager);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refresh_rate_limit_does_not_disable_or_increment_failure_count() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap();

        let error = anyhow::Error::new(UpstreamRateLimitError::new(Some("30".to_string())));
        let returned = manager
            .handle_token_refresh_error(1, error)
            .expect_err("429 应立即返回给调用方");

        assert!(returned.downcast_ref::<UpstreamRateLimitError>().is_some());
        let snapshot = manager.snapshot();
        let entry = &snapshot.entries[0];
        assert_eq!(entry.refresh_failure_count, 0);
        assert!(!entry.disabled);
        assert_eq!(entry.disabled_reason, None);
    }

    #[test]
    fn test_multi_token_manager_switch_to_next() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.refresh_token = Some("token1".to_string());
        let mut cred2 = KiroCredentials::default();
        cred2.refresh_token = Some("token2".to_string());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        let initial_id = manager.snapshot().current_id;

        // 切换到下一个
        assert!(manager.switch_to_next());
        assert_ne!(manager.snapshot().current_id, initial_id);
    }

    #[test]
    fn test_set_load_balancing_mode_persists_to_config_file() {
        let config_path =
            std::env::temp_dir().join(format!("kiro-load-balancing-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&config_path, r#"{"loadBalancingMode":"priority"}"#).unwrap();

        let config = Config::load(&config_path).unwrap();
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        manager
            .set_load_balancing_mode("balanced".to_string())
            .unwrap();

        let persisted = Config::load(&config_path).unwrap();
        assert_eq!(persisted.load_balancing_mode, "balanced");
        assert_eq!(manager.get_load_balancing_mode(), "balanced");

        std::fs::remove_file(&config_path).unwrap();
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_auto_recovers_all_disabled() {
        let config = Config::default();
        let mut cred1 = KiroCredentials::default();
        cred1.access_token = Some("t1".to_string());
        cred1.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        let mut cred2 = KiroCredentials::default();
        cred2.access_token = Some("t2".to_string());
        cred2.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(1);
        }
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(2);
        }

        assert_eq!(manager.available_count(), 0);

        // 应触发自愈：重置失败计数并重新启用，避免必须重启进程
        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert!(ctx.token == "t1" || ctx.token == "t2");
        assert_eq!(manager.available_count(), 2);
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_balanced_retries_until_bad_credential_disabled()
     {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let mut bad_cred = KiroCredentials::default();
        bad_cred.priority = 0;
        bad_cred.refresh_token = Some("bad".to_string());

        let mut good_cred = KiroCredentials::default();
        good_cred.priority = 1;
        good_cred.access_token = Some("good-token".to_string());
        good_cred.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();

        let ctx = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(ctx.id, 2);
        assert_eq!(ctx.token, "good-token");
    }

    #[tokio::test]
    async fn balanced_read_only_selection_does_not_update_current_id() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let first = KiroCredentials {
            access_token: Some("first-token".to_string()),
            expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
            priority: 0,
            ..KiroCredentials::default()
        };
        let second = KiroCredentials {
            access_token: Some("second-token".to_string()),
            expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
            priority: 1,
            ..KiroCredentials::default()
        };
        let manager =
            MultiTokenManager::new(config, vec![first, second], None, None, false).unwrap();

        assert_eq!(manager.snapshot().current_id, 1);
        manager.report_success(1);

        let (context, is_balanced) = manager
            .acquire_context_impl(None, None, false)
            .await
            .unwrap();

        assert!(is_balanced);
        assert_eq!(context.id, 2);
        assert_eq!(manager.snapshot().current_id, 1);

        let context = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(context.id, 2);
        assert_eq!(manager.snapshot().current_id, 2);
    }

    #[tokio::test]
    async fn switching_from_balanced_to_priority_selects_highest_priority() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let first = KiroCredentials {
            access_token: Some("first-token".to_string()),
            expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
            priority: 0,
            ..KiroCredentials::default()
        };
        let second = KiroCredentials {
            access_token: Some("second-token".to_string()),
            expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
            priority: 1,
            ..KiroCredentials::default()
        };
        let manager =
            MultiTokenManager::new(config, vec![first, second], None, None, false).unwrap();

        manager.report_success(1);
        let context = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(context.id, 2);
        assert_eq!(manager.snapshot().current_id, 2);

        manager
            .set_load_balancing_mode("priority".to_string())
            .unwrap();

        assert_eq!(manager.snapshot().current_id, 1);
    }

    #[test]
    fn test_multi_token_manager_report_refresh_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert_eq!(manager.available_count(), 2);
        for _ in 0..(MAX_FAILURES_PER_CREDENTIAL - 1) {
            assert!(manager.report_refresh_failure(1));
        }
        assert_eq!(manager.available_count(), 2);

        assert!(manager.report_refresh_failure(1));
        assert_eq!(manager.available_count(), 1);

        let snapshot = manager.snapshot();
        let first = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(first.disabled);
        assert_eq!(first.refresh_failure_count, MAX_FAILURES_PER_CREDENTIAL);
        assert_eq!(snapshot.current_id, 2);
    }

    #[tokio::test]
    async fn test_multi_token_manager_refresh_failure_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_refresh_failure(1);
            manager.report_refresh_failure(2);
        }
        assert_eq!(manager.available_count(), 0);

        let err = manager
            .acquire_context(None, None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
    }

    #[test]
    fn test_multi_token_manager_report_quota_exhausted() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        assert_eq!(manager.available_count(), 2);
        assert!(manager.report_quota_exhausted(1));
        assert_eq!(manager.available_count(), 1);

        // 再禁用第二个后，无可用凭据
        assert!(!manager.report_quota_exhausted(2));
        assert_eq!(manager.available_count(), 0);
    }

    #[tokio::test]
    async fn test_multi_token_manager_quota_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        manager.report_quota_exhausted(1);
        manager.report_quota_exhausted(2);
        assert_eq!(manager.available_count(), 0);

        let err = manager
            .acquire_context(None, None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
        assert_eq!(manager.available_count(), 0);
    }

    // ============ 凭据级 Region 优先级测试 ============

    #[test]
    fn test_credential_region_priority_uses_credential_auth_region() {
        // 凭据配置了 auth_region 时，应使用凭据的 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-west-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-west-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_credential_region() {
        // 凭据未配置 auth_region 但配置了 region 时，应回退到凭据.region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-central-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_config() {
        // 凭据未配置 auth_region 和 region 时，应回退到 config
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials::default();
        assert!(credentials.auth_region.is_none());
        assert!(credentials.region.is_none());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "us-west-2");
    }

    #[test]
    fn test_multiple_credentials_use_respective_regions() {
        // 多凭据场景下，不同凭据使用各自的 auth_region
        let mut config = Config::default();
        config.region = "ap-northeast-1".to_string();

        let mut cred1 = KiroCredentials::default();
        cred1.auth_region = Some("us-east-1".to_string());

        let mut cred2 = KiroCredentials::default();
        cred2.region = Some("eu-west-1".to_string());

        let cred3 = KiroCredentials::default(); // 无 region，使用 config

        assert_eq!(cred1.effective_auth_region(&config), "us-east-1");
        assert_eq!(cred2.effective_auth_region(&config), "eu-west-1");
        assert_eq!(cred3.effective_auth_region(&config), "ap-northeast-1");
    }

    #[test]
    fn test_idc_oidc_endpoint_uses_credential_auth_region() {
        // 验证 IdC OIDC endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("eu-central-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);

        assert_eq!(refresh_url, "https://oidc.eu-central-1.amazonaws.com/token");
    }

    #[test]
    fn test_social_refresh_endpoint_uses_credential_auth_region() {
        // 验证 Social refresh endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("ap-southeast-1".to_string());

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);

        assert_eq!(
            refresh_url,
            "https://prod.ap-southeast-1.auth.desktop.kiro.dev/refreshToken"
        );
    }

    #[test]
    fn test_api_call_uses_effective_api_region() {
        // 验证 API 调用使用 effective_api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.region = Some("eu-west-1".to_string());

        // 凭据.region 不参与 api_region 回退链
        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.us-west-2.amazonaws.com");
    }

    #[test]
    fn test_api_call_uses_credential_api_region() {
        // 凭据配置了 api_region 时，API 调用应使用凭据的 api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.api_region = Some("eu-central-1".to_string());

        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.eu-central-1.amazonaws.com");
    }

    #[test]
    fn test_rest_api_region_candidates_us_default() {
        // 非 EU 区域 → 主端点 us-east-1，回退 eu-central-1
        assert_eq!(
            rest_api_region_candidates("us-east-1"),
            ["us-east-1", "eu-central-1"]
        );
        assert_eq!(
            rest_api_region_candidates("us-east-2"),
            ["us-east-1", "eu-central-1"]
        );
        assert_eq!(
            rest_api_region_candidates("ap-southeast-1"),
            ["us-east-1", "eu-central-1"]
        );
    }

    #[test]
    fn test_rest_api_region_candidates_eu() {
        // EU 区域 → 主端点 eu-central-1，回退 us-east-1
        assert_eq!(
            rest_api_region_candidates("eu-central-1"),
            ["eu-central-1", "us-east-1"]
        );
        assert_eq!(
            rest_api_region_candidates("eu-west-1"),
            ["eu-central-1", "us-east-1"]
        );
        assert_eq!(
            rest_api_region_candidates("eu-north-1"),
            ["eu-central-1", "us-east-1"]
        );
    }

    #[test]
    fn test_rest_api_region_candidates_uses_credential_auth_region() {
        // Enterprise/IdC 账号导入时仅带 SSO region 字段（无 api_region），
        // effective_auth_region 会回退到 credential.region，进而选对端点。
        let config = Config::default(); // 默认 region = us-east-1

        let mut eu_cred = KiroCredentials::default();
        eu_cred.region = Some("eu-west-1".to_string());
        let sso_region = eu_cred.effective_auth_region(&config);
        assert_eq!(
            rest_api_region_candidates(sso_region),
            ["eu-central-1", "us-east-1"]
        );

        // 未配置任何 region 的凭据回退到 config 默认 us-east-1
        let plain_cred = KiroCredentials::default();
        let sso_region = plain_cred.effective_auth_region(&config);
        assert_eq!(
            rest_api_region_candidates(sso_region),
            ["us-east-1", "eu-central-1"]
        );
    }

    #[test]
    fn test_usage_rest_urls_carry_resolved_profile_arn() {
        // Enterprise / IdC：真实 ARN 必须以 URL 编码形式出现在查询串里，
        // 否则上游返回 403 "User is not authorized to make this call."
        let credentials = KiroCredentials {
            profile_arn: Some(
                "arn:aws:codewhisperer:us-east-1:123456789012:profile/REAL123".to_string(),
            ),
            ..Default::default()
        };
        let host = "q.us-east-1.amazonaws.com";
        let encoded =
            "arn%3Aaws%3Acodewhisperer%3Aus-east-1%3A123456789012%3Aprofile%2FREAL123";

        let arn = credentials.effective_profile_arn();
        assert_eq!(
            usage_limits_url(host, arn),
            format!(
                "https://q.us-east-1.amazonaws.com/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST&isEmailRequired=true&profileArn={}",
                encoded
            )
        );
        assert_eq!(
            available_models_url(host, arn),
            format!(
                "https://q.us-east-1.amazonaws.com/ListAvailableModels?origin=AI_EDITOR&profileArn={}",
                encoded
            )
        );

        // 每个区域端点先试带 ARN，403 时回退到不带
        assert_eq!(
            usage_api_attempts(&credentials, &["us-east-1", "eu-central-1"]),
            vec![
                ("us-east-1", arn),
                ("us-east-1", None),
                ("eu-central-1", arn),
                ("eu-central-1", None),
            ]
        );
    }

    #[test]
    fn test_usage_rest_urls_omit_builder_id_placeholder() {
        // BuilderID 占位符不发送：这些账号沿用不带 profileArn 的老式请求
        use crate::kiro::model::credentials::BUILDER_ID_PROFILE_ARN;

        let credentials = KiroCredentials {
            profile_arn: Some(BUILDER_ID_PROFILE_ARN.to_string()),
            ..Default::default()
        };
        let host = "q.us-east-1.amazonaws.com";

        // 占位符被过滤，只剩「不带 ARN」一种形态：与加此参数前的行为一致
        assert_eq!(credentials.effective_profile_arn(), None);
        assert_eq!(
            usage_api_attempts(&credentials, &["us-east-1", "eu-central-1"]),
            vec![("us-east-1", None), ("eu-central-1", None)]
        );
        assert_eq!(
            usage_limits_url(host, None),
            "https://q.us-east-1.amazonaws.com/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST&isEmailRequired=true"
        );
        assert_eq!(
            available_models_url(host, None),
            "https://q.us-east-1.amazonaws.com/ListAvailableModels?origin=AI_EDITOR"
        );
    }

    #[test]
    fn test_set_user_preference_body_carries_real_profile_arn() {
        let credentials = KiroCredentials {
            profile_arn: Some(
                "arn:aws:codewhisperer:us-east-1:123456789012:profile/REAL123".to_string(),
            ),
            ..Default::default()
        };

        assert_eq!(
            set_user_preference_body(&credentials, "ENABLED"),
            serde_json::json!({
                "overageConfiguration": { "overageStatus": "ENABLED" },
                "profileArn": "arn:aws:codewhisperer:us-east-1:123456789012:profile/REAL123",
            })
        );

        // BuilderID 占位符仍不应作为 Enterprise profileArn 外发。
        let builder_credentials = KiroCredentials {
            profile_arn: Some(
                crate::kiro::model::credentials::BUILDER_ID_PROFILE_ARN.to_string(),
            ),
            ..Default::default()
        };
        assert_eq!(
            set_user_preference_body(&builder_credentials, "DISABLED"),
            serde_json::json!({
                "overageConfiguration": { "overageStatus": "DISABLED" },
            })
        );
    }

    #[test]
    fn test_credential_region_empty_string_treated_as_set() {
        // 空字符串 auth_region 被视为已设置（虽然不推荐，但行为应一致）
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("".to_string());

        let region = credentials.effective_auth_region(&config);
        // 空字符串被视为已设置，不会回退到 config
        assert_eq!(region, "");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响
        let mut config = Config::default();
        config.region = "default".to_string();

        let mut credentials = KiroCredentials::default();
        credentials.auth_region = Some("auth-only".to_string());
        credentials.api_region = Some("api-only".to_string());

        assert_eq!(credentials.effective_auth_region(&config), "auth-only");
        assert_eq!(credentials.effective_api_region(&config), "api-only");
    }

    // ── is_multiple_format 自动升级 ──────────────────────────────────────────

    fn tmp_creds_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("kiro_test_{}.json", name));
        p
    }

    /// 单凭据格式（is_multiple_format=false）启动时自动迁移为数组格式，
    /// 迁移后 persist_credentials 能正确写盘，token rotation 不再丢失。
    #[test]
    fn test_single_format_auto_migrates_to_multiple_on_startup() {
        let path = tmp_creds_path("single_migrate");
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_test_migrate_key".to_string());
        cred.auth_method = Some("api_key".to_string());
        let single_json = serde_json::to_string(&cred).unwrap();
        std::fs::write(&path, &single_json).unwrap();

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(path.clone()),
            false,
        )
        .unwrap();

        assert!(
            manager.is_multiple_format.load(Ordering::Relaxed),
            "单凭据格式应在启动时自动升级为 true"
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.trim_start().starts_with('['),
            "迁移后文件应为数组格式，实际: {}",
            &content[..content.len().min(50)]
        );

        let _ = std::fs::remove_file(&path);
    }

    /// 空凭据列表时不触发迁移
    #[test]
    fn test_empty_credentials_no_migration() {
        let path = tmp_creds_path("empty_no_migrate");
        std::fs::write(&path, "{}").unwrap();

        let manager =
            MultiTokenManager::new(Config::default(), vec![], None, Some(path.clone()), false)
                .unwrap();

        assert!(
            !manager.is_multiple_format.load(Ordering::Relaxed),
            "无凭据时不应触发格式升级"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// add_credential 后 is_multiple_format 必须升级为 true，文件写为数组格式
    #[tokio::test(flavor = "multi_thread")]
    async fn test_add_credential_upgrades_multiple_format() {
        let path = tmp_creds_path("add_cred_upgrade");
        std::fs::write(&path, "[]").unwrap();

        let manager =
            MultiTokenManager::new(Config::default(), vec![], None, Some(path.clone()), false)
                .unwrap();

        assert!(!manager.is_multiple_format.load(Ordering::Relaxed));

        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_test_upgrade_key".to_string());
        cred.auth_method = Some("api_key".to_string());

        manager.add_credential(cred).await.unwrap();

        assert!(
            manager.is_multiple_format.load(Ordering::Relaxed),
            "add_credential 后 is_multiple_format 应升级为 true"
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.trim_start().starts_with('['),
            "add_credential 后文件应为数组格式"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_add_credential_does_not_reuse_deleted_id() {
        let path = tmp_creds_path("add_cred_no_reuse_deleted_id");
        let mut cred1 = KiroCredentials::default();
        cred1.id = Some(1);
        cred1.kiro_api_key = Some("ksk_existing_1".to_string());
        cred1.auth_method = Some("api_key".to_string());

        let mut cred2 = KiroCredentials::default();
        cred2.id = Some(2);
        cred2.kiro_api_key = Some("ksk_existing_2".to_string());
        cred2.auth_method = Some("api_key".to_string());

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![cred1, cred2],
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();

        manager.delete_credential(2).unwrap();

        let mut new_cred = KiroCredentials::default();
        new_cred.kiro_api_key = Some("ksk_new_3".to_string());
        new_cred.auth_method = Some("api_key".to_string());

        let new_id = manager.add_credential(new_cred).await.unwrap();
        assert_eq!(
            new_id, 3,
            "new credential IDs must not reuse deleted IDs, otherwise historical failure logs attach to the new account"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── 并发去重（TOCTOU 回归守卫） ───────────────────────────────────────────

    /// 并发添加多个相同的 API Key 凭据，必须只插入一条。
    ///
    /// `add_credential` 的去重预检（步骤 2）与插入（步骤 5）不在同一临界区，
    /// token 刷新（网络）在锁外完成。8 个并发任务极易有多个同时通过预检，
    /// 此时不带"插入点权威重检"的实现会让重复凭据全部插入。本测试即为此回归守卫。
    /// 选用 API Key 凭据是为了跳过网络刷新，使竞态可在纯本地复现。
    #[tokio::test(flavor = "multi_thread")]
    async fn test_concurrent_add_same_api_key_inserts_once() {
        let path = tmp_creds_path("concurrent_dedup");
        let manager = Arc::new(
            MultiTokenManager::new(Config::default(), vec![], None, Some(path.clone()), true)
                .unwrap(),
        );

        const N: usize = 8;
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let m = Arc::clone(&manager);
            handles.push(tokio::spawn(async move {
                let mut c = KiroCredentials::default();
                c.kiro_api_key = Some("ksk_duplicate".to_string());
                c.auth_method = Some("api_key".to_string());
                m.add_credential(c).await
            }));
        }

        let mut ok_count = 0_usize;
        for h in handles {
            if h.await.unwrap().is_ok() {
                ok_count += 1;
            }
        }
        assert_eq!(
            ok_count, 1,
            "并发添加同一凭据应只成功一次，实际成功 {ok_count} 次"
        );

        let snapshot = manager.snapshot();
        assert_eq!(
            snapshot.entries.len(),
            1,
            "应只插入一条相同凭据，实际 {} 条",
            snapshot.entries.len()
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── try_reload_credential_from_file ─────────────────────────────────────

    /// 文件中有新 refreshToken 时，reload 返回 true 并更新内存凭据
    #[test]
    fn test_reload_from_file_succeeds_when_token_rotated() {
        let path = tmp_creds_path("reload_rotated");

        // 初始 token
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("original_token_aaaa".repeat(10));
        let initial_json = serde_json::to_vec_pretty(&[&cred]).unwrap();
        std::fs::write(&path, &initial_json).unwrap();

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();

        seed_model_cache(&manager, 1, &["glm-5"]);

        // 模拟 IDE rotation：文件写入新 token
        let mut updated_cred = KiroCredentials::default();
        updated_cred.id = Some(1);
        updated_cred.refresh_token = Some("rotated_token_bbbb".repeat(10));
        updated_cred.access_token = Some("new_access".to_string());
        let updated_json = serde_json::to_vec_pretty(&[&updated_cred]).unwrap();
        std::fs::write(&path, &updated_json).unwrap();

        let reloaded = manager.try_reload_credential_from_file(1);
        assert!(reloaded, "文件中有新 token，reload 应返回 true");

        let snapshot = manager.snapshot();
        let entry = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(!entry.disabled, "reload 后凭据应重新启用");
        assert_eq!(entry.failure_count, 0);
        assert_eq!(
            manager.cached_model_support(1, Some("glm-5")),
            CachedModelSupport::Unknown
        );

        let _ = std::fs::remove_file(&path);
    }

    /// 文件 token 与内存相同时，reload 返回 false（无更新可用）
    #[test]
    fn test_reload_from_file_returns_false_when_token_unchanged() {
        let path = tmp_creds_path("reload_unchanged");

        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("same_token".repeat(15));
        let json = serde_json::to_vec_pretty(&[&cred]).unwrap();
        std::fs::write(&path, &json).unwrap();

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();

        let reloaded = manager.try_reload_credential_from_file(1);
        assert!(!reloaded, "token 未变化，reload 应返回 false");

        let _ = std::fs::remove_file(&path);
    }

    /// 未配置 credentials_path 时，reload 返回 false
    #[test]
    fn test_reload_from_file_returns_false_without_path() {
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("some_token".repeat(15));

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            None, // 无文件路径
            false,
        )
        .unwrap();

        let reloaded = manager.try_reload_credential_from_file(1);
        assert!(!reloaded, "无 credentials_path 时应返回 false");
    }

    /// 单凭据文件无 ID 字段时，通过单凭据规则匹配
    #[test]
    fn test_reload_from_file_single_credential_no_id() {
        let path = tmp_creds_path("reload_single_no_id");

        // 初始：无 ID 字段
        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("original_no_id".repeat(10));
        let initial_json = serde_json::to_vec_pretty(&[&cred]).unwrap();
        std::fs::write(&path, &initial_json).unwrap();

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();

        // 文件更新为新 token（无 ID）
        let mut updated = KiroCredentials::default();
        updated.refresh_token = Some("rotated_no_id".repeat(10));
        let updated_json = serde_json::to_vec_pretty(&[&updated]).unwrap();
        std::fs::write(&path, &updated_json).unwrap();

        // 获取实际 ID（manager 自动分配）
        let actual_id = manager.snapshot().entries[0].id;
        let reloaded = manager.try_reload_credential_from_file(actual_id);
        assert!(reloaded, "单凭据无 ID 时仍应能匹配并 reload");

        let _ = std::fs::remove_file(&path);
    }

    /// issue #23 修复：persist_credentials 原子落盘——写盘成功、内容为合法 JSON、
    /// 且不残留临时文件（tmp+rename）。
    #[test]
    fn persist_credentials_writes_atomically_no_tmp_residue() {
        let path = tmp_creds_path("persist_atomic");
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("tok_aaaa".repeat(5));
        std::fs::write(&path, serde_json::to_vec_pretty(&[&cred]).unwrap()).unwrap();
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![cred],
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();

        assert!(manager.persist_credentials().unwrap(), "persist 应写盘成功");

        // 文件为合法 JSON 数组
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.is_array(), "凭据文件应为 JSON 数组");
        // 原子落盘后不应残留临时文件
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "原子落盘后不应残留临时文件");

        let _ = std::fs::remove_file(&path);
    }

    // ===== 账号分组隔离回归测试 =====

    /// 构造一个带 token、属于指定分组的可用凭据
    fn grouped_cred(token: &str, groups: &[&str]) -> KiroCredentials {
        let mut c = KiroCredentials::default();
        c.access_token = Some(token.to_string());
        c.expires_at = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
        c.groups = groups.iter().map(|s| s.to_string()).collect();
        c
    }

    fn model_response(ids: &[&str]) -> ListAvailableModelsResponse {
        ListAvailableModelsResponse {
            models: ids
                .iter()
                .map(|id| UpstreamModel {
                    model_id: (*id).to_string(),
                    model_name: None,
                    description: None,
                    token_limits: None,
                })
                .collect(),
        }
    }

    fn seed_model_cache(manager: &MultiTokenManager, id: u64, models: &[&str]) {
        manager.model_cache.lock().insert(
            id,
            ModelCacheEntry {
                response: model_response(models),
                refreshed_at: Instant::now(),
            },
        );
    }

    #[test]
    fn test_model_cache_fresh_ttl_and_stale_lookup() {
        let mut config = Config::default();
        config.model_cache_ttl_secs = 1;
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();
        manager.model_cache.lock().insert(
            1,
            ModelCacheEntry {
                response: model_response(&["glm-5"]),
                refreshed_at: Instant::now() - StdDuration::from_secs(2),
            },
        );

        assert!(manager.cached_model_response(1, true).is_none());
        assert_eq!(
            manager.cached_model_response(1, false).unwrap().models[0].model_id,
            "glm-5"
        );
    }

    #[tokio::test]
    async fn test_model_cache_refresh_failure_preserves_stale_value() {
        let mut config = Config::default();
        config.model_cache_ttl_secs = 0;
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();
        seed_model_cache(&manager, 1, &["deepseek-3.2"]);

        let response = manager.cached_or_refresh_models_for(1).await.unwrap();
        assert_eq!(response.models[0].model_id, "deepseek-3.2");
        assert!(manager.cached_model_response(1, false).is_some());
    }

    #[test]
    fn test_model_cache_uses_per_credential_singleflight_lock() {
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap();
        let first = manager.model_refresh_lock(42);
        let second = manager.model_refresh_lock(42);
        let other = manager.model_refresh_lock(43);

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other));
    }

    #[test]
    fn test_model_cache_invalidation_on_credential_proxy_change() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![grouped_cred("token", &[])],
            None,
            None,
            false,
        )
        .unwrap();
        seed_model_cache(&manager, 1, &["glm-5"]);

        manager
            .update_credential(
                1,
                None,
                Some(Some("http://127.0.0.1:8080".to_string())),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(
            manager.cached_model_support(1, Some("glm-5")),
            CachedModelSupport::Unknown
        );
    }

    #[test]
    fn test_update_credential_preserves_extensible_metadata() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![grouped_cred("token", &[])],
            None,
            None,
            false,
        )
        .unwrap();
        let metadata = crate::kiro::model::credentials::CredentialMetadata {
            kind: crate::kiro::model::credentials::CredentialType::Boom,
            sale_status: crate::kiro::model::credentials::CredentialSaleStatus::ForSale,
            extra: std::collections::BTreeMap::from([(
                "supplier".to_string(),
                serde_json::Value::String("vendor-a".to_string()),
            )]),
        };

        manager
            .update_credential(1, None, None, None, None, None, None, Some(metadata))
            .unwrap();

        let snapshot = manager.snapshot();
        assert_eq!(
            snapshot.entries[0].metadata.kind,
            crate::kiro::model::credentials::CredentialType::Boom
        );
        assert_eq!(
            snapshot.entries[0].metadata.sale_status,
            crate::kiro::model::credentials::CredentialSaleStatus::ForSale
        );
        assert_eq!(
            snapshot.entries[0].metadata.extra.get("supplier"),
            Some(&serde_json::Value::String("vendor-a".to_string()))
        );
    }

    #[tokio::test]
    async fn test_priority_routing_prefers_priority_over_unknown_model_cache() {
        let mut confirmed = grouped_cred("confirmed", &[]);
        confirmed.priority = 10;
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![grouped_cred("unknown", &[]), confirmed],
            None,
            None,
            false,
        )
        .unwrap();
        seed_model_cache(&manager, 2, &["minimax-m2.5"]);

        let context = manager
            .acquire_context(Some("minimax-m2.5"), None)
            .await
            .unwrap();
        assert_eq!(context.id, 1);
    }

    #[tokio::test]
    async fn test_model_routing_skips_explicitly_unsupported_cache() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![
                grouped_cred("unsupported", &[]),
                grouped_cred("supported", &[]),
            ],
            None,
            None,
            false,
        )
        .unwrap();
        seed_model_cache(&manager, 1, &["glm-5"]);
        seed_model_cache(&manager, 2, &["deepseek-3.2"]);

        let context = manager
            .acquire_context(Some("deepseek-3.2"), None)
            .await
            .unwrap();
        assert_eq!(context.id, 2);
    }

    #[tokio::test]
    async fn test_model_routing_allows_passthrough_without_cache() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![grouped_cred("unknown", &[])],
            None,
            None,
            false,
        )
        .unwrap();

        let context = manager
            .acquire_context(Some("future-model"), None)
            .await
            .unwrap();
        assert_eq!(context.id, 1);
    }

    #[tokio::test]
    async fn test_model_discovery_respects_group_and_reports_cold_start_failure() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials {
                groups: vec!["g1".to_string()],
                ..KiroCredentials::default()
            }],
            None,
            None,
            false,
        )
        .unwrap();

        assert!(matches!(
            manager.discover_models_for_group(Some("g2")).await,
            Err(ModelDiscoveryError::NoAvailableCredentials)
        ));
        assert!(matches!(
            manager.discover_models_for_group(Some("g1")).await,
            Err(ModelDiscoveryError::ColdStartFailed {
                credential_count: 1
            })
        ));
    }

    #[tokio::test]
    async fn test_model_discovery_keeps_partial_cached_success() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![
                grouped_cred("cached", &["g1"]),
                KiroCredentials {
                    groups: vec!["g1".to_string()],
                    ..KiroCredentials::default()
                },
            ],
            None,
            None,
            false,
        )
        .unwrap();
        seed_model_cache(&manager, 1, &["glm-5"]);

        let models = manager.discover_models_for_group(Some("g1")).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "glm-5");
    }

    #[test]
    fn test_group_matches_helper() {
        // 未绑定分组(None)匹配任何账号
        assert!(group_matches(&[], None));
        assert!(group_matches(&["g1".to_string()], None));
        // 绑定分组时只匹配 groups 含该名的账号
        assert!(group_matches(
            &["g1".to_string(), "g2".to_string()],
            Some("g1")
        ));
        assert!(!group_matches(&["g2".to_string()], Some("g1")));
        assert!(!group_matches(&[], Some("g1")));
    }

    #[test]
    fn test_select_next_credential_filters_by_group() {
        // A∈g1, B∈g2, C∈无分组
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![
                grouped_cred("a", &["g1"]),
                grouped_cred("b", &["g2"]),
                grouped_cred("c", &[]),
            ],
            None,
            None,
            false,
        )
        .unwrap();

        // g1 只能选到 A(id=1)
        let g1 = manager.select_next_credential(None, Some("g1"));
        assert_eq!(g1.map(|(id, _)| id), Some(1));
        // g2 只能选到 B(id=2)
        let g2 = manager.select_next_credential(None, Some("g2"));
        assert_eq!(g2.map(|(id, _)| id), Some(2));
        // 不存在的分组 → 无可用账号
        assert!(manager.select_next_credential(None, Some("nope")).is_none());
        // 未绑定分组(None) → 可选到账号
        assert!(manager.select_next_credential(None, None).is_some());
    }

    #[tokio::test]
    async fn test_priority_mode_ignores_stale_current_id() {
        let mut first = grouped_cred("first", &[]);
        first.priority = 0;
        let mut second = grouped_cred("second", &[]);
        second.priority = 10;
        let manager =
            MultiTokenManager::new(Config::default(), vec![first, second], None, None, false)
                .unwrap();

        assert!(manager.switch_to_next());
        assert_eq!(manager.snapshot().current_id, 2);

        let context = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(context.id, 1);
        assert_eq!(manager.snapshot().current_id, 1);
    }

    #[tokio::test]
    async fn test_priority_mode_selects_smallest_priority_in_request_group() {
        let mut other_group = grouped_cred("other", &["g2"]);
        other_group.priority = 0;
        let mut later_in_group = grouped_cred("later", &["g1"]);
        later_in_group.priority = 20;
        let mut first_in_group = grouped_cred("first", &["g1"]);
        first_in_group.priority = 10;
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![other_group, later_in_group, first_in_group],
            None,
            None,
            false,
        )
        .unwrap();

        let context = manager.acquire_context(None, Some("g1")).await.unwrap();
        assert_eq!(context.id, 3);
        assert_eq!(manager.snapshot().current_id, 3);
    }

    #[tokio::test]
    async fn test_priority_mode_falls_through_at_rpm_limit_and_switches_back() {
        let mut config = Config::default();
        config.account_rpm_limit_enabled = true;
        config.account_rpm_limit = 1;

        let mut first = grouped_cred("first", &[]);
        first.priority = 0;
        let mut second = grouped_cred("second", &[]);
        second.priority = 10;
        let manager = MultiTokenManager::new(config, vec![first, second], None, None, false).unwrap();

        let first_context = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(first_context.id, 1);

        let fallback_context = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(fallback_context.id, 2);

        {
            let mut entries = manager.entries.lock();
            let expired = Instant::now() - StdDuration::from_secs(RPM_WINDOW_SECS + 1);
            for timestamp in entries[0].rpm_window.iter_mut() {
                *timestamp = expired;
            }
        }

        let recovered_context = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(recovered_context.id, 1);
    }

    #[tokio::test]
    async fn test_priority_mode_switches_back_after_higher_priority_is_enabled() {
        let mut first = grouped_cred("first", &[]);
        first.priority = 0;
        let mut second = grouped_cred("second", &[]);
        second.priority = 10;
        let manager =
            MultiTokenManager::new(Config::default(), vec![first, second], None, None, false)
                .unwrap();

        manager.set_disabled(1, true).unwrap();
        let fallback_context = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(fallback_context.id, 2);

        manager.set_disabled(1, false).unwrap();
        let recovered_context = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(recovered_context.id, 1);
    }

    #[tokio::test]
    async fn test_priority_mode_uses_id_to_break_equal_priority_ties() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![grouped_cred("first", &[]), grouped_cred("second", &[])],
            None,
            None,
            false,
        )
        .unwrap();

        assert!(manager.switch_to_next());
        assert_eq!(manager.snapshot().current_id, 2);

        let context = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(context.id, 1);
    }

    #[tokio::test]
    async fn test_acquire_context_priority_current_respects_model_support() {
        let mut free_cred = grouped_cred("free", &[]);
        free_cred.subscription_title = Some("KIRO FREE".to_string());

        let mut pro_cred = grouped_cred("pro", &[]);
        pro_cred.subscription_title = Some("KIRO PRO".to_string());
        pro_cred.priority = 10;

        let manager = MultiTokenManager::new(
            Config::default(),
            vec![free_cred, pro_cred],
            None,
            None,
            false,
        )
        .unwrap();

        // Warm current_id with the highest-priority Free account.
        let current = manager.acquire_context(None, None).await.unwrap();
        assert_eq!(current.id, 1);

        let opus = manager
            .acquire_context(Some("claude-opus-4.6"), None)
            .await
            .unwrap();
        assert_eq!(
            opus.id, 2,
            "priority current_id must not bypass Opus subscription filtering"
        );
    }

    #[test]
    fn test_total_count_in_group() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![
                grouped_cred("a", &["g1"]),
                grouped_cred("b", &["g1", "g2"]),
                grouped_cred("c", &[]),
            ],
            None,
            None,
            false,
        )
        .unwrap();

        assert_eq!(manager.total_count_in_group(Some("g1")), 2); // A,B
        assert_eq!(manager.total_count_in_group(Some("g2")), 1); // B
        assert_eq!(manager.total_count_in_group(None), 3); // 全部
        assert_eq!(manager.total_count_in_group(Some("none")), 0);
    }

    #[test]
    fn test_available_count_for_request_respects_group_throttle() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![grouped_cred("a", &["g1"]), grouped_cred("b", &["g2"])],
            None,
            None,
            false,
        )
        .unwrap();

        assert_eq!(
            manager
                .select_next_credential(None, Some("g1"))
                .map(|(id, _)| id),
            Some(1)
        );
        // g1 的唯一凭据进入冷却后，即使全局还有 g2，g1 也必须视为无可用账号。
        assert_eq!(
            manager.report_account_throttled_for_request(
                1,
                StdDuration::from_secs(60),
                None,
                Some("g1"),
            ),
            0
        );
        assert!(manager.select_next_credential(None, Some("g1")).is_none());
        assert_eq!(
            manager
                .select_next_credential(None, Some("g2"))
                .map(|(id, _)| id),
            Some(2)
        );
        assert_eq!(manager.snapshot().available, 1);
    }

    #[test]
    fn test_balanced_mode_independent_per_group() {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();
        // g1: A(id1),B(id2)；g2: C(id3)
        let manager = MultiTokenManager::new(
            config,
            vec![
                grouped_cred("a", &["g1"]),
                grouped_cred("b", &["g1"]),
                grouped_cred("c", &["g2"]),
            ],
            None,
            None,
            false,
        )
        .unwrap();

        // 让 A(id1) 成功若干次 → balanced 应转向 success_count 更小的 B(id2)
        manager.report_success(1);
        manager.report_success(1);
        let pick = manager.select_next_credential(None, Some("g1"));
        assert_eq!(
            pick.map(|(id, _)| id),
            Some(2),
            "balanced 应在 g1 内选 success_count 最小的 B"
        );
        // g2 不受 g1 计数影响，仍只会选到 C(id3)
        let pick_g2 = manager.select_next_credential(None, Some("g2"));
        assert_eq!(pick_g2.map(|(id, _)| id), Some(3));
    }

    #[tokio::test]
    async fn test_acquire_context_strict_isolation_fails_when_group_empty() {
        // g1 只有一个账号 A(id1)，禁用后绑定 g1 的请求应直接失败，不回退到 g2/无分组
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![
                grouped_cred("a", &["g1"]),
                grouped_cred("b", &["g2"]),
                grouped_cred("c", &[]),
            ],
            None,
            None,
            false,
        )
        .unwrap();

        // 正常情况下 g1 能拿到 context
        assert!(manager.acquire_context(None, Some("g1")).await.is_ok());

        // 手动禁用 g1 内唯一账号 A(id1)
        manager.set_disabled(1, true).unwrap();

        // 严格隔离：g1 无可用账号 → Err，且不会选到 B/C
        let res = manager.acquire_context(None, Some("g1")).await;
        assert!(res.is_err(), "g1 内全部账号禁用后应失败，不回退到其他分组");

        // 但 g2 仍可用
        assert!(manager.acquire_context(None, Some("g2")).await.is_ok());
    }
}
