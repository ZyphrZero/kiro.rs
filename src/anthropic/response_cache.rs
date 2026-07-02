//! 中转层「响应缓存」（无外部依赖）
//!
//! 与 [`super::cache_metering`]（只**模拟** cache_creation/cache_read 的 token 计量）
//! 不同，本模块缓存**真实的响应体**：对同一请求（同会话、同 model、同 messages、同 tools）
//! 命中时直接回放上次的完整响应，**完全跳过上游调用**。参考 kiro-account-manager 的
//! `gateway/response_cache.rs` 设计，但裁剪为单层 LRU + TTL：
//!
//! - 键 = `sha256(isolation_seed || model || messages_json || tools_json)`。
//!   `isolation_seed` 复用 cache_metering 的口径（优先 metadata session，否则 key_id），
//!   保证不同会话 / 不同客户端 Key 之间互不命中。
//! - 值 = 已组装好的、可直接下发给客户端的字节（JSON 或 SSE 事件流文本）+ content-type 标记。
//! - TTL：每条 `expires_at`，过期即 miss（lookup 顺手删 + 后台周期清理）。
//! - 容量：表满按 `last_hit_at` LRU 淘汰，沿用 cache_metering 的手写 LRU 思路，
//!   不引入 `lru` crate（与本 crate「缓存层无外部依赖」约定一致）。
//!
//! **只缓存「干净的终态文本响应」**：`tool_use` / 出错 / 中途断流 / 非 `end_turn` 的响应
//! 一律不写入（详见 handler 侧的 `should_cache_*`）。这样命中回放永远是一段自洽、可重放的
//! 完整响应，不会把带 tool_use_id 的中间态错误地跨会话复用。

use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::types::MessagesRequest;

/// 默认 TTL（秒），与 KAM `summary_cache_max_age_seconds` 对齐。
pub const DEFAULT_TTL_SECS: u64 = 180;
/// 默认条目容量上限。每条值可能是数十 KB 的响应体，故默认远小于 cache_metering。
pub const DEFAULT_CAPACITY: usize = 1024;
/// 容量下限（clamp），避免配置成过小值导致缓存无意义地频繁淘汰。
const MIN_CAPACITY: usize = 16;

fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 单条缓存的响应。
#[derive(Clone)]
pub struct CachedResponse {
    /// 可直接写入 HTTP body 的完整字节（JSON 响应体 或 SSE 事件流文本）。
    pub body: Vec<u8>,
    /// true = `text/event-stream`（流式回放）；false = `application/json`（非流式）。
    pub is_sse: bool,
    /// 过期时间戳（unix 秒）。
    expires_at: u64,
    /// 上次访问的单调序号（LRU 淘汰用）。用单调计数器而非秒级时间戳，
    /// 避免同一秒内插入/命中的多条 last-hit 相等、无法区分最旧的问题。
    last_seq: u64,
}

impl CachedResponse {
    fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }
}

struct Inner {
    entries: HashMap<String, CachedResponse>,
    /// 单调递增的访问序号发生器（每次 put / 命中的 get 自增）。
    seq: u64,
}

/// 进程内响应体缓存（单层 LRU + TTL）。
pub struct ResponseCache {
    inner: Mutex<Inner>,
    capacity: usize,
    /// 全局默认开关（运行时可经 Admin API 改）。per-key 覆盖优先于此值。
    default_enabled: AtomicBool,
    /// 全局默认 TTL 秒（运行时可经 Admin API 改）。per-key ttl>0 时覆盖此值。
    default_ttl_secs: AtomicU64,
}

impl ResponseCache {
    /// 创建空缓存。`capacity` 会被 clamp 到 `>= MIN_CAPACITY`。
    /// `default_enabled` / `default_ttl_secs` 为全局默认（来自 config，运行时可改）。
    pub fn new(capacity: usize, default_enabled: bool, default_ttl_secs: u64) -> Self {
        let ttl = if default_ttl_secs == 0 {
            DEFAULT_TTL_SECS
        } else {
            default_ttl_secs
        };
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                seq: 0,
            }),
            capacity: capacity.max(MIN_CAPACITY),
            default_enabled: AtomicBool::new(default_enabled),
            default_ttl_secs: AtomicU64::new(ttl),
        }
    }

    /// 计算缓存键：`hex(sha256(isolation_seed || model || stream || messages_json || tools_json))`。
    ///
    /// 在请求**转换/裁剪之前**用原始 `MessagesRequest` 计算，使键反映客户端真正发的内容；
    /// `isolation_seed` 复用 cache_metering 的会话隔离口径。序列化失败时退回空串参与哈希
    /// （极少见；只会降低命中率，不会错误命中）。
    ///
    /// `stream` 纳入键：流式（SSE）与非流式（JSON）响应体格式不同，命中回放时按 `is_sse`
    /// 重建 Content-Type。若键不含 stream，`stream=false` 请求可能命中流式写入的 SSE 条目、
    /// 收到 `text/event-stream` 而非期待的 `application/json`，导致客户端解析失败。
    pub fn compute_key(req: &MessagesRequest, key_id: u64) -> String {
        let seed = super::cache_metering::isolation_seed(req, key_id);
        let messages_json = serde_json::to_string(&req.messages).unwrap_or_default();
        let tools_json = serde_json::to_string(&req.tools).unwrap_or_default();

        let mut h = Sha256::new();
        h.update(seed.as_bytes());
        h.update(b"\x00");
        h.update(req.model.as_bytes());
        h.update(b"\x00");
        h.update(if req.stream { b"s\x00" } else { b"j\x00" });
        h.update(messages_json.as_bytes());
        h.update(b"\x00");
        h.update(tools_json.as_bytes());
        hex::encode(h.finalize())
    }

    /// 查询。命中且未过期 → 返回克隆并刷新访问序号；过期 → 顺手删除并返回 None。
    pub fn get(&self, key: &str) -> Option<CachedResponse> {
        let now = now_secs();
        let mut inner = self.inner.lock();
        let next_seq = inner.seq.wrapping_add(1);
        match inner.entries.get_mut(key) {
            Some(entry) if !entry.is_expired(now) => {
                entry.last_seq = next_seq;
                let cloned = entry.clone();
                inner.seq = next_seq;
                Some(cloned)
            }
            Some(_) => {
                inner.entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// 写入。`ttl_secs` 为 0 时退回 [`DEFAULT_TTL_SECS`]。写入后若超容量按访问序号淘汰最旧的若干条。
    pub fn put(&self, key: String, body: Vec<u8>, is_sse: bool, ttl_secs: u64) {
        let ttl = if ttl_secs == 0 {
            DEFAULT_TTL_SECS
        } else {
            ttl_secs
        };
        let now = now_secs();
        let mut inner = self.inner.lock();
        let next_seq = inner.seq.wrapping_add(1);
        inner.seq = next_seq;
        let entry = CachedResponse {
            body,
            is_sse,
            expires_at: now.saturating_add(ttl),
            last_seq: next_seq,
        };
        inner.entries.insert(key, entry);
        self.evict_over_capacity(&mut inner);
    }

    /// 容量超限时按访问序号升序淘汰最旧的若干条。
    fn evict_over_capacity(&self, inner: &mut Inner) {
        if inner.entries.len() <= self.capacity {
            return;
        }
        let drop_n = inner.entries.len() - self.capacity;
        let mut victims: Vec<(String, u64)> = inner
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.last_seq))
            .collect();
        victims.sort_by_key(|(_, seq)| *seq);
        for (k, _) in victims.into_iter().take(drop_n) {
            inner.entries.remove(&k);
        }
    }

    /// 删除已过期条目（后台周期任务调用，避免内存膨胀）。
    pub fn evict_expired(&self) {
        let now = now_secs();
        let mut inner = self.inner.lock();
        inner.entries.retain(|_, v| !v.is_expired(now));
    }

    /// 启动后台周期任务：每 60s 清理过期条目。持 Weak，缓存被释放即自动退出。
    pub fn spawn_background(self: Arc<Self>) {
        let weak = Arc::downgrade(&self);
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(60);
            loop {
                tokio::time::sleep(interval).await;
                let Some(cache) = weak.upgrade() else { return };
                cache.evict_expired();
            }
        });
    }

    /// 全局默认开关（运行时值）。
    pub fn default_enabled(&self) -> bool {
        self.default_enabled.load(Ordering::Relaxed)
    }

    /// 全局默认 TTL 秒（运行时值）。
    pub fn default_ttl_secs(&self) -> u64 {
        self.default_ttl_secs.load(Ordering::Relaxed)
    }

    /// 运行时更新全局默认开关（Admin API）。
    pub fn set_default_enabled(&self, enabled: bool) {
        self.default_enabled.store(enabled, Ordering::Relaxed);
    }

    /// 运行时更新全局默认 TTL 秒（Admin API）。`secs=0` 退回 [`DEFAULT_TTL_SECS`]。
    pub fn set_default_ttl_secs(&self, secs: u64) {
        let ttl = if secs == 0 { DEFAULT_TTL_SECS } else { secs };
        self.default_ttl_secs.store(ttl, Ordering::Relaxed);
    }

    /// 解析「该 Key 生效的响应缓存配置」：per-key 覆盖优先，否则回退本缓存的全局默认（运行时值）。
    /// 返回 `(enabled, ttl_secs)`；`enabled=false` 时调用方直接跳过查询/写入。
    pub fn effective_config(&self, key_enabled: Option<bool>, key_ttl_secs: Option<u32>) -> (bool, u64) {
        effective_cache_config(
            key_enabled,
            key_ttl_secs,
            self.default_enabled(),
            self.default_ttl_secs(),
        )
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }
}

/// `Arc<ResponseCache>` 别名。
pub type SharedResponseCache = Arc<ResponseCache>;

/// 解析「该 Key 生效的响应缓存配置」：per-key 覆盖优先，否则回退全局默认。
///
/// 返回 `(enabled, ttl_secs)`。`enabled=false` 时调用方直接跳过缓存查询/写入。
pub fn effective_cache_config(
    key_enabled: Option<bool>,
    key_ttl_secs: Option<u32>,
    global_enabled: bool,
    global_ttl_secs: u64,
) -> (bool, u64) {
    let enabled = key_enabled.unwrap_or(global_enabled);
    let ttl = key_ttl_secs
        .map(|v| v as u64)
        .filter(|v| *v > 0)
        .unwrap_or(global_ttl_secs);
    (enabled, ttl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::{Message, MessagesRequest};

    fn req_with(model: &str, text: &str) -> MessagesRequest {
        MessagesRequest {
            model: model.to_string(),
            max_tokens: 32,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::Value::String(text.to_string()),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        }
    }

    #[test]
    fn put_get_roundtrip() {
        let cache = ResponseCache::new(64, true, 180);
        cache.put("k1".to_string(), b"hello".to_vec(), false, 180);
        let got = cache.get("k1").expect("should hit");
        assert_eq!(got.body, b"hello");
        assert!(!got.is_sse);
    }

    #[test]
    fn miss_on_unknown_key() {
        let cache = ResponseCache::new(64, true, 180);
        assert!(cache.get("nope").is_none());
    }

    #[test]
    fn expired_entry_is_evicted_on_get() {
        let cache = ResponseCache::new(64, true, 180);
        // ttl=0 → DEFAULT_TTL, so force expiry by inserting a manually-expired entry.
        cache.put("k1".to_string(), b"x".to_vec(), false, 1);
        {
            let mut inner = cache.inner.lock();
            inner.entries.get_mut("k1").unwrap().expires_at = 0;
        }
        assert!(cache.get("k1").is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn lru_evicts_least_recently_hit() {
        let cache = ResponseCache::new(MIN_CAPACITY, true, 180);
        for i in 0..MIN_CAPACITY {
            cache.put(format!("k{i}"), vec![i as u8], false, 180);
        }
        // Touch k0 so it's the most-recently-hit.
        assert!(cache.get("k0").is_some());
        // Insert one more → triggers eviction of the oldest last_hit (k1, not k0).
        cache.put("overflow".to_string(), vec![1], false, 180);
        assert_eq!(cache.len(), MIN_CAPACITY);
        assert!(cache.get("k0").is_some(), "recently-hit key must survive");
    }

    #[test]
    fn same_request_same_key() {
        let a = ResponseCache::compute_key(&req_with("claude-opus-4-8", "hi"), 1);
        let b = ResponseCache::compute_key(&req_with("claude-opus-4-8", "hi"), 1);
        assert_eq!(a, b);
    }

    #[test]
    fn different_key_id_different_cache_key() {
        let a = ResponseCache::compute_key(&req_with("claude-opus-4-8", "hi"), 1);
        let b = ResponseCache::compute_key(&req_with("claude-opus-4-8", "hi"), 2);
        assert_ne!(a, b, "different client keys must not collide");
    }

    #[test]
    fn stream_flag_changes_cache_key() {
        // 回归：流式与非流式响应体格式不同（SSE vs JSON），不可共用同一缓存条目，
        // 否则 stream=false 请求可能命中 SSE 条目、收到错误的 Content-Type。
        let mut non_stream = req_with("claude-opus-4-8", "hi");
        non_stream.stream = false;
        let mut stream = req_with("claude-opus-4-8", "hi");
        stream.stream = true;
        assert_ne!(
            ResponseCache::compute_key(&non_stream, 1),
            ResponseCache::compute_key(&stream, 1),
            "stream 与非 stream 必须产生不同缓存键"
        );
    }

    #[test]
    fn different_content_different_cache_key() {
        let a = ResponseCache::compute_key(&req_with("claude-opus-4-8", "hi"), 1);
        let b = ResponseCache::compute_key(&req_with("claude-opus-4-8", "bye"), 1);
        assert_ne!(a, b);
    }

    #[test]
    fn effective_config_per_key_overrides_global() {
        // per-key enable=Some(false) overrides global true
        assert_eq!(
            effective_cache_config(Some(false), None, true, 180),
            (false, 180)
        );
        // per-key ttl overrides global
        assert_eq!(
            effective_cache_config(Some(true), Some(60), false, 180),
            (true, 60)
        );
        // both None → follow global
        assert_eq!(effective_cache_config(None, None, true, 200), (true, 200));
        // per-key ttl=0 ignored → global ttl
        assert_eq!(
            effective_cache_config(Some(true), Some(0), true, 180),
            (true, 180)
        );
    }
}
