use super::*;
use crate::lsp::applicability_cache::{ApplicabilityCache, MAX_CACHED_SNAPSHOTS};
use crate::lsp::registry::servers_with_root_marker;
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
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
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
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
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
fn applicability_walk_rechecks_ancestor_marker_without_watcher_event() {
    let temp = tempfile::tempdir().unwrap();
    let parent = crate::inspect::job::canonicalize_normalized(temp.path());
    let root = parent.join("subdir");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("main.go"), "package main").unwrap();
    let config = config(&root);
    let before = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(!before
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));

    // This marker is outside the watched project, so no watcher event arrives.
    std::fs::write(parent.join("go.mod"), "module example.test/app\n").unwrap();
    let after = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(after
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
    std::fs::remove_file(parent.join("go.mod")).unwrap();
    let removed = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(!removed
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
}

#[test]
fn applicability_walk_equivalent_config_sets_share_one_scan() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let configs: Vec<_> = (0..8)
        .map(|rotation| {
            let mut config = config(&root);
            for offset in 0..16 {
                config
                    .disabled_lsp
                    .insert(format!("custom-{}", (offset + rotation) % 16));
                config
                    .lsp_auto_install_binaries
                    .insert(format!("binary-{}", (offset + rotation) % 16));
                config
                    .lsp_inflight_installs
                    .insert(format!("install-{}", (offset + rotation) % 16));
            }
            config
        })
        .collect();
    let first = serde_json::to_vec(&configs[0]).unwrap();
    assert!(
        configs
            .iter()
            .skip(1)
            .any(|config| serde_json::to_vec(config).unwrap() != first),
        "fixture must exercise different HashSet serialization orders"
    );
    let cache = ApplicabilityCache::default();
    let mut scans = 0;
    for config in &configs {
        cache
            .walk(
                &root,
                config,
                Instant::now() + Duration::from_secs(5),
                || {
                    scans += 1;
                    Ok(scan(&root, config))
                },
            )
            .unwrap();
    }
    assert_eq!(scans, 1, "equivalent set values must share a cache entry");
}

#[test]
fn applicability_walk_keeps_ordered_server_arguments_in_key() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let mut first = config(&root);
    first.lsp_servers.push(crate::config::UserServerDef {
        id: "custom".into(),
        args: vec!["first".into(), "second".into()],
        ..Default::default()
    });
    let mut second = first.clone();
    second.lsp_servers[0].args.reverse();
    let cache = ApplicabilityCache::default();
    let mut scans = 0;
    for config in [&first, &second] {
        cache
            .walk(
                &root,
                config,
                Instant::now() + Duration::from_secs(5),
                || {
                    scans += 1;
                    Ok(scan(&root, config))
                },
            )
            .unwrap();
    }
    assert_eq!(scans, 2, "server argument order changes the snapshot");
}

#[test]
fn applicability_walk_rechecks_ancestor_biome_contents_and_identity() {
    let temp = tempfile::tempdir().unwrap();
    let parent = crate::inspect::job::canonicalize_normalized(temp.path());
    let root = parent.join("subdir");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("main.json"), "{}").unwrap();
    let marker = parent.join("biome.json");
    std::fs::write(&marker, r#"{"files":{"includes":["**"]}}"#).unwrap();
    let config = config(&root);
    let before = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(before
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Biome));

    // Preserve mtime across replacement: identity must still invalidate both
    // the walk result and the mtime-based Biome parse memo.
    let modified =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&marker).unwrap());
    std::fs::rename(&marker, parent.join("old-biome.json")).unwrap();
    std::fs::write(&marker, r#"{"files":{"includes":["**","!**"]}}"#).unwrap();
    filetime::set_file_mtime(&marker, modified).unwrap();
    let after = walk_applicable_area(&root, None, &config, None).unwrap();
    assert!(!after
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Biome));
}

#[test]
fn applicability_walk_manifest_change_invalidates_from_watcher() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
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
fn applicability_walk_fifty_content_batches_do_zero_full_walks() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let source = root.join("main.py");
    std::fs::write(&source, "print(0)").unwrap();
    let config = config(&root);
    let cache = ApplicabilityCache::default();
    let walk = || {
        cache
            .walk(
                &root,
                &config,
                Instant::now() + Duration::from_secs(10),
                || Ok(scan(&root, &config)),
            )
            .unwrap()
    };
    let before = signature(&walk());
    let baseline = cache.metrics(&root).full_walks;
    for n in 1..=50 {
        std::fs::write(&source, format!("print({n})")).unwrap();
        cache.invalidate_batch(
            &root,
            std::slice::from_ref(&source),
            crate::watcher_filter::watcher_event_is_content_only(&notify::EventKind::Modify(
                notify::event::ModifyKind::Data(notify::event::DataChange::Content),
            )),
        );
        assert_eq!(signature(&walk()), before);
    }
    let metrics = cache.metrics(&root);
    assert_eq!(
        metrics.full_walks - baseline,
        0,
        "content edits must not walk"
    );
    assert_eq!(metrics.hits, 50);
    assert_eq!(metrics.retained_content_batches, 50);
    assert_eq!(metrics.warm_validations, 50);
    assert!(metrics.invalidations.is_empty());
}

#[test]
fn applicability_walk_cache_cap_keeps_recently_used_snapshots() {
    // The product hands the cache and the walk roots in
    // `canonicalize_normalized` form (see `walk_applicable_area`); a bare
    // `fs::canonicalize` gives a verbatim `\\?\C:\...` root on Windows,
    // which no production caller passes.
    let warm = tempfile::tempdir().unwrap();
    let warm_root = crate::inspect::job::canonicalize_normalized(warm.path());
    std::fs::write(warm_root.join("main.py"), "").unwrap();
    let warm_config = config(&warm_root);
    let other = tempfile::tempdir().unwrap();
    let other_root = crate::inspect::job::canonicalize_normalized(other.path());
    // Each configuration is its own cache key for the same root.
    let other_config = |n: usize| {
        let mut config = config(&other_root);
        config.disabled_lsp.insert(format!("custom-{n}"));
        config
    };
    let cache = ApplicabilityCache::default();
    let walk = |root: &Path, config: &Config| {
        cache
            .walk(
                root,
                config,
                Instant::now() + Duration::from_secs(10),
                || Ok(scan(root, config)),
            )
            .unwrap();
    };
    walk(&warm_root, &warm_config);
    // Fill the cache to capacity with idle snapshots of other keys.
    for n in 0..MAX_CACHED_SNAPSHOTS - 1 {
        walk(&other_root, &other_config(n));
    }
    // At capacity, a lookup must still find its own warm snapshot.
    walk(&warm_root, &warm_config);
    // A new key at capacity makes room by dropping the least recently used
    // idle snapshot, not one a caller has just used.
    walk(&other_root, &other_config(MAX_CACHED_SNAPSHOTS - 1));
    walk(&warm_root, &warm_config);
    let warm_metrics = cache.metrics(&warm_root);
    assert_eq!(
        warm_metrics.full_walks, 1,
        "a recently used snapshot must survive the cache cap: {warm_metrics:?}"
    );
    assert_eq!(warm_metrics.hits, 2);

    // The cap still bounds the cache: the oldest key was the one dropped,
    // and a key used since then is still warm.
    let other_walks = cache.metrics(&other_root).full_walks;
    walk(&other_root, &other_config(0));
    assert_eq!(cache.metrics(&other_root).full_walks, other_walks + 1);
    walk(&other_root, &other_config(MAX_CACHED_SNAPSHOTS - 2));
    assert_eq!(cache.metrics(&other_root).full_walks, other_walks + 1);
}

#[test]
fn applicability_walk_config_change_keeps_active_content_cache() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let source = root.join("main.py");
    std::fs::write(&source, "print(0)").unwrap();
    let base_config = config(&root);
    let mut unused_config = base_config.clone();
    unused_config
        .disabled_lsp
        .insert("unused-server".to_owned());
    let mut active_config = base_config;
    active_config
        .disabled_lsp
        .insert("active-server".to_owned());
    let cache = ApplicabilityCache::default();
    let walk = |config: &Config| {
        cache
            .walk(
                &root,
                config,
                Instant::now() + Duration::from_secs(10),
                || Ok(scan(&root, config)),
            )
            .unwrap()
    };

    walk(&unused_config);
    walk(&active_config);
    let before_config_invalidation = cache.metrics(&root).full_walks;
    cache.invalidate(
        &root,
        crate::lsp::applicability_cache::InvalidationCause::SelectionInput,
    );
    walk(&active_config);
    let baseline = cache.metrics(&root).full_walks;
    assert_eq!(
        baseline,
        before_config_invalidation + 1,
        "the active config must rebuild"
    );

    for n in 1..=20 {
        std::fs::write(&source, format!("print({n})")).unwrap();
        cache.invalidate_batch(
            &root,
            std::slice::from_ref(&source),
            crate::watcher_filter::watcher_event_is_content_only(&notify::EventKind::Modify(
                notify::event::ModifyKind::Data(notify::event::DataChange::Content),
            )),
        );
        walk(&active_config);
    }

    let metrics = cache.metrics(&root);
    assert_eq!(
        metrics.full_walks - baseline,
        0,
        "content edits must not walk"
    );
    assert_eq!(metrics.retained_content_batches, 20);
    assert_eq!(metrics.warm_validations, 20);
}

struct WatcherWalkFixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    config: Config,
    cache: ApplicabilityCache,
}

impl WatcherWalkFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("main.py"), "print(0)").unwrap();
        std::fs::write(root.join("go.mod"), "module example.test/app\n").unwrap();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join(".gitignore"), "# initial rules\n").unwrap();
        let mut config = config(&root);
        // Biome may probe ancestor directories outside this checkout. Other
        // parallel tempfile fixtures change those directories' metadata; this
        // fixture tests watcher invalidation, not ancestor revalidation.
        config.disabled_lsp.insert("biome".into());
        Self {
            _temp: temp,
            root,
            config,
            cache: ApplicabilityCache::default(),
        }
    }

    fn walk(&self) -> ApplicabilityWalk {
        self.cache
            .walk(
                &self.root,
                &self.config,
                Instant::now() + Duration::from_secs(10),
                || Ok(scan(&self.root, &self.config)),
            )
            .unwrap()
    }

    fn batch(&self, paths: &[PathBuf], kind: notify::EventKind) {
        self.cache.invalidate_batch(
            &self.root,
            paths,
            crate::watcher_filter::watcher_event_is_content_only(&kind),
        );
    }

    fn assert_one_new_walk(&self) -> ApplicabilityWalk {
        let result = self.walk();
        assert_eq!(self.cache.metrics(&self.root).full_walks, 2);
        result
    }
}

fn data_modify() -> notify::EventKind {
    notify::EventKind::Modify(notify::event::ModifyKind::Data(
        notify::event::DataChange::Content,
    ))
}

#[test]
fn applicability_walk_new_source_in_fresh_directory_invalidates() {
    let fixture = WatcherWalkFixture::new();
    assert!(!fixture
        .walk()
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
    let source = fixture.root.join("fresh/deep/main.go");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "package main").unwrap();
    fixture.batch(
        &[source],
        notify::EventKind::Create(notify::event::CreateKind::File),
    );
    assert!(fixture
        .assert_one_new_walk()
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Go));
    assert_eq!(
        fixture.cache.metrics(&fixture.root).invalidations["create_remove_rename_or_directory"],
        1
    );
}

#[test]
fn applicability_walk_deleted_source_invalidates() {
    let fixture = WatcherWalkFixture::new();
    assert!(!fixture.walk().candidates.is_empty());
    let source = fixture.root.join("main.py");
    std::fs::remove_file(&source).unwrap();
    fixture.batch(
        &[source],
        notify::EventKind::Remove(notify::event::RemoveKind::File),
    );
    assert!(!fixture
        .assert_one_new_walk()
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Python));
}

#[test]
fn applicability_walk_new_tsconfig_invalidates() {
    let fixture = WatcherWalkFixture::new();
    std::fs::write(fixture.root.join("main.ts"), "export const a = 1;").unwrap();
    fixture.walk();
    let config = fixture.root.join("tsconfig.json");
    std::fs::write(&config, "{}").unwrap();
    fixture.batch(
        &[config],
        notify::EventKind::Create(notify::event::CreateKind::File),
    );
    fixture.assert_one_new_walk();
}

#[test]
fn applicability_walk_package_json_content_invalidates() {
    let fixture = WatcherWalkFixture::new();
    fixture.walk();
    let manifest = fixture.root.join("package.json");
    std::fs::write(&manifest, r#"{"type":"module"}"#).unwrap();
    fixture.batch(&[manifest], data_modify());
    fixture.assert_one_new_walk();
    assert_eq!(
        fixture.cache.metrics(&fixture.root).invalidations["marker_ignore_config_or_ts_install"],
        1
    );
}

#[test]
fn applicability_walk_gitignore_content_invalidates() {
    let fixture = WatcherWalkFixture::new();
    fixture.walk();
    let ignore = fixture.root.join(".gitignore");
    std::fs::write(&ignore, "main.py\n").unwrap();
    fixture.batch(&[ignore], data_modify());
    let result = fixture.assert_one_new_walk();
    assert!(!result
        .candidates
        .iter()
        .any(|c| c.key.kind == ServerKind::Python));
}

#[test]
fn applicability_walk_overflow_invalidates() {
    let fixture = WatcherWalkFixture::new();
    fixture.walk();
    fixture
        .cache
        .invalidate(&fixture.root, InvalidationCause::OverflowLostEvents);
    fixture.assert_one_new_walk();
    assert_eq!(
        fixture.cache.metrics(&fixture.root).invalidations["overflow_or_lost_events"],
        1
    );
}

#[test]
fn applicability_walk_rename_invalidates() {
    let fixture = WatcherWalkFixture::new();
    fixture.walk();
    let before = fixture.root.join("main.py");
    let after = fixture.root.join("renamed.py");
    std::fs::rename(&before, &after).unwrap();
    fixture.batch(
        &[before, after.clone()],
        notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both,
        )),
    );
    let result = fixture.assert_one_new_walk();
    assert!(result.candidates.iter().any(|c| c.source_file == after));
}

#[test]
fn applicability_walk_custom_marker_and_captured_ignore_content_invalidate() {
    let mut fixture = WatcherWalkFixture::new();
    fixture
        .config
        .lsp_servers
        .push(crate::config::UserServerDef {
            id: "custom".into(),
            root_markers: vec!["project.anchor".into()],
            ..Default::default()
        });
    let marker = fixture.root.join("project.anchor");
    let ignore = fixture.root.join(".ignore");
    std::fs::write(&marker, "before").unwrap();
    std::fs::write(&ignore, "# before").unwrap();
    fixture.walk();
    std::fs::write(&marker, "after").unwrap();
    fixture.batch(&[marker], data_modify());
    fixture.assert_one_new_walk();
    std::fs::write(&ignore, "# after").unwrap();
    fixture.batch(&[ignore], data_modify());
    fixture.walk();
    let metrics = fixture.cache.metrics(&fixture.root);
    assert_eq!(metrics.full_walks, 3);
    assert_eq!(
        metrics.invalidations["marker_ignore_config_or_ts_install"],
        2
    );
    assert!(metrics.dependency_count > 0);
}

/// The cache may be handed a root spelled differently from the normalized
/// paths its probes record: on Windows, `fs::canonicalize` returns a `\\?\`
/// verbatim path. Watcher paths built from that root must still be recognised
/// as selection inputs. A `..` component reproduces the mismatch everywhere.
#[test]
fn applicability_walk_selection_inputs_match_across_root_spellings() {
    let mut fixture = WatcherWalkFixture::new();
    std::fs::create_dir(fixture.root.join("nested")).unwrap();
    fixture.root = fixture.root.join("nested").join("..");
    let ignore = fixture.root.join(".ignore");
    std::fs::write(&ignore, "# before").unwrap();
    fixture.walk();
    std::fs::write(&ignore, "# after").unwrap();
    fixture.batch(&[ignore], data_modify());
    fixture.assert_one_new_walk();
    assert_eq!(
        fixture.cache.metrics(&fixture.root).invalidations["marker_ignore_config_or_ts_install"],
        1
    );
}

#[test]
fn applicability_walk_directory_and_ambiguous_modify_invalidate() {
    for (name, kind) in [
        ("folder", data_modify()),
        (
            "main.py",
            notify::EventKind::Modify(notify::event::ModifyKind::Any),
        ),
    ] {
        let fixture = WatcherWalkFixture::new();
        fixture.walk();
        let path = fixture.root.join(name);
        if name == "folder" {
            std::fs::create_dir(&path).unwrap();
        }
        fixture.batch(&[path], kind);
        fixture.assert_one_new_walk();
    }
}

#[cfg(unix)]
#[test]
fn applicability_walk_symlinked_selection_input_content_invalidates() {
    let fixture = WatcherWalkFixture::new();
    let target = fixture.root.join("workspace.settings");
    std::fs::write(&target, "[workspace]\nmembers = []\n").unwrap();
    std::os::unix::fs::symlink(&target, fixture.root.join("Cargo.toml")).unwrap();
    std::fs::write(fixture.root.join("lib.rs"), "pub fn example() {}\n").unwrap();
    fixture.walk();
    std::fs::write(&target, "[workspace]\nmembers = []\nresolver = '2'\n").unwrap();
    // The watcher resolves the symlink, so its event names the target rather
    // than Cargo.toml. The captured input's identity must still invalidate it.
    fixture.batch(&[target], data_modify());
    fixture.assert_one_new_walk();
    assert_eq!(
        fixture.cache.metrics(&fixture.root).invalidations["marker_ignore_config_or_ts_install"],
        1
    );
}

#[test]
fn applicability_walk_invalidation_returns_uncached_result_to_parked_callers() {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
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
        cache.invalidate(&root, InvalidationCause::CreateRemoveRename);
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
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
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
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
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
    legacy_scoped_walk(project_root, config, None)
}

fn legacy_scoped_walk(
    project_root: &Path,
    config: &Config,
    scopes: Option<&[PathBuf]>,
) -> ApplicabilityWalk {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    let mut markers = HashMap::new();
    let roots = scopes.unwrap_or_default();
    let mut builder =
        ignore::WalkBuilder::new(roots.first().map_or(project_root, PathBuf::as_path));
    for root in roots.iter().skip(1) {
        builder.add(root);
    }
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

fn write_mixed_registry_fixture(root: &Path) {
    for (name, contents) in [
        (
            "Cargo.toml",
            "[workspace]\nmembers=['crates/member']\nexclude=['nested']",
        ),
        (
            "crates/member/Cargo.toml",
            "[package]\nname='member'\nversion='0.1.0'",
        ),
        ("crates/member/src/lib.rs", ""),
        ("nested/Cargo.toml", "[workspace]\nmembers=[]"),
        ("nested/src/lib.rs", ""),
        ("package.json", "{}"),
        (
            "biome.json",
            r#"{"files":{"includes":["**/*.ts","!**/skip.ts"]}}"#,
        ),
        ("src/keep.ts", ""),
        ("src/skip.ts", ""),
        ("src/data.json", "{}"),
        ("src/UPPER.TS", ""),
        ("next/package.json", "{}"),
        (
            "next/node_modules/typescript/package.json",
            r#"{"name":"typescript","version":"7.0.2"}"#,
        ),
        ("next/src/index.ts", ""),
        ("old/package.json", "{}"),
        (
            "old/node_modules/typescript/package.json",
            r#"{"name":"typescript","version":"5.9.3"}"#,
        ),
        ("old/src/index.ts", ""),
        ("python/pyproject.toml", "[tool.pyright]"),
        ("python/deep/requirements.txt", ""),
        ("python/deep/helper.py", ""),
        ("scripts/helper.py", ""),
        ("next/scripts/run.sh", ""),
        ("next/data.yaml", ""),
        ("custom/marker.root", ""),
        ("custom/main.xyz", ""),
        ("custom/other.XYZ", ""),
    ] {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

fn mixed_registry_config(root: &Path, overrides: bool) -> Config {
    let mut config = config(root);
    config.experimental_lsp_ty = true;
    if overrides {
        use crate::config::UserServerDef;
        config.lsp_servers = vec![
            UserServerDef {
                id: "custom".into(),
                binary: "custom-lsp".into(),
                extensions: vec!["xyz".into(), "XYZ".into()],
                root_markers: vec!["marker.root".into()],
                ..Default::default()
            },
            UserServerDef {
                id: "typescript".into(),
                args: vec!["--stdio".into()],
                initialization_options: Some(
                    serde_json::json!({"preferences":{"quotePreference":"single"}}),
                ),
                ..Default::default()
            },
            UserServerDef {
                id: "biome".into(),
                root_markers: vec!["package.json".into()],
                ..Default::default()
            },
            UserServerDef {
                id: "go".into(),
                disabled: true,
                ..Default::default()
            },
        ];
    }
    config
}

#[test]
fn applicability_walk_mixed_registry_matches_legacy_byte_for_byte() {
    let temp = tempfile::tempdir().unwrap();
    // The uncached walk keeps its root's spelling, so hand it the normalized
    // root `walk_applicable_area` would. A bare `canonicalize` is `\\?\` on
    // Windows; the entry-level test below covers that spelling.
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    write_mixed_registry_fixture(&root);
    for overrides in [false, true] {
        let config = mixed_registry_config(&root, overrides);
        for scopes in [
            None,
            Some(vec![root.join("src/skip.ts")]),
            Some(vec![root.join("src/keep.ts")]),
            Some(vec![root.join("next"), root.join("custom")]),
        ] {
            let old = legacy_scoped_walk(&root, &config, scopes.as_deref());
            let new = walk_applicable_area_uncached(
                &root,
                scopes.as_deref(),
                &config,
                Instant::now() + Duration::from_secs(30),
                200_000,
                RootPathMode::WalkEntry,
            )
            .unwrap();
            let candidates = |walk: ApplicabilityWalk| {
                walk.candidates
                    .into_iter()
                    .map(|c| (c.key, c.definition, c.source_file))
                    .collect::<Vec<_>>()
            };
            assert_eq!(new.markers, old.markers);
            assert_eq!(candidates(new), candidates(old));
        }
    }
}

/// Language servers are keyed and started by the roots the production entry
/// returns. Hand the entry the spelling a bare `fs::canonicalize` gives (`\\?\C:\...`
/// on Windows) and require the server keys legacy per-file resolution derives
/// from the same root, none of them verbatim. Source files and marker
/// directories keep each walk's own spelling, so only keys are compared.
#[test]
fn applicability_walk_entry_keys_match_legacy_for_canonicalized_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    write_mixed_registry_fixture(&root);
    for overrides in [false, true] {
        let config = mixed_registry_config(&root, overrides);
        for scopes in [None, Some(vec![root.join("next"), root.join("custom")])] {
            let old = legacy_scoped_walk(&root, &config, scopes.as_deref());
            let new = walk_applicable_area(
                &root,
                scopes.as_deref(),
                &config,
                Some(Instant::now() + Duration::from_secs(30)),
            )
            .unwrap();
            let keys = |walk: &ApplicabilityWalk| {
                walk.candidates
                    .iter()
                    .map(|c| (c.key.clone(), c.definition.clone()))
                    .collect::<Vec<_>>()
            };
            assert!(!new.candidates.is_empty());
            assert_eq!(keys(&new), keys(&old));
            for candidate in &new.candidates {
                assert!(
                    !is_windows_verbatim(&candidate.key.root),
                    "verbatim server root {}",
                    candidate.key.root.display()
                );
            }
        }
    }
}

fn cost_fixture(files_per_directory: usize, directories: usize) -> (tempfile::TempDir, Config) {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    std::fs::write(root.join("package.json"), "{}").unwrap();
    std::fs::write(root.join("biome.json"), r#"{"files":{"includes":["**"]}}"#).unwrap();
    for directory in 0..directories {
        let dir = root.join(format!("packages/pkg{directory}/src"));
        std::fs::create_dir_all(&dir).unwrap();
        for file in 0..files_per_directory {
            let extension = ["ts", "json", "py", "sh", "yaml", "rs", "go", "txt"][file % 8];
            std::fs::write(dir.join(format!("file{file}.{extension}")), "").unwrap();
        }
    }
    let config = config(&root);
    (temp, config)
}

#[test]
fn applicability_walk_sibling_cost_does_not_rebuild_definitions_or_roots() {
    let (temp, config) = cost_fixture(512, 1);
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
    let (walk, allocations) = crate::test_allocations::count(|| scan(&root, &config));
    println!("sibling_files=512 allocations={allocations}");
    assert!(!walk.candidates.is_empty());
    assert!(
        allocations < 40_000,
        "per-file server discovery allocated {allocations} times"
    );
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
    let root = crate::inspect::job::canonicalize_normalized(temp.path());
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
