//! The chunking engine: 512 KiB blocks, per-chunk SHA-256, and the arithmetic
//! that maps a byte offset onto the chunk grid.
//!
//! Every chunk is a grid cell of size [`CHUNK_SIZE`] (or the smaller tail of
//! the file), and every offset in the resume protocol is a multiple of the
//! chunk size. Keeping one invariant — *offsets are grid-aligned* — is what
//! makes resumption a one-liner instead of a bookkeeping exercise: a verified
//! prefix of the receiver's part file always ends on a chunk boundary, so the
//! sender can seek to it and stream, and no chunk can ever partially overlap
//! another.

use ring::digest;

/// Default payload size of a `PAYLOAD_TRANSFER` frame.
///
/// 512 KiB is the point where per-chunk SHA-256 (~2 GB/s) stops being the
/// transfer's bottleneck while the frame stays small enough that one bad
/// chunk costs a fraction of a second to re-send.
pub const CHUNK_SIZE: u32 = 512 * 1024;

/// Largest chunk size the protocol accepts.
pub const MAX_CHUNK_SIZE: u32 = CHUNK_SIZE;

/// Smallest chunk size the protocol accepts.
///
/// Below this, the 53-byte frame header and the per-chunk SHA-256 stop being
/// negligible and an attacker gains a cheap amplification primitive.
pub const MIN_CHUNK_SIZE: u32 = 1024;

/// Length of a chunk SHA-256 digest, in bytes.
pub const CHUNK_SHA256_LEN: usize = 32;

/// A chunk's SHA-256 digest.
pub type ChunkDigest = [u8; CHUNK_SHA256_LEN];

/// Computes the SHA-256 digest of `data`.
#[must_use]
pub fn chunk_sha256(data: &[u8]) -> ChunkDigest {
    let digest = digest::digest(&digest::SHA256, data);
    let mut out = [0_u8; CHUNK_SHA256_LEN];
    out.copy_from_slice(digest.as_ref());
    out
}

/// Turns a partly-finalised SHA-256 context into a digest.
#[must_use]
pub(crate) fn context_digest(context: &digest::Context) -> ChunkDigest {
    let mut out = [0_u8; CHUNK_SHA256_LEN];
    out.copy_from_slice(context.clone().finish().as_ref());
    out
}

/// A half-open byte range `[offset, offset + len)` inside a payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRange {
    /// Offset of the first byte, relative to the start of the payload.
    pub offset: u64,
    /// Number of bytes in this chunk. Always `> 0` and `<= chunk_size`.
    pub len: u32,
}

/// Maps byte offsets onto the chunk grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkPlan {
    chunk_size: u32,
}

impl ChunkPlan {
    /// Creates a plan for the given chunk size.
    ///
    /// # Errors
    ///
    /// Returns [`crate::TransferError::InvalidChunkSize`] unless the size lies
    /// in `MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE`.
    pub const fn new(chunk_size: u32) -> Result<Self, crate::TransferError> {
        if chunk_size < MIN_CHUNK_SIZE || chunk_size > MAX_CHUNK_SIZE {
            return Err(crate::TransferError::InvalidChunkSize { size: chunk_size });
        }
        Ok(Self { chunk_size })
    }

    /// The chunk size this plan was built with.
    #[must_use]
    pub const fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// The largest number of chunks a `total_len`-byte payload can produce.
    #[must_use]
    pub const fn chunk_count(&self, total_len: u64) -> u64 {
        // `div_ceil` is not const-stable on the MSRV this crate declares.
        if total_len == 0 {
            0
        } else {
            (total_len - 1) / self.chunk_size as u64 + 1
        }
    }

    /// Rounds `offset` down onto the chunk grid.
    #[must_use]
    pub const fn align_down(&self, offset: u64) -> u64 {
        offset - (offset % self.chunk_size as u64)
    }

    /// Reports whether `offset` lies on the chunk grid.
    #[must_use]
    pub const fn is_aligned(&self, offset: u64) -> bool {
        offset % self.chunk_size as u64 == 0
    }

    /// The chunk covering `offset`, or `None` when `offset >= total_len`.
    #[must_use]
    pub const fn range_at(&self, offset: u64, total_len: u64) -> Option<ChunkRange> {
        if offset >= total_len {
            return None;
        }
        let remaining = total_len - offset;
        let len = if remaining >= self.chunk_size as u64 {
            self.chunk_size
        } else {
            remaining as u32
        };
        Some(ChunkRange { offset, len })
    }

    /// The offset following `range`.
    #[must_use]
    pub const fn next_offset(&self, range: ChunkRange) -> u64 {
        range.offset + range.len as u64
    }
}

impl Default for ChunkPlan {
    fn default() -> Self {
        Self {
            chunk_size: CHUNK_SIZE,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn plan_rejects_sizes_outside_the_protocol_window() {
        assert!(ChunkPlan::new(0).is_err());
        assert!(ChunkPlan::new(MIN_CHUNK_SIZE - 1).is_err());
        assert!(ChunkPlan::new(MAX_CHUNK_SIZE + 1).is_err());
        assert!(ChunkPlan::new(MIN_CHUNK_SIZE).is_ok());
        assert!(ChunkPlan::new(MAX_CHUNK_SIZE).is_ok());
    }

    #[test]
    fn default_chunk_size_is_512_kib() {
        assert_eq!(CHUNK_SIZE, 512 * 1024);
        assert_eq!(ChunkPlan::default().chunk_size(), CHUNK_SIZE);
    }

    #[test]
    fn chunk_count_rounds_up_and_handles_empty_payloads() {
        let plan = ChunkPlan::new(1024).unwrap();
        assert_eq!(plan.chunk_count(0), 0);
        assert_eq!(plan.chunk_count(1), 1);
        assert_eq!(plan.chunk_count(1024), 1);
        assert_eq!(plan.chunk_count(1025), 2);
    }

    #[test]
    fn ranges_walk_the_payload_exactly_once() {
        let plan = ChunkPlan::new(1024).unwrap();
        let total = 1024 * 3 + 17;
        let mut offset = 0_u64;
        let mut chunks = 0_u64;
        while let Some(range) = plan.range_at(offset, total) {
            assert_eq!(range.offset, offset);
            assert!(range.len > 0);
            assert!(u64::from(range.len) <= u64::from(plan.chunk_size()));
            offset = plan.next_offset(range);
            chunks += 1;
        }
        assert_eq!(offset, total);
        assert_eq!(chunks, plan.chunk_count(total));
    }

    #[test]
    fn the_tail_chunk_is_partial() {
        let plan = ChunkPlan::new(1024).unwrap();
        let range = plan.range_at(2048, 2048 + 300).unwrap();
        assert_eq!(range.len, 300);
        assert!(plan.range_at(2048 + 300, 2048 + 300).is_none());
    }

    #[test]
    fn alignment_rounds_down_to_the_grid() {
        let plan = ChunkPlan::new(1024).unwrap();
        assert!(plan.is_aligned(0));
        assert!(plan.is_aligned(1024));
        assert!(!plan.is_aligned(1025));
        assert_eq!(plan.align_down(1025), 1024);
        assert_eq!(plan.align_down(1024), 1024);
        assert_eq!(plan.align_down(0), 0);
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        // FIPS 180-4 test vector for "abc".
        let digest = chunk_sha256(b"abc");
        assert_eq!(
            digest,
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
    }

    #[test]
    fn chunk_size_and_digest_lengths_agree_with_the_wire_format() {
        assert_eq!(CHUNK_SHA256_LEN, 32);
        assert_eq!(MAX_CHUNK_SIZE as usize, 524_288);
    }
}
