use super::*;
use std::sync::{Arc, Condvar};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};

#[derive(Default)]
pub(crate) struct Counts {
    pub opens: usize,
    pub active: usize,
    pub queries: usize,
}

pub(crate) struct Daemon {
    _directory: tempfile::TempDir,
    pub path: PathBuf,
    pub counts: Arc<(Mutex<Counts>, Condvar)>,
    stop: tokio::sync::oneshot::Sender<()>,
    server: std::thread::JoinHandle<()>,
}

impl Daemon {
    pub fn start(query_group: usize, fail_first_group: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connection.json");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let key = vec![0x42; subc_transport::KEY_LEN];
        let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
        connection_file::write_atomic(
            &path,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(subc_protocol::PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                }],
                key: key.clone(),
                daemon_id,
                pid: std::process::id(),
                daemon_ver: "route-test".into(),
            },
        )
        .unwrap();
        let counts = Arc::new((Mutex::new(Counts::default()), Condvar::new()));
        let recorded = counts.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let mut tasks = tokio::task::JoinSet::new();
                tokio::pin!(stopped);
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        accepted = listener.accept() => {
                            let (mut stream, _) = accepted.unwrap();
                            let key = key.clone();
                            let counts = recorded.clone();
                            tasks.spawn(async move {
                                subc_transport::authenticate_server(&mut stream, &key, &daemon_id, "route-test", Duration::from_secs(2)).await.unwrap();
                                let mut active = 0;
                                let mut pending = Vec::new();
                                let mut failed = false;
                                let mut generation = 0;
                                while let Ok(Some(request)) = subc_transport::read_frame(&mut stream).await {
                                    if request.header.ty == FrameType::Goodbye {
                                        active -= 1;
                                        counts.0.lock().unwrap().active -= 1;
                                        counts.1.notify_all();
                                        continue;
                                    }
                                    if request.header.ty != FrameType::Request { continue; }
                                    let value: Value = serde_json::from_slice(&request.body).unwrap();
                                    let reply = match value["op"].as_str() {
                                        Some("catalog.list") => json!({"op":"catalog.list","generation":1,"modules":[{
                                            "module_id":"synapse","module_version":"1","roles":[{"role":"management_surface",
                                            "operations":[{"name":"models.list","kind":"query"},{"name":"embed.query","kind":"query"},{"name":"embed.batch","kind":"query"}],
                                            "config_schema":{"type":"object"},"observability":[],"identity_scope":[]}],"control_ops":[]}],"subc_ops":["catalog.list","route.open"]}),
                                        Some("route.open") => {
                                            let mut count = counts.0.lock().unwrap();
                                            count.opens += 1;
                                            generation = count.opens;
                                            count.active += 1;
                                            active += 1;
                                            json!({"op":"route.open","route_channel":7,"route_epoch":1})
                                        },
                                        _ if value["method"] == MODELS_LIST_OPERATION => json!({"models":[{"model":pause_test_support::MODEL,"fingerprint":"pinned","table_epoch":1,"dims":2,"recommended_batch":{"rows":100,"token_budget":8192}}]}),
                                        _ if value["method"] == QUERY_OPERATION => {
                                            counts.0.lock().unwrap().queries += 1;
                                            pending.push((request, value));
                                            if pending.len() < query_group { continue; }
                                            // The real consumer enforces its 1024-request route window.
                                            // Holding these replies proves requests overlap on the wire.
                                            for (request, value) in pending.drain(..) {
                                                let reply = if fail_first_group && generation == 1 && !failed {
                                                    json!({"code":"test_transport_failure","message":"retry this connection"})
                                                } else {
                                                    json!({"fingerprint":"pinned","table_epoch":1,"vectors":[{"id":value["params"]["id"],"content_sha256":value["params"]["content_sha256"],"vector":[1.0,0.0]}]})
                                                };
                                                respond(&mut stream, &request, reply, fail_first_group && generation == 1 && !failed).await;
                                            }
                                            failed = true;
                                            continue;
                                        },
                                        _ if value["method"] == "consent.test_timeout" => {
                                            counts.0.lock().unwrap().queries += 1;
                                            counts.1.notify_all();
                                            continue;
                                        },
                                        _ if value["method"] == "consent.test_error" => {
                                            respond(&mut stream, &request, json!({"code":"test_refused","message":"refused"}), true).await;
                                            continue;
                                        },
                                        _ => json!({"result":{"ok":true}}),
                                    };
                                    respond(&mut stream, &request, reply, false).await;
                                }
                                counts.0.lock().unwrap().active -= active;
                                counts.1.notify_all();
                            });
                        }
                    }
                }
                tasks.abort_all();
            });
        });
        Self {
            _directory: directory,
            path,
            counts,
            stop,
            server,
        }
    }

    pub fn config(&self, root: usize) -> SemanticBackendConfig {
        SemanticBackendConfig {
            backend: crate::config::SemanticBackend::Synapse,
            model: pause_test_support::MODEL.into(),
            subc_connection_file: Some(self.path.clone()),
            route_project_root: Some(self._directory.path().join(format!("root-{root}"))),
            timeout_ms: 5_000,
            query_instruction: "off".into(),
            ..Default::default()
        }
    }

    pub fn wait_closed(&self) {
        let (count, _) = self
            .counts
            .1
            .wait_timeout_while(
                self.counts.0.lock().unwrap(),
                Duration::from_secs(2),
                |count| count.active != 0,
            )
            .unwrap();
        assert_eq!(count.active, 0, "completed clients left routes open");
    }

    pub fn finish(self) {
        let _ = self.stop.send(());
        self.server.join().unwrap();
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, request: &Frame, value: Value, error: bool) {
    let frame = Frame::build_with_version(
        request.header.ver,
        if error {
            FrameType::Error
        } else {
            FrameType::Response
        },
        Flags::new(false, Priority::Interactive, false),
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    let _ = subc_transport::write_frame(stream, &frame).await;
}

#[test]
fn synapse_semantic_models_share_one_route_across_roots() {
    let daemon = Daemon::start(1, false);
    let models = (0..12)
        .map(|n| {
            crate::semantic_index::SemanticEmbeddingModel::from_config(&daemon.config(n % 3))
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        daemon.counts.0.lock().unwrap().opens,
        1,
        "semantic models must share their management route"
    );
    drop(models);
    daemon.wait_closed();
    daemon.finish();
}

#[test]
fn synapse_shared_route_keeps_lane_pauses_on_the_requesting_root() {
    use pause_test_support::*;
    crate::semantic_index::with_lane_pause_clock(START, |clock| {
        with_fake_synapse(clock, None, |first_config, _| {
            let mut second_config = first_config.clone();
            let first_root = first_config.route_project_root.as_ref().unwrap();
            let second_root = first_root.join("second-root");
            second_config.route_project_root = Some(second_root.clone());
            let mut first = SynapseEmbeddingClient::from_config(&first_config).unwrap();
            let mut second = SynapseEmbeddingClient::from_config(&second_config).unwrap();
            assert!(Arc::ptr_eq(
                &first.state.connection,
                &second.state.connection
            ));
            assert!(matches!(
                first.embed_query("first", Duration::from_secs(1)),
                Err(SynapseEmbeddingError::LanePaused(_))
            ));
            assert!(lane_pause_for_root(first_root).is_some());
            assert!(lane_pause_for_root(&second_root).is_none());
            assert!(matches!(
                second.embed_query("second", Duration::from_secs(1)),
                Err(SynapseEmbeddingError::LanePaused(_))
            ));
            assert!(lane_pause_for_root(&second_root).is_some());
        });
    });
}

#[test]
fn synapse_shared_reset_reconnects_once_without_serializing_callers() {
    const CALLERS: usize = 8;
    let daemon = Daemon::start(CALLERS, true);
    let clients = (0..CALLERS)
        .map(|n| SynapseEmbeddingClient::from_config(&daemon.config(n)).unwrap())
        .collect::<Vec<_>>();
    let shared = Arc::clone(&clients[0].state.connection);
    let old = shared
        .runtime
        .block_on(clients[0].state.ensure_route())
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(CALLERS));
    std::thread::scope(|scope| {
        let threads = clients
            .into_iter()
            .map(|mut client| {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    assert_eq!(
                        client
                            .embed_query("concurrent", Duration::from_secs(5))
                            .unwrap(),
                        vec![1.0, 0.0]
                    );
                    client
                })
            })
            .collect::<Vec<_>>();
        let clients = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            daemon.counts.0.lock().unwrap().opens,
            2,
            "one failed generation must cause only one reconnect"
        );
        assert_eq!(daemon.counts.0.lock().unwrap().queries, CALLERS * 2);
        let replacement = shared
            .runtime
            .block_on(clients[0].state.ensure_route())
            .unwrap();
        assert!(!Arc::ptr_eq(&old, &replacement));
        shared.runtime.block_on(shared.reset(&old));
        for client in &clients {
            let current = shared
                .runtime
                .block_on(client.state.ensure_route())
                .unwrap();
            assert!(
                Arc::ptr_eq(&replacement, &current),
                "late reset replaced a healthy connection"
            );
        }
    });
    drop(old);
    drop(shared);
    daemon.wait_closed();
    daemon.finish();
}
