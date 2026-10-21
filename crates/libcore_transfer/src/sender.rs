//! Sequential batch upload over one stream.
//!
//! The sender writes the manifest once, then runs the ordinary per-file
//! `HELLO` / `PAYLOAD_TRANSFER` / `COMPLETE` exchange for each entry without
//! reopening the connection. One background task reads receiver frames for the
//! whole batch and routes acks to the active file's watch channel.
//!
//! The per-file engine remains the source of truth for resume and integrity.
//! This module adds only ordering, progress accounting, and the policy that
//! decides whether a bad item ends the batch or the next file starts.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::Path;
use std::time::Duration;

use libcore_transport::{
    chunk_sha256, read_frame, write_abort, write_hello, write_payload, Abort, AbortCode, ChunkPlan,
    Frame, FrameBuffer, Hello, PayloadHeader, PayloadId,
};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite};
use tokio::sync::watch;

use crate::cancel::CancelStream;
use crate::cancel::CancelToken;
use crate::error::BatchError;
use crate::event::BatchEvent;
use crate::manifest::{ManifestError, PreparedBatch, TransferManifest};
use crate::options::BatchOptions;
use crate::progress::{emit_progress, ProgressThrottle};
use crate::protocol::write_manifest;
use crate::queue::{BatchReport, TransferQueue};

#[derive(Clone, Copy, Debug, Default)]
struct AckState {
    offset: Option<u64>,
    repair: bool,
    complete: bool,
    aborted: Option<AbortCode>,
    protocol_error: bool,
    closed: bool,
}

/// Uploads every file in `batch` over one stream.
///
/// The caller must supply an already authenticated stream. The manifest is
/// written first; each file then uses its stable payload id, so a retry of the
/// same batch resumes individual part files on the receiver.
///
/// Recoverable per-item failures are reported in the returned [`BatchReport`]
/// and in [`BatchEvent::FileFailed`]. A fatal connection/protocol failure is
/// returned as `Err`; in that case the queue still holds the state reached so
/// far.
///
/// # Errors
///
/// Returns [`BatchError`] for an empty/invalid batch, a manifest or wire
/// failure, cancellation, or an unrecoverable transport failure.
pub async fn upload_batch<S, F>(
    stream: S,
    batch: &PreparedBatch,
    queue: &TransferQueue,
    options: &BatchOptions,
    token: &CancelToken,
    mut emit: F,
) -> Result<BatchReport, BatchError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: FnMut(BatchEvent),
{
    options.validate()?;
    if batch.is_empty() {
        return Err(BatchError::EmptyBatch);
    }
    let manifest = batch.manifest()?;
    queue.install_manifest(&manifest)?;
    queue.begin()?;

    emit(BatchEvent::Started {
        batch_id: manifest.batch_id(),
        total_files: u32::try_from(manifest.len()).unwrap_or(u32::MAX),
        total_bytes: manifest.total_bytes(),
    });

    let mut stream = CancelStream::new(stream, token.clone());
    if let Err(error) = write_manifest(&mut stream, &manifest).await {
        if token.is_cancelled() {
            queue.cancel();
            return Err(BatchError::Cancelled);
        }
        return Err(error);
    }

    let (reader, mut writer) = tokio::io::split(stream);
    let mut senders = HashMap::with_capacity(batch.len());
    let mut receivers = Vec::with_capacity(batch.len());
    for index in 0..batch.len() {
        let payload_id = manifest
            .payload_id(index)
            .ok_or_else(|| BatchError::Protocol("manifest index lost its payload id".to_owned()))?;
        let (sender, receiver) = watch::channel(AckState::default());
        senders.insert(payload_id.get(), sender);
        receivers.push(receiver);
    }
    let pump = tokio::spawn(pump_ack_frames(reader, senders));

    let mut throttle = ProgressThrottle::new(options.progress_interval);
    let mut fatal: Option<BatchError> = None;

    for (index, file) in batch.files().iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        if token.is_cancelled() {
            queue.cancel();
            pump.abort();
            let _ = pump.await;
            return Err(BatchError::Cancelled);
        }

        let entry = file.entry();
        queue.activate(index)?;
        emit(BatchEvent::FileStarted {
            index,
            relative_path: entry.relative_path().to_owned(),
            size: entry.size(),
        });

        let payload_id = manifest
            .payload_id(index as usize)
            .ok_or_else(|| BatchError::Protocol("manifest index lost its payload id".to_owned()))?;
        let opened = open_source(file.source(), entry.size()).await;
        let outcome = match opened {
            Ok(source) => {
                upload_one(
                    &mut writer,
                    source,
                    entry.size(),
                    entry.sha256(),
                    payload_id,
                    &mut receivers[index as usize],
                    options,
                    token,
                    queue,
                    index,
                    &mut throttle,
                    &mut emit,
                )
                .await
            }
            Err(error) => Err(error),
        };

        match outcome {
            Ok(resumed_from) => {
                queue.complete(index, resumed_from, None)?;
                emit(BatchEvent::FileCompleted {
                    index,
                    relative_path: entry.relative_path().to_owned(),
                    size: entry.size(),
                    resumed_from,
                    destination_path: None,
                });
                emit_progress(
                    queue,
                    index,
                    entry.size(),
                    entry.size(),
                    &mut throttle,
                    true,
                    &mut emit,
                );
            }
            Err(error) => {
                let message = error.to_string();
                queue.fail(index, message.clone())?;
                emit(BatchEvent::FileFailed {
                    index,
                    relative_path: entry.relative_path().to_owned(),
                    kind: error.kind().to_owned(),
                    message,
                });

                let recoverable = error.is_recoverable() && !token.is_cancelled();
                if recoverable && options.continue_on_error {
                    // A local failure before HELLO must still tell the receiver
                    // to skip this entry, otherwise it waits for a payload that
                    // will never arrive.
                    if !matches!(
                        error,
                        BatchError::Transport(libcore_transport::TransferError::PeerAborted { .. })
                    ) && write_abort(
                        &mut writer,
                        &Abort {
                            payload_id,
                            code: AbortCode::StorageFailure,
                        },
                    )
                    .await
                    .is_err()
                    {
                        pump.abort();
                        let _ = pump.await;
                        return Err(error);
                    }
                    continue;
                }

                // Tell the receiver about every file this attempt will never
                // start. It can then finish its own queue deterministically
                // instead of blocking on a half-open connection.
                if write_abort(
                    &mut writer,
                    &Abort {
                        payload_id,
                        code: AbortCode::StorageFailure,
                    },
                )
                .await
                .is_ok()
                {
                    let _ = abort_remaining(&mut writer, &manifest, index as usize + 1).await;
                }
                // The remaining entries will never start in this attempt.
                queue.cancel_pending_from(index.saturating_add(1));
                if !recoverable {
                    fatal = Some(error);
                }
                break;
            }
        }
    }

    pump.abort();
    let _ = pump.await;

    if token.is_cancelled() {
        queue.cancel();
        return Err(BatchError::Cancelled);
    }
    if let Some(error) = fatal {
        queue.finish();
        return Err(error);
    }

    queue.finish();
    let report = queue.report();
    emit(BatchEvent::Finished {
        report: report.clone(),
    });
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn upload_one<W, F>(
    writer: &mut W,
    mut source: File,
    total_len: u64,
    sha256: [u8; 32],
    payload_id: PayloadId,
    ack_rx: &mut watch::Receiver<AckState>,
    options: &BatchOptions,
    token: &CancelToken,
    queue: &TransferQueue,
    index: u32,
    throttle: &mut ProgressThrottle,
    emit: &mut F,
) -> Result<u64, BatchError>
where
    W: AsyncWrite + Unpin,
    F: FnMut(BatchEvent),
{
    let plan = ChunkPlan::new(options.chunk_size)?;

    // HELLO. The digest comes from the authenticated manifest, not from a
    // second local hash: the receiver verifies the bytes against it, so a file
    // changed after preparation fails end-to-end rather than silently sending
    // different content under the agreed name.
    let hello = Hello {
        payload_id,
        total_len,
        chunk_size: plan.chunk_size(),
        file_sha256: options.verify_whole_file.then_some(sha256),
    };
    write_hello(writer, &hello).await?;

    let ack = wait_for_offset(ack_rx, options.response_timeout).await?;
    let start = plan.align_down(ack.min(total_len));
    if start > 0 {
        source.seek(SeekFrom::Start(start)).await?;
    }
    queue.record_progress(index, start)?;
    emit_progress(queue, index, start, total_len, throttle, true, emit);

    let (_bytes_sent, _chunks_sent) = stream_chunks(
        writer,
        &mut source,
        &plan,
        payload_id,
        total_len,
        start,
        ack_rx,
        token,
        queue,
        index,
        throttle,
        emit,
    )
    .await?;

    wait_for_complete(ack_rx, options.response_timeout).await?;
    Ok(start)
}

#[allow(clippy::too_many_arguments)]
async fn stream_chunks<W, F>(
    writer: &mut W,
    source: &mut File,
    plan: &ChunkPlan,
    payload_id: PayloadId,
    total_len: u64,
    start: u64,
    ack_rx: &mut watch::Receiver<AckState>,
    token: &CancelToken,
    queue: &TransferQueue,
    index: u32,
    throttle: &mut ProgressThrottle,
    emit: &mut F,
) -> Result<(u64, u64), BatchError>
where
    W: AsyncWrite + Unpin,
    F: FnMut(BatchEvent),
{
    let mut next = plan.align_down(start.min(total_len));
    let mut high_water = next;
    let mut scratch = vec![0_u8; plan.chunk_size() as usize];
    let mut bytes_sent = 0_u64;
    let mut chunks_sent = 0_u64;
    let mut last_seen: Option<(u64, bool)> = None;

    while next < total_len {
        if token.is_cancelled() {
            return Err(BatchError::Cancelled);
        }
        let snapshot = *ack_rx.borrow();
        if let Some(code) = snapshot.aborted {
            return Err(BatchError::Transport(
                libcore_transport::TransferError::PeerAborted { code },
            ));
        }
        if snapshot.protocol_error {
            return Err(BatchError::Protocol(
                "the receiver sent a frame only a sender may send".to_owned(),
            ));
        }
        if snapshot.closed {
            return Err(BatchError::Transport(
                libcore_transport::TransferError::PeerClosed,
            ));
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
                    source.seek(SeekFrom::Start(next)).await?;
                }
            }
        }

        let range = plan
            .range_at(next, total_len)
            .ok_or_else(|| BatchError::Protocol("chunk plan lost the payload tail".to_owned()))?;
        let len = range.len as usize;
        source
            .read_exact(&mut scratch[..len])
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    BatchError::Manifest(ManifestError::FileChanged(
                        "a source file shrank while it was being sent".to_owned(),
                    ))
                } else {
                    BatchError::Io(error)
                }
            })?;
        let header = PayloadHeader {
            payload_id,
            offset: next,
            checksum: chunk_sha256(&scratch[..len]),
        };
        write_payload(writer, &header, &scratch[..len]).await?;
        next += u64::from(range.len);
        bytes_sent = bytes_sent.saturating_add(u64::from(range.len));
        chunks_sent += 1;
        queue.add_wire_bytes(index, u64::from(range.len))?;
        high_water = high_water.max(next);
        queue.record_progress(index, high_water)?;
        emit_progress(queue, index, high_water, total_len, throttle, false, emit);
    }

    Ok((bytes_sent, chunks_sent))
}

async fn open_source(path: &Path, expected_len: u64) -> Result<File, BatchError> {
    let metadata = tokio::fs::metadata(path).await?;
    if !metadata.is_file() {
        return Err(BatchError::Manifest(ManifestError::NotAFile));
    }
    if metadata.len() != expected_len {
        return Err(BatchError::Manifest(ManifestError::FileChanged(
            path.to_string_lossy().into_owned(),
        )));
    }
    let file = File::open(path).await?;
    Ok(file)
}

async fn abort_remaining<W>(
    writer: &mut W,
    manifest: &TransferManifest,
    start: usize,
) -> Result<(), BatchError>
where
    W: AsyncWrite + Unpin,
{
    for index in start..manifest.len() {
        let payload_id = manifest
            .payload_id(index)
            .ok_or_else(|| BatchError::Protocol("manifest index lost its payload id".to_owned()))?;
        write_abort(
            writer,
            &Abort {
                payload_id,
                code: AbortCode::Unsupported,
            },
        )
        .await?;
    }
    Ok(())
}

async fn pump_ack_frames<R>(mut reader: R, senders: HashMap<u64, watch::Sender<AckState>>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut buffer = FrameBuffer::new();
    loop {
        match read_frame(&mut reader, &mut buffer).await {
            Ok(Frame::AckOffset(ack)) => {
                if let Some(sender) = senders.get(&ack.payload_id.get()) {
                    sender.send_modify(|state| {
                        state.offset = Some(ack.offset);
                        state.repair = ack.repair;
                    });
                } else {
                    mark_all(&senders, |state| state.protocol_error = true);
                    return;
                }
            }
            Ok(Frame::Complete(complete)) => {
                if let Some(sender) = senders.get(&complete.payload_id.get()) {
                    sender.send_modify(|state| state.complete = true);
                } else {
                    mark_all(&senders, |state| state.protocol_error = true);
                    return;
                }
            }
            Ok(Frame::Abort(abort)) => {
                if let Some(sender) = senders.get(&abort.payload_id.get()) {
                    sender.send_modify(|state| state.aborted = Some(abort.code));
                } else {
                    mark_all(&senders, |state| state.protocol_error = true);
                    return;
                }
            }
            Ok(_) => {
                mark_all(&senders, |state| state.protocol_error = true);
                return;
            }
            Err(_) => {
                mark_all(&senders, |state| state.closed = true);
                return;
            }
        }
    }
}

fn mark_all(senders: &HashMap<u64, watch::Sender<AckState>>, update: impl Fn(&mut AckState)) {
    for sender in senders.values() {
        sender.send_modify(|state| update(state));
    }
}

async fn wait_for_offset(
    receiver: &mut watch::Receiver<AckState>,
    timeout: Duration,
) -> Result<u64, BatchError> {
    let wait = async {
        loop {
            let state = *receiver.borrow_and_update();
            if let Some(code) = state.aborted {
                return Err(BatchError::Transport(
                    libcore_transport::TransferError::PeerAborted { code },
                ));
            }
            if state.protocol_error {
                return Err(BatchError::Protocol(
                    "the receiver sent a frame only a sender may send".to_owned(),
                ));
            }
            if state.closed {
                return Err(BatchError::Transport(
                    libcore_transport::TransferError::PeerClosed,
                ));
            }
            if let Some(offset) = state.offset {
                return Ok(offset);
            }
            receiver
                .changed()
                .await
                .map_err(|_| BatchError::Transport(libcore_transport::TransferError::PeerClosed))?;
        }
    };
    tokio::time::timeout(timeout, wait).await.map_err(|_| {
        BatchError::Transport(libcore_transport::TransferError::Timeout(
            "waiting for the receiver's opening ACK_OFFSET",
        ))
    })?
}

async fn wait_for_complete(
    receiver: &mut watch::Receiver<AckState>,
    timeout: Duration,
) -> Result<(), BatchError> {
    let wait = async {
        loop {
            let state = *receiver.borrow_and_update();
            if state.complete {
                return Ok(());
            }
            if let Some(code) = state.aborted {
                return Err(BatchError::Transport(
                    libcore_transport::TransferError::PeerAborted { code },
                ));
            }
            if state.protocol_error {
                return Err(BatchError::Protocol(
                    "the receiver sent a frame only a sender may send".to_owned(),
                ));
            }
            if state.closed {
                return Err(BatchError::Transport(
                    libcore_transport::TransferError::PeerClosed,
                ));
            }
            receiver
                .changed()
                .await
                .map_err(|_| BatchError::Transport(libcore_transport::TransferError::PeerClosed))?;
        }
    };
    tokio::time::timeout(timeout, wait).await.map_err(|_| {
        BatchError::Transport(libcore_transport::TransferError::Timeout(
            "waiting for the receiver's COMPLETE",
        ))
    })?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn ack_channel_routes_only_the_matching_payload() {
        let (first, mut first_rx) = watch::channel(AckState::default());
        let (second, mut second_rx) = watch::channel(AckState::default());
        let mut map = HashMap::new();
        map.insert(1, first);
        map.insert(2, second);

        let task = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        task.block_on(async move {
            mark_all(&map, |state| state.closed = true);
            assert!(first_rx.borrow_and_update().closed);
            assert!(second_rx.borrow_and_update().closed);
        });
    }
}
