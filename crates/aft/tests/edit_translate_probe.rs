//! Opt-in probes for the work a tool call does *before* its handler runs:
//! the argument translation and the undo snapshot an edit takes first.
//!
//! Both tests are ignored because the numbers depend on the machine and on a
//! process-wide allocator counter. Run them with:
//!
//! ```text
//! cargo test --release -p agent-file-tools --test edit_translate_probe -- --ignored --nocapture
//! ```
//!
//! The recorded results live in
//! `.cortexkit/internal-docs/investigations/edit-translate-latency.md`.

use aft::backup::BackupStore;
use aft::config::Config;
use aft::context::AppContext;
use aft::parser::TreeSitterProvider;
use aft::subc_translate::{subc_translate_owned_with_edit_target_probe, TranslateContext};
use serde_json::{json, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SAMPLES: usize = 50;

/// Counts allocations for the whole process. Tests run in parallel in one
/// binary, and each measurement trades one small allocation for the trusting
/// read, so noise only ever makes a count larger.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

struct CountingAlloc;
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

fn allocations() -> u64 {
    ALLOCATIONS.load(Ordering::Relaxed)
}

fn root(name: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("edit-translate-probe")
        .join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create probe root");
    root
}

fn fixture_file(root: &Path, name: &str, lines: usize, token: &str) -> PathBuf {
    let path = root.join(name);
    let mut body = String::new();
    for index in 0..lines {
        if index % 40 == 7 {
            body.push_str(&format!("const VALUE_{index} = \"{token}\";\n"));
        } else {
            body.push_str(&format!(
                "export function fn_{index}() {{\n  return {index};\n}}\n"
            ));
        }
    }
    std::fs::write(&path, body).expect("write fixture");
    path
}

fn ten_hunk_find_replace(padding: usize) -> Value {
    let filler = "x".repeat(padding);
    let hunks: Vec<Value> = (0..10)
        .map(|index| {
            let line = 40 * index + 7;
            json!({
                "oldString": format!("const VALUE_{line} = \"TOKEN_A\"; {filler}"),
                "newString": format!("const VALUE_{line} = \"TOKEN_B\"; {filler}"),
            })
        })
        .collect();
    json!({ "path": "two.ts", "edits": hunks })
}

fn ten_hunk_lines() -> Value {
    let items: Vec<Value> = (0..10)
        .map(|index| {
            let line = index * 40 + 1;
            json!({ "startLine": line, "endLine": line + 1, "content": format!("// block {index}\n") })
        })
        .collect();
    json!({ "path": "two.ts", "edits": items })
}

fn patch(hunks: usize) -> String {
    let mut text = String::from("*** Begin Patch\n");
    text.push_str("*** Update File: one.ts\n");
    for index in 0..hunks {
        let line = 40 * index + 7;
        text.push_str(&format!("@@ const VALUE_{line} @@\n"));
        text.push_str(&format!("-const VALUE_{line} = \"TOKEN_A\";\n"));
        text.push_str(&format!("+const VALUE_{line} = \"TOKEN_B\";\n"));
    }
    text.push_str("*** End Patch\n");
    text
}

fn translate_cases() -> Vec<(&'static str, &'static str, Value)> {
    let body = "export const x = 1;\n".repeat(300);
    vec![
        (
            "edit",
            "find/replace 1 hunk",
            json!({
                "path": "one.ts",
                "edits": [{ "oldString": "const VALUE_47 = \"TOKEN_A\";", "newString": "const VALUE_47 = \"TOKEN_B\";" }],
            }),
        ),
        ("edit", "find/replace 10 hunks", ten_hunk_find_replace(0)),
        (
            "edit",
            "find/replace 10 hunks, 4 KB",
            ten_hunk_find_replace(200),
        ),
        ("edit", "line-range 10 hunks", ten_hunk_lines()),
        (
            "edit",
            "oldString/newString folded",
            json!({
                "path": "one.ts",
                "oldString": "const VALUE_7 = \"TOKEN_A\";",
                "newString": "const VALUE_7 = \"TOKEN_B\";",
            }),
        ),
        (
            "write",
            "6 KB body",
            json!({ "path": "three.ts", "content": body }),
        ),
        ("apply_patch", "1 hunk", json!({ "patchText": patch(1) })),
        ("apply_patch", "10 hunks", json!({ "patchText": patch(10) })),
    ]
}

fn translate(tool: &str, args: &Value, project_root: &Path, app_ctx: &AppContext) {
    // The probe `run_tool_call` installs for a glob-spelled target. It is
    // consulted only for arguments that reach the glob branch, so a call it
    // receives names a filesystem validation the prepare performed.
    let probe = |resolved: &str| {
        aft::commands::edit_match::classify_edit_target(resolved, app_ctx, "probe")
            != aft::commands::edit_match::EditTargetKind::Literal
    };
    let typed = args.as_object().expect("probe arguments are an object");
    if let Err(error) = subc_translate_owned_with_edit_target_probe(
        tool,
        Value::Object(typed.clone()),
        project_root,
        TranslateContext::default(),
        &probe,
    ) {
        panic!("probe arguments must translate: {:?}", error.message);
    }
}

fn pair(mut samples: Vec<Duration>) -> (f64, f64) {
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2].as_secs_f64() * 1000.0;
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)].as_secs_f64() * 1000.0;
    (p50, p95)
}

#[test]
#[ignore = "timing and allocation probe; run with --ignored --nocapture"]
fn edit_translate_timing_probe() {
    let project_root = root("translate");
    fixture_file(&project_root, "one.ts", 400, "TOKEN_A");
    fixture_file(&project_root, "two.ts", 400, "TOKEN_A");
    fixture_file(&project_root, "three.ts", 400, "TOKEN_C");
    let app_ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

    println!(
        "{:12} {:28} {:>10} {:>10} {:>12}",
        "tool", "shape", "p50 ms", "p95 ms", "allocations"
    );
    for (tool, label, args) in translate_cases() {
        translate(tool, &args, &project_root, &app_ctx);

        let mut samples = Vec::new();
        for _ in 0..SAMPLES {
            let started = Instant::now();
            translate(tool, &args, &project_root, &app_ctx);
            samples.push(started.elapsed());
        }
        let (p50, p95) = pair(samples);

        let before = allocations();
        translate(tool, &args, &project_root, &app_ctx);
        let used = allocations() - before - 1;
        println!("{tool:12} {label:28} {p50:10.3} {p95:10.3} {used:12}");
    }
}

fn backup_store(root: &Path) -> BackupStore {
    let mut store = BackupStore::new();
    // The production layout: one storage directory holding per-session stacks.
    store.set_storage_dir(root.join("backups"), 24);
    store.set_db_project_key("edit-translate-probe".to_string());
    store
}

fn snapshot_samples(store: &mut BackupStore, session: &str, path: &Path, depth: usize) -> f64 {
    // `depth` names the stack length the sample starts from, so the numbers
    // separate the first snapshot of a file from a later one that already has
    // history to rewrite.
    for index in 0..depth {
        store
            .snapshot_with_op(session, path, &format!("warm {index}"), None)
            .expect("warm snapshot");
    }
    let mut samples = Vec::new();
    for _ in 0..SAMPLES {
        let started = Instant::now();
        store
            .snapshot_with_op(session, path, "probe", None)
            .expect("snapshot");
        samples.push(started.elapsed());
    }
    pair(samples).0
}

#[test]
#[ignore = "timing probe; run with --ignored --nocapture"]
fn backup_snapshot_cost_probe() {
    let root = root("backup");
    let file = fixture_file(&root, "big.ts", 2000, "TOKEN_A");
    let five_files: Vec<PathBuf> = (0..5)
        .map(|index| fixture_file(&root, &format!("five_{index}.ts"), 2000, "TOKEN_A"))
        .collect();
    let mut store = backup_store(&root);
    let session = "probe-session";

    let empty = snapshot_samples(&mut store, session, &file, 0);
    println!("first snapshot of a 2,000-line file: p50 {empty:.3} ms");
    let warm = snapshot_samples(&mut store, session, &file, 3);
    println!("snapshot on a stack of 3 entries:     p50 {warm:.3} ms");
    let deep = snapshot_samples(&mut store, session, &file, 10);
    println!("snapshot on a stack of 10 entries:    p50 {deep:.3} ms");

    let mut batch = Vec::new();
    for _ in 0..SAMPLES {
        let started = Instant::now();
        for path in &five_files {
            store
                .snapshot_with_op(session, path, "batch", Some("probe-op"))
                .expect("batch snapshot");
        }
        batch.push(started.elapsed());
    }
    let batch_p50 = pair(batch).0;
    println!(
        "five files in one operation:          p50 {batch_p50:.3} ms (per file {:.3} ms)",
        batch_p50 / 5.0
    );
}
