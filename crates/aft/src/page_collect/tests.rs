//! Page collection without a subc connection: reference scanning, path
//! resolution, the collection walk and its limits, and the snapshot store.

use super::scan::{css_references, html_references, srcset_urls, RefKind};
use super::snapshot::{BeginError, ReadRefusal, SnapshotStore, MAX_LIVE, TTL};
use super::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

fn raws(references: &[scan::Reference]) -> Vec<(&str, RefKind)> {
    references
        .iter()
        .map(|reference| (reference.raw.as_str(), reference.kind))
        .collect()
}

fn path(segments: &[&str]) -> Resolved {
    Resolved::Path(segments.iter().map(|segment| segment.to_string()).collect())
}

#[test]
fn page_collect_html_references_in_document_order() {
    let html = br#"<!doctype html><html><head>
        <link rel="stylesheet" href="css/site.css">
        <link rel="shortcut icon" href="favicon.ico">
        <link rel="preload" href="skipped.woff2">
        <style>@import "inline-import.css"; body { background: url('bg.png') }</style>
        <script src="app.js"></script>
        </head><body style="background-image: url(body.png)">
        <img src="a.png" srcset="a-1x.png 1x, a-2x.png 2x">
        <a href="not-collected.html">link</a>
        <source srcset="video.webm">
        </body></html>"#;
    let (references, dropped) = html_references(html);
    assert_eq!(dropped, 0);
    assert_eq!(
        raws(&references),
        vec![
            ("css/site.css", RefKind::Stylesheet),
            ("favicon.ico", RefKind::Plain),
            ("inline-import.css", RefKind::Stylesheet),
            ("bg.png", RefKind::Plain),
            ("app.js", RefKind::Plain),
            ("body.png", RefKind::Plain),
            ("a.png", RefKind::Plain),
            ("a-1x.png", RefKind::Plain),
            ("a-2x.png", RefKind::Plain),
            ("video.webm", RefKind::Plain),
        ]
    );
}

#[test]
fn page_collect_malformed_html_never_panics() {
    let mut nested = Vec::new();
    for _ in 0..5_000 {
        nested.extend_from_slice(b"<div><span");
    }
    nested.extend_from_slice(b" src=deep.png>");
    for input in [
        &b"<img src='unterminated"[..],
        b"<<<>>><img src=ok.png",
        b"\xff\xfe\x00<img src=x>",
        b"",
        &nested,
    ] {
        let _ = html_references(input);
    }
    let (references, _) = html_references(b"<p><img src=ok.png <img src='two.png'>");
    assert!(references.iter().any(|reference| reference.raw == "ok.png"));
}

#[test]
fn page_collect_attribute_entities_are_decoded() {
    let (references, _) = html_references(br#"<img src="a&amp;b.png"><img src="c&#x2e;png">"#);
    assert_eq!(
        raws(&references),
        vec![("a&b.png", RefKind::Plain), ("c.png", RefKind::Plain)]
    );
}

#[test]
fn page_collect_srcset_parsing() {
    assert_eq!(
        srcset_urls("a.png 1x, b.png 2x"),
        vec!["a.png".to_string(), "b.png".to_string()]
    );
    // A comma inside a URL is part of it; only a trailing one ends it.
    assert_eq!(
        srcset_urls("  a.png,b.png, c.png 640w  "),
        vec!["a.png,b.png", "c.png"]
    );
    // A trailing comma ends a URL; descriptors with commas inside
    // parentheses do not split a candidate.
    assert_eq!(
        srcset_urls("img/x.png, y.png (min-width: 1px, 2px) 2x, z.png"),
        vec!["img/x.png", "y.png", "z.png"]
    );
    assert_eq!(srcset_urls(" , ,"), Vec::<String>::new());
}

#[test]
fn page_collect_css_references() {
    let css = br#"
        /* url(commented.png) @import "commented.css"; */
        @import "a.css";
        @IMPORT url(b.css) screen;
        @import url( "c.css" );
        .x { background: URL(img/x.png) no-repeat; content: "url(in-string.png)"; }
        .y { background: url('img/y\'s.png'); }
        .z { background: url(img/\7a .png); }
        .w { background: url(); my-url(nope.png); }
        @font-face { src: url("fonts/f.woff2") format("woff2"); }
    "#;
    let (references, dropped) = css_references(css);
    assert_eq!(dropped, 0);
    assert_eq!(
        raws(&references),
        vec![
            ("a.css", RefKind::Stylesheet),
            ("b.css", RefKind::Stylesheet),
            ("c.css", RefKind::Stylesheet),
            ("img/x.png", RefKind::Plain),
            ("img/y's.png", RefKind::Plain),
            ("img/z.png", RefKind::Plain),
            ("fonts/f.woff2", RefKind::Plain),
        ]
    );
}

#[test]
fn page_collect_references_past_the_cap_are_counted() {
    let mut css = String::new();
    for index in 0..scan::MAX_REFERENCES_PER_FILE + 5 {
        css.push_str(&format!("a{{background:url(i{index}.png)}}"));
    }
    let (references, dropped) = css_references(css.as_bytes());
    assert_eq!(references.len(), scan::MAX_REFERENCES_PER_FILE);
    assert_eq!(dropped, 5);
}

#[test]
fn page_collect_resolve_reference_rules() {
    let none: Vec<String> = Vec::new();
    let css_dir = vec!["css".to_string()];
    // Fragments and queries name no other file.
    for raw in ["", "  ", "#top", "?v=1"] {
        assert_eq!(resolve_reference(raw, &none), Resolved::Skip, "{raw:?}");
    }
    // Anything with a scheme, a host or an absolute path is never fetched.
    for raw in [
        "http://example.com/a.png",
        "https://x/y",
        "data:image/png;base64,AAAA",
        "file:///etc/passwd",
        "//host/a.png",
        "/etc/passwd",
        "\\\\server\\share\\a.png",
        "C:/Windows/win.ini",
        "javascript:alert(1)",
    ] {
        assert_eq!(
            resolve_reference(raw, &none),
            Resolved::NotRelative,
            "{raw:?}"
        );
    }
    assert_eq!(
        resolve_reference("img/a.png?v=2#x", &none),
        path(&["img", "a.png"])
    );
    // Percent-encoding is decoded per segment.
    assert_eq!(
        resolve_reference("img/my%20photo%2Ejpg", &none),
        path(&["img", "my photo.jpg"])
    );
    assert_eq!(
        resolve_reference("caf%C3%A9.png", &none),
        path(&["café.png"])
    );
    // An encoded separator, an invalid UTF-8 byte, or a folder names no file.
    for raw in [
        "a%2Fb.png",
        "a%5Cb.png",
        "a%00.png",
        "%FF.png",
        "img/",
        "..",
        "img/a:b.png",
    ] {
        assert_eq!(resolve_reference(raw, &none), Resolved::Invalid, "{raw:?}");
    }
    // `..` that stays inside the page's folder, and `..` that leaves it.
    assert_eq!(
        resolve_reference("../img/a.png", &css_dir),
        path(&["img", "a.png"])
    );
    assert_eq!(resolve_reference("./a/../b.png", &none), path(&["b.png"]));
    assert_eq!(
        resolve_reference("../../shared/a.png", &css_dir),
        path(&["..", "shared", "a.png"])
    );
    assert_eq!(
        resolve_reference("%2E%2E/up.png", &none),
        path(&["..", "up.png"])
    );
    // A backslash reads as a slash, as browsers read it.
    assert_eq!(
        resolve_reference("img\\a.png", &none),
        path(&["img", "a.png"])
    );
}

#[test]
fn page_collect_limits_defaults_and_ceilings() {
    assert_eq!(Limits::from_request(None).unwrap(), Limits::DEFAULT);
    let limits = Limits::from_request(Some(&json!({
        "entry_max_bytes": 10, "max_files": 100_000, "max_file_bytes": u64::MAX,
        "max_total_bytes": 5, "import_depth": 99, "deadline_ms": 600_000,
    })))
    .unwrap();
    assert_eq!(
        limits,
        Limits {
            entry_max_bytes: 10,
            max_files: Limits::CEILING.max_files,
            max_file_bytes: Limits::CEILING.max_file_bytes,
            max_total_bytes: 5,
            import_depth: Limits::CEILING.import_depth,
            deadline_ms: Limits::CEILING.deadline_ms,
        }
    );
    assert!(Limits::from_request(Some(&json!({"max_files": 0}))).is_err());
    assert!(Limits::from_request(Some(&json!({"deadline_ms": -1}))).is_err());
    assert!(Limits::from_request(Some(&json!([1]))).is_err());
}

#[test]
fn page_collect_mime_by_extension() {
    assert_eq!(mime_for("index.HTML"), "text/html");
    assert_eq!(mime_for("css/a.css"), "text/css");
    assert_eq!(mime_for("img/a.svg"), "image/svg+xml");
    assert_eq!(mime_for("fonts/x.woff2"), "font/woff2");
    assert_eq!(mime_for("README"), "application/octet-stream");
    assert_eq!(mime_for("blob.xyz"), "application/octet-stream");
}

/// Files by absolute path, with denied paths and a record of every read.
#[derive(Default)]
struct FakeSource {
    files: HashMap<PathBuf, Vec<u8>>,
    denied: Vec<PathBuf>,
    reads: Vec<PathBuf>,
}

impl FakeSource {
    fn with(root: &Path, files: &[(&str, &[u8])]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(name, bytes)| (root.join(name), bytes.to_vec()))
                .collect(),
            ..Self::default()
        }
    }
}

impl PageSource for FakeSource {
    fn read(&mut self, path: &Path, limit: u64) -> Result<Vec<u8>, ReadFailure> {
        self.reads.push(path.to_path_buf());
        if self.denied.iter().any(|denied| path.starts_with(denied)) {
            return Err(ReadFailure::Denied);
        }
        let bytes = self.files.get(path).ok_or(ReadFailure::Missing)?;
        if bytes.len() as u64 > limit {
            return Err(ReadFailure::TooLarge {
                size: bytes.len() as u64,
            });
        }
        Ok(bytes.clone())
    }
}

/// Keeps records in memory, computing them as the snapshot builder does.
#[derive(Default)]
struct MemorySink(Vec<(String, Vec<u8>)>);

impl SnapshotSink for MemorySink {
    fn add(&mut self, path: &str, bytes: &[u8]) -> std::io::Result<FileRecord> {
        use sha2::Digest;
        self.0.push((path.to_string(), bytes.to_vec()));
        Ok(FileRecord {
            path: path.to_string(),
            size: bytes.len() as u64,
            sha256: format!("{:x}", sha2::Sha256::digest(bytes)),
            mime: mime_for(path),
        })
    }
}

fn root() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\site")
    } else {
        PathBuf::from("/site")
    }
}

fn run(source: &mut FakeSource, limits: Limits) -> Result<Collected, CollectRefusal> {
    collect(
        &root().join("index.html"),
        &limits,
        Instant::now(),
        source,
        &mut MemorySink::default(),
    )
}

fn names(collected: &Collected) -> Vec<&str> {
    collected
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect()
}

fn omitted(collected: &Collected) -> Vec<(&str, &str)> {
    collected
        .omitted
        .iter()
        .map(|omitted| (omitted.path.as_str(), omitted.reason.as_str()))
        .collect()
}

#[test]
fn page_collect_walk_lists_files_and_omissions() {
    let root = root();
    let mut source = FakeSource::with(
        &root,
        &[
            (
                "index.html",
                br#"<link rel=stylesheet href="css/site.css"><img src="img/a.png">
                <img src="img/a.png#again"><img src="https://cdn/x.png"><img src="https://cdn/x.png">
                <img src="secret/key.png"><img src="gone.png"><img src="index.html">"#,
            ),
            ("css/site.css", b"body{background:url(../img/bg.png)} @import 'more.css';"),
            ("css/more.css", b".m{background:url(m.png)}"),
            ("css/m.png", b"M"),
            ("img/a.png", b"A"),
            ("img/bg.png", b"BG"),
            ("secret/key.png", b"KEY"),
        ],
    );
    source.denied.push(root.join("secret"));
    let collected = run(&mut source, Limits::DEFAULT).unwrap();
    assert_eq!(collected.entry, "index.html");
    assert_eq!(
        names(&collected),
        vec![
            "index.html",
            "css/site.css",
            "img/a.png",
            "img/bg.png",
            "css/more.css",
            "css/m.png"
        ]
    );
    assert_eq!(
        omitted(&collected),
        vec![
            ("https://cdn/x.png", "not_relative"),
            ("secret/key.png", "denied"),
            ("gone.png", "missing"),
        ]
    );
    // Every file is read once, and nothing named only by a not-relative
    // reference is ever read.
    let mut reads = source.reads.clone();
    reads.sort();
    reads.dedup();
    assert_eq!(reads.len(), source.reads.len(), "a file was read twice");
    assert!(!source
        .reads
        .iter()
        .any(|read| read.to_string_lossy().contains("cdn")));
    let a = &collected.files[2];
    assert_eq!(
        (a.size, a.mime, a.sha256.as_str()),
        (
            1,
            "image/png",
            "559aead08264d5795d3909718cdd05abd49572e84fe55590eef31a88a08fdffd"
        )
    );
}

#[test]
fn page_collect_dot_dot_escaping_the_page_folder_is_listed_relative() {
    let root = root();
    let parent = root.parent().unwrap().to_path_buf();
    let mut source = FakeSource::with(
        &root,
        &[
            (
                "index.html",
                b"<img src='../shared/logo.png'><img src='sub/../in.png'>",
            ),
            ("in.png", b"I"),
        ],
    );
    source
        .files
        .insert(parent.join("shared").join("logo.png"), b"L".to_vec());
    let collected = run(&mut source, Limits::DEFAULT).unwrap();
    assert_eq!(
        names(&collected),
        vec!["index.html", "../shared/logo.png", "in.png"]
    );
    assert!(source
        .reads
        .contains(&parent.join("shared").join("logo.png")));
}

#[test]
fn page_collect_every_limit_fires_with_its_reason() {
    let root = root();
    let html: &[u8] = br#"<link rel=stylesheet href=d1.css><img src=big.png><img src=one.png>
        <img src=two.png><img src=three.png>"#;
    let files: &[(&str, &[u8])] = &[
        ("index.html", html),
        ("d1.css", b"@import 'd2.css';"),
        ("d2.css", b"@import 'd3.css';"),
        ("d3.css", b".x{}"),
        ("big.png", &[0u8; 100]),
        ("one.png", &[1u8; 10]),
        ("two.png", &[2u8; 10]),
        ("three.png", &[3u8; 10]),
    ];

    // depth_limit: d1 is one stylesheet deep, d2 two, d3 three.
    let mut source = FakeSource::with(&root, files);
    let collected = run(
        &mut source,
        Limits {
            import_depth: 2,
            ..Limits::DEFAULT
        },
    )
    .unwrap();
    assert_eq!(omitted(&collected), vec![("d3.css", "depth_limit")]);
    assert!(!source.reads.contains(&root.join("d3.css")));

    // over_file_limit
    let mut source = FakeSource::with(&root, files);
    let collected = run(
        &mut source,
        Limits {
            max_file_bytes: 50,
            ..Limits::DEFAULT
        },
    )
    .unwrap();
    assert_eq!(omitted(&collected), vec![("big.png", "over_file_limit")]);

    // over_total_limit: after the page, d1.css and big.png, 15 bytes are
    // left: one.png (10) fits, nothing after it does. d2.css is reached last
    // because the walk takes the page's own references first.
    let used = html.len() as u64 + 17 + 100;
    let mut source = FakeSource::with(&root, files);
    let collected = run(
        &mut source,
        Limits {
            max_total_bytes: used + 15,
            ..Limits::DEFAULT
        },
    )
    .unwrap();
    assert_eq!(
        omitted(&collected),
        vec![
            ("two.png", "over_total_limit"),
            ("three.png", "over_total_limit"),
            ("d2.css", "over_total_limit"),
        ]
    );

    // over_count_limit: the page counts as one file.
    let mut source = FakeSource::with(&root, files);
    let collected = run(
        &mut source,
        Limits {
            max_files: 3,
            ..Limits::DEFAULT
        },
    )
    .unwrap();
    assert_eq!(names(&collected), vec!["index.html", "d1.css", "big.png"]);
    assert_eq!(
        omitted(&collected),
        vec![
            ("one.png", "over_count_limit"),
            ("two.png", "over_count_limit"),
            ("three.png", "over_count_limit"),
            ("d2.css", "over_count_limit"),
        ]
    );

    // deadline: the page is still read; nothing it names is.
    let mut source = FakeSource::with(&root, files);
    let collected = run(
        &mut source,
        Limits {
            deadline_ms: 0,
            ..Limits::DEFAULT
        },
    )
    .unwrap();
    assert_eq!(names(&collected), vec!["index.html"]);
    assert!(omitted(&collected)
        .iter()
        .all(|(_, reason)| *reason == "deadline"));
    assert_eq!(source.reads, vec![root.join("index.html")]);
}

#[test]
fn page_collect_entry_refusals() {
    let root = root();
    let refusal = |files: &[(&str, &[u8])], entry: &str, limits: Limits| {
        let mut source = FakeSource::with(&root, files);
        collect(
            &root.join(entry),
            &limits,
            Instant::now(),
            &mut source,
            &mut MemorySink::default(),
        )
        .unwrap_err()
    };
    let error = refusal(&[("notes.txt", b"<p>")], "notes.txt", Limits::DEFAULT);
    assert_eq!(
        (error.code(), error.detail()),
        ("entry_not_html", json!({"reason": "extension"}))
    );
    let error = refusal(
        &[("index.html", b"\x89PNG\r\n\x1a\n\0\0")],
        "index.html",
        Limits::DEFAULT,
    );
    assert_eq!(
        (error.code(), error.detail()),
        ("entry_not_html", json!({"reason": "content"}))
    );
    let error = refusal(
        &[("index.html", b"just text")],
        "index.html",
        Limits::DEFAULT,
    );
    assert_eq!(error.code(), "entry_not_html");
    let error = refusal(&[], "index.html", Limits::DEFAULT);
    assert_eq!(
        (error.code(), error.detail()),
        ("entry_unreadable", json!({"reason": "missing"}))
    );
    let error = refusal(
        &[("index.html", b"<html>0123456789</html>")],
        "index.html",
        Limits {
            entry_max_bytes: 8,
            ..Limits::DEFAULT
        },
    );
    assert_eq!(
        (error.code(), error.detail()),
        ("entry_too_large", json!({"size": 23, "limit": 8}))
    );
    let mut source = FakeSource::with(&root, &[("index.html", b"<html>")]);
    source.denied.push(root.join("index.html"));
    let error = collect(
        &root.join("index.html"),
        &Limits::DEFAULT,
        Instant::now(),
        &mut source,
        &mut MemorySink::default(),
    )
    .unwrap_err();
    assert_eq!(
        (error.code(), error.detail()),
        ("entry_unreadable", json!({"reason": "denied"}))
    );
    assert!(sniff_html(b"\xEF\xBB\xBF  \n<!doctype html>"));
}

fn store() -> (tempfile::TempDir, Arc<SnapshotStore>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SnapshotStore::open(dir.path()));
    (dir, store)
}

fn one_file(store: &Arc<SnapshotStore>, bytes: &[u8]) -> String {
    let mut builder = store.begin().unwrap();
    builder.add("index.html", bytes).unwrap();
    builder.commit().0
}

#[test]
fn page_collect_snapshot_read_ranges() {
    let (_dir, store) = store();
    let id = one_file(&store, b"0123456789");
    assert_eq!(store.read(&id, "index.html", 0, 4).unwrap(), b"0123");
    assert_eq!(store.read(&id, "index.html", 8, 100).unwrap(), b"89");
    assert_eq!(store.read(&id, "index.html", 10, 1).unwrap(), b"");
    assert_eq!(
        store.read(&id, "index.html", 11, 1),
        Err(ReadRefusal::BadRange)
    );
    assert_eq!(
        store.read(&id, "index.html", 0, 0),
        Err(ReadRefusal::BadRange)
    );
    assert_eq!(
        store.read(&id, "index.html", 0, MAX_READ_LENGTH + 1),
        Err(ReadRefusal::BadRange)
    );
    assert!(store.read(&id, "index.html", 0, MAX_READ_LENGTH).is_ok());
    assert_eq!(
        store.read(&id, "other.html", 0, 1),
        Err(ReadRefusal::NotInSnapshot)
    );
    assert_eq!(
        store.read("no-such-snapshot", "index.html", 0, 1),
        Err(ReadRefusal::Expired)
    );
}

#[test]
fn page_collect_snapshot_expires_and_is_deleted() {
    let (_dir, store) = store();
    let id = one_file(&store, b"<html>");
    let folder = store.root().join(&id);
    assert!(folder.is_dir());
    store.advance_for_test(TTL - Duration::from_millis(1));
    assert!(store.read(&id, "index.html", 0, 1).is_ok());
    store.advance_for_test(Duration::from_millis(1));
    store.expire_due();
    assert!(!folder.exists(), "an expired snapshot's files remain");
    assert_eq!(
        store.read(&id, "index.html", 0, 1),
        Err(ReadRefusal::Expired)
    );
}

#[test]
fn page_collect_fifth_snapshot_is_busy_until_one_ends() {
    let (_dir, store) = store();
    let ids = (0..MAX_LIVE)
        .map(|_| one_file(&store, b"<html>"))
        .collect::<Vec<_>>();
    store.advance_for_test(Duration::from_secs(10));
    match store.begin() {
        Err(BeginError::Busy(busy)) => {
            assert!((40_000..=50_000).contains(&busy.retry_after_ms), "{busy:?}")
        }
        Err(BeginError::Io(error)) => panic!("{error}"),
        Ok(_) => panic!("a fifth snapshot was admitted"),
    }
    assert!(store.release(&ids[0]));
    let builder = store.begin().expect("a released place is free again");
    // A collection in progress holds a place too.
    assert!(matches!(store.begin(), Err(BeginError::Busy(_))));
    drop(builder);
    assert_eq!(store.live_count(), MAX_LIVE - 1);
    store.advance_for_test(TTL);
    assert!(store.begin().is_ok(), "expired snapshots free their places");
}

#[test]
fn page_collect_release_is_idempotent() {
    let (_dir, store) = store();
    let id = one_file(&store, b"<html>");
    let folder = store.root().join(&id);
    assert!(store.release(&id));
    assert!(!folder.exists());
    assert!(!store.release(&id));
    assert!(!store.release("never-existed"));
    assert_eq!(
        store.read(&id, "index.html", 0, 1),
        Err(ReadRefusal::Expired)
    );
}

#[test]
fn page_collect_abandoned_builder_leaves_nothing() {
    let (_dir, store) = store();
    let mut builder = store.begin().unwrap();
    builder.add("index.html", b"<html>").unwrap();
    drop(builder);
    assert_eq!(store.live_count(), 0);
    assert_eq!(std::fs::read_dir(store.root()).unwrap().count(), 0);
}

#[test]
fn page_collect_leftover_snapshots_are_deleted_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let leftover = dir.path().join(snapshot::SNAPSHOT_DIR).join("old");
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("f0"), b"stale").unwrap();
    let _store = SnapshotStore::open(dir.path());
    assert!(!leftover.exists());
}

#[cfg(unix)]
#[test]
fn page_collect_snapshot_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, store) = store();
    let id = one_file(&store, b"<html>");
    let folder = store.root().join(&id);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&folder), 0o700);
    assert_eq!(mode(&folder.join("f0")), 0o600);
}

#[test]
fn page_collect_same_page_twice_has_identical_hashes() {
    let (_dir, store) = store();
    let root = root();
    let files: &[(&str, &[u8])] = &[
        (
            "index.html",
            b"<img src=a.png><link rel=stylesheet href=s.css>",
        ),
        ("a.png", b"PNG"),
        ("s.css", b"p{}"),
    ];
    let mut snapshots = Vec::new();
    for _ in 0..2 {
        let mut builder = store.begin().unwrap();
        let collected = collect(
            &root.join("index.html"),
            &Limits::DEFAULT,
            Instant::now(),
            &mut FakeSource::with(&root, files),
            &mut builder,
        )
        .unwrap();
        let (id, _) = builder.commit();
        snapshots.push((id, collected.files));
    }
    assert_ne!(snapshots[0].0, snapshots[1].0);
    assert_eq!(snapshots[0].1, snapshots[1].1);
}
