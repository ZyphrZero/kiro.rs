//! OpenAI `/v1/chat/completions` handler
//!
//! 归一化 → 共享 `prepare_kiro_request` → provider 分发 → 出站 OpenAI 翻译。
//! 治理（auth / 用量 / trace / cache 计量）全部沿用 Anthropic 路径的同名组件。

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::{Extension, State},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use tokio::time::interval;

use crate::admin::trace_db::outcome;
use crate::anthropic::handlers::{
    PreparedKiroRequest, RequestTracer, RequestTraceOptions, TraceUsage, UsageRecordHook,
    last_attempt_outcome, map_provider_error, prepare_kiro_request,
};
use crate::anthropic::middleware::{AppState, KeyContext};
use crate::anthropic::stream::StreamContext;
use crate::kiro::model::events::Event;
use crate::kiro::parser::decoder::EventStreamDecoder;

use super::convert::to_messages_request;
use super::response::{OpenAiResponseBuilder, OpenAiUsage};
use super::types::ChatCompletionRequest;

/// Ping 间隔（与 Anthropic 路径一致，25s）
const PING_INTERVAL_SECS: u64 = 25;

/// 构造 OpenAI 风格错误响应：`{"error":{"type","message"}}`
fn openai_error(status: StatusCode, err_type: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": { "type": err_type, "message": message }
        })),
    )
        .into_response()
}

/// 从 StreamContext 解析最终 OpenAI usage（prompt = input+cache_creation+cache_read）。
fn usage_from_ctx(ctx: &StreamContext) -> OpenAiUsage {
    let (input, cache_creation, cache_read) = ctx.resolved_usage();
    OpenAiUsage {
        prompt_tokens: input + cache_creation + cache_read,
        completion_tokens: ctx.output_tokens,
    }
}

/// 从 StreamContext 解析 trace 用量（与 Anthropic 路径同源）。
fn trace_usage_from_ctx(ctx: &StreamContext) -> TraceUsage {
    let (input, cache_creation, cache_read) = ctx.resolved_usage();
    TraceUsage {
        input_tokens: input.max(0) as u64,
        output_tokens: ctx.output_tokens.max(0) as u64,
        cache_creation_tokens: cache_creation.max(0) as u64,
        cache_read_tokens: cache_read.max(0) as u64,
        credits: if ctx.credits.is_finite() && ctx.credits > 0.0 {
            ctx.credits
        } else {
            0.0
        },
        context_input_tokens: ctx.context_input_tokens.map(|v| v.max(0) as u64),
    }
}

/// 记录用量到 hook（互斥分摊后的口径，与 trace 一致）。
fn record_usage(hook: &UsageRecordHook, ctx: &StreamContext, credential_id: u64, status: &str) {
    let (input, cache_creation, cache_read) = ctx.resolved_usage();
    hook.record(
        credential_id,
        input,
        ctx.output_tokens,
        cache_creation,
        cache_read,
        ctx.credits,
        status,
    );
}

/// `POST /v1/chat/completions`
pub async fn post_chat_completions(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    JsonExtractor(req): JsonExtractor<ChatCompletionRequest>,
) -> Response {
    tracing::info!(
        model = %req.model,
        stream = %req.stream,
        message_count = %req.messages.len(),
        "Received POST /v1/chat/completions request"
    );

    // 可配置模型映射（全局、运行时热编辑）：客户端模型名 → 目标 Claude 模型名。
    // 命中规则即替换；未命中保持原名。在归一化前执行，目标 Claude 名随后由
    // normalize_model / map_model 正常解析。
    let mut req = req;
    if let Some(mappings) = &state.model_mappings {
        if let Some(target) = mappings.resolve(&req.model) {
            tracing::debug!(from = %req.model, to = %target, "模型映射命中");
            req.model = target;
        }
    }

    // 归一化成 Anthropic MessagesRequest（唯一新增转换层）
    let mut payload = to_messages_request(&req);
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());

    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "Kiro API provider not configured",
            );
        }
    };

    if payload.messages.is_empty() {
        hook.record(0, 0, 0, 0, 0, 0.0, "error");
        return openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages 不能为空",
        );
    }

    // 共享请求侧核心：提示词过滤 + 裁剪 + 转换 + 序列化 + token 估算 + cache 计量
    let prepared: PreparedKiroRequest = match prepare_kiro_request(&state, &mut payload, &key_ctx) {
        Ok(p) => p,
        Err(resp) => {
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            // prepare 已返回含 error.message 的错误体，OpenAI 客户端语义一致
            return resp;
        }
    };

    let is_stream = req.stream;
    let tracer = std::sync::Arc::new(RequestTracer::new(
        &state,
        RequestTraceOptions {
            key_ctx: key_ctx.clone(),
            model: payload.model.clone(),
            is_stream,
            request_bytes: prepared.request_body.len() as u64,
            local_input_tokens: prepared.total_input_tokens.max(0) as u64,
            conversion_ms: prepared.conversion_ms,
            token_count_ms: prepared.token_count_ms,
        },
    ));

    if is_stream {
        handle_stream(provider, payload.model, prepared, hook, tracer, key_ctx.group).await
    } else {
        handle_non_stream(provider, payload.model, prepared, hook, tracer, key_ctx.group).await
    }
}

/// 非流式：缓冲整段上游响应 → 喂给 StreamContext 收集 Anthropic SseEvent → 聚合成
/// 一个 `chat.completion`。复用 StreamContext 而非另写解码，确保 `<invoke>` 文本恢复 /
/// thinking 检测与流式完全一致。
async fn handle_non_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    model: String,
    prepared: PreparedKiroRequest,
    hook: UsageRecordHook,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
) -> Response {
    let call_result = match provider
        .call_api(&prepared.request_body, Some(tracer.as_ref()), group.as_deref())
        .await
    {
        Ok(r) => r,
        Err(e) => {
            hook.record(0, prepared.total_input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    let credential_id = call_result.credential_id;

    let body_bytes = match call_result.response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            hook.record(credential_id, prepared.total_input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "interrupted",
                Some(outcome::STREAM_INTERRUPTED),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return openai_error(StatusCode::BAD_GATEWAY, "api_error", "读取上游响应失败");
        }
    };

    // 用 StreamContext 统一解析，收集全部 SseEvent
    let mut ctx = StreamContext::new_with_thinking(
        model.clone(),
        prepared.total_input_tokens,
        prepared.thinking_enabled,
        prepared.tool_name_map,
        prepared.known_tool_names,
    );
    ctx.cache_usage = prepared.cache_usage;

    let mut builder = OpenAiResponseBuilder::new(model);
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }
    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    for ev in ctx.process_kiro_event(&event) {
                        builder.push_event(&ev);
                    }
                }
            }
            Err(e) => tracing::warn!("解码事件失败: {}", e),
        }
    }
    // flush 末尾事件（关闭 thinking/文本块、message_delta 等），同样喂给 builder 以捕获 stop_reason
    for ev in ctx.generate_final_events() {
        builder.push_event(&ev);
    }

    let usage = usage_from_ctx(&ctx);
    record_usage(&hook, &ctx, credential_id, "success");
    tracer.finalize("success", None, None, None, trace_usage_from_ctx(&ctx));

    let body = builder.build_completion(usage);
    (StatusCode::OK, Json(body)).into_response()
}

/// 单个 OpenAI chunk 序列化为 SSE 行
fn chunk_to_sse(value: &serde_json::Value) -> Bytes {
    Bytes::from(format!("data: {}\n\n", value))
}

/// 流末尾收尾：flush StreamContext 残余事件、发 finish chunk + `[DONE]`，并记录用量 / trace。
///
/// `status` 为 `"success"`（正常结束）或 `"error"`（上游断流）。返回应当写出的 SSE 帧序列。
#[allow(clippy::too_many_arguments)]
fn finalize_stream(
    ctx: &mut StreamContext,
    builder: &mut OpenAiResponseBuilder,
    hook: &UsageRecordHook,
    tracer: &RequestTracer,
    credential_id: u64,
    sent_bytes: u64,
    status: &str,
    error_message: Option<&str>,
) -> Vec<Result<Bytes, Infallible>> {
    let mut out: Vec<Result<Bytes, Infallible>> = Vec::new();
    // flush 残余事件（关闭未闭合块、message_delta 捕获 stop_reason、可能的尾部 thinking_delta）
    for ev in ctx.generate_final_events() {
        for chunk_json in builder.push_event(&ev) {
            out.push(Ok(chunk_to_sse(&chunk_json)));
        }
    }
    let usage = usage_from_ctx(ctx);
    out.push(Ok(chunk_to_sse(&builder.finish_chunk(usage))));
    out.push(Ok(done_frame()));

    record_usage(hook, ctx, credential_id, status);
    if status == "success" {
        tracer.finalize("success", None, None, None, trace_usage_from_ctx(ctx));
    } else {
        tracer.finalize(
            "interrupted",
            Some(outcome::STREAM_INTERRUPTED),
            error_message,
            Some(sent_bytes),
            trace_usage_from_ctx(ctx),
        );
    }
    out
}


/// `data: [DONE]` 终止帧
fn done_frame() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

/// ping 保活帧（OpenAI 客户端忽略未知 data，但保持连接活跃）
fn ping_frame() -> Bytes {
    Bytes::from_static(b": ping\n\n")
}

/// 构造 OpenAI 流式 SSE。复用 Anthropic 路径的 unfold + ping 骨架，逐事件翻译成 OpenAI chunk。
fn build_openai_sse_stream(
    call_result: crate::kiro::provider::KiroCallResult,
    ctx: StreamContext,
    builder: OpenAiResponseBuilder,
    hook: UsageRecordHook,
    tracer: std::sync::Arc<RequestTracer>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let credential_id = call_result.credential_id;
    let crate::kiro::provider::KiroCallResult {
        response,
        account_guard,
        ..
    } = call_result;
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            builder,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            hook,
            credential_id,
            tracer,
            0u64,
            account_guard,
        ),
        |(
            mut body_stream,
            mut ctx,
            mut builder,
            mut decoder,
            finished,
            mut ping_interval,
            hook,
            credential_id,
            tracer,
            mut sent_bytes,
            account_guard,
        )| async move {
            if finished {
                return None;
            }
            tokio::select! {
                chunk_result = body_stream.next() => match chunk_result {
                    Some(Ok(chunk)) => {
                        tracer.mark_first_token();
                        sent_bytes += chunk.len() as u64;
                        if let Err(e) = decoder.feed(&chunk) {
                            tracing::warn!("缓冲区溢出: {}", e);
                        }
                        let mut out: Vec<Result<Bytes, Infallible>> = Vec::new();
                        for result in decoder.decode_iter() {
                            match result {
                                Ok(frame) => {
                                    if let Ok(event) = Event::from_frame(frame) {
                                        for ev in ctx.process_kiro_event(&event) {
                                            for chunk_json in builder.push_event(&ev) {
                                                out.push(Ok(chunk_to_sse(&chunk_json)));
                                            }
                                        }
                                    }
                                }
                                Err(e) => tracing::warn!("解码事件失败: {}", e),
                            }
                        }
                        if !out.is_empty() {
                            tracer.mark_downstream_first_event();
                        }
                        Some((stream::iter(out), (body_stream, ctx, builder, decoder, false, ping_interval, hook, credential_id, tracer, sent_bytes, account_guard)))
                    }
                    Some(Err(e)) => {
                        tracing::error!("读取上游流失败: {}", e);
                        let out = finalize_stream(&mut ctx, &mut builder, &hook, &tracer, credential_id, sent_bytes, "error", Some(&e.to_string()));
                        Some((stream::iter(out), (body_stream, ctx, builder, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes, account_guard)))
                    }
                    None => {
                        let out = finalize_stream(&mut ctx, &mut builder, &hook, &tracer, credential_id, sent_bytes, "success", None);
                        Some((stream::iter(out), (body_stream, ctx, builder, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes, account_guard)))
                    }
                },
                _ = ping_interval.tick() => {
                    let out: Vec<Result<Bytes, Infallible>> = vec![Ok(ping_frame())];
                    Some((stream::iter(out), (body_stream, ctx, builder, decoder, false, ping_interval, hook, credential_id, tracer, sent_bytes, account_guard)))
                }
            }
        },
    )
    .flatten()
}


/// 流式：上游字节 → StreamContext → Anthropic SseEvent → OpenAI chunk SSE。
async fn handle_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    model: String,
    prepared: PreparedKiroRequest,
    hook: UsageRecordHook,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
) -> Response {
    let call_result = match provider
        .call_api_stream(&prepared.request_body, Some(tracer.as_ref()), group.as_deref())
        .await
    {
        Ok(r) => r,
        Err(e) => {
            hook.record(0, prepared.total_input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };

    let mut ctx = StreamContext::new_with_thinking(
        model.clone(),
        prepared.total_input_tokens,
        prepared.thinking_enabled,
        prepared.tool_name_map,
        prepared.known_tool_names,
    );
    ctx.cache_usage = prepared.cache_usage;
    let builder = OpenAiResponseBuilder::new(model);

    let stream = build_openai_sse_stream(call_result, ctx, builder, hook, tracer);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}


