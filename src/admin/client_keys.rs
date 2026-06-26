//! 客户端 API Key 管理
//!
//! 中转站对外分发的"客户端 Key"层。客户端调用 `/v1/messages` 时携带 `csk_*`
//! 格式的 Key，由本模块校验并按 Key 维度记录调用次数与累计 Token。
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

/// 客户端 Key 前缀（区分上游 `ksk_`）
pub const CLIENT_KEY_PREFIX: &str = "csk_";

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
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    #[serde(default)]
    pub total_cache_creation_tokens: u64,
    #[serde(default)]
    pub total_cache_read_tokens: u64,
    /// 是否启用中转层 prompt cache 计量与命中。
    ///
    /// 老数据无此字段时默认 true，避免升级后已有 Key 行为变化。
    #[serde(default = "default_cache_enabled")]
    pub cache_enabled: bool,
    /// 历史上限（History Cap）三态开关。`None`=随全局默认；`Some(true)`=强制开；
    /// `Some(false)`=强制关。开启后发往上游的历史会按字节预算裁剪（见
    /// `converter::apply_history_cap`）。老数据无此字段时为 None（随全局，默认关）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_cap: Option<bool>,
    /// 快速模式（Fast Mode）三态开关。`None`=随全局默认；`Some(true/false)`=强制开/关。
    /// 开启后：不死等账号（短超时换号）+ 流式复用连接池 + 更激进历史预算。
    /// 老数据无此字段时为 None（随全局，默认关）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_mode: Option<bool>,
    /// 累计 credit 计费量（meteringEvent.usage 累加）
    #[serde(default)]
    pub total_credits: f64,
    /// 绑定的账号分组名（可选）
    ///
    /// 设置后，用该 Key 发起的请求只会调度到 groups 包含此分组名的上游账号（严格隔离）。
    /// None 表示不绑定分组，可使用全部账号（与 master apiKey 行为一致）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 系统 Key（由 config.json apiKey bootstrap 生成，不可删除 / 不可轮换）。
    /// 老数据无此字段，默认 false。
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_system: bool,
}

/// 客户端 Key 管理器
///
/// 内部双索引：
/// - `by_key: HashMap<String, u64>` —— 用于 `/v1` 鉴权时 O(1) 查询命中
/// - `entries: HashMap<u64, ClientKey>` —— 用于按 id 读写明细
///
/// 校验比对仍使用 `subtle::ConstantTimeEq` 防止时序攻击。
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

    /// 创建新 Key（生成明文随机串），返回新建条目
    pub fn create(
        &self,
        name: String,
        description: Option<String>,
        group: Option<String>,
        cache_enabled: bool,
    ) -> ClientKey {
        if cache_enabled {
            self.create_with_key(name, description, group, generate_client_key())
        } else {
            self.create_with_key_and_cache(name, description, group, generate_client_key(), false)
        }
    }

    /// 用指定明文创建 Key（仅供首次启动 bootstrap 用，把 config.json apiKey 直接导入为第一条分发密钥）。
    /// 若该明文已存在则跳过，返回已存在的条目。
    pub fn create_with_key(
        &self,
        name: String,
        description: Option<String>,
        group: Option<String>,
        plaintext: String,
    ) -> ClientKey {
        self.create_with_key_and_cache(name, description, group, plaintext, true)
    }

    fn create_with_key_and_cache(
        &self,
        name: String,
        description: Option<String>,
        group: Option<String>,
        plaintext: String,
        cache_enabled: bool,
    ) -> ClientKey {
        let mut inner = self.inner.write();
        // 防止 bootstrap 重复导入同一明文
        if let Some(&id) = inner.by_key.get(&plaintext) {
            return inner
                .entries
                .get(&id)
                .cloned()
                .expect("by_key 与 entries 应一致");
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
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_creation_tokens: 0,
            total_cache_read_tokens: 0,
            cache_enabled,
            history_cap: None,
            fast_mode: None,
            total_credits: 0.0,
            group: group.filter(|g| !g.trim().is_empty()),
            is_system: false,
        };
        inner.by_key.insert(plaintext, id);
        inner.entries.insert(id, entry.clone());
        self.save_locked(&inner);
        entry
    }

    /// 确保 config.json apiKey 对应的系统 Key 存在（幂等，每次启动调用）。
    ///
    /// 系统 Key **固定占用 id=0**：历史 master apiKey 的用量数据都记在 keyId=0 桶里，
    /// 固定 id=0 可让「默认密钥」直接查到升级前的全部用量，保证数据连续。
    ///
    /// - 若该明文已在 id=0：确保 is_system=true（no-op）。
    /// - 若该明文在其它 id（旧版 bootstrap 误建）：迁移到 id=0。
    /// - 明文不存在：在 id=0 新建（id=0 被占用时回退 next_id，极罕见）。
    /// 系统 Key 不可删除；可轮换（轮换时同步 config.apiKey）。
    pub fn ensure_system_key(&self, name: String, description: Option<String>, plaintext: String) {
        let mut inner = self.inner.write();
        match inner.by_key.get(&plaintext).copied() {
            Some(0) => {
                // 已在 id=0：确保 is_system
                if let Some(e) = inner.entries.get_mut(&0) {
                    if !e.is_system {
                        e.is_system = true;
                        self.save_locked(&inner);
                    }
                }
            }
            Some(other) => {
                // 明文在非 0 id 上：尽量迁移到 id=0（对齐历史 keyId=0 用量）
                if !inner.entries.contains_key(&0) {
                    let mut entry = inner
                        .entries
                        .remove(&other)
                        .expect("by_key 与 entries 应一致");
                    entry.id = 0;
                    entry.is_system = true;
                    inner.entries.insert(0, entry);
                    inner.by_key.insert(plaintext, 0);
                    self.save_locked(&inner);
                } else if let Some(e) = inner.entries.get_mut(&other) {
                    // id=0 被别的 Key 占用：仅在原位标记 system
                    if !e.is_system {
                        e.is_system = true;
                        self.save_locked(&inner);
                    }
                }
            }
            None => {
                // 明文不存在：在 id=0 新建（占用则回退 next_id）
                let id = if !inner.entries.contains_key(&0) {
                    0
                } else {
                    let id = inner.next_id;
                    inner.next_id += 1;
                    id
                };
                let entry = ClientKey {
                    id,
                    key: plaintext.clone(),
                    name,
                    description,
                    disabled: false,
                    created_at: Utc::now().to_rfc3339(),
                    last_used_at: None,
                    total_calls: 0,
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    total_cache_creation_tokens: 0,
                    total_cache_read_tokens: 0,
                    cache_enabled: true,
                    history_cap: None,
                    fast_mode: None,
                    total_credits: 0.0,
                    group: None,
                    is_system: true,
                };
                inner.by_key.insert(plaintext, id);
                inner.entries.insert(id, entry);
                self.save_locked(&inner);
            }
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
        cache_enabled: Option<bool>,
        history_cap: Option<Option<bool>>,
        fast_mode: Option<Option<bool>>,
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
                if let Some(enabled) = cache_enabled {
                    e.cache_enabled = enabled;
                }
                // 外层 Some 表示"本次更新此字段"；内层值即三态设定（None=随全局）。
                if let Some(hc) = history_cap {
                    e.history_cap = hc;
                }
                if let Some(fm) = fast_mode {
                    e.fast_mode = fm;
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
        self.inner
            .read()
            .entries
            .get(&id)
            .and_then(|e| e.group.clone())
    }

    /// 返回指定 Key 是否启用 prompt cache。不存在时保守关闭。
    pub fn cache_enabled_of(&self, id: u64) -> bool {
        self.inner
            .read()
            .entries
            .get(&id)
            .map(|e| e.cache_enabled)
            .unwrap_or(false)
    }

    /// 返回指定 Key 的 History Cap 三态设定（None=随全局默认；Key 不存在亦为 None）。
    pub fn history_cap_of(&self, id: u64) -> Option<bool> {
        self.inner
            .read()
            .entries
            .get(&id)
            .and_then(|e| e.history_cap)
    }

    /// 返回指定 Key 的 Fast Mode 三态设定（None=随全局默认；Key 不存在亦为 None）。
    pub fn fast_mode_of(&self, id: u64) -> Option<bool> {
        self.inner
            .read()
            .entries
            .get(&id)
            .and_then(|e| e.fast_mode)
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

    /// 轮换 Key 值：旧 Key 立即失效，生成新明文，保留 id/name/description/group/统计/disabled/is_system。
    /// 用于「明文遗失」「下游怀疑泄漏」场景，比删后重建更安全（不会丢统计与分组绑定）。
    /// 命中且替换成功返回新条目（含新明文）；id 不存在返回 None。
    /// 注意：系统 Key 轮换后调用方需把新明文同步写回 config.json apiKey，避免下次启动重复导入。
    pub fn rotate(&self, id: u64) -> Option<ClientKey> {
        let new_key = generate_client_key();
        let mut inner = self.inner.write();
        // 取出旧条目并从 by_key 索引摘除
        let old_key = inner.entries.get(&id).map(|e| e.key.clone())?;
        inner.by_key.remove(&old_key);
        // 写入新明文 + 索引（is_system 等其余字段保留不变）
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

    /// 校验 Key，命中且未禁用则返回 id；同时更新 `last_used_at`/`total_calls`
    ///
    /// 用 `ConstantTimeEq` 对所有 active Key 做常量时间比对，防止时序攻击；
    /// 之前的 HashMap 直接 lookup 仅作快速短路（命中后还会再做一次常量时间比较）。
    pub fn verify_and_touch(&self, presented: &str) -> Option<u64> {
        if !presented.starts_with(CLIENT_KEY_PREFIX) {
            return None;
        }
        let mut inner = self.inner.write();
        // 第一遍：扫描所有 entry 做常量时间比较，避免 HashMap 短路泄露
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
        let id = hit_id?;
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.total_calls += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
        }
        // 不在每次请求都落盘（高频写入），由 record_usage / 定期 flush 持久化
        Some(id)
    }

    /// 在请求结束时累计 Token 用量并落盘
    pub fn record_usage(
        &self,
        id: u64,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
        credits: f64,
    ) {
        let mut inner = self.inner.write();
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.total_input_tokens += input_tokens;
            entry.total_output_tokens += output_tokens;
            entry.total_cache_creation_tokens += cache_creation_tokens;
            entry.total_cache_read_tokens += cache_read_tokens;
            if credits.is_finite() && credits > 0.0 {
                entry.total_credits += credits;
            }
            entry.last_used_at = Some(Utc::now().to_rfc3339());
        }
        self.save_locked(&inner);
    }

    /// 获取统计后的 active Key 数（未禁用）
    pub fn active_count(&self) -> usize {
        self.inner
            .read()
            .entries
            .values()
            .filter(|e| !e.disabled)
            .count()
    }
}

impl Default for ClientKeyManager {
    fn default() -> Self {
        Self::new()
    }
}

/// serde 辅助：bool 为 false 时跳过序列化
fn is_false(b: &bool) -> bool {
    !b
}

fn default_cache_enabled() -> bool {
    true
}

/// 生成 `csk_` 前缀 + 32 位 base62 随机字符串
pub fn generate_client_key() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let body: String = (0..32)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();
    format!("{}{}", CLIENT_KEY_PREFIX, body)
}

/// 脱敏展示：保留前 8 位（含前缀）和后 4 位
pub fn mask_client_key(key: &str) -> String {
    if key.len() <= 12 {
        return key.to_string();
    }
    format!("{}...{}", &key[..8], &key[key.len() - 4..])
}

/// 默认管理器路径（相对凭据目录）
pub fn default_path_in(dir: &Path) -> PathBuf {
    dir.join("client_api_keys.json")
}

/// Arc 包装，便于注入 axum State
pub type SharedClientKeyManager = Arc<ClientKeyManager>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_verify() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None, true);
        assert!(entry.key.starts_with(CLIENT_KEY_PREFIX));
        assert_eq!(mgr.verify_and_touch(&entry.key), Some(entry.id));
        // 不带前缀的拒绝
        assert_eq!(mgr.verify_and_touch("nope"), None);
    }

    #[test]
    fn disabled_key_rejected() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None, true);
        mgr.set_disabled(entry.id, true);
        assert_eq!(mgr.verify_and_touch(&entry.key), None);
        mgr.set_disabled(entry.id, false);
        assert_eq!(mgr.verify_and_touch(&entry.key), Some(entry.id));
    }

    #[test]
    fn record_usage_accumulates() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None, true);
        mgr.record_usage(entry.id, 100, 50, 0, 0, 0.0);
        mgr.record_usage(entry.id, 200, 30, 5, 10, 1.5);
        let list = mgr.list();
        let e = list.iter().find(|x| x.id == entry.id).unwrap();
        assert_eq!(e.total_input_tokens, 300);
        assert_eq!(e.total_output_tokens, 80);
        assert_eq!(e.total_cache_creation_tokens, 5);
        assert_eq!(e.total_cache_read_tokens, 10);
    }

    #[test]
    fn cache_enabled_can_be_updated() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("test".to_string(), None, None, false);
        assert!(!mgr.cache_enabled_of(entry.id));
        assert!(mgr.update_meta(entry.id, None, None, None, Some(true), None, None));
        assert!(mgr.cache_enabled_of(entry.id));
    }

    #[test]
    fn history_cap_three_state_update() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("hc".to_string(), None, None, true);
        // 默认随全局（None）。
        assert_eq!(mgr.history_cap_of(entry.id), None);
        // 强制开。
        assert!(mgr.update_meta(entry.id, None, None, None, None, Some(Some(true)), None));
        assert_eq!(mgr.history_cap_of(entry.id), Some(true));
        // 强制关。
        assert!(mgr.update_meta(entry.id, None, None, None, None, Some(Some(false)), None));
        assert_eq!(mgr.history_cap_of(entry.id), Some(false));
        // 改回随全局。
        assert!(mgr.update_meta(entry.id, None, None, None, None, Some(None), None));
        assert_eq!(mgr.history_cap_of(entry.id), None);
        // 外层 None = 不动该字段。
        assert!(mgr.update_meta(entry.id, None, None, None, None, None, None));
        assert_eq!(mgr.history_cap_of(entry.id), None);
    }

    #[test]
    fn fast_mode_three_state_update() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create("fm".to_string(), None, None, true);
        // 默认随全局（None）。
        assert_eq!(mgr.fast_mode_of(entry.id), None);
        // 强制开（仅动 fast_mode，history_cap 不受影响）。
        assert!(mgr.update_meta(entry.id, None, None, None, None, None, Some(Some(true))));
        assert_eq!(mgr.fast_mode_of(entry.id), Some(true));
        assert_eq!(mgr.history_cap_of(entry.id), None);
        // 强制关。
        assert!(mgr.update_meta(entry.id, None, None, None, None, None, Some(Some(false))));
        assert_eq!(mgr.fast_mode_of(entry.id), Some(false));
        // 改回随全局。
        assert!(mgr.update_meta(entry.id, None, None, None, None, None, Some(None)));
        assert_eq!(mgr.fast_mode_of(entry.id), None);
        // 外层 None 不动。
        assert!(mgr.update_meta(entry.id, None, None, None, None, None, None));
        assert_eq!(mgr.fast_mode_of(entry.id), None);
    }

    #[test]
    fn mask_format() {
        assert_eq!(mask_client_key("csk_abcdefghijklmnop"), "csk_abcd...mnop");
        assert_eq!(mask_client_key("short"), "short");
    }

    #[test]
    fn rotate_replaces_key_but_keeps_metadata_and_stats() {
        let mgr = ClientKeyManager::new();
        let entry = mgr.create(
            "kb".to_string(),
            Some("desc".into()),
            Some("groupA".into()),
            true,
        );
        // 累计一些统计
        mgr.record_usage(entry.id, 100, 50, 5, 10, 1.5);
        let old_key = entry.key.clone();
        let rotated = mgr.rotate(entry.id).expect("rotate should succeed");
        // 新 Key 与旧 Key 不同、且仍带前缀
        assert_ne!(rotated.key, old_key);
        assert!(rotated.key.starts_with(CLIENT_KEY_PREFIX));
        // 元数据保留
        assert_eq!(rotated.id, entry.id);
        assert_eq!(rotated.name, "kb");
        assert_eq!(rotated.description.as_deref(), Some("desc"));
        assert_eq!(rotated.group.as_deref(), Some("groupA"));
        // 统计保留
        assert_eq!(rotated.total_input_tokens, 100);
        assert_eq!(rotated.total_output_tokens, 50);
        // 旧 Key 立即失效
        assert_eq!(mgr.verify_and_touch(&old_key), None);
        // 新 Key 命中
        assert_eq!(mgr.verify_and_touch(&rotated.key), Some(entry.id));
    }

    #[test]
    fn rotate_unknown_id_returns_none() {
        let mgr = ClientKeyManager::new();
        assert!(mgr.rotate(999).is_none());
    }

    #[test]
    fn ensure_system_key_uses_id_zero() {
        let mgr = ClientKeyManager::new();
        mgr.ensure_system_key("默认密钥".into(), None, "sk-kiro-abc".into());
        // 系统密钥固定在 id=0，对齐历史 keyId=0 用量桶
        assert!(mgr.is_system(0));
        assert_eq!(mgr.list().first().map(|k| k.id), Some(0));
        // 幂等：再次调用不重复创建
        mgr.ensure_system_key("默认密钥".into(), None, "sk-kiro-abc".into());
        assert_eq!(mgr.list().iter().filter(|k| k.is_system).count(), 1);
    }

    #[test]
    fn ensure_system_key_migrates_misplaced_id_to_zero() {
        // 模拟旧版 bootstrap 把 apiKey 误建在 id=1 上的场景
        let mgr = ClientKeyManager::new();
        mgr.create_with_key("默认密钥".into(), None, None, "sk-kiro-abc".into());
        assert_eq!(mgr.list().first().map(|k| k.id), Some(1));
        // 修复后启动：应迁移到 id=0
        mgr.ensure_system_key("默认密钥".into(), None, "sk-kiro-abc".into());
        assert!(mgr.is_system(0));
        assert!(
            !mgr.list()
                .iter()
                .any(|k| k.id == 1 && k.key == "sk-kiro-abc")
        );
    }

    #[test]
    fn system_key_cannot_be_deleted() {
        let mgr = ClientKeyManager::new();
        mgr.ensure_system_key("默认密钥".into(), None, "sk-kiro-abc".into());
        assert!(!mgr.delete(0), "系统密钥 id=0 不可删除");
        assert!(mgr.is_system(0));
    }
}
