//! Open scratch content without letting a replaced symlink widen a read grant.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub(crate) struct OpenCheck {
    root: PathBuf,
    expected: PathBuf,
}

pub(crate) fn is_scratch(path: &Path) -> bool {
    path.ancestors()
        .any(crate::sandbox_spawn::is_managed_task_temp_dir)
}

pub(crate) fn check_path(path: &Path) -> io::Result<Option<OpenCheck>> {
    let Some(root) = path
        .ancestors()
        .find(|path| crate::sandbox_spawn::is_managed_task_temp_dir(path))
    else {
        return Ok(None);
    };
    if crate::project_content::is_sqlite_or_metadata(path) {
        return Err(refused());
    }
    // Both open callers resolve relative paths against the process cwd. Make
    // that base explicit without following links below the task-temp root.
    let absolute_path = std::path::absolute(path)?;
    let absolute_root = std::path::absolute(root)?;
    let remainder = absolute_path
        .strip_prefix(&absolute_root)
        .map_err(|_| refused())?;
    // The root is the directory named by authorization, so resolving its
    // prefix accepts aliases such as /var on macOS. Snapshot it before open;
    // resolving descendants would accept a parent replaced by a symlink after
    // authorization instead of detecting the changed location of the handle.
    let root = crate::inspect::job::normalize_path(&std::fs::canonicalize(absolute_root)?);
    let expected = crate::inspect::job::normalize_path(&root.join(remainder));
    Ok(Some(OpenCheck { root, expected }))
}

pub(crate) fn open(path: &Path) -> io::Result<File> {
    let Some(check) = check_path(path)? else {
        return File::open(path);
    };
    // Sandbox task-temp directory names select stricter open checks so links
    // cannot redirect scratch reads elsewhere. A matching name never grants
    // access; the caller still needs authorization from the task registry.
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    validate_opened(path, &check, &file)?;
    Ok(file)
}

fn validate_opened(path: &Path, check: &OpenCheck, file: &File) -> io::Result<()> {
    let opened = crate::inspect::job::normalize_path(
        &crate::commands::file_read_exact::opened_location(file)?,
    );
    // Judge the descriptor, not a second canonicalization of the input: a
    // parent directory may have become a symlink after permission was checked.
    if !opened.starts_with(&check.root) || opened != check.expected || !file.metadata()?.is_file() {
        return Err(refused());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file.metadata()?.nlink() != 1 {
            return Err(refused());
        }
    }
    if crate::project_content::is_sqlite_or_metadata(&opened)
        || crate::project_content::is_sqlite_or_metadata(path)
    {
        return Err(refused());
    }
    Ok(())
}

pub(crate) fn check_opened(path: &Path, check: Option<&OpenCheck>, file: &File) -> io::Result<()> {
    if let Some(check) = check {
        validate_opened(path, check, file)?;
    }
    Ok(())
}

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "scratch read escaped its task directory or named unsafe content",
    )
}
