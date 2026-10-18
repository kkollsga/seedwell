#!/usr/bin/env python3 -I
"""Render the per-distro results of run-distro-tests.sh as markdown tables.

  summarize-distros.py RESULTS_DIR [distro ...]

Reads <RESULTS_DIR>/<distro>.txt (the RESULT lines) and, for the failing tests, <RESULTS_DIR>/<distro>/<crate>.log.
"""
import os
import re
import sys

CRATES = "trs-bencode trs-metainfo trs-wire trs-tracker trs-storage trs-dht trs-utp trs-http trs-state trs-classify trs-engine trs-control trs-cli trs-portmap".split()
d = sys.argv[1]
distros = sys.argv[2:] or ["debian", "ubuntu", "fedora", "arch", "alpine"]
res = {}
env = {}
for dist in distros:
    p = os.path.join(d, dist + ".txt")
    if not os.path.exists(p):
        continue
    for line in open(p, errors="replace"):
        m = re.match(r"RESULT (\S+) rc=(\d+) passed=(\d+) failed=(\d+) ignored=(\d+) build=(\S+) secs=(\d+)", line)
        if m:
            c, rc, ps, fl, ig, b, s = m.groups()
            res[(dist, c)] = (int(rc), int(ps), int(fl), int(ig), b, int(s))
        if line.startswith("== env:"):
            env[dist] = line[len("== env: "):].strip()
print("| Crate | " + " | ".join(distros) + " |")
print("|---|" + "---|" * len(distros))
tot = {x: [0, 0, 0] for x in distros}
for c in CRATES:
    row = []
    for dist in distros:
        r = res.get((dist, c))
        if not r:
            row.append("not run")
            continue
        rc, ps, fl, ig, b, s = r
        tot[dist][0] += ps
        tot[dist][1] += fl
        tot[dist][2] += ig
        if rc == 0:
            row.append(f"pass {ps}")
        elif b == "ERRORS" and ps == 0 and fl == 0:
            row.append("**BUILD FAILED**")
        else:
            row.append(f"**{fl} FAIL**, {ps} pass")
    print(f"| {c} | " + " | ".join(row) + " |")
print("| **total** | " + " | ".join(f"{tot[x][0]} pass, {tot[x][1]} fail, {tot[x][2]} ignored" for x in distros) + " |")
print()
for dist in distros:
    if dist in env:
        print(f"* {dist}: {env[dist]}")
print()
for dist in distros:
    for c in CRATES:
        r = res.get((dist, c))
        if not r or r[0] == 0:
            continue
        p = os.path.join(d, dist, c + ".log")
        names = []
        if os.path.exists(p):
            txt = open(p, errors="replace").read()
            names = sorted(set(re.findall(r"^test (\S+) \.\.\. FAILED", txt, re.M)))
            panics = re.findall(r"thread '([^']+)' \(\d+\) panicked at ([^\n]+):\n([^\n]*)", txt)
            errs = sorted(set(re.findall(r"^error(?:\[E\d+\])?: [^\n]+", txt, re.M)))[:4]
        print(f"* {dist} / {c}: " + (", ".join(f"`{n}`" for n in names) if names else "build error"))
        if os.path.exists(p):
            for t, loc, msg in panics[:12]:
                print(f"    * `{t}` at `{loc}`: {msg.strip()[:140]}")
            if not names:
                for e in errs:
                    print(f"    * {e[:160]}")
