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
#[ignore = "offline per-step measurement with configurable roots and a large aft.db"]
fn bench_health_rollup_steps() {
    use super::super::test_support::test_root;
    // The defaults use 40 roots. The variables below
    // model a busier daemon (the live one runs 100+ roots, hundreds of
    // threads, populated indexes and views that publish continuously):
    //   AFT_ROLLUP_BENCH_ROOTS    bound roots, one installed view each
    //   AFT_ROLLUP_BENCH_CHECKOUTS_PER_FAMILY  bound checkouts sharing a family
    //   AFT_ROLLUP_BENCH_THREADS  extra idle named threads in the process
    //   AFT_ROLLUP_BENCH_FILES    files in each root's in-memory trigram index
    //   AFT_ROLLUP_BENCH_CHURN=1  between ticks, every root republishes its
    //                             view without changing its path status
    //                             (open, re-mark, clear, close) and reads one new file into its
    //                             symbol cache; one root in ten also edits a
    //                             file in its trigram index
    //   AFT_ROLLUP_BENCH_ARENA_CHUNKS  resident chunks held by views-on
    //                             semantic arenas, spread over four families
    //                             in runs of 100 chunks (views-on daemons
    //                             hold hundreds of thousands)
    //   AFT_ROLLUP_BENCH_CENSUS_READS=1  read the memory census after every
    //                             tick, as a client polling `memory.census`
    //                             does
    let env_count = |name: &str, default: usize| -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    let roots_count = env_count("AFT_ROLLUP_BENCH_ROOTS", 40);
    let checkouts_per_family = env_count("AFT_ROLLUP_BENCH_CHECKOUTS_PER_FAMILY", 1).max(1);
    let thread_count = env_count("AFT_ROLLUP_BENCH_THREADS", 0);
    let files_per_root = env_count("AFT_ROLLUP_BENCH_FILES", 0);
    let churn = std::env::var("AFT_ROLLUP_BENCH_CHURN").is_ok_and(|value| value == "1");
    let arena_chunks = env_count("AFT_ROLLUP_BENCH_ARENA_CHUNKS", 0);
    let census_reads =
        std::env::var("AFT_ROLLUP_BENCH_CENSUS_READS").is_ok_and(|value| value == "1");
    const TICKS: usize = 20;
    let (park_tx, park_rx) = std::sync::mpsc::channel::<()>();
    let park_rx = Arc::new(std::sync::Mutex::new(park_rx));
    let parked = (0..thread_count)
        .map(|index| {
            let park_rx = Arc::clone(&park_rx);
            std::thread::Builder::new()
                .name(format!("bench-idle-{index}"))
                .spawn(move || {
                    let _ = park_rx.lock().map(|rx| rx.recv());
                })
                .unwrap()
        })
        .collect::<Vec<_>>();
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
    // Arenas live while this function holds them; the census finds them
    // through the process registry, as it finds the daemon's.
    let arenas = (0..4)
        .map(|family| {
            crate::views::semantic_arena::arena_for(&storage, &format!("bench-arena-{family}"))
        })
        .collect::<Vec<_>>();
    for run in 0..arena_chunks.div_ceil(100) {
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(&(run as u64).to_le_bytes());
        arenas[run % arenas.len()].install(
            crate::blob_store::v2::FamilyKey::new(
                crate::blob_store::v2::FamilyPlane::Semantic,
                key,
            ),
            crate::semantic_index::SemanticVectors::synthetic_for_test(
                Path::new(&format!("src/arena_{run}.rs")),
                100,
                4,
            ),
        );
    }
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
    let mut churn_view_dirs = Vec::new();
    for i in 0..roots_count {
        let (dir, root) = test_root(&format!("rollup-steps-{i}"));
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.clone()),
                project_root: Some(root.as_path().into()),
                ..crate::config::Config::default()
            },
        ));
        let family = format!("bench-family-{}", i / checkouts_per_family);
        let registry = crate::views::registry::FamilyRegistry::open(&storage, &family).unwrap();
        for view in 0..10 {
            let scope = format!("bench-{i}-{view}");
            let registration = registry.register_view(&scope, root.as_path()).unwrap();
            let mut status =
                crate::path_status::PathStatusStore::open(registration.view_dir()).unwrap();
            status.mark_pending(b"src/a.rs", "bench", 1).unwrap();
            status.mark_failed(b"src/b.rs", "bench", 1).unwrap();
            if view == 0 {
                if churn {
                    churn_view_dirs.push(registration.view_dir().to_path_buf());
                }
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
        if files_per_root > 0 {
            let mut index = crate::search_index::SearchIndex::new();
            for file in 0..files_per_root {
                let path = root
                    .as_path()
                    .join(format!("src/module_{}/file_{file}.rs", file % 64));
                index.index_file(&path, format!("fn item_{file}() {{}}").as_bytes());
            }
            *ctx.search_index().write().unwrap() = Some(index);
        }
        assert!(executor.register_actor(root, ctx));
        dirs.push(dir);
    }
    let cache = HealthRollupCache::new();
    cache.clock_ms.store(0, Ordering::Relaxed);
    cache.refresh_for_test(&executor, &app);
    assert_eq!(registrations.len(), roots_count * 10);
    assert!(executor.actor_entries().iter().all(|(_, ctx)| {
        ctx.view_health_snapshot()
            .is_some_and(|view| view.pending_paths == 1 && view.failed_paths == 1)
    }));
    let actors = executor.actor_entries();
    let mut snapshot_cpu = 0;
    let mut snapshot_elapsed = std::time::Duration::ZERO;
    let reads_before = crate::path_status::HEALTH_READS.with(std::cell::Cell::get);
    for _ in 0..TICKS {
        if churn {
            for view_dir in &churn_view_dirs {
                let mut status = crate::path_status::PathStatusStore::open(view_dir).unwrap();
                status.mark_pending(b"src/a.rs", "bench", 1).unwrap();
                status.clear_many([&b"src/gone.rs"[..]]).unwrap();
            }
        }
        let cpu = cpu_ns();
        let elapsed = std::time::Instant::now();
        for (_, ctx) in &actors {
            std::hint::black_box(ctx.view_health_snapshot());
        }
        snapshot_cpu += cpu_ns() - cpu;
        snapshot_elapsed += elapsed.elapsed();
    }
    println!(
        "view_health_snapshot only: roots={roots_count} checkouts_per_family={checkouts_per_family} ticks={TICKS} cpu_us/tick={:.2} elapsed_ms/tick={:.3}",
        snapshot_cpu as f64 / TICKS as f64 / 1000.,
        snapshot_elapsed.as_secs_f64() * 1000. / TICKS as f64,
    );
    println!(
        "snapshot phase SQLite reopens={}",
        crate::path_status::HEALTH_READS.with(std::cell::Cell::get) - reads_before
    );
    STEPS.with(|steps| *steps.borrow_mut() = Some(BTreeMap::new()));
    let census_builds_before = cache.census_builds_for_test();
    let mut total = 0u64;
    let mut elapsed_steps = BTreeMap::<String, f64>::new();
    let mut elapsed_total = 0.0;
    for tick in 1..=TICKS {
        if churn {
            // What publishing views, reads and an occasional edit do between
            // two ticks; kept out of the measured time.
            for view_dir in &churn_view_dirs {
                let mut status = crate::path_status::PathStatusStore::open(view_dir).unwrap();
                status
                    .mark_pending_many([(&b"src/a.rs"[..], "bench")], 1)
                    .unwrap();
                status.clear_many([&b"src/gone.rs"[..]]).unwrap();
            }
            let path = format!("src/churn_{tick}.rs");
            for (index, (root, ctx)) in executor.actor_entries().into_iter().enumerate() {
                let file = root.as_path().join(&path);
                ctx.symbol_cache().write().unwrap().insert(
                    file.clone(),
                    std::time::SystemTime::UNIX_EPOCH,
                    1,
                    blake3::hash(path.as_bytes()),
                    Vec::new(),
                );
                if index % 10 == tick % 10 {
                    if let Some(index) = ctx.search_index().write().unwrap().as_mut() {
                        index.index_file(&file, b"fn churn() {}");
                    }
                }
            }
        }
        let started = cpu_ns();
        cache.clock_ms.store(tick as u64 * 3000, Ordering::Relaxed);
        let built = build_health_diagnostic_rollup_from_actors(
            &cache,
            &executor,
            &app,
            executor.actor_entries(),
        );
        let tick = &built.tick;
        elapsed_total += tick["elapsed_ms"].as_f64().unwrap();
        for (step, value) in tick["steps"].as_object().unwrap() {
            *elapsed_steps.entry(step.clone()).or_default() +=
                value["elapsed_ms"].as_f64().unwrap();
        }
        let mut publish_timer = Timer::new();
        cache.publish(Some(built));
        publish_timer.mark("publish holder / old JSON drop");
        total += cpu_ns() - started;
        if census_reads {
            std::hint::black_box(cache.memory_census());
        }
    }
    let steps = STEPS.with(|steps| steps.borrow_mut().take().unwrap());
    println!(
        "roots={roots_count} routes=300 persisted_views={} installed_views={roots_count} extra_threads={thread_count} files_per_root={files_per_root} churn={churn} arena_chunks={arena_chunks} census_reads={census_reads} aft_db_bytes={bytes} ticks={TICKS} interval_s=3 CPU (not wall)",
        roots_count * 10
    );
    for (step, ns) in steps {
        println!(
            "{step}: us/tick={:.2} us/second={:.2}",
            ns as f64 / TICKS as f64 / 1000.,
            ns as f64 / TICKS as f64 / 3000.
        );
    }
    for (step, ms) in elapsed_steps {
        println!("{step}: elapsed_ms/tick={:.3}", ms / TICKS as f64);
    }
    println!(
        "BUILD TOTAL: elapsed_ms/tick={:.3}",
        elapsed_total / TICKS as f64
    );
    println!(
        "TOTAL: us/tick={:.2} us/second={:.2}",
        total as f64 / TICKS as f64 / 1000.,
        total as f64 / TICKS as f64 / 3000.
    );
    println!(
        "memory census measured on {} of {TICKS} ticks",
        cache.census_builds_for_test() - census_builds_before
    );
    let mut wal_sizes = BTreeMap::<u64, usize>::new();
    for registration in &registrations {
        let bytes = std::fs::metadata(registration.view_dir().join("derived.sqlite-wal"))
            .map_or(0, |metadata| metadata.len());
        *wal_sizes.entry(bytes).or_default() += 1;
    }
    println!("closed path-status WAL bytes per store (bytes: stores): {wal_sizes:?}");
    println!(
        "closed path-status WAL total_bytes={}",
        wal_sizes
            .iter()
            .map(|(bytes, stores)| bytes * *stores as u64)
            .sum::<u64>()
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
    drop(park_tx);
    drop(arenas);
    for thread in parked {
        thread.join().unwrap();
    }
}
