use super::*;
use crate::log_redact::SECRET_PLACEHOLDER;

#[test]
fn bash_background_restart_hex_watch_redaction_is_bounded_and_unlocked() {
    const TICKET: &str = "d34db33f89ab01cd7654ef2098ac1357";
    let fixture = tempfile::tempdir().unwrap();
    let original = BgTaskRegistry::default();
    let (id, task) = tests::insert_terminal_piped_task(&original, &fixture, "cmd", "", "", false);
    let mut bytes = vec![b'a'; 100 * 1024 * 1024 - 1];
    let end = bytes.len();
    bytes[end - 38..end - 6].copy_from_slice(TICKET.as_bytes());
    bytes[end - 6..].copy_from_slice(b" READY");
    std::fs::write(&task.paths.stdout, &bytes).unwrap();
    let mut metadata = task.lock_state().unwrap().metadata.clone();
    metadata.completion_has_env_secrets = Some(true);
    metadata.completion_has_non_ticket_env_secrets = Some(false);
    metadata.notify_on_completion = false;
    metadata.completion_delivered = true;
    metadata.completion_ticket_digest = Some(blake3::hash(TICKET.as_bytes()).to_hex().to_string());
    let registry = BgTaskRegistry::default();
    registry
        .insert_rehydrated_task(metadata, task.paths.clone(), true)
        .unwrap();
    let recovered = registry.task(&id).unwrap();
    {
        let mut watches = registry.inner.watch_registry.lock().unwrap();
        watches
            .register(id.clone(), WatchPattern::Substring("READY".into()), true)
            .unwrap();
        watches.set_file_cursor(&format!("{id}:stdout"), (end - 1024) as u64);
    }
    let probe_registry = registry.clone();
    let hashes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe_hashes = hashes.clone();
    crate::log_redact::TICKET_HASH_PROBE.with(|probe| {
        *probe.borrow_mut() = Some(Box::new(move || {
            assert!(
                probe_registry.inner.watch_registry.try_lock().is_ok(),
                "ticket redaction holds watch_registry"
            );
            assert!(
                probe_hashes.fetch_add(1, Ordering::SeqCst) < 4096,
                "ticket hash budget exceeded"
            );
        }));
    });
    let (_, unrelated) =
        tests::insert_terminal_piped_task(&registry, &fixture, "true", "", "", false);
    {
        let mut state = unrelated.lock_state().unwrap();
        state.metadata.status = BgTaskStatus::Running;
        state.metadata.notify_on_completion = false;
        state.child_exit_observed = true;
    }
    std::fs::write(&unrelated.paths.exit, "0\n").unwrap();
    {
        let mut tasks = registry.inner.watchdog_tasks.lock().unwrap();
        tasks.insert(id.clone(), recovered.clone());
        tasks.insert(unrelated.task_id.clone(), unrelated.clone());
    }
    let started = Instant::now();
    super::super::watchdog::run_pass(&registry, WatchdogPassCause::Tick, &mut HashMap::new());
    crate::log_redact::TICKET_HASH_PROBE.with(|probe| *probe.borrow_mut() = None);
    assert!(hashes.load(Ordering::SeqCst) > 0);
    assert_eq!(registry.active_watch_count(&id), 0);
    assert_eq!(unrelated.snapshot(0).info.status, BgTaskStatus::Completed);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "unrelated exit stalled behind watch rendering"
    );
}

#[test]
fn bash_background_watch_ticket_straddling_context_edge_is_masked() {
    const TICKET: &str = "d34db33f89ab01cd7654ef2098ac1357";
    const MD5: &str = "fedcba9876543210fedcba9876543210";
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (id, task) = tests::insert_terminal_piped_task(&registry, &fixture, "cmd", "", "", false);
    task.lock_state().unwrap().metadata.completion_ticket_digest =
        Some(blake3::hash(TICKET.as_bytes()).to_hex().to_string());
    for offset in [1, 16, 31] {
        let text = format!(
            "{TICKET}{}READY {MD5} {}{TICKET}",
            "x".repeat(100 - offset),
            "x".repeat(500 - MD5.len() - 2 - offset)
        );
        let start = text.find("READY").unwrap();
        let (matched, context) =
            registry.render_watch_preview(&id, text.as_bytes(), start..start + 5);
        assert_eq!(matched, "READY");
        assert!(context.contains(MD5));
        assert!(
            !context.contains(&TICKET[..16]),
            "ticket prefix survived: {context}"
        );
        assert!(
            !context.contains(&TICKET[16..]),
            "ticket suffix survived: {context}"
        );
    }
}

#[test]
fn bash_background_ticket_budget_withholds_previews_and_keeps_pointers() {
    const TICKET: &str = "d34db33f89ab01cd7654ef2098ac1357";
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let output = format!("{} {TICKET}\n", "a".repeat(9000));
    let (id, task) =
        tests::insert_terminal_piped_task(&registry, &fixture, "cmd", &output, "", false);
    task.lock_state().unwrap().metadata.completion_ticket_digest =
        Some(blake3::hash(TICKET.as_bytes()).to_hex().to_string());
    let (matched, context) = registry.render_watch_preview(&id, output.as_bytes(), 0..output.len());
    assert_eq!(matched, SECRET_PLACEHOLDER);
    assert!(context.starts_with(completion_preview::TRUNCATED_PREVIEW_WITHHELD));
    let completion = registry.completion_snapshot_for_task(&task).unwrap();
    assert!(completion.output_truncated);
    assert!(completion
        .output_preview
        .starts_with(completion_preview::TRUNCATED_PREVIEW_WITHHELD));
    assert!(completion
        .output_preview
        .contains(&task.paths.stdout.display().to_string()));
    assert!(!completion.output_preview.contains(TICKET));
    let mut assembled = output;
    registry.mask_completion_text(&id, &mut assembled);
    assert!(assembled.starts_with(completion_preview::TRUNCATED_PREVIEW_WITHHELD));
    assert!(assembled.contains(&task.paths.stdout.display().to_string()));
    assert!(!assembled.contains(TICKET));
}

#[test]
fn bash_background_completion_ticket_tail_uses_intact_halo() {
    const TICKET: &str = "d34db33f89ab01cd7654ef2098ac1357";
    const MD5: &str = "fedcba9876543210fedcba9876543210";
    for compressed in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let raw = format!(
            "{}\n{TICKET} {}\nmd5 {MD5}\n",
            "x\n".repeat(40_000),
            "y".repeat(540)
        );
        let (_, task) =
            tests::insert_terminal_piped_task(&registry, &fixture, "cmd", &raw, "", compressed);
        task.lock_state().unwrap().metadata.completion_ticket_digest =
            Some(blake3::hash(TICKET.as_bytes()).to_hex().to_string());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe_count = count.clone();
        crate::log_redact::TICKET_HASH_PROBE.with(|probe| {
            *probe.borrow_mut() = Some(Box::new(move || {
                assert!(
                    probe_count.fetch_add(1, Ordering::SeqCst) < 8192,
                    "completion hashed unrendered capture"
                );
            }))
        });
        let preview = registry
            .completion_snapshot_for_task(&task)
            .unwrap()
            .output_preview;
        crate::log_redact::TICKET_HASH_PROBE.with(|probe| *probe.borrow_mut() = None);
        assert!(
            !preview.contains("withheld"),
            "bounded tail was withheld (compressed={compressed}): {preview}"
        );
        assert!(preview.contains(MD5));
        assert!(!preview.contains(&TICKET[..16]));
        assert!(!preview.contains(&TICKET[16..]));
        assert!(count.load(Ordering::SeqCst) > 0);
    }
}

#[test]
fn bash_background_ticket_budget_preserves_long_masked_classifier_line() {
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let raw = format!(
        "{}{{\"api_key\":\"{}\"}}\n",
        "x".repeat(30_000),
        "Z".repeat(40_000)
    );
    let (_, task) = tests::insert_terminal_piped_task(&registry, &fixture, "cmd", &raw, "", false);
    task.lock_state().unwrap().metadata.completion_ticket_digest = Some(
        blake3::hash(b"0123456789abcdef0123456789abcdef")
            .to_hex()
            .to_string(),
    );
    let preview = registry
        .completion_snapshot_for_task(&task)
        .unwrap()
        .output_preview;
    assert!(preview.contains(SECRET_PLACEHOLDER));
    assert!(!preview.contains(&"Z".repeat(64)));
    assert!(!preview.contains("withheld"));
}

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
    let secrets = crate::sandbox_spawn::completion_secrets(
        &plan,
        &HashMap::from([("SERVICE_TOKEN".into(), not_passed.into())]),
    );
    assert_eq!(
        secrets.values.redact(&format!("{actual} {not_passed}")),
        format!("{SECRET_PLACEHOLDER} {not_passed}")
    );
    assert!(secrets.has_non_ticket_secrets);
}

#[test]
fn bash_background_completion_ignores_a_planned_ticket_when_counting_other_secrets() {
    let plan = SpawnPlan::Host {
        shell_path: PathBuf::from("bash"),
        environment: std::collections::BTreeMap::from([(
            crate::gh_shim_ticket::GH_SHIM_TICKET_ENV.into(),
            "0123456789abcdef0123456789abcdef".into(),
        )]),
    };
    let secrets = crate::sandbox_spawn::completion_secrets(&plan, &HashMap::new());
    assert!(!secrets.has_non_ticket_secrets);
    assert!(secrets.ticket_digest.is_none());
    assert!(secrets.values.is_empty());
}

#[test]
fn bash_background_completion_replay_shows_ticket_only_output_and_masks_ticket() {
    const TICKET: &str = "0123456789abcdef0123456789abcdef";
    const MD5_SHAPED: &str = "fedcba9876543210fedcba9876543210";
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let raw_output = format!("ticket={TICKET} md5={MD5_SHAPED}\n");
    let (task_id, task) = tests::insert_terminal_piped_task(
        &registry,
        &fixture,
        "printf ticket",
        &raw_output,
        "",
        false,
    );
    {
        let mut state = task.lock_state().unwrap();
        state.metadata.completion_has_env_secrets = Some(true);
        state.metadata.completion_has_non_ticket_env_secrets = Some(false);
        state.metadata.completion_ticket_digest =
            Some(blake3::hash(TICKET.as_bytes()).to_hex().to_string());
        crate::bash_background::persistence::write_task(&task.paths.json, &state.metadata).unwrap();
    }

    let restarted = BgTaskRegistry::default();
    let metadata = crate::bash_background::persistence::read_task(&task.paths.json).unwrap();
    restarted
        .insert_rehydrated_task(metadata, task.paths.clone(), true)
        .unwrap();
    let replayed = restarted.task(&task_id).unwrap();
    let completion = restarted.completion_snapshot_for_task(&replayed).unwrap();
    assert_eq!(
        completion.output_preview,
        format!("ticket=<REDACTED_SECRET> md5={MD5_SHAPED}\n")
    );
    assert!(!completion.output_preview.contains(TICKET));
    let (match_text, context) =
        restarted.render_watch_preview(&task_id, raw_output.as_bytes(), 7..(7 + TICKET.len()));
    assert_eq!(match_text, "<REDACTED_SECRET>");
    assert!(!context.contains(TICKET));
    assert!(context.contains(MD5_SHAPED));
}

#[test]
fn bash_background_completion_digest_redacts_embedded_ticket_windows_only() {
    const TICKET: &str = "d34db33f89ab01cd7654ef2098ac1357";
    const MD5_SHAPED: &str = "fedcba9876543210fedcba9876543210";
    let digest = blake3::hash(TICKET.as_bytes()).to_hex().to_string();

    let surrounded_by_underscores = format!("prefix_{TICKET}_suffix");
    assert_eq!(
        crate::log_redact::redact_ticket_digest(&surrounded_by_underscores, Some(&digest)).unwrap(),
        "prefix_<REDACTED_SECRET>_suffix"
    );

    let longer_hex_run = format!("a{TICKET}b");
    assert_eq!(
        crate::log_redact::redact_ticket_digest(&longer_hex_run, Some(&digest)).unwrap(),
        "a<REDACTED_SECRET>b"
    );
    let mut match_range = 1..(1 + TICKET.len());
    let redacted_range = crate::log_redact::redact_ticket_digest_range(
        &longer_hex_run,
        &mut match_range,
        Some(&digest),
    )
    .unwrap();
    assert_eq!(redacted_range.get(match_range), Some("<REDACTED_SECRET>"));
    let adjacent_tickets = format!("{TICKET}{TICKET}");
    assert_eq!(
        crate::log_redact::redact_ticket_digest(&adjacent_tickets, Some(&digest)).unwrap(),
        "<REDACTED_SECRET><REDACTED_SECRET>"
    );
    assert_eq!(
        crate::log_redact::redact_ticket_digest(MD5_SHAPED, Some(&digest)).unwrap(),
        MD5_SHAPED
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
fn bash_background_completion_rebuilds_preview_after_long_env_secret_masking() {
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let secret = "S".repeat(40_000);
    let raw_output = format!("{}\n{secret}\n", "x".repeat(30_000));
    let (_, task) = tests::insert_terminal_piped_task(
        &registry,
        &fixture,
        "printf output",
        &raw_output,
        "",
        false,
    );
    {
        let mut state = task.lock_state().unwrap();
        state.metadata.completion_has_env_secrets = Some(true);
        state.metadata.completion_has_non_ticket_env_secrets = Some(true);
        crate::bash_background::persistence::write_task(&task.paths.json, &state.metadata).unwrap();
    }
    *task.completion_secrets.lock().unwrap() =
        Some(crate::log_redact::SecretValues::from_environment([(
            std::ffi::OsStr::new("DEPLOY_TOKEN"),
            std::ffi::OsStr::new(&secret),
        )]));

    let live_preview = registry
        .completion_snapshot_for_task(&task)
        .unwrap()
        .output_preview;
    let metadata = crate::bash_background::persistence::read_task(&task.paths.json).unwrap();
    let restarted = BgTaskRegistry::default();
    restarted
        .insert_rehydrated_task(metadata, task.paths.clone(), true)
        .unwrap();
    let replayed = restarted.task(&task.task_id).unwrap();
    let restarted_preview = restarted
        .completion_snapshot_for_task(&replayed)
        .unwrap()
        .output_preview;

    for (state, preview) in [("live", live_preview), ("restarted", restarted_preview)] {
        assert!(preview.contains(SECRET_PLACEHOLDER), "{state}: {preview:?}");
        assert!(!preview.contains(&"S".repeat(64)), "{state}: {preview:?}");
    }
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
    let mut metadata: PersistedTask =
        serde_json::from_str(&std::fs::read_to_string(&task.paths.json).unwrap()).unwrap();
    // Older metadata could report no secrets because its check did not include
    // AFT's ticket.
    metadata.completion_has_env_secrets = Some(false);
    metadata.completion_preview = Some(
        crate::bash_background::persistence::PersistedCompletionPreview {
            output_preview: "opaque deployment credential\\n".to_string(),
            output_truncated: false,
            bash_output_list_envelope: None,
        },
    );
    crate::bash_background::persistence::write_task(&task.paths.json, &metadata).unwrap();
    assert_eq!(metadata.completion_has_env_secrets, Some(false));
    assert_eq!(metadata.completion_has_non_ticket_env_secrets, None);
    assert_eq!(metadata.completion_ticket_digest, None);
    assert!(metadata.completion_preview.is_some());
    let replayed = BgTaskRegistry::default();
    replayed
        .insert_rehydrated_task(metadata, task.paths.clone(), true)
        .unwrap();
    let recovered = replayed.task(&task.task_id).unwrap();
    let completion = replayed.completion_snapshot_for_task(&recovered).unwrap();
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

fn assert_watch_preview_for_bytes(bytes: &[u8], expected: &str) {
    let fixture = tempfile::tempdir().unwrap();
    let (registry, _db, frames) = tests::registry_with_db_and_frames(fixture.path());
    let (id, task) = tests::insert_terminal_piped_task(&registry, &fixture, "cmd", "", "", false);
    std::fs::write(&task.paths.stdout, bytes).unwrap();
    registry.ensure_task_row(&task);
    registry
        .register_watch(id, WatchPattern::regex("READY.*").unwrap(), true)
        .unwrap();
    let pushed = tests::pattern_match_frames(&frames);
    assert_eq!(pushed.len(), 1);
    assert_eq!(pushed[0].match_text, expected);
    assert_eq!(
        pushed[0].match_offset,
        bytes
            .windows(5)
            .position(|window| window == b"READY")
            .unwrap() as u64,
    );
    assert!(pushed[0].context.contains(expected));
    assert!(registry.inner.watch_registry.lock().is_ok());
    let persisted = registry.pending_pattern_matches_for_session("session");
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].match_text, expected);
}

#[test]
fn bash_background_watch_preview_handles_split_four_byte_character() {
    let text = format!("{}READY😀", "a".repeat(65_530));
    assert_watch_preview_for_bytes(text.as_bytes(), "READY😀");
}

#[test]
fn bash_background_watch_preview_handles_random_multibyte_scan_cuts() {
    let mut seed = 0x564_596_u64;
    for character in ['é', '界', '😀'] {
        for _ in 0..24 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let cut = 1 + (seed >> 32) as usize % (character.len_utf8() - 1);
            let mut bytes = Vec::new();
            for _ in 0..seed as usize % 24 {
                bytes.extend_from_slice("é界😀".as_bytes());
                bytes.extend_from_slice(&[0xff, 0xe2, 0x82]);
            }
            bytes.resize(64 * 1024 - "READY".len() - cut, b'a');
            bytes.extend_from_slice(format!("READY{character}\nTAIL").as_bytes());
            assert_watch_preview_for_bytes(&bytes, &format!("READY{character}"));
        }
    }
}

#[cfg(unix)]
#[test]
fn bash_background_completion_redaction_preserves_pipeline_failure_warning() {
    let token = format!("ghp_{}", "a".repeat(36));
    for compressed in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let command = format!("sh -c 'printf \"%s\\n\" {token}; exit 1' | cat");
        let id = registry
            .spawn_with_shell_in_slot(
                SpawnPlan::Unsandboxed,
                &command,
                super::super::BashShell::Bash,
                PathBuf::from("/bin/bash"),
                "pipeline-redaction".into(),
                fixture.path().into(),
                HashMap::new(),
                super::super::HardKill::After(Duration::from_secs(30)),
                fixture.path().into(),
                super::super::TaskSlot::Background { max: 10 },
                true,
                compressed,
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
        let status = registry
            .status(&id, "pipeline-redaction", None, None, 8192)
            .unwrap();
        let warning = status.output_preview.lines().last().unwrap();
        assert!(warning.contains("(segment 1 of 2) exited 1"), "{warning}");
        let expected = warning.replace(&token, SECRET_PLACEHOLDER);
        let snapshot = registry.completion_snapshot_for_task(&task).unwrap();
        let queued = registry.pending_completions_for_session("pipeline-redaction");
        assert_eq!(queued.len(), 1);
        for completion in [&snapshot, &queued[0]] {
            assert_eq!(completion.exit_code, Some(0));
            assert!(!completion.output_preview.contains(&token));
            assert!(completion.output_preview.contains(SECRET_PLACEHOLDER));
            assert_eq!(completion.output_preview.lines().last().unwrap(), expected);
            assert_eq!(completion.output_preview.matches("note:").count(), 1);
        }
    }
}

#[test]
fn bash_background_completion_redaction_preserves_short_recovery_trailers() {
    let token = format!("ghp_{}", "a".repeat(36));
    for compressed in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let (_, task) = tests::insert_terminal_piped_task(
            &registry,
            &fixture,
            "cmd",
            &format!("{token}\n"),
            "",
            compressed,
        );
        let mut cache = registry.ensure_terminal_output_cache(&task).unwrap();
        let mut recovery = cache.recovery.clone().unwrap_or_else(|| RecoveryContext {
            final_lines: Vec::new(),
            dropped_by_class: BTreeMap::new(),
            had_inner_drop: false,
            offset_hint_eligible: false,
            offset_start_line: None,
            byte_truncated: false,
            disk_truncated_prefix_bytes: 0,
            output_path: cache.output_path.clone(),
            stderr_path: cache.stderr_path.clone(),
            include_stderr_path: false,
            artifact_access: cache.artifact_access.clone(),
        });
        recovery.byte_truncated = true;
        recovery.had_inner_drop = true;
        let marker = recovery_marker(&recovery).unwrap();
        cache.output_preview = append_recovery_marker(&cache.output_preview, &marker);
        cache.output_truncated = true;
        cache.recovery = Some(recovery);
        let metadata = task.state.lock().unwrap().metadata.clone();
        let (preview, truncated) =
            registry.completion_preview_for_notice(&metadata, &cache, Some(&task.paths));
        assert!(truncated);
        assert!(!preview.contains(&token));
        assert!(preview.contains(SECRET_PLACEHOLDER));
        assert_eq!(preview.matches(&marker).count(), 1, "{preview}");
    }
}

#[test]
fn bash_background_watch_preview_bounds_arbitrary_bytes_and_ranges() {
    let fixture = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (id, _) = tests::insert_terminal_piped_task(&registry, &fixture, "cmd", "", "", false);
    let bytes = b"\xff\xc3\xa9\xe7\x95\x8c\xf0\x9f\x98\x80\xe2\x82";
    for (range, expected) in [
        (2..3, "é"),
        (4..5, "界"),
        (7..9, "😀"),
        (11..12, "�"),
        (0..usize::MAX, "�é界😀�"),
        (usize::MAX..usize::MAX, ""),
    ] {
        let (matched, _) = registry.render_watch_preview(&id, bytes, range);
        assert_eq!(matched, expected);
    }
    let mut seed = 0x596_564_u64;
    for _ in 0..256 {
        let mut bytes = Vec::new();
        for _ in 0..64 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            bytes.push((seed >> 32) as u8);
        }
        let start = (seed >> 16) as usize % 96;
        let end = (seed >> 32) as usize % 96;
        let (matched, _) = registry.render_watch_preview(&id, &bytes, start..end);
        assert!(String::from_utf8_lossy(&bytes).contains(&matched));
    }
}
