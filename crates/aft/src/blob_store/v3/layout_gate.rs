//! Isolated storage-layout acquisition; the Python harness supplies immutable corpus bytes.
#![cfg(unix)]
use super::*;
use crate::blob_store::{BlobPlane, BlobStore, FullKey};
use serde_json::json;

const FAMILY: &str = "0123456789abcdef";
const LOOKUP: &str = "SELECT payload_digest FROM blob_payloads WHERE full_key = ?1";

fn key(i: u8) -> FamilyKey {
    let mut bytes = [7; 32];
    bytes[31] = i;
    FamilyKey::new(FamilyPlane::Semantic, bytes)
}

#[cfg(unix)]
fn cpu_ns() -> u64 {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, time.as_mut_ptr()) },
        0
    );
    let time = unsafe { time.assume_init() };
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

#[cfg(unix)]
fn measure(storage: &Path) -> serde_json::Value {
    // Drop every acquisition handle before this fresh read-only open.
    let reader = PlaneReader::open_existing(storage, FAMILY, FamilyPlane::Semantic).unwrap();
    let c = reader.database.connection().unwrap();
    let locator: String = c.query_row("SELECT CASE WHEN origin='pk' THEN 'blob_payloads' ELSE name END FROM pragma_index_list('blob_payloads') WHERE origin='pk' OR name='blob_locator'", [], |r| r.get(0)).unwrap();
    let overflow: u64 = c
        .query_row(
            "SELECT count(*) FROM dbstat WHERE name=?1 AND pagetype='overflow'",
            [&locator],
            |r| r.get(0),
        )
        .unwrap();
    let mut statement = c.prepare(LOOKUP).unwrap();
    let mut batches = Vec::new();
    for batch in 0..12 {
        statement.reset_status(rusqlite::StatementStatus::VmStep);
        statement.reset_status(rusqlite::StatementStatus::FullscanStep);
        let cpu = cpu_ns();
        let wall = Instant::now();
        for i in 0..256 {
            let probe = key((i % 64) as u8);
            let digest = statement
                .query_row([probe.as_bytes().as_slice()], |r| r.get::<_, Vec<u8>>(0))
                .optional()
                .unwrap();
            assert_eq!(digest.is_some(), i % 64 < 32);
        }
        let wall_ns = wall.elapsed().as_nanos() as u64;
        let cpu_ns = cpu_ns() - cpu;
        let row = json!({"wall_ns":wall_ns,"cpu_ns":cpu_ns,
            "vm_steps":statement.get_status(rusqlite::StatementStatus::VmStep),
            "full_scan_steps":statement.get_status(rusqlite::StatementStatus::FullscanStep)});
        if batch != 0 {
            batches.push(row);
        }
    }
    let mut walls: Vec<_> = batches
        .iter()
        .map(|r| r["wall_ns"].as_u64().unwrap())
        .collect();
    walls.sort_unstable();
    json!({"layout_tag":c.query_row("SELECT layout_tag FROM format_meta",[],|r|r.get::<_,u32>(0)).unwrap(),
        "sqlite_version":rusqlite::version(),"sqlite_source_id":c.query_row("SELECT sqlite_source_id()",[],|r|r.get::<_,String>(0)).unwrap(),
        "statement":LOOKUP,"probes_per_batch":256,"present_probes":128,"absent_probes":128,
        "discarded_warmup_batches":1,"batches":batches,"median_wall_ns":walls[5],
        "locator_index":locator,"locator_overflow_pages":overflow})
}

#[test]
#[ignore = "run by scripts/views-performance-gate.py --rowid-layout"]
#[cfg(unix)]
fn rowid_layout_receipt() {
    let _guard = tests::serial();
    let corpus = fs::read_to_string(
        std::env::var_os("AFT_V3_GATE_PAYLOAD").expect("harness corpus payload"),
    )
    .unwrap()
    .into_bytes();
    assert!(!corpus.is_empty());
    let old = tempfile::tempdir().unwrap();
    let new = tempfile::tempdir().unwrap();
    CREATE_3A_LAYOUT.set(true);
    fill(old.path(), &corpus);
    CREATE_3A_LAYOUT.set(false);
    fill(new.path(), &corpus);
    for i in 0..32 {
        let expected: Vec<u8> = corpus.iter().copied().cycle().take(1024 * 1024).collect();
        for root in [old.path(), new.path()] {
            let reader = PlaneReader::open_existing(root, FAMILY, FamilyPlane::Semantic).unwrap();
            assert_eq!(reader.get(&key(i)).unwrap(), Some(expected.clone()));
        }
    }
    let old_result = measure(old.path());
    let new_result = measure(new.path());
    let speedup = old_result["median_wall_ns"].as_f64().unwrap()
        / new_result["median_wall_ns"].as_f64().unwrap();
    let receipt = json!({"schema":1,"old":old_result,"rowid":new_result,"speedup":speedup,"byte_identical_keys":32});
    fs::write(
        std::env::var_os("AFT_V3_GATE_RECEIPT").expect("harness receipt path"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
    eprintln!("v3 lookup speedup: {speedup:.2}x");
    assert!(
        speedup >= 10.0,
        "rowid lookup speedup {speedup:.2}x is below 10x"
    );
    assert_eq!(receipt["rowid"]["locator_overflow_pages"], 0);
    assert!(receipt["old"]["locator_overflow_pages"].as_u64().unwrap() > 0);
}

#[cfg(unix)]
fn fill(storage: &Path, corpus: &[u8]) {
    let mut source = BlobStore::open(storage, FAMILY, BlobPlane::Semantic).unwrap();
    fs::create_dir_all(storage.join("views/holder/pins")).unwrap();
    for i in 0..32 {
        let payload: Vec<u8> = corpus.iter().copied().cycle().take(1024 * 1024).collect();
        source
            .put(
                &FullKey {
                    bytes: *key(i).as_bytes(),
                    plane: BlobPlane::Semantic,
                },
                &payload,
            )
            .unwrap();
        assert_eq!(
            copy_on_first_use(
                storage,
                CopySource::V1 {
                    artifact_key: FAMILY,
                    plane: BlobPlane::Semantic,
                    key: *key(i).as_bytes()
                },
                "holder",
                None
            )
            .unwrap(),
            CopyOutcome::Copied
        );
    }
}
