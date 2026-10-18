//! Opt-in investigation timing; no publication policy depends on these samples.

use std::path::{Path, PathBuf};
use std::time::Instant;

pub(super) struct PublicationTiming {
    enabled: bool,
    path: PathBuf,
    started: Instant,
}

impl PublicationTiming {
    pub(super) fn new(path: &Path) -> Self {
        Self {
            enabled: std::env::var_os("AFT_VIEWS_PERF_HUNT").is_some(),
            path: path.to_owned(),
            started: Instant::now(),
        }
    }

    pub(super) fn phase(&mut self, phase: &str) {
        let now = Instant::now();
        #[cfg(test)]
        PHASE_OBSERVER.with(|observer| {
            if let Some(observer) = observer.borrow().as_ref() {
                observer(phase);
            }
        });
        if self.enabled {
            log::info!(
                "view_perf_hunt path={} phase={} us={}",
                self.path.display(),
                phase,
                now.duration_since(self.started).as_micros()
            );
        }
        self.started = now;
    }
}

#[cfg(test)]
thread_local! {
    static PHASE_OBSERVER: std::cell::RefCell<Option<Box<dyn Fn(&str)>>> =
        const { std::cell::RefCell::new(None) };
}

/// Removes the observer installed by [`observe_publication_phases_for_test`].
#[cfg(test)]
pub(crate) struct PhaseObserverGuard;

#[cfg(test)]
impl Drop for PhaseObserverGuard {
    fn drop(&mut self) {
        PHASE_OBSERVER.with(|observer| observer.borrow_mut().take());
    }
}

/// Lets a test see each publication step as it completes on this thread, so a
/// test that times out mid-publication can name the step it was stuck in.
/// The observer is thread-local: publications on other threads never call it.
#[cfg(test)]
pub(crate) fn observe_publication_phases_for_test(
    observer: impl Fn(&str) + 'static,
) -> PhaseObserverGuard {
    PHASE_OBSERVER.with(|slot| *slot.borrow_mut() = Some(Box::new(observer)));
    PhaseObserverGuard
}
