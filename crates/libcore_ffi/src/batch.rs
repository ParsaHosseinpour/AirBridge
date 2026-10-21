//! Multi-file batch transfer, driven across the FFI.
//!
//! This module wraps `libcore_transfer` without moving any protocol logic into
//! the boundary. A [`BatchTransferSession`] owns:
//!
//! * a preparing [`PreparedBatch`](libcore_transfer::PreparedBatch), populated
//!   by `add_file` / `add_directory` before `run`;
//! * a shared [`TransferQueue`](libcore_transfer::TransferQueue), which the
//!   transfer task updates and the UI reads through [`BatchTransferSession::snapshot`];
//! * the same authenticated handshake as a single-file session, negotiated with
//!   [`MODE_BATCH`] so a batch receiver never mistakes a single file for a
//!   manifest.
//!
//! Events are pull-based, matching the rest of the boundary: `run` resolves to
//! the final report while `next_event` drains batch-level progress. Payload
//! bytes never cross the FFI.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libcore_transfer as core_batch;
use libcore_transfer::{
    BatchError as CoreBatchError, BatchEvent as CoreBatchEvent, BatchOptions as CoreBatchOptions,
    BatchReport as CoreBatchReport, QueueItemStatus, QueueStatus, TransferQueue,
};
use libcore_transport::{ChunkPlan, PayloadId, SecureStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};

use crate::cancel::{Canceller, WaitCancelled};
use crate::crypto::{MODE_BATCH, MODE_PAIRING};
use crate::error::FfiError;
use crate::runtime;
use crate::transfer::{
    answer_peer as deliver_decision, establish_authenticated, io_error, throttled_interval,
    validate_endpoint, AuthObserver, PeerDecision, SecurityConfig, TransferDirection,
};

/// Capacity of the public event ring, matching the single-file adapter.
const EVENT_CHANNEL_CAPACITY: usize = 128;

/// Lifecycle state of a batch. The authoritative snapshot; the event stream
/// may lag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum BatchPhase {
    /// Created and accepting files/directories, not started.
    Idle,
    /// Connecting (send) or binding/accepting (receive).
    Connecting,
    /// The handshake completed and the user is confirming the code.
    Verifying,
    /// The manifest was accepted and files are moving.
    Running,
    /// Every attempted file completed.
    Completed,
    /// At least one file failed, or the session ended with an error.
    Failed,
    /// `cancel` stopped the batch.
    Cancelled,
}

impl BatchPhase {
    const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// State of one file in a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum BatchItemStatus {
    /// Waiting for its turn.
    Pending,
    /// Currently transferring.
    Active,
    /// Verified and committed (receive) or acknowledged (send).
    Completed,
    /// Ended with an error.
    Failed,
    /// Skipped because the batch was cancelled.
    Cancelled,
}

/// One file's immutable state inside a [`BatchSnapshot`] or [`BatchReport`].
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BatchItemInfo {
    /// Zero-based position in the manifest.
    pub index: u32,
    /// Canonical `/`-separated relative path.
    pub relative_path: String,
    /// Expected size in bytes.
    pub size: u64,
    /// Lower-case SHA-256 of the file.
    pub sha256_hex: String,
    /// Current state.
    pub status: BatchItemStatus,
    /// Bytes verified (receive) or acknowledged (send) so far.
    pub bytes_transferred: u64,
    /// Offset this attempt resumed from.
    pub resumed_from: u64,
    /// Payload bytes that crossed the wire, retransmissions included.
    pub bytes_on_wire: u64,
    /// Where a receive committed the file.
    pub destination_path: Option<String>,
    /// Failure detail, when `status` is [`BatchItemStatus::Failed`].
    pub error: Option<String>,
}

/// Immutable view of a batch at one instant.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BatchSnapshot {
    /// Batch identifier chosen by the sender.
    pub batch_id: u64,
    /// Lifecycle state.
    pub phase: BatchPhase,
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
    /// Sum of verified/acknowledged bytes.
    pub transferred_bytes: u64,
    /// `total_bytes - transferred_bytes`.
    pub remaining_bytes: u64,
    /// Lowest active item index, for the current-file label.
    pub current_file_index: Option<u32>,
    /// Time since the batch started.
    pub elapsed_ms: u64,
    /// Linear estimate from the observed byte rate, when running.
    pub estimated_remaining_ms: Option<u64>,
    /// Per-file state in manifest order.
    pub items: Vec<BatchItemInfo>,
}

/// Final outcome of a batch.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BatchReport {
    /// Batch identifier.
    pub batch_id: u64,
    /// Terminal phase: `Completed`, `Failed`, or `Cancelled`.
    pub status: BatchPhase,
    /// Number of files in the manifest.
    pub total_files: u32,
    /// Files completed successfully.
    pub completed_files: u32,
    /// Files that failed.
    pub failed_files: u32,
    /// Sum of every expected size.
    pub total_bytes: u64,
    /// Sum of verified/acknowledged bytes.
    pub transferred_bytes: u64,
    /// Wall-clock duration of the batch.
    pub elapsed_ms: u64,
    /// Per-file outcomes.
    pub files: Vec<BatchItemInfo>,
}

/// Events emitted by a running batch.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum BatchEvent {
    /// The manifest was accepted and the queue is about to start.
    Started {
        /// Batch identifier.
        batch_id: u64,
        /// Number of files.
        total_files: u32,
        /// Sum of every expected size.
        total_bytes: u64,
    },
    /// The handshake completed and the peer's identity is waiting for a
    /// decision. Compare `sas` with the other screen, then call
    /// [`BatchTransferSession::confirm_peer`] or
    /// [`BatchTransferSession::reject_peer`].
    VerificationRequired {
        /// Six-digit Short Authentication String.
        sas: String,
        /// SHA-256 fingerprint of the peer's identity key.
        peer_fingerprint: String,
        /// The peer's uncompressed P-256 public key, for diagnostics.
        peer_public_key: Vec<u8>,
    },
    /// The peer was authenticated and its peer id was bound to the
    /// fingerprint.
    PeerVerified {
        /// Claimed peer id, received only after the handshake.
        peer_id: String,
        /// Fingerprint the id is bound to.
        peer_fingerprint: String,
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
        /// Bytes moved for the active file.
        file_bytes_transferred: u64,
        /// Expected size of the active file.
        file_size: u64,
        /// Bytes moved across the whole batch.
        bytes_transferred: u64,
        /// Sum of every expected size.
        total_bytes: u64,
        /// `total_bytes - bytes_transferred`.
        remaining_bytes: u64,
        /// Files completed successfully so far.
        files_completed: u32,
        /// Total files in the batch.
        files_total: u32,
        /// Linear estimate from the observed rate, when available.
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
        destination_path: Option<String>,
    },
    /// One file failed; the batch may continue with the next item.
    FileFailed {
        /// Zero-based manifest index.
        index: u32,
        /// Canonical relative path.
        relative_path: String,
        /// Short machine-readable kind.
        kind: String,
        /// Human-readable detail.
        message: String,
    },
    /// Terminal success or partial success. Check `report.failed_files` to
    /// distinguish the two.
    Finished {
        /// Final report.
        report: BatchReport,
    },
    /// Terminal failure before a report could be produced.
    Failed {
        /// Short machine-readable kind.
        kind: String,
        /// Human-readable detail.
        message: String,
    },
    /// Terminal cancellation.
    Cancelled,
    /// The consumer fell behind and `missed` events were dropped. Re-read
    /// [`BatchTransferSession::snapshot`].
    Lagged {
        /// Number of dropped events.
        missed: u64,
    },
}

/// Tunables for a batch session.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BatchOptions {
    /// Chunk grid size; 512 KiB by default.
    #[uniffi(default = 524288)]
    pub chunk_size: u32,
    /// How many verified bytes may accumulate before a progress ack is sent.
    #[uniffi(default = 4194304)]
    pub ack_every_bytes: u64,
    /// Verify the whole-file SHA-256 before committing/acknowledging.
    #[uniffi(default = true)]
    pub verify_whole_file: bool,
    /// Deadline for a per-file acknowledgement, the final `COMPLETE`, and the
    /// manifest preamble.
    #[uniffi(default = 30000)]
    pub response_timeout_ms: u64,
    /// Minimum interval between progress events.
    #[uniffi(default = 100)]
    pub progress_interval_ms: u64,
    /// Continue with the next file after a recoverable per-file failure.
    #[uniffi(default = true)]
    pub continue_on_error: bool,
    /// Sender's stable batch id. Leave unset to generate a fresh one; set the
    /// same value on a retry so every entry keeps its manifest payload id and
    /// the receiver can resume its part files. Receivers ignore this field and
    /// learn the batch id from the manifest.
    #[uniffi(default)]
    pub batch_id: Option<u64>,
    /// How long a receive waits for the sender to connect.
    #[uniffi(default = 60000)]
    pub accept_timeout_ms: u64,
    /// This device's long-lived identity, as PKCS#8 DER.
    pub identity_pkcs8: Vec<u8>,
    /// This device's discovery peer id, exchanged after the handshake.
    #[uniffi(default = "")]
    pub local_peer_id: String,
    /// Fingerprints previously verified by the user. Matching peers are
    /// confirmed automatically.
    pub trusted_peer_fingerprints: Vec<String>,
    /// When set, only this peer identity is accepted.
    #[uniffi(default)]
    pub expected_peer_fingerprint: Option<String>,
    /// Deadline for the handshake and identity preamble.
    #[uniffi(default = 30000)]
    pub handshake_timeout_ms: u64,
    /// How long an interactive verification may wait for a decision.
    #[uniffi(default = 120000)]
    pub verification_timeout_ms: u64,
}

impl BatchOptions {
    fn security(&self) -> Result<SecurityConfig, FfiError> {
        SecurityConfig::parse(
            &self.identity_pkcs8,
            &self.local_peer_id,
            self.trusted_peer_fingerprints.as_slice(),
            self.expected_peer_fingerprint.as_deref(),
            self.handshake_timeout_ms,
            self.verification_timeout_ms,
        )
    }

    fn core_options(&self) -> Result<CoreBatchOptions, FfiError> {
        ChunkPlan::new(self.chunk_size).map_err(FfiError::from)?;
        if self.ack_every_bytes == 0 {
            return Err(FfiError::invalid_argument(
                "ack_every_bytes must be non-zero",
            ));
        }
        if self.response_timeout_ms == 0 {
            return Err(FfiError::invalid_argument(
                "response_timeout_ms must be non-zero",
            ));
        }
        if self.accept_timeout_ms == 0 {
            return Err(FfiError::invalid_argument(
                "accept_timeout_ms must be non-zero",
            ));
        }
        if self.progress_interval_ms == 0 {
            return Err(FfiError::invalid_argument(
                "progress_interval_ms must be non-zero",
            ));
        }
        Ok(CoreBatchOptions {
            chunk_size: self.chunk_size,
            ack_every_bytes: self.ack_every_bytes,
            verify_whole_file: self.verify_whole_file,
            response_timeout: Duration::from_millis(self.response_timeout_ms),
            progress_interval: Duration::from_millis(throttled_interval(self.progress_interval_ms)),
            continue_on_error: self.continue_on_error,
        })
    }
}

/// Authoritative session state, independent of the event ring.
struct BatchState {
    phase: BatchPhase,
    report: Option<BatchReport>,
    batch_id: Option<u64>,
    peer_id: Option<String>,
    peer_fingerprint: Option<String>,
}

/// The state every exported method shares.
struct BatchInner {
    direction: TransferDirection,
    address: String,
    destination: Option<String>,
    options: BatchOptions,
    core_options: CoreBatchOptions,
    security: SecurityConfig,
    batch: AsyncMutex<core_batch::PreparedBatch>,
    queue: TransferQueue,
    canceller: Arc<Canceller>,
    token: core_batch::CancelToken,
    prepared_files: AtomicU32,
    prepared_bytes: AtomicU64,
    prepared_items: Mutex<Vec<BatchItemInfo>>,
    state: Mutex<BatchState>,
    /// Taken and dropped on the terminal event, closing the stream afterwards.
    events: Mutex<Option<broadcast::Sender<BatchEvent>>>,
    receiver: Arc<AsyncMutex<Option<broadcast::Receiver<BatchEvent>>>>,
    decision: Mutex<Option<oneshot::Sender<PeerDecision>>>,
    started: AtomicBool,
}

impl BatchInner {
    fn set_phase(&self, phase: BatchPhase) {
        lock(&self.state).phase = phase;
    }

    fn set_terminal(&self, phase: BatchPhase, report: Option<BatchReport>) {
        debug_assert!(phase.is_terminal());
        let mut state = lock(&self.state);
        state.phase = phase;
        if let Some(report) = report {
            state.batch_id = Some(report.batch_id);
            state.report = Some(report);
        }
    }

    fn emit(&self, event: BatchEvent) {
        let guard = lock(&self.events);
        if let Some(sender) = guard.as_ref() {
            let _ = sender.send(event);
        }
    }

    fn close_events(&self) {
        let _ = lock(&self.events).take();
    }

    /// Maps a core event into the FFI shape and publishes it.
    fn emit_core(&self, event: CoreBatchEvent) {
        match event {
            CoreBatchEvent::Started {
                batch_id,
                total_files,
                total_bytes,
            } => {
                lock(&self.state).batch_id = Some(batch_id);
                self.emit(BatchEvent::Started {
                    batch_id,
                    total_files,
                    total_bytes,
                });
            }
            CoreBatchEvent::FileStarted {
                index,
                relative_path,
                size,
            } => self.emit(BatchEvent::FileStarted {
                index,
                relative_path,
                size,
            }),
            CoreBatchEvent::Progress {
                current_file_index,
                file_bytes_transferred,
                file_size,
                bytes_transferred,
                total_bytes,
                remaining_bytes,
                files_completed,
                files_total,
                estimated_remaining_ms,
            } => self.emit(BatchEvent::Progress {
                current_file_index,
                file_bytes_transferred,
                file_size,
                bytes_transferred,
                total_bytes,
                remaining_bytes,
                files_completed,
                files_total,
                estimated_remaining_ms,
            }),
            CoreBatchEvent::FileCompleted {
                index,
                relative_path,
                size,
                resumed_from,
                destination_path,
            } => self.emit(BatchEvent::FileCompleted {
                index,
                relative_path,
                size,
                resumed_from,
                destination_path: destination_path.map(path_string),
            }),
            CoreBatchEvent::FileFailed {
                index,
                relative_path,
                kind,
                message,
            } => self.emit(BatchEvent::FileFailed {
                index,
                relative_path,
                kind,
                message,
            }),
            CoreBatchEvent::Finished { report } => {
                // `run` publishes the terminal event once, after it knows the
                // core call returned successfully. Storing here keeps a
                // concurrent `report()`/`snapshot()` caller from observing a
                // stale state in the tiny window before that happens.
                let report = report_from_core(report);
                lock(&self.state).report = Some(report);
            }
        }
    }

    async fn run(&self) -> Result<BatchReport, FfiError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(FfiError::AlreadyFinished);
        }
        self.set_phase(BatchPhase::Connecting);

        let result = match self.direction {
            TransferDirection::Send => self.send().await,
            TransferDirection::Receive => self.receive().await,
        };

        match result {
            Ok(report) => {
                let phase = report.status;
                self.set_terminal(phase, Some(report.clone()));
                self.emit(BatchEvent::Finished {
                    report: report.clone(),
                });
                self.close_events();
                Ok(report)
            }
            Err(error) => {
                let kind = error.kind().to_owned();
                let message = error.to_string();
                if matches!(error, FfiError::Cancelled) {
                    self.set_terminal(BatchPhase::Cancelled, None);
                    self.emit(BatchEvent::Cancelled);
                } else {
                    self.set_terminal(BatchPhase::Failed, None);
                    self.emit(BatchEvent::Failed { kind, message });
                }
                self.close_events();
                Err(error)
            }
        }
    }

    async fn send(&self) -> Result<BatchReport, FfiError> {
        let batch = self.batch.lock().await;
        if batch.is_empty() {
            return Err(FfiError::invalid_argument("the batch contains no files"));
        }

        let stream = self.connect().await?;
        let _ = stream.set_nodelay(true);
        let (stream, mode) = self.authenticate(stream, true).await?;
        if mode != MODE_BATCH {
            return Err(FfiError::Protocol {
                message: "the peer did not acknowledge batch mode".to_owned(),
            });
        }
        self.set_phase(BatchPhase::Running);

        let report = core_batch::upload_batch(
            stream,
            &batch,
            &self.queue,
            &self.core_options,
            &self.token,
            |event| self.emit_core(event),
        )
        .await
        .map_err(|error| batch_error(error, self))?;
        Ok(report_from_core(report))
    }

    async fn receive(&self) -> Result<BatchReport, FfiError> {
        let destination = self
            .destination
            .as_deref()
            .ok_or_else(|| FfiError::internal("a receive batch has no destination"))?;

        let listener = TcpListener::bind(self.address.as_str())
            .await
            .map_err(io_error)?;

        let accept = tokio::time::timeout(
            Duration::from_millis(self.options.accept_timeout_ms),
            listener.accept(),
        );
        tokio::pin!(accept);
        let (stream, _peer) = tokio::select! {
            biased;
            result = &mut accept => match result {
                Ok(Ok(accepted)) => accepted,
                Ok(Err(error)) => return Err(io_error(error)),
                Err(_) => {
                    return Err(FfiError::Timeout {
                        message: "waiting for an inbound batch connection".to_owned(),
                    })
                }
            },
            () = WaitCancelled(self.canceller.as_ref()) => return Err(FfiError::Cancelled),
        };
        let _ = stream.set_nodelay(true);

        let (stream, mode) = self.authenticate(stream, false).await?;
        if mode == MODE_PAIRING {
            // Pairing and transfers share one listener port. A pairing-only
            // connection carries no manifest; completing normally lets the
            // long-lived receiver re-arm without showing an empty batch.
            return Ok(BatchReport {
                batch_id: 0,
                status: BatchPhase::Completed,
                total_files: 0,
                completed_files: 0,
                failed_files: 0,
                total_bytes: 0,
                transferred_bytes: 0,
                elapsed_ms: 0,
                files: Vec::new(),
            });
        }
        if mode != MODE_BATCH {
            return Err(FfiError::Protocol {
                message: "the peer did not request batch mode".to_owned(),
            });
        }
        self.set_phase(BatchPhase::Running);

        let report = core_batch::receive_batch(
            stream,
            &self.queue,
            Path::new(destination),
            &self.core_options,
            &self.token,
            |event| self.emit_core(event),
        )
        .await
        .map_err(|error| batch_error(error, self))?;
        Ok(report_from_core(report))
    }

    /// Runs the shared authenticated handshake with batch mode.
    async fn authenticate(
        &self,
        stream: TcpStream,
        initiator: bool,
    ) -> Result<(SecureStream<TcpStream>, u8), FfiError> {
        establish_authenticated(
            stream,
            initiator,
            &self.security,
            MODE_BATCH,
            &self.canceller,
            &self.decision,
            self,
        )
        .await
    }

    async fn connect(&self) -> Result<TcpStream, FfiError> {
        let connect = TcpStream::connect(self.address.as_str());
        tokio::pin!(connect);
        tokio::select! {
            biased;
            result = &mut connect => result.map_err(io_error),
            () = WaitCancelled(self.canceller.as_ref()) => Err(FfiError::Cancelled),
        }
    }

    fn cancel(&self) {
        self.canceller.request();
        self.token.cancel();
    }

    fn is_cancelled(&self) -> bool {
        self.canceller.is_requested() || self.token.is_cancelled()
    }

    fn snapshot(&self) -> BatchSnapshot {
        let queue = self.queue.snapshot();
        let state = lock(&self.state);
        let prepared_files = self.prepared_files.load(Ordering::Relaxed);
        let prepared_bytes = self.prepared_bytes.load(Ordering::Relaxed);
        let total_files = if queue.total_files == 0 {
            prepared_files
        } else {
            queue.total_files
        };
        let total_bytes = if queue.total_bytes == 0 {
            prepared_bytes
        } else {
            queue.total_bytes
        };
        let remaining_bytes = total_bytes.saturating_sub(queue.transferred_bytes);
        let items = if queue.items.is_empty() {
            lock(&self.prepared_items).clone()
        } else {
            queue.items.into_iter().map(item_from_core).collect()
        };
        let pending_files = if queue.total_files == 0 {
            total_files
        } else {
            queue.pending_files
        };
        BatchSnapshot {
            batch_id: if queue.batch_id == 0 {
                state.batch_id.unwrap_or(0)
            } else {
                queue.batch_id
            },
            phase: state.phase,
            total_files,
            pending_files,
            active_files: queue.active_files,
            completed_files: queue.completed_files,
            failed_files: queue.failed_files,
            total_bytes,
            transferred_bytes: queue.transferred_bytes,
            remaining_bytes,
            current_file_index: queue.current_file_index,
            elapsed_ms: queue.elapsed_ms,
            estimated_remaining_ms: queue.estimated_remaining_ms,
            items,
        }
    }
}

impl AuthObserver for BatchInner {
    fn set_verifying(&self) {
        self.set_phase(BatchPhase::Verifying);
    }

    fn verification_required(&self, challenge: crate::crypto::PairingChallenge) {
        self.emit(BatchEvent::VerificationRequired {
            sas: challenge.sas,
            peer_fingerprint: challenge.peer_fingerprint,
            peer_public_key: challenge.peer_public_key,
        });
    }

    fn peer_verified(&self, peer_id: String, peer_fingerprint: String) {
        {
            let mut state = lock(&self.state);
            state.peer_id = Some(peer_id.clone());
            state.peer_fingerprint = Some(peer_fingerprint.clone());
        }
        self.emit(BatchEvent::PeerVerified {
            peer_id,
            peer_fingerprint,
        });
    }
}

/// A multi-file send or receive batch.
///
/// Construct with [`Self::send`] or [`Self::receive`], add files with
/// [`Self::add_file`] / [`Self::add_directory`], then call [`Self::run`] once
/// and drain [`Self::next_event`] concurrently. [`Self::snapshot`] is safe to
/// call from any thread at any time.
#[derive(uniffi::Object)]
pub struct BatchTransferSession {
    inner: Arc<BatchInner>,
}

#[uniffi::export]
impl BatchTransferSession {
    /// Creates a sender that will stream the prepared files to `address`
    /// (`host:port`; DNS names are resolved by the runtime).
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] for an empty address, an
    /// unsupported chunk size, a zero timeout/ack cadence, or invalid identity
    /// configuration.
    #[uniffi::constructor]
    pub fn send(address: String, options: BatchOptions) -> Result<Arc<Self>, FfiError> {
        validate_endpoint(&address, ".")?;
        let batch_id = options.batch_id;
        Self::assemble(TransferDirection::Send, address, None, options, batch_id)
    }

    /// Creates a receiver that binds `listen_address` (`ip:port`) and accepts
    /// exactly one inbound batch into `destination`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::send`], plus an empty destination.
    #[uniffi::constructor]
    pub fn receive(
        listen_address: String,
        destination: String,
        options: BatchOptions,
    ) -> Result<Arc<Self>, FfiError> {
        validate_endpoint(&listen_address, &destination)?;
        Self::assemble(
            TransferDirection::Receive,
            listen_address,
            Some(destination),
            options,
            None,
        )
    }

    /// Adds one file to this send batch, hashing it now and rejecting
    /// duplicates.
    ///
    /// `relative_path` defaults to the file's name. It must be a safe relative
    /// path; `..`, absolute paths and Windows drive prefixes are rejected.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] for a missing/unsafe path, an
    /// unsupported file type, a duplicate, or a receive session;
    /// [`FfiError::AlreadyFinished`] once `run` has started.
    pub async fn add_file(
        &self,
        path: String,
        relative_path: Option<String>,
    ) -> Result<(), FfiError> {
        if self.inner.direction != TransferDirection::Send {
            return Err(FfiError::invalid_argument(
                "only a send batch accepts local files",
            ));
        }
        if self.inner.started.load(Ordering::SeqCst) {
            return Err(FfiError::AlreadyFinished);
        }
        let relative = match relative_path {
            Some(relative) => relative,
            None => Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| FfiError::invalid_argument("the file has no usable name"))?
                .to_owned(),
        };

        let mut batch = self.inner.batch.lock().await;
        if self.inner.started.load(Ordering::SeqCst) {
            return Err(FfiError::AlreadyFinished);
        }
        let before = batch.len();
        batch
            .add_file(&path, relative)
            .await
            .map_err(manifest_error)?;
        self.inner.record_prepared(&batch, before);
        Ok(())
    }

    /// Recursively adds every regular file under `directory` to this send
    /// batch.
    ///
    /// The directory's own name becomes the path prefix. Symbolic links are
    /// skipped and files are added in sorted order.
    ///
    /// # Errors
    ///
    /// Same as [`Self::add_file`].
    pub async fn add_directory(&self, directory: String) -> Result<(), FfiError> {
        if self.inner.direction != TransferDirection::Send {
            return Err(FfiError::invalid_argument(
                "only a send batch accepts local files",
            ));
        }
        if self.inner.started.load(Ordering::SeqCst) {
            return Err(FfiError::AlreadyFinished);
        }
        let mut batch = self.inner.batch.lock().await;
        if self.inner.started.load(Ordering::SeqCst) {
            return Err(FfiError::AlreadyFinished);
        }
        let before = batch.len();
        batch
            .add_directory(&directory)
            .await
            .map_err(manifest_error)?;
        self.inner.record_prepared(&batch, before);
        Ok(())
    }

    /// Returns which way bytes will flow.
    #[must_use]
    pub fn direction(&self) -> TransferDirection {
        self.inner.direction
    }

    /// Returns the batch identifier, once known.
    ///
    /// A sender chooses it at construction. A receiver learns it from the
    /// manifest, so this is `None` until the first `Started` event.
    #[must_use]
    pub fn batch_id(&self) -> Option<u64> {
        lock(&self.inner.state).batch_id
    }

    /// Returns the authoritative lifecycle state.
    #[must_use]
    pub fn status(&self) -> BatchPhase {
        lock(&self.inner.state).phase
    }

    /// Returns the completion report, once the batch has finished.
    #[must_use]
    pub fn report(&self) -> Option<BatchReport> {
        lock(&self.inner.state).report.clone()
    }

    /// Returns a consistent view of the queue and lifecycle state.
    #[must_use]
    pub fn snapshot(&self) -> BatchSnapshot {
        self.inner.snapshot()
    }

    /// Returns the peer id claimed after the authenticated handshake.
    #[must_use]
    pub fn peer_id(&self) -> Option<String> {
        lock(&self.inner.state).peer_id.clone()
    }

    /// Returns the SHA-256 fingerprint of the peer's identity key.
    #[must_use]
    pub fn peer_fingerprint(&self) -> Option<String> {
        lock(&self.inner.state).peer_fingerprint.clone()
    }

    /// Confirms the code shown by [`BatchEvent::VerificationRequired`].
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] when no verification is pending.
    pub fn confirm_peer(&self) -> Result<(), FfiError> {
        deliver_decision(&self.inner.decision, PeerDecision::Confirm)
    }

    /// Declines the code shown by [`BatchEvent::VerificationRequired`].
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] when no verification is pending.
    pub fn reject_peer(&self) -> Result<(), FfiError> {
        deliver_decision(&self.inner.decision, PeerDecision::Reject)
    }

    /// Requests cancellation. Non-blocking, idempotent, callable from any
    /// thread. The core task unwinds through its normal cleanup path; the
    /// receiver keeps every part file for a resume.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Runs the batch and returns its report.
    ///
    /// Call this once. Cancellation is prompt while connecting, accepting,
    /// handshaking, reading and writing.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::AlreadyFinished`] on a second call, or the mapped
    /// batch/transport error with [`FfiError::Cancelled`] taking precedence
    /// when `cancel` was called.
    pub async fn run(self: Arc<Self>) -> Result<BatchReport, FfiError> {
        let inner = Arc::clone(&self.inner);
        runtime::run_on_runtime(async move { inner.run().await }).await?
    }

    /// Waits for the next batch event.
    ///
    /// Resolves to `None` after the terminal event once the session has closed
    /// its stream. One consumer at a time; concurrent calls serialize.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::Internal`] if the shared runtime is unavailable.
    pub async fn next_event(&self) -> Result<Option<BatchEvent>, FfiError> {
        let receiver = Arc::clone(&self.inner.receiver);
        runtime::run_on_runtime(async move {
            let mut guard = receiver.lock().await;
            let Some(receiver) = guard.as_mut() else {
                return Ok(None);
            };
            match receiver.recv().await {
                Ok(event) => Ok(Some(event)),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    Ok(Some(BatchEvent::Lagged { missed }))
                }
                Err(broadcast::error::RecvError::Closed) => {
                    *guard = None;
                    Ok(None)
                }
            }
        })
        .await?
    }
}

impl BatchTransferSession {
    fn assemble(
        direction: TransferDirection,
        address: String,
        destination: Option<String>,
        options: BatchOptions,
        batch_id: Option<u64>,
    ) -> Result<Arc<Self>, FfiError> {
        let security = options.security()?;
        let core_options = options.core_options()?;
        let prepared_id = match batch_id {
            Some(id) => id,
            None if matches!(direction, TransferDirection::Send) => random_batch_id()?,
            None => 0,
        };
        let (sender, receiver) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Ok(Arc::new(Self {
            inner: Arc::new(BatchInner {
                direction,
                address,
                destination,
                options,
                core_options,
                security,
                batch: AsyncMutex::new(core_batch::PreparedBatch::new(prepared_id)),
                queue: TransferQueue::empty(),
                canceller: Arc::new(Canceller::new()),
                token: core_batch::CancelToken::new(),
                prepared_files: AtomicU32::new(0),
                prepared_bytes: AtomicU64::new(0),
                prepared_items: Mutex::new(Vec::new()),
                state: Mutex::new(BatchState {
                    phase: BatchPhase::Idle,
                    report: None,
                    batch_id: matches!(direction, TransferDirection::Send).then_some(prepared_id),
                    peer_id: None,
                    peer_fingerprint: None,
                }),
                events: Mutex::new(Some(sender)),
                receiver: Arc::new(AsyncMutex::new(Some(receiver))),
                decision: Mutex::new(None),
                started: AtomicBool::new(false),
            }),
        }))
    }
}

impl BatchInner {
    /// Updates the counters used by `snapshot` before the manifest exists.
    fn record_prepared(&self, batch: &core_batch::PreparedBatch, before: usize) {
        let added: u64 = batch.files()[before..]
            .iter()
            .map(|file| file.entry().size())
            .sum();
        let count = u32::try_from(batch.len() - before).unwrap_or(u32::MAX);
        self.prepared_files.fetch_add(count, Ordering::Relaxed);
        self.prepared_bytes.fetch_add(added, Ordering::Relaxed);

        let mut items = lock(&self.prepared_items);
        for (offset, file) in batch.files()[before..].iter().enumerate() {
            let entry = file.entry();
            items.push(BatchItemInfo {
                index: u32::try_from(before + offset).unwrap_or(u32::MAX),
                relative_path: entry.relative_path().to_owned(),
                size: entry.size(),
                sha256_hex: entry.sha256_hex(),
                status: BatchItemStatus::Pending,
                bytes_transferred: 0,
                resumed_from: 0,
                bytes_on_wire: 0,
                destination_path: None,
                error: None,
            });
        }
    }
}

fn random_batch_id() -> Result<u64, FfiError> {
    PayloadId::random()
        .map(PayloadId::get)
        .map_err(FfiError::from)
}

fn manifest_error(error: core_batch::ManifestError) -> FfiError {
    FfiError::invalid_argument(error.to_string())
}

fn batch_error(error: CoreBatchError, inner: &BatchInner) -> FfiError {
    if inner.is_cancelled() {
        return FfiError::Cancelled;
    }
    match error {
        CoreBatchError::Transport(error) => error.into(),
        CoreBatchError::Manifest(error) => FfiError::Protocol {
            message: format!("manifest error: {error}"),
        },
        CoreBatchError::Queue(error) => FfiError::Protocol {
            message: format!("queue error: {error}"),
        },
        CoreBatchError::Io(error) => FfiError::from(error),
        CoreBatchError::Protocol(message) => FfiError::Protocol { message },
        CoreBatchError::Cancelled => FfiError::Cancelled,
        CoreBatchError::EmptyBatch => FfiError::invalid_argument("the batch contains no files"),
        // `CoreBatchError` is non-exhaustive; a future variant must still map
        // to a well-formed boundary error.
        _ => FfiError::internal("unsupported batch failure"),
    }
}

fn phase_from_queue(status: QueueStatus) -> BatchPhase {
    match status {
        QueueStatus::Idle => BatchPhase::Idle,
        QueueStatus::Running => BatchPhase::Running,
        QueueStatus::Completed => BatchPhase::Completed,
        QueueStatus::Failed => BatchPhase::Failed,
        QueueStatus::Cancelled => BatchPhase::Cancelled,
    }
}

fn item_status_from_queue(status: QueueItemStatus) -> BatchItemStatus {
    match status {
        QueueItemStatus::Pending => BatchItemStatus::Pending,
        QueueItemStatus::Active => BatchItemStatus::Active,
        QueueItemStatus::Completed => BatchItemStatus::Completed,
        QueueItemStatus::Failed => BatchItemStatus::Failed,
        QueueItemStatus::Cancelled => BatchItemStatus::Cancelled,
    }
}

fn item_from_core(item: core_batch::QueueItemSnapshot) -> BatchItemInfo {
    let core_batch::QueueItemSnapshot {
        index,
        relative_path,
        size,
        sha256,
        status,
        bytes_transferred,
        resumed_from,
        bytes_on_wire,
        destination_path,
        error,
    } = item;
    BatchItemInfo {
        index,
        relative_path,
        size,
        sha256_hex: hex_lower(&sha256),
        status: item_status_from_queue(status),
        bytes_transferred,
        resumed_from,
        bytes_on_wire,
        destination_path: destination_path.map(path_string),
        error,
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('?'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('?'));
    }
    out
}

fn report_from_core(report: CoreBatchReport) -> BatchReport {
    BatchReport {
        batch_id: report.batch_id,
        status: phase_from_queue(report.status),
        total_files: report.total_files,
        completed_files: report.completed_files,
        failed_files: report.failed_files,
        total_bytes: report.total_bytes,
        transferred_bytes: report.transferred_bytes,
        elapsed_ms: report.elapsed_ms,
        files: report.files.into_iter().map(item_from_core).collect(),
    }
}

fn path_string(path: std::path::PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::crypto::{PairingSession, MODE_PAIRING, MODE_TRANSFER};
    use libcore_crypto::{fingerprint_hex, Identity};

    struct TestIdentity {
        pkcs8: Vec<u8>,
        fingerprint: String,
    }

    fn test_identity() -> TestIdentity {
        let identity = Identity::generate().unwrap();
        TestIdentity {
            pkcs8: identity.to_pkcs8().to_vec(),
            fingerprint: fingerprint_hex(&identity.fingerprint()),
        }
    }

    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "airbridge-ffi-batch-{}-{tag}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn options(
        identity: &TestIdentity,
        peer_id: &str,
        trusted: &[&str],
        expected: Option<&str>,
    ) -> BatchOptions {
        BatchOptions {
            chunk_size: 64 * 1024,
            ack_every_bytes: 64 * 1024,
            verify_whole_file: true,
            response_timeout_ms: 10_000,
            progress_interval_ms: 50,
            continue_on_error: true,
            batch_id: None,
            accept_timeout_ms: 10_000,
            identity_pkcs8: identity.pkcs8.clone(),
            local_peer_id: peer_id.to_owned(),
            trusted_peer_fingerprints: trusted.iter().map(|value| (*value).to_owned()).collect(),
            expected_peer_fingerprint: expected.map(str::to_owned),
            handshake_timeout_ms: 10_000,
            verification_timeout_ms: 10_000,
        }
    }

    async fn drain_events(session: &BatchTransferSession) -> Vec<BatchEvent> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut events = Vec::new();
            loop {
                match session.next_event().await {
                    Ok(Some(event)) => events.push(event),
                    Ok(None) => break,
                    Err(error) => panic!("event stream failed: {error}"),
                }
            }
            events
        })
        .await
        .expect("draining the batch events timed out")
    }

    #[test]
    fn phases_are_terminal_only_after_completion() {
        assert!(!BatchPhase::Idle.is_terminal());
        assert!(!BatchPhase::Connecting.is_terminal());
        assert!(!BatchPhase::Verifying.is_terminal());
        assert!(!BatchPhase::Running.is_terminal());
        assert!(BatchPhase::Completed.is_terminal());
        assert!(BatchPhase::Failed.is_terminal());
        assert!(BatchPhase::Cancelled.is_terminal());
    }

    #[test]
    fn mode_constants_do_not_collide() {
        assert_ne!(MODE_BATCH, MODE_TRANSFER);
        assert_ne!(MODE_BATCH, MODE_PAIRING);
        assert_ne!(MODE_TRANSFER, MODE_PAIRING);
    }

    #[test]
    fn invalid_batch_options_are_rejected_before_any_io() {
        let identity = test_identity();

        let mut zero_chunk = options(&identity, "sender", &[], None);
        zero_chunk.chunk_size = 0;
        assert!(matches!(
            BatchTransferSession::send("127.0.0.1:1".into(), zero_chunk),
            Err(FfiError::InvalidArgument { .. })
        ));

        let mut zero_ack = options(&identity, "sender", &[], None);
        zero_ack.ack_every_bytes = 0;
        assert!(matches!(
            BatchTransferSession::send("127.0.0.1:1".into(), zero_ack),
            Err(FfiError::InvalidArgument { .. })
        ));

        let mut zero_response = options(&identity, "sender", &[], None);
        zero_response.response_timeout_ms = 0;
        assert!(matches!(
            BatchTransferSession::send("127.0.0.1:1".into(), zero_response),
            Err(FfiError::InvalidArgument { .. })
        ));

        let mut zero_progress = options(&identity, "sender", &[], None);
        zero_progress.progress_interval_ms = 0;
        assert!(matches!(
            BatchTransferSession::send("127.0.0.1:1".into(), zero_progress),
            Err(FfiError::InvalidArgument { .. })
        ));

        let mut missing_identity = options(&identity, "sender", &[], None);
        missing_identity.identity_pkcs8.clear();
        assert!(matches!(
            BatchTransferSession::send("127.0.0.1:1".into(), missing_identity),
            Err(FfiError::InvalidArgument { .. })
        ));

        assert!(matches!(
            BatchTransferSession::send("  ".into(), options(&identity, "sender", &[], None)),
            Err(FfiError::InvalidArgument { .. })
        ));

        let mut zero_accept = options(&identity, "receiver", &[], None);
        zero_accept.accept_timeout_ms = 0;
        assert!(matches!(
            BatchTransferSession::receive("127.0.0.1:0".into(), "destination".into(), zero_accept),
            Err(FfiError::InvalidArgument { .. })
        ));

        assert!(matches!(
            BatchTransferSession::receive(
                "127.0.0.1:0".into(),
                "  ".into(),
                options(&identity, "receiver", &[], None)
            ),
            Err(FfiError::InvalidArgument { .. })
        ));
    }

    #[tokio::test]
    async fn files_and_directories_populate_the_snapshot() {
        let identity = test_identity();
        let root = temp_dir("prepare");
        let album = root.join("album");
        tokio::fs::create_dir_all(album.join("sub")).await.unwrap();
        tokio::fs::write(album.join("one.txt"), b"one")
            .await
            .unwrap();
        tokio::fs::write(album.join("sub").join("two.bin"), b"two-two")
            .await
            .unwrap();
        let manual = root.join("manual.txt");
        tokio::fs::write(&manual, b"manual").await.unwrap();

        let generated = BatchTransferSession::send(
            "127.0.0.1:1".to_owned(),
            options(&identity, "sender", &[], None),
        )
        .unwrap();
        assert!(generated.batch_id().is_some());

        let mut sender_options = options(&identity, "sender", &[], None);
        sender_options.batch_id = Some(0x00c0_ffee_1234_5678);
        let sender = BatchTransferSession::send("127.0.0.1:1".to_owned(), sender_options).unwrap();
        assert_eq!(sender.direction(), TransferDirection::Send);
        assert_eq!(sender.status(), BatchPhase::Idle);
        assert_eq!(sender.batch_id(), Some(0x00c0_ffee_1234_5678));
        assert!(sender.report().is_none());

        sender
            .add_directory(album.to_string_lossy().into_owned())
            .await
            .unwrap();
        sender
            .add_file(
                manual.to_string_lossy().into_owned(),
                Some("manual.txt".to_owned()),
            )
            .await
            .unwrap();

        let snapshot = sender.snapshot();
        assert_eq!(snapshot.phase, BatchPhase::Idle);
        assert_eq!(snapshot.total_files, 3);
        assert_eq!(snapshot.pending_files, 3);
        assert_eq!(snapshot.active_files, 0);
        assert_eq!(snapshot.completed_files, 0);
        assert_eq!(snapshot.total_bytes, 3 + 7 + 6);
        assert_eq!(snapshot.transferred_bytes, 0);
        assert_eq!(snapshot.remaining_bytes, 3 + 7 + 6);
        assert_eq!(snapshot.items.len(), 3);
        assert_eq!(snapshot.items[0].relative_path, "album/one.txt");
        assert_eq!(snapshot.items[1].relative_path, "album/sub/two.bin");
        assert_eq!(snapshot.items[2].relative_path, "manual.txt");
        assert!(snapshot
            .items
            .iter()
            .all(|item| item.status == BatchItemStatus::Pending
                && item.sha256_hex.len() == 64
                && item.bytes_transferred == 0
                && item.destination_path.is_none()));

        let duplicate = sender
            .add_file(
                manual.to_string_lossy().into_owned(),
                Some("manual.txt".to_owned()),
            )
            .await;
        assert!(matches!(duplicate, Err(FfiError::InvalidArgument { .. })));
        assert_eq!(sender.snapshot().total_files, 3);

        let unsafe_path = sender
            .add_file(
                manual.to_string_lossy().into_owned(),
                Some("../escape.txt".to_owned()),
            )
            .await;
        assert!(matches!(unsafe_path, Err(FfiError::InvalidArgument { .. })));
        assert_eq!(sender.snapshot().total_files, 3);

        let receiver = BatchTransferSession::receive(
            "127.0.0.1:0".to_owned(),
            root.join("destination").to_string_lossy().into_owned(),
            options(&identity, "receiver", &[], None),
        )
        .unwrap();
        assert_eq!(receiver.direction(), TransferDirection::Receive);
        assert_eq!(receiver.status(), BatchPhase::Idle);
        assert_eq!(receiver.batch_id(), None);
        assert!(receiver.snapshot().items.is_empty());
        assert!(matches!(
            receiver
                .add_file(manual.to_string_lossy().into_owned(), None)
                .await,
            Err(FfiError::InvalidArgument { .. })
        ));
        assert!(matches!(
            receiver
                .add_directory(album.to_string_lossy().into_owned())
                .await,
            Err(FfiError::InvalidArgument { .. })
        ));

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_trusted_batch_round_trips_every_file() {
        let sender_identity = test_identity();
        let receiver_identity = test_identity();
        let port = free_port();
        let root = temp_dir("roundtrip");
        let source = root.join("source");
        let destination = root.join("destination");
        tokio::fs::create_dir_all(source.join("nested"))
            .await
            .unwrap();
        let alpha = b"alpha payload";
        let beta = vec![0xa5; 8 * 1024];
        tokio::fs::write(source.join("alpha.txt"), alpha)
            .await
            .unwrap();
        tokio::fs::write(source.join("nested").join("beta.bin"), &beta)
            .await
            .unwrap();

        let sender = BatchTransferSession::send(
            format!("127.0.0.1:{port}"),
            options(
                &sender_identity,
                "sender",
                &[receiver_identity.fingerprint.as_str()],
                Some(receiver_identity.fingerprint.as_str()),
            ),
        )
        .unwrap();
        sender
            .add_directory(source.to_string_lossy().into_owned())
            .await
            .unwrap();
        assert_eq!(sender.snapshot().total_files, 2);

        let receiver = BatchTransferSession::receive(
            format!("127.0.0.1:{port}"),
            destination.to_string_lossy().into_owned(),
            options(
                &receiver_identity,
                "receiver",
                &[sender_identity.fingerprint.as_str()],
                Some(sender_identity.fingerprint.as_str()),
            ),
        )
        .unwrap();

        let receiver_task = tokio::spawn(receiver.clone().run());
        tokio::time::sleep(Duration::from_millis(250)).await;
        let sender_task = tokio::spawn(sender.clone().run());

        let sender_report = sender_task.await.unwrap().unwrap();
        let receiver_report = receiver_task.await.unwrap().unwrap();
        let total = u64::try_from(alpha.len() + beta.len()).unwrap();

        assert_eq!(sender_report.status, BatchPhase::Completed);
        assert_eq!(receiver_report.status, BatchPhase::Completed);
        assert_eq!(sender_report.total_files, 2);
        assert_eq!(receiver_report.total_files, 2);
        assert_eq!(sender_report.completed_files, 2);
        assert_eq!(receiver_report.completed_files, 2);
        assert_eq!(sender_report.failed_files, 0);
        assert_eq!(receiver_report.failed_files, 0);
        assert_eq!(sender_report.total_bytes, total);
        assert_eq!(receiver_report.transferred_bytes, total);
        assert_eq!(sender_report.files[0].relative_path, "source/alpha.txt");
        assert_eq!(
            sender_report.files[1].relative_path,
            "source/nested/beta.bin"
        );
        assert!(sender_report
            .files
            .iter()
            .chain(receiver_report.files.iter())
            .all(|item| item.status == BatchItemStatus::Completed));
        assert!(receiver_report
            .files
            .iter()
            .all(|item| item.destination_path.is_some()));

        assert_eq!(sender.batch_id(), receiver.batch_id());
        assert_eq!(sender.status(), BatchPhase::Completed);
        assert_eq!(receiver.status(), BatchPhase::Completed);
        assert_eq!(receiver.peer_id().as_deref(), Some("sender"));
        assert_eq!(
            receiver.peer_fingerprint().as_deref(),
            Some(sender_identity.fingerprint.as_str())
        );

        let received_alpha = tokio::fs::read(destination.join("source/alpha.txt"))
            .await
            .unwrap();
        let received_beta = tokio::fs::read(destination.join("source/nested/beta.bin"))
            .await
            .unwrap();
        assert_eq!(received_alpha, alpha);
        assert_eq!(received_beta, beta);

        let snapshot = receiver.snapshot();
        assert_eq!(snapshot.phase, BatchPhase::Completed);
        assert_eq!(snapshot.total_files, 2);
        assert_eq!(snapshot.pending_files, 0);
        assert_eq!(snapshot.active_files, 0);
        assert_eq!(snapshot.completed_files, 2);
        assert_eq!(snapshot.failed_files, 0);
        assert_eq!(snapshot.total_bytes, total);
        assert_eq!(snapshot.transferred_bytes, total);
        assert_eq!(snapshot.remaining_bytes, 0);
        assert_eq!(snapshot.items.len(), 2);
        assert!(snapshot
            .items
            .iter()
            .all(|item| item.status == BatchItemStatus::Completed));

        let sender_events = drain_events(&sender).await;
        assert!(sender_events
            .iter()
            .any(|event| matches!(event, BatchEvent::Started { total_files: 2, .. })));
        assert!(sender_events
            .iter()
            .any(|event| matches!(event, BatchEvent::PeerVerified { .. })));
        assert_eq!(
            sender_events
                .iter()
                .filter(|event| matches!(event, BatchEvent::FileStarted { .. }))
                .count(),
            2
        );
        assert_eq!(
            sender_events
                .iter()
                .filter(|event| matches!(event, BatchEvent::FileCompleted { .. }))
                .count(),
            2
        );
        assert!(sender_events.iter().all(|event| {
            !matches!(
                event,
                BatchEvent::FileCompleted {
                    destination_path: Some(_),
                    ..
                }
            )
        }));
        assert!(matches!(
            sender_events.last(),
            Some(BatchEvent::Finished { report }) if report.completed_files == 2
        ));

        let receiver_events = drain_events(&receiver).await;
        assert!(receiver_events
            .iter()
            .any(|event| matches!(event, BatchEvent::Started { total_files: 2, .. })));
        assert!(receiver_events.iter().any(|event| matches!(
            event,
            BatchEvent::FileCompleted {
                destination_path: Some(_),
                ..
            }
        )));
        assert!(matches!(
            receiver_events.last(),
            Some(BatchEvent::Finished { report }) if report.completed_files == 2
        ));

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pairing_connection_completes_a_batch_receiver_without_files() {
        let sender_peer = crate::crypto::PeerIdentity::generate().unwrap();
        let sender_fingerprint = sender_peer.fingerprint();
        let receiver_identity = test_identity();
        let port = free_port();
        let root = temp_dir("pairing");

        let receiver = BatchTransferSession::receive(
            format!("127.0.0.1:{port}"),
            root.join("destination").to_string_lossy().into_owned(),
            options(&receiver_identity, "receiver", &[], None),
        )
        .unwrap();
        let receiver_task = tokio::spawn(receiver.clone().run());
        tokio::time::sleep(Duration::from_millis(250)).await;

        let pairing = PairingSession::new(
            format!("127.0.0.1:{port}"),
            sender_peer,
            "sender".to_owned(),
            10_000,
        )
        .unwrap();
        let challenge = pairing.clone().start().await.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(10), receiver.next_event())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match event {
            BatchEvent::VerificationRequired {
                sas,
                peer_fingerprint,
                ..
            } => {
                assert_eq!(sas, challenge.sas);
                assert_eq!(peer_fingerprint, sender_fingerprint);
            }
            other => panic!("expected a verification prompt, got {other:?}"),
        }

        receiver.confirm_peer().unwrap();
        let result = pairing.confirm().await.unwrap();
        assert_eq!(result.peer_id, "receiver");
        assert_eq!(result.peer_fingerprint, receiver_identity.fingerprint);

        let report = receiver_task.await.unwrap().unwrap();
        assert_eq!(report.status, BatchPhase::Completed);
        assert_eq!(report.total_files, 0);
        assert_eq!(report.completed_files, 0);
        assert_eq!(report.failed_files, 0);
        assert!(report.files.is_empty());
        assert_eq!(receiver.status(), BatchPhase::Completed);
        assert_eq!(receiver.peer_id().as_deref(), Some("sender"));
        assert_eq!(
            receiver.peer_fingerprint().as_deref(),
            Some(sender_fingerprint.as_str())
        );

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_a_receive_emits_one_terminal_event() {
        let identity = test_identity();
        let root = temp_dir("cancel");
        let receiver = BatchTransferSession::receive(
            "127.0.0.1:0".to_owned(),
            root.join("destination").to_string_lossy().into_owned(),
            options(&identity, "receiver", &[], None),
        )
        .unwrap();

        let task = tokio::spawn(receiver.clone().run());
        tokio::time::sleep(Duration::from_millis(100)).await;
        receiver.cancel();

        let error = task.await.unwrap().unwrap_err();
        assert!(matches!(error, FfiError::Cancelled));
        assert_eq!(receiver.status(), BatchPhase::Cancelled);
        assert!(receiver.report().is_none());
        assert!(receiver.is_cancelled());
        assert_eq!(drain_events(&receiver).await, vec![BatchEvent::Cancelled]);

        let _ = tokio::fs::remove_dir_all(&root).await;
    }
}
