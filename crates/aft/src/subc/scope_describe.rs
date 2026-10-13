//! Ask the daemon whether a scope ended before killing its shell tasks.
//! Route closure and module drain alone do not establish scope end.

use super::*;
use subc_protocol::{scope::ScopeStamp, session::ModuleControlResponseToModule};

pub(super) fn describe_timeout() -> Duration {
    timing(
        "AFT_TEST_SCOPE_DESCRIBE_TIMEOUT_MS",
        Duration::from_secs(10),
    )
}
fn poll_interval() -> Duration {
    timing("AFT_TEST_SCOPE_POLL_MS", Duration::from_secs(60))
}
fn timing(_name: &str, production: Duration) -> Duration {
    #[cfg(feature = "test-timing-hooks")]
    if let Some(ms) = std::env::var(_name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
    {
        return Duration::from_millis(ms);
    }
    production
}

/// A changed epoch for the same scope reference identifies a rebind of the
/// same agent, not its termination. Ended means the daemon remembers an end;
/// synced NotLive means the owner published its scope set without this scope.
pub(super) fn describe_answer_ends_scope(reply: &ModuleControlResponseToModule) -> bool {
    matches!(
        reply,
        ModuleControlResponseToModule::ScopeDescribe {
            status: subc_protocol::scope::ScopeStatus::Ended,
            ..
        } | ModuleControlResponseToModule::ScopeDescribe {
            status: subc_protocol::scope::ScopeStatus::NotLive,
            owner_synced: true,
            ..
        }
    )
}

pub(super) fn stamp_key(stamp: &ScopeStamp) -> (String, String, u64) {
    (
        principal_id(&Some(stamp.owner.clone())).expect("scope owner is verified"),
        stamp.scope_ref.clone(),
        stamp.scope_epoch,
    )
}

#[derive(Clone)]
pub(super) struct ScopeDescribes {
    goodbye: mpsc::UnboundedSender<ScopeStamp>,
    routed: Arc<StdMutex<HashSet<(String, String, u64)>>>,
}
impl ScopeDescribes {
    pub(super) fn routes(&self, routes: &HashMap<RouteChannel, RouteIdentity>) {
        *self
            .routed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = routes
            .values()
            .filter_map(|identity| identity.scope.as_ref())
            .map(stamp_key)
            .collect();
    }
    pub(super) fn goodbye(&self, stamp: ScopeStamp) {
        let _ = self.goodbye.send(stamp);
    }
}

pub(super) struct EndedScope {
    pub(super) stamp: ScopeStamp,
    pub(super) keys: Vec<crate::db::call_ledger::Key>,
}

pub(super) fn start(
    control: Arc<readiness::DaemonControl>,
    executor: Arc<Executor>,
    run_scopes: Arc<super::run_scope::RunScopes>,
) -> (
    ScopeDescribes,
    mpsc::UnboundedReceiver<EndedScope>,
    JoinHandle<()>,
) {
    let (goodbye, mut closes) = mpsc::unbounded_channel();
    let (ended, receiver) = mpsc::unbounded_channel();
    let routed = Arc::new(StdMutex::new(HashSet::new()));
    let handle = ScopeDescribes {
        goodbye,
        routed: routed.clone(),
    };
    let worker = tokio::spawn(async move {
        if !control.advertises(subc_protocol::scope::SCOPE_DESCRIBE_OP) {
            return;
        }
        let mut queued = HashMap::new();
        let mut retry = HashMap::new();
        let mut pending = HashSet::new();
        let mut inflight = tokio::task::JoinSet::new();
        let mut poll = tokio::time::interval_at(
            tokio::time::Instant::now() + poll_interval(),
            poll_interval(),
        );
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            while inflight.len() < 4 && !queued.is_empty() {
                let key = queued.keys().next().cloned().unwrap();
                let stamp: ScopeStamp = queued.remove(&key).unwrap();
                let control = control.clone();
                inflight.spawn(async move {
                    let reply = control.describe(&stamp).await;
                    (key, stamp, reply)
                });
            }
            tokio::select! {
                Some(stamp) = closes.recv() => {
                    // A route with no running stamped shell work needs no RPC.
                    let scopes = work_scopes(executor.clone(), run_scopes.clone()).await;
                    let key = stamp_key(&stamp);
                    if scopes.iter().any(|live| stamp_key(live) == key) && pending.insert(key.clone()) {
                        queued.insert(key, stamp);
                    }
                }
                _ = poll.tick() => {
                    let scopes = work_scopes(executor.clone(), run_scopes.clone()).await;
                    retry.retain(|key, _| scopes.iter().any(|stamp| &stamp_key(stamp) == key));
                    let routed = routed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                    for stamp in scopes {
                        let key = stamp_key(&stamp);
                        if (!routed.contains(&key) || retry.contains_key(&key)) && pending.insert(key.clone()) {
                            queued.insert(key, stamp);
                        }
                    }
                }
                Some(result) = inflight.join_next(), if !inflight.is_empty() => {
                    if let Ok((key, stamp, reply)) = result {
                        if reply.as_ref().is_ok_and(describe_answer_ends_scope) {
                            let keys = revoke(executor.clone(), &stamp).await;
                            let _ = ended.send(EndedScope { stamp, keys });
                            retry.remove(&key);
                        } else if !matches!(reply, Ok(ModuleControlResponseToModule::ScopeDescribe { status: subc_protocol::scope::ScopeStatus::Live, .. })) {
                            retry.insert(key.clone(), stamp);
                        } else {
                            retry.remove(&key);
                        }
                        // Non-definitive answers and errors retry only on the
                        // regular poll, never in a reconnect-induced busy loop.
                        pending.remove(&key);
                    }
                }
                else => break,
            }
        }
    });
    (handle, receiver, worker)
}

async fn work_scopes(executor: Arc<Executor>, _run_scopes: Arc<super::run_scope::RunScopes>) -> Vec<ScopeStamp> {
    tokio::task::spawn_blocking(move || {
        executor
            .actor_contexts()
            .into_iter()
            .flat_map(|ctx| ctx.bash_background().live_scope_stamps())
            .collect()
    })
    .await
    .unwrap_or_default()
}

async fn revoke(executor: Arc<Executor>, stamp: &ScopeStamp) -> Vec<crate::db::call_ledger::Key> {
    let entries = executor.actor_entries();
    let mut keys = Vec::new();
    for (root, _) in entries {
        let stamp = stamp.clone();
        let (result, receiver) = oneshot::channel();
        let job = executor.submit_async(
            root,
            Lane::Mutating,
            "scope-ended".into(),
            Box::new(move |ctx| {
                let scope = crate::db::call_ledger::ScopeIdentity {
                    owner: principal_id(&Some(stamp.owner.clone()))
                        .expect("scope owner is verified"),
                    scope_ref: stamp.scope_ref.clone(),
                    scope_epoch: stamp.scope_epoch,
                };
                let keys = ctx
                    .db()
                    .and_then(|db| {
                        let conn = db.lock().ok()?;
                        match crate::db::call_ledger::end_scope(
                            &conn,
                            &scope,
                            crate::db::call_ledger::now_ms(),
                        ) {
                            Ok(keys) => Some(keys),
                            Err(error) => {
                                log::warn!("scope-ended ledger settlement failed: {error}");
                                None
                            }
                        }
                    })
                    .unwrap_or_default();
                // Drop the ledger lock before killing: terminal task writes use
                // the actor's same connection and must not deadlock on it.
                ctx.bash_background().kill_scope(&stamp);
                let _ = result.send(keys);
                Response::success("scope-ended", json!({}))
            }),
        );
        let _ = await_executor_response(job, "scope-ended".into()).await;
        keys.extend(receiver.await.unwrap_or_default());
    }
    keys
}

#[cfg(all(test, not(feature = "test-timing-hooks")))]
mod tests {
    use super::*;
    #[test]
    fn production_scope_timing_is_exact() {
        assert_eq!(describe_timeout(), Duration::from_secs(10));
        assert_eq!(poll_interval(), Duration::from_secs(60));
    }
}
