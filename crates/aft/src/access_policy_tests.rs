use crate::config::Config;
use crate::context::AppContext;
use crate::language::StubProvider;
use crate::protocol::RawRequest;
use serde_json::json;
use std::path::{Path, PathBuf};

#[path = "access_policy_task_tests.rs"]
mod task_records;

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

#[cfg(unix)]
#[test]
fn access_policy_preserved_location_resolves_parent_before_dotdot() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("worktree");
    let store = fixture.path().join("store");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir_all(store.join("child")).unwrap();
    let denied = store.join("secret");
    std::fs::write(&denied, "protected").unwrap();
    std::fs::write(root.join("secret"), "own").unwrap();
    std::os::unix::fs::symlink(store.join("child"), root.join("link")).unwrap();
    let candidate = root.join("link/../secret");
    let ctx = context(&root, &denied);
    assert_eq!(
        ctx.validate_write_location("parent-dotdot", &candidate)
            .unwrap_err()
            .data["code"],
        "denied_floor"
    );
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": candidate})),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert_eq!(std::fs::read_to_string(denied).unwrap(), "protected");
    assert_eq!(std::fs::read_to_string(root.join("secret")).unwrap(), "own");
}

fn artifact_read_context() -> (tempfile::TempDir, AppContext, PathBuf) {
    use crate::bash_background::persistence::{task_paths, write_task, PersistedTask};
    use crate::bash_background::BgTaskStatus;

    let directory = tempfile::tempdir().unwrap();
    let resolved = directory.path().canonicalize().unwrap();
    let root = resolved.join("project");
    let storage = resolved.join("storage");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&storage).unwrap();
    let config = Config {
        project_root: Some(root.clone()),
        storage_dir: Some(storage.clone()),
        restrict_to_project_root: false,
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let paths = task_paths(&storage, "policy-session", "bash-0123456789abcdef").unwrap();
    let mut metadata = PersistedTask::starting(
        "bash-0123456789abcdef".into(),
        "policy-session".into(),
        "echo artifact".into(),
        root.clone(),
        Some(root),
        None,
        false,
        false,
    );
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &metadata).unwrap();
    std::fs::write(&paths.stdout, "fn artifact() {}\n").unwrap();
    std::fs::write(&paths.stderr, "").unwrap();
    std::fs::write(&paths.exit, "0\n").unwrap();
    ctx.bash_background()
        .replay_session(&storage, "policy-session")
        .unwrap();
    assert!(ctx
        .bash_background()
        .is_session_owned_artifact_path("policy-session", &paths.stdout));
    ctx.set_harness(crate::harness::Harness::Opencode);
    (directory, ctx, paths.stdout)
}

pub(crate) fn assert_artifact_read_policy(
    handler: impl Fn(&RawRequest, &AppContext) -> crate::protocol::Response,
    command: &str,
    params: impl Fn(&Path) -> serde_json::Value,
) {
    let (_directory, ctx, artifact) = artifact_read_context();
    let owned = handler(&request(command, params(&artifact)), &ctx);
    for code in ["denied_floor", "path_outside_root"] {
        assert_ne!(
            owned.data["code"], code,
            "owner did not reach {command}: {:?}",
            owned.data
        );
    }
    if owned.data["code"] == "invalid_request" {
        // Output artifacts have no source-language extension. Reaching the
        // parser's extension error proves authorization passed, not parsing.
        assert_eq!(
            owned.data["message"],
            "invalid request: unsupported file extension: stdout"
        );
    }
    let unregistered = artifact.parent().unwrap().join("unregistered");
    std::fs::write(&unregistered, "fn artifact() {}\n").unwrap();
    for (session, path) in [
        ("other-session", artifact.as_path()),
        ("policy-session", unregistered.as_path()),
        ("policy-session", artifact.parent().unwrap()),
    ] {
        let mut req = request(command, params(path));
        req.session_id = Some(session.into());
        let denied = handler(&req, &ctx);
        assert!(!denied.success, "{command} leaked: {:?}", denied.data);
        assert_eq!(
            denied.data["code"], "denied_floor",
            "{command}: {:?}",
            denied.data
        );
        assert_eq!(
            denied.data["message"],
            format!(
                "path '{}' is denied by the access policy floor",
                path.display()
            )
        );
    }
}

macro_rules! artifact_read_tool_test {
    ($name:ident, $handler:expr, $command:literal, $params:expr) => {
        #[test]
        fn $name() {
            assert_artifact_read_policy($handler, $command, $params);
        }
    };
}

artifact_read_tool_test!(
    artifact_read_grep,
    crate::commands::grep::handle_grep,
    "grep",
    |path| json!({"path": path, "pattern": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_glob,
    crate::commands::glob::handle_glob,
    "glob",
    |path| json!({"path": path, "pattern": "*"})
);
artifact_read_tool_test!(
    artifact_read_outline,
    crate::commands::outline::handle_outline,
    "outline",
    |path| json!({"file": path})
);

#[test]
fn artifact_read_grep_and_glob_exact_file_under_root_restriction() {
    for restrict in [false, true] {
        let (_directory, ctx, artifact) = artifact_read_context();
        let mut config = (*ctx.config()).clone();
        config.restrict_to_project_root = restrict;
        ctx.set_config(config);
        let grep = crate::commands::grep::handle_grep(
            &request("grep", json!({"path": artifact, "pattern": "artifact"})),
            &ctx,
        );
        assert!(grep.success, "{restrict}: {:?}", grep.data);
        assert_eq!(grep.data["matches"][0]["line_text"], "fn artifact() {}");
        let glob = crate::commands::glob::handle_glob(
            &request("glob", json!({"path": artifact, "pattern": "*"})),
            &ctx,
        );
        assert!(glob.success, "{restrict}: {:?}", glob.data);
        let files = glob.data["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{restrict}: {:?}", glob.data);
        // Glob resolves paths before returning them, so compare file identities rather than
        // temp-directory aliases (macOS) or verbatim path prefixes (Windows).
        assert_eq!(
            std::fs::canonicalize(files[0].as_str().unwrap()).unwrap(),
            std::fs::canonicalize(&artifact).unwrap()
        );
    }
}

#[test]
fn artifact_read_storage_directory_grep_does_not_open_owned_outputs() {
    let (_directory, ctx, artifact) = artifact_read_context();
    let storage = ctx.config().storage_dir.clone().unwrap();
    let probe = crate::project_content::Probe::new(&storage);
    // Use storage as the default search root so this tests walker exclusion,
    // not the access policy's earlier refusal of an explicit storage path.
    ctx.update_config(|config| config.project_root = Some(storage.clone()));
    let grep =
        crate::commands::grep::handle_grep(&request("grep", json!({"pattern": "artifact"})), &ctx);
    assert!(grep.success, "{:?}", grep.data);
    assert_eq!(grep.data["total_matches"], 0, "{:?}", grep.data);
    assert!(probe.paths().is_empty(), "{:?}", probe.paths());
    assert!(ctx
        .bash_background()
        .explicit_task_artifacts("policy-session", &storage)
        .is_none());
    assert!(ctx
        .bash_background()
        .is_session_owned_artifact_path("policy-session", &artifact));
}

#[cfg(unix)]
#[test]
fn artifact_read_owned_live_sqlite_alias_is_refused() {
    let (_directory, ctx, artifact) = artifact_read_context();
    let storage = ctx.config().storage_dir.clone().unwrap();
    let database = storage.join("live.db");
    let _connection = crate::db::file_identity::IdentityConnection::new(
        rusqlite::Connection::open(&database).unwrap(),
        "owned artifact alias test",
    );
    std::fs::remove_file(&artifact).unwrap();
    std::fs::hard_link(&database, &artifact).unwrap();
    assert!(ctx
        .bash_background()
        .is_session_owned_artifact_path("policy-session", &artifact));
    let probe = crate::project_content::Probe::new(&storage);
    assert!(ctx
        .bash_background()
        .explicit_task_artifacts("policy-session", &artifact)
        .unwrap()
        .is_empty());
    let grep = crate::commands::grep::handle_grep(
        &request("grep", json!({"path": artifact, "pattern": "."})),
        &ctx,
    );
    assert!(grep.success, "{:?}", grep.data);
    assert_eq!(grep.data["total_matches"], 0, "{:?}", grep.data);
    let glob = crate::commands::glob::handle_glob(
        &request("glob", json!({"path": artifact, "pattern": "*"})),
        &ctx,
    );
    assert!(glob.success, "{:?}", glob.data);
    assert_eq!(glob.data["total"], 0, "{:?}", glob.data);
    assert!(probe.paths().is_empty(), "{:?}", probe.paths());
}

artifact_read_tool_test!(
    artifact_read_outline_many,
    crate::commands::outline::handle_outline,
    "outline",
    |path| json!({"files": [path]})
);
artifact_read_tool_test!(
    artifact_read_zoom,
    crate::commands::zoom::handle_zoom,
    "zoom",
    |path| json!({"file": path, "symbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_ast_search,
    crate::commands::ast_search::handle_ast_search,
    "ast_search",
    |path| json!({"paths": [path], "lang": "rust", "pattern": "fn $NAME() { $$$ }"})
);
artifact_read_tool_test!(
    artifact_read_search,
    crate::commands::semantic_search::handle_semantic_search,
    "semantic_search",
    |path| json!({"path": path, "query": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_callers,
    crate::commands::callers::handle_callers,
    "callers",
    |path| json!({"file": path, "symbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_call_tree,
    crate::commands::call_tree::handle_call_tree,
    "call_tree",
    |path| json!({"file": path, "symbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_impact,
    crate::commands::impact::handle_impact,
    "impact",
    |path| json!({"file": path, "symbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_trace_to,
    crate::commands::trace_to::handle_trace_to,
    "trace_to",
    |path| json!({"file": path, "symbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_trace_to_symbol,
    crate::commands::trace_to_symbol::handle_trace_to_symbol,
    "trace_to_symbol",
    |path| json!({"file": path, "symbol": "artifact", "toSymbol": "artifact"})
);
artifact_read_tool_test!(
    artifact_read_trace_data,
    crate::commands::trace_data::handle_trace_data,
    "trace_data",
    |path| json!({"file": path, "symbol": "artifact", "expression": "value"})
);
artifact_read_tool_test!(
    artifact_read_lsp_hover,
    crate::commands::lsp_hover::handle_lsp_hover,
    "lsp_hover",
    |path| json!({"file": path, "line": 1, "character": 1})
);
artifact_read_tool_test!(
    artifact_read_lsp_goto_definition,
    crate::commands::lsp_goto_definition::handle_lsp_goto_definition,
    "lsp_goto_definition",
    |path| json!({"file": path, "line": 1, "character": 1})
);
artifact_read_tool_test!(
    artifact_read_lsp_find_references,
    crate::commands::lsp_find_references::handle_lsp_find_references,
    "lsp_find_references",
    |path| json!({"file": path, "line": 1, "character": 1})
);
artifact_read_tool_test!(
    artifact_read_lsp_prepare_rename,
    crate::commands::lsp_prepare_rename::handle_lsp_prepare_rename,
    "lsp_prepare_rename",
    |path| json!({"file": path, "line": 1, "character": 1})
);
artifact_read_tool_test!(
    artifact_read_lsp_inspect,
    crate::commands::lsp_inspect::handle_lsp_inspect,
    "lsp_inspect",
    |path| json!({"file": path})
);
artifact_read_tool_test!(
    artifact_read_lsp_diagnostics,
    crate::commands::lsp_diagnostics::handle_lsp_diagnostics,
    "lsp_diagnostics",
    |path| json!({"file": path})
);
artifact_read_tool_test!(
    artifact_read_inspect,
    crate::commands::inspect::handle_inspect,
    "inspect",
    |path| json!({"scope": path, "sections": ["todos"]})
);
artifact_read_tool_test!(
    artifact_read_conflicts,
    |req, ctx| crate::commands::conflicts::handle_git_conflicts(ctx, req),
    "conflicts",
    |path| json!({"path": path})
);

#[test]
fn artifact_read_ast_replace_still_requires_write_access() {
    let (_directory, ctx, artifact) = artifact_read_context();
    let response = crate::commands::ast_replace::handle_ast_replace(
        &request(
            "ast_replace",
            json!({"paths": [artifact], "lang": "rust", "pattern": "fn $NAME() { $$$ }", "rewrite": "fn changed() {}"}),
        ),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor");
    assert_eq!(
        std::fs::read_to_string(&artifact).unwrap(),
        "fn artifact() {}\n"
    );
}

#[test]
fn artifact_read_search_does_not_authorize_containing_git_store() {
    // Storage resolution gives AFT_STORAGE_DIR precedence over configuration.
    // Keep other tests' process-wide overrides out of this private-store fixture.
    let _storage_override = crate::test_env::without_storage_override();
    let (_directory, ctx, artifact) = artifact_read_context();
    let storage = ctx.config().storage_dir.clone().unwrap();
    let initialized = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .arg(&storage)
        .status()
        .unwrap();
    assert!(initialized.success());
    std::fs::write(storage.join("private.rs"), "fn private_artifact() {}\n").unwrap();
    let mut index = crate::search_index::SearchIndex::build(&storage);
    let cache_dir = crate::search_index::resolve_cache_dir(&storage, Some(&storage));
    assert!(index.write_to_disk(&cache_dir, None));
    let response = crate::commands::semantic_search::handle_semantic_search(
        &request(
            "semantic_search",
            json!({"path": artifact, "pattern": "artifact"}),
        ),
        &ctx,
    );
    assert_eq!(response.data["code"], "denied_floor", "{:?}", response.data);
    assert_eq!(
        response.data["message"],
        format!(
            "path '{}' is denied by the access policy floor",
            std::fs::canonicalize(storage).unwrap().display()
        )
    );
}
