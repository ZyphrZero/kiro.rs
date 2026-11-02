use super::*;
use crate::{
    admin::types::BalanceResponse, kiro::model::credentials::KiroCredentials, model::config::Config,
};
use axum::{Router, routing::post};
use serde_json::{Value, json};

fn manager() -> Arc<MultiTokenManager> {
    Arc::new(
        MultiTokenManager::new(
            Config::default(),
            vec![
                KiroCredentials {
                    id: Some(1),
                    ..Default::default()
                },
                KiroCredentials {
                    id: Some(2),
                    ..Default::default()
                },
                KiroCredentials {
                    id: Some(3),
                    disabled: true,
                    ..Default::default()
                },
            ],
            None,
            None,
            false,
        )
        .unwrap(),
    )
}

fn sample(id: u64, used: f64, limit: f64, now: i64) -> BalanceSample {
    BalanceSample {
        queried_at: now,
        valid_until: None,
        invalid_quota: false,
        balance: Some(BalanceResponse {
            id,
            current_usage: used,
            usage_limit: limit,
            remaining: limit - used,
            subscription_title: None,
            usage_percentage: used / limit * 100.0,
            next_reset_at: Some((now + 86400) as f64),
            overage_enabled: Some(true),
            overage_capable: Some(true),
            overage_capability_raw: None,
        }),
    }
}

fn round(now: i64) -> BalanceRound {
    BalanceRound {
        completed_at: now,
        balances: BTreeMap::from([
            (1, sample(1, 92.0, 100.0, now)),
            (2, sample(2, 500.0, 1000.0, now)),
            (3, sample(3, 10000.0, 10000.0, now)),
        ]),
    }
}

fn settings() -> AlertSettings {
    let mut settings = AlertSettings::default();
    settings.account.enabled = true;
    settings.pool.enabled = true;
    settings
}

fn create(path: &Path) -> Arc<QuotaAlerts> {
    QuotaAlerts::create(
        path,
        manager(),
        Duration::from_secs(300),
        FeishuChannel::default(),
        true,
    )
    .unwrap()
}

async fn webhook(response: Value) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let received = Arc::new(Mutex::new(Vec::new()));
    let captured = received.clone();
    let router = Router::new().route(
        "/hook",
        post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            let response = response.clone();
            async move {
                captured.lock().push(body);
                Json(response)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}/hook"), received, task)
}

#[test]
fn validates_thresholds_and_timings() {
    let mut config = settings();
    assert!(config.validate().is_ok());
    for thresholds in [vec![], vec![0, 90], vec![90, 80], vec![90, 90], vec![101]] {
        config.account.thresholds = thresholds;
        assert!(config.validate().is_err());
    }
    config = settings();
    config.pool_cooldown_secs = 0;
    assert!(config.validate().is_err());
    config.pool_cooldown_secs = 60;
    config.max_data_age_secs = u64::MAX;
    assert!(config.validate().is_err());
}

#[test]
fn feishu_signature_matches_reference_hmac_empty_message() {
    let body = delivery::payload("test", "demo", 1599360473);
    assert_eq!(body["timestamp"], "1599360473");
    assert_eq!(body["sign"], "l1N0gAcBjdwBvGm1xMjOF0XSyaLRpR7tuO5dHfhAYc8=");
    let unsigned = delivery::payload("test", "", 1599360473);
    assert!(unsigned.get("sign").is_none());
    assert!(unsigned.get("timestamp").is_none());
}

#[tokio::test]
async fn network_timeout_does_not_expose_url_or_consume_thresholds() {
    let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured = received.clone();
    let router = Router::new().route(
        "/hook",
        post(move || {
            captured.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            async {
                std::future::pending::<()>().await;
                Json(json!({"code": 0}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let channel = FeishuChannel {
        webhook: format!("http://{address}/hook"),
        secret: "test-only".into(),
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        delivery::send(&client, &channel, "test"),
    )
    .await
    .unwrap();
    assert_eq!(result, Err("飞书请求超时".into()));
    assert_eq!(received.load(std::sync::atomic::Ordering::Relaxed), 3);
    let alerts = create(Path::new(":memory:"));
    let candidate = Candidate::Account {
        id: 1,
        cycle: "1:2000000000".into(),
        threshold: 90,
    };
    alerts
        .record_result(candidate.target(), &result, Some(&candidate))
        .unwrap();
    assert!(alerts.runtime.lock().store.state.account_sent.is_empty());
    task.abort();
}

#[tokio::test]
async fn no_notifications_when_refresher_or_notifier_is_disabled() {
    for (interval, enabled) in [(Duration::ZERO, true), (Duration::from_secs(300), false)] {
        let alerts = QuotaAlerts::create(
            Path::new(":memory:"),
            manager(),
            interval,
            FeishuChannel::default(),
            enabled,
        )
        .unwrap();
        alerts.runtime.lock().store.state.settings = settings();
        alerts
            .process_round(&round(chrono::Utc::now().timestamp()))
            .await
            .unwrap();
        assert!(alerts.response().last_delivery.is_none());
    }
}

#[tokio::test]
async fn weighted_pool_excludes_disabled_in_every_counter() {
    let alerts = create(Path::new(":memory:"));
    let now = chrono::Utc::now().timestamp();
    let overview = alerts.overview(Some(&round(now)), &settings(), now);
    assert_eq!(overview.enabled_accounts, 2);
    assert_eq!(overview.overage_enabled, 2);
    assert_eq!(overview.used, 592.0);
    assert_eq!(overview.limit, 1100.0);
    assert!((overview.percentage.unwrap() - 592.0 / 1100.0 * 100.0).abs() < 1e-8);
    assert_eq!(
        overview.threshold_counts,
        BTreeMap::from([(80, 1), (90, 1), (100, 0)])
    );
    assert!(overview.complete);
    let mut data = round(now);
    data.balances.insert(2, sample(2, 1100.0, 1000.0, now));
    let overview = alerts.overview(Some(&data), &settings(), now);
    assert_eq!(
        overview.threshold_counts,
        BTreeMap::from([(80, 2), (90, 2), (100, 1)])
    );
    assert!(
        overview
            .message(Some(&overview.accounts[0]), 90)
            .contains("统计不含超额付费额度")
    );
}

#[tokio::test]
async fn anomalous_balances_do_not_become_low_usage_or_a_complete_pool() {
    let alerts = create(Path::new(":memory:"));
    let now = chrono::Utc::now().timestamp();
    let mut data = round(now);
    data.balances.get_mut(&2).unwrap().balance = None;
    let overview = alerts.overview(Some(&data), &settings(), now);
    assert_eq!(overview.failed_accounts, 1);
    assert_eq!(overview.overage_unknown, 1);
    data.balances.get_mut(&2).unwrap().invalid_quota = true;
    assert_eq!(
        alerts
            .overview(Some(&data), &settings(), now)
            .invalid_accounts,
        1
    );
    data.balances.get_mut(&2).unwrap().invalid_quota = false;
    assert!(!overview.complete);
    assert_eq!(overview.percentage, None);
    assert!(
        candidates(
            &PersistentState {
                settings: settings(),
                ..Default::default()
            },
            &overview,
            now
        )
        .iter()
        .all(|candidate| matches!(candidate, Candidate::Account { .. }))
    );
    data.balances.get_mut(&1).unwrap().queried_at = now - 900;
    assert_eq!(
        alerts
            .overview(Some(&data), &settings(), now)
            .stale_accounts,
        1
    );
    data.balances.remove(&2);
    assert_eq!(
        alerts
            .overview(Some(&data), &settings(), now)
            .missing_accounts,
        1
    );
    for (used, limit) in [
        (0.0, 0.0),
        (-1.0, 100.0),
        (f64::INFINITY, 100.0),
        (20.0, f64::NAN),
    ] {
        data.balances.insert(2, sample(2, used, limit, now));
        let overview = alerts.overview(Some(&data), &settings(), now);
        assert_eq!(overview.invalid_accounts, 1);
        assert_eq!(overview.percentage, None);
    }
}

#[tokio::test]
async fn cycle_expiry_and_missing_cycle_are_not_fabricated() {
    let alerts = create(Path::new(":memory:"));
    let now = chrono::Utc::now().timestamp();
    let mut data = round(now);
    data.balances.get_mut(&1).unwrap().valid_until = Some(now as f64);
    assert_eq!(
        alerts
            .overview(Some(&data), &settings(), now)
            .stale_accounts,
        1
    );
    data.balances.get_mut(&1).unwrap().valid_until = None;
    data.balances
        .get_mut(&1)
        .unwrap()
        .balance
        .as_mut()
        .unwrap()
        .next_reset_at = Some(now as f64);
    assert_eq!(
        alerts
            .overview(Some(&data), &settings(), now)
            .stale_accounts,
        1
    );
    data.balances
        .get_mut(&1)
        .unwrap()
        .balance
        .as_mut()
        .unwrap()
        .next_reset_at = None;
    let overview = alerts.overview(Some(&data), &settings(), now);
    assert_eq!(overview.unknown_cycle_accounts, 1);
    assert!(overview.complete);
    assert!(
        candidates(
            &PersistentState {
                settings: settings(),
                ..Default::default()
            },
            &overview,
            now
        )
        .is_empty()
    );
    assert_eq!(alerts.overview(None, &settings(), now).percentage, None);
}

#[tokio::test]
async fn account_highest_threshold_persists_across_restart_and_new_cycle_rearms() {
    let path = std::env::temp_dir().join(format!("kiro-quota-test-{}.db", uuid::Uuid::new_v4()));
    let now = chrono::Utc::now().timestamp();
    {
        let alerts = create(&path);
        let overview = alerts.overview(Some(&round(now)), &settings(), now);
        let mut state = PersistentState {
            settings: settings(),
            ..Default::default()
        };
        let pending = candidates(&state, &overview, now);
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending[0],
            Candidate::Account { threshold: 90, .. }
        ));
        pending[0].record(&mut state, now);
        alerts.runtime.lock().store.save(state).unwrap();
    }
    {
        let alerts = create(&path);
        let state = alerts.runtime.lock().store.state.clone();
        let overview = alerts.overview(Some(&round(now)), &settings(), now);
        assert!(candidates(&state, &overview, now).is_empty());
        let mut data = round(now);
        data.balances
            .get_mut(&1)
            .unwrap()
            .balance
            .as_mut()
            .unwrap()
            .next_reset_at = Some((now + 2 * 86400) as f64);
        let overview = alerts.overview(Some(&data), &settings(), now);
        assert_eq!(candidates(&state, &overview, now).len(), 1);
    }
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn pool_crossings_cooldown_and_incomplete_rounds() {
    let alerts = create(Path::new(":memory:"));
    let now = chrono::Utc::now().timestamp();
    let mut data = round(now);
    data.balances.insert(2, sample(2, 950.0, 1000.0, now));
    let overview = alerts.overview(Some(&data), &settings(), now);
    let mut state = PersistentState {
        settings: settings(),
        ..Default::default()
    };
    state.settings.account.enabled = false;
    let pending = candidates(&state, &overview, now);
    assert_eq!(
        pending,
        vec![Candidate::Pool {
            threshold: 90,
            covered: vec![80, 90]
        }]
    );
    pending[0].record(&mut state, now);
    assert!(candidates(&state, &overview, now + 4000).is_empty());
    alerts.runtime.lock().store.save(state.clone()).unwrap();
    let mut low = round(now);
    low.balances.get_mut(&1).unwrap().balance = None;
    alerts.process_round(&low).await.unwrap();
    assert_eq!(
        alerts.runtime.lock().store.state.pool.covered,
        BTreeSet::from([80, 90])
    );
    low.balances.insert(1, sample(1, 0.0, 100.0, now));
    alerts.process_round(&low).await.unwrap();
    state = alerts.runtime.lock().store.state.clone();
    assert!(state.pool.covered.is_empty());
    assert!(candidates(&state, &overview, now + 3599).is_empty());
    assert_eq!(candidates(&state, &overview, now + 3600).len(), 1);
}

#[test]
fn webhook_validation_blocks_untrusted_targets_and_secrets_in_urls() {
    for webhook in [
        "http://open.feishu.cn/open-apis/bot/v2/hook/test",
        "https://localhost/open-apis/bot/v2/hook/test",
        "https://open.feishu.cn.evil.example/open-apis/bot/v2/hook/test",
        "https://user@open.feishu.cn/open-apis/bot/v2/hook/test",
        "https://open.feishu.cn/open-apis/bot/v2/hook/test?secret=foo",
        "https://open.feishu.cn/open-apis/bot/v2/hook/test#fragment",
        "https://open.feishu.cn/open-apis/bot/v2/hook/../admin",
    ] {
        assert!(
            FeishuChannel {
                webhook: webhook.into(),
                secret: String::new()
            }
            .validate()
            .is_err()
        );
    }
    assert!(
        FeishuChannel {
            webhook: "https://open.feishu.cn/open-apis/bot/v2/hook/test-only".into(),
            secret: String::new()
        }
        .validate()
        .is_ok()
    );
}

#[tokio::test]
async fn real_http_delivery_checks_business_status_and_records_only_success() {
    let alerts = create(Path::new(":memory:"));
    let now = chrono::Utc::now().timestamp();
    let (url, received, task) = webhook(json!({"code": 19021, "msg": "private-response"})).await;
    {
        let mut runtime = alerts.runtime.lock();
        runtime.channel.webhook = url;
        runtime.store.state.settings = settings();
    }
    alerts.process_round(&round(now)).await.unwrap();
    assert_eq!(received.lock().len(), 1);
    assert!(alerts.runtime.lock().store.state.account_sent.is_empty());
    let result = alerts.response().last_delivery.unwrap();
    assert!(!result.success);
    assert!(result.message.contains("19021"));
    assert!(!result.message.contains("private-response"));
    task.abort();
    let (url, received, task) = webhook(json!({"code": 0})).await;
    alerts.runtime.lock().channel.webhook = url;
    alerts.process_round(&round(now)).await.unwrap();
    alerts.process_round(&round(now)).await.unwrap();
    assert_eq!(received.lock().len(), 1);
    assert!(alerts.response().last_delivery.unwrap().success);
    assert_eq!(alerts.runtime.lock().store.state.account_sent.len(), 1);
    let text = received.lock()[0]["content"]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.contains("已达 90%"));
    assert!(text.contains("启用账号：2 个"));
    task.abort();
}

#[tokio::test]
async fn retries_are_bounded_and_invalid_success_responses_are_rejected() {
    let (url, received, task) = webhook(json!({"code": 9499})).await;
    let channel = FeishuChannel {
        webhook: url,
        secret: String::new(),
    };
    assert!(
        delivery::send(&reqwest::Client::new(), &channel, "test")
            .await
            .is_err()
    );
    assert_eq!(received.lock().len(), 3);
    task.abort();
    let (url, received, task) = webhook(json!({"msg": "success"})).await;
    let channel = FeishuChannel {
        webhook: url,
        secret: String::new(),
    };
    assert!(
        delivery::send(&reqwest::Client::new(), &channel, "test")
            .await
            .is_err()
    );
    assert_eq!(received.lock().len(), 1);
    task.abort();
}

#[tokio::test]
async fn settings_and_api_never_persist_or_return_channel_secrets() {
    let alerts = create(Path::new(":memory:"));
    let response = alerts
        .update(UpdateAlertsRequest {
            settings: settings(),
            webhook: Some("https://open.feishu.cn/open-apis/bot/v2/hook/test-only".into()),
            signing_secret: Some("test-only-signature-secret".into()),
        })
        .unwrap();
    assert!(response.webhook_configured);
    assert!(response.signing_secret_configured);
    let response = serde_json::to_string(&response).unwrap();
    let state = serde_json::to_string(&alerts.runtime.lock().store.state).unwrap();
    assert!(!response.contains("test-only"));
    assert!(!state.contains("test-only"));
    assert!(
        alerts
            .update(UpdateAlertsRequest {
                settings: settings(),
                webhook: Some(String::new()),
                signing_secret: None
            })
            .is_err()
    );
}

#[tokio::test]
async fn disabled_after_refresh_is_excluded_and_test_does_not_change_dedup() {
    let alerts = create(Path::new(":memory:"));
    let now = chrono::Utc::now().timestamp();
    let data = round(now);
    alerts.manager.set_disabled(1, true).unwrap();
    let overview = alerts.overview(Some(&data), &settings(), now);
    assert_eq!(overview.enabled_accounts, 1);
    assert_eq!(overview.overage_enabled, 1);
    assert_eq!(overview.threshold_counts[&80], 0);
    let (url, received, task) = webhook(json!({"code": 0})).await;
    alerts.runtime.lock().channel.webhook = url;
    alerts.test_send().unwrap();
    assert!(alerts.test_send().is_err());
    tokio::time::timeout(Duration::from_secs(5), async {
        while alerts.response().last_delivery.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(received.lock().len(), 1);
    assert!(alerts.runtime.lock().store.state.account_sent.is_empty());
    task.abort();
}

#[test]
fn corrupt_state_is_not_silently_reset() {
    let path = std::env::temp_dir().join(format!("kiro-quota-corrupt-{}.db", uuid::Uuid::new_v4()));
    {
        let store = Store::open(&path).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO quota_alert_state VALUES (1, 'invalid json')",
                [],
            )
            .unwrap();
    }
    assert!(Store::open(&path).is_err());
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn failed_persistence_stops_automatic_delivery_and_keeps_memory_dedup() {
    let alerts = create(Path::new(":memory:"));
    let (url, received, task) = webhook(json!({"code": 0})).await;
    {
        let mut runtime = alerts.runtime.lock();
        runtime.channel.webhook = url;
        runtime.store.state.settings = settings();
    }
    alerts
        .runtime
        .lock()
        .store
        .connection
        .execute_batch("PRAGMA query_only = ON;")
        .unwrap();
    let now = chrono::Utc::now().timestamp();
    assert!(alerts.process_round(&round(now)).await.is_err());
    assert!(alerts.response().error.is_some());
    assert_eq!(alerts.runtime.lock().store.state.account_sent.len(), 1);
    alerts
        .process_round(&round(chrono::Utc::now().timestamp()))
        .await
        .unwrap();
    assert_eq!(received.lock().len(), 1);
    task.abort();
}

#[tokio::test]
async fn configuration_write_failure_does_not_change_live_settings_or_channel() {
    let alerts = create(Path::new(":memory:"));
    alerts
        .runtime
        .lock()
        .store
        .connection
        .execute_batch("PRAGMA query_only = ON;")
        .unwrap();
    let mut updated = AlertSettings::default();
    updated.pool_cooldown_secs = 60;
    assert!(
        alerts
            .update(UpdateAlertsRequest {
                settings: updated,
                webhook: Some("https://open.feishu.cn/open-apis/bot/v2/hook/fixture-only".into()),
                signing_secret: None,
            })
            .is_err()
    );
    assert_eq!(alerts.response().settings, AlertSettings::default());
    assert!(!alerts.response().webhook_configured);
}

#[tokio::test]
async fn missing_channel_after_restart_still_allows_disabling_each_mode() {
    let alerts = create(Path::new(":memory:"));
    alerts.runtime.lock().store.state.settings = settings();
    let mut updated = settings();
    updated.account.enabled = false;
    let response = alerts
        .update(UpdateAlertsRequest {
            settings: updated.clone(),
            webhook: None,
            signing_secret: None,
        })
        .unwrap();
    assert!(!response.settings.account.enabled);
    assert!(response.settings.pool.enabled);
    updated.pool.enabled = false;
    let response = alerts
        .update(UpdateAlertsRequest {
            settings: updated,
            webhook: None,
            signing_secret: None,
        })
        .unwrap();
    assert!(!response.settings.pool.enabled);
}

#[tokio::test]
async fn round_submission_does_not_wait_for_delivery_and_worker_deduplicates() {
    let alerts = create(Path::new(":memory:"));
    let (url, received, task) = webhook(json!({"code": 0})).await;
    {
        let mut runtime = alerts.runtime.lock();
        runtime.channel.webhook = url;
        runtime.store.state.settings = settings();
    }
    let permit = alerts.send_gate.acquire().await.unwrap();
    let now = chrono::Utc::now().timestamp();
    alerts.submit(round(now));
    assert!(received.lock().is_empty());
    assert!(alerts.response().overview.complete);
    drop(permit);
    tokio::time::timeout(Duration::from_secs(5), async {
        while alerts.response().last_delivery.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(received.lock().len(), 1);
    alerts.submit(round(now));
    let permit = alerts.send_gate.acquire().await.unwrap();
    assert_eq!(received.lock().len(), 1);
    drop(permit);
    task.abort();
}
