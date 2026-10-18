#!/bin/bash
# Seedwell (Linux daemon) against one or more tracker-probe mock trackers.
#   - regular announce timing (A2), dual-stack failure modes (A3), and the many-torrent storm.
# Env:
#   MOCKS     ';'-separated  name,ip4,port,interval,min_interval,v6mode,count[,delay_ms[,mode]]  (mode: leech (default, synthetic
#             torrents without data) or seed (real 20 KB torrents with data on disk))
#             v6mode: none|refuse|blackhole|close|ok|syndrop (v6 SYNs dropped by ip6tables = connect timeout). With v6mode != none the announce host is a name
#             resolving to BOTH ip4 and an RFC 3849 documentation IPv6 address (2001:db8::<last octet>)
#             on lo, i.e. a dual-stack host whose IPv6 side misbehaves (Seedwell refuses loopback trackers).
#   SECONDS_TOTAL (900)  how long to observe after adding      SHUTDOWN_SECS (120) linger on quit
#   LISTEN (51600)  OUT (/out)  UP (unset)  ADD_BATCH (50)
source "$(dirname "$0")/lib.sh"
SECONDS_TOTAL=${SECONDS_TOTAL:-900}; OUT=${OUT:-/out}; LISTEN=${LISTEN:-51600}; export SHUTDOWN_SECS=${SHUTDOWN_SECS:-120}
W=/tmp/mr; mkdir -p $W $OUT
IFS=';' read -ra M <<< "$MOCKS"
declare -A MIP MPORT MMODE
pids=()
for spec in "${M[@]}"; do
  IFS=, read -r name ip port itv minitv v6 count delay mode <<< "$spec"
  addr_alias $ip
  host=$ip
  args=(--listen $ip:$port --interval $itv --min-interval $minitv --label "$name" --out $OUT/mock-$name.jsonl)
  if [ "$v6" != none ]; then
    last=${ip##*.}; ip6=2001:db8::$last
    ip link set lo up; sysctl -q -w net.ipv6.conf.all.disable_ipv6=0 net.ipv6.conf.lo.disable_ipv6=0 2>/dev/null
    addr_alias $ip6
    host=trk-$name.test
    printf '%s %s\n%s %s\n' $ip $host $ip6 $host >> /etc/hosts
    if [ "$v6" = syndrop ]; then  # nothing listens on v6 and SYNs are silently dropped: a firewall black hole (connect timeout)
      v6=refuse; ip6tables -A INPUT -p tcp -d $ip6 --dport $port -j DROP
    fi
    args+=(--v6 $v6 --listen6 [$ip6]:$port)
  fi
  [ -n "${delay:-}" ] && args+=(--delay-ms $delay)
  tracker-probe mock "${args[@]}" 2>$OUT/mock-$name.stderr & pids+=($!)
  mkdir -p $W/t-$name
  if [ "${mode:-leech}" = seed ]; then
    # real (tiny) torrents with data on disk: they verify complete and seed, so they are not limited by the
    # download queue (max_active_downloads <= 100) and a user with hundreds of seeds is what we model
    mkdir -p $W/data-$name $W/t-$name
    for i in $(seq -w 1 $count); do
      head -c 20000 /dev/urandom > $W/data-$name/m-$name-$i.bin
      transmission-create -o $W/t-$name/m-$name-$i.torrent -t http://$host:$port/announce $W/data-$name/m-$name-$i.bin >/dev/null
    done
  else
    tracker-probe mktest --announce http://$host:$port/announce --count $count --out-dir $W/t-$name --prefix "m-$name" >/dev/null
  fi
  MMODE[$name]=${mode:-leech}
  MIP[$name]=$ip; MPORT[$name]=$port
done
sleep 1
marker() { for n in "${!MIP[@]}"; do curl -fs -o /dev/null "http://${MIP[$n]}:${MPORT[$n]}/_marker/$1"; done; log "MARKER $1"; }
ST=$W/state; pre_config $ST
marker run-start
start_daemon seedwell $ST $W/dl $LISTEN --max-active "${MAX_ACTIVE:-100}" --permission full || exit 1; D=$DPID; S=$SOCK
[ -n "${UP:-}" ] && sw $S limits --up $UP >/dev/null
marker add
for n in "${!MIP[@]}"; do
  files=($W/t-$n/*.torrent)
  extra=(); [ "${MMODE[$n]}" = seed ] && extra=(--save-path $W/data-$n)
  # the control socket allows 20 calls/s per client (agents.rate_limit_per_sec) and counts every torrent of a
  # multi-add, so add one by one at <= 10/s and retry only what was refused with "too many requests"
  for f in "${files[@]}"; do
    while out=$(sw $S add "$f" "${extra[@]}" 2>&1); [[ "$out" == *"too many requests"* ]]; do sleep 0.3; done
    sleep 0.1
  done
done
marker torrents-added
log "torrents in Seedwell: $(sw $S list --json | jq .total)"
log "observing ${SECONDS_TOTAL}s"
end=$((SECONDS+SECONDS_TOTAL))
while ((SECONDS<end)); do
  sleep 30
  for n in "${!MIP[@]}"; do printf '%s=%s ' $n "$(grep -c '"kind":"announce"' $OUT/mock-$n.jsonl)"; done; echo >&2
done
sw $S list --json --limit 5000 > $OUT/list-before-quit.json 2>/dev/null || sw $S list --json > $OUT/list-before-quit.json 2>/dev/null
marker quit
T0=$(date +%s)
stop_daemon $D $((SHUTDOWN_SECS+30)); log "daemon exited after $(( $(date +%s)-T0 )) s"
sleep 2
marker run-end
cp $ST/daemon.err $OUT/daemon.err; cp $ST/daemon.out $OUT/daemon.out
for p in "${pids[@]}"; do kill -INT $p 2>/dev/null; done; sleep 2
for spec in "${M[@]}"; do
  IFS=, read -r name _ <<< "$spec"
  { echo "## check $name"; tracker-probe check --expect-announces 1 $OUT/mock-$name.jsonl; echo "exit=$?"; } >> $OUT/check.txt 2>&1
  tracker-probe analyze $OUT/mock-$name.jsonl > $OUT/analyze-$name.txt 2>&1
done
cat $OUT/check.txt
