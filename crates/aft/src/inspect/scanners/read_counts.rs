use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Read counters are enabled explicitly for fixture roots, not for ordinary
/// debug-daemon scans. Counting one root must not collect paths from others.
#[derive(Default)]
pub(super) struct FileReadCounts {
    roots: OnceLock<Mutex<BTreeMap<PathBuf, BTreeMap<PathBuf, usize>>>>,
}

impl FileReadCounts {
    pub(super) const fn new() -> Self {
        Self {
            roots: OnceLock::new(),
        }
    }

    pub(super) fn record(&self, path: &Path) {
        let Some(roots) = self.roots.get() else {
            return;
        };
        if let Ok(mut roots) = roots.lock() {
            for (root, counts) in roots.iter_mut() {
                if path.starts_with(root) {
                    *counts.entry(path.to_path_buf()).or_default() += 1;
                }
            }
        }
    }

    pub(super) fn reset(&self, root: PathBuf) {
        if let Ok(mut roots) = self.roots.get_or_init(Default::default).lock() {
            roots.insert(root, BTreeMap::new());
        }
    }

    pub(super) fn count(&self, root: &Path) -> usize {
        self.roots
            .get()
            .and_then(|roots| roots.lock().ok())
            .and_then(|roots| roots.get(root).map(|counts| counts.values().sum()))
            .unwrap_or_default()
    }
}
