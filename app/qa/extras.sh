#!/usr/bin/env bash
# Two hand-made torrents on top of the standard scene (scenario.sh runs this; needs the app and the seeder):
#   * "Big Buck Bunny": a folder with an .mp4, an .en.srt and a poster.jpg, downloaded from the local seeder
#     (so it lands in Recently Finished and then in Sharing): the Play button of a folder torrent.
#   * "Hidden.Rivers.2016...": a torrent whose announce URLs carry passkeys (a path segment, passkey=,
#     authkey=), seeded in place: Get Info must show them masked. (Not flagged private: a pre-release
#     build refuses private torrents, SPEC 7B.6.)
set -euo pipefail
source "$(dirname "$0")/env.sh"
qa_guard
B="$QA_ROOT/extras-bunny"; P="$QA_ROOT/extras-private"
rm -rf "$B" "$P"
python3 -I "$REPO/app/qa/make_torrents.py" --out "$B" --special bunny --start 3000 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$P" --special private --start 3001 >/dev/null
"$REPO/target/release/seedwell" --socket "$QA_ROOT/seeder/control.sock" add "$B"/torrents/*.torrent --save-path "$B/data" >/dev/null
"$CLI" --socket "$SOCK" add "$B"/torrents/*.torrent --peer 127.0.0.1:7201 >/dev/null || echo "bunny add failed"
"$CLI" --socket "$SOCK" add "$P"/torrents/*.torrent --save-path "$P/data" >/dev/null || echo "private add failed"
