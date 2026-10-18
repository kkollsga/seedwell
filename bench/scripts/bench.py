#!/usr/bin/env python3
"""TorrentRS comparison benchmark harness.  Run with:  python3 -I bench/scripts/bench.py <command> ...

Commands
  netcheck                       quick speed sanity check (curl, ~200 MB)
  fetch-torrents                 download the official distro .torrent files into bench/torrents/ (skips existing)
  download  --clients A,B --plan debian:2,arch:1,ubuntu:1   real-world swarm downloads (interleaved A,B,A,B)
  scale     --clients A,B --sizes 50,200,500                N synthetic seeding torrents on a local mock tracker
  hot       --clients A,B --n 200                           seeder under load from 3 local leechers
  report    [--campaign ID]                                 write bench/reports/<date>-baseline.md
  cleanup                                                   kill stray bench client processes
See bench/README.md.
"""
import argparse
import os
import sys
import time
import urllib.request

# -I (isolated mode) does not put the script dir on sys.path
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from benchlib import runners, util  # noqa: E402
from benchlib.adapters import ADAPTERS  # noqa: E402

TORRENT_URLS = {
    "debian-netinst.torrent": None,     # place the official Debian netinst .torrent in bench/torrents/
    "archlinux-2026.10.01-x86_64.iso.torrent": "https://archlinux.org/releng/releases/2026.10.01/torrent/",
    "ubuntu-26.04.1-live-server-amd64.iso.torrent": "https://releases.ubuntu.com/26.04.1/ubuntu-26.04.1-live-server-amd64.iso.torrent",
    "Fedora-Workstation-Live-x86_64-44.torrent": "https://torrent.fedoraproject.org/torrents/Fedora-Workstation-Live-x86_64-44.torrent",
}


def clients_arg(s):
    names = [c.strip() for c in s.split(",") if c.strip()]
    for n in names:
        if n not in ADAPTERS:
            raise SystemExit(f"unknown client {n}; known: {', '.join(ADAPTERS)}")
    out = []
    for n in names:
        ok, why = ADAPTERS[n]().available()
        if not ok:
            util.log(f"SKIP {n}: {why}")
        else:
            out.append(n)
    if not out:
        raise SystemExit("no usable client")
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    campaign_default = time.strftime("%Y-%m-%d")
    for name in ("netcheck", "download", "scale", "hot", "report"):
        p = sub.add_parser(name)
        p.add_argument("--campaign", default=campaign_default, help="label grouping runs into one report (default: today)")
        if name == "download":
            p.add_argument("--clients", default="transmission,qbittorrent")
            p.add_argument("--plan", default="debian:2,arch:1", help="torrent:repeats,... (debian arch ubuntu fedora)")
            p.add_argument("--seed-secs", type=int, default=120)
            p.add_argument("--max-secs", type=int, default=3600)
            p.add_argument("--budget-gb", type=float, default=15.0)
            p.add_argument("--resume", action="store_true", help="skip runs already completed in this campaign")
        if name == "scale":
            p.add_argument("--clients", default="transmission,qbittorrent")
            p.add_argument("--sizes", default="50,200,500")
            p.add_argument("--idle-secs", type=int, default=600)
            p.add_argument("--reps", type=int, default=1)
            p.add_argument("--resume", action="store_true")
        if name == "hot":
            p.add_argument("--clients", default="transmission,qbittorrent")
            p.add_argument("--n", type=int, default=200)
            p.add_argument("--leecher", default="transmission")
            p.add_argument("--rounds", type=int, default=10)
            p.add_argument("--reps", type=int, default=1)
            p.add_argument("--resume", action="store_true")
    sub.add_parser("cleanup")
    sub.add_parser("fetch-torrents")
    a = ap.parse_args()
    util.CMDLINE = "python3 -I bench/scripts/bench.py " + " ".join(sys.argv[1:])
    util.install_cleanup()

    if a.cmd == "cleanup":
        util.kill_strays()
        return
    if a.cmd == "fetch-torrents":
        util.TORRENTS.mkdir(parents=True, exist_ok=True)
        for fn, url in TORRENT_URLS.items():
            dst = util.TORRENTS / fn
            if dst.exists():
                continue
            if url is None:
                raise SystemExit(f"{dst} is missing: put the official Debian netinst .torrent there")
            else:
                dst.write_bytes(urllib.request.urlopen(url, timeout=60).read())
            util.log(f"fetched {fn}")
        return
    if a.cmd == "report":
        from benchlib import report
        print(report.write_report(a.campaign))
        return

    util.kill_strays()
    if a.cmd == "netcheck":
        runners.netcheck(a.campaign)
    elif a.cmd == "download":
        plan = []
        for item in a.plan.split(","):
            t, _, n = item.partition(":")
            if t not in runners.TORRENT_FILES:
                raise SystemExit(f"unknown torrent {t}")
            plan.append((t, int(n or 1)))
        runners.download_campaign(clients_arg(a.clients), plan, a.campaign, budget_gb=a.budget_gb,
                                  resume=a.resume, seed_secs=a.seed_secs, max_secs=a.max_secs)
    elif a.cmd == "scale":
        done = {(r["client"], r["n"], r["rep"]) for r in util.load_results()
                if r.get("kind") == "scale" and r.get("campaign") == a.campaign and r.get("status") == "ok"}
        clients = clients_arg(a.clients)
        for rep in range(1, a.reps + 1):
            for n in [int(x) for x in a.sizes.split(",")]:
                for c in clients:
                    if a.resume and (c, n, rep) in done:
                        continue
                    runners.with_retry(runners.run_scale, c, n, a.campaign, idle_secs=a.idle_secs, rep=rep)
                    time.sleep(5)
    elif a.cmd == "hot":
        done = {(r["client"], r["n"], r["rep"]) for r in util.load_results()
                if r.get("kind") == "hot" and r.get("campaign") == a.campaign and r.get("status") == "ok"}
        clients = clients_arg(a.clients)
        for rep in range(1, a.reps + 1):
            for c in clients:
                if a.resume and (c, a.n, rep) in done:
                    continue
                runners.with_retry(runners.run_hot, c, a.n, a.campaign, leecher_client=a.leecher, rounds=a.rounds, rep=rep)
                time.sleep(5)


if __name__ == "__main__":
    main()
