//! Views-off graph parity, including new tables automatically. Refresh timestamps
//! and revision/delta storage are normalized; revision/delta contents are checked
//! separately. The named cold-build history exception is protected by a reopen test.

use super::*;
use rusqlite::types::Value;
use std::fs;

type Rows = BTreeMap<String, Vec<Vec<Value>>>;

// Completed cold-build resume history is not rewritten by incremental edits.
// Reopening verifies that a ready generation neither resumes nor adopts a
// leftover staging database. Apart from the separately checked revision/delta
// journal, all other meta values remain part of graph parity.
const COLD_BUILD_HISTORY_KEYS: [&str; 3] = [
    "committed_extracted_bytes",
    "resolve_cursor",
    "staged_corpus_fingerprint",
];

// These are write times, not freshness inputs. Source content hashes and mtimes
// remain compared. backend_status_for_file uses updated_at to choose the latest
// status, so that API's answers are compared separately before ignoring time.
const HISTORY_COLUMNS: [(&str, &str); 2] = [
    ("files", "indexed_at"),
    ("backend_file_state", "updated_at"),
];

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn logical_rows(conn: &Connection) -> Rows {
    let mut tables = conn.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap();
    let names = tables
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let mut result = Rows::new();
    for table in names {
        if table == "projection_delta_spill" {
            continue;
        }
        let mut info = conn
            .prepare(&format!("PRAGMA table_info({})", quote(&table)))
            .unwrap();
        let columns = info
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let projected: Vec<_> = columns
            .iter()
            .filter(|(name, _)| !HISTORY_COLUMNS.contains(&(table.as_str(), name.as_str())))
            .map(|(name, _)| quote(name))
            .collect();
        let mut keys: Vec<_> = columns.iter().filter(|(_, ordinal)| *ordinal > 0).collect();
        keys.sort_by_key(|(_, ordinal)| ordinal);
        let order: Vec<_> = if keys.is_empty() {
            projected.clone()
        } else {
            keys.iter().map(|(name, _)| quote(name)).collect()
        };
        let history_keys = COLD_BUILD_HISTORY_KEYS
            .map(|key| format!("'{key}'"))
            .join(",");
        let predicate = if table == "meta" {
            format!(" WHERE k != 'projection_write_revision' AND k NOT GLOB 'projection_delta_*' AND k NOT IN ({history_keys})")
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT {} FROM {}{} ORDER BY {}",
            projected.join(","),
            quote(&table),
            predicate,
            order.join(",")
        );
        let mut query = conn.prepare(&sql).unwrap();
        let rows = query
            .query_map([], |row| {
                (0..projected.len())
                    .map(|column| row.get::<_, Value>(column))
                    .collect()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        result.insert(table, rows);
    }
    result
}

pub(super) fn compare(
    incremental: &CallGraphStore,
    cold: &CallGraphStore,
) -> std::result::Result<(), String> {
    let paths = {
        let conn = incremental.conn.lock().unwrap();
        let mut query = conn
            .prepare("SELECT DISTINCT file_path FROM backend_file_state")
            .unwrap();
        let paths = query
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        paths
    };
    for path in paths {
        let file = incremental.project_root.join(&path);
        if incremental.backend_status_for_file(&file).unwrap()
            != cold.backend_status_for_file(&file).unwrap()
        {
            return Err(format!("latest backend status differs for {path}"));
        }
    }
    let incremental = logical_rows(&incremental.conn.lock().unwrap());
    let cold = logical_rows(&cold.conn.lock().unwrap());
    for table in incremental
        .keys()
        .chain(cold.keys())
        .collect::<BTreeSet<_>>()
    {
        if incremental.get(table) != cold.get(table) {
            return Err(format!(
                "every-table differential differs in {table}: incremental={:?}, cold={:?}",
                incremental.get(table),
                cold.get(table)
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_delta(
    store: &CallGraphStore,
    before: u64,
    stats: &IncrementalStats,
) -> std::result::Result<(), String> {
    let conn = store.conn.lock().unwrap();
    let after = projection_write_revision(&conn).unwrap().unwrap();
    if after != before + 1 {
        return Err(format!(
            "refresh revision: expected {}, got {after}",
            before + 1
        ));
    }
    let mut payload: String = conn
        .query_row(
            "SELECT v FROM meta WHERE k=?1",
            [format!("projection_delta_{}", after % 64)],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if payload.starts_with("spill:") {
        if payload != format!("spill:{after}") {
            return Err("wrong spill revision".into());
        }
        payload = conn
            .query_row(
                "SELECT payload FROM projection_delta_spill WHERE revision=?1",
                [after],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
    }
    let (revision, callers): (u64, BTreeSet<String>) =
        serde_json::from_str(&payload).map_err(|error| error.to_string())?;
    if revision != after || callers != stats.projection_callers {
        return Err(format!(
            "delta payload differs from refresh report: {payload}; reported {:?}",
            stats.projection_callers
        ));
    }
    Ok(())
}

fn differential(
    files_changed: usize,
) -> (
    tempfile::TempDir,
    CallGraphStore,
    CallGraphStore,
    u64,
    IncrementalStats,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let files: Vec<_> = (0..10).map(|n| {
        let file = root.join(format!("file_{n}.ts"));
        fs::write(&file, format!("export function leaf_{n}() {{ return 1; }}\nexport function caller_{n}() {{ leaf_{n}(); }}\n")).unwrap();
        file
    }).collect();
    let incremental = CallGraphStore::open(temp.path().join("incremental"), root.clone()).unwrap();
    incremental.cold_build(&files).unwrap();
    let before = incremental.projection_write_revision().unwrap().unwrap();
    for (n, file) in files[..files_changed].iter().enumerate() {
        fs::write(file, format!("export function leaf_{n}() {{ return 2; }}\n\nexport function caller_{n}() {{ leaf_{n}(); leaf_{n}(); }}\n")).unwrap();
    }
    let (stats, profile) = incremental
        .refresh_files_profiled(&files[..files_changed])
        .unwrap();
    eprintln!(
        "S2 {files_changed}-file refresh (test profile): {}",
        profile.report()
    );
    let cold = CallGraphStore::open(temp.path().join("cold"), root).unwrap();
    cold.cold_build(&files).unwrap();
    (temp, incremental, cold, before, stats)
}

#[test]
fn s2_every_table_single_file_edit() {
    let (_temp, incremental, cold, before, stats) = differential(1);
    validate_delta(&incremental, before, &stats).unwrap();
    compare(&incremental, &cold).unwrap();
}

#[test]
fn s2_every_table_ten_file_edit() {
    let (_temp, incremental, cold, before, stats) = differential(10);
    validate_delta(&incremental, before, &stats).unwrap();
    compare(&incremental, &cold).unwrap();
}

#[test]
fn s2_every_table_witness_detects_graph_corruption() {
    let (_temp, incremental, cold, before, stats) = differential(1);
    validate_delta(&incremental, before, &stats).unwrap();
    compare(&incremental, &cold).unwrap();
    incremental
        .conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE backend_file_state SET updated_at=updated_at+12345",
            [],
        )
        .unwrap();
    compare(&incremental, &cold).unwrap();
    incremental
        .conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE nodes SET name='corrupt' WHERE id=(SELECT id FROM nodes ORDER BY id LIMIT 1)",
            [],
        )
        .unwrap();
    assert!(compare(&incremental, &cold).unwrap_err().contains("nodes"));
}

#[test]
fn s2_every_table_witness_detects_delta_corruption() {
    let (_temp, incremental, cold, before, stats) = differential(1);
    validate_delta(&incremental, before, &stats).unwrap();
    compare(&incremental, &cold).unwrap();
    let revision = before + 1;
    incremental
        .conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE meta SET v=?1 WHERE k=?2",
            params![
                format!("[{revision},[\"corrupt.ts\"]]"),
                format!("projection_delta_{}", revision % 64)
            ],
        )
        .unwrap();
    assert!(validate_delta(&incremental, before, &stats)
        .unwrap_err()
        .contains("delta payload"));
}

#[test]
fn s2_refreshed_store_reopens_ready_without_resuming_or_adopting_staging() {
    let (_temp, incremental, cold, _before, _stats) = differential(1);
    let dir = incremental.publication_dir.clone();
    let root = incremental.project_root.clone();
    let sqlite = incremental.sqlite_path.clone();
    let generation = incremental.generation.clone();
    let staging = dir.join(format!(
        "{}.staging.sqlite.tmp.resume",
        incremental.project_key
    ));
    let leftover = Connection::open(&staging).unwrap();
    initialize_schema(&leftover).unwrap();
    leftover
        .execute(
            "INSERT INTO meta(k,v) VALUES('staged_build_phase','extracting')",
            [],
        )
        .unwrap();
    drop(leftover);
    let staging_before = fs::read(&staging).unwrap();
    let history_before: Vec<_> = COLD_BUILD_HISTORY_KEYS
        .map(|key| {
            incremental
                .conn
                .lock()
                .unwrap()
                .query_row("SELECT v FROM meta WHERE k=?1", [key], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap()
        })
        .into();
    drop(incremental);
    let readonly = CallGraphStore::open_readonly(dir.clone(), root.clone())
        .unwrap()
        .expect("configure's first readonly probe must find the refreshed generation ready");
    assert_eq!(readonly.sqlite_path(), sqlite);
    assert_eq!(
        readonly
            .direct_callers_of(Path::new("file_0.ts"), "leaf_0")
            .unwrap()
            .len(),
        cold.direct_callers_of(Path::new("file_0.ts"), "leaf_0")
            .unwrap()
            .len()
    );
    drop(readonly);
    // Configure uses ensure_built_with_lease_chunked. Any build/resume phase
    // would invoke its observer, even if it eventually reused the same file.
    let phases = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&phases);
    let thread = std::thread::current().id();
    set_cold_build_phase_observer(Some(Arc::new(move |_| {
        if std::thread::current().id() == thread {
            observed.fetch_add(1, AtomicOrdering::Relaxed);
        }
    })));
    let (reopened, build) =
        CallGraphStore::ensure_built_with_lease_chunked(dir, root, &[], 2).unwrap();
    set_cold_build_phase_observer(None);
    assert!(
        build.is_none(),
        "configure must return a ready generation, not a resumed or new build"
    );
    assert_eq!(
        phases.load(AtomicOrdering::Relaxed),
        0,
        "ready store must not enter any cold-build or resume phase"
    );
    assert_eq!(reopened.sqlite_path, sqlite);
    assert_eq!(reopened.generation, generation);
    assert_eq!(
        fs::read(&staging).unwrap(),
        staging_before,
        "ready reopen must leave unrelated staging untouched"
    );
    assert!(database_ready(&reopened.conn.lock().unwrap()).unwrap());
    let history_after: Vec<_> = COLD_BUILD_HISTORY_KEYS
        .map(|key| {
            reopened
                .conn
                .lock()
                .unwrap()
                .query_row("SELECT v FROM meta WHERE k=?1", [key], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap()
        })
        .into();
    assert_eq!(history_before, history_after);
    compare(&reopened, &cold).unwrap();
    assert_eq!(
        reopened
            .direct_callers_of(Path::new("file_0.ts"), "leaf_0")
            .unwrap()
            .len(),
        cold.direct_callers_of(Path::new("file_0.ts"), "leaf_0")
            .unwrap()
            .len()
    );
}

#[test]
#[ignore = "offline full-corpus measurement; set AFT_S2_MEASURE_ROOT to this worktree"]
fn s2_ten_file_commit_measurement() {
    let source =
        PathBuf::from(std::env::var_os("AFT_S2_MEASURE_ROOT").expect("worktree source root"));
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    perf_tests::copy_tracked_sources(&source, &root);
    let files: Vec<_> = callgraph::walk_project_files(&root).collect();
    let store = CallGraphStore::open(temp.path().join("store"), root.clone()).unwrap();
    store.cold_build(&files).unwrap();
    store.checkpoint_wal_truncate();
    let changed: Vec<_> = [
        "crates/aft/src/commands/zoom.rs",
        "crates/aft/src/commands/inspect.rs",
        "crates/aft/src/watcher_filter.rs",
        "crates/aft/src/logging.rs",
        "crates/aft/src/parser.rs",
        "packages/aft-bridge/src/bridge.ts",
        "packages/aft-bridge/src/format.ts",
        "packages/aft-bridge/src/jsonc.ts",
        "packages/aft-bridge/src/config-keys.ts",
        "packages/aft-bridge/src/downloader.ts",
    ]
    .map(|rel| {
        let path = root.join(rel);
        let mut text = fs::read_to_string(&path).unwrap();
        // Prepending a definition shifts existing source locations, modeling a
        // large-save refresh that rewrites graph rows for all ten files.
        let prefix = if rel.ends_with(".rs") {
            "fn s2_measurement_leaf() {}\n"
        } else {
            "function s2_measurement_leaf() {}\n"
        };
        text.insert_str(0, prefix);
        fs::write(&path, text).unwrap();
        path
    })
    .into();
    let (stats, profile) = store.refresh_files_profiled(&changed).unwrap();
    assert_eq!(stats.changed_files.len(), 10);
    eprintln!(
        "S2 full-corpus ten-file commit measurement: corpus_files={} {}",
        files.len(),
        profile.report()
    );
}
