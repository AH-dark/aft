use super::*;

struct RenameFixture {
    base: UnboundTargetFixture,
    data: PathBuf,
}

impl RenameFixture {
    fn new(paths: &[&str], workflow: &str) -> Self {
        let base = UnboundTargetFixture::new();
        let mut manifest: Value =
            serde_json::from_str(include_str!("../fixtures/gh_shim/v16-manifest.json")).unwrap();
        let additions: Value =
            serde_json::from_str(include_str!("../fixtures/gh_shim/v19-additions.json")).unwrap();
        for tier in ["governed", "admin"] {
            manifest["tiers"][tier]
                .as_array_mut()
                .unwrap()
                .extend(additions["tiers"][tier].as_array().unwrap().iter().cloned());
        }
        manifest["api_rules"]
            .as_array_mut()
            .unwrap()
            .extend(additions["api_rules"].as_array().unwrap().iter().cloned());
        manifest["canonicalization"]["pr ready"] =
            additions["canonicalization"]["pr ready"].clone();
        write_manifest_value(&base.state_home, unix_seconds(), manifest, 19);
        write_fresh_r3_cache_for_manifest(&base.state_home, unix_seconds(), 19);
        let data = base._temp.path().join("rename-data");
        fs::create_dir_all(&data).unwrap();
        fs::write(
            data.join("tree.json"),
            serde_json::to_vec(&json!({
                "truncated": false,
                "tree": paths.iter().map(|path| json!({
                    "path": path, "type": "blob", "sha": "abc123"
                })).collect::<Vec<_>>()
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(data.join("workflow.yml"), workflow).unwrap();
        let gh = base.upstream_bin.join("gh");
        test_helpers::executable_cache::write_executable(&gh, r##"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$GH_SHIM_TEST_RECORD"
# Reject unexpected shapes instead of blessing flags the real gh rejects.
case "$*" in
  'repo view --json nameWithOwner,defaultBranchRef,url') repo=cortexkit/aft ;;
  'repo view other/published --json nameWithOwner,defaultBranchRef,url') repo=other/published ;;
  'api repos/'*)
    if [ "$*" = "api repos/$GH_RENAME_REPO/git/trees/main?recursive=1 --hostname github.com" ]; then
      if [ "${GH_RENAME_API_FAIL:-}" = yes ]; then echo 'HTTP 403' >&2; exit 1; fi
      /bin/cat "$GH_RENAME_DATA/tree.json"
      exit 0
    fi
    if [ "$*" = "api repos/$GH_RENAME_REPO/git/blobs/abc123 -H Accept: application/vnd.github.raw+json --hostname github.com" ]; then
      /bin/cat "$GH_RENAME_DATA/workflow.yml"
      exit 0
    fi
    echo "unexpected api arguments: $*" >&2; exit 2 ;;
  'repo rename '*|'repo edit '*)
    test -s "$GH_RENAME_AUDIT"
    printf 'delegated\n'
    exit 73 ;;
  *) echo "unexpected gh arguments: $*" >&2; exit 2 ;;
esac
printf '{"nameWithOwner":"%s","defaultBranchRef":{"name":"main"},"url":"https://github.com/%s"}\n' "$repo" "$repo"
"##.as_bytes()).unwrap();
        Self { base, data }
    }

    fn command(&self, args: &[&str], repo: &str) -> Command {
        write_fresh_r3_cache_for_repository(&self.base.state_home, unix_seconds(), 19, repo);
        let mut command = shim_command(
            args,
            &self.base.bound_project,
            &self.base.config_home,
            &self.base.state_home,
            &self.base.home,
            &self.base.upstream_bin,
            &self.base.recorder,
        );
        command
            .env("GH_SHIM_BYPASS", "operator")
            .env("GH_RENAME_REPO", repo)
            .env("GH_RENAME_DATA", &self.data)
            .env(
                "GH_RENAME_AUDIT",
                self.base
                    .state_home
                    .join("cortexkit/aft/gh-shim/operator-bypass.jsonl"),
            );
        command
    }

    fn refused(&self, code: &str, detail: &str) {
        let output = self
            .command(&["repo", "rename", "new-name", "--yes"], "cortexkit/aft")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(86), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(code), "{stderr}");
        assert!(stderr.contains(detail), "{stderr}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("delegated"));
    }
}

#[test]
fn gh_shim_rename_reusable_workflow_is_refused() {
    RenameFixture::new(&[".github/workflows/build.yml"], "on:\n  workflow_call:\n")
        .refused("gh_shim_rename_published", ".github/workflows/build.yml");
}

#[test]
fn gh_shim_rename_root_action_is_refused() {
    RenameFixture::new(&["action.yml"], "").refused("gh_shim_rename_published", "action.yml");
}

#[test]
fn gh_shim_rename_nested_action_is_refused() {
    RenameFixture::new(&["path/action.yaml"], "")
        .refused("gh_shim_rename_published", "path/action.yaml");
}

#[test]
fn gh_shim_rename_refusal_reports_count_and_five_paths() {
    let paths = [
        "a/action.yml",
        "b/action.yml",
        "c/action.yml",
        "d/action.yml",
        "e/action.yml",
        "f/action.yml",
    ];
    let fixture = RenameFixture::new(&paths, "");
    let output = fixture
        .command(&["repo", "rename", "new-name", "--yes"], "cortexkit/aft")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(86), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("publishes 6 item(s)"), "{stderr}");
    for path in &paths[..5] {
        assert!(stderr.contains(path), "{stderr}");
    }
    assert!(!stderr.contains(paths[5]), "{stderr}");
    assert!(!fs::read_to_string(&fixture.base.recorder)
        .unwrap()
        .contains("/git/blobs/"));
}

#[test]
fn gh_shim_rename_override_still_requires_operator_bypass() {
    let fixture = RenameFixture::new(&["action.yml"], "");
    let output = fixture
        .command(&["repo", "rename", "new-name", "--yes"], "cortexkit/aft")
        .env_remove("GH_SHIM_BYPASS")
        .env("GH_SHIM_RENAME_PUBLISHED", "allow")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(86), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("gh_shim_admin_tier"));
    assert!(!fixture.base.recorder.exists());
}

#[test]
fn gh_shim_rename_ordinary_workflow_passes_through() {
    let fixture = RenameFixture::new(
        &[".github/workflows/build.yaml"],
        "on: [push, pull_request]\n# workflow_call is not a trigger\njobs: {}\n",
    );
    let output = fixture
        .command(&["repo", "rename", "new-name", "--yes"], "cortexkit/aft")
        .env("GH_HOST", "unrelated.example")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(73), "{output:?}");
    assert_eq!(
        fixture.base.audit_records()[0]["published_paths"],
        json!([])
    );
    let calls = fs::read_to_string(&fixture.base.recorder).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("/git/trees/"))
            .count(),
        1
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("/git/blobs/"))
            .count(),
        1
    );
}

#[test]
fn gh_shim_rename_truncated_tree_is_unverified() {
    let fixture = RenameFixture::new(&[], "");
    fs::write(
        fixture.data.join("tree.json"),
        r#"{"truncated":true,"tree":[]}"#,
    )
    .unwrap();
    fixture.refused("gh_shim_rename_unverified", "truncated");
}

#[test]
fn gh_shim_rename_api_failure_is_unverified() {
    let fixture = RenameFixture::new(&[], "");
    let output = fixture
        .command(&["repo", "rename", "new-name", "--yes"], "cortexkit/aft")
        .env("GH_RENAME_API_FAIL", "yes")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(86), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("gh_shim_rename_unverified"));
}

#[test]
fn gh_shim_rename_override_audits_tree_read_failure() {
    let fixture = RenameFixture::new(&[], "");
    let output = fixture
        .command(&["repo", "rename", "new-name", "--yes"], "cortexkit/aft")
        .env("GH_RENAME_API_FAIL", "yes")
        .env("GH_SHIM_RENAME_PUBLISHED", "allow")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(73), "{output:?}");
    let audit = fixture.base.audit_records();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["rename_published_override"], "allow");
    assert_eq!(audit[0]["published_paths"], Value::Null);
    let reason = audit[0]["published_paths_unverified"].as_str().unwrap();
    assert!(reason.contains("HTTP 403"), "{reason}");
}

#[test]
fn gh_shim_rename_unparseable_workflow_is_unverified() {
    RenameFixture::new(&[".github/workflows/build.yml"], "on: [\n")
        .refused("gh_shim_rename_unverified", "YAML");
}

#[test]
fn gh_shim_rename_override_is_exact_and_audited_before_passthrough() {
    let fixture = RenameFixture::new(&["action.yml"], "");
    let output = fixture
        .command(&["repo", "rename", "new-name", "--yes"], "cortexkit/aft")
        .env("GH_SHIM_RENAME_PUBLISHED", "allow")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(73), "{output:?}");
    let audit = fixture.base.audit_records();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["rename_published_override"], "allow");
    assert_eq!(audit[0]["published_paths"], json!(["action.yml"]));
    assert!(fs::read_to_string(&fixture.base.recorder)
        .unwrap()
        .lines()
        .any(|line| line.contains("/git/trees/")));
    let output = fixture
        .command(&["repo", "rename", "new-name", "--yes"], "cortexkit/aft")
        .env("GH_SHIM_RENAME_PUBLISHED", "ALLOW")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(86), "{output:?}");
}

#[test]
fn gh_shim_rename_explicit_repository_overrides_current_directory() {
    for flag in ["-R", "--repo"] {
        let fixture = RenameFixture::new(&["action.yml"], "");
        let output = fixture
            .command(
                &[
                    "repo",
                    "rename",
                    "new-name",
                    "--yes",
                    flag,
                    "other/published",
                ],
                "other/published",
            )
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(86), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("other/published publishes"));
    }
}

#[test]
fn gh_shim_rename_guard_leaves_other_admin_verbs_unchanged() {
    let fixture = RenameFixture::new(&["action.yml"], "");
    let output = fixture
        .command(&["repo", "edit", "--description", "new"], "cortexkit/aft")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(73), "{output:?}");
    assert_eq!(
        fs::read_to_string(&fixture.base.recorder).unwrap(),
        "repo edit --description new\n"
    );
    assert!(fixture.base.audit_records()[0]
        .get("rename_published_override")
        .is_none());
}
