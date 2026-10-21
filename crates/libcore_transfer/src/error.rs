//! Errors from manifest preparation, queueing and batch transfer.

use std::fmt;
use std::io;

use libcore_transport::TransferError;

use crate::manifest::ManifestError;
use crate::queue::QueueError;

/// Any failure the batch layer can report.
#[derive(Debug)]
#[non_exhaustive]
pub enum BatchError {
    /// A manifest was malformed, or a local file could not be prepared.
    Manifest(ManifestError),
    /// A queue transition was rejected.
    Queue(QueueError),
    /// The per-file resumable engine failed.
    Transport(TransferError),
    /// A local filesystem operation outside the transport engine failed.
    Io(io::Error),
    /// The batch protocol was violated.
    Protocol(String),
    /// The local caller cancelled the batch.
    Cancelled,
    /// A batch cannot start without at least one file.
    EmptyBatch,
}

impl BatchError {
    /// Short machine-readable kind for event payloads.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Manifest(_) => "manifest",
            Self::Queue(_) => "queue",
            Self::Transport(_) => "transport",
            Self::Io(_) => "io",
            Self::Protocol(_) => "protocol",
            Self::Cancelled => "cancelled",
            Self::EmptyBatch => "empty_batch",
        }
    }

    /// Whether the batch can move to the next file after this failure.
    ///
    /// A missing local file or a peer abort for one item is recoverable; a
    /// broken/dead connection, a protocol violation, or a timeout is not.
    #[must_use]
    pub fn is_recoverable(&self) -> bool {
        match self {
            Self::Transport(TransferError::PeerAborted { .. })
            | Self::Transport(TransferError::ContentMismatch)
            | Self::Transport(TransferError::FileChecksumMismatch { .. })
            | Self::Transport(TransferError::ChunkChecksumMismatch { .. }) => true,
            Self::Manifest(_) | Self::Io(_) => true,
            Self::Transport(_) | Self::Queue(_) | Self::Protocol(_) => false,
            Self::Cancelled | Self::EmptyBatch => false,
        }
    }

    /// Whether the failure is a local cancellation.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

impl fmt::Display for BatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(error) => write!(f, "manifest error: {error}"),
            Self::Queue(error) => write!(f, "queue error: {error}"),
            Self::Transport(error) => write!(f, "transfer error: {error}"),
            Self::Io(error) => write!(f, "i/o error: {error}"),
            Self::Protocol(message) => write!(f, "batch protocol error: {message}"),
            Self::Cancelled => f.write_str("the batch was cancelled locally"),
            Self::EmptyBatch => f.write_str("the batch contains no files"),
        }
    }
}

impl std::error::Error for BatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(error) => Some(error),
            Self::Queue(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Protocol(_) | Self::Cancelled | Self::EmptyBatch => None,
        }
    }
}

impl From<ManifestError> for BatchError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<QueueError> for BatchError {
    fn from(error: QueueError) -> Self {
        Self::Queue(error)
    }
}

impl From<TransferError> for BatchError {
    fn from(error: TransferError) -> Self {
        Self::Transport(error)
    }
}

impl From<io::Error> for BatchError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
