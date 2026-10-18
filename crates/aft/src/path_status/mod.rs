//! Per-view status annotations for paths that could not join a complete generation.
//!
//! The path table contains only pending or failed annotations. Removing an
//! annotation means assembly no longer reports a problem for that path; manifest
//! membership remains the authority for whether the path is in a generation.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension, TransactionBehavior};

/// Maximum paths included in a refresh-status response; counts include paths beyond this limit.
pub const VISIBLE_PATH_CAP: usize = 20;

const PATH_STATUS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS path_status (
    rel_path BLOB NOT NULL PRIMARY KEY,
    state TEXT NOT NULL CHECK(state IN ('pending', 'failed')),
    reason TEXT NOT NULL,
    since_generation INTEGER NOT NULL CHECK(since_generation >= 0)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS maintenance_outcomes (
    operation TEXT NOT NULL PRIMARY KEY,
    outcome TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation >= 0)
) WITHOUT ROWID;
"#;

const UPSERT_PATH_STATUS: &str =
    "INSERT INTO path_status (rel_path, state, reason, since_generation)
    VALUES (?1, ?2, ?3, ?4)
    ON CONFLICT(rel_path) DO UPDATE SET
        state = excluded.state,
        reason = excluded.reason,
        since_generation = CASE
            WHEN path_status.state = excluded.state THEN path_status.since_generation
            ELSE excluded.since_generation
        END";

/// The two path problem states exposed by view status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathState {
    Pending,
    Failed,
}

impl PathState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One durable annotation from the per-view `path_status` derived table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathStatus {
    pub rel_path: Vec<u8>,
    pub state: PathState,
    pub reason: String,
    pub since_generation: u64,
}

/// Summary data for refresh-status responses: total counts plus a bounded,
/// bytewise-ordered path list. This is internal response data, not a tool schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathStatusSummary {
    pub pending_count: usize,
    pub failed_count: usize,
    pub paths: Vec<PathStatus>,
}

#[derive(Debug)]
pub enum PathStatusError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    InvalidState(String),
    InvalidGeneration(i64),
    GenerationOutOfRange(u64),
}

impl fmt::Display for PathStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "path-status I/O error: {error}"),
            Self::Sqlite(error) => write!(f, "path-status SQLite error: {error}"),
            Self::InvalidState(state) => write!(f, "invalid path-status state `{state}`"),
            Self::InvalidGeneration(generation) => {
                write!(f, "invalid negative path-status generation {generation}")
            }
            Self::GenerationOutOfRange(generation) => {
                write!(
                    f,
                    "path-status generation {generation} exceeds SQLite INTEGER"
                )
            }
        }
    }
}

impl Error for PathStatusError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::InvalidState(_) | Self::InvalidGeneration(_) | Self::GenerationOutOfRange(_) => {
                None
            }
        }
    }
}

impl From<std::io::Error> for PathStatusError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for PathStatusError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Owns the path-status table in one view's derived SQLite database.
#[derive(Debug)]
pub struct PathStatusStore {
    path: PathBuf,
    connection: crate::db::file_identity::IdentityConnection,
}

impl PathStatusStore {
    /// Opens the conventional derived-state database for one view.
    pub fn open(view_dir: &Path) -> Result<Self, PathStatusError> {
        Self::open_at(&view_dir.join("derived.sqlite"))
    }

    /// Opens a view-derived database at an explicit path. This allows the view
    /// assembler to share its already-created derived database with this table.
    pub fn open_at(path: &Path) -> Result<Self, PathStatusError> {
        if let Some(parent) = path.parent() {
            crate::private_storage::open_dir(parent, parent)?;
        }
        let connection = crate::db::file_identity::IdentityConnection::open(
            path,
            "path_status::PathStatusStore::open_at",
        )?;
        connection.busy_timeout(crate::db::STEADY_BUSY_TIMEOUT)?;
        // This is a long-lived annotation store. A publication can update one
        // row per source file, so rollback journaling creates and unlinks a
        // journal for every annotation instead of appending to one WAL.
        crate::blob_store::retry_while_busy(crate::db::STEADY_BUSY_TIMEOUT, || {
            connection.pragma_update(None, "journal_mode", "WAL")
        })?;
        connection.execute_batch(PATH_STATUS_SCHEMA)?;
        Ok(Self {
            path: path.to_path_buf(),
            connection,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Marks a path pending. Repeated pending reports preserve the generation
    /// where the annotation began, so status consumers can identify its age.
    pub fn mark_pending(
        &mut self,
        rel_path: &[u8],
        reason: impl Into<String>,
        since_generation: u64,
    ) -> Result<(), PathStatusError> {
        self.upsert(
            rel_path,
            PathState::Pending,
            reason.into(),
            since_generation,
        )
    }

    /// Marks a pending set in one IMMEDIATE transaction with the same age rule
    /// as `mark_pending`. Readers see all rows or none; an error or unwinding
    /// cancellation rolls back the entire set, without changing durability.
    /// Reasons may differ per path, and repeated paths are applied in order.
    pub fn mark_pending_many<'a>(
        &mut self,
        rows: impl IntoIterator<Item = (&'a [u8], &'a str)>,
        since_generation: u64,
    ) -> Result<(), PathStatusError> {
        let generation = i64::try_from(since_generation)
            .map_err(|_| PathStatusError::GenerationOutOfRange(since_generation))?;
        let mut rows = rows.into_iter().peekable();
        if rows.peek().is_none() {
            return Ok(());
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut upsert = transaction.prepare_cached(UPSERT_PATH_STATUS)?;
            for (path, reason) in rows {
                upsert.execute(params![
                    path,
                    PathState::Pending.as_str(),
                    reason,
                    generation
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Marks a path failed. A state transition starts a new annotation age.
    pub fn mark_failed(
        &mut self,
        rel_path: &[u8],
        reason: impl Into<String>,
        since_generation: u64,
    ) -> Result<(), PathStatusError> {
        self.upsert(rel_path, PathState::Failed, reason.into(), since_generation)
    }

    /// Removes an annotation after the path joined a complete generation.
    pub fn clear(&mut self, rel_path: &[u8]) -> Result<(), PathStatusError> {
        self.connection.execute(
            "DELETE FROM path_status WHERE rel_path = ?1",
            params![rel_path],
        )?;
        Ok(())
    }

    /// Clears completed paths together, preserving annotations for paths that
    /// still have outstanding work.
    pub(crate) fn clear_many<'a>(
        &mut self,
        paths: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<(), PathStatusError> {
        let transaction = self.connection.transaction()?;
        {
            let mut delete =
                transaction.prepare_cached("DELETE FROM path_status WHERE rel_path = ?1")?;
            for path in paths {
                delete.execute(params![path])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn status_for(&self, rel_path: &[u8]) -> Result<Option<PathStatus>, PathStatusError> {
        let row = self
            .connection
            .query_row(
                "SELECT rel_path, state, reason, since_generation
                 FROM path_status WHERE rel_path = ?1",
                params![rel_path],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(Self::decode_row).transpose()
    }

    /// Returns counts for both states and at most twenty bytewise-ordered paths.
    pub fn record_maintenance_outcome(
        &mut self,
        operation: &str,
        outcome: &str,
        generation: u64,
    ) -> Result<(), PathStatusError> {
        self.connection.execute(
            "INSERT INTO maintenance_outcomes(operation, outcome, generation)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(operation) DO NOTHING",
            params![operation, outcome, generation],
        )?;
        Ok(())
    }

    pub fn maintenance_outcome(
        &self,
        operation: &str,
    ) -> Result<Option<(String, u64)>, PathStatusError> {
        self.connection
            .query_row(
                "SELECT outcome, generation FROM maintenance_outcomes WHERE operation = ?1",
                [operation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(PathStatusError::from)
    }

    pub fn summary(&self) -> Result<PathStatusSummary, PathStatusError> {
        let (pending_count, failed_count) = self.connection.query_row(
            "SELECT
                 COALESCE(SUM(CASE WHEN state = 'pending' THEN 1 ELSE 0 END), 0),
                 COALESCE(SUM(CASE WHEN state = 'failed' THEN 1 ELSE 0 END), 0)
             FROM path_status",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let pending_count = usize::try_from(pending_count)
            .map_err(|_| PathStatusError::InvalidGeneration(pending_count))?;
        let failed_count = usize::try_from(failed_count)
            .map_err(|_| PathStatusError::InvalidGeneration(failed_count))?;

        let mut statement = self.connection.prepare(
            "SELECT rel_path, state, reason, since_generation
             FROM path_status
             ORDER BY rel_path
             LIMIT ?1",
        )?;
        let paths = statement
            .query_map(params![VISIBLE_PATH_CAP as i64], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(Self::decode_row)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PathStatusSummary {
            pending_count,
            failed_count,
            paths,
        })
    }

    fn upsert(
        &mut self,
        rel_path: &[u8],
        state: PathState,
        reason: String,
        since_generation: u64,
    ) -> Result<(), PathStatusError> {
        let generation = i64::try_from(since_generation)
            .map_err(|_| PathStatusError::GenerationOutOfRange(since_generation))?;
        self.connection.execute(
            UPSERT_PATH_STATUS,
            params![rel_path, state.as_str(), reason, generation],
        )?;
        Ok(())
    }

    fn decode_row(
        (rel_path, state, reason, since_generation): (Vec<u8>, String, String, i64),
    ) -> Result<PathStatus, PathStatusError> {
        Ok(PathStatus {
            rel_path,
            state: PathState::parse(&state).ok_or(PathStatusError::InvalidState(state))?,
            reason,
            since_generation: u64::try_from(since_generation)
                .map_err(|_| PathStatusError::InvalidGeneration(since_generation))?,
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::PathStatusStore;
    use std::cell::Cell;

    thread_local! {
        static IN_TRANSACTION: Cell<bool> = const { Cell::new(false) };
        static WRITE_TRANSACTIONS: Cell<usize> = const { Cell::new(0) };
    }

    // Observe SQLite statements, including implicit transactions, rather than
    // counting calls to the batch API: a caller can accidentally bypass that API.
    pub(crate) fn count_write_transactions(
        store: &mut PathStatusStore,
        operation: impl FnOnce(&mut PathStatusStore),
    ) -> usize {
        IN_TRANSACTION.set(false);
        WRITE_TRANSACTIONS.set(0);
        store.connection.trace(Some(|sql| {
            let sql = sql.trim_start();
            if sql.starts_with("BEGIN") {
                IN_TRANSACTION.set(true);
            } else if sql.starts_with("COMMIT") {
                WRITE_TRANSACTIONS.set(WRITE_TRANSACTIONS.get() + 1);
                IN_TRANSACTION.set(false);
            } else if sql.starts_with("ROLLBACK") {
                IN_TRANSACTION.set(false);
            } else if !IN_TRANSACTION.get()
                && (sql.starts_with("INSERT") || sql.starts_with("DELETE"))
            {
                WRITE_TRANSACTIONS.set(WRITE_TRANSACTIONS.get() + 1);
            }
        }));
        operation(store);
        store.connection.trace(None);
        WRITE_TRANSACTIONS.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_batch_matches_per_row_states_reasons_and_ages() {
        let dir = tempfile::tempdir().unwrap();
        let mut per_row = PathStatusStore::open(&dir.path().join("per-row")).unwrap();
        let mut batched = PathStatusStore::open(&dir.path().join("batched")).unwrap();
        for store in [&mut per_row, &mut batched] {
            store.mark_pending(b"pending", "old pending", 2).unwrap();
            store.mark_failed(b"failed", "old failure", 3).unwrap();
            store.mark_pending(b"cleared", "old cleared", 4).unwrap();
            store.clear(b"cleared").unwrap();
            store.mark_failed(b"untouched", "keep failed", 5).unwrap();
        }
        let rows = [
            (b"new".as_slice(), "new path"),
            (b"pending", "still pending"),
            (b"failed", "retry failure"),
            (b"cleared", "requeued"),
            (b"\x80binary", "byte path"),
            (b"new", "last reason wins"),
        ];
        for (path, reason) in rows {
            per_row.mark_pending(path, reason, 9).unwrap();
        }
        batched.mark_pending_many(rows, 9).unwrap();
        assert_eq!(batched.summary().unwrap(), per_row.summary().unwrap());
        for (path, age, reason) in [
            (b"new".as_slice(), 9, "last reason wins"),
            (b"pending", 2, "still pending"),
            (b"failed", 9, "retry failure"),
            (b"cleared", 9, "requeued"),
        ] {
            let row = batched.status_for(path).unwrap().unwrap();
            assert_eq!(row.state, PathState::Pending);
            assert_eq!(row.since_generation, age);
            assert_eq!(row.reason, reason);
        }
        assert_eq!(
            batched.status_for(b"untouched").unwrap().unwrap().state,
            PathState::Failed
        );
    }

    #[test]
    fn pending_batch_sql_error_and_cancellation_roll_back_all_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PathStatusStore::open(dir.path()).unwrap();
        store.mark_failed(b"existing", "failure", 2).unwrap();
        let before = store.summary().unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse_b BEFORE INSERT ON path_status WHEN new.rel_path = x'62'
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        assert!(store
            .mark_pending_many(
                [
                    (b"existing".as_slice(), "retry"),
                    (b"a", "new"),
                    (b"b", "new")
                ],
                9,
            )
            .is_err());
        assert_eq!(store.summary().unwrap(), before);
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_b")
            .unwrap();
        let paths = (0..5000).map(|n| format!("path-{n}")).collect::<Vec<_>>();
        let cancelled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store
                .mark_pending_many(
                    paths.iter().enumerate().map(|(index, path)| {
                        assert_ne!(index, 1000, "injected cancellation after 1000 rows");
                        (path.as_bytes(), "pending")
                    }),
                    9,
                )
                .unwrap();
        }));
        assert!(cancelled.is_err());
        drop(store);
        let store = PathStatusStore::open(dir.path()).unwrap();
        assert_eq!(store.summary().unwrap(), before);
        let integrity: String = store
            .connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }

    #[test]
    fn pending_batch_process_exit_before_commit_leaves_no_partial_rows() {
        const CHILD_DIR: &str = "AFT_PENDING_BATCH_CRASH_DIR";
        const INTERRUPTED_EXIT: i32 = 86;
        if let Some(dir) = std::env::var_os(CHILD_DIR) {
            let mut store = PathStatusStore::open(Path::new(&dir)).unwrap();
            let paths = (0..5000).map(|n| format!("path-{n}")).collect::<Vec<_>>();
            store
                .mark_pending_many(
                    paths.iter().enumerate().map(|(index, path)| {
                        if index == 1000 {
                            // Exit without destructors, leaving SQLite to recover the
                            // uncommitted transaction when the parent opens the store.
                            std::process::exit(INTERRUPTED_EXIT);
                        }
                        (path.as_bytes(), "pending")
                    }),
                    9,
                )
                .unwrap();
            panic!("crash injection did not execute");
        }
        let dir = tempfile::tempdir().unwrap();
        let mut store = PathStatusStore::open(dir.path()).unwrap();
        store.mark_failed(b"existing", "failure", 2).unwrap();
        let before = store.summary().unwrap();
        drop(store);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "path_status::tests::pending_batch_process_exit_before_commit_leaves_no_partial_rows", "--nocapture"])
            .env(CHILD_DIR, dir.path())
            .status().unwrap();
        assert_eq!(status.code(), Some(INTERRUPTED_EXIT));
        let store = PathStatusStore::open(dir.path()).unwrap();
        assert_eq!(store.summary().unwrap(), before);
    }

    #[test]
    fn pending_batch_5000_rows_use_one_immediate_transaction_and_empty_uses_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PathStatusStore::open(dir.path()).unwrap();
        let paths = (0..5000).map(|n| format!("path-{n}")).collect::<Vec<_>>();
        let started = std::time::Instant::now();
        let count = test_support::count_write_transactions(&mut store, |store| {
            store
                .mark_pending_many(paths.iter().map(|path| (path.as_bytes(), "queued")), 1)
                .unwrap();
        });
        eprintln!("5000-row pending transaction: {:?}", started.elapsed());
        assert_eq!(count, 1);
        assert_eq!(store.summary().unwrap().pending_count, 5000);
        let count = test_support::count_write_transactions(&mut store, |store| {
            store.mark_pending_many(std::iter::empty(), 1).unwrap();
        });
        assert_eq!(count, 0);
        assert!(matches!(
            store.mark_pending_many([(b"bad".as_slice(), "bad")], u64::MAX),
            Err(PathStatusError::GenerationOutOfRange(_))
        ));
        assert!(store.status_for(b"bad").unwrap().is_none());
        let synchronous: i64 = store
            .connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 2, "batching must not weaken FULL durability");
    }

    #[test]
    fn repeated_status_writes_use_wal_without_rollback_journals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived.sqlite");
        // Older views already have a DELETE-mode status database.
        let old = rusqlite::Connection::open(&path).unwrap();
        old.execute_batch(PATH_STATUS_SCHEMA).unwrap();
        drop(old);
        let mut store = PathStatusStore::open(dir.path()).unwrap();
        let mode: String = store
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        for index in 0..32 {
            store
                .mark_pending(format!("file_{index}.ts").as_bytes(), "waiting", 1)
                .unwrap();
        }
        assert_eq!(store.summary().unwrap().pending_count, 32);
        assert!(dir.path().join("derived.sqlite-wal").is_file());
        assert!(!dir.path().join("derived.sqlite-journal").exists());
        drop(store);
        assert_eq!(
            PathStatusStore::open(dir.path())
                .unwrap()
                .summary()
                .unwrap()
                .pending_count,
            32
        );
    }

    #[test]
    fn summary_counts_all_rows_but_caps_and_orders_visible_paths_by_bytes() {
        let dir = tempfile::tempdir().expect("create view dir");
        let mut store = PathStatusStore::open(dir.path()).expect("open path-status store");
        assert_eq!(
            store.summary().expect("summarize empty statuses"),
            PathStatusSummary {
                pending_count: 0,
                failed_count: 0,
                paths: Vec::new(),
            }
        );
        for index in 0..25 {
            store
                .mark_pending(format!("z{index:02}").as_bytes(), "read changed", 7)
                .expect("mark pending");
        }
        store
            .mark_failed(b"\x80binary", "quarantined", 8)
            .expect("mark failed");
        store
            .mark_failed(b"a/path", "quota", 9)
            .expect("mark failed");

        let summary = store.summary().expect("summarize statuses");
        assert_eq!(summary.pending_count, 25);
        assert_eq!(summary.failed_count, 2);
        assert_eq!(summary.paths.len(), VISIBLE_PATH_CAP);
        assert_eq!(summary.paths[0].rel_path, b"a/path");
        assert_eq!(summary.paths[1].rel_path, b"z00");
        assert!(summary
            .paths
            .windows(2)
            .all(|paths| paths[0].rel_path <= paths[1].rel_path));
    }

    #[test]
    fn repeated_state_preserves_annotation_age_and_success_clears_it() {
        let dir = tempfile::tempdir().expect("create view dir");
        let mut store = PathStatusStore::open(dir.path()).expect("open path-status store");

        store
            .mark_pending(b"src/lib.rs", "read changed", 3)
            .expect("mark pending");
        store
            .mark_pending(b"src/lib.rs", "still changing", 9)
            .expect("repeat pending");
        assert_eq!(
            store
                .status_for(b"src/lib.rs")
                .expect("read status")
                .expect("pending row")
                .since_generation,
            3
        );

        store
            .mark_failed(b"src/lib.rs", "quarantined", 10)
            .expect("mark failed");
        assert_eq!(
            store
                .status_for(b"src/lib.rs")
                .expect("read status")
                .expect("failed row")
                .since_generation,
            10
        );
        store.clear(b"src/lib.rs").expect("clear completed path");
        assert!(store
            .status_for(b"src/lib.rs")
            .expect("read status")
            .is_none());
    }
    #[test]
    fn completed_path_batch_is_atomic_and_preserves_pending_paths() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = super::PathStatusStore::open(directory.path()).unwrap();
        for path in [b"a".as_slice(), b"b", b"pending"] {
            store.mark_pending(path, "queued", 1).unwrap();
        }
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse_b BEFORE DELETE ON path_status WHEN old.rel_path = x'62'
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        assert!(store.clear_many([b"a".as_slice(), b"b"]).is_err());
        assert!(store.status_for(b"a").unwrap().is_some());
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_b")
            .unwrap();
        store.clear_many([b"a".as_slice(), b"b"]).unwrap();
        assert!(store.status_for(b"a").unwrap().is_none());
        assert!(store.status_for(b"b").unwrap().is_none());
        assert!(store.status_for(b"pending").unwrap().is_some());
    }
}
