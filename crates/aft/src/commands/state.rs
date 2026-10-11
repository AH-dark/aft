use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::json;

use crate::context::AppContext;
use crate::harness::Harness;
use crate::protocol::{RawRequest, Response};

#[derive(Debug, Deserialize)]
struct StateParams {
    key: String,
    #[serde(default)]
    value: Option<String>,
}

pub fn handle_db_get_state(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match parse_params(req, "db_get_state") {
        Ok(params) => params,
        Err(response) => return response,
    };

    let Some(harness) = ctx.harness_opt() else {
        return not_configured(&req.id, "db_get_state");
    };
    let harness_name = harness.storage_segment();
    match ctx.db() {
        Some(db) => match db.lock() {
            Ok(conn) => {
                match crate::db::state::get_harness_state(&conn, &harness_name, &params.key) {
                    Ok(Some(value)) => {
                        return Response::success(&req.id, json!({ "value": value }))
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return Response::error(
                            &req.id,
                            "db_error",
                            format!("db_get_state failed: {error}"),
                        );
                    }
                }
            }
            Err(error) => {
                return Response::error(
                    &req.id,
                    "db_error",
                    format!("db_get_state failed to lock database: {error}"),
                );
            }
        },
        None => {}
    }

    let value = legacy_harness_path(ctx, harness, &params.key)
        .and_then(|path| fs::read_to_string(path).ok());
    Response::success(&req.id, json!({ "value": value }))
}

pub fn handle_db_set_state(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match parse_params(req, "db_set_state") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let Some(value) = params.value else {
        return Response::error(
            &req.id,
            "invalid_request",
            "db_set_state: missing required param 'value'",
        );
    };

    let Some(harness) = ctx.harness_opt() else {
        return not_configured(&req.id, "db_set_state");
    };

    let Some(db) = ctx.db() else {
        return write_legacy_harness_state(&req.id, ctx, harness, &params.key, &value);
    };

    let harness_name = harness.storage_segment();
    match db.lock() {
        Ok(conn) => {
            if let Err(error) = crate::db::state::set_harness_state(
                &conn,
                &harness_name,
                &params.key,
                &value,
                unix_millis(),
            ) {
                return Response::error(
                    &req.id,
                    "db_error",
                    format!("db_set_state failed: {error}"),
                );
            }
        }
        Err(error) => {
            return Response::error(
                &req.id,
                "db_error",
                format!("db_set_state failed to lock database: {error}"),
            );
        }
    }

    if let Some(path) = legacy_harness_path(ctx, harness, &params.key) {
        if let Err(error) = atomic_write(&path, value.as_bytes()) {
            log::warn!(
                "db_set_state legacy write failed for {}: {}",
                path.display(),
                error
            );
        }
    }

    Response::success(&req.id, json!({ "ok": true }))
}

pub fn handle_db_get_host_state(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match parse_params(req, "db_get_host_state") {
        Ok(params) => params,
        Err(response) => return response,
    };

    match ctx.db() {
        Some(db) => match db.lock() {
            Ok(conn) => match crate::db::state::get_host_state(&conn, &params.key) {
                Ok(Some(value)) => return Response::success(&req.id, json!({ "value": value })),
                Ok(None) => {}
                Err(error) => {
                    return Response::error(
                        &req.id,
                        "db_error",
                        format!("db_get_host_state failed: {error}"),
                    );
                }
            },
            Err(error) => {
                return Response::error(
                    &req.id,
                    "db_error",
                    format!("db_get_host_state failed to lock database: {error}"),
                );
            }
        },
        None => {}
    }

    let value = legacy_host_path(ctx, &params.key).and_then(|path| fs::read_to_string(path).ok());
    Response::success(&req.id, json!({ "value": value }))
}

pub fn handle_db_set_host_state(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match parse_params(req, "db_set_host_state") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let Some(value) = params.value else {
        return Response::error(
            &req.id,
            "invalid_request",
            "db_set_host_state: missing required param 'value'",
        );
    };

    let Some(db) = ctx.db() else {
        return write_legacy_host_state(&req.id, ctx, &params.key, &value);
    };

    match db.lock() {
        Ok(conn) => {
            if let Err(error) =
                crate::db::state::set_host_state(&conn, &params.key, &value, unix_millis())
            {
                return Response::error(
                    &req.id,
                    "db_error",
                    format!("db_set_host_state failed: {error}"),
                );
            }
        }
        Err(error) => {
            return Response::error(
                &req.id,
                "db_error",
                format!("db_set_host_state failed to lock database: {error}"),
            );
        }
    }

    if let Some(path) = legacy_host_path(ctx, &params.key) {
        if let Err(error) = atomic_write(&path, value.as_bytes()) {
            log::warn!(
                "db_set_host_state legacy write failed for {}: {}",
                path.display(),
                error
            );
        }
    }

    Response::success(&req.id, json!({ "ok": true }))
}

fn not_configured(request_id: &str, command: &str) -> Response {
    Response::error(
        request_id,
        "not_configured",
        format!("{command}: configure must be called before harness-scoped state is available"),
    )
}

fn db_unavailable_without_fallback(request_id: &str, command: &str, key: &str) -> Response {
    Response::error(
        request_id,
        "db_unavailable",
        format!("{command}: database unavailable and no JSON fallback is defined for key '{key}'"),
    )
}

fn write_legacy_harness_state(
    request_id: &str,
    ctx: &AppContext,
    harness: Harness,
    key: &str,
    value: &str,
) -> Response {
    let Some(path) = legacy_harness_path(ctx, harness, key) else {
        return db_unavailable_without_fallback(request_id, "db_set_state", key);
    };

    match atomic_write(&path, value.as_bytes()) {
        Ok(()) => Response::success(request_id, json!({ "ok": true })),
        Err(error) => Response::error(
            request_id,
            "state_persistence_error",
            format!(
                "db_set_state JSON fallback write failed for {}: {}",
                path.display(),
                error
            ),
        ),
    }
}

fn write_legacy_host_state(request_id: &str, ctx: &AppContext, key: &str, value: &str) -> Response {
    let Some(path) = legacy_host_path(ctx, key) else {
        return db_unavailable_without_fallback(request_id, "db_set_host_state", key);
    };

    match atomic_write(&path, value.as_bytes()) {
        Ok(()) => Response::success(request_id, json!({ "ok": true })),
        Err(error) => Response::error(
            request_id,
            "state_persistence_error",
            format!(
                "db_set_host_state JSON fallback write failed for {}: {}",
                path.display(),
                error
            ),
        ),
    }
}

fn parse_params(req: &RawRequest, command: &str) -> Result<StateParams, Response> {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    serde_json::from_value::<StateParams>(raw_params).map_err(|error| {
        Response::error(
            &req.id,
            "invalid_request",
            format!("{command}: invalid params: {error}"),
        )
    })
}

fn legacy_harness_path(ctx: &AppContext, harness: Harness, key: &str) -> Option<PathBuf> {
    let dir = ctx.storage_dir().join(harness.storage_segment());
    match key {
        "last_announced_version" => Some(repair_root_scoped_harness_file(
            ctx,
            &dir,
            "last_announced_version",
        )),
        "last_update_check" => Some(repair_root_scoped_harness_file(
            ctx,
            &dir,
            "last-update-check.json",
        )),
        "warned_tools" => Some(dir.join("warned_tools.json")),
        _ => None,
    }
}

fn repair_root_scoped_harness_file(
    ctx: &AppContext,
    harness_dir: &Path,
    file_name: &str,
) -> PathBuf {
    let harness_path = harness_dir.join(file_name);
    if harness_path.exists() {
        return harness_path;
    }

    let root_path = ctx.storage_dir().join(file_name);
    if !root_path.exists() {
        return harness_path;
    }

    if let Some(parent) = harness_path.parent() {
        if crate::private_storage::create_dir_all(parent).is_err() {
            return harness_path;
        }
    }
    let _ = fs::rename(root_path, &harness_path);
    harness_path
}

fn legacy_host_path(ctx: &AppContext, key: &str) -> Option<PathBuf> {
    let dir = ctx.storage_dir();
    match key {
        "trusted_filter_projects" => Some(dir.join("trusted-filter-projects.json")),
        _ => None,
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        crate::private_storage::open_dir(parent, parent)?;
    }
    crate::durability::atomic_write(path, bytes, crate::durability::AtomicWriteDurability::File)?;
    // File durability is required; directory durability remains a best-effort mirror policy.
    if let Some(parent) = path.parent() {
        let _ = crate::durability::sync_dir(parent);
    }
    Ok(())
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn state_writer_private_mode_survives_restrictive_umask() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "AFT_TEST_STATE_PRIVATE_UMASK";
        if std::env::var_os(CHILD).is_some() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            // Create the fixture directory first so umask cannot make it inaccessible.
            // Only the state file is created with the restrictive mask.
            unsafe { libc::umask(0o777) };
            super::atomic_write(&path, b"state").unwrap();
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::state::tests::state_writer_private_mode_survives_restrictive_umask",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "umask child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn state_writer_preserves_a_colliding_temporary_and_existing_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"old value").unwrap();
        let collision = std::rc::Rc::new(std::cell::RefCell::new(None));
        let observed = collision.clone();
        crate::durability::BEFORE_TEMP_CREATE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |temporary| {
                std::fs::write(temporary, b"another writer").unwrap();
                *observed.borrow_mut() = Some(temporary.to_path_buf());
            }));
        });
        assert!(super::atomic_write(&path, b"new value").is_err());
        let temporary = collision
            .borrow()
            .clone()
            .expect("actual writer creation hook");
        assert_eq!(std::fs::read(temporary).unwrap(), b"another writer");
        assert_eq!(std::fs::read(path).unwrap(), b"old value");
    }

    #[test]
    fn state_writer_keeps_required_file_sync_and_private_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        crate::durability::take();
        super::atomic_write(&path, b"state payload").unwrap();
        let events = crate::durability::take();
        assert!(events.iter().any(|(kind, event_path)| *kind
            == crate::durability::EventKind::FileSync
            && event_path == &path));
        assert_eq!(std::fs::read(&path).unwrap(), b"state payload");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn state_writer_cleans_owned_temporary_on_failed_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("destination");
        std::fs::create_dir(&path).unwrap();
        assert!(super::atomic_write(&path, b"state").is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert!(path.is_dir());
    }
    use serde_json::json;

    use super::*;
    use crate::config::Config;
    use crate::parser::TreeSitterProvider;

    fn test_context() -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), Config::default())
    }

    fn state_request(command: &str, params: serde_json::Value) -> RawRequest {
        RawRequest {
            id: "state".to_string(),
            command: command.to_string(),
            lsp_hints: None,
            session_id: None,
            params,
        }
    }

    #[test]
    fn db_get_state_before_configure_returns_not_configured() {
        let ctx = test_context();
        let req = state_request("db_get_state", json!({ "key": "warned_tools" }));

        let response = handle_db_get_state(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "not_configured");
    }

    #[test]
    fn db_set_state_uses_json_fallback_when_database_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        ctx.update_config(|config| {
            config.storage_dir = Some(temp.path().to_path_buf());
        });
        ctx.set_harness(Harness::Opencode);

        let set_req = state_request(
            "db_set_state",
            json!({ "key": "warned_tools", "value": "[\"rustfmt\"]" }),
        );
        let set_response = handle_db_set_state(&set_req, &ctx);

        assert!(set_response.success);
        let path = temp.path().join("opencode").join("warned_tools.json");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"rustfmt\"]");

        let get_req = state_request("db_get_state", json!({ "key": "warned_tools" }));
        let get_response = handle_db_get_state(&get_req, &ctx);
        assert!(get_response.success);
        assert_eq!(get_response.data["value"], "[\"rustfmt\"]");
    }

    #[test]
    fn db_set_host_state_uses_json_fallback_when_database_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        ctx.update_config(|config| {
            config.storage_dir = Some(temp.path().to_path_buf());
        });

        let set_req = state_request(
            "db_set_host_state",
            json!({ "key": "trusted_filter_projects", "value": "[]" }),
        );
        let set_response = handle_db_set_host_state(&set_req, &ctx);

        assert!(set_response.success);
        let path = temp.path().join("trusted-filter-projects.json");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
    }
}
