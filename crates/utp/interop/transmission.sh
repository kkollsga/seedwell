#!/usr/bin/env bash
# uTP interop against a real Transmission (libutp) on loopback.
#
#   A) Transmission SEEDS, we connect over uTP, do the BEP 3 handshake, and download the whole
#      payload block by block; bytes are compared with the source file.
#   B) We SEED over uTP only (no TCP listener at all), Transmission LEECHES: it learns our address
#      from a tiny HTTP tracker, dials us over uTP, and must finish a byte-exact download.
#
# Never touches the user's Transmission.app preferences: both daemons run with their own -g config
# dir under bench/runs/utp-interop (git-ignored), on non-default ports, with DHT/LPD/portmap off.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
W="$ROOT/bench/runs/utp-interop"
SIZE_MIB="${SIZE_MIB:-16}"
PIECE_KIB=256
PIECE=$((PIECE_KIB * 1024))
UDP_A=51890   # Transmission seeder peer port (TCP and UDP/uTP)
UDP_ME=51891  # our uTP seeder
UDP_B=51892   # Transmission leecher peer port
RPC_A=9291
RPC_B=9292
TRACKER=9293
TR_A="transmission-remote 127.0.0.1:$RPC_A"
TR_B="transmission-remote 127.0.0.1:$RPC_B"

cleanup() {
  [ -n "${PID_A:-}" ] && kill "$PID_A" 2>/dev/null || true
  [ -n "${PID_B:-}" ] && kill "$PID_B" 2>/dev/null || true
  [ -n "${PID_S:-}" ] && kill "$PID_S" 2>/dev/null || true
}
trap cleanup EXIT

rm -rf "$W"
mkdir -p "$W/a-conf" "$W/b-conf" "$W/data" "$W/dl"

echo "== build example"
(cd "$ROOT" && CARGO_BUILD_JOBS=4 cargo build --release -p trs-utp --example interop 2>&1 | tail -1)
BIN="$ROOT/target/release/examples/interop"

echo "== make ${SIZE_MIB} MiB random payload and torrent (${PIECE_KIB} KiB pieces)"
head -c $((SIZE_MIB * 1024 * 1024)) /dev/urandom > "$W/data/payload.bin"
transmission-create -o "$W/test.torrent" -s "$PIECE_KIB" -t "http://127.0.0.1:$TRACKER/announce" \
  -c "trs-utp interop" --anonymize "$W/data/payload.bin" >/dev/null
IH="$(transmission-show "$W/test.torrent" | awk '/Hash( v1)?:/ {print $NF; exit}')"
echo "info hash: $IH"

settings() { # conf-dir peer-port rpc-port download-dir
  cat > "$1/settings.json" <<JSON
{
  "dht-enabled": false, "lpd-enabled": false, "pex-enabled": false, "utp-enabled": true,
  "port-forwarding-enabled": false, "peer-port": $2, "peer-port-random-on-start": false,
  "rpc-port": $3, "rpc-authentication-required": false, "rpc-whitelist-enabled": false,
  "download-dir": "$4", "incomplete-dir-enabled": false, "watch-dir-enabled": false,
  "encryption": 0, "preferred-transports": ["utp", "tcp"],
  "speed-limit-up-enabled": false, "speed-limit-down-enabled": false,
  "peer-limit-global": 50, "message-level": 2, "script-torrent-done-enabled": false
}
JSON
}

wait_rpc() { # remote-cmd
  for _ in $(seq 1 50); do $1 -l >/dev/null 2>&1 && return 0; sleep 0.2; done
  echo "daemon did not come up"; return 1
}

echo
echo "== A) Transmission 4.x seeds; we download over uTP"
settings "$W/a-conf" "$UDP_A" "$RPC_A" "$W/data"
transmission-daemon -f -g "$W/a-conf" --log-level=debug -e "$W/a.log" >/dev/null 2>&1 &
PID_A=$!
wait_rpc "$TR_A"
$TR_A -a "$W/test.torrent" -w "$W/data" >/dev/null
for _ in $(seq 1 100); do
  $TR_A -l | grep -q "100%" && break
  sleep 0.2
done
$TR_A -l | sed -n '1,3p'
# Transmission re-evaluates chokes every ~10 s, so the first block arrives late; the example holds
# the connection open afterwards so we can read Transmission's view of it.
"$BIN" download "127.0.0.1:$UDP_A" "$IH" "$W/data/payload.bin" "$PIECE" 8 > "$W/a-download.out" 2>&1 &
PID_D=$!
for _ in $(seq 1 60); do grep -q "DOWNLOAD OK" "$W/a-download.out" 2>/dev/null && break; sleep 0.5; done
echo "-- Transmission's view of our connection (flags: T = uTP, I = incoming, U = it uploads to us):"
$TR_A -t 1 -pi | sed -n '1,3p'
wait "$PID_D"
cat "$W/a-download.out"
grep -ci "utp" "$W/a.log" | sed 's/^/Transmission log lines mentioning uTP: /'
kill "$PID_A"; wait "$PID_A" 2>/dev/null || true; PID_A=

echo
echo "== B) we seed over uTP only (no TCP listener); Transmission leeches via the tracker"
# Transmission drops loopback peers handed out by trackers, so advertise the LAN address.
LAN="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true)"
if [ -z "$LAN" ]; then echo "B: skipped (no LAN address found)"; exit 0; fi
echo "advertising $LAN:$UDP_ME"
"$BIN" serve "0.0.0.0:$UDP_ME" "$IH" "$W/data/payload.bin" "$PIECE" "$TRACKER" "$LAN" > "$W/b-serve.out" 2>&1 &
PID_S=$!
sleep 0.5
settings "$W/b-conf" "$UDP_B" "$RPC_B" "$W/dl"
transmission-daemon -f -g "$W/b-conf" --log-level=debug -e "$W/b.log" >/dev/null 2>&1 &
PID_B=$!
wait_rpc "$TR_B"
$TR_B -a "$W/test.torrent" -w "$W/dl" >/dev/null
OK=0
for i in $(seq 1 200); do
  if $TR_B -l | grep -q "100%"; then OK=1; break; fi
  if [ "$i" = 20 ]; then echo "-- Transmission leecher's view (flags: T = uTP, D = downloading from it):"; $TR_B -t 1 -pi | sed -n '1,3p'; fi
  sleep 0.4
done
$TR_B -l | sed -n '1,3p'
if [ "$OK" = 1 ] && cmp -s "$W/dl/payload.bin" "$W/data/payload.bin"; then
  echo "B: Transmission downloaded ${SIZE_MIB} MiB from us over uTP, byte-exact"
else
  echo "B: FAILED (see $W/b.log and $W/b-serve.out)"; cat "$W/b-serve.out"; exit 1
fi
cat "$W/b-serve.out" | sort | uniq -c | sort -rn | head
echo
echo "INTEROP OK"
