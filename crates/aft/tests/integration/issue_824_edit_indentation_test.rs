//! Reproductions for issue #824: an `edit` can leave a Python file with an
//! IndentationError while reporting success.
//!
//! The four `issue_824_*` tests that are not probes drive the real agent path
//! (`tool_call` with the `edit` tool, the same request the OpenCode and Pi
//! plugins send). They are ignored because they fail on the current code. Each
//! one asserts the behaviour the issue asks for, so removing `#[ignore]` turns
//! it into the regression test for the fix. Run them with
//! `cargo test -p agent-file-tools --test integration issue_824 -- --ignored`.
//!
//! The three `probe` tests are diagnostics, not reproductions, and are ignored
//! because they only print evidence or need an external file list. One prints
//! the tree-sitter-python trees for the broken results, to show why syntax
//! validation accepts them. Another checks a candidate block-indentation rule
//! against the broken and valid fixtures in this file. The third runs that rule
//! over a list of valid Python files to look for false rejections.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

/// The file from the issue. `oldString` below drops one indentation level.
const CASE1_BEFORE: &str = "def f(x):\n    if x:\n        a = 1\n        b = 2\n    return x\n";
const CASE1_OLD: &str = "if x:\n    a = 1\n    b = 2";
const CASE1_NEW: &str = "if x:\n    a = 10\n    b = 20";
/// What AFT 0.62.0 and the current code write for case 1: only the first
/// replacement line lands at the file's indentation.
const CASE1_BROKEN: &str = "def f(x):\n    if x:\n    a = 10\n    b = 20\n    return x\n";
/// The replacement re-indented by the same amount the match was off by.
const CASE1_REINDENTED: &str =
    "def f(x):\n    if x:\n        a = 10\n        b = 20\n    return x\n";

/// An exact match whose replacement adds an over-indented line and then a
/// line back at the block's level.
const CASE2_BEFORE: &str = "def g():\n    if True:\n        return 2\n    return 3\n";
const CASE2_OLD: &str = "return 2";
const CASE2_NEW: &str = "return 2\n            y = 5\n        z = 6";
const CASE2_BROKEN: &str =
    "def g():\n    if True:\n        return 2\n            y = 5\n        z = 6\n    return 3\n";

/// TypeScript: `oldString` and `newString` written with no indentation.
const CASE3_BEFORE: &str = "export function f(x: number) {\n  if (x) {\n    const a = 1;\n    const b = 2;\n  }\n  return x;\n}\n";
const CASE3_OLD: &str = "const a = 1;\nconst b = 2;";
const CASE3_NEW: &str = "const a = 10;\nconst b = 20;";
const CASE3_COLUMN_ZERO: &str = "export function f(x: number) {\n  if (x) {\n    const a = 10;\nconst b = 20;\n  }\n  return x;\n}\n";
const CASE3_REINDENTED: &str = "export function f(x: number) {\n  if (x) {\n    const a = 10;\n    const b = 20;\n  }\n  return x;\n}\n";

/// The two argument shapes that reach find/replace: an `edits[]` item (the
/// shape the plugins' schema offers; translated to the `batch` command) and a
/// top-level `oldString` (translated to the `edit_match` command).
#[derive(Clone, Copy, Debug)]
enum Shape {
    EditsArray,
    TopLevel,
}

const SHAPES: [Shape; 2] = [Shape::EditsArray, Shape::TopLevel];

fn edit_arguments(shape: Shape, path: &str, old: &str, new: &str) -> Value {
    match shape {
        Shape::EditsArray => {
            json!({ "path": path, "edits": [{ "oldString": old, "newString": new }] })
        }
        Shape::TopLevel => json!({ "path": path, "oldString": old, "newString": new }),
    }
}

struct EditRun {
    reply: Value,
    after: String,
}

/// Write `before` to `name` in a fresh project, run one `edit` tool call with
/// `validate_on_edit: "syntax"` (the issue's setting), and return the reply
/// and the file content afterwards.
fn run_edit(shape: Shape, name: &str, before: &str, old: &str, new: &str) -> EditRun {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let file = root.join(name);
    fs::write(&file, before).unwrap();

    let mut aft = AftProcess::spawn();
    let configured = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(json!({ "validate_on_edit": "syntax", "format_on_edit": false })),
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "{configured:?}");

    let reply = aft.send(
        &json!({
            "id": "edit",
            "command": "tool_call",
            "name": "edit",
            "arguments": edit_arguments(shape, name, old, new),
        })
        .to_string(),
    );
    let after = fs::read_to_string(&file).unwrap();
    assert!(aft.shutdown().success());
    EditRun { reply, after }
}

fn reply_text(reply: &Value) -> String {
    reply["text"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| reply.to_string())
}

/// Fail with every shape's problem at once, so one run shows whether the
/// `batch` and `edit_match` routes both have the bug.
fn assert_no_failures(failures: Vec<String>) {
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

/// A fuzzy match whose `oldString` is one level too shallow must not leave a
/// file Python cannot parse while reporting success. Either the replacement is
/// re-indented to the file's level, or the edit is refused or rolled back and
/// the file is unchanged.
#[test]
#[ignore = "issue #824 repro: fails until fuzzy matches re-indent the replacement"]
fn issue_824_fuzzy_shallow_python_edit_never_leaves_invalid_file() {
    let mut failures = Vec::new();
    for shape in SHAPES {
        let run = run_edit(shape, "t.py", CASE1_BEFORE, CASE1_OLD, CASE1_NEW);
        if run.after == CASE1_BROKEN {
            failures.push(format!(
                "{shape:?}: the file was left with an IndentationError; reply: {:?}",
                reply_text(&run.reply)
            ));
        } else if run.after != CASE1_REINDENTED && run.after != CASE1_BEFORE {
            failures.push(format!(
                "{shape:?}: unexpected result {:?}; reply: {:?}",
                run.after,
                reply_text(&run.reply)
            ));
        }
    }
    assert_no_failures(failures);
}

/// An exact edit that inserts an over-indented line produces a file Python
/// rejects with "unexpected indent". Syntax validation must roll it back, the
/// same way it rolls back other invalid edits.
#[test]
#[ignore = "issue #824 repro: fails until Python syntax validation checks block indentation"]
fn issue_824_exact_misindented_python_edit_is_rolled_back() {
    let mut failures = Vec::new();
    for shape in SHAPES {
        let run = run_edit(shape, "t.py", CASE2_BEFORE, CASE2_OLD, CASE2_NEW);
        if run.after == CASE2_BROKEN {
            failures.push(format!(
                "{shape:?}: the file was left with an IndentationError; syntax_valid={} rolled_back={}; reply: {:?}",
                run.reply["syntax_valid"],
                run.reply["rolled_back"],
                reply_text(&run.reply)
            ));
        } else if run.after != CASE2_BEFORE || run.reply["rolled_back"] != true {
            failures.push(format!(
                "{shape:?}: expected a rollback, got {:?}; reply: {:?}",
                run.after, run.reply
            ));
        }
    }
    assert_no_failures(failures);
}

/// When a whitespace-insensitive pass was needed to find `oldString`, the
/// agent-facing reply must say so. Today the response carries a
/// `fuzzy_match`/`fuzzy_matches` field, but the reply text the plugins show
/// the agent (rendered by `subc_format::format_edit_summary`) drops it.
#[test]
#[ignore = "issue #824 repro: fails until the edit reply text reports a fuzzy match"]
fn issue_824_fuzzy_edit_reply_text_reports_fuzzy_match() {
    let mut failures = Vec::new();
    for shape in SHAPES {
        for (name, before, old, new) in [
            ("t.py", CASE1_BEFORE, CASE1_OLD, CASE1_NEW),
            ("t.ts", CASE3_BEFORE, CASE3_OLD, CASE3_NEW),
        ] {
            let run = run_edit(shape, name, before, old, new);
            let structured = if run.reply.get("fuzzy_matches").is_some() {
                &run.reply["fuzzy_matches"][0]
            } else {
                &run.reply["fuzzy_match"]
            };
            // The structured field is there today; only the text drops it.
            assert_eq!(
                structured["pass"], 3,
                "{shape:?} {name}: the trimmed-line pass should be the one that matched: {:?}",
                run.reply
            );
            let text = reply_text(&run.reply);
            let lower = text.to_lowercase();
            if !lower.contains("fuzzy") && !lower.contains("indentation") {
                failures.push(format!(
                    "{shape:?} {name}: reply text gives no sign a fuzzy pass was used: {text:?}; structured field: {structured}"
                ));
            }
        }
    }
    assert_no_failures(failures);
}

/// TypeScript stays valid at column 0, so this is a formatting complaint, not
/// a broken file: the second replacement line lands at column 0. Re-indenting
/// it is a behaviour change on a valid outcome.
#[test]
#[ignore = "issue #824 repro: fails until fuzzy matches re-indent the replacement"]
fn issue_824_fuzzy_unindented_ts_edit_keeps_file_indentation() {
    let mut failures = Vec::new();
    for shape in SHAPES {
        let run = run_edit(shape, "t.ts", CASE3_BEFORE, CASE3_OLD, CASE3_NEW);
        if run.after != CASE3_REINDENTED {
            let column_zero = run.after == CASE3_COLUMN_ZERO;
            failures.push(format!(
                "{shape:?}: second line written at column 0: {column_zero}; result {:?}",
                run.after
            ));
        }
    }
    assert_no_failures(failures);
}

// --- Probes -----------------------------------------------------------------

fn parse_python(source: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .expect("load tree-sitter-python");
    parser.parse(source, None).expect("parse python")
}

fn contains_missing(node: tree_sitter::Node<'_>) -> bool {
    let mut cursor = node.walk();
    loop {
        if cursor.node().is_missing() {
            return true;
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return false;
            }
        }
    }
}

/// Prints the tree-sitter-python trees for the two broken results, and pins
/// why syntax validation accepts them: neither tree holds an ERROR or a
/// MISSING node, so `has_error()` (which counts both) is false. Inputs that
/// recover with MISSING nodes show `has_error()` already rejects those.
#[test]
#[ignore = "issue #824 probe: prints parse trees"]
fn issue_824_probe_python_trees_for_broken_results() {
    for (label, source) in [("case1", CASE1_BROKEN), ("case2", CASE2_BROKEN)] {
        let tree = parse_python(source);
        let root = tree.root_node();
        println!("--- {label} ---\n{source}{}\n", root.to_sexp());
        assert!(!root.has_error(), "{label}: tree-sitter reports an error");
        assert!(
            !contains_missing(root),
            "{label}: tree holds a MISSING node"
        );
    }
    // Inputs whose recovery may insert MISSING nodes (Python first, then
    // TypeScript, whose grammar recovers that way more often). At least one
    // must hold a MISSING node and no ERROR node, and `has_error()` must still
    // be true for it: the check counts MISSING nodes regardless of language.
    let mut missing_without_error = 0;
    let typescript: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
    let python: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    for (language, source) in [
        (&python, "if x\n    pass\n"),
        (&python, "def f(a:\n    pass\n"),
        (&python, "x = [1, 2"),
        (&python, "x = (1 + 2\ny = 3\n"),
        (&python, "if x:\n    pass\nelse\n    pass\n"),
        (&python, "print(1\n"),
        (&typescript, "const x = foo(1;\n"),
        (&typescript, "function f() {\n  return 1;\n"),
        (&typescript, "if (x { y(); }\n"),
    ] {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        let sexp = root.to_sexp();
        println!(
            "--- {source:?} (has_error={}) ---\n{sexp}\n",
            root.has_error()
        );
        if contains_missing(root) && !sexp.contains("ERROR") {
            missing_without_error += 1;
            assert!(root.has_error(), "{source:?}: MISSING node not counted");
        }
    }
    assert!(
        missing_without_error > 0,
        "no input produced a MISSING-only tree"
    );

    // Indentation errors of the same family that tree-sitter also accepts.
    for (label, source) in [
        // A line dedented to a level no enclosing block uses
        // ("unindent does not match any outer indentation level").
        (
            "dedent_between_levels",
            "def h():\n    if x:\n        a = 1\n      b = 2\n",
        ),
        // A header whose body is only a comment ("expected an indented block").
        (
            "comment_only_body",
            "def k():\n    if x:\n        # todo\n    return 1\n",
        ),
    ] {
        let tree = parse_python(source);
        println!(
            "--- {label} (has_error={}) ---\n{source}{}\n",
            tree.root_node().has_error(),
            tree.root_node().to_sexp()
        );
    }
}

/// Leading-whitespace widths the way CPython's tokenizer measures them: the
/// column with tab stops every 8, and with every tab counted as 1. Two lines
/// are at the same level only when both agree; a form feed resets the column.
fn cpython_indent(prefix: &str) -> Option<(usize, usize)> {
    let mut col8 = 0;
    let mut col1 = 0;
    for ch in prefix.chars() {
        match ch {
            ' ' => {
                col8 += 1;
                col1 += 1;
            }
            '\t' => {
                col8 = (col8 / 8 + 1) * 8;
                col1 += 1;
            }
            '\x0c' => {
                col8 = 0;
                col1 = 0;
            }
            _ => return None,
        }
    }
    Some((col8, col1))
}

/// The indentation of the line `byte` is on, when `byte` is the first
/// non-whitespace character of that line and the line is not a backslash
/// continuation of the previous one. `None` otherwise.
fn statement_indent(source: &str, byte: usize) -> Option<(usize, usize)> {
    let line_start = source[..byte].rfind('\n').map_or(0, |at| at + 1);
    let indent = cpython_indent(&source[line_start..byte])?;
    if line_start > 0 {
        let previous = source[..line_start - 1].trim_end_matches('\r');
        if previous.ends_with('\\') {
            return None;
        }
    }
    Some(indent)
}

/// A candidate check for the indentation errors tree-sitter-python accepts,
/// not product code. It reports:
/// - a `block` with no statements (tree-sitter accepts `if x:` followed by
///   a dedent as an empty block; Python requires at least one statement);
/// - statements of one `block` (or of the module) that start lines at
///   different indentation;
/// - a module-level statement that starts indented;
/// - a block whose statements are not deeper than the line of its header.
fn python_indentation_problems(tree: &tree_sitter::Tree, source: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut cursor = tree.root_node().walk();
    loop {
        let node = cursor.node();
        if matches!(node.kind(), "block" | "module") {
            let mut walker = node.walk();
            let statements: Vec<_> = node
                .named_children(&mut walker)
                .filter(|child| child.kind() != "comment")
                .collect();
            let row = node.start_position().row + 1;
            if node.kind() == "block" && statements.is_empty() {
                problems.push(format!("line {row}: block with no statements"));
            }
            let mut level = (node.kind() == "module").then_some((0, 0));
            for statement in &statements {
                let Some(indent) = statement_indent(source, statement.start_byte()) else {
                    continue;
                };
                match level {
                    None => level = Some(indent),
                    Some(expected) if expected != indent => problems.push(format!(
                        "line {}: statement indentation {indent:?} differs from its block's {expected:?}",
                        statement.start_position().row + 1
                    )),
                    Some(_) => {}
                }
            }
            if let (Some(level), Some(parent)) = (level, node.parent()) {
                if node.kind() == "block" && node.start_position().row > parent.start_position().row
                {
                    if let Some(header) = statement_indent(source, parent.start_byte()) {
                        if level.0 <= header.0 || level.1 <= header.1 {
                            problems.push(format!(
                                "line {row}: block indentation {level:?} is not deeper than its header's {header:?}"
                            ));
                        }
                    }
                }
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return problems;
            }
        }
    }
}

/// The candidate rule flags each broken result above and accepts the valid
/// fixtures: the files before each edit, the correctly re-indented case 1
/// result, and a few layouts Python allows (same-line suites, comments at any
/// column, backslash continuations, tabs).
#[test]
#[ignore = "issue #824 probe: candidate indentation rule"]
fn issue_824_probe_candidate_rule_flags_broken_results() {
    for (label, source) in [
        ("case1", CASE1_BROKEN),
        ("case2", CASE2_BROKEN),
        (
            "dedent_between_levels",
            "def h():\n    if x:\n        a = 1\n      b = 2\n",
        ),
        (
            "comment_only_body",
            "def k():\n    if x:\n        # todo\n    return 1\n",
        ),
    ] {
        let problems = python_indentation_problems(&parse_python(source), source);
        println!("{label}: {problems:?}");
        assert!(!problems.is_empty(), "{label} was not flagged");
    }
    for (label, source) in [
        ("case1_before", CASE1_BEFORE),
        ("case1_reindented", CASE1_REINDENTED),
        ("case2_before", CASE2_BEFORE),
        ("same_line_suite", "if x: a = 1; b = 2\nelse: pass\n"),
        (
            "comment_at_any_column",
            "def f():\n    a = 1\n# note\n        # note\n    return a\n",
        ),
        (
            "backslash_continuation",
            "def f():\n    a = 1 + \\\n        2\n    return a\n",
        ),
        ("tabs", "def f():\n\tif x:\n\t\treturn 1\n\treturn 2\n"),
    ] {
        let problems = python_indentation_problems(&parse_python(source), source);
        assert!(problems.is_empty(), "{label} falsely flagged: {problems:?}");
    }
}

/// Runs the candidate rule over every file named in the newline-separated
/// list at `AFT_ISSUE_824_CORPUS`. The list should hold only files that
/// Python's own parser accepts, so any file flagged here is a false
/// rejection. Files tree-sitter already rejects are counted separately: the
/// current validator gives those up already.
#[test]
#[ignore = "issue #824 probe: needs AFT_ISSUE_824_CORPUS"]
fn issue_824_probe_candidate_rule_has_no_false_rejections_on_corpus() {
    let list = std::env::var("AFT_ISSUE_824_CORPUS").expect("set AFT_ISSUE_824_CORPUS");
    let list = fs::read_to_string(list).unwrap();
    let mut checked = 0usize;
    let mut tree_sitter_rejected = Vec::new();
    let mut flagged = Vec::new();
    for path in list.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(source) = fs::read_to_string(Path::new(path)) else {
            continue;
        };
        checked += 1;
        let tree = parse_python(&source);
        if tree.root_node().has_error() {
            tree_sitter_rejected.push(path.to_string());
            continue;
        }
        let problems = python_indentation_problems(&tree, &source);
        if !problems.is_empty() {
            flagged.push(format!("{path}: {problems:?}"));
        }
    }
    println!(
        "checked {checked} files; tree-sitter already rejects {}; candidate rule flags {}",
        tree_sitter_rejected.len(),
        flagged.len()
    );
    for path in tree_sitter_rejected.iter().take(20) {
        println!("tree-sitter rejects: {path}");
    }
    for entry in flagged.iter().take(20) {
        println!("flagged: {entry}");
    }
    assert!(checked > 0, "corpus list named no readable files");
    assert!(flagged.is_empty(), "{} false rejections", flagged.len());
}
