//! Purpose-separated, command-lifetime ingress for jailed GitHub reads.
//!
//! The child controls every byte. Neither the ordinary relay ticket nor a
//! caller-selected identity is accepted here; the general relay is unreachable.

#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use serde_json::json;
use serde_json::Value;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
#[cfg(unix)]
use tokio::sync::{watch, Semaphore};

#[cfg(unix)]
use crate::gh_shim_relay::{self as relay, RelayReply};
use crate::gh_shim_relay::{GhRelay, ScopeStamp};

pub(crate) const SOCKET_ENV: &str = "AFT_GH_READ_SOCKET";
pub(crate) const CAPABILITY_FILE_ENV: &str = "AFT_GH_READ_CAPABILITY_FILE";
pub(crate) const STATE_ENV: &str = "AFT_GH_SHIM_STATE_DIR";
pub(crate) const CHILD_KEYS: [&str; 5] = [
    crate::gh_shim_ticket::GH_SHIM_TICKET_ENV,
    crate::gh_shim_ticket::GH_SHIM_WORKER_ENV,
    SOCKET_ENV,
    CAPABILITY_FILE_ENV,
    STATE_ENV,
];
#[cfg(unix)]
const MAX_FRAME: usize = 65_536;
#[cfg(unix)]
const MAX_RESPONSE: usize = 2_097_152;
#[cfg(unix)]
const FRAME_DEADLINE: Duration = Duration::from_secs(2);
#[cfg(unix)]
const CALL_DEADLINE: Duration = Duration::from_secs(20);
#[cfg(unix)]
const CONNECTIONS: usize = 4;
#[cfg(unix)]
static GLOBAL: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(64)));

/// Serde's closed enum rejects unknown operations and authority-bearing fields
/// before registry access. A connection contains exactly one length-prefixed JSON
/// envelope; it is not a SUBC connection and has no subscription protocol.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, tag = "op")]
pub(crate) enum Operation {
    #[serde(rename = "gh_shim.session_context")]
    SessionContext,
    #[serde(rename = "gh_shim.read_request")]
    ReadRequest {
        request: crate::gh_shim::read_contract::Request,
    },
}

#[cfg(unix)]
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    capability: String,
    operation: Operation,
}

pub(crate) struct ScopeQuery {
    pub session: String,
    pub reply: oneshot::Sender<Option<ScopeStamp>>,
}

#[cfg(unix)]
#[derive(Clone)]
struct Service {
    relay: Arc<GhRelay>,
    scopes: mpsc::Sender<ScopeQuery>,
    runtime: tokio::runtime::Handle,
}
#[cfg(unix)]
static SERVICE: Mutex<Option<Service>> = Mutex::new(None);

/// Installed for one module connection. Losing that connection invalidates all
/// brokers that captured its channel, including commands surviving a reconnect.
pub(crate) struct ServiceGuard {
    #[cfg(unix)]
    scopes: mpsc::Sender<ScopeQuery>,
}
impl Drop for ServiceGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let mut service = SERVICE.lock().unwrap_or_else(|p| p.into_inner());
            if service
                .as_ref()
                .is_some_and(|current| current.scopes.same_channel(&self.scopes))
            {
                service.take();
            }
        }
    }
}

pub(crate) fn install(relay: Arc<GhRelay>) -> (ServiceGuard, mpsc::Receiver<ScopeQuery>) {
    let (scopes, rx) = mpsc::channel(32);
    let guard = ServiceGuard {
        #[cfg(unix)]
        scopes: scopes.clone(),
    };
    #[cfg(unix)]
    {
        *SERVICE.lock().unwrap_or_else(|p| p.into_inner()) = Some(Service {
            relay,
            scopes,
            runtime: tokio::runtime::Handle::current(),
        });
    }
    #[cfg(not(unix))]
    let _ = (relay, scopes);
    (guard, rx)
}

#[cfg(unix)]
fn refusal(code: &str) -> RelayReply {
    RelayReply {
        ok: false,
        data: json!({"refusal_code": code, "stage": "worker_broker", "message": code}),
    }
}

#[cfg(unix)]
fn decode(bytes: &[u8]) -> Result<Envelope, RelayReply> {
    if bytes.len() > MAX_FRAME {
        return Err(refusal("frame_too_large"));
    }
    serde_json::from_slice(bytes).map_err(|_| refusal("request_malformed"))
}

#[cfg(unix)]
struct Budget {
    window: Instant,
    requests: u32,
    upstream: u32,
}

#[cfg(unix)]
struct SocketPath(PathBuf);
#[cfg(unix)]
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
#[cfg(unix)]
impl Budget {
    fn take(&mut self, read: bool) -> bool {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.requests = 0;
        }
        if self.requests >= 30 || (read && self.upstream >= 600) {
            return false;
        }
        self.requests += 1;
        self.upstream += u32::from(read);
        true
    }
}

/// Paths, not bearer bytes, are passed through the environment. The socket is
/// a sibling of the writable task temp, so the jail cannot replace its server.
#[cfg(unix)]
pub(crate) fn start(
    ticket: &crate::gh_shim_ticket::PendingTicket,
    task_id: &str,
    temp: &Path,
    deadline: Instant,
    tasks: crate::bash_background::BgTaskRegistry,
    env: &mut std::collections::HashMap<String, String>,
) -> Result<(), String> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let service = SERVICE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .ok_or("worker read broker has no daemon scope service")?;
    let name = temp
        .file_name()
        .ok_or("worker temp has no name")?
        .to_string_lossy();
    let socket = temp.with_file_name(format!("{name}.gh-read.sock"));
    let listener = std::os::unix::net::UnixListener::bind(&socket)
        .map_err(|e| format!("worker read broker bind failed: {e}"))?;
    let socket_path = SocketPath(socket.clone());
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    ticket.reserve_task(task_id);
    let stamp = crate::bash_background::registry::scope_for_spawn()
        .ok_or("worker read broker requires authenticated spawn scope")?;
    let (capability, cancel) = ticket
        .worker_read_capability(socket.clone(), deadline, &stamp)
        .ok_or("worker read broker grant could not be issued")?;
    let cap_file = temp.join(".gh-read-capability");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&cap_file)
        .map_err(|e| e.to_string())?;
    file.write_all(capability.as_bytes())
        .map_err(|e| e.to_string())?;
    let state = temp.join(".gh-shim");
    crate::gh_shim::seed_worker_state(&state)?;
    env.remove(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV);
    env.insert(SOCKET_ENV.into(), socket.to_string_lossy().into_owned());
    env.insert(
        CAPABILITY_FILE_ENV.into(),
        cap_file.to_string_lossy().into_owned(),
    );
    env.insert(STATE_ENV.into(), state.to_string_lossy().into_owned());
    let runtime = service.runtime.clone();
    runtime.spawn(async move {
        let _socket_path = socket_path;
        let listener = match tokio::net::UnixListener::from_std(listener) {
            Ok(listener) => listener,
            Err(_) => {
                cancel.send_replace(true);
                return;
            }
        };
        serve(listener, socket.clone(), service, tasks, cancel.subscribe()).await;
        let _ = std::fs::remove_file(socket);
    });
    Ok(())
}

#[cfg(unix)]
async fn serve(
    listener: tokio::net::UnixListener,
    socket: PathBuf,
    service: Service,
    tasks: crate::bash_background::BgTaskRegistry,
    mut cancel: watch::Receiver<bool>,
) {
    let local = Arc::new(Semaphore::new(CONNECTIONS));
    let upstream = Arc::new(Semaphore::new(2));
    let budget = Arc::new(Mutex::new(Budget {
        window: Instant::now(),
        requests: 0,
        upstream: 0,
    }));
    let mut connections = tokio::task::JoinSet::new();
    // Bound registry work even for syntactically valid guesses at the bearer.
    // This bucket is checked before allocating a task or authenticating a frame.
    let mut ingress = Budget {
        window: Instant::now(),
        requests: 0,
        upstream: 0,
    };
    loop {
        if *cancel.borrow() {
            break;
        }
        let Some(deadline) = crate::gh_shim_ticket::worker_read_deadline(&socket) else {
            break;
        };
        if Instant::now() >= deadline {
            break;
        }
        tokio::select! {
            biased;
            _ = cancel.changed() => break,
            _ = service.scopes.closed() => break,
            _ = tokio::time::sleep_until(deadline.into()) => {},
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break; };
                if !ingress.take(false) { continue; }
                let (Ok(local), Ok(global)) = (local.clone().try_acquire_owned(), GLOBAL.clone().try_acquire_owned()) else { continue; };
                let service = service.clone();
                let socket = socket.clone();
                let tasks = tasks.clone();
                let upstream = upstream.clone();
                let budget = budget.clone();
                let mut revoked = cancel.clone();
                connections.spawn(async move {
                    let (_local, _global) = (local, global);
                    if *revoked.borrow() { return; }
                    tokio::select! {
                        biased;
                        _ = revoked.changed() => {},
                        _ = connection(stream, &socket, &service, &tasks, &upstream, &budget) => {},
                    }
                });
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

#[cfg(unix)]
async fn connection(
    mut stream: tokio::net::UnixStream,
    socket: &Path,
    service: &Service,
    tasks: &crate::bash_background::BgTaskRegistry,
    upstream: &Arc<Semaphore>,
    budget: &Mutex<Budget>,
) {
    let Ok(Ok(bytes)) =
        tokio::time::timeout(FRAME_DEADLINE, read_frame(&mut stream, MAX_FRAME)).await
    else {
        return;
    };
    let Ok(_permit) = upstream.clone().try_acquire_owned() else {
        return;
    };
    let response = tokio::time::timeout(
        CALL_DEADLINE,
        dispatch(
            &bytes,
            socket,
            &service.relay.transport,
            |capability| crate::gh_shim_ticket::redeem_worker_read(capability, socket),
            |task| !tasks.is_task_terminal(task),
            |session| async {
                let (reply, receive) = oneshot::channel();
                service
                    .scopes
                    .try_send(ScopeQuery { session, reply })
                    .map_err(|_| ())?;
                receive.await.map_err(|_| ())
            },
            budget,
        ),
    )
    .await;
    let Ok(response) = response else {
        return;
    };
    let body = json!({"status": if response.ok { "ok" } else { "error" }, "data": response.data});
    if let Ok(bytes) = serde_json::to_vec(&body) {
        if bytes.len() <= MAX_RESPONSE {
            let _ = tokio::time::timeout(FRAME_DEADLINE, write_frame(&mut stream, &bytes)).await;
        }
    }
}

#[cfg(unix)]
pub(crate) async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Vec<u8>> {
    let size = reader.read_u32().await? as usize;
    if size == 0 || size > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid broker frame size",
        ));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

#[cfg(unix)]
pub(crate) async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
) -> std::io::Result<()> {
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await
}

#[cfg(unix)]
use crate::gh_shim_relay::RelayTransport;

/// Client discovery never opens user configuration or a general connection file.
/// In worker mode, absent paths are an error rather than a discovery fallback.
pub(crate) struct Client {
    #[cfg(unix)]
    socket: PathBuf,
    #[cfg(unix)]
    capability_file: PathBuf,
}
impl Client {
    pub(crate) fn from_process() -> Result<Self, String> {
        #[cfg(windows)]
        return Err("gh_shim_worker_read_platform_unsupported_windows: worker read broker is unavailable on Windows".into());
        #[cfg(not(windows))]
        {
            let path = |key| {
                std::env::var_os(key)
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .ok_or_else(|| format!("worker read broker requires {key}"))
            };
            Ok(Self {
                socket: path(SOCKET_ENV)?,
                capability_file: path(CAPABILITY_FILE_ENV)?,
            })
        }
    }
}

pub(crate) fn request(
    client: &Client,
    operation: Operation,
    budget: Duration,
) -> Result<Value, String> {
    #[cfg(not(unix))]
    {
        let _ = (client, operation, budget);
        Err("gh_shim_worker_read_platform_unsupported_windows".into())
    }
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&client.capability_file)
            .map_err(|_| "worker read capability file unavailable")?;
        if !file
            .metadata()
            .map_err(|_| "worker read capability metadata unavailable")?
            .is_file()
        {
            return Err("worker read capability must be a regular file".into());
        }
        let mut capability = String::new();
        file.by_ref()
            .take(33)
            .read_to_string(&mut capability)
            .map_err(|_| "worker read capability unavailable")?;
        if capability.len() != 32 || !capability.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("worker read capability malformed".into());
        }
        let bytes = serde_json::to_vec(&Envelope {
            capability,
            operation,
        })
        .map_err(|_| "worker broker request malformed")?;
        if bytes.len() > MAX_FRAME {
            return Err("worker broker request too large".into());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        runtime.block_on(async {
            tokio::time::timeout(budget.min(CALL_DEADLINE), async {
                let mut stream = tokio::net::UnixStream::connect(&client.socket)
                    .await
                    .map_err(|_| "worker read broker unavailable")?;
                let peer = stream
                    .peer_cred()
                    .map_err(|_| "worker read broker peer unavailable")?;
                if peer.uid() != unsafe { libc::geteuid() } {
                    return Err("worker read broker peer mismatch".to_string());
                }
                write_frame(&mut stream, &bytes)
                    .await
                    .map_err(|_| "worker read broker write failed")?;
                let bytes = read_frame(&mut stream, MAX_RESPONSE)
                    .await
                    .map_err(|_| "worker read broker reply unavailable")?;
                let reply: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| "worker read broker reply malformed")?;
                match reply["status"].as_str() {
                    Some("ok") => reply
                        .get("data")
                        .cloned()
                        .ok_or_else(|| "worker read broker omitted data".into()),
                    Some("error") => Err(format!(
                        "worker read broker refused: {}",
                        reply["data"]["refusal_code"].as_str().unwrap_or("refused")
                    )),
                    _ => Err("worker read broker reply has unknown status".into()),
                }
            })
            .await
            .map_err(|_| "worker read broker timed out".to_string())?
        })
    }
}

#[cfg(unix)]
async fn dispatch<T, A, L, S, F>(
    bytes: &[u8],
    socket: &Path,
    transport: &T,
    authenticate: A,
    live: L,
    scope: S,
    budget: &Mutex<Budget>,
) -> RelayReply
where
    T: RelayTransport,
    A: Fn(&str) -> Option<crate::gh_shim_ticket::Redeemed>,
    L: Fn(&str) -> bool,
    S: Fn(String) -> F,
    F: std::future::Future<Output = Result<Option<ScopeStamp>, ()>>,
{
    let envelope = match decode(bytes) {
        Ok(envelope) => envelope,
        Err(reply) => return reply,
    };
    let Some(identity) = authenticate(&envelope.capability) else {
        return refusal("capability_unknown");
    };
    if !live(&identity.task_id) {
        return refusal("command_ended");
    }
    let read = matches!(envelope.operation, Operation::ReadRequest { .. });
    if !budget.lock().unwrap_or_else(|p| p.into_inner()).take(read) {
        return refusal("read_budget_exceeded");
    }
    let Ok(stamp) = scope(identity.session_id.clone()).await else {
        return refusal("scope_unavailable");
    };
    if !stamp.as_ref().is_some_and(|stamp| {
        crate::gh_shim_ticket::worker_read_scope_matches(&envelope.capability, socket, stamp)
    }) {
        return refusal("worker_binding_changed");
    }
    let context = relay::session_context(&identity, stamp.as_ref());
    if !context.ok || context.data["worker"] != true || !transport.opens_scoped_routes() {
        return refusal("agent_identity_required");
    }
    if transport.read_repository(&identity).is_err() {
        return refusal("worker_reads_disabled");
    }
    // Check again after querying the live route table: revocation must also
    // invalidate requests queued while the daemon was binding or closing routes.
    if authenticate(&envelope.capability).is_none() || !live(&identity.task_id) {
        return refusal("command_ended");
    }
    let reply = match envelope.operation {
        Operation::SessionContext => context,
        Operation::ReadRequest { request } => {
            relay::read_validated_request(transport, &identity, stamp.as_ref(), &request)
                .await
                .0
        }
    };
    let final_scope = scope(identity.session_id.clone()).await.ok().flatten();
    if !final_scope.as_ref().is_some_and(|stamp| {
        crate::gh_shim_ticket::worker_read_scope_matches(&envelope.capability, socket, stamp)
    }) {
        return refusal("worker_binding_changed");
    }
    // An upstream read already sent cannot be undone. Suppress its result if
    // the command ended while the call was in flight.
    if authenticate(&envelope.capability).is_none() || !live(&identity.task_id) {
        return refusal("command_ended");
    }
    if budget.lock().unwrap_or_else(|p| p.into_inner()).requests == 1 {
        log::info!(
            "gh_shim worker_read: session={} task={} read={} ok={}",
            identity.session_id.escape_debug(),
            identity.task_id.escape_debug(),
            read,
            reply.ok
        );
    }
    reply
}

#[cfg(all(test, unix))]
#[path = "gh_shim_worker_broker_tests.rs"]
mod tests;
