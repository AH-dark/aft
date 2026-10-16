use super::*;
use crate::bash_background::persistence::{PinnedDir, ResolvedTask, TaskLayout};

pub(super) struct TaskReadRecord {
    resolved: ResolvedTask,
    metadata: PersistedTask,
    readers: Mutex<HashMap<(String, String, u64), bool>>,
}

/// Recognize the task bundle structurally, without trusting a caller's session
/// namespace or granting access to the enclosing private store.
fn task_directory(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| {
        ancestor
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            == Some(std::ffi::OsStr::new("bash-tasks"))
            && ancestor
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| validate_task_id(name).is_ok())
    })
}

pub(crate) fn is_task_environment(path: &Path) -> bool {
    task_directory(path).is_some_and(|root| path == root.join("control/environment.bin"))
}

/// Recursive readers must not bypass the explicit-path check by entering a
/// bundle through its directory. Do not follow links or credential payloads.
pub(crate) fn safe_task_entry(path: &Path) -> bool {
    if task_directory(path).is_none() {
        return true;
    }
    if is_task_environment(path) {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
        return false;
    }
    if metadata.is_file() {
        return path
            .parent()
            .and_then(|parent| PinnedDir::open(parent).ok())
            .is_some_and(|parent| {
                path.file_name()
                    .is_some_and(|name| parent.open_file(name, false).is_ok())
            });
    }
    true
}

impl BgTaskRegistry {
    fn task_read_record(&self, root: &Path) -> Option<Arc<TaskReadRecord>> {
        if let Some(record) = self.inner.task_read_records.lock().ok()?.get(root).cloned() {
            record.resolved.dirs.task.ensure_current_identity().ok()?;
            record
                .resolved
                .dirs
                .control
                .ensure_current_identity()
                .ok()?;
            record.resolved.dirs.io.ensure_current_identity().ok()?;
            return Some(record);
        }
        let resolved = resolve_task_layout(root.parent()?, root.file_name()?.to_str()?).ok()?;
        if resolved.paths.layout != TaskLayout::Directory {
            return None;
        }
        let metadata = read_task_at(&resolved).ok()?;
        if root.parent()?.file_name()?.to_str()?
            != crate::backup::hash_session(&metadata.session_id)
        {
            return None;
        }
        let record = Arc::new(TaskReadRecord {
            resolved,
            metadata,
            readers: Mutex::new(HashMap::new()),
        });
        let mut records = self.inner.task_read_records.lock().ok()?;
        Some(Arc::clone(
            records.entry(root.to_path_buf()).or_insert(record),
        ))
    }

    fn head_reads_record(&self, record: &TaskReadRecord) -> bool {
        let Some(caller) = crate::access_policy::current_scope() else {
            return false;
        };
        let key = (caller.owner.clone(), caller.scope_ref.clone(), caller.epoch);
        let Ok(mut readers) = record.readers.lock() else {
            return false;
        };
        *readers.entry(key).or_insert_with(|| {
            let Some(scope) = record.metadata.scope.as_ref() else {
                return false;
            };
            if !scope.owner_authorized || scope.kind != subc_protocol::scope::ScopeKind::Worker {
                return false;
            }
            let Some(parent) = scope.parent.as_ref() else {
                return false;
            };
            // A revived worker has a new session and epoch, but its parent
            // scope's owner and reference still identify the owning head.
            if caller.owner != serde_json::to_string(&parent.owner).expect("scope owner serializes")
                || caller.scope_ref != parent.scope_ref
            {
                return false;
            }
            self.inner
                .db_pool
                .read()
                .ok()
                .and_then(|pool| pool.clone())
                .and_then(|pool| {
                    pool.lock()
                        .ok()
                        .and_then(|conn| crate::db::access_grants::is_head(&conn, &caller).ok())
                })
                .unwrap_or(false)
        })
    }

    /// The read-only exception is pinned and cached per bundle, never per file.
    /// Bash's separate output-file exception remains exact-file-only: permission
    /// to inspect a task folder must not let a shell command enter that folder.
    pub(crate) fn is_readable_task_path(&self, session: &str, path: &Path) -> bool {
        if !safe_task_entry(path) {
            return false;
        }
        let Some(root) = task_directory(path) else {
            return false;
        };
        let Some(record) = self.task_read_record(root) else {
            return false;
        };
        let own_io = record.metadata.session_id == session
            && path.starts_with(&record.resolved.paths.io_dir);
        if !own_io && !self.head_reads_record(&record) {
            return false;
        }
        if path.is_dir() {
            return PinnedDir::open(path).is_ok();
        }
        let Ok(parent) = PinnedDir::open(path.parent().unwrap_or(root)) else {
            return false;
        };
        path.file_name()
            .is_some_and(|name| parent.open_file(name, false).is_ok())
    }

    pub(crate) fn readable_worker_task_owner(
        &self,
        task_id: &str,
        storage: &Path,
    ) -> Option<String> {
        validate_task_id(task_id).ok()?;
        crate::access_policy::current_scope()?;
        let cached = self
            .inner
            .task_read_records
            .lock()
            .ok()?
            .values()
            .find(|record| {
                record.metadata.task_id == task_id && record.resolved.paths.dir.starts_with(storage)
            })
            .cloned();
        if let Some(record) = cached {
            let record = self.task_read_record(&record.resolved.paths.dir)?;
            return self
                .head_reads_record(&record)
                .then(|| record.metadata.session_id.clone());
        }
        // Task IDs are unique, but a revived worker's session namespace changes.
        // Resolve the persisted bundle rather than the worker's current session
        // ID, and retain the folder decision for later reads.
        for entry in fs::read_dir(storage.join("bash-tasks")).ok()?.flatten() {
            let root = entry.path().join(task_id);
            if !safe_task_entry(&root) {
                continue;
            }
            if let Some(record) = self.task_read_record(&root) {
                if self.head_reads_record(&record) {
                    return Some(record.metadata.session_id.clone());
                }
            }
        }
        None
    }
}
