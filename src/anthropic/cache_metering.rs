//! 中转层 prompt cache 本地计量模拟（无外部依赖）
//!
//! # 适用范围与边界说明
//! 本实现仅为中转层本地计量模拟，用于在缺少上游 provider usage 事件时，遵循
//! Anthropic 官方 Prompt Caching 规范进行合理回退估算。
//! **本模拟不会实际减少 Kiro 上游的推理开销**，亦不代表 Anthropic 官方对任何工作负载
//! 承诺固定的缓存命中率（命中率由实际提示词结构与断点声明自然决定）。
//!
//! # 官方 Prompt Caching 语义对齐
//! 1. **断点声明模式**：
//!    - **顶层 auto-caching (`MessagesRequest.cache_control`)**：自动缓存开启，
//!      断点自动落在整个 prompt 的最后一个可缓存 block，随多轮对话向前移动
//!      （首轮写入最后一条 User，次轮从上一轮最后一条 User 读取）。自动断点占用 1 个断点槽。
//!    - **显式 block 级缓存 (`cache_control`)**：仅在标记了 cache_control 的 block
//!      处写入缓存 entry。
//!    - **无 cache_control**：若无顶层且无任何 block 级 cache_control，则不模拟任何缓存，
//!      全部计入 uncached input。
//! 2. **20-Block Lookback 回溯查找**：
//!    - 读取匹配时，从每个断点向后（回溯）最多检查 20 个 block，寻找此前真正写入过的
//!      最长前缀断点。只能命中此前实际写入过的断点，未声明断点的中间 block 不会写入或命中。
//!    - 连续的 `tool_use` block 或连续的 `tool_result` block 各按一个回溯位置计算。
//! 3. **TTL 与滑动续期**：
//!    - 默认 ephemeral TTL 为 300 秒（5 分钟）；显式 `ttl=\"1h\"` 为 3600 秒（1 小时）。
//!    - 每个 entry 独立保存自身 TTL。命中时按该 entry 自身的 TTL 从请求时间起滑动续期。
//!    - 混合 TTL 规则：1h 断点必须出现在 5m 断点之前；非法顺序不模拟缓存。
//! 4. **断点上限与守恒**：
//!    - 请求最多支持 4 个断点；超限请求不模拟缓存。
//!    - Token 计量守恒：`input + cache_creation + cache_read == total`。

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// 默认条目上限（防止内存无限增长）
const DEFAULT_CAPACITY: usize = 4096;
/// 最长 TTL（1h，与 Anthropic ttl=\"1h\" 对齐）
const MAX_TTL_SECS: i64 = 3600;
/// 默认 TTL（5min，ephemeral 默认值）
const DEFAULT_TTL_SECS: i64 = 5 * 60;
/// 最大断点数量
const MAX_BREAKPOINTS: usize = 4;
/// 回溯查找最大 block 数（20-block lookback）
const LOOKBACK_BLOCKS: usize = 20;

/// 单个缓存条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// 该前缀段累计的估算 token 数
    pub tokens: u32,
    /// 自身 TTL（秒），用于命中时按自身 TTL 进行滑动续期
    #[serde(default = "default_entry_ttl")]
    pub ttl_secs: i64,
    /// 过期时间戳（unix 秒）
    pub expires_at: i64,
    /// 上次命中时间（用于 LRU 淘汰）
    pub last_hit_at: i64,
}

fn default_entry_ttl() -> i64 {
    DEFAULT_TTL_SECS
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
            ((cache_total as f64) * (self.cache_read as f64 / self.cache_covered_est as f64))
                .round() as i32
        } else {
            0
        };
        let read = read.clamp(0, cache_total);
        let creation = cache_total - read;
        let input = total - cache_total;
        (input, creation, read)
    }
}

/// 异步装箱 Future 别名
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 远端/共享 Prompt Cache 存储抽象（用于 Redis 共享后端及单元测试 Mock）
pub trait RemoteCacheStore: Send + Sync {
    /// 按哈希查找并按 entry 自身 TTL 滑动续期。成功命中返回 Some(tokens)，未命中或失败返回 None
    fn lookup_and_renew<'a>(&'a self, hash: u64) -> BoxFuture<'a, Option<u32>>;

    /// 按哈希写入断点条目并设置 TTL
    fn record_entry<'a>(&'a self, hash: u64, tokens: u32, ttl_secs: i64) -> BoxFuture<'a, ()>;

    /// 多实例并发冷启动同一断点时的轻量去重短锁（best-effort），加锁失败直接返回 false，不阻塞调用方
    fn try_acquire_lock<'a>(&'a self, hash: u64, ttl_ms: u64) -> BoxFuture<'a, bool>;
}

/// 生成 Redis 断点元数据 Key（带版本与命名空间隔离）
pub fn redis_entry_key(hash: u64) -> String {
    format!("kiro:pcm:v1:entry:{:016x}", hash)
}

/// 生成 Redis 去重短锁 Key
pub fn redis_lock_key(hash: u64) -> String {
    format!("kiro:pcm:v1:lock:{:016x}", hash)
}

/// 脱敏 Redis 连接 URL（隐藏密码与认证信息）
pub fn sanitize_redis_url(raw: &str) -> String {
    if let Some((scheme, rest)) = raw.split_once("://") {
        if let Some((_auth_part, host_part)) = rest.split_once('@') {
            return format!("{}://***@{}", scheme, host_part);
        }
    }
    raw.to_string()
}

/// 限频日志记录器（避免 Redis 异常时高频刷屏）
#[derive(Debug)]
pub struct ThrottledLogger {
    last_logged_secs: std::sync::atomic::AtomicI64,
    interval_secs: i64,
}

impl ThrottledLogger {
    pub const fn new(interval_secs: i64) -> Self {
        Self {
            last_logged_secs: std::sync::atomic::AtomicI64::new(0),
            interval_secs,
        }
    }

    pub fn warn(&self, action: &str, err: &dyn std::fmt::Display) {
        let now = now_secs();
        let last = self
            .last_logged_secs
            .load(std::sync::atomic::Ordering::Relaxed);
        if now - last >= self.interval_secs {
            if self
                .last_logged_secs
                .compare_exchange(
                    last,
                    now,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
            {
                tracing::warn!(
                    "Prompt cache Redis {} 异常 (降级本地, {}s 内限频): {}",
                    action,
                    self.interval_secs,
                    err
                );
            }
        }
    }
}

/// 基于 Redis 的共享 Prompt Cache 存储
pub struct RedisCacheStore {
    manager: redis::aio::ConnectionManager,
    timeout_duration: std::time::Duration,
    logger: ThrottledLogger,
}

impl RedisCacheStore {
    /// 通过 Redis URL 建立连接管理器，设置默认 200ms 命令超时
    pub async fn new(redis_url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(redis_url)?;
        let connect_timeout = std::time::Duration::from_millis(500);
        let manager =
            match tokio::time::timeout(connect_timeout, redis::aio::ConnectionManager::new(client))
                .await
            {
                Ok(res) => res?,
                Err(_) => {
                    return Err(redis::RedisError::from((
                        redis::ErrorKind::IoError,
                        "Redis 连接建立超时 (500ms)",
                    )));
                }
            };
        Ok(Self {
            manager,
            timeout_duration: std::time::Duration::from_millis(200),
            logger: ThrottledLogger::new(10),
        })
    }

    /// 设置自定义命令超时
    #[allow(dead_code)]
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout_duration = timeout;
        self
    }
}

impl RemoteCacheStore for RedisCacheStore {
    fn lookup_and_renew<'a>(&'a self, hash: u64) -> BoxFuture<'a, Option<u32>> {
        Box::pin(async move {
            let key = redis_entry_key(hash);
            let mut conn = self.manager.clone();
            let fut = async {
                let raw: Option<String> =
                    redis::cmd("GET").arg(&key).query_async(&mut conn).await?;
                if let Some(s) = raw {
                    if let Ok(entry) = serde_json::from_str::<CacheEntry>(&s) {
                        let now = now_secs();
                        if entry.expires_at > now {
                            let ttl = entry.ttl_secs.clamp(60, MAX_TTL_SECS);
                            let _: redis::RedisResult<()> = redis::cmd("EXPIRE")
                                .arg(&key)
                                .arg(ttl)
                                .query_async(&mut conn)
                                .await;
                            return Ok::<Option<u32>, redis::RedisError>(Some(entry.tokens));
                        }
                    }
                }
                Ok(None)
            };

            match tokio::time::timeout(self.timeout_duration, fut).await {
                Ok(Ok(tokens)) => tokens,
                Ok(Err(e)) => {
                    self.logger.warn("lookup", &e);
                    None
                }
                Err(_) => {
                    self.logger.warn("lookup", &"命令超时 (降级本地)");
                    None
                }
            }
        })
    }

    fn record_entry<'a>(&'a self, hash: u64, tokens: u32, ttl_secs: i64) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let ttl = ttl_secs.clamp(60, MAX_TTL_SECS);
            let now = now_secs();
            let entry = CacheEntry {
                tokens,
                ttl_secs: ttl,
                expires_at: now + ttl,
                last_hit_at: now,
            };
            let json = match serde_json::to_string(&entry) {
                Ok(j) => j,
                Err(e) => {
                    self.logger.warn("record 序列化", &e);
                    return;
                }
            };
            let key = redis_entry_key(hash);
            let mut conn = self.manager.clone();
            let fut = async {
                redis::cmd("SET")
                    .arg(&key)
                    .arg(&json)
                    .arg("EX")
                    .arg(ttl)
                    .query_async(&mut conn)
                    .await
            };

            match tokio::time::timeout(self.timeout_duration, fut).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    self.logger.warn("record", &e);
                }
                Err(_) => {
                    self.logger.warn("record", &"命令超时 (降级本地)");
                }
            }
        })
    }

    fn try_acquire_lock<'a>(&'a self, hash: u64, ttl_ms: u64) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let key = redis_lock_key(hash);
            let mut conn = self.manager.clone();
            let fut = async {
                let res: redis::RedisResult<Option<String>> = redis::cmd("SET")
                    .arg(&key)
                    .arg("1")
                    .arg("NX")
                    .arg("PX")
                    .arg(ttl_ms)
                    .query_async(&mut conn)
                    .await;
                match res {
                    Ok(Some(_)) => true,
                    _ => false,
                }
            };

            match tokio::time::timeout(self.timeout_duration, fut).await {
                Ok(acquired) => acquired,
                Err(_) => false,
            }
        })
    }
}

/// 测试用的内存 Fake 远程存储（支持模拟延迟、超时与故障注入）
#[allow(dead_code)]
#[derive(Default)]
pub struct FakeRemoteStore {
    pub entries: Mutex<HashMap<u64, CacheEntry>>,
    pub locks: Mutex<HashMap<u64, i64>>,
    pub fail_lookups: std::sync::atomic::AtomicBool,
    pub fail_records: std::sync::atomic::AtomicBool,
    pub record_count: std::sync::atomic::AtomicUsize,
}

impl RemoteCacheStore for FakeRemoteStore {
    fn lookup_and_renew<'a>(&'a self, hash: u64) -> BoxFuture<'a, Option<u32>> {
        Box::pin(async move {
            if self.fail_lookups.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            let now = now_secs();
            let mut map = self.entries.lock();
            if let Some(entry) = map.get_mut(&hash) {
                if entry.expires_at > now {
                    let ttl = entry.ttl_secs.clamp(60, MAX_TTL_SECS);
                    entry.last_hit_at = now;
                    entry.expires_at = now + ttl;
                    return Some(entry.tokens);
                }
            }
            None
        })
    }

    fn record_entry<'a>(&'a self, hash: u64, tokens: u32, ttl_secs: i64) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.fail_records.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            self.record_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let ttl = ttl_secs.clamp(60, MAX_TTL_SECS);
            let now = now_secs();
            let mut map = self.entries.lock();
            map.insert(
                hash,
                CacheEntry {
                    tokens,
                    ttl_secs: ttl,
                    expires_at: now + ttl,
                    last_hit_at: now,
                },
            );
        })
    }

    fn try_acquire_lock<'a>(&'a self, hash: u64, ttl_ms: u64) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let now = now_secs();
            let expires_at = now + (ttl_ms.max(1000) / 1000) as i64;
            let mut map = self.locks.lock();
            if let Some(exp) = map.get(&hash) {
                if *exp > now {
                    return false;
                }
            }
            map.insert(hash, expires_at);
            true
        })
    }
}

/// 进程内提示词缓存（带可选 Redis 共享层）
pub struct CacheMeter {
    inner: Mutex<Inner>,
    persist_path: Option<PathBuf>,
    remote: Option<Arc<dyn RemoteCacheStore>>,
    /// 计量模拟总开关（运行时可由 Admin API 切换，无需重启）。
    /// 关闭时 `compute_cache_usage` 直接返回全量 input、零缓存，且不读写任何缓存态。
    enabled: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<u64, CacheEntry>,
    /// 自上次落盘后是否有变化
    dirty: bool,
}

impl CacheMeter {
    /// 创建一个纯本地 cache。`persist_path` 为 `Some` 时会自动从该文件加载历史。
    #[allow(dead_code)]
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        Self::with_remote(persist_path, None)
    }

    /// 创建带可选远端共享存储的 cache。
    pub fn with_remote(
        persist_path: Option<PathBuf>,
        remote: Option<Arc<dyn RemoteCacheStore>>,
    ) -> Self {
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
            remote,
            enabled: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// 计量模拟是否启用。
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 运行时切换计量模拟开关。关闭时保留已有缓存条目（重新开启即可继续命中），
    /// 但期间既不查询也不写入。
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// builder 风格设置初始开关状态。
    pub fn with_enabled(self, enabled: bool) -> Self {
        self.set_enabled(enabled);
        self
    }

    /// 读取 `KIRO_RS_CACHE_METERING` 环境变量表达的开关意图。
    ///
    /// `0` / `off` / `false` / `no` / `disabled`（大小写不敏感、自动 trim）为关闭，
    /// 其余非空值为开启；未设置或空串返回 `None` 表示「未表态」。
    ///
    /// 生效优先级：config.json 显式值 > 本环境变量 > 默认开启。环境变量只作为
    /// 未落配置时的初值（适合全新 docker 部署），一旦从 UI 改过就以 config.json 为准。
    pub fn metering_enabled_from_env() -> Option<bool> {
        let raw = std::env::var("KIRO_RS_CACHE_METERING").ok()?;
        let raw = raw.trim().to_ascii_lowercase();
        if raw.is_empty() {
            return None;
        }
        Some(!matches!(
            raw.as_str(),
            "0" | "off" | "false" | "no" | "disabled"
        ))
    }

    /// 从环境变量 `KIRO_RS_CACHE_REDIS_URL` 初始化 CacheMeter。
    /// 未设置或连接失败时平滑回退到纯本地存储。
    pub async fn from_env(persist_path: Option<PathBuf>) -> Self {
        let redis_url = std::env::var("KIRO_RS_CACHE_REDIS_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let remote: Option<Arc<dyn RemoteCacheStore>> = match redis_url {
            Some(url) => match RedisCacheStore::new(&url).await {
                Ok(store) => {
                    tracing::info!(
                        "Prompt cache 共享 Redis 后端已启用: {}",
                        sanitize_redis_url(&url)
                    );
                    Some(Arc::new(store))
                }
                Err(e) => {
                    tracing::warn!(
                        "Prompt cache 共享 Redis 连接失败，已平滑降级为本地存储: {}",
                        e
                    );
                    None
                }
            },
            None => None,
        };

        Self::with_remote(persist_path, remote)
    }

    /// 查询单个哈希是否在缓存中且未过期。若命中则进行滑动续期（按 entry 自身 TTL），并返回 tokens。
    pub fn lookup_and_renew(&self, hash: u64) -> Option<u32> {
        let now = now_secs();
        let mut inner = self.inner.lock();
        let tokens = if let Some(entry) = inner.entries.get_mut(&hash) {
            if entry.expires_at > now {
                let ttl = entry.ttl_secs.clamp(60, MAX_TTL_SECS);
                entry.ttl_secs = ttl;
                entry.last_hit_at = now;
                entry.expires_at = now + ttl;
                Some(entry.tokens)
            } else {
                None
            }
        } else {
            None
        };
        if tokens.is_some() {
            inner.dirty = true;
        }
        tokens
    }

    /// 写入单个断点条目，按指定的 ttl_secs 设置过期时间
    pub fn record_entry(&self, hash: u64, tokens: u32, ttl_secs: i64) {
        let ttl = ttl_secs.clamp(60, MAX_TTL_SECS);
        let now = now_secs();
        let mut inner = self.inner.lock();
        inner.entries.insert(
            hash,
            CacheEntry {
                tokens,
                ttl_secs: ttl,
                expires_at: now + ttl,
                last_hit_at: now,
            },
        );
        inner.dirty = true;
        self.enforce_capacity(&mut inner);
    }

    fn enforce_capacity(&self, inner: &mut Inner) {
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

/// `Arc<CacheMeter>` 别名
pub type SharedCacheMeter = Arc<CacheMeter>;

// ============================================================================
// 与请求体协议层的接线
// ============================================================================

use super::stream::estimate_tokens;
use super::types::{CacheControl, MessagesRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LookbackGroup {
    ToolUse,
    ToolResult,
}

/// 协议层打平后的一个 Prompt Block
#[derive(Debug, Clone)]
struct PromptBlock {
    signature: Vec<u8>,
    tokens: u32,
    cache_control: Option<CacheControl>,
    invalid_cache_control: bool,
    cacheable: bool,
    lookback_group: Option<LookbackGroup>,
}

/// 识别出的断点
#[derive(Debug, Clone, Copy)]
struct Breakpoint {
    block_idx: usize,
    ttl_secs: i64,
}

/// 从请求体提取按 tools → system → messages 严格顺序拼接的 prompt blocks
fn extract_blocks(req: &MessagesRequest) -> Vec<PromptBlock> {
    let mut blocks = Vec::new();

    // 1. tools (按顺序遍历)
    if let Some(tools) = req.tools.as_ref() {
        for t in tools {
            let value = serde_json::to_value(t).unwrap_or(serde_json::Value::Null);
            let content = without_cache_control(&value);
            let serialized = serde_json::to_string(&content).unwrap_or_default();
            blocks.push(PromptBlock {
                signature: prompt_block_signature("tool", None, &content),
                tokens: estimate_tokens(&serialized).max(0) as u32,
                cache_control: t.cache_control.clone(),
                invalid_cache_control: false,
                cacheable: true,
                lookback_group: None,
            });
        }
    }

    // 2. system (按顺序遍历)
    if let Some(systems) = req.system.as_ref() {
        for sys in systems {
            let value = serde_json::to_value(sys).unwrap_or(serde_json::Value::Null);
            let content = without_cache_control(&value);
            blocks.push(PromptBlock {
                signature: prompt_block_signature("system", None, &content),
                tokens: estimate_tokens(&sys.text).max(0) as u32,
                cache_control: sys.cache_control.clone(),
                invalid_cache_control: false,
                cacheable: !sys.text.trim().is_empty(),
                lookback_group: None,
            });
        }
    }

    // 3. messages (按顺序遍历)
    for msg in &req.messages {
        match &msg.content {
            serde_json::Value::String(s) => {
                let content = serde_json::Value::String(s.clone());
                blocks.push(PromptBlock {
                    signature: prompt_block_signature("message", Some(&msg.role), &content),
                    tokens: estimate_tokens(s).max(0) as u32,
                    cache_control: None,
                    invalid_cache_control: false,
                    cacheable: !s.trim().is_empty(),
                    lookback_group: None,
                });
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    let (cache_control, invalid_cache_control) = match v.get("cache_control") {
                        Some(raw) => match serde_json::from_value::<CacheControl>(raw.clone()) {
                            Ok(cc) => (Some(cc), false),
                            Err(_) => (None, true),
                        },
                        None => (None, false),
                    };
                    let content = without_cache_control(v);
                    let lookback_group = match content.get("type").and_then(|value| value.as_str())
                    {
                        Some("tool_use") => Some(LookbackGroup::ToolUse),
                        Some("tool_result") => Some(LookbackGroup::ToolResult),
                        _ => None,
                    };
                    blocks.push(PromptBlock {
                        signature: prompt_block_signature("message", Some(&msg.role), &content),
                        tokens: block_tokens(&content),
                        cache_control,
                        invalid_cache_control,
                        cacheable: block_is_cacheable(&content),
                        lookback_group,
                    });
                }
            }
            _ => {}
        }
    }

    blocks
}

/// 解析断点集合：显式断点 + 顶层 auto 缓存断点
fn resolve_breakpoints(
    blocks: &[PromptBlock],
    top_cache_control: Option<&CacheControl>,
) -> Vec<Breakpoint> {
    if blocks.is_empty() {
        return Vec::new();
    }

    if blocks.iter().any(|block| block.invalid_cache_control) {
        return Vec::new();
    }

    let mut breakpoints: Vec<Breakpoint> = Vec::new();

    // 1. 显式断点
    for (idx, b) in blocks.iter().enumerate() {
        if let Some(cc) = &b.cache_control {
            if !b.cacheable {
                return Vec::new();
            }
            let Some(ttl) = validated_ttl(cc) else {
                return Vec::new();
            };
            breakpoints.push(Breakpoint {
                block_idx: idx,
                ttl_secs: ttl,
            });
        }
    }

    // 2. 顶层自动缓存：落在最后一个合格 block，占用 1 个断点槽
    if let Some(top_cc) = top_cache_control {
        let Some(auto_ttl) = validated_ttl(top_cc) else {
            return Vec::new();
        };
        if let Some(last_idx) = blocks.iter().rposition(|block| block.cacheable) {
            if let Some(existing) = breakpoints.iter().find(|bp| bp.block_idx == last_idx) {
                if existing.ttl_secs != auto_ttl {
                    return Vec::new();
                }
            } else {
                breakpoints.push(Breakpoint {
                    block_idx: last_idx,
                    ttl_secs: auto_ttl,
                });
            }
        }
    }

    if breakpoints.is_empty() {
        return Vec::new();
    }

    // 按 block_idx 升序排列
    breakpoints.sort_by_key(|bp| bp.block_idx);

    // Anthropic 对超过 4 个断点的请求返回错误。本地回退 API 无法返回该错误，
    // 因此整次禁用模拟，避免截断后虚报一个实际上不会成功的缓存结果。
    if breakpoints.len() > MAX_BREAKPOINTS {
        return Vec::new();
    }

    // 混合 TTL 顺序校验：1h 断点必须位于 5m 断点之前
    // 若 5m 之后出现了 1h，请求本身非法；整次禁用本地模拟。
    let mut seen_5m = false;
    for bp in &breakpoints {
        if bp.ttl_secs <= DEFAULT_TTL_SECS {
            seen_5m = true;
        } else if seen_5m && bp.ttl_secs > DEFAULT_TTL_SECS {
            return Vec::new();
        }
    }

    breakpoints
}

/// 异步调用 CacheMeter 计算本次请求的缓存覆盖情况（优先查询共享 Redis，失败降级本地），并把断点记录回 cache、刷新 TTL。
/// 返回 [`CacheUsage`]，由调用方在拿到真实 total 后做互斥分摊。
pub async fn compute_cache_usage(
    cache: &CacheMeter,
    req: &MessagesRequest,
    key_id: u64,
) -> CacheUsage {
    // 总开关关闭：不查不写，全量 prompt 计入 input（缓存两项为 0）。
    if !cache.is_enabled() {
        return CacheUsage::default();
    }

    let blocks = extract_blocks(req);
    if blocks.is_empty() {
        return CacheUsage::default();
    }

    let prompt_total_est: u32 = blocks.iter().map(|b| b.tokens).sum();

    // 解析断点（显式 + 顶层自动）
    let breakpoints = resolve_breakpoints(&blocks, req.cache_control.as_ref());
    if breakpoints.is_empty() {
        // 无断点：官方不会缓存，全部计入 input
        return CacheUsage {
            prompt_total_est: prompt_total_est as i32,
            ..Default::default()
        };
    }

    // 会话隔离种子
    let Some(seed) = isolation_seed(req, key_id) else {
        return CacheUsage {
            prompt_total_est: prompt_total_est as i32,
            ..Default::default()
        };
    };

    // 计算每个 block 的 cumulative tokens 和 cumulative hashes
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hash_frame(&mut hasher, seed.as_bytes());
    hash_frame(&mut hasher, &request_global_cache_context(req));

    let mut cum_tokens = Vec::with_capacity(blocks.len());
    let mut cum_hashes = Vec::with_capacity(blocks.len());
    let mut current_cum: u32 = 0;

    for b in &blocks {
        hash_frame(&mut hasher, &b.signature);
        current_cum = current_cum.saturating_add(b.tokens);
        cum_tokens.push(current_cum);

        let digest = hasher.clone().finalize();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&digest[..8]);
        cum_hashes.push(u64::from_be_bytes(buf));
    }

    // Lookup: 从每个断点向后检查最多 20 个 block，寻找此前真正写入过的最长前缀
    let mut max_read_tokens: u32 = 0;

    for bp in &breakpoints {
        let start_idx = lookback_start_index(&blocks, bp.block_idx);
        for check_idx in (start_idx..=bp.block_idx).rev() {
            let hash = cum_hashes[check_idx];
            let mut hit_tokens = None;

            // 优先查远端 Redis 共享后端
            if let Some(remote) = &cache.remote {
                if let Some(tokens) = remote.lookup_and_renew(hash).await {
                    // 回填本地保持同步
                    cache.record_entry(hash, tokens, bp.ttl_secs);
                    hit_tokens = Some(tokens);
                }
            }

            // 若远端未配置或 miss / 出错降级，查本地
            if hit_tokens.is_none() {
                hit_tokens = cache.lookup_and_renew(hash);
            }

            if let Some(tokens) = hit_tokens {
                if tokens > max_read_tokens {
                    max_read_tokens = tokens;
                }
                // 当前断点已找到最深命中，不必继续向更浅的 block 回溯
                break;
            }
        }
    }

    // Record: 仅在实际断点处写入 entry，每个 entry 保留自身的 TTL
    for bp in &breakpoints {
        let hash = cum_hashes[bp.block_idx];
        let tokens = cum_tokens[bp.block_idx];
        // 本地必须记录
        cache.record_entry(hash, tokens, bp.ttl_secs);

        // 若配置了远端，写入远端（附带轻量去重锁尝试，不阻塞）
        if let Some(remote) = &cache.remote {
            let _ = remote.try_acquire_lock(hash, 1500).await;
            remote.record_entry(hash, tokens, bp.ttl_secs).await;
        }
    }

    let covered_tokens = breakpoints
        .last()
        .map(|bp| cum_tokens[bp.block_idx])
        .unwrap_or(0);

    CacheUsage {
        cache_read: max_read_tokens as i32,
        cache_covered_est: covered_tokens as i32,
        prompt_total_est: prompt_total_est as i32,
    }
}

/// 同步计算本地缓存覆盖情况（仅使用进程内内存存储，供无异步上下文或纯同步测试复用）。
#[allow(dead_code)]
pub fn compute_cache_usage_sync(
    cache: &CacheMeter,
    req: &MessagesRequest,
    key_id: u64,
) -> CacheUsage {
    // 总开关关闭：不查不写，全量 prompt 计入 input（缓存两项为 0）。
    if !cache.is_enabled() {
        return CacheUsage::default();
    }

    let blocks = extract_blocks(req);
    if blocks.is_empty() {
        return CacheUsage::default();
    }

    let prompt_total_est: u32 = blocks.iter().map(|b| b.tokens).sum();

    // 解析断点（显式 + 顶层自动）
    let breakpoints = resolve_breakpoints(&blocks, req.cache_control.as_ref());
    if breakpoints.is_empty() {
        return CacheUsage {
            prompt_total_est: prompt_total_est as i32,
            ..Default::default()
        };
    }

    let Some(seed) = isolation_seed(req, key_id) else {
        return CacheUsage {
            prompt_total_est: prompt_total_est as i32,
            ..Default::default()
        };
    };

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hash_frame(&mut hasher, seed.as_bytes());
    hash_frame(&mut hasher, &request_global_cache_context(req));

    let mut cum_tokens = Vec::with_capacity(blocks.len());
    let mut cum_hashes = Vec::with_capacity(blocks.len());
    let mut current_cum: u32 = 0;

    for b in &blocks {
        hash_frame(&mut hasher, &b.signature);
        current_cum = current_cum.saturating_add(b.tokens);
        cum_tokens.push(current_cum);

        let digest = hasher.clone().finalize();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&digest[..8]);
        cum_hashes.push(u64::from_be_bytes(buf));
    }

    let mut max_read_tokens: u32 = 0;

    for bp in &breakpoints {
        let start_idx = lookback_start_index(&blocks, bp.block_idx);
        for check_idx in (start_idx..=bp.block_idx).rev() {
            let hash = cum_hashes[check_idx];
            if let Some(tokens) = cache.lookup_and_renew(hash) {
                if tokens > max_read_tokens {
                    max_read_tokens = tokens;
                }
                break;
            }
        }
    }

    for bp in &breakpoints {
        let hash = cum_hashes[bp.block_idx];
        let tokens = cum_tokens[bp.block_idx];
        cache.record_entry(hash, tokens, bp.ttl_secs);
    }

    let covered_tokens = breakpoints
        .last()
        .map(|bp| cum_tokens[bp.block_idx])
        .unwrap_or(0);

    CacheUsage {
        cache_read: max_read_tokens as i32,
        cache_covered_est: covered_tokens as i32,
        prompt_total_est: prompt_total_est as i32,
    }
}

/// 生成会话隔离种子，作为前缀哈希链的最前置输入。
fn isolation_seed(req: &MessagesRequest, key_id: u64) -> Option<String> {
    if let Some(session) = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_deref())
        .and_then(extract_session_id)
    {
        return if key_id == 0 {
            Some(format!("sess:{session}"))
        } else {
            Some(format!("key:{key_id}:sess:{session}"))
        };
    }
    if key_id == 0 {
        return None;
    }
    Some(format!("key:{key_id}"))
}

/// 从 Claude Code 的 user_id 中提取 session 标识。
fn extract_session_id(user_id: &str) -> Option<String> {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id)
        && let Some(sid) = json
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        return Some(sid.to_string());
    }

    user_id
        .split_once("_session_")
        .map(|(_, sid)| sid.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn validated_ttl(cache_control: &CacheControl) -> Option<i64> {
    if !cache_control.cache_type.eq_ignore_ascii_case("ephemeral") {
        return None;
    }
    match cache_control.ttl.as_deref() {
        None => Some(DEFAULT_TTL_SECS),
        Some(ttl) if ttl.eq_ignore_ascii_case("5m") => Some(DEFAULT_TTL_SECS),
        Some(ttl) if ttl.eq_ignore_ascii_case("1h") => Some(MAX_TTL_SECS),
        _ => None,
    }
}

fn without_cache_control(value: &serde_json::Value) -> serde_json::Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("cache_control");
    }
    value
}

fn prompt_block_signature(
    section: &str,
    role: Option<&str>,
    content: &serde_json::Value,
) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "section": section,
        "role": role,
        "content": content,
    }))
    .unwrap_or_default()
}

fn block_is_cacheable(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    match object.get("type").and_then(|value| value.as_str()) {
        Some("thinking" | "redacted_thinking") => false,
        Some("text") => object
            .get("text")
            .and_then(|value| value.as_str())
            .is_some_and(|text| !text.trim().is_empty()),
        Some(_) => true,
        None => false,
    }
}

fn block_tokens(value: &serde_json::Value) -> u32 {
    let block_type = value.get("type").and_then(|value| value.as_str());
    if block_type == Some("image") {
        let (media_type, data) = image_source_parts(value);
        return crate::image_resize::estimate_image_tokens(media_type, data);
    }
    if matches!(block_type, Some("text" | "thinking"))
        && let Some(text) = value
            .get(if block_type == Some("text") {
                "text"
            } else {
                "thinking"
            })
            .and_then(|value| value.as_str())
    {
        return estimate_tokens(text).max(0) as u32;
    }
    let serialized = serde_json::to_string(value).unwrap_or_default();
    estimate_tokens(&serialized).max(0) as u32
}

fn request_global_cache_context(req: &MessagesRequest) -> Vec<u8> {
    let thinking = req.thinking.as_ref().map(|thinking| {
        serde_json::json!({
            "type": thinking.thinking_type,
            "budget_tokens": thinking.budget_tokens,
        })
    });
    let output_config = req.output_config.as_ref().map(|config| {
        serde_json::json!({
            "effort": config.effort,
        })
    });
    serde_json::to_vec(&serde_json::json!({
        "model": req.model,
        "tool_choice": req.tool_choice,
        "thinking": thinking,
        "output_config": output_config,
    }))
    .unwrap_or_default()
}

/// 计算一个断点的 20 个回溯位置起点。连续 tool_use 或 tool_result 块各算一个位置。
fn lookback_start_index(blocks: &[PromptBlock], breakpoint_idx: usize) -> usize {
    let mut idx = breakpoint_idx;
    let mut positions = 1;
    while idx > 0 {
        let previous = idx - 1;
        let same_group = blocks[idx].lookback_group.is_some()
            && blocks[idx].lookback_group == blocks[previous].lookback_group;
        if !same_group {
            if positions == LOOKBACK_BLOCKS {
                break;
            }
            positions += 1;
        }
        idx = previous;
    }
    idx
}

fn hash_frame(hasher: &mut sha2::Sha256, bytes: &[u8]) {
    use sha2::Digest;
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

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
        assert_eq!(cache.lookup_and_renew(1), None);
        cache.record_entry(1, 10, 300);
        assert_eq!(cache.lookup_and_renew(1), Some(10));
    }

    #[test]
    fn ttl_expiry_makes_entry_miss() {
        let cache = CacheMeter::new(None);
        cache.record_entry(42, 100, 60);
        {
            let mut inner = cache.inner.lock();
            if let Some(e) = inner.entries.get_mut(&42) {
                e.expires_at = now_secs() - 1;
            }
        }
        assert_eq!(cache.lookup_and_renew(42), None);
    }

    #[test]
    fn evict_expired_removes_dead_entries() {
        let cache = CacheMeter::new(None);
        cache.record_entry(1, 5, 60);
        cache.record_entry(2, 5, 60);
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
    fn validated_ttl_handles_known_values() {
        let control = |ttl: Option<&str>| CacheControl {
            cache_type: "ephemeral".to_string(),
            ttl: ttl.map(str::to_string),
        };
        assert_eq!(validated_ttl(&control(Some("1h"))), Some(3600));
        assert_eq!(validated_ttl(&control(Some("5m"))), Some(300));
        assert_eq!(validated_ttl(&control(None)), Some(300));
        assert_eq!(validated_ttl(&control(Some("garbage"))), None);
    }

    #[test]
    fn flush_and_reload_round_trip() {
        let tmp = std::env::temp_dir().join(format!("kiro-pc-{}.json", now_secs()));
        let cache = CacheMeter::new(Some(tmp.clone()));
        cache.record_entry(7, 42, 600);
        cache.flush_to_disk();

        let cache2 = CacheMeter::new(Some(tmp.clone()));
        assert_eq!(cache2.lookup_and_renew(7), Some(42));

        let _ = std::fs::remove_file(&tmp);
    }

    fn build_request_with_system_breakpoint() -> MessagesRequest {
        use super::super::types::{CacheControl, Message, SystemMessage};
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
            cache_control: None,
        }
    }

    #[test]
    fn compute_cache_usage_first_miss_then_hit() {
        let cache = CacheMeter::new(None);
        let req = build_request_with_system_breakpoint();

        // 第一次：显式断点在 system，miss → 全部覆盖前缀算 creation（read == 0）
        let u1 = compute_cache_usage_sync(&cache, &req, 1);
        assert!(u1.cache_covered_est > 0, "first call should cover prefix");
        assert_eq!(u1.cache_read, 0, "first call has nothing cached to read");
        let total = u1.prompt_total_est;
        let (in1, cc1, cr1) = u1.split_against_total(total);
        assert!(cc1 > 0, "first call creation>0, cc={}", cc1);
        assert_eq!(cr1, 0);
        assert_eq!(in1 + cc1 + cr1, total, "互斥口径必须自洽");

        // 第二次：相同请求 → 命中
        let u2 = compute_cache_usage_sync(&cache, &req, 1);
        assert!(u2.cache_read > 0, "second call should hit");
        let (in2, cc2, cr2) = u2.split_against_total(total);
        assert_eq!(cc2, 0, "second call creation should be 0, got {}", cc2);
        assert!(cr2 > 0, "second call read>0, cr={}", cr2);
        assert_eq!(in2 + cc2 + cr2, total, "互斥口径必须自洽");
        assert_eq!(cc1, cr2);
    }

    #[test]
    fn split_against_total_is_mutually_exclusive() {
        let u = CacheUsage {
            cache_read: 30,
            cache_covered_est: 80,
            prompt_total_est: 100,
        };
        let (input, creation, read) = u.split_against_total(1000);
        assert_eq!(input + creation + read, 1000);
        assert_eq!(input, 200, "尾部 20% 是未缓存 input");
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
    fn no_cache_control_does_not_cache_at_all() {
        // 无顶层且无 block 级 cache_control：不模拟缓存，全部计入 input，不写入 CacheMeter
        use super::super::types::{Message, MessagesRequest};
        let cache = CacheMeter::new(None);
        let req1 = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String("Question 1".repeat(50)),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String("Answer 1".repeat(50)),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String("Question 2".repeat(50)),
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
        let u1 = compute_cache_usage_sync(&cache, &req1, 1);
        assert_eq!(u1.cache_covered_est, 0);
        assert_eq!(u1.cache_read, 0);
        assert_eq!(cache.len(), 0, "无 cache_control 不得写入任何条目");

        // 第二轮相同前缀请求依然不缓存
        let u2 = compute_cache_usage_sync(&cache, &req1, 1);
        assert_eq!(u2.cache_covered_est, 0);
        assert_eq!(u2.cache_read, 0);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn top_level_auto_caching_advances_and_hits_longest_prefix() {
        // 顶层 auto cache_control：首轮写入最后一个可缓存 block（User 1），次轮命中 User 1
        use super::super::types::{Message, MessagesRequest};
        let cache = CacheMeter::new(None);
        let auto_cc = Some(CacheControl {
            cache_type: "ephemeral".to_string(),
            ttl: Some("5m".to_string()),
        });

        let u1_text = "What is prompt caching? ".repeat(50);
        let a1_text = "Prompt caching allows reusing prefixes. ".repeat(50);
        let u2_text = "How does auto-caching advance across turns? ".repeat(20);

        // Turn 1: 单条 User 消息，开启顶层 auto-caching
        let turn1 = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 64,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::Value::String(u1_text.clone()),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: auto_cc.clone(),
        };

        let res1 = compute_cache_usage_sync(&cache, &turn1, 1);
        assert!(res1.cache_covered_est > 0, "Turn 1 应覆盖到 User 1");
        assert_eq!(res1.cache_read, 0, "Turn 1 无历史可读");
        assert_eq!(cache.len(), 1, "Turn 1 应且仅在 User 1 处写入 1 个断点");

        // Turn 2: [User 1, Assistant 1, User 2]，开启顶层 auto-caching
        let turn2 = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 64,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(u1_text.clone()),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(a1_text.clone()),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(u2_text.clone()),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: auto_cc.clone(),
        };

        let res2 = compute_cache_usage_sync(&cache, &turn2, 1);
        // Turn 2 自动断点落在 User 2，lookback 查找到 Turn 1 写入的 User 1
        assert_eq!(
            res2.cache_read, res1.cache_covered_est,
            "Turn 2 的 cache_read 应等于 Turn 1 写入的 User 1 累计 token"
        );
        assert!(
            res2.cache_covered_est > res2.cache_read,
            "Turn 2 覆盖到 User 2，covered > read"
        );
        // 只有 User 1 和 User 2 两个断点被写入，Assistant 1 没有断点
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn explicit_breakpoints_only_writes_at_marked_blocks() {
        // 显式断点：仅在标记了 cache_control 的 block 处写 entry
        use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage};
        let cache = CacheMeter::new(None);

        let sys_text = "You are a specialized code analyzer. ".repeat(60);
        let u1_text = "Analyze module A. ".repeat(30);
        let a1_text = "Module A looks clean. ".repeat(30);
        let u2_text = "Analyze module B. ".repeat(30);

        let req1 = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 64,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(u1_text.clone()),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(a1_text.clone()),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(u2_text.clone()),
                },
            ],
            stream: false,
            system: Some(vec![SystemMessage {
                text: sys_text.clone(),
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
            cache_control: None, // 无顶层自动缓存
        };

        let res1 = compute_cache_usage_sync(&cache, &req1, 1);
        assert_eq!(res1.cache_read, 0);
        assert_eq!(
            res1.cache_covered_est,
            estimate_tokens(&sys_text) as i32,
            "仅覆盖到显式标记的 system block"
        );
        assert_eq!(cache.len(), 1, "只在显式断点处写入 1 条记录");

        // 第二轮：在 u2 上打显式断点
        let req2 = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 64,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(u1_text.clone()),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(a1_text.clone()),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!([{
                        "type": "text",
                        "text": u2_text.clone(),
                        "cache_control": {"type": "ephemeral"}
                    }]),
                },
            ],
            stream: false,
            system: Some(vec![SystemMessage {
                text: sys_text.clone(),
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
            cache_control: None,
        };

        let res2 = compute_cache_usage_sync(&cache, &req2, 1);
        assert_eq!(
            res2.cache_read, res1.cache_covered_est,
            "命中上一轮写入的 system 断点"
        );
        assert_eq!(cache.len(), 2, "现在 CacheMeter 中应有且仅有 2 个断点");
    }

    #[test]
    fn lookback_20_blocks_boundary() {
        // 20-block lookback 边界测试：
        // 场景 A：断点位于 Block 19，向前 lookback 20 个 block（0..=19）包含 Block 0 → 命中
        // 场景 B：断点位于 Block 20，向前 lookback 20 个 block（1..=20）不包含 Block 0 → miss
        use super::super::types::{Message, MessagesRequest};

        let cache = CacheMeter::new(None);
        let block_text = "dummy block content for lookback test ".repeat(10);

        // Turn 1: 仅在 Block 0 写入断点
        let turn1 = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": block_text.clone(),
                    "cache_control": {"type": "ephemeral"}
                }]),
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
        let res1 = compute_cache_usage_sync(&cache, &turn1, 1);
        assert_eq!(res1.cache_read, 0);
        assert!(res1.cache_covered_est > 0);

        // 场景 A：构造总共 20 个 block（index 0..=19），断点打在 Block 19
        let mut msgs_within_20 = Vec::new();
        for i in 0..20 {
            if i == 19 {
                msgs_within_20.push(Message {
                    role: "user".to_string(),
                    content: serde_json::json!([{
                        "type": "text",
                        "text": format!("{block_text} {i}"),
                        "cache_control": {"type": "ephemeral"}
                    }]),
                });
            } else if i == 0 {
                msgs_within_20.push(Message {
                    role: "user".to_string(),
                    content: serde_json::json!([{
                        "type": "text",
                        "text": block_text.clone()
                    }]),
                });
            } else {
                msgs_within_20.push(Message {
                    role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                    content: serde_json::Value::String(format!("{block_text} {i}")),
                });
            }
        }

        let req_within_20 = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: msgs_within_20,
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };
        let res_within = compute_cache_usage_sync(&cache, &req_within_20, 1);
        assert_eq!(
            res_within.cache_read, res1.cache_covered_est,
            "Block 19 lookback 20 blocks 应该命中 Block 0"
        );

        // 场景 B：构造总共 22 个 block（index 0..=21），在隔离的新 session 中测试超限
        let cache_exceeded = CacheMeter::new(None);
        compute_cache_usage_sync(&cache_exceeded, &turn1, 1);

        let mut msgs_exceeded = Vec::new();
        for i in 0..22 {
            if i == 21 {
                msgs_exceeded.push(Message {
                    role: "user".to_string(),
                    content: serde_json::json!([{
                        "type": "text",
                        "text": format!("{block_text} {i}"),
                        "cache_control": {"type": "ephemeral"}
                    }]),
                });
            } else if i == 0 {
                msgs_exceeded.push(Message {
                    role: "user".to_string(),
                    content: serde_json::json!([{
                        "type": "text",
                        "text": block_text.clone()
                    }]),
                });
            } else {
                msgs_exceeded.push(Message {
                    role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                    content: serde_json::Value::String(format!("{block_text} {i}")),
                });
            }
        }

        let req_exceeded = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: msgs_exceeded,
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };
        let res_exceeded = compute_cache_usage_sync(&cache_exceeded, &req_exceeded, 1);
        assert_eq!(
            res_exceeded.cache_read, 0,
            "Block 21 lookback 20 blocks（仅到 Block 2）无法触及 Block 0，必须 miss"
        );
    }

    #[test]
    fn parallel_tool_blocks_count_as_one_lookback_position() {
        use super::super::types::{Message, MessagesRequest};

        let first_request = || MessagesRequest {
            model: "model-a".to_string(),
            max_tokens: 32,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": "stable prefix",
                    "cache_control": {"type": "ephemeral"}
                }]),
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

        for block_type in ["tool_use", "tool_result"] {
            let cache = CacheMeter::new(None);
            let cold = compute_cache_usage_sync(&cache, &first_request(), 1);

            let parallel_blocks: Vec<serde_json::Value> = (0..25)
                .map(|index| {
                    if block_type == "tool_use" {
                        serde_json::json!({
                            "type": "tool_use",
                            "id": format!("toolu_{index}"),
                            "name": "lookup",
                            "input": {"index": index}
                        })
                    } else {
                        serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": format!("toolu_{index}"),
                            "content": format!("result {index}")
                        })
                    }
                })
                .collect();
            let next_request = MessagesRequest {
                model: "model-a".to_string(),
                max_tokens: 32,
                messages: vec![
                    Message {
                        role: "user".to_string(),
                        content: serde_json::json!([{
                            "type": "text",
                            "text": "stable prefix"
                        }]),
                    },
                    Message {
                        role: if block_type == "tool_use" {
                            "assistant".to_string()
                        } else {
                            "user".to_string()
                        },
                        content: serde_json::Value::Array(parallel_blocks),
                    },
                    Message {
                        role: "user".to_string(),
                        content: serde_json::json!([{
                            "type": "text",
                            "text": "new suffix",
                            "cache_control": {"type": "ephemeral"}
                        }]),
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

            let warm = compute_cache_usage_sync(&cache, &next_request, 1);
            assert_eq!(
                warm.cache_read, cold.cache_covered_est,
                "连续 {block_type} 块必须只占一个回溯位置"
            );
        }
    }

    #[test]
    fn mixed_ttl_independent_expiry_and_renewal() {
        // 5m 与 1h 独立记录 TTL，独立过期与滑动续期
        use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage};
        let cache = CacheMeter::new(None);

        let sys_text = "System Prompt 1h stability ".repeat(40);
        let u1_text = "User Question 5m ephemeral ".repeat(40);

        let req = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": u1_text.clone(),
                    "cache_control": {"type": "ephemeral", "ttl": "5m"}
                }]),
            }],
            stream: false,
            system: Some(vec![SystemMessage {
                text: sys_text.clone(),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: Some("1h".to_string()),
                }),
            }]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };

        let u1 = compute_cache_usage_sync(&cache, &req, 1);
        assert_eq!(u1.cache_read, 0);

        // 模拟 350 秒后：5m 断点过期（300s），1h 断点仍然有效（3600s）
        {
            let mut inner = cache.inner.lock();
            for (_, v) in inner.entries.iter_mut() {
                v.expires_at -= 350;
            }
        }

        // 再次请求
        let u2 = compute_cache_usage_sync(&cache, &req, 1);
        let sys_tokens = estimate_tokens(&sys_text) as i32;
        assert_eq!(
            u2.cache_read, sys_tokens,
            "350 秒后 5m 消息段过期 miss，但 1h 的 system 段必须仍命中并续期"
        );
    }

    #[test]
    fn mixed_ttl_invalid_order_disables_local_metering() {
        // 混合 TTL 顺序非法时，Anthropic 会拒绝请求；本地回退不得虚报缓存。
        use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage};
        let cache = CacheMeter::new(None);

        let req = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": "User msg 1h after 5m sys",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"} // 非法：在 5m 之后
                }]),
            }],
            stream: false,
            system: Some(vec![SystemMessage {
                text: "System msg with 5m".to_string(),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: Some("5m".to_string()),
                }),
            }]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };

        let usage = compute_cache_usage_sync(&cache, &req, 1);
        assert_eq!(usage.cache_covered_est, 0);
        assert_eq!(usage.cache_read, 0);
        assert_eq!(cache.len(), 0, "非法请求不得写入任何本地缓存条目");
    }

    #[test]
    fn max_breakpoints_limit_enforced() {
        // 最多 4 个断点
        use super::super::types::{Message, MessagesRequest};
        let cache = CacheMeter::new(None);

        let mut msgs = Vec::new();
        for i in 0..6 {
            msgs.push(Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": format!("Message {i} with cc"),
                    "cache_control": {"type": "ephemeral"}
                }]),
            });
        }

        let req = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: msgs,
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
        };

        let usage = compute_cache_usage_sync(&cache, &req, 1);
        assert_eq!(usage.cache_covered_est, 0);
        assert_eq!(usage.cache_read, 0);
        assert_eq!(cache.len(), 0, "超过 4 个断点时不得模拟部分成功");
    }

    #[test]
    fn structured_tool_fields_are_part_of_prefix_key() {
        use super::super::types::{CacheControl, Message, MessagesRequest};

        let make =
            |name: &str, id: &str, path: &str, result: &str, is_error: bool| MessagesRequest {
                model: "claude-sonnet-4-5-20250929".to_string(),
                max_tokens: 32,
                messages: vec![
                    Message {
                        role: "user".to_string(),
                        content: serde_json::json!([{"type":"text","text":"inspect a file"}]),
                    },
                    Message {
                        role: "assistant".to_string(),
                        content: serde_json::json!([{
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": {"file_path": path}
                        }]),
                    },
                    Message {
                        role: "user".to_string(),
                        content: serde_json::json!([{
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": result,
                            "is_error": is_error
                        }]),
                    },
                ],
                stream: false,
                system: None,
                tools: None,
                tool_choice: None,
                thinking: None,
                output_config: None,
                metadata: None,
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: None,
                }),
            };

        let identical_cache = CacheMeter::new(None);
        let cold = compute_cache_usage_sync(
            &identical_cache,
            &make("Read", "toolu_1", "/a.rs", "alpha", false),
            1,
        );
        let warm = compute_cache_usage_sync(
            &identical_cache,
            &make("Read", "toolu_1", "/a.rs", "alpha", false),
            1,
        );
        assert_eq!(warm.cache_read, cold.cache_covered_est);

        let changed_input_cache = CacheMeter::new(None);
        compute_cache_usage_sync(
            &changed_input_cache,
            &make("Read", "toolu_1", "/a.rs", "alpha", false),
            1,
        );
        let changed_input = compute_cache_usage_sync(
            &changed_input_cache,
            &make("Read", "toolu_1", "/b.rs", "alpha", false),
            1,
        );
        assert_eq!(
            changed_input.cache_read, 0,
            "tool_use.input 变化必须使前缀失效"
        );

        let changed_result_cache = CacheMeter::new(None);
        compute_cache_usage_sync(
            &changed_result_cache,
            &make("Read", "toolu_1", "/a.rs", "alpha", false),
            1,
        );
        let changed_result = compute_cache_usage_sync(
            &changed_result_cache,
            &make("Read", "toolu_1", "/a.rs", "beta", false),
            1,
        );
        assert_eq!(
            changed_result.cache_read, 0,
            "tool_result.content 变化不得与旧断点产生虚假命中"
        );

        let changed_name_cache = CacheMeter::new(None);
        compute_cache_usage_sync(
            &changed_name_cache,
            &make("Read", "toolu_1", "/a.rs", "alpha", false),
            1,
        );
        let changed_name = compute_cache_usage_sync(
            &changed_name_cache,
            &make("Write", "toolu_1", "/a.rs", "alpha", false),
            1,
        );
        assert_eq!(
            changed_name.cache_read, 0,
            "tool_use.name 变化必须使前缀失效"
        );

        let changed_id_cache = CacheMeter::new(None);
        compute_cache_usage_sync(
            &changed_id_cache,
            &make("Read", "toolu_1", "/a.rs", "alpha", false),
            1,
        );
        let changed_id = compute_cache_usage_sync(
            &changed_id_cache,
            &make("Read", "toolu_2", "/a.rs", "alpha", false),
            1,
        );
        assert_eq!(changed_id.cache_read, 0, "tool_use.id 变化必须使前缀失效");

        let changed_err_cache = CacheMeter::new(None);
        compute_cache_usage_sync(
            &changed_err_cache,
            &make("Read", "toolu_1", "/a.rs", "alpha", false),
            1,
        );
        let changed_err = compute_cache_usage_sync(
            &changed_err_cache,
            &make("Read", "toolu_1", "/a.rs", "alpha", true),
            1,
        );
        assert_eq!(
            changed_err.cache_read, 0,
            "tool_result.is_error 变化必须使前缀失效"
        );
    }

    #[test]
    fn request_configuration_changes_invalidate_prefix() {
        use super::super::types::{CacheControl, Message, MessagesRequest, OutputConfig, Thinking};

        let make = |model: &str,
                    tool_choice: Option<serde_json::Value>,
                    budget_tokens: Option<i32>,
                    effort: Option<&str>| MessagesRequest {
            model: model.to_string(),
            max_tokens: 32,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{"type":"text","text":"stable request"}]),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice,
            thinking: budget_tokens.map(|budget_tokens| Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens,
            }),
            output_config: effort.map(|effort| OutputConfig {
                effort: effort.to_string(),
            }),
            metadata: None,
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        };

        let model_cache = CacheMeter::new(None);
        compute_cache_usage_sync(&model_cache, &make("model-a", None, None, None), 1);
        assert_eq!(
            compute_cache_usage_sync(&model_cache, &make("model-b", None, None, None), 1)
                .cache_read,
            0,
            "model 变化必须 miss"
        );

        let tool_choice_cache = CacheMeter::new(None);
        compute_cache_usage_sync(&tool_choice_cache, &make("model-a", None, None, None), 1);
        assert_eq!(
            compute_cache_usage_sync(
                &tool_choice_cache,
                &make(
                    "model-a",
                    Some(serde_json::json!({"type":"auto"})),
                    None,
                    None
                ),
                1,
            )
            .cache_read,
            0,
            "tool_choice 变化必须 miss"
        );

        let thinking_cache = CacheMeter::new(None);
        compute_cache_usage_sync(&thinking_cache, &make("model-a", None, Some(1024), None), 1);
        assert_eq!(
            compute_cache_usage_sync(&thinking_cache, &make("model-a", None, Some(2048), None), 1)
                .cache_read,
            0,
            "thinking 变化必须 miss"
        );

        let output_config_cache = CacheMeter::new(None);
        compute_cache_usage_sync(
            &output_config_cache,
            &make("model-a", None, None, Some("high")),
            1,
        );
        assert_eq!(
            compute_cache_usage_sync(
                &output_config_cache,
                &make("model-a", None, None, Some("low")),
                1
            )
            .cache_read,
            0,
            "output_config 变化必须 miss"
        );
    }

    #[test]
    fn automatic_cache_uses_last_eligible_block() {
        use super::super::types::{CacheControl, Message, MessagesRequest};

        let make = || MessagesRequest {
            model: "model-a".to_string(),
            max_tokens: 32,
            messages: vec![Message {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type":"text","text":"stable visible content"},
                    {"type":"thinking","thinking":"not a direct breakpoint"},
                    {"type":"redacted_thinking","data":"redacted"},
                    {"type":"text","text":""}
                ]),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        };

        let cache = CacheMeter::new(None);
        let cold = compute_cache_usage_sync(&cache, &make(), 1);
        assert_eq!(
            cold.cache_covered_est,
            estimate_tokens("stable visible content"),
            "automatic 断点应回退到最后一个合格文本块"
        );
        assert!(cold.prompt_total_est > cold.cache_covered_est);
        let warm = compute_cache_usage_sync(&cache, &make(), 1);
        assert_eq!(warm.cache_read, cold.cache_covered_est);

        // 全部 block 都不合格时，不模拟缓存
        let all_ineligible = MessagesRequest {
            model: "model-a".to_string(),
            max_tokens: 32,
            messages: vec![Message {
                role: "assistant".to_string(),
                content: serde_json::json!([
                    {"type":"thinking","thinking":"cannot cache"},
                    {"type":"redacted_thinking","data":"redacted"},
                    {"type":"text","text":"   "}
                ]),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        };

        let empty_cache = CacheMeter::new(None);
        let res = compute_cache_usage_sync(&empty_cache, &all_ineligible, 1);
        assert_eq!(res.cache_covered_est, 0);
        assert_eq!(res.cache_read, 0);
        assert_eq!(empty_cache.len(), 0);
        assert!(res.prompt_total_est > 0);
    }

    #[test]
    fn invalid_cache_controls_do_not_write_entries() {
        use super::super::types::{CacheControl, Message, MessagesRequest};

        let request =
            |content: serde_json::Value, top_cache_control: Option<CacheControl>| MessagesRequest {
                model: "model-a".to_string(),
                max_tokens: 32,
                messages: vec![Message {
                    role: "user".to_string(),
                    content,
                }],
                stream: false,
                system: None,
                tools: None,
                tool_choice: None,
                thinking: None,
                output_config: None,
                metadata: None,
                cache_control: top_cache_control,
            };

        // 1. 非 ephemeral 类型
        let invalid_type_cache = CacheMeter::new(None);
        let invalid_type = compute_cache_usage_sync(
            &invalid_type_cache,
            &request(
                serde_json::json!([{"type":"text","text":"hello"}]),
                Some(CacheControl {
                    cache_type: "persistent".to_string(),
                    ttl: None,
                }),
            ),
            1,
        );
        assert_eq!(invalid_type.cache_covered_est, 0);
        assert_eq!(invalid_type_cache.len(), 0);

        // 2. 显式 cache_control 在 thinking 上
        let thinking_cache = CacheMeter::new(None);
        let explicit_thinking = compute_cache_usage_sync(
            &thinking_cache,
            &request(
                serde_json::json!([{
                    "type":"thinking",
                    "thinking":"secret",
                    "cache_control":{"type":"ephemeral"}
                }]),
                None,
            ),
            1,
        );
        assert_eq!(explicit_thinking.cache_covered_est, 0);
        assert_eq!(thinking_cache.len(), 0);

        // 3. 显式 cache_control 在 redacted_thinking 上
        let redacted_cache = CacheMeter::new(None);
        let explicit_redacted = compute_cache_usage_sync(
            &redacted_cache,
            &request(
                serde_json::json!([{
                    "type":"redacted_thinking",
                    "data":"secret",
                    "cache_control":{"type":"ephemeral"}
                }]),
                None,
            ),
            1,
        );
        assert_eq!(explicit_redacted.cache_covered_est, 0);
        assert_eq!(redacted_cache.len(), 0);

        // 4. 显式 cache_control 在空 text 上
        let empty_text_cache = CacheMeter::new(None);
        let explicit_empty = compute_cache_usage_sync(
            &empty_text_cache,
            &request(
                serde_json::json!([{
                    "type":"text",
                    "text":"",
                    "cache_control":{"type":"ephemeral"}
                }]),
                None,
            ),
            1,
        );
        assert_eq!(explicit_empty.cache_covered_est, 0);
        assert_eq!(empty_text_cache.len(), 0);

        // 5. 顶层自动与显式 TTL 冲突 (5m vs 1h)
        let conflict_cache = CacheMeter::new(None);
        let conflict = compute_cache_usage_sync(
            &conflict_cache,
            &request(
                serde_json::json!([{
                    "type":"text",
                    "text":"hello",
                    "cache_control":{"type":"ephemeral","ttl":"5m"}
                }]),
                Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: Some("1h".to_string()),
                }),
            ),
            1,
        );
        assert_eq!(conflict.cache_covered_est, 0);
        assert_eq!(conflict_cache.len(), 0);

        // 6. 顶层自动与显式同位置同 TTL (5m + 5m) 是 no-op，成功模拟
        let noop_cache = CacheMeter::new(None);
        let noop = compute_cache_usage_sync(
            &noop_cache,
            &request(
                serde_json::json!([{
                    "type":"text",
                    "text":"hello world",
                    "cache_control":{"type":"ephemeral","ttl":"5m"}
                }]),
                Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: Some("5m".to_string()),
                }),
            ),
            1,
        );
        assert!(noop.cache_covered_est > 0);
        assert_eq!(noop_cache.len(), 1);
    }

    /// 代表性多轮会话 fixture：热缓存后自然产生 75%～90% 的 cache_read 占比目标
    ///
    /// 模拟真实 Claude Code 开发会话结构：
    /// - Tools 列表（文件读写、bash、grep 等工具定义，约 800 tokens）
    /// - System instructions（长规则与规范，约 1200 tokens）
    /// - 历史轮次（User 问题与 Assistant 工具调用/代码回答，约 1500 tokens）
    /// - 当前轮输入（新用户问题与 context，约 500～800 tokens）
    /// - 总 input 约为 4000～4300 tokens，其中稳定前缀约为 3500 tokens（占比 80%～85%）
    #[test]
    fn representative_multi_turn_fixture_warm_cache_read_ratio() {
        use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage, Tool};

        let cache = CacheMeter::new(None);
        let top_cc = Some(CacheControl {
            cache_type: "ephemeral".to_string(),
            ttl: Some("5m".to_string()),
        });

        // 构造代表性的真实 tools
        let mut tools = Vec::new();
        for name in ["fs_read", "fs_write", "execute_bash", "grep_search"] {
            let mut schema = std::collections::BTreeMap::new();
            schema.insert("type".to_string(), serde_json::json!("object"));
            schema.insert(
                "properties".to_string(),
                serde_json::json!({
                    "path": {"type": "string", "description": "Absolute path to file"},
                    "content": {"type": "string", "description": "Text content to write or match"},
                    "options": {"type": "object", "description": "Extended execution flags"}
                }),
            );
            tools.push(Tool {
                name: name.to_string(),
                description: format!("Standard development tool for {name} with robust error handling and pagination."),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                cache_control: None,
            });
        }

        // 构造代表性的系统指令（~1200 tokens）
        let system_prompt = "You are Claude, an AI coding assistant designed to help developers with complex programming tasks. Follow KISS principles, provide step-by-step reasoning, always preserve backward compatibility, and avoid premature optimizations. Ensure all file operations are idempotent and verify changes with targeted test runs. ".repeat(25);

        // Turn 1 上下文
        let u1_prompt = "Please review our architecture and help refactor the prompt cache simulator to follow current official specifications. Here is the background and constraints. ".repeat(10);
        let a1_response = "I have analyzed the codebase and requirements. Here is the structured plan and the list of tasks. Let us begin with updating types and literals. ".repeat(15);
        let u2_prompt =
            "Proceed with implementing the 20-block lookback and independent TTL management. "
                .repeat(8);
        let a2_response = "The lookup mechanism and sliding renewal have been implemented. Now running targeted verification tests. ".repeat(12);

        // Turn 3 当前轮提问（~400 tokens）
        let u3_prompt = "All targeted unit tests have passed. Please prepare the final report with structured summary. ".repeat(6);

        // Turn 1 请求
        let turn1 = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::Value::String(u1_prompt.clone()),
            }],
            stream: false,
            system: Some(vec![SystemMessage {
                text: system_prompt.clone(),
                cache_control: None,
            }]),
            tools: Some(tools.clone()),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: top_cc.clone(),
        };

        let u1 = compute_cache_usage_sync(&cache, &turn1, 100);
        assert_eq!(u1.cache_read, 0, "Turn 1 冷启动 cache_read 为 0");
        assert!(u1.cache_covered_est > 0);

        // Turn 2 请求（累积历史）
        let turn2 = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 1024,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(u1_prompt.clone()),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(a1_response.clone()),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(u2_prompt.clone()),
                },
            ],
            stream: false,
            system: Some(vec![SystemMessage {
                text: system_prompt.clone(),
                cache_control: None,
            }]),
            tools: Some(tools.clone()),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: top_cc.clone(),
        };

        compute_cache_usage_sync(&cache, &turn2, 100);

        // Turn 3 请求（热缓存代表性测试）：拥有完整的工具定义、系统提示词、前两轮完整历史和新一轮输入
        let turn3 = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 1024,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(u1_prompt.clone()),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(a1_response.clone()),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(u2_prompt.clone()),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(a2_response.clone()),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(u3_prompt.clone()),
                },
            ],
            stream: false,
            system: Some(vec![SystemMessage {
                text: system_prompt.clone(),
                cache_control: None,
            }]),
            tools: Some(tools.clone()),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: top_cc.clone(),
        };

        let u3 = compute_cache_usage_sync(&cache, &turn3, 100);
        let total = u3.prompt_total_est;
        let (input, creation, read) = u3.split_against_total(total);

        // 验证三项守恒
        assert_eq!(input + creation + read, total);

        let read_ratio = read as f64 / total as f64;
        println!(
            "Fixture Metrics: total={}, input={}, creation={}, read={}, read_ratio={:.2}%",
            total,
            input,
            creation,
            read,
            read_ratio * 100.0
        );

        // 验收目标：热缓存后 cache-read 比例自然落在 75%～90% 区间
        assert!(
            (0.75..=0.90).contains(&read_ratio),
            "热缓存下 cache_read 占比应在 75%～90% 之间，实际: {:.2}% (read: {}, total: {})",
            read_ratio * 100.0,
            read,
            total
        );
    }

    /// 会话隔离：相同前缀内容，不同客户端 Key（key_id）之间不应互相命中。
    #[test]
    fn different_key_id_does_not_cross_hit() {
        use super::super::types::{CacheControl, Message, MessagesRequest, Metadata};
        let cache = CacheMeter::new(None);
        let body = "shared system prompt and history ".repeat(20);
        let make_req = || MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(body.clone()),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(body.clone()),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(body.clone()),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: Some(Metadata {
                user_id: Some("user_account__session_shared-session".to_string()),
            }),
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        };

        // Key=1 建立缓存
        let a = compute_cache_usage_sync(&cache, &make_req(), 1);
        assert!(a.cache_covered_est > 0);
        assert_eq!(a.cache_read, 0);

        // Key=2 相同内容，但隔离种子不同 → 不命中
        let b = compute_cache_usage_sync(&cache, &make_req(), 2);
        assert_eq!(b.cache_read, 0, "不同 key_id 不应命中彼此的前缀");

        // Key=1 再次请求 → 命中自己写入的
        let c = compute_cache_usage_sync(&cache, &make_req(), 1);
        assert!(c.cache_read > 0, "同一 key_id 应命中自己的前缀");
    }

    #[test]
    fn metadata_json_session_scopes_cache() {
        use super::super::types::{CacheControl, Message, MessagesRequest, Metadata};
        let body = "conversation prefix that stays stable ".repeat(20);
        let make = |session: &str| {
            MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 64,
            messages: vec![
                Message {
                    role: "user".into(),
                    content: serde_json::json!([{"type":"text","text":body}]),
                },
                Message {
                    role: "assistant".into(),
                    content: serde_json::json!([{"type":"text","text":body}]),
                },
                Message {
                    role: "user".into(),
                    content: serde_json::json!([{"type":"text","text":body}]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: Some(Metadata {
                user_id: Some(
                    serde_json::json!({
                        "device_id": "2721550240e8e5303fa95053fab0666443ab2b2ea79c2fc67bb6ff336f1297a9",
                        "account_uuid": "",
                        "session_id": session,
                    })
                    .to_string(),
                ),
            }),
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        }
        };

        let cache = CacheMeter::new(None);
        let s1a =
            compute_cache_usage_sync(&cache, &make("c479866d-b846-4e87-807b-a5ed0d84948c"), 0);
        assert_eq!(s1a.cache_read, 0, "首轮无历史可命中");
        assert!(s1a.cache_covered_est > 0, "JSON session 应启用缓存模拟");

        let s2 = compute_cache_usage_sync(&cache, &make("00000000-0000-0000-0000-000000000000"), 0);
        assert_eq!(s2.cache_read, 0, "不同 session 不应命中");

        let s1b =
            compute_cache_usage_sync(&cache, &make("c479866d-b846-4e87-807b-a5ed0d84948c"), 0);
        assert!(s1b.cache_read > 0, "相同 session 应命中");
    }

    #[test]
    fn metadata_session_scopes_cache() {
        use super::super::types::{CacheControl, Message, MessagesRequest, Metadata};
        let body = "conversation prefix that stays stable ".repeat(20);
        let make = |session: &str| MessagesRequest {
            model: "claude-opus-4-8".to_string(),
            max_tokens: 64,
            messages: vec![
                Message {
                    role: "user".into(),
                    content: serde_json::json!([{"type":"text","text":body}]),
                },
                Message {
                    role: "assistant".into(),
                    content: serde_json::json!([{"type":"text","text":body}]),
                },
                Message {
                    role: "user".into(),
                    content: serde_json::json!([{"type":"text","text":body}]),
                },
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
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        };

        let cache = CacheMeter::new(None);
        let s1a = compute_cache_usage_sync(&cache, &make("aaa"), 0);
        assert_eq!(s1a.cache_read, 0);

        let s2 = compute_cache_usage_sync(&cache, &make("bbb"), 0);
        assert_eq!(s2.cache_read, 0, "不同 session 不应命中");

        let s1b = compute_cache_usage_sync(&cache, &make("aaa"), 0);
        assert!(s1b.cache_read > 0, "相同 session 应命中");
    }

    #[test]
    fn master_key_without_session_does_not_simulate_cross_user_cache_hit() {
        use super::super::types::{CacheControl, Message, MessagesRequest};
        let cache = CacheMeter::new(None);
        let body = "shared master-key prompt without any session ".repeat(20);
        let make_req = || MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(body.clone()),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(body.clone()),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::Value::String(body.clone()),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        };

        let a = compute_cache_usage_sync(&cache, &make_req(), 0);
        assert_eq!(a.cache_read, 0);
        assert_eq!(a.cache_covered_est, 0, "主 Key 无 session 不应产生缓存覆盖");

        let b = compute_cache_usage_sync(&cache, &make_req(), 0);
        assert_eq!(b.cache_read, 0);
        assert_eq!(b.cache_covered_est, 0);
    }

    #[test]
    fn tool_signature_stable_across_insert_order() {
        use super::super::types::Tool;
        let build_tool = |insert_required_first: bool| {
            let mut schema = std::collections::BTreeMap::new();
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
        };

        let signature = |tool: Tool| {
            let value = serde_json::to_value(tool).unwrap();
            prompt_block_signature("tool", None, &without_cache_control(&value))
        };
        assert_eq!(signature(build_tool(true)), signature(build_tool(false)));
    }

    #[test]
    fn image_block_contributes_tokens_and_hits() {
        use super::super::types::{CacheControl, Message, MessagesRequest};
        let png = make_test_png(750, 750);
        let img_tokens = crate::image_resize::estimate_image_tokens("image/png", &png) as i32;
        assert!(img_tokens > 100);

        let image_message = || Message {
            role: "user".to_string(),
            content: serde_json::json!([
                {"type":"image","source":{"type":"base64","media_type":"image/png","data": png}},
                {"type":"text","text":"describe"}
            ]),
        };
        let make = |messages: Vec<Message>| MessagesRequest {
            model: "m".to_string(),
            max_tokens: 8,
            messages,
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: Some(CacheControl {
                cache_type: "ephemeral".to_string(),
                ttl: None,
            }),
        };

        let cache = CacheMeter::new(None);
        let u1 = compute_cache_usage_sync(&cache, &make(vec![image_message()]), 1);
        let text_only = estimate_tokens("describe") as i32;
        assert!(
            u1.cache_covered_est >= img_tokens + text_only - 5,
            "covered({}) 应含图片 token({})",
            u1.cache_covered_est,
            img_tokens
        );
        assert_eq!(u1.cache_read, 0);

        let u2 = compute_cache_usage_sync(
            &cache,
            &make(vec![
                image_message(),
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!("a pixel"),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!("q2"),
                },
            ]),
            1,
        );
        assert!(
            u2.cache_read >= img_tokens,
            "含图历史应跨轮命中且 read({}) 含图片 token({})",
            u2.cache_read,
            img_tokens
        );
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

    #[test]
    fn extract_session_id_parses_claude_code_format() {
        assert_eq!(
            extract_session_id("user_xxx_account__session_0b4445e1-uuid"),
            Some("0b4445e1-uuid".to_string())
        );
        assert_eq!(extract_session_id("no-session-here"), None);
        assert_eq!(extract_session_id("trailing_session_"), None);
    }

    #[test]
    fn extract_session_id_parses_json_format() {
        let user_id = r#"{"device_id":"2721550240e8e5303fa95053fab0666443ab2b2ea79c2fc67bb6ff336f1297a9","account_uuid":"","session_id":"c479866d-b846-4e87-807b-a5ed0d84948c"}"#;
        assert_eq!(
            extract_session_id(user_id),
            Some("c479866d-b846-4e87-807b-a5ed0d84948c".to_string())
        );
    }

    #[test]
    fn extract_session_id_json_without_session_falls_back() {
        assert_eq!(extract_session_id(r#"{"device_id":"abc"}"#), None);
        assert_eq!(
            extract_session_id(r#"{"device_id":"abc","session_id":""}"#),
            None
        );
    }

    #[tokio::test]
    async fn test_redis_key_namespace_and_isolation() {
        let key1 = redis_entry_key(0x123456789abcdef0);
        assert_eq!(key1, "kiro:pcm:v1:entry:123456789abcdef0");
        let lock1 = redis_lock_key(0x123456789abcdef0);
        assert_eq!(lock1, "kiro:pcm:v1:lock:123456789abcdef0");

        // URL 脱敏
        let sensitive = "redis://:secret_password@10.0.0.1:6379/1";
        assert_eq!(sanitize_redis_url(sensitive), "redis://***@10.0.0.1:6379/1");
        let user_pass = "redis://user:pass123@redis-cluster.internal:6379/0";
        assert_eq!(
            sanitize_redis_url(user_pass),
            "redis://***@redis-cluster.internal:6379/0"
        );
        let normal = "redis://127.0.0.1:6379";
        assert_eq!(sanitize_redis_url(normal), "redis://127.0.0.1:6379");
    }

    #[tokio::test]
    async fn test_redis_shared_hit_and_independent_ttl_renewal() {
        use super::super::types::{CacheControl, Message, MessagesRequest, SystemMessage};

        let fake_redis = Arc::new(FakeRemoteStore::default());

        let sys_text = "System Prompt 1h stability ".repeat(40);
        let u1_text = "User Question 5m ephemeral ".repeat(40);

        let req = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 32,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": u1_text.clone(),
                    "cache_control": {"type": "ephemeral", "ttl": "5m"}
                }]),
            }],
            system: Some(vec![SystemMessage {
                text: sys_text.clone(),
                cache_control: Some(CacheControl {
                    cache_type: "ephemeral".to_string(),
                    ttl: Some("1h".to_string()),
                }),
            }]),
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            cache_control: None,
            stream: false,
        };

        // 实例 1：挂载 fake_redis，首次请求（冷启动 miss）
        let instance_1 = CacheMeter::with_remote(None, Some(fake_redis.clone()));
        let u1 = compute_cache_usage(&instance_1, &req, 1).await;
        assert_eq!(u1.cache_read, 0, "首次调用应为 miss");
        assert!(u1.cache_covered_est > 0);
        assert_eq!(
            fake_redis
                .record_count
                .load(std::sync::atomic::Ordering::Relaxed),
            2,
            "应向 Redis 写入 2 个断点"
        );

        // 实例 2：全新的本地内存，但挂载相同的 fake_redis 共享层
        let instance_2 = CacheMeter::with_remote(None, Some(fake_redis.clone()));
        assert_eq!(instance_2.len(), 0, "实例 2 本地内存初始为空");

        // 实例 2 请求相同 Prompt：应从共享 Redis 命中！
        let u2 = compute_cache_usage(&instance_2, &req, 1).await;
        assert!(u2.cache_read > 0, "实例 2 必须通过共享 Redis 命中");
        assert_eq!(u2.cache_read, u1.cache_covered_est);
        assert_eq!(instance_2.len(), 2, "命中后断点应回填至实例 2 的本地缓存");

        // 模拟 350 秒后：5m 断点过期（300s），1h 断点依然有效（3600s）
        {
            let mut remote_map = fake_redis.entries.lock();
            for (_, v) in remote_map.iter_mut() {
                v.expires_at -= 350;
            }
            let mut local_map = instance_2.inner.lock();
            for (_, v) in local_map.entries.iter_mut() {
                v.expires_at -= 350;
            }
        }

        // 实例 2 再次请求：5m 断点过期，1h 断点仍然命中
        let u3 = compute_cache_usage(&instance_2, &req, 1).await;
        let sys_tokens = estimate_tokens(&sys_text) as i32;
        assert_eq!(
            u3.cache_read, sys_tokens,
            "350 秒后 5m 消息过期 miss，1h 的 system 断点必须依然命中并滑动续期"
        );
    }

    #[tokio::test]
    async fn test_redis_failure_and_timeout_fallback_to_local() {
        let fake_redis = Arc::new(FakeRemoteStore::default());
        let cache = CacheMeter::with_remote(None, Some(fake_redis.clone()));
        let req = build_request_with_system_breakpoint();

        // 首次请求，写入本地与 Redis
        let u1 = compute_cache_usage(&cache, &req, 1).await;
        assert_eq!(u1.cache_read, 0);
        assert!(u1.cache_covered_est > 0);

        // 模拟 Redis 故障（Lookup 异常 / 超时）
        fake_redis
            .fail_lookups
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // 第二次请求：Redis 失败时必须平滑降级到本地 CacheMeter，正常命中且请求不失败
        let u2 = compute_cache_usage(&cache, &req, 1).await;
        assert!(u2.cache_read > 0, "Redis 故障时应平滑降级到本地缓存并命中");
        assert_eq!(u2.cache_read, u1.cache_covered_est);

        // 模拟 Redis 写入也发生故障
        fake_redis
            .fail_records
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // 第三次请求依然正常完成，不抛错不中断
        let u3 = compute_cache_usage(&cache, &req, 1).await;
        assert!(u3.cache_read > 0);
    }

    #[tokio::test]
    async fn test_redis_concurrency_singleflight_lock() {
        let fake_redis = Arc::new(FakeRemoteStore::default());
        let hash = 0xabcdef1234567890;

        // 首次获取锁成功
        let acq1 = fake_redis.try_acquire_lock(hash, 2000).await;
        assert!(acq1, "首次加锁应成功");

        // 在锁持有期内二次获取应失败（去重锁生效）
        let acq2 = fake_redis.try_acquire_lock(hash, 2000).await;
        assert!(!acq2, "锁未过期时加锁应返回 false");

        // 锁失败不影响 compute_cache_usage
        let cache = CacheMeter::with_remote(None, Some(fake_redis.clone()));
        let req = build_request_with_system_breakpoint();
        let u = compute_cache_usage(&cache, &req, 1).await;
        assert!(
            u.cache_covered_est > 0,
            "即使锁已被占用，也能作为普通 miss 完成处理"
        );
    }

    #[tokio::test]
    async fn test_from_env_fallback_behavior() {
        // 未设置环境变量时，返回纯本地实例
        unsafe {
            std::env::remove_var("KIRO_RS_CACHE_REDIS_URL");
        }
        let cache_local = CacheMeter::from_env(None).await;
        assert!(cache_local.remote.is_none());

        // 设置无效的 Redis 地址时，连接失败平滑降级为本地实例
        unsafe {
            std::env::set_var("KIRO_RS_CACHE_REDIS_URL", "redis://127.0.0.1:9");
        }
        let cache_fallback = CacheMeter::from_env(None).await;
        assert!(cache_fallback.remote.is_none());

        unsafe {
            std::env::remove_var("KIRO_RS_CACHE_REDIS_URL");
        }
    }

    #[test]
    fn test_metering_enabled_from_env() {
        unsafe {
            std::env::remove_var("KIRO_RS_CACHE_METERING");
        }
        assert_eq!(
            CacheMeter::metering_enabled_from_env(),
            None,
            "未设置时不表态，由 config.json / 默认值决定"
        );

        for off in ["0", "off", "OFF", "false", "No", " disabled "] {
            unsafe {
                std::env::set_var("KIRO_RS_CACHE_METERING", off);
            }
            assert_eq!(
                CacheMeter::metering_enabled_from_env(),
                Some(false),
                "{off:?} 应关闭计量模拟"
            );
        }

        for on in ["1", "on", "true", "yes"] {
            unsafe {
                std::env::set_var("KIRO_RS_CACHE_METERING", on);
            }
            assert_eq!(
                CacheMeter::metering_enabled_from_env(),
                Some(true),
                "{on:?} 应开启计量模拟"
            );
        }

        // 空串等同未设置
        unsafe {
            std::env::set_var("KIRO_RS_CACHE_METERING", "   ");
        }
        assert_eq!(
            CacheMeter::metering_enabled_from_env(),
            None,
            "空串视为未表态"
        );

        unsafe {
            std::env::remove_var("KIRO_RS_CACHE_METERING");
        }
    }

    /// 关闭开关后 `compute_cache_usage` 不查不写：既不命中已有条目，也不写入新条目。
    #[tokio::test]
    async fn test_disabled_meter_neither_reads_nor_writes() {
        let cache = CacheMeter::new(None);
        let req = build_request_with_system_breakpoint();

        // 先在开启状态下写入一次，制造可命中的条目
        let warm = compute_cache_usage(&cache, &req, 1).await;
        assert!(warm.cache_covered_est > 0, "开启时应写入缓存条目");

        // 关闭后：即使条目仍在，也不再命中，且返回全零
        cache.set_enabled(false);
        let off = compute_cache_usage(&cache, &req, 1).await;
        assert_eq!(off.cache_read, 0);
        assert_eq!(off.cache_covered_est, 0);
        assert_eq!(off.prompt_total_est, 0, "关闭时不产出任何 estimate 基准");
        assert_eq!(off.split_against_total(9_999), (9_999, 0, 0));

        // 重新开启：此前条目未被清除，应能继续命中
        cache.set_enabled(true);
        let back = compute_cache_usage(&cache, &req, 1).await;
        assert!(back.cache_read > 0, "重新开启后应命中关闭前写入的条目");
    }

    /// 关闭开关后 handler 走 `CacheUsage::default()`：全量计入 input、缓存恒为 0。
    #[test]
    fn test_disabled_metering_reports_zero_cache() {
        let (input, creation, read) = CacheUsage::default().split_against_total(12_345);
        assert_eq!((input, creation, read), (12_345, 0, 0));
    }
}
