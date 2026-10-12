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
        // Registered roots use comparison paths; apply the same spelling rules
        // to reads, including Windows verbatim prefixes and dot components.
        let path = crate::inspect::job::normalize_path(path);
        if let Ok(mut roots) = roots.lock() {
            for (root, counts) in roots.iter_mut() {
                if path.starts_with(root) {
                    *counts.entry(path.clone()).or_default() += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_roots_count_equivalent_path_spellings_only() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("tracked");
        std::fs::create_dir(&root).unwrap();
        let root = crate::inspect::job::canonicalize_normalized(&root);
        let counts = FileReadCounts::new();
        counts.reset(root.clone());
        counts.record(&fixture.path().join("spelling/../tracked/file.rs"));
        counts.record(&fixture.path().join("untracked/file.rs"));
        assert_eq!(counts.count(&root), 1);
    }

    #[cfg(windows)]
    #[test]
    fn registered_roots_count_windows_verbatim_paths() {
        let fixture = tempfile::tempdir().unwrap();
        let file = fixture.path().join("file.rs");
        std::fs::write(&file, "content").unwrap();
        let counts = FileReadCounts::new();
        let root = crate::inspect::job::canonicalize_normalized(fixture.path());
        counts.reset(root.clone());
        counts.record(&std::fs::canonicalize(file).unwrap());
        assert_eq!(counts.count(&root), 1);
    }
}
