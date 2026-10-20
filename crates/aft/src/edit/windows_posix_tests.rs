use super::super::atomic_write_source;
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

fn with_fault<T>(fault: Fault, f: impl FnOnce() -> T) -> T {
    struct Reset(Fault);
    impl Drop for Reset {
        fn drop(&mut self) {
            FAULT.set(self.0);
        }
    }
    let _reset = Reset(FAULT.replace(fault));
    f()
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.txt");
    std::fs::write(&path, b"original").unwrap();
    (dir, path)
}

fn assert_no_temporary(dir: &Path) {
    let entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, [std::ffi::OsString::from("source.txt")]);
}

fn assert_fallback(fault: Fault) {
    let (dir, path) = fixture();
    // Prove this volume and fixture can reach the fast path before forcing a guard.
    atomic_write_source(&path, b"control").unwrap();
    assert_eq!(
        LAST_METHOD.get(),
        Some(Method::PosixRename),
        "{}",
        LAST_REASON.with(|r| r.borrow().clone())
    );
    with_fault(fault, || atomic_write_source(&path, b"edited")).unwrap();
    assert_eq!(
        LAST_METHOD.get(),
        Some(Method::ReplaceFile),
        "fault={fault:?}"
    );
    assert_eq!(std::fs::read(path).unwrap(), b"edited");
    assert_no_temporary(dir.path());
}

#[test]
fn windows_posix_owner_mismatch_falls_back() {
    assert_fallback(Fault::OwnerMismatch);
}

#[test]
fn windows_posix_extra_stream_falls_back() {
    assert_fallback(Fault::ExtraStream);
    let (dir, path) = fixture();
    let stream = path.with_file_name("source.txt:private");
    std::fs::write(&stream, b"stream bytes").unwrap();
    atomic_write_source(&path, b"edited").unwrap();
    assert_eq!(LAST_METHOD.get(), Some(Method::ReplaceFile));
    assert_eq!(std::fs::read(&stream).unwrap(), b"stream bytes");
    assert_eq!(std::fs::read(&path).unwrap(), b"edited");
    assert_no_temporary(dir.path());
}

#[test]
fn windows_posix_unknown_checks_fall_back() {
    for fault in [
        Fault::OwnerUnknown,
        Fault::StreamsUnknown,
        Fault::LinksUnknown,
        Fault::MetadataUnknown,
    ] {
        assert_fallback(fault);
    }
}

#[test]
fn windows_posix_dacl_mismatch_falls_back() {
    assert_fallback(Fault::DaclMismatch);
}

#[test]
fn windows_posix_link_count_guard_falls_back() {
    assert_fallback(Fault::LinksUnknown);
}

#[test]
fn windows_posix_unsupported_volume_falls_back() {
    for code in [
        ERROR_NOT_SUPPORTED,
        ERROR_INVALID_PARAMETER,
        ERROR_INVALID_FUNCTION,
    ] {
        assert_fallback(Fault::Rename(code));
    }
}

#[test]
fn windows_posix_failed_rename_keeps_original_and_removes_temp() {
    let (dir, path) = fixture();
    let error = with_fault(Fault::Rename(5), || atomic_write_source(&path, b"edited")).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(5));
    assert_eq!(LAST_METHOD.get(), Some(Method::PosixRename));
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert_no_temporary(dir.path());
}

#[test]
fn windows_posix_read_only_still_refuses_write() {
    let (dir, path) = fixture();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions.clone()).unwrap();
    assert!(atomic_write_source(&path, b"edited").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
    assert_no_temporary(dir.path());
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions).unwrap();
}

fn set_times(file: &File, creation: u64, access: u64, write: u64) {
    use windows_sys::Win32::Foundation::FILETIME;
    fn ft(value: u64) -> FILETIME {
        FILETIME {
            dwLowDateTime: value as u32,
            dwHighDateTime: (value >> 32) as u32,
        }
    }
    // SAFETY: all three FILETIMEs and the writable handle outlive the call.
    assert_ne!(
        unsafe {
            SetFileTime(
                file.as_raw_handle() as _,
                &ft(creation),
                &ft(access),
                &ft(write),
            )
        },
        0
    );
}

#[test]
fn windows_posix_verbatim_destination_is_supported() {
    let (dir, path) = fixture();
    let verbatim = std::fs::canonicalize(&path).unwrap();
    assert!(verbatim.to_string_lossy().starts_with(r"\\?\"));
    atomic_write_source(&verbatim, b"edited").unwrap();
    assert_eq!(LAST_METHOD.get(), Some(Method::PosixRename));
    assert_eq!(std::fs::read(&path).unwrap(), b"edited");
    assert_no_temporary(dir.path());
}

#[test]
fn windows_posix_rename_path_alignment_preserves_content() {
    let dir = tempfile::tempdir().unwrap();
    // Exercise every alignment of the counted UTF-16 name within the u64
    // buffer; success must not depend on zero-filled allocator padding.
    for length in 1..=8 {
        let path = dir.path().join(format!("{}.txt", "a".repeat(length)));
        std::fs::write(&path, b"old\n").unwrap();
        atomic_write_source(&path, b"new\n").unwrap();
        assert_eq!(LAST_METHOD.get(), Some(Method::PosixRename));
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n", "{path:?}");
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 8);
}

#[test]
fn windows_posix_metadata_matches_replace_file() {
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
    fn stamp(info: &BY_HANDLE_FILE_INFORMATION) -> (u32, u64, u64, u64) {
        use windows_sys::Win32::Foundation::FILETIME;
        fn t(ft: &FILETIME) -> u64 {
            ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
        }
        (
            info.dwFileAttributes,
            t(&info.ftCreationTime),
            t(&info.ftLastAccessTime),
            t(&info.ftLastWriteTime),
        )
    }
    let mut snapshots = Vec::new();
    for (guarded, attributes) in [
        (false, FILE_ATTRIBUTE_ARCHIVE),
        (true, FILE_ATTRIBUTE_ARCHIVE),
        (
            false,
            FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
        ),
        (
            true,
            FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
        ),
    ] {
        let (dir, path) = fixture();
        let temp = dir.path().join("new.txt");
        let original = OpenOptions::new()
            .write(true)
            .read(true)
            .open(&path)
            .unwrap();
        set_times(
            &original,
            132000000000000000,
            132000000010000000,
            132000000020000000,
        );
        drop(original);
        // Compare the guarded policy with ReplaceFileW, including its
        // merging of attributes absent from the replacement.
        assert_ne!(
            unsafe { SetFileAttributesW(wide(&path).as_ptr(), attributes,) },
            0
        );
        std::fs::write(&temp, b"edited").unwrap();
        super::super::windows_dacl::copy_protected(&path, &temp).unwrap();
        let file = OpenOptions::new().write(true).open(&temp).unwrap();
        set_times(
            &file,
            133000000000000000,
            133000000010000000,
            133000000020000000,
        );
        drop(file);
        let dacl_copy = dir.path().join("acl.txt");
        std::fs::write(&dacl_copy, b"").unwrap();
        super::super::windows_dacl::copy_protected(&path, &dacl_copy).unwrap();
        if guarded {
            assert!(replace(&path, &temp, dir.path()).unwrap());
            let expected = if attributes == FILE_ATTRIBUTE_ARCHIVE {
                Method::PosixRename
            } else {
                Method::ReplaceFile
            };
            assert_eq!(LAST_METHOD.get(), Some(expected));
        } else {
            assert!(replace_file(&path, &temp, dir.path()).unwrap());
        }
        let info = information(&File::open(&path).unwrap()).unwrap();
        assert_eq!(stamp(&info).1, 132000000000000000);
        if guarded && attributes == FILE_ATTRIBUTE_ARCHIVE {
            assert!(super::super::windows_dacl::matches(&path, &dacl_copy).unwrap());
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"edited");
        assert!(!temp.exists());
        snapshots.push(stamp(&info));
    }
    assert_eq!(
        snapshots[1], snapshots[0],
        "POSIX metadata must match ReplaceFileW"
    );
    assert_eq!(
        snapshots[3], snapshots[2],
        "attribute fallback must match ReplaceFileW"
    );
}

#[test]
fn windows_posix_concurrent_reader_never_loses_destination() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.txt");
    let bytes = vec![b'x'; 64 * 1024];
    std::fs::write(&path, &bytes).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(Barrier::new(2));
    let reader = {
        let path = path.clone();
        let stop = stop.clone();
        let ready = ready.clone();
        std::thread::spawn(move || {
            let mut missing = 0;
            let mut reads = 0;
            ready.wait();
            while !stop.load(Ordering::Acquire) {
                match std::fs::read(&path) {
                    Ok(content) => assert_eq!(content, vec![b'x'; 64 * 1024]),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing += 1,
                    Err(error) => panic!("reader: {error}"),
                }
                reads += 1;
            }
            (reads, missing)
        })
    };
    ready.wait();
    for _ in 0..200 {
        atomic_write_source(&path, &bytes).unwrap();
        assert_eq!(LAST_METHOD.get(), Some(Method::PosixRename));
    }
    stop.store(true, Ordering::Release);
    let (reads, missing) = reader.join().unwrap();
    assert!(reads > 0);
    assert_eq!(missing, 0, "destination disappeared during {reads} reads");
}

/// Run on native Windows: cargo test -p agent-file-tools --lib --
/// windows_posix_1000_edit_benchmark --ignored --nocapture
#[test]
#[ignore = "1,000 durable 2 MiB edits per replacement method"]
fn windows_posix_1000_edit_benchmark() {
    for (method, fault) in [
        (Method::ReplaceFile, Fault::MetadataUnknown),
        (Method::PosixRename, Fault::None),
    ] {
        let (dir, path) = fixture();
        let bytes = vec![b'x'; 2 * 1024 * 1024];
        std::fs::write(&path, &bytes).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(Barrier::new(2));
        let reader = {
            let (path, stop, ready) = (path.clone(), stop.clone(), ready.clone());
            std::thread::spawn(move || {
                let (mut reads, mut missing, mut sharing) = (0, 0, 0);
                ready.wait();
                while !stop.load(Ordering::Acquire) {
                    match std::fs::read(&path) {
                        Ok(content) => assert_eq!(content.len(), 2 * 1024 * 1024),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => missing += 1,
                        Err(error) if error.raw_os_error() == Some(32) => sharing += 1,
                        Err(error) => panic!("reader: {error}"),
                    }
                    reads += 1;
                }
                (reads, missing, sharing)
            })
        };
        ready.wait();
        let started = std::time::Instant::now();
        let mut errors = 0;
        let mut in_place = 0;
        FALLBACK_COUNTS.with(|counts| counts.borrow_mut().clear());
        with_fault(fault, || {
            for _ in 0..1000 {
                if atomic_write_source(&path, &bytes).is_err() {
                    errors += 1;
                }
                if LAST_METHOD.get() == Some(Method::InPlace) {
                    in_place += 1;
                }
            }
        });
        stop.store(true, Ordering::Release);
        let (reads, missing, sharing) = reader.join().unwrap();
        println!("{method:?}: edits=1000 bytes={} elapsed_ms={} reads={reads} not_found={missing} sharing_violations={sharing} write_errors={errors} in_place={in_place}", bytes.len(), started.elapsed().as_millis());
        FALLBACK_COUNTS.with(|counts| {
            let counts = counts.borrow();
            let count = |reason: &str| counts.get(reason).copied().unwrap_or(0);
            let unsupported: usize = counts.iter().filter(|(reason, _)| reason.starts_with("rename unsupported:")).map(|(_, count)| count).sum();
            println!("{method:?}: fallback_attributes_differ={} fallback_owner_sid_differs={} fallback_path_unrepresentable={} fallback_unsupported_filesystem={unsupported} fallback_reasons={counts:?}", count("file attributes differ"), count("owner SIDs differ"), count("rename namespace is unsupported"));
        });
        if method == Method::PosixRename {
            assert_eq!(LAST_METHOD.get(), Some(method));
            assert!(reads > 0);
            assert_eq!((missing, sharing, errors, in_place), (0, 0, 0, 0));
        }
        assert_no_temporary(dir.path());
    }
}
