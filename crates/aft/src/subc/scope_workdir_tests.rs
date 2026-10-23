use super::*;
use subc_protocol::scope::{ScopeKind, ScopeStamp};

fn stamp() -> ScopeStamp {
    ScopeStamp::new(Principal::Direct, "session-scope", 4, ScopeKind::Head)
}

// Exercise admission and completion, not a second implementation of root selection.
fn bind(root: &Path, scope: Option<ScopeStamp>) -> (Vec<String>, Vec<u8>) {
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
            ack.body.clone()
        })
    });
    let lines = lines
        .into_iter()
        .filter(|line| line.contains("subc scope workdir"))
        .collect();
    (lines, reply)
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
        root.to_str().unwrap(),
        folder.to_str().unwrap(),
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
