//! Error types for the AirBridge crypto core.
//!
//! Errors are deliberately flat and allocation-free on the hot path: an
//! attacker who can make us allocate while reporting a failure has found a
//! denial-of-service primitive. Every variant carries either a `&'static str`
//! or fixed-size integers.

use core::fmt;

/// UKEY2 `AlertType` codes.
///
/// Values are taken verbatim from Google's `ukey.proto` so that an AirBridge
/// peer can emit a wire-format alert that a UKEY2 implementation would
/// recognise. `CommitmentMismatch` is an AirBridge extension: UKEY2 terminates
/// the connection silently on a commitment failure because there is no
/// acknowledgement message after `ClientFinished`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AlertCode {
    /// The message could not be deserialised.
    BadMessage = 1,
    /// `message_type` holds an undefined value.
    BadMessageType = 2,
    /// The message type does not match the expected type at this stage.
    IncorrectMessage = 3,
    /// `message_data` could not be deserialised as the declared type.
    BadMessageData = 4,
    /// The protocol version is unsupported.
    BadVersion = 100,
    /// The nonce is missing or has the wrong length.
    BadRandom = 101,
    /// No mutually supported handshake cipher suite.
    BadHandshakeCipher = 102,
    /// The next-protocol identifier is missing or unsupported.
    BadNextProtocol = 103,
    /// A public key could not be parsed, or is not a valid curve point.
    BadPublicKey = 104,
    /// A revealed public key does not match its earlier commitment.
    ///
    /// AirBridge extension (not present in `ukey.proto`).
    CommitmentMismatch = 105,
    /// An internal error occurred; the peer should abort the handshake.
    InternalError = 200,
}

impl AlertCode {
    /// Returns the on-the-wire numeric code.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// Errors produced by the AirBridge crypto core.
///
/// The enum is `#[non_exhaustive]`: new failure modes will be added as the
/// protocol grows, and callers must tolerate that.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CryptoError {
    /// The operating-system random number generator failed.
    Random,

    /// Elliptic-curve Diffie-Hellman failed.
    ///
    /// In practice this means the peer's public key is not a valid P-256
    /// point, or is the point at infinity. Both are attacker-controlled
    /// inputs, so this is a routine error and never a panic.
    KeyAgreement,

    /// A key-derivation step produced an invalid length or failed internally.
    KeySchedule,

    /// A handshake message was structurally invalid.
    Encoding(&'static str),

    /// The handshake failed at the protocol level.
    ///
    /// `code` maps onto a UKEY2 `AlertType`; `detail` is a static description
    /// suitable for logs but never for the wire (it must not leak secrets).
    Handshake {
        /// The alert code that should be reported to the peer.
        code: AlertCode,
        /// A static, secret-free description of the failure.
        detail: &'static str,
    },

    /// AEAD authentication of a frame failed.
    ///
    /// The frame was forged, corrupted in transit, or sealed under a different
    /// key. These are deliberately indistinguishable to the caller: telling an
    /// attacker *which* check failed is an oracle.
    FrameAuth,

    /// A frame header was structurally invalid.
    FrameFormat(&'static str),

    /// A frame arrived with a sequence number that is not strictly greater
    /// than the highest already accepted.
    ReplayDetected {
        /// The sequence number carried by the rejected frame.
        seq: u64,
        /// The highest sequence number accepted so far.
        highest: u64,
    },

    /// The 64-bit send sequence space is exhausted; the session key must be
    /// rotated. Reaching this state requires 2^64 frames.
    SequenceExhausted,

    /// The peer's derived Short Authentication String did not match ours.
    ///
    /// This is what a successful man-in-the-middle attack looks like from the
    /// application's point of view. The session must be torn down.
    SasMismatch,
}

impl CryptoError {
    /// Returns the UKEY2 alert code that most closely describes this failure.
    #[must_use]
    pub const fn alert_code(&self) -> AlertCode {
        match self {
            Self::Handshake { code, .. } => *code,
            Self::Encoding(_) => AlertCode::BadMessageData,
            Self::KeyAgreement => AlertCode::BadPublicKey,
            Self::ReplayDetected { .. } | Self::FrameAuth | Self::FrameFormat(_) => {
                AlertCode::BadMessage
            }
            Self::Random | Self::KeySchedule | Self::SequenceExhausted | Self::SasMismatch => {
                AlertCode::InternalError
            }
        }
    }

    /// Serialises this error as a UKEY2-format `Alert` message suitable for
    /// sending to the peer.
    ///
    /// The text carries no secret material and is never part of the committed
    /// transcript, so this is best-effort diagnostics rather than a protocol
    /// obligation. Long messages are truncated.
    #[must_use]
    pub fn to_alert(&self) -> Vec<u8> {
        crate::wire::encode_alert(self.alert_code(), &self.to_string())
    }
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Random => f.write_str("random number generator failure"),
            Self::KeyAgreement => f.write_str("elliptic-curve key agreement failed"),
            Self::KeySchedule => f.write_str("key derivation failed"),
            Self::Encoding(detail) => write!(f, "malformed message: {detail}"),
            Self::Handshake { code, detail } => {
                write!(f, "handshake failure ({code:?}): {detail}")
            }
            Self::FrameAuth => f.write_str("frame authentication failed"),
            Self::FrameFormat(detail) => write!(f, "malformed frame: {detail}"),
            Self::ReplayDetected { seq, highest } => {
                write!(f, "replay rejected: seq {seq} is not above {highest}")
            }
            Self::SequenceExhausted => f.write_str("send sequence space exhausted"),
            Self::SasMismatch => f.write_str("short authentication string mismatch"),
        }
    }
}

impl std::error::Error for CryptoError {}
