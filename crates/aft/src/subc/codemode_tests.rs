//! Codemode admission: a run route's tool call acts for the head agent its
//! stamp names, located only through the stamped parent link, and is refused
//! by name whenever that agent's own route can't be found.

use super::*;
use crate::subc::test_support::{route_identity, test_root};
use subc_protocol::call_mode::CallMode;
use subc_protocol::scope::ScopeStamp;

const OWNER: &str = "prefrontal-core";

fn head_stamp(scope_ref: &str, agent: &str) -> ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": OWNER},
        "ref": scope_ref, "scope_epoch": 3, "kind": "head",
        "attributes": {"agent_id": agent},
        "owner_authorized": true,
    }))
    .unwrap()
}

fn run_stamp(agent: &str, parent: &str, parent_state: Option<&str>) -> ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": OWNER},
        "ref": "run-scope", "scope_epoch": 1, "kind": "ephemeral",
        "attributes": {"agent_id": agent, "run_id": "run-1"},
        "parent": {"owner": {"kind": "reserved", "module_id": OWNER}, "ref": parent, "scope_epoch": 3},
        "parent_state": parent_state,
        "owner_authorized": true,
    }))
    .unwrap()
}

fn route(root: &ProjectRootId, session: &str, scope: ScopeStamp, mode: CallMode) -> RouteIdentity {
    let mut identity = route_identity(root, session);
    let data = Arc::get_mut(&mut identity.0).unwrap();
    data.scope = Some(scope);
    data.call_mode = mode;
    data.disabled_tools = Arc::new(vec![format!("disabled-by-{session}")]);
    identity
}

fn channel(channel: u16) -> RouteChannel {
    route_key(channel, 1)
}

struct Fixture {
    _dirs: Vec<tempfile::TempDir>,
    head_root: ProjectRootId,
    routes: HashMap<RouteChannel, RouteIdentity>,
}

/// A head route for `agent-a` on channel 1, a second head (`agent-b`) on
/// channel 2, and the run route on channel 9, bound under a third root.
fn fixture(run: ScopeStamp) -> Fixture {
    let (head_dir, head_root) = test_root("codemode-head");
    let (other_dir, other_root) = test_root("codemode-other-head");
    let (run_dir, run_root) = test_root("codemode-run");
    let routes = HashMap::from([
        (
            channel(1),
            route(
                &head_root,
                "head-a-session",
                head_stamp("head-a", "agent-a"),
                CallMode::Classic,
            ),
        ),
        (
            channel(2),
            route(
                &other_root,
                "head-b-session",
                head_stamp("head-b", "agent-b"),
                CallMode::Classic,
            ),
        ),
        (
            channel(9),
            route(&run_root, "run-session", run, CallMode::Codemode),
        ),
    ]);
    Fixture {
        _dirs: vec![head_dir, other_dir, run_dir],
        head_root,
        routes,
    }
}

fn call(name: &str, arguments: Value) -> Value {
    json!({"name": name, "preset": "head", "arguments": arguments})
}

fn refusal(admission: Admission) -> subc_protocol::ErrorBody {
    match admission {
        Admission::Refused(error) => error,
        Admission::Owner(identity) => panic!("admitted as {}", identity.session),
        Admission::Unchanged => panic!("admission left the call unchanged"),
    }
}

fn owner(admission: Admission) -> RouteIdentity {
    match admission {
        Admission::Owner(identity) => identity,
        Admission::Refused(error) => panic!("refused: {} {:?}", error.code, error.detail),
        Admission::Unchanged => panic!("admission left the call unchanged"),
    }
}

#[test]
fn codemode_call_to_tool_without_codemode_is_refused_call_mode_unsupported() {
    let f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    let body = call("bash_watch", json!({"taskId": "absent"}));
    let error = refusal(admit(&f.routes, channel(9), Some(&body)));
    assert_eq!(
        error.code,
        cortexkit_role_tool_provider::errors::CALL_MODE_UNSUPPORTED
    );
    assert_eq!(
        error.detail,
        Some(json!({"tool": "bash_watch", "mode": "codemode"}))
    );
}

#[test]
fn codemode_agent_named_in_arguments_never_replaces_stamp_agent() {
    let f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    // The arguments name the other head's agent and scope. Only the stamp
    // decides whose access the call runs with.
    let body = call(
        "read",
        json!({
            "path": "notes.txt",
            "agent_id": "agent-b",
            "run_id": "other-run",
            "scope": "head-b",
            "session_id": "head-b-session",
        }),
    );
    let effective = owner(admit(&f.routes, channel(9), Some(&body)));
    let owner = effective.run_owner.as_ref().unwrap();
    assert_eq!(owner.agent_id, "agent-a");
    assert_eq!(owner.run_id, "run-1");
    assert_eq!(effective.session, "head-a-session");
    assert_eq!(effective.root, f.head_root);
}

#[test]
fn codemode_route_stamped_parent_ended_serves_no_new_call() {
    for (state, reason) in [
        (Some("ended"), "parent_ended"),
        (Some("pending"), "parent_pending"),
        (None, "parent_state_missing"),
    ] {
        let f = fixture(run_stamp("agent-a", "head-a", state));
        let body = call("read", json!({"path": "notes.txt"}));
        let error = refusal(admit(&f.routes, channel(9), Some(&body)));
        assert_eq!(error.code, RUN_OWNER_NOT_BOUND, "{state:?}");
        assert_eq!(error.detail.as_ref().unwrap()["reason"], reason);
        assert_eq!(error.detail.as_ref().unwrap()["agent_id"], "agent-a");
        assert_eq!(error.detail.as_ref().unwrap()["run_id"], "run-1");
    }
}

#[test]
fn codemode_without_live_head_route_is_refused_run_owner_not_bound() {
    let mut f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    f.routes.remove(&channel(1));
    let body = call("read", json!({"path": "notes.txt"}));
    let error = refusal(admit(&f.routes, channel(9), Some(&body)));
    assert_eq!(error.code, RUN_OWNER_NOT_BOUND);
    assert_eq!(
        error.detail.as_ref().unwrap()["reason"],
        "head_route_not_bound"
    );
}

#[test]
fn codemode_mismatched_agent_is_refused_run_owner_not_bound() {
    // The run is on agent-b, but its parent link names agent-a's head.
    let f = fixture(run_stamp("agent-b", "head-a", Some("linked")));
    let body = call("read", json!({"path": "notes.txt"}));
    let error = refusal(admit(&f.routes, channel(9), Some(&body)));
    assert_eq!(error.code, RUN_OWNER_NOT_BOUND);
    assert_eq!(error.detail.as_ref().unwrap()["reason"], "agent_mismatch");
}

#[test]
fn codemode_call_runs_with_head_route_access_and_run_lifetime() {
    let f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    let head = f.routes[&channel(1)].clone();
    let run = f.routes[&channel(9)].clone();
    let body = call("edit", json!({"path": "notes.txt"}));
    let effective = owner(admit(&f.routes, channel(9), Some(&body)));
    // Access is the head route's.
    assert_eq!(effective.root, head.root);
    assert_eq!(effective.project_root, head.project_root);
    assert_eq!(effective.session, head.session);
    assert_eq!(effective.harness, head.harness);
    assert_eq!(effective.trust, head.trust);
    assert_eq!(*effective.disabled_tools, *head.disabled_tools);
    assert_eq!(
        access_grants::policy_scope_key(&effective),
        access_grants::scope_key(&head)
    );
    assert_eq!(
        remote_policy::key(&effective, Some("worker")),
        remote_policy::key(&head, Some("worker"))
    );
    assert!(!access_grants::alfonso_worker(&effective));
    assert_eq!(
        access_grants::ask_route(&effective),
        crate::access_policy::AskRoute::Unavailable
    );
    // Lifetime stays the run's: its scope keeps tagging the work it starts.
    assert_eq!(effective.scope, run.scope);
    assert_eq!(effective.call_mode, CallMode::Codemode);
    assert_eq!(effective.role, run.role);
}

#[test]
fn codemode_provider_operations_keep_the_run_route_identity() {
    let mut f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    f.routes.remove(&channel(1));
    for body in [
        json!({"op": "tool.catalog", "preset": "head"}),
        json!({"name": "late_results", "arguments": {}}),
    ] {
        assert!(matches!(
            admit(&f.routes, channel(9), Some(&body)),
            Admission::Unchanged
        ));
    }
}

#[test]
fn classic_route_calls_are_unchanged() {
    let f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    let body = call("read", json!({"path": "notes.txt"}));
    assert!(matches!(
        admit(&f.routes, channel(1), Some(&body)),
        Admission::Unchanged
    ));
}

/// Give `route`'s spawn principal the id `principal_id`.
fn with_principal_id(
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    id: &str,
) {
    let identity = routes.get_mut(&route).unwrap();
    let data = Arc::get_mut(&mut identity.0).unwrap();
    if let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut data.spawn_principal {
        *principal_id = Some(id.to_owned());
    }
}

#[test]
fn codemode_call_uses_head_principal_and_records_ledger_under_run_route() {
    let mut f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    // The run route's principal id would force the strict sandbox if it were
    // used for the call; the head's does not.
    with_principal_id(&mut f.routes, channel(9), "unverified");
    let head = f.routes[&channel(1)].clone();
    let run = f.routes[&channel(9)].clone();
    let body = call("read", json!({"path": "notes.txt"}));
    let effective = owner(admit(&f.routes, channel(9), Some(&body)));
    // Sandbox and permission decisions read the head's principal, id included.
    assert_eq!(effective.spawn_principal, head.spawn_principal);
    assert!(!crate::sandbox_spawn::principal_requires_strict(
        &effective.spawn_principal
    ));
    // The call's ledger rows are keyed the way the run route's late_results
    // and tool.withdraw look them up.
    let call_key = "program-call";
    assert_eq!(
        tool_provider::LedgerEdge::key(&effective, call_key),
        tool_provider::LedgerEdge::key(&run, call_key)
    );
    assert_eq!(
        tool_provider::LedgerEdge::key(&effective, call_key).carrier,
        "unverified"
    );
    assert_eq!(
        effective.run_owner.as_ref().unwrap().ledger_carrier(),
        "unverified"
    );
}

fn audit_read(req: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::read::handle_read(&req, ctx)
}

#[test]
fn codemode_audit_line_is_written_once_per_admitted_call() {
    let f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    let file = f.head_root.as_path().join("audited.txt");
    std::fs::write(&file, "audited contents\n").unwrap();
    let ctx = crate::subc::test_support::test_ctx();
    ctx.update_config(|config| config.project_root = Some(f.head_root.as_path().to_path_buf()));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(f.head_root.clone(), ctx));
    let body = call("read", json!({"path": &file}));
    let effective = owner(admit(&f.routes, channel(9), Some(&body)));
    let routes = overlay(&f.routes, channel(9), effective);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (reply, lines) = crate::logging::capture_log_lines(|| {
        runtime.block_on(async {
            let frame = Frame::build(
                FrameType::Request,
                control_flags(),
                9,
                1,
                77,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap();
            let (writer, mut replies) = mpsc::channel(8);
            let (bash_tx, _bash_rx) = mpsc::channel(8);
            let (touch_tx, _touch_rx) = mpsc::channel(8);
            let (entries, _deferred_rx) = mpsc::unbounded_channel();
            let deferred_tx = DeferredResponseSender {
                entries,
                wake: crate::response_finalize::DeferredResponseWake::default(),
            };
            handle_tool_call(
                &writer,
                &frame,
                None,
                None,
                PhaseTrace::new(Instant::now()),
                &routes,
                &HashMap::new(),
                &ReclaimedRoutes::default(),
                &mut HashMap::new(),
                &executor,
                &ActiveToolCalls::default(),
                &Arc::new(PendingDeferredSetups::default()),
                &Arc::new(Notify::new()),
                &PersistentCancelSignal::new(),
                &bash_tx,
                &touch_tx,
                &Arc::new(DispatchPathMetrics::new()),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut 1,
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                audit_read,
                &deferred_tx,
                false,
                1024 * 1024,
                &drain::ModuleDrainWindow::default(),
                None,
            )
            .await
            .unwrap();
            tokio::time::timeout(Duration::from_secs(10), replies.recv())
                .await
                .expect("the audited call terminates")
                .expect("writer stays open")
        })
    });
    assert!(
        String::from_utf8_lossy(&reply.body).contains("audited contents"),
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let audit: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("codemode call: tool=read"))
        .collect();
    assert_eq!(audit.len(), 1, "{lines:#?}");
    for field in [
        "agent_id=agent-a",
        "run_id=run-1",
        "head_session=head-a-session",
    ] {
        assert!(
            audit[0].contains(field),
            "{field} missing from {}",
            audit[0]
        );
    }
}
