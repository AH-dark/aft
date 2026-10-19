use aft::bash_background::persistence::{create_task_layout, TaskPaths};
use std::path::{Path, PathBuf};

pub struct SessionOutput {
    pub own: TaskPaths,
    pub other: TaskPaths,
    pub other_harness: TaskPaths,
    pub private: Vec<PathBuf>,
}

impl SessionOutput {
    pub fn new(storage: &Path, session: &str) -> Self {
        let harness = storage.join("runner");
        let own = create_task_layout(&harness, session, "bash-1111111111111111")
            .unwrap()
            .paths;
        let other = create_task_layout(&harness, "other-session", "bash-2222222222222222")
            .unwrap()
            .paths;
        let other_harness =
            create_task_layout(&storage.join("pi"), session, "bash-3333333333333333")
                .unwrap()
                .paths;
        for paths in [&own, &other, &other_harness] {
            std::fs::write(&paths.stdout, "saved-output\n").unwrap();
            std::fs::write(&paths.stderr, "saved-error\n").unwrap();
            std::fs::write(&paths.environment, "secret-environment").unwrap();
        }
        let private = [
            storage.join("aft.db"),
            storage.join("aft.db-wal"),
            storage.join("views/private.txt"),
            storage.join("blobs/private.txt"),
            own.io_dir.join("unlisted.sqlite"),
        ]
        .into_iter()
        .map(|path| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            if !path.exists() {
                std::fs::write(&path, "private-data").unwrap();
            }
            path
        })
        .collect();
        Self {
            own,
            other,
            other_harness,
            private,
        }
    }
}
