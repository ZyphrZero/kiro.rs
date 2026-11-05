use super::*;

struct ConfigFile(PathBuf);

impl ConfigFile {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kiro-tool-compatibility-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            r#"{"toolCompatibilityMode":"claude-code","region":"eu-central-1"}"#,
        )
        .unwrap();
        Self(path)
    }

    fn manager(&self) -> MultiTokenManager {
        MultiTokenManager::new(
            Config::load(&self.0).unwrap(),
            vec![KiroCredentials::default()],
            None,
            None,
            false,
        )
        .unwrap()
    }
}

impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn tool_mode_persists_and_survives_restart_without_changing_other_config() {
    let file = ConfigFile::new();
    let manager = file.manager();
    for mode in [
        ToolCompatibilityMode::Raw,
        ToolCompatibilityMode::ClaudeCode,
        ToolCompatibilityMode::Raw,
    ] {
        assert_eq!(manager.set_tool_compatibility_mode(mode).unwrap(), mode);
        assert_eq!(manager.tool_compatibility_mode(), mode);
        let persisted = Config::load(&file.0).unwrap();
        assert_eq!(persisted.tool_compatibility_mode, mode);
        assert_eq!(persisted.region, "eu-central-1");
        assert_eq!(file.manager().tool_compatibility_mode(), mode);
    }
}

#[test]
fn tool_mode_failed_reload_does_not_publish_and_can_retry() {
    let file = ConfigFile::new();
    let manager = file.manager();
    std::fs::write(&file.0, "invalid json").unwrap();
    assert!(
        manager
            .set_tool_compatibility_mode(ToolCompatibilityMode::Raw)
            .is_err()
    );
    assert_eq!(
        manager.tool_compatibility_mode(),
        ToolCompatibilityMode::ClaudeCode
    );
    std::fs::write(&file.0, "{}").unwrap();
    manager
        .set_tool_compatibility_mode(ToolCompatibilityMode::Raw)
        .unwrap();
    assert_eq!(
        manager.tool_compatibility_mode(),
        ToolCompatibilityMode::Raw
    );
}

#[cfg(windows)]
#[test]
fn tool_mode_write_failure_does_not_publish() {
    let file = ConfigFile::new();
    let manager = file.manager();
    let permissions = std::fs::metadata(&file.0).unwrap().permissions();
    let mut readonly = permissions.clone();
    readonly.set_readonly(true);
    std::fs::set_permissions(&file.0, readonly).unwrap();
    let result = manager.set_tool_compatibility_mode(ToolCompatibilityMode::Raw);
    std::fs::set_permissions(&file.0, permissions).unwrap();
    assert!(result.is_err());
    assert_eq!(
        manager.tool_compatibility_mode(),
        ToolCompatibilityMode::ClaudeCode
    );
    assert_eq!(
        Config::load(&file.0).unwrap().tool_compatibility_mode,
        ToolCompatibilityMode::ClaudeCode
    );
}

#[test]
fn tool_mode_requires_persistence_path() {
    let manager = MultiTokenManager::new(
        Config::default(),
        vec![KiroCredentials::default()],
        None,
        None,
        false,
    )
    .unwrap();
    let previous = manager.tool_compatibility_mode();
    assert!(
        manager
            .set_tool_compatibility_mode(ToolCompatibilityMode::ClaudeCode)
            .is_err()
    );
    assert_eq!(manager.tool_compatibility_mode(), previous);
}

#[test]
fn tool_mode_serializes_persistence_and_publication() {
    let file = ConfigFile::new();
    let manager = Arc::new(file.manager());
    let mode_guard = manager.tool_compatibility_mode.lock();
    let first_manager = manager.clone();
    let first = std::thread::spawn(move || {
        first_manager.set_tool_compatibility_mode(ToolCompatibilityMode::Raw)
    });
    let deadline = Instant::now() + StdDuration::from_secs(5);
    let mut persisted_first = false;
    while Instant::now() < deadline {
        if Config::load(&file.0)
            .is_ok_and(|config| config.tool_compatibility_mode == ToolCompatibilityMode::Raw)
        {
            persisted_first = true;
            break;
        }
        std::thread::yield_now();
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let second_manager = manager.clone();
    let second = std::thread::spawn(move || {
        let result = second_manager.set_tool_compatibility_mode(ToolCompatibilityMode::ClaudeCode);
        sender.send(result).unwrap();
    });
    let completed_early = receiver.recv_timeout(StdDuration::from_millis(100)).ok();
    let persisted_during_publication = Config::load(&file.0).unwrap().tool_compatibility_mode;
    drop(mode_guard);
    assert_eq!(first.join().unwrap().unwrap(), ToolCompatibilityMode::Raw);
    second.join().unwrap();
    let second_result = completed_early.as_ref().map(|result| result.is_ok());
    if completed_early.is_none() {
        assert_eq!(
            receiver.recv().unwrap().unwrap(),
            ToolCompatibilityMode::ClaudeCode
        );
    }
    assert!(persisted_first);
    assert!(
        second_result.is_none(),
        "second update bypassed publication lock"
    );
    assert_eq!(persisted_during_publication, ToolCompatibilityMode::Raw);
    assert_eq!(
        manager.tool_compatibility_mode(),
        Config::load(&file.0).unwrap().tool_compatibility_mode
    );
}
