//! Anthropic API 路由配置

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};
use parking_lot::RwLock;

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder};
use crate::admin::trace_db::SharedTraceStore;
use crate::kiro::provider::KiroProvider;

use super::{
    handlers::{count_tokens, get_models, post_messages, post_messages_cc},
    middleware::{AppState, auth_middleware, cors_layer},
    cache_metering::SharedCacheMeter,
};

/// 请求体最大大小限制 (50MB)
const MAX_BODY_SIZE: usize = 50 * 1024 * 1024;

/// 创建带有 KiroProvider 的 Anthropic API 路由
///
/// 当前默认入口走 [`create_router_with_shared_key`]，本函数是给嵌入到其他 Rust
/// 项目的下游使用者预留的扩展点，因此可能在 lib 内部不被引用。
#[allow(dead_code)]
pub fn create_router_with_provider(
    api_key: impl Into<String>,
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
) -> Router {
    let shared_key = Arc::new(RwLock::new(api_key.into()));
    create_router_with_shared_key(
        shared_key,
        kiro_provider,
        extract_thinking,
        None,
        None,
        None,
        None,
        None,
    )
}

/// 与 `create_router_with_provider` 相同，但允许调用方共享 api_key 内存
/// （Admin 模块通过该 Arc 在运行时改 key 后能立刻生效）
#[allow(clippy::too_many_arguments)]
pub fn create_router_with_shared_key(
    api_key: Arc<RwLock<String>>,
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
    client_keys: Option<SharedClientKeyManager>,
    usage_recorder: Option<SharedRecorder>,
    usage_aggregator: Option<SharedAggregator>,
    cache_meter: Option<SharedCacheMeter>,
    trace_store: Option<SharedTraceStore>,
) -> Router {
    let mut state = AppState::with_shared_api_key(api_key, extract_thinking);
    if let Some(provider) = kiro_provider {
        state = state.with_kiro_provider(provider);
    }
    state = state.with_usage(client_keys, usage_recorder, usage_aggregator);
    state = state.with_cache_meter(cache_meter);
    state = state.with_trace_store(trace_store);

    // 需要认证的 /v1 路由
    let v1_routes = Router::new()
        .route("/models", get(get_models))
        .route("/messages", post(post_messages))
        .route("/messages/count_tokens", post(count_tokens))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // 需要认证的 /cc/v1 路由（Claude Code 兼容端点）
    // 与 /v1 的区别：流式响应会等待 contextUsageEvent 后再发送 message_start
    let cc_v1_routes = Router::new()
        .route("/messages", post(post_messages_cc))
        .route("/messages/count_tokens", post(count_tokens))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .nest("/v1", v1_routes)
        .nest("/cc/v1", cc_v1_routes)
        .layer(cors_layer())
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
}
