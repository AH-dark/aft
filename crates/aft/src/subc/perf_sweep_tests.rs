use super::*;

const FIXTURE: &str = include_str!("../../tests/fixtures/perf_sweep.txt");
const SINCE: u64 = 1_735_689_600_000;
fn profile_fixture() -> Value {
    json!({"threads": [
        {"id":"1", "name":"hot", "running_samples":75, "heaviest_paths":[{"samples":75,"frames":["thread_start (in libsystem)","aft[123abc]::engine::derive (in ck-aft)","core[abc]::work"]}]},
        {"id":"2", "name":"noise", "running_samples":25, "heaviest_paths":[{"count":25,"frames":["_pthread_start","???"]}]}
    ]})
}
#[test]
fn perf_sweep_arguments_are_strict_and_bounded() {
    let args = arguments(&json!({"op":"perf.sweep"})).unwrap();
    assert_eq!(args.since_ms, 0);
    assert_eq!(args.profile_seconds, 15);
    for args in [
        json!({"profile_seconds":0}),
        json!({"profile_seconds":31}),
        json!({"profile_seconds":-1}),
        json!({"since_ms":-1}),
        json!({"since_ms":1.5}),
        json!({"pid":1}),
        json!({"path":"/etc/passwd"}),
    ] {
        assert_eq!(arguments(&args).unwrap_err().code, "perf_sweep_bad_request");
    }
    assert!(arguments(&json!({"since_ms":u64::MAX,"profile_seconds":30})).is_ok());
}
#[test]
fn perf_sweep_busy_refusal_and_release() {
    let running = AtomicBool::new(false);
    let first = acquire(&running).unwrap();
    let error = match acquire(&running) {
        Err(e) => e,
        Ok(_) => panic!("overlapping sweep admitted"),
    };
    assert_eq!(error.code, "busy");
    drop(first);
    assert!(
        acquire(&running).is_ok(),
        "completion must release the guard"
    );
}
#[test]
fn perf_sweep_success_shape_and_thread_rules() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    std::fs::write(&log, FIXTURE).unwrap();
    let report = collect(
        &Args {
            since_ms: SINCE,
            profile_seconds: 15,
        },
        &log,
        LOG_BYTES,
        LOG_LINES,
        || Ok(profile_fixture()),
    );
    assert_eq!(report["pid"], std::process::id());
    assert_eq!(report["card_sha256"].as_str().unwrap().len(), 64);
    #[cfg(unix)]
    {
        assert!(report["cpu_percent"].as_f64().is_some(), "{report}");
        assert!(report["rss_bytes"].as_u64().unwrap() > 0);
        assert!(report["uptime"].as_str().is_some());
        assert_eq!(report["load_average"].as_array().unwrap().len(), 3);
    }
    assert_eq!(report["since_ms"], SINCE);
    assert!(report["until_ms"].as_u64().unwrap() >= SINCE);
    assert_eq!(report["threads_total"], 2);
    assert_eq!(report["threads"][0]["share_percent"], 75.0);
    assert_eq!(
        report["threads"][0]["top_stacks"][0]["frames"],
        json!(["engine::derive", "work"])
    );
    assert_eq!(
        report["threads"][1]["top_stacks"][0]["frames"],
        json!(["_pthread_start", "???"])
    );
    assert_eq!(report["log"]["truncated"], false);
    assert_eq!(report["log"]["error"], 1);
    assert_eq!(report["log"]["panic"], 1);
}
#[test]
fn perf_sweep_profile_timeout_is_gap_not_lost_log_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    std::fs::write(&log, FIXTURE).unwrap();
    let report = collect(
        &Args {
            since_ms: SINCE,
            profile_seconds: 15,
        },
        &log,
        LOG_BYTES,
        LOG_LINES,
        || Err("profile_timeout: child exceeded its hard deadline".into()),
    );
    assert_eq!(report["threads"], json!([]));
    assert!(report["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|gap| gap["name"] == "profile_timeout"
            && gap["detail"]
                .as_str()
                .unwrap()
                .starts_with("profile_timeout:")));
    assert_eq!(report["log"]["error"], 1);
    assert_eq!(report["log"]["stats"]["slow_total"]["n"], 4);
}
#[test]
fn perf_sweep_log_caps_stop_the_iterator_and_report_truncation() {
    use std::cell::Cell;
    struct Counted<'a> {
        bytes: &'a [u8],
        read: &'a Cell<usize>,
    }
    impl Read for Counted<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.bytes.read(out)?;
            self.read.set(self.read.get() + n);
            Ok(n)
        }
    }
    let bytes = Cell::new(0);
    let source = vec![b'x'; 100_000];
    let report = parse_log(
        Counted {
            bytes: &source,
            read: &bytes,
        },
        0,
        128,
        LOG_LINES,
    )
    .unwrap();
    assert_eq!(report["truncated"], true);
    assert!(
        bytes.get() <= 129,
        "unterminated line read {} bytes",
        bytes.get()
    );
    assert_eq!(report["lines_in_window"], 0);
    let source = "2025-01-01T00:00:01 WARN one\n".repeat(10_000);
    bytes.set(0);
    let report = parse_log(
        Counted {
            bytes: source.as_bytes(),
            read: &bytes,
        },
        0,
        LOG_BYTES,
        2,
    )
    .unwrap();
    assert_eq!(report["truncated"], true);
    assert_eq!(report["scanned_lines"], 2);
    assert_eq!(report["warn"], 2);
    assert!(
        bytes.get() <= 8192,
        "line cap must stop before the rest of the source"
    );
    let exact = "2025-01-01T00:00:01 WARN one\n";
    assert_eq!(
        parse_log(exact.as_bytes(), 0, exact.len() as u64, 1).unwrap()["truncated"],
        false
    );
}
#[test]
fn perf_sweep_parser_matches_script_python_on_shared_fixture() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/perf_sweep_oracle.py"
        ))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/perf_sweep.txt"
        ))
        .arg(iso_seconds(SINCE))
        .output()
        .expect("python3 required for parser parity");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut actual = parse_log(FIXTURE.as_bytes(), SINCE, LOG_BYTES, LOG_LINES).unwrap();
    for key in ["scanned_lines", "scanned_bytes", "truncated"] {
        actual.as_object_mut().unwrap().remove(key);
    }
    assert_eq!(actual, expected);
    assert_eq!(
        actual["stats"]["slow_total"],
        json!({"n":4,"median":250.5,"p95":301,"max":999})
    );
    assert!(actual["warn_shapes"]
        .as_array()
        .unwrap()
        .contains(&json!({"count":2,"shape":"repeat N root=N"})));
}
#[cfg(unix)]
#[test]
fn perf_sweep_native_child_deadline_and_output_bound() {
    let start = Instant::now();
    let error = child_output(
        Path::new("/bin/sh"),
        &["-c", "sleep 5"],
        start + Duration::from_millis(100),
        100,
    )
    .unwrap_err();
    assert!(error.starts_with("timeout:"), "{error}");
    assert!(start.elapsed() < Duration::from_secs(2));
    let error = child_output(
        Path::new("/bin/sh"),
        &["-c", "printf 123456789"],
        Instant::now() + Duration::from_secs(2),
        8,
    )
    .unwrap_err();
    assert!(error.starts_with("output_limit:"), "{error}");
    assert_eq!(
        child_output(
            Path::new("/bin/sh"),
            &["-c", "printf 12345678"],
            Instant::now() + Duration::from_secs(2),
            8
        )
        .unwrap(),
        b"12345678"
    );
}
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a built aft CLI and macOS sampling permission"]
fn perf_sweep_macos_profile_child_no_daemon_fork() {
    use std::process::{Command, Stdio};
    // libtest cannot answer the profiler's --version probe. Keep a real CLI
    // waiting on stdin as the target, with an explicit local debug artifact.
    let exe =
        std::env::var_os("AFT_PERF_TEST_EXE").expect("set AFT_PERF_TEST_EXE to the built aft CLI");
    let dir = tempfile::tempdir().unwrap();
    let mut target = Command::new(&exe)
        .current_dir(dir.path())
        .env("AFT_STORAGE_DIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (result, forks) = crate::effective_path::fork_counter::forks_during(|| {
        child_output(
            Path::new(&exe),
            &[
                "profile",
                "--pid",
                &target.id().to_string(),
                "--seconds",
                "1",
                "--dsym",
                Path::new(&exe).to_str().unwrap(),
                "--json",
            ],
            Instant::now() + Duration::from_secs(20),
            PROFILE_BYTES,
        )
    });
    let _ = target.kill();
    let _ = target.wait();
    assert_eq!(forks, 0, "the daemon must never fork to sample itself");
    let profile: Value =
        serde_json::from_slice(&result.expect("real profile child must return JSON")).unwrap();
    assert!(!summarize_profile(profile).unwrap().0.is_empty());
}
