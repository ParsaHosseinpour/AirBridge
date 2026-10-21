//! AirBridge file transport.
//!
//! A high-throughput, memory-efficient engine that moves one file across one
//! reliable byte stream. It is deliberately the *stream* layer: framing,
//! chunking, per-chunk integrity and resumption. The `secure` module wraps
//! that stream in the `libcore_crypto` handshake and AES-256-GCM frames before
//! `HELLO` is written, and discovery lives below it in `libcore_discovery`.
//!
//! # Wire format
//!
//! Every frame is a 4-byte big-endian body length, a one-byte tag, and the
//! body:
//!
//! ```text
//! +-----------+----------+-----------------------------------------------+
//! | len:u32BE | tag:u8   | body (len - 1 bytes)                          |
//! +-----------+----------+-----------------------------------------------+
//!
//! HELLO             flags:u8 | payload_id:u64 | total_len:u64 | chunk_size:u32 | file_sha256:32
//! PAYLOAD_TRANSFER  payload_id:u64 | offset:u64 | chunk_sha256:32 | data:0..=512 KiB
//! ACK_OFFSET        payload_id:u64 | offset:u64 | flags:u8
//! COMPLETE          payload_id:u64 | total_len:u64
//! ABORT             payload_id:u64 | code:u8
//! ```
//!
//! The reader validates the declared length against
//! [`MAX_FRAME_BODY`] *before* the length drives an allocation, and validates
//! every body against its tag's exact size afterwards. A `PAYLOAD_TRANSFER`
//! costs 53 bytes of framing around 512 KiB of data: 0.01% overhead.
//!
//! # Chunking
//!
//! Payloads are sliced into 512 KiB blocks ([`CHUNK_SIZE`]) that always start
//! on a multiple of the chunk size. Every chunk carries a 32-byte SHA-256
//! digest of its own bytes. Because offsets are grid-aligned and a verified
//! prefix therefore ends on a grid line, resumption never has to deal with
//! partially verified chunks.
//!
//! # Resumption
//!
//! The receiver persists `{payload id, size, verified offset, prefix digest}`
//! next to its part file and reports its offset in the opening
//! `ACK_OFFSET`. The sender seeks its file descriptor to that offset and
//! streams from there — no chunk that verified on a previous attempt crosses
//! the wire twice. During the stream the receiver acks every
//! `ack_every_bytes` of verified data, and answers an unverifiable chunk with
//! a *repair* ack that rewinds the sender to its verified prefix. Progress
//! acks only ever move the sender forward; repair acks always rewind it.
//!
//! # Trust model
//!
//! The framing layer authenticates nothing by itself; per-chunk digests catch
//! corruption and bugs, not attackers. The whole-file digest in `HELLO`,
//! checked before the part file is renamed into place, makes the *result*
//! end-to-end verified against the sender's bytes. The part file and sidecar
//! are treated as untrusted hints: the sidecar's prefix digest is recomputed
//! from the part file before its offset is honoured, so tampering can force a
//! restart but never a wrong result.
//!
//! # Quick start
//!
//! ```no_run
//! use libcore_transport::{connect, receive_file, upload_file, PayloadId, ReceiveOptions, UploadOptions};
//!
//! # async fn example() -> Result<(), libcore_transport::TransferError> {
//! let payload_id = PayloadId::random()?;
//!
//! // Sender
//! let stream = connect("192.168.1.20:42517".parse().unwrap()).await?;
//! let report = upload_file(
//!     stream,
//!     std::path::Path::new("holiday.mp4"),
//!     &UploadOptions::new(payload_id),
//! )
//! .await?;
//!
//! // Receiver
//! # let stream = connect("127.0.0.1:1".parse().unwrap()).await?;
//! receive_file(
//!     stream,
//!     std::path::Path::new("holiday.mp4"),
//!     &ReceiveOptions::new().with_expected_payload_id(payload_id),
//! )
//! .await?;
//! assert_eq!(report.total_len, 0);
//! # Ok(())
//! # }
//! ```
//!
//! # Memory
//!
//! The sender holds one chunk-sized scratch buffer; the receiver borrows each
//! chunk out of its frame buffer, verifies it in place and writes it to disk
//! before reading the next one. Neither side's footprint grows with file size,
//! and the steady-state cost per transfer is roughly `chunk_size` plus a few
//! kilobytes of control state.

mod chunk;
mod error;
mod frame;
mod receiver;
mod secure;
mod sender;
mod state;

pub use chunk::{
    chunk_sha256, ChunkDigest, ChunkPlan, ChunkRange, CHUNK_SHA256_LEN, CHUNK_SIZE, MAX_CHUNK_SIZE,
    MIN_CHUNK_SIZE,
};
pub use error::TransferError;
pub use frame::{
    read_frame, write_abort, write_ack_offset, write_complete, write_hello, write_payload, Abort,
    AbortCode, AckOffset, Complete, Frame, FrameBuffer, FrameTag, Hello, PayloadFrame,
    PayloadHeader, PayloadId, ABORT_BODY_LEN, ACK_BODY_LEN, COMPLETE_BODY_LEN, HELLO_BODY_LEN,
    LENGTH_PREFIX_LEN, MAX_FRAME_BODY, PAYLOAD_HEADER_LEN,
};
pub use receiver::{
    receive_file, receive_file_mut, receive_file_mut_with_progress, ReceiveOptions, ReceiveReport,
};
pub use secure::{connect_secure, initiate, respond, PendingConnection, SecureStream};
pub use sender::{upload_file, UploadOptions, UploadReport};
pub use state::{part_path, state_path_for, ResumeState, STATE_LEN, STATE_MAGIC, STATE_VERSION};

use std::net::SocketAddr;

use tokio::net::TcpStream;

/// Opens a TCP connection tuned for bulk transfer.
///
/// `TCP_NODELAY` is enabled because the engine already frames in megabyte
/// batches: Nagle would only add latency to the small `HELLO` and
/// `ACK_OFFSET` frames without coalescing anything meaningful on the data
/// path.
///
/// # Errors
///
/// Propagates connection failures as [`TransferError::Io`].
pub async fn connect(address: SocketAddr) -> Result<TcpStream, TransferError> {
    let stream = TcpStream::connect(address).await?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}
