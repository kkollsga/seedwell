//! A deterministic stand-in for the fuzz target (`fuzz/`): mutate valid request lines and random
//! bytes and check the decoder is total, bounded and self-consistent. Runs on stable in CI.

use trs_control::proto::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const SEEDS: &[&str] = &[
    r#"{"v":1,"id":1,"method":"hello","params":{"client":"claude","protocol":1}}"#,
    r#"{"id":2,"method":"list","params":{"filter":"downloading","limit":10,"tracker":"tl"}}"#,
    r#"{"id":3,"method":"add","params":{"source":"magnet:?xt=urn:btih:abcdef&dn=x","paused":true}}"#,
    r#"{"id":4,"method":"remove","params":{"ids":["aaaaaa","bbbbbb"],"delete_files":true}}"#,
    r#"{"id":"s","method":"settings_set","params":{"changes":{"transfers.download_limit":5,"a":[1,{"b":null}]}}}"#,
    r#"{"id":5,"method":"set_file_priorities","params":{"id":"aaaaaa","set":[{"index":0,"priority":"skip"}]}}"#,
    r#"{"id":6,"method":"set_seed_goal","params":{"ids":["aaaaaa"],"ratio":2.5,"at_goal":"remove"}}"#,
    r#"{"id":7,"method":"wait_for","params":{"ids":["aaaaaa"],"timeout_secs":5,"mode":"any"}}"#,
];

#[test]
fn every_method_decodes_with_empty_or_missing_params_without_panicking() {
    for m in Method::ALL {
        for params in ["null", "{}", "[]", "1", "\"x\"", "{\"ids\":[]}"] {
            let line = format!("{{\"id\":1,\"method\":\"{}\",\"params\":{params}}}", m.name());
            let _ = decode_request(line.as_bytes());
        }
    }
}

#[test]
fn mutated_requests_never_panic_and_valid_ones_roundtrip() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut decoded = 0;
    for round in 0..40_000 {
        let mut b = SEEDS[rng.below(SEEDS.len())].as_bytes().to_vec();
        for _ in 0..rng.below(6) {
            if b.is_empty() {
                break;
            }
            let i = rng.below(b.len());
            match rng.below(5) {
                0 => b[i] = rng.next() as u8,
                1 => {
                    b.remove(i);
                }
                2 => b.insert(i, rng.next() as u8),
                3 => b.truncate(i),
                _ => {
                    let j = rng.below(b.len());
                    b.swap(i, j);
                }
            }
        }
        if round % 4 == 0 {
            // Pure noise.
            b = (0..rng.below(300)).map(|_| rng.next() as u8).collect();
        }
        if let Ok(d) = decode_request(&b) {
            decoded += 1;
            let line = encode_request(&d.id, &d.request);
            let again = decode_request(&line[..line.len() - 1]).expect("own encoding decodes");
            assert_eq!(again.request.method(), d.request.method());
        }
    }
    assert!(decoded > 1000, "the mutator should still hit valid requests ({decoded})");
}

#[test]
fn hostile_shapes() {
    let big_array = format!("{{\"id\":1,\"method\":\"pause\",\"params\":{{\"ids\":[{}]}}}}", vec!["\"aaaaaa\""; 50_000].join(","));
    assert!(decode_request(big_array.as_bytes()).is_ok(), "large id lists decode (the server caps them at 1000)");
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    assert!(decode_request(deep.as_bytes()).is_err());
    let deep_obj = format!("{{\"id\":1,\"method\":\"ping\",\"params\":{}}}", "{\"a\":".repeat(10_000) + "1" + &"}".repeat(10_000));
    assert!(decode_request(deep_obj.as_bytes()).is_err());
    for s in [&b"\0\0\0"[..], b"{\"id\":1e999,\"method\":\"ping\"}", b"{\"id\":-1,\"method\":\"ping\"}", b"{\"id\":1.5,\"method\":\"ping\"}", b"{\"id\":18446744073709551616,\"method\":\"ping\"}"] {
        assert!(decode_request(s).is_err(), "{:?}", String::from_utf8_lossy(s));
    }
    // A numeric id at the u64 limit is fine.
    assert!(decode_request(br#"{"id":18446744073709551615,"method":"ping"}"#).is_ok());
}

#[test]
fn base64_decoder_is_total() {
    let mut rng = Rng(42);
    for _ in 0..5000 {
        let s: String = (0..rng.below(200)).map(|_| char::from(b" =+/-_AZaz09\n!"[rng.below(14)])).collect();
        let _ = trs_control::b64::decode(&s, 100);
    }
}
