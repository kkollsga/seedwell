#![no_main]
//! Feeds arbitrary bytes to the frame splitter and the message decoder. Must never panic, and whatever decodes
//! must encode and decode again to the same message.

use libfuzzer_sys::fuzz_target;
use trs_sync_proto::codec::Frame;
use trs_sync_proto::msg::Message;

fuzz_target!(|data: &[u8]| {
    // As a stream of frames (the first 4 bytes of each are its length).
    let mut rest = data;
    while let Ok(Some((frame, used))) = Frame::split(rest) {
        check(frame);
        rest = &rest[used..];
    }
    // As one payload, the first byte choosing the type, so short inputs reach every decoder.
    if let Some((&kind, payload)) = data.split_first() {
        check(Frame { kind: kind % 16, flags: 0, payload });
    }
});

fn check(frame: Frame<'_>) {
    if let Ok(m) = Message::decode(frame) {
        let mut out = Vec::new();
        m.encode(&mut out);
        let again = Frame::split(&out).ok().flatten().map(|(f, _)| Message::decode(f));
        assert_eq!(again, Some(Ok(m)), "encode then decode changed the message");
    }
}
