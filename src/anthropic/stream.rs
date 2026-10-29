//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::collections::HashMap;

use serde_json::json;
use uuid::Uuid;

use crate::kiro::model::events::{Event, MeteringEvent, TokenUsage};

/// thinking 块的 signature 占位字符串
///
/// Anthropic Messages API 协议规定 thinking 模式下，assistant 消息的
/// `{type:"thinking", ...}` 块必须带 `signature` 字段并在下一轮原样回传，
/// 否则 SDK / 服务端会拒绝请求并报：
/// `The content[].thinking in the thinking mode must be passed back to the API`。
///
/// 上游 Kiro 不下发真实签名（它本身不是 Anthropic 服务端），因此 kiro.rs 在
/// thinking 块结束时插入一个非空占位字符串以满足客户端本地校验。
/// converter 在解析 assistant 消息回传 Kiro 时只读 `block.thinking`，不读
/// signature，因此该占位字符串只在客户端 ↔ kiro.rs 之间存在，不会影响转发。
pub(super) const THINKING_SIGNATURE_PLACEHOLDER: &str = "kiro-rs-thinking-signature";

/// 上游内容过滤拦截（`metadataEvent.stopReason = CONTENT_FILTERED`）映射到的
/// Anthropic `stop_reason`。一经设置即为终态，后续事件不得覆盖。
pub(super) const REFUSAL_STOP_REASON: &str = "refusal";

use crate::kiro::model::events::{MarkdownCodeTracker, ToolUseXmlLeakFilter};

/// 找到小于等于目标位置的最近有效UTF-8字符边界
///
/// UTF-8字符可能占用1-4个字节，直接按字节位置切片可能会切在多字节字符中间导致panic。
/// 这个函数从目标位置向前搜索，找到最近的有效字符边界。
fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    if target == 0 {
        return 0;
    }
    // 从目标位置向前搜索有效的字符边界
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// 需要跳过的包裹字符
///
/// 当 thinking 标签被这些字符包裹时，认为是在引用标签而非真正的标签：
/// - 反引号 (`)：行内代码
/// - 双引号 (")：字符串
/// - 单引号 (')：字符串
const QUOTE_CHARS: &[u8] = &[
    b'`', b'"', b'\'', b'\\', b'#', b'!', b'@', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'-',
    b'_', b'=', b'+', b'[', b']', b'{', b'}', b';', b':', b'<', b'>', b',', b'.', b'?', b'/',
];

/// 检查指定位置的字符是否是引用字符
fn is_quote_char(buffer: &str, pos: usize) -> bool {
    buffer
        .as_bytes()
        .get(pos)
        .map(|c| QUOTE_CHARS.contains(c))
        .unwrap_or(false)
}

/// 查找真正的 thinking 结束标签（不被引用字符包裹，且后面有双换行符）
///
/// 当模型在思考过程中提到 `</thinking>` 时，通常会用反引号、引号等包裹，
/// 或者在同一行有其他内容（如"关于 </thinking> 标签"）。
/// 这个函数会跳过这些情况，只返回真正的结束标签位置。
///
/// 跳过的情况：
/// - 被引用字符包裹（反引号、引号等）
/// - 后面没有双换行符（真正的结束标签后面会有 `\n\n`）
/// - 标签在缓冲区末尾（流式处理时需要等待更多内容）
///
/// # 参数
/// - `buffer`: 要搜索的字符串
///
/// # 返回值
/// - `Some(pos)`: 真正的结束标签的起始位置
/// - `None`: 没有找到真正的结束标签
fn find_real_thinking_end_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果被引用字符包裹，跳过
        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 检查后面的内容
        let after_content = &buffer[after_pos..];

        // 如果标签后面内容不足以判断是否有双换行符，等待更多内容
        if after_content.len() < 2 {
            return None;
        }

        // 真正的 thinking 结束标签后面是换行：`\n\n`，或一个 `\n` 后紧跟正文（L1）。
        if after_content.starts_with("\n\n")
            || (after_content.starts_with('\n') && !after_content[1..].starts_with('\n'))
        {
            return Some(absolute_pos);
        }

        // 不是双换行符，跳过继续搜索
        search_start = absolute_pos + 1;
    }

    None
}

/// `find_real_thinking_end_tag` 命中位置之后要剥掉的字节数：标签本身加其后的 1–2 个换行。
fn thinking_end_tag_strip_len(buffer: &str, pos: usize) -> usize {
    let after = pos + "</thinking>".len();
    let newlines = buffer[after..]
        .bytes()
        .take(2)
        .take_while(|&b| b == b'\n')
        .count();
    "</thinking>".len() + newlines
}

/// 查找缓冲区末尾的 thinking 结束标签（允许末尾只有空白字符）
///
/// 用于“边界事件”场景：例如 thinking 结束后立刻进入 tool_use，或流结束，
/// 此时 `</thinking>` 后面可能没有 `\n\n`，但结束标签依然应被识别并过滤。
///
/// 约束：只有当 `</thinking>` 之后全部都是空白字符时才认为是结束标签，
/// 以避免在 thinking 内容中提到 `</thinking>`（非结束标签）时误判。
fn find_real_thinking_end_tag_at_buffer_end(buffer: &str) -> Option<usize> {
    const TAG: &str = "</thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        if has_quote_before || has_quote_after {
            search_start = absolute_pos + 1;
            continue;
        }

        // 只有当标签后面全部是空白字符时才认定为结束标签
        if buffer[after_pos..].trim().is_empty() {
            return Some(absolute_pos);
        }

        search_start = absolute_pos + 1;
    }

    None
}

/// 查找真正的 thinking 开始标签（不被引用字符包裹）
///
/// 与 `find_real_thinking_end_tag` 类似，跳过被引用字符包裹的开始标签。
fn find_real_thinking_start_tag(buffer: &str) -> Option<usize> {
    const TAG: &str = "<thinking>";
    let mut search_start = 0;

    while let Some(pos) = buffer[search_start..].find(TAG) {
        let absolute_pos = search_start + pos;

        // 检查前面是否有引用字符
        let has_quote_before = absolute_pos > 0 && is_quote_char(buffer, absolute_pos - 1);

        // 检查后面是否有引用字符
        let after_pos = absolute_pos + TAG.len();
        let has_quote_after = is_quote_char(buffer, after_pos);

        // 如果不被引用字符包裹，则是真正的开始标签
        if !has_quote_before && !has_quote_after {
            return Some(absolute_pos);
        }

        // 继续搜索下一个匹配
        search_start = absolute_pos + 1;
    }

    None
}

/// 检查 `name_pos`（指向标签名首字母）的前面是否构成合法的开标签起始，
/// 兼容裸写法 `<tag` 和带命名空间前缀的写法 `<prefix:tag`。
///
/// 返回 `Some(lt_pos)`（指向 `<` 的字节位置）表示合法；`None` 表示不是标签。
fn open_tag_lt_pos(buffer: &str, name_pos: usize) -> Option<usize> {
    let bytes = buffer.as_bytes();
    if name_pos == 0 {
        return None;
    }
    let prev = bytes[name_pos - 1];
    if prev == b'<' {
        return Some(name_pos - 1);
    }
    // 形如 `<prefix:tag`：name 前面是 ':'，再往前是一段标识符，再往前是 '<'
    if prev == b':' {
        let i = name_pos - 1; // 指向 ':'
        let mut j = i; // 标识符左边界扫描
        while j > 0 && {
            let c = bytes[j - 1];
            c.is_ascii_alphanumeric() || c == b'_'
        } {
            j -= 1;
        }
        // 标识符非空，且其左边是 '<'
        if j < i && j > 0 && bytes[j - 1] == b'<' {
            return Some(j - 1);
        }
    }
    None
}

/// 查找未被引用字符包裹的 invoke 开标签，返回指向 `<` 的字节位置
///
/// 兼容裸 `<invoke ...>` 与带命名空间前缀 `<prefix:invoke ...>` 两种写法。
/// 复用 `is_quote_char`：若 `<` 前紧贴反引号/引号等包裹字符，视为引用，跳过。
fn find_invoke_start(buffer: &str) -> Option<usize> {
    let mut search = 0;
    while let Some(rel) = buffer[search..].find("invoke") {
        let name_pos = search + rel;
        if let Some(lt) = open_tag_lt_pos(buffer, name_pos) {
            // 标签名后必须是边界字符（空白或 '>'），避免误匹配 invoked 之类
            let after = name_pos + "invoke".len();
            let next_ok = buffer.as_bytes().get(after).map_or(true, |c| {
                c.is_ascii_whitespace() || *c == b'>' || *c == b'/'
            });
            let has_quote_before = lt > 0 && is_quote_char(buffer, lt - 1);
            if next_ok && !has_quote_before {
                return Some(lt);
            }
        }
        search = name_pos + "invoke".len();
    }
    None
}

/// 从 `start` 之后查找第一个 invoke 闭标签，返回结束位置（exclusive，含闭标签）
///
/// 兼容裸 `</invoke>` 与带前缀 `</prefix:invoke>`。找不到返回 `None`（块还没到齐）。
fn find_invoke_block_end(buffer: &str, start: usize) -> Option<usize> {
    // 块 A 的边界 = 下一个 `<invoke` 开标签（即下一个块 B 的起点），没有则到 buffer 结尾。
    // 这样连发 burst（A 紧跟 B）时，A 的搜索区间被 B 的开标签卡住，绝不会吃进 B。
    let boundary = match find_next_invoke_open(buffer, start) {
        Some(p) => p,
        None => buffer.len(),
    };
    // 在 [start, boundary) 区间里取【最后一个】 `</invoke>` 作为真闭合。
    // 贪婪取最后一个 → patch 正文里出现的字面 `</invoke>` 不会导致提前截断；
    // 区间被下一个块开标签卡住 → 不会跨块误合并。
    find_last_invoke_close(buffer, start, boundary)
}

/// 从 `start` 之后查找下一个真正的 `<invoke`（或 `<prefix:invoke`）开标签的字节位置。
/// 跳过 `start` 处当前块自身的开标签。
fn find_next_invoke_open(buffer: &str, start: usize) -> Option<usize> {
    // 先跳过当前块的开标签：从 start 之后第一个 '>' 之后开始找。
    let after_open = match buffer[start..].find('>') {
        Some(rel) => start + rel + 1,
        None => return None,
    };
    // 注意：不能复用 find_invoke_start——它对 `<` 前是 `>`（引用字符）的情况会拒绝，
    // 而连发 burst 里 B 的 `<invoke` 恰好紧跟在 A 的 `</invoke>` 的 `>` 后面。
    // 这里只认结构：`<invoke` 或 `<prefix:invoke`，开标签名后须是空白/`>`/`/` 边界。
    let region = &buffer[after_open..];
    let mut search = 0usize;
    while let Some(rel) = region[search..].find("invoke") {
        let name_pos = search + rel;
        if let Some(lt) = open_tag_lt_pos(region, name_pos) {
            let after = name_pos + "invoke".len();
            let next_ok = region.as_bytes().get(after).map_or(true, |c| {
                c.is_ascii_whitespace() || *c == b'>' || *c == b'/'
            });
            if next_ok {
                return Some(after_open + lt);
            }
        }
        search = name_pos + "invoke".len();
    }
    None
}

/// 在 `[from, boundary)` 区间内查找最后一个 `</invoke>` / `</prefix:invoke>` 的结束位置
/// （exclusive，含闭标签）。找不到返回 `None`（块还没到齐）。
fn find_last_invoke_close(buffer: &str, from: usize, boundary: usize) -> Option<usize> {
    let region_end = boundary.min(buffer.len());
    if from >= region_end {
        return None;
    }
    let region = &buffer[from..region_end];
    let bytes = region.as_bytes();
    let mut search = 0usize;
    let mut last: Option<usize> = None;
    while let Some(rel) = region[search..].find("invoke>") {
        let name_pos = search + rel;
        // '</invoke>' 形式
        if name_pos >= 2 && &region[name_pos - 2..name_pos] == "</" {
            last = Some(from + name_pos + "invoke>".len());
        } else if name_pos >= 1 && bytes[name_pos - 1] == b':' {
            // '</prefix:invoke>' 形式
            let mut j = name_pos - 1; // ':'
            while j > 0 && {
                let c = bytes[j - 1];
                c.is_ascii_alphanumeric() || c == b'_'
            } {
                j -= 1;
            }
            if j >= 2 && &region[j - 2..j] == "</" {
                last = Some(from + name_pos + "invoke>".len());
            }
        }
        search = name_pos + "invoke>".len();
    }
    last
}

/// 从标签字符串中抠出 `name="..."` 的值（取第一个匹配）
fn extract_name_attr(tag: &str) -> Option<String> {
    let needle = "name=\"";
    let rel = tag.find(needle)?;
    let start = rel + needle.len();
    let end_rel = tag[start..].find('"')?;
    Some(tag[start..start + end_rel].to_string())
}

/// 严格解析一个完整 invoke 块，返回 (tool_name, [(参数名, 原始字符串值)])。
///
/// - tool name 来自 invoke 开标签的 `name="..."`（兼容前缀写法）；
/// - 参数值取到下一个参数开标签前的**最后一个** `</parameter>` 为界（允许多行 / 含 `<` /
///   含字面 `</parameter>`）；
/// - 参数之间、参数与闭标签之间只允许空白。参数未闭合、重名、有游离正文都返回 `None`：
///   拿不准就不合成，整块按正文保留。
fn parse_invoke_block_strict(block: &str) -> Option<(String, Vec<(String, String)>)> {
    let open_end = block.find('>')?;
    let tool_name = extract_name_attr(&block[..=open_end])?;
    if tool_name.is_empty() {
        return None;
    }
    let body = &block[open_end + 1..];
    // 块尾的闭标签（`</invoke>` / `</prefix:invoke>`）。
    let inner = &body[..body.rfind("</")?];

    let mut params: Vec<(String, String)> = Vec::new();
    let mut cursor = 0usize;
    loop {
        let rest = &inner[cursor..];
        let Some(lt_rel) = rest.find('<') else {
            if !rest.trim().is_empty() {
                return None;
            }
            break;
        };
        if !rest[..lt_rel].trim().is_empty() {
            return None;
        }
        let lt = cursor + lt_rel;
        let name_kw = lt + inner[lt..].find("parameter")?;
        if open_tag_lt_pos(inner, name_kw) != Some(lt) {
            return None;
        }
        let tag_gt = name_kw + inner[name_kw..].find('>')?;
        let key = extract_name_attr(&inner[name_kw..=tag_gt])?;
        if key.is_empty() || params.iter().any(|(k, _)| *k == key) {
            return None;
        }
        let val_start = tag_gt + 1;
        let (close_start, close_end) = find_param_close(inner, val_start)?;
        params.push((key, inner[val_start..close_start].to_string()));
        cursor = close_end;
    }
    Some((tool_name, params))
}

/// 按工具 input_schema 把 invoke 的字符串参数恢复成带类型的 JSON 对象（H2）。
///
/// 拿不准就返回 `None`（整块按正文保留）：
/// - schema 声明了 `properties` 时，未声明的参数只有 `additionalProperties` 显式允许才接受；
/// - `required` 里的参数必须都在；
/// - 参数按 `type` 解析（见 [`coerce_invoke_value`]），有 `enum` 时值必须在枚举里。
fn coerce_invoke_params(
    params: &[(String, String)],
    schema: &serde_json::Value,
) -> Option<serde_json::Value> {
    let properties = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .filter(|p| !p.is_empty());
    let extra_allowed = match schema.get("additionalProperties") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Object(_)) => true,
        _ => properties.is_none(),
    };
    let mut out = serde_json::Map::new();
    for (key, raw) in params {
        let value = match properties.and_then(|p| p.get(key)) {
            Some(prop) => coerce_invoke_value(raw, prop)?,
            None if extra_allowed => serde_json::Value::String(raw.clone()),
            None => return None,
        };
        out.insert(key.clone(), value);
    }
    if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
        if required
            .iter()
            .filter_map(|v| v.as_str())
            .any(|name| !out.contains_key(name))
        {
            return None;
        }
    }
    Some(serde_json::Value::Object(out))
}

/// 按单个参数的 schema 解析字符串值。integer / number / boolean / null 严格匹配，
/// array / object 须能解析成对应 JSON，string 原样保留；多类型时非字符串类型优先
/// （`60000` 对 `integer|string` 恢复成数字）。没有类型约束（含 anyOf 里无类型的分支）
/// 时按字符串保留。
fn coerce_invoke_value(raw: &str, schema: &serde_json::Value) -> Option<serde_json::Value> {
    fn collect<'a>(schema: &'a serde_json::Value, types: &mut Vec<&'a str>) -> bool {
        match schema.get("type") {
            Some(serde_json::Value::String(t)) => types.push(t.as_str()),
            Some(serde_json::Value::Array(ts)) => types.extend(ts.iter().filter_map(|t| t.as_str())),
            _ => return false,
        }
        true
    }
    let mut types: Vec<&str> = Vec::new();
    let mut untyped_branch = false;
    let top_typed = collect(schema, &mut types);
    let mut has_branches = false;
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(key).and_then(|v| v.as_array()) {
            for branch in branches {
                has_branches = true;
                if !collect(branch, &mut types) {
                    untyped_branch = true;
                }
            }
        }
    }
    if untyped_branch || (!top_typed && !has_branches) {
        types.push("string");
    }
    let value = match types
        .iter()
        .filter(|t| **t != "string")
        .find_map(|t| coerce_invoke_scalar(raw, t))
    {
        Some(v) => v,
        None if types.contains(&"string") => serde_json::Value::String(raw.to_string()),
        None => return None,
    };
    if let Some(allowed) = schema.get("enum").and_then(|v| v.as_array()) {
        if !allowed.contains(&value) {
            return None;
        }
    }
    Some(value)
}

fn coerce_invoke_scalar(raw: &str, ty: &str) -> Option<serde_json::Value> {
    let t = raw.trim();
    let json_of = |pred: fn(&serde_json::Value) -> bool| {
        serde_json::from_str::<serde_json::Value>(t)
            .ok()
            .filter(|v| pred(v))
    };
    match ty {
        "integer" => t.parse::<i64>().ok().map(serde_json::Value::from),
        "number" => json_of(serde_json::Value::is_number),
        "boolean" => match t {
            "true" => Some(serde_json::Value::Bool(true)),
            "false" => Some(serde_json::Value::Bool(false)),
            _ => None,
        },
        "null" => (t == "null").then_some(serde_json::Value::Null),
        "array" => json_of(serde_json::Value::is_array),
        "object" => json_of(serde_json::Value::is_object),
        _ => None,
    }
}

/// 从 `from` 开始查找第一个 parameter 闭标签，返回 (起始位置, 结束位置 exclusive)
///
/// 兼容裸 `</parameter>` 与带前缀 `</prefix:parameter>`。
fn find_param_close(body: &str, from: usize) -> Option<(usize, usize)> {
    // P0-1：参数值（尤其 apply_patch 的 patch 正文）可能含字面 `</parameter>`。
    // 朴素「取第一个 </parameter>」会把值截断。改成「贪婪取边界内最后一个 </parameter>」：
    // 边界 = 下一个 `<parameter name="` 开标签（多参数场景），没有则到 body 结尾。
    // 这样：① 单参数（含 apply_patch）取到真正的最后一个闭合，内容里的字面闭合不误伤；
    //      ② 多参数仍按下一个参数开标签正确切分。
    // 局限（已诚实标注）：若参数值里同时含字面 `<parameter name="`，边界判定会偏早；
    // 实测 apply_patch 正文极少出现该字面串，可接受。
    let boundary = match find_next_param_open(body, from) {
        Some(p) => p,
        None => body.len(),
    };
    let region = &body[from..boundary];
    let kw = "parameter>";
    let mut last: Option<(usize, usize)> = None;
    let mut search = 0usize;
    let bytes = region.as_bytes();
    while let Some(rel) = region[search..].find(kw) {
        let name_pos = search + rel;
        // '</parameter>' 形式
        if name_pos >= 2 && &region[name_pos - 2..name_pos] == "</" {
            last = Some((from + name_pos - 2, from + name_pos + kw.len()));
        } else if name_pos >= 1 && bytes[name_pos - 1] == b':' {
            // '</prefix:parameter>' 形式
            let mut j = name_pos - 1; // ':'
            while j > 0 && {
                let c = bytes[j - 1];
                c.is_ascii_alphanumeric() || c == b'_'
            } {
                j -= 1;
            }
            if j >= 2 && &region[j - 2..j] == "</" {
                last = Some((from + j - 2, from + name_pos + kw.len()));
            }
        }
        search = name_pos + kw.len();
    }
    last
}

/// 从 `from` 开始查找下一个 `<parameter name="`（或 `<prefix:parameter name="`）开标签的字节位置。
/// 用于 `find_param_close` 的贪婪边界：当前参数值最多吃到下一个参数开标签之前。
fn find_next_param_open(body: &str, from: usize) -> Option<usize> {
    let mut search = from;
    while let Some(rel) = body[search..].find("parameter name=\"") {
        let kw_pos = search + rel;
        // 必须是真正的开标签：'parameter' 前面是 '<' 或 '<prefix:'
        if let Some(lt) = open_tag_lt_pos(body, kw_pos) {
            return Some(lt);
        }
        search = kw_pos + "parameter".len();
    }
    None
}

/// 剥掉块前文本尾部的独立 stray token 行（单独一行的 `call` 或 `count`）
///
/// 实测里 `<invoke>` 前常出现一行裸 `call`/`count`，需要从块前叙述文本里剥掉，
/// 避免泄漏给客户端。只剥“尾部、且独占一行”的 stray token，前面的正常叙述保留。
/// 已实测到的 stray token 集合：Opus 长上下文退化时，泄漏的 `<invoke>` 前常有一行裸的
/// `call` / `count` / `card`。集合形式便于以后扩充。
const STRAY_INVOKE_TOKENS: &[&str] = &["call", "count", "card"];

/// 复读熔断阈值：同一个 stray token（call/count/card）连续作为独占一行重复出现
/// 超过这么多次，判定为「Opus 长上下文退化复读死循环」，立即熔断本轮文本输出。
///
/// 取值权衡：正常工具调用前最多出现 1 个引导词行（偶有 2~3），绝不会连续几十次。
/// 设为 32 远高于正常上限、又远低于退化时的数万次，既不误伤正常引导词，又能尽早止血。
const REPEAT_GUARD_TRIP_THRESHOLD: u32 = 32;

/// 复读熔断触发后返回给客户端的错误（L5）。措辞让客户端按「流中断、可重试」处理。
pub(crate) const REPEAT_GUARD_ERROR_MESSAGE: &str = "Upstream response stream was interrupted: the model output degenerated into a repeated-token loop. Retry the request.";

/// 块级复读折叠：对「已完整的整段文本」做一次性复读熔断。
///
/// 用于非流式 / web_search loop 路径（`extract_invoke_content_blocks` 入口）——
/// 那条路不经过流式 `emit_text_delta_raw` 的逐 chunk 熔断，所以在这里独立兜一次。
///
/// 规则与流式版一致：同一个 `STRAY_INVOKE_TOKENS`（call/count/card）连续作为独占一行
/// 重复超过 `REPEAT_GUARD_TRIP_THRESHOLD` 次，判定为 Opus 退化复读，**从超阈值处截断**，
/// 丢弃其后的全部复读垃圾（断雪球、不灌历史）。阈值内的少量引导词重复原样保留。
fn collapse_stray_token_floods(text: &str) -> std::borrow::Cow<'_, str> {
    let mut last_line = "";
    let mut run: u32 = 0;
    let mut cut_at: Option<usize> = None;
    let mut offset = 0usize;
    for segment in text.split_inclusive('\n') {
        let line = segment.trim();
        if STRAY_INVOKE_TOKENS.contains(&line) {
            if line == last_line {
                run += 1;
            } else {
                last_line = line;
                run = 1;
            }
            if run >= REPEAT_GUARD_TRIP_THRESHOLD {
                // 从「本段（这一行）开头」截断：保留阈值内已累计的内容。
                cut_at = Some(offset);
                break;
            }
        } else if !line.is_empty() {
            last_line = line;
            run = 0;
        }
        offset += segment.len();
    }
    match cut_at {
        Some(pos) => std::borrow::Cow::Owned(text[..pos].to_string()),
        None => std::borrow::Cow::Borrowed(text),
    }
}

fn strip_trailing_stray_tokens(before: &str) -> &str {
    let mut end = before.len();
    loop {
        let bytes = before.as_bytes();
        // 先跳过尾部的换行符，定位“最后一行”的真实结束位置
        let mut e = end;
        while e > 0 && (bytes[e - 1] == b'\n' || bytes[e - 1] == b'\r') {
            e -= 1;
        }
        let line_start = before[..e].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let last_line = before[line_start..e].trim();
        // Opus 长上下文退化时，泄漏的 <invoke> 前常有一个孤立的 stray token 行。
        // 实测样本里出现过 call / count / card 三种；用集合便于以后扩充。
        if STRAY_INVOKE_TOKENS.contains(&last_line) {
            // 只剥 stray token 行本身，【保留】前一行末尾的换行符。
            // 旧实现用 line_start - 1 把前一行的换行也吞掉，会把前面的叙述正文和
            // 后续 <invoke> 挤到同一行，导致 invoke_looks_like_real_leak 的“行首”判定
            // 失败、漏捞真泄漏（narrative\ncall\n<invoke>）。改成 end = line_start：
            //   "some text\ncall" -> "some text\n"（行首信号保留）
            //   "call"（无前导正文）-> ""（line_start==0）
            end = line_start;
            if end == 0 {
                return "";
            }
        } else {
            break;
        }
    }
    &before[..end]
}

/// `<invoke>` 前的文本是否让它处在行首：前面没有文本，或上一行已经以换行结束。
/// 同一行前面有任何字符（包括缩进空白、列表符号、引用符号）都不算行首。
fn invoke_looks_like_real_leak(before: &str) -> bool {
    before.is_empty() || before.ends_with('\n')
}

/// 已输出正文的状态 `code` 之后再接 `before`，下一个字符是否位于行首。
fn at_line_start_after(code: &MarkdownCodeTracker, before: &str) -> bool {
    if before.is_empty() {
        code.at_line_start()
    } else {
        invoke_looks_like_real_leak(before)
    }
}

/// 允许从 `<invoke>` 文本合成工具调用的工具：Kiro 侧工具名 → 发给上游的 input_schema。
/// 只包含本次请求声明的工具；为空时不合成任何调用。
pub(crate) type InvokeToolSchemas = HashMap<String, serde_json::Value>;

/// 从发给上游的请求里取出本次声明工具的 input_schema（与模型看到的完全一致）。
/// 只保留 `known_tool_names` 里的工具，排除为历史补的占位工具。
pub(crate) fn invoke_tool_schemas_from_state(
    state: &crate::kiro::model::requests::conversation::ConversationState,
    known_tool_names: &std::collections::HashSet<String>,
) -> InvokeToolSchemas {
    state
        .current_message
        .user_input_message
        .user_input_message_context
        .tools
        .iter()
        .filter(|tool| known_tool_names.contains(&tool.tool_specification.name))
        .map(|tool| {
            (
                tool.tool_specification.name.clone(),
                tool.tool_specification.input_schema.json.clone(),
            )
        })
        .collect()
}

/// 行首出现 `<invoke` 后最多等多少字节的 `</invoke>`；超过就按正文放出，避免一个永不闭合的
/// 标签扣住后续全部回复。正常的 invoke（包括大 patch）远小于此。
const MAX_INVOKE_HOLD_BYTES: usize = 262_144;

/// 已识别、暂时扣住的末尾工具调用组。
#[derive(Debug)]
struct InvokeCandidate {
    /// 被扣住的原文（含块前的 stray token 行），放弃合成时原样吐回。
    raw: String,
    /// Kiro 侧工具名 + 已按 schema 恢复类型的入参。
    calls: Vec<(String, serde_json::Value)>,
}

/// [`InvokeSniffer`] 的输出，保持原文顺序。
#[derive(Debug)]
pub(crate) enum SniffOutput {
    Text(String),
    /// Kiro 侧工具名 + 已按 schema 恢复类型的入参。
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
}

/// 从明文里识别模型写成文本的 `<invoke>` 工具调用（H2）。
///
/// 模型在正文里讲解或举例的工具调用不能变成真实调用，所以只有同时满足以下条件才合成，
/// 其余一律原样作为正文：
/// - 本轮没有原生 tool_use（出现原生调用后调用 [`InvokeSniffer::abandon`]）；
/// - 块位于整段回复末尾：之后只剩空白或同样合格的块，中途再出现正文就整组按正文放出；
/// - 块在行首（允许前面一行是孤立的 stray token），且不在 ``` / ~~~ 围栏、缩进代码块、行内反引号里；
/// - 工具名在本次请求声明的工具里，参数能按该工具的 input_schema 解析（类型按 schema 恢复）。
#[derive(Debug, Default)]
pub(crate) struct InvokeSniffer {
    buffer: String,
    code: MarkdownCodeTracker,
    candidate: Option<InvokeCandidate>,
    disabled: bool,
}

impl InvokeSniffer {
    /// 喂入一段明文，返回可以立即输出的内容。
    pub(crate) fn push(&mut self, text: &str, schemas: &InvokeToolSchemas) -> Vec<SniffOutput> {
        self.buffer.push_str(text);
        let mut out = Vec::new();
        self.drain(schemas, false, &mut out);
        out
    }

    /// 流结束：末尾仍成立的候选块合成为工具调用，其余全部作为正文放出。
    pub(crate) fn finish(&mut self, schemas: &InvokeToolSchemas) -> Vec<SniffOutput> {
        let mut out = Vec::new();
        self.drain(schemas, true, &mut out);
        if let Some(candidate) = self.candidate.take() {
            // drain(at_end) 之后仍有候选，说明其后只剩空白：丢弃空白，合成调用。
            self.buffer.clear();
            out.extend(
                candidate
                    .calls
                    .into_iter()
                    .map(|(name, input)| SniffOutput::ToolCall { name, input }),
            );
        }
        let rest = std::mem::take(&mut self.buffer);
        self.emit_text(rest, &mut out);
        out
    }

    /// 本轮出现了原生 tool_use：不再合成，已扣住的文本按原顺序放出。
    pub(crate) fn abandon(&mut self) -> Vec<SniffOutput> {
        self.disabled = true;
        let mut out = Vec::new();
        self.release_candidate(&mut out);
        let rest = std::mem::take(&mut self.buffer);
        self.emit_text(rest, &mut out);
        out
    }

    fn emit_text(&mut self, text: String, out: &mut Vec<SniffOutput>) {
        if text.is_empty() {
            return;
        }
        self.code.advance(&text);
        if let Some(SniffOutput::Text(last)) = out.last_mut() {
            last.push_str(&text);
        } else {
            out.push(SniffOutput::Text(text));
        }
    }

    fn emit_prefix(&mut self, len: usize, out: &mut Vec<SniffOutput>) {
        let text: String = self.buffer.drain(..len).collect();
        self.emit_text(text, out);
    }

    fn release_candidate(&mut self, out: &mut Vec<SniffOutput>) {
        if let Some(candidate) = self.candidate.take() {
            self.emit_text(candidate.raw, out);
        }
    }

    fn drain(&mut self, schemas: &InvokeToolSchemas, at_end: bool, out: &mut Vec<SniffOutput>) {
        if self.disabled || schemas.is_empty() {
            self.release_candidate(out);
            let rest = std::mem::take(&mut self.buffer);
            self.emit_text(rest, out);
            return;
        }
        loop {
            if self.candidate.is_some() {
                let rest = self.buffer.as_str();
                let gap_len = rest.len() - rest.trim_start().len();
                if gap_len == rest.len() {
                    // 候选之后只有空白：继续等（流结束时由 finish 合成）。
                    return;
                }
                let gap_ok = gap_len == 0 || rest[..gap_len].ends_with('\n');
                if gap_ok {
                    let tail = &rest[gap_len..];
                    if find_invoke_start(tail) == Some(0) {
                        match find_invoke_block_end(tail, 0) {
                            Some(end) => {
                                if let Some(call) = validate_invoke_block(&tail[..end], schemas) {
                                    let raw: String = self.buffer.drain(..gap_len + end).collect();
                                    if let Some(candidate) = self.candidate.as_mut() {
                                        candidate.raw.push_str(&raw);
                                        candidate.calls.push(call);
                                    }
                                    continue;
                                }
                            }
                            None if !at_end
                                && tail.len() <= MAX_INVOKE_HOLD_BYTES
                                && !open_invoke_tag_rejected(tail, schemas) =>
                            {
                                return;
                            }
                            None => {}
                        }
                    } else if !at_end && could_become_invoke_open(tail) {
                        return;
                    }
                }
                // 候选块之后又出现了正文：整组按正文放出，再按普通文本继续扫描。
                self.release_candidate(out);
                continue;
            }

            let Some(start) = find_invoke_start(&self.buffer) else {
                let hold = if at_end { 0 } else { self.partial_invoke_hold_len() };
                let emit = self.buffer.len() - hold;
                self.emit_prefix(emit, out);
                return;
            };
            let before = &self.buffer[..start];
            let stripped_len = strip_trailing_stray_tokens(before).len();
            let eligible = {
                let stripped = &before[..stripped_len];
                let mut probe = self.code.clone();
                probe.advance(stripped);
                at_line_start_after(&self.code, before)
                    && at_line_start_after(&self.code, stripped)
                    && !probe.in_code()
            };
            if !eligible {
                self.emit_prefix(start + 1, out);
                continue;
            }
            match find_invoke_block_end(&self.buffer, start) {
                Some(end) => match validate_invoke_block(&self.buffer[start..end], schemas) {
                    Some(call) => {
                        self.emit_prefix(stripped_len, out);
                        let raw: String = self.buffer.drain(..end - stripped_len).collect();
                        self.candidate = Some(InvokeCandidate {
                            raw,
                            calls: vec![call],
                        });
                    }
                    None => self.emit_prefix(end, out),
                },
                None => {
                    let tail = &self.buffer[start..];
                    if at_end
                        || tail.len() > MAX_INVOKE_HOLD_BYTES
                        || open_invoke_tag_rejected(tail, schemas)
                    {
                        self.emit_prefix(start + 1, out);
                    } else {
                        // 块还没到齐：先放出块前正文，块（含 stray token 行）留着等。
                        self.emit_prefix(stripped_len, out);
                        return;
                    }
                }
            }
        }
    }

    /// 缓冲区末尾可能长成行首 `<invoke` 开标签的字节数（需要留着等下一段）。
    fn partial_invoke_hold_len(&self) -> usize {
        let buf = self.buffer.as_str();
        let Some(lt) = buf.rfind('<') else {
            return 0;
        };
        if !could_become_invoke_open(&buf[lt..]) || !at_line_start_after(&self.code, &buf[..lt]) {
            return 0;
        }
        buf.len() - lt
    }
}

/// `tail`（以 `<` 开头）是否还可能长成 `<invoke` / `<prefix:invoke` 开标签。
fn could_become_invoke_open(tail: &str) -> bool {
    const NAME: &str = "invoke";
    let Some(rest) = tail.strip_prefix('<') else {
        return false;
    };
    if rest.len() > 64 {
        return false;
    }
    let is_ident = |s: &str| s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_');
    match rest.find(':') {
        Some(colon) => {
            let prefix = &rest[..colon];
            !prefix.is_empty() && is_ident(prefix) && NAME.starts_with(&rest[colon + 1..])
        }
        // 可能是裸 `<invoke` 的前缀，也可能是还没写到 `:` 的命名空间前缀。
        None => is_ident(rest),
    }
}

/// 未闭合块的开标签已经完整、但工具名不在可合成表里：不必再等，直接按正文放出。
fn open_invoke_tag_rejected(tail: &str, schemas: &InvokeToolSchemas) -> bool {
    match tail.find('>') {
        Some(gt) => extract_name_attr(&tail[..=gt]).is_none_or(|name| !schemas.contains_key(&name)),
        None => false,
    }
}

/// 解析并按 schema 校验一个完整 invoke 块，返回 (Kiro 侧工具名, 带类型的入参)。
fn validate_invoke_block(
    block: &str,
    schemas: &InvokeToolSchemas,
) -> Option<(String, serde_json::Value)> {
    let (name, params) = parse_invoke_block_strict(block)?;
    let schema = schemas.get(&name)?;
    let input = coerce_invoke_params(&params, schema)?;
    Some((name, input))
}

/// 从完整文本中提取 thinking 块（用于非流式响应）
///
/// 使用与流式处理相同的标签检测逻辑（引用字符过滤），确保一致性。
/// 非流式场景下文本已完整，无需处理跨 chunk 分割问题。
///
/// # 返回值
/// - `(Some(thinking_content), remaining_text)` — 检测到有效 thinking 块
/// - `(None, original_text)` — 未检测到，原样返回
pub(crate) fn extract_thinking_from_complete_text(text: &str) -> (Option<String>, String) {
    // M1：只有回复开头（去掉前导空白后）的 <thinking> 才是思考块；正文中间的字面量按正文保留。
    let start_pos = match find_real_thinking_start_tag(text) {
        Some(pos) if text[..pos].trim().is_empty() => pos,
        _ => return (None, text.to_string()),
    };

    let before = &text[..start_pos];
    let after_open = &text[start_pos + "<thinking>".len()..];

    // 查找结束标签：优先匹配带 \n\n 后缀的，退而使用末尾匹配
    let (thinking_raw, text_after) = if let Some(end_pos) = find_real_thinking_end_tag(after_open) {
        (
            &after_open[..end_pos],
            &after_open[end_pos + thinking_end_tag_strip_len(after_open, end_pos)..],
        )
    } else if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(after_open) {
        let after_tag = end_pos + "</thinking>".len();
        (&after_open[..end_pos], after_open[after_tag..].trim_start())
    } else {
        // 找不到有效的结束标签，不做提取
        return (None, text.to_string());
    };

    // 剥离开头的换行符（与流式处理一致：模型输出 <thinking>\n）
    let thinking_content = thinking_raw.strip_prefix('\n').unwrap_or(thinking_raw);

    // 组装剩余文本：跳过纯空白的 before 部分
    let mut remaining = String::new();
    if !before.trim().is_empty() {
        remaining.push_str(before);
    }
    remaining.push_str(text_after);

    if thinking_content.is_empty() {
        (None, remaining)
    } else {
        (Some(thinking_content.to_string()), remaining)
    }
}

/// 一次性（非流式 / 整段已完整）把 assistant 文本切成 Anthropic content block 序列。
///
/// 与流式路径共用 [`InvokeSniffer`]，规则完全一致（H2）：只有回复末尾、行首、代码外、
/// 工具已声明且参数符合 schema 的 `<invoke>` 块才合成 tool_use，其余原样作为文本。
/// 调用方负责在已有原生 tool_use 时不调用本函数。
pub(crate) fn extract_invoke_content_blocks(
    text: &str,
    schemas: &InvokeToolSchemas,
    tool_name_map: &std::collections::HashMap<String, String>,
) -> Vec<serde_json::Value> {
    // 块级复读熔断：先截断「同一 stray token 连续复读」，再识别 invoke。
    let collapsed = collapse_stray_token_floods(text);
    let mut sniffer = InvokeSniffer::default();
    let mut outputs = sniffer.push(&collapsed, schemas);
    outputs.extend(sniffer.finish(schemas));

    let mut blocks: Vec<serde_json::Value> = Vec::new();
    let mut pending_text = String::new();
    for output in outputs {
        match output {
            SniffOutput::Text(t) => pending_text.push_str(&t),
            SniffOutput::ToolCall { name, input } => {
                if !pending_text.is_empty() {
                    blocks.push(json!({"type": "text", "text": std::mem::take(&mut pending_text)}));
                }
                let tool_use_id = format!("toolu_{}", Uuid::new_v4().to_string().replace('-', ""));
                let completed = CompletedToolUse::from_kiro(tool_use_id, &name, input, tool_name_map);
                blocks.push(completed.to_anthropic_block());
            }
        }
    }
    if !pending_text.is_empty() {
        blocks.push(json!({"type": "text", "text": pending_text}));
    }
    blocks
}

/// 整段文本里是否出现了会被复读熔断截断的 stray token 洪泛（L5，非流式判定用）。
pub(crate) fn stray_token_flood_detected(text: &str) -> bool {
    matches!(
        collapse_stray_token_floods(text),
        std::borrow::Cow::Owned(_)
    )
}

/// 累积完成的工具调用（`ToolUseEvent` 的所有分片拼接、解析成功后的结果）。
#[derive(Debug, Clone)]
pub struct CompletedToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

impl CompletedToolUse {
    /// 从 Kiro 侧 (name, input) 还原为客户端可见的完整工具调用。
    ///
    /// 这是**唯一的还原入口**：名字按 `tool_name_map` 还原、入参按 Kiro 名反向重写
    /// （见 `converter::restore_tool_use_for_client`）。结构化事件、`<invoke>` 文本捞回、
    /// websearch 三条来源都经此收敛，避免各站点各自调用还原逻辑。
    pub fn from_kiro(
        id: String,
        kiro_name: &str,
        input: serde_json::Value,
        tool_name_map: &HashMap<String, String>,
    ) -> Self {
        let (name, input) =
            super::converter::restore_tool_use_for_client(kiro_name, input, tool_name_map);
        Self { id, name, input }
    }

    /// 产出非流式 Anthropic `tool_use` 内容块。**唯一的非流式块拼装点。**
    pub fn to_anthropic_block(&self) -> serde_json::Value {
        json!({
            "type": "tool_use",
            "id": self.id,
            "name": self.name,
            "input": self.input,
        })
    }
}

/// 工具调用 JSON 累积过程中的错误。
///
/// - `InvalidJson`：上游把某个 tool_use 的完整 `input` 拼出来后，仍不是合法 JSON。
/// - `IncompleteJson`：整条流结束时仍有 tool_use 累积了**非空**内容却从未收到
///   `stop=true`，即上游在工具参数写到一半时截断（“流式半截 JSON”）。
///   0 字节的滞留缓冲是无参工具的正常形态，不算截断，见 `ToolJsonAccumulator::finish`。
///
/// 两种情况都**不能**把半截 / 非法 JSON 当成完整工具调用转发给客户端——那会让
/// 客户端拿到无法解析或语义错误的参数去执行工具。这里显式暴露为错误，由上层
/// 决定回 502（非流式 / 缓冲流）或在 SSE 里补一个 `error` 事件（实时流）。
#[derive(Debug, Clone)]
pub enum ToolJsonAccumulatorError {
    InvalidJson {
        tool_use_id: String,
        name: String,
        message: String,
    },
    IncompleteJson {
        tool_use_id: String,
        name: String,
        bytes: usize,
    },
}

impl ToolJsonAccumulatorError {
    /// Anthropic error 事件里统一的 error.type。
    pub fn error_type(&self) -> &'static str {
        "upstream_tool_json_error"
    }

    pub fn message(&self) -> String {
        match self {
            Self::InvalidJson {
                tool_use_id,
                name,
                message,
            } => format!(
                "Upstream returned invalid JSON for tool_use {} ({}): {}",
                tool_use_id, name, message
            ),
            Self::IncompleteJson {
                tool_use_id,
                name,
                bytes,
            } => format!(
                "Upstream ended before completing tool_use {} ({}) JSON input; buffered {} bytes. The tool call was not forwarded to the client.",
                tool_use_id, name, bytes
            ),
        }
    }
}

impl std::fmt::Display for ToolJsonAccumulatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ToolJsonAccumulatorError {}

/// 工具调用参数（JSON）累积器。
///
/// Kiro 把 tool_use 的 `input` JSON 拆成多个 `toolUseEvent` 分片下发，最后一片
/// 带 `stop=true`。分片可能切在 JSON 的任意字节位置（甚至 token 中间），因此
/// **不能**逐片当作 `input_json_delta` 直接转发——必须按 `tool_use_id` 累积，
/// 只在收到 `stop=true` 时整体解析，成功后一次性发出完整的工具调用。
#[derive(Debug, Default)]
pub struct ToolJsonAccumulator {
    /// tool_use_id -> (工具名, 已累积的 JSON 分片)
    buffers: HashMap<String, (String, String)>,
}

impl ToolJsonAccumulator {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
        }
    }

    /// 累积一个 `toolUseEvent` 分片。
    ///
    /// - 未收到 `stop` 时返回 `Ok(None)`（继续缓冲，不发出任何事件）。
    /// - 收到 `stop` 时把累积的 JSON 整体解析：成功返回 `Ok(Some(CompletedToolUse))`，
    ///   失败返回 `Err(InvalidJson)`。空参数按 `{}` 处理。
    /// - 工具名按 `tool_name_map` 还原为客户端原始名（短名 → 原名）。
    pub fn push(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
        tool_name_map: &HashMap<String, String>,
    ) -> Result<Option<CompletedToolUse>, ToolJsonAccumulatorError> {
        let entry = self
            .buffers
            .entry(tool_use.tool_use_id.clone())
            .or_insert_with(|| (tool_use.name.clone(), String::new()));
        if entry.0.is_empty() {
            entry.0 = tool_use.name.clone();
        }
        entry.1.push_str(&tool_use.input);

        if !tool_use.stop {
            return Ok(None);
        }

        let (kiro_name, input_json) = self
            .buffers
            .remove(&tool_use.tool_use_id)
            .unwrap_or_else(|| (tool_use.name.clone(), tool_use.input.clone()));
        let input = if input_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str::<serde_json::Value>(&input_json).map_err(|e| {
                ToolJsonAccumulatorError::InvalidJson {
                    tool_use_id: tool_use.tool_use_id.clone(),
                    name: kiro_name.clone(),
                    message: e.to_string(),
                }
            })?
        };

        // 通过统一入口还原客户端工具名 + 入参。
        Ok(Some(CompletedToolUse::from_kiro(
            tool_use.tool_use_id.clone(),
            &kiro_name,
            input,
            tool_name_map,
        )))
    }

    /// 流结束时收尾：取出所有从未收到 `stop=true` 的缓冲。
    ///
    /// 无参工具（`input_schema` 无属性）只会收到一条 `toolUseEvent`：`input` 为空、
    /// `stop` 缺省为 false。没有后续参数分片要发，上游也就永远不会补一条
    /// `stop=true`，缓冲因此滞留且**为 0 字节**。这不是截断——没有"写到一半"的
    /// JSON 可言——按 `{}` 还原成完整工具调用返回，与 `push` 收到 `stop=true`
    /// 时对空 `input` 的处理保持一致。
    ///
    /// 只有累积到**非空**内容却从未 `stop` 才是上游在参数中途截断，返回
    /// `IncompleteJson`（取字节数最多的那个作代表）。
    pub fn finish(
        &mut self,
        tool_name_map: &HashMap<String, String>,
    ) -> Result<Vec<CompletedToolUse>, ToolJsonAccumulatorError> {
        let (empty, truncated): (Vec<_>, Vec<_>) = std::mem::take(&mut self.buffers)
            .into_iter()
            .partition(|(_, (_, input))| input.trim().is_empty());

        if let Some((tool_use_id, (name, input))) = truncated
            .into_iter()
            .max_by_key(|(_, (_, input))| input.len())
        {
            return Err(ToolJsonAccumulatorError::IncompleteJson {
                tool_use_id,
                name,
                bytes: input.len(),
            });
        }

        // HashMap 迭代序不稳定；按 tool_use_id 排序，保证多个无参调用并发滞留时
        // 发出的内容块顺序可复现。
        let mut recovered = empty;
        recovered.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(recovered
            .into_iter()
            .map(|(tool_use_id, (kiro_name, _))| {
                CompletedToolUse::from_kiro(
                    tool_use_id,
                    &kiro_name,
                    serde_json::json!({}),
                    tool_name_map,
                )
            })
            .collect())
    }
}

/// SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 格式化为 SSE 字符串
    pub fn to_sse_string(&self) -> String {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event,
            serde_json::to_string(&self.data).unwrap_or_default()
        )
    }
}

/// 内容块状态
#[derive(Debug, Clone)]
struct BlockState {
    block_type: String,
    started: bool,
    stopped: bool,
}

impl BlockState {
    fn new(block_type: impl Into<String>) -> Self {
        Self {
            block_type: block_type.into(),
            started: false,
            stopped: false,
        }
    }
}

/// SSE 状态管理器
///
/// 确保 SSE 事件序列符合 Claude API 规范：
/// 1. message_start 只能出现一次
/// 2. content_block 必须先 start 再 delta 再 stop
/// 3. message_delta 只能出现一次，且在所有 content_block_stop 之后
/// 4. message_stop 在最后
#[derive(Debug)]
pub struct SseStateManager {
    /// message_start 是否已发送
    message_started: bool,
    /// message_delta 是否已发送
    message_delta_sent: bool,
    /// 活跃的内容块状态
    active_blocks: HashMap<i32, BlockState>,
    /// 消息是否已结束
    message_ended: bool,
    /// 下一个块索引
    next_block_index: i32,
    /// 当前 stop_reason
    stop_reason: Option<String>,
    /// 是否有工具调用
    has_tool_use: bool,
}

impl Default for SseStateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SseStateManager {
    pub fn new() -> Self {
        Self {
            message_started: false,
            message_delta_sent: false,
            active_blocks: HashMap::new(),
            message_ended: false,
            next_block_index: 0,
            stop_reason: None,
            has_tool_use: false,
        }
    }

    /// 判断指定块是否处于可接收 delta 的打开状态
    fn is_block_open_of_type(&self, index: i32, expected_type: &str) -> bool {
        self.active_blocks
            .get(&index)
            .is_some_and(|b| b.started && !b.stopped && b.block_type == expected_type)
    }

    /// 获取下一个块索引
    pub fn next_block_index(&mut self) -> i32 {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

    /// 记录工具调用
    pub fn set_has_tool_use(&mut self, has: bool) {
        self.has_tool_use = has;
    }

    /// 设置 stop_reason。`refusal` 是终态：拦截后同一条流里再到的
    /// contextUsage / exception 不得把它覆盖成 max_tokens 之类的可重试原因。
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        if self.stop_reason.as_deref() == Some(REFUSAL_STOP_REASON) {
            return;
        }
        self.stop_reason = Some(reason.into());
    }

    /// 获取最终的 stop_reason
    pub fn get_stop_reason(&self) -> String {
        if let Some(ref reason) = self.stop_reason {
            reason.clone()
        } else if self.has_tool_use {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }
    }

    /// 处理 message_start 事件
    pub fn handle_message_start(&mut self, event: serde_json::Value) -> Option<SseEvent> {
        if self.message_started {
            tracing::debug!("跳过重复的 message_start 事件");
            return None;
        }
        self.message_started = true;
        Some(SseEvent::new("message_start", event))
    }

    /// 处理 content_block_start 事件
    pub fn handle_content_block_start(
        &mut self,
        index: i32,
        block_type: &str,
        data: serde_json::Value,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果是 tool_use 块，先关闭之前的文本块
        if block_type == "tool_use" {
            self.has_tool_use = true;
            for (block_index, block) in self.active_blocks.iter_mut() {
                if block.block_type == "text" && block.started && !block.stopped {
                    // 自动发送 content_block_stop 关闭文本块
                    events.push(SseEvent::new(
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop",
                            "index": block_index
                        }),
                    ));
                    block.stopped = true;
                }
            }
        }

        // 检查块是否已存在
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.started {
                tracing::debug!("块 {} 已启动，跳过重复的 content_block_start", index);
                return events;
            }
            block.started = true;
        } else {
            let mut block = BlockState::new(block_type);
            block.started = true;
            self.active_blocks.insert(index, block);
        }

        events.push(SseEvent::new("content_block_start", data));
        events
    }

    /// 处理 content_block_delta 事件
    pub fn handle_content_block_delta(
        &mut self,
        index: i32,
        data: serde_json::Value,
    ) -> Option<SseEvent> {
        // 确保块已启动
        if let Some(block) = self.active_blocks.get(&index) {
            if !block.started || block.stopped {
                tracing::warn!(
                    "块 {} 状态异常: started={}, stopped={}",
                    index,
                    block.started,
                    block.stopped
                );
                return None;
            }
        } else {
            // 块不存在，可能需要先创建
            tracing::warn!("收到未知块 {} 的 delta 事件", index);
            return None;
        }

        Some(SseEvent::new("content_block_delta", data))
    }

    /// 处理 content_block_stop 事件
    pub fn handle_content_block_stop(&mut self, index: i32) -> Option<SseEvent> {
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.stopped {
                tracing::debug!("块 {} 已停止，跳过重复的 content_block_stop", index);
                return None;
            }
            block.stopped = true;
            return Some(SseEvent::new(
                "content_block_stop",
                json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        None
    }

    /// 生成最终事件序列
    pub fn generate_final_events(
        &mut self,
        input_tokens: i32,
        output_tokens: i32,
        cache_creation_input_tokens: i32,
        cache_read_input_tokens: i32,
        metering: Option<&MeteringEvent>,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 关闭所有未关闭的块
        for (index, block) in self.active_blocks.iter_mut() {
            if block.started && !block.stopped {
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop",
                        "index": index
                    }),
                ));
                block.stopped = true;
            }
        }

        // 发送 message_delta
        if !self.message_delta_sent {
            self.message_delta_sent = true;
            let mut usage_json = json!({
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cache_creation_input_tokens": cache_creation_input_tokens,
                "cache_read_input_tokens": cache_read_input_tokens
            });
            // 透传上游 meteringEvent 的 credit_* 字段，让客户端拿到与 Kiro
            // 后端口径一致的计费元数据；只在收到过 meteringEvent 时才追加。
            if let Some(m) = metering {
                usage_json["credit_usage"] = json!(m.usage);
                usage_json["credit_unit"] = json!(m.unit);
                usage_json["credit_unit_plural"] = json!(m.unit_plural);
            }
            events.push(SseEvent::new(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": self.get_stop_reason(),
                        "stop_sequence": null
                    },
                    "usage": usage_json
                }),
            ));
        }

        // 发送 message_stop
        if !self.message_ended {
            self.message_ended = true;
            events.push(SseEvent::new(
                "message_stop",
                json!({ "type": "message_stop" }),
            ));
        }

        events
    }

    /// 生成异常终止事件序列。
    ///
    /// 流已经开始后无法再修改 HTTP 状态码，因此先关闭仍处于打开状态的内容块，
    /// 再发送 Anthropic `error` 事件。异常路径不能发送 `message_delta` 或
    /// `message_stop`，否则下游会把中断的响应误判为正常完成。
    pub fn generate_error_events(&mut self, error_type: &str, message: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        for (index, block) in self.active_blocks.iter_mut() {
            if block.started && !block.stopped {
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop",
                        "index": index
                    }),
                ));
                block.stopped = true;
            }
        }

        self.message_ended = true;
        events.push(SseEvent::new(
            "error",
            json!({
                "type": "error",
                "error": {
                    "type": error_type,
                    "message": message
                }
            }),
        ));
        events
    }
}

use super::converter::get_context_window_size;

/// 流处理上下文
pub struct StreamContext {
    /// SSE 状态管理器
    pub state_manager: SseStateManager,
    /// 请求的模型名称
    pub model: String,
    /// 消息 ID
    pub message_id: String,
    /// 输入 tokens（估算值）
    pub input_tokens: i32,
    /// 从 contextUsageEvent 计算的实际输入 tokens
    pub context_input_tokens: Option<i32>,
    /// 上游 metadataEvent.tokenUsage 的精确最终快照。
    pub provider_token_usage: Option<TokenUsage>,
    /// 输出 tokens 累计
    pub output_tokens: i32,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 工具名称反向映射（短名称 → 原始名称），用于响应时还原
    pub tool_name_map: HashMap<String, String>,
    /// 本次请求声明的所有工具名（原始 client 名）。`<invoke>` 文本容错的灾难兜底：
    /// 只有合成名在此集合里才允许捞回成结构化 tool_use，否则当文本吐出。
    /// 为空（请求未带 tools）时不捞回任何 invoke——宁可漏捞，不可误执行。
    pub known_tool_names: std::collections::HashSet<String>,
    /// H2：允许从 `<invoke>` 文本合成工具调用的工具及其 input_schema（Kiro 侧名）。
    /// 为空时不合成。由 handler 按发给上游的工具定义填充。
    pub invoke_tool_schemas: InvokeToolSchemas,
    /// thinking 是否启用
    pub thinking_enabled: bool,
    /// thinking 内容缓冲区
    pub thinking_buffer: String,
    /// 明文里 `<invoke>` 工具调用的识别器（H2）。
    invoke_sniffer: InvokeSniffer,
    /// 是否在 thinking 块内
    pub in_thinking_block: bool,
    /// thinking 块是否已提取完成
    pub thinking_extracted: bool,
    /// thinking 块索引
    pub thinking_block_index: Option<i32>,
    /// 上游原生 reasoningContentEvent 下发的 thinking 签名
    pending_thinking_signature: Option<String>,
    /// 文本块索引（thinking 启用时动态分配）
    pub text_block_index: Option<i32>,
    /// 是否需要剥离 thinking 内容开头的换行符
    /// 模型输出 `<thinking>\n` 时，`\n` 可能与标签在同一 chunk 或下一 chunk
    strip_thinking_leading_newline: bool,
    /// 中转层 CacheMeter 的缓存覆盖情况（estimate 口径）。最终上报时按真实 total
    /// 做互斥分摊：`input + cache_creation + cache_read == total`，避免把被缓存
    /// 覆盖的前缀重复计进 input_tokens。
    pub cache_usage: super::cache_metering::CacheUsage,
    /// meteringEvent 上报的 credit 计费量（上游真实下发，多次事件累加得到本次总量）
    pub credits: f64,
    /// 最近一次 meteringEvent 完整 payload（含 unit / unit_plural / usage）。
    /// 透传到 message_delta.usage 的 `credit_usage` / `credit_unit` / `credit_unit_plural`
    /// 字段，与 kiro-rs /v1/messages 行为对齐；上游只下发一次则取该次。
    pub metering: Option<MeteringEvent>,
    /// 复读熔断：最近一次作为文本吐出的「尾行」内容（去空白）。
    /// Opus 长上下文退化时会把同一个 stray token（call/count/card）一行一行无限复读，
    /// 我们在文本出口处统计「同一短行连续重复了多少次」。
    repeat_guard_last_line: String,
    /// 复读熔断：当前尾行已连续重复的次数。
    repeat_guard_run: u32,
    /// 复读熔断：是否已经触发过熔断（触发后本轮后续文本一律丢弃，不再吐、不写历史）。
    repeat_guard_tripped: bool,
    /// 工具调用参数 JSON 累积器：按 tool_use_id 缓冲分片，`stop` 时整体解析，
    /// 避免把“流式半截 JSON”当成完整工具调用转发。
    tool_json_accumulator: ToolJsonAccumulator,
    /// 工具调用 JSON 错误（非法 / 半截）。一旦置位，收尾时补发 `error` 事件，
    /// 上层据此把本次请求记为 error 而非 success。
    tool_json_error: Option<ToolJsonAccumulatorError>,
    /// 跨 chunk 过滤混入 assistant 文本的字面 `<tool_use>` XML 泄漏。
    tool_use_xml_filter: ToolUseXmlLeakFilter,
    /// metadataEvent.stopReason 的原始值（上游原生终止原因，M2 / M5）。
    native_stop_reason: Option<String>,
    /// contextUsageEvent 报告上下文用量达到 100%。
    context_window_full: bool,
    /// thinking 未启用时收到的原生 reasoning；只在整轮没有正文和工具调用时作为正文兜底（L9）。
    undisclosed_reasoning: String,
    /// 是否已经发出过非空白正文（L9 判定用）。
    visible_text_emitted: bool,
}

impl StreamContext {
    /// 解析 Anthropic 口径的 `(uncached_input, cache_write, cache_read)`。
    ///
    /// 精确 `metadataEvent.tokenUsage` 优先；只有上游未提供该事件时，才按
    /// contextUsage/请求估算总量与本地 CacheMeter 比例回退。
    pub fn resolved_usage(&self) -> (i32, i32, i32) {
        if let Some(usage) = self.provider_token_usage {
            let usage = usage.sanitized();
            return (
                usage.uncached_input_tokens,
                usage.cache_write_input_tokens,
                usage.cache_read_input_tokens,
            );
        }

        // contextUsage 覆盖的是生成结束后的整个上下文（含本轮输出），
        // 先扣掉本轮输出再做 cache 分摊，避免输出被计两次。
        let total_real = crate::token::input_tokens_excluding_output(
            self.context_input_tokens,
            self.input_tokens,
            self.output_tokens,
        );
        self.cache_usage.split_against_total(total_real)
    }

    /// 精确 provider 输出 token 优先，否则返回流内容的本地估算。
    pub fn resolved_output_tokens(&self) -> i32 {
        self.provider_token_usage
            .map(|usage| usage.sanitized().output_tokens)
            .unwrap_or(self.output_tokens)
            .max(0)
    }

    /// 工具调用 JSON 错误信息（非法 / 半截）。上层据此把本次请求记为 error、
    /// 或在非流式路径返回 502。无错误时返回 `None`。
    pub fn tool_json_error_message(&self) -> Option<String> {
        self.tool_json_error.as_ref().map(|err| err.message())
    }

    /// 创建 StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
        known_tool_names: std::collections::HashSet<String>,
    ) -> Self {
        Self {
            state_manager: SseStateManager::new(),
            model: model.into(),
            message_id: format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
            input_tokens,
            context_input_tokens: None,
            provider_token_usage: None,
            output_tokens: 0,
            tool_block_indices: HashMap::new(),
            tool_name_map,
            known_tool_names,
            invoke_tool_schemas: InvokeToolSchemas::new(),
            thinking_enabled,
            thinking_buffer: String::new(),
            invoke_sniffer: InvokeSniffer::default(),
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            pending_thinking_signature: None,
            text_block_index: None,
            strip_thinking_leading_newline: false,
            cache_usage: super::cache_metering::CacheUsage::default(),
            credits: 0.0,
            metering: None,
            repeat_guard_last_line: String::new(),
            repeat_guard_run: 0,
            repeat_guard_tripped: false,
            tool_json_accumulator: ToolJsonAccumulator::new(),
            tool_json_error: None,
            tool_use_xml_filter: ToolUseXmlLeakFilter::default(),
            native_stop_reason: None,
            context_window_full: false,
            undisclosed_reasoning: String::new(),
            visible_text_emitted: false,
        }
    }

    /// 设置允许从 `<invoke>` 文本合成工具调用的工具 schema（H2），只保留本次请求声明的工具。
    pub fn set_invoke_tool_schemas(&mut self, schemas: InvokeToolSchemas) {
        let known = &self.known_tool_names;
        self.invoke_tool_schemas = schemas
            .into_iter()
            .filter(|(name, _)| known.contains(name))
            .collect();
    }

    /// 测试辅助：链式设置 [`Self::set_invoke_tool_schemas`]。
    #[cfg(test)]
    pub fn with_invoke_tool_schemas(mut self, schemas: InvokeToolSchemas) -> Self {
        self.set_invoke_tool_schemas(schemas);
        self
    }

    /// 按上游原生 metadataEvent.stopReason 决定终止原因（M2 / L2 / M5）。
    ///
    /// - `MAX_TOKENS` → `max_tokens`；`MODEL_CONTEXT_WINDOW_EXCEEDED` → `model_context_window_exceeded`；
    /// - 上游明确给出其他原因（如 END_TURN / TOOL_USE）时，不再用 100% contextUsage 覆盖；
    /// - 没有原生信号时，contextUsage 100% 仍回退为 `model_context_window_exceeded`（原行为）。
    ///
    /// 拒答等已设置的终态由 `set_stop_reason` 自身保护。
    fn apply_native_stop_reason(&mut self) {
        let native = self.native_stop_reason.as_deref().map(str::to_ascii_uppercase);
        match native.as_deref() {
            Some("MAX_TOKENS") => self.state_manager.set_stop_reason("max_tokens"),
            Some("MODEL_CONTEXT_WINDOW_EXCEEDED") => self
                .state_manager
                .set_stop_reason("model_context_window_exceeded"),
            Some(_) => {}
            None if self.context_window_full => self
                .state_manager
                .set_stop_reason("model_context_window_exceeded"),
            None => {}
        }
    }

    /// 生成 message_start 事件
    pub fn create_message_start_event(&self) -> serde_json::Value {
        json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": 1,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        })
    }

    /// 生成初始事件序列 (message_start + 文本块 start)
    ///
    /// 当 thinking 启用时，不在初始化时创建文本块，而是等到实际收到内容时再创建。
    /// 这样可以确保 thinking 块（索引 0）在文本块（索引 1）之前。
    pub fn generate_initial_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // message_start
        let msg_start = self.create_message_start_event();
        if let Some(event) = self.state_manager.handle_message_start(msg_start) {
            events.push(event);
        }

        // 如果启用了 thinking，不在这里创建文本块
        // thinking 块和文本块会在 process_content_with_thinking 中按正确顺序创建
        if self.thinking_enabled {
            return events;
        }

        // 创建初始文本块（仅在未启用 thinking 时）
        let text_block_index = self.state_manager.next_block_index();
        self.text_block_index = Some(text_block_index);
        let text_block_events = self.state_manager.handle_content_block_start(
            text_block_index,
            "text",
            json!({
                "type": "content_block_start",
                "index": text_block_index,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            }),
        );
        events.extend(text_block_events);

        events
    }

    /// 处理 Kiro 事件并转换为 Anthropic SSE 事件
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        match event {
            Event::AssistantResponse(resp) => self.process_assistant_response(&resp.content),
            Event::ToolUse(tool_use) => self.process_tool_use(tool_use),
            Event::ReasoningContent(reasoning) => self.process_reasoning_content(reasoning),
            Event::Metadata(metadata) => {
                if let Some(usage) = metadata.token_usage {
                    let usage = usage.sanitized();
                    tracing::debug!(
                        uncached_input_tokens = usage.uncached_input_tokens,
                        cache_write_input_tokens = usage.cache_write_input_tokens,
                        cache_read_input_tokens = usage.cache_read_input_tokens,
                        output_tokens = usage.output_tokens,
                        "收到 metadataEvent.tokenUsage 精确用量"
                    );
                    // tokenUsage 是最终快照；同一 provider 流内重复出现时取最后一份，
                    // 不能累加，否则会重复计费。
                    self.provider_token_usage = Some(usage);
                }
                if let Some(reason) = metadata.stop_reason.as_deref() {
                    self.native_stop_reason = Some(reason.to_string());
                }
                if let Some(refusal) = metadata.content_filter_refusal() {
                    tracing::warn!("上游内容过滤拒答: {}", refusal);
                    self.state_manager.set_stop_reason(REFUSAL_STOP_REASON);
                }
                Vec::new()
            }
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比计算实际的 input_tokens
                let window_size = get_context_window_size(&self.model);
                let actual_input_tokens =
                    (context_usage.context_usage_percentage * (window_size as f64) / 100.0) as i32;
                self.context_input_tokens = Some(actual_input_tokens);
                // 上下文用量达到 100%：先记下，收尾时上游没有给出原生 stopReason 才报
                // model_context_window_exceeded（M5）。
                if context_usage.context_usage_percentage >= 100.0 {
                    self.context_window_full = true;
                }
                tracing::debug!(
                    "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                    context_usage.context_usage_percentage,
                    actual_input_tokens
                );
                Vec::new()
            }
            Event::Metering(metering) => {
                // 上游 meteringEvent 只下发 credit；token / cache 字段不存在。
                self.credits += metering.usage;
                tracing::debug!(
                    usage = metering.usage,
                    unit = %metering.unit,
                    unit_plural = %metering.unit_plural,
                    "metering credits +{:.6}", metering.usage
                );
                // 保留最近一次完整 payload，用于在 message_delta 里透传 credit_*
                // 字段；如果上游真的多次下发，则以最后一次为准（与 kiro-rs 一致）。
                self.metering = Some(metering.clone());
                Vec::new()
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                tracing::error!("收到错误事件: {} - {}", error_code, error_message);
                Vec::new()
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                // 处理 ContentLengthExceededException
                if exception_type == "ContentLengthExceededException" {
                    self.state_manager.set_stop_reason("max_tokens");
                }
                tracing::warn!("收到异常事件: {} - {}", exception_type, message);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// 处理助手响应事件
    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        // 先过滤字面 <tool_use> XML 泄漏（跨 chunk）。过滤后为空可能是过滤器在缓冲
        // 半个标签，直接返回；后续 token 估算与文本处理都用过滤后内容
        // （被剥离的 XML 因此不计入 output_tokens）。
        let content = self.tool_use_xml_filter.filter(content);
        if content.is_empty() {
            return Vec::new();
        }
        let content = content.as_str();

        let mut events = Vec::new();
        if self.is_thinking_block_open() && !self.in_thinking_block {
            events.extend(self.close_open_thinking_block());
        }

        // 估算 tokens
        self.output_tokens += estimate_tokens(content);

        // 如果启用了thinking，需要处理thinking块
        if self.thinking_enabled {
            events.extend(self.process_content_with_thinking(content));
            return events;
        }

        // 非 thinking 模式同样复用统一的 text_delta 发送逻辑，
        // 以便在 tool_use 自动关闭文本块后能够自愈重建新的文本块，避免“吞字”。
        events.extend(self.create_text_delta_events(content));
        events
    }

    /// 处理包含thinking块的内容
    fn process_content_with_thinking(&mut self, content: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 将内容添加到缓冲区进行处理
        self.thinking_buffer.push_str(content);

        loop {
            if !self.in_thinking_block && !self.thinking_extracted {
                // M1：只有回复开头（去掉前导空白后）的 <thinking> 才是思考块。
                // 开头已经是正文时，后面出现的字面 <thinking> 一律按正文输出。
                let head = self.thinking_buffer.trim_start();
                if !head.is_empty()
                    && !head.starts_with("<thinking>")
                    && !"<thinking>".starts_with(head)
                {
                    self.thinking_extracted = true;
                    continue;
                }
                // 查找 <thinking> 开始标签（跳过被反引号包裹的）
                if let Some(start_pos) = find_real_thinking_start_tag(&self.thinking_buffer)
                    .filter(|&pos| self.thinking_buffer[..pos].trim().is_empty())
                {
                    // 发送 <thinking> 之前的内容作为 text_delta
                    // 注意：如果前面只是空白字符（如 adaptive 模式返回的 \n\n），则跳过，
                    // 避免在 thinking 块之前产生无意义的 text 块导致客户端解析失败
                    let before_thinking = self.thinking_buffer[..start_pos].to_string();
                    if !before_thinking.is_empty() && !before_thinking.trim().is_empty() {
                        events.extend(self.create_text_delta_events(&before_thinking));
                    }

                    // 进入 thinking 块
                    self.in_thinking_block = true;
                    self.strip_thinking_leading_newline = true;
                    self.thinking_buffer =
                        self.thinking_buffer[start_pos + "<thinking>".len()..].to_string();

                    // 创建 thinking 块的 content_block_start 事件
                    let thinking_index = self.state_manager.next_block_index();
                    self.thinking_block_index = Some(thinking_index);
                    let start_events = self.state_manager.handle_content_block_start(
                        thinking_index,
                        "thinking",
                        json!({
                            "type": "content_block_start",
                            "index": thinking_index,
                            "content_block": {
                                "type": "thinking",
                                "thinking": ""
                            }
                        }),
                    );
                    events.extend(start_events);
                } else {
                    // 没有找到 <thinking>，检查是否可能是部分标签
                    // 保留可能是部分标签的内容
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("<thinking>".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        // 如果 thinking 尚未提取，且安全内容只是空白字符，
                        // 则不发送为 text_delta，继续保留在缓冲区等待更多内容。
                        // 这避免了 4.6 模型中 <thinking> 标签跨事件分割时，
                        // 前导空白（如 "\n\n"）被错误地创建为 text 块，
                        // 导致 text 块先于 thinking 块出现的问题。
                        if !safe_content.is_empty() && !safe_content.trim().is_empty() {
                            events.extend(self.create_text_delta_events(&safe_content));
                            self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                        }
                    }
                    break;
                }
            } else if self.in_thinking_block {
                // 剥离 <thinking> 标签后紧跟的换行符（可能跨 chunk）
                if self.strip_thinking_leading_newline {
                    if self.thinking_buffer.starts_with('\n') {
                        self.thinking_buffer = self.thinking_buffer[1..].to_string();
                        self.strip_thinking_leading_newline = false;
                    } else if !self.thinking_buffer.is_empty() {
                        // buffer 非空但不以 \n 开头，不再需要剥离
                        self.strip_thinking_leading_newline = false;
                    }
                    // buffer 为空时保留标志，等待下一个 chunk
                }

                // 在 thinking 块内，查找 </thinking> 结束标签（跳过被反引号包裹的）
                if let Some(end_pos) = find_real_thinking_end_tag(&self.thinking_buffer) {
                    // 提取 thinking 内容
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 结束 thinking 块
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;

                    // 发送空的 thinking_delta 事件，然后发送 content_block_stop 事件
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // signature_delta：满足客户端 thinking 模式下的本地校验
                        events.push(self.create_signature_delta_event(thinking_index));
                        // 再发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 剥离 `</thinking>\n\n`（find_real_thinking_end_tag 已确认 \n\n 存在）
                    self.thinking_buffer =
                        self.thinking_buffer[end_pos + thinking_end_tag_strip_len(&self.thinking_buffer, end_pos)..]
                            .to_string();
                } else {
                    // 没有找到结束标签，发送当前缓冲区内容作为 thinking_delta。
                    // 保留末尾可能是部分 `</thinking>\n\n` 的内容：
                    // find_real_thinking_end_tag 要求标签后有 `\n\n` 才返回 Some，
                    // 因此保留区必须覆盖 `</thinking>\n\n` 的完整长度（13 字节），
                    // 否则当 `</thinking>` 已在 buffer 但 `\n\n` 尚未到达时，
                    // 标签的前几个字符会被错误地作为 thinking_delta 发出。
                    let target_len = self
                        .thinking_buffer
                        .len()
                        .saturating_sub("</thinking>\n\n".len());
                    let safe_len = find_char_boundary(&self.thinking_buffer, target_len);
                    if safe_len > 0 {
                        let safe_content = self.thinking_buffer[..safe_len].to_string();
                        if !safe_content.is_empty() {
                            if let Some(thinking_index) = self.thinking_block_index {
                                events.push(
                                    self.create_thinking_delta_event(thinking_index, &safe_content),
                                );
                            }
                        }
                        self.thinking_buffer = self.thinking_buffer[safe_len..].to_string();
                    }
                    break;
                }
            } else {
                // thinking 已提取完成，剩余内容作为 text_delta
                if !self.thinking_buffer.is_empty() {
                    let remaining = self.thinking_buffer.clone();
                    self.thinking_buffer.clear();
                    events.extend(self.create_text_delta_events(&remaining));
                }
                break;
            }
        }

        events
    }

    /// 创建 text_delta 事件：thinking / 非 thinking 两条路径和两个端点共用的明文出口。
    /// 文本先过 [`InvokeSniffer`]（H2）：可能是末尾工具调用的 `<invoke>` 块暂时扣住，其余立即输出。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        if text.is_empty() {
            return Vec::new();
        }
        let outputs = self.invoke_sniffer.push(text, &self.invoke_tool_schemas);
        self.emit_sniffed(outputs)
    }

    /// 按原顺序发出识别器的输出：正文走 `emit_text_delta_raw`，合成的调用走统一的 tool_use 出口。
    fn emit_sniffed(&mut self, outputs: Vec<SniffOutput>) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for output in outputs {
            match output {
                SniffOutput::Text(text) => events.extend(self.emit_text_delta_raw(&text)),
                SniffOutput::ToolCall { name, input } => {
                    let id = format!("toolu_{}", Uuid::new_v4().to_string().replace('-', ""));
                    let completed =
                        CompletedToolUse::from_kiro(id, &name, input, &self.tool_name_map);
                    events.extend(self.emit_completed_tool_use(completed));
                }
            }
        }
        events
    }

    /// 创建 text_delta 事件（原始逻辑，无嗅探）
    ///
    /// 如果文本块尚未创建，会先创建文本块。
    /// 当发生 tool_use 时，状态机会自动关闭当前文本块；后续文本会自动创建新的文本块继续输出。
    ///
    /// 返回值包含可能的 content_block_start 事件和 content_block_delta 事件。
    /// 复读熔断过滤器：在文本真正吐给客户端之前，逐行检测「同一 stray token 连续复读」。
    ///
    /// 工作方式（流式安全，跨 chunk 累计）：
    /// - 把进来的 `text` 按行切，逐行和上一行（去空白）比较；
    /// - 只对 `STRAY_INVOKE_TOKENS`（call/count/card）这类退化引导词计数，普通文本一律放行；
    /// - 同一 stray token 连续重复达到 `REPEAT_GUARD_TRIP_THRESHOLD` 即「跳闸」；
    /// - 跳闸后：本轮内后续任何文本（含继续复读的 count）一律丢弃，返回空串。
    ///
    /// 返回应当继续吐出的文本（跳闸时返回空串）。
    fn repeat_guard_filter(&mut self, text: &str) -> String {
        // 已跳闸：本轮剩余文本全部丢弃，断雪球。
        if self.repeat_guard_tripped {
            return String::new();
        }

        let mut kept = String::new();
        // 用 split_inclusive 保留换行符，确保放行的正常文本不丢字节。
        for segment in text.split_inclusive('\n') {
            let line = segment.trim();
            if STRAY_INVOKE_TOKENS.contains(&line) {
                if line == self.repeat_guard_last_line {
                    self.repeat_guard_run += 1;
                } else {
                    self.repeat_guard_last_line = line.to_string();
                    self.repeat_guard_run = 1;
                }
                if self.repeat_guard_run >= REPEAT_GUARD_TRIP_THRESHOLD {
                    // 跳闸：丢弃这一行及本轮后续所有文本。已经放行的 kept 保留
                    // （阈值内的少量重复无害），但不再追加，并标记 tripped。
                    self.repeat_guard_tripped = true;
                    return kept;
                }
                // 阈值内：照常放行（少量引导词重复是正常的）。
                kept.push_str(segment);
            } else {
                // 普通文本行（含空行）：重置复读计数，正常放行。
                if !line.is_empty() {
                    self.repeat_guard_last_line = line.to_string();
                    self.repeat_guard_run = 0;
                }
                kept.push_str(segment);
            }
        }
        kept
    }

    fn emit_text_delta_raw(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 🛑 复读熔断（root cause: Opus 长上下文退化，把同一 stray token 一行行无限复读）。
        // 在文本出口处过滤：一旦同一短行连续重复超过阈值，丢弃后续复读文本，
        // 既不让它喷给客户端、不烧满 max_tokens，也不写进对话历史（断雪球）。
        let kept = self.repeat_guard_filter(text);
        if kept.is_empty() {
            return events;
        }
        let text: &str = &kept;

        if !text.trim().is_empty() {
            self.visible_text_emitted = true;
        }

        // 如果当前 text_block_index 指向的块已经被关闭（例如 tool_use 开始时自动 stop），
        // 则丢弃该索引并创建新的文本块继续输出，避免 delta 被状态机拒绝导致“吞字”。
        if let Some(idx) = self.text_block_index {
            if !self.state_manager.is_block_open_of_type(idx, "text") {
                self.text_block_index = None;
            }
        }

        // 获取或创建文本块索引
        let text_index = if let Some(idx) = self.text_block_index {
            idx
        } else {
            // 文本块尚未创建，需要先创建
            let idx = self.state_manager.next_block_index();
            self.text_block_index = Some(idx);

            // 发送 content_block_start 事件
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "text",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // 发送 content_block_delta 事件
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            text_index,
            json!({
                "type": "content_block_delta",
                "index": text_index,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ) {
            events.push(delta_event);
        }

        events
    }

    fn is_thinking_block_open(&self) -> bool {
        self.thinking_block_index
            .is_some_and(|idx| self.state_manager.is_block_open_of_type(idx, "thinking"))
    }

    fn close_open_text_block(&mut self) -> Vec<SseEvent> {
        let Some(idx) = self.text_block_index else {
            return Vec::new();
        };
        if !self.state_manager.is_block_open_of_type(idx, "text") {
            self.text_block_index = None;
            return Vec::new();
        }
        self.text_block_index = None;
        self.state_manager
            .handle_content_block_stop(idx)
            .into_iter()
            .collect()
    }

    fn ensure_thinking_block(&mut self) -> Vec<SseEvent> {
        if self.is_thinking_block_open() {
            return Vec::new();
        }

        let mut events = Vec::new();
        let buffered = std::mem::take(&mut self.thinking_buffer);
        if !buffered.trim().is_empty() {
            events.extend(self.create_text_delta_events(&buffered));
        }
        events.extend(self.close_open_text_block());

        let idx = self.state_manager.next_block_index();
        self.thinking_block_index = Some(idx);
        self.thinking_extracted = true;
        events.extend(self.state_manager.handle_content_block_start(
            idx,
            "thinking",
            json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "thinking",
                    "thinking": ""
                }
            }),
        ));
        events
    }

    fn close_open_thinking_block(&mut self) -> Vec<SseEvent> {
        let Some(idx) = self.thinking_block_index else {
            return Vec::new();
        };
        if !self.state_manager.is_block_open_of_type(idx, "thinking") {
            return Vec::new();
        }

        let signature = self
            .pending_thinking_signature
            .take()
            .unwrap_or_else(|| THINKING_SIGNATURE_PLACEHOLDER.to_string());
        let mut events = vec![
            self.create_thinking_delta_event(idx, ""),
            self.create_signature_delta_event_with(idx, &signature),
        ];
        if let Some(stop_event) = self.state_manager.handle_content_block_stop(idx) {
            events.push(stop_event);
        }
        events
    }

    fn process_reasoning_content(
        &mut self,
        reasoning: &crate::kiro::model::events::ReasoningContentEvent,
    ) -> Vec<SseEvent> {
        if !self.thinking_enabled {
            if let Some(text) = reasoning.text.as_deref()
                && !text.is_empty()
            {
                self.output_tokens += estimate_tokens(text);
                // L9：thinking 未启用时 reasoning 不混入正文，只在整轮没有正文和工具调用时兜底。
                self.undisclosed_reasoning.push_str(text);
                return Vec::new();
            }
            return Vec::new();
        }

        let mut events = Vec::new();

        if let Some(signature) = reasoning.signature.as_deref()
            && !signature.is_empty()
        {
            self.pending_thinking_signature = Some(signature.to_string());
        }

        if let Some(text) = reasoning.text.as_deref()
            && !text.is_empty()
        {
            self.output_tokens += estimate_tokens(text);
            events.extend(self.ensure_thinking_block());
            if let Some(idx) = self.thinking_block_index {
                events.push(self.create_thinking_delta_event(idx, text));
            }
        }

        if let Some(redacted) = reasoning.redacted_content.as_deref()
            && !redacted.is_empty()
        {
            self.output_tokens += 8;
            events.extend(self.create_redacted_thinking_events(redacted));
        }

        events
    }

    fn create_redacted_thinking_events(&mut self, data: &str) -> Vec<SseEvent> {
        let mut events = self.close_open_thinking_block();
        events.extend(self.close_open_text_block());

        let idx = self.state_manager.next_block_index();
        events.extend(self.state_manager.handle_content_block_start(
            idx,
            "redacted_thinking",
            json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "redacted_thinking",
                    "data": data
                }
            }),
        ));
        if let Some(stop_event) = self.state_manager.handle_content_block_stop(idx) {
            events.push(stop_event);
        }
        events
    }

    /// 创建 thinking_delta 事件
    fn create_thinking_delta_event(&self, index: i32, thinking: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": thinking
                }
            }),
        )
    }

    /// 创建 signature_delta 事件
    ///
    /// Anthropic 协议下 thinking 块流式结束前必须发一个 signature_delta，
    /// SDK 会把它聚合到 thinking 块的 `signature` 字段。客户端在下一轮把
    /// assistant 消息回传时本地校验 thinking 块必须带非空 signature，否则抛出
    /// `The content[].thinking in the thinking mode must be passed back to the API`。
    ///
    /// 上游 Kiro 不是 Anthropic 服务端，不会下发真实签名，因此这里发一个非空
    /// 占位字符串以满足客户端本地校验。该字段不参与转发回 Kiro 的逻辑
    /// （converter 只读 `block.thinking`，不读 signature）。
    fn create_signature_delta_event(&self, index: i32) -> SseEvent {
        self.create_signature_delta_event_with(index, THINKING_SIGNATURE_PLACEHOLDER)
    }

    fn create_signature_delta_event_with(&self, index: i32, signature: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": signature,
                }
            }),
        )
    }

    /// 统一的工具调用流式发出口：结构化 `toolUseEvent` 与 `<invoke>` 文本捞回都经此发出。
    ///
    /// 块索引按 `completed.id` 复用/分配（结构化按 tool_use_id 复用；invoke 合成用新 id 故新分配），
    /// 依次发 `content_block_start{name, input:{}}` → 单个完整 `input_json_delta` → `content_block_stop`。
    fn emit_completed_tool_use(&mut self, completed: CompletedToolUse) -> Vec<SseEvent> {
        let mut events = Vec::new();
        self.state_manager.set_has_tool_use(true);

        let block_index = if let Some(&idx) = self.tool_block_indices.get(&completed.id) {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.tool_block_indices.insert(completed.id.clone(), idx);
            idx
        };

        events.extend(self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": completed.id,
                    "name": completed.name,
                    "input": {}
                }
            }),
        ));

        // 一次性发出完整参数 JSON（来源已保证是合法 JSON）。
        self.output_tokens += estimate_tokens(&completed.input.to_string());
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            block_index,
            json!({
                "type": "content_block_delta",
                "index": block_index,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": serde_json::to_string(&completed.input).unwrap_or_else(|_| "{}".to_string())
                }
            }),
        ) {
            events.push(delta_event);
        }

        if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
            events.push(stop_event);
        }

        events
    }

    /// 处理工具使用事件
    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        self.state_manager.set_has_tool_use(true);

        if self.is_thinking_block_open() && !self.in_thinking_block {
            events.extend(self.close_open_thinking_block());
        }

        // H2：本轮有原生 tool_use。识别器扣住的文本按原顺序放出，之后的 `<invoke>` 文本不再合成。
        let held = self.invoke_sniffer.abandon();
        events.extend(self.emit_sniffed(held));

        // tool_use 必须发生在 thinking 结束之后。
        // 但当 `</thinking>` 后面没有 `\n\n`（例如紧跟 tool_use 或流结束）时，
        // thinking 结束标签会滞留在 thinking_buffer，导致后续 flush 时把 `</thinking>` 当作内容输出。
        // 这里在开始 tool_use block 前做一次“边界场景”的结束标签识别与过滤。
        if self.thinking_enabled && self.in_thinking_block {
            if let Some(end_pos) = find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer) {
                let thinking_content = self.thinking_buffer[..end_pos].to_string();
                if !thinking_content.is_empty() {
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(
                            self.create_thinking_delta_event(thinking_index, &thinking_content),
                        );
                    }
                }

                // 结束 thinking 块
                self.in_thinking_block = false;
                self.thinking_extracted = true;

                if let Some(thinking_index) = self.thinking_block_index {
                    // 先发送空的 thinking_delta
                    events.push(self.create_thinking_delta_event(thinking_index, ""));
                    // signature_delta：满足客户端 thinking 模式下的本地校验
                    events.push(self.create_signature_delta_event(thinking_index));
                    // 再发送 content_block_stop
                    if let Some(stop_event) =
                        self.state_manager.handle_content_block_stop(thinking_index)
                    {
                        events.push(stop_event);
                    }
                }

                // 把结束标签后的内容当作普通文本（通常为空或空白）
                let after_pos = end_pos + "</thinking>".len();
                let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                self.thinking_buffer.clear();
                if !remaining.is_empty() {
                    events.extend(self.create_text_delta_events(&remaining));
                }
            }
        }

        // thinking 模式下，process_content_with_thinking 可能会为了探测 `<thinking>` 而暂存一小段尾部文本。
        // 如果此时直接开始 tool_use，状态机会自动关闭 text block，导致这段"待输出文本"看起来被 tool_use 吞掉。
        // 约束：只在尚未进入 thinking block、且 thinking 尚未被提取时，将缓冲区当作普通文本 flush。
        if self.thinking_enabled
            && !self.in_thinking_block
            && !self.thinking_extracted
            && !self.thinking_buffer.is_empty()
        {
            let buffered = std::mem::take(&mut self.thinking_buffer);
            events.extend(self.create_text_delta_events(&buffered));
        }

        // 通过累积器缓冲工具参数 JSON 分片：只有收到 stop=true 且解析成功时才
        // 发出完整的工具调用；半截 / 非法 JSON 记为错误，交由收尾（generate_final_events）
        // 统一补发 error 事件，避免把无法解析的参数当成完整调用转发给客户端。
        let completed = match self
            .tool_json_accumulator
            .push(tool_use, &self.tool_name_map)
        {
            Ok(Some(completed)) => completed,
            Ok(None) => return events,
            Err(e) => {
                tracing::error!("{}", e);
                self.tool_json_error = Some(e);
                self.state_manager.set_stop_reason("error");
                return events;
            }
        };

        // 统一发出（与 <invoke> 文本捞回路径共用同一发出口）。
        events.extend(self.emit_completed_tool_use(completed));
        events
    }

    /// 生成最终事件序列
    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 收尾：flush <tool_use> XML 过滤器的残留（截断的未闭合块会被丢弃），
        // 走同一文本路径，交由后续 thinking / invoke 缓冲一并 flush。
        let leftover = self.tool_use_xml_filter.finish();
        if !leftover.is_empty() {
            if self.thinking_enabled {
                events.extend(self.process_content_with_thinking(&leftover));
            } else {
                events.extend(self.create_text_delta_events(&leftover));
            }
        }

        if self.is_thinking_block_open() && !self.in_thinking_block {
            events.extend(self.close_open_thinking_block());
        }

        // Flush thinking_buffer 中的剩余内容
        if self.thinking_enabled && !self.thinking_buffer.is_empty() {
            if self.in_thinking_block {
                // 末尾可能残留 `</thinking>`（例如紧跟 tool_use 或流结束），需要在 flush 时过滤掉结束标签。
                if let Some(end_pos) =
                    find_real_thinking_end_tag_at_buffer_end(&self.thinking_buffer)
                {
                    let thinking_content = self.thinking_buffer[..end_pos].to_string();
                    if !thinking_content.is_empty() {
                        if let Some(thinking_index) = self.thinking_block_index {
                            events.push(
                                self.create_thinking_delta_event(thinking_index, &thinking_content),
                            );
                        }
                    }

                    // 关闭 thinking 块：先发送空的 thinking_delta，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // signature_delta：满足客户端 thinking 模式下的本地校验
                        events.push(self.create_signature_delta_event(thinking_index));
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }

                    // 把结束标签后的内容当作普通文本（通常为空或空白）
                    let after_pos = end_pos + "</thinking>".len();
                    let remaining = self.thinking_buffer[after_pos..].trim_start().to_string();
                    self.thinking_buffer.clear();
                    self.in_thinking_block = false;
                    self.thinking_extracted = true;
                    if !remaining.is_empty() {
                        events.extend(self.create_text_delta_events(&remaining));
                    }
                } else {
                    // 如果还在 thinking 块内，发送剩余内容作为 thinking_delta
                    if let Some(thinking_index) = self.thinking_block_index {
                        events.push(
                            self.create_thinking_delta_event(thinking_index, &self.thinking_buffer),
                        );
                    }
                    // 关闭 thinking 块：先发送空的 thinking_delta，再发送 content_block_stop
                    if let Some(thinking_index) = self.thinking_block_index {
                        // 先发送空的 thinking_delta
                        events.push(self.create_thinking_delta_event(thinking_index, ""));
                        // signature_delta：满足客户端 thinking 模式下的本地校验
                        events.push(self.create_signature_delta_event(thinking_index));
                        // 再发送 content_block_stop
                        if let Some(stop_event) =
                            self.state_manager.handle_content_block_stop(thinking_index)
                        {
                            events.push(stop_event);
                        }
                    }
                }
            } else {
                // 否则发送剩余内容作为 text_delta
                let buffer_content = self.thinking_buffer.clone();
                events.extend(self.create_text_delta_events(&buffer_content));
            }
            self.thinking_buffer.clear();
        }

        // H2：流结束。回复末尾仍成立的 `<invoke>` 块（且本轮没有原生 tool_use）合成为工具调用，
        // 其余作为正文放出。
        let sniffed = self.invoke_sniffer.finish(&self.invoke_tool_schemas);
        events.extend(self.emit_sniffed(sniffed));

        // 收尾检查工具调用累积器：无参工具（0 字节滞留）在此还原为 `{}` 并正常发出；
        // 累积了非空内容却从未 stop=true 的才是上游中途截断，记为错误。
        // process_tool_use 中已置位的错误保持不变。
        if self.state_manager.get_stop_reason() == REFUSAL_STOP_REASON {
            // Refused generation can end in the middle of a tool call. Drop
            // pending calls rather than recovering them or reporting JSON errors.
            self.tool_json_error = None;
        } else if self.tool_json_error.is_none() {
            let finished = self.tool_json_accumulator.finish(&self.tool_name_map);
            match finished {
                Ok(recovered) => {
                    for completed in recovered {
                        events.extend(self.emit_completed_tool_use(completed));
                    }
                }
                Err(e) => {
                    tracing::error!("{}", e);
                    self.tool_json_error = Some(e);
                    self.state_manager.set_stop_reason("error");
                }
            }
        }

        // 工具调用 JSON 错误必须直接以异常终态结束，不能先发送正常的
        // message_delta/message_stop，否则 Responses 等下游会把请求标记为 completed。
        if let Some(err) = &self.tool_json_error {
            let error_type = err.error_type();
            let message = err.message();
            events.extend(self.generate_error_events(error_type, &message));
            return events;
        }

        // L5：复读熔断触发过，本轮输出不完整，以可重试的 api_error 终止，不能当正常结束。
        if self.repeat_guard_tripped {
            events.extend(self.generate_error_events("api_error", REPEAT_GUARD_ERROR_MESSAGE));
            return events;
        }

        // L9：thinking 未启用时，原生 reasoning 只在整轮既没有正文也没有工具调用时作为正文兜底。
        // 兜底直接走 emit_text_delta_raw，不经过 invoke 识别。
        if !self.thinking_enabled
            && !self.visible_text_emitted
            && !self.state_manager.has_tool_use
            && !self.undisclosed_reasoning.trim().is_empty()
        {
            let reasoning = std::mem::take(&mut self.undisclosed_reasoning);
            events.extend(self.emit_text_delta_raw(&reasoning));
        }

        // M2 / L2 / M5：终止原因以上游原生 stopReason 为准。没有原生截断信号时，只有 thinking 的
        // 一轮按 end_turn 正常结束，不再伪造 max_tokens，也不补空格正文。
        self.apply_native_stop_reason();

        // 精确 metadata 真值优先；缺失时才使用 contextUsage/估算回退。
        let (final_input_tokens, cache_creation, cache_read) = self.resolved_usage();
        let final_output_tokens = self.resolved_output_tokens();

        // 生成最终事件（message_delta + message_stop）
        events.extend(self.state_manager.generate_final_events(
            final_input_tokens,
            final_output_tokens,
            cache_creation,
            cache_read,
            self.metering.as_ref(),
        ));

        events
    }

    /// 上游响应体读取失败时生成异常终态，不 flush 可能已被截断的缓冲内容。
    pub fn generate_error_events(&mut self, error_type: &str, message: &str) -> Vec<SseEvent> {
        // Anthropic requires a signature_delta before a thinking block closes,
        // including abnormal termination. Close it through the thinking-aware
        // path first; the generic state manager then closes any other blocks.
        let mut events = self.close_open_thinking_block();
        events.extend(
            self.state_manager
                .generate_error_events(error_type, message),
        );
        events
    }
}

/// 简单的 token 估算（中英文字符混合）
///
/// 公开供 cache_meter 等模块复用同一估算口径。
pub fn estimate_tokens(text: &str) -> i32 {
    let chars: Vec<char> = text.chars().collect();
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in &chars {
        if *c >= '\u{4E00}' && *c <= '\u{9FFF}' {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // 中文约 1.5 字符/token，英文约 4 字符/token
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;

    (chinese_tokens + other_tokens).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ToolJsonAccumulator: 流式半截 / 非法工具调用 JSON ----

    fn tool_evt(
        id: &str,
        name: &str,
        input: &str,
        stop: bool,
    ) -> crate::kiro::model::events::ToolUseEvent {
        crate::kiro::model::events::ToolUseEvent {
            name: name.to_string(),
            tool_use_id: id.to_string(),
            input: input.to_string(),
            stop,
        }
    }

    #[test]
    fn tool_json_accumulator_reassembles_split_fragments() {
        let mut acc = ToolJsonAccumulator::new();
        let map = HashMap::new();
        // 用非内置工具名，专注验证分片重组本身（内置名的双向映射另有专项测试）。
        // JSON 被切成三片（切在 token 中间），只有最后一片带 stop。
        assert!(
            acc.push(&tool_evt("t1", "custom_tool", "{\"pa", false), &map)
                .unwrap()
                .is_none()
        );
        assert!(
            acc.push(&tool_evt("t1", "custom_tool", "th\":\"/a", false), &map)
                .unwrap()
                .is_none()
        );
        let completed = acc
            .push(&tool_evt("t1", "custom_tool", ".txt\"}", true), &map)
            .unwrap()
            .unwrap();
        assert_eq!(completed.id, "t1");
        assert_eq!(completed.name, "custom_tool");
        assert_eq!(completed.input, serde_json::json!({"path": "/a.txt"}));
    }

    #[test]
    fn tool_json_accumulator_empty_input_is_empty_object() {
        let mut acc = ToolJsonAccumulator::new();
        let completed = acc
            .push(&tool_evt("t1", "noop", "", true), &HashMap::new())
            .unwrap()
            .unwrap();
        assert_eq!(completed.input, serde_json::json!({}));
    }

    #[test]
    fn tool_json_accumulator_invalid_json_errors() {
        let mut acc = ToolJsonAccumulator::new();
        let err = acc
            .push(
                &tool_evt("t1", "read_file", "{not json", true),
                &HashMap::new(),
            )
            .unwrap_err();
        assert_eq!(err.error_type(), "upstream_tool_json_error");
        assert!(matches!(err, ToolJsonAccumulatorError::InvalidJson { .. }));
    }

    #[test]
    fn tool_json_accumulator_incomplete_on_missing_stop() {
        let mut acc = ToolJsonAccumulator::new();
        // 只来了半截、从未 stop → finish() 报 IncompleteJson。
        assert!(
            acc.push(
                &tool_evt("t1", "read_file", "{\"path\":\"/a", false),
                &HashMap::new()
            )
            .unwrap()
            .is_none()
        );
        let err = acc.finish(&HashMap::new()).unwrap_err();
        assert!(matches!(
            err,
            ToolJsonAccumulatorError::IncompleteJson { .. }
        ));
        // 已取出残留后再 finish() 应成功。
        assert!(acc.finish(&HashMap::new()).unwrap().is_empty());
    }

    /// 无参工具：上游只发一条 `input` 为空、`stop` 缺省为 false 的事件，
    /// 之后没有任何分片。finish() 必须还原为 `{}`，而不是报 `buffered 0 bytes`。
    #[test]
    fn tool_json_accumulator_recovers_noarg_tool_on_missing_stop() {
        let mut acc = ToolJsonAccumulator::new();
        assert!(
            acc.push(
                &tool_evt("t1", "mcp__computer-use__list_apps", "", false),
                &HashMap::new()
            )
            .unwrap()
            .is_none()
        );

        let recovered = acc.finish(&HashMap::new()).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].id, "t1");
        assert_eq!(recovered[0].name, "mcp__computer-use__list_apps");
        assert_eq!(recovered[0].input, serde_json::json!({}));
    }

    /// 同时滞留无参调用和真正的半截调用时，截断优先：整条响应必须报错，
    /// 不能只捞回无参的那个就当成功。
    #[test]
    fn tool_json_accumulator_truncation_wins_over_noarg_recovery() {
        let mut acc = ToolJsonAccumulator::new();
        let _ = acc.push(&tool_evt("empty", "list_apps", "", false), &HashMap::new());
        let _ = acc.push(
            &tool_evt("half", "read_file", "{\"path\":\"/a", false),
            &HashMap::new(),
        );

        let err = acc.finish(&HashMap::new()).unwrap_err();
        match err {
            ToolJsonAccumulatorError::IncompleteJson {
                tool_use_id, bytes, ..
            } => {
                assert_eq!(tool_use_id, "half");
                assert_eq!(bytes, "{\"path\":\"/a".len());
            }
            other => panic!("expected IncompleteJson, got {other:?}"),
        }
    }

    /// 多个无参调用同时滞留时全部捞回，且顺序按 tool_use_id 稳定
    /// （HashMap 迭代序不稳定，内容块索引不能随机）。
    #[test]
    fn tool_json_accumulator_recovers_all_noarg_tools_in_stable_order() {
        let mut acc = ToolJsonAccumulator::new();
        for id in ["t3", "t1", "t2"] {
            let _ = acc.push(&tool_evt(id, "list_apps", "", false), &HashMap::new());
        }

        let recovered = acc.finish(&HashMap::new()).unwrap();
        let ids: Vec<&str> = recovered.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["t1", "t2", "t3"]);
    }

    /// 端到端：无参工具在实时 SSE 上必须走正常终态（tool_use + message_stop），
    /// 不能出现 error 事件。这是 issue #79 的回归用例。
    #[test]
    fn noarg_tool_without_stop_ends_stream_normally() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let emitted =
            ctx.process_kiro_event(&Event::ToolUse(tool_evt("t1", "list_apps", "", false)));
        assert!(emitted.is_empty(), "无参调用在收到 stop 前仍应缓冲");

        let final_events = ctx.generate_final_events();
        assert!(
            !final_events.iter().any(|e| e.event == "error"),
            "无参工具不应产生 error 事件: {:?}",
            final_events.iter().map(|e| &e.event).collect::<Vec<_>>()
        );
        assert!(ctx.tool_json_error_message().is_none());

        let start = final_events
            .iter()
            .find(|e| e.event == "content_block_start")
            .expect("应发出 tool_use 内容块");
        assert_eq!(start.data["content_block"]["type"], json!("tool_use"));
        assert_eq!(start.data["content_block"]["name"], json!("list_apps"));

        let delta = final_events
            .iter()
            .find(|e| e.event == "content_block_delta")
            .expect("应发出参数 JSON");
        assert_eq!(delta.data["delta"]["partial_json"], json!("{}"));

        let last = final_events.last().unwrap();
        assert_eq!(last.event, "message_stop");
        let stop_reason = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .map(|e| e.data["delta"]["stop_reason"].clone());
        assert_eq!(stop_reason, Some(json!("tool_use")));
    }

    #[test]
    fn incomplete_tool_json_ends_stream_with_error_only() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let emitted = ctx.process_kiro_event(&Event::ToolUse(tool_evt(
            "t1",
            "read_file",
            "{\"path\":\"/a",
            false,
        )));
        assert!(emitted.is_empty(), "partial tool JSON must remain buffered");

        let final_events = ctx.generate_final_events();
        assert_eq!(final_events.last().unwrap().event, "error");
        assert_eq!(
            final_events.last().unwrap().data["error"]["type"],
            json!("upstream_tool_json_error")
        );
        assert!(
            final_events
                .iter()
                .all(|event| event.event != "message_delta" && event.event != "message_stop")
        );
    }

    #[test]
    fn tool_json_accumulator_restores_short_tool_name() {
        let mut acc = ToolJsonAccumulator::new();
        let mut map = HashMap::new();
        map.insert(
            "short_abc123".to_string(),
            "the_original_very_long_tool_name".to_string(),
        );
        let completed = acc
            .push(&tool_evt("t1", "short_abc123", "{}", true), &map)
            .unwrap()
            .unwrap();
        assert_eq!(completed.name, "the_original_very_long_tool_name");
    }

    /// 防回归：统一管道的两个去向（流式 emit_completed_tool_use 与非流式 to_anthropic_block）
    /// 对同一 CompletedToolUse 产出一致的 id / name / input。
    #[test]
    fn emit_and_block_agree_on_shape() {
        let completed = CompletedToolUse {
            id: "toolu_1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "/a"}),
        };

        // 非流式块
        let block = completed.to_anthropic_block();
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "toolu_1");
        assert_eq!(block["name"], "read_file");
        assert_eq!(block["input"], serde_json::json!({"path": "/a"}));

        // 流式发出：start 的 id/name 与块一致；delta 的 partial_json 解析后与块 input 一致。
        let mut ctx = StreamContext::new_with_thinking(
            "m",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let events = ctx.emit_completed_tool_use(completed);
        let start = events
            .iter()
            .find(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
            })
            .expect("应有 tool_use content_block_start");
        assert_eq!(start.data["content_block"]["id"], block["id"]);
        assert_eq!(start.data["content_block"]["name"], block["name"]);
        let delta = events
            .iter()
            .find(|e| e.event == "content_block_delta")
            .expect("应有 input_json_delta");
        let partial = delta.data["delta"]["partial_json"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(partial).unwrap();
        assert_eq!(
            parsed, block["input"],
            "流式增量拼出的 input 应与非流式块一致"
        );
        assert!(events.iter().any(|e| e.event == "content_block_stop"));
    }

    // ---- <tool_use> XML 泄漏过滤 ----

    #[test]
    fn tool_use_xml_filter_strips_single_chunk_block() {
        let mut f = ToolUseXmlLeakFilter::default();
        let out = f
            .filter("before <tool_use id=\"t\" name=\"Write\">{\"path\":\"/a\"}</tool_use> after")
            + &f.finish();
        assert!(!out.contains("<tool_use"));
        assert!(out.contains("before") && out.contains("after"));
    }

    #[test]
    fn tool_use_xml_filter_strips_cross_chunk_open_split() {
        let mut f = ToolUseXmlLeakFilter::default();
        let mut out = f.filter("before <tool");
        out.push_str(&f.filter("_use id=\"t\">{\"a\":1}</tool_use>after"));
        out.push_str(&f.finish());
        assert!(!out.contains("<tool_use"), "out={out:?}");
        assert!(out.contains("before") && out.contains("after"));
    }

    /// 优化点：闭合标签 `</tool_use>` 被切分到多个 chunk 时也应完整剥离，
    /// 且其后文本不被吞（参考实现在此会漏）。
    #[test]
    fn tool_use_xml_filter_strips_cross_chunk_close_split() {
        let mut f = ToolUseXmlLeakFilter::default();
        let mut out = f.filter("x <tool_use name=\"W\">{\"a\":1}</tool");
        out.push_str(&f.filter("_use>y"));
        out.push_str(&f.finish());
        assert!(!out.contains("<tool_use"), "out={out:?}");
        assert!(
            out.contains('x') && out.contains('y'),
            "闭合跨 chunk 时其后文本不应被吞: {out:?}"
        );
    }

    #[test]
    fn tool_use_xml_filter_keeps_similar_text() {
        let mut f = ToolUseXmlLeakFilter::default();
        let out = f.filter("use <tool_user> here") + &f.finish();
        assert_eq!(out, "use <tool_user> here");
    }

    #[test]
    fn tool_use_xml_filter_keeps_unclosed_at_finish() {
        // H1：未闭合的 <tool_use 原样保留，不吞后文。
        let mut f = ToolUseXmlLeakFilter::default();
        let mut out = f.filter("keep <tool_use name=\"W\">partial...");
        out.push_str(&f.finish());
        assert_eq!(out, "keep <tool_use name=\"W\">partial...");
    }

    #[test]
    fn stream_context_filters_tool_use_xml_leak() {
        let mut ctx = StreamContext::new_with_thinking(
            "m",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_assistant_response("hello <tool"));
        events.extend(ctx.process_assistant_response("_use name=\"W\">{\"a\":1}</tool_use> world"));
        events.extend(ctx.generate_final_events());
        let text: String = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert!(!text.contains("<tool_use"), "泄漏未被过滤: {text:?}");
        assert!(text.contains("hello") && text.contains("world"));
    }

    /// 测试用的「已知工具表」：包含 invoke 测试里会合成的工具名，
    /// 让 🅳 工具表校验放行这些名字，从而能验证捞回逻辑本身。
    /// H2 测试用 schema：exec_command 用真实形态（cmd 必填、yield_time_ms 为整数），其余工具不限参数。
    fn test_tool_schemas() -> InvokeToolSchemas {
        let mut schemas: InvokeToolSchemas = test_known_tools()
            .into_iter()
            .map(|name| (name, json!({"type": "object"})))
            .collect();
        schemas.insert(
            "exec_command".to_string(),
            json!({
                "type": "object",
                "properties": {
                    "cmd": {"type": "string"},
                    "yield_time_ms": {"type": "integer"}
                },
                "required": ["cmd"]
            }),
        );
        schemas
    }

    fn test_known_tools() -> std::collections::HashSet<String> {
        [
            "exec_command",
            "apply_patch",
            "tool_a",
            "tool_b",
            "write_file",
            "wait_agent",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    // ---- extract_invoke_content_blocks: one-shot (non-streaming) reclamation ----

    #[test]
    fn extract_blocks_reclaims_clean_leak_and_strips_stray_token() {
        let text = "call\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">echo hi</parameter>\n</invoke>";
        let blocks = extract_invoke_content_blocks(
            text,
            &test_tool_schemas(),
            &std::collections::HashMap::new(),
        );
        let tu = blocks
            .iter()
            .find(|b| b["type"] == "tool_use")
            .expect("must reclaim tool_use");
        assert_eq!(tu["name"], "exec_command");
        assert_eq!(tu["input"]["cmd"], "echo hi");
        assert!(
            !blocks.iter().any(|b| b["type"] == "text"
                && b["text"]
                    .as_str()
                    .map(|t| t.contains("<invoke"))
                    .unwrap_or(false)),
            "no literal <invoke> may remain as text"
        );
        assert!(
            !blocks
                .iter()
                .any(|b| b["type"] == "text" && b["text"] == "call\n"),
            "stray token line must be stripped"
        );
    }

    #[test]
    fn extract_blocks_restores_shortened_name_via_map() {
        let short = "shrunk_name_abcd1234";
        let original = "an_extremely_long_original_tool_name_that_exceeds_the_limit";
        let text = format!(
            "call\n<invoke name=\"{}\">\n<parameter name=\"x\">y</parameter>\n</invoke>",
            short
        );
        let mut known = InvokeToolSchemas::new();
        known.insert(short.to_string(), json!({"type": "object"}));
        let mut map = std::collections::HashMap::new();
        map.insert(short.to_string(), original.to_string());
        let blocks = extract_invoke_content_blocks(&text, &known, &map);
        let tu = blocks
            .iter()
            .find(|b| b["type"] == "tool_use")
            .expect("reclaimed");
        assert_eq!(
            tu["name"], original,
            "shortened name must be restored to original"
        );
    }

    #[test]
    fn extract_blocks_does_not_reclaim_fenced_or_unknown() {
        // fenced -> display, not reclaimed
        let fenced = "see:\n```\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf /</parameter>\n</invoke>\n```";
        let b1 = extract_invoke_content_blocks(
            fenced,
            &test_tool_schemas(),
            &std::collections::HashMap::new(),
        );
        assert!(
            !b1.iter().any(|b| b["type"] == "tool_use"),
            "fenced must not reclaim"
        );
        // unknown tool name -> not reclaimed
        let unknown = "call\n<invoke name=\"not_a_real_tool\">\n<parameter name=\"x\">y</parameter>\n</invoke>";
        let b2 = extract_invoke_content_blocks(
            unknown,
            &test_tool_schemas(),
            &std::collections::HashMap::new(),
        );
        assert!(
            !b2.iter().any(|b| b["type"] == "tool_use"),
            "unknown name must not reclaim"
        );
    }

    #[test]
    fn extract_blocks_clean_text_is_single_unchanged_text_block() {
        let blocks = extract_invoke_content_blocks(
            "just a normal answer with no tool calls",
            &test_tool_schemas(),
            &std::collections::HashMap::new(),
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "just a normal answer with no tool calls");
    }

    #[test]
    fn test_sse_event_format() {
        let event = SseEvent::new("message_start", json!({"type": "message_start"}));
        let sse_str = event.to_sse_string();

        assert!(sse_str.starts_with("event: message_start\n"));
        assert!(sse_str.contains("data: "));
        assert!(sse_str.ends_with("\n\n"));
    }

    #[test]
    fn test_sse_state_manager_message_start() {
        let mut manager = SseStateManager::new();

        // 第一次应该成功
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_some());

        // 第二次应该被跳过
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_none());
    }

    #[test]
    fn test_sse_state_manager_block_lifecycle() {
        let mut manager = SseStateManager::new();

        // 创建块
        let events = manager.handle_content_block_start(0, "text", json!({}));
        assert_eq!(events.len(), 1);

        // delta
        let event = manager.handle_content_block_delta(0, json!({}));
        assert!(event.is_some());

        // stop
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_some());

        // 重复 stop 应该被跳过
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_none());
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert(
            "short_abc12345".to_string(),
            "mcp__very_long_original_tool_name".to_string(),
        );

        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, map, test_known_tools());
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events
            .iter()
            .find(|e| e.event == "content_block_start")
            .unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"], "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());

        let initial_events = ctx.generate_initial_events();
        assert!(
            initial_events
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text")
        );

        let initial_text_index = ctx
            .text_block_index
            .expect("initial text block index should exist");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true, // 累积器仅在 stop=true 时整体发出工具调用（含关闭前一个块）
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    #[test]
    fn test_tool_use_flushes_pending_thinking_buffer_text_before_tool_block() {
        // thinking 模式下正文后接 tool_use：开头不是 `<thinking>` 的正文立即输出（M1），
        // 不再滞留在 thinking_buffer；文本仍必须排在 tool_use 块之前。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_assistant_response("有修"));
        all.extend(ctx.process_assistant_response("改："));
        all.extend(ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true,
        }));
        all.extend(ctx.generate_final_events());
        assert_eq!(collect_text_content(&all), "有修改：");
        let pos_text = all
            .iter()
            .position(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .expect("text delta");
        let pos_tool = all
            .iter()
            .position(|e| e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use")
            .expect("tool_use start");
        assert!(pos_text < pos_tool, "文本必须在 tool_use 块之前");
    }

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    #[test]
    fn test_find_real_thinking_start_tag_basic() {
        // 基本情况：正常的开始标签
        assert_eq!(find_real_thinking_start_tag("<thinking>"), Some(0));
        assert_eq!(find_real_thinking_start_tag("prefix<thinking>"), Some(6));
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("`<thinking>`"), None);
        assert_eq!(find_real_thinking_start_tag("use `<thinking>` tag"), None);

        // 先有被包裹的，后有真正的开始标签
        assert_eq!(
            find_real_thinking_start_tag("about `<thinking>` tag<thinking>content"),
            Some(22)
        );
    }

    #[test]
    fn test_find_real_thinking_start_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("\"<thinking>\""), None);
        assert_eq!(find_real_thinking_start_tag("the \"<thinking>\" tag"), None);

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_start_tag("'<thinking>'"), None);

        // 混合情况
        assert_eq!(
            find_real_thinking_start_tag("about \"<thinking>\" and '<thinking>' then<thinking>"),
            Some(40)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_basic() {
        // 基本情况：正常的结束标签后面有双换行符
        assert_eq!(find_real_thinking_end_tag("</thinking>\n\n"), Some(0));
        assert_eq!(
            find_real_thinking_end_tag("content</thinking>\n\n"),
            Some(7)
        );
        assert_eq!(
            find_real_thinking_end_tag("some text</thinking>\n\nmore text"),
            Some(9)
        );

        // 没有双换行符的情况
        assert_eq!(find_real_thinking_end_tag("</thinking>"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking>\n"), None);
        assert_eq!(find_real_thinking_end_tag("</thinking> more"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_backticks() {
        // 被反引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("`</thinking>`\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("mention `</thinking>` in code\n\n"),
            None
        );

        // 只有前面有反引号
        assert_eq!(find_real_thinking_end_tag("`</thinking>\n\n"), None);

        // 只有后面有反引号
        assert_eq!(find_real_thinking_end_tag("</thinking>`\n\n"), None);
    }

    #[test]
    fn test_find_real_thinking_end_tag_with_quotes() {
        // 被双引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("\"</thinking>\"\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("the string \"</thinking>\" is a tag\n\n"),
            None
        );

        // 被单引号包裹的应该被跳过
        assert_eq!(find_real_thinking_end_tag("'</thinking>'\n\n"), None);
        assert_eq!(
            find_real_thinking_end_tag("use '</thinking>' as marker\n\n"),
            None
        );

        // 混合情况：双引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about \"</thinking>\" tag</thinking>\n\n"),
            Some(23)
        );

        // 混合情况：单引号包裹后有真正的标签
        assert_eq!(
            find_real_thinking_end_tag("about '</thinking>' tag</thinking>\n\n"),
            Some(23)
        );
    }

    #[test]
    fn test_find_real_thinking_end_tag_mixed() {
        // 先有被包裹的，后有真正的结束标签
        assert_eq!(
            find_real_thinking_end_tag("discussing `</thinking>` tag</thinking>\n\n"),
            Some(28)
        );

        // 多个被包裹的，最后一个是真正的
        assert_eq!(
            find_real_thinking_end_tag("`</thinking>` and `</thinking>` done</thinking>\n\n"),
            Some(36)
        );

        // 多种引用字符混合
        assert_eq!(
            find_real_thinking_end_tag(
                "`</thinking>` and \"</thinking>\" and '</thinking>' done</thinking>\n\n"
            ),
            Some(54)
        );
    }

    #[test]
    fn test_tool_use_immediately_after_thinking_filters_end_tag_and_closes_thinking_block() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();

        // thinking 内容以 `</thinking>` 结尾，但后面没有 `\n\n`（模拟紧跟 tool_use 的场景）
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));

        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true, // 累积器仅在 stop=true 时整体发出工具调用（含关闭前一个块）
        });
        all_events.extend(tool_events);

        all_events.extend(ctx.generate_final_events());

        // 不应把 `</thinking>` 当作 thinking 内容输出
        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered from output"
        );

        // thinking block 必须在 tool_use block 之前关闭
        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");
        let pos_thinking_stop = all_events.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });
        let pos_tool_start = all_events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });
        assert!(
            pos_thinking_stop.is_some(),
            "thinking block should be stopped"
        );
        assert!(pos_tool_start.is_some(), "tool_use block should be started");
        assert!(
            pos_thinking_stop.unwrap() < pos_tool_start.unwrap(),
            "thinking block should stop before tool_use block starts"
        );
    }

    #[test]
    fn test_thinking_block_emits_signature_delta_before_stop() {
        // 客户端在 thinking 模式下要求 thinking 块带 signature 字段，否则下一轮回传时
        // 会抛出 "must be passed back to the API"。本测试验证 thinking 块结束前发送了
        // 一个非空的 signature_delta 事件。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>abc</thinking>\n\nhello"));
        all.extend(ctx.generate_final_events());

        let thinking_index = ctx
            .thinking_block_index
            .expect("thinking block index should exist");

        let pos_sig = all.iter().position(|e| {
            e.event == "content_block_delta"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
        });
        let pos_stop = all.iter().position(|e| {
            e.event == "content_block_stop"
                && e.data["index"].as_i64() == Some(thinking_index as i64)
        });

        assert!(pos_sig.is_some(), "signature_delta should be emitted");
        assert!(pos_stop.is_some(), "content_block_stop should be emitted");
        assert!(
            pos_sig.unwrap() < pos_stop.unwrap(),
            "signature_delta must precede content_block_stop"
        );
    }

    #[test]
    fn error_termination_signs_thinking_before_stop() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let mut events =
            ctx.process_reasoning_content(&crate::kiro::model::events::ReasoningContentEvent {
                text: Some("unfinished reasoning".to_string()),
                signature: None,
                redacted_content: None,
            });
        events.extend(ctx.generate_error_events("upstream_error", "stream interrupted"));

        let thinking_index = ctx.thinking_block_index.unwrap();
        let signature = events
            .iter()
            .position(|event| {
                event.event == "content_block_delta"
                    && event.data["index"] == thinking_index
                    && event.data["delta"]["type"] == "signature_delta"
            })
            .expect("thinking error close must include a signature");
        let stop = events
            .iter()
            .position(|event| {
                event.event == "content_block_stop" && event.data["index"] == thinking_index
            })
            .expect("thinking block must close");
        let error = events
            .iter()
            .position(|event| event.event == "error")
            .expect("stream must end with error");
        assert!(signature < stop && stop < error);
        assert!(events.iter().all(|event| event.event != "message_stop"));
    }

    #[test]
    fn test_final_flush_filters_standalone_thinking_end_tag() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        assert!(
            all_events.iter().all(|e| {
                !(e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "thinking_delta"
                    && e.data["delta"]["thinking"] == "</thinking>")
            }),
            "`</thinking>` should be filtered during final flush"
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_same_chunk() {
        // <thinking>\n 在同一个 chunk 中，\n 应被剥离
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nHello world");

        // 找到所有 thinking_delta 事件
        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        // 拼接所有 thinking 内容
        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_strips_leading_newline_cross_chunk() {
        // <thinking> 在第一个 chunk 末尾，\n 在第二个 chunk 开头
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let events1 = ctx.process_assistant_response("<thinking>");
        let events2 = ctx.process_assistant_response("\nHello world");

        let mut all_events = Vec::new();
        all_events.extend(events1);
        all_events.extend(events2);

        let thinking_deltas: Vec<_> = all_events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_thinking.starts_with('\n'),
            "thinking content should not start with \\n across chunks, got: {:?}",
            full_thinking
        );
    }

    #[test]
    fn test_thinking_no_strip_when_no_leading_newline() {
        // <thinking> 后直接跟内容（无 \n），内容应完整保留
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>abc</thinking>\n\ntext");

        let thinking_deltas: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .collect();

        let full_thinking: String = thinking_deltas
            .iter()
            .filter(|e| {
                !e.data["delta"]["thinking"]
                    .as_str()
                    .unwrap_or("")
                    .is_empty()
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .collect();

        assert_eq!(full_thinking, "abc", "thinking content should be 'abc'");
    }

    #[test]
    fn test_text_after_thinking_strips_leading_newlines() {
        // `</thinking>\n\n` 后的文本不应以 \n\n 开头
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let events = ctx.process_assistant_response("<thinking>\nabc</thinking>\n\n你好");

        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .collect();

        let full_text: String = text_deltas
            .iter()
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect();

        assert!(
            !full_text.starts_with('\n'),
            "text after thinking should not start with \\n, got: {:?}",
            full_text
        );
        assert_eq!(full_text, "你好");
    }

    /// 辅助函数：从事件列表中提取所有 thinking_delta 的拼接内容
    fn collect_thinking_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| {
                e.event == "content_block_delta" && e.data["delta"]["type"] == "thinking_delta"
            })
            .map(|e| e.data["delta"]["thinking"].as_str().unwrap_or(""))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有 text_delta 的拼接内容
    fn collect_text_content(events: &[SseEvent]) -> String {
        events
            .iter()
            .filter(|e| e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta")
            .map(|e| e.data["delta"]["text"].as_str().unwrap_or(""))
            .collect()
    }

    /// 辅助函数：从事件列表中提取所有合成的 tool_use 调用
    ///
    /// 抓 `content_block_start` 里 `content_block.type == "tool_use"` 的 name，
    /// 再配对同 index 的 `input_json_delta.partial_json`，返回 (name, input_json)。
    fn collect_tool_uses(events: &[SseEvent]) -> Vec<(String, String)> {
        let mut result = Vec::new();
        for e in events.iter() {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use" {
                let index = e.data["index"].as_i64();
                let name = e.data["content_block"]["name"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                // 找同 index 的 input_json_delta
                let input = events
                    .iter()
                    .find(|d| {
                        d.event == "content_block_delta"
                            && d.data["index"].as_i64() == index
                            && d.data["delta"]["type"] == "input_json_delta"
                    })
                    .and_then(|d| d.data["delta"]["partial_json"].as_str())
                    .unwrap_or("")
                    .to_string();
                result.push((name, input));
            }
        }
        result
    }

    #[test]
    fn test_invoke_sniff_backtick_wrapped_is_not_captured() {
        // 🔴 防误伤：被反引号包裹的 <invoke> 是引用，不应被抓
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("示例：`<invoke name=\"x\">` 这种写法"));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "被反引号包裹的不应被抓: {:?}", tools);

        let text = collect_text_content(&all);
        assert!(
            text.contains("<invoke name=\"x\">"),
            "原文应原样保留在 text 中: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_sniff_single_bare_invoke() {
        // 🟢 单个裸 invoke（无外壳）
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);
        assert_eq!(tools[0].0, "exec_command", "name 应为 exec_command");
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert_eq!(parsed["cmd"], "ls", "input 应含 cmd=ls");
    }

    #[test]
    fn test_invoke_sniff_param_value_with_lt_multiline_chinese() {
        // 🟢 参数值含 `<`、多行、中文 → 不被截断
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let value = "第一行 a < b\n第二行 路径 /tmp/中文";
        let chunk = format!(
            "<invoke name=\"write_file\"><parameter name=\"content\">{}</parameter></invoke>",
            value
        );
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(&chunk));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert_eq!(
            parsed["content"], value,
            "参数值应完整保留（含 < / 多行 / 中文）"
        );
    }

    #[test]
    fn test_invoke_sniff_two_invokes_sequential() {
        // 🟢 2 个 invoke 串联 → 2 个 tool_use
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "<invoke name=\"tool_a\"><parameter name=\"x\">1</parameter></invoke><invoke name=\"tool_b\"><parameter name=\"y\">2</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 2, "应合成 2 个 tool_use: {:?}", tools);
        assert_eq!(tools[0].0, "tool_a");
        assert_eq!(tools[1].0, "tool_b");
    }

    #[test]
    fn test_invoke_sniff_split_across_chunks() {
        // 🟢 跨 chunk 分片：标签被切碎多次喂入
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<inv"));
        all.extend(ctx.process_assistant_response("oke name=\"exec_command\">"));
        all.extend(ctx.process_assistant_response("<parameter name=\"cmd\">ls</parameter></in"));
        all.extend(ctx.process_assistant_response("voke>"));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "跨 chunk 应合成 1 个 tool_use: {:?}", tools);
        assert_eq!(tools[0].0, "exec_command");
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert_eq!(parsed["cmd"], "ls");
    }

    #[test]
    fn test_invoke_sniff_strips_stray_call_token() {
        // 🟢 stray token：<invoke> 前有单独一行 `call` → 剥掉，text 不含残留 call
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "call\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);

        let text = collect_text_content(&all);
        assert!(
            !text.contains("call"),
            "前置的 stray `call` 应被剥掉，text 不应残留: {:?}",
            text
        );
    }

    #[test]
    fn strip_trailing_stray_preserves_preceding_newline() {
        // 回归：narrative 文本后跟一行 stray token（`some text\ncall`）。
        // 旧实现把 stray 行连同其【前面的换行】一起剥掉 -> 得到 "some text"（无换行结尾），
        // 这会让随后的 invoke_looks_like_real_leak 行首启发式失败、漏捞真泄漏。
        // 正确：只剥 stray 行本身，保留前一行的换行 -> "some text\n"。
        let got = strip_trailing_stray_tokens("some text\ncall");
        assert_eq!(
            got, "some text\n",
            "must keep the newline terminating the narrative line so the invoke stays line-start"
        );
        // 且剥完的结果应让行首判定通过
        assert!(
            invoke_looks_like_real_leak(got),
            "stripped narrative must still look like a line-start leak (ends with newline)"
        );
    }

    #[test]
    fn test_invoke_sniff_reclaims_after_narrative_then_stray_token() {
        // 端到端：`正文\ncall\n<invoke...>` —— 正文 + stray token + 真泄漏 invoke。
        // 旧实现漏捞（stray 剥过头把正文和 invoke 挤一行），修后应成功捞回 tool_use。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "先看看结果。\ncall\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(
            tools.len(),
            1,
            "narrative+stray+invoke 应捞回 1 个 tool_use: {:?}",
            tools
        );
        let text = collect_text_content(&all);
        assert!(text.contains("先看看结果"), "叙述正文应保留: {:?}", text);
        assert!(
            !text.contains("call\n<invoke") && !text.contains("<invoke"),
            "invoke 不应泄漏为文本: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_sniff_keeps_narrative_before_invoke() {
        // 🟢 invoke 前有叙述：text 含"先看看"，1 个 tool_use
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "先看看\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);

        let text = collect_text_content(&all);
        assert!(
            text.contains("先看看"),
            "叙述文本应保留在 text 中: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_sniff_truncated_block_not_captured() {
        // 🔴 截断半块（无 </invoke> 闭合）→ 0 tool_use
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "<invoke name=\"exec_command\"><parameter name=\"cmd\">ls",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "未闭合的块不应被抓: {:?}", tools);
    }

    #[test]
    fn test_invoke_midsentence_not_captured() {
        // 🔴 P1：正文里嵌在句子中间（无反引号、非行首）的 <invoke> 是讨论文本，不应被抓
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "解析器示意：模型吐出 <invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke> 这种文本",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(
            tools.is_empty(),
            "句中讨论的 <invoke> 不应被抓: {:?}",
            tools
        );

        let text = collect_text_content(&all);
        assert!(
            text.contains("解析器示意") && text.contains("这种文本"),
            "正文应完整保留（含前后叙述）: {:?}",
            text
        );
        assert!(
            text.contains("<invoke name=\"exec_command\">"),
            "原 <invoke> 文本应原样保留在 text 中: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_midsentence_unclosed_not_hold() {
        // 🔴 P2：流式中途遇到句中不闭合的 <invoke，不应 hold 住后续文本到流末尾
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        // 第一次 process：句中不闭合的 <invoke>，前面同一行有正文“讨论”
        let first = ctx.process_assistant_response("讨论 <invoke name=\"x\"> 语义，");
        let first_text = collect_text_content(&first);
        assert!(
            first_text.contains("讨论"),
            "句中不闭合的 <invoke 不应 hold 住正文，应及时吐出“讨论”: {:?}",
            first_text
        );

        let mut all = first;
        all.extend(ctx.process_assistant_response("后面内容。"));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(
            tools.is_empty(),
            "不闭合的句中 <invoke 不应被抓: {:?}",
            tools
        );

        let text = collect_text_content(&all);
        assert!(
            text.contains("讨论") && text.contains("语义") && text.contains("后面内容。"),
            "全部正文应完整保留: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_multiline_patch_split_still_captured() {
        // 🟢 P3：行首合法 invoke，参数值是 20+ 行多行文本（模拟 apply_patch），
        // 逐行流式喂入。修复前换行数 ≥16 会被 too_long 误杀降级成文本；修复后应抓到。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        // 构造一个 24 行的多行 patch 内容
        let mut patch_lines = Vec::new();
        for i in 0..24 {
            patch_lines.push(format!("+ line number {i} of the patch body"));
        }
        let patch_value = patch_lines.join("\n");

        // 整块拼好后，按行切片逐片喂入（每片末尾补回换行，最后一行不补）
        let full = format!(
            "<invoke name=\"apply_patch\"><parameter name=\"input\">{}</parameter></invoke>",
            patch_value
        );
        let mut all = Vec::new();
        // 按换行拆成片，逐片喂；保证 invoke 在每片到齐前换行数早已 ≥16
        let bytes = full.as_bytes();
        let mut idx = 0;
        while idx < bytes.len() {
            // 找到下一个换行边界（含换行）作为一片
            let mut end = idx;
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            if end < bytes.len() {
                end += 1; // 把换行也带上
            }
            let piece = std::str::from_utf8(&bytes[idx..end]).unwrap();
            all.extend(ctx.process_assistant_response(piece));
            idx = end;
        }
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(
            tools.len(),
            1,
            "分片喂入的多行 invoke 应抓到 1 个 tool_use: {:?}",
            tools
        );
        assert_eq!(tools[0].0, "apply_patch", "name 应为 apply_patch");
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert_eq!(
            parsed["input"], patch_value,
            "多行参数值应完整保留（换行不丢）"
        );
    }

    #[test]
    fn test_invoke_large_patch_split_captured() {
        // 🟢 P3：参数值 ~17KB 多行，分片喂入，断言抓到 1 个 tool_use（在 256KB 上限之下）。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        // 每行 ~70 字节 × 250 行 ≈ 17KB
        let mut lines = Vec::new();
        for i in 0..250 {
            lines.push(format!(
                "+ patch content row {i:04} padding xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
            ));
        }
        let big_value = lines.join("\n");
        assert!(
            big_value.len() > 16 * 1024,
            "测试数据应 >16KB，实际 {}",
            big_value.len()
        );

        let full = format!(
            "<invoke name=\"apply_patch\"><parameter name=\"input\">{}</parameter></invoke>",
            big_value
        );
        // 固定 512 字节一片喂入（注意 UTF-8 边界，这里内容是 ASCII 安全）
        let mut all = Vec::new();
        let bytes = full.as_bytes();
        let mut idx = 0;
        while idx < bytes.len() {
            let end = (idx + 512).min(bytes.len());
            let piece = std::str::from_utf8(&bytes[idx..end]).unwrap();
            all.extend(ctx.process_assistant_response(piece));
            idx = end;
        }
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(
            tools.len(),
            1,
            "~17KB 分片喂入的 invoke 应抓到 1 个 tool_use: {:?}",
            tools.iter().map(|t| &t.0).collect::<Vec<_>>()
        );
        assert_eq!(tools[0].0, "apply_patch");
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert_eq!(parsed["input"], big_value, "大 patch 参数值应完整保留");
    }

    #[test]
    fn test_unclosed_invoke_eventually_flushed_as_text() {
        // 🟢 锁定字节兜底仍在：行首 `<invoke>` 永不闭合、喂入超过 MAX_INVOKE_HOLD_BYTES，
        // 应被当文本吐出（不无限 hold）。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        // 行首开标签，永不闭合；填充超过上限的纯文本（无 </invoke>）
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<invoke name=\"x\">"));
        // 一次喂入超过上限的内容（用不含 `<` 的填充，避免触发其它路径）
        let filler = "A".repeat(MAX_INVOKE_HOLD_BYTES + 1024);
        all.extend(ctx.process_assistant_response(&filler));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(
            tools.is_empty(),
            "永不闭合的 invoke 不应被抓: {:?}",
            tools.len()
        );

        let text = collect_text_content(&all);
        assert!(
            text.contains("<invoke name=\"x\">"),
            "超上限的未闭合块应被当文本吐出（含开标签）"
        );
        assert!(
            text.contains(&"A".repeat(100)),
            "填充文本应被吐出，不应无限 hold"
        );
    }

    #[test]
    fn test_invoke_in_markdown_list_not_captured() {
        // 🔴 markdown 列表项 `- <invoke>` 当讨论文本，不抓。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "- <invoke name=\"exec_command\"><parameter name=\"cmd\">rm -rf /</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(
            tools.is_empty(),
            "markdown 列表里的 <invoke> 不应被抓: {:?}",
            tools
        );
        let text = collect_text_content(&all);
        assert!(
            text.contains("rm -rf /"),
            "危险命令应留在文本里、不被执行: {:?}",
            text
        );
    }

    #[test]
    fn test_invoke_in_blockquote_not_captured() {
        // 🔴 引用 `> <invoke>` 当讨论文本，不抓。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "> <invoke name=\"exec_command\"><parameter name=\"cmd\">rm -rf /</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert!(
            tools.is_empty(),
            "引用块里的 <invoke> 不应被抓: {:?}",
            tools
        );
        let text = collect_text_content(&all);
        assert!(
            text.contains("rm -rf /"),
            "危险命令应留在文本里、不被执行: {:?}",
            text
        );
    }

    fn block_start_position(events: &[SseEvent], block_type: &str) -> (usize, i64) {
        let pos = events
            .iter()
            .position(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == block_type
            })
            .unwrap_or_else(|| panic!("{block_type} block should start"));
        let idx = events[pos].data["index"]
            .as_i64()
            .unwrap_or_else(|| panic!("{block_type} block index should exist"));
        (pos, idx)
    }

    fn block_stop_position(events: &[SseEvent], index: i64) -> usize {
        events
            .iter()
            .position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(index)
            })
            .unwrap_or_else(|| panic!("block {index} should stop"))
    }

    #[test]
    fn test_end_tag_newlines_split_across_events() {
        // `</thinking>\n` 在 chunk 1，`\n` 在 chunk 2，`text` 在 chunk 3
        // 确保 `</thinking>` 不会被部分当作 thinking 内容发出
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_end_tag_alone_in_chunk_then_newlines_in_next() {
        // `</thinking>` 单独在一个 chunk，`\n\ntext` 在下一个 chunk
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all.extend(ctx.process_assistant_response("\n\n你好"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "你好", "text should be '你好', got: {:?}", text);
    }

    #[test]
    fn test_start_tag_newline_split_across_events() {
        // `\n\n` 在 chunk 1，`<thinking>` 在 chunk 2，`\n` 在 chunk 3
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("\n\n"));
        all.extend(ctx.process_assistant_response("<thinking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("abc</thinking>\n\ntext"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "abc",
            "thinking should be 'abc', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "text", "text should be 'text', got: {:?}", text);
    }

    #[test]
    fn test_full_flow_maximally_split() {
        // 极端拆分：每个关键边界都在不同 chunk
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let mut all = Vec::new();
        // \n\n<thinking>\n 拆成多段
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("<thin"));
        all.extend(ctx.process_assistant_response("king>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("hello"));
        // </thinking>\n\n 拆成多段
        all.extend(ctx.process_assistant_response("</thi"));
        all.extend(ctx.process_assistant_response("nking>"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("\n"));
        all.extend(ctx.process_assistant_response("world"));
        all.extend(ctx.generate_final_events());

        let thinking = collect_thinking_content(&all);
        assert_eq!(
            thinking, "hello",
            "thinking should be 'hello', got: {:?}",
            thinking
        );

        let text = collect_text_content(&all);
        assert_eq!(text, "world", "text should be 'world', got: {:?}", text);
    }

    #[test]
    fn test_thinking_only_ends_turn_without_padding() {
        // L2：只有 thinking、上游没有截断信号时按 end_turn 正常结束，不补空格正文。
        let mut ctx = fr_ctx(true);
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_assistant_response("<thinking>只是想了想</thinking>"));
        all.extend(ctx.generate_final_events());
        assert_eq!(fr_stop_reason(&all), json!("end_turn"));
        assert_eq!(collect_text_content(&all), "");
        assert_eq!(collect_thinking_content(&all), "只是想了想");
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text 的情况，stop_reason 应为 end_turn
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>\n\nHello"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        // thinking + tool_use 的情况，stop_reason 应为 tool_use
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(
            ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
                name: "test_tool".to_string(),
                tool_use_id: "tool_1".to_string(),
                input: "{}".to_string(),
                stop: true,
            }),
        );
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }

    // ===== 新增回归测试：P0-1 参数含字面 XML / 🅱 代码围栏 / 🅳 工具表 / 🅲 card =====

    /// 🅿️ P0-1：参数值里含字面 `</invoke>`，块不应被假闭合截断，input 要完整。
    #[test]
    fn test_invoke_param_value_contains_literal_invoke_close() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        // patch 正文里出现字面 </invoke>，真正的闭合在最后
        let payload = "count\n<invoke name=\"apply_patch\"><parameter name=\"input\">line1\n</invoke>\nstill in patch\nline3</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);
        assert_eq!(tools[0].0, "apply_patch");
        let parsed: serde_json::Value = serde_json::from_str(&tools[0].1).expect("合法 JSON");
        let input = parsed["input"].as_str().expect("有 input");
        assert!(
            input.contains("still in patch"),
            "input 不应被假闭合截断: {input:?}"
        );
        assert!(input.contains("line3"), "input 应含 line3: {input:?}");
        let text = collect_text_content(&all);
        assert!(
            !text.contains("still in patch"),
            "patch 正文不应泄漏到 text: {text:?}"
        );
    }

    /// 🅿️ P0-1：参数值里含字面 `</parameter>`，值不应被截断丢失后半段。
    #[test]
    fn test_invoke_param_value_contains_literal_parameter_close() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let payload = "count\n<invoke name=\"apply_patch\"><parameter name=\"input\">before</parameter> after the fake close</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);
        let parsed: serde_json::Value = serde_json::from_str(&tools[0].1).expect("合法 JSON");
        let input = parsed["input"].as_str().expect("有 input");
        assert!(
            input.contains("after the fake close"),
            "后半段不应丢: {input:?}"
        );
    }

    /// 🅱：代码围栏（```）内的 <invoke> 是正文展示，不应被捞回成 tool_use。
    #[test]
    fn test_invoke_inside_code_fence_not_captured() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let payload = "示例代码：\n```\n<invoke name=\"exec_command\"><parameter name=\"cmd\">rm -rf /</parameter></invoke>\n```\n讲解完毕。";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "围栏内展示文本不应被捞回: {:?}", tools);
        let text = collect_text_content(&all);
        assert!(
            text.contains("<invoke name=\"exec_command\">"),
            "应原样保留: {text:?}"
        );
    }

    /// 🅳：合成出的工具名不在已知工具表里 → 不捞回，当文本吐出（防误执行）。
    #[test]
    fn test_invoke_unknown_tool_name_not_synthesized() {
        // 已知工具表里没有 totally_unknown_tool
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let payload = "count\n<invoke name=\"totally_unknown_tool\"><parameter name=\"x\">1</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "未知工具名不应被合成: {:?}", tools);
        let text = collect_text_content(&all);
        assert!(
            text.contains("totally_unknown_tool"),
            "未知工具应原样当文本: {text:?}"
        );
    }

    /// 🅳：已知工具表为空（请求没带 tools）→ 一律不捞回，宁可漏捞不可误执行。
    #[test]
    fn test_invoke_empty_known_tools_never_captured() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let _ = ctx.generate_initial_events();
        let payload =
            "count\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "工具表为空时不应捞回: {:?}", tools);
    }

    /// 🅲：stray token `card` 也应被剥掉，块仍被捞回。
    #[test]
    fn test_invoke_strips_stray_card_token() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let payload = "我先等结果。\n\ncard\n<invoke name=\"wait_agent\"><parameter name=\"x\">1</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "card 前缀的块应被捞回: {:?}", tools);
        assert_eq!(tools[0].0, "wait_agent");
        let text = collect_text_content(&all);
        assert!(
            !text.contains("card"),
            "card stray token 不应泄漏: {text:?}"
        );
        assert!(text.contains("我先等结果"), "正常叙述应保留: {text:?}");
    }

    /// 🅱 跨 chunk：``` 围栏开标签在 chunk 边界被切碎，仍能正确识别围栏内不捞回。
    #[test]
    fn test_invoke_fence_split_across_chunks() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let mut all = Vec::new();
        // 围栏开标签分两个 chunk 到达
        all.extend(ctx.process_assistant_response("看代码：\n``"));
        all.extend(ctx.process_assistant_response(
            "`\n<invoke name=\"exec_command\"><parameter name=\"cmd\">x</parameter></invoke>\n```",
        ));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "跨 chunk 围栏内不应捞回: {:?}", tools);
    }

    /// 🟡 回归（Reviewer 问题1）：连发 burst，块 A 在 `</invoke>` 前混了非 `>` 收尾文字，
    /// 不应把 A、B 误合并成一个块、也不应让 B 的参数串进 A。两个块都应独立捞回。
    #[test]
    fn test_invoke_burst_with_malformed_block_stays_text() {
        // H2：A 块参数后夹着游离正文（结构不干净），拿不准就整段按正文保留；
        // 紧贴其后的 B 块不在行首，同样不合成。A 的参数因此不可能串进 B。
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let payload = "count\n<invoke name=\"tool_a\"><parameter name=\"x\">1</parameter>trailing plain</invoke><invoke name=\"tool_b\"><parameter name=\"y\">2</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        assert!(collect_tool_uses(&all).is_empty());
        assert_eq!(collect_text_content(&all), payload);
    }

    /// 🟢 正常连发 burst（块紧贴、A 以 </parameter> 收尾）仍应正确拆成两个。
    #[test]
    fn test_invoke_burst_clean_two_blocks() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let payload = "count\n<invoke name=\"tool_a\"><parameter name=\"x\">1</parameter></invoke><invoke name=\"tool_b\"><parameter name=\"y\">2</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 2, "紧贴连发应拆成 2 个: {:?}", tools);
        assert_eq!(tools[0].0, "tool_a");
        assert_eq!(tools[1].0, "tool_b");
    }

    /// 🔁 回放验证：用问题 thread `019e9e8d` 里真实的 `count\n<invoke>` 泄漏原文，
    /// 断言新容错把它捞回成结构化 tool_use（而不是泄漏成字面 XML 文本）。
    /// 真实工具名 exec_command 在工具表里 → 应捞回；参数 cmd / yield_time_ms 完整。
    #[test]
    fn test_invoke_real_leak_sample_from_thread_019e9e8d() {
        let known: std::collections::HashSet<String> =
            ["exec_command", "update_plan", "update_goal"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), known)
                .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        // 逐字摘自 thread 019e9e8d 真实泄漏 assistant 消息
        let real = "）。\n\ncount\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">cd /Users/yuyifeng/.codex/everything-codex/runtime/agent-tools && python3 -m pytest -q -p no:cacheprovider objects/dev/beads/leaves/create_issue/ 2>&1 | tail -8</parameter>\n<parameter name=\"yield_time_ms\">60000</parameter>\n</invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(real));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(
            tools.len(),
            1,
            "真实泄漏样本应被捞回成 1 个 tool_use: {:?}",
            tools
        );
        assert_eq!(tools[0].0, "exec_command", "name 应为 exec_command");
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert!(
            parsed["cmd"].as_str().unwrap_or("").contains("pytest"),
            "cmd 参数应完整保留: {:?}",
            parsed
        );
        assert_eq!(parsed["yield_time_ms"], json!(60000), "yield_time_ms 参数应保留");
        // 关键：字面 <invoke> 不应泄漏到 text
        let text = collect_text_content(&all);
        assert!(
            !text.contains("<invoke name=\"exec_command\">"),
            "字面 <invoke> 不应泄漏到文本: {:?}",
            text
        );
        // count stray token 也不应泄漏
        assert!(
            !text.contains("\ncount\n") && !text.ends_with("count"),
            "count stray token 不应泄漏: {:?}",
            text
        );
    }

    // ---- 复读熔断 (repeat guard)：root cause = Opus 长上下文退化复读 ----

    /// 🔴→🟢 复现真实泄漏：模型一句正常话后无限复读 `count`（thread 019ea4e9 的真账）。
    /// 熔断后吐出的 count 数必须远小于喂入的数量，且不撑满输出。
    #[test]
    fn repeat_guard_trips_on_count_flood() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        // 真实形态：正常话 + call + 海量 count（这里用 5000 次模拟 3.2 万次）
        let mut payload = String::from("先看 crawlee 状态。\n\ncall\n\n");
        for _ in 0..5000 {
            payload.push_str("count\n\n");
        }
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(&payload));
        all.extend(ctx.generate_final_events());

        let text = collect_text_content(&all);
        let emitted_counts = text.matches("count").count();
        assert!(
            emitted_counts < 64,
            "复读应被熔断：吐出的 count 数应远小于喂入的 5000，实际={}",
            emitted_counts
        );
        // 正常开头那句话必须保留（熔断不能误伤正文）
        assert!(
            text.contains("先看 crawlee 状态"),
            "熔断不应误伤正常正文: {:?}",
            &text[..text.len().min(80)]
        );
    }

    /// 🟢 不误伤：正常工具调用前的 1 个引导词 `count` + 真 <invoke> 仍被正常捞回。
    #[test]
    fn repeat_guard_does_not_trip_on_single_stray_token() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let payload =
            "count\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(
            tools.len(),
            1,
            "单个引导词不应触发熔断，invoke 应正常捞回: {:?}",
            tools
        );
        assert_eq!(tools[0].0, "exec_command");
    }

    /// 🟢 不误伤：正常多行文本里偶尔出现 count 单词（非独占行复读）不熔断。
    #[test]
    fn repeat_guard_does_not_trip_on_normal_prose() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let payload =
            "我数了一下 count = 3，然后继续做别的事。\n这是第二行正常文字。\n第三行也正常。";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let text = collect_text_content(&all);
        assert!(
            text.contains("我数了一下"),
            "正常正文不应被熔断: {:?}",
            text
        );
        assert!(
            text.contains("第三行也正常"),
            "正常正文应完整保留: {:?}",
            text
        );
    }

    /// 🟢 跨 chunk 复读也能熔断（流式分片到达，每片一个 count）。
    #[test]
    fn repeat_guard_trips_across_chunks() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("call\n\n"));
        for _ in 0..2000 {
            all.extend(ctx.process_assistant_response("count\n\n"));
        }
        all.extend(ctx.generate_final_events());
        let text = collect_text_content(&all);
        let emitted_counts = text.matches("count").count();
        assert!(
            emitted_counts < 64,
            "跨 chunk 复读也应熔断：实际吐出 count={}",
            emitted_counts
        );
    }

    // ---- 块级复读熔断 (collapse_stray_token_floods)：覆盖 web_search loop 路径 ----

    /// 🔴→🟢 块级路径（extract_invoke_content_blocks / web_search loop）也必须熔断 count 洪水。
    #[test]
    fn extract_blocks_collapses_count_flood() {
        let mut text = String::from("先看 crawlee 状态。\n\ncall\n\n");
        for _ in 0..5000 {
            text.push_str("count\n\n");
        }
        let blocks = extract_invoke_content_blocks(
            &text,
            &test_tool_schemas(),
            &std::collections::HashMap::new(),
        );
        let joined: String = blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect();
        let emitted = joined.matches("count").count();
        assert!(emitted < 64, "块级路径应折叠 count 洪水：实际={}", emitted);
        assert!(
            joined.contains("先看 crawlee 状态"),
            "正常正文应保留: {:?}",
            &joined[..joined.len().min(60)]
        );
    }

    /// 🟢 块级不误伤：单个引导词 count + 真 invoke 仍被捞回。
    #[test]
    fn extract_blocks_keeps_single_stray_and_reclaims() {
        let text = "count\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>";
        let blocks = extract_invoke_content_blocks(
            text,
            &test_tool_schemas(),
            &std::collections::HashMap::new(),
        );
        assert!(
            blocks
                .iter()
                .any(|b| b["type"] == "tool_use" && b["name"] == "exec_command"),
            "单个引导词不应触发折叠，invoke 应捞回: {:?}",
            blocks
        );
    }

    #[test]
    fn test_native_reasoning_event_emits_thinking_with_signature() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("native reasoning".to_string()),
                signature: Some("real-signature".to_string()),
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.process_assistant_response("final answer"));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(collect_thinking_content(&all_events), "native reasoning");
        assert_eq!(collect_text_content(&all_events), "final answer");
        assert!(all_events.iter().any(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"] == "real-signature"
        }));
    }

    #[test]
    fn test_native_reasoning_signature_only_applies_to_next_thinking_text() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: None,
                signature: Some("signature-before-text".to_string()),
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("delayed native reasoning".to_string()),
                signature: None,
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(
            collect_thinking_content(&all_events),
            "delayed native reasoning"
        );
        assert!(all_events.iter().any(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"] == "signature-before-text"
        }));
    }

    #[test]
    fn test_native_reasoning_text_downgrades_to_text_when_thinking_disabled() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            false,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("visible reasoning fallback".to_string()),
                signature: Some("ignored-signature".to_string()),
                redacted_content: Some("ignored-redacted".to_string()),
            },
        )));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(
            collect_text_content(&all_events),
            "visible reasoning fallback"
        );
        assert_eq!(collect_thinking_content(&all_events), "");
        assert!(!all_events.iter().any(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "signature_delta"
        }));
        assert!(!all_events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "redacted_thinking"
        }));
    }

    #[test]
    fn test_native_redacted_thinking_is_ordered_between_thinking_and_text() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("native reasoning".to_string()),
                signature: Some("real-signature".to_string()),
                redacted_content: None,
            },
        )));
        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: None,
                signature: None,
                redacted_content: Some("encrypted-thinking".to_string()),
            },
        )));
        all_events.extend(ctx.process_assistant_response("final answer"));
        all_events.extend(ctx.generate_final_events());

        let (_, thinking_idx) = block_start_position(&all_events, "thinking");
        let thinking_stop_pos = block_stop_position(&all_events, thinking_idx);
        let (redacted_start_pos, redacted_idx) =
            block_start_position(&all_events, "redacted_thinking");
        let redacted_stop_pos = block_stop_position(&all_events, redacted_idx);
        let (text_start_pos, _) = block_start_position(&all_events, "text");

        assert!(
            thinking_stop_pos < redacted_start_pos,
            "thinking block must close before redacted_thinking starts"
        );
        assert!(
            redacted_stop_pos < text_start_pos,
            "redacted_thinking block must close before text starts"
        );
        assert_eq!(collect_thinking_content(&all_events), "native reasoning");
        assert_eq!(collect_text_content(&all_events), "final answer");
    }

    #[test]
    fn test_native_reasoning_event_emits_redacted_thinking() {
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            1,
            true,
            HashMap::new(),
            std::collections::HashSet::new(),
        );
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: None,
                signature: None,
                redacted_content: Some("encrypted-thinking".to_string()),
            },
        )));
        all_events.extend(ctx.generate_final_events());

        assert!(all_events.iter().any(|e| {
            e.event == "content_block_start"
                && e.data["content_block"]["type"] == "redacted_thinking"
                && e.data["content_block"]["data"] == "encrypted-thinking"
        }));
    }

    // ---- credit_usage 透传 ----

    fn parse_metering(payload: &str) -> MeteringEvent {
        serde_json::from_str(payload).unwrap()
    }

    #[test]
    fn test_generate_final_events_omits_credit_fields_without_metering() {
        // 没有 meteringEvent 时不应在 usage 里写 credit_* 字段。
        let mut manager = SseStateManager::new();
        let events = manager.generate_final_events(10, 5, 0, 0, None);
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
    }

    #[test]
    fn test_generate_final_events_carries_credit_fields_when_metering_present() {
        let mut manager = SseStateManager::new();
        let metering = parse_metering(r#"{"unit":"credit","unitPlural":"credits","usage":0.75}"#);
        let events = manager.generate_final_events(10, 5, 0, 0, Some(&metering));
        let delta = events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert_eq!(usage["credit_usage"], json!(0.75));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
        // 既有字段保持原样
        assert_eq!(usage["input_tokens"], json!(10));
        assert_eq!(usage["output_tokens"], json!(5));
    }

    #[test]
    fn error_events_close_open_blocks_without_normal_message_terminal() {
        let mut manager = SseStateManager::new();
        let _ = manager.handle_content_block_start(
            0,
            "text",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        );

        let events = manager.generate_error_events("upstream_error", "stream interrupted");
        assert_eq!(
            events
                .iter()
                .map(|event| event.event.as_str())
                .collect::<Vec<_>>(),
            vec!["content_block_stop", "error"]
        );
        assert_eq!(events[1].data["error"]["type"], json!("upstream_error"));
        assert!(
            events
                .iter()
                .all(|event| { event.event != "message_delta" && event.event != "message_stop" })
        );
    }

    #[test]
    fn test_stream_context_keeps_latest_metering_in_message_delta() {
        use crate::kiro::model::events::MeteringEvent;

        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-7",
            100,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();

        // 第一次下发（异常：上游几乎只会下发一次）
        ctx.process_kiro_event(&Event::Metering(MeteringEvent {
            unit: "credit".into(),
            unit_plural: "credits".into(),
            usage: 0.10,
        }));
        // 第二次下发（应覆盖）
        ctx.process_kiro_event(&Event::Metering(MeteringEvent {
            unit: "credit".into(),
            unit_plural: "credits".into(),
            usage: 0.42,
        }));

        let final_events = ctx.generate_final_events();
        let delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert_eq!(usage["credit_usage"], json!(0.42));
        assert_eq!(usage["credit_unit"], json!("credit"));
        assert_eq!(usage["credit_unit_plural"], json!("credits"));
        // 累计 credit 仍然是两次之和
        assert!((ctx.credits - 0.52).abs() < 1e-9);
    }

    #[test]
    fn test_stream_context_omits_credit_fields_without_metering() {
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-7",
            100,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let _ = ctx.generate_initial_events();
        let final_events = ctx.generate_final_events();
        let delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("must have message_delta");
        let usage = &delta.data["usage"];
        assert!(usage.get("credit_usage").is_none());
        assert!(usage.get("credit_unit").is_none());
        assert!(usage.get("credit_unit_plural").is_none());
    }

    #[test]
    fn provider_usage_overrides_fallback_and_keeps_the_latest_snapshot() {
        use crate::anthropic::cache_metering::CacheUsage;
        use crate::kiro::model::events::MetadataEvent;

        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-7",
            100,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        ctx.context_input_tokens = Some(80);
        ctx.output_tokens = 99;
        ctx.cache_usage = CacheUsage {
            cache_read: 25,
            cache_covered_est: 50,
            prompt_total_est: 100,
        };

        let _ = ctx.process_kiro_event(&Event::Metadata(MetadataEvent {
            token_usage: Some(TokenUsage {
                uncached_input_tokens: 3,
                output_tokens: 11,
                cache_read_input_tokens: 7,
                cache_write_input_tokens: 4,
            }),
            ..Default::default()
        }));
        let _ = ctx.process_kiro_event(&Event::Metadata(MetadataEvent::default()));
        assert_eq!(ctx.resolved_usage(), (3, 4, 7));
        assert_eq!(ctx.resolved_output_tokens(), 11);

        let _ = ctx.process_kiro_event(&Event::Metadata(MetadataEvent {
            token_usage: Some(TokenUsage {
                uncached_input_tokens: -1,
                output_tokens: 22,
                cache_read_input_tokens: 23,
                cache_write_input_tokens: 24,
            }),
            ..Default::default()
        }));
        assert_eq!(ctx.resolved_usage(), (0, 24, 23));
        assert_eq!(ctx.resolved_output_tokens(), 22);
    }

    #[test]
    fn stream_usage_falls_back_to_context_cache_split_and_local_output() {
        use crate::anthropic::cache_metering::CacheUsage;

        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-7",
            100,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        ctx.context_input_tokens = Some(80);
        ctx.output_tokens = 9;
        ctx.cache_usage = CacheUsage {
            cache_read: 25,
            cache_covered_est: 50,
            prompt_total_est: 100,
        };

        // context 80 - output 9 = 71；按 cache_covered_est/prompt_total_est = 50/100
        // 分摊：cache_total = round(71 * 0.5) = 36，read = round(36 * 25/50) = 18，
        // creation = 36 - 18 = 18，uncached tail = 71 - 36 = 35。
        assert_eq!(ctx.resolved_usage(), (35, 18, 18));
        let (input, creation, read) = ctx.resolved_usage();
        assert_eq!(
            input + creation + read,
            71,
            "分摊必须发生在扣减之后，三份之和等于修正后的 total"
        );
        assert_eq!(ctx.resolved_output_tokens(), 9);
    }

    #[test]
    fn stream_usage_without_context_usage_does_not_subtract_output() {
        use crate::anthropic::cache_metering::CacheUsage;

        let make = |output_tokens: i32| {
            let mut ctx = StreamContext::new_with_thinking(
                "claude-opus-4-7",
                100,
                false,
                HashMap::new(),
                test_known_tools(),
            )
            .with_invoke_tool_schemas(test_tool_schemas());
            ctx.context_input_tokens = None;
            ctx.output_tokens = output_tokens;
            ctx.cache_usage = CacheUsage {
                cache_read: 25,
                cache_covered_est: 50,
                prompt_total_est: 100,
            };
            ctx.resolved_usage()
        };

        // 回退值来自 count_all_tokens 的 prompt 估算，本就不含输出：
        // output 取 0 还是 9，分摊结果必须完全一致。
        assert_eq!(make(9), make(0));
        let (input, creation, read) = make(9);
        assert_eq!(
            input + creation + read,
            100,
            "回退分支的 total 仍是 input_tokens 原值"
        );
    }

    #[test]
    fn live_stream_reports_final_provider_usage_without_delaying_initial_event() {
        use crate::kiro::model::events::MetadataEvent;

        let usage = TokenUsage {
            uncached_input_tokens: 3,
            output_tokens: 11,
            cache_read_input_tokens: 7,
            cache_write_input_tokens: 4,
        };
        let mut ctx = StreamContext::new_with_thinking(
            "claude-opus-4-7",
            100,
            false,
            HashMap::new(),
            test_known_tools(),
        )
        .with_invoke_tool_schemas(test_tool_schemas());
        let mut events = ctx.generate_initial_events();
        events.extend(ctx.process_kiro_event(&Event::Metadata(MetadataEvent {
            token_usage: Some(usage),
            ..Default::default()
        })));
        events.extend(ctx.generate_final_events());

        assert_eq!(ctx.resolved_usage(), (3, 4, 7));
        let start_usage = &events
            .iter()
            .find(|event| event.event == "message_start")
            .unwrap()
            .data["message"]["usage"];
        assert_eq!(start_usage["input_tokens"], json!(100));
        assert_eq!(start_usage["cache_creation_input_tokens"], json!(0));
        assert_eq!(start_usage["cache_read_input_tokens"], json!(0));
        let delta_usage = &events
            .iter()
            .find(|event| event.event == "message_delta")
            .unwrap()
            .data["usage"];
        assert_eq!(delta_usage["output_tokens"], json!(11));
    }

    // ---- fix-resp 回归：H1 / H2 / M1 / M2 / M5 / L1–L5 / L9 / L11 ----

    fn fr_ctx(thinking: bool) -> StreamContext {
        StreamContext::new_with_thinking("test-model", 1, thinking, HashMap::new(), test_known_tools())
            .with_invoke_tool_schemas(test_tool_schemas())
    }

    fn fr_run(thinking: bool, chunks: &[&str]) -> Vec<SseEvent> {
        let mut ctx = fr_ctx(thinking);
        let mut all = ctx.generate_initial_events();
        for chunk in chunks {
            all.extend(ctx.process_assistant_response(chunk));
        }
        all.extend(ctx.generate_final_events());
        all
    }

    fn fr_stop_reason(events: &[SseEvent]) -> serde_json::Value {
        events
            .iter()
            .find(|e| e.event == "message_delta")
            .map(|e| e.data["delta"]["stop_reason"].clone())
            .unwrap_or(serde_json::Value::Null)
    }

    const FR_EXEC: &str = "<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf build</parameter>\n</invoke>";

    #[test]
    fn fixresp_h2_invoke_followed_by_prose_stays_text() {
        let text = format!("例如：\n{FR_EXEC}\n上面就是调用格式。");
        let all = fr_run(false, &[&text]);
        assert!(collect_tool_uses(&all).is_empty());
        assert_eq!(collect_text_content(&all), text);
    }

    #[test]
    fn fixresp_h2_indented_code_block_invoke_stays_text() {
        let text = "示例：\n\n    <invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>";
        let all = fr_run(false, &[text]);
        assert!(collect_tool_uses(&all).is_empty());
        assert_eq!(collect_text_content(&all), text);
    }

    #[test]
    fn fixresp_h2_tilde_fence_invoke_stays_text() {
        let text = format!("示例：\n~~~xml\n{FR_EXEC}");
        let all = fr_run(false, &[&text]);
        assert!(collect_tool_uses(&all).is_empty());
        assert_eq!(collect_text_content(&all), text);
    }

    #[test]
    fn fixresp_h2_inline_code_invoke_stays_text() {
        for text in [
            format!("写成 `{}` 即可", FR_EXEC.replace('\n', "")),
            format!("用法 `\n{FR_EXEC}"),
        ] {
            let all = fr_run(false, &[&text]);
            assert!(collect_tool_uses(&all).is_empty(), "{text}");
            assert_eq!(collect_text_content(&all), text);
        }
    }

    #[test]
    fn fixresp_h2_native_tool_use_disables_text_invoke_and_keeps_order() {
        let mut ctx = fr_ctx(false);
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_assistant_response(&format!("先说明：\n{FR_EXEC}")));
        all.extend(ctx.process_kiro_event(&Event::ToolUse(tool_evt("t1", "list_apps", "{}", true))));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "{tools:?}");
        assert_eq!(tools[0].0, "list_apps");
        assert!(collect_text_content(&all).contains("rm -rf build"));
        let text_pos = all
            .iter()
            .position(|e| e.data["delta"]["type"] == "text_delta")
            .expect("text");
        let tool_pos = all
            .iter()
            .position(|e| e.data["content_block"]["type"] == "tool_use")
            .expect("tool");
        assert!(text_pos < tool_pos, "文本必须在工具调用之前");
    }

    #[test]
    fn fixresp_h2_params_restored_by_schema_types() {
        let text = "<invoke name=\"exec_command\">\n<parameter name=\"cmd\">sleep 1</parameter>\n<parameter name=\"yield_time_ms\">60000</parameter>\n</invoke>";
        let blocks = extract_invoke_content_blocks(text, &test_tool_schemas(), &HashMap::new());
        let tu = blocks.iter().find(|b| b["type"] == "tool_use").expect("应合成 tool_use");
        assert_eq!(tu["input"]["yield_time_ms"], json!(60000));
        assert_eq!(tu["input"]["cmd"], json!("sleep 1"));
    }

    #[test]
    fn fixresp_h2_schema_mismatch_stays_text() {
        let schemas = test_tool_schemas();
        let mut strict = schemas.clone();
        strict.insert(
            "exec_command".to_string(),
            json!({
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
                "required": ["cmd"],
                "additionalProperties": false
            }),
        );
        let cases: [(&str, &InvokeToolSchemas); 3] = [
            ("<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter><parameter name=\"yield_time_ms\">soon</parameter></invoke>", &schemas),
            ("<invoke name=\"exec_command\"><parameter name=\"yield_time_ms\">5</parameter></invoke>", &schemas),
            ("<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter><parameter name=\"sudo\">true</parameter></invoke>", &strict),
        ];
        for (text, table) in cases {
            let blocks = extract_invoke_content_blocks(text, table, &HashMap::new());
            assert_eq!(blocks, vec![json!({"type": "text", "text": text})], "{text}");
        }
    }

    #[test]
    fn fixresp_l3_lone_lt_in_prose_is_not_held() {
        let mut ctx = fr_ctx(false);
        let _ = ctx.generate_initial_events();
        let first = ctx.process_assistant_response("如果 a < b 就继续");
        assert_eq!(collect_text_content(&first), "如果 a < b 就继续");
    }

    #[test]
    fn fixresp_l3_line_start_unknown_invoke_is_released_immediately() {
        let mut ctx = fr_ctx(false);
        let _ = ctx.generate_initial_events();
        let chunk = "<invoke name=\"not_declared\">\n后面的正文";
        let first = ctx.process_assistant_response(chunk);
        assert_eq!(collect_text_content(&first), chunk);
    }

    #[test]
    fn fixresp_m1_literal_thinking_mid_text_stays_text() {
        let text = "可以用 <thinking> 标签包住推理，然后正文继续。";
        let all = fr_run(true, &[text]);
        assert_eq!(collect_text_content(&all), text);
        assert_eq!(collect_thinking_content(&all), "");
        assert_eq!(extract_thinking_from_complete_text(text), (None, text.to_string()));
    }

    #[test]
    fn fixresp_l1_single_newline_after_thinking_end() {
        let text = "<thinking>想一想</thinking>\n答案在这里";
        let all = fr_run(true, &[text]);
        assert_eq!(collect_thinking_content(&all), "想一想");
        assert_eq!(collect_text_content(&all), "答案在这里");
        let (thinking, rest) = extract_thinking_from_complete_text(text);
        assert_eq!(thinking.as_deref().map(str::trim), Some("想一想"));
        assert_eq!(rest, "答案在这里");
    }

    #[test]
    fn fixresp_m2_thinking_then_noarg_tool_reports_tool_use() {
        let mut ctx = fr_ctx(true);
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_assistant_response("<thinking>需要列出应用</thinking>"));
        all.extend(ctx.process_kiro_event(&Event::ToolUse(tool_evt("t1", "list_apps", "", false))));
        all.extend(ctx.generate_final_events());
        assert_eq!(fr_stop_reason(&all), json!("tool_use"));
        assert_eq!(collect_tool_uses(&all).len(), 1);
    }

    #[test]
    fn fixresp_m2_native_max_tokens_reports_max_tokens() {
        let mut ctx = fr_ctx(true);
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_assistant_response("<thinking>想到一半"));
        all.extend(ctx.process_kiro_event(&Event::Metadata(
            crate::kiro::model::events::MetadataEvent {
                stop_reason: Some("MAX_TOKENS".to_string()),
                ..Default::default()
            },
        )));
        all.extend(ctx.generate_final_events());
        assert_eq!(fr_stop_reason(&all), json!("max_tokens"));
    }

    #[test]
    fn fixresp_m5_native_end_turn_beats_full_context_usage() {
        let run = |native: Option<&str>| {
            let mut ctx = fr_ctx(false);
            let mut all = ctx.generate_initial_events();
            all.extend(ctx.process_assistant_response("完整的回答。"));
            all.extend(ctx.process_kiro_event(&Event::ContextUsage(
                crate::kiro::model::events::ContextUsageEvent {
                    context_usage_percentage: 100.0,
                },
            )));
            if let Some(reason) = native {
                all.extend(ctx.process_kiro_event(&Event::Metadata(
                    crate::kiro::model::events::MetadataEvent {
                        stop_reason: Some(reason.to_string()),
                        ..Default::default()
                    },
                )));
            }
            all.extend(ctx.generate_final_events());
            fr_stop_reason(&all)
        };
        assert_eq!(run(Some("END_TURN")), json!("end_turn"));
        assert_eq!(run(None), json!("model_context_window_exceeded"));
    }

    #[test]
    fn fixresp_l5_repeat_guard_ends_with_visible_error() {
        let mut payload = String::from("先看状态。\n\n");
        for _ in 0..200 {
            payload.push_str("count\n");
        }
        let all = fr_run(false, &[&payload]);
        let err = all.iter().find(|e| e.event == "error").expect("熔断必须以 error 事件结束");
        assert!(err.data.to_string().contains("api_error"), "{:?}", err.data);
        assert!(!all.iter().any(|e| e.event == "message_stop"));
        assert!(stray_token_flood_detected(&payload));
        assert!(!stray_token_flood_detected("先看状态。\ncount\n正文"));
    }

    #[test]
    fn fixresp_l9_reasoning_not_mixed_into_text_when_thinking_disabled() {
        let reasoning = |t: &str| {
            Event::ReasoningContent(crate::kiro::model::events::ReasoningContentEvent {
                text: Some(t.to_string()),
                signature: None,
                redacted_content: None,
            })
        };
        let mut ctx = fr_ctx(false);
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_kiro_event(&reasoning("内部推理")));
        all.extend(ctx.process_assistant_response("最终回答"));
        all.extend(ctx.generate_final_events());
        assert_eq!(collect_text_content(&all), "最终回答");

        let mut ctx = fr_ctx(false);
        let mut all = ctx.generate_initial_events();
        all.extend(ctx.process_kiro_event(&reasoning("只有推理")));
        all.extend(ctx.generate_final_events());
        assert_eq!(collect_text_content(&all), "只有推理");
    }

    #[test]
    fn fixresp_h1_unclosed_tool_use_does_not_swallow_reply() {
        let all = fr_run(false, &["格式是 <tool_use id=\"x\" name=\"Read\">", " 然后继续回答。"]);
        assert_eq!(
            collect_text_content(&all),
            "格式是 <tool_use id=\"x\" name=\"Read\"> 然后继续回答。"
        );
    }

    #[test]
    fn fixresp_l11_strip_keeps_surrounding_whitespace() {
        let content = "  缩进开头\n<tool_use id=\"t\" name=\"Read\">{}</tool_use>\n";
        assert_eq!(
            crate::kiro::model::events::strip_tool_use_xml_leaks(content),
            "  缩进开头\n\n"
        );
    }
}
