use super::*;
use crate::lsp::applicability_cache::ApplicabilityCache;
use crate::lsp::roots::RootPathMode;

fn config(root: &Path) -> Config {
    Config {
        project_root: Some(root.to_path_buf()),
        ..Config::default()
    }
}

fn scan(root: &Path, config: &Config) -> ApplicabilityWalk {
    walk_applicable_area_uncached(
        root,
        None,
        config,
        Instant::now() + Duration::from_secs(30),
        200_000,
        RootPathMode::WalkEntry,
    )
    .unwrap()
}

fn signature(walk: &ApplicabilityWalk) -> Vec<String> {
    let mut result: Vec<_> = walk
        .candidates
        .iter()
        .map(|candidate| {
            format!(
                "{:?} {} {}",
                candidate.key.kind,
                candidate.key.root.display(),
                candidate.source_file.display()
            )
        })
        .chain(walk.markers.iter().map(|(kind, (_, root, marker))| {
            format!("marker {kind:?} {} {marker}", root.display())
        }))
        .collect();
    result.sort();
    result
}

fn measured<T>(f: impl FnOnce() -> T) -> (T, Vec<PathBuf>, Duration) {
    crate::inspect::job::CANONICALIZE_CALLS.with(|calls| *calls.borrow_mut() = Some(Vec::new()));
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed();
    let calls =
        crate::inspect::job::CANONICALIZE_CALLS.with(|calls| calls.borrow_mut().take().unwrap());
    (result, calls, elapsed)
}

#[test]
fn applicability_walk_plain_files_have_no_per_file_realpath() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
    std::fs::write(root.join("package.json"), "{}").unwrap();
    std::fs::write(root.join("biome.json"), r#"{"files":{"includes":["**"]}}"#).unwrap();
    for extension in ["rs", "ts", "js", "py", "go", "sh", "yaml", "json"] {
        for n in 0..20 {
            std::fs::write(root.join(format!("file{n}.{extension}")), "").unwrap();
        }
    }
    let config = config(&root);
    let (walk, calls, _) = measured(|| walk_applicable_area(&root, None, &config, None).unwrap());
    assert!(!walk.candidates.is_empty());
    assert_eq!(calls, vec![root], "only the walk root may need realpath");
}

#[test]
fn applicability_walk_concurrent_binds_share_one_scan() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::write(root.join("main.py"), "").unwrap();
    let config = config(&root);
    let cache = ApplicabilityCache::default();
    let scans = std::sync::atomic::AtomicUsize::new(0);
    let start = std::sync::Barrier::new(3);
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..2 {
            workers.push(scope.spawn(|| {
                start.wait();
                cache
                    .walk(
                        &root,
                        &config,
                        Instant::now() + Duration::from_secs(10),
                        || {
                            scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Ok(scan(&root, &config))
                        },
                    )
                    .unwrap()
            }));
        }
        start.wait();
        let first = workers.remove(0).join().unwrap();
        let second = workers.remove(0).join().unwrap();
        assert_eq!(signature(&first), signature(&second));
    });
    assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn applicability_walk_manifest_change_invalidates_from_watcher() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::write(root.join("main.go"), "package main").unwrap();
    let config = config(&root);
    let before = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(!before
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
    std::fs::write(root.join("go.mod"), "module example.test/app\n").unwrap();
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config.clone(),
    );
    ctx.lsp_forward_watcher_file_events(&[root.join("go.mod")]);
    let after = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(after
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
}

#[test]
fn applicability_walk_invalidation_returns_uncached_result_to_parked_callers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let config = config(&root);
    let cache = ApplicabilityCache::default();
    let scans = std::sync::atomic::AtomicUsize::new(0);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let (caller, waiter) = std::thread::scope(|scope| {
        let caller = scope.spawn(|| {
            cache.walk(
                &root,
                &config,
                Instant::now() + Duration::from_secs(5),
                || {
                    let attempt = scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let result = scan(&root, &config);
                    if attempt == 0 {
                        started_tx.send(()).unwrap();
                        release_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(5))
                            .unwrap();
                    }
                    Ok(result)
                },
            )
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let waiter = scope.spawn(|| {
            cache.walk(
                &root,
                &config,
                Instant::now() + Duration::from_secs(5),
                || {
                    scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(scan(&root, &config))
                },
            )
        });
        assert!(cache.wait_for_waiter_for_test(Instant::now() + Duration::from_secs(5)));
        std::fs::write(root.join("new.py"), "").unwrap();
        cache.invalidate(&root);
        release_tx.send(()).unwrap();
        (
            caller.join().unwrap().unwrap(),
            waiter.join().unwrap().unwrap(),
        )
    });
    assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(caller.candidates.is_empty());
    assert_eq!(signature(&caller), signature(&waiter));
    let next = cache
        .walk(
            &root,
            &config,
            Instant::now() + Duration::from_secs(5),
            || {
                scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(scan(&root, &config))
            },
        )
        .unwrap();
    assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(next
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Python));
}

#[test]
fn applicability_walk_panic_releases_flight_for_next_caller() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::write(root.join("main.py"), "").unwrap();
    let config = config(&root);
    let cache = ApplicabilityCache::default();
    let scans = std::sync::atomic::AtomicUsize::new(0);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let recovered = std::thread::scope(|scope| {
        let caller = scope.spawn(|| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                cache.walk(
                    &root,
                    &config,
                    Instant::now() + Duration::from_secs(5),
                    || {
                        scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        started_tx.send(()).unwrap();
                        release_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(5))
                            .unwrap();
                        panic!("injected scan panic");
                    },
                )
            }))
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let waiter = scope.spawn(|| {
            cache.walk(
                &root,
                &config,
                Instant::now() + Duration::from_secs(2),
                || {
                    scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(scan(&root, &config))
                },
            )
        });
        assert!(cache.wait_for_waiter_for_test(Instant::now() + Duration::from_secs(5)));
        release_tx.send(()).unwrap();
        assert!(caller.join().unwrap().is_err());
        waiter
            .join()
            .unwrap()
            .expect("panic must release the parked caller, not leave it waiting for its deadline")
    });
    let next = cache
        .walk(
            &root,
            &config,
            Instant::now() + Duration::from_secs(2),
            || panic!("the successful recovery scan should be cached"),
        )
        .unwrap();
    assert_eq!(signature(&next), signature(&recovered));
    assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn applicability_walk_entry_budget_reports_incomplete() {
    let temp = tempfile::tempdir().unwrap();
    for n in 0..10 {
        std::fs::write(temp.path().join(format!("{n}.txt")), "").unwrap();
    }
    let error = walk_applicable_area_uncached(
        temp.path(),
        None,
        &config(temp.path()),
        Instant::now() + Duration::from_secs(10),
        3,
        RootPathMode::WalkEntry,
    )
    .unwrap_err();
    assert!(
        matches!(error, ApplicabilityResolutionError::RootUnreadable { reason, .. } if reason.contains("incomplete: entry limit 3"))
    );
}

#[test]
fn applicability_walk_deadline_is_checked_inside_iterator() {
    let temp = tempfile::tempdir().unwrap();
    let result = walk_applicable_area_uncached(
        temp.path(),
        None,
        &config(temp.path()),
        Instant::now() - Duration::from_secs(1),
        200_000,
        RootPathMode::WalkEntry,
    );
    assert!(matches!(
        result,
        Err(ApplicabilityResolutionError::RequestDeadline { .. })
    ));
}

#[cfg(unix)]
#[test]
fn applicability_walk_matches_legacy_nested_workspaces_and_outside_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.join("crates/member/src")).unwrap();
    std::fs::create_dir_all(root.join("nested/src")).unwrap();
    std::fs::create_dir_all(root.join("python/deep")).unwrap();
    for (file, contents) in [
        (
            "Cargo.toml",
            "[workspace]\nmembers = ['crates/member']\nexclude = ['nested']\n",
        ),
        (
            "crates/member/Cargo.toml",
            "[package]\nname = 'member'\nversion = '0.1.0'",
        ),
        ("crates/member/src/lib.rs", ""),
        ("nested/Cargo.toml", "[workspace]\nmembers = []"),
        ("nested/src/lib.rs", ""),
        ("package.json", "{}"),
        ("main.ts", ""),
        ("python/pyproject.toml", "[tool.pyright]"),
        ("python/deep/requirements.txt", ""),
        ("python/deep/main.py", ""),
    ] {
        std::fs::write(root.join(file), contents).unwrap();
    }
    std::fs::write(outside.path().join("go.mod"), "module outside").unwrap();
    std::fs::write(outside.path().join("outside.go"), "").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("outside")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("outside.go"), root.join("outside.go")).unwrap();
    let config = config(&root);
    let old = legacy_walk(&root, &config);
    let new = scan(&root, &config);
    assert_eq!(signature(&old), signature(&new));
    assert!(new
        .candidates
        .iter()
        .any(|c| c.key.root == root && c.key.kind == ServerKind::Rust));
    assert!(!new.candidates.iter().any(|c| c.key.kind == ServerKind::Go));
}

// Frozen pre-optimization algorithm: keep the independent traversal and public
// root-resolution APIs so parity checks do not compare the fast path to itself.
fn legacy_walk(project_root: &Path, config: &Config) -> ApplicabilityWalk {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    let mut markers = HashMap::new();
    let mut builder = ignore::WalkBuilder::new(project_root);
    builder.same_file_system(true).standard_filters(true);
    let walker = crate::context::apply_project_ignore_rules(&mut builder, project_root)
        .filter_entry(|entry| {
            !crate::lsp::roots::skip_in_server_walk(
                entry.file_name().to_string_lossy().as_ref(),
                entry.depth(),
            )
        })
        .build();
    for entry in walker {
        let entry = entry.unwrap();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let file = entry.path();
        for definition in servers_with_root_marker(file, config) {
            if markers.contains_key(&definition.kind) {
                continue;
            }
            let directory = file.parent().unwrap_or(project_root).to_path_buf();
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            markers.insert(definition.kind.clone(), (definition, directory, name));
        }
        for definition in servers_for_file(file, config) {
            let Some(key) = server_key_for_definition(&definition, file, config) else {
                continue;
            };
            if !seen.insert(key.clone()) {
                continue;
            }
            candidates.push(ApplicableServerCandidate {
                key,
                definition,
                source_file: file.into(),
            });
        }
    }
    ApplicabilityWalk {
        candidates,
        markers,
    }
}

#[test]
#[ignore = "copies the repository for a before/after measurement"]
fn applicability_walk_repository_measurement() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut copied = 0;
    for entry in ignore::WalkBuilder::new(source).build() {
        let entry = entry.unwrap();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let dest = root.join(entry.path().strip_prefix(source).unwrap());
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(entry.path(), dest).unwrap();
        copied += 1;
    }
    let config = config(&root);
    super::super::typescript_project::clear_typescript_selection();
    let (old, old_calls, old_time) = measured(|| legacy_walk(&root, &config));
    super::super::typescript_project::clear_typescript_selection();
    let (new, new_calls, new_time) =
        measured(|| walk_applicable_area(&root, None, &config, None).unwrap());
    let (_, cached_calls, cached_time) =
        measured(|| walk_applicable_area(&root, None, &config, None).unwrap());
    println!("copied_files={copied} legacy: canonicalize={} elapsed={old_time:?}; lexical: canonicalize={} elapsed={new_time:?}; cached: canonicalize={} elapsed={cached_time:?}", old_calls.len(), new_calls.len(), cached_calls.len());
    assert_eq!(signature(&old), signature(&new));
    assert_eq!(new_calls, vec![root]);
}
