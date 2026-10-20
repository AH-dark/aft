//! Native gh is an operator-only exit from the shim, never a worker fallback.

use super::*;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ShellSignals {
    pub(super) sandboxed: bool,
    pub(super) worker_marked: bool,
    pub(super) ticket_present: bool,
}

impl ShellSignals {
    pub(super) fn from_process() -> Self {
        Self {
            sandboxed: native_sandbox_active(),
            worker_marked: std::env::var_os(crate::gh_shim_ticket::GH_SHIM_WORKER_ENV).is_some(),
            ticket_present: std::env::var_os(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV).is_some(),
        }
    }
}

pub(super) fn native_sandbox_active() -> bool {
    #[cfg(target_os = "linux")]
    {
        // no_new_privs is inherited across exec and cannot be unset. Clearing
        // environment variables therefore cannot turn a jailed shell into a head.
        unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 0 }
    }
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn sandbox_check(
                pid: libc::pid_t,
                operation: *const libc::c_char,
                filter: libc::c_int,
                ...
            ) -> libc::c_int;
        }
        // A null operation asks only whether this process has a Seatbelt policy.
        // It neither probes credential files nor changes the policy.
        unsafe { sandbox_check(libc::getpid(), std::ptr::null(), 0) != 0 }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

pub(super) fn permission_with(
    relay: &RelayContext,
    signals: ShellSignals,
    lookup: impl FnOnce(&RelayContext) -> Result<Option<bool>, String>,
) -> Result<(), String> {
    if relay
        .ticket
        .as_deref()
        .is_some_and(|ticket| !ticket.is_empty())
    {
        return match lookup(relay) {
            Ok(Some(false)) if !signals.worker_marked => Ok(()),
            Ok(Some(true)) => Err("operator gh is not available to worker sessions; use governed reads or bot speech".into()),
            _ => Err("the gh-shim ticket did not authenticate a head session; operator gh is not available here".into()),
        };
    }
    if signals.sandboxed {
        return Err(
            "no gh-shim ticket inside a sandboxed agent shell; operator gh is not available here"
                .into(),
        );
    }
    if signals.worker_marked || signals.ticket_present {
        return Err(
            "no valid gh-shim ticket for this agent shell; operator gh is not available here"
                .into(),
        );
    }
    Ok(())
}

pub(super) fn permission_from_process() -> Result<(), String> {
    permission_with(
        &RelayContext::from_process(),
        ShellSignals::from_process(),
        relay_client::worker_session,
    )
}

pub(super) fn dispatch_with(
    args: &[OsString],
    relay: &RelayContext,
    signals: ShellSignals,
    lookup: impl FnOnce(&RelayContext) -> Result<Option<bool>, String>,
    native: impl FnOnce(&[OsString]) -> i32,
) -> i32 {
    match permission_with(relay, signals, lookup) {
        Ok(()) => native(args),
        Err(text) => refuse(RefusalCode::WorkerNativeGh, &text),
    }
}

pub(super) fn dispatch(
    args: &[OsString],
    relay: &RelayContext,
    signals: ShellSignals,
    native: impl FnOnce(&[OsString]) -> i32,
) -> i32 {
    dispatch_with(args, relay, signals, relay_client::worker_session, native)
}
