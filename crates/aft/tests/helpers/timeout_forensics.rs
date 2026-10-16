//! Best-effort child stacks for response timeouts. No diagnostic operation may
//! keep a failed integration test waiting indefinitely.

use std::io::{Read, Seek, SeekFrom};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const CAPTURE_BUDGET: Duration = Duration::from_secs(10);
const OUTPUT_LIMIT: u64 = 256 * 1024;

pub(super) fn capture_stacks(pid: u32) -> String {
    let deadline = Instant::now() + CAPTURE_BUDGET;
    let (sender, receiver) = mpsc::channel();
    // The outer deadline also covers filesystem reads and tool discovery. A
    // stalled /proc read must not defeat the subprocess watchdog.
    let worker = std::thread::Builder::new()
        .name("aft-timeout-stacks".into())
        .spawn(move || {
            let _ = sender.send(collect_stacks(pid, deadline));
        });
    if let Err(error) = worker {
        return format!("stack capture unavailable: could not start worker: {error}");
    }
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(report) => report,
        Err(error) => format!(
            "stack capture stopped after 10s ({error}); collectors: {}",
            collector_names()
        ),
    }
}

fn collector_names() -> &'static str {
    if cfg!(target_os = "macos") {
        "sample"
    } else if cfg!(target_os = "linux") {
        "/proc/<pid>/task/*/stack, eu-stack, gdb"
    } else {
        "no stack collector for this platform"
    }
}

fn collect_stacks(pid: u32, deadline: Instant) -> String {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("sample");
        command.args([pid.to_string(), "3".into()]);
        run_sampler("sample", command, deadline).1
    }
    #[cfg(target_os = "linux")]
    {
        let task_path = format!("/proc/{pid}/task");
        let mut report = String::new();
        let mut have_kernel_stack = false;
        match std::fs::read_dir(&task_path) {
            Ok(tasks) => {
                for task in tasks {
                    if Instant::now() >= deadline {
                        report.push_str("/proc stack capture deadline reached\n");
                        break;
                    }
                    match task {
                        Ok(task) => {
                            let path = task.path().join("stack");
                            match std::fs::read_to_string(&path) {
                                Ok(stack) if !stack.trim().is_empty() => {
                                    have_kernel_stack = true;
                                    report.push_str(&format!("{}:\n{stack}\n", path.display()));
                                }
                                Ok(_) => report.push_str(&format!("{}: empty\n", path.display())),
                                Err(error) => report.push_str(&format!(
                                    "{}: unavailable: {error}\n",
                                    path.display()
                                )),
                            }
                        }
                        Err(error) => report.push_str(&format!("{task_path}: {error}\n")),
                    }
                    if report.len() as u64 >= OUTPUT_LIMIT {
                        report.push_str("/proc stack output limit reached\n");
                        break;
                    }
                }
            }
            Err(error) => report.push_str(&format!("{task_path}/*/stack: unavailable: {error}\n")),
        }
        collect_linux_capture(report, have_kernel_stack, pid, deadline, run_sampler)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (pid, deadline);
        collector_names().into()
    }
}

#[cfg(target_os = "linux")]
fn collect_linux_capture(
    mut report: String,
    have_kernel_stack: bool,
    pid: u32,
    deadline: Instant,
    mut run: impl FnMut(&str, Command, Instant) -> (bool, String),
) -> String {
    report.push_str(if have_kernel_stack {
        "/proc stack collector: captured kernel stacks\n"
    } else {
        "/proc stack collector: no readable kernel stacks\n"
    });
    // Kernel stacks omit the Rust calls behind a hang. Keep them, then try
    // userspace debuggers while the same capture deadline still allows it.
    let mut eu_stack = Command::new("eu-stack");
    eu_stack.args(["-p", &pid.to_string()]);
    let (ok, output) = run("eu-stack", eu_stack, deadline);
    report.push_str(&output);
    if !ok {
        let mut gdb = Command::new("gdb");
        gdb.args([
            "--batch",
            "--nx",
            "-iex",
            "set auto-load off",
            "-p",
            &pid.to_string(),
            "-ex",
            "thread apply all bt",
            "-ex",
            "detach",
        ]);
        report.push_str(&run("gdb", gdb, deadline).1);
    }
    report
}

fn run_sampler(name: &str, mut command: Command, deadline: Instant) -> (bool, String) {
    if Instant::now() >= deadline {
        return (
            false,
            format!("{name}: skipped: stack capture deadline reached\n"),
        );
    }
    let result = (|| -> std::io::Result<(bool, String)> {
        // A file, rather than a pipe, avoids a sampler or its descendants
        // blocking on full stdout or retaining a pipe after the sampler exits.
        let mut output = tempfile::tempfile()?;
        command
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output.try_clone()?);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut sampler = Sampler(Some(command.spawn()?));
        let (ok, status) = loop {
            if let Some(status) = sampler.0.as_mut().unwrap().try_wait()? {
                break (status.success(), status.to_string());
            }
            if Instant::now() >= deadline {
                break (
                    false,
                    "timed out at stack capture deadline; sampler process group killed".into(),
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        drop(sampler);
        output.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        (&mut output).take(OUTPUT_LIMIT).read_to_end(&mut bytes)?;
        let clipped = if output.metadata()?.len() > OUTPUT_LIMIT {
            "\n[sampler output clipped at 256 KiB]"
        } else {
            ""
        };
        Ok((
            ok,
            format!(
                "{name}: {status}\n{}{clipped}\n",
                String::from_utf8_lossy(&bytes)
            ),
        ))
    })();
    result.unwrap_or_else(|error| (false, format!("{name}: unavailable: {error}\n")))
}

struct Sampler(Option<Child>);

impl Drop for Sampler {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        // Kill only the sampler's new process group, never the aft child. Reap
        // off-thread: even waiting for a killed debugger must not extend the cap.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = std::thread::Builder::new()
            .name("aft-sampler-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn stack_sampler_deadline_kills_process_group() {
        let started = Instant::now();
        let temp = tempfile::tempdir().unwrap();
        let child_pid_file = temp.path().join("sleep-child.pid");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "sleep 2 & child=$!; printf '%s\\n' \"$child\" > \"$PID_FILE\"; printf 'sampler entered\\n'; wait \"$child\"",
            ])
            .env("PID_FILE", &child_pid_file);
        let (ok, report) = run_sampler(
            "sleeping-sampler",
            command,
            started + Duration::from_millis(100),
        );
        let child_pid: libc::pid_t = std::fs::read_to_string(&child_pid_file)
            .expect("sampler writes its child PID before waiting")
            .trim()
            .parse()
            .expect("sampler child PID is numeric");
        let mut child_cleanup = ProcessCleanup(Some(child_pid));
        assert!(!ok, "{report}");
        assert!(report.contains("timed out"), "{report}");
        assert!(report.contains("sampler entered"), "{report}");
        assert!(started.elapsed() < Duration::from_secs(1), "{report}");
        let stopped = wait_for_process_stop(child_pid, Instant::now() + Duration::from_millis(300));
        if stopped {
            child_cleanup.0 = None;
        }
        assert!(
            stopped,
            "sampler child {child_pid} outlived process-group timeout: {report}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_kernel_stacks_also_try_userspace_collector() {
        let mut collectors = Vec::new();
        let report = collect_linux_capture(
            "/proc/123/task/123/stack:\nkernel-frame\n".into(),
            true,
            123,
            Instant::now() + CAPTURE_BUDGET,
            |name, _command, _deadline| {
                collectors.push(name.to_string());
                (true, format!("{name}: fake userspace frame\n"))
            },
        );
        assert_eq!(collectors, ["eu-stack"]);
        assert!(report.contains("kernel-frame"), "{report}");
        assert!(
            report.contains("eu-stack: fake userspace frame"),
            "{report}"
        );
    }

    struct ProcessCleanup(Option<libc::pid_t>);

    impl Drop for ProcessCleanup {
        fn drop(&mut self) {
            let Some(pid) = self.0 else {
                return;
            };
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }

    fn wait_for_process_stop(pid: libc::pid_t, deadline: Instant) -> bool {
        loop {
            let exists = match unsafe { libc::kill(pid, 0) } {
                0 => true,
                _ => std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH),
            };
            if !exists {
                return true;
            }
            // A zombie cannot execute even if a minimal container's init has
            // not reaped it yet.
            #[cfg(target_os = "linux")]
            if std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')
                        .and_then(|(_, rest)| rest.split_whitespace().next())
                        .map(|state| state == "Z" || state == "X")
                })
                .unwrap_or(false)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn stack_sampler_reports_missing_tool_by_name() {
        let absent = tempfile::tempdir().unwrap().path().join("absent-sampler");
        let (ok, report) = run_sampler(
            "absent-sampler",
            Command::new(absent),
            Instant::now() + CAPTURE_BUDGET,
        );
        assert!(!ok, "{report}");
        assert!(report.contains("absent-sampler: unavailable"), "{report}");
    }

    #[test]
    fn stack_sampler_preserves_output_and_exit_failure() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "echo stack-frame; echo attach-denied >&2; exit 7"]);
        let (ok, report) = run_sampler("failing-sampler", command, Instant::now() + CAPTURE_BUDGET);
        assert!(!ok, "{report}");
        for expected in ["stack-frame", "attach-denied", "7"] {
            assert!(report.contains(expected), "{report}");
        }
    }
}
