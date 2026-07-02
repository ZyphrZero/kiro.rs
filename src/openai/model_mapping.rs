//! 可配置模型映射（全局、运行时热编辑）
//!
//! 客户端请求的模型名按规则表映射到目标 Claude 模型名，再交给下游
//! [`normalize_model`](super::convert) / [`map_model`](crate::anthropic::converter::map_model)
//! 解析为 Kiro 内部 ID。参考 KAM（kiro-account-manager）的 `resolve_model_mapping` 设计，
//! 但裁剪为：**仅 Claude 目标**、规则类型 `replace`/`alias`（等价，取单一 `target_model`）、
//! 无 loadbalance。
//!
//! - 匹配：对入站模型名做**精确字符串**匹配（与 KAM 一致），命中首条 enabled 规则即返回其目标。
//! - 未命中：返回 `None`，调用方保持原模型名（passthrough）。
//! - 作用域：全局。运行时经 Admin API 改 `Vec<ModelMappingRule>`，并持久化 config.json。

use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// 单条模型映射规则。`replace` 与 `alias` 行为等价（均取 `target_model`），
/// 保留两种类型只为与 KAM 的语义/UI 对齐。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelMappingRule {
    /// 稳定标识（UI 增删用）。
    pub id: String,
    /// 展示名（如 `GPT-5.5 → Opus 4.8`）。
    #[serde(default)]
    pub name: String,
    /// 是否启用；关闭的规则在解析时跳过。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 规则类型：`"replace"` | `"alias"`（等价）。
    #[serde(default = "default_rule_type")]
    pub rule_type: String,
    /// 源模型名（客户端传入，精确匹配）。
    pub source_model: String,
    /// 目标模型名（Claude 系，dashed，如 `claude-opus-4-8`）。
    pub target_model: String,
}

fn default_true() -> bool {
    true
}

fn default_rule_type() -> String {
    "replace".to_string()
}

impl ModelMappingRule {
    /// 规则类型是否合法（仅 `replace`/`alias`）。
    pub fn is_valid_rule_type(&self) -> bool {
        matches!(self.rule_type.as_str(), "replace" | "alias")
    }
}

/// 运行时可热编辑的全局模型映射表。
pub struct ModelMappings {
    rules: RwLock<Vec<ModelMappingRule>>,
}

impl ModelMappings {
    /// 用初始规则集构造。
    pub fn new(rules: Vec<ModelMappingRule>) -> Self {
        Self {
            rules: RwLock::new(rules),
        }
    }

    /// 解析入站模型名 → 目标模型名。命中首条 enabled 且 `source_model` 精确相等的规则即返回其
    /// `target_model`；无匹配返回 `None`（调用方 passthrough）。
    pub fn resolve(&self, model: &str) -> Option<String> {
        let rules = self.rules.read();
        rules
            .iter()
            .find(|r| r.enabled && r.source_model == model)
            .map(|r| r.target_model.clone())
    }

    /// 当前全部规则（克隆，供 Admin API 读取）。
    pub fn get_all(&self) -> Vec<ModelMappingRule> {
        self.rules.read().clone()
    }

    /// 整表替换（运行时立即对后续请求生效）。
    pub fn set_all(&self, rules: Vec<ModelMappingRule>) {
        *self.rules.write() = rules;
    }
}

/// `Arc<ModelMappings>` 别名。
pub type SharedModelMappings = Arc<ModelMappings>;

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(source: &str, target: &str, enabled: bool, rt: &str) -> ModelMappingRule {
        ModelMappingRule {
            id: format!("{source}->{target}"),
            name: String::new(),
            enabled,
            rule_type: rt.to_string(),
            source_model: source.to_string(),
            target_model: target.to_string(),
        }
    }

    #[test]
    fn resolve_exact_match_returns_target() {
        let m = ModelMappings::new(vec![rule("gpt-5.5", "claude-opus-4-8", true, "replace")]);
        assert_eq!(m.resolve("gpt-5.5"), Some("claude-opus-4-8".to_string()));
    }

    #[test]
    fn resolve_no_match_returns_none() {
        let m = ModelMappings::new(vec![rule("gpt-5.5", "claude-opus-4-8", true, "replace")]);
        assert_eq!(m.resolve("gpt-4o"), None);
        // 精确匹配：前缀/子串不命中
        assert_eq!(m.resolve("gpt-5.5-pro"), None);
    }

    #[test]
    fn resolve_disabled_rule_skipped() {
        let m = ModelMappings::new(vec![rule("gpt-5.5", "claude-opus-4-8", false, "replace")]);
        assert_eq!(m.resolve("gpt-5.5"), None);
    }

    #[test]
    fn resolve_alias_equivalent_to_replace() {
        let m = ModelMappings::new(vec![rule("x", "claude-sonnet-4-6", true, "alias")]);
        assert_eq!(m.resolve("x"), Some("claude-sonnet-4-6".to_string()));
    }

    #[test]
    fn resolve_first_enabled_wins() {
        let m = ModelMappings::new(vec![
            rule("dup", "claude-opus-4-8", false, "replace"),
            rule("dup", "claude-sonnet-4-6", true, "replace"),
            rule("dup", "claude-haiku-4-5", true, "replace"),
        ]);
        // 跳过 disabled，命中第二条
        assert_eq!(m.resolve("dup"), Some("claude-sonnet-4-6".to_string()));
    }

    #[test]
    fn set_all_hot_swaps() {
        let m = ModelMappings::new(vec![]);
        assert_eq!(m.resolve("a"), None);
        m.set_all(vec![rule("a", "claude-opus-4-8", true, "replace")]);
        assert_eq!(m.resolve("a"), Some("claude-opus-4-8".to_string()));
        assert_eq!(m.get_all().len(), 1);
    }

    #[test]
    fn rule_type_validation() {
        assert!(rule("a", "b", true, "replace").is_valid_rule_type());
        assert!(rule("a", "b", true, "alias").is_valid_rule_type());
        assert!(!rule("a", "b", true, "loadbalance").is_valid_rule_type());
        assert!(!rule("a", "b", true, "").is_valid_rule_type());
    }
}
