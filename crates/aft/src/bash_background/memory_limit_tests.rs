use super::*;

const ALLOCATOR: &str =
    "bash_background::registry::memory_limit_tests::memory_limit_allocator_child";

#[cfg(unix)]
pub(super) const MEMORY_PRESSURE_COMMAND: &str = "python3 -c 'import time; allocation = bytearray(b\"x\" * (192 * 1024 * 1024)); time.sleep(20)'";

#[cfg(unix)]
pub(super) fn memory_request_context(
    project: &Path,
    storage: &Path,
    cap: Option<u64>,
) -> Arc<crate::context::AppContext> {
    let mut config = crate::config::Config::default();
    config.project_root = Some(project.into());
    config.storage_dir = Some(storage.into());
    config.bash.memory_limit_mib = cap;
    config.sandbox.profile.head = serde_json::from_str("\"off\"").unwrap();
    Arc::new(crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ))
}

#[cfg(unix)]
pub(super) fn spawn_memory_request(
    ctx: &crate::context::AppContext,
    request_id: &str,
    remote: Option<super::super::RemoteLaunch>,
) -> crate::protocol::Response {
    spawn_memory_command(ctx, request_id, "exit 0", false, remote)
}

#[cfg(unix)]
pub(super) fn spawn_memory_command(
    ctx: &crate::context::AppContext,
    request_id: &str,
    command: &str,
    pty: bool,
    remote: Option<super::super::RemoteLaunch>,
) -> crate::protocol::Response {
    super::super::spawn(
        request_id,
        "session",
        command,
        super::super::BashShell::Bash,
        PathBuf::from("/bin/sh"),
        None,
        None,
        super::super::HardKill::After(Duration::from_secs(30)),
        ctx,
        false,
        false,
        false,
        pty,
        24,
        80,
        Vec::new(),
        false,
        None,
        remote,
    )
}

#[cfg(unix)]
#[test]
fn memory_limit_interleaved_requests_keep_admitted_caps_across_reload() {
    for new_cap in [None, Some(128)] {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = memory_request_context(project.path(), storage.path(), Some(64));
        let original = ctx.config();
        let a_ctx = ctx.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let request_a = std::thread::spawn(move || {
            let _pin = a_ctx.pin_config_to(original);
            with_memory_prepare_gate_for_test(
                move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(15)).unwrap();
                },
                || spawn_memory_request(&a_ctx, "memory-request-a", None),
            )
        });
        entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        let mut reloaded = ctx.config().as_ref().clone();
        reloaded.bash.memory_limit_mib = new_cap;
        ctx.set_config(reloaded);
        let request_b = {
            let _pin = ctx.pin_config();
            spawn_memory_request(&ctx, "memory-request-b", None)
        };
        release_tx.send(()).unwrap();
        let request_a = request_a.join().unwrap();
        assert!(request_a.success, "{request_a:?}");
        assert!(request_b.success, "{request_b:?}");
        let cap_for = |response: &crate::protocol::Response| {
            let task = ctx
                .bash_background()
                .task_for_test(response.data["task_id"].as_str().unwrap())
                .unwrap();
            let state = task.lock_state().unwrap();
            state
                .metadata
                .memory_policy
                .as_ref()
                .map(|policy| policy.mib)
        };
        assert_eq!(
            cap_for(&request_a),
            Some(64),
            "request A lost its admitted cap"
        );
        assert_eq!(
            cap_for(&request_b),
            new_cap,
            "request B inherited another request's cap"
        );
    }
}

#[test]
fn memory_limit_allocator_child() {
    let Some(mib) = std::env::var("AFT_TEST_ALLOCATE_MIB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return;
    };
    #[cfg(windows)]
    if let Some(ready) = std::env::var_os("AFT_TEST_MEMORY_CHILD_READY") {
        std::fs::write(ready, std::process::id().to_string()).unwrap();
        let release = PathBuf::from(std::env::var_os("AFT_TEST_MEMORY_CHILD_RELEASE").unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        while !release.exists() {
            assert!(Instant::now() < deadline, "allocator was not released");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let allocation = vec![0x5a_u8; mib * 1024 * 1024];
    std::hint::black_box(&allocation);
    std::thread::park_timeout(Duration::from_secs(12));
    std::hint::black_box(&allocation);
}

fn allocate_past_cap(
    pty: bool,
    foreground: bool,
    aggregate: bool,
    require_kernel: bool,
    attach_error: Option<i32>,
) {
    let temp = tempfile::tempdir().unwrap();
    #[cfg(target_os = "linux")]
    let attach_fault = attach_error.map(|errno| {
        super::super::memory_limit::AttachFaultGuard::for_thread(
            temp.path().join("denied-cgroup"),
            errno,
        )
    });
    #[cfg(not(target_os = "linux"))]
    let _ = attach_error;
    let registry = BgTaskRegistry::default();
    #[cfg(target_os = "linux")]
    let sampler_available = super::super::memory_limit::sampling_available_for_test();
    let executable = std::env::current_exe().unwrap();
    #[cfg(unix)]
    let command = format!("'{}' --exact {ALLOCATOR} --nocapture", executable.display());
    #[cfg(windows)]
    let command = format!(
        "& '{}' --exact {ALLOCATOR} --nocapture",
        executable.display()
    );
    #[cfg(unix)]
    let command = if aggregate {
        format!("{command} & {command} & wait")
    } else {
        command
    };
    #[cfg(windows)]
    assert!(!aggregate);
    let mut env = HashMap::from([(
        "AFT_TEST_ALLOCATE_MIB".into(),
        if aggregate { "40" } else { "192" }.into(),
    )]);
    for key in ["PATH", "SystemRoot", "WINDIR", "USERPROFILE", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    #[cfg(unix)]
    let (shell, shell_path) = (super::super::BashShell::Bash, PathBuf::from("/bin/sh"));
    #[cfg(windows)]
    let (shell, shell_path) = (
        super::super::BashShell::Powershell,
        PathBuf::from("pwsh.exe"),
    );
    let timeout = super::super::HardKill::After(Duration::from_secs(10));
    let id = if pty {
        registry
            .spawn_pty_with_shell(
                SpawnPlan::Unsandboxed,
                &command,
                shell,
                shell_path,
                "memory-test".into(),
                temp.path().into(),
                env,
                timeout,
                Some(64),
                temp.path().into(),
                4,
                true,
                false,
                None,
                24,
                80,
            )
            .unwrap()
    } else {
        registry
            .spawn_with_shell_in_slot_typed(
                SpawnPlan::Unsandboxed,
                &command,
                shell,
                shell_path,
                "memory-test".into(),
                temp.path().into(),
                env,
                timeout,
                Some(64),
                temp.path().into(),
                if foreground {
                    super::super::TaskSlot::Foreground
                } else {
                    super::super::TaskSlot::Background { max: 4 }
                },
                true,
                false,
                None,
            )
            .unwrap()
    };
    #[cfg(target_os = "linux")]
    if let Some(fault) = &attach_fault {
        assert_eq!(
            fault.attempts(),
            1,
            "the injected cgroup attach was not attempted"
        );
        let task = registry.task_for_test(&id).unwrap();
        let state = task.lock_state().unwrap();
        let policy = state.metadata.memory_policy.as_ref().unwrap();
        assert_eq!(policy.mib, 64);
        assert_eq!(
            policy.kind,
            "Linux process-group RSS watcher; kernel memory controller unavailable"
        );
        assert!(policy.cgroup.is_none());
    }
    if require_kernel {
        let kernel = registry
            .task_for_test(&id)
            .unwrap()
            .lock_state()
            .unwrap()
            .metadata
            .memory_policy
            .as_ref()
            .unwrap()
            .cgroup
            .is_some();
        if !kernel {
            let _ = registry.kill(&id, "memory-test");
            report_memory_runtime_skip(
                "memory_limit_linux_kernel_stops_group",
                "runtime launch downgraded to the RSS watcher",
            );
            return;
        }
    }
    #[cfg(target_os = "linux")]
    if !sampler_available
        && registry
            .task_for_test(&id)
            .unwrap()
            .lock_state()
            .unwrap()
            .metadata
            .memory_policy
            .as_ref()
            .unwrap()
            .cgroup
            .is_none()
    {
        let _ = registry.kill(&id, "memory-test");
        let thread = std::thread::current();
        report_memory_runtime_skip(
            thread.name().unwrap_or("memory_limit_allocate_past_cap"),
            "neither a kernel controller nor RSS sampling is available",
        );
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let snapshot = registry
            .status(&id, "memory-test", None, None, 4096)
            .unwrap();
        if snapshot.info.status.is_terminal() {
            assert_eq!(snapshot.info.status, BgTaskStatus::Failed, "{snapshot:?}");
            let reason = snapshot.info.status_reason.clone().unwrap_or_default();
            assert!(reason.contains("64 MiB memory cap"), "{reason}");
            assert!(!reason.contains("timeout"), "{reason}");
            let task = registry.task_for_test(&id).unwrap();
            let state = task.lock_state().unwrap();
            let policy = state.metadata.memory_policy.as_ref().unwrap();
            assert!(reason.contains(&policy.kind), "{reason}");
            println!("enforcement: {}; result: {reason}", policy.kind);
            #[cfg(target_os = "linux")]
            if attach_fault.is_some() {
                assert!(
                    snapshot.output_preview.contains(
                        "Linux process-group RSS watcher; kernel memory controller unavailable"
                    ),
                    "{snapshot:?}"
                );
                assert!(
                    !temp.path().join("denied-cgroup").exists(),
                    "failed cgroup was not removed"
                );
                let persisted = super::super::persistence::read_task(&task.paths.json).unwrap();
                let saved_policy = persisted.memory_policy.unwrap();
                assert!(saved_policy.cgroup.is_none());
                assert_eq!(saved_policy.kind, policy.kind);
            }
            drop(state);
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let completions = registry.pending_completions_for_session("memory-test");
                if let Some(completion) = completions
                    .iter()
                    .find(|completion| completion.task_id == id)
                {
                    assert_eq!(completion.status, BgTaskStatus::Failed);
                    assert_eq!(completion.status_reason.as_deref(), Some(reason.as_str()));
                    if !pty {
                        assert!(
                            task.lock_state().unwrap().descendant_sampling_started,
                            "memory stop bypassed the shared survivor check"
                        );
                    }
                    break;
                }
                assert!(Instant::now() < deadline, "no memory-cap completion result");
                std::thread::sleep(Duration::from_millis(25));
            }
            break;
        }
        if Instant::now() >= deadline {
            let _ = registry.kill(&id, "memory-test");
            panic!("memory cap did not stop the command before its timeout");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn memory_limit_stops_background_command() {
    allocate_past_cap(false, false, false, false, None);
}

#[test]
fn memory_limit_stops_foreground_command() {
    allocate_past_cap(false, true, false, false, None);
}

#[test]
fn memory_limit_stops_pty_command() {
    allocate_past_cap(true, false, false, false, None);
}

#[cfg(windows)]
#[test]
fn memory_limit_cmd_waiter_contains_and_kills_powershell_tree() {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::JobObjects::{
        IsProcessInJob, OpenJobObjectW, JOB_OBJECT_QUERY,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    struct Handles(Vec<HANDLE>);
    impl Drop for Handles {
        fn drop(&mut self) {
            for handle in self.0.drain(..) {
                unsafe {
                    CloseHandle(handle);
                }
            }
        }
    }
    struct TaskGuard<'a>(&'a BgTaskRegistry, String);
    impl Drop for TaskGuard<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill(&self.1, "memory-waiter");
        }
    }

    for form in ["foreground", "background", "pty"] {
        let temp = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let shell_ready = temp.path().join("powershell-pid");
        let child_ready = temp.path().join("allocator-pid");
        let release = temp.path().join("allocate");
        let quote = |path: &Path| path.display().to_string().replace('\'', "''");
        let command = format!(
            "[IO.File]::WriteAllText('{}', [string]$PID); & '{}' --exact {ALLOCATOR} --nocapture",
            quote(&shell_ready),
            quote(&std::env::current_exe().unwrap())
        );
        let mut env = HashMap::from([
            ("AFT_TEST_ALLOCATE_MIB".into(), "512".into()),
            (
                "AFT_TEST_MEMORY_CHILD_READY".into(),
                child_ready.display().to_string(),
            ),
            (
                "AFT_TEST_MEMORY_CHILD_RELEASE".into(),
                release.display().to_string(),
            ),
        ]);
        for key in ["PATH", "SystemRoot", "WINDIR", "USERPROFILE", "TEMP", "TMP"] {
            if let Ok(value) = std::env::var(key) {
                env.insert(key.into(), value);
            }
        }
        let shell = super::super::BashShell::Powershell;
        let timeout = super::super::HardKill::After(Duration::from_secs(45));
        let id = if form == "pty" {
            registry
                .spawn_pty_with_shell(
                    SpawnPlan::Unsandboxed,
                    &command,
                    shell,
                    "pwsh.exe".into(),
                    "memory-waiter".into(),
                    temp.path().into(),
                    env,
                    timeout,
                    Some(256),
                    temp.path().into(),
                    4,
                    true,
                    false,
                    None,
                    24,
                    80,
                )
                .unwrap()
        } else {
            registry
                .spawn_with_shell_in_slot_typed(
                    SpawnPlan::Unsandboxed,
                    &command,
                    shell,
                    "pwsh.exe".into(),
                    "memory-waiter".into(),
                    temp.path().into(),
                    env,
                    timeout,
                    Some(256),
                    temp.path().into(),
                    if form == "foreground" {
                        super::super::TaskSlot::Foreground
                    } else {
                        super::super::TaskSlot::Background { max: 4 }
                    },
                    true,
                    false,
                    None,
                )
                .unwrap()
        };
        let _cleanup = TaskGuard(&registry, id.clone());
        let task = registry.task_for_test(&id).unwrap();
        let root = task.lock_state().unwrap().metadata.child_pid.unwrap();
        let deadline = Instant::now() + Duration::from_secs(25);
        let read_pid = |path: &Path| -> u32 {
            loop {
                if let Ok(text) = std::fs::read_to_string(path) {
                    if let Ok(pid) = text.parse() {
                        return pid;
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "{form}: process did not reach its ready gate: {}",
                    path.display()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let pids = [root, read_pid(&shell_ready), read_pid(&child_ready)];
        let name: Vec<u16> = format!("Local\\AFT-memory-{id}\0").encode_utf16().collect();
        let job = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, 0, name.as_ptr()) };
        assert!(
            !job.is_null(),
            "open memory job: {}",
            std::io::Error::last_os_error()
        );
        let mut handles = Handles(vec![job]);
        for pid in pids {
            const SYNCHRONIZE_RIGHT: u32 = 0x0010_0000;
            let process = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_RIGHT,
                    0,
                    pid,
                )
            };
            assert!(
                !process.is_null(),
                "open PID {pid}: {}",
                std::io::Error::last_os_error()
            );
            handles.0.push(process);
            let mut included = 0;
            assert_ne!(unsafe { IsProcessInJob(process, job, &mut included) }, 0);
            assert_ne!(
                included, 0,
                "{form}: PID {pid} escaped the task's memory job"
            );
        }
        let mut image = [0u16; 1024];
        let mut length = image.len() as u32;
        assert_ne!(
            unsafe { QueryFullProcessImageNameW(handles.0[1], 0, image.as_mut_ptr(), &mut length) },
            0
        );
        assert!(String::from_utf16_lossy(&image[..length as usize])
            .to_ascii_lowercase()
            .ends_with("\\cmd.exe"));
        std::fs::write(&release, b"allocate").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let snapshot = registry
                .status(&id, "memory-waiter", None, None, 4096)
                .unwrap();
            if snapshot.info.status.is_terminal() {
                assert_eq!(snapshot.info.status, BgTaskStatus::Failed, "{snapshot:?}");
                let reason = snapshot.info.status_reason.unwrap_or_default();
                assert!(
                    reason.contains("256 MiB memory cap")
                        && reason.contains("Windows job-object memory limit"),
                    "{reason}"
                );
                assert!(!reason.contains("timeout"), "{reason}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{form}: memory cap did not stop the process tree"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        for process in &handles.0[1..] {
            assert_eq!(
                unsafe { WaitForSingleObject(*process, 5000) },
                WAIT_OBJECT_0,
                "{form}: a capped process survived"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn memory_limit_counts_children_as_one_group() {
    allocate_past_cap(false, false, true, false, None);
}

#[cfg(target_os = "linux")]
#[test]
fn memory_limit_linux_kernel_stops_group() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut probe =
        super::super::memory_limit::Monitor::prepare(512, &format!("probe-{}", random_slug()))
            .unwrap();
    let mut child = Command::new("/bin/sh")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args(["-c", "read -r ready"])
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    probe.attach(child.id()).unwrap();
    let kernel_available = probe.policy.cgroup.is_some();
    drop(child.stdin.take());
    let _ = child.wait();
    drop(probe);
    if !kernel_available {
        report_memory_runtime_skip(
            "memory_limit_linux_kernel_stops_group",
            "create-and-attach probe has no usable delegated cgroup or systemd scope",
        );
        return;
    }
    allocate_past_cap(false, false, true, true, None);
}

pub(super) fn report_memory_runtime_skip(name: &str, reason: &str) {
    use std::io::Write;
    // Bypass libtest's passing-test capture so capability skips remain visible in CI.
    writeln!(std::io::stderr().lock(), "SKIP {name}: {reason}").unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn memory_limit_attach_eperm_foreground_enforces_watcher_cap() {
    allocate_past_cap(false, true, false, false, Some(libc::EPERM));
}

#[cfg(target_os = "linux")]
#[test]
fn memory_limit_attach_eperm_background_enforces_watcher_cap() {
    allocate_past_cap(false, false, false, false, Some(libc::EPERM));
}

#[cfg(target_os = "linux")]
#[test]
fn memory_limit_attach_eperm_pty_enforces_watcher_cap() {
    allocate_past_cap(true, false, false, false, Some(libc::EPERM));
}

#[cfg(target_os = "linux")]
#[test]
fn memory_limit_other_attach_errors_enforce_watcher_cap() {
    for errno in [libc::EACCES, libc::EROFS, libc::ENOENT, libc::EBUSY] {
        allocate_past_cap(false, true, false, false, Some(errno));
    }
}

#[test]
fn memory_limit_report_is_not_a_timeout() {
    let policy = super::super::memory_limit::MemoryPolicy {
        mib: 64,
        kind: "test enforcement".into(),
        cgroup: None,
    };
    assert_eq!(
        policy.exceeded_reason(),
        "Command stopped: exceeded the 64 MiB memory cap (test enforcement)."
    );
}

#[test]
fn memory_limit_quick_piped_success_keeps_exit_zero() {
    let temp = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    registry
        .inner
        .watchdog_started
        .store(true, Ordering::SeqCst);
    let mut env = HashMap::new();
    for key in ["PATH", "SystemRoot", "WINDIR", "USERPROFILE", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    #[cfg(unix)]
    let (shell, shell_path) = (super::super::BashShell::Bash, PathBuf::from("/bin/sh"));
    #[cfg(windows)]
    let (shell, shell_path) = (
        super::super::BashShell::Powershell,
        PathBuf::from("pwsh.exe"),
    );
    #[cfg(unix)]
    let command = "exit 0";
    // An explicit exit in Invoke-Expression ends the wrapper before it records
    // the exit code. Use a normal zero-return command to isolate job accounting.
    #[cfg(windows)]
    let command = "[Console]::Write('ok')";
    let id = registry
        .spawn_with_shell(
            SpawnPlan::Unsandboxed,
            command,
            shell,
            shell_path,
            "quick-success".into(),
            temp.path().into(),
            env,
            super::super::HardKill::After(Duration::from_secs(5)),
            Some(512),
            temp.path().into(),
            4,
            true,
            false,
            None,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let snapshot = registry
            .status(&id, "quick-success", None, None, 4096)
            .unwrap();
        if snapshot.info.status.is_terminal() {
            assert_eq!(
                snapshot.info.status,
                BgTaskStatus::Completed,
                "{snapshot:?}"
            );
            assert_eq!(snapshot.exit_code, Some(0), "{snapshot:?}");
            assert!(snapshot.info.status_reason.is_none(), "{snapshot:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "successful piped task did not complete"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn memory_limit_fast_successful_pty_keeps_exit_zero() {
    let temp = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    // Observe the completed child before the first memory scan, without a timer race.
    registry
        .inner
        .watchdog_started
        .store(true, Ordering::SeqCst);
    let id = registry
        .spawn_pty_with_shell(
            SpawnPlan::Unsandboxed,
            "exit 0",
            super::super::BashShell::Bash,
            PathBuf::from("/bin/sh"),
            "quick-success".into(),
            temp.path().into(),
            HashMap::new(),
            super::super::HardKill::After(Duration::from_secs(5)),
            Some(64),
            temp.path().into(),
            4,
            true,
            false,
            None,
            24,
            80,
        )
        .unwrap();
    let task = registry.task_for_test(&id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let exited = task.lock_state().is_ok_and(|state| {
            matches!(&state.runtime,
            TaskRuntime::Pty(Some(pty)) if pty.exit_observed.load(Ordering::SeqCst))
        });
        if exited && task.terminal_reader_done.load(Ordering::SeqCst) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "successful PTY child did not exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Model a reaped PTY whose process identity was cleared before completion is consumed.
    {
        let mut state = task.lock_state().unwrap();
        state.metadata.child_pid = None;
        state.metadata.pgid = None;
    }
    loop {
        let snapshot = registry
            .status(&id, "quick-success", None, None, 4096)
            .unwrap();
        if snapshot.info.status.is_terminal() {
            assert_eq!(
                snapshot.info.status,
                BgTaskStatus::Completed,
                "{snapshot:?}"
            );
            assert_eq!(snapshot.exit_code, Some(0), "{snapshot:?}");
            assert!(snapshot.info.status_reason.is_none(), "{snapshot:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "successful PTY task did not complete"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn memory_limit_terminal_accounting_failure_still_samples_and_notifies() {
    let temp = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (id, task) = tests::insert_terminal_piped_task(&registry, &temp, "allocate", "", "", false);
    let policy = super::super::memory_limit::MemoryPolicy {
        mib: 64,
        kind: "test enforcement".into(),
        cgroup: None,
    };
    {
        let mut state = task.lock_state().unwrap();
        state.metadata.memory_policy = Some(policy.clone());
        state.metadata.completion_delivered = false;
    }
    let mut monitor = super::super::memory_limit::Monitor::recover(policy.clone(), &id).unwrap();
    monitor.force_exceeded_for_test();
    registry
        .inner
        .memory_monitors
        .lock()
        .unwrap()
        .insert(id, monitor);
    registry.inner.completions.lock().unwrap().clear();
    *task.terminal_notification_routed.lock().unwrap() = false;

    // An unwritable metadata destination must not suppress an observed terminal result.
    let original_metadata = std::fs::read(&task.paths.json).unwrap();
    std::fs::remove_file(&task.paths.json).unwrap();
    std::fs::create_dir(&task.paths.json).unwrap();
    let result = registry.post_terminal_transition(&task, false);
    assert!(
        result.is_ok(),
        "memory settlement blocked publication: {result:?}"
    );
    assert!(task.terminal_persist_dirty.load(Ordering::SeqCst));
    assert!(task.lock_state().unwrap().descendant_sampling_started);
    let completions = registry.pending_completions_for_session("session");
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].status, BgTaskStatus::Failed);
    assert_eq!(
        completions[0].status_reason.as_deref(),
        Some(policy.exceeded_reason().as_str())
    );

    std::fs::remove_dir(&task.paths.json).unwrap();
    std::fs::write(&task.paths.json, original_metadata).unwrap();
    registry.retry_terminal_persist(&task).unwrap();
    assert!(!task.terminal_persist_dirty.load(Ordering::SeqCst));
    let persisted = super::super::persistence::read_task(&task.paths.json).unwrap();
    assert_eq!(persisted.status, BgTaskStatus::Failed);
    assert_eq!(persisted.status_reason, Some(policy.exceeded_reason()));
}
