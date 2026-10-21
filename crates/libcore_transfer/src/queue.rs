//! The batch queue state machine.
//!
//! [`TransferQueue`] is the authoritative view of a batch: which items are
//! pending, which one is active, what has been verified, and why an item
//! failed. It is independent of I/O and of the FFI, so the session layer can
//! update it from the transfer task while a UI thread reads snapshots.
//!
//! The queue supports more than one active slot so a caller can schedule
//! independent connections in parallel, but the manifest protocol itself is
//! ordered: the bundled sender and receiver use one slot and move files
//! sequentially over one authenticated stream.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::manifest::TransferManifest;

/// State of the queue as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueStatus {
    /// Created and populated, but not started.
    Idle,
    /// The queue has started; items may be pending, active or already done.
    Running,
    /// Every item completed successfully.
    Completed,
    /// At least one item failed.
    Failed,
    /// The caller cancelled the batch.
    Cancelled,
}

impl QueueStatus {
    /// Whether the queue has reached a terminal state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// State of one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueItemStatus {
    /// Waiting for a free slot.
    Pending,
    /// Currently being transferred.
    Active,
    /// Verified and committed.
    Completed,
    /// Ended with an error.
    Failed,
    /// Skipped because the batch was cancelled.
    Cancelled,
}

/// Why a queue operation was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// No manifest has been installed, or it contains no files.
    EmptyBatch,
    /// The queue has already left the idle state.
    AlreadyStarted,
    /// The queue has not been started.
    NotStarted,
    /// The queue is already terminal.
    AlreadyFinished,
    /// No item exists at that index.
    UnknownItem {
        /// The requested index.
        index: u32,
    },
    /// The item is not in the required state.
    InvalidTransition {
        /// Item index.
        index: u32,
        /// Current state.
        from: QueueItemStatus,
        /// Requested state.
        to: QueueItemStatus,
    },
    /// Adding another active item would exceed the concurrency limit.
    ConcurrencyExceeded,
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyBatch => f.write_str("the batch has no files"),
            Self::AlreadyStarted => f.write_str("the batch has already started"),
            Self::NotStarted => f.write_str("the batch has not started"),
            Self::AlreadyFinished => f.write_str("the batch has already finished"),
            Self::UnknownItem { index } => write!(f, "batch item {index} does not exist"),
            Self::InvalidTransition { index, from, to } => {
                write!(f, "batch item {index} cannot move from {from:?} to {to:?}")
            }
            Self::ConcurrencyExceeded => f.write_str("the configured concurrency limit is reached"),
        }
    }
}

impl std::error::Error for QueueError {}

/// Immutable view of one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueItemSnapshot {
    /// Zero-based position in the manifest.
    pub index: u32,
    /// Canonical relative path.
    pub relative_path: String,
    /// Expected size.
    pub size: u64,
    /// Expected SHA-256.
    pub sha256: [u8; 32],
    /// Current state.
    pub status: QueueItemStatus,
    /// Bytes verified or written so far.
    pub bytes_transferred: u64,
    /// Offset this attempt resumed from.
    pub resumed_from: u64,
    /// Payload bytes that crossed the wire, retransmissions included.
    pub bytes_on_wire: u64,
    /// Where a receive committed the file.
    pub destination_path: Option<PathBuf>,
    /// Failure detail, when `status` is [`QueueItemStatus::Failed`].
    pub error: Option<String>,
}

impl QueueItemSnapshot {
    /// Lower-case hexadecimal SHA-256, for display and foreign bindings.
    #[must_use]
    pub fn sha256_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.sha256 {
            out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('?'));
            out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('?'));
        }
        out
    }
}

/// Immutable view of the whole queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSnapshot {
    /// Random identifier chosen by the sender.
    pub batch_id: u64,
    /// Queue state.
    pub status: QueueStatus,
    /// Number of files in the manifest.
    pub total_files: u32,
    /// Files still waiting.
    pub pending_files: u32,
    /// Files currently active.
    pub active_files: u32,
    /// Files completed successfully.
    pub completed_files: u32,
    /// Files that failed.
    pub failed_files: u32,
    /// Sum of every expected size.
    pub total_bytes: u64,
    /// Sum of verified/progressed bytes.
    pub transferred_bytes: u64,
    /// `total_bytes - transferred_bytes`.
    pub remaining_bytes: u64,
    /// Lowest active item index, for the current-file label.
    pub current_file_index: Option<u32>,
    /// Time since the queue started, in milliseconds.
    pub elapsed_ms: u64,
    /// Linear estimate from the observed byte rate, when running.
    pub estimated_remaining_ms: Option<u64>,
    /// Per-item snapshots in manifest order.
    pub items: Vec<QueueItemSnapshot>,
}

impl QueueSnapshot {
    /// Builds the terminal report shape from this snapshot.
    #[must_use]
    pub fn into_report(self) -> BatchReport {
        BatchReport {
            batch_id: self.batch_id,
            status: self.status,
            total_files: self.total_files,
            completed_files: self.completed_files,
            failed_files: self.failed_files,
            total_bytes: self.total_bytes,
            transferred_bytes: self.transferred_bytes,
            elapsed_ms: self.elapsed_ms,
            files: self.items,
        }
    }
}

/// Final outcome of a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchReport {
    /// Batch identifier.
    pub batch_id: u64,
    /// Terminal queue state.
    pub status: QueueStatus,
    /// Number of files in the manifest.
    pub total_files: u32,
    /// Files completed successfully.
    pub completed_files: u32,
    /// Files that failed.
    pub failed_files: u32,
    /// Sum of every expected size.
    pub total_bytes: u64,
    /// Sum of verified bytes.
    pub transferred_bytes: u64,
    /// Wall-clock duration of the batch.
    pub elapsed_ms: u64,
    /// Per-file outcomes.
    pub files: Vec<QueueItemSnapshot>,
}

impl BatchReport {
    /// Whether every file completed successfully.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.failed_files == 0 && matches!(self.status, QueueStatus::Completed)
    }
}

#[derive(Debug)]
struct Item {
    entry: crate::manifest::ManifestEntry,
    status: QueueItemStatus,
    bytes_transferred: u64,
    resumed_from: u64,
    bytes_on_wire: u64,
    destination_path: Option<PathBuf>,
    error: Option<String>,
}

#[derive(Debug)]
struct Inner {
    batch_id: u64,
    items: Vec<Item>,
    status: QueueStatus,
    active: Vec<u32>,
    max_active: u32,
    installed: bool,
    started_at: Option<Instant>,
    finished_at: Option<Instant>,
}

/// A shared, thread-safe batch queue.
///
/// Cloning the handle shares the same state, which is what lets a transfer
/// task update the queue while `status()` or `items()` is called from a UI
/// thread.
#[derive(Clone, Debug)]
pub struct TransferQueue {
    inner: Arc<Mutex<Inner>>,
}

impl TransferQueue {
    /// Creates an empty queue. A manifest is installed before the batch runs.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                batch_id: 0,
                items: Vec::new(),
                status: QueueStatus::Idle,
                active: Vec::new(),
                max_active: 1,
                installed: false,
                started_at: None,
                finished_at: None,
            })),
        }
    }

    /// Creates a queue from a manifest, with one active slot.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::EmptyBatch`] when the manifest has no files.
    pub fn from_manifest(manifest: &TransferManifest) -> Result<Self, QueueError> {
        Self::from_manifest_with_concurrency(manifest, 1)
    }

    /// Creates a queue with `max_active` slots.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::EmptyBatch`] for an empty manifest and
    /// [`QueueError::ConcurrencyExceeded`] when `max_active` is zero.
    pub fn from_manifest_with_concurrency(
        manifest: &TransferManifest,
        max_active: u32,
    ) -> Result<Self, QueueError> {
        let queue = Self::empty();
        queue.install_manifest_with_concurrency(manifest, max_active)?;
        Ok(queue)
    }

    /// Installs a manifest into an idle, empty queue.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::AlreadyStarted`] if the queue has been used, or
    /// [`QueueError::EmptyBatch`] for an empty manifest.
    pub fn install_manifest(&self, manifest: &TransferManifest) -> Result<(), QueueError> {
        self.install_manifest_with_concurrency(manifest, 1)
    }

    /// Installs a manifest and sets the concurrency limit.
    ///
    /// # Errors
    ///
    /// See [`Self::install_manifest`].
    pub fn install_manifest_with_concurrency(
        &self,
        manifest: &TransferManifest,
        max_active: u32,
    ) -> Result<(), QueueError> {
        if max_active == 0 {
            return Err(QueueError::ConcurrencyExceeded);
        }
        if manifest.is_empty() {
            return Err(QueueError::EmptyBatch);
        }
        let mut inner = lock(&self.inner);
        if inner.installed || inner.status != QueueStatus::Idle || !inner.items.is_empty() {
            return Err(QueueError::AlreadyStarted);
        }
        inner.batch_id = manifest.batch_id();
        inner.items = manifest
            .entries()
            .iter()
            .map(|entry| Item {
                entry: entry.clone(),
                status: QueueItemStatus::Pending,
                bytes_transferred: 0,
                resumed_from: 0,
                bytes_on_wire: 0,
                destination_path: None,
                error: None,
            })
            .collect();
        inner.max_active = max_active;
        inner.installed = true;
        Ok(())
    }

    /// Moves an idle queue into the running state.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::EmptyBatch`] when no manifest is installed and
    /// [`QueueError::AlreadyFinished`] when the queue is already terminal.
    pub fn begin(&self) -> Result<(), QueueError> {
        let mut inner = lock(&self.inner);
        if !inner.installed || inner.items.is_empty() {
            return Err(QueueError::EmptyBatch);
        }
        if inner.status.is_terminal() {
            return Err(QueueError::AlreadyFinished);
        }
        if inner.status == QueueStatus::Running {
            return Ok(());
        }
        inner.status = QueueStatus::Running;
        inner.started_at = Some(Instant::now());
        Ok(())
    }

    /// Marks the next pending item active and returns its index.
    ///
    /// Returns `Ok(None)` when no item is pending.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::NotStarted`] before [`Self::begin`] and
    /// [`QueueError::ConcurrencyExceeded`] when every slot is busy.
    pub fn activate_next(&self) -> Result<Option<u32>, QueueError> {
        let mut inner = lock(&self.inner);
        if inner.status != QueueStatus::Running {
            return Err(if inner.status.is_terminal() {
                QueueError::AlreadyFinished
            } else {
                QueueError::NotStarted
            });
        }
        if inner.active.len() >= inner.max_active as usize {
            return Err(QueueError::ConcurrencyExceeded);
        }
        let Some(index) = inner
            .items
            .iter()
            .position(|item| item.status == QueueItemStatus::Pending)
        else {
            return Ok(None);
        };
        inner.items[index].status = QueueItemStatus::Active;
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        inner.active.push(index);
        Ok(Some(index))
    }

    /// Marks a specific pending item active.
    ///
    /// # Errors
    ///
    /// Returns an invalid-transition or concurrency error.
    pub fn activate(&self, index: u32) -> Result<(), QueueError> {
        let mut inner = lock(&self.inner);
        if inner.status != QueueStatus::Running {
            return Err(if inner.status.is_terminal() {
                QueueError::AlreadyFinished
            } else {
                QueueError::NotStarted
            });
        }
        if inner.active.len() >= inner.max_active as usize {
            return Err(QueueError::ConcurrencyExceeded);
        }
        let item = inner
            .items
            .get_mut(index as usize)
            .ok_or(QueueError::UnknownItem { index })?;
        if item.status != QueueItemStatus::Pending {
            return Err(QueueError::InvalidTransition {
                index,
                from: item.status,
                to: QueueItemStatus::Active,
            });
        }
        item.status = QueueItemStatus::Active;
        inner.active.push(index);
        Ok(())
    }

    /// Records verified or written bytes for an active item.
    ///
    /// Progress is monotonic: a repair rewind never makes the overall bar go
    /// backwards. Values above the expected size are clamped.
    ///
    /// # Errors
    ///
    /// Returns an invalid-transition error when the item is not active.
    pub fn record_progress(&self, index: u32, bytes_transferred: u64) -> Result<(), QueueError> {
        let mut inner = lock(&self.inner);
        let item = inner
            .items
            .get_mut(index as usize)
            .ok_or(QueueError::UnknownItem { index })?;
        if item.status != QueueItemStatus::Active {
            return Err(QueueError::InvalidTransition {
                index,
                from: item.status,
                to: QueueItemStatus::Active,
            });
        }
        item.bytes_transferred = item
            .bytes_transferred
            .max(bytes_transferred.min(item.entry.size()));
        Ok(())
    }

    /// Records retransmitted payload bytes for an active item.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::UnknownItem`] when the index is out of range.
    pub fn add_wire_bytes(&self, index: u32, bytes: u64) -> Result<(), QueueError> {
        let mut inner = lock(&self.inner);
        let item = inner
            .items
            .get_mut(index as usize)
            .ok_or(QueueError::UnknownItem { index })?;
        item.bytes_on_wire = item.bytes_on_wire.saturating_add(bytes);
        Ok(())
    }

    /// Marks an active item completed.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::UnknownItem`] or an invalid-transition error.
    pub fn complete(
        &self,
        index: u32,
        resumed_from: u64,
        destination_path: Option<PathBuf>,
    ) -> Result<(), QueueError> {
        let mut inner = lock(&self.inner);
        let item = inner
            .items
            .get_mut(index as usize)
            .ok_or(QueueError::UnknownItem { index })?;
        if item.status != QueueItemStatus::Active {
            return Err(QueueError::InvalidTransition {
                index,
                from: item.status,
                to: QueueItemStatus::Completed,
            });
        }
        item.status = QueueItemStatus::Completed;
        item.bytes_transferred = item.entry.size();
        item.resumed_from = resumed_from;
        item.destination_path = destination_path;
        item.error = None;
        inner.active.retain(|active| *active != index);
        Ok(())
    }

    /// Marks a pending or active item failed.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::UnknownItem`] or an invalid-transition error.
    pub fn fail(&self, index: u32, message: impl Into<String>) -> Result<(), QueueError> {
        let mut inner = lock(&self.inner);
        let item = inner
            .items
            .get_mut(index as usize)
            .ok_or(QueueError::UnknownItem { index })?;
        if !matches!(
            item.status,
            QueueItemStatus::Pending | QueueItemStatus::Active
        ) {
            return Err(QueueError::InvalidTransition {
                index,
                from: item.status,
                to: QueueItemStatus::Failed,
            });
        }
        item.status = QueueItemStatus::Failed;
        item.error = Some(message.into());
        inner.active.retain(|active| *active != index);
        Ok(())
    }

    /// Marks every pending or active item from `start` onward cancelled
    /// without changing the queue's lifecycle state.
    ///
    /// Callers use this when they stop early but still finish the batch with a
    /// report: the item that failed stays failed, while entries the peer will
    /// never attempt are clearly marked cancelled rather than left pending.
    pub fn cancel_pending_from(&self, start: u32) {
        let mut inner = lock(&self.inner);
        for item in inner.items.iter_mut().skip(start as usize) {
            if matches!(
                item.status,
                QueueItemStatus::Pending | QueueItemStatus::Active
            ) {
                item.status = QueueItemStatus::Cancelled;
            }
        }
        inner.active.retain(|index| *index < start);
    }

    /// Cancels the queue and every item that has not completed.
    pub fn cancel(&self) {
        let mut inner = lock(&self.inner);
        if inner.status.is_terminal() {
            return;
        }
        inner.status = QueueStatus::Cancelled;
        inner.finished_at = Some(Instant::now());
        inner.active.clear();
        for item in &mut inner.items {
            if matches!(
                item.status,
                QueueItemStatus::Pending | QueueItemStatus::Active
            ) {
                item.status = QueueItemStatus::Cancelled;
            }
        }
    }

    /// Ends a running queue: `Failed` if any item failed, otherwise
    /// `Completed`.
    pub fn finish(&self) -> QueueStatus {
        let mut inner = lock(&self.inner);
        if inner.status.is_terminal() {
            return inner.status;
        }
        let failed = inner
            .items
            .iter()
            .any(|item| item.status == QueueItemStatus::Failed);
        inner.status = if failed {
            QueueStatus::Failed
        } else {
            QueueStatus::Completed
        };
        inner.finished_at = Some(Instant::now());
        inner.active.clear();
        inner.status
    }

    /// Current terminal or lifecycle state.
    #[must_use]
    pub fn status(&self) -> QueueStatus {
        lock(&self.inner).status
    }

    /// Per-item snapshots in manifest order.
    #[must_use]
    pub fn items(&self) -> Vec<QueueItemSnapshot> {
        snapshot(&lock(&self.inner)).items
    }

    /// Full immutable snapshot, including timing and derived totals.
    #[must_use]
    pub fn snapshot(&self) -> QueueSnapshot {
        snapshot(&lock(&self.inner))
    }

    /// Final report built from the current snapshot.
    #[must_use]
    pub fn report(&self) -> BatchReport {
        self.snapshot().into_report()
    }
}

impl Default for TransferQueue {
    fn default() -> Self {
        Self::empty()
    }
}

fn snapshot(inner: &Inner) -> QueueSnapshot {
    let total_files = u32::try_from(inner.items.len()).unwrap_or(u32::MAX);
    let mut pending = 0_u32;
    let mut active = 0_u32;
    let mut completed = 0_u32;
    let mut failed = 0_u32;
    let mut total_bytes = 0_u64;
    let mut transferred_bytes = 0_u64;
    let mut items = Vec::with_capacity(inner.items.len());

    for (index, item) in inner.items.iter().enumerate() {
        total_bytes = total_bytes.saturating_add(item.entry.size());
        transferred_bytes = transferred_bytes.saturating_add(item.bytes_transferred);
        match item.status {
            QueueItemStatus::Pending => pending += 1,
            QueueItemStatus::Active => active += 1,
            QueueItemStatus::Completed => completed += 1,
            QueueItemStatus::Failed => failed += 1,
            QueueItemStatus::Cancelled => {}
        }
        items.push(QueueItemSnapshot {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            relative_path: item.entry.relative_path().to_owned(),
            size: item.entry.size(),
            sha256: item.entry.sha256(),
            status: item.status,
            bytes_transferred: item.bytes_transferred,
            resumed_from: item.resumed_from,
            bytes_on_wire: item.bytes_on_wire,
            destination_path: item.destination_path.clone(),
            error: item.error.clone(),
        });
    }

    let remaining_bytes = total_bytes.saturating_sub(transferred_bytes);
    let elapsed = elapsed(inner);
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let estimated_remaining_ms =
        if inner.status == QueueStatus::Running && transferred_bytes > 0 && remaining_bytes > 0 {
            let estimate = (u128::from(remaining_bytes) * u128::from(elapsed_ms))
                / u128::from(transferred_bytes);
            Some(u64::try_from(estimate).unwrap_or(u64::MAX))
        } else {
            None
        };

    QueueSnapshot {
        batch_id: inner.batch_id,
        status: inner.status,
        total_files,
        pending_files: pending,
        active_files: active,
        completed_files: completed,
        failed_files: failed,
        total_bytes,
        transferred_bytes,
        remaining_bytes,
        current_file_index: inner.active.iter().copied().min(),
        elapsed_ms,
        estimated_remaining_ms,
        items,
    }
}

fn elapsed(inner: &Inner) -> Duration {
    match (inner.started_at, inner.finished_at) {
        (Some(started), Some(finished)) => finished.saturating_duration_since(started),
        (Some(started), None) => started.elapsed(),
        _ => Duration::ZERO,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
