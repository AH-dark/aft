use super::*;
use serde_json::json;

#[test]
fn fresh_wait_releases_manager_and_requires_each_producer_version() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("lib.rs");
    std::fs::write(&file, "pub fn f() {}\n").unwrap();
    let manager = Arc::new(parking_lot::Mutex::new(LspManager::new()));
    let keys = [ServerKind::Rust, ServerKind::TypeScript].map(|kind| ServerKey {
        kind,
        root: root.path().to_path_buf(),
    });
    let expected = keys.iter().cloned().map(|key| (key, 2)).collect::<Vec<_>>();
    let pre = {
        let mut lsp = manager.lock();
        for key in &keys {
            let mut client = LspClient::spawn_managed_with_reclaim_root(
                key.kind.clone(),
                key.root.clone(),
                Path::new("sh"),
                &["-c".into(), "exec sleep 60".into()],
                &HashMap::new(),
                lsp.event_sender_for_test(),
                ServerInstanceId::next(),
                lsp.child_registry.clone(),
                None,
                false,
            )
            .unwrap();
            client.set_rust_analyzer_quiescent(true);
            lsp.clients.insert(key.clone(), client);
            lsp.documents
                .entry(key.clone())
                .or_default()
                .open(file.clone());
            lsp.diagnostics
                .publish_full(key.clone(), file.clone(), Vec::new(), None, Some(1));
        }
        lsp.snapshot_pre_edit_state(&file)
    };
    let parked_manager = Arc::clone(&manager);
    let parked_file = file.clone();
    POST_EDIT_PARK_PROBE.with(|probe| {
        *probe.borrow_mut() = Some(Box::new(move || {
            let lsp = parked_manager
                .try_lock()
                .expect("manager lock held across a diagnostics wait");
            for (key, version) in [(&keys[0], 1), (&keys[0], 2), (&keys[1], 2)] {
                lsp.enqueue_event_for_test(LspEvent::Notification {
                server_kind: key.kind.clone(), root: key.root.clone(),
                method: "textDocument/publishDiagnostics".into(),
                params: Some(json!({"uri": uri_for_path(&parked_file).unwrap(), "version": version,
                    "diagnostics": if version == 1 { json!([{
                        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                        "message":"stale error", "severity":1
                    }]) } else { json!([]) }
                })),
            });
            }
        }))
    });
    let result = wait_for_post_edit_diagnostics_unlocked(
        || manager.lock(),
        &file,
        &expected,
        &pre,
        Duration::from_secs(5),
    );
    assert!(
        result.complete(),
        "partial or stale producer set was treated as complete: {result:?}"
    );
    assert_eq!(result.accepted_snapshots.len(), 2);
    assert!(
        result.diagnostics.is_empty(),
        "an old-version error was accepted as fresh"
    );
    assert!(result
        .accepted_snapshots
        .iter()
        .all(|snapshot| snapshot.document_version == 2));
}
