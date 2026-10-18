#!/usr/bin/env python3
"""Repro for the TorrentRS hot-variant failure (needs TRS_BENCH_BIN; do not run during another benchmark).

A TorrentRS seeder serves one Transmission leecher process (round "a"), the leecher exits, and a *new* leecher
process from the same IP connects (round "b").  Observed 2026-10-11: round a completes (first block after ~30 s),
round b never connects (seeder peers = 0) within 100 s.

  python3 -I bench/scripts/tests/repro-seeder-reconnect.py
"""
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
from benchlib import util  # noqa: E402
from benchlib.adapters import ADAPTERS, LaunchCfg  # noqa: E402
from benchlib.torrents import MockTracker, ensure_scale_set  # noqa: E402

util.install_cleanup()
iface = util.default_iface()
ip = util.lan_ip(iface)
tr = MockTracker(7171, util.RUNS / "trackerlogs" / "repro.jsonl", peers=[f"{ip}:51413"])
mset, ddir = ensure_scale_set(500, tr.announce_url)
mset = mset[:12]
tr.start()
s = ADAPTERS["torrentrs"]("s")
s.launch(LaunchCfg(util.PROFILES / "repro-s", 51413, 0, "local", iface=iface), [(m["torrent"], ddir) for m in mset])
while not s.poll()["complete"]:
    time.sleep(1)


def rnd(tag, port=51501, timeout=100):
    l = ADAPTERS["transmission"]("l")
    dl = util.DOWNLOADS / "repro-l"
    util.rmtree(dl)
    l.launch(LaunchCfg(util.PROFILES / f"repro-l{tag}", port, 19101, "local", iface=iface), [(m["torrent"], dl) for m in mset[:3]])
    t0 = time.time()
    st = l.poll()
    while time.time() - t0 < timeout and not st["complete"]:
        time.sleep(1)
        st = l.poll()
    print(tag, "complete" if st["complete"] else "STUCK", f"{st['done_bytes']}/{st['total_bytes']}", f"t={time.time() - t0:.0f}s",
          "seeder peers", s.poll()["peers"], flush=True)
    l.stop()
    util.rmtree(dl)
    util.rmtree(util.PROFILES / f"repro-l{tag}")


try:
    rnd("a")
    rnd("b")
finally:
    s.stop()
    tr.stop()
    util.rmtree(util.PROFILES / "repro-s")
