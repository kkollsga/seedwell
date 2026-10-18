use super::*;
use proptest::prelude::*;

fn ok(input: &[u8]) -> Value<'_> {
    decode(input)
        .unwrap_or_else(|e| panic!("{:?} should decode: {e}", String::from_utf8_lossy(input)))
}

fn err(input: &[u8]) -> Error {
    match decode(input) {
        Ok(v) => panic!(
            "{:?} should fail, got {v:?}",
            String::from_utf8_lossy(input)
        ),
        Err(e) => e,
    }
}

#[test]
fn ints() {
    assert_eq!(ok(b"i0e"), Value::Int(0));
    assert_eq!(ok(b"i42e"), Value::Int(42));
    assert_eq!(ok(b"i-42e"), Value::Int(-42));
    assert_eq!(ok(b"i9223372036854775807e"), Value::Int(i64::MAX));
    assert_eq!(ok(b"i-9223372036854775808e"), Value::Int(i64::MIN));
}

#[test]
fn int_syntax_errors() {
    for bad in [
        &b"ie"[..],
        b"i-e",
        b"i-0e",
        b"i00e",
        b"i01e",
        b"i-01e",
        b"i1x2e",
        b"i 1e",
        b"i+1e",
    ] {
        assert!(
            matches!(err(bad), Error::InvalidInteger { .. }),
            "{:?}",
            bad
        );
    }
    assert_eq!(err(b"i12"), Error::UnexpectedEof);
    assert_eq!(err(b"i"), Error::UnexpectedEof);
    assert_eq!(err(b"i-"), Error::UnexpectedEof);
}

#[test]
fn int_overflow() {
    assert!(matches!(
        err(b"i9223372036854775808e"),
        Error::IntegerOverflow { .. }
    ));
    assert!(matches!(
        err(b"i-9223372036854775809e"),
        Error::IntegerOverflow { .. }
    ));
    assert!(matches!(
        err(b"i99999999999999999999999999e"),
        Error::IntegerOverflow { .. }
    ));
    assert!(matches!(
        err(b"i18446744073709551616e"),
        Error::IntegerOverflow { .. }
    ));
}

#[test]
fn byte_strings() {
    assert_eq!(ok(b"0:"), Value::Bytes(b""));
    assert_eq!(ok(b"4:spam"), Value::Bytes(b"spam"));
    assert_eq!(ok(b"3:\x00\xff\n"), Value::Bytes(b"\x00\xff\n"));
    assert_eq!(ok(b"10:0123456789"), Value::Bytes(b"0123456789"));
}

#[test]
fn byte_string_errors() {
    assert!(matches!(err(b"00:"), Error::InvalidLength { .. }));
    assert!(matches!(err(b"01:a"), Error::InvalidLength { .. }));
    assert!(matches!(err(b"4x:spam"), Error::InvalidLength { .. }));
    assert!(matches!(
        err(b"99999999999999999999999:a"),
        Error::InvalidLength { .. }
    ));
    assert_eq!(err(b"5:abc"), Error::UnexpectedEof);
    assert_eq!(err(b"4"), Error::UnexpectedEof);
    // A length that would overflow `pos + len` must not wrap.
    assert_eq!(err(b"18446744073709551615:a"), Error::UnexpectedEof);
    assert!(matches!(err(b":abc"), Error::InvalidByte { .. }));
    assert!(matches!(err(b"-1:a"), Error::InvalidByte { .. }));
}

#[test]
fn lists_and_dicts() {
    assert_eq!(ok(b"le"), Value::List(vec![]));
    assert_eq!(ok(b"de"), Value::Dict(vec![]));
    assert_eq!(
        ok(b"l4:spami42ee"),
        Value::List(vec![Value::Bytes(b"spam"), Value::Int(42)])
    );
    let v = ok(b"d3:cow3:moo4:spam4:eggse");
    assert_eq!(v.get(b"cow").and_then(Value::as_str), Some("moo"));
    assert_eq!(v.get(b"spam").and_then(Value::as_bytes), Some(&b"eggs"[..]));
    assert!(v.get(b"missing").is_none());
    assert!(v.is_canonical());
}

#[test]
fn structure_errors() {
    assert_eq!(err(b"l"), Error::UnexpectedEof);
    assert_eq!(err(b"li1e"), Error::UnexpectedEof);
    assert_eq!(err(b"d"), Error::UnexpectedEof);
    assert_eq!(err(b"d3:key"), Error::UnexpectedEof);
    assert!(matches!(
        err(b"d3:keyee"),
        Error::TrailingData { .. } | Error::InvalidByte { .. }
    ));
    assert!(
        matches!(err(b"di1e3:fooe"), Error::InvalidByte { .. }),
        "non-string key"
    );
    assert!(matches!(err(b"x"), Error::InvalidByte { .. }));
    assert!(matches!(err(b"e"), Error::InvalidByte { .. }));
    assert_eq!(err(b""), Error::UnexpectedEof);
}

#[test]
fn trailing_data_rejected_but_prefix_returns_rest() {
    assert_eq!(err(b"i1ei2e"), Error::TrailingData { pos: 3 });
    let (v, rest) = decode_prefix(b"i1ei2e").unwrap();
    assert_eq!(v, Value::Int(1));
    assert_eq!(rest, b"i2e");
    // BEP 9 data messages: dict followed by raw piece bytes.
    let (v, rest) = decode_prefix(b"d8:msg_typei1e5:piecei0ee\x01\x02\x03").unwrap();
    assert_eq!(v.get(b"piece").and_then(Value::as_int), Some(0));
    assert_eq!(rest, b"\x01\x02\x03");
}

#[test]
fn unsorted_dict_accepted_but_not_canonical() {
    let input = b"d1:bi1e1:ai2ee";
    let v = ok(input);
    assert!(!v.is_canonical());
    // Original order is preserved.
    assert_eq!(v.as_dict().unwrap()[0].0, b"b");
    // Re-encoding sorts, so it differs from the input.
    assert_eq!(encode(&v), b"d1:ai2e1:bi1ee");
}

#[test]
fn nested_unsorted_is_not_canonical() {
    assert!(!ok(b"ld1:bi1e1:ai1eee").is_canonical());
    assert!(ok(b"ld1:ai1e1:bi1eee").is_canonical());
}

#[test]
fn duplicate_keys_rejected() {
    assert!(matches!(err(b"d1:ai1e1:ai2ee"), Error::DuplicateKey { .. }));
    // Non-adjacent duplicate in an unsorted dict.
    assert!(matches!(
        err(b"d1:ai1e1:bi1e1:ai2ee"),
        Error::DuplicateKey { .. }
    ));
    // The same key in different dicts is fine.
    ok(b"d1:ad1:ai1ee1:bd1:ai1eee");
}

#[test]
fn depth_limit() {
    let nest = |n: usize| {
        let mut s = vec![b'l'; n];
        s.extend(std::iter::repeat_n(b'e', n));
        s
    };
    assert!(decode(&nest(MAX_DEPTH)).is_ok());
    assert_eq!(err(&nest(MAX_DEPTH + 1)), Error::DepthExceeded);
    // Hostile input: far deeper than the stack could take, but only 1 byte each.
    let deep = vec![b'l'; 1_000_000];
    assert_eq!(err(&deep), Error::DepthExceeded);
    let deep_d = b"d1:a".repeat(1_000_000);
    assert_eq!(err(&deep_d), Error::DepthExceeded);
}

#[test]
fn item_and_size_limits() {
    let limits = DecodeLimits {
        max_items: 3,
        max_bytes: 1024,
    };
    assert!(decode_with_limits(b"li1ei2ee", limits).is_ok()); // list + 2 ints
    assert_eq!(
        decode_with_limits(b"li1ei2ei3ee", limits).unwrap_err(),
        Error::TooManyItems
    );
    // Dict keys count: dict + key + value = 3.
    assert!(decode_with_limits(b"d1:ai1ee", limits).is_ok());
    assert_eq!(
        decode_with_limits(b"d1:ai1e1:bi1ee", limits).unwrap_err(),
        Error::TooManyItems
    );
    let small = DecodeLimits {
        max_items: 100,
        max_bytes: 4,
    };
    assert_eq!(decode_with_limits(b"i1e", small).unwrap(), Value::Int(1));
    assert_eq!(
        decode_with_limits(b"li1ee", small).unwrap_err(),
        Error::InputTooLarge
    );
}

#[test]
fn raw_spans_are_exact_original_bytes() {
    // Unsorted info dict with odd spacing-free but non-canonical ordering.
    let info = b"d6:lengthi5e4:name1:x12:piece lengthi4ee";
    let mut doc = Vec::new();
    doc.extend_from_slice(b"d8:announce3:url4:info");
    doc.extend_from_slice(info);
    doc.extend_from_slice(b"e");
    assert_eq!(raw_dict_get(&doc, b"info").unwrap(), Some(&info[..]));
    // Same bytes, not a re-encoding: the slice points into the input buffer.
    let raw = raw_dict_get(&doc, b"info").unwrap().unwrap();
    assert!(std::ptr::eq(
        raw.as_ptr(),
        doc[doc.len() - 1 - info.len()..].as_ptr()
    ));
    assert_eq!(raw_dict_get(&doc, b"nope").unwrap(), None);
    // Composes.
    assert_eq!(raw_dict_get(raw, b"name").unwrap(), Some(&b"1:x"[..]));
}

#[test]
fn raw_entries_and_lists() {
    let doc = b"d1:ali1ei2ee1:bd1:ci3eee";
    let entries = raw_dict_entries(doc).unwrap();
    assert_eq!(
        entries,
        vec![(&b"a"[..], &b"li1ei2ee"[..]), (&b"b"[..], &b"d1:ci3ee"[..])]
    );
    assert_eq!(
        raw_list_items(b"li1e1:ad1:ai1eee").unwrap(),
        vec![&b"i1e"[..], b"1:a", b"d1:ai1ee"]
    );
}

#[test]
fn raw_api_validates_like_decode() {
    assert!(matches!(
        raw_dict_entries(b"d1:ai01ee"),
        Err(Error::InvalidInteger { .. })
    ));
    assert!(matches!(
        raw_dict_entries(b"d1:ai1e1:ai2ee"),
        Err(Error::DuplicateKey { .. })
    ));
    assert!(matches!(
        raw_dict_entries(b"d1:bi1e1:ai1e1:bi2ee"),
        Err(Error::DuplicateKey { .. })
    ));
    assert!(matches!(
        raw_dict_entries(b"d1:ai1ee0"),
        Err(Error::TrailingData { .. })
    ));
    assert_eq!(
        raw_dict_entries(b"li1ee").unwrap_err(),
        Error::UnexpectedType
    );
    assert_eq!(raw_dict_entries(b"").unwrap_err(), Error::UnexpectedEof);
    assert_eq!(raw_list_items(b"de").unwrap_err(), Error::UnexpectedType);
    assert_eq!(raw_dict_entries(b"d1:a").unwrap_err(), Error::UnexpectedEof);
    let deep = [b"d1:a".repeat(70), b"i1e".to_vec(), b"e".repeat(70)].concat();
    assert_eq!(raw_dict_entries(&deep).unwrap_err(), Error::DepthExceeded);
}

#[test]
fn accessors_on_wrong_types() {
    let v = Value::Int(1);
    assert!(v.as_bytes().is_none() && v.as_list().is_none() && v.as_dict().is_none());
    assert!(v.get(b"a").is_none() && v.as_str().is_none());
    assert_eq!(Value::Bytes(&[0xff]).as_str(), None);
    assert_eq!(Value::Bytes(b"hi").as_int(), None);
}

#[test]
fn encoder_basics() {
    let mut e = Encoder::new();
    e.begin_dict()
        .str("a")
        .int(-5)
        .str("b")
        .begin_list()
        .bytes(b"\x00")
        .end()
        .end();
    assert_eq!(e.as_slice(), b"d1:ai-5e1:bl1:\x00ee");
    assert_eq!(encode(&Value::Int(i64::MIN)), b"i-9223372036854775808e");
    let mut e = Encoder::with_capacity(8);
    e.raw(b"i1e");
    assert_eq!(e.finish(), b"i1e");
}

#[test]
fn encode_sorts_keys_bytewise() {
    let v = Value::Dict(vec![
        (b"b", Value::Int(1)),
        (b"a", Value::Int(2)),
        (b"B", Value::Int(3)),
    ]);
    assert_eq!(encode(&v), b"d1:Bi3e1:ai2e1:bi1ee");
}

#[test]
fn real_world_shaped_document_roundtrips() {
    let doc = b"d8:announce20:http://t.example/ann4:infod6:lengthi3e4:name1:a12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
    let v = ok(doc);
    assert!(v.is_canonical());
    assert_eq!(encode(&v), doc);
}

// ---------------------------------------------------------------- proptest

/// Owned mirror of `Value` so strategies can own their data.
#[derive(Debug, Clone)]
enum Owned {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Owned>),
    Dict(std::collections::BTreeMap<Vec<u8>, Owned>),
}

impl Owned {
    fn borrow(&self) -> Value<'_> {
        match self {
            Owned::Int(i) => Value::Int(*i),
            Owned::Bytes(b) => Value::Bytes(b),
            Owned::List(l) => Value::List(l.iter().map(Owned::borrow).collect()),
            Owned::Dict(d) => {
                Value::Dict(d.iter().map(|(k, v)| (k.as_slice(), v.borrow())).collect())
            }
        }
    }
}

fn owned() -> impl Strategy<Value = Owned> {
    let leaf = prop_oneof![
        any::<i64>().prop_map(Owned::Int),
        prop::collection::vec(any::<u8>(), 0..32).prop_map(Owned::Bytes),
    ];
    leaf.prop_recursive(5, 64, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Owned::List),
            prop::collection::btree_map(prop::collection::vec(any::<u8>(), 0..8), inner, 0..6)
                .prop_map(Owned::Dict),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn encode_then_decode_roundtrips(o in owned()) {
        let v = o.borrow();
        let bytes = encode(&v);
        let back = decode(&bytes).unwrap();
        prop_assert_eq!(&back, &v);
        prop_assert!(back.is_canonical());
        // Canonical output is a fixed point.
        prop_assert_eq!(encode(&back), bytes);
    }

    #[test]
    fn decode_never_panics_and_canonical_inputs_reencode_identically(
        data in prop::collection::vec(any::<u8>(), 0..256)
    ) {
        if let Ok(v) = decode(&data)
            && v.is_canonical() {
                prop_assert_eq!(&encode(&v), &data);
            }
        let _ = decode_prefix(&data);
        let _ = raw_dict_entries(&data);
        let _ = raw_list_items(&data);
    }

    #[test]
    fn raw_spans_match_decoded_values(o in owned()) {
        if let Owned::Dict(_) = o {
            let v = o.borrow();
            let bytes = encode(&v);
            for (k, raw) in raw_dict_entries(&bytes).unwrap() {
                prop_assert_eq!(&encode(v.get(k).unwrap()), raw);
                prop_assert!(decode(raw).is_ok());
            }
        }
    }

    #[test]
    fn any_i64_roundtrips(n in any::<i64>()) {
        let bytes = encode(&Value::Int(n));
        prop_assert_eq!(decode(&bytes).unwrap(), Value::Int(n));
    }
}
