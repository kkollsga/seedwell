//! Interop against a real Transmission (libutp) on loopback. Needs `transmission-daemon`,
//! `transmission-create` and `transmission-remote` from Homebrew, and takes about 40 s (Transmission
//! only re-evaluates chokes every 10 s). Run with:
//! `cargo test -p trs-utp --test interop_transmission -- --ignored --nocapture`

#[test]
#[ignore = "needs Transmission 4.x installed; spawns daemons on ports 9291-9293 and 51890-51892"]
fn transmission_exchanges_a_torrent_over_utp_both_ways() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/interop/transmission.sh");
    let out = std::process::Command::new("bash").arg(script).output().expect("run script");
    let text = String::from_utf8_lossy(&out.stdout);
    println!("{text}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "interop script failed");
    assert!(text.contains("DOWNLOAD OK over uTP"), "direction A missing");
    assert!(text.contains("byte-exact"), "direction B missing");
    assert!(text.contains("INTEROP OK"));
}
