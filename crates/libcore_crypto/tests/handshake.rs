//! End-to-end handshake tests, including a full man-in-the-middle simulation.
//!
//! These tests drive only the public API, exactly as the transport layer will.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use libcore_crypto::{
    AlertCode, CryptoError, EstablishedSession, Initiator, PendingSession, Responder,
    ResponderConfig, Sas, SasDigits, NEXT_PROTOCOL_AEAD,
};

/// Runs an honest handshake and returns both confirmed sessions.
fn establish() -> (EstablishedSession, EstablishedSession) {
    let (client_pending, server_pending) = complete_handshake();

    let server_sas = server_pending.sas(SasDigits::Six).expect("server sas");
    let client = client_pending
        .confirm_with(&server_sas)
        .expect("client confirm");
    let client_sas = *client.sas();
    let server = server_pending
        .confirm_with(&client_sas)
        .expect("server confirm");
    (client, server)
}

/// Runs an honest handshake and returns both sides before SAS confirmation.
fn complete_handshake() -> (PendingSession, PendingSession) {
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

    (client_pending, server_pending)
}

/// Returns the byte lengths of the four honest handshake messages.
fn honest_lengths() -> [usize; 4] {
    let mut client = Initiator::new(NEXT_PROTOCOL_AEAD).expect("initiator");
    let mut server = Responder::new(ResponderConfig::default()).expect("responder");

    let m1 = client.client_init().expect("m1");
    server.accept_client_init(&m1).expect("accept m1");
    let m2 = server.server_init().expect("m2");
    client.accept_server_init(&m2).expect("accept m2");
    let m3 = client.client_finished().expect("m3");
    server.accept_client_finished(&m3).expect("accept m3");
    let (m4, _) = server.finish().expect("finish");

    [m1.len(), m2.len(), m3.len(), m4.len()]
}

/// Runs a handshake with a single bit flipped in one message.
///
/// Returns `Ok(sas)` only if the handshake completed *and* both sides agreed on
/// the same code — that is, if the tampering went undetected.
fn handshake_with_tamper(target: usize, byte: usize) -> Result<Sas, CryptoError> {
    let flip = |message: usize, buffer: &mut [u8]| {
        if message == target && byte < buffer.len() {
            buffer[byte] ^= 0x01;
        }
    };

    let mut client = Initiator::new(NEXT_PROTOCOL_AEAD)?;
    let mut server = Responder::new(ResponderConfig::default())?;

    let mut m1 = client.client_init()?;
    flip(1, &mut m1);
    server.accept_client_init(&m1)?;

    let mut m2 = server.server_init()?;
    flip(2, &mut m2);
    client.accept_server_init(&m2)?;

    let mut m3 = client.client_finished()?;
    flip(3, &mut m3);
    server.accept_client_finished(&m3)?;

    let (mut m4, server_pending) = server.finish()?;
    flip(4, &mut m4);
    let client_pending = client.accept_server_finished(&m4)?;

    let client_sas = client_pending.sas(SasDigits::Six)?;
    let server_sas = server_pending.sas(SasDigits::Six)?;
    if !client_sas.matches(&server_sas) {
        return Err(CryptoError::SasMismatch);
    }
    Ok(client_sas)
}

/// Runs a man-in-the-middle that terminates two independent handshakes.
///
/// Returns the two *victim* sessions: the one seen by the initiator and the one
/// seen by the responder.
fn run_mitm() -> (PendingSession, PendingSession) {
    // Leg 1: honest initiator <-> attacker acting as responder.
    let mut initiator = Initiator::new(NEXT_PROTOCOL_AEAD).expect("initiator");
    let mut attacker_as_responder = Responder::new(ResponderConfig::default()).expect("responder");

    // Leg 2: attacker acting as initiator <-> honest responder.
    let mut attacker_as_initiator = Initiator::new(NEXT_PROTOCOL_AEAD).expect("initiator");
    let mut responder = Responder::new(ResponderConfig::default()).expect("responder");

    let a1 = initiator.client_init().expect("m1");
    attacker_as_responder
        .accept_client_init(&a1)
        .expect("accept m1");
    let ar2 = attacker_as_responder.server_init().expect("m2");
    initiator.accept_server_init(&ar2).expect("accept m2");

    let ai1 = attacker_as_initiator.client_init().expect("m1");
    responder.accept_client_init(&ai1).expect("accept m1");
    let b2 = responder.server_init().expect("m2");
    attacker_as_initiator
        .accept_server_init(&b2)
        .expect("accept m2");

    let a3 = initiator.client_finished().expect("m3");
    attacker_as_responder
        .accept_client_finished(&a3)
        .expect("accept m3");
    let ai3 = attacker_as_initiator.client_finished().expect("m3");
    responder.accept_client_finished(&ai3).expect("accept m3");

    let (ar4, attacker_responder_pending) = attacker_as_responder.finish().expect("finish");
    let initiator_pending = initiator.accept_server_finished(&ar4).expect("accept m4");

    let (b4, responder_pending) = responder.finish().expect("finish");
    let attacker_initiator_pending = attacker_as_initiator
        .accept_server_finished(&b4)
        .expect("accept m4");

    // Each leg is internally consistent: the attacker shares a code with each
    // victim. This is the point — a plain ECDH handshake cannot tell.
    assert_eq!(
        initiator_pending.sas(SasDigits::Six).expect("sas"),
        attacker_responder_pending.sas(SasDigits::Six).expect("sas")
    );
    assert_eq!(
        attacker_initiator_pending.sas(SasDigits::Six).expect("sas"),
        responder_pending.sas(SasDigits::Six).expect("sas")
    );

    (initiator_pending, responder_pending)
}

#[test]
fn handshake_completes_and_both_sides_agree() {
    let (client_pending, server_pending) = complete_handshake();

    assert_eq!(client_pending.role(), libcore_crypto::Role::Initiator);
    assert_eq!(server_pending.role(), libcore_crypto::Role::Responder);

    // Same transcript and same ECDH output must yield the same session id.
    assert_eq!(client_pending.session_id(), server_pending.session_id());

    let client_sas = client_pending.sas(SasDigits::Six).expect("client sas");
    let server_sas = server_pending.sas(SasDigits::Six).expect("server sas");
    assert!(client_sas.matches(&server_sas));
    assert_eq!(client_sas, server_sas);

    // Digit count is a display choice; both sides must agree at any width.
    for digits in [SasDigits::Four, SasDigits::Five, SasDigits::Six] {
        assert_eq!(
            client_pending.sas(digits).expect("client sas"),
            server_pending.sas(digits).expect("server sas")
        );
    }
}

#[test]
fn confirmed_sessions_exchange_frames_in_both_directions() {
    let (mut client, mut server) = establish();

    let request = client.seal(b"hello from the initiator").expect("seal");
    assert_eq!(
        server.open(&request).expect("open"),
        b"hello from the initiator"
    );

    let response = server.seal(b"hello from the responder").expect("seal");
    assert_eq!(
        client.open(&response).expect("open"),
        b"hello from the responder"
    );
}

#[test]
fn sequence_numbers_advance_independently_per_direction() {
    let (mut client, mut server) = establish();
    assert_eq!(client.send_seq(), 0);
    assert_eq!(server.send_seq(), 0);

    for expected in 0..10_u64 {
        let frame = client.seal(b"x").expect("seal");
        server.open(&frame).expect("open");
        assert_eq!(client.send_seq(), expected + 1);
        assert_eq!(server.highest_recv_seq(), Some(expected));
    }

    // The responder's send counter is untouched by receiving.
    assert_eq!(server.send_seq(), 0);
    assert_eq!(client.highest_recv_seq(), None);
}

#[test]
fn replayed_frame_is_rejected_after_handshake() {
    let (mut client, mut server) = establish();
    let frame = client.seal(b"only once").expect("seal");
    server.open(&frame).expect("open");
    assert!(matches!(
        server.open(&frame),
        Err(CryptoError::ReplayDetected { seq: 0, highest: 0 })
    ));
}

#[test]
fn tampered_frame_is_rejected_after_handshake() {
    let (mut client, mut server) = establish();
    let mut frame = client.seal(b"integrity matters").expect("seal");
    let last = frame.len() - 1;
    frame[last] ^= 0xFF;
    assert!(matches!(server.open(&frame), Err(CryptoError::FrameAuth)));
}

#[test]
fn man_in_the_middle_is_detected_by_sas_mismatch() {
    // A six-digit collision between two independent legs has probability 1e-6,
    // so retry rather than accept a flaky test.
    for _ in 0..8 {
        let (initiator_pending, responder_pending) = run_mitm();

        let initiator_sas = initiator_pending.sas(SasDigits::Six).expect("sas");
        let responder_sas = responder_pending.sas(SasDigits::Six).expect("sas");

        if initiator_sas == responder_sas {
            continue;
        }

        // The operator compares the two screens and sees a mismatch.
        assert!(matches!(
            initiator_pending.confirm_with(&responder_sas),
            Err(CryptoError::SasMismatch)
        ));
        assert!(matches!(
            responder_pending.confirm_with(&initiator_sas),
            Err(CryptoError::SasMismatch)
        ));
        return;
    }
    panic!("MITM was not detected in 8 attempts");
}

#[test]
fn any_single_bit_tamper_of_any_message_is_detected() {
    let lengths = honest_lengths();

    for (index, length) in lengths.iter().enumerate() {
        let message = index + 1;
        for byte in 0..*length {
            let result = handshake_with_tamper(message, byte);
            assert!(
                result.is_err(),
                "tampering with message {message}, byte {byte} went undetected"
            );
        }
    }
}

#[test]
fn server_key_substitution_is_rejected() {
    // The client binds to the commitment it received in M2. If M4 carries a
    // different key, the handshake must fail even though both messages are
    // individually well-formed and honestly generated.
    let mut victim = Responder::new(ResponderConfig::default()).expect("victim");
    let mut other = Responder::new(ResponderConfig::default()).expect("other");

    let mut client = Initiator::new(NEXT_PROTOCOL_AEAD).expect("initiator");
    let m1 = client.client_init().expect("m1");

    victim.accept_client_init(&m1).expect("accept m1");
    let m2 = victim.server_init().expect("m2");
    client.accept_server_init(&m2).expect("accept m2");
    let m3 = client.client_finished().expect("m3");

    other.accept_client_init(&m1).expect("accept m1");
    let _ = other.server_init().expect("m2");
    other.accept_client_finished(&m3).expect("accept m3");
    let (foreign_m4, _) = other.finish().expect("finish");

    assert!(matches!(
        client.accept_server_finished(&foreign_m4),
        Err(CryptoError::Handshake {
            code: AlertCode::CommitmentMismatch,
            ..
        })
    ));
}

#[test]
fn client_key_substitution_is_rejected() {
    let mut client = Initiator::new(NEXT_PROTOCOL_AEAD).expect("initiator");
    let mut other_client = Initiator::new(NEXT_PROTOCOL_AEAD).expect("initiator");
    let mut server = Responder::new(ResponderConfig::default()).expect("responder");

    let m1 = client.client_init().expect("m1");
    server.accept_client_init(&m1).expect("accept m1");
    let m2 = server.server_init().expect("m2");
    client.accept_server_init(&m2).expect("accept m2");
    // The attacker's client must run its own first flight so that it reaches
    // the `client_finished` state; its commitment will not match the one the
    // server recorded from the victim's M1.
    other_client.client_init().expect("foreign m1");
    other_client.accept_server_init(&m2).expect("accept m2");

    // M3 carries a key that was never committed to in M1.
    let foreign_m3 = other_client.client_finished().expect("m3");
    assert!(matches!(
        server.accept_client_finished(&foreign_m3),
        Err(CryptoError::Handshake {
            code: AlertCode::CommitmentMismatch,
            ..
        })
    ));
}

#[test]
fn replayed_client_init_is_rejected_on_the_second_use() {
    let mut server = Responder::new(ResponderConfig::default()).expect("responder");
    let mut client = Initiator::new(NEXT_PROTOCOL_AEAD).expect("initiator");
    let m1 = client.client_init().expect("m1");

    server.accept_client_init(&m1).expect("accept m1");
    // A responder is single-use; feeding M1 again is a protocol error.
    assert!(matches!(
        server.accept_client_init(&m1),
        Err(CryptoError::Handshake {
            code: AlertCode::IncorrectMessage,
            ..
        })
    ));
}

#[test]
fn out_of_order_messages_are_rejected() {
    let mut server = Responder::new(ResponderConfig::default()).expect("responder");
    assert!(server.server_init().is_err());

    let mut client = Initiator::new(NEXT_PROTOCOL_AEAD).expect("initiator");
    assert!(client.client_finished().is_err());

    // A poisoned object cannot be revived.
    assert!(client.client_init().is_err());
}

#[test]
fn unsupported_next_protocol_is_rejected() {
    let mut client = Initiator::new("SOME_OTHER_PROTOCOL").expect("initiator");
    let mut server = Responder::new(ResponderConfig::default()).expect("responder");

    let m1 = client.client_init().expect("m1");
    assert!(matches!(
        server.accept_client_init(&m1),
        Err(CryptoError::Handshake {
            code: AlertCode::BadNextProtocol,
            ..
        })
    ));
}

#[test]
fn responder_policy_can_accept_a_custom_protocol() {
    let mut client = Initiator::new("CUSTOM_PROTOCOL").expect("initiator");
    let mut server = Responder::new(ResponderConfig::new(&["CUSTOM_PROTOCOL"])).expect("responder");

    let m1 = client.client_init().expect("m1");
    server.accept_client_init(&m1).expect("accept m1");
}

#[test]
fn every_handshake_is_fresh() {
    let (_c1, s1) = complete_handshake();
    let (_c2, s2) = complete_handshake();

    assert_ne!(s1.session_id(), s2.session_id());
    assert_ne!(
        s1.sas(SasDigits::Six).expect("sas"),
        s2.sas(SasDigits::Six).expect("sas")
    );
}

#[test]
fn sessions_from_different_handshakes_cannot_talk_to_each_other() {
    let (mut client_a, mut server_a) = establish();
    let frame = client_a.seal(b"session a").expect("seal");
    server_a.open(&frame).expect("open");

    // A frame from session A must not be accepted by a server from an
    // unrelated session, even at the same sequence number.
    let (_client_b, mut server_b) = establish();
    assert!(matches!(server_b.open(&frame), Err(CryptoError::FrameAuth)));
}

#[test]
fn confirm_without_verification_still_works() {
    let (client_pending, server_pending) = complete_handshake();
    let mut client = client_pending.confirm(SasDigits::Six).expect("confirm");
    let mut server = server_pending.confirm(SasDigits::Six).expect("confirm");

    let frame = client.seal(b"trusted transport").expect("seal");
    assert_eq!(server.open(&frame).expect("open"), b"trusted transport");
    assert_eq!(
        client.sas().to_string().len(),
        6,
        "the confirmed session retains a displayable code"
    );
}

#[test]
fn errors_serialise_as_alerts() {
    let alert = CryptoError::Handshake {
        code: AlertCode::BadVersion,
        detail: "unsupported version",
    }
    .to_alert();

    assert_eq!(alert[0], 0x05, "alert tag");
    assert_eq!(
        u16::from_be_bytes([alert[1], alert[2]]),
        AlertCode::BadVersion.as_u16()
    );
    assert!(alert.len() > 3);
}
