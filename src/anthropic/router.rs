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
    cache_metering::SharedMeterGovernance,
    handlers::{count_tokens, get_models, post_messages, post_messages_cc},
    middleware::{AppState, auth_middleware, cors_layer},
    response_cache::SharedResponseCache,
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
        true,
        None,
        None,
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
    meter_governance: Option<SharedMeterGovernance>,
    trace_store: Option<SharedTraceStore>,
    usage_gated_streaming: bool,
    response_cache: Option<SharedResponseCache>,
    model_mappings: Option<crate::openai::model_mapping::SharedModelMappings>,
) -> Router {
    let mut state = AppState::new(extract_thinking);
    if let Some(provider) = kiro_provider {
        state = state.with_kiro_provider(provider);
    }
    state = state.with_usage(client_keys, usage_recorder, usage_aggregator);
    state = state.with_meter_governance(meter_governance);
    state = state.with_response_cache(response_cache);
    state = state.with_trace_store(trace_store);
    state = state.with_usage_gated_streaming(usage_gated_streaming);
    state = state.with_model_mappings(model_mappings);

    // 需要认证的 /v1 路由
    let v1_routes = Router::new()
        .route("/models", get(get_models))
        .route("/messages", post(post_messages))
        .route("/messages/count_tokens", post(count_tokens))
        // OpenAI 兼容端点：与 /v1/messages 共用同一 AppState 与 auth_middleware。
        // 入站归一化成 Anthropic MessagesRequest 后复用全部既有管道（见 crate::openai）。
        .route(
            "/chat/completions",
            post(crate::openai::post_chat_completions),
        )
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
