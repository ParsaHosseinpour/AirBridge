//! Identity, SAS pairing and the authenticated preamble shared by sessions.
//!
//! The cryptographic core already exposes an identity key and a handshake that
//! binds that key into its transcript. This module turns those primitives into
//! the two objects a foreign UI needs:
//!
//! * [`PeerIdentity`], a stable ECDSA P-256 key the application persists in its
//!   own private storage. Its fingerprint is the only durable peer identifier
//!   AirBridge trusts.
//! * [`PairingSession`], an explicit connect-verify-exchange flow for a "verify
//!   this device" button. It runs the same handshake and preamble as a file
//!   transfer, but carries no payload.
//!
//! # Preamble
//!
//! After the handshake both sides exchange their *claimed* peer id and bind it
//! to the fingerprint they just authenticated, then agree on the operation:
//!
//! ```text
//! initiator -> responder   peer_id (u16 length + UTF-8)
//! responder -> initiator   peer_id
//! initiator -> responder   mode: 0x01 transfer | 0x02 pairing
//! responder -> initiator   mode echo
//! ```
//!
//! Only after that does `HELLO` (or pairing completion) travel. The peer id is
//! therefore never trusted before the handshake, and a receiver can persist the
//! mapping `fingerprint -> peer id` safely.

use std::sync::Arc;
use std::time::Duration;

use libcore_crypto::{fingerprint_hex, Identity};
use libcore_transport::{initiate, PendingConnection};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::FfiError;
use crate::runtime;

/// Largest peer id accepted on the wire, in UTF-8 bytes.
pub(crate) const MAX_PEER_ID_LEN: usize = 255;

/// Application mode for a transfer connection.
pub(crate) const MODE_TRANSFER: u8 = 0x01;

/// Application mode for a pairing-only connection.
pub(crate) const MODE_PAIRING: u8 = 0x02;

/// Timeout applied to the post-handshake peer id and mode exchange.
const PREAMBLE_TIMEOUT: Duration = Duration::from_secs(30);

/// A stable identity key pair owned by the application.
///
/// Generate once, persist the PKCS#8 bytes in private storage, and construct
/// this object from them on later launches. The fingerprint survives reinstall
/// only if the application backs the key up; AirBridge itself never uploads it.
#[derive(uniffi::Object)]
pub struct PeerIdentity {
    inner: Arc<Identity>,
}

impl PeerIdentity {
    pub(crate) fn arc(&self) -> Arc<Identity> {
        Arc::clone(&self.inner)
    }
}

#[uniffi::export]
impl PeerIdentity {
    /// Generates a fresh identity.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::Crypto`] if the system random number generator or
    /// the key loader fails.
    #[uniffi::constructor]
    pub fn generate() -> Result<Arc<Self>, FfiError> {
        let identity = Identity::generate().map_err(FfiError::from)?;
        Ok(Arc::new(Self {
            inner: Arc::new(identity),
        }))
    }

    /// Restores an identity from the PKCS#8 DER bytes returned by
    /// [`Self::to_pkcs8`].
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::Crypto`] when the bytes are not a valid P-256
    /// private-key document.
    #[uniffi::constructor]
    pub fn from_pkcs8(pkcs8: Vec<u8>) -> Result<Arc<Self>, FfiError> {
        let identity = Identity::from_pkcs8(&pkcs8).map_err(FfiError::from)?;
        Ok(Arc::new(Self {
            inner: Arc::new(identity),
        }))
    }

    /// Returns the PKCS#8 DER encoding for private storage.
    ///
    /// The bytes are secret. Never log them or send them to another device.
    #[must_use]
    pub fn to_pkcs8(&self) -> Vec<u8> {
        self.inner.to_pkcs8().to_vec()
    }

    /// Returns the uncompressed P-256 public key (`0x04 || X || Y`).
    #[must_use]
    pub fn public_key(&self) -> Vec<u8> {
        self.inner.public_key().to_vec()
    }

    /// Returns the lower-case SHA-256 fingerprint (64 hex characters).
    #[must_use]
    pub fn fingerprint(&self) -> String {
        fingerprint_hex(&self.inner.fingerprint())
    }
}

/// The code and identity a pairing partner must confirm.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PairingChallenge {
    /// Six digits, grouped for display by the UI.
    pub sas: String,
    /// SHA-256 fingerprint of the peer's identity key.
    pub peer_fingerprint: String,
    /// The peer's uncompressed P-256 public key, for diagnostics.
    pub peer_public_key: Vec<u8>,
}

/// The authenticated result of an explicit pairing.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PairingResult {
    /// Peer id claimed by the other side *after* the handshake.
    pub peer_id: String,
    /// Fingerprint that the peer id is now bound to.
    pub peer_fingerprint: String,
    /// The code both screens displayed.
    pub sas: String,
}

/// A connect-verify pair operation against another AirBridge device.
///
/// `start` performs the handshake and returns the code to display; `confirm`
/// releases the channel, exchanges peer ids and closes it. The receiver can be
/// a normal long-lived [`crate::FileTransferSession::receive`], which surfaces
/// the same code as a verification event.
#[derive(uniffi::Object)]
pub struct PairingSession {
    address: String,
    identity: Arc<Identity>,
    local_peer_id: String,
    timeout: Duration,
    pending: tokio::sync::Mutex<Option<PendingConnection<TcpStream>>>,
    completed: std::sync::atomic::AtomicBool,
}

#[uniffi::export]
impl PairingSession {
    /// Creates a pairing client for `address` (`host:port`).
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] for an empty address or peer id,
    /// an over-long peer id, or a zero timeout.
    #[uniffi::constructor]
    pub fn new(
        address: String,
        identity: Arc<PeerIdentity>,
        local_peer_id: String,
        timeout_ms: u64,
    ) -> Result<Arc<Self>, FfiError> {
        if address.trim().is_empty() {
            return Err(FfiError::invalid_argument("address must not be empty"));
        }
        validate_peer_id(&local_peer_id)?;
        if timeout_ms == 0 {
            return Err(FfiError::invalid_argument("timeout_ms must be non-zero"));
        }
        Ok(Arc::new(Self {
            address,
            identity: identity.arc(),
            local_peer_id,
            timeout: Duration::from_millis(timeout_ms),
            pending: tokio::sync::Mutex::new(None),
            completed: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    /// Connects, runs the handshake, and returns the code to compare.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::AlreadyFinished`] on a second call, or the mapped
    /// connection/handshake failure. A peer that presents no identity is
    /// rejected because it cannot be bound to a durable fingerprint.
    pub async fn start(self: Arc<Self>) -> Result<PairingChallenge, FfiError> {
        if self.completed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(FfiError::AlreadyFinished);
        }
        let address = self.address.clone();
        let identity = Arc::clone(&self.identity);
        let timeout = self.timeout;
        let (challenge, pending) = runtime::run_on_runtime(async move {
            let stream = tokio::time::timeout(timeout, TcpStream::connect(address))
                .await
                .map_err(|_| FfiError::Timeout {
                    message: "waiting for the peer to accept a pairing connection".to_owned(),
                })?
                .map_err(crate::transfer::io_error)?;
            let _ = stream.set_nodelay(true);
            let pending = initiate(stream, identity, timeout).await?;
            let challenge = challenge_from(&pending)?;
            Ok::<_, FfiError>((challenge, pending))
        })
        .await??;

        let mut slot = self.pending.lock().await;
        if slot.is_some() {
            // Keep the public promise that only one handshake can be pending.
            pending.reject();
            return Err(FfiError::AlreadyFinished);
        }
        // The pending connection is stored only after the handshake completes,
        // so `start` never exposes a half-open socket.
        *slot = Some(pending);
        Ok(challenge)
    }

    /// Confirms the displayed code, exchanges peer ids, and closes the pairing
    /// channel.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::VerificationRejected`] if `start` was never called or
    /// the channel is already gone, or the mapped I/O failure.
    pub async fn confirm(self: Arc<Self>) -> Result<PairingResult, FfiError> {
        let pending = self
            .pending
            .lock()
            .await
            .take()
            .ok_or(FfiError::VerificationRejected)?;
        let peer_fingerprint = pending.peer_fingerprint().ok_or_else(|| FfiError::Crypto {
            message: "the peer did not present an identity key".to_owned(),
        })?;
        let sas = pending.sas().to_padded_string();
        let local_peer_id = self.local_peer_id.clone();

        let result = runtime::run_on_runtime(async move {
            let mut stream = pending.confirm()?;
            let peer_id = exchange_peer_ids(&mut stream, &local_peer_id).await?;
            write_mode(&mut stream, MODE_PAIRING).await?;
            let echoed = read_mode(&mut stream).await?;
            if echoed != MODE_PAIRING {
                return Err(FfiError::Protocol {
                    message: "the peer did not acknowledge pairing mode".to_owned(),
                });
            }
            shutdown(&mut stream).await;
            Ok::<_, FfiError>(peer_id)
        })
        .await??;
        self.completed
            .store(true, std::sync::atomic::Ordering::SeqCst);

        Ok(PairingResult {
            peer_id: result,
            peer_fingerprint: fingerprint_hex(&peer_fingerprint),
            sas,
        })
    }

    /// Drops the pairing connection without confirming it.
    pub async fn reject(&self) {
        if let Some(pending) = self.pending.lock().await.take() {
            pending.reject();
        }
        self.completed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Extracts the displayable challenge from a completed handshake.
pub(crate) fn challenge_from<S>(
    pending: &PendingConnection<S>,
) -> Result<PairingChallenge, FfiError> {
    let fingerprint = pending.peer_fingerprint().ok_or_else(|| FfiError::Crypto {
        message: "the peer did not present an identity key".to_owned(),
    })?;
    let public_key = pending
        .peer_identity_public_key()
        .ok_or_else(|| FfiError::Crypto {
            message: "the peer did not present an identity key".to_owned(),
        })?
        .to_vec();
    Ok(PairingChallenge {
        sas: pending.sas().to_padded_string(),
        peer_fingerprint: fingerprint_hex(&fingerprint),
        peer_public_key: public_key,
    })
}

/// Rejects empty or over-long peer ids before they reach a length prefix.
pub(crate) fn validate_peer_id(peer_id: &str) -> Result<(), FfiError> {
    if peer_id.trim().is_empty() {
        return Err(FfiError::invalid_argument("peer id must not be empty"));
    }
    if peer_id.len() > MAX_PEER_ID_LEN {
        return Err(FfiError::invalid_argument(format!(
            "peer id must be at most {MAX_PEER_ID_LEN} UTF-8 bytes"
        )));
    }
    Ok(())
}

/// Sends this side's peer id and returns the peer's claimed id.
///
/// The caller must already have authenticated the channel; the initiator and
/// responder use the same order (write then read), so neither side can deadlock
/// on a matching-length exchange.
pub(crate) async fn exchange_peer_ids<S>(
    stream: &mut S,
    local_peer_id: &str,
) -> Result<String, FfiError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let local = local_peer_id.as_bytes();
    let length = u16::try_from(local.len())
        .map_err(|_| FfiError::invalid_argument("peer id is too long"))?;
    let exchange = async {
        stream.write_all(&length.to_be_bytes()).await?;
        stream.write_all(local).await?;
        stream.flush().await?;

        let mut length = [0_u8; 2];
        stream.read_exact(&mut length).await?;
        let length = usize::from(u16::from_be_bytes(length));
        if length == 0 || length > MAX_PEER_ID_LEN {
            return Err(FfiError::Protocol {
                message: "the peer sent an out-of-range peer id length".to_owned(),
            });
        }
        let mut bytes = vec![0_u8; length];
        stream.read_exact(&mut bytes).await?;
        String::from_utf8(bytes).map_err(|_| FfiError::Protocol {
            message: "the peer id is not valid UTF-8".to_owned(),
        })
    };
    tokio::time::timeout(PREAMBLE_TIMEOUT, exchange)
        .await
        .map_err(|_| FfiError::Timeout {
            message: "exchanging authenticated peer ids".to_owned(),
        })?
}

/// Writes the one-byte application mode, flushing it immediately.
pub(crate) async fn write_mode<S>(stream: &mut S, mode: u8) -> Result<(), FfiError>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    stream.write_all(&[mode]).await?;
    stream.flush().await?;
    Ok(())
}

/// Reads and validates the one-byte application mode.
pub(crate) async fn read_mode<S>(stream: &mut S) -> Result<u8, FfiError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut mode = [0_u8; 1];
    tokio::time::timeout(PREAMBLE_TIMEOUT, stream.read_exact(&mut mode))
        .await
        .map_err(|_| FfiError::Timeout {
            message: "waiting for the peer's application mode".to_owned(),
        })??;
    match mode[0] {
        MODE_TRANSFER | MODE_PAIRING => Ok(mode[0]),
        _ => Err(FfiError::Protocol {
            message: "the peer requested an unknown application mode".to_owned(),
        }),
    }
}

/// Best-effort orderly close; the pairing result does not depend on it.
pub(crate) async fn shutdown<S>(stream: &mut S)
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let _ = tokio::time::timeout(PREAMBLE_TIMEOUT, stream.shutdown()).await;
}

/// Parses a trusted fingerprint string, pointing at the offending field.
pub(crate) fn parse_fingerprint(value: &str, field: &str) -> Result<[u8; 32], FfiError> {
    libcore_crypto::parse_fingerprint_hex(value).ok_or_else(|| {
        FfiError::invalid_argument(format!(
            "{field} must be 64 hexadecimal characters, got {value:?}"
        ))
    })
}
