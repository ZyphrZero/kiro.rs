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
