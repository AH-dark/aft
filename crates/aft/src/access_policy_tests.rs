use crate::config::Config;
use crate::context::AppContext;
use crate::language::StubProvider;
use crate::protocol::RawRequest;
use serde_json::json;
use std::path::{Path, PathBuf};

fn context(root: &Path, denied: &Path) -> AppContext {
    let mut config = Config {
        project_root: Some(root.to_path_buf()),
        restrict_to_project_root: false,
        ..Config::default()
    };
    config.sandbox.read_deny = vec![denied.to_path_buf()];
    AppContext::new(Box::new(StubProvider), config)
}

fn request(command: &str, params: serde_json::Value) -> RawRequest {
    RawRequest {
        id: "policy-test".into(),
        command: command.into(),
        session_id: Some("policy-session".into()),
        lsp_hints: None,
        params,
    }
}

#[test]
fn access_policy_absolute_floor_inside_own_root() {
    let root = tempfile::tempdir().unwrap();
    let denied = root.path().join("secret");
    std::fs::write(&denied, "protected").unwrap();
    let ctx = context(root.path(), &denied);
    for result in [
        ctx.validate_read_path("read", "policy-session", &denied),
        ctx.validate_path("write", &denied),
        ctx.validate_write_location("unlink", &denied),
    ] {
        let error = result.expect_err("the absolute floor overrides an unrestricted own root");
        assert_eq!(error.data["code"], "denied_floor");
    }
    assert_eq!(std::fs::read_to_string(denied).unwrap(), "protected");
}

#[test]
fn access_policy_move_checks_both_locations_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let denied = root.path().join("floor");
    std::fs::write(&source, "source").unwrap();
    let ctx = context(root.path(), &denied);
    let response = crate::commands::move_file::handle_move_file(
        &request("move_file", json!({"file": source, "destination": denied})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert!(source.exists());
    assert!(!denied.exists());
    assert!(ctx
        .backup()
        .lock()
        .tracked_files("policy-session")
        .is_empty());
    std::fs::write(&denied, "protected").unwrap();
    let response = crate::commands::move_file::handle_move_file(
        &request(
            "move_file",
            json!({"file": denied, "destination": root.path().join("other")}),
        ),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert_eq!(std::fs::read_to_string(denied).unwrap(), "protected");
}

#[test]
fn access_policy_delete_batch_checks_floor_before_first_unlink() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let denied = root.path().join("floor");
    std::fs::write(&source, "source").unwrap();
    std::fs::write(&denied, "protected").unwrap();
    let ctx = context(root.path(), &denied);
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"files": [source, denied]})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert_eq!(std::fs::read_to_string(source).unwrap(), "source");
    assert_eq!(std::fs::read_to_string(denied).unwrap(), "protected");
    assert!(ctx
        .backup()
        .lock()
        .tracked_files("policy-session")
        .is_empty());
}

#[test]
fn access_policy_patch_checks_floor_before_first_hunk() {
    let root = tempfile::tempdir().unwrap();
    let allowed = root.path().join("allowed");
    let denied = root.path().join("floor");
    let ctx = context(root.path(), &denied);
    let patch = "*** Begin Patch\n*** Add File: allowed\n+allowed\n*** Add File: floor\n+protected\n*** End Patch";
    let response = crate::commands::apply_patch::handle_apply_patch(
        &request("apply_patch", json!({"patch_text": patch})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert!(!allowed.exists());
    assert!(!denied.exists());
}

#[test]
fn access_policy_checkpoint_checks_floor_before_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let allowed = root.path().join("allowed");
    let denied = root.path().join("floor");
    std::fs::write(&allowed, "allowed").unwrap();
    std::fs::write(&denied, "protected").unwrap();
    let ctx = context(root.path(), &denied);
    let response = crate::commands::checkpoint::handle_checkpoint(
        &request(
            "checkpoint",
            json!({"name": "floor", "files": [allowed, denied]}),
        ),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert!(ctx
        .checkpoint()
        .lock()
        .file_paths("policy-session", "floor")
        .is_err());
}

fn saved_files(root: &Path) -> [PathBuf; 2] {
    let paths = [root.join("allowed"), root.join("floor")];
    for path in &paths {
        std::fs::write(path, "old").unwrap();
    }
    paths
}

#[test]
fn access_policy_restore_checks_all_stored_locations_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let paths = saved_files(root.path());
    let ctx = context(root.path(), &paths[1]);
    ctx.checkpoint()
        .lock()
        .create_for_files("policy-session", "saved", paths.to_vec())
        .unwrap();
    for path in &paths {
        std::fs::write(path, "new").unwrap();
    }
    let response = crate::commands::restore_checkpoint::handle_restore_checkpoint(
        &request("restore_checkpoint", json!({"name": "saved"})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    for path in paths {
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
    }
}

#[test]
fn access_policy_undo_checks_all_operation_locations_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let paths = saved_files(root.path());
    let ctx = context(root.path(), &paths[1]);
    for path in &paths {
        ctx.backup()
            .lock()
            .snapshot_with_op("policy-session", path, "fixture", Some("saved"))
            .unwrap();
        std::fs::write(path, "new").unwrap();
    }
    let response = crate::commands::undo::handle_undo(&request("undo", json!({})), &ctx);
    assert_eq!(response.data["code"], "denied_floor");
    for path in paths {
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
    }
    assert_eq!(
        ctx.backup()
            .lock()
            .preview_last_operation_paths("policy-session")
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn access_policy_private_store_only_has_own_root_carve_out() {
    let store = tempfile::tempdir().unwrap();
    let root = store.path().join("own-worktree");
    std::fs::create_dir(&root).unwrap();
    let own = root.join("own");
    let sibling = store.path().join("other-worktree/file");
    let mut config = Config {
        project_root: Some(root.clone()),
        storage_dir: Some(store.path().to_path_buf()),
        ..Config::default()
    };
    config.sandbox.write_allow = vec![store.path().to_path_buf()];
    let ctx = AppContext::new(Box::new(StubProvider), config);
    assert_eq!(ctx.validate_path("own", &own).unwrap(), own);
    for result in [
        ctx.validate_path("sibling", &sibling),
        ctx.validate_read_path("read", "policy-session", &sibling),
    ] {
        assert_eq!(result.unwrap_err().data["code"], "denied_floor");
    }
}

#[test]
fn access_policy_recursive_delete_cannot_remove_floor_descendant() {
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    let denied = tree.join("floor");
    std::fs::write(&denied, "protected").unwrap();
    let ctx = context(root.path(), &denied);
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": tree, "recursive": true})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert_eq!(std::fs::read_to_string(denied).unwrap(), "protected");
}

#[test]
fn access_policy_legacy_outcomes_are_byte_identical() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside.path().canonicalize().unwrap().join("missing.txt");
    for configured_root in [None, Some(root_path.clone())] {
        for restrict in [false, true] {
            let ctx = AppContext::new(
                Box::new(StubProvider),
                Config {
                    project_root: configured_root.clone(),
                    restrict_to_project_root: restrict,
                    ..Config::default()
                },
            );
            for path in [Path::new("./missing.txt"), outside_path.as_path()] {
                for check in 0..3 {
                    let actual = match check {
                        0 => ctx.validate_path("parity", path),
                        1 => ctx.validate_read_path("parity", "policy-session", path),
                        _ => ctx.validate_write_location("parity", path),
                    };
                    if configured_root.is_some() && restrict && path == outside_path {
                        let expected = crate::protocol::Response::error(
                            "parity",
                            "path_outside_root",
                            format!(
                                "path '{}' is outside the project root '{}'",
                                path.display(),
                                root_path.display()
                            ),
                        );
                        assert_eq!(
                            serde_json::to_vec(&actual.unwrap_err()).unwrap(),
                            serde_json::to_vec(&expected).unwrap()
                        );
                    } else {
                        let expected = if configured_root.is_some() && restrict {
                            root_path.join("missing.txt")
                        } else {
                            path.to_path_buf()
                        };
                        assert_eq!(
                            serde_json::to_vec(&actual.unwrap()).unwrap(),
                            serde_json::to_vec(&expected).unwrap()
                        );
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn access_policy_floor_follows_parent_alias_but_preserves_final_link() {
    let root = tempfile::tempdir().unwrap();
    let denied = root.path().join("floor");
    std::fs::create_dir(&denied).unwrap();
    std::fs::write(denied.join("file"), "protected").unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&denied, &alias).unwrap();
    let ctx = context(root.path(), &denied);
    assert_eq!(
        ctx.validate_path("read-alias", &alias.join("file"))
            .unwrap_err()
            .data["code"],
        "denied_floor"
    );
    assert_eq!(
        ctx.validate_write_location("write-alias", &alias.join("new"))
            .unwrap_err()
            .data["code"],
        "denied_floor"
    );
    assert_eq!(
        ctx.validate_write_location("unlink-alias", &alias).unwrap(),
        alias
    );
}

#[test]
fn access_policy_restricted_floor_refusal_keeps_legacy_bytes() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("floor");
    let mut config = Config {
        project_root: Some(root_path.clone()),
        restrict_to_project_root: true,
        ..Config::default()
    };
    config.sandbox.read_deny = vec![path.clone()];
    let ctx = AppContext::new(Box::new(StubProvider), config);
    let expected = crate::protocol::Response::error(
        "parity-floor",
        "path_outside_root",
        format!(
            "path '{}' is outside the project root '{}'",
            path.display(),
            root_path.display()
        ),
    );
    for result in [
        ctx.validate_path("parity-floor", &path),
        ctx.validate_write_location("parity-floor", &path),
        ctx.validate_read_path("parity-floor", "session", &path),
    ] {
        assert_eq!(
            serde_json::to_vec(&result.unwrap_err()).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }
}
