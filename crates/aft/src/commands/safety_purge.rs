use std::path::{Path, PathBuf};

use serde_json::json;

use crate::backup::purge::{backup_key_for_path, purge_session_path};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Handle the `safety_purge` command (`aft_safety` op `purge`): permanently
/// remove the calling session's retained copies of `file`, or of everything
/// under it when it is a directory, up to the project root itself.
///
/// Two kinds of copies are removed: undo backups (disk, `aft.db` rows and
/// in-memory caches) and the matching files inside the session's named
/// checkpoints. A checkpoint that still holds other files is rewritten
/// without them; one left empty, or one that cannot be rewritten, is dropped.
/// Nothing belonging to another session is touched, and nothing here can be
/// undone.
///
/// Params: `file` (string, required).
/// Returns: `{ path, undo_stacks_removed, undo_entries_removed,
/// checkpoints_pruned: [{ name, files_removed }], checkpoints_dropped,
/// failures: [{ target, error }], complete, irreversible }`.
pub fn handle_safety_purge(req: &RawRequest, ctx: &AppContext) -> Response {
    match handle_safety_purge_impl(req, ctx) {
        Ok(response) | Err(response) => response,
    }
}

fn handle_safety_purge_impl(req: &RawRequest, ctx: &AppContext) -> Result<Response, Response> {
    let Some(file) = req
        .params
        .get("file")
        .and_then(|value| value.as_str())
        .filter(|path| !path.is_empty())
    else {
        return Ok(Response::error(
            &req.id,
            "invalid_request",
            "purge: missing required param 'path'",
        ));
    };
    let input = ctx.resolve_relative_path(Path::new(file));
    // The same location check restore uses: inside the project root when the
    // root is restricted, and never following a final symlink.
    let path = ctx.validate_write_location(&req.id, &input)?;
    let session = req.session();

    let backups = purge_session_path(ctx.backup(), session, &path, |db_path| {
        ctx.app().db_for_path(db_path)
    })
    .map_err(|message| Response::error(&req.id, "invalid_request", message))?;

    // Checkpoint keys are the validated location as given; backup keys are
    // fully canonical. Either spelling of the purge path selects a file.
    let mut prefixes: Vec<PathBuf> = vec![path.clone()];
    let canonical = backup_key_for_path(&path);
    if canonical != path {
        prefixes.push(canonical);
    }
    let matches = |stored: &Path| prefixes.iter().any(|prefix| stored.starts_with(prefix));
    let checkpoints = ctx
        .checkpoint()
        .lock()
        .purge_paths(session, &matches)
        .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))?;

    let mut failures = backups
        .failures
        .iter()
        .map(|failure| {
            json!({
                "target": format!("undo backups of {}", failure.path),
                "error": failure.error,
            })
        })
        .collect::<Vec<_>>();
    failures.extend(checkpoints.failures.iter().map(|(name, error)| {
        json!({
            "target": format!("checkpoint {name}"),
            "error": error,
        })
    }));

    Ok(Response::success(
        &req.id,
        json!({
            "path": path.display().to_string(),
            "undo_stacks_removed": backups.removed.stacks,
            "undo_entries_removed": backups.removed.entries,
            "checkpoints_pruned": checkpoints
                .pruned
                .iter()
                .map(|(name, files_removed)| json!({ "name": name, "files_removed": files_removed }))
                .collect::<Vec<_>>(),
            "checkpoints_dropped": checkpoints.dropped,
            "complete": failures.is_empty(),
            "failures": failures,
            "irreversible": true,
        }),
    ))
}
