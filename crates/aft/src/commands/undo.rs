use crate::context::AppContext;
use crate::error::AftError;
use crate::protocol::{RawRequest, Response};
use std::path::{Path, PathBuf};

/// Name for the checkpoint that preserves files changed outside AFT before
/// an undo overwrites them: `external-change-<file>-<nanoseconds>`, with
/// `-and-<n>-more` when an operation undo covers several files.
fn external_change_checkpoint_name(paths: &[PathBuf]) -> String {
    let file = paths
        .first()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string())
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let more = match paths.len() {
        0 | 1 => String::new(),
        count => format!("-and-{}-more", count - 1),
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("external-change-{file}{more}-{nanos}")
}

/// Save `paths` as one session-scoped checkpoint. Every file must make it
/// in; a partial checkpoint would let undo overwrite the file left out.
fn save_external_change(
    ctx: &AppContext,
    session: &str,
    paths: &[PathBuf],
) -> Result<String, AftError> {
    let name = external_change_checkpoint_name(paths);
    let mut checkpoints = ctx.checkpoint().lock();
    let info = checkpoints.create_for_files(session, &name, paths.to_vec())?;
    if let Some((path, error)) = info.skipped.first() {
        checkpoints.delete(session, &name);
        return Err(AftError::IoError {
            path: path.display().to_string(),
            message: format!(
                "undo refused: the file changed outside AFT and could not be saved to a \
                 checkpoint before the restore: {error}"
            ),
        });
    }
    Ok(name)
}

/// Handle the `undo` command: restore the latest operation, or one file when requested.
///
/// Params: `file` (string, optional) — path to a single file to undo.
/// Returns: `{ path, backup_id }` on success, or `no_undo_history` error. When
/// the file had changed outside AFT, its content is first saved as a
/// checkpoint, named in `external_change_checkpoint` and in `warning`.
pub fn handle_undo(req: &RawRequest, ctx: &AppContext) -> Response {
    let mut backup = ctx.backup().lock();
    let session = req.session();
    let mut save = |paths: &[PathBuf]| save_external_change(ctx, session, paths);

    let Some(file) = req.params.get("file").and_then(|v| v.as_str()) else {
        let intent_paths = backup
            .preview_last_operation_paths(req.session())
            .unwrap_or_default();
        // Stored backup keys are just as untrusted as explicit tool arguments:
        // authority may have changed since the snapshot was taken.
        for path in &intent_paths {
            if let Err(response) = ctx.validate_write_location(&req.id, path) {
                return response;
            }
        }
        if let Some(reason) = backup.take_latest_skipped_reason_for_undo(req.session(), None) {
            return backup_skipped_response(req, reason);
        }
        let _view_intent =
            crate::views::intent::record_paths(intent_paths.iter().map(|path| path.as_path()));
        return match backup.restore_last_operation_preserving(req.session(), &mut save) {
            Ok(operation) => {
                let mut result = serde_json::json!({
                    "operation": true,
                    "op_id": operation.op_id,
                    "restored_count": operation.restored.len(),
                    "restored": operation.restored.into_iter().map(|file| {
                        let mut restored = serde_json::json!({
                            "path": file.path.display().to_string(),
                            "backup_id": file.backup_id,
                        });
                        if let Some(checkpoint) = file.external_change_checkpoint {
                            restored["external_change_checkpoint"] =
                                serde_json::Value::String(checkpoint);
                        }
                        restored
                    }).collect::<Vec<_>>(),
                    "warnings": operation.warnings,
                });
                if let Some(checkpoint) = operation.external_change_checkpoint {
                    result["external_change_checkpoint"] = serde_json::Value::String(checkpoint);
                }
                Response::success(&req.id, result)
            }
            Err(e) => Response::error(&req.id, e.code(), e.to_string()),
        };
    };

    // Resolve relative paths against the bound project root BEFORE validation so
    // the backup key matches the path the mutating tool recorded. A relative path
    // passed straight to `canonicalize_key` would be joined against the daemon's
    // cwd, missing the stack and reporting a false `no_undo_history`.
    let input = ctx.resolve_relative_path(Path::new(file));
    let resolved = match ctx.validate_write_location(&req.id, &input) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    if let Some(reason) =
        backup.take_latest_skipped_reason_for_undo(req.session(), Some(resolved.as_path()))
    {
        return backup_skipped_response(req, reason);
    }

    let _view_intent = crate::views::intent::record_paths([resolved.as_path()]);
    match backup.restore_latest_detailed(req.session(), &resolved, &mut save) {
        Ok(restored) => {
            let mut result = serde_json::json!({
                "path": file,
                "backup_id": restored.entry.backup_id,
            });
            if let Some(w) = restored.warning {
                result["warning"] = serde_json::Value::String(w);
            }
            if let Some(checkpoint) = restored.external_change_checkpoint {
                result["external_change_checkpoint"] = serde_json::Value::String(checkpoint);
            }
            Response::success(&req.id, result)
        }
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}

fn backup_skipped_response(
    req: &RawRequest,
    reason: crate::backup::BackupSkippedReason,
) -> Response {
    Response::error_with_data(
        &req.id,
        "no_undo_history",
        format!(
            "undo is unavailable for this change because its backup snapshot was skipped ({})",
            reason.as_str()
        ),
        serde_json::json!({ "backup_skipped_reason": reason.as_str() }),
    )
}

/// Handle the `undo_preview` command: return paths the next undo would touch without restoring.
///
/// Params: `file`/`filePath` (string, optional) — when provided, previews that per-file stack;
/// otherwise previews the most recent operation in the session.
/// Returns: `{ paths, count }` on success, or `no_undo_history` error.
pub fn handle_undo_preview(req: &RawRequest, ctx: &AppContext) -> Response {
    let backup = ctx.backup().lock();

    let path = match req
        .params
        .get("file")
        .or_else(|| req.params.get("filePath"))
        .and_then(|v| v.as_str())
    {
        Some(file) => {
            let input = ctx.resolve_relative_path(Path::new(file));
            match ctx.validate_write_location(&req.id, &input) {
                Ok(path) => Some(path),
                Err(response) => return response,
            }
        }
        None => None,
    };

    if let Some(reason) = backup.latest_skipped_reason_for_undo(req.session(), path.as_deref()) {
        return Response::success(
            &req.id,
            serde_json::json!({
                "paths": path.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                "count": path.iter().count(),
                "backup_skipped_reason": reason.as_str(),
                "undo_unavailable": true,
            }),
        );
    }

    let preview = path
        .as_deref()
        .map(|path| {
            backup
                .preview_latest_path(req.session(), path)
                .map(|path| vec![path])
                .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))
        })
        .unwrap_or_else(|| {
            backup
                .preview_last_operation_paths(req.session())
                .map_err(|error| Response::error(&req.id, error.code(), error.to_string()))
        });

    match preview {
        Ok(paths) => Response::success(
            &req.id,
            serde_json::json!({
                "paths": paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                "count": paths.len(),
            }),
        ),
        Err(response) => response,
    }
}
