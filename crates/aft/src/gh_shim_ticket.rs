//! Per-command tickets that let a `gh` shim child speak through the daemon.
//!
//! An agent's children run without any subc credentials, so the `gh` shim
//! cannot prove on its own which agent session launched it. The daemon issues
//! every spawned command an unguessable ticket, hands it to the child as
//! [`GH_SHIM_TICKET_ENV`], and remembers which session and task it belongs to.
//! When the shim asks the daemon to relay a bot write, the ticket is the only
//! thing that selects the session; a session id typed on a command line or
//! placed in the environment carries no authority.
//!
//! Tickets live in this process's memory and nowhere else. They are never
//! written to task JSON, aft.db or the log, they die when their task reaches a
//! terminal state, and a daemon restart forgets all of them, so a task replayed
//! after a restart has no ticket and its shim calls are refused.

use std::sync::{LazyLock, Mutex};

/// Environment variable that carries the ticket into a spawned command.
pub const GH_SHIM_TICKET_ENV: &str = "AFT_GH_SHIM_TICKET";
/// Restriction hint added only by worker launch planning. It cannot grant identity.
pub const GH_SHIM_WORKER_ENV: &str = "AFT_GH_SHIM_WORKER";

const TICKET_BYTES: usize = 16;

/// The session and command a redeemed ticket was issued for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redeemed {
    pub session_id: String,
    /// Background task id, or the id of the tool call that ran `gh` directly.
    /// Empty while a background task is still being spawned.
    pub task_id: String,
    /// Project root recorded when the daemon launched the command. It is used
    /// to capture read_repository at mint and to label outbound routes, not to
    /// re-derive a repository from editable config during a later read.
    pub project_root: String,
    /// Repository captured from the primary checkout when this ticket was
    /// minted. Later worktree config edits cannot retarget an existing ticket.
    pub read_repository: Option<String>,
}

enum Owner {
    Task(String),
    Call(String),
}

impl Owner {
    fn id(&self) -> &str {
        match self {
            Self::Task(id) | Self::Call(id) => id,
        }
    }
}

struct Entry {
    ticket: [u8; TICKET_BYTES],
    session_id: String,
    owner: Owner,
    project_root: String,
    read_repository: Option<String>,
    #[cfg(unix)]
    worker_read: Option<WorkerReadGrant>,
}

/// The worker-read capability is separate from the general relay ticket. Dropping
/// the registry entry tears down its endpoint and every outstanding exchange.
#[cfg(unix)]
struct WorkerReadGrant {
    capability: [u8; TICKET_BYTES],
    cancel: tokio::sync::watch::Sender<bool>,
    socket: std::path::PathBuf,
    deadline: std::time::Instant,
    binding: serde_json::Value,
}

#[cfg(unix)]
impl Drop for WorkerReadGrant {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        let _ = std::fs::remove_file(&self.socket);
    }
}

static REGISTRY: LazyLock<Mutex<Vec<Entry>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn registry() -> std::sync::MutexGuard<'static, Vec<Entry>> {
    REGISTRY.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Whether a session may hold a ticket. Requests without an agent session use
/// the shared default namespace, which is not an agent and cannot speak.
fn session_can_hold_ticket(session_id: &str) -> bool {
    !session_id.trim().is_empty() && session_id != crate::protocol::DEFAULT_SESSION_ID
}

fn encode(ticket: &[u8; TICKET_BYTES]) -> String {
    ticket.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode(text: &str) -> Option<[u8; TICKET_BYTES]> {
    let text = text.trim();
    if text.len() != TICKET_BYTES * 2 || !text.is_ascii() {
        return None;
    }
    let mut ticket = [0_u8; TICKET_BYTES];
    for (index, slot) in ticket.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(ticket)
}

/// Compare two tickets without an early exit, so the time taken does not
/// reveal how many leading bytes of a guess were right.
fn constant_time_eq(left: &[u8; TICKET_BYTES], right: &[u8; TICKET_BYTES]) -> bool {
    let mut difference = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

fn insert(session_id: &str, owner: Owner, project_root: &str) -> Option<String> {
    if !session_can_hold_ticket(session_id) {
        return None;
    }
    let mut ticket = [0_u8; TICKET_BYTES];
    // An operating-system random source failure leaves the command without a
    // ticket, which makes its governed writes refuse rather than guessable.
    getrandom::fill(&mut ticket).ok()?;
    let encoded = encode(&ticket);
    let read_repository =
        crate::gh_shim::ticket_repository_snapshot(std::path::Path::new(project_root));
    registry().push(Entry {
        ticket,
        session_id: session_id.to_string(),
        owner,
        project_root: project_root.to_string(),
        read_repository,
        #[cfg(unix)]
        worker_read: None,
    });
    Some(encoded)
}

/// Look a ticket up. Every registered ticket is compared in full, so the
/// lookup time depends on the number of live tickets, not on the guess.
pub fn redeem(ticket: &str) -> Option<Redeemed> {
    let candidate = decode(ticket)?;
    let entries = registry();
    let mut found = None;
    for entry in entries.iter() {
        if constant_time_eq(&entry.ticket, &candidate) {
            found = Some(Redeemed {
                session_id: entry.session_id.clone(),
                task_id: entry.owner.id().to_string(),
                project_root: entry.project_root.clone(),
                read_repository: entry.read_repository.clone(),
            });
        }
    }
    found
}

/// Forget every ticket issued for a task. Called whenever the task's metadata
/// is written in a terminal state and when its bundle is deleted.
pub fn revoke_task(task_id: &str) {
    if task_id.is_empty() {
        return;
    }
    registry().retain(|entry| !matches!(&entry.owner, Owner::Task(id) if id == task_id));
}

fn revoke(ticket: &str) {
    let Some(candidate) = decode(ticket) else {
        return;
    };
    registry().retain(|entry| !constant_time_eq(&entry.ticket, &candidate));
}

/// Number of live tickets; lets tests prove a ticket was dropped.
#[cfg(all(test, unix))]
pub(crate) fn live_count_for_task(task_id: &str) -> usize {
    registry()
        .iter()
        .filter(|entry| matches!(&entry.owner, Owner::Task(id) if id == task_id))
        .count()
}

/// A ticket for a background task that has not been spawned yet, so its task
/// id is still unknown. Dropping it without [`PendingTicket::bind_task`]
/// revokes it, which covers every early return of a failed spawn.
pub struct PendingTicket {
    ticket: Option<String>,
}

impl PendingTicket {
    /// Issue a ticket for `session_id`, or none for a session that is absent
    /// or the shared default namespace.
    pub fn issue(session_id: &str, project_root: &str) -> Self {
        Self {
            ticket: insert(session_id, Owner::Task(String::new()), project_root),
        }
    }

    /// A caller-selected command workdir is not a repository binding. Missing
    /// trusted project context leaves reads unbound rather than inferring it.
    pub(crate) fn issue_for_project(
        session_id: &str,
        project_root: Option<&std::path::Path>,
    ) -> Self {
        Self::issue(
            session_id,
            &project_root
                .map(|root| root.display().to_string())
                .unwrap_or_default(),
        )
    }

    pub fn value(&self) -> Option<&str> {
        self.ticket.as_deref()
    }

    /// Reserve the task identity before exec, while retaining drop-on-error
    /// revocation until the spawning code commits the ticket with bind_task.
    #[cfg(unix)]
    pub(crate) fn reserve_task(&self, task_id: &str) {
        if let Some(candidate) = self.ticket.as_deref().and_then(decode) {
            for entry in registry().iter_mut() {
                if constant_time_eq(&entry.ticket, &candidate) {
                    entry.owner = Owner::Task(task_id.to_string());
                }
            }
        }
    }

    /// Called only after resolving a native worker-jail plan with the shim
    /// enabled. Host synchronous calls have no entry point to mint this grant.
    #[cfg(unix)]
    pub(crate) fn worker_read_capability(
        &self,
        socket: std::path::PathBuf,
        deadline: std::time::Instant,
        binding: &crate::gh_shim_relay::ScopeStamp,
    ) -> Option<(String, tokio::sync::watch::Sender<bool>)> {
        if !crate::gh_shim_relay::worker_scope(binding) {
            return None;
        }
        let candidate = decode(self.ticket.as_deref()?)?;
        let mut entries = registry();
        let entry = entries
            .iter_mut()
            .find(|entry| constant_time_eq(&entry.ticket, &candidate))?;
        if !matches!(&entry.owner, Owner::Task(id) if !id.is_empty()) || entry.worker_read.is_some()
        {
            return None;
        }
        let mut capability = [0; TICKET_BYTES];
        getrandom::fill(&mut capability).ok()?;
        let (cancel, _) = tokio::sync::watch::channel(false);
        entry.worker_read = Some(WorkerReadGrant {
            capability,
            cancel: cancel.clone(),
            socket,
            deadline,
            binding: serde_json::to_value(binding).ok()?,
        });
        Some((encode(&capability), cancel))
    }

    /// Attach the spawned task's id so the task's terminal transition revokes
    /// the ticket. The caller must then check whether the task already ended
    /// (see `revoke_task`), because a very short command can reach its
    /// terminal state before this runs.
    pub fn bind_task(mut self, task_id: &str) {
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        let Some(candidate) = decode(&ticket) else {
            return;
        };
        for entry in registry().iter_mut() {
            if constant_time_eq(&entry.ticket, &candidate) {
                entry.owner = Owner::Task(task_id.to_string());
            }
        }
    }
}

/// Redeem only at the endpoint that minted this purpose-specific capability.
/// A general ticket, another command's endpoint, and expired grants all fail.
#[cfg(unix)]
pub(crate) fn redeem_worker_read(capability: &str, socket: &std::path::Path) -> Option<Redeemed> {
    let candidate = decode(capability)?;
    let entries = registry();
    entries.iter().find_map(|entry| {
        let grant = entry.worker_read.as_ref()?;
        if !constant_time_eq(&grant.capability, &candidate)
            || grant.socket != socket
            || *grant.cancel.borrow()
            || std::time::Instant::now() >= grant.deadline
        {
            return None;
        }
        Some(Redeemed {
            session_id: entry.session_id.clone(),
            task_id: entry.owner.id().to_string(),
            project_root: entry.project_root.clone(),
            read_repository: entry.read_repository.clone(),
        })
    })
}

#[cfg(unix)]
pub(crate) fn worker_read_scope_matches(
    capability: &str,
    socket: &std::path::Path,
    stamp: &crate::gh_shim_relay::ScopeStamp,
) -> bool {
    let Some(candidate) = decode(capability) else {
        return false;
    };
    let Ok(binding) = serde_json::to_value(stamp) else {
        return false;
    };
    registry().iter().any(|entry| {
        entry.worker_read.as_ref().is_some_and(|grant| {
            constant_time_eq(&grant.capability, &candidate)
                && grant.socket == socket
                && grant.binding == binding
        })
    })
}

#[cfg(unix)]
pub(crate) fn renew_worker_read_deadline(task_id: &str, deadline: std::time::Instant) {
    for entry in registry().iter_mut() {
        if matches!(&entry.owner, Owner::Task(id) if id == task_id) {
            if let Some(grant) = &mut entry.worker_read {
                grant.deadline = grant.deadline.max(deadline);
            }
        }
    }
}

#[cfg(unix)]
pub(crate) fn worker_read_deadline(socket: &std::path::Path) -> Option<std::time::Instant> {
    registry()
        .iter()
        .filter_map(|entry| entry.worker_read.as_ref())
        .find(|grant| grant.socket == socket)
        .map(|grant| grant.deadline)
}

impl Drop for PendingTicket {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            revoke(&ticket);
        }
    }
}

/// A ticket that lives exactly as long as one synchronous `gh` run made by
/// the daemon itself, such as a comment written through the read/edit tools.
pub struct ScopedTicket {
    ticket: Option<String>,
}

impl ScopedTicket {
    pub fn issue(session_id: &str, call_id: &str, project_root: &str) -> Self {
        Self {
            ticket: insert(session_id, Owner::Call(call_id.to_string()), project_root),
        }
    }

    pub fn value(&self) -> Option<&str> {
        self.ticket.as_deref()
    }
}

impl Drop for ScopedTicket {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            revoke(&ticket);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_are_128_bit_hex_and_unique() {
        let first = ScopedTicket::issue("ses-ticket-unique", "call-a", "/p");
        let second = ScopedTicket::issue("ses-ticket-unique", "call-b", "/p");
        let first_value = first.value().unwrap();
        assert_eq!(first_value.len(), 32);
        assert!(first_value.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first_value, second.value().unwrap());
    }

    #[test]
    fn default_and_empty_sessions_get_no_ticket() {
        assert!(
            PendingTicket::issue(crate::protocol::DEFAULT_SESSION_ID, "/p")
                .value()
                .is_none()
        );
        assert!(PendingTicket::issue("", "/p").value().is_none());
        assert!(ScopedTicket::issue("  ", "call", "/p").value().is_none());
    }

    #[test]
    fn redeem_returns_the_issuing_session_and_task() {
        let pending = PendingTicket::issue("ses-redeem", "/p");
        let value = pending.value().unwrap().to_string();
        assert_eq!(
            redeem(&value),
            Some(Redeemed {
                session_id: "ses-redeem".to_string(),
                task_id: String::new(),
                project_root: "/p".to_string(),
                read_repository: None,
            })
        );
        pending.bind_task("task-redeem");
        assert_eq!(redeem(&value).unwrap().task_id, "task-redeem");
        revoke_task("task-redeem");
        assert_eq!(redeem(&value), None);
    }

    #[test]
    fn fabricated_and_malformed_tickets_do_not_redeem() {
        let _live = ScopedTicket::issue("ses-fabricated", "call", "/p");
        assert_eq!(redeem("00000000000000000000000000000000"), None);
        assert_eq!(redeem("ses-fabricated"), None);
        assert_eq!(redeem(""), None);
        assert_eq!(redeem("zz000000000000000000000000000000"), None);
    }

    #[test]
    fn dropping_an_unbound_pending_ticket_revokes_it() {
        let pending = PendingTicket::issue("ses-drop", "/p");
        let value = pending.value().unwrap().to_string();
        drop(pending);
        assert_eq!(redeem(&value), None);
    }

    #[test]
    fn scoped_ticket_dies_with_its_scope() {
        let value = {
            let scoped = ScopedTicket::issue("ses-scoped", "call-scoped", "/p");
            let value = scoped.value().unwrap().to_string();
            assert!(redeem(&value).is_some());
            value
        };
        assert_eq!(redeem(&value), None);
    }

    #[test]
    fn revoking_a_background_task_does_not_revoke_a_same_named_scoped_call() {
        let scoped = ScopedTicket::issue("ses-collision", "task-call-collision", "/p");
        let scoped_value = scoped.value().unwrap().to_string();
        let pending = PendingTicket::issue("ses-collision", "/p");
        let task_value = pending.value().unwrap().to_string();
        pending.bind_task("task-call-collision");

        revoke_task("task-call-collision");

        assert_eq!(redeem(&task_value), None);
        assert_eq!(
            redeem(&scoped_value).unwrap().task_id,
            "task-call-collision"
        );
        drop(scoped);
        assert_eq!(redeem(&scoped_value), None);
    }
}
