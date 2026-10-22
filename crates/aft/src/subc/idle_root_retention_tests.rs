//! What an idle-evicted root keeps in the process: contexts, threads and heap.
//!
//! The measurement runs in a child copy of this test binary so no other test
//! starts or stops threads while it counts them.

use super::*;
use std::sync::Weak;

const CHILD_ENV: &str = "AFT_TEST_IDLE_ROOT_RETENTION_CHILD";
const ROOTS: usize = 12;

fn run_in_child(test_name: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stdout}");
    assert!(
        output.status.success(),
        "measurement child failed:\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains("1 passed"), "child ran no test:\n{stdout}");
}

fn thread_total() -> u64 {
    crate::lifecycle_census::thread_census().total
}

/// Thread names, for the measurement's printed record (Linux only).
fn thread_names() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("comm")).ok())
        .map(|name| name.trim().to_string())
        .collect();
    names.sort();
    names
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn heap_in_use() -> Option<u64> {
    // SAFETY: mallinfo2 only reads allocator statistics.
    Some(unsafe { libc::mallinfo2() }.uordblks as u64)
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn heap_in_use() -> Option<u64> {
    None
}

/// Give detached teardown threads (watcher and LSP reapers) time to exit:
/// polls the thread count every 100 ms and returns once it is at most
/// `at_most`, has not changed for five samples, or 10 seconds have passed.
/// A settling heuristic, not proof that teardown finished.
fn settled_thread_total(at_most: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut previous = thread_total();
    let mut stable_samples = 0;
    loop {
        if previous <= at_most || stable_samples >= 5 || Instant::now() >= deadline {
            return previous;
        }
        std::thread::sleep(Duration::from_millis(100));
        let total = thread_total();
        stable_samples = if total == previous {
            stable_samples + 1
        } else {
            0
        };
        previous = total;
    }
}

#[test]
fn evicted_idle_roots_release_their_per_context_threads() {
    if std::env::var_os(CHILD_ENV).is_none() {
        run_in_child(
            "subc::idle_root_retention_tests::evicted_idle_roots_release_their_per_context_threads",
        );
        return;
    }

    let app = App::default_shared();
    let executor = Arc::new(Executor::new());
    let storage = tempfile::tempdir().unwrap();
    // Warm process-wide state (pools, registries) with one root that is
    // created and dropped, so the baseline holds only process-lifetime work.
    {
        let (_dir, root) = test_support::test_root("retention-warmup");
        let ctx = Arc::new(AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(root.as_path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        ));
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        executor.remove_actor(&root);
    }
    std::thread::sleep(Duration::from_millis(200));
    let baseline_threads = thread_total();
    let baseline_heap = heap_in_use();

    let mut dirs = Vec::new();
    let mut weak_contexts: Vec<Weak<AppContext>> = Vec::new();
    let mut live_roots = HashMap::new();
    for index in 0..ROOTS {
        let (dir, root) = test_support::test_root(&format!("retention-{index}"));
        let ctx = Arc::new(AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(root.as_path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        ));
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        weak_contexts.push(Arc::downgrade(&ctx));
        drop(ctx);
        live_roots.insert(root, RootMeta::new(Instant::now()));
        dirs.push(dir);
    }
    let bound_threads = thread_total();

    // Every route closed: quiesce each root, age it past its TTL and reap.
    let roots: Vec<_> = live_roots.keys().cloned().collect();
    for root in &roots {
        quiesce_unbound_root(root, &mut live_roots, &executor);
        live_roots.get_mut(root).unwrap().last_touched =
            Instant::now() - IDLE_ROOT_TTL - Duration::from_secs(1);
    }
    let outcome = reap_idle_roots(
        Instant::now(),
        &mut live_roots,
        &HashMap::new(),
        &HashMap::new(),
        &executor,
        &DispatchPathMetrics::new(),
    );
    assert_eq!(outcome.evicted, ROOTS, "every idle root is evicted");

    let evicted_threads = settled_thread_total(baseline_threads + 1);
    let evicted_heap = heap_in_use();
    let live_contexts = weak_contexts
        .iter()
        .filter(|ctx| ctx.strong_count() > 0)
        .count();
    println!(
        "retention: roots={ROOTS} baseline_threads={baseline_threads} bound_threads={bound_threads} \
         evicted_threads={evicted_threads} live_contexts={live_contexts} \
         heap_baseline={baseline_heap:?} heap_evicted={evicted_heap:?}"
    );
    assert!(
        evicted_threads <= baseline_threads + 1,
        "idle-evicted roots kept {} thread(s) for {ROOTS} roots (baseline {baseline_threads}, after eviction {evicted_threads})",
        evicted_threads.saturating_sub(baseline_threads)
    );
}

const WORKED_ROOTS: usize = 6;
const SOURCE_FILES: usize = 40;
const SESSION: &str = "retention-session";

fn request(command: &str, params: serde_json::Value) -> crate::protocol::RawRequest {
    crate::protocol::RawRequest {
        id: format!("{command}-retention"),
        command: command.to_string(),
        lsp_hints: None,
        session_id: Some(SESSION.to_string()),
        params,
    }
}

/// A Git checkout with enough Rust source that its search index, callgraph
/// and symbol cache hold real entries.
fn worked_fixture(name: &str) -> (tempfile::TempDir, ProjectRootId) {
    let (dir, root) = test_support::test_root(name);
    for index in 0..SOURCE_FILES {
        let mut source = String::new();
        for function in 0..20 {
            source.push_str(&format!(
                "/// Function {function} of module {index}.\npub fn item_{index}_{function}(value: u64) -> u64 {{\n    let doubled = value * 2;\n    helper_{index}(doubled) + {function}\n}}\n\n"
            ));
        }
        source.push_str(&format!(
            "fn helper_{index}(value: u64) -> u64 {{\n    value + 1\n}}\n"
        ));
        std::fs::write(dir.path().join(format!("module_{index}.rs")), source).unwrap();
    }
    // A legacy project config location: the project watcher never covers it,
    // so the root keeps a config file watch of its own.
    std::fs::create_dir_all(dir.path().join(".opencode")).unwrap();
    std::fs::write(dir.path().join(".opencode/aft.jsonc"), "{}\n").unwrap();
    for args in [
        &["init", "--quiet"][..],
        &["add", "."][..],
        &[
            "-c",
            "user.name=AFT Tests",
            "-c",
            "user.email=aft-tests@example.com",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ][..],
    ] {
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(dir.path()));
        assert!(command.args(args).status().unwrap().success());
    }
    (dir, root)
}

/// Configure the root, wait for its index builds, then do an agent's worth of
/// reads, outlines, searches and writes in one session.
fn work_root(ctx: &AppContext, root: &Path, storage: &Path) {
    let user_config = storage.join(format!(
        "user-{}.jsonc",
        root.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(
        &user_config,
        serde_json::json!({
            "indexes": {"trigram": true, "semantic": false, "callgraph": true}
        })
        .to_string(),
    )
    .unwrap();
    let configure = request(
        "configure",
        serde_json::json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "cortexkit_user_config_path": user_config,
        }),
    );
    {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let response = crate::commands::configure::handle_configure(&configure, ctx);
        assert!(response.success, "configure failed: {response:?}");
    }
    crate::commands::configure::drain_deferred_configure_maintenance(ctx);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        crate::runtime_drain::drain_search_index_events(ctx);
        crate::runtime_drain::drain_callgraph_store_events(ctx);
        crate::runtime_drain::drain_semantic_index_events(ctx);
        if ctx.search_index_rx().read().unwrap().is_none()
            && ctx.callgraph_store_rx().lock().is_none()
            && ctx.semantic_index_rx().lock().is_none()
        {
            break;
        }
        assert!(Instant::now() < deadline, "index builds did not settle");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        ctx.search_index().read().unwrap().is_some(),
        "the fixture must load a search index"
    );

    for index in 0..10 {
        let file = root.join(format!("module_{index}.rs"));
        let read = crate::commands::read::handle_read(
            &request("read", serde_json::json!({"file": file})),
            ctx,
        );
        assert!(read.success, "read failed: {read:?}");
        let outline = crate::commands::outline::handle_outline(
            &request("outline", serde_json::json!({"file": file})),
            ctx,
        );
        assert!(outline.success, "outline failed: {outline:?}");
    }
    for pattern in ["helper_3", "doubled", "item_7_1"] {
        let grep = crate::commands::grep::handle_grep(
            &request(
                "grep",
                serde_json::json!({"pattern": pattern, "max_results": 50}),
            ),
            ctx,
        );
        assert!(grep.success, "grep failed: {grep:?}");
    }
    for index in 0..3 {
        let file = root.join(format!("module_{index}.rs"));
        let mut content = std::fs::read_to_string(&file).unwrap();
        content.push_str("\npub fn appended() {}\n");
        let write = crate::commands::write::handle_write(
            &request(
                "write",
                serde_json::json!({"file": file, "content": content}),
            ),
            ctx,
        );
        assert!(write.success, "write failed: {write:?}");
    }
}

/// Give detached teardown work (LSP and watcher reapers, semantic lane
/// release) time to free its memory: samples allocator usage every 100 ms
/// and returns once two readings differ by less than 16 KiB, or after 50
/// samples. A settling heuristic; `None` where allocator statistics are
/// unavailable.
fn settled_heap() -> Option<u64> {
    let mut previous = heap_in_use()?;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        let current = heap_in_use()?;
        if current.abs_diff(previous) < 16 * 1024 {
            return Some(current);
        }
        previous = current;
    }
    Some(previous)
}

#[test]
fn evicted_worked_roots_keep_only_bookkeeping() {
    if std::env::var_os(CHILD_ENV).is_none() {
        run_in_child("subc::idle_root_retention_tests::evicted_worked_roots_keep_only_bookkeeping");
        return;
    }
    // The roots below use a legacy project config file, which the project
    // watcher never covers, so each root starts its own config file watch.
    crate::config_live::enable_config_watches_for_test();

    let app = App::default_shared();
    let executor = Arc::new(Executor::new());
    let storage = tempfile::tempdir().unwrap();
    let metrics = DispatchPathMetrics::new();
    let mut dirs = Vec::new();
    let mut live_roots = HashMap::new();

    // Bind, work, unbind, age and reap `roots`, returning weak references to
    // their contexts.
    let mut cycle = |names: std::ops::Range<usize>,
                     live_roots: &mut HashMap<ProjectRootId, RootMeta>|
     -> Vec<(ProjectRootId, Weak<AppContext>)> {
        let mut bound = Vec::new();
        for index in names {
            let (dir, root) = worked_fixture(&format!("retention-worked-{index}"));
            let ctx = Arc::new(AppContext::from_app(
                Arc::clone(&app),
                Config {
                    project_root: Some(root.as_path().to_path_buf()),
                    storage_dir: Some(storage.path().to_path_buf()),
                    ..Config::default()
                },
            ));
            assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
            work_root(&ctx, root.as_path(), storage.path());
            // Configure's deferred tail normally starts this watch once the
            // project watcher is up. Sync here as well so the fixture does not
            // depend on when that tail runs.
            crate::config_live::sync_config_watches(&ctx);
            assert!(
                ctx.config_live().has_project_fallback_watch(),
                "the legacy project config never got its own watch: {:?}",
                ctx.config_live()
                    .sources()
                    .map(|sources| sources.project.path)
            );
            let mut semantic = ctx.config().semantic.clone();
            semantic.backend = crate::config::SemanticBackend::OpenAiCompatible;
            semantic.base_url = Some("http://127.0.0.1:9".to_string());
            *ctx.semantic_embedding_model().lock() = Some(
                crate::semantic_index::EmbeddingModel::from_config(&semantic)
                    .expect("query embedding model"),
            );
            bound.push((root.clone(), Arc::downgrade(&ctx)));
            live_roots.insert(root, RootMeta::new(Instant::now()));
            dirs.push(dir);
        }
        for (root, _) in &bound {
            quiesce_unbound_root(root, live_roots, &executor);
            live_roots.get_mut(root).unwrap().last_touched =
                Instant::now() - IDLE_ROOT_TTL - Duration::from_secs(1);
        }
        // Writes leave unpersisted search deltas; the first sweeps queue
        // their teardown persist, and later sweeps evict once it has run.
        let deadline = Instant::now() + Duration::from_secs(60);
        while bound
            .iter()
            .any(|(root, _)| !live_roots[root].idle_artifacts_evicted)
        {
            assert!(Instant::now() < deadline, "worked roots were not evicted");
            reap_idle_roots(
                Instant::now(),
                live_roots,
                &HashMap::new(),
                &HashMap::new(),
                &executor,
                &metrics,
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        bound
    };

    // One root goes through the whole cycle first and is then retired, so
    // process-wide state (grammars, SQLite, pools) is warm before the
    // baseline.
    crate::cache_freshness::warm_strict_verify_pool_for_test();
    for (root, _) in cycle(0..1, &mut live_roots) {
        executor.remove_actor(&root);
        live_roots.remove(&root);
    }
    let baseline_heap = settled_heap();
    let baseline_threads = settled_thread_total(0);

    let evicted = cycle(1..WORKED_ROOTS + 1, &mut live_roots);
    // The reaper hands the reopenable resources (LSP, database pools, config
    // watches, embedding model, compiled output filters) to a background
    // release; wait for it.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let released = evicted.iter().all(|(_, ctx)| {
            let ctx = ctx.upgrade().expect("an evicted root keeps its context");
            let watches = ctx.config_live();
            !watches.has_project_fallback_watch()
                && !watches.has_user_watch()
                && ctx.semantic_embedding_model().lock().is_none()
                && !ctx.filter_registry_loaded()
        });
        if released {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "evicted roots kept their config watches, embedding model or output filters"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    for (_, ctx) in &evicted {
        let ctx = ctx.upgrade().unwrap();
        assert!(ctx.search_index().read().unwrap().is_none());
        assert!(ctx.semantic_index().read().unwrap().is_none());
        assert!(ctx.callgraph_store().read().unwrap().is_none());
        assert_eq!(ctx.symbol_cache().read().unwrap().len(), 0);
        assert!(!ctx.watcher_runtime_active());
    }
    let evicted_threads = settled_thread_total(baseline_threads);
    let evicted_heap = settled_heap();

    // Drop the evicted contexts: the heap that leaves with them is what each
    // evicted root was still holding.
    for (root, _) in &evicted {
        executor.remove_actor(root);
    }
    let dropped_heap = settled_heap();
    assert!(
        evicted.iter().all(|(_, ctx)| ctx.strong_count() == 0),
        "removed actors must drop their contexts"
    );
    let per_root = match (evicted_heap, dropped_heap) {
        (Some(evicted), Some(dropped)) => {
            Some(evicted.saturating_sub(dropped) / WORKED_ROOTS as u64)
        }
        _ => None,
    };
    println!(
        "retention: worked_roots={WORKED_ROOTS} baseline_threads={baseline_threads} \
         evicted_threads={evicted_threads} heap_baseline={baseline_heap:?} \
         heap_evicted={evicted_heap:?} heap_dropped={dropped_heap:?} \
         retained_per_evicted_root={per_root:?}"
    );
    assert!(
        evicted_threads <= baseline_threads + 1,
        "{WORKED_ROOTS} evicted roots kept {} thread(s): {:?}",
        evicted_threads.saturating_sub(baseline_threads),
        thread_names()
    );
    if let Some(per_root) = per_root {
        assert!(
            per_root < 256 * 1024,
            "an evicted root still holds {per_root} heap bytes"
        );
    }
}
