"""Bencode + deterministic synthetic torrent generator (real random data, real piece hashes) and the
local mock tracker wrapper (`tracker-probe mock`)."""
import hashlib
import json
import os
import random
import signal
import subprocess
import time
import urllib.request
from pathlib import Path

from .util import PROBE, REPO, RUNS, BenchError, log, spawn, wait_exit

SCALE_DIR = RUNS / "scale"                 # gitignored: data/, torrents/, manifest.json
PIECE_LEN = 256 * 1024
MIN_SIZE, MAX_SIZE = 1 * 1024 * 1024, 20 * 1024 * 1024


# ------------------------------------------------------------------ bencode
def bencode(v):
    if isinstance(v, int):
        return b"i%de" % v
    if isinstance(v, bytes):
        return b"%d:%s" % (len(v), v)
    if isinstance(v, str):
        return bencode(v.encode())
    if isinstance(v, (list, tuple)):
        return b"l" + b"".join(bencode(x) for x in v) + b"e"
    if isinstance(v, dict):
        return b"d" + b"".join(bencode(k) + bencode(v[k]) for k in sorted(v, key=lambda k: k if isinstance(k, bytes) else k.encode())) + b"e"
    raise TypeError(type(v))


def bdecode(b, i=0):
    c = b[i:i + 1]
    if c == b"i":
        j = b.index(b"e", i)
        return int(b[i + 1:j]), j + 1
    if c == b"l":
        i += 1
        out = []
        while b[i:i + 1] != b"e":
            x, i = bdecode(b, i)
            out.append(x)
        return out, i + 1
    if c == b"d":
        i += 1
        out = {}
        while b[i:i + 1] != b"e":
            k, i = bdecode(b, i)
            x, i = bdecode(b, i)
            out[k] = x
        return out, i + 1
    j = b.index(b":", i)
    n = int(b[i:j])
    return b[j + 1:j + 1 + n], j + 1 + n


def read_torrent(path):
    d, _ = bdecode(Path(path).read_bytes())
    return d


def info_hash(torrent_dict):
    return hashlib.sha1(bencode(torrent_dict[b"info"] if b"info" in torrent_dict else torrent_dict["info"])).hexdigest()


# ------------------------------------------------------------------ synthetic torrents
def _plan(i, seed):
    """Deterministic layout of torrent i: (name, [(relative path parts, size)])."""
    rng = random.Random(seed * 1_000_003 + i)
    total = int(MIN_SIZE + (rng.random() ** 1.6) * (MAX_SIZE - MIN_SIZE))
    name = f"bench-{i:04d}"
    if i % 3 == 0:                                       # every third torrent is multi-file
        k = rng.randint(2, 5)
        cuts = sorted(rng.randint(1, total - 1) for _ in range(k - 1))
        sizes = [b - a for a, b in zip([0] + cuts, cuts + [total])]
        files = [([f"part{j}.bin"] if j % 2 == 0 else ["sub", f"part{j}.bin"], max(1, s)) for j, s in enumerate(sizes)]
    else:
        name = name + ".bin"                              # single-file torrent: info name == file name
        files = [([name], total)]
    return name, files, rng


def _build(i, seed, data_dir, announce):
    name, files, rng = _plan(i, seed)
    h = hashlib.sha1()
    pieces = bytearray()
    buf = b""
    paths = []
    multi = len(files) > 1 or i % 3 == 0
    root = Path(data_dir) / name if multi else Path(data_dir)
    if multi:
        root.mkdir(parents=True, exist_ok=True)
    for parts, size in files:
        p = root.joinpath(*parts)
        p.parent.mkdir(parents=True, exist_ok=True)
        data = rng.randbytes(size)
        if not (p.exists() and p.stat().st_size == size and p.read_bytes() == data):
            p.write_bytes(data)
        buf += data
        while len(buf) >= PIECE_LEN:
            pieces += hashlib.sha1(buf[:PIECE_LEN]).digest()
            buf = buf[PIECE_LEN:]
        paths.append((parts, size))
    if buf:
        pieces += hashlib.sha1(buf).digest()
    info = {"name": name, "piece length": PIECE_LEN, "pieces": bytes(pieces)}
    if multi:
        info["files"] = [{"length": s, "path": parts} for parts, s in paths]
        total = sum(s for _, s in paths)
    else:
        info["length"] = paths[0][1]
        total = paths[0][1]
    t = {"announce": announce, "created by": "torrentrs-bench", "info": info}
    return name, t, total, multi


def ensure_scale_set(n, announce, seed=7):
    """Create (idempotently) n synthetic torrents + data under bench/runs/scale/.  Returns manifest list of
    {"name","torrent","size","multi","infohash"}; torrent i is always the same bytes for a given seed, so
    the 50/200 sets are prefixes of the 500 set."""
    tdir, ddir = SCALE_DIR / "torrents", SCALE_DIR / "data"
    tdir.mkdir(parents=True, exist_ok=True)
    ddir.mkdir(parents=True, exist_ok=True)
    mf_path = SCALE_DIR / "manifest.json"
    mf = json.loads(mf_path.read_text()) if mf_path.exists() else {"seed": seed, "torrents": []}
    if mf.get("seed") != seed:
        raise BenchError("scale set was generated with another seed; delete bench/runs/scale/")
    have = mf["torrents"]
    t0 = time.time()
    for i in range(len(have), n):
        name, t, total, multi = _build(i, seed, ddir, announce)
        tfile = tdir / f"bench-{i:04d}.torrent"
        tfile.write_bytes(bencode(t))
        have.append({"name": name, "torrent": str(tfile), "size": total,
                     "multi": multi, "infohash": info_hash(t)})
        if (i + 1) % 50 == 0:
            log(f"generated {i + 1}/{n} synthetic torrents ({time.time() - t0:.0f}s)")
    # data on disk must match the manifest (a previous run may have been interrupted)
    for m in have[:n]:
        p = ddir / m["name"]
        if not p.exists():
            raise BenchError(f"data for {m['name']} missing; delete bench/runs/scale/ and regenerate")
    # announce URL change -> rewrite only the torrent files (info dict, hence info-hash, is unchanged)
    if mf.get("announce") != announce:
        for m in have:
            d = read_torrent(m["torrent"])
            d[b"announce"] = announce.encode()
            Path(m["torrent"]).write_bytes(bencode(d))
            assert info_hash(d) == m["infohash"]
        mf["announce"] = announce
    mf["torrents"] = have
    mf_path.write_text(json.dumps(mf, indent=1))
    return have[:n], ddir


# ------------------------------------------------------------------ mock tracker
class MockTracker:
    """`tracker-probe mock`: valid announce responses, optional static peer list, JSONL log of every request."""

    def __init__(self, port, out, interval=1800, min_interval=900, peers=None):
        self.port, self.out = port, Path(out)
        self.interval, self.min_interval, self.peers = interval, min_interval, peers or []
        self.proc = None

    @property
    def announce_url(self):
        return f"http://localhost:{self.port}/announce"      # localhost: clients also try [::1] (refused), like capture/README

    def start(self):
        if not PROBE.exists():
            log("building tracker-probe")
            subprocess.run(["cargo", "build", "--release", "-p", "tracker-probe"], cwd=REPO, check=True)
        self.out.parent.mkdir(parents=True, exist_ok=True)
        if self.out.exists():
            self.out.unlink()
        cmd = [str(PROBE), "mock", "--listen", f"127.0.0.1:{self.port}", "--interval", str(self.interval),
               "--min-interval", str(self.min_interval), "--out", str(self.out), "--label", "bench"]
        if self.peers:
            cmd += ["--peers", ",".join(self.peers)]
        err = open(str(self.out) + ".stderr", "ab")
        self.proc = spawn(cmd, stdout=err, stderr=err)
        for _ in range(40):
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{self.port}/_marker/ready", timeout=2).read()
                return self
            except Exception:
                if self.proc.poll() is not None:
                    raise BenchError("tracker-probe mock exited early")
                time.sleep(0.25)
        raise BenchError("mock tracker did not come up")

    def marker(self, name):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{self.port}/_marker/{name}", timeout=3).read()
        except Exception:
            pass

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGINT)       # graceful: flushes the log
            if wait_exit(self.proc, 10) is None:
                self.proc.kill()

    def announce_stats(self, t_from=None, t_to=None):
        """Counts from the JSONL log (call after stop()). Window in unix seconds."""
        total, by_event, hashes, scrapes = 0, {}, set(), 0
        if not self.out.exists():
            return {}
        with open(self.out) as f:
            for line in f:
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                ts = r.get("ts_ms", 0) / 1000
                if (t_from and ts < t_from) or (t_to and ts > t_to):
                    continue
                if r.get("kind") == "scrape":
                    scrapes += 1
                if r.get("kind") != "announce":
                    continue
                total += 1
                ev = next((p["dec"] for p in r.get("params", []) if p["k"] == "event"), "") or "periodic"
                by_event[ev] = by_event.get(ev, 0) + 1
                hashes.add(r.get("info_hash"))
        return {"announces": total, "by_event": by_event, "unique_info_hashes": len(hashes), "scrapes": scrapes}
