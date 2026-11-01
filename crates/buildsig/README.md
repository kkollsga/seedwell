# trs-buildsig

Signed development-build manifests (SPEC 12A, Development builds). A folder the app watches for builds (on the
user's devices: `<Seedwell Sync folder>/builds/`, shared by Folder Sync) holds each system's zip and one `build.json`
signed with the Ed25519 **build key**. The updater (`trs-ffi`'s `updates.rs`, on the Mac and on Windows) installs a
build from such a folder only when `build.json` verifies under a key compiled into this crate (`TRUSTED_KEYS`) and
the zip's SHA-256 is the one signed; anyone else who can write to the folder can drop files there, not builds.

```
build.json = {"manifest": "<manifest JSON text>", "key": "<public key hex>", "signature": "<hex>"}
signature  = Ed25519(build key, "seedwell-build-manifest/v1\n" ‖ manifest text)
manifest   = {"format": 1, "signed_ms": …, "builds": {"macos": {...}, "windows": {...}}}
build      = {"version", "commit", "zip", "sha256", "minimum_system"}
```

* **Where the key lives.** The secret key is made once on the publishing Mac (`tools/build-sign.sh key new`) and
  kept in its login Keychain (generic password, service `Seedwell build key`); `tools/build-sign.sh` reads it from
  there and hands it to the signer on standard input. It is never in the repository, an argument or a log.
* **How a device comes to trust it.** Its public key is in `TRUSTED_KEYS`: it comes with the app, from a release
  (Developer ID signed and notarized on the Mac, checked against `SHA256SUMS` on Windows). No device is asked to
  accept a key, and nothing in a shared folder can add one. A new or replaced key is trusted from the release that
  carries it.
* **The signer.** `seedwell-build-sign` (this crate's binary, a development tool, not shipped): `keygen`, `public`,
  `add <folder> <zip> --sha256 HEX` (the zip's system's entry replaced, the other system's kept while its zip is
  there and the old manifest verifies) and `verify <folder>`.

Dependencies:

| Crate | Why |
|---|---|
| `ring` | Ed25519 signing and verification, and random seeds (already the workspace's crypto: `trs-sync-proto`) |
| `serde`, `serde_json` | `build.json` and its manifest |
| `sha2` | the zips' SHA-256 (as `trs-ffi` computes it) |
