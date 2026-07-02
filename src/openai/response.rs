//! Anthropic `SseEvent` → OpenAI 响应翻译
//!
//! 出站唯一分叉。流式与非流式都把上游字节喂给同一个 `StreamContext`（复用其 `<invoke>`
//! 文本泄漏恢复 + thinking 检测），得到规范的 Anthropic [`SseEvent`] 序列，再由本模块的
//! [`OpenAiResponseBuilder`] 状态机映射成：
//! - 流式：`chat.completion.chunk` 增量（`push_event`）+ 末尾 finish chunk（`finish_chunk`）；
//! - 非流式：聚合后的 `chat.completion`（`build_completion`）。
//!
//! Anthropic→OpenAI 字段映射：
//! | Anthropic SSE | OpenAI delta |
//! |---|---|
//! | `text_delta` | `content` |
//! | `thinking_delta` | `reasoning_content` |
//! | tool_use `content_block_start` | `tool_calls[i]{id,function.name}` |
//! | `input_json_delta` | `tool_calls[i].function.arguments` |
//! | `message_delta.stop_reason` | `finish_reason`（见 [`map_finish_reason`]） |

use serde_json::{Value, json};

use crate::anthropic::stream::SseEvent;

/// OpenAI 响应用量（已由上游 + 本地估算解析完毕）
#[derive(Clone, Copy, Default)]
pub struct OpenAiUsage {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
}

impl OpenAiUsage {
    fn to_json(self) -> Value {
        json!({
            "prompt_tokens": self.prompt_tokens.max(0),
            "completion_tokens": self.completion_tokens.max(0),
            "total_tokens": (self.prompt_tokens + self.completion_tokens).max(0),
        })
    }
}

/// 累积中的单个工具调用（流式增量拼装）
#[derive(Default)]
struct ToolCallAcc {
    /// OpenAI tool_calls 数组里的序号
    oai_index: usize,
    id: String,
    name: String,
    /// 累积的 arguments JSON 文本
    arguments: String,
}

/// Anthropic→OpenAI 响应状态机（流式 + 非流式共用累积）
pub struct OpenAiResponseBuilder {
    id: String,
    created: i64,
    model: String,
    /// 流式：首个 chunk 是否已发送 role
    role_sent: bool,
    /// Anthropic content_block index → 累积工具调用
    tool_calls: std::collections::BTreeMap<i32, ToolCallAcc>,
    /// 下一个 OpenAI tool_calls 序号
    next_oai_index: usize,
    /// 非流式聚合：正文文本
    text: String,
    /// 非流式聚合：reasoning 文本
    reasoning: String,
    /// 上游 stop_reason（来自 message_delta）
    stop_reason: Option<String>,
}

impl OpenAiResponseBuilder {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            created: chrono::Utc::now().timestamp(),
            model: model.into(),
            role_sent: false,
            tool_calls: std::collections::BTreeMap::new(),
            next_oai_index: 0,
            text: String::new(),
            reasoning: String::new(),
            stop_reason: None,
        }
    }

    /// 包一个 `chat.completion.chunk` 信封（含单个 choice delta）
    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }],
        })
    }

    /// 处理一个 Anthropic `SseEvent`，产出 0..N 个 OpenAI 流式 chunk（JSON 值）。
    ///
    /// 不包含末尾的 finish chunk —— 那个带 usage，由 [`Self::finish_chunk`] 在流末尾单独发。
    pub fn push_event(&mut self, ev: &SseEvent) -> Vec<Value> {
        let mut out = Vec::new();
        let data = &ev.data;
        match ev.event.as_str() {
            "content_block_start" => {
                // 仅 tool_use 需要在 start 时发一个带 id/name 的 tool_calls 增量。
                // text/thinking 块等到 delta 才出内容。
                let block = data.get("content_block");
                let btype = block
                    .and_then(|b| b.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if btype == "tool_use" {
                    let anth_index = data.get("index").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    let id = block
                        .and_then(|b| b.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .and_then(|b| b.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let oai_index = self.next_oai_index;
                    self.next_oai_index += 1;
                    self.tool_calls.insert(
                        anth_index,
                        ToolCallAcc {
                            oai_index,
                            id: id.clone(),
                            name: name.clone(),
                            arguments: String::new(),
                        },
                    );
                    let delta = json!({
                        "tool_calls": [{
                            "index": oai_index,
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": "" },
                        }]
                    });
                    let delta = self.maybe_with_role(delta);
                    out.push(self.chunk(delta, None));
                }
            }
            "content_block_delta" => {
                let anth_index = data.get("index").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let delta = data.get("delta");
                let dtype = delta
                    .and_then(|d| d.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match dtype {
                    "text_delta" => {
                        if let Some(t) =
                            delta.and_then(|d| d.get("text")).and_then(|v| v.as_str())
                            && !t.is_empty()
                        {
                            self.text.push_str(t);
                            let delta = self.maybe_with_role(json!({ "content": t }));
                            out.push(self.chunk(delta, None));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) =
                            delta.and_then(|d| d.get("thinking")).and_then(|v| v.as_str())
                            && !t.is_empty()
                        {
                            self.reasoning.push_str(t);
                            let delta = self.maybe_with_role(json!({ "reasoning_content": t }));
                            out.push(self.chunk(delta, None));
                        }
                    }
                    "input_json_delta" => {
                        if let Some(pj) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(|v| v.as_str())
                            && let Some(acc) = self.tool_calls.get_mut(&anth_index)
                        {
                            acc.arguments.push_str(pj);
                            let oai_index = acc.oai_index;
                            out.push(self.chunk(
                                json!({
                                    "tool_calls": [{
                                        "index": oai_index,
                                        "function": { "arguments": pj },
                                    }]
                                }),
                                None,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = data
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|v| v.as_str())
                {
                    self.stop_reason = Some(reason.to_string());
                }
            }
            _ => {}
        }
        out
    }

    /// 首个内容 chunk 需带 `role:"assistant"`；之后不再带。就地把 role 合并进 delta。
    fn maybe_with_role(&mut self, mut delta: Value) -> Value {
        if !self.role_sent {
            self.role_sent = true;
            if let Value::Object(map) = &mut delta {
                map.insert("role".to_string(), json!("assistant"));
            }
        }
        delta
    }

    /// 流末尾的收尾 chunk：delta + finish_reason + usage。
    ///
    /// 若整段响应无任何内容（无 text/thinking/tool_use，如空 `end_turn`），role 从未随内容
    /// chunk 发出过，此处补一个 `role:"assistant"`，避免给只看首 chunk `delta.role` 的客户端
    /// 留下畸形流。
    pub fn finish_chunk(&self, usage: OpenAiUsage) -> Value {
        let finish = map_finish_reason(self.stop_reason.as_deref(), !self.tool_calls.is_empty());
        let delta = if self.role_sent {
            json!({})
        } else {
            json!({ "role": "assistant" })
        };
        let mut chunk = self.chunk(delta, Some(finish));
        // usage 挂在 chunk 顶层（与 OpenAI `stream_options.include_usage` 一致）
        if let Value::Object(map) = &mut chunk {
            map.insert("usage".to_string(), usage.to_json());
        }
        chunk
    }
}

/// Anthropic `stop_reason` → OpenAI `finish_reason`
///
/// - `end_turn` → `stop`
/// - `tool_use`（或已产出 tool_calls）→ `tool_calls`
/// - `max_tokens` / `model_context_window_exceeded` → `length`
/// - 其他/缺省 → `stop`
pub fn map_finish_reason(stop_reason: Option<&str>, has_tool_calls: bool) -> &'static str {
    match stop_reason {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") | Some("model_context_window_exceeded") => "length",
        Some("end_turn") | Some("stop_sequence") => {
            if has_tool_calls {
                "tool_calls"
            } else {
                "stop"
            }
        }
        _ => {
            if has_tool_calls {
                "tool_calls"
            } else {
                "stop"
            }
        }
    }
}

impl OpenAiResponseBuilder {
    /// 非流式：把累积的 text / reasoning / tool_calls 聚合成一个 `chat.completion`。
    ///
    /// 调用前需先把整段响应的所有 `SseEvent` 喂给 [`Self::push_event`]（其累积副作用同时服务
    /// 流式与非流式）。
    pub fn build_completion(&self, usage: OpenAiUsage) -> Value {
        let has_tool_calls = !self.tool_calls.is_empty();
        let finish = map_finish_reason(self.stop_reason.as_deref(), has_tool_calls);

        let mut message = serde_json::Map::new();
        message.insert("role".to_string(), json!("assistant"));
        // content：有文本则为文本，无文本（纯工具调用）则为 null
        if self.text.is_empty() {
            message.insert("content".to_string(), Value::Null);
        } else {
            message.insert("content".to_string(), json!(self.text));
        }
        // reasoning_content：思考内容（非空才挂）
        if !self.reasoning.is_empty() {
            message.insert("reasoning_content".to_string(), json!(self.reasoning));
        }
        // tool_calls：按 oai_index 升序输出
        if has_tool_calls {
            let mut calls: Vec<&ToolCallAcc> = self.tool_calls.values().collect();
            calls.sort_by_key(|c| c.oai_index);
            let arr: Vec<Value> = calls
                .into_iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "type": "function",
                        "function": {
                            "name": c.name,
                            // arguments 必须是 JSON 字符串；为空时给 "{}"
                            "arguments": if c.arguments.is_empty() { "{}".to_string() } else { c.arguments.clone() },
                        }
                    })
                })
                .collect();
            message.insert("tool_calls".to_string(), Value::Array(arr));
        }

        json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": finish,
            }],
            "usage": usage.to_json(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::stream::SseEvent;

    fn ev(event: &str, data: Value) -> SseEvent {
        SseEvent::new(event, data)
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(map_finish_reason(Some("end_turn"), false), "stop");
        assert_eq!(map_finish_reason(Some("tool_use"), false), "tool_calls");
        assert_eq!(map_finish_reason(Some("max_tokens"), false), "length");
        assert_eq!(
            map_finish_reason(Some("model_context_window_exceeded"), false),
            "length"
        );
        // end_turn 但已产出 tool_calls → tool_calls
        assert_eq!(map_finish_reason(Some("end_turn"), true), "tool_calls");
        // 缺省
        assert_eq!(map_finish_reason(None, false), "stop");
    }

    #[test]
    fn empty_stream_finish_chunk_carries_role() {
        // 无任何内容事件：finish_chunk 必须补 role:"assistant"
        let b = OpenAiResponseBuilder::new("m");
        let chunk = b.finish_chunk(OpenAiUsage::default());
        assert_eq!(chunk["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn text_delta_first_chunk_has_role_then_not() {
        let mut b = OpenAiResponseBuilder::new("m");
        let chunks = b.push_event(&ev(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "he"}}),
        ));
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "he");
        // 第二次不再带 role
        let chunks2 = b.push_event(&ev(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "llo"}}),
        ));
        assert!(chunks2[0]["choices"][0]["delta"].get("role").is_none());
        // role 已发，finish 不再补
        let fin = b.finish_chunk(OpenAiUsage::default());
        assert!(fin["choices"][0]["delta"].get("role").is_none());
    }

    #[test]
    fn thinking_delta_maps_to_reasoning_content() {
        let mut b = OpenAiResponseBuilder::new("m");
        let chunks = b.push_event(&ev(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
        ));
        assert_eq!(chunks[0]["choices"][0]["delta"]["reasoning_content"], "hmm");
    }

    #[test]
    fn tool_use_stream_emits_id_name_then_args() {
        let mut b = OpenAiResponseBuilder::new("m");
        // tool_use 块开始（Anthropic index 不必从 0 起）
        let start = b.push_event(&ev(
            "content_block_start",
            json!({"index": 2, "content_block": {
                "type": "tool_use", "id": "toolu_1", "name": "get_x", "input": {}}}),
        ));
        let tc = &start[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 0); // OpenAI 序号从 0 连续
        assert_eq!(tc["id"], "toolu_1");
        assert_eq!(tc["function"]["name"], "get_x");
        // 参数增量
        let arg = b.push_event(&ev(
            "content_block_delta",
            json!({"index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"a\":1}"}}),
        ));
        let tc2 = &arg[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc2["index"], 0);
        assert_eq!(tc2["function"]["arguments"], "{\"a\":1}");
    }

    #[test]
    fn non_stream_build_completion_aggregates() {
        let mut b = OpenAiResponseBuilder::new("m");
        b.push_event(&ev(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "answer"}}),
        ));
        b.push_event(&ev(
            "message_delta",
            json!({"delta": {"stop_reason": "end_turn"}}),
        ));
        let usage = OpenAiUsage {
            prompt_tokens: 10,
            completion_tokens: 3,
        };
        let body = b.build_completion(usage);
        assert_eq!(body["object"], "chat.completion");
        let msg = &body["choices"][0]["message"];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "answer");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["usage"]["prompt_tokens"], 10);
        assert_eq!(body["usage"]["total_tokens"], 13);
    }

    #[test]
    fn non_stream_tool_call_content_null_args_string() {
        let mut b = OpenAiResponseBuilder::new("m");
        b.push_event(&ev(
            "content_block_start",
            json!({"index": 0, "content_block": {
                "type": "tool_use", "id": "t1", "name": "f", "input": {}}}),
        ));
        b.push_event(&ev(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"k\":2}"}}),
        ));
        b.push_event(&ev(
            "message_delta",
            json!({"delta": {"stop_reason": "tool_use"}}),
        ));
        let body = b.build_completion(OpenAiUsage::default());
        let msg = &body["choices"][0]["message"];
        // 纯工具调用：content 为 null
        assert!(msg["content"].is_null());
        let call = &msg["tool_calls"][0];
        assert_eq!(call["id"], "t1");
        // arguments 必须是 JSON 字符串
        assert_eq!(call["function"]["arguments"], "{\"k\":2}");
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn empty_tool_args_become_braces() {
        let mut b = OpenAiResponseBuilder::new("m");
        b.push_event(&ev(
            "content_block_start",
            json!({"index": 0, "content_block": {
                "type": "tool_use", "id": "t1", "name": "f", "input": {}}}),
        ));
        // 无 input_json_delta
        let body = b.build_completion(OpenAiUsage::default());
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
    }
}



