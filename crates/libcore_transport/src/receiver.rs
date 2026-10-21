//! The resumable receive engine.
//!
//! ```text
//! destination.bin.part         payload bytes as they arrive, extended in order
//! destination.bin.part.state   { payload id, size, verified offset, prefix sha256 }
//! ```
//!
//! Chunks are written to a part file only after their SHA-256 verifies, and
//! the verified prefix is persisted before the acknowledgement that reports
//! it. Nothing is renamed into place until the final chunk has verified and —
//! when the sender announced one — the whole-file digest matches.
//!
//! # Failure semantics
//!
//! A corrupt chunk is not an error the transfer needs to die of. The receiver
//! leaves its verified prefix alone and answers with a *repair* ack; the
//! sender rewinds and re-sends from there. A chunk that arrives behind the
//! verified prefix is a duplicate caused by that rewind and is skipped
//! silently. The transfer only fails when the peer abandons the connection,
//! violates the framing rules, or the final digest disagrees — and in the
//! first two of those cases the part file and sidecar survive for the next
//! attempt.
//!
//! # Trust
//!
//! The part file and the sidecar are *hints*. The sidecar's prefix digest is
//! recomputed from the part file before the offset is honoured, and the
//! whole-file digest is recomputed incrementally as chunks verify. A hostile
//! edit to either file can force a restart, but cannot produce a wrong result.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use ring::digest;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::chunk::{chunk_sha256, context_digest, ChunkPlan};
use crate::error::TransferError;
use crate::frame::{
    read_frame, write_abort, write_ack_offset, write_complete, Abort, AbortCode, AckOffset,
    Complete, Frame, FrameBuffer, Hello, PayloadId,
};
use crate::state::{part_path, state_path_for, ResumeState};

/// Tunables for a download.
#[derive(Debug, Clone, Copy)]
pub struct ReceiveOptions {
    /// Reject a `HELLO` for any other payload id.
    ///
    /// Set this when the caller knows which transfer it asked for; leave it
    /// `None` to accept whatever arrives first.
    pub expected_payload_id: Option<PayloadId>,
    /// How many verified bytes may accumulate before a progress ack is sent.
    ///
    /// This bounds how far a sender can run ahead of the receiver's persisted
    /// state, and therefore how much work a dropped connection can waste.
    pub ack_every_bytes: u64,
    /// Verify the prefix digest on resume and the whole-file digest on
    /// completion.
    ///
    /// Disable only when an outer layer already authenticates the stream (for
    /// example a [`libcore_crypto`](https://docs.rs/libcore_crypto) session):
    /// in that case the checks are redundant, and disabling them also removes
    /// the sender's extra hash pass.
    pub verify_file_sha256: bool,
}

impl ReceiveOptions {
    /// Default options.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            expected_payload_id: None,
            ack_every_bytes: 4 * 1024 * 1024,
            verify_file_sha256: true,
        }
    }

    /// Requires a specific payload id.
    #[must_use]
    pub const fn with_expected_payload_id(mut self, payload_id: PayloadId) -> Self {
        self.expected_payload_id = Some(payload_id);
        self
    }

    /// Overrides the acknowledgement cadence.
    #[must_use]
    pub const fn with_ack_every_bytes(mut self, bytes: u64) -> Self {
        self.ack_every_bytes = bytes;
        self
    }

    /// Enables or disables digest checks.
    #[must_use]
    pub const fn with_file_sha256(mut self, verify: bool) -> Self {
        self.verify_file_sha256 = verify;
        self
    }
}

impl Default for ReceiveOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of a completed download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveReport {
    /// Identifier of the payload that was moved.
    pub payload_id: PayloadId,
    /// Total size of the payload, in bytes.
    pub total_len: u64,
    /// Where the payload was committed.
    pub path: PathBuf,
    /// Byte offset the transfer started from.
    pub resumed_from: u64,
}

/// Receives a payload into `destination` until it is verified and committed.
///
/// `destination` is only ever created by the final rename; while bytes are in
/// flight they live in `destination.part`, and progress lives in
/// `destination.part.state`. Both survive failures, which is what makes the
/// next attempt with the same payload id a resume.
///
/// # Errors
///
/// Returns [`TransferError`] on framing violations, peer aborts, digest
/// mismatches and I/O failures. The part file and sidecar are left in place on
/// every error path; they are removed by a successful commit, and the sidecar
/// is also removed when its state is superseded.
pub async fn receive_file<S>(
    stream: S,
    destination: &Path,
    options: &ReceiveOptions,
) -> Result<ReceiveReport, TransferError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut buffer = FrameBuffer::new();

    // 1. HELLO.
    let hello = match read_frame(&mut reader, &mut buffer).await? {
        Frame::Hello(hello) => hello,
        Frame::Abort(abort) => return Err(TransferError::PeerAborted { code: abort.code }),
        _ => return Err(TransferError::Protocol("the sender must open with HELLO")),
    };
    let plan = ChunkPlan::new(hello.chunk_size)?;
    if let Some(expected) = options.expected_payload_id {
        if expected != hello.payload_id {
            let _ = write_abort(
                &mut writer,
                &Abort {
                    payload_id: hello.payload_id,
                    code: AbortCode::Unsupported,
                },
            )
            .await;
            return Err(TransferError::PayloadMismatch {
                expected: expected.get(),
                received: hello.payload_id.get(),
            });
        }
    }

    // 2. Reconcile with whatever survived the previous attempt.
    let part = part_path(destination);
    let state_path = state_path_for(&part);
    let (mut file, mut offset, mut hasher) =
        open_resume(&part, &state_path, &hello, &plan, options).await?;
    let resumed_from = offset;

    // 3. The opening ack: the sender starts exactly here.
    write_ack_offset(
        &mut writer,
        &AckOffset {
            payload_id: hello.payload_id,
            offset,
            repair: false,
        },
    )
    .await?;

    if offset == hello.total_len {
        // Zero-length payloads, and sessions that died after the last chunk
        // but before the rename, both land here: verify, commit, done.
        commit(
            &mut file,
            destination,
            &part,
            &state_path,
            &hello,
            &hasher,
            options.verify_file_sha256,
        )
        .await?;
        write_complete(
            &mut writer,
            &Complete {
                payload_id: hello.payload_id,
                total_len: hello.total_len,
            },
        )
        .await?;
        return Ok(ReceiveReport {
            payload_id: hello.payload_id,
            total_len: hello.total_len,
            path: destination.to_path_buf(),
            resumed_from,
        });
    }

    let ack_every = options.ack_every_bytes.max(1);
    let mut unacked = 0_u64;

    loop {
        match read_frame(&mut reader, &mut buffer).await? {
            Frame::Payload(payload) => {
                let header = payload.header;
                let data = payload.data;

                if header.payload_id != hello.payload_id {
                    return abort_with(
                        &mut writer,
                        hello.payload_id,
                        AbortCode::ProtocolViolation,
                        "chunk belongs to a different payload",
                    )
                    .await;
                }
                if !plan.is_aligned(header.offset) {
                    return abort_with(
                        &mut writer,
                        hello.payload_id,
                        AbortCode::ProtocolViolation,
                        "chunk offset is not aligned to the chunk grid",
                    )
                    .await;
                }
                if data.len() > plan.chunk_size() as usize {
                    return abort_with(
                        &mut writer,
                        hello.payload_id,
                        AbortCode::ProtocolViolation,
                        "chunk exceeds the negotiated chunk size",
                    )
                    .await;
                }
                let end =
                    header
                        .offset
                        .checked_add(data.len() as u64)
                        .ok_or(TransferError::Protocol(
                            "chunk offset overflows the payload space",
                        ))?;
                if end > hello.total_len {
                    return abort_with(
                        &mut writer,
                        hello.payload_id,
                        AbortCode::ProtocolViolation,
                        "chunk extends past the end of the payload",
                    )
                    .await;
                }

                // Everything up to `offset` is verified. A chunk entirely
                // behind it is a duplicate created by a legitimate rewind;
                // skipping it is cheaper than verifying it twice.
                if end <= offset {
                    continue;
                }
                if header.offset > offset {
                    // The sender is ahead of what we can accept; ask for the
                    // bytes we actually need.
                    write_ack_offset(
                        &mut writer,
                        &AckOffset {
                            payload_id: hello.payload_id,
                            offset,
                            repair: true,
                        },
                    )
                    .await?;
                    continue;
                }

                // `header.offset == offset`: contiguous with the verified
                // prefix. This is the only path that advances state.
                if chunk_sha256(data) != header.checksum {
                    write_ack_offset(
                        &mut writer,
                        &AckOffset {
                            payload_id: hello.payload_id,
                            offset,
                            repair: true,
                        },
                    )
                    .await?;
                    continue;
                }

                file.write_all(data).await?;
                hasher.update(data);
                offset = end;
                unacked += data.len() as u64;

                if unacked >= ack_every {
                    // Persist before acknowledging: a receiver that crashes
                    // after this point re-verifies the prefix and lands on the
                    // same offset.
                    persist_state(&state_path, &hello, offset, &hasher).await?;
                    write_ack_offset(
                        &mut writer,
                        &AckOffset {
                            payload_id: hello.payload_id,
                            offset,
                            repair: false,
                        },
                    )
                    .await?;
                    unacked = 0;
                }

                if offset == hello.total_len {
                    persist_state(&state_path, &hello, offset, &hasher).await?;
                    commit(
                        &mut file,
                        destination,
                        &part,
                        &state_path,
                        &hello,
                        &hasher,
                        options.verify_file_sha256,
                    )
                    .await?;
                    write_complete(
                        &mut writer,
                        &Complete {
                            payload_id: hello.payload_id,
                            total_len: hello.total_len,
                        },
                    )
                    .await?;
                    return Ok(ReceiveReport {
                        payload_id: hello.payload_id,
                        total_len: hello.total_len,
                        path: destination.to_path_buf(),
                        resumed_from,
                    });
                }
            }
            Frame::Abort(abort) => return Err(TransferError::PeerAborted { code: abort.code }),
            Frame::Hello(_) => {
                return abort_with(
                    &mut writer,
                    hello.payload_id,
                    AbortCode::ProtocolViolation,
                    "sender sent a second HELLO",
                )
                .await
            }
            Frame::AckOffset(_) | Frame::Complete(_) => {
                return abort_with(
                    &mut writer,
                    hello.payload_id,
                    AbortCode::ProtocolViolation,
                    "sender sent a frame only a receiver may send",
                )
                .await
            }
        }
    }
}

/// Reconciles the part file and sidecar with the incoming `HELLO` and opens
/// the part file positioned at the first byte the sender must (re-)send.
async fn open_resume(
    part: &Path,
    state_path: &Path,
    hello: &Hello,
    plan: &ChunkPlan,
    options: &ReceiveOptions,
) -> Result<(File, u64, digest::Context), TransferError> {
    let part_len = match tokio::fs::metadata(part).await {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };

    let mut offset = 0_u64;
    let mut hasher = digest::Context::new(&digest::SHA256);

    if let Some(state) = ResumeState::load(state_path).await {
        let matches = state.payload_id == hello.payload_id
            && state.total_len == hello.total_len
            && state.chunk_size == hello.chunk_size
            && state.file_sha256 == hello.file_sha256;
        if matches {
            let candidate =
                plan.align_down(state.verified_offset.min(part_len).min(hello.total_len));
            if candidate > 0 {
                if options.verify_file_sha256 {
                    hasher = hash_prefix(part, candidate).await?;
                    if context_digest(&hasher) == state.prefix_sha256 {
                        offset = candidate;
                    } else {
                        // The part file does not hold the bytes the sidecar
                        // claims; discard both and start over.
                        hasher = digest::Context::new(&digest::SHA256);
                    }
                } else {
                    offset = candidate;
                }
            }
        }
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(part)
        .await?;
    // Anything beyond the resume point is unverified by definition; verify
    // again rather than trust it.
    file.set_len(offset).await?;
    file.seek(SeekFrom::Start(offset)).await?;
    Ok((file, offset, hasher))
}

/// Hashes the first `end` bytes of `part`.
async fn hash_prefix(part: &Path, end: u64) -> Result<digest::Context, TransferError> {
    let mut file = File::open(part).await?;
    let mut hasher = digest::Context::new(&digest::SHA256);
    let mut scratch = vec![0_u8; 256 * 1024];
    let mut remaining = end;
    while remaining > 0 {
        let want = usize::try_from(remaining.min(scratch.len() as u64)).unwrap_or(scratch.len());
        let read = file.read(&mut scratch[..want]).await?;
        if read == 0 {
            return Err(TransferError::Protocol(
                "part file is shorter than the recorded resume offset",
            ));
        }
        hasher.update(&scratch[..read]);
        remaining -= read as u64;
    }
    Ok(hasher)
}

/// Persists the verified prefix.
async fn persist_state(
    state_path: &Path,
    hello: &Hello,
    offset: u64,
    hasher: &digest::Context,
) -> Result<(), TransferError> {
    let state = ResumeState {
        payload_id: hello.payload_id,
        total_len: hello.total_len,
        chunk_size: hello.chunk_size,
        verified_offset: offset,
        file_sha256: hello.file_sha256,
        prefix_sha256: context_digest(hasher),
    };
    state.store(state_path).await
}

/// Verifies the whole-file digest, flushes, renames and cleans up.
async fn commit(
    file: &mut File,
    destination: &Path,
    part: &Path,
    state_path: &Path,
    hello: &Hello,
    hasher: &digest::Context,
    verify: bool,
) -> Result<(), TransferError> {
    if verify {
        if let Some(expected) = hello.file_sha256 {
            let actual = context_digest(hasher);
            if actual != expected {
                return Err(TransferError::FileChecksumMismatch { expected, actual });
            }
        }
    }
    file.sync_all().await?;
    tokio::fs::rename(part, destination).await?;
    match tokio::fs::remove_file(state_path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Sends an abort frame best-effort and returns the matching protocol error.
async fn abort_with<W>(
    writer: &mut W,
    payload_id: PayloadId,
    code: AbortCode,
    detail: &'static str,
) -> Result<ReceiveReport, TransferError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let _ = write_abort(writer, &Abort { payload_id, code }).await;
    Err(TransferError::Protocol(detail))
}
