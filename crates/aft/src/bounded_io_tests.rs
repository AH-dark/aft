use super::*;
use std::sync::mpsc::{channel, Receiver, Sender};

struct HelperGate {
    reached: Sender<()>,
    release: Receiver<()>,
    finished: Sender<()>,
}

static HELPER_GATES: LazyLock<Mutex<HashMap<PathBuf, HelperGate>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static BLOCKED_DIRECTORIES: LazyLock<Mutex<HashMap<PathBuf, Arc<AtomicUsize>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn before_helper(path: &Path) {
    let gate = HELPER_GATES.lock().unwrap().remove(path);
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
