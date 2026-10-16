use super::*;
use crate::bash_background::persistence::{task_paths, write_task};

pub(crate) fn wait_fixture(
    explicit_timeout: Option<u64>,
) -> (tempfile::TempDir, crate::context::AppContext, Arc<BgTask>) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = crate::context::AppContext::from_app(
        crate::context::App::default_shared(),
        crate::config::Config {
            project_root: Some(dir.path().to_path_buf()),
            storage_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        },
    );
    let registry = ctx.bash_background();
    assert_eq!(
        crate::commands::bash_orchestrate::worker_wait_max_ms(&ctx),
        1_800_000
    );
    let task_id = "bash-0000000000000001";
    let session_id = "head-session";
    let paths = task_paths(dir.path(), session_id, task_id).unwrap();
    std::fs::create_dir_all(&paths.dir).unwrap();
    let mut metadata = PersistedTask::starting(
        task_id.into(),
        session_id.into(),
        "fake running command".into(),
        dir.path().to_path_buf(),
        Some(dir.path().to_path_buf()),
        Some(explicit_timeout.unwrap_or(1_800_000)),
        false,
        false,
    );
    metadata.default_hard_kill = explicit_timeout.is_none();
    metadata.started_at = unix_millis().saturating_sub(1_900);
    // Insert a terminal fixture to avoid restart renewal, then make it running.
    // Its backdated start exercises elapsed-time renewal without sleeping or
    // spawning a process; there is no watchdog to race the message detach.
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &metadata).unwrap();
    registry
        .insert_rehydrated_task(metadata, paths, false)
        .unwrap();
    let task = registry.task_for_session(task_id, session_id).unwrap();
    assert!(task.started.elapsed() >= Duration::from_millis(1_900));
    assert_eq!(task.hard_kill_renewable, explicit_timeout.is_none());
    {
        let mut state = task.state.lock().unwrap();
        state.metadata.status = BgTaskStatus::Running;
        state.metadata.finished_at = None;
        state.metadata.exit_code = None;
        state.metadata.duration_ms = None;
        write_task(&task.paths.json, &state.metadata).unwrap();
    }
    (dir, ctx, task)
}

fn message_detach_limit(worker_session: bool, explicit_timeout: Option<u64>) -> u64 {
    let (_dir, ctx, task) = wait_fixture(explicit_timeout);
    let registry = ctx.bash_background();
    let task_id = &task.task_id;
    let session_id = &task.session_id;
    let request = serde_json::from_value(serde_json::json!({
        "id": "message-detach", "command": "bash", "session_id": session_id,
        "worker_session": worker_session,
        "params": { "wait": true, "timeout": explicit_timeout }
    }))
    .unwrap();
    let outcome = crate::commands::bash_orchestrate::build_bash_outcome(
        &request,
        &ctx,
        crate::protocol::Response::success(
            "message-detach",
            serde_json::json!({"task_id": task_id, "status": "running"}),
        ),
    );
    let crate::response_finalize::DispatchOutcome::Deferred(mut pending) = outcome else {
        panic!("the fake command must remain in a blocking wait");
    };
    assert!(registry.signal_wait_mode_detach(session_id));
    let response = (pending.poll)(&ctx).expect("a user message detaches the wait");
    assert!(response.success, "detach failed: {:?}", response.data);
    assert!(
        response
            .data
            .to_string()
            .contains("Detached because a user message arrived"),
        "the message-detach path must run: {:?}",
        response.data
    );
    assert_eq!(registry.active_wait_session_count(), 0);
    registry
        .hard_kill_deadline(task_id, session_id)
        .unwrap()
        .limit_ms
}

#[test]
fn head_message_detach_preserves_default_hard_kill() {
    assert_eq!(message_detach_limit(false, None), 1_800_000);
}

#[test]
fn pending_message_detach_hands_off_both_foreground_wait_modes() {
    for wait in [true, false] {
        let (_dir, ctx, task) = wait_fixture(None);
        let request = serde_json::from_value(serde_json::json!({
            "id": "pending-message", "command": "bash", "session_id": task.session_id,
            "params": { "wait": wait }
        }))
        .unwrap();
        let admission = serde_json::from_value(serde_json::json!({
            "id": "admission", "command": "bash_wait_detach", "session_id": task.session_id,
            "pending": true
        }))
        .unwrap();
        assert!(crate::commands::bash_wait_detach::handle(&admission, &ctx).success);
        let outcome = crate::commands::bash_orchestrate::build_bash_outcome(
            &request,
            &ctx,
            crate::protocol::Response::success(
                "pending-message",
                serde_json::json!({"task_id": task.task_id, "status": "running"}),
            ),
        );
        let response = match outcome {
            crate::response_finalize::DispatchOutcome::Immediate(response) => response,
            crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
                (pending.poll)(&ctx).expect("pending input must detach immediately")
            }
        };
        assert!(response.success);
        assert!(response
            .data
            .to_string()
            .contains("Detached because a user message arrived"));
        assert_eq!(response.data["task_id"], task.task_id);
        assert_eq!(ctx.bash_background().active_wait_session_count(), 0);
    }
}

#[test]
fn pending_message_does_not_change_background_launch() {
    let (_dir, ctx, task) = wait_fixture(None);
    ctx.bash_background()
        .request_wait_mode_detach(&task.session_id);
    let request = serde_json::from_value(serde_json::json!({
        "id": "background", "command": "bash", "session_id": task.session_id,
        "params": { "background": true }
    }))
    .unwrap();
    let outcome = crate::commands::bash_orchestrate::build_bash_outcome(
        &request,
        &ctx,
        crate::protocol::Response::success(
            "background",
            serde_json::json!({"task_id": task.task_id, "status": "running"}),
        ),
    );
    let crate::response_finalize::DispatchOutcome::Immediate(response) = outcome else {
        panic!("background launch must not enter the foreground wait path");
    };
    assert!(!response
        .data
        .to_string()
        .contains("Detached because a user message arrived"));
    assert_eq!(ctx.bash_background().active_wait_session_count(), 0);
    ctx.bash_background()
        .begin_wait_mode_session(&task.session_id, "later-foreground");
    assert!(ctx
        .bash_background()
        .take_task_wait_mode_detach(&task.session_id, "later-foreground"));
}

#[test]
fn worker_message_detach_renews_default_hard_kill() {
    assert!(message_detach_limit(true, None) >= 1_801_900);
}

#[test]
fn worker_message_detach_preserves_explicit_hard_kill() {
    assert_eq!(message_detach_limit(true, Some(45_000)), 45_000);
}

#[test]
fn head_capped_wait_promotion_preserves_default_hard_kill() {
    let (_dir, ctx, task) = wait_fixture(None);
    let response = crate::commands::bash_orchestrate::promote_bash(
        &ctx,
        &task.task_id,
        &task.session_id,
        None,
        1_800_000,
        "cap-handoff",
        false,
        false,
        true,
    );
    assert!(response.success, "promotion failed: {:?}", response.data);
    assert_eq!(
        ctx.bash_background()
            .hard_kill_deadline(&task.task_id, &task.session_id)
            .map(|deadline| deadline.limit_ms),
        Some(1_800_000)
    );
}

#[test]
fn worker_capped_wait_promotion_renews_default_hard_kill() {
    let (_dir, ctx, task) = wait_fixture(None);
    let response = crate::commands::bash_orchestrate::promote_bash(
        &ctx,
        &task.task_id,
        &task.session_id,
        None,
        1_800_000,
        "cap-handoff",
        true,
        true,
        true,
    );
    assert!(response.success, "promotion failed: {:?}", response.data);
    assert!(
        ctx.bash_background()
            .hard_kill_deadline(&task.task_id, &task.session_id)
            .unwrap()
            .limit_ms
            >= 1_801_900
    );
}
