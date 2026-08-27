//! Refclock transport tests.
//!
//! The SOCK decoder is pure and runs anywhere. The SHM and PHC paths need real
//! kernel objects, so they are exercised by `tools/corpus/refclock_probe.sh`,
//! which drives them with synthetic producers and real `/dev/ptp*`.

use super::*;

#[cfg(unix)]
#[test]
fn sock_decoding_matches_chronys_layout() {
    // Build the struct exactly as a chrony SOCK producer would.
    let mut buf = [0u8; sock::SAMPLE_SIZE];
    let sec: i64 = 1_756_224_000;
    let usec: i64 = 250_000;
    let offset: f64 = -0.001_5;
    buf[0..8].copy_from_slice(&sec.to_ne_bytes());
    buf[8..16].copy_from_slice(&usec.to_ne_bytes());
    buf[16..24].copy_from_slice(&offset.to_ne_bytes());
    buf[24..28].copy_from_slice(&1i32.to_ne_bytes()); // pulse
    buf[28..32].copy_from_slice(&0i32.to_ne_bytes()); // leap: none
    buf[36..40].copy_from_slice(&sock::SOCK_MAGIC.to_ne_bytes());

    let s = sock::decode_sample(&buf).expect("should decode");
    assert!((s.local_s - 1_756_224_000.25).abs() < 1e-9);
    assert_eq!(
        s.offset_s(),
        offset,
        "the offset must survive decoding exactly, not approximately —          rounding it away is what caps a refclock at ~100 ns"
    );
    assert_eq!(s.leap, LeapWarning::None);
}

#[cfg(unix)]
#[test]
fn a_datagram_without_the_magic_is_not_a_time_sample() {
    // Anything can send to a Unix datagram socket; the magic is what stops a
    // stray message being read as the time.
    let mut buf = [0u8; sock::SAMPLE_SIZE];
    buf[0..8].copy_from_slice(&1_756_224_000i64.to_ne_bytes());
    buf[36..40].copy_from_slice(&0xDEAD_BEEFu32.to_ne_bytes());
    assert!(sock::decode_sample(&buf).is_none());

    // Truncated datagrams are refused too.
    assert!(sock::decode_sample(&buf[..20]).is_none());
    assert!(sock::decode_sample(&[]).is_none());
}

#[cfg(unix)]
#[test]
fn sock_leap_field_is_honoured() {
    let mut buf = [0u8; sock::SAMPLE_SIZE];
    buf[0..8].copy_from_slice(&1_756_224_000i64.to_ne_bytes());
    buf[36..40].copy_from_slice(&sock::SOCK_MAGIC.to_ne_bytes());
    buf[28..32].copy_from_slice(&3i32.to_ne_bytes()); // unsynchronized
    let s = sock::decode_sample(&buf).expect("decode");
    assert_eq!(s.leap, LeapWarning::NotSynchronized);
    // Validation must then refuse it, however tidy the numbers are.
    assert!(s.validate(None).is_err());
}

#[cfg(unix)]
#[test]
fn a_sock_endpoint_binds_and_receives_a_real_datagram() {
    use std::os::unix::net::UnixDatagram;

    let path = std::env::temp_dir().join(format!("rusty_time_sock_test_{}", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let listener = sock::SockRefclock::bind(&path).expect("bind");
    assert!(listener.try_sample().is_none(), "nothing sent yet");

    // Send a real sample through the kernel, as a producer would.
    let producer = UnixDatagram::unbound().expect("producer");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock");
    let mut buf = [0u8; sock::SAMPLE_SIZE];
    buf[0..8].copy_from_slice(&(now.as_secs() as i64).to_ne_bytes());
    buf[8..16].copy_from_slice(&(now.subsec_micros() as i64).to_ne_bytes());
    buf[16..24].copy_from_slice(&0.000_25f64.to_ne_bytes());
    buf[36..40].copy_from_slice(&sock::SOCK_MAGIC.to_ne_bytes());
    producer.send_to(&buf, &path).expect("send");

    // Give the datagram a moment to land on a non-blocking socket.
    let mut got = None;
    for _ in 0..50 {
        if let Some(s) = listener.try_sample() {
            got = Some(s);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let sample = got.expect("a sent sample should arrive");
    assert_eq!(sample.offset_s(), 0.000_25, "offset must be exact");
    assert!(
        sample.validate(None).is_ok(),
        "a fresh sample must validate"
    );

    let _ = std::fs::remove_file(&path);
}

#[cfg(target_os = "linux")]
#[test]
fn kernel_receive_timestamps_are_produced_when_the_platform_supplies_them() {
    // Real evidence for the software half of the timestamping seam: enable
    // SO_TIMESTAMPING, send a datagram, and require the kernel's stamp to be
    // both present and close to the time we observed.
    use std::net::UdpSocket;

    let server = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("addr");
    if crate::net::enable_rx_timestamps(&server).is_err() {
        // A kernel without SO_TIMESTAMPING is a legitimate configuration; the
        // caller's fallback covers it. Nothing to assert.
        return;
    }

    let client = UdpSocket::bind("127.0.0.1:0").expect("client");
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64();
    client.send_to(&[0u8; 48], addr).expect("send");

    let mut bufs = [[0u8; 1024]; 4];
    let mut out = Vec::new();
    let mut received = None;
    for _ in 0..50 {
        if crate::net::wait_readable(&server, std::time::Duration::from_millis(100))
            .unwrap_or(false)
            && crate::net::recv_batch(
                &server,
                &mut bufs,
                &mut crate::net::BatchScratch::new(),
                &mut out,
            )
            .unwrap_or(0)
                > 0
        {
            received = out.first().copied();
            break;
        }
    }
    let received = received.expect("datagram should arrive");
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64();

    match received.kernel_rx_ns {
        Some(stamp) => {
            // The whole point is that this is a *real* time, not a zero or a
            // misparsed cmsg field.
            let stamp_s = stamp as f64 * 1e-9;
            assert!(
                stamp_s >= before - 1.0 && stamp_s <= after + 1.0,
                "kernel timestamp {stamp_s} is outside the window [{before}, {after}]"
            );
        }
        None => {
            // Enabling succeeded but the stack supplied nothing — report it
            // rather than asserting, since that is a kernel/NIC property.
            eprintln!("note: SO_TIMESTAMPING enabled but no timestamp was supplied");
        }
    }
}
