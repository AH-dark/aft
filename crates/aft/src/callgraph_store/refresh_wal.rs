//! Checkpoint scheduling for views-off refreshes. The connection observes WAL
//! commits; the worker decides when queued writes can yield to maintenance.

use super::*;

const SOFT_BYTES: u64 = 64 * 1024 * 1024;
const HARD_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct Policy {
    observed_bytes: u64,
    soft_bytes: u64,
    hard_bytes: u64,
    restart_pending: bool,
    pub(super) restart_attempts: u64,
    pub(super) restart_busy: u64,
    pub(super) passive_attempts: u64,
    #[cfg(test)]
    events: Vec<&'static str>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            observed_bytes: 0,
            soft_bytes: SOFT_BYTES,
            hard_bytes: HARD_BYTES,
            restart_pending: false,
            restart_attempts: 0,
            restart_busy: 0,
            passive_attempts: 0,
            #[cfg(test)]
            events: Vec::new(),
        }
    }
}

impl Policy {
    pub(super) fn before_begin(&mut self, conn: &TrackedConnection) {
        if self.observed_bytes <= self.hard_bytes && !self.restart_pending {
            return;
        }
        self.restart_attempts += 1;
        // A reader may pin this WAL for much longer than a write transaction.
        // Maintenance must yield immediately rather than stall this root and
        // every other root sharing the refresh worker. Restore the writer's
        // existing timeout even when SQLite reports a checkpoint error.
        let checkpoint = (|| -> rusqlite::Result<_> {
            let timeout_ms: u64 =
                conn.pragma_query_value(None, "busy_timeout", |row| row.get(0))?;
            conn.busy_timeout(Duration::ZERO)?;
            let result = conn.checkpoint_wal_as(
                WalCheckpointMode::Restart,
                crate::write_ledger::Domain::CallgraphCheckpoint,
            );
            conn.busy_timeout(Duration::from_millis(timeout_ms))?;
            result
        })();
        let complete = match checkpoint {
            Ok(result) => result.busy == 0 && result.log_frames == result.checkpointed_frames,
            Err(error) => {
                crate::slog_warn!("callgraph refresh pre-BEGIN RESTART checkpoint failed: {error}");
                false
            }
        };
        self.restart_pending = !complete;
        if complete {
            self.observed_bytes = 0;
        } else {
            self.restart_busy += 1;
            crate::slog_warn!("callgraph refresh RESTART busy or partial; retrying before next refresh (attempts={}, busy={})", self.restart_attempts, self.restart_busy);
        }
        #[cfg(test)]
        self.events.push(if complete { "restart" } else { "busy" });
    }

    pub(super) fn after_commit(&mut self, conn: &TrackedConnection) {
        self.observed_bytes = conn.observed_wal_bytes();
        #[cfg(test)]
        self.events.push("commit");
    }

    pub(super) fn when_unqueued(&mut self, conn: &TrackedConnection, queued: bool) {
        if queued || self.observed_bytes <= self.soft_bytes {
            return;
        }
        self.passive_attempts += 1;
        #[cfg(test)]
        self.events.push("passive");
        match conn.checkpoint_wal_as(
            WalCheckpointMode::Passive,
            crate::write_ledger::Domain::CallgraphCheckpoint,
        ) {
            Ok(result) if result.busy == 0 && result.log_frames == result.checkpointed_frames => {
                // A blocked hard-cap attempt must still be retried as RESTART:
                // PASSIVE can backfill without securing permission to reuse.
            }
            Ok(result) => crate::slog_warn!(
                "callgraph refresh PASSIVE checkpoint busy or partial: {:?}",
                result
            ),
            Err(error) => crate::slog_warn!("callgraph refresh PASSIVE checkpoint failed: {error}"),
        }
    }
}

impl CallGraphStore {
    pub(super) fn finish_refresh_wal(&self, queued: bool) {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        self.refresh_wal
            .lock()
            .unwrap()
            .when_unqueued(&conn, queued);
    }
}

#[cfg(test)]
mod tests;
