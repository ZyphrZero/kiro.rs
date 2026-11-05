use super::*;
use crate::{
    kiro::{model::credentials::CredentialsConfig, token_manager::MultiTokenManager},
    model::config::Config,
};
use serde_json::json;
use std::sync::Arc;

struct Fixture {
    directory: PathBuf,
    path: PathBuf,
}

impl Fixture {
    fn new(records: Value) -> Self {
        let directory = std::env::temp_dir().join(format!("kiro-dedup-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("credentials.json");
        fs::write(&path, serde_json::to_vec_pretty(&records).unwrap()).unwrap();
        Self { directory, path }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for entry in fs::read_dir(&self.directory).unwrap() {
            let path = entry.unwrap().path();
            fs::remove_file(path).unwrap();
        }
        fs::remove_dir(&self.directory).unwrap();
    }
}

fn api_credential(id: u64, key: &str) -> Value {
    json!({"id":id, "kiroApiKey":key, "authMethod":"api_key"})
}

#[test]
fn issue151_preview_is_read_only_and_never_prints_secrets() {
    let fixture = Fixture::new(json!([
        api_credential(1, "fixture-shared-secret"),
        api_credential(2, "fixture-shared-secret")
    ]));
    let before = fs::read(&fixture.path).unwrap();
    let report = run(&fixture.path, false).unwrap();
    assert!(!report.applied);
    assert_eq!(report.retained_count, 1);
    assert_eq!(report.groups[0].keep_id, Some(1));
    assert_eq!(fs::read(&fixture.path).unwrap(), before);
    assert_eq!(fs::read_dir(&fixture.directory).unwrap().count(), 1);
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(!serialized.contains("fixture-shared-secret"));
    assert!(!serialized.contains(&hex::encode(Sha256::digest(b"fixture-shared-secret"))));
}

#[test]
fn issue151_cleanup_keeps_used_original_id_metadata_and_stats_with_backups() {
    let mut original = api_credential(7, "fixture-key");
    original["createdAt"] = json!("2026-10-15T00:00:00Z");
    original["groups"] = json!(["restricted"]);
    original["disabled"] = json!(true);
    original["priority"] = json!(9);
    original["proxyUrl"] = json!("http://127.0.0.1:1");
    original["customUnknownField"] = json!({"keep":"exactly"});
    let fixture = Fixture::new(json!([
        api_credential(1, "fixture-key"),
        original.clone(),
        api_credential(20, "fixture-key")
    ]));
    let stats_path = fixture.directory.join("kiro_stats.json");
    let stats = serde_json::to_vec(
        &json!({"7":{"success_count":4023,"last_used_at":"2026-11-05T00:00:00Z"}}),
    )
    .unwrap();
    fs::write(&stats_path, &stats).unwrap();
    let before = fs::read(&fixture.path).unwrap();
    let report = run(&fixture.path, true).unwrap();
    assert!(report.applied);
    assert_eq!(report.groups[0].keep_id, Some(7));
    assert_eq!(fs::read(report.backup.unwrap()).unwrap(), before);
    assert_eq!(fs::read(report.stats_backup.unwrap()).unwrap(), stats);
    assert_eq!(fs::read(&stats_path).unwrap(), stats);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&fixture.path).unwrap()).unwrap(),
        json!([original])
    );
    assert_eq!(reserved_id(&fixture.path).unwrap(), 20);
    let manager = MultiTokenManager::new(
        Config::default(),
        CredentialsConfig::load(&fixture.path)
            .unwrap()
            .into_sorted_credentials(),
        None,
        Some(fixture.path.clone()),
        true,
    )
    .unwrap();
    assert_eq!(manager.snapshot().entries[0].id, 7);
    assert_eq!(manager.snapshot().entries[0].success_count, 4023);
    let files_after_first = fs::read_dir(&fixture.directory).unwrap().count();
    assert!(!run(&fixture.path, true).unwrap().applied);
    assert_eq!(
        fs::read_dir(&fixture.directory).unwrap().count(),
        files_after_first
    );
}

#[test]
fn issue151_identity_uses_full_typed_secret_not_email_or_masked_suffix() {
    let credentials: Vec<KiroCredentials> = serde_json::from_value(json!([
        {"kiroApiKey":"first-sameSuffix", "email":"same@example.invalid"},
        {"kiroApiKey":"other-sameSuffix", "email":"same@example.invalid"},
        {"kiroApiKey":"first-sameSuffix"},
        {"refreshToken":"oauth-secret"},
        {"refreshToken":"oauth-secret"},
        {"kiroApiKey":"oauth-secret"},
        {"kiroApiKey":""}, {"kiroApiKey":""}, {}, {}
    ]))
    .unwrap();
    assert_eq!(duplicate_groups(&credentials), vec![vec![0, 2], vec![3, 4]]);
}

#[test]
fn issue151_refresh_token_cleanup_prefers_known_created_at_without_fabricating_dates() {
    let fixture = Fixture::new(json!([
        {"id":1,"refreshToken":"fixture-refresh"},
        {"id":2,"refreshToken":"fixture-refresh","createdAt":"2026-10-15T00:00:00Z"},
        {"id":3,"refreshToken":"different-refresh"}
    ]));
    let report = run(&fixture.path, true).unwrap();
    assert_eq!(report.groups[0].kind, "refreshToken");
    assert_eq!(report.groups[0].keep_id, Some(2));
    let records: Value = serde_json::from_slice(&fs::read(&fixture.path).unwrap()).unwrap();
    assert!(records[1].get("createdAt").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn issue151_removed_ids_are_not_reused_after_multiple_restarts() {
    let fixture = Fixture::new(json!([
        api_credential(1, "fixture-key"),
        api_credential(20, "fixture-key")
    ]));
    run(&fixture.path, true).unwrap();
    for _ in 0..3 {
        let manager = MultiTokenManager::new(
            Config::default(),
            CredentialsConfig::load(&fixture.path)
                .unwrap()
                .into_sorted_credentials(),
            None,
            Some(fixture.path.clone()),
            true,
        )
        .unwrap();
        drop(manager);
    }
    let manager = Arc::new(
        MultiTokenManager::new(
            Config::default(),
            CredentialsConfig::load(&fixture.path)
                .unwrap()
                .into_sorted_credentials(),
            None,
            Some(fixture.path.clone()),
            true,
        )
        .unwrap(),
    );
    manager.set_test_models_fetcher(Arc::new(|_| {
        Box::pin(async { anyhow::bail!("fixture does not call upstream") })
    }));
    let credential = serde_json::from_value(api_credential(0, "fixture-new-key")).unwrap();
    assert!(manager.add_credential(credential).await.unwrap() > 20);
}

#[test]
fn issue151_malformed_stats_and_id_conflicts_do_not_modify_credentials() {
    let fixture = Fixture::new(json!([
        api_credential(1, "fixture-key"),
        api_credential(2, "fixture-key"),
        api_credential(1, "different-key")
    ]));
    let original = fs::read(&fixture.path).unwrap();
    assert!(run(&fixture.path, true).is_err());
    assert_eq!(fs::read(&fixture.path).unwrap(), original);
    fs::write(fixture.directory.join("kiro_stats.json"), b"{invalid}").unwrap();
    assert!(run(&fixture.path, true).is_err());
    assert_eq!(fs::read(&fixture.path).unwrap(), original);
}

#[cfg(windows)]
#[test]
fn issue151_readonly_credentials_fail_without_overwriting_source() {
    let fixture = Fixture::new(json!([
        api_credential(1, "fixture-key"),
        api_credential(2, "fixture-key")
    ]));
    let original = fs::read(&fixture.path).unwrap();
    let permissions = fs::metadata(&fixture.path).unwrap().permissions();
    let mut readonly = permissions.clone();
    readonly.set_readonly(true);
    fs::set_permissions(&fixture.path, readonly).unwrap();
    let result = run(&fixture.path, true);
    fs::set_permissions(&fixture.path, permissions).unwrap();
    assert!(result.is_err());
    assert_eq!(fs::read(&fixture.path).unwrap(), original);
    assert!(!fs::read_dir(&fixture.directory).unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|extension| extension == "tmp")
    }));
}

#[cfg(unix)]
#[test]
fn issue151_backups_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(json!([
        api_credential(1, "fixture-key"),
        api_credential(2, "fixture-key")
    ]));
    let report = run(&fixture.path, true).unwrap();
    assert_eq!(
        fs::metadata(report.backup.unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
