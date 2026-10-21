//! 4-byte big-endian length-prefixed binary framing.
//!
//! # Frame layout
//!
//! ```text
//! +-----------+----------+-----------------------------------+
//! | len:u32BE | tag:u8   | body (len - 1 bytes)              |
//! +-----------+----------+-----------------------------------+
//! ```
//!
//! `len` counts the tag byte and the body, never the four length bytes
//! themselves. The reader refuses any frame whose body is empty, whose body
//! exceeds [`MAX_FRAME_BODY`], or whose body is not exactly as long as the tag
//! says. The length is therefore fully enforced *before* it drives an
//! allocation, which is the same discipline the crypto core applies to frame
//! payload lengths.
//!
//! # Bodies
//!
//! ```text
//! HELLO             flags:u8 | payload_id:u64 | total_len:u64 | chunk_size:u32 | file_sha256:32
//! PAYLOAD_TRANSFER  payload_id:u64 | offset:u64 | chunk_sha256:32 | data:0..=chunk_size
//! ACK_OFFSET        payload_id:u64 | offset:u64 | flags:u8
//! COMPLETE          payload_id:u64 | total_len:u64
//! ABORT             payload_id:u64 | code:u8
//! ```
//!
//! `flags` in `HELLO` bit 0 marks the presence of the whole-file SHA-256
//! digest; when the bit is clear the digest field is all zeros and ignored.
//! `flags` in `ACK_OFFSET` bit 0 requests a repair: the sender must rewind to
//! `offset` regardless of where it currently is. Without that bit an ack is a
//! *progress report*: the sender may skip forward past chunks the receiver has
//! already verified, but a lagging report must never drag it backwards.
//! Conflating the two is how naive resumable protocols end up retransmitting
//! multiple megabytes per lost ack.

use core::fmt;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::chunk::{CHUNK_SHA256_LEN, MAX_CHUNK_SIZE};
use crate::error::TransferError;

/// Length of the big-endian body-length prefix, in bytes.
pub const LENGTH_PREFIX_LEN: usize = 4;

/// Length of a `PAYLOAD_TRANSFER` body header, excluding the chunk bytes.
pub const PAYLOAD_HEADER_LEN: usize = 8 + 8 + CHUNK_SHA256_LEN;

/// Length of a `HELLO` body, in bytes.
pub const HELLO_BODY_LEN: usize = 1 + 8 + 8 + 4 + CHUNK_SHA256_LEN;

/// Length of an `ACK_OFFSET` body, in bytes.
pub const ACK_BODY_LEN: usize = 8 + 8 + 1;

/// Length of a `COMPLETE` body, in bytes.
pub const COMPLETE_BODY_LEN: usize = 8 + 8;

/// Length of an `ABORT` body, in bytes.
pub const ABORT_BODY_LEN: usize = 8 + 1;

/// Largest body the reader will accept: the tag plus a full payload chunk.
pub const MAX_FRAME_BODY: usize = 1 + PAYLOAD_HEADER_LEN + MAX_CHUNK_SIZE as usize;

/// Bit in `HELLO` flags marking a present whole-file digest.
const HELLO_FLAG_FILE_SHA256: u8 = 0x01;

/// Bit in `ACK_OFFSET` flags requesting a rewind.
const ACK_FLAG_REPAIR: u8 = 0x01;

/// Identifies one logical payload across connections and restarts.
///
/// The sender chooses it once (see [`PayloadId::random`]) and re-uses it for
/// every attempt to move the same bytes; that is what lets a receiver match a
/// fresh connection to its persisted part file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PayloadId(u64);

impl PayloadId {
    /// Wraps a raw identifier.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Generates a fresh identifier from the system random number generator.
    ///
    /// # Errors
    ///
    /// Returns [`TransferError::Random`] when the OS RNG is unavailable.
    pub fn random() -> Result<Self, TransferError> {
        use ring::rand::{SecureRandom as _, SystemRandom};
        let mut bytes = [0_u8; 8];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| TransferError::Random)?;
        Ok(Self(u64::from_be_bytes(bytes)))
    }
}

impl fmt::Display for PayloadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// The transfer header: everything the receiver needs to plan for the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// Identifier of the payload being offered.
    pub payload_id: PayloadId,
    /// Total size of the payload, in bytes.
    pub total_len: u64,
    /// Grid size every chunk offset is aligned to.
    pub chunk_size: u32,
    /// SHA-256 over the whole file, when the sender computed it.
    pub file_sha256: Option<[u8; CHUNK_SHA256_LEN]>,
}

/// A chunk payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadFrame<'a> {
    /// Chunk metadata.
    pub header: PayloadHeader,
    /// Chunk bytes; the borrow points into the reader's [`FrameBuffer`].
    pub data: &'a [u8],
}

/// Metadata carried by every chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadHeader {
    /// Identifier of the payload this chunk belongs to.
    pub payload_id: PayloadId,
    /// Byte offset of this chunk, on the chunk grid.
    pub offset: u64,
    /// SHA-256 over `data`.
    pub checksum: [u8; CHUNK_SHA256_LEN],
}

/// The receiver's statement of contiguous verified bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckOffset {
    /// Identifier of the payload being acknowledged.
    pub payload_id: PayloadId,
    /// Next byte the receiver needs, on the chunk grid.
    pub offset: u64,
    /// When set, the sender must rewind to `offset` even if it has moved on.
    pub repair: bool,
}

/// Terminal frame: the receiver has verified and committed the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Complete {
    /// Identifier of the completed payload.
    pub payload_id: PayloadId,
    /// Total size of the committed payload, in bytes.
    pub total_len: u64,
}

/// Why a side gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AbortCode {
    /// The peer violated the framing or sequencing rules.
    ProtocolViolation = 1,
    /// A digest check failed in a way the peer cannot repair.
    IntegrityCheckFailed = 2,
    /// Local storage failed (disk full, permissions).
    StorageFailure = 3,
    /// The peer does not support this transfer's parameters.
    Unsupported = 4,
}

impl AbortCode {
    /// Returns the on-the-wire byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Parses an on-the-wire byte.
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::ProtocolViolation),
            2 => Some(Self::IntegrityCheckFailed),
            3 => Some(Self::StorageFailure),
            4 => Some(Self::Unsupported),
            _ => None,
        }
    }
}

/// A non-terminal or terminal abort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Abort {
    /// Identifier of the payload being abandoned.
    pub payload_id: PayloadId,
    /// Machine-readable reason.
    pub code: AbortCode,
}

/// One decoded frame.
///
/// Payload frames borrow their bytes from the [`FrameBuffer`] that was passed
/// to [`read_frame`]; every other frame is small and owned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame<'a> {
    /// Transfer header; senders open with this.
    Hello(Hello),
    /// Chunk; senders stream these.
    Payload(PayloadFrame<'a>),
    /// Verification progress; receivers send these.
    AckOffset(AckOffset),
    /// Success; receivers send this last.
    Complete(Complete),
    /// Failure; either side may send this.
    Abort(Abort),
}

impl Frame<'_> {
    /// The payload id carried by any frame.
    #[must_use]
    pub const fn payload_id(&self) -> PayloadId {
        match self {
            Self::Hello(hello) => hello.payload_id,
            Self::Payload(payload) => payload.header.payload_id,
            Self::AckOffset(ack) => ack.payload_id,
            Self::Complete(complete) => complete.payload_id,
            Self::Abort(abort) => abort.payload_id,
        }
    }
}

/// Reusable read buffer for [`read_frame`].
///
/// One buffer per direction per connection is the whole memory budget of the
/// receive path: a payload frame's bytes are borrowed out of it, verified and
/// written to disk before the next frame is read, so the engine never holds
/// more than one chunk in memory.
#[derive(Debug)]
pub struct FrameBuffer {
    buf: Vec<u8>,
}

impl FrameBuffer {
    /// Creates an empty buffer; it grows to the largest frame it sees.
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Creates a buffer pre-sized for a `body_len`-byte body.
    #[must_use]
    pub fn with_capacity(body_len: usize) -> Self {
        Self {
            buf: Vec::with_capacity(body_len),
        }
    }

    /// Bytes currently allocated by the buffer.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    fn resize(&mut self, len: usize) {
        self.buf.resize(len, 0);
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

impl Default for FrameBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameTag {
    /// Parses an on-the-wire tag byte.
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Hello),
            2 => Some(Self::PayloadTransfer),
            3 => Some(Self::AckOffset),
            4 => Some(Self::Complete),
            5 => Some(Self::Abort),
            _ => None,
        }
    }
}

/// Discriminator byte of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameTag {
    /// [`Frame::Hello`].
    Hello = 1,
    /// [`Frame::Payload`].
    PayloadTransfer = 2,
    /// [`Frame::AckOffset`].
    AckOffset = 3,
    /// [`Frame::Complete`].
    Complete = 4,
    /// [`Frame::Abort`].
    Abort = 5,
}

/// Reads one frame, borrowing any payload bytes from `buffer`.
///
/// # Errors
///
/// * [`TransferError::Protocol`] for a zero-length, unknown-tagged or
///   wrong-sized body;
/// * [`TransferError::FrameTooLarge`] when the declared body exceeds
///   [`MAX_FRAME_BODY`];
/// * [`TransferError::PeerClosed`] when the stream ends mid-frame.
pub async fn read_frame<'b, R>(
    reader: &mut R,
    buffer: &'b mut FrameBuffer,
) -> Result<Frame<'b>, TransferError>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut prefix = [0_u8; LENGTH_PREFIX_LEN];
    reader.read_exact(&mut prefix).await.map_err(map_eof)?;
    let body_len = usize::try_from(u32::from_be_bytes(prefix))
        .map_err(|_| TransferError::Protocol("frame length does not fit in usize"))?;
    if body_len == 0 || body_len > MAX_FRAME_BODY {
        return Err(TransferError::FrameTooLarge {
            size: body_len,
            max: MAX_FRAME_BODY,
        });
    }

    buffer.resize(body_len);
    reader
        .read_exact(buffer.as_mut_slice())
        .await
        .map_err(map_eof)?;

    let body = buffer.as_slice();
    let tag = FrameTag::from_wire(body[0]).ok_or(TransferError::Protocol("unknown frame tag"))?;
    let mut cursor = Cursor::new(&body[1..]);

    let frame = match tag {
        FrameTag::Hello => Frame::Hello(cursor.hello()?),
        FrameTag::PayloadTransfer => {
            let header = PayloadHeader {
                payload_id: PayloadId(cursor.u64()?),
                offset: cursor.u64()?,
                checksum: cursor.digest()?,
            };
            let data = cursor.rest();
            if data.is_empty() {
                return Err(TransferError::Protocol(
                    "payload frame carries no chunk bytes",
                ));
            }
            Frame::Payload(PayloadFrame { header, data })
        }
        FrameTag::AckOffset => {
            let ack = AckOffset {
                payload_id: PayloadId(cursor.u64()?),
                offset: cursor.u64()?,
                repair: cursor.u8()? & ACK_FLAG_REPAIR != 0,
            };
            cursor.finish()?;
            Frame::AckOffset(ack)
        }
        FrameTag::Complete => {
            let complete = Complete {
                payload_id: PayloadId(cursor.u64()?),
                total_len: cursor.u64()?,
            };
            cursor.finish()?;
            Frame::Complete(complete)
        }
        FrameTag::Abort => {
            let abort = Abort {
                payload_id: PayloadId(cursor.u64()?),
                code: AbortCode::from_wire(cursor.u8()?)
                    .ok_or(TransferError::Protocol("unknown abort code"))?,
            };
            cursor.finish()?;
            Frame::Abort(abort)
        }
    };
    Ok(frame)
}

/// Writes a `HELLO` frame.
pub async fn write_hello<W>(writer: &mut W, hello: &Hello) -> Result<(), TransferError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut body = [0_u8; HELLO_BODY_LEN];
    if let Some(digest) = &hello.file_sha256 {
        body[0] = HELLO_FLAG_FILE_SHA256;
        body[1 + 8 + 8 + 4..].copy_from_slice(digest);
    }
    body[1..9].copy_from_slice(&hello.payload_id.get().to_be_bytes());
    body[9..17].copy_from_slice(&hello.total_len.to_be_bytes());
    body[17..21].copy_from_slice(&hello.chunk_size.to_be_bytes());
    write_tagged(writer, FrameTag::Hello, &body).await
}

/// Writes a `PAYLOAD_TRANSFER` frame: header and chunk bytes back to back.
///
/// The header is assembled in one fixed-size buffer and written in a single
/// call, so a chunk costs two socket writes regardless of chunk size.
pub async fn write_payload<W>(
    writer: &mut W,
    header: &PayloadHeader,
    data: &[u8],
) -> Result<(), TransferError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    if data.is_empty() {
        return Err(TransferError::Protocol("refusing to send an empty chunk"));
    }
    if data.len() > MAX_CHUNK_SIZE as usize {
        return Err(TransferError::FrameTooLarge {
            size: data.len(),
            max: MAX_CHUNK_SIZE as usize,
        });
    }
    let body_len = PAYLOAD_HEADER_LEN + data.len();
    let declared = u32::try_from(1 + body_len)
        .map_err(|_| TransferError::Protocol("frame body length overflow"))?;

    let mut prefix = [0_u8; LENGTH_PREFIX_LEN + 1 + PAYLOAD_HEADER_LEN];
    prefix[..LENGTH_PREFIX_LEN].copy_from_slice(&declared.to_be_bytes());
    prefix[LENGTH_PREFIX_LEN] = FrameTag::PayloadTransfer as u8;
    let body = &mut prefix[LENGTH_PREFIX_LEN + 1..];
    body[..8].copy_from_slice(&header.payload_id.get().to_be_bytes());
    body[8..16].copy_from_slice(&header.offset.to_be_bytes());
    body[16..].copy_from_slice(&header.checksum);

    writer.write_all(&prefix).await?;
    writer.write_all(data).await?;
    Ok(())
}

/// Writes an `ACK_OFFSET` frame.
pub async fn write_ack_offset<W>(writer: &mut W, ack: &AckOffset) -> Result<(), TransferError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut body = [0_u8; ACK_BODY_LEN];
    body[..8].copy_from_slice(&ack.payload_id.get().to_be_bytes());
    body[8..16].copy_from_slice(&ack.offset.to_be_bytes());
    body[16] = if ack.repair { ACK_FLAG_REPAIR } else { 0 };
    write_tagged(writer, FrameTag::AckOffset, &body).await
}

/// Writes a `COMPLETE` frame.
pub async fn write_complete<W>(writer: &mut W, complete: &Complete) -> Result<(), TransferError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut body = [0_u8; COMPLETE_BODY_LEN];
    body[..8].copy_from_slice(&complete.payload_id.get().to_be_bytes());
    body[8..].copy_from_slice(&complete.total_len.to_be_bytes());
    write_tagged(writer, FrameTag::Complete, &body).await
}

/// Writes an `ABORT` frame.
pub async fn write_abort<W>(writer: &mut W, abort: &Abort) -> Result<(), TransferError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut body = [0_u8; ABORT_BODY_LEN];
    body[..8].copy_from_slice(&abort.payload_id.get().to_be_bytes());
    body[8] = abort.code.as_u8();
    write_tagged(writer, FrameTag::Abort, &body).await
}

/// Writes `tag` and `body` behind a big-endian length prefix.
async fn write_tagged<W>(writer: &mut W, tag: FrameTag, body: &[u8]) -> Result<(), TransferError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let declared = u32::try_from(1 + body.len())
        .map_err(|_| TransferError::Protocol("frame body length overflow"))?;
    let mut prefix = [0_u8; LENGTH_PREFIX_LEN + 1];
    prefix[..LENGTH_PREFIX_LEN].copy_from_slice(&declared.to_be_bytes());
    prefix[LENGTH_PREFIX_LEN] = tag as u8;
    writer.write_all(&prefix).await?;
    writer.write_all(body).await?;
    Ok(())
}

/// Maps a clean end-of-stream onto [`TransferError::PeerClosed`].
fn map_eof(error: std::io::Error) -> TransferError {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        TransferError::PeerClosed
    } else {
        TransferError::Io(error)
    }
}

/// Bounds-checked reader over a frame body.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], TransferError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(TransferError::Protocol("frame body length overflow"))?;
        if end > self.buf.len() {
            return Err(TransferError::Protocol(
                "frame body is shorter than its tag requires",
            ));
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, TransferError> {
        Ok(self.take(1)?[0])
    }

    fn u64(&mut self) -> Result<u64, TransferError> {
        let bytes = self.take(8)?;
        let mut array = [0_u8; 8];
        array.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(array))
    }

    fn u32(&mut self) -> Result<u32, TransferError> {
        let bytes = self.take(4)?;
        let mut array = [0_u8; 4];
        array.copy_from_slice(bytes);
        Ok(u32::from_be_bytes(array))
    }

    fn digest(&mut self) -> Result<[u8; CHUNK_SHA256_LEN], TransferError> {
        let bytes = self.take(CHUNK_SHA256_LEN)?;
        let mut array = [0_u8; CHUNK_SHA256_LEN];
        array.copy_from_slice(bytes);
        Ok(array)
    }

    fn hello(&mut self) -> Result<Hello, TransferError> {
        let flags = self.u8()?;
        let payload_id = PayloadId(self.u64()?);
        let total_len = self.u64()?;
        let chunk_size = self.u32()?;
        let digest = self.digest()?;
        self.finish()?;
        let file_sha256 = if flags & HELLO_FLAG_FILE_SHA256 != 0 {
            Some(digest)
        } else {
            None
        };
        Ok(Hello {
            payload_id,
            total_len,
            chunk_size,
            file_sha256,
        })
    }

    fn rest(&mut self) -> &'a [u8] {
        let rest = &self.buf[self.pos..];
        self.pos = self.buf.len();
        rest
    }

    fn finish(&self) -> Result<(), TransferError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(TransferError::Protocol("frame body has trailing bytes"))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::chunk::chunk_sha256;

    #[tokio::test]
    async fn hello_roundtrips_with_and_without_a_digest() {
        let with = Hello {
            payload_id: PayloadId::new(7),
            total_len: 1234,
            chunk_size: 4096,
            file_sha256: Some([0xAB; 32]),
        };
        let mut bytes = Vec::new();
        write_hello(&mut bytes, &with).await.unwrap();
        let mut slice = bytes.as_slice();
        let mut buffer = FrameBuffer::new();
        match read_frame(&mut slice, &mut buffer).await.unwrap() {
            Frame::Hello(hello) => assert_eq!(hello, with),
            other => panic!("expected hello, got {other:?}"),
        }

        let without = Hello {
            file_sha256: None,
            ..with
        };
        let mut bytes = Vec::new();
        write_hello(&mut bytes, &without).await.unwrap();
        let mut slice = bytes.as_slice();
        let mut buffer = FrameBuffer::new();
        match read_frame(&mut slice, &mut buffer).await.unwrap() {
            Frame::Hello(hello) => assert_eq!(hello, without),
            other => panic!("expected hello, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn payload_roundtrips_byte_for_byte() {
        let data = vec![0x5A_u8; 4096];
        let header = PayloadHeader {
            payload_id: PayloadId::new(1),
            offset: 8192,
            checksum: chunk_sha256(&data),
        };
        let mut bytes = Vec::new();
        write_payload(&mut bytes, &header, &data).await.unwrap();
        assert_eq!(
            bytes.len(),
            LENGTH_PREFIX_LEN + 1 + PAYLOAD_HEADER_LEN + data.len()
        );
        let mut slice = bytes.as_slice();
        let mut buffer = FrameBuffer::new();
        match read_frame(&mut slice, &mut buffer).await.unwrap() {
            Frame::Payload(payload) => {
                assert_eq!(payload.header, header);
                assert_eq!(payload.data, data.as_slice());
            }
            other => panic!("expected payload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn small_frames_roundtrip() {
        let ack = AckOffset {
            payload_id: PayloadId::new(3),
            offset: 4096,
            repair: true,
        };
        let mut bytes = Vec::new();
        write_ack_offset(&mut bytes, &ack).await.unwrap();
        let mut slice = bytes.as_slice();
        let mut buffer = FrameBuffer::new();
        assert_eq!(
            read_frame(&mut slice, &mut buffer).await.unwrap(),
            Frame::AckOffset(ack)
        );

        let complete = Complete {
            payload_id: PayloadId::new(3),
            total_len: 999,
        };
        let mut bytes = Vec::new();
        write_complete(&mut bytes, &complete).await.unwrap();
        let mut slice = bytes.as_slice();
        assert_eq!(
            read_frame(&mut slice, &mut buffer).await.unwrap(),
            Frame::Complete(complete)
        );

        let abort = Abort {
            payload_id: PayloadId::new(3),
            code: AbortCode::StorageFailure,
        };
        let mut bytes = Vec::new();
        write_abort(&mut bytes, &abort).await.unwrap();
        let mut slice = bytes.as_slice();
        assert_eq!(
            read_frame(&mut slice, &mut buffer).await.unwrap(),
            Frame::Abort(abort)
        );
    }

    #[tokio::test]
    async fn repair_flag_is_not_confused_with_progress_acks() {
        for repair in [false, true] {
            let ack = AckOffset {
                payload_id: PayloadId::new(9),
                offset: 1024,
                repair,
            };
            let mut bytes = Vec::new();
            write_ack_offset(&mut bytes, &ack).await.unwrap();
            let mut slice = bytes.as_slice();
            let mut buffer = FrameBuffer::new();
            match read_frame(&mut slice, &mut buffer).await.unwrap() {
                Frame::AckOffset(decoded) => assert_eq!(decoded.repair, repair),
                other => panic!("expected ack, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn zero_length_frame_is_rejected() {
        let bytes = [0_u8; 4];
        let mut slice = bytes.as_slice();
        let mut buffer = FrameBuffer::new();
        assert!(matches!(
            read_frame(&mut slice, &mut buffer).await,
            Err(TransferError::FrameTooLarge { size: 0, .. })
        ));
    }

    #[tokio::test]
    async fn oversized_length_prefix_is_rejected_before_allocation() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());
        let mut slice = bytes.as_slice();
        let mut buffer = FrameBuffer::new();
        assert!(matches!(
            read_frame(&mut slice, &mut buffer).await,
            Err(TransferError::FrameTooLarge { max, .. }) if max == MAX_FRAME_BODY
        ));
        assert_eq!(
            buffer.capacity(),
            0,
            "nothing may be allocated for a rejected frame"
        );
    }

    #[tokio::test]
    async fn unknown_tag_is_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.push(0x7F);
        let mut slice = bytes.as_slice();
        let mut buffer = FrameBuffer::new();
        assert!(matches!(
            read_frame(&mut slice, &mut buffer).await,
            Err(TransferError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn truncated_bodies_are_rejected() {
        // Length prefix promises a full ABORT body, stream ends early.
        let full = {
            let mut bytes = Vec::new();
            write_abort(
                &mut bytes,
                &Abort {
                    payload_id: PayloadId::new(1),
                    code: AbortCode::ProtocolViolation,
                },
            )
            .await
            .unwrap();
            bytes
        };
        for cut in 1..full.len() {
            let mut slice = &full[..cut];
            let mut buffer = FrameBuffer::new();
            assert!(
                read_frame(&mut slice, &mut buffer).await.is_err(),
                "accepted a frame truncated to {cut} bytes"
            );
        }
    }

    #[tokio::test]
    async fn wrong_sized_bodies_are_rejected() {
        // An ACK body with trailing garbage must not be accepted.
        let mut bytes = Vec::new();
        write_ack_offset(
            &mut bytes,
            &AckOffset {
                payload_id: PayloadId::new(1),
                offset: 0,
                repair: false,
            },
        )
        .await
        .unwrap();
        let declared = u32::try_from(bytes.len() - LENGTH_PREFIX_LEN).unwrap();
        bytes[..4].copy_from_slice(&(declared + 1).to_be_bytes());
        bytes.push(0);

        let mut slice = bytes.as_slice();
        let mut buffer = FrameBuffer::new();
        assert!(matches!(
            read_frame(&mut slice, &mut buffer).await,
            Err(TransferError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn empty_payload_chunk_is_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(1 + PAYLOAD_HEADER_LEN as u32).to_be_bytes());
        bytes.push(FrameTag::PayloadTransfer as u8);
        bytes.extend_from_slice(&[0_u8; PAYLOAD_HEADER_LEN]);
        let mut slice = bytes.as_slice();
        let mut buffer = FrameBuffer::new();
        assert!(matches!(
            read_frame(&mut slice, &mut buffer).await,
            Err(TransferError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn payload_larger_than_max_chunk_is_rejected_on_write() {
        let data = vec![0_u8; MAX_CHUNK_SIZE as usize + 1];
        let mut out = Vec::new();
        let header = PayloadHeader {
            payload_id: PayloadId::new(1),
            offset: 0,
            checksum: [0; 32],
        };
        assert!(matches!(
            write_payload(&mut out, &header, &data).await,
            Err(TransferError::FrameTooLarge { .. })
        ));
        assert!(
            out.is_empty(),
            "a rejected write must not emit a partial frame"
        );
    }

    #[test]
    fn payload_id_renders_as_fixed_width_hex() {
        assert_eq!(PayloadId::new(0x1234).to_string(), "0000000000001234");
    }
}
