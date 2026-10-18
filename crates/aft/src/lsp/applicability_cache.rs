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
    running: bool,
    result: Option<ApplicabilityWalk>,
}

#[derive(Default)]
struct Flight {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
pub(super) struct ApplicabilityCache {
    entries: Mutex<HashMap<Key, Arc<Flight>>>,
}

pub(super) fn shared() -> &'static ApplicabilityCache {
    static CACHE: OnceLock<ApplicabilityCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

impl ApplicabilityCache {
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
            if state.running {
                flight.changed.wait_until(&mut state, deadline);
                continue;
            }
            state.running = true;
            let generation = state.generation;
            drop(state);
            let result = scan();
            let mut state = flight.state.lock();
            state.running = false;
            flight.changed.notify_all();
            // A watcher event during a scan invalidates that scan too, not
            // just the result of the next bind. Retry against the new tree.
            if state.generation != generation {
                continue;
            }
            if let Ok(walk) = &result {
                state.result = Some(walk.clone());
            }
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
