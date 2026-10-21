//! Sequential batch receive over one stream.
//!
//! The receiver reads one manifest, installs it into the queue, and then calls
//! the ordinary per-file [`libcore_transport::receive_file_mut_with_progress`]
//! engine once per entry. The transport engine remains responsible for
//! resumption, per-chunk integrity and verified commits; this module adds the
//! manifest-derived destination, batch progress and the decision of whether one
//! bad file ends the batch or the next one starts.
//!
//! # Destination safety
//!
//! Every path in the manifest has already been validated by the decoder, but
//! the destination is still rebuilt component by component here. A relative
//! path can therefore only ever name a descendant of `destination_root`, even
//! if a future manifest version changes the wire validation.

use std::path::{Path, PathBuf};

use libcore_transport::{
    receive_file_mut_with_progress, write_abort, Abort, AbortCode, ReceiveOptions, TransferError,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::cancel::{CancelStream, CancelToken};
use crate::error::BatchError;
use crate::event::BatchEvent;
use crate::manifest::{ManifestEntry, TransferManifest};
use crate::options::BatchOptions;
use crate::progress::{emit_progress, ProgressThrottle};
use crate::protocol::read_manifest;
use crate::queue::{BatchReport, TransferQueue};

/// Receives every file in the peer's manifest under `destination_root`.
///
/// The caller supplies the empty [`TransferQueue`] it wants the transfer to
/// populate; the manifest read from the stream is installed into it before the
/// first file starts. The stream must already be authenticated.
///
/// Recoverable per-item failures are reported in [`BatchEvent::FileFailed`] and
/// in the returned [`BatchReport`]. When `continue_on_error` is disabled, the
/// receiver tells the sender to skip the remaining entries and returns a report
/// for the files it actually attempted. A fatal connection/protocol failure is
/// returned as `Err`.
///
/// # Errors
///
/// Returns [`BatchError`] for an empty/invalid manifest, a queue that is not
/// fresh, cancellation, or an unrecoverable transport failure.
pub async fn receive_batch<S, F>(
    stream: S,
    queue: &TransferQueue,
    destination_root: &Path,
    options: &BatchOptions,
    token: &CancelToken,
    mut emit: F,
) -> Result<BatchReport, BatchError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(BatchEvent),
{
    options.validate()?;
    let mut stream = CancelStream::new(stream, token.clone());

    let manifest =
        match tokio::time::timeout(options.response_timeout, read_manifest(&mut stream)).await {
            Ok(Ok(manifest)) => manifest,
            Ok(Err(error)) => {
                if token.is_cancelled() {
                    queue.cancel();
                    return Err(BatchError::Cancelled);
                }
                return Err(error);
            }
            Err(_) => {
                if token.is_cancelled() {
                    queue.cancel();
                    return Err(BatchError::Cancelled);
                }
                return Err(BatchError::Transport(TransferError::Timeout(
                    "waiting for the batch manifest",
                )));
            }
        };
    if token.is_cancelled() {
        queue.cancel();
        return Err(BatchError::Cancelled);
    }
    if manifest.is_empty() {
        return Err(BatchError::EmptyBatch);
    }

    tokio::fs::create_dir_all(destination_root).await?;
    queue.install_manifest(&manifest)?;
    queue.begin()?;

    emit(BatchEvent::Started {
        batch_id: manifest.batch_id(),
        total_files: u32::try_from(manifest.len()).unwrap_or(u32::MAX),
        total_bytes: manifest.total_bytes(),
    });

    let mut throttle = ProgressThrottle::new(options.progress_interval);

    for (position, entry) in manifest.entries().iter().enumerate() {
        let index = u32::try_from(position).unwrap_or(u32::MAX);
        if token.is_cancelled() {
            queue.cancel();
            return Err(BatchError::Cancelled);
        }

        let payload_id = manifest
            .payload_id(position)
            .ok_or_else(|| BatchError::Protocol("manifest index lost its payload id".to_owned()))?;
        queue.activate(index)?;
        emit(BatchEvent::FileStarted {
            index,
            relative_path: entry.relative_path().to_owned(),
            size: entry.size(),
        });

        // Prepare the destination before announcing anything to the sender.
        // Any failure here means the peer is still waiting for this entry's
        // HELLO, so the handler below sends an ABORT to skip it.
        let destination = match prepare_destination(destination_root, entry).await {
            Ok(destination) => destination,
            Err(error) => {
                record_failure(queue, index, entry, &error, &mut emit)?;
                if token.is_cancelled() {
                    queue.cancel();
                    return Err(BatchError::Cancelled);
                }
                let stop = !(error.is_recoverable() && options.continue_on_error);
                if write_abort(
                    &mut stream,
                    &Abort {
                        payload_id,
                        code: AbortCode::StorageFailure,
                    },
                )
                .await
                .is_err()
                {
                    queue.cancel_pending_from(index.saturating_add(1));
                    queue.finish();
                    return Err(error);
                }
                if stop {
                    queue.cancel_pending_from(index.saturating_add(1));
                    abort_remaining(&mut stream, &manifest, position + 1).await;
                    return finish(queue, &mut emit);
                }
                continue;
            }
        };

        let mut receive_options = ReceiveOptions::new()
            .with_expected_payload_id(payload_id)
            .with_expected_total_len(entry.size())
            .with_ack_every_bytes(options.ack_every_bytes)
            .with_file_sha256(options.verify_whole_file);
        if options.verify_whole_file {
            receive_options = receive_options.with_expected_file_sha256(entry.sha256());
        }

        let result =
            receive_file_mut_with_progress(&mut stream, &destination, &receive_options, |bytes| {
                if queue.record_progress(index, bytes).is_ok() {
                    emit_progress(
                        queue,
                        index,
                        bytes,
                        entry.size(),
                        &mut throttle,
                        false,
                        &mut emit,
                    );
                }
            })
            .await;

        match result {
            Ok(report) => {
                queue.complete(index, report.resumed_from, Some(report.path.clone()))?;
                emit(BatchEvent::FileCompleted {
                    index,
                    relative_path: entry.relative_path().to_owned(),
                    size: entry.size(),
                    resumed_from: report.resumed_from,
                    destination_path: Some(report.path),
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
                let error = BatchError::from(error);
                record_failure(queue, index, entry, &error, &mut emit)?;

                if token.is_cancelled() {
                    queue.cancel();
                    return Err(BatchError::Cancelled);
                }

                let recoverable = error.is_recoverable();
                // Tell the peer this payload is over even when the transport
                // engine already sent an abort: routing is by payload id, so a
                // duplicate is harmless, and a digest/disk failure after the
                // final chunk would otherwise leave the sender waiting for a
                // COMPLETE that will never come.
                let _ = write_abort(
                    &mut stream,
                    &Abort {
                        payload_id,
                        code: if recoverable {
                            AbortCode::StorageFailure
                        } else {
                            AbortCode::ProtocolViolation
                        },
                    },
                )
                .await;

                if !recoverable {
                    queue.cancel_pending_from(index.saturating_add(1));
                    queue.finish();
                    return Err(error);
                }
                if !options.continue_on_error {
                    queue.cancel_pending_from(index.saturating_add(1));
                    abort_remaining(&mut stream, &manifest, position + 1).await;
                    return finish(queue, &mut emit);
                }
            }
        }
    }

    finish(queue, &mut emit)
}

/// Builds the destination under `root` and creates its parent directory.
async fn prepare_destination(root: &Path, entry: &ManifestEntry) -> Result<PathBuf, BatchError> {
    let destination = destination_path(root, entry.relative_path())?;
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    Ok(destination)
}

/// Joins a validated relative path onto `root`, component by component.
///
/// This is deliberately not `root.join(relative)`: on Windows a backslash or a
/// drive prefix could otherwise escape the root. The manifest uses only `/`
/// separators and every component is checked again here.
fn destination_path(root: &Path, relative: &str) -> Result<PathBuf, BatchError> {
    let mut path = root.to_path_buf();
    for component in relative.split('/') {
        if component.is_empty() || component == "." || component == ".." || component.contains(':')
        {
            return Err(BatchError::Protocol(format!(
                "the peer sent an unsafe relative path: {relative:?}"
            )));
        }
        path.push(component);
    }
    Ok(path)
}

/// Records and publishes one item failure.
fn record_failure<F>(
    queue: &TransferQueue,
    index: u32,
    entry: &ManifestEntry,
    error: &BatchError,
    emit: &mut F,
) -> Result<(), BatchError>
where
    F: FnMut(BatchEvent),
{
    let message = error.to_string();
    queue.fail(index, message.clone())?;
    emit(BatchEvent::FileFailed {
        index,
        relative_path: entry.relative_path().to_owned(),
        kind: error.kind().to_owned(),
        message,
    });
    Ok(())
}

/// Tells the sender to skip every entry from `start` onward.
async fn abort_remaining<S>(stream: &mut S, manifest: &TransferManifest, start: usize)
where
    S: AsyncWrite + Unpin,
{
    for index in start..manifest.len() {
        let Some(payload_id) = manifest.payload_id(index) else {
            break;
        };
        if write_abort(
            stream,
            &Abort {
                payload_id,
                code: AbortCode::Unsupported,
            },
        )
        .await
        .is_err()
        {
            break;
        }
    }
}

/// Finishes the queue, publishes the final report, and returns it.
fn finish<F>(queue: &TransferQueue, emit: &mut F) -> Result<BatchReport, BatchError>
where
    F: FnMut(BatchEvent),
{
    queue.finish();
    let report = queue.report();
    emit(BatchEvent::Finished {
        report: report.clone(),
    });
    Ok(report)
}
