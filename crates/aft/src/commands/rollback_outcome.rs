//! Shared recovery reporting for mutating commands.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

pub(super) struct RollbackOutcome {
    restored: bool,
    deleted: HashSet<PathBuf>,
    errors: Vec<String>,
}

impl RollbackOutcome {
    pub(super) fn from_restore(result: Result<(), String>) -> Self {
        match result {
            Ok(()) => Self {
                restored: true,
                deleted: HashSet::new(),
                errors: Vec::new(),
            },
            Err(error) => Self {
                restored: false,
                deleted: HashSet::new(),
                errors: vec![error],
            },
        }
    }

    pub(super) fn record_deletion(&mut self, path: &Path, result: std::io::Result<()>) {
        match result {
            Ok(()) => {
                self.deleted.insert(path.to_path_buf());
            }
            Err(error) => self
                .errors
                .push(format!("failed to delete {}: {error}", path.display())),
        }
    }

    pub(super) fn restored(&self) -> bool {
        self.restored
    }
    pub(super) fn deleted(&self, path: &Path) -> bool {
        self.deleted.contains(path)
    }
    pub(super) fn complete(&self) -> bool {
        self.restored && self.errors.is_empty()
    }

    pub(super) fn append_to(&self, data: &mut Value, checkpoint: &str) {
        data["rollback_complete"] = self.complete().into();
        if !self.complete() {
            data["rollback_error"] = self.errors.join("; ").into();
            data["recovery_checkpoint"] = checkpoint.into();
        }
    }

    pub(super) fn failure_message(&self, message: &str, checkpoint: &str) -> String {
        if self.complete() {
            return message.to_string();
        }
        format!(
            "{message}; rollback incomplete: {}; recovery checkpoint: {checkpoint}",
            self.errors.join("; ")
        )
    }
}
