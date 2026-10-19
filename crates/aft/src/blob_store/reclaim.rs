//! Bounded space reclamation for rebuildable SQLite blob caches.

use std::time::{Duration, Instant};

use rusqlite::Connection;

/// Logical database pages examined and returned by one reclaim pass. WAL-backed
/// files become physically shorter when SQLite next checkpoints them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReclaimReport {
    pub examined_stores: u64,
    pub examined_pages: u64,
    pub free_pages: u64,
    pub reclaimed_pages: u64,
    pub reclaimed_bytes: u64,
    pub steps: u64,
    pub legacy_stores: u64,
    pub deferred: bool,
}

impl ReclaimReport {
    pub(crate) fn merge(&mut self, other: Self) {
        self.examined_stores += other.examined_stores;
        self.examined_pages += other.examined_pages;
        self.free_pages += other.free_pages;
        self.reclaimed_pages += other.reclaimed_pages;
        self.reclaimed_bytes += other.reclaimed_bytes;
        self.steps += other.steps;
        self.legacy_stores += other.legacy_stores;
        self.deferred |= other.deferred;
    }
}

#[derive(Clone, Copy)]
struct Policy {
    min_free_bytes: u64,
    max_pages: u64,
    step_pages: u64,
}

// Keep reusable free pages unless at least 20% AND 256 MiB can be returned.
// At the usual 4 KiB page size a pass moves at most 16 MiB, in 64 KiB steps.
const POLICY: Policy = Policy {
    min_free_bytes: 256 * 1024 * 1024,
    max_pages: 4096,
    step_pages: 16,
};
pub(crate) const PASS_BUDGET: Duration = Duration::from_secs(1);

pub(crate) fn reclaim(
    connection: &Connection,
    deadline: Option<Instant>,
    cancelled: &dyn Fn() -> bool,
) -> rusqlite::Result<ReclaimReport> {
    reclaim_with_policy(connection, deadline, cancelled, POLICY)
}

fn reclaim_with_policy(
    connection: &Connection,
    deadline: Option<Instant>,
    cancelled: &dyn Fn() -> bool,
    policy: Policy,
) -> rusqlite::Result<ReclaimReport> {
    let deadline = deadline
        .unwrap_or_else(|| Instant::now() + PASS_BUDGET)
        .min(Instant::now() + PASS_BUDGET);
    // Reclaim must not inherit a writer's multi-second busy wait. The caller
    // owns this connection exclusively for the duration of the pass.
    let timeout: u64 = connection.pragma_query_value(None, "busy_timeout", |r| r.get(0))?;
    connection.busy_timeout(Duration::ZERO)?;
    let result = reclaim_inner(connection, deadline, cancelled, policy);
    connection.busy_timeout(Duration::from_millis(timeout))?;
    result
}

fn reclaim_inner(
    connection: &Connection,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
    policy: Policy,
) -> rusqlite::Result<ReclaimReport> {
    let mut report = ReclaimReport::default();
    let stop = || Instant::now() >= deadline || cancelled();
    if stop() {
        report.deferred = true;
        return Ok(report);
    }
    let page_size: u64 = connection.pragma_query_value(None, "page_size", |r| r.get(0))?;
    let pages: u64 = connection.pragma_query_value(None, "page_count", |r| r.get(0))?;
    let free: u64 = connection.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    report.examined_stores = 1;
    report.examined_pages = pages;
    report.free_pages = free;
    let mode: u32 = connection.pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    if mode != 2 {
        // NONE needs a full rewrite to acquire pointer maps. Leave old caches
        // reusable; only a newly created store opts into incremental vacuum.
        report.legacy_stores = 1;
        return Ok(report);
    }
    if free == 0 || free < pages.div_ceil(5) || free * page_size < policy.min_free_bytes {
        return Ok(report);
    }

    let target = free.min(policy.max_pages);
    let mut stepped = 0;
    while stepped < target {
        if stop() {
            report.deferred = true;
            break;
        }
        let n = policy.step_pages.min(target - stepped);
        let before_step = stepped;
        let mut statement = connection.prepare(&format!("PRAGMA incremental_vacuum({n})"))?;
        let mut rows = statement.query([])?;
        report.steps += 1;
        // This PRAGMA yields once per page despite returning zero columns.
        // execute_batch steps only once, silently abandoning the remaining work.
        loop {
            if stop() {
                report.deferred = true;
                break;
            }
            match rows.next() {
                Ok(Some(_)) => stepped += 1,
                Ok(None) => break,
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if matches!(
                        error.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    ) =>
                {
                    report.deferred = true;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if report.deferred || stepped == before_step {
            break;
        }
    }
    let after: u64 = connection.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    report.reclaimed_pages = free.saturating_sub(after);
    report.reclaimed_bytes = report.reclaimed_pages * page_size;
    report.deferred |= after >= pages.div_ceil(5) && after * page_size >= policy.min_free_bytes;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn fixture(mode: &str, deleted: u64) -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(&format!(
            "PRAGMA auto_vacuum={mode}; CREATE TABLE payloads (id INTEGER PRIMARY KEY, data BLOB);
            WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<512)
            INSERT INTO payloads SELECT x, zeroblob(4000) FROM n;
            DELETE FROM payloads WHERE id <= {deleted};"
        ))
        .unwrap();
        c
    }

    fn pages(c: &Connection) -> u64 {
        c.pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap()
    }

    fn policy() -> Policy {
        Policy {
            min_free_bytes: 64 * 1024,
            max_pages: 32,
            step_pages: 4,
        }
    }

    #[test]
    fn large_freelist_is_reclaimed_over_bounded_passes() {
        let c = fixture("INCREMENTAL", 400);
        let initial = pages(&c);
        for _ in 0..3 {
            let before = pages(&c);
            let report = reclaim_with_policy(&c, None, &|| false, policy()).unwrap();
            let removed = before - pages(&c);
            // One pointer-map page may disappear in addition to each free page.
            assert!(removed <= 33, "reclaimed {removed} pages in one pass");
            assert_eq!(report.reclaimed_pages, 32);
            assert_eq!(report.steps, 8);
            assert!(report.deferred);
        }
        assert!(pages(&c) < initial);
        assert!(pages(&c) > initial - 100);
    }

    #[test]
    fn below_either_threshold_is_untouched() {
        for (deleted, min_free_bytes) in [(50, 0), (400, 4 * 1024 * 1024)] {
            let c = fixture("INCREMENTAL", deleted);
            let before = pages(&c);
            let report = reclaim_with_policy(
                &c,
                None,
                &|| false,
                Policy {
                    min_free_bytes,
                    ..policy()
                },
            )
            .unwrap();
            assert_eq!(pages(&c), before);
            assert_eq!(report.steps, 0);
            assert!(!report.deferred);
            assert!(report.free_pages > 0);
        }
    }

    #[test]
    fn legacy_none_is_never_vacuumed_or_converted() {
        let c = fixture("NONE", 400);
        let before = pages(&c);
        let report = reclaim_with_policy(&c, None, &|| false, policy()).unwrap();
        assert_eq!(pages(&c), before);
        assert_eq!(report.legacy_stores, 1);
        assert_eq!(report.steps, 0);
        assert_eq!(
            c.pragma_query_value(None, "auto_vacuum", |r| r.get::<_, u32>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn cancellation_between_steps_keeps_database_valid() {
        let c = fixture("INCREMENTAL", 400);
        let checks = Cell::new(0);
        let cancel = || {
            checks.set(checks.get() + 1);
            checks.get() > 6
        };
        let report = reclaim_with_policy(&c, None, &cancel, policy()).unwrap();
        assert!(report.reclaimed_pages > 0 && report.reclaimed_pages < 32);
        assert!(report.deferred);
        assert_eq!(
            c.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM payloads", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            112
        );
    }

    #[test]
    fn expired_deadline_performs_no_reclaim() {
        let c = fixture("INCREMENTAL", 400);
        let before = pages(&c);
        let report = reclaim_with_policy(&c, Some(Instant::now()), &|| false, policy()).unwrap();
        assert_eq!(pages(&c), before);
        assert!(report.deferred);
        assert_eq!(report.steps, 0);
    }
}
