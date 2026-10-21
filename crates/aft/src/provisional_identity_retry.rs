//! Background retry for a root whose bind found aft.db busy.
//!
//! A bind reads older standing-root pins from aft.db without waiting, so a
//! database held by another writer leaves the root's identity provisional:
//! configure turns the root's indexes off rather than guess an identity a
//! later bind might contradict. Without a retry that state lasts until the
//! folder binds again, which may never happen: a daemon restart binds dozens
//! of roots at once, so several can meet a busy database, and a long-lived
//! session keeps its bind for hours.
//!
//! So a successful bind that leaves the identity provisional only because
//! aft.db was busy arms a retry here. The configure-tail maintenance drain
//! (the executor's maintenance lane under the attached daemon, the idle
//! drain in the standalone loop) polls it with bounded backoff. Each due
//! attempt first checks, without waiting, whether the pins are readable; once
//! they are, it replays the same configure request. That replay is the bind
//! the folder would otherwise need: it settles and saves the identity and
//! enables the indexes exactly as a successful first bind does.
//!
//! The retry never outlives the bind it belongs to. A newer configure replaces
//! it (re-arming only if that bind is provisional for the same reason), an
//! unbound or quiesced root cancels it, and a root whose identity is already
//! settled drops it without replaying anything, so a retry can never re-key a
//! settled root. A provisional identity caused by unreadable Git (for example
//! a dubious-ownership refusal) never arms one: retrying cannot change Git's
//! answer, so that root still waits for its next bind.

use std::time::{Duration, Instant};

use crate::context::AppContext;
use crate::protocol::RawRequest;
use crate::search_index::{PROVISIONAL_IDENTITY_EXHAUSTED_REASON, PROVISIONAL_IDENTITY_REASON};

/// Delay before the first retry after the bind.
const FIRST_DELAY: Duration = Duration::from_millis(250);
/// Each further delay doubles up to this ceiling.
const MAX_DELAY: Duration = Duration::from_secs(10);
/// Stop retrying once this long has passed since the bind. The exhausted
/// degraded reason names this window, so keep both in step.
const GIVE_UP_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy)]
struct Timing {
    first_delay: Duration,
    max_delay: Duration,
    give_up_after: Duration,
}

const PRODUCTION_TIMING: Timing = Timing {
    first_delay: FIRST_DELAY,
    max_delay: MAX_DELAY,
    give_up_after: GIVE_UP_AFTER,
};

#[cfg(test)]
thread_local! {
    static TIMING_FOR_TEST: std::cell::Cell<Option<Timing>> = const { std::cell::Cell::new(None) };
}

fn timing() -> Timing {
    #[cfg(test)]
    if let Some(timing) = TIMING_FOR_TEST.with(std::cell::Cell::get) {
        return timing;
    }
    PRODUCTION_TIMING
}

/// Run `body` with retry delays and the give-up window replaced on this
/// thread, so a unit test can make an attempt due at once or exhaust it.
#[cfg(test)]
pub(crate) fn with_timing_for_test<T>(
    first_delay: Duration,
    give_up_after: Duration,
    body: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Timing>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TIMING_FOR_TEST.with(|slot| slot.set(self.0));
        }
    }
    let previous = TIMING_FOR_TEST.with(|slot| {
        slot.replace(Some(Timing {
            first_delay,
            max_delay: first_delay,
            give_up_after,
        }))
    });
    let _restore = Restore(previous);
    body()
}

/// Delay before attempt number `attempts` (zero-based): the first delay,
/// doubling, capped at the maximum.
fn delay_before(attempts: u32, timing: Timing) -> Duration {
    timing
        .first_delay
        .saturating_mul(1u32 << attempts.min(16))
        .min(timing.max_delay)
}

struct PendingRetry {
    /// The configure request whose bind left the identity provisional.
    request: RawRequest,
    /// The configure generation that bind published. Any later configure
    /// moves it, which makes this retry stale.
    generation: u64,
    armed_at: Instant,
    attempts: u32,
    next_attempt_at: Instant,
    exhausted: bool,
}

/// Per-root retry state, held by the root's [`AppContext`].
#[derive(Default)]
pub(crate) struct RetrySlot(parking_lot::Mutex<Option<PendingRetry>>);

impl RetrySlot {
    /// True when an armed retry's next attempt is due. Never waits: a held
    /// slot reads as not due, and the next maintenance tick probes again.
    pub(crate) fn is_due(&self) -> bool {
        self.0.try_lock().is_some_and(|slot| {
            slot.as_ref()
                .is_some_and(|retry| !retry.exhausted && Instant::now() >= retry.next_attempt_at)
        })
    }

    /// True while a retry is armed and has not given up.
    #[cfg(test)]
    pub(crate) fn is_armed(&self) -> bool {
        self.0.lock().as_ref().is_some_and(|retry| !retry.exhausted)
    }

    #[cfg(test)]
    pub(crate) fn attempts_for_test(&self) -> Option<u32> {
        self.0.lock().as_ref().map(|retry| retry.attempts)
    }
}

/// Record what a finished configure means for the retry. Called for every
/// configure, including a replay started by [`drain`].
pub(crate) fn note_configure_outcome(ctx: &AppContext, request: &RawRequest, success: bool) {
    if !success {
        // A failed configure publishes nothing, so the bind the retry belongs
        // to still stands. If the failed attempt advanced the generation, the
        // next drain finds the retry stale and drops it.
        return;
    }
    let busy_at_bind = ctx
        .degraded_reasons()
        .iter()
        .any(|reason| reason == PROVISIONAL_IDENTITY_REASON);
    let mut slot = ctx.provisional_identity_retry().0.lock();
    if !busy_at_bind {
        // Settled, provisional for a reason a retry cannot fix, or not
        // needing an identity at all: nothing to retry.
        *slot = None;
        return;
    }
    let now = Instant::now();
    if slot.is_none() {
        crate::slog_info!(
            "identity provisional (aft.db busy) for root={}; retrying in the background",
            ctx.canonical_cache_root_opt()
                .map(|root| root.display().to_string())
                .unwrap_or_default()
        );
    }
    *slot = Some(PendingRetry {
        request: request.clone(),
        generation: ctx.configure_generation(),
        armed_at: now,
        attempts: 0,
        next_attempt_at: now + delay_before(0, timing()),
        exhausted: false,
    });
}

/// Stop retrying, for a root that unbound or quiesced. Returns whether a
/// retry was armed.
pub(crate) fn cancel(ctx: &AppContext) -> bool {
    ctx.provisional_identity_retry().0.lock().take().is_some()
}

/// Run one due retry attempt. Returns true when it replayed the bind, which
/// queued that bind's configure tail on the context.
///
/// Runs only from the configure-tail maintenance drain, never from an
/// interactive request.
pub(crate) fn drain(ctx: &AppContext) -> bool {
    let slot = ctx.provisional_identity_retry();
    if !slot.is_due() {
        return false;
    }
    let Some(mut retry) = slot.0.lock().take() else {
        return false;
    };
    let root = ctx.canonical_cache_root_opt();
    // A root with no bound route does no indexing.
    if ctx.subc_unbound_quiesced() {
        return false;
    }
    // Only the bind that armed the retry may be replayed, and only while its
    // identity is still provisional because aft.db was busy. Anything else
    // means a newer configure owns the root, or the identity has settled and
    // must not be keyed again.
    let still_busy_provisional = ctx
        .degraded_reasons()
        .iter()
        .any(|reason| reason == PROVISIONAL_IDENTITY_REASON);
    let settled = root
        .as_deref()
        .is_some_and(|root| ctx.cached_artifact_cache_key(root).is_some());
    if retry.generation != ctx.configure_generation() || !still_busy_provisional || settled {
        return false;
    }
    let Some(root) = root else {
        return false;
    };
    let storage = crate::bash_background::storage_dir(ctx.config().storage_dir.as_deref());
    let timing = timing();
    if !crate::root_identity::legacy_pins_readable(&root, &storage) {
        retry.attempts = retry.attempts.saturating_add(1);
        reschedule(ctx, &root, retry, timing);
        return false;
    }

    let attempts = retry.attempts.saturating_add(1);
    let response = crate::commands::configure::handle_configure(&retry.request, ctx);
    let mut rearmed = slot.0.lock();
    match rearmed.take() {
        // The database was taken again between the check and the replay:
        // keep the original window and backoff instead of starting over.
        Some(mut next) => {
            next.armed_at = retry.armed_at;
            next.attempts = attempts;
            drop(rearmed);
            reschedule(ctx, &root, next, timing);
        }
        None if response.success => {
            crate::slog_info!(
                "identity settled for root={} after aft.db became free ({attempts} attempt(s), {} ms after bind); indexes enabled",
                root.display(),
                retry.armed_at.elapsed().as_millis()
            );
        }
        None => {
            // The replay failed outright, so the provisional bind still
            // stands. Retry it on the same schedule.
            drop(rearmed);
            retry.attempts = attempts;
            crate::slog_warn!(
                "identity retry for root={} could not replay the bind: {}",
                root.display(),
                response.data
            );
            reschedule(ctx, &root, retry, timing);
        }
    }
    true
}

/// Store `retry` with its next attempt time, or give up once its window has
/// passed and say so in the root's degraded reasons.
fn reschedule(ctx: &AppContext, root: &std::path::Path, mut retry: PendingRetry, timing: Timing) {
    let now = Instant::now();
    if now.duration_since(retry.armed_at) >= timing.give_up_after {
        retry.exhausted = true;
        ctx.replace_degraded_reason(
            PROVISIONAL_IDENTITY_REASON,
            PROVISIONAL_IDENTITY_EXHAUSTED_REASON,
        );
        crate::slog_warn!(
            "identity retry gave up for root={} after {} attempt(s): aft.db stayed busy; indexes stay off until the folder binds again",
            root.display(),
            retry.attempts
        );
    } else {
        retry.next_attempt_at = now + delay_before(retry.attempts, timing);
    }
    *ctx.provisional_identity_retry().0.lock() = Some(retry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_the_first_delay_to_the_ceiling() {
        let delays: Vec<_> = (0..8)
            .map(|attempt| delay_before(attempt, PRODUCTION_TIMING))
            .collect();
        assert_eq!(
            delays,
            [250, 500, 1_000, 2_000, 4_000, 8_000, 10_000, 10_000].map(Duration::from_millis)
        );
        assert_eq!(delay_before(u32::MAX, PRODUCTION_TIMING), MAX_DELAY);
    }

    #[test]
    fn exhausted_reason_names_the_give_up_window() {
        let minutes = GIVE_UP_AFTER.as_secs() / 60;
        assert!(
            PROVISIONAL_IDENTITY_EXHAUSTED_REASON.contains(&format!("for {minutes} minutes")),
            "{PROVISIONAL_IDENTITY_EXHAUSTED_REASON}"
        );
    }
}
