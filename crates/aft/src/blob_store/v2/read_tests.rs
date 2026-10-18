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
