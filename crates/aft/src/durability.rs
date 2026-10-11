//! Durable file commits use the platform's full sync, including F_FULLFSYNC on
//! macOS. Rebuildable and process-lifetime stores deliberately do not call these
//! sync helpers. Atomic writes select their durability explicitly. The test
//! ledger observes the real calls, never substitutes for them.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

// Some binaries do not use every atomic-write durability option. Callers choose
// whether private state needs no sync, file sync, or file plus directory sync.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub(crate) enum AtomicWriteDurability {
    None,
    // Callers publishing several files sync the directory once after the batch.
    File,
    FileAndDirectory,
}

/// Publish private state without leaving a partial destination or a failed temp.
/// Authorization and directory creation remain the caller's responsibility.
pub(crate) fn atomic_write(
    path: &Path,
    bytes: &[u8],
    durability: AtomicWriteDurability,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let temporary = parent.join(format!(".aft-{:032x}.tmp", u128::from_ne_bytes(random)));
    #[cfg(test)]
    before_temp_create(&temporary);
    let file = crate::private_storage::options()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let mut cleanup = TempCleanup(Some(temporary.clone()));
    // Close before replacing so Windows does not retain an open temp handle.
    (move || -> io::Result<()> {
        let mut file = file;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file.metadata()?.permissions().mode() & 0o777 != crate::private_storage::FILE_MODE {
                file.set_permissions(fs::Permissions::from_mode(
                    crate::private_storage::FILE_MODE,
                ))?;
            }
        }
        file.write_all(bytes)?;
        if !matches!(durability, AtomicWriteDurability::None) {
            sync_file(&file, path)?;
        }
        Ok(())
    })()?;
    crate::fs_lock::rename_over(&temporary, path)?;
    cleanup.0 = None;
    record(EventKind::AtomicReplace, path);
    if matches!(durability, AtomicWriteDurability::FileAndDirectory) {
        sync_dir(parent)?;
    }
    Ok(())
}

struct TempCleanup(Option<PathBuf>);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
type BeforeTempCreate = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_TEMP_CREATE: std::cell::RefCell<Option<BeforeTempCreate>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn before_temp_create(path: &Path) {
    let hook = BEFORE_TEMP_CREATE.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(path);
    }
}

pub(crate) fn sync_file(file: &File, path: &Path) -> io::Result<()> {
    file.sync_all()?;
    record(EventKind::FileSync, path);
    Ok(())
}

pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
        record(EventKind::DirectorySync, path);
    }
    // Windows cannot open a directory as a regular File. Its rename uses
    // MoveFileExW write-through semantics; data files are synced before rename.
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventKind {
    FileSync,
    #[cfg_attr(not(unix), allow(dead_code))]
    DirectorySync,
    DirectoryCreated,
    AtomicReplace,
    CorruptCheckpointSkipped,
}

#[cfg(test)]
thread_local! {
    static EVENTS: std::cell::RefCell<Vec<(EventKind, std::path::PathBuf)>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

#[inline]
pub(crate) fn record(kind: EventKind, path: &Path) {
    #[cfg(test)]
    EVENTS.with(|events| events.borrow_mut().push((kind, path.to_path_buf())));
    #[cfg(not(test))]
    let _ = (kind, path);
}

#[cfg(test)]
pub(crate) fn take() -> Vec<(EventKind, std::path::PathBuf)> {
    EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
}

#[cfg(test)]
pub(crate) fn sync_count(events: &[(EventKind, std::path::PathBuf)]) -> usize {
    events
        .iter()
        .filter(|(kind, _)| matches!(kind, EventKind::FileSync | EventKind::DirectorySync))
        .count()
}
