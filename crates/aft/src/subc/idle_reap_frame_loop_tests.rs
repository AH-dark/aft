//! Cost of the idle-root reaper's sweep, which runs every drain tick
//! (`DRAIN_TICK_PERIOD`) on the subc frame loop that also reads and writes
//! every client frame.
//!
//! An unbound root past its idle TTL that an artifact blocker keeps resident
//! (a standing root, running bash, an unpersisted search delta) has its
//! pending watcher-derived path sets taken and put back on every sweep. These
//! tests pin what that round trip costs.

use super::test_support::{test_ctx, test_root};
use super::*;

/// Unbound, quiesced, TTL-aged roots, each registered with one executor and
/// kept resident by the standing-root artifact blocker.
struct BlockedUnboundRoots {
    _dirs: Vec<tempfile::TempDir>,
    contexts: Vec<(ProjectRootId, Arc<AppContext>)>,
    executor: Arc<Executor>,
    live_roots: HashMap<ProjectRootId, RootMeta>,
}

impl BlockedUnboundRoots {
    fn new(count: usize) -> Self {
        let executor = Arc::new(Executor::new());
        let mut dirs = Vec::with_capacity(count);
        let mut contexts = Vec::with_capacity(count);
        let mut live_roots = HashMap::with_capacity(count);
        for index in 0..count {
            let (dir, root) = test_root(&format!("idle-reap-frame-loop-{index}-"));
            let ctx = test_ctx();
            assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
            ctx.mark_subc_unbound();
            ctx.set_standing_artifact_exempt(true);
            let mut meta = RootMeta::new(Instant::now());
            meta.unbound_quiesced = true;
            live_roots.insert(root.clone(), meta);
            dirs.push(dir);
            contexts.push((root, ctx));
        }
        Self {
            _dirs: dirs,
            contexts,
            executor,
            live_roots,
        }
    }

    /// Replace every root's pending sets with `per_set` paths in each of the
    /// search, callgraph, Tier-2 and semantic sets. The paths share the root
    /// prefix and nest a few directories deep, as watcher events do, so
    /// ordering them compares several leading components first.
    fn fill_pending(&self, per_set: usize) {
        for (root, ctx) in &self.contexts {
            ctx.clear_pending_index_updates();
            let paths = || {
                (0..per_set).map(|index| {
                    root.as_path().join(format!(
                        "src/module_{}/submodule_{}/file_{index}.rs",
                        index % 17,
                        index % 5
                    ))
                })
            };
            ctx.add_pending_search_index_paths(paths());
            ctx.add_pending_callgraph_store_paths(paths());
            ctx.add_pending_tier2_paths(paths());
            ctx.add_pending_semantic_index_paths(paths());
        }
    }

    /// One sweep as the frame loop runs it: past every root's TTL, with no
    /// bound routes, no binds in flight and no new presence observation.
    fn sweep(&mut self, metrics: &DispatchPathMetrics) -> IdleReapOutcome {
        reap_idle_roots_with_presence(
            Instant::now() + IDLE_ROOT_TTL + Duration::from_secs(1),
            &mut self.live_roots,
            &HashMap::new(),
            &HashMap::new(),
            &self.executor,
            metrics,
            &RootPresence::default(),
        )
    }
}

/// Per-sweep wall time of the reaper at the daemon's observed scale (about
/// 170 roots). Ignored by default because it measures time; run it with
/// `cargo test -p agent-file-tools --lib -- --ignored --nocapture
/// idle_reap_sweep_cost_at_170_roots`.
#[test]
#[ignore = "benchmark: prints per-sweep timings"]
fn idle_reap_sweep_cost_at_170_roots() {
    const ROOTS: usize = 170;
    const SWEEPS: usize = 21;
    let mut fixture = BlockedUnboundRoots::new(ROOTS);
    let metrics = DispatchPathMetrics::new();
    for per_set in [0, 50, 500, 2_000] {
        fixture.fill_pending(per_set);
        let mut samples = Vec::with_capacity(SWEEPS);
        for _ in 0..SWEEPS {
            let started = Instant::now();
            let outcome = fixture.sweep(&metrics);
            samples.push(started.elapsed());
            assert_eq!(outcome.evicted, 0, "the standing blocker must hold every root");
        }
        samples.sort();
        println!(
            "idle reap sweep: roots={ROOTS} pending_paths_per_root={} median={:?} min={:?} max={:?}",
            per_set * 4,
            samples[SWEEPS / 2],
            samples[0],
            samples[SWEEPS - 1],
        );
    }
}
