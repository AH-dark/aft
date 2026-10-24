//! Validate opt-in result bodies and preserve legacy bytes over the same real replies.

use super::*;
use crate::run_tool_call::{
    finish_tool_call_response, run_tool_call, ToolCallContext, ToolCallOutcome, ToolCallResult,
};
use cortexkit_role_tool_provider::result::{validate, ToolResult};
use std::cell::Cell;

#[test]
fn content_v1_error_code_is_preserved_or_falls_back() {
    for (response, expected) in [
        (
            Response::error("test", "path_outside_root", "outside"),
            "path_outside_root",
        ),
        (
            Response {
                id: "test".into(),
                success: false,
                data: json!({}),
            },
            "tool_error",
        ),
    ] {
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            1,
            control_flags(),
            &ToolCallResult {
                text: "failed".into(),
                response,
            },
            opted_in(BindTrust::FirstParty),
        )
        .unwrap();
        let frame = project_frame(frame);
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["errorCode"], expected);
        validate(&serde_json::from_slice::<ToolResult>(&frame.body).unwrap()).unwrap();
    }
}

fn opted_in(trust: BindTrust) -> wire::ToolResponsePolicy {
    wire::ToolResponsePolicy {
        trust,
        tool_result_body: true,
    }
}

fn project_frame(mut outgoing: wire::WriterFrame) -> Frame {
    outgoing.project_tool_result().unwrap();
    outgoing.frame
}

#[test]
fn content_v1_error_code_normalization_and_success_omission() {
    for (raw, expected) in [
        ("HTTP-Error / 403", "http_error_403"),
        ("__Permission__Required__", "permission_required"),
        ("403", "error_403"),
        ("!?", "tool_error"),
    ] {
        let result = ToolCallResult {
            text: "failed".into(),
            response: Response::error("id", raw, "failed"),
        };
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            1,
            control_flags(),
            &result,
            opted_in(BindTrust::Untrusted),
        )
        .unwrap();
        let frame = project_frame(frame);
        let decoded: ToolResult = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(decoded.error_code.as_deref(), Some(expected));
        validate(&decoded).unwrap();
    }
    let result = ToolCallResult {
        text: String::new(),
        response: Response::success("id", json!({"code":"not_an_error"})),
    };
    let frame = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(1, 1),
        1,
        control_flags(),
        &result,
        opted_in(BindTrust::FirstParty),
    )
    .unwrap();
    let frame = project_frame(frame);
    let decoded: ToolResult = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(decoded.error_code, None);
    assert_eq!(decoded.content.len(), 1);
    validate(&decoded).unwrap();
}

#[test]
fn content_v1_serialized_size_is_strictly_below_three_mib() {
    let mut result = ToolCallResult {
        text: String::new(),
        response: Response::success("id", json!({})),
    };
    let empty = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(1, 1),
        7,
        control_flags(),
        &result,
        opted_in(BindTrust::Untrusted),
    )
    .unwrap();
    let empty = project_frame(empty);
    let capacity = result_body::MAX_RESULT_BYTES - empty.body.len();
    for (bytes, code) in [
        (capacity - 1, None),
        (capacity, Some("response_too_large")),
        (capacity + 1, Some("response_too_large")),
    ] {
        result.text = "x".repeat(bytes);
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            7,
            control_flags(),
            &result,
            opted_in(BindTrust::Untrusted),
        )
        .unwrap();
        let frame = project_frame(frame);
        assert_eq!(frame.header.corr, 7);
        let decoded: ToolResult = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(decoded.error_code.as_deref(), code);
        assert!(frame.body.len() < result_body::MAX_RESULT_BYTES);
        validate(&decoded).unwrap();
    }
}

fn scratch_project() -> tempfile::TempDir {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    tempfile::tempdir_in(scratch).unwrap()
}

#[test]
fn content_v1_images_are_ordered_and_kept_in_structured_content() {
    let project = scratch_project();
    let root = project.path();
    let path = root.join("small.png");
    image::RgbImage::new(8, 7).save(&path).unwrap();
    let ctx = context(root);
    let _budget = result_body::enter(true);
    let result = run(&ctx, root, "read", json!({"path": path}), &|req, ctx| {
        crate::commands::read::handle_read(&req, ctx)
    });
    assert!(result.response.success, "{:?}", result.response);
    for trust in [BindTrust::FirstParty, BindTrust::Untrusted] {
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(1, 1),
            1,
            control_flags(),
            &result,
            opted_in(trust),
        )
        .unwrap();
        let frame = project_frame(frame);
        validate(&serde_json::from_slice::<ToolResult>(&frame.body).unwrap()).unwrap();
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["content"][0]["type"], "text");
        assert_eq!(body["content"][1]["type"], "image");
        assert_eq!(body["content"][1]["width"], 8);
        assert_eq!(body["content"][1]["height"], 7);
        assert_eq!(body["content"][1]["mimeType"], "image/png");
        assert!(body["content"][1]["alt"]
            .as_str()
            .unwrap()
            .contains("small.png"));
        if trust == BindTrust::FirstParty {
            assert_eq!(
                body["content"][1]["data"],
                body["structuredContent"]["attachments"][0]["data"]
            );
        }
    }
}

#[test]
fn content_v1_builders_refuse_large_images_and_bound_ast_matches() {
    let project = scratch_project();
    let root = project.path();
    let ctx = context(root);
    let _budget = result_body::enter(true);
    let mut state = 0x12345678u32;
    let image = image::RgbImage::from_fn(1024, 1024, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8])
    });
    let path = root.join("large.png");
    image.save(&path).unwrap();
    let result = run(&ctx, root, "read", json!({"path":path}), &|req, ctx| {
        crate::commands::read::handle_read(&req, ctx)
    });
    assert!(!result.response.success);
    assert_eq!(result.response.data["code"], "response_too_large");
    assert!(result.text.contains("a smaller image"));
    let ast = root.join("large.js");
    std::fs::write(&ast, "console.log(1);\n".repeat(24000)).unwrap();
    let result = run(
        &ctx,
        root,
        "ast_search",
        json!({"pattern":"console.log($X)","lang":"javascript","paths":[ast]}),
        &|req, ctx| crate::commands::ast_search::handle_ast_search(&req, ctx),
    );
    assert!(result.response.success);
    assert_eq!(result.response.data["total_matches"], 24000);
    let shown = result.response.data["matches"].as_array().unwrap().len();
    assert!(shown > 0 && shown < 24000);
    assert_eq!(result.response.data["complete"], false);
    assert_eq!(
        result.response.data["matches_list_envelope"]["shown"],
        shown
    );
    assert!(result.text.contains("shown "));
    assert!(result.text.contains("24000 results (budget)"));
    let frame = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(1, 1),
        1,
        control_flags(),
        &result,
        opted_in(BindTrust::FirstParty),
    )
    .unwrap();
    let frame = project_frame(frame);
    let decoded: ToolResult = serde_json::from_slice(&frame.body).unwrap();
    assert_ne!(
        decoded.is_error,
        Some(true),
        "bounded search should fit without losing its prefix"
    );
    validate(&decoded).unwrap();
}

#[test]
fn content_v1_catalog_switch_preserves_schemas_digests_and_pins() {
    for preset in tool_provider::CatalogPreset::ALL {
        let original = tool_provider::tools_for_session(preset, &[], true, true, true);
        let mut enabled = original.clone();
        tool_provider::declare_result_format(&mut enabled, true);
        assert!(!enabled.is_empty());
        for (before, after) in original.iter().zip(&enabled) {
            assert_eq!(
                after.result_format.as_ref().unwrap().as_str(),
                result_body::RESULT_FORMAT
            );
            let mut after = serde_json::to_value(after).unwrap();
            after.as_object_mut().unwrap().remove("result_format");
            assert_eq!(
                serde_json::to_value(before).unwrap(),
                after,
                "only result_format may change"
            );
        }
        assert!(original.iter().all(|tool| tool.result_format.is_none()));
    }
}

#[tokio::test]
async fn content_v1_capability_is_selected_per_bind_not_per_call() {
    let project = scratch_project();
    let root = project.path().to_path_buf();
    let module_root = root.clone();
    let (mut daemon, module) = tokio::io::duplex(64 * 1024);
    let module_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (read, write) = tokio::io::split(module);
        runtime.block_on(run_module_loop(
            read,
            write,
            &module_root.join("absent-connection.json"),
            App::default_shared(),
            Arc::new(Executor::new()),
            |request, _| {
                if request.command == "configure" {
                    return Response::success(request.id, json!({}));
                }
                Response::error_with_data(
                    "same-id",
                    "path_outside_root",
                    "outside root",
                    json!({"producer_budget": result_body::active()}),
                )
            },
            Some(module_root.join("absent-user-config.json")),
            false,
            usize::MAX,
            None,
            &module_root.join("storage"),
            QuiescedLsp::ReapInBackground,
            None,
            None,
        ))
    });
    async fn receive(daemon: &mut tokio::io::DuplexStream, channel: u16, corr: u64) -> Frame {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let frame = read_frame(daemon).await.unwrap().unwrap();
                if frame.header.channel == channel && frame.header.corr == corr {
                    return frame;
                }
            }
        })
        .await
        .expect("route response timed out")
    }
    let hello = receive(&mut daemon, 0, HELLO_CORR).await;
    assert_eq!(hello.header.ty, FrameType::Hello);
    write_frame(
        &mut daemon,
        &Frame::build(
            FrameType::HelloAck,
            control_flags(),
            0,
            0,
            HELLO_CORR,
            b"{}".to_vec(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let mut replies = Vec::new();
    for (channel, capabilities) in [
        (1, Some(vec![result_body::RESULT_FORMAT.to_owned()])),
        (2, Some(vec!["content/v1".to_owned()])),
        (3, None),
    ] {
        let bind = ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: subc_protocol::BindIdentity::new(&root, "opencode", "result-bodies"),
            principal: Some(Principal::Direct),
            consumer_capabilities: capabilities,
            admission_facts: Default::default(),
            scope: None,
            role_versions: None,
            call_mode: None,
        };
        write_frame(
            &mut daemon,
            &Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                channel as u64,
                serde_json::to_vec(&bind).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let ack = receive(&mut daemon, 0, channel as u64).await;
        assert_eq!(
            ack.header.ty,
            FrameType::Response,
            "bind refused: {}",
            String::from_utf8_lossy(&ack.body)
        );
        assert!(matches!(
            serde_json::from_slice::<ModuleControlResponse>(&ack.body).unwrap(),
            ModuleControlResponse::RouteBindAck {}
        ));
        // Both calls on a route must agree, even when a call claims the opposite capability.
        for spoof in [false, true] {
            let call = json!({"name":"read", "arguments":{"path":"missing.txt"}, "consumer_capabilities": if spoof { vec![result_body::RESULT_FORMAT] } else { vec![] }});
            write_frame(
                &mut daemon,
                &Frame::build(
                    FrameType::Request,
                    control_flags(),
                    channel,
                    1,
                    10 + u64::from(spoof),
                    serde_json::to_vec(&call).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            replies.push((
                channel,
                receive(&mut daemon, channel, 10 + u64::from(spoof)).await,
            ));
        }
    }
    drop(daemon);
    module_thread.join().unwrap().unwrap();
    for (channel, frame) in replies {
        assert_eq!(frame.header.ty, FrameType::Response);
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["isError"], true);
        assert_eq!(body["structuredContent"]["producer_budget"], channel == 1);
        if channel == 1 {
            assert_eq!(body["errorCode"], "path_outside_root");
            validate(&serde_json::from_slice::<ToolResult>(&frame.body).unwrap()).unwrap();
        } else {
            assert!(
                body.get("errorCode").is_none(),
                "legacy route must ignore call capability claims"
            );
        }
    }
}

#[test]
fn content_v1_retained_legacy_replies_use_the_current_route() {
    for trust in [BindTrust::FirstParty, BindTrust::Untrusted] {
        let result = ToolCallResult {
            text: "outside root".into(),
            response: Response::error("old-record", "path_outside_root", "outside root"),
        };
        let legacy = wire::tests::legacy_tool_response_body(&result, trust != BindTrust::Untrusted);
        let stored = crate::db::call_ledger::RecordedFrame {
            ty: FrameType::Response,
            body: legacy.clone(),
        };
        for enabled in [false, true] {
            let frame =
                Frame::build(stored.ty, control_flags(), 4, 2, 19, stored.body.clone()).unwrap();
            let outgoing = wire::WriterFrame::retained_tool_response(
                frame,
                wire::ToolResponsePolicy {
                    trust,
                    tool_result_body: enabled,
                },
                None,
            );
            let frame = project_frame(outgoing);
            if enabled {
                let body: ToolResult = serde_json::from_slice(&frame.body).unwrap();
                validate(&body).unwrap();
                assert_eq!(
                    body.error_code.as_deref(),
                    Some(if trust == BindTrust::Untrusted {
                        "tool_error"
                    } else {
                        "path_outside_root"
                    })
                );
            } else {
                assert_eq!(
                    frame.body, legacy,
                    "pre-switch record must replay byte for byte on a legacy route"
                );
            }
        }
        assert_eq!(
            stored.body, legacy,
            "projection must not mutate retained bytes"
        );
    }
    let oversized = ToolCallResult {
        text: "large sidecar".into(),
        response: Response::success(
            "old-large",
            json!({"data":"x".repeat(result_body::MAX_RESULT_BYTES)}),
        ),
    };
    let legacy = wire::tests::legacy_tool_response_body(&oversized, true);
    let outgoing = wire::WriterFrame::retained_tool_response(
        Frame::build(FrameType::Response, control_flags(), 1, 1, 1, legacy).unwrap(),
        opted_in(BindTrust::FirstParty),
        None,
    );
    let frame = project_frame(outgoing);
    let body: ToolResult = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(body.error_code.as_deref(), Some("response_too_large"));
    validate(&body).unwrap();

    use base64::Engine;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 7))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let github_image = ToolCallResult {
        text: "read image".into(),
        response: Response::success(
            "old-github-image",
            json!({"attachments":[{"kind":"image","mime":"image/png","data":base64::engine::general_purpose::STANDARD.encode(bytes.into_inner()),"source_url":"https://github.com/user-attachments/files/7/image.png"}]}),
        ),
    };
    let legacy = wire::tests::legacy_tool_response_body(&github_image, true);
    let outgoing = wire::WriterFrame::retained_tool_response(
        Frame::build(
            FrameType::Response,
            control_flags(),
            1,
            1,
            1,
            legacy.clone(),
        )
        .unwrap(),
        opted_in(BindTrust::FirstParty),
        Some("issue://7"),
    );
    let frame = project_frame(outgoing);
    let body: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(body["content"][1]["width"], 8);
    assert_eq!(body["content"][1]["height"], 7);
    assert_eq!(body["content"][1]["alt"], "issue://7");
    validate(&serde_json::from_slice::<ToolResult>(&frame.body).unwrap()).unwrap();
    assert!(
        !serde_json::from_slice::<Value>(&legacy).unwrap()["structuredContent"]["attachments"][0]
            .as_object()
            .unwrap()
            .contains_key("width")
    );
}

#[tokio::test]
async fn content_v1_ledger_records_before_projection_and_replays_by_route() {
    use crate::db::call_ledger as ledger;
    let (dir, root) = test_support::test_root("result-projection-ledger");
    let ctx = test_support::test_ctx();
    let path = dir.path().join("aft.db");
    ctx.set_db(Arc::new(StdMutex::new(crate::db::open(&path).unwrap())));
    let executor = Arc::new(Executor::new());
    executor.register_actor(root.clone(), ctx);
    let identity = |enabled| {
        let mut identity = test_support::route_identity(&root, "result-projection");
        let data = Arc::get_mut(&mut identity.0).unwrap();
        data.role = tool_provider::RouteRole::ToolProviderV1;
        data.tool_result_body = enabled;
        identity
    };
    let key = tool_provider::LedgerEdge::key(&identity(true), "retained-result");
    let (edge, mut ready) = tool_provider::LedgerEdge::new();
    let metrics = Arc::new(DispatchPathMetrics::new());
    let (actual, receiver) = mpsc::channel(10);
    let (pong, pong_receiver) = mpsc::channel(1);
    let (mut daemon, module) = tokio::io::duplex(64 * 1024);
    let output = spawn_writer_task(module, receiver, pong_receiver, metrics.clone());
    let (writer, proxy) = edge.proxy(actual, executor.clone(), metrics.clone());
    let decode = |corr| {
        DecodedFrame {
        parsed_body: Default::default(), confinement: None, launch_restore: None, codemode_policies: None,
        frame: Frame::build(FrameType::Request, control_flags(), 42, 1, corr,
            serde_json::to_vec(&json!({"name":"read","arguments":{"path":"missing.txt"},"call_key":"retained-result"})).unwrap()).unwrap(),
        phase_trace: PhaseTrace::new(Instant::now()),
    }
    };
    assert!(edge
        .defer(
            decode(7),
            &HashMap::from([(route_key(42, 1), identity(true))]),
            &executor,
            &writer,
            &metrics
        )
        .is_ok());
    tokio::time::timeout(Duration::from_secs(5), ready.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(edge
        .defer(
            decode(10),
            &HashMap::from([(route_key(42, 1), identity(true))]),
            &executor,
            &writer,
            &metrics
        )
        .is_ok());
    // The actor barrier makes the duplicate observe the pending row before the
    // original reply settles it, rather than accidentally testing a settled replay.
    executor
        .submit_async(
            root.clone(),
            Lane::Mutating,
            "admission-barrier".into(),
            Box::new(|_| Response::success("admission-barrier", json!({}))),
        )
        .await
        .unwrap();
    let result = ToolCallResult {
        text: "outside root".into(),
        response: Response::error("retained-id", "path_outside_root", "outside root"),
    };
    let legacy = wire::tests::legacy_tool_response_body(&result, true);
    let outgoing = wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        route_key(42, 1),
        7,
        control_flags(),
        &result,
        opted_in(BindTrust::FirstParty),
    )
    .unwrap();
    send_reliable_writer_frame(&writer, &metrics, outgoing, "recording-order test")
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut daemon))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&first.body).unwrap()["errorCode"],
        "path_outside_root"
    );
    let attached = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut daemon))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        std::collections::BTreeSet::from([first.header.corr, attached.header.corr]),
        std::collections::BTreeSet::from([7, 10])
    );
    let attached_body: ToolResult = serde_json::from_slice(&attached.body).unwrap();
    assert_eq!(
        attached_body.error_code.as_deref(),
        Some("path_outside_root")
    );
    validate(&attached_body).unwrap();
    let conn = crate::db::open(&path).unwrap();
    let stored = ledger::get(&conn, &key).unwrap().unwrap().frame.unwrap();
    assert_eq!(
        stored.body, legacy,
        "the ledger must retain pre-projection legacy bytes"
    );
    for (enabled, corr) in [(false, 8), (true, 9)] {
        assert!(edge
            .defer(
                decode(corr),
                &HashMap::from([(route_key(42, 1), identity(enabled))]),
                &executor,
                &writer,
                &metrics
            )
            .is_ok());
        let reply = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut daemon))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(reply.header.corr, corr);
        if enabled {
            let decoded: ToolResult = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(decoded.error_code.as_deref(), Some("path_outside_root"));
            validate(&decoded).unwrap();
        } else {
            assert_eq!(
                reply.body, legacy,
                "legacy replay must use the stored bytes verbatim"
            );
        }
    }
    drop(writer);
    proxy.await.unwrap();
    drop(pong);
    output.await.unwrap().unwrap();
}

thread_local! {
    static REPLIES: Cell<usize> = const { Cell::new(0) };
    static CODES: std::cell::RefCell<std::collections::BTreeSet<String>> = const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

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
        let frame = project_frame(frame);
        assert_eq!(
            frame.body,
            wire::tests::legacy_tool_response_body(result, trust != BindTrust::Untrusted),
            "legacy bytes: {tool}/{shape}/{label}"
        );
        let frame = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            route_key(2, 1),
            1,
            control_flags(),
            result,
            opted_in(trust),
        )
        .unwrap();
        let frame = project_frame(frame);
        let decoded: ToolResult = serde_json::from_slice(&frame.body).unwrap();
        validate(&decoded).unwrap_or_else(|problem| panic!("{tool}/{shape}/{label}: {problem}"));
        assert!(frame.body.len() < result_body::MAX_RESULT_BYTES);
        REPLIES.set(REPLIES.get() + 1);
        if let Some(code) = result.response.data["code"].as_str() {
            CODES.with_borrow_mut(|codes| {
                codes.insert(code.to_owned());
            });
        }
        println!(
            "| {tool} | {shape} | {label} | {} | {} | validates |",
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
fn content_v1_real_reply_validation_report() {
    REPLIES.set(0);
    CODES.with_borrow_mut(|codes| codes.clear());
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
    #[cfg(unix)]
    assert_eq!(REPLIES.get(), 264, "the real-reply corpus must not shrink");
    println!(
        "Validated {} opted-in replies and {} byte-identical legacy replies; machine codes: {:?}",
        REPLIES.get(),
        REPLIES.get(),
        CODES.with_borrow(|codes| codes.clone())
    );
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
