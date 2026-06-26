//! Anthropic API 路由配置

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::SharedTraceStore;
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder};
use crate::kiro::provider::KiroProvider;

use super::{
    cache_metering::SharedCacheMeter,
    handlers::{count_tokens, get_models, post_messages, post_messages_cc},
    middleware::{AppState, auth_middleware, cors_layer},
};

/// 请求体最大大小限制 (50MB)
const MAX_BODY_SIZE: usize = 50 * 1024 * 1024;

/// 创建带有 KiroProvider 的 Anthropic API 路由
///
/// 给嵌入到其他 Rust 项目的下游使用者预留的扩展点。
#[allow(dead_code)]
pub fn create_router_with_provider(
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
) -> Router {
    create_router(
        kiro_provider,
        extract_thinking,
        None,
        None,
        None,
        None,
        None,
        super::middleware::HistoryCapState::default(),
        super::middleware::FastModeState::default(),
    )
}

/// 创建 Anthropic API 路由（供 main.rs 使用）
#[allow(clippy::too_many_arguments)]
pub fn create_router(
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
    client_keys: Option<SharedClientKeyManager>,
    usage_recorder: Option<SharedRecorder>,
    usage_aggregator: Option<SharedAggregator>,
    cache_meter: Option<SharedCacheMeter>,
    trace_store: Option<SharedTraceStore>,
    history_cap: super::middleware::HistoryCapState,
    fast_mode: super::middleware::FastModeState,
) -> Router {
    let mut state = AppState::new(extract_thinking);
    if let Some(provider) = kiro_provider {
        state = state.with_kiro_provider(provider);
    }
    state = state.with_usage(client_keys, usage_recorder, usage_aggregator);
    state = state.with_cache_meter(cache_meter);
    state = state.with_trace_store(trace_store);
    state = state.with_history_cap(history_cap);
    state = state.with_fast_mode(fast_mode);

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
