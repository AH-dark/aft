use super::*;
use crate::{
    config::Config,
    context::{CallgraphStoreAccess, ViewRuntimeSnapshot},
    executor::{Executor, ExecutorConfig, Lane},
    parser::TreeSitterProvider,
    path_identity::ProjectRootId,
    protocol::Response,
};
use crossbeam_channel::{Receiver, Sender};
use std::{path::Path, time::Duration};

#[test]
fn semantic_index_wait_is_classified_without_changing_retry_backoff() {
    let message = crate::context::VIEW_PUBLICATION_WAITING_FOR_SEMANTIC_INDEX;
    assert_eq!(
        classify_publication_failure(message),
        PublicationFailureKind::SemanticIndexWaiting
    );
    assert_eq!(
        classify_publication_failure("view I/O failed writing manifest: permission denied"),
        PublicationFailureKind::Failure
    );

    let root = tempfile::tempdir().unwrap();
    let mut retry = PublicationRetry::default();
    retry.failed(root.path(), message);
    assert_eq!(retry.failures, 1);
    assert_eq!(retry.delay, Duration::from_secs(1));
    retry.failed(root.path(), message);
    assert_eq!(retry.failures, 2);
    assert_eq!(retry.delay, Duration::from_secs(2));
    assert!(retry.due().is_some());
}

#[test]
fn semantic_index_waits_do_not_silence_the_first_real_failure() {
    let root = tempfile::tempdir().unwrap();
    let failures = Failures::new(root.path().to_path_buf());
    let waiting = crate::context::VIEW_PUBLICATION_WAITING_FOR_SEMANTIC_INDEX;
    let real = "view I/O failed writing semantic blobs: permission denied";
    let mut retry = PublicationRetry::default();
    retry.failed(root.path(), waiting);
    retry.failed(root.path(), waiting);
    assert!(!failures.next().3, "a semantic-index wait must not warn");
    assert!(!failures.next().3, "a semantic-index wait must not warn");
    retry.failed(root.path(), real);
    let (_, delay, count, warned) = failures.next();
    assert!(warned, "the first real failure after waits must warn");
    assert_eq!(count, 3, "waits still count toward the shared retry streak");
    assert_eq!(delay, Duration::from_secs(4), "retry backoff is unchanged");
    retry.failed(root.path(), real);
    assert!(failures.next().3, "the second real failure warns too");
    retry.failed(root.path(), real);
    assert!(
        !failures.next().3,
        "a repeat inside the log interval is counted, not logged"
    );
}

/// A publication that keeps failing over a missing payload stays visible:
/// health names a typed reason, and the log keeps reporting the streak once
/// per interval with the number of retries it did not log.
#[test]
fn persistent_missing_payload_failures_keep_logging_and_report_a_typed_reason() {
    let root = tempfile::tempdir().unwrap();
    let failures = Failures::new(root.path().to_path_buf());
    let error = "manifest references missing Callgraph blob 00ff";
    assert_eq!(
        classify_publication_failure(error),
        PublicationFailureKind::MissingPayload
    );
    let mut retry = PublicationRetry::default();
    assert!(retry.failure_snapshot().is_none());
    for _ in 0..3 {
        retry.failed(root.path(), error);
    }
    assert!(failures.next().3);
    assert!(failures.next().3);
    assert!(!failures.next().3, "inside the log interval");
    assert_eq!(retry.suppressed, 1);

    // Every later backoff step that falls outside the interval is logged.
    for repeat in 4..=6 {
        retry.last_warned = Instant::now().checked_sub(retry.log_interval);
        retry.failed(root.path(), error);
        let (_, _, count, warned) = failures.next();
        assert_eq!(count, repeat);
        assert!(warned, "backoff step {repeat} is logged");
        assert_eq!(retry.suppressed, 0);
    }

    let failure = retry
        .failure_snapshot()
        .expect("a failing streak is reported");
    assert_eq!(failure.reason, "missing_payload");
    assert_eq!(failure.repeat_count, 6);
    assert_eq!(failure.error, error);
    assert!(failure.next_retry_ms.is_some());
    retry.reset(root.path());
    assert!(retry.failure_snapshot().is_none());
}

static CAS_TIMINGS: LazyLock<Mutex<HashMap<PathBuf, Vec<Duration>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
pub(super) fn record_cas(root: PathBuf, elapsed: Duration) {
    eprintln!(
        "view pointer CAS and handle swap: {} ms",
        elapsed.as_millis()
    );
    CAS_TIMINGS.lock().entry(root).or_default().push(elapsed);
}

static SETTLED: LazyLock<Mutex<HashMap<PathBuf, Sender<u64>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

type Failure = (Instant, Duration, usize, bool);

#[test]
fn standalone_queued_writer_blocks_new_readers_but_not_nested_dispatch() {
    let outer = standalone_request_gate();
    let (writer_tx, writer_rx) = crossbeam_channel::bounded(1);
    let writer = std::thread::spawn(move || {
        let _write = STANDALONE_EPOCH.epoch.write();
        writer_tx.send(()).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while STANDALONE_EPOCH.epoch.try_read().is_some() {
        assert!(Instant::now() < deadline, "writer must reach the read gate");
        std::thread::yield_now();
    }
    let nested = standalone_request_gate();
    let admitted_new_reader = std::thread::spawn(|| {
        if standalone_read_is_nested() {
            STANDALONE_EPOCH.epoch.try_read_recursive().is_some()
        } else {
            STANDALONE_EPOCH.epoch.try_read().is_some()
        }
    })
    .join()
    .unwrap();
    drop(nested);
    drop(outer);
    writer_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("writer progress after readers finish");
    writer.join().unwrap();
    assert!(
        !admitted_new_reader,
        "an unrelated deferred reader must not bypass a queued publication writer"
    );
}
static FAILURES: LazyLock<Mutex<HashMap<PathBuf, Sender<Failure>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn record_failure(root: &Path, delay: Duration, count: usize, warned: bool) {
    if let Some(tx) = FAILURES.lock().get(root) {
        let _ = tx.send((Instant::now(), delay, count, warned));
    }
}

struct Failures {
    root: PathBuf,
    rx: Receiver<Failure>,
}
impl Failures {
    fn new(root: PathBuf) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        FAILURES.lock().insert(root.clone(), tx);
        Self { root, rx }
    }
    fn next(&self) -> Failure {
        self.rx
            .recv_timeout(Duration::from_secs(60))
            .expect("publication failure")
    }
}
impl Drop for Failures {
    fn drop(&mut self) {
        FAILURES.lock().remove(&self.root);
    }
}

pub(super) fn settled(id: u64, root: &Path) {
    if let Some(tx) = SETTLED.lock().get(root) {
        let _ = tx.send(id);
    }
}

struct Completions {
    root: PathBuf,
    rx: Receiver<u64>,
}
impl Completions {
    fn new(root: PathBuf) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        SETTLED.lock().insert(root.clone(), tx);
        Self { root, rx }
    }
    fn next(&self) -> u64 {
        self.rx
            .recv_timeout(Duration::from_secs(60))
            .expect("publication completion")
    }
}
impl Drop for Completions {
    fn drop(&mut self) {
        SETTLED.lock().remove(&self.root);
    }
}

struct GateEntry {
    phase: &'static str,
    /// Matching phase reports to let through before the gate holds one.
    skip: usize,
    started: Sender<u64>,
    release: Receiver<()>,
}
static GATES: LazyLock<Mutex<HashMap<PathBuf, GateEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PHASES: LazyLock<Mutex<HashMap<PathBuf, Vec<(u64, String)>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn phase_gate(id: u64, root: &Path, phase: &str) {
    PHASES
        .lock()
        .entry(root.to_owned())
        .or_default()
        .push((id, phase.to_owned()));
    let gate = {
        let mut gates = GATES.lock();
        match gates.get_mut(root) {
            Some(gate) if gate.phase == phase && gate.skip > 0 => {
                gate.skip -= 1;
                None
            }
            Some(gate) if gate.phase == phase => gates.remove(root),
            _ => None,
        }
    };
    if let Some(gate) = gate {
        let _ = gate.started.send(id);
        // Only dropping Gate releases the build, even if a test's hang guard fires.
        gate.release.recv().expect("release publication gate");
    }
}
struct Gate {
    arrived: Receiver<u64>,
    release: Sender<()>,
}
impl Gate {
    fn new(root: &Path, phase: &'static str) -> Self {
        Self::after(root, phase, 0)
    }
    /// Holds the publication at the report of `phase` that follows `skip`
    /// earlier reports of the same phase on `root`.
    fn after(root: &Path, phase: &'static str, skip: usize) -> Self {
        let (started, arrived) = crossbeam_channel::bounded(1);
        let (release, released) = crossbeam_channel::bounded(1);
        GATES.lock().insert(
            root.to_owned(),
            GateEntry {
                phase,
                skip,
                started,
                release: released,
            },
        );
        Self { arrived, release }
    }
    fn started(&self) -> u64 {
        self.arrived
            .recv_timeout(Duration::from_secs(60))
            .expect("publication reached gate")
    }
}
impl Drop for Gate {
    fn drop(&mut self) {
        let _ = self.release.try_send(());
    }
}

struct Fixture {
    project: tempfile::TempDir,
    _storage: tempfile::TempDir,
    ctx: Arc<AppContext>,
    executor: Executor,
    root: ProjectRootId,
    view: crate::views::ViewStore,
    initial: String,
}
impl Fixture {
    fn new() -> Self {
        Self::with_files(0)
    }
    fn with_files(files: usize) -> Self {
        let _ = env_logger::Builder::new()
            .is_test(true)
            .filter_module("aft::views::generation", log::LevelFilter::Info)
            .try_init();
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(project.path()).unwrap();
        git(&root_path, &["init", "--quiet"]);
        std::fs::write(root_path.join("tracked.rs"), "pub fn previous() {}\n").unwrap();
        for file in 0..files {
            std::fs::write(
                root_path.join(format!("file_{file}.rs")),
                format!("pub fn previous_{file}() {{}}\n"),
            )
            .unwrap();
        }
        commit(&root_path);
        let mut config = Config {
            project_root: Some(root_path.clone()),
            storage_dir: Some(storage.path().to_owned()),
            indexes: crate::config::IndexesConfig {
                trigram: false,
                semantic: false,
                callgraph: true,
            },
            ..Config::default()
        };
        config.views.enabled = true;
        let ctx = Arc::new(AppContext::new(Box::new(TreeSitterProvider::new()), config));
        ctx.set_canonical_cache_root(root_path.clone());
        let scope = crate::path_identity::project_scope_key(&root_path);
        let view = crate::views::ViewStore::open(storage.path(), &scope).unwrap();
        ctx.install_view_runtime(
            ViewRuntimeSnapshot {
                query_pin: None,
                storage: storage.path().to_owned(),
                family: "publication-fixture".to_owned(),
                scope,
                view_dir: view.view_dir().to_owned(),
                generation: None,
                manifest: None,
                head_fingerprint: String::new(),
                head_metadata: crate::alias::capture_git_head_metadata(&root_path, None).unwrap(),
                pending_paths: BTreeSet::new(),
                pending_inputs: Default::default(),
            },
            None,
        );
        let initial = ctx
            .publish_view_paths(BTreeSet::new(), true)
            .unwrap()
            .generation
            .unwrap();
        let root = ProjectRootId::from_path(&root_path).unwrap();
        let executor = Executor::with_config(ExecutorConfig {
            pool_size: 4,
            read_cap: 2,
            actor_cap: 3,
            heavy_permits: 2,
            drr_quantum: 1,
        });
        executor.register_actor(root.clone(), Arc::clone(&ctx));
        Self {
            project,
            _storage: storage,
            ctx,
            executor,
            root,
            view,
            initial,
        }
    }
    /// The root spelling the publication job records and reports: the context's
    /// canonical cache root (a `std::fs::canonicalize` result, verbatim on
    /// Windows). `ProjectRootId::as_path()` is a different spelling there, so
    /// gates, timing tables and health rows are keyed on this one.
    fn job_root(&self) -> PathBuf {
        self.ctx
            .canonical_cache_root_opt()
            .expect("fixture configured a canonical cache root")
    }
    fn change(&self, symbol: &str) {
        std::fs::write(
            self.project.path().join("tracked.rs"),
            format!("pub fn {symbol}() {{}}\n"),
        )
        .unwrap();
        commit(self.project.path());
    }
    fn schedule(&self) {
        self.schedule_paths(BTreeSet::from([b"tracked.rs".to_vec()]));
    }
    fn schedule_paths(&self, paths: BTreeSet<Vec<u8>>) {
        let response = self
            .executor
            .submit(
                self.root.clone(),
                Lane::MaintenanceCommit,
                "views.schedule".to_owned(),
                Box::new(move |ctx| match schedule(ctx, paths, true) {
                    Ok(()) => Response::success("views.schedule", serde_json::json!({})),
                    Err(error) => Response::error("views.schedule", "publication", error),
                }),
            )
            .recv_timeout(Duration::from_secs(60))
            .unwrap();
        assert!(response.success, "{:?}", response.data);
    }
    fn wait_idle(&self) {
        self.wait_idle_for(Duration::from_secs(10));
    }
    fn wait_idle_for(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while running_for_context(&self.ctx) {
            assert!(
                Instant::now() < deadline,
                "publication did not settle: {}",
                health_snapshot()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_cold_parity(&self) {
        let runtime = self.ctx.view_runtime_snapshot().unwrap();
        let generation = runtime.generation.unwrap();
        let manifest = runtime.manifest.unwrap();
        let cold_dir = tempfile::tempdir().unwrap();
        let cold = cold_dir.path().join("cold.sqlite");
        let blobs = crate::blob_store::BlobStore::open(
            &runtime.storage,
            &runtime.family,
            crate::blob_store::BlobPlane::Callgraph,
        )
        .unwrap();
        crate::views::materialization::materialize_manifest_view_database(
            &cold,
            blobs.path(),
            &manifest,
        )
        .unwrap();
        crate::views::materialization::parity::assert_logical_parity(
            &self.view.derived_path(&generation).unwrap(),
            &cold,
        );
        self.ctx.publish_view_paths(BTreeSet::new(), true).unwrap();
        let rebuilt = self.ctx.view_runtime_snapshot().unwrap().manifest.unwrap();
        assert_eq!(
            manifest.to_json_bytes().unwrap(),
            rebuilt.to_json_bytes().unwrap(),
            "the incremental manifest must equal a full read of the final tree"
        );
    }
}

#[test]
fn publication_burst_is_single_flight_and_converges() {
    let fixture = Fixture::with_files(20);
    fixture.change("first");
    let gate = Gate::new(&fixture.job_root(), "derived");
    fixture.schedule();
    gate.started();
    for edit in 0..20 {
        let path = format!("file_{edit}.rs");
        std::fs::write(
            fixture.project.path().join(&path),
            format!("pub fn edit_{edit}() {{}}\n"),
        )
        .unwrap();
        fixture.schedule_paths(BTreeSet::from([path.into_bytes()]));
    }
    let in_flight = health_snapshot()
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["root"] == fixture.job_root().to_string_lossy().as_ref())
        .count();
    drop(gate);
    fixture.wait_idle_for(Duration::from_secs(60));
    assert_eq!(CAS_TIMINGS.lock()[&fixture.job_root()].len(), 2);
    assert_eq!(in_flight, 1, "edits must not start competing publications");
    assert_eq!(
        crate::views::assembly::publication_outcomes_for_test(&fixture.job_root()),
        vec!["published", "published", "published"]
    );
    fixture.assert_cold_parity();
}

#[test]
fn publication_failed_batch_is_included_in_follow_up() {
    let fixture = Fixture::with_files(1);
    fixture.change("failed_batch");
    let failures = Failures::new(fixture.job_root());
    let gate = Gate::new(&fixture.job_root(), "working_tree_read");
    fixture.schedule();
    gate.started();
    let tracked = fixture.project.path().join("tracked.rs");
    std::fs::remove_file(&tracked).unwrap();
    std::fs::create_dir(&tracked).unwrap();
    std::fs::write(
        fixture.project.path().join("file_0.rs"),
        "pub fn pending() {}\n",
    )
    .unwrap();
    fixture.schedule_paths(BTreeSet::from([b"file_0.rs".to_vec()]));
    let retry_gate = Gate::new(&fixture.job_root(), "retry_wait");
    drop(gate);
    failures.next();
    retry_gate.started();
    std::fs::remove_dir(&tracked).unwrap();
    std::fs::write(&tracked, "pub fn failed_batch() {}\n").unwrap();
    drop(retry_gate);
    fixture.wait_idle_for(Duration::from_secs(60));
    assert_eq!(CAS_TIMINGS.lock()[&fixture.job_root()].len(), 1);
    assert_eq!(
        crate::views::assembly::publication_outcomes_for_test(&fixture.job_root()),
        vec!["published", "cancelled_or_failed", "published"]
    );
    fixture.assert_cold_parity();
}

#[test]
fn publication_rescan_only_promotes_its_first_follow_up_to_full_read() {
    let fixture = Fixture::with_files(2);
    fixture.change("rescan");
    let gate = Gate::new(&fixture.job_root(), "derived");
    fixture.schedule_paths(BTreeSet::new());
    gate.started();
    std::fs::write(
        fixture.project.path().join("file_0.rs"),
        "pub fn first() {}\n",
    )
    .unwrap();
    fixture.schedule_paths(BTreeSet::from([b"file_0.rs".to_vec()]));
    let follow_up = Gate::new(&fixture.job_root(), "derived");
    drop(gate);
    follow_up.started();
    std::fs::write(
        fixture.project.path().join("file_1.rs"),
        "pub fn second() {}\n",
    )
    .unwrap();
    fixture.schedule_paths(BTreeSet::from([b"file_1.rs".to_vec()]));
    let pending = {
        let jobs = JOBS.lock();
        let job = jobs
            .values()
            .find(|job| job.root == fixture.job_root())
            .unwrap();
        assert!(
            job.paths.is_empty(),
            "the first follow-up must read everything"
        );
        job.pending.as_ref().unwrap().paths.clone()
    };
    drop(follow_up);
    fixture.wait_idle_for(Duration::from_secs(60));
    assert_eq!(pending, BTreeSet::from([b"file_1.rs".to_vec()]));
    assert_eq!(CAS_TIMINGS.lock()[&fixture.job_root()].len(), 3);
    fixture.assert_cold_parity();
}

#[test]
fn publication_unbind_abandons_in_flight_and_pending_work() {
    let fixture = Fixture::new();
    fixture
        .ctx
        .set_unbound_build_abandon_grace_for_test(Duration::ZERO);
    fixture.change("unbound");
    let gate = Gate::new(&fixture.job_root(), "derived");
    fixture.schedule();
    gate.started();
    fixture.schedule();
    fixture.ctx.mark_subc_unbound();
    drop(gate);
    fixture.wait_idle_for(Duration::from_secs(60));
    assert_eq!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(fixture.initial.as_str())
    );
    assert!(!CAS_TIMINGS.lock().contains_key(&fixture.job_root()));
}

#[test]
fn publication_follow_up_converges_to_final_tree() {
    let fixture = Fixture::with_files(2);
    fixture.change("first");
    let gate = Gate::new(&fixture.job_root(), "cas");
    fixture.schedule();
    gate.started();
    for file in 0..2 {
        let path = format!("file_{file}.rs");
        std::fs::write(
            fixture.project.path().join(&path),
            format!("pub fn final_{file}() {{}}\n"),
        )
        .unwrap();
        fixture.schedule_paths(BTreeSet::from([path.into_bytes()]));
    }
    drop(gate);
    fixture.wait_idle_for(Duration::from_secs(60));
    fixture.assert_cold_parity();
}

#[test]
fn publication_conflict_follow_up_uses_winning_base() {
    let fixture = Fixture::with_files(1);
    fixture.change("older");
    let gate = Gate::new(&fixture.job_root(), "cas");
    fixture.schedule();
    gate.started();
    std::fs::write(
        fixture.project.path().join("tracked.rs"),
        "pub fn winner() {}\n",
    )
    .unwrap();
    let winner = fixture
        .ctx
        .publish_view_paths(BTreeSet::new(), true)
        .unwrap()
        .generation
        .unwrap();
    std::fs::write(
        fixture.project.path().join("file_0.rs"),
        "pub fn pending() {}\n",
    )
    .unwrap();
    fixture.schedule_paths(BTreeSet::from([b"file_0.rs".to_vec()]));
    drop(gate);
    fixture.wait_idle_for(Duration::from_secs(60));
    assert_ne!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(winner.as_str())
    );
    assert_eq!(
        CAS_TIMINGS.lock()[&fixture.job_root()].len(),
        1,
        "the stale attempt must not commit"
    );
    fixture.assert_cold_parity();
}

#[test]
fn publication_configure_invalidates_old_work_without_losing_pending_paths() {
    let fixture = Fixture::with_files(2);
    fixture.change("older");
    let gate = Gate::new(&fixture.job_root(), "cas");
    fixture.schedule();
    gate.started();
    std::fs::write(
        fixture.project.path().join("file_0.rs"),
        "pub fn pending_before() {}\n",
    )
    .unwrap();
    fixture.schedule_paths(BTreeSet::from([b"file_0.rs".to_vec()]));
    cancel_for_context(&fixture.ctx);
    fixture
        .ctx
        .configure_content_generation_flag()
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    std::fs::write(
        fixture.project.path().join("file_1.rs"),
        "pub fn pending_after() {}\n",
    )
    .unwrap();
    fixture.schedule_paths(BTreeSet::from([b"file_1.rs".to_vec()]));
    drop(gate);
    fixture.wait_idle_for(Duration::from_secs(60));
    assert_eq!(
        CAS_TIMINGS.lock()[&fixture.job_root()].len(),
        1,
        "the old configuration must not commit"
    );
    fixture.assert_cold_parity();
}

/// Run alone because the assembly delay environment applies to every root in
/// the process. The release file holds all attempts while the edit burst lands.
#[test]
#[ignore = "5k-file before/after measurement; run with --ignored --test-threads=1"]
fn publication_burst_5k_measurement() {
    let fixture = Fixture::with_files(5000);
    fixture.change("first");
    let release = fixture._storage.path().join("publication-release");
    std::env::set_var("AFT_TEST_VIEW_PUBLICATION_GATE_FILE", &release);
    std::env::set_var("AFT_TEST_VIEW_PUBLICATION_DELAY_MS", "25");
    let gate = Gate::new(&fixture.job_root(), "manifest");
    let started = Instant::now();
    fixture.schedule();
    gate.started();
    drop(gate);
    for edit in 0..20 {
        let path = format!("file_{edit}.rs");
        std::fs::write(
            fixture.project.path().join(&path),
            format!("pub fn final_{edit}() {{}}\n"),
        )
        .unwrap();
        fixture.schedule_paths(BTreeSet::from([path.into_bytes()]));
    }
    std::fs::write(&release, "release").unwrap();
    fixture.wait_idle_for(Duration::from_secs(300));
    std::env::remove_var("AFT_TEST_VIEW_PUBLICATION_GATE_FILE");
    std::env::remove_var("AFT_TEST_VIEW_PUBLICATION_DELAY_MS");
    let outcomes = crate::views::assembly::publication_outcomes_for_test(&fixture.job_root());
    eprintln!("BURST_5K edits=20 files=5000 attempts={} committed={} published={} cancelled_or_failed={} total_ms={} outcomes={:?}",
        outcomes.len() - 1,
        CAS_TIMINGS.lock().get(&fixture.job_root()).map_or(0, Vec::len),
        outcomes[1..].iter().filter(|outcome| **outcome == "published").count(),
        outcomes[1..].iter().filter(|outcome| **outcome == "cancelled_or_failed").count(),
        started.elapsed().as_millis(), &outcomes[1..]);
    fixture.assert_cold_parity();
}
fn git(root: &Path, args: &[&str]) {
    let mut command = std::process::Command::new("git");
    crate::test_env::apply_hermetic_git_env(command.current_dir(root));
    assert!(command.args(args).output().unwrap().status.success());
}
fn commit(root: &Path) {
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=AFT",
            "-c",
            "user.email=aft@example.com",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
}

#[test]
fn inspect_view_pending_publication_without_deferred_job_requests_no_refresh() {
    let fixture = Fixture::new();
    fixture.ctx.set_harness(crate::harness::Harness::Opencode);
    fixture.ctx.reset_tier2_refresh_scheduler_at(Instant::now());
    fixture.change("no_inspection_requested");
    let before = fixture
        .ctx
        .inspect_manager()
        .automatic_tier2_schedule_count_for_test();
    fixture.schedule();
    fixture.wait_idle();
    assert!(
        !fixture.ctx.tier2_pull_demand_pending(),
        "publication without deferred analysis must not request a Tier-2 pull"
    );
    assert_eq!(
        fixture
            .ctx
            .inspect_manager()
            .automatic_tier2_schedule_count_for_test(),
        before
    );
}

#[test]
fn inspect_view_pending_publication_reschedules_dead_code_without_another_inspect() {
    use crate::inspect::{InspectCategory, InspectSnapshot, JobScope};
    let fixture = Fixture::new();
    fixture.ctx.set_harness(crate::harness::Harness::Opencode);
    let root = fixture.ctx.canonical_cache_root();
    let family = crate::search_index::artifact_cache_key(&root);
    crate::root_cache::configure_artifact_access(&root, &family, false);
    fixture.ctx.reset_tier2_refresh_scheduler_at(Instant::now());
    fixture.change("after_publication");
    let manager = fixture.ctx.inspect_manager();
    let snapshot = InspectSnapshot {
        project_root: root.clone(),
        inspect_dir: fixture.ctx.inspect_dir(),
        config: fixture.ctx.config(),
        symbol_cache: fixture.ctx.symbol_cache(),
        inspect_writer: true,
        callgraph_writer: true,
    };
    let pending = manager.tier2_run_with_reuse_blocking_fresh(
        snapshot.clone(),
        InspectCategory::DeadCode,
        JobScope::for_project(root.clone()),
    );
    assert_eq!(
        pending.payload().map(|payload| &payload["code"]),
        Some(&serde_json::json!("view_pending")),
        "{pending:?}"
    );
    assert_ne!(
        pending
            .payload()
            .and_then(|payload| payload["callgraph_available"].as_bool()),
        Some(true)
    );
    assert!(!fixture.ctx.tier2_pull_demand_pending());

    fixture.schedule();
    fixture.wait_idle();
    assert!(
        fixture.ctx.tier2_pull_demand_pending(),
        "view publication must wake deferred Tier-2 work"
    );
    // A publication callback can run before the job records its pending result.
    // Recreate that scheduler state and let only the idle probe discover demand.
    fixture.ctx.reset_tier2_refresh_scheduler_at(Instant::now());
    assert!(!fixture.ctx.tier2_pull_demand_pending());
    manager.set_tier2_in_flight_for_test(InspectCategory::DeadCode, true);
    assert!(!fixture.ctx.tier2_refresh_dispatch_due());
    assert_eq!(
        fixture
            .ctx
            .tick_tier2_refresh_scheduler_at(Instant::now(), 0),
        None
    );
    assert!(manager.deferred_view_refresh_ready(&fixture.ctx.canonical_cache_root()));
    manager.set_tier2_in_flight_for_test(InspectCategory::DeadCode, false);
    assert!(
        fixture.ctx.tier2_refresh_dispatch_due(),
        "a quiet checkout must discover its owed published view without a watcher edit"
    );
    assert_eq!(
        fixture
            .ctx
            .tick_tier2_refresh_scheduler_at(Instant::now(), 0),
        Some(crate::inspect::Tier2TriggerReason::Pull)
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while manager.tier2_any_in_flight() {
        assert!(
            Instant::now() < deadline,
            "automatic Tier-2 refresh did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    manager.drain_completions();
    let outcome = manager.tier2_read_cached_readonly(
        snapshot,
        InspectCategory::DeadCode,
        JobScope::for_project(root),
    );
    assert_eq!(
        outcome
            .payload()
            .map(|payload| &payload["callgraph_available"]),
        Some(&serde_json::json!(true)),
        "{outcome:?}"
    );
    assert!(!manager.deferred_view_refresh_ready(&fixture.ctx.canonical_cache_root()));
    fixture.change("no_more_deferred_analysis");
    fixture.schedule();
    fixture.wait_idle();
    assert!(
        !fixture.ctx.tier2_pull_demand_pending(),
        "a later publication must not repeat the consumed refresh"
    );
}

#[test]
fn inspect_view_pending_reply_names_publication_not_changed_sources() {
    use crate::inspect::{InspectCategory, InspectSnapshot, JobScope};
    let fixture = Fixture::new();
    fixture.ctx.set_harness(crate::harness::Harness::Opencode);
    let root = fixture.ctx.canonical_cache_root();
    let family = crate::search_index::artifact_cache_key(&root);
    crate::root_cache::configure_artifact_access(&root, &family, false);
    let snapshot = InspectSnapshot {
        project_root: root.clone(),
        inspect_dir: fixture.ctx.inspect_dir(),
        config: fixture.ctx.config(),
        symbol_cache: fixture.ctx.symbol_cache(),
        inspect_writer: true,
        callgraph_writer: true,
    };
    let initial = fixture
        .ctx
        .inspect_manager()
        .tier2_run_with_reuse_blocking_fresh(
            snapshot,
            InspectCategory::DeadCode,
            JobScope::for_project(root),
        );
    assert_eq!(initial.payload().unwrap()["callgraph_available"], true);
    fixture.change("waiting_for_view");
    let response = crate::commands::inspect::handle_inspect_warm_for_test(
        &crate::protocol::RawRequest {
            id: "pending-view".into(),
            command: "inspect".into(),
            params: serde_json::json!({"scope": "tracked.rs", "sections": ["dead_code"]}),
            session_id: None,
            lsp_hints: None,
        },
        &fixture.ctx,
    );
    assert!(response.success, "{:?}", response.data);
    let text = response.data["text"].as_str().unwrap();
    assert!(
        text.contains("dead code waits for this checkout's index view to publish"),
        "{text}"
    );
    assert!(
        text.contains("publication schedules a Tier-2 refresh"),
        "{text}"
    );
    assert!(!text.contains("sources or configuration changed"), "{text}");
    assert!(
        !text.contains("cached source contributions, file set, or analysis configuration changed"),
        "{text}"
    );
    assert_eq!(
        response.data["summary"]["dead_code"]["gaps"][0]["kind"],
        "view_pending"
    );
}

#[test]
fn publication_busy_readiness_uses_backoff_and_recovers_after_unlock() {
    let fixture = Fixture::new();
    let derived = fixture.view.derived_path(&fixture.initial).unwrap();
    crate::views::wait_for_derived_checkpoint_for_test(fixture.view.view_dir());
    let writer = crate::db::file_identity::IdentityConnection::open(
        &derived,
        "publication_busy_readiness_test",
    )
    .unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();
    fixture.change("after_unlock");
    let failures = Failures::new(fixture.job_root());
    let completions = Completions::new(fixture.job_root());
    fixture.schedule();
    let (_, delay, count, _) = failures.next();
    assert_eq!(delay, Duration::from_secs(1));
    assert_eq!(count, 1);
    assert_eq!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(fixture.initial.as_str())
    );
    assert_eq!(
        crate::views::assembly::cold_build_counts(fixture.view.view_dir()),
        Default::default()
    );
    assert!(fixture
        .ctx
        .view_publication_retry()
        .lock()
        .error
        .as_deref()
        .unwrap()
        .contains("database is locked"));
    assert!(!fixture
        .ctx
        .view_publication_retry()
        .lock()
        .error
        .as_deref()
        .unwrap()
        .contains("invalid view manifest"));
    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);

    completions.next();
    fixture.wait_idle();
    assert_ne!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(fixture.initial.as_str())
    );
    assert_eq!(
        crate::views::assembly::cold_build_counts(fixture.view.view_dir()),
        Default::default()
    );
    assert!(fixture.ctx.view_publication_retry().lock().due().is_none());
    match fixture.ctx.callgraph_store_for_ops() {
        CallgraphStoreAccess::Ready(store) => {
            assert!(crate::callgraph_store::CallGraphRead::node_for(
                &store,
                Path::new("tracked.rs"),
                "after_unlock"
            )
            .is_ok())
        }
        _ => panic!("publication did not recover after the writer released its lock"),
    }
}

#[test]
fn publication_omits_staged_and_unstaged_working_tree_deletions() {
    use crate::views::{
        contracts::{PlaneAdapter, PlaneLoader, ViewAccess},
        first_load::{
            CallgraphBridge, CheckoutDriver, ConfiguredMembershipWalker, SiblingLoader,
            TrigramBridge,
        },
        manifest_v2::Producers,
        registry::FamilyRegistry,
    };
    let mut failures = Vec::new();
    for staged in [false, true] {
        let fixture = Fixture::new();
        std::fs::write(fixture.project.path().join("kept.rs"), "pub fn kept() {}\n").unwrap();
        commit(fixture.project.path());
        let root = fixture.job_root();
        let registry =
            FamilyRegistry::open(fixture._storage.path(), "publication-fixture").unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "query", &root)
            .unwrap();
        let policy = crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1 << 20,
        };
        let trigram = Arc::new(TrigramBridge::new(
            fixture._storage.path().to_owned(),
            policy,
        ));
        let callgraph = Arc::new(CallgraphBridge::default());
        let adapters: Vec<Arc<dyn PlaneAdapter>> =
            vec![trigram.adapter.clone(), callgraph.adapter.clone()];
        let driver = Arc::new(
            CheckoutDriver::new(
                owner.clone(),
                Producers {
                    trigram: policy.fingerprint_hex(),
                    semantic: None,
                    callgraph: crate::views::callgraph::PRODUCER.into(),
                },
                None,
                Arc::new(ConfiguredMembershipWalker),
                vec![trigram.clone(), callgraph.clone()],
            )
            .with_adapters(adapters.clone()),
        );
        let loader = SiblingLoader::new(driver.clone(), adapters);
        let access = ViewAccess::Owner(owner);
        let before = loader.load(&access).unwrap();
        let index = trigram
            .adapter
            .resident("query", before.snapshot.generation().name())
            .unwrap();
        assert_eq!(
            index
                .query(&root, &before.snapshot, "previous")
                .matches
                .len(),
            1
        );
        assert_eq!(
            callgraph
                .adapter
                .reader(&access, &before.snapshot)
                .unwrap()
                .store
                .indexed_file_count()
                .unwrap(),
            2
        );
        std::fs::remove_file(fixture.project.path().join("tracked.rs")).unwrap();
        if staged {
            git(fixture.project.path(), &["add", "-u"]);
        }
        let result = fixture.ctx.publish_view_paths(BTreeSet::new(), true);
        let report = match result {
            Ok(report) => report,
            Err(error) => {
                failures.push(format!("staged={staged}: {error}"));
                continue;
            }
        };
        assert!(report.published);
        assert!(report
            .manifest
            .unwrap()
            .get(&crate::views::RelPath::new(b"tracked.rs".to_vec()).unwrap())
            .is_none());
        let CallgraphStoreAccess::Ready(store) = fixture.ctx.callgraph_store_for_ops() else {
            panic!("deleted-file view reader unavailable")
        };
        assert_eq!(
            crate::callgraph_store::CallGraphRead::indexed_file_count(&store).unwrap(),
            1
        );
        driver.record_absolute_change(&root.join("tracked.rs"));
        let after = loader.load(&access).unwrap();
        assert!(after.pending_planes.is_empty());
        let index = trigram
            .adapter
            .resident("query", after.snapshot.generation().name())
            .unwrap();
        let deleted = index.query(&root, &after.snapshot, "previous");
        assert!(deleted.matches.is_empty());
        assert!(deleted.gaps.is_empty());
        assert_eq!(index.query(&root, &after.snapshot, "kept").matches.len(), 1);
        let reader = callgraph.adapter.reader(&access, &after.snapshot).unwrap();
        assert_eq!(reader.store.indexed_file_count().unwrap(), 1);
        assert!(reader
            .store
            .node_for(Path::new("tracked.rs"), "previous")
            .is_err());
        assert!(reader.store.node_for(Path::new("kept.rs"), "kept").is_ok());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// An upgrade that bumps the call-graph producer finds the checkout's view
/// already published under the previous producer. The owner must rebuild and
/// serve a new generation rather than keep serving, or refusing, the old one.
#[test]
fn callgraph_producer_change_republishes_an_existing_generation() {
    use crate::blob_store::v2::FamilyPlane;
    use crate::views::{
        callgraph::{BlobReader, PRODUCER},
        contracts::{PlaneAdapter, PlaneLoader, ViewAccess},
        first_load::{
            CallgraphBridge, CheckoutDriver, ConfiguredMembershipWalker, SiblingLoader,
            TrigramBridge,
        },
        manifest_v2::{GenerationName, Producers},
        registry::FamilyRegistry,
    };
    // The producer the previous release stamped on its generations.
    const PREVIOUS_PRODUCER: &str = "ruled-callgraph-v9-without-ast-index";
    assert_ne!(PREVIOUS_PRODUCER, PRODUCER);
    let fixture = Fixture::new();
    let root = fixture.job_root();
    let registry = FamilyRegistry::open(fixture._storage.path(), "publication-fixture").unwrap();
    let owner = registry
        .register_view_with_storage(fixture._storage.path(), "query", &root)
        .unwrap();
    let access = ViewAccess::Owner(owner.clone());
    let policy = crate::blob_store::v2::TrigramPolicy {
        max_file_size: 1 << 20,
    };
    let load = || {
        let trigram = Arc::new(TrigramBridge::new(
            fixture._storage.path().to_owned(),
            policy,
        ));
        let callgraph = Arc::new(CallgraphBridge::default());
        let adapters: Vec<Arc<dyn PlaneAdapter>> =
            vec![trigram.adapter.clone(), callgraph.adapter.clone()];
        let driver = Arc::new(
            CheckoutDriver::new(
                owner.clone(),
                Producers {
                    trigram: policy.fingerprint_hex(),
                    semantic: None,
                    callgraph: PRODUCER.into(),
                },
                None,
                Arc::new(ConfiguredMembershipWalker),
                vec![trigram.clone(), callgraph.clone()],
            )
            .with_adapters(adapters.clone()),
        );
        let loaded = SiblingLoader::new(driver, adapters).load(&access).unwrap();
        (callgraph, loaded)
    };

    // Build the checkout's generation, then publish the same files stamped
    // with the previous producer, as the previous release left it on disk.
    let (_, built) = load();
    let views = owner.view_store().unwrap();
    let built_name = built.snapshot.generation().name().to_string();
    let mut previous = built.snapshot.generation().manifest().clone();
    previous.header_mut().producers.callgraph = PREVIOUS_PRODUCER.into();
    let previous_name = GenerationName::for_manifest(&previous).unwrap();
    let store = owner.open_store(FamilyPlane::Callgraph).unwrap();
    // The current build refuses to materialize under another producer, so the
    // previous generation reuses the database built for the same files.
    crate::views::clone_derived_for_test(
        &views.derived_path(&built_name).unwrap(),
        &views.derived_path(&previous_name.to_string()).unwrap(),
    )
    .unwrap();
    let prepared = views
        .prepare_v2(&previous_name, Some(&built_name), &previous, None)
        .unwrap();
    views.commit_v2(prepared, None).unwrap();
    let previous_name = previous_name.to_string();
    assert_eq!(
        views.current_generation().unwrap().as_deref(),
        Some(previous_name.as_str())
    );

    let (callgraph, current) = load();
    assert!(
        current.pending_planes.is_empty(),
        "{:?}",
        current.pending_planes
    );
    let generation = current.snapshot.generation();
    assert_ne!(generation.name(), previous_name);
    assert_eq!(generation.manifest().header().producers.callgraph, PRODUCER);
    assert_eq!(
        views.current_generation().unwrap().as_deref(),
        Some(generation.name())
    );
    let reader = callgraph
        .adapter
        .reader(&access, &current.snapshot)
        .unwrap();
    assert!(reader
        .store
        .node_for(Path::new("tracked.rs"), "previous")
        .is_ok());

    // The served database equals a cold build of the new manifest.
    let cold_dir = tempfile::tempdir().unwrap();
    let cold = cold_dir.path().join("cold.sqlite");
    crate::views::callgraph::materialize(&cold, generation.manifest(), &BlobReader(store.reader()))
        .unwrap();
    crate::views::materialization::parity::assert_logical_parity(
        &views.derived_path(generation.name()).unwrap(),
        &cold,
    );
}

#[test]
fn publication_omits_file_deleted_after_head_listing() {
    let fixture = Fixture::new();
    let mut deleted = false;
    let mut prepared = fixture
        .ctx
        .prepare_view_paths(BTreeSet::new(), true, &mut |phase| {
            if phase == "working_tree_read" && !deleted {
                std::fs::remove_file(fixture.project.path().join("tracked.rs")).unwrap();
                deleted = true;
            }
            Ok(())
        })
        .unwrap();
    assert!(
        deleted,
        "the deletion must happen after listing, before reading"
    );
    let report = fixture.ctx.commit_view_update(&mut prepared).unwrap();
    assert!(report.published);
    assert!(report.manifest.unwrap().entries().next().is_none());
}

#[test]
fn publication_keeps_reused_semantic_rows_pinned_during_handoff() {
    let fixture = Fixture::new();
    let root = std::fs::canonicalize(fixture.project.path()).unwrap();
    let source = b"pub fn next_semantic() {}\n";
    let path = root.join("tracked.rs");
    std::fs::write(&path, source).unwrap();
    let mut index = crate::semantic_index::SemanticIndex::build(
        &root,
        &[path],
        &mut |texts: Vec<String>| Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect()),
        64,
    )
    .unwrap();
    let fingerprint = crate::semantic_index::SemanticIndexFingerprint {
        dimension: 2,
        chunking_version: 2,
        ..Default::default()
    };
    index.set_fingerprint(fingerprint.clone());
    let snapshot = fixture.ctx.view_runtime_snapshot().unwrap();
    let mut request = crate::migration::SemanticMigrationRequest::for_root(
        &snapshot.storage,
        &root,
        fingerprint.as_string(),
    );
    request.family.clone_from(&snapshot.family);
    request.view.clone_from(&snapshot.scope);
    let key = crate::blob_store::SemanticKey::from_bytes(
        source,
        b"tracked.rs",
        &request.chunker_version,
        &request.embed_template_version,
        &request.configured_model_fingerprint,
    )
    .full_key();
    drop(crate::migration::store_live_semantic_blobs(&request, &index).unwrap());
    *fixture.ctx.semantic_index().write().unwrap() = Some(index);
    let mut config = (*fixture.ctx.config()).clone();
    config.indexes.semantic = true;
    fixture.ctx.set_config(config);
    let collect = || {
        crate::gc::sweep_semantic_handoff_for_test(crate::gc::SweepRequest {
            durable_storage: &snapshot.storage,
            storage: &snapshot.storage,
            family: &snapshot.family,
            view_dir: &snapshot.view_dir,
            byte_budget: 0,
            now_ms: crate::pins::now_ms().saturating_add(crate::gc::BLOB_AGE_FLOOR_MS + 1),
            references: crate::gc::SweepReferences::default(),
        })
        .unwrap()
    };
    let assert_semantic_present = || {
        let store = crate::blob_store::BlobStore::open(
            &snapshot.storage,
            &snapshot.family,
            crate::blob_store::BlobPlane::Semantic,
        )
        .unwrap();
        assert!(
            store.get(&key).unwrap().is_some(),
            "publication dropped the semantic pin during handoff"
        );
    };
    let mut swept_handoff = false;
    let mut prepared = fixture
        .ctx
        .prepare_view_paths(
            BTreeSet::from([b"tracked.rs".to_vec()]),
            true,
            &mut |phase| {
                if phase == "working_tree_read" && !swept_handoff {
                    swept_handoff = true;
                    collect();
                    assert_semantic_present();
                }
                Ok(())
            },
        )
        .unwrap();
    assert!(swept_handoff, "handoff sweep callback did not run");
    collect();
    {
        let store = crate::blob_store::BlobStore::open(
            &snapshot.storage,
            &snapshot.family,
            crate::blob_store::BlobPlane::Semantic,
        )
        .unwrap();
        assert!(
            store.get(&key).unwrap().is_some(),
            "prepared publication dropped the semantic pin before commit"
        );
    }
    let report = fixture.ctx.commit_view_update(&mut prepared).unwrap();
    assert!(report.generation.is_some());
}

#[test]
fn publication_retires_root_deleted_during_source_read() {
    let fixture = Fixture::new();
    let result = fixture
        .ctx
        .prepare_view_paths(BTreeSet::new(), true, &mut |phase| {
            if phase == "working_tree_read" {
                std::fs::remove_dir_all(fixture.project.path()).unwrap();
            }
            Ok(())
        });
    assert!(matches!(result, Err(error) if error == "view publication root was deleted"));
    assert!(fixture.ctx.view_runtime_snapshot().is_none());
    assert_eq!(
        fixture.view.current_generation().unwrap().unwrap(),
        fixture.initial
    );
}

#[test]
fn publication_does_not_reuse_a_deleted_unchanged_member() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.project.path().join("tracked.rs")).unwrap();
    let report = fixture
        .ctx
        .publish_view_paths(BTreeSet::from([b"other.rs".to_vec()]), true)
        .unwrap();
    assert!(report.published);
    assert!(report.manifest.unwrap().entries().next().is_none());
}

#[test]
fn publication_rescan_then_event_still_reads_the_whole_checkout() {
    use crate::watcher_filter::{RescanReason, WatcherDispatchEvent};
    let fixture = Fixture::new();
    std::fs::write(fixture.job_root().join("later.rs"), "pub fn before() {}\n").unwrap();
    commit(&fixture.job_root());
    let generation = fixture
        .ctx
        .publish_view_paths(BTreeSet::new(), true)
        .unwrap()
        .generation
        .unwrap();
    let before = fixture.view.load_manifest(&generation).unwrap();
    std::fs::write(
        fixture.job_root().join("tracked.rs"),
        "pub fn missed_by_watcher() {}\n",
    )
    .unwrap();
    std::fs::write(
        fixture.job_root().join("later.rs"),
        "pub fn reported() {}\n",
    )
    .unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    *fixture.ctx.watcher_rx().lock() = Some(rx);
    tx.send(WatcherDispatchEvent::RescanRequired(
        RescanReason::KernelDropped,
    ))
    .unwrap();
    // Complete the rescan before the later event arrives in the quiet window.
    crate::runtime_drain::drain_watcher_to_completion_for_test(&fixture.ctx, usize::MAX);
    assert!(fixture
        .ctx
        .watcher_drain_slice()
        .lock()
        .as_ref()
        .unwrap()
        .view_publication_due
        .is_some());
    tx.send(WatcherDispatchEvent::Paths(vec![fixture
        .job_root()
        .join("later.rs")]))
        .unwrap();
    crate::runtime_drain::drain_watcher_to_completion_for_test(&fixture.ctx, usize::MAX);
    let mut state = fixture.ctx.watcher_drain_slice().lock().take().unwrap();
    state.view_publication_due = Some(Instant::now());
    crate::runtime_drain::publish_view_if_quiet(&fixture.ctx, &mut state);
    let generation = fixture.view.current_generation().unwrap().unwrap();
    let after = fixture.view.load_manifest(&generation).unwrap();
    let missed = crate::views::RelPath::new(b"tracked.rs".to_vec()).unwrap();
    assert_ne!(
        before.get(&missed),
        after.get(&missed),
        "the scheduled publication must read the edit omitted by the dropped events"
    );
    assert!(state.view_publication_due.is_none());
}

#[test]
fn publication_unrepresentable_watcher_path_preserves_full_read() {
    for root_event in [false, true] {
        let fixture = Fixture::new();
        std::fs::write(fixture.job_root().join("other.rs"), "pub fn before() {}\n").unwrap();
        commit(&fixture.job_root());
        fixture
            .ctx
            .publish_view_paths(BTreeSet::new(), true)
            .unwrap();
        let generation = fixture.view.current_generation().unwrap().unwrap();
        let before = fixture.view.load_manifest(&generation).unwrap();
        std::fs::write(
            fixture.job_root().join("tracked.rs"),
            "pub fn missed_spelling() {}\n",
        )
        .unwrap();
        std::fs::write(
            fixture.job_root().join("other.rs"),
            "pub fn reported() {}\n",
        )
        .unwrap();
        let mut state = crate::context::WatcherDrainSliceState::new(
            fixture.ctx.configure_generation(),
            fixture.ctx.configure_content_generation(),
        );
        state
            .view_publication_paths
            .insert(fixture.job_root().join("other.rs"));
        state.view_publication_paths.insert(if root_event {
            fixture.job_root()
        } else {
            fixture
                .job_root()
                .with_file_name("different-root-spelling")
                .join("tracked.rs")
        });
        state.view_publication_due = Some(Instant::now());
        crate::runtime_drain::publish_view_if_quiet(&fixture.ctx, &mut state);
        let generation = fixture.view.current_generation().unwrap().unwrap();
        let after = fixture.view.load_manifest(&generation).unwrap();
        let path = crate::views::RelPath::new(b"tracked.rs".to_vec()).unwrap();
        assert_ne!(
            before.get(&path),
            after.get(&path),
            "unrepresentable event was silently discarded: root_event={root_event}"
        );
    }
}

#[test]
fn publication_idle_full_verification_runs_without_a_watcher_event() {
    for with_receiver in [false, true] {
        let fixture = Fixture::new();
        let _sender = if with_receiver {
            let (tx, rx) = crossbeam_channel::unbounded();
            *fixture.ctx.watcher_rx().lock() = Some(rx);
            Some(tx)
        } else {
            None
        };
        let before = fixture.view.load_manifest(&fixture.initial).unwrap();
        std::fs::write(
            fixture.job_root().join("tracked.rs"),
            "pub fn missed_during_silence() {}\n",
        )
        .unwrap();
        crate::views::assembly::expire_verification_for_test(fixture.view.view_dir());
        assert!(
            fixture.ctx.watcher_drain_has_work(),
            "the maintenance pump must wake an idle root at its verification deadline"
        );
        crate::runtime_drain::drain_watcher_to_completion_for_test(&fixture.ctx, usize::MAX);
        let generation = fixture.view.current_generation().unwrap().unwrap();
        let after = fixture.view.load_manifest(&generation).unwrap();
        assert_ne!(
        before, after,
        "no new watcher event is needed to discover the missed edit: with_receiver={with_receiver}"
    );
        assert!(!fixture.ctx.view_full_verification_due());
        assert!(fixture
            .ctx
            .watcher_drain_slice()
            .lock()
            .as_ref()
            .unwrap()
            .view_publication_due
            .is_none());
    }
}

#[test]
fn publication_cache_is_released_on_quiesce_eviction_and_clear() {
    for action in ["quiesce", "evict", "try_evict", "clear", "replace"] {
        let fixture = Fixture::new();
        let view_dir = fixture.view.view_dir();
        assert!(crate::views::assembly::cache_contains_for_test(view_dir));
        match action {
            "quiesce" => fixture.ctx.mark_subc_unbound(),
            "evict" => assert!(fixture.ctx.evict_idle_artifacts()),
            "try_evict" => assert!(fixture.ctx.try_evict_idle_artifacts()),
            "clear" => fixture.ctx.clear_view_runtime(),
            "replace" => {
                let mut snapshot = fixture.ctx.view_runtime_snapshot().unwrap();
                let replacement =
                    crate::views::ViewStore::open(fixture._storage.path(), "replacement").unwrap();
                snapshot.scope = "replacement".into();
                snapshot.view_dir = replacement.view_dir().to_path_buf();
                snapshot.generation = None;
                snapshot.manifest = None;
                snapshot.query_pin = None;
                fixture.ctx.install_view_runtime(snapshot, None);
            }
            _ => unreachable!(),
        }
        assert!(
            !crate::views::assembly::cache_contains_for_test(view_dir),
            "cache survived {action}"
        );
    }
}

#[test]
fn publication_io_error_names_operation_and_path() {
    let fixture = Fixture::new();
    let path = fixture.project.path().join("tracked.rs");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let error = fixture
        .ctx
        .publish_view_paths(BTreeSet::new(), true)
        .unwrap_err();
    let error = error.to_string();
    assert!(error.starts_with("view I/O failed reading "), "{error}");
    assert!(error.contains("tracked.rs"), "{error}");
    assert_ne!(fixture.view.current_generation().unwrap(), None);
}

#[test]
fn publication_failure_streak_survives_changing_artifact_error_paths() {
    let root = tempfile::tempdir().unwrap();
    let mut retry = PublicationRetry::default();
    retry.failed(
        root.path(),
        "view I/O failed writing trigram-1.bin: Permission denied",
    );
    retry.failed(
        root.path(),
        "view I/O failed writing trigram-2.bin: Permission denied",
    );
    assert_eq!(retry.failures, 2);
    assert_eq!(retry.delay, Duration::from_secs(2));
}

#[test]
fn publication_repeated_failure_backs_off_and_edit_resets_deadline() {
    let fixture = Fixture::new();
    let root = fixture.job_root();
    {
        let mut retry = fixture.ctx.view_publication_retry().lock();
        retry.initial = Duration::from_millis(80);
        retry.maximum = Duration::from_millis(320);
    }
    let failures = Failures::new(root.clone());
    let completions = Completions::new(root.clone());
    let path = root.join("tracked.rs");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    fixture.schedule();
    let mut observed = Vec::new();
    for _ in 0..4 {
        observed.push(failures.next());
    }
    // Cancel before asserting, so even a broken retry policy leaves no live
    // worker behind when an assertion fails.
    cancel_for_context(&fixture.ctx);
    completions.next();
    fixture.wait_idle();
    assert_eq!(
        observed
            .iter()
            .map(|row| row.1.as_millis())
            .collect::<Vec<_>>(),
        vec![80, 160, 320, 320]
    );
    assert_eq!(
        observed.iter().map(|row| row.2).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        observed.iter().map(|row| row.3).collect::<Vec<_>>(),
        vec![true, true, false, false]
    );
    for pair in observed.windows(2) {
        assert!(
            pair[1].0.duration_since(pair[0].0) >= pair[0].1,
            "retry ran before its deadline: {pair:?}"
        );
    }
    // A fresh scheduling request without a source edit must retain the streak.
    fixture.schedule();
    let repeated = failures.next();
    cancel_for_context(&fixture.ctx);
    completions.next();
    fixture.wait_idle();
    assert_eq!(repeated.2, 5);
    let edited = root.join("untracked.rs");
    std::fs::write(&edited, "pub fn edit() {}\n").unwrap();
    fixture.ctx.record_checkout_watcher_change(&edited);
    assert!(fixture.ctx.view_publication_retry().lock().due().is_none());
    fixture.schedule();
    let reset = failures.next();
    cancel_for_context(&fixture.ctx);
    completions.next();
    fixture.wait_idle();
    assert_eq!(
        (reset.1, reset.2, reset.3),
        (Duration::from_millis(80), 1, true)
    );
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, "pub fn recovered() {}\n").unwrap();
    fixture.ctx.record_checkout_watcher_change(&path);
    fixture.schedule();
    completions.next();
    fixture.wait_idle();
    assert!(fixture.ctx.view_publication_retry().lock().due().is_none());
    assert_ne!(
        fixture.view.current_generation().unwrap().unwrap(),
        fixture.initial
    );
}

#[test]
fn publication_inline_failure_obeys_the_shared_retry_deadline() {
    let fixture = Fixture::new();
    let root = fixture.job_root();
    let path = root.join("tracked.rs");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let before = fixture.ctx.view_publication_attempts_for_test();
    let mut state = crate::context::WatcherDrainSliceState::new(
        fixture.ctx.configure_generation(),
        fixture.ctx.configure_content_generation(),
    );
    state.view_publication_paths.insert(path.clone());
    state.view_publication_due = Some(Instant::now());
    crate::runtime_drain::publish_view_if_quiet(&fixture.ctx, &mut state);
    let due = fixture.ctx.view_publication_retry().lock().due().unwrap();
    assert_eq!(fixture.ctx.view_publication_attempts_for_test(), before + 1);
    assert!(state.view_publication_paths.contains(&path));
    state.view_publication_due = Some(Instant::now());
    crate::runtime_drain::publish_view_if_quiet(&fixture.ctx, &mut state);
    assert_eq!(fixture.ctx.view_publication_attempts_for_test(), before + 1);
    assert_eq!(state.view_publication_due, Some(due));
    fixture.ctx.record_checkout_watcher_change(&path);
    state.view_publication_due = Some(Instant::now());
    crate::runtime_drain::publish_view_if_quiet(&fixture.ctx, &mut state);
    assert_eq!(fixture.ctx.view_publication_attempts_for_test(), before + 2);
}

#[cfg(unix)]
#[test]
fn publication_omits_deleted_symlinks() {
    for staged in [false, true] {
        let fixture = Fixture::new();
        let link = fixture.project.path().join("link.rs");
        std::os::unix::fs::symlink("tracked.rs", &link).unwrap();
        commit(fixture.project.path());
        fixture
            .ctx
            .publish_view_paths(BTreeSet::new(), true)
            .unwrap();
        std::fs::remove_file(&link).unwrap();
        if staged {
            git(fixture.project.path(), &["add", "-u"]);
        }
        let report = fixture
            .ctx
            .publish_view_paths(BTreeSet::new(), true)
            .unwrap();
        assert!(report.published);
        assert_eq!(report.manifest.unwrap().entries().count(), 1);
    }
}

#[test]
fn publication_build_does_not_delay_same_root_bind_and_read() {
    let fixture = Fixture::new();
    fixture.change("next");
    let gate = Gate::new(fixture.job_root().as_path(), "derived");
    fixture.schedule();
    gate.started();
    let health = health_snapshot();
    assert!(health.as_array().unwrap().iter().any(|job| job["root"]
        == fixture.job_root().as_path().to_string_lossy().as_ref()
        && job["phase"] == "derived"
        && job["barrier_holder"] == false));
    let bind = fixture.executor.submit(
        fixture.root.clone(),
        Lane::Mutating,
        "route.bind".to_owned(),
        Box::new(|_| Response::success("route.bind", serde_json::json!({}))),
    );
    let read = fixture.executor.submit(
        fixture.root.clone(),
        Lane::PureRead,
        "view.read".to_owned(),
        Box::new(|ctx| {
            let CallgraphStoreAccess::Ready(store) = ctx.callgraph_store_for_ops() else {
                panic!("previous view reader unavailable")
            };
            assert!(crate::callgraph_store::CallGraphRead::node_for(
                &store,
                Path::new("tracked.rs"),
                "previous"
            )
            .is_ok());
            Response::success("view.read", serde_json::json!({}))
        }),
    );
    // Completion before releasing the build proves it holds neither the writer
    // barrier nor a reader reservation. These timeouts only guard against hangs;
    // thread scheduling and pointer fsync latency are not the property under test.
    let bound = bind
        .recv_timeout(Duration::from_secs(60))
        .expect("same-root bind must complete while publication is held");
    assert!(bound.success, "same-root bind failed: {bound:?}");
    let read_result = read
        .recv_timeout(Duration::from_secs(60))
        .expect("same-root read must complete while publication is held");
    assert!(
        read_result.success,
        "previous view read failed: {read_result:?}"
    );
    assert_eq!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(fixture.initial.as_str())
    );
    drop(gate);
    fixture.wait_idle();
    assert_ne!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(fixture.initial.as_str())
    );
}

#[test]
fn invalidated_publication_cancels_before_derived_and_removes_generation_files() {
    let fixture = Fixture::new();
    fixture.change("older");
    let completions = Completions::new(fixture.job_root());
    let gate = Gate::new(fixture.job_root().as_path(), "blobs");
    fixture.schedule();
    let older = gate.started();
    cancel_for_context(&fixture.ctx);
    drop(gate);
    assert_eq!(completions.next(), older);
    fixture.change("newer");
    let newer_gate = Gate::new(fixture.job_root().as_path(), "cas");
    fixture.schedule();
    let newer_id = newer_gate.started();
    let delay = std::env::var("AFT_TEST_TIMING_DELAY_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    std::thread::sleep(Duration::from_millis(delay));
    drop(newer_gate);
    // Waiting for lifecycle completion also observes cleanup, not just CAS.
    assert_eq!(completions.next(), newer_id);
    let newer = fixture.view.current_generation().unwrap();
    assert_ne!(newer.as_deref(), Some(fixture.initial.as_str()));
    assert_eq!(fixture.view.current_generation().unwrap(), newer);
    assert!(
        !PHASES.lock()[fixture.job_root().as_path()]
            .iter()
            .any(|(id, phase)| *id == older && phase == "derived"),
        "superseded assembly entered derived phase instead of cancelling"
    );
    let files = std::fs::read_dir(fixture.view.view_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with("derived-")
                || name.starts_with("manifest-")
                || name.starts_with("trigram-")
        })
        .collect::<Vec<_>>();
    assert!(files.iter().all(|name| name.contains(&fixture.initial) || name.contains(newer.as_deref().unwrap())), "orphan generation files: {files:?}");
    let CallgraphStoreAccess::Ready(store) = fixture.ctx.callgraph_store_for_ops() else {
        panic!("newer reader unavailable")
    };
    assert!(crate::callgraph_store::CallGraphRead::node_for(
        &store,
        Path::new("tracked.rs"),
        "newer"
    )
    .is_ok());
}

#[test]
fn generation_sweep_waits_for_the_last_query_handle() {
    let fixture = Fixture::new();
    let CallgraphStoreAccess::Ready(previous_reader) = fixture.ctx.callgraph_store_for_ops() else {
        panic!("previous reader unavailable")
    };
    // The initial publication also owns a deferred checkpoint connection. Settle
    // it before replacing the view, so the old query handle is the only keeper
    // of the obsolete database whose lifetime this test asserts.
    crate::views::wait_for_derived_checkpoint_for_test(fixture.view.view_dir());
    fixture.change("next");
    fixture.schedule();
    fixture.wait_idle();
    assert_eq!(fixture.view.sweep_generations().unwrap(), 0);
    assert!(fixture
        .view
        .derived_path(&fixture.initial)
        .unwrap()
        .is_file());
    assert!(crate::callgraph_store::CallGraphRead::node_for(
        &previous_reader,
        Path::new("tracked.rs"),
        "previous"
    )
    .is_ok());
    drop(previous_reader);
    assert_eq!(fixture.view.sweep_generations().unwrap(), 1);
    assert!(!fixture
        .view
        .derived_path(&fixture.initial)
        .unwrap()
        .exists());
    assert!(!fixture
        .view
        .manifest_path(&fixture.initial)
        .unwrap()
        .exists());
}

#[test]
fn publication_health_reports_root_and_each_off_lane_phase() {
    let fixture = Fixture::new();
    for phase in ["manifest", "blobs", "derived", "cas"] {
        fixture.change(&format!("phase_{phase}"));
        let gate = Gate::new(fixture.job_root().as_path(), phase);
        fixture.schedule();
        let id = gate.started();
        let health = health_snapshot();
        let job = health
            .as_array()
            .unwrap()
            .iter()
            .find(|job| job["id"] == id)
            .unwrap();
        assert_eq!(
            job["root"],
            fixture.job_root().as_path().to_string_lossy().as_ref()
        );
        assert_eq!(job["phase"], phase);
        assert_eq!(job["barrier_holder"], false);
        assert!(
            !fixture.executor.actor_is_idle(&fixture.root),
            "an off-lane build must prevent actor retirement"
        );
        drop(gate);
        fixture.wait_idle();
    }
}

#[test]
fn deleted_root_pending_publication_retires_once_without_rescheduling() {
    let fixture = Fixture::new();
    fixture.change("next");
    let root = fixture.job_root();
    let before = fixture.ctx.view_publication_attempts_for_test();
    // The detached job reports "manifest" twice per attempt: once before it
    // queues for its cold-build slot, and again as the first step of
    // preparation, after the attempt counter has moved. Hold it at the second
    // report, so the root disappears while preparation is running.
    //
    // Holding it at the first report instead makes the count depend on timing.
    // Slot admission checks cancellation, and that check probes the root on
    // disk again only once 50 ms (or 256 checks) have passed since its last
    // probe on the worker thread. When the deletion takes longer than that, as
    // it can on Windows, admission sees the deleted root and the job retires
    // without preparing at all: zero attempts, which is also correct.
    let gate = Gate::after(&root, "manifest", 1);
    let completions = Completions::new(root.clone());
    fixture.schedule();
    gate.started();
    assert_eq!(
        fixture.ctx.view_publication_attempts_for_test(),
        before + 1,
        "the gate must hold the publication inside its preparation attempt"
    );
    std::fs::remove_dir_all(fixture.project.path()).unwrap();
    drop(gate);
    completions.next();
    fixture.wait_idle();
    assert_eq!(
        fixture.ctx.view_publication_attempts_for_test(),
        before + 1,
        "pending publication must make exactly one preparation attempt"
    );
    assert!(
        fixture.ctx.view_runtime_snapshot().is_none(),
        "deletion must retire views state, not just stop the current thread"
    );
    // Repeated quiet-window ticks simulate watcher publication retries after
    // checkout deletion. Late paths must not recreate a retired publication.
    let mut state = crate::context::WatcherDrainSliceState::new(
        fixture.ctx.configure_generation(),
        fixture.ctx.configure_content_generation(),
    );
    for _ in 0..3 {
        state.view_publication_due = Some(Instant::now());
        state.view_publication_paths.insert(root.join("tracked.rs"));
        crate::runtime_drain::publish_view_if_quiet(&fixture.ctx, &mut state);
        assert!(state.view_publication_due.is_none());
        assert!(state.view_publication_paths.is_empty());
        assert!(schedule(&fixture.ctx, BTreeSet::new(), true).is_ok());
    }
    assert_eq!(
        fixture.ctx.view_publication_attempts_for_test(),
        before + 1,
        "deleted publication must never be re-scheduled"
    );
    assert!(!running_for_context(&fixture.ctx));
}

/// Waits until the limiter's queue shows a waiter of `kind`. The deadline only
/// guards against a hang; the condition is the queue state itself.
fn wait_until_queued(
    limiter: &crate::cold_build_limiter::ColdBuildLimiter,
    kind: &str,
    failure: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !limiter.census().queued.iter().any(|job| job.kind == kind) {
        assert!(
            Instant::now() < deadline,
            "{failure}: {:?}",
            limiter.census()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn publication_queues_for_a_cold_build_slot_behind_an_earlier_waiter() {
    let fixture = Fixture::new();
    fixture.ctx.isolate_cold_build_limiter_for_test(1);
    let limiter = fixture.ctx.cold_build_limiter();
    let held = limiter.try_acquire().expect("hold the only slot");
    let (admitted_tx, admitted_rx) = crossbeam_channel::bounded(1);
    let earlier = {
        let limiter = Arc::clone(&limiter);
        let root = fixture.job_root();
        std::thread::spawn(move || {
            let permit = crate::cold_build_limiter::acquire_blocking_while_with_limiter(
                &limiter,
                "earlier maintenance",
                || true,
            )
            .expect("earlier waiter is admitted");
            // Observed while the earlier waiter owns the slot: the publication
            // must still be queued and must not have started building.
            let publication_queued = limiter
                .census()
                .queued
                .iter()
                .any(|job| job.kind == PUBLICATION_LIMITER_KIND);
            let publication_built = PHASES
                .lock()
                .get(&root)
                .is_some_and(|phases| phases.iter().any(|(_, phase)| phase == "blobs"));
            admitted_tx
                .send((publication_queued, publication_built))
                .unwrap();
            drop(permit);
        })
    };
    wait_until_queued(
        &limiter,
        "earlier maintenance",
        "the earlier request never queued",
    );
    fixture.change("next");
    fixture.schedule();
    wait_until_queued(
        &limiter,
        PUBLICATION_LIMITER_KIND,
        "the publication never queued for a cold-build slot",
    );
    drop(held);
    let (publication_queued, publication_built) = admitted_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the earlier waiter was admitted");
    earlier.join().unwrap();
    assert!(
        publication_queued && !publication_built,
        "the publication took the freed slot ahead of an earlier waiter \
         (queued={publication_queued} built={publication_built})"
    );
    fixture.wait_idle();
    assert_ne!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(fixture.initial.as_str())
    );
}

#[test]
fn publication_waiting_for_the_epoch_write_gate_holds_no_cold_build_slot() {
    let fixture = Fixture::new();
    fixture.ctx.isolate_cold_build_limiter_for_test(1);
    let limiter = fixture.ctx.cold_build_limiter();
    let (epoch, detached_writers) = {
        let state = fixture.executor.inner.state.lock();
        let actor = state.actors.get(&fixture.root).expect("actor");
        (
            Arc::clone(&actor.epoch),
            Arc::clone(&actor.detached_writers),
        )
    };
    fixture.change("next");
    // The held read guard simulates maintenance work on this root that has
    // not yet released the actor's read gate.
    let reader = epoch.read();
    fixture.schedule();
    // A detached publication counts itself as writer demand while it waits
    // for the epoch write gate.
    let deadline = Instant::now() + Duration::from_secs(60);
    while detached_writers.load(std::sync::atomic::Ordering::Acquire) == 0 {
        assert!(
            Instant::now() < deadline,
            "the publication never reached the epoch write gate: {}",
            health_snapshot()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let census = limiter.census();
    assert!(
        census.holders.is_empty(),
        "the publication held a cold-build slot while waiting for the epoch write gate: {census:?}"
    );
    drop(reader);
    fixture.wait_idle();
    assert_ne!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(fixture.initial.as_str())
    );
}

/// A publication writes path status only after it has left its commit
/// barrier: the root actor's write gate and the job table are held only for
/// the pointer swap, never for status writes into SQLite, which on a large
/// checkout would make reads and other projects' publications wait.
#[test]
fn publication_status_writes_run_outside_the_commit_barrier() {
    let fixture = Fixture::new();
    let view_dir = fixture.view.view_dir().to_owned();
    // A row the next publication settles, so it has status to write.
    crate::path_status::PathStatusStore::open(&view_dir)
        .unwrap()
        .mark_pending(b"gone.rs", "view publication scheduled", 1)
        .unwrap();
    let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&writes);
    crate::views::assembly::before_settle_write_for_test(&view_dir, move || {
        seen.lock()
            .unwrap()
            .push(super::in_commit_barrier_for_test());
    });
    fixture.change("next");
    fixture.schedule();
    fixture.wait_idle();
    assert_eq!(
        *writes.lock().unwrap(),
        vec![false],
        "status written inside the commit barrier, or not at all"
    );
    assert!(crate::path_status::PathStatusStore::open(&view_dir)
        .unwrap()
        .status_for(b"gone.rs")
        .unwrap()
        .is_none());
}

#[test]
fn context_keeps_a_typed_conflict_and_the_winning_generation_for_retry() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.project.path().join("tracked.rs"),
        "pub fn contender() {}\n",
    )
    .unwrap();
    let paths = BTreeSet::from([b"tracked.rs".to_vec()]);
    let mut first = fixture
        .ctx
        .prepare_view_paths(paths.clone(), true, &mut |_| Ok(()))
        .unwrap();
    let mut second = fixture
        .ctx
        .prepare_view_paths(paths, true, &mut |_| Ok(()))
        .unwrap();
    let winner = fixture
        .ctx
        .commit_view_update(&mut first)
        .unwrap()
        .generation
        .unwrap();
    let error = fixture.ctx.commit_view_update(&mut second).unwrap_err();
    assert!(
        matches!(&error, PublicationFailure::View(crate::views::ViewError::PublicationConflict {
        attempted_generation, current_generation: Some(current),
    }) if !attempted_generation.is_empty() && *current == winner)
    );
    assert_eq!(
        fixture
            .ctx
            .view_runtime_snapshot()
            .unwrap()
            .generation
            .as_deref(),
        Some(winner.as_str())
    );
    let mut retry = PublicationRetry::default();
    retry.failed(fixture.project.path(), "storage failure");
    retry.failed(fixture.project.path(), "storage failure");
    retry.failed_publication(fixture.project.path(), &error);
    assert!(matches!(retry.category, Some(FailureCategory::Conflict)));
    assert_eq!(retry.failures, 1);
    assert_eq!(retry.delay, retry.initial);
    assert!(retry.due().is_some());
    retry.failed_publication(
        fixture.project.path(),
        &PublicationFailure::Other(error.to_string()),
    );
    assert!(matches!(
        retry.category,
        Some(FailureCategory::StorageOrBuild)
    ));
    assert_eq!(
        retry.failures, 1,
        "formatted text must not decide the retry category"
    );
}

/// A publication writes its derived-output index row only after it has left
/// its commit barrier. The index is a SQLite file shared by every checkout of
/// the family, possibly held by another process; writing it under the root's
/// write gate or the process-wide job table would make this root's queries and
/// other roots' publications wait for an optional hint.
#[test]
fn publication_writes_the_derived_index_outside_the_commit_barrier() {
    use crate::views::assembly::{before_index_write_for_test, index_writes_for_test, IndexWrite};
    let fixture = Fixture::new();
    let other = Fixture::new();
    let view_dir = fixture.view.view_dir().to_owned();
    assert_eq!(
        index_writes_for_test(&view_dir),
        vec![IndexWrite::Recorded],
        "the fixture's first publication must build and index a derived graph"
    );
    let index = fixture
        ._storage
        .path()
        .join("blobs")
        .join("publication-fixture")
        .join("derived-index.sqlite");
    assert!(index.is_file());
    // Another connection holds the index's write lock, as another process
    // updating the family's index would.
    let holder = rusqlite::Connection::open(&index).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let epoch = {
        let state = fixture.executor.inner.state.lock();
        Arc::clone(&state.actors.get(&fixture.root).expect("actor").epoch)
    };
    let (entered_tx, entered_rx) = crossbeam_channel::bounded::<(bool, bool, bool)>(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
    before_index_write_for_test(&view_dir, move || {
        // Whether this thread still holds the barrier, whether the job table
        // is free, and whether this root's write gate is released.
        let state = (
            super::in_commit_barrier_for_test(),
            super::JOBS.try_lock().is_some(),
            epoch.try_read().is_some(),
        );
        let _ = entered_tx.try_send(state);
        // Blocks only the first write: the test drops the sender to release it.
        let _ = release_rx.recv_timeout(Duration::from_secs(30));
    });

    fixture.change("next");
    fixture.schedule();
    let state = entered_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the publication never reached its index write");
    assert_eq!(
        state,
        (false, true, true),
        "(inside commit barrier, job table free, write gate free) at the index write"
    );
    // The new generation is already served while the index write waits.
    assert_ne!(
        fixture.view.current_generation().unwrap().as_deref(),
        Some(fixture.initial.as_str())
    );
    // A publication on another root needs the job table and finishes now.
    other.change("elsewhere");
    other.schedule();
    other.wait_idle();
    assert_ne!(
        other.view.current_generation().unwrap().as_deref(),
        Some(other.initial.as_str())
    );
    drop(release_tx);
    fixture.wait_idle_for(Duration::from_secs(30));
    assert_eq!(
        index_writes_for_test(&view_dir).last(),
        Some(&IndexWrite::Failed),
        "a locked index is skipped with a log line, not waited on"
    );

    // Once the lock is gone, the next publication records its row.
    holder.execute_batch("ROLLBACK;").unwrap();
    drop(holder);
    fixture.change("again");
    fixture.schedule();
    fixture.wait_idle_for(Duration::from_secs(30));
    assert_eq!(
        index_writes_for_test(&view_dir).last(),
        Some(&IndexWrite::Recorded)
    );
}
