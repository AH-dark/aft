use super::*;
use crate::db::wal_credit_probe::{main_io_counts, open_counted};
use rusqlite::OpenFlags;
use std::fs;
use std::io::Write;

#[test]
fn s2_queued_batch_commits_before_soft_checkpoint() {
    let _guard = REFRESH_WORKER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_temp, store, source) = fixture();
    let root = store.project_root.clone();
    let dir = store.publication_dir.clone();
    let second = root.join("second.ts");
    let policy = Arc::new(Mutex::new(Policy {
        soft_bytes: 1,
        hard_bytes: u64::MAX,
        ..Policy::default()
    }));
    drop(store);
    let worker = isolated_callgraph_refresh_worker_for_test(root.clone());
    set_callgraph_refresh_worker_test_seam(root.clone(), Duration::ZERO, false);
    REFRESH_WORKER_TEST_SEAMS
        .get()
        .unwrap()
        .lock()
        .unwrap()
        .get_mut(&root)
        .unwrap()
        .wal_policy = Some(Arc::clone(&policy));
    let (held, release) = install_callgraph_refresh_worker_test_gate(root.clone());
    let pending = Arc::new(parking_lot::Mutex::new(BTreeSet::new()));
    fs::write(&source, "export function first() {}\n").unwrap();
    enqueue_callgraph_store_refresh(
        dir.clone(),
        root.clone(),
        vec![source],
        Arc::clone(&pending),
    );
    held.recv_timeout(Duration::from_secs(10)).unwrap();
    fs::write(&second, "export function second() {}\n").unwrap();
    enqueue_callgraph_store_refresh(dir, root.clone(), vec![second], pending);
    release.send(()).unwrap();
    assert!(worker.wait(Duration::from_secs(30)));
    // Shutdown joins the worker, including maintenance after publishing idle.
    drop(worker);
    let events = policy.lock().unwrap().events.clone();
    clear_callgraph_refresh_worker_test_seam(&root);
    assert_eq!(
        events,
        ["commit", "commit", "passive"],
        "queued refreshes must precede the soft checkpoint"
    );
}

fn fixture() -> (tempfile::TempDir, CallGraphStore, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let source = root.join("main.ts");
    fs::write(&source, "export function before() { return 1; }\n").unwrap();
    let store = CallGraphStore::open(temp.path().join("store"), root).unwrap();
    store.cold_build(std::slice::from_ref(&source)).unwrap();
    store.checkpoint_wal_truncate();
    (temp, store, source)
}

fn thresholds(store: &CallGraphStore, soft: u64, hard: u64) {
    assert!(hard >= soft * 2);
    let mut policy = store.refresh_wal.lock().unwrap();
    policy.soft_bytes = soft;
    policy.hard_bytes = hard;
}

fn edit(store: &CallGraphStore, source: &Path, name: &str) {
    fs::write(
        source,
        format!("export function {name}() {{ return 2; }}\n"),
    )
    .unwrap();
    store.refresh_files(&[source.to_path_buf()]).unwrap();
    assert!(
        store.node_for(Path::new("main.ts"), name).is_ok(),
        "committed graph remains queryable"
    );
}

#[test]
fn s2_connection_roles_keep_their_checkpoint_thresholds() {
    let (_temp, store, source) = fixture();
    let ordinary = TrackedConnection::open_in_memory(SqliteStore::CallgraphGeneration).unwrap();
    configure_connection(&ordinary).unwrap();
    assert_eq!(ordinary.wal_autocheckpoint_pages(), 4000);
    let build = TrackedConnection::open_in_memory(SqliteStore::CallgraphColdGeneration).unwrap();
    configure_build_connection(&build).unwrap();
    assert_eq!(
        build.wal_autocheckpoint_pages(),
        crate::db::lifecycle::DEFAULT_WAL_AUTOCHECKPOINT_PAGES
    );
    let views = Connection::open_in_memory().unwrap();
    crate::views::materialization::configure_materialization_connection(&views).unwrap();
    assert_eq!(
        views
            .pragma_query_value(None, "wal_autocheckpoint", |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    edit(&store, &source, "after");
    let conn = store.conn.lock().unwrap();
    assert_eq!(
        conn.wal_autocheckpoint_pages(),
        0,
        "only the refresh writer disables autocheckpoint"
    );
    assert_eq!(
        conn.pragma_query_value(None, "wal_autocheckpoint", |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn s2_non_refresh_writer_still_autocheckpoints_at_4000_pages() {
    let temp = tempfile::tempdir().unwrap();
    let raw = open_counted(&temp.path().join("ordinary.sqlite"), OpenFlags::default());
    let conn = TrackedConnection::from_connection(raw, SqliteStore::CallgraphGeneration).unwrap();
    configure_connection(&conn).unwrap();
    conn.execute_batch("CREATE TABLE payload (data BLOB);")
        .unwrap();
    let before = main_io_counts(&conn);
    conn.execute_batch("BEGIN; WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<5000) INSERT INTO payload SELECT zeroblob(4096) FROM n; COMMIT;").unwrap();
    let after = main_io_counts(&conn);
    assert!(
        after.0 > before.0,
        "non-refresh COMMIT still checkpoints main-file pages"
    );
    assert!(
        after.1 > before.1,
        "non-refresh COMMIT still syncs the main file"
    );
    assert_eq!(conn.wal_autocheckpoint_pages(), 4000);
}

#[test]
fn s2_refresh_commit_has_zero_main_file_writes_and_syncs() {
    let (_temp, store, source) = fixture();
    // Close the original owning handle before opening through the VFS shim.
    // No descriptor on the live SQLite file set is opened for observation.
    let mut slot = store.conn.lock().unwrap();
    let old = std::mem::replace(
        &mut *slot,
        TrackedConnection::open_in_memory(SqliteStore::CallgraphGeneration).unwrap(),
    );
    drop(old);
    *slot = TrackedConnection::from_connection(
        open_counted(store.sqlite_path(), OpenFlags::default()),
        SqliteStore::CallgraphGeneration,
    )
    .unwrap();
    configure_connection(&slot).unwrap();
    let before = main_io_counts(&slot);
    drop(slot);
    let source_text: String = (0..65000)
        .map(|n| format!("export function leaf_{n}() {{ return {n}; }}\n"))
        .collect();
    fs::write(&source, source_text).unwrap();
    store.refresh_files(&[source]).unwrap();
    let conn = store.conn.lock().unwrap();
    assert!(
        conn.observed_wal_bytes() > 4000 * 4096,
        "refresh must cross the old autocheckpoint threshold, got {} bytes",
        conn.observed_wal_bytes()
    );
    assert!(
        conn.observed_wal_bytes() < SOFT_BYTES,
        "no post-commit soft checkpoint may mask the COMMIT oracle"
    );
    assert_eq!(
        main_io_counts(&conn),
        before,
        "BEGIN through return of COMMIT must not write or sync the main database"
    );
}

#[test]
fn s2_soft_checkpoint_reuses_wal_without_hard_cap_fallback() {
    let (_temp, store, source) = fixture();
    thresholds(&store, 4096, u64::MAX);
    let text: String = (0..200)
        .map(|n| format!("export function leaf_{n}() {{ return {n}; }}\n"))
        .collect();
    fs::write(&source, text).unwrap();
    store.refresh_files(std::slice::from_ref(&source)).unwrap();
    let first = store.conn.lock().unwrap().observed_wal_bytes();
    assert!(first > 4096);
    assert_eq!(store.refresh_wal.lock().unwrap().passive_attempts, 1);
    // A trailing newline changes freshness but no graph rows, producing a
    // small follow-up commit that distinguishes WAL reuse from appending.
    fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    store.refresh_files(&[source]).unwrap();
    let second = store.conn.lock().unwrap().observed_wal_bytes();
    assert!(
        second < first,
        "the next commit must restart from the WAL's beginning: {first} -> {second}"
    );
    assert_eq!(store.refresh_wal.lock().unwrap().restart_attempts, 0);
}

#[test]
fn s2_hard_cap_retries_busy_restart_before_begin() {
    let (_temp, store, source) = fixture();
    thresholds(&store, 1024, 2048);
    store
        .conn
        .lock()
        .unwrap()
        .busy_timeout(Duration::from_millis(20))
        .unwrap();
    let reader = Connection::open(store.sqlite_path()).unwrap();
    reader.execute_batch("BEGIN; SELECT * FROM nodes;").unwrap();
    edit(&store, &source, "first");
    edit(&store, &source, "second");
    edit(&store, &source, "third");
    let held_bytes = store.conn.lock().unwrap().observed_wal_bytes();
    {
        let policy = store.refresh_wal.lock().unwrap();
        assert_eq!(policy.restart_attempts, 2);
        assert_eq!(
            policy.restart_busy, 2,
            "reader-pinned RESTARTs must be counted busy"
        );
    }
    reader.execute_batch("ROLLBACK").unwrap();
    edit(&store, &source, "fourth");
    let reused = store.conn.lock().unwrap().observed_wal_bytes();
    assert!(
        reused < held_bytes,
        "successful pre-BEGIN RESTART allows reuse"
    );
    let policy = store.refresh_wal.lock().unwrap();
    assert_eq!(policy.restart_attempts, 3);
    assert_eq!(policy.restart_busy, 2);
    assert!(
        policy
            .events
            .windows(2)
            .any(|pair| pair == ["restart", "commit"]),
        "successful RESTART must precede the next commit"
    );
}
