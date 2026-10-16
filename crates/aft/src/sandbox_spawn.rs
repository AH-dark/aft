//! Policy and process wiring for agent-provided shell commands.
//!
//! Every agent bash process reaches [`resolve_sandbox_spawn`] and carries the
//! resulting [`SpawnPlan`] into one of the two process-creation primitives:
//! detached pipes or PTY. Foreground orchestration uses the detached registry
//! too, so it does not create a third process-creation path.
//!
//! AFT also starts processes for its own implementation. Those are outside this
//! seam because they do not execute an agent command: external formatters,
//! linters, and type checkers in `format`; LSP servers and Windows LSP cleanup in
//! `lsp::client` and `lsp::child_registry`; git probes in `search_index`,
//! `readonly_artifacts`, `commands::configure`, and `commands::conflicts`; login
//! shell PATH discovery in `effective_path`; and Windows process liveness or
//! termination helpers in `artifact_owner`, `fs_lock`, and
//! `bash_background::process`. Image and PDF handling in `commands::read` is
//! in-process and creates no child. Keeping this inventory here makes the
//! agent-command boundary explicit without accidentally applying agent policy to
//! AFT's internal tooling.

use std::cell::RefCell;
#[cfg(any(test, target_os = "linux"))]
use std::collections::BTreeSet;
use std::collections::{BTreeMap, HashMap};
#[cfg(target_os = "linux")]
use std::ffi::{CStr, CString};
use std::ffi::{OsStr, OsString};
use std::fs::File;
#[cfg(unix)]
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(not(unix))]
type RawFd = i32;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
#[cfg(unix)]
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;

use crate::context::AppContext;
use crate::sandbox_profile::SandboxProfile;

#[cfg(unix)]
mod task_temp;

pub const SANDBOX_UNAVAILABLE_EXIT_CODE: i32 = 78;

/// Server-authenticated trust classification for a route bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PrincipalTrust {
    FirstParty,
    Untrusted,
}

/// Principal data supplied by the server-side transport, never by a bash body.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AuthenticatedPrincipal {
    /// Standalone NDJSON and first-party plugin bindings have no route identity.
    FirstParty,
    /// Identity captured from an authenticated subc route bind.
    RouteBind {
        trust: PrincipalTrust,
        route_channel: u16,
        route_epoch: u32,
        project_root: PathBuf,
        harness: String,
        session_id: String,
        /// Server principal label (`direct`, `reserved:<module>`, or
        /// `unverified`). `None` preserves an absent principal for future
        /// fail-closed policy instead of silently treating it as first-party.
        principal_id: Option<String>,
    },
}

/// Sandbox tier requested by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedSandboxTier {
    Disabled,
    Native,
    Host,
}

pub(crate) type ChildEnvironment = BTreeMap<OsString, OsString>;

#[cfg(unix)]
const ESCALATION_GRANT_TTL: Duration = Duration::from_secs(120);
#[cfg(unix)]
const ESCALATION_DIGEST_TAG: &[u8] = b"aft-escalation-payload-v3";
#[cfg(unix)]
const ENVIRONMENT_TAG: &[u8] = b"AFTENV1\0";
#[cfg(unix)]
const ESCALATION_TIER: &[u8] = b"host";

#[cfg(unix)]
#[derive(Debug, Clone)]
struct EscalationGrant {
    principal: AuthenticatedPrincipal,
    root: PathBuf,
    digest: blake3::Hash,
    expires_at: Instant,
    consumed: bool,
    /// The grant was used up by a refused consume attempt, so its payload
    /// bundle was never handed to a task.
    refused: bool,
    /// Set once the grant can no longer hand its payload bundle to a task
    /// (it expired or was refused) and the bundle has been scheduled for
    /// deletion. The metadata stays so a replay still gets its exact refusal.
    payload_reclaimed: bool,
    session_dir: PathBuf,
    task_id: String,
}

#[cfg(unix)]
#[derive(Debug, Default)]
pub(crate) struct EscalationGrantStore {
    grants: HashMap<String, EscalationGrant>,
}

#[cfg(unix)]
impl EscalationGrantStore {
    #[cfg(test)]
    pub(crate) fn len_for_test(&self) -> usize {
        self.grants.len()
    }

    /// Mark every grant whose payload can no longer be claimed (expired, or
    /// refused on a consume attempt) and return those bundles' locations so
    /// the caller can delete them after releasing the store lock. A grant
    /// consumed successfully keeps its bundle: the running task owns it.
    fn take_unclaimable_payloads(&mut self, now: Instant) -> Vec<(PathBuf, String)> {
        let mut payloads = Vec::new();
        for grant in self.grants.values_mut() {
            let expired_unused = !grant.consumed && now >= grant.expires_at;
            if grant.payload_reclaimed || !(grant.refused || expired_unused) {
                continue;
            }
            grant.payload_reclaimed = true;
            payloads.push((grant.session_dir.clone(), grant.task_id.clone()));
        }
        payloads
    }
}

/// Delete escalation payload bundles that no task can claim any more.
#[cfg(unix)]
fn delete_unclaimable_payloads(payloads: Vec<(PathBuf, String)>) {
    for (session_dir, task_id) in payloads {
        if let Ok(task) = crate::bash_background::persistence::resolve_uninitialized_task_layout(
            &session_dir,
            &task_id,
        ) {
            let _ = crate::bash_background::persistence::delete_resolved_task(&task);
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostEscalationAttempt {
    pub grant_id: String,
    pub command: Vec<u8>,
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub shell_path: PathBuf,
    pub environment: ChildEnvironment,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscalationRefusal {
    Expired,
    Consumed,
    DigestMismatch,
    WrongPrincipal,
}

#[cfg(unix)]
impl EscalationRefusal {
    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Consumed => "consumed",
            Self::DigestMismatch => "digest_mismatch",
            Self::WrongPrincipal => "wrong_principal",
        }
    }
}

/// Agent-command path that is about to create a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxTaskKind {
    BashForeground,
    BashBackground,
    BashPty,
}

#[cfg(unix)]
struct PreparedTaskInner {
    paths: crate::bash_background::persistence::TaskPaths,
    dirs: crate::bash_background::persistence::TaskDirs,
    digest: blake3::Hash,
    environment: ChildEnvironment,
    command_bytes: Arc<Vec<u8>>,
    wrapper_bytes: Arc<Vec<u8>>,
    _command_file: Arc<File>,
    _wrapper_file: Arc<File>,
    _environment_file: Arc<File>,
}

/// A materialized payload whose bytes have already been validated through held handles.
#[cfg(unix)]
#[derive(Clone)]
#[doc(hidden)]
pub struct PreparedTask(Arc<PreparedTaskInner>);

#[cfg(unix)]
impl std::fmt::Debug for PreparedTask {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedTask")
            .field("task_id", &self.0.paths.task_id)
            .field("digest", &self.0.digest.to_hex().as_str())
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl PartialEq for PreparedTask {
    fn eq(&self, other: &Self) -> bool {
        self.0.paths.task_id == other.0.paths.task_id
            && self.0.paths.session_dir == other.0.paths.session_dir
            && self.0.digest == other.0.digest
    }
}

#[cfg(unix)]
impl Eq for PreparedTask {}

#[cfg(unix)]
pub(crate) struct PayloadInvocation {
    pub(crate) wrapper_text: OsString,
    pub(crate) command_text: OsString,
}

#[cfg(unix)]
impl PreparedTask {
    #[cfg(test)]
    pub(crate) fn paths(&self) -> &crate::bash_background::persistence::TaskPaths {
        &self.0.paths
    }

    pub(crate) fn resolved_task(&self) -> crate::bash_background::persistence::ResolvedTask {
        crate::bash_background::persistence::ResolvedTask {
            paths: self.0.paths.clone(),
            dirs: self.0.dirs.clone(),
        }
    }

    pub(crate) fn environment(&self) -> &ChildEnvironment {
        &self.0.environment
    }

    pub(crate) fn command_text(&self) -> Result<&str, String> {
        std::str::from_utf8(&self.0.command_bytes)
            .map_err(|error| format!("verified bash command is not UTF-8: {error}"))
    }

    pub(crate) fn payload_read_grants(&self) -> Vec<PathBuf> {
        control_payload_read_grants(&self.0.paths.io_dir)
            .expect("prepared task paths already passed strict validation")
    }

    pub(crate) fn invocation(&self) -> Result<PayloadInvocation, String> {
        let wrapper = std::str::from_utf8(&self.0.wrapper_bytes)
            .map_err(|error| format!("verified wrapper payload is not UTF-8: {error}"))?;
        Ok(PayloadInvocation {
            wrapper_text: OsString::from(wrapper),
            command_text: OsString::from(self.command_text()?),
        })
    }
}

/// Complete process-launch decision consumed by a spawn primitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnPlan {
    Unsandboxed,
    Host {
        shell_path: PathBuf,
        environment: ChildEnvironment,
    },
    Launcher {
        profile: SandboxProfile,
        launcher_path: PathBuf,
    },
    #[cfg(unix)]
    Prepared {
        plan: Box<SpawnPlan>,
        task: PreparedTask,
    },
    Refused {
        code: &'static str,
        message: String,
        mismatch_class: Option<&'static str>,
    },
}

#[cfg(unix)]
#[derive(serde::Serialize, serde::Deserialize)]
enum SavedSpawnPolicy {
    Unsandboxed,
    Host {
        shell_path: PathBuf,
    },
    Launcher {
        profile: SandboxProfile,
        launcher_path: PathBuf,
    },
}

/// Private recovery data, never serialized into agent-readable task metadata.
/// The environment is exactly the one already in the verified private payload;
/// memory-only shim tickets and daemon credentials are not added to it.
#[cfg(unix)]
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedLocalLaunch {
    policy: SavedSpawnPolicy,
    root: PathBuf,
    cwd: PathBuf,
    principal: AuthenticatedPrincipal,
    shell_path: PathBuf,
    environment: Vec<(OsString, OsString)>,
    command: Vec<u8>,
    payload_digest: [u8; 32],
    linux_scope: bool,
}

#[cfg(unix)]
pub(crate) const REMOTE_LOCAL_LAUNCH: &str = "remote-local-launch";

#[cfg(unix)]
pub(crate) fn save_local_launch(
    plan: &SpawnPlan,
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell: &Path,
    linux_scope: bool,
) -> Result<String, String> {
    let prepared = plan
        .prepared_task()
        .ok_or("local fallback payload absent")?;
    let policy = match plan.policy() {
        SpawnPlan::Unsandboxed => SavedSpawnPolicy::Unsandboxed,
        SpawnPlan::Host { shell_path, .. } => SavedSpawnPolicy::Host {
            shell_path: shell_path.clone(),
        },
        SpawnPlan::Launcher {
            profile,
            launcher_path,
        } => SavedSpawnPolicy::Launcher {
            profile: profile.clone(),
            launcher_path: launcher_path.clone(),
        },
        _ => return Err("refused local launch cannot be saved".into()),
    };
    let snapshot = SavedLocalLaunch {
        policy,
        root: root.into(),
        cwd: cwd.into(),
        principal: principal.clone(),
        shell_path: plan.host_shell_path().unwrap_or(shell).into(),
        environment: prepared
            .environment()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        command: prepared.0.command_bytes.as_ref().clone(),
        payload_digest: *prepared.0.digest.as_bytes(),
        linux_scope,
    };
    let bytes = serde_json::to_vec(&snapshot).map_err(|e| e.to_string())?;
    let mut file = crate::bash_background::persistence::create_control_file(
        &prepared.0.dirs,
        REMOTE_LOCAL_LAUNCH,
        &bytes,
    )
    .map_err(|e| e.to_string())?;
    use std::io::Write;
    file.flush().map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    let mut hash = blake3::Hasher::new();
    hash.update(prepared.0.paths.task_id.as_bytes());
    hash.update(&bytes);
    Ok(hash.finalize().to_hex().to_string())
}

/// Reopen the frozen launch policy and verify the original payload. A missing
/// or changed snapshot refuses recovery; current configuration is never used.
#[cfg(unix)]
pub(crate) fn restore_local_launch(
    task: &crate::bash_background::persistence::ResolvedTask,
    expected: &str,
) -> Result<(SpawnPlan, PathBuf, HashMap<String, String>, bool), String> {
    let mut file =
        crate::bash_background::persistence::open_control_file(task, REMOTE_LOCAL_LAUNCH)
            .map_err(|e| e.to_string())?;
    let bytes = read_held_payload(&mut file)?;
    let mut hash = blake3::Hasher::new();
    hash.update(task.paths.task_id.as_bytes());
    hash.update(&bytes);
    if hash.finalize().to_hex().as_str() != expected {
        return Err("local fallback snapshot digest mismatch".into());
    }
    let snapshot: SavedLocalLaunch = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let environment: ChildEnvironment = snapshot.environment.into_iter().collect();
    let plan = match snapshot.policy {
        SavedSpawnPolicy::Unsandboxed => SpawnPlan::Unsandboxed,
        SavedSpawnPolicy::Host { shell_path } => SpawnPlan::Host {
            shell_path,
            environment: environment.clone(),
        },
        SavedSpawnPolicy::Launcher {
            profile,
            launcher_path,
        } => SpawnPlan::Launcher {
            profile,
            launcher_path,
        },
    };
    let prepared = verify_payload(
        crate::bash_background::persistence::ResolvedTask {
            paths: task.paths.clone(),
            dirs: task.dirs.clone(),
        },
        &snapshot.command,
        &snapshot.root,
        &snapshot.cwd,
        &snapshot.principal,
        &snapshot.shell_path,
        &environment,
        Some(blake3::Hash::from_bytes(snapshot.payload_digest)),
        false,
    )?;
    let env = environment
        .into_iter()
        .map(|(k, v)| {
            Ok((
                k.into_string()
                    .map_err(|_| "non-Unicode fallback environment key")?,
                v.into_string()
                    .map_err(|_| "non-Unicode fallback environment value")?,
            ))
        })
        .collect::<Result<HashMap<_, _>, String>>()?;
    Ok((
        plan.with_prepared_task(prepared),
        snapshot.shell_path,
        env,
        snapshot.linux_scope,
    ))
}

impl SpawnPlan {
    fn policy(&self) -> &Self {
        #[cfg(unix)]
        if let Self::Prepared { plan, .. } = self {
            return plan.policy();
        }
        self
    }

    #[cfg(unix)]
    pub(crate) fn with_prepared_task(self, task: PreparedTask) -> Self {
        if matches!(self, Self::Refused { .. }) {
            return self;
        }

        #[cfg(target_os = "linux")]
        let mut plan = self;
        #[cfg(not(target_os = "linux"))]
        let plan = self;

        #[cfg(target_os = "linux")]
        if let Self::Launcher { profile, .. } = &mut plan {
            let payload_grants = task.payload_read_grants();
            if let Err(error) = add_linux_payload_read_grants(profile, &payload_grants) {
                return Self::Refused {
                    code: "sandbox_unavailable",
                    message: format!(
                        "native sandbox payload read grants violate the read floor: {error}"
                    ),
                    mismatch_class: None,
                };
            }
        }

        Self::Prepared {
            plan: Box::new(plan),
            task,
        }
    }

    #[cfg(unix)]
    pub(crate) fn prepared_task(&self) -> Option<&PreparedTask> {
        match self {
            Self::Prepared { task, .. } => Some(task),
            _ => None,
        }
    }

    pub fn payload_read_grants(&self) -> Vec<PathBuf> {
        #[cfg(unix)]
        if let Some(task) = self.prepared_task() {
            return task.payload_read_grants();
        }
        Vec::new()
    }

    pub(crate) fn refusal_code(&self) -> Option<&'static str> {
        match self.policy() {
            Self::Refused { code, .. } => Some(code),
            _ => None,
        }
    }

    pub(crate) fn refusal_message(&self) -> Option<&str> {
        match self.policy() {
            Self::Refused { message, .. } => Some(message),
            _ => None,
        }
    }

    pub(crate) fn refusal_mismatch_class(&self) -> Option<&'static str> {
        match self.policy() {
            Self::Refused { mismatch_class, .. } => *mismatch_class,
            _ => None,
        }
    }

    pub(crate) fn is_native_launcher(&self) -> bool {
        matches!(self.policy(), Self::Launcher { .. })
    }

    #[cfg(unix)]
    pub(crate) fn host_shell_path(&self) -> Option<&Path> {
        match self.policy() {
            Self::Host { shell_path, .. } => Some(shell_path),
            _ => None,
        }
    }

    pub(crate) fn temp_dir(&self) -> Option<&Path> {
        match self.policy() {
            Self::Launcher { profile, .. } => Some(&profile.temp_dir),
            _ => None,
        }
    }

    pub(crate) fn cleanup_unspawned(&self) {
        let Some(temp_dir) = self.temp_dir() else {
            return;
        };
        if is_managed_task_temp_dir(temp_dir) {
            let _ = cleanup_task_temp_dir(temp_dir);
        }
    }

    #[cfg(test)]
    #[cfg(unix)]
    pub(crate) fn launcher_for_test(profile: SandboxProfile, launcher_path: PathBuf) -> Self {
        Self::Launcher {
            profile,
            launcher_path,
        }
    }

    #[cfg(test)]
    pub(crate) fn refused_for_test(code: &'static str) -> Self {
        Self::Refused {
            code,
            message: format!("bash process creation refused by sandbox policy: {code}"),
            mismatch_class: None,
        }
    }
}

/// One resolver invocation captured by the project-keyed test seam.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSpawnObservation {
    pub principal: AuthenticatedPrincipal,
    pub requested_tier: RequestedSandboxTier,
    pub task_kind: SandboxTaskKind,
}

static TEST_OBSERVATIONS: OnceLock<Mutex<HashMap<PathBuf, Vec<SandboxSpawnObservation>>>> =
    OnceLock::new();

thread_local! {
    static CURRENT_PRINCIPAL: RefCell<Option<AuthenticatedPrincipal>> = const { RefCell::new(None) };
    static WORKER_JAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(test)]
    static TEST_PLAN_OVERRIDE: RefCell<Option<SpawnPlan>> = const { RefCell::new(None) };
}

struct PrincipalScope(Option<AuthenticatedPrincipal>);

impl Drop for PrincipalScope {
    fn drop(&mut self) {
        CURRENT_PRINCIPAL.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

/// Run dispatch with server-owned principal data installed for bash resolution.
pub(crate) fn with_authenticated_principal<R>(
    principal: AuthenticatedPrincipal,
    run: impl FnOnce() -> R,
) -> R {
    let previous = CURRENT_PRINCIPAL.with(|slot| slot.replace(Some(principal)));
    let _scope = PrincipalScope(previous);
    run()
}

struct WorkerJailScope(bool);

impl Drop for WorkerJailScope {
    fn drop(&mut self) {
        WORKER_JAIL.with(|slot| slot.set(self.0));
    }
}

/// Run a stamped Alfonso worker's shell dispatch inside the worktree jail.
///
/// While `jailed` is true, a shell runs under the native sandbox whatever the
/// user's `sandbox.enabled` says, with writes confined to the route's roots
/// (plus `write_allow`, approved directory grants, caches and the task temp
/// dir). Where the sandbox cannot be enforced the command is refused with
/// `sandbox_unavailable`; it never runs unconfined. Heads and standalone
/// sessions keep the configured behaviour.
pub(crate) fn with_worker_jail<R>(jailed: bool, run: impl FnOnce() -> R) -> R {
    let previous = WORKER_JAIL.with(|slot| slot.replace(jailed));
    let _scope = WorkerJailScope(previous);
    run()
}

/// Whether the shell dispatch on this thread belongs to a jailed worker.
pub(crate) fn worker_jail_active() -> bool {
    WORKER_JAIL.with(std::cell::Cell::get)
}

/// The configured sandbox switch, forced on for a jailed worker.
pub(crate) fn sandbox_required(ctx: &AppContext) -> bool {
    ctx.config().sandbox.enabled || worker_jail_active()
}

fn worker_jail_refusal(reason: &str) -> SpawnPlan {
    SpawnPlan::Refused {
        code: "sandbox_unavailable",
        message: format!(
            "worker shells must run in the native sandbox, which cannot be enforced here: {reason}"
        ),
        mismatch_class: None,
    }
}

/// Current dispatch principal. Standalone requests are first-party by construction.
pub(crate) fn current_authenticated_principal() -> AuthenticatedPrincipal {
    CURRENT_PRINCIPAL
        .with(|slot| slot.borrow().clone())
        .unwrap_or(AuthenticatedPrincipal::FirstParty)
}

pub(crate) fn principal_is_first_party(principal: &AuthenticatedPrincipal) -> bool {
    matches!(
        principal,
        AuthenticatedPrincipal::FirstParty
            | AuthenticatedPrincipal::RouteBind {
                trust: PrincipalTrust::FirstParty,
                ..
            }
    )
}

#[cfg(unix)]
pub(crate) fn approved_payload_environment(
    overrides: &HashMap<String, String>,
    temp_dir: &Path,
) -> ChildEnvironment {
    sandboxed_child_environment(overrides, temp_dir)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn mint_host_escalation_grant(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    command: &[u8],
    root: &Path,
    cwd: &Path,
    shell_path: &Path,
    environment: &ChildEnvironment,
    storage_dir: &Path,
    session_id: &str,
) -> Result<String, String> {
    mint_host_escalation_grant_at(
        ctx,
        principal,
        command,
        root,
        cwd,
        shell_path,
        environment,
        storage_dir,
        session_id,
        Instant::now(),
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn mint_host_escalation_grant_at(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    command: &[u8],
    root: &Path,
    cwd: &Path,
    shell_path: &Path,
    environment: &ChildEnvironment,
    storage_dir: &Path,
    session_id: &str,
    now: Instant,
) -> Result<String, String> {
    let task = crate::bash_background::persistence::allocate_task_layout(storage_dir, session_id)
        .map_err(|error| format!("failed to allocate escalation payload bundle: {error}"))?;
    let prepared = match prepare_task_payload(
        &task,
        command,
        root,
        cwd,
        principal,
        shell_path,
        environment,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = crate::bash_background::persistence::delete_resolved_task(&task);
            return Err(error);
        }
    };
    let mut store = ctx.escalation_grants().lock();
    let unclaimable = store.take_unclaimable_payloads(now);
    let grant_id = loop {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| format!("failed to mint sandbox escalation grant: {error}"))?;
        let candidate = format!("esc_{}", hex_bytes(&random));
        if !store.grants.contains_key(&candidate) {
            break candidate;
        }
    };
    let grant = EscalationGrant {
        principal: principal.clone(),
        root: root.to_path_buf(),
        digest: prepared.0.digest,
        expires_at: now + ESCALATION_GRANT_TTL,
        consumed: false,
        refused: false,
        payload_reclaimed: false,
        session_dir: prepared.0.paths.session_dir.clone(),
        task_id: prepared.0.paths.task_id.clone(),
    };
    store.grants.insert(grant_id.clone(), grant);
    drop(store);
    delete_unclaimable_payloads(unclaimable);
    Ok(grant_id)
}

/// Validate one consume attempt and mark the grant used. A refusal that uses
/// up the grant also marks it refused, so its payload bundle is reclaimed.
#[cfg(unix)]
fn check_escalation_grant(
    store: &mut EscalationGrantStore,
    principal: &AuthenticatedPrincipal,
    attempt: &HostEscalationAttempt,
    now: Instant,
) -> Result<(blake3::Hash, PathBuf, String), EscalationRefusal> {
    let Some(grant) = store.grants.get_mut(&attempt.grant_id) else {
        return Err(EscalationRefusal::DigestMismatch);
    };
    if grant.consumed {
        return Err(EscalationRefusal::Consumed);
    }
    let refusal = if now >= grant.expires_at {
        Some(EscalationRefusal::Expired)
    } else if grant.principal != *principal {
        Some(EscalationRefusal::WrongPrincipal)
    } else if grant.root != attempt.root {
        Some(EscalationRefusal::DigestMismatch)
    } else {
        None
    };
    grant.consumed = true;
    if let Some(refusal) = refusal {
        grant.refused = true;
        return Err(refusal);
    }
    Ok((
        grant.digest,
        grant.session_dir.clone(),
        grant.task_id.clone(),
    ))
}

#[cfg(unix)]
fn consume_host_escalation_grant_at(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    attempt: &HostEscalationAttempt,
    now: Instant,
) -> Result<PreparedTask, EscalationRefusal> {
    let (digest, session_dir, task_id) = {
        let mut store = ctx.escalation_grants().lock();
        let checked = check_escalation_grant(&mut store, principal, attempt, now);
        let unclaimable = store.take_unclaimable_payloads(now);
        drop(store);
        delete_unclaimable_payloads(unclaimable);
        checked?
    };

    let task = crate::bash_background::persistence::resolve_uninitialized_task_layout(
        &session_dir,
        &task_id,
    )
    .map_err(|_| EscalationRefusal::DigestMismatch)?;
    verify_payload(
        task,
        &attempt.command,
        &attempt.root,
        &attempt.cwd,
        principal,
        &attempt.shell_path,
        &attempt.environment,
        Some(digest),
        true,
    )
    .map_err(|_| EscalationRefusal::DigestMismatch)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_task_payload(
    task: &crate::bash_background::persistence::ResolvedTask,
    command: &[u8],
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell_path: &Path,
    environment: &ChildEnvironment,
) -> Result<PreparedTask, String> {
    materialize_payload(
        crate::bash_background::persistence::ResolvedTask {
            paths: task.paths.clone(),
            dirs: task.dirs.clone(),
        },
        command,
        root,
        cwd,
        principal,
        shell_path,
        environment,
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn materialize_payload(
    task: crate::bash_background::persistence::ResolvedTask,
    command_bytes: &[u8],
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell_path: &Path,
    environment: &ChildEnvironment,
) -> Result<PreparedTask, String> {
    let environment_bytes = encode_environment(environment);
    let digest = payload_digest(
        &task.paths.task_id,
        command_bytes,
        crate::bash_background::process::PAYLOAD_WRAPPER,
        &environment_bytes,
        root,
        cwd,
        principal,
        shell_path,
        environment,
    );
    crate::bash_background::persistence::create_control_file(
        &task.dirs,
        crate::bash_background::persistence::COMMAND_FILE,
        command_bytes,
    )
    .map_err(|error| format!("failed to materialize command payload: {error}"))?;
    crate::bash_background::persistence::create_control_file(
        &task.dirs,
        crate::bash_background::persistence::WRAPPER_FILE,
        crate::bash_background::process::PAYLOAD_WRAPPER,
    )
    .map_err(|error| format!("failed to materialize wrapper payload: {error}"))?;
    crate::bash_background::persistence::create_control_file(
        &task.dirs,
        crate::bash_background::persistence::ENVIRONMENT_FILE,
        &environment_bytes,
    )
    .map_err(|error| format!("failed to materialize environment payload: {error}"))?;
    crate::bash_background::persistence::create_control_file(
        &task.dirs,
        crate::bash_background::persistence::MANIFEST_FILE,
        digest.as_bytes(),
    )
    .map_err(|error| format!("failed to materialize payload manifest: {error}"))?;
    verify_payload(
        task,
        command_bytes,
        root,
        cwd,
        principal,
        shell_path,
        environment,
        Some(digest),
        true,
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn verify_payload(
    task: crate::bash_background::persistence::ResolvedTask,
    expected_command: &[u8],
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell_path: &Path,
    expected_environment: &ChildEnvironment,
    expected_digest: Option<blake3::Hash>,
    reject_extra_objects: bool,
) -> Result<PreparedTask, String> {
    if reject_extra_objects {
        validate_payload_control_names(&task)?;
    }

    let mut command = crate::bash_background::persistence::open_control_file(
        &task,
        crate::bash_background::persistence::COMMAND_FILE,
    )
    .map_err(|error| format!("failed to open command payload: {error}"))?;
    let mut wrapper = crate::bash_background::persistence::open_control_file(
        &task,
        crate::bash_background::persistence::WRAPPER_FILE,
    )
    .map_err(|error| format!("failed to open wrapper payload: {error}"))?;
    let mut environment_file = crate::bash_background::persistence::open_control_file(
        &task,
        crate::bash_background::persistence::ENVIRONMENT_FILE,
    )
    .map_err(|error| format!("failed to open environment payload: {error}"))?;
    let mut manifest = crate::bash_background::persistence::open_control_file(
        &task,
        crate::bash_background::persistence::MANIFEST_FILE,
    )
    .map_err(|error| format!("failed to open payload manifest: {error}"))?;

    let command_bytes = read_held_payload(&mut command)?;
    let wrapper_bytes = read_held_payload(&mut wrapper)?;
    let environment_bytes = read_held_payload(&mut environment_file)?;
    let manifest_bytes = read_held_payload(&mut manifest)?;
    let environment = decode_environment(&environment_bytes)?;
    let digest = payload_digest(
        &task.paths.task_id,
        &command_bytes,
        &wrapper_bytes,
        &environment_bytes,
        root,
        cwd,
        principal,
        shell_path,
        expected_environment,
    );
    if command_bytes != expected_command
        || environment != *expected_environment
        || manifest_bytes.as_slice() != digest.as_bytes()
        || expected_digest.is_some_and(|expected| expected != digest)
    {
        return Err("escalation payload manifest digest mismatch".to_string());
    }
    if reject_extra_objects {
        validate_payload_control_names(&task)?;
    }
    command
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind command payload: {error}"))?;
    wrapper
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind wrapper payload: {error}"))?;
    environment_file
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind environment payload: {error}"))?;
    Ok(PreparedTask(Arc::new(PreparedTaskInner {
        paths: task.paths,
        dirs: task.dirs,
        digest,
        environment,
        command_bytes: Arc::new(command_bytes),
        wrapper_bytes: Arc::new(wrapper_bytes),
        _command_file: Arc::new(command),
        _wrapper_file: Arc::new(wrapper),
        _environment_file: Arc::new(environment_file),
    })))
}

#[cfg(unix)]
fn validate_payload_control_names(
    task: &crate::bash_background::persistence::ResolvedTask,
) -> Result<(), String> {
    let mut names = task
        .dirs
        .control
        .list_names()
        .map_err(|error| format!("failed to enumerate escalation payload: {error}"))?;
    names.sort();
    let mut expected = [
        OsString::from(crate::bash_background::persistence::COMMAND_FILE),
        OsString::from(crate::bash_background::persistence::ENVIRONMENT_FILE),
        OsString::from(crate::bash_background::persistence::MANIFEST_FILE),
        OsString::from(crate::bash_background::persistence::WRAPPER_FILE),
    ];
    expected.sort();
    if names.as_slice() != expected.as_slice() {
        return Err(format!(
            "escalation payload contains a missing or extra object: {names:?}"
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn read_held_payload(file: &mut File) -> Result<Vec<u8>, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind held payload: {error}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read held payload: {error}"))?;
    Ok(bytes)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn payload_digest(
    task_id: &str,
    command: &[u8],
    wrapper: &[u8],
    environment_bytes: &[u8],
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell_path: &Path,
    environment: &ChildEnvironment,
) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, ESCALATION_DIGEST_TAG);
    hash_field(&mut hasher, task_id.as_bytes());
    for (role, name, bytes) in [
        (
            b"command".as_slice(),
            crate::bash_background::persistence::COMMAND_FILE,
            command,
        ),
        (
            b"wrapper".as_slice(),
            crate::bash_background::persistence::WRAPPER_FILE,
            wrapper,
        ),
        (
            b"environment".as_slice(),
            crate::bash_background::persistence::ENVIRONMENT_FILE,
            environment_bytes,
        ),
    ] {
        hash_field(&mut hasher, role);
        hash_field(&mut hasher, name.as_bytes());
        hash_field(&mut hasher, bytes);
    }
    hash_field(&mut hasher, &os_bytes(root.as_os_str()));
    hash_field(&mut hasher, &os_bytes(cwd.as_os_str()));
    hash_principal(&mut hasher, principal);
    hash_field(&mut hasher, &os_bytes(shell_path.as_os_str()));
    hash_field(&mut hasher, env!("CARGO_PKG_VERSION").as_bytes());
    hash_field(&mut hasher, ESCALATION_TIER);
    hasher.update(&(environment.len() as u64).to_be_bytes());
    for (key, value) in environment {
        hash_field(&mut hasher, &os_bytes(key));
        hash_field(&mut hasher, &os_bytes(value));
    }
    hasher.finalize()
}

#[cfg(unix)]
fn encode_environment(environment: &ChildEnvironment) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(ENVIRONMENT_TAG);
    bytes.extend_from_slice(&(environment.len() as u64).to_be_bytes());
    for (key, value) in environment {
        let key = os_bytes(key);
        let value = os_bytes(value);
        bytes.extend_from_slice(&(key.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&key);
        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&value);
    }
    bytes
}

#[cfg(unix)]
fn decode_environment(bytes: &[u8]) -> Result<ChildEnvironment, String> {
    use std::os::unix::ffi::OsStringExt;

    let mut cursor = ENVIRONMENT_TAG.len();
    if !bytes.starts_with(ENVIRONMENT_TAG) {
        return Err("invalid environment payload tag".to_string());
    }
    let count = read_u64(bytes, &mut cursor)?;
    let mut environment = ChildEnvironment::new();
    for _ in 0..count {
        let key_len = read_u64(bytes, &mut cursor)? as usize;
        let key = take_bytes(bytes, &mut cursor, key_len)?;
        let value_len = read_u64(bytes, &mut cursor)? as usize;
        let value = take_bytes(bytes, &mut cursor, value_len)?;
        if environment
            .insert(OsString::from_vec(key), OsString::from_vec(value))
            .is_some()
        {
            return Err("duplicate key in environment payload".to_string());
        }
    }
    if cursor != bytes.len() {
        return Err("trailing bytes in environment payload".to_string());
    }
    Ok(environment)
}

#[cfg(unix)]
fn read_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let field = take_bytes(bytes, cursor, 8)?;
    Ok(u64::from_be_bytes(field.try_into().map_err(|_| {
        "invalid environment length field".to_string()
    })?))
}

#[cfg(unix)]
fn take_bytes(bytes: &[u8], cursor: &mut usize, len: usize) -> Result<Vec<u8>, String> {
    let end = cursor
        .checked_add(len)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| "truncated environment payload".to_string())?;
    let value = bytes[*cursor..end].to_vec();
    *cursor = end;
    Ok(value)
}

#[cfg(unix)]
fn hash_principal(hasher: &mut blake3::Hasher, principal: &AuthenticatedPrincipal) {
    match principal {
        AuthenticatedPrincipal::FirstParty => hash_field(hasher, b"first_party"),
        AuthenticatedPrincipal::RouteBind {
            trust,
            route_channel,
            route_epoch,
            project_root,
            harness,
            session_id,
            principal_id,
        } => {
            hash_field(hasher, b"route_bind");
            hash_field(
                hasher,
                match trust {
                    PrincipalTrust::FirstParty => b"first_party",
                    PrincipalTrust::Untrusted => b"untrusted",
                },
            );
            hasher.update(&route_channel.to_be_bytes());
            hasher.update(&route_epoch.to_be_bytes());
            hash_field(hasher, &os_bytes(project_root.as_os_str()));
            hash_field(hasher, harness.as_bytes());
            hash_field(hasher, session_id.as_bytes());
            match principal_id {
                Some(id) => {
                    hasher.update(&[1]);
                    hash_field(hasher, id.as_bytes());
                }
                None => {
                    hasher.update(&[0]);
                }
            }
        }
    }
}

#[cfg(unix)]
fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(unix)]
fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Returns true on Unix when native sandboxing is enabled for a first-party caller.
///
/// Process spawning and in-process bash rewriting both use this check so the
/// command executes through the configured sandbox instead of being rewritten
/// to run inside the unsandboxed AFT process.
pub(crate) fn native_sandbox_enforced(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
) -> bool {
    cfg!(unix) && sandbox_required(ctx) && principal_is_first_party(principal)
}

pub(crate) fn unsupported_platform_sandbox_refusal(ctx: &AppContext) -> Option<SpawnPlan> {
    if cfg!(unix) {
        return None;
    }
    if worker_jail_active() {
        return Some(worker_jail_refusal("this platform has no native sandbox"));
    }
    ctx.config().sandbox.enabled.then(|| SpawnPlan::Refused {
        code: "sandbox_unavailable",
        message: "sandbox is not supported on this platform; disable sandbox.enabled or run on macOS/Linux"
            .to_string(),
        mismatch_class: None,
    })
}

/// Resolve policy for an agent-command process.
///
/// `task_bundle_dir` must be the already-created directory that owns the task's
/// capture files. The native builder grants a fresh private temp directory
/// beneath it; capture files themselves use pre-opened descriptors.
pub fn resolve_sandbox_spawn(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    requested_tier: RequestedSandboxTier,
    task_kind: SandboxTaskKind,
    task_bundle_dir: &Path,
    host_escalation: Option<&HostEscalationAttempt>,
) -> SpawnPlan {
    note_test_observation(ctx, principal, requested_tier, task_kind);

    // This check must remain ahead of every platform and production-policy
    // branch. Windows tests use it to avoid entering process paths that cannot
    // consume native launcher plans.
    #[cfg(test)]
    if let Some(plan) = TEST_PLAN_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return plan;
    }

    // An enabled policy must never degrade into an ordinary child merely
    // because this build has no kernel sandbox backend.
    if let Some(refusal) = unsupported_platform_sandbox_refusal(ctx) {
        return refusal;
    }

    if worker_jail_active() {
        // A jailed worker never runs unconfined: neither an unsandboxed tier
        // nor a host escalation, nor a principal the native launcher refuses.
        if requested_tier != RequestedSandboxTier::Native {
            return worker_jail_refusal("the command asked to leave the sandbox");
        }
        if !native_sandbox_enforced(ctx, principal) {
            return worker_jail_refusal("the route's principal cannot use the native launcher");
        }
    }

    if requested_tier == RequestedSandboxTier::Disabled || !sandbox_required(ctx) {
        return SpawnPlan::Unsandboxed;
    }

    if requested_tier == RequestedSandboxTier::Host {
        if !principal_is_first_party(principal) {
            return SpawnPlan::Refused {
                code: "sandbox_escalation_denied",
                message: "sandbox host escalation is unavailable to untrusted principals"
                    .to_string(),
                mismatch_class: Some("wrong_principal"),
            };
        }

        #[cfg(windows)]
        {
            let _ = (ctx, task_kind, task_bundle_dir, host_escalation);
            unreachable!("unsupported platforms return before host-tier resolution");
        }

        #[cfg(unix)]
        {
            let Some(attempt) = host_escalation else {
                return escalation_refused(EscalationRefusal::DigestMismatch);
            };
            return match consume_host_escalation_grant_at(ctx, principal, attempt, Instant::now()) {
                Ok(prepared) => SpawnPlan::Host {
                    shell_path: attempt.shell_path.clone(),
                    environment: prepared.environment().clone(),
                }
                .with_prepared_task(prepared),
                Err(refusal) => escalation_refused(refusal),
            };
        }

        #[cfg(all(not(unix), not(windows)))]
        {
            let _ = (ctx, task_kind, task_bundle_dir, host_escalation);
            unreachable!("unsupported platforms return before host-tier resolution");
        }
    }

    if !native_sandbox_enforced(ctx, principal) {
        return SpawnPlan::Unsandboxed;
    }

    #[cfg(windows)]
    {
        let _ = (ctx, task_kind, task_bundle_dir);
        unreachable!("unsupported platforms return before native-tier resolution")
    }

    #[cfg(unix)]
    {
        let profile = match build_native_profile(ctx, principal, task_bundle_dir) {
            Ok(profile) => profile,
            Err(error) => {
                return sandbox_setup_refusal(ctx, principal, error);
            }
        };
        let launcher_path = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                let _ = cleanup_task_temp_dir(&profile.temp_dir);
                return sandbox_setup_refusal(
                    ctx,
                    principal,
                    format!("failed to locate the aft executable: {error}"),
                );
            }
        };
        crate::slog_info!(
            "sandbox profile apply: tier=native task_kind={task_kind:?} writable_roots={} read_deny={}",
            profile.writable_roots.len(),
            profile.read_deny.len()
        );
        crate::slog_debug!(
            "sandbox profile paths: tier=native writable_roots={:?} write_deny_nested={:?} read_deny={:?} socket_deny={:?} cache_roots={:?} temp_dir={:?}",
            profile.writable_roots,
            profile.write_deny_nested,
            profile.read_deny,
            profile.socket_deny,
            profile.cache_roots,
            profile.temp_dir
        );
        SpawnPlan::Launcher {
            profile,
            launcher_path,
        }
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (ctx, principal, task_kind, task_bundle_dir);
        unreachable!("unsupported platforms return before native-tier resolution")
    }
}

#[cfg(unix)]
fn sandbox_setup_refusal(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    cause: impl std::fmt::Display,
) -> SpawnPlan {
    let config = ctx.config();
    let root = config
        .project_root
        .as_deref()
        .map_or_else(|| Path::new("<unknown>"), |root| root);
    let session = match principal {
        AuthenticatedPrincipal::RouteBind { session_id, .. } => session_id.clone(),
        AuthenticatedPrincipal::FirstParty => {
            crate::log_ctx::current_session().unwrap_or_else(|| "<unknown>".to_string())
        }
    };
    let message = format!(
        "sandbox setup for {} failed: {cause}; set sandbox.enabled=false to disable native sandboxing",
        root.display()
    );
    crate::slog_warn!("{message}; root={} session={session}", root.display());
    SpawnPlan::Refused {
        code: "sandbox_unavailable",
        message,
        mismatch_class: None,
    }
}

#[cfg(unix)]
fn escalation_refused(refusal: EscalationRefusal) -> SpawnPlan {
    let class = refusal.class();
    SpawnPlan::Refused {
        code: "sandbox_escalation_denied",
        message: format!("sandbox host escalation grant refused: {class}"),
        mismatch_class: Some(class),
    }
}

#[cfg(unix)]
fn build_native_profile(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    task_bundle_dir: &Path,
) -> Result<SandboxProfile, String> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            "HOME is not set, so credential and cache paths cannot be resolved".to_string()
        })?;
    if !home.is_absolute() {
        return Err(format!("HOME must be absolute: {}", home.display()));
    }
    let home = home
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize HOME {}: {error}", home.display()))?;
    if !home.is_dir() {
        return Err(format!("HOME is not a directory: {}", home.display()));
    }

    let mut project_roots = Vec::new();
    if let Some(root) = &ctx.config().project_root {
        project_roots.push(root.clone());
    }
    if let AuthenticatedPrincipal::RouteBind { project_root, .. } = principal {
        if !project_roots.contains(project_root) {
            project_roots.push(project_root.clone());
        }
    }
    if project_roots.is_empty() {
        project_roots.push(
            std::env::current_dir()
                .map_err(|error| format!("failed to resolve the current project root: {error}"))?,
        );
    }

    for root in &mut project_roots {
        if !root.is_dir() {
            return Err(format!(
                "project root is not an existing directory: {}",
                root.display()
            ));
        }
        *root = root.canonicalize().map_err(|error| {
            format!(
                "failed to canonicalize project root {}: {error}",
                root.display()
            )
        })?;
    }
    project_roots.sort_unstable();
    project_roots.dedup();

    if !task_bundle_dir.is_dir() {
        return Err(format!(
            "task io directory is not an existing directory: {}",
            task_bundle_dir.display()
        ));
    }
    let task_io_dir = task_bundle_dir
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize task io directory: {error}"))?;
    let session_store = session_store_for_task_io(&task_io_dir)?;

    let git_policies = project_roots
        .iter()
        .map(|root| resolve_git_policy(root))
        .collect::<Result<Vec<_>, _>>()?;
    let temp_dir = create_task_temp_dir(&task_io_dir)?;
    let result = (|| {
        let mut policy = ctx.access_policy("").map_err(|response| {
            response.data["message"]
                .as_str()
                .unwrap_or("access policy could not be resolved")
                .to_string()
        })?;
        policy.roots = project_roots.clone();
        policy
            .private_stores
            .read_allow
            .extend(project_roots.iter().cloned());
        policy.private_stores.read_allow.sort_unstable();
        policy.private_stores.read_allow.dedup();
        policy.private_stores.write_allow = project_roots.clone();
        policy.private_stores.deny.push(session_store.clone());
        let mut write_deny_nested = Vec::new();
        let mut shell_write_deny = Vec::new();
        let mut shell_read_deny = Vec::new();
        let jailed = worker_jail_active();
        #[cfg(target_os = "macos")]
        if jailed {
            install_worker_mktemp(&temp_dir)?;
        }
        for (root, git_policy) in project_roots.iter().zip(&git_policies) {
            #[cfg(target_os = "linux")]
            write_deny_nested.push(root.join(".git"));
            if jailed {
                if let Some((git_dir, _)) = &git_policy.linked {
                    // These files bind the checkout to operator-owned Git
                    // configuration. A shell must not redirect that binding
                    // and turn model-written configuration into read grants.
                    write_deny_nested.push(root.join(".git"));
                    write_deny_nested.extend(
                        ["config.worktree", "commondir", "gitdir"].map(|name| git_dir.join(name)),
                    );
                }
                // Protect executable filters and project configuration, not
                // scratch documents that file tools already let workers write.
                write_deny_nested.extend([
                    root.join(".cortexkit/aft.jsonc"),
                    root.join(".cortexkit/aft/filters"),
                ]);
            } else {
                write_deny_nested.push(root.join(".cortexkit"));
            }
            #[cfg(target_os = "macos")]
            shell_write_deny.extend(git_policy.hooks.iter().cloned());
            if jailed {
                // Git's configured policy hooks must execute on commits. Keep
                // them immutable even when their directory is in private data.
                policy.private_stores.read_allow.extend(
                    git_policy
                        .hooks
                        .iter()
                        .filter(|path| path.is_dir())
                        .cloned(),
                );
                policy
                    .private_stores
                    .read_allow
                    .extend(git_policy.read_roots.iter().cloned());
                policy
                    .private_stores
                    .read_metadata_allow
                    .extend(git_policy.metadata_allow.iter().cloned());
                for (key, path) in &git_policy.configured_paths {
                    if (key == "core.excludesFile" && path.is_file())
                        || (key != "core.excludesFile" && path.is_dir())
                    {
                        policy.private_stores.read_allow.push(path.clone());
                        if key == "core.excludesFile"
                            && git_policy
                                .linked
                                .as_ref()
                                .is_some_and(|(git_dir, _)| path.starts_with(git_dir))
                        {
                            // core.excludesFile can be inside the linked worktree's Git
                            // directory. Landlock cannot subtract a file from that
                            // writable directory; report the nested exclusion
                            // instead of refusing all Git commands.
                            write_deny_nested.push(path.clone());
                        } else {
                            shell_write_deny.push(path.clone());
                        }
                    }
                }
            } else {
                // Non-worker native shells still cannot read repository hooks.
                shell_read_deny.extend(git_policy.hooks.iter().cloned());
            }
        }
        let storage = ctx.storage_dir();
        let mut managed_paths = Vec::new();
        if ctx.config().github.shim {
            managed_paths.push(storage.join(crate::agent_child_env::SHIMS_DIR_NAME));
            managed_paths.push(crate::agent_child_env::shim_binary(&ctx.config())?);
        }
        if ctx.config().git.co_author != "off" {
            managed_paths.push(crate::agent_child_env::managed_git_hooks_dir(&storage));
            if ctx.config().git.co_author == "auto" {
                managed_paths.push(crate::agent_child_env::shim_binary(&ctx.config())?);
            }
        }
        // Governance injection materializes these before resolving a spawn.
        // Do not grant a missing shim directory or a broad binary parent.
        managed_paths.retain(|path| path.exists());
        shell_write_deny.extend(managed_paths);

        let mut cache_roots = vec![
            home.join(".cargo/registry"),
            home.join(".cargo/git"),
            home.join(".rustup/downloads"),
            home.join(".npm"),
            home.join(".bun/install/cache"),
            home.join(".cache/pip"),
            home.join(".cache/uv"),
            home.join(".cache/go-build"),
            home.join(".gradle/caches"),
            home.join(".m2/repository"),
        ];
        #[cfg(target_os = "macos")]
        cache_roots.extend([
            home.join("Library/Caches/pip"),
            home.join("Library/Caches/uv"),
            home.join("Library/Caches/go-build"),
        ]);
        cache_roots.retain(|path| path.is_dir());
        // A jailed worker's shell keeps writes to its worktree, plus what normal
        // builds and commits need, all taken from operator configuration.
        let jail = worker_jail_active()
            .then(|| worker_jail_grants(&home, &project_roots, &git_policies, &policy));
        if let Some(jail) = &jail {
            policy
                .private_stores
                .read_allow
                .extend(jail.read_only.iter().cloned());
            policy
                .private_stores
                .read_allow
                .extend(jail.files.iter().cloned());
            policy
                .private_stores
                .write_allow
                .extend(jail.files.iter().cloned());
            shell_write_deny.extend(jail.read_only.iter().cloned());
            cache_roots.extend(jail.caches.iter().cloned());
            cache_roots.sort_unstable();
            cache_roots.dedup();
        }

        let mut socket_deny = vec![PathBuf::from("/var/run/docker.sock")];
        if let Some(agent_socket) =
            std::env::var_os("SSH_AUTH_SOCK").filter(|value| !value.is_empty())
        {
            socket_deny.push(PathBuf::from(agent_socket));
        }

        let mut profile = policy
            .compile_shell(temp_dir.clone(), cache_roots, socket_deny)
            .map_err(|error| error.to_string())?;
        profile.write_deny.extend(shell_write_deny);
        profile.write_deny_nested = write_deny_nested;
        profile.read_deny.extend(shell_read_deny);
        if let Some(jail) = jail {
            // Read server destinations from the operator's environment, never
            // from a worker request or a file the worker can edit.
            profile.loopback_tcp_connect_deny =
                sccache_server_ports(std::env::var("SCCACHE_SERVER_PORT").ok().as_deref());
            if let Some(path) = std::env::var_os("SCCACHE_SERVER_UDS").filter(|v| !v.is_empty()) {
                let path = PathBuf::from(path);
                let path = if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()
                        .map_err(|error| format!("failed to resolve sccache socket: {error}"))?
                        .join(path)
                };
                profile.socket_deny.push(path);
            }
            profile.writable_roots.extend(jail.roots);
            profile.writable_roots.sort_unstable();
            profile.writable_roots.dedup();
            profile.writable_files = jail.files;
        }
        profile = profile
            .canonicalize_for_launch()
            .map_err(|error| error.to_string())?;
        refuse_store_overlap(&profile, &session_store, &task_io_dir)?;

        #[cfg(target_os = "linux")]
        let profile = {
            let git_read_roots = git_policies
                .iter()
                .flat_map(|policy| policy.read_roots.iter().cloned())
                .collect::<Vec<_>>();
            profile = build_linux_profile(profile, &home, &git_read_roots)?;
            profile = profile
                .canonicalize_for_launch()
                .map_err(|error| error.to_string())?;
            validate_final_read_rules(&profile.read_allow, &profile.read_deny)?;
            assert!(
                validate_final_read_rules(&profile.read_allow, &profile.read_deny).is_ok(),
                "final Landlock read grants overlap a denied path"
            );
            profile
        };

        Ok(profile)
    })();
    if result.is_err() {
        let _ = cleanup_task_temp_dir(&temp_dir);
    }
    result
}

/// Adjust the governed child environment for a jailed worker shell. The caller
/// has already added managed Git and GitHub command settings to the request
/// environment.
///
/// - `RUSTC_WRAPPER` and `CARGO_BUILD_RUSTC_WRAPPER` are set empty, which makes
///   cargo ignore any `build.rustc-wrapper` from its configuration. A wrapper
///   such as sccache hands compiles to a server that runs outside the sandbox
///   and writes outputs wherever it is told, so it must not run for a jailed
///   build. Jailed local builds get slower; workers build with runon.
/// - `CARGO_HOME` and `RUSTUP_HOME`, when the daemon has them, so cargo and
///   rustup in the shell use the homes whose caches the jail grants instead of
///   the defaults under HOME. Other daemon variables are inherited only through
///   the sandbox's base-environment allowlist.
/// - All `SCCACHE_*` settings are removed, including server port/UDS and startup
///   notification channels. Storage endpoints are not compile-server addresses,
///   but are removed too so future sccache channels cannot bypass this scrub.
pub(crate) fn apply_worker_jail_environment(environment: &mut HashMap<String, String>) {
    if !worker_jail_active() {
        return;
    }
    environment.retain(|key, _| !key.starts_with("SCCACHE_"));
    for key in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
        environment.insert(key.to_string(), String::new());
    }
    #[cfg(unix)]
    for key in ["CARGO_HOME", "RUSTUP_HOME"] {
        if !environment.contains_key(key) {
            if let Some(path) = absolute_env_path(key) {
                environment.insert(key.to_string(), path.to_string_lossy().into_owned());
            }
        }
    }
}

#[cfg(unix)]
fn sccache_server_ports(configured: Option<&str>) -> Vec<u16> {
    let mut ports = vec![4226];
    if let Some(port) = configured.and_then(|value| value.parse::<u16>().ok()) {
        if port != 0 && port != 4226 {
            ports.push(port);
        }
    }
    ports
}

/// What a jailed worker's shell may write outside its worktree.
#[cfg(unix)]
#[derive(Debug, Default, PartialEq, Eq)]
struct WorkerJailGrants {
    /// Package-manager and compiler caches (added to the cache roots).
    caches: Vec<PathBuf>,
    /// Directories outside the worktree: Git stores and operator build targets.
    roots: Vec<PathBuf>,
    /// Exact-file write grants for packed refs, Cargo cache tracking and
    /// build-slot locks, without granting their parent directories.
    files: Vec<PathBuf>,
    /// Cargo configuration, .rust-build-seeds, and immutable compiler launcher files.
    read_only: Vec<PathBuf>,
}

/// Cargo's lock and global-cache tracking files at the root of CARGO_HOME. The
/// directory itself also holds `bin` and `config.toml`, which stay read-only:
/// a shell that could rewrite them could run code outside the sandbox later.
#[cfg(unix)]
const CARGO_HOME_FILES: &[&str] = &[
    ".package-cache",
    ".package-cache-mutate",
    ".global-cache",
    ".global-cache-journal",
    ".global-cache-wal",
    ".global-cache-shm",
];

#[cfg(unix)]
fn absolute_env_path(name: &str) -> Option<PathBuf> {
    crate::environment::non_empty_os_var(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// What a jailed worker's shell may write outside its worktree. Every entry
/// comes from operator configuration: the AFT process environment, cargo
/// configuration files outside the worktree, and the standard cache homes.
/// Nothing comes from the command, its environment, or files inside the
/// worktree, which the model can edit.
///
/// - Git: a linked worktree's own Git directory, the shared object, ref and
///   log stores, and exactly `packed-refs`, `packed-refs.lock`, and
///   `packed-refs.new`. The shared
///   `config`, hooks, `info` and other worktrees' entries stay read-only.
/// - Cargo: `$CARGO_HOME/registry` and `git`, its lock files, and
///   `$RUSTUP_HOME/downloads` and `tmp`.
/// - Build targets named by `CARGO_TARGET_DIR` in the AFT environment or by
///   `build.target-dir` in cargo configuration outside the worktree, unless the
///   target is a protected path or would contain the home or the worktree.
#[cfg(unix)]
fn worker_jail_grants(
    home: &Path,
    project_roots: &[PathBuf],
    git_policies: &[GitPolicy],
    policy: &crate::access_policy::AccessPolicy,
) -> WorkerJailGrants {
    let mut grants = WorkerJailGrants {
        roots: linked_git_grants(git_policies),
        files: linked_git_file_grants(git_policies),
        ..WorkerJailGrants::default()
    };

    let cargo_home = absolute_env_path("CARGO_HOME").unwrap_or_else(|| home.join(".cargo"));
    if cargo_home.is_dir() {
        for cache in ["registry", "git"] {
            let cache = cargo_home.join(cache);
            if std::fs::create_dir_all(&cache).is_ok() {
                grants.caches.push(cache);
            }
        }
        grants
            .files
            .extend(CARGO_HOME_FILES.iter().map(|name| cargo_home.join(name)));
    }
    let rustup_home = absolute_env_path("RUSTUP_HOME").unwrap_or_else(|| home.join(".rustup"));
    if rustup_home.is_dir() {
        for cache in ["downloads", "tmp"] {
            let cache = rustup_home.join(cache);
            if std::fs::create_dir_all(&cache).is_ok() {
                grants.caches.push(cache);
            }
        }
    }

    // Cargo reads configuration from CARGO_HOME and from every ancestor of the
    // working directory. Only files outside the worktree are operator-owned.
    let mut configs = vec![
        (
            cargo_home.join("config.toml"),
            cargo_home.parent().map(Path::to_path_buf),
        ),
        (
            cargo_home.join("config"),
            cargo_home.parent().map(Path::to_path_buf),
        ),
    ];
    for root in project_roots {
        if let Some(container) = root.parent() {
            for path in [
                container.join(".cargo/alfonso-test-threads"),
                container.join(".rust-build-seeds"),
            ] {
                if let Ok(path) = path.canonicalize() {
                    if !project_roots.iter().any(|root| path.starts_with(root)) {
                        grants.read_only.push(path);
                    }
                }
            }
        }
        for ancestor in root.ancestors().skip(1) {
            for name in ["config.toml", "config"] {
                configs.push((
                    ancestor.join(".cargo").join(name),
                    Some(ancestor.to_path_buf()),
                ));
            }
        }
    }
    let mut targets = ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"]
        .into_iter()
        .filter_map(absolute_env_path)
        .collect::<Vec<_>>();
    let mut seen = std::collections::BTreeSet::new();
    for (path, base) in configs {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(config_path) = path.canonicalize() else {
            continue;
        };
        if !config_path.is_file()
            || project_roots
                .iter()
                .any(|root| config_path.starts_with(root))
        {
            continue;
        }
        grants.read_only.push(config_path.clone());
        let Some(build) = cargo_build_settings(&config_path) else {
            continue;
        };
        if let Some(wrapper) = build.rustc_wrapper {
            let wrapper = PathBuf::from(wrapper);
            let wrapper = if wrapper.is_absolute() {
                Some(wrapper)
            } else {
                base.as_ref().map(|base| base.join(wrapper))
            };
            if let Some(wrapper) = wrapper {
                add_build_slot_grants(&wrapper, project_roots, policy, &mut grants);
            }
        }
        if let Some(target) = build.target_dir {
            let target = PathBuf::from(target);
            match (&base, target.is_absolute()) {
                (_, true) => targets.push(target),
                (Some(base), false) => targets.push(base.join(target)),
                (None, false) => {}
            }
        }
    }

    for target in targets {
        let target = crate::inspect::job::normalize_path(&target);
        if !jail_target_allowed(&target, home, project_roots, policy) {
            log::warn!(
                "worker sandbox: ignoring configured cargo target {}: it would contain the home, a project root, or a protected path",
                target.display()
            );
            continue;
        }
        if std::fs::create_dir_all(&target).is_ok() {
            grants.roots.push(target);
        }
    }
    grants
}

/// Retain only the operator-configured `prefrontal-build-slot` launcher and its
/// `rustc-lowprio-direct` companion: they take a slot lock and compile inside
/// the sandbox rather than asking a shared server. The Cargo configuration and
/// launcher files must be outside every model-writable worktree.
#[cfg(unix)]
fn add_build_slot_grants(
    wrapper: &Path,
    project_roots: &[PathBuf],
    policy: &crate::access_policy::AccessPolicy,
    grants: &mut WorkerJailGrants,
) {
    let Ok(wrapper) = wrapper.canonicalize() else {
        return;
    };
    if wrapper.file_name() != Some(OsStr::new("prefrontal-build-slot")) || !wrapper.is_file() {
        return;
    }
    let Some(dir) = wrapper.parent() else {
        return;
    };
    if project_roots
        .iter()
        .any(|root| root.starts_with(dir) || dir.starts_with(root))
        || !policy
            .private_stores
            .deny
            .iter()
            .any(|deny| dir.starts_with(deny) && dir != deny)
    {
        return;
    }
    let paths = [
        wrapper.clone(),
        dir.join("rustc-lowprio-direct"),
        dir.join(".root"),
        dir.join(".cache"),
        dir.join("limit"),
    ];
    let Ok(paths) = paths
        .into_iter()
        .map(|path| path.canonicalize())
        .collect::<Result<Vec<_>, _>>()
    else {
        return;
    };
    if paths
        .iter()
        .any(|path| !path.is_file() || !path.starts_with(dir))
    {
        return;
    }
    let Some(limit) = std::fs::read_to_string(dir.join("limit"))
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|limit| (1..=256).contains(limit))
    else {
        return;
    };
    // Landlock's single-file grants require existing inodes. Provision only
    // the configured queue and slot locks, never a writable directory grant.
    // Include both endpoints so zero-based and one-based slot numbering work.
    let locks = std::iter::once(dir.join("queue.lock"))
        .chain((0..=limit).map(|slot| dir.join(format!("{slot}.lock"))));
    let mut files = Vec::new();
    for path in locks {
        if std::fs::symlink_metadata(&path)
            .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
        {
            return;
        }
        if std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .is_err()
        {
            return;
        }
        files.push(path);
    }
    grants.files.extend(files);
    grants.read_only.extend(paths);
}

/// A linked worktree's own Git directory and its main repository's shared
/// object, ref and log stores: what `git add` and `git commit` write. The
/// shared `config`, hooks, `info` and other worktrees' entries are left out.
#[cfg(unix)]
fn linked_git_grants(git_policies: &[GitPolicy]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for (git_dir, common_dir) in git_policies.iter().filter_map(|git| git.linked.as_ref()) {
        roots.push(git_dir.clone());
        roots.extend(
            ["objects", "refs", "logs"]
                .into_iter()
                .map(|store| common_dir.join(store))
                .filter(|store| store.is_dir()),
        );
    }
    roots
}

/// Git locks packed refs and stages a rewritten table in `packed-refs.new`
/// before renaming it into place. Exact-file grants keep adjacent config and
/// hooks read-only, including when the lock and staging files do not exist yet.
#[cfg(unix)]
fn linked_git_file_grants(git_policies: &[GitPolicy]) -> Vec<PathBuf> {
    git_policies
        .iter()
        .filter_map(|git| git.linked.as_ref())
        .flat_map(|(_, common_dir)| {
            ["packed-refs", "packed-refs.lock", "packed-refs.new"].map(|name| common_dir.join(name))
        })
        .collect()
}

#[cfg(unix)]
#[derive(Debug, Default, PartialEq, Eq)]
struct CargoBuildSettings {
    target_dir: Option<String>,
    rustc_wrapper: Option<String>,
}

/// The `[build]` settings of one cargo configuration file, if it exists and parses.
#[cfg(unix)]
fn cargo_build_settings(path: &Path) -> Option<CargoBuildSettings> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_cargo_build_settings(&text)
}

#[cfg(unix)]
fn parse_cargo_build_settings(text: &str) -> Option<CargoBuildSettings> {
    let table = text.parse::<toml::Table>().ok()?;
    let build = table.get("build")?.as_table()?;
    let field = |name: &str| {
        build
            .get(name)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    };
    Some(CargoBuildSettings {
        target_dir: field("target-dir"),
        rustc_wrapper: field("rustc-wrapper"),
    })
}

/// A configured build target may not contain the home or a project root
/// (granting it would grant them), and may not be a protected path.
#[cfg(unix)]
fn jail_target_allowed(
    target: &Path,
    home: &Path,
    project_roots: &[PathBuf],
    policy: &crate::access_policy::AccessPolicy,
) -> bool {
    use crate::access_policy::{AccessDecision, AccessDenial};
    if !target.is_absolute() || target.parent().is_none() {
        return false;
    }
    let target = crate::access_policy::policy_path(target);
    !home.starts_with(&target)
        && !project_roots.iter().any(|root| root.starts_with(&target))
        && policy.decide_tree_write(&target) != AccessDecision::Deny(AccessDenial::Floor)
}

#[cfg(unix)]
pub(crate) fn control_payload_read_grants(task_io: &Path) -> Result<Vec<PathBuf>, String> {
    if task_io.file_name() != Some(OsStr::new("io")) {
        return Err("task payload grants require the directory-layout io path".to_string());
    }
    let task_dir = task_io
        .parent()
        .ok_or_else(|| "task io directory has no task parent".to_string())?;
    let task_id = task_dir
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| "task directory has no UTF-8 identity".to_string())?;
    crate::bash_background::persistence::validate_task_id(task_id)
        .map_err(|error| error.to_string())?;
    let control = task_dir.join("control");
    Ok(vec![
        control.join(crate::bash_background::persistence::COMMAND_FILE),
        control.join(crate::bash_background::persistence::WRAPPER_FILE),
        control.join(crate::bash_background::persistence::ENVIRONMENT_FILE),
    ])
}

#[cfg(unix)]
fn session_store_for_task_io(task_io: &Path) -> Result<PathBuf, String> {
    let Some(task_dir) = task_io.parent() else {
        return Err("task io directory has no task parent".to_string());
    };
    let directory_layout = task_io.file_name() == Some(OsStr::new("io"))
        && task_dir
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|task_id| {
                crate::bash_background::persistence::validate_task_id(task_id).is_ok()
            });
    let candidate = if directory_layout {
        task_dir
            .parent()
            .ok_or_else(|| "task directory has no session parent".to_string())?
    } else {
        task_io
    };
    candidate
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize bash task session store: {error}"))
}

#[cfg(unix)]
fn refuse_store_overlap(
    profile: &SandboxProfile,
    session_store: &Path,
    task_io: &Path,
) -> Result<(), String> {
    for root in profile.write_allow_roots() {
        if root == task_io || root.starts_with(task_io) {
            continue;
        }
        if root == session_store
            || root.starts_with(session_store)
            || session_store.starts_with(root)
        {
            return Err(format!(
                "sandbox writable root overlaps the bash task session store: writable={} store={}",
                root.display(),
                session_store.display()
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Debug)]
struct GitPolicy {
    read_roots: Vec<PathBuf>,
    hooks: Vec<PathBuf>,
    /// For a linked worktree: its own Git directory (`<common>/worktrees/<name>`)
    /// and the shared common directory of the main repository.
    linked: Option<(PathBuf, PathBuf)>,
    configured_paths: Vec<(String, PathBuf)>,
    metadata_allow: Vec<PathBuf>,
}

#[cfg(unix)]
#[derive(Clone)]
struct CachedGitPolicy {
    policy: GitPolicy,
    inputs: GitPolicyInputs,
    worker: bool,
}

#[cfg(unix)]
fn git_policy_cache() -> &'static Mutex<HashMap<PathBuf, CachedGitPolicy>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedGitPolicy>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct GitInputIdentity {
    device: u64,
    inode: u64,
    symlink: bool,
    // Directory contents (index.lock, refs, etc.) change on ordinary Git use.
    // Only a directory replacement affects config lookup, not its mtime.
    contents: Option<(u64, i64, i64, i64, i64)>,
}

#[cfg(unix)]
fn git_input_identity(path: &Path) -> std::io::Result<Option<GitInputIdentity>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(GitInputIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            symlink: metadata.file_type().is_symlink(),
            contents: (!metadata.is_dir()).then(|| {
                (
                    metadata.len(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            }),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
#[derive(Clone)]
struct GitPolicyInputs {
    environment: Vec<(OsString, OsString)>,
    executable: PathBuf,
    files: BTreeMap<PathBuf, Option<GitInputIdentity>>,
    resolved_paths: Vec<(PathBuf, PathBuf)>,
}

#[cfg(unix)]
impl GitPolicyInputs {
    fn watch(&mut self, path: &Path) -> Option<()> {
        // Include parent identities: a config symlink or any ancestor can be
        // retargeted without changing the inode of the previously read target.
        for ancestor in path.ancestors() {
            self.files
                .entry(ancestor.to_path_buf())
                .or_insert(git_input_identity(ancestor).ok()?);
        }
        let target = match path.canonicalize() {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (!path.ancestors().any(|ancestor| {
                    self.files
                        .get(ancestor)
                        .and_then(Option::as_ref)
                        .is_some_and(|identity| identity.symlink)
                }))
                .then_some(());
            }
            Err(_) => return None,
        };
        if target != path {
            for ancestor in target.ancestors() {
                self.files
                    .entry(ancestor.to_path_buf())
                    .or_insert(git_input_identity(ancestor).ok()?);
            }
            self.resolved_paths.push((path.to_path_buf(), target));
        }
        Some(())
    }

    fn unchanged(&self) -> bool {
        self.environment == git_policy_environment()
            && git_policy_executable().as_ref() == Some(&self.executable)
            && self.files.iter().all(|(path, identity)| {
                git_input_identity(path).is_ok_and(|current| current == *identity)
            })
            && self.resolved_paths.iter().all(|(path, expected)| {
                canonicalize_policy_path(path.clone(), "cached Git policy")
                    .is_ok_and(|current| current == *expected)
            })
    }
}

#[cfg(unix)]
fn git_policy_environment() -> Vec<(OsString, OsString)> {
    #[cfg(test)]
    if let Some(environment) = GIT_POLICY_TEST_ENV.with(|value| value.borrow().clone()) {
        return environment;
    }
    let mut environment = std::env::vars_os()
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            key.starts_with("GIT_") || matches!(key.as_ref(), "HOME" | "XDG_CONFIG_HOME" | "PATH")
        })
        .collect::<Vec<_>>();
    environment.sort_unstable();
    environment
}

#[cfg(unix)]
fn mentions_git_include(bytes: &[u8]) -> bool {
    bytes
        .windows(b"include".len())
        .any(|word| word.eq_ignore_ascii_case(b"include"))
}

#[cfg(unix)]
fn git_policy_executable() -> Option<PathBuf> {
    which::which_in(
        "git",
        Some(crate::effective_path::effective_path()),
        std::env::current_dir().ok()?,
    )
    .ok()
}

#[cfg(unix)]
fn git_policy_inputs(
    project_root: &Path,
    git_dir: &Path,
    common_dir: &Path,
    environment: Vec<(OsString, OsString)>,
) -> Option<GitPolicyInputs> {
    #[cfg(test)]
    if GIT_POLICY_BENCH_OLD.with(|value| value.get()) {
        return None;
    }
    if !crate::developer_tools::git_usable() {
        return None;
    }
    // These overrides redirect Git away from the .git/commondir topology whose
    // config files we track. Resolve normally, but never reuse that policy.
    if environment.iter().any(|(key, _)| {
        matches!(
            key.to_str(),
            Some("GIT_DIR" | "GIT_COMMON_DIR" | "GIT_WORK_TREE")
        )
    }) {
        return None;
    }
    // Command-scope includes can load arbitrary files too. Treat any mention
    // conservatively, including comments and inactive includeIf directives.
    if environment.iter().any(|(key, value)| {
        key.to_string_lossy().starts_with("GIT_CONFIG") && mentions_git_include(&os_bytes(value))
    }) {
        return None;
    }
    let mut inputs = GitPolicyInputs {
        environment,
        executable: git_policy_executable()?,
        files: BTreeMap::new(),
        resolved_paths: Vec::new(),
    };
    // Git's compiled-in system-config location can change if its executable is
    // replaced, or another git appears earlier on PATH.
    inputs.watch(&inputs.executable.clone())?;
    let mut configs = vec![common_dir.join("config"), git_dir.join("config.worktree")];
    // Git knows its compiled-in system prefix and the global-file overrides.
    // Query the *candidate* files, not only --show-origin's existing files, so
    // creating a previously absent system/global config invalidates the cache.
    // Older Git without these variables remains correct but uncached.
    for variable in ["GIT_CONFIG_SYSTEM", "GIT_CONFIG_GLOBAL"] {
        let output = git_policy_output(project_root, &["var", variable]).ok()?;
        if !output.status.success() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            for path in output
                .stdout
                .split(|byte| *byte == b'\n')
                .filter(|path| !path.is_empty())
            {
                let path = PathBuf::from(OsString::from_vec(path.to_vec()));
                configs.push(if path.is_absolute() {
                    path
                } else {
                    project_root.join(path)
                });
            }
        }
    }
    if let Some((_, path)) = inputs
        .environment
        .iter()
        .find(|(key, _)| key == "GIT_CONFIG")
    {
        let path = PathBuf::from(path);
        configs.push(if path.is_absolute() {
            path
        } else {
            project_root.join(path)
        });
    }
    inputs.watch(&project_root.join(".git"))?;
    inputs.watch(&git_dir.join("commondir"))?;
    // Capture the identity before reading: a concurrent rewrite during either
    // config inspection or policy resolution must not publish a cache entry.
    for config in configs {
        inputs.watch(&config)?;
        match std::fs::read(&config) {
            Ok(bytes) if mentions_git_include(&bytes) => return None,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(inputs)
}

#[cfg(all(test, unix))]
thread_local! {
    static GIT_POLICY_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static GIT_POLICY_TEST_ENV: RefCell<Option<Vec<(OsString, OsString)>>> = const { RefCell::new(None) };
    static GIT_POLICY_BENCH_OLD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(unix)]
fn git_policy_output(project_root: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    #[cfg(test)]
    GIT_POLICY_CALLS.with(|count| count.set(count.get() + 1));
    let mut command = crate::effective_path::new_command("git");
    #[cfg(test)]
    if GIT_POLICY_BENCH_OLD.with(|value| value.get()) {
        command.env("PATH", crate::effective_path::effective_path());
    }
    #[cfg(test)]
    GIT_POLICY_TEST_ENV.with(|value| {
        if let Some(environment) = value.borrow().as_ref() {
            for (key, _) in
                std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("GIT_"))
            {
                command.env_remove(key);
            }
            command.envs(environment.iter().cloned());
        }
    });
    command.arg("-C").arg(project_root).args(args).output()
}

#[cfg(unix)]
fn resolve_git_policy(project_root: &Path) -> Result<GitPolicy, String> {
    let environment = git_policy_environment();
    let worker = worker_jail_active();
    let cached = git_policy_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(project_root)
        .cloned();
    // Neither filesystem inspection nor a git subprocess runs under this lock.
    // Concurrent cold misses may duplicate queries rather than block other roots.
    if let Some(cached) = cached {
        if cached.worker == worker
            && cached.inputs.environment == environment
            && cached.inputs.unchanged()
        {
            return Ok(cached.policy);
        }
    }
    let (policy, inputs) = resolve_git_policy_uncached(project_root, environment, worker)?;
    let entry = inputs
        .filter(GitPolicyInputs::unchanged)
        .map(|inputs| CachedGitPolicy {
            policy: policy.clone(),
            inputs,
            worker,
        });
    let mut cache = git_policy_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(entry) = entry {
        cache.insert(project_root.to_path_buf(), entry);
    } else {
        cache.remove(project_root);
    }
    Ok(policy)
}

#[cfg(unix)]
fn resolve_git_policy_uncached(
    project_root: &Path,
    environment: Vec<(OsString, OsString)>,
    worker: bool,
) -> Result<(GitPolicy, Option<GitPolicyInputs>), String> {
    let dot_git = project_root.join(".git");
    let dot_git_identity = git_input_identity(&dot_git).ok();
    let metadata = match std::fs::symlink_metadata(&dot_git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                GitPolicy {
                    read_roots: Vec::new(),
                    hooks: vec![dot_git.join("hooks")],
                    linked: None,
                    configured_paths: Vec::new(),
                    metadata_allow: Vec::new(),
                },
                None,
            ));
        }
        Err(error) => {
            return Err(format!(
                "failed to inspect Git metadata {}: {error}",
                dot_git.display()
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "refusing sandbox profile with symlinked Git metadata: {}",
            dot_git.display()
        ));
    }

    let git_dir = if metadata.is_dir() {
        dot_git.canonicalize().map_err(|error| {
            format!(
                "failed to canonicalize Git directory {}: {error}",
                dot_git.display()
            )
        })?
    } else if metadata.is_file() {
        let pointer = std::fs::read_to_string(&dot_git).map_err(|error| {
            format!(
                "failed to read linked-worktree Git pointer {}: {error}",
                dot_git.display()
            )
        })?;
        let pointer = pointer
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                format!(
                    "linked-worktree Git pointer is malformed: {}",
                    dot_git.display()
                )
            })?;
        let pointer = PathBuf::from(pointer);
        let pointer = if pointer.is_absolute() {
            pointer
        } else {
            project_root.join(pointer)
        };
        pointer.canonicalize().map_err(|error| {
            format!(
                "failed to resolve linked-worktree Git directory {}: {error}",
                pointer.display()
            )
        })?
    } else {
        return Err(format!(
            "Git metadata is neither a file nor directory: {}",
            dot_git.display()
        ));
    };
    if !git_dir.is_dir() {
        return Err(format!(
            "resolved Git directory is not a directory: {}",
            git_dir.display()
        ));
    }

    let commondir_file = git_dir.join("commondir");
    let commondir_identity = git_input_identity(&commondir_file).ok();
    let common_dir = match std::fs::read_to_string(&commondir_file) {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Err(format!(
                    "Git commondir pointer is empty: {}",
                    commondir_file.display()
                ));
            }
            let value = PathBuf::from(value);
            let value = if value.is_absolute() {
                value
            } else {
                git_dir.join(value)
            };
            value.canonicalize().map_err(|error| {
                format!(
                    "failed to resolve Git commondir {}: {error}",
                    value.display()
                )
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
        Err(error) => {
            return Err(format!(
                "failed to read Git commondir {}: {error}",
                commondir_file.display()
            ));
        }
    };
    if !common_dir.is_dir() {
        return Err(format!(
            "resolved Git commondir is not a directory: {}",
            common_dir.display()
        ));
    }

    let mut inputs = git_policy_inputs(project_root, &git_dir, &common_dir, environment);
    if dot_git_identity != git_input_identity(&dot_git).ok()
        || commondir_identity != git_input_identity(&commondir_file).ok()
    {
        inputs = None;
    }
    let (hooks_input, hooks) = resolve_hooks_path(project_root, &common_dir)?;
    let mut metadata_allow = Vec::new();
    let mut configured_paths = Vec::new();
    for key in if worker {
        &[
            "core.hooksPath",
            "mason.originalHooksPath",
            "core.excludesFile",
        ][..]
    } else {
        &[]
    } {
        if let Some((input, path)) = configured_git_path(project_root, key, &mut metadata_allow)? {
            if let Some(inputs) = &mut inputs {
                inputs.resolved_paths.push((input, path.clone()));
            }
            configured_paths.push((key.to_string(), path));
        }
    }
    // Re-resolve canonical paths on hits so creation of, or changes to, symlinks
    // in hooks/excludes paths cannot leave a deny pointing at the old target.
    if let Some(inputs) = &mut inputs {
        inputs.resolved_paths.push((hooks_input, hooks.clone()));
    }
    if !metadata_allow.is_empty() {
        inputs = None;
    }
    let linked = (git_dir != common_dir).then(|| (git_dir.clone(), common_dir.clone()));
    let read_roots = {
        let mut read_roots = vec![git_dir, common_dir];
        read_roots.sort_unstable();
        read_roots.dedup();
        read_roots
    };
    Ok((
        GitPolicy {
            read_roots,
            hooks: vec![hooks],
            linked,
            configured_paths,
            metadata_allow,
        },
        inputs,
    ))
}

#[cfg(unix)]
fn resolve_hooks_path(
    project_root: &Path,
    common_dir: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    // Without runnable git nothing can read core.hooksPath, and nothing can
    // run a hook either; deny the default hooks directory, which is what an
    // unset core.hooksPath resolves to.
    if !crate::developer_tools::git_usable() {
        return resolve_git_policy_path(common_dir.join("hooks"), "Git hooks");
    }
    let configured = git_policy_output(project_root, &["config", "--path", "core.hooksPath"])
        .map_err(|error| {
            format!(
                "failed to query core.hooksPath for {}: {error}",
                project_root.display()
            )
        })?;
    if configured.status.success() {
        let configured = String::from_utf8(configured.stdout).map_err(|error| {
            format!(
                "core.hooksPath for {} is not UTF-8: {error}",
                project_root.display()
            )
        })?;
        if configured.trim().is_empty() {
            return Err(format!(
                "core.hooksPath for {} is empty",
                project_root.display()
            ));
        }
        let resolved = git_policy_output(
            project_root,
            &["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
        )
        .map_err(|error| {
            format!(
                "failed to resolve core.hooksPath for {}: {error}",
                project_root.display()
            )
        })?;
        if !resolved.status.success() {
            return Err(format!(
                "git could not resolve core.hooksPath for {}: {}",
                project_root.display(),
                String::from_utf8_lossy(&resolved.stderr).trim()
            ));
        }
        let resolved = String::from_utf8(resolved.stdout).map_err(|error| {
            format!(
                "resolved core.hooksPath for {} is not UTF-8: {error}",
                project_root.display()
            )
        })?;
        let resolved = PathBuf::from(resolved.trim());
        if !resolved.is_absolute() {
            return Err(format!(
                "git returned a non-absolute core.hooksPath for {}: {}",
                project_root.display(),
                resolved.display()
            ));
        }
        return resolve_git_policy_path(resolved, "core.hooksPath");
    }

    if configured.status.code() != Some(1) || !configured.stdout.is_empty() {
        return Err(format!(
            "git could not query core.hooksPath for {}: {}",
            project_root.display(),
            String::from_utf8_lossy(&configured.stderr).trim()
        ));
    }
    resolve_git_policy_path(common_dir.join("hooks"), "Git hooks")
}

/// Resolve operator-owned Git path settings before any command-supplied
/// environment is applied. `--path` expands Git's home-relative spelling.
#[cfg(unix)]
fn configured_git_path(
    project_root: &Path,
    key: &str,
    metadata_allow: &mut Vec<PathBuf>,
) -> Result<Option<(PathBuf, PathBuf)>, String> {
    if !project_root.join(".git").exists() || !crate::developer_tools::git_usable() {
        return Ok(None);
    }
    let worktree_config = git_policy_output(
        project_root,
        &["config", "--local", "--bool", "extensions.worktreeConfig"],
    )
    .map_err(|error| format!("failed to query worktree config extension: {error}"))?;
    let worktree_config = worktree_config.status.success() && worktree_config.stdout == b"true\n";
    // AFT injects core.hooksPath through command-scope GIT_CONFIG_* settings.
    // Its hooks then query repository scopes to run the configured policy.
    // Resolve those same scopes so the injected path cannot hide that policy.
    for scope in ["--worktree", "--local", "--global", "--system"] {
        if scope == "--worktree" && !worktree_config {
            continue;
        }
        let output = git_policy_output(project_root, &["config", scope, "--path", "--get", key])
            .map_err(|error| format!("failed to query {key}: {error}"))?;
        if output.status.code() == Some(1) && output.stdout.is_empty() {
            continue;
        }
        if !output.status.success() {
            return Err(format!(
                "failed to query {key}: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let value = String::from_utf8(output.stdout)
            .map_err(|error| format!("{key} is not UTF-8: {error}"))?;
        let path = PathBuf::from(value.trim());
        if !path.as_os_str().is_empty() {
            let path = if path.is_absolute() {
                path
            } else {
                project_root.join(path)
            };
            for ancestor in path.ancestors() {
                if std::fs::symlink_metadata(ancestor)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    metadata_allow.push(ancestor.to_path_buf());
                }
            }
            return resolve_git_policy_path(path, key).map(Some);
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn resolve_git_policy_path(path: PathBuf, field: &str) -> Result<(PathBuf, PathBuf), String> {
    canonicalize_policy_path(path.clone(), field).map(|resolved| (path, resolved))
}

#[cfg(unix)]
fn canonicalize_policy_path(path: PathBuf, field: &str) -> Result<PathBuf, String> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut ancestor = path.clone();
            let mut tail = Vec::new();
            loop {
                match ancestor.canonicalize() {
                    Ok(mut canonical) => {
                        for component in tail.iter().rev() {
                            canonical.push(component);
                        }
                        return Ok(canonical);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let component =
                            ancestor.file_name().map(ToOwned::to_owned).ok_or_else(|| {
                                format!(
                                    "failed to canonicalize {field} path {}: {error}",
                                    path.display()
                                )
                            })?;
                        tail.push(component);
                        if !ancestor.pop() {
                            return Err(format!(
                                "failed to canonicalize {field} path {}: {error}",
                                path.display()
                            ));
                        }
                    }
                    Err(error) => {
                        return Err(format!(
                            "failed to canonicalize {field} path {}: {error}",
                            path.display()
                        ));
                    }
                }
            }
        }
        Err(error) => Err(format!(
            "failed to canonicalize {field} path {}: {error}",
            path.display()
        )),
    }
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone)]
struct IntendedReadGrant {
    path: PathBuf,
    force_children: bool,
    mandatory: bool,
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListedReadChild {
    path: PathBuf,
    is_dir: bool,
}

#[cfg(any(test, target_os = "linux"))]
trait ReadDirectoryLister {
    fn children(&mut self, parent: &Path) -> std::io::Result<Vec<ListedReadChild>>;
}

#[cfg(target_os = "linux")]
fn build_linux_profile(
    profile: SandboxProfile,
    home: &Path,
    git_read_roots: &[PathBuf],
) -> Result<SandboxProfile, String> {
    let mut intended = Vec::new();
    for path in [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib32",
        "/lib64",
        "/etc",
        "/opt",
        "/run",
        "/proc",
        "/sys/devices/system/cpu",
        "/sys/fs/cgroup",
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
        "/dev/ptmx",
        "/dev/pts",
        "/dev/fd",
        "/dev/stdin",
        "/dev/stdout",
        "/dev/stderr",
    ] {
        if let Some(path) = canonicalize_existing_static(Path::new(path))? {
            intended.push(IntendedReadGrant {
                path,
                force_children: false,
                mandatory: true,
            });
        }
    }
    if let Some(path) = canonicalize_existing_static(Path::new("/var"))? {
        intended.push(IntendedReadGrant {
            path,
            // Enumerating /var avoids following the /var/run symlink back into /run.
            force_children: true,
            mandatory: true,
        });
    }

    compute_linux_profile(
        profile,
        home,
        git_read_roots,
        &intended,
        &mut SecureReadDirectoryLister,
    )
}

/// Compute the exact launcher policy from canonical paths and an injected
/// directory inventory. Only the wrapper above discovers host filesystem paths;
/// this policy calculation also runs in tests on hosts without Landlock.
#[cfg(any(test, target_os = "linux"))]
fn compute_linux_profile(
    mut profile: SandboxProfile,
    home: &Path,
    git_read_roots: &[PathBuf],
    base_read_grants: &[IntendedReadGrant],
    lister: &mut impl ReadDirectoryLister,
) -> Result<SandboxProfile, String> {
    validate_mandatory_floor_overlap(profile.write_allow_roots(), &profile.write_deny)?;
    profile
        .data_policy
        .validate_grants(
            profile.write_allow_roots(),
            &profile.data_policy.write_allow,
            &profile.writable_files,
        )
        .map_err(|error| error.to_string())?;

    let mut intended = base_read_grants.to_vec();

    intended.push(IntendedReadGrant {
        path: home.to_path_buf(),
        force_children: true,
        mandatory: false,
    });
    intended.extend(
        profile
            .write_allow_roots()
            .into_iter()
            .chain(
                profile
                    .writable_files
                    .iter()
                    .map(PathBuf::as_path)
                    .filter(|file| file.is_file()),
            )
            .map(|path| IntendedReadGrant {
                path: path.to_path_buf(),
                force_children: false,
                mandatory: false,
            }),
    );
    intended.extend(
        git_read_roots
            .iter()
            .cloned()
            .map(|path| IntendedReadGrant {
                path,
                force_children: false,
                mandatory: false,
            }),
    );

    let split_denies = profile
        .read_deny
        .iter()
        .chain(&profile.data_policy.deny)
        .cloned()
        .collect::<Vec<_>>();
    let (mut grants, mut listed) =
        split_read_grants_with_inventory(&intended, &split_denies, lister)?;
    // Reintroduce only explicit private-tree exceptions, still split around
    // ordinary denies (including the connection file and repository hooks).
    // A nested repository-hooks deny means reads are granted per existing
    // project child. Files newly created at the project root after this launch
    // cannot be read within the same sandboxed command; writes remain allowed.
    let exceptions = profile
        .data_policy
        .read_allow
        .iter()
        .filter(|path| !profile.writable_files.contains(path) || path.is_file())
        .map(|path| IntendedReadGrant {
            path: path.clone(),
            force_children: false,
            mandatory: false,
        })
        .collect::<Vec<_>>();
    let (exception_grants, exception_listed) =
        split_read_grants_with_inventory(&exceptions, &profile.read_deny, lister)?;
    // Direct and explicitly supplied grants remain required even if the same
    // path was also discovered while splitting a different read root.
    let required = grants
        .iter()
        .filter(|path| !listed.contains(path))
        .chain(
            exception_grants
                .iter()
                .filter(|path| !exception_listed.contains(path)),
        )
        .chain(&profile.read_allow)
        .cloned()
        .collect::<BTreeSet<_>>();
    grants.extend(exception_grants);
    grants.extend(profile.read_allow);
    listed.extend(exception_listed);
    listed.retain(|path| !required.contains(path));
    listed.sort_unstable();
    listed.dedup();
    grants.sort_unstable();
    grants.dedup();
    profile
        .data_policy
        .validate_grants(
            grants.iter().map(PathBuf::as_path),
            &profile.data_policy.read_allow,
            &profile.writable_files,
        )
        .map_err(|error| error.to_string())?;
    profile.read_allow = grants;
    profile.read_allow_listed = listed;
    Ok(profile)
}

#[cfg(target_os = "linux")]
fn add_linux_payload_read_grants(
    profile: &mut SandboxProfile,
    payload_grants: &[PathBuf],
) -> Result<(), String> {
    let intended = payload_grants
        .iter()
        .map(|path| {
            let path = path.canonicalize().map_err(|error| {
                format!(
                    "mandatory payload read path is unavailable: {}: {error}",
                    path.display()
                )
            })?;
            Ok(IntendedReadGrant {
                path,
                force_children: false,
                mandatory: true,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut lister = SecureReadDirectoryLister;
    let payload_grants = split_read_grants(&intended, &profile.read_deny, &mut lister)?;
    // Payload files are exact, daemon-verified objects, not directory grants.
    profile
        .data_policy
        .read_allow
        .extend(payload_grants.iter().cloned());

    let mut final_read_allow = profile.read_allow.clone();
    profile
        .read_allow_listed
        .retain(|path| !payload_grants.contains(path));
    final_read_allow.extend(payload_grants);
    final_read_allow.sort_unstable();
    final_read_allow.dedup();
    validate_final_read_rules(&final_read_allow, &profile.read_deny)?;
    profile
        .data_policy
        .validate_grants(
            final_read_allow.iter().map(PathBuf::as_path),
            &profile.data_policy.read_allow,
            &profile.writable_files,
        )
        .map_err(|error| error.to_string())?;
    assert!(
        validate_final_read_rules(&final_read_allow, &profile.read_deny).is_ok(),
        "final Landlock read grants overlap a denied path after adding payload files"
    );
    profile.read_allow = final_read_allow;
    Ok(())
}

#[cfg(target_os = "linux")]
fn canonicalize_existing_static(path: &Path) -> Result<Option<PathBuf>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => match path.canonicalize() {
            Ok(path) => Ok(Some(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!(
                "failed to canonicalize static read root {}: {error}",
                path.display()
            )),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "failed to inspect static read root {}: {error}",
            path.display()
        )),
    }
}

#[cfg(any(test, target_os = "linux"))]
fn split_read_grants(
    intended: &[IntendedReadGrant],
    denies: &[PathBuf],
    lister: &mut impl ReadDirectoryLister,
) -> Result<Vec<PathBuf>, String> {
    split_read_grants_with_inventory(intended, denies, lister).map(|(paths, _)| paths)
}

#[cfg(any(test, target_os = "linux"))]
fn split_read_grants_with_inventory(
    intended: &[IntendedReadGrant],
    denies: &[PathBuf],
    lister: &mut impl ReadDirectoryLister,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let mut emitted = BTreeMap::new();
    for grant in intended {
        split_read_grant(grant, denies, lister, false, &mut emitted)?;
    }
    let paths = emitted.keys().cloned().collect::<Vec<_>>();
    let listed = emitted
        .into_iter()
        .filter_map(|(path, listed)| listed.then_some(path))
        .collect();
    validate_final_read_rules(&paths, denies)?;
    Ok((paths, listed))
}

#[cfg(any(test, target_os = "linux"))]
fn split_read_grant(
    grant: &IntendedReadGrant,
    denies: &[PathBuf],
    lister: &mut impl ReadDirectoryLister,
    from_listing: bool,
    emitted: &mut BTreeMap<PathBuf, bool>,
) -> Result<(), String> {
    if let Some(deny) = denies
        .iter()
        .find(|deny| grant.path == **deny || grant.path.starts_with(deny))
    {
        if grant.mandatory {
            return Err(format!(
                "sandbox_unavailable: mandatory read root {} is denied by {}",
                grant.path.display(),
                deny.display()
            ));
        }
        return Ok(());
    }

    let contains_deny = denies.iter().any(|deny| deny.starts_with(&grant.path));
    if !grant.force_children && !contains_deny {
        emitted
            .entry(grant.path.clone())
            .and_modify(|listed| *listed &= from_listing)
            .or_insert(from_listing);
        return Ok(());
    }

    let children = match lister.children(&grant.path) {
        Ok(children) => children,
        Err(error) if from_listing && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "sandbox_unavailable: cannot split read root {}: {error}",
                grant.path.display()
            ))
        }
    };
    for child in children {
        // An exact denied file is simply omitted by the recursive call. Only a
        // strict descendant deny would require traversing this child as a directory.
        let child_contains_deny = denies
            .iter()
            .any(|deny| deny != &child.path && deny.starts_with(&child.path));
        if child_contains_deny && !child.is_dir {
            return Err(format!(
                "sandbox_unavailable: deny chain crosses non-directory path {}",
                child.path.display()
            ));
        }
        split_read_grant(
            &IntendedReadGrant {
                path: child.path,
                force_children: false,
                mandatory: false,
            },
            denies,
            lister,
            true,
            emitted,
        )?;
    }
    Ok(())
}

#[cfg(any(test, target_os = "linux"))]
fn validate_mandatory_floor_overlap<'a>(
    writable_roots: impl IntoIterator<Item = &'a Path>,
    mandatory_floor: &[PathBuf],
) -> Result<(), String> {
    for writable in writable_roots {
        for secret in mandatory_floor {
            if paths_overlap(writable, secret) {
                return Err(format!(
                    "writable root {} overlaps mandatory secret floor {}",
                    writable.display(),
                    secret.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(any(test, target_os = "linux"))]
fn validate_final_read_rules(read_allow: &[PathBuf], denies: &[PathBuf]) -> Result<(), String> {
    for grant in read_allow {
        for deny in denies {
            if paths_overlap(grant, deny) {
                return Err(format!(
                    "sandbox_unavailable: final read grant {} overlaps denied path {}",
                    grant.display(),
                    deny.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(any(test, target_os = "linux"))]
fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

#[cfg(target_os = "linux")]
struct SecureReadDirectoryLister;

#[cfg(target_os = "linux")]
impl ReadDirectoryLister for SecureReadDirectoryLister {
    fn children(&mut self, parent: &Path) -> std::io::Result<Vec<ListedReadChild>> {
        let parent_fd = open_absolute_no_symlinks(parent, true)?;
        let readable_fd =
            open_directory_for_enumeration(parent_fd.as_raw_fd()).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "failed to open directory for enumeration {}: {error}",
                        parent.display()
                    ),
                )
            })?;
        let duplicate = unsafe { libc::dup(readable_fd.as_raw_fd()) };
        if duplicate < 0 {
            let error = std::io::Error::last_os_error();
            return Err(std::io::Error::new(
                error.kind(),
                format!(
                    "failed to duplicate directory fd for {}: {}",
                    parent.display(),
                    error
                ),
            ));
        }
        let directory = unsafe { libc::fdopendir(duplicate) };
        if directory.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(duplicate) };
            return Err(std::io::Error::new(
                error.kind(),
                format!(
                    "failed to enumerate directory {}: {error}",
                    parent.display()
                ),
            ));
        }

        let result = (|| {
            let mut children = Vec::new();
            loop {
                unsafe { *libc::__errno_location() = 0 };
                let entry = unsafe { libc::readdir(directory) };
                if entry.is_null() {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(0) {
                        break;
                    }
                    return Err(std::io::Error::new(
                        error.kind(),
                        format!("failed while enumerating {}: {error}", parent.display()),
                    ));
                }
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                let name = OsStr::from_bytes(name);
                let diagnostic_path = parent.join(name);
                let diagnostic = match std::fs::symlink_metadata(&diagnostic_path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(std::io::Error::new(
                            error.kind(),
                            format!(
                                "directory entry changed while inspecting {}: {error}",
                                diagnostic_path.display()
                            ),
                        ))
                    }
                };
                if diagnostic.file_type().is_symlink() {
                    continue;
                }

                let child_fd = match open_child_no_symlinks(parent_fd.as_raw_fd(), name) {
                    Ok(fd) => fd,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(std::io::Error::new(
                            error.kind(),
                            format!(
                                "directory entry changed while opening {}: {error}",
                                diagnostic_path.display()
                            ),
                        ))
                    }
                };
                let metadata = fstat_fd(&child_fd).map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!(
                            "failed to inspect opened directory entry {}: {error}",
                            diagnostic_path.display()
                        ),
                    )
                })?;
                if metadata.st_mode & libc::S_IFMT == libc::S_IFLNK {
                    continue;
                }
                children.push(ListedReadChild {
                    path: diagnostic_path,
                    is_dir: metadata.st_mode & libc::S_IFMT == libc::S_IFDIR,
                });
            }
            children.sort_unstable_by(|left, right| left.path.cmp(&right.path));
            Ok(children)
        })();
        unsafe { libc::closedir(directory) };
        result
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[cfg(target_os = "linux")]
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
#[cfg(target_os = "linux")]
const RESOLVE_BENEATH: u64 = 0x08;

#[cfg(target_os = "linux")]
fn open_absolute_no_symlinks(path: &Path, directory: bool) -> std::io::Result<OwnedFd> {
    if !path.is_absolute() {
        return Err(std::io::Error::other(format!(
            "path is not absolute: {}",
            path.display()
        )));
    }
    let root = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root < 0 {
        let error = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            error.kind(),
            format!("failed to open filesystem root: {}", error),
        ));
    }
    let root = unsafe { OwnedFd::from_raw_fd(root) };
    let components = normalized_relative_components(path).map_err(std::io::Error::other)?;
    if components.is_empty() {
        return Ok(root);
    }

    let relative = components
        .iter()
        .fold(PathBuf::new(), |path, component| path.join(component));
    let relative = CString::new(relative.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other(format!("path contains NUL: {}", path.display())))?;
    let flags = libc::O_PATH | libc::O_CLOEXEC | if directory { libc::O_DIRECTORY } else { 0 };
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let opened = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            relative.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        ) as libc::c_int
    };
    if opened >= 0 {
        return Ok(unsafe { OwnedFd::from_raw_fd(opened) });
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOSYS) {
        return Err(std::io::Error::new(
            error.kind(),
            format!("secure open failed for {}: {error}", path.display()),
        ));
    }

    let mut current = root;
    for (index, component) in components.iter().enumerate() {
        let component = CString::new(component.as_bytes())
            .map_err(|_| std::io::Error::other(format!("path contains NUL: {}", path.display())))?;
        let last = index + 1 == components.len();
        let mut flags = libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        if !last || directory {
            flags |= libc::O_DIRECTORY;
        }
        let opened = unsafe { libc::openat(current.as_raw_fd(), component.as_ptr(), flags) };
        if opened < 0 {
            let error = std::io::Error::last_os_error();
            return Err(std::io::Error::new(
                error.kind(),
                format!(
                    "component-wise secure open failed for {}: {}",
                    path.display(),
                    error
                ),
            ));
        }
        let opened = unsafe { OwnedFd::from_raw_fd(opened) };
        let metadata = fstat_fd(&opened).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("failed to inspect {}: {error}", path.display()),
            )
        })?;
        if metadata.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err(std::io::Error::other(format!(
                "secure open encountered a symlink: {}",
                path.display()
            )));
        }
        current = opened;
    }
    Ok(current)
}

#[cfg(target_os = "linux")]
fn open_directory_for_enumeration(parent_fd: i32) -> Result<OwnedFd, std::io::Error> {
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let opened = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent_fd,
            c".".as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        ) as libc::c_int
    };
    if opened >= 0 {
        return Ok(unsafe { OwnedFd::from_raw_fd(opened) });
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOSYS) {
        return Err(error);
    }

    let opened = unsafe {
        libc::openat(
            parent_fd,
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if opened < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(opened) })
    }
}

#[cfg(target_os = "linux")]
fn open_child_no_symlinks(parent_fd: i32, name: &OsStr) -> Result<OwnedFd, std::io::Error> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let opened = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent_fd,
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        ) as libc::c_int
    };
    if opened >= 0 {
        return Ok(unsafe { OwnedFd::from_raw_fd(opened) });
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOSYS) {
        return Err(error);
    }

    let opened = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if opened < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let opened = unsafe { OwnedFd::from_raw_fd(opened) };
    let metadata = fstat_fd(&opened)?;
    if metadata.st_mode & libc::S_IFMT == libc::S_IFLNK {
        return Err(std::io::Error::from_raw_os_error(libc::ELOOP));
    }
    Ok(opened)
}

#[cfg(target_os = "linux")]
fn fstat_fd(fd: &OwnedFd) -> Result<libc::stat, std::io::Error> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), metadata.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { metadata.assume_init() })
}

#[cfg(target_os = "linux")]
fn normalized_relative_components(path: &Path) -> Result<Vec<&OsStr>, String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(component) => components.push(component),
            _ => {
                return Err(format!(
                    "path is not normalized for secure open: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(components)
}

#[cfg(unix)]
pub(crate) fn create_task_temp_dir(task_bundle_dir: &Path) -> Result<PathBuf, String> {
    task_temp::create(task_bundle_dir)
        .map_err(|error| format!("failed to allocate task temp directory: {error}"))
}

#[cfg(target_os = "macos")]
fn install_worker_mktemp(temp_dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let bin = temp_dir.join(".worker-bin");
    std::fs::create_dir(&bin)
        .map_err(|error| format!("failed to create task temporary helpers: {error}"))?;
    let shim = bin.join("mktemp");
    // Apple mktemp prefers confstr(_CS_DARWIN_USER_TEMP_DIR) over TMPDIR
    // when no template is supplied. An explicit template keeps that default
    // operation inside the task; user-supplied templates retain their meaning.
    std::fs::write(
        &shim,
        r#"#!/bin/sh
for argument do
    case "$argument" in
        -*) flags=${argument#-}
            case "$flags" in ''|*[!dqu]*) exec /usr/bin/mktemp "$@" ;; esac ;;
        *) exec /usr/bin/mktemp "$@" ;;
    esac
done
exec /usr/bin/mktemp "$@" "${TMPDIR:?}/tmp.XXXXXXXX"
"#,
    )
    .map_err(|error| format!("failed to write task temporary helper: {error}"))?;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("failed to make task temporary helper executable: {error}"))
}

pub(crate) fn is_managed_task_temp_dir(path: &Path) -> bool {
    #[cfg(unix)]
    if task_temp::is_managed(path) {
        return true;
    }
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with(".sandbox-tmp-"))
}

pub(crate) fn cleanup_task_temp_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if task_temp::is_managed(path) {
        return task_temp::remove(path);
    }
    // Older persisted tasks kept their temporary files inside the IO bundle.
    if is_managed_task_temp_dir(path) {
        return match std::fs::remove_dir_all(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        };
    }
    Ok(())
}

fn note_test_observation(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    requested_tier: RequestedSandboxTier,
    task_kind: SandboxTaskKind,
) {
    let Some(observations) = TEST_OBSERVATIONS.get() else {
        return;
    };
    let Some(project_root) = ctx.config().project_root.clone() else {
        return;
    };
    let project_root = observation_key(&project_root);
    if let Some(project) = observations
        .lock()
        .expect("sandbox spawn test observation mutex poisoned")
        .get_mut(&project_root)
    {
        project.push(SandboxSpawnObservation {
            principal: principal.clone(),
            requested_tier,
            task_kind,
        });
    }
}

/// Start recording resolver calls for one project root.
#[doc(hidden)]
pub fn install_sandbox_spawn_test_seam(project_root: PathBuf) {
    TEST_OBSERVATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("sandbox spawn test observation mutex poisoned")
        .insert(observation_key(&project_root), Vec::new());
}

/// Snapshot resolver calls recorded for one project root.
#[doc(hidden)]
pub fn sandbox_spawn_test_observations(project_root: &Path) -> Vec<SandboxSpawnObservation> {
    TEST_OBSERVATIONS
        .get()
        .and_then(|observations| {
            observations
                .lock()
                .expect("sandbox spawn test observation mutex poisoned")
                .get(&observation_key(project_root))
                .cloned()
        })
        .unwrap_or_default()
}

/// Remove one project-root resolver test seam.
#[doc(hidden)]
pub fn clear_sandbox_spawn_test_seam(project_root: &Path) {
    if let Some(observations) = TEST_OBSERVATIONS.get() {
        observations
            .lock()
            .expect("sandbox spawn test observation mutex poisoned")
            .remove(&observation_key(project_root));
    }
}

fn observation_key(project_root: &Path) -> PathBuf {
    std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf())
}

#[cfg(test)]
pub(crate) fn with_spawn_plan_for_test<R>(plan: SpawnPlan, run: impl FnOnce() -> R) -> R {
    struct PlanScope(Option<SpawnPlan>);

    impl Drop for PlanScope {
        fn drop(&mut self) {
            TEST_PLAN_OVERRIDE.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }

    let previous = TEST_PLAN_OVERRIDE.with(|slot| slot.replace(Some(plan)));
    let _scope = PlanScope(previous);
    run()
}

/// Build a detached `Command` while enforcing the required launch plan.
///
/// Windows detached spawns route through the shell-candidate ladder, which
/// enforces the plan inline, so this helper is Unix-only.
#[cfg(unix)]
pub(crate) const CHILD_EXIT_FD: RawFd = 3;
#[cfg(unix)]
pub(crate) const CHILD_FAILURE_FD: RawFd = 4;

#[cfg(unix)]
pub(crate) fn apply_marker_fd_allowlist(
    command: &mut Command,
    exit_fd: RawFd,
    failure_fd: RawFd,
) -> Result<(RawFd, RawFd), String> {
    use std::os::unix::process::CommandExt;

    let fd_limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let fd_limit = if fd_limit > 0 {
        (fd_limit as RawFd).min(65_536)
    } else {
        1_024
    };
    unsafe {
        command.pre_exec(move || {
            let exit_copy = libc::fcntl(exit_fd, libc::F_DUPFD_CLOEXEC, 6);
            if exit_copy < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let failure_copy = libc::fcntl(failure_fd, libc::F_DUPFD_CLOEXEC, 6);
            if failure_copy < 0 {
                let error = std::io::Error::last_os_error();
                libc::close(exit_copy);
                return Err(error);
            }
            if libc::dup2(exit_copy, CHILD_EXIT_FD) < 0
                || libc::dup2(failure_copy, CHILD_FAILURE_FD) < 0
            {
                let error = std::io::Error::last_os_error();
                libc::close(exit_copy);
                libc::close(failure_copy);
                return Err(error);
            }
            libc::close(exit_copy);
            libc::close(failure_copy);
            for fd in (CHILD_FAILURE_FD + 1)..fd_limit {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
            Ok(())
        });
    }
    Ok((CHILD_EXIT_FD, CHILD_FAILURE_FD))
}

#[cfg(unix)]
pub(crate) fn detached_command_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
    exit_fd: RawFd,
    failure_fd: RawFd,
) -> Result<(Command, Option<File>), String> {
    let (program, args, profile_handle) = command_argv_for_plan(
        plan,
        program,
        args,
        task_marker,
        Some((exit_fd, failure_fd)),
    )?;
    let mut command = crate::effective_path::new_command(program);
    command.args(args);
    crate::bash_background::process::start_new_session(&mut command);
    Ok((command, profile_handle))
}

fn isolated_environment_for_plan(
    plan: &SpawnPlan,
    request_environment: &HashMap<String, String>,
) -> Option<ChildEnvironment> {
    #[cfg(unix)]
    if !matches!(plan.policy(), SpawnPlan::Unsandboxed) {
        if let Some(task) = plan.prepared_task() {
            return Some(task.environment().clone());
        }
    }
    match plan.policy() {
        SpawnPlan::Host { environment, .. } => Some(environment.clone()),
        SpawnPlan::Launcher { profile, .. } => {
            let mut environment =
                sandboxed_child_environment(request_environment, &profile.temp_dir);
            #[cfg(target_os = "macos")]
            if profile.temp_dir.join(".worker-bin/mktemp").is_file() {
                let mut paths = vec![profile.temp_dir.join(".worker-bin")];
                if let Some(path) = environment.get(OsStr::new("PATH")) {
                    paths.extend(std::env::split_paths(path));
                }
                if let Ok(path) = std::env::join_paths(paths) {
                    environment.insert(OsString::from("PATH"), path);
                }
            }
            // RUSTC_WRAPPER was emptied to disable out-of-sandbox compile servers.
            // Restore only the direct launcher whose executable this profile
            // resolved from operator config and explicitly made read-only.
            if let Some(wrapper) = profile.data_policy.read_allow.iter().find(|path| {
                path.file_name() == Some(OsStr::new("prefrontal-build-slot"))
                    && profile.write_deny.contains(path)
            }) {
                for key in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
                    environment.insert(OsString::from(key), wrapper.as_os_str().to_os_string());
                }
            }
            Some(environment)
        }
        SpawnPlan::Unsandboxed | SpawnPlan::Refused { .. } => None,
        #[cfg(unix)]
        SpawnPlan::Prepared { .. } => unreachable!("policy() unwraps prepared plans"),
    }
}

fn sandboxed_child_environment(
    request_environment: &HashMap<String, String>,
    temp_dir: &Path,
) -> ChildEnvironment {
    let mut environment = std::env::vars_os()
        .filter(|(key, _)| sandbox_base_environment_key(key))
        .collect::<ChildEnvironment>();
    // PATH must be AFT's enriched value rather than the daemon's original
    // value, which can omit package-manager and user tool locations.
    environment.insert(
        OsString::from("PATH"),
        crate::effective_path::effective_path().to_os_string(),
    );
    for (key, value) in request_environment {
        // A request-supplied environment reaches the OUTER launcher / `/bin/sh`
        // supervisor, which exec before Landlock/Seatbelt is installed. A
        // dynamic-loader or shell/interpreter startup hook here would execute
        // code OUTSIDE confinement, so those keys are dropped even though they
        // arrived through the (otherwise honored) request environment.
        if is_preexec_hijack_env_key(key.as_str())
            || key == crate::privacy_spawn::CONTROL_ENV
            || crate::agent_child_env::is_subc_credential_env_key(key)
            || key == crate::gh_shim_ticket::GH_SHIM_TICKET_ENV
        {
            continue;
        }
        environment.insert(OsString::from(key), OsString::from(value));
    }
    for key in ["TMPDIR", "TEMP", "TMP"] {
        environment.insert(OsString::from(key), temp_dir.as_os_str().to_os_string());
    }
    environment
}

fn sandbox_base_environment_key(key: &OsStr) -> bool {
    key.to_str().is_some_and(|key| {
        matches!(key, "HOME" | "USER" | "LOGNAME" | "SHELL" | "TERM" | "LANG")
            || key.starts_with("LC_")
    })
}

/// Environment keys that make a process load a library or run code during its
/// own startup — before a native sandbox is installed in the launcher chain.
/// These are refused from the request environment regardless of value so an
/// injected `LD_PRELOAD` / `DYLD_INSERT_LIBRARIES` / `BASH_ENV` cannot execute
/// outside confinement. Matching is case-sensitive; POSIX environment names are
/// case-sensitive and the loaders only honor the exact upper-case spellings.
fn is_preexec_hijack_env_key(key: &str) -> bool {
    // Dynamic-loader families (glibc/musl `LD_*`, macdyld `DYLD_*`): the whole
    // prefix executes/loads at exec time, so block the family, not a fixed set.
    if key.starts_with("LD_") || key.starts_with("DYLD_") || key.starts_with("BASH_FUNC_") {
        return true;
    }
    matches!(
        key,
        // POSIX/bash shell startup + trace hooks (the supervisor is `/bin/sh`).
        "BASH_ENV"
            | "ENV"
            | "SHELLOPTS"
            | "BASHOPTS"
            | "PROMPT_COMMAND"
            | "PS4"
            | "IFS"
            // Interpreter auto-run / library-injection hooks.
            | "PYTHONSTARTUP"
            | "PYTHONPATH"
            | "PYTHONHOME"
            | "PERL5OPT"
            | "PERL5LIB"
            | "PERLLIB"
            | "PERL5DB"
            | "RUBYOPT"
            | "RUBYLIB"
            | "NODE_OPTIONS"
            // glibc auxiliary loader hooks.
            | "GCONV_PATH"
            | "LOCPATH"
            | "NLSPATH"
            | "HOSTALIASES"
            | "RESOLV_HOST_CONF"
    )
}

#[cfg(unix)]
pub(crate) fn approved_environment_for_plan(
    plan: &SpawnPlan,
    request_environment: &HashMap<String, String>,
) -> ChildEnvironment {
    isolated_environment_for_plan(plan, request_environment)
        .unwrap_or_else(|| approved_payload_environment(request_environment, &std::env::temp_dir()))
}

#[cfg(unix)]
pub(crate) fn apply_sandbox_environment(
    plan: &SpawnPlan,
    command: &mut Command,
    request_environment: &HashMap<String, String>,
) {
    if let Some(environment) = isolated_environment_for_plan(plan, request_environment) {
        // Host snapshots and native-launcher allowlists are complete child
        // environments. Clear the daemon environment first so loader hooks,
        // shell startup hooks, and cloud credentials cannot leak around them.
        command.env_clear().envs(environment);
        if let Some(ticket) = memory_only_ticket(request_environment) {
            command.env(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV, ticket);
        }
    }
}

/// Install after all environment/session/fd adapters and before spawning.
#[cfg(unix)]
pub(crate) fn disclaim_command_for_plan(
    plan: &SpawnPlan,
    command: &mut Command,
    environment: &HashMap<String, String>,
) -> Result<(), String> {
    command.env_remove(crate::privacy_spawn::CONTROL_ENV);
    crate::privacy_spawn::install(
        command,
        crate::privacy_spawn::requested(environment),
        isolated_environment_for_plan(plan, environment).is_some(),
    )
}

/// The gh shim ticket is kept out of the isolated environment above because
/// that environment is written to the task's payload file on disk, and the
/// ticket must exist only in memory. It is added back here, at exec time.
fn memory_only_ticket(request_environment: &HashMap<String, String>) -> Option<&str> {
    request_environment
        .get(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV)
        .map(String::as_str)
}

/// Build a PTY `CommandBuilder` while enforcing the required launch plan.
pub(crate) fn pty_command_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
    workdir: &Path,
    env: &HashMap<String, String>,
) -> Result<(CommandBuilder, Option<File>), String> {
    let (program, args, profile_handle) =
        command_argv_for_plan(plan, program, args, task_marker, None)?;
    let mut command = CommandBuilder::new(program);
    #[cfg(windows)]
    {
        // portable-pty refreshes its base environment from the registry, which
        // can replace a process-local PATH and hide the shell we just selected.
        // Inherit the actual parent block, as std::process::Command does, before
        // applying the launch plan's isolation and request overrides below.
        command.env_clear();
        for (key, value) in std::env::vars_os() {
            command.env(key, value);
        }
    }
    for arg in args {
        command.arg(arg);
    }
    command.cwd(workdir.as_os_str());
    if let Some(environment) = isolated_environment_for_plan(plan, env) {
        command.env_clear();
        for (key, value) in environment {
            command.env(key, value);
        }
        if let Some(ticket) = memory_only_ticket(env) {
            command.env(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV, ticket);
        }
    } else {
        // Sandbox-disabled PTYs retain the historical full inheritance and add
        // only request overrides. An inherited gh shim ticket is removed first:
        // it belongs to whatever started this process, and the request map
        // re-adds the child's own ticket when one was issued.
        command.env_remove(crate::gh_shim_ticket::GH_SHIM_TICKET_ENV);
        for (key, value) in env {
            command.env(key, value);
        }
    }
    crate::agent_child_env::scrub_pty_command(&mut command);
    Ok((command, profile_handle))
}

fn command_argv_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
    marker_fds: Option<(RawFd, RawFd)>,
) -> Result<(OsString, Vec<OsString>, Option<File>), String> {
    match plan.policy() {
        SpawnPlan::Unsandboxed | SpawnPlan::Host { .. } => {
            Ok((program.to_os_string(), args.to_vec(), None))
        }
        SpawnPlan::Refused { code, .. } => Err((*code).to_string()),
        SpawnPlan::Launcher {
            profile,
            launcher_path,
        } => {
            #[cfg(unix)]
            {
                launcher_argv(
                    profile,
                    launcher_path,
                    program,
                    args,
                    task_marker,
                    marker_fds,
                    plan.prepared_task(),
                )
            }
            #[cfg(not(unix))]
            {
                launcher_argv(
                    profile,
                    launcher_path,
                    program,
                    args,
                    task_marker,
                    marker_fds,
                    None,
                )
            }
        }
        #[cfg(unix)]
        SpawnPlan::Prepared { .. } => unreachable!("policy() unwraps prepared plans"),
    }
}

/// Auxiliary read-only CLI lookups inherit the command's exact launch policy,
/// rather than resolving a new principal or silently falling back to the host.
#[cfg(unix)]
pub(crate) fn probe_command_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Result<Command, String> {
    let (program, args, _profile_handle) =
        command_argv_for_plan(plan, program, args, Path::new(""), None)?;
    let mut command = crate::effective_path::new_command(program);
    command.args(args).current_dir(cwd);
    crate::agent_child_env::apply_to_command(&mut command, env);
    apply_sandbox_environment(plan, &mut command, env);
    crate::bash_background::process::start_new_session(&mut command);
    disclaim_command_for_plan(plan, &mut command, env)?;
    Ok(command)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn launcher_argv(
    profile: &SandboxProfile,
    launcher_path: &Path,
    program: &OsStr,
    args: &[OsString],
    _task_marker: &Path,
    marker_fds: Option<(RawFd, RawFd)>,
    _prepared: Option<&PreparedTask>,
) -> Result<(OsString, Vec<OsString>, Option<File>), String> {
    let profile_json = serde_json::to_string(profile)
        .map_err(|error| format!("failed to serialize sandbox profile: {error}"))?;
    // With marker descriptors, a `/bin/sh` supervisor records an exit status
    // when the launcher fails before the target runs. Exit status 78 alone
    // cannot say who failed, because the target may legitimately exit 78
    // after the launcher has exec'd it. The launcher therefore writes the
    // `sandbox_unavailable` failure marker itself, through `--failure-fd`,
    // only when confinement could not be applied; the supervisor reads that
    // marker instead of guessing from the status.
    let Some((exit_fd, failure_fd)) = marker_fds else {
        let mut wrapped = vec![
            OsString::from("sandbox-launch"),
            OsString::from("--profile-json"),
            OsString::from(profile_json),
            OsString::from("--"),
            program.to_os_string(),
        ];
        wrapped.extend_from_slice(args);
        return Ok((launcher_path.as_os_str().to_os_string(), wrapped, None));
    };

    let mut wrapped = vec![
        OsString::from("-c"),
        OsString::from(
            r#"launcher=$1
profile_json=$2
exit_fd=$3
failure_fd=$4
shift 4
"$launcher" sandbox-launch --profile-json "$profile_json" --failure-fd "$failure_fd" -- "$@"
code=$?
if [ "$code" -eq 78 ] && [ -s "/dev/fd/$failure_fd" ]; then
  if [ ! -s "/dev/fd/$exit_fd" ]; then
    printf "%s" "$code" >&"$exit_fd"
  fi
fi
exit "$code""#,
        ),
        OsString::from("aft-sandbox-supervisor"),
        launcher_path.as_os_str().to_os_string(),
        OsString::from(profile_json),
        OsString::from(exit_fd.to_string()),
        OsString::from(failure_fd.to_string()),
        program.to_os_string(),
    ];
    wrapped.extend_from_slice(args);
    Ok((OsString::from("/bin/sh"), wrapped, None))
}

#[cfg(not(unix))]
#[allow(clippy::too_many_arguments)]
fn launcher_argv(
    _profile: &SandboxProfile,
    _launcher_path: &Path,
    _program: &OsStr,
    _args: &[OsString],
    _task_marker: &Path,
    _marker_fds: Option<(i32, i32)>,
    _prepared: Option<&()>,
) -> Result<(OsString, Vec<OsString>, Option<File>), String> {
    Err("sandbox_unavailable".to_string())
}

#[cfg(all(test, windows))]
mod windows_pty_environment_tests {
    use super::*;

    #[test]
    fn windows_pty_inherits_process_search_path_not_registry_environment() {
        let dir = tempfile::tempdir().unwrap();
        let (command, _) = pty_command_for_plan(
            &SpawnPlan::Unsandboxed,
            OsStr::new("pwsh.exe"),
            &[],
            &dir.path().join("metadata.json"),
            dir.path(),
            &HashMap::new(),
        )
        .unwrap();
        for key in ["PATH", "PATHEXT", "TEMP", "USERPROFILE"] {
            assert_eq!(
                command.get_env(key),
                std::env::var_os(key).as_deref(),
                "{key}"
            );
        }
    }

    #[test]
    fn windows_pty_request_environment_overrides_process_environment() {
        let dir = tempfile::tempdir().unwrap();
        let request = HashMap::from([
            ("Path".to_string(), dir.path().display().to_string()),
            ("PATHEXT".to_string(), ".AFT".to_string()),
        ]);
        let (command, _) = pty_command_for_plan(
            &SpawnPlan::Unsandboxed,
            OsStr::new("pwsh.exe"),
            &[],
            &dir.path().join("metadata.json"),
            dir.path(),
            &request,
        )
        .unwrap();
        assert_eq!(command.get_env("PATH"), Some(OsStr::new(&request["Path"])));
        assert_eq!(command.get_env("PATHEXT"), Some(OsStr::new(".AFT")));
    }
}

#[cfg(all(test, unix))]
mod tests {

    use super::*;

    struct SupervisorRun {
        code: Option<i32>,
        exit_marker: String,
        failure_marker: String,
    }

    /// Run the marker-fd supervisor with a fake launcher script, the given
    /// request environment and shell, and return what it recorded.
    fn run_supervisor(
        launcher_body: &str,
        target: &[&str],
        request: &HashMap<String, String>,
        shell: &[&str],
    ) -> SupervisorRun {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let temp = project.join("temp");
        std::fs::create_dir_all(&temp).unwrap();
        let launcher = root.path().join("launcher");
        std::fs::write(&launcher, launcher_body).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        let profile = SandboxProfile::build(
            vec![project],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            temp,
        )
        .unwrap();
        let plan = SpawnPlan::launcher_for_test(profile.clone(), launcher.clone());
        let exit_path = root.path().join("exit");
        let failure_path = root.path().join("failure");
        let exit_file = File::create(&exit_path).unwrap();
        let failure_file = File::create(&failure_path).unwrap();
        let target = target.iter().map(OsString::from).collect::<Vec<_>>();
        let mut command = Command::new(shell[0]);
        command.args(&shell[1..]);
        let fds = apply_marker_fd_allowlist(
            &mut command,
            exit_file.as_raw_fd(),
            failure_file.as_raw_fd(),
        )
        .unwrap();
        let (_, args, _) = launcher_argv(
            &profile,
            &launcher,
            target[0].as_os_str(),
            &target[1..],
            Path::new("unused"),
            Some(fds),
            None,
        )
        .unwrap();
        command.args(args);
        apply_sandbox_environment(&plan, &mut command, request);
        let status = command.status().unwrap();
        SupervisorRun {
            code: status.code(),
            exit_marker: std::fs::read_to_string(exit_path).unwrap(),
            failure_marker: std::fs::read_to_string(failure_path).unwrap(),
        }
    }

    /// The fake launcher's argv is `sandbox-launch --profile-json <json>
    /// --failure-fd <fd> -- <target...>`.
    const FAILING_LAUNCHER: &str = "#!/bin/sh\nprintf %s sandbox_unavailable >&\"$5\"\nexit 78\n";
    const EXEC_LAUNCHER: &str = "#!/bin/sh\nshift 6\nexec \"$@\"\n";

    #[test]
    fn supervisor_reports_unavailable_only_when_the_launcher_says_so() {
        let failed = run_supervisor(
            FAILING_LAUNCHER,
            &["/bin/true"],
            &HashMap::new(),
            &["/bin/sh"],
        );
        assert_eq!(failed.code, Some(78));
        assert_eq!(failed.failure_marker, "sandbox_unavailable");
        assert_eq!(failed.exit_marker, "78");

        // A target that exits 78 after a successful launch is an ordinary
        // command status, not a confinement failure.
        let target = run_supervisor(
            EXEC_LAUNCHER,
            &["/bin/sh", "-c", "exit 78"],
            &HashMap::new(),
            &["/bin/sh"],
        );
        assert_eq!(target.code, Some(78));
        assert_eq!(target.failure_marker, "");
        assert_eq!(target.exit_marker, "");
    }

    #[test]
    fn exported_function_cannot_run_in_outer_supervisor() {
        let sentinel = tempfile::tempdir().unwrap();
        let sentinel = sentinel.path().join("unconfined-function");
        let request = HashMap::from([
            (
                "BASH_FUNC_printf%%".to_string(),
                "() { echo escaped > \"$SENTINEL\"; }".to_string(),
            ),
            (
                "SENTINEL".to_string(),
                sentinel.to_string_lossy().into_owned(),
            ),
        ]);
        // Bash in POSIX mode stands in for a Bash-backed /bin/sh (as on
        // macOS) even on Linux hosts where /bin/sh is dash, which does not
        // import exported functions. The supervisor's own `printf` runs
        // outside confinement after the launcher fails.
        let run = run_supervisor(
            FAILING_LAUNCHER,
            &["/bin/true"],
            &request,
            &["bash", "--posix"],
        );
        assert_eq!(run.code, Some(78));
        assert!(
            !sentinel.exists(),
            "request function ran outside the launcher"
        );
        assert_eq!(run.failure_marker, "sandbox_unavailable");
        assert_eq!(run.exit_marker, "78");
    }

    #[test]
    fn host_plan_clears_inherited_environment_and_applies_snapshot() {
        let environment =
            ChildEnvironment::from([(OsString::from("APPROVED"), OsString::from("snapshot"))]);
        let plan = SpawnPlan::Host {
            shell_path: PathBuf::from("/bin/sh"),
            environment,
        };
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("test -z \"$SHOULD_DISAPPEAR\" && printf %s \"$APPROVED\"")
            .env("SHOULD_DISAPPEAR", "yes");
        apply_sandbox_environment(&plan, &mut command, &HashMap::new());
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"snapshot");
    }

    #[test]
    fn launcher_plan_clears_ambient_environment_and_applies_safe_base() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let temp = root.path().join("temp");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        let profile = SandboxProfile::build(
            vec![project],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp,
        )
        .unwrap();
        let expected_temp = profile.temp_dir.clone();
        let plan = SpawnPlan::launcher_for_test(profile, PathBuf::from("/usr/bin/true"));
        let request_environment = HashMap::from([
            ("TERM".to_string(), "aft-test-term".to_string()),
            ("REQUEST_SENTINEL".to_string(), "request-value".to_string()),
        ]);
        let mut command = Command::new("/usr/bin/env");
        command
            .env("LD_PRELOAD", "/untrusted/loader.so")
            .env("DYLD_INSERT_LIBRARIES", "/untrusted/loader.dylib")
            .env("BASH_ENV", "/untrusted/bash-env")
            .env("AWS_SECRET_ACCESS_KEY", "ambient-secret");

        apply_sandbox_environment(&plan, &mut command, &request_environment);
        let output = command.output().unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();

        for leaked in [
            "LD_PRELOAD=",
            "DYLD_INSERT_LIBRARIES=",
            "BASH_ENV=",
            "AWS_SECRET_ACCESS_KEY=",
        ] {
            assert!(
                !output.contains(leaked),
                "ambient variable leaked: {leaked}"
            );
        }
        assert!(output.contains("REQUEST_SENTINEL=request-value\n"));
        assert!(output.contains("TERM=aft-test-term\n"));
        assert!(output.contains(&format!(
            "PATH={}\n",
            crate::effective_path::effective_path().to_string_lossy()
        )));
        if let Some(home) = std::env::var_os("HOME") {
            assert!(output.contains(&format!("HOME={}\n", home.to_string_lossy())));
        }
        for key in ["TMPDIR", "TEMP", "TMP"] {
            assert!(output.contains(&format!("{key}={}\n", expected_temp.display())));
        }
    }

    #[cfg(unix)]
    #[test]
    fn request_environment_cannot_inject_preexec_hijack_variables() {
        // The pre-sandbox boundary: a request-supplied environment is applied to
        // the OUTER launcher / `/bin/sh` supervisor, which run BEFORE Landlock or
        // Seatbelt is installed. A loader/shell/interpreter startup hook arriving
        // through the request env must be dropped, or injected code executes
        // outside confinement. (The sibling test covers the ambient direction;
        // this covers the request-supplied direction the re-audit flagged.)
        let project = tempfile::tempdir().unwrap();
        let temp = project.path().join("sandbox-temp");
        std::fs::create_dir(&temp).unwrap();
        let profile = crate::sandbox_profile::SandboxProfile::build(
            vec![project.path().to_path_buf()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp,
        )
        .unwrap();
        let plan = SpawnPlan::launcher_for_test(profile, PathBuf::from("/usr/bin/true"));

        // Every dangerous key arrives through the REQUEST environment this time.
        let hijacks = [
            ("LD_PRELOAD", "/untrusted/loader.so"),
            ("LD_LIBRARY_PATH", "/untrusted/lib"),
            ("LD_AUDIT", "/untrusted/audit.so"),
            ("DYLD_INSERT_LIBRARIES", "/untrusted/loader.dylib"),
            ("DYLD_LIBRARY_PATH", "/untrusted/dylib"),
            ("BASH_ENV", "/untrusted/bash-env"),
            ("ENV", "/untrusted/sh-env"),
            ("PROMPT_COMMAND", "/untrusted/cmd"),
            ("PS4", "evil"),
            ("PYTHONSTARTUP", "/untrusted/py"),
            ("PYTHONPATH", "/untrusted/pypath"),
            ("PERL5OPT", "-M/untrusted"),
            ("RUBYOPT", "-r/untrusted"),
            ("NODE_OPTIONS", "--require=/untrusted"),
            ("GCONV_PATH", "/untrusted/gconv"),
            ("LOCPATH", "/untrusted/loc"),
        ];
        let request_environment: HashMap<String, String> = hijacks
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .chain([("REQUEST_SENTINEL".to_string(), "kept".to_string())])
            .collect();

        let mut command = Command::new("/usr/bin/env");
        apply_sandbox_environment(&plan, &mut command, &request_environment);
        let output = command.output().unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();

        for (key, _) in hijacks {
            assert!(
                !output.contains(&format!("{key}=")),
                "request-supplied hijack var reached the pre-sandbox child: {key}\n{output}"
            );
        }
        // A benign request var still flows through, proving we filtered rather
        // than dropped the whole request environment.
        assert!(output.contains("REQUEST_SENTINEL=kept\n"), "{output}");
    }

    #[test]
    fn request_environment_cannot_smuggle_subc_credentials_into_sandbox_launcher() {
        let temp = tempfile::tempdir().unwrap();
        let request_environment = HashMap::from([
            ("SUBC_MODULE_ID".to_string(), "aft".to_string()),
            ("SUBC_LAUNCH_NONCE".to_string(), "nonce".to_string()),
            (
                "SUBC_FUTURE_CREDENTIAL".to_string(),
                "future-secret".to_string(),
            ),
            ("REQUEST_SENTINEL".to_string(), "kept".to_string()),
        ]);

        let environment = sandboxed_child_environment(&request_environment, temp.path());

        assert!(environment
            .keys()
            .filter_map(|key| key.to_str())
            .all(|key| !crate::agent_child_env::is_subc_credential_env_key(key)));
        assert_eq!(
            environment.get(OsStr::new("REQUEST_SENTINEL")),
            Some(&OsString::from("kept"))
        );
    }

    #[test]
    fn unsandboxed_plan_preserves_inherited_and_request_environment() {
        let plan = SpawnPlan::Unsandboxed;
        let request_environment =
            HashMap::from([("REQUEST_SENTINEL".to_string(), "request-value".to_string())]);
        #[cfg(target_os = "macos")]
        let loader_hook = "/usr/lib/libSystem.B.dylib";
        #[cfg(target_os = "linux")]
        let loader_hook = "libc.so.6";
        let mut command = Command::new("/usr/bin/env");
        command
            .env("UNSANDBOXED_PARENT_SENTINEL", "raw-parent")
            .env("LD_PRELOAD", loader_hook)
            .env("DYLD_INSERT_LIBRARIES", loader_hook)
            .env("AWS_SECRET_ACCESS_KEY", "ambient-cloud-secret")
            .envs(&request_environment);

        let before = command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
            .collect::<Vec<_>>();
        apply_sandbox_environment(&plan, &mut command, &request_environment);
        let after = command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
            .collect::<Vec<_>>();
        assert_eq!(after, before, "unsandboxed environment overrides changed");
        let output = command.output().unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.contains("UNSANDBOXED_PARENT_SENTINEL=raw-parent\n"));
        assert!(output.contains(&format!("LD_PRELOAD={loader_hook}\n")));
        #[cfg(target_os = "linux")]
        assert!(output.contains(&format!("DYLD_INSERT_LIBRARIES={loader_hook}\n")));
        assert!(output.contains("AWS_SECRET_ACCESS_KEY=ambient-cloud-secret\n"));
        assert!(output.contains("REQUEST_SENTINEL=request-value\n"));
    }

    #[test]
    fn product_profile_is_passed_as_a_verified_buffer() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let temp = root.path().join("temp");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        let profile = SandboxProfile::build(
            vec![project],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp,
        )
        .unwrap();
        let (_program, args, retained) = launcher_argv(
            &profile,
            Path::new("/bin/aft"),
            OsStr::new("/bin/sh"),
            &[OsString::from("-c"), OsString::from("true")],
            Path::new("unused"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(args[0], "sandbox-launch");
        assert_eq!(args[1], "--profile-json");
        assert!(serde_json::from_str::<SandboxProfile>(args[2].to_str().unwrap()).is_ok());
        assert!(retained.is_none());
        assert!(!root.path().join("sandbox-profile.json").exists());
    }
}

#[cfg(all(test, unix))]
mod git_policy_cache_tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let environment = vec![
                (OsString::from("HOME"), root.clone().into_os_string()),
                (
                    OsString::from("XDG_CONFIG_HOME"),
                    root.join("xdg").into_os_string(),
                ),
                (
                    OsString::from("GIT_CONFIG_GLOBAL"),
                    root.join("global").into_os_string(),
                ),
                (
                    OsString::from("GIT_CONFIG_SYSTEM"),
                    root.join("system").into_os_string(),
                ),
            ];
            GIT_POLICY_TEST_ENV.with(|value| *value.borrow_mut() = Some(environment));
            let output = git_policy_output(&root, &["init", "--quiet"]).unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let fixture = Self { _dir: dir, root };
            fixture.hooks("first-hooks");
            GIT_POLICY_CALLS.with(|count| count.set(0));
            fixture
        }

        fn hooks(&self, path: &str) {
            std::fs::create_dir_all(self.root.join(path)).unwrap();
            std::fs::write(
                self.root.join(".git/config"),
                format!(
                    "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\thooksPath = {path}\n"
                ),
            )
            .unwrap();
        }

        fn resolve(&self) -> (GitPolicy, usize) {
            let before = GIT_POLICY_CALLS.with(|count| count.get());
            let policy = with_worker_jail(true, || resolve_git_policy(&self.root)).unwrap();
            (policy, GIT_POLICY_CALLS.with(|count| count.get()) - before)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            git_policy_cache().lock().unwrap().remove(&self.root);
            GIT_POLICY_TEST_ENV.with(|value| *value.borrow_mut() = None);
        }
    }

    #[test]
    fn git_policy_cache_second_spawn_uses_zero_git_queries() {
        let fixture = Fixture::new();
        let (first, cold) = fixture.resolve();
        assert!(cold > 0);
        let (second, warm) = fixture.resolve();
        assert_eq!(second.hooks, first.hooks);
        assert_eq!(warm, 0, "unchanged policy must not spawn git");
    }

    #[test]
    fn git_policy_cache_changed_config_reports_new_not_stale_hooks() {
        let fixture = Fixture::new();
        let (old, cold) = fixture.resolve();
        assert_eq!(fixture.resolve().1, 0);
        fixture.hooks("changed-longer-hooks");
        let (new, changed) = fixture.resolve();
        assert_eq!(
            new.hooks,
            vec![fixture.root.join("changed-longer-hooks")],
            "stale hooks path after common config change: {:?}",
            new.hooks
        );
        assert_ne!(new.hooks, old.hooks);
        assert_eq!(changed, cold, "one complete re-resolution is required");
        assert_eq!(fixture.resolve().1, 0);
    }

    #[test]
    fn git_policy_cache_include_directives_always_recompute() {
        let fixture = Fixture::new();
        let included = fixture.root.join("included");
        std::fs::write(&included, "[core]\n hooksPath = included-hooks\n").unwrap();
        std::fs::create_dir(fixture.root.join("included-hooks")).unwrap();
        std::fs::write(
            fixture.root.join(".git/config"),
            format!("[include]\n path = {}\n", included.display()),
        )
        .unwrap();
        let (first, cold) = fixture.resolve();
        assert_eq!(first.hooks, vec![fixture.root.join("included-hooks")]);
        assert!(cold > 0);
        assert!(
            fixture.resolve().1 > 0,
            "include config must never be cached"
        );
        std::fs::write(
            &included,
            "[core]\n hooksPath = replacement-included-hooks\n",
        )
        .unwrap();
        let (changed, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(
            changed.hooks,
            vec![fixture.root.join("replacement-included-hooks")]
        );
    }

    #[test]
    fn git_policy_cache_global_system_worktree_and_environment_invalidate() {
        let fixture = Fixture::new();
        fixture.resolve();
        for name in ["global", "system"] {
            std::fs::write(
                fixture.root.join(name),
                "[mason]\n originalHooksPath = policy-hooks\n",
            )
            .unwrap();
            let (policy, calls) = fixture.resolve();
            assert!(calls > 0, "creating {name} config must invalidate");
            assert!(policy.configured_paths.contains(&(
                "mason.originalHooksPath".into(),
                fixture.root.join("policy-hooks")
            )));
            assert_eq!(fixture.resolve().1, 0);
        }
        let config = fixture.root.join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("[extensions]\n worktreeConfig = true\n");
        std::fs::write(config, text).unwrap();
        fixture.resolve();
        std::fs::write(
            fixture.root.join(".git/config.worktree"),
            "[core]\n hooksPath = worktree-hooks\n",
        )
        .unwrap();
        let (policy, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(policy.hooks, vec![fixture.root.join("worktree-hooks")]);
        assert_eq!(fixture.resolve().1, 0);
        GIT_POLICY_TEST_ENV.with(|value| {
            value.borrow_mut().as_mut().unwrap().extend([
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                ("GIT_CONFIG_KEY_0".into(), "core.hooksPath".into()),
                ("GIT_CONFIG_VALUE_0".into(), "environment-hooks".into()),
            ])
        });
        let (policy, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(policy.hooks, vec![fixture.root.join("environment-hooks")]);
        assert_eq!(fixture.resolve().1, 0);
    }

    #[test]
    fn git_policy_cache_detects_hooks_symlink_retargeting() {
        let fixture = Fixture::new();
        fixture.resolve();
        std::fs::create_dir(fixture.root.join("redirected-hooks")).unwrap();
        std::fs::remove_dir(fixture.root.join("first-hooks")).unwrap();
        std::os::unix::fs::symlink("redirected-hooks", fixture.root.join("first-hooks")).unwrap();
        let (policy, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(policy.hooks, vec![fixture.root.join("redirected-hooks")]);
    }

    #[test]
    fn git_policy_cache_pointer_and_commondir_invalidate() {
        let fixture = Fixture::new();
        let first = fixture.root.join("first-git");
        std::fs::rename(fixture.root.join(".git"), &first).unwrap();
        std::fs::write(fixture.root.join(".git"), "gitdir: first-git\n").unwrap();
        fixture.resolve();
        assert_eq!(fixture.resolve().1, 0);
        let other = fixture.root.join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(git_policy_output(&other, &["init", "--quiet"])
            .unwrap()
            .status
            .success());
        std::fs::write(
            other.join(".git/config"),
            "[core]\n bare = false\n hooksPath = other-hooks\n",
        )
        .unwrap();
        std::fs::write(fixture.root.join(".git"), "gitdir: other/.git\n").unwrap();
        let (policy, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(policy.hooks, vec![fixture.root.join("other-hooks")]);
        std::fs::write(fixture.root.join(".git"), "gitdir: first-git\n").unwrap();
        fixture.resolve();
        assert_eq!(fixture.resolve().1, 0);
        std::fs::write(first.join("commondir"), "../other/.git\n").unwrap();
        let (policy, calls) = fixture.resolve();
        assert!(calls > 0);
        assert_eq!(policy.hooks, vec![fixture.root.join("other-hooks")]);
        assert_eq!(fixture.resolve().1, 0);
    }

    #[test]
    fn git_policy_cache_include_if_even_inactive_disables_caching() {
        let fixture = Fixture::new();
        let config = fixture.root.join(".git/config");
        let mut contents = std::fs::read_to_string(&config).unwrap();
        contents
            .push_str("[includeIf \"gitdir:/nonexistent-aft-repo/\"]\n path = absent-include\n");
        std::fs::write(config, contents).unwrap();
        assert!(fixture.resolve().1 > 0);
        assert!(fixture.resolve().1 > 0);
    }

    #[test]
    fn git_policy_cache_redirected_git_directory_is_never_cached() {
        let fixture = Fixture::new();
        GIT_POLICY_TEST_ENV.with(|value| {
            value
                .borrow_mut()
                .as_mut()
                .unwrap()
                .push(("GIT_DIR".into(), fixture.root.join(".git").into_os_string()))
        });
        assert!(fixture.resolve().1 > 0);
        assert!(
            fixture.resolve().1 > 0,
            "redirected Git config inputs are not tracked"
        );
    }

    /// Compare the old explicit-PATH fork with inherited-PATH helper launches.
    /// This measures jail policy preparation without entering a nested sandbox.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "manual large-heap spawn measurement; run alone with --nocapture"]
    fn large_heap_spawn_latency() {
        crate::effective_path::initialize_process_path();
        let fixture = Fixture::new();
        assert!(git_policy_inputs(
            &fixture.root,
            &fixture.root.join(".git"),
            &fixture.root.join(".git"),
            git_policy_environment()
        )
        .is_some());
        let commit = git_policy_output(
            &fixture.root,
            &[
                "-c",
                "user.name=Bench",
                "-c",
                "user.email=bench@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "benchmark",
            ],
        )
        .unwrap();
        assert!(
            commit.status.success(),
            "{}",
            String::from_utf8_lossy(&commit.stderr)
        );
        let mib: usize = std::env::var("AFT_SPAWN_BENCH_HEAP_MIB")
            .ok()
            .map(|value| value.parse().unwrap())
            .unwrap_or(2048);
        // Touch real allocations, and park threads rather than generating load.
        let heap: Vec<Vec<u8>> = (0..mib * 8).map(|_| vec![1u8; 128 * 1024]).collect();
        let barrier = Arc::new(std::sync::Barrier::new(65));
        let threads: Vec<_> = (0..64)
            .map(|_| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let allocation = vec![7u8; 128 * 1024];
                    barrier.wait();
                    barrier.wait();
                    std::hint::black_box(allocation);
                })
            })
            .collect();
        barrier.wait();
        fn measure(name: &str, mut run: impl FnMut()) {
            let mut samples = Vec::new();
            for _ in 0..20 {
                let started = Instant::now();
                run();
                samples.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(f64::total_cmp);
            eprintln!(
                "{name}: n=20 mean_ms={:.3} median_ms={:.3} p95_ms={:.3}",
                samples.iter().sum::<f64>() / 20.0,
                samples[10],
                samples[18]
            );
        }
        eprintln!(
            "heap_mib={mib} parked_threads=64 git={}",
            String::from_utf8_lossy(
                &git_policy_output(&fixture.root, &["--version"])
                    .unwrap()
                    .stdout
            )
            .trim()
        );
        for old in [true, false] {
            GIT_POLICY_BENCH_OLD.with(|value| value.set(old));
            let label = if old {
                "before_fork"
            } else {
                "after_posix_spawn"
            };
            measure(&format!("worktree_probe/{label}"), || {
                assert!(git_policy_output(
                    &fixture.root,
                    &[
                        "rev-parse",
                        "--path-format=absolute",
                        "--git-dir",
                        "--git-common-dir"
                    ]
                )
                .unwrap()
                .status
                .success());
            });
            measure(&format!("cache_key_resolve/{label}"), || {
                assert!(
                    git_policy_output(&fixture.root, &["rev-list", "--max-parents=0", "HEAD"])
                        .unwrap()
                        .status
                        .success()
                );
            });
            measure(&format!("jail_policy_cold/{label}"), || {
                git_policy_cache().lock().unwrap().remove(&fixture.root);
                fixture.resolve();
            });
            fixture.resolve();
            measure(&format!("jail_policy_warm/{label}"), || {
                fixture.resolve();
            });
            measure(&format!("shell_preexec/{label}"), || {
                use std::os::fd::AsRawFd;
                let marker = File::open("/dev/null").unwrap();
                let mut command = crate::effective_path::new_command("/bin/sh");
                if old {
                    command.env("PATH", crate::effective_path::effective_path());
                }
                command.args(["-c", "true"]);
                crate::bash_background::process::start_new_session(&mut command);
                apply_marker_fd_allowlist(&mut command, marker.as_raw_fd(), marker.as_raw_fd())
                    .unwrap();
                crate::privacy_spawn::install(&mut command, true, false).unwrap();
                assert!(command.status().unwrap().success());
            });
        }
        GIT_POLICY_BENCH_OLD.with(|value| value.set(false));
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        std::hint::black_box(heap);
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn sandbox_task_temp_supports_long_unix_socket_names() {
        use std::os::unix::net::{UnixListener, UnixStream};
        let root = tempfile::tempdir().unwrap();
        let bundle = root
            .path()
            .join("runner/bash-tasks/0123456789abcdef/bash-0123456789abcdef/io");
        std::fs::create_dir_all(&bundle).unwrap();
        let temp = create_task_temp_dir(&bundle).unwrap();
        let nested = temp.join("a".repeat(30));
        std::fs::create_dir(&nested).unwrap();
        let socket = nested.join(format!("{}.sock", "b".repeat(30)));
        let bound = UnixListener::bind(&socket);
        if bound.is_ok() {
            UnixStream::connect(&socket).unwrap();
        }
        cleanup_task_temp_dir(&temp).unwrap();
        assert!(bound.is_ok(), "cannot bind {}: {bound:?}", socket.display());
    }

    #[cfg(unix)]
    #[test]
    fn worker_build_slot_grants_are_exact_and_reject_model_writable_launchers() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let store = home.join(".local/share/cortexkit");
        let project = store.join("alfonso/worktrees/container/own");
        let slots = store.join("operator/build-slots");
        for path in [&project, &slots] {
            std::fs::create_dir_all(path).unwrap();
        }
        for name in [
            "prefrontal-build-slot",
            "rustc-lowprio-direct",
            ".root",
            ".cache",
        ] {
            std::fs::write(slots.join(name), "operator-owned").unwrap();
        }
        std::fs::write(slots.join("limit"), "2\n").unwrap();
        let policy = crate::access_policy::AccessPolicy::from_config(
            &crate::config::Config::default(),
            &store.join("aft"),
            vec![project.clone()],
            &home,
        );
        let mut grants = WorkerJailGrants::default();
        add_build_slot_grants(
            &slots.join("prefrontal-build-slot"),
            &[project.clone()],
            &policy,
            &mut grants,
        );
        assert_eq!(
            grants.files,
            ["queue.lock", "0.lock", "1.lock", "2.lock"].map(|name| slots.join(name))
        );
        assert_eq!(grants.read_only.len(), 5);
        assert!(grants.roots.is_empty() && grants.caches.is_empty());
        assert!(grants
            .read_only
            .iter()
            .all(|path| path.is_file() && path.parent() == Some(slots.as_path())));
        let mut refused = WorkerJailGrants::default();
        add_build_slot_grants(
            &slots.join("prefrontal-build-slot"),
            &[slots.clone()],
            &policy,
            &mut refused,
        );
        assert_eq!(
            refused,
            WorkerJailGrants::default(),
            "a model-writable launcher must never be pinned"
        );
        #[cfg(unix)]
        {
            std::fs::remove_file(slots.join(".root")).unwrap();
            std::fs::write(home.join("secret"), "secret").unwrap();
            std::os::unix::fs::symlink(home.join("secret"), slots.join(".root")).unwrap();
            add_build_slot_grants(
                &slots.join("prefrontal-build-slot"),
                &[project],
                &policy,
                &mut refused,
            );
            assert_eq!(
                refused,
                WorkerJailGrants::default(),
                "a sibling symlink must not widen the read exception"
            );
        }
    }

    fn context(project_root: PathBuf) -> AppContext {
        AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project_root),
                sandbox: crate::config::SandboxConfig {
                    enabled: true,
                    ..crate::config::SandboxConfig::default()
                },
                ..crate::config::Config::default()
            },
        )
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn concurrent_sandbox_marker_pty_and_git_spawns_preserve_fd_ownership() {
        use portable_pty::{CommandBuilder, PtySize};
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        use std::process::Stdio;
        use std::sync::{Arc, Barrier};

        const ITERATIONS: usize = 32;
        let fixture = tempfile::tempdir().unwrap();
        let project = fixture.path().join("project");
        let task_io = fixture
            .path()
            .join("tasks/session/bash-0000000000000001/io");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&task_io).unwrap();
        let initialized = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&project)
            .status()
            .unwrap();
        assert!(initialized.success());

        let ctx = Arc::new(context(project));
        let task_io = Arc::new(task_io);
        let barrier = Arc::new(Barrier::new(4));

        let resolver = {
            let ctx = Arc::clone(&ctx);
            let task_io = Arc::clone(&task_io);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERATIONS {
                    let plan = resolve_sandbox_spawn(
                        &ctx,
                        &AuthenticatedPrincipal::FirstParty,
                        RequestedSandboxTier::Native,
                        SandboxTaskKind::BashBackground,
                        &task_io,
                        None,
                    );
                    assert!(plan.is_native_launcher(), "unexpected plan: {plan:?}");
                    plan.cleanup_unspawned();
                }
            })
        };
        let detached = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERATIONS {
                    let exit = tempfile::tempfile().unwrap();
                    let failure = tempfile::tempfile().unwrap();
                    crate::bash_background::persistence::set_close_on_exec(exit.as_raw_fd(), false)
                        .unwrap();
                    crate::bash_background::persistence::set_close_on_exec(
                        failure.as_raw_fd(),
                        false,
                    )
                    .unwrap();
                    let mut command = Command::new("/usr/bin/true");
                    apply_marker_fd_allowlist(&mut command, exit.as_raw_fd(), failure.as_raw_fd())
                        .unwrap();
                    let status = command
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .unwrap();
                    assert!(status.success());
                }
            })
        };
        let pty = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERATIONS {
                    let pair = portable_pty::native_pty_system()
                        .openpty(PtySize {
                            rows: 24,
                            cols: 80,
                            pixel_width: 0,
                            pixel_height: 0,
                        })
                        .unwrap();
                    let mut child = pair
                        .slave
                        .spawn_command(CommandBuilder::new("/usr/bin/true"))
                        .unwrap();
                    child.wait().unwrap();
                }
            })
        };
        let probes = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERATIONS {
                    let output = Command::new("git").arg("--version").output().unwrap();
                    assert!(output.status.success());
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(libc::O_CLOEXEC)
                        .open("/dev/null")
                        .unwrap();
                    drop(file);
                }
            })
        };

        resolver.join().unwrap();
        detached.join().unwrap();
        pty.join().unwrap();
        probes.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn worker_jail_reads_only_build_settings_from_cargo_config() {
        assert_eq!(
            parse_cargo_build_settings(
                "[build]\nrustc-wrapper = \"/x/rustc-lowprio\"\ntarget-dir = \"shared-target\"\njobs = 12\n"
            ),
            Some(CargoBuildSettings {
                target_dir: Some("shared-target".into()),
                rustc_wrapper: Some("/x/rustc-lowprio".into()),
            })
        );
        assert_eq!(parse_cargo_build_settings("[net]\noffline = true\n"), None);
        assert_eq!(parse_cargo_build_settings("not toml ["), None);
    }

    #[cfg(unix)]
    #[test]
    fn worker_jail_sccache_ports_always_include_default_and_operator_port() {
        assert_eq!(sccache_server_ports(None), vec![4226]);
        assert_eq!(sccache_server_ports(Some("invalid")), vec![4226]);
        assert_eq!(sccache_server_ports(Some("65536")), vec![4226]);
        assert_eq!(sccache_server_ports(Some("0")), vec![4226]);
        assert_eq!(sccache_server_ports(Some("4226")), vec![4226]);
        assert_eq!(sccache_server_ports(Some("54226")), vec![4226, 54226]);
    }

    #[test]
    fn worker_jail_environment_scrubs_sccache_channels_only_when_jailed() {
        let keys = [
            "SCCACHE_SERVER_PORT",
            "SCCACHE_SERVER_UDS",
            "SCCACHE_STARTUP_NOTIFY",
            "SCCACHE_REDIS_ENDPOINT",
            "SCCACHE_WEBDAV_ENDPOINT",
            "SCCACHE_ENDPOINT",
            "SCCACHE_DIRECT",
            "SCCACHE_FUTURE_SERVER_CHANNEL",
        ];
        let original = keys
            .into_iter()
            .chain(["KEEP_ME"])
            .map(|key| (key.to_string(), "operator-value".to_string()))
            .collect::<HashMap<_, _>>();
        let mut environment = original.clone();
        with_worker_jail(false, || apply_worker_jail_environment(&mut environment));
        assert_eq!(environment, original);
        with_worker_jail(true, || apply_worker_jail_environment(&mut environment));
        for key in keys {
            assert!(
                !environment.contains_key(key),
                "jailed client retained {key}"
            );
        }
        assert_eq!(environment["KEEP_ME"], "operator-value");
    }

    #[cfg(unix)]
    #[test]
    fn worker_jail_environment_disables_rustc_wrappers_only_when_jailed() {
        let mut free = HashMap::new();
        apply_worker_jail_environment(&mut free);
        assert!(free.is_empty());
        let mut jailed = HashMap::from([("RUSTC_WRAPPER".to_string(), "sccache".to_string())]);
        with_worker_jail(true, || apply_worker_jail_environment(&mut jailed));
        assert_eq!(jailed["RUSTC_WRAPPER"], "");
        assert_eq!(jailed["CARGO_BUILD_RUSTC_WRAPPER"], "");
    }

    #[cfg(unix)]
    #[test]
    fn worker_jail_target_may_not_contain_home_roots_or_protected_paths() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let home = base.join("home");
        let root = base.join("work").join("worktree");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let policy = crate::access_policy::AccessPolicy::from_config(
            &crate::config::Config::default(),
            &base.join("storage"),
            vec![root.clone()],
            &home,
        );
        let roots = [root.clone()];
        assert!(jail_target_allowed(
            &base.join("shared-target"),
            &home,
            &roots,
            &policy
        ));
        assert!(jail_target_allowed(
            &root.join("target"),
            &home,
            &roots,
            &policy
        ));
        assert!(!jail_target_allowed(&base, &home, &roots, &policy));
        assert!(!jail_target_allowed(&home, &home, &roots, &policy));
        assert!(!jail_target_allowed(
            &base.join("work"),
            &home,
            &roots,
            &policy
        ));
        assert!(!jail_target_allowed(
            &home.join(".ssh/target"),
            &home,
            &roots,
            &policy
        ));
        assert!(!jail_target_allowed(
            &home.join(".local/share/cortexkit/target"),
            &home,
            &roots,
            &policy
        ));
        assert!(!jail_target_allowed(
            Path::new("relative"),
            &home,
            &roots,
            &policy
        ));
        assert!(!jail_target_allowed(Path::new("/"), &home, &roots, &policy));
    }

    #[cfg(unix)]
    #[test]
    fn worker_jail_git_grants_cover_shared_stores_but_not_config_hooks_or_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join("main/.git");
        let own = common.join("worktrees/own");
        for path in [
            common.join("objects"),
            common.join("refs"),
            common.join("logs"),
            common.join("hooks"),
            common.join("info"),
            own.clone(),
            common.join("worktrees/sibling"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        let linked = GitPolicy {
            read_roots: Vec::new(),
            hooks: vec![common.join("hooks")],
            linked: Some((own.clone(), common.clone())),
            configured_paths: Vec::new(),
            metadata_allow: Vec::new(),
        };
        let main = GitPolicy {
            read_roots: Vec::new(),
            hooks: Vec::new(),
            linked: None,
            configured_paths: Vec::new(),
            metadata_allow: Vec::new(),
        };
        let grants = linked_git_grants(&[linked, main]);
        assert_eq!(
            grants,
            vec![
                own,
                common.join("objects"),
                common.join("refs"),
                common.join("logs")
            ]
        );
        for refused in ["config", "hooks", "info", "worktrees/sibling", ""] {
            let path = common.join(refused);
            assert!(
                !grants.iter().any(|grant| path.starts_with(grant)),
                "{} must stay read-only",
                path.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn worker_jail_packed_refs_grants_are_exact_files_even_before_creation() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let home = base.join("home");
        let worktree = base.join("worktree");
        let common = base.join("main/.git");
        let own = common.join("worktrees/own");
        for path in [&home, &worktree, &own] {
            std::fs::create_dir_all(path).unwrap();
        }
        let policy = crate::access_policy::AccessPolicy::from_config(
            &crate::config::Config::default(),
            &base.join("storage"),
            vec![worktree.clone()],
            &home,
        );
        let git = GitPolicy {
            read_roots: Vec::new(),
            hooks: vec![common.join("hooks")],
            linked: Some((own, common.clone())),
            configured_paths: Vec::new(),
            metadata_allow: Vec::new(),
        };
        let grants = worker_jail_grants(&home, &[worktree], &[git], &policy);
        let files = [
            common.join("packed-refs"),
            common.join("packed-refs.lock"),
            common.join("packed-refs.new"),
        ];
        for file in &files {
            assert!(!file.exists());
            assert!(
                grants.files.contains(file),
                "missing exact file grant: {file:?}"
            );
        }
        assert!(!grants.roots.iter().any(|root| common.starts_with(root)));
        for name in [
            "config",
            "hooks/post-commit",
            "info/exclude",
            "packed-refs.other",
            "packed-refs.new.other",
            "HEAD.lock",
            "ORIG_HEAD",
            "FETCH_HEAD",
        ] {
            let refused = common.join(name);
            assert!(!grants.files.contains(&refused));
            assert!(!grants.roots.iter().any(|root| refused.starts_with(root)));
        }

        // A lock can disappear between collecting private-store exceptions and
        // canonicalizing the profile, or before the launcher revalidates it.
        let mut policy = policy;
        policy.private_stores.read_allow.extend(files.clone());
        policy.private_stores.write_allow.extend(files.clone());
        let mut profile = policy
            .compile_shell(home.clone(), Vec::new(), Vec::new())
            .unwrap();
        profile.writable_roots.extend(grants.roots);
        profile.writable_files = grants.files;
        let profile = profile.canonicalize_for_launch().unwrap();
        let transferred: SandboxProfile =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        assert_eq!(transferred.canonicalize_for_launch().unwrap(), profile);
        for file in &files {
            assert!(profile.writable_files.contains(file));
            assert!(profile.data_policy.read_allow.contains(file));
            assert!(profile.data_policy.write_allow.contains(file));
        }
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_setup_refusal_names_root_and_cause_before_remedy() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let plan = crate::log_ctx::with_session(Some("fd-audit".to_string()), || {
            sandbox_setup_refusal(
                &ctx,
                &AuthenticatedPrincipal::FirstParty,
                "failed to query core.hooksPath: Bad file descriptor (os error 9)",
            )
        });
        let message = plan.refusal_message().expect("sandbox refusal message");
        assert!(message.starts_with(&format!(
            "sandbox setup for {} failed: failed to query core.hooksPath: Bad file descriptor (os error 9)",
            project.path().display()
        )));
        assert!(message.ends_with("set sandbox.enabled=false to disable native sandboxing"));
    }

    #[cfg(unix)]
    #[test]
    fn native_sandbox_predicate_controls_spawn_and_rewrite() {
        let project = tempfile::tempdir().unwrap();
        let file = project.path().join("rewrite-probe.txt");
        std::fs::write(&file, "sandboxed\n").unwrap();
        let ctx = context(project.path().to_path_buf());
        ctx.update_config(|config| config.experimental_bash_rewrite = true);
        // Capture files belong to the private task store, not the project. Using
        // the project as task IO would also deny project access on Landlock.
        let task = crate::bash_background::persistence::allocate_task_layout(
            &ctx.storage_dir(),
            "sandbox-predicate-test",
        )
        .unwrap();
        let principal = AuthenticatedPrincipal::FirstParty;
        let command = format!("cat {}", file.display());

        assert!(native_sandbox_enforced(&ctx, &principal));
        assert!(crate::bash_rewrite::try_rewrite(&command, None, &ctx, &principal).is_none());
        let sandboxed = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            &task.paths.io_dir,
            None,
        );
        assert!(
            matches!(&sandboxed, SpawnPlan::Launcher { .. }),
            "native sandbox must use the launcher: {sandboxed:?}"
        );
        sandboxed.cleanup_unspawned();

        ctx.update_config(|config| config.sandbox.enabled = false);
        assert!(!native_sandbox_enforced(&ctx, &principal));
        assert!(crate::bash_rewrite::try_rewrite(&command, None, &ctx, &principal).is_some());
        assert_eq!(
            resolve_sandbox_spawn(
                &ctx,
                &principal,
                RequestedSandboxTier::Native,
                SandboxTaskKind::BashForeground,
                &task.paths.io_dir,
                None,
            ),
            SpawnPlan::Unsandboxed
        );
    }

    #[test]
    fn untrusted_principal_never_enters_the_native_launcher() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::Untrusted,
            route_channel: 7,
            route_epoch: 1,
            project_root: project.path().to_path_buf(),
            harness: "mcp:test".to_string(),
            session_id: "untrusted-sandbox-test".to_string(),
            principal_id: Some("unverified".to_string()),
        };

        let plan = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        // The invariant is that an untrusted principal never receives a native
        // Launcher plan. On Unix that surfaces as a downgrade to Unsandboxed
        // (the untrusted principal fails the first-party enforcement check); on
        // platforms without a kernel backend the enabled policy fails closed
        // with a platform refusal before the trust check is reached. Both honor
        // the invariant, so assert the exact non-Launcher outcome per platform.
        #[cfg(unix)]
        assert_eq!(plan, SpawnPlan::Unsandboxed);
        #[cfg(not(unix))]
        assert!(
            matches!(&plan, SpawnPlan::Refused { code, .. } if *code == "sandbox_unavailable"),
            "untrusted principal must never reach the native launcher; got {plan:?}"
        );
    }

    #[cfg(unix)]
    fn grant_attempt(
        grant_id: String,
        project: &Path,
        command: &[u8],
        environment: ChildEnvironment,
    ) -> HostEscalationAttempt {
        HostEscalationAttempt {
            grant_id,
            command: command.to_vec(),
            root: project.to_path_buf(),
            cwd: project.to_path_buf(),
            shell_path: PathBuf::from("/bin/sh"),
            environment,
        }
    }

    #[cfg(unix)]
    fn mint_test_grant(
        ctx: &AppContext,
        principal: &AuthenticatedPrincipal,
        project: &Path,
        command: &[u8],
        environment: &ChildEnvironment,
        now: Instant,
    ) -> String {
        mint_host_escalation_grant_at(
            ctx,
            principal,
            command,
            project,
            project,
            Path::new("/bin/sh"),
            environment,
            &project.join(".aft-test-storage"),
            "test-session",
            now,
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn refusal_class(plan: &SpawnPlan) -> Option<&'static str> {
        plan.refusal_mismatch_class()
    }

    #[cfg(unix)]
    #[test]
    fn escalation_grant_binds_exact_command_and_environment() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment = ChildEnvironment::from([
            (OsString::from("A"), OsString::from("one")),
            (OsString::from("B"), OsString::from("two")),
        ]);

        for (command, retry_environment) in [
            (b"printf approved!".as_slice(), environment.clone()),
            (
                b"printf approved".as_slice(),
                ChildEnvironment::from([
                    (OsString::from("A"), OsString::from("changed")),
                    (OsString::from("B"), OsString::from("two")),
                ]),
            ),
        ] {
            let grant_id = mint_test_grant(
                &ctx,
                &principal,
                project.path(),
                b"printf approved",
                &environment,
                Instant::now(),
            );
            let attempt = grant_attempt(grant_id, project.path(), command, retry_environment);
            let plan = resolve_sandbox_spawn(
                &ctx,
                &principal,
                RequestedSandboxTier::Host,
                SandboxTaskKind::BashForeground,
                project.path(),
                Some(&attempt),
            );
            assert_eq!(refusal_class(&plan), Some("digest_mismatch"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn escalation_grant_is_single_use_and_expires() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment =
            ChildEnvironment::from([(OsString::from("ONLY"), OsString::from("snapshot"))]);
        let grant_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let attempt = grant_attempt(grant_id, project.path(), b"true", environment.clone());
        let first = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        assert!(matches!(first.policy(), SpawnPlan::Host { .. }));
        let second = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        assert_eq!(refusal_class(&second), Some("consumed"));

        let expired_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now() - ESCALATION_GRANT_TTL - Duration::from_millis(1),
        );
        let expired_attempt =
            grant_attempt(expired_id, project.path(), b"true", environment.clone());
        let expired = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&expired_attempt),
        );
        assert_eq!(refusal_class(&expired), Some("expired"));
    }

    #[cfg(unix)]
    #[test]
    fn unclaimable_escalation_payloads_are_reclaimed_and_refusals_stay_exact() {
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment =
            ChildEnvironment::from([(OsString::from("ONLY"), OsString::from("snapshot"))]);
        let bundle_exists = |grant_id: &str| {
            let store = ctx.escalation_grants().lock();
            let grant = &store.grants[grant_id];
            grant.session_dir.join(&grant.task_id).exists()
                || grant
                    .session_dir
                    .join(format!("{}.json", grant.task_id))
                    .exists()
        };
        let start = Instant::now();
        let stale = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            start,
        );
        assert!(bundle_exists(&stale));

        // The next mint after expiry deletes the unused bundle.
        let later = start + ESCALATION_GRANT_TTL;
        let fresh = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            later,
        );
        assert!(!bundle_exists(&stale), "expired payload bundle was kept");
        assert!(bundle_exists(&fresh));
        let replay = grant_attempt(stale, project.path(), b"true", environment.clone());
        assert_eq!(
            consume_host_escalation_grant_at(&ctx, &principal, &replay, later)
                .err()
                .map(EscalationRefusal::class),
            Some("expired")
        );

        // A refused attempt uses the grant up, so its bundle goes too.
        let mut wrong_root = grant_attempt(fresh.clone(), project.path(), b"true", environment);
        wrong_root.root = elsewhere.path().to_path_buf();
        assert_eq!(
            consume_host_escalation_grant_at(&ctx, &principal, &wrong_root, later)
                .err()
                .map(EscalationRefusal::class),
            Some("digest_mismatch")
        );
        assert!(!bundle_exists(&fresh), "refused payload bundle was kept");
        assert_eq!(ctx.escalation_grants().lock().len_for_test(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn escalation_grant_binds_principal_and_happy_path_reuses_snapshot() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::FirstParty,
            route_channel: 9,
            route_epoch: 2,
            project_root: project.path().to_path_buf(),
            harness: "opencode".to_string(),
            session_id: "session-x".to_string(),
            principal_id: Some("direct".to_string()),
        };
        let environment =
            ChildEnvironment::from([(OsString::from("APPROVED"), OsString::from("snapshot"))]);
        let wrong_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let wrong_attempt = grant_attempt(wrong_id, project.path(), b"true", environment.clone());
        let wrong = resolve_sandbox_spawn(
            &ctx,
            &AuthenticatedPrincipal::FirstParty,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&wrong_attempt),
        );
        assert_eq!(refusal_class(&wrong), Some("wrong_principal"));

        let happy_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let happy_attempt = grant_attempt(happy_id, project.path(), b"true", environment.clone());
        let happy = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&happy_attempt),
        );
        match happy.policy() {
            SpawnPlan::Host {
                shell_path,
                environment: actual,
            } => {
                assert_eq!(shell_path, Path::new("/bin/sh"));
                assert_eq!(actual, &environment);
            }
            other => panic!("expected host plan, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_host_request_refuses_without_minting_a_grant() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::Untrusted,
            route_channel: 7,
            route_epoch: 1,
            project_root: project.path().to_path_buf(),
            harness: "mcp:test".to_string(),
            session_id: "untrusted-escalation-test".to_string(),
            principal_id: Some("unverified".to_string()),
        };
        let plan = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        assert_eq!(plan.refusal_code(), Some("sandbox_escalation_denied"));
        assert!(ctx.escalation_grants().lock().grants.is_empty());
    }

    #[cfg(unix)]
    fn grant_task_paths(ctx: &AppContext, grant_id: &str) -> (PathBuf, String) {
        let store = ctx.escalation_grants().lock();
        let grant = store.grants.get(grant_id).unwrap();
        (grant.session_dir.clone(), grant.task_id.clone())
    }

    #[cfg(unix)]
    #[test]
    fn escalated_payload_path_race_is_refused_and_burns_the_grant() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment = ChildEnvironment::new();
        let grant_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let (session_dir, task_id) = grant_task_paths(&ctx, &grant_id);
        let command_path = session_dir
            .join(&task_id)
            .join("control")
            .join(crate::bash_background::persistence::COMMAND_FILE);
        let victim = project.path().join("victim");
        std::fs::write(&victim, b"victim-bytes").unwrap();
        std::fs::remove_file(&command_path).unwrap();
        symlink(&victim, &command_path).unwrap();

        let attempt = grant_attempt(grant_id.clone(), project.path(), b"true", environment);
        let refused = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        assert_eq!(refusal_class(&refused), Some("digest_mismatch"));
        assert_eq!(std::fs::read(&victim).unwrap(), b"victim-bytes");
        let consumed = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        assert_eq!(refusal_class(&consumed), Some("consumed"));
    }

    #[cfg(unix)]
    #[test]
    fn verified_host_payload_executes_verified_buffers_after_inode_mutation() {
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment = ChildEnvironment::new();
        let grant_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let attempt = grant_attempt(grant_id, project.path(), b"true", environment);
        let plan = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        let prepared = plan.prepared_task().expect("verified prepared task");
        let command_path = prepared
            .paths()
            .control_dir
            .join(crate::bash_background::persistence::COMMAND_FILE);
        let victim = project.path().join("victim");
        std::fs::write(&victim, b"victim-bytes").unwrap();
        let payload = prepared.invocation().unwrap();
        // Mutate the same inode after verification. Execution must use the
        // verified in-memory buffers rather than rereading the held file.
        std::fs::write(
            &command_path,
            format!("printf hacked > {}", victim.display()),
        )
        .unwrap();
        let exit_path = project.path().join("exit");
        let exit = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&exit_path)
            .unwrap();
        crate::bash_background::persistence::set_close_on_exec(exit.as_raw_fd(), false).unwrap();
        // Mirror production (apply_marker_fd_allowlist): the real marker fd is
        // dup2'd onto the low, single-digit CHILD_EXIT_FD before exec, and the
        // wrapper is handed that literal. POSIX sh (dash) only parses a
        // single-digit `>&N` redirect target, so passing a raw multi-digit fd
        // here would diverge from production and fail under dash with
        // "Bad fd number" (production never hits this: it always remaps to 3).
        let raw_exit_fd = exit.as_raw_fd();
        let exit_fd = CHILD_EXIT_FD.to_string();
        let mut command = Command::new("/bin/sh");
        command.args([
            OsStr::new("-c"),
            payload.wrapper_text.as_os_str(),
            OsStr::new("aft-payload-wrapper"),
            OsStr::new("/bin/sh"),
            payload.command_text.as_os_str(),
            OsStr::new(&exit_fd),
        ]);
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(move || {
                    if raw_exit_fd != CHILD_EXIT_FD && libc::dup2(raw_exit_fd, CHILD_EXIT_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let status = command.status().unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read(&victim).unwrap(), b"victim-bytes");
    }

    #[cfg(unix)]
    #[test]
    fn approval_spawn_drift_matrix_refuses_every_bound_field() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let approved = ChildEnvironment::from([(OsString::from("A"), OsString::from("one"))]);
        for drift in [
            "command",
            "newline",
            "encoding",
            "cwd",
            "root",
            "shell",
            "environment",
            "environment_file_encoding",
            "wrapper_template",
        ] {
            let grant_id = mint_test_grant(
                &ctx,
                &principal,
                project.path(),
                b"printf approved",
                &approved,
                Instant::now(),
            );
            let mut attempt = grant_attempt(
                grant_id,
                project.path(),
                b"printf approved",
                approved.clone(),
            );
            match drift {
                "command" => attempt.command = b"printf changed".to_vec(),
                "newline" => attempt.command.push(b'\n'),
                "encoding" => attempt.command.push(0xff),
                "cwd" => attempt.cwd = project.path().join("changed-cwd"),
                "root" => attempt.root = project.path().join("changed-root"),
                "shell" => attempt.shell_path = PathBuf::from("/bin/bash"),
                "environment" => {
                    attempt
                        .environment
                        .insert(OsString::from("A"), OsString::from("two"));
                }
                "environment_file_encoding" | "wrapper_template" => {
                    let (session_dir, task_id) = grant_task_paths(&ctx, &attempt.grant_id);
                    let name = if drift == "wrapper_template" {
                        crate::bash_background::persistence::WRAPPER_FILE
                    } else {
                        crate::bash_background::persistence::ENVIRONMENT_FILE
                    };
                    std::fs::write(
                        session_dir.join(task_id).join("control").join(name),
                        b"drift",
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let refused = resolve_sandbox_spawn(
                &ctx,
                &principal,
                RequestedSandboxTier::Host,
                SandboxTaskKind::BashForeground,
                project.path(),
                Some(&attempt),
            );
            assert_eq!(refusal_class(&refused), Some("digest_mismatch"), "{drift}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn payload_read_grant_seam_exposes_only_exact_control_objects() {
        let storage = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment = ChildEnvironment::from([(OsString::from("SAFE"), OsString::from("yes"))]);
        let layout =
            crate::bash_background::persistence::allocate_task_layout(storage.path(), "session")
                .unwrap();
        let task = prepare_task_payload(
            &layout,
            b"true",
            project.path(),
            project.path(),
            &principal,
            Path::new("/bin/sh"),
            &environment,
        )
        .unwrap();
        let plan = SpawnPlan::Unsandboxed.with_prepared_task(task.clone());
        let grants = plan.payload_read_grants();
        assert_eq!(grants.len(), 3);
        assert_eq!(grants, task.payload_read_grants());
        assert!(grants
            .iter()
            .all(|path| path.parent() == Some(task.paths().control_dir.as_path())));
        assert!(!grants.contains(&task.paths().manifest));
        let sandbox_temp = storage.path().join("sandbox-temp");
        std::fs::create_dir_all(&sandbox_temp).unwrap();
        let b2_profile = SandboxProfile::build(
            vec![project.path().to_path_buf()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![task.paths().control_dir.clone()],
            Vec::new(),
            Vec::new(),
            sandbox_temp,
        )
        .unwrap();
        assert!(b2_profile
            .read_deny
            .contains(&std::fs::canonicalize(&task.paths().control_dir).unwrap()));
        assert!(grants.iter().all(|path| {
            path.parent() == Some(task.paths().control_dir.as_path())
                && path != &task.paths().manifest
        }));

        #[cfg(target_os = "linux")]
        {
            let refused = SpawnPlan::launcher_for_test(b2_profile, PathBuf::from("/usr/bin/false"))
                .with_prepared_task(task.clone());
            assert_eq!(refused.refusal_code(), Some("sandbox_unavailable"));
            assert!(refused
                .refusal_message()
                .is_some_and(|message| message.contains("mandatory read root")));

            let allowed_temp = storage.path().join("allowed-sandbox-temp");
            std::fs::create_dir_all(&allowed_temp).unwrap();
            let allowed_profile = SandboxProfile::build(
                vec![project.path().to_path_buf()],
                Vec::new(),
                Vec::new(),
                vec![project.path().to_path_buf()],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                allowed_temp,
            )
            .unwrap();
            let prepared =
                SpawnPlan::launcher_for_test(allowed_profile, PathBuf::from("/usr/bin/true"))
                    .with_prepared_task(task.clone());
            let profile = match prepared {
                SpawnPlan::Prepared { plan, .. } => match *plan {
                    SpawnPlan::Launcher { profile, .. } => profile,
                    other => panic!("expected launcher plan, got {other:?}"),
                },
                other => panic!("expected prepared plan, got {other:?}"),
            };
            let canonical_payloads = grants
                .iter()
                .map(|path| path.canonicalize().unwrap())
                .collect::<Vec<_>>();
            assert!(canonical_payloads
                .iter()
                .all(|path| profile.read_allow.contains(path)));
            assert!(profile.read_allow.iter().all(|path| {
                !path.starts_with(&task.paths().control_dir) || canonical_payloads.contains(path)
            }));
            validate_final_read_rules(&profile.read_allow, &profile.read_deny).unwrap();
        }

        assert_eq!(task.environment(), &environment);
    }

    #[cfg(unix)]
    #[test]
    fn writable_roots_refuse_both_session_store_overlap_directions() {
        fn profile(base: &Path, write_root: PathBuf) -> SandboxProfile {
            let project = base.join("project");
            let home = base.join("home");
            let temp = base.join("temp");
            for path in [&project, &home, &temp, &write_root] {
                std::fs::create_dir_all(path).unwrap();
            }
            SandboxProfile::build(
                vec![project, write_root],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                temp,
            )
            .unwrap()
        }

        let base = tempfile::tempdir().unwrap();
        let session = base.path().join("store/session");
        let io = session.join("bash-0000000000000001/io");
        let control = session.join("bash-0000000000000001/control");
        std::fs::create_dir_all(&io).unwrap();
        std::fs::create_dir_all(&control).unwrap();
        let canonical_session = std::fs::canonicalize(&session).unwrap();
        let canonical_io = std::fs::canonicalize(&io).unwrap();

        let ancestor = profile(base.path(), base.path().join("store"));
        assert!(refuse_store_overlap(&ancestor, &canonical_session, &canonical_io).is_err());
        let descendant = profile(base.path(), control);
        assert!(refuse_store_overlap(&descendant, &canonical_session, &canonical_io).is_err());
        let allowed = profile(base.path(), io);
        assert!(refuse_store_overlap(&allowed, &canonical_session, &canonical_io).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn enabled_host_request_is_refused_on_windows_without_a_grant() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let plan = resolve_sandbox_spawn(
            &ctx,
            &AuthenticatedPrincipal::FirstParty,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        assert_eq!(plan.refusal_code(), Some("sandbox_unavailable"));
        assert_eq!(
            plan.refusal_message(),
            Some(
                "sandbox is not supported on this platform; disable sandbox.enabled or run on macOS/Linux"
            )
        );
    }

    #[cfg(windows)]
    #[test]
    fn enabled_native_tier_is_refused_on_windows() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let plan = resolve_sandbox_spawn(
            &ctx,
            &AuthenticatedPrincipal::FirstParty,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        assert_eq!(plan.refusal_code(), Some("sandbox_unavailable"));
        assert_eq!(
            plan.refusal_message(),
            Some(
                "sandbox is not supported on this platform; disable sandbox.enabled or run on macOS/Linux"
            )
        );
    }
}

#[cfg(test)]
mod read_allow_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn vanished_listed_sibling_is_dropped_without_widening_read_access() {
        struct RemovingLister {
            parent: PathBuf,
            sibling: PathBuf,
            removed: bool,
        }
        impl ReadDirectoryLister for RemovingLister {
            fn children(&mut self, parent: &Path) -> std::io::Result<Vec<ListedReadChild>> {
                let children = SecureReadDirectoryLister.children(parent)?;
                if parent == self.parent {
                    assert!(children.iter().any(|child| child.path == self.sibling));
                    std::fs::remove_dir_all(&self.sibling).expect("remove listed sibling");
                    self.removed = true;
                }
                Ok(children)
            }
        }

        let root = tempfile::tempdir().expect("fixture");
        let root = root.path().canonicalize().expect("canonical fixture");
        let shared = root.join("shared");
        let vanished = shared.join("vanished");
        let kept = shared.join("kept");
        let denied = shared.join("denied");
        let project = root.join("project");
        let temp = root.join("task-temp");
        for path in [&vanished, &kept, &denied, &project, &temp] {
            std::fs::create_dir_all(path).expect("fixture directory");
        }
        let profile = SandboxProfile::build(
            vec![project.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![denied.clone()],
            Vec::new(),
            Vec::new(),
            temp.clone(),
        )
        .expect("base profile");
        let mut lister = RemovingLister {
            parent: shared.clone(),
            sibling: vanished.clone(),
            removed: false,
        };
        let profile = compute_linux_profile(profile, &shared, &[], &[], &mut lister)
            .expect("split shared read root");
        assert!(lister.removed, "deletion hook must run after enumeration");
        assert!(profile.read_allow.contains(&vanished));
        let listed_grants = profile.read_allow.clone();
        let profile = profile
            .canonicalize_for_launch()
            .expect("a vanished listed sibling must not refuse sandbox setup");
        assert_eq!(profile.read_allow, vec![project, kept, temp]);
        assert!(profile
            .read_allow
            .iter()
            .all(|path| listed_grants.contains(path)));
        assert!(!profile
            .read_allow
            .iter()
            .any(|path| denied.starts_with(path)));
        assert!(!profile.read_allow.contains(&shared));

        std::fs::create_dir(&vanished).expect("recreate removed sibling");
        let new_sibling = shared.join("new-sibling");
        std::fs::create_dir(&new_sibling).expect("create later sibling");
        for path in [&vanished, &new_sibling, &denied] {
            assert!(!profile
                .read_allow
                .iter()
                .any(|grant| path.starts_with(grant)));
        }
        let transferred: SandboxProfile =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        assert_eq!(transferred.canonicalize_for_launch().unwrap(), profile);
    }

    #[derive(Default)]
    struct FakeLister {
        entries: BTreeMap<PathBuf, Result<Vec<ListedReadChild>, (std::io::ErrorKind, String)>>,
    }

    impl FakeLister {
        fn directory(mut self, parent: &str, children: &[(&str, bool)]) -> Self {
            let parent = PathBuf::from(parent);
            self.entries.insert(
                parent.clone(),
                Ok(children
                    .iter()
                    .map(|(name, is_dir)| ListedReadChild {
                        path: parent.join(name),
                        is_dir: *is_dir,
                    })
                    .collect()),
            );
            self
        }

        fn failure(mut self, parent: &str, message: &str) -> Self {
            self.entries.insert(
                PathBuf::from(parent),
                Err((std::io::ErrorKind::Other, message.to_string())),
            );
            self
        }

        fn missing(mut self, parent: &str) -> Self {
            self.entries.insert(
                PathBuf::from(parent),
                Err((std::io::ErrorKind::NotFound, "directory vanished".into())),
            );
            self
        }
    }

    impl ReadDirectoryLister for FakeLister {
        fn children(&mut self, parent: &Path) -> std::io::Result<Vec<ListedReadChild>> {
            self.entries
                .get(parent)
                .cloned()
                .unwrap_or_else(|| {
                    Err((
                        std::io::ErrorKind::Other,
                        format!("unexpected enumeration of {}", parent.display()),
                    ))
                })
                .map_err(|(kind, message)| std::io::Error::new(kind, message))
        }
    }

    fn grant(path: &str, force_children: bool, mandatory: bool) -> IntendedReadGrant {
        IntendedReadGrant {
            path: PathBuf::from(path),
            force_children,
            mandatory,
        }
    }

    /// Canonical directory inventory independent of the host filesystem. The
    /// connection is a regular file outside CortexKit data, just as in the
    /// production spawn fixture. Private subtrees are intentionally absent from
    /// the inventory: policy may grant exact exceptions, never enumerate them.
    fn linux_profile_model() -> (SandboxProfile, FakeLister, Vec<IntendedReadGrant>) {
        let data = "/home/alice/.local/share/cortexkit";
        let storage = format!("{data}/aft");
        let project = format!("{data}/alfonso/worktrees/own");
        let session = format!("{storage}/opencode/bash-tasks/current");
        let temp = format!("{session}/bash-own/io/aft-sandbox-own");
        let shims = format!("{storage}/shims");
        let hooks = format!("{storage}/cache/git-hooks/current");
        let binary = format!("{data}/bin/ck-aft");
        let connection = PathBuf::from("/home/alice/daemon/connection.json");
        let profile = SandboxProfile {
            v: crate::sandbox_profile::SANDBOX_PROFILE_VERSION,
            data_policy: crate::sandbox_profile::SandboxDataPolicy {
                deny: vec![
                    data.into(),
                    "/home/alice/.local/state/cortexkit".into(),
                    storage.into(),
                    session.into(),
                ],
                read_allow: vec![
                    project.clone().into(),
                    temp.clone().into(),
                    shims.clone().into(),
                    hooks.clone().into(),
                    binary.clone().into(),
                ],
                write_allow: vec![project.clone().into(), temp.clone().into()],
                read_metadata_allow: Vec::new(),
            },
            writable_roots: vec![project.into()],
            writable_files: Vec::new(),
            write_deny: vec![
                connection.clone(),
                "/home/alice/.ssh".into(),
                shims.into(),
                hooks.into(),
                binary.into(),
            ],
            write_deny_nested: Vec::new(),
            read_allow: Vec::new(),
            read_allow_listed: Vec::new(),
            read_deny: vec![
                connection,
                "/home/alice/.ssh".into(),
                "/run/user".into(),
                "/run/credentials".into(),
                "/run/secrets".into(),
            ],
            socket_deny: Vec::new(),
            loopback_tcp_connect_deny: Vec::new(),
            cache_roots: vec!["/home/alice/.npm".into()],
            temp_dir: temp.into(),
        };
        let inventory = FakeLister::default()
            .directory(
                "/home/alice",
                &[
                    (".ssh", true),
                    (".local", true),
                    ("daemon", true),
                    ("notes", false),
                    (".npm", true),
                ],
            )
            .directory(
                "/home/alice/daemon",
                &[("connection.json", false), ("readme", false)],
            )
            .directory("/home/alice/.local", &[("share", true), ("state", true)])
            .directory(
                "/home/alice/.local/share",
                &[("cortexkit", true), ("other-program", true)],
            )
            .directory(
                "/home/alice/.local/state",
                &[("cortexkit", true), ("other-state", true)],
            )
            .directory(
                "/run",
                &[
                    ("user", true),
                    ("credentials", true),
                    ("secrets", true),
                    ("lock", true),
                ],
            );
        (
            profile,
            inventory,
            vec![grant("/usr", false, true), grant("/run", false, true)],
        )
    }

    #[test]
    fn linux_profile_private_tree_carve_outs_are_exact() {
        let (profile, mut inventory, base) = linux_profile_model();
        let profile = compute_linux_profile(
            profile,
            Path::new("/home/alice"),
            &[],
            &base,
            &mut inventory,
        )
        .expect("a worktree inside private data must produce a usable Linux profile");
        let mut expected = [
            "/usr", "/run/lock", "/home/alice/.npm", "/home/alice/notes", "/home/alice/daemon/readme",
            "/home/alice/.local/share/other-program", "/home/alice/.local/state/other-state",
            "/home/alice/.local/share/cortexkit/alfonso/worktrees/own",
            "/home/alice/.local/share/cortexkit/aft/opencode/bash-tasks/current/bash-own/io/aft-sandbox-own",
            "/home/alice/.local/share/cortexkit/aft/shims",
            "/home/alice/.local/share/cortexkit/aft/cache/git-hooks/current",
            "/home/alice/.local/share/cortexkit/bin/ck-aft",
        ].map(PathBuf::from).to_vec();
        expected.sort_unstable();
        assert_eq!(
            profile.read_allow, expected,
            "read/execute rules must be exact"
        );
        let writes = profile.write_allow_roots();
        assert_eq!(writes, vec![
            Path::new("/home/alice/.local/share/cortexkit/aft/opencode/bash-tasks/current/bash-own/io/aft-sandbox-own"),
            Path::new("/home/alice/.local/share/cortexkit/alfonso/worktrees/own"),
            Path::new("/home/alice/.npm"),
        ]);
        for deny in &profile.data_policy.deny {
            assert!(
                !profile
                    .read_allow
                    .iter()
                    .any(|grant| deny.starts_with(grant)),
                "a read grant covers private tree {}",
                deny.display()
            );
            assert!(
                !writes.iter().any(|grant| deny.starts_with(grant)),
                "a write grant covers private tree {}",
                deny.display()
            );
        }
        // These victim paths are deliberately outside every exact exception.
        for victim in [
            "/home/alice/daemon/connection.json",
            "/home/alice/.local/share/cortexkit/other-module/store",
            "/home/alice/.local/share/cortexkit/aft/undo/history",
            "/home/alice/.local/share/cortexkit/alfonso/worktrees/other/file",
            "/home/alice/.local/share/cortexkit/aft/opencode/bash-tasks/other/io/stdout",
            "/home/alice/.local/state/cortexkit/gh-shim/manifest.json",
        ] {
            assert!(
                !profile
                    .read_allow
                    .iter()
                    .any(|grant| Path::new(victim).starts_with(grant)),
                "readable victim: {victim}"
            );
            assert!(
                !writes
                    .iter()
                    .any(|grant| Path::new(victim).starts_with(grant)),
                "writable victim: {victim}"
            );
        }
        for managed in [
            "/home/alice/.local/share/cortexkit/aft/shims",
            "/home/alice/.local/share/cortexkit/aft/cache/git-hooks/current",
            "/home/alice/.local/share/cortexkit/bin/ck-aft",
        ] {
            assert!(
                profile.read_allow.contains(&PathBuf::from(managed)),
                "missing managed read/execute grant: {managed}"
            );
            assert!(
                !writes
                    .iter()
                    .any(|grant| paths_overlap(grant, Path::new(managed))),
                "managed executable writable: {managed}"
            );
        }
    }

    #[test]
    fn linux_profile_repository_hooks_preserve_existing_child_reads() {
        let (mut profile, mut inventory, base) = linux_profile_model();
        let project = "/home/alice/.local/share/cortexkit/alfonso/worktrees/own";
        let git = format!("{project}/.git");
        profile.read_deny.push(format!("{git}/hooks").into());
        inventory = inventory
            .directory(
                project,
                &[("tracked", false), ("src", true), (".git", true)],
            )
            .directory(
                &git,
                &[
                    ("hooks", true),
                    ("objects", true),
                    ("refs", true),
                    ("index", false),
                ],
            );
        let profile = compute_linux_profile(
            profile,
            Path::new("/home/alice"),
            &[git.into()],
            &base,
            &mut inventory,
        )
        .expect("nested hooks must not make the entire project unavailable");
        assert!(profile.write_allow_roots().contains(&Path::new(project)));
        for readable in ["tracked", "src", ".git/objects", ".git/refs", ".git/index"] {
            assert!(
                profile
                    .read_allow
                    .contains(&Path::new(project).join(readable)),
                "missing project child: {readable}"
            );
        }
        for denied in [".git/hooks/pre-commit", "new-at-project-root"] {
            let denied = Path::new(project).join(denied);
            assert!(
                !profile
                    .read_allow
                    .iter()
                    .any(|grant| denied.starts_with(grant)),
                "unexpected readable path: {}",
                denied.display()
            );
        }
    }

    #[test]
    fn linux_profile_broad_base_read_grant_is_split_around_private_data() {
        let (profile, mut inventory, mut base) = linux_profile_model();
        // Even another source of read access to the parent must be split, not
        // emitted wholesale alongside the precise private-tree exceptions.
        base.push(grant("/home/alice/.local/share", false, true));
        let profile = compute_linux_profile(
            profile,
            Path::new("/home/alice"),
            &[],
            &base,
            &mut inventory,
        )
        .expect("a broad base read grant can be split without refusing the worktree");
        assert!(profile.read_allow.contains(&PathBuf::from(
            "/home/alice/.local/share/cortexkit/alfonso/worktrees/own"
        )));
        assert!(profile
            .read_allow
            .contains(&PathBuf::from("/home/alice/.local/share/other-program")));
        assert!(!profile
            .read_allow
            .iter()
            .any(|grant| Path::new("/home/alice/.local/share/cortexkit").starts_with(grant)));
    }

    #[test]
    fn linux_profile_connection_file_inside_project_still_refuses() {
        let (mut profile, mut inventory, base) = linux_profile_model();
        let connection = profile.writable_roots[0].join("connection.json");
        profile.read_deny.push(connection.clone());
        profile.write_deny.push(connection);
        let error = compute_linux_profile(
            profile,
            Path::new("/home/alice"),
            &[],
            &base,
            &mut inventory,
        )
        .expect_err("private-tree exceptions must not override credential denies");
        assert!(error.contains("overlaps mandatory secret floor"), "{error}");
    }

    #[test]
    fn vanished_listed_deny_ancestor_is_omitted_during_split() {
        let mut lister = FakeLister::default()
            .directory("/shared", &[("vanished", true), ("kept", true)])
            .missing("/shared/vanished");
        let denies = vec![PathBuf::from("/shared/vanished/secret")];
        let (paths, listed) = split_read_grants_with_inventory(
            &[grant("/shared", true, false)],
            &denies,
            &mut lister,
        )
        .expect("a listed child disappearing before recursion only removes its grant");
        assert_eq!(paths, vec![PathBuf::from("/shared/kept")]);
        assert_eq!(listed, paths);
        assert!(
            split_read_grants(
                &[grant("/shared/vanished", true, false)],
                &denies,
                &mut lister,
            )
            .is_err(),
            "an explicitly requested missing directory still refuses"
        );
    }

    #[test]
    fn direct_read_grant_remains_required_when_also_listed() {
        let mut lister = FakeLister::default().directory("/shared", &[("required", false)]);
        for grants in [
            vec![
                grant("/shared", true, false),
                grant("/shared/required", false, false),
            ],
            vec![
                grant("/shared/required", false, false),
                grant("/shared", true, false),
            ],
        ] {
            let (paths, listed) =
                split_read_grants_with_inventory(&grants, &[], &mut lister).unwrap();
            assert_eq!(paths, vec![PathBuf::from("/shared/required")]);
            assert!(
                listed.is_empty(),
                "direct provenance wins regardless of grant order"
            );
        }
    }

    #[test]
    fn denied_connection_file_is_omitted_without_refusing_readable_siblings() {
        let mut lister = FakeLister::default().directory(
            "/home/alice/daemon",
            &[("connection.json", false), ("ordinary.txt", false)],
        );
        let emitted = split_read_grants(
            &[grant("/home/alice/daemon", false, false)],
            &[PathBuf::from("/home/alice/daemon/connection.json")],
            &mut lister,
        )
        .expect("an exact denied file must be omitted, not treated as a directory");
        assert_eq!(
            emitted,
            vec![PathBuf::from("/home/alice/daemon/ordinary.txt")]
        );
    }

    #[test]
    fn deny_chain_crossing_regular_file_still_refuses() {
        let mut lister =
            FakeLister::default().directory("/home/alice", &[("not-a-directory", false)]);
        let error = split_read_grants(
            &[grant("/home/alice", true, false)],
            &[PathBuf::from("/home/alice/not-a-directory/secret")],
            &mut lister,
        )
        .expect_err("a strict descendant deny cannot be represented through a regular file");
        assert!(
            error.contains("deny chain crosses non-directory path"),
            "{error}"
        );
    }

    #[test]
    fn read_grants_split_home_across_all_deny_chains() {
        let mut lister = FakeLister::default()
            .directory(
                "/home/alice",
                &[
                    (".ssh", true),
                    (".config", true),
                    ("work", true),
                    ("notes", false),
                ],
            )
            .directory(
                "/home/alice/.config",
                &[("gcloud", true), ("cortexkit", true), ("editor", true)],
            )
            .directory("/home/alice/work", &[("private", true), ("src", true)]);
        let denies = [
            "/home/alice/.ssh",
            "/home/alice/.config/gcloud",
            "/home/alice/.config/cortexkit",
            "/home/alice/work/private",
        ]
        .map(PathBuf::from);

        let emitted = split_read_grants(&[grant("/home/alice", true, false)], &denies, &mut lister)
            .expect("split HOME grants");

        assert_eq!(
            emitted,
            [
                "/home/alice/.config/editor",
                "/home/alice/notes",
                "/home/alice/work/src",
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn secure_enumeration_omits_home_child_symlinks() {
        let mut lister = FakeLister::default()
            .directory("/home/alice", &[("ordinary", true), ("plain-file", false)]);
        let emitted = split_read_grants(
            &[grant("/home/alice", true, false)],
            &[PathBuf::from("/home/alice/.ssh")],
            &mut lister,
        )
        .expect("split HOME grants");

        assert_eq!(
            emitted,
            ["/home/alice/ordinary", "/home/alice/plain-file"].map(PathBuf::from)
        );
        assert!(!emitted.iter().any(|path| path.ends_with("secret-link")));
    }

    #[test]
    fn enumeration_race_refuses_instead_of_weakening_the_floor() {
        let mut lister = FakeLister::default().failure("/home/alice", "entry disappeared");
        let error = split_read_grants(
            &[grant("/home/alice", true, false)],
            &[PathBuf::from("/home/alice/.ssh")],
            &mut lister,
        )
        .expect_err("racing enumeration must fail closed");

        assert!(error.contains("cannot split read root /home/alice"));
        assert!(error.contains("entry disappeared"));
    }

    #[test]
    fn mandatory_floor_rejects_equal_containing_and_nested_writable_roots() {
        let floor = vec![PathBuf::from("/home/alice/.ssh")];
        for writable in [
            Path::new("/home/alice/.ssh"),
            Path::new("/home/alice"),
            Path::new("/home/alice/.ssh/cache"),
        ] {
            let error = validate_mandatory_floor_overlap([writable], &floor)
                .expect_err("mandatory floor overlap must refuse");
            assert!(error.contains("overlaps mandatory secret floor"));
        }
        validate_mandatory_floor_overlap([Path::new("/home/alice/project")], &floor)
            .expect("disjoint writable root");
    }

    #[test]
    fn ordinary_read_deny_under_writable_root_is_split_not_refused() {
        let mut lister =
            FakeLister::default().directory("/project", &[("private", true), ("src", true)]);
        let writable_root = PathBuf::from("/project");
        let emitted = split_read_grants(
            &[IntendedReadGrant {
                path: writable_root.clone(),
                force_children: false,
                mandatory: false,
            }],
            &[PathBuf::from("/project/private")],
            &mut lister,
        )
        .expect("ordinary deny should be expressible");

        assert_eq!(emitted, vec![PathBuf::from("/project/src")]);
        assert_eq!(writable_root, PathBuf::from("/project"));
    }

    #[test]
    fn static_var_grant_splits_when_home_is_beneath_it() {
        let mut lister = FakeLister::default()
            .directory("/var", &[("home", true), ("log", true)])
            .directory("/var/home", &[("alice", true)])
            .directory("/var/home/alice", &[(".ssh", true), ("work", true)]);
        let emitted = split_read_grants(
            &[grant("/var", true, true)],
            &[PathBuf::from("/var/home/alice/.ssh")],
            &mut lister,
        )
        .expect("split /var around HOME floor");

        assert_eq!(
            emitted,
            ["/var/home/alice/work", "/var/log"].map(PathBuf::from)
        );
    }

    #[test]
    fn run_sensitive_directories_are_removed_by_canonical_deny_chain() {
        let mut lister = FakeLister::default().directory(
            "/run",
            &[
                ("lock", true),
                ("user", true),
                ("credentials", true),
                ("secrets", true),
            ],
        );
        let emitted = split_read_grants(
            &[grant("/run", false, true)],
            &[
                PathBuf::from("/run/user"),
                PathBuf::from("/run/credentials"),
                PathBuf::from("/run/secrets"),
            ],
            &mut lister,
        )
        .expect("split /run");

        assert_eq!(emitted, vec![PathBuf::from("/run/lock")]);
        assert!(!emitted.iter().any(|path| {
            path == Path::new("/run/credentials") || path == Path::new("/run/secrets")
        }));
    }

    #[test]
    fn final_validation_rejects_every_overlap_direction() {
        let deny = vec![PathBuf::from("/home/alice/.ssh")];
        for grant in [
            PathBuf::from("/home/alice"),
            PathBuf::from("/home/alice/.ssh"),
            PathBuf::from("/home/alice/.ssh/key"),
        ] {
            assert!(validate_final_read_rules(&[grant], &deny).is_err());
        }
        validate_final_read_rules(&[PathBuf::from("/home/alice/work")], &deny)
            .expect("disjoint final grant");
    }

    #[test]
    fn grant_beneath_ordinary_deny_is_dropped_but_mandatory_grant_refuses() {
        let deny = vec![PathBuf::from("/restricted")];
        let mut lister = FakeLister::default();
        let emitted = split_read_grants(
            &[grant("/restricted/project", false, false)],
            &deny,
            &mut lister,
        )
        .expect("ordinary grant is optional");
        assert!(emitted.is_empty());

        let error = split_read_grants(
            &[grant("/restricted/system", false, true)],
            &deny,
            &mut lister,
        )
        .expect_err("mandatory grant under deny must refuse");
        assert!(error.contains("mandatory read root"));
    }

    #[cfg(unix)]
    #[test]
    fn linked_worktree_resolves_common_git_dir_and_shared_hooks() {
        // Neutralize ambient co_author hooksPath env injection (highest git
        // config scope) so the worktree's own hooks resolution decides.
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let fixture = tempfile::tempdir().expect("fixture");
        let main = fixture.path().join("main");
        let worktree = fixture.path().join("linked");
        std::fs::create_dir(&main).expect("main repository");
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&main)
            .status()
            .expect("git init")
            .success());
        std::fs::write(main.join("tracked"), b"tracked").expect("tracked file");
        assert!(Command::new("git")
            .args(["add", "tracked"])
            .current_dir(&main)
            .status()
            .expect("git add")
            .success());
        assert!(Command::new("git")
            .args([
                "-c",
                "user.name=AFT Test",
                "-c",
                "user.email=aft@example.invalid",
                "commit",
                "-qm",
                "initial",
            ])
            .current_dir(&main)
            .status()
            .expect("git commit")
            .success());
        assert!(Command::new("git")
            .args(["worktree", "add", "-q"])
            .arg(&worktree)
            .arg("HEAD")
            .current_dir(&main)
            .status()
            .expect("git worktree add")
            .success());

        let policy = resolve_git_policy(&worktree).expect("resolve linked worktree policy");
        let common = main.join(".git").canonicalize().expect("common git dir");
        assert_eq!(policy.hooks, vec![common.join("hooks")]);
        #[cfg(target_os = "linux")]
        assert!(policy.read_roots.contains(&common));
    }

    #[cfg(unix)]
    #[test]
    fn configured_hooks_path_is_resolved_to_its_effective_location() {
        // Neutralize ambient co_author hooksPath env injection (highest git
        // config scope) so the temp repo's local core.hooksPath decides.
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        std::fs::create_dir(&project).expect("project");
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project)
            .status()
            .expect("git init")
            .success());
        assert!(Command::new("git")
            .args(["config", "core.hooksPath", "custom-hooks"])
            .current_dir(&project)
            .status()
            .expect("git config")
            .success());

        let policy = resolve_git_policy(&project).expect("resolve configured hooks path");
        assert_eq!(
            policy.hooks,
            vec![project
                .canonicalize()
                .expect("canonical project")
                .join("custom-hooks")]
        );
    }
}
