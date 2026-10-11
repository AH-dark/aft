use super::*;
use crate::config::UserServerDef;
use std::os::unix::fs::PermissionsExt;

fn reserved_fixture() -> (
    tempfile::TempDir,
    Config,
    PathBuf,
    LspManager,
    ServerDef,
    ServerKey,
) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("marker"), "marker").unwrap();
    let file = root.path().join("source.reserved");
    std::fs::write(&file, "content").unwrap();
    let mut frame = Vec::new();
    super::super::transport::write_message(
        &mut frame,
        r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#,
    )
    .unwrap();
    let script = root.path().join("server.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ >> '{}'\nIFS= read -r header\nprintf '%s' '{}'\nexec sleep 60\n",
            root.path().join("spawns").display(),
            String::from_utf8(frame).unwrap(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = Config {
        lsp_servers: vec![UserServerDef {
            id: "reserved".into(),
            extensions: vec!["reserved".into()],
            binary: "reserved".into(),
            args: Vec::new(),
            root_markers: vec!["marker".into()],
            env: HashMap::new(),
            initialization_options: None,
            disabled: false,
        }],
        ..Config::default()
    };
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Custom(Arc::from("reserved")), script);
    let def = servers_for_file(&file, &config).remove(0);
    let key = server_key_for_definition(&def, &file, &config).unwrap();
    (root, config, file, manager, def, key)
}

#[test]
fn locked_file_start_preserves_reservation_and_reports_in_progress() {
    let (root, config, file, mut manager, def, key) = reserved_fixture();
    let FileServerStart::Spawn(prepared) =
        manager.begin_file_server_start(&def, &key, &file, &config)
    else {
        panic!("start not reserved")
    };
    let signal = Arc::clone(&manager.starting[&key].signal);
    let result = manager.ensure_server_for_file_detailed(&file, &config);
    assert!(
        matches!(&result.attempts[0].result, ServerAttemptResult::InProgress { server_key } if server_key == &key),
        "a reserved start was misreported: {result:?}"
    );
    assert!(result.successful.is_empty());
    assert!(
        !root.path().join("spawns").exists(),
        "locked path launched a duplicate"
    );
    assert!(!signal.wait_until(Instant::now()));
    assert!(!manager.failed_spawns.contains_key(&key));
    assert!(!manager.transient_backoff.contains_key(&key));
    let client = prepared
        .run(Some(Duration::from_secs(5)))
        .unwrap_or_else(|_| panic!("handshake failed"));
    let pid = client.child_pid();
    assert!(manager
        .finish_file_server_start(&def, &key, Ok(client))
        .is_none());
    assert_eq!(manager.clients[&key].child_pid(), pid);
    assert_eq!(
        std::fs::read_to_string(root.path().join("spawns"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(signal.wait_until(Instant::now()));
}

#[test]
fn locked_area_start_preserves_reservation_and_reports_in_progress() {
    let (root, config, file, mut manager, def, key) = reserved_fixture();
    let FileServerStart::Spawn(_prepared) =
        manager.begin_file_server_start(&def, &key, &file, &config)
    else {
        panic!("start not reserved")
    };
    let snapshot = ApplicableServerSnapshot {
        server_keys: vec![key.clone()],
        candidates: vec![ApplicableServerCandidate {
            key: key.clone(),
            definition: def,
            source_file: file,
        }],
        ..ApplicableServerSnapshot::default()
    };
    let result = manager.start_applicable_servers(&snapshot, &config);
    assert_eq!(
        result.in_progress,
        vec![key.clone()],
        "reserved area start was reported ready or failed"
    );
    assert!(result.successful.is_empty());
    assert!(result.failures.is_empty());
    assert!(
        !root.path().join("spawns").exists(),
        "area path launched a duplicate"
    );
    assert!(manager.starting.contains_key(&key));
    assert!(!manager.failed_spawns.contains_key(&key));
    manager.release_start_reservation(&key);
}

#[test]
fn in_progress_is_never_cached_as_a_start_failure() {
    let (_root, _config, _file, mut manager, _def, key) = reserved_fixture();
    let result = manager.record_failed_spawn(
        &key,
        ServerAttemptResult::InProgress {
            server_key: key.clone(),
        },
        FailureDurability::Transient,
    );
    assert!(matches!(result, ServerAttemptResult::InProgress { .. }));
    assert!(
        !manager.failed_spawns.contains_key(&key),
        "in-progress start became a cached failure"
    );
    assert!(!manager.transient_backoff.contains_key(&key));
}
