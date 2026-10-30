//! 代理 IP 池管理
//!
//! 独立于凭据管理，存储为 proxy_pool.json
//!
//! 除增删改查外，还提供主动健康检查：周期性（或按需）通过每个代理请求一个
//! 轻量公网探测端点，记录连通性与延迟；连续探测失败达阈值的代理会被自动禁用。
//!
//! 健康检查覆盖两类条目：
//! - 手动登记的代理；
//! - 由 [`ProxyPoolManager::register_detected`] 自动登记的、凭据实际在用但未登记
//!   到池内的代理（含全局代理）。这类条目带 `autoDetected` 标记。
//!
//! 被自动禁用的条目仍会继续探测，恢复连通后自动重新启用；用户手动禁用的条目
//! 不参与探测，也不会被自动启用。

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// 健康检查探测端点：返回 204 No Content 的轻量公网地址，不依赖上游 Kiro。
const PROXY_HEALTH_CHECK_URL: &str = "https://www.gstatic.com/generate_204";
/// 单次探测超时（秒）
const PROXY_PROBE_TIMEOUT_SECS: u64 = 8;
/// 连续探测失败阈值：达到后自动禁用（与凭据的 MAX_FAILURES_PER_CREDENTIAL 对齐）
const MAX_PROXY_PROBE_FAILURES: u32 = 3;

/// 代理健康状态
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyHealth {
    /// 尚未探测
    #[default]
    Unknown,
    /// 最近一次探测成功
    Healthy,
    /// 最近一次探测失败
    Unhealthy,
}

/// 持久化的代理条目
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyEntry {
    pub id: u64,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 健康状态（健康检查结果）
    #[serde(default)]
    pub health: ProxyHealth,
    /// 最近一次成功探测的延迟（毫秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 最近一次探测时间（RFC3339）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    /// 连续探测失败计数（成功后清零）
    #[serde(default)]
    pub consecutive_failures: u32,
    /// 是否由健康检查自动禁用（区别于用户手动禁用）
    #[serde(default)]
    pub auto_disabled: bool,

    /// 是否由自动检测登记（凭据或全局配置在用、但此前未登记到池内）
    #[serde(default)]
    pub auto_detected: bool,
}

fn default_true() -> bool {
    true
}

/// 代理分配结果
pub enum GetUrlResult {
    /// 代理存在且已启用，返回 URL
    Ok(String),
    /// 代理不存在
    NotFound,
    /// 代理存在但已被禁用
    Disabled,
}

/// 一次全量健康检查的摘要
#[derive(Debug, Clone, Default)]
pub struct CheckSummary {
    /// 探测成功数
    pub healthy: usize,
    /// 探测失败数
    pub unhealthy: usize,
    /// 本轮新增的自动禁用数
    pub auto_disabled: usize,
    /// 本轮从自动禁用状态恢复启用的数量
    pub auto_recovered: usize,
}

/// 某个代理 URL 在池内的可用性判定
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyAvailability {
    /// 已登记且可用（启用且最近一次探测未失败）
    Available,
    /// 已登记但不可用（被禁用，或最近一次探测失败）
    Unavailable,
    /// 未登记，无判定依据
    Untracked,
}

/// 单个代理探测结果
enum ProbeResult {
    Ok { latency_ms: u32 },
    Err { error: String },
}

/// 一次探测回写后的状态迁移结果，用于摘要统计
#[derive(Debug, Clone, Copy, Default)]
struct ProbeOutcome {
    /// 本次探测判定为不可用
    unhealthy: bool,
    /// 本次新增自动禁用
    newly_disabled: bool,
    /// 本次从自动禁用状态恢复启用
    recovered: bool,
}

pub struct ProxyPoolManager {
    entries: Mutex<Vec<ProxyEntry>>,
    // 仅需原子自增，不需要与 entries 联锁；约定独立使用，无锁顺序问题
    next_id: AtomicU64,
    path: Option<PathBuf>,
    /// TLS 后端，构建探测用 HTTP client 时需要
    tls_backend: TlsBackend,
}

/// 隐去代理 URL 中的认证信息，供日志输出使用。
///
/// `socks5://user:pass@host:1080` → `socks5://***@host:1080`；不含认证信息时原样返回。
pub fn mask_proxy_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some(parts) => parts,
        None => return url.to_string(),
    };
    match rest.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://***@{host}"),
        None => url.to_string(),
    }
}

/// 校验代理 URL 的 scheme 是否合法
fn validate_proxy_url(url: &str) -> anyhow::Result<()> {
    let valid_schemes = ["http://", "https://", "socks5://", "socks4://"];
    if !valid_schemes.iter().any(|s| url.starts_with(s)) {
        anyhow::bail!(
            "代理 URL scheme 无效，支持: http/https/socks4/socks5（收到: {}）",
            url
        );
    }
    // 简单检查 host:port 存在
    let after_scheme = valid_schemes
        .iter()
        .find(|s| url.starts_with(*s))
        .map(|s| &url[s.len()..])
        .unwrap_or(url);
    // after_scheme 可能是 user:pass@host:port 或 host:port
    let host_part = after_scheme.rsplit('@').next().unwrap_or(after_scheme);
    if !host_part.contains(':') {
        anyhow::bail!("代理 URL 缺少端口号: {}", url);
    }
    Ok(())
}

impl ProxyPoolManager {
    pub fn new(path: Option<PathBuf>, tls_backend: TlsBackend) -> Self {
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Vec<ProxyEntry>>(&s).ok())
            .unwrap_or_default();

        let next_id = entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;

        Self {
            entries: Mutex::new(entries),
            next_id: AtomicU64::new(next_id),
            path,
            tls_backend,
        }
    }

    pub fn list(&self) -> Vec<ProxyEntry> {
        self.entries.lock().clone()
    }

    pub fn add(&self, url: String, label: Option<String>) -> anyhow::Result<ProxyEntry> {
        let url = url.trim().to_string();
        if url.is_empty() {
            anyhow::bail!("代理 URL 不能为空");
        }
        validate_proxy_url(&url)?;

        let mut entries = self.entries.lock();

        if entries.iter().any(|e| e.url == url) {
            anyhow::bail!("代理 URL 已存在: {}", url);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let entry = ProxyEntry {
            id,
            url,
            label,
            enabled: true,
            health: ProxyHealth::Unknown,
            latency_ms: None,
            last_checked_at: None,
            consecutive_failures: 0,
            auto_disabled: false,
            auto_detected: false,
        };
        entries.push(entry.clone());
        drop(entries);

        self.persist()?;
        Ok(entry)
    }

    /// 批量添加：在单次加锁内完成所有插入，最后统一持久化一次
    pub fn batch_add(&self, urls: Vec<String>) -> (Vec<ProxyEntry>, Vec<String>) {
        let mut added = vec![];
        let mut errors = vec![];

        let mut entries = self.entries.lock();
        for url in urls {
            let url = url.trim().to_string();
            if url.is_empty() || url.starts_with('#') {
                continue;
            }
            if let Err(e) = validate_proxy_url(&url) {
                errors.push(e.to_string());
                continue;
            }
            if entries.iter().any(|e| e.url == url) {
                errors.push(format!("代理 URL 已存在: {}", url));
                continue;
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let entry = ProxyEntry {
                id,
                url,
                label: None,
                enabled: true,
                health: ProxyHealth::Unknown,
                latency_ms: None,
                last_checked_at: None,
                consecutive_failures: 0,
                auto_disabled: false,
                auto_detected: false,
            };
            entries.push(entry.clone());
            added.push(entry);
        }
        drop(entries);

        if !added.is_empty() {
            if let Err(e) = self.persist() {
                tracing::warn!("批量添加代理后持久化失败: {}", e);
            }
        }

        (added, errors)
    }

    pub fn delete(&self, id: u64) -> anyhow::Result<()> {
        let mut entries = self.entries.lock();
        let len_before = entries.len();
        entries.retain(|e| e.id != id);
        if entries.len() == len_before {
            anyhow::bail!("代理不存在: {}", id);
        }
        drop(entries);
        self.persist()?;
        Ok(())
    }

    /// 设置代理启用/禁用状态
    ///
    /// 用户手动启用时清除「健康检查自动禁用」标记与连续失败计数，
    /// 让该代理重新参与健康检查与分配。
    pub fn set_enabled(&self, id: u64, enabled: bool) -> anyhow::Result<()> {
        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;
        entry.enabled = enabled;
        if enabled {
            entry.auto_disabled = false;
            entry.consecutive_failures = 0;
        }
        drop(entries);
        self.persist()?;
        Ok(())
    }

    /// 获取代理 URL，区分"不存在"和"已禁用"两种情况
    pub fn get_url(&self, id: u64) -> GetUrlResult {
        match self.entries.lock().iter().find(|e| e.id == id) {
            None => GetUrlResult::NotFound,
            Some(e) if !e.enabled => GetUrlResult::Disabled,
            Some(e) => GetUrlResult::Ok(e.url.clone()),
        }
    }

    /// 获取所有「可用于分配」的代理 URL：已启用且非 Unhealthy
    pub fn assignable_urls(&self) -> Vec<String> {
        self.entries
            .lock()
            .iter()
            .filter(|e| e.enabled && e.health != ProxyHealth::Unhealthy)
            .map(|e| e.url.clone())
            .collect()
    }

    /// 判定指定 URL 在池内的可用性。
    ///
    /// 未登记的 URL 返回 [`ProxyAvailability::Untracked`]：没有探测记录时不做任何
    /// 推断，调用方不应据此轮换凭据。
    pub fn availability(&self, url: &str) -> ProxyAvailability {
        match self.entries.lock().iter().find(|e| e.url == url) {
            None => ProxyAvailability::Untracked,
            Some(e) if e.enabled && e.health != ProxyHealth::Unhealthy => {
                ProxyAvailability::Available
            }
            Some(_) => ProxyAvailability::Unavailable,
        }
    }

    /// 把「实际在用但未登记」的代理 URL 自动登记进池，纳入后续健康检查。
    ///
    /// 已登记的 URL 原样保留（不改动其 enabled / health 等状态）；非法 URL 跳过。
    /// 返回本次新登记的条目数。
    pub fn register_detected(&self, urls: &[String]) -> usize {
        let mut added = 0usize;
        {
            let mut entries = self.entries.lock();
            for url in urls {
                let url = url.trim();
                if url.is_empty() || validate_proxy_url(url).is_err() {
                    continue;
                }
                if entries.iter().any(|e| e.url == url) {
                    continue;
                }
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                entries.push(ProxyEntry {
                    id,
                    url: url.to_string(),
                    label: None,
                    enabled: true,
                    health: ProxyHealth::Unknown,
                    latency_ms: None,
                    last_checked_at: None,
                    consecutive_failures: 0,
                    auto_disabled: false,
                    auto_detected: true,
                });
                added += 1;
                tracing::info!("检测到在用但未登记的代理，已自动登记进池: {}", url);
            }
        }

        if added > 0 {
            if let Err(e) = self.persist() {
                tracing::warn!("自动登记代理后持久化失败: {}", e);
            }
        }
        added
    }

    fn persist(&self) -> anyhow::Result<()> {
        let path = match &self.path {
            Some(p) => p,
            None => return Ok(()),
        };
        let entries = self.entries.lock();
        let json = serde_json::to_string_pretty(&*entries)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

// ============ 健康检查 ============

impl ProxyPoolManager {
    /// 探测单个代理 URL 的连通性与延迟。
    ///
    /// 通过该代理请求 `PROXY_HEALTH_CHECK_URL`，成功（HTTP 2xx/3xx）即视为连通，
    /// 返回往返延迟；任何网络错误或非预期状态码视为失败。
    async fn probe_one(&self, url: &str) -> ProbeResult {
        let proxy = ProxyConfig::new(url);
        let client = match build_client(Some(&proxy), PROXY_PROBE_TIMEOUT_SECS, self.tls_backend) {
            Ok(c) => c,
            Err(e) => {
                return ProbeResult::Err {
                    error: format!("构建探测 client 失败: {}", e),
                };
            }
        };

        let started = Instant::now();
        match client.get(PROXY_HEALTH_CHECK_URL).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() || status.is_redirection() {
                    ProbeResult::Ok {
                        latency_ms: started.elapsed().as_millis().min(u32::MAX as u128) as u32,
                    }
                } else {
                    ProbeResult::Err {
                        error: format!("探测端点返回非预期状态: {}", status),
                    }
                }
            }
            Err(e) => ProbeResult::Err {
                error: e.to_string(),
            },
        }
    }

    /// 将一次探测结果回写到指定条目，并按需触发自动禁用或自动恢复。
    fn apply_probe_result(entry: &mut ProxyEntry, result: &ProbeResult) -> ProbeOutcome {
        entry.last_checked_at = Some(chrono::Utc::now().to_rfc3339());
        match result {
            ProbeResult::Ok { latency_ms } => {
                entry.health = ProxyHealth::Healthy;
                entry.latency_ms = Some(*latency_ms);
                entry.consecutive_failures = 0;
                // 自动禁用的条目恢复连通后自动重新启用；手动禁用的条目保持禁用
                let recovered = entry.auto_disabled;
                if recovered {
                    entry.enabled = true;
                    entry.auto_disabled = false;
                    tracing::info!("代理 #{} 探测恢复连通，已自动重新启用", entry.id);
                }
                ProbeOutcome {
                    unhealthy: false,
                    newly_disabled: false,
                    recovered,
                }
            }
            ProbeResult::Err { error } => {
                entry.health = ProxyHealth::Unhealthy;
                entry.latency_ms = None;
                entry.consecutive_failures += 1;
                tracing::warn!(
                    "代理 #{} 探测失败（{}/{}）: {}",
                    entry.id,
                    entry.consecutive_failures,
                    MAX_PROXY_PROBE_FAILURES,
                    error
                );
                let mut newly_disabled = false;
                if entry.consecutive_failures >= MAX_PROXY_PROBE_FAILURES && entry.enabled {
                    entry.enabled = false;
                    entry.auto_disabled = true;
                    newly_disabled = true;
                    tracing::error!(
                        "代理 #{} 连续探测失败 {} 次，已自动禁用",
                        entry.id,
                        entry.consecutive_failures
                    );
                }
                ProbeOutcome {
                    unhealthy: true,
                    newly_disabled,
                    recovered: false,
                }
            }
        }
    }

    /// 全量健康检查：并发探测所有「已启用」及「自动禁用」代理，回写结果并持久化一次。
    ///
    /// 自动禁用的条目一并探测，以便恢复连通后自动重新启用；用户手动禁用的条目跳过
    /// （手动重新启用会清零计数）。
    pub async fn check_all(&self) -> CheckSummary {
        // 快照待探测的 (id, url)，避免长时间持锁
        let targets: Vec<(u64, String)> = self
            .entries
            .lock()
            .iter()
            .filter(|e| e.enabled || e.auto_disabled)
            .map(|e| (e.id, e.url.clone()))
            .collect();

        if targets.is_empty() {
            return CheckSummary::default();
        }

        let probes = targets
            .iter()
            .map(|(id, url)| async move { (*id, self.probe_one(url).await) });
        let results = futures::future::join_all(probes).await;

        let mut summary = CheckSummary::default();
        {
            let mut entries = self.entries.lock();
            for (id, result) in &results {
                if let Some(entry) = entries.iter_mut().find(|e| e.id == *id) {
                    let outcome = Self::apply_probe_result(entry, result);
                    if outcome.unhealthy {
                        summary.unhealthy += 1;
                    } else {
                        summary.healthy += 1;
                    }
                    if outcome.newly_disabled {
                        summary.auto_disabled += 1;
                    }
                    if outcome.recovered {
                        summary.auto_recovered += 1;
                    }
                }
            }
        }

        if let Err(e) = self.persist() {
            tracing::warn!("健康检查后持久化失败: {}", e);
        }
        summary
    }

    /// 单个代理即时探测（供 UI「测试」按钮调用），回写结果并持久化。
    pub async fn check_one(&self, id: u64) -> anyhow::Result<ProxyEntry> {
        let url = self
            .entries
            .lock()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.url.clone())
            .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;

        let result = self.probe_one(&url).await;

        let entry = {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("代理不存在: {}", id))?;
            Self::apply_probe_result(entry, &result);
            entry.clone()
        };

        self.persist()?;
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(url: &str) -> ProxyEntry {
        ProxyEntry {
            id: 1,
            url: url.to_string(),
            label: None,
            enabled: true,
            health: ProxyHealth::Unknown,
            latency_ms: None,
            last_checked_at: None,
            consecutive_failures: 0,
            auto_disabled: false,
            auto_detected: false,
        }
    }

    #[test]
    fn old_json_without_new_fields_deserializes() {
        // 旧格式 JSON 只有 id/url/label/enabled，新字段应由 serde default 补全
        let json = r#"[{"id":1,"url":"socks5://127.0.0.1:1080","enabled":true}]"#;
        let entries: Vec<ProxyEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.health, ProxyHealth::Unknown);
        assert_eq!(e.latency_ms, None);
        assert_eq!(e.consecutive_failures, 0);
        assert!(!e.auto_disabled);
    }

    #[test]
    fn probe_failure_increments_and_auto_disables_at_threshold() {
        let mut entry = make_entry("socks5://127.0.0.1:1080");
        let err = ProbeResult::Err {
            error: "connection refused".to_string(),
        };
        // 前两次失败：计数累加，仍启用
        for n in 1..MAX_PROXY_PROBE_FAILURES {
            let outcome = ProxyPoolManager::apply_probe_result(&mut entry, &err);
            assert!(outcome.unhealthy);
            assert!(!outcome.newly_disabled);
            assert_eq!(entry.consecutive_failures, n);
            assert!(entry.enabled);
            assert!(!entry.auto_disabled);
        }
        // 第 N 次失败：自动禁用
        let outcome = ProxyPoolManager::apply_probe_result(&mut entry, &err);
        assert!(outcome.newly_disabled);
        assert_eq!(entry.consecutive_failures, MAX_PROXY_PROBE_FAILURES);
        assert!(!entry.enabled);
        assert!(entry.auto_disabled);
    }

    #[test]
    fn probe_success_clears_failures_and_marks_healthy() {
        let mut entry = make_entry("socks5://127.0.0.1:1080");
        entry.consecutive_failures = 2;
        entry.health = ProxyHealth::Unhealthy;
        let ok = ProbeResult::Ok { latency_ms: 123 };
        let outcome = ProxyPoolManager::apply_probe_result(&mut entry, &ok);
        assert!(!outcome.unhealthy);
        assert!(!outcome.newly_disabled);
        assert!(!outcome.recovered);
        assert_eq!(entry.consecutive_failures, 0);
        assert_eq!(entry.health, ProxyHealth::Healthy);
        assert_eq!(entry.latency_ms, Some(123));
    }

    #[test]
    fn auto_disabled_entry_recovers_on_successful_probe() {
        let mut entry = make_entry("socks5://127.0.0.1:1080");
        entry.enabled = false;
        entry.auto_disabled = true;
        entry.health = ProxyHealth::Unhealthy;
        entry.consecutive_failures = MAX_PROXY_PROBE_FAILURES;

        let outcome = ProxyPoolManager::apply_probe_result(&mut entry, &ProbeResult::Ok {
            latency_ms: 50,
        });
        assert!(outcome.recovered);
        assert!(entry.enabled);
        assert!(!entry.auto_disabled);
        assert_eq!(entry.consecutive_failures, 0);
    }

    #[test]
    fn manually_disabled_entry_stays_disabled_on_successful_probe() {
        let mut entry = make_entry("socks5://127.0.0.1:1080");
        entry.enabled = false;
        entry.auto_disabled = false;

        let outcome = ProxyPoolManager::apply_probe_result(&mut entry, &ProbeResult::Ok {
            latency_ms: 50,
        });
        assert!(!outcome.recovered);
        assert!(!entry.enabled);
    }

    #[test]
    fn register_detected_adds_only_untracked_valid_urls() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let existing = mgr.add("socks5://127.0.0.1:1080".to_string(), None).unwrap();

        let added = mgr.register_detected(&[
            "socks5://127.0.0.1:1080".to_string(), // 已登记，跳过
            "http://10.0.0.1:8080".to_string(),    // 新增
            "not-a-proxy".to_string(),             // 非法，跳过
            String::new(),                         // 空，跳过
        ]);
        assert_eq!(added, 1);

        let list = mgr.list();
        assert_eq!(list.len(), 2);
        let detected = list.iter().find(|e| e.url == "http://10.0.0.1:8080").unwrap();
        assert!(detected.auto_detected);
        assert!(detected.enabled);
        // 已登记条目不被自动检测改写
        let kept = list.iter().find(|e| e.id == existing.id).unwrap();
        assert!(!kept.auto_detected);
    }

    #[test]
    fn availability_distinguishes_untracked_from_unavailable() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let entry = mgr.add("socks5://127.0.0.1:1080".to_string(), None).unwrap();

        assert_eq!(
            mgr.availability("socks5://127.0.0.1:1080"),
            ProxyAvailability::Available
        );
        assert_eq!(
            mgr.availability("socks5://127.0.0.1:9999"),
            ProxyAvailability::Untracked
        );

        mgr.set_enabled(entry.id, false).unwrap();
        assert_eq!(
            mgr.availability("socks5://127.0.0.1:1080"),
            ProxyAvailability::Unavailable
        );
    }

    #[test]
    fn set_enabled_true_clears_auto_disable_state() {
        let mgr = ProxyPoolManager::new(None, TlsBackend::Rustls);
        let entry = mgr.add("socks5://127.0.0.1:1080".to_string(), None).unwrap();
        // 模拟自动禁用状态
        {
            let mut entries = mgr.entries.lock();
            let e = entries.iter_mut().find(|e| e.id == entry.id).unwrap();
            e.enabled = false;
            e.auto_disabled = true;
            e.consecutive_failures = MAX_PROXY_PROBE_FAILURES;
        }
        mgr.set_enabled(entry.id, true).unwrap();
        let list = mgr.list();
        let e = list.iter().find(|e| e.id == entry.id).unwrap();
        assert!(e.enabled);
        assert!(!e.auto_disabled);
        assert_eq!(e.consecutive_failures, 0);
    }
}
