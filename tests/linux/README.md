# tests/linux: Seedwell on Linux distros and against real tracker software

Everything here runs in containers (colima/docker on the Mac) and never touches the host's
Seedwell, Transmission or Downloads. Daemons run with `HOME` in a temp dir and `TRS_FORBID_REAL_HOME=1`.

```
colima start --cpu 4 --memory 6
tests/linux/run-distro-tests.sh [--src DIR] [--out DIR] [debian ubuntu fedora arch alpine]   # cargo test, 14 crates, per distro
tests/linux/build-linux-binaries.sh [--src DIR] --out DIR                                    # seedwell + tracker-probe for Linux
```

## 1. Distro test matrix

`docker/Dockerfile.<distro>` is a toolchain image only (compiler, cmake, perl, rustup stable). The source tree is
streamed into a docker volume at run time (without `target/` and `fuzz/`), so a run never touches the working tree.
`in-container/test-crates.sh` runs `cargo test --no-fail-fast -p <crate>` for bencode, metainfo, wire, tracker,
storage, dht, utp, http, state, classify, engine, control, cli and portmap, and prints one `RESULT` line per crate
(passed / failed / ignored / build errors / seconds) plus the failing test names. Logs: `<out>/<distro>/<crate>.log`.

| Distro | Image | libc |
|---|---|---|
| Debian 13 | `debian:13` | glibc |
| Ubuntu 26.04 | `ubuntu:26.04` | glibc |
| Fedora (latest) | `fedora:latest` | glibc |
| Arch | `lopsided/archlinux` (the official `archlinux` image is x86_64 only; this is the multi-arch image, Arch Linux ARM on arm64) | glibc |
| Alpine | `alpine:latest` | musl |

`PLATFORM=linux/amd64` (and `--build-arg BASE=archlinux` for Arch) runs the same matrix under emulation.

## 2. Lab: mock trackers, real trackers, real clients

* `build-linux-binaries.sh` builds `seedwell` (CLI + `daemon`) and `tracker-probe` in the Debian 13 image.
* `lab/Dockerfile` (image `trs-lab`): Debian 13 + `transmission-daemon` 4.1.0-beta.2, `qbittorrent-nox` 5.1.0,
  `opentracker` (all distro packages) + the two binaries. `lab/Dockerfile.trackers` (image `trs-trackers`): chihaya
  (built from source, `main`), `aquatic_http`, `aquatic_udp`, `aquatic_ws`.
* `lab/run-in-lab.sh NAME SCRIPT [VAR=val ...]` runs a script from `lab/` in a fresh container and copies `/out` back.
  * `lab/a1-lifecycle.sh`: Seedwell against Debian's real tracker through `tracker-probe proxy` (add, pause at 20 %,
    30 s, resume, complete, seed 5 min, graceful quit, relaunch, remove).
  * `lab/mock-run.sh`: Seedwell against `tracker-probe mock` instances (regular announce timing, dual-stack failure
    modes, the 500-torrent storm). `MOCKS='name,ip,port,interval,min,v6mode,count[,delay_ms[,seed]]'`.
* `lab/swarm.py up|matrix|transports|handshake|collect|down`: opentracker, chihaya, aquatic and three clients on a
  docker network (`trslab`, 198.51.100.0/24): Seedwell seeds to Transmission and qBittorrent, and the reverse, over
  HTTP and UDP trackers, with sha256 verification; TCP/uTP interop with iptables byte counters.
* `lab/pcap2jsonl.py`: tracker-side pcap to `tracker-probe` records, so `tracker-probe check` judges what a real
  tracker received. `lab/peerwire.py`: decodes BitTorrent and BEP 10 handshakes from a pcap.

### Why RFC 5737 addresses

Seedwell's production destination policy (`trs-tracker` `DestinationPolicy::STRICT`, SSRF protection) refuses
loopback, link-local and private-network tracker addresses. `seedwell daemon` has no switch to relax that, and the
test must exercise the unmodified policy, so every mock and tracker listens on a documentation address
(203.0.113.0/24, 198.51.100.0/24, 2001:db8::/32) added to `lo` or the lab network, which the policy allows.
On macOS that is impossible without root (no loopback alias), so the macOS binary cannot talk to a local mock;
the lab runs the identical Rust code on Linux.

## Status

The distro Dockerfiles install the latest stable Rust with rustup, so results shift with the toolchain (this run: rustc 1.99.0).
