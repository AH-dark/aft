use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};
use subc_protocol::Principal;

use crate::bash_background::registry::BgTaskRegistry;
use crate::protocol::Response;

pub(crate) const BASH_KILL_SCOPE_OPERATION: &str = "bash.kill_scope";

/// Stop a cancelled worker's bash tasks without revoking its scope.
///
/// Only the tasks held by `registries` are stopped. The module's frame loop
/// uses [`handle_bash_kill_scope_with_saved_tasks`] so tasks saved by an
/// earlier module instance are stopped as well.
#[cfg(all(test, unix))]
pub(crate) fn handle_bash_kill_scope(
    params: Value,
    principal: Option<&Principal>,
    registries: &[BgTaskRegistry],
) -> Response {
    handle_bash_kill_scope_with_saved_tasks(params, principal, registries, None)
}

/// Stop a cancelled worker's bash tasks without revoking its scope, including
/// its unfinished tasks saved under `saved_storage_root` that no live
/// registry holds.
///
/// After a module restart, a project's saved tasks are replayed only when
/// that project binds again, but their processes keep running meanwhile.
/// They are recovered from their saved records and stopped now, not left to
/// a later replay: a project may never bind again, and until it does nothing
/// would enforce even the task's own time limit. Each such task is marked
/// `from_saved_record` in the reply. If a saved task of this scope cannot be
/// recovered, the reply is an error naming it, never a completed stop.
pub(crate) fn handle_bash_kill_scope_with_saved_tasks(
    params: Value,
    principal: Option<&Principal>,
    registries: &[BgTaskRegistry],
    saved_storage_root: Option<&Path>,
) -> Response {
    const ID: &str = "management-bash-kill-scope";
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Params {
        scope: subc_protocol::scope::ScopeStamp,
    }
    let params = match serde_json::from_value::<Params>(params) {
        Ok(params) => params,
        Err(error) => return Response::error(ID, "invalid_request", error.to_string()),
    };
    if principal != Some(&params.scope.owner) {
        return Response::error(
            ID,
            "kill_scope_not_owner",
            "bash.kill_scope requires the scope's owner",
        );
    }
    let (recovered, recovery_errors) = match saved_storage_root {
        Some(root) => BgTaskRegistry::recover_saved_scope_tasks(root, &params.scope, registries),
        None => (Vec::new(), Vec::new()),
    };
    let all_registries = registries
        .iter()
        .map(|registry| (registry, false))
        .chain(recovered.iter().map(|registry| (registry, true)))
        .collect::<Vec<_>>();
    let mut killed = Vec::new();
    let mut already_finished = 0;
    // Finish all local kills before requesting remote cancellation. All remote
    // workers receive intent before any wait, and share one reply deadline.
    let mut remote_tasks = Vec::new();
    let mut deadline = Instant::now();
    let mut kill_error = None;
    'passes: for remote in [false, true] {
        if remote {
            deadline = Instant::now() + Duration::from_secs(10);
        }
        for (registry, from_saved_record) in &all_registries {
            match registry.pause_scope(&params.scope, remote) {
                Ok(report) => {
                    for mut task in report.killed {
                        task.from_saved_record = *from_saved_record;
                        if task.remote.is_some() {
                            remote_tasks.push(((*registry).clone(), killed.len()));
                        }
                        killed.push(task);
                    }
                    already_finished += report.already_finished;
                }
                Err(error) => {
                    kill_error = Some(error);
                    break 'passes;
                }
            }
        }
    }
    // Recovered remote tasks have no worker yet. Start them only now that
    // their pause is recorded: each worker then cancels a job the runner
    // already holds, or withholds a resubmission or local fallback, and
    // records that outcome, which the loop below waits for.
    for registry in &recovered {
        registry.start_held_remote_workers();
    }
    if kill_error.is_none() {
        for (registry, index) in remote_tasks {
            let task = &mut killed[index];
            task.remote =
                Some(registry.wait_owner_pause_cancel(&task.task_id, &task.session_id, deadline));
        }
    }
    // The recovered tasks are stopped and their records saved; a later
    // replay of their project restores them as finished.
    for registry in &recovered {
        registry.detach();
    }
    if let Some(error) = kill_error {
        return Response::error(ID, "kill_scope_failed", error);
    }
    if !recovery_errors.is_empty() {
        return Response::error(ID, "kill_scope_failed", recovery_errors.join("; "));
    }
    killed.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    Response::success(
        ID,
        json!({"killed":killed, "already_finished":already_finished, "not_found":0}),
    )
}

#[cfg(all(test, unix))]
#[path = "bash_kill_scope_tests.rs"]
mod tests;
