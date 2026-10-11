use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::io::{self, BufRead};
use std::sync::Arc;

// Match the binary remote-tool protocol's 64 MiB frame-body ceiling.
// Aggregate retention across standalone input queues is a separate limit.
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_RETAINED_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_RETAINED_ITEMS: usize = 64;
pub(crate) const MAX_HELD_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_HELD_ITEMS: usize = 32;

#[derive(Debug)]
pub(crate) struct OversizedLine {
    pub limit: usize,
}

impl std::fmt::Display for OversizedLine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.limit == MAX_LINE_BYTES {
            formatter.write_str("request exceeds the 64 MiB (67108864 bytes) input limit; split it into smaller requests")
        } else {
            write!(
                formatter,
                "request exceeds the {} byte input limit; split it into smaller requests",
                self.limit
            )
        }
    }
}

impl std::error::Error for OversizedLine {}

pub(crate) struct ReadLine {
    pub result: io::Result<String>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub scratch_capacity: usize,
}

pub(crate) fn read_bounded_line(
    reader: &mut impl BufRead,
    limit: usize,
) -> io::Result<Option<ReadLine>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    let mut consumed = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        consumed = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let payload_len = newline.unwrap_or(available.len());
        if !oversized {
            if payload_len > limit.saturating_sub(bytes.len()) {
                oversized = true;
                bytes.clear();
            } else {
                let needed = bytes.len() + payload_len;
                if needed > bytes.capacity() {
                    let capacity =
                        needed.max(bytes.capacity().saturating_mul(2).max(1024).min(limit));
                    bytes.reserve_exact(capacity - bytes.len());
                }
                bytes.extend_from_slice(&available[..payload_len]);
            }
        }
        reader.consume(payload_len + usize::from(newline.is_some()));
        if newline.is_some() {
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            break;
        }
    }
    if !consumed {
        return Ok(None);
    }
    // Record the real buffer capacity so tests can detect a full-line allocation
    // even if the reader rejects the line afterward. Oversized tails are drained
    // without retaining their bytes in this buffer.
    let scratch_capacity = bytes.capacity();
    let result = if oversized {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            OversizedLine { limit },
        ))
    } else {
        String::from_utf8(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request is not valid UTF-8"))
    };
    Ok(Some(ReadLine {
        result,
        scratch_capacity,
    }))
}

#[derive(Default)]
struct Retained {
    bytes: usize,
    items: usize,
    closed: bool,
}

pub(crate) struct InputBudget {
    state: Mutex<Retained>,
    changed: Condvar,
    max_bytes: usize,
    max_items: usize,
}

impl InputBudget {
    pub fn new(max_bytes: usize, max_items: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(Retained::default()),
            changed: Condvar::new(),
            max_bytes,
            max_items,
        })
    }

    fn room(&self, state: &Retained, bytes: usize) -> bool {
        state.items < self.max_items
            && state
                .bytes
                .checked_add(bytes)
                .is_some_and(|total| total <= self.max_bytes)
    }

    #[cfg(test)]
    pub fn try_admit(
        self: &Arc<Self>,
        result: io::Result<String>,
    ) -> Result<InputFrame, io::Result<String>> {
        let bytes = retained_capacity(&result);
        let mut state = self.state.lock();
        if state.closed || !self.room(&state, bytes) {
            return Err(result);
        }
        state.bytes += bytes;
        state.items += 1;
        Ok(InputFrame {
            result,
            _permit: Retention {
                budget: Arc::clone(self),
                bytes,
            },
        })
    }

    pub fn admit(self: &Arc<Self>, result: io::Result<String>) -> Option<InputFrame> {
        let bytes = retained_capacity(&result);
        if bytes > self.max_bytes {
            return None;
        }
        let mut state = self.state.lock();
        while !state.closed && !self.room(&state, bytes) {
            self.changed.wait(&mut state);
        }
        if state.closed {
            return None;
        }
        state.bytes += bytes;
        state.items += 1;
        Some(InputFrame {
            result,
            _permit: Retention {
                budget: Arc::clone(self),
                bytes,
            },
        })
    }

    pub fn close(&self) {
        self.state.lock().closed = true;
        self.changed.notify_all();
    }
}

fn retained_capacity(result: &io::Result<String>) -> usize {
    result.as_ref().map_or(1, |line| line.capacity().max(1))
}

struct Retention {
    budget: Arc<InputBudget>,
    bytes: usize,
}

impl Drop for Retention {
    fn drop(&mut self) {
        let mut state = self.budget.state.lock();
        state.bytes -= self.bytes;
        state.items -= 1;
        drop(state);
        self.budget.changed.notify_all();
    }
}

pub(crate) struct InputFrame {
    pub result: io::Result<String>,
    _permit: Retention,
}

impl std::fmt::Debug for InputFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputFrame")
            .field("retained_bytes", &self.retained_bytes())
            .field("valid_utf8", &self.result.is_ok())
            .finish()
    }
}

impl InputFrame {
    pub fn retained_bytes(&self) -> usize {
        self._permit.bytes
    }
}

pub(crate) struct HeldInput {
    frames: VecDeque<InputFrame>,
    bytes: usize,
    max_bytes: usize,
    max_items: usize,
}

impl HeldInput {
    pub fn new(max_bytes: usize, max_items: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            bytes: 0,
            max_bytes,
            max_items,
        }
    }

    pub fn push(&mut self, frame: InputFrame) -> Result<(), InputFrame> {
        if self.frames.len() >= self.max_items
            || self
                .bytes
                .checked_add(frame.retained_bytes())
                .is_none_or(|total| total > self.max_bytes)
        {
            return Err(frame);
        }
        self.bytes += frame.retained_bytes();
        self.frames.push_back(frame);
        Ok(())
    }

    pub fn pop_back(&mut self) -> Option<InputFrame> {
        let frame = self.frames.pop_back()?;
        self.bytes -= frame.retained_bytes();
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_line_never_allocates_full_frame_and_recovers_next_line() {
        let input = format!(
            "{}\n{{\"id\":\"next\",\"command\":\"ping\"}}\n",
            "x".repeat(4096)
        );
        let mut reader = io::BufReader::with_capacity(7, io::Cursor::new(input));
        let oversized = read_bounded_line(&mut reader, 32).unwrap().unwrap();
        assert!(oversized
            .result
            .unwrap_err()
            .get_ref()
            .unwrap()
            .is::<OversizedLine>());
        assert!(
            oversized.scratch_capacity <= 32,
            "scratch allocation exceeded the incremental ceiling: {}",
            oversized.scratch_capacity
        );
        let next = read_bounded_line(&mut reader, 32)
            .unwrap()
            .unwrap()
            .result
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&next).unwrap()["id"],
            "next"
        );
        assert!(read_bounded_line(&mut reader, 32).unwrap().is_none());
    }

    #[test]
    fn retained_budget_survives_channel_queued_and_held_moves() {
        let budget = InputBudget::new(8, 2);
        let frame = budget.admit(Ok("12345678".to_string())).unwrap();
        let (sender, receiver) = crossbeam_channel::bounded(1);
        sender
            .send(frame)
            .unwrap_or_else(|_| panic!("receiver alive"));
        let mut queued = VecDeque::new();
        queued.push_back(receiver.recv().unwrap());
        let mut held = HeldInput::new(8, 2);
        assert!(held.push(queued.pop_front().unwrap()).is_ok());
        assert!(
            budget.try_admit(Ok("x".to_string())).is_err(),
            "moving a frame must not release its retained-byte charge"
        );
        drop(held.pop_back());
        assert!(budget.try_admit(Ok("12345678".to_string())).is_ok());
        assert_eq!(budget.state.lock().items, 0);
    }

    #[test]
    fn held_capacity_leaves_room_for_control_input() {
        let budget = InputBudget::new(16, 4);
        let mut held = HeldInput::new(8, 2);
        assert!(held
            .push(budget.admit(Ok("12345678".to_string())).unwrap())
            .is_ok());
        let refused = held.push(budget.admit(Ok("x".to_string())).unwrap());
        assert!(
            refused.is_err(),
            "held work must not consume the control-input reserve"
        );
        drop(refused);
        let control = budget.try_admit(Ok("status".to_string()));
        assert!(control.is_ok());
    }

    #[test]
    fn held_item_cap_preserves_control_reserve_for_small_frames() {
        let budget = InputBudget::new(16, 4);
        let mut held = HeldInput::new(8, 2);
        assert!(held.push(budget.admit(Ok(String::new())).unwrap()).is_ok());
        assert!(held.push(budget.admit(Ok(String::new())).unwrap()).is_ok());
        let refused = held.push(budget.admit(Ok(String::new())).unwrap());
        assert!(
            refused.is_err(),
            "small held frames must not bypass item admission"
        );
        drop(refused);
        assert!(budget.try_admit(Ok("status".to_string())).is_ok());
    }

    #[test]
    fn empty_lines_and_errors_count_toward_item_admission() {
        let budget = InputBudget::new(16, 2);
        let first = budget.admit(Ok(String::new())).unwrap();
        let second = budget
            .admit(Err(io::Error::from(io::ErrorKind::InvalidData)))
            .unwrap();
        assert!(
            budget.try_admit(Ok(String::new())).is_err(),
            "zero-length frames must not bypass item admission"
        );
        drop(first);
        drop(second);
        assert_eq!(budget.state.lock().items, 0);
    }

    #[test]
    fn input_ceiling_matches_subc_and_preserves_utf8_and_eof_framing() {
        assert_eq!(MAX_LINE_BYTES, subc_protocol::MAX_FRAME_BODY_LEN as usize);
        let mut reader = io::Cursor::new(b"\xff\n1234\r\nlast");
        assert_eq!(
            read_bounded_line(&mut reader, 8)
                .unwrap()
                .unwrap()
                .result
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            read_bounded_line(&mut reader, 8)
                .unwrap()
                .unwrap()
                .result
                .unwrap(),
            "1234"
        );
        assert_eq!(
            read_bounded_line(&mut reader, 4)
                .unwrap()
                .unwrap()
                .result
                .unwrap(),
            "last"
        );
        assert!(read_bounded_line(&mut reader, 4).unwrap().is_none());
    }

    #[test]
    fn closing_budget_refuses_admission_without_leaking_retention() {
        let budget = InputBudget::new(8, 1);
        let held = budget.admit(Ok("12345678".to_string())).unwrap();
        budget.close();
        drop(held);
        assert!(
            budget.admit(Ok("x".to_string())).is_none(),
            "closed ingress must refuse admission even after capacity returns"
        );
        assert_eq!(budget.state.lock().bytes, 0);
    }
}
