#!/usr/bin/env python3
"""Generates legal synthetic test torrents for Seedwell QA: random data in place + .torrent files.

Nothing here touches the real home folder: every path is given on the command line and must lie
under a temp directory. Names look like real releases so content detection has something to do.

    python3 -I make_torrents.py --out /tmp/qa/swarm --count 500 [--trackers z=http://127.0.0.1:7101/announce,...]
Writes <out>/data/<name>... and <out>/torrents/<name>.torrent, plus <out>/manifest.json.
"""
import argparse, hashlib, json, os, random, sys
from pathlib import Path

PIECE = 64 * 1024
SCALE = 1


def benc(v):
    if isinstance(v, int):
        return b"i%de" % v
    if isinstance(v, bytes):
        return b"%d:%s" % (len(v), v)
    if isinstance(v, str):
        return benc(v.encode())
    if isinstance(v, list):
        return b"l" + b"".join(benc(x) for x in v) + b"e"
    if isinstance(v, dict):
        return b"d" + b"".join(benc(k) + benc(v[k]) for k in sorted(v, key=lambda k: k if isinstance(k, bytes) else k.encode())) + b"e"
    raise TypeError(type(v))


MOVIES = ["Night.of.the.Living.Dead.1968", "Sprite.Fright.2021", "Big.Buck.Bunny.2008", "Sintel.2010", "Tears.of.Steel.2012",
          "Elephants.Dream.2006", "Hidden.Rivers.2016", "Glass.Lantern.1963", "Metropolis.1927", "Her.Girl.Tuesday.1940"]
SHOWS = ["Example.Show", "Fallen.Kingdom", "Open.Documentary", "Public.Domain.Theatre", "Space.Station.Diaries"]
ALBUMS = [("Kevin.MacLeod", "Royalty.Free.Vol.{n}"), ("Open.Music.Archive", "Collection.{n}"), ("Jamendo.Selection", "Best.Of.{n}")]
BOOKS = ["Frankenstein.-.Mary.Shelley", "Pride.and.Prejudice.-.Jane.Austen", "Moby.Dick.-.Herman.Melville", "Dracula.-.Bram.Stoker"]
SOFT = ["ubuntu-26.04-desktop-arm64", "debian-13.2.0-amd64-netinst", "archlinux-2026.10.01-x86_64", "Fedora-Workstation-Live-43", "linuxmint-22-cinnamon"]


def plan(i, rng):
    """(name, [(relative path, size)], category)"""
    k = i % 7
    n = i // 7 + 1
    small = lambda lo=40, hi=900: rng.randint(lo, hi) * 1024 * SCALE
    if k == 0:
        t = MOVIES[(i // 7) % len(MOVIES)] + f".{n}.1080p.BluRay.x264" if n > len(MOVIES) else MOVIES[(i // 7) % len(MOVIES)] + ".1080p.BluRay.x264"
        return t, [(f"{t}.mkv", small(300, 1400))], "movie"
    if k == 1:
        show = SHOWS[(i // 7) % len(SHOWS)]
        s = 1 + (i // 35) % 4
        t = f"{show}.S{s:02d}.1080p.WEB-DL.x264" + (f".{n}" if n > 5 else "")
        eps = [(f"{t}/{show}.S{s:02d}E{e:02d}.1080p.mkv", small(120, 500)) for e in range(1, rng.randint(3, 6))]
        return t, eps, "tv"
    if k == 2:
        a, al = ALBUMS[(i // 7) % len(ALBUMS)]
        t = f"{a}.-.{al.format(n=n)}.(2019).FLAC"
        tr = [(f"{t}/{j:02d}.Track.{j}.flac", small(60, 260)) for j in range(1, rng.randint(4, 9))]
        return t, tr, "music"
    if k == 3:
        t = BOOKS[(i // 7) % len(BOOKS)] + (f".{n}" if n > 4 else "")
        return t, [(f"{t}.epub", small(30, 200))], "book"
    if k == 4:
        t = SOFT[(i // 7) % len(SOFT)] + (f".{n}" if n > 5 else "")
        return t, [(f"{t}.iso", small(400, 1800))], "software"
    if k == 5:
        t = f"Common.Voice.Corpus.{n}"
        fs = [(f"{t}/clips/clip{j:03d}.mp3", small(20, 120)) for j in range(1, rng.randint(5, 12))]
        return t, fs, "other"
    t = f"Public.Domain.Radio.Plays.Vol.{n}"
    return t, [(f"{t}.m4b", small(200, 900))], "audiobook"


# Hand-made torrents for the Get Info and Play checks (`--special`).
PASSKEY = "0123456789abcdef0123456789abcdef"
def _season():
    n = "Night.Harbor.S01.1080p.WEB-DL.DDP5.1.H.264"
    fs = [(f"{n}/Season 1/Night.Harbor.S01E{e:02d}.1080p.WEB-DL.mkv", 1_800_000 + e * 50_000) for e in range(1, 9)]
    fs += [(f"{n}/Season 1/Extras/Night.Harbor.S01.Sample.mkv", 300_000), (f"{n}/Season 1/Night.Harbor.S01.nfo", 4_000)]
    fs += [(f"{n}/Season 1/Subs/Night.Harbor.S01E{e:02d}.en.srt", 30_000) for e in range(1, 4)]
    return (n, fs, "tv", False, [])


def _archive():
    n = "Public.Domain.Photo.Archive"
    fs = [(f"{n}/scans/box-{b:02d}/page-{b * 100 + p:04d}.jpg", 2_500) for b in range(20) for p in range(100)]
    fs += [(f"{n}/film/reel-{r}.mkv", 200_000) for r in range(1, 4)] + [(f"{n}/README.txt", 1_200)]
    return (n, fs, "other", False, [])


def _two_seasons():
    # "Harbor Watch S01-S02" (private, announce to the local mock tracker): S01 small, S02 large, so the
    # re-download after ticking S02 runs long enough to capture.
    n = "Harbor.Watch.S01-S02.1080p.WEB-DL"
    fs = [(f"{n}/S01/Harbor.Watch.S01E{e:02d}.1080p.mkv", 2_600_000 + e * 20_000) for e in range(1, 7)]
    fs += [(f"{n}/S02/Harbor.Watch.S02E{e:02d}.1080p.mkv", 11_000_000 + e * 100_000) for e in range(1, 9)]
    return (n, fs, "tv", True, ["http://127.0.0.1:7105/announce"])


SPECIAL = {
    # Re-download QA: a private two-season pack; the user first picks S01 only, later ticks S02.
    "twoseasons": _two_seasons(),
    # Files section QA: a season pack under one top folder, and a 2,000-file archive.
    "season": _season(),
    "archive": _archive(),
    # A folder with a video, a subtitle and a poster (what webtorrent.io ships): its Play button.
    "bunny": ("Big Buck Bunny", [("Big Buck Bunny/Big Buck Bunny.mp4", 3 * 1024 * 1024), ("Big Buck Bunny/Big Buck Bunny.en.srt", 9 * 1024),
                                 ("Big Buck Bunny/poster.jpg", 120 * 1024)], "movie", False, []),
    # A torrent whose announce URLs carry passkeys (path segment, passkey=, authkey=): the masked URLs in Get Info.
    # Not flagged private: pre-release builds refuse private torrents (SPEC 7B.6); the masking does not look at the flag.
    "private": ("Hidden.Rivers.2016.1080p.BluRay.x264", [("Hidden.Rivers.2016.1080p.BluRay.x264.mkv", 1200 * 1024)], "movie", False,
                [f"http://127.0.0.1:7102/announce?passkey={PASSKEY}&authkey=AUTHSECRET99",
                 f"http://127.0.0.1:7101/{PASSKEY}/announce"]),
}


def build(out: Path, i, announce, seed, special=None):
    rng = random.Random(seed * 100003 + i)
    private, extra = False, []
    if special:
        name, files, cat, private, extra = SPECIAL[special]
    else:
        name, files, cat = plan(i, rng)
    ddir = out / "data"
    h = hashlib.sha1()
    pieces = bytearray()
    buf = b""
    multi = len(files) > 1 or "/" in files[0][0]
    entries = []
    for rel, size in files:
        p = ddir / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        data = rng.randbytes(size)
        if not (p.exists() and p.stat().st_size == size and p.read_bytes() == data):
            p.write_bytes(data)
        buf += data
        while len(buf) >= PIECE:
            pieces += hashlib.sha1(buf[:PIECE]).digest()
            buf = buf[PIECE:]
        entries.append((rel, size))
    if buf:
        pieces += hashlib.sha1(buf).digest()
    if multi:
        root = name
        info = {"name": root, "piece length": PIECE, "pieces": bytes(pieces),
                "files": [{"length": s, "path": r.split("/")[1:]} for r, s in entries]}
    else:
        info = {"name": files[0][0], "piece length": PIECE, "pieces": bytes(pieces), "length": files[0][1]}
    if private:
        info["private"] = 1
    t = {"created by": "seedwell-qa", "info": info}
    if extra:
        t["announce"] = extra[0]
        t["announce-list"] = [[u] for u in extra]
    elif announce:
        t["announce"] = announce
    tdir = out / "torrents"
    tdir.mkdir(parents=True, exist_ok=True)
    tf = tdir / f"{i:04d}-{name[:60]}.torrent"
    tf.write_bytes(benc(t))
    return {"i": i, "name": info["name"], "category": cat, "torrent": str(tf), "size": sum(s for _, s in entries),
            "infohash": hashlib.sha1(benc(info)).hexdigest(), "announce": announce, "multi": multi}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--count", type=int, default=20)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--trackers", default="", help="comma list tag=url; torrents rotate over them, 'public' = none")
    ap.add_argument("--start", type=int, default=0)
    ap.add_argument("--special", choices=sorted(SPECIAL), help="write one hand-made torrent instead of a generated set")
    ap.add_argument("--scale", type=int, default=1, help="multiply file sizes (downloads that take a while)")
    a = ap.parse_args()
    global SCALE
    SCALE = a.scale
    out = Path(a.out).resolve()
    home = Path(os.path.expanduser("~")).resolve()
    tmp = Path("/private/tmp").resolve(), Path(os.environ.get("TMPDIR", "/tmp")).resolve(), Path("/private/var").resolve()
    if not any(str(out).startswith(str(t)) for t in tmp):
        sys.exit(f"refusing: --out must be under a temp directory, got {out}")
    trackers = [("public", "")]
    if a.trackers:
        trackers = []
        for part in a.trackers.split(","):
            tag, _, url = part.partition("=")
            trackers.append((tag, "" if url in ("", "none") else url))
    rows = []
    for i in range(a.start, a.start + (1 if a.special else a.count)):
        tag, url = trackers[i % len(trackers)]
        r = build(out, i, url, a.seed, a.special)
        r["tracker"] = tag
        rows.append(r)
    mf = out / "manifest.json"
    old = json.loads(mf.read_text()) if mf.exists() else []
    byi = {r["i"]: r for r in old}
    byi.update({r["i"]: r for r in rows})
    mf.write_text(json.dumps(sorted(byi.values(), key=lambda r: r["i"]), indent=1))
    print(f"wrote {len(rows)} torrents under {out} ({sum(r['size'] for r in rows) / 1e6:.0f} MB of data)")


if __name__ == "__main__":
    main()
