#!/usr/bin/env python3
"""The big-folder scene's phases and verdict (capture_bigfolder.sh).

    bigfolder.py phase --ctl <qa-control> --cli <seedwell> --sock <control.sock> --secs N --out <json>
                       [--peer-sock <the second device's control.sock>] [--channel NAME] [--torrent NAME] [--add --folder NAME --shots DIR]
    bigfolder.py verdict <baseline.json> <add.json> [--runs-file <file>]

A phase starts the app's latency probes (`ui latency start`: the main thread's beats every 10 ms, the core's reads
every 50 ms), then cycles through every part of the app as fast as the app answers (each QA command timed from the
moment it is written to its ack), while a thread of its own makes CLI round trips (`status`, `sync list`) through the
app's control socket, timed from outside, and `sync list` on the second device (`--peer-sock`), which joins the folder
meanwhile; the verdict shows each command's slowest. `--add` presses the Add Folder window's Start first (the window is set up by
the script) and adds the new folder's own surfaces to the cycle, with a few shots (excused from the verdict).

The verdict compares the add phase with the thresholds below; they are the user's "100 % interactive", not tuned to
pass: a miss is the finding.
"""
import argparse
import json
import subprocess
import sys
import threading
import time

# During the add (thresholds of the request; never loosened to pass).
BEAT_MS = 100        # no main-thread beat later than this, outside the renderer's captures
SURFACE_MS = 500     # every surface command answered within this
CLI_MS = 1000        # every CLI round trip within this
CORE_MS = 500        # the core's reads (snapshot, sync snapshot) under this

TABS = ["general", "transfers", "folders", "rules", "network", "folderSync", "devices", "agents", "advanced"]


class QA:
    """The app's QA control file: commands appended, `<file>.ack` counts the lines done, reports in `<file>.bench`."""

    def __init__(self, ctl):
        self.ctl = ctl
        self.bench = ctl + ".bench"

    def _lines(self):
        with open(self.ctl, "rb") as f:
            return f.read().count(b"\n")

    def _ack(self):
        try:
            with open(self.ctl + ".ack") as f:
                return int(f.read().strip() or 0)
        except (OSError, ValueError):
            return -1

    def cmd(self, line, timeout=120):
        """Runs one command; the time from writing it to its ack (ms), and its report lines."""
        before = self._bench_len()
        want = self._lines() + 1
        t0 = time.monotonic()
        with open(self.ctl, "a") as f:
            f.write(line + "\n")
        while self._ack() != want:
            if time.monotonic() - t0 > timeout:
                raise RuntimeError(f"no answer in {timeout} s to: {line}")
            time.sleep(0.002)
        ms = (time.monotonic() - t0) * 1000
        return ms, self._bench_new(before)

    def _bench_len(self):
        try:
            with open(self.bench, "rb") as f:
                return f.read().count(b"\n")
        except OSError:
            return 0

    def _bench_new(self, before):
        try:
            with open(self.bench, encoding="utf-8", errors="replace") as f:
                return f.read().splitlines()[before:]
        except OSError:
            return []


class CliLoop(threading.Thread):
    """CLI round trips through the app's control socket, one after the other, timed from outside; with `peer_sock`, also
    `sync list` on the second device, which joins the folder while this one adds it."""

    def __init__(self, cli, sock, peer_sock=None):
        super().__init__(daemon=True)
        self.cli, self.sock = cli, sock
        self.calls = [("status", sock, ["status"]), ("sync list", sock, ["sync", "list"])]
        if peer_sock:
            self.calls.append(("sync list (peer)", peer_sock, ["sync", "list"]))
        self.stop = threading.Event()
        self.trips = []  # (name, ms, ok, at_s)
        self.t0 = time.monotonic()

    def run(self):
        while not self.stop.is_set():
            for name, sock, args in self.calls:
                t = time.monotonic()
                try:
                    r = subprocess.run([self.cli, "--socket", sock] + args, capture_output=True, timeout=30)
                    ok = r.returncode == 0
                except subprocess.TimeoutExpired:
                    ok = False
                self.trips.append((name, (time.monotonic() - t) * 1000, ok, t - self.t0))
            self.stop.wait(0.1)


def latency(qa):
    _, lines = qa.cmd("ui latency report")
    for l in lines:
        if l.startswith("latency-json: "):
            return json.loads(l[len("latency-json: "):]), [x for x in lines if not x.startswith("latency-json")]
    raise RuntimeError("no latency report: " + " | ".join(lines))


def phase(a):
    qa = QA(a.ctl)
    cycle = ["menu open", "menu-sync", "menu close", "window open", "section downloads", "section sharing"]
    # Settings opened as the menu's Settings… opens it, then every tab as a click on its tab bar.
    cycle += ["settings general"] + [f"settings-tab {t}" for t in TABS[1:]] + ["settings close"]
    if a.channel:
        cycle.append(f"chat open {a.channel}")
    if a.torrent:
        cycle += [f"getinfo {a.torrent}", "info none"]
    cycle.append("ui ping")
    if a.add:
        cycle += [f"sync-page {a.folder}", f"sync-edit {a.folder}", f"sync-share {a.folder}", "sync-close"]
    shots = []
    if a.add and a.shots:
        # A few pictures while the folder lists: the menu's Folder Sync row and the folder's page (excused).
        shots = [(1.0, "menu open", f"shot {a.shots}/bigfolder-menu-1s.png menu"),
                 (4.0, "menu open", f"shot {a.shots}/bigfolder-menu-4s.png menu"),
                 (8.0, f"sync-page {a.folder}", f"shot {a.shots}/bigfolder-page-8s.png main")]

    surface = []  # (command, ms, at_s, report)
    cli = CliLoop(a.cli, a.sock, a.peer_sock)
    qa.cmd("ui latency start")
    t0 = time.monotonic()
    cli.t0 = t0
    cli.start()
    if a.add:
        ms, rep = qa.cmd("sync-add start")
        surface.append(("sync-add start", ms, 0.0, rep[-1] if rep else ""))
    i = 0
    shot_ms = []
    while time.monotonic() - t0 < a.secs:
        at = time.monotonic() - t0
        if shots and at >= shots[0][0]:
            _, prep, shot = shots.pop(0)
            qa.cmd(prep)
            ms, rep = qa.cmd(shot)
            shot_ms.append((shot.split()[1], ms))
            continue
        c = cycle[i % len(cycle)]
        i += 1
        ms, rep = qa.cmd(c)
        surface.append((c, ms, at, rep[-1] if rep else ""))
    qa.cmd("ui latency stop")
    cli.stop.set()
    cli.join(timeout=35)
    lat, lat_lines = latency(qa)
    out = {
        "name": a.name,
        "secs": a.secs,
        "latency": lat,
        "latency_lines": lat_lines,
        "surface": [{"cmd": c, "ms": ms, "at_s": at, "report": r} for c, ms, at, r in surface],
        "cli": [{"cmd": n, "ms": ms, "ok": ok, "at_s": at} for n, ms, ok, at in cli.trips],
        "shots": [{"shot": n, "ms": ms} for n, ms in shot_ms],
    }
    with open(a.out, "w") as f:
        json.dump(out, f, indent=1)
    s = summary(out)
    print(f"{a.name}: beats {s['beats']} max {s['beat_max']:.1f} ms ({s['beat_action']}) p99 {s['beat_p99']:.1f} ms "
          f"over50 {s['over50']} over100 {s['over100']} | surface {s['surface_n']} max {s['surface_max']:.0f} ms "
          f"({s['surface_max_cmd']}) | cli {s['cli_n']} max {s['cli_max']:.0f} ms | core max {s['core_max']:.1f} ms")
    for l in lat_lines:
        print("   " + l)


def p99(v):
    if not v:
        return 0.0
    v = sorted(v)
    return v[min(len(v) - 1, max(0, -(-99 * len(v) // 100) - 1))]


def summary(d):
    lat = d["latency"]
    surf = d["surface"]
    cli = d["cli"]
    core = lat.get("core", {})
    worst_surf = max(surf, key=lambda x: x["ms"]) if surf else {"cmd": "-", "ms": 0}
    worst_cli = max(cli, key=lambda x: x["ms"]) if cli else {"cmd": "-", "ms": 0}
    return {
        "beats": lat["beats"],
        "beat_max": lat["max_ms"],
        "beat_action": lat["max_action"],
        "beat_p99": lat["p99_ms"],
        "beat_max_ran": lat.get("max_ran_ms", 0),
        "over50": lat["over50"],
        "over100": lat["over100"],
        "excused_max": lat["excused_max_ms"],
        "surface_n": len(surf),
        "surface_max": worst_surf["ms"],
        "surface_max_cmd": worst_surf["cmd"],
        "surface_p99": p99([x["ms"] for x in surf]),
        "cli_n": len(cli),
        "cli_max": worst_cli["ms"],
        "cli_max_cmd": worst_cli["cmd"],
        "cli_p99": p99([x["ms"] for x in cli]),
        "cli_failed": sum(1 for x in cli if not x["ok"]),
        # The slowest of each command (`sync list (peer)`: the second device's, as it joins).
        "cli_by_cmd": {c: max(x["ms"] for x in cli if x["cmd"] == c) for c in sorted({x["cmd"] for x in cli})},
        "core_snapshot_max": core.get("snapshot", {}).get("max_ms", 0),
        "core_snapshot_p99": core.get("snapshot", {}).get("p99_ms", 0),
        "core_sync_max": core.get("sync", {}).get("max_ms", 0),
        "core_sync_p99": core.get("sync", {}).get("p99_ms", 0),
        "core_max": max([v.get("max_ms", 0) for v in core.values()] or [0]),
    }


def verdict(a):
    base = json.load(open(a.baseline))
    add = json.load(open(a.add))
    b, d = summary(base), summary(add)
    rows = [
        ("main-thread beats", "beats", "{:.0f}"),
        ("beat max ms", "beat_max", "{:.1f}"),
        ("worst beat: main ran ms", "beat_max_ran", "{:.1f}"),
        ("beat p99 ms", "beat_p99", "{:.1f}"),
        ("beats > 50 ms", "over50", "{:.0f}"),
        ("beats > 100 ms", "over100", "{:.0f}"),
        ("captures (excused) max ms", "excused_max", "{:.0f}"),
        ("surface commands", "surface_n", "{:.0f}"),
        ("surface max ms", "surface_max", "{:.0f}"),
        ("surface p99 ms", "surface_p99", "{:.0f}"),
        ("CLI round trips", "cli_n", "{:.0f}"),
        ("CLI max ms", "cli_max", "{:.0f}"),
        ("CLI p99 ms", "cli_p99", "{:.0f}"),
        ("CLI failed", "cli_failed", "{:.0f}"),
        ("core snapshot max ms", "core_snapshot_max", "{:.1f}"),
        ("core snapshot p99 ms", "core_snapshot_p99", "{:.1f}"),
        ("core sync max ms", "core_sync_max", "{:.1f}"),
        ("core sync p99 ms", "core_sync_p99", "{:.1f}"),
    ]
    print(f"{'metric':28} {'baseline':>10} {'add':>10}")
    for label, k, fmt in rows:
        print(f"{label:28} {fmt.format(b[k]):>10} {fmt.format(d[k]):>10}")
    for c in sorted(set(b["cli_by_cmd"]) | set(d["cli_by_cmd"])):
        bm, dm = b["cli_by_cmd"].get(c), d["cli_by_cmd"].get(c)
        print(f"{'CLI ' + c + ' max ms':28} {'-' if bm is None else f'{bm:.0f}':>10} {'-' if dm is None else f'{dm:.0f}':>10}")
    print(f"worst beat during the add: {d['beat_max']:.1f} ms at \"{d['beat_action']}\"; worst command: "
          f"{d['surface_max_cmd']} {d['surface_max']:.0f} ms; worst CLI: {d['cli_max_cmd']} {d['cli_max']:.0f} ms")

    fails = []
    if d["beat_max"] > BEAT_MS:
        fails.append(f"a main-thread beat came {d['beat_max']:.0f} ms late (over {BEAT_MS} ms) at \"{d['beat_action']}\"; "
                     f"the main thread ran {d['beat_max_ran']:.0f} ms of it (the rest it waited: for a CPU, or blocked)")
    slow = [x for x in add["surface"] if x["ms"] > SURFACE_MS]
    if slow:
        w = max(slow, key=lambda x: x["ms"])
        fails.append(f"{len(slow)} surface command(s) took over {SURFACE_MS} ms (worst: {w['cmd']} {w['ms']:.0f} ms at +{w['at_s']:.1f} s)")
    slow = [x for x in add["cli"] if x["ms"] > CLI_MS or not x["ok"]]
    if slow:
        w = max(slow, key=lambda x: x["ms"])
        fails.append(f"{len(slow)} CLI round trip(s) over {CLI_MS} ms or failed (worst: {w['cmd']} {w['ms']:.0f} ms at +{w['at_s']:.1f} s)")
    if d["core_max"] >= CORE_MS:
        fails.append(f"a core read took {d['core_max']:.0f} ms (snapshot {d['core_snapshot_max']:.0f}, sync {d['core_sync_max']:.0f}; limit {CORE_MS} ms)")
    if a.runs_file:
        with open(a.runs_file, "a") as f:
            f.write(json.dumps({"label": a.label, "baseline": b, "add": d, "fails": fails}) + "\n")
    for x in fails:
        print("  FAIL: " + x)
    if not fails:
        print("  ok: the app stayed interactive during the add")
    sys.exit(1 if fails else 0)


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="what", required=True)
    ph = sub.add_parser("phase")
    ph.add_argument("--ctl", required=True)
    ph.add_argument("--cli", required=True)
    ph.add_argument("--sock", required=True)
    ph.add_argument("--peer-sock")
    ph.add_argument("--secs", type=float, required=True)
    ph.add_argument("--out", required=True)
    ph.add_argument("--name", default="phase")
    ph.add_argument("--channel")
    ph.add_argument("--torrent")
    ph.add_argument("--add", action="store_true")
    ph.add_argument("--folder")
    ph.add_argument("--shots")
    v = sub.add_parser("verdict")
    v.add_argument("baseline")
    v.add_argument("add")
    v.add_argument("--runs-file")
    v.add_argument("--label", default="")
    a = p.parse_args()
    if a.what == "phase":
        phase(a)
    else:
        verdict(a)


if __name__ == "__main__":
    main()
