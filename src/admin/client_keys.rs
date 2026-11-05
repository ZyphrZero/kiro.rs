//! 客户端 API Key 管理
//!
//! 管理中转站下发的客户端 Key。生成值以 `sk-` 开头；鉴权不校验前缀，只按完整值匹配。
//!
//! 与上游 Kiro 凭据（`KiroCredentials`，`ksk_*`）相互独立：
//! - 上游凭据池：服务对接 Kiro 的"出口"
//! - 客户端 Key：中转站对外的"入口"
//!
//! 持久化为 `client_api_keys.json`（与 `credentials.json` 同目录）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use super::usage_stats::UsageRecord;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BillingMode {
    #[default]
    Credits,
    Calls,
}

/// 单条客户端 Key
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientKey {
    pub id: u64,
    /// 明文 Key（中转站场景，校验需原值，不做 hash）
    pub key: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub total_calls: u64,
    #[serde(default)]
    pub total_successful_calls: u64,
    #[serde(default)]
    pub billing_mode: BillingMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_calls: Option<u64>,
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    #[serde(default)]
    pub total_cache_creation_tokens: u64,
    #[serde(default)]
    pub total_cache_read_tokens: u64,
    /// 累计 credit 计费量（meteringEvent.usage 累加）
    #[serde(default)]
    pub total_credits: f64,
    /// 累计积分（credit）使用上限。None 表示不限制。
    ///
    /// 达到上限后该 Key 的后续请求会被鉴权层拒绝（HTTP 429）。上限基于累计
    /// `total_credits`，通过「重置统计」清零后可重新计费。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_credits: Option<f64>,
    /// 绑定的账号分组名（可选）
    ///
    /// 设置后，用该 Key 发起的请求只会调度到 groups 包含此分组名的上游账号（严格隔离）。
    /// None 表示不绑定分组，可使用全部账号。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 系统 Key（由 config.json apiKey 同步，不可删除、可轮换）。
    /// 老数据无此字段，默认 false。
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_system: bool,
}

/// 鉴权结果：区分「命中」「超额」「未命中」，供中间件返回不同 HTTP 状态。
#[derive(Debug, Clone, PartialEq)]
pub enum KeyAuth {
    /// 命中且未超额，携带 Key id
    Ok(u64),
    /// 命中但已达积分上限
    OverLimit { id: u64, used: f64, limit: f64 },
    CallsOverLimit { id: u64, used: u64, limit: u64 },
    /// 未匹配到任何启用的 Key
    NotFound,
}

/// `by_key` 仅用于判重；鉴权扫描 `entries` 并做常量时间比较。
pub struct ClientKeyManager {
    inner: RwLock<Inner>,
    path: Option<PathBuf>,
}

struct Inner {
    entries: HashMap<u64, ClientKey>,
    by_key: HashMap<String, u64>,
    next_id: u64,
}

impl ClientKeyManager {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                entries: HashMap::new(),
                by_key: HashMap::new(),
                next_id: 1,
            }),
            path: None,
        }
    }

    /// 从文件加载（不存在时返回空管理器）
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let entries: Vec<ClientKey> = if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            if content.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&content)?
            }
        } else {
            Vec::new()
        };

        let mut by_key = HashMap::with_capacity(entries.len());
        let mut by_id = HashMap::with_capacity(entries.len());
        let mut max_id = 0u64;
        for ck in entries {
            max_id = max_id.max(ck.id);
            by_key.insert(ck.key.clone(), ck.id);
            by_id.insert(ck.id, ck);
        }

        Ok(Self {
            inner: RwLock::new(Inner {
                entries: by_id,
                by_key,
                next_id: max_id + 1,
            }),
            path: Some(path),
        })
    }

    fn save_locked(&self, inner: &Inner) {
        let path = match &self.path {
            Some(p) => p,
            None => return,
        };
        let mut list: Vec<&ClientKey> = inner.entries.values().collect();
        list.sort_by_key(|k| k.id);
        match serde_json::to_string_pretty(&list) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("写入客户端 Key 文件失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化客户端 Key 失败: {}", e),
        }
    }

    /// 列表（按 id 升序）
    pub fn list(&self) -> Vec<ClientKey> {
        let inner = self.inner.read();
        let mut list: Vec<ClientKey> = inner.entries.values().cloned().collect();
        list.sort_by_key(|k| k.id);
        list
    }

    /// 生成并保存新 Key。
    pub fn create(
        &self,
        name: String,
        description: Option<String>,
        group: Option<String>,
    ) -> ClientKey {
        self.create_with_key(name, description, group, generate_client_key())
    }

    /// 使用指定明文创建 Key；明文已存在时返回原条目。
    pub fn create_with_key(
        &self,
        name: String,
        description: Option<String>,
        group: Option<String>,
        plaintext: String,
    ) -> ClientKey {
        let mut inner = self.inner.write();
        if let Some(&id) = inner.by_key.get(&plaintext) {
            return inner.entries.get(&id).cloned().expect("by_key 与 entries 应一致");
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let entry = ClientKey {
            id,
            key: plaintext.clone(),
            name,
            description,
            disabled: false,
            created_at: Utc::now().to_rfc3339(),
            last_used_at: None,
            total_calls: 0,
            total_successful_calls: 0,
            billing_mode: BillingMode::default(),
            max_calls: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_creation_tokens: 0,
            total_cache_read_tokens: 0,
            total_credits: 0.0,
            max_credits: None,
            group: group.filter(|g| !g.trim().is_empty()),
            is_system: false,
        };
        inner.by_key.insert(plaintext, id);
        inner.entries.insert(id, entry.clone());
        self.save_locked(&inner);
        entry
    }

    /// 将 `config.apiKey` 同步为唯一的 `id=0` 系统 Key。配置值变化时保留元数据与统计、
    /// 重新启用新值，并删除与新旧明文冲突的非系统条目，使旧值立即失效。
    pub fn sync_system_key(&self, name: String, description: Option<String>, plaintext: String) {
        let mut inner = self.inner.write();
        let previous_key = inner.entries.get(&0).map(|entry| entry.key.clone());
        let mut changed = false;

        if let Some(entry) = inner.entries.get_mut(&0) {
            if entry.key != plaintext {
                entry.key = plaintext.clone();
                entry.disabled = false;
                changed = true;
            }
            if !entry.is_system {
                entry.is_system = true;
                changed = true;
            }
        } else {
            inner.entries.insert(
                0,
                ClientKey {
                    id: 0,
                    key: plaintext.clone(),
                    name,
                    description,
                    disabled: false,
                    created_at: Utc::now().to_rfc3339(),
                    last_used_at: None,
                    total_calls: 0,
                    total_successful_calls: 0,
                    billing_mode: BillingMode::default(),
                    max_calls: None,
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    total_cache_creation_tokens: 0,
                    total_cache_read_tokens: 0,
                    total_credits: 0.0,
                    max_credits: None,
                    group: None,
                    is_system: true,
                },
            );
            changed = true;
        }

        let entries_before = inner.entries.len();
        inner.entries.retain(|id, entry| {
            *id == 0
                || (entry.key != plaintext
                    && previous_key
                        .as_deref()
                        .map(|old_key| entry.key != old_key)
                        .unwrap_or(true))
        });
        changed |= inner.entries.len() != entries_before;

        for (id, entry) in inner.entries.iter_mut() {
            if *id != 0 && entry.is_system {
                entry.is_system = false;
                changed = true;
            }
        }

        let by_key: HashMap<String, u64> = inner
            .entries
            .iter()
            .map(|(id, entry)| (entry.key.clone(), *id))
            .collect();
        changed |= inner.by_key != by_key;
        inner.by_key = by_key;

        if changed {
            self.save_locked(&inner);
        }
    }

    pub fn delete(&self, id: u64) -> bool {
        let mut inner = self.inner.write();
        // 系统 Key 拒绝删除
        if inner.entries.get(&id).map(|e| e.is_system).unwrap_or(false) {
            return false;
        }
        let removed = match inner.entries.remove(&id) {
            Some(e) => {
                inner.by_key.remove(&e.key);
                true
            }
            None => false,
        };
        if removed {
            self.save_locked(&inner);
        }
        removed
    }

    pub fn set_disabled(&self, id: u64, disabled: bool) -> bool {
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                e.disabled = disabled;
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    pub fn update_meta(
        &self,
        id: u64,
        name: Option<String>,
        description: Option<Option<String>>,
        group: Option<Option<String>>,
        billing: Option<(BillingMode, Option<u64>)>,
    ) -> bool {
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                if let Some(n) = name {
                    e.name = n;
                }
                if let Some(d) = description {
                    e.description = d;
                }
                if let Some(g) = group {
                    e.group = g.filter(|s| !s.trim().is_empty());
                }
                if let Some((mode, max_calls)) = billing {
                    e.billing_mode = mode;
                    e.max_calls = max_calls;
                }
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    /// 返回指定 Key 绑定的分组名（None 表示未绑定或 Key 不存在）
    pub fn group_of(&self, id: u64) -> Option<String> {
        self.inner.read().entries.get(&id).and_then(|e| e.group.clone())
    }

    /// 列出所有当前被引用的分组名（仅去重，不带计数）。
    pub fn used_group_names(&self) -> Vec<String> {
        let inner = self.inner.read();
        let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in inner.entries.values() {
            if let Some(g) = &e.group {
                set.insert(g.clone());
            }
        }
        let mut list: Vec<String> = set.into_iter().collect();
        list.sort();
        list
    }

    /// 统计指定分组被多少把 Key 绑定（用于分组管理页 / 删除前提示）。
    pub fn count_with_group(&self, group: &str) -> usize {
        self.inner
            .read()
            .entries
            .values()
            .filter(|e| e.group.as_deref() == Some(group))
            .count()
    }

    /// 指定 id 的 Key 是否为系统 Key（不存在也返回 false）。
    pub fn is_system(&self, id: u64) -> bool {
        self.inner
            .read()
            .entries
            .get(&id)
            .map(|e| e.is_system)
            .unwrap_or(false)
    }

    /// 把所有引用 `old` 的 Key 的 group 字段改为 `new`（分组改名级联用）。
    /// 返回受影响的 Key 数。
    pub fn rename_group(&self, old: &str, new: &str) -> usize {
        let mut inner = self.inner.write();
        let mut affected = 0usize;
        for entry in inner.entries.values_mut() {
            if entry.group.as_deref() == Some(old) {
                entry.group = Some(new.to_string());
                affected += 1;
            }
        }
        if affected > 0 {
            self.save_locked(&inner);
        }
        affected
    }

    /// 把所有引用 `name` 的 Key 的 group 字段清空（强删分组级联用）。
    /// 返回受影响的 Key 数。
    pub fn clear_group(&self, name: &str) -> usize {
        let mut inner = self.inner.write();
        let mut affected = 0usize;
        for entry in inner.entries.values_mut() {
            if entry.group.as_deref() == Some(name) {
                entry.group = None;
                affected += 1;
            }
        }
        if affected > 0 {
            self.save_locked(&inner);
        }
        affected
    }

    /// 生成新明文并保留 id、元数据、分组、统计及状态；旧明文立即失效。
    /// 系统 Key 的调用方必须同步 `config.apiKey`，否则重启时配置值会覆盖轮换结果。
    pub fn rotate(&self, id: u64) -> Option<ClientKey> {
        let new_key = generate_client_key();
        let mut inner = self.inner.write();
        let old_key = inner.entries.get(&id).map(|e| e.key.clone())?;
        inner.by_key.remove(&old_key);
        let entry = inner.entries.get_mut(&id)?;
        entry.key = new_key.clone();
        let snapshot = entry.clone();
        inner.by_key.insert(new_key, id);
        self.save_locked(&inner);
        Some(snapshot)
    }

    /// 重置计数（保留 Key 与名称）
    pub fn reset_stats(&self, id: u64) -> bool {
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                e.total_calls = 0;
                e.total_successful_calls = 0;
                e.total_input_tokens = 0;
                e.total_output_tokens = 0;
                e.total_cache_creation_tokens = 0;
                e.total_cache_read_tokens = 0;
                e.total_credits = 0.0;
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    /// 不校验前缀，常量时间匹配所有启用 Key；命中后更新使用记录。
    ///
    /// 仅返回命中的 Key id（不含额度判定）。需要额度拦截时用 [`Self::verify_and_touch_ex`]。
    #[allow(dead_code)] // 保留的兼容接口，生产路径已切换到 verify_and_touch_ex
    pub fn verify_and_touch(&self, presented: &str) -> Option<u64> {
        match self.verify_and_touch_ex(presented) {
            KeyAuth::Ok(id) => Some(id),
            // 兼容旧语义：超额与未命中都视为 None
            KeyAuth::OverLimit { .. } | KeyAuth::CallsOverLimit { .. } | KeyAuth::NotFound => None,
        }
    }

    /// 常量时间匹配所有启用 Key；命中后按计费模式检查积分或有效调用上限，
    /// 否则累加调用次数并返回 [`KeyAuth::Ok`]。
    pub fn verify_and_touch_ex(&self, presented: &str) -> KeyAuth {
        let mut inner = self.inner.write();
        let mut hit_id: Option<u64> = None;
        for (id, ck) in inner.entries.iter() {
            if ck.disabled {
                continue;
            }
            if ck.key.as_bytes().ct_eq(presented.as_bytes()).into() {
                hit_id = Some(*id);
                // 不 break，继续完整扫描以保持常量时间
            }
        }
        let Some(id) = hit_id else {
            return KeyAuth::NotFound;
        };
        if let Some(entry) = inner.entries.get_mut(&id) {
            match entry.billing_mode {
                BillingMode::Credits => {
                    if let Some(limit) = entry
                        .max_credits
                        .filter(|limit| entry.total_credits >= *limit)
                    {
                        return KeyAuth::OverLimit {
                            id,
                            used: entry.total_credits,
                            limit,
                        };
                    }
                }
                BillingMode::Calls => {
                    if let Some(limit) = entry
                        .max_calls
                        .filter(|limit| entry.total_successful_calls >= *limit)
                    {
                        return KeyAuth::CallsOverLimit {
                            id,
                            used: entry.total_successful_calls,
                            limit,
                        };
                    }
                }
            }
            entry.total_calls += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
        }
        // 不在每次请求都落盘（高频写入），由 record_usage / 定期 flush 持久化
        KeyAuth::Ok(id)
    }

    /// 设置或清除单个 Key 的积分使用上限。`None` 表示取消限制。
    ///
    /// 传入 `Some(v)` 时会归一为非负有限值；小于 0 或非有限值一律拒绝为无效。
    /// 返回 `false` 表示 Key 不存在或入参非法。
    pub fn set_max_credits(&self, id: u64, max_credits: Option<f64>) -> bool {
        if let Some(v) = max_credits {
            if !v.is_finite() || v < 0.0 {
                return false;
            }
        }
        let mut inner = self.inner.write();
        let updated = match inner.entries.get_mut(&id) {
            Some(e) => {
                e.max_credits = max_credits;
                true
            }
            None => false,
        };
        if updated {
            self.save_locked(&inner);
        }
        updated
    }

    /// 成功结算时累计用量；内部压缩轮次保留用量但不重复计次。
    pub fn record_usage(&self, usage: &UsageRecord, count_call: bool) {
        let mut inner = self.inner.write();
        if let Some(entry) = inner.entries.get_mut(&usage.key_id) {
            if count_call {
                entry.total_successful_calls = entry.total_successful_calls.saturating_add(1);
            }
            entry.total_input_tokens += usage.input_tokens;
            entry.total_output_tokens += usage.output_tokens;
            entry.total_cache_creation_tokens += usage.cache_creation_tokens;
            entry.total_cache_read_tokens += usage.cache_read_tokens;
            if usage.credits.is_finite() && usage.credits > 0.0 {
                entry.total_credits += usage.credits;
            }
            entry.last_used_at = Some(Utc::now().to_rfc3339());
        }
        self.save_locked(&inner);
    }

    pub fn record_successful_call(&self, id: u64) {
        let mut inner = self.inner.write();
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.total_successful_calls = entry.total_successful_calls.saturating_add(1);
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            self.save_locked(&inner);
        }
    }

    /// 获取统计后的 active Key 数（未禁用）
    pub fn active_count(&self) -> usize {
        self.inner.read().entries.values().filter(|e| !e.disabled).count()
    }
}

impl Default for ClientKeyManager {
    fn default() -> Self {
        Self::new()
    }
}

fn is_false(b: &bool) -> bool {
    !b
}

/// 生成 `sk-` 前缀 + 32 位 base62 随机字符串
pub fn generate_client_key() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let body: String = (0..32)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();
    format!("sk-{}", body)
}

/// 脱敏展示：保留前 8 个字符（含前缀）和后 4 个字符
pub fn mask_client_key(key: &str) -> String {
    let char_count = key.chars().count();
    if char_count <= 12 {
        return key.to_string();
    }
    let start: String = key.chars().take(8).collect();
    let end: String = key.chars().skip(char_count - 4).collect();
    format!("{start}...{end}")
}

pub fn default_path_in(dir: &Path) -> PathBuf {
    dir.join("client_api_keys.json")
}

pub type SharedClientKeyManager = Arc<ClientKeyManager>;

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(
        key_id: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
        credits: f64,
    ) -> UsageRecord {
        UsageRecord {
            ts: Utc::now().to_rfc3339(),
            key_id,
            credential_id: 1,
            model: "test-model".into(),
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
            credits,
            duration_ms: 0,
            status: "success".into(),
        }
    }

    #[test]
    fn create_and_verify() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        assert!(entry.key.starts_with("sk-"));
        assert_eq!(mgr.verify_and_touch(&entry.key), Some(entry.id));
        assert_eq!(mgr.verify_and_touch("nope"), None);
    }

    #[test]
    fn disabled_key_rejected() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        mgr.set_disabled(entry.id, true);
        assert_eq!(mgr.verify_and_touch(&entry.key), None);
        mgr.set_disabled(entry.id, false);
        assert_eq!(mgr.verify_and_touch(&entry.key), Some(entry.id));
    }

    #[test]
    fn record_usage_accumulates() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        mgr.record_usage(&usage(entry.id, 100, 50, 0, 0, 0.0), true);
        mgr.record_usage(&usage(entry.id, 200, 30, 5, 10, 1.5), true);
        let list = mgr.list();
        let e = list.iter().find(|x| x.id == entry.id).unwrap();
        assert_eq!(e.total_input_tokens, 300);
        assert_eq!(e.total_output_tokens, 80);
        assert_eq!(e.total_cache_creation_tokens, 5);
        assert_eq!(e.total_cache_read_tokens, 10);
        assert_eq!(e.total_successful_calls, 2);
    }

    #[test]
    fn calls_billing_uses_successful_requests_not_auth_attempts_or_credits() {
        let manager = ClientKeyManager::new();
        let entry = manager.create("calls".into(), None, None);
        assert!(manager.update_meta(
            entry.id,
            None,
            None,
            None,
            Some((BillingMode::Calls, Some(2)))
        ));
        assert!(manager.set_max_credits(entry.id, Some(0.0)));
        for _ in 0..4 {
            assert_eq!(
                manager.verify_and_touch_ex(&entry.key),
                KeyAuth::Ok(entry.id)
            );
        }
        manager.record_usage(&usage(entry.id, 10, 20, 0, 0, 50.0), false);
        assert_eq!(manager.list()[0].total_successful_calls, 0);
        manager.record_usage(&usage(entry.id, 10, 20, 0, 0, 50.0), true);
        assert_eq!(
            manager.verify_and_touch_ex(&entry.key),
            KeyAuth::Ok(entry.id)
        );
        manager.record_usage(&usage(entry.id, 0, 0, 0, 0, 0.0), true);
        assert_eq!(
            manager.verify_and_touch_ex(&entry.key),
            KeyAuth::CallsOverLimit {
                id: entry.id,
                used: 2,
                limit: 2,
            }
        );
        assert_eq!(manager.verify_and_touch(&entry.key), None);
        let snapshot = manager.list().remove(0);
        assert_eq!(snapshot.total_calls, 5);
        assert_eq!(snapshot.total_successful_calls, 2);
        assert_eq!(snapshot.total_credits, 100.0);

        let rotated = manager.rotate(entry.id).unwrap();
        assert_eq!(rotated.total_successful_calls, 2);
        assert_eq!(rotated.billing_mode, BillingMode::Calls);
        assert_eq!(rotated.max_calls, Some(2));
        assert!(manager.reset_stats(entry.id));
        assert_eq!(manager.list()[0].total_successful_calls, 0);
        assert_eq!(
            manager.verify_and_touch_ex(&rotated.key),
            KeyAuth::Ok(entry.id)
        );
    }

    #[test]
    fn calls_billing_zero_unlimited_and_mode_switch_keep_counters() {
        let manager = ClientKeyManager::new();
        let entry = manager.create("calls".into(), None, None);
        assert!(manager.update_meta(
            entry.id,
            None,
            None,
            None,
            Some((BillingMode::Calls, Some(0)))
        ));
        assert_eq!(
            manager.verify_and_touch_ex(&entry.key),
            KeyAuth::CallsOverLimit {
                id: entry.id,
                used: 0,
                limit: 0,
            }
        );
        assert!(manager.update_meta(entry.id, None, None, None, Some((BillingMode::Calls, None))));
        assert_eq!(
            manager.verify_and_touch_ex(&entry.key),
            KeyAuth::Ok(entry.id)
        );
        manager.record_usage(&usage(entry.id, 1, 1, 0, 0, 2.0), true);
        assert!(manager.set_max_credits(entry.id, Some(1.0)));
        assert!(manager.update_meta(
            entry.id,
            None,
            None,
            None,
            Some((BillingMode::Credits, None))
        ));
        assert_eq!(
            manager.verify_and_touch_ex(&entry.key),
            KeyAuth::OverLimit {
                id: entry.id,
                used: 2.0,
                limit: 1.0,
            }
        );
        assert!(manager.update_meta(
            entry.id,
            None,
            None,
            None,
            Some((BillingMode::Calls, Some(1)))
        ));
        assert_eq!(
            manager.verify_and_touch_ex(&entry.key),
            KeyAuth::CallsOverLimit {
                id: entry.id,
                used: 1,
                limit: 1,
            }
        );
    }

    #[test]
    fn calls_billing_persists_and_legacy_keys_keep_credit_billing() {
        let path =
            std::env::temp_dir().join(format!("client-key-billing-{}.json", uuid::Uuid::new_v4()));
        let manager = ClientKeyManager::load(&path).unwrap();
        let entry = manager.create("calls".into(), None, None);
        assert!(manager.update_meta(
            entry.id,
            None,
            None,
            None,
            Some((BillingMode::Calls, Some(1)))
        ));
        manager.record_usage(&usage(entry.id, 1, 1, 0, 0, 0.0), true);
        let reloaded = ClientKeyManager::load(&path).unwrap();
        assert_eq!(
            reloaded.verify_and_touch_ex(&entry.key),
            KeyAuth::CallsOverLimit {
                id: entry.id,
                used: 1,
                limit: 1,
            }
        );
        std::fs::remove_file(path).unwrap();

        let mut legacy = serde_json::to_value(entry).unwrap();
        for field in ["billingMode", "maxCalls", "totalSuccessfulCalls"] {
            legacy.as_object_mut().unwrap().remove(field);
        }
        legacy["totalCalls"] = serde_json::json!(123);
        let restored: ClientKey = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.billing_mode, BillingMode::Credits);
        assert_eq!(restored.max_calls, None);
        assert_eq!(restored.total_successful_calls, 0);
        assert_eq!(restored.total_calls, 123);
    }

    #[tokio::test]
    async fn calls_billing_admin_create_update_and_list() {
        use super::super::{handlers, middleware::AdminState, types::*};
        use axum::{
            Json,
            body::to_bytes,
            extract::{Path, Query, State},
            http::StatusCode,
            response::IntoResponse,
        };
        use serde_json::{Value, json};

        let manager = Arc::new(ClientKeyManager::new());
        let tokens = Arc::new(
            crate::kiro::token_manager::MultiTokenManager::new(
                crate::model::config::Config::default(),
                Vec::new(),
                None,
                None,
                false,
            )
            .unwrap(),
        );
        let state = AdminState::new(
            "fixture-admin",
            crate::admin::AdminService::new(tokens, Vec::new()),
            manager.clone(),
            Arc::new(crate::admin::usage_stats::UsageAggregator::new()),
            Arc::new(crate::admin::trace_db::TraceStore::open_in_memory().unwrap()),
            Arc::new(crate::admin::groups::GroupManager::new()),
        );
        let payload =
            serde_json::from_value(json!({"name": "calls", "billingMode": "calls", "maxCalls": 2}))
                .unwrap();
        let response = handlers::create_client_key(State(state.clone()), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let created: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        let id = created["id"].as_u64().unwrap();
        assert_eq!(manager.list()[0].billing_mode, BillingMode::Calls);
        assert_eq!(manager.list()[0].max_calls, Some(2));

        for (payload, expected_limit) in [
            (json!({"billingMode": "calls", "maxCalls": 0}), Some(0)),
            (json!({"billingMode": "calls", "maxCalls": null}), None),
        ] {
            let response = handlers::update_client_key(
                State(state.clone()),
                Path(id),
                Json(serde_json::from_value(payload).unwrap()),
            )
            .await
            .into_response();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(manager.list()[0].max_calls, expected_limit);
        }
        let response = handlers::update_client_key(
            State(state.clone()),
            Path(id),
            Json(serde_json::from_value(json!({"name": "renamed"})).unwrap()),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(manager.list()[0].billing_mode, BillingMode::Calls);
        let response = handlers::update_client_key(
            State(state.clone()),
            Path(id),
            Json(serde_json::from_value(json!({"maxCalls": 5})).unwrap()),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(manager.list()[0].max_calls, None);
        let response = handlers::update_client_key(
            State(state.clone()),
            Path(id + 1),
            Json(serde_json::from_value(json!({"billingMode": "calls", "maxCalls": 5})).unwrap()),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = handlers::list_client_keys(State(state), Query(ClientKeysQuery::default()))
            .await
            .into_response();
        let listed: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(listed["keys"][0]["billingMode"], "calls");
        assert_eq!(listed["keys"][0]["totalSuccessfulCalls"], 0);
        assert!(listed["keys"][0].get("key").is_none());
        assert!(listed["keys"][0].get("maskedKey").is_some());

        for max_calls in [json!(-1), json!(1.5), json!("10")] {
            assert!(
                serde_json::from_value::<CreateClientKeyRequest>(
                    json!({"name": "invalid", "billingMode": "calls", "maxCalls": max_calls})
                )
                .is_err()
            );
            assert!(
                serde_json::from_value::<UpdateClientKeyRequest>(
                    json!({"billingMode": "calls", "maxCalls": max_calls})
                )
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<CreateClientKeyRequest>(
                json!({"name": "invalid", "billingMode": "typo"})
            )
            .is_err()
        );
        let legacy: CreateClientKeyRequest =
            serde_json::from_value(json!({"name": "legacy"})).unwrap();
        assert_eq!(legacy.billing_mode, BillingMode::Credits);
    }

    #[test]
    fn max_credits_blocks_when_exceeded() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        assert!(mgr.set_max_credits(entry.id, Some(2.0)));

        // 未超额：正常放行
        assert_eq!(mgr.verify_and_touch_ex(&entry.key), KeyAuth::Ok(entry.id));

        // 累计到达上限后拒绝
        mgr.record_usage(&usage(entry.id, 0, 0, 0, 0, 2.5), true);
        match mgr.verify_and_touch_ex(&entry.key) {
            KeyAuth::OverLimit { id, used, limit } => {
                assert_eq!(id, entry.id);
                assert!((used - 2.5).abs() < 1e-9);
                assert!((limit - 2.0).abs() < 1e-9);
            }
            other => panic!("expected OverLimit, got {other:?}"),
        }
        // 旧接口兼容：超额返回 None
        assert_eq!(mgr.verify_and_touch(&entry.key), None);

        // 重置统计后恢复放行
        assert!(mgr.reset_stats(entry.id));
        assert_eq!(mgr.verify_and_touch_ex(&entry.key), KeyAuth::Ok(entry.id));

        // 清除上限：不再拦截
        mgr.record_usage(&usage(entry.id, 0, 0, 0, 0, 999.0), true);
        assert!(mgr.set_max_credits(entry.id, None));
        assert_eq!(mgr.verify_and_touch_ex(&entry.key), KeyAuth::Ok(entry.id));
    }

    #[test]
    fn set_max_credits_rejects_invalid() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None);
        assert!(!mgr.set_max_credits(entry.id, Some(-1.0)));
        assert!(!mgr.set_max_credits(entry.id, Some(f64::NAN)));
        assert!(!mgr.set_max_credits(999, Some(5.0)));
        assert!(mgr.set_max_credits(entry.id, Some(0.0)));
    }

    #[test]
    fn mask_format() {
        assert_eq!(mask_client_key("sk-abcdefghijklmnop"), "sk-abcde...mnop");
        assert_eq!(mask_client_key("short"), "short");
        assert_eq!(mask_client_key("密钥🔐测试abcdefgh"), "密钥🔐测试abc...efgh");
    }

    #[test]
    fn rotate_replaces_key_but_keeps_metadata_and_stats() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("kb".to_string(), Some("desc".into()), Some("groupA".into()));
        mgr.record_usage(&usage(entry.id, 100, 50, 5, 10, 1.5), true);
        let old_key = entry.key.clone();
        let rotated = mgr.rotate(entry.id).expect("rotate should succeed");
        assert_ne!(rotated.key, old_key);
        assert!(rotated.key.starts_with("sk-"));
        assert_eq!(rotated.id, entry.id);
        assert_eq!(rotated.name, "kb");
        assert_eq!(rotated.description.as_deref(), Some("desc"));
        assert_eq!(rotated.group.as_deref(), Some("groupA"));
        assert_eq!(rotated.total_input_tokens, 100);
        assert_eq!(rotated.total_output_tokens, 50);
        assert_eq!(mgr.verify_and_touch(&old_key), None);
        assert_eq!(mgr.verify_and_touch(&rotated.key), Some(entry.id));
    }

    #[test]
    fn rotate_unknown_id_returns_none() {
        let mgr = ClientKeyManager::new();
        assert!(mgr.rotate(999).is_none());
    }

    #[test]
    fn sync_system_key_uses_id_zero() {
        let mgr = ClientKeyManager::new();
        mgr.sync_system_key("默认密钥".into(), None, "custom-api-key".into());
        assert!(mgr.is_system(0));
        assert_eq!(mgr.list().first().map(|k| k.id), Some(0));
        assert_eq!(mgr.verify_and_touch("custom-api-key"), Some(0));
        mgr.sync_system_key("默认密钥".into(), None, "custom-api-key".into());
        assert_eq!(mgr.list().iter().filter(|k| k.is_system).count(), 1);
    }

    #[test]
    fn sync_system_key_replaces_config_value_and_revokes_old_key() {
        let mgr = ClientKeyManager::new();
        mgr.sync_system_key("默认密钥".into(), Some("初始描述".into()), "custom-a".into());
        mgr.update_meta(
            0,
            Some("保留名称".into()),
            Some(Some("保留描述".into())),
            Some(Some("group-a".into())),
            None,
        );
        mgr.record_usage(&usage(0, 100, 50, 5, 10, 1.5), true);
        assert_eq!(mgr.verify_and_touch("custom-a"), Some(0));
        mgr.set_disabled(0, true);

        let conflicting = mgr.create_with_key(
            "冲突密钥".into(),
            None,
            None,
            "custom-b".into(),
        );
        assert_ne!(conflicting.id, 0);

        mgr.sync_system_key("默认密钥".into(), None, "custom-b".into());

        assert_eq!(mgr.verify_and_touch("custom-a"), None);
        assert_eq!(mgr.verify_and_touch("custom-b"), Some(0));
        let entries = mgr.list();
        assert_eq!(entries.len(), 1);
        let system = &entries[0];
        assert_eq!(system.id, 0);
        assert!(system.is_system);
        assert_eq!(system.name, "保留名称");
        assert_eq!(system.description.as_deref(), Some("保留描述"));
        assert_eq!(system.group.as_deref(), Some("group-a"));
        assert!(!system.disabled);
        assert_eq!(system.total_input_tokens, 100);
        assert_eq!(system.total_output_tokens, 50);
    }

    #[test]
    fn system_key_cannot_be_deleted() {
        let mgr = ClientKeyManager::new();
        mgr.sync_system_key("默认密钥".into(), None, "custom-api-key".into());
        assert!(!mgr.delete(0), "系统密钥 id=0 不可删除");
        assert!(mgr.is_system(0));
    }
}
