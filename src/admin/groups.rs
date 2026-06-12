//! 账号分组管理（独立实体版）
//!
//! 此模块把"分组"从依附于凭据 / 客户端 Key 的字符串标签，提升为一等实体：
//! - 分组在 `groups.json` 中独立持久化（与 `credentials.json` 同目录）
//! - 凭据 / 客户端 Key 的 `groups`/`group` 字段引用分组**名字**（保持 schema 兼容）
//! - 增删改凭据 / Key 时，校验所引用的每个分组名都已注册（防 typo 漂移）
//! - 改名走级联：自动同步所有引用的凭据与 Key
//!
//! 设计参考 `client_keys.rs` 的 RwLock + JSON 持久化模式。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// 单个分组（持久化实体）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    /// 分组名（主键，区分大小写、不允许重名、不允许首尾空白）
    pub name: String,
    /// 备注（可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 创建时间（ISO8601）
    pub created_at: String,
}

/// 分组管理器（线程安全 + 自动持久化）
pub struct GroupManager {
    inner: RwLock<Inner>,
    path: Option<PathBuf>,
}

struct Inner {
    /// 按 name 索引；HashMap 保证 O(1) 存在性查询
    entries: std::collections::HashMap<String, Group>,
}

impl GroupManager {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                entries: std::collections::HashMap::new(),
            }),
            path: None,
        }
    }

    /// 从 `groups.json` 加载（不存在时返回空管理器）
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let list: Vec<Group> = if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            if content.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&content)?
            }
        } else {
            Vec::new()
        };

        let mut entries = std::collections::HashMap::with_capacity(list.len());
        for g in list {
            entries.insert(g.name.clone(), g);
        }

        Ok(Self {
            inner: RwLock::new(Inner { entries }),
            path: Some(path),
        })
    }

    fn save_locked(&self, inner: &Inner) {
        let path = match &self.path {
            Some(p) => p,
            None => return,
        };
        let mut list: Vec<&Group> = inner.entries.values().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        match serde_json::to_string_pretty(&list) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("写入分组文件失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化分组失败: {}", e),
        }
    }

    /// 列出所有分组（按 name 字典序）
    pub fn list(&self) -> Vec<Group> {
        let inner = self.inner.read();
        let mut list: Vec<Group> = inner.entries.values().cloned().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// 单个查询
    pub fn get(&self, name: &str) -> Option<Group> {
        self.inner.read().entries.get(name).cloned()
    }

    /// 是否存在指定分组（用于凭据 / Key 写入前校验）
    pub fn exists(&self, name: &str) -> bool {
        self.inner.read().entries.contains_key(name)
    }

    /// 校验一组名字是否全部已注册；返回未注册的名字列表（调用方据此决定是否拒绝写入）
    #[allow(dead_code)]
    pub fn missing<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let inner = self.inner.read();
        names
            .into_iter()
            .filter(|n| !inner.entries.contains_key(*n))
            .map(|s| s.to_string())
            .collect()
    }

    /// 创建分组。重名直接报错，不会静默覆盖（避免误创建丢备注）
    pub fn create(&self, name: String, description: Option<String>) -> anyhow::Result<Group> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            anyhow::bail!("分组名不能为空");
        }
        if trimmed.chars().count() > 64 {
            anyhow::bail!("分组名过长（最多 64 字符）");
        }
        let mut inner = self.inner.write();
        if inner.entries.contains_key(trimmed) {
            anyhow::bail!("分组已存在: {}", trimmed);
        }
        let group = Group {
            name: trimmed.to_string(),
            description: description.map(|d| d.trim().to_string()).filter(|d| !d.is_empty()),
            created_at: Utc::now().to_rfc3339(),
        };
        inner.entries.insert(group.name.clone(), group.clone());
        self.save_locked(&inner);
        Ok(group)
    }

    /// 更新备注（不改名字）
    pub fn update_description(
        &self,
        name: &str,
        description: Option<String>,
    ) -> anyhow::Result<Group> {
        let mut inner = self.inner.write();
        let entry = inner
            .entries
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("分组不存在: {}", name))?;
        entry.description = description.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
        let cloned = entry.clone();
        self.save_locked(&inner);
        Ok(cloned)
    }

    /// 改名。返回 `Ok(new_name)`；调用方负责级联更新凭据 / Key 中的引用。
    /// `new_name` 必须未被占用；若与 `old_name` 完全一致则视为 no-op 直接返回成功。
    pub fn rename(&self, old_name: &str, new_name: &str) -> anyhow::Result<Group> {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            anyhow::bail!("新分组名不能为空");
        }
        if trimmed.chars().count() > 64 {
            anyhow::bail!("分组名过长（最多 64 字符）");
        }

        let mut inner = self.inner.write();
        if !inner.entries.contains_key(old_name) {
            anyhow::bail!("分组不存在: {}", old_name);
        }
        if trimmed == old_name {
            return Ok(inner.entries.get(old_name).cloned().unwrap());
        }
        if inner.entries.contains_key(trimmed) {
            anyhow::bail!("目标分组名已存在: {}", trimmed);
        }
        let mut group = inner.entries.remove(old_name).unwrap();
        group.name = trimmed.to_string();
        inner.entries.insert(group.name.clone(), group.clone());
        self.save_locked(&inner);
        Ok(group)
    }

    /// 删除分组。调用方应先确认无引用（或显式接受级联清理）。
    /// 返回 `true` 表示真的删了；返回 `false` 表示原本就不存在。
    pub fn delete(&self, name: &str) -> bool {
        let mut inner = self.inner.write();
        let removed = inner.entries.remove(name).is_some();
        if removed {
            self.save_locked(&inner);
        }
        removed
    }

    /// 启动迁移：从已有名字集合（凭据 groups + Key.group 聚合）反向写入注册表。
    /// 已存在的名字保持原备注 / 创建时间不变；只补缺。返回新增数量。
    pub fn bootstrap_from_existing<I: IntoIterator<Item = String>>(&self, names: I) -> usize {
        let mut inner = self.inner.write();
        let now = Utc::now().to_rfc3339();
        let mut added = 0usize;
        for raw in names {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !inner.entries.contains_key(trimmed) {
                inner.entries.insert(
                    trimmed.to_string(),
                    Group {
                        name: trimmed.to_string(),
                        description: None,
                        created_at: now.clone(),
                    },
                );
                added += 1;
            }
        }
        if added > 0 {
            self.save_locked(&inner);
        }
        added
    }
}

impl Default for GroupManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 默认管理器路径（相对凭据目录）
pub fn default_path_in(dir: &Path) -> PathBuf {
    dir.join("groups.json")
}

/// Arc 包装，便于注入 axum State
pub type SharedGroupManager = Arc<GroupManager>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_list_sorted() {
        let mgr = GroupManager::new();
        mgr.create("zz".into(), None).unwrap();
        mgr.create("aa".into(), Some("first".into())).unwrap();
        let list = mgr.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "aa");
        assert_eq!(list[0].description.as_deref(), Some("first"));
        assert_eq!(list[1].name, "zz");
    }

    #[test]
    fn create_rejects_duplicate() {
        let mgr = GroupManager::new();
        mgr.create("dup".into(), None).unwrap();
        assert!(mgr.create("dup".into(), None).is_err());
        assert!(mgr.create("  dup  ".into(), None).is_err()); // trim 后等价
    }

    #[test]
    fn create_rejects_empty_or_too_long() {
        let mgr = GroupManager::new();
        assert!(mgr.create("".into(), None).is_err());
        assert!(mgr.create("   ".into(), None).is_err());
        assert!(mgr.create("a".repeat(65), None).is_err());
    }

    #[test]
    fn missing_reports_unregistered() {
        let mgr = GroupManager::new();
        mgr.create("known".into(), None).unwrap();
        let missing = mgr.missing(["known", "ghost", "another-ghost"].iter().copied());
        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&"ghost".to_string()));
        assert!(missing.contains(&"another-ghost".to_string()));
    }

    #[test]
    fn rename_swaps_key() {
        let mgr = GroupManager::new();
        mgr.create("old".into(), Some("note".into())).unwrap();
        let renamed = mgr.rename("old", "new").unwrap();
        assert_eq!(renamed.name, "new");
        assert_eq!(renamed.description.as_deref(), Some("note"));
        assert!(!mgr.exists("old"));
        assert!(mgr.exists("new"));
    }

    #[test]
    fn rename_to_existing_fails() {
        let mgr = GroupManager::new();
        mgr.create("a".into(), None).unwrap();
        mgr.create("b".into(), None).unwrap();
        assert!(mgr.rename("a", "b").is_err());
        // 原数据不变
        assert!(mgr.exists("a"));
        assert!(mgr.exists("b"));
    }

    #[test]
    fn rename_same_name_is_noop() {
        let mgr = GroupManager::new();
        mgr.create("x".into(), None).unwrap();
        assert!(mgr.rename("x", "x").is_ok());
        assert!(mgr.rename("x", "  x  ").is_ok());
    }

    #[test]
    fn delete_returns_correct_flag() {
        let mgr = GroupManager::new();
        mgr.create("g".into(), None).unwrap();
        assert!(mgr.delete("g"));
        assert!(!mgr.delete("g"));
        assert!(!mgr.exists("g"));
    }

    #[test]
    fn bootstrap_dedups_and_skips_existing() {
        let mgr = GroupManager::new();
        mgr.create("existing".into(), Some("kept".into())).unwrap();
        let added = mgr.bootstrap_from_existing(vec![
            "existing".into(), // 已存在 → 跳过，备注保留
            "new1".into(),
            "new1".into(), // 重复 → 第二次跳过
            "  new2  ".into(),
            "".into(), // 空 → 跳过
        ]);
        assert_eq!(added, 2); // new1 + new2
        let list = mgr.list();
        assert_eq!(list.len(), 3);
        // existing 的备注没被覆盖
        let existing = mgr.get("existing").unwrap();
        assert_eq!(existing.description.as_deref(), Some("kept"));
    }

    #[test]
    fn load_empty_file_yields_empty_manager() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("kiro_test_groups_empty_{}.json", std::process::id()));
        std::fs::write(&path, "").unwrap();
        let mgr = GroupManager::load(&path).unwrap();
        assert!(mgr.list().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_roundtrip_preserves_data() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("kiro_test_groups_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mgr = GroupManager::load(&path).unwrap();
        mgr.create("alpha".into(), Some("a-desc".into())).unwrap();
        mgr.create("beta".into(), None).unwrap();

        // 重新加载
        let mgr2 = GroupManager::load(&path).unwrap();
        let list = mgr2.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "alpha");
        assert_eq!(list[0].description.as_deref(), Some("a-desc"));
        assert_eq!(list[1].name, "beta");

        let _ = std::fs::remove_file(&path);
    }
}
