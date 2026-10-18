#!/usr/bin/env python3 -I
"""Summarise a tracker-probe mock capture of a Seedwell run (announce timing, rate, concurrency, events).

  summarize-mock.py mock-NAME.jsonl [--interval N] [--min-interval M]

Prints per capture: announces by event and by server address (v4/v6), regular-announce gaps (min/median/max, only
gaps that follow a started or another regular announce), the maximum number of requests in any W-second window against the
burst-3 + 1/s budget of SPEC 7C.3 invariant 5, the maximum in-flight count the mock saw, and when `stopped` went out
relative to the `quit` marker.
"""
import bisect
import collections
import json
import sys


def main():
    path = sys.argv[1]
    recs = [json.loads(line) for line in open(path)]
    mk = {r["marker"]: r["ts_ms"] / 1000 for r in recs if r["kind"] == "marker"}
    ann = [r for r in recs if r["kind"] == "announce"]
    scr = [r for r in recs if r["kind"] == "scrape"]

    def ev(r):
        return ([p["v"] for p in r["params"] if p["k"] == "event"] or ["-"])[0]

    print(f"{path}: {len(ann)} announces, {len(scr)} scrapes")
    print("  events:", dict(collections.Counter(ev(r) for r in ann)))
    print("  server address:", dict(collections.Counter(r.get("local") for r in ann)))
    by = collections.defaultdict(list)
    for r in ann:
        by[r["info_hash"]].append((r["ts_ms"] / 1000, ev(r)))
    gaps = []
    for v in by.values():
        v.sort()
        for (a, ea), (b, eb) in zip(v, v[1:]):
            if eb == "-" and ea in ("-", "started"):
                gaps.append(b - a)
    gaps.sort()
    if gaps:
        print(f"  regular gaps: n={len(gaps)} min={gaps[0]:.1f}s median={gaps[len(gaps) // 2]:.1f}s max={gaps[-1]:.1f}s")
    ts = sorted(r["ts_ms"] / 1000 for r in ann)
    for w in (1, 2, 5, 30, 300):
        mx = max((bisect.bisect_left(ts, t + w) - i for i, t in enumerate(ts)), default=0)
        print(f"  max requests in any {w:3d} s window: {mx} (budget {3 + w})")
    inflight = [r.get("inflight", 0) for r in ann]
    if inflight:
        print("  max in flight when a request arrived:", max(inflight))
    q = mk.get("quit")
    st = [r["ts_ms"] / 1000 for r in ann if ev(r) == "stopped"]
    if q and st:
        print(f"  stopped: {len(st)} for {len({r['info_hash'] for r in ann if ev(r) == 'stopped'})} torrents; first {min(st) - q:+.1f}s, last {max(st) - q:+.1f}s after the quit marker")
    started = {r["info_hash"] for r in ann if ev(r) == "started"}
    print(f"  torrents with started: {len(started)}; with at least one announce: {len(by)}")


if __name__ == "__main__":
    main()
