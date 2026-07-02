//! OpenAI `ChatCompletionRequest` → Anthropic `MessagesRequest` 归一化
//!
//! 这是 OpenAI 兼容的**唯一**新增请求转换层。归一化后产物是真正的 `MessagesRequest`，
//! 因此下游 `prepare_kiro_request` 及所有 Anthropic 格式专用治理函数都能原样复用。

use serde_json::{Value, json};

use crate::anthropic::types::{MessagesRequest, Message, SystemMessage, Thinking, Tool};

use super::types::ChatCompletionRequest;

/// 最大思考预算（与 Anthropic 路径 `override_thinking_from_model_name` 对齐）
const THINKING_BUDGET_TOKENS: i32 = 20000;

/// 非 Claude 别名（gpt-* 等）的默认映射目标。
///
/// 选 sonnet-4.6 作默认：1M 上下文、速度/能力均衡。默认模型升级时只改这一处
/// （历史上 4.5 → 4.6 → 4.7 → 4.8 多次变动，散落字面量易漏改）。
const DEFAULT_ALIAS_MODEL: &str = "claude-sonnet-4-6";

/// 解析客户端模型名 → kiro.rs 可识别的模型名。
///
/// 策略（用户选择「别名 + Claude 透传」）：
/// - `claude-*`：原样透传（保留精确名与 `-thinking` 后缀，交给下游 `map_model` 模糊匹配）；
/// - `gpt-*` / 其他 OpenAI 别名：映射到默认 Claude 模型（sonnet-4.6，1M 上下文），
///   若原名带 `-thinking` 后缀则保留，使思考开关仍生效。
fn normalize_model(model: &str) -> String {
    let lower = model.to_lowercase();
    // 已是 Claude 名（含 sonnet/opus/haiku 关键字）→ 透传
    if lower.contains("claude")
        || lower.contains("sonnet")
        || lower.contains("opus")
        || lower.contains("haiku")
    {
        return model.to_string();
    }
    // OpenAI 别名 → 默认 Claude；保留 thinking 后缀
    let base = DEFAULT_ALIAS_MODEL;
    if lower.contains("thinking") {
        format!("{base}-thinking")
    } else {
        base.to_string()
    }
}

/// 模型名带 `thinking` 时构造 Anthropic `Thinking` 配置。
///
/// 与 Anthropic 路径 `override_thinking_from_model_name` 对齐：opus-4.6 用 `adaptive`，
/// 其余用 `enabled`。
fn thinking_from_model(model: &str) -> Option<Thinking> {
    let lower = model.to_lowercase();
    if !lower.contains("thinking") {
        return None;
    }
    let is_opus_4_6 =
        lower.contains("opus") && (lower.contains("4-6") || lower.contains("4.6"));
    let thinking_type = if is_opus_4_6 { "adaptive" } else { "enabled" };
    Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: THINKING_BUDGET_TOKENS,
    })
}

/// 把 OpenAI 的 `content`（string 或 part 数组）转成 Anthropic 内容块数组。
///
/// 支持的 part：`text`/`input_text` → text 块；`image_url`/`input_image` 且为 data URL
/// → image 块（base64）。HTTP(S) 图片 URL 无法被上游拉取，直接跳过（kiro.rs 不做外链抓取）。
fn convert_content_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![json!({ "type": "text", "text": s })]
            }
        }
        Value::Array(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match part_type {
                    "text" | "input_text" => {
                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            blocks.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    "image_url" | "image" | "input_image" => {
                        if let Some(block) = convert_image_part(part) {
                            blocks.push(block);
                        }
                    }
                    _ => {}
                }
            }
            blocks
        }
        _ => Vec::new(),
    }
}

/// 把 OpenAI 图片 part 转成 Anthropic image 块（仅支持 data URL）。
///
/// 形态兼容：`{image_url:{url}}`、`{image_url:"<url>"}`、`{image:{url}}`。
/// data URL 形如 `data:image/png;base64,<data>`。
fn convert_image_part(part: &Value) -> Option<Value> {
    // 取出 url 字符串
    let url = part
        .get("image_url")
        .and_then(|v| v.get("url").and_then(|u| u.as_str()).or_else(|| v.as_str()))
        .or_else(|| part.get("image").and_then(|v| v.get("url").and_then(|u| u.as_str())))
        .or_else(|| part.get("url").and_then(|v| v.as_str()))?;

    let rest = url.strip_prefix("data:")?;
    let (media_type, b64part) = rest.split_once(',')?;
    let media_type = media_type
        .strip_suffix(";base64")
        .unwrap_or(media_type)
        .to_string();
    let media_type = if media_type.is_empty() {
        "image/png".to_string()
    } else {
        media_type
    };
    let data = b64part.to_string();

    Some(json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media_type,
            "data": data,
        }
    }))
}

/// 把 assistant 消息的 `tool_calls` 数组转成 Anthropic `tool_use` 内容块。
///
/// OpenAI：`{id, type:"function", function:{name, arguments:"<json-string>"}}`
/// Anthropic：`{type:"tool_use", id, name, input:<json-object>}`
/// `arguments` 是 JSON 字符串，解析失败时退化为 `{}`。
fn convert_tool_calls(tool_calls: &[Value]) -> Vec<Value> {
    let mut blocks = Vec::new();
    for tc in tool_calls {
        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let func = tc.get("function");
        let name = func
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let input = func
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or_else(|| json!({}));
        blocks.push(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }));
    }
    blocks
}

/// 把一条 OpenAI `tool` 消息转成 Anthropic `tool_result` 内容块。
fn convert_tool_result(msg: &Value) -> Option<Value> {
    let tool_use_id = msg.get("tool_call_id").and_then(|v| v.as_str())?;
    // content 可为 string 或 part 数组；统一转成内容块数组
    let content_value = msg.get("content").cloned().unwrap_or(Value::Null);
    let blocks = convert_content_blocks(&content_value);
    let content = if blocks.is_empty() {
        // 空结果也给一个空文本块，保持配对合法
        json!([{ "type": "text", "text": "" }])
    } else {
        Value::Array(blocks)
    };
    Some(json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": content,
    }))
}

/// 构造一条 Anthropic 消息（content 为内容块数组）。
fn make_message(role: &str, content: Vec<Value>) -> Message {
    Message {
        role: role.to_string(),
        content: Value::Array(content),
    }
}

/// 把 OpenAI `tools` 数组转成 Anthropic `Tool`。
///
/// 兼容两种形态：
/// - 嵌套式：`{type:"function", function:{name, description, parameters}}`
/// - 扁平式：`{type:"function", name, description, parameters}`
fn convert_tools(tools: &[Value]) -> Option<Vec<Tool>> {
    let mut out = Vec::new();
    for t in tools {
        let func = t.get("function");
        let name = func
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .or_else(|| t.get("name").and_then(|v| v.as_str()))
            .unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let description = func
            .and_then(|f| f.get("description"))
            .and_then(|v| v.as_str())
            .or_else(|| t.get("description").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        let parameters = func
            .and_then(|f| f.get("parameters"))
            .or_else(|| t.get("parameters"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let input_schema = match parameters {
            Value::Object(map) => map.into_iter().collect(),
            _ => Default::default(),
        };
        out.push(Tool {
            tool_type: None,
            name: name.to_string(),
            description,
            input_schema,
            max_uses: None,
            cache_control: None,
        });
    }
    if out.is_empty() { None } else { Some(out) }
}

/// 把 OpenAI Chat Completions 请求归一化成 Anthropic `MessagesRequest`。
///
/// 连续的 `tool` 消息会被批量并入同一条 `user` 消息的 `tool_result` 块，符合 Anthropic
/// 「assistant 的多个 tool_use 由下一条 user 的多个 tool_result 应答」的配对约定。
pub fn to_messages_request(req: &ChatCompletionRequest) -> MessagesRequest {
    let model = normalize_model(&req.model);
    let thinking = thinking_from_model(&model);

    let mut system_texts: Vec<String> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    // 累积连续 tool 消息，遇到下一条非 tool 消息时 flush 成一条 user(tool_result)
    let mut pending_tool_results: Vec<Value> = Vec::new();

    fn flush(messages: &mut Vec<Message>, pending: &mut Vec<Value>) {
        if !pending.is_empty() {
            messages.push(make_message("user", std::mem::take(pending)));
        }
    }

    for msg in &req.messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = msg.get("content").cloned().unwrap_or(Value::Null);

        match role {
            "system" | "developer" => {
                flush(&mut messages, &mut pending_tool_results);
                // system 内容可为 string 或 part 数组；提取所有文本拼接
                for block in convert_content_blocks(&content) {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        system_texts.push(text.to_string());
                    }
                }
            }
            "tool" => {
                if let Some(tr) = convert_tool_result(msg) {
                    pending_tool_results.push(tr);
                }
            }
            "assistant" => {
                flush(&mut messages, &mut pending_tool_results);
                let mut blocks = convert_content_blocks(&content);
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
                    blocks.extend(convert_tool_calls(tool_calls));
                }
                // 全空的 assistant 消息跳过，避免空块
                if !blocks.is_empty() {
                    messages.push(make_message("assistant", blocks));
                }
            }
            _ => {
                // user 及未知角色统一按 user 处理
                flush(&mut messages, &mut pending_tool_results);
                let blocks = convert_content_blocks(&content);
                if !blocks.is_empty() {
                    messages.push(make_message("user", blocks));
                }
            }
        }
    }
    // 收尾：末尾若仍有未 flush 的 tool 结果
    flush(&mut messages, &mut pending_tool_results);

    let system = if system_texts.is_empty() {
        None
    } else {
        Some(vec![SystemMessage {
            text: system_texts.join("\n\n"),
            cache_control: None,
        }])
    };

    let tools = req.tools.as_deref().and_then(convert_tools);

    MessagesRequest {
        model,
        max_tokens: req.resolved_max_tokens(),
        messages,
        stream: req.stream,
        system,
        tools,
        tool_choice: None,
        thinking,
        output_config: None,
        metadata: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(value: serde_json::Value) -> ChatCompletionRequest {
        serde_json::from_value(value).expect("valid ChatCompletionRequest")
    }

    fn text_of(msg: &Message, idx: usize) -> String {
        msg.content[idx]["text"].as_str().unwrap_or("").to_string()
    }

    #[test]
    fn gpt_alias_maps_to_default_claude() {
        assert_eq!(normalize_model("gpt-4o"), DEFAULT_ALIAS_MODEL);
        assert_eq!(normalize_model("gpt-4"), DEFAULT_ALIAS_MODEL);
        // 别名带 thinking 后缀要保留
        assert_eq!(
            normalize_model("gpt-4o-thinking"),
            format!("{DEFAULT_ALIAS_MODEL}-thinking")
        );
    }

    #[test]
    fn claude_names_pass_through() {
        assert_eq!(normalize_model("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(
            normalize_model("claude-sonnet-4-6-thinking"),
            "claude-sonnet-4-6-thinking"
        );
    }

    #[test]
    fn thinking_suffix_opus_46_is_adaptive_others_enabled() {
        let opus = thinking_from_model("claude-opus-4-6-thinking").unwrap();
        assert_eq!(opus.thinking_type, "adaptive");
        assert_eq!(opus.budget_tokens, THINKING_BUDGET_TOKENS);

        let sonnet = thinking_from_model("claude-sonnet-4-6-thinking").unwrap();
        assert_eq!(sonnet.thinking_type, "enabled");

        assert!(thinking_from_model("claude-opus-4-8").is_none());
    }

    #[test]
    fn system_and_user_string_content() {
        let r = req(json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"}
            ]
        }));
        let m = to_messages_request(&r);
        assert_eq!(m.system.as_ref().unwrap()[0].text, "be brief");
        assert_eq!(m.messages.len(), 1);
        assert_eq!(m.messages[0].role, "user");
        assert_eq!(text_of(&m.messages[0], 0), "hi");
    }

    #[test]
    fn multiple_system_messages_joined() {
        let r = req(json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "a"},
                {"role": "developer", "content": "b"},
                {"role": "user", "content": "x"}
            ]
        }));
        let m = to_messages_request(&r);
        assert_eq!(m.system.as_ref().unwrap()[0].text, "a\n\nb");
    }

    #[test]
    fn assistant_tool_calls_become_tool_use() {
        let r = req(json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"NYC\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ]
        }));
        let m = to_messages_request(&r);
        // user, assistant(tool_use), user(tool_result)
        assert_eq!(m.messages.len(), 3);
        let tu = &m.messages[1].content[0];
        assert_eq!(tu["type"], "tool_use");
        assert_eq!(tu["id"], "call_1");
        assert_eq!(tu["name"], "get_weather");
        assert_eq!(tu["input"]["city"], "NYC");
        let tr = &m.messages[2].content[0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["tool_use_id"], "call_1");
    }

    #[test]
    fn consecutive_tool_messages_batched_into_one_user_turn() {
        let r = req(json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "a", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "b", "type": "function", "function": {"name": "g", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "a", "content": "r1"},
                {"role": "tool", "tool_call_id": "b", "content": "r2"}
            ]
        }));
        let m = to_messages_request(&r);
        // assistant(2 tool_use), user(2 tool_result) — 两条 tool 合并成一条 user
        assert_eq!(m.messages.len(), 2);
        assert_eq!(m.messages[1].role, "user");
        assert_eq!(m.messages[1].content.as_array().unwrap().len(), 2);
        assert_eq!(m.messages[1].content[0]["tool_use_id"], "a");
        assert_eq!(m.messages[1].content[1]["tool_use_id"], "b");
    }

    #[test]
    fn bad_tool_arguments_fall_back_to_empty_object() {
        let r = req(json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "x", "type": "function",
                     "function": {"name": "f", "arguments": "not json"}}
                ]}
            ]
        }));
        let m = to_messages_request(&r);
        assert_eq!(m.messages[0].content[0]["input"], json!({}));
    }

    #[test]
    fn empty_assistant_message_skipped() {
        let r = req(json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": ""}
            ]
        }));
        let m = to_messages_request(&r);
        // 空 assistant 被丢弃
        assert_eq!(m.messages.len(), 1);
        assert_eq!(m.messages[0].role, "user");
    }

    #[test]
    fn image_data_url_parsed_http_url_dropped() {
        let r = req(json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}}
            ]}]
        }));
        let m = to_messages_request(&r);
        let blocks = m.messages[0].content.as_array().unwrap();
        // text + 1 image(data url)，http url 被丢弃
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn tools_nested_and_flat_forms() {
        let r = req(json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "x"}],
            "tools": [
                {"type": "function", "function": {
                    "name": "nested", "description": "d1",
                    "parameters": {"type": "object", "properties": {}}}},
                {"type": "function", "name": "flat", "description": "d2",
                 "parameters": {"type": "object"}}
            ]
        }));
        let m = to_messages_request(&r);
        let tools = m.tools.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "nested");
        assert_eq!(tools[0].description, "d1");
        assert_eq!(tools[1].name, "flat");
    }

    #[test]
    fn max_completion_tokens_fallback() {
        let r = req(json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "x"}],
            "max_completion_tokens": 1234
        }));
        let m = to_messages_request(&r);
        assert_eq!(m.max_tokens, 1234);
    }
}



