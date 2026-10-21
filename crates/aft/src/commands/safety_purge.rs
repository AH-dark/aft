use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::backup::purge::{purge_session_path, session_path_keys, PurgeTarget, SavedPathMatch};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

#[cfg(test)]
thread_local! {
    static AFTER_UNDO_PURGE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// Handle the `safety_purge` command (`aft_safety` op `purge`): permanently
/// remove the calling session's retained copies of `file`, or of everything
/// under it when it is a directory, up to the project root itself.
///
/// Two kinds of copies are removed: undo backups (disk, `aft.db` rows and
/// in-memory caches) and the matching files inside the session's named
/// checkpoints. A checkpoint that still holds other files is rewritten
/// without them; one left empty is dropped. A rewrite failure keeps the
/// checkpoint recoverable and is reported as an incomplete purge.
/// Nothing belonging to another session is touched, and nothing here can be
/// undone.
///
/// Params: `file` (string, required).
/// Returns: `{ path, undo_stacks_removed, undo_entries_removed,
/// undo_paths_removed, checkpoint_paths_removed,
/// checkpoints_pruned: [{ name, files_removed }], checkpoints_dropped,
/// failures: [{ target, error }], complete, irreversible }`.
///
/// Saved paths are compared by location, not spelling (see [`PurgeTarget`]),
/// so a copy saved through a symlinked directory is purged with its real
/// path. A checkpoint copy whose parent directory no longer exists cannot be
/// told apart from the purged path; it is kept and listed in `failures`, and
/// `complete` is false.
pub fn handle_safety_purge(req: &RawRequest, ctx: &AppContext) -> Response {
    match handle_safety_purge_impl(req, ctx) {
        Ok(response) | Err(response) => response,
    }
}

/// Handle the `safety_purge_paths` command: list the saved paths a
/// `safety_purge` of `file` would remove for the calling session, without
/// removing anything, so a plugin can ask permission for each one first. A
/// purge reaches every saved copy under a directory whether or not that
/// directory still exists, so the path itself is not enough to ask for.
///
/// Params: `file` (string, required).
/// Returns: `{ path, paths }`, `paths` sorted: the checkpoint copies'
/// spellings and the undo-backup keys.
pub fn handle_safety_purge_paths(req: &RawRequest, ctx: &AppContext) -> Response {
    match handle_safety_purge_paths_impl(req, ctx) {
        Ok(response) | Err(response) => response,
    }
}

fn handle_safety_purge_paths_impl(
    req: &RawRequest,
    ctx: &AppContext,
) -> Result<Response, Response> {
    let path = purge_location(req, ctx)?;
    let session = req.session();
    let target = PurgeTarget::new(&path);
    let mut paths = session_path_keys(ctx.backup(), session, &path, |db_path| {
        ctx.app().db_for_path(db_path)
    })
    .into_iter()
    .collect::<BTreeSet<_>>();
    let (selected, _unknown) = classify_checkpoint_copies(req, ctx, &target)?;
    paths.extend(selected.into_iter().map(|(_, path)| path));
    Ok(Response::success(
        &req.id,
        json!({
            "path": path.display().to_string(),
            "paths": paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>(),
        }),
    ))
}

/// The validated location `file` names: inside the project root when the
/// root is restricted, and never following a final symlink. The same check
/// restore uses.
fn purge_location(req: &RawRequest, ctx: &AppContext) -> Result<PathBuf, Response> {
    let Some(file) = req
        .params
        .get("file")
        .and_then(|value| value.as_str())
        .filter(|path| !path.is_empty())
    else {
        return Err(Response::error(
            &req.id,
            "invalid_request",
            "purge: missing required param 'path'",
        ));
    };
    let input = ctx.resolve_relative_path(Path::new(file));
    ctx.validate_write_location(&req.id, &input)
}

/// A checkpoint and one file in it, as `(checkpoint name, absolute path)`.
type CheckpointCopy = (String, PathBuf);

/// Sort this session's checkpoint copies into those the purge removes and
/// those whose identity cannot be determined. Unrelated copies are dropped.
fn classify_checkpoint_copies(
    req: &RawRequest,
    ctx: &AppContext,
    target: &PurgeTarget,
) -> Result<(Vec<CheckpointCopy>, Vec<CheckpointCopy>), Response> {
    let copies = ctx
        .checkpoint()
        .lock()
        .session_file_paths(req.session())
        .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))?;
    let mut selected = Vec::new();
    let mut unknown = Vec::new();
    for copy in copies {
        match target.classify(&copy.1, false) {
            SavedPathMatch::Selected => selected.push(copy),
            SavedPathMatch::Unknown => unknown.push(copy),
            SavedPathMatch::Unrelated => {}
        }
    }
    Ok((selected, unknown))
}

fn handle_safety_purge_impl(req: &RawRequest, ctx: &AppContext) -> Result<Response, Response> {
    let path = purge_location(req, ctx)?;
    let session = req.session();

    let target = PurgeTarget::new(&path);
    // Load checkpoints before deleting undo history. A later I/O failure still
    // produces a partial report, since undo removals cannot be rolled back.
    let (_, unknown) = classify_checkpoint_copies(req, ctx, &target)?;
    let backups = purge_session_path(ctx.backup(), session, &path, |db_path| {
        ctx.app().db_for_path(db_path)
    })
    .map_err(|message| Response::error(&req.id, "invalid_request", message))?;

    #[cfg(test)]
    AFTER_UNDO_PURGE.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });

    let matches = |stored: &Path| target.classify(stored, false) == SavedPathMatch::Selected;
    let checkpoints = ctx
        .checkpoint()
        .lock()
        .purge_paths(session, &matches)
        .unwrap_or_else(|error| crate::checkpoint::CheckpointPurge {
            failures: vec![("session".to_string(), error.to_string())],
            ..Default::default()
        });

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
    failures.extend(
        unknown
            .iter()
            .filter(|(name, _)| !checkpoints.dropped.contains(name))
            .map(|(name, stored)| {
                json!({
                    "target": format!("checkpoint {name} copy of {}", stored.display()),
                    "error": "its parent directory no longer exists, so whether it is another spelling of the purged path cannot be told; it was kept",
                })
            }),
    );

    Ok(Response::success(
        &req.id,
        json!({
            "path": path.display().to_string(),
            "undo_stacks_removed": backups.removed.stacks,
            "undo_entries_removed": backups.removed.entries,
            "undo_paths_removed": backups.removed_paths,
            "checkpoint_paths_removed": checkpoints.paths,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::language::StubProvider;
    use std::fs;

    #[test]
    fn safety_purge_late_checkpoint_failure_reports_undo_removals() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let secret = project.path().join("secret.env");
        fs::write(&secret, "saved-secret").unwrap();
        let ctx = AppContext::new(
            Box::new(StubProvider),
            Config {
                project_root: Some(project.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        )
        .with_test_checkpoint_store();
        let req: RawRequest = serde_json::from_value(json!({
            "id": "late-purge", "command": "safety_purge", "session_id": "s1", "file": secret,
        }))
        .unwrap();
        ctx.backup()
            .lock()
            .snapshot_with_op("s1", &secret, "fixture", Some("op"))
            .unwrap();
        let checkpoint = ctx
            .checkpoint()
            .lock()
            .create_for_files("s1", "saved", vec![secret.clone()])
            .unwrap();
        let session_dir = checkpoint
            .storage_path
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let aside = session_dir.with_extension("unreadable-test");
        let hook_session = session_dir.clone();
        let hook_aside = aside.clone();
        AFTER_UNDO_PURGE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::rename(&hook_session, &hook_aside).unwrap();
                fs::write(&hook_session, "not a directory").unwrap();
            }))
        });
        let response = serde_json::to_value(handle_safety_purge(&req, &ctx)).unwrap();
        fs::remove_file(&session_dir).unwrap();
        fs::rename(&aside, &session_dir).unwrap();
        assert_eq!(
            response["success"], true,
            "late failure must be a partial result: {response}"
        );
        assert_eq!(response["complete"], false, "{response}");
        assert_eq!(response["undo_stacks_removed"], 1, "{response}");
        assert_eq!(response["undo_entries_removed"], 1, "{response}");
        assert_eq!(
            response["undo_paths_removed"],
            json!([secret]),
            "{response}"
        );
        assert_eq!(
            response["checkpoint_paths_removed"],
            json!([]),
            "{response}"
        );
        assert!(
            response["failures"][0]["error"]
                .as_str()
                .unwrap()
                .contains("failed to read durable checkpoint session"),
            "{response}"
        );
        assert!(ctx.backup().lock().history("s1", &secret).is_empty());
        fs::write(&secret, "changed").unwrap();
        ctx.checkpoint().lock().restore("s1", "saved").unwrap();
        assert_eq!(fs::read_to_string(&secret).unwrap(), "saved-secret");
    }
}
