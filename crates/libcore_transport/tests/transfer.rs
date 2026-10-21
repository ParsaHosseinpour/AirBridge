//! End-to-end transport tests over real loopback TCP streams.
//!
//! These tests deliberately exercise the parts that unit tests cannot: a
//! stream dying mid-transfer, a corrupted chunk being repaired on the same
//! connection, and a resumed sender seeking past bytes the receiver already
//! verified. The scripted tests speak the wire format directly so that each
//! step is deterministic.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use libcore_transport::{
    chunk_sha256, connect, part_path, read_frame, receive_file, state_path_for, upload_file,
    write_hello, write_payload, Frame, FrameBuffer, Hello, PayloadHeader, PayloadId,
    ReceiveOptions, ReceiveReport, TransferError, UploadOptions, CHUNK_SIZE, MIN_CHUNK_SIZE,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;

/// Deterministic, non-repeating-in-small-windows payload bytes.
fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| {
            let value = index.wrapping_mul(37).wrapping_add(11);
            (value ^ (value >> 8)) as u8
        })
        .collect()
}

/// Creates a fresh, unique directory under the platform temp directory.
fn temp_dir(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "airbridge-transport-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// A stream wrapper that accepts `budget` bytes and then refuses to write.
///
/// Reads pass through, so the sender still sees the receiver's acks. The
/// budget intentionally cuts a frame mid-write on the next call, which is what
/// a dropped connection looks like from `write_all`'s perspective.
struct Cut<S> {
    inner: S,
    budget: u64,
    written: u64,
}

impl<S> Cut<S> {
    fn new(inner: S, budget: u64) -> Self {
        Self {
            inner,
            budget,
            written: 0,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Cut<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Cut<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.written >= this.budget {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "test connection cut",
            )));
        }
        let remaining = (this.budget - this.written) as usize;
        let cap = remaining.min(buf.len());
        match Pin::new(&mut this.inner).poll_write(cx, &buf[..cap]) {
            Poll::Ready(Ok(written)) => {
                this.written += written as u64;
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

/// Accepts one connection and receives into `destination`.
async fn receive_one(
    listener: TcpListener,
    destination: PathBuf,
    options: ReceiveOptions,
) -> Result<ReceiveReport, TransferError> {
    let (stream, _) = listener.accept().await.unwrap();
    receive_file(stream, &destination, &options).await
}

#[tokio::test]
async fn roundtrip_moves_multiple_chunks() {
    let directory = temp_dir("roundtrip");
    let source = directory.join("source.bin");
    let destination = directory.join("destination.bin");
    let len = 3 * CHUNK_SIZE as usize + CHUNK_SIZE as usize / 2 + 7;
    let data = pattern(len);
    std::fs::write(&source, &data).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = tokio::spawn(receive_one(
        listener,
        destination.clone(),
        ReceiveOptions::new(),
    ));

    let payload_id = PayloadId::random().unwrap();
    let stream = connect(address).await.unwrap();
    let sent = upload_file(stream, &source, &UploadOptions::new(payload_id))
        .await
        .unwrap();
    let received = receiver.await.unwrap().unwrap();

    assert_eq!(std::fs::read(&destination).unwrap(), data);
    assert_eq!(sent.payload_id, payload_id);
    assert_eq!(sent.total_len, len as u64);
    assert_eq!(sent.resumed_from, 0);
    assert_eq!(sent.bytes_sent, len as u64);
    assert_eq!(sent.chunks_sent, 4);
    assert_eq!(received.total_len, len as u64);
    assert_eq!(received.resumed_from, 0);
    assert_eq!(received.path, destination);
    assert!(!part_path(&destination).exists());
    assert!(!state_path_for(&part_path(&destination)).exists());
}

#[tokio::test]
async fn resume_seeks_past_chunks_the_receiver_verified() {
    let directory = temp_dir("resume");
    let source = directory.join("source.bin");
    let destination = directory.join("destination.bin");
    let chunk = u64::from(CHUNK_SIZE);
    let len = 3 * chunk + chunk / 2 + 5;
    let data = pattern(len as usize);
    std::fs::write(&source, &data).unwrap();

    let payload_id = PayloadId::random().unwrap();
    let upload_options = UploadOptions::new(payload_id);
    let receive_options = ReceiveOptions::new().with_ack_every_bytes(chunk);

    // Leg 1: allow HELLO plus two full chunk frames, then cut the stream. The
    // receiver persists two 512 KiB chunks before the connection dies.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = tokio::spawn(receive_one(listener, destination.clone(), receive_options));

    // 58-byte HELLO frame + two chunk frames (53 bytes of framing each).
    let budget = 58 + 2 * (chunk + 53);
    let stream = connect(address).await.unwrap();
    let cut = upload_file(Cut::new(stream, budget), &source, &upload_options).await;
    assert!(matches!(cut, Err(TransferError::Io(_))));
    assert!(receiver.await.unwrap().is_err());
    assert_eq!(
        std::fs::metadata(part_path(&destination)).unwrap().len(),
        2 * chunk
    );

    // Leg 2: same payload id, same destination, fresh connection. The sender
    // must skip the two verified chunks instead of retransmitting them.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = tokio::spawn(receive_one(listener, destination.clone(), receive_options));

    let stream = connect(address).await.unwrap();
    let sent = upload_file(stream, &source, &upload_options).await.unwrap();
    let received = receiver.await.unwrap().unwrap();

    assert_eq!(sent.resumed_from, 2 * chunk);
    assert_eq!(sent.bytes_sent, len - 2 * chunk);
    assert_eq!(sent.chunks_sent, 2);
    assert_eq!(received.resumed_from, 2 * chunk);
    assert_eq!(std::fs::read(&destination).unwrap(), data);
    assert!(!part_path(&destination).exists());
}

#[tokio::test]
async fn a_corrupt_chunk_is_repaired_on_the_same_connection() {
    let directory = temp_dir("repair");
    let destination = directory.join("destination.bin");
    let chunk = u64::from(MIN_CHUNK_SIZE);
    let len = 2 * chunk + 512;
    let data = pattern(len as usize);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = tokio::spawn(receive_one(
        listener,
        destination.clone(),
        ReceiveOptions::new().with_ack_every_bytes(len),
    ));

    let mut stream = connect(address).await.unwrap();
    let payload_id = PayloadId::random().unwrap();
    write_hello(
        &mut stream,
        &Hello {
            payload_id,
            total_len: len,
            chunk_size: MIN_CHUNK_SIZE,
            file_sha256: Some(chunk_sha256(&data)),
        },
    )
    .await
    .unwrap();

    let mut buffer = FrameBuffer::new();
    match read_frame(&mut stream, &mut buffer).await.unwrap() {
        Frame::AckOffset(ack) => {
            assert_eq!(ack.offset, 0);
            assert!(!ack.repair);
        }
        other => panic!("expected the opening ack, got {other:?}"),
    }

    // Chunk 0 arrives with bytes that do not match its announced digest. The
    // receiver must answer with a repair ack at its verified prefix and not
    // advance.
    let first = &data[..chunk as usize];
    let mut corrupted = first.to_vec();
    corrupted[0] ^= 0xff;
    write_payload(
        &mut stream,
        &PayloadHeader {
            payload_id,
            offset: 0,
            checksum: chunk_sha256(first),
        },
        &corrupted,
    )
    .await
    .unwrap();

    match read_frame(&mut stream, &mut buffer).await.unwrap() {
        Frame::AckOffset(ack) => {
            assert_eq!(ack.offset, 0);
            assert!(ack.repair);
        }
        other => panic!("expected a repair ack, got {other:?}"),
    }

    // Rewind and send everything correctly on the same stream.
    let mut offset = 0_usize;
    while offset < data.len() {
        let end = (offset + chunk as usize).min(data.len());
        let slice = &data[offset..end];
        write_payload(
            &mut stream,
            &PayloadHeader {
                payload_id,
                offset: offset as u64,
                checksum: chunk_sha256(slice),
            },
            slice,
        )
        .await
        .unwrap();
        offset = end;
    }

    // The last chunk crosses the acknowledgement threshold, so a progress ack
    // precedes COMPLETE.
    let mut saw_progress_ack = false;
    loop {
        match read_frame(&mut stream, &mut buffer).await.unwrap() {
            Frame::AckOffset(ack) => {
                assert_eq!(ack.offset, len);
                assert!(!ack.repair);
                saw_progress_ack = true;
            }
            Frame::Complete(complete) => {
                assert_eq!(complete.payload_id, payload_id);
                assert_eq!(complete.total_len, len);
                break;
            }
            other => panic!("expected an ack or COMPLETE, got {other:?}"),
        }
    }
    assert!(saw_progress_ack);

    let received = receiver.await.unwrap().unwrap();
    assert_eq!(received.resumed_from, 0);
    assert_eq!(std::fs::read(&destination).unwrap(), data);
}

#[tokio::test]
async fn tampered_part_file_forces_a_restart_instead_of_a_wrong_result() {
    let directory = temp_dir("tampered");
    let source = directory.join("source.bin");
    let destination = directory.join("destination.bin");
    let chunk = u64::from(CHUNK_SIZE);
    let len = 3 * chunk + 17;
    let data = pattern(len as usize);
    std::fs::write(&source, &data).unwrap();

    let payload_id = PayloadId::random().unwrap();
    let upload_options = UploadOptions::new(payload_id);
    let receive_options = ReceiveOptions::new().with_ack_every_bytes(chunk);

    // Leave two verified chunks on disk, as before.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = tokio::spawn(receive_one(listener, destination.clone(), receive_options));
    let budget = 58 + 2 * (chunk + 53);
    let stream = connect(address).await.unwrap();
    assert!(
        upload_file(Cut::new(stream, budget), &source, &upload_options)
            .await
            .is_err()
    );
    assert!(receiver.await.unwrap().is_err());

    // Corrupt the part file behind the sidecar's back. The prefix digest no
    // longer matches, so the receiver must discard the resume point.
    let part = part_path(&destination);
    let mut bytes = std::fs::read(&part).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&part, &bytes).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = tokio::spawn(receive_one(listener, destination.clone(), receive_options));
    let stream = connect(address).await.unwrap();
    let sent = upload_file(stream, &source, &upload_options).await.unwrap();
    let received = receiver.await.unwrap().unwrap();

    assert_eq!(sent.resumed_from, 0);
    assert_eq!(received.resumed_from, 0);
    assert_eq!(std::fs::read(&destination).unwrap(), data);
}

#[tokio::test]
async fn empty_payloads_commit_without_any_chunks() {
    let directory = temp_dir("empty");
    let source = directory.join("empty.bin");
    let destination = directory.join("empty-out.bin");
    std::fs::write(&source, b"").unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = tokio::spawn(receive_one(
        listener,
        destination.clone(),
        ReceiveOptions::new(),
    ));

    let stream = connect(address).await.unwrap();
    let sent = upload_file(
        stream,
        &source,
        &UploadOptions::new(PayloadId::random().unwrap()),
    )
    .await
    .unwrap();
    let received = receiver.await.unwrap().unwrap();

    assert_eq!(sent.total_len, 0);
    assert_eq!(sent.bytes_sent, 0);
    assert_eq!(sent.chunks_sent, 0);
    assert_eq!(received.total_len, 0);
    assert_eq!(std::fs::read(&destination).unwrap(), Vec::<u8>::new());
}

#[tokio::test]
async fn a_whole_file_digest_mismatch_is_never_committed() {
    let directory = temp_dir("digest");
    let destination = directory.join("destination.bin");
    let chunk = u64::from(MIN_CHUNK_SIZE);
    let len = 2 * chunk;
    let data = pattern(len as usize);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = tokio::spawn(receive_one(
        listener,
        destination.clone(),
        ReceiveOptions::new().with_ack_every_bytes(len),
    ));

    let mut stream = connect(address).await.unwrap();
    let payload_id = PayloadId::random().unwrap();
    write_hello(
        &mut stream,
        &Hello {
            payload_id,
            total_len: len,
            chunk_size: MIN_CHUNK_SIZE,
            // Deliberately wrong: the receiver must not rename the part file.
            file_sha256: Some([0_u8; 32]),
        },
    )
    .await
    .unwrap();

    let mut buffer = FrameBuffer::new();
    assert!(matches!(
        read_frame(&mut stream, &mut buffer).await.unwrap(),
        Frame::AckOffset(_)
    ));

    for offset in (0..data.len()).step_by(chunk as usize) {
        let end = (offset + chunk as usize).min(data.len());
        let slice = &data[offset..end];
        write_payload(
            &mut stream,
            &PayloadHeader {
                payload_id,
                offset: offset as u64,
                checksum: chunk_sha256(slice),
            },
            slice,
        )
        .await
        .unwrap();
    }

    // Keep the socket open until the receiver has finished writing its ack;
    // dropping it first would turn the final write into a connection reset on
    // some platforms and mask the digest failure.
    let error = receiver.await.unwrap().unwrap_err();
    drop(stream);
    assert!(
        matches!(error, TransferError::FileChecksumMismatch { .. }),
        "unexpected error: {error:?}"
    );
    assert!(!destination.exists());
    assert!(part_path(&destination).exists());
}
