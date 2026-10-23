//! Codemode runs: a program that BASAL runs for a head agent calls AFT tools
//! over its own run route. The daemon stamps that route with the run's scope:
//! `attributes.agent_id`, `attributes.run_id`, a `parent` link to the head
//! scope that started the run, and `parent_state`.
//!
//! Every tool call on a run route runs with the head agent's own access and
//! never more. The stamped parent link is used only to find the head scope.
//! AFT then finds its own live route bound under that scope, requires that
//! route's stamp to carry the same `agent_id` as the run, and runs the call
//! with that route's root, session, policy, grants, config, disabled tools,
//! sandbox profile and confinement. The run route's own bind root and session
//! are configuration, never authority, and so are the call's arguments.
//!
//! A run with no such head route, a parent link that is not `linked`, or a
//! different `agent_id` is refused with `run_owner_not_bound`.

use super::*;

/// Refusal for a codemode call whose head agent's access can't be located.
pub(super) const RUN_OWNER_NOT_BOUND: &str = "run_owner_not_bound";

/// The head agent a codemode call acts for, resolved from the run's stamp.
#[derive(Debug)]
pub(super) struct RunOwner {
    pub(super) agent_id: String,
    pub(super) run_id: String,
    /// The head agent's own route. Authority keyed by route identity (grant
    /// scope, confinement key) is read from it, never from the run route.
    pub(super) head: RouteIdentity,
    /// The run route the call arrived on, for audit lines.
    pub(super) run_route: RouteChannel,
    /// The run route's own principal. It grants nothing: the call runs with
    /// the head route's principal. It only names the call-ledger carrier, so
    /// the call's ledger rows are found again by `late_results` and
    /// `tool.withdraw` on the run route, which answer for that principal.
    pub(super) run_principal: AuthenticatedPrincipal,
}

impl RunOwner {
    /// The carrier this call's call-ledger rows and shell tasks are recorded
    /// under: the run route's, never the head's.
    pub(super) fn ledger_carrier(&self) -> String {
        tool_provider::ledger_carrier(&self.run_principal)
    }
}

/// How a request on a route is admitted with respect to codemode runs.
pub(super) enum Admission {
    /// Not a codemode tool call: the route's own identity applies.
    Unchanged,
    /// A codemode tool call, to run with this identity built from the head
    /// agent's route.
    Owner(RouteIdentity),
    /// A codemode tool call that must not run. The error is terminal.
    Refused(subc_protocol::ErrorBody),
}

fn not_bound(
    reason: &str,
    message: impl Into<String>,
    agent_id: Option<&str>,
    run_id: Option<&str>,
) -> subc_protocol::ErrorBody {
    subc_protocol::ErrorBody::new(RUN_OWNER_NOT_BOUND, message).with_detail(json!({
        "reason": reason,
        "agent_id": agent_id,
        "run_id": run_id,
    }))
}

/// Whether a request body is a tool call rather than a provider operation
/// (`tool.catalog`, `late_results`, ...). Provider operations on a run route
/// keep the run route's identity; only tool calls act for the head agent.
fn tool_call_name(body: &Value) -> Option<&str> {
    if body
        .get("op")
        .and_then(Value::as_str)
        .is_some_and(|op| op != "tool.call")
    {
        return None;
    }
    let name = body.get("name").and_then(Value::as_str).unwrap_or("");
    (!tool_provider::recognized_operation(name)).then_some(name)
}

/// Decide how the request on `route` is admitted. Only codemode tool calls
/// are affected. The order follows the contract: whether the tool serves
/// codemode at all, then whether the run's stamp names a live head route for
/// the same agent.
pub(super) fn admit(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    body: Option<&Value>,
) -> Admission {
    let Some(run) = routes.get(&route) else {
        return Admission::Unchanged;
    };
    if run.call_mode != subc_protocol::call_mode::CallMode::Codemode {
        return Admission::Unchanged;
    }
    let Some(name) = body.and_then(tool_call_name) else {
        return Admission::Unchanged;
    };
    if let Err(error) = tool_provider::admit_call_mode(name, &run.call_mode) {
        return Admission::Refused(error);
    }
    match resolve_owner(routes, route, run) {
        Ok(effective) => Admission::Owner(effective),
        Err(error) => {
            crate::slog_warn!(
                "codemode call refused: tool={name} route={}/{} code={} detail={}",
                route.channel,
                route.epoch,
                error.code,
                error.detail.as_ref().unwrap_or(&Value::Null)
            );
            Admission::Refused(error)
        }
    }
}

/// Copy of `routes` in which `route` carries the head-derived identity, so
/// every admission step after this one sees the head agent's access.
pub(super) fn overlay(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    effective: RouteIdentity,
) -> HashMap<RouteChannel, RouteIdentity> {
    let mut routes = routes.clone();
    routes.insert(route, effective);
    routes
}

fn resolve_owner(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    run: &RouteIdentity,
) -> Result<RouteIdentity, subc_protocol::ErrorBody> {
    use subc_protocol::scope::{ParentState, ScopeKind};
    let Some(stamp) = run.scope.as_ref() else {
        return Err(not_bound(
            "run_scope_missing",
            "codemode route carries no run scope",
            None,
            None,
        ));
    };
    let agent_id = stamp
        .attributes
        .agent_id
        .as_deref()
        .filter(|id| !id.is_empty());
    let run_id = stamp
        .attributes
        .run_id
        .as_deref()
        .filter(|id| !id.is_empty());
    let (Some(agent_id), Some(run_id)) = (agent_id, run_id) else {
        return Err(not_bound(
            "run_stamp_incomplete",
            "the run's stamp does not name both its agent and its run",
            agent_id,
            run_id,
        ));
    };
    if !stamp.owner_authorized {
        return Err(not_bound(
            "run_owner_unauthorized",
            "the run scope's owner is not authorized to open runs",
            Some(agent_id),
            Some(run_id),
        ));
    }
    let Some(parent) = stamp.parent.as_ref() else {
        return Err(not_bound(
            "parent_missing",
            "the run's stamp carries no parent link to a head scope",
            Some(agent_id),
            Some(run_id),
        ));
    };
    // A pending link is unverified and grants nothing; an ended one is final.
    // Neither may locate the head, so no new call runs on them.
    match stamp.parent_state {
        Some(ParentState::Linked) => {}
        state => {
            let reason = match state {
                Some(ParentState::Pending) => "parent_pending",
                Some(ParentState::Ended) => "parent_ended",
                _ => "parent_state_missing",
            };
            return Err(not_bound(
                reason,
                "the run's parent link is not linked to a live head scope",
                Some(agent_id),
                Some(run_id),
            ));
        }
    }
    let mut heads: Vec<(&RouteChannel, &RouteIdentity)> = routes
        .iter()
        .filter(|(channel, identity)| {
            **channel != route
                && identity.run_owner.is_none()
                && identity.call_mode == subc_protocol::call_mode::CallMode::Classic
                && identity.scope.as_ref().is_some_and(|head| {
                    head.kind == ScopeKind::Head
                        && head.owner_authorized
                        && head.owner == parent.owner
                        && head.scope_ref == parent.scope_ref
                        && head.scope_epoch == parent.scope_epoch
                })
        })
        .collect();
    if heads.is_empty() {
        return Err(not_bound(
            "head_route_not_bound",
            "no live route of the run's head scope is bound to AFT",
            Some(agent_id),
            Some(run_id),
        ));
    }
    heads.retain(|(_, identity)| {
        identity
            .scope
            .as_ref()
            .and_then(|head| head.attributes.agent_id.as_deref())
            == Some(agent_id)
    });
    // Plugins cache a session's routes per project root. Prefer the head routes
    // at the run's root, so a route opened while working in another checkout
    // does not compete with this checkout's head. A run may also bind elsewhere:
    // without a matching root, all head candidates must still agree on access.
    // Neither provider role nor channel order establishes authority. Compare
    // every eligible route, then inherit one complete identity, never a union.
    if heads.iter().any(|(_, head)| head.root == run.root) {
        heads.retain(|(_, head)| head.root == run.root);
    }
    heads.sort_by_key(|(channel, _)| (channel.channel, channel.epoch));
    let Some((head_channel, head)) = heads.first() else {
        return Err(not_bound(
            "agent_mismatch",
            "the head route's agent is not the agent the run is on",
            Some(agent_id),
            Some(run_id),
        ));
    };
    for (other_channel, other) in &heads {
        let fields = head_access_differences(head, other);
        if !fields.is_empty() {
            let channels = [head_channel.channel, other_channel.channel];
            crate::slog_info!(
                "codemode head route ambiguity: run_id={run_id} fields={} channels={},{}",
                fields.join(","),
                channels[0],
                channels[1]
            );
            return Err(subc_protocol::ErrorBody::new(
                RUN_OWNER_NOT_BOUND,
                format!(
                    "the head scope's routes {} and {} disagree on access settings: {}",
                    channels[0],
                    channels[1],
                    fields.join(", ")
                ),
            )
            .with_detail(json!({
                "reason": "head_route_ambiguous",
                "agent_id": agent_id,
                "run_id": run_id,
                "fields": fields,
                "channels": channels,
            })));
        }
    }
    Ok(effective_identity(run, head, agent_id, run_id, route))
}

fn head_access_differences(head: &RouteIdentity, other: &RouteIdentity) -> Vec<&'static str> {
    [
        ("root", other.root != head.root),
        ("project_root", other.project_root != head.project_root),
        ("harness", other.harness != head.harness),
        ("session", other.session != head.session),
        ("trust", other.trust != head.trust),
        (
            "spawn_principal",
            !same_principal_access(&other.spawn_principal, &head.spawn_principal),
        ),
        (
            "consumer_elicitation_capable",
            other.consumer_elicitation_capable != head.consumer_elicitation_capable,
        ),
        (
            "disabled_tools",
            other.disabled_tools != head.disabled_tools,
        ),
    ]
    .into_iter()
    .filter_map(|(field, differs)| differs.then_some(field))
    .collect()
}

#[derive(PartialEq, Eq)]
struct PrincipalAccessClass {
    strict_sandbox: bool,
    flow_read_carrier: bool,
}

fn principal_access_class(principal: &AuthenticatedPrincipal) -> PrincipalAccessClass {
    PrincipalAccessClass {
        strict_sandbox: crate::sandbox_spawn::principal_requires_strict(principal),
        flow_read_carrier: super::flow_read::principal_is_flow_read_carrier(principal),
    }
}

/// Compare the production sandbox and flow-read rules, not the principal ID.
/// Route channels and epochs identify individual binds, not access settings;
/// the selected head still supplies its complete, unchanged principal.
fn same_principal_access(a: &AuthenticatedPrincipal, b: &AuthenticatedPrincipal) -> bool {
    match (a, b) {
        (
            AuthenticatedPrincipal::RouteBind {
                trust: a_trust,
                project_root: a_root,
                harness: a_harness,
                session_id: a_session,
                principal_id: _,
                route_channel: _,
                route_epoch: _,
            },
            AuthenticatedPrincipal::RouteBind {
                trust: b_trust,
                project_root: b_root,
                harness: b_harness,
                session_id: b_session,
                principal_id: _,
                route_channel: _,
                route_epoch: _,
            },
        ) => {
            a_trust == b_trust
                && a_root == b_root
                && a_harness == b_harness
                && a_session == b_session
                && principal_access_class(a) == principal_access_class(b)
        }
        _ => a == b,
    }
}

/// The identity a codemode call runs under. Root, session, harness, trust,
/// disabled tools and sandbox principal (its id included) are the head
/// route's. The scope stays the run's, so the run's lifetime controls
/// (deadline, scope-end kill, call ledger settlement) keep applying to the
/// work the program starts; only the call-ledger carrier is the run route's
/// (see [`RunOwner::ledger_carrier`]).
fn effective_identity(
    run: &RouteIdentity,
    head: &RouteIdentity,
    agent_id: &str,
    run_id: &str,
    route: RouteChannel,
) -> RouteIdentity {
    let run_scope = OnceLock::new();
    if let Some(scope) = run.run_scope.get() {
        let _ = run_scope.set(Arc::clone(scope));
    }
    RouteIdentity(Arc::new(RouteIdentityData {
        root: head.root.clone(),
        project_root: head.project_root.clone(),
        harness: head.harness.clone(),
        session: head.session.clone(),
        // The reply is framed for the route the call arrived on.
        role: run.role,
        trust: head.trust,
        spawn_principal: head.spawn_principal.clone(),
        // Kept so an untrusted head's shell call reaches the same approval
        // step as the head's direct call, which then refuses it because a run
        // route has nobody to answer.
        consumer_elicitation_capable: head.consumer_elicitation_capable,
        disabled_tools: Arc::clone(&head.disabled_tools),
        scope: run.scope.clone(),
        call_mode: run.call_mode.clone(),
        run_scope,
        // The run route's own counter already noted this call.
        made_tool_call: AtomicBool::new(true),
        run_owner: Some(Arc::new(RunOwner {
            agent_id: agent_id.to_owned(),
            run_id: run_id.to_owned(),
            head: head.clone(),
            run_route: route,
            run_principal: run.spawn_principal.clone(),
        })),
    }))
}

/// Record who a codemode call ran for. Every admitted call writes one line.
pub(super) fn audit_call(identity: &RouteIdentity, tool: &str, corr: u64) {
    if let Some(owner) = identity.run_owner.as_ref() {
        crate::slog_info!(
            "codemode call: tool={tool} agent_id={} run_id={} head_session={} head_root={} run_route={}/{} corr={corr}",
            owner.agent_id,
            owner.run_id,
            identity.session,
            identity.project_root.display(),
            owner.run_route.channel,
            owner.run_route.epoch,
        );
    }
}

#[cfg(test)]
#[path = "codemode_tests.rs"]
mod tests;
