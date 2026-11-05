use super::*;
use std::{collections::HashMap, convert::Infallible, time::Duration};

fn frame(event: &str, data: Value) -> Vec<u8> {
    format!("event: {event}\r\ndata: {data}\r\n\r\n").into_bytes()
}

fn start() -> Vec<u8> {
    frame(
        "message_start",
        json!({"message":{"usage":{"input_tokens":999,"output_tokens":1}}}),
    )
}

fn finish(usage: Value, reason: &str) -> Vec<u8> {
    [
        frame(
            "message_delta",
            json!({"delta":{"stop_reason":reason}, "usage":usage}),
        ),
        frame("message_stop", json!({})),
    ]
    .concat()
}

fn chunks(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn output(body: Body) -> String {
    let response = chat_streaming_response(body, "gpt-5.6-luna".into());
    let bytes = tokio::time::timeout(
        Duration::from_secs(2),
        to_bytes(response.into_body(), 1024 * 1024),
    )
    .await
    .expect("Chat adapter waited for transport EOF after message_stop")
    .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn issue98_conversion_preserves_streaming_preference() {
    for streaming in [false, true] {
        let request = serde_json::from_value(json!({
            "model":"gpt-5.6-luna","stream":streaming,
            "messages":[{"role":"user","content":"请生成一段足够长的文本"}]
        }))
        .unwrap();
        assert_eq!(
            openai_to_anthropic(request, None).unwrap().stream,
            streaming
        );
    }
}

#[tokio::test]
async fn issue98_live_adapter_preserves_final_usage_and_optional_credits() {
    for with_credits in [false, true] {
        let mut usage = json!({"input_tokens":3,"output_tokens":11,"cache_creation_input_tokens":4,"cache_read_input_tokens":7});
        if with_credits {
            usage["credit_usage"] = json!(0.33);
            usage["credit_unit"] = json!("credit");
            usage["credit_unit_plural"] = json!("credits");
        }
        let text = output(Body::from([start(), finish(usage, "end_turn")].concat())).await;
        let chunks = chunks(&text);
        let final_chunk = chunks.last().unwrap();
        assert_eq!(final_chunk["usage"]["prompt_tokens"], 14);
        assert_eq!(final_chunk["usage"]["completion_tokens"], 11);
        assert_eq!(final_chunk["usage"]["total_tokens"], 25);
        assert!(
            chunks[..chunks.len() - 1]
                .iter()
                .all(|chunk| chunk.get("usage").is_none())
        );
        if with_credits {
            assert_eq!(final_chunk["usage"]["credit_usage"], 0.33);
            assert_eq!(final_chunk["usage"]["credit_unit"], "credit");
            assert_eq!(final_chunk["usage"]["credit_unit_plural"], "credits");
        } else {
            for field in ["credit_usage", "credit_unit", "credit_unit_plural"] {
                assert!(final_chunk["usage"].get(field).is_none());
            }
        }
        assert_eq!(text.matches("data: [DONE]").count(), 1);
    }
}

#[tokio::test]
async fn issue98_fragmented_utf8_and_crlf_preserve_each_text_delta() {
    let wire = [
        start(),
        b": heartbeat\r\n\r\n".to_vec(),
        frame(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"text_delta","text":"第一段🙂"}}),
        ),
        frame(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"text_delta","text":"第二段\n"}}),
        ),
        finish(json!({"input_tokens":0,"output_tokens":5}), "end_turn"),
    ]
    .concat();
    let body = Body::from_stream(stream::iter(
        wire.into_iter()
            .map(|byte| Ok::<_, Infallible>(Bytes::from(vec![byte]))),
    ));
    let text = output(body).await;
    let chunks = chunks(&text);
    let deltas: Vec<_> = chunks
        .iter()
        .filter_map(|chunk| {
            chunk
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
        })
        .collect();
    assert_eq!(deltas, ["第一段🙂", "第二段\n"]);
    assert_eq!(chunks.last().unwrap()["usage"]["prompt_tokens"], 0);
}

#[tokio::test]
async fn issue98_parallel_tools_keep_indices_and_incremental_arguments() {
    let wire = [
        start(),
        frame("content_block_start", json!({"index":8,"content_block":{"type":"tool_use","id":"call_a","name":"first","input":{}}})),
        frame("content_block_start", json!({"index":2,"content_block":{"type":"tool_use","id":"call_b","name":"second","input":{}}})),
        frame("content_block_delta", json!({"index":8,"delta":{"type":"input_json_delta","partial_json":"{\"text\":\"中"}})),
        frame("content_block_delta", json!({"index":2,"delta":{"type":"input_json_delta","partial_json":"{\"ok\":"}})),
        frame("content_block_delta", json!({"index":8,"delta":{"type":"input_json_delta","partial_json":"文\"}"}})),
        frame("content_block_delta", json!({"index":2,"delta":{"type":"input_json_delta","partial_json":"true}"}})),
        finish(json!({"input_tokens":3,"output_tokens":10}), "tool_use"),
    ].concat();
    let text = output(Body::from(wire)).await;
    let chunks = chunks(&text);
    let mut arguments: HashMap<u64, String> = HashMap::new();
    let mut ids = HashMap::new();
    for chunk in &chunks {
        if let Some(calls) = chunk
            .pointer("/choices/0/delta/tool_calls")
            .and_then(Value::as_array)
        {
            for call in calls {
                let index = call["index"].as_u64().unwrap();
                if let Some(id) = call["id"].as_str() {
                    assert!(ids.insert(index, id).is_none());
                }
                arguments
                    .entry(index)
                    .or_default()
                    .push_str(call["function"]["arguments"].as_str().unwrap());
            }
        }
    }
    assert_eq!(ids[&0], "call_a");
    assert_eq!(ids[&1], "call_b");
    assert_eq!(
        serde_json::from_str::<Value>(&arguments[&0]).unwrap(),
        json!({"text":"中文"})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&arguments[&1]).unwrap(),
        json!({"ok":true})
    );
    assert_eq!(
        chunks.last().unwrap()["choices"][0]["finish_reason"],
        "tool_calls"
    );
}

#[tokio::test]
async fn issue98_message_stop_completes_without_waiting_for_body_eof() {
    let wire = [
        start(),
        finish(json!({}), "end_turn"),
        frame("message_stop", json!({})),
    ]
    .concat();
    let body = Body::from_stream(
        stream::once(async move { Ok::<_, Infallible>(Bytes::from(wire)) })
            .chain(stream::pending()),
    );
    let text = output(body).await;
    assert_eq!(text.matches("data: [DONE]").count(), 1);
}

#[tokio::test]
async fn issue98_invalid_frames_and_truncated_streams_never_emit_done() {
    for wire in [
        [
            start(),
            b"event: content_block_delta\r\ndata: not-json\r\n\r\n".to_vec(),
        ]
        .concat(),
        [start(), b"event: content_block_delta\r\ndata: {".to_vec()].concat(),
    ] {
        let text = output(Body::from(wire)).await;
        assert!(chunks(&text).last().unwrap().get("error").is_some());
        assert!(!text.contains("[DONE]"));
    }
}
