use super::*;

const TEST_API_KEY: &str = "fixture-issue151-key";

#[test]
fn issue151_apply_requires_explicit_dedup_mode() {
    assert!(Args::try_parse_from(["kiro-rs", "--apply-credential-dedup"]).is_err());
    let arguments = Args::try_parse_from([
        "kiro-rs",
        "--deduplicate-credentials",
        "--apply-credential-dedup",
    ])
    .unwrap();
    assert!(arguments.deduplicate_credentials);
    assert!(arguments.apply_credential_dedup);
}

#[test]
fn issue151_empty_environment_key_does_not_create_records() {
    for key in ["", " \t\n"] {
        let mut credentials = Vec::new();
        merge_environment_api_key(&mut credentials, key.into());
        assert!(credentials.is_empty());
    }
}

#[test]
fn issue151_environment_key_reuses_existing_record_without_overriding_metadata() {
    let original: KiroCredentials = serde_json::from_value(serde_json::json!({
        "id": 7,
        "kiroApiKey": TEST_API_KEY,
        "createdAt": "2026-10-15T00:00:00Z",
        "priority": 5,
        "groups": ["restricted"],
        "disabled": true,
        "proxyUrl": "http://127.0.0.1:1"
    }))
    .unwrap();
    let expected = serde_json::to_value(&original).unwrap();
    let mut credentials = vec![original];
    merge_environment_api_key(&mut credentials, TEST_API_KEY.into());
    assert_eq!(credentials.len(), 1);
    assert_eq!(serde_json::to_value(&credentials[0]).unwrap(), expected);
}

#[test]
fn issue151_environment_key_restart_does_not_grow_credentials_file() {
    let directory = std::env::temp_dir().join(format!("kiro-issue151-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("credentials.json");
    std::fs::write(&path, "[]").unwrap();
    let mut counts = Vec::new();
    for _ in 0..5 {
        let config = CredentialsConfig::load(&path).unwrap();
        let mut credentials = config.into_sorted_credentials();
        merge_environment_api_key(&mut credentials, TEST_API_KEY.into());
        let manager = MultiTokenManager::new(
            Config::default(),
            credentials,
            None,
            Some(path.clone()),
            true,
        )
        .unwrap();
        counts.push(manager.snapshot().entries.len());
        drop(manager);
    }
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir(directory).unwrap();
    assert_eq!(counts, vec![1, 1, 1, 1, 1]);
}

#[test]
fn issue151_new_environment_key_has_created_at() {
    let mut credentials = Vec::new();
    merge_environment_api_key(&mut credentials, TEST_API_KEY.into());
    assert_eq!(credentials.len(), 1);
    assert!(
        credentials[0]
            .created_at
            .as_deref()
            .is_some_and(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok())
    );
}
