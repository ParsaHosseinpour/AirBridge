//! Long-lived peer identity keys.
//!
//! The handshake authenticates a *channel*: two ephemeral P-256 keys and a
//! transcript. That is enough to stop a passive observer, but not enough to
//! remember a peer between sessions — the ephemeral key is different every
//! time. A stable identity key fills that gap.
//!
//! An [`Identity`] is an ECDSA P-256 key pair. During the handshake each party
//! includes its public key in its `Finished` message and signs the transcript
//! so far together with its own ephemeral public key. The responder's
//! transcript already contains the initiator's key, so its signature commits
//! to both. The signature proves that the party presenting the identity
//! actually holds its private half, and the transcript binding means the SAS
//! covers the identity too. Trust can therefore be persisted as the SHA-256
//! fingerprint of the public key instead of an unauthenticated discovery id.
//!
//! The private key is PKCS#8 and is kept in [`Zeroizing`] memory. It is never
//! printed by `Debug`, cloned, or exposed by any accessor other than the
//! explicit [`Identity::to_pkcs8`] export the application needs for storage.

use core::fmt;

use ring::rand::SystemRandom;
use ring::signature::{self, KeyPair as _};
use zeroize::Zeroizing;

use crate::error::CryptoError;

/// Domain separator mixed into every identity fingerprint.
const DOMAIN_FINGERPRINT: &[u8] = b"AirBridge v1 identity fingerprint";

/// Length of a SHA-256 identity fingerprint, in bytes.
pub const FINGERPRINT_LEN: usize = 32;

/// A long-lived ECDSA P-256 identity.
pub struct Identity {
    key_pair: signature::EcdsaKeyPair,
    public_key: Vec<u8>,
    pkcs8: Zeroizing<Vec<u8>>,
}

impl Identity {
    /// Generates a fresh identity.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::Identity`] if the system RNG fails or the
    /// generated key cannot be loaded.
    pub fn generate() -> Result<Self, CryptoError> {
        let rng = SystemRandom::new();
        let document = signature::EcdsaKeyPair::generate_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &rng,
        )
        .map_err(|_| CryptoError::Identity("identity key generation failed"))?;
        Self::from_pkcs8(document.as_ref())
    }

    /// Loads an identity from PKCS#8 DER bytes, as returned by
    /// [`Identity::to_pkcs8`].
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::Identity`] when the bytes are not a valid P-256
    /// private key document.
    pub fn from_pkcs8(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.is_empty() {
            return Err(CryptoError::Identity("identity key is empty"));
        }
        let rng = SystemRandom::new();
        let key_pair = signature::EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            bytes,
            &rng,
        )
        .map_err(|_| CryptoError::Identity("identity key is not valid PKCS#8"))?;
        let public_key = key_pair.public_key().as_ref().to_vec();
        Ok(Self {
            key_pair,
            public_key,
            pkcs8: Zeroizing::new(bytes.to_vec()),
        })
    }

    /// Returns the PKCS#8 DER encoding for persistent storage.
    ///
    /// The returned buffer is sensitive: store it in the platform's private
    /// application storage and never log it.
    #[must_use]
    pub fn to_pkcs8(&self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(self.pkcs8.to_vec())
    }

    /// Returns the uncompressed public key (`0x04 || X || Y`).
    #[must_use]
    pub fn public_key(&self) -> &[u8] {
        &self.public_key
    }

    /// Returns the SHA-256 fingerprint of the public key.
    #[must_use]
    pub fn fingerprint(&self) -> [u8; FINGERPRINT_LEN] {
        fingerprint(self.public_key())
    }

    /// Signs `message` with this identity.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::Identity`] if the signer fails.
    pub(crate) fn sign(&self, message: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let rng = SystemRandom::new();
        self.key_pair
            .sign(&rng, message)
            .map(|signature| signature.as_ref().to_vec())
            .map_err(|_| CryptoError::Identity("identity signature failed"))
    }

    /// Verifies a signature attributed to `public_key`.
    pub(crate) fn verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
        let key = signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, public_key);
        key.verify(message, signature).is_ok()
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately prints only public material.
        f.debug_struct("Identity")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

/// Computes the stable fingerprint of an identity public key.
#[must_use]
pub fn fingerprint(public_key: &[u8]) -> [u8; FINGERPRINT_LEN] {
    use ring::digest;
    let mut ctx = digest::Context::new(&digest::SHA256);
    ctx.update(DOMAIN_FINGERPRINT);
    ctx.update(public_key);
    let digest = ctx.finish();
    let mut out = [0_u8; FINGERPRINT_LEN];
    out.copy_from_slice(digest.as_ref());
    out
}

/// Renders a fingerprint as lower-case hexadecimal.
#[must_use]
pub fn fingerprint_hex(fingerprint: &[u8; FINGERPRINT_LEN]) -> String {
    let mut out = String::with_capacity(FINGERPRINT_LEN * 2);
    for byte in fingerprint {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Parses a lower-case or upper-case hexadecimal fingerprint.
///
/// # Errors
///
/// Returns `None` for a wrong length or a non-hexadecimal character.
#[must_use]
pub fn parse_fingerprint_hex(value: &str) -> Option<[u8; FINGERPRINT_LEN]> {
    let value = value.trim();
    if value.len() != FINGERPRINT_LEN * 2 {
        return None;
    }
    let mut out = [0_u8; FINGERPRINT_LEN];
    let bytes = value.as_bytes();
    for (index, slot) in out.iter_mut().enumerate() {
        let high = hex_value(bytes[index * 2])?;
        let low = hex_value(bytes[index * 2 + 1])?;
        *slot = (high << 4) | low;
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn identity_roundtrips_through_pkcs8() {
        let identity = Identity::generate().expect("generate");
        let restored = Identity::from_pkcs8(&identity.to_pkcs8()).expect("restore");
        assert_eq!(identity.public_key(), restored.public_key());
        assert_eq!(identity.fingerprint(), restored.fingerprint());
    }

    #[test]
    fn fingerprints_are_stable_and_distinct() {
        let a = Identity::generate().expect("a");
        let b = Identity::generate().expect("b");
        assert_eq!(a.fingerprint(), fingerprint(a.public_key()));
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint().len(), FINGERPRINT_LEN);
    }

    #[test]
    fn signatures_verify_only_for_the_right_message_and_key() {
        let identity = Identity::generate().expect("identity");
        let signature = identity.sign(b"airbridge").expect("sign");
        assert!(Identity::verify(
            identity.public_key(),
            b"airbridge",
            &signature
        ));
        assert!(!Identity::verify(
            identity.public_key(),
            b"airbridgf",
            &signature
        ));

        let other = Identity::generate().expect("other");
        assert!(!Identity::verify(
            other.public_key(),
            b"airbridge",
            &signature
        ));
    }

    #[test]
    fn invalid_pkcs8_is_rejected() {
        assert!(Identity::from_pkcs8(&[]).is_err());
        assert!(Identity::from_pkcs8(&[1, 2, 3]).is_err());
    }

    #[test]
    fn hex_roundtrip() {
        let identity = Identity::generate().expect("identity");
        let hex = fingerprint_hex(&identity.fingerprint());
        assert_eq!(hex.len(), 64);
        assert_eq!(parse_fingerprint_hex(&hex), Some(identity.fingerprint()));
        assert_eq!(
            parse_fingerprint_hex(&hex.to_uppercase()),
            Some(identity.fingerprint())
        );
        assert_eq!(parse_fingerprint_hex("not hex"), None);
    }
}
