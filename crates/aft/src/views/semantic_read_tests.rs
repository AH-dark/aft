#[test]
fn resident_chunk_shortcut_matches_store_and_full_rebuild() {
    let source = large_source();
    let edited = source.replace("input + 149\n", "input * 149\n");
    let mut payloads = Vec::new();
    for resident_shortcut in [true, false] {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), &[("src/large.rs", &source)]);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let arena = plane.arena("family");
        arena.set_chunk_shortcut(resident_shortcut);
        let snapshot = edit_snapshot(&checkout, &edited);
        let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
        let probe = crate::blob_store::v2::read_tests::ReadProbe::start(&store);
        let before = crate::views::io::work_snapshot();
        let report = checkout.fill(&snapshot, &Model::default(), "model-a");
        let after = crate::views::io::work_snapshot();
        let reads = probe.finish();
        let gets: usize = reads
            .statements
            .iter()
            .filter(|(sql, _)| sql.starts_with("SELECT payload, payload_digest, payload_schema"))
            .map(|(_, (calls, _))| calls)
            .sum();
        eprintln!("LARGE_EDIT shortcut={resident_shortcut} chunks={} resident_hits={} gets={gets} payload_bytes={} page_misses={}", report.chunks, report.chunk_resident_hits, after["semantic_store_payload_bytes"].as_u64().unwrap() - before["semantic_store_payload_bytes"].as_u64().unwrap(), reads.misses);
        assert_canonical_blob(&checkout, &edited);
        assert_eq!(gets, if resident_shortcut { 4 } else { 366 });
        assert_eq!(report.source_reads, 1);
        assert_eq!(report.embedded_texts, 1);
        assert_eq!(
            report.chunk_resident_hits > 0,
            resident_shortcut,
            "{report:?}"
        );
        arena.set_chunk_shortcut(true);
        let key = plane.producer.key(edited.as_bytes(), &large_rel_path());
        payloads.push(
            checkout
                .owner
                .open_store(FamilyPlane::Semantic)
                .unwrap()
                .get(&key)
                .unwrap()
                .unwrap(),
        );
        let run = arena.get(&key).unwrap();
        let chunk = crate::semantic_index::chunk_view_file(
            root.path(),
            Path::new("src/large.rs"),
            edited.as_bytes(),
            plane.producer.caps,
        )
        .unwrap()
        .unwrap()
        .remove(0);
        let chunk_key = plane.producer.chunk_key(&chunk.embed_text);
        assert!(arena.chunk_vector(&chunk_key).is_some());
        plane.unbind(&checkout.access);
        drop(run);
        arena.trim();
        assert!(
            arena.chunk_vector(&chunk_key).is_none(),
            "the chunk index retained an evicted run"
        );
    }
    assert_eq!(
        payloads[0], payloads[1],
        "resident and store shortcuts differ"
    );
}

#[test]
fn legacy_file_vectors_never_seed_chunk_shortcuts() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let source = large_source();
    write_tree(root.path(), &[("src/large.rs", &source)]);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
    let store = checkout.owner.open_store(FamilyPlane::Semantic).unwrap();
    let chunks = crate::semantic_index::chunk_view_file(
        root.path(),
        Path::new("src/large.rs"),
        source.as_bytes(),
        plane.producer.caps,
    )
    .unwrap()
    .unwrap();
    for chunk in &chunks {
        let payload = encode_chunk_vector(
            &plane.producer.chunk_identity(),
            &chunk.embed_text,
            &vector("model-a", &chunk.embed_text),
        )
        .unwrap();
        store
            .put_or_touch(&plane.producer.chunk_key(&chunk.embed_text), &payload)
            .unwrap();
    }
    let mut legacy = crate::semantic_index::embed_view_files(
        vec![chunks],
        &mut |texts| Model::default().embed("legacy-floats", texts),
        64,
    )
    .unwrap();
    let key = plane.producer.key(source.as_bytes(), &large_rel_path());
    store
        .put_or_touch(
            &key,
            &legacy
                .remove(0)
                .encode_view_payload(&plane.producer.payload()),
        )
        .unwrap();
    checkout.load();
    let edited = source.replace("input + 149\n", "input * 149\n");
    let snapshot = edit_snapshot(&checkout, &edited);
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    assert_eq!(report.embedded_texts, 1);
    assert_canonical_blob(&checkout, &edited);
    let shifted = format!("\n{edited}");
    let snapshot = edit_snapshot(&checkout, &shifted);
    let report = checkout.fill(&snapshot, &Model::default(), "model-a");
    assert_eq!(report.embedded_texts, 0);
    assert!(
        report.chunk_resident_hits >= 300,
        "one-time validation did not warm canonical chunks: {report:?}"
    );
    assert_canonical_blob(&checkout, &shifted);
}

#[test]
fn incremental_reuse_requires_matching_path_and_content() {
    let storage = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), FILES);
    let plane = new_plane(storage.path(), "model-a");
    let checkout = ready_checkout(storage.path(), root.path(), &plane);
    let old = checkout.load();
    let resident = plane
        .resident(&checkout.access, old.generation().name())
        .unwrap();
    let state = ViewState::default();
    let unchanged = RelPath::new(b"src/beta.rs".to_vec()).unwrap();
    let DiskState::Present { content, .. } = old.disk_state(&unchanged) else {
        panic!("missing member")
    };
    assert!(matches!(
        SemanticPlane::vector_source(&state, &resident, &old, &unchanged, content),
        VectorSource::Base
    ));
    write_tree(
        root.path(),
        &[("src/alpha.rs", "pub fn alpha_total() -> u32 { 999 }\n")],
    );
    checkout.driver.register_write_intent();
    let changed = checkout.load();
    let path = RelPath::new(b"src/alpha.rs".to_vec()).unwrap();
    let DiskState::Present { content, .. } = changed.disk_state(&path) else {
        panic!("missing changed member")
    };
    assert!(
        matches!(
            SemanticPlane::vector_source(&state, &resident, &changed, &path, content),
            VectorSource::Pending
        ),
        "changed content reused an old generation's vector"
    );
    let report = checkout.fill(&changed, &Model::default(), "model-a");
    assert_eq!(
        report.source_reads, 1,
        "unchanged paths were filled again: {report:?}"
    );
    let next = checkout.load();
    let report = checkout.fill(&next, &Model::default(), "model-a");
    assert_eq!(
        (report.queued, report.source_reads, report.chunks),
        (0, 0, 0)
    );
}

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
    assert!(
        listing.status.success(),
        "{}",
        String::from_utf8_lossy(&listing.stderr)
    );
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut files = 0;
    let mut edit_paths = Vec::new();
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
        if contents.stdout.len() > 4_000 && contents.stdout.len() < 20_000 {
            edit_paths.push(path.to_owned());
        }
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
    first.load();
    if std::env::var_os("AFT_SEMANTIC_READ_PROBE_DISABLE_SHORTCUT").is_some() {
        plane.arena("family").set_chunk_shortcut(false);
    }
    describe(&store);
    let sample_path = Path::new(&edit_paths[0]);
    let sample_bytes = std::fs::read(root.path().join(sample_path)).unwrap();
    let sample_chunks = crate::semantic_index::chunk_view_file(
        root.path(),
        sample_path,
        &sample_bytes,
        plane.producer.caps,
    )
    .unwrap()
    .unwrap();
    for (index, chunk) in sample_chunks.iter().take(10).enumerate() {
        let key = plane.producer.chunk_key(&chunk.embed_text);
        let probe = ReadProbe::start(&store);
        let payload = store.get(&key).unwrap().unwrap();
        let counts = probe.finish();
        eprintln!(
            "COLD_GET sample={index} payload_bytes={} page_cache_misses={}",
            payload.len(),
            counts.misses
        );
    }
    let second = Checkout::open(storage.path(), "second", sibling.path(), &plane);
    let before_sibling = crate::views::io::work_snapshot();
    let probe = ReadProbe::start(&store);
    let second_snapshot = second.load();
    let second_report = fill(&second, &second_snapshot);
    eprintln!("SIBLING {second_report:?}\nSQL {:?}", probe.finish());
    let after_sibling = crate::views::io::work_snapshot();
    eprintln!(
        "SIBLING_COUNTERS gets={} payload_bytes={} fills={}",
        after_sibling["semantic_store_gets"].as_u64().unwrap()
            - before_sibling["semantic_store_gets"].as_u64().unwrap(),
        after_sibling["semantic_store_payload_bytes"]
            .as_u64()
            .unwrap()
            - before_sibling["semantic_store_payload_bytes"]
                .as_u64()
                .unwrap(),
        after_sibling["semantic_fills"].as_u64().unwrap()
            - before_sibling["semantic_fills"].as_u64().unwrap()
    );
    assert_eq!(second_report.embedded_texts, 0);
    assert_eq!(second_report.stored_hits, 0);
    assert_eq!(second_report.queued, 0);
    eprintln!(
        "SIBLING arena decodes={} reads={}",
        plane.arena("family").decode_count(),
        plane.arena("family").store_reads()
    );
    for (label, edits) in [("one-file", 1), ("ten-file", 10), ("no-change", 0)] {
        for index in 0..edits {
            let path = sibling.path().join(&edit_paths[index]);
            let mut text = std::fs::read_to_string(&path).unwrap();
            text.push_str(&format!(
                "\npub fn probe_{index}() -> u32 {{ {} }}\n",
                edits + index
            ));
            std::fs::write(path, text).unwrap();
        }
        let before = crate::views::io::work_snapshot();
        second.driver.register_write_intent();
        let snapshot = second.load();
        let report = fill(&second, &snapshot);
        let after = crate::views::io::work_snapshot();
        let delta = [
            "semantic_store_gets",
            "semantic_store_payload_bytes",
            "semantic_touch_rows_written",
            "semantic_touch_transactions",
            "semantic_files_filled",
        ]
        .map(|key| {
            (
                key,
                after[key].as_u64().unwrap() - before[key].as_u64().unwrap(),
            )
        });
        eprintln!(
            "REPUBLISH {label} queued={} filled={} chunks={} delta={delta:?}",
            report.queued, report.source_reads, report.chunks
        );
        assert!(report.errors.is_empty() && report.store_errors.is_empty());
        assert_eq!(report.source_reads, edits);
        if edits == 0 {
            assert_eq!(delta.map(|(_, value)| value), [0; 5]);
        }
    }
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
