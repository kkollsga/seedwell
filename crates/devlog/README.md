# trs-devlog

The developer-mode log of tracker communication (SPEC 8.7). `Settings > Advanced > Developer mode`, or
`seedwell daemon --dev-mode`.

```
tracker task ──Entry (raw URL)──▶ DevLog::log ──mask──▶ JSON line ──bounded channel──▶ writer thread ──▶ trackers.log (+ .1, .2)
```

* **Logging boundary.** `Entry` carries the raw tracker URL; `DevLog::log` masks it (`trs_state::url::mask_secrets_for_log`:
  passkey / authkey / torrent_pass / pid ... values, 20+ alphanumeric, 32+ hex and base64-looking path segments, userinfo),
  masks every free text (`mask_text`), masks the `key` field and every credential-named field (also under a
  prefix like `announce.key`), keeps `info_hash` and our `peer_id`, and only then formats the line. The writer and the
  viewers only ever see masked text.
* **Zero cost off.** `DevLog::enabled()` is one relaxed atomic load; the engine builds no entry while it is false, and
  the writer thread does not exist until the first switch on.
* **Never blocks.** The channel holds 4096 lines (`Limits::queue`); a full channel drops the line and counts it
  (`dropped()`). A stalled writer is a test.
* **Files.** `trackers.log`, rotated at 10 MB, three files (`.1`, `.2`). `DevLog::clear()` deletes them and makes the
  writer forget lines queued before it.
* **Readers.** `read::tail` (newest N lines that pass a `Query`), `read::Follower` (`--follow`, survives rotation),
  `view::Row` (a line for people: summary + detail), used by the CLI (`seedwell log trackers`) and the app's viewer.

Line format: see SPEC 8.7. One JSON object per line; `kind` is `announce`, `scrape`, `decision`, `gate` or `quit`.
