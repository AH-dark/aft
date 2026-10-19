//! Machine-parsed Git probes that keep "this is not a repository" separate from
//! failures whose meaning is uncertain (damaged metadata, broken refs, signals).
//!
//! Callers keep their own path decoding, canonicalization and error text; this
//! module only decides which failures may be treated as certain absence.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    /// Git exited normally and no repository marker is visible on disk.
    NotRepository,
    /// A verified repository whose current branch has no commit yet.
    NoCommit,
    /// Anything else, including damaged metadata and signal deaths.
    Other,
}

#[derive(Debug)]
pub(crate) enum ProbeError {
    Spawn(io::Error),
    Failed { kind: FailureKind, output: Output },
}

/// A Git command whose diagnostics are always in the C locale.
pub(crate) fn command() -> Command {
    let mut builder = crate::effective_path::new_command("git");
    // These probes classify Git's diagnostic text, so an inherited translation
    // must not change it. Environment overrides go before `build` resolves
    // the executable against the child PATH.
    builder
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("LANGUAGE", "C");
    builder.build()
}

pub(crate) fn toplevel(root: &Path) -> Result<Vec<u8>, ProbeError> {
    probe(root, &["rev-parse", "--show-toplevel"])
}

pub(crate) fn root_commits(root: &Path) -> Result<Vec<u8>, ProbeError> {
    probe(root, &["rev-list", "--max-parents=0", "HEAD"])
}

pub(crate) fn probe(root: &Path, args: &[&str]) -> Result<Vec<u8>, ProbeError> {
    let output = command()
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(ProbeError::Spawn)?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    classify_failure(root, args, output, || unborn_head(root))
}

fn classify_failure(
    root: &Path,
    args: &[&str],
    output: Output,
    verify_unborn: impl FnOnce() -> bool,
) -> Result<Vec<u8>, ProbeError> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Only Git's ordinary fatal exit (128) can certify absence or an unborn
    // branch. A process killed by a signal may have printed anything first.
    let ordinary_fatal_exit = output.status.code() == Some(128);
    let absence = stderr
        .starts_with("fatal: not a git repository (or any of the parent directories):")
        || stderr.starts_with("fatal: not a git repository (or any parent up to mount point");
    let kind = if ordinary_fatal_exit && absence && !repository_marker_or_uncertainty(root) {
        FailureKind::NotRepository
    } else if ordinary_fatal_exit
        && args == ["rev-list", "--max-parents=0", "HEAD"]
        && (stderr.starts_with("fatal: ambiguous argument 'HEAD':")
            || stderr.starts_with("fatal: bad revision 'HEAD'"))
        && verify_unborn()
    {
        FailureKind::NoCommit
    } else {
        FailureKind::Other
    };
    Err(ProbeError::Failed { kind, output })
}

fn repository_marker_or_uncertainty(root: &Path) -> bool {
    // Git also prints its ordinary absence message when a `.git` it would
    // normally accept is damaged (for example a corrupt HEAD). A marker that
    // looks like a repository, or one that cannot be checked, is therefore
    // uncertainty rather than absence.
    for ancestor in root.ancestors() {
        if git_marker_is_repository_candidate(&ancestor.join(".git")) {
            return true;
        }
        // A bare repository has no `.git` entry; recognise its layout instead.
        if ancestor.join("HEAD").exists()
            && ancestor.join("objects").exists()
            && ancestor.join("refs").exists()
        {
            return true;
        }
    }
    false
}

/// Whether a `.git` entry is something Git would treat as a repository.
///
/// Git only accepts a `.git` directory that holds a HEAD file; without one it
/// keeps searching the parent directories. A stub such as a `.git` holding
/// only `info/exclude` is therefore a plain folder, exactly as Git sees it.
/// A `.git` file (a worktree or submodule link) is a candidate because Git
/// follows it. Any entry whose state cannot be read counts as a candidate.
fn git_marker_is_repository_candidate(marker: &Path) -> bool {
    let absent = |error: &io::Error| {
        matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
        )
    };
    match std::fs::metadata(marker) {
        Ok(metadata) if metadata.is_dir() => match std::fs::symlink_metadata(marker.join("HEAD")) {
            Ok(_) => true,
            Err(error) => !absent(&error),
        },
        Ok(_) => true,
        Err(error) => !absent(&error),
    }
}

fn unborn_head(root: &Path) -> bool {
    // A valid repository without any commit may use the path-derived cache
    // identity. Before allowing that, prove the repository is readable, HEAD
    // names a branch, and that branch's ref really does not exist: a broken or
    // unreadable ref produces the same quiet missing-HEAD result.
    let Ok(repository) = command()
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-dir"])
        .output()
    else {
        return false;
    };
    if !repository.status.success() {
        return false;
    }
    let missing_head = command()
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .is_ok_and(|output| output.status.code() == Some(1) && output.stderr.is_empty());
    if !missing_head {
        return false;
    }
    let Ok(symbolic) = command()
        .arg("-C")
        .arg(root)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output()
    else {
        return false;
    };
    if !symbolic.status.success() || !symbolic.stderr.is_empty() {
        return false;
    }
    let Some(reference) = git_output_path(&symbolic.stdout) else {
        return false;
    };
    let Some(loose) = git_path(root, reference.as_os_str()) else {
        return false;
    };
    let Some(packed) = git_path(root, std::ffi::OsStr::new("packed-refs")) else {
        return false;
    };
    #[cfg(test)]
    BEFORE_LOOSE_REF_CHECK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook(&loose);
        }
    });
    // These filesystem checks run after the Git subprocesses above, so a ref
    // created (or broken) in between is still seen. Only actual nonexistence
    // of the loose ref proves an unborn branch.
    if !matches!(std::fs::symlink_metadata(loose), Err(error) if error.kind() == io::ErrorKind::NotFound)
    {
        return false;
    }
    match std::fs::File::open(packed) {
        Ok(_) => true,
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    }
}

/// Test fixture: a real repository whose HEAD file is corrupt, so Git rejects
/// it and prints its ordinary absence message even though a repository exists.
#[cfg(test)]
pub(crate) fn init_repository_with_corrupt_head_for_test(path: &Path) {
    assert!(command()
        .args(["init", "--quiet"])
        .arg(path)
        .status()
        .unwrap()
        .success());
    std::fs::write(path.join(".git").join("HEAD"), "not a ref\n").unwrap();
}

/// Test fixture: a `.git` folder holding only `info/exclude` and no HEAD, which
/// Git itself does not treat as a repository.
#[cfg(test)]
pub(crate) fn write_stub_git_directory_for_test(path: &Path) {
    let info = path.join(".git").join("info");
    std::fs::create_dir_all(&info).unwrap();
    std::fs::write(info.join("exclude"), "ignored/\n").unwrap();
}

#[cfg(test)]
type BeforeLooseRefCheck = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
thread_local! {
    /// Test hook run just before the loose-ref existence check in `unborn_head`.
    static BEFORE_LOOSE_REF_CHECK: std::cell::RefCell<Option<BeforeLooseRefCheck>> = const { std::cell::RefCell::new(None) };
}

fn git_path(root: &Path, name: &std::ffi::OsStr) -> Option<PathBuf> {
    let output = command()
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-path"])
        .arg(name)
        .output()
        .ok()?;
    if !output.status.success() || !output.stderr.is_empty() {
        return None;
    }
    let path = git_output_path(&output.stdout)?;
    Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

/// Decode one line of Git path output, keeping raw bytes on Unix.
fn git_output_path(bytes: &[u8]) -> Option<PathBuf> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    if bytes.is_empty() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure_kind(result: Result<Vec<u8>, ProbeError>) -> FailureKind {
        match result.unwrap_err() {
            ProbeError::Failed { kind, .. } => kind,
            ProbeError::Spawn(error) => panic!("Git must be runnable for probe fixtures: {error}"),
        }
    }

    fn init_repository(path: &Path) {
        assert!(command()
            .args(["init", "--quiet"])
            .arg(path)
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn machine_git_command_pins_its_diagnostic_locale() {
        let command = command();
        let env = command
            .get_envs()
            .collect::<std::collections::HashMap<_, _>>();
        for key in ["LC_ALL", "LANG", "LANGUAGE"] {
            assert_eq!(
                env.get(std::ffi::OsStr::new(key)).copied().flatten(),
                Some(std::ffi::OsStr::new("C")),
                "Git diagnostic locale override missing: {key}"
            );
        }
    }

    #[test]
    fn corrupt_git_metadata_is_not_repository_absence() {
        let dir = tempfile::tempdir().unwrap();
        crate::git_probe::init_repository_with_corrupt_head_for_test(dir.path());
        assert_eq!(failure_kind(toplevel(dir.path())), FailureKind::Other);
        assert_eq!(failure_kind(root_commits(dir.path())), FailureKind::Other);
    }

    /// Git does not treat a `.git` folder without HEAD as a repository, so such
    /// a stub stays a determinate "not a repository".
    #[test]
    fn stub_git_directory_without_head_is_repository_absence() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_git_directory_for_test(dir.path());
        assert_eq!(
            failure_kind(toplevel(dir.path())),
            FailureKind::NotRepository
        );
        assert_eq!(
            failure_kind(root_commits(dir.path())),
            FailureKind::NotRepository
        );
    }

    #[test]
    fn broken_head_reference_is_not_an_unborn_repository() {
        let dir = tempfile::tempdir().unwrap();
        init_repository(dir.path());
        let head = std::fs::read_to_string(dir.path().join(".git/HEAD")).unwrap();
        let reference = head.trim().strip_prefix("ref: ").unwrap();
        std::fs::write(
            dir.path().join(".git").join(reference),
            "not an object id\n",
        )
        .unwrap();
        assert_eq!(failure_kind(root_commits(dir.path())), FailureKind::Other);
    }

    #[test]
    fn ref_appearing_after_head_verification_is_not_unborn() {
        let dir = tempfile::tempdir().unwrap();
        init_repository(dir.path());
        let reached = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed = reached.clone();
        BEFORE_LOOSE_REF_CHECK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |reference| {
                std::fs::write(reference, "not an object id\n").unwrap();
                observed.set(true);
            }));
        });
        assert_eq!(failure_kind(root_commits(dir.path())), FailureKind::Other);
        assert!(
            reached.get(),
            "real Git missing-HEAD verification must reach the filesystem check"
        );
    }

    #[test]
    fn empty_directory_is_repository_absence_but_unborn_repo_has_no_commit() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            failure_kind(toplevel(dir.path())),
            FailureKind::NotRepository
        );
        init_repository(dir.path());
        assert!(toplevel(dir.path()).is_ok());
        assert_eq!(
            failure_kind(root_commits(dir.path())),
            FailureKind::NoCommit
        );
    }

    #[cfg(unix)]
    #[test]
    fn ambiguous_head_diagnostic_requires_verified_unborn_repository() {
        use std::os::unix::process::ExitStatusExt;
        let dir = tempfile::tempdir().unwrap();
        let output = Output {
            status: std::process::ExitStatus::from_raw(128 << 8),
            stdout: Vec::new(),
            stderr: b"fatal: ambiguous argument 'HEAD': unknown revision or path not in the working tree.\n".to_vec(),
        };
        assert_eq!(
            failure_kind(classify_failure(
                dir.path(),
                &["rev-list", "--max-parents=0", "HEAD"],
                output,
                || false,
            )),
            FailureKind::Other
        );
    }

    #[cfg(unix)]
    #[test]
    fn signalled_probe_is_not_absence_even_with_absence_diagnostic() {
        use std::os::unix::process::ExitStatusExt;
        let dir = tempfile::tempdir().unwrap();
        let output = Output {
            // Raw wait status 9: terminated by SIGKILL, no exit code.
            status: std::process::ExitStatus::from_raw(9),
            stdout: Vec::new(),
            stderr: b"fatal: not a git repository (or any of the parent directories): .git\n"
                .to_vec(),
        };
        assert_eq!(
            failure_kind(classify_failure(dir.path(), &[], output, || false)),
            FailureKind::Other
        );
    }
}
