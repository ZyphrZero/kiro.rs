use chrono::{Duration, Utc};
use serde_json::json;

use super::{CostStore, ledger};
use crate::kiro::parser::frame::Frame;
use crate::kiro::parser::header::{HeaderValue, Headers};

fn meter(payload: &str) -> Frame {
    let mut headers = Headers::new();
    headers.insert(
        ":event-type".into(),
        HeaderValue::String("meteringEvent".into()),
    );
    Frame {
        headers,
        payload: payload.as_bytes().to_vec(),
    }
}

#[tokio::test]
async fn account_costs_persist_and_preserve_attempt_attribution() {
    let directory = std::env::temp_dir().join(format!("kiro-cost-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("costs.db");
    let store = CostStore::open(path.clone()).unwrap();
    let now = Utc::now();
    for (id, amount, credits) in [(1, "10", "100"), (2, "12", "200")] {
        let lot = uuid::Uuid::new_v4().to_string();
        let purchase = json!({
            "id": lot, "credentialId": id, "amount": amount, "credits": credits,
            "paidAt": now - Duration::minutes(5), "effectiveAt": now - Duration::minutes(5),
            "kind": "plan",
        });
        store
            .purchase(serde_json::from_value(purchase.clone()).unwrap())
            .await
            .unwrap();
        store
            .purchase(serde_json::from_value(purchase).unwrap())
            .await
            .unwrap();
    }

    let mut first = store
        .start_call(1, "opus", "ide", 0, "api", Some("same-request"))
        .await;
    first.observe(&meter(r#"{"unit":"credit","usage":30}"#));
    first.finish("partial").await;
    let mut retry = store
        .start_call(2, "opus", "ide", 1, "api", Some("same-request"))
        .await;
    retry.observe(&meter(r#"{"unit":"credit","usage":25}"#));
    retry.observe(&meter(r#"{"unit":"credit","usage":25}"#));
    retry.finish("complete").await;
    let missing = store.start_call(2, "opus", "ide", 0, "api", None).await;
    missing.finish("complete").await;

    // Drop the first writer and read the same persisted facts with a new store.
    drop(store);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let store = CostStore::open(path.clone()).unwrap();
    assert_eq!(CostStore::reserved_credential_id(&path).unwrap(), 2);
    let get_report = |id| {
        let window = serde_json::from_value(json!({
            "credentialId": id,
            "from": now - Duration::minutes(10), "to": Utc::now() + Duration::minutes(1),
        }))
        .unwrap();
        ledger::report(&store, window).unwrap()
    };
    let all = get_report(None);
    assert_eq!(all["totals"]["paid"], "22");
    assert_eq!(all["totals"]["usageCost"], "6");
    assert_eq!(all["totals"]["remainingCost"], "16");
    assert_eq!(all["totals"]["partialMeteringCalls"], 1);
    assert_eq!(all["totals"]["missingMeteringCalls"], 1);
    for (id, credits, attempt) in [(1, "30", 0), (2, "50", 1)] {
        let account = get_report(Some(id));
        assert_eq!(account["totals"]["usageCost"], "3");
        assert_eq!(account["totals"]["reportedCredits"], credits);
        let call = account["recentCalls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|call| call["requestId"] == "same-request")
            .unwrap();
        assert_eq!(call["attempt"], attempt);
    }
    drop(store);
    for _ in 0..20 {
        if std::fs::remove_dir_all(&directory).is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[test]
fn metering_does_not_convert_source_to_float_or_treat_missing_as_zero() {
    assert_eq!(
        super::metering_literal(&meter(r#"{"unit":"credit","usage":0.0169543708291874}"#))
            .as_deref(),
        Some("0.0169543708291874")
    );
    assert_eq!(
        super::metering_literal(&meter(r#"{"unit":"credit","usage":0}"#)).as_deref(),
        Some("0")
    );
    assert_eq!(
        super::metering_literal(&meter(r#"{"unit":"credit"}"#)),
        None
    );
    assert_eq!(
        super::metering_literal(&meter(r#"{"unit":"credit","usage":"0"}"#)),
        None
    );
    assert_eq!(
        super::metering_literal(&meter(r#"{"unit":"unknown","usage":1}"#)),
        None
    );
}

struct Fixture {
    directory: std::path::PathBuf,
    store: CostStore,
}

impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("kiro-cost-fix-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let store = CostStore::open(directory.join("costs.db")).unwrap();
        Self { directory, store }
    }

    fn db(&self) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(self.directory.join("costs.db")).unwrap();
        db.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
        db
    }

    fn report(&self, from: chrono::DateTime<Utc>, credential: Option<u64>) -> serde_json::Value {
        ledger::report(
            &self.store,
            serde_json::from_value(json!({
                "from": from, "to": Utc::now() + Duration::minutes(1), "credentialId": credential,
            }))
            .unwrap(),
        )
        .unwrap()
    }

    async fn close(self) {
        self.store.shutdown().await.unwrap();
        drop(self.store);
        std::fs::remove_dir_all(self.directory).unwrap();
    }
}

fn insert_lot(
    db: &rusqlite::Connection,
    credential: u64,
    amount: &str,
    credits: &str,
    effective: chrono::DateTime<Utc>,
    kind: &str,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    db.execute(
        "INSERT INTO cost_lots(id,credential_id,paid_at,effective_at,amount,credits,kind)
        VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![
            id,
            credential,
            (Utc::now() - Duration::hours(1)).timestamp_millis(),
            effective.timestamp_millis(),
            amount,
            credits,
            kind
        ],
    )
    .unwrap();
    id
}

fn insert_call(db: &rusqlite::Connection, id: &str, at: chrono::DateTime<Utc>, raw: &str) {
    db.execute(
        "INSERT INTO cost_calls VALUES (?1,1,?2,'test','ide',0,'api',NULL,'complete')",
        rusqlite::params![id, at.timestamp_millis()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO cost_meters VALUES (?1,1,?2)",
        rusqlite::params![id, raw],
    )
    .unwrap();
}

#[tokio::test]
async fn failed_start_is_recovered_and_diagnostics_survive_reopen() {
    let fixture = Fixture::new();
    let db = fixture.db();
    insert_lot(
        &db,
        1,
        "10",
        "100",
        Utc::now() - Duration::days(1),
        "purchase",
    );
    insert_call(&db, "baseline", Utc::now() - Duration::minutes(1), "1");
    db.execute_batch(
        "CREATE TRIGGER fail_start BEFORE INSERT ON cost_calls
        WHEN NEW.status IS NULL BEGIN SELECT RAISE(FAIL,'injected Start failure'); END;",
    )
    .unwrap();
    let mut call = fixture
        .store
        .start_call(1, "test", "ide", 0, "api", Some("recovered"))
        .await;
    fixture.store.flush().await.unwrap();
    call.observe(&meter(r#"{"unit":"credit","usage":7}"#));
    call.finish("complete").await;
    fixture.store.flush().await.unwrap();
    let report = fixture.report(Utc::now() - Duration::days(2), Some(1));
    assert_eq!(report["totals"]["attempts"], 2);
    assert_eq!(report["totals"]["reportedCredits"], "8");
    assert_eq!(report["totals"]["usageCost"], "0.8");
    assert_eq!(report["totals"]["partialMeteringCalls"], 1);
    assert!(report["captureErrors"].as_u64().unwrap() > 0);
    fixture.store.shutdown().await.unwrap();
    let reopened = CostStore::open(fixture.directory.join("costs.db")).unwrap();
    assert!(reopened.previous_capture_errors > 0);
    reopened.shutdown().await.unwrap();
    drop(reopened);
    drop(db);
    drop(fixture.store);
    std::fs::remove_dir_all(fixture.directory).unwrap();
}

#[tokio::test]
async fn finish_replays_meter_writes_that_failed() {
    let fixture = Fixture::new();
    let db = fixture.db();
    let mut call = fixture
        .store
        .start_call(1, "test", "ide", 0, "api", None)
        .await;
    fixture.store.flush().await.unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_meter BEFORE INSERT ON cost_meters
        BEGIN SELECT RAISE(FAIL,'injected meter failure'); END;",
    )
    .unwrap();
    call.observe(&meter(r#"{"unit":"credit","usage":7}"#));
    fixture.store.flush().await.unwrap();
    db.execute_batch("DROP TRIGGER fail_meter;").unwrap();
    call.finish("complete").await;
    let report = fixture.report(Utc::now() - Duration::days(1), Some(1));
    assert_eq!(report["totals"]["reportedCredits"], "7");
    assert_eq!(report["totals"]["partialMeteringCalls"], 1);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM cost_meters", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        1
    );
    drop(db);
    fixture.close().await;
}

#[tokio::test]
async fn start_does_not_wait_for_sqlite_and_busy_transaction_is_retried() {
    let fixture = Fixture::new();
    let db = fixture.db();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut call = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        fixture.store.start_call(1, "test", "ide", 0, "api", None),
    )
    .await
    .unwrap();
    call.observe(&meter(r#"{"unit":"credit","usage":7}"#));
    call.finish("complete").await;
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    db.execute_batch("COMMIT").unwrap();
    fixture.store.flush().await.unwrap();
    let report = fixture.report(Utc::now() - Duration::days(1), Some(1));
    assert_eq!(report["totals"]["attempts"], 1);
    assert_eq!(report["totals"]["reportedCredits"], "7");
    assert_eq!(report["totals"]["partialMeteringCalls"], 1);
    drop(db);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_cost_db_keeps_existing_credentials_and_blocks_new_ids() {
    use crate::{
        kiro::{model::credentials::KiroCredentials, token_manager::MultiTokenManager},
        model::config::Config,
    };
    for garbage in [true, false] {
        for enabled in [true, false] {
            let directory =
                std::env::temp_dir().join(format!("kiro-cost-startup-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            if garbage {
                std::fs::write(directory.join("costs.db"), b"garbage").unwrap();
            } else {
                rusqlite::Connection::open(directory.join("costs.db"))
                    .unwrap()
                    .execute_batch("CREATE TABLE cost_calls(credential_id INTEGER)")
                    .unwrap();
            }
            let mut config = Config::default();
            config.cost_tracking_enabled = enabled;
            let make_credential = |id, key: &str| KiroCredentials {
                id,
                kiro_api_key: Some(key.into()),
                auth_method: Some("api_key".into()),
                ..Default::default()
            };
            let credentials = vec![
                make_credential(Some(7), "synthetic-existing"),
                make_credential(None, "synthetic-unassigned"),
            ];
            std::fs::write(
                directory.join("credentials.json"),
                serde_json::to_string(&credentials).unwrap(),
            )
            .unwrap();
            let manager = std::sync::Arc::new(
                MultiTokenManager::new(
                    config,
                    credentials,
                    None,
                    Some(directory.join("credentials.json")),
                    true,
                )
                .unwrap(),
            );
            assert_eq!(manager.snapshot().entries.len(), 1);
            assert_eq!(manager.snapshot().entries[0].id, 7);
            assert!(
                manager
                    .add_credential(make_credential(None, "synthetic-new"))
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("成本库")
            );
            let persisted: Vec<KiroCredentials> = serde_json::from_str(
                &std::fs::read_to_string(directory.join("credentials.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(persisted.len(), 2);
            assert!(persisted.iter().any(|c| c.id.is_none()));
            drop(manager);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}

#[tokio::test]
async fn pending_cost_and_expiry_balances_are_separate_in_pool_and_account() {
    let fixture = Fixture::new();
    let db = fixture.db();
    insert_lot(
        &db,
        1,
        "10",
        "100",
        Utc::now() + Duration::days(2),
        "purchase",
    );
    let expired = insert_lot(
        &db,
        2,
        "8",
        "80",
        Utc::now() - Duration::days(2),
        "purchase",
    );
    db.execute(
        "UPDATE cost_lots SET expires_at=?1 WHERE id=?2",
        rusqlite::params![(Utc::now() - Duration::days(1)).timestamp_millis(), expired],
    )
    .unwrap();
    let from = Utc::now() - Duration::days(3);
    let report = fixture.report(from, None);
    assert_eq!(report["totals"]["paid"], "18");
    assert_eq!(report["totals"]["pendingCost"], "10");
    assert_eq!(report["totals"]["pendingExpiryLoss"], "8");
    assert_eq!(report["totals"]["remainingCost"], "0");
    assert_eq!(fixture.report(from, Some(1))["totals"]["pendingCost"], "10");
    assert_eq!(
        fixture.report(from, Some(2))["totals"]["pendingExpiryLoss"],
        "8"
    );
    drop(db);
    fixture.close().await;
}

#[tokio::test]
async fn invalid_numbers_are_isolated_without_rounding_valid_credits() {
    let fixture = Fixture::new();
    let db = fixture.db();
    for index in 0..8 {
        insert_call(
            &db,
            &format!("large-{index}"),
            Utc::now() - Duration::minutes(1),
            "1e28",
        );
    }
    insert_call(
        &db,
        "tiny",
        Utc::now() - Duration::minutes(1),
        "0.00000000000000000000000000001",
    );
    insert_call(
        &db,
        "good",
        Utc::now() - Duration::minutes(1),
        "0.0169543708291874",
    );
    insert_call(&db, "zero", Utc::now() - Duration::minutes(1), "0");
    let report = fixture.report(Utc::now() - Duration::days(1), Some(1));
    assert_eq!(report["totals"]["invalidMeteringCalls"], 9);
    assert_eq!(report["totals"]["partialMeteringCalls"], 9);
    assert_eq!(report["totals"]["missingMeteringCalls"], 0);
    assert_eq!(report["totals"]["reportedCredits"], "0.0169543708291874");
    assert_eq!(report["totalsAvailable"], true);
    assert_eq!(report["coverageState"], "partial");
    drop(db);
    fixture.close().await;
}

#[tokio::test]
async fn checkpoints_match_replay_and_backdated_facts_invalidate_them() {
    let fixture = Fixture::new();
    let db = fixture.db();
    let today = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    insert_lot(&db, 1, "10", "100", today - Duration::days(10), "purchase");
    insert_call(&db, "old", today - Duration::days(3), "50");
    insert_call(&db, "current", Utc::now() - Duration::milliseconds(1), "10");
    let first = fixture.report(today, Some(1));
    let cached = fixture.report(today, Some(1));
    assert_eq!(first["totals"], cached["totals"]);
    assert_eq!(first["lots"], cached["lots"]);
    assert_eq!(first["recentCalls"], cached["recentCalls"]);
    assert_eq!(first["replayedCalls"], 2);
    assert_eq!(cached["replayedCalls"], 1);
    assert_eq!(cached["totals"]["remainingCost"], "4");
    insert_lot(&db, 1, "20", "100", today - Duration::days(10), "plan");
    let changed = fixture.report(today, Some(1));
    assert_eq!(changed["replayedCalls"], 2);
    assert_eq!(changed["totals"]["usageCost"], "2");
    assert_eq!(changed["totals"]["remainingCost"], "18");
    db.execute("INSERT INTO cost_meters VALUES ('old',2,'10')", [])
        .unwrap();
    let late_meter = fixture.report(today, Some(1));
    assert_eq!(late_meter["replayedCalls"], 2);
    assert_eq!(late_meter["totals"]["remainingCost"], "16");
    assert_eq!(
        fixture.report(today, Some(1))["totals"],
        late_meter["totals"]
    );
    drop(db);
    fixture.close().await;
}

#[tokio::test]
async fn aggregate_precision_overflow_hides_totals_instead_of_failing_report() {
    let fixture = Fixture::new();
    let db = fixture.db();
    let at = Utc::now() - Duration::minutes(1);
    for index in 0..100 {
        insert_call(&db, &format!("large-{index:03}"), at, "1000000000");
    }
    insert_call(
        &db,
        "tiny",
        at + Duration::seconds(1),
        "0.000000000000000001",
    );
    let report = fixture.report(at - Duration::seconds(1), Some(1));
    assert_eq!(report["totalsAvailable"], false);
    assert!(report["totals"]["reportedCredits"].is_null());
    assert_eq!(report["totals"]["attempts"], 101);
    assert_eq!(report["totals"]["invalidMeteringCalls"], 0);
    assert_eq!(report["coverageState"], "partial");
    assert_eq!(report["accounts"][0]["totalsAvailable"], false);
    drop(db);
    fixture.close().await;
}

#[tokio::test]
async fn recent_details_are_limited_but_totals_cover_the_entire_window() {
    let fixture = Fixture::new();
    let db = fixture.db();
    let at = Utc::now() - Duration::minutes(1);
    for index in 0..150 {
        insert_call(
            &db,
            &format!("detail-{index:03}"),
            at + Duration::milliseconds(index),
            "1",
        );
    }
    let report = fixture.report(at - Duration::seconds(1), Some(1));
    assert_eq!(report["totals"]["attempts"], 150);
    assert_eq!(report["totals"]["reportedCredits"], "150");
    assert_eq!(report["recentCalls"].as_array().unwrap().len(), 100);
    assert_eq!(report["recentCalls"][0]["id"], "detail-149");
    assert_eq!(report["recentCalls"][99]["id"], "detail-050");
    drop(db);
    fixture.close().await;
}

#[tokio::test]
async fn checkpoints_reject_stale_snapshots_but_allow_newer_traffic() {
    let fixture = Fixture::new();
    let db = fixture.db();
    let today = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    let revision: i64 = db
        .query_row("SELECT revision FROM cost_revision", [], |r| r.get(0))
        .unwrap();
    insert_call(&db, "old", today - Duration::days(1), "1");
    super::checkpoints::save(
        &fixture.directory.join("costs.db"),
        1,
        revision,
        &[(today.timestamp_millis(), "stale".into())],
    )
    .unwrap();
    assert!(
        super::checkpoints::load(&db, 1, today.timestamp_millis())
            .unwrap()
            .is_none()
    );
    let revision: i64 = db
        .query_row("SELECT revision FROM cost_revision", [], |r| r.get(0))
        .unwrap();
    insert_call(&db, "new", Utc::now(), "1");
    super::checkpoints::save(
        &fixture.directory.join("costs.db"),
        1,
        revision,
        &[(today.timestamp_millis(), "safe".into())],
    )
    .unwrap();
    assert_eq!(
        super::checkpoints::load(&db, 1, today.timestamp_millis())
            .unwrap()
            .unwrap()
            .1,
        "safe"
    );
    drop(db);
    fixture.close().await;
}

#[tokio::test]
async fn shutdown_drains_queued_meters_and_interrupted_calls() {
    let fixture = Fixture::new();
    for _ in 0..100 {
        let mut call = fixture
            .store
            .start_call(1, "test", "ide", 0, "api", None)
            .await;
        call.observe(&meter(r#"{"unit":"credit","usage":1}"#));
        drop(call);
    }
    fixture.store.shutdown().await.unwrap();
    let db = fixture.db();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM cost_meters", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        100
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM cost_calls WHERE status='interrupted'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        100
    );
    drop(db);
    drop(fixture.store);
    std::fs::remove_dir_all(fixture.directory).unwrap();
}

#[tokio::test]
#[ignore = "manual release benchmark: one million synthetic calls"]
async fn benchmark_history_checkpoints() {
    let fixture = Fixture::new();
    let mut db = fixture.db();
    let today = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    insert_lot(
        &db,
        1,
        "1000",
        "1000000",
        today - Duration::days(500),
        "purchase",
    );
    let tx = db.transaction().unwrap();
    {
        let mut calls = tx
            .prepare("INSERT INTO cost_calls VALUES (?1,1,?2,'test','ide',0,'api',NULL,'complete')")
            .unwrap();
        let mut meters = tx
            .prepare("INSERT INTO cost_meters VALUES (?1,1,'0.1')")
            .unwrap();
        for index in 0..1_000_000i64 {
            let id = format!("bench-{index}");
            let at = (today - Duration::days(400)).timestamp_millis() + index * 30_000;
            calls.execute(rusqlite::params![id, at]).unwrap();
            meters.execute([id]).unwrap();
        }
    }
    tx.commit().unwrap();
    for days in [1, 90, 365] {
        let from = today - Duration::days(days - 1);
        let start = std::time::Instant::now();
        let first = fixture.report(from, Some(1));
        let cold = start.elapsed();
        let start = std::time::Instant::now();
        let cached = fixture.report(from, Some(1));
        let warm = start.elapsed();
        assert_eq!(first["totals"], cached["totals"]);
        eprintln!(
            "history=1000000 days={days} first={cold:?} warm={warm:?} replayed_first={} replayed_warm={}",
            first["replayedCalls"], cached["replayedCalls"]
        );
        if days == 1 {
            assert_eq!(cached["replayedCalls"], 0);
        }
    }
    drop(db);
    fixture.close().await;
}

#[tokio::test]
#[ignore = "manual release benchmark: local writer latency, not upstream TTFT"]
async fn benchmark_capture_latency() {
    let fixture = Fixture::new();
    let mut starts = Vec::new();
    let mut finishes = Vec::new();
    let begin = std::time::Instant::now();
    for _ in 0..500 {
        let at = std::time::Instant::now();
        let mut call = fixture
            .store
            .start_call(1, "bench", "ide", 0, "api", None)
            .await;
        starts.push(at.elapsed().as_micros());
        call.observe(&meter(r#"{"unit":"credit","usage":1}"#));
        let at = std::time::Instant::now();
        call.finish("complete").await;
        finishes.push(at.elapsed().as_micros());
    }
    starts.sort_unstable();
    finishes.sort_unstable();
    eprintln!(
        "local calls=500 elapsed={:?} start_p50_us={} start_p99_us={} finish_p50_us={} finish_p99_us={}",
        begin.elapsed(),
        starts[250],
        starts[494],
        finishes[250],
        finishes[494]
    );
    fixture.close().await;
}
