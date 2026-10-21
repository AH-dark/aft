use super::*;
use crate::gh_shim_relay::{RelayTransport, ScopeSelector, TransportError};
use crate::gh_shim_ticket::{PendingTicket, ScopedTicket};
use std::sync::atomic::{AtomicUsize, Ordering};

fn stamp() -> ScopeStamp {
    ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "worker-broker",
        7,
        subc_protocol::scope::ScopeKind::Worker,
    )
    .with_attributes(
        subc_protocol::scope::ScopeAttributes::new()
            .with_agent_id(Some("worker-principal".into()))
            .with_delegates(true),
    )
    .with_owner_authorized(true)
}

struct Grant {
    ticket: PendingTicket,
    capability: String,
    socket: PathBuf,
    task: String,
}
impl Grant {
    fn new() -> Self {
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).unwrap();
        let task = format!("broker-test-{id:x?}");
        let socket = PathBuf::from(format!("/not-a-real-broker/{task}"));
        let ticket = PendingTicket::issue(&format!("ses-{task}"), "/server/project");
        ticket.reserve_task(&task);
        let (capability, _) = ticket
            .worker_read_capability(
                socket.clone(),
                Instant::now() + Duration::from_secs(60),
                &stamp(),
            )
            .unwrap();
        Self {
            ticket,
            capability,
            socket,
            task,
        }
    }
    fn envelope(&self, operation: Operation) -> Vec<u8> {
        serde_json::to_vec(&Envelope {
            capability: self.capability.clone(),
            operation,
        })
        .unwrap()
    }
}

#[derive(Default)]
struct Transport {
    calls: AtomicUsize,
    roots: Mutex<Vec<String>>,
    requests: Mutex<Vec<Value>>,
}
impl RelayTransport for Transport {
    fn read_repository(&self, ticket: &crate::gh_shim_ticket::Redeemed) -> Result<String, String> {
        self.roots.lock().unwrap().push(ticket.project_root.clone());
        Ok("cortexkit/aft".into())
    }
    async fn plexus(
        &self,
        project: &str,
        session: &str,
        scope: Option<&ScopeSelector>,
        body: Value,
    ) -> Result<Value, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(project, "/server/project");
        assert!(session.starts_with("ses-broker-test-"));
        assert_eq!(scope.unwrap().scope_epoch, Some(7));
        assert_eq!(body["name"], "github_read");
        assert_eq!(body["arguments"]["repository"], "cortexkit/aft");
        assert!(body["arguments"].get("principal").is_none());
        self.requests.lock().unwrap().push(body);
        Ok(json!({"sentinel":"worker-bound-read"}))
    }
    fn opens_scoped_routes(&self) -> bool {
        true
    }
}
fn budget() -> Mutex<Budget> {
    Mutex::new(Budget {
        window: Instant::now(),
        requests: 0,
        upstream: 0,
    })
}
fn read(repository: &str) -> Operation {
    Operation::ReadRequest {
        request: serde_json::from_value(json!({
            "action":"github.repo.get", "repository":repository, "args":{"path":""}
        }))
        .unwrap(),
    }
}
async fn call(
    grant: &Grant,
    transport: &Transport,
    bytes: &[u8],
    scope: Option<ScopeStamp>,
) -> RelayReply {
    dispatch(
        bytes,
        &grant.socket,
        transport,
        |cap| crate::gh_shim_ticket::redeem_worker_read(cap, &grant.socket),
        |_| true,
        |_| std::future::ready(Ok(scope.clone())),
        &budget(),
    )
    .await
}

#[tokio::test]
async fn gh_shim_broker_t1_allowlist_precedes_authentication_and_upstream() {
    let transport = Transport::default();
    let redeems = AtomicUsize::new(0);
    let scopes = AtomicUsize::new(0);
    let mut attacks = vec![
        vec![],
        b"{}".to_vec(),
        b"{broken".to_vec(),
        vec![b'x'; MAX_FRAME + 1],
    ];
    for op in [
        "gh_shim.bot_request",
        "gh_shim.bindings_read",
        "",
        "unknown",
        "tool.call",
        "subscribe",
    ] {
        attacks.push(
            serde_json::to_vec(&json!({"capability":"a".repeat(32),"operation":{"op":op}}))
                .unwrap(),
        );
    }
    for bytes in attacks {
        let reply = dispatch(
            &bytes,
            Path::new("/endpoint"),
            &transport,
            |_| {
                redeems.fetch_add(1, Ordering::SeqCst);
                None
            },
            |_| true,
            |_| async {
                scopes.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            },
            &budget(),
        )
        .await;
        assert!(!reply.ok);
    }
    assert_eq!(
        redeems.load(Ordering::SeqCst),
        0,
        "refuse before redemption"
    );
    assert_eq!(scopes.load(Ordering::SeqCst), 0);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn gh_shim_broker_t2_capability_binding_and_atomic_revocation() {
    let a = Grant::new();
    let b = Grant::new();
    assert!(crate::gh_shim_ticket::redeem_worker_read(&a.capability, &a.socket).is_some());
    assert!(crate::gh_shim_ticket::redeem_worker_read(&a.capability, &b.socket).is_none());
    assert!(
        crate::gh_shim_ticket::redeem_worker_read(a.ticket.value().unwrap(), &a.socket).is_none()
    );
    assert!(crate::gh_shim_ticket::redeem(&a.capability).is_none());
    let scoped = ScopedTicket::issue("host-scope", "host-call", "/server/project");
    assert!(
        crate::gh_shim_ticket::redeem_worker_read(scoped.value().unwrap(), &a.socket).is_none()
    );
    crate::gh_shim_ticket::revoke_task(&a.task);
    assert!(crate::gh_shim_ticket::redeem_worker_read(&a.capability, &a.socket).is_none());
    assert!(crate::gh_shim_ticket::redeem_worker_read(&b.capability, &b.socket).is_some());
    let cap = b.capability.clone();
    let socket = b.socket.clone();
    drop(b);
    assert!(crate::gh_shim_ticket::redeem_worker_read(&cap, &socket).is_none());
    let head = PendingTicket::issue("head-command", "/server/project");
    head.reserve_task("head-command");
    let mut head_stamp = stamp();
    head_stamp.kind = subc_protocol::scope::ScopeKind::Head;
    assert!(head
        .worker_read_capability(
            PathBuf::from("/head"),
            Instant::now() + Duration::from_secs(1),
            &head_stamp
        )
        .is_none());
}

#[tokio::test]
async fn gh_shim_broker_t3_identity_is_server_derived_and_scope_is_live() {
    let grant = Grant::new();
    let transport = Transport::default();
    let good = grant.envelope(read("cortexkit/aft"));
    let reply = call(&grant, &transport, &good, Some(stamp())).await;
    assert!(reply.ok, "{reply:?}");
    assert_eq!(reply.data["sentinel"], "worker-bound-read");
    let reply = call(
        &grant,
        &transport,
        &grant.envelope(read("cortexkit/other")),
        Some(stamp()),
    )
    .await;
    assert_eq!(reply.data["refusal_code"], "repository_not_visible");
    for field in [
        "session_id",
        "project_root",
        "scope",
        "repository",
        "ticket",
        "first_party",
    ] {
        let mut body: Value = serde_json::from_slice(&good).unwrap();
        body[field] = json!("foreign");
        assert!(
            !call(
                &grant,
                &transport,
                &serde_json::to_vec(&body).unwrap(),
                Some(stamp())
            )
            .await
            .ok
        );
    }
    assert!(!call(&grant, &transport, &good, None).await.ok);
    let mut head = stamp();
    head.kind = subc_protocol::scope::ScopeKind::Head;
    assert!(!call(&grant, &transport, &good, Some(head)).await.ok);
    let mut rebind = stamp();
    rebind.scope_epoch += 1;
    assert!(!call(&grant, &transport, &good, Some(rebind)).await.ok);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn gh_shim_broker_t4_frame_and_rate_and_upstream_budgets_are_bounded() {
    let (mut tx, mut rx) = tokio::io::duplex(8);
    tx.write_u32((MAX_FRAME + 1) as u32).await.unwrap();
    assert!(read_frame(&mut rx, MAX_FRAME).await.is_err());
    let mut limits = Budget {
        window: Instant::now(),
        requests: 29,
        upstream: 599,
    };
    assert!(limits.take(true));
    assert!(!limits.take(true));
    limits.window = Instant::now() - Duration::from_secs(2);
    assert!(limits.take(false));
    assert!(
        !limits.take(true),
        "upstream budget does not refill with request rate"
    );
}

#[tokio::test]
async fn gh_shim_broker_revocation_during_lookup_refuses_before_upstream() {
    let grant = Grant::new();
    let transport = Transport::default();
    let reply = dispatch(
        &grant.envelope(read("cortexkit/aft")),
        &grant.socket,
        &transport,
        |cap| crate::gh_shim_ticket::redeem_worker_read(cap, &grant.socket),
        |_| true,
        |_| async {
            crate::gh_shim_ticket::revoke_task(&grant.task);
            Ok(Some(stamp()))
        },
        &budget(),
    )
    .await;
    assert_eq!(reply.data["refusal_code"], "worker_binding_changed");
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn gh_shim_broker_t6_environment_strip_pty_and_exec_reinsertion() {
    use std::collections::HashMap;
    let environment: HashMap<String, String> = CHILD_KEYS
        .into_iter()
        .map(|key| (key.into(), "outer-command".into()))
        .collect();
    let mut command = std::process::Command::new("unused");
    command.envs(&environment);
    crate::agent_child_env::apply_to_command(&mut command, &HashMap::new());
    for key in CHILD_KEYS {
        assert!(
            command.get_envs().any(|(k, v)| k == key && v.is_none()),
            "inherited {key}"
        );
    }
    let mut pty = portable_pty::CommandBuilder::new("unused");
    for (key, value) in &environment {
        pty.env(key, value);
    }
    crate::agent_child_env::scrub_pty_command(&mut pty);
    for key in CHILD_KEYS {
        assert!(
            !pty.iter_full_env_as_str().any(|(k, _)| k == key),
            "PTY inherited {key}"
        );
    }
    let plan = crate::sandbox_spawn::SpawnPlan::RelaxedUnenforced;
    let serialized = crate::sandbox_spawn::approved_environment_for_plan(&plan, &environment);
    for key in [
        crate::gh_shim_ticket::GH_SHIM_TICKET_ENV,
        SOCKET_ENV,
        CAPABILITY_FILE_ENV,
    ] {
        assert!(
            !serialized.contains_key(std::ffi::OsStr::new(key)),
            "serialized {key}"
        );
    }
    crate::sandbox_spawn::apply_sandbox_environment(&plan, &mut command, &environment);
    for key in [SOCKET_ENV, CAPABILITY_FILE_ENV] {
        assert!(command
            .get_envs()
            .any(|(k, v)| k == key && v == Some(std::ffi::OsStr::new("outer-command"))));
    }
    let mut pty_environment = environment.clone();
    pty_environment.insert("SUBC_LAUNCH_NONCE".into(), "must-not-reach-child".into());
    let (pty, _) = crate::sandbox_spawn::pty_command_for_plan(
        &crate::sandbox_spawn::SpawnPlan::Unsandboxed,
        std::ffi::OsStr::new("unused"),
        &[],
        Path::new("unused-marker"),
        Path::new("."),
        &pty_environment,
    )
    .unwrap();
    assert!(!pty
        .iter_full_env_as_str()
        .any(|(key, _)| key.starts_with("SUBC_")));
    for key in CHILD_KEYS {
        assert!(pty
            .iter_full_env_as_str()
            .any(|(k, v)| k == key && v == "outer-command"));
    }
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.github.shim = false;
    config.git.co_author = "off".into();
    let mut child = environment;
    crate::sandbox_spawn::with_worker_jail(true, || {
        crate::agent_child_env::inject(&config, root.path(), &mut child, Some("general-ticket"))
    })
    .unwrap();
    assert!(!child.contains_key(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV));
    assert!(!child.contains_key(SOCKET_ENV));
    assert!(!child.contains_key(CAPABILITY_FILE_ENV));
}

#[test]
fn gh_shim_broker_missing_project_root_never_infers_repository_from_workdir() {
    let ticket = PendingTicket::issue_for_project("worker-without-project-context", None);
    let identity = crate::gh_shim_ticket::redeem(ticket.value().unwrap()).unwrap();
    assert_eq!(identity.project_root, "");
    assert_eq!(identity.read_repository, None);
}

#[tokio::test]
async fn gh_shim_broker_t4_real_listener_bounds_connections_and_read_deadlines() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("broker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let ticket = PendingTicket::issue("connection-budget-session", "/server/project");
    ticket.reserve_task("connection-budget-command");
    let (_, cancel) = ticket
        .worker_read_capability(
            socket.clone(),
            Instant::now() + Duration::from_secs(10),
            &stamp(),
        )
        .unwrap();
    let (scopes, _receiver) = mpsc::channel(1);
    let service = Service {
        relay: GhRelay::new(root.path().join("unused-connection.json")),
        scopes,
        runtime: tokio::runtime::Handle::current(),
        endpoints: Err("unused by this listener fixture".into()),
        connection_slots: Arc::new(Semaphore::new(64)),
    };
    let connection_slots = service.connection_slots.clone();
    let server = tokio::spawn(serve(
        listener,
        socket.clone(),
        service,
        Default::default(),
        cancel.subscribe(),
    ));
    let mut clients = Vec::new();
    for _ in 0..CONNECTIONS {
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        stream.write_u32(MAX_FRAME as u32).await.unwrap();
        clients.push(stream);
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while connection_slots.available_permits() != 64 - CONNECTIONS {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real accepted connections must reserve bounded global slots");
    let mut excess = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_millis(500), excess.read_u8())
        .await
        .expect("excess connection must close without waiting for frame deadline");
    assert!(closed.is_err());
    for mut client in clients {
        assert!(
            tokio::time::timeout(FRAME_DEADLINE + Duration::from_secs(1), client.read_u8())
                .await
                .unwrap()
                .is_err(),
            "slow frames must be disconnected"
        );
    }
    drop(ticket);
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn gh_shim_broker_scope_close_discards_already_forwarded_read() {
    let grant = Grant::new();
    let transport = Transport::default();
    let queries = AtomicUsize::new(0);
    let reply = dispatch(
        &grant.envelope(read("cortexkit/aft")),
        &grant.socket,
        &transport,
        |cap| crate::gh_shim_ticket::redeem_worker_read(cap, &grant.socket),
        |_| true,
        |_| {
            std::future::ready(Ok(if queries.fetch_add(1, Ordering::SeqCst) == 0 {
                Some(stamp())
            } else {
                None
            }))
        },
        &budget(),
    )
    .await;
    assert!(!reply.ok);
    assert!(reply.data.get("sentinel").is_none());
    assert_eq!(
        transport.calls.load(Ordering::SeqCst),
        1,
        "exercise the post-upstream fence, not an early refusal"
    );
}

#[test]
fn gh_shim_broker_t2_bundle_delete_and_bind_after_terminal_cannot_resurrect() {
    let root = tempfile::tempdir().unwrap();
    let task =
        crate::bash_background::persistence::allocate_task_layout(root.path(), "bundle-worker")
            .unwrap();
    let ticket = PendingTicket::issue("bundle-worker", "/server/project");
    ticket.reserve_task(&task.paths.task_id);
    let socket = root.path().join("bundle.sock");
    let (capability, _) = ticket
        .worker_read_capability(
            socket.clone(),
            Instant::now() + Duration::from_secs(60),
            &stamp(),
        )
        .unwrap();
    // An unfinished bundle has no metadata. Revocation must happen even when
    // deleting that bundle subsequently fails to resolve its on-disk record.
    let deletion = crate::bash_background::persistence::delete_task_bundle(&task.paths);
    assert_eq!(deletion.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    ticket.bind_task(&task.paths.task_id);
    assert!(crate::gh_shim_ticket::redeem_worker_read(&capability, &socket).is_none());
}

#[tokio::test]
async fn gh_shim_broker_deadline_renewal_follows_the_task_without_an_independent_ttl() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("renew.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let ticket = PendingTicket::issue("renew-worker", "/server/project");
    ticket.reserve_task("renew-worker-command");
    let (_, cancel) = ticket
        .worker_read_capability(
            socket.clone(),
            Instant::now() + Duration::from_millis(50),
            &stamp(),
        )
        .unwrap();
    let (scopes, _receiver) = mpsc::channel(1);
    let service = Service {
        relay: GhRelay::new(root.path().join("unused.json")),
        scopes,
        runtime: tokio::runtime::Handle::current(),
        endpoints: Err("unused by this listener fixture".into()),
        connection_slots: Arc::new(Semaphore::new(64)),
    };
    let server = tokio::spawn(serve(
        listener,
        socket.clone(),
        service,
        Default::default(),
        cancel.subscribe(),
    ));
    crate::gh_shim_ticket::renew_worker_read_deadline(
        "renew-worker-command",
        Instant::now() + Duration::from_secs(5),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut client = tokio::net::UnixStream::connect(&socket)
        .await
        .expect("renewed task keeps its broker beyond the first deadline");
    write_frame(&mut client, b"{}").await.unwrap();
    assert!(!read_frame(&mut client, MAX_RESPONSE)
        .await
        .unwrap()
        .is_empty());
    drop(ticket);
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn gh_shim_endpoint_cleanup_does_not_unlink_a_replacement() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.path().join("000000000001.gh-read.sock");
    let generation = crate::gh_shim_worker_endpoints::Generation::new(root.path()).unwrap();
    let (_listener, owner) = generation.bind("000000000001").unwrap();
    let ticket = PendingTicket::issue("cleanup-owner-session", "/server/project");
    ticket.reserve_task("cleanup-owner-command");
    ticket
        .worker_read_capability(
            socket.clone(),
            Instant::now() + Duration::from_secs(60),
            &stamp(),
        )
        .unwrap();
    assert!(ticket.attach_worker_read_endpoint(owner.clone()));
    drop(ticket);
    assert!(!socket.exists());
    let replacement_generation =
        crate::gh_shim_worker_endpoints::Generation::new(root.path()).unwrap();
    let (_replacement, _replacement_owner) = replacement_generation.bind("000000000001").unwrap();
    drop(owner);
    assert!(
        socket.exists(),
        "late cleanup of the old endpoint must not unlink its replacement"
    );
}

#[tokio::test]
async fn gh_shim_broker_raw_frames_enforce_worker_read_policy() {
    let grant = Grant::new();
    let transport = Transport::default();
    let query = |query: Value| json!({"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":"issues","query":query}});
    let mut attacks = vec![
        json!({"action":"github.repo.get","repository":"cortexkit/other","args":{"path":""}}),
        json!({"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":"graphql"}}),
        query(json!({"unknown_option":"value"})),
        query(json!({"q":"repo:other"})),
        query(json!({"state":"x".repeat(4097)})),
    ];
    for q in [
        "repo:other term",
        "org:cortexkit term",
        "user:someone term",
        "term OR other",
        "(term)",
    ] {
        attacks.push(
            json!({"action":"github.search.issues","repository":"cortexkit/aft","args":{"q":q}}),
        );
    }
    for request in attacks {
        let raw = serde_json::to_vec(&json!({"capability":grant.capability,"operation":{"op":"gh_shim.read_request","request":request}})).unwrap();
        let reply = call(&grant, &transport, &raw, Some(stamp())).await;
        assert!(!reply.ok, "daemon admitted raw request: {request}");
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            0,
            "refuse before any upstream call: {request}"
        );
        if request["args"]["query"].get("unknown_option").is_some()
            || request["args"]["query"].get("q").is_some()
        {
            assert_eq!(reply.data["refusal_code"], "worker_query_key_not_allowed");
            assert!(reply.data["allowed_keys"]
                .as_array()
                .unwrap()
                .iter()
                .any(|key| key == "state"));
            assert!(reply.data["message"]
                .as_str()
                .unwrap()
                .contains("Allowed keys"));
        }
    }
    let raw = serde_json::to_vec(&json!({"capability":grant.capability,"operation":{"op":"gh_shim.read_request","request":query(json!({"state":"open"}))}})).unwrap();
    assert!(call(&grant, &transport, &raw, Some(stamp())).await.ok);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn gh_shim_broker_revocation_discards_in_flight_reply() {
    struct Held {
        inner: Transport,
        reached: tokio::sync::Notify,
        resume: tokio::sync::Notify,
    }
    impl RelayTransport for Held {
        fn read_repository(
            &self,
            ticket: &crate::gh_shim_ticket::Redeemed,
        ) -> Result<String, String> {
            self.inner.read_repository(ticket)
        }
        fn opens_scoped_routes(&self) -> bool {
            true
        }
        async fn plexus(
            &self,
            root: &str,
            session: &str,
            scope: Option<&ScopeSelector>,
            body: Value,
        ) -> Result<Value, TransportError> {
            let reply = self.inner.plexus(root, session, scope, body).await;
            self.reached.notify_one();
            self.resume.notified().await;
            reply
        }
    }
    let grant = Arc::new(Grant::new());
    let held = Arc::new(Held {
        inner: Default::default(),
        reached: Default::default(),
        resume: Default::default(),
    });
    let task_grant = grant.clone();
    let task_held = held.clone();
    let task = tokio::spawn(async move {
        dispatch(
            &task_grant.envelope(read("cortexkit/aft")),
            &task_grant.socket,
            task_held.as_ref(),
            |cap| crate::gh_shim_ticket::redeem_worker_read(cap, &task_grant.socket),
            |_| true,
            |_| std::future::ready(Ok(Some(stamp()))),
            &budget(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), held.reached.notified())
        .await
        .unwrap();
    assert_eq!(held.inner.calls.load(Ordering::SeqCst), 1);
    crate::gh_shim_ticket::revoke_task(&grant.task);
    held.resume.notify_one();
    let reply = task.await.unwrap();
    assert!(
        !reply.ok,
        "a live observer must not receive the completed upstream result after revocation"
    );
    assert!(reply.data.get("sentinel").is_none());
    assert_eq!(held.inner.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn gh_shim_broker_query_keys_and_limits_are_daemon_side() {
    let grant = Grant::new();
    let transport = Transport::default();
    let envelope = |query: Value| {
        serde_json::to_vec(&json!({"capability":grant.capability,"operation":{"op":"gh_shim.read_request","request":{"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":"issues","query":query}}}})).unwrap()
    };
    assert!(
        call(
            &grant,
            &transport,
            &envelope(json!({"state":"open"})),
            Some(stamp())
        )
        .await
        .ok
    );
    for key in [
        "unreviewed",
        "q",
        "query",
        "url",
        "method",
        "headers",
        "Authorization",
    ] {
        let reply = call(
            &grant,
            &transport,
            &envelope(json!({(key):"value"})),
            Some(stamp()),
        )
        .await;
        assert!(!reply.ok, "raw worker query key {key} must be refused");
        assert_eq!(reply.data["refusal_code"], "worker_query_key_not_allowed");
        assert_eq!(reply.data["key"], key);
        assert!(reply.data["allowed_keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|key| key == "state"));
    }
    let reply = call(
        &grant,
        &transport,
        &envelope(json!({"state":"x".repeat(4097)})),
        Some(stamp()),
    )
    .await;
    assert_eq!(reply.data["refusal_code"], "worker_query_value_too_long");
    let keys = [
        "page",
        "per_page",
        "ref",
        "branch",
        "head_sha",
        "event",
        "actor",
        "status",
        "check_suite_id",
        "state",
        "labels",
        "milestone",
        "assignee",
        "creator",
        "mentioned",
        "since",
        "sort",
    ];
    let query: serde_json::Map<String, Value> = keys
        .into_iter()
        .map(|key| (key.into(), json!("1")))
        .collect();
    let reply = call(
        &grant,
        &transport,
        &envelope(Value::Object(query)),
        Some(stamp()),
    )
    .await;
    assert_eq!(reply.data["refusal_code"], "worker_query_too_many_pairs");
    assert_eq!(
        transport.calls.load(Ordering::SeqCst),
        1,
        "only the allowed key may reach upstream"
    );
}

#[tokio::test]
async fn gh_shim_broker_logs_every_admitted_read_in_one_window() {
    let grant = Grant::new();
    let transport = Transport::default();
    let limits = budget();
    let lines = Mutex::new(Vec::new());
    for _ in 0..2 {
        let reply = dispatch_recorded(
            &grant.envelope(read("cortexkit/aft")),
            &grant.socket,
            &transport,
            |cap| crate::gh_shim_ticket::redeem_worker_read(cap, &grant.socket),
            |_| true,
            |_| std::future::ready(Ok(Some(stamp()))),
            &limits,
            &|line| lines.lock().unwrap().push(line.to_string()),
        )
        .await;
        assert!(reply.ok);
    }
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        lines.lock().unwrap().len(),
        2,
        "every read, not just the first request in a rate window, needs an audit record"
    );
    assert!(lines
        .lock()
        .unwrap()
        .iter()
        .all(|line| line.contains("read=true")));
}

#[test]
fn gh_shim_broker_seatbelt_root_uses_the_shared_namespace_definition() {
    let source = include_str!("cli/sandbox_launch/seatbelt.rs");
    let helper = source
        .split("fn render_worker_broker_denies(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(
        helper.contains("managed_task_temp_root()"),
        "the network rule must use the same root as endpoint creation"
    );
    assert!(
        !helper.contains("/private/tmp/aft-w"),
        "do not duplicate a platform namespace literal in the network fence"
    );
}

#[tokio::test]
async fn gh_shim_broker_encoded_paths_are_canonicalized_before_policy() {
    let grant = Grant::new();
    let transport = Transport::default();
    for path in [
        "gr%61phql",
        "%67raphql",
        "%2567raphql",
        "graphql%2Fquery",
        "issues%2F..%2Fgraphql",
        "%2Fgraphql",
        "issues/%2e%2e/graphql",
        "issues/../graphql",
        "issues/./7",
        "issues/%00",
    ] {
        let bytes=serde_json::to_vec(&json!({"capability":grant.capability,"operation":{"op":"gh_shim.read_request","request":{"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":path}}}})).unwrap();
        let reply = call(&grant, &transport, &bytes, Some(stamp())).await;
        assert!(
            !reply.ok,
            "encoded or dot-segment worker path must be refused: {path}"
        );
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            0,
            "refuse before upstream: {path}"
        );
    }
    let bytes=serde_json::to_vec(&json!({"capability":grant.capability,"operation":{"op":"gh_shim.read_request","request":{"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":"br%61nches/feature%2ftopic"}}}})).unwrap();
    assert!(
        call(&grant, &transport, &bytes, Some(stamp())).await.ok,
        "an encoded slash in the one branch-name segment remains supported"
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        transport.requests.lock().unwrap()[0]["arguments"]["args"]["path"],
        "branches/feature%2Ftopic"
    );
}

#[tokio::test]
async fn gh_shim_broker_query_control_characters_are_refused() {
    let grant = Grant::new();
    let transport = Transport::default();
    for value in ["open\r\nX-Injected: value", "open\0", "open\u{7f}"] {
        let bytes=serde_json::to_vec(&json!({"capability":grant.capability,"operation":{"op":"gh_shim.read_request","request":{"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":"issues","query":{"state":value}}}}})).unwrap();
        let reply = call(&grant, &transport, &bytes, Some(stamp())).await;
        assert_eq!(
            reply.data["refusal_code"], "worker_query_value_invalid",
            "{reply:?}"
        );
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn gh_shim_broker_query_values_cannot_inject_parameters() {
    let grant = Grant::new();
    let transport = Transport::default();
    for value in ["open&x=y", "open=closed", "open#fragment", "open%26x%3Dy"] {
        let bytes=serde_json::to_vec(&json!({"capability":grant.capability,"operation":{"op":"gh_shim.read_request","request":{"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":"issues","query":{"state":value}}}}})).unwrap();
        let reply = call(&grant, &transport, &bytes, Some(stamp())).await;
        assert!(
            !reply.ok,
            "worker query delimiters must not reach the URL-building provider: {value}"
        );
        assert_eq!(reply.data["refusal_code"], "worker_query_value_ambiguous");
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    }
}
