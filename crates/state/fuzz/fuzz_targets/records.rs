#![no_main]
use libfuzzer_sys::fuzz_target;
use trs_state::{ResumeData, TorrentRecord};

// Binary records read from state.redb: decoding must not panic, and what decodes must re-encode stably.
fuzz_target!(|data: &[u8]| {
    if let Ok(r) = TorrentRecord::decode(data) {
        let again = TorrentRecord::decode(&r.encode()).expect("re-encoded record must decode");
        assert_eq!(again, r);
    }
    if let Ok(r) = ResumeData::decode(data) {
        let again = ResumeData::decode(&r.encode().expect("decoded resume data is valid")).expect("re-encoded resume must decode");
        assert_eq!(again, r);
    }
});
