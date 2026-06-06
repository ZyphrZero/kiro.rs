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
    /// 累计 credit 计费量（meteringEvent.usage 累加）
    #[serde(default)]
    pub total_credits: f64,
    /// 绑定的账号分组名（可选）
    ///
    /// 设置后，用该 Key 发起的请求只会调度到 groups 包含此分组名的上游账号（严格隔离）。
    /// None 表示不绑定分组，可使用全部账号（与 master apiKey 行为一致）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
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
    pub fn create(&self, name: String, description: Option<String>, group: Option<String>) -> ClientKey {
        let key = generate_client_key();
        let mut inner = self.inner.write();
        let id = inner.next_id;
        inner.next_id += 1;
        let entry = ClientKey {
            id,
            key: key.clone(),
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
            total_credits: 0.0,
            group: group.filter(|g| !g.trim().is_empty()),
        };
        inner.by_key.insert(key, id);
        inner.entries.insert(id, entry.clone());
        self.save_locked(&inner);
        entry
    }

    pub fn delete(&self, id: u64) -> bool {
        let mut inner = self.inner.write();
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
        self.inner.read().entries.values().filter(|e| !e.disabled).count()
    }
}

impl Default for ClientKeyManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 生成 `csk_` 前缀 + 32 位 base62 随机字符串
pub fn generate_client_key() -> String {
    const CHARSET: &[u8] =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
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
        let entry = mgr.create("test".to_string(), None, None);
        assert!(entry.key.starts_with(CLIENT_KEY_PREFIX));
        assert_eq!(mgr.verify_and_touch(&entry.key), Some(entry.id));
        // 不带前缀的拒绝
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
    fn mask_format() {
        assert_eq!(mask_client_key("csk_abcdefghijklmnop"), "csk_abcd...mnop");
        assert_eq!(mask_client_key("short"), "short");
    }
}
