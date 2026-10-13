use super::*;

use crate::sandbox_spawn::{
    resolve_sandbox_spawn, with_worker_jail, RequestedSandboxTier, SandboxTaskKind, SpawnPlan,
};

/// Serializes the consent tests: they share the process-wide test transport
/// and the test override for how long a call is held for an answer.
static CONSENT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
struct FakeState {
    up: bool,
    calls: Vec<(String, Value)>,
    /// Elicitation id per consent call key: the same key returns the same ask.
    asks: HashMap<String, String>,
    /// Settled replies per elicitation id.
    answers: HashMap<String, Value>,
}

/// A stand-in for Cingulate that keeps one ask per call key and long-polls.
#[derive(Default)]
struct FakeCingulate {
    state: StdMutex<FakeState>,
}

impl FakeCingulate {
    fn up() -> Arc<Self> {
        let fake = Arc::new(Self::default());
        fake.state.lock().unwrap().up = true;
        fake
    }

    fn calls(&self, method: &str) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    fn answer(&self, call_key: &str, reply: Value) {
        let mut state = self.state.lock().unwrap();
        let id = state.asks[call_key].clone();
        state.answers.insert(id, reply);
    }

    fn reply(&self, method: &str, params: &Value) -> Result<Option<Value>, ConsentError> {
        let mut state = self.state.lock().unwrap();
        if !state.up {
            return Err(ConsentError::Unavailable("cingulate is not running".into()));
        }
        state.calls.push((method.to_string(), params.clone()));
        match method {
            "consent.request" => {
                // Cingulate refuses unknown fields and requires head_policy on
                // worker-scope asks; these tests ask only from worker scopes.
                let keys: Vec<_> = params.as_object().unwrap().keys().cloned().collect();
                assert_eq!(
                    keys,
                    [
                        "args_hash",
                        "call_key",
                        "head_policy",
                        "kind",
                        "operation",
                        "paths",
                        "pattern",
                        "reason",
                        "tool"
                    ]
                );
                let key = params["call_key"].as_str().unwrap().to_string();
                let next = format!("acc_{}", state.asks.len() + 1);
                let id = state.asks.entry(key).or_insert(next).clone();
                Ok(Some(json!({"elicitation_id": id})))
            }
            "consent.await" => {
                let id = params["elicitation_id"].as_str().unwrap();
                assert!(params["timeout_ms"].as_u64().unwrap() <= 120_000);
                Ok(state.answers.get(id).cloned())
            }
            _ => Ok(Some(
                json!({"state": "refused:access_withdrawn", "outcome": "access_withdrawn", "code": "access_withdrawn"}),
            )),
        }
    }
}

struct SharedFake(Arc<FakeCingulate>);

impl ConsentTransport for SharedFake {
    fn call(&self, call: ConsentCall) -> ConsentFuture {
        let fake = Arc::clone(&self.0);
        Box::pin(async move {
            // Go through the real envelope encoder and decoder on both sides.
            let sent: Value = serde_json::from_slice(&encode(call.method, &call.params)?).unwrap();
            let params = sent["params"].clone();
            let wait = Duration::from_millis(params["timeout_ms"].as_u64().unwrap_or(0));
            let end = Instant::now() + wait.min(call.timeout);
            let result = loop {
                if let Some(reply) = fake.reply(call.method, &params)? {
                    break reply;
                }
                // An open ask long-polls until it settles or the wait ends.
                fake.state.lock().unwrap().calls.pop();
                if Instant::now() >= end {
                    break json!({"state": "pending:head"});
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            decode_result(&serde_json::to_vec(&json!({ "result": result })).unwrap())
        })
    }
}

fn stamp(name: &str, kind: &str, parent: Option<&str>) -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
        "ref": name,
        "scope_epoch": 7,
        "kind": kind,
        "parent": parent.map(|parent| json!({
            "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
            "ref": parent,
            "scope_epoch": 7,
        })),
        "owner_authorized": true,
    }))
    .unwrap()
}

fn identity(root: &Path, scope: Option<subc_protocol::scope::ScopeStamp>) -> RouteIdentity {
    let session = scope
        .as_ref()
        .map_or("standalone".to_string(), |stamp| stamp.scope_ref.clone());
    RouteIdentity(Arc::new(RouteIdentityData {
        root: ProjectRootId::from_path(root).unwrap(),
        project_root: root.into(),
        harness: "runner".into(),
        session: session.clone(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::FirstParty,
            route_channel: 41,
            route_epoch: 1,
            project_root: root.into(),
            harness: "runner".into(),
            session_id: session,
            principal_id: Some("reserved:broca".into()),
        },
        consumer_elicitation_capable: false,
        disabled_tools: Arc::new(vec![]),
        scope,
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        run_scope: OnceLock::new(),
        made_tool_call: AtomicBool::new(false),
    }))
}

struct Fixture {
    dir: tempfile::TempDir,
    head_root: PathBuf,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    head: RouteIdentity,
    worker: RouteIdentity,
    active: ActiveToolCalls,
    connection_cancel: PersistentCancelSignal,
    fake: Arc<FakeCingulate>,
}

fn fixture(fake: Arc<FakeCingulate>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let head_root = crate::access_policy::policy_path(&dir.path().join("head"));
    let worker_root = crate::access_policy::policy_path(&dir.path().join("worker"));
    std::fs::create_dir_all(&head_root).unwrap();
    std::fs::create_dir_all(&worker_root).unwrap();
    std::fs::write(head_root.join("shared.txt"), "head-owned contents\n").unwrap();
    std::fs::write(dir.path().join("elsewhere.txt"), "unheld contents\n").unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(worker_root.clone());
    config.storage_dir = Some(dir.path().join("store"));
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&dir.path().join("store").join("aft.db")).unwrap(),
    )));
    let head = identity(&head_root, Some(stamp("head", "head", None)));
    let worker = identity(&worker_root, Some(stamp("worker", "worker", Some("head"))));
    access_grants::register(&head, &ctx).unwrap();
    access_grants::register(&worker, &ctx).unwrap();
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(worker.root.clone(), Arc::clone(&ctx)));
    install_test_transport(Some(Arc::new(SharedFake(Arc::clone(&fake)))));
    Fixture {
        dir,
        head_root,
        ctx,
        executor,
        head,
        worker,
        active: Arc::default(),
        connection_cancel: PersistentCancelSignal::new(),
        fake,
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        install_test_transport(None);
        set_hold_budget_for_test(None);
    }
}

fn read_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::read::handle_read(&request, ctx)
}

impl Fixture {
    /// Send one `read` tool call on `identity`'s route and return the reply body.
    async fn read(&self, identity: &RouteIdentity, path: &Path, corr: u64) -> Value {
        let mut replies = self.send_read(identity, path, corr).await;
        let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
            .await
            .expect("the call ends with one terminal frame")
            .expect("writer stays open");
        serde_json::from_slice(&reply.body).unwrap()
    }

    async fn send_read(
        &self,
        identity: &RouteIdentity,
        path: &Path,
        corr: u64,
    ) -> mpsc::Receiver<WriterFrame> {
        self.send_with(identity, path, corr, read_dispatch).await
    }

    async fn send_with(
        &self,
        identity: &RouteIdentity,
        path: &Path,
        corr: u64,
        dispatch: DispatchFn,
    ) -> mpsc::Receiver<WriterFrame> {
        if self.executor.actor_context(&identity.root).is_none() {
            self.executor
                .register_actor(identity.root.clone(), Arc::clone(&self.ctx));
        }
        let routes = HashMap::from([(route_key(41, 1), identity.clone())]);
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            41,
            1,
            corr,
            serde_json::to_vec(&json!({
                "name": "read",
                "preset": if identity.scope.as_ref().is_some_and(|s| s.kind == subc_protocol::scope::ScopeKind::Head) { "head" } else { "worker" },
                "arguments": {"filePath": path},
            }))
            .unwrap(),
        )
        .unwrap();
        let (writer, replies) = mpsc::channel(8);
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
            PhaseTrace::new(Instant::now()),
            &routes,
            &HashMap::new(),
            &ReclaimedRoutes::default(),
            &mut HashMap::new(),
            &self.executor,
            &self.active,
            &Arc::new(PendingDeferredSetups::default()),
            &Arc::new(Notify::new()),
            &self.connection_cancel,
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
            dispatch,
            &deferred_tx,
            false,
            1024 * 1024,
            &drain::ModuleDrainWindow::default(),
        )
        .await
        .unwrap();
        replies
    }

    fn asks(&self) -> Vec<asks::AskRecord> {
        let db = self.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        asks::for_scope(&conn, &access_grants::scope_key(&self.worker)).unwrap()
    }

    fn worker_grants(&self) -> Vec<AccessGrant> {
        let db = self.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        crate::db::access_grants::list(&conn, &access_grants::scope_key(&self.worker)).unwrap()
    }
}

fn code(reply: &Value) -> &str {
    reply["structuredContent"]["code"].as_str().unwrap_or("")
}

fn text(reply: &Value) -> String {
    reply["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

async fn wait_for(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "condition not reached in time");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn consent_held_call_becomes_pending_and_retry_runs_after_allow_once() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let target = f.head_root.join("shared.txt");

    let held = f.read(&f.worker, &target, 7).await;
    assert_eq!(code(&held), "access_pending", "{held}");
    assert!(text(&held).contains("end your turn") || text(&held).contains("End your turn"));
    assert!(!text(&held).contains("head-owned contents"));
    let requests = f.fake.calls("consent.request");
    assert_eq!(requests.len(), 1);
    let first = &requests[0];
    assert_eq!(first["kind"], "tool_permission");
    assert_eq!(first["operation"], "read");
    assert_eq!(first["head_policy"], "allow");
    assert_eq!(first["paths"][0], json!(target));
    let call_key = first["call_key"].as_str().unwrap().to_string();
    assert_eq!(f.asks().len(), 1, "the open ask is persisted");

    f.fake.answer(
        &call_key,
        json!({"state": "settled", "outcome": "allow_once", "answered_by": "head"}),
    );
    // The retry arrives under a new transport call; it re-sends the original
    // consent call key, gets the same ask, and runs once.
    let allowed = f.read(&f.worker, &target, 8).await;
    assert_ne!(code(&allowed), "access_pending", "{allowed}");
    assert!(text(&allowed).contains("head-owned contents"), "{allowed}");
    let requests = f.fake.calls("consent.request");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["call_key"], json!(call_key));
    assert!(f.asks().is_empty(), "allow_once is used up by the run");
    assert!(
        f.worker_grants().is_empty(),
        "allow_once installs no standing grant"
    );

    // A later identical call asks again under a new consent call key.
    let again = f.read(&f.worker, &target, 9).await;
    assert_eq!(code(&again), "access_pending", "{again}");
    let requests = f.fake.calls("consent.request");
    assert_eq!(requests.len(), 3);
    assert_ne!(requests[2]["call_key"], json!(call_key));
}

#[tokio::test]
async fn consent_answer_within_the_budget_runs_the_held_call() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_secs(8)));
    let target = f.head_root.join("shared.txt");
    let mut replies = f.send_read(&f.worker, &target, 7).await;
    let fake = Arc::clone(&f.fake);
    wait_for(|| !fake.calls("consent.request").is_empty()).await;
    let call_key = f.fake.calls("consent.request")[0]["call_key"]
        .as_str()
        .unwrap()
        .to_string();
    f.fake.answer(
        &call_key,
        json!({"state": "settled", "outcome": "allow_scope"}),
    );
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    let reply: Value = serde_json::from_slice(&reply.body).unwrap();
    assert!(text(&reply).contains("head-owned contents"), "{reply}");
    // allow_scope with no answered_by is the head's answer; the head holds the
    // path, so the scope grant is installed on the worker's scope.
    assert_eq!(f.worker_grants().len(), 1);
}

#[tokio::test]
async fn consent_floor_path_is_denied_without_asking() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let secret = f.worker.project_root.join("secret.txt");
    std::fs::write(&secret, "credential").unwrap();
    f.ctx
        .update_config(|config| config.sandbox.read_deny.push(secret.clone()));
    // A standing grant on the worker's scope cannot override the read_deny
    // floor: the path stays denied even when a grant covers it.
    {
        let db = f.ctx.db().unwrap();
        crate::db::access_grants::insert(
            &db.lock().unwrap(),
            &access_grants::scope_key(&f.worker),
            &AccessGrant {
                path: secret.clone(),
                kind: GrantKind::File,
                operation: AccessOperation::Read,
            },
        )
        .unwrap();
    }
    let reply = f.read(&f.worker, &secret, 7).await;
    assert_eq!(code(&reply), "denied_floor", "{reply}");
    assert!(!text(&reply).contains("credential"));
    assert!(f.fake.calls("consent.request").is_empty());
}

#[tokio::test]
async fn consent_cingulate_down_returns_access_unavailable_never_a_host_prompt() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(Arc::new(FakeCingulate::default()));
    let reply = f.read(&f.worker, &f.head_root.join("shared.txt"), 7).await;
    assert_eq!(code(&reply), "access_unavailable", "{reply}");
    assert!(!reply.to_string().contains("permission_required"));
    assert!(!text(&reply).contains("head-owned contents"));
}

#[tokio::test]
async fn consent_head_answer_is_rechecked_against_the_current_head_policy() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let target = f.dir.path().join("elsewhere.txt");
    let head_grant = AccessGrant {
        path: crate::access_policy::policy_path(&target),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    {
        let db = f.ctx.db().unwrap();
        crate::db::access_grants::insert(
            &db.lock().unwrap(),
            &access_grants::scope_key(&f.head),
            &head_grant,
        )
        .unwrap();
    }
    let pending = f.read(&f.worker, &target, 7).await;
    assert_eq!(code(&pending), "access_pending", "{pending}");
    let request = &f.fake.calls("consent.request")[0];
    assert_eq!(request["head_policy"], "allow");
    // The head's only covering grant is revoked while the ask is pending.
    {
        let db = f.ctx.db().unwrap();
        crate::db::access_grants::revoke(
            &db.lock().unwrap(),
            &access_grants::scope_key(&f.head),
            &head_grant.path,
        )
        .unwrap();
    }
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state": "settled", "outcome": "allow_scope", "answered_by": "head"}),
    );
    let declined = f.read(&f.worker, &target, 8).await;
    assert_eq!(code(&declined), "access_declined", "{declined}");
    assert!(!text(&declined).contains("unheld contents"));
    assert!(f.worker_grants().is_empty(), "no worker grant is recorded");
    assert!(f.asks().is_empty());
}

#[tokio::test]
async fn consent_user_answer_is_not_rechecked_against_the_head() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let target = f.dir.path().join("elsewhere.txt");
    let pending = f.read(&f.worker, &target, 7).await;
    assert_eq!(code(&pending), "access_pending", "{pending}");
    let request = &f.fake.calls("consent.request")[0];
    assert_eq!(request["head_policy"], "not_held");
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state": "settled", "outcome": "allow_once", "answered_by": "user"}),
    );
    let allowed = f.read(&f.worker, &target, 8).await;
    assert!(text(&allowed).contains("unheld contents"), "{allowed}");
}

#[tokio::test]
async fn consent_stamped_head_outside_its_root_keeps_todays_allow() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let target = f.dir.path().join("elsewhere.txt");
    let reply = f.read(&f.head, &target, 7).await;
    assert!(text(&reply).contains("unheld contents"), "{reply}");
    assert!(f.fake.calls("consent.request").is_empty());
}

#[tokio::test]
async fn consent_cancel_and_route_close_withdraw_but_a_transport_drop_keeps_the_ask() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_secs(8)));
    let target = f.head_root.join("shared.txt");
    let fake = Arc::clone(&f.fake);

    // Cancel: the held call's ask is withdrawn and forgotten.
    let _replies = f.send_read(&f.worker, &target, 7).await;
    wait_for(|| fake.calls("consent.request").len() == 1).await;
    assert!(cancel_active_tool_call(&f.active, &f.executor, route_key(41, 1), 7, "test").is_some());
    wait_for(|| fake.calls("consent.withdraw").len() == 1).await;
    wait_for(|| f.asks().is_empty()).await;

    // Route close (the consumer's Goodbye) withdraws too.
    let _replies = f.send_read(&f.worker, &target, 8).await;
    wait_for(|| fake.calls("consent.request").len() == 2).await;
    note_route_closed(route_key(41, 1));
    wait_for(|| fake.calls("consent.withdraw").len() == 2).await;
    wait_for(|| f.asks().is_empty()).await;

    // A transport drop detaches without withdrawing; the retry re-sends the
    // persisted call key.
    let _replies = f.send_read(&f.worker, &target, 9).await;
    wait_for(|| fake.calls("consent.request").len() == 3).await;
    let call_key = fake.calls("consent.request")[2]["call_key"].clone();
    // Connection teardown cancels the connection first, then drops its calls.
    f.connection_cancel.cancel();
    cancel_all_active_tool_calls(&f.active, &f.executor, "connection teardown");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(fake.calls("consent.withdraw").len(), 2);
    assert_eq!(f.asks().len(), 1);
    assert_eq!(f.asks()[0].call_key.as_deref(), call_key.as_str());
}

#[tokio::test]
async fn consent_same_key_retry_replays_the_recorded_access_pending() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let held = f.read(&f.worker, &f.head_root.join("shared.txt"), 7).await;
    assert_eq!(code(&held), "access_pending");
    // The ledger records a keyed call's terminal frame and replays it for a
    // retry under the same transport call key; it never re-dispatches.
    let db = f.ctx.db().unwrap();
    let conn = db.lock().unwrap();
    let key = crate::db::call_ledger::Key {
        carrier: "reserved:broca".into(),
        call_key: "transport-key".into(),
    };
    use crate::db::call_ledger::{admit, settle, Admission, RecordedFrame, State};
    assert!(matches!(
        admit(&conn, &key, "digest", None, State::DispatchStarted).unwrap(),
        Admission::New
    ));
    let recorded = RecordedFrame {
        ty: FrameType::Response,
        body: serde_json::to_vec(&held).unwrap(),
    };
    assert!(settle(&conn, &key, &recorded, "access_pending", None, 1).unwrap());
    let Admission::Repeat(row) =
        admit(&conn, &key, "digest", None, State::DispatchStarted).unwrap()
    else {
        panic!("a same-key retry is a repeat");
    };
    assert_eq!(row.state, State::Settled);
    let replayed: Value = serde_json::from_slice(&row.frame.unwrap().body).unwrap();
    assert_eq!(code(&replayed), "access_pending");
}

#[test]
fn consent_routes_asks_by_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let worker = identity(dir.path(), Some(stamp("w", "worker", Some("h"))));
    let head = identity(dir.path(), Some(stamp("h", "head", None)));
    let head_run = identity(dir.path(), Some(stamp("r", "ephemeral", None)));
    let worker_run = identity(dir.path(), Some(stamp("r", "ephemeral", Some("w"))));
    let standalone = identity(dir.path(), None);
    let mut foreign = stamp("x", "worker", Some("h"));
    foreign.owner = Principal::Direct;
    let foreign = identity(dir.path(), Some(foreign));
    for (identity, asks) in [
        (&worker, true),
        (&worker_run, true),
        (&head, false),
        (&head_run, false),
        (&standalone, false),
        (&foreign, false),
    ] {
        assert_eq!(
            access_grants::ask_route(identity) != AskRoute::Lowered,
            asks
        );
    }
}

#[test]
fn consent_hold_budget_is_capped_at_100_seconds() {
    assert_eq!(declared_hold_budget(None), Duration::from_secs(20));
    assert_eq!(
        declared_hold_budget(Some(Duration::from_secs(1_800))),
        Duration::from_secs(100)
    );
}

fn vector(name: &str) -> Vec<u8> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/subc/fixtures/cingulate-tool-permission-v1/vectors");
    let bytes = std::fs::read(dir.join(name)).unwrap();
    // Each vector must be listed in Cingulate's SHA256SUMS with this hash.
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap();
    let expected = sums
        .lines()
        .find_map(|line| {
            let (hash, file) = line.split_once("  ")?;
            (file == name).then_some(hash)
        })
        .unwrap_or_else(|| panic!("{name} is not in SHA256SUMS"));
    use sha2::Digest;
    let actual: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(actual, expected, "{name} differs from Cingulate's manifest");
    bytes
}

fn vector_json(name: &str) -> Value {
    serde_json::from_slice(&vector(name)).unwrap()
}

#[test]
fn consent_vectors_requests_encode_byte_for_byte() {
    let open = request_params(
        "call-1",
        "read",
        "read",
        "/tmp/file",
        "Inspect file",
        "hash",
        "/tmp/file",
        Some("not_held"),
    );
    for name in ["worker-open-request.json", "replay-request.json"] {
        assert_eq!(
            encode("consent.request", &open).unwrap(),
            vector(name),
            "{name}"
        );
    }
    for (name, id) in [
        ("await-pending-request.json", "acc_2"),
        ("await-settled-request.json", "acc_1"),
    ] {
        assert_eq!(
            encode(
                "consent.await",
                &json!({"elicitation_id": id, "timeout_ms": 0})
            )
            .unwrap(),
            vector(name),
            "{name}"
        );
    }
    assert_eq!(
        encode("consent.withdraw", &json!({"elicitation_id": "acc_2"})).unwrap(),
        vector("withdrawn-request.json")
    );
}

#[test]
fn consent_vectors_replies_decode() {
    for name in ["worker-open-reply.json", "replay-reply.json"] {
        let result = decode_result(&vector(name)).unwrap();
        assert_eq!(decode_elicitation_id(&result).unwrap(), "acc_1", "{name}");
    }
    assert_eq!(
        decode_record(&decode_result(&vector("await-pending-reply.json")).unwrap()).unwrap(),
        Settlement::Pending
    );
    assert_eq!(
        decode_record(&decode_result(&vector("await-settled-reply.json")).unwrap()).unwrap(),
        Settlement::Allow {
            scope: true,
            by_user: true
        }
    );
    assert_eq!(
        decode_record(&decode_result(&vector("withdrawn-reply.json")).unwrap()).unwrap(),
        Settlement::Refused("access_withdrawn".into())
    );
    // The head relay's replies embed the ask record, which is still pending
    // in both.
    for name in ["head-not-held-reply.json", "escalated-reply.json"] {
        let result = decode_result(&vector(name)).unwrap();
        assert_eq!(
            decode_record(&result["elicitation"]).unwrap(),
            Settlement::Pending,
            "{name}"
        );
    }
    // The requests these replies answer are the head relay's, not AFT's.
    for name in ["head-not-held-request.json", "escalated-request.json"] {
        assert_eq!(vector_json(name)["method"], "consent.answer", "{name}");
    }
}

#[test]
fn consent_vectors_settled_allow_without_answered_by_is_the_head() {
    let mut record = decode_result(&vector("await-settled-reply.json")).unwrap();
    record.as_object_mut().unwrap().remove("answered_by");
    record["outcome"] = json!("allow_once");
    assert_eq!(
        decode_record(&record).unwrap(),
        Settlement::Allow {
            scope: false,
            by_user: false
        }
    );
    record["outcome"] = json!("deny");
    record["note"] = json!("not this one");
    assert_eq!(
        decode_record(&record).unwrap(),
        Settlement::Deny {
            note: Some("not this one".into())
        }
    );
    record["state"] = json!("refused:access_head_gone");
    record["outcome"] = json!("access_head_gone");
    assert_eq!(
        decode_record(&record).unwrap(),
        Settlement::Refused("access_head_gone".into())
    );
    // Shapes outside the contract are refused: a record without a state, an
    // id under another key, and a reply without the `result` envelope.
    assert!(decode_record(&json!({"outcome": "allow_once"})).is_err());
    assert!(decode_elicitation_id(&json!({"id": "acc_1"})).is_err());
    assert!(decode_result(br#"{"elicitation_id":"acc_1"}"#).is_err());
}

#[test]
fn consent_vectors_bind_stamps_route_asks_under_the_worker_stamp() {
    let stamps = vector_json("bind-stamps.json");
    let worker: subc_protocol::scope::ScopeStamp =
        serde_json::from_value(stamps["worker"]["scope"].clone()).unwrap();
    let head: subc_protocol::scope::ScopeStamp =
        serde_json::from_value(stamps["head"]["scope"].clone()).unwrap();
    assert_eq!(stamps["worker"]["principal"], "reserved:aft");
    let dir = tempfile::tempdir().unwrap();
    let worker_identity = identity(dir.path(), Some(worker.clone()));
    let head_identity = identity(dir.path(), Some(head));
    assert!(access_grants::alfonso_worker(&worker_identity));
    assert!(!access_grants::alfonso_worker(&head_identity));
    // Asks go out under the asking worker's own stamp.
    let sent = selector(&worker);
    assert_eq!(sent.scope_ref, "worker");
    assert_eq!(sent.scope_epoch, Some(3));
    assert_eq!(sent.owner, worker.owner);
}

fn jail_context(root: &Path, sandbox: bool) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.to_path_buf());
    config.storage_dir = Some(root.join(".store"));
    config.sandbox.enabled = sandbox;
    AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config)
}

#[test]
fn consent_worker_jail_never_runs_a_shell_unconfined() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("worktree");
    let task = dir.path().join("task").join("io");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&task).unwrap();
    let ctx = jail_context(&root, false);
    let principal = AuthenticatedPrincipal::RouteBind {
        trust: PrincipalTrust::FirstParty,
        route_channel: 41,
        route_epoch: 1,
        project_root: root.clone(),
        harness: "runner".into(),
        session_id: "worker".into(),
        principal_id: Some("reserved:broca".into()),
    };
    let resolve = |tier| {
        resolve_sandbox_spawn(
            &ctx,
            &principal,
            tier,
            SandboxTaskKind::BashForeground,
            &task,
            None,
        )
    };
    // A head (no jail) keeps the user's choice: sandbox off runs unsandboxed.
    assert!(matches!(
        resolve(RequestedSandboxTier::Disabled),
        SpawnPlan::Unsandboxed
    ));
    with_worker_jail(true, || {
        // The jail forces the native sandbox even with sandbox.enabled off;
        // where it cannot be built the shell is refused by name.
        let native = resolve(RequestedSandboxTier::Native);
        assert!(
            native.is_native_launcher() || native.refusal_code() == Some("sandbox_unavailable"),
            "{native:?}"
        );
        if let SpawnPlan::Launcher { profile, .. } = &native {
            // Writes are confined to the worktree: the directory holding it
            // (and the sibling worktrees in it) is not writable.
            let worktree = root.canonicalize().unwrap();
            let parent = worktree.parent().unwrap();
            assert!(profile.writable_roots.contains(&worktree), "{profile:?}");
            assert!(
                !profile
                    .writable_roots
                    .iter()
                    .any(|writable| parent.starts_with(writable)),
                "{profile:?}"
            );
        }
        for tier in [RequestedSandboxTier::Disabled, RequestedSandboxTier::Host] {
            assert_eq!(resolve(tier).refusal_code(), Some("sandbox_unavailable"));
        }
        let untrusted = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::Untrusted,
            route_channel: 41,
            route_epoch: 1,
            project_root: root.clone(),
            harness: "runner".into(),
            session_id: "worker".into(),
            principal_id: None,
        };
        assert_eq!(
            resolve_sandbox_spawn(
                &ctx,
                &untrusted,
                RequestedSandboxTier::Native,
                SandboxTaskKind::BashForeground,
                &task,
                None,
            )
            .refusal_code(),
            Some("sandbox_unavailable")
        );
    });
    assert!(!crate::sandbox_spawn::worker_jail_active());
}

static RERUN_STARTED: AtomicBool = AtomicBool::new(false);
static RERUN_CANCELLED: AtomicBool = AtomicBool::new(false);

/// Reads normally, except that an approved rerun (one carrying `allow_once`
/// approvals) waits until its job is cancelled, and records whether it was.
fn blocking_rerun_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    if let AskRoute::Consent { once } = crate::access_policy::current_ask_route() {
        if !once.is_empty() {
            RERUN_STARTED.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !crate::executor::current_job_cancelled() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            RERUN_CANCELLED.store(crate::executor::current_job_cancelled(), Ordering::SeqCst);
            return Response::error(&request.id, "request_cancelled", "rerun stopped");
        }
    }
    read_dispatch(request, ctx)
}

async fn start_blocked_rerun(f: &Fixture, corr: u64) -> mpsc::Receiver<WriterFrame> {
    RERUN_STARTED.store(false, Ordering::SeqCst);
    RERUN_CANCELLED.store(false, Ordering::SeqCst);
    set_hold_budget_for_test(Some(Duration::from_secs(8)));
    let replies = f
        .send_with(
            &f.worker,
            &f.head_root.join("shared.txt"),
            corr,
            blocking_rerun_dispatch,
        )
        .await;
    let fake = Arc::clone(&f.fake);
    wait_for(|| !fake.calls("consent.request").is_empty()).await;
    let call_key = fake.calls("consent.request")[0]["call_key"]
        .as_str()
        .unwrap()
        .to_string();
    f.fake.answer(
        &call_key,
        json!({"state": "settled", "outcome": "allow_once", "answered_by": "head"}),
    );
    wait_for(|| RERUN_STARTED.load(Ordering::SeqCst)).await;
    replies
}

#[tokio::test]
async fn consent_cancel_reaches_the_approved_rerun() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let _replies = start_blocked_rerun(&f, 7).await;
    // The rerun is the call's active job: a Cancel for the call stops it.
    assert!(cancel_active_tool_call(&f.active, &f.executor, route_key(41, 1), 7, "test").is_some());
    wait_for(|| RERUN_CANCELLED.load(Ordering::SeqCst)).await;
}

#[tokio::test]
async fn consent_route_close_reaches_the_approved_rerun() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let _replies = start_blocked_rerun(&f, 7).await;
    note_route_closed(route_key(41, 1));
    wait_for(|| RERUN_CANCELLED.load(Ordering::SeqCst)).await;
    wait_for(|| f.asks().is_empty()).await;
}
