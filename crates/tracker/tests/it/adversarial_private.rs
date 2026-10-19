//! Adversarial audit: private-tracker containment (SPEC 7B.4, 7C.3 #7) and
//! passkey secrecy. Needs the `transport` feature for the redirect resolver.
#![cfg(feature = "transport")]

use crate::adversarial_support;

use adversarial_support::*;
use proptest::prelude::*;
use trs_tracker::http::resolve_redirect;
use trs_tracker::store::{TimingKey, TimingStore};
use trs_tracker::url::Scheme;
use trs_tracker::{AnnounceParams, Event, TorrentEvent, TrackerUrl, build_announce_url};

const SECRET: &str = "S3CR3TPASSKEY0123456789abcdef";

fn announce_params() -> AnnounceParams {
    AnnounceParams {
        info_hash: [3; 20],
        peer_id: *b"-SX0001-abcdefghijkl",
        port: 1,
        uploaded: 0,
        downloaded: 0,
        corrupt: 0,
        left: 10,
        key: 5,
        event: Some(Event::Started),
        numwant: 50,
        tracker_id: None,
        ipv4: None,
        ipv6: None,
        crypto: Default::default(),
    }
}

// ------------------------------------------------------------ redirect fuzz

fn authority_host(uri: &hyper::Uri) -> Option<String> {
    // `t.example.` and `t.example` are the same machine (and the gate budgets
    // them as one host): compare without the root dot.
    uri.host()
        .map(|h| h.to_ascii_lowercase().trim_end_matches('.').to_owned())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(5000), ..ProptestConfig::default() })]

    /// Whatever a (hostile) tracker puts in `Location`, if the resolver accepts
    /// it, the HTTP client (hyper's `Uri`) must connect to the *same host* the
    /// gate budgets and the torrent registered: no parser differential.
    #[test]
    fn accepted_redirects_always_stay_on_the_registered_host(
        scheme in prop_oneof![Just("http"), Just("https"), Just("HTTP"), Just("ftp"), Just("udp"), Just(""), Just("//")],
        host in prop_oneof![
            Just("t.example"), Just("T.EXAMPLE"), Just("evil.example"), Just("t.example.evil.example"),
            Just("t.example@evil.example"), Just("evil.example@t.example"), Just("t.example\\@evil.example"),
            Just("evil.example#@t.example"), Just("evil.example?@t.example"), Just("t.example#.evil.example"),
            Just("t.example:80@evil.example"), Just("t.example%2eevil.example"), Just("t.example.:80"),
            Just("[::1]"), Just("t.example:"), Just(""), Just("t.example\t.evil.example"),
            Just("t.example/@evil.example"), Just("t.example:99999"), Just("t.example:0x50"),
        ],
        tail in prop_oneof![Just(""), Just("/"), Just("/announce?x=1"), Just("?q"), Just("#frag"), Just("/a b"), Just("/..//x"), Just("/\u{e9}")],
        sep in prop_oneof![Just("://"), Just(":/"), Just(":///"), Just("")],
        relative in prop::bool::ANY,
    ) {
        let origin = TrackerUrl::parse("http://t.example/announce?passkey=abc").unwrap();
        let cur = "http://t.example/announce?passkey=abc&info_hash=x";
        let location = if relative {
            format!("/{host}{tail}")
        } else {
            format!("{scheme}{sep}{host}{tail}")
        };
        if let Some((sch, abs)) = resolve_redirect(&origin, Scheme::Http, cur, &location) {
            prop_assert!(sch != Scheme::Udp);
            // (a URI the client cannot parse is refused by it, which is safe)
            if let Ok(uri) = abs.parse::<hyper::Uri>() {
                let h = authority_host(&uri);
                prop_assert_eq!(h.as_deref(), Some("t.example"), "location {:?} -> {:?}", location, abs);
                prop_assert!(uri.authority().is_none_or(|a| !a.as_str().contains('@')), "userinfo in {:?}", abs);
            }
        }
    }

    /// For any URL the parser accepts, the host that hyper would connect to for
    /// the final announce URL equals `TrackerUrl::host()` (the gate's key).
    #[test]
    fn announce_url_host_matches_the_gate_key(
        host in prop_oneof![Just("tracker.example"), Just("TRACKER.Example"), Just("a-b.c_d.example"), Just("[2001:db8::1]"), Just("1.2.3.4"), Just("t.example:80@x"), Just("t.example\\x")],
        port in prop_oneof![Just(""), Just(":6969"), Just(":80"), Just(":443")],
        path in prop_oneof![Just("/announce"), Just("/a/b/announce?passkey=zz"), Just(""), Just("?passkey=q"), Just("/x#frag"), Just("/ann ounce"), Just("/%41nn"), Just("/a?b=c&d")],
        scheme in prop_oneof![Just("http"), Just("https"), Just("HTTP")],
    ) {
        let input = format!("{scheme}://{host}{port}{path}");
        if let Ok(u) = TrackerUrl::parse(&input) {
            let announce = build_announce_url(&u, &announce_params());
            if let Ok(uri) = announce.parse::<hyper::Uri>() {
                let h = authority_host(&uri).unwrap_or_default();
                let key = u.host();
                let want = if key.contains(':') { format!("[{key}]") } else { key.to_owned() };
                prop_assert_eq!(h, want, "input {:?} -> {:?}", input, announce);
                prop_assert_eq!(uri.port_u16().unwrap_or(if u.scheme() == Scheme::Https { 443 } else { 80 }), u.port());
            }
        }
    }
}

#[test]
fn handpicked_redirect_tricks_are_refused() {
    let origin = TrackerUrl::parse("https://t.example:8443/announce?passkey=abc").unwrap();
    let cur = "https://t.example:8443/announce?passkey=abc&x=1";
    for loc in [
        "http://t.example:8443/announce",            // downgrade
        "https://evil.example/announce?passkey=abc", // other host
        "https://t.example@evil.example/",           // userinfo
        "https://evil.example\\@t.example/",         // backslash
        "https://t.example.evil.example/",           // suffix trick
        "ftp://t.example/",
        "https://[::1]/",
        "javascript:alert(1)",
        "data:text/plain,hi",
        "",
        "   ",
    ] {
        assert!(
            resolve_redirect(&origin, Scheme::Https, cur, loc).is_none(),
            "{loc}"
        );
    }
}

#[test]
fn scheme_relative_location_is_treated_as_a_path_on_the_same_host() {
    // RFC 3986 says `//evil.example/x` is a network-path reference to ANOTHER
    // host. The resolver reads it as an absolute path on the current authority
    // (`http://t.example//evil.example/x`): wrong per RFC but it stays on the
    // registered host, which is what matters here.
    let origin = TrackerUrl::parse("http://t.example/announce?passkey=abc").unwrap();
    let (_, abs) = resolve_redirect(
        &origin,
        Scheme::Http,
        "http://t.example/announce?passkey=abc",
        "//evil.example/announce",
    )
    .unwrap();
    assert_eq!(abs.parse::<hyper::Uri>().unwrap().host(), Some("t.example"));
}

#[test]
fn redirect_to_another_port_of_the_same_host_is_followed_by_design() {
    // Documented in the audit: only the host is pinned, not the port. A MITM or a
    // compromised tracker host can bounce the announce (incl. passkey in the
    // original URL is NOT forwarded, only whatever Location carries) to any port
    // of the same host. Same host => same operator, so this is tolerated.
    let origin = TrackerUrl::parse("http://t.example/announce?passkey=abc").unwrap();
    let r = resolve_redirect(
        &origin,
        Scheme::Http,
        "http://t.example/announce?passkey=abc",
        "http://t.example:22/x",
    );
    assert!(r.is_some());
}

// ---------------------------------------------------------------- secrecy

#[test]
fn url_formatting_never_prints_the_passkey() {
    for raw in [
        format!("http://t.example/announce?passkey={SECRET}"),
        format!("https://t.example:8443/{SECRET}/announce"),
        format!("udp://t.example:6969/announce?pk={SECRET}"),
        format!("http://[2001:db8::1]:80/{SECRET}"),
    ] {
        let u = TrackerUrl::parse(&raw).unwrap();
        for s in [format!("{u}"), format!("{u:?}"), format!("{u:#?}")] {
            assert!(!s.contains(SECRET), "{s}");
        }
    }
    // rejected URLs: the error type carries no input
    for raw in [
        format!("ftp://t.example/{SECRET}"),
        format!("http://user:{SECRET}@t.example/"),
        format!("http://t.example:99999/{SECRET}"),
    ] {
        let e = TrackerUrl::parse(&raw).unwrap_err();
        assert!(!format!("{e} {e:?}").contains(SECRET));
    }
}

#[test]
fn hub_gate_and_report_debug_output_never_contain_the_passkey() {
    let url = format!("http://t.test/announce?passkey={SECRET}");
    let mut lab = Lab::new(Box::new(|sent| match sent.n {
        0 => vec![(d(1), ok(1800, None))],
        1 => vec![(d(1), timeout())],
        _ => vec![(d(1), reject(&format!("invalid passkey, see {}", "x")))],
    }));
    let sp = spec(1, &[&[url.as_str()]]);
    let sp_debug = format!("{sp:?} {sp:#?}");
    assert!(!sp_debug.contains(SECRET), "TorrentSpec Debug leaks");
    lab.add(sp);
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(8000));
    assert!(lab.sent.len() >= 3);
    let mut all = String::new();
    for r in &lab.reports {
        all.push_str(&format!("{r:?}\n{r:#?}\n"));
    }
    for e in &lab.gate_events {
        all.push_str(&format!("{e:?}\n"));
    }
    for x in &lab.sent {
        all.push_str(&format!("{:?}\n", x.p));
    }
    assert!(
        !all.contains(SECRET),
        "reports/events/params leak the passkey"
    );
    // the request object handed to the transports prints safely too
    let mut hub = trs_tracker::hub::TrackerHub::new(
        trs_tracker::gate::GateConfig::default(),
        trs_tracker::MemoryStore::new(),
        3,
    );
    hub.add_torrent(spec(2, &[&[url.as_str()]]), ms(0)).unwrap();
    hub.set_stats(
        &ih(2),
        trs_tracker::Stats {
            uploaded: 0,
            downloaded: 0,
            corrupt: 0,
            left: 1,
            size: 1,
        },
        ms(0),
    );
    hub.torrent_event(&ih(2), TorrentEvent::Start, ms(0));
    let mut reqs = vec![];
    for t in 0..20 {
        reqs.extend(hub.tick(ms(t * 1000)));
    }
    assert_eq!(reqs.len(), 1);
    let dbg = format!("{:?} {:#?} {:?}", reqs[0], reqs[0], reqs[0].ticket());
    assert!(!dbg.contains(SECRET), "GatedRequest Debug leaks: {dbg}");
}

/// FIXED audit defect (privacy, low): `TimingKey` derives `Debug` over the raw tracker URL and
/// `MemoryStore` derives `Debug` over a map keyed by it. Any `{:?}` of a store
/// key (e.g. in a store implementation's error log, or `dbg!(hub.store())`)
/// prints the passkey, contradicting "never logged" (SPEC 7B.4).
#[test]
fn bug_store_keys_leak_the_passkey_in_debug() {
    let key = TimingKey {
        info_hash: [1; 20],
        tracker_url: format!("http://t.test/announce?passkey={SECRET}"),
    };
    assert!(!format!("{key:?}").contains(SECRET));
    let mut st = trs_tracker::MemoryStore::new();
    st.save(&key, &Default::default()).unwrap();
    assert!(!format!("{st:?}").contains(SECRET));
}

#[test]
fn tracker_supplied_text_is_sanitised_but_may_echo_the_passkey() {
    // Documented limitation: `failure reason` is shown verbatim (SPEC 7C.4). A
    // tracker that echoes the passkey in its message (some do) puts it in the
    // UI string. The engine must treat Report::Halted/Warning text as sensitive.
    let body = format!(
        "d14:failure reason{}:bad passkey {SECRET}e",
        12 + SECRET.len()
    );
    let e = trs_tracker::parse_announce_response(body.as_bytes()).unwrap_err();
    assert!(e.to_string().contains(SECRET));
}

// ------------------------------------------------------------ containment

#[test]
fn hub_only_ever_contacts_urls_from_the_spec_even_with_hostile_answers() {
    // answers carry peers, external ip, tracker ids and redirects-as-errors;
    // none of that may add a destination
    let mut lab = Lab::new(Box::new(|sent| {
        let mut r = ok(300, None).unwrap();
        r.tracker_id = Some(b"http://evil.test/".to_vec().into_boxed_slice());
        r.warning = Some("please announce to http://evil.test/announce".into());
        r.peers = vec![trs_tracker::Peer {
            addr: "127.0.0.1:1".parse().unwrap(),
            peer_id: None,
        }];
        let _ = sent;
        vec![(d(1), Ok(r))]
    }));
    lab.add(spec(
        1,
        &[
            &["http://t.test/announce?pk=1"],
            &["http://u.test/announce?pk=1"],
        ],
    ));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(24 * 3600));
    assert!(
        lab.sent
            .iter()
            .all(|x| x.host == "t.test" || x.host == "u.test")
    );
    assert_eq!(lab.host_count("evil.test"), 0);
    // the tracker id is echoed percent-encoded, never as a URL
    let url = build_announce_url(
        &TrackerUrl::parse("http://t.test/announce?pk=1").unwrap(),
        &{
            let mut p = announce_params();
            p.tracker_id = Some(
                b"http://evil.test/&event=completed"
                    .to_vec()
                    .into_boxed_slice(),
            );
            p
        },
    );
    assert!(!url.contains("&event=completed"));
    assert!(url.contains("trackerid=http%3A%2F%2Fevil.test%2F%26event%3Dcompleted"));
}
