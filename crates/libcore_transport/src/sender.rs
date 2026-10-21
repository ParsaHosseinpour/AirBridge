//! The resumable upload engine.
//!
//! ```text
//! sender                                receiver
//!   |-- HELLO (payload id, size, sha256) -->|
//!   |<------------- ACK_OFFSET (where to start) --|
//!   |-- PAYLOAD_TRANSFER chunk ------------>|  verify, then...
//!   |<------------- ACK_OFFSET (progress) --|  ...every ack_every_bytes
//!   |   ...                                 |
//!   |<----------------- COMPLETE -----------|  after the final chunk
//! ```
//!
//! # Why the sender listens
//!
//! A sender that streams blind can only be told "you are wrong" by closing.
//! This one keeps its read half live in a small pump task: every frame the
//! receiver sends lands in a [`watch`] channel, and before *each* chunk the
//! sending loop folds the newest state into its position.
//!
//! * A progress ack above the sender's position means the receiver verified
//!   further than the sender knew, which happens after a repair; the sender
//!   skips forward instead of re-sending.
//! * A repair ack means the receiver saw a chunk it could not verify; the
//!   sender rewinds to the acknowledged offset and streams again from there.
//!
//! That is the whole resumption mechanism, and it runs identically on a fresh
//! connection and on the tenth retry of a 40 GiB payload: the receiver's
//! opening `ACK_OFFSET` is simply where the loop starts.
//!
//! # Memory
//!
//! One chunk-sized scratch buffer (~512 KiB) plus two small frame buffers.
//! The file is read straight into the scratch buffer, hashed in place, and
//! written to the socket; nothing about the transfer scales with file size.

use std::io::SeekFrom;
use std::path::Path;
use std::time::Duration;

use ring::digest;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite};
use tokio::sync::watch;

use crate::chunk::{chunk_sha256, context_digest, ChunkPlan, CHUNK_SIZE};
use crate::error::TransferError;
use crate::frame::{
    read_frame, write_hello, write_payload, AbortCode, Frame, FrameBuffer, Hello, PayloadHeader,
    PayloadId,
};

/// Tunables for an upload.
#[derive(Debug, Clone, Copy)]
pub struct UploadOptions {
    /// Identifies the payload across connections and restarts. Re-use the same
    /// id to resume; generate a fresh one (see [`PayloadId::random`]) for a
    /// new payload.
    pub payload_id: PayloadId,
    /// Chunk grid size. Must lie in
    /// `crate::MIN_CHUNK_SIZE..=crate::MAX_CHUNK_SIZE`.
    pub chunk_size: u32,
    /// Compute and announce the whole-file SHA-256.
    ///
    /// This costs one extra linear read of the file on the sender and one
    /// one-time prefix re-read on a resuming receiver, and it buys an
    /// end-to-end integrity check before the receiver renames its part file
    /// into place.
    pub compute_file_sha256: bool,
    /// Deadline for the receiver's opening ack and for the final `COMPLETE`.
    ///
    /// The streaming phase itself is unbounded: a slow link is not an error.
    pub response_timeout: Duration,
}

impl UploadOptions {
    /// Default options for `payload_id`.
    #[must_use]
    pub const fn new(payload_id: PayloadId) -> Self {
        Self {
            payload_id,
            chunk_size: CHUNK_SIZE,
            compute_file_sha256: true,
            response_timeout: Duration::from_secs(30),
        }
    }

    /// Overrides the chunk size.
    #[must_use]
    pub const fn with_chunk_size(mut self, chunk_size: u32) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Enables or disables the whole-file digest.
    #[must_use]
    pub const fn with_file_sha256(mut self, compute: bool) -> Self {
        self.compute_file_sha256 = compute;
        self
    }

    /// Overrides the response deadline.
    #[must_use]
    pub const fn with_response_timeout(mut self, timeout: Duration) -> Self {
        self.response_timeout = timeout;
        self
    }
}

/// Outcome of a completed upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadReport {
    /// Identifier of the payload that was moved.
    pub payload_id: PayloadId,
    /// Total size of the payload, in bytes.
    pub total_len: u64,
    /// Byte offset the transfer started from: zero on a fresh payload, the
    /// receiver's verified prefix on a resumed one.
    pub resumed_from: u64,
    /// Payload bytes written to the socket, retransmissions included.
    pub bytes_sent: u64,
    /// Chunks written to the socket, retransmissions included.
    pub chunks_sent: u64,
}

/// What the ack pump knows about the receiver.
#[derive(Debug, Clone, Copy, Default)]
struct AckState {
    offset: Option<u64>,
    repair: bool,
    complete: bool,
    aborted: Option<AbortCode>,
    protocol_error: bool,
    closed: bool,
}

/// Uploads `path` over `stream` until the receiver commits it.
///
/// The whole resumable-transfer contract is visible in the signature: the
/// caller opens a stream (for TCP, prefer [`crate::connect`]), calls this, and
/// on failure keeps the same [`PayloadId`] for the next attempt.
///
/// # Errors
///
/// Returns [`TransferError`] on any I/O failure, framing violation, peer
/// abort, or timeout. After a failure the receiver's part file and sidecar
/// remain in place, so retrying with the same payload id resumes rather than
/// restarts.
pub async fn upload_file<S>(
    stream: S,
    path: &Path,
    options: &UploadOptions,
) -> Result<UploadReport, TransferError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let plan = ChunkPlan::new(options.chunk_size)?;
    let total_len = tokio::fs::metadata(path).await?.len();

    let file_sha256 = if options.compute_file_sha256 {
        let mut scratch = vec![0_u8; plan.chunk_size() as usize];
        let digest = hash_file(path, &mut scratch).await?;
        // Free the hashing buffer before the streaming buffer is allocated:
        // the upload's peak memory stays at one chunk either way.
        drop(scratch);
        Some(digest)
    } else {
        None
    };

    let (mut reader, mut writer) = tokio::io::split(stream);
    let hello = Hello {
        payload_id: options.payload_id,
        total_len,
        chunk_size: plan.chunk_size(),
        file_sha256,
    };
    write_hello(&mut writer, &hello).await?;

    // The receiver's first frame is always an ACK_OFFSET: zero for a fresh
    // payload, the persisted verified prefix for a resumed one.
    let start_offset = {
        let mut buffer = FrameBuffer::new();
        let frame = tokio::time::timeout(
            options.response_timeout,
            read_frame(&mut reader, &mut buffer),
        )
        .await
        .map_err(|_| TransferError::Timeout("waiting for the receiver's opening ACK_OFFSET"))??;
        match frame {
            Frame::AckOffset(ack) => {
                if ack.payload_id != options.payload_id {
                    return Err(TransferError::PayloadMismatch {
                        expected: options.payload_id.get(),
                        received: ack.payload_id.get(),
                    });
                }
                ack.offset.min(total_len)
            }
            Frame::Abort(abort) => return Err(TransferError::PeerAborted { code: abort.code }),
            _ => {
                return Err(TransferError::Protocol(
                    "the receiver must answer HELLO with ACK_OFFSET",
                ))
            }
        }
    };

    let (ack_tx, mut ack_rx) = watch::channel(AckState::default());
    let pump = tokio::spawn(pump_receiver_frames(reader, ack_tx));

    let resumed_from = plan.align_down(start_offset);
    let streamed = stream_chunks(
        &mut writer,
        path,
        &plan,
        options,
        total_len,
        resumed_from,
        &mut ack_rx,
    )
    .await;

    let result = match streamed {
        Err(error) => Err(error),
        Ok((bytes_sent, chunks_sent)) => {
            match tokio::time::timeout(options.response_timeout, wait_for_complete(&mut ack_rx))
                .await
            {
                Ok(Ok(())) => Ok(UploadReport {
                    payload_id: options.payload_id,
                    total_len,
                    resumed_from,
                    bytes_sent,
                    chunks_sent,
                }),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(TransferError::Timeout(
                    "waiting for the receiver's COMPLETE",
                )),
            }
        }
    };

    // Every exit path passes through here, so the pump cannot outlive the
    // call and keep a half-read stream alive.
    pump.abort();
    let _ = pump.await;
    result
}

/// Streams chunks until `total_len`, folding receiver feedback in before each
/// chunk. Returns `(bytes_sent, chunks_sent)`, retransmissions included.
async fn stream_chunks<W>(
    writer: &mut W,
    path: &Path,
    plan: &ChunkPlan,
    options: &UploadOptions,
    total_len: u64,
    start: u64,
    ack_rx: &mut watch::Receiver<AckState>,
) -> Result<(u64, u64), TransferError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut file = File::open(path).await?;
    let mut scratch = vec![0_u8; plan.chunk_size() as usize];
    let mut next = plan.align_down(start.min(total_len));
    if next > 0 {
        file.seek(SeekFrom::Start(next)).await?;
    }

    let mut bytes_sent = 0_u64;
    let mut chunks_sent = 0_u64;
    let mut last_seen: Option<(u64, bool)> = None;

    while next < total_len {
        let snapshot = *ack_rx.borrow();
        if let Some(code) = snapshot.aborted {
            return Err(TransferError::PeerAborted { code });
        }
        if snapshot.protocol_error {
            return Err(TransferError::Protocol(
                "the receiver sent a frame only a sender may send",
            ));
        }
        if snapshot.closed {
            return Err(TransferError::PeerClosed);
        }

        // Fold receiver feedback in. A repair ack always moves the position
        // (that is the point of the bit); a progress ack may only skip the
        // sender forward, never drag a lagging report into a rewind.
        if let Some(offset) = snapshot.offset {
            if last_seen != Some((offset, snapshot.repair)) {
                last_seen = Some((offset, snapshot.repair));
                let target = plan.align_down(offset.min(total_len));
                let rewind = snapshot.repair && target != next;
                let skip = !snapshot.repair && target > next;
                if rewind || skip {
                    next = target;
                    file.seek(SeekFrom::Start(next)).await?;
                }
            }
        }

        let range = plan
            .range_at(next, total_len)
            .ok_or(TransferError::Protocol("chunk plan lost the payload tail"))?;
        let len = range.len as usize;
        if file.read_exact(&mut scratch[..len]).await.is_err() {
            // The file changed under us; the receiver's partial state stays
            // valid for a retry with the same file.
            return Err(TransferError::Protocol(
                "source file shrank while it was being sent",
            ));
        }
        let header = PayloadHeader {
            payload_id: options.payload_id,
            offset: next,
            checksum: chunk_sha256(&scratch[..len]),
        };
        write_payload(writer, &header, &scratch[..len]).await?;
        next += u64::from(range.len);
        bytes_sent += u64::from(range.len);
        chunks_sent += 1;
    }

    Ok((bytes_sent, chunks_sent))
}

/// Reads and classifies receiver frames for the lifetime of the transfer.
async fn pump_receiver_frames<R>(mut reader: R, ack_tx: watch::Sender<AckState>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut buffer = FrameBuffer::new();
    loop {
        match read_frame(&mut reader, &mut buffer).await {
            Ok(Frame::AckOffset(ack)) => {
                ack_tx.send_modify(|state| {
                    state.offset = Some(ack.offset);
                    state.repair = ack.repair;
                });
            }
            Ok(Frame::Complete(_)) => {
                ack_tx.send_modify(|state| state.complete = true);
                return;
            }
            Ok(Frame::Abort(abort)) => {
                ack_tx.send_modify(|state| state.aborted = Some(abort.code));
                return;
            }
            Ok(_) => {
                ack_tx.send_modify(|state| state.protocol_error = true);
                return;
            }
            Err(_) => {
                ack_tx.send_modify(|state| state.closed = true);
                return;
            }
        }
    }
}

/// Waits until the receiver reports `COMPLETE`, or reports why it will never
/// do so.
async fn wait_for_complete(ack_rx: &mut watch::Receiver<AckState>) -> Result<(), TransferError> {
    loop {
        {
            let snapshot = *ack_rx.borrow();
            if snapshot.complete {
                return Ok(());
            }
            if let Some(code) = snapshot.aborted {
                return Err(TransferError::PeerAborted { code });
            }
            if snapshot.protocol_error {
                return Err(TransferError::Protocol(
                    "the receiver sent a frame only a sender may send",
                ));
            }
            if snapshot.closed {
                return Err(TransferError::PeerClosed);
            }
        }
        if ack_rx.changed().await.is_err() {
            return Err(TransferError::PeerClosed);
        }
    }
}

/// Streams the file through SHA-256 once.
async fn hash_file(path: &Path, scratch: &mut [u8]) -> Result<[u8; 32], TransferError> {
    let mut file = File::open(path).await?;
    let mut hasher = digest::Context::new(&digest::SHA256);
    loop {
        let read = file.read(scratch).await?;
        if read == 0 {
            break;
        }
        hasher.update(&scratch[..read]);
    }
    Ok(context_digest(&hasher))
}
