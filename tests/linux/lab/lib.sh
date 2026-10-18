# Helpers for the in-container lab scripts (sourced). Everything runs with HOME in a temp dir and
# TRS_FORBID_REAL_HOME=1, so nothing outside the lab directory can be touched.
set -u
export HOME=${HOME_LAB:-/tmp/home-lab} TRS_FORBID_REAL_HOME=1
mkdir -p "$HOME"
log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&2; }

# addr_alias IP : a routable-looking (RFC 5737 documentation) address on lo. Seedwell's production
# destination policy refuses loopback/private trackers; documentation ranges are allowed.
addr_alias() { for a in "$@"; do case "$a" in *:*) ip -6 addr add "$a/128" dev lo nodad 2>/dev/null;; *) ip addr add "$a/32" dev lo 2>/dev/null;; esac; done; }

# sw SOCKET args... : the seedwell CLI against a daemon socket
sw() { local s="$1"; shift; seedwell --socket "$s" "$@"; }

# start_daemon NAME STATE_DIR DL_DIR PORT [extra daemon flags]  -> sets DPID, SOCK
start_daemon() {
  local name="$1" st="$2" dl="$3" port="$4"; shift 4
  mkdir -p "$st" "$dl"
  seedwell daemon --state-dir "$st" --download-dir "$dl" --listen-port "$port" --no-dht --shutdown-secs "${SHUTDOWN_SECS:-30}" "$@" \
     >>"$st/daemon.out" 2>>"$st/daemon.err" &
  DPID=$!; SOCK="$st/control.sock"
  for _ in $(seq 1 50); do grep -q 'daemon ready' "$st/daemon.out" 2>/dev/null && return 0; kill -0 $DPID 2>/dev/null || { log "daemon $name died"; cat "$st/daemon.err" >&2; return 1; }; sleep 0.2; done
  log "daemon $name did not become ready"; return 1
}
# pre_config STATE_DIR : only the knobs the daemon flags do not cover (reference runs: PEX/LSD/uTP off)
pre_config() { mkdir -p "$1"; cat > "$1/config.toml" <<TOML
[network]
pex = false
lsd = false
utp = ${UTP:-false}
ipv6 = ${IPV6:-false}
TOML
}
# jq helpers on the control JSON
t_field() { sw "$1" list --json | jq -r --arg id "$2" ".transfers[] | select(.id|startswith(\$id)) | .$3"; }
first_id() { sw "$1" list --json | jq -r '.transfers[0].id'; }
stop_daemon() { kill -TERM "$1" 2>/dev/null; for _ in $(seq 1 $((${2:-60}*2))); do kill -0 "$1" 2>/dev/null || return 0; sleep 0.5; done; log "daemon $1 still alive after ${2:-60}s"; return 1; }
