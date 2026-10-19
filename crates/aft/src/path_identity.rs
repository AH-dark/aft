//! Project-root identity primitives for per-checkout routing and state.
//!
//! Three distinct identity roles live here or are named here — do NOT conflate
//! them, because conflating them is the bug class this module exists to kill:
//!
//! 1. [`ProjectRootId`] (re-exported from `cortexkit-paths`) — the canonical
//!    *path* identity for ROUTING and per-root SCOPING: bridge routing, RPC
//!    port files, the (future) ProjectActor map and lease, and the
//!    bash/compression/backup/checkpoint scope keys derived via
//!    [`project_scope_key`]. Shared byte-for-byte with subc by construction
//!    (same crate), so a session attaches to the same root subc routed it to.
//!
//! 2. Repository artifact identity — use one shared cache key for data that can
//!    be reused across all worktrees of the same Git repository, and a separate
//!    key for checkout-specific inspect data. For Git repositories, derive the
//!    shared key from the repository's root commit so every worktree can reuse
//!    the same search cache; for non-Git projects, derive it from the canonical
//!    filesystem path. Keep the previous `project_cache_key` format for
//!    compatibility with existing caches.
//!
//! 3. Operation-target / lexical path handling (create-file fallback, relative
//!    path joins) — not identity; stays local to its call sites.
//!
//! Consequence of the split: two worktrees of one repo SHARE the artifact key
//! (same root commit → shared index) but get DISTINCT `ProjectRootId`s → a
//! distinct [`project_scope_key`]. A background bash task, undo history, or
//! token-savings stat created in worktree A never surfaces under worktree B.

use std::path::{Path, PathBuf};

pub use cortexkit_paths::{IdentityError, ProjectRootId};

/// Stable 16-hex scope key for per-root mutable state keyed by *canonical path*:
/// background bash tasks, compression aggregation, backup metadata, checkpoint
/// locks.
///
/// Derived from [`ProjectRootId`] (the shared canonical path), so it is the
/// per-checkout identity — distinct from `artifact_cache_key`, which is the
/// per-repository identity (root commit). Two worktrees of one repo get
/// different scope keys (correct) but the same artifact key (shared index).
///
/// Non-existent roots fall back to a lexical normalization so derivation is
/// total: the shared crate rejects non-existent paths, and while runtime
/// project roots always exist, `current_dir`-derived callers (e.g. a default
/// checkpoint store) and tests may pass a path that does not.
pub fn project_scope_key(project_root: &Path) -> String {
    let canonical = ProjectRootId::from_path(project_root)
        .map(ProjectRootId::into_path_buf)
        .unwrap_or_else(|_| lexical_normalize(project_root));
    hash16(&canonical)
}

/// Inputs are canonical policy identities, including home aliases and native
/// volume roots. Exclude root-level system directories and all home ancestors.
pub(crate) fn directory_scope_is_narrow(directory: &Path, home: &Path, volume: &Path) -> bool {
    project_root_path_is_narrow(directory, home, volume)
        && directory.parent().and_then(Path::parent).is_some()
        && directory.parent() != Some(volume)
}

/// Decide whether a canonical project root admits watching and indexing.
/// A volume's direct children can be ordinary projects, unlike permission-card
/// directory scopes, which also exclude root-level containers.
/// Unknown protected-directory identities must not admit background work.
pub(crate) fn project_root_is_narrow(directory: &Path, home: &Path, volume: &Path) -> Option<bool> {
    if !project_root_path_is_narrow(directory, home, volume) {
        return Some(false);
    }
    protected_directory_alias(directory, home, volume).map(|protected| !protected)
}

fn project_root_path_is_narrow(directory: &Path, home: &Path, volume: &Path) -> bool {
    directory.is_absolute()
        && directory.parent().is_some()
        && !home.starts_with(directory)
        && directory != volume
}

/// Bind mounts can preserve a protected directory's identity without preserving
/// its path spelling. Device/inode checks also catch same-device volume aliases.
#[cfg(unix)]
pub(crate) fn directory_is_protected_alias(
    directory: &Path,
    home: &Path,
    volume: &Path,
) -> Option<bool> {
    if protected_directory_alias(directory, home, volume)? {
        return Some(true);
    }
    Some(directory_identity(directory.parent()?)? == directory_identity(volume)?)
}

#[cfg(unix)]
fn directory_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn protected_directory_alias(directory: &Path, home: &Path, volume: &Path) -> Option<bool> {
    let candidate = directory_identity(directory)?;
    for ancestor in home.ancestors().chain(std::iter::once(volume)) {
        if directory_identity(ancestor)? == candidate {
            return Some(true);
        }
    }
    Some(false)
}

#[cfg(not(unix))]
fn protected_directory_alias(_directory: &Path, _home: &Path, _volume: &Path) -> Option<bool> {
    Some(false)
}

#[cfg(not(unix))]
pub(crate) fn directory_is_protected_alias(
    _directory: &Path,
    _home: &Path,
    _volume: &Path,
) -> Option<bool> {
    Some(false)
}

fn hash16(path: &Path) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..16].to_string()
}

/// Resolve lexical dot components without touching the filesystem. Leading
/// relative parents remain significant; rooted paths cannot escape their root.
/// This is not containment validation, and comparison callers apply any
/// Windows verbatim-prefix policy before passing their path here.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => match result.components().next_back() {
                Some(Component::Normal(_)) => {
                    result.pop();
                }
                Some(Component::RootDir) => {}
                Some(Component::Prefix(_)) if path.has_root() => {}
                _ => result.push(component),
            },
            Component::CurDir => {}
            other => result.push(other),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn project_root_volume_children_are_not_permission_scopes() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        for name in ["app", "workspace", "src", "code"] {
            let project = volume.join(name);
            std::fs::create_dir(&project).unwrap();
            assert_eq!(
                super::project_root_is_narrow(&project, &home, &volume),
                Some(true),
                "a project directly below a volume must stay enabled: {name}"
            );
            assert!(
                !super::directory_scope_is_narrow(&project, &home, &volume),
                "permission-card scopes must remain stricter than project roots"
            );
            #[cfg(unix)]
            assert_eq!(
                super::directory_is_protected_alias(&project, &home, &volume),
                Some(true)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn project_root_paths_allow_container_roots_but_not_protected_roots() {
        let home = std::path::Path::new("/home/person");
        let volume = std::path::Path::new("/");
        for project in ["/app", "/workspace", "/src", "/code"] {
            assert!(super::project_root_path_is_narrow(
                std::path::Path::new(project),
                home,
                volume
            ));
        }
        for protected in ["/", "/home", "/home/person"] {
            assert!(!super::project_root_path_is_narrow(
                std::path::Path::new(protected),
                home,
                volume
            ));
        }
        let mounted_volume = std::path::Path::new("/Volumes/Work");
        assert!(!super::project_root_path_is_narrow(
            mounted_volume,
            home,
            mounted_volume
        ));
        assert!(super::project_root_path_is_narrow(
            std::path::Path::new("/Volumes/Work/app"),
            home,
            mounted_volume,
        ));
    }

    #[cfg(windows)]
    #[test]
    fn project_root_paths_allow_drive_children_but_not_protected_roots() {
        let home = std::path::Path::new(r"C:\Users\person");
        let volume = std::path::Path::new(r"C:\");
        for project in [r"C:\app", r"C:\workspace", r"C:\src", r"C:\code"] {
            assert!(super::project_root_path_is_narrow(
                std::path::Path::new(project),
                home,
                volume
            ));
        }
        for protected in [volume, home, home.parent().unwrap()] {
            assert!(!super::project_root_path_is_narrow(protected, home, volume));
        }
    }

    #[test]
    fn project_root_protected_directories_remain_disabled() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        for root in [
            &volume,
            &home,
            home.parent().unwrap(),
            volume.ancestors().last().unwrap(),
        ] {
            assert_eq!(
                super::project_root_is_narrow(root, &home, &volume),
                Some(false)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn project_root_unknown_identity_is_not_admitted() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        let missing = volume.join("missing/project");
        assert_eq!(
            super::project_root_is_narrow(&missing, &home, &volume),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_root_protected_aliases_remain_disabled() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        // Following symlinks gives the same inode identities as protected
        // directories without requiring privileged bind mounts in the test.
        for (name, target) in [
            ("app", home.as_path()),
            ("workspace", home.parent().unwrap()),
            ("code", volume.as_path()),
        ] {
            let alias = volume.join(name);
            std::os::unix::fs::symlink(target, &alias).unwrap();
            assert!(super::project_root_path_is_narrow(&alias, &home, &volume));
            assert_eq!(
                super::project_root_is_narrow(&alias, &home, &volume),
                Some(false)
            );
        }
    }

    use super::*;
    use std::fs;

    #[test]
    fn lexical_normalization_preserves_parents_and_root_boundaries() {
        for (input, expected) in [
            ("../../x", "../../x"),
            ("../a/../../x", "../../x"),
            ("a/../b", "b"),
            ("a/../../x", "../x"),
        ] {
            assert_eq!(lexical_normalize(Path::new(input)), Path::new(expected));
        }
        #[cfg(unix)]
        assert_eq!(lexical_normalize(Path::new("/../../x")), Path::new("/x"));
    }

    #[cfg(windows)]
    #[test]
    fn lexical_normalization_preserves_drive_relative_parents() {
        assert_eq!(
            lexical_normalize(Path::new(r"C:..\..\x")),
            Path::new(r"C:..\..\x")
        );
        assert_eq!(
            lexical_normalize(Path::new(r"C:\..\..\x")),
            Path::new(r"C:\x")
        );
    }

    /// A fresh root directory, removed when the returned guard drops.
    fn temp_root(label: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("aft-path-identity-{label}-"))
            .tempdir()
            .expect("create temp root");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn scope_key_is_stable_and_16_hex() {
        let (_root_dir, root) = temp_root("stable");
        let a = project_scope_key(&root);
        let b = project_scope_key(&root);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn scope_key_distinguishes_distinct_roots() {
        let (_a_dir, a) = temp_root("root-a");
        let (_b_dir, b) = temp_root("root-b");
        assert_ne!(project_scope_key(&a), project_scope_key(&b));
    }

    #[test]
    fn scope_key_canonicalizes_equivalent_spellings() {
        let (_root_dir, root) = temp_root("spelling");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("create nested");
        // `root/nested/..` and `root/.` both canonicalize to `root`.
        assert_eq!(
            project_scope_key(&root),
            project_scope_key(&nested.join("..")),
        );
        assert_eq!(project_scope_key(&root), project_scope_key(&root.join(".")));
    }

    #[test]
    fn scope_key_total_on_non_existent_root() {
        // Non-existent path: lexical fallback, no panic, stable.
        let missing = std::env::temp_dir().join("aft-path-identity-definitely-missing-xyz/sub/..");
        let key = project_scope_key(&missing);
        assert_eq!(key.len(), 16);
    }

    /// The headline P0 invariant: a linked git worktree SHARES the main
    /// checkout's artifact cache key (same root commit → shared index, opened
    /// read-only) but gets a DISTINCT scope key (its own checkout path → its own
    /// bash/compression/backup/checkpoint namespace). Before the split, both
    /// were the same root-commit key, so worktree A's tasks/undo bled into B.
    #[test]
    fn worktree_shares_artifact_key_but_has_distinct_scope_key() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        use std::process::Command;

        let mut git_version = Command::new("git");
        let git_ok = crate::test_env::apply_hermetic_git_env(&mut git_version)
            .arg("--version")
            .output()
            .is_ok();
        if !git_ok {
            eprintln!("skipping: git not available");
            return;
        }

        let tmp = std::env::temp_dir().join(format!(
            "aft-worktree-iso-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let main = tmp.join("main");
        fs::create_dir_all(&main).expect("create main checkout");

        let run = |args: &[&str], cwd: &std::path::Path| {
            let mut command = Command::new("git");
            assert!(
                crate::test_env::apply_hermetic_git_env(command.current_dir(cwd))
                    .args(args)
                    .status()
                    .expect("run git")
                    .success(),
                "git {args:?} failed"
            );
        };
        run(&["init"], &main);
        fs::write(main.join("f.txt"), "x\n").expect("write file");
        run(&["add", "."], &main);
        run(
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e.x",
                "commit",
                "-m",
                "init",
            ],
            &main,
        );

        let worktree = tmp.join("wt");
        run(
            &[
                "worktree",
                "add",
                worktree.to_str().unwrap(),
                "-b",
                "feature",
            ],
            &main,
        );

        let main_artifact = crate::search_index::artifact_cache_key(&main);
        let wt_artifact = crate::search_index::artifact_cache_key(&worktree);
        let main_scope = project_scope_key(&main);
        let wt_scope = project_scope_key(&worktree);

        // Same repo (same root commit) → SHARED artifact key (worktree reuses
        // main's on-disk index, read-only).
        assert_eq!(
            main_artifact, wt_artifact,
            "worktree must share the main checkout's artifact cache key"
        );
        // Distinct checkout path → DISTINCT scope key (no bash/undo/stats bleed).
        assert_ne!(
            main_scope, wt_scope,
            "worktree must get its own per-checkout scope key"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
