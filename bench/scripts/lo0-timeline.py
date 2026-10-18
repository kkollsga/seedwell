#!/usr/bin/env python3
"""Sample the loopback byte counter every 100 ms: `netstat -I lo0 -b`, column Obytes.

    python3 -I bench/scripts/lo0-timeline.py OUT.tsv [SECONDS]

Writes `wall_ms<TAB>Obytes` lines (wall clock, so it lines up with TRS_UTP_TRACE output and the
harness log). Stops after SECONDS (default: until SIGTERM)."""
import subprocess, sys, time

out = open(sys.argv[1], "w")
end = time.time() + float(sys.argv[2]) if len(sys.argv) > 2 else None
nxt = time.time()
while end is None or time.time() < end:
    nxt += 0.1
    r = subprocess.run(["netstat", "-I", "lo0", "-b"], capture_output=True, text=True).stdout.splitlines()
    if len(r) > 1:
        cols = r[1].split()
        # Obytes is the last column on the link row
        out.write(f"{int(time.time() * 1000)}\t{cols[-2]}\n")
        out.flush()
    time.sleep(max(0, nxt - time.time()))
