//! Canonical wire encoding for handshake messages.
//!
//! # Why not protobuf
//!
//! UKEY2 is specified in protobuf, and its client commitment is a hash of the
//! *raw serialized bytes* of the `ClientFinished` message. Protobuf has no
//! canonical encoding: the same logical message admits many valid byte
//! representations (field order, varint width, unknown fields, packed vs
//! unpacked repeated fields). Any peer that parses and then re-serialises
//! before hashing computes a different commitment, and — more importantly — an
//! attacker can mutate bytes without changing the parsed message. That is a
//! textbook commitment-malleability bug.
//!
//! Google avoids it by retaining the raw bytes and never re-serialising. We
//! remove the hazard instead: every message here has exactly one encoding, the
//! decoder rejects trailing bytes and unknown tags, and the commitment is
//! defined over that canonical form.
//!
//! The trade-off is that AirBridge is **not** byte-compatible with Google's
//! protobuf framing. See the crate-level docs for the interoperability note.
//!
//! # Layout
//!
//! All integers are big-endian. Every message starts with a one-byte tag.
//!
//! ```text
//! ClientInit      (0x01): tag | version:u32 | random:[32] | suite:u16
//!                              | next_protocol:lpp | commitment:[64]
//! ServerInit      (0x02): tag | version:u32 | random:[32] | suite:u16
//!                              | commitment:[64]
//! ClientFinished  (0x03): tag | public_key:lpp
//! ServerFinished  (0x04): tag | public_key:lpp
//! Alert           (0x05): tag | code:u16 | message:lpp
//! ```
//!
//! `lpp` is a `u16` big-endian length followed by exactly that many bytes.

use crate::error::{AlertCode, CryptoError};

/// Tag byte for a `ClientInit` message.
pub(crate) const TAG_CLIENT_INIT: u8 = 0x01;
/// Tag byte for a `ServerInit` message.
pub(crate) const TAG_SERVER_INIT: u8 = 0x02;
/// Tag byte for a `ClientFinished` message.
pub(crate) const TAG_CLIENT_FINISHED: u8 = 0x03;
/// Tag byte for a `ServerFinished` message.
pub(crate) const TAG_SERVER_FINISHED: u8 = 0x04;
/// Tag byte for an `Alert` message.
pub(crate) const TAG_ALERT: u8 = 0x05;

/// The only protocol version this implementation speaks.
pub(crate) const PROTOCOL_VERSION: u32 = 1;

/// Length of the per-party nonce, in bytes. UKEY2 fixes this at 32.
pub(crate) const NONCE_LEN: usize = 32;

/// Length of a SHA-512 commitment, in bytes.
pub(crate) const COMMITMENT_LEN: usize = 64;

/// Length of an uncompressed P-256 public key (`0x04 || X || Y`).
pub(crate) const P256_PUBLIC_KEY_LEN: usize = 65;

/// Upper bound on the encoded size of any handshake message.
///
/// A `ClientInit` with a maximal protocol string is under 400 bytes, so this is
/// generous. The cap exists so that a peer cannot make us buffer unbounded data
/// during the handshake, before any key is established.
pub(crate) const MAX_MESSAGE_BYTES: usize = 1024;

/// Maximum length of a `next_protocol` identifier.
pub(crate) const MAX_NEXT_PROTOCOL_LEN: usize = 255;

/// The next-protocol identifier used by the AirBridge AEAD frame layer.
pub const NEXT_PROTOCOL_AEAD: &str = "AIRBRIDGE_V1_AES256GCM";

/// A UKEY2 handshake cipher suite.
///
/// Only P-256/SHA-512 is implemented. Curve25519/SHA-512 is reserved by the
/// UKEY2 specification (`200`) and is deliberately absent rather than stubbed,
/// so that a peer cannot negotiate a suite that silently does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CipherSuite {
    /// NIST P-256 for ECDH, SHA-512 for commitments and the key schedule.
    P256Sha512 = 100,
}

impl CipherSuite {
    /// Returns the on-the-wire numeric identifier.
    #[must_use]
    pub const fn wire_id(self) -> u16 {
        self as u16
    }

    /// Parses an on-the-wire identifier, returning `None` if unsupported.
    #[must_use]
    pub const fn from_wire(value: u16) -> Option<Self> {
        match value {
            100 => Some(Self::P256Sha512),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Encoding primitives
// ---------------------------------------------------------------------------

/// A buffer that only ever grows by whole, already-validated fields.
pub(crate) struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn new(tag: u8) -> Self {
        Self {
            buf: Vec::with_capacity(64),
        }
        .with_tag(tag)
    }

    fn with_tag(mut self, tag: u8) -> Self {
        self.buf.push(tag);
        self
    }

    pub(crate) fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Writes a `u16` length prefix followed by `bytes`.
    pub(crate) fn length_prefixed(&mut self, bytes: &[u8]) -> Result<(), CryptoError> {
        let len = u16::try_from(bytes.len())
            .map_err(|_| CryptoError::Encoding("field too long to encode"))?;
        self.u16(len);
        self.raw(bytes);
        Ok(())
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// A cursor that cannot read past the end of its buffer.
///
/// Every length is validated against the number of bytes actually remaining
/// *before* it is used to slice, so a hostile `u16` length of `0xFFFF` costs
/// nothing and allocates nothing.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CryptoError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(CryptoError::Encoding("length overflow"))?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(CryptoError::Encoding("truncated message"))?;
        self.pos = end;
        Ok(slice)
    }

    pub(crate) fn tag(&mut self) -> Result<u8, CryptoError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, CryptoError> {
        let raw = self.take(2)?;
        Ok(u16::from_be_bytes([raw[0], raw[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, CryptoError> {
        let raw = self.take(4)?;
        Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], CryptoError> {
        let raw = self.take(N)?;
        let mut out = [0_u8; N];
        out.copy_from_slice(raw);
        Ok(out)
    }

    pub(crate) fn length_prefixed(&mut self) -> Result<&'a [u8], CryptoError> {
        let len = usize::from(self.u16()?);
        self.take(len)
    }

    /// Asserts that the buffer was consumed exactly.
    ///
    /// Trailing bytes are rejected rather than ignored: otherwise an attacker
    /// could append data that a future parser revision might interpret
    /// differently, changing the meaning of a transcript that was already
    /// committed to.
    pub(crate) fn finish(self) -> Result<(), CryptoError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(CryptoError::Encoding("trailing bytes after message"))
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// The client's first message (UKEY2 `M1`).
pub(crate) struct ClientInit {
    pub(crate) random: [u8; NONCE_LEN],
    pub(crate) suite: CipherSuite,
    pub(crate) commitment: [u8; COMMITMENT_LEN],
    pub(crate) next_protocol: String,
}

/// The server's response (UKEY2 `M2`).
pub(crate) struct ServerInit {
    pub(crate) random: [u8; NONCE_LEN],
    pub(crate) suite: CipherSuite,
    pub(crate) commitment: [u8; COMMITMENT_LEN],
}

/// The client's key reveal (UKEY2 `M3`).
pub(crate) struct ClientFinished {
    pub(crate) public_key: Vec<u8>,
}

/// The server's key reveal (AirBridge extension, `M4`).
pub(crate) struct ServerFinished {
    pub(crate) public_key: Vec<u8>,
}

pub(crate) fn encode_client_init(msg: &ClientInit) -> Result<Vec<u8>, CryptoError> {
    if msg.next_protocol.len() > MAX_NEXT_PROTOCOL_LEN {
        return Err(CryptoError::Encoding("next_protocol is too long"));
    }
    let mut w = Writer::new(TAG_CLIENT_INIT);
    w.u32(PROTOCOL_VERSION);
    w.raw(&msg.random);
    w.u16(msg.suite.wire_id());
    w.length_prefixed(msg.next_protocol.as_bytes())?;
    w.raw(&msg.commitment);
    Ok(w.finish())
}

pub(crate) fn decode_client_init(r: &mut Reader<'_>) -> Result<ClientInit, CryptoError> {
    let version = r.u32()?;
    if version != PROTOCOL_VERSION {
        return Err(CryptoError::Handshake {
            code: AlertCode::BadVersion,
            detail: "ClientInit version mismatch",
        });
    }
    let random = r.array::<NONCE_LEN>()?;
    let suite = decode_suite(r.u16()?)?;
    let protocol_bytes = r.length_prefixed()?;
    let next_protocol = core::str::from_utf8(protocol_bytes)
        .map_err(|_| CryptoError::Handshake {
            code: AlertCode::BadNextProtocol,
            detail: "next_protocol is not valid UTF-8",
        })?
        .to_owned();
    let commitment = r.array::<COMMITMENT_LEN>()?;
    Ok(ClientInit {
        random,
        suite,
        commitment,
        next_protocol,
    })
}

pub(crate) fn encode_server_init(msg: &ServerInit) -> Vec<u8> {
    let mut w = Writer::new(TAG_SERVER_INIT);
    w.u32(PROTOCOL_VERSION);
    w.raw(&msg.random);
    w.u16(msg.suite.wire_id());
    w.raw(&msg.commitment);
    w.finish()
}

pub(crate) fn decode_server_init(r: &mut Reader<'_>) -> Result<ServerInit, CryptoError> {
    let version = r.u32()?;
    if version != PROTOCOL_VERSION {
        return Err(CryptoError::Handshake {
            code: AlertCode::BadVersion,
            detail: "ServerInit version mismatch",
        });
    }
    let random = r.array::<NONCE_LEN>()?;
    let suite = decode_suite(r.u16()?)?;
    let commitment = r.array::<COMMITMENT_LEN>()?;
    Ok(ServerInit {
        random,
        suite,
        commitment,
    })
}

pub(crate) fn encode_client_finished(msg: &ClientFinished) -> Result<Vec<u8>, CryptoError> {
    let mut w = Writer::new(TAG_CLIENT_FINISHED);
    w.length_prefixed(&msg.public_key)?;
    Ok(w.finish())
}

pub(crate) fn decode_client_finished(r: &mut Reader<'_>) -> Result<ClientFinished, CryptoError> {
    let public_key = r.length_prefixed()?.to_vec();
    Ok(ClientFinished { public_key })
}

pub(crate) fn encode_server_finished(msg: &ServerFinished) -> Result<Vec<u8>, CryptoError> {
    let mut w = Writer::new(TAG_SERVER_FINISHED);
    w.length_prefixed(&msg.public_key)?;
    Ok(w.finish())
}

pub(crate) fn decode_server_finished(r: &mut Reader<'_>) -> Result<ServerFinished, CryptoError> {
    let public_key = r.length_prefixed()?.to_vec();
    Ok(ServerFinished { public_key })
}

/// Encodes an alert.
///
/// Alerts are diagnostic: they never form part of the committed transcript, and
/// a truncated message is preferable to failing to tell the peer why we are
/// aborting.
pub(crate) fn encode_alert(code: AlertCode, message: &str) -> Vec<u8> {
    const MAX_ALERT_MESSAGE: usize = 200;
    let mut end = message.len().min(MAX_ALERT_MESSAGE);
    while !message.is_char_boundary(end) {
        end -= 1;
    }

    let mut w = Writer::new(TAG_ALERT);
    w.u16(code.as_u16());
    let length = u16::try_from(end).unwrap_or_default();
    w.u16(length);
    w.raw(&message.as_bytes()[..end]);
    w.finish()
}

fn decode_suite(value: u16) -> Result<CipherSuite, CryptoError> {
    CipherSuite::from_wire(value).ok_or(CryptoError::Handshake {
        code: AlertCode::BadHandshakeCipher,
        detail: "unsupported handshake cipher suite",
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sample_client_init() -> ClientInit {
        ClientInit {
            random: [0x11; NONCE_LEN],
            suite: CipherSuite::P256Sha512,
            commitment: [0x22; COMMITMENT_LEN],
            next_protocol: NEXT_PROTOCOL_AEAD.to_owned(),
        }
    }

    #[test]
    fn client_init_roundtrips() {
        let original = sample_client_init();
        let encoded = encode_client_init(&original).expect("encode");
        let mut reader = Reader::new(&encoded);
        assert_eq!(reader.tag().expect("tag"), TAG_CLIENT_INIT);
        let decoded = decode_client_init(&mut reader).expect("decode");
        reader.finish().expect("exact consumption");

        assert_eq!(decoded.random, original.random);
        assert_eq!(decoded.suite, original.suite);
        assert_eq!(decoded.commitment, original.commitment);
        assert_eq!(decoded.next_protocol, original.next_protocol);
    }

    #[test]
    fn encoding_is_canonical() {
        // The same logical message must always produce identical bytes, which
        // is the whole reason we are not using protobuf.
        let a = encode_client_init(&sample_client_init()).expect("encode");
        let b = encode_client_init(&sample_client_init()).expect("encode");
        assert_eq!(a, b);
    }

    #[test]
    fn truncated_message_is_rejected() {
        let encoded = encode_client_init(&sample_client_init()).expect("encode");
        for cut in 0..encoded.len() {
            let mut reader = Reader::new(&encoded[..cut]);
            let result = reader.tag().and_then(|_| decode_client_init(&mut reader));
            assert!(result.is_err(), "truncation at {cut} was accepted");
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut encoded = encode_client_init(&sample_client_init()).expect("encode");
        encoded.push(0x00);
        let mut reader = Reader::new(&encoded);
        reader.tag().expect("tag");
        decode_client_init(&mut reader).expect("decode");
        assert!(reader.finish().is_err());
    }

    #[test]
    fn hostile_length_prefix_does_not_allocate() {
        // Claim a 65535-byte protocol string but supply none.
        let mut bytes = vec![TAG_CLIENT_INIT];
        bytes.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        bytes.extend_from_slice(&[0xAA; NONCE_LEN]);
        bytes.extend_from_slice(&CipherSuite::P256Sha512.wire_id().to_be_bytes());
        bytes.extend_from_slice(&u16::MAX.to_be_bytes());
        let mut reader = Reader::new(&bytes);
        reader.tag().expect("tag");
        assert!(matches!(
            decode_client_init(&mut reader),
            Err(CryptoError::Encoding("truncated message"))
        ));
    }

    #[test]
    fn wrong_version_is_reported_as_bad_version() {
        let mut bytes = vec![TAG_CLIENT_INIT];
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        bytes.extend_from_slice(&[0u8; NONCE_LEN + 2 + COMMITMENT_LEN]);
        let mut reader = Reader::new(&bytes);
        reader.tag().expect("tag");
        assert!(matches!(
            decode_client_init(&mut reader),
            Err(CryptoError::Handshake {
                code: AlertCode::BadVersion,
                ..
            })
        ));
    }

    #[test]
    fn unknown_suite_is_rejected() {
        let mut bytes = vec![TAG_CLIENT_INIT];
        bytes.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        bytes.extend_from_slice(&[0u8; NONCE_LEN]);
        bytes.extend_from_slice(&0xDEAD_u16.to_be_bytes());
        let mut reader = Reader::new(&bytes);
        reader.tag().expect("tag");
        assert!(matches!(
            decode_client_init(&mut reader),
            Err(CryptoError::Handshake {
                code: AlertCode::BadHandshakeCipher,
                ..
            })
        ));
    }

    #[test]
    fn absurd_tag_is_reachable_but_not_decoded() {
        // The Reader is length-safe regardless of tag; the state machine is
        // responsible for rejecting unexpected tags.
        let mut reader = Reader::new(&[0xFF]);
        assert_eq!(reader.tag().expect("tag"), 0xFF);
        assert!(reader.finish().is_ok());
    }

    #[test]
    fn alerts_are_bounded_and_prefix_preserving() {
        let long = "alert ".repeat(1000);
        let encoded = encode_alert(AlertCode::InternalError, &long);

        // tag + code + length + at most 200 bytes of text
        assert!(encoded.len() <= 1 + 2 + 2 + 200);
        assert_eq!(encoded[0], TAG_ALERT);

        let mut reader = Reader::new(&encoded);
        reader.tag().expect("tag");
        assert_eq!(
            reader.u16().expect("code"),
            AlertCode::InternalError.as_u16()
        );
        let message = reader.length_prefixed().expect("message");
        reader.finish().expect("exact consumption");
        assert!(long.as_bytes().starts_with(message));
    }

    #[test]
    fn alert_truncation_respects_utf8_boundaries() {
        // Every prefix must remain valid UTF-8 even when the cut lands inside a
        // multi-byte character.
        for pad in 0..4 {
            let text = format!("{}€€€", "a".repeat(197 + pad));
            let encoded = encode_alert(AlertCode::BadMessage, &text);
            let mut reader = Reader::new(&encoded);
            reader.tag().expect("tag");
            reader.u16().expect("code");
            let message = reader.length_prefixed().expect("message");
            assert!(core::str::from_utf8(message).is_ok());
        }
    }
}
