//! AirBridge protocol cryptography.
//!
//! This crate is the cryptographic core of AirBridge, a cross-platform
//! peer-to-peer file sharing engine. It implements a UKEY2-derived handshake,
//! an HKDF-SHA512 key schedule, AES-256-GCM framing with replay protection,
//! and Short Authentication String verification.
//!
//! # Quick start
//!
//! ```
//! use libcore_crypto::{
//!     Initiator, Responder, ResponderConfig, SasDigits, NEXT_PROTOCOL_AEAD,
//! };
//! # fn main() -> Result<(), libcore_crypto::CryptoError> {
//! let mut client = Initiator::new(NEXT_PROTOCOL_AEAD)?;
//! let mut server = Responder::new(ResponderConfig::default())?;
//!
//! // Four-message handshake.
//! let m1 = client.client_init()?;
//! server.accept_client_init(&m1)?;
//! let m2 = server.server_init()?;
//! client.accept_server_init(&m2)?;
//! let m3 = client.client_finished()?;
//! server.accept_client_finished(&m3)?;
//! let (m4, server_pending) = server.finish()?;
//! let client_pending = client.accept_server_finished(&m4)?;
//!
//! // Both sides derive the same six-digit code.
//! let client_sas = client_pending.sas(SasDigits::Six)?;
//! let server_sas = server_pending.sas(SasDigits::Six)?;
//! assert_eq!(client_sas, server_sas);
//!
//! // The operator compares the codes; only then is a session released.
//! let mut client_session = client_pending.confirm_with(&server_sas)?;
//! let mut server_session = server_pending.confirm_with(&client_sas)?;
//!
//! let frame = client_session.seal(b"hello")?;
//! assert_eq!(server_session.open(&frame)?, b"hello");
//! # Ok(())
//! # }
//! ```
//!
//! # Protocol
//!
//! ```text
//! M1  I -> R   ClientInit      version, random_c, suite, SHA-512 commitment to pk_c
//! M2  R -> I   ServerInit      version, random_s, suite, SHA-512 commitment to pk_s
//! M3  I -> R   ClientFinished  pk_c          (checked against the M1 commitment)
//! M4  R -> I   ServerFinished  pk_s          (checked against the M2 commitment)
//! ```
//!
//! Both parties then compute the shared secret `DHS` by P-256 ECDH and derive
//! from `DHS` and the full transcript `M1 | M2 | M3 | M4`:
//!
//! * `AUTH_STRING`, which the SAS is derived from, and
//! * an independent key block yielding directional AES-256 keys and GCM nonce
//!   prefixes.
//!
//! # Security properties
//!
//! * **Mutual commitment.** Neither party can choose an ephemeral key as a
//!   function of the other's. See the [`Initiator`] docs for why this needs a
//!   fourth message and what it buys over stock UKEY2.
//! * **Transcript binding.** Every derived value depends on all four messages,
//!   so any in-flight modification changes the keys and the SAS and causes an
//!   immediate, loud failure rather than a silent compromise.
//! * **Key separation.** The SAS is derived from `AUTH_STRING`, which comes
//!   from a different `HKDF-Extract` than the frame keys. Displaying the SAS
//!   therefore reveals nothing about the traffic keys.
//! * **Directional keys.** The two directions use independent keys, nonce
//!   prefixes and counters, which defeats reflection attacks.
//! * **Replay protection.** Frames carry a strictly increasing 64-bit sequence
//!   number that is authenticated as associated data, and the receiver commits
//!   its high-water mark only after the tag verifies.
//! * **No `unsafe`.** Forbidden at the workspace level.
//!
//! # Non-goals
//!
//! This crate does not authenticate *identities*. It authenticates a *channel*.
//! Trust in the peer's identity comes from the SAS comparison or from some
//! external credential, and that decision belongs to the application.
//!
//! It is also not a general-purpose crypto library: the primitives are not
//! re-exported, and the only supported construction is the one the protocol
//! needs.
//!
//! # Relationship to Google's UKEY2
//!
//! The message framing, the commitment-then-reveal structure, the `UKEY2 v1
//! auth` / `UKEY2 v1 next` HKDF salts and the transcript-binding rule all come
//! from the UKEY2 specification. Four points of divergence are deliberate:
//!
//! 1. **A fourth message**, to make commitments mutual (see above).
//! 2. **HKDF-SHA512**, as the specification's `P256_SHA512` suite requires.
//!    Google's C++ implementation actually uses HKDF-SHA256 with a
//!    single-block expand, contradicting the suite it advertises.
//! 3. **A 13-byte salt**, as written in the specification. Google's C++ passes
//!    `sizeof(literal)`, silently including the trailing NUL and producing a
//!    14-byte salt.
//! 4. **A canonical binary encoding** instead of protobuf, which has no
//!    canonical form and therefore makes any commitment over serialized bytes
//!    fragile.
//!
//! The consequence is that AirBridge speaks a UKEY2-*derived* protocol, not
//! Google's wire format. Interoperating with Nearby Connections would require a
//! compatibility layer; the divergence points are isolated and documented so
//! that work stays bounded.
//!
//! # Layering
//!
//! This crate has no async runtime and no I/O. That is intentional: it is the
//! component that belongs in the trusted computing base, so it does not inherit
//! an executor's threading or cancellation model. The transport layer drives
//! the handshake by moving byte vectors.

mod error;
mod frame;
mod handshake;
mod kdf;
mod sas;
mod wire;

pub use error::{AlertCode, CryptoError};
pub use frame::{Direction, AEAD_TAG_LEN, FRAME_HEADER_LEN, MAX_FRAME_PAYLOAD};
pub use handshake::{EstablishedSession, Initiator, PendingSession, Responder, ResponderConfig};
pub use sas::{Sas, SasDigits};
pub use wire::{CipherSuite, NEXT_PROTOCOL_AEAD};

/// Which side of a handshake a party is on.
///
/// The role is bound into the key schedule, the direction of every frame, and
/// the domain separator of every commitment, so the two roles are never
/// interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The party that sends `ClientInit` first.
    Initiator,
    /// The party that answers with `ServerInit`.
    Responder,
}

impl Role {
    /// Returns the opposite role.
    #[must_use]
    pub const fn peer(self) -> Self {
        match self {
            Self::Initiator => Self::Responder,
            Self::Responder => Self::Initiator,
        }
    }
}
