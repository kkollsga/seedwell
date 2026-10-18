//! Adversarial audit: hostile tracker bodies and datagrams.

use std::time::Duration;

use proptest::prelude::*;
use trs_tracker::response::{
    AnnounceError, MAX_BODY, MAX_INTERVAL, MAX_PEERS, MAX_TEXT_CHARS, MAX_TRACKER_ID, ParseError,
    parse_announce_response,
};
use trs_tracker::retry_after::parse_retry_after;
use trs_tracker::scrape::parse_scrape_response;
use trs_tracker::udp_proto::{self, Reply};

fn bytes(b: &[u8]) -> Vec<u8> {
    let mut v = format!("{}:", b.len()).into_bytes();
    v.extend_from_slice(b);
    v
}
fn int(i: i64) -> Vec<u8> {
    format!("i{i}e").into_bytes()
}
fn dict(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut e: Vec<_> = entries.to_vec();
    e.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = b"d".to_vec();
    for (k, v) in e {
        out.extend(bytes(k.as_bytes()));
        out.extend(v);
    }
    out.push(b'e');
    out
}

fn check_ok_invariants(r: &trs_tracker::AnnounceResponse) {
    assert!(r.interval <= MAX_INTERVAL);
    assert!(r.interval >= Duration::from_secs(60), "{:?}", r.interval);
    match r.min_interval {
        Some(m) => {
            assert!(m >= Duration::from_secs(60) && m <= MAX_INTERVAL);
            assert!(r.interval >= m);
        }
        None => assert!(r.interval >= Duration::from_secs(300)),
    }
    assert!(r.peers.len() <= MAX_PEERS);
    assert!(r.warning.as_ref().is_none_or(|w| w.chars().count() <= MAX_TEXT_CHARS && !w.chars().any(char::is_control)));
    assert!(
        r.tracker_id
            .as_ref()
            .is_none_or(|t| !t.is_empty() && t.len() <= MAX_TRACKER_ID)
    );
}

#[test]
fn oversize_and_boundary_bodies() {
    let mut big = b"d8:intervali900e5:peers".to_vec();
    big.extend(bytes(&vec![0u8; MAX_BODY]));
    big.push(b'e');
    assert!(big.len() > MAX_BODY);
    assert_eq!(
        parse_announce_response(&big).unwrap_err(),
        AnnounceError::Garbage(ParseError::TooLarge)
    );
    // exactly at the limit is still parsed (no off-by-one panic)
    let mut at = b"d8:intervali900e5:peers".to_vec();
    let pad = MAX_BODY - at.len() - 1 - "1048544:".len();
    at.extend(bytes(&vec![0u8; pad]));
    at.push(b'e');
    assert_eq!(at.len(), MAX_BODY);
    let r = parse_announce_response(&at).unwrap();
    assert!(r.peers.is_empty(), "zero ip / port skipped");
}

#[test]
fn deep_nesting_and_item_floods_are_rejected_without_blowing_the_stack() {
    for n in [64usize, 65, 1000, 500_000] {
        let mut b = b"d1:a".to_vec();
        b.extend(std::iter::repeat_n(b'l', n));
        b.extend(std::iter::repeat_n(b'e', n));
        b.push(b'e');
        let r = parse_announce_response(&b);
        if n >= 64 {
            assert!(r.is_err(), "depth {n}");
        }
    }
    // 200 000 tiny items in a 1 MiB body: over the item cap
    let mut b = b"d1:al".to_vec();
    for _ in 0..200_000 {
        b.extend_from_slice(b"i1e");
    }
    b.extend_from_slice(b"ee");
    assert!(b.len() < MAX_BODY);
    assert!(parse_announce_response(&b).is_err());
    // a million-key dict
    let mut b = b"d".to_vec();
    for i in 0..60_000u32 {
        b.extend(bytes(format!("{i:08}").as_bytes()));
        b.extend(int(1));
    }
    b.push(b'e');
    let _ = parse_announce_response(&b);
}

#[test]
fn hostile_intervals_are_always_clamped() {
    let cases: Vec<Vec<u8>> = vec![
        int(i64::MIN),
        int(-1),
        int(0),
        int(1),
        int(59),
        int(60),
        int(299),
        int(i64::MAX),
        bytes(b"900"),
        bytes(b"-5"),
        b"le".to_vec(),
        b"de".to_vec(),
    ];
    let mins: Vec<Option<Vec<u8>>> = vec![
        None,
        Some(int(i64::MIN)),
        Some(int(0)),
        Some(int(1)),
        Some(int(30)),
        Some(int(61)),
        Some(int(i64::MAX)),
        Some(bytes(b"x")),
    ];
    for iv in &cases {
        for m in &mins {
            let mut e = vec![("interval", iv.clone())];
            if let Some(m) = m {
                e.push(("min interval", m.clone()));
            }
            let r = parse_announce_response(&dict(&e)).unwrap();
            check_ok_invariants(&r);
        }
    }
}

#[test]
fn interval_equals_min_interval_equals_sixty_is_honoured_as_asked() {
    // Documented deviation: libtorrent never announces more often than every
    // 300 s whatever the tracker says (FINDINGS.md); we follow a tracker that
    // explicitly sets both to 60 s. A hostile/MITM response can therefore make
    // us announce once a minute, which is the SPEC 7C.1 rule ("respect with a 60 s floor").
    let r = parse_announce_response(&dict(&[("interval", int(60)), ("min interval", int(60))]))
        .unwrap();
    assert_eq!(r.interval, Duration::from_secs(60));
}

#[test]
fn failure_reason_wins_over_everything_and_is_sanitised() {
    let mut body = b"d14:failure reason".to_vec();
    let mut msg = b"line1\nline2\x00\x1b[31m".to_vec();
    msg.extend_from_slice(&[0xff, 0xfe, 0xc3, 0x28]);
    msg.extend(std::iter::repeat_n(b'x', 5000));
    body.extend(bytes(&msg));
    body.extend_from_slice(b"8:intervali1e5:peers6:");
    body.extend_from_slice(&[1, 2, 3, 4, 0x1a, 0xe1]);
    body.push(b'e');
    // keys must be sorted for our own encoder only; the decoder accepts any order
    match parse_announce_response(&body).unwrap_err() {
        AnnounceError::Rejected { reason, retry_in } => {
            assert!(reason.chars().count() <= MAX_TEXT_CHARS);
            assert!(!reason.chars().any(char::is_control));
            assert!(retry_in.is_none());
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn failure_reason_of_the_wrong_type_is_not_a_failure() {
    // Not bytes: ignored, the rest of the body is used. Documented leniency.
    let r = parse_announce_response(&dict(&[("failure reason", int(5)), ("interval", int(900))]));
    assert!(r.is_ok());
    let r = parse_announce_response(&dict(&[
        ("failure reason", bytes(b"")),
        ("interval", int(900)),
    ]));
    assert!(
        matches!(r, Err(AnnounceError::Rejected { .. })),
        "empty reason is still a rejection"
    );
}

#[test]
fn compact_peer_lists_with_odd_lengths_never_panic_and_never_misalign() {
    for len in 0..40usize {
        let peers: Vec<u8> = (0..len)
            .map(|i| (i as u8).wrapping_mul(37).wrapping_add(1))
            .collect();
        let r = parse_announce_response(&dict(&[("interval", int(900)), ("peers", bytes(&peers))]))
            .unwrap();
        assert!(r.peers.len() <= len / 6);
        let r6 =
            parse_announce_response(&dict(&[("interval", int(900)), ("peers6", bytes(&peers))]))
                .unwrap();
        assert!(r6.peers.len() <= len / 18);
    }
    // the cap holds for combined v4 + v6 + a million entries
    let v4: Vec<u8> = (0..30_000u32)
        .flat_map(|i| [10, (i >> 8) as u8, i as u8, 1, 0x1a, 0xe1])
        .collect();
    let v6: Vec<u8> = (0..5_000u32)
        .flat_map(|i| {
            let mut a = [0u8; 18];
            a[0] = 0x20;
            a[1] = 1;
            a[15] = (i % 250 + 1) as u8;
            a[16] = 0x1a;
            a[17] = 0xe1;
            a
        })
        .collect();
    let r = parse_announce_response(&dict(&[
        ("interval", int(900)),
        ("peers", bytes(&v4)),
        ("peers6", bytes(&v6)),
    ]))
    .unwrap();
    assert_eq!(r.peers.len(), MAX_PEERS);
}

#[test]
fn dictionary_peers_with_hostile_entries() {
    let entry = |ip: Vec<u8>, port: Vec<u8>| dict(&[("ip", ip), ("port", port)]);
    let mut list = b"l".to_vec();
    for (ip, port) in [
        (bytes(b"999.1.1.1"), int(80)),
        (bytes(b"1.2.3.4"), int(70_000)),
        (bytes(b"1.2.3.4"), int(-1)),
        (bytes(b"1.2.3.4"), int(0)),
        (bytes(b"0.0.0.0"), int(80)),
        (int(5), int(80)),
        (bytes(b"fe80::1%en0"), int(80)),
        (bytes(&[0xff, 0xfe]), int(80)),
        (bytes(b"example.com"), int(80)),
        (bytes(b" 5.6.7.8 "), int(6881)),
        (bytes(b"2001:db8::1"), int(6881)),
    ] {
        list.extend(entry(ip, port));
    }
    list.push(b'e');
    let r = parse_announce_response(&dict(&[("interval", int(900)), ("peers", list)])).unwrap();
    assert_eq!(r.peers.len(), 2, "{:?}", r.peers);
}

#[test]
fn tracker_id_warning_and_external_ip_limits() {
    let long_id = vec![b'a'; MAX_TRACKER_ID + 1];
    let ok_id = vec![b'a'; MAX_TRACKER_ID];
    let r = parse_announce_response(&dict(&[
        ("interval", int(900)),
        ("tracker id", bytes(&long_id)),
    ]))
    .unwrap();
    assert!(r.tracker_id.is_none());
    let r = parse_announce_response(&dict(&[
        ("interval", int(900)),
        ("tracker id", bytes(&ok_id)),
    ]))
    .unwrap();
    assert_eq!(r.tracker_id.as_deref(), Some(ok_id.as_slice()));
    let r = parse_announce_response(&dict(&[("interval", int(900)), ("tracker id", bytes(b""))]))
        .unwrap();
    assert!(r.tracker_id.is_none());
    let w = "w".repeat(10_000);
    let r = parse_announce_response(&dict(&[
        ("interval", int(900)),
        ("warning message", bytes(w.as_bytes())),
    ]))
    .unwrap();
    assert_eq!(r.warning.unwrap().chars().count(), MAX_TEXT_CHARS);
    for ip in [
        vec![1u8, 2, 3],
        vec![1u8; 5],
        vec![0u8; 17],
        b"not an ip".to_vec(),
    ] {
        let r = parse_announce_response(&dict(&[
            ("interval", int(900)),
            ("external ip", bytes(&ip)),
        ]))
        .unwrap();
        assert!(r.external_ip.is_none(), "{ip:?}");
    }
}

#[test]
fn html_and_whitespace_prefixed_bodies_are_garbage_not_success() {
    for body in [
        &b"<html><body>Just a moment...</body></html>"[..],
        b"",
        b"\n d8:intervali900ee",
        b"\xef\xbb\xbfd8:intervali900ee",
        b"HTTP/1.1 200 OK\r\n\r\nd8:intervali900ee",
        b"i42e",
        b"le",
        b"4:spam",
        b"d8:intervali900e",                 // truncated
        b"d8:intervali900e8:intervali900ee", // duplicate key
        b"d8:intervali0900ee",               // leading zero
    ] {
        let r = parse_announce_response(body);
        assert!(
            r.is_err(),
            "{:?} parsed as {:?}",
            String::from_utf8_lossy(body),
            r
        );
    }
    // tolerated: trailing garbage, unsorted keys
    assert!(parse_announce_response(b"d8:intervali900eeTRAILING").is_ok());
    assert!(parse_announce_response(b"d5:peers0:8:intervali900ee").is_ok());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(3000), ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_bytes_never_panic_and_ok_results_satisfy_the_clamps(b in proptest::collection::vec(any::<u8>(), 0..400)) {
        if let Ok(r) = parse_announce_response(&b) { check_ok_invariants(&r); }
    }

    #[test]
    fn structured_dictionaries_are_clamped(
        interval in prop::option::of(any::<i64>()),
        min in prop::option::of(any::<i64>()),
        peers in proptest::collection::vec(any::<u8>(), 0..200),
        peers6 in proptest::collection::vec(any::<u8>(), 0..200),
        warning in proptest::collection::vec(any::<u8>(), 0..50),
        failure in prop::option::of(proptest::collection::vec(any::<u8>(), 0..30)),
    ) {
        let mut e: Vec<(&str, Vec<u8>)> = vec![("peers", bytes(&peers)), ("peers6", bytes(&peers6)), ("warning message", bytes(&warning))];
        if let Some(i) = interval { e.push(("interval", int(i))); }
        if let Some(m) = min { e.push(("min interval", int(m))); }
        if let Some(f) = &failure { e.push(("failure reason", bytes(f))); }
        match parse_announce_response(&dict(&e)) {
            Ok(r) => { prop_assert!(failure.is_none()); check_ok_invariants(&r); }
            Err(AnnounceError::Rejected { .. }) => prop_assert!(failure.is_some()),
            Err(e) => prop_assert!(false, "unexpected {:?}", e),
        }
    }

    #[test]
    fn udp_datagrams_never_panic(b in proptest::collection::vec(any::<u8>(), 0..300), v6 in any::<bool>()) {
        if let Ok(Reply::Announce { response, .. }) = udp_proto::decode(&b, v6) {
            check_ok_invariants(&response);
        }
        let _ = parse_scrape_response(&b);
    }

    #[test]
    fn retry_after_never_panics_and_is_bounded_by_the_class(s in "\\PC{0,40}", now in any::<u64>()) {
        let r = parse_retry_after(&s, now);
        let e = AnnounceError::Http { status: 503, retry_after: r };
        if let trs_tracker::response::FailureClass::Backoff { hint: Some(h) } = e.class() {
            prop_assert!(h <= MAX_INTERVAL);
        }
    }
}

#[test]
fn udp_announce_reply_extremes() {
    let mut p = Vec::new();
    p.extend_from_slice(&1u32.to_be_bytes()); // announce
    p.extend_from_slice(&7u32.to_be_bytes()); // tid
    p.extend_from_slice(&u32::MAX.to_be_bytes()); // interval
    p.extend_from_slice(&u32::MAX.to_be_bytes()); // leechers
    p.extend_from_slice(&u32::MAX.to_be_bytes()); // seeders
    p.extend_from_slice(&[1, 2, 3, 4, 0x1a, 0xe1, 9]); // 1 peer + 1 stray byte
    match udp_proto::decode(&p, false).unwrap() {
        Reply::Announce { response, tid } => {
            assert_eq!(tid, 7);
            assert_eq!(response.interval, MAX_INTERVAL);
            assert_eq!(response.peers.len(), 1);
        }
        r => panic!("{r:?}"),
    }
    // interval 0 -> default, never an immediate re-announce
    p[8..12].copy_from_slice(&0u32.to_be_bytes());
    match udp_proto::decode(&p, false).unwrap() {
        Reply::Announce { response, .. } => assert!(response.interval >= Duration::from_secs(300)),
        r => panic!("{r:?}"),
    }
    // error packets are sanitised
    let mut e = Vec::new();
    e.extend_from_slice(&3u32.to_be_bytes());
    e.extend_from_slice(&9u32.to_be_bytes());
    e.extend_from_slice(b"bad\x00\x1b[2Jthing");
    match udp_proto::decode(&e, false).unwrap() {
        Reply::Error { message, .. } => assert!(!message.chars().any(char::is_control)),
        r => panic!("{r:?}"),
    }
}
