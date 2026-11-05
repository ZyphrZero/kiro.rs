use super::*;

pub(super) fn first_text() -> String {
    "first streamed paragraph 中文。".repeat(128)
}

pub(super) fn second_text() -> String {
    "second streamed paragraph 中文。".repeat(128)
}

struct Gateway {
    url: String,
    client: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
}

impl Gateway {
    async fn start(fixture: &Fixture) -> Self {
        let client_keys = Arc::new(crate::admin::ClientKeyManager::new());
        client_keys.create_with_key("fixture".into(), None, None, "fixture-chat-key".into());
        let app = crate::anthropic::router::create_router_with_shared_provider(
            fixture.state.kiro_provider.clone(),
            false,
            ToolCompatibilityMode::Raw,
            Some(client_keys),
            None,
            None,
            None,
            None,
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        Self { url, client, task }
    }

    async fn request(&self, mut payload: serde_json::Value) -> reqwest::Response {
        payload["model"] = json!("gpt-5.6-luna");
        payload["stream"] = json!(true);
        payload["messages"] = json!([{"role":"user","content":"请生成一段足够长的文本"}]);
        tokio::time::timeout(
            Duration::from_secs(5),
            self.client
                .post(&self.url)
                .header("x-api-key", "fixture-chat-key")
                .json(&payload)
                .send(),
        )
        .await
        .expect("Chat handler buffered upstream completion before response headers")
        .unwrap()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn chunks(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn receive_until(
    body: &mut (impl futures::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin),
    received: &mut Vec<u8>,
    predicate: impl Fn(&str) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(chunk) = body.next().await {
            received.extend_from_slice(&chunk.unwrap());
            if let Ok(text) = std::str::from_utf8(received) {
                if text.ends_with("\n\n") && predicate(text) {
                    return;
                }
            }
        }
        panic!("HTTP stream ended before the expected event");
    })
    .await
    .expect("Chat SSE did not arrive while upstream completion was gated");
}

#[tokio::test]
async fn issue98_http_text_deltas_arrive_before_upstream_is_allowed_to_finish() {
    let fixture = fixture(Mode::GatedChatText).await;
    let gateway = Gateway::start(&fixture).await;
    let response = gateway.request(json!({})).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    assert!(!response.headers().contains_key("content-length"));
    let mut body = response.bytes_stream();
    let mut received = Vec::new();
    receive_until(&mut body, &mut received, |text| {
        chunks(text).iter().any(|chunk| {
            chunk
                .pointer("/choices/0/delta/content")
                .and_then(serde_json::Value::as_str)
                == Some(first_text().as_str())
        })
    })
    .await;
    let early = std::str::from_utf8(&received).unwrap();
    assert!(!early.contains("[DONE]"));
    assert!(!early.contains("second streamed paragraph"));
    fixture.release_tool_arguments.notify_one();
    receive_until(&mut body, &mut received, |text| {
        chunks(text).iter().any(|chunk| {
            chunk
                .pointer("/choices/0/delta/content")
                .and_then(serde_json::Value::as_str)
                == Some(second_text().as_str())
        })
    })
    .await;
    assert!(!std::str::from_utf8(&received).unwrap().contains("[DONE]"));
    fixture.release_tool_arguments.notify_one();
    while let Some(chunk) = body.next().await {
        received.extend_from_slice(&chunk.unwrap());
    }
    let text = std::str::from_utf8(&received).unwrap();
    let chunks = chunks(text);
    let content: Vec<_> = chunks
        .iter()
        .filter_map(|chunk| {
            chunk
                .pointer("/choices/0/delta/content")
                .and_then(serde_json::Value::as_str)
        })
        .collect();
    assert_eq!(content, [first_text(), second_text()]);
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk["id"] == chunks[0]["id"] && chunk["model"] == "gpt-5.6-luna")
    );
    let final_chunk = chunks.last().unwrap();
    assert_eq!(final_chunk["choices"][0]["finish_reason"], "stop");
    assert_eq!(final_chunk["usage"]["prompt_tokens"], 18);
    assert_eq!(final_chunk["usage"]["completion_tokens"], 17);
    assert_eq!(text.matches("data: [DONE]").count(), 1);
}

#[tokio::test]
async fn issue98_http_tool_arguments_arrive_incrementally_before_completion() {
    let fixture = fixture(Mode::GatedToolArguments).await;
    let gateway = Gateway::start(&fixture).await;
    let response = gateway.request(json!({"tools":[{"type":"function","function":{
        "name":"Write","description":"Return the supplied arguments", "parameters":{
            "type":"object","properties":{"file_path":{"type":"string"},"content":{"type":"string"}}
        }
    }}]})).await;
    let mut body = response.bytes_stream();
    let mut received = Vec::new();
    receive_until(&mut body, &mut received, |text| {
        chunks(text).iter().any(|chunk| {
            chunk
                .pointer("/choices/0/delta/tool_calls/0/function/arguments")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|input| input.contains("first"))
        })
    })
    .await;
    assert!(!std::str::from_utf8(&received).unwrap().contains("[DONE]"));
    fixture.release_tool_arguments.notify_one();
    while let Some(chunk) = body.next().await {
        received.extend_from_slice(&chunk.unwrap());
    }
    let text = std::str::from_utf8(&received).unwrap();
    let chunks = chunks(text);
    let calls: Vec<_> = chunks
        .iter()
        .filter_map(|chunk| chunk.pointer("/choices/0/delta/tool_calls/0"))
        .collect();
    assert_eq!(calls[0]["id"], "call_1");
    assert_eq!(calls[0]["function"]["name"], "Write");
    assert!(calls.iter().all(|call| call["index"] == 0));
    let arguments: Vec<_> = calls
        .iter()
        .filter_map(|call| call["function"]["arguments"].as_str())
        .filter(|part| !part.is_empty())
        .collect();
    assert_eq!(arguments.len(), 2);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&arguments.concat()).unwrap(),
        json!({"file_path":"/not-created","content":"first second"})
    );
    assert_eq!(
        chunks.last().unwrap()["choices"][0]["finish_reason"],
        "tool_calls"
    );
    assert_eq!(text.matches("data: [DONE]").count(), 1);
}

#[tokio::test]
async fn issue98_http_interruption_never_reports_successful_completion() {
    let fixture = fixture(Mode::Interrupted).await;
    let gateway = Gateway::start(&fixture).await;
    let text = gateway.request(json!({})).await.text().await.unwrap();
    assert!(text.contains("early content"));
    assert!(
        chunks(&text)
            .iter()
            .any(|chunk| chunk.get("error").is_some())
    );
    assert!(!text.contains("[DONE]"));
    assert!(!chunks(&text).iter().any(|chunk| {
        chunk
            .pointer("/choices/0/finish_reason")
            .is_some_and(|reason| !reason.is_null())
    }));
}
