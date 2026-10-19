//! Offline, thread-CPU profiling of the real health rollup, not a parallel implementation.
use super::*;
use std::cell::RefCell;
use std::collections::BTreeMap;

thread_local! {
    static STEPS: RefCell<Option<BTreeMap<&'static str, u64>>> = const { RefCell::new(None) };
}

fn cpu_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
        0
    );
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

pub(super) struct Timer(Option<u64>);

impl Timer {
    pub(super) fn new() -> Self {
        Self(STEPS.with(|steps| steps.borrow().is_some().then(cpu_ns)))
    }

    pub(super) fn mark(&mut self, step: &'static str) {
        if let Some(previous) = self.0 {
            let now = cpu_ns();
            STEPS.with(|steps| {
                *steps
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .entry(step)
                    .or_default() += now - previous;
            });
            self.0 = Some(now);
        }
    }
}

#[test]
#[ignore = "offline per-step CPU measurement with 40 roots and a large aft.db"]
fn bench_health_rollup_steps() {
    use super::super::test_support::test_root;
    const ROOTS: usize = 40;
    const TICKS: usize = 20;
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    let db_path = storage.join("aft.db");
    let mut db = crate::db::open(&db_path).unwrap();
    let tx = db.transaction().unwrap();
    tx.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<160000)
        INSERT INTO bash_tasks (harness, session_id, task_id, project_key, command, cwd, status, started_at, completed_at, completion_delivered)
        SELECT 'opencode', 'session', printf('task-%d',x), 'project', hex(zeroblob(1000)), '.', 'completed', 0, 1, 1 FROM n;").unwrap();
    tx.commit().unwrap();
    let bytes: u64 = std::fs::metadata(&db_path).unwrap().len();
    let app = App::default_shared();
    app.set_db(Arc::new(std::sync::Mutex::new(db)));
    app.set_open_route_count(300);
    let executor = Executor::with_config(crate::executor::ExecutorConfig {
        pool_size: 1,
        read_cap: 1,
        actor_cap: 64,
        heavy_permits: 1,
        drr_quantum: 1,
    });
    let mut dirs = Vec::new();
    let mut registrations = Vec::new();
    for i in 0..ROOTS {
        let (dir, root) = test_root(&format!("rollup-steps-{i}"));
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                project_root: Some(root.as_path().into()),
                ..crate::config::Config::default()
            },
        ));
        let family = format!("bench-family-{i}");
        let registry = crate::views::registry::FamilyRegistry::open(&storage, &family).unwrap();
        for view in 0..10 {
            let scope = format!("bench-{i}-{view}");
            let registration = registry.register_view(&scope, root.as_path()).unwrap();
            let mut status =
                crate::path_status::PathStatusStore::open(registration.view_dir()).unwrap();
            status.mark_pending(b"src/a.rs", "bench", 1).unwrap();
            status.mark_failed(b"src/b.rs", "bench", 1).unwrap();
            if view == 0 {
                ctx.update_config(|config| config.views.enabled = true);
                ctx.install_view_runtime(
                    crate::context::ViewRuntimeSnapshot {
                        query_pin: None,
                        storage: storage.clone(),
                        family: family.clone(),
                        scope,
                        view_dir: registration.view_dir().to_path_buf(),
                        generation: Some("1-bench".into()),
                        manifest: None,
                        head_fingerprint: String::new(),
                        head_metadata: crate::alias::GitHeadMetadata {
                            head_path: std::path::PathBuf::new(),
                            head_mtime: None,
                            resolved_ref_path: None,
                            resolved_ref_mtime: None,
                        },
                        pending_paths: Default::default(),
                        pending_inputs: Default::default(),
                    },
                    None,
                );
            }
            registrations.push(registration);
        }
        assert!(executor.register_actor(root, ctx));
        dirs.push(dir);
    }
    let cache = HealthRollupCache::new();
    cache.clock_ms.store(0, Ordering::Relaxed);
    cache.refresh_for_test(&executor, &app);
    assert_eq!(registrations.len(), 400);
    assert!(executor.actor_entries().iter().all(|(_, ctx)| {
        ctx.view_health_snapshot()
            .is_some_and(|view| view.pending_paths == 1 && view.failed_paths == 1)
    }));
    STEPS.with(|steps| *steps.borrow_mut() = Some(BTreeMap::new()));
    let started = cpu_ns();
    for tick in 1..=TICKS {
        cache.clock_ms.store(tick as u64 * 3000, Ordering::Relaxed);
        let built = build_health_diagnostic_rollup_from_actors(
            &cache,
            &executor,
            &app,
            executor.actor_entries(),
        );
        let mut publish_timer = Timer::new();
        cache.publish(Some(built));
        publish_timer.mark("publish holder / old JSON drop");
    }
    let total = cpu_ns() - started;
    let steps = STEPS.with(|steps| steps.borrow_mut().take().unwrap());
    println!("roots={ROOTS} routes=300 persisted_views=400 installed_views=40 aft_db_bytes={bytes} ticks={TICKS} interval_s=3 CPU (not wall)");
    for (step, ns) in steps {
        println!(
            "{step}: us/tick={:.2} us/second={:.2}",
            ns as f64 / TICKS as f64 / 1000.,
            ns as f64 / TICKS as f64 / 3000.
        );
    }
    println!(
        "TOTAL: us/tick={:.2} us/second={:.2}",
        total as f64 / TICKS as f64 / 1000.,
        total as f64 / TICKS as f64 / 3000.
    );
    let metrics = DispatchPathMetrics::new();
    let pending = HashMap::new();
    let before = cpu_ns();
    for _ in 0..TICKS {
        let report = build_health_report(&cache, &executor, &pending, &metrics, &app);
        std::hint::black_box(serde_json::to_vec(&report.metrics).unwrap());
    }
    println!(
        "on-demand reply + wire JSON (not tick): us/probe={:.2}",
        (cpu_ns() - before) as f64 / TICKS as f64 / 1000.
    );
}
