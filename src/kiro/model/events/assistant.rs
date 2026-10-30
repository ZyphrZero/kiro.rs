//! 助手响应事件
//!
//! 处理 assistantResponseEvent 类型的事件

use serde::{Deserialize, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

const TOOL_USE_XML_PREFIX: &str = "<tool_use";
const TOOL_USE_XML_CLOSE: &str = "</tool_use>";

/// 一个带属性的 `<tool_use ...>` 开标签之后，最多等多少字节的 `</tool_use>`。
/// 超过仍未闭合就按正文原样放出，避免一个永不闭合的标签扣住后续全部回复。
const MAX_TOOL_USE_HOLD_BYTES: usize = 65_536;

/// 增量追踪「已经作为正文输出的文本」末尾所处的 Markdown 代码上下文。
///
/// 用于判断下一个字符是否落在代码里：``` / `~~~` 围栏、缩进代码块（行首 4 空格或 tab）、
/// 未闭合的行内反引号。拿不准时偏向「在代码里」，因为调用方在代码里一律按正文原样保留。
#[derive(Debug, Clone, Default)]
pub(crate) struct MarkdownCodeTracker {
    /// 当前打开的围栏：(围栏字符, 长度)。
    fence: Option<(char, usize)>,
    /// 当前还没换行的那一行。
    line: String,
    /// 本段落里未闭合的行内反引号串长度（0 表示不在行内代码里）。
    inline_ticks: usize,
}

impl MarkdownCodeTracker {
    /// 把一段已经输出的正文推进进状态。
    pub(crate) fn advance(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.line);
                self.finish_line(&line);
            } else {
                self.line.push(ch);
            }
        }
    }

    fn finish_line(&mut self, line: &str) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some((fc, flen)) = self.fence {
            if fence_marker(line).is_some_and(|(c, n, rest)| {
                c == fc && n >= flen && rest.trim().is_empty()
            }) {
                self.fence = None;
            }
            return;
        }
        if let Some((c, n, _)) = fence_marker(line) {
            self.fence = Some((c, n));
            self.inline_ticks = 0;
            return;
        }
        if line.trim().is_empty() {
            // 空行结束段落：未闭合的反引号串按字面处理。
            self.inline_ticks = 0;
            return;
        }
        self.inline_ticks = scan_inline_ticks(self.inline_ticks, line);
    }

    /// 下一个字符是否落在代码里。
    pub(crate) fn in_code(&self) -> bool {
        if self.fence.is_some() {
            return true;
        }
        if self.line.starts_with("    ") || self.line.starts_with('\t') {
            return true;
        }
        scan_inline_ticks(self.inline_ticks, &self.line) > 0
    }

    /// 下一个字符是否位于行首（本行此前没有任何字符，包括空白）。
    pub(crate) fn at_line_start(&self) -> bool {
        self.line.is_empty()
    }
}

/// 行首（允许前导空白）的 ``` / `~~~` 围栏标记：返回 (字符, 长度, 标记之后的内容)。
fn fence_marker(line: &str) -> Option<(char, usize, &str)> {
    let trimmed = line.trim_start();
    let c = trimmed.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = trimmed.chars().take_while(|&x| x == c).count();
    if n < 3 {
        return None;
    }
    let rest = &trimmed[n * c.len_utf8()..];
    // CommonMark：反引号围栏的 info string 不能再含反引号。
    if c == '`' && rest.contains('`') {
        return None;
    }
    Some((c, n, rest))
}

/// 从 `open`（0 = 不在行内代码里）开始扫描一行，返回行尾时未闭合的反引号串长度。
fn scan_inline_ticks(mut open: usize, line: &str) -> usize {
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && open == 0 {
            chars.next();
            continue;
        }
        if c != '`' {
            continue;
        }
        let mut n = 1;
        while chars.peek() == Some(&'`') {
            chars.next();
            n += 1;
        }
        if open == 0 {
            open = n;
        } else if n == open {
            open = 0;
        }
    }
    open
}

/// 跨 chunk 剥离混入 assistant 文本的字面 `<tool_use ...>...</tool_use>` XML 泄漏。
///
/// Kiro 有时把工具调用意图以字面 XML 吐进文本里，真正的调用走结构化 `toolUseEvent`，
/// 这段 XML 是重复噪声。但模型讲解消息格式时也会写出 `<tool_use>`，所以只剥离同时满足
/// 以下条件的块，其余一律按正文原样输出：
/// - 开标签带属性（`<tool_use id=... name=...>`，真实泄漏都带；裸 `<tool_use>` 是讲解）；
/// - 不在代码里（围栏、缩进代码块、行内反引号）；
/// - 在 [`MAX_TOOL_USE_HOLD_BYTES`] 内等到了 `</tool_use>`。没有闭合就原样输出，不吞后文。
#[derive(Debug, Default)]
pub(crate) struct ToolUseXmlLeakFilter {
    buffer: String,
    code: MarkdownCodeTracker,
}

enum ToolUseTag {
    /// 不是要剥离的泄漏块：原样输出 `<tool_use` 继续扫描。
    Keep,
    /// 完整泄漏块，结束位置（相对开标签起点，exclusive）。
    Strip(usize),
    /// 还不能判定，等更多内容。
    Wait,
}

impl ToolUseXmlLeakFilter {
    /// 喂入一段文本，返回可以安全输出的部分（可能为空，表示在等闭合）。
    pub(crate) fn filter(&mut self, content: &str) -> String {
        self.buffer.push_str(content);
        self.drain(false)
    }

    /// 流结束：残留内容（含未闭合的 `<tool_use`）原样输出。
    pub(crate) fn finish(&mut self) -> String {
        self.drain(true)
    }

    fn drain(&mut self, at_end: bool) -> String {
        let buf = std::mem::take(&mut self.buffer);
        let mut out = String::with_capacity(buf.len());
        let mut rest = buf.as_str();
        loop {
            let Some(start) = rest.find(TOOL_USE_XML_PREFIX) else {
                let keep = if at_end {
                    0
                } else {
                    longest_prefix_suffix(rest, TOOL_USE_XML_PREFIX)
                };
                let emit = &rest[..rest.len() - keep];
                self.code.advance(emit);
                out.push_str(emit);
                self.buffer = rest[rest.len() - keep..].to_string();
                return out;
            };
            let before = &rest[..start];
            self.code.advance(before);
            out.push_str(before);
            let tail = &rest[start..];
            match self.classify(tail, at_end) {
                ToolUseTag::Strip(end) => {
                    rest = &tail[end..];
                }
                ToolUseTag::Keep => {
                    let lit = &tail[..TOOL_USE_XML_PREFIX.len()];
                    self.code.advance(lit);
                    out.push_str(lit);
                    rest = &tail[TOOL_USE_XML_PREFIX.len()..];
                }
                ToolUseTag::Wait => {
                    self.buffer = tail.to_string();
                    return out;
                }
            }
        }
    }

    fn classify(&self, tail: &str, at_end: bool) -> ToolUseTag {
        if self.code.in_code() {
            return ToolUseTag::Keep;
        }
        let after = &tail[TOOL_USE_XML_PREFIX.len()..];
        let Some(next) = after.chars().next() else {
            return if at_end { ToolUseTag::Keep } else { ToolUseTag::Wait };
        };
        // 只有带属性的开标签才可能是泄漏：`<tool_use` 后必须紧跟空白。
        if !next.is_whitespace() {
            return ToolUseTag::Keep;
        }
        let Some(gt) = after.find('>') else {
            return if at_end || tail.len() > MAX_TOOL_USE_HOLD_BYTES {
                ToolUseTag::Keep
            } else {
                ToolUseTag::Wait
            };
        };
        if after[..gt].trim().is_empty() {
            // `<tool_use >`：没有属性，按讲解文本处理。
            return ToolUseTag::Keep;
        }
        let body_start = TOOL_USE_XML_PREFIX.len() + gt + 1;
        match tail[body_start..].find(TOOL_USE_XML_CLOSE) {
            Some(close) => ToolUseTag::Strip(body_start + close + TOOL_USE_XML_CLOSE.len()),
            None if at_end || tail.len() > MAX_TOOL_USE_HOLD_BYTES => ToolUseTag::Keep,
            None => ToolUseTag::Wait,
        }
    }
}

/// 返回 `s` 末尾「恰好是 `needle` 某个真前缀」的最长长度（chunk 边界保留用）。
fn longest_prefix_suffix(s: &str, needle: &str) -> usize {
    let max = s.len().min(needle.len().saturating_sub(1));
    for len in (1..=max).rev() {
        if s.is_char_boundary(s.len() - len) && needle.starts_with(&s[s.len() - len..]) {
            return len;
        }
    }
    0
}

/// 一次性剥离完整文本里的 `<tool_use>` 泄漏，规则与流式 [`ToolUseXmlLeakFilter`] 完全相同。
/// 不改动正文首尾空白（与流式输出一致）。
pub(crate) fn strip_tool_use_xml_leaks(content: &str) -> String {
    let mut filter = ToolUseXmlLeakFilter::default();
    let mut out = filter.filter(content);
    out.push_str(&filter.finish());
    out
}

/// 助手响应事件
///
/// 包含 AI 助手的流式响应内容
///
/// # 设计说明
///
/// 此结构体只保留实际使用的 `content` 字段，其他 API 返回的字段
/// 通过 `#[serde(flatten)]` 捕获到 `extra` 中，确保反序列化不会失败。
///
/// # 示例
///
/// ```rust
/// use kiro_rs::kiro::model::events::AssistantResponseEvent;
///
/// let json = r#"{"content":"Hello, world!"}"#;
/// let event: AssistantResponseEvent = serde_json::from_str(json).unwrap();
/// assert_eq!(event.content, "Hello, world!");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantResponseEvent {
    /// 响应内容片段
    #[serde(default)]
    pub content: String,

    /// 捕获其他未使用的字段，确保反序列化兼容性
    #[serde(flatten)]
    #[serde(skip_serializing)]
    #[allow(dead_code)]
    extra: serde_json::Value,
}

impl EventPayload for AssistantResponseEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

impl Default for AssistantResponseEvent {
    fn default() -> Self {
        Self {
            content: String::new(),
            extra: serde_json::Value::Null,
        }
    }
}

impl std::fmt::Display for AssistantResponseEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_tool_use_xml_leaks() {
        // 剥离标签块本身，保留其两侧文本（周围换行原样保留）。
        let content =
            "before\n<tool_use id=\"toolu_1\" name=\"Read\">\n{\"path\":\"/a\"}\n</tool_use>\nafter";
        assert_eq!(strip_tool_use_xml_leaks(content), "before\n\nafter");
    }

    #[test]
    fn test_strip_tool_use_xml_leaks_keeps_similar_text() {
        // `<tool_user>` 不是真标签（`<tool_use` 后紧跟 `r`），应原样保留。
        let content = "use <tool_user> as an example";
        assert_eq!(strip_tool_use_xml_leaks(content), content);
    }

    /// H1：开标签被截断（流结束仍无 `>` / `</tool_use>`）时原样保留，不吞内容。
    #[test]
    fn test_strip_tool_use_xml_leaks_keeps_truncated_open_tag() {
        let content = "before\n\n<tool_use id=\"toolu_1\" name=\"Write\"";
        assert_eq!(strip_tool_use_xml_leaks(content), content);
    }

    /// H1：讲解格式时写出的裸 `<tool_use>` 没有闭合，其后的正文必须原样保留。
    #[test]
    fn strip_tool_use_keeps_explanatory_literal_and_following_text() {
        let content = "说明：<tool_use> 块包含 id、name、input。后续正文\n第二段回答";
        assert_eq!(strip_tool_use_xml_leaks(content), content);
    }

    /// H1：带属性但没有闭合的开标签同样原样保留后文。
    #[test]
    fn strip_tool_use_keeps_unclosed_attributed_tag_and_following_text() {
        let content = "示例 <tool_use id=\"t1\" name=\"Read\"> 之后还有很长的回答。";
        assert_eq!(strip_tool_use_xml_leaks(content), content);
    }

    /// H1：代码里（反引号、``` 和 ~~~ 围栏、缩进代码块）的完整块不剥离。
    #[test]
    fn strip_tool_use_keeps_blocks_inside_code() {
        let block = "<tool_use id=\"t\" name=\"Read\">{}</tool_use>";
        for content in [
            format!("行内 `{block}` 讲解"),
            format!("```xml\n{block}\n```\n后文"),
            format!("~~~\n{block}\n~~~\n后文"),
            format!("示例：\n\n    {block}\n后文"),
        ] {
            assert_eq!(strip_tool_use_xml_leaks(&content), content, "{content:?}");
        }
    }

    /// 流式：未闭合块跨 chunk 等待，流结束时原样吐回。
    #[test]
    fn tool_use_filter_streaming_releases_unclosed_at_finish() {
        let mut f = ToolUseXmlLeakFilter::default();
        let mut out = f.filter("讲解 <tool_use id=\"t1\">");
        out.push_str(&f.filter(" 后续正文"));
        out.push_str(&f.finish());
        assert_eq!(out, "讲解 <tool_use id=\"t1\"> 后续正文");
    }

    #[test]
    fn test_deserialize_simple() {
        let json = r#"{"content":"Hello, world!"}"#;
        let event: AssistantResponseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.content, "Hello, world!");
    }

    #[test]
    fn test_deserialize_with_extra_fields() {
        // 确保包含额外字段时反序列化不会失败
        let json = r#"{
            "content": "Done",
            "conversationId": "conv-123",
            "messageId": "msg-456",
            "messageStatus": "COMPLETED",
            "followupPrompt": {
                "content": "Would you like me to explain further?",
                "userIntent": "EXPLAIN_CODE_SELECTION"
            }
        }"#;
        let event: AssistantResponseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.content, "Done");
    }

    #[test]
    fn test_serialize_minimal() {
        let event = AssistantResponseEvent::default();
        let event = AssistantResponseEvent {
            content: "Test".to_string(),
            ..event
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"content\":\"Test\""));
        // extra 字段不应该被序列化
        assert!(!json.contains("extra"));
    }

    #[test]
    fn test_display() {
        let event = AssistantResponseEvent {
            content: "test".to_string(),
            ..Default::default()
        };
        assert_eq!(format!("{}", event), "test");
    }
}