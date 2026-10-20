use super::*;
use crate::edit::{atomic_write_source, atomic_write_source_with, SourceWriteMode};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[test]
fn default_primitive_is_replace_file() {
    assert!(!enabled());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("default.txt");
    std::fs::write(&path, b"old").unwrap();
    let (result, events) = using(Primitive::ReplaceFile, || {
        atomic_write_source(&path, b"new")
    });
    assert_eq!(result.unwrap().mode, SourceWriteMode::Replaced);
    assert!(
        events.is_empty(),
        "the default must not enter the experiment"
    );
    assert_eq!(std::fs::read(path).unwrap(), b"new");
}

#[test]
fn unsupported_rename_errors_record_replacefile_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fallback.txt");
    // Real filesystem and OS versions can return different codes; exercise the
    // fallback policy, not a claim that these fixtures emulate those systems.
    for code in [1, 50, 87, 120, 124, 5, 32] {
        std::fs::write(&path, b"old").unwrap();
        let (result, events) = using(Primitive::InjectError(code), || {
            atomic_write_source(&path, b"new")
        });
        assert_eq!(result.unwrap().mode, SourceWriteMode::Replaced);
        assert_eq!(
            events,
            vec![Event::Fallback {
                code: Some(code),
                reason: fallback_reason(Some(code))
            }]
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }
}

#[test]
fn equal_owners_allow_rename_ex() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("equal-owner.txt");
    std::fs::write(&path, b"old").unwrap();
    let (result, events) = using(Primitive::RenameEx, || {
        OWNER_COMPARISON.set(Some(true));
        atomic_write_source(&path, b"new")
    });
    assert_eq!(result.unwrap().mode, SourceWriteMode::Replaced);
    assert_eq!(events, vec![Event::Renamed]);
    assert_eq!(std::fs::read(path).unwrap(), b"new");
}

#[test]
fn owner_difference_uses_replacefile_with_reason() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("different-owner.txt");
    std::fs::write(&path, b"old").unwrap();
    let (result, events) = using(Primitive::RenameEx, || {
        OWNER_COMPARISON.set(Some(false));
        atomic_write_source(&path, b"new")
    });
    assert_eq!(result.unwrap().mode, SourceWriteMode::Replaced);
    assert_eq!(
        events,
        vec![Event::Fallback {
            code: None,
            reason: "owner_differs"
        }]
    );
    assert_eq!(std::fs::read(path).unwrap(), b"new");
}

#[test]
fn failed_replacefile_fallback_never_requests_in_place() {
    let (result, events) = using(Primitive::InjectError(50), || {
        replace_with_fallback(Path::new("unused"), Path::new("unused"), || Ok(false))
    });
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("in-place fallback disabled"));
    assert_eq!(
        events,
        vec![Event::Fallback {
            code: Some(50),
            reason: fallback_reason(Some(50))
        }]
    );
}

#[test]
fn hard_linked_targets_bypass_rename_experiment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("linked.txt");
    let alias = dir.path().join("alias.txt");
    std::fs::write(&path, b"old").unwrap();
    std::fs::hard_link(&path, &alias).unwrap();
    let (result, events) = using(Primitive::RenameEx, || atomic_write_source(&path, b"new"));
    assert_eq!(result.unwrap().mode, SourceWriteMode::InPlace);
    assert!(events.is_empty());
    assert_eq!(std::fs::read(path).unwrap(), b"new");
    assert_eq!(std::fs::read(alias).unwrap(), b"new");
}

#[test]
fn readonly_targets_are_not_overridden() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("readonly.txt");
    std::fs::write(&path, b"old").unwrap();
    let permissions = std::fs::metadata(&path).unwrap().permissions();
    let mut readonly = permissions.clone();
    readonly.set_readonly(true);
    std::fs::set_permissions(&path, readonly).unwrap();
    let (result, events) = using(Primitive::RenameEx, || atomic_write_source(&path, b"new"));
    std::fs::set_permissions(&path, permissions).unwrap();
    assert!(result.is_err());
    assert!(events.is_empty());
    assert_eq!(std::fs::read(path).unwrap(), b"old");
}

#[derive(Debug)]
struct ReaderStats {
    attempts: usize,
    missing: usize,
    affected: std::collections::BTreeSet<usize>,
    other: std::collections::BTreeMap<Option<i32>, usize>,
}

fn filesystem_name(path: &Path) -> io::Result<String> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
    let file = std::fs::File::open(path)?;
    let mut name = [0u16; 64];
    // SAFETY: the file is open, the output name buffer is writable, and unused
    // optional volume-information outputs are null with length zero.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle() as _,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let end = name.iter().position(|ch| *ch == 0).unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..end]))
}

#[test]
#[ignore = "native Windows measurement: 1000 x 2 MiB per primitive; run with --ignored --nocapture"]
fn compare_concurrent_readers() {
    const ROUNDS: usize = 1000;
    for primitive in [Primitive::ReplaceFile, Primitive::RenameEx] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("single.txt");
        let bytes = vec![b'x'; 2 * 1024 * 1024];
        std::fs::write(&path, &bytes).unwrap();
        eprintln!("probe filesystem: {:?}", filesystem_name(&path));
        let stop = AtomicBool::new(false);
        let round = AtomicUsize::new(0);
        let ready = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            let readers: Vec<_> = [false, true]
                .into_iter()
                .map(|open| {
                    let (path, stop, round, ready) = (&path, &stop, &round, &ready);
                    scope.spawn(move || {
                        let mut stats = ReaderStats {
                            attempts: 0,
                            missing: 0,
                            affected: Default::default(),
                            other: Default::default(),
                        };
                        ready.wait();
                        while !stop.load(Ordering::Acquire) {
                            let result = if open {
                                std::fs::File::open(path).map(drop)
                            } else {
                                std::fs::metadata(path).map(drop)
                            };
                            stats.attempts += 1;
                            if let Err(error) = result {
                                if error.kind() == io::ErrorKind::NotFound {
                                    stats.missing += 1;
                                    stats.affected.insert(round.load(Ordering::Acquire));
                                } else {
                                    *stats.other.entry(error.raw_os_error()).or_default() += 1;
                                }
                            }
                        }
                        (open, stats)
                    })
                })
                .collect();
            ready.wait();
            let started = std::time::Instant::now();
            let (result, events) = using(primitive, || {
                for iteration in 0..ROUNDS {
                    round.store(iteration, Ordering::Release);
                    let result = atomic_write_source(&path, &bytes);
                    if !matches!(&result, Ok(outcome) if outcome.mode == SourceWriteMode::Replaced)
                    {
                        return Err(format!("replacement {iteration}: {result:?}"));
                    }
                }
                Ok(())
            });
            stop.store(true, Ordering::Release);
            eprintln!(
                "probe: primitive={primitive:?} replacements={ROUNDS} elapsed_ms={}",
                started.elapsed().as_millis()
            );
            let stats: Vec<_> = readers
                .into_iter()
                .map(|reader| reader.join().unwrap())
                .collect();
            result.unwrap();
            for (open, stats) in stats {
                eprintln!("probe: primitive={primitive:?} open={open} attempts={} not_found={} affected_replacements={}/{} other_errors={:?}", stats.attempts, stats.missing, stats.affected.len(), ROUNDS, stats.other);
                assert!(stats.attempts > 0, "the reader must actually run");
                if primitive == Primitive::RenameEx {
                    assert_eq!(stats.missing, 0, "RenameEx reader: {stats:?}");
                    assert!(stats.other.is_empty(), "RenameEx reader: {stats:?}");
                } else {
                    assert!(
                        stats.other.keys().all(|code| *code == Some(32)),
                        "unexpected baseline errors: {stats:?}"
                    );
                }
            }
            if primitive == Primitive::RenameEx {
                assert_eq!(events.len(), ROUNDS);
                assert!(
                    events.iter().all(|event| *event == Event::Renamed),
                    "{events:?}"
                );
            } else {
                assert!(events.is_empty());
            }
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        });
    }
}

#[derive(Debug, PartialEq, Eq)]
struct MetadataSnapshot {
    owner: Vec<u8>,
    dacl: Option<Vec<u8>>,
    attributes: u32,
    creation_time: u64,
}

fn snapshot(path: &Path) -> MetadataSnapshot {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Security::{
        GetFileSecurityW, GetLengthSid, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, ACL,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let flags = DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION;
    let mut needed = 0;
    // SAFETY: a null zero-length buffer asks Windows for the required size.
    unsafe { GetFileSecurityW(wide.as_ptr(), flags, std::ptr::null_mut(), 0, &mut needed) };
    assert!(needed > 0, "{}", io::Error::last_os_error());
    let mut descriptor = vec![0u64; (needed as usize).div_ceil(8)];
    let mut owner = std::ptr::null_mut();
    let mut defaulted = 0;
    let mut present = 0;
    let mut acl: *mut ACL = std::ptr::null_mut();
    // SAFETY: the descriptor is aligned and sized for the API. Windows returns
    // pointers to a SID and ACL within that descriptor, which remains live.
    let (owner, dacl) = unsafe {
        assert_ne!(
            GetFileSecurityW(
                wide.as_ptr(),
                flags,
                descriptor.as_mut_ptr().cast(),
                needed,
                &mut needed
            ),
            0,
            "{}",
            io::Error::last_os_error()
        );
        assert_ne!(
            GetSecurityDescriptorOwner(descriptor.as_mut_ptr().cast(), &mut owner, &mut defaulted),
            0
        );
        assert!(!owner.is_null());
        let owner =
            std::slice::from_raw_parts(owner.cast::<u8>(), GetLengthSid(owner) as usize).to_vec();
        assert_ne!(
            GetSecurityDescriptorDacl(
                descriptor.as_mut_ptr().cast(),
                &mut present,
                &mut acl,
                &mut defaulted
            ),
            0
        );
        let dacl = if present == 0 || acl.is_null() {
            None
        } else {
            Some(std::slice::from_raw_parts(acl.cast::<u8>(), (*acl).AclSize as usize).to_vec())
        };
        (owner, dacl)
    };
    let metadata = std::fs::metadata(path).unwrap();
    MetadataSnapshot {
        owner,
        dacl,
        attributes: metadata.file_attributes(),
        creation_time: metadata.creation_time(),
    }
}

fn icacls(path: &Path, args: &[&str]) {
    let output = std::process::Command::new("icacls")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn current_user_sid() -> String {
    let output = std::process::Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .rsplit(',')
        .next()
        .unwrap()
        .trim_matches('"')
        .to_owned()
}

fn set_creation_and_attributes(path: &Path, creation: u64, attributes: u32) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::{SetFileAttributesW, SetFileTime};
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    let creation = FILETIME {
        dwLowDateTime: creation as u32,
        dwHighDateTime: (creation >> 32) as u32,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: the handle, FILETIME and NUL-terminated path live through the calls.
    unsafe {
        assert_ne!(
            SetFileTime(
                file.as_raw_handle() as _,
                &creation,
                std::ptr::null(),
                std::ptr::null()
            ),
            0,
            "{}",
            io::Error::last_os_error()
        );
        assert_ne!(
            SetFileAttributesW(wide.as_ptr(), attributes),
            0,
            "{}",
            io::Error::last_os_error()
        );
    }
}

fn stream(path: &Path, name: &str) -> std::path::PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(":");
    path.push(name);
    path.into()
}

#[test]
fn compare_metadata_preservation() {
    use std::io::Write;
    thread_local! {
        static PREPARED: RefCell<Option<MetadataSnapshot>> = const { RefCell::new(None) };
    }
    for primitive in [Primitive::ReplaceFile, Primitive::RenameEx] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.txt");
        std::fs::write(&path, b"old").unwrap();
        let sid = current_user_sid();
        icacls(
            &path,
            &[
                "/inheritance:r",
                "/grant:r",
                &format!("*{sid}:(F)"),
                "*S-1-5-32-551:(R)",
            ],
        );
        icacls(&path, &["/setowner", &format!("*{sid}")]);
        std::fs::write(stream(&path, "original-only"), b"old stream").unwrap();
        std::fs::write(stream(&path, "conflict"), b"old conflict").unwrap();
        set_creation_and_attributes(&path, 125_911_584_000_000_000, 0x2 | 0x4 | 0x20);
        let original = snapshot(&path);
        let mut writer_stage = None;
        let (result, events) = using(primitive, || {
            BEFORE_COMMIT.set(Some(|_, temporary| {
                PREPARED.with_borrow_mut(|prepared| *prepared = Some(snapshot(temporary)));
            }));
            atomic_write_source_with(&path, b"new", |file, bytes| {
                let temporaries: Vec<_> = std::fs::read_dir(dir.path())
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "tmp"))
                    .collect();
                assert_eq!(temporaries.len(), 1);
                let temporary = &temporaries[0];
                assert_eq!(
                    snapshot(temporary).dacl,
                    original.dacl,
                    "DACL must be prepared before content"
                );
                // Unprivileged runners can assign only their own SID as owner.
                // Record that limit instead of requiring a privileged account.
                icacls(temporary, &["/setowner", &format!("*{sid}")]);
                std::fs::write(stream(temporary, "source-only"), b"new stream").unwrap();
                std::fs::write(stream(temporary, "conflict"), b"new conflict").unwrap();
                file.write_all(bytes)?;
                set_creation_and_attributes(temporary, 126_227_808_000_000_000, 0x20);
                writer_stage = Some(snapshot(temporary));
                Ok(())
            })
        });
        assert_eq!(result.unwrap().mode, SourceWriteMode::Replaced);
        let writer_stage = writer_stage.unwrap();
        let prepared = PREPARED.with_borrow_mut(Option::take).unwrap();
        let after = snapshot(&path);
        let original_stream = std::fs::read(stream(&path, "original-only"));
        eprintln!("metadata: primitive={primitive:?} original={original:?} writer_stage={writer_stage:?} prepared_after_sync={prepared:?} after={after:?} original_stream={original_stream:?} events={events:?}");
        assert_ne!(original.attributes, writer_stage.attributes);
        // The shared pipeline restores the original Permissions after writing;
        // on Windows that also restores hidden/system/archive attribute bits.
        assert_eq!(prepared.attributes, original.attributes);
        assert_ne!(original.creation_time, prepared.creation_time);
        assert_eq!(after.owner, prepared.owner, "prepared owner must survive");
        assert_eq!(after.attributes, prepared.attributes);
        assert_eq!(
            after.dacl, prepared.dacl,
            "prepared explicit DACL must survive"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(
            std::fs::read(stream(&path, "source-only")).unwrap(),
            b"new stream"
        );
        assert_eq!(
            std::fs::read(stream(&path, "conflict")).unwrap(),
            b"new conflict"
        );
        if primitive == Primitive::RenameEx {
            assert_eq!(events, vec![Event::Renamed]);
            // Creation-time tunneling can retain the old name's timestamp even
            // with RenameEx. Measure which file's timestamp survived rather
            // than promise replacement-time semantics the API does not state.
            assert!(
                after.creation_time == original.creation_time
                    || after.creation_time == prepared.creation_time,
                "unexpected creation time: {after:?}"
            );
            assert_eq!(original_stream.unwrap_err().kind(), io::ErrorKind::NotFound);
        } else {
            assert!(events.is_empty());
            assert_eq!(after.creation_time, original.creation_time);
            assert_eq!(original_stream.unwrap(), b"old stream");
        }
    }
}

#[test]
fn crash_before_rename_child() {
    let Some(root) = std::env::var_os("AFT_RENAME_CRASH_ROOT") else {
        return;
    };
    let path = std::path::PathBuf::from(root).join("original.txt");
    using(Primitive::RenameEx, || {
        BEFORE_COMMIT.set(Some(|destination, temporary| {
            assert_eq!(std::fs::read(destination).unwrap(), b"original");
            assert_eq!(std::fs::read(temporary).unwrap(), b"replacement");
            std::fs::write(
                destination.with_file_name("checkpoint"),
                temporary.as_os_str().to_string_lossy().as_bytes(),
            )
            .unwrap();
            // Exit without unwinding, so the temporary cleanup cannot run.
            std::process::exit(73);
        }));
        atomic_write_source(&path, b"replacement").unwrap();
    });
    panic!("the synced-temporary checkpoint was not reached");
}

#[test]
fn crash_before_rename_preserves_original() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.txt");
    std::fs::write(&path, b"original").unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "edit::windows_rename_experiment::tests::crash_before_rename_child",
            "--nocapture",
        ])
        .env("AFT_RENAME_CRASH_ROOT", dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(73), "{output:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    let temporary = std::fs::read_to_string(dir.path().join("checkpoint")).unwrap();
    assert_eq!(std::fs::read(temporary).unwrap(), b"replacement");
}
