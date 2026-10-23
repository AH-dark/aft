//! In-process ledger projections, with file signatures for other-process writes.
use super::{PathStatusStore, PathStatusSummary};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

type Files = [Option<(u64, Option<SystemTime>)>; 2];

fn files(path: &Path) -> Files {
    let metadata = |path: &Path| {
        std::fs::metadata(path)
            .ok()
            .map(|metadata| (metadata.len(), metadata.modified().ok()))
    };
    [metadata(path), metadata(&path.with_extension("sqlite-wal"))]
}

#[derive(Debug)]
struct Projection {
    revision: u64,
    files: Option<Files>,
    read_at: Instant,
    summary: Option<Arc<PathStatusSummary>>,
}

#[derive(Debug)]
pub(crate) struct Health {
    path: PathBuf,
    projection: Mutex<Projection>,
}

type Registry = Mutex<HashMap<PathBuf, Weak<Health>>>;
static REGISTRY: OnceLock<Registry> = OnceLock::new();

impl Health {
    pub(crate) fn for_view(view_dir: &Path) -> Arc<Self> {
        Self::for_path(&view_dir.join("derived.sqlite"))
    }

    pub(super) fn for_path(path: &Path) -> Arc<Self> {
        let mut registry = REGISTRY.get_or_init(Default::default).lock();
        if let Some(health) = registry.get(path).and_then(Weak::upgrade) {
            return health;
        }
        let health = Arc::new(Self {
            path: path.to_path_buf(),
            projection: Mutex::new(Projection {
                revision: 0,
                files: None,
                read_at: Instant::now(),
                summary: None,
            }),
        });
        registry.insert(path.to_path_buf(), Arc::downgrade(&health));
        health
    }

    /// A local writer publishes through its already-open connection. The next
    /// health tick need not reopen SQLite just because its WAL changed.
    pub(super) fn committed(&self, store: &PathStatusStore) -> u64 {
        let revision = self.projection.lock().revision;
        let before = files(&self.path);
        let summary = store.health_summary().ok().map(Arc::new);
        let after = files(&self.path);
        let mut projection = self.projection.lock();
        let uncontended = projection.revision == revision;
        projection.revision = projection.revision.wrapping_add(1);
        projection.files = (uncontended && before == after).then_some(after);
        projection.read_at = Instant::now();
        projection.summary = summary;
        projection.revision
    }

    pub(crate) fn snapshot(&self) -> (u64, Option<Arc<PathStatusSummary>>) {
        let current_files = files(&self.path);
        let revision = {
            let projection = self.projection.lock();
            if projection.files == Some(current_files)
                && projection.read_at.elapsed() < Duration::from_secs(30)
            {
                return (projection.revision, projection.summary.clone());
            }
            projection.revision
        };
        // Another process can write the ledger or remove its view directory.
        // Re-read unknown signatures next tick, with a 30-second backstop for
        // changes that leave file sizes and modification times unchanged.
        #[cfg(test)]
        super::HEALTH_READS.with(|reads| reads.set(reads.get() + 1));
        let summary = PathStatusStore::open_at(&self.path)
            .ok()
            .and_then(|store| store.health_summary().ok())
            .map(Arc::new);
        let after = files(&self.path);
        let mut projection = self.projection.lock();
        if projection.revision != revision {
            // A concurrent commit wins. Do not label the older read with the
            // writer's version: it must be reconsidered on the next tick.
            return (revision, summary);
        }
        projection.revision = projection.revision.wrapping_add(1);
        projection.files = (current_files == after).then_some(after);
        projection.read_at = Instant::now();
        projection.summary = summary.clone();
        (projection.revision, summary)
    }
}

impl Drop for Health {
    fn drop(&mut self) {
        let mut registry = REGISTRY.get_or_init(Default::default).lock();
        if registry
            .get(&self.path)
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            registry.remove(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_status::{SkipReason, StatusIntent};
    use std::collections::{BTreeMap, BTreeSet, HashSet};

    #[test]
    fn local_writers_publish_pending_failed_clear_and_settled_skips() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        health.snapshot();
        let reads = super::super::HEALTH_READS.with(std::cell::Cell::get);
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        writer.mark_pending(b"a", "waiting", 1).unwrap();
        assert_eq!(health.snapshot().1.unwrap().pending_count, 1);
        writer
            .mark_pending_many([(&b"b"[..], "waiting")], 1)
            .unwrap();
        assert_eq!(health.snapshot().1.unwrap().pending_count, 2);
        writer.mark_failed(b"a", "missing payload", 1).unwrap();
        assert_eq!(health.snapshot().1.unwrap().failed_count, 1);
        writer.clear(b"a").unwrap();
        assert_eq!(health.snapshot().1.unwrap().failed_count, 0);
        writer.clear_many([&b"b"[..]]).unwrap();
        assert_eq!(health.snapshot().1.unwrap().pending_count, 0);
        writer
            .settle(&StatusIntent {
                generation: 2,
                marked_through: writer.mark_epoch().unwrap(),
                members: HashSet::from([b"c".to_vec()]),
                keep_pending: BTreeSet::new(),
                failed: BTreeMap::new(),
                fresh_skips: BTreeMap::from([(
                    b"c".to_vec(),
                    Some(SkipReason::UnsupportedGitMode),
                )]),
            })
            .unwrap();
        let settled = health.snapshot().1.unwrap();
        assert_eq!(
            settled.skipped_by_reason.get("unsupported_git_mode"),
            Some(&1)
        );
        assert_eq!(settled.skipped_sample[0].rel_path, b"c");
        drop(writer);
        assert_eq!(
            health.snapshot().1.unwrap().skipped_by_reason,
            settled.skipped_by_reason
        );
        assert_eq!(super::super::HEALTH_READS.with(std::cell::Cell::get), reads);
    }

    #[test]
    fn unknown_write_before_local_close_and_backstop_force_disk_reads() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        writer.mark_pending(b"a", "waiting", 1).unwrap();
        assert_eq!(health.snapshot().1.unwrap().pending_count, 1);
        let cached_files = health.projection.lock().files;
        let before = super::super::HEALTH_READS.with(std::cell::Cell::get);
        // Bypass the in-process publisher, as an external process does.
        let external = rusqlite::Connection::open(writer.path()).unwrap();
        external
            .execute("UPDATE path_status SET state = 'failed'", [])
            .unwrap();
        drop(external);
        drop(writer);
        assert_eq!(
            health.projection.lock().files,
            cached_files,
            "closing must never bless an external signature"
        );
        assert_eq!(health.snapshot().1.unwrap().failed_count, 1);
        assert_eq!(
            super::super::HEALTH_READS.with(std::cell::Cell::get),
            before + 1
        );
        health.projection.lock().read_at = Instant::now() - Duration::from_secs(31);
        assert_eq!(health.snapshot().1.unwrap().failed_count, 1);
        assert_eq!(
            super::super::HEALTH_READS.with(std::cell::Cell::get),
            before + 2
        );
    }

    #[test]
    fn last_local_close_preserves_signature_and_wal_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let health = Health::for_view(dir.path());
        let mut writer = PathStatusStore::open(dir.path()).unwrap();
        writer.mark_pending(b"a", "waiting", 1).unwrap();
        let before = files(writer.path());
        assert!(before[1].is_some(), "committed frames remain in the WAL");
        assert!(writer
            .connection
            .db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE)
            .unwrap());
        let threshold: u64 = writer
            .connection
            .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
            .unwrap();
        assert_eq!(threshold, 1000);
        drop(writer);
        assert_eq!(
            files(&health.path),
            before,
            "local close must not change health signatures"
        );
        assert_eq!(health.snapshot().1.unwrap().pending_count, 1);
        let reopened = PathStatusStore::open(dir.path()).unwrap();
        assert_eq!(
            reopened.summary().unwrap().pending_count,
            1,
            "committed WAL survives close/reopen"
        );
    }

    #[test]
    fn wal_reset_reclaims_closed_store_high_water() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("derived.sqlite-wal");
        let configured: i64;
        {
            let mut writer = PathStatusStore::open(dir.path()).unwrap();
            configured = writer
                .connection
                .pragma_query_value(None, "journal_size_limit", |row| row.get(0))
                .unwrap();
            // Exceed the normal 1,000-page autocheckpoint in one commit. The
            // allocation remains after close until a subsequent WAL reset.
            writer
                .mark_pending(b"large", "x".repeat(5 * 1024 * 1024), 1)
                .unwrap();
        }
        let high_water = std::fs::metadata(&wal).unwrap().len();
        assert!(high_water > crate::db::lifecycle::WAL_JOURNAL_SIZE_LIMIT as u64);
        {
            let mut writer = PathStatusStore::open(dir.path()).unwrap();
            // Reopening can rebuild the WAL index. Establish a completed
            // checkpoint on this connection before testing its next reset.
            let checkpoint: (i64, i64, i64) = writer
                .connection
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .unwrap();
            assert_eq!(checkpoint.0, 0);
            assert_eq!(checkpoint.1, checkpoint.2);
            writer
                .mark_pending(b"small", "after checkpoint", 2)
                .unwrap();
        }
        let after_reset = std::fs::metadata(&wal).unwrap().len();
        assert!(
            after_reset <= crate::db::lifecycle::WAL_JOURNAL_SIZE_LIMIT as u64,
            "checkpointed WAL high-water allocation was retained: {high_water} -> {after_reset}"
        );
        assert_eq!(configured, crate::db::lifecycle::WAL_JOURNAL_SIZE_LIMIT);
        assert_eq!(
            PathStatusStore::open(dir.path())
                .unwrap()
                .summary()
                .unwrap()
                .pending_count,
            2
        );
        println!("closed path-status WAL reset: {high_water} -> {after_reset} bytes");
    }
}
