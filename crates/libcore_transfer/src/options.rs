//! Tunables shared by batch upload and receive.

use std::time::Duration;

use libcore_transport::{ChunkPlan, CHUNK_SIZE};

use crate::error::BatchError;

/// Per-batch transfer tunables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchOptions {
    /// Chunk grid size, in bytes.
    pub chunk_size: u32,
    /// How many verified bytes the receiver may accumulate before acking.
    pub ack_every_bytes: u64,
    /// Announce and verify the whole-file SHA-256 in each `HELLO`.
    pub verify_whole_file: bool,
    /// Deadline for a per-file opening ack, `COMPLETE`, or the manifest read.
    pub response_timeout: Duration,
    /// Minimum interval between overall progress events. Zero emits every
    /// chunk, which is useful in tests but not for a UI.
    pub progress_interval: Duration,
    /// Continue with the next file after a recoverable per-file failure.
    pub continue_on_error: bool,
}

impl BatchOptions {
    /// Default batch options: 512 KiB chunks, 4 MiB ack cadence, digest
    /// verification enabled, 30 s response deadline, 100 ms progress,
    /// continue after recoverable item failures.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            chunk_size: CHUNK_SIZE,
            ack_every_bytes: 4 * 1024 * 1024,
            verify_whole_file: true,
            response_timeout: Duration::from_secs(30),
            progress_interval: Duration::from_millis(100),
            continue_on_error: true,
        }
    }

    /// Validates the protocol-visible fields.
    ///
    /// # Errors
    ///
    /// Returns [`BatchError::Transport`] for an unsupported chunk size and
    /// [`BatchError::Protocol`] for a zero ack cadence or timeout.
    pub fn validate(&self) -> Result<(), BatchError> {
        ChunkPlan::new(self.chunk_size).map_err(BatchError::from)?;
        if self.ack_every_bytes == 0 {
            return Err(BatchError::Protocol(
                "ack_every_bytes must be non-zero".to_owned(),
            ));
        }
        if self.response_timeout.is_zero() {
            return Err(BatchError::Protocol(
                "response_timeout must be non-zero".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self::new()
    }
}
