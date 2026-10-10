use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{mpsc, Arc};

use crate::watcher_filter::SharedGitignore;

#[cfg(target_os = "macos")]
mod fsevents;
#[cfg(target_os = "linux")]
mod inotify;
#[cfg(windows)]
mod windows;

#[cfg(any(windows, test))]
mod notification_buffer {
    pub(super) struct NotificationBuffer {
        // ReadDirectoryChangesW requires DWORD alignment regardless of the
        // allocator's incidental alignment for byte allocations.
        storage: Vec<u32>,
        byte_len: usize,
    }

    impl NotificationBuffer {
        pub(super) fn new(byte_len: usize) -> Self {
            Self {
                storage: vec![0; byte_len.div_ceil(4)],
                byte_len,
            }
        }

        pub(super) fn len(&self) -> usize {
            self.byte_len
        }

        pub(super) fn resize(&mut self, byte_len: usize) {
            self.storage.resize(byte_len.div_ceil(4), 0);
            self.byte_len = byte_len;
        }

        pub(super) fn as_bytes(&self) -> &[u8] {
            // SAFETY: u32 has no padding or invalid bit patterns, all words
            // are initialized, and byte_len never exceeds the allocation.
            unsafe { std::slice::from_raw_parts(self.storage.as_ptr().cast(), self.byte_len) }
        }

        pub(super) fn as_mut_ptr(&mut self) -> *mut u8 {
            self.storage.as_mut_ptr().cast()
        }

        pub(super) fn complete_into(&mut self, completed: &mut Self) {
            // The next read uses the spare allocation while translation owns
            // the completed one. Neither copying nor reallocation is needed.
            std::mem::swap(self, completed);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn element_alignment<T>(_: &[T]) -> usize {
            std::mem::align_of::<T>()
        }

        #[test]
        fn kernel_notification_buffer_has_dword_alignment_contract() {
            let mut buffer = NotificationBuffer::new(257);
            assert!(element_alignment(&buffer.storage) >= 4);
            assert_eq!(buffer.as_mut_ptr() as usize % 4, 0);
            assert_eq!(buffer.as_bytes().len(), 257);
            buffer.resize(64);
            assert_eq!(buffer.len(), 64);
        }

        #[test]
        fn notification_completion_swaps_allocations_without_copying() {
            let mut active = NotificationBuffer::new(256);
            let mut completed = NotificationBuffer::new(256);
            let active_pointer = active.as_mut_ptr();
            let completed_pointer = completed.as_mut_ptr();
            active.complete_into(&mut completed);
            assert_eq!(completed.as_bytes().as_ptr(), active_pointer);
            assert_eq!(active.as_mut_ptr(), completed_pointer);
            active.complete_into(&mut completed);
            assert_eq!(active.as_mut_ptr(), active_pointer);
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use fsevents::DirectoryWatcher;
#[cfg(target_os = "macos")]
pub(crate) use fsevents::ProjectWatcher;
#[cfg(target_os = "linux")]
pub(crate) use inotify::ProjectWatcher;
#[cfg(windows)]
pub(crate) use windows::ProjectWatcher;

/// Whether the backend compiled into this binary inherits an exclusion down
/// the subtree below it.
///
/// Only the Linux backend does: it adds one watch per directory and never
/// descends into an excluded or ignored one. FSEvents exclusion paths and the
/// Windows per-directory handles are both matched as exact paths, so an
/// excluded parent covers nothing underneath it. This lives here, in the
/// module that picks the backend, so the derivation rules in `watcher_filter`
/// never name a platform themselves.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) const BACKEND_EXCLUSION_COVERAGE: crate::watcher_filter::WatcherExclusionCoverage =
    if cfg!(target_os = "linux") {
        crate::watcher_filter::WatcherExclusionCoverage::Subtree
    } else {
        crate::watcher_filter::WatcherExclusionCoverage::ExactPath
    };

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) struct ProjectWatcher {
    _watcher: notify::RecommendedWatcher,
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
impl ProjectWatcher {
    fn create(
        root: PathBuf,
        extra_watch_paths: Vec<PathBuf>,
        tx: mpsc::Sender<notify::Result<notify::Event>>,
    ) -> notify::Result<Self> {
        use notify::{RecursiveMode, Watcher};

        let mut watcher = notify::recommended_watcher(tx)?;
        watcher.watch(&root, RecursiveMode::Recursive)?;
        for path in extra_watch_paths {
            if path.exists() {
                watcher.watch(&path, RecursiveMode::NonRecursive)?;
            }
        }
        Ok(Self { _watcher: watcher })
    }
}

pub(crate) fn create_project_watcher(
    root: PathBuf,
    extra_watch_paths: Vec<PathBuf>,
    tx: mpsc::Sender<notify::Result<notify::Event>>,
    matcher: SharedGitignore,
    matcher_generation: Arc<AtomicU64>,
) -> notify::Result<ProjectWatcher> {
    #[cfg(target_os = "macos")]
    {
        return ProjectWatcher::create(root, extra_watch_paths, tx, matcher, matcher_generation);
    }
    #[cfg(target_os = "linux")]
    {
        return ProjectWatcher::create(root, extra_watch_paths, tx, matcher, matcher_generation);
    }
    #[cfg(windows)]
    {
        return ProjectWatcher::create(root, extra_watch_paths, tx, matcher, matcher_generation);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (matcher, matcher_generation);
        ProjectWatcher::create(root, extra_watch_paths, tx)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn render_exclusions(
    root: &std::path::Path,
    exclusions: &[crate::watcher_filter::WatcherExclusion],
    matcher_generation: u64,
) -> String {
    let seeded = exclusions
        .iter()
        .map(|exclusion| {
            exclusion
                .path()
                .strip_prefix(root)
                .unwrap_or(exclusion.path())
                .display()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(",");
    let sources = exclusions
        .iter()
        .map(|exclusion| exclusion.source().as_str())
        .collect::<Vec<_>>()
        .join(",");
    // Generation zero means no ignore rules were ever loaded for this root, so
    // the plan could only see `.git`. Say so in the line instead of letting it
    // read like a healthy root that simply has nothing to exclude.
    let unloaded = if matcher_generation == 0 {
        " matcher=unloaded"
    } else {
        ""
    };
    format!(
        "watcher exclusions: seeded=[{seeded}] by=[{sources}] root={}{unloaded}",
        root.display()
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn log_exclusions(
    root: &std::path::Path,
    exclusions: &[crate::watcher_filter::WatcherExclusion],
    matcher_generation: u64,
) {
    crate::slog_info!(
        "{}",
        render_exclusions(root, exclusions, matcher_generation)
    );
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use ignore::gitignore::GitignoreBuilder;

    use super::*;
    use crate::watcher_filter::{derive_excluded_subtrees, WATCHER_EXCLUSION_LIMIT};

    #[test]
    fn exclusion_log_names_each_seed_and_decision_source() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::create_dir(root.path().join("generated")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "target/\ngenerated/\n").unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let mut builder = GitignoreBuilder::new(&root);
        builder.add(root.join(".gitignore"));
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let exclusions = derive_excluded_subtrees(&root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));

        assert_eq!(
            render_exclusions(&root, &exclusions, 1),
            format!(
                "watcher exclusions: seeded=[target,generated] by=[ecosystem,gitignore] root={}",
                root.display()
            )
        );
        assert_eq!(
            render_exclusions(&root, &exclusions[..0], 0),
            format!(
                "watcher exclusions: seeded=[] by=[] root={} matcher=unloaded",
                root.display()
            )
        );
    }
}
