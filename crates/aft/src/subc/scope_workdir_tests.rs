use super::*;
use subc_protocol::scope::{ScopeKind, ScopeStamp};

thread_local! {
    static COMPARISON_OBSERVER: std::cell::RefCell<Option<oneshot::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn take_comparison_observer() -> Option<oneshot::Sender<()>> {
    COMPARISON_OBSERVER.with(|observer| observer.borrow_mut().take())
}

fn stamp() -> ScopeStamp {
    ScopeStamp::new(Principal::Direct, "session-scope", 4, ScopeKind::Head)
}

// Exercise admission and completion, not a second implementation of root selection.
fn bind(root: &Path, scope: Option<ScopeStamp>) -> (Vec<String>, Vec<u8>) {
    bind_observed(root, scope, |_| {})
}

fn bind_observed(
    root: &Path,
    scope: Option<ScopeStamp>,
    after_ack: impl FnOnce(&mut Option<oneshot::Receiver<()>>),
) -> (Vec<String>, Vec<u8>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (reply, lines) = crate::logging::capture_log_lines(|| {
        runtime.block_on(async {
            let app = App::default_shared();
            let executor = Arc::new(Executor::new());
            let metrics = Arc::new(DispatchPathMetrics::new());
            let (writer_tx, mut writer_rx) = mpsc::channel(8);
            let (completion_tx, mut completion_rx) = mpsc::channel(8);
            let (lossy_tx, _lossy_rx) = mpsc::channel(8);
            let (reliable_tx, _reliable_rx) = mpsc::unbounded_channel();
            let senders = PushSenders {
                lossy_tx,
                reliable_tx,
                lossy_overflow: Arc::new(push::LossyOverflow::default()),
                lossy_seq: Arc::new(AtomicU64::new(0)),
                fleet_status_client: FleetStatusClient::channel(1).0,
            };
            let request = ModuleControlRequest::RouteBind {
                route_channel: 7,
                epoch: 1,
                target: RouteTarget::ToolProvider {
                    module_id: "aft".into(),
                },
                identity: subc_protocol::BindIdentity::new(root, "opencode", "folder-session"),
                principal: Some(Principal::Direct),
                consumer_capabilities: None,
                admission_facts: Default::default(),
                scope: scope.clone(),
                role_versions: None,
                call_mode: None,
            };
            let frame = Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                91,
                serde_json::to_vec(&request).unwrap(),
            )
            .unwrap();
            let mut pending = HashMap::new();
            let mut installed = HashMap::new();
            let mut routes = HashMap::new();
            let mut observed = scope
                .as_ref()
                .and_then(|stamp| stamp.workdir.as_ref())
                .map(|_| {
                    let (tx, rx) = oneshot::channel();
                    COMPARISON_OBSERVER.with(|observer| *observer.borrow_mut() = Some(tx));
                    rx
                });
            handle_control_request(
                &writer_tx,
                &frame,
                &app,
                &executor,
                &Arc::new(standing::StandingActor::new(app.clone(), executor.clone())),
                &mut HashMap::new(),
                &mut pending,
                &mut installed,
                &mut routes,
                &mut HashSet::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &Arc::new(StdMutex::new(HashMap::new())),
                &mut PendingSubcResponses::default(),
                &mut RetryBuffer::new(),
                &mut HashMap::new(),
                &Arc::new(Notify::new()),
                &completion_tx,
                &metrics,
                None,
                &HealthRollupCache::new(),
                &senders,
                |request, _| Response::success(request.id, request.params),
                Some(&root.join("absent-user-config.json")),
                usize::MAX,
                None,
            )
            .await
            .unwrap();
            let completion = tokio::time::timeout(Duration::from_secs(5), completion_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let expected_root = ProjectRootId::from_path(root).unwrap();
            assert!(completion.configure_response.success);
            assert_eq!(completion.bind_root_id, expected_root);
            assert_eq!(completion.identity.root, expected_root);
            assert_eq!(completion.identity.project_root, root);
            assert_eq!(completion.identity.scope, scope);
            assert_eq!(completion.identity.trust, BindTrust::FirstParty);
            match &completion.identity.spawn_principal {
                AuthenticatedPrincipal::RouteBind {
                    project_root,
                    session_id,
                    ..
                } => {
                    assert_eq!(project_root, root);
                    assert_eq!(session_id, "folder-session");
                }
                other => panic!("unexpected spawn principal: {other:?}"),
            }
            assert_eq!(
                completion.configure_response.data["project_root"],
                root.to_string_lossy().as_ref()
            );
            assert_eq!(executor.actor_count(), 1);
            assert_eq!(executor.actor_entries()[0].0, expected_root);
            handle_route_bind_completion(
                &writer_tx,
                completion,
                &mut routes,
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut pending,
                &mut installed,
                &executor,
                &Arc::new(Notify::new()),
                &metrics,
                None,
            )
            .await
            .unwrap();
            assert_eq!(routes[&route_key(7, 1)].root, expected_root);
            let ack = writer_rx.try_recv().expect("successful bind reply");
            assert_eq!(ack.header.ty, FrameType::Response);
            assert_eq!(
                (ack.header.channel, ack.header.epoch, ack.header.corr),
                (0, 0, 91)
            );
            after_ack(&mut observed);
            if let Some(observed) = observed {
                tokio::time::timeout(Duration::from_secs(8), observed)
                    .await
                    .unwrap()
                    .unwrap();
            }
            ack.body.clone()
        })
    });
    let lines = lines
        .into_iter()
        .filter(|line| line.contains("subc scope workdir"))
        .collect();
    (lines, reply)
}

#[cfg(unix)]
#[test]
fn scope_workdir_held_lookup_does_not_delay_bind_ack() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let workdir = root.join("held-workdir");
    let held = crate::bounded_io::stall_running_helper(&workdir);
    let started = Instant::now();
    let (lines, reply) = bind_observed(
        &root,
        Some(stamp().with_workdir(Some(workdir.to_string_lossy().into_owned()))),
        |observed| {
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "diagnostic delayed bind ACK by {:?}",
                started.elapsed()
            );
            held.reached
                .recv_timeout(Duration::from_secs(1))
                .expect("workdir lookup reached the held helper");
            assert!(
                matches!(
                    observed.as_mut().unwrap().try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ),
                "comparison finished while its lookup was held"
            );
            drop(held);
        },
    );
    assert_eq!(reply, br#"{"op":"route.bind"}"#);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("subc scope workdir differs:"),
        "{lines:?}"
    );
}

#[cfg(unix)]
#[test]
fn scope_workdir_expired_lookup_logs_skipped_without_guessing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let workdir = root.join("timed-out-workdir");
    let held = crate::bounded_io::stall_running_helper(&workdir);
    let (lines, reply) = bind_observed(
        &root,
        Some(stamp().with_workdir(Some(workdir.to_string_lossy().into_owned()))),
        |_| {
            held.reached
                .recv_timeout(Duration::from_secs(1))
                .expect("workdir lookup reached the held helper");
        },
    );
    drop(held);
    assert_eq!(reply, br#"{"op":"route.bind"}"#);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("subc scope workdir comparison skipped:"),
        "{lines:?}"
    );
    assert!(!lines[0].contains("workdir differs:"), "{lines:?}");
}

#[test]
fn scope_workdir_matching_is_silent_and_preserves_project_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (lines, _) = bind(
        &root,
        Some(
            stamp()
                .with_workdir(Some(root.join(".").to_string_lossy().into_owned()))
                .with_project_id(Some("project-match".into())),
        ),
    );
    assert!(lines.is_empty(), "{lines:?}");
    #[cfg(unix)]
    {
        let alias = root.join("folder-alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let (lines, _) = bind(
            &alias,
            Some(stamp().with_workdir(Some(root.to_string_lossy().into_owned()))),
        );
        assert!(lines.is_empty(), "{lines:?}");
    }
}

#[test]
fn scope_workdir_mismatch_logs_once_and_preserves_project_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let folder = root.join("missing-session-folder");
    let expected_root = ProjectRootId::from_path(&root).unwrap();
    let expected_folder = expected_root.as_path().join("missing-session-folder");
    let (lines, _) = bind(
        &root,
        Some(
            stamp()
                .with_workdir(Some(folder.to_string_lossy().into_owned()))
                .with_project_id(Some("project-mismatch".into())),
        ),
    );
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(
        line.starts_with("INFO subc scope workdir differs:"),
        "{line}"
    );
    for value in [
        expected_root.as_path().to_str().unwrap(),
        expected_folder.to_str().unwrap(),
        "folder-session",
        "project-mismatch",
    ] {
        assert!(line.contains(value), "missing {value}: {line}");
    }
}

#[test]
fn scope_workdir_old_stamp_is_silent_and_preserves_project_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let old_stamp: ScopeStamp = serde_json::from_str(
        r#"{"owner":{"kind":"direct"},"ref":"session-scope","scope_epoch":4,"kind":"head","owner_authorized":false}"#,
    ).unwrap();
    assert!(old_stamp.workdir.is_none());
    assert!(old_stamp.project_id.is_none());
    assert_eq!(serde_json::to_vec(&old_stamp).unwrap(), br#"{"owner":{"kind":"direct"},"ref":"session-scope","scope_epoch":4,"kind":"head","owner_authorized":false}"#);
    let (lines, old_reply) = bind(&root, Some(old_stamp));
    let (unscoped_lines, unscoped_reply) = bind(&root, None);
    assert!(lines.is_empty(), "{lines:?}");
    assert!(unscoped_lines.is_empty(), "{unscoped_lines:?}");
    assert_eq!(old_reply, unscoped_reply);
}

#[test]
fn scope_workdir_bind_reply_is_byte_for_byte_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for scope in [
        None,
        Some(stamp()),
        Some(stamp().with_workdir(Some(root.to_string_lossy().into_owned()))),
        Some(
            stamp()
                .with_workdir(Some(root.join("other").to_string_lossy().into_owned()))
                .with_project_id(Some("project-other".into())),
        ),
    ] {
        let (_, reply) = bind(&root, scope);
        assert_eq!(reply, br#"{"op":"route.bind"}"#);
    }
}
