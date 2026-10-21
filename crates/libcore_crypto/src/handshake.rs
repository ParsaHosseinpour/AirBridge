//! The AirBridge handshake: a UKEY2-derived, mutually-committing key exchange.
//!
//! # Message flow
//!
//! ```text
//! M1  Initiator -> Responder   ClientInit    version, random_c, suite, commit(pk_c), next_protocol
//! M2  Responder -> Initiator   ServerInit    version, random_s, suite, commit(pk_s)
//! M3  Initiator -> Responder   ClientFinished pk_c
//! M4  Responder -> Initiator   ServerFinished pk_s
//! ```
//!
//! # What UKEY2 does, and what we changed
//!
//! UKEY2 is a three-message exchange in which only the *client* commits. The
//! client hashes the serialized `ClientFinished` message into `M1`, before it
//! has seen the server's public key. The server, by contrast, chooses its key
//! after seeing the client's commitment, so it is fully adaptive.
//!
//! We add a fourth message so that **both** parties commit before revealing:
//!
//! * the initiator commits to `pk_c` in `M1` and reveals it in `M3`;
//! * the responder commits to `pk_s` in `M2` — after seeing `commit(pk_c)` but
//!   before `pk_c` is revealed — and reveals it in `M4`.
//!
//! Neither party can choose an ephemeral key as a function of the other's.
//! That is the property UKEY2's security discussion claims for the client, now
//! held symmetrically. A commitment is a SHA-512 hash over
//!
//! ```text
//! domain || version || suite || random || public_key
//! ```
//!
//! with distinct `domain` strings for the two roles. Domain separation matters:
//! without it, a client commitment could be replayed as a server commitment
//! (a reflection attack), and the hash would still verify.
//!
//! Binding the party's own nonce into its commitment also ties the commitment
//! to this specific session, so a commitment recorded from an earlier handshake
//! cannot be reused.
//!
//! # Verification code
//!
//! After `M4`, both parties derive the key schedule (see [`crate::kdf`]) and a
//! Short Authentication String (see [`crate::sas`]). The SAS must be compared
//! out of band before the session may be used; [`PendingSession`] enforces that
//! by not handing out a [`FrameCipher`] until the caller explicitly confirms.
//!
//! # Failure handling
//!
//! Any error poisons the handshake object permanently. A state machine that can
//! be nudged back into an earlier state after a failure is a rich source of
//! protocol bugs, so this one cannot be.

use core::fmt;

use ring::rand::{SecureRandom as _, SystemRandom};
use ring::{agreement, digest};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::error::{AlertCode, CryptoError};
use crate::frame::FrameCipher;
use crate::kdf::KeySchedule;
use crate::sas::{Sas, SasDigits};
use crate::wire::{
    self, CipherSuite, ClientFinished, Reader, ServerFinished, COMMITMENT_LEN, MAX_MESSAGE_BYTES,
    MAX_NEXT_PROTOCOL_LEN, NONCE_LEN, P256_PUBLIC_KEY_LEN, PROTOCOL_VERSION, TAG_CLIENT_FINISHED,
    TAG_CLIENT_INIT, TAG_SERVER_FINISHED, TAG_SERVER_INIT,
};
use crate::Role;

/// Domain separator for the initiator's key commitment.
const DOMAIN_COMMIT_CLIENT: &[u8] = b"AirBridge UKEY2 v1 client-key-commitment";

/// Domain separator for the responder's key commitment.
const DOMAIN_COMMIT_SERVER: &[u8] = b"AirBridge UKEY2 v1 server-key-commitment";

// ---------------------------------------------------------------------------
// Ephemeral P-256 key pair
// ---------------------------------------------------------------------------

/// A single-use P-256 ECDH key pair.
///
/// The private scalar lives inside `ring`, which handles it in constant time
/// and zeroises it on drop. It is never exposed, cloned, or serialised.
struct EphemeralKeyPair {
    private: Option<agreement::EphemeralPrivateKey>,
    public: Vec<u8>,
}

impl EphemeralKeyPair {
    fn generate() -> Result<Self, CryptoError> {
        let rng = SystemRandom::new();
        let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
            .map_err(|_| CryptoError::KeyAgreement)?;
        let public = private
            .compute_public_key()
            .map_err(|_| CryptoError::KeyAgreement)?;
        Ok(Self {
            private: Some(private),
            public: public.as_ref().to_vec(),
        })
    }

    fn public_key(&self) -> &[u8] {
        &self.public
    }

    /// Computes the raw ECDH output (the x-coordinate of the shared point).
    ///
    /// Consumes the key pair: an ephemeral key must never produce two shared
    /// secrets, because that would make the nonce schedule across two sessions
    /// depend on the same private scalar.
    fn agree(mut self, peer_public_key: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        if peer_public_key.len() != P256_PUBLIC_KEY_LEN || peer_public_key.first() != Some(&0x04) {
            return Err(CryptoError::Handshake {
                code: AlertCode::BadPublicKey,
                detail: "peer public key is not an uncompressed P-256 point",
            });
        }
        let private = self.private.take().ok_or(CryptoError::KeyAgreement)?;
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, peer_public_key);
        agreement::agree_ephemeral(private, &peer, |secret| Zeroizing::new(secret.to_vec()))
            .map_err(|_| CryptoError::Handshake {
                code: AlertCode::BadPublicKey,
                detail: "peer public key is not a valid curve point",
            })
    }
}

impl fmt::Debug for EphemeralKeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately prints only the public half.
        f.debug_struct("EphemeralKeyPair")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

/// Computes a key commitment.
fn commitment(
    domain: &[u8],
    suite: CipherSuite,
    random: &[u8; NONCE_LEN],
    public_key: &[u8],
) -> [u8; COMMITMENT_LEN] {
    let mut ctx = digest::Context::new(&digest::SHA512);
    ctx.update(domain);
    ctx.update(&PROTOCOL_VERSION.to_be_bytes());
    ctx.update(&suite.wire_id().to_be_bytes());
    ctx.update(random);
    ctx.update(public_key);
    let mut out = [0_u8; COMMITMENT_LEN];
    out.copy_from_slice(ctx.finish().as_ref());
    out
}

/// Constant-time comparison of two commitments.
fn commitments_equal(a: &[u8; COMMITMENT_LEN], b: &[u8; COMMITMENT_LEN]) -> bool {
    bool::from(a[..].ct_eq(&b[..]))
}

/// Policy for a [`Responder`]: which next protocols and cipher suites it will
/// accept.
///
/// The responder chooses, so this is where a deployment expresses its
/// configuration. The default accepts only [`wire::NEXT_PROTOCOL_AEAD`] over
/// P-256/SHA-512.
#[derive(Debug, Clone)]
pub struct ResponderConfig {
    accepted_protocols: Vec<String>,
    accepted_suites: Vec<CipherSuite>,
}

impl ResponderConfig {
    /// Builds a config that accepts the given next-protocol identifiers over
    /// P-256/SHA-512.
    #[must_use]
    pub fn new(protocols: &[&str]) -> Self {
        Self {
            accepted_protocols: protocols.iter().map(|p| (*p).to_owned()).collect(),
            accepted_suites: vec![CipherSuite::P256Sha512],
        }
    }

    /// Replaces the accepted cipher suites.
    ///
    /// `P256Sha512` is currently the only suite that exists, so this is mainly
    /// useful for pinning and for forward compatibility.
    #[must_use]
    pub fn with_suites(mut self, suites: &[CipherSuite]) -> Self {
        self.accepted_suites = suites.to_vec();
        self
    }

    fn accepts_protocol(&self, protocol: &str) -> bool {
        self.accepted_protocols.iter().any(|p| p == protocol)
    }

    fn accepts_suite(&self, suite: CipherSuite) -> bool {
        self.accepted_suites.contains(&suite)
    }
}

impl Default for ResponderConfig {
    fn default() -> Self {
        Self::new(&[wire::NEXT_PROTOCOL_AEAD])
    }
}

// ---------------------------------------------------------------------------
// Pending and established sessions
// ---------------------------------------------------------------------------

/// A completed handshake whose SAS has not yet been confirmed.
///
/// This type exists to make the safe path the only path. A frame cipher is only
/// obtainable through [`PendingSession::confirm`] or
/// [`PendingSession::confirm_with`], so an application cannot accidentally
/// start sending file data before the operator has compared the codes.
pub struct PendingSession {
    role: Role,
    schedule: KeySchedule,
}

impl PendingSession {
    fn new(role: Role, schedule: KeySchedule) -> Self {
        Self { role, schedule }
    }

    /// The local role in this handshake.
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// The public session identifier, bound into every frame's associated data.
    #[must_use]
    pub fn session_id(&self) -> &[u8; 32] {
        &self.schedule.session_id
    }

    /// Derives the Short Authentication String for display.
    ///
    /// Show this to the operator, have them compare it with the code shown on
    /// the peer, and only then call [`PendingSession::confirm_with`].
    ///
    /// # Errors
    ///
    /// In practice this cannot fail; the error exists because the underlying
    /// KDF is fallible.
    pub fn sas(&self, digits: SasDigits) -> Result<Sas, CryptoError> {
        Sas::derive(&self.schedule.auth_secret[..], digits)
    }

    /// Completes the handshake, recording `digits` as the SAS to retain.
    ///
    /// This **does not** verify anything. Use it only when the transport is
    /// authenticated by some other means (for example a pre-shared PIN), or
    /// when the SAS was displayed and confirmed by the operator out of band.
    ///
    /// # Errors
    ///
    /// Returns an error if constructing the frame cipher fails.
    pub fn confirm(self, digits: SasDigits) -> Result<EstablishedSession, CryptoError> {
        let sas = self.sas(digits)?;
        self.finish(sas)
    }

    /// Completes the handshake, asserting that the peer's SAS matched.
    ///
    /// Use this when the application has collected the peer's code (typed in,
    /// scanned, or read aloud). A mismatch means an active man-in-the-middle
    /// was present, or that the two parties are not in the same session, and
    /// the caller must tear the connection down.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::SasMismatch`] if the codes differ.
    pub fn confirm_with(self, peer_sas: &Sas) -> Result<EstablishedSession, CryptoError> {
        let sas = self.sas(peer_sas.digits())?;
        if !sas.matches(peer_sas) {
            return Err(CryptoError::SasMismatch);
        }
        self.finish(sas)
    }

    fn finish(self, sas: Sas) -> Result<EstablishedSession, CryptoError> {
        let frames = FrameCipher::new(self.role, &self.schedule)?;
        // Dropping `self.schedule` here zeroises AUTH_STRING and both frame
        // keys. `ring` owns its copy of the key material from this point.
        Ok(EstablishedSession {
            role: self.role,
            session_id: self.schedule.session_id,
            sas,
            frames,
        })
    }
}

/// An authenticated, replay-protected session.
///
/// Produced only after the SAS step, so possession of this value means the
/// application chose to trust the peer.
pub struct EstablishedSession {
    role: Role,
    session_id: [u8; 32],
    sas: Sas,
    frames: FrameCipher,
}

impl EstablishedSession {
    /// The local role in this session.
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// The SAS that was confirmed for this session.
    #[must_use]
    pub const fn sas(&self) -> &Sas {
        &self.sas
    }

    /// The public session identifier.
    #[must_use]
    pub const fn session_id(&self) -> &[u8; 32] {
        &self.session_id
    }

    /// Encrypts and authenticates a frame.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::FrameFormat`] if `plaintext` exceeds
    /// [`crate::MAX_FRAME_PAYLOAD`], and [`CryptoError::SequenceExhausted`]
    /// once the send counter is exhausted.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        self.frames.seal(plaintext)
    }

    /// Verifies and decrypts a frame.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::FrameFormat`] for a malformed frame,
    /// [`CryptoError::ReplayDetected`] for a sequence number at or below the
    /// receive high-water mark, and [`CryptoError::FrameAuth`] if the tag does
    /// not verify.
    pub fn open(&mut self, frame: &[u8]) -> Result<Vec<u8>, CryptoError> {
        self.frames.open(frame)
    }

    /// The sequence number the next [`EstablishedSession::seal`] will use.
    #[must_use]
    pub const fn send_seq(&self) -> u64 {
        self.frames.send_seq()
    }

    /// The highest sequence number accepted so far.
    #[must_use]
    pub const fn highest_recv_seq(&self) -> Option<u64> {
        self.frames.highest_recv_seq()
    }
}

// ---------------------------------------------------------------------------
// Initiator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitiatorState {
    Start,
    SentClientInit,
    ReceivedServerInit,
    SentClientFinished,
    Complete,
    Poisoned,
}

/// The party that speaks first.
///
/// Drive it in order:
/// [`client_init`](Initiator::client_init) →
/// [`accept_server_init`](Initiator::accept_server_init) →
/// [`client_finished`](Initiator::client_finished) →
/// [`accept_server_finished`](Initiator::accept_server_finished).
pub struct Initiator {
    state: InitiatorState,
    suite: CipherSuite,
    next_protocol: String,
    key_pair: Option<EphemeralKeyPair>,
    public_key: Vec<u8>,
    random: [u8; NONCE_LEN],
    peer_random: [u8; NONCE_LEN],
    peer_commitment: [u8; COMMITMENT_LEN],
    transcript: Vec<u8>,
}

impl Initiator {
    /// Creates an initiator for the given next-protocol identifier.
    ///
    /// # Errors
    ///
    /// Fails if the protocol identifier is empty, too long, or if key
    /// generation fails.
    pub fn new(next_protocol: &str) -> Result<Self, CryptoError> {
        if next_protocol.is_empty() || next_protocol.len() > MAX_NEXT_PROTOCOL_LEN {
            return Err(CryptoError::Encoding(
                "next_protocol length is out of range",
            ));
        }
        let key_pair = EphemeralKeyPair::generate()?;
        let public_key = key_pair.public_key().to_vec();
        Ok(Self {
            state: InitiatorState::Start,
            suite: CipherSuite::P256Sha512,
            next_protocol: next_protocol.to_owned(),
            key_pair: Some(key_pair),
            public_key,
            random: [0_u8; NONCE_LEN],
            peer_random: [0_u8; NONCE_LEN],
            peer_commitment: [0_u8; COMMITMENT_LEN],
            transcript: Vec::with_capacity(MAX_MESSAGE_BYTES),
        })
    }

    /// Produces `M1`, the `ClientInit` message.
    ///
    /// # Errors
    ///
    /// Fails if called out of order, or if the system RNG or encoder fails.
    pub fn client_init(&mut self) -> Result<Vec<u8>, CryptoError> {
        let result = self.client_init_inner();
        self.poison_on_error(&result);
        result
    }

    /// Consumes `M2`, the `ServerInit` message.
    ///
    /// # Errors
    ///
    /// Fails if the message is malformed, selects an unexpected suite, or
    /// arrives out of order.
    pub fn accept_server_init(&mut self, message: &[u8]) -> Result<(), CryptoError> {
        let result = self.accept_server_init_inner(message);
        self.poison_on_error(&result);
        result
    }

    /// Produces `M3`, the `ClientFinished` message.
    ///
    /// # Errors
    ///
    /// Fails if called out of order or if the encoder fails.
    pub fn client_finished(&mut self) -> Result<Vec<u8>, CryptoError> {
        let result = self.client_finished_inner();
        self.poison_on_error(&result);
        result
    }

    /// Consumes `M4`, the `ServerFinished` message, and derives the session.
    ///
    /// # Errors
    ///
    /// Fails if the server's revealed public key does not match the commitment
    /// it made in `M2` — the signature of a tampering intermediary — or if key
    /// agreement fails.
    pub fn accept_server_finished(
        &mut self,
        message: &[u8],
    ) -> Result<PendingSession, CryptoError> {
        let result = self.accept_server_finished_inner(message);
        if result.is_err() {
            self.state = InitiatorState::Poisoned;
        }
        result
    }

    fn poison_on_error<T>(&mut self, result: &Result<T, CryptoError>) {
        if result.is_err() {
            self.state = InitiatorState::Poisoned;
        }
    }

    fn expect_state(&self, expected: InitiatorState) -> Result<(), CryptoError> {
        if self.state == expected {
            Ok(())
        } else {
            Err(CryptoError::Handshake {
                code: AlertCode::IncorrectMessage,
                detail: "handshake message received out of order",
            })
        }
    }

    fn client_init_inner(&mut self) -> Result<Vec<u8>, CryptoError> {
        self.expect_state(InitiatorState::Start)?;

        let rng = SystemRandom::new();
        rng.fill(&mut self.random)
            .map_err(|_| CryptoError::Random)?;

        let msg = wire::ClientInit {
            random: self.random,
            suite: self.suite,
            commitment: commitment(
                DOMAIN_COMMIT_CLIENT,
                self.suite,
                &self.random,
                &self.public_key,
            ),
            next_protocol: self.next_protocol.clone(),
        };
        let encoded = wire::encode_client_init(&msg)?;
        self.transcript.extend_from_slice(&encoded);
        self.state = InitiatorState::SentClientInit;
        Ok(encoded)
    }

    fn accept_server_init_inner(&mut self, message: &[u8]) -> Result<(), CryptoError> {
        self.expect_state(InitiatorState::SentClientInit)?;
        let mut reader = open_message(message, TAG_SERVER_INIT)?;
        let msg = wire::decode_server_init(&mut reader)?;
        reader.finish()?;

        if msg.suite != self.suite {
            return Err(CryptoError::Handshake {
                code: AlertCode::BadHandshakeCipher,
                detail: "server selected a suite that was not offered",
            });
        }

        self.peer_random = msg.random;
        self.peer_commitment = msg.commitment;
        self.transcript.extend_from_slice(message);
        self.state = InitiatorState::ReceivedServerInit;
        Ok(())
    }

    fn client_finished_inner(&mut self) -> Result<Vec<u8>, CryptoError> {
        self.expect_state(InitiatorState::ReceivedServerInit)?;
        let msg = ClientFinished {
            public_key: self.public_key.clone(),
        };
        let encoded = wire::encode_client_finished(&msg)?;
        self.transcript.extend_from_slice(&encoded);
        self.state = InitiatorState::SentClientFinished;
        Ok(encoded)
    }

    fn accept_server_finished_inner(
        &mut self,
        message: &[u8],
    ) -> Result<PendingSession, CryptoError> {
        self.expect_state(InitiatorState::SentClientFinished)?;
        let mut reader = open_message(message, TAG_SERVER_FINISHED)?;
        let msg = wire::decode_server_finished(&mut reader)?;
        reader.finish()?;

        // The decisive check: the responder committed to this key in M2,
        // before it had seen ours. A mismatch means M2 and M4 came from
        // different parties, or M4 was tampered with in flight.
        let expected = commitment(
            DOMAIN_COMMIT_SERVER,
            self.suite,
            &self.peer_random,
            &msg.public_key,
        );
        if !commitments_equal(&expected, &self.peer_commitment) {
            return Err(CryptoError::Handshake {
                code: AlertCode::CommitmentMismatch,
                detail: "server public key does not match its commitment",
            });
        }

        self.transcript.extend_from_slice(message);

        let key_pair = self.key_pair.take().ok_or(CryptoError::Handshake {
            code: AlertCode::InternalError,
            detail: "ephemeral key pair already consumed",
        })?;
        let shared = key_pair.agree(&msg.public_key)?;
        let schedule = KeySchedule::derive(&shared[..], &self.transcript)?;

        self.state = InitiatorState::Complete;
        Ok(PendingSession::new(Role::Initiator, schedule))
    }
}

// ---------------------------------------------------------------------------
// Responder
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponderState {
    Start,
    ReceivedClientInit,
    SentServerInit,
    ReceivedClientFinished,
    Poisoned,
}

/// The party that answers.
///
/// Drive it in order:
/// [`accept_client_init`](Responder::accept_client_init) →
/// [`server_init`](Responder::server_init) →
/// [`accept_client_finished`](Responder::accept_client_finished) →
/// [`finish`](Responder::finish).
pub struct Responder {
    state: ResponderState,
    config: ResponderConfig,
    suite: CipherSuite,
    key_pair: Option<EphemeralKeyPair>,
    public_key: Vec<u8>,
    random: [u8; NONCE_LEN],
    peer_random: [u8; NONCE_LEN],
    peer_commitment: [u8; COMMITMENT_LEN],
    peer_public_key: Vec<u8>,
    transcript: Vec<u8>,
}

impl Responder {
    /// Creates a responder under the given policy.
    ///
    /// # Errors
    ///
    /// Fails if key generation fails.
    pub fn new(config: ResponderConfig) -> Result<Self, CryptoError> {
        let key_pair = EphemeralKeyPair::generate()?;
        let public_key = key_pair.public_key().to_vec();
        Ok(Self {
            state: ResponderState::Start,
            config,
            suite: CipherSuite::P256Sha512,
            key_pair: Some(key_pair),
            public_key,
            random: [0_u8; NONCE_LEN],
            peer_random: [0_u8; NONCE_LEN],
            peer_commitment: [0_u8; COMMITMENT_LEN],
            peer_public_key: Vec::new(),
            transcript: Vec::with_capacity(MAX_MESSAGE_BYTES),
        })
    }

    /// Consumes `M1`, the `ClientInit` message.
    ///
    /// # Errors
    ///
    /// Fails if the message is malformed, if the offered suite or next protocol
    /// is unacceptable, or if it arrives out of order.
    pub fn accept_client_init(&mut self, message: &[u8]) -> Result<(), CryptoError> {
        let result = self.accept_client_init_inner(message);
        if result.is_err() {
            self.state = ResponderState::Poisoned;
        }
        result
    }

    /// Produces `M2`, the `ServerInit` message, containing this responder's key
    /// commitment.
    ///
    /// # Errors
    ///
    /// Fails if called out of order, or if the system RNG fails.
    pub fn server_init(&mut self) -> Result<Vec<u8>, CryptoError> {
        let result = self.server_init_inner();
        if result.is_err() {
            self.state = ResponderState::Poisoned;
        }
        result
    }

    /// Consumes `M3`, the `ClientFinished` message, verifying the client's
    /// commitment.
    ///
    /// # Errors
    ///
    /// Fails if the revealed key does not match the commitment from `M1`.
    pub fn accept_client_finished(&mut self, message: &[u8]) -> Result<(), CryptoError> {
        let result = self.accept_client_finished_inner(message);
        if result.is_err() {
            self.state = ResponderState::Poisoned;
        }
        result
    }

    /// Produces `M4`, the `ServerFinished` message, and derives the session.
    ///
    /// Returns the bytes to send followed by the pending session.
    ///
    /// # Errors
    ///
    /// Fails if called out of order or if key agreement fails.
    pub fn finish(mut self) -> Result<(Vec<u8>, PendingSession), CryptoError> {
        if self.state != ResponderState::ReceivedClientFinished {
            self.state = ResponderState::Poisoned;
            return Err(CryptoError::Handshake {
                code: AlertCode::IncorrectMessage,
                detail: "handshake message received out of order",
            });
        }

        let msg = ServerFinished {
            public_key: self.public_key.clone(),
        };
        let encoded = wire::encode_server_finished(&msg)?;
        self.transcript.extend_from_slice(&encoded);

        let key_pair = self.key_pair.take().ok_or(CryptoError::Handshake {
            code: AlertCode::InternalError,
            detail: "ephemeral key pair already consumed",
        })?;
        let shared = key_pair.agree(&self.peer_public_key)?;
        let schedule = KeySchedule::derive(&shared[..], &self.transcript)?;

        Ok((encoded, PendingSession::new(Role::Responder, schedule)))
    }

    fn accept_client_init_inner(&mut self, message: &[u8]) -> Result<(), CryptoError> {
        if self.state != ResponderState::Start {
            return Err(CryptoError::Handshake {
                code: AlertCode::IncorrectMessage,
                detail: "handshake message received out of order",
            });
        }

        let mut reader = open_message(message, TAG_CLIENT_INIT)?;
        let msg = wire::decode_client_init(&mut reader)?;
        reader.finish()?;

        if !self.config.accepts_suite(msg.suite) {
            return Err(CryptoError::Handshake {
                code: AlertCode::BadHandshakeCipher,
                detail: "no mutually supported cipher suite",
            });
        }
        if !self.config.accepts_protocol(&msg.next_protocol) {
            return Err(CryptoError::Handshake {
                code: AlertCode::BadNextProtocol,
                detail: "unsupported next protocol",
            });
        }

        self.suite = msg.suite;
        self.peer_random = msg.random;
        self.peer_commitment = msg.commitment;
        self.transcript.extend_from_slice(message);
        self.state = ResponderState::ReceivedClientInit;
        Ok(())
    }

    fn server_init_inner(&mut self) -> Result<Vec<u8>, CryptoError> {
        if self.state != ResponderState::ReceivedClientInit {
            return Err(CryptoError::Handshake {
                code: AlertCode::IncorrectMessage,
                detail: "handshake message received out of order",
            });
        }

        let rng = SystemRandom::new();
        rng.fill(&mut self.random)
            .map_err(|_| CryptoError::Random)?;

        let msg = wire::ServerInit {
            random: self.random,
            suite: self.suite,
            // Committing to our own key *after* seeing the client's commitment
            // but *before* seeing the client's key. This is what makes the
            // commitment mutual: we are bound before we learn pk_c.
            commitment: commitment(
                DOMAIN_COMMIT_SERVER,
                self.suite,
                &self.random,
                &self.public_key,
            ),
        };
        let encoded = wire::encode_server_init(&msg);
        self.transcript.extend_from_slice(&encoded);
        self.state = ResponderState::SentServerInit;
        Ok(encoded)
    }

    fn accept_client_finished_inner(&mut self, message: &[u8]) -> Result<(), CryptoError> {
        if self.state != ResponderState::SentServerInit {
            return Err(CryptoError::Handshake {
                code: AlertCode::IncorrectMessage,
                detail: "handshake message received out of order",
            });
        }

        let mut reader = open_message(message, TAG_CLIENT_FINISHED)?;
        let msg = wire::decode_client_finished(&mut reader)?;
        reader.finish()?;

        let expected = commitment(
            DOMAIN_COMMIT_CLIENT,
            self.suite,
            &self.peer_random,
            &msg.public_key,
        );
        if !commitments_equal(&expected, &self.peer_commitment) {
            return Err(CryptoError::Handshake {
                code: AlertCode::CommitmentMismatch,
                detail: "client public key does not match its commitment",
            });
        }

        self.peer_public_key = msg.public_key;
        self.transcript.extend_from_slice(message);
        self.state = ResponderState::ReceivedClientFinished;
        Ok(())
    }
}

/// Validates the envelope of an incoming handshake message and returns a reader
/// positioned just past the tag byte.
fn open_message<'a>(message: &'a [u8], expected_tag: u8) -> Result<Reader<'a>, CryptoError> {
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(CryptoError::Encoding(
            "handshake message exceeds maximum size",
        ));
    }
    let mut reader = Reader::new(message);
    let tag = reader.tag()?;
    if tag != expected_tag {
        return Err(CryptoError::Handshake {
            code: AlertCode::IncorrectMessage,
            detail: "unexpected handshake message type",
        });
    }
    Ok(reader)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn client_and_server_commitments_are_domain_separated() {
        let random = [0x11_u8; NONCE_LEN];
        let pk = [0x04_u8; P256_PUBLIC_KEY_LEN];
        let client = commitment(DOMAIN_COMMIT_CLIENT, CipherSuite::P256Sha512, &random, &pk);
        let server = commitment(DOMAIN_COMMIT_SERVER, CipherSuite::P256Sha512, &random, &pk);
        assert_ne!(client, server);
    }

    #[test]
    fn commitment_binds_every_input() {
        let random = [0x11_u8; NONCE_LEN];
        let other_random = [0x12_u8; NONCE_LEN];
        let pk = [0x04_u8; P256_PUBLIC_KEY_LEN];

        let base = commitment(DOMAIN_COMMIT_CLIENT, CipherSuite::P256Sha512, &random, &pk);

        let different_nonce = commitment(
            DOMAIN_COMMIT_CLIENT,
            CipherSuite::P256Sha512,
            &other_random,
            &pk,
        );
        let mut other_pk = pk;
        other_pk[10] ^= 0x01;
        let different_key = commitment(
            DOMAIN_COMMIT_CLIENT,
            CipherSuite::P256Sha512,
            &random,
            &other_pk,
        );

        assert_ne!(base, different_nonce);
        assert_ne!(base, different_key);
    }

    #[test]
    fn commitment_is_deterministic() {
        let random = [0x11_u8; NONCE_LEN];
        let pk = [0x04_u8; P256_PUBLIC_KEY_LEN];
        assert_eq!(
            commitment(DOMAIN_COMMIT_CLIENT, CipherSuite::P256Sha512, &random, &pk),
            commitment(DOMAIN_COMMIT_CLIENT, CipherSuite::P256Sha512, &random, &pk)
        );
    }

    #[test]
    fn out_of_order_calls_are_rejected_and_poison_the_object() {
        let mut initiator = Initiator::new(wire::NEXT_PROTOCOL_AEAD).expect("initiator");
        // M3 before M1.
        assert!(initiator.client_finished().is_err());
        // The object is now unusable, even for the step that would have been
        // legal from the original state.
        assert!(initiator.client_init().is_err());
    }

    #[test]
    fn oversize_message_is_rejected_before_parsing() {
        let mut initiator = Initiator::new(wire::NEXT_PROTOCOL_AEAD).expect("initiator");
        initiator.client_init().expect("m1");
        let oversize = vec![0_u8; MAX_MESSAGE_BYTES + 1];
        assert!(matches!(
            initiator.accept_server_init(&oversize),
            Err(CryptoError::Encoding(_))
        ));
    }

    #[test]
    fn invalid_protocol_identifier_is_rejected_at_construction() {
        assert!(Initiator::new("").is_err());
        assert!(Initiator::new(&"x".repeat(MAX_NEXT_PROTOCOL_LEN + 1)).is_err());
    }
}
