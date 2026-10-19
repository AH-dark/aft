use super::*;

fn vectors() -> Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v20-read-vectors.json"
    ))
    .unwrap()
}
fn get() -> ReadRequest {
    ReadRequest::Get {
        path: "/repos/cortexkit/aft/pulls/7/files".into(),
        accept: None,
    }
}
fn wire(request: &ReadRequest) -> WireRequest {
    executor::wire_request("cortexkit/aft", request).unwrap()
}
fn decode(value: &Value) -> Result<ReadReply, ReadError> {
    executor::decode_reply(&wire(&get()), value)
}

#[test]
fn read_draft_success_reads_result_not_mcp_presentation_or_structured_content() {
    let mut vector = vectors()["get_success"].clone();
    vector["structuredContent"] = json!({"status":404,"body":"must not be used"});
    let ReadReply::Get(reply) = decode(&vector).unwrap() else {
        panic!("wrong reply");
    };
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, json!([{"filename":"src/lib.rs"}]));
    assert_eq!(reply.next_page, Some(2));
    let missing = json!({"content":[{"type":"text","text":serde_json::to_string(&vectors()["get_success"]).unwrap()}],"isError":false});
    assert_eq!(decode(&missing).unwrap_err().status(), 86);
}

macro_rules! refusal_test {
    ($name:ident, $code:literal, $message:literal) => {
        #[test]
        fn $name() {
            let error = decode(&vectors()[$code]).unwrap_err();
            assert_eq!(error.status(), 86);
            assert!(
                error.message("aft-bot").contains($message),
                "{}",
                error.message("aft-bot")
            );
            assert!(!error.message("aft-bot").contains('\n'));
        }
    };
}
refusal_test!(
    read_draft_budget_refusal,
    "read_budget_exhausted",
    "read budget for aft-bot used up; retry in 30 min"
);
refusal_test!(
    read_draft_repository_not_visible_refusal,
    "repository_not_visible",
    "repository is not visible to the scoped bot"
);
refusal_test!(
    read_draft_endpoint_not_allowed_refusal,
    "endpoint_not_allowed",
    "field query.secret is not allowed"
);
refusal_test!(
    read_draft_response_too_large_refusal,
    "response_too_large",
    "exceeds 1 MiB; narrow the request"
);
refusal_test!(
    read_draft_agent_identity_required_refusal,
    "agent_identity_required",
    "requires a bot identity on the stamped route"
);

#[test]
fn read_draft_vendor_error_uses_gh_failure_status_and_http_status_message() {
    let error = decode(&vectors()["vendor_error"]).unwrap_err();
    assert_eq!(error.status(), 1);
    assert_eq!(error.message("ignored"), "HTTP 404: GitHub answered 404");
}

#[test]
fn read_draft_malformed_and_unknown_refusals_fail_closed_by_code() {
    for (value, code) in [
        (
            json!({"isError":true,"error":{"code":"future_code","message":"future","transmission":"not_sent"}}),
            "future_code",
        ),
        (
            json!({"isError":false,"structuredContent":{"status":200,"body":{},"next_page":null}}),
            "missing",
        ),
        (
            json!({"isError":true,"error":{"code":"endpoint_not_allowed","message":"no field","transmission":"not_sent"}}),
            "endpoint_not_allowed",
        ),
        (
            json!({"result":{"status":200,"body":{},"next_page":null}}),
            "missing",
        ),
    ] {
        let error = decode(&value).unwrap_err();
        assert_eq!(error.status(), 86);
        assert!(error.message("bot").contains(&format!("code {code}")));
    }
    let mut vector = vectors()["agent_identity_required"].clone();
    vector["error"]["message"] = json!("one\ntwo\rthree");
    let message = decode(&vector).unwrap_err().message("bot");
    assert!(!message.contains(['\n', '\r']));
}

#[test]
fn read_draft_transmission_guard_rejects_refusal_and_vendor_mismatches() {
    for code in [
        "read_budget_exhausted",
        "repository_not_visible",
        "endpoint_not_allowed",
        "response_too_large",
        "agent_identity_required",
        "vendor_error",
    ] {
        let mut vector = vectors()[code].clone();
        vector["error"]["transmission"] = json!(if code == "vendor_error" {
            "not_sent"
        } else {
            "sent"
        });
        let error = decode(&vector).unwrap_err();
        assert_eq!(error.status(), 86);
        assert!(error.message("bot").contains("unexpected transmission"));
        assert!(error.message("bot").contains(code));
    }
}

#[test]
fn read_draft_root_is_empty_and_lone_slash_never_reaches_transport() {
    let argv = [OsString::from("api"), OsString::from("/")];
    assert!(plan(&argv, "cortexkit/aft").is_err());
    let root = wire(&ReadRequest::Get {
        path: "/repos/cortexkit/aft".into(),
        accept: None,
    });
    assert_eq!(
        serde_json::to_value(root).unwrap(),
        json!({"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":"","accept":"application/vnd.github+json"}})
    );
    for path in [
        "/",
        "/repos/cortexkit/aft/",
        "/repos/other/repo/issues",
        "https://api.github.com/repos/cortexkit/aft/issues",
        "/repos/cortexkit/aft/a/../issues",
        "/repos/cortexkit/aft/a%2Fb",
    ] {
        assert!(
            executor::wire_request(
                "cortexkit/aft",
                &ReadRequest::Get {
                    path: path.into(),
                    accept: None
                }
            )
            .is_err(),
            "{path}"
        );
    }
    let mut relative = wire(&get());
    if let WireRequest::Get { args, .. } = &mut relative {
        args.path = "/".into();
    }
    assert!(relative.validate().is_err());
}

#[test]
fn read_draft_queries_are_flat_and_accept_mime_values_are_exact() {
    let request = wire(&ReadRequest::Get {
        path: "/repos/cortexkit/aft/issues?q=a%20b&state=open".into(),
        accept: None,
    });
    assert_eq!(
        serde_json::to_value(request).unwrap()["args"]["query"],
        json!({"q":"a b","state":"open"})
    );
    assert!(executor::wire_request(
        "cortexkit/aft",
        &ReadRequest::Get {
            path: "/repos/cortexkit/aft/issues?a=1&a=2".into(),
            accept: None
        }
    )
    .is_err());
    for (accept, mime) in [
        ("diff", "application/vnd.github.diff"),
        ("patch", "application/vnd.github.patch"),
        ("raw", "application/vnd.github.raw+json"),
    ] {
        let request = wire(&ReadRequest::Get {
            path: "/repos/cortexkit/aft/pulls/7".into(),
            accept: Some(accept.into()),
        });
        assert_eq!(
            serde_json::to_value(&request).unwrap()["args"]["accept"],
            mime
        );
        let result = executor::decode_reply(
            &request,
            &json!({"isError":false,"result":{"status":200,"body":"text\n","next_page":null}}),
        )
        .unwrap();
        assert_eq!(result.bytes().unwrap(), b"text\n");
    }
}

#[test]
fn read_draft_search_and_logs_use_exact_args_and_reply_types() {
    let request = wire(&ReadRequest::Search {
        action: "github.search.issues".into(),
        query: "needle is:issue".into(),
        page: 1,
        per_page: 30,
    });
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({"action":"github.search.issues","repository":"cortexkit/aft","args":{"q":"needle is:issue","page":1,"per_page":30}})
    );
    assert!(matches!(
        executor::decode_reply(&request, &vectors()["search_success"]).unwrap(),
        ReadReply::Search(SearchReply {
            total_count: 1,
            incomplete_results: false,
            ..
        })
    ));
    let request = wire(&ReadRequest::JobLog {
        job_id: 42,
        range: Some(ByteRange {
            offset: 0,
            length: 1_048_576,
        }),
    });
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({"action":"github.actions.job_log","repository":"cortexkit/aft","args":{"job_id":42,"offset":0,"length":1048576}})
    );
    assert!(matches!(
        executor::decode_reply(&request, &vectors()["log_success"]).unwrap(),
        ReadReply::JobLog(LogReply {
            offset: 0,
            length: 6,
            total_bytes: 6,
            ..
        })
    ));
    let tail = wire(&ReadRequest::JobLog {
        job_id: 42,
        range: None,
    });
    assert_eq!(
        serde_json::to_value(tail).unwrap()["args"],
        json!({"job_id":42})
    );
}

#[test]
fn read_draft_log_metadata_and_next_page_are_not_optional_or_inferred() {
    let request = wire(&ReadRequest::JobLog {
        job_id: 42,
        range: Some(ByteRange {
            offset: 0,
            length: 1_048_576,
        }),
    });
    for result in [
        json!({"body":"x","offset":0,"length":1}),
        json!({"body":"x","offset":1,"length":1,"total_bytes":2}),
        json!({"body":"x","offset":0,"length":2,"total_bytes":2}),
    ] {
        assert_eq!(
            executor::decode_reply(&request, &json!({"isError":false,"result":result}))
                .unwrap_err()
                .status(),
            86
        );
    }
    for next in [
        json!("https://other.example"),
        json!("/repos/other/repo/issues"),
        json!(0),
        json!(-1),
    ] {
        assert_eq!(
            decode(&json!({"isError":false,"result":{"status":200,"body":[],"next_page":next}}))
                .unwrap_err()
                .status(),
            86
        );
    }
    assert_eq!(
        decode(&json!({"isError":false,"result":{"status":200,"body":[]}}))
            .unwrap_err()
            .status(),
        86
    );
}

#[test]
fn read_draft_oversized_success_is_refused_and_error_content_is_never_decoded() {
    let response = json!({"isError":false,"result":{"status":200,"body":"a".repeat(1_048_577),"next_page":null}});
    assert_eq!(decode(&response).unwrap_err().status(), 86);
    let request = wire(&get());
    let mut value = vectors()["vendor_error"].clone();
    value["content"] =
        json!([{"type":"text","text":serde_json::to_string(&vectors()["get_success"]).unwrap()}]);
    assert_eq!(
        executor::decode_reply(&request, &value)
            .unwrap_err()
            .status(),
        1
    );
}

#[test]
fn read_draft_v19_stops_before_the_executor_transport() {
    let binding = AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "not-sent".into(),
    };
    let manifest: Manifest = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v19-manifest.json"
    ))
    .unwrap();
    let relay = RelayContext {
        connection_file: None,
        ticket: None,
        transient_delays: [Duration::ZERO; 2],
        relay_budget: Duration::ZERO,
    };
    let context = ExecutionContext {
        binding: &binding,
        manifest: &manifest,
        relay: &relay,
    };
    let error = execute_read_with(&context, &get(), &mut |_| {
        panic!("v19 must never send a read")
    })
    .unwrap_err();
    assert_eq!(error.status(), 86);
}

#[test]
fn read_draft_executor_crosses_real_daemon_framing_without_identity_or_retries() {
    use std::sync::Arc;
    let daemon = relay_client::tests::FakeRelayDaemon::spawn(Arc::new(|body| {
        assert_eq!(body["op"], crate::gh_shim_relay::READ_REQUEST_OPERATION);
        assert_eq!(
            body["params"],
            json!({"ticket":"opaque-command-ticket","request":{"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":"pulls/7/files","accept":"application/vnd.github+json"}}})
        );
        json!({"op":crate::gh_shim_relay::READ_REQUEST_OPERATION,"status":"ok","data":vectors()["get_success"]})
    }));
    let dir = tempfile::tempdir().unwrap();
    let connection = dir.path().join("connection.json");
    daemon.write_connection_file(&connection);
    let binding = AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "must-not-be-transmitted".into(),
    };
    let manifest: Manifest = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v20-manifest.json"
    ))
    .unwrap();
    let relay = RelayContext {
        connection_file: Some(connection),
        ticket: Some("opaque-command-ticket".into()),
        transient_delays: [Duration::ZERO; 2],
        relay_budget: Duration::from_secs(5),
    };
    let context = ExecutionContext {
        binding: &binding,
        manifest: &manifest,
        relay: &relay,
    };
    let reply = execute_read(&context, &get()).unwrap();
    assert_eq!(reply.next_page(), Some(2));
    assert_eq!(daemon.requests.lock().unwrap().len(), 1);
}

#[test]
fn audit_encoded_reserved_bytes_are_rejected_by_the_shared_api_contract() {
    for encoded in [
        "%3a", "%3A", "%3f", "%3F", "%23", "%2f", "%2F", "%2e", "%2E",
    ] {
        let request = WireRequest::Get {
            repository: "cortexkit/aft".into(),
            args: GetArgs {
                path: format!("compare/main{encoded}other...topic"),
                query: BTreeMap::new(),
                accept: Accept::Json,
            },
        };
        assert!(request.validate().is_err(), "{encoded}");
    }
    assert!(wire(&ReadRequest::Get {
        path: "/repos/cortexkit/aft/compare/main...topic".into(),
        accept: None
    })
    .validate()
    .is_ok());
}

#[test]
fn audit_search_boolean_operators_and_parentheses_cannot_escape_appended_repo() {
    for q in [
        "needle OR other",
        "needle AND other",
        "needle NOT other",
        "needle or other",
        "(needle)",
        "needle OR (other)",
        "\"OR\"",
        "\"a (label)\"",
    ] {
        let request = WireRequest::Code {
            repository: "cortexkit/aft".into(),
            args: SearchArgs {
                q: q.into(),
                page: Some(1),
                per_page: Some(30),
            },
        };
        assert!(request.validate().is_err(), "{q}");
    }
    assert!(read_contract::validate_search("needle language:Rust is:open").is_ok());
}

#[test]
fn audit_search_repo_org_user_qualifiers_have_one_shared_guard() {
    for q in [
        "needle repo:other/repo",
        "needle org:other",
        "needle user:other",
        "\"repo:other/repo\"",
        "REPO:other/repo",
        "-repo:other/repo",
    ] {
        assert!(read_contract::validate_search(q).is_err(), "{q}");
    }
    assert!(read_contract::validate_search("needle").is_ok());
}

#[test]
fn audit_search_unclosed_quotes_and_dangling_escapes_are_rejected() {
    for q in ["needle \"", "label:\"unfinished", "needle\\"] {
        assert!(read_contract::validate_search(q).is_err(), "{q}");
    }
    assert!(read_contract::validate_search("label:\"two words\" needle").is_ok());
    assert!(read_contract::validate_search(r#"label:"a\"b" needle"#).is_ok());
}

#[test]
fn audit_identity_lookup_reports_missing_connection_bad_ticket_and_old_daemon() {
    use std::sync::Arc;
    let missing = RelayContext {
        connection_file: None,
        ticket: Some("bad-ticket".into()),
        transient_delays: [Duration::ZERO; 2],
        relay_budget: Duration::from_secs(1),
    };
    assert!(relay_client::worker_session(&missing)
        .unwrap_err()
        .contains("connection"));
    for (code, message) in [
        ("ticket_unknown", "no live command ticket"),
        (
            "operation_unknown",
            "old daemon has no session context operation",
        ),
    ] {
        let daemon = relay_client::tests::FakeRelayDaemon::spawn(Arc::new(move |body| {
            assert_eq!(body["op"], crate::gh_shim_relay::SESSION_CONTEXT_OPERATION);
            assert_eq!(body["params"], json!({"ticket":"bad-ticket"}));
            json!({"op":crate::gh_shim_relay::SESSION_CONTEXT_OPERATION,"status":"error","data":{"refusal_code":code,"message":message}})
        }));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("connection.json");
        daemon.write_connection_file(&file);
        let context = RelayContext {
            connection_file: Some(file),
            ticket: Some("bad-ticket".into()),
            transient_delays: [Duration::ZERO; 2],
            relay_budget: Duration::from_secs(1),
        };
        assert!(relay_client::worker_session(&context)
            .unwrap_err()
            .contains(message));
        assert_eq!(daemon.requests.lock().unwrap().len(), 1);
    }
}
