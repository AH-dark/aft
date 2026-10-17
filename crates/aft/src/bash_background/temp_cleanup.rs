use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};

type Removal = (
    PathBuf,
    Sender<std::io::Result<()>>,
    Arc<super::registry::DbWriteOrder>,
);
const QUEUE_CAPACITY: usize = 64;

/// Scratch deletion has its own single worker: the periodic maintenance lane
/// only runs once a minute, and timeout workers must remain free to kill tasks.
pub(super) fn schedule(
    path: &Path,
    owner: Arc<super::registry::DbWriteOrder>,
) -> Option<Receiver<std::io::Result<()>>> {
    static QUEUE: std::sync::Mutex<Option<Sender<Removal>>> = std::sync::Mutex::new(None);
    let queue = {
        let mut queue = QUEUE.lock().unwrap_or_else(|poison| poison.into_inner());
        if queue.is_none() {
            let (tx, rx) = crossbeam_channel::bounded::<Removal>(QUEUE_CAPACITY);
            match std::thread::Builder::new()
                .name("aft-bash-scratch-cleanup".into())
                .spawn(move || {
                    for (path, result, _owner) in rx {
                        let removed =
                            std::panic::catch_unwind(|| remove(&path)).unwrap_or_else(|_| {
                                Err(std::io::Error::other("scratch cleanup panicked"))
                            });
                        let _ = result.send(removed);
                    }
                }) {
                Ok(_) => *queue = Some(tx),
                Err(error) => crate::slog_warn!("failed to start bash scratch cleanup: {error}"),
            }
        }
        queue.clone()?
    };
    schedule_on(&queue, path, owner)
}

fn schedule_on(
    queue: &Sender<Removal>,
    path: &Path,
    owner: Arc<super::registry::DbWriteOrder>,
) -> Option<Receiver<std::io::Result<()>>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    // Full queues leave the durable scratch path untouched for a later pass or
    // the stale sweep. Never wait for the worker from a task observer.
    // Retain the task's process-wide ownership even if its registry unloads,
    // so persisted GC cannot remove the record while deletion is still running.
    queue.try_send((path.to_path_buf(), tx, owner)).ok()?;
    Some(rx)
}

pub(super) fn remove(path: &Path) -> std::io::Result<()> {
    #[cfg(all(test, unix))]
    {
        let gate = removal_gates().lock().unwrap().remove(path);
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.recv_timeout(std::time::Duration::from_secs(30));
        }
    }
    crate::sandbox_spawn::cleanup_task_temp_dir(path)
}

#[cfg(all(test, unix))]
type RemovalGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

#[cfg(all(test, unix))]
fn removal_gates(
) -> &'static std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, RemovalGate>> {
    static GATES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, RemovalGate>>,
    > = std::sync::OnceLock::new();
    GATES.get_or_init(Default::default)
}

#[cfg(all(test, unix))]
pub(super) fn block_removal(
    path: &Path,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    removal_gates()
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), (entered_tx, release_rx));
    (entered_rx, release_tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_background_temp_cleanup_full_queue_defers_without_waiting() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let owner = Arc::new(super::super::registry::DbWriteOrder::default());
        let first = schedule_on(&tx, Path::new("first"), owner.clone()).unwrap();
        assert!(schedule_on(&tx, Path::new("deferred"), owner.clone()).is_none());
        let (path, result, _) = rx.recv().unwrap();
        assert_eq!(path, Path::new("first"));
        result.send(Ok(())).unwrap();
        first.recv().unwrap().unwrap();
        assert!(schedule_on(&tx, Path::new("retry"), owner.clone()).is_some());
        drop(rx);
        assert!(schedule_on(&tx, Path::new("disconnected"), owner).is_none());
    }
}
