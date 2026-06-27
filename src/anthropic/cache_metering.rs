//! 中转层 prompt cache 计量（无状态、确定性、delta-based）
//!
//! Kiro 上游既不做 prompt cache、也不下发 cache_creation / cache_read 字段（实测
//! meteringEvent 只给 credit 计费量），所以中转层上报的缓存计费**纯粹是合成给下游看
//! 的数字**，不对应任何真实缓存命中、也不影响真实成本。下游按 read/creation **分别计价**
//! （creation 贵、read 便宜），所以合成数字必须**经济上自洽**：creation 每轮只应反映
//! 「本轮新增的那一段」，不能随对话变长而虚高。
//!
//! 既然底层没有真实缓存，就不该去"忠实模拟"真实缓存那套随时间/负载漂移的不确定行为。
//! 本模块按**多轮对话缓存实际怎么累积**做纯函数式、确定性的结构化拆分（delta-based）：
//!
//! ```text
//! input    = 最后一条 message（本轮新问题）          —— 未缓存
//! creation = 倒数第二条 message（刚完成、本轮才写入缓存的那段响应）—— 有界，恒为一条
//! read     = system + tools + 更早的全部历史          —— 上一轮已缓存
//! 首轮(messages 仅 1 条) → creation = system+tools（首次写缓存）、read = 0
//! ```
//!
//! 关键性质：**creation 每轮有界（≈一条消息），read 随历史累积增长**。对话越长 read 越大、
//! read:creation 比值自然往上漂——既真实又不死板，且贵的 creation 桶不会被历史规模放大。
//! 同一段对话无论何时重放、负载如何，结果**完全相同**（请求结构的纯函数）。
//!
//! 命中率 `R` ∈ [0,1] 是 **read 留存阻尼**（默认 1.0）：`read_final = read × R`，被砍掉的
//! `read × (1−R)` 推回 input（相当于"假装这段前缀没命中缓存"→ 不给折扣）。R **不触碰
//! creation**，所以贵桶始终经济正确；R=1 给足缓存折扣（真实），调低则更保守。可全局设也可
//! per-key 覆盖。
//!
//! 无后台任务、无落盘、无内存增长——计量只是请求级的纯计算。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// `compute_structural_cache_usage` 的结果：按 estimate 口径算出的三桶基准 + read 留存
/// 阻尼，最终由 [`CacheUsage::split_against_total`] 对真实 total 做互斥分摊。
///
/// 三个 estimate 是比例基准（不是最终值）——真正的 token 数要在拿到真实 total（contextUsage
/// 真值或 count_tokens 估算）后才按比例算出，因为流式响应直到末尾才知道真实 total。
#[derive(Debug, Clone, Copy)]
pub struct CacheUsage {
    /// 本轮新输入（最后一条 message）的 estimate token——这部分永不计入缓存。
    pub input_est: i32,
    /// 本轮新写入缓存的 delta（倒数第二条 message；首轮为 system+tools）的 estimate token。
    pub creation_est: i32,
    /// 整个 prompt（system + tools + 全部 messages）的 estimate token，比例分摊的分母。
    pub prompt_total_est: i32,
    /// read 留存阻尼 R ∈ [0,1]：read 桶保留 `read × R`，其余推回 input（不给缓存折扣）。
    pub read_ratio: f64,
}

impl Default for CacheUsage {
    /// 默认 = 不模拟缓存：`prompt_total_est == 0` 使 `split_against_total` 全量计入 input。
    fn default() -> Self {
        Self {
            input_est: 0,
            creation_est: 0,
            prompt_total_est: 0,
            read_ratio: 1.0,
        }
    }
}

impl CacheUsage {
    /// 按真实 total 口径做互斥分摊，返回 `(input_tokens, cache_creation, cache_read)`，
    /// 三者满足 `input + creation + read == total_real`。
    ///
    /// `total_real` 是最终上报口径的全量 prompt token。input / creation 各按其 estimate 占比
    /// 折算到真实 total，剩余即 read；再对 read 施加留存阻尼 R（砍掉的部分推回 input）。
    /// 无可缓存内容（`prompt_total_est <= 0`）时全部计入 input，不凭空造缓存计数。
    pub fn split_against_total(&self, total_real: i32) -> (i32, i32, i32) {
        let total = total_real.max(0);
        if self.prompt_total_est <= 0 || total == 0 {
            return (total, 0, 0);
        }
        let denom = self.prompt_total_est as f64;
        let input_share = (self.input_est as f64 / denom).clamp(0.0, 1.0);
        let creation_share = (self.creation_est as f64 / denom).clamp(0.0, 1.0);

        // input / creation 按占比折算，clamp 保证 input + creation <= total。
        let mut input = ((total as f64) * input_share).round() as i32;
        input = input.clamp(0, total);
        let mut creation = ((total as f64) * creation_share).round() as i32;
        creation = creation.clamp(0, total - input);

        // 剩余即已缓存前缀（read 基数）。
        let read_base = total - input - creation;
        if read_base <= 0 {
            return (input, creation, 0);
        }
        // read 留存阻尼：保留 read_base × R，被砍部分推回 input（无缓存折扣），creation 不动。
        let r = self.read_ratio.clamp(0.0, 1.0);
        let read = ((read_base as f64) * r).round() as i32;
        let read = read.clamp(0, read_base);
        input += read_base - read;
        (input, creation, read)
    }
}

/// 计量运行时治理：持有全局 read 留存阻尼 R（运行时可经 Admin API 调整）。
///
/// 取代旧的有状态 `CacheMeter`——不再需要容量 / TTL / 落盘 / 会话级开关，只剩一个原子旋钮。
pub struct MeterGovernance {
    /// 全局 R 的 bit 表示（f64 → u64，原子读写）。per-key 未覆盖时用此值。
    read_ratio_bits: AtomicU64,
}

impl MeterGovernance {
    /// 用初始 R 构造（clamp 到 [0,1]）。
    pub fn new(read_ratio: f64) -> Self {
        Self {
            read_ratio_bits: AtomicU64::new(read_ratio.clamp(0.0, 1.0).to_bits()),
        }
    }

    /// 当前全局 R。
    pub fn read_ratio(&self) -> f64 {
        f64::from_bits(self.read_ratio_bits.load(Ordering::Relaxed))
    }

    /// 设置全局 R（clamp 到 [0,1]），运行时立即对后续请求生效。
    pub fn set_read_ratio(&self, ratio: f64) {
        self.read_ratio_bits
            .store(ratio.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

/// `Arc<MeterGovernance>` 别名
pub type SharedMeterGovernance = Arc<MeterGovernance>;

// ============================================================================
// 与请求体协议层的接线
// ============================================================================

use super::stream::estimate_tokens;
use super::types::{MessagesRequest, SystemMessage, Tool};

/// 计算本次请求的 delta-based 结构化缓存覆盖情况。纯函数：只看请求结构与 R，不依赖任何
/// 跨请求状态、时间、负载。返回 [`CacheUsage`]，由调用方在拿到真实 total 后做互斥分摊。
///
/// 桶划分（见模块文档）：input = 最后一条 message；creation = 倒数第二条 message（首轮为
/// system+tools）；read = 其余前缀。`read_ratio` 是该请求生效的 R（per-key 覆盖优先，否则
/// 全局 [`MeterGovernance`]）。
pub fn compute_structural_cache_usage(req: &MessagesRequest, read_ratio: f64) -> CacheUsage {
    // system + tools 开销（首轮即首次写入缓存的那段）。
    let mut overhead: i32 = 0;
    if let Some(tools) = req.tools.as_ref() {
        for t in tools {
            overhead = overhead.saturating_add(tool_tokens(t));
        }
    }
    if let Some(systems) = req.system.as_ref() {
        for sys in systems {
            overhead = overhead.saturating_add(system_tokens(sys));
        }
    }

    let n = req.messages.len();
    if n == 0 {
        // 无 message：无可缓存内容，全入 input（prompt_total_est=0 触发默认分摊）。
        return CacheUsage {
            input_est: 0,
            creation_est: 0,
            prompt_total_est: 0,
            read_ratio: read_ratio.clamp(0.0, 1.0),
        };
    }

    let msg_est: Vec<i32> = req.messages.iter().map(message_tokens).collect();
    let msgs_total: i32 = msg_est.iter().fold(0, |a, b| a.saturating_add(*b));
    let prompt_total_est = overhead.saturating_add(msgs_total);

    // input = 最后一条 message（本轮新问题）。
    let input_est = msg_est[n - 1];
    // creation = 本轮新写入缓存的 delta：
    //   首轮(n==1) → system+tools（第一次把 system/tools 写进缓存）
    //   否则       → 倒数第二条 message（刚完成、本轮才补进缓存的那段响应，有界）
    let creation_est = if n == 1 { overhead } else { msg_est[n - 2] };

    CacheUsage {
        input_est,
        creation_est,
        prompt_total_est,
        read_ratio: read_ratio.clamp(0.0, 1.0),
    }
}

/// 估算一条 message 的 token：遍历 content blocks，文本 / thinking 走文本估算，
/// 图片走 Anthropic 口径尺寸估算。string content 直接估算原文。
fn message_tokens(msg: &super::types::Message) -> i32 {
    match &msg.content {
        serde_json::Value::String(s) => estimate_tokens(s).max(0),
        serde_json::Value::Array(arr) => {
            let mut sum: i32 = 0;
            for v in arr {
                if v.get("type").and_then(|t| t.as_str()) == Some("image") {
                    let (media_type, data) = image_source_parts(v);
                    sum = sum
                        .saturating_add(crate::image_resize::estimate_image_tokens(media_type, data) as i32);
                } else {
                    let text = block_token_text(v);
                    if !text.is_empty() {
                        sum = sum.saturating_add(estimate_tokens(&text).max(0));
                    }
                }
            }
            sum
        }
        _ => 0,
    }
}

/// 工具的 token 估算：name + description + schema 拼接原文。
fn tool_tokens(t: &Tool) -> i32 {
    let schema = serde_json::to_string(&t.input_schema).unwrap_or_default();
    estimate_tokens(&format!("{} {} {}", t.name, t.description, schema)).max(0)
}

/// system block 的 token 估算。
fn system_tokens(s: &SystemMessage) -> i32 {
    estimate_tokens(&s.text).max(0)
}

/// content block 的 token 估算原文：仅 text + thinking 的纯文本。
fn block_token_text(v: &serde_json::Value) -> String {
    let s = |key: &str| v.get(key).and_then(|x| x.as_str()).unwrap_or("");
    let text = s("text");
    let thinking = s("thinking");
    if thinking.is_empty() {
        text.to_string()
    } else if text.is_empty() {
        thinking.to_string()
    } else {
        format!("{text} {thinking}")
    }
}

/// 从 image content block 取 `(media_type, base64_data)`，缺字段时返回空串（估算走保底）。
fn image_source_parts(v: &serde_json::Value) -> (&str, &str) {
    let src = v.get("source");
    let media_type = src
        .and_then(|s| s.get("media_type"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let data = src
        .and_then(|s| s.get("data"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    (media_type, data)
}

// ============================================================================
// 会话隔离种子（响应缓存 response_cache 复用同一口径构造缓存键）
// ============================================================================

/// 生成会话隔离种子。
///
/// 优先级：
///   1. metadata.user_id 里的 session 段（Claude Code 格式含 `_session_<uuid>`）；
///   2. 退回客户端 Key id。
///
/// `pub(crate)`：响应缓存复用同一套会话隔离口径构造缓存键，保证两者隔离边界一致。
pub(crate) fn isolation_seed(req: &MessagesRequest, key_id: u64) -> String {
    if let Some(session) = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_deref())
        .and_then(extract_session_id)
    {
        return format!("sess:{session}");
    }
    format!("key:{key_id}")
}

/// 从 Claude Code 的 user_id 中提取 session 标识。
/// 格式形如 `user_<hash>_account__session_<uuid>`，取 `_session_` 之后的部分。
fn extract_session_id(user_id: &str) -> Option<String> {
    user_id
        .split_once("_session_")
        .map(|(_, sid)| sid.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{Message, MessagesRequest, Metadata, SystemMessage};

    fn msg(role: &str, text: &str) -> Message {
        Message {
            role: role.to_string(),
            content: serde_json::json!([{ "type": "text", "text": text }]),
        }
    }

    fn req_with(messages: Vec<Message>, system: Option<Vec<SystemMessage>>) -> MessagesRequest {
        MessagesRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 32,
            messages,
            stream: false,
            system,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    // ---- split_against_total ------------------------------------------------

    #[test]
    fn split_no_prefix_all_input() {
        // prompt_total_est == 0（默认）→ 全量计入 input。
        let u = CacheUsage::default();
        assert_eq!(u.split_against_total(500), (500, 0, 0));
    }

    #[test]
    fn split_three_buckets_by_share() {
        // input 占比 10%、creation 占比 5%，剩余 85% 为 read（R=1 全留存）。
        let u = CacheUsage {
            input_est: 10,
            creation_est: 5,
            prompt_total_est: 100,
            read_ratio: 1.0,
        };
        let (input, creation, read) = u.split_against_total(1000);
        assert_eq!(input, 100);
        assert_eq!(creation, 50);
        assert_eq!(read, 850);
        assert_eq!(input + creation + read, 1000);
    }

    #[test]
    fn split_creation_bounded_independent_of_history() {
        // creation 只随 creation_est 占比走，不随 read 基数（历史规模）变化——贵桶有界。
        // 短历史：total 小
        let short = CacheUsage {
            input_est: 10,
            creation_est: 20,
            prompt_total_est: 100,
            read_ratio: 1.0,
        };
        // 长历史：同样的 input/creation 占比，但 prompt_total 大得多（read 基数暴涨）
        let long = CacheUsage {
            input_est: 10,
            creation_est: 20,
            prompt_total_est: 1000,
            read_ratio: 1.0,
        };
        let (_, c_short, _) = short.split_against_total(300);
        let (_, c_long, r_long) = long.split_against_total(3000);
        // creation 占比相同（20/100 vs 20/1000 → 真实 total 也等比放大），关键是 read 吃掉增量
        assert_eq!(c_short, 60); // 300 × 20/100
        assert_eq!(c_long, 60); // 3000 × 20/1000 —— creation 不被历史放大
        assert!(r_long > 2000, "历史增长全进 read（便宜桶），不进 creation");
    }

    #[test]
    fn split_read_retention_pushes_to_input_not_creation() {
        // R<1：read 被砍的部分推回 input，creation 纹丝不动（贵桶经济正确）。
        let u = CacheUsage {
            input_est: 10,
            creation_est: 10,
            prompt_total_est: 100,
            read_ratio: 0.5,
        };
        let (input, creation, read) = u.split_against_total(1000);
        // base: input=100, creation=100, read_base=800
        // R=0.5 → read=400，被砍 400 推回 input → input=500
        assert_eq!(input, 500);
        assert_eq!(creation, 100, "creation 不受 R 影响");
        assert_eq!(read, 400);
        assert_eq!(input + creation + read, 1000);
    }

    #[test]
    fn split_ratio_zero_no_read() {
        // R=0：完全不给缓存折扣，read 全部推回 input；creation 仍按其占比保留。
        let u = CacheUsage {
            input_est: 10,
            creation_est: 10,
            prompt_total_est: 100,
            read_ratio: 0.0,
        };
        let (input, creation, read) = u.split_against_total(1000);
        assert_eq!(creation, 100);
        assert_eq!(read, 0);
        assert_eq!(input, 900);
    }

    #[test]
    fn split_is_deterministic() {
        let u = CacheUsage {
            input_est: 33,
            creation_est: 41,
            prompt_total_est: 207,
            read_ratio: 1.0,
        };
        let a = u.split_against_total(4096);
        let b = u.split_against_total(4096);
        assert_eq!(a, b);
        assert_eq!(a.0 + a.1 + a.2, 4096, "互斥口径必须自洽");
    }

    #[test]
    fn split_zero_total_safe() {
        let u = CacheUsage {
            input_est: 10,
            creation_est: 10,
            prompt_total_est: 100,
            read_ratio: 1.0,
        };
        assert_eq!(u.split_against_total(0), (0, 0, 0));
    }

    // ---- compute_structural_cache_usage ------------------------------------

    #[test]
    fn compute_single_message_first_write() {
        // 单条 message + system：input=该 message，creation=system(首次写缓存)，read=0。
        let req = req_with(
            vec![msg("user", "hello there friend")],
            Some(vec![SystemMessage {
                text: "you are helpful ".repeat(20),
                cache_control: None,
            }]),
        );
        let u = compute_structural_cache_usage(&req, 1.0);
        assert!(u.input_est > 0);
        assert!(u.creation_est > 0, "首轮 system+tools 计作 creation");
        let (input, creation, read) = u.split_against_total(u.prompt_total_est);
        assert_eq!(read, 0, "首轮无 read");
        assert!(input > 0 && creation > 0);
        assert_eq!(input + creation + read, u.prompt_total_est);
    }

    #[test]
    fn compute_single_message_no_overhead_all_input() {
        // 单条 message、无 system/tools：creation_est=0 → 全入 input。
        let req = req_with(vec![msg("user", "hi")], None);
        let u = compute_structural_cache_usage(&req, 1.0);
        assert_eq!(u.creation_est, 0);
        assert_eq!(u.input_est, u.prompt_total_est);
        let (input, creation, read) = u.split_against_total(u.prompt_total_est.max(1));
        assert_eq!(creation, 0);
        assert_eq!(read, 0);
        assert_eq!(input, u.prompt_total_est.max(1));
    }

    #[test]
    fn compute_multi_turn_delta_creation_is_prev_message() {
        // 历史(u1,a1) + 本轮 u2：input=u2，creation=a1(倒数第二条)，read=system+tools+u1。
        let big = "the quick brown fox ".repeat(40);
        let req = req_with(
            vec![
                msg("user", &big),
                msg("assistant", &big),
                msg("user", "short new question"),
            ],
            Some(vec![SystemMessage {
                text: "you are helpful ".repeat(50),
                cache_control: None,
            }]),
        );
        let u = compute_structural_cache_usage(&req, 1.0);
        let a1_est = message_tokens(&msg("assistant", &big));
        let u2_est = message_tokens(&msg("user", "short new question"));
        assert_eq!(u.creation_est, a1_est, "creation = 倒数第二条 message");
        assert_eq!(u.input_est, u2_est, "input = 最后一条 message");
        let (input, creation, read) = u.split_against_total(u.prompt_total_est);
        assert!(read > 0, "非首轮应有 cache_read");
        assert!(creation > 0);
        assert!(read > creation, "read（system+u1）应远大于 creation（仅 a1）");
        assert_eq!(input + creation + read, u.prompt_total_est);
    }

    #[test]
    fn compute_creation_does_not_grow_with_history() {
        // 核心经济性质：对话越长，creation 仍≈一条 message，不随历史线性增长。
        let unit = "lorem ipsum dolor sit amet ".repeat(10);
        let short = req_with(
            vec![msg("user", &unit), msg("assistant", &unit), msg("user", "q")],
            None,
        );
        // 长对话：20 条历史 + 本轮
        let mut long_msgs: Vec<Message> = Vec::new();
        for i in 0..10 {
            long_msgs.push(msg("user", &format!("{unit} {i}")));
            long_msgs.push(msg("assistant", &unit));
        }
        long_msgs.push(msg("user", "q"));
        let long = req_with(long_msgs, None);

        let cu_short = compute_structural_cache_usage(&short, 1.0);
        let cu_long = compute_structural_cache_usage(&long, 1.0);
        // creation_est 都≈一条 assistant 消息，长对话不放大
        let a_est = message_tokens(&msg("assistant", &unit));
        assert_eq!(cu_short.creation_est, a_est);
        assert_eq!(cu_long.creation_est, a_est, "长对话 creation 仍是一条 message");
        // 而 prompt_total（→read 基数）长对话远大于短对话
        assert!(cu_long.prompt_total_est > cu_short.prompt_total_est * 5);

        let (_, c_short, _) = cu_short.split_against_total(cu_short.prompt_total_est);
        let (_, c_long, r_long) = cu_long.split_against_total(cu_long.prompt_total_est);
        assert!(r_long > c_long * 5, "长对话增量几乎全进便宜的 read 桶");
        // creation 真实 token 不爆炸（两者同数量级，长对话甚至更小，因占比被摊薄）
        assert!(c_long <= c_short + 5);
    }

    #[test]
    fn compute_read_retention_controls_discount() {
        // R 越大，read 越多、input 越少；creation 不变。
        let body = "lorem ipsum dolor sit amet ".repeat(20);
        let req = req_with(
            vec![msg("user", &body), msg("assistant", &body), msg("user", "q")],
            None,
        );
        let total = compute_structural_cache_usage(&req, 1.0).prompt_total_est;
        let (i_lo, c_lo, r_lo) = compute_structural_cache_usage(&req, 0.5).split_against_total(total);
        let (i_hi, c_hi, r_hi) = compute_structural_cache_usage(&req, 1.0).split_against_total(total);
        assert!(r_hi > r_lo, "R 越大 read 越多");
        assert!(i_hi < i_lo, "R 越大 input 越少（折扣更足）");
        assert_eq!(c_lo, c_hi, "creation 不受 R 影响");
    }

    #[test]
    fn compute_image_message_counts_tokens() {
        let png = make_test_png(750, 750);
        let img_tokens = crate::image_resize::estimate_image_tokens("image/png", &png) as i32;
        assert!(img_tokens > 100);
        let req = req_with(
            vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data": png}},
                        {"type":"text","text":"describe"}
                    ]),
                },
                msg("assistant", "a pixel"),
                msg("user", "and now"),
            ],
            None,
        );
        let u = compute_structural_cache_usage(&req, 1.0);
        // 含图历史(u1)在 read 前缀里 → prompt_total 应远大于本轮纯文本新输入。
        assert!(u.prompt_total_est >= img_tokens, "prompt_total 应含图片 token");
        let (_, _, read) = u.split_against_total(u.prompt_total_est);
        assert!(read > img_tokens / 2, "含图历史进 read 桶");
    }

    #[test]
    fn compute_empty_messages_safe() {
        let req = req_with(vec![], None);
        let u = compute_structural_cache_usage(&req, 1.0);
        assert_eq!(u.input_est, 0);
        assert_eq!(u.creation_est, 0);
        assert_eq!(u.split_against_total(100), (100, 0, 0));
    }

    // ---- MeterGovernance ---------------------------------------------------

    #[test]
    fn governance_get_set_and_clamp() {
        let g = MeterGovernance::new(0.8);
        assert!((g.read_ratio() - 0.8).abs() < 1e-9);
        g.set_read_ratio(0.95);
        assert!((g.read_ratio() - 0.95).abs() < 1e-9);
        // clamp 到 [0,1]
        g.set_read_ratio(1.5);
        assert!((g.read_ratio() - 1.0).abs() < 1e-9);
        g.set_read_ratio(-0.2);
        assert!((g.read_ratio() - 0.0).abs() < 1e-9);
        assert!((MeterGovernance::new(2.0).read_ratio() - 1.0).abs() < 1e-9);
    }

    // ---- isolation_seed ----------------------------------------------------

    #[test]
    fn isolation_seed_prefers_session_then_key() {
        let mut req = req_with(vec![msg("user", "x")], None);
        assert_eq!(isolation_seed(&req, 7), "key:7");
        req.metadata = Some(Metadata {
            user_id: Some("user_abc_account__session_uuid-123".to_string()),
        });
        assert_eq!(isolation_seed(&req, 7), "sess:uuid-123");
    }

    #[test]
    fn extract_session_id_parses_claude_code_format() {
        assert_eq!(
            extract_session_id("user_xxx_account__session_0b4445e1-uuid"),
            Some("0b4445e1-uuid".to_string())
        );
        assert_eq!(extract_session_id("no-session-here"), None);
        assert_eq!(extract_session_id("trailing_session_"), None);
    }

    fn make_test_png(w: u32, h: u32) -> String {
        use base64::{Engine, engine::general_purpose::STANDARD as B64};
        use image::{ImageFormat, Rgb, RgbImage};
        use std::io::Cursor;
        let mut img = RgbImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.put_pixel(x, y, Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
            }
        }
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        B64.encode(&buf)
    }
}
