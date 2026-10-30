//! Anthropic → Kiro 协议转换器
//!
//! 负责将 Anthropic API 请求格式转换为 Kiro API 请求格式

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::kiro::model::requests::conversation::{
    AssistantMessage, ConversationState, CurrentMessage, HistoryAssistantMessage,
    HistoryUserMessage, KiroImage, Message, UserInputMessage, UserInputMessageContext, UserMessage,
};
use crate::kiro::model::requests::kiro::{
    AdditionalModelRequestFields, KiroOutputConfig, KiroReasoningConfig,
};
use crate::kiro::model::requests::tool::{
    InputSchema, Tool, ToolResult, ToolSpecification, ToolUseEntry,
};
use crate::model::config::ToolCompatibilityMode;

use super::types::{ContentBlock, ImageSource, MessagesRequest};

use crate::image_resize::{ResizeConfig, maybe_shrink_image};

/// 规范化 JSON Schema，修复 MCP 工具定义中常见的类型问题
/// 规范化 JSON Schema，修复工具定义中常见的类型问题
///
/// 问题根源：Claude Code / MCP 工具定义使用 JSON Schema Draft 2020-12 语法（`$schema`、
/// `exclusiveMinimum` 为数字等），kiro CLI endpoint 仅接受 Draft 07 格式，
/// 不合规字段会导致 ValidationException "Improperly formed request."。
fn normalize_json_schema(schema: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = schema else {
        return serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": true
        });
    };

    // 移除 $schema（kiro API 不接受此字段，且 Draft 2020-12 声明会触发校验失败）
    obj.remove("$schema");

    // 剥离顶层 oneOf/allOf/anyOf：Bedrock/Kiro 的 ToolInputSchema 不支持顶层组合关键字，
    // 否则返回 TOOL_SCHEMA_INVALID 400（常见于 Claude Code workflow 并行子代理携带的 MCP tools）。
    strip_top_level_combinators(&mut obj);

    // type 顶层必须是 "object"（Bedrock ToolInputSchema 硬约束）；非 object 一律强制修正。
    let current_type = obj
        .get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if current_type.as_deref() != Some("object") {
        if let Some(ref original) = current_type {
            tracing::warn!(
                original_type = %original,
                "tool inputSchema 顶层 type 不是 object，已强制修正（Bedrock 硬约束）"
            );
        }
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("object".to_string()),
        );
    }

    // properties（必须是 object）；递归规范化每个 property 的子 schema
    match obj.remove("properties") {
        Some(serde_json::Value::Object(props)) => {
            let normalized: serde_json::Map<String, serde_json::Value> = props
                .into_iter()
                .map(|(k, v)| (k, normalize_property_schema(v)))
                .collect();
            obj.insert("properties".to_string(), serde_json::Value::Object(normalized));
        }
        _ => { obj.insert("properties".to_string(), serde_json::Value::Object(serde_json::Map::new())); }
    }

    // required（必须是 string 数组）
    let required = match obj.remove("required") {
        Some(serde_json::Value::Array(arr)) => serde_json::Value::Array(
            arr.into_iter()
                .filter_map(|v| v.as_str().map(|s| serde_json::Value::String(s.to_string())))
                .collect(),
        ),
        _ => serde_json::Value::Array(Vec::new()),
    };
    obj.insert("required".to_string(), required);

    // additionalProperties（允许 bool 或 object，其他按 true 处理）
    match obj.get("additionalProperties") {
        Some(serde_json::Value::Bool(_)) | Some(serde_json::Value::Object(_)) => {}
        _ => { obj.insert("additionalProperties".to_string(), serde_json::Value::Bool(true)); }
    }

    serde_json::Value::Object(obj)
}

/// 剥离顶层 `oneOf`/`anyOf`/`allOf`，并尽量从 variant 中恢复语义字段。
///
/// Bedrock/Kiro 的 ToolInputSchema 不支持顶层组合关键字（AWS 文档 + Anthropic API 均如此）。
/// 若原 schema 没有 `properties`，则遍历各 combinator，取第一个 `type=object` 的 variant，
/// 提取其 `properties`/`required`/`additionalProperties`/`description`（避免退化成空对象丢掉全部入参）。
fn strip_top_level_combinators(obj: &mut serde_json::Map<String, serde_json::Value>) {
    let has_properties_initially = obj.contains_key("properties");

    for combinator in &["oneOf", "anyOf", "allOf"] {
        let Some(serde_json::Value::Array(variants)) = obj.remove(*combinator) else {
            continue;
        };

        // 原始 schema 已有 properties，或前一个 combinator 已提取过 → 纯剥离，不再恢复。
        if has_properties_initially || obj.contains_key("properties") {
            continue;
        }

        // 从 variants 中找第一个 type=object 的变体，提取关键字段。
        for variant in variants {
            let serde_json::Value::Object(m) = variant else {
                continue;
            };
            if m.get("type").and_then(|v| v.as_str()) != Some("object") {
                continue;
            }
            for key in &["properties", "required", "additionalProperties", "description"] {
                if let Some(val) = m.get(*key) {
                    obj.entry(key.to_string()).or_insert_with(|| val.clone());
                }
            }
            break;
        }
    }
}

/// 规范化 property 级别的子 schema（非顶层 inputSchema）
///
/// 处理 Draft 2020-12 特有字段，使其兼容 Draft 07：
/// - 移除 `$schema`
/// - `exclusiveMinimum`/`exclusiveMaximum` 为数字时（Draft 2019-09+）移除（Draft 07 仅支持 bool）
/// - `maximum`/`minimum` 超过 i32 范围时移除（部分 AWS validator 不接受超大整数约束）
fn normalize_property_schema(schema: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = schema else {
        return schema;
    };

    obj.remove("$schema");

    // exclusiveMinimum/exclusiveMaximum：Draft 2019-09+ 为数字，Draft 07 为 bool；移除数字形式
    if obj.get("exclusiveMinimum").and_then(|v| v.as_f64()).is_some() {
        obj.remove("exclusiveMinimum");
    }
    if obj.get("exclusiveMaximum").and_then(|v| v.as_f64()).is_some() {
        obj.remove("exclusiveMaximum");
    }

    // maximum/minimum 超过 i64::MAX 或为 JavaScript MAX_SAFE_INTEGER (9007199254740991) 时移除
    for key in &["maximum", "minimum"] {
        if let Some(v) = obj.get(*key).and_then(|v| v.as_f64()) {
            if v > 2_147_483_647.0 || v < -2_147_483_648.0 {
                obj.remove(*key);
            }
        }
    }

    // 递归处理嵌套 properties
    if let Some(serde_json::Value::Object(props)) = obj.remove("properties") {
        let normalized: serde_json::Map<String, serde_json::Value> = props
            .into_iter()
            .map(|(k, v)| (k, normalize_property_schema(v)))
            .collect();
        obj.insert("properties".to_string(), serde_json::Value::Object(normalized));
    }

    // 递归处理 items（数组元素 schema）
    if let Some(items) = obj.remove("items") {
        obj.insert("items".to_string(), normalize_property_schema(items));
    }

    serde_json::Value::Object(obj)
}

/// 追加到 Write 工具 description 末尾的内容
const WRITE_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the content to write exceeds 150 lines, you MUST only write the first 50 lines using this tool, then use `Edit` tool to append the remaining content in chunks of no more than 50 lines each. If needed, leave a unique placeholder to help append content. Do NOT attempt to write all content at once.";

/// 追加到 Edit 工具 description 末尾的内容
const EDIT_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: If the `new_string` content exceeds 50 lines, you MUST split it into multiple Edit calls, each replacing no more than 50 lines at a time. If used to append content, leave a unique placeholder to help append content. On the final chunk, do NOT include the placeholder.";

/// 追加到 Bash 工具 description 末尾的内容（上游可能在超大命令处截断）
const BASH_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: Do not send very large commands, inline scripts, or heredocs. If a command would exceed 100 lines or ~8000 characters, first create/modify a script file with chunked Write/Edit calls, then run a short command that executes it. Do not retry the same oversized command after a failure; split it smaller.";

/// 追加到系统提示词的分块写入策略。只要求遵守分块限制、不中途征求同意；
/// 不能要求"不说话"，否则会压掉客户端自己要求的进度说明。
const SYSTEM_CHUNKED_POLICY: &str = "\
When the Write or Edit tool has content size limits, always follow them. \
Never suggest bypassing these limits via alternative tools. \
Never ask the user whether to switch approaches. \
Complete all chunked operations without stopping to ask for confirmation.";

/// Kiro 请求没有 system 字段，客户端系统提示只能作为第一条 user 消息发出。
/// 用标签包住，让模型知道这段是客户端系统提示而不是用户发言。
const CLIENT_SYSTEM_PROMPT_OPEN: &str = "<client_system_prompt>";
const CLIENT_SYSTEM_PROMPT_CLOSE: &str = "</client_system_prompt>";

/// 上面那条 user 消息之后必须跟一条 assistant 消息（Kiro 历史要求一问一答）。
/// 只说明身份，不带任何风格要求。
const CLIENT_SYSTEM_PROMPT_ACK: &str =
    "Understood. I will follow the client system prompt above.";

/// 没有系统提示、只有 thinking 配置时的那对消息里的 assistant 一句。
const THINKING_CONFIG_ACK: &str = "Understood. I will use the thinking configuration above.";

/// 历史里的工具调用没有拿到结果（客户端中断等）时，网关补的 tool_result 内容。
const INTERRUPTED_TOOL_RESULT: &str =
    "No result was received for this tool call; it was interrupted before it returned.";

const MAX_MODEL_ID_LEN: usize = 256;

fn invalid_model_reason(model: &str) -> Option<&'static str> {
    if model.trim().is_empty() {
        Some("模型 ID 不能为空")
    } else if model.len() > MAX_MODEL_ID_LEN {
        Some("模型 ID 过长")
    } else if model.chars().any(char::is_control) {
        Some("模型 ID 不能包含控制字符")
    } else {
        None
    }
}

fn canonical_version(parts: &[&str]) -> Option<String> {
    let first = *parts.first()?;
    if parts.len() == 1
        && first.contains('.')
        && first
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    {
        return Some(first.to_string());
    }
    if !first.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    match parts {
        [_, second] if second.chars().all(|c| c.is_ascii_digit()) => {
            Some(format!("{}.{}", first, second))
        }
        [_] => Some(first.to_string()),
        _ => None,
    }
}

/// 规范化 Anthropic 客户端常见的 Claude ID，同时不猜测非 Claude 模型。
fn normalize_claude_model(model: &str) -> Option<String> {
    let mut normalized = model.to_ascii_lowercase();
    loop {
        let mut stripped_suffix = false;
        for suffix in ["-thinking", "-latest"] {
            if let Some(stripped) = normalized.strip_suffix(suffix) {
                normalized = stripped.to_string();
                stripped_suffix = true;
            }
        }
        if !stripped_suffix {
            break;
        }
    }
    if let Some((base, suffix)) = normalized.rsplit_once('-')
        && suffix.len() == 8
        && suffix.chars().all(|c| c.is_ascii_digit())
    {
        normalized = base.to_string();
    }

    let body = normalized.strip_prefix("claude-")?;
    const FAMILIES: [&str; 5] = ["sonnet", "opus", "haiku", "fable", "mythos"];

    for family in FAMILIES {
        if let Some(rest) = body.strip_prefix(family) {
            let rest = rest
                .strip_prefix('-')
                .or_else(|| rest.strip_prefix('.'))
                .unwrap_or(rest);
            let version_parts: Vec<&str> = rest.split('-').collect();
            let version = canonical_version(&version_parts)?;
            return Some(format!("claude-{}-{}", family, version));
        }
    }

    // 旧式日期 ID 把系列名放在版本之后，例如 claude-3-5-sonnet-20241022。
    let parts: Vec<&str> = body.split('-').collect();
    let family_index = parts.iter().position(|part| FAMILIES.contains(part))?;
    if family_index == 0 || family_index + 1 != parts.len() {
        return None;
    }
    let version = canonical_version(&parts[..family_index])?;
    Some(format!("claude-{}-{}", parts[family_index], version))
}

/// 模型映射：自定义别名优先，已知 Claude 格式规范化，其余合法 ID 原样透传。
pub fn map_model(model: &str) -> Option<String> {
    if invalid_model_reason(model).is_some() {
        return None;
    }

    // 自定义模型表优先（大小写不敏感精确匹配），可新增或覆盖内置映射。
    if let Some(custom) = crate::model::custom_models::lookup(model) {
        return Some(custom.backend_id);
    }

    normalize_claude_model(model).or_else(|| Some(model.to_string()))
}

/// 根据模型名称返回对应的上下文窗口大小
///
/// 复用 `map_model` 的映射逻辑，确保窗口大小判断与模型映射一致。
/// Kiro 于 2026-03-24 将 Opus 4.6 和 Sonnet 4.6 升级至 1M 上下文。
/// Sonnet 5 / Sonnet 5.5 / Opus 4.7 / 4.8 / Opus 5 / 5.5 / Haiku 5.5 同 1M
///
/// 注意：本函数的返回值会在 `Event::ContextUsage` 处被用来把上游只回报的
/// 百分比换算成 token 数（`pct × window / 100`）。漏配某个 1M 模型不会影响
/// 发往上游的请求，但会让该模型的 usage 上报缩小 5 倍，进而使客户端的
/// 上下文进度条与自动压缩阈值全部失准。新增 1M 模型时务必同步此处。
pub fn get_context_window_size(model: &str) -> i32 {
    // 自定义模型若显式声明了上下文窗口，优先返回。
    if let Some(custom) = crate::model::custom_models::lookup(model) {
        if let Some(window) = custom.context_window {
            return window;
        }
    }

    match map_model(model) {
        // Kiro exposes the GPT-5.6 family with a 1M context window.
        Some(mapped)
            if matches!(
                mapped.as_str(),
                "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna"
            ) =>
        {
            1_000_000
        }
        // Keep the existing fallback for other GPT models until their limits are confirmed.
        Some(mapped) if mapped.starts_with("gpt") => 272_000,
        Some(mapped)
            if mapped == "claude-sonnet-4.6"
                || mapped == "claude-sonnet-4.8"
                || mapped == "claude-sonnet-5"
                || mapped == "claude-sonnet-5.5"
                || mapped == "claude-opus-4.6"
                || mapped == "claude-opus-4.7"
                || mapped == "claude-opus-4.8"
                || mapped == "claude-opus-5"
                || mapped == "claude-opus-5.5"
                || mapped == "claude-haiku-5.5"
                || mapped == "claude-fable-5" =>
        {
            1_000_000
        }
        _ => 200_000,
    }
}

fn model_uses_gpt_reasoning_effort(model_id: &str) -> bool {
    matches!(
        model_id.to_ascii_lowercase().as_str(),
        "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna"
    )
}

/// 是否为已确认接受原生 reasoning effort 字段的模型。
fn model_supports_native_reasoning(model_id: &str) -> bool {
    if model_uses_gpt_reasoning_effort(model_id) {
        return true;
    }
    // 自定义模型可按 backend_id 声明支持 reasoning。
    if crate::model::custom_models::backend_supports_reasoning(model_id) {
        return true;
    }
    let m = model_id.to_ascii_lowercase();
    matches!(
        m.as_str(),
        "claude-opus-4.6" | "claude-opus-4.7" | "claude-opus-4.8" | "claude-sonnet-4.6"
    ) || m.contains("fable-5")
        || m.contains("mythos-5")
        || m.contains("sonnet-5")
        || m.contains("opus-5")
        || m.contains("haiku-5")
        || m.contains("claude-5")
}

/// 本次请求是否请求了原生 reasoning。
///
/// Opus 4.6 有历史约束：上游只在 **adaptive** thinking 下接受 `output_config`
/// （普通 enabled / 纯 effort 会 400），故单独判定；其余支持模型放宽为
/// 「thinking 启用（enabled/adaptive） **或** 显式 `output_config.effort`」即算请求。
fn native_reasoning_requested(req: &MessagesRequest, model_id: &str) -> bool {
    if model_id == "claude-opus-4.6" {
        return req
            .thinking
            .as_ref()
            .is_some_and(|t| t.thinking_type == "adaptive");
    }
    req.thinking.as_ref().is_some_and(|t| t.is_enabled())
        || req
            .output_config
            .as_ref()
            .is_some_and(|oc| !oc.effort.trim().is_empty())
}

/// 由 Anthropic `thinking.budget_tokens` 推导 effort 档位。
///
/// 当客户端只发标准 `thinking:{type:"enabled",budget_tokens:N}`、不带 `output_config`
/// 时，用它把「思考预算」映射到 Kiro 的 effort。（本项目 budget_tokens 上限 24576，
/// 故经此推导实际最高到 `high`；`xhigh` 仍需客户端显式 `output_config.effort`。）
fn effort_from_budget_tokens(tokens: i32) -> &'static str {
    match tokens {
        i32::MIN..=4_000 => "low",
        4_001..=16_000 => "medium",
        16_001..=64_000 => "high",
        _ => "xhigh",
    }
}

/// 选定最终下发的 effort：优先显式 `output_config.effort`；否则据 `budget_tokens`
/// 推导；再统一过 [`normalize_effort_for_model`]（按模型把 xhigh 安全降级等）。
fn select_native_reasoning_effort(req: &MessagesRequest, model_id: &str) -> String {
    let raw = req
        .output_config
        .as_ref()
        .map(|oc| oc.effort.trim().to_string())
        .filter(|e| !e.is_empty())
        .or_else(|| {
            req.thinking
                .as_ref()
                .filter(|t| t.is_enabled())
                .map(|t| effort_from_budget_tokens(t.budget_tokens).to_string())
        })
        .unwrap_or_else(|| "high".to_string());
    normalize_effort_for_model(model_id, &raw).unwrap_or_else(|| "high".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffortTier {
    None,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl EffortTier {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x-high" | "x_high" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

fn normalize_effort_for_model(model_id: &str, raw_effort: &str) -> Option<String> {
    let trimmed = raw_effort.trim();
    if trimmed.is_empty() {
        return None;
    }

    let requested = match EffortTier::parse(trimmed) {
        Some(tier) => tier,
        None => {
            tracing::debug!(
                model_id = %model_id,
                effort = %trimmed,
                fallback_effort = EffortTier::High.as_str(),
                "falling back unsupported output_config.effort"
            );
            return Some(EffortTier::High.as_str().to_string());
        }
    };

    // `xhigh` is a newer effort tier. Known older effort-capable models reject
    // it with `Invalid additionalModelRequestFields`, so map to the nearest
    // lower tier instead of failing the request. Unknown/future models keep
    // recognized values intact to avoid maintaining a brittle full allow-list.
    let normalized = if requested == EffortTier::None
        && !model_uses_gpt_reasoning_effort(model_id)
    {
        EffortTier::High
    } else if requested == EffortTier::XHigh && !model_supports_xhigh_effort(model_id) {
        EffortTier::High
    } else {
        requested
    };
    if normalized != requested || normalized.as_str() != trimmed {
        tracing::debug!(
            model_id = %model_id,
            effort = %trimmed,
            normalized_effort = normalized.as_str(),
            "normalized output_config.effort for model"
        );
    }

    Some(normalized.as_str().to_string())
}

fn model_supports_xhigh_effort(model_id: &str) -> bool {
    let model = model_id.to_ascii_lowercase();

    // Anthropic documents xhigh for Opus 4.7/4.8, Fable 5, and Mythos 5.
    if model.contains("opus-4.7")
        || model.contains("opus-4.8")
        || model.contains("fable-5")
        || model.contains("mythos-5")
        || model.contains("claude-5")
    {
        return true;
    }

    // Known Kiro/Claude model ids that predate xhigh. Keep this as a compact
    // deny-list, not a full capability matrix.
    !matches!(
        model.as_str(),
        "claude-opus-4.6"
            | "claude-sonnet-4.6"
            | "claude-opus-4.5"
            | "claude-sonnet-4.5"
            | "claude-haiku-4.5"
    )
}

fn build_additional_model_request_fields(
    req: &MessagesRequest,
    model_id: &str,
) -> Option<AdditionalModelRequestFields> {
    // 显式关闭 thinking：不下发任何 reasoning 字段。
    if req
        .thinking
        .as_ref()
        .is_some_and(|t| t.thinking_type == "disabled")
    {
        return None;
    }

    // 仅对确认支持 effort 的模型下发，避免上游 schema 校验 400。
    if !model_supports_native_reasoning(model_id) {
        if let Some(oc) = &req.output_config
            && !oc.effort.trim().is_empty()
        {
            tracing::debug!(
                model_id = %model_id,
                "skipping unsupported reasoning effort for model"
            );
        }
        return None;
    }

    // 需要客户端确实请求了 reasoning（thinking 启用或显式 effort；opus 4.6 需 adaptive）。
    if !native_reasoning_requested(req, model_id) {
        return None;
    }

    let effort = select_native_reasoning_effort(req, model_id);
    if model_uses_gpt_reasoning_effort(model_id) {
        Some(AdditionalModelRequestFields {
            output_config: None,
            reasoning: Some(KiroReasoningConfig { effort }),
        })
    } else {
        Some(AdditionalModelRequestFields {
            output_config: Some(KiroOutputConfig { effort }),
            reasoning: None,
        })
    }
}

/// 转换结果
#[derive(Debug)]
pub struct ConversionResult {
    /// 转换后的 Kiro 请求
    pub conversation_state: ConversationState,
    /// 工具名称映射（短名称 → 原始名称），仅当存在超长工具名时非空
    pub tool_name_map: HashMap<String, String>,
    /// 本次请求声明的所有工具名（原始 client 名）。用于 `<invoke>` 文本容错的灾难兜底：
    /// 只有合成出的工具名在此集合里，才允许把字面 `<invoke>` 捞回成结构化 tool_use；
    /// 否则当普通文本吐出，避免把「正文展示的工具调用」误执行成真命令。
    pub known_tool_names: std::collections::HashSet<String>,
    /// Additional model request fields (including `output_config.effort`), translated from the
    /// `output_config` field of the client's Anthropic request. Not sent when empty.
    pub additional_model_request_fields: Option<AdditionalModelRequestFields>,
}

/// Internal conversion purpose. Clients cannot select this directly; the
/// Responses adapter uses compact modes only after validating a terminal
/// `compaction_trigger` item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConversionPurpose {
    Generate,
    Compact,
    CompactFallback,
}

const COMPACTION_HISTORY_TOOL_NAME: &str = "kiro_compaction_history_tool";

/// 转换错误
#[derive(Debug)]
pub enum ConversionError {
    InvalidModel(String),
    EmptyMessages,
    InvalidMessageSequence(String),
    /// Claude Code 工具无法映射到 Kiro 内置工具（如内置缺 schema）。
    UnsupportedToolMapping(String),
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversionError::InvalidModel(reason) => write!(f, "无效模型 ID: {}", reason),
            ConversionError::EmptyMessages => write!(f, "消息列表为空"),
            ConversionError::InvalidMessageSequence(reason) => {
                write!(f, "消息序列无效: {}", reason)
            }
            ConversionError::UnsupportedToolMapping(reason) => {
                write!(f, "工具映射不支持: {}", reason)
            }
        }
    }
}

impl std::error::Error for ConversionError {}

/// 从 metadata.user_id 中提取 session UUID
///
/// 支持两种格式:
/// 1. 字符串格式: user_xxx_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705
/// 2. JSON 格式: {"device_id":"...","account_uuid":"...","session_id":"UUID"}
///
/// 提取 session UUID 作为 conversationId
fn extract_session_id(user_id: &str) -> Option<String> {
    // 先尝试 JSON 解析
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id) {
        if let Some(session_id) = json.get("session_id").and_then(|v| v.as_str()) {
            if is_valid_uuid(session_id) {
                return Some(session_id.to_string());
            }
        }
    }

    // 回退到字符串格式: 查找 "session_" 后面的内容
    if let Some(pos) = user_id.find("session_") {
        let session_part = &user_id[pos + 8..]; // "session_" 长度为 8
        if session_part.len() >= 36 {
            let uuid_str = &session_part[..36];
            if is_valid_uuid(uuid_str) {
                return Some(uuid_str.to_string());
            }
        }
    }
    None
}

/// 验证 36 字符的连字符 UUID 格式
fn is_valid_uuid(s: &str) -> bool {
    s.len() == 36 && Uuid::parse_str(s).is_ok()
}

/// 请求没有会话 id 时，按内容派生稳定的 conversationId：系统提示 + 第一条 user 消息。
/// 两者在同一会话的后续轮次里不变，所以每轮得到同一个 id；只取文本和图片数据，
/// 不受客户端每轮挪动 cache_control 断点的影响。
fn derive_conversation_id(req: &MessagesRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"kiro-rs.derived-conversation.v1\0");
    for s in req.system.iter().flatten() {
        hasher.update(s.text.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(b"\x01");
    if let Some(first) = req.messages.iter().find(|m| m.role == "user") {
        match &first.content {
            // 纯字符串按「一个 text 块」计：pi-ai 等客户端第一轮把首条消息转成带 cache_control
            // 的块数组，之后几轮又发成字符串，两种写法必须得到同一个 id。
            serde_json::Value::String(s) => {
                hasher.update(s.as_bytes());
                hasher.update(b"\0");
            }
            serde_json::Value::Array(blocks) => {
                for block in blocks {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        hasher.update(text.as_bytes());
                    } else if let Some(data) =
                        block.pointer("/source/data").and_then(|v| v.as_str())
                    {
                        hasher.update(data.as_bytes());
                    } else if let Some(t) = block.get("type").and_then(|v| v.as_str()) {
                        hasher.update(t.as_bytes());
                    }
                    hasher.update(b"\0");
                }
            }
            _ => {}
        }
    }
    let digest = format!("{:x}", hasher.finalize());
    Uuid::new_v5(&Uuid::NAMESPACE_OID, digest.as_bytes()).to_string()
}

/// 由 conversationId 确定性派生 agentContinuationId（UUID v5，命名空间固定）。
/// 同一会话在进程重启、多副本之间都得到同一个值。
fn derive_agent_continuation_id(conversation_id: &str) -> String {
    let name = format!("agent-continuation:{conversation_id}");
    Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes()).to_string()
}

/// 收集历史消息中使用的所有工具名称
fn collect_history_tool_names(history: &[Message]) -> Vec<String> {
    let mut tool_names = Vec::new();

    for msg in history {
        if let Message::Assistant(assistant_msg) = msg {
            if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                for tool_use in tool_uses {
                    if !tool_names.contains(&tool_use.name) {
                        tool_names.push(tool_use.name.clone());
                    }
                }
            }
        }
    }

    tool_names
}

/// 为历史中使用但不在 tools 列表中的工具创建占位符定义
/// Kiro API 要求：历史消息中引用的工具必须在 currentMessage.tools 中有定义
fn create_placeholder_tool(name: &str) -> Tool {
    Tool {
        tool_specification: ToolSpecification {
            name: name.to_string(),
            description: "Unavailable: this tool appeared earlier in the conversation but is not available now. Do not call it.".to_string(),
            input_schema: InputSchema::from_json(serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": true
            })),
        },
    }
}

fn create_compaction_history_tool() -> Tool {
    let mut tool = create_placeholder_tool(COMPACTION_HISTORY_TOOL_NAME);
    tool.tool_specification.description =
        "Inert placeholder for structured tool calls in compacted history. Do not call it."
            .to_string();
    tool
}

fn sorted_ids(ids: &std::collections::HashSet<String>) -> Vec<String> {
    let mut ids = ids.iter().cloned().collect::<Vec<_>>();
    ids.sort();
    ids
}

/// 将 Anthropic 请求转换为 Kiro 请求
/// 便捷入口（测试用）：默认按 ClaudeCode 模式转换。
#[cfg(test)]
pub fn convert_request(req: &MessagesRequest) -> Result<ConversionResult, ConversionError> {
    convert_request_with_mode(req, ToolCompatibilityMode::ClaudeCode)
}

pub fn convert_request_with_mode(
    req: &MessagesRequest,
    tool_compatibility_mode: ToolCompatibilityMode,
) -> Result<ConversionResult, ConversionError> {
    convert_request_with_purpose(
        req,
        tool_compatibility_mode,
        ConversionPurpose::Generate,
    )
}

pub(crate) fn convert_request_with_purpose(
    req: &MessagesRequest,
    tool_compatibility_mode: ToolCompatibilityMode,
    purpose: ConversionPurpose,
) -> Result<ConversionResult, ConversionError> {
    // 1. 映射模型
    let model_id = map_model(&req.model).ok_or_else(|| {
        ConversionError::InvalidModel(
            invalid_model_reason(&req.model)
                .unwrap_or("模型 ID 无效")
                .to_string(),
        )
    })?;

    // 2. 检查消息列表
    if req.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    // 2.4. 规范化 role=system 的消息（Kiro 历史只有 user/assistant 两种角色）。
    // 有文本的变成一条用 <system-reminder> 包住的 user 消息，留在原位置，随后和相邻的
    // user 消息合并；只带 output_config 的空 system 消息丢掉；其他角色报错。
    // 必须在 prefill 截断和末尾合并之前做，否则末尾的 system 会被当成 prefill 截掉。
    let normalized_messages = normalize_system_messages(&req.messages)?;
    let source: &[super::types::Message] = normalized_messages
        .as_deref()
        .unwrap_or(req.messages.as_slice());
    if source.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    // 2.5. 预处理 prefill：如果末尾是 assistant，丢弃并截断到最后一条 user。
    // Claude 4.x 已弃用 assistant prefill，Kiro API 也不支持续写，只能从最后一条 user 重新生成。
    let messages: &[_] = if source.last().is_some_and(|m| m.role == "assistant") {
        let last_user_idx = source
            .iter()
            .rposition(|m| m.role == "user")
            .ok_or(ConversionError::EmptyMessages)?;
        tracing::warn!(
            dropped_messages = source.len() - last_user_idx - 1,
            "末尾是 assistant 消息（prefill），Kiro 不支持续写，已丢弃并从最后一条 user 重新生成"
        );
        &source[..=last_user_idx]
    } else {
        source
    };

    // 2.6. 末尾连续多条 user（典型：tool_result 之后紧跟一条客户端提醒）合并成一条当前消息。
    // 不合并时 build_history 会把倒数第二条 user 放进历史，再补一条假的 assistant "OK"；
    // 模型把这条 "OK" 当成自己说过的话，会在工具结果之后学着只回 "OK"。
    let merged_tail: Vec<super::types::Message>;
    let messages: &[_] = match purpose {
        ConversionPurpose::Generate => match merge_trailing_user_messages(messages) {
            Some(merged) => {
                merged_tail = merged;
                &merged_tail
            }
            None => messages,
        },
        _ => messages,
    };

    // 3. 生成会话 ID 和代理 ID
    // 优先从 metadata.user_id 中提取 session UUID 作为 conversationId
    // 没有会话 id 时按内容派生稳定 id（系统提示 + 第一条 user 消息），不再每次随机：
    // 随机 id 让按会话固定凭据的粘性路由永远不命中。
    let conversation_id = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_ref())
        .and_then(|user_id| extract_session_id(user_id))
        .unwrap_or_else(|| derive_conversation_id(req));
    // agentContinuationId 从 conversationId 确定性派生，让同一会话的请求体在会话 id 之外
    // 完全一致。实测上游 prompt cache 按内容前缀匹配、与会话 id 无关，所以这一步对缓存
    // 没有直接收益；保留确定性只是为了请求可复现、便于排查。
    // UUID v5 保持与此前随机 v4 相同的 36 字符格式。
    let agent_continuation_id = derive_agent_continuation_id(&conversation_id);

    // 4. 确定触发类型
    let chat_trigger_type = determine_chat_trigger_type(req);

    // 6. 转换工具定义（超长名称自动缩短并记录映射；ClaudeCode 模式做内置工具适配）
    let mut tool_name_map = HashMap::new();
    let fallback = purpose == ConversionPurpose::CompactFallback;
    let mut tools = if fallback {
        Vec::new()
    } else {
        convert_tools(&req.tools, &mut tool_name_map, tool_compatibility_mode)?
    };

    // 收集本次请求声明的所有工具名（原始 client 名），供 `<invoke>` 容错的工具表校验。
    let mut known_tool_names: std::collections::HashSet<String> = if fallback {
        std::collections::HashSet::new()
    } else {
        req.tools
            .as_ref()
            .map(|ts| ts.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default()
    };
    // 建议3 修复：超长工具名（>63）会被 shorten 成短名发给上游，模型回吐的也是短名。
    // tool_name_map 的 key 正是这些短名，一并加入，避免「超长名工具的合法 invoke 被漏捞」。
    for short in tool_name_map.keys() {
        known_tool_names.insert(short.clone());
    }

    // 7. 构建历史消息（需要先构建，以便收集历史中使用的工具）。
    // 图片去重集合由历史和当前消息共用：同一张图只在第一次出现时发原图，
    // 每条消息第一次出现时就是最终形态，进了历史以后不再变（上游按内容前缀缓存）。
    let mut image_dedup = ImageDedup::new();
    let mut history = build_history(
        req,
        messages,
        &model_id,
        &mut tool_name_map,
        tool_compatibility_mode,
        purpose,
        &mut image_dedup,
    )?;

    // 5. 处理最后一条消息作为 current_message（经过 prefill 预处理，末尾必为 user）
    let last_message = messages.last().unwrap();
    let mut images = Vec::new();
    let (mut text_content, tool_results) =
        process_message_content(&last_message.content, &mut image_dedup, &mut images)?;

    // 8. 验证并过滤 tool_use/tool_result 配对
    let validated_tool_results = if purpose != ConversionPurpose::Generate {
        validate_compaction_tool_sequence(&history, &tool_results)?;
        tool_results.clone()
    } else {
        // 已在历史里配对过、这次内容又不同的 tool_result：配对校验会丢掉它，
        // 先把新内容并进当前消息正文，不悄悄丢。
        let resent = resent_tool_result_notes(&history, &tool_results);
        if !resent.is_empty() {
            let mut parts = resent;
            if !text_content.is_empty() {
                parts.push(std::mem::take(&mut text_content));
            }
            text_content = parts.join("\n");
        }
        let (mut validated_tool_results, orphaned_tool_use_ids) =
            validate_tool_pairing(&history, &tool_results);
        // 没拿到结果的工具调用保留在历史里，补一条写明"被中断、没有结果"的 tool_result
        // （Kiro 要求每个 tool_use 都有结果）。历史中段的补到紧跟它的那条 user 消息，
        // 最后一条 assistant 的补到当前消息。
        let current_synth = pair_orphaned_tool_uses(&mut history, &orphaned_tool_use_ids);
        validated_tool_results.extend(current_synth);
        validated_tool_results
    };

    // 10. 收集历史中使用的工具名称，为缺失的工具生成占位符定义
    // Kiro API 要求：历史消息中引用的工具必须在 tools 列表中有定义
    // 注意：Kiro 匹配工具名称时忽略大小写，所以这里也需要忽略大小写比较
    let history_tool_names = collect_history_tool_names(&history);
    let existing_tool_names: std::collections::HashSet<_> = tools
        .iter()
        .map(|t| t.tool_specification.name.to_lowercase())
        .collect();

    if fallback {
        if !history_tool_names.is_empty() {
            tools.push(create_compaction_history_tool());
            known_tool_names.insert(COMPACTION_HISTORY_TOOL_NAME.to_string());
        }
    } else {
        for tool_name in history_tool_names {
            if !existing_tool_names.contains(&tool_name.to_lowercase()) {
                tools.push(create_placeholder_tool(&tool_name));
            }
        }
    }

    // 11. 构建 UserInputMessageContext
    let mut context = UserInputMessageContext::new();
    if !tools.is_empty() {
        context = context.with_tools(tools);
    }
    if !validated_tool_results.is_empty() {
        context = context.with_tool_results(validated_tool_results);
    }

    // 12. 构建当前消息
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    let content = text_content;

    let mut user_input = UserInputMessage::new(content, &model_id)
        .with_context(context)
        .with_origin("AI_EDITOR");

    if !images.is_empty() {
        user_input = user_input.with_images(images);
    }

    let current_message = CurrentMessage::new(user_input);

    // 13. 构建 ConversationState
    let conversation_state = ConversationState::new(conversation_id)
        .with_agent_continuation_id(agent_continuation_id)
        .with_agent_task_type("vibe")
        .with_chat_trigger_type(chat_trigger_type)
        .with_current_message(current_message)
        .with_history(history);

    if !tool_name_map.is_empty() {
        tracing::info!(
            "工具名称映射: {} 个超长名称已缩短",
            tool_name_map.len()
        );
    }

    // 14. Extract effort into AdditionalModelRequestFields only for models that accept it.
    //
    // The system-prompt thinking prefix remains available for every thinking mode. The real
    // wire field is narrower: newer/non-adaptive models reject it with
    // `additionalModelRequestFields is not supported for this model`, so keep the field opt-in
    // by upstream model capability rather than by the mere presence of client output_config.
    let additional_model_request_fields = build_additional_model_request_fields(req, &model_id);

    let upstream_effort = additional_model_request_fields.as_ref().and_then(|f| {
        f.output_config
            .as_ref()
            .map(|c| c.effort.as_str())
            .or_else(|| f.reasoning.as_ref().map(|r| r.effort.as_str()))
    });
    tracing::info!(
        model_id = %model_id,
        thinking_type = req.thinking.as_ref().map(|t| t.thinking_type.as_str()).unwrap_or("-"),
        budget_tokens = req.thinking.as_ref().map(|t| t.budget_tokens).unwrap_or(0),
        client_effort = req.output_config.as_ref().map(|c| c.effort.as_str()).unwrap_or("-"),
        upstream_effort = upstream_effort.unwrap_or("-"),
        "thinking 配置"
    );

    Ok(ConversionResult {
        conversation_state,
        tool_name_map,
        known_tool_names,
        additional_model_request_fields,
    })
}

/// 确定聊天触发类型
/// "AUTO" 模式可能会导致 400 Bad Request 错误
fn determine_chat_trigger_type(_req: &MessagesRequest) -> String {
    "MANUAL".to_string()
}

/// 图片去重表：图片 base64 的 SHA256 → 第一次出现的位置说明（用于占位文字）。
/// 历史和当前消息共用一张表，按消息顺序处理，所以表里记的总是更早的那次。
type ImageDedup = HashMap<String, String>;

/// 一个图片块的处理结果
enum ImageOutcome {
    /// 原图已放进本条消息的 images 列表，值是 1 起的序号
    Attached(usize),
    /// 和前面某张图完全相同，没有重发，值是占位文字
    Omitted(String),
    /// Kiro 收不了这张图（url/file 来源、格式不支持），值是占位文字
    Unsupported(String),
}

/// 处理消息内容，提取文本、图片和工具结果。
///
/// 图片追加到 `images`（Kiro 只有消息级的 images 列表），按 `dedup` 去重：
/// 同一张图（base64 完全相同）只在第一次出现时发原图，之后换成占位文字。
fn process_message_content(
    content: &serde_json::Value,
    dedup: &mut ImageDedup,
    images: &mut Vec<KiroImage>,
) -> Result<(String, Vec<ToolResult>), ConversionError> {
    let mut text_parts = Vec::new();
    let mut tool_results = Vec::new();

    match content {
        serde_json::Value::String(s) => {
            text_parts.push(s.clone());
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                let block_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let block = match serde_json::from_value::<ContentBlock>(item.clone()) {
                    Ok(block) => block,
                    Err(e) => {
                        tracing::warn!(block_type, error = %e, "无法解析的内容块，留占位文字");
                        text_parts.push(unsupported_block_placeholder(item));
                        continue;
                    }
                };
                match block.block_type.as_str() {
                    "text" => {
                        if let Some(text) = block.text {
                            text_parts.push(text);
                        }
                    }
                    "image" => {
                        // 顶层图片正常附上时不加文字（保持既有消息内容不变），
                        // 没发出去的留占位，让模型知道这里有过一张图。
                        let outcome = match block.source {
                            Some(source) => {
                                extract_kiro_image(&source, "an earlier user message", dedup, images)
                            }
                            None => ImageOutcome::Unsupported(
                                "[image omitted: the image block has no source]".to_string(),
                            ),
                        };
                        match outcome {
                            ImageOutcome::Attached(_) => {}
                            ImageOutcome::Omitted(text) | ImageOutcome::Unsupported(text) => {
                                text_parts.push(text)
                            }
                        }
                    }
                    "tool_result" => {
                        if let Some(tool_use_id) = block.tool_use_id {
                            let result_content =
                                extract_tool_result_content(&block.content, &tool_use_id, dedup, images);
                            let is_error = block.is_error.unwrap_or(false);

                            let mut result = if is_error {
                                ToolResult::error(&tool_use_id, result_content)
                            } else {
                                ToolResult::success(&tool_use_id, result_content)
                            };
                            result.status =
                                Some(if is_error { "error" } else { "success" }.to_string());

                            tool_results.push(result);
                        }
                    }
                    // assistant 侧的块，在 assistant 消息里处理
                    "tool_use" | "thinking" | "redacted_thinking" => {}
                    _ => {
                        tracing::warn!(block_type, "Kiro 不支持的内容块，留占位文字");
                        text_parts.push(unsupported_block_placeholder(item));
                    }
                }
            }
        }
        _ => {}
    }

    Ok((text_parts.join("\n"), tool_results))
}

/// 丢弃的内容块留一句占位，让模型知道这里有过内容，不至于以为用户什么都没给。
fn unsupported_block_placeholder(item: &serde_json::Value) -> String {
    let block_type = item
        .get("type")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .unwrap_or("unknown");
    if block_type == "document" {
        if let Some(title) = item.get("title").and_then(|v| v.as_str()).filter(|t| !t.is_empty()) {
            return format!("[document \"{title}\" omitted: documents are not supported by this gateway]");
        }
        return "[document omitted: documents are not supported by this gateway]".to_string();
    }
    format!("[{block_type} content omitted: not supported by this gateway]")
}

/// 从 media_type 获取图片格式（大小写不敏感，image/jpg 按 jpeg 处理）
fn get_image_format(media_type: &str) -> Option<String> {
    match media_type.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => Some("jpeg".to_string()),
        "image/png" => Some("png".to_string()),
        "image/gif" => Some("gif".to_string()),
        "image/webp" => Some("webp".to_string()),
        _ => None,
    }
}

/// Converts an image block's source into a `KiroImage` and pushes it onto the top-level `images`.
///
/// Reuses the same conversion chain for every image (format validation + SHA256 dedup + resize + `from_base64`),
/// so an image inside a tool_result is lifted into the top-level images field the same way.
/// `origin` names where this image sits, recorded for later duplicates' placeholder text.
fn extract_kiro_image(
    source: &ImageSource,
    origin: &str,
    dedup: &mut ImageDedup,
    images: &mut Vec<KiroImage>,
) -> ImageOutcome {
    if source.data.is_empty() {
        let kind = if source.source_type.is_empty() {
            "unknown"
        } else {
            source.source_type.as_str()
        };
        tracing::warn!(source_type = kind, "图片没有 base64 数据（Kiro 只收 base64 图片），留占位文字");
        return ImageOutcome::Unsupported(format!(
            "[image omitted: {kind} image source is not supported by this gateway, only base64 images are]"
        ));
    }
    let Some(format) = get_image_format(&source.media_type) else {
        tracing::warn!(media_type = %source.media_type, "不支持的图片格式，留占位文字");
        return ImageOutcome::Unsupported(format!(
            "[image omitted: unsupported image format {}]",
            source.media_type
        ));
    };
    let mut hasher = Sha256::new();
    hasher.update(source.data.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    if let Some(first_seen) = dedup.get(&digest) {
        return ImageOutcome::Omitted(format!(
            "[image omitted: same image as in {first_seen} in this conversation]"
        ));
    }
    dedup.insert(digest, origin.to_string());
    let cfg = ResizeConfig::from_env();
    let processed = maybe_shrink_image(cfg, &format, &source.data);
    images.push(KiroImage::from_base64(processed.format, processed.data_base64));
    ImageOutcome::Attached(images.len())
}

/// 提取工具结果内容
///
/// Text elements stay as tool_result text; blocks with `type=="image"` are extracted into a `KiroImage`
/// and lifted to the top-level `images` (Amazon Q's `ToolResult` has no image field, so images can only go through the top-level channel).
/// The text records where each image went ("[image #N attached to this message]", N = position in the
/// message's images list) so the model can tell which tool produced which image.
fn extract_tool_result_content(
    content: &Option<serde_json::Value>,
    tool_use_id: &str,
    dedup: &mut ImageDedup,
    images: &mut Vec<KiroImage>,
) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let origin = format!("tool result {tool_use_id}");
            let mut parts = Vec::new();
            for item in arr {
                let item_type = item.get("type").and_then(|v| v.as_str());
                match item_type {
                    Some("image") => {
                        let outcome = match serde_json::from_value::<ContentBlock>(item.clone())
                            .ok()
                            .and_then(|b| b.source)
                        {
                            Some(source) => extract_kiro_image(&source, &origin, dedup, images),
                            None => ImageOutcome::Unsupported(
                                "[image omitted: the image block has no source]".to_string(),
                            ),
                        };
                        parts.push(match outcome {
                            ImageOutcome::Attached(n) => {
                                format!("[image #{n} attached to this message]")
                            }
                            ImageOutcome::Omitted(text) | ImageOutcome::Unsupported(text) => text,
                        });
                    }
                    Some("text") | None => {
                        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                    Some(other) => {
                        tracing::warn!(block_type = other, "tool_result 里有 Kiro 不支持的内容块，留占位文字");
                        parts.push(unsupported_block_placeholder(item));
                    }
                }
            }
            parts.join("\n")
        }
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// 验证并过滤 tool_use/tool_result 配对
///
/// 收集所有 tool_use_id，验证 tool_result 是否匹配
/// 静默跳过孤立的 tool_use 和 tool_result，输出警告日志
///
/// # Arguments
/// * `history` - 历史消息引用
/// * `tool_results` - 当前消息中的 tool_result 列表
///
/// # Returns
/// 元组：(经过验证和过滤后的 tool_result 列表, 孤立的 tool_use_id 集合)
fn validate_tool_pairing(
    history: &[Message],
    tool_results: &[ToolResult],
) -> (Vec<ToolResult>, std::collections::HashSet<String>) {
    use std::collections::HashSet;

    // 1. 收集所有历史中的 tool_use_id
    let mut all_tool_use_ids: HashSet<String> = HashSet::new();
    // 2. 收集历史中已经有 tool_result 的 tool_use_id
    let mut history_tool_result_ids: HashSet<String> = HashSet::new();

    for msg in history {
        match msg {
            Message::Assistant(assistant_msg) => {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    for tool_use in tool_uses {
                        all_tool_use_ids.insert(tool_use.tool_use_id.clone());
                    }
                }
            }
            Message::User(user_msg) => {
                // 收集历史 user 消息中的 tool_results
                for result in &user_msg
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    history_tool_result_ids.insert(result.tool_use_id.clone());
                }
            }
        }
    }

    // 3. 计算真正未配对的 tool_use_ids（排除历史中已配对的）
    let mut unpaired_tool_use_ids: HashSet<String> = all_tool_use_ids
        .difference(&history_tool_result_ids)
        .cloned()
        .collect();

    // 4. 过滤并验证当前消息的 tool_results
    let mut filtered_results = Vec::new();

    for result in tool_results {
        if unpaired_tool_use_ids.contains(&result.tool_use_id) {
            // 配对成功
            filtered_results.push(result.clone());
            unpaired_tool_use_ids.remove(&result.tool_use_id);
        } else if all_tool_use_ids.contains(&result.tool_use_id) {
            // tool_use 存在但已经在历史中配对过了，这是重复的 tool_result
            tracing::warn!(
                "跳过重复的 tool_result：该 tool_use 已在历史中配对，tool_use_id={}",
                result.tool_use_id
            );
        } else {
            // 孤立 tool_result - 找不到对应的 tool_use
            tracing::warn!(
                "跳过孤立的 tool_result：找不到对应的 tool_use，tool_use_id={}",
                result.tool_use_id
            );
        }
    }

    // 5. 检测真正孤立的 tool_use（有 tool_use 但在历史和当前消息中都没有 tool_result）
    for orphaned_id in &unpaired_tool_use_ids {
        tracing::warn!(
            "检测到孤立的 tool_use：找不到对应的 tool_result，将补一条中断结果，tool_use_id={}",
            orphaned_id
        );
    }

    (filtered_results, unpaired_tool_use_ids)
}

fn validate_compaction_tool_sequence(
    history: &[Message],
    current_tool_results: &[ToolResult],
) -> Result<(), ConversionError> {
    let mut pending = std::collections::HashSet::new();
    let mut seen = std::collections::HashSet::new();

    for message in history {
        match message {
            Message::Assistant(assistant) => {
                for tool_use in assistant
                    .assistant_response_message
                    .tool_uses
                    .iter()
                    .flatten()
                {
                    if !seen.insert(tool_use.tool_use_id.clone()) {
                        return Err(ConversionError::InvalidMessageSequence(format!(
                            "duplicate tool_use id in compaction history: {}",
                            tool_use.tool_use_id
                        )));
                    }
                    pending.insert(tool_use.tool_use_id.clone());
                }
            }
            Message::User(user) => {
                for result in &user
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    if !pending.remove(&result.tool_use_id) {
                        return Err(ConversionError::InvalidMessageSequence(format!(
                            "tool_result has no pending tool_use in compaction history: {}",
                            result.tool_use_id
                        )));
                    }
                }
            }
        }
    }

    for result in current_tool_results {
        if !pending.remove(&result.tool_use_id) {
            return Err(ConversionError::InvalidMessageSequence(format!(
                "current tool_result has no pending tool_use in compaction history: {}",
                result.tool_use_id
            )));
        }
    }

    if !pending.is_empty() {
        return Err(ConversionError::InvalidMessageSequence(format!(
            "compaction history contains tool_use without tool_result: {}",
            sorted_ids(&pending).join(", ")
        )));
    }
    Ok(())
}

/// 给没拿到结果的 tool_use 补一条 tool_result
///
/// Kiro API 要求每个 tool_use 必须有对应的 tool_result，否则返回 400 Bad Request。
/// 以前的做法是把这些 tool_use 从历史里删掉，模型就看不到自己真实发起过的调用。
/// 现在保留调用，在紧跟它的那条 user 消息里补一条 error 状态的 tool_result，
/// 内容如实写明调用被中断、没有结果。
///
/// 返回需要补到当前消息里的结果（孤立调用在历史最后一条 assistant 里时）。
fn pair_orphaned_tool_uses(
    history: &mut [Message],
    orphaned_ids: &std::collections::HashSet<String>,
) -> Vec<ToolResult> {
    if orphaned_ids.is_empty() {
        return Vec::new();
    }
    let mut handled: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut pending: Vec<ToolResult> = Vec::new();
    for msg in history.iter_mut() {
        match msg {
            Message::Assistant(assistant_msg) => {
                for tool_use in assistant_msg
                    .assistant_response_message
                    .tool_uses
                    .iter()
                    .flatten()
                {
                    if orphaned_ids.contains(&tool_use.tool_use_id)
                        && handled.insert(tool_use.tool_use_id.clone())
                    {
                        pending.push(ToolResult::error(
                            &tool_use.tool_use_id,
                            INTERRUPTED_TOOL_RESULT,
                        ));
                    }
                }
            }
            Message::User(user_msg) => {
                if !pending.is_empty() {
                    user_msg
                        .user_input_message
                        .user_input_message_context
                        .tool_results
                        .append(&mut pending);
                }
            }
        }
    }
    pending
}

/// 当前消息里重发、内容又和历史里那次不同的 tool_result。
///
/// 这类结果会被配对校验当作重复丢掉（同一个 tool_use 只能有一个结果）；
/// 把新内容整理成正文文字返回，调用方并进当前消息，不悄悄丢。内容相同的重发不返回。
fn resent_tool_result_notes(history: &[Message], tool_results: &[ToolResult]) -> Vec<String> {
    if tool_results.is_empty() {
        return Vec::new();
    }
    let mut earlier: HashMap<&str, String> = HashMap::new();
    for msg in history {
        if let Message::User(user_msg) = msg {
            for result in &user_msg
                .user_input_message
                .user_input_message_context
                .tool_results
            {
                earlier
                    .entry(result.tool_use_id.as_str())
                    .or_insert_with(|| tool_result_text(result));
            }
        }
    }
    tool_results
        .iter()
        .filter_map(|result| {
            let previous = earlier.get(result.tool_use_id.as_str())?;
            let text = tool_result_text(result);
            if *previous == text {
                return None;
            }
            tracing::warn!(
                tool_use_id = %result.tool_use_id,
                "tool_result 重发且内容不同：Kiro 每个 tool_use 只能配一个结果，新内容并进当前消息正文"
            );
            Some(format!(
                "[tool result for {} sent again with different content]\n{}",
                result.tool_use_id, text
            ))
        })
        .collect()
}

fn tool_result_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Kiro API 工具名称最大长度限制
const TOOL_NAME_MAX_LEN: usize = 63;

/// 生成确定性短名称：截断前缀 + "_" + 8 位 SHA256 hex
fn shorten_tool_name(name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    let hash_hex = format!("{:x}", hasher.finalize());
    let hash_suffix = &hash_hex[..8];
    // 54 prefix + 1 underscore + 8 hash = 63。上限按字节算（调用方按 name.len() 判断超长），
    // 截断位置取不超过 54 字节的最后一个字符边界，多字节名字缩短后也不超过 63 字节。
    let prefix_max = TOOL_NAME_MAX_LEN - 1 - 8;
    let mut cut = prefix_max.min(name.len());
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    let prefix = &name[..cut];
    format!("{}_{}", prefix, hash_suffix)
}

/// 如果名称超长则缩短，并记录映射（short → original）
fn map_tool_name(name: &str, tool_name_map: &mut HashMap<String, String>) -> String {
    if name.len() <= TOOL_NAME_MAX_LEN {
        return name.to_string();
    }
    let short = shorten_tool_name(name);
    tool_name_map.insert(short.clone(), name.to_string());
    short
}

/// 转换工具定义
/// Claude Code 内置工具名 → Kiro 内置工具名。
fn claude_code_tool_name_to_kiro(name: &str) -> Option<&'static str> {
    match name {
        "Write" => Some("fs_write"),
        "Edit" => Some("str_replace"),
        "Bash" => Some("execute_bash"),
        "Read" => Some("read_file"),
        "Glob" => Some("file_search"),
        "Grep" => Some("grep_search"),
        "LS" => Some("list_directory"),
        "WebSearch" => Some("web_search"),
        _ => None,
    }
}

fn is_claude_code_mode(mode: ToolCompatibilityMode) -> bool {
    mode == ToolCompatibilityMode::ClaudeCode
}

/// 出站工具名映射：ClaudeCode 模式命中内置则改名并记录 `kiro名 → 客户端名`；
/// 否则回退到长名缩短逻辑（map_tool_name）。
fn map_client_tool_name_to_kiro(
    name: &str,
    tool_name_map: &mut HashMap<String, String>,
    mode: ToolCompatibilityMode,
) -> String {
    if is_claude_code_mode(mode)
        && let Some(kiro_name) = claude_code_tool_name_to_kiro(name)
    {
        tool_name_map
            .entry(kiro_name.to_string())
            .or_insert_with(|| name.to_string());
        return kiro_name.to_string();
    }
    map_tool_name(name, tool_name_map)
}

fn optional_number(value: &serde_json::Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_u64().map(|v| v as i64))
}

fn take_first(
    obj: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<serde_json::Value> {
    keys.iter().find_map(|key| obj.get(*key).cloned())
}

fn maybe_insert(
    out: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<serde_json::Value>,
) {
    if let Some(value) = value
        && !value.is_null()
    {
        out.insert(key.to_string(), value);
    }
}

fn default_explanation(tool_name: &str) -> serde_json::Value {
    serde_json::Value::String(format!("Mapped from Claude Code {} tool.", tool_name))
}

/// 出站入参重写（Anthropic → Kiro 内置工具入参键）。Raw / 非内置直通。
fn map_tool_input_to_kiro(
    client_name: &str,
    input: serde_json::Value,
    mode: ToolCompatibilityMode,
) -> Result<serde_json::Value, ConversionError> {
    if !is_claude_code_mode(mode) {
        return Ok(input);
    }
    let Some(kiro_name) = claude_code_tool_name_to_kiro(client_name) else {
        return Ok(input);
    };
    let serde_json::Value::Object(obj) = input else {
        return Ok(input);
    };

    let mut out = serde_json::Map::new();
    match (client_name, kiro_name) {
        ("Write", "fs_write") => {
            maybe_insert(&mut out, "path", take_first(&obj, &["file_path", "path"]));
            maybe_insert(&mut out, "text", take_first(&obj, &["content", "text"]));
        }
        ("Edit", "str_replace") => {
            maybe_insert(&mut out, "path", take_first(&obj, &["file_path", "path"]));
            maybe_insert(&mut out, "oldStr", take_first(&obj, &["old_string", "oldStr"]));
            maybe_insert(&mut out, "newStr", take_first(&obj, &["new_string", "newStr"]));
        }
        ("Bash", "execute_bash") => {
            maybe_insert(&mut out, "command", take_first(&obj, &["command"]));
            maybe_insert(&mut out, "timeout", take_first(&obj, &["timeout"]));
        }
        ("Read", "read_file") => {
            // Claude Code 的 `pages`（PDF 页范围）在 Kiro read_file 中无对应字段。本函数只处理
            // 回放的历史 tool_use，客户端无法修正，拒绝会让整个会话永久 400；这里随白名单
            // 构造自然丢弃，Kiro 看到的只是一次普通读取历史。
            maybe_insert(&mut out, "path", take_first(&obj, &["file_path", "path"]));
            let offset = obj.get("offset").and_then(optional_number);
            let limit = obj.get("limit").and_then(optional_number);
            if let Some(start) = offset {
                out.insert("start_line".to_string(), serde_json::json!(start));
            }
            if let Some(limit) = limit {
                let end = offset.map(|start| start + limit - 1).unwrap_or(limit);
                out.insert("end_line".to_string(), serde_json::json!(end));
            }
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
            out.entry("explanation".to_string())
                .or_insert_with(|| default_explanation(client_name));
        }
        ("Glob", "file_search") => {
            maybe_insert(&mut out, "query", take_first(&obj, &["pattern", "query"]));
            maybe_insert(
                &mut out,
                "excludePattern",
                take_first(&obj, &["excludePattern", "exclude"]),
            );
            if let Some(v) = take_first(&obj, &["includeIgnoredFiles", "include_ignored"]) {
                let mapped = match v {
                    serde_json::Value::Bool(true) => serde_json::json!("yes"),
                    serde_json::Value::Bool(false) => serde_json::json!("no"),
                    other => other,
                };
                out.insert("includeIgnoredFiles".to_string(), mapped);
            }
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
            out.entry("explanation".to_string())
                .or_insert_with(|| default_explanation(client_name));
        }
        ("Grep", "grep_search") => {
            maybe_insert(&mut out, "query", take_first(&obj, &["pattern", "query"]));
            maybe_insert(
                &mut out,
                "includePattern",
                take_first(&obj, &["glob", "includePattern"]),
            );
            maybe_insert(
                &mut out,
                "excludePattern",
                take_first(&obj, &["excludePattern", "exclude"]),
            );
            maybe_insert(
                &mut out,
                "caseSensitive",
                take_first(&obj, &["caseSensitive", "case_sensitive"]),
            );
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
        }
        ("LS", "list_directory") => {
            maybe_insert(&mut out, "path", take_first(&obj, &["path"]));
            maybe_insert(&mut out, "depth", take_first(&obj, &["depth"]));
            maybe_insert(&mut out, "explanation", take_first(&obj, &["explanation"]));
            out.entry("explanation".to_string())
                .or_insert_with(|| default_explanation(client_name));
        }
        ("WebSearch", "web_search") => {
            maybe_insert(&mut out, "query", take_first(&obj, &["query"]));
        }
        _ => return Ok(serde_json::Value::Object(obj)),
    }
    Ok(serde_json::Value::Object(out))
}

/// 入站入参还原（Kiro 内置工具入参键 → Anthropic）。**以 Kiro 名匹配**，故自动只在
/// 出站确实映射过（ClaudeCode）时生效；Raw 模式 / 长名缩短 / 透传工具一律直通。
///
/// 这是相对参考实现的一处修正：参考以“客户端名”匹配，导致 Raw 模式下客户端自带的、
/// 恰好叫 `Read` 的工具入参也会被误改写。以 Kiro 名匹配避免了该误伤，且入站无需穿透 mode。
fn map_tool_input_from_kiro(kiro_name: &str, input: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(obj) = input else {
        return input;
    };
    let mut out = serde_json::Map::new();
    match kiro_name {
        "fs_write" => {
            maybe_insert(&mut out, "file_path", take_first(&obj, &["path", "file_path"]));
            maybe_insert(&mut out, "content", take_first(&obj, &["text", "content"]));
        }
        "str_replace" => {
            maybe_insert(&mut out, "file_path", take_first(&obj, &["path", "file_path"]));
            maybe_insert(
                &mut out,
                "old_string",
                take_first(&obj, &["oldStr", "old_string"]),
            );
            maybe_insert(
                &mut out,
                "new_string",
                take_first(&obj, &["newStr", "new_string"]),
            );
        }
        "execute_bash" => {
            maybe_insert(&mut out, "command", take_first(&obj, &["command"]));
            maybe_insert(&mut out, "timeout", take_first(&obj, &["timeout"]));
        }
        "read_file" => {
            maybe_insert(&mut out, "file_path", take_first(&obj, &["path", "file_path"]));
            let start = obj.get("start_line").and_then(optional_number);
            let end = obj.get("end_line").and_then(optional_number);
            if let Some(start) = start {
                out.insert("offset".to_string(), serde_json::json!(start));
            }
            if let Some(end) = end {
                let limit = start.map(|s| end - s + 1).unwrap_or(end);
                if limit > 0 {
                    out.insert("limit".to_string(), serde_json::json!(limit));
                }
            }
        }
        "file_search" => {
            maybe_insert(&mut out, "pattern", take_first(&obj, &["query", "pattern"]));
        }
        "grep_search" => {
            maybe_insert(&mut out, "pattern", take_first(&obj, &["query", "pattern"]));
            maybe_insert(&mut out, "glob", take_first(&obj, &["includePattern", "glob"]));
            maybe_insert(
                &mut out,
                "case_sensitive",
                take_first(&obj, &["caseSensitive", "case_sensitive"]),
            );
        }
        "list_directory" => {
            maybe_insert(&mut out, "path", take_first(&obj, &["path"]));
        }
        "web_search" => {
            maybe_insert(&mut out, "query", take_first(&obj, &["query"]));
        }
        _ => return serde_json::Value::Object(obj),
    }
    serde_json::Value::Object(out)
}

/// 入站还原工具名 + 入参给客户端。名字从 `tool_name_map`（kiro名→客户端名）还原。
///
/// 入参只对出站时确实做过内置名映射的工具反向重写（ClaudeCode 模式下 Write→fs_write 等，
/// 映射表里记着 `kiro名 → 客户端内置名`）。Raw 模式不做内置映射，客户端自己的
/// web_search / read_file 等工具入参原样返回；长名缩短的工具也只还原名字。
pub fn restore_tool_use_for_client(
    kiro_name: &str,
    input: serde_json::Value,
    tool_name_map: &HashMap<String, String>,
) -> (String, serde_json::Value) {
    let Some(client_name) = tool_name_map.get(kiro_name) else {
        return (kiro_name.to_string(), input);
    };
    let builtin_mapped = claude_code_tool_name_to_kiro(client_name) == Some(kiro_name);
    let client_input = if builtin_mapped {
        map_tool_input_from_kiro(kiro_name, input)
    } else {
        input
    };
    (client_name.clone(), client_input)
}

fn optional_schema(schema: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "anyOf": [schema, {"type": "null"}] })
}

/// Kiro 内置工具的描述（含防截断后缀）。
fn kiro_builtin_tool_description(kiro_name: &str, fallback: &str) -> String {
    match kiro_name {
        "fs_write" => format!(
            "Write text content to a file.\n{}",
            WRITE_TOOL_DESCRIPTION_SUFFIX
        ),
        "str_replace" => format!(
            "Replace an exact string in a file.\n{}",
            EDIT_TOOL_DESCRIPTION_SUFFIX
        ),
        "execute_bash" => format!(
            "Execute the specified bash command.\n{}",
            BASH_TOOL_DESCRIPTION_SUFFIX
        ),
        "read_file" => "Read a single file with optional line range specification.".to_string(),
        "file_search" => "Search for files by fuzzy file path query.".to_string(),
        "grep_search" => "Search file contents using a regex pattern.".to_string(),
        "list_directory" => "List directory contents.".to_string(),
        "web_search" => "Search the web for up-to-date information.".to_string(),
        _ if fallback.trim().is_empty() => kiro_name.to_string(),
        _ => fallback.to_string(),
    }
}

/// Kiro 内置工具的硬编码 input schema（Draft-07 子集）。
fn kiro_builtin_tool_schema(kiro_name: &str) -> Option<serde_json::Value> {
    Some(match kiro_name {
        "fs_write" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute path to file."},
                "text": {"type": "string", "description": "Contents to write into the file."}
            },
            "required": ["path", "text"],
            "additionalProperties": false
        }),
        "str_replace" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute path to file."},
                "oldStr": {"type": "string", "description": "Exact string to replace."},
                "newStr": {"type": "string", "description": "Replacement string."}
            },
            "required": ["path", "oldStr", "newStr"],
            "additionalProperties": false
        }),
        "execute_bash" => serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Bash command to execute."},
                "timeout": optional_schema(serde_json::json!({"type": "number", "description": "Optional timeout in milliseconds."}))
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        "read_file" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to file to read."},
                "start_line": optional_schema(serde_json::json!({"type": "number", "description": "Starting line number."})),
                "end_line": optional_schema(serde_json::json!({"type": "number", "description": "Ending line number."})),
                "explanation": {"type": "string", "description": "Why this file is being read."}
            },
            "required": ["path", "explanation"],
            "additionalProperties": false
        }),
        "file_search" => serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Fuzzy filename query."},
                "explanation": {"type": "string", "description": "Why this search is being performed."},
                "excludePattern": optional_schema(serde_json::json!({"type": "string", "description": "Glob pattern for files to exclude."})),
                "includeIgnoredFiles": optional_schema(serde_json::json!({"type": "string", "description": "Whether to include ignored files, yes or no."}))
            },
            "required": ["query", "explanation"],
            "additionalProperties": false
        }),
        "grep_search" => serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "description": "Regex pattern to search for."},
                "caseSensitive": optional_schema(serde_json::json!({"type": "boolean", "description": "Whether the search should be case sensitive."})),
                "includePattern": optional_schema(serde_json::json!({"type": "string", "description": "Glob pattern for files to include."})),
                "excludePattern": optional_schema(serde_json::json!({"type": "string", "description": "Glob pattern for files to exclude."})),
                "explanation": optional_schema(serde_json::json!({"type": "string", "description": "Why this search is being performed."}))
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        "list_directory" => serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to directory."},
                "depth": optional_schema(serde_json::json!({"type": "number", "description": "Depth of recursive listing."})),
                "explanation": {"type": "string", "description": "Why this directory is being listed."}
            },
            "required": ["path", "explanation"],
            "additionalProperties": false
        }),
        "web_search" => serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query."}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        _ => return None,
    })
}

fn convert_tools(
    tools: &Option<Vec<super::types::Tool>>,
    tool_name_map: &mut HashMap<String, String>,
    mode: ToolCompatibilityMode,
) -> Result<Vec<Tool>, ConversionError> {
    let Some(tools) = tools else {
        return Ok(Vec::new());
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    for t in tools {
        // ClaudeCode 模式隐藏 Kiro 不支持的 fs_append。
        if is_claude_code_mode(mode) && t.name == "fs_append" {
            tracing::debug!("Claude Code 兼容模式隐藏 fs_append 工具");
            continue;
        }

        let mapped_name = map_client_tool_name_to_kiro(&t.name, tool_name_map, mode);
        // 按小写名去重（Kiro 工具名大小写不敏感），首个出现者胜出。
        if !seen.insert(mapped_name.to_lowercase()) {
            tracing::warn!(
                tool = %t.name,
                mapped = %mapped_name,
                "工具名与前面的工具只差大小写（Kiro 不区分大小写），已跳过这个工具"
            );
            continue;
        }

        let is_builtin =
            is_claude_code_mode(mode) && claude_code_tool_name_to_kiro(&t.name).is_some();

        let description = if is_builtin {
            kiro_builtin_tool_description(&mapped_name, &t.description)
        } else {
            let mut description = t.description.clone();
            // 非内置（或 Raw 模式）：保留旧的 Write/Edit/Bash 后缀（按原始名匹配）。
            let suffix = match t.name.as_str() {
                "Write" => WRITE_TOOL_DESCRIPTION_SUFFIX,
                "Edit" => EDIT_TOOL_DESCRIPTION_SUFFIX,
                "Bash" => BASH_TOOL_DESCRIPTION_SUFFIX,
                _ => "",
            };
            if !suffix.is_empty() {
                description.push('\n');
                description.push_str(suffix);
            }
            // kiro API 不接受空描述，填充占位符
            if description.trim().is_empty() {
                t.name.clone()
            } else {
                description
            }
        };

        // 限制描述长度为 10000 字符（安全截断 UTF-8，单次遍历）
        let description = match description.char_indices().nth(10000) {
            Some((idx, _)) => {
                tracing::warn!(
                    tool = %t.name,
                    original_chars = description.chars().count(),
                    "工具描述超过 10000 字符，已截断"
                );
                description[..idx].to_string()
            }
            None => description,
        };

        let schema = if is_builtin {
            kiro_builtin_tool_schema(&mapped_name).ok_or_else(|| {
                ConversionError::UnsupportedToolMapping(format!(
                    "{} 无法映射到 Kiro 工具 schema",
                    t.name
                ))
            })?
        } else {
            normalize_json_schema(serde_json::json!(t.input_schema))
        };

        out.push(Tool {
            tool_specification: ToolSpecification {
                name: mapped_name,
                description,
                input_schema: InputSchema::from_json(schema),
            },
        });
    }

    Ok(out)
}

/// 生成thinking标签前缀
fn generate_thinking_prefix(req: &MessagesRequest, model_id: &str) -> Option<String> {
    if let Some(t) = &req.thinking {
        if t.thinking_type == "enabled" {
            return Some(format!(
                "<thinking_mode>enabled</thinking_mode><max_thinking_length>{}</max_thinking_length>",
                t.budget_tokens
            ));
        } else if t.thinking_type == "adaptive" {
            // 和原生字段用同一套选择逻辑（显式 effort > budget_tokens 推导 > high），
            // 否则只给 budget_tokens 时前缀写 high、原生字段写 low，两处互相矛盾。
            let effort = select_native_reasoning_effort(req, model_id);
            return Some(format!(
                "<thinking_mode>adaptive</thinking_mode><thinking_effort>{}</thinking_effort>",
                effort
            ));
        }
    }
    None
}

/// 检查内容是否已包含thinking标签
fn has_thinking_tags(content: &str) -> bool {
    content.contains("<thinking_mode>") || content.contains("<max_thinking_length>")
}

/// 请求里有没有带尺寸限制后缀的 Write/Edit/Bash 工具（按原始名匹配，与工具描述后缀的条件一致）
fn has_chunked_tools(req: &MessagesRequest) -> bool {
    req.tools.as_ref().is_some_and(|tools| {
        tools
            .iter()
            .any(|t| matches!(t.name.as_str(), "Write" | "Edit" | "Bash"))
    })
}

/// 把末尾连续的多条 user 消息按原顺序合并成一条；末尾只有一条 user 时返回 None。
fn merge_trailing_user_messages(
    messages: &[super::types::Message],
) -> Option<Vec<super::types::Message>> {
    let start = messages
        .iter()
        .rposition(|m| m.role != "user")
        .map_or(0, |i| i + 1);
    if messages.len().saturating_sub(start) < 2 {
        return None;
    }
    let mut blocks = Vec::new();
    for m in &messages[start..] {
        match &m.content {
            serde_json::Value::String(s) => {
                blocks.push(serde_json::json!({"type": "text", "text": s}))
            }
            serde_json::Value::Array(arr) => blocks.extend(arr.iter().cloned()),
            _ => {}
        }
    }
    let mut out = messages[..start].to_vec();
    out.push(super::types::Message {
        role: "user".to_string(),
        content: serde_json::Value::Array(blocks),
    });
    Some(out)
}

/// 把 role=system 的消息规范化成 Kiro 能表达的形式。没有 system 消息时返回 None（原样使用）。
///
/// - 有文本的 system 消息变成一条 user 消息，文本用 <system-reminder> 包住，留在原位置。
///   之后它和相邻的 user 消息合并（历史里由 build_history 合并，末尾由
///   merge_trailing_user_messages 合并），所以落在时间上紧挨着它的那条 user 消息里；
///   末尾的 system 消息成为当前消息的一部分，不会被当成 prefill 截掉，
///   也不改动最前面的系统提示（上游按前缀缓存，改它会让整段缓存失效）。
/// - 没有文本的 system 消息（例如只带 output_config）直接丢掉。
/// - user/assistant/system 以外的角色丢掉并记 warn 日志。Anthropic 官方会对这种请求报 400，
///   但网关是多个客户端共用的，修复前这类消息被静默丢弃、请求照常成功，改成报错会让
///   原本能用的客户端突然失败；记 warn 是为了不再"静默"。
fn normalize_system_messages(
    messages: &[super::types::Message],
) -> Result<Option<Vec<super::types::Message>>, ConversionError> {
    let known = |m: &super::types::Message| matches!(m.role.as_str(), "user" | "assistant" | "system");
    if messages.iter().all(|m| known(m) && m.role != "system") {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        if !known(m) {
            tracing::warn!(role = %m.role, "丢弃不支持的消息角色（只认 user/assistant/system）");
            continue;
        }
        if m.role != "system" {
            out.push(m.clone());
            continue;
        }
        let text = system_message_text(&m.content);
        if text.trim().is_empty() {
            tracing::debug!("丢弃没有文本的 system 消息");
            continue;
        }
        out.push(super::types::Message {
            role: "user".to_string(),
            content: serde_json::json!([{
                "type": "text",
                "text": format!("<system-reminder>\n{text}\n</system-reminder>")
            }]),
        });
    }
    Ok(Some(out))
}

/// system 消息的文本：字符串，或 content 数组里所有 text 块按换行拼接
fn system_message_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 上游模型是否把已结束回合的 thinking 留在上下文里。
///
/// 按 Anthropic 文档（extended thinking 的 "thinking block caching behavior" 一节）：
/// Opus 4.5 及以后的模型默认保留之前所有 thinking；更早的模型在出现非 tool_result 的
/// user 内容后忽略之前的 thinking。Sonnet 4.5、Haiku 4.5 早于 Opus 4.5，按旧规则；
/// Sonnet 4.6 及以后、5 系按新规则。非 Claude 模型（GPT 等）按旧规则：推理内容只在
/// 本回合的工具循环里有用。
fn model_keeps_prior_thinking(model_id: &str) -> bool {
    let id = model_id.trim().to_ascii_lowercase();
    let Some(rest) = id.strip_prefix("claude-") else {
        return false;
    };
    let (family, version) = rest.split_once('-').unwrap_or((rest, ""));
    let mut numbers = version
        .split(['.', '-'])
        .map_while(|part| part.parse::<u32>().ok());
    let major = numbers.next();
    let minor = numbers.next().unwrap_or(0);
    // claude-3-7-sonnet、claude-5 这类以代数开头的 id
    if let Ok(generation) = family.parse::<u32>() {
        return generation >= 5;
    }
    match (family, major) {
        ("opus", Some(m)) => (m, minor) >= (4, 5),
        ("sonnet", Some(m)) => (m, minor) >= (4, 6),
        ("haiku", Some(m)) => m >= 5,
        // fable / mythos 等 5 系以后的新家族
        (_, Some(m)) => m >= 5,
        (_, None) => false,
    }
}

/// 已结束回合里的 assistant 消息条数。
///
/// 回合在最后一条带非 tool_result 内容的 user 消息处开始；它之前的 assistant 消息都属于
/// 已结束的回合，之后的属于还在工具循环里的当前回合。按原始消息计算（system 消息不算
/// user 消息），规范化和合并不改变 assistant 消息的先后序号。
fn finished_assistant_turns(messages: &[super::types::Message]) -> usize {
    let starts_turn = |m: &super::types::Message| {
        m.role == "user"
            && match &m.content {
                serde_json::Value::String(s) => !s.trim().is_empty(),
                serde_json::Value::Array(blocks) => blocks.iter().any(|b| {
                    match b.get("type").and_then(|v| v.as_str()) {
                        Some("tool_result") => false,
                        Some("text") => b
                            .get("text")
                            .and_then(|v| v.as_str())
                            .is_some_and(|t| !t.trim().is_empty()),
                        _ => true,
                    }
                }),
                _ => false,
            }
    };
    match messages.iter().rposition(starts_turn) {
        Some(boundary) => messages[..boundary]
            .iter()
            .filter(|m| m.role == "assistant")
            .count(),
        None => 0,
    }
}

/// 构建历史消息
///
/// # Arguments
/// * `req` - 原始请求，用于读取 `system`、`thinking` 等配置字段
/// * `messages` - 经过 prefill 预处理的消息切片，末尾必定是 user 消息。
///   注意：该切片与 `req.messages` 可能不同（prefill 时会截断末尾的 assistant 消息），
///   调用方应始终使用此参数而非 `req.messages`。
/// * `model_id` - 已映射的 Kiro 模型 ID
fn build_history(
    req: &MessagesRequest,
    messages: &[super::types::Message],
    model_id: &str,
    tool_name_map: &mut HashMap<String, String>,
    mode: ToolCompatibilityMode,
    purpose: ConversionPurpose,
    image_dedup: &mut ImageDedup,
) -> Result<Vec<Message>, ConversionError> {
    let mut history = Vec::new();

    // 生成thinking前缀（如果需要）
    let thinking_prefix = generate_thinking_prefix(req, model_id);

    // 1. 处理系统消息：Kiro 请求没有 system 字段，作为第一对 user + assistant 发出。
    let system_content: String = req
        .system
        .iter()
        .flatten()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    if !system_content.trim().is_empty() {
        let mut parts: Vec<String> = Vec::new();
        // 注入thinking标签到最前面（如果需要且客户端系统提示里没有）
        if let Some(ref prefix) = thinking_prefix
            && !has_thinking_tags(&system_content)
        {
            parts.push(prefix.clone());
        }
        // 客户端系统提示用标签包住，和用户发言区分开
        parts.push(format!(
            "{CLIENT_SYSTEM_PROMPT_OPEN}\n{system_content}\n{CLIENT_SYSTEM_PROMPT_CLOSE}"
        ));
        // 分块写入策略只对带 Write/Edit/Bash 工具（Claude Code 风格）的客户端追加；
        // 它是网关加的话，放在客户端系统提示标签外面。
        if has_chunked_tools(req) {
            parts.push(SYSTEM_CHUNKED_POLICY.to_string());
        }
        history.push(Message::User(HistoryUserMessage::new(parts.join("\n"), model_id)));
        history.push(Message::Assistant(HistoryAssistantMessage::new(
            CLIENT_SYSTEM_PROMPT_ACK,
        )));
    } else if let Some(prefix) = thinking_prefix {
        // 没有系统提示（或系统提示只有空文本）但有thinking配置：单独插入一对
        history.push(Message::User(HistoryUserMessage::new(prefix, model_id)));
        history.push(Message::Assistant(HistoryAssistantMessage::new(
            THINKING_CONFIG_ACK,
        )));
    }

    // 2. 处理常规消息历史
    // 最后一条消息作为 currentMessage，不加入历史
    // 经过 prefill 预处理后，messages 末尾必定是 user，故直接截掉最后一条即可
    let history_end_index = messages.len().saturating_sub(1);

    // 已结束回合的 thinking 是否回放按模型区分（见 model_keeps_prior_thinking）：
    // 不保留的模型，序号小于 finished_turns 的 assistant 消息丢掉 thinking。
    let finished_turns = if model_keeps_prior_thinking(model_id) {
        0
    } else {
        finished_assistant_turns(&req.messages)
    };
    let mut assistant_ordinal = 0usize;
    let mut buffer_keeps_thinking = true;

    // 收集并配对消息
    let mut user_buffer: Vec<&super::types::Message> = Vec::new();
    let mut assistant_buffer: Vec<&super::types::Message> = Vec::new();

    for i in 0..history_end_index {
        let msg = &messages[i];

        if msg.role == "user" {
            // 先处理累积的 assistant 消息
            if !assistant_buffer.is_empty() {
                let merged = merge_assistant_messages(
                    &assistant_buffer,
                    tool_name_map,
                    mode,
                    purpose,
                    buffer_keeps_thinking,
                )?;
                history.push(Message::Assistant(merged));
                assistant_buffer.clear();
            }
            user_buffer.push(msg);
        } else if msg.role == "assistant" {
            // 先处理累积的 user 消息
            if !user_buffer.is_empty() {
                let merged_user = merge_user_messages(&user_buffer, model_id, image_dedup)?;
                history.push(Message::User(merged_user));
                user_buffer.clear();
            }
            // 连续多条 assistant 之间没有 user，必然在同一个回合里，按第一条判定
            if assistant_buffer.is_empty() {
                buffer_keeps_thinking = assistant_ordinal >= finished_turns;
            }
            assistant_ordinal += 1;
            // 累积 assistant 消息（支持连续多条）
            assistant_buffer.push(msg);
        }
    }

    // 处理末尾累积的 assistant 消息
    if !assistant_buffer.is_empty() {
        let merged = merge_assistant_messages(
            &assistant_buffer,
            tool_name_map,
            mode,
            purpose,
            buffer_keeps_thinking,
        )?;
        history.push(Message::Assistant(merged));
    }

    // 历史末尾不能是没回答的 user（Kiro 历史一问一答，当前消息又是 user）。
    // Generate 已在入口合并末尾连续 user，正常请求走不到这里；走到说明消息序列异常，
    // 报错，不伪造一条 assistant 回复。
    if !user_buffer.is_empty() {
        return Err(ConversionError::InvalidMessageSequence(
            if purpose == ConversionPurpose::Generate {
                "history ends with a user message that has no assistant reply".to_string()
            } else {
                "compaction would move an unanswered user message into history".to_string()
            },
        ));
    }

    Ok(history)
}

/// 合并多个 user 消息
fn merge_user_messages(
    messages: &[&super::types::Message],
    model_id: &str,
    dedup: &mut ImageDedup,
) -> Result<HistoryUserMessage, ConversionError> {
    let mut content_parts = Vec::new();
    // 图片按合并后的整条消息编号，tool_result 里的 "[image #N ...]" 指这里的第 N 张
    let mut all_images = Vec::new();
    let mut all_tool_results = Vec::new();

    for msg in messages {
        let (text, tool_results) = process_message_content(&msg.content, dedup, &mut all_images)?;
        if !text.is_empty() {
            content_parts.push(text);
        }
        all_tool_results.extend(tool_results);
    }

    let content = content_parts.join("\n");
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    let mut user_msg = UserMessage::new(&content, model_id);

    if !all_images.is_empty() {
        user_msg = user_msg.with_images(all_images);
    }

    if !all_tool_results.is_empty() {
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(all_tool_results);
        user_msg = user_msg.with_context(ctx);
    }

    Ok(HistoryUserMessage {
        user_input_message: user_msg,
    })
}

/// 转换 assistant 消息
///
/// Kiro 历史 assistant 消息只有正文和 toolUses，没有放 thinking 的字段，
/// 保留的 thinking 只能写成正文里的 <thinking> 标签。每段 thinking 单独成一段，
/// 和正文按原来的先后顺序排列，段与段之间空一行。
/// `keep_thinking` 为 false（已结束回合、模型不保留旧 thinking）时丢掉 thinking；
/// redacted_thinking 是加密内容，Kiro 无法回放，一律丢掉。
fn convert_assistant_message(
    msg: &super::types::Message,
    tool_name_map: &mut HashMap<String, String>,
    mode: ToolCompatibilityMode,
    purpose: ConversionPurpose,
    keep_thinking: bool,
) -> Result<HistoryAssistantMessage, ConversionError> {
    // 按原顺序排列的段落：<thinking>…</thinking> 或一段正文
    let mut segments: Vec<String> = Vec::new();
    // 连续的 text 块直接拼接（引用等会把一段话拆成多个 text 块）
    let mut text_run = String::new();
    let mut dropped_thinking = 0usize;
    let mut dropped_redacted = 0usize;
    let mut tool_uses = Vec::new();

    match &msg.content {
        serde_json::Value::String(s) => {
            text_run = s.clone();
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "thinking" => {
                            let Some(thinking) = block.thinking.filter(|t| !t.is_empty()) else {
                                continue;
                            };
                            if !text_run.is_empty() {
                                segments.push(std::mem::take(&mut text_run));
                            }
                            if keep_thinking {
                                segments.push(format!("<thinking>{thinking}</thinking>"));
                            } else {
                                dropped_thinking += 1;
                            }
                        }
                        "redacted_thinking" => {
                            if !text_run.is_empty() {
                                segments.push(std::mem::take(&mut text_run));
                            }
                            dropped_redacted += 1;
                        }
                        "text" => {
                            if let Some(text) = block.text {
                                text_run.push_str(&text);
                            }
                        }
                        "tool_use" => {
                            if let (Some(id), Some(name)) = (block.id, block.name) {
                                let input = block.input.unwrap_or(serde_json::json!({}));
                                let (mapped_name, input) = if purpose
                                    == ConversionPurpose::CompactFallback
                                {
                                    (COMPACTION_HISTORY_TOOL_NAME.to_string(), input)
                                } else {
                                    (
                                        map_client_tool_name_to_kiro(
                                            &name,
                                            tool_name_map,
                                            mode,
                                        ),
                                        map_tool_input_to_kiro(&name, input, mode)?,
                                    )
                                };
                                tool_uses
                                    .push(ToolUseEntry::new(id, mapped_name).with_input(input));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    if !text_run.is_empty() {
        segments.push(text_run);
    }
    if dropped_redacted > 0 {
        tracing::debug!(
            count = dropped_redacted,
            "丢弃历史 redacted_thinking：加密内容，Kiro 无法回放"
        );
    }
    if dropped_thinking > 0 {
        tracing::debug!(
            count = dropped_thinking,
            "丢弃已结束回合的 thinking：该模型不保留旧回合 thinking"
        );
    }

    // 注意: Kiro API 要求 content 字段不能为空，当只有 tool_use（或 thinking 被丢掉）时需要占位符
    let final_content = if segments.is_empty()
        && (!tool_uses.is_empty() || dropped_thinking > 0 || dropped_redacted > 0)
    {
        " ".to_string()
    } else {
        segments.join("\n\n")
    };

    let mut assistant = AssistantMessage::new(final_content);
    if !tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(tool_uses);
    }

    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

/// 合并多个连续的 assistant 消息为一条
/// 用于处理网络不稳定时产生的连续 assistant 消息（Issue #79）
fn merge_assistant_messages(
    messages: &[&super::types::Message],
    tool_name_map: &mut HashMap<String, String>,
    mode: ToolCompatibilityMode,
    purpose: ConversionPurpose,
    keep_thinking: bool,
) -> Result<HistoryAssistantMessage, ConversionError> {
    assert!(!messages.is_empty());
    if messages.len() == 1 {
        return convert_assistant_message(messages[0], tool_name_map, mode, purpose, keep_thinking);
    }

    let mut all_tool_uses: Vec<ToolUseEntry> = Vec::new();
    let mut content_parts: Vec<String> = Vec::new();

    for msg in messages {
        let converted =
            convert_assistant_message(msg, tool_name_map, mode, purpose, keep_thinking)?;
        let am = converted.assistant_response_message;
        if !am.content.trim().is_empty() {
            content_parts.push(am.content);
        }
        if let Some(tus) = am.tool_uses {
            all_tool_uses.extend(tus);
        }
    }

    let content = if content_parts.is_empty() && !all_tool_uses.is_empty() {
        " ".to_string()
    } else {
        content_parts.join("\n\n")
    };

    let mut assistant = AssistantMessage::new(content);
    if !all_tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(all_tool_uses);
    }
    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_from_json(v: serde_json::Value) -> MessagesRequest {
        serde_json::from_value(v).expect("valid request json")
    }

    #[test]
    fn test_trailing_user_messages_merge_without_fake_ok() {
        let req = request_from_json(serde_json::json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 1024,
            "system": "You are a test agent.",
            "messages": [
                {"role": "user", "content": "run the tool"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_ok1", "name": "run_code", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_ok1", "content": "tool output"}
                ]},
                {"role": "user", "content": "REMINDER_70c4 background job finished"}
            ],
            "tools": [{"name": "run_code", "description": "Run code",
                       "input_schema": {"type": "object", "properties": {}}}]
        }));
        let result = convert_request(&req).unwrap();
        let state = &result.conversation_state;
        assert!(
            !state.history.iter().any(|m| matches!(m,
                Message::Assistant(a) if a.assistant_response_message.content == "OK")),
            "trailing user messages must not produce a fake assistant OK in history"
        );
        assert!(matches!(state.history.last(), Some(Message::Assistant(_))));
        let current = &state.current_message.user_input_message;
        assert!(current.content.contains("REMINDER_70c4"));
        assert_eq!(current.user_input_message_context.tool_results.len(), 1);
    }

    #[test]
    fn test_chunked_policy_only_with_chunked_tools() {
        let build = |tool: &str| {
            request_from_json(serde_json::json!({
                "model": "claude-sonnet-4.5",
                "max_tokens": 1024,
                "system": "sys",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": tool, "description": "d",
                           "input_schema": {"type": "object", "properties": {}}}]
            }))
        };
        let has_policy = |req: &MessagesRequest| {
            convert_request(req).unwrap().conversation_state.history.iter().any(|m| {
                matches!(m, Message::User(u)
                    if u.user_input_message.content.contains("Complete all chunked operations"))
            })
        };
        assert!(has_policy(&build("Write")));
        assert!(!has_policy(&build("run_code")));
    }

    #[test]
    fn test_map_model_sonnet() {
        assert!(
            map_model("claude-sonnet-4-5-20250929")
                .unwrap()
                .contains("sonnet")
        );
        assert!(
            map_model("claude-sonnet-4-6")
                .unwrap()
                .contains("sonnet")
        );
    }

    #[test]
    fn test_map_model_opus() {
        assert!(
            map_model("claude-opus-4-5-20251101")
                .unwrap()
                .contains("opus")
        );
    }

    #[test]
    fn test_map_model_opus_4_7() {
        assert_eq!(
            map_model("claude-opus-4-7"),
            Some("claude-opus-4.7".to_string())
        );
        assert_eq!(
            map_model("claude-opus-4.7-thinking"),
            Some("claude-opus-4.7".to_string())
        );
        assert_eq!(get_context_window_size("claude-opus-4-7"), 1_000_000);
    }

    #[test]
    fn test_map_model_opus_4_8() {
        assert_eq!(
            map_model("claude-opus-4-8"),
            Some("claude-opus-4.8".to_string())
        );
        assert_eq!(
            map_model("claude-opus-4.8-thinking"),
            Some("claude-opus-4.8".to_string())
        );
        assert_eq!(get_context_window_size("claude-opus-4-8"), 1_000_000);
    }

    #[test]
    fn test_map_model_sonnet_4_8() {
        assert_eq!(
            map_model("claude-sonnet-4-8"),
            Some("claude-sonnet-4.8".to_string())
        );
        assert_eq!(
            map_model("claude-sonnet-4.8-thinking"),
            Some("claude-sonnet-4.8".to_string())
        );
        assert_eq!(get_context_window_size("claude-sonnet-4-8"), 1_000_000);
    }

    #[test]
    fn test_map_model_sonnet_5() {
        assert_eq!(
            map_model("claude-sonnet-5"),
            Some("claude-sonnet-5".to_string())
        );
        assert_eq!(
            map_model("claude-sonnet-5-20260101-thinking"),
            Some("claude-sonnet-5".to_string())
        );
        // 点号形式 sonnet.5 也应命中
        assert_eq!(
            map_model("claude-sonnet.5"),
            Some("claude-sonnet-5".to_string())
        );
        assert_eq!(
            map_model("claude-sonnet5"),
            Some("claude-sonnet-5".to_string())
        );
        assert_eq!(get_context_window_size("claude-sonnet-5"), 1_000_000);
        assert_eq!(
            map_model("claude-sonnet-5-5"),
            Some("claude-sonnet-5.5".to_string())
        );
        assert_eq!(get_context_window_size("claude-sonnet-5-5"), 1_000_000);
        assert_eq!(
            map_model("claude-3-5-sonnet-20241022"),
            Some("claude-sonnet-3.5".to_string())
        );
    }

    #[test]
    fn test_map_model_fable_5() {
        assert_eq!(
            map_model("claude-fable-5"),
            Some("claude-fable-5".to_string())
        );
        assert_eq!(
            map_model("claude-fable-5-thinking"),
            Some("claude-fable-5".to_string())
        );
        assert_eq!(get_context_window_size("claude-fable-5"), 1_000_000);
    }

    #[test]
    fn test_map_model_haiku() {
        assert!(
            map_model("claude-haiku-4-20250514")
                .unwrap()
                .contains("haiku")
        );
    }

    #[test]
    fn test_map_model_open_passthrough() {
        for model in [
            "glm-5",
            "minimax-m2.5",
            "deepseek-3.2",
            "gpt-4",
            "future-model-2030",
        ] {
            assert_eq!(map_model(model), Some(model.to_string()));
        }
    }

    #[test]
    fn test_map_model_future_claude_formats() {
        assert_eq!(
            map_model("claude-opus-5"),
            Some("claude-opus-5".to_string())
        );
        assert_eq!(
            map_model("claude-opus-5-latest"),
            Some("claude-opus-5".to_string())
        );
        assert_eq!(
            map_model("claude-opus-5-20270101-thinking"),
            Some("claude-opus-5".to_string())
        );
        assert_eq!(
            map_model("claude-sonnet-5-2"),
            Some("claude-sonnet-5.2".to_string())
        );
        for model in [
            "claude-opus-5-5",
            "claude-opus-5.5",
            "claude-opus-5-5-thinking",
            "claude-opus-5.5-latest",
            "claude-opus-5-5-20270101",
            "CLAUDE-OPUS-5-5",
        ] {
            assert_eq!(
                map_model(model),
                Some("claude-opus-5.5".to_string()),
                "{model} 应映射到 claude-opus-5.5"
            );
        }
        assert_eq!(
            map_model("claude-opus-5-beta"),
            Some("claude-opus-5-beta".to_string())
        );
    }

    /// Opus 5 的上下文窗口回归测试。
    ///
    /// 该模型曾被漏配在 1M 名单之外，导致 `Event::ContextUsage` 把上游回报的
    /// 百分比乘以 200_000，usage 上报缩小 5 倍。
    #[test]
    fn test_context_window_opus_5() {
        assert_eq!(get_context_window_size("claude-opus-5"), 1_000_000);
        // 别名/后缀变体经 map_model 归一化后同样落在 1M
        assert_eq!(get_context_window_size("claude-opus-5-latest"), 1_000_000);
        assert_eq!(
            get_context_window_size("claude-opus-5-20270101-thinking"),
            1_000_000
        );
        assert_eq!(get_context_window_size("claude-opus.5"), 1_000_000);
        // opus-4-5 不得被误匹配为 opus-5
        assert_eq!(get_context_window_size("claude-opus-4-5"), 200_000);
    }

    /// 1M 名单的整体校验：新增 1M 模型时应同步此处，避免再次漏配。
    #[test]
    fn test_context_window_1m_family() {
        for model in [
            "claude-sonnet-4-6",
            "claude-sonnet-4-8",
            "claude-sonnet-5",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-opus-5.5-thinking",
            "claude-haiku-5-5",
            "claude-haiku-5.5-20270101",
            "claude-fable-5",
        ] {
            assert_eq!(
                get_context_window_size(model),
                1_000_000,
                "{model} 应为 1M 上下文窗口"
            );
        }
        // 未纳入 1M 的模型仍回退 200k
        for model in ["claude-haiku-4-5", "claude-sonnet-4-5", "claude-opus-4-5"] {
            assert_eq!(
                get_context_window_size(model),
                200_000,
                "{model} 应回退 200k"
            );
        }
    }

    #[test]
    fn test_map_model_rejects_invalid_ids() {
        assert!(map_model("").is_none());
        assert!(map_model("   ").is_none());
        assert!(map_model("bad\nmodel").is_none());
        assert!(map_model(&"x".repeat(MAX_MODEL_ID_LEN + 1)).is_none());
    }

    #[test]
    fn test_map_model_gpt_5_6_family() {
        // Kiro serves the GPT-5.6 family; ids pass through verbatim.
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert_eq!(map_model(model), Some(model.to_string()));
            assert_eq!(
                get_context_window_size(model),
                1_000_000,
                "{model} should use the 1M context window"
            );
        }

        // Do not silently expand unconfirmed GPT model windows.
        assert_eq!(get_context_window_size("gpt-5.5"), 272_000);
    }

    #[test]
    fn test_map_model_thinking_suffix_sonnet() {
        // thinking 后缀不应影响 sonnet 模型映射
        let result = map_model("claude-sonnet-4-5-20250929-thinking");
        assert_eq!(result, Some("claude-sonnet-4.5".to_string()));
    }

    #[test]
    fn test_map_model_thinking_suffix_opus_4_5() {
        // thinking 后缀不应影响 opus 4.5 模型映射
        let result = map_model("claude-opus-4-5-20251101-thinking");
        assert_eq!(result, Some("claude-opus-4.5".to_string()));
    }

    #[test]
    fn test_map_model_thinking_suffix_opus_4_6() {
        // thinking 后缀不应影响 opus 4.6 模型映射
        let result = map_model("claude-opus-4-6-thinking");
        assert_eq!(result, Some("claude-opus-4.6".to_string()));
    }

    #[test]
    fn test_map_model_thinking_suffix_haiku() {
        // thinking 后缀不应影响 haiku 模型映射
        let result = map_model("claude-haiku-4-5-20251001-thinking");
        assert_eq!(result, Some("claude-haiku-4.5".to_string()));
    }

    fn minimal_request_with_output_config(model: &str) -> MessagesRequest {
        minimal_request_with_effort(model, "high")
    }

    fn minimal_request_with_effort(model: &str, effort: &str) -> MessagesRequest {
        use super::super::types::{Message as AnthropicMessage, OutputConfig};

        MessagesRequest {
            model: model.to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: Some(OutputConfig {
                effort: effort.to_string(),
            }),
            metadata: None,
            cache_control: None,
        }
    }

    fn minimal_adaptive_thinking_request_with_output_config(model: &str) -> MessagesRequest {
        use super::super::types::Thinking;

        let mut req = minimal_request_with_output_config(model);
        req.thinking = Some(Thinking {
            thinking_type: "adaptive".to_string(),
            budget_tokens: 20000,
        });
        req
    }

    fn minimal_adaptive_thinking_request_with_effort(model: &str, effort: &str) -> MessagesRequest {
        use super::super::types::Thinking;

        let mut req = minimal_request_with_effort(model, effort);
        req.thinking = Some(Thinking {
            thinking_type: "adaptive".to_string(),
            budget_tokens: 20000,
        });
        req
    }

    fn minimal_thinking_request(model: &str, thinking_type: &str) -> MessagesRequest {
        use super::super::types::Thinking;

        let mut req = minimal_request_with_output_config(model);
        req.output_config = None;
        req.thinking = Some(Thinking {
            thinking_type: thinking_type.to_string(),
            budget_tokens: 20000,
        });
        req
    }

    #[test]
    fn test_output_config_does_not_emit_unsupported_additional_fields() {
        let req = minimal_request_with_output_config("claude-sonnet-4-8-thinking");
        let result = convert_request(&req).unwrap();

        assert!(
            result.additional_model_request_fields.is_none(),
            "sonnet 4.8 rejects additionalModelRequestFields even when the client sends output_config"
        );
    }

    #[test]
    fn test_output_config_does_not_emit_for_unconfirmed_dynamic_model() {
        let req = minimal_request_with_output_config("glm-5");
        let result = convert_request(&req).unwrap();

        assert!(result.additional_model_request_fields.is_none());
        assert_eq!(
            result
                .conversation_state
                .current_message
                .user_input_message
                .model_id,
            "glm-5"
        );
    }

    #[test]
    fn test_opus_5_5_maps_upstream_id_and_keeps_xhigh() {
        let req =
            minimal_adaptive_thinking_request_with_effort("claude-opus-5-5-thinking", "xhigh");
        let result = convert_request(&req).unwrap();

        assert_eq!(
            result
                .conversation_state
                .current_message
                .user_input_message
                .model_id,
            "claude-opus-5.5"
        );
        let fields = result
            .additional_model_request_fields
            .expect("opus 5.5 adaptive thinking should keep output_config");
        assert_eq!(fields.output_config.unwrap().effort, "xhigh");
    }

    #[test]
    fn test_haiku_5_5_maps_upstream_id_and_keeps_effort() {
        // Kiro 为 Haiku 5.5 提供 low / medium / high / xhigh / max 全部档位
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            let req = minimal_adaptive_thinking_request_with_effort("claude-haiku-5-5", effort);
            let result = convert_request(&req).unwrap();

            assert_eq!(
                result
                    .conversation_state
                    .current_message
                    .user_input_message
                    .model_id,
                "claude-haiku-5.5"
            );
            let fields = result
                .additional_model_request_fields
                .expect("haiku 5.5 adaptive thinking should keep output_config");
            assert_eq!(fields.output_config.unwrap().effort, effort);
        }

        // haiku-4.5 不得被误判为支持原生 reasoning
        assert!(!model_supports_native_reasoning("claude-haiku-4.5"));
    }

    #[test]
    fn test_output_config_does_not_emit_for_non_adaptive_opus_4_6() {
        let req = minimal_request_with_output_config("claude-opus-4-6");
        let result = convert_request(&req).unwrap();

        assert!(
            result.additional_model_request_fields.is_none(),
            "opus 4.6 only uses additionalModelRequestFields for adaptive thinking"
        );
    }

    #[test]
    fn test_thinking_does_not_emit_additional_fields_for_sonnet_4_5() {
        let req = minimal_thinking_request("claude-sonnet-4-5-20250929-thinking", "enabled");
        let result = convert_request(&req).unwrap();

        assert!(
            result.additional_model_request_fields.is_none(),
            "sonnet 4.5 rejects additionalModelRequestFields even when thinking is enabled"
        );
    }

    #[test]
    fn test_enabled_thinking_does_not_emit_output_config_for_opus_4_6() {
        let mut req = minimal_request_with_output_config("claude-opus-4-6-thinking");
        req.thinking = minimal_thinking_request("claude-opus-4-6-thinking", "enabled").thinking;
        let result = convert_request(&req).unwrap();

        assert!(
            result.additional_model_request_fields.is_none(),
            "opus 4.6 output_config is only accepted on adaptive thinking requests"
        );
    }

    #[test]
    fn test_output_config_emits_additional_fields_for_opus_4_6() {
        let req = minimal_adaptive_thinking_request_with_output_config("claude-opus-4-6-thinking");
        let result = convert_request(&req).unwrap();

        let fields = result
            .additional_model_request_fields
            .expect("opus 4.6 adaptive thinking should keep the real effort field");
        assert_eq!(
            fields.output_config.unwrap().effort,
            "high",
            "effort should be passed through for the supported model"
        );
    }

    #[test]
    fn test_output_config_downgrades_xhigh_for_opus_4_6() {
        let req =
            minimal_adaptive_thinking_request_with_effort("claude-opus-4-6-thinking", "xhigh");
        let result = convert_request(&req).unwrap();

        let fields = result
            .additional_model_request_fields
            .expect("opus 4.6 adaptive thinking should keep output_config");
        assert_eq!(
            fields.output_config.unwrap().effort,
            "high",
            "opus 4.6 upstream only accepts low/medium/high/max, so xhigh should downgrade"
        );
    }

    #[test]
    fn test_output_config_downgrades_xhigh_for_known_older_models() {
        for model in [
            "claude-opus-4.6",
            "claude-sonnet-4.6",
            "claude-opus-4.5",
            "claude-sonnet-4.5",
            "claude-haiku-4.5",
        ] {
            assert_eq!(
                normalize_effort_for_model(model, "xhigh").as_deref(),
                Some("high"),
                "{model} should not emit xhigh"
            );
        }
    }

    #[test]
    fn test_output_config_preserves_xhigh_for_models_without_known_restriction() {
        assert_eq!(
            normalize_effort_for_model("claude-opus-4.7", "xhigh").as_deref(),
            Some("xhigh"),
            "opus 4.7 supports xhigh"
        );
        assert_eq!(
            normalize_effort_for_model("claude-opus-4.8", "xhigh").as_deref(),
            Some("xhigh"),
            "opus 4.8 supports xhigh"
        );
        assert_eq!(
            normalize_effort_for_model("claude-5", "xhigh").as_deref(),
            Some("xhigh"),
            "claude 5 supports xhigh"
        );
        assert_eq!(
            normalize_effort_for_model("claude-sonnet-5.1", "xhigh").as_deref(),
            Some("xhigh"),
            "future models should not require explicit allow-listing for recognized effort values"
        );
        assert_eq!(
            normalize_effort_for_model("claude-unknown-9", "xhigh").as_deref(),
            Some("xhigh"),
            "unknown future models should keep recognized effort values"
        );
    }

    #[test]
    fn test_output_config_normalizes_effort_case_and_spacing() {
        let req =
            minimal_adaptive_thinking_request_with_effort("claude-opus-4-6-thinking", "  MAX  ");
        let result = convert_request(&req).unwrap();

        let fields = result
            .additional_model_request_fields
            .expect("opus 4.6 adaptive thinking should keep output_config");
        assert_eq!(
            fields.output_config.unwrap().effort,
            "max",
            "effort should be normalized before being sent to upstream"
        );
    }

    #[test]
    fn test_output_config_unknown_effort_falls_back_to_high() {
        let req =
            minimal_adaptive_thinking_request_with_effort("claude-opus-4-6-thinking", "extreme");
        let result = convert_request(&req).unwrap();

        let fields = result
            .additional_model_request_fields
            .expect("opus 4.6 adaptive thinking should keep output_config");
        assert_eq!(
            fields.output_config.unwrap().effort,
            "high",
            "unknown effort values should fall back instead of causing upstream validation errors"
        );
    }

    // ---- Fix 3: 原生 thinking effort 下发拓宽 + budget 推导 ----

    #[test]
    fn effort_from_budget_tokens_maps_tiers() {
        assert_eq!(effort_from_budget_tokens(2_000), "low");
        assert_eq!(effort_from_budget_tokens(4_000), "low");
        assert_eq!(effort_from_budget_tokens(10_000), "medium");
        assert_eq!(effort_from_budget_tokens(16_000), "medium");
        assert_eq!(effort_from_budget_tokens(20_000), "high");
        assert_eq!(effort_from_budget_tokens(64_000), "high");
        assert_eq!(effort_from_budget_tokens(100_000), "xhigh");
    }

    #[test]
    fn model_supports_native_reasoning_allows_confirmed_and_5_family() {
        for m in [
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "claude-opus-4.6",
            "claude-opus-4.7",
            "claude-opus-4.8",
            "claude-sonnet-4.6",
            "claude-fable-5",
            "claude-sonnet-5",
            "claude-opus-5.5",
        ] {
            assert!(model_supports_native_reasoning(m), "{m} 应支持原生 reasoning");
        }
        for m in [
            "claude-sonnet-4.8",
            "claude-sonnet-4.5",
            "claude-opus-4.5",
            "claude-haiku-4.5",
        ] {
            assert!(
                !model_supports_native_reasoning(m),
                "{m} 未确认支持，不应下发 output_config"
            );
        }
    }

    #[test]
    fn gpt_5_6_effort_uses_reasoning_wire_field() {
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            for effort in ["none", "low", "medium", "high", "xhigh", "max"] {
                let req = minimal_request_with_effort(model, effort);
                let fields = convert_request(&req)
                    .unwrap()
                    .additional_model_request_fields
                    .expect("GPT-5.6 effort should be forwarded");
                assert!(fields.output_config.is_none());
                assert_eq!(fields.reasoning.unwrap().effort, effort);
            }
        }
    }

    #[test]
    fn none_effort_falls_back_for_claude() {
        assert_eq!(
            normalize_effort_for_model("claude-opus-4.7", "none").as_deref(),
            Some("high")
        );
    }

    #[test]
    fn enabled_thinking_emits_output_config_for_opus_4_7() {
        // 标准 Anthropic thinking:{type:"enabled"}（无 output_config）→ 由 budget 推导 effort。
        let req = minimal_thinking_request("claude-opus-4-7", "enabled");
        let result = convert_request(&req).unwrap();
        let fields = result
            .additional_model_request_fields
            .expect("opus 4.7 enabled thinking should emit output_config");
        // 默认 budget_tokens=20000 → high。
        assert_eq!(fields.output_config.unwrap().effort, "high");
    }

    #[test]
    fn enabled_thinking_emits_output_config_for_sonnet_4_6() {
        let req = minimal_thinking_request("claude-sonnet-4-6", "enabled");
        let result = convert_request(&req).unwrap();
        assert!(
            result.additional_model_request_fields.is_some(),
            "sonnet 4.6 enabled thinking should emit output_config"
        );
    }

    #[test]
    fn budget_tokens_derive_effort_for_opus_4_8() {
        use super::super::types::Thinking;
        let mut req = minimal_thinking_request("claude-opus-4-8", "enabled");
        req.thinking = Some(Thinking {
            thinking_type: "enabled".into(),
            budget_tokens: 3_000,
        });
        let result = convert_request(&req).unwrap();
        assert_eq!(
            result
                .additional_model_request_fields
                .unwrap()
                .output_config
                .unwrap()
                .effort,
            "low",
            "budget_tokens=3000 应推导为 low"
        );
    }

    #[test]
    fn disabled_thinking_emits_nothing_even_for_supported_model() {
        let req = minimal_thinking_request("claude-opus-4-8", "disabled");
        let result = convert_request(&req).unwrap();
        assert!(
            result.additional_model_request_fields.is_none(),
            "thinking disabled 不应下发任何字段"
        );
    }

    #[test]
    fn explicit_effort_emits_for_fable_5_without_thinking() {
        // 仅 output_config.effort（无 thinking）在 5 系模型上也算请求了 reasoning。
        let req = minimal_request_with_effort("claude-fable-5", "xhigh");
        let result = convert_request(&req).unwrap();
        let fields = result
            .additional_model_request_fields
            .expect("fable-5 显式 effort 应下发");
        // fable-5 支持 xhigh（model_supports_xhigh_effort），不降级。
        assert_eq!(fields.output_config.unwrap().effort, "xhigh");
    }

    // ---- normalize_json_schema: 顶层 type / 组合关键字（PR#6）----

    #[test]
    fn normalize_schema_forces_top_level_type_object() {
        let s = normalize_json_schema(serde_json::json!({
            "type": "array",
            "items": {"type": "string"}
        }));
        assert_eq!(s["type"], "object", "顶层 type 非 object 应被强制修正");
    }

    #[test]
    fn normalize_schema_strips_top_level_oneof_and_recovers_fields() {
        let s = normalize_json_schema(serde_json::json!({
            "oneOf": [
                {"type": "object", "properties": {"locs": {"type": "array"}}, "required": ["locs"]},
                {"type": "object", "properties": {"labels": {"type": "array"}}, "required": ["labels"]}
            ]
        }));
        assert_eq!(s["type"], "object");
        assert!(s.get("oneOf").is_none(), "顶层 oneOf 应被剥离");
        assert!(
            s["properties"].get("locs").is_some(),
            "应从首个 object variant 恢复 properties"
        );
        assert_eq!(s["required"], serde_json::json!(["locs"]));
    }

    #[test]
    fn normalize_schema_strips_top_level_anyof_without_object_variant() {
        let s = normalize_json_schema(serde_json::json!({
            "anyOf": [{"type": "string"}, {"type": "number"}]
        }));
        assert_eq!(s["type"], "object");
        assert!(s.get("anyOf").is_none());
        assert_eq!(s["properties"], serde_json::json!({}));
    }

    #[test]
    fn test_determine_chat_trigger_type() {
        // 无工具时返回 MANUAL
        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };
        assert_eq!(determine_chat_trigger_type(&req), "MANUAL");
    }

    #[test]
    fn test_collect_history_tool_names() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 创建包含工具使用的历史消息
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
            ToolUseEntry::new("tool-2", "write")
                .with_input(serde_json::json!({"path": "/out.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_names = collect_history_tool_names(&history);
        assert_eq!(tool_names.len(), 2);
        assert!(tool_names.contains(&"read".to_string()));
        assert!(tool_names.contains(&"write".to_string()));
    }

    #[test]
    fn test_create_placeholder_tool() {
        let tool = create_placeholder_tool("my_custom_tool");

        assert_eq!(tool.tool_specification.name, "my_custom_tool");
        assert!(!tool.tool_specification.description.is_empty());

        // 验证 JSON 序列化正确
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"name\":\"my_custom_tool\""));
    }

    #[test]
    fn test_shorten_tool_name_deterministic() {
        let long_name = "mcp__some_very_long_server_name__some_very_long_tool_name_that_exceeds_limit";
        assert!(long_name.len() > TOOL_NAME_MAX_LEN);

        let short1 = shorten_tool_name(long_name);
        let short2 = shorten_tool_name(long_name);
        assert_eq!(short1, short2, "相同输入应产生相同的短名称");
        assert!(short1.len() <= TOOL_NAME_MAX_LEN, "短名称长度应 <= 63，实际 {}", short1.len());
    }

    #[test]
    fn test_shorten_tool_name_uniqueness() {
        let name_a = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_a";
        let name_b = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_b";
        let short_a = shorten_tool_name(name_a);
        let short_b = shorten_tool_name(name_b);
        assert_ne!(short_a, short_b, "不同输入应产生不同的短名称");
    }

    #[test]
    fn test_map_tool_name_short_passthrough() {
        let mut map = HashMap::new();
        let result = map_tool_name("short_name", &mut map);
        assert_eq!(result, "short_name");
        assert!(map.is_empty(), "短名称不应产生映射");
    }

    #[test]
    fn test_map_tool_name_long_creates_mapping() {
        let mut map = HashMap::new();
        let long_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        let result = map_tool_name(long_name, &mut map);
        assert!(result.len() <= TOOL_NAME_MAX_LEN);
        assert_eq!(map.get(&result), Some(&long_name.to_string()));
    }

    // ---- Tool Call 双向兼容（ClaudeCode 内置工具名/入参映射）----

    fn cc_tool(name: &str) -> super::super::types::Tool {
        let mut schema = std::collections::BTreeMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));
        super::super::types::Tool {
            name: name.to_string(),
            description: String::new(),
            input_schema: schema,
            tool_type: None,
            max_uses: None,
            cache_control: None,
        }
    }

    #[test]
    fn cc_builtin_name_table() {
        assert_eq!(claude_code_tool_name_to_kiro("Write"), Some("fs_write"));
        assert_eq!(claude_code_tool_name_to_kiro("Edit"), Some("str_replace"));
        assert_eq!(claude_code_tool_name_to_kiro("Bash"), Some("execute_bash"));
        assert_eq!(claude_code_tool_name_to_kiro("Read"), Some("read_file"));
        assert_eq!(claude_code_tool_name_to_kiro("Glob"), Some("file_search"));
        assert_eq!(claude_code_tool_name_to_kiro("Grep"), Some("grep_search"));
        assert_eq!(claude_code_tool_name_to_kiro("LS"), Some("list_directory"));
        assert_eq!(claude_code_tool_name_to_kiro("WebSearch"), Some("web_search"));
        assert_eq!(claude_code_tool_name_to_kiro("MyTool"), None);
    }

    #[test]
    fn cc_kiro_builtin_schema_covers_all_eight() {
        for k in [
            "fs_write",
            "str_replace",
            "execute_bash",
            "read_file",
            "file_search",
            "grep_search",
            "list_directory",
            "web_search",
        ] {
            assert!(kiro_builtin_tool_schema(k).is_some(), "{k} 应有内置 schema");
        }
        assert!(kiro_builtin_tool_schema("fs_append").is_none());
    }

    #[test]
    fn cc_convert_tools_maps_names_hides_fs_append_and_dedups() {
        let mut map = HashMap::new();
        // Write 与 fs_write 都映射/命中 fs_write（小写去重）；fs_append 被隐藏。
        let tools = Some(vec![
            cc_tool("Write"),
            cc_tool("Read"),
            cc_tool("fs_append"),
            cc_tool("fs_write"),
        ]);
        let out = convert_tools(&tools, &mut map, ToolCompatibilityMode::ClaudeCode).unwrap();
        let names: Vec<&str> = out
            .iter()
            .map(|t| t.tool_specification.name.as_str())
            .collect();
        assert!(names.contains(&"fs_write"), "Write 应映射为 fs_write");
        assert!(names.contains(&"read_file"), "Read 应映射为 read_file");
        assert!(!names.contains(&"fs_append"), "fs_append 应被隐藏");
        assert_eq!(
            names.iter().filter(|n| **n == "fs_write").count(),
            1,
            "Write 与 fs_write 应按小写去重为一个"
        );
        assert_eq!(map.get("read_file").map(|s| s.as_str()), Some("Read"));
    }

    #[test]
    fn cc_convert_tools_substitutes_builtin_schema() {
        let mut map = HashMap::new();
        let out = convert_tools(
            &Some(vec![cc_tool("Write")]),
            &mut map,
            ToolCompatibilityMode::ClaudeCode,
        )
        .unwrap();
        // 硬编码 fs_write schema：包含 path/text。
        let s = serde_json::to_string(&out[0]).unwrap();
        assert!(s.contains("\"path\""), "内置 schema 应含 path");
        assert!(s.contains("\"text\""), "内置 schema 应含 text");
    }

    #[test]
    fn cc_raw_mode_keeps_client_tool_names() {
        let mut map = HashMap::new();
        let out = convert_tools(
            &Some(vec![cc_tool("Write")]),
            &mut map,
            ToolCompatibilityMode::Raw,
        )
        .unwrap();
        assert_eq!(out[0].tool_specification.name, "Write", "Raw 模式不改名");
        assert!(map.is_empty(), "Raw 模式不记录内置映射");
    }

    #[test]
    fn cc_outbound_input_write_and_read() {
        let out = map_tool_input_to_kiro(
            "Write",
            serde_json::json!({"file_path": "/a.txt", "content": "hi"}),
            ToolCompatibilityMode::ClaudeCode,
        )
        .unwrap();
        assert_eq!(out, serde_json::json!({"path": "/a.txt", "text": "hi"}));

        let read = map_tool_input_to_kiro(
            "Read",
            serde_json::json!({"file_path": "/a", "offset": 10, "limit": 5}),
            ToolCompatibilityMode::ClaudeCode,
        )
        .unwrap();
        assert_eq!(read["path"], serde_json::json!("/a"));
        assert_eq!(read["start_line"], serde_json::json!(10));
        assert_eq!(read["end_line"], serde_json::json!(14)); // 10 + 5 - 1
        assert!(read.get("explanation").is_some(), "Read 缺省注入 explanation");
    }

    #[test]
    fn cc_outbound_read_pages_is_dropped() {
        let out = map_tool_input_to_kiro(
            "Read",
            serde_json::json!({"file_path": "/a", "offset": 3, "pages": "1-3"}),
            ToolCompatibilityMode::ClaudeCode,
        )
        .unwrap();
        assert!(out.get("pages").is_none());
        assert_eq!(out["path"], serde_json::json!("/a"));
        assert_eq!(out["start_line"], serde_json::json!(3));
    }

    /// 回归：历史 assistant 消息里的 Read.pages（含空串）不应让整个请求 400。
    /// 该入参只会出现在回放历史中，客户端无法修正，报错会让该会话此后每次请求都失败。
    #[test]
    fn cc_history_read_with_pages_converts_without_error() {
        use super::super::types::Message as AnthropicMessage;

        for pages in [serde_json::json!("1-3"), serde_json::json!("")] {
            let msg = AnthropicMessage {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type": "tool_use", "id": "toolu_01PDF", "name": "Read",
                     "input": {"file_path": "/a.pdf", "pages": pages}}
                ]),
            };
            let mut map = HashMap::new();
            let result = convert_assistant_message(
                &msg,
                &mut map,
                ToolCompatibilityMode::ClaudeCode,
                ConversionPurpose::Generate,
                true,
            )
            .unwrap_or_else(|e| panic!("pages={pages} 的历史 Read 应能转换: {e}"));

            let tool_uses = result
                .assistant_response_message
                .tool_uses
                .expect("应保留 tool_use");
            assert_eq!(tool_uses[0].name, "read_file");
            assert_eq!(tool_uses[0].input["path"], serde_json::json!("/a.pdf"));
            assert!(
                tool_uses[0].input.get("pages").is_none(),
                "Kiro read_file schema 为 additionalProperties:false，不能带 pages"
            );
        }
    }

    #[test]
    fn cc_raw_mode_input_passthrough() {
        let input = serde_json::json!({"file_path": "/a.txt", "content": "hi"});
        let out = map_tool_input_to_kiro("Write", input.clone(), ToolCompatibilityMode::Raw).unwrap();
        assert_eq!(out, input, "Raw 模式入参原样透传");
    }

    #[test]
    fn cc_roundtrip_write_out_then_in() {
        let client = serde_json::json!({"file_path": "/a.txt", "content": "hello"});
        let kiro =
            map_tool_input_to_kiro("Write", client.clone(), ToolCompatibilityMode::ClaudeCode)
                .unwrap();
        assert_eq!(kiro, serde_json::json!({"path": "/a.txt", "text": "hello"}));
        let mut map = HashMap::new();
        map.insert("fs_write".to_string(), "Write".to_string());
        let (name, restored) = restore_tool_use_for_client("fs_write", kiro, &map);
        assert_eq!(name, "Write");
        assert_eq!(restored, client, "出入站往返应还原客户端入参");
    }

    #[test]
    fn cc_inbound_restore_read_file() {
        let mut map = HashMap::new();
        map.insert("read_file".to_string(), "Read".to_string());
        let (name, restored) = restore_tool_use_for_client(
            "read_file",
            serde_json::json!({"path": "/a", "start_line": 10, "end_line": 14}),
            &map,
        );
        assert_eq!(name, "Read");
        assert_eq!(restored["file_path"], serde_json::json!("/a"));
        assert_eq!(restored["offset"], serde_json::json!(10));
        assert_eq!(restored["limit"], serde_json::json!(5)); // 14 - 10 + 1
    }

    /// 优化点回归：入站还原以 **Kiro 名** 匹配，故 Raw 模式下客户端自带、恰好叫
    /// "Read" 的工具，其入参不会被误改写（tool_name_map 无该条目，"Read" 非 Kiro 内置名）。
    #[test]
    fn cc_inbound_restore_keyed_on_kiro_name_not_client_name() {
        let map = HashMap::new();
        let input = serde_json::json!({"offset": 3, "custom_field": true});
        let (name, restored) = restore_tool_use_for_client("Read", input.clone(), &map);
        assert_eq!(name, "Read");
        assert_eq!(
            restored, input,
            "客户端自带 Read 工具在 Raw 下入参必须原样保留（不被误映射）"
        );
    }

    #[test]
    fn cc_convert_request_default_maps_builtin_names() {
        use super::super::types::Message as AnthropicMessage;
        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("hi"),
            }],
            system: None,
            stream: false,
            tools: Some(vec![cc_tool("Write"), cc_tool("Read")]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };
        // convert_request 测试垫片默认 ClaudeCode 模式。
        let result = convert_request(&req).unwrap();
        assert_eq!(result.tool_name_map.get("fs_write").map(|s| s.as_str()), Some("Write"));
        assert_eq!(result.tool_name_map.get("read_file").map(|s| s.as_str()), Some("Read"));
    }

    #[test]
    fn test_tool_name_mapping_in_convert_request() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        assert!(long_tool_name.len() > TOOL_NAME_MAX_LEN);

        let mut schema = std::collections::BTreeMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("test"),
                },
            ],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };

        let result = convert_request(&req).unwrap();

        // 应该有映射
        assert_eq!(result.tool_name_map.len(), 1);

        // 映射中的值应该是原始名称
        let (short, original) = result.tool_name_map.iter().next().unwrap();
        assert_eq!(original, long_tool_name);
        assert!(short.len() <= TOOL_NAME_MAX_LEN);

        // Kiro 请求中的工具名应该是短名称
        let tools = &result.conversation_state.current_message.user_input_message
            .user_input_message_context.tools;
        assert_eq!(tools[0].tool_specification.name, *short);
    }

    #[test]
    fn test_tool_name_mapping_in_history() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";

        let mut schema = std::collections::BTreeMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("use the tool"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "calling tool"},
                        {"type": "tool_use", "id": "toolu_01", "name": long_tool_name, "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01", "content": "done"}
                    ]),
                },
            ],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            }]),
            thinking: None,
            tool_choice: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };

        let result = convert_request(&req).unwrap();
        let short_name = result.tool_name_map.iter().next().unwrap().0.clone();

        // 历史中 assistant 消息的 tool_use name 也应该被映射
        let history = &result.conversation_state.history;
        let mut found = false;
        for msg in history {
            if let Message::Assistant(a) = msg {
                if let Some(ref tool_uses) = a.assistant_response_message.tool_uses {
                    for tu in tool_uses {
                        if tu.tool_use_id == "toolu_01" {
                            assert_eq!(tu.name, short_name, "历史中的 tool_use name 应该是短名称");
                            found = true;
                        }
                    }
                }
            }
        }
        assert!(found, "应该在历史中找到 tool_use");
    }

    #[test]
    fn test_history_tools_added_to_tools_list() {
        use super::super::types::Message as AnthropicMessage;

        // 创建一个请求，历史中有工具使用，但 tools 列表为空
        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "I'll read the file."},
                        {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/test.txt"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None, // 没有提供工具定义
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };

        let result = convert_request(&req).unwrap();

        // 验证 tools 列表中包含了历史中使用的工具的占位符定义
        let tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;

        assert!(!tools.is_empty(), "tools 列表不应为空");
        assert!(
            tools.iter().any(|t| t.tool_specification.name == "read"),
            "tools 列表应包含 'read' 工具的占位符定义"
        );
    }

    #[test]
    fn test_extract_session_id_valid() {
        // 测试有效的 user_id 格式
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_8bb5523b-ec7c-4540-a9ca-beb6d79f1552";
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_format() {
        // 测试 JSON 格式的 user_id
        let user_id = r#"{"device_id":"0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd","account_uuid":"","session_id":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_invalid_session() {
        // 测试 JSON 格式但 session_id 不是有效 UUID
        let user_id = r#"{"device_id":"abc","session_id":"not-a-uuid"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_no_session() {
        // 测试没有 session 的 user_id
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_invalid_uuid() {
        // 测试无效的 UUID 格式
        let user_id = "user_xxx_session_invalid-uuid";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_convert_request_with_session_metadata() {
        use super::super::types::{Message as AnthropicMessage, Metadata};

        // 测试带有 metadata 的请求，应该使用 session UUID 作为 conversationId
        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: Some(Metadata {
                user_id: Some(
                    "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_a0662283-7fd3-4399-a7eb-52b9a717ae88".to_string(),
                ),
            }),
            cache_control: None,
        };

        let result = convert_request(&req).unwrap();
        assert_eq!(
            result.conversation_state.conversation_id,
            "a0662283-7fd3-4399-a7eb-52b9a717ae88"
        );

        // agentContinuationId 由 conversationId 确定性派生：同一会话两次转换必须相同，
        // 且保持 UUID 格式（上游按 36 字符 UUID 接受）。
        let again = convert_request(&req).unwrap();
        let first_acid = result.conversation_state.agent_continuation_id.clone().unwrap();
        let second_acid = again.conversation_state.agent_continuation_id.clone().unwrap();
        assert_eq!(first_acid, second_acid, "同一会话的 agentContinuationId 必须稳定");
        assert!(is_valid_uuid(&first_acid));
        assert_ne!(
            first_acid, result.conversation_state.conversation_id,
            "派生值不应与 conversationId 相同"
        );
        assert_eq!(
            first_acid,
            derive_agent_continuation_id("a0662283-7fd3-4399-a7eb-52b9a717ae88")
        );
        // 不同会话派生出不同值
        assert_ne!(first_acid, derive_agent_continuation_id("other-session"));
    }

    #[test]
    fn test_convert_request_without_metadata() {
        use super::super::types::Message as AnthropicMessage;

        // 测试没有 metadata 的请求，应该生成新的 UUID
        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };

        let result = convert_request(&req).unwrap();
        // 验证生成的是有效的 UUID 格式
        assert_eq!(result.conversation_state.conversation_id.len(), 36);
        assert_eq!(
            result
                .conversation_state
                .conversation_id
                .chars()
                .filter(|c| *c == '-')
                .count(),
            4
        );
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_result() {
        // 测试孤立的 tool_result 被过滤
        // 历史中没有 tool_use，但 tool_results 中有 tool_result
        let history = vec![
            Message::User(HistoryUserMessage::new("Hello", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage::new("Hi there!")),
        ];

        let tool_results = vec![ToolResult::success("orphan-123", "some result")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 孤立的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "孤立的 tool_result 应该被过滤");
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_use() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试孤立的 tool_use（有 tool_use 但没有对应的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-orphan", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // 没有 tool_result
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空（因为没有 tool_result）
        // 同时应该返回孤立的 tool_use_id
        assert!(filtered.is_empty());
        assert!(orphaned.contains("tool-orphan"));
    }

    #[test]
    fn test_validate_tool_pairing_valid() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试正常配对的情况
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_results = vec![ToolResult::success("tool-1", "file content")];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 配对成功，应该保留，无孤立
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_mixed() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试混合情况：部分配对成功，部分孤立
        let mut assistant_msg = AssistantMessage::new("I'll use two tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // tool_results: tool-1 配对，tool-3 孤立
        let tool_results = vec![
            ToolResult::success("tool-1", "result 1"),
            ToolResult::success("tool-3", "orphan result"), // 孤立
        ];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 只有 tool-1 应该保留
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        // tool-2 是孤立的 tool_use（无 result），tool-3 是孤立的 tool_result
        assert!(orphaned.contains("tool-2"));
    }

    #[test]
    fn test_validate_tool_pairing_history_already_paired() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试历史中已配对的 tool_use 不应该被报告为孤立
        // 场景：多轮对话中，之前的 tool_use 已经在历史中有对应的 tool_result
        let mut assistant_msg1 = AssistantMessage::new("I'll read the file.");
        assistant_msg1 = assistant_msg1.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 构建历史中的 user 消息，包含 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            // 第一轮：用户请求
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            // 第一轮：assistant 使用工具
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg1,
            }),
            // 第二轮：用户返回工具结果（历史中已配对）
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            // 第二轮：assistant 响应
            Message::Assistant(HistoryAssistantMessage::new("The file contains...")),
        ];

        // 当前消息没有 tool_results（用户只是继续对话）
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空，且不应该有孤立 tool_use
        // 因为 tool-1 已经在历史中配对了
        assert!(filtered.is_empty());
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_duplicate_result() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试重复的 tool_result（历史中已配对，当前消息又发送了相同的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 历史中已有 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            Message::Assistant(HistoryAssistantMessage::new("Done")),
        ];

        // 当前消息又发送了相同的 tool_result（重复）
        let tool_results = vec![ToolResult::success("tool-1", "file content again")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 重复的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "重复的 tool_result 应该被过滤");
    }

    #[test]
    fn test_convert_assistant_message_tool_use_only() {
        use super::super::types::Message as AnthropicMessage;

        // 测试仅包含 tool_use 的 assistant 消息（无 text 块）
        // Kiro API 要求 content 字段不能为空
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let result = convert_assistant_message(
            &msg,
            &mut HashMap::new(),
            ToolCompatibilityMode::Raw,
            ConversionPurpose::Generate,
            true,
        )
        .expect("应该成功转换");

        // 验证 content 不为空（使用占位符）
        assert!(
            !result.assistant_response_message.content.is_empty(),
            "content 不应为空"
        );
        assert_eq!(
            result.assistant_response_message.content, " ",
            "仅 tool_use 时应使用 ' ' 占位符"
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
        assert_eq!(tool_uses[0].name, "read_file");
    }

    #[test]
    fn test_convert_assistant_message_with_text_and_tool_use() {
        use super::super::types::Message as AnthropicMessage;

        // 测试同时包含 text 和 tool_use 的 assistant 消息
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "text", "text": "Let me read that file for you."},
                {"type": "tool_use", "id": "toolu_02XYZ", "name": "read_file", "input": {"path": "/data.json"}}
            ]),
        };

        let result = convert_assistant_message(
            &msg,
            &mut HashMap::new(),
            ToolCompatibilityMode::Raw,
            ConversionPurpose::Generate,
            true,
        )
        .expect("应该成功转换");

        // 验证 content 使用原始文本（不是占位符）
        assert_eq!(
            result.assistant_response_message.content,
            "Let me read that file for you."
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_02XYZ");
    }

    #[test]
    fn test_pair_orphaned_tool_uses_in_history() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 历史中段的孤立 tool_use：调用保留，结果补到紧跟它的那条 user 消息
        let mut assistant_msg = AssistantMessage::new("I'll use multiple tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-3", "delete").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
            Message::User(HistoryUserMessage::new("next", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage::new("ok")),
        ];

        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());
        orphaned.insert("tool-3".to_string());

        let for_current = pair_orphaned_tool_uses(&mut history, &orphaned);
        assert!(for_current.is_empty());

        if let Message::Assistant(ref assistant_msg) = history[1] {
            let tool_uses = assistant_msg
                .assistant_response_message
                .tool_uses
                .as_ref()
                .expect("应该还有 tool_uses");
            assert_eq!(tool_uses.len(), 3, "模型真实发起的调用都要保留");
        } else {
            panic!("应该是 Assistant 消息");
        }
        if let Message::User(ref user_msg) = history[2] {
            let results = &user_msg.user_input_message.user_input_message_context.tool_results;
            let ids: Vec<&str> = results.iter().map(|r| r.tool_use_id.as_str()).collect();
            assert_eq!(ids, vec!["tool-1", "tool-3"]);
            assert!(results.iter().all(|r| r.status.as_deref() == Some("error")));
        } else {
            panic!("应该是 User 消息");
        }
    }

    #[test]
    fn test_pair_orphaned_tool_uses_in_last_assistant() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 最后一条 assistant 的孤立 tool_use：结果交给调用方放进当前消息
        let mut assistant_msg = AssistantMessage::new("I'll use a tool.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());

        let for_current = pair_orphaned_tool_uses(&mut history, &orphaned);
        assert_eq!(for_current.len(), 1);
        assert_eq!(for_current[0].tool_use_id, "tool-1");
        if let Message::Assistant(ref assistant_msg) = history[1] {
            assert!(assistant_msg.assistant_response_message.tool_uses.is_some());
        } else {
            panic!("应该是 Assistant 消息");
        }
    }

    #[test]
    fn test_merge_consecutive_assistant_messages() {
        // 测试连续 assistant 消息被正确合并（Issue #79）
        use super::super::types::Message as AnthropicMessage;

        let msg1 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "Let me think about this..."},
                {"type": "text", "text": " "}
            ]),
        };

        let msg2 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "I should read the file."},
                {"type": "text", "text": "Let me read that file."},
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let messages: Vec<&AnthropicMessage> = vec![&msg1, &msg2];
        let result = merge_assistant_messages(
            &messages,
            &mut HashMap::new(),
            ToolCompatibilityMode::Raw,
            ConversionPurpose::Generate,
            true,
        )
        .expect("合并应成功");

        let content = &result.assistant_response_message.content;
        assert!(content.contains("<thinking>"), "应包含 thinking 标签");
        assert!(content.contains("Let me read that file"), "应包含第二条消息的 text 内容");

        let tool_uses = result.assistant_response_message.tool_uses.expect("应有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
    }

    #[test]
    fn test_consecutive_assistant_with_tool_use_result_pairing() {
        // 测试 Issue #79 的完整场景
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the config file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "I need to read the file..."},
                        {"type": "text", "text": " "}
                    ]),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "Let me read the config."},
                        {"type": "text", "text": "I'll read the config file for you."},
                        {"type": "tool_use", "id": "toolu_01XYZ", "name": "read_file", "input": {"path": "/config.json"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01XYZ", "content": "{\"key\": \"value\"}"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };

        let result = convert_request(&req);
        assert!(result.is_ok(), "连续 assistant 消息场景不应报错: {:?}", result.err());

        let state = result.unwrap().conversation_state;
        let mut found_tool_use = false;
        for msg in &state.history {
            if let Message::Assistant(assistant_msg) = msg {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    if tool_uses.iter().any(|t| t.tool_use_id == "toolu_01XYZ") {
                        found_tool_use = true;
                        break;
                    }
                }
            }
        }
        assert!(found_tool_use, "合并后的 assistant 消息应包含 tool_use");
    }

    // base64 of a 1x1 PNG (valid PNG header, so resize just passes it through)
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    #[test]
    fn test_tool_result_image_lifts_to_top_level() {
        use super::super::types::Message as AnthropicMessage;

        // user question -> assistant tool_use -> user tool_result (with image + text)
        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("take a screenshot"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "tool-1", "name": "screenshot", "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": [
                            {"type": "text", "text": "here is the screen"},
                            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}}
                        ]}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };

        let result = convert_request(&req).unwrap();
        let msg = &result.conversation_state.current_message.user_input_message;

        // image is lifted to the top-level images
        assert_eq!(msg.images.len(), 1, "image in tool_result should be lifted to top-level images");
        assert_eq!(msg.images[0].format, "png");
        assert_eq!(msg.images[0].source.bytes, TINY_PNG_B64);

        // tool_result itself keeps the text plus a marker saying where the image went (no base64)
        let tr = &msg.user_input_message_context.tool_results;
        assert_eq!(tr.len(), 1);
        assert_eq!(
            tr[0].content[0].get("text").and_then(|v| v.as_str()),
            Some("here is the screen\n[image #1 attached to this message]"),
            "tool_result content should keep the text and contain no base64"
        );
    }

    #[test]
    fn test_tool_result_text_only_unchanged() {
        use super::super::types::Message as AnthropicMessage;

        // text-only tool_result: regression unchanged, should produce no top-level image
        let req = MessagesRequest {
            model: "claude-sonnet-4.5".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("read the file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/a.txt"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };

        let result = convert_request(&req).unwrap();
        let msg = &result.conversation_state.current_message.user_input_message;

        assert!(msg.images.is_empty(), "text-only tool_result should produce no top-level image");
        let tr = &msg.user_input_message_context.tool_results;
        assert_eq!(tr.len(), 1);
        assert_eq!(
            tr[0].content[0].get("text").and_then(|v| v.as_str()),
            Some("file content"),
            "text-only tool_result content should be preserved as-is"
        );
    }

    // ---- fix-req 回归测试：请求方向审查 M1、L0–L12，响应方向审查 H3 ----
    mod fixreq {
        use super::*;
        use serde_json::json;

        const GIF_B64: &str = "R0lGODlhAQABAAAAACw=";

        fn convert(v: serde_json::Value) -> ConversionResult {
            convert_request(&request_from_json(v)).expect("conversion should succeed")
        }

        fn convert_raw(v: serde_json::Value) -> ConversionResult {
            convert_request_with_mode(&request_from_json(v), ToolCompatibilityMode::Raw)
                .expect("conversion should succeed")
        }

        fn assistant_contents(r: &ConversionResult) -> Vec<String> {
            r.conversation_state
                .history
                .iter()
                .filter_map(|m| match m {
                    Message::Assistant(a) => Some(a.assistant_response_message.content.clone()),
                    _ => None,
                })
                .collect()
        }

        fn history_users(r: &ConversionResult) -> Vec<&UserMessage> {
            r.conversation_state
                .history
                .iter()
                .filter_map(|m| match m {
                    Message::User(u) => Some(&u.user_input_message),
                    _ => None,
                })
                .collect()
        }

        fn current(r: &ConversionResult) -> &UserInputMessage {
            &r.conversation_state.current_message.user_input_message
        }

        fn result_text(r: &ToolResult) -> String {
            r.content
                .iter()
                .filter_map(|c| c.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn run_code_tool() -> serde_json::Value {
            json!({"name": "run_code", "description": "Run a program",
                   "input_schema": {"type": "object", "properties": {"code": {"type": "string"}}}})
        }

        fn has_fake_ok(r: &ConversionResult) -> bool {
            assistant_contents(r).iter().any(|c| c == "OK")
        }

        #[test]
        fn fixreq_m1_older_model_drops_finished_turn_thinking_only() {
            let r = convert(json!({
                "model": "claude-sonnet-4.5",
                "max_tokens": 1024,
                "thinking": {"type": "enabled", "budget_tokens": 2048},
                "messages": [
                    {"role": "user", "content": "first task"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "OLD_TURN_PLAN_31", "signature": "sig"},
                        {"type": "text", "text": "first task done"}
                    ]},
                    {"role": "user", "content": "second task"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "CURRENT_LOOP_PLAN_32", "signature": "sig"},
                        {"type": "tool_use", "id": "toolu_m1a", "name": "run_code", "input": {"code": "1"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_m1a", "content": "out"}
                    ]}
                ],
                "tools": [run_code_tool()]
            }));
            let assistants = assistant_contents(&r);
            assert!(
                !assistants.iter().any(|c| c.contains("OLD_TURN_PLAN_31")),
                "older models ignore thinking from finished turns; got {assistants:?}"
            );
            assert!(assistants.iter().any(|c| c == "first task done"));
            assert_eq!(
                assistants.last().map(String::as_str),
                Some("<thinking>CURRENT_LOOP_PLAN_32</thinking>"),
                "thinking of the turn still in the tool loop must be kept"
            );
        }

        #[test]
        fn fixreq_m1_older_model_new_user_text_ends_the_turn() {
            let r = convert(json!({
                "model": "claude-sonnet-4.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "do it"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "LOOP_PLAN_33", "signature": "sig"},
                        {"type": "tool_use", "id": "toolu_m1b", "name": "run_code", "input": {}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_m1b", "content": "ok"}
                    ]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "WRAP_UP_34", "signature": "sig"},
                        {"type": "text", "text": "all done"}
                    ]},
                    {"role": "user", "content": "thanks, next question"}
                ],
                "tools": [run_code_tool()]
            }));
            let assistants = assistant_contents(&r);
            assert!(!assistants.iter().any(|c| c.contains("<thinking>")), "{assistants:?}");
            assert_eq!(assistants, vec![" ".to_string(), "all done".to_string()]);
        }

        #[test]
        fn fixreq_m1_newer_model_keeps_thinking_in_order_with_separators() {
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "thinking": {"type": "adaptive"},
                "messages": [
                    {"role": "user", "content": "first task"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "OLD_TURN_PLAN_35", "signature": "sig"},
                        {"type": "text", "text": "first done"}
                    ]},
                    {"role": "user", "content": "second task"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "T1", "signature": "sig"},
                        {"type": "text", "text": "X"},
                        {"type": "redacted_thinking", "data": "REDACTED_SECRET_36"},
                        {"type": "thinking", "thinking": "T2", "signature": "sig"},
                        {"type": "text", "text": "Y"},
                        {"type": "tool_use", "id": "toolu_m1c", "name": "run_code", "input": {}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_m1c", "content": "out"}
                    ]}
                ],
                "tools": [run_code_tool()]
            }));
            let assistants = assistant_contents(&r);
            assert!(
                assistants.iter().any(|c| c == "<thinking>OLD_TURN_PLAN_35</thinking>\n\nfirst done"),
                "Opus 4.5+ keeps prior-turn thinking by default; got {assistants:?}"
            );
            assert_eq!(
                assistants.last().map(String::as_str),
                Some("<thinking>T1</thinking>\n\nX\n\n<thinking>T2</thinking>\n\nY"),
                "interleaved thinking and text must keep their order and be separated"
            );
            let body = serde_json::to_string(&r.conversation_state).unwrap();
            assert!(!body.contains("REDACTED_SECRET_36"));
        }

        #[test]
        fn fixreq_l0_mid_conversation_system_message_reaches_model() {
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "start"},
                    {"role": "assistant", "content": "started"},
                    {"role": "system", "content": "SYS_NOTE_41 tool set changed"},
                    {"role": "user", "content": "continue"}
                ]
            }));
            let content = &current(&r).content;
            assert!(
                content.contains("<system-reminder>\nSYS_NOTE_41 tool set changed\n</system-reminder>"),
                "{content}"
            );
            assert!(content.contains("continue"));
        }

        #[test]
        fn fixreq_l0_system_between_user_messages_has_no_fake_ok() {
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "first"},
                    {"role": "assistant", "content": "reply"},
                    {"role": "user", "content": "PART_ONE_51"},
                    {"role": "system", "content": [{"type": "text", "text": "SYS_MID_52"}]},
                    {"role": "user", "content": "PART_TWO_53"}
                ]
            }));
            assert!(!has_fake_ok(&r), "{:?}", assistant_contents(&r));
            assert!(matches!(r.conversation_state.history.last(), Some(Message::Assistant(_))));
            let content = &current(&r).content;
            for marker in ["PART_ONE_51", "SYS_MID_52", "PART_TWO_53"] {
                assert!(content.contains(marker), "{marker} missing from {content}");
            }
        }

        #[test]
        fn fixreq_l0_l10_trailing_system_is_not_dropped_as_prefill() {
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "first"},
                    {"role": "assistant", "content": "reply"},
                    {"role": "user", "content": "second question"},
                    {"role": "system", "content": "SYS_TAIL_61"}
                ]
            }));
            let content = &current(&r).content;
            assert!(content.contains("second question"), "{content}");
            assert!(content.contains("SYS_TAIL_61"), "{content}");

            // 真正的 assistant prefill 仍按原规则丢弃（Kiro 不支持续写）
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "q"},
                    {"role": "assistant", "content": "PARTIAL_62"}
                ]
            }));
            assert_eq!(current(&r).content, "q");
            assert!(!assistant_contents(&r).iter().any(|c| c.contains("PARTIAL_62")));
        }

        #[test]
        fn fixreq_l0_effort_only_system_messages_are_dropped() {
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "hi"},
                    {"role": "system", "content": [], "output_config": {"effort": "low"}},
                    {"role": "assistant", "content": "hello"},
                    {"role": "user", "content": "go"},
                    {"role": "system", "content": [], "output_config": {"effort": "low"}}
                ]
            }));
            assert_eq!(current(&r).content, "go");
            assert!(!has_fake_ok(&r));
            let body = serde_json::to_string(&r.conversation_state).unwrap();
            assert!(!body.contains("system-reminder"), "{body}");
        }

        #[test]
        fn fixreq_l0_unknown_role_is_dropped_not_rejected() {
            // convert() 内部 unwrap：修复前这里报 InvalidMessageSequence，测试会 panic
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "first"},
                    {"role": "assistant", "content": "reply"},
                    {"role": "user", "content": "BEFORE_61"},
                    {"role": "tool", "content": "STRAY_62"},
                    {"role": "user", "content": "AFTER_63"}
                ]
            }));
            assert!(!has_fake_ok(&r), "{:?}", assistant_contents(&r));
            let content = &current(&r).content;
            assert!(content.contains("BEFORE_61") && content.contains("AFTER_63"), "前后两条 user 都应进当前消息: {content}");
            let all = serde_json::to_string(&r.conversation_state).unwrap();
            assert!(!all.contains("STRAY_62"), "未知角色的内容不应进入上游请求");
        }

        #[test]
        fn fixreq_l1_client_system_prompt_is_labelled_with_neutral_ack() {
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "system": "SYS_BODY_71",
                "messages": [{"role": "user", "content": "hi"}]
            }));
            let first_user = &history_users(&r)[0].content;
            assert!(
                first_user.contains("<client_system_prompt>\nSYS_BODY_71\n</client_system_prompt>"),
                "{first_user}"
            );
            let ack = &assistant_contents(&r)[0];
            assert_ne!(ack, "I will follow these instructions.");
            assert!(ack.contains("client system prompt"), "{ack}");
        }

        #[test]
        fn fixreq_l1_chunked_policy_does_not_ask_for_silence() {
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "system": "sys",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "Write", "description": "Write a file",
                           "input_schema": {"type": "object", "properties": {}}}]
            }));
            let first_user = &history_users(&r)[0].content;
            assert!(first_user.contains(
                "Complete all chunked operations without stopping to ask for confirmation."
            ));
            assert!(!first_user.contains("without commentary"), "{first_user}");
            assert!(!first_user.contains("silently"), "{first_user}");
        }

        #[test]
        fn fixreq_l2_empty_system_text_keeps_thinking_prefix() {
            for system in [json!(""), json!([{"type": "text", "text": ""}])] {
                let r = convert(json!({
                    "model": "claude-opus-5.5",
                    "max_tokens": 1024,
                    "system": system,
                    "thinking": {"type": "adaptive"},
                    "output_config": {"effort": "xhigh"},
                    "messages": [{"role": "user", "content": "hi"}]
                }));
                let users = history_users(&r);
                let first = users.first().expect("thinking prefix pair should exist");
                assert!(first.content.contains(
                    "<thinking_mode>adaptive</thinking_mode><thinking_effort>xhigh</thinking_effort>"
                ));
            }
        }

        #[test]
        fn fixreq_l2_adaptive_prefix_effort_matches_wire_effort() {
            let r = convert(json!({
                "model": "claude-opus-4.7",
                "max_tokens": 1024,
                "thinking": {"type": "adaptive", "budget_tokens": 3000},
                "messages": [{"role": "user", "content": "hi"}]
            }));
            let wire = r
                .additional_model_request_fields
                .as_ref()
                .and_then(|f| f.output_config.as_ref())
                .map(|c| c.effort.clone());
            assert_eq!(wire.as_deref(), Some("low"));
            let first = &history_users(&r)[0].content;
            assert!(first.contains("<thinking_effort>low</thinking_effort>"), "{first}");
        }

        #[test]
        fn fixreq_l3_placeholder_tool_is_marked_unavailable() {
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "go"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_l3", "name": "old_tool", "input": {}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_l3", "content": "done"}
                    ]}
                ],
                "tools": [run_code_tool()]
            }));
            let tools = &current(&r).user_input_message_context.tools;
            let placeholder = tools
                .iter()
                .find(|t| t.tool_specification.name == "old_tool")
                .expect("history tool needs a placeholder definition");
            let d = &placeholder.tool_specification.description;
            assert!(d.contains("not available"), "{d}");
            assert!(d.contains("Do not call it"), "{d}");
        }

        #[test]
        fn fixreq_l4_tool_result_images_are_numbered() {
            let mut req = json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "read both images"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_img1", "name": "read_image", "input": {"path": "a.png"}},
                        {"type": "tool_use", "id": "toolu_img2", "name": "read_image", "input": {"path": "b.gif"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_img1", "content": [
                            {"type": "text", "text": "first result"},
                            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}}
                        ]},
                        {"type": "tool_result", "tool_use_id": "toolu_img2", "content": [
                            {"type": "image", "source": {"type": "base64", "media_type": "image/gif", "data": GIF_B64}}
                        ]}
                    ]}
                ]
            });
            let expected = [
                "first result\n[image #1 attached to this message]",
                "[image #2 attached to this message]",
            ];
            let r = convert(req.clone());
            let cur = current(&r);
            assert_eq!(cur.images.len(), 2);
            let texts: Vec<String> =
                cur.user_input_message_context.tool_results.iter().map(result_text).collect();
            assert_eq!(texts, expected);

            // 下一轮这条消息进了历史，编号不变
            req["messages"].as_array_mut().unwrap().extend([
                json!({"role": "assistant", "content": "both read"}),
                json!({"role": "user", "content": "thanks"}),
            ]);
            let r = convert(req);
            let users = history_users(&r);
            let in_history = users.last().unwrap();
            assert_eq!(in_history.images.len(), 2);
            let texts: Vec<String> = in_history
                .user_input_message_context
                .tool_results
                .iter()
                .map(result_text)
                .collect();
            assert_eq!(texts, expected);
        }

        #[test]
        fn fixreq_l5_repeated_image_is_deduplicated_in_current_message_too() {
            let image = json!({"type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}});
            let mut req = json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": [image.clone(), {"type": "text", "text": "look"}]},
                    {"role": "assistant", "content": "seen"},
                    {"role": "user", "content": [image.clone(), {"type": "text", "text": "again"}]}
                ]
            });
            let r1 = convert(req.clone());
            let cur = current(&r1);
            assert!(cur.images.is_empty(), "repeated image must not be re-sent");
            assert!(cur.content.contains("same image as"), "{}", cur.content);
            assert!(!cur.content.contains("screenshot"), "{}", cur.content);
            let round1_content = cur.content.clone();

            req["messages"].as_array_mut().unwrap().extend([
                json!({"role": "assistant", "content": "ok"}),
                json!({"role": "user", "content": "more"}),
            ]);
            let r2 = convert(req);
            let users = history_users(&r2);
            let moved = users.last().unwrap();
            assert_eq!(moved.content, round1_content, "a message must not change once it is in history");
            assert!(moved.images.is_empty());
        }

        #[test]
        fn fixreq_l6_unsupported_content_leaves_placeholders() {
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "see attachments"},
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/heic", "data": "AAAA"}},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpg", "data": TINY_PNG_B64}},
                    {"type": "document", "title": "Q3 report",
                     "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0="}}
                ]}]
            }));
            let cur = current(&r);
            assert_eq!(cur.images.len(), 1, "image/jpg is a JPEG alias");
            assert!(cur.content.contains("url image source"), "{}", cur.content);
            assert!(cur.content.contains("image/heic"), "{}", cur.content);
            assert!(cur.content.contains("Q3 report"), "{}", cur.content);

            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "search"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_l6", "name": "kb_search", "input": {}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_l6", "content": [
                            {"type": "text", "text": "r"},
                            {"type": "search_result", "source": "https://kb/1", "title": "T",
                             "content": [{"type": "text", "text": "snippet"}]}
                        ]}
                    ]}
                ]
            }));
            let text = result_text(&current(&r).user_input_message_context.tool_results[0]);
            assert!(text.starts_with("r\n"), "{text}");
            assert!(text.contains("search_result"), "{text}");
        }

        #[test]
        fn fixreq_l7_orphaned_tool_use_gets_an_interrupted_result() {
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "run it"},
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "running"},
                        {"type": "tool_use", "id": "toolu_orphan", "name": "run_code", "input": {"code": "x"}}
                    ]},
                    {"role": "user", "content": "never mind, do something else"}
                ],
                "tools": [run_code_tool()]
            }));
            let kept = r.conversation_state.history.iter().any(|m| matches!(m,
                Message::Assistant(a) if a.assistant_response_message.tool_uses.as_ref()
                    .is_some_and(|t| t.iter().any(|u| u.tool_use_id == "toolu_orphan"))));
            assert!(kept, "the model's real tool call must stay in history");
            let results = &current(&r).user_input_message_context.tool_results;
            let synth = results
                .iter()
                .find(|t| t.tool_use_id == "toolu_orphan")
                .expect("orphaned call needs a result");
            assert_eq!(synth.status.as_deref(), Some("error"));
            assert!(result_text(synth).contains("interrupted"));

            // 历史中段的孤立调用：结果补到紧跟它的那条 user 消息里
            let r = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [
                    {"role": "user", "content": "go"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_a1", "name": "run_code", "input": {}},
                        {"type": "tool_use", "id": "toolu_a2", "name": "run_code", "input": {}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_a1", "content": "r1"}
                    ]},
                    {"role": "assistant", "content": "done"},
                    {"role": "user", "content": "next"}
                ],
                "tools": [run_code_tool()]
            }));
            let users = history_users(&r);
            let ids: Vec<&str> = users[1]
                .user_input_message_context
                .tool_results
                .iter()
                .map(|t| t.tool_use_id.as_str())
                .collect();
            assert_eq!(ids, vec!["toolu_a1", "toolu_a2"]);
        }

        #[test]
        fn fixreq_l7_resent_tool_result_with_new_content_is_kept() {
            let build = |resent: &str| {
                json!({
                    "model": "claude-opus-5.5",
                    "max_tokens": 1024,
                    "messages": [
                        {"role": "user", "content": "go"},
                        {"role": "assistant", "content": [
                            {"type": "tool_use", "id": "toolu_dup", "name": "run_code", "input": {}}
                        ]},
                        {"role": "user", "content": [
                            {"type": "tool_result", "tool_use_id": "toolu_dup", "content": "OLD_OUT_81"}
                        ]},
                        {"role": "assistant", "content": "ok"},
                        {"role": "user", "content": [
                            {"type": "tool_result", "tool_use_id": "toolu_dup", "content": resent},
                            {"type": "text", "text": "check"}
                        ]}
                    ],
                    "tools": [run_code_tool()]
                })
            };
            let r = convert(build("NEW_OUT_82"));
            let cur = current(&r);
            assert!(cur.content.contains("NEW_OUT_82"), "{}", cur.content);
            assert!(cur.content.contains("check"));
            assert!(cur.user_input_message_context.tool_results.is_empty());

            let r = convert(build("OLD_OUT_81"));
            assert_eq!(current(&r).content, "check", "identical resend adds nothing");
        }

        #[test]
        fn fixreq_l9_shortened_multibyte_name_fits_byte_limit() {
            let name = format!("{}_tool", "工具".repeat(20));
            assert!(name.len() > TOOL_NAME_MAX_LEN);
            let short = shorten_tool_name(&name);
            assert!(short.len() <= TOOL_NAME_MAX_LEN, "{} bytes: {short}", short.len());

            // 全 ASCII 名称的缩短结果与以前一致（54 字节前缀 + "_" + 8 位哈希）
            let ascii = "mcp__some_very_long_server_name__some_very_long_tool_name_that_exceeds_limit";
            let s = shorten_tool_name(ascii);
            assert_eq!(s.len(), TOOL_NAME_MAX_LEN);
            assert_eq!(&s[..54], &ascii[..54]);
        }

        #[test]
        fn fixreq_l12_conversation_id_is_derived_from_content_without_session() {
            let base = json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "system": "agent system prompt",
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "build the parser", "cache_control": {"type": "ephemeral"}}
                ]}]
            });
            let id = |v: serde_json::Value| convert(v).conversation_state.conversation_id;
            let id1 = id(base.clone());
            assert_eq!(id1, id(base.clone()), "same conversation must keep its id");
            assert!(Uuid::parse_str(&id1).is_ok());

            let mut next = base.clone();
            next["messages"][0]["content"][0]
                .as_object_mut()
                .unwrap()
                .remove("cache_control");
            next["messages"].as_array_mut().unwrap().extend([
                json!({"role": "assistant", "content": "parsed"}),
                json!({"role": "user", "content": [
                    {"type": "text", "text": "now tests", "cache_control": {"type": "ephemeral"}}
                ]}),
            ]);
            assert_eq!(id(next), id1, "later rounds and moved cache breakpoints keep the id");

            let mut other = base.clone();
            other["messages"][0]["content"][0]["text"] = json!("a different task");
            assert_ne!(id(other), id1);

            let mut with_session = base;
            with_session["metadata"] = json!({
                "user_id": "user_x_account__session_a0662283-7fd3-4399-a7eb-52b9a717ae88"
            });
            assert_eq!(id(with_session), "a0662283-7fd3-4399-a7eb-52b9a717ae88");
        }

        #[test]
        fn fixreq_l12_string_and_single_text_block_first_message_share_id() {
            // pi-ai 第一轮：首条 user 是带 cache_control 的块数组；第二轮起：同一条消息是纯字符串。
            let as_block = json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "system": "agent system prompt",
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "build the parser", "cache_control": {"type": "ephemeral"}}
                ]}]
            });
            let as_string = json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "system": "agent system prompt",
                "messages": [
                    {"role": "user", "content": "build the parser"},
                    {"role": "assistant", "content": "parsed"},
                    {"role": "user", "content": [{"type": "text", "text": "now tests", "cache_control": {"type": "ephemeral"}}]}
                ]
            });
            let id = |v: serde_json::Value| convert(v).conversation_state.conversation_id;
            assert_eq!(id(as_block), id(as_string));
        }

        #[test]
        fn fixreq_l12_session_id_must_be_a_real_uuid() {
            assert_eq!(
                extract_session_id("user_x_account__session_zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz"),
                None
            );
            assert_eq!(
                extract_session_id(r#"{"session_id":"zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz"}"#),
                None
            );
            assert_eq!(
                extract_session_id("user_x_account__session_8bb5523b-ec7c-4540-a9ca-beb6d79f1552")
                    .as_deref(),
                Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552")
            );
        }

        #[test]
        fn fixreq_h3_raw_mode_keeps_client_tool_input() {
            let r = convert_raw(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "search"}],
                "tools": [
                    {"name": "web_search", "description": "Search the web",
                     "input_schema": {"type": "object", "properties": {
                         "queries": {"type": "array", "items": {"type": "string"}}}}},
                    {"name": "read_file", "description": "Read a file",
                     "input_schema": {"type": "object", "properties": {
                         "path": {"type": "string"}, "start_line": {"type": "number"}}}}
                ]
            }));
            let input = json!({"queries": ["kiro gateway", "rust"]});
            let (name, restored) =
                restore_tool_use_for_client("web_search", input.clone(), &r.tool_name_map);
            assert_eq!(name, "web_search");
            assert_eq!(restored, input, "raw mode must not rewrite client tool input");

            let input = json!({"path": "/a", "start_line": 3, "custom": true});
            let (name, restored) =
                restore_tool_use_for_client("read_file", input.clone(), &r.tool_name_map);
            assert_eq!(name, "read_file");
            assert_eq!(restored, input);

            // ClaudeCode 模式下确实做过名字映射的工具照旧还原
            let cc = convert(json!({
                "model": "claude-opus-5.5",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "search"}],
                "tools": [{"name": "WebSearch", "description": "Search",
                           "input_schema": {"type": "object", "properties": {}}}]
            }));
            let (name, restored) = restore_tool_use_for_client(
                "web_search",
                json!({"query": "q", "extra": 1}),
                &cc.tool_name_map,
            );
            assert_eq!(name, "WebSearch");
            assert_eq!(restored, json!({"query": "q"}));
        }
    }
}
