use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! {
    static ROOTS_FOR_TEST: std::cell::RefCell<Option<Vec<PathBuf>>> =
        const { std::cell::RefCell::new(None) };
}

/// Restores the previous temp-root override when dropped.
#[cfg(test)]
pub(crate) struct RootsOverride(Option<Vec<PathBuf>>);

#[cfg(test)]
impl Drop for RootsOverride {
    fn drop(&mut self) {
        let previous = self.0.take();
        ROOTS_FOR_TEST.with(|roots| *roots.borrow_mut() = previous);
    }
}

/// Make this thread's access policies treat `roots` as the only system temp
/// directories, so a test can exercise the temp guards on a directory it owns
/// instead of the host's real `/tmp`.
#[cfg(test)]
pub(crate) fn override_roots_for_test(roots: Vec<PathBuf>) -> RootsOverride {
    RootsOverride(ROOTS_FOR_TEST.with(|current| current.borrow_mut().replace(roots)))
}

/// The temp-root override installed on the calling thread, if any. Read it on
/// the request thread: policy construction may run on the bounded IO helper.
pub(crate) fn roots_override() -> Option<Vec<PathBuf>> {
    #[cfg(test)]
    {
        ROOTS_FOR_TEST.with(|roots| roots.borrow().clone())
    }
    #[cfg(not(test))]
    {
        None
    }
}

/// Every system temp directory the standalone temp guards cover.
pub(crate) fn guard_roots(overridden: Option<&[PathBuf]>) -> Vec<PathBuf> {
    match overridden {
        Some(roots) => roots.iter().map(|root| super::policy_path(root)).collect(),
        None => crate::bash_permissions::system_temp_roots()
            .iter()
            .map(|path| super::policy_path(path))
            .collect(),
    }
}

/// The temp directories a standalone call may use without an outside ask.
pub(crate) fn allowed_roots(overridden: Option<&[PathBuf]>) -> Vec<PathBuf> {
    match overridden {
        Some(roots) => roots.iter().map(|root| super::policy_path(root)).collect(),
        None => roots(),
    }
}

pub(crate) fn roots() -> Vec<PathBuf> {
    let own = if cfg!(target_os = "macos") {
        std::env::var_os("TMPDIR").map(PathBuf::from)
    } else {
        Some(std::env::temp_dir())
    };
    roots_for(own, cfg!(target_os = "macos"))
}

pub(crate) fn roots_for(own: Option<PathBuf>, macos: bool) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.extend(own);
    #[cfg(unix)]
    if !macos {
        roots.extend(["/tmp", "/var/tmp", "/private/tmp", "/private/var/tmp"].map(PathBuf::from));
    }
    #[cfg(not(unix))]
    let _ = macos;
    roots
        .into_iter()
        .filter(|root| root.is_absolute() && root.parent().is_some())
        .map(|root| super::policy_path(&root))
        .collect()
}

pub(crate) fn managed_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.push(std::env::temp_dir().join("aft-w"));
    #[cfg(unix)]
    roots.extend(
        [
            "/tmp/aft-w",
            "/private/tmp/aft-w",
            "/var/tmp/aft-w",
            "/private/var/tmp/aft-w",
        ]
        .map(PathBuf::from),
    );
    roots
        .into_iter()
        .map(|root| super::policy_path(&root))
        .collect()
}

/// Metadata-only checks run before a temp allowance, never after opening data.
pub(crate) fn safe_file(path: &Path) -> bool {
    let sqlite_name = path.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .to_ascii_lowercase()
            .contains(".sqlite")
    });
    if sqlite_name || crate::project_content::is_sqlite_or_metadata(path) {
        return false;
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
    };
    if metadata.is_dir() || metadata.file_type().is_symlink() {
        return true;
    }
    if !metadata.is_file() {
        return false;
    }
    single_link(path, &metadata)
}

#[cfg(unix)]
fn single_link(_path: &Path, metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() == 1
}

#[cfg(windows)]
fn single_link(path: &Path, _metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
    else {
        return false;
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    unsafe {
        GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) != 0
            && info.nNumberOfLinks == 1
    }
}

#[cfg(not(any(unix, windows)))]
fn single_link(_path: &Path, _metadata: &std::fs::Metadata) -> bool {
    false
}

pub(crate) fn safe_tree(path: &Path, managed_roots: &[PathBuf], owned: bool) -> bool {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return safe_file(path);
    }
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        if !owned
            && (managed_roots.iter().any(|root| path.starts_with(root))
                || crate::task_temp_reads::is_scratch(&path))
        {
            return false;
        }
        if !safe_file(&path) {
            return false;
        }
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            let Ok(entries) = std::fs::read_dir(&path) else {
                return false;
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    return false;
                };
                pending.push(entry.path());
            }
        }
    }
    true
}
