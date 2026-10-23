//! Hash current checkout bytes and attach their trigrams to `snapshot::LiveDelta`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::blob_store::v2::{ContentHash, FamilyPlane, TrigramPolicy};

use super::segment_store::TrigramPayload;
use super::snapshot::{DiskState, LiveDelta, LiveEntry, WatcherState};
use super::RelPath;

/// The hash and payload are computed from the same read; policy identity makes
/// a stale attachment ineligible for pruning after configuration changes.
#[derive(Clone, Debug)]
pub struct Attachment {
    pub content: ContentHash,
    pub policy: [u8; 32],
    pub payload: TrigramPayload,
}

pub fn entry(bytes: &[u8], policy: &TrigramPolicy, seq: u64) -> LiveEntry {
    let mut entry = LiveEntry::new(DiskState::of_bytes(bytes), seq);
    entry.attachments.insert(
        FamilyPlane::Trigram,
        Arc::new(Attachment {
            content: ContentHash::of(bytes),
            policy: policy.fingerprint(),
            payload: TrigramPayload::extract(bytes, policy),
        }),
    );
    entry
}

/// File names whose content decides walker membership.
pub const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".aftignore", ".ignore"];

/// The disk state of the root ignore file `name`: its bytes, or absent when
/// it cannot be read as a file.
pub fn root_ignore_file_state(root: &Path, name: &str) -> DiskState {
    match std::fs::read(root.join(name)) {
        Ok(bytes) => DiskState::of_bytes(&bytes),
        Err(_) => DiskState::Absent,
    }
}

/// The disk state of every root ignore file, present or absent. A walk
/// records it so a later query can tell whether the rules it ran under still
/// hold, including for an ignore file that ignores itself and so is never a
/// walked member.
pub fn root_ignore_files(root: &Path) -> super::snapshot::RootIgnoreFiles {
    IGNORE_FILE_NAMES
        .iter()
        .filter_map(|name| {
            RelPath::new(name.as_bytes().to_vec())
                .ok()
                .map(|path| (path, root_ignore_file_state(root, name)))
        })
        .collect()
}

/// A partial walk retains its named gaps. It is never promoted to healthy.
#[derive(Debug, Default)]
pub struct Walk {
    pub entries: BTreeMap<RelPath, LiveEntry>,
    pub gaps: Vec<PathBuf>,
}

pub fn strict_walk(root: &Path, policy: &TrigramPolicy, seq: u64) -> Walk {
    let mut walk = Walk::default();
    // Reuse SearchIndex's walker (git ignores, .aftignore and excluded build
    // directories), but retain every traversal error as a coverage gap.
    for result in crate::search_index::project_walk_builder(root).build() {
        let item = match result {
            Ok(item) => item,
            Err(error) => {
                record_walk_error(&mut walk, &error, root);
                continue;
            }
        };
        if let Some(error) = item.error() {
            record_walk_error(&mut walk, error, item.path());
        }
        if !item.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = item.path();
        let relative = path
            .strip_prefix(root)
            .ok()
            .and_then(|path| RelPath::from_os_path(path).ok());
        match (relative, crate::project_content::read(path)) {
            (Some(relative), Ok(bytes)) => {
                walk.entries.insert(relative, entry(&bytes, policy, seq));
            }
            _ => walk.gaps.push(path.to_path_buf()),
        }
    }
    walk.gaps.sort();
    walk.gaps.dedup();
    walk
}

fn record_walk_error(walk: &mut Walk, error: &ignore::Error, fallback: &Path) {
    match error {
        ignore::Error::WithPath { path, err } => record_walk_error(walk, err, path),
        ignore::Error::WithLineNumber { err, .. } | ignore::Error::WithDepth { err, .. } => {
            record_walk_error(walk, err, fallback);
        }
        ignore::Error::Partial(errors) if !errors.is_empty() => {
            for error in errors {
                record_walk_error(walk, error, fallback);
            }
        }
        ignore::Error::Loop { child, .. } => walk.gaps.push(child.clone()),
        _ => walk.gaps.push(fallback.to_path_buf()),
    }
}

/// Initial checkout loading, branch switches, lost watcher events and ignore
/// changes all require this full content walk.
/// No stat cache participates, even if size and mtime have not moved.
pub fn reconcile(delta: &mut LiveDelta, root: &Path, policy: &TrigramPolicy) -> Walk {
    delta.set_watcher(WatcherState::Reconciling);
    let seq = delta.bump_epoch();
    // Read before the walk: an edit landing between the two then shows up as
    // a difference from this record, never as a record of rules the walk did
    // not apply.
    let ignore_files = root_ignore_files(root);
    let walk = strict_walk(root, policy, seq);
    if !walk.gaps.is_empty() || super::intent::active(root) {
        return walk;
    }
    delta.record_root_ignore_files(Some(ignore_files));
    let snapshot = delta.snapshot();
    let paths = snapshot
        .membership()
        .into_keys()
        .chain(snapshot.pending_intent().cloned())
        .chain(walk.entries.keys().cloned())
        .collect::<BTreeSet<_>>();
    for path in paths {
        let observed = walk
            .entries
            .get(&path)
            .cloned()
            .unwrap_or_else(|| LiveEntry::new(DiskState::Absent, seq));
        delta.apply(path.clone(), observed);
        delta.resolve_intent(&path, snapshot.intent_version());
    }
    delta.set_watcher(WatcherState::Healthy);
    walk
}

/// A filesystem watcher event always causes a content read, even with unchanged
/// metadata. For a rename the runtime calls this for both old and new paths.
pub fn apply_event(
    delta: &mut LiveDelta,
    root: &Path,
    path: &RelPath,
    policy: &TrigramPolicy,
) -> std::io::Result<()> {
    let absolute = root.join(
        super::segment_store::rel_path_to_os(path)
            .map_err(|e| std::io::Error::other(e.to_string()))?,
    );
    if crate::project_content::is_excluded(&absolute) {
        return Ok(());
    }
    let snapshot = delta.snapshot();
    let observed = match crate::project_content::read(absolute) {
        Ok(bytes) => entry(&bytes, policy, delta.version() + 1),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            LiveEntry::new(DiskState::Absent, delta.version() + 1)
        }
        Err(error) => {
            delta.bump_epoch();
            delta.set_watcher(WatcherState::Overflowed);
            return Err(error);
        }
    };
    delta.apply(path.clone(), observed);
    if !super::intent::active(root) {
        delta.resolve_intent(path, snapshot.intent_version());
    }
    // Membership can change along with bytes (including nested ignore rules).
    // Watcher events do not prove whether paths pass current ignore rules,
    // so recalculate that membership before allowing index pruning.
    let walk = reconcile(delta, root, policy);
    if !walk.gaps.is_empty() {
        return Err(std::io::Error::other(format!(
            "watcher reconcile gaps: {:?}",
            walk.gaps
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walker_error_keeps_the_named_culprit_instead_of_the_root() {
        let root = tempfile::tempdir().unwrap();
        let culprit = root.path().join("nested/.gitignore");
        let error = ignore::Error::WithPath {
            path: culprit.clone(),
            err: Box::new(ignore::Error::Io(std::io::Error::other("unreadable rules"))),
        };
        let mut walk = Walk::default();
        record_walk_error(&mut walk, &error, root.path());
        assert_eq!(walk.gaps, vec![culprit]);
    }

    #[cfg(unix)]
    #[test]
    fn partial_walker_errors_keep_raw_paths_distinct_from_lossy_aliases() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let raw = root
            .path()
            .join(std::ffi::OsString::from_vec(b"rules-\xff".to_vec()));
        let replacement = root.path().join("rules-\u{fffd}");
        let named = |path: PathBuf| ignore::Error::WithPath {
            path,
            err: Box::new(ignore::Error::Io(std::io::Error::other("unreadable"))),
        };
        let error = ignore::Error::Partial(vec![named(raw.clone()), named(replacement.clone())]);
        let mut walk = Walk::default();
        record_walk_error(&mut walk, &error, root.path());
        assert_eq!(walk.gaps, vec![raw, replacement]);
        assert_ne!(walk.gaps[0], walk.gaps[1]);
    }

    #[test]
    fn unnamed_walker_errors_keep_a_conservative_root_gap() {
        let root = tempfile::tempdir().unwrap();
        let mut walk = Walk::default();
        record_walk_error(
            &mut walk,
            &ignore::Error::Io(std::io::Error::other("unknown traversal failure")),
            root.path(),
        );
        assert_eq!(walk.gaps, vec![root.path().to_path_buf()]);
    }
}
