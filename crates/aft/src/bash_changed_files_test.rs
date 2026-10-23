use super::*;
use notify::event::{CreateKind, ModifyKind, RemoveKind};
use std::process::Command;

fn filesystem_activity_lease() -> std::sync::MutexGuard<'static, ()> {
    // Keep quiet-project measurements separate from other large fixtures. The
    // saturation regression still runs its own concurrent 50k-file writer.
    static ACTIVITY: Mutex<()> = Mutex::new(());
    ACTIVITY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap()
        .success());
}
fn repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    git(
        root.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(root.path(), &["config", "user.name", "Test"]);
    std::fs::write(root.path().join("clean.txt"), "old\n").unwrap();
    std::fs::write(root.path().join("deleted.txt"), "gone\n").unwrap();
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "initial"]);
    root
}
fn event(journal: &Journal, root: &Path, name: &str, kind: notify::EventKind) {
    journal.event(&notify::Event::new(kind).add_path(root.join(name)));
}
fn capture(root: &Path, mode: BashChangedFiles) -> (Arc<Journal>, Arc<Capture>) {
    let journal = Arc::new(Journal::default());
    let capture = Capture::begin(root.to_path_buf(), journal.clone(), mode, true);
    (journal, capture)
}
#[test]
fn bash_changed_files_list_and_diff_in_real_git_repo() {
    let root = repo();
    for mode in [BashChangedFiles::List, BashChangedFiles::Diff] {
        let (journal, capture) = capture(root.path(), mode);
        std::fs::write(root.path().join("clean.txt"), "new\n").unwrap();
        event(
            &journal,
            root.path(),
            "clean.txt",
            notify::EventKind::Modify(ModifyKind::Any),
        );
        let report = capture.finish();
        assert_eq!(report.files[0].path, "clean.txt");
        assert_eq!(report.files[0].status, "modified");
        assert!(report.text.starts_with(HEADER));
        if mode == BashChangedFiles::Diff {
            let diff = report.files[0].diff.as_deref().unwrap();
            assert!(diff.contains("-old\n+new"), "{diff}");
        } else {
            assert!(report.files[0].diff.is_none());
        }
        git(root.path(), &["checkout", "--", "clean.txt"]);
    }
}
#[test]
fn bash_changed_files_already_dirty_and_existing_untracked_have_no_diff() {
    let root = repo();
    std::fs::write(root.path().join("clean.txt"), "dirty before\n").unwrap();
    std::fs::write(root.path().join("untracked.txt"), "before\n").unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    for name in ["clean.txt", "untracked.txt"] {
        std::fs::write(root.path().join(name), "after\n").unwrap();
        event(
            &journal,
            root.path(),
            name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.finish();
    assert_eq!(report.files.len(), 2);
    assert!(report
        .files
        .iter()
        .all(|file| file.diff.is_none() && file.reason.as_deref() == Some(DIRTY)));
}
#[test]
fn bash_changed_files_deleted_new_binary_and_large() {
    let root = repo();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    std::fs::remove_file(root.path().join("deleted.txt")).unwrap();
    event(
        &journal,
        root.path(),
        "deleted.txt",
        notify::EventKind::Remove(RemoveKind::File),
    );
    for (name, bytes) in [
        ("new.txt", b"added\n".to_vec()),
        ("binary", vec![0, 1, 255]),
        ("large", vec![b'x'; MAX_FILE_BYTES + 1]),
    ] {
        std::fs::write(root.path().join(name), bytes).unwrap();
        event(
            &journal,
            root.path(),
            name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.finish();
    let find = |name| report.files.iter().find(|file| file.path == name).unwrap();
    assert!(find("deleted.txt").diff.as_ref().unwrap().contains("-gone"));
    assert_eq!(find("deleted.txt").status, "deleted");
    assert!(find("deleted.txt")
        .diff
        .as_ref()
        .unwrap()
        .contains("+++ /dev/null"));
    assert!(find("new.txt").diff.as_ref().unwrap().contains("+added"));
    assert_eq!(find("new.txt").status, "added");
    assert!(find("new.txt")
        .diff
        .as_ref()
        .unwrap()
        .contains("--- /dev/null"));
    assert_eq!(
        find("binary").reason.as_deref(),
        Some("no diff: binary file")
    );
    assert!(find("large").reason.as_ref().unwrap().contains("too large"));
}
#[test]
fn bash_changed_files_caps_files_lines_bytes_and_overflow() {
    let root = repo();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    for i in 0..MAX_FILES + 4 {
        let name = format!("new-{i:03}.txt");
        std::fs::write(root.path().join(&name), "x\n".repeat(500)).unwrap();
        event(
            &journal,
            root.path(),
            &name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    journal.gap();
    let report = capture.finish();
    assert!(report.files.len() <= MAX_FILES);
    assert!(report.text.len() <= MAX_REPORT_BYTES);
    assert!(serde_json::to_vec(report).unwrap().len() <= MAX_REPORT_BYTES);
    assert!(report.text.contains("shown 200 of"));
    assert!(report.text.contains(OVERFLOW));
    assert_eq!(report.changed_files_list_envelope.reason, Some(Reason::Cap));
    assert!(report.changed_files_list_envelope.total.is_at_least());
    assert!(report
        .files
        .iter()
        .all(|file| file.diff_lines_list_envelope.as_ref().unwrap().shown == MAX_DIFF_LINES));
}
#[test]
fn bash_changed_files_ring_overrun_and_outside_git_are_honest() {
    let root = tempfile::tempdir().unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    for _ in 0..MAX_EVENTS + 2 {
        event(
            &journal,
            root.path(),
            "note.txt",
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.finish();
    assert!(report.text.contains(OVERFLOW));
    assert!(report.text.contains(OUTSIDE_GIT));
    assert!(report.files[0].diff.is_none());
    assert_eq!(capture.finish().text, report.text);
}
#[test]
fn bash_changed_files_off_does_no_probes_or_watcher_registration() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.path().to_path_buf());
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    assert_eq!(ctx.config().bash.changed_files, BashChangedFiles::Off);
    assert!(Capture::start(&ctx).is_none());
    assert!(ctx.watcher_counters().bash_changes.get().is_none());
}
#[test]
fn bash_changed_files_50k_fixture_start_latency() {
    let _activity = filesystem_activity_lease();
    let root = repo();
    let many = root.path().join("many");
    std::fs::create_dir(&many).unwrap();
    for i in 0..50_000 {
        std::fs::write(many.join(format!("file-{i}")), "fixture\n").unwrap();
    }
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "50k files"]);
    let start = Instant::now();
    let (_, list) = capture(root.path(), BashChangedFiles::List);
    let list_elapsed = start.elapsed();
    let start = Instant::now();
    let (_, diff) = capture(root.path(), BashChangedFiles::Diff);
    let diff_elapsed = start.elapsed();
    eprintln!("bash changed-files 50k fixture: list={list_elapsed:?}, diff={diff_elapsed:?}, git_available={}", diff.git.is_some());
    assert!(list_elapsed < Duration::from_millis(100));
    assert!(diff_elapsed < Duration::from_secs(1));
    assert!(list.git.is_none());
}

#[test]
fn bash_changed_files_sealed_window_excludes_later_events_and_overflow() {
    let root = tempfile::tempdir().unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::List);
    event(
        &journal,
        root.path(),
        "during.txt",
        notify::EventKind::Create(CreateKind::File),
    );
    capture.seal();
    event(
        &journal,
        root.path(),
        "after.txt",
        notify::EventKind::Create(CreateKind::File),
    );
    journal.gap();
    let report = capture.finish();
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.files[0].path, "during.txt");
    assert!(!report.text.contains(OVERFLOW));
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
struct NativeWatcher {
    watcher: Option<crate::watcher_backend::ProjectWatcher>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    counters: Arc<crate::context::WatcherCounters>,
    journal: Arc<Journal>,
    markers: Arc<Mutex<Vec<PathBuf>>>,
    generated: Arc<std::sync::atomic::AtomicUsize>,
    generated_ready: std::sync::mpsc::Receiver<()>,
    delay_last_write: Arc<std::sync::atomic::AtomicBool>,
    root: PathBuf,
}
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl NativeWatcher {
    fn new(root: &Path, watch_git: bool) -> Self {
        use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
        let root = root.canonicalize().unwrap();
        let counters = crate::context::watcher_counters_for_root(&root);
        let journal = counters
            .bash_changes
            .get_or_init(|| Arc::new(Journal::default()))
            .clone();
        let mut ignores = ignore::gitignore::GitignoreBuilder::new(&root);
        ignores.add_line(None, "/target/").unwrap();
        let matcher = Arc::new(std::sync::RwLock::new(Some(Arc::new(
            ignores.build().unwrap(),
        ))));
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = crate::watcher_backend::create_project_watcher(
            root.clone(),
            if watch_git {
                vec![root.join(".git")]
            } else {
                vec![]
            },
            tx,
            matcher,
            Arc::new(AtomicU64::new(0)),
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let observed = counters.clone();
        let observed_journal = journal.clone();
        let markers = Arc::new(Mutex::new(Vec::new()));
        let recorded_markers = markers.clone();
        let generated = Arc::new(AtomicUsize::new(0));
        let (generated_tx, generated_ready) = std::sync::mpsc::sync_channel(1);
        let generated_events = generated.clone();
        let delay_last_write = Arc::new(AtomicBool::new(false));
        let delayed_consumer = delay_last_write.clone();
        let join = std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                if let Ok(Ok(event)) = rx.recv_timeout(Duration::from_millis(10)) {
                    if delayed_consumer.load(Ordering::Acquire)
                        && event.paths.iter().any(|path| {
                            path.file_name()
                                .is_some_and(|name| name == "last-write.txt")
                        })
                    {
                        // Simulate delayed intake without dropping native events. A
                        // native flush alone must not stand in for journal delivery.
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    for path in &event.paths {
                        if crate::watcher_backend::flush::is_marker_path(path) {
                            recorded_markers.lock().unwrap().push(path.clone());
                        }
                        if path
                            .components()
                            .any(|part| part.as_os_str() == "generated")
                        {
                            generated_events.fetch_add(1, Ordering::Relaxed);
                            let _ = generated_tx.try_send(());
                        }
                    }
                    if observed.bash_flush.observe(&event) {
                        continue;
                    }
                    if event.need_rescan() {
                        observed_journal.gap();
                    }
                    observed_journal.event(&event);
                }
            }
        });
        Self {
            watcher: Some(watcher),
            stop,
            join: Some(join),
            counters,
            journal,
            markers,
            generated,
            generated_ready,
            delay_last_write,
            root,
        }
    }
    fn capture(&self) -> Arc<Capture> {
        let mut capture = Capture::begin(
            self.root.clone(),
            self.journal.clone(),
            BashChangedFiles::List,
            true,
        );
        Arc::get_mut(&mut capture).unwrap().flush = Some(self.counters.bash_flush.clone());
        capture
    }
    fn ready(&self) {
        // Quiet-project measurements begin after the native service has
        // acknowledged startup. The saturation regression deliberately does not.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.counters.bash_flush.wait(
            &self.root,
            (Instant::now() + crate::watcher_backend::flush::BASH_FLUSH_LIMIT).min(deadline),
        ) {
            assert!(
                Instant::now() < deadline,
                "native watcher did not become ready"
            );
        }
    }
}
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl Drop for NativeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        drop(self.watcher.take());
        self.join.take().unwrap().join().unwrap();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "requires a live FSEvents service; run the focused native gate separately from large fixtures"
)]
fn bash_changed_files_flush_native_catches_exit_write() {
    let _activity = filesystem_activity_lease();
    let root = tempfile::tempdir().unwrap();
    let native = NativeWatcher::new(root.path(), false);
    native.ready();
    for i in 0..5 {
        let capture = native.capture();
        let name = format!("last-write-{i}.txt");
        std::fs::write(root.path().join(&name), "last write before exit\n").unwrap();
        let report = capture.finish();
        assert!(
            report.files.iter().any(|file| file.path == name),
            "{report:?}; root={:?}; journal={:?}",
            native.root,
            native.journal.state.lock().unwrap()
        );
        assert!(report.incomplete.is_empty(), "{report:?}");
        assert!(!report
            .files
            .iter()
            .any(|file| file.path.contains("aft-flush")));
    }
    assert!(native
        .markers
        .lock()
        .unwrap()
        .iter()
        .all(|path| !path.exists()));
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn bash_changed_files_flush_native_git_directory_and_linked_worktree_markers() {
    let root = repo();
    let native = NativeWatcher::new(root.path(), true);
    assert!(native.counters.bash_flush.wait(
        &native.root,
        Instant::now() + crate::watcher_backend::flush::BASH_FLUSH_LIMIT
    ));
    let markers = native.markers.lock().unwrap();
    assert!(!markers.is_empty());
    assert!(markers
        .iter()
        .all(|path| path.parent() == Some(native.root.join(".git").as_path()) && !path.exists()));
    drop(markers);
    drop(native);
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("linked");
    git(
        root.path(),
        &[
            "worktree",
            "add",
            "-qb",
            "flush-linked",
            linked.to_str().unwrap(),
        ],
    );
    assert!(linked.join(".git").is_file());
    let native = NativeWatcher::new(&linked, false);
    assert!(native.counters.bash_flush.wait(
        &native.root,
        Instant::now() + crate::watcher_backend::flush::BASH_FLUSH_LIMIT
    ));
    let markers = native.markers.lock().unwrap();
    assert!(!markers.is_empty());
    assert!(markers
        .iter()
        .all(|path| path.parent() == Some(native.root.as_path()) && !path.exists()));
}

#[test]
fn bash_changed_files_flush_timeout_and_remaining_call_budget() {
    let root = tempfile::tempdir().unwrap();
    let (journal, mut capture) = capture(root.path(), BashChangedFiles::List);
    let flush = Arc::new(crate::watcher_backend::flush::FlushControl::default());
    Arc::get_mut(&mut capture).unwrap().flush = Some(flush.clone());
    let _budget = capture.budget(Some(Instant::now() + Duration::from_millis(20)));
    event(
        &journal,
        root.path(),
        "changed.txt",
        notify::EventKind::Create(CreateKind::File),
    );
    let start = Instant::now();
    let report = capture.finish();
    assert_eq!(report.incomplete, vec![NOT_CAUGHT_UP]);
    assert!(report.changed_files_list_envelope.total.is_at_least());
    assert!(start.elapsed() < Duration::from_millis(150));
    assert!(!root
        .path()
        .read_dir()
        .unwrap()
        .any(|entry| crate::watcher_backend::flush::is_marker_path(&entry.unwrap().path())));
    assert!(!flush.wait(
        root.path(),
        Instant::now() + crate::watcher_backend::flush::BASH_FLUSH_LIMIT
    ));
    assert!(!root
        .path()
        .read_dir()
        .unwrap()
        .any(|entry| crate::watcher_backend::flush::is_marker_path(&entry.unwrap().path())));
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "requires a live FSEvents service; run the focused native gate separately from large fixtures"
)]
fn bash_changed_files_flush_native_latency_during_build() {
    let _activity = filesystem_activity_lease();
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='flush-build-fixture'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("src/lib.rs"),
        "include!(\"generated.rs\");\n",
    )
    .unwrap();
    std::fs::write(root.path().join("build.rs"), r##"fn main() {
        std::fs::create_dir_all("src/generated").unwrap();
        let mut modules = String::new();
        for i in 0..2000 {
            std::fs::write(format!("src/generated/m{i}.rs"), format!("pub const VALUE: usize = {i};\n")).unwrap();
            modules.push_str(&format!("#[path=\"generated/m{i}.rs\"] pub mod m{i};\n"));
            // Sustain source writes without synthetic CPU load during sampling.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::fs::write("src/generated.rs", modules).unwrap();
        println!("cargo:rerun-if-changed=build.rs");
    }"##).unwrap();
    let native = NativeWatcher::new(root.path(), false);
    native.ready();
    let measure = || {
        let start = Instant::now();
        let caught_up = native.counters.bash_flush.wait(
            &native.root,
            start + crate::watcher_backend::flush::BASH_FLUSH_LIMIT,
        );
        (start.elapsed(), caught_up)
    };
    let quiet = (0..10).map(|_| measure()).collect::<Vec<_>>();
    let mut child = Command::new("cargo")
        .args(["check", "--offline", "--quiet"])
        .current_dir(root.path())
        .env("CARGO_TARGET_DIR", root.path().join("target"))
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .spawn()
        .unwrap();
    let mut busy = Vec::new();
    native
        .generated_ready
        .recv_timeout(Duration::from_secs(30))
        .expect("build started source generation");
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        let before = native.generated.load(Ordering::Relaxed);
        let measurement = measure();
        if native.generated.load(Ordering::Relaxed) != before {
            busy.push(measurement);
        }
        if busy.len() == 20 {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("fixture build exceeded deadline");
        }
    }
    assert!(child.wait().unwrap().success());
    assert!(
        !busy.is_empty(),
        "no barrier overlapped generated-source writes"
    );
    let summary = |values: &[(Duration, bool)]| {
        let sum = values
            .iter()
            .map(|(time, _)| time.as_secs_f64() * 1000.0)
            .sum::<f64>();
        let max = values
            .iter()
            .map(|(time, _)| time.as_secs_f64() * 1000.0)
            .fold(0.0_f64, f64::max);
        format!(
            "n={} mean_ms={:.3} max_ms={:.3} timeouts={}",
            values.len(),
            sum / values.len() as f64,
            max,
            values.iter().filter(|(_, ok)| !ok).count()
        )
    };
    eprintln!(
        "bash flush barrier {} quiet [{}] active_cargo_source_generation [{}]",
        std::env::consts::OS,
        summary(&quiet),
        summary(&busy)
    );
    assert!(quiet.iter().all(|(_, ok)| *ok));
}

#[test]
fn bash_changed_files_flush_markers_are_excluded_from_project_walks() {
    let root = tempfile::tempdir().unwrap();
    for name in [".aft-flush-123-1", ".aft-flush-not-private", "visible.rs"] {
        std::fs::write(root.path().join(name), "pub const VALUE: usize = 1;\n").unwrap();
    }
    let marker = root.path().join(".aft-flush-123-1");
    assert!(crate::project_content::is_excluded(&marker));
    assert!(!crate::project_content::is_excluded(
        &root.path().join(".aft-flush-not-private")
    ));
    let paths = crate::search_index::project_walk_builder(root.path())
        .build()
        .filter_map(Result::ok)
        .map(|entry| entry.path().to_path_buf())
        .collect::<Vec<_>>();
    assert!(!paths.contains(&marker));
    assert!(paths.contains(&root.path().join("visible.rs")));
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "requires a live FSEvents service; this focused native gate includes its own concurrent 50k-file writer"
)]
fn bash_changed_files_flush_native_no_false_complete_during_50k_activity() {
    let _activity = filesystem_activity_lease();
    let root = tempfile::tempdir().unwrap();
    let activity = tempfile::tempdir().unwrap();
    let activity_path = activity.path().to_path_buf();
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
    let writer = std::thread::spawn(move || {
        for i in 0..50_000 {
            std::fs::write(activity_path.join(format!("fixture-{i}")), "fixture\n").unwrap();
            if i == 100 {
                started_tx.send(()).unwrap();
            }
        }
    });
    started_rx.recv().unwrap();
    let native = NativeWatcher::new(root.path(), false);
    native
        .delay_last_write
        .store(true, std::sync::atomic::Ordering::Release);
    let capture = native.capture();
    std::fs::write(
        root.path().join("last-write.txt"),
        "written immediately before completion\n",
    )
    .unwrap();
    let report = capture.finish();
    writer.join().unwrap();
    if report.incomplete.is_empty() {
        assert!(
            report
                .files
                .iter()
                .any(|file| file.path == "last-write.txt"),
            "false complete: {report:?}"
        );
    } else {
        // Under service saturation a bounded timeout is honest; a successful
        // flush must never silently omit the write, as FlushSync alone could.
        assert_eq!(report.incomplete, vec![NOT_CAUGHT_UP], "{report:?}");
    }
}

#[test]
fn bash_changed_files_flush_inherits_reply_budget_before_spawn_and_clears_on_handoff() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.path().into());
    config.bash.changed_files = BashChangedFiles::List;
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let deadline = Instant::now() + Duration::from_millis(20);
    let budget = BudgetGuard::new(deadline);
    let receipt = Arc::new(crate::bash_background::SpawnReceipt::new(
        Instant::now() + Duration::from_secs(30),
    ));
    receipt.bind_changed_files_deadline(budget.shared());
    let capture = crate::bash_background::with_spawn_receipt(receipt.clone(), || {
        Capture::start(&ctx).unwrap()
    });
    assert_eq!(*capture.call_deadline.lock().unwrap(), Some(deadline));
    receipt.limit_changed_files_deadline(deadline + Duration::from_secs(1));
    assert_eq!(*capture.call_deadline.lock().unwrap(), Some(deadline));
    drop(budget);
    assert!(capture.call_deadline.lock().unwrap().is_none());
    receipt.limit_changed_files_deadline(deadline);
    assert!(
        capture.call_deadline.lock().unwrap().is_none(),
        "handoff cannot revive a caller budget"
    );
    let late =
        crate::bash_background::with_spawn_receipt(receipt, || Capture::start(&ctx).unwrap());
    assert!(late.call_deadline.lock().unwrap().is_none());
}

#[test]
fn bash_changed_files_diff_rendering_respects_reply_deadline() {
    let root = repo();
    let (journal, mut capture) = capture(root.path(), BashChangedFiles::Diff);
    assert!(capture.git.is_some(), "real git before-images are required");
    Arc::get_mut(&mut capture).unwrap().diff_delay_for_test = Duration::from_millis(200);
    for name in ["clean.txt", "deleted.txt"] {
        std::fs::write(root.path().join(name), "changed\n").unwrap();
        event(
            &journal,
            root.path(),
            name,
            notify::EventKind::Modify(ModifyKind::Any),
        );
    }
    let started = Instant::now();
    let _budget = capture.budget(Some(started + Duration::from_millis(30)));
    let report = capture.finish();
    assert!(
        started.elapsed() < Duration::from_millis(120),
        "render overran reply budget: {:?}",
        started.elapsed()
    );
    assert_eq!(
        report.files.len(),
        2,
        "remaining paths should still be listed"
    );
    assert!(report.files.iter().all(|file| file.diff.is_none()));
    assert!(
        report.incomplete.is_empty(),
        "a complete path list needs no new caveat"
    );
}

#[test]
fn bash_changed_files_expired_render_budget_marks_cut_list_incomplete() {
    let root = tempfile::tempdir().unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::List);
    std::fs::write(root.path().join("changed.txt"), "changed").unwrap();
    event(
        &journal,
        root.path(),
        "changed.txt",
        notify::EventKind::Create(CreateKind::File),
    );
    let _budget = capture.budget(Some(Instant::now() - Duration::from_millis(1)));
    let report = capture.finish();
    assert!(report.files.is_empty());
    assert_eq!(report.incomplete, vec![NOT_CAUGHT_UP]);
    assert_eq!(
        report.changed_files_list_envelope.reason,
        Some(Reason::TimeLimit)
    );
    assert!(report.changed_files_list_envelope.total.is_at_least());
}
