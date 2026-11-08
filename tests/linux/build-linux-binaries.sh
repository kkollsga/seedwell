#!/usr/bin/env bash
# Build `seedwell` (CLI + daemon) and `tracker-probe` for Linux inside the Debian 13 toolchain image
# (aarch64 when run on Apple Silicon via colima). Output: <out>/seedwell, <out>/tracker-probe
# Usage: build-linux-binaries.sh [--src DIR] [--out DIR]
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$(cd "$HERE/../.." && pwd)"; OUT="$HERE/bin"
while [[ "${1:-}" == --* ]]; do case "$1" in --src) SRC="$2"; shift 2;; --out) OUT="$2"; shift 2;; *) echo "unknown $1"; exit 2;; esac; done
mkdir -p "$OUT"
docker build -q -t seedwell-linux-debian -f "$HERE/docker/Dockerfile.debian" "$HERE/docker" >/dev/null
CACHE="${CACHE_VOL:-seedwell-cache-build}"
docker volume rm -f seedwell-src-build >/dev/null
docker volume create seedwell-src-build >/dev/null; docker volume create "$CACHE" >/dev/null
( cd "$SRC" && tar -c --exclude=target --exclude=fuzz --exclude=.git crates tools Cargo.toml Cargo.lock ) \
  | docker run --rm -i -v seedwell-src-build:/src seedwell-linux-debian sh -c 'tar -x -C /src && find /src -type f -exec touch {} +'
docker rm -f seedwell-build >/dev/null 2>&1 || true
docker run --name seedwell-build -v seedwell-src-build:/src -v "$CACHE":/cache \
  -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" seedwell-linux-debian sh -c '
  mkdir -p /out /cache/registry && rm -rf /opt/cargo/registry && ln -sfn /cache/registry /opt/cargo/registry && cd /src && CARGO_TARGET_DIR=/cache/target cargo build --release -p seedwell-cli -p tracker-probe 2>&1 | tail -5 &&
  cp /cache/target/release/seedwell /cache/target/release/tracker-probe /out/ && /out/seedwell --version'
docker cp seedwell-build:/out/. "$OUT"/ && docker rm seedwell-build >/dev/null
