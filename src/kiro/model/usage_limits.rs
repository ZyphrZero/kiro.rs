//! 使用额度查询数据模型
//!
//! 包含 getUsageLimits API 的响应类型定义

use serde::Deserialize;

/// 使用额度查询响应
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimitsResponse {
    /// 下次重置日期 (Unix 时间戳)
    #[serde(default)]
    pub next_date_reset: Option<f64>,

    /// 订阅信息
    #[serde(default)]
    pub subscription_info: Option<SubscriptionInfo>,

    /// 使用量明细列表
    #[serde(default)]
    pub usage_breakdown_list: Vec<UsageBreakdown>,

    /// 超额配置（用户当前是否开启了超额；可能不存在）
    #[serde(default)]
    pub overage_configuration: Option<OverageConfiguration>,

    /// 用户信息（请求带 isEmailRequired=true 时上游返回）
    #[serde(default)]
    pub user_info: Option<UserInfo>,
}

/// 用户信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInfo {
    /// 账号邮箱
    #[serde(default)]
    pub email: Option<String>,
}

/// 订阅信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionInfo {
    /// 订阅标题 (KIRO PRO+ / KIRO FREE 等)
    #[serde(default)]
    pub subscription_title: Option<String>,

    /// 是否可以开启超额（"ENABLED" / "DISABLED" / "NOT_AVAILABLE" 等）
    /// 这表示账号"能否"开启超额，FREE 等订阅通常返回 NOT_AVAILABLE
    #[serde(default)]
    pub overage_capability: Option<String>,
}

/// 超额配置
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverageConfiguration {
    /// 用户当前是否开启了超额（兼容字段）
    #[serde(default)]
    pub overage_enabled: Option<bool>,

    /// 用户当前的超额状态字符串（"ENABLED" / "DISABLED"）
    #[serde(default)]
    pub overage_status: Option<String>,
}

/// 使用量明细
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct UsageBreakdown {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: i64,

    /// 当前使用量（精确值）
    #[serde(default)]
    pub current_usage_with_precision: f64,

    /// 奖励额度列表
    #[serde(default)]
    pub bonuses: Vec<Bonus>,

    /// 免费试用信息
    #[serde(default)]
    pub free_trial_info: Option<FreeTrialInfo>,

    /// 下次重置日期 (Unix 时间戳)
    #[serde(default)]
    pub next_date_reset: Option<f64>,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: i64,

    /// 使用限额（精确值）
    #[serde(default)]
    pub usage_limit_with_precision: f64,
}

/// 奖励额度
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bonus {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: f64,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: f64,

    /// 状态 (ACTIVE / EXPIRED)
    #[serde(default)]
    pub status: Option<String>,
}

impl Bonus {
    /// 检查 bonus 是否处于激活状态
    pub fn is_active(&self) -> bool {
        self.status
            .as_deref()
            .map(|s| s == "ACTIVE")
            .unwrap_or(false)
    }
}

/// 免费试用信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct FreeTrialInfo {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: i64,

    /// 当前使用量（精确值）
    #[serde(default)]
    pub current_usage_with_precision: f64,

    /// 免费试用过期时间 (Unix 时间戳)
    #[serde(default)]
    pub free_trial_expiry: Option<f64>,

    /// 免费试用状态 (ACTIVE / EXPIRED)
    #[serde(default)]
    pub free_trial_status: Option<String>,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: i64,

    /// 使用限额（精确值）
    #[serde(default)]
    pub usage_limit_with_precision: f64,
}

// ============ 便捷方法实现 ============

impl FreeTrialInfo {
    /// 检查免费试用是否处于激活状态
    pub fn is_active(&self) -> bool {
        self.free_trial_status
            .as_deref()
            .map(|s| s == "ACTIVE")
            .unwrap_or(false)
    }
}

impl UsageLimitsResponse {
    /// 获取订阅标题
    pub fn subscription_title(&self) -> Option<&str> {
        self.subscription_info
            .as_ref()
            .and_then(|info| info.subscription_title.as_deref())
    }

    /// 获取账号邮箱（上游 userInfo.email，可能为空）
    pub fn email(&self) -> Option<&str> {
        self.user_info
            .as_ref()
            .and_then(|info| info.email.as_deref())
            .filter(|s| !s.is_empty())
    }

    /// 用户当前是否开启了超额（兼容 overageEnabled / overageStatus）
    pub fn overage_enabled(&self) -> Option<bool> {
        let cfg = self.overage_configuration.as_ref()?;
        if let Some(enabled) = cfg.overage_enabled {
            return Some(enabled);
        }
        cfg.overage_status
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("ENABLED"))
    }

    /// 账号是否"能"开启超额（基于 subscriptionInfo.overageCapability）
    /// `Some(true)` = 可开启 (OVERAGE_CAPABLE)；`Some(false)` = 此订阅明确不支持
    /// (NOT_OVERAGE_CAPABLE / NOT_AVAILABLE)；`None` = 上游未给字段或值未识别
    pub fn overage_capable(&self) -> Option<bool> {
        let cap = self
            .subscription_info
            .as_ref()
            .and_then(|s| s.overage_capability.as_deref())?;
        let normalized = cap.trim().to_uppercase();
        if normalized == "OVERAGE_CAPABLE" {
            return Some(true);
        }
        if normalized == "NOT_OVERAGE_CAPABLE" || normalized == "NOT_AVAILABLE" {
            return Some(false);
        }
        // 未识别的取值不要硬性判定为"不支持"，返回 None 让前端显示"未知"
        None
    }

    /// 获取第一个使用量明细
    fn primary_breakdown(&self) -> Option<&UsageBreakdown> {
        self.usage_breakdown_list.first()
    }

    /// 获取总使用限额（精确值）
    ///
    /// 累加基础额度、激活的免费试用额度和激活的奖励额度
    pub fn usage_limit(&self) -> f64 {
        let Some(breakdown) = self.primary_breakdown() else {
            return 0.0;
        };

        let mut total = breakdown.usage_limit_with_precision;

        // 累加激活的 free trial 额度
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.is_active() {
                total += trial.usage_limit_with_precision;
            }
        }

        // 累加激活的 bonus 额度
        for bonus in &breakdown.bonuses {
            if bonus.is_active() {
                total += bonus.usage_limit;
            }
        }

        total
    }

    /// 获取总当前使用量（精确值）
    ///
    /// 累加基础使用量、激活的免费试用使用量和激活的奖励使用量
    pub fn current_usage(&self) -> f64 {
        let Some(breakdown) = self.primary_breakdown() else {
            return 0.0;
        };

        let mut total = breakdown.current_usage_with_precision;

        // 累加激活的 free trial 使用量
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.is_active() {
                total += trial.current_usage_with_precision;
            }
        }

        // 累加激活的 bonus 使用量
        for bonus in &breakdown.bonuses {
            if bonus.is_active() {
                total += bonus.current_usage;
            }
        }

        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_email_from_user_info() {
        let json = r#"{
            "subscriptionInfo": { "subscriptionTitle": "KIRO PRO+" },
            "userInfo": { "email": "alice@example.com", "userId": "u-123" }
        }"#;
        let resp: UsageLimitsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.subscription_title(), Some("KIRO PRO+"));
        assert_eq!(resp.email(), Some("alice@example.com"));
    }

    #[test]
    fn test_email_none_when_user_info_absent() {
        let json = r#"{ "subscriptionInfo": { "subscriptionTitle": "KIRO FREE" } }"#;
        let resp: UsageLimitsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.email(), None);
    }

    #[test]
    fn test_email_none_when_empty_string() {
        // 上游可能返回空字符串邮箱，应视为无邮箱
        let json = r#"{ "userInfo": { "email": "" } }"#;
        let resp: UsageLimitsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.email(), None);
    }
}
