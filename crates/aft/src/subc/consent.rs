//! Cingulate consent client for stamped Alfonso workers.
//!
//! When the access policy says `ask` for a worker's in-process tool call, the
//! call is held while the question goes to Cingulate (`consent.request`,
//! `consent.await`, `consent.withdraw`). Cingulate routes a worker's ask to its
//! head and, if escalated, to the user in the Alfonso apps. AFT never shows a
//! host prompt for these calls and never falls back to one: if Cingulate
//! cannot be reached the call ends with `access_unavailable`.
//!
//! An ask never expires. The call is held for at most the route's reply budget
//! (never more than 100 s); an ask still open then ends the call with
//! `access_pending`, and the persisted ask (see [`crate::db::access_asks`]) lets
//! the retried call pick up the same Cingulate ask by its original call key.
//!
//! Wire shape, as AFT sends it: a management route to the `cingulate` module,
//! opened under the asking route's own scope (so Cingulate reads the asker and
//! its head from the daemon's stamp), carrying `{"method": <name>, "params":
//! {...}}`. `consent.request` returns the ask's `elicitation_id` and, when it
//! has already settled, its outcome. `consent.await {elicitationId, wait_ms}`
//! long-polls; `consent.withdraw {elicitationId}` withdraws.
use super::*;
use crate::access_policy::{AccessGrant, AccessOperation, AskRoute, HeadPolicy};
use crate::db::access_asks::{self as asks, AskKey};
use std::future::Future;
use std::pin::Pin;
use subc_protocol::scope::ScopeSelector;

pub(super) const CINGULATE_MODULE_ID: &str = "cingulate";

/// Cingulate refusals that end an ask by name. Each ends the held call with
/// the same code, and none grants anything.
const TERMINAL_REFUSALS: &[&str] = &[
    "access_no_head",
    "access_head_gone",
    "access_not_placed",
    "access_withdrawn",
    "access_declined",
];

/// Clients give a call without a declared reply budget at least this long
/// (the shortest supported transport deadline).
const UNDECLARED_REPLY_BUDGET: Duration = Duration::from_secs(25);
/// Time kept back from the reply budget to run the approved call and deliver it.
const RERUN_MARGIN: Duration = Duration::from_secs(5);
/// No call is held longer than this, whatever its reply budget.
const MAX_HOLD: Duration = Duration::from_secs(100);
/// How often a held call checks for cancellation, route close and scope end.
const SIGNAL_POLL: Duration = Duration::from_millis(100);
/// Bound on one `consent.request` or `consent.withdraw` round trip.
const UNARY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ConsentError {
    /// Cingulate or the daemon could not be reached.
    Unavailable(String),
    /// Cingulate answered with a named refusal.
    Refused { code: String, message: String },
}

pub(super) type ConsentFuture = Pin<Box<dyn Future<Output = Result<Value, ConsentError>> + Send>>;

/// One consent method sent under the asking route's scope.
#[derive(Clone, Debug)]
pub(super) struct ConsentCall {
    pub(super) scope: ScopeSelector,
    pub(super) project_root: PathBuf,
    pub(super) session: String,
    pub(super) method: &'static str,
    pub(super) params: Value,
    pub(super) timeout: Duration,
}

/// The outbound consent channel. Production talks to Cingulate over the
/// daemon; tests install a fake.
pub(super) trait ConsentTransport: Send + Sync {
    fn call(&self, call: ConsentCall) -> ConsentFuture;
}

static TRANSPORT: LazyLock<StdMutex<Option<Arc<dyn ConsentTransport>>>> =
    LazyLock::new(|| StdMutex::new(None));

#[cfg(test)]
static TEST_TRANSPORT: LazyLock<StdMutex<Option<Arc<dyn ConsentTransport>>>> =
    LazyLock::new(|| StdMutex::new(None));

/// Install the production transport for this process's daemon connection.
pub(super) fn install_subc_transport(connection_file: &Path) {
    *TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(SharedSubcConsent(
        Arc::new(SubcConsentTransport::new(connection_file.to_path_buf())),
    )));
}

/// Replace the transport for consent tests; the module loop's production
/// install never overrides it.
#[cfg(test)]
pub(super) fn install_test_transport(transport: Option<Arc<dyn ConsentTransport>>) {
    *TEST_TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = transport;
}

fn transport() -> Option<Arc<dyn ConsentTransport>> {
    #[cfg(test)]
    {
        let test = TEST_TRANSPORT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if test.is_some() {
            return test;
        }
    }
    TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
static HOLD_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(super) fn set_hold_budget_for_test(budget: Option<Duration>) {
    HOLD_OVERRIDE_MS.store(
        budget.map_or(0, |budget| budget.as_millis().max(1) as u64),
        Ordering::SeqCst,
    );
}

/// How long a call is held for an answer before it ends with `access_pending`.
/// A tool that declares a reply budget is held for that budget, otherwise for
/// the shortest client transport deadline, less a margin to run the approved
/// call and deliver it, and never longer than 100 s.
pub(super) fn hold_budget(declared_reply: Option<Duration>) -> Duration {
    #[cfg(test)]
    {
        let ms = HOLD_OVERRIDE_MS.load(Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    declared_hold_budget(declared_reply)
}

fn declared_hold_budget(declared_reply: Option<Duration>) -> Duration {
    declared_reply
        .unwrap_or(UNDECLARED_REPLY_BUDGET)
        .saturating_sub(RERUN_MARGIN)
        .min(MAX_HOLD)
}

/// Production transport over the daemon's own authenticated subc connection,
/// as `reserved:aft`. The connection opens lazily and reopens after a failure.
struct SubcConsentTransport {
    connection_file: PathBuf,
    consumer: tokio::sync::Mutex<Option<Arc<subc_client_rs::SubcConsumer>>>,
}

impl SubcConsentTransport {
    fn new(connection_file: PathBuf) -> Self {
        Self {
            connection_file,
            consumer: tokio::sync::Mutex::new(None),
        }
    }

    async fn consumer(&self) -> Result<Arc<subc_client_rs::SubcConsumer>, ConsentError> {
        let mut slot = self.consumer.lock().await;
        if let Some(consumer) = slot.as_ref() {
            return Ok(Arc::clone(consumer));
        }
        if crate::launch_nonce::consumer_identity().is_none() {
            return Err(ConsentError::Unavailable(
                "the daemon has no subc module identity to ask with".into(),
            ));
        }
        let consumer = crate::fleet_status::connect_subc_consumer(
            &self.connection_file,
            subc_client_rs::ConsumerOptions::default(),
        )
        .await
        .map_err(|error| ConsentError::Unavailable(error.to_string()))?;
        let consumer = Arc::new(consumer);
        *slot = Some(Arc::clone(&consumer));
        Ok(consumer)
    }

    async fn forget(&self, failed: &Arc<subc_client_rs::SubcConsumer>) {
        let mut slot = self.consumer.lock().await;
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, failed))
        {
            *slot = None;
        }
    }

    async fn send(self: Arc<Self>, call: ConsentCall) -> Result<Value, ConsentError> {
        use subc_client_rs::{CallError, CallOptions};
        let consumer = self.consumer().await?;
        let identity = subc_protocol::BindIdentity::new(
            call.project_root.clone(),
            "aft-consent",
            call.session.clone(),
        );
        let options = CallOptions {
            consumer_identity: crate::launch_nonce::consumer_identity(),
            timeout: call.timeout,
            route_retry_deadline: call.timeout,
            ..CallOptions::default()
        };
        // A refused scoped open is never retried unscoped: Cingulate must see
        // the asker's stamp to know who asks and who answers.
        let route = match consumer
            .open_route_scoped(
                RouteTarget::ManagementSurface {
                    module_id: CINGULATE_MODULE_ID.into(),
                },
                identity,
                call.scope.clone(),
                options,
            )
            .await
        {
            Ok(route) => route,
            Err(CallError::Module(body)) => {
                return Err(ConsentError::Refused {
                    code: body.code,
                    message: body.message,
                })
            }
            Err(error) => {
                self.forget(&consumer).await;
                return Err(ConsentError::Unavailable(error.to_string()));
            }
        };
        let body = serde_json::to_vec(&json!({"method": call.method, "params": call.params}))
            .map_err(|error| ConsentError::Unavailable(error.to_string()))?;
        let options = CallOptions {
            timeout: call.timeout,
            ..CallOptions::default()
        };
        match consumer.request(&route, body, options).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| ConsentError::Unavailable("cingulate's reply was not JSON".into())),
            Err(CallError::Module(body)) => Err(ConsentError::Refused {
                code: body.code,
                message: body.message,
            }),
            Err(error) => {
                self.forget(&consumer).await;
                Err(ConsentError::Unavailable(error.to_string()))
            }
        }
    }
}

/// The installed handle: one shared connection state for every consent call.
struct SharedSubcConsent(Arc<SubcConsentTransport>);

impl ConsentTransport for SharedSubcConsent {
    fn call(&self, call: ConsentCall) -> ConsentFuture {
        Box::pin(Arc::clone(&self.0).send(call))
    }
}

/// The capability a tool response asks Cingulate for, when the in-process
/// access check refused with the internal ask code.
pub(super) fn requested_ask(response: &Response) -> Option<AccessGrant> {
    if response.success
        || response.data.get("code").and_then(Value::as_str)
            != Some(crate::access_policy::ACCESS_ASK_REQUIRED)
    {
        return None;
    }
    serde_json::from_value(response.data.get("ask")?.clone()).ok()
}

/// Calls currently held on each route, so a route close can withdraw their asks.
static HELD: LazyLock<StdMutex<HashMap<RouteChannel, Vec<Arc<AtomicBool>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Marks one held call on its route for the length of the hold.
pub(super) struct RouteHold {
    route: RouteChannel,
    closed: Arc<AtomicBool>,
}

impl RouteHold {
    fn new(route: RouteChannel) -> Self {
        let closed = Arc::new(AtomicBool::new(false));
        HELD.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(route)
            .or_default()
            .push(Arc::clone(&closed));
        Self { route, closed }
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl Drop for RouteHold {
    fn drop(&mut self) {
        let mut held = HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(flags) = held.get_mut(&self.route) {
            flags.retain(|flag| !Arc::ptr_eq(flag, &self.closed));
            if flags.is_empty() {
                held.remove(&self.route);
            }
        }
    }
}

/// The consumer closed `route` (a Goodbye, not a transport drop or a
/// higher-epoch rebind): every call held on it withdraws its ask.
pub(super) fn note_route_closed(route: RouteChannel) {
    if let Some(flags) = HELD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&route)
    {
        for flag in flags {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

/// The parts of a held call the consent flow reads.
pub(super) struct HeldCall {
    pub(super) identity: RouteIdentity,
    pub(super) tool: String,
    pub(super) arguments: Value,
    pub(super) request_id: String,
    pub(super) executor: Arc<Executor>,
    pub(super) route: RouteChannel,
    pub(super) corr: u64,
    pub(super) active: ActiveToolCalls,
    pub(super) connection_cancel: PersistentCancelSignal,
    pub(super) drain: drain::ModuleDrainWindow,
}

/// What a held call does after its ask was handled.
enum Hold {
    /// Run the call again with these capabilities approved once. `holder`
    /// owns them until the call finishes.
    Run {
        once: Vec<AccessGrant>,
        ask: AskKey,
        holder: String,
    },
    /// End the call with this response.
    Answer(Response),
    /// Nobody waits for this call any more (cancelled, route closed, transport
    /// gone); the caller sends nothing.
    Abandoned,
}

/// Run a stamped worker's call, holding it on Cingulate whenever the access
/// check asks. `first` is the call's first response; `rerun` runs it again
/// with approved capabilities installed. Returns the final response with its
/// formatted text when the run produced one, or `None` when nobody waits.
pub(super) async fn run_with_consent<F, Fut>(
    call: HeldCall,
    first: (Response, String),
    mut rerun: F,
) -> Option<(Response, Option<String>)>
where
    F: FnMut(AskRoute) -> Fut,
    Fut: Future<Output = (Response, String)>,
{
    let Some(mut grant) = requested_ask(&first.0) else {
        return Some((first.0, Some(first.1)));
    };
    let route_hold = RouteHold::new(call.route);
    let deadline = Instant::now() + hold_budget(None);
    let mut holder: Option<(AskKey, String)> = None;
    loop {
        let own = holder.as_ref().map(|(_, holder)| holder.clone());
        match hold(&call, grant, deadline, &route_hold, own).await {
            Hold::Run {
                once,
                ask,
                holder: claimed,
            } => {
                holder = Some((ask, claimed));
                let (response, text) = rerun(AskRoute::Consent { once }).await;
                match requested_ask(&response) {
                    Some(next) => grant = next,
                    None => {
                        if let Some((ask, claimed)) = holder.take() {
                            // The approvals are used up by this run, whatever it returned.
                            let _ = with_db(&call, move |conn, _| {
                                asks::consume(conn, &ask, &claimed).map(|_| ())
                            })
                            .await;
                        }
                        return Some((response, Some(text)));
                    }
                }
            }
            Hold::Answer(response) => return Some((response, None)),
            Hold::Abandoned => return None,
        }
    }
}

async fn with_db<T: Send + 'static>(
    call: &HeldCall,
    work: impl FnOnce(&rusqlite::Connection, &AppContext) -> rusqlite::Result<T> + Send + 'static,
) -> Result<T, String> {
    let Some(ctx) = call.executor.actor_context(&call.identity.root) else {
        return Err("executor actor is not registered".into());
    };
    tokio::task::spawn_blocking(move || {
        let db = ctx
            .db()
            .ok_or_else(|| "aft.db is unavailable".to_string())?;
        let conn = db
            .lock()
            .map_err(|_| "aft.db mutex is poisoned".to_string())?;
        work(&conn, &ctx).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// One ask ready to send.
#[derive(Clone)]
struct Prepared {
    key: AskKey,
    call_key: String,
    request: Value,
    elicitation_id: Option<String>,
    selector: ScopeSelector,
    grant: AccessGrant,
}

enum Prep {
    Ask(Prepared),
    /// Another dispatch of the same call is running with this call's approvals.
    Busy,
}

fn selector(stamp: &subc_protocol::scope::ScopeStamp) -> ScopeSelector {
    ScopeSelector {
        owner: stamp.owner.clone(),
        scope_ref: stamp.scope_ref.clone(),
        scope_epoch: Some(stamp.scope_epoch),
    }
}

fn operation_name(operation: AccessOperation) -> &'static str {
    match operation {
        AccessOperation::Read => "read",
        AccessOperation::Write => "write",
    }
}

/// The ask's identity: tool, canonical arguments, resolved cwd, and the asking
/// scope at its epoch. A changed cwd or epoch is a different ask.
fn args_hash(identity: &RouteIdentity, tool: &str, arguments: &Value) -> String {
    let stamp = identity.scope.as_ref();
    cortexkit_role_tool_provider::catalog::composition_digest(&json!({
        "tool": tool,
        "arguments": arguments,
        "cwd": identity.project_root,
        "scope_ref": stamp.map(|stamp| stamp.scope_ref.clone()),
        "scope_epoch": stamp.map(|stamp| stamp.scope_epoch),
    }))
    .expect("ask identity is JSON")
}

/// Whether the worker's head holds `grant` itself, by its own policy or a
/// standing grant. A worker with no head never has an `allow` head policy.
fn head_policy(
    conn: &rusqlite::Connection,
    ctx: &AppContext,
    identity: &RouteIdentity,
    grant: &AccessGrant,
) -> rusqlite::Result<HeadPolicy> {
    let Some(head) = access_grants::consent_head(conn, identity)? else {
        return Ok(HeadPolicy::NotHeld);
    };
    Ok(access_grants::policy_for_scope(conn, &head, ctx)?
        .map_or(HeadPolicy::NotHeld, |policy| policy.head_policy(grant)))
}

fn new_call_key() -> String {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("operating system random source for consent call keys");
    let hex: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("aft-ask-{hex}")
}

fn prepare(
    conn: &rusqlite::Connection,
    ctx: &AppContext,
    identity: &RouteIdentity,
    tool: &str,
    arguments: &Value,
    grant: &AccessGrant,
    own_holder: Option<&str>,
) -> rusqlite::Result<Prep> {
    let stamp = identity
        .scope
        .as_ref()
        .expect("only stamped workers ask Cingulate");
    let key = AskKey {
        scope: access_grants::scope_key(identity),
        args_hash: args_hash(identity, tool, arguments),
    };
    let selector = selector(stamp);
    if let Some(record) = asks::get(conn, &key)? {
        if let (Some(call_key), Some(request), Some(pending)) = (
            record.call_key.clone(),
            record.request.clone(),
            record.pending.clone(),
        ) {
            // Re-send the ask under the call key it was first sent with, so
            // Cingulate returns the same ask rather than a second one.
            return Ok(Prep::Ask(Prepared {
                key,
                call_key,
                request,
                elicitation_id: record.elicitation_id,
                selector,
                grant: pending,
            }));
        }
        if record.holder_is_live() && record.holder.as_deref() != own_holder {
            return Ok(Prep::Busy);
        }
        if !record.holder_is_live() {
            // Approvals claimed by a process that has since ended are not
            // reused: the earlier call may or may not have run.
            asks::discard(conn, &key)?;
        }
    }
    let call_key = new_call_key();
    let mut request = json!({
        "kind": "tool_permission",
        "call_key": call_key,
        "tool": tool,
        "operation": operation_name(grant.operation),
        "paths": [grant.path],
        "reason": format!(
            "{} needs to {} a path outside this worker's worktree",
            tool.chars().take(64).collect::<String>(),
            operation_name(grant.operation)
        ),
        "args_hash": key.args_hash,
        "pattern": grant,
    });
    if identity
        .scope
        .as_ref()
        .is_some_and(|stamp| stamp.kind != subc_protocol::scope::ScopeKind::Head)
    {
        request["head_policy"] = json!(head_policy(conn, ctx, identity, grant)?);
    }
    asks::open(
        conn,
        &key,
        &call_key,
        &serde_json::to_value(&selector).expect("scope selector is JSON"),
        &request,
        grant,
        crate::db::call_ledger::now_ms(),
    )?;
    Ok(Prep::Ask(Prepared {
        key,
        call_key,
        request,
        elicitation_id: None,
        selector,
        grant: grant.clone(),
    }))
}

/// Cingulate's answer as AFT applies it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Settlement {
    Pending,
    Allow { scope: bool, by_user: bool },
    Deny { note: Option<String> },
    Refused(String),
}

fn settlement(reply: &Value) -> Settlement {
    let outcome = reply
        .get("outcome")
        .or_else(|| reply.get("answer"))
        .and_then(Value::as_str);
    // A settled allow with no `answered_by` is treated as the head's answer,
    // so it is checked against the head's authority again.
    let by_user = reply.get("answered_by").and_then(Value::as_str) == Some("user");
    let note = reply
        .get("note")
        .and_then(Value::as_str)
        .map(|note| note.chars().take(160).collect());
    match outcome {
        Some("allow_once") => Settlement::Allow {
            scope: false,
            by_user,
        },
        Some("allow_scope") => Settlement::Allow {
            scope: true,
            by_user,
        },
        Some("deny") => Settlement::Deny { note },
        Some(code) if code.starts_with("access_") => Settlement::Refused(code.to_string()),
        _ => match reply.get("state").and_then(Value::as_str) {
            Some("withdrawn") => Settlement::Refused("access_withdrawn".into()),
            _ => Settlement::Pending,
        },
    }
}

fn elicitation_id(reply: &Value) -> Option<String> {
    ["elicitation_id", "elicitationId", "id"]
        .iter()
        .find_map(|key| reply.get(*key).and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn answer(call: &HeldCall, code: &str, message: String, prep_grant: &AccessGrant) -> Response {
    Response::error_with_data(&call.request_id, code, message, json!({"ask": prep_grant}))
}

fn pending_answer(call: &HeldCall, grant: &AccessGrant) -> Response {
    answer(
        call,
        "access_pending",
        format!(
            "Waiting for permission to {} {}. The request has gone to your head. End your turn now; you will be resumed with the answer, and then you should run this same call again.",
            operation_name(grant.operation),
            grant.path.display()
        ),
        grant,
    )
}

fn unavailable_answer(call: &HeldCall, grant: &AccessGrant, detail: &str) -> Response {
    answer(
        call,
        "access_unavailable",
        format!(
            "Outside access denied: the consent service is unavailable, so permission to {} {} could not be requested ({detail}).",
            operation_name(grant.operation),
            grant.path.display()
        ),
        grant,
    )
}

fn refused_answer(
    call: &HeldCall,
    grant: &AccessGrant,
    code: &str,
    note: Option<&str>,
) -> Response {
    let target = format!(
        "{} {}",
        operation_name(grant.operation),
        grant.path.display()
    );
    let message = match code {
        "access_declined" => match note {
            Some(note) => format!("Permission to {target} was declined: {note}"),
            None => format!("Permission to {target} was declined."),
        },
        "access_no_head" | "access_head_gone" => {
            format!("Outside access denied ({code}): there is no head to ask for permission to {target}.")
        }
        "access_not_placed" => {
            format!("Outside access denied: the request for permission to {target} could not be placed.")
        }
        "access_withdrawn" => format!("The request for permission to {target} was withdrawn."),
        _ => format!("Outside access denied ({code}) for {target}."),
    };
    answer(call, code, message, grant)
}

enum Stop {
    /// Nobody waits for the call any more and the ask goes with it.
    Withdraw,
    /// The transport dropped or the module is restarting: keep the ask so the
    /// retried call picks it up again.
    Detach,
    /// The codemode run's scope ended.
    RunEnded,
}

fn stop_signal(call: &HeldCall, route_hold: &RouteHold) -> Option<Stop> {
    let detached = || call.connection_cancel.is_cancelled() || call.drain.is_active();
    if detached() {
        return Some(Stop::Detach);
    }
    if route_hold.closed() {
        return Some(Stop::Withdraw);
    }
    if call
        .identity
        .run_scope
        .get()
        .is_some_and(|scope| scope.revoked())
    {
        return Some(Stop::RunEnded);
    }
    if !active_tool_call_is_registered(&call.active, call.route, call.corr) {
        // Connection teardown cancels the connection before it drops calls,
        // so a call gone with the connection still up was cancelled or its
        // asker's route was reclaimed.
        return Some(if detached() {
            Stop::Detach
        } else {
            Stop::Withdraw
        });
    }
    None
}

fn consent_call(
    call: &HeldCall,
    prep: &Prepared,
    method: &'static str,
    params: Value,
    timeout: Duration,
) -> ConsentCall {
    ConsentCall {
        scope: prep.selector.clone(),
        project_root: call.identity.project_root.clone(),
        session: call.identity.session.clone(),
        method,
        params,
        timeout,
    }
}

async fn withdraw(call: &HeldCall, prep: &Prepared, elicitation: &str) {
    if let Some(transport) = transport() {
        if let Err(error) = transport
            .call(consent_call(
                call,
                prep,
                "consent.withdraw",
                json!({"elicitationId": elicitation}),
                UNARY_TIMEOUT,
            ))
            .await
        {
            log::warn!("consent.withdraw failed for {elicitation}: {error:?}");
        }
    }
    let key = prep.key.clone();
    let _ = with_db(call, move |conn, _| asks::discard(conn, &key).map(|_| ())).await;
}

async fn discard(call: &HeldCall, prep: &Prepared) {
    let key = prep.key.clone();
    let _ = with_db(call, move |conn, _| asks::discard(conn, &key).map(|_| ())).await;
}

async fn hold(
    call: &HeldCall,
    grant: AccessGrant,
    deadline: Instant,
    route_hold: &RouteHold,
    own_holder: Option<String>,
) -> Hold {
    let identity = call.identity.clone();
    let tool = call.tool.clone();
    let arguments = call.arguments.clone();
    let asked = grant.clone();
    let prepared = with_db(call, move |conn, ctx| {
        prepare(
            conn,
            ctx,
            &identity,
            &tool,
            &arguments,
            &asked,
            own_holder.as_deref(),
        )
    })
    .await;
    let prep = match prepared {
        Ok(Prep::Ask(prep)) => prep,
        Ok(Prep::Busy) => return Hold::Answer(pending_answer(call, &grant)),
        Err(error) => return Hold::Answer(unavailable_answer(call, &grant, &error)),
    };
    let Some(transport) = transport() else {
        return Hold::Answer(unavailable_answer(
            call,
            &prep.grant,
            "no consent transport is connected",
        ));
    };
    let reply = transport
        .call(consent_call(
            call,
            &prep,
            "consent.request",
            prep.request.clone(),
            UNARY_TIMEOUT,
        ))
        .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => return failed(call, &prep, error).await,
    };
    let Some(elicitation) = elicitation_id(&reply).or(prep.elicitation_id.clone()) else {
        return Hold::Answer(unavailable_answer(
            call,
            &prep.grant,
            "cingulate returned no elicitation id",
        ));
    };
    {
        let key = prep.key.clone();
        let call_key = prep.call_key.clone();
        let id = elicitation.clone();
        let _ = with_db(call, move |conn, _| {
            asks::set_elicitation(conn, &key, &call_key, &id)
        })
        .await;
    }
    let mut settled = settlement(&reply);
    loop {
        if settled != Settlement::Pending {
            return finish(call, &prep, settled).await;
        }
        let now = Instant::now();
        if now >= deadline {
            park(call, &prep, &elicitation);
            return Hold::Answer(pending_answer(call, &prep.grant));
        }
        let wait = deadline - now;
        let awaited = transport.call(consent_call(
            call,
            &prep,
            "consent.await",
            json!({"elicitationId": elicitation, "wait_ms": wait.as_millis() as u64}),
            wait + UNARY_TIMEOUT,
        ));
        tokio::pin!(awaited);
        let reply = loop {
            tokio::select! {
                reply = &mut awaited => break Some(reply),
                _ = tokio::time::sleep(SIGNAL_POLL) => {
                    match stop_signal(call, route_hold) {
                        Some(Stop::Detach) => return Hold::Abandoned,
                        Some(Stop::Withdraw) => {
                            withdraw(call, &prep, &elicitation).await;
                            return Hold::Abandoned;
                        }
                        Some(Stop::RunEnded) => {
                            withdraw(call, &prep, &elicitation).await;
                            return Hold::Answer(refused_answer(call, &prep.grant, "access_withdrawn", None));
                        }
                        None if Instant::now() >= deadline => break None,
                        None => {}
                    }
                }
            }
        };
        match reply {
            None => {}
            Some(Ok(reply)) => settled = settlement(&reply),
            Some(Err(error)) => return failed(call, &prep, error).await,
        }
    }
}

/// A transport failure keeps the ask for the retry; a named refusal ends it.
async fn failed(call: &HeldCall, prep: &Prepared, error: ConsentError) -> Hold {
    match error {
        ConsentError::Unavailable(detail) => {
            Hold::Answer(unavailable_answer(call, &prep.grant, &detail))
        }
        ConsentError::Refused { code, message } => {
            if TERMINAL_REFUSALS.contains(&code.as_str()) {
                discard(call, prep).await;
                Hold::Answer(refused_answer(call, &prep.grant, &code, None))
            } else {
                Hold::Answer(unavailable_answer(
                    call,
                    &prep.grant,
                    &format!("{code}: {message}"),
                ))
            }
        }
    }
}

enum Applied {
    Run(Vec<AccessGrant>),
    /// The head no longer holds what it allowed.
    HeadNotHeld,
    /// Another dispatch claimed the answer first, or the ask is gone.
    NotClaimed,
}

async fn finish(call: &HeldCall, prep: &Prepared, settled: Settlement) -> Hold {
    match settled {
        Settlement::Pending => unreachable!("finish runs on settled answers"),
        Settlement::Deny { note } => {
            discard(call, prep).await;
            Hold::Answer(refused_answer(
                call,
                &prep.grant,
                "access_declined",
                note.as_deref(),
            ))
        }
        Settlement::Refused(code) => {
            discard(call, prep).await;
            Hold::Answer(refused_answer(call, &prep.grant, &code, None))
        }
        Settlement::Allow { scope, by_user } => {
            let holder = asks::new_holder();
            let identity = call.identity.clone();
            let key = prep.key.clone();
            let call_key = prep.call_key.clone();
            let grant = prep.grant.clone();
            let claimant = holder.clone();
            let applied = with_db(call, move |conn, ctx| {
                // A head may pass on only what it holds now: its authority is
                // checked again against the head's current policy and grants.
                if !by_user && head_policy(conn, ctx, &identity, &grant)? != HeadPolicy::Allow {
                    asks::discard(conn, &key)?;
                    return Ok(Applied::HeadNotHeld);
                }
                let Some(approved) = asks::claim(conn, &key, &call_key, &claimant)? else {
                    return Ok(Applied::NotClaimed);
                };
                if scope {
                    crate::db::access_grants::insert(
                        conn,
                        &access_grants::policy_scope_key(&identity),
                        &grant,
                    )?;
                }
                Ok(Applied::Run(approved))
            })
            .await;
            match applied {
                Ok(Applied::Run(once)) => Hold::Run {
                    once,
                    ask: prep.key.clone(),
                    holder,
                },
                Ok(Applied::HeadNotHeld) => Hold::Answer(refused_answer(
                    call,
                    &prep.grant,
                    "access_declined",
                    Some("the head no longer holds this permission itself"),
                )),
                Ok(Applied::NotClaimed) => Hold::Answer(pending_answer(call, &prep.grant)),
                Err(error) => Hold::Answer(unavailable_answer(call, &prep.grant, &error)),
            }
        }
    }
}

/// A run-scoped call that ends with `access_pending` leaves its ask with the
/// run, so the ask is withdrawn when the run's scope ends.
fn park(call: &HeldCall, prep: &Prepared, elicitation: &str) {
    let Some(scope) = call.identity.run_scope.get() else {
        return;
    };
    let Some(db) = call
        .executor
        .actor_context(&call.identity.root)
        .and_then(|ctx| ctx.db())
    else {
        return;
    };
    scope.park_ask(ParkedAsk {
        call: consent_call(
            call,
            prep,
            "consent.withdraw",
            json!({"elicitationId": elicitation}),
            UNARY_TIMEOUT,
        ),
        key: prep.key.clone(),
        db,
    });
}

/// An ask left open by a run-scoped call.
pub(super) struct ParkedAsk {
    call: ConsentCall,
    key: AskKey,
    db: Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
}

/// Withdraw the asks a codemode run left open, once its scope has ended.
pub(super) async fn withdraw_parked(parked: Vec<ParkedAsk>) {
    for ask in parked {
        if let Some(transport) = transport() {
            if let Err(error) = transport.call(ask.call.clone()).await {
                log::warn!("consent.withdraw for an ended run failed: {error:?}");
            }
        }
        let db = Arc::clone(&ask.db);
        let key = ask.key.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(conn) = db.lock() {
                let _ = asks::discard(&conn, &key);
            }
        })
        .await;
    }
}

static REATTACHED: LazyLock<StdMutex<HashSet<(String, String, u64)>>> =
    LazyLock::new(|| StdMutex::new(HashSet::new()));

/// On the first bind of a stamped worker scope in this process, re-send every
/// ask it left open under its persisted call key, so Cingulate keeps the same
/// ask; nothing is withdrawn. Approvals claimed by an earlier process are
/// discarded, never reused.
pub(super) fn reattach(
    ctx: &AppContext,
    identity: &RouteIdentity,
    runtime: &tokio::runtime::Handle,
) {
    if !access_grants::alfonso_worker(identity) {
        return;
    }
    let scope = access_grants::scope_key(identity);
    if !REATTACHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((scope.owner.clone(), scope.scope_ref.clone(), scope.epoch))
    {
        return;
    }
    let Some(db) = ctx.db() else {
        return;
    };
    let records = {
        let Ok(conn) = db.lock() else {
            return;
        };
        let Ok(records) = asks::for_scope(&conn, &scope) else {
            return;
        };
        for record in records
            .iter()
            .filter(|r| r.call_key.is_none() && !r.holder_is_live())
        {
            let _ = asks::discard(&conn, &record.key);
        }
        records
    };
    let Some(transport) = transport() else {
        return;
    };
    for record in records {
        let (Some(call_key), Some(request)) = (record.call_key.clone(), record.request.clone())
        else {
            continue;
        };
        let Ok(scope) = serde_json::from_value::<ScopeSelector>(record.selector.clone()) else {
            continue;
        };
        let call = ConsentCall {
            scope,
            project_root: identity.project_root.clone(),
            session: identity.session.clone(),
            method: "consent.request",
            params: request,
            timeout: UNARY_TIMEOUT,
        };
        let transport = Arc::clone(&transport);
        let db = Arc::clone(&db);
        runtime.spawn(async move {
            match transport.call(call).await {
                Ok(reply) => {
                    if let Some(id) = elicitation_id(&reply) {
                        let key = record.key.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            if let Ok(conn) = db.lock() {
                                let _ = asks::set_elicitation(&conn, &key, &call_key, &id);
                            }
                        })
                        .await;
                    }
                }
                Err(error) => log::warn!("consent reattach failed: {error:?}"),
            }
        });
    }
}

#[cfg(test)]
#[path = "consent_tests.rs"]
mod tests;
