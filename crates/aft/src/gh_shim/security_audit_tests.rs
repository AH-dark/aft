use super::*;

struct Env(&'static str, Option<OsString>);
impl Env {
    fn set(key: &'static str, value: Option<&OsStr>) -> Self {
        let old = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        Self(key, old)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => std::env::set_var(self.0, value),
            None => std::env::remove_var(self.0),
        }
    }
}

struct Fixture {
    paths: StatePaths,
    home: tempfile::TempDir,
    _env: Vec<Env>,
    _guard: crate::test_env::GhShimStateGuard,
}
impl Fixture {
    fn new(version: u64) -> Self {
        let guard = crate::test_env::gh_shim_state_guard();
        let home = tempfile::tempdir().unwrap();
        let mut env = vec![
            Env::set("HOME", Some(home.path().as_os_str())),
            Env::set("USERPROFILE", Some(home.path().as_os_str())),
            Env::set("XDG_CONFIG_HOME", Some(home.path().as_os_str())),
            Env::set("GH_REPO", Some(OsStr::new("cortexkit/aft"))),
        ];
        for key in [
            "GH_SHIM_BYPASS",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GH_CONFIG_DIR",
            "GIT_CONFIG_COUNT",
        ] {
            env.push(Env::set(key, None));
        }
        let paths = StatePaths::from_process();
        let name = if version == 19 {
            include_str!("../../tests/fixtures/gh_shim/v19-manifest.json")
        } else {
            include_str!("../../tests/fixtures/gh_shim/v20-manifest.json")
        };
        let manifest: Manifest = serde_json::from_str(name).unwrap();
        tests::write_signed_manifest(&paths, manifest, unix_seconds());
        Self {
            paths,
            home,
            _env: env,
            _guard: guard,
        }
    }
    fn regress(&self) {
        assert!(matches!(
            resolve_manifest(&self.paths, unix_seconds()),
            ManifestResolution::Active(_)
        ));
        fs::write(&self.paths.manifest, b"not a signed envelope").unwrap();
    }
    fn repo(&self) -> PathBuf {
        let root = self.home.path().join("checkout");
        let output = Command::new("git")
            .args(["init", "--quiet"])
            .arg(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let status = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/cortexkit/aft.git",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        root
    }
}
fn args(raw: &[&str]) -> Vec<OsString> {
    raw.iter().map(OsString::from).collect()
}

fn failure_flow(regressed: bool) {
    let fixture = Fixture::new(20);
    if regressed {
        fixture.regress();
    }
    for raw in [
        vec!["api", "-X", "POST", "repos/cortexkit/aft/issues"],
        vec!["api", "-f", "a=b", "repos/cortexkit/aft/issues"],
        vec!["api", "--input", "f", "repos/cortexkit/aft/issues"],
        vec!["api", "graphql"],
        vec!["issue", "create"],
        vec!["issue", "create", "--repo", "outside/unbound"],
        vec!["auth", "token"],
    ] {
        let mut lookups = 0;
        let status = run_with(
            &args(&raw),
            |_| panic!("legacy refusal must not invoke native gh: {raw:?}"),
            |_, _, _, _| {
                lookups += 1;
                Err("ticket_unknown".into())
            },
        );
        assert_eq!(status, 86, "{raw:?}");
        assert_eq!(lookups, 1, "the actual lookup-error boundary was exercised");
    }
    let raw = args(&["api", "repos/cortexkit/aft/issues", "--jq", ".[].title"]);
    let mut forwarded = Vec::new();
    let status = run_with(
        &raw,
        |args| {
            forwarded.push(args.to_vec());
            23
        },
        |_, _, _, _| Err("old daemon".into()),
    );
    assert_eq!(status, 23);
    assert_eq!(forwarded, vec![raw]);
}

#[test]
fn audit_lookup_error_resumes_all_active_v19_guards() {
    failure_flow(false);
}
#[test]
fn audit_lookup_error_resumes_regressed_v19_classification() {
    failure_flow(true);
}

#[test]
fn audit_repository_names_have_only_github_characters() {
    for repo in [
        "../user",
        "owner/..",
        "./repo",
        "?/repo",
        "owner/#",
        "%2e/repo",
        "owner/repo%2fmore",
        "ow ner/repo",
        "owner/répo",
    ] {
        assert!(canonical_repository_key(repo).is_none(), "{repo}");
        let request = read_contract::Request::Get {
            repository: repo.into(),
            args: read_contract::GetArgs {
                path: "issues".into(),
                query: BTreeMap::new(),
                accept: read_contract::Accept::Json,
            },
        };
        assert!(request.validate().is_err(), "{repo}");
    }
    assert_eq!(
        canonical_repository_key("Owner-1/Repo_2.name"),
        Some("owner-1/repo_2.name".into())
    );
}

#[test]
fn audit_daemon_resolves_verified_v20_checkout_not_ambient_repo() {
    let fixture = Fixture::new(20);
    let repo = fixture.repo();
    let _foreign = Env::set("GH_REPO", Some(OsStr::new("cortexkit/common-auth")));
    assert_eq!(
        verified_worker_read_repository(&repo).unwrap(),
        "cortexkit/aft"
    );
    fixture.regress();
    assert!(verified_worker_read_repository(&repo).is_err());
}

#[test]
fn audit_daemon_rejects_v19_manifest_before_read_binding() {
    let fixture = Fixture::new(19);
    let repo = fixture.repo();
    let error = verified_worker_read_repository(&repo).unwrap_err();
    assert!(error.contains("manifest v20"), "{error}");
}

#[test]
fn audit_v19_status_matches_legacy_bytes_even_with_a_lookup_record() {
    let fixture = Fixture::new(19);
    fs::write(
        fixture.paths.root.join("read-session-lookup.json"),
        br#"{"outcome":"unavailable","reason":"old daemon","at_unix_secs":7}"#,
    )
    .unwrap();
    let rendered = render_self_report(&fixture.paths).unwrap();
    let report: Value = serde_json::from_str(&rendered).unwrap();
    let legacy_keys = [
        "shim_version",
        "gh_routing_schema_floor",
        "unexpected_gh_route_advertiser",
        "bound_holder",
        "agent_binding",
        "last_seam_refusal",
        "cached_manifest",
        "last_rung",
        "last_probe",
        "bypass_audit",
        "bypass_audit_error",
        "executing_image",
        "executing_image_error",
        "real_gh_resolution",
        "real_gh_resolution_error",
        "manifests_retained",
        "manifests_dir",
    ];
    let mut legacy = Map::new();
    for key in legacy_keys {
        legacy.insert(key.to_string(), report[key].clone());
    }
    let expected = format!("{}\n", serde_json::to_string(&legacy).unwrap());
    assert_eq!(
        rendered, expected,
        "v19's closed field sequence and serialization must remain unchanged"
    );
    assert!(report.get("last_read_lookup").is_none());
}
