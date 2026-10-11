//! Run lifetime tests use real bash children and daemon scope.describe replies
//! that report whether a run is live, ended, or absent after owner synchronization.

use super::*;
use std::path::Path;

static OBSERVED_TASK: OnceLock<Mutex<Option<(aft::bash_background::BgTaskRegistry, String)>>> =
    OnceLock::new();

fn observing_dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    let bash = req.command == "bash";
    let response = bridge_dispatch(req, ctx);
    if bash {
        if let Some(task) = response.data.get("task_id").and_then(Value::as_str) {
            *OBSERVED_TASK
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap() = Some((ctx.bash_background().clone(), task.into()));
        }
    }
    response
}

fn assert_run_not_killing() {
    let (registry, task) = OBSERVED_TASK
        .get()
        .unwrap()
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    let snapshot = registry
        .status(&task, "run-session", None, None, 0)
        .unwrap();
    assert_eq!(
        snapshot.info.status,
        BgTaskStatus::Running,
        "unconfirmed scope close must not start a kill"
    );
}

async fn assert_scope_kill_settled() {
    let (registry, task) = OBSERVED_TASK
        .get()
        .unwrap()
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = registry
            .status(&task, "run-session", None, None, 0)
            .unwrap();
        if snapshot.info.status.is_terminal() {
            assert_eq!(snapshot.info.status, BgTaskStatus::Killed);
            assert_eq!(
                snapshot.info.status_reason.as_deref(),
                Some("scope revoked")
            );
            assert!(
                aft::commands::bash_orchestrate::format_foreground_result(&snapshot)
                    .contains("killed (scope revoked)")
            );
            return;
        }
        assert!(Instant::now() < deadline, "scope kill did not settle");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn stamp() -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind":"reserved", "module_id":"prefrontal-core"},
        "ref":"run-scope", "scope_epoch":1, "kind":"worker",
        "attributes":{"agent_id":"agent-a", "run_id":"run-a"},
        "owner_authorized":true,
        "expires_at_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 + 30000
    }))
    .unwrap()
}

async fn bind(stream: &mut tokio::net::TcpStream, root: &Path, channel: u16, run: bool) {
    let mut scope = stamp();
    if !run {
        scope.attributes.run_id = None;
    }
    bind_mode(
        stream,
        root,
        channel,
        Some(if run {
            subc_protocol::call_mode::CallMode::Codemode
        } else {
            subc_protocol::call_mode::CallMode::Classic
        }),
        Some(scope),
    )
    .await;
}

async fn bind_mode(
    stream: &mut tokio::net::TcpStream,
    root: &Path,
    channel: u16,
    mode: Option<subc_protocol::call_mode::CallMode>,
    scope: Option<subc_protocol::scope::ScopeStamp>,
) {
    let config_path = root.join(".cortexkit/aft.jsonc");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        config_path,
        serde_json::to_vec(&json!({
            "bash":{"foreground_wait_window_ms":10}
        }))
        .unwrap(),
    )
    .unwrap();
    let body = ModuleControlRequest::RouteBind {
        route_channel: channel,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".into(),
        },
        identity: BindIdentity::new(root, "opencode", "run-session"),
        principal: Some(Principal::Direct),
        consumer_capabilities: None,
        admission_facts: Default::default(),
        scope,
        role_versions: None,
        call_mode: mode,
    };
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            channel as u64,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await;
    expect_route_bind_ack(stream, channel as u64).await;
}

async fn call(stream: &mut tokio::net::TcpStream, channel: u16, corr: u64, args: Value) {
    named_call(stream, channel, corr, "bash", args).await;
}

async fn named_call(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    name: &str,
    args: Value,
) {
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            control_flags(),
            channel,
            1,
            corr,
            serde_json::to_vec(&json!({"name":name, "preset":"head", "arguments":args})).unwrap(),
        )
        .unwrap(),
    )
    .await;
}

#[test]
fn call_modes_refuse_flow_writes_and_unknown_but_serve_read_and_classic() {
    run_subc_bridge_test(
        "mode calls",
        Duration::from_secs(30),
        |input| async move {
            use subc_protocol::call_mode::CallMode;
            let mut daemon = open_fake_daemon_session(input).await;
            std::fs::write(daemon.root1.join("mode.txt"), "mode-read-ok").unwrap();
            let mut flow = stamp();
            flow.attributes.flow_id = Some("mode-flow".into());
            flow.attributes.run_id = None;
            flow.expires_at_ms = None;
            bind_mode(
                &mut daemon.stream,
                &daemon.root1,
                1,
                Some(CallMode::Flow),
                Some(flow),
            )
            .await;
            for (corr, name, args) in [
                (100, "bash", json!({"command":"echo forbidden"})),
                (101, "edit", json!({"filePath":"mode.txt","edits":[]})),
                (102, "inspect", json!({})),
            ] {
                named_call(&mut daemon.stream, 1, corr, name, args).await;
                let reply = read_frame_timeout(&mut daemon.stream, "mode refusal").await;
                assert_eq!(reply.header.ty, FrameType::Error);
                let error: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
                assert_eq!(
                    error.code,
                    cortexkit_role_tool_provider::errors::CALL_MODE_UNSUPPORTED
                );
                assert_eq!(error.detail, Some(json!({"tool":name,"mode":"flow"})));
            }
            named_call(
                &mut daemon.stream,
                1,
                103,
                "read",
                json!({"filePath":"mode.txt"}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "flow read").await;
            assert_eq!(
                reply.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&reply.body)
            );
            assert!(String::from_utf8_lossy(&reply.body).contains("mode-read-ok"));
            bind_mode(
                &mut daemon.stream,
                &daemon.root1,
                2,
                Some(CallMode::Unknown("future".into())),
                None,
            )
            .await;
            named_call(
                &mut daemon.stream,
                2,
                104,
                "read",
                json!({"filePath":"mode.txt"}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "unknown mode").await;
            let error: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(
                error.code,
                cortexkit_role_tool_provider::errors::CALL_MODE_UNSUPPORTED
            );
            assert_eq!(error.detail, Some(json!({"tool":"read","mode":"future"})));
            bind_mode(&mut daemon.stream, &daemon.root1, 3, None, Some(stamp())).await;
            call(
                &mut daemon.stream,
                3,
                105,
                json!({"command":"echo classic-ok","timeout":1000}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "classic bash").await;
            assert_eq!(reply.header.ty, FrameType::Response);
            assert!(String::from_utf8_lossy(&reply.body).contains("classic-ok"));
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
    );
}

#[test]
fn codemode_bash_refuses_missing_bind_deadline() {
    run_subc_bridge_test(
        "missing run deadline",
        Duration::from_secs(30),
        |input| async move {
            let mut daemon = open_fake_daemon_session(input).await;
            let scope = stamp().with_expires_at_ms(None);
            bind_mode(
                &mut daemon.stream,
                &daemon.root1,
                1,
                Some(subc_protocol::call_mode::CallMode::Codemode),
                Some(scope),
            )
            .await;
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":"echo must-not-run","timeout":2000}),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "missing deadline refusal").await;
            assert_eq!(reply.header.ty, FrameType::Error);
            let error: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(error.code, "run_deadline_missing");
            assert_eq!(error.detail, Some(json!({"tool":"bash","mode":"codemode"})));
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
    );
}

static OBSERVED_TIMEOUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn timeout_observing_dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    if req.command == "bash" {
        OBSERVED_TIMEOUT.store(
            req.params["timeout"].as_u64().unwrap(),
            std::sync::atomic::Ordering::Release,
        );
    }
    bridge_dispatch(req, ctx)
}

#[test]
fn codemode_bash_clamps_requested_timeout_to_bind_deadline() {
    run_subc_bridge_test_with_dispatch(
        "run deadline clamp",
        Duration::from_secs(30),
        |input| async move {
            let mut daemon = open_fake_daemon_session(input).await;
            let mut scope = stamp();
            scope.expires_at_ms = Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    + 5000,
            );
            bind_mode(
                &mut daemon.stream,
                &daemon.root1,
                1,
                Some(subc_protocol::call_mode::CallMode::Codemode),
                Some(scope),
            )
            .await;
            OBSERVED_TIMEOUT.store(0, std::sync::atomic::Ordering::Release);
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":"echo clamped","timeout":20000}),
            )
            .await;
            let request = describe(&mut daemon.stream).await;
            answer(
                &mut daemon.stream,
                &request,
                subc_protocol::scope::ScopeStatus::Live,
                true,
                Some(1),
            )
            .await;
            let reply = read_frame_timeout(&mut daemon.stream, "clamped bash").await;
            assert_eq!(reply.header.ty, FrameType::Response);
            let timeout = OBSERVED_TIMEOUT.load(std::sync::atomic::Ordering::Acquire);
            assert!(
                (1..=5000).contains(&timeout),
                "timeout was {timeout}, must be clamped to bind deadline"
            );
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        timeout_observing_dispatch,
    );
}

async fn describe(stream: &mut tokio::net::TcpStream) -> Frame {
    let request = read_frame_timeout(stream, "scope.describe").await;
    assert_eq!(request.header.ty, FrameType::Request);
    assert_eq!(request.header.channel, 0);
    let body: subc_protocol::session::ModuleControlRequestFromModule =
        serde_json::from_slice(&request.body).unwrap();
    assert!(
        matches!(body, subc_protocol::session::ModuleControlRequestFromModule::ScopeDescribe {owner, scope_ref}
        if owner == stamp().owner && scope_ref == "run-scope")
    );
    request
}

async fn answer(
    stream: &mut tokio::net::TcpStream,
    request: &Frame,
    status: subc_protocol::scope::ScopeStatus,
    synced: bool,
    epoch: Option<u64>,
) {
    let reply = subc_protocol::session::ModuleControlResponseToModule::ScopeDescribe {
        status,
        scope_epoch: epoch,
        daemon_incarnation: "daemon-a".into(),
        owner_synced: synced,
        owner_configured: true,
        scope: (status == subc_protocol::scope::ScopeStatus::Live).then(|| {
            let mut scope = stamp();
            scope.scope_epoch = epoch.unwrap();
            scope
        }),
    };
    send_frame(
        stream,
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            request.header.corr,
            serde_json::to_vec(&reply).unwrap(),
        )
        .unwrap(),
    )
    .await;
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    if std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, suffix)| suffix.starts_with('Z'))
    }) {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
async fn child_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "bash did not publish grandchild pid"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
async fn wait_dead(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(
            Instant::now() < deadline,
            "grandchild {pid} survived the scope end"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
struct ChildCleanup(u32);
#[cfg(unix)]
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0 as i32, libc::SIGKILL);
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum CloseCase {
    Ended,
    NewEpoch,
    NotLive,
    NotSynced,
    SlowResync,
    Error,
    Timeout,
    Duplicate,
    NonRun,
}

#[cfg(unix)]
fn close_case(name: &'static str, case: CloseCase) {
    run_subc_bridge_test_with_dispatch(
        name,
        Duration::from_secs(30),
        move |input| async move {
            use subc_protocol::scope::ScopeStatus;
            let mut daemon = open_fake_daemon_session(input).await;
            let run = !matches!(case, CloseCase::NonRun);
            bind(&mut daemon.stream, &daemon.root1, 1, run).await;
            if matches!(case, CloseCase::Duplicate) {
                bind(&mut daemon.stream, &daemon.root1, 2, true).await;
            }
            let pid_path = daemon.root1.join("grandchild.pid");
            let command = format!(
                "(trap '' TERM; sleep 30) & echo $! > '{}'; wait",
                pid_path.display()
            );
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":command,"timeout":20000,"wait":true}),
            )
            .await;
            if run {
                let request = describe(&mut daemon.stream).await;
                // The daemon confirms the same live epoch before bash starts;
                // the bind's deadline remains the upper bound on its timeout.
                answer(
                    &mut daemon.stream,
                    &request,
                    ScopeStatus::Live,
                    true,
                    Some(1),
                )
                .await;
            }
            let pid = child_pid(&pid_path).await;
            let _cleanup = ChildCleanup(pid);
            assert!(
                alive(pid),
                "control: grandchild must be live before closing the scope"
            );
            // Close the unused sibling first: it must already share the run's
            // lifetime state even though it has never made a tool call.
            let first_close = if matches!(case, CloseCase::Duplicate) {
                2
            } else {
                1
            };
            send_route_goodbye(&mut daemon.stream, first_close, 200).await;
            if matches!(case, CloseCase::NonRun) {
                // A non-run route still neither describes its scope nor kills its
                // foreground task merely because its carrier closed the route.
                assert!(read_frame_within(
                    &mut daemon.stream,
                    Duration::from_millis(300),
                    "non-run close"
                )
                .await
                .is_none());
                assert!(alive(pid));
            } else {
                let request = describe(&mut daemon.stream).await;
                assert!(alive(pid), "route close alone must not kill");
                if matches!(case, CloseCase::Duplicate) {
                    send_route_goodbye(&mut daemon.stream, 1, 201).await;
                    assert!(
                        read_frame_within(
                            &mut daemon.stream,
                            Duration::from_millis(300),
                            "duplicate describe"
                        )
                        .await
                        .is_none(),
                        "duplicate closes must share one in-flight describe"
                    );
                }
                match case {
                    CloseCase::NotSynced => {
                        answer(
                            &mut daemon.stream,
                            &request,
                            ScopeStatus::NotLive,
                            false,
                            None,
                        )
                        .await;
                        let retry = describe(&mut daemon.stream).await;
                        assert!(alive(pid), "not-synced must hold work");
                        assert_run_not_killing();
                        answer(
                            &mut daemon.stream,
                            &retry,
                            ScopeStatus::Ended,
                            true,
                            Some(1),
                        )
                        .await;
                    }
                    CloseCase::SlowResync => {
                        let mut request = request;
                        for attempt in 0..3 {
                            answer(
                                &mut daemon.stream,
                                &request,
                                ScopeStatus::NotLive,
                                false,
                                None,
                            )
                            .await;
                            request = read_frame_within(
                                &mut daemon.stream,
                                Duration::from_secs(5),
                                "confirmation after an inconclusive reply",
                            )
                            .await
                            .expect("scope confirmation stopped after three inconclusive replies");
                            assert_eq!(request.header.ty, FrameType::Request);
                            assert_eq!(request.header.channel, 0);
                            let body: Value = serde_json::from_slice(&request.body).unwrap();
                            assert_eq!(body["op"], "scope.describe");
                            assert!(alive(pid), "inconclusive reply {attempt} must hold work");
                            assert_run_not_killing();
                        }
                        answer(
                            &mut daemon.stream,
                            &request,
                            ScopeStatus::Ended,
                            true,
                            Some(1),
                        )
                        .await;
                    }
                    CloseCase::Error => {
                        send_frame(
                            &mut daemon.stream,
                            Frame::build(
                                FrameType::Error,
                                control_flags(),
                                0,
                                0,
                                request.header.corr,
                                serde_json::to_vec(&subc_protocol::ErrorBody::new(
                                    "unavailable",
                                    "not ready",
                                ))
                                .unwrap(),
                            )
                            .unwrap(),
                        )
                        .await;
                        let retry = describe(&mut daemon.stream).await;
                        assert!(alive(pid), "describe error must hold work");
                        assert_run_not_killing();
                        answer(
                            &mut daemon.stream,
                            &retry,
                            ScopeStatus::Ended,
                            true,
                            Some(1),
                        )
                        .await;
                    }
                    CloseCase::Timeout => {
                        let retry = describe(&mut daemon.stream).await;
                        assert!(alive(pid), "describe timeout must hold work");
                        assert_run_not_killing();
                        answer(
                            &mut daemon.stream,
                            &retry,
                            ScopeStatus::Ended,
                            true,
                            Some(1),
                        )
                        .await;
                    }
                    CloseCase::NewEpoch => {
                        answer(
                            &mut daemon.stream,
                            &request,
                            ScopeStatus::Live,
                            true,
                            Some(2),
                        )
                        .await
                    }
                    CloseCase::NotLive => {
                        answer(
                            &mut daemon.stream,
                            &request,
                            ScopeStatus::NotLive,
                            true,
                            None,
                        )
                        .await
                    }
                    _ => {
                        answer(
                            &mut daemon.stream,
                            &request,
                            ScopeStatus::Ended,
                            true,
                            Some(1),
                        )
                        .await
                    }
                }
                wait_dead(pid).await;
                assert_scope_kill_settled().await;
                if matches!(case, CloseCase::Duplicate) {
                    assert!(
                        read_frame_within(
                            &mut daemon.stream,
                            Duration::from_millis(300),
                            "queued duplicate describe"
                        )
                        .await
                        .is_none(),
                        "duplicate closes must not queue another describe after the first finishes"
                    );
                }
            }
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        observing_dispatch,
    );
}

#[cfg(unix)]
#[test]
fn scope_revocation_ended_kills_grandchild_group() {
    close_case("scope-ended-group", CloseCase::Ended);
}
#[cfg(unix)]
#[test]
fn scope_revocation_live_new_epoch_kills_group() {
    close_case("scope-new-epoch", CloseCase::NewEpoch);
}
#[cfg(unix)]
#[test]
fn scope_revocation_not_live_synced_kills_group() {
    close_case("scope-not-live", CloseCase::NotLive);
}
#[cfg(unix)]
#[test]
fn scope_revocation_not_synced_holds_group() {
    close_case("scope-not-synced", CloseCase::NotSynced);
}
#[cfg(unix)]
#[test]
fn scope_revocation_describe_error_holds_group() {
    close_case("scope-describe-error", CloseCase::Error);
}
#[cfg(unix)]
#[test]
fn scope_revocation_describe_timeout_holds_group() {
    close_case("scope-describe-timeout", CloseCase::Timeout);
}
#[cfg(unix)]
#[test]
fn scope_revocation_duplicate_closes_share_describe() {
    close_case("scope-duplicate-close", CloseCase::Duplicate);
}
#[cfg(unix)]
#[test]
fn scope_revocation_non_run_route_close_is_unchanged() {
    close_case("scope-non-run-close", CloseCase::NonRun);
}

#[test]
fn scope_revocation_run_background_refused_before_spawn() {
    run_subc_bridge_test_with_dispatch(
        "run-background-refused",
        Duration::from_secs(15),
        |input| async move {
            let mut daemon = open_fake_daemon_session(input).await;
            bind(&mut daemon.stream, &daemon.root1, 1, true).await;
            let marker = daemon.root1.join("must-not-exist");
            for (corr, option) in [(100, "background"), (101, "pty")] {
                let mut args = json!({"command":format!("touch '{}'", marker.display())});
                args[option] = json!(true);
                call(&mut daemon.stream, 1, corr, args).await;
                let response =
                    read_frame_timeout(&mut daemon.stream, "named background refusal").await;
                assert_eq!(response.header.ty, FrameType::Error);
                let error: ErrorBody = serde_json::from_slice(&response.body).unwrap();
                assert_eq!(error.code, "run_background_refused");
                assert!(error.message.contains("codemode runs can't background"));
                assert!(!marker.exists());
            }
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

#[test]
fn scope_revocation_unreadable_run_record_refused_before_spawn() {
    run_subc_bridge_test_with_dispatch(
        "run-record-refused",
        Duration::from_secs(15),
        |input| async move {
            let mut daemon = open_fake_daemon_session(input).await;
            bind(&mut daemon.stream, &daemon.root1, 1, true).await;
            let marker = daemon.root1.join("must-not-exist");
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":format!("touch '{}'", marker.display())}),
            )
            .await;
            let request = describe(&mut daemon.stream).await;
            send_frame(
                &mut daemon.stream,
                Frame::build(
                    FrameType::Error,
                    control_flags(),
                    0,
                    0,
                    request.header.corr,
                    serde_json::to_vec(&ErrorBody::new("unavailable", "cannot read record"))
                        .unwrap(),
                )
                .unwrap(),
            )
            .await;
            let response =
                read_frame_timeout(&mut daemon.stream, "unreadable record refusal").await;
            assert_eq!(response.header.ty, FrameType::Error);
            let error: ErrorBody = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(error.code, "run_scope_unavailable");
            assert!(!marker.exists());
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

#[test]
fn scope_revocation_codemode_watch_not_served() {
    run_subc_bridge_test_with_dispatch(
        "run-watch-background",
        Duration::from_secs(15),
        |input| async move {
            let mut daemon = open_fake_daemon_session(input).await;
            bind(&mut daemon.stream, &daemon.root1, 1, true).await;
            send_frame(&mut daemon.stream, Frame::build(FrameType::Request, control_flags(), 1, 1, 100,
            serde_json::to_vec(&json!({"name":"bash_watch", "preset":"worker", "arguments":{"taskId":"absent", "background":true}})).unwrap()).unwrap()).await;
            let response = read_frame_timeout(&mut daemon.stream, "watch background refusal").await;
            assert_eq!(response.header.ty, FrameType::Error);
            let error: ErrorBody = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(
                error.code,
                cortexkit_role_tool_provider::errors::CALL_MODE_UNSUPPORTED
            );
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

#[test]
fn scope_revocation_non_run_background_is_unchanged() {
    run_subc_bridge_test_with_dispatch(
        "non-run-background",
        Duration::from_secs(15),
        |input| async move {
            let mut daemon = open_fake_daemon_session(input).await;
            bind(&mut daemon.stream, &daemon.root1, 1, false).await;
            let marker = daemon.root1.join("background-ran");
            call(&mut daemon.stream, 1, 100, json!({"command":format!("sleep 0.05; printf done > '{}'", marker.display()),"background":true})).await;
            let response = read_frame_timeout(&mut daemon.stream, "normal background launch").await;
            assert_eq!(response.header.ty, FrameType::Response);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !marker.exists() {
                assert!(
                    Instant::now() < deadline,
                    "non-run background command did not run"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

#[test]
fn scope_revocation_record_timeout_refuses_without_blocking_frames() {
    run_subc_bridge_test_with_dispatch(
        "run-record-timeout",
        Duration::from_secs(15),
        |input| async move {
            let mut daemon = open_fake_daemon_session(input).await;
            bind(&mut daemon.stream, &daemon.root1, 1, true).await;
            let marker = daemon.root1.join("must-not-exist");
            call(
                &mut daemon.stream,
                1,
                100,
                json!({"command":format!("touch '{}'", marker.display())}),
            )
            .await;
            let _request = describe(&mut daemon.stream).await;
            send_frame(
                &mut daemon.stream,
                Frame::build(FrameType::Ping, control_flags(), 0, 0, 500, Vec::new()).unwrap(),
            )
            .await;
            let pong = read_frame_within(
                &mut daemon.stream,
                Duration::from_secs(1),
                "pong while scope read waits",
            )
            .await
            .expect("scope read blocked the frame loop");
            assert_eq!(pong.header.ty, FrameType::Pong);
            let response = read_frame_timeout(&mut daemon.stream, "record timeout refusal").await;
            assert_eq!(response.header.ty, FrameType::Error);
            let error: ErrorBody = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(error.code, "run_scope_unavailable");
            assert!(!marker.exists());
            send_connection_goodbye(&mut daemon.stream).await;
        },
        |_, _, _| {},
        bridge_dispatch,
    );
}

#[cfg(unix)]
#[test]
fn scope_revocation_three_inconclusive_then_ended_kills_group() {
    close_case("scope-slow-owner-resync", CloseCase::SlowResync);
}
