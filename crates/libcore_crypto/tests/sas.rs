//! Tests for the public Short Authentication String surface.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use libcore_crypto::{Initiator, Responder, ResponderConfig, Sas, SasDigits, NEXT_PROTOCOL_AEAD};

fn handshake_sas(digits: SasDigits) -> (Sas, Sas) {
    let mut client = Initiator::new(NEXT_PROTOCOL_AEAD).expect("initiator");
    let mut server = Responder::new(ResponderConfig::default()).expect("responder");

    let m1 = client.client_init().expect("m1");
    server.accept_client_init(&m1).expect("accept m1");
    let m2 = server.server_init().expect("m2");
    client.accept_server_init(&m2).expect("accept m2");
    let m3 = client.client_finished().expect("m3");
    server.accept_client_finished(&m3).expect("accept m3");
    let (m4, server_pending) = server.finish().expect("finish");
    let client_pending = client.accept_server_finished(&m4).expect("accept m4");

    (
        client_pending.sas(digits).expect("client sas"),
        server_pending.sas(digits).expect("server sas"),
    )
}

#[test]
fn both_parties_derive_the_same_code() {
    for digits in [SasDigits::Four, SasDigits::Five, SasDigits::Six] {
        let (client, server) = handshake_sas(digits);
        assert_eq!(client, server);
        assert!(client.matches(&server));
        assert_eq!(client.digits(), digits);
    }
}

#[test]
fn rendered_length_matches_the_requested_digit_count() {
    for (digits, expected) in [
        (SasDigits::Four, 4),
        (SasDigits::Five, 5),
        (SasDigits::Six, 6),
    ] {
        let (client, _) = handshake_sas(digits);
        let rendered = client.to_string();
        assert_eq!(rendered.len(), expected);
        assert_eq!(rendered, client.to_padded_string());
        assert!(rendered.chars().all(|c| c.is_ascii_digit()));
    }
}

#[test]
fn codes_stay_within_their_decimal_range() {
    let (client, _) = handshake_sas(SasDigits::Six);
    assert!(client.code() < 1_000_000);

    let (client, _) = handshake_sas(SasDigits::Four);
    assert!(client.code() < 10_000);
}

#[test]
fn codes_for_different_digit_counts_are_not_truncations_of_each_other() {
    // Deriving 4 digits must not simply drop two digits from the 6-digit code;
    // the width is bound into the KDF info.
    let (four, _) = handshake_sas(SasDigits::Four);
    let (six, _) = handshake_sas(SasDigits::Six);
    assert_ne!(four.code(), six.code() % 10_000);
}

#[test]
fn matching_rejects_a_different_width() {
    let (four, _) = handshake_sas(SasDigits::Four);
    let (six, _) = handshake_sas(SasDigits::Six);

    assert_ne!(four.digits(), six.digits());
    assert!(!four.matches(&six));
    assert!(!six.matches(&four));
}
