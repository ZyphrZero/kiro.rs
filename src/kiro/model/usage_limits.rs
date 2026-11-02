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
    #[serde(default)]
    pub resource_type: Option<String>,
    /// 当前使用量
    #[serde(default)]
    pub current_usage: Option<i64>,

    /// 当前使用量（精确值）
    #[serde(default)]
    pub current_usage_with_precision: Option<f64>,

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
    pub usage_limit: Option<i64>,

    /// 使用限额（精确值）
    #[serde(default)]
    pub usage_limit_with_precision: Option<f64>,
}

/// 奖励额度
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bonus {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: Option<f64>,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: Option<f64>,

    /// 状态 (ACTIVE / EXPIRED)
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub expires_on: Option<f64>,
}

impl Bonus {
    /// 检查 bonus 是否处于激活状态
    pub fn is_active_at(&self, now: f64) -> bool {
        self.status
            .as_deref()
            .map(|s| s == "ACTIVE")
            .unwrap_or(false)
            && self
                .expires_on
                .is_none_or(|expiry| expiry.is_finite() && expiry > now)
    }
}

/// 免费试用信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct FreeTrialInfo {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: Option<i64>,

    /// 当前使用量（精确值）
    #[serde(default)]
    pub current_usage_with_precision: Option<f64>,

    /// 免费试用过期时间 (Unix 时间戳)
    #[serde(default)]
    pub free_trial_expiry: Option<f64>,

    /// 免费试用状态 (ACTIVE / EXPIRED)
    #[serde(default)]
    pub free_trial_status: Option<String>,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: Option<i64>,

    /// 使用限额（精确值）
    #[serde(default)]
    pub usage_limit_with_precision: Option<f64>,
}

// ============ 便捷方法实现 ============

impl FreeTrialInfo {
    /// 检查免费试用是否处于激活状态
    pub fn is_active_at(&self, now: f64) -> bool {
        self.free_trial_status
            .as_deref()
            .map(|s| s == "ACTIVE")
            .unwrap_or(false)
            && self
                .free_trial_expiry
                .is_none_or(|expiry| expiry.is_finite() && expiry > now)
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
        cfg.overage_status.as_deref().and_then(|status| {
            match status.trim().to_ascii_uppercase().as_str() {
                "ENABLED" => Some(true),
                "DISABLED" => Some(false),
                _ => None,
            }
        })
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

    /// 优先选择 CREDIT 明细；仅在旧响应只有一项且未声明类型时兼容该项。
    fn primary_breakdown(&self) -> Option<&UsageBreakdown> {
        self.usage_breakdown_list
            .iter()
            .find(|item| {
                item.resource_type
                    .as_deref()
                    .is_some_and(|resource| resource.eq_ignore_ascii_case("CREDIT"))
            })
            .or_else(|| {
                self.usage_breakdown_list.first().filter(|item| {
                    self.usage_breakdown_list.len() == 1 && item.resource_type.is_none()
                })
            })
    }

    pub fn next_reset_at(&self) -> Option<f64> {
        self.next_date_reset.or_else(|| {
            self.primary_breakdown()
                .and_then(|item| item.next_date_reset)
        })
    }

    /// 返回同一时刻的（已用、总额度）；缺失或无效的额度项不能当成零使用量。
    pub fn credit_totals_at(&self, now: f64) -> Option<(f64, f64)> {
        let breakdown = self.primary_breakdown()?;
        let mut used = breakdown
            .current_usage_with_precision
            .or(breakdown.current_usage.map(|value| value as f64))?;
        let mut limit = breakdown
            .usage_limit_with_precision
            .or(breakdown.usage_limit.map(|value| value as f64))?;
        let valid = |used: f64, limit: f64| {
            used.is_finite() && used >= 0.0 && limit.is_finite() && limit >= 0.0
        };
        if !valid(used, limit) {
            return None;
        }
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.is_active_at(now) {
                let trial_used = trial
                    .current_usage_with_precision
                    .or(trial.current_usage.map(|value| value as f64))?;
                let trial_limit = trial
                    .usage_limit_with_precision
                    .or(trial.usage_limit.map(|value| value as f64))?;
                if !valid(trial_used, trial_limit) {
                    return None;
                }
                used += trial_used;
                limit += trial_limit;
            }
        }
        for bonus in &breakdown.bonuses {
            if bonus.is_active_at(now) {
                let bonus_used = bonus.current_usage?;
                let bonus_limit = bonus.usage_limit?;
                if !valid(bonus_used, bonus_limit) {
                    return None;
                }
                used += bonus_used;
                limit += bonus_limit;
            }
        }
        valid(used, limit).then_some((used, limit))
    }

    pub fn credit_expiry_at(&self, now: f64) -> Option<f64> {
        let breakdown = self.primary_breakdown()?;
        let trial_expiry = breakdown
            .free_trial_info
            .as_ref()
            .filter(|trial| trial.is_active_at(now))
            .and_then(|trial| trial.free_trial_expiry);
        breakdown
            .bonuses
            .iter()
            .filter(|bonus| bonus.is_active_at(now))
            .filter_map(|bonus| bonus.expires_on)
            .chain(trial_expiry)
            .min_by(f64::total_cmp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_totals_include_only_active_unexpired_credits_not_overage() {
        let response: UsageLimitsResponse = serde_json::from_value(serde_json::json!({
            "usageBreakdownList": [
                {"resourceType": "OTHER", "usageLimit": 999999, "currentUsage": 0},
                {
                    "resourceType": "CREDIT", "currentUsageWithPrecision": 50.5, "usageLimitWithPrecision": 100.0,
                    "currentOveragesWithPrecision": 300.0, "overageLimit": 99999,
                    "nextDateReset": 4000,
                    "freeTrialInfo": {"freeTrialStatus": "ACTIVE", "freeTrialExpiry": 2000, "currentUsage": 20, "usageLimit": 50},
                    "bonuses": [
                        {"status": "ACTIVE", "expiresOn": 2000, "currentUsage": 10, "usageLimit": 25},
                        {"status": "ACTIVE", "expiresOn": 900, "currentUsage": 100, "usageLimit": 500},
                        {"status": "EXPIRED", "currentUsage": 100, "usageLimit": 500}
                    ]
                }
            ]
        })).unwrap();
        assert_eq!(response.credit_totals_at(1000.0), Some((80.5, 175.0)));
        assert_eq!(response.credit_totals_at(2000.0), Some((50.5, 100.0)));
        assert_eq!(response.next_reset_at(), Some(4000.0));
        assert_eq!(response.credit_expiry_at(1000.0), Some(2000.0));
        assert_eq!(response.credit_expiry_at(2000.0), None);
    }

    #[test]
    fn credit_totals_validate_missing_and_negative_components_and_preserve_zero_precision() {
        for breakdown in [
            serde_json::json!({"usageLimit": 100}),
            serde_json::json!({"currentUsage": 20, "usageLimit": -1}),
            serde_json::json!({"currentUsage": 20, "usageLimit": 100, "bonuses": [{"status": "ACTIVE", "currentUsage": -10, "usageLimit": 100}]}),
        ] {
            let response: UsageLimitsResponse =
                serde_json::from_value(serde_json::json!({"usageBreakdownList": [breakdown]}))
                    .unwrap();
            assert_eq!(response.credit_totals_at(1000.0), None);
        }
        let response: UsageLimitsResponse =
            serde_json::from_value(serde_json::json!({"usageBreakdownList": [{
                "currentUsage": 10, "usageLimit": 100, "currentUsageWithPrecision": 0.0
            }]}))
            .unwrap();
        assert_eq!(response.credit_totals_at(1000.0), Some((0.0, 100.0)));
    }

    #[test]
    fn unknown_overage_status_stays_unknown() {
        let response: UsageLimitsResponse = serde_json::from_value(serde_json::json!({
            "overageConfiguration": {"overageStatus": "NEW_UNKNOWN_VALUE"}
        }))
        .unwrap();
        assert_eq!(response.overage_enabled(), None);
    }

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
