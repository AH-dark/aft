#[allow(dead_code)]
#[path = "helpers/binary_cache.rs"]
mod cache;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Barrier,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
fn executable_cache_reuses_one_immutable_content_addressed_path() {
    let bytes = b"#!/bin/sh\nprintf '%s\n' cached\n";
    let first = crate::test_helpers::executable_cache::cached_executable(bytes).unwrap();
    let before = fs::metadata(&first).unwrap();
    let second = crate::test_helpers::executable_cache::cached_executable(bytes).unwrap();
    let after = fs::metadata(&second).unwrap();

    assert_eq!(first, second);
    assert_eq!(first.parent().unwrap().file_name().unwrap().len(), 64);
    assert_eq!(fs::read(&second).unwrap(), bytes);
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(before.ino(), after.ino());
    }
}

#[test]
fn concurrent_executable_cache_users_share_one_published_path() {
    use crate::test_helpers::executable_cache::cached_executable;

    let bytes = b"#!/bin/sh\nexit 0\n";
    let barrier = Arc::new(Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                cached_executable(bytes).unwrap()
            })
        })
        .collect();
    let paths: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();

    assert!(paths.iter().all(|path| path == &paths[0]));
    assert_eq!(fs::read(&paths[0]).unwrap(), bytes);
}

#[test]
fn concurrent_builders_compile_once() {
    let dir = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let builds = Arc::new(AtomicUsize::new(0));
    let key = cache::key(&[b"source"], "rustc", "target");
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let (root, barrier, builds, key) = (
                dir.path().to_owned(),
                barrier.clone(),
                builds.clone(),
                key.clone(),
            );
            std::thread::spawn(move || {
                barrier.wait();
                cache::install(&root, &key, "target", |pending| {
                    builds.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    fs::write(pending, b"complete executable")
                })
                .unwrap()
            })
        })
        .collect();
    let paths: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(paths[0], paths[1]);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert_eq!(fs::read(&paths[0]).unwrap(), b"complete executable");
    assert_eq!(
        fs::read_dir(dir.path().join("fake-lsp-server"))
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.path().is_dir())
            .count(),
        1
    );
}

#[cfg(target_os = "linux")]
#[test]
fn installation_never_write_opens_a_running_executable() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let script = br#"#!/bin/sh
printf 'ready\n'
read release
"#;
    let source_key = cache::key(&[script], "rustc", "unix");
    let source = cache::install_bytes(dir.path(), &source_key, "unix", script).unwrap();
    let mut child = Command::new(&source)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready.trim(), "ready");
    let key = cache::key(&[b"running executable"], "rustc", "unix");
    // A hard link preserves the running inode. Linux rejects even a momentary
    // write-open; syncing an immutable helper must require only read access.
    let installed = cache::install(dir.path(), &key, "unix", |pending| {
        fs::hard_link(&source, pending)
    });
    writeln!(child.stdin.take().unwrap(), "release").unwrap();
    assert!(child.wait().unwrap().success());
    installed.expect("installation must not write-open executable inodes");
}

#[test]
fn source_toolchain_and_target_changes_get_new_keys() {
    let original = cache::key(&[b"source"], "rustc 1", "unix");
    let dir = tempfile::tempdir().unwrap();
    let first = cache::install_bytes(dir.path(), &original, "unix", b"first").unwrap();
    let changed = cache::key(&[b"changed source"], "rustc 1", "unix");
    let second = cache::install_bytes(dir.path(), &changed, "unix", b"second").unwrap();
    assert_ne!(first, second);
    assert_eq!(fs::read(first).unwrap(), b"first");
    assert_eq!(fs::read(second).unwrap(), b"second");
    assert_ne!(original, cache::key(&[b"source"], "rustc 2", "unix"));
    assert_ne!(original, cache::key(&[b"source"], "rustc 1", "windows"));
    assert_eq!(
        cache::executable(Path::new("entry"), "x86_64-pc-windows-msvc"),
        Path::new("entry/fake-lsp-server.exe")
    );
}

#[test]
fn cache_hits_preserve_bytes_inode_and_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let key = cache::key(&[b"source"], "rustc", "target");
    let first = cache::install(dir.path(), &key, "target", |pending| {
        fs::write(pending, b"original")?;
        filetime::set_file_mtime(pending, filetime::FileTime::from_unix_time(1_000_000, 0))
    })
    .unwrap();
    let before = fs::metadata(&first).unwrap();
    let second = cache::install_bytes(dir.path(), &key, "target", b"must not overwrite").unwrap();
    let after = fs::metadata(&second).unwrap();
    assert_eq!(first, second);
    assert_eq!(fs::read(second).unwrap(), b"original");
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(before.ino(), after.ino());
    }
}

#[test]
fn pruning_uses_install_records_keeps_four_and_expires_old_entries() {
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let now_secs = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut paths = Vec::new();
    for n in 0..7 {
        let entry = dir.path().join(format!("{n:064x}"));
        fs::create_dir(&entry).unwrap();
        fs::write(entry.join("fake-lsp-server"), b"executable").unwrap();
        let installed = if n == 0 {
            now_secs - 15 * 86400
        } else {
            now_secs - 10 + n
        };
        fs::write(entry.join("installed"), installed.to_string()).unwrap();
        paths.push(entry);
    }
    // A recent executable mtime must not rescue an expired install record.
    cache::prune(dir.path(), &paths[6], now);
    for (n, path) in paths.iter().enumerate() {
        assert_eq!(path.exists(), n >= 3, "entry {n}");
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 4);
}

#[test]
#[ignore = "subprocess entry point for cache reuse test"]
fn cache_builder_child() {
    let root = std::env::var_os("AFT_CACHE_TEST_ROOT").expect("child cache root");
    let path = crate::fake_lsp::build_in(Path::new(&root));
    // Execute the installed helper, not a proxy file or the mutable Cargo output.
    let status = Command::new(path)
        .env("AFT_FAKE_LSP_NO_WATCHED_FILES", "1")
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn two_worktree_runs_compile_once_and_preserve_installed_file() {
    let cache_root = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let repository = fixture.path().join("repository");
    let worktree_one = fixture.path().join("one");
    let worktree_two = fixture.path().join("two");
    let git = |cwd: &Path, args: &[&std::ffi::OsStr]| {
        let mut command = Command::new("git");
        crate::test_helpers::apply_hermetic_git_env(&mut command);
        let output = command
            .current_dir(cwd)
            .args([
                "-c",
                "user.name=AFT Test",
                "-c",
                "user.email=aft-test@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(fixture.path(), &["init".as_ref(), repository.as_os_str()]);
    git(
        &repository,
        &[
            "commit".as_ref(),
            "--allow-empty".as_ref(),
            "-m".as_ref(),
            "fixture".as_ref(),
        ],
    );
    for path in [&worktree_one, &worktree_two] {
        git(
            &repository,
            &[
                "worktree".as_ref(),
                "add".as_ref(),
                "--detach".as_ref(),
                path.as_os_str(),
                "HEAD".as_ref(),
            ],
        );
    }
    let run = |cwd: &Path| {
        Command::new(std::env::current_exe().unwrap())
            .current_dir(cwd)
            .env("AFT_CACHE_TEST_ROOT", cache_root.path())
            .args([
                "--exact",
                "fake_helper_cache_test::cache_builder_child",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .unwrap()
    };
    let first = run(&worktree_one);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let binary = fs::read_dir(cache_root.path().join("fake-lsp-server"))
        .unwrap()
        .filter_map(Result::ok)
        .find_map(|entry| {
            let binary = cache::executable(&entry.path(), std::env::consts::OS);
            binary.is_file().then_some(binary)
        })
        .unwrap();
    let bytes = fs::read(&binary).unwrap();
    let before = fs::metadata(&binary).unwrap();
    let prune_marker = cache_root.path().join("fake-lsp-server/.last-pruned-run");
    let marker_before = if prune_marker.is_file() {
        filetime::set_file_mtime(
            &prune_marker,
            filetime::FileTime::from_unix_time(1_000_000, 0),
        )
        .unwrap();
        Some(fs::metadata(&prune_marker).unwrap().modified().unwrap())
    } else {
        None
    };
    let second = run(&worktree_two);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let first_builds = String::from_utf8_lossy(&first.stderr)
        .matches("Building shared AFT test helper")
        .count();
    let second_builds = String::from_utf8_lossy(&second.stderr)
        .matches("Building shared AFT test helper")
        .count();
    assert_eq!((first_builds, second_builds), (1, 0));
    eprintln!("helper compilations across two worktrees: {first_builds}, {second_builds}");
    assert_eq!(bytes, fs::read(&binary).unwrap());
    let after = fs::metadata(&binary).unwrap();
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    if let Some(marker_before) = marker_before {
        assert_eq!(
            marker_before,
            fs::metadata(&prune_marker).unwrap().modified().unwrap(),
            "nextest subprocesses must share one prune per run"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(before.ino(), after.ino());
    }
}
