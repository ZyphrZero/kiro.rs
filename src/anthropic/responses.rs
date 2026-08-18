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
//!
//! 每个请求维护一张 name → 声明类型 的 [`ToolKindMap`]，请求翻译时生成、
//! 响应构造时消费，保证出方向 item 类型永远与声明一致。
//!
//! 客户端显式声明 `type:"web_search"` 时由 kiro-rs 内部代答：注入原生
//! `web_search_20250305` 并进入 handlers 的 web_search agentic loop。普通
//! 文本和本地工具请求不注入搜索工具，以便沿用 Anthropic 的实时流。
//!
//! `stream:true` 时内部同样使用流式 Messages 请求，并逐事件把 Anthropic SSE
//! 翻译成 Responses SSE；显式 WebSearch 仍由现有 agentic loop 整轮缓冲。

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    convert::Infallible,
};

use axum::{
    Json,
    body::{Body, to_bytes},
    extract::{Extension, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::handlers::post_messages;
use super::middleware::{AppState, KeyContext};
use super::openai::{
    ParsedResponse, collect_text_strings, now_ts, parse_anthropic_message, push_merged,
    resolve_session_metadata,
};
use super::types::{
    Message, MessagesRequest, Metadata, OutputConfig, SystemMessage, Thinking, Tool,
};

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
    #[serde(default = "default_parallel_tool_calls")]
    pub parallel_tool_calls: bool,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
}

fn default_parallel_tool_calls() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone)]
struct ResponsesResponseConfig {
    parallel_tool_calls: bool,
    tool_choice: Value,
    tools: Vec<Value>,
}

impl Default for ResponsesResponseConfig {
    fn default() -> Self {
        Self {
            parallel_tool_calls: true,
            tool_choice: json!("auto"),
            tools: Vec::new(),
        }
    }
}

impl ResponsesResponseConfig {
    fn from_request(req: &ResponsesRequest) -> Self {
        let mut tools = req.tools.clone().unwrap_or_default();
        if let Value::Array(items) = &req.input {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("additional_tools")
                    && let Some(extra) = item.get("tools").and_then(Value::as_array)
                {
                    tools.extend(extra.iter().cloned());
                }
            }
        }
        Self {
            parallel_tool_calls: req.parallel_tool_calls,
            tool_choice: req.tool_choice.clone().unwrap_or_else(|| json!("auto")),
            tools,
        }
    }
}

// ============================ Handler ============================

/// `POST /v1/responses`
pub async fn post_responses(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    headers: HeaderMap,
    Json(req): Json<ResponsesRequest>,
) -> Response {
    let want_stream = req.stream;
    let model = req.model.clone();
    let response_config = ResponsesResponseConfig::from_request(&req);
    let metadata = resolve_session_metadata(req.prompt_cache_key.as_deref(), &headers);

    tracing::info!(
        model = %model,
        stream = %want_stream,
        "Received POST /v1/responses request"
    );

    // 1. Responses -> Anthropic 请求翻译（同时得到工具声明类型表）
    let (anthropic_req, tool_kinds) = match responses_to_anthropic(req, metadata) {
        Ok(r) => r,
        Err(msg) => {
            return responses_error(StatusCode::BAD_REQUEST, "invalid_request_error", &msg);
        }
    };

    // 2. 复用 Anthropic 全链路。流式请求会得到标准 Anthropic SSE。
    let inner = post_messages(State(state), Extension(key_ctx), Json(anthropic_req)).await;

    let status = inner.status();
    if !status.is_success() {
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
        return Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body_bytes))
            .unwrap();
    }

    if want_stream {
        return responses_streaming_response(inner.into_body(), model, tool_kinds, response_config);
    }

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

    let body = build_responses_object_with_config(&parsed, &tool_kinds, &response_config);
    (StatusCode::OK, Json(body)).into_response()
}

// ============================ 请求翻译 ============================

fn responses_to_anthropic(
    req: ResponsesRequest,
    metadata: Option<Metadata>,
) -> Result<(MessagesRequest, ToolKindMap), String> {
    let max_tokens = req
        .max_output_tokens
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS);

    let mut system: Vec<SystemMessage> = Vec::new();
    if let Some(instr) = req.instructions.as_ref()
        && !instr.trim().is_empty()
    {
        system.push(SystemMessage {
            text: instr.clone(),
            cache_control: None,
        });
    }

    let mut merged: Vec<(String, Vec<Value>)> = Vec::new();
    // codex 0.144 把工具声明放进 input 里的 `additional_tools` item
    //（顶层 `tools` 通常为空）；两处都收集，合并转换。
    let mut declared_entries: Vec<Value> = req.tools.clone().unwrap_or_default();

    match &req.input {
        // input 直接是字符串 → 单条 user 文本
        Value::String(s) if !s.is_empty() => {
            push_merged(&mut merged, "user", vec![json!({"type":"text","text":s})]);
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

                translate_input_item(item, &mut system, &mut merged)?;
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

    let hosted_web_search_declared = declared_entries
        .iter()
        .any(|entry| entry.get("type").and_then(Value::as_str) == Some("web_search"));

    let tool_choice = req
        .tool_choice
        .as_ref()
        .map(convert_tool_choice)
        .transpose()?;
    let tool_choice_none = tool_choice
        .as_ref()
        .is_some_and(|choice| choice.get("type").and_then(Value::as_str) == Some("none"));

    // 翻译 codex 声明的本地工具，并记录每个工具的声明类型（出方向要用）。
    let mut tool_kinds: ToolKindMap = HashMap::new();
    let mut tool_list = convert_responses_tools(&declared_entries, &mut tool_kinds, None);

    // `tool_choice: none` 是显式禁止工具调用，不能让 hosted web_search
    // 注入工具，也不能把声明的工具继续转发给上游。
    if !tool_choice_none
        // 只有 hosted web_search 声明才进入内部搜索循环。若客户端声明了同名
        // function，则客户端工具优先，避免破坏工具类型及名称还原。
        && hosted_web_search_declared
        && !tool_list.iter().any(|t| t.name == "web_search")
    {
        if tool_list.is_empty() {
            // noop 保证不触发单 WebSearch fast-path，继续使用已验证的 agentic loop。
            tool_list.push(Tool {
                tool_type: None,
                name: "noop".to_string(),
                description: "Placeholder tool; never call this.".to_string(),
                input_schema: Default::default(),
                max_uses: None,
                cache_control: None,
            });
            tool_list.push(native_web_search_tool());
            system.push(SystemMessage {
                text: NUDGE_STRICT.to_string(),
                cache_control: None,
            });
        } else {
            tool_list.push(native_web_search_tool());
            system.push(SystemMessage {
                text: NUDGE_SOFT.to_string(),
                cache_control: None,
            });
        }
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

    let tools = if tool_choice_none || tool_list.is_empty() {
        None
    } else {
        Some(tool_list)
    };
    let output_config = req
        .reasoning
        .as_ref()
        .and_then(|r| r.effort.clone())
        .filter(|e| !e.trim().is_empty())
        .map(|effort| OutputConfig { effort });
    let thinking = req.reasoning.and_then(|reasoning| {
        reasoning
            .effort
            .filter(|effort| !effort.trim().is_empty())
            .map(|_| Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: 20_000,
            })
    });

    Ok((
        MessagesRequest {
            model: req.model,
            max_tokens,
            messages,
            stream: req.stream,
            system: if system.is_empty() {
                None
            } else {
                Some(system)
            },
            tools,
            tool_choice,
            thinking,
            output_config,
            metadata,
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
) -> Result<(), String> {
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
            let input: Value = serde_json::from_str(args_str).map_err(|error| {
                format!("input function_call {call_id} has invalid JSON arguments: {error}")
            })?;
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
                return Ok(());
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
    Ok(())
}

/// 把 Responses message.content（字符串或数组）转成 Anthropic content blocks
fn content_blocks(content: Option<&Value>) -> Vec<Value> {
    let mut out = Vec::new();
    match content {
        Some(Value::String(s)) if !s.is_empty() => {
            out.push(json!({"type":"text","text":s}));
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                let ty = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ty {
                    "input_text" | "output_text" | "text" => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str())
                            && !t.is_empty()
                        {
                            out.push(json!({"type":"text","text":t}));
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

fn convert_tool_choice(tc: &Value) -> Result<Value, String> {
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => Ok(json!({"type":"auto"})),
            "required" => Ok(json!({"type":"any"})),
            "none" => Ok(json!({"type":"none"})),
            other => Err(format!("unsupported tool_choice value: {other}")),
        },
        Value::Object(object) => {
            let choice_type = object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("function");
            match choice_type {
                "function" => {
                    let name = object
                        .get("name")
                        .or_else(|| object.get("function").and_then(|f| f.get("name")))
                        .and_then(Value::as_str)
                        .ok_or_else(|| "tool_choice function is missing name".to_string())?;
                    let namespace = object.get("namespace").and_then(Value::as_str);
                    Ok(json!({"type":"tool","name":flat_tool_name(namespace, name)}))
                }
                "auto" => Ok(json!({"type":"auto"})),
                "required" => Ok(json!({"type":"any"})),
                "none" => Ok(json!({"type":"none"})),
                other => Err(format!("unsupported tool_choice type: {other}")),
            }
        }
        _ => Err("tool_choice must be a string or object".to_string()),
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
            if map.len() == 1
                && let Some(Value::String(s)) = map.values().next()
            {
                return s.clone();
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
        "input_tokens_details": { "cached_tokens": p.cached_tokens },
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

fn build_response_object_from(
    p: &ParsedResponse,
    view: &ResponsesView,
    id: &str,
    config: &ResponsesResponseConfig,
) -> Value {
    let mut obj = json!({
        "id": id,
        "object": "response",
        "created_at": now_ts(),
        "status": view.status,
        "model": p.model,
        "output": view.output,
        "usage": view.usage,
        "parallel_tool_calls": config.parallel_tool_calls,
        "tool_choice": config.tool_choice,
        "tools": config.tools,
    });
    if view.status == "incomplete" {
        obj["incomplete_details"] = json!({ "reason": "max_output_tokens" });
    }
    obj
}

fn build_responses_object_with_config(
    p: &ParsedResponse,
    kinds: &ToolKindMap,
    config: &ResponsesResponseConfig,
) -> Value {
    let view = build_view(p, kinds);
    let id = new_resp_id();
    build_response_object_from(p, &view, &id, config)
}

#[cfg(test)]
fn build_responses_object(p: &ParsedResponse, kinds: &ToolKindMap) -> Value {
    build_responses_object_with_config(p, kinds, &ResponsesResponseConfig::default())
}

// ============================ 流式响应翻译 ============================

#[derive(Debug)]
enum StreamingBlock {
    Text {
        item_id: String,
        output_index: Option<i64>,
        text: String,
    },
    Reasoning {
        item_id: String,
        output_index: Option<i64>,
        text: String,
    },
    Tool {
        call_id: String,
        flat_name: String,
        arguments: String,
    },
    WebSearch {
        item_id: String,
        query: String,
        output_index: i64,
    },
    Ignored,
}

struct ResponsesStreamContext {
    response_id: String,
    created_at: i64,
    model: String,
    tool_kinds: ToolKindMap,
    response_config: ResponsesResponseConfig,
    sequence_number: i64,
    next_output_index: i64,
    blocks: HashMap<i64, StreamingBlock>,
    output: Vec<Value>,
    input_tokens: i64,
    cache_creation_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    credit_usage: Option<f64>,
    credit_unit: Option<String>,
    credit_unit_plural: Option<String>,
    stop_reason: Option<String>,
    saw_message_stop: bool,
    terminal: bool,
}

impl ResponsesStreamContext {
    fn new(
        model: String,
        tool_kinds: ToolKindMap,
        response_config: ResponsesResponseConfig,
    ) -> Self {
        Self {
            response_id: new_resp_id(),
            created_at: now_ts(),
            model,
            tool_kinds,
            response_config,
            sequence_number: 0,
            next_output_index: 0,
            blocks: HashMap::new(),
            output: Vec::new(),
            input_tokens: 0,
            cache_creation_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            credit_usage: None,
            credit_unit: None,
            credit_unit_plural: None,
            stop_reason: None,
            saw_message_stop: false,
            terminal: false,
        }
    }

    fn initial_events(&mut self) -> Vec<Bytes> {
        let response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": "in_progress",
            "model": self.model,
            "output": [],
        });
        vec![
            self.emit("response.created", json!({ "response": response.clone() })),
            self.emit("response.in_progress", json!({ "response": response })),
        ]
    }

    fn emit(&mut self, event_type: &str, mut payload: Value) -> Bytes {
        payload["type"] = json!(event_type);
        payload["sequence_number"] = json!(self.sequence_number);
        self.sequence_number += 1;
        Bytes::from(format!("event: {event_type}\ndata: {}\n\n", payload))
    }

    fn allocate_output_index(&mut self) -> i64 {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }

    fn handle_anthropic_event(&mut self, event: &str, data: Value) -> Vec<Bytes> {
        if self.terminal {
            return Vec::new();
        }
        // The hosted web-search loop carries unsigned provider reasoning as an
        // out-of-band field on the next standard event. Consume it first so the
        // Responses reasoning item precedes the visible answer/tool item.
        let mut events = data
            .get("kiro_thinking")
            .and_then(Value::as_str)
            .filter(|thinking| !thinking.is_empty())
            .map(|thinking| self.emit_reasoning_summary(thinking))
            .unwrap_or_default();
        let translated = match event {
            "message_start" => {
                if let Some(usage) = data.pointer("/message/usage") {
                    self.update_usage(usage);
                }
                Vec::new()
            }
            "content_block_start" => self.handle_block_start(&data),
            "content_block_delta" => self.handle_block_delta(&data),
            "content_block_stop" => self.handle_block_stop(&data),
            "message_delta" => {
                if let Some(usage) = data.get("usage") {
                    self.update_usage(usage);
                }
                self.stop_reason = data
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Vec::new()
            }
            "message_stop" => {
                self.saw_message_stop = true;
                self.finish()
            }
            "error" => {
                let error_type = data
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .unwrap_or("server_error");
                let message = data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream stream failed");
                vec![self.fail(error_type, message)]
            }
            "ping" => vec![Bytes::from(": ping\n\n")],
            _ => Vec::new(),
        };
        events.extend(translated);
        events
    }

    fn handle_block_start(&mut self, data: &Value) -> Vec<Bytes> {
        let Some(index) = data.get("index").and_then(Value::as_i64) else {
            return Vec::new();
        };
        let block = data.get("content_block").unwrap_or(&Value::Null);
        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
        let state = match block_type {
            "text" => StreamingBlock::Text {
                item_id: new_msg_id(),
                output_index: None,
                text: String::new(),
            },
            "thinking" => StreamingBlock::Reasoning {
                item_id: new_rs_id(),
                output_index: None,
                text: String::new(),
            },
            "tool_use" => StreamingBlock::Tool {
                call_id: block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                flat_name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                arguments: String::new(),
            },
            "server_tool_use"
                if block.get("name").and_then(Value::as_str) == Some("web_search") =>
            {
                let item_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(new_fc_id);
                let query = block
                    .pointer("/input/query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let output_index = self.allocate_output_index();
                let added = json!({
                    "type": "web_search_call",
                    "id": item_id,
                    "status": "in_progress",
                    "action": { "type": "search", "query": query },
                });
                self.blocks.insert(
                    index,
                    StreamingBlock::WebSearch {
                        item_id,
                        query,
                        output_index,
                    },
                );
                return vec![self.emit(
                    "response.output_item.added",
                    json!({ "output_index": output_index, "item": added }),
                )];
            }
            _ => StreamingBlock::Ignored,
        };
        self.blocks.insert(index, state);
        Vec::new()
    }

    fn handle_block_delta(&mut self, data: &Value) -> Vec<Bytes> {
        let Some(index) = data.get("index").and_then(Value::as_i64) else {
            return Vec::new();
        };
        let delta = data.get("delta").unwrap_or(&Value::Null);
        match delta.get("type").and_then(Value::as_str).unwrap_or("") {
            "text_delta" => {
                let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                self.handle_text_delta(index, text)
            }
            "thinking_delta" => {
                let text = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                self.handle_reasoning_delta(index, text)
            }
            "input_json_delta" => {
                if let Some(StreamingBlock::Tool { arguments, .. }) = self.blocks.get_mut(&index) {
                    arguments.push_str(
                        delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    );
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_text_delta(&mut self, index: i64, delta: &str) -> Vec<Bytes> {
        if delta.is_empty() {
            return Vec::new();
        }
        let needs_start = matches!(
            self.blocks.get(&index),
            Some(StreamingBlock::Text {
                output_index: None,
                ..
            })
        );
        if needs_start {
            let output_index = self.allocate_output_index();
            if let Some(StreamingBlock::Text {
                output_index: block_index,
                ..
            }) = self.blocks.get_mut(&index)
            {
                *block_index = Some(output_index);
            }
        }
        let Some(StreamingBlock::Text {
            item_id,
            output_index: Some(output_index),
            text,
        }) = self.blocks.get_mut(&index)
        else {
            return Vec::new();
        };
        let first = text.is_empty();
        text.push_str(delta);
        let item_id = item_id.clone();
        let output_index = *output_index;
        let mut events = Vec::new();
        if first {
            events.push(self.emit(
                "response.output_item.added",
                json!({ "output_index": output_index, "item": {
                    "type": "message", "id": item_id, "status": "in_progress",
                    "role": "assistant", "content": [],
                }}),
            ));
            events.push(self.emit(
                "response.content_part.added",
                json!({
                    "item_id": item_id, "output_index": output_index, "content_index": 0,
                    "part": { "type": "output_text", "text": "", "annotations": [] },
                }),
            ));
        }
        events.push(self.emit(
            "response.output_text.delta",
            json!({
                "item_id": item_id, "output_index": output_index,
                "content_index": 0, "delta": delta,
            }),
        ));
        events
    }

    fn handle_reasoning_delta(&mut self, index: i64, delta: &str) -> Vec<Bytes> {
        if delta.is_empty() {
            return Vec::new();
        }
        let needs_start = matches!(
            self.blocks.get(&index),
            Some(StreamingBlock::Reasoning {
                output_index: None,
                ..
            })
        );
        if needs_start {
            let output_index = self.allocate_output_index();
            if let Some(StreamingBlock::Reasoning {
                output_index: block_index,
                ..
            }) = self.blocks.get_mut(&index)
            {
                *block_index = Some(output_index);
            }
        }
        let Some(StreamingBlock::Reasoning {
            item_id,
            output_index: Some(output_index),
            text,
        }) = self.blocks.get_mut(&index)
        else {
            return Vec::new();
        };
        let first = text.is_empty();
        text.push_str(delta);
        let item_id = item_id.clone();
        let output_index = *output_index;
        let mut events = Vec::new();
        if first {
            events.push(self.emit(
                "response.output_item.added",
                json!({ "output_index": output_index, "item": {
                    "type": "reasoning", "id": item_id, "summary": [],
                }}),
            ));
        }
        events.push(self.emit(
            "response.reasoning_summary_text.delta",
            json!({
                "item_id": item_id, "output_index": output_index,
                "summary_index": 0, "delta": delta,
            }),
        ));
        events
    }

    fn handle_block_stop(&mut self, data: &Value) -> Vec<Bytes> {
        let Some(index) = data.get("index").and_then(Value::as_i64) else {
            return Vec::new();
        };
        match self.blocks.remove(&index) {
            Some(StreamingBlock::Text {
                item_id,
                output_index: Some(output_index),
                text,
            }) => self.finish_text(item_id, output_index, text),
            Some(StreamingBlock::Reasoning {
                item_id,
                output_index: Some(output_index),
                text,
            }) => self.finish_reasoning(item_id, output_index, text),
            Some(StreamingBlock::Tool {
                call_id,
                flat_name,
                arguments,
            }) => self.finish_tool(call_id, flat_name, arguments),
            Some(StreamingBlock::WebSearch {
                item_id,
                query,
                output_index,
            }) => self.finish_web_search(item_id, query, output_index),
            _ => Vec::new(),
        }
    }

    fn finish_text(&mut self, item_id: String, output_index: i64, text: String) -> Vec<Bytes> {
        let item = json!({
            "type": "message", "id": item_id, "status": "completed",
            "role": "assistant", "content": [{
                "type": "output_text", "text": text, "annotations": [],
            }],
        });
        self.output.push(item.clone());
        vec![
            self.emit(
                "response.output_text.done",
                json!({
                    "item_id": item_id, "output_index": output_index,
                    "content_index": 0, "text": text,
                }),
            ),
            self.emit(
                "response.content_part.done",
                json!({
                    "item_id": item_id, "output_index": output_index, "content_index": 0,
                    "part": { "type": "output_text", "text": text, "annotations": [] },
                }),
            ),
            self.emit(
                "response.output_item.done",
                json!({ "output_index": output_index, "item": item }),
            ),
        ]
    }

    fn finish_reasoning(&mut self, item_id: String, output_index: i64, text: String) -> Vec<Bytes> {
        let item = json!({
            "type": "reasoning", "id": item_id,
            "summary": [{ "type": "summary_text", "text": text }],
        });
        self.output.push(item.clone());
        vec![
            self.emit(
                "response.reasoning_summary_text.done",
                json!({
                    "item_id": item_id, "output_index": output_index,
                    "summary_index": 0, "text": text,
                }),
            ),
            self.emit(
                "response.output_item.done",
                json!({ "output_index": output_index, "item": item }),
            ),
        ]
    }

    /// Translate the hosted web-search loop's out-of-band reasoning extension.
    /// It deliberately does not use an Anthropic `thinking` block because those
    /// require a provider signature when replayed by Anthropic clients.
    fn emit_reasoning_summary(&mut self, text: &str) -> Vec<Bytes> {
        let item_id = new_rs_id();
        let output_index = self.allocate_output_index();
        let item = json!({
            "type": "reasoning", "id": item_id,
            "summary": [{ "type": "summary_text", "text": text }],
        });
        self.output.push(item.clone());
        vec![
            self.emit(
                "response.output_item.added",
                json!({ "output_index": output_index, "item": {
                    "type": "reasoning", "id": item_id, "summary": [],
                }}),
            ),
            self.emit(
                "response.reasoning_summary_text.delta",
                json!({
                    "item_id": item_id, "output_index": output_index,
                    "summary_index": 0, "delta": text,
                }),
            ),
            self.emit(
                "response.reasoning_summary_text.done",
                json!({
                    "item_id": item_id, "output_index": output_index,
                    "summary_index": 0, "text": text,
                }),
            ),
            self.emit(
                "response.output_item.done",
                json!({ "output_index": output_index, "item": item }),
            ),
        ]
    }

    fn finish_tool(
        &mut self,
        call_id: String,
        flat_name: String,
        mut arguments: String,
    ) -> Vec<Bytes> {
        if arguments.trim().is_empty() {
            arguments = "{}".to_string();
        }
        let output_index = self.allocate_output_index();
        let decl = self.tool_kinds.get(&flat_name);
        let (name, namespace, kind) = match decl {
            Some(d) => (d.name.clone(), d.namespace.clone(), d.kind),
            None => (flat_name, None, DeclaredToolKind::Function),
        };
        let (item, added, delta_event, done_event, done_key, value) = match kind {
            DeclaredToolKind::Custom => {
                let input = custom_input_text(&arguments);
                let mut item = json!({
                    "type": "custom_tool_call", "id": new_ctc_id(),
                    "call_id": call_id, "name": name, "input": input,
                    "status": "completed",
                });
                if let Some(ns) = namespace {
                    item["namespace"] = json!(ns);
                }
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                (
                    item,
                    added,
                    "response.custom_tool_call_input.delta",
                    "response.custom_tool_call_input.done",
                    "input",
                    input,
                )
            }
            DeclaredToolKind::Function => {
                let mut item = json!({
                    "type": "function_call", "id": new_fc_id(),
                    "call_id": call_id, "name": name, "arguments": arguments,
                    "status": "completed",
                });
                if let Some(ns) = namespace {
                    item["namespace"] = json!(ns);
                }
                let mut added = item.clone();
                added["status"] = json!("in_progress");
                added["arguments"] = json!("");
                (
                    item,
                    added,
                    "response.function_call_arguments.delta",
                    "response.function_call_arguments.done",
                    "arguments",
                    arguments,
                )
            }
        };
        let item_id = item["id"].as_str().unwrap_or("").to_string();
        self.output.push(item.clone());
        let mut delta_payload = json!({
            "item_id": item_id,
            "output_index": output_index,
        });
        delta_payload["delta"] = json!(value);
        let mut done_payload = json!({
            "item_id": item_id,
            "output_index": output_index,
        });
        done_payload[done_key] = json!(value);
        vec![
            self.emit(
                "response.output_item.added",
                json!({ "output_index": output_index, "item": added }),
            ),
            self.emit(delta_event, delta_payload),
            self.emit(done_event, done_payload),
            self.emit(
                "response.output_item.done",
                json!({ "output_index": output_index, "item": item }),
            ),
        ]
    }

    fn finish_web_search(
        &mut self,
        item_id: String,
        query: String,
        output_index: i64,
    ) -> Vec<Bytes> {
        let item = json!({
            "type": "web_search_call", "id": item_id, "status": "completed",
            "action": { "type": "search", "query": query },
        });
        self.output.push(item.clone());
        vec![self.emit(
            "response.output_item.done",
            json!({ "output_index": output_index, "item": item }),
        )]
    }

    fn update_usage(&mut self, usage: &Value) {
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_i64) {
            self.input_tokens = value.max(0);
        }
        if let Some(value) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_i64)
        {
            self.cache_creation_tokens = value.max(0);
        }
        if let Some(value) = usage.get("output_tokens").and_then(Value::as_i64) {
            self.output_tokens = value.max(0);
        }
        if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_i64) {
            self.cached_tokens = value.max(0);
        }
        if let Some(value) = usage.get("reasoning_tokens").and_then(Value::as_i64) {
            self.reasoning_tokens = value.max(0);
        }
        if let Some(value) = usage.get("credit_usage").and_then(Value::as_f64) {
            self.credit_usage = Some(value);
        }
        if let Some(value) = usage.get("credit_unit").and_then(Value::as_str) {
            self.credit_unit = Some(value.to_string());
        }
        if let Some(value) = usage.get("credit_unit_plural").and_then(Value::as_str) {
            self.credit_unit_plural = Some(value.to_string());
        }
    }

    fn usage(&self) -> Value {
        let total_input_tokens = self
            .input_tokens
            .saturating_add(self.cache_creation_tokens)
            .saturating_add(self.cached_tokens);
        let mut usage = json!({
            "input_tokens": total_input_tokens,
            "input_tokens_details": { "cached_tokens": self.cached_tokens },
            "output_tokens": self.output_tokens,
            "output_tokens_details": { "reasoning_tokens": self.reasoning_tokens },
            "total_tokens": total_input_tokens.saturating_add(self.output_tokens),
        });
        if let Some(value) = self.credit_usage {
            usage["credit_usage"] = json!(value);
        }
        if let Some(value) = &self.credit_unit {
            usage["credit_unit"] = json!(value);
        }
        if let Some(value) = &self.credit_unit_plural {
            usage["credit_unit_plural"] = json!(value);
        }
        usage
    }

    fn response_object(&self, status: &str) -> Value {
        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": self.model,
            "output": self.output,
            "usage": self.usage(),
            "parallel_tool_calls": self.response_config.parallel_tool_calls,
            "tool_choice": self.response_config.tool_choice,
            "tools": self.response_config.tools,
        });
        if status == "incomplete" {
            response["incomplete_details"] = json!({ "reason": "max_output_tokens" });
        }
        response
    }

    fn finish(&mut self) -> Vec<Bytes> {
        if self.terminal {
            return Vec::new();
        }
        if !self.saw_message_stop {
            return vec![self.fail("server_error", "upstream stream ended before message_stop")];
        }
        let incomplete = matches!(
            self.stop_reason.as_deref(),
            Some("max_tokens" | "model_context_window_exceeded")
        );
        let status = if incomplete {
            "incomplete"
        } else {
            "completed"
        };
        let event = if incomplete {
            "response.incomplete"
        } else {
            "response.completed"
        };
        self.terminal = true;
        vec![self.emit(event, json!({ "response": self.response_object(status) }))]
    }

    fn fail(&mut self, error_type: &str, message: &str) -> Bytes {
        self.terminal = true;
        let mut response = self.response_object("failed");
        response["error"] = json!({
            "code": error_type,
            "message": message,
        });
        self.emit("response.failed", json!({ "response": response }))
    }
}

fn responses_streaming_response(
    body: Body,
    model: String,
    tool_kinds: ToolKindMap,
    response_config: ResponsesResponseConfig,
) -> Response {
    let mut context = ResponsesStreamContext::new(model, tool_kinds, response_config);
    let pending = VecDeque::from(context.initial_events());
    let stream = stream::unfold(
        (
            body.into_data_stream(),
            Vec::<u8>::new(),
            context,
            pending,
            false,
        ),
        |(mut body, mut buffer, mut context, mut pending, mut finished)| async move {
            loop {
                if let Some(bytes) = pending.pop_front() {
                    return Some((
                        Ok::<Bytes, Infallible>(bytes),
                        (body, buffer, context, pending, finished),
                    ));
                }
                if finished || context.terminal {
                    return None;
                }
                match body.next().await {
                    Some(Ok(chunk)) => {
                        buffer.extend_from_slice(&chunk);
                        for frame in take_sse_frames(&mut buffer) {
                            if context.terminal {
                                break;
                            }
                            match parse_sse_frame(&frame) {
                                Ok(Some((event, data))) => {
                                    pending.extend(context.handle_anthropic_event(&event, data))
                                }
                                Ok(None) => {}
                                Err(message) => {
                                    pending.push_back(context.fail("server_error", &message))
                                }
                            }
                        }
                    }
                    Some(Err(error)) => {
                        pending.push_back(context.fail(
                            "upstream_error",
                            &format!("failed to read upstream stream: {error}"),
                        ));
                        finished = true;
                    }
                    None => {
                        if buffer.iter().any(|byte| !byte.is_ascii_whitespace()) {
                            pending.push_back(
                                context
                                    .fail("server_error", "upstream sent an incomplete SSE frame"),
                            );
                        } else {
                            pending.extend(context.finish());
                        }
                        finished = true;
                    }
                }
            }
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn take_sse_frames(buffer: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    loop {
        let lf = buffer.windows(2).position(|window| window == b"\n\n");
        let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
        let delimiter = match (lf, crlf) {
            (Some(a), Some(b)) if a <= b => Some((a, 2)),
            (Some(_), Some(b)) => Some((b, 4)),
            (Some(a), None) => Some((a, 2)),
            (None, Some(b)) => Some((b, 4)),
            (None, None) => None,
        };
        let Some((position, length)) = delimiter else {
            break;
        };
        let frame = buffer.drain(..position).collect::<Vec<_>>();
        buffer.drain(..length);
        if frame.iter().any(|byte| !byte.is_ascii_whitespace()) {
            frames.push(frame);
        }
    }
    frames
}

fn parse_sse_frame(frame: &[u8]) -> Result<Option<(String, Value)>, String> {
    let text = std::str::from_utf8(frame)
        .map_err(|error| format!("upstream sent invalid UTF-8 SSE: {error}"))?;
    let mut event = None;
    let mut data_lines = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim_start());
        }
    }
    let Some(event) = event else {
        return Ok(None);
    };
    if event == "ping" {
        return Ok(Some((event, json!({ "type": "ping" }))));
    }
    let data = serde_json::from_str::<Value>(&data_lines.join("\n"))
        .map_err(|error| format!("failed to parse upstream SSE event {event}: {error}"))?;
    Ok(Some((event, data)))
}

/// 把完整结果合成为 Responses SSE 事件序列
///
/// codex 只从 `response.output_item.done` 构建回合内容（`.added` 用于进度
/// 展示，`response.completed` 只取 id/usage），所以每个 item 只要保证
/// added/done 成对且 done 携带完整内容即可。delta 事件是锦上添花。
#[cfg(test)]
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
    let final_obj =
        build_response_object_from(p, &view, &resp_id, &ResponsesResponseConfig::default());
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

    #[test]
    fn responses_body_session_metadata_is_forwarded() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "input": simple_input(),
            "prompt_cache_key": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .unwrap();
        let metadata = resolve_session_metadata(req.prompt_cache_key.as_deref(), &HeaderMap::new());
        let (anthropic, _) = responses_to_anthropic(req, metadata).unwrap();

        assert_eq!(
            anthropic
                .metadata
                .and_then(|value| value.user_id)
                .as_deref(),
            Some("session_550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn responses_header_session_fallback_and_invalid_candidates_are_tolerated() {
        let req = req_with(json!([]), simple_input());
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-client-request-id",
            "session_67e55044-10b1-426f-9247-bb680e5fe0c8"
                .parse()
                .unwrap(),
        );
        let metadata = resolve_session_metadata(req.prompt_cache_key.as_deref(), &headers);
        let (anthropic, _) = responses_to_anthropic(req, metadata).unwrap();
        assert_eq!(
            anthropic
                .metadata
                .and_then(|value| value.user_id)
                .as_deref(),
            Some("session_67e55044-10b1-426f-9247-bb680e5fe0c8")
        );

        let req = req_with(json!([]), simple_input());
        headers.insert("x-client-request-id", "invalid".parse().unwrap());
        let metadata = resolve_session_metadata(Some("also-invalid"), &headers);
        assert!(metadata.is_none());
        let (anthropic, _) = responses_to_anthropic(req, metadata).unwrap();
        assert!(anthropic.metadata.is_none());
    }

    fn parsed_with_tool_calls(tool_calls: Vec<Value>) -> ParsedResponse {
        ParsedResponse {
            model: "gpt-5.6-sol".to_string(),
            text: String::new(),
            tool_calls,
            finish_reason: "tool_calls".to_string(),
            prompt_tokens: 10,
            cached_tokens: 0,
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

    fn event_text(events: Vec<Bytes>) -> String {
        events
            .into_iter()
            .map(|bytes| String::from_utf8(bytes.to_vec()).unwrap())
            .collect()
    }

    fn feed_event(context: &mut ResponsesStreamContext, event: &str, data: Value) -> String {
        event_text(context.handle_anthropic_event(event, data))
    }

    fn sequence_numbers(sse: &str) -> Vec<i64> {
        sse.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| {
                serde_json::from_str::<Value>(data).unwrap()["sequence_number"]
                    .as_i64()
                    .unwrap()
            })
            .collect()
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
        let (anth, kinds) = responses_to_anthropic(req, None).unwrap();
        assert_eq!(
            kinds.get("exec").map(|d| d.kind),
            Some(DeclaredToolKind::Custom)
        );
        assert_eq!(
            kinds.get("wait").map(|d| d.kind),
            Some(DeclaredToolKind::Function)
        );
        let tools = anth.tools.as_ref().unwrap();
        assert!(tools.iter().any(|t| t.name == "exec"));
        assert!(tools.iter().any(|t| t.name == "wait"));
        assert!(!tools.iter().any(|t| t.name == "web_search"));
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
        let (anth, kinds) = responses_to_anthropic(req, None).unwrap();
        let decl = kinds
            .get("collaboration__spawn_agent")
            .expect("flattened name");
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
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
        let tu = &anth.messages[1].content.as_array().unwrap()[0];
        assert_eq!(tu["type"], "tool_use");
        assert_eq!(
            tu["name"], "collaboration__spawn_agent",
            "replayed namespaced call must use the flat name the model was declared"
        );
    }

    #[test]
    fn tool_choice_none_disables_forwarded_tools() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "input": simple_input(),
            "tools": [{ "type": "function", "name": "shell", "parameters": {} }],
            "tool_choice": "none",
        }))
        .unwrap();
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
        assert!(anth.tools.is_none());
        assert_eq!(anth.tool_choice, Some(json!({"type": "none"})));
    }

    #[test]
    fn namespaced_tool_choice_is_flattened() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "input": simple_input(),
            "tools": [{
                "type": "namespace",
                "name": "collaboration",
                "tools": [{ "type": "function", "name": "spawn_agent", "parameters": {} }]
            }],
            "tool_choice": {
                "type": "function",
                "name": "spawn_agent",
                "namespace": "collaboration"
            },
        }))
        .unwrap();
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
        assert_eq!(
            anth.tool_choice,
            Some(json!({"type": "tool", "name": "collaboration__spawn_agent"}))
        );
    }

    #[test]
    fn reasoning_effort_enables_non_stream_reasoning_extraction() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "input": simple_input(),
            "reasoning": { "effort": "medium" },
        }))
        .unwrap();
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
        assert!(anth.thinking.as_ref().is_some_and(Thinking::is_enabled));
        assert_eq!(anth.output_config.as_ref().unwrap().effort, "medium");
    }

    #[test]
    fn malformed_historical_function_arguments_are_rejected() {
        let req = req_with(
            json!([]),
            json!([
                { "type": "message", "role": "user", "content": "go" },
                { "type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{" },
            ]),
        );
        let error = responses_to_anthropic(req, None).unwrap_err();
        assert!(error.contains("invalid JSON arguments"));
    }

    #[test]
    fn response_metadata_preserves_request_controls() {
        let config = ResponsesResponseConfig {
            parallel_tool_calls: false,
            tool_choice: json!("required"),
            tools: vec![json!({ "type": "function", "name": "shell" })],
        };
        let mut p = parsed_with_tool_calls(vec![]);
        p.text = "done".to_string();
        let response = build_responses_object_with_config(&p, &ToolKindMap::new(), &config);
        assert_eq!(response["parallel_tool_calls"], json!(false));
        assert_eq!(response["tool_choice"], json!("required"));
        assert_eq!(response["tools"][0]["name"], json!("shell"));
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
        let (anth, kinds) = responses_to_anthropic(req, None).unwrap();
        assert_eq!(
            kinds.get("apply_patch").map(|d| d.kind),
            Some(DeclaredToolKind::Custom)
        );

        let tools = anth.tools.as_ref().unwrap();
        let ap = tools.iter().find(|t| t.name == "apply_patch").unwrap();
        // 包装 schema：单个 input 字符串字段
        let props = ap.input_schema.get("properties").unwrap();
        assert!(props.get("input").is_some(), "wrapper input field required");
        assert_eq!(ap.input_schema.get("required").unwrap(), &json!(["input"]));
        // grammar 附加到描述
        assert!(ap.description.contains("Apply a patch."));
        assert!(ap.description.contains("lark grammar"));
        assert!(ap.description.contains("start: PATCH"));
        // 普通本地工具不应触发缓冲式 WebSearch loop。
        assert!(!tools.iter().any(|t| t.name == "web_search"));
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
        let (anth, kinds) = responses_to_anthropic(req, None).unwrap();
        assert_eq!(
            kinds.get("shell").map(|d| d.kind),
            Some(DeclaredToolKind::Function)
        );
        let tools = anth.tools.as_ref().unwrap();
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
    fn no_tools_remains_toolless() {
        let req = req_with(json!([]), simple_input());
        let (anth, kinds) = responses_to_anthropic(req, None).unwrap();
        assert!(kinds.is_empty());
        assert!(anth.tools.is_none());
        assert!(system_texts(&anth).is_empty());
    }

    #[test]
    fn local_tools_do_not_add_web_search_nudge() {
        let req = req_with(
            json!([{ "type": "function", "name": "shell", "parameters": {} }]),
            simple_input(),
        );
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
        let tools = anth.tools.as_ref().unwrap();
        assert!(tools.iter().any(|tool| tool.name == "shell"));
        assert!(!tools.iter().any(|tool| tool.name == "web_search"));
        assert!(system_texts(&anth).is_empty());
    }

    #[test]
    fn web_search_name_collision_skips_native_injection() {
        let req = req_with(
            json!([{ "type": "function", "name": "web_search", "parameters": {} }]),
            simple_input(),
        );
        let (anth, kinds) = responses_to_anthropic(req, None).unwrap();
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
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
        let tools = anth.tools.as_ref().unwrap();
        let ws: Vec<&Tool> = tools.iter().filter(|t| t.name == "web_search").collect();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].tool_type.as_deref(), Some("web_search_20250305"));
        assert!(
            system_texts(&anth)
                .iter()
                .any(|text| text.contains("Use your other tools normally"))
        );
    }

    #[test]
    fn hosted_web_search_without_local_tools_uses_agentic_loop_pair() {
        let req = req_with(json!([{ "type": "web_search" }]), simple_input());
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
        let tools = anth.tools.as_ref().unwrap();
        let names: Vec<_> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, vec!["noop", "web_search"]);
        assert!(
            system_texts(&anth)
                .iter()
                .any(|text| text.contains("Do not call any other tool"))
        );
    }

    #[test]
    fn stream_flag_is_forwarded_to_messages_request() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.6-sol",
            "input": simple_input(),
            "stream": true,
        }))
        .unwrap();
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
        assert!(anth.stream);
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
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
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
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
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
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
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
        let (anth, _) = responses_to_anthropic(req, None).unwrap();
        assert_eq!(anth.messages.len(), 1);
        assert_eq!(anth.messages[0].role, "user");
    }

    // ---- custom input 解包回退链 ----

    #[test]
    fn custom_input_unwrap_fallbacks() {
        // 标准：{"input": "..."}
        assert_eq!(
            custom_input_text(r#"{"input":"*** Begin Patch"}"#),
            "*** Begin Patch"
        );
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
    fn streaming_text_preserves_each_delta_and_completes_with_usage() {
        let mut context = ResponsesStreamContext::new(
            "gpt-5.6-sol".into(),
            ToolKindMap::new(),
            ResponsesResponseConfig::default(),
        );
        let mut sse = event_text(context.initial_events());
        sse.push_str(&feed_event(
            &mut context,
            "message_start",
            json!({
                "type": "message_start",
                "message": { "usage": { "input_tokens": 7 } },
            }),
        ));
        sse.push_str(&feed_event(
            &mut context,
            "content_block_start",
            json!({
                "type": "content_block_start", "index": 0,
                "content_block": { "type": "text", "text": "" },
            }),
        ));
        let first = feed_event(
            &mut context,
            "content_block_delta",
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": "hel" },
            }),
        );
        assert!(first.contains("response.output_text.delta"));
        assert!(first.contains("\"delta\":\"hel\""));
        assert!(!first.contains("response.output_text.done"));
        sse.push_str(&first);

        let second = feed_event(
            &mut context,
            "content_block_delta",
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": "lo" },
            }),
        );
        assert_eq!(
            second.matches("event: response.output_text.delta").count(),
            1
        );
        assert!(second.contains("\"delta\":\"lo\""));
        sse.push_str(&second);
        sse.push_str(&feed_event(
            &mut context,
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ));
        sse.push_str(&feed_event(
            &mut context,
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": {
                    "output_tokens": 3,
                    "cache_read_input_tokens": 2,
                    "credit_usage": 0.25,
                    "credit_unit": "credit",
                    "credit_unit_plural": "credits"
                },
            }),
        ));
        sse.push_str(&feed_event(
            &mut context,
            "message_stop",
            json!({ "type": "message_stop" }),
        ));

        assert_eq!(sse.matches("event: response.output_text.delta").count(), 2);
        assert!(sse.contains("\"text\":\"hello\""));
        assert!(sse.contains("event: response.completed"));
        assert!(sse.contains("\"input_tokens\":9"));
        assert!(sse.contains("\"output_tokens\":3"));
        assert!(sse.contains("\"cached_tokens\":2"));
        assert!(sse.contains("\"credit_usage\":0.25"));
        let sequences = sequence_numbers(&sse);
        assert_eq!(sequences, (0..sequences.len() as i64).collect::<Vec<_>>());
    }

    #[test]
    fn streaming_tool_waits_for_complete_json_and_restores_custom_namespace() {
        let mut kinds = ToolKindMap::new();
        kinds.insert(
            "collaboration__apply_patch".into(),
            DeclaredTool {
                kind: DeclaredToolKind::Custom,
                name: "apply_patch".into(),
                namespace: Some("collaboration".into()),
            },
        );
        let mut context = ResponsesStreamContext::new(
            "gpt-5.6-sol".into(),
            kinds,
            ResponsesResponseConfig::default(),
        );
        context.initial_events();
        assert!(
            feed_event(
                &mut context,
                "content_block_start",
                json!({
                    "type": "content_block_start", "index": 1,
                    "content_block": {
                        "type": "tool_use", "id": "toolu_1",
                        "name": "collaboration__apply_patch", "input": {}
                    },
                }),
            )
            .is_empty()
        );
        assert!(
            feed_event(
                &mut context,
                "content_block_delta",
                json!({
                    "type": "content_block_delta", "index": 1,
                    "delta": { "type": "input_json_delta", "partial_json": "{\"input\":\"PATCH" },
                }),
            )
            .is_empty()
        );
        assert!(
            feed_event(
                &mut context,
                "content_block_delta",
                json!({
                    "type": "content_block_delta", "index": 1,
                    "delta": { "type": "input_json_delta", "partial_json": " BODY\"}" },
                }),
            )
            .is_empty()
        );
        let completed = feed_event(
            &mut context,
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 1 }),
        );
        assert!(completed.contains("response.custom_tool_call_input.delta"));
        assert!(completed.contains("response.custom_tool_call_input.done"));
        assert!(completed.contains("\"input\":\"PATCH BODY\""));
        assert!(completed.contains("\"name\":\"apply_patch\""));
        assert!(completed.contains("\"namespace\":\"collaboration\""));
        assert!(completed.contains("\"call_id\":\"toolu_1\""));
        assert!(!completed.contains("\"done_key\""));
    }

    #[test]
    fn streaming_error_maps_to_failed_without_completed() {
        let mut context = ResponsesStreamContext::new(
            "gpt-5.6-sol".into(),
            ToolKindMap::new(),
            ResponsesResponseConfig::default(),
        );
        let mut sse = event_text(context.initial_events());
        sse.push_str(&feed_event(
            &mut context,
            "error",
            json!({
                "type": "error",
                "error": { "type": "api_error", "message": "broken upstream" },
            }),
        ));
        sse.push_str(&event_text(context.finish()));
        assert!(sse.contains("event: response.failed"));
        assert!(sse.contains("broken upstream"));
        assert!(sse.contains("\"code\":\"api_error\""));
        assert!(!sse.contains("event: response.completed"));
    }

    #[test]
    fn streaming_usage_includes_uncached_created_and_read_cache_tokens() {
        let mut context = ResponsesStreamContext::new(
            "gpt-5.6-sol".into(),
            ToolKindMap::new(),
            ResponsesResponseConfig::default(),
        );
        context.update_usage(&json!({
            "input_tokens": 3,
            "cache_creation_input_tokens": 4,
            "cache_read_input_tokens": 7,
            "output_tokens": 5
        }));

        let usage = context.usage();
        assert_eq!(usage["input_tokens"], json!(14));
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], json!(7));
        assert_eq!(usage["output_tokens"], json!(5));
        assert_eq!(usage["total_tokens"], json!(19));
    }

    #[test]
    fn hosted_web_search_thinking_becomes_a_responses_reasoning_item() {
        let mut context = ResponsesStreamContext::new(
            "gpt-5.6-sol".into(),
            ToolKindMap::new(),
            ResponsesResponseConfig::default(),
        );
        let sse = feed_event(
            &mut context,
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 2},
                "kiro_thinking": "search reasoning"
            }),
        );

        assert!(sse.contains("event: response.reasoning_summary_text.delta"));
        assert!(sse.contains("event: response.reasoning_summary_text.done"));
        assert!(sse.contains("search reasoning"));
        assert_eq!(context.output.len(), 1);
        assert_eq!(context.output[0]["type"], json!("reasoning"));
    }

    #[test]
    fn hosted_web_search_reasoning_precedes_final_answer_item() {
        let mut context = ResponsesStreamContext::new(
            "gpt-5.6-sol".into(),
            ToolKindMap::new(),
            ResponsesResponseConfig::default(),
        );
        context.initial_events();
        let started = feed_event(
            &mut context,
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 2,
                "content_block": {"type": "text", "text": ""},
                "kiro_thinking": "search reasoning"
            }),
        );
        let answer = feed_event(
            &mut context,
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {"type": "text_delta", "text": "final answer"}
            }),
        );

        assert!(started.contains("response.reasoning_summary_text.done"));
        assert!(answer.contains("response.output_text.delta"));
        assert_eq!(context.next_output_index, 2);
        assert!(started.contains("\"output_index\":0"));
        assert!(answer.contains("\"output_index\":1"));
    }

    #[tokio::test]
    async fn message_stop_completes_without_waiting_for_upstream_eof() {
        let upstream = stream::iter([Ok::<Bytes, Infallible>(Bytes::from_static(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ))])
        .chain(stream::pending());
        let response = responses_streaming_response(
            Body::from_stream(upstream),
            "gpt-5.6-sol".into(),
            ToolKindMap::new(),
            ResponsesResponseConfig::default(),
        );
        let mut body = response.into_body().into_data_stream();

        let mut output = String::new();
        while !output.contains("event: response.completed") {
            let chunk = tokio::time::timeout(std::time::Duration::from_millis(100), body.next())
                .await
                .expect("response.completed must not wait for upstream EOF")
                .expect("stream must emit response.completed")
                .unwrap();
            output.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        let end = tokio::time::timeout(std::time::Duration::from_millis(100), body.next())
            .await
            .expect("translated stream must terminate after message_stop");
        assert!(end.is_none());
    }

    #[test]
    fn streaming_max_tokens_maps_to_incomplete() {
        let mut context = ResponsesStreamContext::new(
            "gpt-5.6-sol".into(),
            ToolKindMap::new(),
            ResponsesResponseConfig::default(),
        );
        context.initial_events();
        feed_event(
            &mut context,
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "max_tokens" },
                "usage": { "output_tokens": 12 },
            }),
        );
        let sse = feed_event(
            &mut context,
            "message_stop",
            json!({ "type": "message_stop" }),
        );
        assert!(sse.contains("event: response.incomplete"));
        assert!(sse.contains("\"reason\":\"max_output_tokens\""));
    }

    #[test]
    fn sse_parser_handles_chunk_boundaries_and_crlf() {
        let mut buffer = b"event: ping\r\ndata: {}\r\n\r".to_vec();
        assert!(take_sse_frames(&mut buffer).is_empty());
        buffer.extend_from_slice(b"\nevent: message_stop\n");
        let first = take_sse_frames(&mut buffer);
        assert_eq!(first.len(), 1);
        assert_eq!(parse_sse_frame(&first[0]).unwrap().unwrap().0, "ping");
        buffer.extend_from_slice(b"data: {\"type\":\"message_stop\"}\n\n");
        let second = take_sse_frames(&mut buffer);
        assert_eq!(second.len(), 1);
        assert_eq!(
            parse_sse_frame(&second[0]).unwrap().unwrap().0,
            "message_stop"
        );
    }

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
            .find(|l| l.starts_with("data: ") && l.contains("response.custom_tool_call_input.done"))
            .expect("custom input done event data line");
        assert!(input_done_line.contains("\"input\":\"PATCH BODY\""));
        // added 也必须带完整 input（codex 反序列化要求字段存在）
        let added_line = sse
            .lines()
            .find(|l| {
                l.starts_with("data: ")
                    && l.contains("custom_tool_call")
                    && l.contains("in_progress")
            })
            .expect("added event data line");
        assert!(
            added_line.contains("PATCH BODY"),
            "added item carries full input"
        );
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

    #[test]
    fn usage_maps_cached_subset_in_json_and_completed_sse() {
        let anthropic = json!({
            "content": [],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 3,
                "cache_creation_input_tokens": 4,
                "cache_read_input_tokens": 7,
                "output_tokens": 5
            }
        });
        let p = parse_anthropic_message(&anthropic, "gpt-5.6-sol");
        let view = build_view(&p, &ToolKindMap::new());

        assert_eq!(view.usage["input_tokens"], json!(14));
        assert_eq!(
            view.usage["input_tokens_details"]["cached_tokens"],
            json!(7)
        );
        assert_eq!(view.usage["output_tokens"], json!(5));
        assert_eq!(view.usage["total_tokens"], json!(19));

        let sse = build_responses_sse(&p, &ToolKindMap::new());
        let completed = sse
            .lines()
            .find(|line| line.starts_with("data: ") && line.contains("response.completed"))
            .expect("response.completed data line");
        let event: Value = serde_json::from_str(completed.trim_start_matches("data: ")).unwrap();
        assert_eq!(event["response"]["usage"], view.usage);
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
