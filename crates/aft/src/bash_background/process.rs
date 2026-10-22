#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(unix)]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
/// Shared process-termination helpers for both foreground bash and background
/// bash tasks. Extracted to avoid duplication between `commands/bash.rs` and
/// `bash_background/registry.rs`.
///
/// Termination is graceful-first: SIGTERM + 2-second grace period, then
/// SIGKILL on Unix. On Windows, `taskkill /T /F` kills the entire process tree.
use std::process::Child;
#[cfg(windows)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::thread;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

pub const TERMINATE_GRACE: Duration = Duration::from_secs(2);
pub const LIVE_DESCENDANT_CAP: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveDescendant {
    pub pid: u32,
    pub comm: String,
    pub argv0: String,
}

/// Enumerate surviving members of a task's process group. `None` means the
/// platform has no process-group enumeration support for these tasks today.
pub fn live_process_group_members(pgid: i32) -> Option<(Vec<LiveDescendant>, usize)> {
    #[cfg(target_os = "linux")]
    let mut members = linux_process_group_members(pgid);
    #[cfg(target_os = "macos")]
    let mut members = macos_process_group_members(pgid);
    #[cfg(windows)]
    {
        let _ = pgid;
        return None;
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let _ = pgid;
        return None;
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        members.sort_by_key(|member| member.pid);
        let omitted = members.len().saturating_sub(LIVE_DESCENDANT_CAP);
        members.truncate(LIVE_DESCENDANT_CAP);
        Some((members, omitted))
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn systemd_scope_argv(
    systemd_run: &Path,
    executable: &Path,
    args: &[OsString],
) -> (PathBuf, Vec<OsString>) {
    let mut wrapped = ["--user", "--scope", "--collect", "--quiet"]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    wrapped.push(executable.as_os_str().to_os_string());
    wrapped.extend_from_slice(args);
    (systemd_run.to_path_buf(), wrapped)
}

#[cfg(target_os = "linux")]
fn select_systemd_scope_launcher_with(
    find: impl FnOnce() -> Option<PathBuf>,
    probe: impl FnOnce(&Path) -> bool,
) -> Option<PathBuf> {
    let launcher = find()?;
    probe(&launcher).then_some(launcher)
}

#[cfg(target_os = "linux")]
pub(crate) fn select_systemd_scope_launcher() -> Option<PathBuf> {
    select_systemd_scope_launcher_with(
        || which::which("systemd-run").ok(),
        |launcher| probe_systemd_scope(launcher, Duration::from_secs(1)),
    )
}

#[cfg(target_os = "linux")]
fn probe_systemd_scope(launcher: &Path, limit: Duration) -> bool {
    let mut command = std::process::Command::new(launcher);
    command
        .args(["--user", "--scope", "--collect", "--quiet", "/bin/true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    start_new_session(&mut command);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if Instant::now() < deadline => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            _ => {
                // The availability probe must not retain a launch indefinitely.
                // Kill its session as well as the client, then reap the client.
                unsafe {
                    libc::killpg(child.id() as i32, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_process_group_members(pgid: i32) -> Vec<LiveDescendant> {
    // An empty group needs no /proc walk. EPERM still means the group exists
    // and must be enumerated, including reparented descendants.
    if pgid > 0 && unsafe { libc::kill(-pgid, 0) } == -1 && io_group_missing() {
        return Vec::new();
    }
    #[cfg(test)]
    PROC_SCAN_COUNT.with(|count| count.set(count.get() + 1));
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| linux_process_member(pid, pgid))
        .collect()
}

#[cfg(target_os = "linux")]
fn io_group_missing() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(all(test, target_os = "linux"))]
thread_local! { static PROC_SCAN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(target_os = "linux")]
fn linux_process_member(pid: u32, expected_pgid: i32) -> Option<LiveDescendant> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(") ")?;
    let comm = stat.get(open + 1..close)?.to_string();
    let mut fields = stat.get(close + 2..)?.split_whitespace();
    let state = fields.next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse::<i32>().ok()?;
    if pgrp != expected_pgid || state == "Z" {
        return None;
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let argv0 = cmdline
        .split(|byte| *byte == 0)
        .next()
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .unwrap_or_else(|| comm.clone());
    Some(LiveDescendant { pid, comm, argv0 })
}

#[cfg(target_os = "macos")]
fn macos_process_group_members(pgid: i32) -> Vec<LiveDescendant> {
    use std::ffi::{c_char, c_int, c_void};

    #[link(name = "proc")]
    extern "C" {
        fn proc_listpgrppids(pgrpid: u32, buffer: *mut c_void, buffersize: c_int) -> c_int;
        fn proc_name(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    }

    if pgid <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0_u32; 16_384];
    let count = unsafe {
        proc_listpgrppids(
            pgid as u32,
            pids.as_mut_ptr().cast(),
            i32::try_from(pids.len() * std::mem::size_of::<u32>()).unwrap_or(i32::MAX),
        )
    };
    if count <= 0 {
        return Vec::new();
    }
    pids.truncate(count as usize);
    pids.into_iter()
        .filter(|pid| *pid != 0)
        .filter_map(|pid| {
            let mut name = [0 as c_char; 1024];
            let name_len =
                unsafe { proc_name(pid as c_int, name.as_mut_ptr().cast(), name.len() as u32) };
            if name_len <= 0 {
                return None;
            }
            let comm = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let argv0 = macos_argv0(pid).unwrap_or_else(|| comm.clone());
            Some(LiveDescendant { pid, comm, argv0 })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn macos_argv0(pid: u32) -> Option<String> {
    const CTL_KERN: libc::c_int = 1;
    const KERN_PROCARGS2: libc::c_int = 49;
    let mut mib = [CTL_KERN, KERN_PROCARGS2, i32::try_from(pid).ok()?];
    let mut size = 0_usize;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size <= std::mem::size_of::<libc::c_int>()
    {
        return None;
    }
    let mut bytes = vec![0_u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    bytes.truncate(size);
    let mut cursor = std::mem::size_of::<libc::c_int>();
    cursor += bytes.get(cursor..)?.iter().position(|byte| *byte == 0)?;
    while bytes.get(cursor).is_some_and(|byte| *byte == 0) {
        cursor += 1;
    }
    let end = cursor
        + bytes
            .get(cursor..)?
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len().saturating_sub(cursor));
    (end > cursor).then(|| String::from_utf8_lossy(&bytes[cursor..end]).into_owned())
}

/// The Unix payload wrapper runs the user's command in the same shell that
/// owns the pipeline. Bash exposes per-segment statuses as `PIPESTATUS`, while
/// zsh exposes them as `pipestatus`; plain POSIX sh has neither and therefore
/// receives the original command without a capture suffix. The status stream
/// is published in the task's already-writable io directory only after the
/// pipeline finishes. Temporarily closing an inherited capture descriptor is
/// insufficient: some shells pass their saved redirection copy to pipeline
/// children. Exit/failure markers belong only to the outer wrapper. Windows
/// shells have no portable per-segment status equivalent.
#[cfg(unix)]
// POSIX command exits cannot produce this signed marker. Windows uses the full
// 32-bit exit range, so neither this gate nor its marker interpretation is used there.
pub(crate) const BOOTSTRAP_ABORTED_MARKER: i32 = i32::MIN;

#[cfg(unix)]
pub(crate) const PAYLOAD_WRAPPER: &[u8] = br#"#!/bin/sh
shell=$1
command=$2
exit_fd=$3
pipeline_status_path=$4
pipeline_shell=$5
if [ "${6-}" = "wait-for-identity" ]; then
  if ! IFS= read -r __aft_start || [ "$__aft_start" != "run" ]; then
    printf '%s' '-2147483648' >&"$exit_fd"
    exit 125
  fi
  exec </dev/null
fi
case "$pipeline_status_path:$pipeline_shell" in
  ?*:bash)
    "$shell" -c "__aft_pipeline_status=\$1
shift
$command
__aft_code=\$? __aft_ps=(\"\${PIPESTATUS[@]}\")
__aft_status_tmp=\"\${__aft_pipeline_status}.tmp.\$\$\"
(umask 077; set -C; printf '%s\\n' \"\${__aft_ps[@]}\" > \"\$__aft_status_tmp\") 2>/dev/null &&
  mv -f -- \"\$__aft_status_tmp\" \"\$__aft_pipeline_status\" 2>/dev/null ||
  rm -f -- \"\$__aft_status_tmp\" 2>/dev/null
exit \"\$__aft_code\"" "$shell" "$pipeline_status_path" 3>&- 4>&-
    ;;
  ?*:zsh)
    "$shell" -c "__aft_pipeline_status=\$1
shift
$command
__aft_code=\$? __aft_ps=(\"\${pipestatus[@]}\")
__aft_status_tmp=\"\${__aft_pipeline_status}.tmp.\$\$\"
(umask 077; set -C; printf '%s\\n' \"\${__aft_ps[@]}\" > \"\$__aft_status_tmp\") 2>/dev/null &&
  mv -f -- \"\$__aft_status_tmp\" \"\$__aft_pipeline_status\" 2>/dev/null ||
  rm -f -- \"\$__aft_status_tmp\" 2>/dev/null
exit \"\$__aft_code\"" "$shell" "$pipeline_status_path" 3>&- 4>&-
    ;;
  *)
    # POSIX sh has no per-segment pipeline status array; preserve the original
    # invocation instead of changing its semantics with a best-effort guess.
    "$shell" -c "$command" 3>&- 4>&-
    ;;
esac
code=$?
printf "%s" "$code" >&"$exit_fd"
exit "$code"
"#;

#[cfg(unix)]
pub(crate) fn pipeline_shell_kind(shell: &Path) -> Option<&'static str> {
    match shell.file_name().and_then(|name| name.to_str()) {
        Some("bash") => Some("bash"),
        Some("zsh") => Some("zsh"),
        _ => None,
    }
}

/// Start a non-PTY child in a new session before exec. The bridge can inherit a
/// harness's controlling terminal even though the child uses pipe-backed stdio;
/// an interactive shell could otherwise perform job control against that terminal.
#[cfg(unix)]
pub(crate) fn start_new_session(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(unix)]
pub fn terminate_process(child: &mut Child) {
    let pgid = child.id() as i32;
    terminate_pgid(pgid, Some(child));
}

#[cfg(unix)]
pub fn terminate_pgid(pgid: i32, mut child: Option<&mut Child>) {
    terminate_pgid_with(pgid, || {
        child
            .as_deref_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))))
    });
}

#[cfg(unix)]
pub(crate) fn terminate_pgid_with(pgid: i32, mut leader_exited: impl FnMut() -> bool) {
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    let grace_started = Instant::now();
    while grace_started.elapsed() < TERMINATE_GRACE {
        if leader_exited() {
            // The direct child (process-group leader) exited. Stop waiting,
            // but still SIGKILL the whole group below — a descendant that
            // ignored SIGTERM can outlive the leader (the wrapper-shell /
            // CLI-spawns-child orphan class). killpg on an already-empty
            // group is a harmless ESRCH.
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

#[cfg(windows)]
pub fn terminate_process(child: &mut Child) {
    terminate_pid(child.id());
}

#[cfg(windows)]
pub fn terminate_pid(pid: u32) {
    let pid = pid.to_string();
    let _ = Command::new("taskkill")
        .args(["/PID", &pid, "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    (unsafe { libc::kill(pid, 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> bool {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> Handle;
        fn GetExitCodeProcess(hProcess: Handle, lpExitCode: *mut u32) -> i32;
        fn CloseHandle(hObject: Handle) -> i32;
    }

    const FALSE: i32 = 0;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 0x103;

    if pid == 0 {
        return false;
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code) != 0 && exit_code == STILL_ACTIVE;
        let _ = CloseHandle(handle);
        ok
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn documented_termination_grace_matches_policy() {
        let comment = include_str!("process.rs")
            .lines()
            .find(|line| line.starts_with("/// Termination is graceful-first:"))
            .unwrap();
        assert!(comment.contains(&format!(
            "{}-second grace period",
            super::TERMINATE_GRACE.as_secs()
        )));
    }
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_scope_argv_wraps_the_tool_shell() {
        let args = vec![OsString::from("-c"), OsString::from("printf ready")];
        let (program, wrapped) = systemd_scope_argv(
            Path::new("/usr/bin/systemd-run"),
            Path::new("/bin/sh"),
            &args,
        );
        assert_eq!(program, PathBuf::from("/usr/bin/systemd-run"));
        assert_eq!(
            wrapped,
            vec![
                "--user",
                "--scope",
                "--collect",
                "--quiet",
                "/bin/sh",
                "-c",
                "printf ready",
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_scope_falls_back_when_user_manager_is_unreachable() {
        let probes = std::cell::Cell::new(0);
        let selected = select_systemd_scope_launcher_with(
            || Some(PathBuf::from("/usr/bin/systemd-run")),
            |_| {
                probes.set(probes.get() + 1);
                false
            },
        );
        assert_eq!(selected, None);
        assert_eq!(probes.get(), 1);
    }

    #[test]
    fn is_process_alive_returns_true_for_self() {
        assert!(is_process_alive(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn payload_wrapper_captures_bash_and_zsh_pipeline_statuses() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        use std::process::Command;

        let mut shells = vec![("/bin/bash", "bash")];
        if Path::new("/bin/zsh").is_file() {
            shells.push(("/bin/zsh", "zsh"));
        }

        for (shell, kind) in shells {
            let temp = tempfile::tempdir().expect("create wrapper test directory");
            let exit_path = temp.path().join("exit");
            let status_path = temp.path().join("pipeline ' status");
            let exit_file = std::fs::File::create(&exit_path).expect("create exit marker");
            let status_file = std::fs::File::create(&status_path).expect("create status file");
            let original_status_inode = status_file.metadata().unwrap().ino();
            let failure_file = std::fs::File::create(temp.path().join("failure")).unwrap();
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                std::str::from_utf8(PAYLOAD_WRAPPER).expect("wrapper is UTF-8"),
                "aft-payload-wrapper",
                shell,
                "false | true",
                "3",
                status_path.to_str().unwrap(),
                kind,
            ]);
            crate::sandbox_spawn::apply_marker_fd_allowlist(
                &mut command,
                exit_file.as_raw_fd(),
                failure_file.as_raw_fd(),
            )
            .unwrap();
            let result = command.status().expect("run payload wrapper");
            drop(exit_file);
            drop(status_file);
            assert!(result.success(), "wrapper failed for {kind}");
            assert_eq!(std::fs::read_to_string(&status_path).unwrap(), "1\n0\n");
            assert_ne!(
                std::fs::metadata(&status_path).unwrap().ino(),
                original_status_inode,
                "status must be published through an atomic rename"
            );
            assert_eq!(std::fs::read_to_string(&exit_path).unwrap(), "0");
        }
    }

    #[cfg(unix)]
    #[test]
    fn payload_bootstrap_eof_and_wrong_release_never_execute_command() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::process::{Command, Stdio};
        for release in [None, Some(b"not-run\n".as_slice())] {
            let temp = tempfile::tempdir().unwrap();
            let exit_path = temp.path().join("exit");
            let exit = std::fs::File::create(&exit_path).unwrap();
            let failure = std::fs::File::create(temp.path().join("failure")).unwrap();
            let marker = temp.path().join("must-not-run");
            let payload = format!("printf ran > '{}'", marker.display());
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    std::str::from_utf8(PAYLOAD_WRAPPER).unwrap(),
                    "aft-payload-wrapper",
                    "/bin/sh",
                    &payload,
                    "3",
                    "",
                    "",
                    "wait-for-identity",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            crate::sandbox_spawn::apply_marker_fd_allowlist(
                &mut command,
                exit.as_raw_fd(),
                failure.as_raw_fd(),
            )
            .unwrap();
            let mut child = command.spawn().unwrap();
            let mut input = child.stdin.take().unwrap();
            if let Some(release) = release {
                input.write_all(release).unwrap();
            }
            drop(input);
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(125));
            assert!(
                !marker.exists(),
                "bootstrap must not run user code without release"
            );
            assert_eq!(std::fs::read_to_string(exit_path).unwrap(), "-2147483648");
        }
    }

    #[cfg(unix)]
    #[test]
    fn payload_wrapper_keeps_control_fds_out_of_user_commands() {
        use std::os::fd::AsRawFd;
        use std::process::Command;

        let mut shells = vec![("/bin/sh", ""), ("/bin/bash", "bash")];
        if Path::new("/bin/zsh").is_file() {
            shells.push(("/bin/zsh", "zsh"));
        }
        for (shell, kind) in shells {
            for pipeline in [false, true] {
                if pipeline && kind.is_empty() {
                    continue;
                }
                let temp = tempfile::tempdir().unwrap();
                let exit_path = temp.path().join("exit");
                let status_path = temp.path().join("status");
                let exit = std::fs::File::create(&exit_path).unwrap();
                let failure = std::fs::File::create(temp.path().join("failure")).unwrap();
                let _status = std::fs::File::create(&status_path).unwrap();
                let probe = temp.path().join("probe");
                // A redirection probe would create Bash's own saved stderr fd
                // and falsely report it as an inherited control descriptor.
                crate::executable_cache::write_executable(&probe, "#!/bin/bash\nif [ ! -c /dev/fd/0 ] || IFS= read -r input; then\n  echo 'command stdin was not restored to /dev/null' >&2\n  exit 44\nfi\nif [ ! -e /dev/fd/1 ] || [ ! -e /dev/fd/2 ]; then\n  echo 'descriptor lookup cannot see open stdio' >&2\n  exit 43\nfi\nfor fd in 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do\n  if [ -e \"/dev/fd/$fd\" ]; then\n    echo \"leaked control fd $fd\" >&2\n    exit 42\n  fi\ndone\nprintf clean\n".as_bytes()).unwrap();
                let payload = if pipeline {
                    format!("'{}' | cat", probe.display())
                } else {
                    format!("'{}'", probe.display())
                };
                let mut command = Command::new("/bin/sh");
                command.args([
                    "-c",
                    std::str::from_utf8(PAYLOAD_WRAPPER).unwrap(),
                    "aft-payload-wrapper",
                    shell,
                    &payload,
                    "3",
                    if pipeline {
                        status_path.to_str().unwrap()
                    } else {
                        ""
                    },
                    kind,
                    "wait-for-identity",
                ]);
                crate::sandbox_spawn::apply_marker_fd_allowlist(
                    &mut command,
                    exit.as_raw_fd(),
                    failure.as_raw_fd(),
                )
                .unwrap();
                use std::io::Write;
                use std::process::Stdio;
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let mut child = command.spawn().unwrap();
                child.stdin.take().unwrap().write_all(b"run\n").unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{shell} pipeline={pipeline}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    output.stdout,
                    b"clean",
                    "{shell} pipeline={pipeline}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(std::fs::read_to_string(&exit_path).unwrap(), "0");
                if pipeline {
                    assert_eq!(std::fs::read_to_string(&status_path).unwrap(), "0\n0\n");
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_pipeline_capture_preserves_shell_pipeline_semantics() {
        use std::os::fd::AsRawFd;
        use std::process::{Command, Stdio};

        let mut shells = vec![("/bin/bash", "bash")];
        if Path::new("/bin/zsh").is_file() {
            shells.push(("/bin/zsh", "zsh"));
        }
        for (shell, kind) in shells {
            for payload in [
                "! false | true",
                "time true | cat",
                "printf '%s' \"$0:${1-unset}:$#\" | cat",
                "printf 'sample\\n' | { read value; trap 'printf \"%s\\n\" \"$value\"' EXIT; }",
            ] {
                // The uninstrumented shell is the reference for shell-specific
                // last-stage scope, pipeline negation, timing and positional args.
                let reference = Command::new(shell)
                    .args(["-c", payload])
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                let temp = tempfile::tempdir().unwrap();
                let exit_path = temp.path().join("exit");
                let status_path = temp.path().join("pipeline-status");
                let exit = std::fs::File::create(&exit_path).unwrap();
                let failure = std::fs::File::create(temp.path().join("failure")).unwrap();
                let mut command = Command::new("/bin/sh");
                command.args([
                    "-c",
                    std::str::from_utf8(PAYLOAD_WRAPPER).unwrap(),
                    "aft-payload-wrapper",
                    shell,
                    payload,
                    "3",
                    status_path.to_str().unwrap(),
                    kind,
                ]);
                crate::sandbox_spawn::apply_marker_fd_allowlist(
                    &mut command,
                    exit.as_raw_fd(),
                    failure.as_raw_fd(),
                )
                .unwrap();
                let captured = command.stdin(Stdio::null()).output().unwrap();
                assert_eq!(
                    captured.status.code(),
                    reference.status.code(),
                    "{shell}: {payload}"
                );
                assert_eq!(captured.stdout, reference.stdout, "{shell}: {payload}");
                assert!(status_path.is_file(), "{shell}: capture was not published");
                assert_eq!(
                    std::fs::read_to_string(exit_path).unwrap(),
                    reference.status.code().unwrap().to_string()
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_pty_child_starts_as_its_own_session_and_process_group_leader() {
        use std::process::Command;

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        start_new_session(&mut command);
        let mut child = command.spawn().expect("spawn isolated child");
        let pid = child.id() as libc::pid_t;

        assert_eq!(unsafe { libc::getsid(pid) }, pid);
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);

        terminate_process(&mut child);
        let _ = child.wait();
    }

    #[test]
    fn is_process_alive_returns_false_for_dead_pid() {
        #[cfg(unix)]
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "true"])
            .spawn()
            .expect("spawn true");

        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "exit 0"])
            .spawn()
            .expect("spawn cmd");

        let pid = child.id();
        child.wait().expect("wait for child");

        assert!(!is_process_alive(pid));
    }

    /// Regression: when the process-group LEADER exits during the SIGTERM grace
    /// window, `terminate_pgid` must still SIGKILL the rest of the group. A
    /// TERM-ignoring descendant (the wrapper-shell / CLI-spawns-child orphan
    /// class) used to survive because the old code returned the instant the
    /// leader was reaped, skipping the group SIGKILL.
    #[cfg(unix)]
    #[test]
    fn terminate_pgid_kills_term_ignoring_descendant_after_leader_exits() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("desc.pid");
        let ready = dir.path().join("ready");

        // Leader becomes its own process-group leader (setsid → pgid == pid).
        // It backgrounds a descendant shell that ignores SIGTERM, signals
        // readiness (so the trap is definitely installed before we terminate),
        // then sleeps. The leader waits for readiness and exits — so by the time
        // we call terminate_pgid, the leader is gone and only SIGKILL can reap
        // the descendant.
        let script = format!(
            "sh -c \"trap '' TERM; echo \\$$ > '{pid}'; touch '{ready}'; sleep 30\" & \
             while [ ! -f '{ready}' ]; do sleep 0.02; done; exit 0",
            pid = pidfile.display(),
            ready = ready.display(),
        );
        let mut leader = unsafe {
            std::process::Command::new("/bin/sh")
                .args(["-c", &script])
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()
                .expect("spawn leader")
        };
        let pgid = leader.id() as i32;

        // Wait for the descendant to be ready (trap installed + pid written).
        let start = Instant::now();
        while !ready.exists() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        let desc_pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("descendant pid file")
            .trim()
            .parse()
            .expect("parse descendant pid");
        assert!(is_process_alive(desc_pid), "descendant should be alive");

        terminate_pgid(pgid, Some(&mut leader));

        // The TERM-ignoring descendant must be gone (SIGKILL'd via the group).
        let start = Instant::now();
        while is_process_alive(desc_pid) && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !is_process_alive(desc_pid),
            "TERM-ignoring descendant must be SIGKILLed when the group is terminated"
        );
    }
}

/// Check that a persisted PID still names the process instance launched for a task.
/// On platforms without process start-time inspection, a live PID is protected;
/// deleting a live task is worse than temporarily retaining a recycled PID.
pub fn is_recorded_process_alive(pid: u32, task_started_at_ms: u64) -> bool {
    const PROCESS_START_GRACE_MS: u64 = 30_000;

    if !is_process_alive(pid) {
        return false;
    }
    crate::root_cache::process_start_time_ms(pid).is_none_or(|started_at_ms| {
        started_at_ms <= task_started_at_ms.saturating_add(PROCESS_START_GRACE_MS)
    })
}
#[cfg(target_os = "linux")]
#[test]
fn lifecycle_empty_group_avoids_proc_scan() {
    let mut command = std::process::Command::new("/bin/true");
    start_new_session(&mut command);
    let mut child = command.spawn().unwrap();
    let pgid = child.id() as i32;
    child.wait().unwrap();
    PROC_SCAN_COUNT.with(|count| count.set(0));
    assert!(linux_process_group_members(pgid).is_empty());
    assert_eq!(
        PROC_SCAN_COUNT.with(std::cell::Cell::get),
        0,
        "empty group required a host-wide scan"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn lifecycle_scope_probe_deadline_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let launcher = dir.path().join("systemd-run");
    crate::executable_cache::write_executable(&launcher, b"#!/bin/sh\nsleep 0.1\nexit 0\n")
        .unwrap();
    assert!(
        !probe_systemd_scope(&launcher, Duration::ZERO),
        "expired probe still reported availability"
    );
}
