# trs-rendezvous

The client of a Seedwell rendezvous, and its setup on the user's Cloudflare account (SPEC §7G.3). The Worker itself
is `tools/rendezvous-worker/worker.js` (embedded here as `WORKER_JS`, uploaded as it is); the record format, which
two devices must agree on, is `trs_sync_proto::rendezvous`; when to publish and whom to look up is
`trs_sync_net::rendezvous`. This crate moves opaque bytes.

- `client`: `GET /v1`, `GET`/`PUT`/`DELETE /v1/r/<id>`, writes signed with the device's write key for that rendezvous
  (`WriteKey`: random Ed25519, never the device key), the self-test, and the address as Seedwell keeps it
  (`normalize_url`: `https://` only, plain `http` for this computer alone).
- `cloudflare`: the setup with an API token through the Cloudflare REST API: token check, account, the module upload
  with its Durable Object binding and `new_sqlite_classes` migration, the workers.dev subdomain (made when the account
  has none) and route, the address read from the API, the self-test. The endpoints are cited in the module's
  documentation. The token lives only in `Api` and is wiped when it is dropped; it is never logged or stored.
- `testkit` (feature, tests only): loopback fakes of the Worker and of the Cloudflare API.

## Dependencies

- `trs-http`: the one HTTP(S) stack of the app (hyper + rustls), through `request_small`.
- `ring`: Ed25519 for the write key, SHA-256 for the signed text, random bytes (already in every crate of the tree).
- `serde_json`: the Cloudflare API's JSON and the upload's metadata.
- `tokio`: timers for the self-test's retries (and, with `testkit`, the fake servers' sockets).
