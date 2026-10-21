//! The crate-wide Rust module domain is seeded only when the module digest
//! changes. Each transition compares every table with a cold build of the
//! target manifest.

use super::*;

/// Adds or replaces a configuration input, as the manifest builder stores
/// `Cargo.toml`.
fn config(conn: &Connection, manifest: &mut Manifest, path: &str, source: &str) {
    let payload = join::CallgraphBlob::config(source.as_bytes(), "fixture")
        .to_bytes()
        .unwrap();
    let key = blake3::hash(&payload);
    conn.execute(
        "INSERT OR IGNORE INTO blob_payloads VALUES (?1, ?2, ?3, 1)",
        params![
            key.as_bytes().as_slice(),
            payload,
            blake3::hash(&payload).as_bytes().as_slice()
        ],
    )
    .unwrap();
    insert(
        manifest,
        path,
        ManifestEntry::Regular {
            mode: 0o100644,
            planes: RegularPlanes {
                callgraph: Some(key.to_hex().to_string()),
                semantic: None,
            },
            resolution_input: true,
        },
    );
}

fn insert(manifest: &mut Manifest, path: &str, entry: ManifestEntry) {
    *manifest = Manifest::new(
        manifest
            .entries()
            .filter(|(candidate, _)| candidate.as_bytes() != path.as_bytes())
            .map(|(path, entry)| (path.clone(), entry.clone())),
    )
    .unwrap();
    manifest
        .insert(RelPath::new(path.as_bytes()).unwrap(), entry)
        .unwrap();
}

/// A file without call-graph output, such as the non-`.rs` file a `#[path]`
/// attribute can name.
fn plain(manifest: &mut Manifest, path: &str) {
    insert(
        manifest,
        path,
        ManifestEntry::Regular {
            mode: 0o100644,
            planes: RegularPlanes {
                callgraph: None,
                semantic: None,
            },
            resolution_input: false,
        },
    );
}

fn remove(manifest: &mut Manifest, path: &str) {
    *manifest = Manifest::new(
        manifest
            .entries()
            .filter(|(candidate, _)| candidate.as_bytes() != path.as_bytes())
            .map(|(path, entry)| (path.clone(), entry.clone())),
    )
    .unwrap();
}

fn rust_fixture(files: &[(&str, &str)]) -> Fixture {
    let mut f = fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    f.base = manifest(&conn, files);
    f.next = f.base.clone();
    f
}

/// Replaces one file of `f.next` with `source`.
fn edit(f: &mut Fixture, path: &str, source: &str) {
    let conn = Connection::open(&f.blobs).unwrap();
    let edited = manifest(&conn, &[(path, source)]);
    let (_, entry) = edited.entries().next().unwrap();
    insert(&mut f.next, path, entry.clone());
}

/// Applies `f.base -> f.next` to a copy of the cold base, requires every table
/// to equal a cold build of `f.next`, and returns the incremental stats.
fn transition(f: &Fixture, label: &str) -> MaterializeStats {
    let (_, copy) = prepare(f);
    let stats = apply_manifest_diff(&copy, &f.base, &f.next, &f.blobs).unwrap();
    let cold = f.dir.path().join(format!("{label}-cold.sqlite"));
    materialize_manifest_view_database(&cold, &f.blobs, &f.next).unwrap();
    assert_snapshot_parity(&snapshot(&cold), &snapshot(&copy));
    println!("{label}: {stats:?}");
    stats
}

/// The same transition with the domain seeded on every publication, as before
/// the digest existed.
fn transition_always_seeded(f: &Fixture, label: &str) -> MaterializeStats {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            module_digest::FORCE_CHANGED.with(|forced| forced.set(false));
        }
    }
    module_digest::FORCE_CHANGED.with(|forced| forced.set(true));
    let _reset = Reset;
    transition(f, label)
}

const DOMAIN_CALLERS: [&str; 5] = ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs"];

/// Five callers reach an inline module by path and take its type as a
/// receiver, so their bindings and receiver type traces read the crate-wide
/// module index. No `mod` declares `src/leaf.rs`, so no file depends on it
/// and no trace reads it.
fn domain_fixture() -> Fixture {
    let caller =
        |name: &str| format!("pub fn {name}(t: crate::m::T) {{ crate::m::f(); t.go(); }}\n");
    let callers = DOMAIN_CALLERS
        .iter()
        .map(|path| {
            let name = path.trim_start_matches("src/").trim_end_matches(".rs");
            (*path, caller(name))
        })
        .collect::<Vec<_>>();
    let mut files = vec![
        (
            "src/lib.rs",
            "pub mod m {\n    pub struct T;\n    impl T {\n        pub fn go(&self) {}\n    }\n    pub fn f() {}\n}\nmod a;\nmod b;\nmod c;\nmod d;\nmod e;\n"
                .to_string(),
        ),
        ("src/leaf.rs", "pub fn leaf() -> u32 {\n    1\n}\n".to_string()),
    ];
    files.extend(callers.into_iter().map(|(path, source)| (path, source)));
    let files = files
        .iter()
        .map(|(path, source)| (*path, source.as_str()))
        .collect::<Vec<_>>();
    rust_fixture(&files)
}

#[test]
fn body_only_rust_edit_selects_nothing_through_the_module_domain() {
    let mut f = domain_fixture();
    // The added line moves the function's end, so the parse blob changes
    // while no module digest input does.
    edit(
        &mut f,
        "src/leaf.rs",
        "pub fn leaf() -> u32 {\n    // edited\n    2\n}\n",
    );

    // Seeded as before the digest, the domain alone selects the crate root
    // and the five callers, and five receiver type traces.
    let seeded = transition_always_seeded(&f, "body-only-always-seeded");
    assert!(seeded.module_domain_callers >= 5, "{seeded:?}");
    assert!(seeded.module_domain_traces >= 5, "{seeded:?}");

    // The counted work comes first, so a regression names what it costs.
    let stats = transition(&f, "body-only");
    assert_eq!(stats.module_domain_callers, 0, "{stats:?}");
    assert_eq!(stats.module_domain_traces, 0, "{stats:?}");
    assert!(stats.selected_callers <= 1, "{stats:?}");
    assert_eq!(stats.dispatch_type_target_traces, 0, "{stats:?}");
    assert_eq!(stats.module_digest_decoded_blobs, 1, "{stats:?}");
    assert!(!stats.module_digest_changed, "{stats:?}");
    assert!(!stats.module_digest_full, "{stats:?}");
}

#[test]
fn typescript_only_edit_decodes_no_blob_to_decide_seeding() {
    let mut f = domain_fixture();
    let conn = Connection::open(&f.blobs).unwrap();
    let web = manifest(
        &conn,
        &[
            (
                "web/app.ts",
                "import { util } from './util'; export function app() { return util(); }",
            ),
            ("web/util.ts", "export function util() { return 1; }"),
        ],
    );
    for (path, entry) in web.entries() {
        insert(
            &mut f.base,
            std::str::from_utf8(path.as_bytes()).unwrap(),
            entry.clone(),
        );
    }
    f.next = f.base.clone();
    edit(
        &mut f,
        "web/util.ts",
        "export function util() {\n  return 2;\n}",
    );
    let seeded = transition_always_seeded(&f, "typescript-always-seeded");
    assert!(seeded.module_domain_callers >= 5, "{seeded:?}");
    let stats = transition(&f, "typescript");
    assert_eq!(stats.module_domain_callers, 0, "{stats:?}");
    assert_eq!(stats.module_domain_traces, 0, "{stats:?}");
    assert_eq!(stats.module_digest_decoded_blobs, 0, "{stats:?}");
    assert!(!stats.module_digest_changed, "{stats:?}");
}

/// `src/p/q.rs` is declared by `src/r.rs` through `#[path]`; adding `mod q;`
/// to `src/p.rs` makes `src/p.rs` its parent, because the first declaring
/// file in path order wins. `super::f()` in `q.rs` then names `p.rs`'s `f`,
/// and `q.rs` records no dependency on `p.rs`, only on the module domain.
/// There is no crate root: every file of a crate depends on its root, and the
/// root on every file it declares, which would select `q.rs` through them.
fn parent_fixture() -> Fixture {
    let mut f = rust_fixture(&[
        ("src/p.rs", "pub fn f() {}\n"),
        ("src/r.rs", "#[path = \"p/q.rs\"]\nmod q;\npub fn f() {}\n"),
        ("src/p/q.rs", "pub fn g() {\n    super::f();\n}\n"),
    ]);
    edit(&mut f, "src/p.rs", "mod q;\npub fn f() {}\n");
    f
}

#[test]
fn mod_declaration_added_seeds_the_module_domain() {
    let f = parent_fixture();
    let stats = transition(&f, "mod-declaration-added");
    assert!(stats.module_digest_changed, "{stats:?}");
    assert!(!stats.module_digest_full, "{stats:?}");
    assert!(stats.module_domain_callers >= 1, "{stats:?}");
}

/// A base whose graph and `view_manifest_fingerprint` are at M2 while its
/// digest rows were recorded at M1, as when a writer that does not maintain
/// them published M2. M1 has `mod q;` in `src/p.rs`, M2 removes it and M3
/// adds it back. The stale rows therefore already describe M3, so an update
/// from them sees no change and would leave `q.rs` calling `r.rs`'s `f`:
/// at M2 `q.rs` depends on `r.rs` and the module domain, not on `p.rs`.
#[test]
fn stale_digest_records_are_computed_in_full_and_seed_the_domain() {
    let f = parent_fixture();
    let (with_declaration, without_declaration) = (&f.next, &f.base);
    let m1 = f.dir.path().join("m1.sqlite");
    materialize_manifest_view_database(&m1, &f.blobs, with_declaration).unwrap();
    let m2 = f.dir.path().join("m2.sqlite");
    std::fs::copy(&m1, &m2).unwrap();
    apply_manifest_diff(&m2, with_declaration, without_declaration, &f.blobs).unwrap();
    {
        let conn = Connection::open(&m2).unwrap();
        conn.execute("ATTACH DATABASE ?1 AS m1", [m1.to_str().unwrap()])
            .unwrap();
        conn.execute_batch(
            "DELETE FROM view_module_digest_inputs;
             DELETE FROM view_module_path_targets;
             INSERT INTO view_module_digest_inputs SELECT * FROM m1.view_module_digest_inputs;
             INSERT INTO view_module_path_targets SELECT * FROM m1.view_module_path_targets;
             INSERT OR REPLACE INTO meta(k, v) SELECT k, v FROM m1.meta
                 WHERE k IN ('view_module_digest', 'view_module_digest_fingerprint');
             DETACH DATABASE m1;",
        )
        .unwrap();
    }
    let stats = apply_manifest_diff(&m2, without_declaration, with_declaration, &f.blobs).unwrap();
    println!("stale: {stats:?}");
    let cold = f.dir.path().join("m3-cold.sqlite");
    materialize_manifest_view_database(&cold, &f.blobs, with_declaration).unwrap();
    assert_snapshot_parity(&snapshot(&cold), &snapshot(&m2));
    assert!(stats.module_digest_full, "{stats:?}");
    assert!(stats.module_digest_changed, "{stats:?}");
    assert!(stats.module_domain_callers >= 1, "{stats:?}");
}

#[test]
fn rust_mod_file_moved_to_directory_form_seeds_the_module_domain() {
    let caller = "mod target; pub fn caller() { target::target(); }";
    let mut f = rust_fixture(&[
        ("src/lib.rs", caller),
        ("src/target.rs", "pub fn target() {}"),
        ("other/src/lib.rs", "pub fn other() {}"),
    ]);
    remove(&mut f.next, "src/target.rs");
    edit(&mut f, "src/target/mod.rs", "pub fn target() {}");
    let stats = transition(&f, "mod-file-moved");
    assert!(stats.module_digest_changed, "{stats:?}");
}

/// `[lib] path` makes `src/root.rs` the crate root, which moves the file
/// its `mod foo;` names from `src/root/foo.rs` to `src/foo.rs`. No `.rs`
/// file changes.
#[test]
fn cargo_manifest_change_alone_seeds_the_module_domain() {
    let mut f = rust_fixture(&[
        ("src/root.rs", "mod foo;\npub fn r() {}\n"),
        ("src/foo.rs", "pub fn a() {\n    super::r();\n}\n"),
        ("src/root/foo.rs", "pub fn b() {\n    super::r();\n}\n"),
    ]);
    let conn = Connection::open(&f.blobs).unwrap();
    config(
        &conn,
        &mut f.base,
        "Cargo.toml",
        "[package]\nname = \"app\"\n",
    );
    config(
        &conn,
        &mut f.next,
        "Cargo.toml",
        "[package]\nname = \"app\"\n[lib]\npath = \"src/root.rs\"\n",
    );
    let stats = transition(&f, "cargo-only");
    assert!(stats.module_digest_changed, "{stats:?}");
    assert_eq!(stats.module_digest_decoded_blobs, 0, "{stats:?}");
}

#[test]
fn cargo_workspace_member_and_crate_added_seed_the_module_domain() {
    let caller = "pub fn caller() { new_crate::target(); }";
    let mut f = rust_fixture(&[
        ("app/src/lib.rs", caller),
        ("unrelated/src/lib.rs", "pub fn other() {}"),
    ]);
    let conn = Connection::open(&f.blobs).unwrap();
    config(
        &conn,
        &mut f.base,
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\"]",
    );
    config(
        &conn,
        &mut f.base,
        "app/Cargo.toml",
        "[package]\nname = \"app\"",
    );
    f.next = f.base.clone();
    config(
        &conn,
        &mut f.next,
        "Cargo.toml",
        "[workspace]\nmembers = [\"app\", \"new\"]",
    );
    config(
        &conn,
        &mut f.next,
        "new/Cargo.toml",
        "[package]\nname = \"new-crate\"",
    );
    edit(&mut f, "new/src/lib.rs", "pub fn target() {}");
    let stats = transition(&f, "cargo-member-added");
    assert!(stats.module_digest_changed, "{stats:?}");
}

/// `#[path]` may name a file that is not Rust source; its existence decides
/// whether `mod gen;` has a file. No `.rs` file changes.
fn path_target_fixture() -> Fixture {
    let mut f = rust_fixture(&[(
        "src/lib.rs",
        "#[path = \"generated/gen.inc\"]\nmod gen;\npub fn r() {}\n",
    )]);
    plain(&mut f.next, "src/generated/gen.inc");
    f
}

#[test]
fn non_rust_path_target_added_seeds_the_module_domain() {
    let f = path_target_fixture();
    let stats = transition(&f, "path-target-added");
    assert!(stats.module_digest_changed, "{stats:?}");
    assert_eq!(stats.module_digest_decoded_blobs, 0, "{stats:?}");
}

#[test]
fn non_rust_path_target_removed_seeds_the_module_domain() {
    let mut f = path_target_fixture();
    std::mem::swap(&mut f.base, &mut f.next);
    let stats = transition(&f, "path-target-removed");
    assert!(stats.module_digest_changed, "{stats:?}");
    assert_eq!(stats.module_digest_decoded_blobs, 0, "{stats:?}");
}

#[test]
fn path_attribute_changed_seeds_the_module_domain() {
    let mut f = rust_fixture(&[
        (
            "src/lib.rs",
            "#[path = \"a_impl.rs\"]\nmod imp;\npub fn h() {}\n",
        ),
        ("src/a_impl.rs", "pub fn g() {\n    super::h();\n}\n"),
        ("src/b_impl.rs", "pub fn g() {\n    super::h();\n}\n"),
    ]);
    edit(
        &mut f,
        "src/lib.rs",
        "#[path = \"b_impl.rs\"]\nmod imp;\npub fn h() {}\n",
    );
    let stats = transition(&f, "path-attribute-changed");
    assert!(stats.module_digest_changed, "{stats:?}");
}

/// `crate::x::inner::h` resolves through the inline module of `src/x.rs`,
/// which gains `h`.
#[test]
fn function_added_to_inline_module_seeds_the_module_domain() {
    let mut f = rust_fixture(&[
        ("src/lib.rs", "mod x;\nmod y;\n"),
        ("src/x.rs", "pub mod inner {\n    pub fn other() {}\n}\n"),
        ("src/y.rs", "pub fn y() {\n    crate::x::inner::h();\n}\n"),
    ]);
    edit(
        &mut f,
        "src/x.rs",
        "pub mod inner {\n    pub fn other() {}\n    pub fn h() {}\n}\n",
    );
    let stats = transition(&f, "inline-function-added");
    assert!(stats.module_digest_changed, "{stats:?}");
}
