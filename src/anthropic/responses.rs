//! OpenAI Responses 兼容端点
//!
//! 把 OpenAI `POST /v1/responses` 请求翻译成内部的 Anthropic
//! [`MessagesRequest`]，复用 [`super::handlers::post_messages`] 的完整链路，
//! 再把 Anthropic 响应翻译回 Responses 格式。
//!
//! 为什么需要这个端点：Codex CLI 自 0.122 起移除了 `wire_api = "chat"`，
//! 只支持 `wire_api = "responses"`——即向 `<base_url>/responses` POST，
//! 走 OpenAI 的 Responses API 协议。因此 `chat/completions` 端点对 Codex
//! 无效，必须提供 Responses 端点。
//!
//! 工具桥接（完整 codex 能力的关键）：codex 的工具按声明类型分两类，
//! 应答的 item 种类必须与声明一致，否则 codex 直接终止本轮
//! （"tool <name> invoked with incompatible payload"）：
//! - `type:"function"`（shell / exec_command / write_stdin / update_plan /
//!   view_image / MCP 工具）→ 应答 `function_call`（JSON 字符串 `arguments`）；
//! - `type:"custom"`（自由文本工具：apply_patch 的 lark 语法、code-mode exec）
//!   → 应答 `custom_tool_call`（原始字符串 `input`）。
//!   Anthropic 侧没有自由文本工具，进方向包一层
//!   `{"input": <string>}` 单字段 schema，出方向再解包。
//! 每个请求维护一张 name → 声明类型 的 [`ToolKindMap`]，请求翻译时生成、
//! 响应构造时消费，保证出方向 item 类型永远与声明一致。
//!
//! web_search 始终由 kiro-rs 内部代答（codex 自带的搜索插件在自定义
//! provider 下 401）：注入原生 `web_search_20250305`，请求因此必然进入
//! handlers 的 web_search agentic loop——该 loop 会内部消化 web_search、
//! 把其它 client 工具的 tool_use 原样透传（见 websearch_loop.rs）。
//!
//! 说明：内部调用始终以非流式方式执行，`stream: true` 的请求在拿到完整结果后
//! 合成为 Responses 的 SSE 事件序列（response.created / output_item /
//! output_text.delta / function_call_arguments / response.completed）。
//! codex 只从 `response.output_item.done` 构建回合内容，缓冲式合成足够。

use std::collections::{BTreeMap, HashMap};

use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Extension, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::handlers::post_messages;
use super::middleware::{AppState, KeyContext};
use super::openai::{
    ParsedResponse, collect_text_strings, now_ts, parse_anthropic_message, push_merged,
};
use super::types::{Message, MessagesRequest, OutputConfig, SystemMessage, Tool};

/// 读取内部响应体时的上限（64MB，与请求体上限对齐）
const MAX_INNER_BODY: usize = 64 * 1024 * 1024;

/// 未显式给出 max_output_tokens 时的默认输出上限
const DEFAULT_MAX_TOKENS: i32 = 32000;

/// 无 codex 工具时的严格提示（保持既有已验证的纯聊天/搜索行为）
const NUDGE_STRICT: &str = "You have a web_search tool that returns live results. For anything \
time-sensitive — current events, news, recent sports results, prices, releases, or facts \
that may be newer than your training data — call web_search before answering, and never \
claim something did not happen without searching first. Do not call any other tool.";

/// 有 codex 工具时的软化提示（其它工具照常使用）
const NUDGE_SOFT: &str = "You have a web_search tool that returns live results. For anything \
time-sensitive — current events, news, recent sports results, prices, releases, or facts \
that may be newer than your training data — call web_search before answering, and never \
claim something did not happen without searching first. Use your other tools normally for \
all other work.";

// ============================ 工具声明类型 ============================

/// Responses 客户端声明的工具类型。出方向的 item 种类必须与之一致。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DeclaredToolKind {
    /// `type:"function"` → 应答 `function_call`
    Function,
    /// `type:"custom"`（自由文本）→ 应答 `custom_tool_call`
    Custom,
}

/// 一个已声明工具的完整身份。codex 0.144 的工具可能挂在 `namespace`
/// 分组下（如 collaboration 子代理工具）：对 Anthropic 模型展平为
/// `ns__name`，应答时还原为 `name` + `namespace` 字段。
#[derive(Clone, Debug)]
struct DeclaredTool {
    kind: DeclaredToolKind,
    /// 原始工具名（不含 namespace 前缀）
    name: String,
    /// 所属 namespace（codex 的 ToolName::new(namespace, name) 需要）
    namespace: Option<String>,
}

/// 展平名（模型看到的名字）→ 声明信息（每请求独立，无全局状态）
type ToolKindMap = HashMap<String, DeclaredTool>;

/// 模型侧的展平工具名：namespace 用 `__` 连接（Anthropic 工具名不允许 `.`）
fn flat_tool_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => format!("{ns}__{name}"),
        _ => name.to_string(),
    }
}

// ============================ 请求类型 ============================

#[derive(Debug, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default)]
    pub instructions: Option<String>,
    /// 可以是字符串或 input item 数组
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_output_tokens: Option<i32>,
    /// codex 声明的工具（function / custom / web_search / ...），
    /// 会被翻译为 Anthropic 工具并转发给 Kiro 模型。
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default)]
    pub effort: Option<String>,
}

// ============================ Handler ============================

/// `POST /v1/responses`
pub async fn post_responses(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    Json(req): Json<ResponsesRequest>,
) -> Response {
    let want_stream = req.stream;
    let model = req.model.clone();

    tracing::info!(
        model = %model,
        stream = %want_stream,
        "Received POST /v1/responses request"
    );

    // 1. Responses -> Anthropic 请求翻译（同时得到工具声明类型表）
    let (anthropic_req, tool_kinds) = match responses_to_anthropic(req) {
        Ok(r) => r,
        Err(msg) => {
            return responses_error(StatusCode::BAD_REQUEST, "invalid_request_error", &msg);
        }
    };

    // 2. 复用 Anthropic 全链路（内部强制非流式）
    let inner = post_messages(State(state), Extension(key_ctx), Json(anthropic_req)).await;

    let status = inner.status();
    let body_bytes = match to_bytes(inner.into_body(), MAX_INNER_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return responses_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("failed to read upstream response: {e}"),
            );
        }
    };

    // 上游非 2xx：原样透传
    if !status.is_success() {
        return Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body_bytes))
            .unwrap();
    }

    let anthropic: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return responses_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("failed to parse upstream response: {e}"),
            );
        }
    };

    // 3. Anthropic -> Responses 响应翻译
    let parsed = parse_anthropic_message(&anthropic, &model);

    if want_stream {
        let sse = build_responses_sse(&parsed, &tool_kinds);
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(sse))
            .unwrap()
    } else {
        let body = build_responses_object(&parsed, &tool_kinds);
        (StatusCode::OK, Json(body)).into_response()
    }
}

// ============================ 请求翻译 ============================

fn responses_to_anthropic(
    req: ResponsesRequest,
) -> Result<(MessagesRequest, ToolKindMap), String> {
    let max_tokens = req
        .max_output_tokens
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS);

    let mut system: Vec<SystemMessage> = Vec::new();
    if let Some(instr) = req.instructions.as_ref() {
        if !instr.trim().is_empty() {
            system.push(SystemMessage {
                text: instr.clone(),
                cache_control: None,
            });
        }
    }

    let mut merged: Vec<(String, Vec<Value>)> = Vec::new();
    // codex 0.144 把工具声明放进 input 里的 `additional_tools` item
    //（顶层 `tools` 通常为空）；两处都收集，合并转换。
    let mut declared_entries: Vec<Value> = req.tools.clone().unwrap_or_default();

    match &req.input {
        // input 直接是字符串 → 单条 user 文本
        Value::String(s) => {
            if !s.is_empty() {
                push_merged(&mut merged, "user", vec![json!({"type":"text","text":s})]);
            }
        }
        Value::Array(items) => {
            for item in items {
                let item_type = item.get("type").and_then(|v| v.as_str());

                // additional_tools item：真正的工具声明清单（codex 0.144）。
                // developer 角色的 message item（AGENTS.md / user_instructions /
                // environment_context）会在 translate_input_item 里归入 system。
                if item_type == Some("additional_tools") {
                    if let Some(list) = item.get("tools").and_then(|v| v.as_array()) {
                        declared_entries.extend(list.iter().cloned());
                    }
                    continue;
                }

                translate_input_item(item, &mut system, &mut merged);
            }
        }
        _ => {}
    }

    let messages: Vec<Message> = merged
        .into_iter()
        .filter(|(_, blocks)| !blocks.is_empty())
        .map(|(role, blocks)| Message {
            role,
            content: Value::Array(blocks),
        })
        .collect();

    if messages.is_empty() {
        return Err("input must contain at least one user/assistant message".to_string());
    }

    // 翻译 codex 声明的工具，并记录每个工具的声明类型（出方向要用）。
    let mut tool_kinds: ToolKindMap = HashMap::new();
    let mut tool_list = convert_responses_tools(&declared_entries, &mut tool_kinds, None);

    if tool_list.is_empty() {
        // 无 codex 工具（纯聊天流）：保持既有已验证行为——noop 占位 +
        // 原生 web_search（占位保证 tool_count > 1，走 agentic loop 而非
        // 单工具 fast-path）+ 严格提示。
        tool_list = vec![
            Tool {
                tool_type: None,
                name: "noop".to_string(),
                description: "Placeholder tool; never call this.".to_string(),
                input_schema: Default::default(),
                max_uses: None,
                cache_control: None,
            },
            native_web_search_tool(),
        ];
        system.push(SystemMessage {
            text: NUDGE_STRICT.to_string(),
            cache_control: None,
        });
    } else {
        // 有 codex 工具：追加原生 web_search（除非客户端已声明同名工具，
        // 避免名字冲突破坏 tool_name 还原逻辑），并使用软化提示。
        // 注入后 has_web_search_among_tools 命中 → 走 agentic loop，
        // loop 会把非 web_search 的 client 工具 tool_use 原样透传。
        if !tool_list.iter().any(|t| t.name == "web_search") {
            tool_list.push(native_web_search_tool());
        }
        system.push(SystemMessage {
            text: NUDGE_SOFT.to_string(),
            cache_control: None,
        });
    }

    let custom_count = tool_kinds
        .values()
        .filter(|d| d.kind == DeclaredToolKind::Custom)
        .count();
    tracing::info!(
        tool_count = tool_list.len(),
        custom_count = custom_count,
        "responses: forwarding tools to upstream"
    );

    let tools = Some(tool_list);
    let tool_choice = req.tool_choice.as_ref().and_then(convert_tool_choice);
    let output_config = req
        .reasoning
        .and_then(|r| r.effort)
        .filter(|e| !e.trim().is_empty())
        .map(|effort| OutputConfig { effort });

    Ok((
        MessagesRequest {
            model: req.model,
            max_tokens,
            messages,
            stream: false,
            system: if system.is_empty() { None } else { Some(system) },
            tools,
            tool_choice,
            thinking: None,
            output_config,
            metadata: None,
        },
        tool_kinds,
    ))
}

/// 原生 web_search 工具（kiro-rs 内部代答，最多 5 轮）
fn native_web_search_tool() -> Tool {
    Tool {
        tool_type: Some("web_search_20250305".to_string()),
        name: "web_search".to_string(),
        description: String::new(),
        input_schema: Default::default(),
        max_uses: Some(5),
        cache_control: None,
    }
}

/// 把 Responses `tools` 数组（顶层 + additional_tools item）翻译成
/// Anthropic 工具，并登记声明信息。
///
/// - `function`：JSON schema 原样映射（converter 内部处理 >63 字符的
///   名字缩短与还原，这里保持展平名）。
/// - `custom`（自由文本，如 code-mode 的 exec）：包一层
///   `{"input": <string>}` 单字段 schema，grammar/format 追加到
///   description 里提示模型输入格式。
/// - `namespace`：分组容器（如 collaboration 子代理工具），递归展开，
///   模型侧展平为 `ns__name`，应答时还原 namespace 字段。
/// - `web_search`：跳过（原生注入统一代答）。
/// - 其它类型（local_shell / tool_search ...）：警告并跳过。
fn convert_responses_tools(
    entries: &[Value],
    kinds: &mut ToolKindMap,
    namespace: Option<&str>,
) -> Vec<Tool> {
    let mut out = Vec::new();
    for entry in entries {
        let ty = entry
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("function");
        match ty {
            "function" => {
                // codex 用扁平结构 {type,name,description,strict,parameters}；
                // 兼容 chat-completions 的嵌套结构 {type,function:{...}}。
                let func = entry.get("function").unwrap_or(entry);
                let name = func
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let description = func
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut input_schema: BTreeMap<String, Value> = BTreeMap::new();
                if let Some(Value::Object(params)) = func.get("parameters") {
                    for (k, v) in params {
                        input_schema.insert(k.clone(), v.clone());
                    }
                }
                let flat = flat_tool_name(namespace, &name);
                kinds.insert(
                    flat.clone(),
                    DeclaredTool {
                        kind: DeclaredToolKind::Function,
                        name,
                        namespace: namespace.map(str::to_string),
                    },
                );
                out.push(Tool {
                    tool_type: None,
                    name: flat,
                    description,
                    input_schema,
                    max_uses: None,
                    cache_control: None,
                });
            }
            "custom" => {
                let name = entry
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let mut description = entry
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // 自由文本工具的语法（如 apply_patch 的 lark grammar）
                // 附到描述里，让模型知道 input 字符串该长什么样。
                if let Some(format) = entry.get("format") {
                    let syntax = format
                        .get("syntax")
                        .and_then(|v| v.as_str())
                        .unwrap_or("grammar");
                    if let Some(def) = format.get("definition").and_then(|v| v.as_str()) {
                        description
                            .push_str(&format!("\n\nInput format ({syntax} grammar):\n{def}"));
                    }
                }
                let mut input_schema: BTreeMap<String, Value> = BTreeMap::new();
                input_schema.insert("type".to_string(), json!("object"));
                input_schema.insert(
                    "properties".to_string(),
                    json!({
                        "input": {
                            "type": "string",
                            "description": "The complete raw tool input text. Do NOT wrap it in JSON or escape it.",
                        }
                    }),
                );
                input_schema.insert("required".to_string(), json!(["input"]));
                input_schema.insert("additionalProperties".to_string(), json!(false));
                let flat = flat_tool_name(namespace, &name);
                kinds.insert(
                    flat.clone(),
                    DeclaredTool {
                        kind: DeclaredToolKind::Custom,
                        name,
                        namespace: namespace.map(str::to_string),
                    },
                );
                out.push(Tool {
                    tool_type: None,
                    name: flat,
                    description,
                    input_schema,
                    max_uses: None,
                    cache_control: None,
                });
            }
            "namespace" => {
                let ns = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if ns.is_empty() || namespace.is_some() {
                    // 嵌套 namespace 未见于协议，保守跳过
                    tracing::warn!("responses: skipping empty/nested namespace tool group");
                    continue;
                }
                if let Some(nested) = entry.get("tools").and_then(|v| v.as_array()) {
                    let ns_desc = entry
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let mut converted = convert_responses_tools(nested, kinds, Some(ns));
                    // 分组描述附到每个成员前，保留上下文
                    if !ns_desc.is_empty() {
                        for t in &mut converted {
                            t.description = format!("[{ns}] {ns_desc}\n{}", t.description);
                        }
                    }
                    out.extend(converted);
                }
            }
            // 托管 web_search 声明：原生注入统一代答，无需单独转发。
            "web_search" => {}
            other => {
                tracing::warn!(tool_type = %other, "responses: skipping unsupported tool type");
            }
        }
    }
    out
}

/// 翻译单个 Responses input item 到 Anthropic 结构
fn translate_input_item(
    item: &Value,
    system: &mut Vec<SystemMessage>,
    merged: &mut Vec<(String, Vec<Value>)>,
) {
    let ty = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match ty {
        // 助手发起的工具调用（function 类型）。namespace 工具还原为展平名，
        // 与进方向声明及模型产出保持一致。
        "function_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let namespace = item.get("namespace").and_then(|v| v.as_str());
            let args_str = item
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
            let block = json!({
                "type": "tool_use",
                "id": call_id,
                "name": flat_tool_name(namespace, &name),
                "input": input,
            });
            push_merged(merged, "assistant", vec![block]);
        }
        // 助手发起的自由文本工具调用（custom 类型）：回放时按进方向的
        // 包装 schema 复原为 {"input": <string>}，保证与模型当初产出的
        // tool_use 逐字一致（Kiro/Bedrock 校验 tool_use/tool_result 配对）。
        "custom_tool_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let namespace = item.get("namespace").and_then(|v| v.as_str());
            let input_str = item.get("input").and_then(|v| v.as_str()).unwrap_or("");
            let block = json!({
                "type": "tool_use",
                "id": call_id,
                "name": flat_tool_name(namespace, &name),
                "input": { "input": input_str },
            });
            push_merged(merged, "assistant", vec![block]);
        }
        // 工具执行结果 → Anthropic 里属于 user 轮（function 与 custom 同构）
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = stringify_output(item.get("output"));
            let block = json!({
                "type": "tool_result",
                "tool_use_id": call_id,
                "content": content,
            });
            push_merged(merged, "user", vec![block]);
        }
        // 推理项 / 已完成的搜索展示项 / 压缩项：对 Anthropic 请求无意义，忽略
        "reasoning" | "web_search_call" | "compaction" => {}
        // "message" 或未标注 type 但带 role 的项
        _ => {
            let role = item.get("role").and_then(|v| v.as_str());
            let Some(role) = role else {
                return;
            };
            match role {
                "system" | "developer" => {
                    for text in collect_content_text(item.get("content")) {
                        system.push(SystemMessage {
                            text,
                            cache_control: None,
                        });
                    }
                }
                "user" | "assistant" => {
                    let blocks = content_blocks(item.get("content"));
                    push_merged(merged, role, blocks);
                }
                _ => {}
            }
        }
    }
}

/// 把 Responses message.content（字符串或数组）转成 Anthropic content blocks
fn content_blocks(content: Option<&Value>) -> Vec<Value> {
    let mut out = Vec::new();
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                out.push(json!({"type":"text","text":s}));
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                let ty = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ty {
                    "input_text" | "output_text" | "text" => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                out.push(json!({"type":"text","text":t}));
                            }
                        }
                    }
                    "input_image" => {
                        // Responses: image_url 是字符串（可能是 data: URL）
                        if let Some(block) = image_block(part) {
                            out.push(block);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// 仅收集纯文本（system 内容用）
fn collect_content_text(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        Some(Value::Array(_)) => collect_text_strings(content),
        _ => Vec::new(),
    }
}

/// Responses input_image（仅支持 data: URL）转 Anthropic image block
fn image_block(part: &Value) -> Option<Value> {
    let url = part.get("image_url").and_then(|v| v.as_str())?;
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some(json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media_type,
            "data": data,
        }
    }))
}

/// function_call_output.output 归一化为字符串
fn stringify_output(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(_)) => collect_text_strings(output).join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn convert_tool_choice(tc: &Value) -> Option<Value> {
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({"type":"auto"})),
            "required" => Some(json!({"type":"any"})),
            "none" => None,
            _ => Some(json!({"type":"auto"})),
        },
        Value::Object(_) => {
            // Responses: {"type":"function","name":"..."}
            let name = tc
                .get("name")
                .or_else(|| tc.get("function").and_then(|f| f.get("name")))
                .and_then(|v| v.as_str());
            name.map(|n| json!({"type":"tool","name":n}))
        }
        _ => None,
    }
}

// ============================ 响应翻译 ============================

fn new_resp_id() -> String {
    format!("resp_{}", Uuid::new_v4().to_string().replace('-', ""))
}
fn new_msg_id() -> String {
    format!("msg_{}", Uuid::new_v4().to_string().replace('-', ""))
}
fn new_fc_id() -> String {
    format!("fc_{}", Uuid::new_v4().to_string().replace('-', ""))
}
fn new_ctc_id() -> String {
    format!("ctc_{}", Uuid::new_v4().to_string().replace('-', ""))
}
fn new_rs_id() -> String {
    format!("rs_{}", Uuid::new_v4().to_string().replace('-', ""))
}

/// 从自由文本工具的 arguments JSON 里解出原始 input 字符串。
///
/// 回退链：`{"input": <string>}` → 单字段字符串对象 → 原样返回 arguments。
/// 模型偶尔不守 schema 时也能兜住（codex 端还有自己的解析重试兜底）。
fn custom_input_text(arguments_json: &str) -> String {
    let parsed: Value = match serde_json::from_str(arguments_json) {
        Ok(v) => v,
        Err(_) => return arguments_json.to_string(),
    };
    match parsed {
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get("input") {
                return s.clone();
            }
            if map.len() == 1 {
                if let Some(Value::String(s)) = map.values().next() {
                    return s.clone();
                }
            }
            arguments_json.to_string()
        }
        Value::String(s) => s,
        _ => arguments_json.to_string(),
    }
}

/// 从 ParsedResponse 计算 (status, output items, usage)
struct ResponsesView {
    status: String,
    output: Vec<Value>,
    usage: Value,
}

fn build_view(p: &ParsedResponse, kinds: &ToolKindMap) -> ResponsesView {
    let status = if p.finish_reason == "length" {
        "incomplete".to_string()
    } else {
        "completed".to_string()
    };

    let mut output = Vec::new();

    // 推理摘要放最前（思考先于可见输出发生）
    if !p.thinking.is_empty() {
        output.push(json!({
            "type": "reasoning",
            "id": new_rs_id(),
            "summary": [{ "type": "summary_text", "text": p.thinking }],
        }));
    }

    // 内部代答的 web_search 以 web_search_call 展示（codex 渲染 "Searched the web"）
    for (id, query) in &p.web_searches {
        output.push(json!({
            "type": "web_search_call",
            "id": id,
            "status": "completed",
            "action": { "type": "search", "query": query },
        }));
    }

    if !p.text.is_empty() {
        output.push(json!({
            "type": "message",
            "id": new_msg_id(),
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": p.text,
                "annotations": [],
            }],
        }));
    }

    for tc in &p.tool_calls {
        let call_id = tc
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let func = tc.get("function");
        let flat_name = func
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let arguments = func
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str())
            .unwrap_or("{}")
            .to_string();
        let decl = kinds.get(flat_name.as_str());
        // 展平名还原：namespace 工具应答时用原名 + namespace 字段
        let (name, namespace) = match decl {
            Some(d) => (d.name.clone(), d.namespace.clone()),
            None => (flat_name.clone(), None),
        };
        match decl.map(|d| d.kind) {
            // custom 声明 → custom_tool_call（原始字符串 input），
            // 否则 codex 校验 payload 种类失败（"incompatible payload"）。
            Some(DeclaredToolKind::Custom) => {
                let mut item = json!({
                    "type": "custom_tool_call",
                    "id": new_ctc_id(),
                    "call_id": call_id,
                    "name": name,
                    "input": custom_input_text(&arguments),
                    "status": "completed",
                });
                if let Some(ns) = namespace {
                    item["namespace"] = json!(ns);
                }
                output.push(item);
            }
            // function 声明或未声明（模型幻觉出的名字走最兼容的 function_call）
            _ => {
                let mut item = json!({
                    "type": "function_call",
                    "id": new_fc_id(),
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                    "status": "completed",
                });
                if let Some(ns) = namespace {
                    item["namespace"] = json!(ns);
                }
                output.push(item);
            }
        }
    }

    let mut usage = json!({
        "input_tokens": p.prompt_tokens,
        "input_tokens_details": { "cached_tokens": 0 },
        "output_tokens": p.completion_tokens,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": p.prompt_tokens + p.completion_tokens,
    });
    // 透传上游 meteringEvent 写入的 credit_* 字段（仅在拿到 meteringEvent 时）。
    if let Some(credit_usage) = p.credit_usage {
        usage["credit_usage"] = json!(credit_usage);
    }
    if let Some(credit_unit) = &p.credit_unit {
        usage["credit_unit"] = json!(credit_unit);
    }
    if let Some(credit_unit_plural) = &p.credit_unit_plural {
        usage["credit_unit_plural"] = json!(credit_unit_plural);
    }

    ResponsesView {
        status,
        output,
        usage,
    }
}

fn build_response_object_from(p: &ParsedResponse, view: &ResponsesView, id: &str) -> Value {
    let mut obj = json!({
        "id": id,
        "object": "response",
        "created_at": now_ts(),
        "status": view.status,
        "model": p.model,
        "output": view.output,
        "usage": view.usage,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
    });
    if view.status == "incomplete" {
        obj["incomplete_details"] = json!({ "reason": "max_output_tokens" });
    }
    obj
}

fn build_responses_object(p: &ParsedResponse, kinds: &ToolKindMap) -> Value {
    let view = build_view(p, kinds);
    let id = new_resp_id();
    build_response_object_from(p, &view, &id)
}

/// 把完整结果合成为 Responses SSE 事件序列
///
/// codex 只从 `response.output_item.done` 构建回合内容（`.added` 用于进度
/// 展示，`response.completed` 只取 id/usage），所以每个 item 只要保证
/// added/done 成对且 done 携带完整内容即可。delta 事件是锦上添花。
fn build_responses_sse(p: &ParsedResponse, kinds: &ToolKindMap) -> String {
    let view = build_view(p, kinds);
    let resp_id = new_resp_id();
    let mut out = String::new();
    let mut seq: i64 = 0;

    let mut emit = |ty: &str, mut payload: Value, seq: &mut i64| {
        payload["type"] = json!(ty);
        payload["sequence_number"] = json!(*seq);
        *seq += 1;
        out.push_str("event: ");
        out.push_str(ty);
        out.push_str("\ndata: ");
        out.push_str(&payload.to_string());
        out.push_str("\n\n");
    };

    // response.created + in_progress
    let created_response = json!({
        "id": resp_id,
        "object": "response",
        "created_at": now_ts(),
        "status": "in_progress",
        "model": p.model,
        "output": [],
    });
    emit(
        "response.created",
        json!({ "response": created_response.clone() }),
        &mut seq,
    );
    emit(
        "response.in_progress",
        json!({ "response": created_response }),
        &mut seq,
    );

    for (idx, item) in view.output.iter().enumerate() {
        let output_index = idx as i64;
        let item_ty = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let item_id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mid = item_id.as_str();

        match item_ty {
            "message" => {
                let text = item
                    .pointer("/content/0/text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                emit(
                    "response.output_item.added",
                    json!({ "output_index": output_index, "item": {
                        "type": "message", "id": mid, "status": "in_progress",
                        "role": "assistant", "content": [],
                    }}),
                    &mut seq,
                );
                emit(
                    "response.content_part.added",
                    json!({
                        "item_id": mid,
                        "output_index": output_index,
                        "content_index": 0,
                        "part": { "type": "output_text", "text": "", "annotations": [] },
                    }),
                    &mut seq,
                );
                emit(
                    "response.output_text.delta",
                    json!({
                        "item_id": mid,
                        "output_index": output_index,
                        "content_index": 0,
                        "delta": text,
                    }),
                    &mut seq,
                );
                emit(
                    "response.output_text.done",
                    json!({
                        "item_id": mid,
                        "output_index": output_index,
                        "content_index": 0,
                        "text": text,
                    }),
                    &mut seq,
                );
                emit(
                    "response.content_part.done",
                    json!({
                        "item_id": mid,
                        "output_index": output_index,
                        "content_index": 0,
                        "part": { "type": "output_text", "text": text, "annotations": [] },
                    }),
                    &mut seq,
                );
            }
            "function_call" => {
                let arguments = item
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}")
                    .to_string();
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["arguments"] = json!("");
                emit(
                    "response.output_item.added",
                    json!({ "output_index": output_index, "item": added }),
                    &mut seq,
                );
                emit(
                    "response.function_call_arguments.delta",
                    json!({
                        "item_id": mid,
                        "output_index": output_index,
                        "delta": arguments.as_str(),
                    }),
                    &mut seq,
                );
                emit(
                    "response.function_call_arguments.done",
                    json!({
                        "item_id": mid,
                        "output_index": output_index,
                        "arguments": arguments.as_str(),
                    }),
                    &mut seq,
                );
            }
            "custom_tool_call" => {
                // added 必须带完整 input（codex 的 CustomToolCall 反序列化
                // 要求 input 字段存在；缓冲式合成没有渐进展示的意义）。
                let input = item
                    .get("input")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                emit(
                    "response.output_item.added",
                    json!({ "output_index": output_index, "item": added }),
                    &mut seq,
                );
                emit(
                    "response.custom_tool_call_input.delta",
                    json!({
                        "item_id": mid,
                        "output_index": output_index,
                        "delta": input.as_str(),
                    }),
                    &mut seq,
                );
                emit(
                    "response.custom_tool_call_input.done",
                    json!({
                        "item_id": mid,
                        "output_index": output_index,
                        "input": input.as_str(),
                    }),
                    &mut seq,
                );
            }
            "reasoning" => {
                let text = item
                    .pointer("/summary/0/text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                emit(
                    "response.output_item.added",
                    json!({ "output_index": output_index, "item": {
                        "type": "reasoning", "id": mid, "summary": [],
                    }}),
                    &mut seq,
                );
                emit(
                    "response.reasoning_summary_text.delta",
                    json!({
                        "item_id": mid,
                        "output_index": output_index,
                        "summary_index": 0,
                        "delta": text,
                    }),
                    &mut seq,
                );
                emit(
                    "response.reasoning_summary_text.done",
                    json!({
                        "item_id": mid,
                        "output_index": output_index,
                        "summary_index": 0,
                        "text": text,
                    }),
                    &mut seq,
                );
            }
            _ => {
                // web_search_call 等：added(in_progress) + done(完整) 即可
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                emit(
                    "response.output_item.added",
                    json!({ "output_index": output_index, "item": added }),
                    &mut seq,
                );
            }
        }

        // 统一收尾：done 携带完整 item
        emit(
            "response.output_item.done",
            json!({ "output_index": output_index, "item": item.clone() }),
            &mut seq,
        );
    }

    // response.completed（完整对象含 usage）
    let final_obj = build_response_object_from(p, &view, &resp_id);
    let completed_event = if view.status == "incomplete" {
        "response.incomplete"
    } else {
        "response.completed"
    };
    emit(completed_event, json!({ "response": final_obj }), &mut seq);

    out
}

fn responses_error(status: StatusCode, err_type: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "message": message,
            "type": err_type,
        }
    });
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 测试辅助 ----

    fn req_with(tools: Value, input: Value) -> ResponsesRequest {
        serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "input": input,
            "tools": tools,
        }))
        .unwrap()
    }

    fn simple_input() -> Value {
        json!([{ "type": "message", "role": "user", "content": "hi" }])
    }

    fn parsed_with_tool_calls(tool_calls: Vec<Value>) -> ParsedResponse {
        ParsedResponse {
            model: "gpt-5.6-sol".to_string(),
            text: String::new(),
            tool_calls,
            finish_reason: "tool_calls".to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            thinking: String::new(),
            web_searches: Vec::new(),
            credit_usage: None,
            credit_unit: None,
            credit_unit_plural: None,
        }
    }

    fn kinds_of(pairs: &[(&str, DeclaredToolKind)]) -> ToolKindMap {
        pairs
            .iter()
            .map(|(n, k)| {
                (
                    n.to_string(),
                    DeclaredTool {
                        kind: *k,
                        name: n.to_string(),
                        namespace: None,
                    },
                )
            })
            .collect()
    }

    fn system_texts(req: &MessagesRequest) -> Vec<String> {
        req.system
            .as_ref()
            .map(|s| s.iter().map(|m| m.text.clone()).collect())
            .unwrap_or_default()
    }

    // ---- 请求方向：工具声明转换 ----

    #[test]
    fn additional_tools_item_declares_tools() {
        // codex 0.144：顶层 tools 为空，声明在 input 的 additional_tools item 里
        let req = req_with(
            json!([]),
            json!([
                { "type": "additional_tools", "role": "developer", "tools": [
                    { "type": "custom", "name": "exec", "description": "Run JS" },
                    { "type": "function", "name": "wait", "parameters": { "type": "object" } },
                ]},
                { "type": "message", "role": "user", "content": "hi" },
            ]),
        );
        let (anth, kinds) = responses_to_anthropic(req).unwrap();
        assert_eq!(kinds.get("exec").map(|d| d.kind), Some(DeclaredToolKind::Custom));
        assert_eq!(kinds.get("wait").map(|d| d.kind), Some(DeclaredToolKind::Function));
        let tools = anth.tools.as_ref().unwrap();
        assert!(tools.iter().any(|t| t.name == "exec"));
        assert!(tools.iter().any(|t| t.name == "wait"));
        assert!(tools.iter().any(|t| t.name == "web_search"));
        assert!(!tools.iter().any(|t| t.name == "noop"));
        // additional_tools item 本身不进消息
        assert_eq!(anth.messages.len(), 1);
    }

    #[test]
    fn namespace_tools_flattened_and_restored() {
        let req = req_with(
            json!([]),
            json!([
                { "type": "additional_tools", "role": "developer", "tools": [
                    { "type": "namespace", "name": "collaboration", "description": "Sub-agents.",
                      "tools": [
                          { "type": "function", "name": "spawn_agent", "parameters": { "type": "object" } },
                      ]},
                ]},
                { "type": "message", "role": "user", "content": "hi" },
            ]),
        );
        let (anth, kinds) = responses_to_anthropic(req).unwrap();
        let decl = kinds.get("collaboration__spawn_agent").expect("flattened name");
        assert_eq!(decl.kind, DeclaredToolKind::Function);
        assert_eq!(decl.name, "spawn_agent");
        assert_eq!(decl.namespace.as_deref(), Some("collaboration"));
        let tools = anth.tools.as_ref().unwrap();
        let t = tools
            .iter()
            .find(|t| t.name == "collaboration__spawn_agent")
            .expect("flattened tool declared to the model");
        assert!(t.description.contains("Sub-agents."));

        // 应答方向：展平名 → 原名 + namespace 字段
        let p = parsed_with_tool_calls(vec![json!({
            "id": "toolu_9", "type": "function",
            "function": { "name": "collaboration__spawn_agent", "arguments": "{}" },
        })]);
        let view = build_view(&p, &kinds);
        let fc = view
            .output
            .iter()
            .find(|i| i["type"] == "function_call")
            .unwrap();
        assert_eq!(fc["name"], "spawn_agent");
        assert_eq!(fc["namespace"], "collaboration");
        assert_eq!(fc["call_id"], "toolu_9");
    }

    #[test]
    fn namespaced_function_call_replay_uses_flat_name() {
        let req = req_with(
            json!([]),
            json!([
                { "type": "message", "role": "user", "content": "go" },
                { "type": "function_call", "call_id": "c9", "name": "spawn_agent",
                  "namespace": "collaboration", "arguments": "{\"task\":\"x\"}" },
                { "type": "function_call_output", "call_id": "c9", "output": "spawned" },
            ]),
        );
        let (anth, _) = responses_to_anthropic(req).unwrap();
        let tu = &anth.messages[1].content.as_array().unwrap()[0];
        assert_eq!(tu["type"], "tool_use");
        assert_eq!(
            tu["name"], "collaboration__spawn_agent",
            "replayed namespaced call must use the flat name the model was declared"
        );
    }

    #[test]
    fn custom_tool_declared_maps_to_wrapper_and_kind() {
        let req = req_with(
            json!([{
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch.",
                "format": { "type": "grammar", "syntax": "lark", "definition": "start: PATCH" },
            }]),
            simple_input(),
        );
        let (anth, kinds) = responses_to_anthropic(req).unwrap();
        assert_eq!(
            kinds.get("apply_patch").map(|d| d.kind),
            Some(DeclaredToolKind::Custom)
        );

        let tools = anth.tools.unwrap();
        let ap = tools.iter().find(|t| t.name == "apply_patch").unwrap();
        // 包装 schema：单个 input 字符串字段
        let props = ap.input_schema.get("properties").unwrap();
        assert!(props.get("input").is_some(), "wrapper input field required");
        assert_eq!(ap.input_schema.get("required").unwrap(), &json!(["input"]));
        // grammar 附加到描述
        assert!(ap.description.contains("Apply a patch."));
        assert!(ap.description.contains("lark grammar"));
        assert!(ap.description.contains("start: PATCH"));
        // 原生 web_search 注入，noop 不再存在
        assert!(tools.iter().any(|t| t.name == "web_search"
            && t.tool_type.as_deref() == Some("web_search_20250305")));
        assert!(!tools.iter().any(|t| t.name == "noop"));
    }

    #[test]
    fn function_tool_maps_schema_verbatim() {
        let req = req_with(
            json!([{
                "type": "function",
                "name": "shell",
                "description": "Run a command.",
                "parameters": {
                    "type": "object",
                    "properties": { "command": { "type": "array", "items": { "type": "string" } } },
                    "required": ["command"],
                },
            }]),
            simple_input(),
        );
        let (anth, kinds) = responses_to_anthropic(req).unwrap();
        assert_eq!(
            kinds.get("shell").map(|d| d.kind),
            Some(DeclaredToolKind::Function)
        );
        let tools = anth.tools.unwrap();
        let shell = tools.iter().find(|t| t.name == "shell").unwrap();
        assert_eq!(shell.input_schema.get("type").unwrap(), &json!("object"));
        assert!(
            shell
                .input_schema
                .get("properties")
                .unwrap()
                .get("command")
                .is_some()
        );
    }

    #[test]
    fn noop_fallback_when_no_codex_tools() {
        let req = req_with(json!([]), simple_input());
        let (anth, kinds) = responses_to_anthropic(req).unwrap();
        assert!(kinds.is_empty());
        let tools = anth.tools.as_ref().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["noop", "web_search"]);
        // 严格提示保留
        let sys = system_texts(&anth);
        assert!(
            sys.iter()
                .any(|t| t.contains("Do not call any other tool")),
            "strict nudge must be kept for tool-less flows"
        );
    }

    #[test]
    fn nudge_softened_when_codex_tools_present() {
        let req = req_with(
            json!([{ "type": "function", "name": "shell", "parameters": {} }]),
            simple_input(),
        );
        let (anth, _) = responses_to_anthropic(req).unwrap();
        let sys = system_texts(&anth);
        assert!(
            !sys.iter().any(|t| t.contains("Do not call any other tool")),
            "strict nudge must be removed when codex tools are forwarded"
        );
        assert!(
            sys.iter()
                .any(|t| t.contains("Use your other tools normally")),
            "soft nudge must be present"
        );
    }

    #[test]
    fn web_search_name_collision_skips_native_injection() {
        let req = req_with(
            json!([{ "type": "function", "name": "web_search", "parameters": {} }]),
            simple_input(),
        );
        let (anth, kinds) = responses_to_anthropic(req).unwrap();
        assert_eq!(
            kinds.get("web_search").map(|d| d.kind),
            Some(DeclaredToolKind::Function)
        );
        let tools = anth.tools.unwrap();
        let ws: Vec<&Tool> = tools.iter().filter(|t| t.name == "web_search").collect();
        assert_eq!(ws.len(), 1, "exactly one web_search tool");
        assert!(
            ws[0].tool_type.is_none(),
            "the client's function tool wins; no native server tool injected"
        );
    }

    #[test]
    fn hosted_web_search_declaration_covered_by_native() {
        let req = req_with(
            json!([
                { "type": "web_search" },
                { "type": "function", "name": "shell", "parameters": {} },
            ]),
            simple_input(),
        );
        let (anth, _) = responses_to_anthropic(req).unwrap();
        let tools = anth.tools.unwrap();
        let ws: Vec<&Tool> = tools.iter().filter(|t| t.name == "web_search").collect();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].tool_type.as_deref(), Some("web_search_20250305"));
    }

    // ---- 请求方向：input item 回放 ----

    #[test]
    fn custom_tool_call_round_trip_replay() {
        let req = req_with(
            json!([{ "type": "custom", "name": "apply_patch" }]),
            json!([
                { "type": "message", "role": "user", "content": "patch it" },
                { "type": "custom_tool_call", "call_id": "c1", "name": "apply_patch",
                  "input": "*** Begin Patch\nRAW\n*** End Patch" },
                { "type": "custom_tool_call_output", "call_id": "c1", "output": "Done!" },
            ]),
        );
        let (anth, _) = responses_to_anthropic(req).unwrap();
        assert_eq!(anth.messages.len(), 3);

        let assistant = &anth.messages[1];
        assert_eq!(assistant.role, "assistant");
        let tu = &assistant.content.as_array().unwrap()[0];
        assert_eq!(tu["type"], "tool_use");
        assert_eq!(tu["id"], "c1");
        assert_eq!(tu["name"], "apply_patch");
        // 与进方向包装 schema 逐字一致
        assert_eq!(tu["input"]["input"], "*** Begin Patch\nRAW\n*** End Patch");

        let user = &anth.messages[2];
        assert_eq!(user.role, "user");
        let tr = &user.content.as_array().unwrap()[0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["tool_use_id"], "c1");
        assert_eq!(tr["content"], "Done!");
    }

    #[test]
    fn function_call_output_array_shape_stringified() {
        let req = req_with(
            json!([{ "type": "function", "name": "shell", "parameters": {} }]),
            json!([
                { "type": "message", "role": "user", "content": "run" },
                { "type": "function_call", "call_id": "f1", "name": "shell",
                  "arguments": "{\"command\":[\"ls\"]}" },
                { "type": "function_call_output", "call_id": "f1",
                  "output": [
                      { "type": "output_text", "text": "line1" },
                      { "type": "output_text", "text": "line2" },
                  ] },
            ]),
        );
        let (anth, _) = responses_to_anthropic(req).unwrap();
        let tr = &anth.messages[2].content.as_array().unwrap()[0];
        assert_eq!(tr["content"], "line1\nline2");
    }

    #[test]
    fn developer_items_become_system() {
        let req = req_with(
            json!([]),
            json!([
                { "type": "message", "role": "developer",
                  "content": [{ "type": "input_text", "text": "AGENTS.md rules here" }] },
                { "type": "message", "role": "user", "content": "hi" },
            ]),
        );
        let (anth, _) = responses_to_anthropic(req).unwrap();
        let sys = system_texts(&anth);
        assert!(
            sys.iter().any(|t| t.contains("AGENTS.md rules here")),
            "developer items must reach the model as system text. sys={sys:?}"
        );
        // 且不出现在 messages 里
        assert_eq!(anth.messages.len(), 1);
        assert_eq!(anth.messages[0].role, "user");
    }

    #[test]
    fn reasoning_and_presentation_items_skipped_on_replay() {
        let req = req_with(
            json!([]),
            json!([
                { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "hmm" }] },
                { "type": "web_search_call", "status": "completed" },
                { "type": "compaction", "encrypted_content": "xxx" },
                { "type": "message", "role": "user", "content": "hi" },
            ]),
        );
        let (anth, _) = responses_to_anthropic(req).unwrap();
        assert_eq!(anth.messages.len(), 1);
        assert_eq!(anth.messages[0].role, "user");
    }

    // ---- custom input 解包回退链 ----

    #[test]
    fn custom_input_unwrap_fallbacks() {
        // 标准：{"input": "..."}
        assert_eq!(custom_input_text(r#"{"input":"*** Begin Patch"}"#), "*** Begin Patch");
        // 单字段字符串对象
        assert_eq!(custom_input_text(r#"{"cmd":"echo hi"}"#), "echo hi");
        // 多字段对象 → 原样
        assert_eq!(
            custom_input_text(r#"{"a":1,"b":"c"}"#),
            r#"{"a":1,"b":"c"}"#
        );
        // 非 JSON → 原样
        assert_eq!(custom_input_text("raw text"), "raw text");
        // JSON 字符串 → 解出
        assert_eq!(custom_input_text(r#""just a string""#), "just a string");
    }

    // ---- 响应方向：build_view ----

    #[test]
    fn build_view_emits_custom_tool_call_for_custom_kind() {
        let kinds = kinds_of(&[("apply_patch", DeclaredToolKind::Custom)]);
        let p = parsed_with_tool_calls(vec![json!({
            "id": "toolu_1",
            "type": "function",
            "function": { "name": "apply_patch", "arguments": "{\"input\":\"*** Begin Patch\"}" },
        })]);
        let view = build_view(&p, &kinds);
        let item = view
            .output
            .iter()
            .find(|i| i["type"] == "custom_tool_call")
            .expect("custom_tool_call item must be emitted");
        assert_eq!(item["call_id"], "toolu_1");
        assert_eq!(item["name"], "apply_patch");
        assert_eq!(item["input"], "*** Begin Patch");
        assert!(
            !view.output.iter().any(|i| i["type"] == "function_call"),
            "must not also emit a function_call for the same tool"
        );
    }

    #[test]
    fn build_view_emits_function_call_for_function_and_unknown_kinds() {
        let kinds = kinds_of(&[("shell", DeclaredToolKind::Function)]);
        let p = parsed_with_tool_calls(vec![
            json!({
                "id": "toolu_1", "type": "function",
                "function": { "name": "shell", "arguments": "{\"command\":[\"ls\"]}" },
            }),
            json!({
                "id": "toolu_2", "type": "function",
                "function": { "name": "hallucinated_tool", "arguments": "{}" },
            }),
        ]);
        let view = build_view(&p, &kinds);
        let fcs: Vec<&Value> = view
            .output
            .iter()
            .filter(|i| i["type"] == "function_call")
            .collect();
        assert_eq!(fcs.len(), 2, "function + unknown both map to function_call");
        assert_eq!(fcs[0]["name"], "shell");
        assert_eq!(fcs[0]["arguments"], "{\"command\":[\"ls\"]}");
        assert_eq!(fcs[0]["call_id"], "toolu_1");
    }

    #[test]
    fn build_view_orders_reasoning_search_message_tools() {
        let kinds = kinds_of(&[("shell", DeclaredToolKind::Function)]);
        let mut p = parsed_with_tool_calls(vec![json!({
            "id": "toolu_1", "type": "function",
            "function": { "name": "shell", "arguments": "{}" },
        })]);
        p.text = "answer".to_string();
        p.thinking = "let me think".to_string();
        p.web_searches = vec![("srvtoolu_1".to_string(), "rust news".to_string())];
        let view = build_view(&p, &kinds);
        let types: Vec<&str> = view
            .output
            .iter()
            .map(|i| i["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            vec!["reasoning", "web_search_call", "message", "function_call"]
        );
        assert_eq!(
            view.output[0]["summary"][0]["text"], "let me think",
            "reasoning summary carries the thinking text"
        );
        assert_eq!(view.output[1]["action"]["query"], "rust news");
    }

    // ---- 响应方向：SSE ----

    #[test]
    fn sse_contains_custom_item_events_with_full_input() {
        let kinds = kinds_of(&[("apply_patch", DeclaredToolKind::Custom)]);
        let p = parsed_with_tool_calls(vec![json!({
            "id": "toolu_1", "type": "function",
            "function": { "name": "apply_patch", "arguments": "{\"input\":\"PATCH BODY\"}" },
        })]);
        let sse = build_responses_sse(&p, &kinds);
        assert!(sse.contains("event: response.output_item.added"));
        assert!(sse.contains("event: response.output_item.done"));
        assert!(sse.contains("event: response.custom_tool_call_input.delta"));
        assert!(sse.contains("event: response.custom_tool_call_input.done"));
        assert!(sse.contains("\"custom_tool_call\""));
        assert!(sse.contains("PATCH BODY"));
        assert!(sse.contains("event: response.completed"));
        let delta_pos = sse
            .find("event: response.custom_tool_call_input.delta")
            .unwrap();
        let input_done_pos = sse
            .find("event: response.custom_tool_call_input.done")
            .unwrap();
        let item_done_pos = sse.find("event: response.output_item.done").unwrap();
        assert!(
            delta_pos < input_done_pos && input_done_pos < item_done_pos,
            "custom input must finish before the output item is marked done"
        );
        let input_done_line = sse
            .lines()
            .find(|l| {
                l.starts_with("data: ")
                    && l.contains("response.custom_tool_call_input.done")
            })
            .expect("custom input done event data line");
        assert!(input_done_line.contains("\"input\":\"PATCH BODY\""));
        // added 也必须带完整 input（codex 反序列化要求字段存在）
        let added_line = sse
            .lines()
            .find(|l| l.starts_with("data: ") && l.contains("custom_tool_call") && l.contains("in_progress"))
            .expect("added event data line");
        assert!(added_line.contains("PATCH BODY"), "added item carries full input");
    }

    #[test]
    fn sse_function_call_flow_unchanged() {
        let kinds = kinds_of(&[("shell", DeclaredToolKind::Function)]);
        let p = parsed_with_tool_calls(vec![json!({
            "id": "toolu_1", "type": "function",
            "function": { "name": "shell", "arguments": "{\"command\":[\"ls\"]}" },
        })]);
        let sse = build_responses_sse(&p, &kinds);
        assert!(sse.contains("event: response.function_call_arguments.delta"));
        assert!(sse.contains("event: response.function_call_arguments.done"));
        assert!(sse.contains("\"function_call\""));
        assert!(sse.contains("event: response.completed"));
    }

    #[test]
    fn sse_reasoning_summary_events() {
        let kinds = ToolKindMap::new();
        let mut p = parsed_with_tool_calls(vec![]);
        p.text = "hi".to_string();
        p.thinking = "deep thought".to_string();
        p.finish_reason = "stop".to_string();
        let sse = build_responses_sse(&p, &kinds);
        assert!(sse.contains("event: response.reasoning_summary_text.delta"));
        assert!(sse.contains("deep thought"));
        assert!(sse.contains("\"reasoning\""));
    }

    // ---- credit_usage 透传 ----

    #[test]
    fn response_object_omits_credit_fields_without_metering() {
        let kinds = ToolKindMap::new();
        let p = parsed_with_tool_calls(vec![]);
        let obj = build_responses_object(&p, &kinds);
        let usage = &obj["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
    }

    #[test]
    fn response_object_carries_credit_fields_when_metering_present() {
        let kinds = ToolKindMap::new();
        let mut p = parsed_with_tool_calls(vec![]);
        p.credit_usage = Some(0.25);
        p.credit_unit = Some("credit".to_string());
        p.credit_unit_plural = Some("credits".to_string());
        let obj = build_responses_object(&p, &kinds);
        let usage = &obj["usage"];
        assert_eq!(usage["credit_usage"], json!(0.25));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
        // 原有字段保持原样
        assert_eq!(usage["input_tokens"], json!(10));
        assert_eq!(usage["output_tokens"], json!(5));
    }

    #[test]
    fn response_completed_sse_event_contains_credit_fields() {
        let kinds = ToolKindMap::new();
        let mut p = parsed_with_tool_calls(vec![]);
        p.credit_usage = Some(0.99);
        p.credit_unit = Some("credit".to_string());
        p.credit_unit_plural = Some("credits".to_string());
        let sse = build_responses_sse(&p, &kinds);
        assert!(sse.contains("\"credit_usage\":0.99"));
        assert!(sse.contains("\"credit_unit\":\"credit\""));
        assert!(sse.contains("\"credit_unit_plural\":\"credits\""));
    }
}
