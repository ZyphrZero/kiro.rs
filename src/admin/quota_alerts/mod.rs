mod delivery;
mod model;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, watch};

use crate::kiro::token_manager::MultiTokenManager;

use super::{AdminState, error::AdminServiceError};
use delivery::FeishuChannel;
use model::{AccountUsage, PoolOverview};
pub use model::{AlertSettings, BalanceRound, BalanceSample};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PoolState {
    covered: BTreeSet<u8>,
    last_sent_at: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryResult {
    pub at: i64,
    pub success: bool,
    pub target: String,
    pub message: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistentState {
    settings: AlertSettings,
    account_sent: BTreeMap<String, u8>,
    pool: PoolState,
    last_delivery: Option<DeliveryResult>,
}

struct Store {
    connection: Connection,
    state: PersistentState,
}

impl Store {
    fn open(path: &Path) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(storage_error)?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(storage_error)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS quota_alert_state (id INTEGER PRIMARY KEY CHECK(id = 1), data TEXT NOT NULL);",
        ).map_err(storage_error)?;
        let data: Option<String> = connection
            .query_row(
                "SELECT data FROM quota_alert_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        let state: PersistentState = match data {
            Some(data) => serde_json::from_str(&data)
                .map_err(|_| "额度告警状态文件损坏，停止发送以避免重复通知".to_string())?,
            None => PersistentState::default(),
        };
        state.settings.validate()?;
        Ok(Self { connection, state })
    }

    fn save(&mut self, state: PersistentState) -> Result<(), String> {
        let data =
            serde_json::to_string(&state).map_err(|_| "额度告警状态序列化失败".to_string())?;
        self.connection
            .execute(
                "INSERT INTO quota_alert_state (id, data) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
                params![data],
            )
            .map_err(storage_error)?;
        self.state = state;
        Ok(())
    }
}

fn storage_error(error: rusqlite::Error) -> String {
    format!("额度告警持久化失败：{error}")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateAlertsRequest {
    pub settings: AlertSettings,
    pub webhook: Option<String>,
    pub signing_secret: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertsResponse {
    pub settings: AlertSettings,
    pub webhook_configured: bool,
    pub signing_secret_configured: bool,
    pub notifier_enabled: bool,
    pub refresh_interval_secs: u64,
    pub sending: bool,
    pub last_delivery: Option<DeliveryResult>,
    pub error: Option<String>,
    pub overview: PoolOverview,
}

struct Runtime {
    store: Store,
    channel: FeishuChannel,
    revision: u64,
    error: Option<String>,
}

pub struct QuotaAlerts {
    runtime: Mutex<Runtime>,
    manager: Arc<MultiTokenManager>,
    rounds: watch::Sender<Option<Arc<BalanceRound>>>,
    client: reqwest::Client,
    send_gate: Arc<Semaphore>,
    notifier_enabled: bool,
    refresh_interval: Duration,
}

impl QuotaAlerts {
    pub fn start(
        path: &Path,
        manager: Arc<MultiTokenManager>,
        refresh_interval: Duration,
    ) -> Result<Arc<Self>, String> {
        let webhook = read_env("KIRO_RS_FEISHU_WEBHOOK")?;
        let secret = read_env("KIRO_RS_FEISHU_SECRET")?;
        let notifier_enabled = match read_env("KIRO_RS_QUOTA_ALERT_NOTIFIER")?.as_str() {
            "" | "1" | "true" => true,
            "0" | "false" => false,
            _ => return Err("KIRO_RS_QUOTA_ALERT_NOTIFIER 必须是 true/false 或 1/0".into()),
        };
        Self::create(
            path,
            manager,
            refresh_interval,
            FeishuChannel { webhook, secret },
            notifier_enabled,
        )
    }

    fn create(
        path: &Path,
        manager: Arc<MultiTokenManager>,
        refresh_interval: Duration,
        channel: FeishuChannel,
        notifier_enabled: bool,
    ) -> Result<Arc<Self>, String> {
        channel.validate()?;
        let store = Store::open(path)?;
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "无法创建飞书 HTTP 客户端".to_string())?;
        let (rounds, mut receiver) = watch::channel(None);
        let alerts = Arc::new(Self {
            runtime: Mutex::new(Runtime {
                store,
                channel,
                revision: 0,
                error: None,
            }),
            manager,
            rounds,
            client,
            send_gate: Arc::new(Semaphore::new(1)),
            notifier_enabled,
            refresh_interval,
        });
        let weak = Arc::downgrade(&alerts);
        tokio::spawn(async move {
            while receiver.changed().await.is_ok() {
                let round = receiver.borrow_and_update().clone();
                let Some(alerts) = weak.upgrade() else { break };
                if let Some(round) = round {
                    if let Err(error) = alerts.process_round(&round).await {
                        tracing::warn!("额度告警处理失败：{error}");
                        alerts.runtime.lock().error = Some(error);
                    }
                }
            }
        });
        Ok(alerts)
    }

    pub fn submit(&self, round: BalanceRound) {
        self.rounds.send_replace(Some(Arc::new(round)));
    }

    fn overview(
        &self,
        round: Option<&BalanceRound>,
        settings: &AlertSettings,
        now: i64,
    ) -> PoolOverview {
        let accounts = self
            .manager
            .snapshot()
            .entries
            .into_iter()
            .filter(|entry| !entry.disabled)
            .map(|entry| {
                AccountUsage::from_sample(
                    entry.id,
                    entry
                        .email
                        .filter(|email| !email.is_empty())
                        .unwrap_or_else(|| format!("账号 #{}", entry.id))
                        .chars()
                        .filter(|character| {
                            !character.is_control() && !matches!(character, '<' | '>')
                        })
                        .take(120)
                        .collect(),
                    round.and_then(|round| round.balances.get(&entry.id)),
                    now,
                    settings.max_data_age_secs,
                )
            })
            .collect();
        PoolOverview::new(accounts, settings, round.map(|round| round.completed_at))
    }

    pub fn response(&self) -> AlertsResponse {
        let runtime = self.runtime.lock();
        let settings = runtime.store.state.settings.clone();
        let round = self.rounds.borrow().clone();
        AlertsResponse {
            overview: self.overview(round.as_deref(), &settings, chrono::Utc::now().timestamp()),
            settings,
            webhook_configured: !runtime.channel.webhook.is_empty(),
            signing_secret_configured: !runtime.channel.secret.is_empty(),
            notifier_enabled: self.notifier_enabled,
            refresh_interval_secs: self.refresh_interval.as_secs(),
            sending: self.send_gate.available_permits() == 0,
            last_delivery: runtime.store.state.last_delivery.clone(),
            error: runtime.error.clone(),
        }
    }

    pub fn update(
        &self,
        request: UpdateAlertsRequest,
    ) -> Result<AlertsResponse, AdminServiceError> {
        request
            .settings
            .validate()
            .map_err(AdminServiceError::InvalidCredential)?;
        {
            let mut runtime = self.runtime.lock();
            let mut channel = runtime.channel.clone();
            if let Some(webhook) = request.webhook {
                channel.webhook = webhook.trim().to_string();
            }
            if let Some(secret) = request.signing_secret {
                channel.secret = secret.trim().to_string();
            }
            channel
                .validate()
                .map_err(AdminServiceError::InvalidCredential)?;
            let newly_enabled = (request.settings.account.enabled
                && !runtime.store.state.settings.account.enabled)
                || (request.settings.pool.enabled && !runtime.store.state.settings.pool.enabled);
            if channel.webhook.is_empty()
                && (newly_enabled
                    || (!runtime.channel.webhook.is_empty()
                        && (request.settings.account.enabled || request.settings.pool.enabled)))
            {
                return Err(AdminServiceError::InvalidCredential(
                    "开启告警前请先配置飞书 Webhook".into(),
                ));
            }
            let mut state = runtime.store.state.clone();
            state.settings = request.settings;
            runtime
                .store
                .save(state)
                .map_err(AdminServiceError::InternalError)?;
            runtime.channel = channel;
            runtime.revision += 1;
        }
        self.rounds.send_modify(|_| {});
        Ok(self.response())
    }

    fn record_result(
        &self,
        target: String,
        result: &Result<(), String>,
        delivered: Option<&Candidate>,
    ) -> Result<(), String> {
        let mut runtime = self.runtime.lock();
        let mut state = runtime.store.state.clone();
        let now = chrono::Utc::now().timestamp();
        state.last_delivery = Some(DeliveryResult {
            at: now,
            success: result.is_ok(),
            target,
            message: match result {
                Ok(()) => "飞书已确认发送成功".into(),
                Err(error) => error.clone(),
            },
        });
        if result.is_ok() {
            if let Some(candidate) = delivered {
                candidate.record(&mut state, now);
            }
        }
        if let Err(error) = runtime.store.save(state.clone()) {
            // 已投递但落盘失败时禁止本进程继续自动发送，避免重试风暴。
            runtime.store.state = state;
            runtime.error = Some(format!(
                "{error}；停止自动发送，请修复存储后重启，最后一条可能重复投递"
            ));
            return Err(runtime.error.clone().unwrap());
        }
        Ok(())
    }

    async fn process_round(&self, round: &BalanceRound) -> Result<(), String> {
        if !self.notifier_enabled || self.refresh_interval.is_zero() {
            return Ok(());
        }
        let (settings, revision) = {
            let runtime = self.runtime.lock();
            if runtime.error.is_some() {
                return Ok(());
            }
            (runtime.store.state.settings.clone(), runtime.revision)
        };
        let now = chrono::Utc::now().timestamp();
        let overview = self.overview(Some(round), &settings, now);
        let candidates = {
            let mut runtime = self.runtime.lock();
            let mut state = runtime.store.state.clone();
            if settings.pool.enabled {
                if let Some(percentage) = overview.percentage {
                    state
                        .pool
                        .covered
                        .retain(|threshold| percentage >= f64::from(*threshold));
                    if state.pool.covered != runtime.store.state.pool.covered {
                        runtime.store.save(state.clone())?;
                    }
                }
            }
            candidates(&state, &overview, now)
        };
        for candidate in candidates {
            let permit = self
                .send_gate
                .acquire()
                .await
                .map_err(|_| "告警发送队列已关闭".to_string())?;
            if self
                .rounds
                .borrow()
                .as_ref()
                .is_some_and(|latest| latest.completed_at > round.completed_at)
            {
                return Ok(());
            }
            let channel = {
                let runtime = self.runtime.lock();
                if runtime.revision != revision || runtime.error.is_some() {
                    return Ok(());
                }
                runtime.channel.clone()
            };
            let latest = self.overview(Some(round), &settings, chrono::Utc::now().timestamp());
            let Some(text) = candidate.message(&latest) else {
                continue;
            };
            let result = delivery::send(&self.client, &channel, &text).await;
            self.record_result(candidate.target(), &result, Some(&candidate))?;
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(permit);
            if result.is_err() {
                break;
            }
        }
        Ok(())
    }

    pub fn test_send(self: &Arc<Self>) -> Result<(), AdminServiceError> {
        if !self.notifier_enabled {
            return Err(AdminServiceError::InvalidCredential(
                "当前实例未指定为通知实例".into(),
            ));
        }
        let permit = self.send_gate.clone().try_acquire_owned().map_err(|_| {
            AdminServiceError::RateLimited {
                retry_after: Some("10".into()),
            }
        })?;
        let channel = self.runtime.lock().channel.clone();
        if channel.webhook.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "尚未配置飞书 Webhook".into(),
            ));
        }
        let overview = self.response().overview;
        let alerts = Arc::clone(self);
        tokio::spawn(async move {
            let text = format!("Kiro 账号额度预警 · 测试通知\n{}", overview.summary());
            let result = delivery::send(&alerts.client, &channel, &text).await;
            if let Err(error) = alerts.record_result("测试通知".into(), &result, None) {
                tracing::warn!("{error}");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(permit);
        });
        Ok(())
    }
}

fn read_env(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Ok(value) => Ok(value.trim().to_string()),
        Err(std::env::VarError::NotPresent) => Ok(String::new()),
        Err(_) => Err(format!("{name} 不是有效的 Unicode 字符串")),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Candidate {
    Account {
        id: u64,
        cycle: String,
        threshold: u8,
    },
    Pool {
        threshold: u8,
        covered: Vec<u8>,
    },
}

impl Candidate {
    fn target(&self) -> String {
        match self {
            Self::Account { id, threshold, .. } => format!("账号 #{id} ≥{threshold}%"),
            Self::Pool { threshold, .. } => format!("账号池 ≥{threshold}%"),
        }
    }

    fn message(&self, overview: &PoolOverview) -> Option<String> {
        match self {
            Self::Account {
                id,
                cycle,
                threshold,
            } => {
                let account = overview.accounts.iter().find(|account| account.id == *id)?;
                if account.cycle_key().as_ref() != Some(cycle)
                    || !account
                        .percentage
                        .is_some_and(|value| value >= f64::from(*threshold))
                {
                    return None;
                }
                Some(overview.message(Some(account), *threshold))
            }
            Self::Pool { threshold, .. } => overview
                .percentage
                .filter(|percentage| *percentage >= f64::from(*threshold))
                .map(|_| overview.message(None, *threshold)),
        }
    }

    fn record(&self, state: &mut PersistentState, now: i64) {
        match self {
            Self::Account {
                cycle, threshold, ..
            } => {
                state
                    .account_sent
                    .entry(cycle.clone())
                    .and_modify(|value| *value = (*value).max(*threshold))
                    .or_insert(*threshold);
                state.account_sent.retain(|key, _| {
                    key.rsplit_once(':')
                        .and_then(|(_, reset)| reset.parse::<i64>().ok())
                        .is_some_and(|reset| reset > now - 90 * 86400)
                });
            }
            Self::Pool { covered, .. } => {
                state.pool.covered.extend(covered);
                state.pool.last_sent_at = Some(now);
            }
        }
    }
}

fn candidates(state: &PersistentState, overview: &PoolOverview, now: i64) -> Vec<Candidate> {
    let mut pending = Vec::new();
    if state.settings.account.enabled {
        for account in &overview.accounts {
            let (Some(percentage), Some(cycle)) = (account.percentage, account.cycle_key()) else {
                continue;
            };
            let covered = state.account_sent.get(&cycle).copied().unwrap_or(0);
            if let Some(threshold) = state
                .settings
                .account
                .thresholds
                .iter()
                .rev()
                .find(|threshold| **threshold > covered && percentage >= f64::from(**threshold))
            {
                pending.push(Candidate::Account {
                    id: account.id,
                    cycle,
                    threshold: *threshold,
                });
            }
        }
    }
    if state.settings.pool.enabled
        && state
            .pool
            .last_sent_at
            .is_none_or(|last| now.saturating_sub(last) >= state.settings.pool_cooldown_secs as i64)
    {
        if let Some(percentage) = overview.percentage {
            let reached: Vec<u8> = state
                .settings
                .pool
                .thresholds
                .iter()
                .copied()
                .filter(|threshold| percentage >= f64::from(*threshold))
                .collect();
            if let Some(threshold) = reached
                .iter()
                .rev()
                .find(|threshold| !state.pool.covered.contains(threshold))
            {
                pending.push(Candidate::Pool {
                    threshold: *threshold,
                    covered: reached,
                });
            }
        }
    }
    pending
}

pub async fn get_alerts(State(state): State<AdminState>) -> Response {
    match state.service.quota_alerts() {
        Ok(alerts) => Json(alerts.response()).into_response(),
        Err(error) => error.into_http_response(),
    }
}

pub async fn update_alerts(
    State(state): State<AdminState>,
    Json(request): Json<UpdateAlertsRequest>,
) -> Response {
    match state
        .service
        .quota_alerts()
        .and_then(|alerts| alerts.update(request))
    {
        Ok(response) => Json(response).into_response(),
        Err(error) => error.into_http_response(),
    }
}

pub async fn test_alerts(State(state): State<AdminState>) -> Response {
    match state
        .service
        .quota_alerts()
        .and_then(|alerts| alerts.test_send())
    {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"message": "测试已排队，请查看最近发送结果"})),
        )
            .into_response(),
        Err(error) => error.into_http_response(),
    }
}
