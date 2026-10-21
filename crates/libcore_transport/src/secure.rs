//! Handshake and AES-256-GCM channel setup for a transport stream.
//!
//! The file engine in [`crate::sender`] and [`crate::receiver`] deliberately
//! knows nothing about cryptography: it reads and writes a reliable byte
//! stream. This module supplies that stream *after* the AirBridge handshake,
//! so `HELLO` and every following frame travel inside authenticated frames.
//!
//! # Before any application data
//!
//! [`initiate`] (the TCP connector) and [`respond`] (the TCP acceptor) run the
//! complete four-message UKEY2-derived exchange. The resulting
//! [`PendingConnection`] shows the Short Authentication String and the peer's
//! stable identity fingerprint. No key material is released until the caller
//! confirms, mirroring [`libcore_crypto::PendingSession`].
//!
//! # Stream semantics
//!
//! A confirmed [`SecureStream`] behaves like any other `AsyncRead` +
//! `AsyncWrite` stream. Writes are sealed into AES-256-GCM frames, reads are
//! authenticated and decrypted, and the underlying stream's byte order is
//! preserved. The existing 4-byte transport framing therefore needs no
//! changes: it simply travels inside the encrypted channel.
//!
//! Each `poll_write` becomes at most one crypto frame. The transport writes
//! chunk headers and chunk bodies separately, so the overhead is a 30-byte
//! frame header/tag per write, not per payload chunk.
//!
//! # Identity and trust
//!
//! Both sides present a long-lived ECDSA P-256 identity and require one from
//! the peer. The handshake transcript includes those keys, and each `Finished`
//! message carries a signature over the transcript and the signer's ephemeral
//! key. Trust can therefore be persisted as the SHA-256 fingerprint returned by
//! [`PendingConnection::peer_fingerprint`], and a later connection can check
//! that fingerprint before releasing the stream.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use libcore_crypto::{
    CryptoError, EstablishedSession, Identity, Initiator, PendingSession, Responder,
    ResponderConfig, Sas, SasDigits, AEAD_TAG_LEN, FRAME_HEADER_LEN, MAX_FRAME_PAYLOAD,
    NEXT_PROTOCOL_AEAD,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

use crate::error::TransferError;

/// Largest handshake message this layer will accept.
///
/// The crypto core bounds every message it produces to 1024 bytes; the
/// transport frame adds no fields, so the same limit is enforced before any
/// allocation.
const MAX_HANDSHAKE_MESSAGE: usize = 1024;

/// Header size of a length-prefixed handshake message.
const HANDSHAKE_LENGTH_LEN: usize = 4;

/// A completed handshake whose stream is not released until the caller
/// confirms the displayed SAS.
///
/// This type exists so that neither the sender nor the receiver can start
/// writing `HELLO` before the operator (or a persisted fingerprint) has made a
/// trust decision.
pub struct PendingConnection<S> {
    stream: Option<S>,
    pending: Option<PendingSession>,
    sas: Sas,
    peer_fingerprint: Option<[u8; libcore_crypto::FINGERPRINT_LEN]>,
}

impl<S> PendingConnection<S> {
    fn new(stream: S, pending: PendingSession) -> Result<Self, TransferError> {
        let sas = pending.sas(SasDigits::Six).map_err(TransferError::Crypto)?;
        let peer_fingerprint = pending.peer_fingerprint();
        Ok(Self {
            stream: Some(stream),
            pending: Some(pending),
            sas,
            peer_fingerprint,
        })
    }

    /// The six-digit code both parties must compare.
    #[must_use]
    pub const fn sas(&self) -> Sas {
        self.sas
    }

    /// The peer's stable identity fingerprint, when the peer presented one.
    #[must_use]
    pub const fn peer_fingerprint(&self) -> Option<[u8; libcore_crypto::FINGERPRINT_LEN]> {
        self.peer_fingerprint
    }

    /// The peer's long-term identity public key, when one was presented.
    #[must_use]
    pub fn peer_identity_public_key(&self) -> Option<&[u8]> {
        self.pending
            .as_ref()
            .and_then(PendingSession::peer_identity_public_key)
    }

    /// Confirms the SAS shown by `sas()` and releases the encrypted stream.
    ///
    /// The caller must only call this after the operator has compared the code
    /// with the peer, or after [`Self::peer_fingerprint`] matched a previously
    /// trusted fingerprint.
    ///
    /// # Errors
    ///
    /// Returns a crypto or I/O error if the frame cipher cannot be created.
    pub fn confirm(self) -> Result<SecureStream<S>, TransferError> {
        let pending = self
            .pending
            .ok_or(TransferError::Protocol("handshake was already confirmed"))?;
        let session = pending
            .confirm(self.sas.digits())
            .map_err(TransferError::Crypto)?;
        let stream = self.stream.ok_or(TransferError::Protocol(
            "handshake stream was already consumed",
        ))?;
        Ok(SecureStream::new(stream, session))
    }

    /// Confirms that the peer's displayed code is `peer_sas`.
    ///
    /// # Errors
    ///
    /// Returns [`libcore_crypto::CryptoError::SasMismatch`] wrapped in
    /// [`TransferError::Crypto`] when the codes differ.
    pub fn confirm_with(self, peer_sas: &Sas) -> Result<SecureStream<S>, TransferError> {
        let pending = self
            .pending
            .ok_or(TransferError::Protocol("handshake was already confirmed"))?;
        let session = pending
            .confirm_with(peer_sas)
            .map_err(TransferError::Crypto)?;
        let stream = self.stream.ok_or(TransferError::Protocol(
            "handshake stream was already consumed",
        ))?;
        Ok(SecureStream::new(stream, session))
    }

    /// Drops the connection without releasing a usable channel.
    pub fn reject(self) {
        drop(self.stream);
        drop(self.pending);
    }
}

/// Connects to `address` and drives the initiator side of the handshake.
///
/// # Errors
///
/// Propagates connection, framing, crypto and deadline failures.
pub async fn connect_secure(
    address: std::net::SocketAddr,
    identity: Arc<Identity>,
    timeout: Duration,
) -> Result<PendingConnection<TcpStream>, TransferError> {
    let stream = crate::connect(address).await?;
    initiate(stream, identity, timeout).await
}

/// Drives the initiator side of the handshake over an already-open stream.
///
/// # Errors
///
/// Propagates framing, crypto and deadline failures. The peer's identity is
/// required: an anonymous peer cannot be bound to a persistent fingerprint.
pub async fn initiate<S>(
    stream: S,
    identity: Arc<Identity>,
    timeout: Duration,
) -> Result<PendingConnection<S>, TransferError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut stream = stream;
    let pending = tokio::time::timeout(timeout, async {
        let mut initiator = Initiator::with_identity(NEXT_PROTOCOL_AEAD, identity)
            .map_err(TransferError::Crypto)?;

        let m1 = initiator.client_init().map_err(TransferError::Crypto)?;
        write_handshake(&mut stream, &m1).await?;

        let m2 = read_handshake(&mut stream).await?;
        initiator
            .accept_server_init(&m2)
            .map_err(TransferError::Crypto)?;

        let m3 = initiator.client_finished().map_err(TransferError::Crypto)?;
        write_handshake(&mut stream, &m3).await?;

        let m4 = read_handshake(&mut stream).await?;
        initiator
            .accept_server_finished(&m4)
            .map_err(TransferError::Crypto)
    })
    .await
    .map_err(|_| TransferError::Timeout("waiting for the peer handshake"))??;

    PendingConnection::new(stream, pending)
}

/// Drives the responder side of the handshake over an accepted stream.
///
/// # Errors
///
/// Propagates framing, crypto and deadline failures. The initiator's identity
/// is required.
pub async fn respond<S>(
    stream: S,
    identity: Arc<Identity>,
    timeout: Duration,
) -> Result<PendingConnection<S>, TransferError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut stream = stream;
    let pending = tokio::time::timeout(timeout, async {
        let mut responder = Responder::new(ResponderConfig::default().with_identity(identity))
            .map_err(TransferError::Crypto)?;

        let m1 = read_handshake(&mut stream).await?;
        responder
            .accept_client_init(&m1)
            .map_err(TransferError::Crypto)?;

        let m2 = responder.server_init().map_err(TransferError::Crypto)?;
        write_handshake(&mut stream, &m2).await?;

        let m3 = read_handshake(&mut stream).await?;
        responder
            .accept_client_finished(&m3)
            .map_err(TransferError::Crypto)?;

        let (m4, pending) = responder.finish().map_err(TransferError::Crypto)?;
        write_handshake(&mut stream, &m4).await?;
        Ok::<_, TransferError>(pending)
    })
    .await
    .map_err(|_| TransferError::Timeout("waiting for the peer handshake"))??;

    PendingConnection::new(stream, pending)
}

/// Writes a length-prefixed handshake message.
async fn write_handshake<W>(writer: &mut W, message: &[u8]) -> Result<(), TransferError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let length = u32::try_from(message.len())
        .map_err(|_| TransferError::Protocol("handshake message is too long"))?;
    if message.is_empty() || message.len() > MAX_HANDSHAKE_MESSAGE {
        return Err(TransferError::Protocol(
            "handshake message length is out of range",
        ));
    }
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(message).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one length-prefixed handshake message, bounded before allocation.
async fn read_handshake<R>(reader: &mut R) -> Result<Vec<u8>, TransferError>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut length = [0_u8; HANDSHAKE_LENGTH_LEN];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(TransferError::PeerClosed)
        }
        Err(error) => return Err(error.into()),
    }

    let length = u32::from_be_bytes(length);
    let length = usize::try_from(length)
        .map_err(|_| TransferError::Protocol("handshake length does not fit in usize"))?;
    if length == 0 || length > MAX_HANDSHAKE_MESSAGE {
        return Err(TransferError::Protocol(
            "handshake message length is out of range",
        ));
    }

    let mut message = vec![0_u8; length];
    reader.read_exact(&mut message).await?;
    Ok(message)
}

/// An authenticated, replay-protected byte stream.
///
/// The wrapper preserves the underlying stream's byte order. Every write is
/// sealed as one crypto frame; reads accumulate one frame at a time and expose
/// its plaintext through the normal `AsyncRead` contract.
pub struct SecureStream<S> {
    inner: S,
    session: EstablishedSession,

    read_header: [u8; FRAME_HEADER_LEN],
    read_header_filled: usize,
    read_frame: Vec<u8>,
    read_frame_filled: usize,

    plaintext: Vec<u8>,
    plaintext_pos: usize,

    write_frame: Vec<u8>,
    write_frame_pos: usize,
    write_plain_len: usize,
}

impl<S> SecureStream<S> {
    fn new(inner: S, session: EstablishedSession) -> Self {
        Self {
            inner,
            session,
            read_header: [0_u8; FRAME_HEADER_LEN],
            read_header_filled: 0,
            read_frame: Vec::new(),
            read_frame_filled: 0,
            plaintext: Vec::new(),
            plaintext_pos: 0,
            write_frame: Vec::new(),
            write_frame_pos: 0,
            write_plain_len: 0,
        }
    }

    /// The public session identifier bound into every frame.
    #[must_use]
    pub fn session_id(&self) -> &[u8; 32] {
        self.session.session_id()
    }

    /// The peer identity fingerprint recorded by the handshake.
    #[must_use]
    pub fn peer_fingerprint(&self) -> Option<[u8; libcore_crypto::FINGERPRINT_LEN]> {
        self.session.peer_fingerprint()
    }

    /// The underlying stream, once the caller no longer needs the channel.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.inner
    }

    fn has_pending_write(&self) -> bool {
        self.write_frame_pos < self.write_frame.len()
    }
}

impl<S> AsyncRead for SecureStream<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        loop {
            if this.plaintext_pos < this.plaintext.len() {
                let available = &this.plaintext[this.plaintext_pos..];
                let count = available.len().min(buf.remaining());
                buf.put_slice(&available[..count]);
                this.plaintext_pos += count;
                if this.plaintext_pos == this.plaintext.len() {
                    this.plaintext.clear();
                    this.plaintext_pos = 0;
                }
                return Poll::Ready(Ok(()));
            }

            if this.read_header_filled < FRAME_HEADER_LEN {
                let mut header =
                    ReadBuf::new(&mut this.read_header[this.read_header_filled..FRAME_HEADER_LEN]);
                match Pin::new(&mut this.inner).poll_read(cx, &mut header) {
                    Poll::Ready(Ok(())) => {
                        let filled = header.filled().len();
                        if filled == 0 {
                            if this.read_header_filled == 0 {
                                return Poll::Ready(Ok(()));
                            }
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "connection ended inside an encrypted frame header",
                            )));
                        }
                        this.read_header_filled += filled;
                        if this.read_header_filled < FRAME_HEADER_LEN {
                            continue;
                        }

                        let payload_len = u32::from_be_bytes([
                            this.read_header[10],
                            this.read_header[11],
                            this.read_header[12],
                            this.read_header[13],
                        ]);
                        let payload_len = usize::try_from(payload_len).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "encrypted frame length does not fit in usize",
                            )
                        })?;
                        if payload_len > MAX_FRAME_PAYLOAD {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "encrypted frame exceeds the maximum payload size",
                            )));
                        }
                        let total = FRAME_HEADER_LEN
                            .checked_add(payload_len)
                            .and_then(|value| value.checked_add(AEAD_TAG_LEN))
                            .ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "encrypted frame length overflows",
                                )
                            })?;
                        this.read_frame.clear();
                        this.read_frame.resize(total, 0);
                        this.read_frame[..FRAME_HEADER_LEN].copy_from_slice(&this.read_header);
                        this.read_frame_filled = FRAME_HEADER_LEN;
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => return Poll::Pending,
                }
            }

            if this.read_frame_filled < this.read_frame.len() {
                let mut body = ReadBuf::new(&mut this.read_frame[this.read_frame_filled..]);
                match Pin::new(&mut this.inner).poll_read(cx, &mut body) {
                    Poll::Ready(Ok(())) => {
                        let filled = body.filled().len();
                        if filled == 0 {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "connection ended inside an encrypted frame",
                            )));
                        }
                        this.read_frame_filled += filled;
                        if this.read_frame_filled < this.read_frame.len() {
                            continue;
                        }
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => return Poll::Pending,
                }
            }

            if this.read_frame_filled == this.read_frame.len() && !this.read_frame.is_empty() {
                let opened = this
                    .session
                    .open(&this.read_frame)
                    .map_err(crypto_read_error)?;
                this.read_header_filled = 0;
                this.read_frame.clear();
                this.read_frame_filled = 0;
                this.plaintext = opened;
                this.plaintext_pos = 0;
                continue;
            }
        }
    }
}

impl<S> AsyncWrite for SecureStream<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if !this.has_pending_write() {
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let plain_len = buf.len().min(MAX_FRAME_PAYLOAD);
            this.write_frame = this
                .session
                .seal(&buf[..plain_len])
                .map_err(crypto_write_error)?;
            this.write_frame_pos = 0;
            this.write_plain_len = plain_len;
        }

        loop {
            let remaining = &this.write_frame[this.write_frame_pos..];
            match Pin::new(&mut this.inner).poll_write(cx, remaining) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "the underlying stream accepted no encrypted bytes",
                    )))
                }
                Poll::Ready(Ok(written)) => {
                    this.write_frame_pos += written;
                    if this.write_frame_pos == this.write_frame.len() {
                        this.write_frame.clear();
                        this.write_frame_pos = 0;
                        return Poll::Ready(Ok(this.write_plain_len));
                    }
                }
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        while this.has_pending_write() {
            let remaining = &this.write_frame[this.write_frame_pos..];
            match Pin::new(&mut this.inner).poll_write(cx, remaining) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "the underlying stream accepted no encrypted bytes",
                    )))
                }
                Poll::Ready(Ok(written)) => {
                    this.write_frame_pos += written;
                    if this.write_frame_pos == this.write_frame.len() {
                        this.write_frame.clear();
                        this.write_frame_pos = 0;
                    }
                }
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }

        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        while this.has_pending_write() {
            let remaining = &this.write_frame[this.write_frame_pos..];
            match Pin::new(&mut this.inner).poll_write(cx, remaining) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "the underlying stream accepted no encrypted bytes",
                    )))
                }
                Poll::Ready(Ok(written)) => {
                    this.write_frame_pos += written;
                    if this.write_frame_pos == this.write_frame.len() {
                        this.write_frame.clear();
                        this.write_frame_pos = 0;
                    }
                }
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

fn crypto_read_error(error: CryptoError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("encrypted frame rejected: {error}"),
    )
}

fn crypto_write_error(error: CryptoError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("frame could not be encrypted: {error}"),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);

    async fn pair() -> (
        PendingConnection<tokio::io::DuplexStream>,
        PendingConnection<tokio::io::DuplexStream>,
        Arc<Identity>,
        Arc<Identity>,
    ) {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client_identity = Arc::new(Identity::generate().unwrap());
        let server_identity = Arc::new(Identity::generate().unwrap());
        let (client, server) = tokio::join!(
            initiate(client_io, Arc::clone(&client_identity), TIMEOUT),
            respond(server_io, Arc::clone(&server_identity), TIMEOUT),
        );
        (
            client.unwrap(),
            server.unwrap(),
            client_identity,
            server_identity,
        )
    }

    #[tokio::test]
    async fn handshake_binds_the_peer_identity_and_agrees_on_the_sas() {
        let (client, server, client_identity, server_identity) = pair().await;

        assert_eq!(
            client.peer_fingerprint(),
            Some(server_identity.fingerprint())
        );
        assert_eq!(
            server.peer_fingerprint(),
            Some(client_identity.fingerprint())
        );
        assert_eq!(client.sas(), server.sas());
        assert_eq!(client.sas().to_string().len(), 6);
    }

    #[tokio::test]
    async fn confirmed_streams_preserve_order_in_both_directions() {
        let (client, server, _, _) = pair().await;
        let mut client = client.confirm().unwrap();
        let mut server = server.confirm().unwrap();

        let request = vec![0x5a_u8; 200_000];
        let response = b"acknowledged".to_vec();

        let (written, echoed) = tokio::join!(
            async {
                client.write_all(&request).await.unwrap();
                client.flush().await.unwrap();
                request.len()
            },
            async {
                let mut received = vec![0_u8; request.len()];
                server.read_exact(&mut received).await.unwrap();
                assert_eq!(received, request);
                server.write_all(&response).await.unwrap();
                server.flush().await.unwrap();
                response.clone()
            },
        );

        let mut back = vec![0_u8; echoed.len()];
        client.read_exact(&mut back).await.unwrap();
        assert_eq!(back, echoed);
        assert_eq!(written, request.len());
    }

    #[tokio::test]
    async fn confirm_with_accepts_the_code_shown_by_the_peer() {
        let (client, server, _, _) = pair().await;
        let server_sas = server.sas();
        let mut client = client.confirm_with(&server_sas).unwrap();
        let mut server = server.confirm().unwrap();

        client.write_all(b"ping").await.unwrap();
        client.flush().await.unwrap();
        let mut buffer = [0_u8; 4];
        server.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"ping");
    }

    #[tokio::test]
    async fn a_tampered_encrypted_frame_is_rejected() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client_identity = Arc::new(Identity::generate().unwrap());
        let server_identity = Arc::new(Identity::generate().unwrap());

        // Corruption starts disabled so the handshake itself is untouched.
        let corrupt_after = Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
        let client_written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let client_io = TamperingStream::new(
            client_io,
            Arc::clone(&corrupt_after),
            Arc::clone(&client_written),
        );

        let (client, server) = tokio::join!(
            initiate(client_io, client_identity, TIMEOUT),
            respond(server_io, server_identity, TIMEOUT),
        );
        let mut client = client.unwrap().confirm().unwrap();
        let mut server = server.unwrap().confirm().unwrap();

        let handshake_bytes = client_written.load(std::sync::atomic::Ordering::SeqCst);
        corrupt_after.store(handshake_bytes, std::sync::atomic::Ordering::SeqCst);
        client.write_all(b"payload").await.unwrap();
        client.flush().await.unwrap();

        let mut buffer = [0_u8; 7];
        let error = server.read_exact(&mut buffer).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    /// Wraps a stream and flips one bit in the first write at or after a
    /// configured byte offset.
    struct TamperingStream<S> {
        inner: S,
        corrupt_after: Arc<std::sync::atomic::AtomicUsize>,
        written: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl<S> TamperingStream<S> {
        fn new(
            inner: S,
            corrupt_after: Arc<std::sync::atomic::AtomicUsize>,
            written: Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            Self {
                inner,
                corrupt_after,
                written,
            }
        }
    }

    impl<S> AsyncRead for TamperingStream<S>
    where
        S: AsyncRead + Unpin,
    {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
        }
    }

    impl<S> AsyncWrite for TamperingStream<S>
    where
        S: AsyncWrite + Unpin,
    {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            let offset = this.corrupt_after.load(std::sync::atomic::Ordering::SeqCst);
            let written = this.written.load(std::sync::atomic::Ordering::SeqCst);
            if offset != usize::MAX && offset >= written && offset < written + buf.len() {
                let mut copy = buf.to_vec();
                copy[offset - written] ^= 0x01;
                let result = Pin::new(&mut this.inner).poll_write(cx, &copy);
                if let Poll::Ready(Ok(count)) = result {
                    this.written
                        .fetch_add(count, std::sync::atomic::Ordering::SeqCst);
                }
                return result;
            }
            let result = Pin::new(&mut this.inner).poll_write(cx, buf);
            if let Poll::Ready(Ok(count)) = result {
                this.written
                    .fetch_add(count, std::sync::atomic::Ordering::SeqCst);
            }
            result
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }
}
