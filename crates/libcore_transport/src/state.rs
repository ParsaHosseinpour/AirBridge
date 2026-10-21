//! Persisted resume state.
//!
//! A receiver writes a small sidecar file next to its part file after every
//! acknowledged span. The sidecar records which bytes are known-good — but it
//! is treated as an *untrusted hint*: anyone with write access to the
//! directory can edit it. What actually protects the resumed transfer is that
//!
//! * the sidecar also carries the SHA-256 of the verified prefix, which the
//!   receiver recomputes from the part file before honouring the offset, and
//! * the running whole-file digest is checked against the sender's announced
//!   digest before the part file is renamed into place.
//!
//! So a tampered sidecar can at worst make a transfer restart from zero; it
//! cannot make a receiver accept wrong bytes. That is also why a missing or
//! corrupt sidecar is not an error: [`ResumeState::load`] returns `None` and
//! the receiver starts the payload from byte zero.

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::chunk::CHUNK_SHA256_LEN;
use crate::error::TransferError;
use crate::frame::PayloadId;

/// Magic bytes at the start of a sidecar file.
pub const STATE_MAGIC: [u8; 4] = *b"ABRS";

/// Current sidecar format version.
pub const STATE_VERSION: u8 = 1;

/// Encoded size of a resume state, in bytes.
pub const STATE_LEN: usize = 4 + 1 + 1 + 8 + 8 + 4 + 8 + CHUNK_SHA256_LEN + CHUNK_SHA256_LEN;

/// Bit 0: a whole-file digest is present.
const STATE_FLAG_FILE_SHA256: u8 = 0x01;

/// Everything a receiver needs to reconcile a fresh connection with its part
/// file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeState {
    /// Identifies the payload this state belongs to.
    pub payload_id: PayloadId,
    /// Total size of the payload, in bytes.
    pub total_len: u64,
    /// Chunk grid the offsets are aligned to.
    pub chunk_size: u32,
    /// Number of verified bytes at the start of the part file, always a
    /// multiple of `chunk_size`.
    pub verified_offset: u64,
    /// The whole-file digest announced by the sender, when it had one.
    pub file_sha256: Option<[u8; CHUNK_SHA256_LEN]>,
    /// SHA-256 over the part file's first `verified_offset` bytes.
    pub prefix_sha256: [u8; CHUNK_SHA256_LEN],
}

impl ResumeState {
    /// Serialises the state into its fixed-size encoding.
    #[must_use]
    pub fn encode(&self) -> [u8; STATE_LEN] {
        let mut out = [0_u8; STATE_LEN];
        out[..4].copy_from_slice(&STATE_MAGIC);
        out[4] = STATE_VERSION;
        out[5] = if self.file_sha256.is_some() {
            STATE_FLAG_FILE_SHA256
        } else {
            0
        };
        out[6..14].copy_from_slice(&self.payload_id.get().to_be_bytes());
        out[14..22].copy_from_slice(&self.total_len.to_be_bytes());
        out[22..26].copy_from_slice(&self.chunk_size.to_be_bytes());
        out[26..34].copy_from_slice(&self.verified_offset.to_be_bytes());
        if let Some(digest) = &self.file_sha256 {
            out[34..34 + CHUNK_SHA256_LEN].copy_from_slice(digest);
        }
        out[34 + CHUNK_SHA256_LEN..].copy_from_slice(&self.prefix_sha256);
        out
    }

    /// Parses a fixed-size encoding.
    ///
    /// # Errors
    ///
    /// Returns [`TransferError::Protocol`] when the magic, version, length or
    /// internal consistency checks fail.
    pub fn decode(bytes: &[u8]) -> Result<Self, TransferError> {
        if bytes.len() != STATE_LEN {
            return Err(TransferError::Protocol("resume state has the wrong length"));
        }
        if bytes[..4] != STATE_MAGIC {
            return Err(TransferError::Protocol("resume state magic mismatch"));
        }
        if bytes[4] != STATE_VERSION {
            return Err(TransferError::Protocol("unsupported resume state version"));
        }
        let flags = bytes[5];
        let mut payload_id = [0_u8; 8];
        payload_id.copy_from_slice(&bytes[6..14]);
        let mut total = [0_u8; 8];
        total.copy_from_slice(&bytes[14..22]);
        let mut chunk = [0_u8; 4];
        chunk.copy_from_slice(&bytes[22..26]);
        let mut offset = [0_u8; 8];
        offset.copy_from_slice(&bytes[26..34]);
        let mut file_sha256 = [0_u8; CHUNK_SHA256_LEN];
        file_sha256.copy_from_slice(&bytes[34..34 + CHUNK_SHA256_LEN]);
        let mut prefix_sha256 = [0_u8; CHUNK_SHA256_LEN];
        prefix_sha256.copy_from_slice(&bytes[34 + CHUNK_SHA256_LEN..]);

        let state = Self {
            payload_id: PayloadId::new(u64::from_be_bytes(payload_id)),
            total_len: u64::from_be_bytes(total),
            chunk_size: u32::from_be_bytes(chunk),
            verified_offset: u64::from_be_bytes(offset),
            file_sha256: (flags & STATE_FLAG_FILE_SHA256 != 0).then_some(file_sha256),
            prefix_sha256,
        };
        if state.verified_offset > state.total_len {
            return Err(TransferError::Protocol(
                "resume offset exceeds payload length",
            ));
        }
        Ok(state)
    }

    /// Loads a sidecar file.
    ///
    /// Returns `None` for a missing, unreadable, truncated or corrupt file,
    /// and for a future format version. All of those mean the same thing to a
    /// caller: no trustworthy state, start from zero.
    pub async fn load(path: &Path) -> Option<Self> {
        let bytes = fs::read(path).await.ok()?;
        Self::decode(&bytes).ok()
    }

    /// Writes a sidecar file.
    ///
    /// The write happens after the corresponding part-file bytes have been
    /// flushed by the caller, so a crash between the two leaves state that is
    /// *older* than the data, never newer.
    ///
    /// # Errors
    ///
    /// Propagates filesystem failures.
    pub async fn store(&self, path: &Path) -> Result<(), TransferError> {
        fs::write(path, self.encode()).await?;
        Ok(())
    }
}

/// The part file that receives payload bytes before the final rename.
#[must_use]
pub fn part_path(destination: &Path) -> PathBuf {
    let mut name = destination.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

/// The sidecar path belonging to a part file.
#[must_use]
pub fn state_path_for(part: &Path) -> PathBuf {
    let mut name = part.as_os_str().to_os_string();
    name.push(".state");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sample() -> ResumeState {
        ResumeState {
            payload_id: PayloadId::new(0xDEAD_BEEF),
            total_len: 10_000,
            chunk_size: 4096,
            verified_offset: 8192,
            file_sha256: Some([0x11; 32]),
            prefix_sha256: [0x22; 32],
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let state = sample();
        assert_eq!(ResumeState::decode(&state.encode()).unwrap(), state);
    }

    #[test]
    fn absent_file_digest_roundtrips() {
        let state = ResumeState {
            file_sha256: None,
            ..sample()
        };
        let decoded = ResumeState::decode(&state.encode()).unwrap();
        assert_eq!(decoded, state);
        assert_eq!(decoded.file_sha256, None);
    }

    #[test]
    fn encoding_is_deterministic() {
        assert_eq!(sample().encode(), sample().encode());
        assert_eq!(
            ResumeState::decode(&sample().encode()).unwrap().encode(),
            sample().encode()
        );
    }

    #[test]
    fn wrong_length_is_rejected() {
        let bytes = sample().encode();
        assert!(ResumeState::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut bytes = sample().encode();
        bytes[0] = b'X';
        assert!(ResumeState::decode(&bytes).is_err());
    }

    #[test]
    fn future_version_is_rejected() {
        let mut bytes = sample().encode();
        bytes[4] = STATE_VERSION + 1;
        assert!(ResumeState::decode(&bytes).is_err());
    }

    #[test]
    fn offset_beyond_total_is_rejected() {
        let state = ResumeState {
            verified_offset: 10_001,
            ..sample()
        };
        assert!(ResumeState::decode(&state.encode()).is_err());
    }

    #[test]
    fn corrupt_state_is_rejected_rather_than_fatal() {
        // `load` maps every failure onto `None`; `decode` is the strict half.
        assert!(ResumeState::decode(b"not a resume state").is_err());
    }

    #[test]
    fn derived_paths_are_stable() {
        let dest = Path::new("/tmp/file.bin");
        let part = part_path(dest);
        assert_eq!(part, Path::new("/tmp/file.bin.part"));
        assert_eq!(state_path_for(&part), Path::new("/tmp/file.bin.part.state"));
    }
}
