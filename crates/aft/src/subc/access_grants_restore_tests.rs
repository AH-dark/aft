use super::*;

// Wire fixtures update process-wide write counters. Give each its own process
// so other tests' health-report size budgets do not depend on fixture ordering.
fn in_isolated_process(test: &str) -> bool {
    if std::env::var("AFT_LAUNCH_RESTORE_TEST").as_deref() == Ok(test) {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(format!("subc::access_grants::tests::slow_restore::{test}"))
        .args(["--exact", "--nocapture"])
        .env("AFT_LAUNCH_RESTORE_TEST", test)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{test}:\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "child must run one test: {stdout}"
    );
    false
}

struct WireRestore {
    daemon: tokio::io::DuplexStream,
    module: Option<std::thread::JoinHandle<Result<ModuleLoopExit, SubcError>>>,
    ctx: Arc<AppContext>,
    revived: RouteIdentity,
    target: PathBuf,
    _dir: tempfile::TempDir,
}

impl WireRestore {
    async fn new() -> Self {
        let (dir, ctx, head, _) = default_fixture();
        let worker = identity(&head.project_root, "worker", "worker", Some("head"));
        register(&worker, &ctx).unwrap();
        let target = policy_path(&dir.path().join("saved-launch"));
        std::fs::create_dir_all(&target).unwrap();
        remote_policy::catalog(
            catalog_request(json!([
                {"path":head.project_root.join("healthy-launch"),"mode":"read"},
                {"path":target,"mode":"write"}
            ])),
            &worker,
            &ctx,
        )
        .unwrap();
        let revived = at_epoch(&worker, 8);
        let ctx = Arc::new(ctx);
        let executor = Arc::new(Executor::new());
        executor.register_actor(revived.root.clone(), ctx.clone());
        let (daemon, module) = tokio::io::duplex(64 * 1024);
        let app = ctx.app().clone();
        let path = dir.path().to_path_buf();
        let module = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (read, write) = tokio::io::split(module);
            runtime.block_on(run_module_loop(
                read,
                write,
                &path.join("absent-connection.json"),
                app,
                executor,
                |request, _| Response::success(request.id, json!({"executed":true})),
                Some(path.join("absent-config.json")),
                false,
                usize::MAX,
                None,
                &path.join("store"),
                QuiescedLsp::ReapInBackground,
                None,
                None,
            ))
        });
        let mut fixture = Self {
            daemon,
            module: Some(module),
            ctx,
            revived,
            target,
            _dir: dir,
        };
        let hello = read_frame(&mut fixture.daemon).await.unwrap().unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        fixture
            .send(FrameType::HelloAck, 0, HELLO_CORR, json!({}))
            .await;
        for (channel, scope) in [(41, fixture.revived.scope.clone()), (42, None)] {
            let bind = ModuleControlRequest::RouteBind {
                route_channel: channel,
                epoch: 1,
                target: RouteTarget::ToolProvider {
                    module_id: "aft".into(),
                },
                identity: subc_protocol::BindIdentity::new(
                    &fixture.revived.project_root,
                    "runner",
                    if channel == 41 { "worker" } else { "healthy" },
                ),
                principal: Some(Principal::Reserved {
                    module_id: "broca".into(),
                }),
                consumer_capabilities: None,
                admission_facts: Default::default(),
                scope,
                role_versions: None,
                call_mode: None,
            };
            fixture
                .send(
                    FrameType::Request,
                    0,
                    channel as u64,
                    serde_json::to_value(bind).unwrap(),
                )
                .await;
            let reply = fixture
                .reply(channel as u64, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(reply.header.ty, FrameType::Response, "{reply:?}");
        }
        fixture
    }

    async fn send(&mut self, ty: FrameType, channel: u16, corr: u64, body: Value) {
        let frame = Frame::build(
            ty,
            control_flags(),
            channel,
            u32::from(channel != 0),
            corr,
            if matches!(ty, FrameType::Cancel | FrameType::Goodbye) {
                Vec::new()
            } else {
                serde_json::to_vec(&body).unwrap()
            },
        )
        .unwrap();
        write_frame(&mut self.daemon, &frame).await.unwrap();
    }

    async fn call(&mut self, corr: u64) {
        self.send(
            FrameType::Request,
            41,
            corr,
            json!({
                "name":"read", "preset":"worker", "arguments":{"path":"probe.txt"},
                "deadline_ms":crate::bash_background::persistence::unix_millis() + 700,
            }),
        )
        .await;
    }

    async fn reply(&mut self, corr: u64, budget: Duration) -> Option<Frame> {
        tokio::time::timeout(budget, async {
            loop {
                let frame = read_frame(&mut self.daemon).await.unwrap().unwrap();
                if frame.header.corr == corr {
                    return frame;
                }
            }
        })
        .await
        .ok()
    }

    fn assert_unrestored(&self) {
        let key = scope_key(&self.revived);
        assert!(!self.ctx.launch_grants_materialized(&key));
        let db = self.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        assert!(store::list(&conn, &key).unwrap().is_empty());
        let markers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM access_materialized WHERE epoch='8'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            markers, 0,
            "failed restoration must leave no materialization marker"
        );
    }

    async fn stop(mut self) {
        self.send(FrameType::Goodbye, 0, 999, json!({})).await;
        self.module.take().unwrap().join().unwrap().unwrap();
    }
}

#[tokio::test]
async fn grants_stalled_restore_keeps_other_routes_and_cancel_live() {
    if !in_isolated_process("grants_stalled_restore_keeps_other_routes_and_cancel_live") {
        return;
    }
    let mut wire = WireRestore::new().await;
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    wire.call(100).await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    wire.send(
        FrameType::Request,
        42,
        101,
        json!({"name":"read", "arguments":{}}),
    )
    .await;
    let healthy = wire.reply(101, Duration::from_millis(300)).await;
    wire.send(FrameType::Cancel, 41, 100, json!({})).await;
    let cancelled = wire.reply(100, Duration::from_millis(300)).await;
    drop(stall);
    wire.stop().await;
    assert!(
        healthy.is_some(),
        "another route stopped answering while launch restoration stalled"
    );
    let cancelled = cancelled.expect("Cancel stopped answering while launch restoration stalled");
    let body: Value = serde_json::from_slice(&cancelled.body).unwrap();
    assert_eq!(body["code"], "cancelled");
}

#[tokio::test]
async fn grants_stalled_restore_keeps_drain_live() {
    if !in_isolated_process("grants_stalled_restore_keeps_drain_live") {
        return;
    }
    let mut wire = WireRestore::new().await;
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    wire.call(100).await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    wire.send(
        FrameType::Push,
        0,
        103,
        serde_json::to_value(ModuleControlCommand::Draining {
            reason: subc_protocol::RouteCloseReason::Reload,
            deadline_ms: 1_000,
        })
        .unwrap(),
    )
    .await;
    let drained = wire.reply(100, Duration::from_millis(300)).await;
    drop(stall);
    wire.stop().await;
    let drained = drained.expect("drain stopped answering while launch restoration stalled");
    let body: Value = serde_json::from_slice(&drained.body).unwrap();
    assert_eq!(body["code"], "module_reloading");
}

#[test]
fn grants_stalled_restore_worker_exits_before_path_recovers() {
    if !in_isolated_process("grants_stalled_restore_worker_exits_before_path_recovers") {
        return;
    }
    let (_dir, ctx, head, worker) = default_fixture();
    let target = head.project_root.join("saved-launch");
    remote_policy::catalog(
        catalog_request(json!([
            {"path":head.project_root.join("healthy-launch"),"mode":"read"},
            {"path":target,"mode":"write"}
        ])),
        &worker,
        &ctx,
    )
    .unwrap();
    let revived = at_epoch(&worker, 8);
    let ctx = Arc::new(ctx);
    let stall = crate::bounded_io::stall_running_helper(&target);
    let (tx, rx) = std::sync::mpsc::channel();
    let worker_ctx = ctx.clone();
    let worker_identity = revived.clone();
    let progress = Arc::new(StdMutex::new(target.clone()));
    let thread = std::thread::spawn(move || {
        let result = restore_launch_before(
            &worker_identity,
            &worker_ctx,
            "worker",
            Instant::now() + Duration::from_millis(500),
            progress,
        );
        tx.send(result).unwrap();
    });
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let result = rx.recv_timeout(Duration::from_secs(2));
    drop(stall);
    thread.join().unwrap();
    let refused = result
        .expect("restoration worker outlived its filesystem deadline")
        .unwrap_err();
    assert_eq!(refused.code, "grants_unavailable");
    let detail = refused.detail.unwrap();
    assert_eq!(detail["path"], json!(target));
    assert_eq!(detail["retryable"], true);
    let db = ctx.db().unwrap();
    assert!(store::list(&db.lock().unwrap(), &scope_key(&revived))
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn grants_stalled_restore_refuses_within_deadline_without_partial_grants() {
    if !in_isolated_process("grants_stalled_restore_refuses_within_deadline_without_partial_grants")
    {
        return;
    }
    let mut wire = WireRestore::new().await;
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    wire.call(100).await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let reply = wire.reply(100, Duration::from_secs(2)).await;
    wire.assert_unrestored();
    drop(stall);
    let target = wire.target.clone();
    wire.stop().await;
    let reply = reply.expect("stalled launch restoration did not refuse within its deadline");
    assert_eq!(reply.header.ty, FrameType::Error);
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["code"], "grants_unavailable");
    assert_eq!(body["detail"]["retryable"], true);
    assert_eq!(body["detail"]["path"], json!(target));
}

#[tokio::test]
async fn grants_stalled_restore_retries_after_path_recovers() {
    if !in_isolated_process("grants_stalled_restore_retries_after_path_recovers") {
        return;
    }
    let mut wire = WireRestore::new().await;
    let stall = crate::bounded_io::stall_running_helper(&wire.target);
    wire.call(100).await;
    stall.reached.recv_timeout(Duration::from_secs(5)).unwrap();
    let refused = wire.reply(100, Duration::from_secs(2)).await;
    drop(stall);
    if refused.is_some() {
        wire.assert_unrestored();
    }
    wire.call(102).await;
    let recovered = wire.reply(102, Duration::from_secs(3)).await.unwrap();
    let body: Value = serde_json::from_slice(&recovered.body).unwrap();
    assert_eq!(body["structuredContent"]["success"], true, "{body}");
    let db = wire.ctx.db().unwrap();
    let grants = store::list(&db.lock().unwrap(), &scope_key(&wire.revived)).unwrap();
    assert_eq!(grants.len(), 2);
    assert!(grants.contains(&AccessGrant {
        path: wire.target.clone(),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Write,
    }));
    assert_eq!(
        wire.ctx.launch_restore_attempts(),
        2,
        "a failed restoration must not be memoized"
    );
    wire.stop().await;
    let refused =
        refused.expect("the stalled attempt never refused, so recovery was not a fresh admission");
    assert_eq!(refused.header.ty, FrameType::Error);
    let body: Value = serde_json::from_slice(&refused.body).unwrap();
    assert_eq!(body["code"], "grants_unavailable");
    assert_eq!(body["detail"]["retryable"], true);
}
