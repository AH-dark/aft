//! Open scratch content without letting a replaced symlink widen a read grant.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub(crate) fn is_scratch(path: &Path) -> bool {
    path.ancestors()
        .any(crate::sandbox_spawn::is_managed_task_temp_dir)
}

pub(crate) fn check_path(path: &Path) -> io::Result<()> {
    if is_scratch(path) && crate::project_content::is_sqlite_or_metadata(path) {
        return Err(refused());
    }
    Ok(())
}

pub(crate) fn open(path: &Path) -> io::Result<File> {
    let Some(root) = path
        .ancestors()
        .find(|path| crate::sandbox_spawn::is_managed_task_temp_dir(path))
    else {
        return File::open(path);
    };
    // Sandbox task-temp directory names select stricter open checks so links
    // cannot redirect scratch reads elsewhere. A matching name never grants
    // access; the caller still needs authorization from the task registry.
    if crate::project_content::is_sqlite_or_metadata(path) {
        return Err(refused());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    validate_opened(path, root, &file)?;
    Ok(file)
}

pub(crate) fn validate_opened(path: &Path, root: &Path, file: &File) -> io::Result<()> {
    let opened = crate::inspect::job::normalize_path(
        &crate::commands::file_read_exact::opened_location(file)?,
    );
    let root = crate::inspect::job::normalize_path(root);
    // Judge the descriptor, not a second canonicalization of the input: a
    // parent directory may have become a symlink after permission was checked.
    if !opened.starts_with(&root)
        || opened != crate::inspect::job::normalize_path(path)
        || !file.metadata()?.is_file()
    {
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

pub(crate) fn check_opened(path: &Path, file: &File) -> io::Result<()> {
    if let Some(root) = path
        .ancestors()
        .find(|path| crate::sandbox_spawn::is_managed_task_temp_dir(path))
    {
        validate_opened(path, root, file)?;
    }
    Ok(())
}

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "scratch read escaped its task directory or named unsafe content",
    )
}
