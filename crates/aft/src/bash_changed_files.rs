//! Bounded, observational shell-change reporting. No tree walk or file snapshots.
use crate::config::BashChangedFiles;
use crate::list_envelope::{ListEnvelope, Reason};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) const MAX_EVENTS: usize = 8192;
pub(crate) const MAX_FILES: usize = 40;
pub(crate) const MAX_DIFF_LINES: usize = 200;
pub(crate) const MAX_REPORT_BYTES: usize = 64 * 1024;
const MAX_FILE_BYTES: usize = 256 * 1024;
const HEADER: &str =
    "Files changed while the command ran (changes by other processes may be included):";
const DIRTY: &str = "no diff: it had uncommitted changes before the command";
const OUTSIDE_GIT: &str =
    "No diffs: the project is not in a git repository, so only paths are listed.";
const GIT_UNAVAILABLE: &str = "No diffs: git before-images could not be captured within the time/output budget, so only paths are listed.";
const OVERFLOW: &str = "Incomplete: the file watcher overflowed, so some changes may be missing.";
pub(crate) const NOT_CAUGHT_UP: &str = "Incomplete: the file watcher had not caught up when the command ended, so some changes may be missing.";

#[derive(Default, Debug)]
pub(crate) struct Journal {
    state: Mutex<JournalState>,
}
#[derive(Default, Debug)]
struct JournalState {
    sequence: u64,
    gaps: u64,
    events: VecDeque<(u64, PathBuf, bool)>,
}
impl Journal {
    pub(crate) fn event(&self, event: &notify::Event) {
        if !crate::watcher_filter::watcher_event_invalidates(&event.kind) {
            return;
        }
        if matches!(
            event.kind,
            notify::EventKind::Create(notify::event::CreateKind::Folder)
                | notify::EventKind::Remove(notify::event::RemoveKind::Folder)
        ) {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for path in &event.paths {
            state.sequence += 1;
            let sequence = state.sequence;
            if state.events.len() == MAX_EVENTS {
                state.events.pop_front();
            }
            state
                .events
                .push_back((sequence, path.clone(), event.kind.is_create()));
        }
    }
    pub(crate) fn gap(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .gaps += 1;
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_lines_list_envelope: Option<ListEnvelope>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub text: String,
    pub files: Vec<ChangedFile>,
    pub incomplete: Vec<String>,
    pub changed_files_list_envelope: ListEnvelope,
}

#[derive(Debug)]
pub(crate) struct Capture {
    root: PathBuf,
    journal: Arc<Journal>,
    sequence: u64,
    gaps: u64,
    mode: BashChangedFiles,
    watcher_active: bool,
    flush: Option<Arc<crate::watcher_backend::flush::FlushControl>>,
    call_deadline: SharedCallDeadline,
    git: Option<GitStart>,
    end: OnceLock<(u64, u64)>,
    report: OnceLock<Report>,
}
#[derive(Debug)]
struct GitStart {
    directory: PathBuf,
    head: String,
    dirty: BTreeSet<PathBuf>,
}

thread_local! { static CURRENT: RefCell<Option<Arc<Capture>>> = const { RefCell::new(None) }; }
pub(crate) struct Scope(Option<Arc<Capture>>);
pub(crate) type SharedCallDeadline = Arc<Mutex<Option<Instant>>>;
pub(crate) struct BudgetGuard(SharedCallDeadline);
impl BudgetGuard {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self(Arc::new(Mutex::new(Some(deadline))))
    }
    pub(crate) fn shared(&self) -> SharedCallDeadline {
        self.0.clone()
    }
    pub(crate) fn clear(&self) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}
impl Drop for BudgetGuard {
    fn drop(&mut self) {
        self.clear();
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = std::mem::take(&mut self.0));
    }
}
pub(crate) fn scope(capture: Option<Arc<Capture>>) -> Scope {
    Scope(CURRENT.with(|current| current.replace(capture)))
}
pub(crate) fn current() -> Option<Arc<Capture>> {
    CURRENT.with(|current| current.borrow().clone())
}

impl Capture {
    pub(crate) fn start(ctx: &crate::context::AppContext) -> Option<Arc<Self>> {
        let mode = ctx.config().bash.changed_files;
        if mode == BashChangedFiles::Off {
            return None;
        }
        let root = ctx.config().project_root.clone()?;
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let journal = ctx
            .watcher_counters()
            .bash_changes
            .get_or_init(|| Arc::new(Journal::default()))
            .clone();
        let mut capture = Self::begin(root, journal, mode, ctx.watcher_runtime_active());
        Arc::get_mut(&mut capture).unwrap().flush = Some(ctx.watcher_counters().bash_flush.clone());
        if let Some(deadline) = crate::bash_background::changed_files_deadline_for_spawn() {
            Arc::get_mut(&mut capture).unwrap().call_deadline = deadline;
        }
        Some(capture)
    }
    fn begin(
        root: PathBuf,
        journal: Arc<Journal>,
        mode: BashChangedFiles,
        watcher_active: bool,
    ) -> Arc<Self> {
        // Git probes share one deadline and bounded output. Untracked paths are
        // classified from watcher create events, not an up-front directory scan.
        let git = if mode == BashChangedFiles::Diff {
            GitStart::read(&root)
        } else {
            None
        };
        let state = journal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (sequence, gaps) = (state.sequence, state.gaps);
        drop(state);
        Arc::new(Self {
            root,
            journal,
            sequence,
            gaps,
            mode,
            watcher_active,
            flush: None,
            call_deadline: Arc::new(Mutex::new(None)),
            git,
            end: OnceLock::new(),
            report: OnceLock::new(),
        })
    }
    pub(crate) fn finish(&self) -> &Report {
        self.report.get_or_init(|| {
            let deadline = Instant::now() + crate::watcher_backend::flush::BASH_FLUSH_LIMIT;
            let deadline = self
                .call_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .map_or(deadline, |call| call.min(deadline));
            let caught_up = self.watcher_active
                && self
                    .flush
                    .as_ref()
                    .is_none_or(|flush| flush.wait(&self.root, deadline));
            self.seal();
            self.render(caught_up)
        })
    }
    pub(crate) fn budget(self: &Arc<Self>, deadline: Option<Instant>) -> BudgetGuard {
        *self
            .call_deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = deadline;
        BudgetGuard(self.call_deadline.clone())
    }
    pub(crate) fn seal(&self) {
        self.end.get_or_init(|| {
            let state = self
                .journal
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (state.sequence, state.gaps)
        });
    }
    fn render(&self, caught_up: bool) -> Report {
        let state = self
            .journal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (end_sequence, end_gaps) = *self.end.get().expect("capture sealed before rendering");
        let overflow = end_gaps != self.gaps
            || state
                .events
                .front()
                .is_some_and(|(sequence, _, _)| *sequence > self.sequence + 1);
        let mut paths = BTreeMap::new();
        for (sequence, path, created) in &state.events {
            if *sequence > self.sequence && *sequence <= end_sequence {
                if let Ok(relative) = path.strip_prefix(&self.root) {
                    if !relative.as_os_str().is_empty()
                        && !relative.components().any(|c| {
                            matches!(
                                c.as_os_str().to_str(),
                                Some(".git" | "target" | "node_modules")
                            )
                        })
                    {
                        if !path.is_dir() {
                            paths.entry(relative.to_path_buf()).or_insert(*created);
                        }
                    }
                }
            }
        }
        drop(state);
        let mut incomplete = Vec::new();
        if overflow {
            incomplete.push(OVERFLOW.to_string());
        }
        if !caught_up {
            incomplete.push(NOT_CAUGHT_UP.to_string());
        }
        let total = paths.len();
        let mut files = Vec::new();
        let mut serialized_files_bytes = 0;
        let mut text = HEADER.to_string();
        let mut cut = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        for (path, created) in paths {
            if files.len() == MAX_FILES {
                cut = true;
                break;
            }
            let absolute = self.root.join(&path);
            if absolute.is_dir() {
                continue;
            }
            let exists = absolute.symlink_metadata().is_ok();
            let mut file = ChangedFile {
                path: path.to_string_lossy().into_owned(),
                status: if !exists {
                    "deleted"
                } else if created {
                    "added"
                } else {
                    "modified"
                },
                diff: None,
                reason: None,
                diff_lines_list_envelope: None,
            };
            if self.mode == BashChangedFiles::Diff {
                if let Some(git) = &self.git {
                    self.diff_file(git, &path, created, exists, deadline, &mut file);
                }
            }
            let mut rendered = format!(
                "\n  {} {}",
                file.status,
                serde_json::to_string(&file.path).unwrap_or_default()
            );
            // Ordinary paths stay readable; control characters remain escaped.
            if !file.path.chars().any(char::is_control) {
                rendered = format!("\n  {} {}", file.status, file.path);
            }
            if let Some(reason) = &file.reason {
                rendered.push_str(&format!(" ({reason})"));
            }
            if let Some(diff) = &file.diff {
                rendered.push('\n');
                rendered.push_str(diff);
            }
            let file_bytes =
                serde_json::to_vec(&file).map_or(MAX_REPORT_BYTES, |bytes| bytes.len() + 1);
            let text_bytes = serde_json::to_vec(&format!("{text}{rendered}"))
                .map_or(MAX_REPORT_BYTES, |bytes| bytes.len());
            if text_bytes + serialized_files_bytes + file_bytes > MAX_REPORT_BYTES - 2048 {
                cut = true;
                break;
            }
            serialized_files_bytes += file_bytes;
            text.push_str(&rendered);
            files.push(file);
        }
        if files.is_empty() {
            text.push_str("\n  (no file changes observed)");
        }
        if self.mode == BashChangedFiles::Diff && self.git.is_none() {
            text.push('\n');
            text.push_str(
                if self.root.ancestors().any(|path| path.join(".git").exists()) {
                    GIT_UNAVAILABLE
                } else {
                    OUTSIDE_GIT
                },
            );
        }
        for reason in &incomplete {
            text.push('\n');
            text.push_str(reason);
        }
        let mut list_envelope = crate::list_surfaces::bash::build_changed_files_envelope(
            files.len(),
            total,
            false,
            overflow || !caught_up,
        );
        if cut && list_envelope.reason.is_none() {
            list_envelope.reason = Some(Reason::Cap);
            list_envelope.causes.push(Reason::Cap);
        }
        if list_envelope.reason.is_some() {
            let trailer = crate::list_surfaces::bash::envelope_trailer(&list_envelope);
            text.push('\n');
            text.push_str(&trailer);
        }
        Report {
            text,
            files,
            incomplete,
            changed_files_list_envelope: list_envelope,
        }
    }
    fn diff_file(
        &self,
        git: &GitStart,
        path: &Path,
        _created: bool,
        exists: bool,
        deadline: Instant,
        file: &mut ChangedFile,
    ) {
        let absolute = self.root.join(path);
        if path.to_str().is_none() {
            file.reason = Some("no diff: path is not valid UTF-8".into());
            return;
        }
        if git.dirty.iter().any(|dirty| absolute.starts_with(dirty)) {
            file.status = if exists { "modified" } else { "deleted" };
            file.reason = Some(DIRTY.into());
            return;
        }
        let Ok(relative) = absolute.strip_prefix(&git.directory) else {
            file.reason = Some("no diff: path is outside the git root".into());
            return;
        };
        let spec = format!(
            "{}:{}",
            git.head,
            relative.to_string_lossy().replace('\\', "/")
        );
        let before = crate::revision_read::bounded_git_bytes(
            &git.directory,
            &["show", &spec],
            deadline,
            MAX_FILE_BYTES,
        );
        let before = match before {
            Some(before) => {
                file.status = if exists { "modified" } else { "deleted" };
                before
            }
            None if crate::revision_read::bounded_git_bytes(
                &git.directory,
                &[
                    "ls-tree",
                    "-z",
                    &git.head,
                    "--",
                    &relative.to_string_lossy(),
                ],
                deadline,
                4096,
            )
            .is_some_and(|entry| entry.is_empty()) =>
            {
                file.status = if exists { "added" } else { "deleted" };
                Vec::new()
            }
            None => {
                file.reason = Some("no diff: no clean git before-image is available (untracked, unreadable or too large)".into());
                return;
            }
        };
        if !before.is_empty() {
            file.status = if exists { "modified" } else { "deleted" };
        }
        let after = if exists {
            if !std::fs::canonicalize(&absolute).is_ok_and(|path| path.starts_with(&self.root)) {
                file.reason = Some("no diff: path resolves outside the project".into());
                return;
            }
            match std::fs::symlink_metadata(&absolute) {
                Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_FILE_BYTES as u64 => {
                    use std::io::Read;
                    let mut bytes = Vec::new();
                    if std::fs::File::open(&absolute)
                        .and_then(|f| f.take((MAX_FILE_BYTES + 1) as u64).read_to_end(&mut bytes))
                        .is_err()
                        || bytes.len() > MAX_FILE_BYTES
                    {
                        file.reason = Some("no diff: file is unreadable or too large".into());
                        return;
                    }
                    bytes
                }
                _ => {
                    file.reason =
                        Some("no diff: file is too large or is not a regular file".into());
                    return;
                }
            }
        } else {
            Vec::new()
        };
        let (Ok(before), Ok(after)) = (std::str::from_utf8(&before), std::str::from_utf8(&after))
        else {
            file.reason = Some("no diff: binary file".into());
            return;
        };
        if before.contains('\0') || after.contains('\0') {
            file.reason = Some("no diff: binary file".into());
            return;
        }
        let name = file.path.replace(['\n', '\r', '\t'], "?");
        let diff = similar::TextDiff::configure()
            .timeout(Duration::from_millis(50))
            .diff_lines(before, after);
        let rendered = diff
            .unified_diff()
            .header(
                &if file.status == "added" {
                    "/dev/null".into()
                } else {
                    format!("a/{name}")
                },
                &if file.status == "deleted" {
                    "/dev/null".into()
                } else {
                    format!("b/{name}")
                },
            )
            .to_string();
        let total = rendered.lines().count();
        let mut shown = 0;
        let mut capped = String::new();
        for line in rendered.lines() {
            if shown == MAX_DIFF_LINES || capped.len() + line.len() + 1 > MAX_REPORT_BYTES / 2 {
                break;
            }
            capped.push_str(line);
            capped.push('\n');
            shown += 1;
        }
        if shown < total {
            let envelope =
                crate::list_surfaces::bash::build_changed_files_envelope(shown, total, true, false);
            capped.push_str(&crate::list_surfaces::bash::envelope_trailer(&envelope));
            capped.push('\n');
            file.diff_lines_list_envelope = Some(envelope);
        }
        file.diff = Some(capped);
    }
}
impl GitStart {
    fn read(root: &Path) -> Option<Self> {
        let deadline = Instant::now() + Duration::from_millis(250);
        let run = |args: &[&str]| {
            crate::revision_read::bounded_git_bytes(root, args, deadline, MAX_REPORT_BYTES)
        };
        let directory = PathBuf::from(
            String::from_utf8(run(&["rev-parse", "--show-toplevel"])?)
                .ok()?
                .trim_end(),
        );
        let head = String::from_utf8(run(&["rev-parse", "--verify", "HEAD"])?)
            .ok()?
            .trim()
            .to_string();
        // Compare both index and worktree against the pinned HEAD, including
        // staged changes. Never refresh or write the caller's index.
        let bytes = run(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            "--no-renames",
        ])?;
        let dirty = bytes
            .split(|b| *b == 0)
            .filter(|p| p.len() >= 3)
            .map(|p| directory.join(String::from_utf8_lossy(&p[3..]).as_ref()))
            .collect();
        Some(Self {
            directory,
            head,
            dirty,
        })
    }
}

#[cfg(test)]
#[path = "bash_changed_files_test.rs"]
mod tests;
