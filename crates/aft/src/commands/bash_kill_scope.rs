use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use subc_protocol::Principal;

use crate::bash_background::registry::BgTaskRegistry;
use crate::protocol::Response;

pub(crate) const BASH_KILL_SCOPE_OPERATION: &str = "bash.kill_scope";

/// Stop a cancelled worker's bash tasks without revoking its scope.
pub(crate) fn handle_bash_kill_scope(
    params: Value,
    principal: Option<&Principal>,
    registries: &[BgTaskRegistry],
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
    let mut killed = Vec::new();
    let mut already_finished = 0;
    // Finish all local kills before requesting remote cancellation. All remote
    // workers receive intent before any wait, and share one reply deadline.
    let mut remote_tasks = Vec::new();
    let mut deadline = Instant::now();
    for remote in [false, true] {
        if remote {
            deadline = Instant::now() + Duration::from_secs(10);
        }
        for registry in registries {
            match registry.pause_scope(&params.scope, remote) {
                Ok(report) => {
                    for task in report.killed {
                        if task.remote.is_some() {
                            remote_tasks.push((registry.clone(), killed.len()));
                        }
                        killed.push(task);
                    }
                    already_finished += report.already_finished;
                }
                Err(error) => return Response::error(ID, "kill_scope_failed", error),
            }
        }
    }
    for (registry, index) in remote_tasks {
        let task = &mut killed[index];
        task.remote =
            Some(registry.wait_owner_pause_cancel(&task.task_id, &task.session_id, deadline));
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
