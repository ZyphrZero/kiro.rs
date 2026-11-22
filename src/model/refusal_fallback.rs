//! 上游内容过滤拒答（`CONTENT_FILTERED`）自动回退配置与会话粘性管理器
//!
//! 当模型（如 Claude Opus 5.5）因严格分类器（例如 `CYBER`）在尚未向客户端输出正文或工具调用前
//! 返回 `CONTENT_FILTERED` 时，按用户配置自动回退到指定的常规模型（默认预置映射
//! `claude-opus-5.5 -> claude-opus-4.8`，总开关默认关闭，需显式启用）。
//!
//! 可选支持会话级短 TTL 粘性回退：同一 `conversationId` 触发拒答并回退后，在 TTL 内后续轮次
//! 直接走回退模型，避免每轮先等待主模型几十秒再撞墙，同时持续命中回退模型的 Prompt Cache。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

/// 默认会话级粘性回退时长（秒）：15 分钟
pub const DEFAULT_REFUSAL_FALLBACK_SESSION_TTL_SECS: u64 = 15 * 60;

/// 会话级粘性回退时长上限（秒）：24 小时
pub const MAX_REFUSAL_FALLBACK_SESSION_TTL_SECS: u64 = 24 * 3600;

/// 会话级粘性表最大条目数，防止异常流量导致内存无限增长
const MAX_SESSION_BINDINGS: usize = 4096;

/// 默认回退模型映射（总开关默认关闭，开启后开箱即用支持 Opus 5.5 -> Opus 4.8）
pub fn default_refusal_fallback_models() -> HashMap<String, String> {
    HashMap::from([("claude-opus-5.5".to_string(), "claude-opus-4.8".to_string())])
}

pub fn default_refusal_fallback_session_ttl_secs() -> u64 {
    DEFAULT_REFUSAL_FALLBACK_SESSION_TTL_SECS
}

/// 拒答自动回退配置（持久化于 `config.json` 的 `refusalFallback` 字段）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RefusalFallbackConfig {
    /// 总开关（默认 false，显式开启才生效）
    #[serde(default)]
    pub enabled: bool,
    /// 源模型 → 回退目标模型映射（匹配时按 `map_model` 规范化并忽略大小写）
    #[serde(default = "default_refusal_fallback_models")]
    pub models: HashMap<String, String>,
    /// 同会话粘性回退时长（秒；0 表示关闭粘性仅单轮回退；默认 900 = 15 分钟）
    #[serde(default = "default_refusal_fallback_session_ttl_secs")]
    pub session_ttl_secs: u64,
}

impl Default for RefusalFallbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            models: default_refusal_fallback_models(),
            session_ttl_secs: default_refusal_fallback_session_ttl_secs(),
        }
    }
}

/// 运行期统计快照
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefusalFallbackStats {
    /// 已触发拒答自动回退的总次数
    pub fallback_triggered: u64,
    /// 会话粘性命中、直接路由至回退模型的总次数
    pub sticky_hits: u64,
    /// 当前活跃的会话粘性绑定数
    pub active_sessions: usize,
}

struct StickyFallbackEntry {
    target_model: String,
    expires_at: Instant,
}

/// 线程安全的热重载拒答回退管理器
#[derive(Clone)]
pub struct RefusalFallbackManager {
    config: Arc<RwLock<RefusalFallbackConfig>>,
    sessions: Arc<Mutex<HashMap<(String, String), StickyFallbackEntry>>>,
    fallback_triggered: Arc<AtomicU64>,
    sticky_hits: Arc<AtomicU64>,
}

impl RefusalFallbackManager {
    pub fn new(config: RefusalFallbackConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            fallback_triggered: Arc::new(AtomicU64::new(0)),
            sticky_hits: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 规范化模型匹配键：优先走 `converter::map_model`（统一 `claude-opus-5-5` / `claude-opus-5.5` /
    /// `-thinking` 后缀），再统一小写。
    pub fn canonical_model_key(model: &str) -> String {
        let trimmed = model.trim();
        crate::anthropic::converter::map_model(trimmed)
            .unwrap_or_else(|| trimmed.to_string())
            .to_ascii_lowercase()
    }

    /// 校验配置合法性（Admin API 保存前调用）
    pub fn validate_config(cfg: &RefusalFallbackConfig) -> anyhow::Result<()> {
        if cfg.session_ttl_secs > MAX_REFUSAL_FALLBACK_SESSION_TTL_SECS {
            anyhow::bail!(
                "会话粘性回退时长不能超过 {} 秒（24 小时）",
                MAX_REFUSAL_FALLBACK_SESSION_TTL_SECS
            );
        }
        let mut normalized_pairs: HashMap<String, String> = HashMap::new();
        for (src, dst) in &cfg.models {
            let src_trim = src.trim();
            let dst_trim = dst.trim();
            if src_trim.is_empty() || dst_trim.is_empty() {
                anyhow::bail!("回退映射的源模型和目标模型均不能为空");
            }
            let src_key = Self::canonical_model_key(src_trim);
            let dst_key = Self::canonical_model_key(dst_trim);
            if src_key == dst_key {
                anyhow::bail!("回退映射不能指向自身: {} -> {}", src_trim, dst_trim);
            }
            if normalized_pairs.insert(src_key.clone(), dst_key).is_some() {
                anyhow::bail!("存在重复的源模型规则（规范化后冲突）: {}", src_trim);
            }
        }
        // 避免简单环路（A -> B 且 B -> A）
        for (src_key, dst_key) in &normalized_pairs {
            if normalized_pairs.get(dst_key) == Some(src_key) {
                anyhow::bail!("回退映射不能存在循环指向: {} <-> {}", src_key, dst_key);
            }
        }
        Ok(())
    }

    /// 规范化配置中的前后空白（保存前清理）
    pub fn sanitized_config(cfg: RefusalFallbackConfig) -> RefusalFallbackConfig {
        let models = cfg
            .models
            .into_iter()
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .filter(|(k, v)| !k.is_empty() && !v.is_empty())
            .collect();
        RefusalFallbackConfig {
            enabled: cfg.enabled,
            models,
            session_ttl_secs: cfg.session_ttl_secs,
        }
    }

    /// 判断上游 `stopDetails.refusal.category` 是否允许自动回退。
    ///
    /// - `None` / 空串（Kiro 裸 `CONTENT_FILTERED` 未带细分类别）：允许
    /// - `CYBER`（大小写不敏感，Anthropic/Bedrock 官方支持回退的误报高发类别）：允许
    /// - 其它显式高危类别（如 `BIO` / `CBRN` / `CSAM` / `SELF_HARM` 等）：拒绝回退
    pub fn is_fallback_eligible_category(category: Option<&str>) -> bool {
        match category.map(str::trim).filter(|s| !s.is_empty()) {
            None => true,
            Some(cat) => cat.eq_ignore_ascii_case("cyber"),
        }
    }

    /// 获取当前配置快照
    pub fn config(&self) -> RefusalFallbackConfig {
        self.config.read().clone()
    }

    /// 热更新配置；若关闭或变更了映射，自动清理不再匹配的旧会话粘性条目
    pub fn set_config(&self, next: RefusalFallbackConfig) {
        let next = Self::sanitized_config(next);
        {
            let mut guard = self.config.write();
            *guard = next.clone();
        }
        let mut sessions = self.sessions.lock();
        if !next.enabled || next.session_ttl_secs == 0 {
            sessions.clear();
        } else {
            let now = Instant::now();
            sessions.retain(|(_, src_key), entry| {
                entry.expires_at > now
                    && Self::lookup_in_map(&next.models, src_key)
                        .is_some_and(|t| t == entry.target_model)
            });
        }
    }

    /// 获取运行期统计快照
    pub fn stats(&self) -> RefusalFallbackStats {
        let now = Instant::now();
        let active_sessions = {
            let mut sessions = self.sessions.lock();
            sessions.retain(|_, v| v.expires_at > now);
            sessions.len()
        };
        RefusalFallbackStats {
            fallback_triggered: self.fallback_triggered.load(Ordering::Relaxed),
            sticky_hits: self.sticky_hits.load(Ordering::Relaxed),
            active_sessions,
        }
    }

    fn lookup_in_map(models: &HashMap<String, String>, requested_model: &str) -> Option<String> {
        let raw_lower = requested_model.trim().to_ascii_lowercase();
        let canonical = Self::canonical_model_key(requested_model);

        for (src, dst) in models {
            let src_trim = src.trim();
            let dst_trim = dst.trim();
            if src_trim.is_empty() || dst_trim.is_empty() {
                continue;
            }
            if src_trim.eq_ignore_ascii_case(&raw_lower)
                || Self::canonical_model_key(src_trim) == canonical
            {
                if Self::canonical_model_key(dst_trim) == canonical {
                    return None;
                }
                return Some(dst_trim.to_string());
            }
        }
        None
    }

    /// 查询某请求模型在触发拒答时是否配置了回退目标模型
    pub fn lookup_target_model(&self, requested_model: &str) -> Option<String> {
        let cfg = self.config.read();
        if !cfg.enabled {
            return None;
        }
        Self::lookup_in_map(&cfg.models, requested_model)
    }

    /// 请求发起前检查该会话是否处于粘性回退窗口内；命中时续期并返回回退目标模型
    pub fn lookup_sticky_session(
        &self,
        conversation_id: &str,
        requested_model: &str,
    ) -> Option<String> {
        if conversation_id.is_empty() {
            return None;
        }
        let (enabled, ttl_secs, configured_target) = {
            let cfg = self.config.read();
            if !cfg.enabled || cfg.session_ttl_secs == 0 {
                return None;
            }
            (
                cfg.enabled,
                cfg.session_ttl_secs,
                Self::lookup_in_map(&cfg.models, requested_model),
            )
        };
        if !enabled {
            return None;
        }
        let configured_target = configured_target?;
        let key = (
            conversation_id.to_string(),
            Self::canonical_model_key(requested_model),
        );
        let now = Instant::now();
        let mut sessions = self.sessions.lock();
        if let Some(entry) = sessions.get_mut(&key) {
            if entry.expires_at > now && entry.target_model == configured_target {
                entry.expires_at = now + Duration::from_secs(ttl_secs);
                self.sticky_hits.fetch_add(1, Ordering::Relaxed);
                return Some(entry.target_model.clone());
            }
            sessions.remove(&key);
        }
        None
    }

    /// 记录一次成功触发的拒答回退，并按 TTL 绑定会话粘性
    pub fn record_session_fallback(
        &self,
        conversation_id: &str,
        requested_model: &str,
        target_model: &str,
    ) {
        self.fallback_triggered.fetch_add(1, Ordering::Relaxed);
        if conversation_id.is_empty() {
            return;
        }
        let ttl_secs = {
            let cfg = self.config.read();
            if !cfg.enabled || cfg.session_ttl_secs == 0 {
                return;
            }
            cfg.session_ttl_secs
        };
        let now = Instant::now();
        let key = (
            conversation_id.to_string(),
            Self::canonical_model_key(requested_model),
        );
        let mut sessions = self.sessions.lock();
        if sessions.len() >= MAX_SESSION_BINDINGS && !sessions.contains_key(&key) {
            sessions.retain(|_, v| v.expires_at > now);
            if sessions.len() >= MAX_SESSION_BINDINGS {
                if let Some(oldest_key) = sessions
                    .iter()
                    .min_by_key(|(_, v)| v.expires_at)
                    .map(|(k, _)| k.clone())
                {
                    sessions.remove(&oldest_key);
                }
            }
        }
        sessions.insert(
            key,
            StickyFallbackEntry {
                target_model: target_model.to_string(),
                expires_at: now + Duration::from_secs(ttl_secs),
            },
        );
    }

    /// 清理所有已过期的会话粘性绑定
    pub fn evict_expired(&self) {
        let now = Instant::now();
        self.sessions.lock().retain(|_, v| v.expires_at > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_disabled_with_opus_5_5_to_4_8_preset() {
        let cfg = RefusalFallbackConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(
            cfg.models.get("claude-opus-5.5").map(String::as_str),
            Some("claude-opus-4.8")
        );
        assert_eq!(cfg.session_ttl_secs, 900);
    }

    #[test]
    fn lookup_matches_canonical_variants_only_when_enabled() {
        let mgr = RefusalFallbackManager::new(RefusalFallbackConfig::default());
        assert_eq!(mgr.lookup_target_model("claude-opus-5.5"), None);

        mgr.set_config(RefusalFallbackConfig {
            enabled: true,
            ..Default::default()
        });
        assert_eq!(
            mgr.lookup_target_model("claude-opus-5.5").as_deref(),
            Some("claude-opus-4.8")
        );
        assert_eq!(
            mgr.lookup_target_model("claude-opus-5-5").as_deref(),
            Some("claude-opus-4.8")
        );
        assert_eq!(
            mgr.lookup_target_model("claude-opus-5-5-thinking").as_deref(),
            Some("claude-opus-4.8")
        );
        assert_eq!(mgr.lookup_target_model("claude-opus-4.8"), None);
    }

    #[test]
    fn sticky_session_binding_tracks_hits_and_clears_when_disabled() {
        let mgr = RefusalFallbackManager::new(RefusalFallbackConfig {
            enabled: true,
            session_ttl_secs: 600,
            ..Default::default()
        });
        assert_eq!(
            mgr.lookup_sticky_session("sess-1", "claude-opus-5.5"),
            None
        );

        mgr.record_session_fallback("sess-1", "claude-opus-5-5", "claude-opus-4.8");
        assert_eq!(
            mgr.lookup_sticky_session("sess-1", "claude-opus-5.5").as_deref(),
            Some("claude-opus-4.8")
        );
        assert_eq!(
            mgr.lookup_sticky_session("sess-2", "claude-opus-5.5"),
            None
        );

        let stats = mgr.stats();
        assert_eq!(stats.fallback_triggered, 1);
        assert_eq!(stats.sticky_hits, 1);
        assert_eq!(stats.active_sessions, 1);

        // 关闭开关时立即清空粘性表
        mgr.set_config(RefusalFallbackConfig {
            enabled: false,
            ..Default::default()
        });
        assert_eq!(mgr.stats().active_sessions, 0);
        assert_eq!(
            mgr.lookup_sticky_session("sess-1", "claude-opus-5.5"),
            None
        );
    }

    #[test]
    fn eligible_category_allows_bare_and_cyber_only() {
        assert!(RefusalFallbackManager::is_fallback_eligible_category(None));
        assert!(RefusalFallbackManager::is_fallback_eligible_category(Some("")));
        assert!(RefusalFallbackManager::is_fallback_eligible_category(Some("CYBER")));
        assert!(RefusalFallbackManager::is_fallback_eligible_category(Some("cyber")));
        assert!(!RefusalFallbackManager::is_fallback_eligible_category(Some("BIO")));
        assert!(!RefusalFallbackManager::is_fallback_eligible_category(Some("CBRN")));
    }

    #[test]
    fn validate_config_rejects_self_loops_cycles_and_excessive_ttl() {
        assert!(RefusalFallbackManager::validate_config(&RefusalFallbackConfig::default()).is_ok());

        let self_loop = RefusalFallbackConfig {
            enabled: true,
            models: HashMap::from([("claude-opus-5.5".into(), "claude-opus-5-5".into())]),
            session_ttl_secs: 900,
        };
        assert!(RefusalFallbackManager::validate_config(&self_loop).is_err());

        let cycle = RefusalFallbackConfig {
            enabled: true,
            models: HashMap::from([
                ("claude-opus-5.5".into(), "claude-opus-4.8".into()),
                ("claude-opus-4.8".into(), "claude-opus-5.5".into()),
            ]),
            session_ttl_secs: 900,
        };
        assert!(RefusalFallbackManager::validate_config(&cycle).is_err());

        let too_long = RefusalFallbackConfig {
            enabled: true,
            models: default_refusal_fallback_models(),
            session_ttl_secs: MAX_REFUSAL_FALLBACK_SESSION_TTL_SECS + 1,
        };
        assert!(RefusalFallbackManager::validate_config(&too_long).is_err());
    }
}
