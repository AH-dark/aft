//! Validate existing wire bodies without declaring result_format on any catalog entry.

use super::*;
use crate::run_tool_call::{
    finish_tool_call_response, run_tool_call, ToolCallContext, ToolCallOutcome, ToolCallResult,
};
use cortexkit_role_tool_provider::result::{validate, ToolResult};
use std::cell::Cell;

fn context(root: &Path) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.to_path_buf());
    config.storage_dir = Some(root.join(".storage"));
    config.harness = Some(crate::harness::Harness::Opencode);
    config.experimental_bash_background = true;
    config.restrict_to_project_root = true;
    config.disabled_tools.clear();
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_harness(crate::harness::Harness::Opencode);
    ctx
}

fn run(
    ctx: &AppContext,
    root: &Path,
    tool: &str,
    args: Value,
    dispatch: &crate::run_tool_call::DispatchFn<'_>,
) -> ToolCallResult {
    let format = crate::subc_format::FormatContext::from_tool_call(tool, &args, root);
    let call = ToolCallContext {
        project_root: root.to_path_buf(),
        session_id: Some("content-v1-audit".into()),
        request_id: "content-v1-audit".into(),
        diagnostics_on_edit: false,
        preview: false,
        edit_slot_survives: None,
        report_registration_downgrade: false,
        standard_edit_grammar: true,
        disabled_tools: Some(Arc::new(Vec::new())),
        worker_session: true,
        host_tool_names: None,
    };
    let ToolCallOutcome::Unary(result) = run_tool_call(
        tool,
        args,
        &format,
        &call,
        ctx,
        dispatch,
        Some(&|response, text| finalize(ctx, tool, response, text)),
        None,
    );
    result
}

fn finalize(ctx: &AppContext, tool: &str, response: &mut Response, text: &mut String) {
    crate::response_finalize::finalize_tool_response(
        response,
        text,
        ctx,
        "content-v1-audit",
        tool,
        false,
    );
}

fn record(tool: &str, shape: &str, result: &ToolCallResult) {
    for (label, trust) in [
        ("trusted", BindTrust::FirstParty),
        ("untrusted", BindTrust::Untrusted),
    ] {
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            1,
            Flags::new(false, Priority::Interactive, false),
            result,
            trust,
        )
        .unwrap();
        let decoded: ToolResult = serde_json::from_slice(&frame.body).unwrap();
        let outcome = validate(&decoded)
            .map(|()| "validates".to_string())
            .unwrap_or_else(|problem| format!("{}: {}", problem.member, problem.reason));
        println!(
            "| {tool} | {shape} | {label} | {} | {} | {outcome} |",
            frame.body.len(),
            result
                .response
                .data
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("—"),
        );
    }
}

fn expand(value: Value, root: &Path) -> Value {
    match value {
        Value::String(text) => {
            Value::String(text.replace("<PROJECT_ROOT>", &root.to_string_lossy()))
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|item| expand(item, root)).collect())
        }
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, value)| (key, expand(value, root)))
                .collect(),
        ),
        other => other,
    }
}

fn native_response(mut flat: Value) -> Response {
    let object = flat.as_object_mut().unwrap();
    let id = object
        .remove("id")
        .and_then(|id| id.as_str().map(str::to_owned))
        .unwrap();
    let success = object
        .remove("success")
        .and_then(|success| success.as_bool())
        .unwrap();
    Response {
        id,
        success,
        data: flat,
    }
}

#[test]
#[ignore = "observational content/v1 readiness audit; prints current failures, not an opt-in gate"]
fn content_v1_real_reply_validation_report() {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let project = tempfile::tempdir_in(&scratch).unwrap();
    let root = project.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.ts"), "const value = 1;\n").unwrap();
    let ctx = context(root);
    println!("| Tool | Shape/source | Bind | Wire bytes | Native code | Validator result |");
    println!("| --- | --- | --- | ---: | --- | --- |");

    // Replay captured native responses at the dispatch boundary, not handwritten envelopes.
    let fixture_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/subc_parity/format");
    let mut fixtures: Vec<_> = std::fs::read_dir(fixture_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    fixtures.sort();
    let mut covered = std::collections::BTreeSet::new();
    for fixture in &fixtures {
        let input: Value =
            serde_json::from_slice(&std::fs::read(fixture.join("input.json")).unwrap()).unwrap();
        let tool = input["tool_name"].as_str().unwrap();
        let args = expand(input["ctx"]["agent_args"].clone(), root);
        let native = expand(input["native_response_json"].clone(), root);
        let result = if tool == "inspect" {
            // run_tool_call hard-codes inspect dispatch. Replay its captured responses through
            // finish_tool_call_response, the shared renderer used after execution.
            let format = crate::subc_format::FormatContext::from_tool_call(tool, &args, root);
            finish_tool_call_response(
                tool,
                &format,
                native_response(native),
                false,
                Some(&|response, text| finalize(&ctx, tool, response, text)),
                None,
            )
        } else {
            let dispatched = Cell::new(false);
            let result = run(&ctx, root, tool, args, &|_, _| {
                dispatched.set(true);
                native_response(native.clone())
            });
            assert!(
                dispatched.get(),
                "fixture {fixture:?} failed before native response replay: {:?}",
                result.response
            );
            result
        };
        covered.insert(tool.to_string());
        record(
            tool,
            fixture.file_name().unwrap().to_str().unwrap(),
            &result,
        );
    }
    assert!(
        fixtures.len() >= 90,
        "audit must not silently miss the parity corpus"
    );

    let catalog = tool_provider::tools_for_session(
        tool_provider::CatalogPreset::Worker,
        &[],
        true,
        true,
        true,
    );
    for tool in catalog {
        let result = run(&ctx, root, &tool.name, json!([]), &|_, _| {
            panic!("invalid arguments dispatched")
        });
        assert!(
            !result.response.success,
            "{} failed to exercise an error",
            tool.name
        );
        record(&tool.name, "live invalid arguments", &result);
    }
    println!(
        "Parity corpus: {} fixtures, {} tool names",
        fixtures.len(),
        covered.len()
    );
    live_cases(Arc::new(ctx), root, &scratch);
}

#[cfg(unix)]
fn resolve(ctx: &AppContext, outcome: crate::response_finalize::DispatchOutcome) -> Response {
    match outcome {
        crate::response_finalize::DispatchOutcome::Immediate(response) => response,
        crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(response) = (pending.poll)(ctx) {
                    break response;
                }
                assert!(Instant::now() < deadline, "shell outcome did not resolve");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(unix)]
fn live_cases(ctx: Arc<AppContext>, root: &Path, scratch: &Path) {
    struct Shutdown(Arc<AppContext>);
    impl Drop for Shutdown {
        fn drop(&mut self) {
            self.0.bash_background().shutdown();
        }
    }
    let _shutdown = Shutdown(Arc::clone(&ctx));
    let dispatch = |request: RawRequest, ctx: &AppContext| match request.command.as_str() {
        "read" => crate::commands::read::handle_read(&request, ctx),
        "bash" => resolve(
            ctx,
            crate::commands::bash_orchestrate::build_bash_outcome(
                &request,
                ctx,
                crate::commands::bash::handle(&request, ctx),
            ),
        ),
        "bash_status" => crate::commands::bash_status::handle(&request, ctx),
        "bash_kill" => crate::commands::bash_kill::handle(&request, ctx),
        "bash_write" => crate::commands::bash_write::handle(&request, ctx),
        "ast_search" => crate::commands::ast_search::handle_ast_search(&request, ctx),
        other => panic!("unexpected live command: {other}"),
    };
    let external = tempfile::tempdir_in(scratch).unwrap();
    let outside = external.path().join("outside.txt");
    std::fs::write(&outside, "external content").unwrap();
    let result = run(&ctx, root, "read", json!({"path": outside}), &dispatch);
    assert_eq!(result.response.data["code"], "path_outside_root");
    record("read", "live path_outside_root", &result);

    ctx.update_config(|config| config.restrict_to_project_root = false);
    let result = {
        let owner = Arc::new(());
        let _ask =
            crate::access_policy::ask_route_guard(Some(crate::access_policy::AskRoute::Plugin {
                scope: crate::access_policy::plugin::Scope::new(&owner, "content-v1-audit"),
                once: Vec::new(),
            }));
        run(&ctx, root, "read", json!({"path": outside}), &dispatch)
    };
    assert_eq!(result.response.data["code"], "permission_required");
    record("read", "live permission_required", &result);

    let denied = root.join("denied.txt");
    std::fs::write(&denied, "denied content").unwrap();
    ctx.update_config(|config| {
        config.restrict_to_project_root = true;
        config.sandbox.read_deny = vec![denied.clone()];
    });
    let result = run(&ctx, root, "read", json!({"path": denied}), &dispatch);
    assert_eq!(result.response.data["code"], "denied_floor");
    record("read", "live denied_floor", &result);
    ctx.update_config(|config| config.sandbox.read_deny.clear());

    let foreground = run(
        &ctx,
        root,
        "bash",
        json!({"command":"printf foreground-ok"}),
        &dispatch,
    );
    assert!(foreground.response.success, "{:?}", foreground.response);
    assert!(
        foreground.text.contains("foreground-ok"),
        "foreground must include completed stdout"
    );
    record("bash", "live foreground", &foreground);
    let failed = run(
        &ctx,
        root,
        "bash",
        json!({"command":"printf command-failed; exit 7"}),
        &dispatch,
    );
    assert_eq!(failed.response.data["exit_code"], 7);
    record("bash", "live foreground nonzero exit", &failed);
    let powershell = run(
        &ctx,
        root,
        "powershell",
        json!({"command":"Write-Output foreground-ok"}),
        &|_, _| native_response(serde_json::to_value(&foreground.response).unwrap()),
    );
    record(
        "powershell",
        "foreground native shell response replay",
        &powershell,
    );

    let background = run(
        &ctx,
        root,
        "bash",
        json!({"command":"printf ready; sleep 30", "background":true}),
        &dispatch,
    );
    assert!(background.response.success, "{:?}", background.response);
    record("bash", "live background launch", &background);
    let task = background.response.data["task_id"].as_str().unwrap();
    record(
        "bash_status",
        "live running",
        &run(&ctx, root, "bash_status", json!({"taskId":task}), &dispatch),
    );
    let watched = run(
        &ctx,
        root,
        "bash_watch",
        json!({"taskId":task,"pattern":"ready","timeoutMs":5000}),
        &|request, _| match crate::commands::bash_watch::handle_deferred(&request, Arc::clone(&ctx))
        {
            crate::response_finalize::DispatchOutcome::Immediate(response) => response,
            crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if let Some(response) = (pending.poll)(&ctx) {
                        break response;
                    }
                    assert!(Instant::now() < deadline, "bash_watch did not resolve");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        },
    );
    assert!(watched.response.success, "{:?}", watched.response);
    record("bash_watch", "live pattern match", &watched);
    record(
        "bash_kill",
        "live kill",
        &run(&ctx, root, "bash_kill", json!({"taskId":task}), &dispatch),
    );

    let pty = run(
        &ctx,
        root,
        "bash",
        json!({"command":"cat","pty":true}),
        &dispatch,
    );
    assert!(pty.response.success, "{:?}", pty.response);
    let task = pty.response.data["task_id"].as_str().unwrap();
    let written = run(
        &ctx,
        root,
        "bash_write",
        json!({"taskId":task,"input":"hello\n"}),
        &dispatch,
    );
    assert!(written.response.success, "{:?}", written.response);
    record("bash_write", "live PTY write", &written);
    let _ = run(&ctx, root, "bash_kill", json!({"taskId":task}), &dispatch);

    let large_bash = run(
        &ctx,
        root,
        "bash",
        json!({"command":"python3 -c \"print('x' * 4000000)\"","compressed":false}),
        &dispatch,
    );
    assert!(large_bash.response.success, "{:?}", large_bash.response);
    assert_eq!(large_bash.response.data["exit_code"], 0);
    assert!(
        large_bash.text.contains("xxxx"),
        "large output must have been captured"
    );
    record("bash", "live 4MB stdout", &large_bash);

    let mut state = 0x12345678u32;
    let image = image::RgbImage::from_fn(1024, 1024, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8])
    });
    let image_path = root.join("large.png");
    image.save(&image_path).unwrap();
    let result = run(&ctx, root, "read", json!({"path":image_path}), &dispatch);
    assert!(result.response.success, "{:?}", result.response);
    assert!(
        result.response.data["attachments"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "large image must be attached, not omitted"
    );
    record("read", "live large image", &result);

    let ast_path = root.join("large.js");
    std::fs::write(&ast_path, "console.log(1);\n".repeat(24000)).unwrap();
    let result = run(
        &ctx,
        root,
        "ast_search",
        json!({"pattern":"console.log($X)","lang":"javascript","paths":[ast_path]}),
        &dispatch,
    );
    assert!(result.response.success, "{:?}", result.response);
    record("ast_search", "live 24000 matches", &result);
    assert_eq!(
        result.response.data["matches"].as_array().unwrap().len(),
        24000
    );
}

#[cfg(not(unix))]
fn live_cases(_: Arc<AppContext>, _: &Path, _: &Path) {
    println!("Live shell cases require Unix; replay cases remain available on this host.");
}
