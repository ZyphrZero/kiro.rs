//! 中转层 prompt cache（无外部依赖）
//!
//! Kiro 上游不下发 cache_creation / cache_read token 字段（实测 meteringEvent
//! 只给 credit 计费量），所以这里在中转层自行模拟"提示词缓存"，复现 Anthropic
//! 滑动窗口缓存的「最长公共前缀命中」语义：
//!
//! - 把 prompt 的稳定前缀按 message 边界切成一条递增前缀段链：
//!   `[tools+system] → [+msg0] → [+msg1] → ... → [+msg(n-2)]`，每段 hash 是
//!   「从头累积到该边界」的指纹，token 是该前缀的累计估算。
//! - 最后一条 message（当前轮新输入）不切段——它是本轮 cache_creation 的尾部。
//! - lookup 取最深命中段 = 最长已缓存前缀 = `cache_read_input_tokens`；其后到
//!   末段 = `cache_creation_input_tokens`；完全 miss → cache_read = 0。
//!
//! 跨轮命中的关键：历史消息逐字节不变，故 Turn N+1 的历史前缀段 hash 必然等于
//! Turn N 写入的同一段。会话隔离：哈希链以一个隔离种子起头（优先 metadata
//! session，否则客户端 Key id），使不同会话 / Key 的相同前缀互不命中。
//!
//! 内存 + JSON 落盘：每分钟一次写到 `cache_dir/cache_metering.json`，启动时读
//! 回过期记录会被丢掉。**不依赖 Redis 或任何外部 KV**。

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// 默认条目上限（防止内存无限增长）
const DEFAULT_CAPACITY: usize = 4096;
/// 最长 TTL（1h，与 Anthropic ttl="1h" 对齐）
const MAX_TTL_SECS: i64 = 3600;
/// 默认 TTL（5min，ephemeral 默认值）
const DEFAULT_TTL_SECS: i64 = 5 * 60;

/// 单个缓存条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// 该前缀段累计的估算 token 数
    pub tokens: u32,
    /// 过期时间戳（unix 秒）
    pub expires_at: i64,
    /// 上次命中时间（用于 LRU 淘汰）
    pub last_hit_at: i64,
}

/// 一次查询的结果（每段一份）
#[derive(Debug, Clone, Copy)]
pub struct SegmentResult {
    /// 该段是否命中
    pub hit: bool,
    /// 该段累计 tokens（保留供调试 / 调用方扩展，dead_code 抑制）
    #[allow(dead_code)]
    pub tokens: u32,
}

/// `compute_cache_usage` 的结果：缓存计费量 + 比例分摊所需的 estimate 口径基准。
///
/// `cache_creation` / `cache_read` 是按 `estimate_tokens` 口径算出的「被缓存覆盖
/// 前缀」的拆分；但最终上报要换算到**真实 total 口径**（contextUsage 真值或
/// `count_tokens` 估算），两个估算器尺度不同，所以这里额外带出两个 estimate 口径
/// 的基准量，供调用方做**无量纲比例分摊**：
///   - `cache_covered_est` = 被缓存覆盖前缀的 estimate token（= creation + read）
///   - `prompt_total_est`  = 整个 prompt（含最深断点之后未缓存尾部）的 estimate token
///
/// 调用方据此算 `prefix_ratio = cache_covered_est / prompt_total_est`，再乘到真实
/// total 上得到缓存覆盖部分，剩余即未缓存的 `input_tokens`，三者互斥相加 == total。
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheUsage {
    /// 缓存读取 token（estimate 口径，最深命中段累计）。
    /// creation 部分 = `cache_covered_est − cache_read`，无需单独存储。
    pub cache_read: i32,
    /// 被缓存覆盖前缀的 estimate token 总量（read + creation）。
    pub cache_covered_est: i32,
    /// 整个 prompt 的 estimate token 总量（比例分摊的分母）。
    pub prompt_total_est: i32,
}

impl CacheUsage {
    /// 按真实 total 口径做互斥分摊，返回 `(input_tokens, cache_creation, cache_read)`。
    ///
    /// `total_real` 是最终上报口径的全量 prompt token（contextUsage 真值优先，
    /// 否则 `count_tokens` 估算）。三者满足 `input + creation + read == total_real`。
    ///
    /// 无缓存覆盖（`cache_covered_est == 0`）或基准缺失时，直接返回
    /// `(total_real, 0, 0)`——全部计入 input，不凭空造缓存计数。
    pub fn split_against_total(&self, total_real: i32) -> (i32, i32, i32) {
        let total = total_real.max(0);
        if self.cache_covered_est <= 0 || self.prompt_total_est <= 0 {
            return (total, 0, 0);
        }
        // 比例无量纲，跨估算器成立；clamp 到 [0, total] 防止 estimate 偏差越界。
        let ratio = (self.cache_covered_est as f64 / self.prompt_total_est as f64).clamp(0.0, 1.0);
        let cache_total = ((total as f64) * ratio).round() as i32;
        let cache_total = cache_total.min(total);
        // 在缓存覆盖部分内部，按 estimate 口径的 read/creation 占比二次拆分。
        let read = if self.cache_covered_est > 0 {
            ((cache_total as f64) * (self.cache_read as f64 / self.cache_covered_est as f64)).round()
                as i32
        } else {
            0
        };
        let read = read.clamp(0, cache_total);
        let creation = cache_total - read;
        let input = total - cache_total;
        (input, creation, read)
    }
}

/// 进程内提示词缓存
pub struct CacheMeter {
    inner: Mutex<Inner>,
    persist_path: Option<PathBuf>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<u64, CacheEntry>,
    /// 自上次落盘后是否有变化
    dirty: bool,
}

impl CacheMeter {
    /// 创建一个空 cache。`persist_path` 为 `Some` 时会自动从该文件加载历史。
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        let mut inner = Inner::default();
        if let Some(path) = persist_path.as_ref() {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(entries) = serde_json::from_slice::<HashMap<u64, CacheEntry>>(&bytes) {
                    let now = now_secs();
                    for (k, v) in entries {
                        if v.expires_at > now {
                            inner.entries.insert(k, v);
                        }
                    }
                    tracing::info!(
                        "CacheMeter 重建：从 {} 加载 {} 条有效记录",
                        path.display(),
                        inner.entries.len()
                    );
                }
            }
        }
        Self {
            inner: Mutex::new(inner),
            persist_path,
        }
    }

    /// 查询一组前缀段哈希，返回每段命中情况；命中段会刷新 last_hit_at。
    ///
    /// `segment_hashes` 顺序必须与请求中 cache_control 断点顺序一致；
    /// `segment_tokens` 是每段累计 tokens（即 segment_hashes[i] 对应的整段累加值）。
    pub fn lookup(&self, segment_hashes: &[u64], segment_tokens: &[u32]) -> Vec<SegmentResult> {
        debug_assert_eq!(segment_hashes.len(), segment_tokens.len());
        let now = now_secs();
        let mut inner = self.inner.lock();
        let mut out = Vec::with_capacity(segment_hashes.len());
        for (h, t) in segment_hashes.iter().zip(segment_tokens.iter()) {
            let hit = match inner.entries.get_mut(h) {
                Some(entry) if entry.expires_at > now => {
                    entry.last_hit_at = now;
                    true
                }
                _ => false,
            };
            out.push(SegmentResult { hit, tokens: *t });
        }
        out
    }

    /// 把一组前缀段写入缓存（用于 miss 后登记 / 续期）。`ttl_secs` clip 到 [60, MAX_TTL_SECS]。
    pub fn record(&self, segment_hashes: &[u64], segment_tokens: &[u32], ttl_secs: i64) {
        debug_assert_eq!(segment_hashes.len(), segment_tokens.len());
        let ttl = ttl_secs.clamp(60, MAX_TTL_SECS);
        let now = now_secs();
        let expires_at = now + ttl;
        let mut inner = self.inner.lock();
        for (h, t) in segment_hashes.iter().zip(segment_tokens.iter()) {
            inner.entries.insert(
                *h,
                CacheEntry {
                    tokens: *t,
                    expires_at,
                    last_hit_at: now,
                },
            );
        }
        inner.dirty = true;
        // 容量超限：按 last_hit_at 淘汰最旧的若干条
        if inner.entries.len() > DEFAULT_CAPACITY {
            let drop_n = inner.entries.len() - DEFAULT_CAPACITY;
            let mut victims: Vec<(u64, i64)> = inner
                .entries
                .iter()
                .map(|(k, v)| (*k, v.last_hit_at))
                .collect();
            victims.sort_by_key(|x| x.1);
            for (k, _) in victims.into_iter().take(drop_n) {
                inner.entries.remove(&k);
            }
        }
    }

    /// 把当前快照写到 persist_path（仅在 dirty 时实际落盘）
    pub fn flush_to_disk(&self) {
        let path = match self.persist_path.clone() {
            Some(p) => p,
            None => return,
        };
        let snapshot = {
            let mut inner = self.inner.lock();
            if !inner.dirty {
                return;
            }
            inner.dirty = false;
            inner.entries.clone()
        };
        let json = match serde_json::to_vec(&snapshot) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("CacheMeter 序列化失败: {}", e);
                return;
            }
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, json) {
            tracing::warn!("CacheMeter 落盘失败 {}: {}", path.display(), e);
        }
    }

    /// 启动后台周期任务：定期 flush + 清理过期条目
    pub fn spawn_background(self: Arc<Self>) {
        let weak = Arc::downgrade(&self);
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(60);
            loop {
                tokio::time::sleep(interval).await;
                let Some(cache) = weak.upgrade() else { return };
                cache.evict_expired();
                cache.flush_to_disk();
            }
        });
    }

    /// 删除已过期条目（lookup 不命中过期时只是返回 miss，不会顺手清理；
    /// 这里在后台周期里清一次，避免内存膨胀）。
    pub fn evict_expired(&self) {
        let now = now_secs();
        let mut inner = self.inner.lock();
        let before = inner.entries.len();
        inner.entries.retain(|_, v| v.expires_at > now);
        if inner.entries.len() != before {
            inner.dirty = true;
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 解析 cache_control 的 ttl 字符串（"5m" / "1h"）→ 秒
pub fn parse_ttl(ttl: Option<&str>) -> i64 {
    match ttl {
        Some(s) if s.eq_ignore_ascii_case("1h") => 3600,
        Some(s) if s.eq_ignore_ascii_case("5m") => 300,
        _ => DEFAULT_TTL_SECS,
    }
}

/// `Arc<CacheMeter>` 别名
pub type SharedCacheMeter = Arc<CacheMeter>;

// ============================================================================
// 与请求体协议层的接线
// ============================================================================

use super::stream::estimate_tokens;
use super::types::{CacheControl, MessagesRequest, SystemMessage, Tool};

/// 协议层提取出来的一个"段"（segment）：从请求开头累计到本断点的所有内容。
///
/// `tokens` 是该前缀**累计**的估算 token 数；`hash` 由前缀文本的累加 SHA-256
/// 折叠得到（取低 64 位作 key，与 CacheMeter 的 u64 key 兼容）。
#[derive(Debug, Clone, Copy)]
struct Segment {
    hash: u64,
    cumulative_tokens: u32,
    /// 该段单独的 ttl（秒）
    ttl_secs: i64,
}

/// 调用 CacheMeter 计算本次请求的缓存覆盖情况，并把所有断点（含命中段）记录回
/// cache、刷新 TTL。返回 [`CacheUsage`]，由调用方在拿到真实 total 后做互斥分摊。
///
/// **完全按 Anthropic 协议**：取最深命中的段索引 i*，那么（estimate 口径）
/// - `cache_read = segments[i*].cumulative_tokens`
/// - `cache_creation = segments.last().cumulative_tokens - segments[i*].cumulative_tokens`
///
/// 全部 miss 时 cache_read = 0，cache_creation = 最深段累计 tokens。
///
/// 注意 `cache_creation` 只累计到**最深断点**为止；最深断点之后的 prompt 尾部
/// （未被任何 cache_control 覆盖）属于真 input，不计入缓存——这正是 `prompt_total_est`
/// 与 `cache_covered_est` 的差值。
///
/// 没有任何 cache_control 断点时，返回全零的 `CacheUsage`（`split_against_total`
/// 会把 total 全部计入 input）且不写入。
///
/// `key_id` 是客户端 Key id，用于会话隔离：前缀哈希会混入一个隔离种子（优先取
/// 请求 metadata 里的 session，否则退回 key_id），使不同会话 / 不同客户端 Key 的
/// 缓存互不命中——同一前缀只在同一会话内复用。
pub fn compute_cache_usage(cache: &CacheMeter, req: &MessagesRequest, key_id: u64) -> CacheUsage {
    let (segments, prompt_total_est) = extract_segments(req, key_id);
    if segments.is_empty() {
        // 无断点：仍带出 prompt_total_est 以便调用方将来扩展，但 covered=0 → 全入 input。
        return CacheUsage {
            prompt_total_est: prompt_total_est as i32,
            ..Default::default()
        };
    }

    let hashes: Vec<u64> = segments.iter().map(|s| s.hash).collect();
    let cum_tokens: Vec<u32> = segments.iter().map(|s| s.cumulative_tokens).collect();
    let results = cache.lookup(&hashes, &cum_tokens);

    // 诊断（DEBUG 级）：打印每段 hash / 累计 token / 命中情况，排查跨轮 miss。
    if tracing::enabled!(tracing::Level::DEBUG) {
        let dump: Vec<String> = segments
            .iter()
            .zip(results.iter())
            .enumerate()
            .map(|(i, (s, r))| {
                format!("[{i}] hash={} cum={} hit={}", s.hash, s.cumulative_tokens, r.hit)
            })
            .collect();
        tracing::debug!(
            "CacheMeter: {} 段, msgs={} | {}",
            segments.len(),
            req.messages.len(),
            dump.join(", ")
        );
    }

    let deepest_hit = results.iter().rposition(|r| r.hit);
    // 被缓存覆盖的前缀 = 最深断点累计（最深断点之后的尾部是未缓存的真 input）。
    // 命中时 read = 命中段累计、creation = covered − read；全 miss 时 read = 0。
    let covered = *cum_tokens.last().unwrap();
    let cache_read = match deepest_hit {
        Some(i) => cum_tokens[i],
        None => 0u32,
    };

    // 把所有段一次性写回（命中段刷新 last_hit_at；未命中段插入）。所有段共用同一
    // ttl（detect_max_ttl 的单值），单次加锁 + 单次容量检查，避免逐段重复开销。
    cache.record(&hashes, &cum_tokens, segments[0].ttl_secs);

    CacheUsage {
        cache_read: cache_read as i32,
        cache_covered_est: covered as i32,
        prompt_total_est: prompt_total_est as i32,
    }
}

/// 从请求体里按顺序提取断点段：tools → system → messages
///
/// 这个顺序与 Anthropic 拼接 prompt 的顺序对齐：tools 在最前，system 次之，
/// 然后才是 messages。每遇到一个 cache_control 断点就产生一个 Segment。
/// 累计 token 数随处理顺序累加，永远是当前位置的"前缀总量"。
///
/// 返回 `(segments, prompt_total_est)`，其中 `prompt_total_est` 是喂完整个 prompt
/// （含最深断点之后的尾部）后的 estimate token 累计，用作比例分摊的分母。
///
/// `key_id` 用于会话隔离：哈希以一个隔离种子起头（优先用 metadata session，否则
/// key_id），种子不计入 token，只让不同会话的同前缀产生不同 hash → 互不命中。
fn extract_segments(req: &MessagesRequest, key_id: u64) -> (Vec<Segment>, u32) {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut cum_tokens: u32 = 0;
    let mut segments: Vec<Segment> = Vec::new();

    // 会话隔离种子：作为哈希链最前置的输入，不进 token 估算。同一会话内前缀稳定
    // 复用；跨会话 / 跨客户端 Key 的相同前缀因种子不同而 hash 不同，互不命中。
    hasher.update(isolation_seed(req, key_id).as_bytes());

    // feed 解耦哈希与 token 估算：`hash_text` 进哈希链（决定命中），`token_text`
    // 进 token 累计（决定数值口径）。两者分离是为了让 token 计数贴近**原文**，
    // 不被签名前缀（"block:"/"tool:"）、分隔符（"|"）、role 名等噪声污染；而哈希
    // 仍用结构化签名以保持命中判定稳定。token_text 传空串即「只哈希、不计 token」。
    let feed = |hasher: &mut Sha256, hash_text: &str, token_text: &str, cum: &mut u32| {
        hasher.update(hash_text.as_bytes());
        if !token_text.is_empty() {
            *cum = cum.saturating_add(estimate_tokens(token_text).max(0) as u32);
        }
    };

    let commit = |hasher: &Sha256, cum: u32, segments: &mut Vec<Segment>, ttl_secs: i64| {
        let digest = hasher.clone().finalize();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&digest[..8]);
        let hash = u64::from_be_bytes(buf);
        segments.push(Segment {
            hash,
            cumulative_tokens: cum,
            ttl_secs,
        });
    };

    // 前缀链匹配模型（复现 Anthropic 滑动窗口缓存的"最长公共前缀命中"语义）：
    //
    // 把 prompt 的稳定前缀按 message 边界切成一条**递增前缀段链**：
    //   [tools+system] → [+msg0] → [+msg1] → ... → [+msg(n-2)]
    // 每个段的 hash 是「从头累积到该边界」的指纹，token 是该前缀的累计估算。
    // 最后一条 message（当前轮新输入）只喂进哈希算 prompt_total_est，**不切段**
    // ——它是本轮 cache_creation 的尾部，且不应被当作可复用前缀。
    //
    // 为什么这样能跨轮命中：历史消息在多轮间逐字节不变，所以 Turn N+1 的
    // [+msg_k] 段 hash 必然等于 Turn N 写入的同一个 [+msg_k] 段。lookup 取最深
    // 命中段即「最长已缓存前缀」= cache_read；其后到末段 = cache_creation。
    //
    // 旧策略（"倒数第二个 user"锚点）的致命缺陷：带 tool_result 的对话里
    // tool_result 也是 role=user，锚点每轮指向不同物理消息，前缀永不对齐，
    // 导致 cache_read 恒为 0、全部记成 creation。

    // 统一 ttl：探测整个请求里出现过的最大 cache_control.ttl，否则默认 5m。
    let ttl = detect_max_ttl(req);

    // 1. tools（全部喂入，作为前缀基础的一部分；工具定义跨轮稳定）。
    if let Some(tools) = req.tools.as_ref() {
        for t in tools {
            feed(&mut hasher, &tool_signature(t), &tool_token_text(t), &mut cum_tokens);
        }
    }

    // 2. system —— 跳过「首个带 cache_control 的 block 之前」的动态头部。
    //
    // Claude Code 在 system 数组最前面注入一个**每轮变化**的小 block（如当前
    // 时间 / session 标记），且故意**不打 cache_control**；真正稳定的大段
    // （工具说明、规则）才带 cache_control。若从该动态头开始累积哈希，整条前缀
    // 链会被它每轮污染、全部 miss——这正是实测「只创建不命中」的根因。
    //
    // 因此：当 system 中存在至少一个带 cache_control 的 block 时，跳过其之前的
    // 所有 block，从首个 cache_control 边界开始累积（对齐客户端的稳定缓存意图）。
    // 若没有任何 cache_control，则全部纳入（无从判断动态边界，保持原样）。
    if let Some(systems) = req.system.as_ref() {
        let skip_until = systems
            .iter()
            .position(|s| s.cache_control.is_some())
            .unwrap_or(0);
        for sys in systems.iter().skip(skip_until) {
            feed(&mut hasher, &system_signature(sys), &sys.text, &mut cum_tokens);
        }
    }

    // tools+system 前缀作为链的第一个段（仅当确实有内容时）。
    if cum_tokens > 0 {
        commit(&hasher, cum_tokens, &mut segments, ttl);
    }

    // 3. messages：除最后一条外，每条 message 边界切一个递增前缀段。
    let last_idx = req.messages.len().saturating_sub(1);
    for (idx, msg) in req.messages.iter().enumerate() {
        // role 进哈希（区分 user/assistant 边界），但不计入 token。
        feed(&mut hasher, &msg.role, "", &mut cum_tokens);
        match &msg.content {
            serde_json::Value::String(s) => {
                feed(&mut hasher, s, s, &mut cum_tokens);
            }
            serde_json::Value::Array(arr) => {
                // 逐 block 处理：文本块哈希用结构化签名、token 算原文；图片块哈希纳入
                // 图片数据指纹（区分不同图）、token 用 Anthropic 口径估算（(w×h)/750）。
                // 不反序列化整个 block、不 clone Value：省开销，且避免「某 block
                // 反序列化失败被跳过」造成的前缀漂移。
                for v in arr {
                    if v.get("type").and_then(|t| t.as_str()) == Some("image") {
                        // 图片：哈希喂 media_type + 数据（保证不同图 hash 不同、同图稳定），
                        // token 按真实尺寸估算后直接累加（base64 不进文本 estimate）。
                        let (media_type, data) = image_source_parts(v);
                        hasher.update(b"block:image|");
                        hasher.update(media_type.as_bytes());
                        hasher.update(b"|");
                        hasher.update(data.as_bytes());
                        let img_tokens = crate::image_resize::estimate_image_tokens(media_type, data);
                        cum_tokens = cum_tokens.saturating_add(img_tokens);
                    } else {
                        feed(
                            &mut hasher,
                            &block_signature_value(v),
                            &block_token_text(v),
                            &mut cum_tokens,
                        );
                    }
                }
            }
            _ => {}
        }
        // 最后一条不切段（当前轮新输入，属 cache_creation 尾部）。
        if idx != last_idx {
            commit(&hasher, cum_tokens, &mut segments, ttl);
        }
    }

    (segments, cum_tokens)
}

/// 生成会话隔离种子，作为前缀哈希链的最前置输入。
///
/// 优先级：
///   1. metadata.user_id 里的 session 段（Claude Code 格式含 `_session_<uuid>`）
///      —— 最精确的会话维度，同一会话多轮共享、跨会话隔离。
///   2. 退回客户端 Key id —— 至少保证不同客户端 Key 之间隔离。
///
/// 种子只参与哈希、不计入 token 估算，因此不影响 cache_creation/read 的数值口径。
fn isolation_seed(req: &MessagesRequest, key_id: u64) -> String {
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
///
/// 格式形如 `user_<hash>_account__session_<uuid>`，取 `_session_` 之后的部分。
/// 不含该标记时返回 None（交由调用方退回 key_id）。
fn extract_session_id(user_id: &str) -> Option<String> {
    user_id
        .split_once("_session_")
        .map(|(_, sid)| sid.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 探测请求里出现过的最大 cache_control.ttl（"1h" 优先于 "5m"）；
/// 无任何 cache_control 时返回默认 5m。决定写入缓存段的存活时长。
fn detect_max_ttl(req: &MessagesRequest) -> i64 {
    let mut ttl = DEFAULT_TTL_SECS;
    let mut bump = |cc: Option<&CacheControl>| {
        if let Some(cc) = cc {
            ttl = ttl.max(parse_ttl(cc.ttl.as_deref()));
        }
    };
    if let Some(tools) = req.tools.as_ref() {
        for t in tools {
            bump(t.cache_control.as_ref());
        }
    }
    if let Some(systems) = req.system.as_ref() {
        for sys in systems {
            bump(sys.cache_control.as_ref());
        }
    }
    for msg in &req.messages {
        if let serde_json::Value::Array(arr) = &msg.content {
            for v in arr {
                if let Some(t) = v
                    .get("cache_control")
                    .and_then(|cc| cc.get("ttl"))
                    .and_then(|t| t.as_str())
                {
                    ttl = ttl.max(parse_ttl(Some(t)));
                }
            }
        }
    }
    ttl
}

fn tool_signature(t: &Tool) -> String {
    // 把 name + description + input_schema 序列化为稳定文本
    let schema = serde_json::to_string(&t.input_schema).unwrap_or_default();
    format!("tool:{}|{}|{}", t.name, t.description, schema)
}

/// 工具的 token 估算原文：name + description + schema 拼接，不含签名前缀/分隔符。
/// 与 [`tool_signature`] 分离，让 token 计数贴近真实内容、不被结构标记污染。
fn tool_token_text(t: &Tool) -> String {
    let schema = serde_json::to_string(&t.input_schema).unwrap_or_default();
    format!("{} {} {}", t.name, t.description, schema)
}

fn system_signature(s: &SystemMessage) -> String {
    format!("sys:{}", s.text)
}

/// 直接从 content block 的 JSON 值算签名，只取 type/text/thinking 三个字段。
///
/// 不反序列化整个 ContentBlock、不 clone：image 的 base64、tool_use 的 input、
/// tool_result 的 content 等大字段或易变字段都不参与签名，保证前缀指纹稳定且廉价。
fn block_signature_value(v: &serde_json::Value) -> String {
    let s = |key: &str| v.get(key).and_then(|x| x.as_str()).unwrap_or("");
    format!("block:{}|{}|{}", s("type"), s("text"), s("thinking"))
}

/// content block 的 token 估算原文：仅 text + thinking 的纯文本，不含签名结构标记。
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

/// 从 image content block 的 JSON 值取 `(media_type, base64_data)`。
///
/// 兼容 base64 source（`source.type == "base64"`）；缺字段时返回空串，由调用方
/// 的图片 token 估算走保底逻辑。url 类图片无 data，返回空 data（估算保底）。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_miss_then_record_then_hit() {
        let cache = CacheMeter::new(None);
        let hashes = [1u64, 2u64];
        let tokens = [10u32, 25u32];
        let r1 = cache.lookup(&hashes, &tokens);
        assert!(r1.iter().all(|s| !s.hit));

        cache.record(&hashes, &tokens, 300);
        let r2 = cache.lookup(&hashes, &tokens);
        assert!(r2.iter().all(|s| s.hit));
    }

    #[test]
    fn ttl_expiry_makes_entry_miss() {
        let cache = CacheMeter::new(None);
        cache.record(&[42], &[100], 60);
        // 手动让条目过期
        {
            let mut inner = cache.inner.lock();
            if let Some(e) = inner.entries.get_mut(&42) {
                e.expires_at = now_secs() - 1;
            }
        }
        let r = cache.lookup(&[42], &[100]);
        assert!(!r[0].hit);
    }

    #[test]
    fn evict_expired_removes_dead_entries() {
        let cache = CacheMeter::new(None);
        cache.record(&[1, 2], &[5, 5], 60);
        {
            let mut inner = cache.inner.lock();
            for (_, v) in inner.entries.iter_mut() {
                v.expires_at = now_secs() - 1;
            }
        }
        cache.evict_expired();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn parse_ttl_handles_known_values() {
        assert_eq!(parse_ttl(Some("1h")), 3600);
        assert_eq!(parse_ttl(Some("5m")), 300);
        assert_eq!(parse_ttl(None), 300);
        assert_eq!(parse_ttl(Some("garbage")), 300);
    }

    #[test]
    fn flush_and_reload_round_trip() {
        let tmp = std::env::temp_dir().join(format!("kiro-pc-{}.json", now_secs()));
        let cache = CacheMeter::new(Some(tmp.clone()));
        cache.record(&[7], &[42], 600);
        cache.flush_to_disk();

        let cache2 = CacheMeter::new(Some(tmp.clone()));
        let r = cache2.lookup(&[7], &[42]);
        assert!(r[0].hit);

        let _ = std::fs::remove_file(&tmp);
    }

    fn build_request_with_system_breakpoint() -> super::super::types::MessagesRequest {
        use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage};
        MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::Value::String("Hello".to_string()),
            }],
            stream: false,
            system: Some(vec![SystemMessage {
                text: "You are a helpful assistant. ".repeat(100),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: None,
                }),
            }]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    #[test]
    fn compute_cache_usage_first_miss_then_hit() {
        let cache = CacheMeter::new(None);
        let req = build_request_with_system_breakpoint();

        // 第一次：所有段都 miss → 覆盖前缀全部算 creation（read == 0）。
        let u1 = compute_cache_usage(&cache, &req, 1);
        assert!(u1.cache_covered_est > 0, "first call should cover prefix");
        assert_eq!(u1.cache_read, 0, "first call has nothing cached to read");
        // 用真实 total 分摊：全部进 creation，input = total − covered。
        let total = u1.prompt_total_est; // 取 estimate total 作为「真实 total」便于断言
        let (in1, cc1, cr1) = u1.split_against_total(total);
        assert!(cc1 > 0, "first call creation>0, cc={}", cc1);
        assert_eq!(cr1, 0);
        assert_eq!(in1 + cc1 + cr1, total, "互斥口径必须自洽");

        // 第二次：相同请求 → 命中，覆盖前缀全部算 read（creation == 0）。
        let u2 = compute_cache_usage(&cache, &req, 1);
        assert!(u2.cache_read > 0, "second call should hit");
        let (in2, cc2, cr2) = u2.split_against_total(total);
        assert_eq!(cc2, 0, "second call creation should be 0, got {}", cc2);
        assert!(cr2 > 0, "second call read>0, cr={}", cr2);
        assert_eq!(in2 + cc2 + cr2, total, "互斥口径必须自洽");
        // 两次拆分的「缓存覆盖部分」一致：第一次的 creation == 第二次的 read。
        assert_eq!(cc1, cr2);
    }

    #[test]
    fn split_against_total_is_mutually_exclusive() {
        // input + creation + read 必须恒等于 total，且缓存覆盖比例正确分摊。
        let u = CacheUsage {
            cache_read: 30,
            cache_covered_est: 80, // creation 部分 = 50
            prompt_total_est: 100,
        };
        // covered 占 prompt 的 80% → 真实 total=1000 时缓存覆盖 800。
        let (input, creation, read) = u.split_against_total(1000);
        assert_eq!(input + creation + read, 1000);
        assert_eq!(input, 200, "尾部 20% 是未缓存 input");
        // 覆盖部分 800 内按 read:creation = 30:50 拆分 → read=300, creation=500。
        assert_eq!(read, 300);
        assert_eq!(creation, 500);
    }

    #[test]
    fn split_against_total_no_cache_all_input() {
        let u = CacheUsage {
            cache_read: 0,
            cache_covered_est: 0,
            prompt_total_est: 100,
        };
        assert_eq!(u.split_against_total(500), (500, 0, 0));
    }

    #[test]
    fn compute_cache_usage_single_message_no_prefix() {
        // 单条 user 消息、无 system/tools：没有可缓存的历史前缀（最后一条不切段）
        // → covered=0，total 全进 input。
        use super::super::types::{Message, MessagesRequest};
        let cache = CacheMeter::new(None);
        let req = MessagesRequest {
            model: "x".to_string(),
            max_tokens: 8,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::Value::String("Hello".to_string()),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        let u = compute_cache_usage(&cache, &req, 1);
        assert_eq!(u.cache_covered_est, 0);
        assert_eq!(u.split_against_total(123), (123, 0, 0));
    }

    /// 构造一个普通工具，input_schema 的顶层 key 按给定顺序插入。
    /// 用于验证：无论插入顺序如何，tool_signature 都稳定（BTreeMap 保证）。
    fn build_tool_with_schema_order(insert_required_first: bool) -> super::super::types::Tool {
        use super::super::types::Tool;
        let mut schema = std::collections::BTreeMap::new();
        // 故意用不同的插入顺序，模拟上游 JSON 解析的不确定迭代序。
        if insert_required_first {
            schema.insert("required".to_string(), serde_json::json!([]));
            schema.insert("properties".to_string(), serde_json::json!({}));
            schema.insert("type".to_string(), serde_json::json!("object"));
        } else {
            schema.insert("type".to_string(), serde_json::json!("object"));
            schema.insert("properties".to_string(), serde_json::json!({}));
            schema.insert("required".to_string(), serde_json::json!([]));
        }
        Tool {
            tool_type: None,
            name: "my_tool".to_string(),
            description: "desc".to_string(),
            input_schema: schema,
            max_uses: None,
            cache_control: None,
        }
    }

    #[test]
    fn tool_signature_stable_across_insert_order() {
        let a = build_tool_with_schema_order(true);
        let b = build_tool_with_schema_order(false);
        // 逻辑等价、插入顺序不同的 schema 必须产生相同签名，
        // 否则 tools 段 hash 抖动会让后续 system/messages 断点连锁 miss。
        assert_eq!(tool_signature(&a), tool_signature(&b));
    }

    #[test]
    fn compute_cache_usage_tools_hit_regardless_of_schema_order() {        use super::super::types::{CacheControl, Message, MessagesRequest};

        let make_req = |insert_required_first: bool| {
            let mut tool = build_tool_with_schema_order(insert_required_first);
            tool.cache_control = Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            });
            MessagesRequest {
                model: "claude-sonnet-4-5-20250929".to_string(),
                max_tokens: 32,
                messages: vec![Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String("Hello".to_string()),
                }],
                stream: false,
                system: None,
                tools: Some(vec![tool]),
                tool_choice: None,
                thinking: None,
                output_config: None,
                metadata: None,
            }
        };

        let cache = CacheMeter::new(None);
        // 第一次：用一种插入顺序，应写缓存（miss → read==0）。
        let u1 = compute_cache_usage(&cache, &make_req(false), 1);
        assert!(u1.cache_covered_est > 0, "first call should cover prefix");
        assert_eq!(u1.cache_read, 0);

        // 第二次：换一种插入顺序但逻辑等价，应命中缓存（read 等于第一次覆盖前缀）。
        let u2 = compute_cache_usage(&cache, &make_req(true), 1);
        assert_eq!(
            u2.cache_read, u1.cache_covered_est,
            "schema 顺序不应影响命中：second read 应等于 first covered"
        );
    }

    /// 构造一条带 cache_control 的 user/assistant 文本消息。
    fn msg_with_cc(role: &str, text: &str, with_cc: bool) -> super::super::types::Message {
        use super::super::types::Message;
        let block = if with_cc {
            serde_json::json!({
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"}
            })
        } else {
            serde_json::json!({"type": "text", "text": text})
        };
        Message {
            role: role.to_string(),
            content: serde_json::Value::Array(vec![block]),
        }
    }

    fn req_with_messages(messages: Vec<super::super::types::Message>) -> super::super::types::MessagesRequest {
        use super::super::types::MessagesRequest;
        MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages,
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    /// 模拟 Claude Code 真实工具调用序列：tool_use(assistant) / tool_result(user)
    /// 块每轮回传时带每次新生成的 id。验证前缀链对「含 id 漂移的工具块」仍能命中。
    #[test]
    fn tool_call_history_still_hits_despite_id_drift() {
        let body = "analyze the repository structure carefully ".repeat(15);
        // assistant 轮：一个 tool_use 块，input 是工具参数，id 每轮可能不同。
        let assistant_tool = |id: &str| {
            use super::super::types::Message;
            Message {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type": "text", "text": body},
                    {"type": "tool_use", "id": id, "name": "bash", "input": {"cmd": "ls"}}
                ]),
            }
        };
        // user 轮：tool_result 块，tool_use_id 对应上面的 id。
        let user_result = |id: &str| {
            use super::super::types::Message;
            Message {
                role: "user".to_string(),
                content: serde_json::json!([
                    {"type": "tool_result", "tool_use_id": id, "content": body}
                ]),
            }
        };
        let user_text = |t: &str| msg_with_cc("user", t, false);

        let cache = CacheMeter::new(None);
        // Turn 1: user → assistant(tool_use #a) → user(tool_result #a) → assistant(text) → user(新问题)
        let turn1 = req_with_messages(vec![
            user_text(&body),
            assistant_tool("toolu_aaa"),
            user_result("toolu_aaa"),
            msg_with_cc("assistant", &body, false),
            user_text("next question one"),
        ]);
        let u1 = compute_cache_usage(&cache, &turn1, 1);
        assert!(u1.cache_covered_est > 0);
        assert_eq!(u1.cache_read, 0, "turn1 无历史可命中");

        // Turn 2: 追加 assistant(text) + user(新问题)。前 5 条历史逐字节不变。
        let turn2 = req_with_messages(vec![
            user_text(&body),
            assistant_tool("toolu_aaa"),
            user_result("toolu_aaa"),
            msg_with_cc("assistant", &body, false),
            user_text("next question one"),
            msg_with_cc("assistant", &body, false),
            user_text("next question two"),
        ]);
        let u2 = compute_cache_usage(&cache, &turn2, 1);
        assert!(
            u2.cache_read > 0,
            "turn2 应命中 turn1 的历史前缀（即便工具块带 id）"
        );
        assert_eq!(
            u2.cache_read, u1.cache_covered_est,
            "命中的最深前缀应等于上一轮 covered"
        );
    }

    #[test]
    fn multi_turn_prefix_chain_produces_read_hit() {
        // 前缀链模型：turn4 在 turn3 基础上追加 a/u 一对，历史前缀逐字节不变，
        // 所以 turn4 应命中 turn3 写入的最深历史前缀段（cache_read > 0）。
        let cache = CacheMeter::new(None);
        let body = "the quick brown fox jumps over the lazy dog ".repeat(20);

        // 第 3 轮：u,a,u,a,u（5 条）。切段：除最后一条外，每条 message 一个前缀段
        // → idx 0,1,2,3 共 4 个段（无 system/tools）。
        let turn3 = req_with_messages(vec![
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, true),
        ]);
        let u3 = compute_cache_usage(&cache, &turn3, 1);
        assert!(u3.cache_covered_est > 0, "turn3 should create cache");
        assert_eq!(u3.cache_read, 0, "turn3 has no prior cache to read");

        // 第 4 轮：追加 a3,u4（7 条）。历史 idx 0..=5 切段，最后一条 idx6 不切。
        // turn3 的最深段在 idx3（其前缀=u,a,u,a），turn4 的 idx3 段前缀逐字节相同
        // → 命中。turn4 还新增 idx4,5 两个更深的历史前缀段。
        let turn4 = req_with_messages(vec![
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, true),
        ]);
        let u4 = compute_cache_usage(&cache, &turn4, 1);
        assert!(u4.cache_read > 0, "turn4 should hit a prior-turn prefix");
        // turn4 命中的最深前缀 = turn3 的最深段（idx3 前缀，即 turn3 的 covered）。
        assert_eq!(
            u4.cache_read, u3.cache_covered_est,
            "read 应等于上一轮写入的最深历史前缀"
        );
        // turn4 覆盖前缀更深（新增历史段）→ creation 部分 > 0。
        assert!(
            u4.cache_covered_est > u4.cache_read,
            "turn4 仍会为新增的历史前缀创建缓存"
        );
    }

    #[test]
    fn prefix_chain_works_without_any_cache_control() {
        // 新模型不依赖 cache_control：只要有跨轮稳定的历史前缀就能命中。
        // 这复现 Anthropic 自动前缀缓存语义，与旧"必须有 cache_control"策略不同。
        let cache = CacheMeter::new(None);
        let body = "lorem ipsum dolor sit amet ".repeat(20);
        let turn1 = req_with_messages(vec![
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
        ]);
        let u1 = compute_cache_usage(&cache, &turn1, 1);
        assert!(u1.cache_covered_est > 0, "应为历史前缀创建缓存段");
        assert_eq!(u1.cache_read, 0);

        let turn2 = req_with_messages(vec![
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
            msg_with_cc("assistant", &body, false),
            msg_with_cc("user", &body, false),
        ]);
        let u2 = compute_cache_usage(&cache, &turn2, 1);
        assert!(u2.cache_read > 0, "无 cache_control 也应跨轮命中历史前缀");
    }

    /// 复现实测根因：system[0] 是每轮变化的动态头（无 cache_control），
    /// 其后是带 cache_control 的稳定大块。跳过动态头后，稳定前缀应跨轮命中。
    #[test]
    fn dynamic_system_header_does_not_break_cache_hit() {
        use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage};
        let stable_sys = "You are a coding assistant. ".repeat(200);
        let body = "implement the feature step by step ".repeat(15);

        let make_req = |dyn_header: &str, msgs: Vec<Message>| MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 64,
            messages: msgs,
            stream: false,
            system: Some(vec![
                // sys[0]：每轮变化的动态头（如当前时间），无 cache_control。
                SystemMessage {
                    text: dyn_header.to_string(),
                    cache_control: None,
                },
                // sys[1]：稳定大块，带 cache_control。
                SystemMessage {
                    text: stable_sys.clone(),
                    cache_control: Some(CacheControl {
                        cache_type: "ephemeral".to_string(),
                        ttl: None,
                    }),
                },
            ]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let cache = CacheMeter::new(None);
        // Turn 1：动态头 = "now=1001"，3 条消息。
        let u1 = compute_cache_usage(
            &cache,
            &make_req(
                "now=1001",
                vec![
                    msg_with_cc("user", &body, false),
                    msg_with_cc("assistant", &body, false),
                    msg_with_cc("user", &body, false),
                ],
            ),
            1,
        );
        assert!(u1.cache_covered_est > 0);
        assert_eq!(u1.cache_read, 0, "turn1 无历史可命中");

        // Turn 2：动态头变成 "now=2002"（不同！），追加一对 a/u。
        // 跳过动态头后，sys[1]+历史前缀逐字节不变 → 必须命中。
        let u2 = compute_cache_usage(
            &cache,
            &make_req(
                "now=2002",
                vec![
                    msg_with_cc("user", &body, false),
                    msg_with_cc("assistant", &body, false),
                    msg_with_cc("user", &body, false),
                    msg_with_cc("assistant", &body, false),
                    msg_with_cc("user", &body, false),
                ],
            ),
            1,
        );
        assert!(
            u2.cache_read > 0,
            "动态 system 头变化不应破坏稳定前缀命中（实测根因）"
        );
    }

    /// 会话隔离：相同前缀内容，不同客户端 Key（key_id）之间不应互相命中。
    #[test]
    fn different_key_id_does_not_cross_hit() {
        let cache = CacheMeter::new(None);
        let body = "shared system prompt and history ".repeat(20);
        let msgs = || {
            vec![
                msg_with_cc("user", &body, false),
                msg_with_cc("assistant", &body, false),
                msg_with_cc("user", &body, false),
            ]
        };
        // Key=1 建立缓存。
        let a = compute_cache_usage(&cache, &req_with_messages(msgs()), 1);
        assert!(a.cache_covered_est > 0);
        assert_eq!(a.cache_read, 0);
        // Key=2 相同内容，但隔离种子不同 → 不命中（视为新建）。
        let b = compute_cache_usage(&cache, &req_with_messages(msgs()), 2);
        assert_eq!(b.cache_read, 0, "不同 key_id 不应命中彼此的前缀");
        // Key=1 再来一次相同内容 → 命中自己上次写入的。
        let c = compute_cache_usage(&cache, &req_with_messages(msgs()), 1);
        assert!(c.cache_read > 0, "同一 key_id 应命中自己的前缀");
    }

    /// 会话隔离：metadata.user_id 里 session 不同 → 不命中；session 相同 → 命中。
    #[test]
    fn metadata_session_scopes_cache() {
        use super::super::types::{Message, MessagesRequest, Metadata};
        let body = "conversation prefix that stays stable ".repeat(20);
        let make = |session: &str| MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 64,
            messages: vec![
                Message { role: "user".into(), content: serde_json::json!([{"type":"text","text":body}]) },
                Message { role: "assistant".into(), content: serde_json::json!([{"type":"text","text":body}]) },
                Message { role: "user".into(), content: serde_json::json!([{"type":"text","text":body}]) },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: Some(Metadata {
                user_id: Some(format!("user_abc_account__session_{session}")),
            }),
        };
        let cache = CacheMeter::new(None);
        // 同 key_id（都为 0），仅 session 不同——靠 metadata session 隔离。
        let s1a = compute_cache_usage(&cache, &make("aaa"), 0);
        assert_eq!(s1a.cache_read, 0);
        let s2 = compute_cache_usage(&cache, &make("bbb"), 0);
        assert_eq!(s2.cache_read, 0, "不同 session 不应命中");
        let s1b = compute_cache_usage(&cache, &make("aaa"), 0);
        assert!(s1b.cache_read > 0, "相同 session 应命中");
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

    /// token 口径纯净性：cum_tokens 只算原文，不含 role / 签名前缀 / 分隔符噪声。
    #[test]
    fn token_count_excludes_signature_noise() {
        use super::super::types::{Message, MessagesRequest};
        // 两条消息：第一条是历史（切段），内容为已知纯文本；最后一条占位（不切段）。
        let history_text = "the quick brown fox jumps over the lazy dog";
        let req = MessagesRequest {
            model: "m".to_string(),
            max_tokens: 8,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!([{"type": "text", "text": history_text}]),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String("ok".to_string()),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        let u = compute_cache_usage(&CacheMeter::new(None), &req, 1);
        // 历史段（第一条）的 covered 应严格等于纯文本 estimate——
        // 不含 "user" role、"block:" 前缀、"|" 分隔符的任何 token。
        let pure = estimate_tokens(history_text) as i32;
        assert_eq!(
            u.cache_covered_est, pure,
            "covered 应只算原文 token，实测 {} vs 纯文本 {}",
            u.cache_covered_est, pure
        );
    }

    /// 含图片的历史段：covered 应计入图片的 Anthropic 口径 token，且跨轮稳定命中。
    #[test]
    fn image_block_contributes_tokens_and_hits() {
        use super::super::types::{Message, MessagesRequest};
        // 用 image_resize 的同款 PNG 生成器造一张 750×750（≈750 token）的真图。
        let png = make_test_png(750, 750);
        let img_tokens = crate::image_resize::estimate_image_tokens("image/png", &png) as i32;
        assert!(img_tokens > 100, "前提：测试图应有可观 token，实测 {img_tokens}");

        let make = |trailing: &str| MessagesRequest {
            model: "m".to_string(),
            max_tokens: 8,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data": png}},
                        {"type":"text","text":"describe"}
                    ]),
                },
                Message { role: "assistant".to_string(), content: serde_json::json!("a pixel") },
                Message { role: "user".to_string(), content: serde_json::json!(trailing) },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let cache = CacheMeter::new(None);
        // Turn 1：含图的 user 是历史第一段，其 covered 必须包含图片 token。
        let u1 = compute_cache_usage(&cache, &make("q1"), 1);
        let text_only = estimate_tokens("describe") as i32;
        // 最深历史段至少覆盖到 [含图user] 段，covered 应 ≥ 图片 token（远大于纯文本）。
        assert!(
            u1.cache_covered_est >= img_tokens + text_only - 5,
            "covered({}) 应含图片 token({})", u1.cache_covered_est, img_tokens
        );
        assert_eq!(u1.cache_read, 0);

        // Turn 2：追加一轮，含图历史逐字节不变 → 命中（read 含图片 token）。
        let u2 = compute_cache_usage(&cache, &make("q2"), 1);
        assert!(
            u2.cache_read >= img_tokens,
            "含图历史应跨轮命中且 read({}) 含图片 token({})", u2.cache_read, img_tokens
        );
    }

    /// 测试用 PNG 生成器（与 image_resize 测试同款，渐变填充更接近真实压缩比）。
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
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png).unwrap();
        B64.encode(&buf)
    }
}
