#!/usr/bin/env bash
# The full baseline campaign, in the order used for bench/reports/<date>-baseline.md.
# Usage: bench/scripts/run-baseline.sh [campaign-id]     (about 3-4 hours; strictly sequential)
# TorrentRS is included when TRS_BENCH_BIN points at an executable (see bench/README.md).
set -u
cd "$(dirname "$0")/../.."
C="${1:-$(date +%Y-%m-%d)}"
CLIENTS="transmission,qbittorrent"
[ -n "${TRS_BENCH_BIN:-}" ] && [ -x "$TRS_BENCH_BIN" ] && CLIENTS="$CLIENTS,torrentrs"
B="python3 -I bench/scripts/bench.py"
$B fetch-torrents
$B netcheck --campaign "$C"
$B download --campaign "$C" --resume --clients "$CLIENTS" --plan debian:2,arch:1 --budget-gb 15
$B scale    --campaign "$C" --resume --clients "$CLIENTS" --sizes 50,200,500
$B hot      --campaign "$C" --resume --clients "$CLIENTS" --n 200
$B report   --campaign "$C"
