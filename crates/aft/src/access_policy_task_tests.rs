use super::*;
use crate::bash_background::persistence::{
    create_task_layout, write_task, PersistedTask, TaskPaths,
};
use crate::bash_background::BgTaskStatus;
use crate::db::access_grants::{self as scopes, ScopeKey};
use std::sync::{Arc, Mutex};

struct Fixture {
    _dir: tempfile::TempDir,
    ctx: AppContext,
    head: ScopeKey,
    worker: TaskPaths,
}

fn stamp(
    name: &str,
    kind: &str,
    epoch: u64,
    parent: Option<&str>,
) -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
        "ref": name, "kind": kind, "scope_epoch": epoch, "owner_authorized": true,
        "parent": parent.map(|name| json!({
            "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
            "ref": name, "scope_epoch": epoch
        }))
    }))
    .unwrap()
}

fn record(
    ctx: &AppContext,
    session: &str,
    id: &str,
    scope: subc_protocol::scope::ScopeStamp,
) -> TaskPaths {
    let config = ctx.config();
    let root = config.project_root.clone().unwrap();
    let paths = create_task_layout(config.storage_dir.as_ref().unwrap(), session, id)
        .unwrap()
        .paths;
    let mut metadata = PersistedTask::starting(
        id.into(),
        session.into(),
        "printf worker-output".into(),
        root.clone(),
        Some(root),
        None,
        false,
        false,
    );
    metadata.scope = Some(scope);
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    metadata.completion_delivered = true;
    write_task(&paths.json, &metadata).unwrap();
    std::fs::write(&paths.command, "printf worker-output\n").unwrap();
    std::fs::write(&paths.environment, "SECRET_CREDENTIAL=hidden-environment\n").unwrap();
    std::fs::write(&paths.stdout, "worker-output\n").unwrap();
    std::fs::write(&paths.stderr, "").unwrap();
    std::fs::write(&paths.exit, "0\n").unwrap();
    paths
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let storage = dir.path().join("store");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    let ctx = AppContext::new(
        Box::new(StubProvider),
        Config {
            project_root: Some(root),
            storage_dir: Some(storage.clone()),
            restrict_to_project_root: false,
            ..Config::default()
        },
    );
    let db = Arc::new(Mutex::new(
        crate::db::open(&storage.join("aft.db")).unwrap(),
    ));
    ctx.set_db(Arc::clone(&db));
    ctx.bash_background().set_db_pool(db);
    let head = ScopeKey::from_stamp(&stamp("head", "head", 9, None));
    {
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        scopes::register_scope(&conn, &head, None, &ctx.config()).unwrap();
        scopes::register_agent(&conn, &head, "head-agent", true).unwrap();
    }
    let worker = record(
        &ctx,
        "worker-session",
        "bash-0123456789abcdef",
        stamp("worker", "worker", 1, Some("head")),
    );
    Fixture {
        _dir: dir,
        ctx,
        head,
        worker,
    }
}

fn read_as(ctx: &AppContext, session: &str, path: &Path) -> crate::protocol::Response {
    let mut req = request("read", json!({"file": path}));
    req.session_id = Some(session.into());
    crate::commands::read::handle_read(&req, ctx)
}

fn assert_floor(response: crate::protocol::Response, path: &Path) {
    assert!(!response.success, "{:?}", response.data);
    assert_eq!(response.data["code"], "denied_floor", "{:?}", response.data);
    assert_eq!(
        response.data["message"],
        format!(
            "path '{}' is denied by the access policy floor",
            path.display()
        )
    );
}

#[test]
fn access_policy_head_reads_worker_task_records() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        for (path, expected) in [
            (&f.worker.command, "printf worker-output"),
            (&f.worker.json, "worker-session"),
            (&f.worker.stdout, "worker-output"),
            (&f.worker.exit, "0"),
        ] {
            let response = read_as(&f.ctx, "head-session", path);
            assert!(response.success, "{}: {:?}", path.display(), response.data);
            assert!(
                response.data.to_string().contains(expected),
                "{:?}",
                response.data
            );
        }
    });
}

#[test]
fn access_policy_head_reads_revived_worker_task_records() {
    let f = fixture();
    let revived = record(
        &f.ctx,
        "revived-session",
        "bash-1123456789abcdef",
        stamp("worker", "worker", 8, Some("head")),
    );
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        for path in [&f.worker.command, &revived.command] {
            let response = read_as(&f.ctx, "head-session", path);
            assert!(response.success, "{:?}", response.data);
        }
        let mut req = request("bash_status", json!({"task_id": revived.task_id}));
        req.session_id = Some("head-session".into());
        let response = crate::commands::bash_status::handle(&req, &f.ctx);
        assert!(response.success, "{:?}", response.data);
    });
}

#[test]
fn access_policy_other_head_cannot_read_worker_task_records() {
    let f = fixture();
    let mut other = f.head.clone();
    other.scope_ref = "other-head".into();
    {
        let db = f.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        scopes::register_scope(&conn, &other, None, &f.ctx.config()).unwrap();
        scopes::register_agent(&conn, &other, "other-agent", true).unwrap();
    }
    crate::access_policy::with_scope(Some(other), || {
        assert_floor(
            read_as(&f.ctx, "other-head-session", &f.worker.command),
            &f.worker.command,
        )
    });
    let mut other_owner = f.head.clone();
    other_owner.owner = "another-owner".into();
    {
        let db = f.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        scopes::register_scope(&conn, &other_owner, None, &f.ctx.config()).unwrap();
        scopes::register_agent(&conn, &other_owner, "foreign-agent", true).unwrap();
    }
    crate::access_policy::with_scope(Some(other_owner), || {
        assert_floor(
            read_as(&f.ctx, "other-head-session", &f.worker.stdout),
            &f.worker.stdout,
        )
    });
    assert_floor(
        read_as(&f.ctx, "unscoped-session", &f.worker.stdout),
        &f.worker.stdout,
    );
}

#[test]
fn access_policy_worker_cannot_read_sibling_or_head_task_records() {
    let f = fixture();
    let sibling = ScopeKey::from_stamp(&stamp("sibling", "worker", 1, Some("head")));
    let head_task = record(
        &f.ctx,
        "head-session",
        "bash-2123456789abcdef",
        stamp("head", "head", 9, None),
    );
    crate::access_policy::with_scope(Some(sibling), || {
        assert_floor(
            read_as(&f.ctx, "sibling-session", &f.worker.stdout),
            &f.worker.stdout,
        );
        assert_floor(
            read_as(&f.ctx, "sibling-session", &head_task.command),
            &head_task.command,
        );
    });
}

#[test]
fn access_policy_head_cannot_read_task_environment() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        assert_floor(
            read_as(&f.ctx, "head-session", &f.worker.environment),
            &f.worker.environment,
        )
    });
}

#[test]
fn access_policy_session_cannot_read_task_environment() {
    let f = fixture();
    assert_floor(
        read_as(&f.ctx, "worker-session", &f.worker.environment),
        &f.worker.environment,
    );
    f.ctx
        .update_config(|config| config.project_root = Some(f.worker.dir.clone()));
    assert_floor(
        read_as(&f.ctx, "worker-session", &f.worker.environment),
        &f.worker.environment,
    );
}

#[test]
fn access_policy_indexed_task_entries_exclude_environment() {
    let f = fixture();
    let filters = crate::search_index::PathFilters::default();
    assert!(filters.matches(&f.worker.dir, &f.worker.command));
    assert!(!filters.matches(&f.worker.dir, &f.worker.environment));
}

#[test]
fn access_policy_head_cannot_write_worker_task_records() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        let mut req = request(
            "write",
            json!({"file": f.worker.command, "content": "changed"}),
        );
        req.session_id = Some("head-session".into());
        assert_floor(
            crate::commands::write::handle_write(&req, &f.ctx),
            &f.worker.command,
        );
    });
    assert_eq!(
        std::fs::read_to_string(&f.worker.command).unwrap(),
        "printf worker-output\n"
    );
}

#[test]
fn access_policy_own_task_io_folder_grep_and_glob() {
    let f = fixture();
    let mut req = request(
        "grep",
        json!({"path": f.worker.io_dir, "pattern": "worker-output"}),
    );
    req.session_id = Some("worker-session".into());
    let response = crate::commands::grep::handle_grep(&req, &f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert_eq!(response.data["total_matches"], 1, "{:?}", response.data);
    req.command = "glob".into();
    req.params = json!({"path": f.worker.io_dir, "pattern": "*"});
    let response = crate::commands::glob::handle_glob(&req, &f.ctx);
    assert!(response.success, "{:?}", response.data);
    assert!(
        response.data.to_string().contains("stdout"),
        "{:?}",
        response.data
    );
    assert_floor(
        read_as(&f.ctx, "worker-session", &f.worker.command),
        &f.worker.command,
    );
}

#[test]
fn access_policy_head_status_is_read_only_for_worker_tasks() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        let mut req = request(
            "bash_status",
            json!({"task_id": f.worker.task_id, "output_offset": 0}),
        );
        req.session_id = Some("head-session".into());
        let status = crate::commands::bash_status::handle(&req, &f.ctx);
        assert!(status.success, "{:?}", status.data);
        assert_eq!(status.data["exit_code"], 0);
        assert_eq!(status.data["output_chunk_base64"], "d29ya2VyLW91dHB1dAo=");
        req.command = "bash_kill".into();
        let kill = crate::commands::bash_kill::handle(&req, &f.ctx);
        assert_eq!(kill.data["code"], "task_not_found", "{:?}", kill.data);
    });
}

#[test]
fn access_policy_head_recursive_task_reads_exclude_environment() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        let mut req = request(
            "grep",
            json!({"path": f.worker.dir, "pattern": "worker-output|hidden-environment"}),
        );
        req.session_id = Some("head-session".into());
        let response = crate::commands::grep::handle_grep(&req, &f.ctx);
        assert!(response.success, "{:?}", response.data);
        assert_eq!(response.data["total_matches"], 3, "{:?}", response.data);
        assert!(!response.data.to_string().contains("hidden-environment"));
        req.command = "glob".into();
        req.params = json!({"path": f.worker.dir, "pattern": "**/*"});
        let response = crate::commands::glob::handle_glob(&req, &f.ctx);
        assert!(response.success, "{:?}", response.data);
        assert!(response.data.to_string().contains("metadata.json"));
        assert!(!response.data.to_string().contains("environment.bin"));
    });
}

#[test]
fn access_policy_foreign_worker_status_and_controls_are_denied() {
    let f = fixture();
    for session in ["other-head-session", "sibling-session", "unscoped-session"] {
        let mut req = request("bash_status", json!({"task_id": f.worker.task_id}));
        req.session_id = Some(session.into());
        let response = crate::commands::bash_status::handle(&req, &f.ctx);
        assert_eq!(
            response.data["code"], "task_not_found",
            "{:?}",
            response.data
        );
    }
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        let mut req = request(
            "bash_write",
            json!({"task_id": f.worker.task_id, "input": "x"}),
        );
        req.session_id = Some("head-session".into());
        let response = crate::commands::bash_write::handle(&req, &f.ctx);
        assert_eq!(
            response.data["code"], "task_not_found",
            "{:?}",
            response.data
        );
    });
}

#[test]
fn access_policy_task_folder_cache_is_pinned_and_shared_between_files() {
    let f = fixture();
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        assert!(read_as(&f.ctx, "head-session", &f.worker.command).success);
        std::fs::write(&f.worker.json, "invalid metadata").unwrap();
        assert!(read_as(&f.ctx, "head-session", &f.worker.stdout).success);
        std::fs::rename(&f.worker.dir, f.worker.dir.with_extension("old")).unwrap();
        std::fs::create_dir_all(&f.worker.control_dir).unwrap();
        std::fs::write(&f.worker.command, "replacement").unwrap();
        assert_floor(
            read_as(&f.ctx, "head-session", &f.worker.command),
            &f.worker.command,
        );
    });
}

#[test]
fn access_policy_head_status_reads_worker_task_from_another_project() {
    let f = fixture();
    let mut metadata = crate::bash_background::persistence::read_task(&f.worker.json).unwrap();
    metadata.project_root = Some(f._dir.path().join("worker-project"));
    std::fs::create_dir_all(metadata.project_root.as_ref().unwrap()).unwrap();
    write_task(&f.worker.json, &metadata).unwrap();
    let mut req = request(
        "bash_status",
        json!({"task_id": f.worker.task_id, "output_offset": 0, "stderr_offset": 0}),
    );
    req.session_id = Some("head-session".into());
    let head = crate::access_policy::with_scope(Some(f.head.clone()), || {
        crate::commands::bash_status::handle(&req, &f.ctx)
    });
    assert!(head.success, "{:?}", head.data);
    f.ctx
        .update_config(|config| config.project_root = metadata.project_root.clone());
    req.session_id = Some("worker-session".into());
    let worker = crate::commands::bash_status::handle(&req, &f.ctx);
    assert!(worker.success, "{:?}", worker.data);
    assert_eq!(head.data, worker.data);
}

#[cfg(unix)]
#[test]
fn access_policy_head_cannot_control_live_worker_task() {
    let f = fixture();
    let config = f.ctx.config();
    let registry = f.ctx.bash_background();
    let task = crate::bash_background::registry::with_scope_spawn(
        Some(stamp("worker", "worker", 1, Some("head"))),
        || {
            registry
                .spawn(
                    crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                    "exec sleep 30",
                    "worker-session".into(),
                    config.project_root.clone().unwrap(),
                    std::collections::HashMap::new(),
                    crate::bash_background::HardKill::After(std::time::Duration::from_secs(30)),
                    config.storage_dir.clone().unwrap(),
                    10,
                    true,
                    false,
                    config.project_root.clone(),
                )
                .unwrap()
        },
    );
    struct Stop(crate::bash_background::BgTaskRegistry, String);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.kill(&self.1, "worker-session");
        }
    }
    let _stop = Stop(registry.clone(), task.clone());
    let mut req = request("bash_status", json!({"task_id": task}));
    req.session_id = Some("head-session".into());
    crate::access_policy::with_scope(Some(f.head.clone()), || {
        let status = crate::commands::bash_status::handle(&req, &f.ctx);
        assert!(status.success, "{:?}", status.data);
        assert_eq!(status.data["status"], "running");
        req.command = "bash_kill".into();
        let kill = crate::commands::bash_kill::handle(&req, &f.ctx);
        assert_eq!(kill.data["code"], "task_not_found", "{:?}", kill.data);
    });
    assert_eq!(
        registry
            .status(&task, "worker-session", None, None, 0)
            .unwrap()
            .info
            .status,
        BgTaskStatus::Running
    );
    req.session_id = Some("worker-session".into());
    assert!(crate::commands::bash_kill::handle(&req, &f.ctx).success);
}
