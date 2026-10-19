//! Grant operations and launch validation use daemon-attested route ancestry.
use super::*;
use crate::access_policy::{
    policy_path, AccessGrant, AccessOperation, AccessPolicy, GrantKind, HeadPolicy,
};
use crate::db::access_grants::{self as store, ScopeKey};

fn refusal(code: &str, message: impl Into<String>) -> subc_protocol::ErrorBody {
    subc_protocol::ErrorBody::new(code, message.into())
}

pub(super) fn scope_key(identity: &RouteIdentity) -> ScopeKey {
    if let Some(stamp) = identity
        .scope
        .as_ref()
        .filter(|stamp| stamp.owner_authorized)
    {
        return ScopeKey::from_stamp(stamp);
    }
    ScopeKey {
        owner: "session".into(),
        scope_ref: format!(
            "session:{}",
            json!([
                identity.project_root,
                identity.harness,
                identity.session,
                principal_label_for_identity(identity)
            ])
        ),
        epoch: 0,
    }
}

/// A codemode run has its own lifetime, but uses its agent's authority. Keep
/// grant checks on the attested parent identity instead of copying grants into
/// the run, so revocation remains effective and ending a run retains agent grants.
pub(super) fn policy_scope_key(identity: &RouteIdentity) -> ScopeKey {
    if let Some(stamp) = identity
        .scope
        .as_ref()
        .filter(|stamp| stamp.owner_authorized)
    {
        if stamp.kind == subc_protocol::scope::ScopeKind::Ephemeral {
            if let Some(parent) = stamp.parent.as_ref() {
                return ScopeKey {
                    owner: serde_json::to_string(&parent.owner).expect("scope owner serializes"),
                    scope_ref: parent.scope_ref.clone(),
                    epoch: parent.scope_epoch,
                };
            }
        }
    }
    scope_key(identity)
}

fn principal_label_for_identity(identity: &RouteIdentity) -> Option<&str> {
    match &identity.spawn_principal {
        AuthenticatedPrincipal::RouteBind { principal_id, .. } => principal_id.as_deref(),
        _ => None,
    }
}

pub(super) fn register(
    identity: &RouteIdentity,
    ctx: &AppContext,
) -> Result<(), subc_protocol::ErrorBody> {
    let Some(db) = ctx.db() else {
        return Err(refusal(
            "grants_unavailable",
            "scope persistence unavailable",
        ));
    };
    let conn = db
        .lock()
        .map_err(|_| refusal("grants_unavailable", "grant database lock poisoned"))?;
    register_on(&conn, identity, ctx)
}

/// Woken after each scope registration commits. A flow read can reach AFT
/// before its parent head's queued registration has run; it waits on this
/// instead of refusing a read the parent's policy would allow.
pub(super) fn scope_registered() -> &'static tokio::sync::Notify {
    static REGISTERED: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    REGISTERED.get_or_init(tokio::sync::Notify::new)
}

fn register_on(
    conn: &rusqlite::Connection,
    identity: &RouteIdentity,
    ctx: &AppContext,
) -> Result<(), subc_protocol::ErrorBody> {
    register_scope_rows(conn, identity, ctx)?;
    scope_registered().notify_waiters();
    Ok(())
}

fn register_scope_rows(
    conn: &rusqlite::Connection,
    identity: &RouteIdentity,
    ctx: &AppContext,
) -> Result<(), subc_protocol::ErrorBody> {
    if identity
        .scope
        .as_ref()
        .is_some_and(|stamp| !stamp.owner_authorized)
    {
        return Err(refusal(
            "grants_scope_refused",
            "scope owner is not authorized",
        ));
    }
    let mut config = ctx.config().as_ref().clone();
    config.project_root = Some(identity.project_root.clone());
    let parent = identity
        .scope
        .as_ref()
        .and_then(|s| s.parent.as_ref())
        .map(|parent| ScopeKey {
            owner: serde_json::to_string(&parent.owner).expect("scope owner serializes"),
            scope_ref: parent.scope_ref.clone(),
            epoch: parent.scope_epoch,
        });
    store::register_scope(conn, &scope_key(identity), parent.as_ref(), &config)
        .map_err(|e| refusal("grants_unavailable", e.to_string()))?;
    if let Some(stamp) = identity.scope.as_ref().filter(|s| alfonso(s)) {
        if stamp.kind == subc_protocol::scope::ScopeKind::Worker {
            store::register_worker(conn, &scope_key(identity))
                .map_err(|e| refusal("grants_unavailable", e.to_string()))?;
        }
        if matches!(
            stamp.kind,
            subc_protocol::scope::ScopeKind::Head | subc_protocol::scope::ScopeKind::Ephemeral
        ) {
            // Preserve the attested role even when no agent id was supplied.
            // Empty ids are never used to link a run to another session.
            store::register_agent(
                conn,
                &scope_key(identity),
                stamp.attributes.agent_id.as_deref().unwrap_or(""),
                stamp.kind == subc_protocol::scope::ScopeKind::Head,
            )
            .map_err(|e| refusal("grants_unavailable", e.to_string()))?;
        }
    }
    Ok(())
}

pub(super) fn operation(
    body: Value,
    op: &str,
    identity: &RouteIdentity,
    ctx: &AppContext,
) -> Result<Value, subc_protocol::ErrorBody> {
    let Some(db) = ctx.db() else {
        return Err(refusal(
            "grants_unavailable",
            "scope persistence unavailable",
        ));
    };
    let conn = db
        .lock()
        .map_err(|_| refusal("grants_unavailable", "grant database lock poisoned"))?;
    register_on(&conn, identity, ctx)?;
    let caller = scope_key(identity);
    let requested = body.get("scope");
    let target_ref = match requested {
        None => caller.scope_ref.as_str(),
        Some(Value::String(name)) => name,
        Some(Value::Object(object)) => object
            .get("ref")
            .and_then(Value::as_str)
            .ok_or_else(|| refusal("grants_scope_refused", "scope requires a ref"))?,
        _ => return Err(refusal("grants_scope_refused", "scope requires a ref")),
    };
    let target_owner = requested
        .and_then(|v| v.get("owner"))
        .map(|v| {
            serde_json::from_value::<Principal>(v.clone())
                .map(|owner| serde_json::to_string(&owner).expect("scope owner serializes"))
        })
        .transpose()
        .map_err(|_| refusal("grants_scope_refused", "invalid scope owner"))?
        .unwrap_or_else(|| caller.owner.clone());
    let mut target = store::resolve_scope(&conn, &target_owner, target_ref)
        .map_err(|e| refusal("grants_unavailable", e.to_string()))?
        .ok_or_else(|| {
            refusal(
                "grants_scope_refused",
                format!("scope {target_ref} is not owned by this caller or its child"),
            )
        })?;
    if let Some(epoch) = requested.and_then(|v| v.get("scope_epoch")) {
        target.epoch = epoch
            .as_u64()
            .ok_or_else(|| refusal("grants_scope_refused", "invalid scope epoch"))?;
    }
    if identity.scope.as_ref().is_some_and(|s| !s.owner_authorized)
        || !store::authorized(&conn, &caller, &target)
            .map_err(|e| refusal("grants_unavailable", e.to_string()))?
    {
        return Err(refusal(
            "grants_scope_refused",
            format!(
                "caller {} may not manage scope {target_ref}",
                caller.scope_ref
            ),
        ));
    }
    match op {
        "grants.list" => Ok(
            json!({"scope": target.scope_ref, "scope_epoch": target.epoch, "grants": store::list(&conn,&target).map_err(|e| refusal("grants_unavailable",e.to_string()))?}),
        ),
        "grants.revoke" => {
            let raw = body
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| refusal("grants_path_refused", "path must be absolute"))?;
            if !Path::new(raw).is_absolute() {
                return Err(refusal("grants_path_refused", "path must be absolute"));
            }
            let path = policy_path(Path::new(raw));
            let removed = store::revoke(&conn, &target, &path)
                .map_err(|e| refusal("grants_unavailable", e.to_string()))?;
            Ok(
                json!({"scope":target.scope_ref,"scope_epoch":target.epoch,"path":path,"revoked":removed}),
            )
        }
        _ => Err(refusal(
            "unsupported_operation",
            "unsupported grant operation",
        )),
    }
}

fn alfonso(stamp: &subc_protocol::scope::ScopeStamp) -> bool {
    stamp.owner_authorized
        && matches!(&stamp.owner, Principal::Reserved { module_id } if module_id == "prefrontal-core")
        && matches!(
            stamp.kind,
            subc_protocol::scope::ScopeKind::Head
                | subc_protocol::scope::ScopeKind::Worker
                | subc_protocol::scope::ScopeKind::Ephemeral
        )
}

pub(super) fn head_scope(identity: &RouteIdentity) -> Option<ScopeKey> {
    let stamp = identity.scope.as_ref().filter(|s| alfonso(s))?;
    let own = ScopeKey::from_stamp(stamp);
    match stamp.kind {
        subc_protocol::scope::ScopeKind::Head => Some(own),
        _ => match stamp.parent.as_ref() {
            Some(parent) => Some(ScopeKey {
                owner: serde_json::to_string(&parent.owner).expect("scope owner serializes"),
                scope_ref: parent.scope_ref.clone(),
                epoch: parent.scope_epoch,
            }),
            None => None,
        },
    }
}

/// Whether this route's calls are a stamped Alfonso worker's: a `Worker`
/// scope, or an `Ephemeral` (codemode run) scope that names a parent. Heads'
/// runs carry no parent, so a parented run is a worker's run and is judged as
/// that worker.
pub(super) fn alfonso_worker(identity: &RouteIdentity) -> bool {
    identity.scope.as_ref().is_some_and(|stamp| {
        alfonso(stamp)
            && match stamp.kind {
                subc_protocol::scope::ScopeKind::Worker => true,
                subc_protocol::scope::ScopeKind::Ephemeral => stamp.parent.is_some(),
                _ => false,
            }
    })
}

/// Routes attested as prefrontal-core heads, workers or runs send access asks
/// to Cingulate, the consent service, never to a plugin prompt. Other routes
/// retain compatibility behavior: an outside-root ask is treated as allow.
pub(super) fn ask_route(identity: &RouteIdentity) -> crate::access_policy::AskRoute {
    if identity.scope.as_ref().is_some_and(alfonso) {
        crate::access_policy::AskRoute::Consent { once: Vec::new() }
    } else {
        crate::access_policy::AskRoute::Lowered
    }
}

/// The head whose authority a worker's ask is judged against. A worker's
/// stamp names its head as `parent`. A worker's run names the worker, whose
/// registered parent is the head.
pub(super) fn consent_head(
    conn: &rusqlite::Connection,
    identity: &RouteIdentity,
) -> rusqlite::Result<Option<ScopeKey>> {
    let Some(stamp) = identity.scope.as_ref().filter(|s| alfonso(s)) else {
        return Ok(None);
    };
    let Some(parent) = stamp.parent.as_ref() else {
        return Ok(None);
    };
    let parent = ScopeKey {
        owner: serde_json::to_string(&parent.owner).expect("scope owner serializes"),
        scope_ref: parent.scope_ref.clone(),
        epoch: parent.scope_epoch,
    };
    match stamp.kind {
        subc_protocol::scope::ScopeKind::Worker => Ok(Some(parent)),
        subc_protocol::scope::ScopeKind::Ephemeral => store::parent(conn, &parent),
        _ => Ok(None),
    }
}

pub(super) fn launch_key(
    identity: &RouteIdentity,
    preset: &str,
) -> Option<crate::db::remote_exec::PolicyKey> {
    identity
        .scope
        .as_ref()
        .filter(|s| alfonso(s) && s.kind == subc_protocol::scope::ScopeKind::Worker)?;
    remote_policy::key(identity, Some(preset))
}

/// Build a scope's policy while holding the connection's lock throughout.
/// Production paths take a `PolicySnapshot` under the lock and resolve it
/// after releasing the lock instead; tests use this as a shorthand.
#[cfg(test)]
pub(super) fn policy_for_scope(
    conn: &rusqlite::Connection,
    key: &ScopeKey,
    ctx: &AppContext,
) -> rusqlite::Result<Option<AccessPolicy>> {
    Ok(policy_snapshot(conn, key)?.map(|snapshot| snapshot.resolve(&ctx.storage_dir())))
}

#[derive(Clone)]
pub(super) struct PolicySnapshot {
    config: Config,
    mode: crate::access_policy::ApprovalMode,
    grants: Vec<AccessGrant>,
    worker: bool,
}

/// Two snapshots are equal when they hold the same stored authority. `Config`
/// has no `PartialEq`, so it is compared through its serialized form; a
/// config that cannot be serialized compares unequal, which only makes a
/// caller re-read the snapshot.
impl PartialEq for PolicySnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.worker == other.worker
            && self.grants == other.grants
            && match (
                serde_json::to_value(&self.config),
                serde_json::to_value(&other.config),
            ) {
                (Ok(left), Ok(right)) => left == right,
                _ => false,
            }
    }
}

/// Copy database-backed authority without probing any filesystem paths.
pub(super) fn policy_snapshot(
    conn: &rusqlite::Connection,
    key: &ScopeKey,
) -> rusqlite::Result<Option<PolicySnapshot>> {
    let Some(config) = store::scope_config(conn, key)? else {
        return Ok(None);
    };
    Ok(Some(PolicySnapshot {
        config,
        mode: store::mode(conn, key)?,
        grants: store::list(conn, key)?,
        worker: store::is_worker(conn, key)?,
    }))
}

impl PolicySnapshot {
    /// Resolve filesystem-backed path identities separately from the database snapshot.
    pub(super) fn resolve(self, storage: &Path) -> AccessPolicy {
        #[cfg(test)]
        unlocked_hook::run(unlocked_hook::Point::Resolve);
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let roots = self
            .config
            .project_root
            .as_ref()
            .map(|root| vec![policy_path(root)])
            .unwrap_or_default();
        let mut policy = AccessPolicy::from_config(&self.config, storage, roots, &home);
        policy.mode = self.mode;
        // Worker standing reads resolve daemon-owned roots on disk, so they
        // are installed here rather than while the database lock is held.
        policy.install_scope_grants(self.grants, self.worker);
        policy
    }
}

/// Tests observe the steps that run with the aft.db lock released, on their
/// own thread: to check that the lock really is free there, or to change the
/// stored policy at that moment as a concurrent route could.
#[cfg(test)]
pub(super) mod unlocked_hook {
    use std::cell::RefCell;

    /// Where the hook was called from.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(in crate::subc) enum Point {
        /// `PolicySnapshot::resolve`, before it probes the filesystem.
        Resolve,
        /// `catalog`, after it read the stored launch parameters and before
        /// it validates them.
        LaunchValidation,
    }

    thread_local! {
        static HOOK: RefCell<Option<Box<dyn Fn(Point)>>> = const { RefCell::new(None) };
    }

    /// Removes the hook when dropped.
    pub(in crate::subc) struct Installed(());

    impl Drop for Installed {
        fn drop(&mut self) {
            HOOK.with(|hook| hook.borrow_mut().take());
        }
    }

    pub(in crate::subc) fn install(hook: impl Fn(Point) + 'static) -> Installed {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Installed(())
    }

    pub(super) fn run(point: Point) {
        HOOK.with(|hook| {
            if let Some(hook) = hook.borrow().as_ref() {
                hook(point);
            }
        });
    }
}

/// The policy of the head a worker's consent check is judged against, as
/// stored: `None` when the worker has no head or the head has no scope row.
#[derive(Clone, Default, PartialEq)]
pub(super) struct HeadSnapshot {
    key: Option<ScopeKey>,
    policy: Option<PolicySnapshot>,
}

impl HeadSnapshot {
    /// Probes the filesystem: call it with the aft.db lock released.
    pub(super) fn resolve(&self, storage: &Path) -> Option<AccessPolicy> {
        self.policy.clone().map(|policy| policy.resolve(storage))
    }
}

/// Read the stored policy of the head that `consent_head` names.
pub(super) fn consent_head_snapshot(
    conn: &rusqlite::Connection,
    identity: &RouteIdentity,
) -> rusqlite::Result<HeadSnapshot> {
    let Some(key) = consent_head(conn, identity)? else {
        return Ok(HeadSnapshot::default());
    };
    let policy = policy_snapshot(conn, &key)?;
    Ok(HeadSnapshot {
        key: Some(key),
        policy,
    })
}

/// Run `commit` under the aft.db lock with what `resolve` derived from a
/// database snapshot taken by `read`, without holding the lock while
/// `resolve` probes the filesystem.
///
/// The snapshot is taken under the lock, `resolve` runs after it is
/// released, and `commit` runs only once a fresh `read` under the lock again
/// matches the resolved snapshot. The result is therefore the same as doing
/// all three under one lock: if another route changed the stored policy in
/// between, the new snapshot is resolved and checked again. Each retry needs
/// such a concurrent change, so the loop ends once the stored policy stops
/// changing.
pub(super) fn resolve_unlocked<S: PartialEq, R, T, E>(
    db: &std::sync::Mutex<crate::db::TrackedConnection>,
    read: impl Fn(&rusqlite::Connection) -> Result<S, E>,
    resolve: impl Fn(&S) -> R,
    commit: impl FnOnce(&rusqlite::Connection, R) -> Result<T, E>,
    poisoned: impl Fn() -> E,
) -> Result<T, E> {
    let mut snapshot = {
        let conn = db.lock().map_err(|_| poisoned())?;
        read(&conn)?
    };
    loop {
        let resolved = resolve(&snapshot);
        let conn = db.lock().map_err(|_| poisoned())?;
        let current = read(&conn)?;
        if current == snapshot {
            return commit(&conn, resolved);
        }
        snapshot = current;
    }
}

fn param_refused(index: usize, path: Value, reason: &str) -> subc_protocol::ErrorBody {
    let mut error = refusal(
        "plan_param_refused",
        format!("allow_paths[{index}]: {reason}"),
    );
    error.detail = Some(json!({"param":"allow_paths","index":index,"path":path,"reason":reason}));
    error
}

/// Launch entries are subtree capabilities. Checking only the entry's root would
/// silently delegate a denied or not-held descendant to the worker.
pub(super) fn validate(
    raw: &Value,
    head: Option<&AccessPolicy>,
) -> Result<Vec<AccessGrant>, subc_protocol::ErrorBody> {
    let list = raw
        .as_array()
        .ok_or_else(|| param_refused(0, Value::Null, "malformed"))?;
    if list.len() > 64 {
        return Err(param_refused(
            64,
            list[64].get("path").cloned().unwrap_or(Value::Null),
            "malformed",
        ));
    }
    let mut grants: Vec<AccessGrant> = Vec::new();
    for (index, entry) in list.iter().enumerate() {
        let path_value = entry.get("path").cloned().unwrap_or(Value::Null);
        let malformed = || param_refused(index, path_value.clone(), "malformed");
        let object = entry.as_object().ok_or_else(malformed)?;
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(malformed)?;
        let mode = match object.get("mode").and_then(Value::as_str) {
            Some("read") => AccessOperation::Read,
            Some("write") => AccessOperation::Write,
            _ => return Err(malformed()),
        };
        // A leading `~` is never expanded: `~/x` is not absolute and is refused
        // below. A `~` inside an absolute path is a literal name, as in Windows
        // short names (C:\Users\RUNNER~1), so it is not malformed.
        if object.len() != 2
            || path.is_empty()
            || path.contains(['*', '?', '[', ']', '{', '}', '\0'])
            || Path::new(path)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(malformed());
        }
        if !Path::new(path).is_absolute() {
            return Err(param_refused(index, path_value, "not_absolute"));
        }
        let grant = AccessGrant {
            path: policy_path(Path::new(path)),
            kind: GrantKind::Subtree,
            operation: mode,
        };
        let Some(head) = head else {
            return Err(param_refused(index, path_value, "head_not_held"));
        };
        if let Some(floor) = head.capability_floor(mode, GrantKind::Subtree, &grant.path) {
            let mut error = param_refused(index, path_value, "denied_floor");
            error.message.push_str(&format!(
                " ({}: {})",
                floor.rule.description(),
                floor.path.display()
            ));
            if let Some(detail) = error.detail.as_mut() {
                detail["floor_rule"] = json!(floor.rule.label());
                detail["floor_path"] = json!(floor.path);
            }
            return Err(error);
        }
        if head.head_policy(&grant) != HeadPolicy::Allow {
            return Err(param_refused(index, path_value, "head_not_held"));
        }
        if let Some(prior) = grants.iter_mut().find(|g| g.path == grant.path) {
            if mode == AccessOperation::Write {
                prior.operation = mode;
            }
        } else {
            grants.push(grant);
        }
    }
    Ok(grants)
}

pub(super) fn catalog(
    raw: Option<&Value>,
    identity: &RouteIdentity,
    ctx: &AppContext,
    preset: &str,
) -> Result<(), subc_protocol::ErrorBody> {
    if identity
        .scope
        .as_ref()
        .is_some_and(|stamp| !stamp.owner_authorized)
    {
        if let Some(raw) = raw {
            validate(raw, None)?;
        }
        return Ok(());
    }
    if raw.is_none() && launch_key(identity, preset).is_none() {
        return Ok(());
    }
    let Some(db) = ctx.db() else {
        if let Some(raw) = raw {
            validate(raw, None)?;
        }
        return Ok(());
    };
    let lock = || {
        db.lock()
            .map_err(|_| refusal("grants_unavailable", "grant database lock poisoned"))
    };
    let head_key = head_scope(identity);
    let read_head = |conn: &rusqlite::Connection| {
        head_key
            .as_ref()
            .map(|key| policy_snapshot(conn, key))
            .transpose()
            .map(Option::flatten)
            .map_err(|e| refusal("grants_unavailable", e.to_string()))
    };
    let launch = launch_key(identity, preset);
    let launch_params = |conn: &rusqlite::Connection, key: &crate::db::remote_exec::PolicyKey| {
        store::launch_params(conn, key).map_err(|e| refusal("grants_unavailable", e.to_string()))
    };
    let storage = ctx.storage_dir();
    // The head's policy and `validate` both resolve paths on disk, so they
    // run with the lock released. Each later locked step first checks that
    // the head's stored policy is unchanged, and starts over with the new
    // one if it changed; the materialized grants are therefore those a
    // single locked pass would produce. A refusal from `validate` writes
    // nothing and is the answer for the snapshot it was checked against.
    let mut snapshot = {
        let conn = lock()?;
        register_on(&conn, identity, ctx)?;
        read_head(&conn)?
    };
    loop {
        let head = snapshot.clone().map(|policy| policy.resolve(&storage));
        if let Some(raw) = raw {
            validate(raw, head.as_ref())?;
        }
        let Some(key) = launch.as_ref() else {
            return Ok(());
        };
        let stored = {
            let conn = lock()?;
            let current = read_head(&conn)?;
            if current != snapshot {
                snapshot = current;
                continue;
            }
            launch_params(&conn, key)?
        };
        let empty = json!([]);
        let chosen = stored.as_ref().or(raw).unwrap_or(&empty);
        #[cfg(test)]
        unlocked_hook::run(unlocked_hook::Point::LaunchValidation);
        let grants = validate(chosen, head.as_ref())?;
        let conn = lock()?;
        let current = read_head(&conn)?;
        if current != snapshot {
            snapshot = current;
            continue;
        }
        // Another launch can store this scope's parameters first; validate
        // those instead.
        if launch_params(&conn, key)? != stored {
            continue;
        }
        store::materialize(&conn, key, chosen, &grants)
            .map_err(|e| refusal("grants_unavailable", e.to_string()))?;
        return Ok(());
    }
}

#[cfg(test)]
#[path = "access_grants_tests.rs"]
mod tests;
