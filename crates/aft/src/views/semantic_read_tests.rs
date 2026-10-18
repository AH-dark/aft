// Kept out of the normal suite because this probe embeds every Rust source in
// the checkout with a deterministic 768-dimensional model, without a service.
#[test]
#[ignore = "repository-sized SQLite read probe; run explicitly with --ignored --nocapture"]
fn repository_semantic_fill_read_probe() {
    use crate::blob_store::v2::read_tests::{describe, ReadProbe};

    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let revision = std::env::var("AFT_SEMANTIC_READ_PROBE_REV").unwrap_or_else(|_| "HEAD".into());
    let listing = std::process::Command::new("git")
        .current_dir(&source)
        .args([
            "ls-tree",
            "-r",
            "--name-only",
            "-z",
            &revision,
            "--",
            "crates/aft/src",
        ])
        .output()
        .unwrap();
    assert!(listing.status.success());
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut files = 0;
    for path in listing
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(path).unwrap();
        if !path.ends_with(".rs") {
            continue;
        }
        let destination = root.path().join(path);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        let contents = std::process::Command::new("git")
            .current_dir(&source)
            .args(["show", &format!("{revision}:{path}")])
            .output()
            .unwrap();
        assert!(contents.status.success());
        std::fs::write(destination, contents.stdout).unwrap();
        files += 1;
    }
    assert!(files > 500, "not a repository-sized input: {files}");
    let git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .current_dir(root.path())
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Probe",
        "-c",
        "user.email=probe@example.test",
        "commit",
        "--quiet",
        "-m",
        "probe",
    ]);
    let sibling = tempfile::tempdir().unwrap();
    git(&[
        "worktree",
        "add",
        "--quiet",
        "--detach",
        sibling.path().to_str().unwrap(),
    ]);
    let plane = new_plane(storage.path(), "read-probe-768");
    let first = Checkout::open(storage.path(), "first", root.path(), &plane);
    let snapshot = first.load();
    let store = first.owner.open_store(FamilyPlane::Semantic).unwrap();
    let fill = |checkout: &Checkout, snapshot: &Snapshot| {
        checkout
            .plane
            .fill(
                &checkout.owner,
                snapshot,
                FillBudget {
                    max_files: usize::MAX,
                    ..FillBudget::default()
                },
                &mut |texts| {
                    Ok(texts
                        .iter()
                        .map(|text| {
                            vector("read-probe-768", text)
                                .into_iter()
                                .cycle()
                                .take(768)
                                .collect()
                        })
                        .collect())
                },
                &|| snapshot.clone(),
            )
            .unwrap()
    };
    let probe = ReadProbe::start(&store);
    let first_report = fill(&first, &snapshot);
    eprintln!(
        "INPUT files={files}; FIRST {first_report:?}\nSQL {:?}",
        probe.finish()
    );
    assert!(first_report.errors.is_empty() && first_report.store_errors.is_empty());
    assert_eq!(first_report.installed, first_report.queued);
    describe(&store);
    let second = Checkout::open(storage.path(), "second", sibling.path(), &plane);
    let probe = ReadProbe::start(&store);
    let second_snapshot = second.load();
    let second_report = fill(&second, &second_snapshot);
    eprintln!("SIBLING {second_report:?}\nSQL {:?}", probe.finish());
    assert_eq!(second_report.embedded_texts, 0);
    assert_eq!(second_report.stored_hits, 0);
    assert_eq!(second_report.queued, 0);
    eprintln!(
        "SIBLING arena decodes={} reads={}",
        plane.arena("family").decode_count(),
        plane.arena("family").store_reads()
    );
    plane.unbind(&first.access);
    plane.unbind(&second.access);
    drop(first);
    drop(second);
    drop(plane);
    let plane = new_plane(storage.path(), "read-probe-768");
    let reopened = Checkout::open(storage.path(), "reopened", sibling.path(), &plane);
    let probe = ReadProbe::start(&store);
    let snapshot = reopened.load();
    let report = fill(&reopened, &snapshot);
    eprintln!("REOPENED {report:?}\nSQL {:?}", probe.finish());
    assert_eq!(report.embedded_texts, 0);
    assert_eq!(report.queued, 0);
    assert_eq!(
        plane.arena("family").decode_count(),
        first_report.installed as u64
    );
}
