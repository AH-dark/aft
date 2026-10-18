//! Process-local single-flight for complete, whole-project applicability walks.
//! Physical roots, not Git identities, are the cache boundary: linked worktrees
//! can have different untracked sources and installed TypeScript versions even
//! when their HEAD commits match.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use parking_lot::{Condvar, Mutex};

use super::manager::{ApplicabilityResolutionError, ApplicabilityWalk};
use crate::config::Config;

type Key = (PathBuf, Vec<u8>);

#[derive(Default)]
struct State {
    generation: u64,
    running: Option<Arc<RunningScan>>,
    result: Option<ApplicabilityWalk>,
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
                return Ok(result.clone());
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
            let result = scan();
            let mut state = flight.state.lock();
            // Under continuous edits, returning one uncached scan preserves the
            // uncached walk's progress. Only unchanged scans may serve new binds.
            if state.generation == running.generation {
                if let Ok(walk) = &result {
                    state.result = Some(walk.clone());
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
