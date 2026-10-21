//! Multi-file transfer orchestration.
//!
//! `libcore_transport` moves one file over one reliable byte stream. This crate
//! adds what a multi-file session needs without changing that engine:
//!
//! * a canonical, validated [`TransferManifest`] that both sides agree on
//!   before the first payload byte moves;
//! * a thread-safe [`TransferQueue`] that tracks pending, active, completed and
//!   failed items plus overall progress and an ETA;
//! * [`upload_batch`] and [`receive_batch`], which carry one manifest and then
//!   run the existing per-file resumable exchange for every entry over the same
//!   authenticated stream;
//! * [`BatchEvent`], a small data-only event stream that the FFI forwards to
//!   Kotlin and C#.
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use libcore_transfer::{
//!     receive_batch, BatchOptions, CancelToken, TransferQueue,
//! };
//!
//! # async fn example<S>(stream: S) -> Result<(), libcore_transfer::BatchError>
//! # where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin {
//! let queue = TransferQueue::empty();
//! let report = receive_batch(
//!     stream,
//!     &queue,
//!     Path::new("downloads"),
//!     &BatchOptions::new(),
//!     &CancelToken::new(),
//!     |_event| {},
//! )
//! .await?;
//! assert_eq!(report.completed_files, report.total_files);
//! # Ok(())
//! # }
//! ```
//!
//! # Trust
//!
//! The manifest is carried inside the caller's authenticated stream, so its
//! paths, sizes and digests are bound to the same peer as the payload. The
//! receiver still re-validates every path and requires each `HELLO` to match
//! the manifest before the transport engine writes a byte.

mod cancel;
mod error;
mod event;
mod manifest;
mod options;
mod progress;
mod protocol;
mod queue;
mod receiver;
mod sender;

pub use cancel::{is_cancelled_error, CancelStream, CancelToken};
pub use error::BatchError;
pub use event::BatchEvent;
pub use manifest::{
    normalize_relative_path, payload_id_for, ManifestEntry, ManifestError, PreparedBatch,
    PreparedFile, TransferManifest, MANIFEST_ENTRY_FIXED_LEN, MANIFEST_HEADER_LEN, MANIFEST_MAGIC,
    MANIFEST_VERSION, MAX_MANIFEST_BYTES, MAX_MANIFEST_FILES, MAX_MANIFEST_PATH_BYTES,
};
pub use options::BatchOptions;
pub use protocol::{read_manifest, write_manifest};
pub use queue::{
    BatchReport, QueueError, QueueItemSnapshot, QueueItemStatus, QueueSnapshot, QueueStatus,
    TransferQueue,
};
pub use receiver::receive_batch;
pub use sender::upload_batch;
