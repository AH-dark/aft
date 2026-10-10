//! Durable file commits use the platform's full sync, including F_FULLFSYNC on
//! macOS. Rebuildable and process-lifetime stores deliberately do not call these
//! helpers. The test ledger observes the real calls, never substitutes for them.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
    let file = crate::private_storage::options()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let _cleanup = TempCleanup(temporary.clone());
    // Close before replacing so Windows does not retain an open temp handle.
    (move || -> io::Result<()> {
        let mut file = file;
        file.write_all(bytes)?;
        if !matches!(durability, AtomicWriteDurability::None) {
            sync_file(&file, path)?;
        }
        Ok(())
    })()?;
    crate::fs_lock::rename_over(&temporary, path)?;
    record(EventKind::AtomicReplace, path);
    if matches!(durability, AtomicWriteDurability::FileAndDirectory) {
        sync_dir(parent)?;
    }
    Ok(())
}

struct TempCleanup(PathBuf);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
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
