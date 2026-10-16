//! Real HTTP fixtures for gateway regressions and repeatable latency measurements.
use super::{
    handlers::{post_messages, post_messages_cc},
    middleware::{AppState, KeyContext},
    openai::post_chat_completions,
    responses::post_responses,
};
use crate::{
    admin::trace_db::{TraceKeySource, TraceQuery, TraceStore},
    kiro::{
        endpoint::{KiroEndpoint, RequestContext},
        model::credentials::KiroCredentials,
        parser::crc::crc32,
        provider::KiroProvider,
        token_manager::MultiTokenManager,
    },
    model::config::{Config, ToolCompatibilityMode},
};
use axum::{
    Extension, Json, Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use bytes::Bytes;
use futures::{StreamExt, stream};
use parking_lot::Mutex;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
enum Mode {
    Normal(Duration),
    Interrupted,
    Overloaded,
    Large,
    Truncated,
    InvalidFrame,
    Empty,
    Tools,
    Stalled,
    SlowHeaders,
    SlowErrorBody,
    MetadataFirst,
    Active,
}

struct Fixture {
    state: AppState,
    calls: Arc<AtomicUsize>,
    connections: Arc<Mutex<HashSet<SocketAddr>>>,
    close_headers: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct LocalEndpoint(String);
impl KiroEndpoint for LocalEndpoint {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn api_url(&self, _: &RequestContext<'_>) -> String {
        self.0.clone()
    }
    fn mcp_url(&self, _: &RequestContext<'_>) -> String {
        self.0.clone()
    }
    fn decorate_api(
        &self,
        req: reqwest::RequestBuilder,
        _: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        req
    }
    fn decorate_mcp(
        &self,
        req: reqwest::RequestBuilder,
        _: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        req
    }
    fn transform_api_body(&self, body: &str, _: &RequestContext<'_>) -> String {
        body.to_owned()
    }
}

fn frame(event: &str, payload: serde_json::Value) -> Bytes {
    let mut headers = Vec::new();
    for (name, value) in [(":message-type", "event"), (":event-type", event)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = serde_json::to_vec(&payload).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32(&out).to_be_bytes());
    out.extend(headers);
    out.extend(payload);
    out.extend_from_slice(&crc32(&out).to_be_bytes());
    Bytes::from(out)
}

async fn fixture(mode: Mode) -> Fixture {
    fixture_with_config(mode, Config::default()).await
}

async fn fixture_with_config(mode: Mode, config: Config) -> Fixture {
    let calls = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(Mutex::new(HashSet::new()));
    let close_headers = Arc::new(AtomicUsize::new(0));
    let (seen_calls, seen_connections, seen_close) =
        (calls.clone(), connections.clone(), close_headers.clone());
    let app = Router::new().route("/", post(move |ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap| {
        let (calls, connections, close) = (seen_calls.clone(), seen_connections.clone(), seen_close.clone());
        async move {
            calls.fetch_add(1, Ordering::Relaxed);
            connections.lock().insert(peer);
            if headers.get("connection").is_some_and(|v| v == "close") { close.fetch_add(1, Ordering::Relaxed); }
            if matches!(mode, Mode::Overloaded) {
                return Response::builder().status(500).body(Body::from(r#"{"message":"high load","reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#)).unwrap();
            }
            if matches!(mode, Mode::SlowHeaders) { tokio::time::sleep(Duration::from_secs(4)).await; }
            if matches!(mode, Mode::SlowErrorBody) {
                return Response::builder().status(500).body(Body::from_stream(stream::pending::<Result<Bytes, Infallible>>())).unwrap();
            }
            if matches!(mode, Mode::Empty) { return Response::new(Body::empty()); }
            if matches!(mode, Mode::Large) {
                let frames = (0..18000).map(|_| frame("assistantResponseEvent", json!({"content": "x".repeat(1024)})));
                return Response::new(Body::from_stream(stream::iter(frames.map(Ok::<_, Infallible>))));
            }
            if matches!(mode, Mode::Truncated | Mode::InvalidFrame) {
                let mut bytes = frame("assistantResponseEvent", json!({"content": "damaged"})).to_vec();
                if matches!(mode, Mode::Truncated) { bytes.truncate(bytes.len() - 4); }
                else { let last = bytes.len() - 1; bytes[last] ^= 1; }
                return Response::new(Body::from(bytes));
            }
            if matches!(mode, Mode::Tools) {
                let mut bytes = Vec::new();
                for part in [
                    frame("toolUseEvent", json!({"name":"run", "toolUseId":"call_1", "input":"{\"command\":", "stop":false})),
                    frame("toolUseEvent", json!({"name":"run", "toolUseId":"call_1", "input":"\"hello\"}", "stop":true})),
                    frame("metadataEvent", json!({"tokenUsage":{"uncachedInputTokens":13,"outputTokens":17,"cacheReadInputTokens":3,"cacheWriteInputTokens":2}})),
                ] { bytes.extend_from_slice(&part); }
                return Response::new(Body::from(bytes));
            }
            let first = frame("assistantResponseEvent", json!({"content": "early content ".repeat(64)}));
            let metadata = frame("metadataEvent", json!({"tokenUsage": {"uncachedInputTokens": 13, "outputTokens": 17, "cacheReadInputTokens": 3, "cacheWriteInputTokens": 2}}));
            let chunks = stream::unfold(0, move |step| {
                let first = first.clone(); let metadata = metadata.clone();
                async move {
                    match step {
                        0 => { tokio::time::sleep(Duration::from_millis(20)).await; Some((Ok::<_, std::io::Error>(if matches!(mode, Mode::MetadataFirst) { metadata } else { first }), 1)) }
                        1 if matches!(mode, Mode::MetadataFirst) => { tokio::time::sleep(Duration::from_millis(100)).await; Some((Ok(first), 2)) }
                        1..=5 if matches!(mode, Mode::Active) => { tokio::time::sleep(Duration::from_millis(350)).await; Some((Ok(if step == 5 { metadata } else { first }), step + 1)) }
                        6 if matches!(mode, Mode::Active) => None,
                        1 if matches!(mode, Mode::Interrupted) => { tokio::time::sleep(Duration::from_millis(50)).await; Some((Err(std::io::Error::other("fixture disconnected")), 2)) }
                        1 if matches!(mode, Mode::Stalled) => futures::future::pending().await,
                        1 => { if let Mode::Normal(gap) = mode { tokio::time::sleep(gap).await; } Some((Ok(metadata), 2)) }
                        _ => None,
                    }
                }
            });
            Response::new(Body::from_stream(chunks))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let creds: KiroCredentials = serde_json::from_value(json!({"id": 1, "kiroApiKey": "fixture-only", "machineId": "0000000000000000000000000000000000000000000000000000000000000000"})).unwrap();
    let manager = Arc::new(MultiTokenManager::new(config, vec![creds], None, None, false).unwrap());
    let endpoints = HashMap::from([(
        "fixture".to_owned(),
        Arc::new(LocalEndpoint(url)) as Arc<dyn KiroEndpoint>,
    )]);
    let provider = KiroProvider::new(manager, endpoints, "fixture".to_owned());
    let state = AppState::new(false, ToolCompatibilityMode::Raw)
        .with_shared_kiro_provider(Arc::new(provider));
    Fixture {
        state,
        calls,
        connections,
        close_headers,
        task,
    }
}

fn key() -> KeyContext {
    KeyContext {
        key_id: 0,
        group: None,
        key_source: TraceKeySource::MasterApiKey,
        client_ip: None,
    }
}
async fn request(state: AppState, endpoint: &str, streaming: bool) -> Response {
    let req = json!({"model": "gpt-5.6-sol", "max_tokens": 64, "stream": streaming, "messages": [{"role": "user", "content": "hello"}], "input": [{"role":"user", "content":"hello"}]});
    if endpoint == "chat" {
        post_chat_completions(
            axum::extract::State(state),
            Extension(key()),
            HeaderMap::new(),
            Json(serde_json::from_value(req).unwrap()),
        )
        .await
    } else if endpoint == "responses" {
        post_responses(
            axum::extract::State(state),
            Extension(key()),
            HeaderMap::new(),
            Json(serde_json::from_value(req).unwrap()),
        )
        .await
    } else if endpoint == "cc" {
        post_messages_cc(
            axum::extract::State(state),
            Extension(key()),
            Json(serde_json::from_value(req).unwrap()),
        )
        .await
    } else {
        post_messages(
            axum::extract::State(state),
            Extension(key()),
            Json(serde_json::from_value(req).unwrap()),
        )
        .await
    }
}

async fn first_content(
    body: &mut (impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin),
) {
    while let Some(chunk) = body.next().await {
        if String::from_utf8_lossy(&chunk.unwrap()).contains("early content") {
            return;
        }
    }
    panic!("stream ended without content");
}

#[tokio::test]
async fn gateway_cc_emits_content_before_upstream_finishes() {
    let f = fixture(Mode::Normal(Duration::from_millis(400))).await;
    let mut body = request(f.state.clone(), "cc", true)
        .await
        .into_body()
        .into_data_stream();
    tokio::time::timeout(Duration::from_millis(250), first_content(&mut body))
        .await
        .expect("CC buffered content until EOF");
    let mut rest = String::new();
    while let Some(chunk) = body.next().await {
        rest.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    assert!(rest.contains("\"input_tokens\":13"));
    assert!(rest.contains("\"output_tokens\":17"));
}

#[tokio::test]
async fn gateway_chat_emits_content_before_upstream_finishes() {
    let f = fixture(Mode::Normal(Duration::from_millis(400))).await;
    let started = Instant::now();
    let response = request(f.state.clone(), "chat", true).await;
    let mut body = response.into_body().into_data_stream();
    first_content(&mut body).await;
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "Chat buffered content until EOF"
    );
}

#[tokio::test]
async fn gateway_cc_disconnect_is_an_error_not_message_stop() {
    let f = fixture(Mode::Interrupted).await;
    let response = request(f.state.clone(), "cc", true).await;
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("event: error"),
        "disconnect was hidden: {text}"
    );
    assert!(!text.contains("event: message_stop"));
}

#[tokio::test]
async fn gateway_reuses_upstream_connection() {
    let f = fixture(Mode::Normal(Duration::ZERO)).await;
    for _ in 0..3 {
        let response = request(f.state.clone(), "messages", false).await;
        assert_eq!(response.status(), StatusCode::OK);
        to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    }
    assert_eq!(f.close_headers.load(Ordering::Relaxed), 0);
    assert_eq!(f.connections.lock().len(), 1);
}

#[tokio::test]
async fn gateway_model_overload_does_not_retry_three_times() {
    let f = fixture(Mode::Overloaded).await;
    let response = request(f.state.clone(), "cc", true).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.headers().contains_key("retry-after"));
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("overloaded_error"));
    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
    let second = request(f.state.clone(), "cc", true).await;
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        f.calls.load(Ordering::Relaxed),
        1,
        "cooldown still hits overloaded upstream"
    );
}

#[tokio::test]
async fn gateway_non_stream_decodes_responses_larger_than_decoder_buffer() {
    let f = fixture(Mode::Large).await;
    let response = request(f.state.clone(), "messages", false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap();
    let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        response["content"][0]["text"].as_str().unwrap().len(),
        18000 * 1024
    );
}

#[tokio::test]
async fn gateway_damaged_frames_are_errors_not_success() {
    for mode in [Mode::Truncated, Mode::InvalidFrame, Mode::Empty] {
        let f = fixture(mode).await;
        for endpoint in ["cc", "messages"] {
            let response = request(f.state.clone(), endpoint, true).await;
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                text.contains("event: error"),
                "corrupt stream was accepted: {text}"
            );
            assert!(!text.contains("event: message_stop"));
        }
        let response = request(f.state.clone(), "messages", false).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }
}

#[tokio::test]
async fn gateway_chat_disconnect_has_no_success_finish_or_done() {
    let f = fixture(Mode::Interrupted).await;
    let response = request(f.state.clone(), "chat", true).await;
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\"error\""));
    assert!(!text.contains("[DONE]"));
    assert!(!text.contains("\"finish_reason\":\"stop\""));
}

#[tokio::test]
async fn gateway_chat_preserves_overload_retry_after() {
    let f = fixture(Mode::Overloaded).await;
    let response = request(f.state.clone(), "chat", true).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get("retry-after").unwrap(), "5");
}

#[tokio::test]
async fn gateway_responses_preserves_overload_retry_after() {
    let f = fixture(Mode::Overloaded).await;
    let response = request(f.state.clone(), "responses", true).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get("retry-after").unwrap(), "5");
}

#[tokio::test]
async fn gateway_chat_tool_arguments_and_final_usage_are_complete() {
    let f = fixture(Mode::Tools).await;
    let response = request(f.state.clone(), "chat", true).await;
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    let chunks: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut arguments = String::new();
    for chunk in &chunks {
        if let Some(args) = chunk
            .pointer("/choices/0/delta/tool_calls/0/function/arguments")
            .and_then(|v| v.as_str())
        {
            arguments.push_str(args);
        }
    }
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&arguments).unwrap(),
        json!({"command":"hello"})
    );
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk.pointer("/choices/0/finish_reason") == Some(&json!("tool_calls")))
    );
    let usage = &chunks.last().unwrap()["usage"];
    assert_eq!(usage["prompt_tokens"], 18);
    assert_eq!(usage["completion_tokens"], 17);
    assert!(text.ends_with("data: [DONE]\n\n"));
}

#[tokio::test]
async fn gateway_stalled_body_times_out_without_normal_completion() {
    let config: Config = serde_json::from_value(json!({"upstreamReadTimeoutSecs":1})).unwrap();
    let f = fixture_with_config(Mode::Stalled, config).await;
    let response = request(f.state.clone(), "cc", true).await;
    let bytes = tokio::time::timeout(
        Duration::from_secs(2),
        to_bytes(response.into_body(), 1024 * 1024),
    )
    .await
    .expect("idle stream did not time out")
    .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("event: error"));
    assert!(!text.contains("event: message_stop"));
}

#[tokio::test]
async fn gateway_header_timeout_does_not_repeat_long_waits() {
    let config: Config = serde_json::from_value(json!({"upstreamReadTimeoutSecs":1})).unwrap();
    let f = fixture_with_config(Mode::SlowHeaders, config).await;
    let response =
        tokio::time::timeout(Duration::from_secs(2), request(f.state.clone(), "cc", true))
            .await
            .expect("header deadline was ignored or retried");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn gateway_error_body_timeout_does_not_repeat_long_waits() {
    let config: Config = serde_json::from_value(json!({"upstreamReadTimeoutSecs":1})).unwrap();
    let f = fixture_with_config(Mode::SlowErrorBody, config).await;
    let response =
        tokio::time::timeout(Duration::from_secs(2), request(f.state.clone(), "cc", true))
            .await
            .expect("error response body deadline was ignored or retried");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(f.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn gateway_first_token_trace_ignores_metadata() {
    let mut f = fixture(Mode::MetadataFirst).await;
    let store = Arc::new(TraceStore::open_in_memory().unwrap());
    f.state = f.state.clone().with_trace_store(Some(store.clone()));
    let response = request(f.state.clone(), "cc", true).await;
    to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let (records, count) = store.query_paged(&TraceQuery {
        limit: 10,
        ..Default::default()
    });
    assert_eq!(count, 1);
    assert!(
        records[0].first_token_ms.unwrap() >= 90,
        "metadata was counted as the first token: {:?}",
        records[0].first_token_ms
    );
}

#[tokio::test]
async fn gateway_active_stream_can_exceed_idle_deadline() {
    let config: Config = serde_json::from_value(json!({"upstreamReadTimeoutSecs":1})).unwrap();
    let f = fixture_with_config(Mode::Active, config).await;
    let started = Instant::now();
    let response = request(f.state.clone(), "cc", true).await;
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(started.elapsed() > Duration::from_secs(1));
    assert!(text.contains("event: message_stop"));
    assert!(!text.contains("event: error"));
}

#[tokio::test]
#[ignore = "controlled latency measurement; run with --ignored --nocapture"]
async fn gateway_latency_measurement() {
    for endpoint in ["cc", "chat", "messages"] {
        let f = fixture(Mode::Normal(Duration::from_millis(200))).await;
        let mut first = Vec::new();
        let mut total = Vec::new();
        for _ in 0..10 {
            let start = Instant::now();
            let mut body = request(f.state.clone(), endpoint, true)
                .await
                .into_body()
                .into_data_stream();
            first_content(&mut body).await;
            first.push(start.elapsed().as_secs_f64() * 1000.0);
            while let Some(chunk) = body.next().await {
                chunk.unwrap();
            }
            total.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        first.sort_by(f64::total_cmp);
        total.sort_by(f64::total_cmp);
        println!(
            "LATENCY endpoint={endpoint} first_p50_ms={:.2} first_p95_ms={:.2} total_p50_ms={:.2} connections={} requests={}",
            first[5],
            first[9],
            total[5],
            f.connections.lock().len(),
            f.calls.load(Ordering::Relaxed)
        );
    }
}
