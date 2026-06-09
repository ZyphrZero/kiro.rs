//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::collections::HashMap;

use serde_json::json;
use uuid::Uuid;

use crate::kiro::model::events::Event;

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

        // 真正的 thinking 结束标签后面会有双换行符 `\n\n`
        if after_content.starts_with("\n\n") {
            return Some(absolute_pos);
        }

        // 不是双换行符，跳过继续搜索
        search_start = absolute_pos + 1;
    }

    None
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

/// 解析一个完整 invoke 块，抠出 (tool_name, input_json_string)
///
/// - tool name 来自 invoke 开标签的 `name="..."`（兼容 antml: 前缀）
/// - 参数为零个或多个 `<parameter name="K">V</parameter>`（兼容前缀）
/// - 参数值取到下一个参数开标签前的**最后一个** `</parameter>` 为界（贪婪），
///   允许多行 / 含 `<` / 中文 / 含字面 `</parameter>`（P0-1 修复）
/// - 用 serde_json 拼成 object（值都是字符串，自动转义）
/// - 无合法 name 或拼不出合法 JSON 返回 `None`
fn parse_invoke_block(block: &str) -> Option<(String, String)> {
    // invoke 开标签 = 块开头到第一个 '>'
    let open_end = block.find('>')?;
    let open_tag = &block[..=open_end];
    let tool_name = extract_name_attr(open_tag)?;
    if tool_name.is_empty() {
        return None;
    }

    let mut map = serde_json::Map::new();
    let body = &block[open_end + 1..];
    let mut cursor = 0usize;
    while let Some(rel) = body[cursor..].find("parameter name=\"") {
        let name_kw = cursor + rel;
        // 确认是真正的 '<parameter' 或 '<prefix:parameter' 开标签
        // name_kw 指向 'parameter'，往前应是 '<' 或 '<prefix:'
        // 确认是真正的开标签（'<parameter' / '<prefix:parameter'）；仅用于校验，不需要位置值
        if open_tag_lt_pos(body, name_kw).is_none() {
            cursor = name_kw + "parameter".len();
            continue;
        }
        // 找该参数开标签的 '>'
        let tag_gt = match body[name_kw..].find('>') {
            Some(r) => name_kw + r,
            None => break, // 开标签未闭合，停止
        };
        let param_open_tag = &body[name_kw..tag_gt + 1];
        // 从 'parameter name="..."' 抠 key（剥掉前缀干扰：直接找 name="）
        let key = match extract_name_attr(param_open_tag) {
            Some(k) => k,
            None => {
                cursor = tag_gt + 1;
                continue;
            }
        };
        // 参数值取到 </parameter>（兼容前缀）为界。find_param_close 较贵，只调一次，
        // 同时复用 (闭标签起始, 闭标签结束) 两个值：起始用于切值，结束用于推进游标。
        let val_start = tag_gt + 1;
        let (close_start, close_end) = match find_param_close(body, val_start) {
            Some(pair) => pair,
            None => break, // 值未闭合，停止
        };
        let value = &body[val_start..close_start];
        map.insert(key, serde_json::Value::String(value.to_string()));
        // 推进到闭标签之后
        cursor = close_end;
    }

    let obj = serde_json::Value::Object(map);
    let s = serde_json::to_string(&obj).ok()?;
    Some((tool_name, s))
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

/// 判定一个 `<invoke>` 块到底像“真泄漏的工具调用”还是“正文里讨论的文本”
///
/// 实测真泄漏的 `<invoke>` 都出现在**行首**（前面是流的开头、或上一行已经换行结束），
/// 而正文讨论里的 `<invoke>` 一般**嵌在一句话中间**——前面同一行还有普通文字。
///
/// 判定规则（输入 `before` 是 `<invoke>` 之前、已剥过 stray token 的文本）：
/// - `before` 为空（`<invoke>` 在流开头）→ 像真泄漏，抓。
/// - `before` 去掉尾部空格/制表符后以换行结尾（`<invoke>` 独占新行）→ 抓。
/// - 否则（同一行前面还有非空白正文）→ 像讨论文本，不抓。
///
/// 注意：这里的“尾部空白”只剥行内空白（空格 / 制表符），不剥换行；
/// 换行结尾才是“另起一行”的信号。
fn invoke_looks_like_real_leak(before: &str) -> bool {
    // 剥掉尾部的行内空白（空格 / 制表符），但保留换行
    let trimmed = before.trim_end_matches([' ', '\t']);
    // 行首：要么前面什么都没有，要么上一行已经以换行结束
    trimmed.is_empty() || trimmed.ends_with('\n') || trimmed.ends_with('\r')
}

/// 推进「代码围栏」奇偶状态，对切分到多个 chunk 的 ``` 分隔符鲁棒。
///
/// 只在遇到换行符时才对「已重组的完整行」判定是否为围栏行（行首去空白后以 ``` 开头）。
/// 未遇换行的尾部留在 `partial` 里，等后续 chunk 拼齐——所以即使 ``` 被切成
/// `` `` `` + `` ` `` 两个 chunk，重组成完整行后仍能正确翻转 `open`。
///
/// 返回值仅在内部使用；主要副作用是更新 `open` 与 `partial`。
fn advance_code_fence_state(open: &mut bool, partial: &mut String, text: &str) {
    for ch in text.chars() {
        if ch == '\n' {
            if partial.trim_start().starts_with("```") {
                *open = !*open;
            }
            partial.clear();
        } else {
            partial.push(ch);
        }
    }
}

/// 纯函数：在不改动真实状态的前提下，试算「把 `text` 走完之后围栏是否打开」。
/// 用于 drain 决策处判断某个 `<invoke>` 是否落在围栏内。
fn fence_open_after(open: bool, partial: &str, text: &str) -> bool {
    let mut o = open;
    let mut p = partial.to_string();
    advance_code_fence_state(&mut o, &mut p, text);
    // 还要考虑：partial 里残留的「未换行行」如果本身已经是 ``` 开头，
    // 它在遇到换行前不算翻转（保守：只有完整行才翻转）。这里返回已翻转的 o。
    o
}

/// 计算缓冲区末尾“可能是部分 `<invoke` 开标签前缀”的字节数，需要保留等待更多内容
///
/// 例如缓冲区以 `<inv` / `<` / `<i` 结尾时，可能是被切碎的 invoke 开标签，
/// 保留这段尾巴等下一个 chunk 拼齐，避免把半个标签当文本吐出去。
fn partial_invoke_tag_suffix_len(buf: &str) -> usize {
    // 任何形如 `<...`（最后一个 '<' 之后没有 '>'）的尾巴都可能是部分开标签
    if let Some(lt) = buf.rfind('<') {
        if !buf[lt..].contains('>') {
            return buf.len() - lt;
        }
    }
    0
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
    let start_pos = match find_real_thinking_start_tag(text) {
        Some(pos) => pos,
        None => return (None, text.to_string()),
    };

    let before = &text[..start_pos];
    let after_open = &text[start_pos + "<thinking>".len()..];

    // 查找结束标签：优先匹配带 \n\n 后缀的，退而使用末尾匹配
    let (thinking_raw, text_after) = if let Some(end_pos) = find_real_thinking_end_tag(after_open) {
        (
            &after_open[..end_pos],
            &after_open[end_pos + "</thinking>\n\n".len()..],
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

/// 一次性（非流式 / 整段已完整）把 assistant 文本切成 Anthropic content block 序列，
/// 把混在文本里的字面 `<invoke name="...">...</invoke>` 工具调用捞回成结构化 `tool_use`。
///
/// 复用与流式 `drain_invoke_sniff_buffer` **完全相同**的安全判定，避免误抓正文里讨论的命令：
///   ① 行首判定 `invoke_looks_like_real_leak`（块前去 stray token 后须在行首）
///   ② 代码围栏判定 `fence_open_after`（被 ``` 包裹的展示文本不捞）
///   ③ 工具表硬护栏 `known_tool_names`（解析出的工具名必须是本次请求声明的工具）
/// 任一不满足 → 该 `<invoke>` 块当普通文本原样保留。
///
/// 与流式版本的区别：这里输入是**已完整**的整段文本，所以不需要 hold 缓冲、
/// 部分开标签、`MAX_INVOKE_HOLD_BYTES` 那套增量逻辑——直接线性扫描即可。
///
/// 返回的 content block 形态与调用方现有约定一致：
///   - 文本：`{"type":"text","text": "..."}`
///   - 工具：`{"type":"tool_use","id":"toolu_...","name":"...","input": {...}}`
/// 文本块按需合并相邻片段；空文本片段不产出。`input` 解析失败时 fall back 成 `{}`。
///
/// `tool_name_map`（短名 → 原始名）用于把捞回的工具名还原成客户端可识别的原始名，
/// 与流式 `synthesize_tool_use` 一致；映射为空或命中失败时按原名返回。
pub(crate) fn extract_invoke_content_blocks(
    text: &str,
    known_tool_names: &std::collections::HashSet<String>,
    tool_name_map: &std::collections::HashMap<String, String>,
) -> Vec<serde_json::Value> {
    // 🛑 块级复读熔断：先把 Opus 退化的「同一 stray token 连续复读」截断，
    // 再做 invoke 嗅探。覆盖 web_search loop（99.9% 真实流量）这条非流式路径。
    let collapsed = collapse_stray_token_floods(text);
    let text: &str = &collapsed;
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    let mut pending_text = String::new();
    // 围栏奇偶状态：跨「已吐出的文本」累进，确保 ``` 跨片段也能正确判定。
    let mut fence_open = false;
    let mut fence_partial = String::new();

    let push_text = |blocks: &mut Vec<serde_json::Value>, pending: &mut String| {
        if !pending.is_empty() {
            blocks.push(serde_json::json!({"type": "text", "text": pending.clone()}));
            pending.clear();
        }
    };

    let mut rest = text;
    loop {
        let start = match find_invoke_start(rest) {
            Some(s) => s,
            None => {
                pending_text.push_str(rest);
                break;
            }
        };
        let end = match find_invoke_block_end(rest, start) {
            Some(e) => e,
            None => {
                // 块没闭合（整段已完整仍未见 </invoke>）→ 不是干净的工具调用，整段当文本。
                pending_text.push_str(rest);
                break;
            }
        };

        let before = &rest[..start];
        let stripped_before = strip_trailing_stray_tokens(before);
        // ③ 围栏：在「块之前的文本」走完后围栏是否打开
        let fence_after_before = fence_open_after(fence_open, &fence_partial, before);
        // ② 工具名解析 + 工具表护栏
        let parsed = parse_invoke_block(&rest[start..end]);
        let name_known = parsed
            .as_ref()
            .map(|(n, _)| known_tool_names.contains(n))
            .unwrap_or(false);

        if invoke_looks_like_real_leak(stripped_before) && !fence_after_before && name_known {
            // 真泄漏：保留剥过 stray token 的前文（推进围栏），再产出结构化 tool_use。
            if !stripped_before.is_empty() {
                advance_code_fence_state(&mut fence_open, &mut fence_partial, stripped_before);
                pending_text.push_str(stripped_before);
            }
            push_text(&mut blocks, &mut pending_text);
            let (name, input_json) = parsed.expect("parsed is Some when name_known");
            let input: serde_json::Value =
                serde_json::from_str(&input_json).unwrap_or_else(|_| serde_json::json!({}));
            // Restore the original (client-facing) tool name: long names (>63) are shortened
            // before being sent upstream, so the model may leak the SHORT name. The host
            // matches on the original name — mirror synthesize_tool_use's restoration.
            let name = tool_name_map.get(&name).cloned().unwrap_or(name);
            let tool_use_id = format!("toolu_{}", Uuid::new_v4().to_string().replace('-', ""));
            blocks.push(serde_json::json!({
                "type": "tool_use",
                "id": tool_use_id,
                "name": name,
                "input": input,
            }));
        } else {
            // 不捞（句中 / 围栏内 / 工具名未知 / 解析失败）→ 整块（含 before）当文本，推进围栏。
            let chunk = &rest[..end];
            advance_code_fence_state(&mut fence_open, &mut fence_partial, chunk);
            pending_text.push_str(chunk);
        }
        rest = &rest[end..];
    }

    push_text(&mut blocks, &mut pending_text);
    blocks
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

    /// 设置 stop_reason
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        self.stop_reason = Some(reason.into());
    }

    /// 检查是否存在非 thinking 类型的内容块（如 text 或 tool_use）
    fn has_non_thinking_blocks(&self) -> bool {
        self.active_blocks
            .values()
            .any(|b| b.block_type != "thinking")
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
            events.push(SseEvent::new(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": self.get_stop_reason(),
                        "stop_sequence": null
                    },
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                        "cache_creation_input_tokens": cache_creation_input_tokens,
                        "cache_read_input_tokens": cache_read_input_tokens
                    }
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
    /// 跨整条流的「代码围栏」奇偶状态：每遇到一行以 ``` 开头就翻转。
    /// 在围栏内（true）时，`<invoke>` 一律不捞回（视为正文展示的代码块）。
    pub code_fence_open: bool,
    /// 围栏检测的「未完成行」累加器：只在遇到换行时才对完整行判定是否为 ``` 围栏行。
    /// 这样即使 ``` 分隔符被切分到多个 chunk（如 `` `` + ` ``），重组成完整行后仍能正确识别。
    pub fence_scan_partial: String,
    /// thinking 是否启用
    pub thinking_enabled: bool,
    /// thinking 内容缓冲区
    pub thinking_buffer: String,
    /// invoke 文本嗅探缓冲区（用于从明文流里嗅探字面 `<invoke>` 工具调用块）
    pub invoke_sniff_buffer: String,
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
    /// meteringEvent 上报的 credit 计费量（上游真实下发）
    pub credits: f64,
    /// 复读熔断：最近一次作为文本吐出的「尾行」内容（去空白）。
    /// Opus 长上下文退化时会把同一个 stray token（call/count/card）一行一行无限复读，
    /// 我们在文本出口处统计「同一短行连续重复了多少次」。
    repeat_guard_last_line: String,
    /// 复读熔断：当前尾行已连续重复的次数。
    repeat_guard_run: u32,
    /// 复读熔断：是否已经触发过熔断（触发后本轮后续文本一律丢弃，不再吐、不写历史）。
    repeat_guard_tripped: bool,
}

impl StreamContext {
    /// 解析最终上报口径的 `(input_tokens, cache_creation, cache_read)`。
    ///
    /// total 真值优先取 contextUsage（上游真实百分比×窗口），否则用客户端估算的
    /// `input_tokens`；再由 [`CacheUsage::split_against_total`] 做互斥分摊。
    pub fn resolved_usage(&self) -> (i32, i32, i32) {
        let total_real = self.context_input_tokens.unwrap_or(self.input_tokens);
        self.cache_usage.split_against_total(total_real)
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
            output_tokens: 0,
            tool_block_indices: HashMap::new(),
            tool_name_map,
            known_tool_names,
            code_fence_open: false,
            fence_scan_partial: String::new(),
            thinking_enabled,
            thinking_buffer: String::new(),
            invoke_sniff_buffer: String::new(),
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            pending_thinking_signature: None,
            text_block_index: None,
            strip_thinking_leading_newline: false,
            cache_usage: super::cache_metering::CacheUsage::default(),
            credits: 0.0,
            repeat_guard_last_line: String::new(),
            repeat_guard_run: 0,
            repeat_guard_tripped: false,
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
            Event::ContextUsage(context_usage) => {
                // 从上下文使用百分比计算实际的 input_tokens
                let window_size = get_context_window_size(&self.model);
                let actual_input_tokens =
                    (context_usage.context_usage_percentage * (window_size as f64) / 100.0) as i32;
                self.context_input_tokens = Some(actual_input_tokens);
                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                if context_usage.context_usage_percentage >= 100.0 {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
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
                tracing::debug!("metering credits +{:.6}", metering.usage);
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
        if content.is_empty() {
            return Vec::new();
        }

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
                // 查找 <thinking> 开始标签（跳过被反引号包裹的）
                if let Some(start_pos) = find_real_thinking_start_tag(&self.thinking_buffer) {
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
                        self.thinking_buffer[end_pos + "</thinking>\n\n".len()..].to_string();
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

    /// 创建 text_delta 事件（带 invoke 嗅探的统一明文漏斗）
    ///
    /// 这是 thinking / 非 thinking 两条路径 + 两个端点唯一共用的明文出口。
    /// 在这里把文本累进 `invoke_sniff_buffer`，循环嗅探完整的字面 `<invoke>` 工具调用块：
    /// - 命中完整块：先把块前文本（剥掉尾部独立的 `call`/`count` 行）走 `emit_text_delta_raw` 吐出，
    ///   再合成结构化 tool_use 事件，再继续循环；
    /// - 未命中完整块：保留可能的部分标签尾巴留在缓冲区，其余走 `emit_text_delta_raw`。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        if text.is_empty() {
            return Vec::new();
        }
        self.invoke_sniff_buffer.push_str(text);
        self.drain_invoke_sniff_buffer(false)
    }

    /// 行首未闭合 `<invoke` 块的字节上限。仅用于防止"行首一个永不闭合的 `<invoke`
    /// 把整条流永久 hold 住"这种极端情况；正常的 invoke（哪怕是大 patch）都远小于此，
    /// 所以不会误杀合法的多行/分片工具调用。
    const MAX_INVOKE_HOLD_BYTES: usize = 262_144;

    /// 嗅探并排空 `invoke_sniff_buffer`
    ///
    /// - `flush=false`（流式中途）：未命中完整块时，保留可能是部分标签的尾巴（最长一个未闭合
    ///   `<invoke` 块或一段疑似开标签前缀），其余前缀文本走 `emit_text_delta_raw` 吐出。
    /// - `flush=true`（流末尾）：不再保留尾巴，剩余全部走 `emit_text_delta_raw` 吐出（防尾字节丢）。
    fn drain_invoke_sniff_buffer(&mut self, flush: bool) -> Vec<SseEvent> {
        let mut events = Vec::new();
        // Drive the loop on an owned local buffer taken out of `self` ONCE, instead of
        // cloning `self.invoke_sniff_buffer` on every iteration. Under degraded-model
        // floods this buffer can grow up to MAX_INVOKE_HOLD_BYTES, so a per-iteration
        // full clone was O(n) per loop (quadratic overall). The only in-loop allocation
        // now is the (smaller) remainder after a reclaimed block. Every exit path writes
        // the intended remainder back into `self.invoke_sniff_buffer` (empty if fully
        // consumed); the Some->Some path keeps looping on the local `buf`.
        let mut buf = std::mem::take(&mut self.invoke_sniff_buffer);
        loop {
            match find_invoke_start(&buf) {
                Some(start) => {
                    match find_invoke_block_end(&buf, start) {
                        Some(end) => {
                            // 命中完整块：先判定它像真泄漏还是正文讨论（P1 歧义信号）
                            let before = strip_trailing_stray_tokens(&buf[..start]);
                            // 🅱 先把 before 里的围栏开合并进一个「试算」状态：如果这个 <invoke>
                            // 落在代码围栏内（正文展示的代码块），一律不捞回，当文本吐出。
                            let fence_after_before = fence_open_after(
                                self.code_fence_open,
                                &self.fence_scan_partial,
                                before,
                            );
                            // 🅳 灾难兜底：只有解析出的工具名在本次请求声明的工具表里，才允许捞回。
                            // 表为空（请求没带 tools）或名字不在表里 → 当文本吐，宁可漏捞不可误执行。
                            let parsed = parse_invoke_block(&buf[start..end]);
                            let name_known = parsed
                                .as_ref()
                                .map(|(n, _)| self.known_tool_names.contains(n))
                                .unwrap_or(false);
                            if invoke_looks_like_real_leak(before) && !fence_after_before && name_known {
                                // 真泄漏：吐块前文本（剥掉尾部独立的 call/count 行）+ 合成 tool_use
                                if !before.is_empty() {
                                    events.extend(self.emit_text_delta_raw(before));
                                }
                                // parsed 在上面已确认是 Some 且 name_known
                                let (name, input_json) = parsed.expect("parsed is Some when name_known");
                                events.extend(self.synthesize_tool_use(name, input_json));
                            } else {
                                // 不捞回（嵌句中 / 围栏内 / 工具名未知 / 解析失败）→ 整段当普通文本吐出
                                events.extend(self.emit_text_delta_raw(&buf[..end]));
                            }
                            // 推进本地缓冲区到块之后，继续循环（不再回写 self、不再整体 clone）
                            buf = buf[end..].to_string();
                            continue;
                        }
                        None => {
                            // 块还没到齐。先用 P1 行首判定：不在行首的 <invoke 当讨论文本，
                            // 直接整段吐出，不进 hold 缓冲（P2：避免 hold 住后续文本到流末尾）。
                            let before = strip_trailing_stray_tokens(&buf[..start]);
                            // 🅱 围栏内的未闭合 <invoke> 也不 hold（是正文代码块），直接当文本吐。
                            let fence_after_before = fence_open_after(
                                self.code_fence_open,
                                &self.fence_scan_partial,
                                before,
                            );
                            if !invoke_looks_like_real_leak(before) || fence_after_before {
                                if !buf.is_empty() {
                                    events.extend(self.emit_text_delta_raw(&buf));
                                }
                                break;
                            }
                            // 行首的未闭合块：把 start 之前的文本吐出，保留 start.. 等闭合
                            if start > 0 {
                                events.extend(self.emit_text_delta_raw(&buf[..start]));
                            }
                            let remainder = buf[start..].to_string();
                            if flush {
                                // flush 模式：残留半块当普通文本吐出
                                if !remainder.is_empty() {
                                    events.extend(self.emit_text_delta_raw(&remainder));
                                }
                            } else {
                                // P2 上限：hold 的 <invoke 块累计超过阈值仍没等到 </invoke>，
                                // 放弃等待，当普通文本吐出，避免无限期 hold 后续文本。
                                // 仅用纯字节上限兜底"永不闭合的 `<invoke` 把流卡死"；
                                // 不再按换行数放弃——多行参数（apply_patch 等）是常态，
                                // 换行数不是放弃 hold 的好信号，否则会误杀分片到达的合法 invoke。
                                let too_long = remainder.len() > Self::MAX_INVOKE_HOLD_BYTES;
                                if too_long {
                                    events.extend(self.emit_text_delta_raw(&remainder));
                                } else {
                                    // 保留半块到 self，等下一片到达再续
                                    self.invoke_sniff_buffer = remainder;
                                }
                            }
                            break;
                        }
                    }
                }
                None => {
                    // 没有任何 invoke 开标签
                    if flush {
                        if !buf.is_empty() {
                            events.extend(self.emit_text_delta_raw(&buf));
                        }
                    } else {
                        // 保留一段可能是部分 `<invoke` 开标签前缀的尾巴，其余吐出
                        let keep = partial_invoke_tag_suffix_len(&buf);
                        let split = buf.len() - keep;
                        let safe = find_char_boundary(&buf, split);
                        if safe > 0 {
                            events.extend(self.emit_text_delta_raw(&buf[..safe]));
                        }
                        self.invoke_sniff_buffer = buf[safe..].to_string();
                    }
                    break;
                }
            }
        }
        events
    }

    /// 合成一组结构化 tool_use 事件（照抄 process_tool_use 的 6 步）
    fn synthesize_tool_use(&mut self, parsed_name: String, input_json: String) -> Vec<SseEvent> {
        let mut events = Vec::new();
        self.state_manager.set_has_tool_use(true);
        let block_index = self.state_manager.next_block_index();
        let tool_use_id = format!("toolu_{}", Uuid::new_v4().to_string().replace('-', ""));
        self.tool_block_indices
            .insert(tool_use_id.clone(), block_index);
        let name = self
            .tool_name_map
            .get(&parsed_name)
            .cloned()
            .unwrap_or(parsed_name);
        events.extend(self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": tool_use_id,
                    "name": name,
                    "input": {}
                }
            }),
        ));
        if let Some(d) = self.state_manager.handle_content_block_delta(
            block_index,
            json!({
                "type": "content_block_delta",
                "index": block_index,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": input_json
                }
            }),
        ) {
            events.push(d);
        }
        if let Some(s) = self.state_manager.handle_content_block_stop(block_index) {
            events.push(s);
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

        // 🅱 维护跨流的代码围栏奇偶状态：所有真正作为「文本」吐出的内容都过这里，
        // 在此累进围栏状态，使后续 <invoke> 能判断自己是否落在代码块内。
        let mut fence_open = self.code_fence_open;
        let mut fence_partial = std::mem::take(&mut self.fence_scan_partial);
        advance_code_fence_state(&mut fence_open, &mut fence_partial, text);
        self.code_fence_open = fence_open;
        self.fence_scan_partial = fence_partial;

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
                return self.create_text_delta_events(text);
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

        // 获取或分配块索引
        let block_index = if let Some(&idx) = self.tool_block_indices.get(&tool_use.tool_use_id) {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.tool_block_indices
                .insert(tool_use.tool_use_id.clone(), idx);
            idx
        };

        // 还原工具名称（如果有映射）
        let original_name = self
            .tool_name_map
            .get(&tool_use.name)
            .cloned()
            .unwrap_or_else(|| tool_use.name.clone());

        // 发送 content_block_start
        let start_events = self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": tool_use.tool_use_id,
                    "name": original_name,
                    "input": {}
                }
            }),
        );
        events.extend(start_events);

        // 发送参数增量 (ToolUseEvent.input 是 String 类型)
        if !tool_use.input.is_empty() {
            self.output_tokens += (tool_use.input.len() as i32 + 3) / 4; // 估算 token

            if let Some(delta_event) = self.state_manager.handle_content_block_delta(
                block_index,
                json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": tool_use.input
                    }
                }),
            ) {
                events.push(delta_event);
            }
        }

        // 如果是完整的工具调用（stop=true），发送 content_block_stop
        if tool_use.stop {
            if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
                events.push(stop_event);
            }
        }

        events
    }

    /// 生成最终事件序列
    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

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

        // 如果整个流中只产生了 thinking 块，没有 text 也没有 tool_use，
        // 则设置 stop_reason 为 max_tokens（表示模型耗尽了 token 预算在思考上），
        // 并补发一套完整的 text 事件（内容为一个空格），确保 content 数组中有 text 块
        if self.thinking_enabled
            && self.thinking_block_index.is_some()
            && !self.state_manager.has_non_thinking_blocks()
        {
            self.state_manager.set_stop_reason("max_tokens");
            events.extend(self.create_text_delta_events(" "));
        }

        // Flush invoke 嗅探缓冲区的残留：先再嗅探一次完整块（万一最后一块就是完整 invoke），
        // 剩下的走 emit_text_delta_raw flush 出去（防尾字节丢）。
        if !self.invoke_sniff_buffer.is_empty() {
            events.extend(self.drain_invoke_sniff_buffer(true));
        }

        // 互斥口径：total 真值（contextUsage 优先）− 缓存覆盖 = 未缓存的 input。
        let (final_input_tokens, cache_creation, cache_read) = self.resolved_usage();

        // 生成最终事件
        events.extend(self.state_manager.generate_final_events(
            final_input_tokens,
            self.output_tokens,
            cache_creation,
            cache_read,
        ));
        events
    }
}

/// 缓冲流处理上下文 - 用于 /cc/v1/messages 流式请求
///
/// 与 `StreamContext` 不同，此上下文会缓冲所有事件直到流结束，
/// 然后用从 `contextUsageEvent` 计算的正确 `input_tokens` 更正 `message_start` 事件。
///
/// 工作流程：
/// 1. 使用 `StreamContext` 正常处理所有 Kiro 事件
/// 2. 把生成的 SSE 事件缓存起来（而不是立即发送）
/// 3. 流结束时，找到 `message_start` 事件并更新其 `input_tokens`
/// 4. 一次性返回所有事件
pub struct BufferedStreamContext {
    /// 内部流处理上下文（复用现有的事件处理逻辑）
    inner: StreamContext,
    /// 缓冲的所有事件（包括 message_start、content_block_start 等）
    event_buffer: Vec<SseEvent>,
    /// 是否已经生成了初始事件
    initial_events_generated: bool,
}

impl BufferedStreamContext {
    /// 创建缓冲流上下文
    pub fn new(
        model: impl Into<String>,
        estimated_input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
        known_tool_names: std::collections::HashSet<String>,
    ) -> Self {
        let inner = StreamContext::new_with_thinking(
            model,
            estimated_input_tokens,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
        );
        Self {
            inner,
            event_buffer: Vec::new(),
            initial_events_generated: false,
        }
    }

    /// 注入由 CacheMeter 计算的缓存覆盖情况（estimate 口径），最终上报时分摊。
    pub fn set_cache_usage(&mut self, cache_usage: super::cache_metering::CacheUsage) {
        self.inner.cache_usage = cache_usage;
    }

    /// 处理 Kiro 事件并缓冲结果
    ///
    /// 复用 StreamContext 的事件处理逻辑，但把结果缓存而不是立即发送。
    pub fn process_and_buffer(&mut self, event: &crate::kiro::model::events::Event) {
        // 首次处理事件时，先生成初始事件（message_start 等）
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 处理事件并缓冲结果
        let events = self.inner.process_kiro_event(event);
        self.event_buffer.extend(events);
    }

    /// 完成流处理并返回所有事件
    ///
    /// 此方法会：
    /// 1. 生成最终事件（message_delta, message_stop）
    /// 2. 用正确的 input_tokens 更正 message_start 事件
    /// 3. 返回所有缓冲的事件
    pub fn finish_and_get_all_events(&mut self) -> Vec<SseEvent> {
        // 如果从未处理过事件，也要生成初始事件
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 互斥口径分摊：total 真值 − 缓存覆盖 = 未缓存 input（与 inner 收尾一致）。
        let (final_input_tokens, cache_creation, cache_read) = self.inner.resolved_usage();

        // 生成最终事件（StreamContext 内部会用同样的优先级与分摊）
        let final_events = self.inner.generate_final_events();
        self.event_buffer.extend(final_events);

        // 更正 message_start 事件中的 input_tokens 与 cache_* 字段
        for event in &mut self.event_buffer {
            if event.event == "message_start" {
                if let Some(message) = event.data.get_mut("message") {
                    if let Some(usage) = message.get_mut("usage") {
                        usage["input_tokens"] = serde_json::json!(final_input_tokens);
                        usage["cache_creation_input_tokens"] = serde_json::json!(cache_creation);
                        usage["cache_read_input_tokens"] = serde_json::json!(cache_read);
                    }
                }
            }
        }

        std::mem::take(&mut self.event_buffer)
    }

    /// 取出最终用量（在 finish_and_get_all_events 之后调用）
    ///
    /// 返回顺序：(input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, credits)
    pub fn final_usage(&self) -> (i32, i32, i32, i32, f64) {
        let (input, creation, read) = self.inner.resolved_usage();
        (
            input,
            self.inner.output_tokens,
            creation,
            read,
            self.inner.credits,
        )
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

    /// 测试用的「已知工具表」：包含 invoke 测试里会合成的工具名，
    /// 让 🅳 工具表校验放行这些名字，从而能验证捞回逻辑本身。
    fn test_known_tools() -> std::collections::HashSet<String> {
        ["exec_command", "apply_patch", "tool_a", "tool_b", "write_file", "wait_agent"]
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
            &test_known_tools(),
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
                && b["text"].as_str().map(|t| t.contains("<invoke")).unwrap_or(false)),
            "no literal <invoke> may remain as text"
        );
        assert!(
            !blocks.iter().any(|b| b["type"] == "text" && b["text"] == "call\n"),
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
        let mut known = std::collections::HashSet::new();
        known.insert(short.to_string());
        let mut map = std::collections::HashMap::new();
        map.insert(short.to_string(), original.to_string());
        let blocks = extract_invoke_content_blocks(&text, &known, &map);
        let tu = blocks.iter().find(|b| b["type"] == "tool_use").expect("reclaimed");
        assert_eq!(tu["name"], original, "shortened name must be restored to original");
    }

    #[test]
    fn extract_blocks_does_not_reclaim_fenced_or_unknown() {
        // fenced -> display, not reclaimed
        let fenced = "see:\n```\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">rm -rf /</parameter>\n</invoke>\n```";
        let b1 = extract_invoke_content_blocks(fenced, &test_known_tools(), &std::collections::HashMap::new());
        assert!(!b1.iter().any(|b| b["type"] == "tool_use"), "fenced must not reclaim");
        // unknown tool name -> not reclaimed
        let unknown = "call\n<invoke name=\"not_a_real_tool\">\n<parameter name=\"x\">y</parameter>\n</invoke>";
        let b2 = extract_invoke_content_blocks(unknown, &test_known_tools(), &std::collections::HashMap::new());
        assert!(!b2.iter().any(|b| b["type"] == "tool_use"), "unknown name must not reclaim");
    }

    #[test]
    fn extract_blocks_clean_text_is_single_unchanged_text_block() {
        let blocks = extract_invoke_content_blocks(
            "just a normal answer with no tool calls",
            &test_known_tools(),
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

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map, test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());

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
            stop: false,
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
        // thinking 模式下，短文本可能被暂存在 thinking_buffer 以等待 `<thinking>` 的跨 chunk 匹配。
        // 当紧接着出现 tool_use 时，应先 flush 这段文本，再开始 tool_use block。
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
        let _initial_events = ctx.generate_initial_events();

        // 两段短文本（各 2 个中文字符），总长度仍可能不足以满足 safe_len>0 的输出条件，
        // 因而会留在 thinking_buffer 中等待后续 chunk。
        let ev1 = ctx.process_assistant_response("有修");
        assert!(
            ev1.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should be buffered under thinking mode"
        );
        let ev2 = ctx.process_assistant_response("改：");
        assert!(
            ev2.iter().all(|e| e.event != "content_block_delta"),
            "short prefix should still be buffered under thinking mode"
        );

        let events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });

        let text_start_index = events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        let pos_text_delta = events.iter().position(|e| {
            e.event == "content_block_delta" && e.data["delta"]["type"] == "text_delta"
        });
        let pos_text_stop = text_start_index.and_then(|idx| {
            events.iter().position(|e| {
                e.event == "content_block_stop" && e.data["index"].as_i64() == Some(idx)
            })
        });
        let pos_tool_start = events.iter().position(|e| {
            e.event == "content_block_start" && e.data["content_block"]["type"] == "tool_use"
        });

        assert!(
            text_start_index.is_some(),
            "should start a text block to flush buffered text"
        );
        assert!(
            pos_text_delta.is_some(),
            "should flush buffered text as text_delta"
        );
        assert!(
            pos_text_stop.is_some(),
            "should stop text block before tool_use block starts"
        );
        assert!(pos_tool_start.is_some(), "should start tool_use block");

        let pos_text_delta = pos_text_delta.unwrap();
        let pos_text_stop = pos_text_stop.unwrap();
        let pos_tool_start = pos_tool_start.unwrap();

        assert!(
            pos_text_delta < pos_text_stop && pos_text_stop < pos_tool_start,
            "ordering should be: text_delta -> text_stop -> tool_use_start"
        );

        assert!(
            events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "有修改："
            }),
            "flushed text should equal the buffered prefix"
        );
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();

        // thinking 内容以 `</thinking>` 结尾，但后面没有 `\n\n`（模拟紧跟 tool_use 的场景）
        all_events.extend(ctx.process_assistant_response("<thinking>abc</thinking>"));

        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "Write".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
    fn test_final_flush_filters_standalone_thinking_end_tag() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
        let _ = ctx.generate_initial_events();

        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(
            "先看看结果。\ncall\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>",
        ));
        all.extend(ctx.generate_final_events());

        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "narrative+stray+invoke 应捞回 1 个 tool_use: {:?}", tools);
        let text = collect_text_content(&all);
        assert!(text.contains("先看看结果"), "叙述正文应保留: {:?}", text);
        assert!(!text.contains("call\n<invoke") && !text.contains("<invoke"), "invoke 不应泄漏为文本: {:?}", text);
    }

    #[test]
    fn test_invoke_sniff_keeps_narrative_before_invoke() {
        // 🟢 invoke 前有叙述：text 含"先看看"，1 个 tool_use
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
        let _ = ctx.generate_initial_events();

        // 行首开标签，永不闭合；填充超过上限的纯文本（无 </invoke>）
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response("<invoke name=\"x\">"));
        // 一次喂入超过上限的内容（用不含 `<` 的填充，避免触发其它路径）
        let filler = "A".repeat(StreamContext::MAX_INVOKE_HOLD_BYTES + 1024);
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
            .position(|e| e.event == "content_block_stop" && e.data["index"].as_i64() == Some(index))
            .unwrap_or_else(|| panic!("block {index} should stop"))
    }

    #[test]
    fn test_end_tag_newlines_split_across_events() {
        // `</thinking>\n` 在 chunk 1，`\n` 在 chunk 2，`text` 在 chunk 3
        // 确保 `</thinking>` 不会被部分当作 thinking 内容发出
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
    fn test_thinking_only_sets_max_tokens_stop_reason() {
        // 整个流只有 thinking 块，没有 text 也没有 tool_use，stop_reason 应为 max_tokens
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
        let _initial_events = ctx.generate_initial_events();

        let mut all_events = Vec::new();
        all_events.extend(ctx.process_assistant_response("<thinking>\nabc</thinking>"));
        all_events.extend(ctx.generate_final_events());

        let message_delta = all_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "stop_reason should be max_tokens when only thinking is produced"
        );

        // 应补发一套完整的 text 事件（content_block_start + delta 空格 + content_block_stop）
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "should emit text content_block_start"
        );
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "should emit text_delta with a single space"
        );
        // text block 应被 generate_final_events 自动关闭
        let text_block_index = all_events
            .iter()
            .find_map(|e| {
                if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                    e.data["index"].as_i64()
                } else {
                    None
                }
            })
            .expect("text block should exist");
        assert!(
            all_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(text_block_index)
            }),
            "text block should be stopped"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        // thinking + text 的情况，stop_reason 应为 end_turn
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), test_known_tools());
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
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        assert!(input.contains("still in patch"), "input 不应被假闭合截断: {input:?}");
        assert!(input.contains("line3"), "input 应含 line3: {input:?}");
        let text = collect_text_content(&all);
        assert!(!text.contains("still in patch"), "patch 正文不应泄漏到 text: {text:?}");
    }

    /// 🅿️ P0-1：参数值里含字面 `</parameter>`，值不应被截断丢失后半段。
    #[test]
    fn test_invoke_param_value_contains_literal_parameter_close() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
        let _ = ctx.generate_initial_events();
        let payload = "count\n<invoke name=\"apply_patch\"><parameter name=\"input\">before</parameter> after the fake close</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "应合成 1 个 tool_use: {:?}", tools);
        let parsed: serde_json::Value = serde_json::from_str(&tools[0].1).expect("合法 JSON");
        let input = parsed["input"].as_str().expect("有 input");
        assert!(input.contains("after the fake close"), "后半段不应丢: {input:?}");
    }

    /// 🅱：代码围栏（```）内的 <invoke> 是正文展示，不应被捞回成 tool_use。
    #[test]
    fn test_invoke_inside_code_fence_not_captured() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
        let _ = ctx.generate_initial_events();
        let payload = "示例代码：\n```\n<invoke name=\"exec_command\"><parameter name=\"cmd\">rm -rf /</parameter></invoke>\n```\n讲解完毕。";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "围栏内展示文本不应被捞回: {:?}", tools);
        let text = collect_text_content(&all);
        assert!(text.contains("<invoke name=\"exec_command\">"), "应原样保留: {text:?}");
    }

    /// 🅳：合成出的工具名不在已知工具表里 → 不捞回，当文本吐出（防误执行）。
    #[test]
    fn test_invoke_unknown_tool_name_not_synthesized() {
        // 已知工具表里没有 totally_unknown_tool
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
        let _ = ctx.generate_initial_events();
        let payload = "count\n<invoke name=\"totally_unknown_tool\"><parameter name=\"x\">1</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "未知工具名不应被合成: {:?}", tools);
        let text = collect_text_content(&all);
        assert!(text.contains("totally_unknown_tool"), "未知工具应原样当文本: {text:?}");
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
        let payload = "count\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "工具表为空时不应捞回: {:?}", tools);
    }

    /// 🅲：stray token `card` 也应被剥掉，块仍被捞回。
    #[test]
    fn test_invoke_strips_stray_card_token() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
        let _ = ctx.generate_initial_events();
        let payload = "我先等结果。\n\ncard\n<invoke name=\"wait_agent\"><parameter name=\"x\">1</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "card 前缀的块应被捞回: {:?}", tools);
        assert_eq!(tools[0].0, "wait_agent");
        let text = collect_text_content(&all);
        assert!(!text.contains("card"), "card stray token 不应泄漏: {text:?}");
        assert!(text.contains("我先等结果"), "正常叙述应保留: {text:?}");
    }

    /// 🅱 跨 chunk：``` 围栏开标签在 chunk 边界被切碎，仍能正确识别围栏内不捞回。
    #[test]
    fn test_invoke_fence_split_across_chunks() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
        let _ = ctx.generate_initial_events();
        let mut all = Vec::new();
        // 围栏开标签分两个 chunk 到达
        all.extend(ctx.process_assistant_response("看代码：\n``"));
        all.extend(ctx.process_assistant_response("`\n<invoke name=\"exec_command\"><parameter name=\"cmd\">x</parameter></invoke>\n```"));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert!(tools.is_empty(), "跨 chunk 围栏内不应捞回: {:?}", tools);
    }

    /// 🟡 回归（Reviewer 问题1）：连发 burst，块 A 在 `</invoke>` 前混了非 `>` 收尾文字，
    /// 不应把 A、B 误合并成一个块、也不应让 B 的参数串进 A。两个块都应独立捞回。
    #[test]
    fn test_invoke_burst_with_trailing_text_not_merged() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
        let _ = ctx.generate_initial_events();
        let payload = "count\n<invoke name=\"tool_a\"><parameter name=\"x\">1</parameter>trailing plain</invoke><invoke name=\"tool_b\"><parameter name=\"y\">2</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 2, "应独立合成 2 个 tool_use，不能误合并: {:?}", tools);
        assert_eq!(tools[0].0, "tool_a");
        assert_eq!(tools[1].0, "tool_b");
        let a: serde_json::Value = serde_json::from_str(&tools[0].1).expect("合法 JSON");
        let b: serde_json::Value = serde_json::from_str(&tools[1].1).expect("合法 JSON");
        assert!(a.get("y").is_none(), "B 的参数 y 不应串进 A: {a:?}");
        assert_eq!(a["x"], "1");
        assert_eq!(b["y"], "2");
    }

    /// 🟢 正常连发 burst（块紧贴、A 以 </parameter> 收尾）仍应正确拆成两个。
    #[test]
    fn test_invoke_burst_clean_two_blocks() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
            ["exec_command", "update_plan", "update_goal"].iter().map(|s| s.to_string()).collect();
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), known);
        let _ = ctx.generate_initial_events();
        // 逐字摘自 thread 019e9e8d 真实泄漏 assistant 消息
        let real = "）。\n\ncount\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">cd /Users/yuyifeng/.codex/everything-codex/runtime/agent-tools && python3 -m pytest -q -p no:cacheprovider objects/dev/beads/leaves/create_issue/ 2>&1 | tail -8</parameter>\n<parameter name=\"yield_time_ms\">60000</parameter>\n</invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(real));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "真实泄漏样本应被捞回成 1 个 tool_use: {:?}", tools);
        assert_eq!(tools[0].0, "exec_command", "name 应为 exec_command");
        let parsed: serde_json::Value =
            serde_json::from_str(&tools[0].1).expect("input 应为合法 JSON");
        assert!(
            parsed["cmd"].as_str().unwrap_or("").contains("pytest"),
            "cmd 参数应完整保留: {:?}", parsed
        );
        assert_eq!(parsed["yield_time_ms"], "60000", "yield_time_ms 参数应保留");
        // 关键：字面 <invoke> 不应泄漏到 text
        let text = collect_text_content(&all);
        assert!(
            !text.contains("<invoke name=\"exec_command\">"),
            "字面 <invoke> 不应泄漏到文本: {:?}", text
        );
        // count stray token 也不应泄漏
        assert!(!text.contains("\ncount\n") && !text.ends_with("count"),
            "count stray token 不应泄漏: {:?}", text);
    }

    // ---- 复读熔断 (repeat guard)：root cause = Opus 长上下文退化复读 ----

    /// 🔴→🟢 复现真实泄漏：模型一句正常话后无限复读 `count`（thread 019ea4e9 的真账）。
    /// 熔断后吐出的 count 数必须远小于喂入的数量，且不撑满输出。
    #[test]
    fn repeat_guard_trips_on_count_flood() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
        let _ = ctx.generate_initial_events();
        let payload =
            "count\n<invoke name=\"exec_command\"><parameter name=\"cmd\">ls</parameter></invoke>";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let tools = collect_tool_uses(&all);
        assert_eq!(tools.len(), 1, "单个引导词不应触发熔断，invoke 应正常捞回: {:?}", tools);
        assert_eq!(tools[0].0, "exec_command");
    }

    /// 🟢 不误伤：正常多行文本里偶尔出现 count 单词（非独占行复读）不熔断。
    #[test]
    fn repeat_guard_does_not_trip_on_normal_prose() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
        let _ = ctx.generate_initial_events();
        let payload = "我数了一下 count = 3，然后继续做别的事。\n这是第二行正常文字。\n第三行也正常。";
        let mut all = Vec::new();
        all.extend(ctx.process_assistant_response(payload));
        all.extend(ctx.generate_final_events());
        let text = collect_text_content(&all);
        assert!(text.contains("我数了一下"), "正常正文不应被熔断: {:?}", text);
        assert!(text.contains("第三行也正常"), "正常正文应完整保留: {:?}", text);
    }

    /// 🟢 跨 chunk 复读也能熔断（流式分片到达，每片一个 count）。
    #[test]
    fn repeat_guard_trips_across_chunks() {
        let mut ctx =
            StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), test_known_tools());
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
            &test_known_tools(),
            &std::collections::HashMap::new(),
        );
        let joined: String = blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect();
        let emitted = joined.matches("count").count();
        assert!(emitted < 64, "块级路径应折叠 count 洪水：实际={}", emitted);
        assert!(joined.contains("先看 crawlee 状态"), "正常正文应保留: {:?}", &joined[..joined.len().min(60)]);
    }

    /// 🟢 块级不误伤：单个引导词 count + 真 invoke 仍被捞回。
    #[test]
    fn extract_blocks_keeps_single_stray_and_reclaims() {
        let text = "count\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n</invoke>";
        let blocks = extract_invoke_content_blocks(
            text,
            &test_known_tools(),
            &std::collections::HashMap::new(),
        );
        assert!(
            blocks.iter().any(|b| b["type"] == "tool_use" && b["name"] == "exec_command"),
            "单个引导词不应触发折叠，invoke 应捞回: {:?}",
            blocks
        );
    }

    #[test]
    fn test_native_reasoning_event_emits_thinking_with_signature() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), std::collections::HashSet::new());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), std::collections::HashSet::new());
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

        assert_eq!(collect_thinking_content(&all_events), "delayed native reasoning");
        assert!(all_events.iter().any(|e| {
            e.event == "content_block_delta"
                && e.data["delta"]["type"] == "signature_delta"
                && e.data["delta"]["signature"] == "signature-before-text"
        }));
    }

    #[test]
    fn test_native_reasoning_text_downgrades_to_text_when_thinking_disabled() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new(), std::collections::HashSet::new());
        let mut all_events = ctx.generate_initial_events();

        all_events.extend(ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent {
                text: Some("visible reasoning fallback".to_string()),
                signature: Some("ignored-signature".to_string()),
                redacted_content: Some("ignored-redacted".to_string()),
            },
        )));
        all_events.extend(ctx.generate_final_events());

        assert_eq!(collect_text_content(&all_events), "visible reasoning fallback");
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), std::collections::HashSet::new());
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
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new(), std::collections::HashSet::new());
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
}
