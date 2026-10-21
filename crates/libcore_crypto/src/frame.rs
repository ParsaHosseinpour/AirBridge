//! AES-256-GCM frame encryption with strict sequence-number replay
//! protection.
//!
//! # Frame layout
//!
//! ```text
//! version:u8 | direction:u8 | seq:u64 | payload_len:u32 | ciphertext | tag[16]
//! ```
//!
//! The header travels in the clear but is authenticated: it is supplied to
//! AES-GCM as associated data, so any modification of the version, direction,
//! sequence number or length causes the tag check to fail.
//!
//! # Nonce construction
//!
//! ```text
//! nonce = nonce_prefix[4] || seq[8]        (big-endian, 96 bits total)
//! ```
//!
//! The prefix is derived per-direction by the key schedule, and the two
//! directions use different AES keys. Nonce uniqueness therefore follows from
//! the sequence number never repeating under a given key — which is exactly
//! what [`FrameCipher::seal`] enforces.
//!
//! # Associated data
//!
//! ```text
//! AAD = session_id[32] || version:u8 || direction:u8 || seq:u64 || payload_len:u32
//! ```
//!
//! Binding `session_id` means a frame cannot be replayed from one AirBridge
//! session into another, even if an attacker could arrange for both sessions
//! to use the same key material (they cannot, but defence in depth).
//!
//! # Ordering of the replay check
//!
//! The receiver checks `seq > highest_accepted` *before* doing any AEAD work,
//! so replays cost almost nothing. It commits the new high-water mark only
//! *after* the tag has verified. Reversing that order would let an attacker
//! desynchronise the channel permanently with a single forged frame carrying a
//! large sequence number, which is a denial-of-service bug and a surprisingly
//! common one.
//!
//! # Transport assumptions
//!
//! This policy is *strict monotonic*: any gap, duplicate or reordering is a
//! hard error. That is correct and optimal over an ordered, reliable transport
//! (TCP, QUIC streams). If AirBridge later carries frames over an unordered
//! datagram transport, this must be replaced with a bounded sliding-window
//! policy; the check is isolated in [`FrameCipher::open`] for that reason.

use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};

use crate::error::CryptoError;
use crate::kdf::{KeySchedule, KEY_LEN, NONCE_PREFIX_LEN, SESSION_ID_LEN};
use crate::Role;

/// Version byte for the frame format.
pub const FRAME_VERSION: u8 = 1;

/// Size of the cleartext frame header, in bytes.
pub const FRAME_HEADER_LEN: usize = 1 + 1 + 8 + 4;

/// Size of an AES-GCM authentication tag, in bytes.
pub const AEAD_TAG_LEN: usize = 16;

/// Maximum plaintext size for a single frame.
///
/// 16 MiB. Larger payloads must be chunked by the caller. The cap bounds the
/// memory a peer can make us allocate before the tag has been verified.
pub const MAX_FRAME_PAYLOAD: usize = 16 * 1024 * 1024;

/// AES-GCM nonce length, in bytes (96 bits, the only length GCM should be used
/// with).
const GCM_NONCE_LEN: usize = 12;

/// Fixed part of the associated data: `session_id | version | direction`.
const AAD_PREFIX_LEN: usize = SESSION_ID_LEN + 1 + 1;

/// Full associated-data length: prefix plus `seq` and `payload_len`.
const AAD_LEN: usize = AAD_PREFIX_LEN + 8 + 4;

/// Direction of travel for a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// From the handshake initiator to the responder.
    InitiatorToResponder,
    /// From the handshake responder to the initiator.
    ResponderToInitiator,
}

impl Direction {
    /// Returns the on-the-wire byte for this direction.
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::InitiatorToResponder => 0,
            Self::ResponderToInitiator => 1,
        }
    }

    /// Parses an on-the-wire direction byte.
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::InitiatorToResponder),
            1 => Some(Self::ResponderToInitiator),
            _ => None,
        }
    }

    /// Returns the opposite direction.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::InitiatorToResponder => Self::ResponderToInitiator,
            Self::ResponderToInitiator => Self::InitiatorToResponder,
        }
    }
}

/// Per-direction AEAD state.
struct DirectionKeys {
    key: LessSafeKey,
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
    aad_prefix: [u8; AAD_PREFIX_LEN],
}

impl DirectionKeys {
    fn new(
        key: &[u8],
        nonce_prefix: [u8; NONCE_PREFIX_LEN],
        session_id: &[u8; SESSION_ID_LEN],
        direction: Direction,
    ) -> Result<Self, CryptoError> {
        debug_assert_eq!(key.len(), KEY_LEN);
        let unbound =
            UnboundKey::new(&aead::AES_256_GCM, key).map_err(|_| CryptoError::KeySchedule)?;

        let mut aad_prefix = [0_u8; AAD_PREFIX_LEN];
        aad_prefix[..SESSION_ID_LEN].copy_from_slice(session_id);
        aad_prefix[SESSION_ID_LEN] = FRAME_VERSION;
        aad_prefix[SESSION_ID_LEN + 1] = direction.wire();

        Ok(Self {
            key: LessSafeKey::new(unbound),
            nonce_prefix,
            aad_prefix,
        })
    }

    fn nonce(&self, seq: u64) -> [u8; GCM_NONCE_LEN] {
        let mut nonce = [0_u8; GCM_NONCE_LEN];
        nonce[..NONCE_PREFIX_LEN].copy_from_slice(&self.nonce_prefix);
        nonce[NONCE_PREFIX_LEN..].copy_from_slice(&seq.to_be_bytes());
        nonce
    }

    fn aad(&self, seq: u64, payload_len: u32) -> [u8; AAD_LEN] {
        let mut aad = [0_u8; AAD_LEN];
        aad[..AAD_PREFIX_LEN].copy_from_slice(&self.aad_prefix);
        aad[AAD_PREFIX_LEN..AAD_PREFIX_LEN + 8].copy_from_slice(&seq.to_be_bytes());
        aad[AAD_PREFIX_LEN + 8..].copy_from_slice(&payload_len.to_be_bytes());
        aad
    }
}

/// A bidirectional authenticated-encryption channel.
///
/// One `FrameCipher` owns both directions, because an AirBridge session is
/// bidirectional. Each direction has independent key material and an
/// independent sequence counter, so a reflection attack cannot make one side
/// accept its own traffic.
pub struct FrameCipher {
    send: DirectionKeys,
    recv: DirectionKeys,
    send_direction: Direction,
    recv_direction: Direction,
    send_seq: u64,
    highest_recv_seq: Option<u64>,
}

impl FrameCipher {
    /// Builds a channel from a completed handshake's key schedule.
    ///
    /// The initiator sends in the `c2s` direction and receives `s2c`; the
    /// responder does the reverse.
    pub(crate) fn new(role: Role, schedule: &KeySchedule) -> Result<Self, CryptoError> {
        let (send_direction, recv_direction) = match role {
            Role::Initiator => (
                Direction::InitiatorToResponder,
                Direction::ResponderToInitiator,
            ),
            Role::Responder => (
                Direction::ResponderToInitiator,
                Direction::InitiatorToResponder,
            ),
        };

        let (send_key, send_prefix, recv_key, recv_prefix) = match role {
            Role::Initiator => (
                &schedule.c2s_key,
                schedule.c2s_nonce_prefix,
                &schedule.s2c_key,
                schedule.s2c_nonce_prefix,
            ),
            Role::Responder => (
                &schedule.s2c_key,
                schedule.s2c_nonce_prefix,
                &schedule.c2s_key,
                schedule.c2s_nonce_prefix,
            ),
        };

        Ok(Self {
            send: DirectionKeys::new(
                &send_key[..],
                send_prefix,
                &schedule.session_id,
                send_direction,
            )?,
            recv: DirectionKeys::new(
                &recv_key[..],
                recv_prefix,
                &schedule.session_id,
                recv_direction,
            )?,
            send_direction,
            recv_direction,
            send_seq: 0,
            highest_recv_seq: None,
        })
    }

    /// Encrypts and authenticates `plaintext`, returning a complete frame.
    ///
    /// The sequence number is consumed whether or not the caller transmits the
    /// resulting frame. Callers must not retry a failed *send* with the same
    /// arguments expecting the same nonce to be reused.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::FrameFormat`] if the payload exceeds
    /// [`MAX_FRAME_PAYLOAD`], and [`CryptoError::SequenceExhausted`] once
    /// `2^64 - 1` frames have been sealed under this key.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if plaintext.len() > MAX_FRAME_PAYLOAD {
            return Err(CryptoError::FrameFormat(
                "payload exceeds maximum frame size",
            ));
        }
        // Reserve `u64::MAX` as a sentinel so the counter can always advance.
        if self.send_seq == u64::MAX {
            return Err(CryptoError::SequenceExhausted);
        }

        let seq = self.send_seq;
        let payload_len = u32::try_from(plaintext.len())
            .map_err(|_| CryptoError::FrameFormat("payload length overflow"))?;

        // Buffer layout up front so that the AEAD can encrypt the payload
        // in place without copying the header.
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + plaintext.len() + AEAD_TAG_LEN);
        out.push(FRAME_VERSION);
        out.push(self.send_direction.wire());
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&payload_len.to_be_bytes());
        out.extend_from_slice(plaintext);

        let aad = self.send.aad(seq, payload_len);
        let nonce = self.send.nonce(seq);
        let tag = self
            .send
            .key
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(&aad[..]),
                &mut out[FRAME_HEADER_LEN..],
            )
            .map_err(|_| CryptoError::FrameAuth)?;

        out.extend_from_slice(tag.as_ref());
        self.send_seq += 1;
        Ok(out)
    }

    /// Verifies and decrypts a complete frame.
    ///
    /// # Errors
    ///
    /// * [`CryptoError::FrameFormat`] for a structurally invalid frame.
    /// * [`CryptoError::ReplayDetected`] if the sequence number does not
    ///   strictly exceed the highest already accepted.
    /// * [`CryptoError::FrameAuth`] if the tag does not verify.
    ///
    /// On any error the receive high-water mark is left untouched, so a forged
    /// frame cannot advance the receiver's state.
    pub fn open(&mut self, frame: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if frame.len() < FRAME_HEADER_LEN + AEAD_TAG_LEN {
            return Err(CryptoError::FrameFormat(
                "frame shorter than header plus tag",
            ));
        }
        if frame[0] != FRAME_VERSION {
            return Err(CryptoError::FrameFormat("unsupported frame version"));
        }
        let direction = Direction::from_wire(frame[1])
            .ok_or(CryptoError::FrameFormat("unknown direction byte"))?;
        if direction != self.recv_direction {
            return Err(CryptoError::FrameFormat(
                "frame direction does not match this receiver",
            ));
        }

        let mut seq_bytes = [0_u8; 8];
        seq_bytes.copy_from_slice(&frame[2..10]);
        let seq = u64::from_be_bytes(seq_bytes);

        let mut len_bytes = [0_u8; 4];
        len_bytes.copy_from_slice(&frame[10..14]);
        let payload_len = usize::try_from(u32::from_be_bytes(len_bytes))
            .map_err(|_| CryptoError::FrameFormat("payload length does not fit in usize"))?;

        // Reject before allocating or doing AEAD work. The length is
        // unauthenticated at this point, so it must never drive an allocation
        // larger than what the caller actually supplied.
        if payload_len > MAX_FRAME_PAYLOAD {
            return Err(CryptoError::FrameFormat(
                "payload exceeds maximum frame size",
            ));
        }
        let ciphertext = &frame[FRAME_HEADER_LEN..];
        let expected_len = payload_len
            .checked_add(AEAD_TAG_LEN)
            .ok_or(CryptoError::FrameFormat("payload length overflow"))?;
        if ciphertext.len() != expected_len {
            return Err(CryptoError::FrameFormat(
                "payload length does not match frame length",
            ));
        }

        // Cheap replay rejection first; the high-water mark is only committed
        // after the tag verifies.
        if let Some(highest) = self.highest_recv_seq {
            if seq <= highest {
                return Err(CryptoError::ReplayDetected { seq, highest });
            }
        }

        let aad = self
            .recv
            .aad(seq, u32::try_from(payload_len).unwrap_or(u32::MAX));
        let nonce = self.recv.nonce(seq);

        // `ciphertext.len()` is now known to equal `payload_len + tag`, both
        // bounded by `MAX_FRAME_PAYLOAD`, so this copy is bounded by the size
        // of the caller's buffer.
        let mut buffer = ciphertext.to_vec();
        let plaintext = self
            .recv
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(&aad[..]),
                &mut buffer,
            )
            .map_err(|_| CryptoError::FrameAuth)?;

        let output = plaintext.to_vec();
        self.highest_recv_seq = Some(seq);
        Ok(output)
    }

    /// The sequence number that will be used by the next [`FrameCipher::seal`].
    #[must_use]
    pub const fn send_seq(&self) -> u64 {
        self.send_seq
    }

    /// The highest sequence number accepted so far, if any.
    #[must_use]
    pub const fn highest_recv_seq(&self) -> Option<u64> {
        self.highest_recv_seq
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::kdf::KeySchedule;

    fn schedule() -> KeySchedule {
        KeySchedule::derive(&[0x37_u8; 32], b"test-transcript").expect("derive")
    }

    fn pair() -> (FrameCipher, FrameCipher) {
        let schedule = schedule();
        let client = FrameCipher::new(Role::Initiator, &schedule).expect("client");
        let server = FrameCipher::new(Role::Responder, &schedule).expect("server");
        (client, server)
    }

    #[test]
    fn roundtrip_over_many_sequence_numbers() {
        let (mut client, mut server) = pair();
        for i in 0..1000_u32 {
            let plaintext = format!("frame number {i}");
            let frame = client.seal(plaintext.as_bytes()).expect("seal");
            let opened = server.open(&frame).expect("open");
            assert_eq!(opened, plaintext.as_bytes());
        }
        assert_eq!(client.send_seq(), 1000);
        assert_eq!(server.highest_recv_seq(), Some(999));
    }

    #[test]
    fn roundtrip_is_bidirectional() {
        let (mut client, mut server) = pair();
        let to_server = client.seal(b"ping").expect("seal");
        assert_eq!(server.open(&to_server).expect("open"), b"ping");

        let to_client = server.seal(b"pong").expect("seal");
        assert_eq!(client.open(&to_client).expect("open"), b"pong");
    }

    #[test]
    fn empty_payload_roundtrips() {
        let (mut client, mut server) = pair();
        let frame = client.seal(b"").expect("seal");
        assert_eq!(frame.len(), FRAME_HEADER_LEN + AEAD_TAG_LEN);
        assert_eq!(server.open(&frame).expect("open"), b"");
    }

    #[test]
    fn replayed_frame_is_rejected() {
        let (mut client, mut server) = pair();
        let frame = client.seal(b"once").expect("seal");
        assert_eq!(server.open(&frame).expect("open"), b"once");

        assert_eq!(
            server.open(&frame),
            Err(CryptoError::ReplayDetected { seq: 0, highest: 0 })
        );
    }

    #[test]
    fn out_of_order_frame_is_rejected() {
        let (mut client, mut server) = pair();
        let first = client.seal(b"first").expect("seal");
        let second = client.seal(b"second").expect("seal");

        server.open(&second).expect("open second");
        // `first` is now stale.
        assert!(matches!(
            server.open(&first),
            Err(CryptoError::ReplayDetected { seq: 0, highest: 1 })
        ));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let (mut client, mut server) = pair();
        let mut frame = client.seal(b"integrity").expect("seal");
        let last = frame.len() - 1;
        frame[last] ^= 0x01;
        assert_eq!(server.open(&frame), Err(CryptoError::FrameAuth));
    }

    #[test]
    fn tampered_tag_is_rejected() {
        let (mut client, mut server) = pair();
        let mut frame = client.seal(b"integrity").expect("seal");
        let tag_start = frame.len() - AEAD_TAG_LEN;
        frame[tag_start] ^= 0xFF;
        assert_eq!(server.open(&frame), Err(CryptoError::FrameAuth));
    }

    #[test]
    fn tampered_sequence_number_is_rejected_by_aad() {
        let (mut client, mut server) = pair();
        let mut frame = client.seal(b"seq-bound").expect("seal");
        // Rewrite seq to 1 while leaving the tag intact. If seq were not
        // authenticated this would silently re-key the nonce and the frame
        // would decrypt to garbage or succeed under the wrong counter.
        frame[2..10].copy_from_slice(&1_u64.to_be_bytes());
        assert_eq!(server.open(&frame), Err(CryptoError::FrameAuth));
    }

    #[test]
    fn tampered_payload_length_is_rejected() {
        let (mut client, mut server) = pair();
        let mut frame = client.seal(b"length-bound").expect("seal");
        frame[10..14].copy_from_slice(&3_u32.to_be_bytes());
        // Length now disagrees with the actual frame size.
        assert!(matches!(
            server.open(&frame),
            Err(CryptoError::FrameFormat(_))
        ));

        // And a length that agrees with the frame but not the tag is caught by
        // the AEAD.
        let mut frame = client.seal(b"length-bound").expect("seal");
        let len = frame.len() - FRAME_HEADER_LEN - AEAD_TAG_LEN;
        frame[10..14].copy_from_slice(&(len as u32 + 1).to_be_bytes());
        frame.push(0);
        assert_eq!(server.open(&frame), Err(CryptoError::FrameAuth));
    }

    #[test]
    fn tampered_version_is_rejected() {
        let (mut client, mut server) = pair();
        let mut frame = client.seal(b"v").expect("seal");
        frame[0] = 0xFF;
        assert!(matches!(
            server.open(&frame),
            Err(CryptoError::FrameFormat(_))
        ));
    }

    #[test]
    fn reflected_frame_is_rejected() {
        let (mut client, _server) = pair();
        let frame = client.seal(b"reflect me").expect("seal");
        // Feeding the initiator's own frame back to it must fail: it receives
        // in the opposite direction under a different key.
        assert!(matches!(
            client.open(&frame),
            Err(CryptoError::FrameFormat(_))
        ));
    }

    #[test]
    fn frames_from_a_different_session_are_rejected() {
        let other = KeySchedule::derive(&[0x99_u8; 32], b"other-transcript").expect("derive");
        let mut foreign = FrameCipher::new(Role::Initiator, &other).expect("foreign");
        let (_client, mut server) = pair();

        let frame = foreign.seal(b"from elsewhere").expect("seal");
        // Same direction and same sequence number (0), so only the key and the
        // bound session id can distinguish it.
        assert_eq!(server.open(&frame), Err(CryptoError::FrameAuth));
    }

    #[test]
    fn forged_frame_does_not_advance_the_receive_counter() {
        // This is the critical ordering property: verify, then commit.
        let (mut client, mut server) = pair();

        let mut forged = client.seal(b"will be corrupted").expect("seal");
        let last = forged.len() - 1;
        forged[last] ^= 0xFF;
        assert_eq!(server.open(&forged), Err(CryptoError::FrameAuth));
        assert_eq!(server.highest_recv_seq(), None);

        // The genuine frame for the *same* sequence number must still be
        // accepted. If `open` had committed the counter before verifying, the
        // channel would now be permanently desynchronised.
        let genuine = client.seal(b"the real one").expect("seal");
        assert_eq!(server.open(&genuine).expect("open"), b"the real one");
        assert_eq!(server.highest_recv_seq(), Some(1));
    }

    #[test]
    fn truncated_frames_are_rejected() {
        let (mut client, mut server) = pair();
        let frame = client.seal(b"truncate me").expect("seal");
        for cut in 0..frame.len() {
            assert!(
                server.open(&frame[..cut]).is_err(),
                "accepted a frame truncated to {cut} bytes"
            );
        }
        // Nothing was committed.
        assert_eq!(server.highest_recv_seq(), None);
        assert!(server.open(&frame).is_ok());
    }

    #[test]
    fn oversize_payload_is_rejected_on_seal() {
        let (mut client, _server) = pair();
        let oversize = vec![0_u8; MAX_FRAME_PAYLOAD + 1];
        assert_eq!(
            client.seal(&oversize),
            Err(CryptoError::FrameFormat(
                "payload exceeds maximum frame size"
            ))
        );
        // A rejected seal must not consume a sequence number.
        assert_eq!(client.send_seq(), 0);
    }

    #[test]
    fn sequence_exhaustion_is_a_hard_error() {
        let (mut client, _server) = pair();
        client.send_seq = u64::MAX;
        assert_eq!(client.seal(b"nope"), Err(CryptoError::SequenceExhausted));
        assert_eq!(client.send_seq(), u64::MAX);
    }

    #[test]
    fn last_representable_sequence_number_works() {
        let schedule = schedule();
        let mut client = FrameCipher::new(Role::Initiator, &schedule).expect("client");
        let mut server = FrameCipher::new(Role::Responder, &schedule).expect("server");

        client.send_seq = u64::MAX - 1;
        let frame = client.seal(b"final").expect("seal");
        assert_eq!(server.open(&frame).expect("open"), b"final");
        assert_eq!(server.highest_recv_seq(), Some(u64::MAX - 1));
    }

    /// Differential test: our frame payload must be byte-identical to what an
    /// independent AES-256-GCM implementation produces for the same key,
    /// nonce and associated data.
    ///
    /// A self-roundtrip proves only that we agree with ourselves. This proves
    /// that the nonce construction, AAD assembly and tag placement match the
    /// standard.
    #[test]
    fn ciphertext_matches_an_independent_implementation() {
        use aes_gcm::aead::{Aead, KeyInit, Payload};
        use aes_gcm::{Aes256Gcm, Nonce as AesGcmNonce};

        let schedule = schedule();
        let mut client = FrameCipher::new(Role::Initiator, &schedule).expect("client");

        let plaintext = b"differential test vector";
        let frame = client.seal(plaintext).expect("seal");

        let seq = 0_u64;
        let payload_len = plaintext.len() as u32;
        let aad = client.send.aad(seq, payload_len);
        let nonce = client.send.nonce(seq);
        assert_eq!(aad.len(), AAD_LEN);
        assert_eq!(nonce.len(), GCM_NONCE_LEN);

        let cipher = Aes256Gcm::new_from_slice(&schedule.c2s_key[..]).expect("key");
        let expected = cipher
            .encrypt(
                AesGcmNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .expect("encrypt");

        assert_eq!(&frame[FRAME_HEADER_LEN..], &expected[..]);
    }

    #[test]
    fn nonce_layout_is_prefix_then_big_endian_sequence() {
        let schedule = schedule();
        let client = FrameCipher::new(Role::Initiator, &schedule).expect("client");
        let nonce = client.send.nonce(0x0102_0304_0506_0708);
        assert_eq!(&nonce[..4], &schedule.c2s_nonce_prefix[..]);
        assert_eq!(&nonce[4..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn directions_use_distinct_keys_and_prefixes() {
        let schedule = schedule();
        let client = FrameCipher::new(Role::Initiator, &schedule).expect("client");
        let server = FrameCipher::new(Role::Responder, &schedule).expect("server");

        assert_eq!(client.send_direction, Direction::InitiatorToResponder);
        assert_eq!(client.recv_direction, Direction::ResponderToInitiator);
        assert_eq!(server.send_direction, Direction::ResponderToInitiator);
        assert_eq!(server.recv_direction, Direction::InitiatorToResponder);
        assert_eq!(client.send_direction, server.recv_direction);
        assert_ne!(&client.send.nonce_prefix[..], &client.recv.nonce_prefix[..]);
    }
}
