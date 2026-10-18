#!/bin/bash
# A1: Seedwell lifecycle against Debian's real tracker through tracker-probe's logging proxy.
#   add -> pause at ~20% -> 30 s -> resume -> complete -> seed 5 min -> graceful quit -> relaunch -> remove
# Runs inside the lab container (needs NET_ADMIN for the address alias, NET_RAW for tcpdump).
# Env: SEED_SECS (300) PAUSE_PCT (20) UP_LIMIT (200KB) OUT (/out)
source "$(dirname "$0")/lib.sh"
SEED_SECS=${SEED_SECS:-300}; PAUSE_PCT=${PAUSE_PCT:-20}; UP_LIMIT=${UP_LIMIT:-200KB}; OUT=${OUT:-/out}
PROXY_IP=203.0.113.9; PROXY=7081; W=/tmp/a1; mkdir -p $W $OUT
addr_alias $PROXY_IP
CAP=$OUT/seedwell-debian.jsonl
tcpdump -i lo -n -s0 -U -w $OUT/seedwell-debian.pcap "tcp port $PROXY" 2>/dev/null & TCPD=$!
tracker-probe rewrite /in/debian-netinst.torrent --proxy http://$PROXY_IP:$PROXY --id debian -o $W/debian.torrent
tracker-probe proxy --listen $PROXY_IP:$PROXY --label "seedwell-$(seedwell --version | awk '{print $2}')" --out $CAP \
   --map debian=http://bttracker.debian.org:6969/announce 2>>$OUT/proxy.stderr & PXY=$!
for _ in $(seq 1 20); do curl -fs -o /dev/null http://$PROXY_IP:$PROXY/_marker/proxy-ready && break; sleep 1; done
marker() { curl -fs -o /dev/null http://$PROXY_IP:$PROXY/_marker/$1; log "MARKER $1"; }

ST=$W/state; DL=$W/dl
pre_config $ST
marker run-start
start_daemon seedwell $ST $DL 51423 || exit 1; D=$DPID; S=$SOCK
marker client-launched
sw $S limits --up $UP_LIMIT >/dev/null
sw $S settings get network.utp 2>&1 | head -1 >&2 || true
marker add
sw $S add $W/debian.torrent
ID=$(first_id $S); log "id=$ID"
log "waiting for ${PAUSE_PCT}%"
until awk "BEGIN{exit !($(t_field $S $ID progress)*100 >= $PAUSE_PCT)}"; do sleep 3; done
marker pause;  sw $S pause $ID >/dev/null
log "paused 30 s"; sleep 30
marker resume; sw $S resume $ID >/dev/null
log "waiting for completion"
until [ "$(t_field $S $ID progress)" = "1" ] || [ "$(t_field $S $ID progress)" = "1.0" ]; do sleep 5; done
marker complete-detected
log "seeding $SEED_SECS s"; sleep $SEED_SECS
marker seed-end
marker quit; stop_daemon $D 60; marker quit-done
sleep 5
marker relaunch; start_daemon seedwell $ST $DL 51423 || exit 1; D=$DPID; S=$SOCK
# wait for its announce after relaunch
for _ in $(seq 1 120); do
  n=$(awk '/"marker":"relaunch"/{f=1;next} f&&/"kind":"announce"/{n++} END{print n+0}' $CAP); [ "$n" -gt 0 ] && break; sleep 1; done
sleep 10
marker remove; sw $S remove $ID -y >/dev/null
sleep 15
marker quit-final; stop_daemon $D 60
marker run-end
kill -INT $PXY; sleep 1; kill $TCPD 2>/dev/null
cp $ST/daemon.err $OUT/seedwell-debian.daemon.err 2>/dev/null; cp $ST/daemon.out $OUT/seedwell-debian.daemon.out 2>/dev/null
tracker-probe analyze $CAP > $OUT/seedwell-debian.txt || true
tracker-probe check $CAP > $OUT/seedwell-debian.check.txt; echo "check exit=$?" >> $OUT/seedwell-debian.check.txt
cat $OUT/seedwell-debian.check.txt
