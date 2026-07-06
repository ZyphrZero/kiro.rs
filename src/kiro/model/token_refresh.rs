use serde::{Deserialize, Serialize};

/// 刷新 Token 的请求体 (Social 认证)
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// 刷新 Token 的响应体 (Social 认证)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub profile_arn: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

/// IdC Token 刷新请求体 (AWS SSO OIDC)
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdcRefreshRequest {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub grant_type: String,
}

/// IdC Token 刷新响应体 (AWS SSO OIDC)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdcRefreshResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub profile_arn: Option<String>,
}

/// 企业 SSO (external_idp, 如 Azure AD) Token 刷新响应体。
///
/// 标准 OAuth2 token 响应（snake_case）。IdP 不返回 profileArn（由
/// `ListAvailableProfiles` 用 EXTERNAL_IDP token type 另行解析）。
#[derive(Debug, Deserialize)]
pub struct ExternalIdpTokenResponse {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

// ============ AWS SSO OIDC 设备授权流程 ============

/// 注册 OIDC 客户端请求体
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterClientRequest {
    pub client_name: String,
    pub client_type: String,
    pub scopes: Vec<String>,
    pub grant_types: Vec<String>,
    pub issuer_url: String,
}

/// 注册 OIDC 客户端响应体
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterClientResponse {
    pub client_id: String,
    pub client_secret: String,
    // 上游字段，仅用于完整反序列化记录；当前流程不依赖具体值
    #[allow(dead_code)]
    pub client_id_issued_at: Option<i64>,
    #[allow(dead_code)]
    pub client_secret_expires_at: Option<i64>,
}

/// 发起设备授权请求体
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartDeviceAuthorizationRequest {
    pub client_id: String,
    pub client_secret: String,
    pub start_url: String,
}

/// 发起设备授权响应体
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartDeviceAuthorizationResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: i64,
    pub interval: i64,
}

/// 轮询 Token 请求体（设备授权）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTokenRequest {
    pub client_id: String,
    pub client_secret: String,
    pub grant_type: String,
    pub device_code: String,
}

/// 轮询 Token 响应体
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

/// AWS SSO OIDC 错误响应
#[derive(Debug, Deserialize)]
pub struct OidcErrorResponse {
    pub error: String,
    // 详细描述供日志使用，反序列化时保留以便排错
    #[allow(dead_code)]
    #[serde(default)]
    pub error_description: Option<String>,
}

// ============ Social (Portal) 登录流程 ============

/// Social token 交换请求体（PKCE）
#[derive(Debug, Serialize)]
pub struct SocialCreateTokenRequest {
    pub code: String,
    pub code_verifier: String,
    pub redirect_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invitation_code: Option<String>,
}

/// Social token 响应体
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocialCreateTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub profile_arn: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_external_idp_token_response_deserialize() {
        // 标准 OAuth2 snake_case 响应
        let json = r#"{
            "access_token": "eyJ...new",
            "refresh_token": "rt-rotated",
            "expires_in": 3600,
            "token_type": "Bearer"
        }"#;
        let resp: ExternalIdpTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "eyJ...new");
        assert_eq!(resp.refresh_token.as_deref(), Some("rt-rotated"));
        assert_eq!(resp.expires_in, Some(3600));
    }

    #[test]
    fn test_external_idp_token_response_without_refresh_token() {
        // 部分 IdP 刷新时不下发新 refresh_token
        let json = r#"{"access_token":"a","expires_in":900}"#;
        let resp: ExternalIdpTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "a");
        assert!(resp.refresh_token.is_none());
        assert_eq!(resp.expires_in, Some(900));
    }
}
