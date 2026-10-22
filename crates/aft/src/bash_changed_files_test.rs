use super::*;
use notify::event::{CreateKind, ModifyKind, RemoveKind};
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap()
        .success());
}
fn repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    git(
        root.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(root.path(), &["config", "user.name", "Test"]);
    std::fs::write(root.path().join("clean.txt"), "old\n").unwrap();
    std::fs::write(root.path().join("deleted.txt"), "gone\n").unwrap();
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "initial"]);
    root
}
fn event(journal: &Journal, root: &Path, name: &str, kind: notify::EventKind) {
    journal.event(&notify::Event::new(kind).add_path(root.join(name)));
}
fn capture(root: &Path, mode: BashChangedFiles) -> (Arc<Journal>, Arc<Capture>) {
    let journal = Arc::new(Journal::default());
    let capture = Capture::begin(root.to_path_buf(), journal.clone(), mode, true);
    (journal, capture)
}
#[test]
fn bash_changed_files_list_and_diff_in_real_git_repo() {
    let root = repo();
    for mode in [BashChangedFiles::List, BashChangedFiles::Diff] {
        let (journal, capture) = capture(root.path(), mode);
        std::fs::write(root.path().join("clean.txt"), "new\n").unwrap();
        event(
            &journal,
            root.path(),
            "clean.txt",
            notify::EventKind::Modify(ModifyKind::Any),
        );
        let report = capture.finish();
        assert_eq!(report.files[0].path, "clean.txt");
        assert_eq!(report.files[0].status, "modified");
        assert!(report.text.starts_with(HEADER));
        if mode == BashChangedFiles::Diff {
            let diff = report.files[0].diff.as_deref().unwrap();
            assert!(diff.contains("-old\n+new"), "{diff}");
        } else {
            assert!(report.files[0].diff.is_none());
        }
        git(root.path(), &["checkout", "--", "clean.txt"]);
    }
}
#[test]
fn bash_changed_files_already_dirty_and_existing_untracked_have_no_diff() {
    let root = repo();
    std::fs::write(root.path().join("clean.txt"), "dirty before\n").unwrap();
    std::fs::write(root.path().join("untracked.txt"), "before\n").unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    for name in ["clean.txt", "untracked.txt"] {
        std::fs::write(root.path().join(name), "after\n").unwrap();
        event(
            &journal,
            root.path(),
            name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.finish();
    assert_eq!(report.files.len(), 2);
    assert!(report
        .files
        .iter()
        .all(|file| file.diff.is_none() && file.reason.as_deref() == Some(DIRTY)));
}
#[test]
fn bash_changed_files_deleted_new_binary_and_large() {
    let root = repo();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    std::fs::remove_file(root.path().join("deleted.txt")).unwrap();
    event(
        &journal,
        root.path(),
        "deleted.txt",
        notify::EventKind::Remove(RemoveKind::File),
    );
    for (name, bytes) in [
        ("new.txt", b"added\n".to_vec()),
        ("binary", vec![0, 1, 255]),
        ("large", vec![b'x'; MAX_FILE_BYTES + 1]),
    ] {
        std::fs::write(root.path().join(name), bytes).unwrap();
        event(
            &journal,
            root.path(),
            name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.finish();
    let find = |name| report.files.iter().find(|file| file.path == name).unwrap();
    assert!(find("deleted.txt").diff.as_ref().unwrap().contains("-gone"));
    assert_eq!(find("deleted.txt").status, "deleted");
    assert!(find("deleted.txt")
        .diff
        .as_ref()
        .unwrap()
        .contains("+++ /dev/null"));
    assert!(find("new.txt").diff.as_ref().unwrap().contains("+added"));
    assert_eq!(find("new.txt").status, "added");
    assert!(find("new.txt")
        .diff
        .as_ref()
        .unwrap()
        .contains("--- /dev/null"));
    assert_eq!(
        find("binary").reason.as_deref(),
        Some("no diff: binary file")
    );
    assert!(find("large").reason.as_ref().unwrap().contains("too large"));
}
#[test]
fn bash_changed_files_caps_files_lines_bytes_and_overflow() {
    let root = repo();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    for i in 0..MAX_FILES + 4 {
        let name = format!("new-{i:03}.txt");
        std::fs::write(root.path().join(&name), "x\n".repeat(500)).unwrap();
        event(
            &journal,
            root.path(),
            &name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    journal.gap();
    let report = capture.finish();
    assert!(report.files.len() <= MAX_FILES);
    assert!(report.text.len() <= MAX_REPORT_BYTES);
    assert!(serde_json::to_vec(report).unwrap().len() <= MAX_REPORT_BYTES);
    assert!(report.text.contains("shown 200 of"));
    assert!(report.text.contains(OVERFLOW));
    assert_eq!(report.changed_files_list_envelope.reason, Some(Reason::Cap));
    assert!(report.changed_files_list_envelope.total.is_at_least());
    assert!(report
        .files
        .iter()
        .all(|file| file.diff_lines_list_envelope.as_ref().unwrap().shown == MAX_DIFF_LINES));
}
#[test]
fn bash_changed_files_ring_overrun_and_outside_git_are_honest() {
    let root = tempfile::tempdir().unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::Diff);
    for _ in 0..MAX_EVENTS + 2 {
        event(
            &journal,
            root.path(),
            "note.txt",
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.finish();
    assert!(report.text.contains(OVERFLOW));
    assert!(report.text.contains(OUTSIDE_GIT));
    assert!(report.files[0].diff.is_none());
    assert_eq!(capture.finish().text, report.text);
}
#[test]
fn bash_changed_files_off_does_no_probes_or_watcher_registration() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.path().to_path_buf());
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    assert_eq!(ctx.config().bash.changed_files, BashChangedFiles::Off);
    assert!(Capture::start(&ctx).is_none());
    assert!(ctx.watcher_counters().bash_changes.get().is_none());
}
#[test]
fn bash_changed_files_50k_fixture_start_latency() {
    let root = repo();
    let many = root.path().join("many");
    std::fs::create_dir(&many).unwrap();
    for i in 0..50_000 {
        std::fs::write(many.join(format!("file-{i}")), "fixture\n").unwrap();
    }
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "50k files"]);
    let start = Instant::now();
    let (_, list) = capture(root.path(), BashChangedFiles::List);
    let list_elapsed = start.elapsed();
    let start = Instant::now();
    let (_, diff) = capture(root.path(), BashChangedFiles::Diff);
    let diff_elapsed = start.elapsed();
    eprintln!("bash changed-files 50k fixture: list={list_elapsed:?}, diff={diff_elapsed:?}, git_available={}", diff.git.is_some());
    assert!(list_elapsed < Duration::from_millis(100));
    assert!(diff_elapsed < Duration::from_secs(1));
    assert!(list.git.is_none());
}

#[test]
fn bash_changed_files_sealed_window_excludes_later_events_and_overflow() {
    let root = tempfile::tempdir().unwrap();
    let (journal, capture) = capture(root.path(), BashChangedFiles::List);
    event(
        &journal,
        root.path(),
        "during.txt",
        notify::EventKind::Create(CreateKind::File),
    );
    capture.seal();
    event(
        &journal,
        root.path(),
        "after.txt",
        notify::EventKind::Create(CreateKind::File),
    );
    journal.gap();
    let report = capture.finish();
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.files[0].path, "during.txt");
    assert!(!report.text.contains(OVERFLOW));
}
