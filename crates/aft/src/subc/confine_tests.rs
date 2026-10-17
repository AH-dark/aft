use super::*;
use crate::context::confinement;
use crate::run_tool_call::{run_tool_call, ToolCallContext, ToolCallOutcome};

struct Fixture {
    repo: tempfile::TempDir,
    storage: tempfile::TempDir,
    pin: PathBuf,
    live: PathBuf,
    ctx: AppContext,
    route: RouteIdentity,
}

impl Fixture {
    fn new() -> Self {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=AFT",
                "-c",
                "user.email=aft@example.invalid",
                "commit",
                "-qm",
                "fixture",
                "--allow-empty",
            ],
            vec!["worktree", "add", "-q", "--detach", "pin", "HEAD"],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let pin = root.join("pin");
        let live = root.join("live");
        std::fs::create_dir(&live).unwrap();
        std::fs::write(live.join("sentinel.rs"), "fn outside_secret() {}\n").unwrap();
        std::fs::write(pin.join("only.rs"), "fn pinned_token() {}\n").unwrap();
        std::os::unix::fs::symlink("../live", pin.join("escape")).unwrap();
        std::os::unix::fs::symlink("../live/sentinel.rs", pin.join("escape-file")).unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(&pin, storage.path(), None);
        ctx.update_config(|config| config.restrict_to_project_root = false);
        let route = identity(&pin, "designer", 7, true);
        catalog(
            json!({"preset":"worker", "params":{"confine":"root"}}),
            &route,
            &ctx,
        )
        .unwrap();
        Self {
            repo,
            storage,
            pin,
            live,
            ctx,
            route,
        }
    }

    fn escapes(&self) -> Vec<(&'static str, String)> {
        vec![
            ("parent", "../live/sentinel.rs".into()),
            ("directory-symlink", "escape/sentinel.rs".into()),
            ("file-symlink", "escape-file".into()),
            (
                "absolute",
                self.live.join("sentinel.rs").display().to_string(),
            ),
            ("dot-parent", "./../live/sentinel.rs".into()),
            ("missing-relative", "sentinel.rs".into()),
        ]
    }

    fn call(&self, name: &str, args: Value) -> Response {
        call(&self.ctx, &self.route, name, args)
    }
}

fn call(ctx: &AppContext, route: &RouteIdentity, name: &str, args: Value) -> Response {
    let _scope = crate::access_policy::scope_guard(Some(access_grants::policy_scope_key(route)));
    let _ask =
        crate::access_policy::ask_route_guard(Some(crate::access_policy::AskRoute::Consent {
            once: vec![],
        }));
    let format =
        crate::subc_format::FormatContext::from_tool_call(name, &args, &route.project_root);
    let tool_ctx = ToolCallContext {
        project_root: route.project_root.clone(),
        session_id: Some(route.session.clone()),
        request_id: "confine-vector".into(),
        diagnostics_on_edit: false,
        preview: false,
        edit_slot_survives: None,
        report_registration_downgrade: false,
        standard_edit_grammar: true,
        disabled_tools: Some(route.disabled_tools.clone()),
        worker_session: true,
        host_tool_names: None,
    };
    with_call_policy(ctx, route, &tool_ctx.request_id, name, args, |args| {
        let ToolCallOutcome::Unary(result) = run_tool_call(
            name,
            args,
            &format,
            &tool_ctx,
            ctx,
            &|request, ctx| match request.command.as_str() {
                "read" => crate::commands::read::handle_read(&request, ctx),
                "grep" => crate::commands::grep::handle_grep(&request, ctx),
                "glob" => crate::commands::glob::handle_glob(&request, ctx),
                "outline" => crate::commands::outline::handle_outline(&request, ctx),
                _ => Response::success(&request.id, json!({"unexpected_dispatch": true})),
            },
            None,
            None,
        );
        result.response
    })
    .unwrap_or_else(|response| response)
}

fn arguments(name: &str, input: &str) -> Value {
    match name {
        "read" => json!({"filePath": input}),
        "outline" => json!({"target": input}),
        "grep" => json!({"pattern":"outside_secret|pinned_token", "path":input}),
        "glob" => json!({"pattern":"**/*.rs", "path":input}),
        _ => unreachable!(),
    }
}

#[test]
fn confined_escape_matrix() {
    let f = Fixture::new();
    for name in ["read", "grep", "glob", "outline"] {
        for (route, input) in f.escapes().into_iter().chain([
            ("issue", "issue://1".into()),
            ("pr", "pr://1".into()),
            ("url", "https://example.invalid/private".into()),
            ("dot-symlink", "./escape/sentinel.rs".into()),
        ]) {
            let response = f.call(name, arguments(name, &input));
            assert_eq!(
                response.data["code"], "path_outside_confined_root",
                "{name} {route}: {response:?}"
            );
            assert!(response.data["message"].as_str().unwrap().contains(&input));
            let text = serde_json::to_string(&response).unwrap();
            assert!(!text.contains("outside_secret"));
            if route.contains("symlink") {
                assert!(!text.contains(&f.live.display().to_string()));
            }
            assert!(response.data.get("ask").is_none());
        }
        let response = f.call(name, arguments(name, "./only.rs"));
        assert_ne!(
            response.data["code"], "path_outside_confined_root",
            "{name}: {response:?}"
        );
        assert!(response.success, "{name}: {response:?}");
    }
}

#[test]
fn confined_never_asks() {
    let f = Fixture::new();
    let before = confinement::ask_calls();
    for name in ["read", "grep", "glob", "outline"] {
        let response = f.call(name, arguments(name, "../live/sentinel.rs"));
        assert_eq!(response.data["code"], "path_outside_confined_root");
    }
    assert_eq!(
        confinement::ask_calls(),
        before,
        "confined access reached the consent ask path"
    );
}

#[test]
fn confined_keeps_in_root_denial_floors() {
    let f = Fixture::new();
    f.ctx
        .update_config(|config| config.sandbox.read_deny = vec![f.pin.join("only.rs")]);
    let response = f.call("read", arguments("read", "only.rs"));
    assert!(!response.success, "{response:?}");
    assert!(!serde_json::to_string(&response)
        .unwrap()
        .contains("pinned_token"));
}

#[test]
fn confined_call_time_tool_check() {
    let f = Fixture::new();
    for name in [
        "bash",
        "bash_watch",
        "bash_status",
        "bash_write",
        "bash_kill",
        "powershell",
        "search",
        "zoom",
        "callgraph",
        "inspect",
        "write",
        "edit",
        "apply_patch",
        "delete",
        "move",
        "import",
        "safety",
        "ast_search",
        "ast_replace",
        "status",
        "conflicts",
        "configure",
    ] {
        let response = f.call(name, json!({}));
        assert_eq!(
            response.data["code"], "tool_not_offered_confined",
            "{name}: {response:?}"
        );
    }
    // Looking up the worker key is independent of a caller's claimed preset.
    assert!(admit_confined(&f.ctx, &f.route, "bash").is_err());
}

#[test]
fn confined_restart_and_fail_closed() {
    let f = Fixture::new();
    drop(f.ctx);
    let restarted = context(&f.pin, f.storage.path(), None);
    assert!(confined(&restarted, &f.route).unwrap());
    let response = call(
        &restarted,
        &f.route,
        "read",
        arguments("read", "../live/sentinel.rs"),
    );
    assert_eq!(response.data["code"], "path_outside_confined_root");
    let answer = catalog(json!({"preset":"worker"}), &f.route, &restarted).unwrap();
    assert_eq!(answer["tools"].as_array().unwrap().len(), 4);
    let db = restarted.db().unwrap();
    db.lock()
        .unwrap()
        .execute("UPDATE remote_exec_policies SET params='broken'", [])
        .unwrap();
    assert_eq!(
        call(&restarted, &f.route, "read", arguments("read", "only.rs")).data["code"],
        "confine_policy_unavailable"
    );
}

#[test]
fn confined_walks_ignore_ready_parent_index() {
    let f = Fixture::new();
    std::fs::write(f.pin.join("ghost.rs"), "fn pinned_token() {}\n").unwrap();
    std::fs::remove_file(f.pin.join("only.rs")).unwrap();
    let index = crate::search_index::SearchIndex::build(f.repo.path());
    assert!(index.ready);
    std::fs::remove_file(f.pin.join("ghost.rs")).unwrap();
    std::fs::write(f.pin.join("only.rs"), "fn pinned_token() {}\n").unwrap();
    *f.ctx.search_index().write().unwrap() = Some(index);
    std::fs::write(f.live.join("ignore-rules"), "only.rs\n").unwrap();
    for name in [".gitignore", ".ignore", ".aftignore"] {
        std::os::unix::fs::symlink("../live/ignore-rules", f.pin.join(name)).unwrap();
    }
    for name in ["grep", "glob"] {
        let response = f.call(name, arguments(name, "."));
        assert!(response.success, "{response:?}");
        assert_eq!(
            response.data["fallback"], "filesystem",
            "{name}: {response:?}"
        );
        assert_eq!(response.data["complete"], true);
        assert!(response.data.get("walk_truncated").is_none());
        assert!(!response.data["text"].as_str().unwrap().contains("(walk)"));
        let files = if name == "grep" {
            response.data["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["file"].as_str().unwrap())
                .collect::<Vec<_>>()
        } else {
            response.data["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|file| file.as_str().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(files, [f.pin.join("only.rs").to_str().unwrap()]);
        let text = serde_json::to_string(&response).unwrap();
        for forbidden in ["sentinel.rs", "outside_secret", "ghost.rs", "escape-file"] {
            assert!(!text.contains(forbidden), "{name}: {text}");
        }
    }
}

#[test]
fn confined_catalog_validation_and_monotonic_freeze() {
    let f = Fixture::new();
    for value in [json!("other"), json!(null), json!(false), json!({})] {
        assert_eq!(
            catalog(
                json!({"preset":"worker", "params":{"confine":value}}),
                &f.route,
                &f.ctx
            )
            .unwrap_err()
            .code,
            "invalid_confine"
        );
    }
    for (preset, scoped) in [("head", true), ("reader", true), ("worker", false)] {
        let route = identity(&f.pin, "unscoped", 7, scoped);
        assert_eq!(
            catalog(
                json!({"preset":preset,"params":{"confine":"root"}}),
                &route,
                &f.ctx
            )
            .unwrap_err()
            .code,
            "confine_requires_scoped_worker"
        );
    }
    assert!(tool_provider::describe()["capabilities"]
        .as_array()
        .unwrap()
        .contains(&json!("tool-catalog-confine/v1")));
    catalog(
        json!({"preset":"worker","params":{"remote_exec":{"enabled":false}}}),
        &f.route,
        &f.ctx,
    )
    .unwrap();
    assert!(confined(&f.ctx, &f.route).unwrap());
    let other = identity(&f.pin, "other-worker", 7, true);
    catalog(
        json!({"preset":"worker","params":{"remote_exec":{"enabled":false}}}),
        &other,
        &f.ctx,
    )
    .unwrap();
    assert!(!confined(&f.ctx, &other).unwrap());
    catalog(
        json!({"preset":"worker","params":{"confine":"root"}}),
        &other,
        &f.ctx,
    )
    .unwrap();
    assert!(confined(&f.ctx, &other).unwrap());
    let unconfined = identity(&f.pin, "ordinary-worker", 7, true);
    assert!(!confined(&f.ctx, &unconfined).unwrap());
    assert!(call(&f.ctx, &unconfined, "read", arguments("read", "only.rs")).success);
    assert!(admit_confined(&f.ctx, &unconfined, "bash").is_ok());
}

fn vector(f: &Fixture, name: &str, value: Value) {
    fn stable(value: Value, root: &str) -> Value {
        match value {
            Value::String(text) => Value::String(text.replace(root, "<fixture>")),
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|item| stable(item, root)).collect())
            }
            Value::Object(items) => Value::Object(
                items
                    .into_iter()
                    .map(|(key, value)| {
                        let value = if key == "search_ms" {
                            json!(0.0)
                        } else {
                            stable(value, root)
                        };
                        (key, value)
                    })
                    .collect(),
            ),
            value => value,
        }
    }
    let value = stable(value, f.pin.parent().unwrap().to_str().unwrap());
    let bytes = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/confine")
        .join(format!("{name}.json"));
    if std::env::var_os("AFT_REGENERATE_CONFINE_VECTORS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        bytes,
        "vector drift: {}",
        path.display()
    );
}

#[test]
fn confined_handler_golden_vectors() {
    let f = Fixture::new();
    let request = json!({"op":"tool.catalog", "preset":"worker", "params":{"confine":"root"}});
    vector(&f, "catalog-request", request.clone());
    vector(
        &f,
        "catalog-reply",
        catalog(request, &f.route, &f.ctx).unwrap(),
    );
    for name in ["read", "grep", "glob", "outline"] {
        for (route, input) in f.escapes() {
            let response = f.call(name, arguments(name, &input));
            assert_eq!(response.data["code"], "path_outside_confined_root");
            vector(
                &f,
                &format!("{name}-{route}"),
                serde_json::to_value(response).unwrap(),
            );
        }
    }
    for name in ["grep", "glob"] {
        let response =
            crate::grep_executor::with_walk_limit(0, || f.call(name, arguments(name, ".")));
        assert_eq!(response.data["complete"], false, "{name}: {response:?}");
        assert_eq!(
            response.data["walk_truncated"], true,
            "{name}: {response:?}"
        );
        let text = crate::subc_format::format_response(name, &response, false);
        assert!(text.contains("(walk)"), "{name}: {text}");
        vector(
            &f,
            &format!("{name}-walk"),
            json!({"native":serde_json::to_value(response).unwrap(),"text":text}),
        );
    }
}

#[test]
fn confined_grants_and_owned_outputs_cannot_escape() {
    use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
    use crate::bash_background::persistence::{task_paths, write_task, PersistedTask};
    use crate::bash_background::BgTaskStatus;
    let f = Fixture::new();
    let scope = access_grants::policy_scope_key(&f.route);
    let db = f.ctx.db().unwrap();
    crate::db::access_grants::insert(
        &db.lock().unwrap(),
        &scope,
        &AccessGrant {
            path: f.live.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        },
    )
    .unwrap();
    let paths = task_paths(f.storage.path(), "designer", "bash-0123456789abcdef").unwrap();
    let mut metadata = PersistedTask::starting(
        "bash-0123456789abcdef".into(),
        "designer".into(),
        "fixture".into(),
        f.pin.clone(),
        Some(f.pin.clone()),
        None,
        false,
        false,
    );
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &metadata).unwrap();
    std::fs::write(&paths.stdout, "fn output() {}\n").unwrap();
    std::fs::write(&paths.stderr, "").unwrap();
    std::fs::write(&paths.exit, "0\n").unwrap();
    f.ctx
        .bash_background()
        .replay_session(f.storage.path(), "designer")
        .unwrap();
    assert!(f
        .ctx
        .bash_background()
        .is_session_owned_artifact_path("designer", &paths.stdout));
    for name in ["read", "grep", "glob", "outline"] {
        for input in [f.live.join("sentinel.rs"), paths.stdout.clone()] {
            let response = f.call(name, arguments(name, input.to_str().unwrap()));
            assert!(
                response.data["message"]
                    .as_str()
                    .unwrap()
                    .contains(input.to_str().unwrap()),
                "{name}: {response:?}"
            );
            assert_eq!(
                response.data["code"], "path_outside_confined_root",
                "{name}: {response:?}"
            );
        }
    }
}

fn pattern_cases(f: &Fixture) -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "glob-parent-pattern",
            "glob",
            json!({"pattern":"../live/**"}),
        ),
        (
            "glob-absolute-pattern",
            "glob",
            json!({"pattern":format!("{}/*", f.live.display())}),
        ),
        (
            "glob-nested-parent-pattern",
            "glob",
            json!({"pattern":"**/../../live/*"}),
        ),
        (
            "grep-parent-include",
            "grep",
            json!({"pattern":"outside_secret|pinned_token", "include":"../live/*"}),
        ),
        (
            "grep-outside-excludes",
            "grep",
            json!({"pattern":"outside_secret|pinned_token", "include":"**/*.rs", "exclude":["../live/*", "**/../../live/*", format!("{}/*", f.live.display())]}),
        ),
    ]
}

fn assert_pinned_results(f: &Fixture, name: &str, response: &Response) -> Vec<String> {
    assert!(response.success, "{name}: {response:?}");
    assert_eq!(response.data["complete"], true, "{name}: {response:?}");
    assert_eq!(
        response.data["fallback"], "filesystem",
        "{name}: {response:?}"
    );
    let paths = if name == "grep" {
        response.data["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["file"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    } else {
        response.data["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|path| path.as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    for path in &paths {
        assert!(
            Path::new(path).starts_with(&f.pin),
            "{name} escaped through {path}"
        );
        assert_eq!(
            Path::new(path).canonicalize().unwrap(),
            f.pin.join("only.rs")
        );
    }
    assert!(
        !serde_json::to_string(response)
            .unwrap()
            .contains("outside_secret"),
        "{name}: {response:?}"
    );
    paths
}

#[test]
fn confined_patterns_filter_only_pinned_walk_candidates() {
    let f = Fixture::new();
    for (case, name, args) in pattern_cases(&f) {
        let response = f.call(name, args);
        let paths = assert_pinned_results(&f, name, &response);
        if case == "grep-outside-excludes" {
            assert_eq!(paths, [f.pin.join("only.rs").display().to_string()]);
        } else {
            assert!(paths.is_empty(), "{case}: {response:?}");
        }
    }
    for name in ["grep", "glob"] {
        assert_eq!(
            assert_pinned_results(&f, name, &f.call(name, arguments(name, "."))),
            [f.pin.join("only.rs").display().to_string()]
        );
    }
}

#[test]
fn confined_grep_rayon_scans_inherit_fence() {
    let f = Fixture::new();
    let response = f.call("grep", arguments("grep", "."));
    assert_eq!(
        assert_pinned_results(&f, "grep", &response),
        [f.pin.join("only.rs").display().to_string()]
    );
    assert!(response.data["files_searched"].as_u64().unwrap() > 0);
}

fn read_on_filesystem_helper(f: &Fixture, input: &str) -> Response {
    let ctx = Arc::new(context(&f.pin, f.storage.path(), None));
    ctx.update_config(|config| config.restrict_to_project_root = false);
    let request = crate::protocol::RawRequest {
        id: "confine-helper".into(),
        command: "read".into(),
        lsp_hints: None,
        session_id: Some(f.route.session.clone()),
        params: json!({"file":input}),
    };
    let helper_ctx = ctx.clone();
    let caller_thread = std::thread::current().id();
    with_call_policy(
        &ctx,
        &f.route,
        &request.id.clone(),
        "read",
        json!({}),
        |_| {
            crate::bounded_io::run(&f.pin, None, move |_| {
                assert_ne!(std::thread::current().id(), caller_thread);
                assert!(
                    confinement::for_request(&request.id),
                    "filesystem helper lost the confinement fence"
                );
                assert!(helper_ctx.request_force_restrict(&request.id));
                let asks = confinement::ask_calls();
                let response = crate::commands::read::handle_read(&request, &helper_ctx);
                assert_eq!(confinement::ask_calls(), asks);
                Ok(response)
            })
            .unwrap()
        },
    )
    .unwrap()
}

#[test]
fn confined_read_helper_inherits_fence_and_refuses_symlink_targets() {
    let f = Fixture::new();
    for input in ["escape/sentinel.rs", "escape-file"] {
        let response = read_on_filesystem_helper(&f, input);
        assert_eq!(
            response.data["code"], "path_outside_confined_root",
            "{response:?}"
        );
        assert!(response.data["message"].as_str().unwrap().contains(input));
        let text = serde_json::to_string(&response).unwrap();
        assert!(!text.contains(f.live.to_str().unwrap()));
        assert!(!text.contains("outside_secret"));
    }
}

#[test]
fn confined_in_root_symlink_is_allowed_and_walks_skip_outside_links() {
    let f = Fixture::new();
    std::os::unix::fs::symlink("only.rs", f.pin.join("safe-file.rs")).unwrap();
    for name in ["read", "grep", "glob", "outline"] {
        let response = f.call(name, arguments(name, "safe-file.rs"));
        assert!(response.success, "{name}: {response:?}");
        if name == "read" || name == "outline" {
            assert!(
                serde_json::to_string(&response)
                    .unwrap()
                    .contains("pinned_token"),
                "{name}: {response:?}"
            );
        } else {
            assert_eq!(
                assert_pinned_results(&f, name, &response),
                [f.pin.join("only.rs").display().to_string()]
            );
        }
    }
    std::os::unix::fs::symlink("../live/sentinel.rs", f.pin.join("outside-file.rs")).unwrap();
    for name in ["grep", "glob"] {
        let response = f.call(name, arguments(name, "."));
        assert_eq!(
            assert_pinned_results(&f, name, &response),
            [f.pin.join("only.rs").display().to_string()]
        );
    }
}

#[test]
fn confined_review_golden_vectors() {
    let f = Fixture::new();
    for (case, name, args) in pattern_cases(&f) {
        let response = f.call(name, args);
        assert_pinned_results(&f, name, &response);
        vector(
            &f,
            &format!("review-{case}"),
            serde_json::to_value(response).unwrap(),
        );
    }
    for (case, input) in [
        ("directory-symlink", "escape/sentinel.rs"),
        ("file-symlink", "escape-file"),
    ] {
        let response = read_on_filesystem_helper(&f, input);
        assert_eq!(response.data["code"], "path_outside_confined_root");
        vector(
            &f,
            &format!("review-read-helper-{case}"),
            serde_json::to_value(response).unwrap(),
        );
    }
    for name in ["grep", "glob"] {
        let response = f.call(name, arguments(name, "."));
        assert_eq!(
            assert_pinned_results(&f, name, &response),
            [f.pin.join("only.rs").display().to_string()]
        );
        vector(
            &f,
            &format!("review-{name}-walk-encounters-outside-symlink"),
            serde_json::to_value(response).unwrap(),
        );
    }
}
