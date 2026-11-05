use super::*;
use serde_json::json;

#[tokio::test]
async fn issue151_single_and_batch_import_reject_existing_api_key() {
    let api_key = "fixture-issue151-api-key";
    let credential = serde_json::from_value(json!({
        "id":7, "kiroApiKey":api_key, "createdAt":"2026-10-15T00:00:00Z"
    }))
    .unwrap();
    let manager = Arc::new(
        MultiTokenManager::new(Config::default(), vec![credential], None, None, false).unwrap(),
    );
    let service = AdminService::new(manager.clone(), vec!["ide".into()]);
    let request =
        || serde_json::from_value(json!({"authMethod":"api_key", "kiroApiKey":api_key})).unwrap();
    let single = service.add_credential(request()).await.unwrap_err();
    assert!(single.to_string().contains("凭据已存在"));
    assert!(!single.to_string().contains(api_key));
    let batch = service.import_one_credential(request(), false).await;
    assert!(matches!(batch.status, ImportStatus::Duplicate));
    assert!(!batch.error.unwrap().contains(api_key));
    assert_eq!(manager.snapshot().total, 1);
    assert_eq!(manager.snapshot().entries[0].id, 7);
}

#[tokio::test]
async fn issue151_parallel_batch_import_inserts_api_key_once() {
    let manager =
        Arc::new(MultiTokenManager::new(Config::default(), vec![], None, None, false).unwrap());
    manager.set_test_models_fetcher(Arc::new(|_| {
        Box::pin(async { anyhow::bail!("fixture does not call upstream") })
    }));
    let service = AdminService::new(manager.clone(), vec!["ide".into()]);
    let request = || {
        serde_json::from_value(
            json!({"authMethod":"api_key", "kiroApiKey":"fixture-issue151-api-key"}),
        )
        .unwrap()
    };
    let (first, second) = tokio::join!(
        service.import_one_credential(request(), false),
        service.import_one_credential(request(), false),
    );
    let statuses = [first.status, second.status];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| matches!(status, ImportStatus::Imported))
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| matches!(status, ImportStatus::Duplicate))
            .count(),
        1
    );
    assert_eq!(manager.snapshot().total, 1);
    assert!(manager.snapshot().entries[0].created_at.is_some());
}
