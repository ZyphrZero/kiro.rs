//! Admin API 错误类型定义

use std::fmt;

use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

use super::types::AdminErrorResponse;

/// Admin 服务错误类型
#[derive(Debug)]
pub enum AdminServiceError {
    /// 凭据不存在
    NotFound { id: u64 },

    /// 上游服务调用失败（网络、API 错误等）
    UpstreamError(String),

    InvalidBalance { id: u64 },

    /// 上游明确返回限流，可选携带合法 Retry-After。
    RateLimited { retry_after: Option<String> },

    /// 内部状态错误
    InternalError(String),

    /// 凭据无效（验证失败）
    InvalidCredential(String),
}

impl fmt::Display for AdminServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdminServiceError::NotFound { id } => {
                write!(f, "凭据不存在: {}", id)
            }
            AdminServiceError::UpstreamError(_) => write!(f, "上游服务请求失败"),
            AdminServiceError::InvalidBalance { id } => write!(f, "凭据 #{id} 的上游额度数据缺失或无效"),
            AdminServiceError::RateLimited { .. } => write!(f, "上游请求过于频繁，请稍后重试"),
            AdminServiceError::InternalError(msg) => write!(f, "内部错误: {}", msg),
            AdminServiceError::InvalidCredential(msg) => write!(f, "凭据无效: {}", msg),
        }
    }
}

impl std::error::Error for AdminServiceError {}

impl AdminServiceError {
    /// 获取对应的 HTTP 状态码
    pub fn status_code(&self) -> StatusCode {
        match self {
            AdminServiceError::NotFound { .. } => StatusCode::NOT_FOUND,
            AdminServiceError::UpstreamError(_) => StatusCode::BAD_GATEWAY,
            AdminServiceError::InvalidBalance { .. } => StatusCode::BAD_GATEWAY,
            AdminServiceError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            AdminServiceError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AdminServiceError::InvalidCredential(_) => StatusCode::BAD_REQUEST,
        }
    }

    /// 转换为 API 错误响应
    pub fn into_response(self) -> AdminErrorResponse {
        match &self {
            AdminServiceError::NotFound { .. } => AdminErrorResponse::not_found(self.to_string()),
            AdminServiceError::UpstreamError(_) => AdminErrorResponse::api_error(self.to_string()),
            AdminServiceError::InvalidBalance { .. } => AdminErrorResponse::api_error(self.to_string()),
            AdminServiceError::RateLimited { .. } => {
                AdminErrorResponse::rate_limit(self.to_string())
            }
            AdminServiceError::InternalError(_) => {
                AdminErrorResponse::internal_error(self.to_string())
            }
            AdminServiceError::InvalidCredential(_) => {
                AdminErrorResponse::invalid_request(self.to_string())
            }
        }
    }

    pub fn into_http_response(self) -> Response {
        if let AdminServiceError::UpstreamError(message) = &self {
            tracing::warn!(error = %message, "Admin 上游服务请求失败");
        }
        let retry_after = match &self {
            AdminServiceError::RateLimited { retry_after } => retry_after.clone(),
            _ => None,
        };
        let status = self.status_code();
        let mut response = (status, Json(self.into_response())).into_response();
        if let Some(value) = retry_after.and_then(|value| value.parse().ok()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rate_limit_response_has_status_header_and_stable_body() {
        let response = AdminServiceError::RateLimited {
            retry_after: Some("120".to_string()),
        }
        .into_http_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "120");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert!(!body["error"]["message"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn upstream_error_response_does_not_expose_raw_body() {
        let secret = "aws-account=123456789012 request-id=private-request";
        let response = AdminServiceError::UpstreamError(secret.to_string()).into_http_response();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains(secret));
        assert!(body.contains("上游服务请求失败"));
    }
}
