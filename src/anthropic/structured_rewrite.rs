//! `output_config.format = json_schema` 的请求注入与响应改写。
//!
//! Kiro 上游没有原生 structured output 字段，因此：请求侧把 schema 以系统提示形式注入，
//! 响应侧从模型回复里提取第一个符合 schema 的 JSON，并用它替换 text 内容。
//! 提取不到合规 JSON 时原样返回，只打一行不含内容的 warn。

use std::collections::HashMap;

use serde_json::{Value, json};

use super::stream::SseEvent;
use super::structured_output::{extract_conforming_json, system_instruction};
use super::types::{MessagesRequest, SystemMessage};

/// 请求带有 `output_config.format = json_schema` 时，把 schema 指令追加到 system 末尾
/// （不动已有块，保持其上的 cache 断点有效），并返回 schema 供响应改写使用。
pub(super) fn inject_system_instruction(payload: &mut MessagesRequest) -> Option<Value> {
    let schema = payload
        .output_config
        .as_ref()
        .and_then(|config| config.json_schema())
        .cloned()?;
    payload
        .system
        .get_or_insert_with(Vec::new)
        .push(SystemMessage {
            text: system_instruction(&schema),
            cache_control: None,
        });
    Some(schema)
}

fn warn_no_conforming_json(model: &str, text_len: usize) {
    tracing::warn!(
        model = %model,
        text_len,
        "structured output: no schema-conforming JSON found in reply, returning it unchanged"
    );
}

/// 非流式：把 `content` 里的 text 块替换为单个只含 JSON 的 text 块
/// （位置取第一个 text 块处；thinking / tool_use 等其它块原样保留）。
pub(super) fn rewrite_message_json(body: &mut Value, schema: &Value, model: &str) -> bool {
    let Some(content) = body.get_mut("content").and_then(Value::as_array_mut) else {
        return false;
    };
    let is_text = |block: &Value| block.get("type").and_then(Value::as_str) == Some("text");
    let text: String = content
        .iter()
        .filter(|block| is_text(block))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    let Some(json_text) = extract_conforming_json(&text, schema) else {
        warn_no_conforming_json(model, text.len());
        return false;
    };
    let Some(first) = content.iter().position(is_text) else {
        return false;
    };
    let mut rewritten = Vec::with_capacity(content.len());
    for (index, block) in std::mem::take(content).into_iter().enumerate() {
        if index == first {
            rewritten.push(json!({"type": "text", "text": json_text}));
        } else if !is_text(&block) {
            rewritten.push(block);
        }
    }
    *content = rewritten;
    true
}

fn block_index(event: &SseEvent) -> Option<u64> {
    event.data.get("index").and_then(Value::as_u64)
}

/// 流式：对缓冲的 `message_start` 之后的全部事件做改写——丢弃原 text 块的事件，
/// 在第一个 text 块处发出 start / 单个 text_delta / stop，并把块下标重新顺序编号
/// （thinking、tool_use 块原样保留）。提取失败、或流里已有 `error` 事件时原样返回。
pub(super) fn rewrite_sse_events(
    events: Vec<SseEvent>,
    schema: &Value,
    model: &str,
) -> Vec<SseEvent> {
    if events.iter().any(|event| event.event == "error") {
        return events;
    }

    let mut text_blocks = std::collections::HashSet::new();
    for event in &events {
        if event.event == "content_block_start"
            && event.data["content_block"]["type"] == "text"
            && let Some(index) = block_index(event)
        {
            text_blocks.insert(index);
        }
    }
    let mut text = String::new();
    for event in &events {
        if event.event == "content_block_delta"
            && event.data["delta"]["type"] == "text_delta"
            && block_index(event).is_some_and(|index| text_blocks.contains(&index))
            && let Some(piece) = event.data["delta"]["text"].as_str()
        {
            text.push_str(piece);
        }
    }
    let Some(json_text) = extract_conforming_json(&text, schema) else {
        warn_no_conforming_json(model, text.len());
        return events;
    };

    let mut index_map: HashMap<u64, u64> = HashMap::new();
    let mut next_index = 0u64;
    let mut replaced = false;
    let mut out = Vec::with_capacity(events.len());
    for mut event in events {
        let Some(old) = block_index(&event) else {
            out.push(event);
            continue;
        };
        if text_blocks.contains(&old) {
            if event.event == "content_block_start" && !replaced {
                replaced = true;
                let index = next_index;
                next_index += 1;
                out.push(SseEvent::new(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                ));
                out.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": json_text}
                    }),
                ));
                out.push(SseEvent::new(
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                ));
            }
            continue;
        }
        let new = *index_map.entry(old).or_insert_with(|| {
            let index = next_index;
            next_index += 1;
            index
        });
        event.data["index"] = json!(new);
        out.push(event);
    }
    out
}

/// 流式结构化输出的缓冲：`message_start` 已先行发出，其余事件（含初始 text 块 start
/// 和结尾事件）攒到流结束，再一次性改写发出。
pub(super) struct StructuredCollector {
    schema: Value,
    model: String,
    held: Vec<SseEvent>,
}

impl StructuredCollector {
    pub(super) fn new(schema: Value, model: &str) -> Self {
        Self {
            schema,
            model: model.to_string(),
            held: Vec::new(),
        }
    }

    pub(super) fn hold(&mut self, events: Vec<SseEvent>) {
        self.held.extend(events);
    }

    /// 流被中断：按原顺序交还已缓冲的事件，不做改写。
    pub(super) fn take_unchanged(&mut self) -> Vec<SseEvent> {
        std::mem::take(&mut self.held)
    }

    /// 流正常结束：缓冲 + 结尾事件一起改写。
    pub(super) fn finish(&mut self, final_events: Vec<SseEvent>) -> Vec<SseEvent> {
        let mut events = std::mem::take(&mut self.held);
        events.extend(final_events);
        rewrite_sse_events(events, &self.schema, &self.model)
    }
}

/// 把初始事件拆成 (立即发送的 message_start, 需要缓冲的其余事件)。
pub(super) fn split_initial_events(initial: Vec<SseEvent>) -> (Vec<SseEvent>, Vec<SseEvent>) {
    let split = initial
        .iter()
        .position(|event| event.event != "message_start")
        .unwrap_or(initial.len());
    let mut sent = initial;
    let held = sent.split_off(split);
    (sent, held)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "ok": {"type": "boolean"},
                "reason": {"type": "string"},
                "impossible": {"type": "boolean"}
            },
            "required": ["ok", "reason"],
            "additionalProperties": false
        })
    }

    fn start(index: u64, block: Value) -> SseEvent {
        SseEvent::new(
            "content_block_start",
            json!({"type": "content_block_start", "index": index, "content_block": block}),
        )
    }
    fn delta(index: u64, delta: Value) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": index, "delta": delta}),
        )
    }
    fn stop(index: u64) -> SseEvent {
        SseEvent::new(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        )
    }
    fn tail() -> Vec<SseEvent> {
        vec![
            SseEvent::new(
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}}),
            ),
            SseEvent::new("message_stop", json!({"type": "message_stop"})),
        ]
    }

    #[test]
    fn non_stream_replaces_text_keeps_thinking_and_tool_use() {
        let mut body = json!({
            "id": "msg_1", "model": "m", "stop_reason": "end_turn", "usage": {"output_tokens": 5},
            "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "s"},
                {"type": "text", "text": "The condition holds.\n{\"ok\":true,\"reason\":\"done\"}"},
                {"type": "tool_use", "id": "t", "name": "n", "input": {}}
            ]
        });
        assert!(rewrite_message_json(&mut body, &goal_schema(), "m"));
        assert_eq!(body["id"], "msg_1");
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["usage"]["output_tokens"], 5);
        let content = body["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(
            content[1],
            json!({"type": "text", "text": "{\"ok\":true,\"reason\":\"done\"}"})
        );
        assert_eq!(content[2]["type"], "tool_use");
    }

    #[test]
    fn non_stream_without_conforming_json_is_unchanged() {
        let original = json!({"content": [{"type": "text", "text": "I'm checking... no json"}]});
        let mut body = original.clone();
        assert!(!rewrite_message_json(&mut body, &goal_schema(), "m"));
        assert_eq!(body, original);
    }

    #[test]
    fn stream_rewrite_emits_single_text_block_and_renumbers() {
        let mut events = vec![
            start(0, json!({"type": "thinking", "thinking": ""})),
            delta(0, json!({"type": "thinking_delta", "thinking": "hmm"})),
            stop(0),
            start(1, json!({"type": "text", "text": ""})),
            delta(
                1,
                json!({"type": "text_delta", "text": "I'm checking. {\"ok\":"}),
            ),
            delta(
                1,
                json!({"type": "text_delta", "text": "false,\"reason\":\"x\"}"}),
            ),
            stop(1),
            start(
                2,
                json!({"type": "tool_use", "id": "t", "name": "n", "input": {}}),
            ),
            stop(2),
        ];
        events.extend(tail());
        let out = rewrite_sse_events(events, &goal_schema(), "m");
        let names: Vec<_> = out.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(
            names,
            [
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(out[3].data["index"], 1);
        assert_eq!(
            out[4].data["delta"]["text"],
            "{\"ok\":false,\"reason\":\"x\"}"
        );
        assert_eq!(out[6].data["index"], 2);
        assert_eq!(out[8].data["usage"]["output_tokens"], 9);
    }

    #[test]
    fn stream_without_conforming_json_or_with_error_is_unchanged() {
        let mut events = vec![
            start(0, json!({"type": "text", "text": ""})),
            delta(0, json!({"type": "text_delta", "text": "no json here"})),
            stop(0),
        ];
        events.extend(tail());
        let out = rewrite_sse_events(events.clone(), &goal_schema(), "m");
        assert_eq!(
            out.iter().map(SseEvent::to_sse_string).collect::<Vec<_>>(),
            events
                .iter()
                .map(SseEvent::to_sse_string)
                .collect::<Vec<_>>()
        );

        let mut with_error = vec![
            start(0, json!({"type": "text", "text": ""})),
            delta(
                0,
                json!({"type": "text_delta", "text": "{\"ok\":true,\"reason\":\"r\"}"}),
            ),
            SseEvent::new("error", json!({"type": "error"})),
        ];
        with_error.extend(tail());
        let out = rewrite_sse_events(with_error.clone(), &goal_schema(), "m");
        assert_eq!(out.len(), with_error.len());
        assert_eq!(
            out[1].data["delta"]["text"],
            "{\"ok\":true,\"reason\":\"r\"}"
        );
    }

    #[test]
    fn split_initial_events_sends_only_message_start() {
        let initial = vec![
            SseEvent::new("message_start", json!({"type": "message_start"})),
            start(0, json!({"type": "text", "text": ""})),
        ];
        let (sent, held) = split_initial_events(initial);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].event, "message_start");
        assert_eq!(held.len(), 1);
    }
}
