//! Detached view construction. Only CAS and handle installation use the actor
//! epoch; the build owns no executor worker or reader/writer reservation.
use super::JobCancellation;
use crate::context::AppContext;
use parking_lot::{Mutex, RwLock};
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    sync::{atomic::AtomicUsize, Arc, LazyLock},
    time::{Duration, Instant},
};

/// The outcome that determines whether a failed publication is expected during
/// startup or should be treated as a fault in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicationFailureKind {
    SemanticIndexWaiting,
    /// The manifest names a payload the family blob store does not hold.
    MissingPayload,
    Failure,
}

impl PublicationFailureKind {
    /// The reason code view health reports for a failing publication.
    const fn as_str(self) -> &'static str {
        match self {
            Self::SemanticIndexWaiting => "waiting_for_semantic_index",
            Self::MissingPayload => "missing_payload",
            Self::Failure => "publication_failed",
        }
    }
}

fn classify_publication_failure(error: &str) -> PublicationFailureKind {
    if error == crate::context::VIEW_PUBLICATION_WAITING_FOR_SEMANTIC_INDEX {
        PublicationFailureKind::SemanticIndexWaiting
    } else if error.contains("manifest references missing") {
        // `ViewError::MissingBlob` from the closure check, or the manifest
        // joiner's own missing-payload error during a cold build.
        PublicationFailureKind::MissingPayload
    } else {
        PublicationFailureKind::Failure
    }
}

/// The shortest time between two warnings about one root's failing
/// publication, after the first two. Retries back off from one second to five
/// minutes, so once the delay passes this interval every retry is logged.
const FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// One retry deadline for both inline and detached publications of a root.
/// Keeping it on the context preserves the delay when another scheduler wake
/// supersedes a detached job without changing the checkout.
pub(crate) struct PublicationRetry {
    error: Option<String>,
    kind: Option<PublicationFailureKind>,
    failures: usize,
    /// Real failures in the current streak, counted apart from semantic-index
    /// waits so a warm-up wait never uses up the warnings a real fault gets.
    real_failures: usize,
    /// When the last warning for this streak was logged.
    last_warned: Option<Instant>,
    /// Real failures not logged since the last warning.
    suppressed: usize,
    log_interval: Duration,
    delay: Duration,
    due: Option<Instant>,
    initial: Duration,
    maximum: Duration,
}

impl Default for PublicationRetry {
    fn default() -> Self {
        Self {
            error: None,
            kind: None,
            failures: 0,
            real_failures: 0,
            last_warned: None,
            suppressed: 0,
            log_interval: FAILURE_LOG_INTERVAL,
            delay: Duration::ZERO,
            due: None,
            initial: Duration::from_secs(1),
            maximum: Duration::from_secs(300),
        }
    }
}

impl PublicationRetry {
    pub(crate) fn due(&self) -> Option<Instant> {
        self.due
    }

    /// The failing publication streak view health reports, or `None` while
    /// publications succeed or only wait for the semantic index.
    pub(crate) fn failure_snapshot(&self) -> Option<crate::context::ViewPublicationFailure> {
        let kind = self
            .kind
            .filter(|kind| *kind != PublicationFailureKind::SemanticIndexWaiting)?;
        Some(crate::context::ViewPublicationFailure {
            reason: kind.as_str(),
            repeat_count: self.failures,
            error: self.error.clone().unwrap_or_default(),
            next_retry_ms: self
                .due
                .map(|due| due.saturating_duration_since(Instant::now()).as_millis() as u64),
        })
    }

    pub(crate) fn reset(&mut self, root: &std::path::Path) {
        if self.failures > 1 {
            log::info!(
                "content-addressed view publication retry reset root={} repeat_count={} error={}",
                root.display(),
                self.failures,
                self.error.as_deref().unwrap_or_default()
            );
        }
        self.error = None;
        self.kind = None;
        self.failures = 0;
        self.real_failures = 0;
        self.last_warned = None;
        self.suppressed = 0;
        self.delay = Duration::ZERO;
        self.due = None;
    }

    fn failed(&mut self, root: &std::path::Path, error: &str) {
        // Failed generations have fresh artifact names, so comparing complete
        // error strings would turn the same storage fault into a new failure
        // every attempt. Only progress or a checkout change ends the streak.
        if self.failures > 0 {
            self.failures = self.failures.saturating_add(1);
            self.delay = (self.delay * 2).min(self.maximum);
        } else {
            self.failures = 1;
            self.delay = self.initial;
        }
        self.error = Some(error.to_owned());
        let kind = classify_publication_failure(error);
        self.kind = Some(kind);
        self.due = Some(Instant::now() + self.delay);
        let waiting = kind == PublicationFailureKind::SemanticIndexWaiting;
        if !waiting {
            self.real_failures = self.real_failures.saturating_add(1);
        }
        // Report the first two real failures, then every later retry at most
        // once per `log_interval`, counting the retries left unlogged, so a
        // publication that never recovers stays visible in the log. Waiting
        // for the semantic index is expected during warm-up.
        let warn = !waiting
            && (self.real_failures <= 2
                || self
                    .last_warned
                    .is_none_or(|at| at.elapsed() >= self.log_interval));
        if waiting {
            log::debug!(
                "content-addressed view publication waiting root={} repeat_count={} retry_ms={} error={}",
                root.display(), self.failures, self.delay.as_millis(), error
            );
            #[cfg(test)]
            tests::record_failure(root, self.delay, self.failures, false);
        } else if warn {
            log::warn!(
                "content-addressed view publication failed root={} reason={} repeat_count={} unlogged_since_last_warning={} retry_ms={} error={}",
                root.display(), kind.as_str(), self.failures, self.suppressed, self.delay.as_millis(), error
            );
            self.last_warned = Some(Instant::now());
            self.suppressed = 0;
            #[cfg(test)]
            tests::record_failure(root, self.delay, self.failures, true);
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            #[cfg(test)]
            tests::record_failure(root, self.delay, self.failures, false);
        }
    }
}

#[derive(Clone)]
struct Target {
    ctx: Arc<AppContext>,
    epoch: Arc<RwLock<()>>,
    /// The actor's count of detached threads waiting to write `epoch`, which
    /// running maintenance reads as writer demand.
    detached_writers: Arc<AtomicUsize>,
}
thread_local! { static CURRENT: RefCell<Option<Target>> = const { RefCell::new(None) }; }

pub(super) struct ActorScope(Option<Target>);
impl ActorScope {
    pub(super) fn install(
        ctx: Arc<AppContext>,
        epoch: Arc<RwLock<()>>,
        detached_writers: Arc<AtomicUsize>,
    ) -> Self {
        // From here on, a call-graph query on `ctx` running outside a scope
        // does not publish an on-demand build inline on its own thread; it
        // leaves the build to a query or drain running under a scope like
        // this one (see `AppContext::schedule_lazy_callgraph_build`).
        ctx.note_publication_scope();
        Self(CURRENT.with(|slot| {
            slot.replace(Some(Target {
                ctx,
                epoch,
                detached_writers,
            }))
        }))
    }
}
impl Drop for ActorScope {
    fn drop(&mut self) {
        CURRENT.with(|slot| slot.replace(self.0.take()));
    }
}

struct Running {
    root: PathBuf,
    context: usize,
    token: JobCancellation,
    phase: String,
    started: Instant,
    phase_started: Instant,
    paths: BTreeSet<Vec<u8>>,
}
static JOBS: LazyLock<Mutex<HashMap<u64, Running>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) fn health_snapshot() -> serde_json::Value {
    let jobs = JOBS.lock();
    serde_json::Value::Array(jobs.iter().map(|(id, job)| serde_json::json!({
        "id": id, "kind": "views.publication", "root": job.root, "phase": job.phase,
        "elapsed_ms": job.started.elapsed().as_millis() as u64,
        "phase_ms": job.phase_started.elapsed().as_millis() as u64,
        // A health report only observes: the full check can stat the root and
        // signal the job, and this runs on the module's frame loop under JOBS.
        "cancel_requested": job.token.cancel_already_requested(), "barrier_holder": false,
    })).collect())
}

pub(super) fn running_for_context(ctx: &Arc<AppContext>) -> bool {
    running_for(ctx)
}

/// True while a view publication for `ctx` is queued or running.
pub(crate) fn running_for(ctx: &AppContext) -> bool {
    let address = ctx as *const AppContext as usize;
    JOBS.lock().values().any(|job| job.context == address)
}

/// What the queued or running view publications for `ctx` have yet to
/// install: `None` when none is running, otherwise the root-relative paths
/// they carry. An empty set means a publication of the whole checkout (after
/// lost watcher events or an ignore-rule change, for example), whose changed
/// files are not known in advance.
pub(crate) fn unpublished_paths_for(ctx: &AppContext) -> Option<BTreeSet<Vec<u8>>> {
    let address = ctx as *const AppContext as usize;
    let jobs = JOBS.lock();
    let mut running = jobs
        .values()
        .filter(|job| job.context == address)
        .peekable();
    running.peek()?;
    let mut paths = BTreeSet::new();
    for job in running {
        if job.paths.is_empty() {
            return Some(BTreeSet::new());
        }
        paths.extend(job.paths.iter().cloned());
    }
    Some(paths)
}

/// True when [`schedule`] called on this thread for `ctx` hands the work to a
/// detached worker instead of publishing inline on the calling thread.
pub(crate) fn detaches_for(ctx: &AppContext) -> bool {
    CURRENT.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|target| std::ptr::eq(ctx, target.ctx.as_ref()))
    })
}

/// Request id and census kind of a view publication's cold-build admission.
const PUBLICATION_LIMITER_KIND: &str = "view publication";

/// A detached publication's cold-build slot. It covers preparation through
/// assembly and manifest loading. [`PublicationSlot::release`] runs before the
/// publication waits for the query-pin file lock, the root actor's epoch write
/// gate or the process-wide job table, so those waits hold no build capacity.
/// Each acquired slot logs once when released, with how long the publication
/// queued for it and held it. A publication cancelled while queued logs no
/// release line. Failed attempts are already spaced by the shared retry
/// backoff, so the line needs no rate limit.
struct PublicationSlot {
    permit: Option<crate::cold_build_limiter::ColdBuildPermit>,
    root: PathBuf,
    wait: Duration,
    acquired: Instant,
}

impl PublicationSlot {
    /// Queues in the limiter's maintenance class, behind requests of that
    /// class that queued earlier. `None` when the publication is cancelled
    /// while it waits.
    fn acquire(
        limiter: &Arc<crate::cold_build_limiter::ColdBuildLimiter>,
        root: &std::path::Path,
        cancelled: impl Fn() -> bool,
    ) -> Option<Self> {
        let started = Instant::now();
        let request = crate::cold_build_limiter::ColdBuildAdmissionRequest::labelled_with_root(
            root.display().to_string(),
            PUBLICATION_LIMITER_KIND,
            crate::cold_build_limiter::ColdBuildAdmissionClass::Maintenance,
        );
        let permit = crate::cold_build_limiter::acquire_blocking_while_cancellable_with_limiter(
            limiter,
            PUBLICATION_LIMITER_KIND,
            request,
            || true,
            cancelled,
        )?;
        Some(Self {
            permit: Some(permit),
            root: root.to_owned(),
            wait: started.elapsed(),
            acquired: Instant::now(),
        })
    }

    fn release(&mut self) {
        if let Some(permit) = self.permit.take() {
            drop(permit);
            log::info!(
                "view publication cold-build slot released root={} wait_ms={} hold_ms={}",
                self.root.display(),
                self.wait.as_millis(),
                self.acquired.elapsed().as_millis()
            );
        }
    }
}

impl Drop for PublicationSlot {
    fn drop(&mut self) {
        self.release();
    }
}

struct Lifecycle(u64);
impl Lifecycle {
    fn phase(&self, phase: &str) -> crate::views::Result<()> {
        let mut jobs = JOBS.lock();
        let job = jobs.get_mut(&self.0).expect("live view publication");
        if job.token.cancel_requested_before_commit() {
            return Err(crate::views::ViewError::InvalidManifest(
                "view publication superseded".to_owned(),
            ));
        }
        if job.phase != phase {
            job.phase = phase.to_owned();
            job.phase_started = Instant::now();
        }
        #[cfg(test)]
        let root = job.root.clone();
        drop(jobs);
        #[cfg(test)]
        test_gate(self.0, &root, phase);
        Ok(())
    }
}
impl Drop for Lifecycle {
    fn drop(&mut self) {
        let job = JOBS.lock().remove(&self.0);
        #[cfg(test)]
        if let Some(job) = job.as_ref() {
            tests::settled(self.0, &job.root);
        }
        drop(job);
    }
}

/// Take the actor's epoch write gate from this detached thread, counted as
/// writer demand while it waits: a configure tail holding the read gate then
/// steps aside at its next unit instead of keeping this install (and, the lock
/// being task-fair, every new reader queued behind it) waiting for the rest of
/// the tail.
fn write_epoch_as_detached_writer(target: &Target) -> parking_lot::RwLockWriteGuard<'_, ()> {
    target
        .detached_writers
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let guard = target.epoch.write();
    target
        .detached_writers
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    guard
}

/// The standalone (stdin/stdout) runtime's stand-in for a root actor's epoch.
///
/// The subc daemon runs configure maintenance as executor jobs inside a root
/// actor, so [`schedule`] detaches a view publication there. The standalone
/// runtime serves requests and maintenance on one thread with no actor; without
/// this, the first publication of a large checkout ran inline on that thread
/// and every request waited for it. Standalone maintenance installs this
/// epoch with [`install_standalone_scope`], so publications detach, and each
/// standalone request holds its read side through [`standalone_request_gate`],
/// so a detached publication's commit (the write side) never runs in the
/// middle of a request or a maintenance unit.
struct StandaloneEpoch {
    epoch: Arc<RwLock<()>>,
    detached_writers: Arc<AtomicUsize>,
}

static STANDALONE_EPOCH: LazyLock<StandaloneEpoch> = LazyLock::new(|| StandaloneEpoch {
    epoch: Arc::new(RwLock::new(())),
    detached_writers: Arc::new(AtomicUsize::new(0)),
});

/// Held while a standalone request or maintenance unit runs. The read is
/// recursive because a request can dispatch a nested request (`tool_call`).
/// Deferred workers can overlap, so only nested reads on the same thread bypass
/// a queued writer. New requests use the task-fair read and cannot prolong a
/// publication's wait by continually joining the in-flight readers.
pub struct StandaloneGate {
    _read: parking_lot::RwLockReadGuard<'static, ()>,
}

pub fn standalone_request_gate() -> StandaloneGate {
    let read = if standalone_read_is_nested() {
        STANDALONE_EPOCH.epoch.read_recursive()
    } else {
        STANDALONE_EPOCH.epoch.read()
    };
    STANDALONE_READ_DEPTH.with(|depth| depth.set(depth.get() + 1));
    StandaloneGate { _read: read }
}

thread_local! {
    static STANDALONE_READ_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn standalone_read_is_nested() -> bool {
    STANDALONE_READ_DEPTH.with(|depth| depth.get() > 0)
}

impl Drop for StandaloneGate {
    fn drop(&mut self) {
        STANDALONE_READ_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

/// Lets view publications scheduled by standalone configure maintenance on
/// `ctx` detach instead of running inline, for as long as the returned value
/// lives. It also holds the request gate, as a maintenance unit is serialized
/// with publication commits the same way a request is.
pub struct StandaloneScope {
    _scope: ActorScope,
    _gate: StandaloneGate,
}

pub fn install_standalone_scope(ctx: Arc<AppContext>) -> StandaloneScope {
    StandaloneScope {
        _gate: standalone_request_gate(),
        _scope: ActorScope::install(
            ctx,
            Arc::clone(&STANDALONE_EPOCH.epoch),
            Arc::clone(&STANDALONE_EPOCH.detached_writers),
        ),
    }
}

pub(crate) fn cancel_for_context(ctx: &AppContext) {
    let context = ctx as *const AppContext as usize;
    for job in JOBS.lock().values().filter(|job| job.context == context) {
        job.token.request_cancel();
    }
}

/// Scheduling succeeds once ownership has moved to the detached worker.
/// Callers outside any actor scope (tests and one-shot tools that drain
/// configure maintenance themselves) retain the synchronous API.
pub(crate) fn schedule(
    ctx: &AppContext,
    mut paths: BTreeSet<Vec<u8>>,
    allow_semantic_blob_put: bool,
) -> Result<(), String> {
    if ctx.retire_deleted_view_root() {
        return Ok(());
    }
    let target = CURRENT.with(|slot| slot.borrow().clone());
    let Some(target) = target.filter(|target| std::ptr::eq(ctx, target.ctx.as_ref())) else {
        if ctx
            .view_publication_retry()
            .lock()
            .due()
            .is_some_and(|due| Instant::now() < due)
        {
            return Err("view publication retry is not due".to_owned());
        }
        let result = ctx
            .publish_view_paths(paths, allow_semantic_blob_put)
            .map(|_| ());
        let root = ctx.canonical_cache_root_opt().unwrap_or_default();
        let mut retry = ctx.view_publication_retry().lock();
        match &result {
            Ok(()) => retry.reset(&root),
            Err(error) => retry.failed(&root, error),
        }
        return result;
    };
    let root = ctx
        .canonical_cache_root_opt()
        .ok_or("view root is not configured")?;
    let content_generation = ctx.configure_content_generation();
    if ctx.subc_unbound_quiesced() || !root.is_dir() {
        return Err("view root is unbound or missing".to_owned());
    }
    let token = JobCancellation::new()
        .with_root(&root)
        .with_lifecycle(ctx.subc_lifecycle_admission());
    token.mark_running();
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    {
        let mut jobs = JOBS.lock();
        for job in jobs.values_mut().filter(|job| job.root == root) {
            job.token.request_cancel();
            if paths.is_empty() || job.paths.is_empty() {
                paths.clear();
            } else {
                paths.extend(job.paths.iter().cloned());
            }
        }
        jobs.insert(
            id,
            Running {
                root,
                context: Arc::as_ptr(&target.ctx) as usize,
                token: token.clone(),
                phase: "manifest".to_owned(),
                started: Instant::now(),
                phase_started: Instant::now(),
                paths: paths.clone(),
            },
        );
    }
    let lifecycle = Lifecycle(id);
    std::thread::Builder::new()
        .name("aft-view-publication".to_owned())
        .stack_size(super::EXECUTOR_WORKER_STACK_BYTES)
        .spawn(move || {
            let _cancellation = super::install_job_cancellation(token.clone());
            // Whether the last attempt could publish without a linked
            // worktree's derived call graph. A query may need that graph while
            // such an attempt runs; see the follow-up after the loop.
            let mut ran_deferred = false;
            loop {
                if target.ctx.retire_deleted_view_root() {
                    break;
                }
                if token.cancel_requested_before_commit()
                    || target.ctx.configure_content_generation() != content_generation
                    || !target.ctx.config().views.enabled
                {
                    break;
                }
                let retry_due = target.ctx.view_publication_retry().lock().due();
                if let Some(wait) = retry_due
                    .and_then(|due| due.checked_duration_since(Instant::now()))
                {
                    // Recheck the root's shared deadline while waiting: a watcher
                    // edit can reset it without waiting for the old delay to end.
                    lifecycle.phase("retry_wait").ok();
                    token.wait_for_cancellation(wait.min(Duration::from_secs(1)));
                    continue;
                }
                ran_deferred = target.ctx.defer_callgraph_build();
                let result = (|| {
                    lifecycle
                        .phase("manifest")
                        .map_err(|error| error.to_string())?;
                    let mut slot = PublicationSlot::acquire(
                        &target.ctx.cold_build_limiter(),
                        &target.ctx.canonical_cache_root(),
                        || token.cancel_requested_before_commit(),
                    )
                    .ok_or_else(|| "view publication superseded".to_owned())?;
                    let _progress = crate::cold_build_limiter::progress::start(
                        &target.ctx.canonical_cache_root(),
                        "view publication",
                        if paths.is_empty() { None } else { Some(paths.len()) },
                        crate::cold_build_limiter::progress::StartLog::Quiet,
                    );
                    crate::cold_build_limiter::progress::phase("preparing", if paths.is_empty() { None } else { Some(paths.len()) });
                    let mut prepared = target.ctx.prepare_view_paths(
                        paths.clone(),
                        allow_semantic_blob_put,
                        &mut |phase| {
                            if phase == crate::context::VIEW_PUBLICATION_PIN_PHASE {
                                slot.release();
                            }
                            lifecycle.phase(phase)
                        },
                    )?;
                    // The remaining steps wait for the actor's epoch write gate
                    // and the process-wide job table; none of them builds.
                    slot.release();
                    crate::cold_build_limiter::progress::advance(paths.len());
                    crate::cold_build_limiter::progress::phase("publishing", None);
                    // Equivalent unbind/rebind retains this root actor and its epoch.
                    // Content-changing configure is rejected by commit_view_update.
                    let report = {
                        let _epoch = write_epoch_as_detached_writer(&target);
                        lifecycle.phase("cas").map_err(|error| error.to_string())?;
                        // Sealing and cancellation are atomic with respect to superseding
                        // submissions, which hold JOBS while signaling this token.
                        let _jobs = JOBS.lock();
                        #[cfg(test)]
                        let _barrier = CommitBarrierMark::enter();
                        if token.cancel_requested_before_commit() {
                            return Err("view publication superseded".to_owned());
                        }
                        #[cfg(test)]
                        let cas_started = Instant::now();
                        let report = target.ctx.commit_view_update(&mut prepared)?;
                        #[cfg(test)]
                        tests::record_cas(target.ctx.canonical_cache_root(), cas_started.elapsed());
                        let _ = token.try_seal_committed();
                        report
                    };
                    // Path status is written only now, after the write gate and
                    // the job table are released, so neither is held while it
                    // opens SQLite and writes.
                    prepared.settle_status();
                    log::info!(
                    "content-addressed view publication published={} blob_puts={} pending_paths={} root={}",
                    report.published,
                    report.blob_puts,
                    report.pending_paths.len(),
                    target.ctx.canonical_cache_root().display()
                );
                    Ok::<(), String>(())
                })();
                match result {
                    Ok(()) => {
                        // Resolve the root before taking the retry lock, so this
                        // lock never waits on another one while held.
                        let root = target.ctx.canonical_cache_root();
                        target.ctx.view_publication_retry().lock().reset(&root);
                        break;
                    }
                    Err(error) => {
                        // Root deletion also cancels its token. Retire the view
                        // before testing cancellation, or the short circuit
                        // would leave readers attached to a vanished checkout.
                        if target.ctx.retire_deleted_view_root() || target.ctx.view_runtime_snapshot().is_none()
                            || token.cancel_requested_before_commit()
                            || target.ctx.configure_content_generation() != content_generation {
                            break;
                        }
                        // Retain the complete path union until a successful install;
                        // scheduling acknowledgement is not publication completion.
                        let root = target.ctx.canonical_cache_root();
                        target.ctx.view_publication_retry().lock().failed(&root, &error);
                    }
                }
            }
            drop(lifecycle);
            // A query needed the worktree's call graph while this job published
            // without it. Now that this job has left the job table, schedule
            // that build. The scheduled publication builds the graph instead of
            // deferring it, so it does not trigger another follow-up like this.
            if ran_deferred && target.ctx.lazy_callgraph_build_outstanding() {
                let _scope = ActorScope::install(
                    Arc::clone(&target.ctx),
                    Arc::clone(&target.epoch),
                    Arc::clone(&target.detached_writers),
                );
                target.ctx.schedule_lazy_callgraph_build();
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
#[path = "view_publication_tests.rs"]
mod tests;

#[cfg(test)]
thread_local! {
    static IN_COMMIT_BARRIER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks this thread as holding the publication commit barrier (the root
/// actor's write gate and the job table) for as long as it lives.
#[cfg(test)]
struct CommitBarrierMark;

#[cfg(test)]
impl CommitBarrierMark {
    fn enter() -> Self {
        IN_COMMIT_BARRIER.set(true);
        Self
    }
}

#[cfg(test)]
impl Drop for CommitBarrierMark {
    fn drop(&mut self) {
        IN_COMMIT_BARRIER.set(false);
    }
}

/// Whether this thread is inside a publication's commit barrier.
#[cfg(test)]
pub(crate) fn in_commit_barrier_for_test() -> bool {
    IN_COMMIT_BARRIER.get()
}

#[cfg(test)]
fn test_gate(id: u64, root: &std::path::Path, phase: &str) {
    tests::phase_gate(id, root, phase);
}
