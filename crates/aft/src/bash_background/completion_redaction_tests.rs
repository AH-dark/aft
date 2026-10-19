use super::*;
use crate::log_redact::SECRET_PLACEHOLDER;

fn fixture_preview(input: &str, compressed: bool, failed: bool) -> String {
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (_, task) = tests::insert_terminal_piped_task(
        &registry,
        &fixture,
        "printf output",
        input,
        "",
        compressed,
    );
    if failed {
        task.state.lock().unwrap().metadata.exit_code = Some(1);
    }
    registry
        .completion_snapshot_for_task(&task)
        .unwrap()
        .output_preview
}

#[test]
fn bash_background_completion_preserves_realistic_non_secret_output() {
    for output in [
        "running 42 tests\ntest task::sk-settings ... ok\ntest result: ok. 42 passed; 0 failed\n",
        "commit 0f78c1bcb3ab4d501d6ebc491617b097416f9138\nAuthor: Example <user@example.com>\n    update task-settings\n",
        "added 42 packages, and audited 128 packages in 2s\n12 packages are looking for funding\nfound 0 vulnerabilities\n",
        "uuid 550e8400-e29b-41d4-a716-446655440000 fixture SGVsbG8gd29ybGQhYWJjZGVmZ2g= sk-settings\n",
    ] {
        assert_eq!(fixture_preview(output, false, false), output);
    }
}

#[test]
fn bash_background_completion_uses_the_selected_child_environment() {
    let actual = "actual-child-credential";
    let not_passed = "not-passed-to-child";
    let plan = SpawnPlan::Host {
        shell_path: PathBuf::from("bash"),
        environment: std::collections::BTreeMap::from([("SERVICE_TOKEN".into(), actual.into())]),
    };
    let secrets = crate::sandbox_spawn::completion_secret_values(
        &plan,
        &HashMap::from([("SERVICE_TOKEN".into(), not_passed.into())]),
    );
    assert_eq!(
        secrets.redact(&format!("{actual} {not_passed}")),
        format!("{SECRET_PLACEHOLDER} {not_passed}")
    );
}

#[test]
fn bash_background_completion_redacts_credentials_not_status_or_artifacts() {
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let token = format!("ghp_{}", "a".repeat(36));
    let (_, task) = tests::insert_terminal_piped_task(
        &registry,
        &fixture,
        "redaction",
        &format!("{token}\n"),
        "",
        false,
    );
    let completion = registry.completion_snapshot_for_task(&task).unwrap();
    assert_eq!(
        completion.output_preview,
        format!("{}\n", crate::log_redact::SECRET_PLACEHOLDER)
    );
    assert_eq!(completion.exit_code, Some(0));
    assert_eq!(
        registry
            .status(&task.task_id, "session", None, None, 8192)
            .unwrap()
            .output_preview,
        format!("{token}\n")
    );
    assert_eq!(
        std::fs::read_to_string(&task.paths.stdout).unwrap(),
        format!("{token}\n")
    );
}

#[test]
fn bash_background_completion_masks_all_common_formats() {
    for secret in [
        "-----BEGIN PRIVATE KEY-----\nprivate body\n-----END PRIVATE KEY-----",
        "ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "gho_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "ghu_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "ghs_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "ghr_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz",
        "sk-abcdefghijklmnopqrstuvwxyz0123456789ABCDEF",
        "eyJhbGciOi.eyJzdWIiOi.c2lnbmF0dXJl",
        "AKIAABCDEFGHIJKLMNOP",
        "Authorization: Bearer opaque-auth-value",
        "x-api-key: opaque-api-key-value",
        "https://user:password@example.com/path",
        "https://example.com/path?access_token=opaque-query-value",
        "xoxb-12345-slack-token",
        "{\"password\":\"opaque-password\"}",
    ] {
        for compressed in [false, true] {
            let preview = fixture_preview(&format!("{secret}\n"), compressed, false);
            assert!(
                !preview.contains(secret),
                "unmasked {secret:?}: {preview:?}"
            );
            assert!(
                preview.contains(SECRET_PLACEHOLDER) || preview.contains("https://***@"),
                "missing placeholder: {preview:?}"
            );
        }
    }
}

#[test]
fn bash_background_completion_masks_before_every_preview_cut() {
    let token = format!("ghp_{}", "Q".repeat(36));
    for compressed in [false, true] {
        for prefix in [500, 6 * 1024 - 10, 20 * 1024 - 10] {
            let input = format!(
                "{} {token} {}\nTAIL\n",
                "x".repeat(prefix),
                "y".repeat(60 * 1024)
            );
            let preview = fixture_preview(&input, compressed, true);
            assert!(
                !preview.contains("ghp_"),
                "prefix at cut {prefix}: {preview}"
            );
            assert!(
                !preview.contains("QQQQ"),
                "suffix at cut {prefix}: {preview}"
            );
        }
        let input = format!("{} {token} {}\n", "x".repeat(30_000), "z".repeat(590));
        let preview = fixture_preview(&input, compressed, false);
        assert!(!preview.contains("QQQQ"), "tail cut: {preview}");
    }
}

#[cfg(unix)]
#[test]
fn bash_background_completion_masks_per_call_env_values() {
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let value = "opaque deployment credential";
    let id = registry
        .spawn_with_shell_in_slot(
            SpawnPlan::Unsandboxed,
            "printf '%s\\n' \"$DEPLOY_tOkEn\"",
            super::super::BashShell::Bash,
            PathBuf::from("/bin/bash"),
            "env-redaction".into(),
            fixture.path().into(),
            HashMap::from([("DEPLOY_tOkEn".into(), value.into())]),
            super::super::HardKill::After(Duration::from_secs(30)),
            fixture.path().into(),
            super::super::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(fixture.path().into()),
        )
        .unwrap();
    let task = registry.task(&id).unwrap();
    let start = Instant::now();
    while !task.is_terminal() {
        registry.poll_task(&task).unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::yield_now();
    }
    let completion = registry.completion_snapshot_for_task(&task).unwrap();
    assert_eq!(completion.output_preview.trim(), SECRET_PLACEHOLDER);
    assert_eq!(completion.exit_code, Some(0));
    let queued = registry.pending_completions_for_session("env-redaction");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].output_preview.trim(), SECRET_PLACEHOLDER);
    let status = registry
        .status(&id, "env-redaction", None, None, 8192)
        .unwrap();
    assert_eq!(status.output_preview.trim(), value);
    assert_eq!(
        std::fs::read_to_string(&task.paths.stdout).unwrap().trim(),
        value
    );
    assert!(!std::fs::read_to_string(&task.paths.json)
        .unwrap()
        .contains(value));
}

#[test]
fn bash_background_completion_replay_withholds_output_and_keeps_pointers() {
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (_, task) = tests::insert_terminal_piped_task(
        &registry,
        &fixture,
        "cmd",
        "opaque deployment credential\n",
        "",
        false,
    );
    *task.completion_secrets.lock().unwrap() = None;
    let completion = registry.completion_snapshot_for_task(&task).unwrap();
    assert!(completion
        .output_preview
        .starts_with(completion_preview::RESTART_PREVIEW_WITHHELD));
    assert!(completion
        .output_preview
        .contains(&task.paths.stdout.display().to_string()));
    assert!(!completion
        .output_preview
        .contains("opaque deployment credential"));
    assert_eq!(completion.exit_code, Some(0));
}

#[test]
fn bash_background_watch_matches_raw_token_but_masks_pushed_and_persisted_text() {
    let token = format!("ghp_{}", "a".repeat(36));
    for (needle, prefix) in [(token.as_str(), 0), ("ghp_", 64 * 1024 - 7), ("READY", 0)] {
        let fixture = tempfile::tempdir().unwrap();
        let (registry, _db, frames) = tests::registry_with_db_and_frames(fixture.path());
        let text = format!(
            "{} {token}\nREADY {} {token}\n",
            "x".repeat(prefix),
            "x".repeat(490)
        );
        let (id, task) =
            tests::insert_terminal_piped_task(&registry, &fixture, "cmd", &text, "", false);
        registry.ensure_task_row(&task);
        registry
            .register_watch(id.clone(), WatchPattern::Substring(needle.into()), true)
            .unwrap();
        let pushed = tests::pattern_match_frames(&frames);
        assert_eq!(pushed.len(), 1, "raw watch did not fire for {needle}");
        assert_eq!(pushed[0].reason, "pattern_match");
        assert_eq!(pushed[0].match_offset, text.find(needle).unwrap() as u64);
        assert!(!pushed[0].match_text.contains("ghp_"));
        assert!(!pushed[0].context.contains("ghp_"));
        assert!(!pushed[0].context.contains("aaaa"));
        assert!(pushed[0].context.contains(SECRET_PLACEHOLDER));
        let persisted = registry.pending_pattern_matches_for_session("session");
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].match_text, pushed[0].match_text);
        assert_eq!(persisted[0].context, pushed[0].context);
    }
}
