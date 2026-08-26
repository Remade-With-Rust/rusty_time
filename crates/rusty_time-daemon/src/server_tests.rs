//! Tests for the NTP server: admission, interleaved mode, NTS, and the
//! adversarial cases (reflection, amplification, forgery, garbage).

use super::*;
use crate::nts_session::NtsSession;

fn peer(n: u8) -> SocketAddr {
    format!("192.0.2.{n}:123").parse().expect("addr")
}

/// The same host, a different ephemeral source port.
fn peer_port(n: u8, port: u16) -> SocketAddr {
    format!("192.0.2.{n}:{port}").parse().expect("addr")
}

fn state_with(rate_limit: RateLimitConfig) -> Arc<Mutex<ServerState>> {
    let mut ring = KeyRing::new(3);
    ring.rotate_in(MasterKey {
        id: 7,
        key: [0x5A; 32],
    });
    Arc::new(Mutex::new(ServerState {
        clients: ClientTable::new(64, rate_limit),
        ring,
        stratum: 2,
        started_unix: 0,
    }))
}

/// Rate limiting effectively off, for tests about protocol behavior rather
/// than admission.
fn test_state() -> Arc<Mutex<ServerState>> {
    state_with(RateLimitConfig {
        interval_log2: -20,
        burst: 1_000_000,
        leak_shift: 0,
        global_rate_hz: 0.0,
        global_burst: 0.0,
    })
}

#[test]
fn answers_plain_client_requests() {
    let state = test_state();
    let req = NtpPacket::client_request(4, NtpTimestamp(0x1234_5678_9ABC_DEF0)).to_bytes();
    let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
    let reply = build_reply(&req, peer(1), recv, &state, &SystemClock).expect("reply");
    let p = NtpPacket::parse(&reply).expect("parse");
    assert_eq!(p.mode, Mode::Server);
    assert_eq!(p.stratum, 2);
    // The origin timestamp must echo the client transmit: that is the client's
    // only anti-spoofing binding.
    assert_eq!(p.origin_ts, NtpTimestamp(0x1234_5678_9ABC_DEF0));
}

#[test]
fn refuses_to_reflect_server_mode_packets() {
    // A server-mode datagram must never be answered, or the server becomes a
    // reflection amplifier between two victims.
    let state = test_state();
    let mut p = NtpPacket::client_request(4, NtpTimestamp(1));
    p.mode = Mode::Server;
    let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
    assert!(build_reply(&p.to_bytes(), peer(1), recv, &state, &SystemClock).is_none());
    assert_eq!(state.lock().expect("lock").clients.stats.refused, 1);
}

#[test]
fn a_flood_is_rate_limited_and_kissed_off_sparingly() {
    let state = state_with(RateLimitConfig {
        interval_log2: 3,
        burst: 2,
        leak_shift: 2,
        global_rate_hz: 0.0,
        global_burst: 0.0,
    });
    let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
    let mut answers = 0;
    let mut kods = 0;
    for i in 0..40u64 {
        let req = NtpPacket::client_request(4, NtpTimestamp(i)).to_bytes();
        if let Some(reply) = build_reply(&req, peer(9), recv, &state, &SystemClock) {
            let p = NtpPacket::parse(&reply).expect("parse");
            if p.stratum == 0 && &p.reference_id == b"RATE" {
                kods += 1;
            } else {
                answers += 1;
            }
        }
    }
    // The whole point: a flood must cost the attacker far more packets than it
    // costs us.
    assert!(
        answers <= 3,
        "rate limiter let {answers} full answers through"
    );
    assert!(kods > 0, "no Kiss-o'-Death emitted at all");
    assert!(
        answers + kods < 20,
        "server replied to {}/40 of a flood, which is an amplifier",
        answers + kods
    );
}

#[test]
fn changing_source_port_does_not_reset_the_rate_limit() {
    // The source port is chosen by the sender. If it were part of the client
    // key, anyone could refill their bucket by picking a new ephemeral port
    // and the limiter would be decorative.
    let state = state_with(RateLimitConfig {
        interval_log2: 3,
        burst: 4,
        leak_shift: 8, // rarely kiss, so we count real answers
        global_rate_hz: 0.0,
        global_burst: 0.0,
    });
    let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
    let mut answers = 0;
    for port in 40_000u16..40_040 {
        let req = NtpPacket::client_request(4, NtpTimestamp(port as u64)).to_bytes();
        if let Some(reply) = build_reply(&req, peer_port(9, port), recv, &state, &SystemClock) {
            let p = NtpPacket::parse(&reply).expect("parse");
            if p.stratum != 0 {
                answers += 1;
            }
        }
    }
    assert!(
        answers <= 5,
        "40 requests from 40 source ports on ONE address got {answers} answers with a burst of 4 \
         — the limiter is keyed on the port and can be bypassed"
    );
    assert_eq!(
        state.lock().expect("lock").clients.len(),
        1,
        "one address must be one client, whatever port it uses"
    );
}

#[test]
fn one_clients_flood_does_not_deny_service_to_another() {
    let state = state_with(RateLimitConfig {
        interval_log2: 3,
        burst: 2,
        leak_shift: 2,
        global_rate_hz: 0.0,
        global_burst: 0.0,
    });
    let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
    for i in 0..100u64 {
        let req = NtpPacket::client_request(4, NtpTimestamp(i)).to_bytes();
        let _ = build_reply(&req, peer(9), recv, &state, &SystemClock);
    }
    // An innocent client still gets a real answer.
    let req = NtpPacket::client_request(4, NtpTimestamp(999)).to_bytes();
    let reply = build_reply(&req, peer(10), recv, &state, &SystemClock).expect("reply");
    let p = NtpPacket::parse(&reply).expect("parse");
    assert_ne!(p.stratum, 0, "innocent client received a Kiss-o-Death");
    assert_eq!(p.stratum, 2);
}

#[test]
fn interleaved_mode_reports_the_previous_exchanges_timestamps() {
    let state = test_state();
    let clock = SystemClock;
    let client = peer(3);

    // Exchange 1: basic, and the server learns this client.
    let recv1 = NtpTimestamp::from_unix(1_756_224_000, 0);
    let req1 = NtpPacket::client_request(4, NtpTimestamp(0xAAAA)).to_bytes();
    let reply1 = build_reply(&req1, client, recv1, &state, &clock).expect("reply 1");
    let p1 = NtpPacket::parse(&reply1).expect("parse 1");
    // Bit 0 is the server marking, so compare the rest.
    assert_eq!(p1.receive_ts.0 & !1, recv1.0 & !1);

    // The driver reports the true transmit timestamp after sending.
    let true_tx = NtpTimestamp::from_unix(1_756_224_000, 123_456);
    state
        .lock()
        .expect("lock")
        .clients
        .note_transmit(&client_key(client), true_tx);

    // Exchange 2: the client echoes the receive timestamp it was given (that is
    // how it asks for interleaved) and puts its own T4 in the request's
    // receive field, which the reply must echo back as origin.
    let recv2 = NtpTimestamp::from_unix(1_756_224_064, 0);
    let client_t4 = NtpTimestamp(0xC0FF_EE00_1234_5678);
    let mut req2 = NtpPacket::client_request(4, NtpTimestamp(0xBBBB));
    req2.origin_ts = p1.receive_ts;
    req2.receive_ts = client_t4;
    let reply2 = build_reply(&req2.to_bytes(), client, recv2, &state, &clock).expect("reply 2");
    let p2 = NtpPacket::parse(&reply2).expect("parse 2");

    // Receive is THIS exchange's (so the client can interleave again); only
    // transmit looks back. Bit 0 is the server marking.
    assert_eq!(
        p2.receive_ts.0 & !1,
        recv2.0 & !1,
        "interleaved reply must report the CURRENT receive timestamp"
    );
    assert_eq!(
        p2.transmit_ts.0 & !1,
        true_tx.0 & !1,
        "should report the true transmit of the previous response"
    );
    // THE rule chrony checks: an interleaved reply echoes the request's
    // RECEIVE field into origin. Echoing transmit instead makes the client
    // classify the reply as basic and silently misread stale timestamps —
    // observed live as a 146 ms offset error against chrony's `xleave`.
    assert_eq!(
        p2.origin_ts, client_t4,
        "interleaved reply must echo the request's receive field as origin"
    );
    assert_eq!(
        state
            .lock()
            .expect("lock")
            .clients
            .stats
            .interleaved_responses,
        1
    );
}

#[test]
fn a_client_not_asking_for_interleaved_gets_this_exchanges_timestamps() {
    let state = test_state();
    let clock = SystemClock;
    let client = peer(4);
    let recv1 = NtpTimestamp::from_unix(1_756_224_000, 0);
    let req1 = NtpPacket::client_request(4, NtpTimestamp(0xAAAA)).to_bytes();
    let _ = build_reply(&req1, client, recv1, &state, &clock).expect("reply 1");
    state.lock().expect("lock").clients.note_transmit(
        &client_key(client),
        NtpTimestamp::from_unix(1_756_224_000, 1),
    );

    // Origin left at zero (a plain client): basic mode.
    let recv2 = NtpTimestamp::from_unix(1_756_224_064, 0);
    let req2 = NtpPacket::client_request(4, NtpTimestamp(0xBBBB)).to_bytes();
    let reply2 = build_reply(&req2, client, recv2, &state, &clock).expect("reply 2");
    let p2 = NtpPacket::parse(&reply2).expect("parse 2");
    assert_eq!(
        p2.receive_ts.0 & !1,
        recv2.0 & !1,
        "basic mode must use this exchange"
    );
    // A basic reply echoes the request's TRANSMIT field as origin.
    assert_eq!(p2.origin_ts, NtpTimestamp(0xBBBB));
    // And the server marking must hold in both modes.
    assert_eq!(p2.receive_ts.0 & 1, 1, "receive must have bit 0 set");
    assert_eq!(p2.transmit_ts.0 & 1, 0, "transmit must have bit 0 clear");
    assert_eq!(
        state
            .lock()
            .expect("lock")
            .clients
            .stats
            .interleaved_responses,
        0
    );
}

#[test]
fn nts_round_trip_client_to_server() {
    let state = test_state();
    let keys = NtsKeys {
        c2s: [0x11; 32],
        s2c: [0x22; 32],
    };
    let cookie = {
        let guard = state.lock().expect("lock");
        rusty_time_nts::cookie::mint(&guard.ring, &keys, &[3; COOKIE_NONCE_LEN]).expect("mint")
    };

    let mut session = NtsSession::for_test(keys.clone(), vec![cookie]);
    let header = NtpPacket::client_request(4, NtpTimestamp(0xAABB_CCDD_1122_3344)).to_bytes();
    let request = session.protect(&header).expect("protect");

    let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
    let reply = build_reply(&request, peer(5), recv, &state, &SystemClock).expect("reply");

    assert_eq!(session.cookies_held(), 0, "cookie spent");
    session
        .verify(&reply)
        .expect("client verifies server reply");
    assert!(
        session.cookies_held() >= 1,
        "server must replenish the spent cookie"
    );
}

#[test]
fn forged_authenticator_gets_no_answer() {
    let state = test_state();
    let keys = NtsKeys {
        c2s: [0x11; 32],
        s2c: [0x22; 32],
    };
    let cookie = {
        let guard = state.lock().expect("lock");
        rusty_time_nts::cookie::mint(&guard.ring, &keys, &[3; COOKIE_NONCE_LEN]).expect("mint")
    };
    let mut session = NtsSession::for_test(keys, vec![cookie]);
    let header = NtpPacket::client_request(4, NtpTimestamp(1)).to_bytes();
    let mut request = session.protect(&header).expect("protect");
    let last = request.len() - 1;
    request[last] ^= 0xFF;
    let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
    assert!(
        build_reply(&request, peer(6), recv, &state, &SystemClock).is_none(),
        "server answered a forged NTS request"
    );
}

#[test]
fn unknown_cookie_gets_no_answer() {
    let state = test_state();
    let keys = NtsKeys {
        c2s: [0x11; 32],
        s2c: [0x22; 32],
    };
    let mut foreign = KeyRing::new(3);
    foreign.rotate_in(MasterKey {
        id: 7, // same id, different key
        key: [0xEE; 32],
    });
    let cookie =
        rusty_time_nts::cookie::mint(&foreign, &keys, &[3; COOKIE_NONCE_LEN]).expect("mint");
    let mut session = NtsSession::for_test(keys, vec![cookie]);
    let header = NtpPacket::client_request(4, NtpTimestamp(1)).to_bytes();
    let request = session.protect(&header).expect("protect");
    let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
    assert!(build_reply(&request, peer(7), recv, &state, &SystemClock).is_none());
}

#[test]
fn placeholder_count_cannot_amplify_without_bound() {
    let state = test_state();
    let keys = NtsKeys {
        c2s: [0x11; 32],
        s2c: [0x22; 32],
    };
    let cookie = {
        let guard = state.lock().expect("lock");
        rusty_time_nts::cookie::mint(&guard.ring, &keys, &[3; COOKIE_NONCE_LEN]).expect("mint")
    };
    let header = NtpPacket::client_request(4, NtpTimestamp(1)).to_bytes();
    let uid = [9u8; UNIQUE_ID_LEN];
    let nonce = [4u8; NONCE_LEN];
    let request =
        ef::protect_request(&header, &uid, &cookie, 60, &keys.c2s, &nonce).expect("protect");

    let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
    let reply = build_reply(&request, peer(8), recv, &state, &SystemClock).expect("reply");
    assert!(
        reply.len() <= request.len(),
        "reply ({}) exceeded request ({}): amplification",
        reply.len(),
        request.len()
    );
}

#[test]
fn malformed_datagrams_never_panic() {
    let state = test_state();
    let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
    let mut rng = 0xACE1_2345_6789_BEEFu64;
    for _ in 0..20_000 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let len = (rng % 200) as usize;
        let mut p = vec![0u8; len];
        for b in p.iter_mut() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *b = rng as u8;
        }
        if len > 0 {
            p[0] = (4 << 3) | 3; // bias toward "looks like a client request"
        }
        let _ = build_reply(&p, peer(11), recv, &state, &SystemClock);
    }
}
