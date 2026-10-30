//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use crate::admin::client_keys::{KeyAuth, SharedClientKeyManager};
use crate::admin::trace_db::{SharedTraceStore, TraceKeySource};
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder};
use crate::common::auth;
use crate::kiro::provider::KiroProvider;

use super::cache_metering::SharedCacheMeter;
use super::types::ErrorResponse;

/// 命中的鉴权上下文（注入到请求扩展，供 handler 记录用量）
#[derive(Clone, Debug)]
pub struct KeyContext {
    /// 命中的客户端 Key id
    pub key_id: u64,
    /// 该 Key 绑定的账号分组；None 表示未绑定，可使用全部账号
    pub group: Option<String>,
    /// 命中的入口 Key 类型。
    pub key_source: TraceKeySource,
    /// 客户端 IP（转发头优先，回落到 TCP 对端），仅用于请求日志
    pub client_ip: Option<String>,
}

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// 工具兼容模式（ClaudeCode 内置工具名/入参双向适配 / Raw 透传）
    pub tool_compatibility_mode: crate::model::config::ToolCompatibilityMode,
    /// 客户端 Key 管理器（可选，未启用 Admin 时为 None）
    pub client_keys: Option<SharedClientKeyManager>,
    /// 用量日志记录器
    pub usage_recorder: Option<SharedRecorder>,
    /// 用量聚合器
    pub usage_aggregator: Option<SharedAggregator>,
    /// 中转层缓存计量（基于 cache_control 断点的内存缓存）
    pub cache_meter: Option<SharedCacheMeter>,
    /// 请求链路追踪存储（SQLite，可选）
    pub trace_store: Option<SharedTraceStore>,
    /// 自定义响应头管理器（热重载）
    pub custom_headers: Option<crate::model::custom_headers::CustomHeadersManager>,
}

impl AppState {
    /// 创建新的应用状态（不含 client_keys 的基础构造，供嵌入 / 测试使用）
    #[allow(dead_code)]
    pub fn new(
        extract_thinking: bool,
        tool_compatibility_mode: crate::model::config::ToolCompatibilityMode,
    ) -> Self {
        Self {
            kiro_provider: None,
            extract_thinking,
            tool_compatibility_mode,
            client_keys: None,
            usage_recorder: None,
            usage_aggregator: None,
            cache_meter: None,
            trace_store: None,
            custom_headers: None,
        }
    }

    /// 注入可与 Admin 控制面共享的 KiroProvider。
    pub fn with_shared_kiro_provider(mut self, provider: Arc<KiroProvider>) -> Self {
        self.kiro_provider = Some(provider);
        self
    }

    /// 注入用量记录组件
    pub fn with_usage(
        mut self,
        client_keys: Option<SharedClientKeyManager>,
        recorder: Option<SharedRecorder>,
        aggregator: Option<SharedAggregator>,
    ) -> Self {
        self.client_keys = client_keys;
        self.usage_recorder = recorder;
        self.usage_aggregator = aggregator;
        self
    }

    /// 注入缓存计量器
    pub fn with_cache_meter(mut self, cache: Option<SharedCacheMeter>) -> Self {
        self.cache_meter = cache;
        self
    }

    /// 注入链路追踪存储
    pub fn with_trace_store(mut self, store: Option<SharedTraceStore>) -> Self {
        self.trace_store = store;
        self
    }

    /// 注入自定义响应头管理器
    pub fn with_custom_headers(
        mut self,
        custom_headers: Option<crate::model::custom_headers::CustomHeadersManager>,
    ) -> Self {
        self.custom_headers = custom_headers;
        self
    }
}

/// API Key 认证中间件
///
/// 所有入口 Key 统一按已存储的完整值精确匹配，不限制前缀。命中后向请求扩展注入
/// [`KeyContext`]，供 handler 记录用量时使用。
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let presented = match auth::extract_api_key(&request) {
        Some(k) => k,
        None => {
            let error = ErrorResponse::authentication_error();
            return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
        }
    };

    if let Some(mgr) = &state.client_keys {
        match mgr.verify_and_touch_ex(&presented) {
            KeyAuth::Ok(id) => {
                let group = mgr.group_of(id);
                let client_ip = auth::extract_client_ip(&request);
                request.extensions_mut().insert(KeyContext {
                    key_id: id,
                    group,
                    key_source: TraceKeySource::ClientKey,
                    client_ip,
                });
                return next.run(request).await;
            }
            KeyAuth::OverLimit { used, limit, .. } => {
                let error = ErrorResponse::new(
                    "rate_limit_error",
                    format!(
                        "该 API Key 已达到积分使用上限（已用 {:.2} / 上限 {:.2}），请联系管理员调整额度或重置统计",
                        used, limit
                    ),
                );
                return (StatusCode::TOO_MANY_REQUESTS, Json(error)).into_response();
            }
            KeyAuth::NotFound => {}
        }
    }

    let error = ErrorResponse::authentication_error();
    (StatusCode::UNAUTHORIZED, Json(error)).into_response()
}

/// CORS 中间件层
///
/// **安全说明**：当前配置允许所有来源（Any），这是为了支持公开 API 服务。
/// 如果需要更严格的安全控制，请根据实际需求配置具体的允许来源、方法和头信息。
///
/// # 配置说明
/// - `allow_origin(Any)`: 允许任何来源的请求
/// - `allow_methods(Any)`: 允许任何 HTTP 方法
/// - `allow_headers(Any)`: 允许任何请求头
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}
