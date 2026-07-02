//! OpenAI 兼容 API 模块
//!
//! 在既有 Anthropic 管道之上叠加 OpenAI `/v1/chat/completions` 兼容端点。
//!
//! # 设计（方案 A：入口归一化）
//!
//! 入站的 OpenAI [`ChatCompletionRequest`](types::ChatCompletionRequest) 先被
//! [`convert`] 翻译成既有的 Anthropic [`MessagesRequest`](crate::anthropic::types::MessagesRequest)，
//! 随后**复用**既有请求侧管道：`prepare_kiro_request`（提示词过滤 / 裁剪 / 转换 / token 估算 /
//! cache 计量）→ provider 分发。因归一化后是真正的 `MessagesRequest`，cache 计量
//! （`compute_cache_usage_for_key`，经由 `prepare_kiro_request` 内部）原样复用，无需并行版本。
//!
//! **响应回放缓存（`resolve_response_cache`）刻意不接**：其缓存键只按「会话内容 + key_id」算，
//! 不分端点；若 OpenAI 请求与某条 Anthropic `/cc` 请求归一化后内容一致，会命中同一条目并把
//! **Anthropic SSE 字节**原样回放给 OpenAI 客户端，格式错乱。故 OpenAI 路径跳过响应回放缓存，
//! 只保留 cache 计量。
//!
//! 唯一分叉在出站：流式与非流式都把上游字节喂给同一个
//! [`StreamContext`](crate::anthropic::stream::StreamContext)（复用其 `<invoke>` 文本泄漏恢复
//! + thinking 检测），得到 Anthropic [`SseEvent`](crate::anthropic::stream::SseEvent)，再由
//! [`response`] 的状态机映射成 OpenAI `chat.completion.chunk` / `chat.completion`。
//!
//! # 支持端点
//! - `POST /v1/chat/completions`（流式 + 非流式）
//!
//! 模型列表沿用 Anthropic 的 `GET /v1/models`（其 JSON 形态本就 OpenAI 兼容）。

pub mod convert;
pub mod handlers;
pub mod model_mapping;
pub mod response;
pub mod types;

pub use handlers::post_chat_completions;
