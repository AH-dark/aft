//! Views-off method-dispatch narrowing: which callers a refresh recomputes
//! dispatch edges for, checked against a cold build of the edited tree with
//! the every-table differential.

use super::*;
use std::fs;

/// The hub file F. Its methods receive name-match and type-match edges from
/// three caller files, path-form `Hub::build()` calls from two more files, and
/// it holds a dispatch call of its own.
const HUB_RS: &str = "pub struct Hub;
pub struct Other;
impl Hub {
    pub fn ping(&self) {}
    pub fn pong(&self) {}
    pub fn moved(&self) {}
    pub fn renamed(&self) {}
    pub fn build() -> Hub { Hub }
}
impl Other {
    pub fn other_only(&self) {}
}
pub fn helper() {}
pub fn own_call(x: &Hub) { x.ping(); }
";

/// `x.*` calls get type-match edges (the receiver's type is in the signature);
/// `alias.*` calls get name-match edges.
const CALLER_RS: &str = "use crate::hub::{self, Hub};
pub fn typed(x: &Hub) { x.ping(); x.moved(); x.renamed(); }
pub fn named(y: &Hub) { let alias = y; alias.pong(); alias.moved(); alias.renamed(); }
pub fn direct() { hub::helper(); }
";

const PATH_CALLER_RS: &str = "use crate::hub::Hub;
pub fn path_calls() { Hub::build(); Hub::not_yet(); }
";

const EXTRA_RS: &str = "pub struct Extra;
impl Extra {
    pub fn unrelated(&self) {}
}
";

const LIB_RS: &str = "mod hub; mod caller0; mod caller1; mod caller2; mod path0; mod path1; mod extra; mod ghost_matching; mod ghost_unrelated;\n";

/// `ns.helper()` resolves directly while `web/hub.ts` exports `helper`, and
/// falls back to a name-match edge into `web/tsother.ts` when it does not.
const HUB_TS: &str = "export class Box { open(): void {} }\nexport function helper(): void {}\n";
const TS_CALLER: &str = "import * as ns from './hub';\nexport function useIt(b: any): void { ns.helper(); b.open(); }\n";
const TS_OTHER: &str = "export class Crate { helper(): void {} }\n";

/// Writes the fixture: F, its callers, and unresolved path-form calls whose
/// last segment is `probe` (`matching` of them) or `elsewhere` (`unrelated`).
fn write_fixture(root: &Path, matching: usize, unrelated: usize) {
    fs::create_dir_all(root).unwrap();
    let put = |rel: &str, text: &str| fs::write(root.join(rel), text).unwrap();
    put("hub.rs", HUB_RS);
    for n in 0..3 {
        put(&format!("caller{n}.rs"), CALLER_RS);
    }
    for n in 0..2 {
        put(&format!("path{n}.rs"), PATH_CALLER_RS);
    }
    put("extra.rs", EXTRA_RS);
    let calls = |call: &str, count: usize| {
        let mut text = String::from("pub fn calls() {\n");
        for _ in 0..count {
            text.push_str(&format!("    {call}();\n"));
        }
        text.push_str("}\n");
        text
    };
    put("ghost_matching.rs", &calls("Ghost::probe", matching));
    put(
        "ghost_unrelated.rs",
        &calls("Phantom::elsewhere", unrelated),
    );
    put("lib.rs", LIB_RS);
    put(
        "Cargo.toml",
        "[package]\nname='fixture'\nversion='0.1.0'\n[lib]\npath='lib.rs'\n",
    );
    // A subdirectory keeps `./hub` from also naming the Rust `hub.rs`.
    fs::create_dir_all(root.join("web")).unwrap();
    put("web/hub.ts", HUB_TS);
    put("web/tscaller.ts", TS_CALLER);
    put("web/tsother.ts", TS_OTHER);
}

fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in [root.to_path_buf(), root.join("web")] {
        files.extend(
            fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| matches!(ext, "rs" | "ts"))
                }),
        );
    }
    files.sort();
    files
}

fn edit(root: &Path, rel: &str, from: &str, to: &str) -> PathBuf {
    let path = root.join(rel);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(from), "{rel} does not contain {from:?}");
    fs::write(&path, text.replacen(from, to, 1)).unwrap();
    path
}

fn count(store: &CallGraphStore, sql: &str) -> usize {
    store
        .conn
        .lock()
        .unwrap()
        .query_row(sql, [], |row| row.get::<_, usize>(0))
        .unwrap()
}

fn set(files: &[&str]) -> BTreeSet<String> {
    files.iter().map(|file| file.to_string()).collect()
}

/// A cold-built fixture store that edit shapes are applied to.
struct Fixture {
    temp: tempfile::TempDir,
    root: PathBuf,
    store: CallGraphStore,
}

impl Fixture {
    fn new(matching: usize, unrelated: usize) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        write_fixture(&root, matching, unrelated);
        let store = CallGraphStore::open(temp.path().join("incremental"), root.clone()).unwrap();
        store.cold_build(&source_files(&root)).unwrap();
        Self { temp, root, store }
    }

    /// Refreshes `changed` incrementally, then checks the refresh's revision
    /// and delta and compares every table with a cold build of the same tree.
    fn refresh_and_compare(&self, label: &str, changed: &[PathBuf]) -> IncrementalStats {
        let before = self.store.projection_write_revision().unwrap().unwrap();
        let stats = self.store.refresh_files(changed).unwrap();
        eprintln!(
            "{label}: dispatch_callers_recomputed={:?} dispatch_path_form_rows_examined={}",
            stats.dispatch_callers_recomputed, stats.dispatch_path_form_rows_examined
        );
        let cold_dir = self.temp.path().join(format!("cold-{label}"));
        let cold = CallGraphStore::open(cold_dir, self.root.clone()).unwrap();
        cold.cold_build(&source_files(&self.root)).unwrap();
        incremental_parity::validate_delta(&self.store, before, &stats)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        incremental_parity::compare(&self.store, &cold)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        stats
    }
}

#[test]
fn fixture_has_the_dispatch_shape_the_edit_shapes_rely_on() {
    let fixture = Fixture::new(10, 1000);
    let store = &fixture.store;
    let dispatch_callers = |provenance: &str| {
        count(
            store,
            &format!(
                "SELECT count(DISTINCT r.caller_file) FROM edges e JOIN refs r ON r.ref_id = e.ref_id
                 WHERE e.target_file = 'hub.rs' AND e.provenance = '{provenance}' AND r.caller_file <> 'hub.rs'"
            ),
        )
    };
    assert!(dispatch_callers(PROVENANCE_NAME_MATCH) >= 3);
    assert!(dispatch_callers(PROVENANCE_TYPE_MATCH) >= 3);
    // Unresolved `Hub::method` calls in two files besides F's callers.
    assert_eq!(
        count(
            store,
            "SELECT count(DISTINCT caller_file) FROM refs
             WHERE kind = 'call' AND status = 'unresolved' AND short_name GLOB 'Hub::*'"
        ),
        2
    );
    // F's own dispatch call.
    assert_eq!(
        count(
            store,
            "SELECT count(*) FROM edges e JOIN refs r ON r.ref_id = e.ref_id
             WHERE r.caller_file = 'hub.rs' AND e.provenance = 'type_match'"
        ),
        1
    );
    for (segment, expected) in [("probe", 10), ("elsewhere", 1000)] {
        assert_eq!(
            count(
                store,
                &format!(
                    "SELECT count(*) FROM refs WHERE kind = 'call' AND status = 'unresolved'
                     AND callee_last_segment = '{segment}' AND short_name <> '{segment}'"
                )
            ),
            expected
        );
    }
    // The transitions edit relies on `ns.helper()` resolving directly.
    assert_eq!(
        count(
            store,
            "SELECT count(*) FROM refs WHERE caller_file = 'web/tscaller.ts' AND short_name = 'helper' AND status = 'resolved'"
        ),
        1
    );
}

#[test]
fn s3_a_pure_line_shift_recomputes_only_the_edited_file() {
    let fixture = Fixture::new(10, 1000);
    let path = fixture.root.join("hub.rs");
    fs::write(&path, format!("// shifted\n{HUB_RS}")).unwrap();
    let stats = fixture.refresh_and_compare("a-shift", &[path]);
    assert_eq!(stats.dispatch_callers_recomputed, set(&["hub.rs"]));
    assert_eq!(stats.dispatch_path_form_rows_examined, 0);
}

#[test]
fn s3_b_added_method_examines_only_matching_path_form_rows() {
    for (matching, unrelated) in [(10, 1000), (1000, 10)] {
        let fixture = Fixture::new(matching, unrelated);
        let path = edit(
            &fixture.root,
            "hub.rs",
            "impl Hub {\n",
            "impl Hub {\n    pub fn probe(&self) {}\n",
        );
        let stats = fixture.refresh_and_compare(&format!("b-add-{matching}"), &[path]);
        assert_eq!(
            stats.dispatch_path_form_rows_examined, matching,
            "matching={matching} unrelated={unrelated}"
        );
        assert!(stats
            .dispatch_callers_recomputed
            .contains("ghost_matching.rs"));
        assert!(!stats
            .dispatch_callers_recomputed
            .contains("ghost_unrelated.rs"));
    }
}

#[test]
fn s3_c_removed_method_with_inbound_edges() {
    let fixture = Fixture::new(10, 1000);
    let path = edit(&fixture.root, "hub.rs", "    pub fn pong(&self) {}\n", "");
    let stats = fixture.refresh_and_compare("c-remove", &[path]);
    for caller in ["caller0.rs", "caller1.rs", "caller2.rs"] {
        assert!(stats.dispatch_callers_recomputed.contains(caller));
    }
}

#[test]
fn s3_d_renamed_method() {
    let fixture = Fixture::new(10, 1000);
    let path = edit(&fixture.root, "hub.rs", "fn renamed(", "fn renamed_again(");
    fixture.refresh_and_compare("d-rename", &[path]);
}

#[test]
fn s3_e_method_moved_between_two_types_in_the_file() {
    let fixture = Fixture::new(10, 1000);
    edit(&fixture.root, "hub.rs", "    pub fn moved(&self) {}\n", "");
    let path = edit(
        &fixture.root,
        "hub.rs",
        "impl Other {\n",
        "impl Other {\n    pub fn moved(&self) {}\n",
    );
    fixture.refresh_and_compare("e-move", &[path]);
}

#[test]
fn s3_f_deleted_file() {
    let fixture = Fixture::new(10, 1000);
    // Deleting a module file also drops its `mod` declaration, as a crate
    // that still builds would.
    let path = fixture.root.join("hub.rs");
    fs::remove_file(&path).unwrap();
    let lib = edit(&fixture.root, "lib.rs", "mod hub; ", "");
    let stats = fixture.refresh_and_compare("f-delete", &[path, lib]);
    for caller in [
        "caller0.rs",
        "caller1.rs",
        "caller2.rs",
        "path0.rs",
        "path1.rs",
    ] {
        assert!(stats.dispatch_callers_recomputed.contains(caller));
    }
}

#[test]
fn s3_g_same_named_method_added_in_another_file() {
    let fixture = Fixture::new(10, 1000);
    let path = edit(
        &fixture.root,
        "extra.rs",
        "impl Extra {\n",
        "impl Extra {\n    pub fn pong(&self) {}\n",
    );
    fixture.refresh_and_compare("g-same-name", &[path]);
}

#[test]
fn s3_h_callsite_only_edit_in_a_caller() {
    let fixture = Fixture::new(10, 1000);
    let path = edit(&fixture.root, "caller0.rs", "alias.pong()", "alias.ping()");
    fixture.refresh_and_compare("h-callsite", &[path]);
}

#[test]
fn s3_i_direct_to_dispatch_and_back() {
    let fixture = Fixture::new(10, 1000);
    // `ns.helper()` in the unedited `web/tscaller.ts` loses its direct target
    // and becomes a name-match call into `web/tsother.ts`...
    let path = edit(
        &fixture.root,
        "web/hub.ts",
        "export function helper(): void {}\n",
        "",
    );
    let stats = fixture.refresh_and_compare("i-direct-to-dispatch", std::slice::from_ref(&path));
    assert!(stats
        .dispatch_callers_recomputed
        .contains("web/tscaller.ts"));
    assert_eq!(
        count(
            &fixture.store,
            "SELECT count(*) FROM edges e JOIN refs r ON r.ref_id = e.ref_id
             WHERE r.caller_file = 'web/tscaller.ts' AND e.target_file = 'web/tsother.ts' AND e.provenance = 'name_match'"
        ),
        1
    );
    // ...and resolves directly again once the export returns.
    fs::write(&path, HUB_TS).unwrap();
    let stats = fixture.refresh_and_compare("i-dispatch-to-direct", &[path]);
    assert!(stats
        .dispatch_callers_recomputed
        .contains("web/tscaller.ts"));
    assert_eq!(
        count(
            &fixture.store,
            "SELECT count(*) FROM edges e JOIN refs r ON r.ref_id = e.ref_id
             WHERE r.caller_file = 'web/tscaller.ts' AND e.provenance = 'name_match' AND e.target_file = 'web/tsother.ts'"
        ),
        0
    );
}

/// The plan rows SQLite chooses for `sql`, the path-form dispatch lookup or a
/// variant of it, or the error preparing it.
fn path_form_plan(conn: &Connection, sql: &str) -> std::result::Result<Vec<String>, String> {
    let mut statement = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .map_err(|error| error.to_string())?;
    let plan = statement
        .query_map(params!["probe"], |row| row.get::<_, String>(3))
        .map_err(|error| error.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    Ok(plan)
}

/// Ok when the lookup searches the partial index and never scans `refs`.
fn check_path_form_plan(
    plan: std::result::Result<Vec<String>, String>,
) -> std::result::Result<(), String> {
    let plan = plan?;
    let uses_index = plan.iter().any(|detail| {
        detail.starts_with("SEARCH refs USING INDEX idx_refs_unresolved_last_segment")
    });
    let scans_refs = plan.iter().any(|detail| detail.starts_with("SCAN refs"));
    if uses_index && !scans_refs {
        Ok(())
    } else {
        Err(format!("path-form lookup plan: {plan:?}"))
    }
}

#[test]
fn s3_path_form_lookup_searches_the_partial_index() {
    let fixture = Fixture::new(10, 1000);
    let conn = fixture.store.conn.lock().unwrap();
    let sql = PATH_FORM_DISPATCH_CALLERS_SQL;
    check_path_form_plan(path_form_plan(&conn, sql)).unwrap();
    // Statistics from `ANALYZE` (which `PRAGMA optimize` can run) must not
    // move the lookup off the index.
    conn.execute_batch("ANALYZE;").unwrap();
    check_path_form_plan(path_form_plan(&conn, sql)).unwrap();
    // Witnesses: the check fails when the lookup stops implying the index's
    // `WHERE`, and when the index is gone.
    let bypassed = sql.replace(" AND status = 'unresolved'", "");
    assert_ne!(bypassed, sql);
    assert!(check_path_form_plan(path_form_plan(&conn, &bypassed)).is_err());
    conn.execute_batch("DROP INDEX idx_refs_unresolved_last_segment;")
        .unwrap();
    assert!(check_path_form_plan(path_form_plan(&conn, sql)).is_err());
}

/// The probe edit of the call-graph incremental-speed study: a leading comment
/// that shifts every line, plus a new uniquely named caller/callee pair at the
/// end of the file.
fn speed_probe_edit(original: &str, tag: &str) -> String {
    let caller = format!("aft_speed_probe_caller_{tag}");
    let callee = format!("aft_speed_probe_callee_{tag}");
    let mut text = format!("// aft speed probe {tag}\n{original}");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "\npub fn {caller}() -> usize {{\n    {callee}()\n}}\npub fn {callee}() -> usize {{\n    {}\n}}\n",
        tag.len()
    ));
    text
}

fn host_load() -> String {
    fs::read_to_string("/proc/loadavg")
        .map(|load| load.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Views-off refresh phases for the probe edit on two Rust hub files of a real
/// repository. `AFT_S3_MEASURE_ROOT` names a git checkout whose tracked
/// sources are copied; `AFT_S3_MEASURE_ROUNDS` (default 3) sets the edits per
/// file. Each edit is undone with an unmeasured refresh before the next.
#[test]
#[ignore = "offline measurement: set AFT_S3_MEASURE_ROOT to a git checkout"]
fn s3_offline_hub_refresh_measurement() {
    let source = PathBuf::from(std::env::var_os("AFT_S3_MEASURE_ROOT").expect("source checkout"));
    let rounds = std::env::var("AFT_S3_MEASURE_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3usize);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    perf_tests::copy_tracked_sources(&source, &root);
    let files: Vec<_> = callgraph::walk_project_files(&root).collect();
    let store = CallGraphStore::open(temp.path().join("store"), root.clone()).unwrap();
    let started = Instant::now();
    store.cold_build(&files).unwrap();
    eprintln!(
        "S3 measurement cold build: files={} ms={} load={}",
        files.len(),
        started.elapsed().as_millis(),
        host_load()
    );
    for round in 0..rounds {
        for rel in ["crates/aft/src/protocol.rs", "crates/aft/src/context.rs"] {
            let path = root.join(rel);
            let original = fs::read_to_string(&path).unwrap();
            fs::write(&path, speed_probe_edit(&original, &format!("s3r{round}"))).unwrap();
            store.checkpoint_wal_truncate();
            let load = host_load();
            let (stats, profile) = store
                .refresh_files_profiled(std::slice::from_ref(&path))
                .unwrap();
            eprintln!(
                "S3 measurement {rel} round={round} load_before=[{load}]: {} dependency_selected_refs={} refreshed_own_files={} projection_callers={} dispatch_callers_recomputed={} dispatch_path_form_rows_examined={}",
                profile.report(),
                stats.dependency_selected_refs,
                stats.refreshed_own_files,
                stats.projection_callers.len(),
                stats.dispatch_callers_recomputed.len(),
                stats.dispatch_path_form_rows_examined,
            );
            fs::write(&path, original).unwrap();
            store.refresh_files(std::slice::from_ref(&path)).unwrap();
        }
    }
}
