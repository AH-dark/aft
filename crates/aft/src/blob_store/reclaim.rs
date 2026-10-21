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
    /// A checkpoint after the reclaim could not copy the whole write-ahead
    /// log into the main file, usually because another connection holds an
    /// older read snapshot, so the main file keeps its old length until a
    /// later pass checkpoints again.
    pub checkpoint_blocked: bool,
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
        self.checkpoint_blocked |= other.checkpoint_blocked;
    }
}

#[derive(Clone, Copy)]
struct Policy {
    min_free_bytes: u64,
    /// Free pages are returned only when they are at least a fifth of the
    /// database's pages.
    min_free_share: bool,
    max_pages: u64,
    step_pages: u64,
}

impl Policy {
    /// Whether `free` of the database's `pages` are worth returning.
    fn eligible(&self, free: u64, pages: u64, page_size: u64) -> bool {
        free > 0
            && (!self.min_free_share || free >= pages.div_ceil(5))
            && free * page_size >= self.min_free_bytes
    }
}

// Keep reusable free pages unless at least 20% AND 256 MiB can be returned.
// At the usual 4 KiB page size a pass moves at most 16 MiB, in 64 KiB steps.
const POLICY: Policy = Policy {
    min_free_bytes: 256 * 1024 * 1024,
    min_free_share: true,
    max_pages: 4096,
    step_pages: 16,
};
pub(crate) const PASS_BUDGET: Duration = Duration::from_secs(1);

/// Free pages one [`reclaim_all_free_pages`] pass returns at most: 64 MiB at
/// the usual 4 KiB page size.
pub(crate) const ALL_FREE_PAGES_MAX_PAGES: u64 = 16_384;

pub(crate) fn reclaim(
    connection: &Connection,
    deadline: Option<Instant>,
    cancelled: &dyn Fn() -> bool,
) -> rusqlite::Result<ReclaimReport> {
    reclaim_with_policy(connection, deadline, cancelled, POLICY)
}

/// Returns free pages whatever their share of the database or their total
/// size, at most `max_pages` of them and within the same time budget as
/// [`reclaim`], in the same small incremental-vacuum steps. For a store
/// whose free pages alone put it over a disk limit: there, keeping them for
/// reuse keeps the file over the limit.
pub(crate) fn reclaim_all_free_pages(
    connection: &Connection,
    deadline: Option<Instant>,
    max_pages: u64,
) -> rusqlite::Result<ReclaimReport> {
    reclaim_with_policy(
        connection,
        deadline,
        &|| false,
        Policy {
            min_free_bytes: 0,
            min_free_share: false,
            max_pages,
            ..POLICY
        },
    )
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
    if !policy.eligible(free, pages, page_size) {
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
    // Judge what remains against the database's current size: reclaiming pages
    // shrinks the file, so the starting page count understates the free share.
    let remaining_pages: u64 = connection.pragma_query_value(None, "page_count", |r| r.get(0))?;
    report.deferred |= policy.eligible(after, remaining_pages, page_size);
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
            min_free_share: true,
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
    fn deferred_matches_eligibility_of_the_shrunk_database() {
        // Free shares near the 20% line, so a pass can drop the remaining share
        // below 20% of the starting size while it still exceeds 20% of the new one.
        for deleted in 95..=140 {
            let c = fixture("INCREMENTAL", deleted);
            let report = reclaim_with_policy(
                &c,
                None,
                &|| false,
                Policy {
                    min_free_bytes: 0,
                    min_free_share: true,
                    max_pages: 8,
                    step_pages: 4,
                },
            )
            .unwrap();
            let free: u64 = c
                .pragma_query_value(None, "freelist_count", |r| r.get(0))
                .unwrap();
            let eligible = free > 0 && free >= pages(&c).div_ceil(5);
            assert_eq!(
                report.deferred,
                eligible,
                "deleted={deleted}: free={free} pages={}",
                pages(&c)
            );
        }
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

    /// Free pages below both thresholds (50 of 512 pages, 200 KiB) are
    /// returned when every free page is asked for, at most `max_pages` per
    /// pass, with the rest reported as remaining work.
    #[test]
    fn all_free_pages_ignore_both_thresholds_and_stop_at_the_cap() {
        let c = fixture("INCREMENTAL", 50);
        let free: u64 = c
            .pragma_query_value(None, "freelist_count", |r| r.get(0))
            .unwrap();
        assert!(free >= 50, "{free}");
        let usual = reclaim(&c, None, &|| false).unwrap();
        assert_eq!((usual.steps, usual.reclaimed_pages), (0, 0));

        let before = pages(&c);
        let capped = reclaim_all_free_pages(&c, None, 20).unwrap();
        assert_eq!(capped.reclaimed_pages, 20);
        assert!(capped.deferred);
        assert!(before - pages(&c) >= 20 && before - pages(&c) <= 21);

        let rest = reclaim_all_free_pages(&c, None, 10_000).unwrap();
        assert_eq!(rest.reclaimed_pages, free - 20);
        assert!(!rest.deferred, "{rest:?}");
        assert_eq!(
            c.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
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
