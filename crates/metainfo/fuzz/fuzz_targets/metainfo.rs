#![no_main]
use libfuzzer_sys::fuzz_target;
use trs_metainfo::Metainfo;

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = Metainfo::from_bytes(data) {
        // Invariants every accepted torrent must satisfy.
        assert!(m.file_count() > 0 && m.total_length > 0);
        let mut end = 0u64;
        for f in m.files() {
            assert!(!f.path.is_empty() && !f.path.starts_with('/'));
            for c in f.components() {
                assert!(!c.is_empty() && c != "." && c != ".." && !c.contains(['/', '\0', ':']));
            }
            end = end.max(f.offset + f.length);
        }
        assert!(end <= m.total_length);
        assert_eq!(m.piece_hashes().len(), 20 * m.piece_count());
        assert!(m.info_bytes(data).is_some());
        for i in 0..m.piece_count().min(4) {
            assert!(m.piece_hash(i).is_some() && m.piece_size(i).is_some());
        }
    }
    // The bare-info entry point takes untrusted network bytes (BEP 9).
    let _ = Metainfo::from_info_bytes(data);
});
