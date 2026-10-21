use super::*;
use std::sync::{MutexGuard, OnceLock};
use tempfile::tempdir;

const A: &str = "0123456789abcdef";
const B: &str = "fedcba9876543210";
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    set_free_space_for_test(u64::MAX, u64::MAX / 2);
    guard
}
fn key() -> FamilyKey {
    FamilyKey::new(FamilyPlane::Semantic, [7; 32])
}
fn seed(storage: &Path, family: &str) {
    publish(storage, family, 1024, None, |p| {
        p.put(&key(), b"v3 data", 1)
    })
    .unwrap();
}
#[cfg(unix)]
fn inventory(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                inventory(&path)
            } else {
                meta.blocks() * 512
            }
        })
        .sum()
}
fn db(storage: &Path, family: &str) -> PathBuf {
    family_dir(storage, family).unwrap().join("members.sqlite")
}

#[test]
fn floor_refusal_before_creation_and_budget_recheck_leave_no_plane_or_row() {
    let _guard = serial();
    let root = tempdir().unwrap();
    set_free_space_for_test(NAMESPACE_BUDGET - 1, 8 * NAMESPACE_BUDGET);
    let error = publish(root.path(), A, 1, None, |_| Ok(())).unwrap_err();
    assert_eq!(error.to_string(), "device_free_space_floor");
    assert!(!root.path().join("blobs/v3").exists());
    assert!(!root.path().join("reader-floor.json").exists());
    set_free_space_for_test(u64::MAX, u64::MAX / 2);
    let error = publish(root.path(), A, NAMESPACE_BUDGET, None, |_| -> Result<()> {
        panic!("budget must refuse before body")
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "v3_namespace_budget");
    let ledger = LedgerReader::open_existing(root.path(), A).unwrap();
    assert_eq!(ledger.accounting().unwrap().reserved_bytes, 0);
    assert_eq!(
        ledger
            .connection
            .query_row("SELECT COUNT(*) FROM reservations", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert!(!plane_path(root.path(), A, FamilyPlane::Semantic)
        .unwrap()
        .exists());
}

struct Recheck;
impl DurabilityObserver for Recheck {
    fn reached(&self, step: DurabilityStep) {
        if step == DurabilityStep::V3FloorRaised {
            set_free_space_for_test(NAMESPACE_BUDGET, 8 * NAMESPACE_BUDGET);
        }
    }
}
#[test]
fn floor_recheck_refuses_without_reservation_or_plane_store() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let error = publish(root.path(), A, 1, Some(&Recheck), |_| -> Result<()> {
        panic!("floor must refuse")
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "device_free_space_floor");
    assert!(db(root.path(), A).exists());
    let ledger = LedgerReader::open_existing(root.path(), A).unwrap();
    assert_eq!(
        ledger
            .connection
            .query_row("SELECT COUNT(*) FROM reservations", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert!(!plane_path(root.path(), A, FamilyPlane::Semantic)
        .unwrap()
        .exists());
}

#[test]
fn sequential_reservations_sum_other_family_without_mutating_it() {
    let _guard = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut a = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let reservation = a.reserve(100).unwrap();
    let before = a.accounting().unwrap();
    set_free_space_for_test(NAMESPACE_BUDGET + 150, 8 * NAMESPACE_BUDGET);
    let started = std::time::Instant::now();
    let error = publish_locked(root.path(), B, 100, &lock, None, |_| Ok(())).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(error.to_string(), "device_free_space_floor");
    assert_eq!(a.accounting().unwrap(), before);
    assert_eq!(
        LedgerReader::open_existing(root.path(), A)
            .unwrap()
            .accounting()
            .unwrap(),
        before
    );
    a.release(reservation).unwrap();
}

#[test]
fn unreadable_peer_ledger_refuses_instead_of_ignoring_reserved_bytes() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let dir = family_dir(root.path(), A).unwrap();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("members.sqlite"), b"not sqlite").unwrap();
    let error = publish(root.path(), B, 100, None, |_| Ok(())).unwrap_err();
    assert_eq!(error.to_string(), "reservation_ledger_unreadable");
    assert!(!db(root.path(), B).exists());
}

#[test]
fn publication_observes_floor_before_files_and_flush_before_commit() {
    let _guard = serial();
    let root = tempdir().unwrap();
    struct Observer<'a>(&'a Path, std::cell::RefCell<Vec<DurabilityStep>>);
    impl DurabilityObserver for Observer<'_> {
        fn reached(&self, step: DurabilityStep) {
            self.1.borrow_mut().push(step);
            match step {
                DurabilityStep::V3FloorRaised => {
                    assert!(!self.0.join("blobs/v3").exists());
                    let floor = fs::read_to_string(self.0.join("reader-floor.json")).unwrap();
                    assert!(floor.contains("views_v3_registry"));
                    assert!(floor.contains("views_v3_store"));
                }
                DurabilityStep::ClosureFlushed => {
                    let ledger = LedgerReader::open_existing(self.0, A).unwrap();
                    assert!(ledger.accounting().unwrap().reserved_bytes > 0);
                    assert_eq!(
                        PlaneReader::open_existing(self.0, A, FamilyPlane::Semantic)
                            .unwrap()
                            .get(&key())
                            .unwrap(),
                        Some(b"v3 data".to_vec())
                    );
                }
                DurabilityStep::BlobCommitted
                | DurabilityStep::SegmentRowRecorded
                | DurabilityStep::SegmentFileSynced
                | DurabilityStep::SegmentDurable
                | DurabilityStep::ManifestWritten
                | DurabilityStep::ManifestParentSynced
                | DurabilityStep::PointerCas
                | DurabilityStep::PointerSynced => panic!("v2 boundary in v3 publication"),
            }
        }
    }
    let observer = Observer(root.path(), std::cell::RefCell::new(Vec::new()));
    publish(root.path(), A, 1024, Some(&observer), |p| {
        p.put(&key(), b"v3 data", 1)
    })
    .unwrap();
    assert_eq!(
        *observer.1.borrow(),
        vec![
            DurabilityStep::V3FloorRaised,
            DurabilityStep::ClosureFlushed
        ]
    );
    for step in DurabilityStep::V3_ONLY {
        assert!(DurabilityStep::ALL.contains(&step));
        assert_eq!(DurabilityStep::parse(step.as_str()), Some(step));
    }
    assert_eq!(
        DurabilityStep::parse("v3-floor-raised"),
        Some(DurabilityStep::V3FloorRaised)
    );
    assert_eq!(
        DurabilityStep::parse("closure-flushed"),
        Some(DurabilityStep::ClosureFlushed)
    );
    assert!(!root.path().join("blobs/v2").exists());
}

#[cfg(unix)]
#[test]
fn accounting_matches_independent_inventory_after_twenty_reuse_cycles() {
    let _guard = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    for _ in 0..20 {
        publish(root.path(), A, 1024, None, |p| p.put(&key(), b"v3 data", 1)).unwrap();
        let lock = ReservationLock::acquire(root.path()).unwrap();
        let writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
        assert_eq!(
            writer.accounting().unwrap().usage(),
            inventory(&family_dir(root.path(), A).unwrap())
        );
    }
}

#[cfg(unix)]
#[test]
fn registry_dominated_growth_is_charged_and_deletion_waits_for_measurement() {
    let _guard = serial();
    let root = tempdir().unwrap();
    publish(root.path(), A, 2 * 1024 * 1024, None, |p| {
        for i in 0..1000 {
            p.registry
                .register_member(&format!("scope-{i}"), "base", None)?;
        }
        Ok(())
    })
    .unwrap();
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let baseline = writer.accounting().unwrap().usage();
    assert!(baseline > 100_000);
    assert_eq!(baseline, inventory(&writer.dir));
    let file = writer.dir.join("garbage");
    fs::write(&file, vec![3; 1024 * 1024]).unwrap();
    writer.covering_measurement().unwrap();
    let larger = writer.accounting().unwrap().usage();
    assert_eq!(larger, inventory(&writer.dir));
    fs::remove_file(file).unwrap();
    assert_eq!(writer.accounting().unwrap().usage(), larger);
    writer.covering_measurement().unwrap();
    assert_eq!(writer.accounting().unwrap().usage(), baseline);
}

#[cfg(unix)]
#[test]
fn failed_plane_commit_releases_reservation_but_keeps_orphan_charge() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let error = publish(root.path(), A, 2 * 1024 * 1024, None, |p| {
        p.put(&key(), &vec![5; 1024 * 1024], 1)?;
        Err::<(), _>(Error::Refused("injected_failure"))
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "injected_failure");
    let released = LedgerReader::open_existing(root.path(), A)
        .unwrap()
        .accounting()
        .unwrap();
    assert_eq!(released.reserved_bytes, 0);
    assert!(released.usage() >= inventory(&family_dir(root.path(), A).unwrap()));
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let usage = writer.accounting().unwrap();
    assert_eq!(usage.reserved_bytes, 0);
    assert_eq!(usage.usage(), inventory(&writer.dir));
    assert!(usage.usage() >= 1024 * 1024);
    let error = publish_locked(
        root.path(),
        A,
        NAMESPACE_BUDGET - 1024 * 1024,
        &lock,
        None,
        |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "v3_namespace_budget");
    assert_eq!(
        PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic)
            .unwrap()
            .get(&key())
            .unwrap()
            .unwrap()
            .len(),
        1024 * 1024
    );
}

fn check_drift(sql: &str, expected: &str) {
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let path = db(root.path(), A);
    let c = Connection::open(&path).unwrap();
    c.execute_batch(sql).unwrap();
    drop(c);
    let error = LedgerReader::open_existing(root.path(), A)
        .err()
        .expect("must refuse");
    assert!(error.to_string().ends_with(expected), "{error}");
    assert!(UnsupportedPersistedFormat::is_refusal_message(
        &error.to_string()
    ));
}
#[test]
fn stored_schema_drift_is_refused() {
    let _g = serial();
    check_drift("UPDATE format_meta SET schema_text='drift'", "schema_drift");
}
#[test]
fn added_schema_object_is_refused() {
    let _g = serial();
    check_drift("CREATE VIEW extra AS SELECT 1", "schema_drift");
}
#[test]
fn application_id_swap_is_refused() {
    let _g = serial();
    check_drift(
        &format!("PRAGMA application_id={PLANE_APPLICATION_ID}"),
        "application_id",
    );
}
#[test]
fn unknown_layout_tag_is_refused() {
    let _g = serial();
    check_drift("UPDATE format_meta SET layout_tag=99", "layout_tag:99");
}
#[test]
fn pragma_mismatch_is_refused_by_name() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    BAD_PRAGMA.with(|bad| bad.set(Some("query_only")));
    let error = LedgerReader::open_existing(root.path(), A)
        .err()
        .expect("must refuse");
    BAD_PRAGMA.with(|bad| bad.set(None));
    assert!(error.to_string().ends_with("pragma:query_only"), "{error}");
}
#[test]
fn drift_refusal_is_cleared_after_valid_replacement_and_never_becomes_floor() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let path = db(root.path(), A);
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE VIEW extra AS SELECT 1")
        .unwrap();
    let old = LedgerReader::open_existing(root.path(), A)
        .err()
        .unwrap()
        .to_string();
    assert!(persisted_format::refusal_covering(PersistedStore::ViewsV3Registry, &path).is_some());
    let replacement = tempdir().unwrap();
    seed(replacement.path(), A);
    fs::copy(db(replacement.path(), A), &path).unwrap();
    LedgerReader::open_existing(root.path(), A).unwrap();
    assert!(persisted_format::refusal_covering(PersistedStore::ViewsV3Registry, &path).is_none());
    assert!(UnsupportedPersistedFormat::is_refusal_message(&old));
}

fn child(storage: &Path, family: &str, mode: &str, control: &Path) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "blob_store::v3::tests::v3_child", "--nocapture"])
        .env("AFT_V3_TEST_STORAGE", storage)
        .env("AFT_V3_TEST_FAMILY", family)
        .env("AFT_V3_TEST_MODE", mode)
        .env("AFT_V3_TEST_CONTROL", control)
        .env("AFT_V3_TEST_ADMISSION_CONTROL", control.parent().unwrap())
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}
fn wait_file(path: &Path, child: &mut std::process::Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before {}",
            path.display()
        );
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("child never reached {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn v3_child() {
    let Some(storage) = std::env::var_os("AFT_V3_TEST_STORAGE") else {
        return;
    };
    let storage = PathBuf::from(storage);
    let family = std::env::var("AFT_V3_TEST_FAMILY").unwrap();
    let mode = std::env::var("AFT_V3_TEST_MODE").unwrap();
    let control = PathBuf::from(std::env::var_os("AFT_V3_TEST_CONTROL").unwrap());
    set_free_space_for_test(u64::MAX, u64::MAX / 2);
    if mode == "reserve" {
        set_free_space_for_test(NAMESPACE_BUDGET + 150, 8 * NAMESPACE_BUDGET);
        let result = (|| -> Result<()> {
            let rendezvous = control.parent().unwrap();
            fs::write(rendezvous.join(format!("started-{family}")), b"ready")?;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !(rendezvous.join(format!("started-{A}")).exists()
                && rendezvous.join(format!("started-{B}")).exists())
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "second reservation child did not start"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            let lock = ReservationLock::acquire(&storage)?;
            let mut writer = RegistryWriter::open_existing(&storage, &family, &lock)?;
            let reservation = writer.reserve(100)?;
            drop(writer);
            drop(lock);
            fs::write(&control, b"reserved")?;
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !control.with_extension("release").exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "parent did not release reservation"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            let lock = ReservationLock::acquire(&storage)?;
            RegistryWriter::open_existing(&storage, &family, &lock)?.release(reservation)
        })();
        if let Err(error) = result {
            fs::write(control, error.to_string()).unwrap();
        }
        return;
    }
    if mode == "normal" {
        seed(&storage, &family);
        return;
    }
    struct Park<'a> {
        step: DurabilityStep,
        control: &'a Path,
    }
    impl DurabilityObserver for Park<'_> {
        fn reached(&self, step: DurabilityStep) {
            if step == self.step {
                fs::write(self.control, step.as_str()).unwrap();
                loop {
                    std::thread::park();
                }
            }
        }
    }
    let observer = Park {
        step: DurabilityStep::parse(&mode).unwrap(),
        control: &control,
    };
    publish(&storage, &family, 2 * 1024 * 1024, Some(&observer), |p| {
        p.put(&key(), &vec![9; 1024 * 1024], 1)
    })
    .unwrap();
    panic!("child did not stop at its requested durability boundary");
}

#[test]
fn two_processes_reserving_in_different_families_have_only_one_winner() {
    let _g = serial();
    let root = tempdir().unwrap();
    let control = tempdir().unwrap();
    publish(root.path(), A, 0, None, |_| Ok(())).unwrap();
    publish(root.path(), B, 0, None, |_| Ok(())).unwrap();
    let ready_a = control.path().join("a");
    let ready_b = control.path().join("b");
    let mut a = child(root.path(), A, "reserve", &ready_a);
    let mut b = child(root.path(), B, "reserve", &ready_b);
    wait_file(&ready_a, &mut a);
    wait_file(&ready_b, &mut b);
    let outcomes = [
        fs::read_to_string(&ready_a).unwrap(),
        fs::read_to_string(&ready_b).unwrap(),
    ];
    fs::write(ready_a.with_extension("release"), b"go").unwrap();
    fs::write(ready_b.with_extension("release"), b"go").unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    assert_eq!(
        outcomes.iter().filter(|r| r.as_str() == "reserved").count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| r.as_str() == "device_free_space_floor")
            .count(),
        1,
        "{outcomes:?}"
    );
}

#[cfg(unix)]
#[test]
fn killed_v3_publishers_recover_at_both_new_boundaries_without_losing_orphans() {
    let _g = serial();
    for step in DurabilityStep::V3_ONLY {
        let root = tempdir().unwrap();
        let control = tempdir().unwrap();
        let ready = control.path().join("ready");
        let mut publisher = child(root.path(), A, step.as_str(), &ready);
        wait_file(&ready, &mut publisher);
        publisher.kill().unwrap();
        publisher.wait().unwrap();
        assert!(fs::read_to_string(root.path().join("reader-floor.json"))
            .unwrap()
            .contains("views_v3_store"));
        match step {
            DurabilityStep::V3FloorRaised => {
                assert!(!root.path().join("blobs/v3").exists());
                seed(root.path(), A);
            }
            DurabilityStep::ClosureFlushed => {
                let outstanding = LedgerReader::open_existing(root.path(), A)
                    .unwrap()
                    .accounting()
                    .unwrap();
                assert!(outstanding.reserved_bytes > 0);
                let lock = ReservationLock::acquire(root.path()).unwrap();
                let writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
                let accounting = writer.accounting().unwrap();
                assert_eq!(accounting.reserved_bytes, 0);
                assert_eq!(accounting.usage(), inventory(&writer.dir));
                assert!(accounting.usage() >= 1024 * 1024);
                let released: u64 = writer
                    .connection
                    .query_row(
                        "SELECT COUNT(*) FROM reservations WHERE state='released'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(released, 1);
                assert_eq!(
                    PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic)
                        .unwrap()
                        .get(&key())
                        .unwrap()
                        .unwrap(),
                    vec![9; 1024 * 1024]
                );
            }
            DurabilityStep::BlobCommitted
            | DurabilityStep::SegmentRowRecorded
            | DurabilityStep::SegmentFileSynced
            | DurabilityStep::SegmentDurable
            | DurabilityStep::ManifestWritten
            | DurabilityStep::ManifestParentSynced
            | DurabilityStep::PointerCas
            | DurabilityStep::PointerSynced => panic!("V3_ONLY contains a v2 boundary"),
        }
    }
}

#[cfg(unix)]
#[test]
fn normal_child_exit_keeps_data_and_inventory_charge() {
    let _g = serial();
    let root = tempdir().unwrap();
    let control = tempdir().unwrap();
    assert!(child(root.path(), A, "normal", control.path())
        .wait()
        .unwrap()
        .success());
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    assert_eq!(writer.accounting().unwrap().usage(), inventory(&writer.dir));
    assert_eq!(
        PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic)
            .unwrap()
            .get(&key())
            .unwrap(),
        Some(b"v3 data".to_vec())
    );
}

fn file_snapshot(path: &Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
    let mut files = Vec::new();
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            files.extend(file_snapshot(&path));
        } else {
            files.push((
                path.clone(),
                fs::read(&path).unwrap(),
                entry.metadata().unwrap().modified().unwrap(),
            ));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}
#[test]
fn ledger_read_does_not_write_create_sidecars_or_remeasure_closed_or_live_registry() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let dir = family_dir(root.path(), A).unwrap();
    let before = file_snapshot(&dir);
    let ledger = LedgerReader::open_existing(root.path(), A).unwrap();
    let original = ledger.accounting().unwrap();
    assert_eq!(file_snapshot(&dir), before);
    drop(ledger);
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut writer = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let reservation = writer.reserve(100).unwrap();
    let before = file_snapshot(&dir);
    let ledger = LedgerReader::open_existing(root.path(), A).unwrap();
    assert_eq!(ledger.accounting().unwrap().reserved_bytes, 100);
    assert!(original.measured_bytes > 0);
    assert_eq!(file_snapshot(&dir), before);
    drop(ledger);
    writer.release(reservation).unwrap();
}

pub(super) fn admission_hook() {
    let Some(control) = std::env::var_os("AFT_V3_TEST_ADMISSION_CONTROL") else {
        return;
    };
    let control = PathBuf::from(control);
    let family = std::env::var("AFT_V3_TEST_FAMILY").unwrap();
    fs::write(control.join(format!("checked-{family}")), b"checked").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while !(control.join(format!("checked-{A}")).exists()
        && control.join(format!("checked-{B}")).exists())
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn nested_writer_uses_existing_root_guard_and_does_not_touch_peer_ledger() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let a = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let before = file_snapshot(&a.dir);
    let accounting = a.accounting().unwrap();
    let started = std::time::Instant::now();
    publish_locked(root.path(), B, 1024, &lock, None, |p| {
        p.put(&key(), b"v3 data", 1)
    })
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(a.accounting().unwrap(), accounting);
    assert_eq!(file_snapshot(&a.dir), before);
}

#[test]
fn capacity_percentage_floor_applies_above_the_four_gib_minimum() {
    let _g = serial();
    let root = tempdir().unwrap();
    set_free_space_for_test(2 * NAMESPACE_BUDGET, 200 * NAMESPACE_BUDGET);
    assert_eq!(
        publish(root.path(), A, 1, None, |_| Ok(()))
            .unwrap_err()
            .to_string(),
        "device_free_space_floor"
    );
    assert!(!root.path().join("blobs/v3").exists());
}

#[cfg(unix)]
#[test]
fn committed_metadata_growth_is_charged_before_next_writer_open() {
    let _g = serial();
    let root = tempdir().unwrap();
    publish(root.path(), A, 2 * 1024 * 1024, None, |p| {
        for i in 0..1000 {
            p.registry
                .register_member(&format!("scope-{i}"), "view", None)?;
        }
        Ok(())
    })
    .unwrap();
    let accounting = LedgerReader::open_existing(root.path(), A)
        .unwrap()
        .accounting()
        .unwrap();
    assert!(
        accounting.committed_charges > 100_000,
        "metadata growth must replace the reservation: {accounting:?}"
    );
    assert!(accounting.usage() >= inventory(&family_dir(root.path(), A).unwrap()));
}

#[cfg(unix)]
#[test]
fn reclaimed_plane_pages_reduce_usage_only_at_the_next_covering_measurement() {
    let _g = serial();
    let root = tempdir().unwrap();
    // The existing reclaim policy deliberately retains free lists below 256 MiB.
    publish(root.path(), A, 300 * 1024 * 1024, None, |p| {
        p.put(&key(), &vec![7; 272 * 1024 * 1024], 1)
    })
    .unwrap();
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut registry = RegistryWriter::open_existing(root.path(), A, &lock).unwrap();
    let before = registry.accounting().unwrap().usage();
    let path = plane_path(root.path(), A, FamilyPlane::Semantic).unwrap();
    let c = open_db(&path, Kind::Plane, false, false).unwrap();
    c.execute("DELETE FROM blob_payloads", []).unwrap();
    checkpoint(&c).unwrap();
    for _ in 0..16 {
        super::super::reclaim::reclaim(&c, None, &|| false).unwrap();
    }
    drop(c);
    assert_eq!(registry.accounting().unwrap().usage(), before);
    registry.covering_measurement().unwrap();
    let after = registry.accounting().unwrap().usage();
    assert_eq!(after, inventory(&registry.dir));
    let freed = before - after;
    assert!(freed > 0, "incremental reclaim must free allocated pages");
    let reservation = registry.reserve(NAMESPACE_BUDGET - before + freed).unwrap();
    registry.release(reservation).unwrap();
}

#[test]
fn newer_format_keeps_artifact_refusal_and_malformed_file_reports_found_zero() {
    let _g = serial();
    let root = tempdir().unwrap();
    seed(root.path(), A);
    let path = db(root.path(), A);
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE format_meta SET format_version=2", [])
        .unwrap();
    match LedgerReader::open_existing(root.path(), A).err().unwrap() {
        Error::Incompatible(error) => {
            assert_eq!(error.found, 2);
            assert_eq!(error.source, persisted_format::RefusalSource::Artifact);
        }
        error => panic!("{error}"),
    }
    fs::write(&path, b"not sqlite").unwrap();
    match LedgerReader::open_existing(root.path(), A).err().unwrap() {
        Error::Incompatible(error) => {
            assert_eq!(error.found, 0);
            assert_eq!(
                error.source,
                persisted_format::RefusalSource::Incompatible(IncompatibleReason::SchemaDrift)
            );
        }
        error => panic!("{error}"),
    }
}

#[test]
fn plane_open_enforces_the_same_schema_application_and_pragma_contract() {
    let _g = serial();
    for (sql, reason) in [
        ("CREATE VIEW extra AS SELECT 1", "schema_drift"),
        ("UPDATE format_meta SET schema_text='wrong'", "schema_drift"),
        ("PRAGMA application_id=1095128146", "application_id"),
        ("UPDATE format_meta SET layout_tag=88", "layout_tag:88"),
    ] {
        let root = tempdir().unwrap();
        seed(root.path(), A);
        let path = plane_path(root.path(), A, FamilyPlane::Semantic).unwrap();
        Connection::open(&path).unwrap().execute_batch(sql).unwrap();
        let error = PlaneReader::open_existing(root.path(), A, FamilyPlane::Semantic)
            .err()
            .expect("plane contract must refuse");
        assert!(error.to_string().ends_with(reason), "{error}");
    }
    let root = tempdir().unwrap();
    seed(root.path(), A);
    BAD_PRAGMA.with(|bad| bad.set(Some("synchronous")));
    let error = publish(root.path(), A, 1, None, |_| Ok(())).unwrap_err();
    BAD_PRAGMA.with(|bad| bad.set(None));
    assert!(error.to_string().ends_with("pragma:synchronous"), "{error}");
}

#[test]
fn member_kind_constraint_and_v3_namespace_are_not_v2_registration() {
    let _g = serial();
    let root = tempdir().unwrap();
    publish(root.path(), A, 1024, None, |p| {
        p.registry
            .register_member("view-scope", "view", Some(root.path()))?;
        p.registry.register_member("base-scope", "base", None)?;
        assert!(p
            .registry
            .register_member("invalid-scope", "other", None)
            .is_err());
        Ok(())
    })
    .unwrap();
    let ledger = LedgerReader::open_existing(root.path(), A).unwrap();
    let count:u64=ledger.connection.query_row("SELECT COUNT(*) FROM members WHERE kind IN ('view','base') AND state='active' AND missing_sweeps=0 AND last_bind_ms>0",[],|r|r.get(0)).unwrap();
    assert_eq!(count, 2);
    assert!(!root.path().join("blobs/v2").exists());
    assert!(view_dir(root.path(), A, "view-scope")
        .unwrap()
        .starts_with(family_dir(root.path(), A).unwrap()));
}
