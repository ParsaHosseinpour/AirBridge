//! Batch-level progress events.
//!
//! These are the orchestration events the FFI maps into its own event channel.
//! They are deliberately pure data: no callback runs foreign code, and every
//! value is `Clone` so a session can both publish it and keep a snapshot.

use std::path::PathBuf;

use crate::queue::BatchReport;

/// An event emitted while a batch runs or finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchEvent {
    /// The manifest was accepted and the queue is about to start.
    Started {
        /// Batch identifier.
        batch_id: u64,
        /// Number of files in the manifest.
        total_files: u32,
        /// Sum of every expected size.
        total_bytes: u64,
    },
    /// One file is becoming active.
    FileStarted {
        /// Zero-based manifest index.
        index: u32,
        /// Canonical relative path.
        relative_path: String,
        /// Expected size.
        size: u64,
    },
    /// Throttled overall progress sample.
    Progress {
        /// The active file, or `None` between files.
        current_file_index: Option<u32>,
        /// Bytes verified or written for the active file.
        file_bytes_transferred: u64,
        /// Expected size of the active file.
        file_size: u64,
        /// Bytes verified or written across the whole batch.
        bytes_transferred: u64,
        /// Sum of every expected size.
        total_bytes: u64,
        /// `total_bytes - bytes_transferred`.
        remaining_bytes: u64,
        /// Files completed successfully so far.
        files_completed: u32,
        /// Total files in the batch.
        files_total: u32,
        /// Linear estimate from the observed rate, when enough data exists.
        estimated_remaining_ms: Option<u64>,
    },
    /// One file was verified and committed/acknowledged.
    FileCompleted {
        /// Zero-based manifest index.
        index: u32,
        /// Canonical relative path.
        relative_path: String,
        /// File size.
        size: u64,
        /// Offset this attempt resumed from.
        resumed_from: u64,
        /// Destination path for a receive; `None` for a send.
        destination_path: Option<PathBuf>,
    },
    /// One file failed; the batch may continue with the next item.
    FileFailed {
        /// Zero-based manifest index.
        index: u32,
        /// Canonical relative path.
        relative_path: String,
        /// Short failure kind.
        kind: String,
        /// Human-readable detail.
        message: String,
    },
    /// Terminal success or partial success. Check `report.failed_files` to
    /// distinguish the two.
    Finished {
        /// Final queue report.
        report: BatchReport,
    },
}
