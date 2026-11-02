use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::admin::types::BalanceResponse;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AlertMode {
    pub enabled: bool,
    pub thresholds: Vec<u8>,
}

impl Default for AlertMode {
    fn default() -> Self {
        Self {
            enabled: false,
            thresholds: vec![80, 90, 100],
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AlertSettings {
    pub account: AlertMode,
    pub pool: AlertMode,
    pub pool_cooldown_secs: u64,
    pub max_data_age_secs: u64,
}

impl Default for AlertSettings {
    fn default() -> Self {
        Self {
            account: AlertMode::default(),
            pool: AlertMode::default(),
            pool_cooldown_secs: 3600,
            max_data_age_secs: 900,
        }
    }
}

impl AlertSettings {
    pub fn validate(&self) -> Result<(), String> {
        for mode in [&self.account, &self.pool] {
            if mode.thresholds.is_empty()
                || mode.thresholds.len() > 10
                || mode
                    .thresholds
                    .iter()
                    .any(|value| !(1..=100).contains(value))
                || mode.thresholds.windows(2).any(|pair| pair[0] >= pair[1])
            {
                return Err("阈值必须为 1–100 的递增整数，不能重复，最多 10 档".into());
            }
        }
        if !(60..=604_800).contains(&self.pool_cooldown_secs) {
            return Err("账号池冷却时间必须在 60–604800 秒之间".into());
        }
        if !(60..=604_800).contains(&self.max_data_age_secs) {
            return Err("数据有效期必须在 60–604800 秒之间".into());
        }
        Ok(())
    }

    pub fn thresholds(&self) -> Vec<u8> {
        let mut thresholds = self.account.thresholds.clone();
        thresholds.extend(&self.pool.thresholds);
        thresholds.sort_unstable();
        thresholds.dedup();
        thresholds
    }
}

#[derive(Clone, Default)]
pub struct BalanceRound {
    pub completed_at: i64,
    pub balances: BTreeMap<u64, BalanceSample>,
}

#[derive(Clone)]
pub struct BalanceSample {
    pub queried_at: i64,
    pub balance: Option<BalanceResponse>,
    pub valid_until: Option<f64>,
    pub invalid_quota: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DataStatus {
    Valid,
    Missing,
    Failed,
    Stale,
    Invalid,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsage {
    pub id: u64,
    pub name: String,
    pub status: DataStatus,
    pub queried_at: Option<i64>,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub percentage: Option<f64>,
    pub overage_enabled: Option<bool>,
    pub next_reset_at: Option<i64>,
}

impl AccountUsage {
    pub fn from_sample(
        id: u64,
        name: String,
        sample: Option<&BalanceSample>,
        now: i64,
        max_age: u64,
    ) -> Self {
        let mut account = Self {
            id,
            name,
            status: DataStatus::Missing,
            queried_at: sample.map(|sample| sample.queried_at),
            used: None,
            limit: None,
            percentage: None,
            overage_enabled: None,
            next_reset_at: None,
        };
        let Some(sample) = sample else { return account };
        let Some(balance) = &sample.balance else {
            account.status = if sample.invalid_quota {
                DataStatus::Invalid
            } else {
                DataStatus::Failed
            };
            return account;
        };
        if sample.queried_at > now
            || now - sample.queried_at >= max_age as i64
            || sample
                .valid_until
                .is_some_and(|expiry| !expiry.is_finite() || expiry <= now as f64)
        {
            account.status = DataStatus::Stale;
            return account;
        }
        account.overage_enabled = balance.overage_enabled;
        account.next_reset_at = balance
            .next_reset_at
            .filter(|reset| reset.is_finite() && *reset > 0.0 && *reset < 253_402_300_800.0)
            .map(|reset| reset as i64);
        if account.next_reset_at.is_some_and(|reset| reset <= now) {
            account.status = DataStatus::Stale;
            account.overage_enabled = None;
            return account;
        }
        let percentage = balance.current_usage / balance.usage_limit * 100.0;
        if !balance.current_usage.is_finite()
            || balance.current_usage < 0.0
            || !balance.usage_limit.is_finite()
            || balance.usage_limit <= 0.0
            || !percentage.is_finite()
        {
            account.status = DataStatus::Invalid;
            return account;
        }
        account.status = DataStatus::Valid;
        account.used = Some(balance.current_usage);
        account.limit = Some(balance.usage_limit);
        account.percentage = Some(percentage);
        account
    }

    pub fn cycle_key(&self) -> Option<String> {
        self.next_reset_at
            .map(|reset| format!("{}:{reset}", self.id))
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolOverview {
    pub completed_at: Option<i64>,
    pub enabled_accounts: usize,
    pub valid_accounts: usize,
    pub overage_enabled: usize,
    pub overage_unknown: usize,
    pub missing_accounts: usize,
    pub failed_accounts: usize,
    pub stale_accounts: usize,
    pub invalid_accounts: usize,
    pub unknown_cycle_accounts: usize,
    pub complete: bool,
    pub used: f64,
    pub limit: f64,
    pub percentage: Option<f64>,
    pub threshold_counts: BTreeMap<u8, usize>,
    pub accounts: Vec<AccountUsage>,
}

impl PoolOverview {
    pub fn new(
        accounts: Vec<AccountUsage>,
        settings: &AlertSettings,
        completed_at: Option<i64>,
    ) -> Self {
        let count = |status| {
            accounts
                .iter()
                .filter(|account| account.status == status)
                .count()
        };
        let valid_accounts = count(DataStatus::Valid);
        let used: f64 = accounts.iter().filter_map(|account| account.used).sum();
        let limit: f64 = accounts.iter().filter_map(|account| account.limit).sum();
        let complete = !accounts.is_empty()
            && accounts.len() == valid_accounts
            && used.is_finite()
            && limit.is_finite()
            && limit > 0.0
            && (used / limit * 100.0).is_finite();
        let threshold_counts = settings
            .thresholds()
            .into_iter()
            .map(|threshold| {
                let count = accounts
                    .iter()
                    .filter(|account| {
                        account
                            .percentage
                            .is_some_and(|value| value >= f64::from(threshold))
                    })
                    .count();
                (threshold, count)
            })
            .collect();
        Self {
            completed_at,
            enabled_accounts: accounts.len(),
            valid_accounts,
            overage_enabled: accounts
                .iter()
                .filter(|account| account.overage_enabled == Some(true))
                .count(),
            overage_unknown: accounts
                .iter()
                .filter(|account| account.overage_enabled.is_none())
                .count(),
            missing_accounts: count(DataStatus::Missing),
            failed_accounts: count(DataStatus::Failed),
            stale_accounts: count(DataStatus::Stale),
            invalid_accounts: count(DataStatus::Invalid),
            unknown_cycle_accounts: accounts
                .iter()
                .filter(|account| {
                    account.status == DataStatus::Valid && account.next_reset_at.is_none()
                })
                .count(),
            complete,
            used: if used.is_finite() { used } else { 0.0 },
            limit: if limit.is_finite() { limit } else { 0.0 },
            percentage: complete.then_some(used / limit * 100.0),
            threshold_counts,
            accounts,
        }
    }

    pub fn message(&self, account: Option<&AccountUsage>, threshold: u8) -> String {
        let mut text = format!("Kiro 账号额度预警 · 已达 {threshold}%\n");
        if let Some(account) = account {
            let overage = match account.overage_enabled {
                Some(true) => "已开启",
                Some(false) => "未开启",
                None => "未知",
            };
            let reset = account
                .next_reset_at
                .and_then(|timestamp| DateTime::from_timestamp(timestamp, 0))
                .map(|date| {
                    date.with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap())
                        .format("%Y-%m-%d %H:%M（UTC+8）")
                        .to_string()
                })
                .unwrap_or_else(|| "未知".into());
            text.push_str(&format!(
                "触发账号：{}（#{}）\n已用额度：{:.2} / {:.2} credits（{:.2}%）\n剩余额度：{:.2} credits\nOverage：{overage}\n下次额度重置：{reset}\n",
                account.name, account.id, account.used.unwrap_or(0.0), account.limit.unwrap_or(0.0),
                account.percentage.unwrap_or(0.0), (account.limit.unwrap_or(0.0) - account.used.unwrap_or(0.0)).max(0.0),
            ));
        } else {
            text.push_str("触发来源：账号池整体\n");
        }
        text.push_str(&self.summary());
        text
    }

    pub fn summary(&self) -> String {
        let mut text = format!(
            "\n账号池概况\n启用账号：{} 个，其中开启 overage：{} 个，状态未知：{} 个\n",
            self.enabled_accounts, self.overage_enabled, self.overage_unknown,
        );
        if let Some(percentage) = self.percentage {
            text.push_str(&format!(
                "整体使用率：{percentage:.2}%（{:.2} / {:.2} credits）\n剩余额度：{:.2} credits\n",
                self.used,
                self.limit,
                (self.limit - self.used).max(0.0)
            ));
        } else {
            text.push_str("整体使用率：不可用（数据不完整或无有效账号，暂停整体阈值判断）\n");
        }
        for (threshold, count) in &self.threshold_counts {
            text.push_str(&format!("≥{threshold}%：{count} 个\n"));
        }
        text.push_str(&format!(
            "数据缺失：{} 个；查询失败：{} 个；过期：{} 个；额度无效：{} 个；额度周期未知：{} 个\n阈值人数为有效数据的累计数量。\n统计不含超额付费额度；达到 100% 的账号若开启 overage，仍可能继续使用。",
            self.missing_accounts, self.failed_accounts, self.stale_accounts, self.invalid_accounts, self.unknown_cycle_accounts,
        ));
        text
    }
}
