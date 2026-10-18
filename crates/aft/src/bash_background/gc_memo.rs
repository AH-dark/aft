//! What the persisted-task sweep (`BgTaskRegistry::maybe_gc_persisted`)
//! learned on earlier passes over a storage root, so later passes need not
//! reopen tasks that cannot have become deletable since.
//!
//! The sweep walks the store a bounded number of directory entries per pass
//! and wraps around when it reaches the end. Most of a large store is history
//! the sweep must keep: finished tasks whose result was never delivered, and
//! tasks written within the modification grace. Without a memory of those
//! verdicts, every lap reopened each task's folders and parsed its metadata
//! twice, only to reach the same answer again.
//!
//! A remembered verdict is trusted only while everything it was derived from
//! is unchanged: the session folder (a task layout appearing or vanishing
//! beside it), the task folder (its `control/` or `io/` folder replaced), and
//! the metadata file itself. Metadata is always written by atomic replacement,
//! so any rewrite gives the file a new identity and a new modification time.
//! Checking those takes a few `stat` calls and opens nothing. Conditions that
//! can change without any file in the store changing are evaluated again
//! every visit: the end of the modification grace, and the age and
//! missing-worktree tests that retire an abandoned completion. Running tasks,
//! and anything the sweep failed to read, are never remembered at all.
//!
//! The memory lives in the daemon process, keyed by canonical storage root,
//! next to the walk position kept by `gc_cursor`. A restart starts both from
//! scratch, so the first lap after a restart evaluates every task once.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use super::persistence::TaskLayout;

/// The parts of a file's metadata that every replacement or rewrite changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    modified: SystemTime,
    len: u64,
    identity: (u64, u64),
}

impl Stamp {
    /// Reads the stamp without following a symlink. A filesystem that cannot
    /// report a modification time yields an error, so nothing is remembered
    /// from evidence that cannot be checked later.
    pub(crate) fn of(path: &Path) -> std::io::Result<Self> {
        #[cfg(test)]
        super::persistence::work_counts::record_stat();
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(Self {
            modified: metadata.modified()?,
            len: metadata.len(),
            identity: identity(&metadata),
        })
    }
}

#[cfg(unix)]
fn identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(windows)]
fn identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    // Stable Rust has no file index on Windows; a replacement file is a new
    // file with its own creation time, and its write time changes as well.
    use std::os::windows::fs::MetadataExt;
    (metadata.creation_time(), 0)
}

#[cfg(not(any(unix, windows)))]
fn identity(_metadata: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// True while a file modified at `modified` is inside `grace`. Matches the
/// sweep's own grace test: a modification time in the future is not inside it.
pub(crate) fn within_grace(modified: SystemTime, grace: Duration) -> bool {
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|age| age < grace)
        .unwrap_or(false)
}

/// Whether an undelivered, finished task's result is abandoned: it finished
/// more than seven days ago and both its project root and its working
/// directory are gone. The caller has already established that the task is
/// terminal and its completion undelivered.
pub(crate) fn abandonment_due(
    finished_at: Option<u64>,
    project_root: Option<&Path>,
    workdir: &Path,
    now_ms: u64,
) -> bool {
    const ABANDONMENT_MS: u64 = 7 * 24 * 60 * 60 * 1000;
    finished_at.is_some_and(|finished| now_ms.saturating_sub(finished) > ABANDONMENT_MS)
        && project_root.is_some_and(|root| matches!(root.try_exists(), Ok(false)))
        && matches!(workdir.try_exists(), Ok(false))
}

/// Why the sweep left a task in place, and what could make it deletable
/// without its files changing.
#[derive(Clone, Debug)]
pub(crate) enum Wait {
    /// Its metadata was written within the modification grace.
    Grace,
    /// It finished but its result was never delivered. Unless its metadata
    /// changes (a delivery rewrites it), only abandonment can retire it.
    Abandonment {
        finished_at: Option<u64>,
        project_root: Option<Arc<Path>>,
        workdir: Arc<Path>,
    },
}

#[derive(Clone, Debug)]
struct Verdict {
    layout: TaskLayout,
    session: Stamp,
    /// The task folder's stamp; `None` for the legacy flat layout, whose
    /// files sit directly in the session folder.
    task_dir: Option<Stamp>,
    metadata: Stamp,
    wait: Wait,
}

impl Verdict {
    fn metadata_path(&self, session_dir: &Path, task_id: &str) -> PathBuf {
        match self.layout {
            TaskLayout::Directory => session_dir
                .join(task_id)
                .join(super::persistence::CONTROL_DIR)
                .join(super::persistence::METADATA_FILE),
            TaskLayout::Flat => session_dir.join(format!("{task_id}.json")),
        }
    }

    /// Whether the task is still certainly not deletable. Any stamp that
    /// differs or cannot be read means "judge it again", never "skip".
    fn still_waiting(
        &self,
        session_dir: &Path,
        session: &Stamp,
        task_id: &str,
        grace: Duration,
        now_ms: u64,
    ) -> bool {
        if self.session != *session {
            return false;
        }
        if let Some(task_dir) = &self.task_dir {
            if Stamp::of(&session_dir.join(task_id)).ok().as_ref() != Some(task_dir) {
                return false;
            }
        }
        if Stamp::of(&self.metadata_path(session_dir, task_id))
            .ok()
            .as_ref()
            != Some(&self.metadata)
        {
            return false;
        }
        match &self.wait {
            Wait::Grace => within_grace(self.metadata.modified, grace),
            Wait::Abandonment {
                finished_at,
                project_root,
                workdir,
            } => !abandonment_due(*finished_at, project_root.as_deref(), workdir, now_ms),
        }
    }
}

struct Remembered {
    verdict: Verdict,
    /// The lap in which the sweep last reached this task. Entries not reached
    /// during a whole lap belong to deleted tasks and are dropped at its end.
    seen_lap: u64,
}

/// One storage root's sweep progress, reported on the health surface.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct PersistedGcHealth {
    /// Path of the canonical `bash-tasks` folder (inside a harness storage
    /// root) whose sweep these figures describe.
    pub root: String,
    pub passes: u64,
    /// Full laps over the store completed since the daemon started.
    pub laps_completed: u64,
    /// Set when the last pass used up its entry budget before reaching the end
    /// of the store; the next pass resumes where it stopped. On a store larger
    /// than one pass's budget this is set after every pass of a lap except
    /// the last.
    pub last_pass_out_of_budget: bool,
    pub passes_out_of_budget: u64,
    /// Task names the last pass reached, how many of them it had to open and
    /// judge, and how many it skipped because a remembered verdict still held.
    pub last_pass_tasks: usize,
    pub last_pass_evaluated: usize,
    pub last_pass_unchanged: usize,
    /// Session folders that could not be opened during the last pass.
    pub last_pass_unreadable_sessions: usize,
    /// Directory-entry reads that failed during the last pass.
    pub last_pass_directory_read_errors: usize,
    /// Tasks currently remembered as not deletable.
    pub remembered_tasks: usize,
    /// Why the last pass could not walk the store, if it could not. Such a
    /// pass deleted nothing because it could not look, not because nothing
    /// was due.
    pub last_error: Option<String>,
}

#[derive(Default)]
struct RootState {
    /// Generation of the current store walk, including walks with read errors.
    lap: u64,
    sessions: HashMap<PathBuf, HashMap<String, Remembered>>,
    /// Sessions whose task listings were read during the current store walk.
    read_sessions: HashSet<PathBuf>,
    /// Sessions whose task listings were incomplete during the current store walk.
    unreadable_sessions: HashSet<PathBuf>,
    /// Whether any root-directory entry could not be read during this store walk.
    root_directory_unreadable: bool,
    /// Project roots and working directories are shared by many tasks; keep
    /// one copy of each.
    paths: HashSet<Arc<Path>>,
    health: PersistedGcHealth,
    /// Process-wide order of this root's last pass, to name the most recent
    /// failure across roots.
    last_pass_seq: u64,
}

impl RootState {
    fn intern(&mut self, path: &Path) -> Arc<Path> {
        if let Some(existing) = self.paths.get(path) {
            return Arc::clone(existing);
        }
        let interned: Arc<Path> = Arc::from(path);
        self.paths.insert(Arc::clone(&interned));
        interned
    }

    fn remembered_tasks(&self) -> usize {
        self.sessions.values().map(HashMap::len).sum()
    }
}

fn roots() -> &'static Mutex<HashMap<PathBuf, RootState>> {
    static ROOTS: std::sync::OnceLock<Mutex<HashMap<PathBuf, RootState>>> =
        std::sync::OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_root<R>(root: &Path, work: impl FnOnce(&mut RootState) -> R) -> R {
    let mut roots = roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = roots
        .entry(root.to_path_buf())
        .or_insert_with(|| RootState {
            health: PersistedGcHealth {
                root: root.display().to_string(),
                ..PersistedGcHealth::default()
            },
            ..RootState::default()
        });
    work(state)
}

/// The sweep's memory for one canonical `bash-tasks` root.
pub(crate) struct GcMemo {
    root: PathBuf,
}

/// The file stamps a remembered keep-verdict depends on, captured before the
/// sweep read the task metadata that the verdict is based on.
pub(crate) struct Evidence {
    pub layout: TaskLayout,
    pub session: Stamp,
    pub task_dir: Option<Stamp>,
    pub metadata: Stamp,
}

/// Input for [`Wait::Abandonment`], borrowed from the task's metadata.
pub(crate) struct AbandonmentInput<'a> {
    pub finished_at: Option<u64>,
    pub project_root: Option<&'a Path>,
    pub workdir: &'a Path,
}

impl GcMemo {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// True when the task was judged not deletable earlier and nothing it was
    /// judged on has changed. Opens nothing: at most two `stat` calls, plus
    /// two for the worktree checks of an undelivered result old enough to be
    /// abandoned. A verdict that no longer holds is forgotten.
    pub(crate) fn still_waiting(
        &self,
        session_dir: &Path,
        session: Option<&Stamp>,
        task_id: &str,
        grace: Duration,
        now_ms: u64,
    ) -> bool {
        let Some(session) = session else {
            return false;
        };
        let verdict = with_root(&self.root, |state| {
            let lap = state.lap;
            let entry = state.sessions.get_mut(session_dir)?.get_mut(task_id)?;
            entry.seen_lap = lap;
            Some(entry.verdict.clone())
        });
        let Some(verdict) = verdict else {
            return false;
        };
        // The stats run without the lock held: other roots' sweeps share it.
        let waiting = verdict.still_waiting(session_dir, session, task_id, grace, now_ms);
        if !waiting {
            self.forget(session_dir, task_id);
        }
        waiting
    }

    pub(crate) fn remember_grace(&self, session_dir: &Path, task_id: &str, evidence: Evidence) {
        self.remember(session_dir, task_id, evidence, |_| Wait::Grace);
    }

    pub(crate) fn remember_undelivered(
        &self,
        session_dir: &Path,
        task_id: &str,
        evidence: Evidence,
        input: AbandonmentInput<'_>,
    ) {
        self.remember(session_dir, task_id, evidence, |state| Wait::Abandonment {
            finished_at: input.finished_at,
            project_root: input.project_root.map(|root| state.intern(root)),
            workdir: state.intern(input.workdir),
        });
    }

    fn remember(
        &self,
        session_dir: &Path,
        task_id: &str,
        evidence: Evidence,
        wait: impl FnOnce(&mut RootState) -> Wait,
    ) {
        with_root(&self.root, |state| {
            let wait = wait(state);
            let lap = state.lap;
            state
                .sessions
                .entry(session_dir.to_path_buf())
                .or_default()
                .insert(
                    task_id.to_string(),
                    Remembered {
                        verdict: Verdict {
                            layout: evidence.layout,
                            session: evidence.session,
                            task_dir: evidence.task_dir,
                            metadata: evidence.metadata,
                            wait,
                        },
                        seen_lap: lap,
                    },
                );
        });
    }

    /// Marks a session whose task listing was successfully read in this walk.
    pub(crate) fn session_read(&self, session_dir: &Path) {
        with_root(&self.root, |state| {
            if state.sessions.contains_key(session_dir) {
                state.read_sessions.insert(session_dir.to_path_buf());
            }
        });
    }

    /// Keeps memo entries for sessions whose task listing was incomplete.
    pub(crate) fn session_unreadable(&self, session_dir: &Path) {
        with_root(&self.root, |state| {
            if state.sessions.contains_key(session_dir) {
                state.unreadable_sessions.insert(session_dir.to_path_buf());
            }
        });
    }

    /// Keeps unseen session entries when the root listing itself was incomplete.
    pub(crate) fn root_directory_unreadable(&self) {
        with_root(&self.root, |state| state.root_directory_unreadable = true);
    }

    pub(crate) fn forget(&self, session_dir: &Path, task_id: &str) {
        with_root(&self.root, |state| {
            if let Some(tasks) = state.sessions.get_mut(session_dir) {
                tasks.remove(task_id);
                if tasks.is_empty() {
                    state.sessions.remove(session_dir);
                }
            }
        });
    }

    /// Memoized task entries absent from successfully read sessions are dropped;
    /// entries for sessions that could not be inspected are preserved. A root
    /// that no longer exists is forgotten entirely.
    pub(crate) fn finish_pass(&self, pass: PassReport) {
        static PASS_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if pass.root_missing {
            roots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.root);
            return;
        }
        let seq = PASS_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        with_root(&self.root, |state| {
            state.last_pass_seq = seq;
            if pass.walk_finished {
                let lap = state.lap;
                let read_sessions = std::mem::take(&mut state.read_sessions);
                let unreadable_sessions = std::mem::take(&mut state.unreadable_sessions);
                let root_directory_unreadable =
                    std::mem::replace(&mut state.root_directory_unreadable, false);
                state.sessions.retain(|session_dir, tasks| {
                    if unreadable_sessions.contains(session_dir) {
                        // A partial listing cannot prove which remembered tasks
                        // disappeared, so preserve the session's verdicts.
                    } else if read_sessions.contains(session_dir) {
                        tasks.retain(|_, entry| entry.seen_lap >= lap);
                    } else if !root_directory_unreadable {
                        return false;
                    }
                    !tasks.is_empty()
                });
                state.paths.retain(|path| Arc::strong_count(path) > 1);
                state.lap = lap.saturating_add(1);
                if pass.error.is_none() {
                    state.health.laps_completed = state.health.laps_completed.saturating_add(1);
                }
            }
            let remembered = state.remembered_tasks();
            let health = &mut state.health;
            health.passes = health.passes.saturating_add(1);
            health.last_pass_out_of_budget = !pass.walk_finished && pass.error.is_none();
            if health.last_pass_out_of_budget {
                health.passes_out_of_budget = health.passes_out_of_budget.saturating_add(1);
            }
            health.last_pass_tasks = pass.tasks;
            health.last_pass_evaluated = pass.evaluated;
            health.last_pass_unchanged = pass.unchanged;
            health.last_pass_unreadable_sessions = pass.unreadable_sessions;
            health.last_pass_directory_read_errors = pass.directory_read_errors;
            health.remembered_tasks = remembered;
            health.last_error = pass.error;
        });
    }
}

/// What one sweep pass did, for [`GcMemo::finish_pass`].
#[derive(Debug, Default)]
pub(crate) struct PassReport {
    pub walk_finished: bool,
    /// The `bash-tasks` folder does not exist, so there is nothing to sweep
    /// or remember.
    pub root_missing: bool,
    pub error: Option<String>,
    pub tasks: usize,
    pub evaluated: usize,
    pub unchanged: usize,
    pub unreadable_sessions: usize,
    pub directory_read_errors: usize,
}

/// A storage root whose last sweep pass could not walk it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersistedGcFailure {
    pub root: String,
    pub error: String,
}

/// Sweep progress across every storage root this process sweeps, as fixed
/// size totals so the health payload does not grow with the number of roots.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct PersistedGcSummary {
    pub roots: usize,
    /// Roots whose last pass used up its entry budget before the end of the
    /// store. Each resumes where it stopped on its next pass (once a minute),
    /// so this is a backlog being worked through, not a stall.
    pub roots_out_of_budget: usize,
    pub passes_out_of_budget_total: u64,
    pub laps_completed_total: u64,
    /// Tasks known to be kept, which later laps skip without opening.
    pub remembered_tasks_total: usize,
    /// Roots whose last pass could not walk the store at all, and the most
    /// recent such failure. Those passes deleted nothing because they could
    /// not look, not because nothing was due.
    pub roots_failing: usize,
    pub last_failure: Option<PersistedGcFailure>,
}

/// Sweep progress totals for the health surface.
pub fn persisted_gc_health() -> PersistedGcSummary {
    let roots = roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut summary = PersistedGcSummary {
        roots: roots.len(),
        ..PersistedGcSummary::default()
    };
    let mut last_failure_seq = None;
    for state in roots.values() {
        let health = &state.health;
        summary.roots_out_of_budget += usize::from(health.last_pass_out_of_budget);
        summary.passes_out_of_budget_total = summary
            .passes_out_of_budget_total
            .saturating_add(health.passes_out_of_budget);
        summary.laps_completed_total = summary
            .laps_completed_total
            .saturating_add(health.laps_completed);
        summary.remembered_tasks_total += health.remembered_tasks;
        if let Some(error) = &health.last_error {
            summary.roots_failing += 1;
            if last_failure_seq.is_none_or(|seq| state.last_pass_seq > seq) {
                last_failure_seq = Some(state.last_pass_seq);
                summary.last_failure = Some(PersistedGcFailure {
                    root: health.root.clone(),
                    error: error.clone(),
                });
            }
        }
    }
    summary
}

/// Sweep progress for one canonical `bash-tasks` root.
#[cfg(test)]
pub(crate) fn persisted_gc_health_for(root: &Path) -> Option<PersistedGcHealth> {
    roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(root)
        .map(|state| state.health.clone())
}
