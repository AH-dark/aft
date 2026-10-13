use super::*;
use serde_json::{json, Value};
use std::time::SystemTime;

fn framed(messages: &[Value]) -> String {
    let mut bytes = Vec::new();
    for message in messages {
        super::super::transport::write_message(&mut bytes, &message.to_string()).unwrap();
    }
    String::from_utf8(bytes).unwrap()
}

fn progress(kind: &str, token: &str) -> Value {
    json!({"jsonrpc":"2.0", "method":"$/progress", "params": {
        "token": token, "value": {"kind":kind, "title":"cargo check"}
    }})
}

fn status() -> Value {
    json!({"jsonrpc":"2.0", "method":"experimental/serverStatus", "params": {
        "health":"ok", "quiescent":true
    }})
}

fn spawn(manager: &LspManager, root: &Path, script: &str) -> LspClient {
    LspClient::spawn_managed_with_reclaim_root(
        ServerKind::Rust,
        root.to_path_buf(),
        Path::new("sh"),
        &["-c".into(), script.into()],
        &HashMap::new(),
        manager.event_sender_for_test(),
        ServerInstanceId::next(),
        manager.child_registry.clone(),
        None,
        false,
    )
    .unwrap()
}

fn recv(manager: &LspManager) -> ReceivedLspEvent {
    manager
        .event_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("framed reader event")
}

fn key(root: &Path) -> ServerKey {
    ServerKey {
        kind: ServerKind::Rust,
        root: root.to_path_buf(),
    }
}

#[test]
fn old_instance_begin_end_cannot_certify_replacement_check() {
    let root = tempfile::tempdir().unwrap();
    let mut manager = LspManager::new();
    let old = spawn(
        &manager,
        root.path(),
        &format!(
            "printf '%s' '{}'; exec sleep 60",
            framed(&[
                progress("begin", "rust-analyzer/flycheck/0"),
                progress("end", "rust-analyzer/flycheck/0")
            ])
        ),
    );
    let begin = recv(&manager);
    let end = recv(&manager);
    let mut live = spawn(&manager, root.path(), "exec sleep 60");
    live.set_rust_analyzer_quiescent(true);
    let key = key(root.path());
    manager.clients.insert(key.clone(), live);
    manager.handle_received_event(&begin);
    assert_eq!(
        manager.clients[&key].rust_check_state(Instant::now(), Duration::ZERO),
        RustCheckState::Current,
        "old begin started a check in the replacement"
    );
    manager.handle_received_event(&end);
    assert!(
        !manager.clients[&key].rust_check_completed_current(Instant::now(), Duration::ZERO),
        "old begin/end certified the replacement's compiler results"
    );
    drop(old);
}

#[test]
fn old_instance_end_cannot_finish_live_check_with_same_token() {
    let root = tempfile::tempdir().unwrap();
    let mut manager = LspManager::new();
    let mut live = spawn(
        &manager,
        root.path(),
        &format!(
            "printf '%s' '{}'; exec sleep 60",
            framed(&[
                progress("begin", "rust-analyzer/flycheck/0"),
                progress("end", "rust-analyzer/flycheck/0")
            ])
        ),
    );
    let live_begin = recv(&manager);
    let live_end = recv(&manager);
    let old = spawn(
        &manager,
        root.path(),
        &format!(
            "printf '%s' '{}'; exec sleep 60",
            framed(&[progress("end", "rust-analyzer/flycheck/0")])
        ),
    );
    let old_end = recv(&manager);
    live.set_rust_analyzer_quiescent(true);
    let key = key(root.path());
    manager.clients.insert(key.clone(), live);
    manager.handle_received_event(&live_begin);
    manager.handle_received_event(&old_end);
    assert_eq!(
        manager.clients[&key].rust_check_state(Instant::now(), Duration::ZERO),
        RustCheckState::Running,
        "old end completed a live check with a reused token"
    );
    manager.handle_received_event(&live_end);
    assert!(manager.clients[&key].rust_check_completed_current(Instant::now(), Duration::ZERO));
    drop(old);
}

#[test]
fn old_instance_status_cannot_promote_replacement_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let mut manager = LspManager::new();
    let old = spawn(
        &manager,
        root.path(),
        &format!("printf '%s' '{}'; exec sleep 60", framed(&[status()])),
    );
    let old_status = recv(&manager);
    let live = spawn(
        &manager,
        root.path(),
        &format!("printf '%s' '{}'; exec sleep 60", framed(&[status()])),
    );
    let live_status = recv(&manager);
    let key = key(root.path());
    let file = root.path().join("lib.rs");
    manager.clients.insert(key.clone(), live);
    manager.diagnostics.publish_full_with_provisional(
        key.clone(),
        file.clone(),
        Vec::new(),
        None,
        Some(0),
        true,
    );
    manager.handle_received_event(&old_status);
    assert!(
        manager.clients[&key].diagnostics_are_provisional(),
        "old status settled the replacement"
    );
    assert!(manager.diagnostics.entries_for_file(&file)[0].1.provisional);
    manager.handle_received_event(&live_status);
    assert!(!manager.clients[&key].diagnostics_are_provisional());
    assert!(!manager.diagnostics.entries_for_file(&file)[0].1.provisional);
    drop(old);
}

#[test]
fn deferred_reader_begin_keeps_ordinal_across_a_missing_earlier_begin() {
    let root = tempfile::tempdir().unwrap();
    let mut manager = LspManager::new();
    let first = framed(&[progress("begin", "rust-analyzer/flycheck/0")]);
    let second = framed(&[
        progress("begin", "rust-analyzer/flycheck/1"),
        progress("end", "rust-analyzer/flycheck/1"),
    ]);
    let mut live = spawn(&manager, root.path(), &format!(
        "printf '%s' '{first}'; while [ ! -f release ]; do sleep 0.02; done; printf '%s' '{second}'; exec sleep 60"));
    let _earlier_begin_not_applied = recv(&manager);
    live.begin_rust_workspace_reload(SystemTime::now());
    live.set_rust_analyzer_quiescent(true);
    let key = key(root.path());
    manager.starting.insert(
        key.clone(),
        StartReservation {
            instance: live.instance_id(),
            deferred_events: Vec::new(),
            clients_generation: manager.clients_generation,
            signal: Arc::new(StartSignal::default()),
        },
    );
    std::fs::write(root.path().join("release"), "release").unwrap();
    let begin = recv(&manager);
    let end = recv(&manager);
    assert_eq!(begin.rust_check_begin.unwrap().ordinal, 2);
    manager.handle_received_event(&begin);
    manager.handle_received_event(&end);
    assert_eq!(manager.starting[&key].deferred_events.len(), 2);
    manager.clients.insert(key.clone(), live);
    manager.release_start_reservation(&key);
    assert!(
        manager.clients[&key].rust_check_completed_current(Instant::now(), Duration::ZERO),
        "replay reconstructed a drain ordinal instead of the reader's post-reload ordinal"
    );
}

#[test]
fn stale_event_filter_preserves_the_raw_drain_budget() {
    let root = tempfile::tempdir().unwrap();
    let mut manager = LspManager::new();
    let old = spawn(
        &manager,
        root.path(),
        &format!(
            "printf '%s' '{}'; exec sleep 60",
            framed(&[
                progress("begin", "rust-analyzer/flycheck/0"),
                progress("end", "rust-analyzer/flycheck/0"),
                status()
            ])
        ),
    );
    let events = [recv(&manager), recv(&manager), recv(&manager)];
    let live = spawn(&manager, root.path(), "exec sleep 60");
    manager.clients.insert(key(root.path()), live);
    for event in events {
        manager.event_tx.send(event).unwrap();
    }
    let drained = manager.drain_events_bounded(1);
    assert!(
        drained.events.is_empty(),
        "stale events leaked to public consumers"
    );
    assert_eq!(
        manager.event_rx.len(),
        2,
        "filtering bypassed the raw event budget"
    );
    assert!(drained.has_more);
    drop(old);
}

#[test]
fn completed_check_uses_the_exact_reader_begin_timestamp() {
    use super::super::completed_rust_check::{CompletedRustCheck, Runtime};
    let root = tempfile::tempdir().unwrap();
    let mut manager = LspManager::new();
    let mut live = spawn(
        &manager,
        root.path(),
        &format!(
            "printf '%s' '{}'; exec sleep 60",
            framed(&[
                progress("begin", "rust-analyzer/flycheck/0"),
                progress("begin", "rust-analyzer/flycheck/1")
            ])
        ),
    );
    let _earlier_begin_not_applied = recv(&manager);
    let event = recv(&manager);
    let reader_time = event.rust_check_begin.unwrap().began;
    live.completed_rust_check = CompletedRustCheck::in_memory(
        root.path(),
        root.path(),
        Runtime {
            binary: root.path().join("missing-analyzer"),
            args: Vec::new(),
            env: HashMap::new(),
            options: None,
            launch_env: None,
            validation_delay: Duration::ZERO,
        },
    );
    assert!(live.completed_rust_check.is_some());
    let key = key(root.path());
    manager.clients.insert(key.clone(), live);
    manager.handle_received_event(&event);
    assert_eq!(
        manager.clients[&key]
            .completed_rust_check
            .as_ref()
            .unwrap()
            .began_for_test(),
        Some(reader_time),
        "completed-check capture replaced receipt metadata with drain time"
    );
}
