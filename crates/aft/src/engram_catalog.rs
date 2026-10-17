//! AFT's backup catalog for ENGRAM, the fleet backup service.
//!
//! ENGRAM reads `<storage root>/engram-catalog.json` to learn which parts of
//! AFT's storage it may capture. Every module writes its own catalog at
//! startup so a freshly provisioned machine is covered without a manual step.
//!
//! Two ENGRAM rules shape what is written here:
//! - a declared path that does not exist halts the whole fleet capture, so
//!   every entry is derived from what is on disk at the moment of writing;
//! - entry ids are the key of ENGRAM's capture history, so an entry for a
//!   folder that already had a catalog entry keeps exactly that id.
//!
//! Today the catalog only declares the per-harness `backups` trees (and any
//! parked `backups.parked-<suffix>` copies) as device-local filetrees: they are
//! documented to ENGRAM but not captured. The rest of the storage root is
//! intentionally not declared yet.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;

/// File name ENGRAM looks for in each module's data directory.
pub const CATALOG_FILENAME: &str = "engram-catalog.json";

/// ENGRAM checks that `module_id` matches the name of the directory holding
/// the catalog; AFT's storage root is `<fleet data home>/aft`.
const MODULE_ID: &str = "aft";

/// Descriptor schema version understood by ENGRAM.
const SCHEMA_VERSION: u32 = 1;

/// Harness folder names with a fixed spelling.
const FIXED_HARNESS_DIRS: &[&str] = &["opencode", "pi", "runner"];

/// Prefixes of harness folders whose names embed a client name or fingerprint.
const MCP_HARNESS_PREFIX: &str = "mcp--";
const FED_HARNESS_PREFIX: &str = "fed--";

const BACKUPS_DIR: &str = "backups";
const PARKED_BACKUPS_PREFIX: &str = "backups.parked-";

/// Length of the hex hash an MCP harness folder name ends with
/// (`mcp--<readable client>--<32 hex>`).
const MCP_HASH_HEX_LEN: usize = 32;

#[derive(Debug, Serialize)]
struct Descriptor {
    schema_version: u32,
    module_id: &'static str,
    entries: Vec<Entry>,
}

#[derive(Debug, Serialize)]
struct Entry {
    entry_id: String,
    class: &'static str,
    mechanism: &'static str,
    path: String,
}

/// What [`write_catalog`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The file was created or replaced.
    Written,
    /// The file already held exactly these bytes, so it was left untouched
    /// (its modification time is preserved).
    Unchanged,
}

/// One backup folder found on disk, before its entry id is settled.
struct Candidate {
    /// `/`-separated path relative to the storage root.
    path: String,
    /// The id this folder gets unless another folder already took it.
    preferred_id: String,
    /// The id used when `preferred_id` is taken by an earlier folder.
    fallback_id: String,
}

/// Write the catalog once at daemon startup. Never fails: a catalog that
/// cannot be written must not stop AFT from serving, so errors become a
/// warning in the log.
pub fn write_catalog_at_startup(storage_root: &Path) {
    match write_catalog(storage_root) {
        Ok(WriteOutcome::Written) => crate::slog_info!(
            "engram catalog written: {}",
            storage_root.join(CATALOG_FILENAME).display()
        ),
        Ok(WriteOutcome::Unchanged) => {}
        Err(error) => crate::slog_warn!(
            "engram catalog not written under {}: {error}",
            storage_root.display()
        ),
    }
}

/// Render the catalog for `storage_root` and write it atomically, skipping
/// the write when the file already holds the same bytes.
pub fn write_catalog(storage_root: &Path) -> io::Result<WriteOutcome> {
    if !storage_root.is_dir() {
        // Nothing to describe yet, and creating the storage root is not this
        // module's job.
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("storage root {} does not exist", storage_root.display()),
        ));
    }
    let bytes = render_catalog(storage_root)?;
    write_if_changed(&storage_root.join(CATALOG_FILENAME), &bytes)
}

/// Render the catalog bytes for the folders that exist under `storage_root`
/// right now. The same tree always renders the same bytes.
pub fn render_catalog(storage_root: &Path) -> io::Result<Vec<u8>> {
    let descriptor = Descriptor {
        schema_version: SCHEMA_VERSION,
        module_id: MODULE_ID,
        entries: assign_entry_ids(collect_candidates(storage_root)?),
    };
    let mut bytes = serde_json::to_vec_pretty(&descriptor).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn collect_candidates(storage_root: &Path) -> io::Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for harness_dir in sorted_dir_names(storage_root)? {
        let Some(label) = harness_label(&harness_dir) else {
            continue;
        };
        let harness_path = storage_root.join(&harness_dir);
        if !is_real_dir(&harness_path) {
            continue;
        }
        if is_real_dir(&harness_path.join(BACKUPS_DIR)) {
            candidates.push(Candidate {
                path: format!("{harness_dir}/{BACKUPS_DIR}"),
                preferred_id: format!("aft/backups-{label}"),
                // Only reachable when two MCP folders share a readable client
                // name; the folder name itself carries the distinguishing hash.
                fallback_id: format!("aft/backups-{}", harness_dir.replace("--", "-")),
            });
        }
        // Sorted names put the oldest dated park first, so the first parked
        // folder keeps the undated id and later ones add their suffix.
        for child in sorted_dir_names(&harness_path)? {
            let Some(suffix) = child.strip_prefix(PARKED_BACKUPS_PREFIX) else {
                continue;
            };
            if !is_safe_name_part(suffix) || !is_real_dir(&harness_path.join(&child)) {
                continue;
            }
            candidates.push(Candidate {
                path: format!("{harness_dir}/{child}"),
                preferred_id: format!("aft/backups-{label}-parked"),
                fallback_id: format!("aft/backups-{label}-parked-{suffix}"),
            });
        }
    }
    Ok(candidates)
}

/// Give each candidate a unique id, in order: the preferred id when free,
/// else the fallback id, else the fallback with a counter.
fn assign_entry_ids(candidates: Vec<Candidate>) -> Vec<Entry> {
    let mut used = BTreeSet::new();
    let mut entries = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let entry_id = if !used.contains(&candidate.preferred_id) {
            candidate.preferred_id
        } else if !used.contains(&candidate.fallback_id) {
            candidate.fallback_id
        } else {
            (2..)
                .map(|n| format!("{}-{n}", candidate.fallback_id))
                .find(|id| !used.contains(id))
                .expect("an unbounded counter always finds a free id")
        };
        used.insert(entry_id.clone());
        entries.push(Entry {
            entry_id,
            class: "device-local",
            mechanism: "filetree",
            path: candidate.path,
        });
    }
    entries
}

/// The harness part of an entry id for a storage-root folder, or `None` when
/// the folder is not a harness folder.
///
/// MCP folders are `mcp--<readable client>--<hash>`; the id uses the client
/// name only (`mcp-claude-code`), matching the ids ENGRAM already has.
/// Federated folders have no readable name, so the whole folder name is used
/// with its `--` separators shortened.
fn harness_label(dir_name: &str) -> Option<String> {
    if FIXED_HARNESS_DIRS.contains(&dir_name) {
        return Some(dir_name.to_string());
    }
    if !is_safe_name_part(dir_name) {
        return None;
    }
    if let Some(rest) = dir_name.strip_prefix(MCP_HARNESS_PREFIX) {
        let readable = match rest.rsplit_once("--") {
            Some((readable, hash))
                if !readable.is_empty()
                    && hash.len() == MCP_HASH_HEX_LEN
                    && hash.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                readable
            }
            _ => rest,
        };
        if readable.is_empty() {
            return None;
        }
        return Some(format!("mcp-{}", readable.replace("--", "-")));
    }
    if dir_name.starts_with(FED_HARNESS_PREFIX) && dir_name.len() > FED_HARNESS_PREFIX.len() {
        return Some(dir_name.replace("--", "-"));
    }
    None
}

/// Folder names copied into ids and paths are limited to the characters AFT
/// itself uses for them, so a stray folder can never produce a path ENGRAM
/// rejects (separators, `..`, drive prefixes, non-ASCII).
fn is_safe_name_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A directory that is not a symlink: ENGRAM resolves declared paths itself,
/// and a link would make the declared folder depend on its target.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// UTF-8 child names of `dir`, sorted. Non-UTF-8 names can never be harness
/// or backup folders and are skipped.
fn sorted_dir_names(dir: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        if let Ok(name) = entry?.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Write `bytes` to `target` unless the file already holds them.
///
/// The bytes go to a temporary file in the same folder, which is then renamed
/// over the target, so a reader never sees a half-written catalog; the
/// temporary file is removed when any step fails. A dev or test build refuses
/// to write into the account's real storage, like every other versioned
/// record AFT writes there.
fn write_if_changed(target: &Path, bytes: &[u8]) -> io::Result<WriteOutcome> {
    if fs::read(target).is_ok_and(|existing| existing == bytes) {
        return Ok(WriteOutcome::Unchanged);
    }
    crate::production_storage::refuse_write(target)?;
    crate::durability::atomic_write(
        target,
        bytes,
        crate::durability::AtomicWriteDurability::File,
    )?;
    Ok(WriteOutcome::Written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Harness;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    /// The MCP folder name on the machine whose hand-written catalog these
    /// ids continue.
    const LIVE_MCP_DIR: &str = "mcp--claude-code--b0deb2ca83f40515f2f23d13bfe131c1";

    /// A throwaway storage root named `aft`, because ENGRAM requires the
    /// catalog's `module_id` to match the folder holding it.
    fn storage_root() -> (tempfile::TempDir, PathBuf) {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join(MODULE_ID);
        fs::create_dir(&root).unwrap();
        (fixture, root)
    }

    fn mkdir(root: &Path, rel: &str) {
        fs::create_dir_all(root.join(rel)).unwrap();
    }

    fn entries(bytes: &[u8]) -> Vec<(String, String)> {
        let value: Value = serde_json::from_slice(bytes).unwrap();
        value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["entry_id"].as_str().unwrap().to_string(),
                    entry["path"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// The checks ENGRAM's `catalog::plan` and the `catalog_check` preflight
    /// apply, ported here because engram-core is not a dependency of AFT:
    /// supported schema, `module_id` equal to the holding folder's name, known
    /// classes and mechanisms, unique entry ids, a non-blank reason and a path
    /// for excluded entries, held-read-transaction consent for portable page-db
    /// entries, root-relative `/`-separated paths with no empty, `.` or `..`
    /// part, no backslash and no drive prefix. On top of that every declared
    /// path must exist, since a missing one halts the whole fleet capture.
    fn assert_engram_accepts(root: &Path, bytes: &[u8]) {
        let value: Value = serde_json::from_slice(bytes).expect("catalog is JSON");
        assert_eq!(value["schema_version"], 1, "supported schema version");
        assert_eq!(
            value["module_id"].as_str(),
            root.file_name().and_then(|name| name.to_str()),
            "module_id must match the folder holding the catalog"
        );
        let mut seen = BTreeSet::new();
        for entry in value["entries"].as_array().expect("entries array") {
            let id = entry["entry_id"].as_str().expect("entry_id string");
            assert!(!id.is_empty(), "entry id must not be empty");
            assert!(seen.insert(id.to_string()), "duplicate entry id {id}");
            let class = entry["class"].as_str().expect("class string");
            assert!(
                ["portable", "device-local", "rebuild", "excluded"].contains(&class),
                "{id}: unknown class {class}"
            );
            let mechanism = entry["mechanism"].as_str().expect("mechanism string");
            assert!(
                [
                    "whole-db",
                    "page-db",
                    "owner-export",
                    "filetree",
                    "log-stream"
                ]
                .contains(&mechanism),
                "{id}: unknown mechanism {mechanism}"
            );
            if class == "excluded" {
                let reason = entry["reason"].as_str().unwrap_or("");
                assert!(!reason.trim().is_empty(), "{id}: excluded needs a reason");
                assert!(entry["path"].is_string(), "{id}: excluded needs a path");
            }
            if class == "portable" && mechanism == "page-db" {
                assert_eq!(
                    entry["writer_interaction"], "read-transaction-live",
                    "{id}: page-db needs read-transaction-live"
                );
            }
            let path = entry["path"]
                .as_str()
                .expect("every AFT entry names a path");
            let bytes = path.as_bytes();
            assert!(
                !path.contains('\\')
                    && !(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
                    && !Path::new(path).is_absolute()
                    && path
                        .split('/')
                        .all(|part| !part.is_empty() && part != "." && part != ".."),
                "{id}: path {path:?} is not a canonical root-relative path"
            );
            assert!(
                root.join(path).exists(),
                "{id}: declared path {path} does not exist"
            );
        }
    }

    #[test]
    fn todays_backup_folders_keep_todays_entry_ids() {
        // The MCP folder the hand-written catalog describes is the one AFT
        // derives for the `claude-code` client.
        assert_eq!(
            Harness::Mcp {
                client: "claude-code".to_string()
            }
            .storage_segment(),
            LIVE_MCP_DIR
        );
        let (_fixture, root) = storage_root();
        for rel in [
            "opencode/backups",
            "pi/backups",
            "runner/backups",
            &format!("{LIVE_MCP_DIR}/backups"),
            "opencode/backups.parked-20260710",
            // Siblings that are not backup trees and must stay undeclared.
            "opencode/checkpoints",
            "opencode/bash-tasks",
            "semantic",
            "index",
            "logs",
        ] {
            mkdir(&root, rel);
        }
        fs::write(root.join("aft.db"), b"").unwrap();

        let bytes = render_catalog(&root).unwrap();
        assert_engram_accepts(&root, &bytes);
        let mut found = entries(&bytes);
        found.sort();
        let mut expected = vec![
            ("aft/backups-opencode", "opencode/backups"),
            ("aft/backups-pi", "pi/backups"),
            ("aft/backups-runner", "runner/backups"),
            (
                "aft/backups-mcp-claude-code",
                "mcp--claude-code--b0deb2ca83f40515f2f23d13bfe131c1/backups",
            ),
            (
                "aft/backups-opencode-parked",
                "opencode/backups.parked-20260710",
            ),
        ]
        .into_iter()
        .map(|(id, path)| (id.to_string(), path.to_string()))
        .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(found, expected);

        let value: Value = serde_json::from_slice(&bytes).unwrap();
        for entry in value["entries"].as_array().unwrap() {
            assert_eq!(entry["class"], "device-local");
            assert_eq!(entry["mechanism"], "filetree");
        }
    }

    #[test]
    fn only_folders_that_exist_are_declared() {
        let (_fixture, root) = storage_root();
        assert!(entries(&render_catalog(&root).unwrap()).is_empty());

        // A harness folder without backups, a `backups` that is a file, a
        // parked name with no suffix, and a folder that is not a harness.
        mkdir(&root, "pi");
        mkdir(&root, "runner");
        fs::write(root.join("runner/backups"), b"").unwrap();
        mkdir(&root, "opencode/backups.parked-");
        mkdir(&root, "semantic/backups");
        let bytes = render_catalog(&root).unwrap();
        assert!(
            entries(&bytes).is_empty(),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        assert_engram_accepts(&root, &bytes);

        mkdir(&root, "pi/backups");
        assert_eq!(
            entries(&render_catalog(&root).unwrap()),
            vec![("aft/backups-pi".to_string(), "pi/backups".to_string())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_backup_folders_are_not_declared() {
        let (fixture, root) = storage_root();
        let elsewhere = fixture.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        mkdir(&root, "opencode");
        std::os::unix::fs::symlink(&elsewhere, root.join("opencode/backups")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("pi")).unwrap();
        assert!(entries(&render_catalog(&root).unwrap()).is_empty());
    }

    #[test]
    fn the_same_tree_renders_the_same_bytes() {
        let tree = [
            "runner/backups",
            "opencode/backups.parked-20260801",
            "opencode/backups",
            "fed--0123456789abcdef--89abcdef/backups",
            "opencode/backups.parked-20260710",
            "pi/backups",
        ];
        let (_first_fixture, first) = storage_root();
        for rel in tree {
            mkdir(&first, rel);
        }
        let (_second_fixture, second) = storage_root();
        for rel in tree.iter().rev() {
            mkdir(&second, rel);
        }
        let bytes = render_catalog(&first).unwrap();
        assert_eq!(bytes, render_catalog(&first).unwrap());
        assert_eq!(bytes, render_catalog(&second).unwrap());
        assert_engram_accepts(&first, &bytes);
        assert!(entries(&bytes).contains(&(
            "aft/backups-fed-0123456789abcdef-89abcdef".to_string(),
            "fed--0123456789abcdef--89abcdef/backups".to_string()
        )));
    }

    #[test]
    fn a_later_parked_folder_gets_a_dated_id_and_the_first_keeps_its_id() {
        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups.parked-20260710");
        let before = entries(&render_catalog(&root).unwrap());
        assert_eq!(
            before,
            vec![(
                "aft/backups-opencode-parked".to_string(),
                "opencode/backups.parked-20260710".to_string()
            )]
        );

        mkdir(&root, "opencode/backups.parked-20260901");
        let bytes = render_catalog(&root).unwrap();
        assert_engram_accepts(&root, &bytes);
        assert_eq!(
            entries(&bytes),
            vec![
                (
                    "aft/backups-opencode-parked".to_string(),
                    "opencode/backups.parked-20260710".to_string()
                ),
                (
                    "aft/backups-opencode-parked-20260901".to_string(),
                    "opencode/backups.parked-20260901".to_string()
                ),
            ]
        );
    }

    #[test]
    fn mcp_clients_with_the_same_readable_name_get_distinct_ids() {
        let (_fixture, root) = storage_root();
        for client in ["a/b", "a:b"] {
            let segment = Harness::Mcp {
                client: client.to_string(),
            }
            .storage_segment();
            mkdir(&root, &format!("{segment}/backups"));
        }
        let bytes = render_catalog(&root).unwrap();
        assert_engram_accepts(&root, &bytes);
        let ids = entries(&bytes)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "aft/backups-mcp-a-b");
        assert!(ids[1].starts_with("aft/backups-mcp-a-b-"), "{ids:?}");
    }

    #[test]
    fn an_unchanged_catalog_is_not_rewritten() {
        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups");
        assert_eq!(write_catalog(&root).unwrap(), WriteOutcome::Written);
        let target = root.join(CATALOG_FILENAME);
        let first = fs::read(&target).unwrap();
        assert_engram_accepts(&root, &first);

        // Age the file so a rewrite would be visible even on filesystems with
        // coarse timestamps.
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(write_catalog(&root).unwrap(), WriteOutcome::Unchanged);
        assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), old);
        assert_eq!(fs::read(&target).unwrap(), first);

        mkdir(&root, "pi/backups");
        assert_eq!(write_catalog(&root).unwrap(), WriteOutcome::Written);
        assert_ne!(fs::metadata(&target).unwrap().modified().unwrap(), old);
        assert_engram_accepts(&root, &fs::read(&target).unwrap());
    }

    /// The temporary file the next atomic write on this thread creates.
    fn capture_temp_path() -> std::rc::Rc<std::cell::RefCell<Option<PathBuf>>> {
        let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = std::rc::Rc::clone(&seen);
        crate::durability::BEFORE_TEMP_CREATE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path: &Path| {
                *slot.borrow_mut() = Some(path.to_path_buf());
            }));
        });
        seen
    }

    fn temp_files(root: &Path) -> Vec<String> {
        sorted_dir_names(root)
            .unwrap()
            .into_iter()
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn the_catalog_is_written_through_a_temp_file_that_is_then_gone() {
        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups");
        let temp = capture_temp_path();
        let _ = crate::durability::take();
        assert_eq!(write_catalog(&root).unwrap(), WriteOutcome::Written);

        let temp = temp.borrow().clone().expect("the write used a temp file");
        assert_eq!(
            temp.parent(),
            Some(root.as_path()),
            "temp sits beside the target"
        );
        assert!(!temp.exists(), "temp file is gone after the rename");
        assert!(temp_files(&root).is_empty());
        let target = root.join(CATALOG_FILENAME);
        assert!(crate::durability::take().iter().any(|(kind, path)| {
            *kind == crate::durability::EventKind::AtomicReplace && *path == target
        }));
        assert_eq!(fs::read(&target).unwrap(), render_catalog(&root).unwrap());
    }

    #[test]
    fn an_interrupted_write_leaves_no_partial_file_behind() {
        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups");
        // A non-empty directory where the catalog should go makes the final
        // rename fail after the temp file was fully written.
        let target = root.join(CATALOG_FILENAME);
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"x").unwrap();
        let temp = capture_temp_path();

        assert!(write_catalog(&root).is_err());
        let temp = temp
            .borrow()
            .clone()
            .expect("the write reached its temp file");
        assert!(!temp.exists(), "a failed write removes its temp file");
        assert!(temp_files(&root).is_empty());
        assert!(target.join("keep").exists(), "the target was not replaced");

        // Startup only logs the failure.
        write_catalog_at_startup(&root);
        assert!(temp_files(&root).is_empty());
    }

    #[test]
    fn a_missing_storage_root_is_an_error_not_a_panic() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join(MODULE_ID);
        assert!(write_catalog(&root).is_err());
        write_catalog_at_startup(&root);
        assert!(
            !root.exists(),
            "the catalog writer never creates the storage root"
        );
    }
}
