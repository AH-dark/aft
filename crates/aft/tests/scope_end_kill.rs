//! Real daemon frames and real shell processes exercise scope lifetime.

use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use subc_protocol::session::ModuleControlRequest;
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, Principal, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_server,
    connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION},
    read_frame, write_frame,
};
use tokio::{net::TcpListener, sync::mpsc};

#[path = "helpers/aft_binary.rs"]
mod aft_binary;

struct Daemon {
    root: PathBuf,
    child: Child,
    writer: tokio::net::tcp::OwnedWriteHalf,
    frames: mpsc::UnboundedReceiver<Frame>,
    reader: tokio::task::JoinHandle<()>,
    parked: VecDeque<Frame>,
    corr: u64,
    controls: Vec<(u64, String)>,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.reader.abort();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn flags() -> Flags {
    Flags::new(false, Priority::Interactive, false)
}
fn body(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).unwrap_or(Value::Null)
}
fn stamp(owner: &str, epoch: u64) -> subc_protocol::scope::ScopeStamp {
    subc_protocol::scope::ScopeStamp::new(
        Principal::Reserved {
            module_id: owner.into(),
        },
        "same-ref",
        epoch,
        subc_protocol::scope::ScopeKind::Head,
    )
    .with_owner_authorized(true)
}
fn sleep_command(seconds: u64) -> String {
    #[cfg(windows)]
    return format!("Start-Sleep -Seconds {seconds}");
    #[cfg(not(windows))]
    format!("sleep {seconds}")
}
impl Daemon {
    async fn spawn(root: &Path, ops: &[&str]) -> Self {
        let config = root.join("project/.cortexkit");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("aft.jsonc"),
            serde_json::to_vec(
                &json!({"indexes":{"callgraph":false,"trigram":false,"semantic":false}}),
            )
            .unwrap(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: vec![0x42; subc_transport::KEY_LEN],
            daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
            pid: std::process::id(),
            daemon_ver: "scope-tests".into(),
        };
        let connection_path = root.join("connection.json");
        connection_file::write_atomic(&connection_path, &connection).unwrap();
        let mut command = Command::new(aft_binary::aft_binary());
        command
            .current_dir(root)
            .arg("--subc")
            .arg(&connection_path)
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .env_remove("AFT_STORAGE_DIR")
            .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
            .env("AFT_TEST_SCOPE_POLL_MS", "500")
            .env("AFT_TEST_SCOPE_DESCRIBE_TIMEOUT_MS", "200")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (variable, directory) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            let path = root.join(directory);
            std::fs::create_dir_all(&path).unwrap();
            command.env(variable, path);
        }
        let child = command.spawn().unwrap();
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .unwrap()
            .unwrap();
        authenticate_server(
            &mut socket,
            &[0x42; subc_transport::KEY_LEN],
            &[0x24; subc_transport::DAEMON_ID_LEN],
            "scope-tests",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let hello = read_frame(&mut socket).await.unwrap().unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: ops.iter().map(|op| (*op).into()).collect(),
            subc_capabilities: vec![],
            storage: None,
            machine_id: None,
        };
        write_frame(
            &mut socket,
            &Frame::build(
                FrameType::HelloAck,
                flags(),
                0,
                0,
                hello.header.corr,
                serde_json::to_vec(&ack).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let (mut read, writer) = socket.into_split();
        let (tx, frames) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            while let Ok(Some(frame)) = read_frame(&mut read).await {
                if tx.send(frame).is_err() {
                    break;
                }
            }
        });
        Self {
            root: root.into(),
            child,
            writer,
            frames,
            reader,
            parked: VecDeque::new(),
            corr: 100,
            controls: vec![],
        }
    }
    async fn send(
        &mut self,
        ty: FrameType,
        channel: u16,
        epoch: u32,
        corr: u64,
        value: Option<Value>,
    ) {
        let bytes = value
            .map(|value| serde_json::to_vec(&value).unwrap())
            .unwrap_or_default();
        write_frame(
            &mut self.writer,
            &Frame::build(ty, flags(), channel, epoch, corr, bytes).unwrap(),
        )
        .await
        .unwrap();
    }
    async fn next_matching(
        &mut self,
        within: Duration,
        matches: impl Fn(&Frame) -> bool,
    ) -> Option<Frame> {
        if let Some(index) = self.parked.iter().position(&matches) {
            return self.parked.remove(index);
        }
        let until = tokio::time::Instant::now() + within;
        loop {
            let frame = tokio::time::timeout_at(until, self.frames.recv())
                .await
                .ok()??;
            if frame.header.ty == FrameType::Ping {
                self.send(FrameType::Pong, 0, 0, frame.header.corr, None)
                    .await;
                continue;
            }
            if frame.header.ty == FrameType::Push {
                continue;
            }
            if frame.header.ty == FrameType::Request && frame.header.channel == 0 {
                let op = body(&frame)["op"].as_str().unwrap_or("").to_owned();
                self.controls.push((frame.header.corr, op.clone()));
                if matches!(op.as_str(), "supervisor.live_roots" | "catalog.update") {
                    let response = if op == "supervisor.live_roots" {
                        json!({"op":"supervisor.live_roots","roots":[],"unknown_root_bindings":0,"total_bindings":0})
                    } else {
                        json!({"op":"catalog.update"})
                    };
                    self.send(FrameType::Response, 0, 0, frame.header.corr, Some(response))
                        .await;
                    continue;
                }
            }
            if matches(&frame) {
                return Some(frame);
            }
            self.parked.push_back(frame);
        }
    }
    async fn bind(
        &mut self,
        channel: u16,
        bind_epoch: u32,
        principal: &str,
        scope: Option<subc_protocol::scope::ScopeStamp>,
        legacy: bool,
    ) {
        self.corr += 1;
        let corr = self.corr;
        let bind = ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: bind_epoch,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: BindIdentity::new(
                &self.root.join("project"),
                "runner",
                &format!("session-{channel}"),
            ),
            principal: Some(if principal == "direct" {
                Principal::Direct
            } else {
                Principal::Reserved {
                    module_id: principal.into(),
                }
            }),
            consumer_capabilities: Some(vec!["elicitation".into()]),
            admission_facts: None,
            scope,
            role_versions: (!legacy)
                .then(|| BTreeMap::from([("tool-provider".into(), "v1".into())])),
            call_mode: None,
        };
        self.send(
            FrameType::Request,
            0,
            0,
            corr,
            Some(serde_json::to_value(bind).unwrap()),
        )
        .await;
        let ack = self
            .next_matching(Duration::from_secs(15), |frame| {
                frame.header.channel == 0
                    && frame.header.corr == corr
                    && frame.header.ty != FrameType::Request
            })
            .await
            .expect("bind reply");
        assert_eq!(ack.header.ty, FrameType::Response, "{}", body(&ack));
        if !legacy {
            // Bind acknowledgement precedes asynchronous aft.db opening.
            // late_results must become available before submitting shell calls;
            // this readiness probe consumes no call key or ledger row.
            let until = tokio::time::Instant::now() + Duration::from_secs(15);
            loop {
                let corr = self
                    .start(
                        channel,
                        bind_epoch,
                        json!({"name":"late_results","arguments":{}}),
                    )
                    .await;
                let ready = self.reply(channel, corr).await;
                if ready.header.ty == FrameType::Response {
                    break;
                }
                assert_eq!(
                    body(&ready)["code"],
                    "database_initializing",
                    "{}",
                    body(&ready)
                );
                assert!(tokio::time::Instant::now() < until);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    async fn start(&mut self, channel: u16, epoch: u32, request: Value) -> u64 {
        self.corr += 1;
        let corr = self.corr;
        self.send(FrameType::Request, channel, epoch, corr, Some(request))
            .await;
        corr
    }
    async fn reply(&mut self, channel: u16, corr: u64) -> Frame {
        self.next_matching(Duration::from_secs(15), |frame| {
            frame.header.channel == channel
                && frame.header.corr == corr
                && matches!(frame.header.ty, FrameType::Error | FrameType::Response)
        })
        .await
        .expect("terminal reply")
    }
    async fn call(&mut self, channel: u16, request: Value) -> Frame {
        let corr = self.start(channel, 1, request).await;
        self.reply(channel, corr).await
    }
    async fn ask(&mut self, channel: u16) -> Frame {
        self.next_matching(Duration::from_secs(10), |frame| {
            frame.header.channel == channel && frame.header.ty == FrameType::Request
        })
        .await
        .expect("permission question")
    }
    async fn allow(&mut self, channel: u16) {
        let ask = self.ask(channel).await;
        self.send(
            FrameType::Response,
            channel,
            ask.header.epoch,
            ask.header.corr,
            Some(json!({"decision":"allow"})),
        )
        .await;
    }
    #[cfg(feature = "test-timing-hooks")]
    async fn describe(&mut self) -> Frame {
        self.next_matching(Duration::from_secs(3), |frame| {
            frame.header.channel == 0
                && frame.header.ty == FrameType::Request
                && body(frame)["op"] == "scope.describe"
        })
        .await
        .expect("scope.describe")
    }
    #[cfg(feature = "test-timing-hooks")]
    async fn answer(&mut self, request: &Frame, status: &str, synced: bool, epoch: u64) {
        self.send(FrameType::Response, 0, 0, request.header.corr, Some(json!({"op":"scope.describe", "status":status,"scope_epoch":epoch,"owner_synced":synced,"owner_configured":true,"daemon_incarnation":"test-daemon"}))).await;
    }
    #[cfg(feature = "test-timing-hooks")]
    async fn goodbye(&mut self, channel: u16, epoch: u32) {
        self.send(FrameType::Goodbye, channel, epoch, 0, None).await;
    }
    async fn drain(&mut self) {
        self.send(FrameType::Push, 0, 0, 0, Some(json!({"op":"module.draining","reason":"restart","deadline_ms":aft::bash_background::persistence::unix_millis()+5000}))).await;
    }
    async fn status(&mut self, channel: u16, task: &str) -> String {
        let frame = self
            .call(
                channel,
                json!({"name":"bash_status","preset":"head","arguments":{"taskId":task}}),
            )
            .await;
        let value = body(&frame);
        value["content"][0]["text"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string())
    }
    async fn wait_status(&mut self, channel: u16, task: &str, expected: &str) -> String {
        let until = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let text = self.status(channel, task).await;
            if text.contains(expected) {
                return text;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "expected {expected}: {text}"
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
}
fn shell(key: Option<&str>, background: bool, seconds: u64) -> Value {
    let mut call = json!({"name":"bash","preset":"head","arguments":{"command":sleep_command(seconds),"background":background,"timeout":20000}});
    if let Some(key) = key {
        call["call_key"] = json!(key);
    }
    call
}

fn gated_shell(root: &Path, key: Option<&str>) -> Value {
    let mut call = shell(key, false, 15);
    let release = root
        .join("project/release")
        .display()
        .to_string()
        .replace('\\', "/");
    #[cfg(windows)]
    let command = format!(
        "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 50 }}",
        release.replace('\'', "''")
    );
    #[cfg(not(windows))]
    let command = format!(
        "while [ ! -f '{}' ]; do sleep 0.05; done",
        release.replace('\'', "'\\''")
    );
    call["arguments"]["command"] = json!(command);
    call
}

#[tokio::test]
async fn drain_detaches_keyed_and_keyless_calls_and_retains_only_real_exit() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    daemon.bind(2, 1, "direct", None, false).await;
    let keyed = daemon
        .start(1, 1, gated_shell(&root, Some("drained")))
        .await;
    daemon.allow(1).await;
    let plain = daemon.start(2, 1, gated_shell(&root, None)).await;
    running_task(&root, "session-1").await;
    running_task(&root, "session-2").await;
    daemon.drain().await;
    let mut tasks = vec![];
    for (channel, corr, late) in [(1, keyed, true), (2, plain, false)] {
        let reply = daemon.reply(channel, corr).await;
        assert_eq!(reply.header.ty, FrameType::Error, "{}", body(&reply));
        let response = body(&reply);
        assert_eq!(response["code"], "detached", "{response}");
        assert_eq!(response["message"], "call detached; command still running");
        assert_eq!(response["detail"]["late_results"], late);
        tasks.push(response["detail"]["task_id"].as_str().unwrap().to_owned());
    }
    for task in &tasks {
        let records = task_metadata(&root);
        let record = &records
            .iter()
            .find(|(_, record)| record["task_id"] == *task)
            .unwrap()
            .1;
        assert_eq!(record["status"], "running", "{record}");
    }
    assert!(daemon
        .next_matching(Duration::from_millis(100), |frame| frame.header.ty
            == FrameType::Request
            && frame.header.channel == 0)
        .await
        .is_none());
    std::fs::write(root.join("project/release"), "release").unwrap();
    drop(daemon);
    let mut daemon = Daemon::spawn(&root, &[]).await;
    daemon
        .bind(1, 1, "direct", Some(stamp("owner", 1)), false)
        .await;
    daemon.bind(2, 1, "direct", None, false).await;
    daemon
        .bind(3, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    for (index, task) in tasks.iter().enumerate() {
        daemon
            .wait_status(index as u16 + 1, task, "completed (exit 0)")
            .await;
    }
    let page = body(
        &daemon
            .call(3, json!({"name":"late_results","arguments":{}}))
            .await,
    );
    assert_eq!(page["entries"].as_array().unwrap().len(), 1, "{page}");
    assert_eq!(page["entries"][0]["call_key"], "drained");
    assert_eq!(page["entries"][0]["kind"], "result");
    assert!(
        !page.to_string().contains("\"code\":\"detached\""),
        "{page}"
    );
    daemon
        .call(
            3,
            json!({"name":"late_results.ack","arguments":{"through":page["cursor"]}}),
        )
        .await;
    let page = body(
        &daemon
            .call(3, json!({"name":"late_results","arguments":{}}))
            .await,
    );
    assert_eq!(page["entries"], json!([]));
}

fn task_metadata(root: &Path) -> Vec<(PathBuf, Value)> {
    fn visit(directory: &Path, out: &mut Vec<(PathBuf, Value)>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, out);
            } else if path.file_name().and_then(|name| name.to_str()) == Some("metadata.json") {
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                        out.push((path, value));
                    }
                }
            }
        }
    }
    let mut tasks = vec![];
    visit(root, &mut tasks);
    tasks
}
async fn running_task(root: &Path, session: &str) -> (PathBuf, Value) {
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(task) = task_metadata(root)
            .into_iter()
            .find(|(_, task)| task["session_id"] == session && task["status"] == "running")
        {
            return task;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "running task for {session} never persisted: {:?}",
            task_metadata(root)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn goodbye_ended_kills_exact_owner_and_refuses_prepared_call() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "owner-a", Some(stamp("owner-a", 1)), false)
        .await;
    daemon
        .bind(2, 1, "owner-b", Some(stamp("owner-b", 1)), false)
        .await;
    daemon
        .bind(3, 1, "owner-a", Some(stamp("owner-a", 1)), false)
        .await;
    let _first = daemon.start(1, 1, shell(Some("first"), false, 15)).await;
    daemon.allow(1).await;
    let _other = daemon.start(2, 1, shell(Some("other"), true, 15)).await;
    daemon.allow(2).await;
    let first = running_task(&root, "session-1").await.1;
    let other = running_task(&root, "session-2").await.1;
    assert_eq!(first["scope"]["owner"]["module_id"], "owner-a");
    let held = daemon.start(3, 1, shell(Some("prepared"), false, 15)).await;
    daemon.ask(3).await;
    daemon.goodbye(1, 1).await;
    let describe = daemon.describe().await;
    assert_eq!(body(&describe)["owner"]["module_id"], "owner-a");
    daemon.answer(&describe, "ended", true, 1).await;
    let refused = body(&daemon.reply(3, held).await);
    assert_eq!(refused["code"], "not_started", "{refused}");
    assert_eq!(refused["detail"]["reason"], "scope_ended");
    let withdraw = body(
        &daemon
            .call(
                3,
                json!({"name":"tool.withdraw","arguments":{"call_key":"prepared"}}),
            )
            .await,
    );
    assert_eq!(withdraw, json!({"answer":"refused","reason":"scope_ended"}));
    let task = first["task_id"].as_str().unwrap();
    daemon
        .bind(4, 1, "owner-a", Some(stamp("owner-a", 1)), false)
        .await;
    // Rebind channel 1 to session-1: bash_status only exposes tasks
    // started in the caller's session.
    daemon
        .bind(1, 2, "direct", Some(stamp("owner-a", 1)), false)
        .await;
    let corr = daemon
        .start(
            1,
            2,
            json!({"name":"bash_status","preset":"head","arguments":{"taskId":task}}),
        )
        .await;
    let status = body(&daemon.reply(1, corr).await).to_string();
    assert!(
        status.contains("killed") && status.contains("scope_revoked"),
        "{status}"
    );
    assert_eq!(
        task_metadata(&root)
            .iter()
            .find(|(_, task)| task["task_id"] == other["task_id"])
            .unwrap()
            .1["status"],
        "running"
    );
    daemon.send(FrameType::Cancel, 2, 1, _other, None).await;
    assert_eq!(body(&daemon.reply(2, _other).await)["code"], "cancelled");
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn nondefinitive_answers_retry_without_killing_and_detached_result_is_once() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "carrier", Some(stamp("owner", 1)), false)
        .await;
    daemon
        .start(1, 1, gated_shell(&root, Some("natural")))
        .await;
    daemon.allow(1).await;
    let task = running_task(&root, "session-1").await.1;
    daemon
        .bind(2, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    daemon
        .bind(3, 1, "carrier", Some(stamp("owner", 1)), false)
        .await;
    daemon.goodbye(1, 1).await;
    let query = daemon.describe().await;
    daemon.answer(&query, "not_live", false, 1).await;
    let query = daemon.describe().await;
    daemon
        .send(
            FrameType::Error,
            0,
            0,
            query.header.corr,
            Some(json!({"code":"busy","message":"not definitive"})),
        )
        .await;
    let timed_out = daemon.describe().await;
    let retry = daemon.describe().await;
    assert_ne!(timed_out.header.corr, retry.header.corr);
    daemon.answer(&retry, "live", true, 2).await;
    assert_eq!(
        task_metadata(&root)
            .iter()
            .find(|(_, value)| value["task_id"] == task["task_id"])
            .unwrap()
            .1["status"],
        "running"
    );
    std::fs::write(root.join("project/release"), "release").unwrap();
    let until = tokio::time::Instant::now() + Duration::from_secs(8);
    let page = loop {
        let page = body(
            &daemon
                .call(2, json!({"name":"late_results","arguments":{}}))
                .await,
        );
        if !page["entries"].as_array().unwrap().is_empty() {
            break page;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "late result never arrived: {page}, tasks={:?}",
            task_metadata(&root)
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    };
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["entries"][0]["call_key"], "natural");
    assert_eq!(page["entries"][0]["kind"], "result");
    // The module named "carrier" is not in AFT's trusted consumer list,
    // so its wire result omits structured status. Empty command output
    // renders only the task marker; the persisted task record proves the exit.
    let completed = task_metadata(&root)
        .into_iter()
        .find(|(_, record)| record["task_id"] == task["task_id"])
        .unwrap()
        .1;
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["exit_code"], 0);
    let carrier = body(
        &daemon
            .call(3, json!({"name":"late_results","arguments":{}}))
            .await,
    );
    assert_eq!(carrier["entries"], json!([]));
    daemon
        .call(
            2,
            json!({"name":"late_results.ack","arguments":{"through":page["cursor"]}}),
        )
        .await;
    assert_eq!(
        body(
            &daemon
                .call(2, json!({"name":"late_results","arguments":{}}))
                .await
        )["entries"],
        json!([])
    );
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn rebind_live_epoch_move_survives_then_old_answer_spares_new_epoch() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(
        &root,
        &["scope.describe", "catalog.update", "supervisor.live_roots"],
    )
    .await;
    daemon
        .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    daemon.start(1, 1, shell(Some("old"), false, 15)).await;
    daemon.allow(1).await;
    let old = running_task(&root, "session-1").await.1;
    daemon
        .bind(1, 2, "owner", Some(stamp("owner", 2)), false)
        .await;
    let poll = daemon.describe().await;
    daemon.answer(&poll, "live", true, 2).await;
    assert_eq!(
        task_metadata(&root)
            .iter()
            .find(|(_, task)| task["task_id"] == old["task_id"])
            .unwrap()
            .1["status"],
        "running"
    );
    let poll = daemon.describe().await;
    let current = daemon.start(1, 2, shell(Some("new"), true, 15)).await;
    daemon.allow(1).await;
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if task_metadata(&root)
            .iter()
            .any(|(_, task)| task["scope"]["scope_epoch"] == 2 && task["status"] == "running")
        {
            break;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    daemon.answer(&poll, "ended", true, 1).await;
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if task_metadata(&root)
            .iter()
            .any(|(_, task)| task["task_id"] == old["task_id"] && task["status"] == "killed")
        {
            break;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let tasks = task_metadata(&root);
    let new = &tasks
        .iter()
        .find(|(_, task)| task["scope"]["scope_epoch"] == 2)
        .unwrap()
        .1;
    assert_eq!(new["status"], "running");
    assert_eq!(
        tasks
            .iter()
            .find(|(_, task)| task["task_id"] == old["task_id"])
            .unwrap()
            .1["status_reason"],
        "scope_revoked"
    );
    let mut corrs = std::collections::HashSet::new();
    for (corr, _) in &daemon.controls {
        assert!(
            corrs.insert(corr),
            "control corr reused: {:?}",
            daemon.controls
        );
    }
    assert!(daemon.controls.iter().any(|(_, op)| op == "catalog.update"));
    daemon.send(FrameType::Cancel, 1, 2, current, None).await;
    assert_eq!(body(&daemon.reply(1, current).await)["code"], "cancelled");
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn goodbye_without_tasks_and_nonadvertising_daemon_send_no_describe() {
    for ops in [vec!["scope.describe"], vec![]] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("module");
        let mut daemon = Daemon::spawn(&root, &ops).await;
        daemon
            .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
            .await;
        if ops.is_empty() {
            let call = daemon
                .start(1, 1, shell(Some("unadvertised"), false, 3))
                .await;
            daemon.allow(1).await;
            running_task(&root, "session-1").await;
            // Without scope.describe advertisement no request is sent.
            // Closing the route alone still cannot terminate the command.
            daemon.goodbye(1, 1).await;
            assert!(daemon
                .next_matching(Duration::from_millis(700), |frame| frame.header.ty
                    == FrameType::Request
                    && frame.header.channel == 0)
                .await
                .is_none());
            assert_eq!(task_metadata(&root)[0].1["status"], "running");
            assert!(daemon
                .next_matching(Duration::from_millis(50), |frame| frame.header.channel == 1
                    && frame.header.corr == call)
                .await
                .is_none());
        } else {
            daemon.goodbye(1, 1).await;
            assert!(daemon
                .next_matching(Duration::from_millis(700), |frame| frame.header.ty
                    == FrameType::Request
                    && frame.header.channel == 0)
                .await
                .is_none());
        }
    }
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn restart_recovers_stamped_task_and_polls_only_after_interval() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    daemon
        .start(1, 1, shell(Some("recovered"), false, 15))
        .await;
    daemon.allow(1).await;
    let task = running_task(&root, "session-1").await.1;
    drop(daemon);
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    // Recover the original session under a new epoch without revoking it.
    daemon
        .bind(1, 1, "direct", Some(stamp("owner", 2)), false)
        .await;
    assert!(daemon
        .status(1, task["task_id"].as_str().unwrap())
        .await
        .contains("running"));
    let poll = daemon.describe().await;
    daemon.answer(&poll, "not_live", true, 1).await;
    let status = daemon
        .wait_status(1, task["task_id"].as_str().unwrap(), "killed")
        .await;
    assert!(status.contains("scope revoked"), "{status}");
}

#[tokio::test]
async fn explicit_cancel_kills_while_keyless_and_legacy_create_no_ledger_rows() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &[]).await;
    daemon.bind(1, 1, "direct", None, false).await;
    daemon.bind(2, 1, "direct", None, true).await;
    let cancel = daemon.start(1, 1, shell(None, false, 15)).await;
    let task = running_task(&root, "session-1").await.1;
    daemon.send(FrameType::Cancel, 1, 1, cancel, None).await;
    assert_eq!(body(&daemon.reply(1, cancel).await)["code"], "cancelled");
    daemon
        .wait_status(1, task["task_id"].as_str().unwrap(), "killed")
        .await;
    let legacy = daemon.call(2, shell(Some("legacy-key"), true, 1)).await;
    assert_eq!(legacy.header.ty, FrameType::Response, "{}", body(&legacy));
    drop(daemon);
    fn database(path: &Path) -> Option<PathBuf> {
        for entry in std::fs::read_dir(path).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                if let Some(found) = database(&path) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|name| name == "aft.db") {
                return Some(path);
            }
        }
        None
    }
    let conn = rusqlite::Connection::open(database(&root).unwrap()).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM call_ledger", [], |row| row
            .get::<_, usize>(0))
            .unwrap(),
        0
    );
}

#[cfg(all(windows, feature = "test-timing-hooks"))]
#[tokio::test]
async fn goodbye_ended_kills_powershell_through_the_same_stamped_registry() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    let mut request = shell(Some("powershell"), true, 15);
    request["name"] = json!("powershell");
    request["arguments"]["command"] = json!("Start-Sleep -Seconds 15");
    daemon.start(1, 1, request).await;
    daemon.allow(1).await;
    let task = running_task(&root, "session-1").await.1;
    daemon.goodbye(1, 1).await;
    let query = daemon.describe().await;
    daemon.answer(&query, "ended", true, 1).await;
    daemon
        .bind(1, 2, "direct", Some(stamp("owner", 1)), false)
        .await;
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let corr = daemon.start(1, 2, json!({"name":"bash_status","preset":"head","arguments":{"taskId":task["task_id"]}})).await;
        let status = body(&daemon.reply(1, corr).await).to_string();
        if status.contains("killed") {
            assert!(status.contains("scope revoked"));
            break;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn connection_loss_detaches_without_queueing_or_killing() {
    use tokio::io::AsyncWriteExt;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("module");
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "owner", Some(stamp("owner", 1)), false)
        .await;
    daemon.start(1, 1, shell(Some("lost"), false, 15)).await;
    daemon.allow(1).await;
    let task = running_task(&root, "session-1").await.1;
    daemon.writer.shutdown().await.unwrap();
    daemon.reader.abort();
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    while daemon.child.try_wait().unwrap().is_none() {
        assert!(
            tokio::time::Instant::now() < until,
            "module did not finish connection-loss shutdown"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(daemon.controls.is_empty());
    assert_eq!(task_metadata(&root)[0].1["status"], "running");
    drop(daemon);
    let mut daemon = Daemon::spawn(&root, &["scope.describe"]).await;
    daemon
        .bind(1, 1, "direct", Some(stamp("owner", 1)), false)
        .await;
    assert!(daemon
        .status(1, task["task_id"].as_str().unwrap())
        .await
        .contains("running"));
    assert!(daemon
        .next_matching(Duration::from_millis(700), |frame| frame.header.ty
            == FrameType::Request
            && frame.header.channel == 0)
        .await
        .is_none());
    daemon
        .call(
            1,
            json!({"name":"bash_kill","preset":"head","arguments":{"taskId":task["task_id"]}}),
        )
        .await;
}
