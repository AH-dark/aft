//! Reproductions for two reported tool defects, driven through the agent tool
//! path (`tool_call`), which is what the OpenCode plugin sends.
//!
//! Issue #828 (`aft_import`): a trailing same-line comment splits organize into
//! independent segments, Python groups sort by raw statement text, a leading
//! comment loses its import when a segment is reordered, and `remove` leaves
//! the removed statement's trailing comment behind.
//!
//! Issue #827 (`ast_grep_replace` / `ast_grep_search`): a dry run previews a
//! rewrite that apply refuses for invalid syntax, and a variadic capture is
//! displayed with its punctuation tokens joined by commas (`1,,,2`).
//!
//! Each test is ignored while the defect is open; run them with
//! `--ignored` to see the current failure.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::helpers::AftProcess;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create temp project");
    // Canonical root so fixture paths agree with the paths the tool reports.
    let root = fs::canonicalize(dir.path()).expect("canonical project root");
    for (name, body) in files {
        fs::write(root.join(name), body).expect("write fixture");
    }
    (dir, root)
}

fn configured(root: &Path) -> AftProcess {
    let mut aft = AftProcess::spawn();
    let resp = aft.configure(root);
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
    aft
}

fn tool_call(aft: &mut AftProcess, id: &str, name: &str, arguments: Value) -> Value {
    aft.send(
        &json!({ "id": id, "command": "tool_call", "name": name, "arguments": arguments })
            .to_string(),
    )
}

/// The agent-visible reply: rendered text on success, the message on failure.
fn reply_text(resp: &Value) -> String {
    resp["text"]
        .as_str()
        .or_else(|| resp["message"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn line_index(content: &str, line: &str) -> usize {
    content
        .lines()
        .position(|candidate| candidate.trim_end() == line)
        .unwrap_or_else(|| panic!("missing line {line:?} in:\n{content}"))
}

// ---------------------------------------------------------------------------
// #828 aft_import
// ---------------------------------------------------------------------------

#[test]
#[ignore = "fails today (#828): a trailing `# noqa` on an import splits organize into separately sorted segments"]
fn organize_python_trailing_comment_does_not_split_stdlib_group() {
    let source = "import sys\nimport os\nimport zeta.side  # noqa\nimport abc\nfrom . import rel\nfrom collections import OrderedDict\nimport json\n\nprint(sys, os, abc, rel, OrderedDict, json)\n";
    let (_dir, root) = project(&[("case.py", source)]);
    let mut aft = configured(&root);

    let resp = tool_call(
        &mut aft,
        "org-py-noqa",
        "import",
        json!({ "op": "organize", "filePath": "case.py" }),
    );
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    let content = fs::read_to_string(root.join("case.py")).unwrap();

    // The comment belongs to its import and must travel with it.
    assert!(
        content.contains("import zeta.side  # noqa\n"),
        "trailing comment must stay on its import:\n{content}"
    );
    // All five stdlib imports form one block: no other import line sits
    // between the first and the last of them.
    let stdlib = [
        "import abc",
        "import json",
        "import os",
        "import sys",
        "from collections import OrderedDict",
    ];
    let positions: Vec<usize> = stdlib.iter().map(|l| line_index(&content, l)).collect();
    let first = *positions.iter().min().unwrap();
    let last = *positions.iter().max().unwrap();
    let foreign = content
        .lines()
        .enumerate()
        .filter(|(i, _)| *i > first && *i < last)
        .filter(|(_, line)| !line.trim().is_empty() && !stdlib.contains(&line.trim_end()))
        .map(|(_, line)| line.to_string())
        .collect::<Vec<_>>();
    assert!(
        foreign.is_empty(),
        "stdlib imports were split by {foreign:?}:\n{content}"
    );

    aft.shutdown();
}

#[test]
#[ignore = "fails today (#828): Python groups sort by raw statement text, so `from x import` lines precede `import y` lines"]
fn organize_python_sorts_group_by_module_not_raw_text() {
    let source = "import json\nfrom collections import OrderedDict\nimport abc\n\nprint(json, OrderedDict, abc)\n";
    let (_dir, root) = project(&[("sorting.py", source)]);
    let mut aft = configured(&root);

    let resp = tool_call(
        &mut aft,
        "org-py-sort",
        "import",
        json!({ "op": "organize", "filePath": "sorting.py" }),
    );
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    let content = fs::read_to_string(root.join("sorting.py")).unwrap();

    // Both isort conventions (plain imports first, or one module-ordered list)
    // put `import abc` before `from collections ...` and `import json`.
    let abc = line_index(&content, "import abc");
    let collections = line_index(&content, "from collections import OrderedDict");
    let json_line = line_index(&content, "import json");
    assert!(
        abc < collections && abc < json_line,
        "stdlib group is not in module order:\n{content}"
    );

    aft.shutdown();
}

#[test]
#[ignore = "fails today (#828): organize reorders a comment-bounded segment and leaves the leading comment above a different import"]
fn organize_ts_leading_comment_stays_with_its_import() {
    let source = "import \"./z-side\";\nimport {z} from \"./z\";\n// about y\nimport {y} from \"./y\";\nimport fs from \"fs\";\nimport \"reflect-metadata\";\nimport {x} from \"./x\";\nimport path from \"path\";\n\nexport const all = [z, y, fs, x, path];\n";
    let (_dir, root) = project(&[("case.ts", source)]);
    let mut aft = configured(&root);

    let resp = tool_call(
        &mut aft,
        "org-ts-comment",
        "import",
        json!({ "op": "organize", "filePath": "case.ts" }),
    );
    assert_eq!(resp["success"], true, "organize should succeed: {resp:?}");
    let content = fs::read_to_string(root.join("case.ts")).unwrap();

    let comment = line_index(&content, "// about y");
    let y_import = line_index(&content, "import { y } from \"./y\";");
    assert_eq!(
        comment + 1,
        y_import,
        "`// about y` must stay directly above the ./y import:\n{content}"
    );
    // Side-effect imports stay ordering barriers: nothing crosses them.
    let side = line_index(&content, "import \"reflect-metadata\";");
    assert!(
        line_index(&content, "import fs from \"fs\";") < side,
        "{content}"
    );
    assert!(
        side < line_index(&content, "import { x } from \"./x\";"),
        "{content}"
    );

    aft.shutdown();
}

#[test]
#[ignore = "fails today (#828): remove deletes the statement but leaves its same-line trailing comment on its own line"]
fn remove_takes_the_trailing_same_line_comment_with_the_import() {
    let ts = "// about y\nimport { y } from \"./y\"; // trailing\nimport { w } from \"./w\";\n\nexport const all = [w];\n";
    let py = "import os  # noqa\nimport sys\n\nprint(sys)\n";
    let (_dir, root) = project(&[("case.ts", ts), ("case.py", py)]);
    let mut aft = configured(&root);

    let resp = tool_call(
        &mut aft,
        "rm-ts",
        "import",
        json!({ "op": "remove", "filePath": "case.ts", "module": "./y" }),
    );
    assert_eq!(resp["success"], true, "remove should succeed: {resp:?}");
    let content = fs::read_to_string(root.join("case.ts")).unwrap();
    assert!(
        !content.contains("// trailing"),
        "the removed import's trailing comment was left behind:\n{content}"
    );
    assert!(
        content.contains("import { w } from \"./w\";\n"),
        "{content}"
    );

    let resp = tool_call(
        &mut aft,
        "rm-py",
        "import",
        json!({ "op": "remove", "filePath": "case.py", "module": "os" }),
    );
    assert_eq!(resp["success"], true, "remove should succeed: {resp:?}");
    let content = fs::read_to_string(root.join("case.py")).unwrap();
    assert!(
        !content.contains("# noqa"),
        "the removed import's trailing comment was left behind:\n{content}"
    );
    assert!(content.starts_with("import sys\n"), "{content}");

    aft.shutdown();
}

// ---------------------------------------------------------------------------
// #827 ast_grep_replace / ast_grep_search
// ---------------------------------------------------------------------------

#[test]
#[ignore = "fails today (#827): dry run reports success for a rewrite that apply refuses as invalid syntax"]
fn ast_replace_dry_run_refuses_rewrite_that_apply_would_refuse() {
    let original = "foo(1, 2);\nfoo(3, 4);\n";
    let (_dir, root) = project(&[("dry.ts", original)]);
    let mut aft = configured(&root);
    let args = |dry_run: bool| {
        json!({
            "pattern": "foo($A, $B)",
            "rewrite": "bar($B, $A",
            "lang": "typescript",
            "paths": ["dry.ts"],
            "dryRun": dry_run,
        })
    };

    // Control: apply refuses this rewrite today.
    let applied = tool_call(&mut aft, "apply", "ast_replace", args(false));
    assert_eq!(applied["success"], false, "apply must refuse: {applied:?}");
    assert!(
        reply_text(&applied).contains("syntax validation"),
        "{applied:?}"
    );

    let preview = tool_call(&mut aft, "preview", "ast_replace", args(true));
    let text = reply_text(&preview);
    assert!(
        text.contains("syntax validation"),
        "the preview must report the same syntax refusal as apply, got success={} text:\n{text}",
        preview["success"]
    );
    assert!(
        !text.contains("Would replace"),
        "the preview must not promise a replacement apply refuses:\n{text}"
    );
    assert_eq!(fs::read_to_string(root.join("dry.ts")).unwrap(), original);

    aft.shutdown();
}

#[test]
#[ignore = "fails today (#827): a variadic capture renders as its node texts joined by commas, punctuation included (`1,,,2`)"]
fn ast_search_renders_variadic_capture_as_source_text() {
    let (_dir, root) = project(&[("search.ts", "foo(1, 2);\n")]);
    let mut aft = configured(&root);

    let resp = tool_call(
        &mut aft,
        "search",
        "ast_search",
        json!({ "pattern": "foo($$$A)", "lang": "typescript", "paths": ["search.ts"] }),
    );
    assert_eq!(resp["success"], true, "search should succeed: {resp:?}");
    let text = reply_text(&resp);
    assert!(
        text.lines().any(|line| line.trim() == "$A: 1, 2"),
        "variadic capture should show its source text:\n{text}"
    );

    aft.shutdown();
}
