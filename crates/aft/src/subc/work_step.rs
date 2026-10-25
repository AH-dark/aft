//! Single-writer, bounded-read diagnostics for the current frame-loop step.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

// Diagnostic text is bounded independently of request size. Tool/command names
// and entry points fit here; oversized names are explicitly marked as clipped.
const TEXT_BYTES: usize = 512;

pub(super) struct WorkStep {
    generation: AtomicU64,
    text: [AtomicU8; TEXT_BYTES],
    since_ms: AtomicU64,
}

impl Default for WorkStep {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            text: std::array::from_fn(|_| AtomicU8::new(0)),
            since_ms: AtomicU64::new(0),
        }
    }
}

impl WorkStep {
    /// Only the frame loop publishes. Sequential consistency makes a matching
    /// even generation a coherent snapshot of all bytes and the start time.
    pub(super) fn enter(&self, entry: &str, command: &str, request_id: u64, now_ms: u64) {
        self.publish(
            &format!("entry={entry} command={command:?} request_id={request_id}"),
            now_ms,
        );
    }

    fn publish(&self, text: &str, since_ms: u64) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let bytes = text.as_bytes();
        for (index, byte) in self.text.iter().enumerate() {
            byte.store(bytes.get(index).copied().unwrap_or(0), Ordering::SeqCst);
        }
        if bytes.len() >= TEXT_BYTES {
            let suffix = b" [clipped]\0";
            for (byte, value) in self.text[TEXT_BYTES - suffix.len()..].iter().zip(suffix) {
                byte.store(*value, Ordering::SeqCst);
            }
        }
        self.since_ms.store(since_ms, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub(super) fn clear(&self) {
        self.publish("entry=none", 0);
    }

    pub(super) fn snapshot(&self, now_ms: u64) -> String {
        // Never spin on a publisher: it may be suspended halfway through an
        // update. An explicitly incomplete snapshot is better than a watchdog
        // that waits for the very thread it is diagnosing.
        let before = self.generation.load(Ordering::SeqCst);
        if before % 2 != 0 {
            return "entry=updating".to_string();
        }
        let bytes = self.text.each_ref().map(|byte| byte.load(Ordering::SeqCst));
        let since = self.since_ms.load(Ordering::SeqCst);
        if self.generation.load(Ordering::SeqCst) != before {
            return "entry=updating".to_string();
        }
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(TEXT_BYTES);
        if end == 0 {
            return "entry=none".to_string();
        }
        format!(
            "{} step_elapsed_ms={}",
            String::from_utf8_lossy(&bytes[..end]),
            now_ms.saturating_sub(since),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_publisher_never_blocks_snapshot() {
        let step = WorkStep::default();
        step.enter("poll_if_woken", "read", 891, 10);
        assert_eq!(
            step.snapshot(35),
            "entry=poll_if_woken command=\"read\" request_id=891 step_elapsed_ms=25"
        );
        // Simulate a publisher stopped between the first and last atomic write.
        step.generation.fetch_add(1, Ordering::SeqCst);
        assert_eq!(step.snapshot(40), "entry=updating");
    }
}
