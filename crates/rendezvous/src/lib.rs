//! seedwell-rendezvous: the client of a Seedwell rendezvous, and setting one up on the user's Cloudflare account
//! (SPEC §7G.3).
//!
//! A rendezvous is an address book the user runs on their own Cloudflare account: a Worker with one SQLite-backed
//! Durable Object per record (`tools/rendezvous-worker/worker.js`, embedded here as [`WORKER_JS`]). Devices keep a
//! sealed record of where they can be reached there and look up the records of devices they share something with
//! when those are not connected. The record format is `seedwell_sync_proto::rendezvous`; this crate moves opaque bytes.
//!
//! * [`client`]: `GET`/`PUT`/`DELETE /v1/r/<id>`, writes signed with a per-device write key ([`client::WriteKey`]),
//!   and the self-test.
//! * [`cloudflare`]: deploying the Worker with an API token through the Cloudflare REST API (no Node, no wrangler).
//! * `testkit` (feature): loopback fakes of the Worker and of the Cloudflare API.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod client;
pub mod cloudflare;
#[cfg(feature = "testkit")]
pub mod testkit;

/// The Worker's script, as `tools/rendezvous-worker/worker.js` has it (Seedwell uploads it as it is).
pub const WORKER_JS: &str = include_str!("../../../tools/rendezvous-worker/worker.js");

/// The Worker's name on the account (and the first label of its `workers.dev` address).
pub const SCRIPT_NAME: &str = "seedwell-rendezvous";

/// The Durable Object class in [`WORKER_JS`] and its binding.
pub const CLASS_NAME: &str = "Rendezvous";
/// See [`CLASS_NAME`].
pub const BINDING: &str = "RENDEZVOUS";
/// The Durable Object migration that creates [`CLASS_NAME`] (SQLite storage), as `wrangler.jsonc` has it.
pub const MIGRATION_TAG: &str = "v1";
/// The Worker's compatibility date (`wrangler.jsonc`).
pub const COMPATIBILITY_DATE: &str = "2026-10-01";

/// The API version the client speaks (`/v1/…`).
pub const API: u32 = 1;
/// The largest record the Worker keeps.
pub const MAX_RECORD: usize = 1024;

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Lower-case hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Bytes from hex (either case); `None` when it is not hex.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_embedded_worker_matches_the_constants_and_wrangler_config() {
        let js = super::WORKER_JS;
        assert!(js.contains("export class Rendezvous"), "class {}", super::CLASS_NAME);
        assert!(js.contains("env.RENDEZVOUS"), "binding {}", super::BINDING);
        assert!(js.contains("const MAX_RECORD = 1024;"));
        assert!(js.contains("const API = 1;"));
        assert!(!js.lines().any(|l| l.starts_with("import ")), "no imports: uploaded as one module");
        let w = include_str!("../../../tools/rendezvous-worker/wrangler.jsonc");
        for s in [super::SCRIPT_NAME, super::CLASS_NAME, super::BINDING, super::MIGRATION_TAG, super::COMPATIBILITY_DATE] {
            assert!(w.contains(&format!("\"{s}\"")), "wrangler.jsonc says {s}");
        }
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(super::unhex(&super::hex(&[0, 1, 0xab, 0xff])), Some(vec![0, 1, 0xab, 0xff]));
        assert_eq!(super::unhex("abc"), None);
        assert_eq!(super::unhex("zz"), None);
    }
}
