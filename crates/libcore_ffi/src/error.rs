//! The single error type crossing the FFI boundary.
//!
//! Every fallible export returns [`FfiError`]; every terminal failure event
//! carries the same information as a `(kind, message)` pair. The variants are
//! deliberately coarse and string-only: an error path that allocates richly or
//! carries unbounded attacker-controlled data is an error path a peer can use
//! to pressure the process.
//!
//! Messages never contain secret material: the underlying crates already
//! redact key bytes and digests from their `Display` impls, and this layer adds
//! no payload bytes of its own.

use core::fmt;

use libcore_discovery::DiscoveryError;
use libcore_transport::{AbortCode, TransferError};

/// Why a peer abandoned a transfer, mirroring the wire abort codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum AbortReason {
    /// The peer violated the framing or sequencing rules.
    ProtocolViolation,
    /// A digest check failed in a way the peer cannot repair.
    IntegrityCheckFailed,
    /// The peer's local storage failed (disk full, permissions).
    StorageFailure,
    /// The peer does not support this transfer's parameters.
    Unsupported,
    /// An abort code this build does not know.
    Other,
}

impl From<AbortCode> for AbortReason {
    fn from(code: AbortCode) -> Self {
        match code {
            AbortCode::ProtocolViolation => Self::ProtocolViolation,
            AbortCode::IntegrityCheckFailed => Self::IntegrityCheckFailed,
            AbortCode::StorageFailure => Self::StorageFailure,
            AbortCode::Unsupported => Self::Unsupported,
            // `AbortCode` is `#[non_exhaustive]`; a newer peer may send a code
            // this build has not learned yet.
            _ => Self::Other,
        }
    }
}

/// Everything the FFI surface can fail with.
#[derive(Clone, Debug, uniffi::Error)]
pub enum FfiError {
    /// A caller supplied a value the engine cannot use (bad hex, bad
    /// address, empty path, chunk size out of range).
    InvalidArgument {
        /// Human-readable detail.
        message: String,
    },

    /// An operation was requested before the engine was started.
    NotStarted,

    /// `PeerDiscoveryEngine::start` was called twice.
    AlreadyStarted,

    /// A one-shot session was run a second time.
    AlreadyFinished,

    /// The transfer was stopped by `cancel()`.
    ///
    /// The receiver's part file and sidecar survive, so retrying with the
    /// same payload id resumes rather than restarts.
    Cancelled,

    /// The peer closed the connection before the transfer completed.
    PeerClosed,

    /// A wait exceeded its configured deadline.
    Timeout {
        /// Which wait expired.
        message: String,
    },

    /// The peer sent a structurally invalid frame, or a state machine was
    /// entered out of order.
    Protocol {
        /// Human-readable detail.
        message: String,
    },

    /// The underlying socket or filesystem failed.
    Io {
        /// Human-readable detail.
        message: String,
    },

    /// A frame carried a different payload id than the transfer in progress.
    PayloadMismatch {
        /// The payload id the local side expected.
        expected: u64,
        /// The payload id that arrived.
        received: u64,
    },

    /// A chunk or whole-file digest did not match.
    ChecksumMismatch {
        /// Human-readable detail.
        message: String,
    },

    /// The peer sent an abort frame.
    PeerAborted {
        /// The code the peer aborted with.
        code: AbortReason,
    },

    /// Discovery failed to start or a discovery operation failed.
    Discovery {
        /// Human-readable detail.
        message: String,
    },

    /// An internal invariant failed (runtime unavailable, task panicked).
    Internal {
        /// Human-readable detail.
        message: String,
    },
}

impl FfiError {
    /// Builds an [`Self::InvalidArgument`].
    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            message: message.into(),
        }
    }

    /// Builds an [`Self::Internal`].
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    /// A short machine-readable kind, used by failure events.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::InvalidArgument { .. } => "invalid_argument",
            Self::NotStarted => "not_started",
            Self::AlreadyStarted => "already_started",
            Self::AlreadyFinished => "already_finished",
            Self::Cancelled => "cancelled",
            Self::PeerClosed => "peer_closed",
            Self::Timeout { .. } => "timeout",
            Self::Protocol { .. } => "protocol",
            Self::Io { .. } => "io",
            Self::PayloadMismatch { .. } => "payload_mismatch",
            Self::ChecksumMismatch { .. } => "checksum_mismatch",
            Self::PeerAborted { .. } => "peer_aborted",
            Self::Discovery { .. } => "discovery",
            Self::Internal { .. } => "internal",
        }
    }
}

impl fmt::Display for FfiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArgument { message } => write!(f, "invalid argument: {message}"),
            Self::NotStarted => f.write_str("the engine has not been started"),
            Self::AlreadyStarted => f.write_str("the engine has already been started"),
            Self::AlreadyFinished => f.write_str("the session has already run"),
            Self::Cancelled => f.write_str("the operation was cancelled locally"),
            Self::PeerClosed => f.write_str("peer closed the connection"),
            Self::Timeout { message } => write!(f, "timed out: {message}"),
            Self::Protocol { message } => write!(f, "protocol violation: {message}"),
            Self::Io { message } => write!(f, "i/o error: {message}"),
            Self::PayloadMismatch { expected, received } => write!(
                f,
                "payload id mismatch: expected {expected:016x}, received {received:016x}"
            ),
            Self::ChecksumMismatch { message } => write!(f, "checksum mismatch: {message}"),
            Self::PeerAborted { code } => write!(f, "peer aborted the transfer: {code:?}"),
            Self::Discovery { message } => write!(f, "discovery error: {message}"),
            Self::Internal { message } => write!(f, "internal error: {message}"),
        }
    }
}

impl std::error::Error for FfiError {}

impl From<TransferError> for FfiError {
    fn from(error: TransferError) -> Self {
        match error {
            TransferError::Io(error) => Self::Io {
                message: error.to_string(),
            },
            TransferError::Protocol(detail) => Self::Protocol {
                message: detail.to_owned(),
            },
            TransferError::PeerClosed => Self::PeerClosed,
            TransferError::Timeout(what) => Self::Timeout {
                message: what.to_owned(),
            },
            TransferError::PayloadMismatch { expected, received } => {
                Self::PayloadMismatch { expected, received }
            }
            TransferError::FileChecksumMismatch { .. } => Self::ChecksumMismatch {
                message: error_message(
                    "the whole-file digest did not match the sender's announced digest",
                ),
            },
            TransferError::ChunkChecksumMismatch { offset } => Self::ChecksumMismatch {
                message: format!("chunk at offset {offset} did not verify"),
            },
            TransferError::PeerAborted { code } => Self::PeerAborted { code: code.into() },
            TransferError::FrameTooLarge { size, max } => Self::Protocol {
                message: format!("frame body of {size} bytes exceeds the {max}-byte maximum"),
            },
            TransferError::InvalidChunkSize { size } => Self::InvalidArgument {
                message: format!("chunk size {size} is outside the supported range"),
            },
            TransferError::Random => Self::Internal {
                message: error_message("the system random number generator failed"),
            },
            // `TransferError` is `#[non_exhaustive]`; a future variant must
            // still produce a well-formed error at the boundary.
            _ => Self::Internal {
                message: error_message("unsupported transfer failure"),
            },
        }
    }
}

impl From<DiscoveryError> for FfiError {
    fn from(error: DiscoveryError) -> Self {
        Self::Discovery {
            message: error.to_string(),
        }
    }
}

impl From<uniffi::UnexpectedUniFFICallbackError> for FfiError {
    fn from(error: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Internal {
            message: format!("a foreign callback failed: {error}"),
        }
    }
}

/// `TransferError` cannot be formatted after moving a field out of it; this
/// keeps the fixed strings in one place.
fn error_message(detail: &str) -> String {
    detail.to_owned()
}
