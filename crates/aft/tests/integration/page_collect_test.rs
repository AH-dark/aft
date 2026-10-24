//! `page.collect`, `page.read` and `page.release` through the real module
//! loop: core's management route asks for a session's page, and the reply
//! lists exactly what that session's read policy lets through.
//!
//! With `PAGE_COLLECT_PRINT_SAMPLES=1` the test prints every request and
//! reply as `PAGE_COLLECT_SAMPLE <name> <json>` lines; those are the
//! samples core and the phone app are built against.

use super::*;
use base64::Engine;

const HARNESS: &str = "test-harness";
const SESSION: &str = "page-session";
const CORE_CHANNEL: u16 = 12;
const OTHER_CHANNEL: u16 = 13;

fn sample(name: &str, request: &Value, reply: &Value) {
    if std::env::var_os("PAGE_COLLECT_PRINT_SAMPLES").is_some() {
        println!(
            "PAGE_COLLECT_SAMPLE {name} {}",
            json!({"request": request, "reply": reply})
        );
    }
}

struct Site {
    corr: u64,
}

impl Site {
    async fn call(
        &mut self,
        stream: &mut tokio::net::TcpStream,
        channel: u16,
        op: &str,
        params: Value,
    ) -> (Value, Value) {
        self.corr += 1;
        let request = json!({"op": op, "params": params});
        send_management_request(stream, channel, self.corr, request.clone()).await;
        let reply = read_management_response(stream, channel, self.corr, op).await;
        (request, reply)
    }

    /// `page.read` answers with a binary frame of raw bytes, or a JSON
    /// refusal.
    async fn read(
        &mut self,
        stream: &mut tokio::net::TcpStream,
        params: Value,
    ) -> (Value, Result<Vec<u8>, Value>, Value) {
        self.corr += 1;
        let request = json!({"op": "page.read", "params": params});
        send_management_request(stream, CORE_CHANNEL, self.corr, request.clone()).await;
        let frame = read_frame_timeout(stream, "page.read reply").await;
        assert_eq!(frame.header.ty, FrameType::Response);
        assert_eq!(frame.header.channel, CORE_CHANNEL);
        assert_eq!(frame.header.corr, self.corr);
        if frame.header.flags.is_binary() {
            let shown = json!({
                "frame_flags_binary": true,
                "body_len": frame.body.len(),
                "body_base64": base64::engine::general_purpose::STANDARD.encode(&frame.body),
            });
            (request, Ok(frame.body), shown)
        } else {
            let reply: Value = serde_json::from_slice(&frame.body).unwrap();
            (request, Err(reply.clone()), reply)
        }
    }
}

fn collect_params(entry: &str) -> Value {
    json!({
        "harness": HARNESS,
        "session": SESSION,
        "entry": entry,
        "limits": {
            "entry_max_bytes": 2_097_152, "max_files": 64, "max_file_bytes": 8_388_608,
            "max_total_bytes": 20_971_520, "import_depth": 4, "deadline_ms": 5000,
        },
    })
}

fn write_site(root: &std::path::Path) {
    let site = root.join("site");
    std::fs::create_dir_all(site.join("css")).unwrap();
    std::fs::create_dir_all(site.join("img")).unwrap();
    std::fs::write(
        site.join("index.html"),
        r#"<!doctype html>
<html><head>
  <link rel="stylesheet" href="css/site.css">
  <link rel="icon" href="img/icon.png">
  <link rel="stylesheet" href="https://cdn.example.com/x.css">
</head><body>
  <img src="img/logo%20mark.png" srcset="img/logo%20mark.png 1x, img/logo@2x.png 2x">
  <img src="data:image/png;base64,AAAA">
  <img src="img/missing.png">
  <img src="cache.db">
  <img src="../../outside.png">
  <script src="app.js?v=3"></script>
</body></html>
"#,
    )
    .unwrap();
    std::fs::write(
        site.join("css/site.css"),
        "@import 'theme.css';\nbody { background: url(../img/bg.png); }\n",
    )
    .unwrap();
    std::fs::write(site.join("css/theme.css"), ".t { color: red; }\n").unwrap();
    std::fs::write(site.join("img/logo mark.png"), b"\x89PNG logo").unwrap();
    std::fs::write(site.join("img/logo@2x.png"), b"\x89PNG logo2x").unwrap();
    std::fs::write(site.join("img/icon.png"), b"\x89PNG icon").unwrap();
    std::fs::write(site.join("img/bg.png"), b"\x89PNG bg").unwrap();
    std::fs::write(site.join("app.js"), "console.log('page');\n").unwrap();
    std::fs::write(site.join("cache.db"), "SQLite format 3\0").unwrap();
    // Outside the bound root: the session's policy refuses it.
    std::fs::write(root.parent().unwrap().join("outside.png"), b"outside").unwrap();
    std::fs::write(root.parent().unwrap().join("outside.html"), "<html>").unwrap();
    std::fs::write(site.join("notes.txt"), "not a page").unwrap();
    std::fs::write(
        site.join("big.html"),
        format!("<html>{}</html>", "x".repeat(64)),
    )
    .unwrap();
}

async fn bind_tool_route(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    root: &std::path::Path,
    session: &str,
    principal: Principal,
) {
    send_route_bind_with_harness_session_principal_and_doc(
        stream,
        channel,
        channel as u64 + 900,
        root,
        HARNESS,
        session,
        Some(principal),
        json!({"indexes": {"trigram": false, "semantic": false, "callgraph": false}}),
    )
    .await;
    expect_route_bind_ack(stream, channel as u64 + 900).await;
}

fn code(reply: &Value) -> &str {
    assert_eq!(reply["status"], "error", "{reply}");
    reply["data"]["code"].as_str().unwrap()
}

fn file_hashes(reply: &Value) -> Vec<(String, String)> {
    reply["data"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["path"].as_str().unwrap().to_string(),
                file["sha256"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

async fn drive_page_collect_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    // The session's bind root is a folder inside the temp root, so the
    // files beside it are outside the session's reach.
    let project = root1.join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_site(&project);
    bind_tool_route(&mut stream, 1, &project, SESSION, Principal::Direct).await;
    send_management_route_bind(
        &mut stream,
        CORE_CHANNEL,
        20,
        &root1,
        Some(Principal::Reserved {
            module_id: "prefrontal-core".into(),
        }),
    )
    .await;
    expect_route_bind_ack(&mut stream, 20).await;
    send_management_route_bind(
        &mut stream,
        OTHER_CHANNEL,
        21,
        &root1,
        Some(Principal::Reserved {
            module_id: "prefrontal".into(),
        }),
    )
    .await;
    expect_route_bind_ack(&mut stream, 21).await;
    let mut site = Site { corr: 1000 };
    let stream = &mut stream;

    // A bundle with omissions.
    let (request, first) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.collect",
            collect_params("site/index.html"),
        )
        .await;
    sample("collect-bundle-with-omitted", &request, &first);
    assert_eq!(first["status"], "ok", "{first}");
    let data = &first["data"];
    assert_eq!(data["entry"], "index.html");
    let paths = file_hashes(&first)
        .into_iter()
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "index.html",
            "css/site.css",
            "img/icon.png",
            "img/logo mark.png",
            "img/logo@2x.png",
            "app.js",
            "css/theme.css",
            "img/bg.png",
        ]
    );
    assert_eq!(
        data["omitted"],
        json!([
            {"path": "https://cdn.example.com/x.css", "reason": "not_relative"},
            {"path": "data:image/png;base64,AAAA", "reason": "not_relative"},
            {"path": "img/missing.png", "reason": "missing"},
            {"path": "cache.db", "reason": "denied"},
            {"path": "../../outside.png", "reason": "denied"},
        ])
    );
    let snapshot = data["snapshot"].as_str().unwrap().to_string();
    assert!(data["expires_at_ms"].as_u64().unwrap() > 0);

    // The same page again: a new snapshot with identical hashes.
    let (request, second) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.collect",
            collect_params("site/index.html"),
        )
        .await;
    sample("collect-same-page-again", &request, &second);
    assert_eq!(file_hashes(&first), file_hashes(&second));
    let second_snapshot = second["data"]["snapshot"].as_str().unwrap().to_string();
    assert_ne!(snapshot, second_snapshot);

    // page.read and its refusals.
    let (request, bytes, shown) = site
        .read(
            stream,
            json!({"snapshot": snapshot, "path": "img/logo mark.png", "offset": 0, "length": 1_048_576}),
        )
        .await;
    sample("read-bytes", &request, &shown);
    assert_eq!(bytes.unwrap(), b"\x89PNG logo");
    let (request, bytes, shown) = site
        .read(
            stream,
            json!({"snapshot": snapshot, "path": "css/theme.css", "offset": 5, "length": 5}),
        )
        .await;
    sample("read-range", &request, &shown);
    assert_eq!(bytes.unwrap(), b"color");
    for (name, params, expected) in [
        (
            "read-refused-not-in-snapshot",
            json!({"snapshot": snapshot, "path": "cache.db", "offset": 0, "length": 16}),
            "not_in_snapshot",
        ),
        (
            "read-refused-bad-range",
            json!({"snapshot": snapshot, "path": "app.js", "offset": 0, "length": 1_048_577}),
            "bad_range",
        ),
        (
            "read-refused-snapshot-expired",
            json!({"snapshot": "00000000000000000000000000000000", "path": "app.js", "offset": 0, "length": 16}),
            "snapshot_expired",
        ),
    ] {
        let (request, bytes, shown) = site.read(stream, params).await;
        sample(name, &request, &shown);
        assert_eq!(code(&bytes.unwrap_err()), expected);
    }

    // Release is idempotent.
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.release",
            json!({"snapshot": snapshot}),
        )
        .await;
    sample("release", &request, &reply);
    assert_eq!(
        reply["data"],
        json!({"snapshot": snapshot, "released": true})
    );
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.release",
            json!({"snapshot": snapshot}),
        )
        .await;
    sample("release-again", &request, &reply);
    assert_eq!(
        reply["data"],
        json!({"snapshot": snapshot, "released": false})
    );
    let (_, bytes, _) = site
        .read(
            stream,
            json!({"snapshot": snapshot, "path": "app.js", "offset": 0, "length": 1}),
        )
        .await;
    assert_eq!(code(&bytes.unwrap_err()), "snapshot_expired");

    // Refusals about the page itself.
    let mut too_small = collect_params("site/big.html");
    too_small["limits"]["entry_max_bytes"] = json!(32);
    for (name, params, expected) in [
        (
            "collect-refused-entry-unreadable-missing",
            collect_params("site/absent.html"),
            "entry_unreadable",
        ),
        (
            "collect-refused-entry-unreadable-denied",
            collect_params("../outside.html"),
            "entry_unreadable",
        ),
        (
            "collect-refused-entry-too-large",
            too_small,
            "entry_too_large",
        ),
        (
            "collect-refused-entry-not-html",
            collect_params("site/notes.txt"),
            "entry_not_html",
        ),
        (
            "collect-refused-session-not-bound",
            json!({"harness": HARNESS, "session": "no-such-session", "entry": "site/index.html"}),
            "session_not_bound",
        ),
    ] {
        let (request, reply) = site
            .call(stream, CORE_CHANNEL, "page.collect", params)
            .await;
        sample(name, &request, &reply);
        assert_eq!(code(&reply), expected, "{name}");
    }

    // Two live routes of one session that disagree on trust.
    bind_tool_route(stream, 2, &project, "split-session", Principal::Direct).await;
    bind_tool_route(stream, 3, &project, "split-session", Principal::Unverified).await;
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.collect",
            json!({"harness": HARNESS, "session": "split-session", "entry": "site/index.html"}),
        )
        .await;
    sample("collect-refused-session-route-ambiguous", &request, &reply);
    assert_eq!(code(&reply), "session_route_ambiguous");

    // A caller other than core.
    let (request, reply) = site
        .call(
            stream,
            OTHER_CHANNEL,
            "page.collect",
            collect_params("site/index.html"),
        )
        .await;
    sample("collect-refused-not-core", &request, &reply);
    assert_eq!(code(&reply), "page_caller_refused");

    // At most four live snapshots: the second collection above is live.
    let mut live = vec![second_snapshot];
    for _ in 0..3 {
        let (_, reply) = site
            .call(
                stream,
                CORE_CHANNEL,
                "page.collect",
                collect_params("site/index.html"),
            )
            .await;
        live.push(reply["data"]["snapshot"].as_str().unwrap().to_string());
    }
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.collect",
            collect_params("site/index.html"),
        )
        .await;
    sample("collect-refused-busy", &request, &reply);
    assert_eq!(code(&reply), "busy");
    assert!(reply["data"]["detail"]["retry_after_ms"].as_u64().unwrap() > 0);
    for snapshot in live {
        let (_, reply) = site
            .call(
                stream,
                CORE_CHANNEL,
                "page.release",
                json!({"snapshot": snapshot}),
            )
            .await;
        assert_eq!(reply["data"]["released"], true);
    }

    send_connection_goodbye(stream).await;
}

#[test]
fn subc_bridge_page_collect_management_operations() {
    run_subc_bridge_production_test(
        "subc_bridge_page_collect_management_operations",
        Duration::from_secs(60),
        drive_page_collect_daemon,
        |_, _, _| {},
    );
}
