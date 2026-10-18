use super::*;
use std::sync::mpsc::{channel, Receiver, Sender};

struct HelperGate {
    reached: Sender<()>,
    release: Receiver<()>,
    finished: Sender<()>,
}

static HELPER_GATES: LazyLock<Mutex<HashMap<PathBuf, HelperGate>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static RUNNING_GATES: LazyLock<Mutex<HashMap<PathBuf, HelperGate>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(unix)]
pub(crate) struct HelperStall {
    path: PathBuf,
    gates: &'static Mutex<HashMap<PathBuf, HelperGate>>,
    pub(crate) reached: Receiver<()>,
    release: Sender<()>,
    finished: Receiver<()>,
}

#[cfg(unix)]
impl Drop for HelperStall {
    fn drop(&mut self) {
        self.gates.lock().unwrap().remove(&self.path);
        if self.release.send(()).is_ok() {
            let _ = self.finished.recv_timeout(IO_BUDGET);
        }
    }
}

#[cfg(unix)]
pub(crate) fn stall_helper(path: &Path) -> HelperStall {
    stall_helper_at(path, &HELPER_GATES)
}

#[cfg(unix)]
pub(crate) fn stall_running_helper(path: &Path) -> HelperStall {
    stall_helper_at(path, &RUNNING_GATES)
}

#[cfg(unix)]
fn stall_helper_at(
    path: &Path,
    gates: &'static Mutex<HashMap<PathBuf, HelperGate>>,
) -> HelperStall {
    let (reached, reached_rx) = channel();
    let (release, release_rx) = channel();
    let (finished, finished_rx) = channel();
    gates.lock().unwrap().insert(
        path.to_path_buf(),
        HelperGate {
            reached,
            release: release_rx,
            finished,
        },
    );
    HelperStall {
        path: path.to_path_buf(),
        gates,
        reached: reached_rx,
        release,
        finished: finished_rx,
    }
}

static BLOCKED_DIRECTORIES: LazyLock<Mutex<HashMap<PathBuf, Arc<AtomicUsize>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn before_helper(path: &Path) {
    wait_at_gate(path, &HELPER_GATES);
}

pub(super) fn after_helper_start(path: &Path) {
    wait_at_gate(path, &RUNNING_GATES);
}

fn wait_at_gate(path: &Path, gates: &Mutex<HashMap<PathBuf, HelperGate>>) {
    let gate = gates.lock().unwrap().remove(path);
    if let Some(gate) = gate {
        let _ = gate.reached.send(());
        let _ = gate.release.recv();
        let _ = gate.finished.send(());
    }
}

pub(super) fn before_directory_probe(path: &Path) -> io::Result<()> {
    if let Some(fired) = BLOCKED_DIRECTORIES.lock().unwrap().get(path) {
        fired.fetch_add(1, Ordering::Relaxed);
        return Err(failure(
            path,
            "read_blocked",
            "injected stalled directory enumeration",
        ));
    }
    Ok(())
}

struct BlockDirectory(PathBuf, Arc<AtomicUsize>);

impl BlockDirectory {
    fn new(path: &Path) -> Self {
        let fired = Arc::new(AtomicUsize::new(0));
        BLOCKED_DIRECTORIES
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), Arc::clone(&fired));
        Self(path.to_path_buf(), fired)
    }
}

impl Drop for BlockDirectory {
    fn drop(&mut self) {
        BLOCKED_DIRECTORIES.lock().unwrap().remove(&self.0);
    }
}

#[test]
fn metadata_and_canonicalize_do_not_enumerate_an_ancestor() {
    let dir = tempfile::tempdir().unwrap();
    let ancestor = std::fs::canonicalize(dir.path()).unwrap();
    let root = ancestor.join("project");
    std::fs::create_dir(&root).unwrap();
    let blocked = BlockDirectory::new(&ancestor);
    assert!(
        with_budget(Some(Budget::after(IO_BUDGET)), || metadata(&root))
            .unwrap()
            .is_dir()
    );
    let resolved = with_budget(Some(Budget::after(IO_BUDGET)), || canonicalize(&root)).unwrap();
    assert_eq!(resolved, std::fs::canonicalize(&root).unwrap());
    assert_eq!(blocked.1.load(Ordering::Relaxed), 0);
}

// Only macOS and Linux currently identify local filesystems for direct access;
// Windows and unknown mounts retain per-file helper isolation.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn sibling_metadata_reuses_traversal_without_granting_listing_access() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    assert!(local_filesystem(&root).unwrap());
    let paths: Vec<_> = (0..8).map(|n| root.join(format!("file-{n}"))).collect();
    for path in &paths {
        std::fs::write(path, "sibling").unwrap();
    }
    let blocked = BlockDirectory::new(&root);
    let budget = Budget::after(IO_BUDGET);
    with_budget(Some(budget.clone()), || {
        for path in &paths {
            assert!(metadata(path).unwrap().is_file());
        }
        assert_eq!(budget.measurements(), (1, 1), "one cold stat helper");
        assert_eq!(blocked.1.load(Ordering::Relaxed), 0);
        let error = read(&paths[0], None).unwrap_err();
        assert!(error.to_string().contains("read_blocked"), "{error}");
        assert_eq!(blocked.1.load(Ordering::Relaxed), 1);
        assert_eq!(
            budget.measurements(),
            (2, 2),
            "reading still probes the parent"
        );
    });
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn canonicalize_warms_sibling_traversal_shared_only_within_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    assert!(local_filesystem(&root).unwrap());
    let first = root.join("first");
    let second = root.join("second");
    std::fs::write(&first, "first").unwrap();
    std::fs::write(&second, "second").unwrap();
    let blocked = BlockDirectory::new(&root);
    let budget = Budget::after(IO_BUDGET);
    with_budget(Some(budget.clone()), || {
        assert_eq!(canonicalize(&first).unwrap(), first);
        assert_eq!(canonicalize(&second).unwrap(), second);
        let worker_budget = budget.clone();
        let worker_path = second.clone();
        std::thread::spawn(move || {
            with_budget(Some(worker_budget), || {
                assert!(metadata(&worker_path).unwrap().is_file());
            });
        })
        .join()
        .unwrap();
        assert_eq!(budget.measurements(), (1, 1));
    });
    assert!(!cached_traversal(&root), "no evidence outside the request");
    let fresh_budget = Budget::after(IO_BUDGET);
    with_budget(Some(fresh_budget.clone()), || {
        assert!(metadata(&second).unwrap().is_file());
        assert_eq!(
            fresh_budget.measurements(),
            (1, 1),
            "a new request probes again"
        );
    });
    assert_eq!(blocked.1.load(Ordering::Relaxed), 0);
}

#[test]
fn failed_stats_do_not_warm_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing");
    let budget = Budget::after(IO_BUDGET);
    with_budget(Some(budget.clone()), || {
        assert_eq!(metadata(&path).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(
            canonicalize(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(!cached_traversal(dir.path()));
        assert_eq!(budget.measurements(), (2, 2));
        std::fs::write(&path, "now present").unwrap();
        assert!(metadata(&path).unwrap().is_file());
        assert_eq!(budget.measurements(), (3, 3));
    });
}

#[test]
fn traversal_does_not_answer_an_explicit_directory_access_probe() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("file");
    std::fs::write(&path, "contents").unwrap();
    let blocked = BlockDirectory::new(&root);
    with_budget(Some(Budget::after(IO_BUDGET)), || {
        assert!(metadata(&path).unwrap().is_file());
        let error = probe_target(&root).unwrap_err();
        assert!(error.to_string().contains("read_blocked"), "{error}");
        assert_eq!(blocked.1.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn timed_out_traversal_does_not_publish_late_success() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("file");
    std::fs::write(&path, "contents").unwrap();
    let budget = Budget::after(Duration::from_millis(250));
    let caller_budget = budget.clone();
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (finished_tx, finished_rx) = channel();
    struct Finished(Sender<()>);
    impl Drop for Finished {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let caller = std::thread::spawn(move || {
        with_budget(Some(caller_budget), || {
            run_traversal(&path, move |path| {
                let result = std::fs::metadata(path)?;
                started_tx.send(()).unwrap();
                let _ = release_rx.recv();
                Ok((result, Finished(finished_tx)))
            })
            .err()
            .unwrap()
        })
    });
    let started = started_rx.recv_timeout(IO_BUDGET);
    let error = caller.join().unwrap();
    release_tx.send(()).unwrap();
    finished_rx.recv_timeout(IO_BUDGET).unwrap();
    started.expect("the stat must finish before holding its result past the deadline");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(timeout_code(&error), "read_blocked");
    with_budget(Some(budget), || assert!(!cached_traversal(&root)));
}

#[cfg(unix)]
#[test]
fn traversal_keeps_symlink_stats_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let file = root.join("file");
    let link = root.join("link");
    std::fs::write(&file, "contents").unwrap();
    std::os::unix::fs::symlink(&file, &link).unwrap();
    let budget = Budget::after(IO_BUDGET);
    with_budget(Some(budget.clone()), || {
        assert!(metadata(&file).unwrap().is_file());
        assert!(metadata(&link).unwrap().is_file());
        assert_eq!(canonicalize(&link).unwrap(), file);
        assert_eq!(budget.measurements(), (3, 3));
    });
}

#[test]
fn delete_does_not_enumerate_project_root_parent() {
    let dir = tempfile::tempdir().unwrap();
    let ancestor = std::fs::canonicalize(dir.path()).unwrap();
    let root = ancestor.join("project");
    std::fs::create_dir(&root).unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.restrict_to_project_root = true;
    config.storage_dir = Some(ancestor.join("store"));
    config.backup.enabled = Some(false);
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let blocked = BlockDirectory::new(&ancestor);
    // Separate requests exercise both the initial root resolution and its memo.
    for name in ["cold", "cached"] {
        let target = root.join(name);
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("file.txt"), "delete me").unwrap();
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": name, "command": "delete_file", "files": [target], "recursive": true
        }))
        .unwrap();
        let response = crate::commands::delete_file::handle_delete_file(&request, &ctx);
        assert!(response.success, "{name}: {:?}", response.data);
        assert!(!target.exists());
    }
    assert_eq!(blocked.1.load(Ordering::Relaxed), 0);
}

#[test]
fn explicit_directory_probe_still_observes_blocked_access() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = BlockDirectory::new(dir.path());
    let error = with_budget(Some(Budget::after(IO_BUDGET)), || {
        probe_directory(dir.path(), None)
    })
    .unwrap_err();
    assert!(error.to_string().contains("read_blocked"));
    assert_eq!(blocked.1.load(Ordering::Relaxed), 1);
}

#[test]
fn unscheduled_helper_is_queue_timeout_and_never_runs_late() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unscheduled");
    let (reached_tx, reached_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (finished_tx, finished_rx) = channel();
    HELPER_GATES.lock().unwrap().insert(
        path.clone(),
        HelperGate {
            reached: reached_tx,
            release: release_rx,
            finished: finished_tx,
        },
    );
    let fired = Arc::new(AtomicUsize::new(0));
    let work_fired = Arc::clone(&fired);
    let (dropped_tx, dropped_rx) = channel();
    struct Dropped(Sender<()>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let dropped = Dropped(dropped_tx);
    let caller = std::thread::spawn(move || {
        run_with_limiter(
            &path,
            Some(Instant::now() + Duration::from_millis(80)),
            Arc::new(Limiter::new(1)),
            move |_| {
                let _dropped = dropped;
                work_fired.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        )
    });
    let reached = reached_rx.recv_timeout(IO_BUDGET);
    let error = caller.join().unwrap().unwrap_err();
    release_tx.send(()).unwrap();
    finished_rx.recv_timeout(IO_BUDGET).unwrap();
    dropped_rx.recv_timeout(IO_BUDGET).unwrap();
    reached.expect("the helper must reach the scheduling gate");
    assert!(
        error.to_string().contains("read_io_queue_timeout"),
        "{error}"
    );
    assert!(!error.to_string().contains("read_blocked"), "{error}");
    assert_eq!(
        fired.load(Ordering::Relaxed),
        0,
        "expired work must not start after release"
    );
}

#[test]
fn saturated_limiter_is_queue_timeout_and_does_not_start_work() {
    let limiter = Arc::new(Limiter::new(1));
    let _permit = limiter.acquire(Instant::now() + IO_BUDGET).unwrap();
    let fired = Arc::new(AtomicUsize::new(0));
    let work_fired = Arc::clone(&fired);
    let error = run_with_limiter(
        Path::new("queued"),
        Some(Instant::now() + Duration::from_millis(80)),
        limiter,
        move |_| {
            work_fired.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("read_io_queue_timeout"),
        "{error}"
    );
    assert!(!error.to_string().contains("read_blocked"), "{error}");
    assert_eq!(fired.load(Ordering::Relaxed), 0);
    let cached_error = ProbeError::from(&error).error();
    let response = access_error("queued", Path::new("queued"), cached_error);
    assert_eq!(
        response.data["code"], "read_io_queue_timeout",
        "{response:?}"
    );
}

#[cfg(unix)]
#[test]
fn queued_time_does_not_consume_running_access_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queued-then-healthy");
    let gate = stall_helper(&path);
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (result_tx, result_rx) = channel();
    let caller = std::thread::spawn(move || {
        result_tx
            .send(with_budget(
                Some(Budget::after(Duration::from_secs(5))),
                || {
                    run_with_timeout(
                        &path,
                        None,
                        Arc::new(Limiter::new(1)),
                        Duration::from_secs(1),
                        None,
                        move |_| {
                            started_tx.send(()).unwrap();
                            let _ = release_rx.recv();
                            Ok(42)
                        },
                    )
                },
            ))
            .unwrap();
    });
    gate.reached.recv_timeout(IO_BUDGET).unwrap();
    // Model a loaded runner that schedules the helper late, then a healthy
    // operation taking less than its own access budget, but not the leftover.
    assert!(result_rx.recv_timeout(Duration::from_millis(750)).is_err());
    drop(gate);
    let started = started_rx.recv_timeout(IO_BUDGET);
    let early = result_rx.recv_timeout(Duration::from_millis(500));
    let _ = release_tx.send(());
    caller.join().unwrap();
    started.expect("the filesystem operation must actually start");
    let result = early.unwrap_or_else(|_| result_rx.recv_timeout(IO_BUDGET).unwrap());
    assert_eq!(
        result.unwrap(),
        42,
        "queueing must not exhaust the access cap"
    );
}

#[test]
fn running_access_keeps_shorter_request_deadline() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (finished_tx, finished_rx) = channel();
    let caller = std::thread::spawn(move || {
        with_budget(Some(Budget::after(Duration::from_millis(80))), || {
            run_with_timeout(
                Path::new("running-past-request"),
                None,
                Arc::new(Limiter::new(1)),
                Duration::from_secs(1),
                None,
                move |_| {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv();
                    let _ = finished_tx.send(());
                    Ok(())
                },
            )
        })
    });
    let started = started_rx.recv_timeout(IO_BUDGET);
    let error = caller.join().unwrap().unwrap_err();
    let _ = release_tx.send(());
    finished_rx.recv_timeout(IO_BUDGET).unwrap();
    started.expect("the filesystem operation must actually start");
    assert_eq!(timeout_code(&error), "read_blocked");
    let message = error.to_string();
    let duration = message
        .split_once("within ")
        .unwrap()
        .1
        .split_once(" s")
        .unwrap()
        .0
        .parse::<f64>()
        .unwrap();
    assert!((0.0..=0.080).contains(&duration), "{error}");
}

#[cfg(unix)]
#[test]
fn queued_time_without_request_budget_does_not_consume_running_access_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queued-without-request-budget");
    let gate = stall_helper(&path);
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (result_tx, result_rx) = channel();
    let caller = std::thread::spawn(move || {
        assert!(current().is_none(), "this call must have no request budget");
        result_tx
            .send(run_with_timeout(
                &path,
                None,
                Arc::new(Limiter::new(1)),
                Duration::from_secs(1),
                None,
                move |_| {
                    started_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                    Ok(42)
                },
            ))
            .unwrap();
    });
    gate.reached.recv_timeout(IO_BUDGET).unwrap();
    assert!(result_rx.recv_timeout(Duration::from_millis(750)).is_err());
    drop(gate);
    let started = started_rx.recv_timeout(IO_BUDGET);
    let early = result_rx.recv_timeout(Duration::from_millis(500));
    let _ = release_tx.send(());
    caller.join().unwrap();
    started.expect("the filesystem operation must actually start");
    let result = early.unwrap_or_else(|_| result_rx.recv_timeout(IO_BUDGET).unwrap());
    assert_eq!(result.unwrap(), 42, "queueing is not a request deadline");
}

#[cfg(unix)]
#[test]
fn shared_probe_waiter_follows_queued_then_running_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let queued = stall_helper(&path);
    let running = stall_running_helper(&path);
    let budget = Budget::after(Duration::from_secs(20));
    let owner_path = path.clone();
    let owner_budget = budget.clone();
    let owner = std::thread::spawn(move || {
        with_budget(Some(owner_budget), || probe_directory(&owner_path, None))
    });
    queued.reached.recv_timeout(IO_BUDGET).unwrap();
    let (waiting_tx, waiting_rx) = channel();
    let (result_tx, result_rx) = channel();
    let waiter_budget = budget.clone();
    let waiter = std::thread::spawn(move || {
        with_budget(Some(waiter_budget), || {
            waiting_tx.send(()).unwrap();
            result_tx.send(probe_directory(&path, None)).unwrap();
        });
    });
    waiting_rx.recv_timeout(IO_BUDGET).unwrap();
    // Neither phase exceeds the access cap, but their sum exceeds the old
    // waiter's fixed five-second allowance.
    assert!(result_rx.recv_timeout(Duration::from_millis(3750)).is_err());
    drop(queued);
    running.reached.recv_timeout(IO_BUDGET).unwrap();
    let early = result_rx.recv_timeout(Duration::from_millis(2500));
    drop(running);
    let owner_result = owner.join().unwrap();
    waiter.join().unwrap();
    let result = early.unwrap_or_else(|_| result_rx.recv_timeout(IO_BUDGET).unwrap());
    result.expect("a shared waiter must receive the healthy owner's result");
    owner_result.unwrap();
    assert_eq!(budget.measurements(), (1, 1), "the probe must be shared");
    assert!(!budget.failed.load(Ordering::Acquire));
}

#[cfg(unix)]
#[test]
fn shared_probe_waiter_keeps_shorter_request_deadline_after_owner_starts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let queued = stall_helper(&path);
    let running = stall_running_helper(&path);
    let budget = Budget::after(Duration::from_secs(10));
    let owner_path = path.clone();
    let owner_budget = budget.clone();
    let owner = std::thread::spawn(move || {
        with_budget(Some(owner_budget), || probe_directory(&owner_path, None))
    });
    queued.reached.recv_timeout(IO_BUDGET).unwrap();
    let (waiting_tx, waiting_rx) = channel();
    let (result_tx, result_rx) = channel();
    let waiter = std::thread::spawn(move || {
        with_budget(Some(budget), || {
            let at = Instant::now() + Duration::from_millis(500);
            waiting_tx.send(at).unwrap();
            result_tx.send(probe_directory(&path, Some(at))).unwrap();
        });
    });
    let at = waiting_rx.recv_timeout(IO_BUDGET).unwrap();
    assert!(result_rx.recv_timeout(Duration::from_millis(100)).is_err());
    drop(queued);
    running.reached.recv_timeout(IO_BUDGET).unwrap();
    let result = result_rx.recv_timeout(Duration::from_secs(2));
    let finished = Instant::now();
    drop(running);
    let _ = owner.join().unwrap();
    waiter.join().unwrap();
    let error = result.unwrap().unwrap_err();
    assert_eq!(timeout_code(&error), "read_io_queue_timeout", "{error}");
    assert!(finished >= at, "the waiter must not expire early");
    assert!(finished < at + Duration::from_secs(1));
}

#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
#[test]
fn shared_probe_waiters_preserve_owner_queue_and_access_timeout_codes() {
    for running in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let gate = if running {
            stall_running_helper(&path)
        } else {
            stall_helper(&path)
        };
        let budget =
            Budget::after(Duration::from_secs(5)).with_access_budget(Duration::from_millis(200));
        let owner_path = path.clone();
        let owner_budget = budget.clone();
        let owner = std::thread::spawn(move || {
            with_budget(Some(owner_budget), || probe_directory(&owner_path, None))
        });
        gate.reached.recv_timeout(IO_BUDGET).unwrap();
        let (result_tx, result_rx) = channel();
        let waiter = std::thread::spawn(move || {
            with_budget(Some(budget), || {
                result_tx.send(probe_directory(&path, None)).unwrap();
            });
        });
        let result = result_rx.recv_timeout(Duration::from_secs(2));
        drop(gate);
        let owner_error = owner.join().unwrap().unwrap_err();
        waiter.join().unwrap();
        let waiter_error = result.unwrap().unwrap_err();
        let expected = if running {
            "read_blocked"
        } else {
            "read_io_queue_timeout"
        };
        assert_eq!(timeout_code(&owner_error), expected, "{owner_error}");
        assert_eq!(timeout_code(&waiter_error), expected, "{waiter_error}");
    }
}

#[test]
fn running_helper_timeout_has_only_native_platform_hint() {
    let (release_tx, release_rx) = channel();
    let (started_tx, started_rx) = channel();
    let (finished_tx, finished_rx) = channel();
    let caller = std::thread::spawn(move || {
        run_with_limiter(
            Path::new("running"),
            None,
            Arc::new(Limiter::new(1)),
            move |_| {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv();
                finished_tx.send(()).unwrap();
                Ok(())
            },
        )
    });
    let started = started_rx.recv_timeout(IO_BUDGET);
    let error = caller.join().unwrap().unwrap_err();
    release_tx.send(()).unwrap();
    finished_rx.recv_timeout(IO_BUDGET).unwrap();
    started.expect("the helper must start its operation");
    assert!(error.to_string().contains("read_blocked"), "{error}");
    let message = error.to_string();
    let (prefix, duration_and_hint) = message.split_once("within ").unwrap();
    assert_eq!(prefix, "read_blocked: running: OS access did not finish ");
    let (duration, hint) = duration_and_hint.split_once(" s").unwrap();
    assert!(duration.parse::<f64>().unwrap() > 0.0);
    let expected_hint = if cfg!(target_os = "macos") {
        "; on macOS, check ck-aft access in Privacy & Security, then retry"
    } else {
        ""
    };
    assert_eq!(hint, expected_hint);
}
