#![no_main]
use libfuzzer_sys::fuzz_target;
use trs_classify::Category;
use trs_state::rules::TorrentAttrs;
use trs_state::{Document, RuleSet};

// Input: rules.toml text, then a NUL and a few attribute bytes. Resolution must be total.
fuzz_target!(|data: &[u8]| {
    let (doc, attrs) = match data.iter().position(|b| *b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    let text = String::from_utf8_lossy(doc);
    if let Ok(p) = RuleSet::parse_str(&text) {
        let out = p.value.render().expect("a validated rule set must render");
        let back = RuleSet::parse_str(&out).expect("rendered rules must parse");
        assert_eq!(back.value, p.value);
        let b = |i: usize| attrs.get(i).copied().unwrap_or(0);
        let tracker = ["a", "b", "tl", "public"][(b(0) % 4) as usize];
        let linked = ["a", "b", "tl", "x"][(b(1) % 4) as usize];
        let a = TorrentAttrs {
            tracker: if b(2) % 2 == 0 { Some(tracker) } else { None },
            linked_rule: if b(3) % 2 == 0 { Some(linked) } else { None },
            private: b(4) % 2 == 0,
            category: Category::ALL[(b(5) as usize) % Category::ALL.len()],
        };
        let r1 = p.value.resolve(&a);
        assert_eq!(r1, p.value.resolve(&a));
        let _ = r1.goal_reached(f64::from(b(6)) / 10.0, trs_state::duration::Secs(u64::from(b(7)) * 3600));
    }
});
