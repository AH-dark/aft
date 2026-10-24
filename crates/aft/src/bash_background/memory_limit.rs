//! Per-command aggregate memory accounting. Launchers keep shell commands
//! behind a gate until their memory-enforcement setup is complete.
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Matches the existing watchdog tick: two samples per second, only for capped tasks.
pub(super) const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
#[cfg(unix)]
const PID_BUDGET: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPolicy {
    pub mib: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<std::path::PathBuf>,
}

impl MemoryPolicy {
    pub fn note(&self) -> String {
        format!("Memory cap: {} MiB ({}).", self.mib, self.kind)
    }

    pub fn exceeded_reason(&self) -> String {
        format!(
            "Command stopped: exceeded the {} MiB memory cap ({}).",
            self.mib, self.kind
        )
    }

    fn accounting_failure_reason(&self) -> String {
        format!(
            "Command stopped: memory-cap accounting could not complete its bounded process scan ({}; cap {} MiB).",
            self.kind, self.mib
        )
    }
}

pub(crate) struct Monitor {
    pub policy: MemoryPolicy,
    last_sample: Option<Instant>,
    exceeded: bool,
    accounting_failed: bool,
    #[cfg(target_os = "linux")]
    bootstrap: Option<std::process::Child>,
    #[cfg(windows)]
    job: windows::Job,
}

impl Monitor {
    pub fn prepare(mib: u64, task_id: &str) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        let (cgroup, bootstrap) = linux::create(mib, task_id);
        #[cfg(not(target_os = "linux"))]
        let cgroup = None;
        #[cfg(windows)]
        let job = windows::Job::new(mib, task_id)?;
        let kind = if cgroup.is_some() {
            "Linux cgroup v2 memory.max"
        } else if cfg!(target_os = "linux") {
            "Linux process-group RSS watcher; kernel memory controller unavailable"
        } else if cfg!(target_os = "macos") {
            "macOS process-group physical-footprint watcher"
        } else {
            "Windows job-object memory limit"
        };
        let _ = task_id;
        Ok(Self {
            policy: MemoryPolicy {
                mib,
                kind: kind.into(),
                cgroup,
            },
            last_sample: None,
            exceeded: false,
            accounting_failed: false,
            #[cfg(target_os = "linux")]
            bootstrap,
            #[cfg(windows)]
            job,
        })
    }

    pub fn attach(&mut self, pid: u32) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        if let Some(path) = &self.policy.cgroup {
            std::fs::write(path.join("cgroup.procs"), pid.to_string())
                .map_err(|error| format!("memory cap attachment failed: {error}"))?;
        }
        #[cfg(target_os = "linux")]
        if let Some(mut helper) = self.bootstrap.take() {
            drop(helper.stdin.take());
            let _ = helper.wait();
        }
        #[cfg(windows)]
        self.job.attach(pid)?;
        let _ = pid;
        Ok(())
    }

    pub fn recover(policy: MemoryPolicy, task_id: &str) -> Result<Self, String> {
        #[cfg(windows)]
        let job = windows::Job::new(policy.mib, task_id)?;
        let _ = task_id;
        Ok(Self {
            policy,
            last_sample: None,
            exceeded: false,
            accounting_failed: false,
            #[cfg(target_os = "linux")]
            bootstrap: None,
            #[cfg(windows)]
            job,
        })
    }

    pub fn exceeded(&mut self, pgid: i32) -> bool {
        if self.exceeded {
            return true;
        }
        if self.policy.cgroup.is_none()
            && !cfg!(windows)
            && self
                .last_sample
                .is_some_and(|last| last.elapsed() < SAMPLE_INTERVAL)
        {
            return false;
        }
        self.last_sample = Some(Instant::now());
        #[cfg(target_os = "linux")]
        let over = match &self.policy.cgroup {
            Some(path) => linux::oom(path),
            None => match group_bytes(pgid) {
                Some(bytes) => bytes > self.policy.mib * 1024 * 1024,
                None => {
                    self.accounting_failed = true;
                    true
                }
            },
        };
        #[cfg(target_os = "macos")]
        let over = match group_bytes(pgid) {
            Some(bytes) => bytes > self.policy.mib * 1024 * 1024,
            None => {
                self.accounting_failed = true;
                true
            }
        };
        #[cfg(windows)]
        let over = self.job.exceeded();
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let over = false;
        let _ = pgid;
        self.exceeded = over;
        over
    }

    pub fn terminate(&self) {
        #[cfg(target_os = "linux")]
        if let Some(path) = &self.policy.cgroup {
            let _ = std::fs::write(path.join("cgroup.kill"), "1");
        }
        #[cfg(windows)]
        self.job.terminate();
    }

    pub fn reason(&self) -> String {
        if self.accounting_failed {
            self.policy.accounting_failure_reason()
        } else {
            self.policy.exceeded_reason()
        }
    }

    #[cfg(windows)]
    pub fn outcome_note(&self) -> Option<&'static str> {
        if self.exceeded {
            None
        } else {
            self.job.outcome_note()
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(mut helper) = self.bootstrap.take() {
            let _ = helper.kill();
            let _ = helper.wait();
        }
        #[cfg(target_os = "linux")]
        if let Some(path) = &self.policy.cgroup {
            let _ = std::fs::remove_dir(path);
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};

    fn current(pid: u32) -> Option<PathBuf> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        let relative = text.lines().find_map(|line| line.strip_prefix("0::"))?;
        // Paths containing '..' are outside this process's cgroup namespace.
        if Path::new(relative)
            .components()
            .any(|part| part == std::path::Component::ParentDir)
        {
            return None;
        }
        Some(Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')))
    }

    fn delegated(parent: &Path, mib: u64, task_id: &str) -> Option<PathBuf> {
        let path = parent.join(format!("aft-memory-{task_id}"));
        std::fs::create_dir(&path).ok()?;
        let result = (|| {
            std::fs::write(path.join("memory.max"), (mib * 1024 * 1024).to_string()).ok()?;
            std::fs::write(path.join("memory.oom.group"), "1").ok()?;
            // Swap must not turn the physical-memory ceiling into an unbounded allocation budget.
            std::fs::write(path.join("memory.swap.max"), "0").ok()?;
            Some(path.clone())
        })();
        if result.is_none() {
            let _ = std::fs::remove_dir(&path);
        }
        result
    }

    pub(super) fn create(
        mib: u64,
        task_id: &str,
    ) -> (Option<PathBuf>, Option<std::process::Child>) {
        let Some(parent) = current(std::process::id()) else {
            return (None, None);
        };
        if let Some(path) = delegated(&parent, mib, task_id) {
            return (Some(path), None);
        }
        // Ask the user manager for delegation when the current cgroup is not writable.
        // The bootstrap waits on stdin, so no uncontained payload can start.
        use std::process::{Command, Stdio};
        let Ok(mut helper) = Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--property=Delegate=yes",
            ])
            .arg(format!("--property=MemoryMax={}", mib * 1024 * 1024))
            .args(["/bin/sh", "-c", "read -r ready"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return (None, None);
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut result = None;
        while std::time::Instant::now() < deadline {
            let child_pid =
                std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", helper.id()))
                    .ok()
                    .and_then(|children| children.split_whitespace().next()?.parse::<u32>().ok());
            let scope = current(helper.id())
                .filter(|scope| scope != &parent)
                .or_else(|| child_pid.and_then(current).filter(|scope| scope != &parent));
            if let Some(scope) = scope {
                if std::fs::write(scope.join("memory.oom.group"), "1").is_ok()
                    && std::fs::write(scope.join("memory.swap.max"), "0").is_ok()
                {
                    result = Some(scope);
                }
                break;
            }
            if helper.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Keep the delegated scope alive until the real task has joined its child cgroup.
        // A populated descendant keeps the scope's cgroup from being collected.
        if result.is_some() {
            return (result, Some(helper));
        }
        let _ = helper.kill();
        let _ = helper.wait();
        (None, None)
    }

    pub(super) fn oom(path: &Path) -> bool {
        std::fs::read_to_string(path.join("memory.events")).is_ok_and(|text| {
            text.lines().any(|line| {
                line.split_once(' ').is_some_and(|(key, value)| {
                    matches!(key, "oom" | "oom_kill" | "oom_group_kill")
                        && value.parse::<u64>().unwrap_or(0) > 0
                })
            })
        })
    }
}

#[cfg(target_os = "linux")]
fn group_bytes(pgid: i32) -> Option<u64> {
    let mut sum = 0_u64;
    // Bound all work, not just matching members. Reaching the budget fails closed.
    for (index, entry) in std::fs::read_dir("/proc").ok()?.enumerate() {
        if index >= PID_BUDGET {
            return None;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<_> = rest.split_whitespace().collect();
        if fields.get(2).and_then(|value| value.parse::<i32>().ok()) != Some(pgid) {
            continue;
        }
        let rss = fields
            .get(21)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        sum = sum.saturating_add(
            rss.saturating_mul(unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64),
        );
    }
    Some(sum)
}

#[cfg(target_os = "macos")]
fn group_pids(pgid: i32) -> Option<Vec<u32>> {
    use std::ffi::{c_int, c_void};
    #[link(name = "proc")]
    extern "C" {
        fn proc_listpgrppids(group: u32, buffer: *mut c_void, size: c_int) -> c_int;
    }
    let estimate = unsafe { proc_listpgrppids(pgid as u32, std::ptr::null_mut(), 0) };
    if estimate <= 0 {
        return None;
    }
    let mut capacity = (estimate as usize).clamp(16, PID_BUDGET);
    for _ in 0..3 {
        let mut pids = vec![0_u32; capacity];
        let count = unsafe {
            proc_listpgrppids(
                pgid as u32,
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(pids.as_slice()) as c_int,
            )
        };
        if count < 0 {
            return None;
        }
        if (count as usize) < capacity {
            pids.resize(count as usize, 0);
            return Some(pids);
        }
        if capacity == PID_BUDGET {
            return None;
        }
        capacity = capacity.saturating_mul(2).min(PID_BUDGET);
    }
    None
}

#[cfg(target_os = "macos")]
fn group_bytes(pgid: i32) -> Option<u64> {
    let pids = group_pids(pgid)?;
    let mut sum = 0_u64;
    for pid in pids {
        if pid == 0 {
            continue;
        }
        let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::proc_pid_rusage(
                pid as i32,
                libc::RUSAGE_INFO_V2,
                (&mut usage as *mut libc::rusage_info_v2).cast(),
            )
        } == 0
        {
            sum = sum.saturating_add(usage.ri_phys_footprint);
        }
    }
    Some(sum)
}

#[cfg(windows)]
#[path = "memory_limit_windows.rs"]
mod windows;

#[cfg(test)]
mod agent_text_tests {
    use super::MemoryPolicy;

    #[test]
    fn memory_limit_note_matches_approved_text() {
        let policy = MemoryPolicy {
            mib: 64,
            kind: "test enforcement".into(),
            cgroup: None,
        };
        assert_eq!(policy.note(), "Memory cap: 64 MiB (test enforcement).");
    }

    #[test]
    fn memory_limit_accounting_failure_matches_approved_text() {
        let policy = MemoryPolicy {
            mib: 64,
            kind: "test enforcement".into(),
            cgroup: None,
        };
        assert_eq!(policy.accounting_failure_reason(), "Command stopped: memory-cap accounting could not complete its bounded process scan (test enforcement; cap 64 MiB).");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    struct Group(Vec<std::process::Child>);
    impl Drop for Group {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[test]
    fn memory_limit_macos_decodes_exact_three_process_group_and_measures_cost() {
        let first = std::process::Command::new("/bin/sleep")
            .arg("20")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = first.id() as i32;
        let mut group = Group(vec![first]);
        for _ in 0..2 {
            group.0.push(
                std::process::Command::new("/bin/sleep")
                    .arg("20")
                    .process_group(pgid)
                    .spawn()
                    .unwrap(),
            );
        }
        let mut expected: Vec<_> = group.0.iter().map(|child| child.id()).collect();
        expected.sort_unstable();
        let mut actual = group_pids(pgid).unwrap();
        actual.sort_unstable();
        assert_eq!(
            actual, expected,
            "libproc returns PID counts, not byte counts"
        );
        let start = Instant::now();
        for _ in 0..1000 {
            assert!(group_bytes(pgid).unwrap() > 0);
        }
        println!(
            "memory watcher: 1000 samples, 3 processes; {:.2} us/sample; interval {} ms",
            start.elapsed().as_secs_f64() * 1000.0,
            SAMPLE_INTERVAL.as_millis()
        );
    }
}
