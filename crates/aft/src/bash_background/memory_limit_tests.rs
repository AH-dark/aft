use super::*;

const ALLOCATOR: &str =
    "bash_background::registry::memory_limit_tests::memory_limit_allocator_child";

#[test]
fn memory_limit_allocator_child() {
    let Some(mib) = std::env::var("AFT_TEST_ALLOCATE_MIB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return;
    };
    let allocation = vec![0x5a_u8; mib * 1024 * 1024];
    std::hint::black_box(&allocation);
    std::thread::park_timeout(Duration::from_secs(12));
    std::hint::black_box(&allocation);
}

fn allocate_past_cap(pty: bool, foreground: bool, aggregate: bool, require_kernel: bool) {
    let temp = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    registry.set_memory_limit(Some(64));
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
            panic!("no delegated cgroup v2 memory controller or systemd user scope available");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let snapshot = registry
            .status(&id, "memory-test", None, None, 4096)
            .unwrap();
        if snapshot.info.status.is_terminal() {
            assert_eq!(snapshot.info.status, BgTaskStatus::Failed, "{snapshot:?}");
            let reason = snapshot.info.status_reason.unwrap_or_default();
            assert!(reason.contains("64 MiB memory cap"), "{reason}");
            assert!(!reason.contains("timeout"), "{reason}");
            let task = registry.task_for_test(&id).unwrap();
            let state = task.lock_state().unwrap();
            let policy = state.metadata.memory_policy.as_ref().unwrap();
            assert!(reason.contains(&policy.kind), "{reason}");
            println!("enforcement: {}; result: {reason}", policy.kind);
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
    allocate_past_cap(false, false, false, false);
}

#[test]
fn memory_limit_stops_foreground_command() {
    allocate_past_cap(false, true, false, false);
}

#[test]
fn memory_limit_stops_pty_command() {
    allocate_past_cap(true, false, false, false);
}

#[cfg(unix)]
#[test]
fn memory_limit_counts_children_as_one_group() {
    allocate_past_cap(false, false, true, false);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a delegated cgroup v2 memory controller or a systemd user manager"]
fn memory_limit_linux_kernel_stops_group() {
    allocate_past_cap(false, false, true, true);
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
