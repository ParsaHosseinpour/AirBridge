//! One bounded, resumable file transfer, driven across the FFI.
//!
//! # What this wraps
//!
//! `libcore_transport` moves one file over one reliable byte stream. It
//! deliberately exposes completion reports, not progress callbacks: the
//! protocol's internal acks are about correctness, not display. This module
//! adds the two things a UI needs without touching the engine:
//!
//! * **Progress.** The TCP stream is wrapped in [`ProgressStream`], which
//!   counts bytes as they are read or written and emits throttled
//!   [`TransferEvent::Progress`] samples. No payload bytes cross the FFI:
//!   the file stays on disk and progress is a pair of integers.
//! * **Cancellation.** [`FileTransferSession::cancel`] flips a
//!   [`Canceller`]; the wrapped stream fails its next poll, the transport
//!   unwinds through its normal error path (aborting its ack pump and leaving
//!   the receiver's part file intact), and `run` resolves to
//!   [`FfiError::Cancelled`].
//!
//! # Threading and lifetime
//!
//! `run` spawns the whole transfer onto the shared runtime; the foreign
//! coroutine/task only awaits a join handle. `cancel` is safe to call from any
//! thread — including a UI thread — at any time between construction and
//! completion. It is idempotent and non-blocking.
//!
//! A session is one-shot: `run` twice returns [`FfiError::AlreadyFinished`].
//! For a retry with the same payload id, construct a new session (the receiver
//! resumes from its persisted part file automatically).

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use libcore_crypto::{fingerprint_hex, Identity};
use libcore_transport as core;
use libcore_transport::{PendingConnection, SecureStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};

use crate::cancel::{Canceller, WaitCancelled};
use crate::crypto::{
    exchange_peer_ids, parse_fingerprint, read_mode, validate_peer_id, write_mode, MODE_PAIRING,
    MODE_TRANSFER,
};
use crate::error::FfiError;
use crate::runtime;

/// Capacity of the public event ring.
///
/// Progress is throttled to 100 ms by default, so this is several seconds of
/// UI samples even for a fast local link. A slower consumer gets
/// [`TransferEvent::Lagged`] rather than an unbounded queue.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Smallest progress interval the adapter will honour.
///
/// A caller that asks for zero (or 1 ms) would otherwise turn every socket
/// poll into a broadcast send. Clamping keeps the FFI channel meaningful and
/// costs nothing for a human-visible progress bar.
const MIN_PROGRESS_INTERVAL_MS: u64 = 16;

/// Direction of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TransferDirection {
    /// This node reads a local file and streams it to the peer.
    Send,
    /// This node waits for a peer and writes the received file.
    Receive,
}

/// Lifecycle state of a session. The authoritative snapshot; the event stream
/// may lag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TransferStatus {
    /// Constructed but `run` has not been called.
    Idle,
    /// Connecting to the peer, or (for a receive) binding and accepting.
    Connecting,
    /// The encrypted handshake completed and the user is confirming the code.
    Verifying,
    /// Bytes are moving.
    Running,
    /// The payload was verified and committed (receive) or acknowledged
    /// (send).
    Completed,
    /// The session ended with an error.
    Failed,
    /// The session was stopped by `cancel`.
    Cancelled,
}

/// Outcome of a completed transfer, for either direction.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TransferReport {
    /// Identifier of the payload that was moved.
    pub payload_id: u64,
    /// Total size of the payload, in bytes.
    pub total_bytes: u64,
    /// Byte offset the transfer started from: zero on a fresh payload, the
    /// receiver's verified prefix on a resumed one.
    pub resumed_from: u64,
    /// Payload bytes moved through the encrypted stream adapter. This equals
    /// `total_bytes - resumed_from` on a clean transfer and is larger when the
    /// receiver requested repairs.
    pub bytes_on_wire: u64,
    /// Chunks written to the socket (sends only), retransmissions included.
    pub chunks_sent: Option<u64>,
    /// Where the payload was committed (receives only).
    pub destination_path: Option<String>,
    /// Peer id claimed *after* the authenticated handshake.
    pub peer_id: Option<String>,
    /// SHA-256 fingerprint of the peer's long-lived identity key.
    pub peer_fingerprint: Option<String>,
    /// True when this connection performed a pairing-only exchange; no file
    /// was moved and `destination_path` is `None`.
    pub pairing_only: bool,
}

/// Events emitted by a running session.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TransferEvent {
    /// The session connected/bound successfully and is about to move bytes.
    Started {
        /// Payload size when known up front. A receive cannot know it until
        /// the sender's `HELLO` arrives inside the transport engine, so it is
        /// `None`; the completed [`TransferReport`] carries the final size.
        total_bytes: Option<u64>,
    },
    /// The handshake completed and the peer's identity is waiting for a
    /// decision. Compare `sas` with the other screen, then call
    /// [`FileTransferSession::confirm_peer`] or
    /// [`FileTransferSession::reject_peer`].
    VerificationRequired {
        /// Six-digit Short Authentication String, derived from the handshake
        /// transcript that includes both identity keys.
        sas: String,
        /// SHA-256 fingerprint of the peer's identity key.
        peer_fingerprint: String,
        /// The peer's uncompressed P-256 public key, for diagnostics.
        peer_public_key: Vec<u8>,
    },
    /// The peer was authenticated and its peer id was bound to the fingerprint.
    PeerVerified {
        /// Claimed peer id, received only after the handshake.
        peer_id: String,
        /// Fingerprint the id is bound to.
        peer_fingerprint: String,
    },
    /// Throttled progress sample. `bytes_transferred` is plaintext payload
    /// counted by the adapter, not a percentage.
    Progress {
        /// Bytes moved so far.
        bytes_transferred: u64,
        /// Payload size when known up front.
        total_bytes: Option<u64>,
    },
    /// Success. The same report is returned by `run` and by
    /// [`FileTransferSession::report`].
    Completed {
        /// The completed transfer.
        report: TransferReport,
    },
    /// Terminal failure. `run` throws the same information as an
    /// [`FfiError`]; observers that only drain events can still classify it.
    Failed {
        /// Short machine-readable kind, e.g. `"io"` or `"checksum_mismatch"`.
        kind: String,
        /// Human-readable detail.
        message: String,
    },
    /// Terminal cancellation.
    Cancelled,
    /// The consumer fell behind and `missed` events were dropped. Re-read
    /// [`FileTransferSession::status`] and [`FileTransferSession::report`].
    Lagged {
        /// Number of dropped events.
        missed: u64,
    },
}

/// Tunables for an upload.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SendOptions {
    /// Payload id to use. `None` generates one at construction (read it back
    /// with [`FileTransferSession::payload_id`] so it can be sent to the peer
    /// over your control channel). Re-use the id to resume a failed transfer.
    #[uniffi(default)]
    pub payload_id: Option<u64>,
    /// Chunk grid size; 512 KiB by default.
    #[uniffi(default = 524288)]
    pub chunk_size: u32,
    /// Compute and announce the whole-file SHA-256, enabling the receiver's
    /// end-to-end result check.
    #[uniffi(default = true)]
    pub verify_whole_file: bool,
    /// Deadline for the receiver's opening ack and final `COMPLETE`.
    #[uniffi(default = 30000)]
    pub response_timeout_ms: u64,
    /// Minimum interval between progress events.
    #[uniffi(default = 100)]
    pub progress_interval_ms: u64,
    /// This device's long-lived identity, as PKCS#8 DER. Generate it once
    /// with [`crate::PeerIdentity::generate`] and persist it privately.
    pub identity_pkcs8: Vec<u8>,
    /// This device's discovery peer id. It is exchanged only after the
    /// authenticated handshake.
    #[uniffi(default = "")]
    pub local_peer_id: String,
    /// Fingerprints previously verified by the user. A peer whose fingerprint
    /// is in this list is confirmed automatically, without a prompt.
    pub trusted_peer_fingerprints: Vec<String>,
    /// When set, the peer must present exactly this fingerprint. Use it to pin
    /// a device across retries; any other identity fails with
    /// [`FfiError::IdentityMismatch`].
    #[uniffi(default)]
    pub expected_peer_fingerprint: Option<String>,
    /// Deadline for the four-message handshake and the identity preamble.
    #[uniffi(default = 30000)]
    pub handshake_timeout_ms: u64,
    /// How long an interactive verification may wait for
    /// [`FileTransferSession::confirm_peer`] or
    /// [`FileTransferSession::reject_peer`].
    #[uniffi(default = 120000)]
    pub verification_timeout_ms: u64,
}

impl SendOptions {
    fn security(&self) -> Result<SecurityConfig, FfiError> {
        core::ChunkPlan::new(self.chunk_size).map_err(FfiError::from)?;
        if self.response_timeout_ms == 0 {
            return Err(FfiError::invalid_argument(
                "response_timeout_ms must be non-zero",
            ));
        }
        if self.progress_interval_ms == 0 {
            return Err(FfiError::invalid_argument(
                "progress_interval_ms must be non-zero",
            ));
        }
        SecurityConfig::parse(
            &self.identity_pkcs8,
            &self.local_peer_id,
            self.trusted_peer_fingerprints.as_slice(),
            self.expected_peer_fingerprint.as_deref(),
            self.handshake_timeout_ms,
            self.verification_timeout_ms,
        )
    }
}

/// Tunables for a download.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ReceiveOptions {
    /// Reject a `HELLO` for any other payload id. Set this when the app knows
    /// which transfer it expects; `None` accepts whatever arrives first.
    #[uniffi(default)]
    pub expected_payload_id: Option<u64>,
    /// How many verified bytes may accumulate before a progress ack is sent.
    #[uniffi(default = 4194304)]
    pub ack_every_bytes: u64,
    /// Verify the prefix and whole-file digests.
    #[uniffi(default = true)]
    pub verify_whole_file: bool,
    /// How long to wait for the sender to connect before giving up.
    #[uniffi(default = 60000)]
    pub accept_timeout_ms: u64,
    /// Minimum interval between progress events.
    #[uniffi(default = 100)]
    pub progress_interval_ms: u64,
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
    /// Deadline for the four-message handshake and the identity preamble.
    #[uniffi(default = 30000)]
    pub handshake_timeout_ms: u64,
    /// How long an interactive verification may wait for
    /// [`FileTransferSession::confirm_peer`] or
    /// [`FileTransferSession::reject_peer`].
    #[uniffi(default = 120000)]
    pub verification_timeout_ms: u64,
}

impl ReceiveOptions {
    fn security(&self) -> Result<SecurityConfig, FfiError> {
        if self.ack_every_bytes == 0 {
            return Err(FfiError::invalid_argument(
                "ack_every_bytes must be non-zero",
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
        SecurityConfig::parse(
            &self.identity_pkcs8,
            &self.local_peer_id,
            self.trusted_peer_fingerprints.as_slice(),
            self.expected_peer_fingerprint.as_deref(),
            self.handshake_timeout_ms,
            self.verification_timeout_ms,
        )
    }
}

/// Validated identity, trust and deadline configuration.
struct SecurityConfig {
    identity: Arc<Identity>,
    local_peer_id: String,
    expected_peer_fingerprint: Option<[u8; 32]>,
    trusted_peer_fingerprints: Vec<[u8; 32]>,
    handshake_timeout: Duration,
    verification_timeout: Duration,
}

impl SecurityConfig {
    fn parse(
        identity_pkcs8: &[u8],
        local_peer_id: &str,
        trusted: &[String],
        expected: Option<&str>,
        handshake_timeout_ms: u64,
        verification_timeout_ms: u64,
    ) -> Result<Self, FfiError> {
        if identity_pkcs8.is_empty() {
            return Err(FfiError::invalid_argument(
                "identity_pkcs8 must contain a persisted PeerIdentity key",
            ));
        }
        let identity = Identity::from_pkcs8(identity_pkcs8).map_err(|error| {
            FfiError::invalid_argument(format!("identity_pkcs8 is not a valid key: {error}"))
        })?;
        validate_peer_id(local_peer_id)?;
        if handshake_timeout_ms == 0 {
            return Err(FfiError::invalid_argument(
                "handshake_timeout_ms must be non-zero",
            ));
        }
        if verification_timeout_ms == 0 {
            return Err(FfiError::invalid_argument(
                "verification_timeout_ms must be non-zero",
            ));
        }

        let expected_peer_fingerprint = expected
            .map(|value| parse_fingerprint(value, "expected_peer_fingerprint"))
            .transpose()?;
        let mut parsed_trusted = Vec::with_capacity(trusted.len());
        for (index, value) in trusted.iter().enumerate() {
            parsed_trusted.push(parse_fingerprint(
                value,
                &format!("trusted_peer_fingerprints[{index}]"),
            )?);
        }

        Ok(Self {
            identity: Arc::new(identity),
            local_peer_id: local_peer_id.to_owned(),
            expected_peer_fingerprint,
            trusted_peer_fingerprints: parsed_trusted,
            handshake_timeout: Duration::from_millis(handshake_timeout_ms),
            verification_timeout: Duration::from_millis(verification_timeout_ms),
        })
    }

    fn trusts(&self, fingerprint: &[u8; 32]) -> bool {
        self.trusted_peer_fingerprints.contains(fingerprint)
    }
}

/// An explicit answer to [`TransferEvent::VerificationRequired`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerDecision {
    Confirm,
    Reject,
}

/// How the session was configured.
#[derive(Clone, Debug)]
enum Mode {
    Send(SendOptions),
    Receive(ReceiveOptions),
}

/// Authoritative session snapshot.
#[derive(Clone, Debug)]
struct SessionState {
    status: TransferStatus,
    report: Option<TransferReport>,
    payload_id: Option<u64>,
    peer_id: Option<String>,
    peer_fingerprint: Option<String>,
}

/// The state every exported method shares.
struct SessionInner {
    direction: TransferDirection,
    /// `host:port` to connect to (send) or `ip:port` to bind (receive).
    address: String,
    /// Source file (send) or destination file (receive).
    path: String,
    mode: Mode,
    security: SecurityConfig,
    canceller: Arc<Canceller>,
    state: Mutex<SessionState>,
    /// The event sender; taken and dropped when the session reaches a
    /// terminal state, which closes the stream after the terminal event has
    /// been buffered.
    events: Mutex<Option<broadcast::Sender<TransferEvent>>>,
    receiver: Arc<AsyncMutex<Option<broadcast::Receiver<TransferEvent>>>>,
    /// Receives the user's answer to a pending SAS prompt.
    decision: Mutex<Option<oneshot::Sender<PeerDecision>>>,
    started: AtomicBool,
}

impl SessionInner {
    fn set_status(&self, status: TransferStatus) {
        lock(&self.state).status = status;
    }

    fn set_payload_id(&self, payload_id: u64) {
        lock(&self.state).payload_id = Some(payload_id);
    }

    fn set_report(&self, report: TransferReport) {
        lock(&self.state).report = Some(report);
    }

    /// Emits an event if the stream is still open.
    fn emit(&self, event: TransferEvent) {
        let guard = lock(&self.events);
        if let Some(sender) = guard.as_ref() {
            // `send` never blocks: slow receivers lag and are told so.
            let _ = sender.send(event);
        }
    }

    /// Publishes a terminal event, then closes the stream.
    fn finish(&self, status: TransferStatus, event: TransferEvent) {
        self.set_status(status);
        self.emit(event);
        // Dropping the sender lets receivers drain buffered events and then
        // observe `Closed`, which is what terminates a foreign flow/task.
        let _ = lock(&self.events).take();
    }

    /// Runs the whole session. Always called on the shared runtime.
    async fn run(&self) -> Result<TransferReport, FfiError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(FfiError::AlreadyFinished);
        }
        self.set_status(TransferStatus::Connecting);

        let result = match &self.mode {
            Mode::Send(options) => self.upload(options.clone()).await,
            Mode::Receive(options) => self.receive(options.clone()).await,
        };

        match result {
            Ok(report) => {
                self.set_report(report.clone());
                self.finish(
                    TransferStatus::Completed,
                    TransferEvent::Completed {
                        report: report.clone(),
                    },
                );
                Ok(report)
            }
            Err(error) => {
                if matches!(error, FfiError::Cancelled) {
                    self.finish(TransferStatus::Cancelled, TransferEvent::Cancelled);
                } else {
                    self.finish(
                        TransferStatus::Failed,
                        TransferEvent::Failed {
                            kind: error.kind().to_owned(),
                            message: error.to_string(),
                        },
                    );
                }
                Err(error)
            }
        }
    }

    /// Connects, authenticates, streams the file and waits for `COMPLETE`.
    async fn upload(&self, options: SendOptions) -> Result<TransferReport, FfiError> {
        let total = tokio::fs::metadata(&self.path)
            .await
            .map_err(io_error)?
            .len();

        let stream = self.connect().await?;
        // Nagle would only add latency to the small control frames.
        let _ = stream.set_nodelay(true);

        let (stream, _mode) = self.establish(stream, true).await?;
        let peer_id = self.peer_id();
        let peer_fingerprint = self.peer_fingerprint();

        let payload_id = lock(&self.state)
            .payload_id
            .ok_or_else(|| FfiError::internal("the payload id was not chosen at construction"))?;
        let payload_id = core::PayloadId::new(payload_id);

        let counter = Arc::new(AtomicU64::new(0));
        self.set_status(TransferStatus::Running);
        self.emit(TransferEvent::Started {
            total_bytes: Some(total),
        });

        let stream = ProgressStream::new(
            stream,
            Some(total),
            Arc::clone(&counter),
            Arc::clone(&self.canceller),
            throttled_interval(options.progress_interval_ms),
            self.event_sender(),
        );

        let upload_options = core::UploadOptions::new(payload_id)
            .with_chunk_size(options.chunk_size)
            .with_file_sha256(options.verify_whole_file)
            .with_response_timeout(Duration::from_millis(options.response_timeout_ms));

        let upload = core::upload_file(stream, Path::new(&self.path), &upload_options);
        let result = drive(&self.canceller, upload).await;

        match result {
            Ok(report) => Ok(TransferReport {
                payload_id: report.payload_id.get(),
                total_bytes: report.total_len,
                resumed_from: report.resumed_from,
                bytes_on_wire: counter.load(Ordering::Relaxed),
                chunks_sent: Some(report.chunks_sent),
                destination_path: None,
                peer_id,
                peer_fingerprint,
                pairing_only: false,
            }),
            Err(error) => Err(cancel_or(error, &self.canceller)),
        }
    }

    /// Binds, accepts exactly one sender, and receives the payload.
    async fn receive(&self, options: ReceiveOptions) -> Result<TransferReport, FfiError> {
        let listener = TcpListener::bind(self.address.as_str())
            .await
            .map_err(io_error)?;

        let accept = tokio::time::timeout(
            Duration::from_millis(options.accept_timeout_ms),
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
                        message: "waiting for an inbound transfer connection".to_owned(),
                    })
                }
            },
            () = WaitCancelled(self.canceller.as_ref()) => return Err(FfiError::Cancelled),
        };
        let _ = stream.set_nodelay(true);

        let (stream, mode) = self.establish(stream, false).await?;
        let peer_id = self.peer_id();
        let peer_fingerprint = self.peer_fingerprint();

        if mode == MODE_PAIRING {
            // A pairing connection carries no payload. Completing normally
            // lets a long-lived receiver re-arm without showing an offer.
            return Ok(TransferReport {
                payload_id: 0,
                total_bytes: 0,
                resumed_from: 0,
                bytes_on_wire: 0,
                chunks_sent: None,
                destination_path: None,
                peer_id,
                peer_fingerprint,
                pairing_only: true,
            });
        }

        let counter = Arc::new(AtomicU64::new(0));
        self.set_status(TransferStatus::Running);
        self.emit(TransferEvent::Started { total_bytes: None });

        let stream = ProgressStream::new(
            stream,
            None,
            Arc::clone(&counter),
            Arc::clone(&self.canceller),
            throttled_interval(options.progress_interval_ms),
            self.event_sender(),
        );

        let mut receive_options = core::ReceiveOptions::new()
            .with_ack_every_bytes(options.ack_every_bytes)
            .with_file_sha256(options.verify_whole_file);
        if let Some(expected) = options.expected_payload_id {
            receive_options =
                receive_options.with_expected_payload_id(core::PayloadId::new(expected));
        }

        let receive = core::receive_file(stream, Path::new(&self.path), &receive_options);
        let result = drive(&self.canceller, receive).await;

        match result {
            Ok(report) => {
                self.set_payload_id(report.payload_id.get());
                Ok(TransferReport {
                    payload_id: report.payload_id.get(),
                    total_bytes: report.total_len,
                    resumed_from: report.resumed_from,
                    bytes_on_wire: counter.load(Ordering::Relaxed),
                    chunks_sent: None,
                    destination_path: Some(report.path.to_string_lossy().into_owned()),
                    peer_id,
                    peer_fingerprint,
                    pairing_only: false,
                })
            }
            Err(error) => Err(cancel_or(error, &self.canceller)),
        }
    }

    /// Runs the handshake, applies the identity policy and exchanges the
    /// authenticated peer id plus the application mode.
    async fn establish(
        &self,
        stream: TcpStream,
        initiator: bool,
    ) -> Result<(SecureStream<TcpStream>, u8), FfiError> {
        let timeout = self.security.handshake_timeout;
        let identity = Arc::clone(&self.security.identity);
        let handshake = async move {
            if initiator {
                core::initiate(stream, identity, timeout).await
            } else {
                core::respond(stream, identity, timeout).await
            }
        };

        let pending = tokio::select! {
            biased;
            result = handshake => result?,
            () = WaitCancelled(self.canceller.as_ref()) => return Err(FfiError::Cancelled),
        };

        let fingerprint = pending.peer_fingerprint().ok_or_else(|| FfiError::Crypto {
            message: "the peer did not present an identity key".to_owned(),
        })?;

        if let Some(expected) = self.security.expected_peer_fingerprint {
            if expected != fingerprint {
                pending.reject();
                return Err(FfiError::IdentityMismatch {
                    expected: fingerprint_hex(&expected),
                    actual: fingerprint_hex(&fingerprint),
                });
            }
        }

        let mut secure = if self.security.trusts(&fingerprint) {
            pending.confirm()?
        } else {
            self.verify(pending, &fingerprint).await?
        };

        let exchange = async {
            let peer_id = exchange_peer_ids(&mut secure, &self.security.local_peer_id).await?;
            let mode = if initiator {
                write_mode(&mut secure, MODE_TRANSFER).await?;
                let echoed = read_mode(&mut secure).await?;
                if echoed != MODE_TRANSFER {
                    return Err(FfiError::Protocol {
                        message: "the peer did not acknowledge transfer mode".to_owned(),
                    });
                }
                MODE_TRANSFER
            } else {
                let requested = read_mode(&mut secure).await?;
                write_mode(&mut secure, requested).await?;
                requested
            };
            Ok::<_, FfiError>((peer_id, mode))
        };

        let (peer_id, mode) = tokio::select! {
            biased;
            result = exchange => result?,
            () = WaitCancelled(self.canceller.as_ref()) => return Err(FfiError::Cancelled),
        };

        self.set_peer(peer_id, fingerprint);
        Ok((secure, mode))
    }

    /// Shows the SAS and waits for the foreign UI's decision.
    async fn verify(
        &self,
        pending: PendingConnection<TcpStream>,
        fingerprint: &[u8; 32],
    ) -> Result<SecureStream<TcpStream>, FfiError> {
        let sas = pending.sas();
        let challenge = TransferEvent::VerificationRequired {
            sas: sas.to_padded_string(),
            peer_fingerprint: fingerprint_hex(fingerprint),
            peer_public_key: pending
                .peer_identity_public_key()
                .unwrap_or_default()
                .to_vec(),
        };

        let (sender, receiver) = oneshot::channel();
        {
            let mut slot = lock(&self.decision);
            if slot.is_some() {
                pending.reject();
                return Err(FfiError::Protocol {
                    message: "a verification is already pending".to_owned(),
                });
            }
            *slot = Some(sender);
        }

        self.set_status(TransferStatus::Verifying);
        self.emit(challenge);

        let decision = tokio::select! {
            biased;
            result = tokio::time::timeout(self.security.verification_timeout, receiver) => {
                match result {
                    Ok(Ok(decision)) => decision,
                    // The sender was dropped without an answer.
                    Ok(Err(_)) => PeerDecision::Reject,
                    Err(_) => {
                        lock(&self.decision).take();
                        pending.reject();
                        return Err(FfiError::Timeout {
                            message: "waiting for the user to verify the peer".to_owned(),
                        });
                    }
                }
            }
            () = WaitCancelled(self.canceller.as_ref()) => {
                lock(&self.decision).take();
                pending.reject();
                return Err(FfiError::Cancelled);
            }
        };
        lock(&self.decision).take();

        match decision {
            PeerDecision::Confirm => Ok(pending.confirm()?),
            PeerDecision::Reject => {
                pending.reject();
                Err(FfiError::VerificationRejected)
            }
        }
    }

    /// Records and publishes the binding between fingerprint and peer id.
    fn set_peer(&self, peer_id: String, fingerprint: [u8; 32]) {
        let peer_fingerprint = fingerprint_hex(&fingerprint);
        {
            let mut state = lock(&self.state);
            state.peer_id = Some(peer_id.clone());
            state.peer_fingerprint = Some(peer_fingerprint.clone());
        }
        self.emit(TransferEvent::PeerVerified {
            peer_id,
            peer_fingerprint,
        });
    }

    fn peer_id(&self) -> Option<String> {
        lock(&self.state).peer_id.clone()
    }

    fn peer_fingerprint(&self) -> Option<String> {
        lock(&self.state).peer_fingerprint.clone()
    }

    /// Answers a pending [`TransferEvent::VerificationRequired`] with approval.
    fn confirm_peer(&self) -> Result<(), FfiError> {
        self.answer_peer(PeerDecision::Confirm)
    }

    /// Answers a pending [`TransferEvent::VerificationRequired`] by declining.
    fn reject_peer(&self) -> Result<(), FfiError> {
        self.answer_peer(PeerDecision::Reject)
    }

    fn answer_peer(&self, decision: PeerDecision) -> Result<(), FfiError> {
        let sender = lock(&self.decision)
            .take()
            .ok_or_else(|| FfiError::invalid_argument("no peer verification is pending"))?;
        sender
            .send(decision)
            .map_err(|_| FfiError::VerificationRejected)
    }

    /// Connects to the sender's chosen peer, observing cancellation.
    async fn connect(&self) -> Result<TcpStream, FfiError> {
        let connect = TcpStream::connect(self.address.as_str());
        tokio::pin!(connect);
        tokio::select! {
            biased;
            result = &mut connect => result.map_err(io_error),
            () = WaitCancelled(self.canceller.as_ref()) => Err(FfiError::Cancelled),
        }
    }

    /// A clone of the event sender, or a closed dummy channel.
    ///
    /// The adapter can outlive `finish` by a few instructions; emitting into
    /// a closed channel is a no-op, so a dummy sender keeps the type simple.
    fn event_sender(&self) -> broadcast::Sender<TransferEvent> {
        lock(&self.events).as_ref().map_or_else(
            || {
                // No receiver is kept; sends fail and are ignored.
                let (sender, _receiver) = broadcast::channel(1);
                sender
            },
            Clone::clone,
        )
    }
}

/// One bounded transfer over one connection.
///
/// Construct with [`Self::send`] or [`Self::receive`], then call
/// [`Self::run`] once and drain [`Self::next_event`] concurrently. Dropping the
/// last reference before `run` completes leaves the transfer running until it
/// finishes; call [`Self::cancel`] first if that is not what you want.
#[derive(uniffi::Object)]
pub struct FileTransferSession {
    inner: Arc<SessionInner>,
}

#[uniffi::export]
impl FileTransferSession {
    /// Creates a sender that will stream `file_path` to `address`
    /// (`host:port`; DNS names are resolved by the runtime).
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] for an empty address or path, an
    /// unsupported chunk size, a zero timeout, or a failed payload-id
    /// generation.
    #[uniffi::constructor]
    pub fn send(
        address: String,
        file_path: String,
        options: SendOptions,
    ) -> Result<Arc<Self>, FfiError> {
        let security = options.security()?;
        validate_endpoint(&address, &file_path)?;
        let payload_id = match options.payload_id {
            Some(id) => id,
            None => core::PayloadId::random().map_err(FfiError::from)?.get(),
        };
        Ok(Arc::new(Self::assemble(
            TransferDirection::Send,
            address,
            file_path,
            Mode::Send(options),
            Some(payload_id),
            security,
        )))
    }

    /// Creates a receiver that binds `listen_address` (`ip:port`) and accepts
    /// exactly one inbound transfer into `destination`.
    ///
    /// `destination` is only created by the final rename; bytes in flight live
    /// in `destination.part` and survive failures.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] for an empty address or path, a
    /// zero timeout/ack cadence, or a malformed expected payload id.
    #[uniffi::constructor]
    pub fn receive(
        listen_address: String,
        destination: String,
        options: ReceiveOptions,
    ) -> Result<Arc<Self>, FfiError> {
        let security = options.security()?;
        validate_endpoint(&listen_address, &destination)?;
        let payload_id = options.expected_payload_id;
        Ok(Arc::new(Self::assemble(
            TransferDirection::Receive,
            listen_address,
            destination,
            Mode::Receive(options),
            payload_id,
            security,
        )))
    }

    /// Returns which way bytes will flow.
    #[must_use]
    pub fn direction(&self) -> TransferDirection {
        self.inner.direction
    }

    /// Returns the payload id, when known.
    ///
    /// A sender always knows it (generated at construction if not supplied).
    /// A receiver knows it when `expected_payload_id` was set, or after the
    /// transfer completes.
    #[must_use]
    pub fn payload_id(&self) -> Option<u64> {
        lock(&self.inner.state).payload_id
    }

    /// Returns the authoritative lifecycle state.
    #[must_use]
    pub fn status(&self) -> TransferStatus {
        lock(&self.inner.state).status
    }

    /// Returns the completion report, once the transfer has completed.
    #[must_use]
    pub fn report(&self) -> Option<TransferReport> {
        lock(&self.inner.state).report.clone()
    }

    /// Returns the peer id claimed after the authenticated handshake.
    ///
    /// The value is only meaningful once the connection has reached the
    /// verification or running phase; it is bound to
    /// [`Self::peer_fingerprint`] and never used for trust before that.
    #[must_use]
    pub fn peer_id(&self) -> Option<String> {
        self.inner.peer_id()
    }

    /// Returns the SHA-256 fingerprint of the peer's identity key.
    #[must_use]
    pub fn peer_fingerprint(&self) -> Option<String> {
        self.inner.peer_fingerprint()
    }

    /// Confirms the code shown by [`TransferEvent::VerificationRequired`].
    ///
    /// The session resumes as soon as the decision is delivered. Calling this
    /// without a pending verification returns [`FfiError::InvalidArgument`].
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] when no verification is pending,
    /// or [`FfiError::VerificationRejected`] when the session has already
    /// unwound.
    pub fn confirm_peer(&self) -> Result<(), FfiError> {
        self.inner.confirm_peer()
    }

    /// Declines the code shown by [`TransferEvent::VerificationRequired`].
    ///
    /// The connection is dropped and `run` resolves to
    /// [`FfiError::VerificationRejected`].
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] when no verification is pending.
    pub fn reject_peer(&self) -> Result<(), FfiError> {
        self.inner.reject_peer()
    }

    /// Requests cancellation.
    ///
    /// Non-blocking, idempotent and callable from any thread. The client is
    /// unblocked immediately; the transfer unwinds on the runtime and `run`
    /// resolves to [`FfiError::Cancelled`]. The peer sees a closed connection,
    /// and the receiver's persisted part file remains resumable.
    pub fn cancel(&self) {
        self.inner.canceller.request();
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.canceller.is_requested()
    }

    /// Runs the transfer and returns its report.
    ///
    /// Call this once. Cancellation is prompt while connecting, accepting,
    /// reading and writing; the final `COMPLETE` handshake is bounded by the
    /// configured response timeout.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::AlreadyFinished`] on a second call; otherwise the
    /// mapped transport error, with [`FfiError::Cancelled`] taking precedence
    /// when `cancel` was called.
    pub async fn run(self: Arc<Self>) -> Result<TransferReport, FfiError> {
        let inner = Arc::clone(&self.inner);
        runtime::run_on_runtime(async move { inner.run().await }).await?
    }

    /// Waits for the next progress/status event.
    ///
    /// Resolves to `None` after the terminal event once the session has closed
    /// its stream. One consumer at a time; concurrent calls serialize.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::Internal`] if the shared runtime is unavailable.
    pub async fn next_event(&self) -> Result<Option<TransferEvent>, FfiError> {
        let receiver = Arc::clone(&self.inner.receiver);
        runtime::run_on_runtime(async move {
            let mut guard = receiver.lock().await;
            let Some(receiver) = guard.as_mut() else {
                return Ok(None);
            };
            match receiver.recv().await {
                Ok(event) => Ok(Some(event)),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    Ok(Some(TransferEvent::Lagged { missed }))
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

impl FileTransferSession {
    fn assemble(
        direction: TransferDirection,
        address: String,
        path: String,
        mode: Mode,
        payload_id: Option<u64>,
        security: SecurityConfig,
    ) -> Self {
        let (sender, receiver) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(SessionInner {
                direction,
                address,
                path,
                mode,
                security,
                canceller: Arc::new(Canceller::new()),
                state: Mutex::new(SessionState {
                    status: TransferStatus::Idle,
                    report: None,
                    payload_id,
                    peer_id: None,
                    peer_fingerprint: None,
                }),
                events: Mutex::new(Some(sender)),
                receiver: Arc::new(AsyncMutex::new(Some(receiver))),
                decision: Mutex::new(None),
                started: AtomicBool::new(false),
            }),
        }
    }
}

/// Rejects empty endpoints; syntax errors surface from bind/connect as I/O
/// errors with the platform's own message.
fn validate_endpoint(address: &str, path: &str) -> Result<(), FfiError> {
    if address.trim().is_empty() {
        return Err(FfiError::invalid_argument("address must not be empty"));
    }
    if path.trim().is_empty() {
        return Err(FfiError::invalid_argument("file path must not be empty"));
    }
    Ok(())
}

/// Clamps a requested progress interval to something a channel can absorb.
fn throttled_interval(requested_ms: u64) -> u64 {
    requested_ms.max(MIN_PROGRESS_INTERVAL_MS)
}

/// Maps a raw I/O failure through the transport error type for consistent
/// classification.
pub(crate) fn io_error(error: io::Error) -> FfiError {
    FfiError::from(core::TransferError::Io(error))
}

/// Cancellation wins over whatever error the unwinding transfer reported.
fn cancel_or(error: core::TransferError, canceller: &Canceller) -> FfiError {
    if canceller.is_requested() {
        FfiError::Cancelled
    } else {
        error.into()
    }
}

/// Drives `future`, but stops waiting on cancellation and lets it unwind.
///
/// Dropping a transfer future would leak the transport's ack-pump task, so the
/// cancel path *awaits* the future instead: the wrapped stream observes the
/// flag on its next poll and fails, which runs the engine's normal cleanup
/// (the pump is aborted, the part file and sidecar stay for a retry).
async fn drive<F>(canceller: &Canceller, future: F) -> F::Output
where
    F: Future,
{
    let mut future = Box::pin(future);
    let outcome = tokio::select! {
        biased;
        result = &mut future => Some(result),
        () = WaitCancelled(canceller) => None,
    };
    match outcome {
        Some(result) => result,
        // The canceller is now set; the next poll of the stream fails.
        None => future.await,
    }
}

/// A stream wrapper that counts bytes and samples progress.
///
/// It sits between the transport engine and the socket. Reads are counted for
/// a receive, writes for a send. Cancellation is checked on every poll and the
/// task is woken by the [`Canceller`], so a socket blocked on a silent peer
/// still terminates promptly.
struct ProgressStream<S> {
    inner: S,
    total: Option<u64>,
    counter: Arc<AtomicU64>,
    canceller: Arc<Canceller>,
    started: Instant,
    interval_ms: u64,
    last_emit_ms: u64,
    sender: broadcast::Sender<TransferEvent>,
}

impl<S> ProgressStream<S> {
    fn new(
        inner: S,
        total: Option<u64>,
        counter: Arc<AtomicU64>,
        canceller: Arc<Canceller>,
        interval_ms: u64,
        sender: broadcast::Sender<TransferEvent>,
    ) -> Self {
        Self {
            inner,
            total,
            counter,
            canceller,
            started: Instant::now(),
            interval_ms,
            last_emit_ms: 0,
            sender,
        }
    }

    /// Counts `delta` bytes and emits a sample when the interval has elapsed.
    fn record(&mut self, delta: usize) {
        if delta == 0 {
            return;
        }
        let delta = u64::try_from(delta).unwrap_or(u64::MAX);
        let total = self.counter.fetch_add(delta, Ordering::Relaxed) + delta;
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if elapsed_ms >= self.last_emit_ms.saturating_add(self.interval_ms) {
            self.last_emit_ms = elapsed_ms;
            let _ = self.sender.send(TransferEvent::Progress {
                bytes_transferred: total,
                total_bytes: self.total,
            });
        }
    }

    /// Registers the task for cancellation wakeup and reports whether the
    /// transfer must stop. See [`Canceller`] for the ordering argument.
    fn poll_cancelled(&self, cx: &mut Context<'_>) -> bool {
        self.canceller.poll_cancelled(cx)
    }
}

impl<S> AsyncRead for ProgressStream<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.poll_cancelled(cx) {
            return Poll::Ready(Err(cancelled_io_error()));
        }
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            let delta = buf.filled().len().saturating_sub(before);
            this.record(delta);
        }
        result
    }
}

impl<S> AsyncWrite for ProgressStream<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.poll_cancelled(cx) {
            return Poll::Ready(Err(cancelled_io_error()));
        }
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(written)) => {
                this.record(written);
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// The error a cancelled stream returns; classification happens in
/// [`cancel_or`], not by inspecting this value.
fn cancelled_io_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "transfer cancelled by the local application",
    )
}

/// Locks a std mutex, recovering from poisoning.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::crypto::{PairingSession, PeerIdentity};

    fn security_options() -> (Vec<u8>, String) {
        let identity = Identity::generate().unwrap();
        (identity.to_pkcs8().to_vec(), "test-peer".to_owned())
    }

    #[test]
    fn constructors_choose_and_expose_payload_ids() {
        let (identity_pkcs8, local_peer_id) = security_options();
        let sender = FileTransferSession::send(
            "127.0.0.1:1".to_owned(),
            "file.bin".to_owned(),
            SendOptions {
                payload_id: Some(7),
                chunk_size: core::CHUNK_SIZE,
                verify_whole_file: true,
                response_timeout_ms: 1_000,
                progress_interval_ms: 100,
                identity_pkcs8: identity_pkcs8.clone(),
                local_peer_id: local_peer_id.clone(),
                trusted_peer_fingerprints: Vec::new(),
                expected_peer_fingerprint: None,
                handshake_timeout_ms: 1_000,
                verification_timeout_ms: 1_000,
            },
        )
        .unwrap();
        assert_eq!(sender.payload_id(), Some(7));
        assert_eq!(sender.direction(), TransferDirection::Send);
        assert_eq!(sender.status(), TransferStatus::Idle);
        assert_eq!(sender.peer_id(), None);

        let receiver = FileTransferSession::receive(
            "127.0.0.1:0".to_owned(),
            "file.bin".to_owned(),
            ReceiveOptions {
                expected_payload_id: Some(7),
                ack_every_bytes: 1024,
                verify_whole_file: false,
                accept_timeout_ms: 1_000,
                progress_interval_ms: 100,
                identity_pkcs8,
                local_peer_id,
                trusted_peer_fingerprints: Vec::new(),
                expected_peer_fingerprint: None,
                handshake_timeout_ms: 1_000,
                verification_timeout_ms: 1_000,
            },
        )
        .unwrap();
        assert_eq!(receiver.payload_id(), Some(7));
        assert_eq!(receiver.direction(), TransferDirection::Receive);
    }

    #[test]
    fn invalid_options_are_rejected_before_any_io() {
        let (identity_pkcs8, local_peer_id) = security_options();
        let bad_chunk = SendOptions {
            payload_id: None,
            chunk_size: 0,
            verify_whole_file: true,
            response_timeout_ms: 1_000,
            progress_interval_ms: 100,
            identity_pkcs8: identity_pkcs8.clone(),
            local_peer_id: local_peer_id.clone(),
            trusted_peer_fingerprints: Vec::new(),
            expected_peer_fingerprint: None,
            handshake_timeout_ms: 1_000,
            verification_timeout_ms: 1_000,
        };
        assert!(matches!(
            FileTransferSession::send("host:1".into(), "f".into(), bad_chunk),
            Err(FfiError::InvalidArgument { .. })
        ));

        let empty_path = SendOptions {
            payload_id: None,
            chunk_size: core::CHUNK_SIZE,
            verify_whole_file: true,
            response_timeout_ms: 1_000,
            progress_interval_ms: 100,
            identity_pkcs8,
            local_peer_id,
            trusted_peer_fingerprints: Vec::new(),
            expected_peer_fingerprint: None,
            handshake_timeout_ms: 1_000,
            verification_timeout_ms: 1_000,
        };
        assert!(FileTransferSession::send("host:1".into(), "  ".into(), empty_path).is_err());
    }

    #[test]
    fn missing_identity_is_rejected_before_any_io() {
        let options = SendOptions {
            payload_id: None,
            chunk_size: core::CHUNK_SIZE,
            verify_whole_file: true,
            response_timeout_ms: 1_000,
            progress_interval_ms: 100,
            identity_pkcs8: Vec::new(),
            local_peer_id: "test-peer".to_owned(),
            trusted_peer_fingerprints: Vec::new(),
            expected_peer_fingerprint: None,
            handshake_timeout_ms: 1_000,
            verification_timeout_ms: 1_000,
        };
        assert!(matches!(
            FileTransferSession::send("host:1".into(), "f".into(), options),
            Err(FfiError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn progress_interval_is_clamped() {
        assert_eq!(throttled_interval(0), MIN_PROGRESS_INTERVAL_MS);
        assert_eq!(throttled_interval(1), MIN_PROGRESS_INTERVAL_MS);
        assert_eq!(throttled_interval(100), 100);
    }

    use std::sync::atomic::AtomicU32;

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

    fn temp_path(tag: &str) -> std::path::PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "airbridge-ffi-{}-{tag}-{unique}.bin",
            std::process::id()
        ))
    }

    fn send_options(
        identity: &TestIdentity,
        peer_id: &str,
        trusted: &[&str],
        expected: Option<&str>,
    ) -> SendOptions {
        SendOptions {
            payload_id: Some(77),
            chunk_size: 64 * 1024,
            verify_whole_file: true,
            response_timeout_ms: 10_000,
            progress_interval_ms: 50,
            identity_pkcs8: identity.pkcs8.clone(),
            local_peer_id: peer_id.to_owned(),
            trusted_peer_fingerprints: trusted.iter().map(|value| (*value).to_owned()).collect(),
            expected_peer_fingerprint: expected.map(str::to_owned),
            handshake_timeout_ms: 10_000,
            verification_timeout_ms: 10_000,
        }
    }

    fn receive_options(
        identity: &TestIdentity,
        peer_id: &str,
        trusted: &[&str],
        expected: Option<&str>,
    ) -> ReceiveOptions {
        ReceiveOptions {
            expected_payload_id: Some(77),
            ack_every_bytes: 64 * 1024,
            verify_whole_file: true,
            accept_timeout_ms: 10_000,
            progress_interval_ms: 50,
            identity_pkcs8: identity.pkcs8.clone(),
            local_peer_id: peer_id.to_owned(),
            trusted_peer_fingerprints: trusted.iter().map(|value| (*value).to_owned()).collect(),
            expected_peer_fingerprint: expected.map(str::to_owned),
            handshake_timeout_ms: 10_000,
            verification_timeout_ms: 10_000,
        }
    }

    async fn wait_for_verification(session: &FileTransferSession) -> (String, String) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match session.next_event().await {
                    Ok(Some(TransferEvent::VerificationRequired {
                        sas,
                        peer_fingerprint,
                        ..
                    })) => return (sas, peer_fingerprint),
                    Ok(Some(TransferEvent::Failed { kind, message })) => {
                        panic!("session failed before verification: {kind}: {message}")
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => panic!("event stream closed before verification"),
                    Err(error) => panic!("event stream failed: {error}"),
                }
            }
        })
        .await
        .expect("verification event timed out")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_verified_transfer_round_trips_a_file() {
        let sender_identity = test_identity();
        let receiver_identity = test_identity();
        let port = free_port();
        let source = temp_path("source");
        let destination = temp_path("destination");
        tokio::fs::write(&source, b"authenticated payload")
            .await
            .unwrap();

        let receiver = FileTransferSession::receive(
            format!("127.0.0.1:{port}"),
            destination.to_string_lossy().into_owned(),
            receive_options(&receiver_identity, "receiver", &[], None),
        )
        .unwrap();

        let receiver_task = tokio::spawn(receiver.clone().run());
        tokio::time::sleep(Duration::from_millis(250)).await;

        let sender = FileTransferSession::send(
            format!("127.0.0.1:{port}"),
            source.to_string_lossy().into_owned(),
            send_options(&sender_identity, "sender", &[], None),
        )
        .unwrap();
        let sender_task = tokio::spawn(sender.clone().run());

        let (sender_sas, sender_seen) = wait_for_verification(&sender).await;
        let (receiver_sas, receiver_seen) = wait_for_verification(&receiver).await;
        assert_eq!(sender_sas, receiver_sas);
        assert_eq!(sender_seen, receiver_identity.fingerprint);
        assert_eq!(receiver_seen, sender_identity.fingerprint);

        sender.confirm_peer().unwrap();
        receiver.confirm_peer().unwrap();

        let sender_report = sender_task.await.unwrap().unwrap();
        let receiver_report = receiver_task.await.unwrap().unwrap();
        assert!(!sender_report.pairing_only);
        assert!(!receiver_report.pairing_only);
        assert_eq!(receiver_report.peer_id.as_deref(), Some("sender"));
        assert_eq!(
            receiver_report.peer_fingerprint.as_deref(),
            Some(sender_identity.fingerprint.as_str())
        );
        assert_eq!(
            tokio::fs::read(&destination).await.unwrap(),
            b"authenticated payload"
        );

        let _ = tokio::fs::remove_file(&source).await;
        let _ = tokio::fs::remove_file(&destination).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_previously_verified_fingerprint_skips_the_prompt() {
        let sender_identity = test_identity();
        let receiver_identity = test_identity();
        let port = free_port();
        let source = temp_path("trusted-source");
        let destination = temp_path("trusted-destination");
        tokio::fs::write(&source, b"trusted payload").await.unwrap();

        let receiver = FileTransferSession::receive(
            format!("127.0.0.1:{port}"),
            destination.to_string_lossy().into_owned(),
            receive_options(
                &receiver_identity,
                "receiver",
                &[sender_identity.fingerprint.as_str()],
                Some(sender_identity.fingerprint.as_str()),
            ),
        )
        .unwrap();
        let receiver_task = tokio::spawn(receiver.clone().run());
        tokio::time::sleep(Duration::from_millis(250)).await;

        let sender = FileTransferSession::send(
            format!("127.0.0.1:{port}"),
            source.to_string_lossy().into_owned(),
            send_options(
                &sender_identity,
                "sender",
                &[receiver_identity.fingerprint.as_str()],
                Some(receiver_identity.fingerprint.as_str()),
            ),
        )
        .unwrap();
        let sender_task = tokio::spawn(sender.clone().run());

        let sender_report = sender_task.await.unwrap().unwrap();
        let receiver_report = receiver_task.await.unwrap().unwrap();
        assert_eq!(
            sender_report.peer_fingerprint.as_deref(),
            Some(receiver_identity.fingerprint.as_str())
        );
        assert_eq!(
            receiver_report.peer_fingerprint.as_deref(),
            Some(sender_identity.fingerprint.as_str())
        );
        assert_eq!(
            tokio::fs::read(&destination).await.unwrap(),
            b"trusted payload"
        );

        let _ = tokio::fs::remove_file(&source).await;
        let _ = tokio::fs::remove_file(&destination).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pinned_identity_that_does_not_match_is_rejected() {
        let sender_identity = test_identity();
        let receiver_identity = test_identity();
        let pinned = test_identity();
        let port = free_port();
        let source = temp_path("pinned-source");
        let destination = temp_path("pinned-destination");
        tokio::fs::write(&source, b"payload").await.unwrap();

        let receiver = FileTransferSession::receive(
            format!("127.0.0.1:{port}"),
            destination.to_string_lossy().into_owned(),
            receive_options(
                &receiver_identity,
                "receiver",
                &[sender_identity.fingerprint.as_str()],
                Some(sender_identity.fingerprint.as_str()),
            ),
        )
        .unwrap();
        let receiver_task = tokio::spawn(receiver.clone().run());
        tokio::time::sleep(Duration::from_millis(250)).await;

        let sender = FileTransferSession::send(
            format!("127.0.0.1:{port}"),
            source.to_string_lossy().into_owned(),
            send_options(
                &sender_identity,
                "sender",
                &[],
                Some(pinned.fingerprint.as_str()),
            ),
        )
        .unwrap();

        let error = sender.run().await.unwrap_err();
        assert!(matches!(error, FfiError::IdentityMismatch { .. }));
        let _ = receiver_task.await;
        let _ = tokio::fs::remove_file(&source).await;
        let _ = tokio::fs::remove_file(&destination).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejecting_the_code_aborts_the_session() {
        let sender_identity = test_identity();
        let receiver_identity = test_identity();
        let port = free_port();
        let source = temp_path("reject-source");
        let destination = temp_path("reject-destination");
        tokio::fs::write(&source, b"payload").await.unwrap();

        let receiver = FileTransferSession::receive(
            format!("127.0.0.1:{port}"),
            destination.to_string_lossy().into_owned(),
            receive_options(&receiver_identity, "receiver", &[], None),
        )
        .unwrap();
        let receiver_task = tokio::spawn(receiver.clone().run());
        tokio::time::sleep(Duration::from_millis(250)).await;

        let sender = FileTransferSession::send(
            format!("127.0.0.1:{port}"),
            source.to_string_lossy().into_owned(),
            send_options(&sender_identity, "sender", &[], None),
        )
        .unwrap();
        let sender_task = tokio::spawn(sender.clone().run());

        let _ = wait_for_verification(&sender).await;
        let _ = wait_for_verification(&receiver).await;

        sender.reject_peer().unwrap();
        receiver.reject_peer().unwrap();

        assert!(matches!(
            sender_task.await.unwrap(),
            Err(FfiError::VerificationRejected)
        ));
        assert!(matches!(
            receiver_task.await.unwrap(),
            Err(FfiError::VerificationRejected)
        ));

        let _ = tokio::fs::remove_file(&source).await;
        let _ = tokio::fs::remove_file(&destination).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pairing_only_connections_bind_the_peer_id_to_the_fingerprint() {
        let sender_peer = PeerIdentity::generate().unwrap();
        let sender_fingerprint = sender_peer.fingerprint();
        let receiver_identity = test_identity();
        let port = free_port();
        let destination = temp_path("pairing-destination");

        let receiver = FileTransferSession::receive(
            format!("127.0.0.1:{port}"),
            destination.to_string_lossy().into_owned(),
            receive_options(&receiver_identity, "receiver", &[], None),
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
        let (receiver_sas, receiver_seen) = wait_for_verification(&receiver).await;
        assert_eq!(challenge.sas, receiver_sas);
        assert_eq!(receiver_seen, sender_fingerprint);

        receiver.confirm_peer().unwrap();
        let result = pairing.confirm().await.unwrap();
        assert_eq!(result.peer_id, "receiver");
        assert_eq!(result.peer_fingerprint, receiver_identity.fingerprint);
        assert_eq!(result.sas, challenge.sas);

        let report = receiver_task.await.unwrap().unwrap();
        assert!(report.pairing_only);
        assert_eq!(report.payload_id, 0);
        assert_eq!(report.peer_id.as_deref(), Some("sender"));
        assert_eq!(
            report.peer_fingerprint.as_deref(),
            Some(sender_fingerprint.as_str())
        );

        let _ = tokio::fs::remove_file(&destination).await;
    }
}
