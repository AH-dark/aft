//! Process-local single-flight for complete, whole-project applicability walks.
//! Physical roots, not Git identities, are the cache boundary: linked worktrees
//! can have different untracked sources and installed TypeScript versions even
//! when their HEAD commits match.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime};

use parking_lot::{Condvar, Mutex};

use super::manager::{ApplicabilityResolutionError, ApplicabilityWalk};
use crate::config::Config;

type Key = (PathBuf, Vec<u8>);

#[derive(PartialEq, Eq)]
enum PathState {
    Missing,
    Present {
        identity: Option<(u64, u64)>,
        modified: Option<SystemTime>,
        created: Option<SystemTime>,
        len: u64,
        is_dir: bool,
    },
    Unreadable,
}

impl PathState {
    fn read(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(metadata) => Self::Present {
                identity: file_identity(path, &metadata),
                modified: metadata.modified().ok(),
                created: metadata.created().ok(),
                len: metadata.len(),
                is_dir: metadata.is_dir(),
            },
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Self::Missing
            }
            Err(_) => Self::Unreadable,
        }
    }
}

#[cfg(unix)]
fn file_identity(_path: &Path, metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn file_identity(path: &Path, _metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the open file owns a valid handle and `info` is writable. A
    // successful call initializes the structure before it is read below.
    let info = unsafe {
        if GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) == 0 {
            return None;
        }
        info.assume_init()
    };
    Some((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_path: &Path, _metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

struct Dependencies {
    id: u64,
    root: PathBuf,
    paths: HashMap<PathBuf, PathState>,
}

thread_local! {
    // Root discovery is synchronous, including registry filters. Capture its
    // probes on this thread without changing the public root-lookup APIs.
    static DEPENDENCIES: RefCell<Option<Dependencies>> = const { RefCell::new(None) };
}

/// Record a selection input before probing it. Files beneath the watched root
/// are invalidated by watcher batches; ancestors need bounded stat rechecks.
pub(super) fn observe_path(path: &Path) {
    DEPENDENCIES.with_borrow_mut(|capture| {
        if let Some(capture) = capture {
            let path = crate::inspect::job::normalize_path(path);
            if !path.starts_with(&capture.root) {
                capture
                    .paths
                    .entry(path.clone())
                    .or_insert_with(|| PathState::read(&path));
            }
        }
    });
}

pub(super) fn is_external_input(path: &Path) -> bool {
    DEPENDENCIES.with_borrow(|capture| {
        capture
            .as_ref()
            .is_some_and(|capture| !path.starts_with(&capture.root))
    })
}

pub(super) fn capture_id() -> Option<u64> {
    DEPENDENCIES.with_borrow(|capture| capture.as_ref().map(|capture| capture.id))
}

fn observe_ancestor_ignores(root: &Path) {
    // The ignore walker reads parent rules itself, outside the root-lookup
    // helpers instrumented below. Include absent rules so creating one counts.
    for dir in root.ancestors().skip(1) {
        for name in [
            ".ignore",
            ".gitignore",
            ".aftignore",
            ".git",
            ".jj",
            ".git/info/exclude",
        ] {
            observe_path(&dir.join(name));
        }
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        observe_path(&home.join(".gitconfig"));
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        observe_path(&config.join("git/config"));
    }
    if let Some(path) = ignore::gitignore::gitconfig_excludes_path() {
        observe_path(&path);
    }
}

struct Capture(Option<Dependencies>);

impl Capture {
    fn start(root: &Path) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(DEPENDENCIES.replace(Some(Dependencies {
            id,
            root: root.into(),
            paths: HashMap::new(),
        })))
    }

    fn finish(self) -> HashMap<PathBuf, PathState> {
        DEPENDENCIES.with_borrow_mut(|capture| {
            std::mem::take(&mut capture.as_mut().expect("active capture").paths)
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        DEPENDENCIES.set(self.0.take());
    }
}

fn unchanged(paths: &HashMap<PathBuf, PathState>) -> bool {
    paths.iter().all(|(path, state)| {
        matches!(
            state,
            PathState::Missing
                | PathState::Present {
                    identity: Some(_),
                    ..
                }
        ) && *state == PathState::read(path)
    })
}

#[derive(Default)]
struct State {
    generation: u64,
    running: Option<Arc<RunningScan>>,
    result: Option<ApplicabilityWalk>,
    dependencies: HashMap<PathBuf, PathState>,
}

#[derive(Default)]
struct Flight {
    state: Mutex<State>,
}

#[derive(Default)]
struct Completion {
    finished: bool,
    result: Option<Result<ApplicabilityWalk, ApplicabilityResolutionError>>,
}

#[derive(Default)]
struct RunningScan {
    generation: u64,
    completion: Mutex<Completion>,
    changed: Condvar,
}

struct ScanGuard<'a> {
    flight: &'a Flight,
    scan: Arc<RunningScan>,
}

impl Drop for ScanGuard<'_> {
    fn drop(&mut self) {
        // Unwinding must release both the root's reservation and callers parked
        // on this scan. A missing result tells those callers to try a new scan.
        let mut state = self.flight.state.lock();
        if state
            .running
            .as_ref()
            .is_some_and(|scan| Arc::ptr_eq(scan, &self.scan))
        {
            state.running = None;
        }
        self.scan.completion.lock().finished = true;
        self.scan.changed.notify_all();
    }
}

#[derive(Default)]
pub(super) struct ApplicabilityCache {
    entries: Mutex<HashMap<Key, Arc<Flight>>>,
    #[cfg(test)]
    waiter_joined: (Mutex<bool>, Condvar),
}

pub(super) fn shared() -> &'static ApplicabilityCache {
    static CACHE: OnceLock<ApplicabilityCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

impl ApplicabilityCache {
    #[cfg(test)]
    pub(super) fn wait_for_waiter_for_test(&self, deadline: Instant) -> bool {
        let (joined, changed) = &self.waiter_joined;
        let mut joined = joined.lock();
        while !*joined {
            if changed.wait_until(&mut joined, deadline).timed_out() {
                return false;
            }
        }
        true
    }

    #[cfg(test)]
    fn note_waiter_for_test(&self) {
        *self.waiter_joined.0.lock() = true;
        self.waiter_joined.1.notify_all();
    }

    pub(super) fn walk(
        &self,
        root: &Path,
        config: &Config,
        deadline: Instant,
        mut scan: impl FnMut() -> Result<ApplicabilityWalk, ApplicabilityResolutionError>,
    ) -> Result<ApplicabilityWalk, ApplicabilityResolutionError> {
        // Configuration controls custom markers, disabled servers and ignores.
        // Key the full snapshot so no future selector can silently escape the key.
        let mut config_key = serde_json::to_value(config).expect("config serializes");
        // Only these fields are sets. Server arguments, marker priority and
        // other ordered lists must retain their configured order.
        for field in [
            "disabled_lsp",
            "lsp_auto_install_binaries",
            "lsp_inflight_installs",
        ] {
            if let Some(serde_json::Value::Array(values)) = config_key.get_mut(field) {
                values.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            }
        }
        config_key.sort_all_objects();
        let key = (
            root.to_path_buf(),
            serde_json::to_vec(&config_key).expect("config serializes"),
        );
        let flight = {
            let mut entries = self.entries.lock();
            if entries.len() >= 64 {
                // Never evict a flight with a caller: doing so would admit a
                // second concurrent scan of that root and configuration.
                entries.retain(|_, flight| Arc::strong_count(flight) > 1);
            }
            Arc::clone(entries.entry(key).or_default())
        };
        loop {
            let mut state = flight.state.lock();
            if Instant::now() >= deadline {
                return Err(ApplicabilityResolutionError::RequestDeadline { root: root.into() });
            }
            if let Some(result) = &state.result {
                if unchanged(&state.dependencies) {
                    return Ok(result.clone());
                }
                state.result = None;
                state.generation = state.generation.wrapping_add(1);
            }
            if let Some(running) = &state.running {
                let running = Arc::clone(running);
                let joined_generation = running.generation == state.generation;
                drop(state);
                let mut completion = running.completion.lock();
                #[cfg(test)]
                self.note_waiter_for_test();
                while !completion.finished {
                    if running
                        .changed
                        .wait_until(&mut completion, deadline)
                        .timed_out()
                    {
                        return Err(ApplicabilityResolutionError::RequestDeadline {
                            root: root.into(),
                        });
                    }
                }
                // Retain a per-scan completion for its original waiter cohort.
                // Later binds must not consume a result invalidated before they
                // arrived, even if they waited for that scan to release the root.
                if joined_generation {
                    if let Some(result) = &completion.result {
                        return result.clone();
                    }
                }
                continue;
            }
            let running = Arc::new(RunningScan {
                generation: state.generation,
                ..RunningScan::default()
            });
            state.running = Some(Arc::clone(&running));
            drop(state);
            let guard = ScanGuard {
                flight: &flight,
                scan: Arc::clone(&running),
            };
            let capture = Capture::start(root);
            observe_ancestor_ignores(root);
            let result = scan();
            let dependencies = capture.finish();
            let mut state = flight.state.lock();
            // Under continuous edits, returning one uncached scan preserves the
            // uncached walk's progress. Only unchanged scans may serve new binds.
            if state.generation == running.generation && unchanged(&dependencies) {
                if let Ok(walk) = &result {
                    state.result = Some(walk.clone());
                    state.dependencies = dependencies;
                }
            }
            drop(state);
            running.completion.lock().result = Some(result.clone());
            drop(guard);
            return result;
        }
    }

    pub(super) fn invalidate(&self, root: &Path) {
        for ((cached_root, _), flight) in self.entries.lock().iter() {
            // Nested routes can observe the same change through different
            // watchers. Ancestor/descendant invalidation covers both routes.
            if cached_root.starts_with(root) || root.starts_with(cached_root) {
                let mut state = flight.state.lock();
                state.generation = state.generation.wrapping_add(1);
                state.result = None;
            }
        }
    }
}
