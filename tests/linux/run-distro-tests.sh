#!/usr/bin/env bash
# Build per-distro toolchain images and run `cargo test` for the platform-independent crates in each.
# Usage: run-distro-tests.sh [--src DIR] [--out DIR] [distro ...]     distros: debian ubuntu fedora arch alpine
# --src: a source tree (default: ../.. relative to this script). It is streamed into a docker volume
#        (target/, fuzz/ excluded), so nothing in the working tree is touched.
# Needs docker (colima start --cpu 4 --memory 6). Results: <out>/<distro>.txt and <out>/<distro>/<crate>.log
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$(cd "$HERE/../.." && pwd)"; OUT="$HERE/results"
while [[ "${1:-}" == --* ]]; do case "$1" in --src) SRC="$2"; shift 2;; --out) OUT="$2"; shift 2;; *) echo "unknown $1"; exit 2;; esac; done
DISTROS=("$@"); [[ ${#DISTROS[@]} -eq 0 ]] && DISTROS=(debian ubuntu fedora arch alpine)
mkdir -p "$OUT"
docker volume create trs-registry >/dev/null   # crate sources, shared by all distros (downloaded once)
for d in "${DISTROS[@]}"; do
  img="trs-linux-$d"; vol_src="trs-src-$d"; vol_cache="trs-cache-$d"; vol_out="trs-out-$d"
  echo "##### $d: image"
  docker build --platform "${PLATFORM:-linux/arm64}" -q -t "$img" -f "$HERE/docker/Dockerfile.$d" "$HERE/docker" >/dev/null || { echo "image build FAILED for $d" | tee "$OUT/$d.txt"; continue; }
  docker volume rm -f "$vol_src" >/dev/null; docker volume create "$vol_src" >/dev/null; docker volume create "$vol_cache" >/dev/null; docker volume rm -f "$vol_out" >/dev/null; docker volume create "$vol_out" >/dev/null
  rm -rf "$OUT/$d"; mkdir -p "$OUT/$d"
  ( cd "$SRC" && tar -c --exclude=target --exclude=fuzz --exclude=.git crates tools Cargo.toml Cargo.lock ) \
    | docker run --rm -i -v "$vol_src:/src" "$img" sh -c 'tar -x -C /src && find /src -type f -exec touch {} +' || continue
  # (touch: tar keeps the sources' old mtimes, and cargo would treat a cached build from an OLDER snapshot as fresh)
  docker run --rm -i -v "$vol_src:/src" "$img" sh -c 'cat > /src/.test-crates.sh' < "$HERE/in-container/test-crates.sh"
  echo "##### $d: tests"
  docker run --rm --cpus "${TEST_CPUS:-4}" -v "$vol_src:/src" -v "$vol_cache:/cache" -v "$vol_out:/out" -v trs-registry:/registry \
     -e CRATES -e TEST_ARGS -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
     "$img" sh /src/.test-crates.sh 2>&1 | tee "$OUT/$d.txt"
  # logs out of the VM (named volume, because bind mounts of arbitrary host paths are not shared with colima)
  docker run --rm -v "$vol_out:/out:ro" "$img" tar -c -C /out . | tar -x -C "$OUT/$d"
  # a debug build tree of the workspace is 10-25 GB: free it before the next distro (the colima disk is 100 GB)
  docker volume rm -f "$vol_cache" "$vol_src" "$vol_out" >/dev/null
done
