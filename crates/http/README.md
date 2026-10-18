# trs-http

Direct HTTP(S) downloads for Seedwell (SPEC 7D): large files from plain links
with pause, resume, throttling and queueing, staged in `.incomplete`, never
seeded and never shown in Sharing. Engine-agnostic: plain structs in, a
`RateGate` injected, progress and events out.

```text
Downloader::new(DownloaderConfig, Arc<dyn RateGate>) -> Downloader      (Clone, shares one client + per-host caps)
Downloader::start(DownloadRequest) -> Result<DownloadTask, Error>      (tokio runtime required)
Downloader::probe(url) -> Result<ProbeInfo, Error>                     (Range: bytes=0-0; name, type, size, ranges)
DownloadTask::{pause, resume, cancel(keep_part), set_rate_limit, progress, subscribe, events, wait}
list_staged(incomplete_dir) -> Vec<StagedInfo>      discard_staged(incomplete_dir, id)
trait RateGate { fn acquire(&self, bytes: usize) -> BoxFuture<'_, ()>; }   TokenBucket, Unlimited provided
```

`DownloadRequest { id, url, dest_dir, incomplete_dir, file_name, connections, sha256, rate_limit, start_paused }`.
`Progress { state, total, done, rate, connections, error, file_name, final_url, content_type, restarts, retries, final_path }`
is published through a `watch` channel (about 4 updates/s); `Event` (`Resolved`, `Restarted(reason)`, `Retrying`,
`Completed`, `Failed`, `StateChanged`) goes through a bounded `mpsc` channel (best effort).

## Behaviour

* **Staging.** `<incomplete>/<name>.part` (sparse, pre-sized when the length is known) plus `<name>.part.json`, a small
  line-oriented sidecar: id, URL, final URL, name, strong ETag, Last-Modified, total, `segmentable`, content type and the
  completed ranges. The sidecar is rewritten atomically (temp + fsync + rename) every `checkpoint_interval` (2 s), and
  always **after** `fsync` of the data it claims, so a crash can lose at most one interval of work and can never claim
  bytes that are not on disk. A new process resumes by passing the same `id` (or scan with `list_staged`).
  On completion: `fsync`, optional SHA-256, then a no-clobber move (hard link, else rename, else copy across volumes)
  into `dest_dir`, with ` (2)` collision suffixes; the sidecar is removed last.
* **Resume.** `Range` + `If-Range` (strong ETag, else `Last-Modified`). Weak ETags are not validators. With no
  validator the download restarts instead of appending blindly. A `206` is accepted only if `Content-Range` starts
  exactly at the requested offset, ends inside the requested range, agrees with `Content-Length` and the known total,
  and the response ETag / Last-Modified still match; a wrong range is a hard `RangeMismatch` (not one byte is
  written). A `200` to a range request, a changed validator, size or `.part` means: delete the staged files, tell the
  user (`Event::Restarted(reason)`), start from zero (at most 3 times per run).
* **Segments.** `connections > 1` is honoured only when the first `Range: bytes=0-1048575` probe answers `206`, the
  total is known and a validator exists. Work is a queue of 1-16 MiB pieces pulled by up to N workers, which balances
  slow connections adaptively and keeps the sidecar to a handful of ranges. The cap "Connections per host"
  (`max_connections_per_host`, default 4) is a semaphore per `host:port` shared by **all** tasks, including redirect
  hops. Fallback to a single connection is automatic.
* **Network.** Redirects <= 10 (loops detected), https -> http refused, only http/https, no URL credentials.
  Timeouts: connect 15 s, idle 60 s per frame. Retries with exponential backoff and jitter (50-100 % of
  `base * 2^n`, capped) for network errors, 408, 429 and 5xx, `Retry-After` honoured (seconds or HTTP date, capped
  by `max_retry_after`), `max_retries` consecutive failures without progress, then `RetriesExhausted{last}`. TLS
  verification failures are never retried. If a redirect target (often a signed URL) starts returning 401/403/404/410
  mid-download the original URL is tried once. User-Agent is exactly `Seedwell/<version>`; the only other headers are
  `Accept`, `Accept-Encoding: identity`, `Range`, `If-Range`. No cookies or auth headers (SPEC 12 #15).
* **Names.** `Content-Disposition` (RFC 6266, `filename*` UTF-8 / ISO-8859-1 wins) else the last path segment
  (percent-decoded; the redirect target's unless the original looks more like a file name), sanitized: no separators,
  `:`, control or bidi characters, no leading dots, <= 200 bytes keeping the extension.
* **Throttle.** Every <= 64 KiB is first granted by the per-download limiter and then by the injected global gate
  before it is written, so TCP back-pressure does the rest. `TokenBucket` is a smooth GCRA pacer (measured within 1 %
  of target; see tests).
* **Memory.** One 256 KiB buffer per connection plus one network frame; streaming positional writes (`pwrite`);
  nothing is buffered per file. Hash verification reads in 1 MiB blocks.
* **Errors.** Typed `Error`: `DiskFull` (ENOSPC/EDQUOT), `PermissionDenied` (EACCES/EROFS), `Io`, `Http{status}`,
  `RangeMismatch`, `ChecksumMismatch{expected, actual}` (the `.part` is kept, nothing is moved), ... After `Failed`,
  `resume()` retries. `cancel(false)` deletes `.part` and sidecar, `cancel(true)` keeps them. Dropping a
  `DownloadTask` aborts it like a crash (progress up to the last checkpoint stays resumable); `pause()` first for a
  lossless stop.

## Dependencies (kept to what `trs-tracker` already links)

| crate | why |
|---|---|
| `hyper` 1, `hyper-util` (client-legacy), `http-body-util`, `bytes` | the same HTTP/1.1 client as `trs-tracker`; one keep-alive pool |
| `hyper-rustls` 0.27 (`ring`, `webpki-roots`) | TLS with full certificate verification, same as the tracker |
| `tokio` (rt, net, time, sync, macros) | async runtime pieces: tasks, timers, `watch`/`mpsc`, `select!` |
| `sha2` 0.11 | optional SHA-256 verification (ARMv8 backend, same as storage/metainfo) |
| `httpdate` | `Retry-After` HTTP-date; already in the lock file via hyper |

No `serde` (the sidecar is a 10-line text format with a hardened parser), no `rand` (splitmix64 for jitter), no
`percent-encoding` (20 lines), no `unsafe`. Dev-only: `tempfile`, hyper's `server` feature for the test server.

## Tests

`cargo test -p trs-http` runs unit tests and `tests/download.rs`, which drives a local hyper server that can serve or
ignore ranges, change ETag mid-way, answer 429 + `Retry-After`, drop the connection mid-body, stall, trickle, redirect
(chains, loops, cross-host), and send assorted `Content-Disposition` headers. Fuzz targets (`fuzz/`):
`content_disposition` (parser + sanitizer + collision naming invariants) and `sidecar` (parser round trip):
`cargo +nightly fuzz run content_disposition`.
