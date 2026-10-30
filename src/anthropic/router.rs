//! Anthropic API 路由配置

use std::sync::Arc;

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
use crate::model::config::ToolCompatibilityMode;

use super::{
    cache_metering::SharedCacheMeter,
    handlers::{count_tokens, get_models, post_messages, post_messages_cc},
    middleware::{AppState, auth_middleware, cors_layer},
    openai::post_chat_completions,
    responses::post_responses,
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
    tool_compatibility_mode: ToolCompatibilityMode,
) -> Router {
    create_router(
        kiro_provider,
        extract_thinking,
        tool_compatibility_mode,
        None,
        None,
        None,
        None,
        None,
        None,
    )
}

/// 创建 Anthropic API 路由（供 main.rs 使用）
#[allow(clippy::too_many_arguments)]
pub fn create_router(
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
    tool_compatibility_mode: ToolCompatibilityMode,
    client_keys: Option<SharedClientKeyManager>,
    usage_recorder: Option<SharedRecorder>,
    usage_aggregator: Option<SharedAggregator>,
    cache_meter: Option<SharedCacheMeter>,
    trace_store: Option<SharedTraceStore>,
    custom_headers: Option<crate::model::custom_headers::CustomHeadersManager>,
) -> Router {
    create_router_with_shared_provider(
        kiro_provider.map(Arc::new),
        extract_thinking,
        tool_compatibility_mode,
        client_keys,
        usage_recorder,
        usage_aggregator,
        cache_meter,
        trace_store,
        custom_headers,
    )
}

/// 创建共享 KiroProvider 的路由，供主程序同时挂载 API 与 Admin 控制面。
#[allow(clippy::too_many_arguments)]
pub fn create_router_with_shared_provider(
    kiro_provider: Option<Arc<KiroProvider>>,
    extract_thinking: bool,
    tool_compatibility_mode: ToolCompatibilityMode,
    client_keys: Option<SharedClientKeyManager>,
    usage_recorder: Option<SharedRecorder>,
    usage_aggregator: Option<SharedAggregator>,
    cache_meter: Option<SharedCacheMeter>,
    trace_store: Option<SharedTraceStore>,
    custom_headers: Option<crate::model::custom_headers::CustomHeadersManager>,
) -> Router {
    let mut state = AppState::new(extract_thinking, tool_compatibility_mode);
    if let Some(provider) = kiro_provider {
        state = state.with_shared_kiro_provider(provider);
    }
    state = state.with_usage(client_keys, usage_recorder, usage_aggregator);
    state = state.with_cache_meter(cache_meter);
    state = state.with_trace_store(trace_store);
    state = state.with_custom_headers(custom_headers);

    // 需要认证的 /v1 路由
    let v1_routes = Router::new()
        .route("/models", get(get_models))
        .route("/messages", post(post_messages))
        .route("/messages/count_tokens", post(count_tokens))
        .route("/chat/completions", post(post_chat_completions))
        .route("/responses", post(post_responses))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // 需要认证的 /cc/v1 路由（Claude Code 兼容端点）
    // 与 /v1 的区别：流式内容即时发送，最终用量由 message_delta 更正
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
