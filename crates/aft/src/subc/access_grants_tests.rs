use super::*;

fn identity(root: &Path, name: &str, kind: &str, parent: Option<&str>) -> RouteIdentity {
    let stamp = serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":name,"scope_epoch":7,"kind":kind,"parent":parent.map(|name|json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":name,"scope_epoch":7})),"owner_authorized":true})).unwrap();
    RouteIdentity(Arc::new(RouteIdentityData {
        root: ProjectRootId::from_path(root).unwrap(),
        project_root: root.into(),
        harness: "runner".into(),
        session: name.into(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::RouteBind {
            trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
            route_channel: 41,
            route_epoch: 1,
            project_root: root.into(),
            harness: "runner".into(),
            session_id: name.into(),
            principal_id: Some("reserved:broca".into()),
        },
        consumer_elicitation_capable: false,
        disabled_tools: Arc::new(vec![]),
        scope: Some(stamp),
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        run_scope: OnceLock::new(),
        made_tool_call: AtomicBool::new(false),
    }))
}
fn context(root: &Path, storage: &Path) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.into());
    config.storage_dir = Some(storage.into());
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&storage.join("aft.db")).unwrap(),
    )));
    ctx
}
fn fixture() -> (tempfile::TempDir, AppContext, RouteIdentity, RouteIdentity) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("head");
    let work = dir.path().join("worker");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let ctx = context(&root, &dir.path().join("store"));
    let head = identity(&root, "head", "head", None);
    let worker = identity(&work, "worker", "worker", Some("head"));
    register(&head, &ctx).unwrap();
    register(&worker, &ctx).unwrap();
    (dir, ctx, head, worker)
}
fn catalog_request(raw: Value) -> Value {
    json!({"preset":"worker","params":{"allow_paths":raw}})
}

#[test]
fn grants_allow_paths_preflight_fetch_revoke_restart_reclaim() {
    let (dir, ctx, head, worker) = fixture();
    let target = head.project_root.join("shared");
    let raw = json!([{"path":target,"mode":"read"},{"path":target,"mode":"write"}]);
    remote_policy::catalog(catalog_request(raw.clone()), &head, &ctx).unwrap();
    assert!(
        operation(json!({}), "grants.list", &head, &ctx).unwrap()["grants"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        operation(json!({"scope":"worker"}), "grants.list", &head, &ctx).unwrap()["grants"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    remote_policy::catalog(catalog_request(raw.clone()), &worker, &ctx).unwrap();
    let granted = operation(json!({"scope":"worker"}), "grants.list", &head, &ctx).unwrap();
    assert_eq!(granted["grants"].as_array().unwrap().len(), 1);
    assert_eq!(granted["grants"][0]["mode"], "write");
    operation(
        json!({"scope":"worker","path":target}),
        "grants.revoke",
        &head,
        &ctx,
    )
    .unwrap();
    let storage = ctx.storage_dir();
    drop(ctx);
    std::fs::remove_dir(&worker.project_root).unwrap();
    std::fs::create_dir(&worker.project_root).unwrap();
    let restarted = context(&worker.project_root, &storage);
    remote_policy::catalog(catalog_request(raw), &worker, &restarted).unwrap();
    remote_policy::catalog(json!({"preset":"worker"}), &worker, &restarted).unwrap();
    assert!(
        operation(json!({}), "grants.list", &worker, &restarted).unwrap()["grants"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let db = restarted.db().unwrap();
    let conn = db.lock().unwrap();
    let policy = policy_for_scope(&conn, &scope_key(&worker), &restarted)
        .unwrap()
        .unwrap();
    assert_eq!(
        policy.decide(AccessOperation::Write, &target, false),
        AccessDecision::Ask
    );
    assert!(dir.path().exists());
}

#[test]
fn grants_allow_paths_atomic_validation_and_named_refusals() {
    let (_dir, ctx, head, worker) = fixture();
    for (bad, reason) in [
        (json!({"path":"relative","mode":"read"}), "not_absolute"),
        (json!({"path":"/tmp/../outside","mode":"read"}), "malformed"),
        (json!({"path":"/tmp/*","mode":"read"}), "malformed"),
        (json!({"path":"/tmp/outside"}), "malformed"),
        (json!({"path":"/tmp/outside","mode":"execute"}), "malformed"),
        (
            json!({"path":worker.project_root.join("outside"),"mode":"read"}),
            "head_not_held",
        ),
    ] {
        let raw = json!([{"path":head.project_root.join("ok"),"mode":"read"},bad]);
        for caller in [&head, &worker] {
            let error =
                remote_policy::catalog(catalog_request(raw.clone()), caller, &ctx).unwrap_err();
            assert_eq!(error.code, "plan_param_refused");
            assert_eq!(error.detail.as_ref().unwrap()["index"], 1);
            assert_eq!(error.detail.as_ref().unwrap()["param"], "allow_paths");
            assert_eq!(error.detail.as_ref().unwrap()["reason"], reason);
        }
        assert!(
            operation(json!({}), "grants.list", &worker, &ctx).unwrap()["grants"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let conn = ctx.db().unwrap();
        assert!(store::launch_params(
            &conn.lock().unwrap(),
            &remote_policy::key(&worker, Some("worker")).unwrap()
        )
        .unwrap()
        .is_none());
    }
    assert_eq!(
        validate(&json!({}), None).unwrap_err().detail.unwrap()["reason"],
        "malformed"
    );
    let oversized = Value::Array(
        (0..65)
            .map(|_| json!({"path":head.project_root,"mode":"read"}))
            .collect(),
    );
    assert_eq!(
        validate(&oversized, None).unwrap_err().detail.unwrap()["index"],
        64
    );
}

#[test]
fn grants_catalog_refetch_rechecks_head_grant_and_floor() {
    let (_dir, ctx, head, worker) = fixture();
    let target = worker.project_root.join("allowed");
    let grant = AccessGrant {
        path: target.clone(),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    };
    {
        let db = ctx.db().unwrap();
        store::insert(&db.lock().unwrap(), &scope_key(&head), &grant).unwrap();
    }
    let raw = json!([{"path":target,"mode":"read"}]);
    remote_policy::catalog(catalog_request(raw), &worker, &ctx).unwrap();
    operation(json!({"path":target}), "grants.revoke", &head, &ctx).unwrap();
    let error = remote_policy::catalog(json!({"preset":"worker"}), &worker, &ctx).unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "head_not_held");
    ctx.update_config(|config| {
        config
            .sandbox
            .read_deny
            .push(head.project_root.join("secret"))
    });
    register(&head, &ctx).unwrap();
    let error = remote_policy::catalog(
        catalog_request(json!([{"path":head.project_root,"mode":"write"}])),
        &head,
        &ctx,
    )
    .unwrap_err();
    assert_eq!(error.detail.unwrap()["reason"], "denied_floor");
}

#[test]
fn grants_operations_refuse_other_scopes_and_end_drops_grants() {
    let (_dir, ctx, head, worker) = fixture();
    let other = identity(&worker.project_root, "other", "worker", Some("head"));
    register(&other, &ctx).unwrap();
    for op in ["grants.list", "grants.revoke"] {
        for target in ["head", "other", "unknown"] {
            assert_eq!(
                operation(
                    json!({"scope":target,"path":"/tmp/outside"}),
                    op,
                    &worker,
                    &ctx
                )
                .unwrap_err()
                .code,
                "grants_scope_refused"
            );
        }
    }
    assert!(operation(json!({"scope":"worker"}), "grants.list", &head, &ctx).is_ok());
    remote_policy::catalog(
        catalog_request(json!([{"path":head.project_root.join("shared"),"mode":"read"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    {
        let db = ctx.db().unwrap();
        store::end_scope_grants(&db.lock().unwrap(), &scope_key(&worker)).unwrap();
    }
    assert_eq!(
        operation(json!({}), "grants.list", &worker, &ctx)
            .unwrap_err()
            .code,
        "grants_scope_refused"
    );
    let db = ctx.db().unwrap();
    assert!(store::list(&db.lock().unwrap(), &scope_key(&worker))
        .unwrap()
        .is_empty());
}

#[test]
fn grants_policy_reads_revocation_on_the_next_check() {
    let (_dir, ctx, head, worker) = fixture();
    ctx.update_config(|config| config.restrict_to_project_root = true);
    let path = head.project_root.join("shared");
    std::fs::write(&path, "protected").unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":path,"mode":"write"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    ctx.update_config(|config| config.project_root = Some(worker.project_root.clone()));
    crate::access_policy::with_scope(Some(scope_key(&worker)), || {
        assert_eq!(
            ctx.access_policy("check")
                .unwrap()
                .decide(AccessOperation::Read, &path, false),
            AccessDecision::Allow
        );
        assert!(ctx.validate_read_path("check", "worker", &path).is_ok());
        operation(json!({"path":path}), "grants.revoke", &worker, &ctx).unwrap();
        assert_eq!(
            ctx.access_policy("check")
                .unwrap()
                .decide(AccessOperation::Read, &path, false),
            AccessDecision::Deny(AccessDenial::OutsideRoots)
        );
        assert!(ctx.validate_read_path("check", "worker", &path).is_err());
    });
}

#[test]
fn grants_ordinary_detach_and_reclamation_keep_standing_capabilities() {
    let (dir, ctx, _head, worker) = fixture();
    let grant = AccessGrant {
        path: dir.path().join("shared"),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    };
    {
        let db = ctx.db().unwrap();
        store::insert(&db.lock().unwrap(), &scope_key(&worker), &grant).unwrap();
    }
    let storage = ctx.storage_dir();
    let scope = scope_key(&worker);
    drop(worker);
    drop(ctx);
    std::fs::remove_dir(dir.path().join("worker")).unwrap();
    std::fs::create_dir(dir.path().join("worker")).unwrap();
    let rebound = context(&dir.path().join("worker"), &storage);
    let db = rebound.db().unwrap();
    assert_eq!(
        store::list(&db.lock().unwrap(), &scope).unwrap(),
        vec![grant]
    );
}

#[test]
fn grants_empty_launch_is_materialized_and_cannot_be_widened_on_refetch() {
    let (_dir, ctx, head, worker) = fixture();
    remote_policy::catalog(json!({"preset":"worker"}), &worker, &ctx).unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":head.project_root.join("shared"),"mode":"write"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    assert!(
        operation(json!({}), "grants.list", &worker, &ctx).unwrap()["grants"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn grants_deferred_delete_carries_scope() {
    let (_dir, ctx, head, worker) = fixture();
    ctx.backup().lock().set_policy(crate::backup::BackupPolicy {
        enabled: false,
        ..Default::default()
    });
    let target = head.project_root.join("shared");
    std::fs::write(&target, "protected").unwrap();
    remote_policy::catalog(
        catalog_request(json!([{"path":target,"mode":"write"}])),
        &worker,
        &ctx,
    )
    .unwrap();
    ctx.update_config(|config| {
        config.project_root = Some(worker.project_root.clone());
        config.restrict_to_project_root = true;
    });
    let ctx = Arc::new(ctx);
    let request: RawRequest = serde_json::from_value(
        json!({"id":"deferred-grant","command":"delete_file","session_id":"worker","file":target}),
    )
    .unwrap();
    let outcome = crate::access_policy::with_scope(Some(scope_key(&worker)), || {
        crate::commands::delete_file::handle_delete_deferred_with_restriction(
            &request,
            Arc::clone(&ctx),
            false,
        )
    });
    let crate::response_finalize::DispatchOutcome::Deferred(mut pending) = outcome else {
        panic!("external temp-file delete must defer");
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let response = loop {
        if let Some(response) = (pending.poll)(&ctx) {
            break response;
        }
        assert!(
            Instant::now() < deadline,
            "deferred delete did not terminate"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(response.success, "{}", response.data);
    assert!(!target.exists());
}

#[test]
fn grants_exact_file_authority_reaches_preserved_final_write_checks() {
    let (_dir, ctx, head, worker) = fixture();
    let path = head.project_root.join("file");
    std::fs::write(&path, "protected").unwrap();
    let grant = AccessGrant {
        path: path.clone(),
        kind: GrantKind::File,
        operation: AccessOperation::Write,
    };
    {
        let db = ctx.db().unwrap();
        store::insert(&db.lock().unwrap(), &scope_key(&worker), &grant).unwrap();
    }
    ctx.update_config(|config| {
        config.project_root = Some(worker.project_root.clone());
        config.restrict_to_project_root = true;
    });
    crate::access_policy::with_scope(Some(scope_key(&worker)), || {
        assert!(ctx.validate_write_location("file-check", &path).is_ok());
        assert!(ctx
            .validate_write_location("file-check", &path.with_file_name("sibling"))
            .is_err());
        operation(json!({"path":path}), "grants.revoke", &worker, &ctx).unwrap();
        assert!(ctx.validate_write_location("file-check", &path).is_err());
    });
}

#[test]
fn grants_codemode_run_uses_agent_policy_and_revocation_without_copying() {
    let (_dir, ctx, head, worker) = fixture();
    let path = head.project_root.join("shared");
    std::fs::write(&path, "protected").unwrap();
    let grant = AccessGrant {
        path: path.clone(),
        kind: GrantKind::File,
        operation: AccessOperation::Write,
    };
    let mut run = identity(&worker.project_root, "run", "ephemeral", Some("worker"));
    Arc::get_mut(&mut run.0).unwrap().call_mode = subc_protocol::call_mode::CallMode::Codemode;
    register(&run, &ctx).unwrap();
    ctx.update_config(|config| {
        config.project_root = Some(worker.project_root.clone());
        config.restrict_to_project_root = true;
    });
    let worker_key = scope_key(&worker);
    let run_key = scope_key(&run);
    assert_ne!(run_key, worker_key);
    let db = ctx.db().unwrap();
    store::insert(&db.lock().unwrap(), &worker_key, &grant).unwrap();
    crate::access_policy::with_scope(Some(policy_scope_key(&run)), || {
        assert!(ctx.validate_read_path("run-check", "worker", &path).is_ok());
        operation(json!({"path":path}), "grants.revoke", &worker, &ctx).unwrap();
        assert!(ctx
            .validate_read_path("run-check", "worker", &path)
            .is_err());
    });
    let conn = db.lock().unwrap();
    assert!(store::list(&conn, &run_key).unwrap().is_empty());
    store::insert(&conn, &worker_key, &grant).unwrap();
    store::end_scope_grants(&conn, &run_key).unwrap();
    assert_eq!(store::list(&conn, &worker_key).unwrap(), vec![grant]);
}
