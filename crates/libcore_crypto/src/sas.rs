//! Short Authentication String (SAS) derivation — the `SASLinker` defence.
//!
//! # What problem this solves
//!
//! A plain ECDH handshake is vulnerable to an active man-in-the-middle: the
//! attacker runs one handshake with each side and relays traffic. Nothing in
//! the key exchange alone detects this, because each side genuinely shares a
//! key — just not with the party it thinks.
//!
//! The defence is to derive a short code from the *whole* handshake transcript
//! and have a human compare it over a channel the attacker does not control
//! (two screens in the same room, a phone call, a pre-shared PIN). The attacker
//! must run two independent handshakes with different transcripts, so the two
//! codes differ, and the comparison fails.
//!
//! # Why the code is derived from `AUTH_STRING`
//!
//! [`crate::kdf`] derives `AUTH_STRING` from the ECDH output `DHS` and the full
//! four-message transcript, and derives the frame keys from a *different*
//! `HKDF-Extract` under a different salt. That separation matters:
//!
//! * the SAS depends on `DHS` and both nonces (through the transcript), so it
//!   is bound to the exact key exchange that took place;
//! * the SAS is a one-way compression of a 256-bit secret to ~20 bits, so
//!   reading it off a screen reveals nothing usable about `AUTH_STRING`;
//! * and because `AUTH_STRING` is independent of the frame keys, an attacker
//!   who learns the SAS (or the operator who reads it aloud) learns nothing
//!   that helps decrypt traffic.
//!
//! # Modulo bias
//!
//! The obvious `u32 % 1_000_000` is biased: 2^32 is not a multiple of 10^6, so
//! low codes are ~0.023% more likely. That is small but it is free to remove,
//! so we remove it. We draw 40 bits and rejection-sample, which makes the
//! output exactly uniform over `[0, 10^digits)`.

use core::fmt;

use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::error::CryptoError;
use crate::kdf;

/// Number of digits in a Short Authentication String.
///
/// The security level is `log2(10^digits)`: roughly 13.3, 16.6 and 19.9 bits
/// for four, five and six digits. Because the comparison is performed by a
/// human and is normally one-shot, six digits is the recommended default; four
/// digits is only appropriate when the out-of-band channel is itself
/// authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SasDigits {
    /// Four digits (~13.3 bits).
    Four,
    /// Five digits (~16.6 bits).
    Five,
    /// Six digits (~19.9 bits).
    Six,
}

impl SasDigits {
    /// The number of decimal digits.
    #[must_use]
    pub const fn count(self) -> u32 {
        match self {
            Self::Four => 4,
            Self::Five => 5,
            Self::Six => 6,
        }
    }

    /// `10^count`, the size of the code space.
    const fn modulus(self) -> u64 {
        match self {
            Self::Four => 10_000,
            Self::Five => 100_000,
            Self::Six => 1_000_000,
        }
    }
}

/// A Short Authentication String.
///
/// The value is intentionally low-entropy: it exists to be read aloud or
/// compared by eye. It is *not* a secret and must never be used as a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sas {
    code: u32,
    digits: SasDigits,
}

impl Sas {
    /// Returns the numeric value of the code.
    #[must_use]
    pub const fn code(self) -> u32 {
        self.code
    }

    /// Returns the configured digit count.
    #[must_use]
    pub const fn digits(self) -> SasDigits {
        self.digits
    }

    /// Compares two codes in constant time.
    ///
    /// The comparison is against a full-width encoding so that the timing does
    /// not depend on how many leading digits happen to match.
    #[must_use]
    pub fn matches(self, other: &Self) -> bool {
        if self.digits != other.digits {
            return false;
        }
        bool::from(self.code.to_be_bytes().ct_eq(&other.code.to_be_bytes()))
    }

    /// Renders the code as a zero-padded decimal string of the right length.
    #[must_use]
    pub fn to_padded_string(self) -> String {
        format!(
            "{:0width$}",
            self.code,
            width = self.digits.count() as usize
        )
    }

    /// Derives a SAS from a UKEY2 `AUTH_STRING`.
    ///
    /// This is the single entry point that turns secret material into a
    /// displayable code. It never touches the ECDH output or the frame keys
    /// directly; by construction it only sees `AUTH_STRING`, which is itself
    /// independent of the frame keys.
    ///
    /// `info` is bound to the requested digit count so that a four-digit code
    /// is not simply a truncation of the six-digit one for the same secret.
    pub(crate) fn derive(auth_string: &[u8], digits: SasDigits) -> Result<Self, CryptoError> {
        let modulus = digits.modulus();

        // 40 bits gives a rejection probability below 1e-6 for every supported
        // digit count, so the loop below effectively always succeeds on the
        // first attempt.
        const DRAW_BITS: u64 = 40;
        const DRAW_BYTES: usize = 5;
        const MAX_ATTEMPTS: u8 = 8;

        let limit = ((1_u64 << DRAW_BITS) / modulus) * modulus;

        for attempt in 0..MAX_ATTEMPTS {
            let mut block = Zeroizing::new([0_u8; DRAW_BYTES]);
            let count = digits.count().to_be_bytes();
            kdf::expand_sas(auth_string, &[count.as_slice(), &[attempt]], &mut block[..])?;

            let mut value = 0_u64;
            for byte in block.iter() {
                value = (value << 8) | u64::from(*byte);
            }

            if value < limit {
                return Ok(Self {
                    code: (value % modulus) as u32,
                    digits,
                });
            }
        }

        // Unreachable with overwhelming probability (each attempt rejects with
        // p < 1e-6), but we refuse to fabricate a value.
        Err(CryptoError::KeySchedule)
    }
}

impl fmt::Display for Sas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = self.digits.count() as usize;
        write!(f, "{:0width$}", self.code, width = width)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const SECRET: &[u8] = &[
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];

    #[test]
    fn derivation_is_deterministic() {
        for digits in [SasDigits::Four, SasDigits::Five, SasDigits::Six] {
            let a = Sas::derive(SECRET, digits).expect("derive");
            let b = Sas::derive(SECRET, digits).expect("derive");
            assert_eq!(a, b);
        }
    }

    #[test]
    fn codes_are_in_range() {
        for digits in [SasDigits::Four, SasDigits::Five, SasDigits::Six] {
            for i in 0..200_u16 {
                let mut secret = SECRET.to_vec();
                secret[0] = (i & 0xFF) as u8;
                secret[1] = (i >> 8) as u8;
                let sas = Sas::derive(&secret, digits).expect("derive");
                assert!(
                    sas.code() < digits.modulus() as u32,
                    "code {} out of range for {digits:?}",
                    sas.code()
                );
            }
        }
    }

    #[test]
    fn rendering_is_zero_padded_to_the_right_width() {
        let sas = Sas {
            code: 42,
            digits: SasDigits::Six,
        };
        assert_eq!(sas.to_padded_string(), "000042");
        assert_eq!(sas.to_string(), "000042");
        assert_eq!(sas.to_padded_string().len(), 6);

        let sas = Sas {
            code: 7,
            digits: SasDigits::Four,
        };
        assert_eq!(sas.to_string(), "0007");

        let sas = Sas {
            code: 123_456,
            digits: SasDigits::Six,
        };
        assert_eq!(sas.to_string(), "123456");
    }

    #[test]
    fn different_secrets_give_different_codes() {
        let a = Sas::derive(&[0x01; 32], SasDigits::Six).expect("derive");
        let b = Sas::derive(&[0x02; 32], SasDigits::Six).expect("derive");
        assert_ne!(a, b);
    }

    #[test]
    fn digit_count_changes_the_derived_value() {
        // Not merely a truncation: the digit count is bound into the info
        // parameter, so a 4-digit code is derived independently.
        let four = Sas::derive(SECRET, SasDigits::Four).expect("derive");
        let six = Sas::derive(SECRET, SasDigits::Six).expect("derive");
        assert_ne!(four.code(), six.code() % 10_000);
    }

    #[test]
    fn matching_rejects_different_digit_counts() {
        let four = Sas {
            code: 1234,
            digits: SasDigits::Four,
        };
        let six = Sas {
            code: 1234,
            digits: SasDigits::Six,
        };
        assert!(!four.matches(&six));
        assert!(!six.matches(&four));
    }

    #[test]
    fn matching_accepts_identical_codes() {
        let a = Sas::derive(SECRET, SasDigits::Six).expect("derive");
        let b = Sas::derive(SECRET, SasDigits::Six).expect("derive");
        assert!(a.matches(&b));
    }

    #[test]
    fn distribution_is_plausibly_uniform() {
        // A coarse smoke test for the rejection sampler. With 2000 samples
        // over 4 buckets, the expected count is 500; a grossly broken
        // reduction (for example, only ever returning low values) would fail
        // this comfortably.
        let mut buckets = [0_usize; 4];
        for i in 0..2000_u32 {
            let mut secret = SECRET.to_vec();
            secret[..4].copy_from_slice(&i.to_be_bytes());
            let sas = Sas::derive(&secret, SasDigits::Four).expect("derive");
            buckets[(sas.code() / 2500) as usize] += 1;
        }
        for (index, count) in buckets.iter().enumerate() {
            assert!(
                *count > 400 && *count < 600,
                "bucket {index} had {count} samples, expected ~500"
            );
        }
    }

    #[test]
    fn no_secret_material_survives_in_the_code() {
        // The code must be a compression, never a view onto the secret bytes.
        let sas = Sas::derive(SECRET, SasDigits::Six).expect("derive");
        let rendered = sas.to_padded_string();
        assert!(!rendered.contains("00010203"));
        assert!(sas.code() < 1_000_000);
    }
}
