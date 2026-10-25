//! Thread CPU samples, never process CPU or elapsed wall time.

use std::thread::ThreadId;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ThreadCpuSample {
    thread: ThreadId,
    cpu: Duration,
}

impl ThreadCpuSample {
    pub(crate) fn now() -> Option<Self> {
        Some(Self {
            thread: std::thread::current().id(),
            cpu: thread_cpu_time()?,
        })
    }

    pub(crate) fn elapsed(start: Option<Self>, end: Option<Self>) -> Option<Duration> {
        let (start, end) = (start?, end?);
        if start.thread != end.thread {
            return None;
        }
        end.cpu.checked_sub(start.cpu)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn thread_cpu_time() -> Option<Duration> {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // The OS initializes the timespec on success; no clock value is used on failure.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) } != 0 {
        return None;
    }
    let time = unsafe { time.assume_init() };
    Some(Duration::new(
        time.tv_sec.try_into().ok()?,
        time.tv_nsec.try_into().ok()?,
    ))
}

#[cfg(windows)]
fn thread_cpu_time() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // The pseudo-handle names the calling thread and does not need closing.
    if unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return None;
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let ticks = ticks(kernel).checked_add(ticks(user))?;
    Some(Duration::from_secs(ticks / 10_000_000) + Duration::from_nanos((ticks % 10_000_000) * 100))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn thread_cpu_time() -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;
    use std::time::Instant;

    #[test]
    fn cpu_phase_clock_failure_is_unknown() {
        assert_eq!(ThreadCpuSample::elapsed(None, ThreadCpuSample::now()), None);
        assert_eq!(ThreadCpuSample::elapsed(ThreadCpuSample::now(), None), None);
    }

    #[test]
    #[ignore = "clock-read microbenchmark, run explicitly with --ignored --nocapture"]
    fn cpu_phase_clock_overhead() {
        const N: u32 = 100_000;
        let start = Instant::now();
        for _ in 0..N {
            black_box(Instant::now());
        }
        let wall_only = start.elapsed();
        let start = Instant::now();
        for _ in 0..N {
            black_box(Instant::now());
            black_box(ThreadCpuSample::now());
        }
        let paired = start.elapsed();
        println!(
            "{N} boundaries: wall-only={:.1} ns, wall+CPU={:.1} ns, added={:.1} ns/boundary",
            wall_only.as_nanos() as f64 / f64::from(N),
            paired.as_nanos() as f64 / f64::from(N),
            paired.saturating_sub(wall_only).as_nanos() as f64 / f64::from(N)
        );
    }
}
