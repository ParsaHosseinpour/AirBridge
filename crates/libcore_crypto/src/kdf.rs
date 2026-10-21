//! HKDF-SHA512 key schedule.
//!
//! # Relationship to UKEY2
//!
//! The published UKEY2 specification derives two secrets from the ECDH output
//! `DHS` and the handshake transcript:
//!
//! ```text
//! PRK_AUTH = HKDF-Extract(salt = "UKEY2 v1 auth", IKM = DHS)
//! PRK_NEXT = HKDF-Extract(salt = "UKEY2 v1 next", IKM = DHS)
//!
//! AUTH_STRING = HKDF-Expand(PRK_AUTH, info = M1 | M2, L_auth)
//! NEXT_SECRET = HKDF-Expand(PRK_NEXT, info = M1 | M2, L_next)
//! ```
//!
//! This module implements exactly that, with three deliberate differences:
//!
//! 1. **SHA-512, per the specification.** Google's C++ implementation actually
//!    uses HKDF-SHA256 with a single-block expand (`crypto_ops.h` documents
//!    `CryptoOps::Hkdf` as "HKDF (RFC 5869) with the SHA-256 hash"), which
//!    contradicts the `P256_SHA512` ciphersuite it advertises. We follow the
//!    specification and its hash.
//!
//! 2. **The salt is not NUL-terminated.** `ukey2_handshake.cc` builds its salt
//!    with `string(kUkey2HkdfSalt, sizeof(kUkey2HkdfSalt))`, and `sizeof` of a
//!    string literal includes the trailing NUL. The real salt in Google's
//!    implementation is therefore 14 bytes, not 13. We use the 13-byte value
//!    the specification names. This is a one-constant change if interoperability
//!    with Google's implementation is ever required.
//!
//! 3. **The transcript covers all four messages.** UKEY2 binds `M1 | M2` and
//!    relies on the client's commitment to bind `M3`. Our handshake has a
//!    fourth message, so the transcript is `M1 | M2 | M3 | M4`. Binding more
//!    data can never weaken the derivation.
//!
//! # Key separation
//!
//! `AUTH_STRING` and the frame keys come from independent `HKDF-Extract` calls
//! under different salts, so learning one reveals nothing about the other.
//! That property is what makes it safe to display `AUTH_STRING` (or a
//! truncation of it) on screen: the UKEY2 specification calls this out
//! explicitly, and [`crate::sas`] relies on it.
//!
//! The two directional frame keys are carved out of a single expanded key
//! block rather than derived by separate `HKDF-Expand` calls. Both are sound;
//! the key-block form avoids any concatenation ambiguity in the `info`
//! parameter and matches how UKEY2 itself splits a master key into `client`
//! and `server` keys.

use ring::{digest, hkdf};
use zeroize::Zeroizing;

use crate::error::CryptoError;

/// UKEY2 salt for the authentication string.
///
/// 13 bytes, as written in the specification. See the module docs for why this
/// deliberately excludes the trailing NUL that Google's C++ passes.
pub(crate) const SALT_AUTH: &[u8] = b"UKEY2 v1 auth";

/// UKEY2 salt for the next-protocol key material.
pub(crate) const SALT_NEXT: &[u8] = b"UKEY2 v1 next";

/// Domain separator for the SAS expansion.
pub(crate) const SALT_SAS: &[u8] = b"AirBridge v1 sas";

/// Domain separator mixed into the SAS `info` parameter.
pub(crate) const SAS_DOMAIN: &[u8] = b"AirBridge v1 sas digits";

/// Domain separator for the public session identifier.
const DOMAIN_SESSION_ID: &[u8] = b"AirBridge v1 session id";

/// Length of `AUTH_STRING`, in bytes.
pub(crate) const AUTH_SECRET_LEN: usize = 32;

/// Length of an AES-256 key, in bytes.
pub(crate) const KEY_LEN: usize = 32;

/// Length of the GCM nonce prefix, in bytes.
pub(crate) const NONCE_PREFIX_LEN: usize = 4;

/// Length of the public session identifier, in bytes.
pub(crate) const SESSION_ID_LEN: usize = 32;

/// Total expanded key block: `c2s_key | s2c_key | c2s_prefix | s2c_prefix`.
const KEY_BLOCK_LEN: usize = KEY_LEN * 2 + NONCE_PREFIX_LEN * 2;

/// The output of the UKEY2 key schedule.
///
/// All secret fields are zeroised on drop. The struct is deliberately not
/// `Debug`, `Clone`, or `Copy`.
pub(crate) struct KeySchedule {
    /// `AUTH_STRING`: the high-entropy value the SAS is derived from. Never
    /// used as a frame key.
    pub(crate) auth_secret: Zeroizing<[u8; AUTH_SECRET_LEN]>,

    /// Key for initiator-to-responder frames.
    pub(crate) c2s_key: Zeroizing<[u8; KEY_LEN]>,

    /// Key for responder-to-initiator frames.
    pub(crate) s2c_key: Zeroizing<[u8; KEY_LEN]>,

    /// GCM nonce prefix for initiator-to-responder frames.
    pub(crate) c2s_nonce_prefix: [u8; NONCE_PREFIX_LEN],

    /// GCM nonce prefix for responder-to-initiator frames.
    pub(crate) s2c_nonce_prefix: [u8; NONCE_PREFIX_LEN],

    /// Public, collision-resistant binding for this handshake. Used as
    /// associated data on every frame so that a frame cannot be moved between
    /// sessions.
    pub(crate) session_id: [u8; SESSION_ID_LEN],
}

impl KeySchedule {
    /// Derives the full session key material.
    ///
    /// `dhs` is the raw P-256 ECDH output (the x-coordinate of the shared
    /// point). `transcript` is the concatenation of the four handshake
    /// messages in order.
    pub(crate) fn derive(dhs: &[u8], transcript: &[u8]) -> Result<Self, CryptoError> {
        // AUTH_STRING = HKDF-Expand(HKDF-Extract("UKEY2 v1 auth", DHS), transcript)
        let mut auth_secret = Zeroizing::new([0_u8; AUTH_SECRET_LEN]);
        expand(SALT_AUTH, dhs, &[transcript], &mut auth_secret[..])?;

        // NEXT_SECRET = HKDF-Expand(HKDF-Extract("UKEY2 v1 next", DHS), transcript)
        let mut block = Zeroizing::new([0_u8; KEY_BLOCK_LEN]);
        expand(SALT_NEXT, dhs, &[transcript], &mut block[..])?;

        let mut c2s_key = Zeroizing::new([0_u8; KEY_LEN]);
        let mut s2c_key = Zeroizing::new([0_u8; KEY_LEN]);
        c2s_key.copy_from_slice(&block[..KEY_LEN]);
        s2c_key.copy_from_slice(&block[KEY_LEN..KEY_LEN * 2]);

        let mut c2s_nonce_prefix = [0_u8; NONCE_PREFIX_LEN];
        let mut s2c_nonce_prefix = [0_u8; NONCE_PREFIX_LEN];
        c2s_nonce_prefix.copy_from_slice(&block[KEY_LEN * 2..KEY_LEN * 2 + NONCE_PREFIX_LEN]);
        s2c_nonce_prefix.copy_from_slice(&block[KEY_LEN * 2 + NONCE_PREFIX_LEN..]);

        // A plain domain-separated hash. `session_id` is public by design; it
        // is bound into every frame's associated data.
        let mut ctx = digest::Context::new(&digest::SHA512);
        ctx.update(DOMAIN_SESSION_ID);
        ctx.update(transcript);
        let digest = ctx.finish();
        let mut session_id = [0_u8; SESSION_ID_LEN];
        session_id.copy_from_slice(&digest.as_ref()[..SESSION_ID_LEN]);

        Ok(Self {
            auth_secret,
            c2s_key,
            s2c_key,
            c2s_nonce_prefix,
            s2c_nonce_prefix,
            session_id,
        })
    }
}

/// Adapter that lets us request an arbitrary output length from `ring`'s HKDF,
/// whose `expand` is generic over its own key-type markers rather than `usize`.
struct OkmLen(usize);

impl hkdf::KeyType for OkmLen {
    fn len(&self) -> usize {
        self.0
    }
}

/// One-shot HKDF-SHA512 extract-then-expand.
fn expand(salt: &[u8], ikm: &[u8], info: &[&[u8]], out: &mut [u8]) -> Result<(), CryptoError> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA512, salt);
    let prk = salt.extract(ikm);
    let okm = prk
        .expand(info, OkmLen(out.len()))
        .map_err(|_| CryptoError::KeySchedule)?;
    okm.fill(out).map_err(|_| CryptoError::KeySchedule)
}

/// Expands SAS material from a UKEY2 `AUTH_STRING`.
///
/// The caller supplies only the trailing `info` components; [`SAS_DOMAIN`] is
/// always prepended so that SAS output can never collide with any other use of
/// the authentication secret.
pub(crate) fn expand_sas(
    auth_string: &[u8],
    info: &[&[u8]],
    out: &mut [u8],
) -> Result<(), CryptoError> {
    let mut parts: Vec<&[u8]> = Vec::with_capacity(info.len() + 1);
    parts.push(SAS_DOMAIN);
    parts.extend_from_slice(info);
    expand(SALT_SAS, auth_string, &parts, out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use ring::hmac;

    /// Independent re-implementation of RFC 5869 on top of `ring::hmac`,
    /// used to cross-check `ring::hkdf`. If `ring`'s HKDF module ever
    /// regressed, a self-consistency test would not notice; this would.
    fn rfc5869_expand(prk: &[u8], info: &[u8], len: usize) -> Vec<u8> {
        let key = hmac::Key::new(hmac::HMAC_SHA512, prk);
        let mut out = Vec::with_capacity(len);
        let mut previous: Vec<u8> = Vec::new();
        let mut counter = 1_u8;
        while out.len() < len {
            let mut ctx = hmac::Context::with_key(&key);
            ctx.update(&previous);
            ctx.update(info);
            ctx.update(&[counter]);
            previous = ctx.sign().as_ref().to_vec();
            out.extend_from_slice(&previous);
            counter += 1;
        }
        out.truncate(len);
        out
    }

    fn rfc5869_extract(salt: &[u8], ikm: &[u8]) -> Vec<u8> {
        let key = hmac::Key::new(hmac::HMAC_SHA512, salt);
        hmac::sign(&key, ikm).as_ref().to_vec()
    }

    #[test]
    fn hkdf_matches_the_rfc5869_construction() {
        let dhs = [0x5A_u8; 32];
        let transcript = b"transcript-bytes";

        for len in [1_usize, 32, 64, 65, 72, 128] {
            let mut actual = vec![0_u8; len];
            expand(SALT_NEXT, &dhs, &[transcript], &mut actual).expect("hkdf");
            let prk = rfc5869_extract(SALT_NEXT, &dhs);
            let expected = rfc5869_expand(&prk, transcript, len);
            assert_eq!(actual, expected, "mismatch at length {len}");
        }
    }

    #[test]
    fn multi_part_info_matches_concatenation() {
        // ring takes `info` as a slice of slices; confirm that they are simply
        // concatenated, since the SAS derives its info from several pieces.
        let dhs = [0x11_u8; 32];
        let mut split = [0_u8; 32];
        let mut joined = [0_u8; 32];
        expand(SALT_SAS, &dhs, &[b"abc", b"def"], &mut split).expect("split");
        expand(SALT_SAS, &dhs, &[b"abcdef"], &mut joined).expect("joined");
        assert_eq!(split, joined);
    }

    #[test]
    fn salt_constants_are_spec_conformant() {
        // Google's C++ passes `sizeof(literal)`, which includes the NUL. We
        // deliberately do not. Pin the decision so nobody "fixes" it later
        // without understanding the interop consequence.
        assert_eq!(SALT_AUTH, b"UKEY2 v1 auth");
        assert_eq!(SALT_AUTH.len(), 13);
        assert_eq!(SALT_NEXT, b"UKEY2 v1 next");
        assert_eq!(SALT_NEXT.len(), 13);
        assert!(!SALT_AUTH.contains(&0));
        assert!(!SALT_NEXT.contains(&0));
    }

    #[test]
    fn derivation_is_deterministic() {
        let dhs = [0x42_u8; 32];
        let transcript = b"m1m2m3m4";
        let a = KeySchedule::derive(&dhs, transcript).expect("derive");
        let b = KeySchedule::derive(&dhs, transcript).expect("derive");
        assert_eq!(a.c2s_key[..], b.c2s_key[..]);
        assert_eq!(a.s2c_key[..], b.s2c_key[..]);
        assert_eq!(a.session_id, b.session_id);
        assert_eq!(a.auth_secret[..], b.auth_secret[..]);
    }

    #[test]
    fn directional_keys_are_independent() {
        let schedule = KeySchedule::derive(&[0x42_u8; 32], b"transcript").expect("derive");
        assert_ne!(schedule.c2s_key[..], schedule.s2c_key[..]);
        assert_ne!(schedule.c2s_nonce_prefix, schedule.s2c_nonce_prefix);
    }

    #[test]
    fn auth_secret_is_independent_of_frame_keys() {
        // The security argument for showing the SAS on screen depends on this:
        // observing AUTH_STRING must not reveal the frame keys, and vice versa.
        let schedule = KeySchedule::derive(&[0x42_u8; 32], b"transcript").expect("derive");
        assert_ne!(schedule.auth_secret[..], schedule.c2s_key[..]);
        assert_ne!(schedule.auth_secret[..], schedule.s2c_key[..]);
    }

    #[test]
    fn transcript_is_bound_into_every_output() {
        let dhs = [0x42_u8; 32];
        let a = KeySchedule::derive(&dhs, b"transcript-a").expect("derive");
        let b = KeySchedule::derive(&dhs, b"transcript-b").expect("derive");

        assert_ne!(a.c2s_key[..], b.c2s_key[..]);
        assert_ne!(a.s2c_key[..], b.s2c_key[..]);
        assert_ne!(a.auth_secret[..], b.auth_secret[..]);
        assert_ne!(a.session_id, b.session_id);
    }

    #[test]
    fn dhs_is_bound_into_secret_outputs() {
        let transcript = b"transcript";
        let a = KeySchedule::derive(&[0x01_u8; 32], transcript).expect("derive");
        let b = KeySchedule::derive(&[0x02_u8; 32], transcript).expect("derive");

        assert_ne!(a.c2s_key[..], b.c2s_key[..]);
        assert_ne!(a.s2c_key[..], b.s2c_key[..]);
        assert_ne!(a.auth_secret[..], b.auth_secret[..]);
        assert_ne!(a.c2s_nonce_prefix, b.c2s_nonce_prefix);
        assert_ne!(a.s2c_nonce_prefix, b.s2c_nonce_prefix);

        // `session_id` is deliberately a function of the public transcript
        // alone. The transcript already contains both ephemeral public keys, so
        // the identifier is still unique per handshake; keeping DHS out of it
        // lets an observer compute it. Frame integrity does not depend on this
        // value being secret, only on it being bound as associated data.
        assert_eq!(a.session_id, b.session_id);
    }
}
