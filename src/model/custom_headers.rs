//! 自定义响应头规则与动态变量插值管理器
//!
//! 允许用户在 UI / config.json 中自由定制下发给客户端的 HTTP 响应头。
//! 键名完全自定义，值支持从运行时变量动态插值（例如 `{trace_id}`, `{model}` 等），
//! 支持同时下发多个响应头并支持单独启用/停用。

use std::sync::Arc;

use axum::http::header::{HeaderName, HeaderValue};
use axum::response::Response;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// 单条自定义响应头规则
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CustomHeaderRule {
    /// 响应头名称（如 X-Oneapi-Request-Id, X-Request-Id, X-Trace-Id 等）
    pub key: String,
    /// 响应头模板值（支持变量插值，如 `{trace_id}`, `{model}`, `{credential_id}` 等）
    pub value: String,
    /// 是否启用此响应头
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// 默认配置的响应头列表（开箱即用支持 NewAPI / OneAPI 的 upstream_request_id 提取）
pub fn default_custom_headers() -> Vec<CustomHeaderRule> {
    vec![
        CustomHeaderRule {
            key: "X-Oneapi-Request-Id".to_string(),
            value: "{trace_id}".to_string(),
            enabled: true,
        },
        CustomHeaderRule {
            key: "X-Request-Id".to_string(),
            value: "{trace_id}".to_string(),
            enabled: true,
        },
    ]
}

/// 响应头变量插值上下文
#[derive(Debug, Clone, Default)]
pub struct HeaderInterpolationContext<'a> {
    /// 请求全局唯一 Trace ID
    pub trace_id: &'a str,
    /// 客户端请求的模型名称
    pub model: &'a str,
    /// 客户端 API Key ID（如未认证或主密钥通常为 0）
    pub key_id: Option<u64>,
    /// 分组名称（如 default, vip 等）
    pub group: Option<&'a str>,
    /// 本次请求命中的上游凭据 ID
    pub credential_id: Option<u64>,
    /// 客户端 IP 地址
    pub client_ip: Option<&'a str>,
}

impl<'a> HeaderInterpolationContext<'a> {
    /// 对模板字符串中的占位变量进行替换
    pub fn interpolate(&self, template: &str) -> String {
        let mut result = template.to_string();
        result = result.replace("{trace_id}", self.trace_id);
        result = result.replace("{request_id}", self.trace_id);
        result = result.replace("{model}", self.model);
        if let Some(key_id) = self.key_id {
            result = result.replace("{key_id}", &key_id.to_string());
        } else {
            result = result.replace("{key_id}", "");
        }
        result = result.replace("{group}", self.group.unwrap_or(""));
        if let Some(cred_id) = self.credential_id {
            result = result.replace("{credential_id}", &cred_id.to_string());
        } else {
            result = result.replace("{credential_id}", "");
        }
        result = result.replace("{client_ip}", self.client_ip.unwrap_or(""));
        result
    }
}

/// 线程安全的热重载自定义响应头管理器
#[derive(Clone)]
pub struct CustomHeadersManager {
    rules: Arc<RwLock<Vec<CustomHeaderRule>>>,
}

impl CustomHeadersManager {
    pub fn new(rules: Vec<CustomHeaderRule>) -> Self {
        Self {
            rules: Arc::new(RwLock::new(rules)),
        }
    }

    /// 获取当前所有规则快照
    pub fn get_rules(&self) -> Vec<CustomHeaderRule> {
        self.rules.read().clone()
    }

    /// 热更新全部规则
    pub fn set_rules(&self, new_rules: Vec<CustomHeaderRule>) {
        *self.rules.write() = new_rules;
    }

    /// 获取所有当前启用的 HeaderName 列表
    pub fn get_active_header_names(&self) -> Vec<HeaderName> {
        self.rules
            .read()
            .iter()
            .filter(|r| r.enabled)
            .filter_map(|r| HeaderName::from_bytes(r.key.trim().as_bytes()).ok())
            .collect()
    }

    /// 根据插值上下文，向 HTTP 响应对象注入所有启用的自定义响应头
    pub fn attach_to_response(&self, resp: &mut Response, ctx: &HeaderInterpolationContext) {
        let rules = self.rules.read();
        for rule in rules.iter() {
            if !rule.enabled {
                continue;
            }
            let key = rule.key.trim();
            if key.is_empty() {
                continue;
            }
            let Ok(header_name) = HeaderName::from_bytes(key.as_bytes()) else {
                continue;
            };
            let interpolated = ctx.interpolate(&rule.value);
            // 过滤 CRLF 防止 HTTP 响应头分割（Header Splitting）攻击
            let sanitized = interpolated.replace(['\r', '\n'], "");
            if let Ok(header_value) = HeaderValue::from_str(&sanitized) {
                resp.headers_mut().insert(header_name, header_value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;

    #[test]
    fn test_default_custom_headers() {
        let defaults = default_custom_headers();
        assert_eq!(defaults.len(), 2);
        assert_eq!(defaults[0].key, "X-Oneapi-Request-Id");
        assert_eq!(defaults[0].value, "{trace_id}");
        assert!(defaults[0].enabled);
        assert_eq!(defaults[1].key, "X-Request-Id");
        assert_eq!(defaults[1].value, "{trace_id}");
        assert!(defaults[1].enabled);
    }

    #[test]
    fn test_interpolation() {
        let ctx = HeaderInterpolationContext {
            trace_id: "0fc99995-b8cc-46b9-bf25-c44d0e3b1447",
            model: "claude-3-7-sonnet",
            key_id: Some(1024),
            group: Some("vip"),
            credential_id: Some(7),
            client_ip: Some("192.168.1.100"),
        };

        assert_eq!(
            ctx.interpolate("{trace_id}"),
            "0fc99995-b8cc-46b9-bf25-c44d0e3b1447"
        );
        assert_eq!(
            ctx.interpolate("{request_id}"),
            "0fc99995-b8cc-46b9-bf25-c44d0e3b1447"
        );
        assert_eq!(ctx.interpolate("{model}"), "claude-3-7-sonnet");
        assert_eq!(ctx.interpolate("{key_id}"), "1024");
        assert_eq!(ctx.interpolate("{group}"), "vip");
        assert_eq!(ctx.interpolate("{credential_id}"), "7");
        assert_eq!(ctx.interpolate("{client_ip}"), "192.168.1.100");
        assert_eq!(
            ctx.interpolate("prefix-{group}-{model}-suffix"),
            "prefix-vip-claude-3-7-sonnet-suffix"
        );
        assert_eq!(ctx.interpolate("static-value"), "static-value");
    }

    #[test]
    fn test_attach_to_response() {
        let rules = vec![
            CustomHeaderRule {
                key: "X-Oneapi-Request-Id".to_string(),
                value: "{trace_id}".to_string(),
                enabled: true,
            },
            CustomHeaderRule {
                key: "X-Upstream-Model".to_string(),
                value: "{model}".to_string(),
                enabled: true,
            },
            CustomHeaderRule {
                key: "X-Disabled-Header".to_string(),
                value: "should-not-exist".to_string(),
                enabled: false,
            },
        ];
        let manager = CustomHeadersManager::new(rules);
        let mut resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap();

        let ctx = HeaderInterpolationContext {
            trace_id: "test-trace-123",
            model: "claude-opus-4-6",
            key_id: Some(1),
            group: None,
            credential_id: Some(42),
            client_ip: None,
        };

        manager.attach_to_response(&mut resp, &ctx);

        assert_eq!(
            resp.headers().get("x-oneapi-request-id").unwrap(),
            "test-trace-123"
        );
        assert_eq!(
            resp.headers().get("x-upstream-model").unwrap(),
            "claude-opus-4-6"
        );
        assert!(resp.headers().get("x-disabled-header").is_none());
    }
}
