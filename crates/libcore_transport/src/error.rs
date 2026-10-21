//! Error type for the AirBridge file transport.
//!
//! As in the crypto core, the variants are flat and carry no heap-allocated
//! strings: an error path that allocates is an error path an attacker can make
//! us walk while holding transfer state hostage.

use core::fmt;
use std::io;

use crate::frame::AbortCode;

/// Errors produced by the AirBridge transport engine.
#[derive(Debug)]
#[non_exhaustive]
pub enum TransferError {
    /// The underlying stream or filesystem failed.
    Io(io::Error),

    /// A peer sent a structurally invalid frame.
    Protocol(&'static str),

    /// The peer closed the connection before the transfer completed.
    PeerClosed,

    /// A wait exceeded its configured deadline.
    ///
    /// The transfer is *not* lost: the receiver has persisted everything it
    /// verified, and a later attempt resumes from there.
    Timeout(&'static str),

    /// A frame carried a different payload id than the transfer in progress.
    PayloadMismatch {
        /// The payload id the local side was expecting.
        expected: u64,
        /// The payload id that arrived.
        received: u64,
    },

    /// A chunk offset lies outside the file, or is not aligned to the chunk
    /// grid.
    OffsetOutOfRange {
        /// The offending offset.
        offset: u64,
        /// The total length of the payload.
        total_len: u64,
    },

    /// A chunk's SHA-256 digest did not match the digest in its header.
    ///
    /// The receiver answers this with a repair request; the variant is only
    /// returned when the caller is the sender and the receiver reported the
    /// failure, or when a receiver is asked to abort.
    ChunkChecksumMismatch {
        /// Chunk offset at which the mismatch was observed.
        offset: u64,
    },

    /// The whole-file SHA-256 digest did not match the announced digest.
    FileChecksumMismatch {
        /// Digest announced in the transfer header.
        expected: [u8; 32],
        /// Digest computed over the received bytes.
        actual: [u8; 32],
    },

    /// The peer sent an abort frame.
    PeerAborted {
        /// The code the peer aborted with.
        code: AbortCode,
    },

    /// A frame's declared body length exceeded the protocol maximum.
    FrameTooLarge {
        /// Declared body length, in bytes.
        size: usize,
        /// Largest legal body length, in bytes.
        max: usize,
    },

    /// A chunk size is zero, or larger than [`crate::MAX_CHUNK_SIZE`].
    InvalidChunkSize {
        /// The rejected chunk size.
        size: u32,
    },

    /// The system random number generator failed.
    Random,

    /// The cryptographic handshake or the encrypted channel failed.
    ///
    /// This covers authentication failures, replayed or reordered frames, and
    /// a malformed handshake message. The variants inside
    /// [`libcore_crypto::CryptoError`] carry the precise reason.
    Crypto(libcore_crypto::CryptoError),
}

impl From<io::Error> for TransferError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "i/o error: {error}"),
            Self::Protocol(detail) => write!(f, "protocol violation: {detail}"),
            Self::PeerClosed => f.write_str("peer closed the connection"),
            Self::Timeout(what) => write!(f, "timed out {what}"),
            Self::PayloadMismatch { expected, received } => write!(
                f,
                "payload id mismatch: expected {expected:016x}, received {received:016x}"
            ),
            Self::OffsetOutOfRange { offset, total_len } => write!(
                f,
                "chunk offset {offset} is out of range for a {total_len}-byte payload"
            ),
            Self::ChunkChecksumMismatch { offset } => {
                write!(f, "chunk checksum mismatch at offset {offset}")
            }
            Self::FileChecksumMismatch { expected, actual } => write!(
                f,
                "file checksum mismatch: expected {}, computed {}",
                hex(expected),
                hex(actual)
            ),
            Self::PeerAborted { code } => write!(f, "peer aborted the transfer: {code:?}"),
            Self::FrameTooLarge { size, max } => {
                write!(
                    f,
                    "frame body of {size} bytes exceeds the {max}-byte maximum"
                )
            }
            Self::InvalidChunkSize { size } => {
                write!(f, "invalid chunk size: {size}")
            }
            Self::Random => f.write_str("system random number generator failure"),
            Self::Crypto(error) => write!(f, "cryptographic failure: {error}"),
        }
    }
}

impl std::error::Error for TransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Crypto(error) => Some(error),
            _ => None,
        }
    }
}

/// Lower-case hex formatting for digests.
fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('?'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('?'));
    }
    out
}
