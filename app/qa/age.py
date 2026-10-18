#!/usr/bin/env python3
"""QA only: makes most torrents look dormant by rewriting `last_activity` in the app's
`torrent-extra.bin` (36-byte records: id[20], queue_pos u32, last_activity u32, error_since u32,
flags u8 + 3 pad). Run while the app is NOT running. Every `keep`-th torrent stays recent.

    age.py <state-dir> [--keep 5] [--days 3]"""
import struct, sys, time
from pathlib import Path

args = sys.argv[1:]
state = Path(args[0])
keep = int(args[args.index("--keep") + 1]) if "--keep" in args else 5
days = int(args[args.index("--days") + 1]) if "--days" in args else 3
if not str(state).startswith(("/private/tmp", "/tmp", "/var/folders")):
    sys.exit("refusing: state dir must be a temp dir")
p = state / "torrent-extra.bin"
b = bytearray(p.read_bytes())
n = len(b) // 36
old = int(time.time()) - days * 86400
for i in range(n):
    if i % keep == 0:
        continue
    off = i * 36 + 24
    struct.pack_into("<I", b, off, old)
p.write_bytes(bytes(b))
print(f"aged {n - (n + keep - 1) // keep} of {n} torrents")
