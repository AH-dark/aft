use super::*;

fn manifest() -> Manifest {
    let mut value: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v16-manifest.json"
    ))
    .unwrap();
    let additions: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v19-additions.json"
    ))
    .unwrap();
    value["manifest_version"] = additions["manifest_version"].clone();
    for tier in ["governed", "admin"] {
        value["tiers"][tier]
            .as_array_mut()
            .unwrap()
            .extend(additions["tiers"][tier].as_array().unwrap().iter().cloned());
    }
    value["api_rules"]
        .as_array_mut()
        .unwrap()
        .extend(additions["api_rules"].as_array().unwrap().iter().cloned());
    value["canonicalization"]["pr ready"] = additions["canonicalization"]["pr ready"].clone();
    serde_json::from_value(value).unwrap()
}

fn args(raw: &[&str]) -> Vec<OsString> {
    raw.iter().map(OsString::from).collect()
}

fn rung() -> RungRecord {
    RungDetermination::r3(
        1_700_000_000,
        19,
        &RungRecordProvenance {
            image_path: "/test/aft".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            repo_key: "cortexkit/aft".into(),
        },
    )
    .record
}

#[test]
fn v19_pr_ready_is_versioned_bot_speech() {
    let manifest = manifest();
    manifest.validate().unwrap();
    for platform in ["macos", "linux"] {
        for raw in [vec!["pr", "ready", "7"], vec!["pr", "ready", "7", "--undo"]] {
            assert!(matches!(
                classify(&args(&raw), &manifest, platform),
                Classification::Governed { tuple, .. } if tuple == "pr ready"
            ));
            let mut old = manifest.clone();
            old.manifest_version = 18;
            assert!(matches!(
                classify(&args(&raw), &old, platform),
                Classification::Unclassified
            ));
            let mut undeclared = manifest.clone();
            undeclared
                .tiers
                .get_mut(&Tier::Governed)
                .unwrap()
                .retain(|row| row.tuple() != "pr ready");
            assert!(matches!(
                classify(&args(&raw), &undeclared, platform),
                Classification::Unclassified
            ));
        }
    }
}

#[test]
fn v19_pr_ready_undo_reaches_the_daemon_wire() {
    let manifest = manifest();
    let canonical = &manifest.canonicalization["pr ready"];
    for undo in [false, true] {
        let mut raw = vec!["pr", "ready", "7", "--repo", "cortexkit/aft"];
        if undo {
            raw.push("--undo");
        }
        let request = canonicalize_governed(&args(&raw), "pr ready", canonical, 19).unwrap();
        let mutation = GithubReadMutation::from_governed_request(&request).unwrap();
        assert_eq!(mutation.resource_kind, GithubReadResourceKind::PullRequest);
        assert_eq!(mutation.resource_number, 7);
        let wire = governed_wire_request(&rung(), "alfonso-aft", request);
        assert_eq!(wire["verb"], "pr ready");
        assert_eq!(wire["number"], "7");
        assert_eq!(wire["repository"], "cortexkit/aft");
        assert_eq!(wire["undo"], undo);
        assert!(wire.get("action").is_none());
        assert!(wire.get("body").is_none());
    }
    for suffix in [
        vec!["--undo", "--undo"],
        vec!["--undo=false"],
        vec!["--comment", "not a ready flag"],
        vec!["--delete-branch"],
    ] {
        let mut raw = vec!["pr", "ready", "7", "--repo", "cortexkit/aft"];
        raw.extend(suffix);
        assert!(canonicalize_governed(&args(&raw), "pr ready", canonical, 19).is_err());
    }
}

struct BypassEnv(Option<OsString>);

impl BypassEnv {
    fn set(value: Option<&str>) -> Self {
        let previous = std::env::var_os("GH_SHIM_BYPASS");
        match value {
            Some(value) => std::env::set_var("GH_SHIM_BYPASS", value),
            None => std::env::remove_var("GH_SHIM_BYPASS"),
        }
        Self(previous)
    }
}

impl Drop for BypassEnv {
    fn drop(&mut self) {
        match &self.0 {
            Some(value) => std::env::set_var("GH_SHIM_BYPASS", value),
            None => std::env::remove_var("GH_SHIM_BYPASS"),
        }
    }
}

fn check_admin(raw: &[&str], tuple: &str) {
    let _lock = crate::test_env::process_env_lock();
    let _bypass = BypassEnv::set(None);
    let manifest = manifest();
    manifest.validate().unwrap();
    let args = args(raw);
    for platform in ["macos", "linux"] {
        assert!(matches!(classify(&args, &manifest, platform),
            Classification::Admin { tuple: found } if found == tuple));
        let mut old = manifest.clone();
        old.manifest_version = 18;
        assert!(matches!(
            classify(&args, &old, platform),
            Classification::Unclassified
        ));
    }
    let mut undeclared = manifest.clone();
    if tuple.starts_with("api:") {
        undeclared.api_rules.retain(|rule| api_tuple(rule) != tuple);
    } else {
        undeclared
            .tiers
            .get_mut(&Tier::Admin)
            .unwrap()
            .retain(|row| row.tuple() != tuple);
    }
    assert!(matches!(
        classify(&args, &undeclared, "linux"),
        Classification::Unclassified
    ));
    let directory = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(directory.path().to_path_buf());
    let binding = AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "alfonso-aft".into(),
    };
    let dispatch = || {
        let classification = classify(&args, &manifest, "linux");
        let run_upstream = |forwarded: &[OsString]| {
            assert_eq!(forwarded, args);
            let (records, error) = read_bypass_audit(&paths);
            assert!(error.is_none());
            let records = records.unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0]["tuple"], tuple);
            73
        };
        if tuple == "repo create" {
            let target = TargetRepository::from_invocation(&args);
            let write = unbound_write(&args, &manifest, "linux", &target, directory.path())
                .expect("a new repository has no bot binding");
            assert!(matches!(write.target, WriteTarget::NotARepository { .. }));
            dispatch_unbound_write(&args, &write, &paths, 1_700_000_000, run_upstream)
        } else {
            dispatch_r3(
                &args,
                classification,
                &manifest,
                &paths,
                &rung(),
                &binding,
                1_700_000_000,
                run_upstream,
            )
        }
    };
    assert_eq!(dispatch(), REFUSAL_EXIT_STATUS);
    assert!(!paths.bypass_audit.exists());
    let _operator = BypassEnv::set(Some("operator"));
    assert_eq!(dispatch(), 73);
}

#[test]
fn v19_repo_rename_requires_operator_bypass() {
    check_admin(
        &["repo", "rename", "new-name", "--repo", "cortexkit/aft"],
        "repo rename",
    );
}

#[test]
fn v19_repo_create_requires_operator_bypass_even_without_a_binding() {
    check_admin(
        &["repo", "create", "cortexkit/new-repo", "--private"],
        "repo create",
    );
}

#[test]
fn v19_private_reporting_put_requires_operator_bypass() {
    check_admin(
        &[
            "api",
            "-X",
            "PUT",
            "repos/cortexkit/aft/private-vulnerability-reporting",
        ],
        "api:PUT:/repos/*/*/private-vulnerability-reporting",
    );
}

#[test]
fn v19_private_reporting_delete_requires_operator_bypass() {
    check_admin(
        &[
            "api",
            "-X",
            "DELETE",
            "/repos/cortexkit/aft/private-vulnerability-reporting",
        ],
        "api:DELETE:/repos/*/*/private-vulnerability-reporting",
    );
}

#[test]
fn v19_private_reporting_put_rejects_other_paths() {
    let manifest = manifest();
    for path in [
        "repos/cortexkit/aft/private-vulnerability-reporting/extra",
        "repos/cortexkit/aft/private-vulnerability-reporting-extra",
        "repos/cortexkit/aft/other-setting",
        "repos/cortexkit/aft/extra/private-vulnerability-reporting",
        "repos/cortexkit//private-vulnerability-reporting",
    ] {
        assert!(
            matches!(
                classify(&args(&["api", "-X", "PUT", path]), &manifest, "linux"),
                Classification::Unclassified
            ),
            "{path}"
        );
    }
}

#[test]
fn v19_private_reporting_rejects_post_but_keeps_get_mechanical() {
    let manifest = manifest();
    let endpoint = "repos/cortexkit/aft/private-vulnerability-reporting";
    for method in ["POST", "PATCH"] {
        assert!(matches!(
            classify(&args(&["api", "-X", method, endpoint]), &manifest, "linux"),
            Classification::Unclassified
        ));
        let mut wrong_method = manifest.clone();
        wrong_method
            .api_rules
            .iter_mut()
            .find(|rule| rule.method == "PUT" && rule.path_glob == PRIVATE_REPORTING_PATH_GLOB)
            .unwrap()
            .method = method.into();
        assert!(matches!(
            classify(
                &args(&["api", "-X", method, endpoint]),
                &wrong_method,
                "linux"
            ),
            Classification::Unclassified
        ));
    }
    assert!(matches!(
        classify(&args(&["api", endpoint]), &manifest, "linux"),
        Classification::Mechanical
    ));
}

#[test]
fn v19_pr_ready_refuses_an_unbound_repository() {
    let _lock = crate::test_env::process_env_lock();
    let _bypass = BypassEnv::set(None);
    let manifest = manifest();
    let directory = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(directory.path().to_path_buf());
    for verb in ["ready", "close", "reopen"] {
        let args = args(&["pr", verb, "7", "--repo", "outside/unbound"]);
        let target = TargetRepository::from_invocation(&args);
        assert!(governing_binding(
            &classify(&args, &manifest, "linux"),
            &manifest,
            &target,
            directory.path()
        )
        .is_none());
        let write = unbound_write(&args, &manifest, "linux", &target, directory.path()).unwrap();
        assert_eq!(
            dispatch_unbound_write(&args, &write, &paths, 1_700_000_000, |_| panic!(
                "unbound bot speech must not reach upstream"
            )),
            REFUSAL_EXIT_STATUS
        );
    }
    assert!(!paths.bypass_audit.exists());
}

#[test]
fn v19_pr_ready_without_an_agent_session_never_falls_back_to_upstream() {
    let _lock = crate::test_env::process_env_lock();
    let manifest = manifest();
    let directory = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(directory.path().to_path_buf());
    let binding = AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "alfonso-aft".into(),
    };
    let relay = RelayContext {
        connection_file: None,
        ticket: None,
        transient_delays: [Duration::ZERO; 2],
        relay_budget: Duration::from_secs(1),
    };
    for bypass in [None, Some("operator")] {
        let _bypass = BypassEnv::set(bypass);
        let args = args(&["pr", "ready", "7", "--undo", "--repo", "cortexkit/aft"]);
        let classification = classify(&args, &manifest, "linux");
        assert_eq!(
            dispatch_r3_with_relay_at(
                &args,
                classification,
                &manifest,
                &paths,
                &rung(),
                &binding,
                1_700_000_000,
                |_| panic!("bot speech must not use operator credentials"),
                &relay,
                directory.path()
            ),
            REFUSAL_EXIT_STATUS
        );
        let request = canonicalize_governed(
            &args,
            "pr ready",
            &manifest.canonicalization["pr ready"],
            19,
        )
        .unwrap();
        assert!(matches!(
            route_governed(
                &paths,
                &rung(),
                &binding,
                request,
                1_700_000_000,
                &manifest,
                &relay
            ),
            RouteOutcome::NoAgentSession
        ));
    }
}

#[test]
fn v19_pr_ready_rejects_a_different_signed_canonicalization() {
    let manifest = manifest();
    let args = args(&["pr", "ready", "7", "--repo", "cortexkit/aft"]);
    let mut canonical = manifest.canonicalization["pr ready"].clone();
    canonical.body_fields.clear();
    assert!(canonicalize_governed(&args, "pr ready", &canonical, 19).is_err());
}
