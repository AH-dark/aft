use super::*;

use std::collections::BTreeMap;
use std::ffi::{c_int, c_uint, c_void, CStr};

#[derive(Debug, Default)]
pub(crate) struct ReadCounts {
    pub statements: BTreeMap<String, (usize, i32)>,
    pub misses: i32,
}

struct Trace {
    db: *mut rusqlite::ffi::sqlite3,
    start: i32,
    counts: ReadCounts,
}

unsafe extern "C" fn trace(
    mask: c_uint,
    context: *mut c_void,
    stmt: *mut c_void,
    _: *mut c_void,
) -> c_int {
    let trace = &mut *context.cast::<Trace>();
    let (mut misses, mut high) = (0, 0);
    rusqlite::ffi::sqlite3_db_status(
        trace.db,
        rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS,
        &mut misses,
        &mut high,
        0,
    );
    if mask == rusqlite::ffi::SQLITE_TRACE_STMT as c_uint {
        trace.start = misses;
    } else {
        let sql = CStr::from_ptr(rusqlite::ffi::sqlite3_sql(stmt.cast()))
            .to_string_lossy()
            .into_owned();
        let entry = trace.counts.statements.entry(sql).or_default();
        entry.0 += 1;
        entry.1 += misses - trace.start;
    }
    0
}

/// Measures the owning connection, without opening any of its SQLite files.
pub(crate) struct ReadProbe {
    store: FamilyStore,
    trace: Box<Trace>,
}

impl ReadProbe {
    pub(crate) fn start(store: &FamilyStore) -> Self {
        let connection = lock(&store.inner.connection);
        connection.execute_batch("PRAGMA shrink_memory").unwrap();
        cache_misses(&connection, true);
        let mut trace = Box::new(Trace {
            db: unsafe { connection.handle() },
            start: 0,
            counts: ReadCounts::default(),
        });
        unsafe {
            rusqlite::ffi::sqlite3_trace_v2(
                trace.db,
                (rusqlite::ffi::SQLITE_TRACE_STMT | rusqlite::ffi::SQLITE_TRACE_PROFILE) as c_uint,
                Some(self::trace),
                (&mut *trace as *mut Trace).cast(),
            );
        }
        Self {
            store: store.clone(),
            trace,
        }
    }

    pub(crate) fn finish(mut self) -> ReadCounts {
        self.trace.counts.misses = cache_misses(&lock(&self.store.inner.connection), false);
        std::mem::take(&mut self.trace.counts)
    }
}

impl Drop for ReadProbe {
    fn drop(&mut self) {
        let _connection = lock(&self.store.inner.connection);
        unsafe {
            rusqlite::ffi::sqlite3_trace_v2(self.trace.db, 0, None, std::ptr::null_mut());
        }
    }
}

/// Inserts `rows` straight into the store's payload table, in large
/// transactions on the store's own connection, so tests can build stores of
/// several gigabytes quickly. Every row gets epoch 0 and a correct digest.
pub(crate) fn bulk_insert(
    store: &FamilyStore,
    rows: impl IntoIterator<Item = ([u8; 32], Vec<u8>)>,
) {
    let mut connection = lock(&store.inner.connection);
    let mut rows = rows.into_iter().peekable();
    while rows.peek().is_some() {
        let tx = connection.transaction().unwrap();
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO blob_payloads
                     (full_key, payload, payload_digest, payload_schema, ref_epoch, created_at_ms)
                     VALUES (?1, ?2, ?3, ?4, 0, 0)",
                )
                .unwrap();
            for (key, payload) in rows.by_ref().take(10_000) {
                insert
                    .execute(params![
                        key.as_slice(),
                        &payload,
                        blake3::hash(&payload).as_bytes().as_slice(),
                        i64::from(store.inner.plane.payload_schema()),
                    ])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
}

/// Pages SQLite reports free in the store file.
pub(crate) fn freelist_pages(store: &FamilyStore) -> u64 {
    lock(&store.inner.connection)
        .pragma_query_value(None, "freelist_count", |row| row.get(0))
        .unwrap()
}

/// Sets one row's epoch directly; its size and presence do not change.
pub(crate) fn set_epoch(store: &FamilyStore, key: &FamilyKey, epoch: u64) {
    lock(&store.inner.connection)
        .execute(
            "UPDATE blob_payloads SET ref_epoch = ?2 WHERE full_key = ?1",
            params![key.as_bytes().as_slice(), epoch as i64],
        )
        .unwrap();
}

/// Whether the store's payload totals are complete and would be used.
pub(crate) fn has_totals(store: &FamilyStore) -> bool {
    exact_totals(&lock(&store.inner.connection))
        .unwrap()
        .is_some()
}

/// The row count and payload bytes from one scan of every row on `connection`.
fn scanned(connection: &rusqlite::Connection) -> (u64, u64) {
    connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(payload)), 0) FROM blob_payloads",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

/// The store's totals as `usage` reports them, checked against a fresh scan
/// and SQLite's integrity check, with the totals required to be in use.
fn assert_totals_exact(store: &FamilyStore, connection: &rusqlite::Connection, after: &str) {
    assert!(has_totals(store), "totals incomplete after {after}");
    let usage = store.usage().unwrap();
    assert_eq!(
        (usage.rows, usage.payload_bytes),
        scanned(connection),
        "totals differ from a scan after {after}"
    );
    assert_eq!(
        connection
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok",
        "after {after}"
    );
}

fn far_deadline() -> std::time::Instant {
    std::time::Instant::now() + Duration::from_secs(600)
}

fn semantic_store(dir: &Path, family: &str) -> FamilyStore {
    FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir, family),
        FamilyPlane::Semantic,
    )
    .unwrap()
}

fn put_rows(store: &FamilyStore, bytes: std::ops::RangeInclusive<u8>) {
    for byte in bytes {
        store
            .put_or_touch(
                &FamilyKey::new(FamilyPlane::Semantic, [byte; 32]),
                &vec![byte; 1_500 + 37 * usize::from(byte)],
            )
            .unwrap();
    }
}

fn total_changes_of(connection: &rusqlite::Connection) -> u64 {
    connection
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap()
}

/// Once set up, the exact total is read from the totals row with no scan,
/// stays equal to a scan through this code's puts, touches and deletes, and
/// through a `DELETE` with no `WHERE` (which SQLite would otherwise run as a
/// truncation that skips row triggers). Inserts and deletes fire a trigger;
/// an epoch-only update fires none.
#[test]
fn payload_totals_stay_exact_and_epoch_updates_fire_no_trigger() {
    let dir = tempfile::tempdir().unwrap();
    let store = semantic_store(dir.path(), "totals");
    put_rows(&store, 1..=200);
    assert!(!has_totals(&store));
    assert!(matches!(
        store.ensure_payload_totals(far_deadline()).unwrap(),
        PayloadTotals::Built { .. }
    ));
    assert_eq!(
        store.ensure_payload_totals(far_deadline()).unwrap(),
        PayloadTotals::Present
    );
    let check = rusqlite::Connection::open(store.path()).unwrap();
    take_logical_aggregates();
    assert_totals_exact(&store, &check, "setup");
    assert_eq!(
        take_logical_aggregates(),
        0,
        "the total was read, not summed"
    );

    let key = FamilyKey::new(FamilyPlane::Semantic, [201; 32]);
    let connection = || lock(&store.inner.connection);
    let before = total_changes_of(&connection());
    store.put_or_touch(&key, &[7; 9_000]).unwrap();
    assert_eq!(
        total_changes_of(&connection()) - before,
        2,
        "an insert updates the totals row"
    );
    assert_totals_exact(&store, &check, "an insert");

    let epoch = store.raise_epoch(1).unwrap();
    let before = total_changes_of(&connection());
    store.touch(&[key]).unwrap();
    store.put_or_touch(&key, &[7; 9_000]).unwrap();
    assert_eq!(
        total_changes_of(&connection()) - before,
        1,
        "an epoch-only update fires no trigger"
    );
    assert_totals_exact(&store, &check, "touches");

    let old = FamilyKey::new(FamilyPlane::Semantic, [3; 32]);
    let before = total_changes_of(&connection());
    assert!(store.delete_if_unreferenced_since(&old, epoch).unwrap());
    // The row, the totals row, and the deletion count of the store's
    // deletion epoch.
    assert_eq!(
        total_changes_of(&connection()) - before,
        3,
        "a delete updates the totals row and the deletion count"
    );
    assert_totals_exact(&store, &check, "a delete");

    connection()
        .execute("DELETE FROM blob_payloads", [])
        .unwrap();
    assert_totals_exact(&store, &check, "a DELETE with no WHERE");
    assert_eq!(scanned(&check), (0, 0));
}

/// A store with the totals stays consistent, and its totals exact, under
/// the exact statements AFT v0.60.0 and v0.61.0 run on a family store,
/// which predate the totals and use the same bundled SQLite
/// (libsqlite3-sys 0.30.1, SQLite 3.46.0): opening, put, put-or-touch,
/// touch, membership and payload reads, listing, the sum, the GC delete,
/// both releases' incremental vacuum, and a full VACUUM, which must keep
/// the triggers. They run on a separate connection, as another process
/// running an older binary would.
#[test]
fn payload_totals_stay_exact_under_releases_that_predate_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = semantic_store(dir.path(), "rollback");
    put_rows(&store, 1..=60);
    assert!(matches!(
        store.ensure_payload_totals(far_deadline()).unwrap(),
        PayloadTotals::Built { .. }
    ));

    let old = rusqlite::Connection::open(store.path()).unwrap();
    old.busy_timeout(Duration::from_secs(5)).unwrap();
    // Both releases open a store with these statements; the schema text is
    // unchanged since v0.60.0.
    old.pragma_update(None, "journal_mode", "WAL").unwrap();
    old.pragma_update(None, "synchronous", "NORMAL").unwrap();
    old.execute_batch(STORE_SCHEMA).unwrap();
    assert_totals_exact(&store, &old, "an older binary opened the store");

    let key = |byte: u8| [byte; 32];
    let schema = i64::from(FamilyPlane::Semantic.payload_schema());
    let insert = |byte: u8, len: usize| {
        let payload = vec![byte; len];
        old.execute(
            "INSERT INTO blob_payloads
                     (full_key, payload, payload_digest, payload_schema, ref_epoch, created_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                key(byte).as_slice(),
                &payload,
                blake3::hash(&payload).as_bytes().as_slice(),
                schema,
                0_i64,
                0_i64
            ],
        )
        .unwrap();
    };
    // v0.60.0 put: digest probe, then insert.
    assert!(old
        .query_row(
            "SELECT payload_digest FROM blob_payloads WHERE full_key = ?1",
            params![key(100).as_slice()],
            |_| Ok(()),
        )
        .optional()
        .unwrap()
        .is_none());
    insert(100, 70_000);
    assert_totals_exact(&store, &old, "a v0.60.0 put");
    // v0.61.0 put_or_touch: digest and epoch probe, then insert or reuse.
    assert!(old
        .query_row(
            "SELECT payload_digest, ref_epoch FROM blob_payloads WHERE full_key = ?1",
            params![key(101).as_slice()],
            |_| Ok(()),
        )
        .optional()
        .unwrap()
        .is_none());
    insert(101, 300);
    old.execute(
        "UPDATE blob_payloads SET ref_epoch = MAX(ref_epoch, ?2) WHERE full_key = ?1",
        params![key(100).as_slice(), 3_i64],
    )
    .unwrap();
    assert_totals_exact(&store, &old, "a v0.61.0 put_or_touch");
    // v0.61.0 touch: covering probe, then the same epoch update.
    let epoch: i64 = old
        .query_row(
            "SELECT ref_epoch FROM blob_payloads INDEXED BY blob_membership WHERE full_key = ?1",
            params![key(101).as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    old.execute(
        "UPDATE blob_payloads SET ref_epoch = MAX(ref_epoch, ?2) WHERE full_key = ?1",
        params![key(101).as_slice(), epoch + 1],
    )
    .unwrap();
    assert_totals_exact(&store, &old, "a touch");
    // Reads, listing and the release's own sum.
    for sql in [
        "SELECT 1 FROM blob_payloads WHERE full_key = ?1",
        "SELECT 1 FROM blob_payloads INDEXED BY blob_membership WHERE full_key = ?1",
        "SELECT payload, payload_digest, payload_schema FROM blob_payloads WHERE full_key = ?1",
    ] {
        assert!(old
            .query_row(sql, params![key(100).as_slice()], |_| Ok(()))
            .optional()
            .unwrap()
            .is_some());
    }
    let listed = old
        .prepare(
            "SELECT full_key, length(payload), ref_epoch FROM blob_payloads
             ORDER BY ref_epoch ASC, full_key ASC LIMIT ?1",
        )
        .unwrap()
        .query_map(params![i64::MAX], |row| row.get::<_, u64>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(listed.iter().sum::<u64>(), scanned(&old).1);
    // The GC delete, for several rows, as a sweep runs it.
    for byte in 1..=20_u8 {
        let deleted = old
            .execute(
                "DELETE FROM blob_payloads WHERE full_key = ?1 AND ref_epoch < ?2",
                params![key(byte).as_slice(), 1_i64],
            )
            .unwrap();
        assert_eq!(deleted, 1);
    }
    assert_totals_exact(&store, &old, "the GC deletes");
    // v0.61.0 reclaim steps incremental vacuum; v0.60.0 ran it whole and
    // then truncated the write-ahead log.
    {
        let mut statement = old.prepare("PRAGMA incremental_vacuum(16)").unwrap();
        let mut rows = statement.query([]).unwrap();
        while rows.next().unwrap().is_some() {}
    }
    assert_totals_exact(&store, &old, "v0.61.0 incremental vacuum");
    old.execute_batch("PRAGMA incremental_vacuum;").unwrap();
    old.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    assert_totals_exact(&store, &old, "v0.60.0 incremental vacuum");
    old.execute_batch("VACUUM").unwrap();
    assert_totals_exact(&store, &old, "VACUUM");
    insert(102, 5_000);
    assert_totals_exact(&store, &old, "a put after VACUUM");
}

/// Totals that are missing a part, or altered, are never used: the total
/// is summed from every row instead, and the next setup replaces them.
/// Without the table, the triggers that remain make every insert and delete
/// fail until setup replaces them; nothing but tampering removes the table,
/// since setup drops and recreates it inside one transaction.
#[test]
fn incomplete_or_altered_payload_totals_are_summed_and_rebuilt() {
    let tamperings = [
        "DROP TRIGGER payload_totals_delete",
        "DROP TRIGGER payload_totals_insert;
         CREATE TRIGGER payload_totals_insert AFTER INSERT ON blob_payloads BEGIN
             UPDATE payload_totals SET row_count = row_count + 1 WHERE singleton = 1;
         END",
        "DELETE FROM payload_totals",
        "DROP TABLE payload_totals",
    ];
    for tampering in tamperings {
        let dir = tempfile::tempdir().unwrap();
        let store = semantic_store(dir.path(), "tampered");
        put_rows(&store, 1..=40);
        store.ensure_payload_totals(far_deadline()).unwrap();
        assert!(has_totals(&store));
        lock(&store.inner.connection)
            .execute_batch(tampering)
            .unwrap();
        if tampering.starts_with("DROP TABLE") {
            assert!(store
                .put_or_touch(&FamilyKey::new(FamilyPlane::Semantic, [41; 32]), &[1; 10])
                .is_err());
        } else {
            put_rows(&store, 41..=45);
        }
        assert!(!has_totals(&store), "{tampering}");
        let check = rusqlite::Connection::open(store.path()).unwrap();
        take_logical_aggregates();
        let usage = store.usage().unwrap();
        assert_eq!(
            (usage.rows, usage.payload_bytes),
            scanned(&check),
            "{tampering}"
        );
        assert_eq!(
            take_logical_aggregates(),
            1,
            "{tampering}: summed, not read"
        );
        assert!(matches!(
            store.ensure_payload_totals(far_deadline()).unwrap(),
            PayloadTotals::Built { .. }
        ));
        put_rows(&store, 46..=50);
        assert_totals_exact(&store, &check, tampering);
    }
}

/// Setup waits only briefly for another writer, and a setup that outlives
/// its deadline is rolled back, leaves the store usable, and is not tried
/// again in this process.
#[test]
fn payload_totals_setup_skips_a_busy_store_and_gives_up_past_its_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let store = semantic_store(dir.path(), "busy");
    put_rows(&store, 1..=3);
    let other = rusqlite::Connection::open(store.path()).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = std::time::Instant::now();
    assert_eq!(
        store.ensure_payload_totals(far_deadline()).unwrap(),
        PayloadTotals::Busy
    );
    // This call includes scheduler pauses around SQLite's busy timeout. Keep
    // a finite bound, but allow two seconds beyond the product limit for a
    // delayed worker on a loaded CI host.
    const BUSY_WAIT_SCHEDULER_SLACK: Duration = Duration::from_secs(2);
    let waited = started.elapsed();
    assert!(
        waited < TOTALS_SETUP_BUSY_WAIT + BUSY_WAIT_SCHEDULER_SLACK,
        "it waited {waited:?}"
    );
    other.execute_batch("ROLLBACK").unwrap();
    drop(other);
    assert!(!has_totals(&store));

    let late = tempfile::tempdir().unwrap();
    let store = semantic_store(late.path(), "late");
    bulk_insert(
        &store,
        (0_u32..5_000).map(|index| {
            (
                *blake3::hash(&index.to_le_bytes()).as_bytes(),
                vec![7; 3_000],
            )
        }),
    );
    assert_eq!(
        store
            .ensure_payload_totals(std::time::Instant::now())
            .unwrap(),
        PayloadTotals::GaveUp
    );
    assert!(!has_totals(&store));
    assert_eq!(
        store.ensure_payload_totals(far_deadline()).unwrap(),
        PayloadTotals::GaveUp,
        "a setup that ran out of time is not retried in this process"
    );
    store
        .put_or_touch(&FamilyKey::new(FamilyPlane::Semantic, [9; 32]), &[9; 100])
        .unwrap();
    assert_eq!(store.usage().unwrap().rows, 5_001);
    assert!(!has_totals(&store));
}

/// Runs `pragma` on the store's own connection.
pub(crate) fn set_pragma(store: &FamilyStore, pragma: &str) {
    lock(&store.inner.connection).execute_batch(pragma).unwrap();
}

fn temp_store_of(inner: &StoreInner) -> i64 {
    lock(&inner.connection)
        .pragma_query_value(None, "temp_store", |row| row.get(0))
        .unwrap()
}

/// Both ways of opening a family store, for writing and read-only, give a
/// connection that keeps temporary storage (statement journals and sorts)
/// in memory.
#[test]
fn every_family_store_connection_keeps_temporary_storage_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let writer = semantic_store(dir.path(), "writer");
    assert_eq!(temp_store_of(&writer.inner), 2);
    drop(semantic_store(dir.path(), "reader"));
    let reader = FamilyStoreReader::open_existing(dir.path(), "reader", FamilyPlane::Semantic)
        .unwrap()
        .unwrap();
    assert_eq!(temp_store_of(&reader.inner), 2);
}

/// Of every statement a family store connection runs (puts, touches, reads,
/// quarantine, the totals and their setup, listings, deletes, segments and
/// space reclaim), only the oldest-first listing builds a temporary b-tree,
/// for its sort, which its row limit bounds. Kept in memory on these
/// connections, any other such b-tree would be a new memory cost.
#[test]
fn only_the_oldest_first_listing_builds_a_temporary_b_tree() {
    let dir = tempfile::tempdir().unwrap();
    let store = FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir.path(), "temp-trees"),
        FamilyPlane::Trigram,
    )
    .unwrap();
    let key = |byte: u8| FamilyKey::new(FamilyPlane::Trigram, [byte; 32]);
    let probe = ReadProbe::start(&store);
    for byte in 1..=50_u8 {
        store.put_or_touch(&key(byte), &vec![byte; 5_000]).unwrap();
    }
    store.put_or_touch(&key(1), &vec![1; 5_000]).unwrap();
    store.touch(&[key(1), key(2)]).unwrap();
    let epoch = store.raise_epoch(1).unwrap();
    store.touch(&[key(1), key(2)]).unwrap();
    store.put_or_touch(&key(3), &vec![3; 5_000]).unwrap();
    store.get(&key(4)).unwrap();
    store.contains(&key(4)).unwrap();
    store.quarantine(&key(99)).unwrap();
    store.usage().unwrap();
    store.ensure_payload_totals(far_deadline()).unwrap();
    store.usage().unwrap();
    store.rows_oldest(10).unwrap();
    store.delete_if_unreferenced_since(&key(5), epoch).unwrap();
    let segment = [7_u8; 32];
    store.begin_segment(&segment).unwrap();
    store.finish_segment(&segment, 10).unwrap();
    store.touch_segment(&segment).unwrap();
    store.segment(&segment).unwrap();
    store.segments().unwrap();
    store
        .delete_segment_if_unreferenced_since(&segment, epoch + 1, &dir.path().join("absent"))
        .unwrap();
    store.reclaim_space().unwrap();
    store.payload_bytes_upper_bound().unwrap();
    store.gc_epoch().unwrap();
    let statements = probe.finish().statements;
    assert!(statements.len() > 20, "{statements:?}");

    let connection = lock(&store.inner.connection);
    let mut temp_trees = Vec::new();
    for sql in statements.keys() {
        let Ok(mut plan) = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}")) else {
            continue;
        };
        let mut rows = plan.raw_query();
        while let Some(row) = rows.next().unwrap() {
            let detail: String = row.get(3).unwrap();
            if detail.contains("TEMP B-TREE") {
                temp_trees.push((sql.trim().to_string(), detail));
            }
        }
    }
    assert!(
        !temp_trees.is_empty()
            && temp_trees
                .iter()
                .all(|(sql, _)| sql.starts_with("SELECT p.full_key, length(p.payload)")),
        "{temp_trees:#?}"
    );
}

/// Pages of one b-tree in the store, from SQLite's `dbstat` table.
pub(crate) fn btree_pages(store: &FamilyStore, name: &str) -> i64 {
    lock(&store.inner.connection)
        .query_row(
            "SELECT COUNT(*) FROM dbstat WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .unwrap()
}

pub(crate) fn describe(store: &FamilyStore) {
    let connection = lock(&store.inner.connection);
    for pragma in ["page_size", "page_count", "cache_size", "mmap_size"] {
        let value: i64 = connection
            .query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))
            .unwrap();
        eprintln!("{pragma}={value}");
    }
    let sizes: (i64, i64, i64, f64, i64) = connection
        .query_row(
            "SELECT COUNT(*), MIN(length(payload)), MAX(length(payload)), AVG(length(payload)),
         SUM(length(payload) > 1002) FROM blob_payloads",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    eprintln!("payloads (count, min, max, mean bytes, over 4KiB index-local limit): {sizes:?}");
    for sql in [
        "SELECT payload, payload_digest, payload_schema FROM blob_payloads WHERE full_key = ?1",
        "SELECT payload_digest FROM blob_payloads WHERE full_key = ?1",
        "UPDATE blob_payloads SET ref_epoch = MAX(ref_epoch, ?2) WHERE full_key = ?1",
        "SELECT 1 FROM blob_payloads WHERE full_key = ?1",
        "SELECT ref_epoch FROM blob_payloads INDEXED BY blob_membership WHERE full_key = ?1",
    ] {
        let mut stmt = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let mut rows = stmt.raw_query();
        while let Some(row) = rows.next().unwrap() {
            eprintln!("EQP {sql}: {}", row.get::<_, String>(3).unwrap());
        }
    }
}

fn cache_misses(connection: &rusqlite::Connection, reset: bool) -> i32 {
    let (mut current, mut high) = (0, 0);
    // The caller holds the store's connection lock for the entire measurement.
    let result = unsafe {
        rusqlite::ffi::sqlite3_db_status(
            connection.handle(),
            rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS,
            &mut current,
            &mut high,
            i32::from(reset),
        )
    };
    assert_eq!(result, rusqlite::ffi::SQLITE_OK);
    current
}

#[test]
fn contains_query_plan_uses_covering_membership_index() {
    let dir = tempfile::tempdir().unwrap();
    let store = FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir.path(), "membership"),
        FamilyPlane::Semantic,
    )
    .unwrap();
    let key = FamilyKey::new(FamilyPlane::Semantic, [1; 32]);
    store.put_or_touch(&key, &vec![42; 16 * 1024]).unwrap();
    let probe = ReadProbe::start(&store);
    assert!(store.contains(&key).unwrap());
    assert!(!store
        .contains(&FamilyKey::new(FamilyPlane::Semantic, [2; 32]))
        .unwrap());
    let counts = probe.finish();
    let queries = counts
        .statements
        .keys()
        .filter(|sql| sql.starts_with("SELECT 1 FROM blob_payloads"))
        .collect::<Vec<_>>();
    assert_eq!(queries.len(), 1, "{counts:?}");
    let connection = lock(&store.inner.connection);
    let mut statement = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {}", queries[0]))
        .unwrap();
    let plans = statement
        .query_map(params![key.as_bytes().as_slice()], |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        plans
            .iter()
            .any(|plan| plan.contains("USING COVERING INDEX blob_membership")),
        "contains must use the payload-free membership index: {plans:?}"
    );
}

fn total_changes(store: &FamilyStore) -> u64 {
    lock(&store.inner.connection)
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn touch_same_epoch_opens_no_write_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let store = FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir.path(), "touch-no-write"),
        FamilyPlane::Semantic,
    )
    .unwrap();
    let key = FamilyKey::new(FamilyPlane::Semantic, [1; 32]);
    store.put_or_touch(&key, b"payload").unwrap();
    take_touch_transactions();
    let before = total_changes(&store);
    assert_eq!(store.touch(&[key, key]).unwrap().touched, 2);
    assert_eq!(store.touch(&[]).unwrap().touched, 0);
    assert_eq!(total_changes(&store), before);
    assert_eq!(
        take_touch_transactions(),
        0,
        "same-epoch touch opened a writer"
    );
}

#[test]
fn touch_old_epoch_updates_each_row_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir.path(), "touch-advance"),
        FamilyPlane::Semantic,
    )
    .unwrap();
    let key = FamilyKey::new(FamilyPlane::Semantic, [1; 32]);
    let missing = FamilyKey::new(FamilyPlane::Semantic, [2; 32]);
    store.put_or_touch(&key, b"payload").unwrap();
    let epoch = store.raise_epoch(1).unwrap();
    take_touch_transactions();
    let before = total_changes(&store);
    let report = store.touch(&[key, missing, key]).unwrap();
    assert_eq!(report.ref_epoch, epoch);
    assert_eq!(report.touched, 2);
    assert_eq!(report.missing, vec![missing]);
    assert_eq!(total_changes(&store) - before, 1);
    assert_eq!(take_touch_transactions(), 1);
    assert!(!store.delete_if_unreferenced_since(&key, epoch).unwrap());
    store.touch(&[key, missing]).unwrap();
    assert_eq!(take_touch_transactions(), 0);
}

#[test]
fn put_or_touch_same_epoch_reuse_writes_no_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir.path(), "reuse"),
        FamilyPlane::Semantic,
    )
    .unwrap();
    let key = FamilyKey::new(FamilyPlane::Semantic, [1; 32]);
    store.put_or_touch(&key, b"payload").unwrap();
    let before = total_changes(&store);
    assert_eq!(
        store.put_or_touch(&key, b"payload").unwrap(),
        PutOrTouch::Reused { ref_epoch: 0 }
    );
    assert_eq!(
        total_changes(&store) - before,
        0,
        "same-epoch reuse wrote a row"
    );
}

#[test]
fn put_or_touch_older_epoch_reuse_advances_and_prevents_collection() {
    let dir = tempfile::tempdir().unwrap();
    let store = FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir.path(), "advance"),
        FamilyPlane::Semantic,
    )
    .unwrap();
    let key = FamilyKey::new(FamilyPlane::Semantic, [1; 32]);
    store.put_or_touch(&key, b"payload").unwrap();
    let epoch = store.raise_epoch(1).unwrap();
    let before = total_changes(&store);
    assert_eq!(
        store.put_or_touch(&key, b"payload").unwrap(),
        PutOrTouch::Reused { ref_epoch: epoch }
    );
    assert_eq!(total_changes(&store) - before, 1);
    assert!(!store.delete_if_unreferenced_since(&key, epoch).unwrap());
    assert!(store.delete_if_unreferenced_since(&key, epoch + 1).unwrap());
}

#[test]
fn put_or_touch_digest_conflict_does_not_write_or_advance_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let store = FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir.path(), "conflict"),
        FamilyPlane::Semantic,
    )
    .unwrap();
    let key = FamilyKey::new(FamilyPlane::Semantic, [1; 32]);
    store.put_or_touch(&key, b"payload").unwrap();
    let epoch = store.raise_epoch(1).unwrap();
    let before = total_changes(&store);
    assert!(
        matches!(store.put_or_touch(&key, b"different"), Err(StoreError::ConflictingPayload(k)) if k == key)
    );
    assert_eq!(total_changes(&store), before);
    assert_eq!(store.get(&key).unwrap().as_deref(), Some(&b"payload"[..]));
    assert!(store.delete_if_unreferenced_since(&key, epoch).unwrap());
}

#[test]
fn payload_upper_bound_includes_uncheckpointed_wal_pages() {
    let dir = tempfile::tempdir().unwrap();
    let store = FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir.path(), "wal-bound"),
        FamilyPlane::Semantic,
    )
    .unwrap();
    lock(&store.inner.connection)
        .execute_batch("PRAGMA wal_autocheckpoint = 0; PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let main_bytes = std::fs::metadata(store.path()).unwrap().len();
    let key = FamilyKey::new(FamilyPlane::Semantic, [1; 32]);
    let payload = vec![42; 1024 * 1024];
    store.put_or_touch(&key, &payload).unwrap();
    assert_eq!(std::fs::metadata(store.path()).unwrap().len(), main_bytes);
    let bound = store.payload_bytes_upper_bound().unwrap();
    assert!(bound > main_bytes);
    assert!(bound >= payload.len() as u64);
    assert!(bound >= store.usage().unwrap().payload_bytes);
}

#[test]
fn current_epoch_semantic_touch_reads_only_membership_index() {
    let dir = tempfile::tempdir().unwrap();
    let access = StoreWriteAccess::for_registered_view(dir.path(), "read-budget");
    let store = FamilyStore::open(&access, FamilyPlane::Semantic).unwrap();
    let payload = vec![42; 16 * 1024];
    {
        let mut connection = lock(&store.inner.connection);
        let tx = connection.transaction().unwrap();
        for i in 0u32..2048 {
            let key = blake3::hash(&i.to_le_bytes());
            tx.execute(
                "INSERT INTO blob_payloads VALUES (?1, ?2, ?3, 1, 0, 0)",
                params![
                    key.as_bytes().as_slice(),
                    &payload,
                    blake3::hash(&payload).as_bytes().as_slice()
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        connection.execute_batch("PRAGMA shrink_memory").unwrap();
        cache_misses(&connection, true);
    }
    let keys = (0u32..2048)
        .map(|i| {
            FamilyKey::new(
                FamilyPlane::Semantic,
                *blake3::hash(&i.to_le_bytes()).as_bytes(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(store.touch(&keys).unwrap().touched, keys.len());
    let misses = cache_misses(&lock(&store.inner.connection), false);
    eprintln!("semantic current-epoch touch: 2048 keys, {misses} cache misses");
    assert!(
        misses < 128,
        "current-epoch touch read {misses} pages; only the narrow membership index should be read"
    );
}

#[test]
fn touch_preserves_newer_epochs_and_reports_missing_and_duplicate_keys() {
    let dir = tempfile::tempdir().unwrap();
    let store = FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir.path(), "epochs"),
        FamilyPlane::Semantic,
    )
    .unwrap();
    let present = FamilyKey::new(FamilyPlane::Semantic, [1; 32]);
    let missing = FamilyKey::new(FamilyPlane::Semantic, [2; 32]);
    store.put_or_touch(&present, b"payload").unwrap();
    lock(&store.inner.connection)
        .execute(
            "UPDATE blob_payloads SET ref_epoch = 10 WHERE full_key = ?1",
            params![present.as_bytes().as_slice()],
        )
        .unwrap();
    let report = store.touch(&[present, missing, present, missing]).unwrap();
    assert_eq!(report.touched, 2);
    assert_eq!(report.missing, vec![missing, missing]);
    assert_eq!(report.ref_epoch, 0);
    let epoch: i64 = lock(&store.inner.connection)
        .query_row(
            "SELECT ref_epoch FROM blob_payloads WHERE full_key = ?1",
            params![present.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(epoch, 10);
    assert_eq!(
        store.get(&present).unwrap().as_deref(),
        Some(&b"payload"[..])
    );
}

#[test]
fn deletion_epoch_counts_each_deleted_row_from_any_writer() {
    let dir = tempfile::tempdir().unwrap();
    let store = FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir.path(), "deletions"),
        FamilyPlane::Semantic,
    )
    .unwrap();
    let start = store.deletion_epoch().unwrap().expect("set up on open");
    let keys = (1u8..=4)
        .map(|byte| FamilyKey::new(FamilyPlane::Semantic, [byte; 32]))
        .collect::<Vec<_>>();
    for key in &keys {
        store.put_or_touch(key, b"payload").unwrap();
    }
    // Inserts, reuse and epoch touches delete nothing.
    store.put_or_touch(&keys[0], b"payload").unwrap();
    let epoch = store.raise_epoch(5).unwrap();
    store.touch(&keys).unwrap();
    assert_eq!(store.deletion_epoch().unwrap(), Some(start));
    let count =
        |epoch: DeletionEpoch| u64::from_le_bytes(epoch.to_bytes()[16..].try_into().unwrap());
    assert!(store
        .delete_if_unreferenced_since(&keys[0], epoch + 1)
        .unwrap());
    // A refused conditional delete removes no row and counts nothing.
    assert!(!store.delete_if_unreferenced_since(&keys[1], 0).unwrap());
    assert_eq!(
        count(store.deletion_epoch().unwrap().unwrap()),
        count(start) + 1
    );
    // v0.61.0's delete statement, run by a connection that knows nothing of
    // the counter, is counted by the trigger in the store's schema.
    let other = rusqlite::Connection::open(store.path()).unwrap();
    assert_eq!(
        other
            .execute(
                "DELETE FROM blob_payloads WHERE full_key = ?1 AND ref_epoch < ?2",
                params![keys[1].as_bytes().as_slice(), i64::MAX],
            )
            .unwrap(),
        1
    );
    // So is a delete of several rows in one statement, once per row.
    other
        .execute(
            "DELETE FROM blob_payloads WHERE full_key IN (?1, ?2)",
            params![keys[2].as_bytes().as_slice(), keys[3].as_bytes().as_slice()],
        )
        .unwrap();
    let after = store.deletion_epoch().unwrap().unwrap();
    assert_eq!(count(after), count(start) + 4);
    assert_eq!(after.to_bytes()[..16], start.to_bytes()[..16]);
    // A DELETE with no WHERE, which SQLite runs as a truncation only when
    // the table has no delete trigger, is counted row by row too.
    for byte in 5u8..=7 {
        store
            .put_or_touch(
                &FamilyKey::new(FamilyPlane::Semantic, [byte; 32]),
                b"payload",
            )
            .unwrap();
    }
    other.execute("DELETE FROM blob_payloads", []).unwrap();
    assert_eq!(
        count(store.deletion_epoch().unwrap().unwrap()),
        count(start) + 7
    );
    let integrity: String = other
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
}

#[test]
fn damaged_deletion_epoch_reads_as_none_and_rebuilds_under_a_new_generation() {
    let dir = tempfile::tempdir().unwrap();
    let store = FamilyStore::open(
        &StoreWriteAccess::for_registered_view(dir.path(), "damaged-deletions"),
        FamilyPlane::Semantic,
    )
    .unwrap();
    let original = store.deletion_epoch().unwrap().unwrap();
    let other = rusqlite::Connection::open(store.path()).unwrap();
    for damage in [
        "DROP TRIGGER payload_deletions_count",
        "UPDATE payload_deletions SET count = -1",
        "UPDATE payload_deletions SET generation = X'00'",
        "UPDATE payload_deletions SET generation = 'text'",
        "DELETE FROM payload_deletions",
        "DROP TABLE payload_deletions",
    ] {
        store.rebuild_deletion_epoch_for_test();
        assert!(store.deletion_epoch().unwrap().is_some());
        other.execute_batch(damage).unwrap();
        assert_eq!(store.deletion_epoch().unwrap(), None, "{damage}");
    }
    // A rebuilt counter never repeats an earlier value, even from count 0.
    store.rebuild_deletion_epoch_for_test();
    let rebuilt = store.deletion_epoch().unwrap().unwrap();
    assert_ne!(rebuilt.to_bytes()[..16], original.to_bytes()[..16]);
    // Setup keeps a valid counter it finds.
    set_up_deletion_epoch(&mut lock(&store.inner.connection)).unwrap();
    assert_eq!(store.deletion_epoch().unwrap(), Some(rebuilt));
}
