//! Prompt-free real macOS launch coverage. Only process attribution and ordinary
//! temporary files/terminal IO are observed; no TCC-protected API is exercised.

use super::helpers::{user_config, AftProcess};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SESSION: &str = "privacy-spawn";

fn macos_native_fixture_project() -> tempfile::TempDir {
    // First-exec assessment of a new executable under macOS's /var/folders
    // TMPDIR can consume the command's entire one-second timeout. Keep the
    // compiled probe under /tmp, as the native bridge fixtures do.
    tempfile::Builder::new()
        .prefix("aft-privacy-spawn-")
        .tempdir_in("/tmp")
        .unwrap()
}

fn configure(aft: &mut AftProcess, project: &Path, storage: &Path, sandbox: bool) {
    let response = aft.send(&json!({
        "id": "cfg-privacy", "session_id": SESSION, "command": "configure",
        "harness": "opencode", "project_root": project, "storage_dir": storage,
        "config": user_config(json!({ "bash": { "disclaim_privacy": true }, "sandbox": { "enabled": sandbox } })),
    }).to_string());
    assert_eq!(response["success"], true, "{response}");
}

fn request(aft: &mut AftProcess, params: Value) -> Value {
    aft.send(
        &json!({"id":"privacy-command", "session_id":SESSION, "command":"bash", "params":params})
            .to_string(),
    )
}

fn terminal(aft: &mut AftProcess, response: Value) -> Value {
    let task = response["task_id"].as_str().expect("task id").to_owned();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = aft.send(&json!({"id":"privacy-status", "session_id":SESSION, "command":"bash_status", "params":{"task_id":task}}).to_string());
        assert_eq!(status["success"], true, "{status}");
        if matches!(
            status["status"].as_str(),
            Some("completed" | "failed" | "timed_out" | "killed")
        ) {
            return status;
        }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn output(status: &Value) -> String {
    if let Some(path) = status["output_path"].as_str() {
        std::fs::read_to_string(path).unwrap()
    } else {
        status["output_preview"]
            .as_str()
            .or_else(|| status["output"].as_str())
            .expect("command output")
            .to_owned()
    }
}

fn attribution_probe(project: &Path) {
    let mut cc = Command::new("/usr/bin/cc")
        .args(["-x", "c", "-o"])
        .arg(project.join("probe"))
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    cc.stdin.take().unwrap().write_all(br#"
#include <dlfcn.h>
#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <sys/ioctl.h>
int main(int argc, char **argv) {
  /* getpgrp needs no process enumeration permission under Seatbelt. */
  if (argc == 2 && strcmp(argv[1], "group") == 0) { printf("%d\n", getpgrp()); return 0; }
  if (argc == 2 && strcmp(argv[1], "identity") == 0) { printf("%d %d %d\n", getpgrp(), getppid(), getuid()); return 0; }
  int (*responsible)(int) = dlsym(RTLD_DEFAULT, "responsibility_get_pid_responsible_for_pid");
  if (!responsible) return 41;
  int group = getpgrp();
  printf("group=%d responsible-group=%d responsible-child=%d session=%d tty=%d\n", group, responsible(group), responsible(getpid()), getsid(0), isatty(0));
  /* Later exec descendants may be attributed to themselves. The launch group's
     leader must be its own responsible process, never the AFT supervisor. */
  if (responsible(group) != group || getsid(0) != group) return 42;
  if (fcntl(3, F_GETFD) >= 0 || fcntl(4, F_GETFD) >= 0) return 43;
  if (isatty(0)) {
    /* Seatbelt need not permit reopening /dev/tty. Query the inherited fd. */
    if (tcgetpgrp(0) != group) return 44;
    struct winsize size;
    if (ioctl(0, TIOCGWINSZ, &size) != 0) return 45;
    printf("size=%dx%d\n", size.ws_row, size.ws_col);
  }
  puts("own-responsible-group");
  return 0;
}
"#).unwrap();
    assert!(cc.wait().unwrap().success());
    super::helpers::warm_executable(&project.join("probe"), &["group"]);
}

#[test]
fn disclaimed_foreground_background_and_pty_preserve_attribution_io_and_exit_with_seatbelt() {
    let project = macos_native_fixture_project();
    let storage = tempfile::tempdir().unwrap();
    attribution_probe(project.path());
    let mut aft = AftProcess::spawn();
    for sandbox in [false, true] {
        configure(&mut aft, project.path(), storage.path(), sandbox);
        for (background, pty) in [(false, false), (true, false), (true, true)] {
            let response = request(
                &mut aft,
                json!({
                    "command": "./probe || exit $?; printf 'env=%s\\n' \"$PRIVACY_TEST\"; pwd; printf stderr >&2; exit 23",
                "background": background, "pty":pty, "wait": !background,
                "pty_rows":29, "pty_cols":97,
                    "compressed":false, "env":{"PRIVACY_TEST":"kept"}, "timeout": 10000,
                }),
            );
            assert_eq!(
                response["success"], true,
                "sandbox={sandbox} pty={pty}: {response}"
            );
            let status = terminal(&mut aft, response);
            let text = output(&status);
            assert_eq!(
                status["exit_code"], 23,
                "sandbox={sandbox} pty={pty}: {status} output={text}"
            );
            assert!(text.contains(if pty { "tty=1" } else { "tty=0" }), "{text}");
            assert!(text.contains("own-responsible-group"), "{text}");
            assert!(text.contains("env=kept"), "{text}");
            assert!(
                text.contains(project.path().canonicalize().unwrap().to_str().unwrap()),
                "{text}"
            );
            if pty {
                assert!(text.contains("stderr"), "{text}");
                assert!(text.contains("size=29x97"), "{text}");
            }
        }
    }
}

fn timeout_group_snapshot(group: i32) -> String {
    match Command::new("/bin/ps")
        .args(["-o", "pid,ppid,pgid,uid,stat,lstart,comm", "-g"])
        .arg(group.to_string())
        .output()
    {
        Ok(output) => format!(
            "ps -g {group}: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ),
        Err(error) => format!("ps -g {group} failed: {error}"),
    }
}

#[test]
fn disclaimed_timeout_kills_foreground_background_and_pty_groups_with_seatbelt() {
    let project = macos_native_fixture_project();
    let storage = tempfile::tempdir().unwrap();
    attribution_probe(project.path());
    let mut aft = AftProcess::spawn_with_env(&[("RUST_LOG", std::ffi::OsStr::new("aft=debug"))]);
    let only_sandboxed_foreground =
        std::env::var("AFT_TEST_PRIVACY_TIMEOUT_ONLY_SANDBOXED_FOREGROUND").as_deref() == Ok("1");
    for sandbox in [false, true] {
        configure(&mut aft, project.path(), storage.path(), sandbox);
        for (background, pty) in [(false, false), (true, false), (true, true)] {
            if only_sandboxed_foreground && (!sandbox || background || pty) {
                continue;
            }
            let case = format!("sandbox={sandbox} background={background} pty={pty}");
            eprintln!("timeout-group starting {case}");
            let group_file = project.path().join("group");
            let _ = std::fs::remove_file(&group_file);
            let response = request(
                &mut aft,
                json!({
                    "command":"./probe identity > group; printf started; trap '' TERM; sleep 30", "background":background, "pty":pty,
                    "compressed":false, "timeout":1000,
                }),
            );
            assert_eq!(response["success"], true, "{response}");
            let status = terminal(&mut aft, response.clone());
            assert_eq!(status["status"], "timed_out", "{status}");
            assert_eq!(status["exit_code"], 124, "{status}");
            if !output(&status).contains("started") {
                let stderr = status["stderr_path"].as_str().map(std::fs::read_to_string);
                eprintln!("{case}: no started marker; launch response={response:#}\ntask stderr={stderr:?}\ngroup file={:?}", std::fs::read_to_string(&group_file));
                let (exit, stderr) = aft.stderr_output();
                eprintln!("{case}: supervisor exit={exit}; sandbox startup diagnostics:\n{stderr}");
                panic!("{case}: command did not reach the started marker: {status:#}");
            }
            let identity = std::fs::read_to_string(group_file).unwrap();
            let ids: Vec<i32> = identity
                .split_whitespace()
                .map(|id| id.parse().unwrap())
                .collect();
            assert_eq!(ids.len(), 3, "{case}: {identity:?}");
            let group = ids[0];
            eprintln!(
                "timeout-group {case}: pgid={group} shell_pid={} uid={}",
                ids[1], ids[2]
            );
            assert!(group > 0);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let result = unsafe { libc::kill(-group, 0) };
                let error = std::io::Error::last_os_error().raw_os_error();
                if result != 0 {
                    if error != Some(libc::ESRCH) {
                        eprintln!("timeout-group {case}: kill(-{group}, 0)={result}, errno={error:?}, identity={identity:?}\n{}\nstatus={status}", timeout_group_snapshot(group));
                    }
                    assert_eq!(error, Some(libc::ESRCH), "{case}, identity={identity:?}");
                    break;
                }
                if Instant::now() >= deadline {
                    panic!("{case}: timed-out process group {group} survived, identity={identity:?}\n{}\nstatus={status}", timeout_group_snapshot(group));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
