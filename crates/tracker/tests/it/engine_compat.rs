//! The calls `crates/engine/src/tracker_driver.rs` makes, spelled exactly as it
//! spells them, so a signature change that would break the engine fails here
//! first.
#![cfg(feature = "transport")]

use trs_tracker::gate::GateConfig;
use trs_tracker::http::HttpTransport;
use trs_tracker::hub::{TorrentSpec, TrackerHub};
use trs_tracker::machine::Report;
use trs_tracker::store::{StoreError, TimingKey, TimingRecord, TimingStore};
use trs_tracker::udp::UdpConfig;
use trs_tracker::url::Scheme;
use trs_tracker::{Millis, Stats, TorrentEvent, TrackerUrl};

struct BoxedTiming(Box<dyn TimingStore + Send>);

impl TimingStore for BoxedTiming {
    fn load(&self, key: &TimingKey) -> Option<TimingRecord> {
        self.0.load(key)
    }
    fn save(&mut self, key: &TimingKey, record: &TimingRecord) -> Result<(), StoreError> {
        self.0.save(key, record)
    }
}

#[test]
fn the_engine_driver_compiles_against_this_api_and_works() {
    let mut hub = TrackerHub::new(
        GateConfig::default(),
        BoxedTiming(Box::new(trs_tracker::MemoryStore::new())),
        7,
    );
    hub.hold_until(Millis(0));
    let _http = HttpTransport::new("1.0.0");
    let _udp_cfg = UdpConfig::default();
    let url = TrackerUrl::parse("http://t.example/announce?passkey=zz").unwrap();
    let mut spec = TorrentSpec::new([1; 20], [2; 20], 3, 6881, vec![vec![url.clone()], vec![]]);
    spec.numwant = 50;
    spec.added_complete = false;
    spec.private = true;
    let id = spec.info_hash;
    let now = Millis(1_000);
    assert!(hub.add_torrent(spec, now).is_ok());
    hub.set_stats(
        &id,
        Stats {
            uploaded: 0,
            downloaded: 0,
            left: 10,
            size: 10,
        },
        now,
    );
    hub.torrent_event(&id, TorrentEvent::Start, now);
    for req in hub.tick(Millis(10_000)) {
        let _ = req.ticket();
        assert!(matches!(
            req.url().scheme(),
            Scheme::Http | Scheme::Https | Scheme::Udp
        ));
        let _ = req.url().host().to_owned();
        hub.charge_extra(req.url().host(), 0);
        hub.complete(
            req.ticket(),
            Err(trs_tracker::AnnounceError::Transport(
                trs_tracker::TransportError::Connect,
            )),
            Millis(10_000),
        );
    }
    hub.manual_reannounce(&id, now);
    hub.network_changed(now);
    let _ = hub.next_wake(now).map(|w| w.since(now));
    for r in hub.take_reports() {
        match r.report {
            Report::Announced { .. } | Report::Halted { .. } | Report::Failed { .. } => {}
            _ => {}
        }
    }
    let _ = hub.take_gate_events();
    // tracker listing as `info()` does it
    let set = hub.set(&id).unwrap();
    let mut seen = 0;
    for (tier_idx, slots) in set.tiers().iter().enumerate() {
        for &slot in slots {
            let url = set.url(slot).unwrap();
            let _ = (tier_idx as u32, url.to_string());
            let (last, next, failures) = match set.machine(slot) {
                Some(m) => (m.record().last_announce_at, m.regular_at(), m.failures()),
                None => (None, None, 0),
            };
            let _ = (last.map(|m| m.0), next.map(|m| m.0), failures);
            seen += 1;
        }
    }
    assert_eq!(seen, 1);
    hub.torrent_event(&id, TorrentEvent::Stop, now);
    let _ = hub.is_quiescent(&id);
    let _ = hub.remove_torrent(&id);
}
