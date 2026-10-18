#!/bin/sh
# Runs inside a distro container: cargo test for the platform-independent crates.
# Env: CRATES (space separated, default below), TEST_ARGS (extra args after --).
set -u
cd /src || exit 99
export HOME=/tmp/home-$$ TRS_FORBID_REAL_HOME=1 CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} CARGO_TARGET_DIR=/cache/target CARGO_NET_RETRY=5
mkdir -p "$HOME" /registry
# keep the crate registry in the cache volume: a re-run must not download the dependency tree again
rm -rf /opt/cargo/registry; ln -sfn /registry /opt/cargo/registry
CRATES=${CRATES:-"trs-bencode trs-metainfo trs-wire trs-tracker trs-storage trs-dht trs-utp trs-http trs-state trs-classify trs-engine trs-control trs-cli trs-portmap"}
echo "== env: $(. /etc/os-release; echo "$PRETTY_NAME") | $(uname -m) | $(rustc -vV | sed -n '1p;5p' | tr '\n' ' ') | libc: $(ldd --version 2>&1 | head -1)"
for c in $CRATES; do
  start=$(date +%s)
  echo "=== $c: build+test"
  cargo test --no-fail-fast -p "$c" ${TEST_ARGS:-} > "/out/$c.log" 2>&1
  rc=$?
  end=$(date +%s)
  passed=$(grep -E '^test result:' "/out/$c.log" | sed -E 's/.* ([0-9]+) passed.*/\1/' | awk '{s+=$1} END{print s+0}')
  failed=$(grep -E '^test result:' "/out/$c.log" | sed -E 's/.* ([0-9]+) failed.*/\1/' | awk '{s+=$1} END{print s+0}')
  ign=$(grep -E '^test result:' "/out/$c.log" | sed -E 's/.* ([0-9]+) ignored.*/\1/' | awk '{s+=$1} END{print s+0}')
  built=ok; grep -q -E '^error(\[E[0-9]+\])?:' "/out/$c.log" && built=ERRORS
  echo "RESULT $c rc=$rc passed=$passed failed=$failed ignored=$ign build=$built secs=$((end-start))"
  grep -E '^test .* FAILED|^error(\[E[0-9]+\])?:|panicked at|^failures:|could not compile' "/out/$c.log" | sort -u | head -20 | sed 's/^/    /'
done
