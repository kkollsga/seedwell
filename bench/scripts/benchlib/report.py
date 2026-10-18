"""Generate bench/reports/<date>-baseline.md from bench/runs/results.jsonl."""
import statistics
from collections import defaultdict

from .runners import machine_info
from .util import BENCH, load_results

MB = 1e6
MIB = 1024 * 1024


def stat(vals, scale=1.0, nd=1, unit=""):
    vals = [v for v in vals if v is not None]
    if not vals:
        return "n/a"
    f = lambda x: f"{x / scale:.{nd}f}"
    m = statistics.median(vals)
    if len(vals) == 1:
        return f"{f(m)}{unit}"
    return f"{f(m)}{unit} ({f(min(vals))}–{f(max(vals))})"


def mmss(sec):
    if sec is None:
        return "n/a"
    return f"{int(sec // 60)}:{int(sec % 60):02d}"


def stat_time(vals):
    vals = [v for v in vals if v is not None]
    if not vals:
        return "n/a"
    m = statistics.median(vals)
    if len(vals) == 1:
        return mmss(m)
    return f"{mmss(m)} ({mmss(min(vals))}–{mmss(max(vals))})"


def table(head, rows):
    out = ["| " + " | ".join(head) + " |", "|" + "|".join("---" for _ in head) + "|"]
    out += ["| " + " | ".join(str(c) for c in r) + " |" for r in rows]
    return "\n".join(out) + "\n"


def kb_mb(v):
    return None if v is None else v * 1024


def write_report(campaign):
    res = [r for r in load_results() if r.get("campaign") == campaign]
    if not res:
        raise SystemExit(f"no results for campaign {campaign}")
    ok = [r for r in res if r.get("status") == "ok"]
    dl = [r for r in ok if r["kind"] == "download"]
    sc = [r for r in ok if r["kind"] == "scale"]
    hot = [r for r in ok if r["kind"] == "hot"]
    net = [r for r in res if r["kind"] == "netcheck"]
    bad = [r for r in res if r.get("status") not in ("ok", None) and r["kind"] != "netcheck"]
    clients = sorted({r["client"] for r in ok}, key=lambda c: ["transmission", "qbittorrent", "torrentrs"].index(c) if c in ("transmission", "qbittorrent", "torrentrs") else 9)
    versions = {r["client"]: r["version"] for r in ok}
    mi = machine_info()
    L = []
    w = L.append
    w(f"# TorrentRS baseline benchmark, campaign {campaign}\n")
    w("Median of runs with (min–max) in parentheses. All numbers are produced by `bench/scripts/bench.py`; "
      "raw per-second samples are in `bench/runs/csv/` (gitignored), run records in `bench/runs/results.jsonl`.\n")
    w("## Machine and conditions\n")
    w(table(["Item", "Value"], [
        ["Machine", f"{mi['model']} ({mi['chip']}), {mi['ncpu']} cores, {mi['memsize_gb']} GiB RAM"],
        ["macOS", f"{mi['macos']} ({mi['build']}), Darwin {mi['kernel']}"],
        ["Python", mi["python"]],
        ["Report generated", mi["uptime"]],
    ] + [[f"Client: {c}", versions[c]] for c in clients]))
    if net:
        w("**Network speed sanity check** (curl, single connection):\n")
        rows = []
        for n in net:
            for t in n["tests"]:
                if t["bytes"] < 1_000_000:          # failed probe (e.g. HTTP error page)
                    continue
                rows.append([n["start"], t["name"], t["final_host"], f"{t['bytes'] / MB:.0f} MB", f"{t['secs']:.1f} s",
                             f"{t['Bps'] / MB:.1f} MB/s ({t['Bps'] * 8 / MB:.0f} Mbit/s)"])
        w(table(["When", "Test", "Host", "Size", "Time", "Throughput"], rows))
    loads = [r["loadavg_start"][0] for r in res if r.get("loadavg_start")]
    if loads:
        w(f"Machine load average (1 min) at the start of runs: median {statistics.median(loads):.1f}, "
          f"min {min(loads):.1f}, max {max(loads):.1f}. Other agents compile in parallel on this machine, so absolute CPU "
          "figures include scheduling noise; per-run values are in the tables below.\n")
    used = sum(r.get("bytes_downloaded", 0) for r in res)
    w(f"**Bandwidth used by this campaign (downloaded):** {used / 1e9:.2f} GB.\n")

    # ------------------------------------------------------------- download
    if dl:
        w("## 1. Real-world download (public swarms, legal Linux ISOs)\n")
        w("Fresh profile per run, no rate limits, DHT/PEX/LSD/uTP/port mapping at client defaults (on), "
          "same fixed listen port 51413, peer limits equalised (200 global / 100 per torrent), download queue off, "
          "interleaved A,B,A,B, files and state deleted after each run, 2 min seeding after completion. "
          "Peak rate is the best 10 s average computed from the byte counter (client-agnostic). "
          "RSS = `ps -o rss` summed over the process tree sampled every 1 s; footprint = `phys_footprint` "
          "(Activity Monitor / `footprint`, excludes clean file-backed pages). Steady = median over the second half of the download. "
          "CPU = user+sys seconds of the client process (`proc_pid_rusage`).\n")
        for t in sorted({r["torrent"] for r in dl}):
            rs = [r for r in dl if r["torrent"] == t]
            size = rs[0]["total_bytes"]
            w(f"### {t} ({size / 1e9:.2f} GB)\n")
            speed, res_rows = [], []
            for c in clients:
                x = [r for r in rs if r["client"] == c]
                if not x:
                    continue
                speed.append([c, len(x), stat_time([r.get("t_complete_s") for r in x]),
                              stat([r.get("mean_down_Bps") for r in x], MB, 2, " MB/s"),
                              stat([r.get("peak_down_10s_Bps") for r in x], MB, 2, " MB/s"),
                              stat([r.get("t_first_byte_s") for r in x], 1, 0, " s"),
                              stat([r.get("peers_peak") for r in x], 1, 0), stat([r.get("peers_median") for r in x], 1, 0)])
                res_rows.append([c, stat([kb_mb(r.get("rss_peak_kb")) for r in x], MIB, 0, " MiB"),
                                 stat([kb_mb(r.get("rss_steady_kb")) for r in x], MIB, 0, " MiB"),
                                 stat([kb_mb(r.get("foot_peak_kb")) for r in x], MIB, 0, " MiB"),
                                 stat([r.get("cpu_s_at_complete") for r in x], 1, 1, " s"),
                                 stat([r.get("cpu_pct_while_downloading") for r in x], 1, 1, " %"),
                                 stat([kb_mb(r.get("seed_rss_median_kb")) for r in x], MIB, 0, " MiB"),
                                 stat([r.get("seed_cpu_pct") for r in x], 1, 2, " %"),
                                 stat([r.get("seed_pkg_wkups_per_s") for r in x], 1, 1)])
            w(table(["Client", "Runs", "Time to complete (m:ss)", "Mean rate", "Peak rate (10 s avg)", "Time to first byte",
                     "Peers (peak)", "Peers (median)"], speed))
            w(table(["Client", "RSS peak", "RSS steady", "Footprint peak", "CPU time to complete", "CPU % while downloading",
                     "RSS seeding (median)", "CPU % seeding", "Idle wake-ups/s seeding"], res_rows))
        w("### All download runs\n")
        w(table(["Start", "Torrent", "Client", "Rep", "Load1 start", "Load1 mean", "Complete (m:ss)", "Mean MB/s", "Peak RSS MiB", "CPU s", "Seed s"],
                [[r["start"][5:16], r["torrent"], r["client"], r["rep"], r["loadavg_start"][0], r.get("load1_mean"),
                  mmss(r.get("t_complete_s")), f"{(r.get('mean_down_Bps') or 0) / MB:.2f}", f"{(r.get('rss_peak_kb') or 0) / 1024:.0f}",
                  r.get("cpu_s_total"), r.get("seed_secs_target")] for r in sorted(dl, key=lambda r: r["start"])]))

    # ------------------------------------------------------------- scale
    if sc:
        w("## 2. Scale / idle memory (local, no Internet)\n")
        w("N synthetic torrents (random data 1–20 MB, one third multi-file, 256 KiB pieces; all data already complete) "
          "are loaded into one client process and verified, then seeded idle on loopback against `tracker-probe mock` "
          "(announce interval 1800 s, DHT/PEX/LSD/port mapping off). No API polling happens during the idle window, so the "
          "measurement does not perturb the client. t0 = all N torrents seeding. "
          "Headline memory is `footprint` (`phys_footprint`, SPEC §2 metric); RSS (`ps`) is shown too and includes file-backed pages "
          "that libtorrent's mmap disk I/O leaves resident after verification. "
          "Wake-ups come from `proc_pid_rusage` (`ri_pkg_idle_wkups` / `ri_interrupt_wkups`, the counters `powermetrics` reports, no sudo needed); "
          "`top` IDLEW is a 20 s cross-check at the end.\n")
        sizes = sorted({r["n"] for r in sc})
        rows = []
        for n in sizes:
            for c in clients:
                x = [r for r in sc if r["n"] == n and r["client"] == c]
                if not x:
                    continue
                rows.append([n, c, len(x), stat([r["ready_secs"] for r in x], 1, 0, " s"),
                             stat([kb_mb(r["foot_ready_kb"]) for r in x], MIB, 1, " MiB"),
                             stat([kb_mb(r["foot_2min_kb"]) for r in x], MIB, 1, " MiB"),
                             stat([kb_mb(r["foot_10min_kb"]) for r in x], MIB, 1, " MiB"),
                             stat([kb_mb(r["rss_ready_kb"]) for r in x], MIB, 1, " MiB"),
                             stat([kb_mb(r["rss_2min_kb"]) for r in x], MIB, 1, " MiB"),
                             stat([kb_mb(r["rss_10min_kb"]) for r in x], MIB, 1, " MiB"),
                             stat([kb_mb(r["rss_peak_load_kb"]) for r in x], MIB, 0, " MiB")])
        w("**Memory**\n")
        w(table(["N", "Client", "Runs", "Time to all seeding", "Footprint at t0", "Footprint t0+2 min", "Footprint t0+10 min",
                 "RSS at t0", "RSS t0+2 min", "RSS t0+10 min", "RSS peak while loading"], rows))
        rows = []
        for n in sizes:
            for c in clients:
                x = [r for r in sc if r["n"] == n and r["client"] == c]
                if not x:
                    continue
                ev = lambda r, k: r.get("tracker_idle", {}).get("by_event", {}).get(k, 0)
                idle_ann = [r.get("tracker_idle", {}).get("announces", 0) - ev(r, "started") - ev(r, "stopped") for r in x]
                late_started = [ev(r, "started") for r in x]
                setup_ann = [r.get("tracker_setup", {}).get("announces") for r in x]
                top = [r["top_idlew"]["idlew_per_s"] for r in x if r.get("top_idlew")]
                topcpu = [r["top_idlew"]["cpu_pct"] for r in x if r.get("top_idlew")]
                rows.append([n, c, stat([r["cpu_pct_idle"] for r in x], 1, 3, " %"),
                             stat([r["cpu_pct_idle_after2min"] for r in x], 1, 3, " %"),
                             stat([r["pkg_wkups_per_s"] for r in x], 1, 1), stat([r["pkg_wkups_per_s_after2min"] for r in x], 1, 1),
                             stat([r["intr_wkups_per_s_after2min"] for r in x], 1, 1),
                             stat(top, 1, 1) if top else "n/a", stat(topcpu, 1, 1, " %") if topcpu else "n/a",
                             stat(setup_ann, 1, 0), stat(late_started, 1, 0), stat(idle_ann, 1, 0)])
        w("**Idle CPU, wake-ups, tracker announces**\n")
        w(table(["N", "Client", "CPU % over idle window", "CPU % after first 2 min", "Idle wake-ups/s (whole window)",
                 "Idle wake-ups/s (after 2 min)", "Interrupt wake-ups/s (after 2 min)", "`top` IDLEW/s", "`top` %CPU",
                 "`started` announces before t0", "`started` announces after t0 (staggered start-up)", "Periodic announces during idle"], rows))
        # marginal cost per torrent
        rows = []
        for c in clients:
            lo = [r for r in sc if r["client"] == c and r["n"] == min(sizes)]
            hi = [r for r in sc if r["client"] == c and r["n"] == max(sizes)]
            if lo and hi and min(sizes) != max(sizes):
                dn = max(sizes) - min(sizes)
                fl = statistics.median([r["foot_10min_kb"] for r in lo])
                fh = statistics.median([r["foot_10min_kb"] for r in hi])
                rl = statistics.median([r["rss_10min_kb"] for r in lo])
                rh = statistics.median([r["rss_10min_kb"] for r in hi])
                rows.append([c, f"{min(sizes)} → {max(sizes)}", f"{(fh - fl) / dn:.1f} KiB", f"{(rh - rl) / dn:.1f} KiB"])
        if rows:
            w("**Marginal cost of one more idle seeding torrent** (SPEC §2 target: ≤ 4 KB resident)\n")
            w(table(["Client", "N range", "Δ footprint per torrent", "Δ RSS per torrent"], rows))

    # ------------------------------------------------------------- hot
    if hot:
        w("## 3. Hot variant: seeder serving local leechers\n")
        w("The seeder holds N torrents (as above). Per round, 3 fresh leecher instances (separate processes, "
          "profile and download dirs) download the same 10 largest torrents simultaneously from the seeder over the LAN address "
          "(the mock tracker hands out the seeder as the only peer); leechers are discarded and re-created each round. "
          "Throughput is ground truth from the leechers' byte counters (first byte → all complete), not the seeder's smoothed rate. "
          "Footprint/RSS are the seeder's.\n")
        rows = []
        for n in sorted({r["n"] for r in hot}):
            for c in clients:
                x = [r for r in hot if r["n"] == n and r["client"] == c]
                if not x:
                    continue
                rows.append([n, c, f"{x[0]['n_leechers']}x {x[0]['leecher_version']}", x[0]["rounds"],
                             stat([r["upload_Bps_mean"] for r in x], MB, 0, " MB/s"),
                             stat([r["upload_Bps_peak_1s"] for r in x], MB, 0, " MB/s"),
                             stat([r["seeder_reported_up_peak_Bps"] for r in x], MB, 0, " MB/s"),
                             stat([r["cpu_pct_hot"] for r in x], 1, 0, " %"),
                             stat([r["cpu_s_per_GB_served"] for r in x], 1, 2, " s/GB"),
                             stat([kb_mb(r["foot_baseline_kb"]) for r in x], MIB, 1, " MiB"),
                             stat([kb_mb(r["foot_hot_peak_kb"]) for r in x], MIB, 1, " MiB"),
                             stat([kb_mb(r["foot_after_kb"]) for r in x], MIB, 1, " MiB"),
                             stat([kb_mb(r["rss_hot_peak_kb"]) for r in x], MIB, 0, " MiB")])
        w(table(["N", "Seeder", "Leechers", "Rounds", "Upload mean", "Upload peak (1 s)", "Seeder-reported peak", "Seeder CPU % (while transferring)",
                 "CPU per GB served", "Footprint before", "Footprint hot peak", "Footprint after", "RSS hot peak"], rows))

    if bad:
        w("## Failed / incomplete runs (excluded from tables)\n")
        w(table(["Start", "Kind", "Client", "Status", "Detail"],
                [[r["start"], r["kind"], r["client"], r.get("status"), (str(r.get("error") or "") + f" ({r.get('torrent') or 'N=' + str(r.get('n'))})")] for r in bad]))

    w("## Exact commands\n")
    cmds = []
    for r in res:
        c = r.get("cmd")
        if c and c not in cmds:
            cmds.append(c)
    w("```sh\n" + "\n".join(cmds) + "\n```\n")
    trs = [r for r in ok if r["client"] == "torrentrs"]
    w(f"Environment: TorrentRS = `$TRS_BENCH_BIN` ({trs[0]['version'] if trs else 'not run'}); qBittorrent binary "
      "`$QBT` (qBittorrent 5.2.3); Transmission from Homebrew `transmission-cli`.\n")
    notes = BENCH / "reports" / f"notes-{campaign}.md"
    if notes.exists():
        w("## Notes and anomalies\n")
        w(notes.read_text())
    out = BENCH / "reports" / f"{campaign}-baseline.md"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text("\n".join(L))
    return str(out)
