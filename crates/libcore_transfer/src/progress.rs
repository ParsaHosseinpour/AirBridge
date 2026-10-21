//! Shared progress throttling for the sender and receiver.
//!
//! Both directions report the same shape: the active file, its byte count, and
//! the queue's overall snapshot. Keeping the throttle here means a receive and
//! a send with the same [`crate::BatchOptions`] emit at the same cadence.

use std::time::{Duration, Instant};

use crate::event::BatchEvent;
use crate::queue::TransferQueue;

/// Limits how often an overall progress event is emitted.
pub(crate) struct ProgressThrottle {
    interval: Duration,
    last: Option<Instant>,
}

impl ProgressThrottle {
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
        }
    }

    pub(crate) fn due(&mut self) -> bool {
        let now = Instant::now();
        match self.last {
            Some(last) if now.saturating_duration_since(last) < self.interval => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

/// Emits a progress sample from the queue's current snapshot.
pub(crate) fn emit_progress<F>(
    queue: &TransferQueue,
    index: u32,
    file_bytes: u64,
    file_size: u64,
    throttle: &mut ProgressThrottle,
    force: bool,
    emit: &mut F,
) where
    F: FnMut(BatchEvent),
{
    if !force && !throttle.due() {
        return;
    }
    let snapshot = queue.snapshot();
    emit(BatchEvent::Progress {
        current_file_index: Some(index),
        file_bytes_transferred: file_bytes.min(file_size),
        file_size,
        bytes_transferred: snapshot.transferred_bytes,
        total_bytes: snapshot.total_bytes,
        remaining_bytes: snapshot.remaining_bytes,
        files_completed: snapshot.completed_files,
        files_total: snapshot.total_files,
        estimated_remaining_ms: snapshot.estimated_remaining_ms,
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::queue::TransferQueue;
    use std::sync::{Arc, Mutex};

    #[test]
    fn throttle_always_emits_the_first_sample() {
        let mut throttle = ProgressThrottle::new(Duration::from_secs(3600));
        assert!(throttle.due());
        assert!(!throttle.due());
    }

    #[test]
    fn throttle_zero_interval_emits_every_time() {
        let mut throttle = ProgressThrottle::new(Duration::ZERO);
        assert!(throttle.due());
        assert!(throttle.due());
    }

    #[test]
    fn progress_events_are_plain_data() {
        let seen: Arc<Mutex<Vec<BatchEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let queue = TransferQueue::empty();
        let mut throttle = ProgressThrottle::new(Duration::ZERO);
        let mut emit = |event| seen.lock().unwrap().push(event);
        emit_progress(&queue, 0, 10, 100, &mut throttle, true, &mut emit);
        assert!(matches!(
            seen.lock().unwrap().first(),
            Some(BatchEvent::Progress {
                file_bytes_transferred: 10,
                ..
            })
        ));
    }
}
