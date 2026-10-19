use super::*;

fn args(raw: &[&str]) -> Vec<OsString> {
    raw.iter().map(OsString::from).collect()
}
fn manifest() -> Manifest {
    let manifest: Manifest = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v20-manifest.json"
    ))
    .unwrap();
    manifest.validate().unwrap();
    manifest
}
fn planned(raw: &[&str]) -> Plan {
    plan(&args(raw), "cortexkit/aft").unwrap()
}
fn reply(value: Value) -> ReadReply {
    ReadReply {
        body: serde_json::to_vec(&value).unwrap(),
        total_bytes: None,
        next_path: None,
    }
}
fn run_json(raw: &[&str], responses: Vec<Value>) -> (Value, Vec<ReadRequest>, i32) {
    let plan = planned(raw);
    let mut responses = responses.into_iter();
    let mut calls = Vec::new();
    let mut output = Vec::new();
    let status = render::run(
        &plan,
        &mut |request| {
            calls.push(request.clone());
            Ok(reply(responses.next().expect("unexpected request")))
        },
        &mut output,
    )
    .unwrap();
    assert!(responses.next().is_none());
    (serde_json::from_slice(&output).unwrap(), calls, status)
}

#[test]
fn v20_worker_reads_admit_the_reviewed_verbs() {
    for command in TUPLES {
        let mut raw: Vec<_> = command.split_whitespace().collect();
        match *command {
            "api" => raw.push("repos/cortexkit/aft/issues"),
            "run view" | "pr view" | "pr diff" | "pr files" | "pr checks" | "issue view"
            | "issue comments" => raw.push("42"),
            "workflow view" => raw.push("ci.yml"),
            "search code" | "search issues" => raw.push("needle"),
            _ => {}
        }
        assert!(plan(&args(&raw), "cortexkit/aft").is_ok(), "{raw:?}");
    }
}

#[test]
fn v20_get_only_guard_refuses_implicit_and_explicit_writes() {
    for suffix in [
        vec!["-f", "title=x"],
        vec!["-F", "title=x"],
        vec!["--field=title=x"],
        vec!["--raw-field", "title=x"],
        vec!["-X", "POST"],
        vec!["-XPUT"],
        vec!["--method=PATCH"],
        vec!["-X", "DELETE"],
        vec!["-X", "HEAD"],
    ] {
        let mut raw = vec!["api"];
        raw.extend(suffix);
        raw.push("repos/cortexkit/aft/issues");
        let error = plan(&args(&raw), "cortexkit/aft").unwrap_err();
        assert!(error.contains("GET only"), "{raw:?}: {error}");
    }
    let plan = planned(&[
        "api",
        "-X",
        "GET",
        "-f",
        "title=x",
        "repos/cortexkit/aft/issues",
    ]);
    assert_eq!(
        plan.api,
        Some(ReadRequest::Get {
            path: "/repos/cortexkit/aft/issues?title=x".into(),
            accept: None
        })
    );
}

#[test]
fn v20_bound_repo_guard_refuses_other_repositories_hosts_and_graphql() {
    for endpoint in [
        "repos/cortexkit/other/issues",
        "/repos/elsewhere/aft/issues",
        "/repos/cortexkit/aft2/issues",
        "https://api.github.com/repos/cortexkit/aft/issues",
        "https://evil.example/repos/cortexkit/aft/issues",
        "//api.github.com/repos/cortexkit/aft/issues",
        "user",
        "/orgs/cortexkit/repos",
        "graphql",
        "/graphql",
        "/repos/cortexkit/aft/../other/issues",
        "/repos/cortexkit/aft/%2e%2e/other",
        "/repos/cortexkit/aft/issues#fragment",
        "/repos/cortexkit/aft/issues\\other",
    ] {
        assert!(
            plan(&args(&["api", endpoint]), "cortexkit/aft").is_err(),
            "{endpoint}"
        );
    }
    for endpoint in [
        "repos/cortexkit/aft/issues",
        "/repos/cortexkit/aft/pulls/42",
        "/repos/cortexkit/aft/actions/runs?per_page=5",
    ] {
        assert!(
            plan(&args(&["api", endpoint]), "cortexkit/aft").is_ok(),
            "{endpoint}"
        );
    }
}

#[test]
fn v20_input_and_unsafe_headers_are_always_refused() {
    for suffix in [
        vec!["--input", "payload.json"],
        vec!["--input=-"],
        vec!["-XGET", "--input=x"],
        vec!["--hostname", "github.example"],
        vec!["--hostname", "evil.example", "--hostname", "api.github.com"],
        vec!["-H", "Authorization: token fake"],
        vec!["-H", "Accept: application/vnd.github.raw"],
        vec!["-H", "Accept: diff"],
        vec!["-XGET", "-F", "file=@secret"],
    ] {
        let mut raw = vec!["api", "repos/cortexkit/aft/issues"];
        raw.extend(suffix);
        assert!(plan(&args(&raw), "cortexkit/aft").is_err(), "{raw:?}");
    }
}

#[test]
fn v20_diff_accept_is_limited_to_pulls_and_compare() {
    for path in [
        "repos/cortexkit/aft/pulls/42",
        "/repos/cortexkit/aft/compare/main...topic",
    ] {
        for format in ["diff", "patch"] {
            let header = format!("Accept: application/vnd.github.{format}");
            assert!(
                matches!(planned(&["api", path, "-H", &header]).api, Some(ReadRequest::Get { accept: Some(found), .. }) if found == format)
            );
        }
    }
    for path in [
        "repos/cortexkit/aft/pulls",
        "repos/cortexkit/aft/pulls/42/files",
        "repos/cortexkit/aft/compare",
        "repos/cortexkit/aft/issues/42",
    ] {
        assert!(plan(
            &args(&["api", path, "-H", "Accept: patch"]),
            "cortexkit/aft"
        )
        .is_err());
    }
}

#[test]
fn v20_unqualified_search_uses_named_actions_without_adding_repo_in_the_shim() {
    for (verb, action) in [
        ("code", "github.search.code"),
        ("issues", "github.search.issues"),
    ] {
        let (value, calls, _) = run_json(
            &["search", verb, "needle", "--json", "url", "--limit", "1"],
            vec![json!({"items":[{"html_url":"https://github.com/cortexkit/aft/issues/42"}]})],
        );
        assert_eq!(
            value,
            json!([{"url":"https://github.com/cortexkit/aft/issues/42"}])
        );
        let ReadRequest::Search {
            action: found,
            query,
            ..
        } = &calls[0]
        else {
            panic!("not a named search");
        };
        assert_eq!(found, action);
        assert!(!query.contains("repo:"));
    }
    for query in [
        "needle repo:elsewhere/repo",
        "repo:cortexkit/aft needle",
        "needle OR org:other",
        "needle user:other",
        "(REPO:other/repo)",
        "\"repo:other/repo\"",
        "-repo:other/repo",
    ] {
        assert!(
            plan(&args(&["search", "code", query]), "cortexkit/aft").is_err(),
            "{query}"
        );
    }
}

#[test]
fn v20_three_megabyte_job_log_pages_into_one_byte_exact_stream() {
    let mut log = Vec::with_capacity(3 * REPLY_BYTES);
    for index in 0..3 * REPLY_BYTES {
        log.push((index % 251) as u8);
    }
    let plan = planned(&["api", "repos/cortexkit/aft/actions/jobs/42/logs"]);
    let mut ranges = Vec::new();
    let mut output = Vec::new();
    let status = render::run(
        &plan,
        &mut |request| {
            let ReadRequest::JobLog {
                job_id,
                range: Some(range),
            } = request
            else {
                panic!("full logs must not request Plexus's no-range tail");
            };
            assert_eq!(*job_id, 42);
            assert_eq!(range.length, REPLY_BYTES as u64);
            ranges.push(range.clone());
            let offset = range.offset as usize;
            Ok(ReadReply {
                body: log[offset..offset + REPLY_BYTES].to_vec(),
                total_bytes: Some(log.len() as u64),
                next_path: None,
            })
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(status, 0);
    assert_eq!(
        ranges.iter().map(|range| range.offset).collect::<Vec<_>>(),
        vec![0, 1_048_576, 2_097_152]
    );
    assert_eq!(output, log);
}

#[test]
fn v20_log_failed_selects_failed_jobs_and_steps_across_page_boundaries() {
    let plan = planned(&["run", "view", "8", "--log-failed"]);
    let log = b"2026-10-19T10:00:01Z ok\n2026-10-19T10:00:03Z bad\n";
    let mut requests = Vec::new();
    let mut output = Vec::new();
    render::run(&plan, &mut |request| {
        requests.push(request.clone());
        match request {
            ReadRequest::Get { path, .. } => { assert!(path.starts_with("/repos/cortexkit/aft/actions/runs/8/jobs?")); Ok(reply(json!({"jobs":[{"id":42,"name":"build","conclusion":"failure","steps":[{"name":"setup","started_at":"2026-10-19T10:00:00Z","completed_at":"2026-10-19T10:00:02Z","conclusion":"success"},{"name":"test","started_at":"2026-10-19T10:00:02Z","completed_at":"2026-10-19T10:00:04Z","conclusion":"failure"}]},{"id":43,"conclusion":"success"}]}))) }
            ReadRequest::JobLog { job_id, range: Some(range) } => {
                assert_eq!(*job_id, 42);
                let start = range.offset as usize;
                let end = (start + 13).min(log.len());
                Ok(ReadReply { body: log[start..end].to_vec(), total_bytes: Some(log.len() as u64), next_path: None })
            }
            _ => panic!("unexpected request"),
        }
    }, &mut output).unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "build\ttest\t2026-10-19T10:00:03Z bad\n"
    );
    assert_eq!(requests.len(), 5);
}

#[test]
fn v20_log_protocol_rejects_missing_total_stalls_and_oversized_replies() {
    for response in [
        ReadReply {
            body: b"a".to_vec(),
            total_bytes: None,
            next_path: None,
        },
        ReadReply {
            body: Vec::new(),
            total_bytes: Some(3),
            next_path: None,
        },
        ReadReply {
            body: vec![0; REPLY_BYTES + 1],
            total_bytes: Some(3_000_000),
            next_path: None,
        },
        ReadReply {
            body: b"too long".to_vec(),
            total_bytes: Some(2),
            next_path: None,
        },
    ] {
        let plan = planned(&["api", "repos/cortexkit/aft/actions/jobs/42/logs"]);
        let mut response = Some(response);
        assert!(render::run(
            &plan,
            &mut |_| Ok(response.take().unwrap()),
            &mut Vec::new()
        )
        .is_err());
    }
}

#[test]
fn v20_budget_refusal_names_bot_rounds_minutes_and_is_not_retried() {
    let plan = planned(&["api", "repos/cortexkit/aft/issues"]);
    let mut calls = 0;
    let error = render::run(
        &plan,
        &mut |_| {
            calls += 1;
            Err(ReadError::Budget {
                retry_after_ms: 60_001,
            })
        },
        &mut Vec::new(),
    )
    .unwrap_err();
    assert_eq!(calls, 1);
    assert_eq!(error.status(), REFUSAL_EXIT_STATUS);
    assert_eq!(
        error.message("aft-bot"),
        "read budget for aft-bot used up; retry in 2 min"
    );
}

#[test]
fn v20_head_and_older_manifests_keep_mechanical_classification() {
    let _lock = crate::test_env::process_env_lock();
    let manifest = manifest();
    let raw = args(&["api", "repos/cortexkit/aft/issues"]);
    assert!(matches!(
        classify(&raw, &manifest, "linux"),
        Classification::Mechanical
    ));
    assert!(
        plan_for_session(&raw, &manifest, "linux", false, "cortexkit/aft")
            .unwrap()
            .is_none()
    );
    assert!(
        plan_for_session(&raw, &manifest, "linux", true, "cortexkit/aft")
            .unwrap()
            .is_some()
    );
    let mut previous = manifest.clone();
    previous.manifest_version = 19;
    assert!(
        plan_for_session(&raw, &previous, "linux", true, "cortexkit/aft")
            .unwrap()
            .is_none()
    );
    let mut undeclared = manifest.clone();
    undeclared.worker_reads.clear();
    assert!(
        plan_for_session(&raw, &undeclared, "linux", true, "cortexkit/aft")
            .unwrap()
            .is_none()
    );
    assert!(
        plan_for_session(&raw, &manifest, "windows", true, "cortexkit/aft")
            .unwrap()
            .is_none()
    );
    let relay = RelayContext {
        connection_file: None,
        ticket: None,
        transient_delays: [Duration::ZERO; 2],
        relay_budget: Duration::ZERO,
    };
    assert_eq!(relay_client::worker_session(&relay).unwrap(), None);
}

#[test]
fn v20_signed_read_class_cannot_add_unknown_rows_or_change_v19_writes() {
    let manifest = manifest();
    let previous: Manifest = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v19-manifest.json"
    ))
    .unwrap();
    for raw in [
        vec!["pr", "ready", "42"],
        vec!["issue", "comment", "42", "--body", "hello"],
        vec!["workflow", "run", "ci.yml"],
        vec!["pr", "merge", "42"],
        vec![
            "api",
            "-X",
            "PATCH",
            "repos/cortexkit/aft/issues/comments/42",
            "-f",
            "body=hello",
        ],
        vec![
            "api",
            "-X",
            "PUT",
            "repos/cortexkit/aft/private-vulnerability-reporting",
        ],
    ] {
        assert_eq!(
            format!("{:?}", classify(&args(&raw), &manifest, "linux")),
            format!("{:?}", classify(&args(&raw), &previous, "linux"))
        );
        assert!(
            !candidate(&args(&raw), &manifest, "linux"),
            "write intercepted: {raw:?}"
        );
    }
    let mut invalid = manifest.clone();
    invalid
        .worker_reads
        .push(TupleDecl::Name("pr merge".into()));
    assert!(invalid.validate().unwrap_err().contains("unreviewed"));
    invalid = manifest.clone();
    invalid.worker_reads.push(invalid.worker_reads[0].clone());
    assert!(invalid.validate().unwrap_err().contains("duplicate"));
    invalid = manifest.clone();
    invalid.manifest_version = 19;
    assert!(invalid.validate().unwrap_err().contains("v20"));
    let mut current = serde_json::to_value(&manifest).unwrap();
    let mut previous = serde_json::to_value(&previous).unwrap();
    for field in ["worker_reads", "manifest_version", "issued_at_unix_secs"] {
        current.as_object_mut().unwrap().remove(field);
        previous.as_object_mut().unwrap().remove(field);
    }
    assert_eq!(current, previous);
}

#[test]
fn v20_native_json_aliases_and_jq_render_from_constrained_gets() {
    let (value, requests, _) = run_json(
        &[
            "run",
            "list",
            "--branch",
            "main",
            "--status",
            "failure",
            "--json",
            "databaseId,displayTitle,headBranch,workflowName",
        ],
        vec![
            json!({"workflow_runs":[{"id":9,"display_title":"Build","head_branch":"main","name":"CI"}]}),
        ],
    );
    assert_eq!(
        value,
        json!([{"databaseId":9,"displayTitle":"Build","headBranch":"main","workflowName":"CI"}])
    );
    assert!(
        matches!(&requests[0], ReadRequest::Get { path, .. } if path == "/repos/cortexkit/aft/actions/runs?branch=main&status=failure&per_page=20&page=1")
    );
    let (value, requests, _) = run_json(
        &[
            "pr",
            "view",
            "42",
            "--json",
            "number,title,state,headRefName,baseRefName,comments",
        ],
        vec![
            json!({"number":42,"title":"A change","state":"closed","merged_at":"2026-10-19","head":{"ref":"topic"},"base":{"ref":"main"}}),
            json!([{"body":"hello","user":{"login":"bot"}}]),
        ],
    );
    assert_eq!(value["state"], "MERGED");
    assert_eq!(value["headRefName"], "topic");
    assert_eq!(value["comments"][0]["body"], "hello");
    assert_eq!(requests.len(), 2);
    let (value, _, _) = run_json(
        &["issue", "view", "42", "--json", "number,title,state"],
        vec![json!({"number":42,"title":"A bug","state":"open"})],
    );
    assert_eq!(value, json!({"number":42,"title":"A bug","state":"OPEN"}));
    let plan = planned(&[
        "pr",
        "view",
        "42",
        "--json",
        "number,title",
        "--jq",
        ".title",
    ]);
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |_| Ok(reply(json!({"number":42,"title":"A change"}))),
        &mut output,
    )
    .unwrap();
    assert_eq!(output, b"A change\n");
}

#[test]
fn v20_graphql_only_fields_refuse_by_name_before_execution() {
    for field in [
        "reviewDecision",
        "projectItems",
        "autoMergeRequest",
        "statusCheckRollup",
    ] {
        let error = plan(
            &args(&["pr", "view", "42", "--json", field]),
            "cortexkit/aft",
        )
        .unwrap_err();
        assert_eq!(
            error,
            format!("{field} isn't available to workers: it needs GraphQL, which v20 excludes")
        );
    }
    for raw in [
        vec!["issue", "view", "42", "--web"],
        vec!["pr", "checks", "42", "--watch"],
        vec!["run", "view", "42", "--json", "notAField"],
        vec!["pr", "view", "42", "--jq", ".title"],
        vec!["pr", "view", "https://github.com/other/repo/pull/42"],
        vec!["api", "--repo", "other/repo", "repos/cortexkit/aft/issues"],
        vec!["workflow", "view"],
    ] {
        assert!(plan(&args(&raw), "cortexkit/aft").is_err(), "{raw:?}");
    }
}

#[test]
fn v20_diff_files_comments_workflows_releases_and_repo_use_gets() {
    let plan = planned(&["pr", "diff", "42", "--patch"]);
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |request| {
            assert_eq!(
                *request,
                ReadRequest::Get {
                    path: "/repos/cortexkit/aft/pulls/42".into(),
                    accept: Some("patch".into())
                }
            );
            Ok(ReadReply {
                body: b"From abc\npatch\n".to_vec(),
                total_bytes: None,
                next_path: None,
            })
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(output, b"From abc\npatch\n");
    for raw in [
        vec!["pr", "files", "42"],
        vec!["pr", "diff", "42", "--name-only"],
    ] {
        let mut output = Vec::new();
        render::run(&planned(&raw), &mut |request| {
            assert!(matches!(request, ReadRequest::Get { path, accept: None } if path.starts_with("/repos/cortexkit/aft/pulls/42/files?")));
            Ok(reply(json!([{"filename":"src/lib.rs"},{"filename":"README.md"}])))
        }, &mut output).unwrap();
        assert_eq!(output, b"src/lib.rs\nREADME.md\n");
    }
    let mut output = Vec::new();
    render::run(
        &planned(&["issue", "comments", "42"]),
        &mut |_| Ok(reply(json!([{"body":"hello","user":{"login":"bot"}}]))),
        &mut output,
    )
    .unwrap();
    assert_eq!(output, b"bot\thello\n");
    let (value, _, _) = run_json(
        &["workflow", "list", "--json", "id,name,state"],
        vec![
            json!({"workflows":[{"id":7,"name":"CI","state":"active"},{"id":8,"state":"disabled_manually"}]}),
        ],
    );
    assert_eq!(value, json!([{"id":7,"name":"CI","state":"active"}]));
    let mut responses = vec![
        json!({"path":".github/workflows/ci.yml"}),
        json!({"content":"bmFtZTogQ0kK\n"}),
    ]
    .into_iter();
    let mut calls = Vec::new();
    let mut output = Vec::new();
    render::run(
        &planned(&["workflow", "view", "ci.yml", "--yaml", "--ref", "topic"]),
        &mut |request| {
            calls.push(request.clone());
            Ok(reply(responses.next().unwrap()))
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(output, b"name: CI\n");
    assert!(
        matches!(&calls[1], ReadRequest::Get { path, .. } if path == "/repos/cortexkit/aft/contents/.github/workflows/ci.yml?ref=topic")
    );
    let (value, _, _) = run_json(
        &[
            "release",
            "view",
            "v1.2.3",
            "--json",
            "tagName,isDraft,body",
        ],
        vec![json!({"tag_name":"v1.2.3","draft":false,"body":"notes"})],
    );
    assert_eq!(
        value,
        json!({"tagName":"v1.2.3","isDraft":false,"body":"notes"})
    );
    let (value, _, _) = run_json(
        &[
            "repo",
            "view",
            "--json",
            "nameWithOwner,description,isPrivate",
        ],
        vec![json!({"full_name":"cortexkit/aft","description":"tools","private":true})],
    );
    assert_eq!(
        value,
        json!({"nameWithOwner":"cortexkit/aft","description":"tools","isPrivate":true})
    );
}

#[test]
fn v20_checks_render_and_preserve_failure_and_pending_exit_codes() {
    for (state, expected_status, bucket) in [
        ("success", 0, "pass"),
        ("failure", 1, "fail"),
        ("pending", 8, "pending"),
    ] {
        let (value, requests, status) = run_json(
            &["pr", "checks", "42", "--json", "name,state,bucket,link"],
            vec![
                json!({"head":{"sha":"abc"}}),
                json!({"check_runs":[{"name":"CI","status":"completed","conclusion":state,"html_url":"https://github.com/check/7"}]}),
                json!({"statuses":[]}),
            ],
        );
        assert_eq!(status, expected_status);
        assert_eq!(value[0]["bucket"], bucket);
        assert_eq!(requests.len(), 3);
    }
}

#[test]
fn v20_api_pagination_is_streamed_and_links_are_bound_again() {
    let plan = planned(&["api", "repos/cortexkit/aft/issues", "--paginate", "--slurp"]);
    let mut calls = 0;
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |_| {
            calls += 1;
            Ok(ReadReply {
                body: format!("[{calls}]").into_bytes(),
                total_bytes: None,
                next_path: (calls == 1).then(|| "/repos/cortexkit/aft/issues?page=2".into()),
            })
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(output, b"[[1],[2]]\n");
    assert_eq!(calls, 2);
    for next in ["https://evil.example/issues", "/repos/other/repo/issues"] {
        let error = render::run(
            &plan,
            &mut |_| {
                Ok(ReadReply {
                    body: b"[]".to_vec(),
                    total_bytes: None,
                    next_path: Some(next.into()),
                })
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error.status(), REFUSAL_EXIT_STATUS);
    }
}

#[test]
fn v20_pending_executor_refuses_without_delegation_or_credentials() {
    let binding = AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "aft-bot".into(),
    };
    let manifest = manifest();
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
    let error = execute_read(
        &context,
        &ReadRequest::Get {
            path: "/repos/cortexkit/aft/issues".into(),
            accept: None,
        },
    )
    .unwrap_err();
    assert!(error.message("aft-bot").contains("not sent"));
    assert_eq!(error.status(), REFUSAL_EXIT_STATUS);
}

#[test]
fn v20_preparer_pins_test_only_fixture_and_preserves_every_v19_row() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixture = root.join("crates/aft/tests/fixtures/gh_shim/v19-manifest.json");
    let raw = fs::read(&fixture).unwrap();
    // This fixture digest is pinned for this test, not used as signing material.
    assert_eq!(
        format!("{:x}", Sha256::digest(&raw)),
        "12c14ca1c8779b358998b000ef6210c75147ceec1f8c318fd5e7be42799a3573"
    );
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("v20.json");
    let run = |input: &Path, issued: &str| {
        Command::new("python3")
            .arg(root.join("scripts/prepare-gh-shim-v20.py"))
            .arg(input)
            .arg(&destination)
            .args(["--issued-at-unix-secs", issued])
            .output()
            .unwrap()
    };
    let output = run(&fixture, "1792420000");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = fs::read(&destination).unwrap();
    let digest = format!("{:x}", Sha256::digest(&payload));
    assert!(String::from_utf8(output.stdout).unwrap().contains(&digest));
    assert!(fs::read_to_string(destination.with_extension("sha256"))
        .unwrap()
        .starts_with(&digest));
    let mut current: Value = serde_json::from_slice(&payload).unwrap();
    let mut previous: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        current["worker_reads"].as_array().unwrap().len(),
        TUPLES.len()
    );
    for field in ["worker_reads", "manifest_version", "issued_at_unix_secs"] {
        current.as_object_mut().unwrap().remove(field);
        previous.as_object_mut().unwrap().remove(field);
    }
    assert_eq!(current, previous);
    assert!(!run(&fixture, "1792326319").status.success());
    let bad = dir.path().join("bad.json");
    fs::write(&bad, [raw, b" ".to_vec()].concat()).unwrap();
    assert!(!run(&bad, "1792420000").status.success());
}

#[test]
fn v20_head_passthrough_is_byte_identical_when_lookup_is_unavailable() {
    let raw = args(&["api", "repos/cortexkit/aft/issues", "--jq", ".[].title"]);
    let mut diagnostics = Vec::new();
    let mut calls = 0;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let status = fallback_with(
        &raw,
        "old daemon has no session-context operation",
        |forwarded| {
            calls += 1;
            assert_eq!(forwarded, raw);
            stdout.extend_from_slice(b"first\nsecond\n");
            stderr.extend_from_slice(b"upstream warning\n");
            0
        },
        &mut diagnostics,
    );
    assert_eq!(status, 0);
    assert_eq!(calls, 1);
    assert_eq!(stdout, b"first\nsecond\n");
    assert_eq!(stderr, b"upstream warning\n");
    assert!(diagnostics.is_empty());
    let status = fallback_with(&raw, "connection refused", |_| 1, &mut diagnostics);
    assert_eq!(status, 1);
    assert_eq!(
        diagnostics,
        b"gh: governed read routing unavailable (identity lookup: connection refused)\n"
    );
}

#[test]
fn v20_status_records_the_last_lookup_outcome_and_reason() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(dir.path().to_path_buf());
    for (result, outcome, reason) in [
        (Ok(Some(true)), "worker", Value::Null),
        (Ok(Some(false)), "head", Value::Null),
        (
            Ok(None),
            "unscoped",
            json!("no authenticated scoped command session"),
        ),
        (
            Err("daemon offline".into()),
            "unavailable",
            json!("daemon offline"),
        ),
    ] {
        record_lookup(&paths, &result);
        let value = serde_json::to_value(last_lookup(&paths).unwrap()).unwrap();
        assert_eq!(value["outcome"], outcome);
        assert_eq!(value["reason"], reason);
        let status: Value = serde_json::from_str(&render_self_report(&paths).unwrap()).unwrap();
        assert_eq!(status["last_read_lookup"], value);
    }
}

#[test]
fn v20_native_modifiers_are_not_silently_ignored() {
    for raw in [
        vec!["pr", "diff", "42", "--color", "always"],
        vec!["workflow", "view", "ci.yml", "--ref", "main"],
        vec!["pr", "list", "--author", "@me"],
    ] {
        let plan = plan(&args(&raw), "cortexkit/aft");
        if raw[0] == "pr" && raw[1] == "list" {
            assert!(render::run(
                &plan.unwrap(),
                &mut |_| panic!("@me must refuse before a request"),
                &mut Vec::new()
            )
            .is_err());
        } else {
            assert!(plan.is_err(), "{raw:?}");
        }
    }
    let (value, requests, _) = run_json(
        &[
            "run",
            "list",
            "--workflow",
            "old.yml",
            "--all",
            "--json",
            "databaseId",
        ],
        vec![
            json!({"id":8,"state":"disabled_manually"}),
            json!({"workflow_runs":[{"id":9}]}),
        ],
    );
    assert_eq!(value, json!([{"databaseId":9}]));
    assert!(
        matches!(&requests[1], ReadRequest::Get { path, .. } if path.starts_with("/repos/cortexkit/aft/actions/workflows/8/runs?"))
    );
    let plan = planned(&["run", "view", "42", "--verbose"]);
    let mut responses = vec![json!({"id":42}), json!({"jobs":[{"id":8,"name":"build","steps":[{"name":"test step","conclusion":"success"}]}]})].into_iter();
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |_| Ok(reply(responses.next().unwrap())),
        &mut output,
    )
    .unwrap();
    assert!(String::from_utf8(output)
        .unwrap()
        .contains("  success test step\n"));
    let (value, _, _) = run_json(
        &["pr", "checks", "42", "--json", "event,workflow"],
        vec![
            json!({"head":{"sha":"abc"}}),
            json!({"check_runs":[{"app":{"slug":"github-actions"},"check_suite":{"id":7}}]}),
            json!({"statuses":[]}),
            json!({"workflow_runs":[{"event":"pull_request","name":"CI"}]}),
        ],
    );
    assert_eq!(value, json!([{"event":"pull_request","workflow":"CI"}]));
}

#[test]
fn v20_rest_backed_json_hydrates_repository_and_preserves_native_aliases() {
    let (value, _, _) = run_json(
        &["run", "list", "--json", "startedAt"],
        vec![json!({"workflow_runs":[{"id":9,"run_started_at":"2026-10-19T12:00:00Z"}]})],
    );
    assert_eq!(value, json!([{"startedAt":"2026-10-19T12:00:00Z"}]));
    let (value, requests, _) = run_json(
        &["search", "issues", "needle", "--json", "number,repository"],
        vec![
            json!({"items":[{"number":42,"repository_url":"https://api.github.com/repos/cortexkit/aft"}]}),
            json!({"full_name":"cortexkit/aft","name":"aft","private":true}),
        ],
    );
    assert_eq!(value[0]["repository"]["nameWithOwner"], "cortexkit/aft");
    assert_eq!(value[0]["repository"]["isPrivate"], true);
    assert!(
        matches!(&requests[1], ReadRequest::Get { path, .. } if path == "/repos/cortexkit/aft")
    );
    let (value, _, _) = run_json(
        &["release", "view", "v1", "--json", "author"],
        vec![json!({"author":{"node_id":"user-id","login":"bot","type":"Bot"}})],
    );
    assert_eq!(value["author"]["id"], "user-id");
    assert_eq!(value["author"]["is_bot"], true);
    let (value, _, _) = run_json(
        &["pr", "view", "42", "--json", "files"],
        vec![
            json!({"number":42}),
            json!([{"filename":"src/lib.rs","additions":2,"deletions":1}]),
        ],
    );
    assert_eq!(value["files"][0]["path"], "src/lib.rs");
}
