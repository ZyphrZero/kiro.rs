//! 事件模型
//!
//! 定义 generateAssistantResponse 流式响应的事件类型

mod assistant;
mod base;
mod context_usage;
mod metadata;
mod metering;
mod reasoning;
mod tool_use;

pub use assistant::AssistantResponseEvent;
pub(crate) use assistant::{MarkdownCodeTracker, ToolUseXmlLeakFilter, strip_tool_use_xml_leaks};
pub use base::Event;
pub use context_usage::ContextUsageEvent;
pub use metadata::{MetadataEvent, TokenUsage};
pub use metering::MeteringEvent;
pub use reasoning::ReasoningContentEvent;
pub use tool_use::ToolUseEvent;
