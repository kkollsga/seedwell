"""The three benchmarks: real-world download, scale/idle memory, hot (local leechers)."""
import csv
import json
import os
import platform
import statistics
import subprocess
import time

from .adapters import ADAPTERS, LaunchCfg
from .torrents import MockTracker, ensure_scale_set, read_torrent
from .util import (BENCH, DOWNLOADS, PROFILES, RUNS, TORRENTS, BenchError, ProcSampler, append_result,
                   default_iface, footprint_report, kill_strays, lan_ip, load_results, loadavg, log, med, now_iso,
                   rmtree, top_idlew, wait_port_free)

CSV_DIR = RUNS / "csv"
PEER_PORT = 51413            # same fixed listen port for every client (runs are strictly sequential)
API_PORT = {"transmission": 19091, "qbittorrent": 18091, "torrentrs": 0}
TRACKER_PORT = 7171
SEEDER_PORT = 51413

TORRENT_FILES = {
    "debian": TORRENTS / "debian-netinst.torrent",
    "arch": TORRENTS / "archlinux-2026.10.01-x86_64.iso.torrent",
    "ubuntu": TORRENTS / "ubuntu-26.04.1-live-server-amd64.iso.torrent",
    "fedora": TORRENTS / "Fedora-Workstation-Live-x86_64-44.torrent",
}


# ------------------------------------------------------------------ helpers
def write_csv(name, rows):
    CSV_DIR.mkdir(parents=True, exist_ok=True)
    p = CSV_DIR / f"{name}.csv"
    if rows:
        cols = []
        for r in rows:
            for k in r:
                if k not in cols:
                    cols.append(k)
        with open(p, "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=cols)
            w.writeheader()
            w.writerows(rows)
    return str(p.relative_to(BENCH))


def make_row(t0, smp, st, extra=None):
    row = {"t": round(smp["t"] - t0, 2), "rss_kb": smp["rss_kb"], "foot_kb": smp["foot_kb"],
           "cpu_s": round(smp["cpu_s"], 3), "pkg_wkups": smp["pkg_wkups"], "intr_wkups": smp["intr_wkups"],
           "nproc": smp["nproc"], "load1": loadavg()[0]}
    if st:
        row.update({"done_bytes": st["done_bytes"], "total_bytes": st["total_bytes"], "down_rate": st["down_rate"],
                    "up_rate": st["up_rate"], "peers": st["peers"], "state": st["state"], "n_seeding": st["n_seeding"],
                    "n_checking": st["n_checking"]})
    if extra:
        row.update(extra)
    return row


def safe_poll(ad):
    try:
        return ad.poll()
    except Exception as e:           # API hiccup (client busy): keep sampling
        log(f"poll failed: {e}")
        return None


def tick(next_t, period):
    next_t += period
    d = next_t - time.time()
    if d > 0:
        time.sleep(d)
    else:
        next_t = time.time()
    return next_t


def moving_peak(rows, key, window=10):
    best = 0.0
    for i in range(window, len(rows)):
        dt = rows[i]["t"] - rows[i - window]["t"]
        if dt > 0 and key in rows[i] and key in rows[i - window]:
            best = max(best, (rows[i][key] - rows[i - window][key]) / dt)
    return best


def total_bytes_used(campaign):
    return sum(r.get("bytes_downloaded", 0) for r in load_results() if r.get("campaign") == campaign)


def machine_info():
    def sh(*a):
        return subprocess.run(a, capture_output=True, text=True).stdout.strip()
    return {"model": sh("sysctl", "-n", "hw.model"), "chip": sh("sysctl", "-n", "machdep.cpu.brand_string"),
            "ncpu": sh("sysctl", "-n", "hw.ncpu"), "memsize_gb": round(int(sh("sysctl", "-n", "hw.memsize")) / 2**30, 1),
            "macos": sh("sw_vers", "-productVersion"), "build": sh("sw_vers", "-buildVersion"),
            "kernel": platform.release(), "python": platform.python_version(), "uptime": sh("uptime")}


# ------------------------------------------------------------------ netcheck
def netcheck(campaign):
    tests = [("cloudflare-25MB", "https://speed.cloudflare.com/__down?bytes=25000000", None),
             ("debian-mirror-100MB-range", "https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/debian-13.7.0-amd64-netinst.iso", "0-104857599")]
    res = []
    for name, url, rng in tests:
        cmd = ["curl", "-sL", "-o", "/dev/null", "-m", "300", "-w", "%{size_download} %{time_total} %{speed_download} %{url_effective}"]
        if rng:
            cmd += ["-r", rng]
        out = subprocess.run(cmd + [url], capture_output=True, text=True).stdout.split()
        if len(out) >= 4:
            res.append({"name": name, "bytes": int(out[0]), "secs": float(out[1]), "Bps": float(out[2]), "final_host": out[3].split("/")[2]})
            log(f"netcheck {name}: {float(out[2]) / 1e6:.1f} MB/s ({float(out[2]) * 8 / 1e6:.0f} Mbit/s) via {res[-1]['final_host']}")
    rec = {"kind": "netcheck", "campaign": campaign, "start": now_iso(), "tests": res,
           "bytes_downloaded": sum(t["bytes"] for t in res), "loadavg": loadavg()}
    append_result(rec)
    return rec


def with_retry(fn, *args, tries=2, **kw):
    """Run a benchmark; on failure (client killed by another process, port clash, ...) clean up and retry.
    The failed attempt stays in results.jsonl (listed under 'Failed runs' in the report)."""
    for attempt in range(1, tries + 1):
        try:
            return fn(*args, **kw)
        except Exception as e:           # KeyboardInterrupt/SystemExit pass through
            log(f"attempt {attempt}/{tries} failed: {e!r}")
            kill_strays()
            if attempt == tries:
                log("giving up on this run")
                return None
            time.sleep(15)


# ------------------------------------------------------------------ 1. real-world download
def run_download(client, tname, rep, campaign, *, seed_secs=120, max_secs=3600, stall_secs=900, speed_cap=None):
    tfile = TORRENT_FILES[tname]
    ad = ADAPTERS[client](client)
    ok, why = ad.available()
    if not ok:
        raise BenchError(f"{client}: {why}")
    stamp = time.strftime("%Y%m%d-%H%M%S")
    run_id = f"{stamp}-dl-{tname}-{client}-r{rep}"
    prof, dl = PROFILES / run_id, DOWNLOADS / run_id
    kill_strays()
    wait_port_free(PEER_PORT)
    rec = {"kind": "download", "campaign": campaign, "client": client, "version": ad.version(), "torrent": tname,
           "rep": rep, "run_id": run_id, "start": now_iso(), "loadavg_start": loadavg(), "status": "error",
           "seed_secs_target": seed_secs}
    rows, t_complete, tc_row = [], None, None
    log(f"=== download {tname} / {client} / rep {rep} (load {rec['loadavg_start']})")
    try:
        rmtree(prof)
        rmtree(dl)
        cfg = LaunchCfg(prof, PEER_PORT, API_PORT[client], "internet")
        t_launch = time.time()
        ad.launch(cfg, [(tfile, dl)])
        t0 = time.time()
        rec["launch_secs"] = round(t0 - t_launch, 2)
        sampler = ProcSampler(ad.pid)
        last_prog, last_done, next_t = t0, 0, t0
        while True:
            next_t = tick(next_t, 1.0)
            st = safe_poll(ad)
            smp = sampler.sample()
            if not smp["alive"] or not ad.alive():
                rec["status"] = "client-died"
                break
            if st is None:
                continue
            row = make_row(t0, smp, st)
            rows.append(row)
            now = smp["t"]
            if st["done_bytes"] > last_done:
                last_done, last_prog = st["done_bytes"], now
            if st["complete"] and t_complete is None:
                t_complete, tc_row = now - t0, row
                log(f"complete in {t_complete:.0f}s; seeding {seed_secs}s")
            if t_complete is not None and now - t0 >= t_complete + seed_secs:
                rec["status"] = "ok"
                break
            if t_complete is None and now - last_prog > stall_secs:
                rec["status"] = "stalled"
                break
            if t_complete is None and now - t0 > max_secs:
                rec["status"] = "timeout"
                break
            if len(rows) % 60 == 0:
                log(f"  t={row['t']:.0f}s {row['done_bytes'] / max(1, row['total_bytes']) * 100:.1f}% "
                    f"{row['down_rate'] / 1e6:.1f}MB/s peers={row['peers']} rss={row['rss_kb'] / 1024:.0f}MB")
        t_stop = time.time()
        rec["exit_code"] = ad.stop() if ad.alive() else ad.proc.poll()
        rec["stop_secs"] = round(time.time() - t_stop, 2)
    except BaseException as e:
        rec["error"] = repr(e)
        log(f"run failed: {e!r}")
        raise
    finally:
        ad.kill()
        rmtree(dl)
        if rec["status"] == "ok":
            rmtree(prof)           # keep profile logs of failed runs for diagnosis
        if rows:
            rec["csv"] = write_csv(run_id, rows)
            rec.update(summarize_download(rows, t_complete, tc_row))
        append_result(rec)
    return rec


def summarize_download(rows, t_complete, tc_row):
    last = rows[-1]
    total = max(r["total_bytes"] for r in rows)
    first_byte = next((r["t"] for r in rows if r["done_bytes"] > 0), None)
    half = next((r["t"] for r in rows if total and r["done_bytes"] >= total / 2), None)
    dl_rows = [r for r in rows if t_complete is None or r["t"] <= t_complete]
    steady = [r for r in dl_rows if t_complete and r["t"] >= t_complete / 2]
    seed = [r for r in rows if t_complete is not None and r["t"] > t_complete]
    s = {"total_bytes": total, "bytes_downloaded": last["done_bytes"], "t_complete_s": round(t_complete, 1) if t_complete else None,
         "t_first_byte_s": first_byte, "t_half_s": half, "load1_mean": round(statistics.mean(r["load1"] for r in rows), 2),
         "rss_peak_kb": max(r["rss_kb"] for r in rows), "foot_peak_kb": max(r["foot_kb"] for r in rows),
         "peers_peak": max(r["peers"] for r in dl_rows), "peers_median": med([r["peers"] for r in dl_rows]),
         "reported_down_peak_Bps": max(r["down_rate"] for r in dl_rows),
         "peak_down_10s_Bps": round(moving_peak(dl_rows, "done_bytes", 10)),
         "samples": len(rows)}
    if t_complete:
        s["mean_down_Bps"] = round(total / t_complete)
        s["rss_steady_kb"] = med([r["rss_kb"] for r in steady])
        s["foot_steady_kb"] = med([r["foot_kb"] for r in steady])
        s["cpu_s_at_complete"] = tc_row["cpu_s"]
        s["cpu_s_total"] = last["cpu_s"]
        s["cpu_pct_while_downloading"] = round(100 * tc_row["cpu_s"] / t_complete, 1)
        if seed:
            s["seed_rss_median_kb"] = med([r["rss_kb"] for r in seed])
            s["seed_foot_median_kb"] = med([r["foot_kb"] for r in seed])
            dt = seed[-1]["t"] - seed[0]["t"]
            if dt > 0:
                s["seed_cpu_pct"] = round(100 * (seed[-1]["cpu_s"] - seed[0]["cpu_s"]) / dt, 2)
                s["seed_pkg_wkups_per_s"] = round((seed[-1]["pkg_wkups"] - seed[0]["pkg_wkups"]) / dt, 1)
                s["seed_up_mean_Bps"] = round(statistics.mean(r["up_rate"] for r in seed))
    return s


def download_campaign(clients, plan, campaign, *, budget_gb, resume, seed_secs, max_secs):
    """plan: [(torrent, repeats)]; interleaved A,B,A,B per torrent."""
    done = {(r["client"], r["torrent"], r["rep"]) for r in load_results()
            if r.get("kind") == "download" and r.get("campaign") == campaign and r.get("status") == "ok"}
    for tname, reps in plan:
        info = read_torrent(TORRENT_FILES[tname])[b"info"]
        tsize = info[b"length"] if b"length" in info else sum(f[b"length"] for f in info[b"files"])
        for rep in range(1, reps + 1):
            for client in clients:
                if resume and (client, tname, rep) in done:
                    log(f"skip (done) {tname}/{client}/r{rep}")
                    continue
                used = total_bytes_used(campaign)
                if used + tsize > budget_gb * 1e9:
                    log(f"BUDGET: {used / 1e9:.1f} GB used + {tsize / 1e9:.1f} GB would exceed {budget_gb} GB; stopping")
                    return
                r = with_retry(run_download, client, tname, rep, campaign, seed_secs=seed_secs, max_secs=max_secs)
                if r and r["status"] == "client-died" and r.get("bytes_downloaded", 0) < 0.1 * tsize:
                    log("client died early; retrying once")
                    with_retry(run_download, client, tname, rep, campaign, seed_secs=seed_secs, max_secs=max_secs, tries=1)
                time.sleep(10)          # let sockets/DHT state settle between runs


# ------------------------------------------------------------------ 2. scale / idle
def _wait_ready(ad, sampler, t0, rows, n, timeout, phase="load"):
    next_t = time.time()
    st = None
    while time.time() - t0 < timeout:
        next_t = tick(next_t, 2.0)
        st = safe_poll(ad)
        smp = sampler.sample()
        if not smp["alive"]:
            raise BenchError("client died during load")
        rows.append(make_row(t0, smp, st, {"phase": phase}))
        if st and st["n"] >= n and st["complete"]:
            return st
        if st and st["n_error"]:
            raise BenchError(f"{st['n_error']} torrents in error state")
    raise BenchError(f"not all {n} torrents ready after {timeout}s: {st}")


def _at(rows, t):
    """row closest to time t"""
    return min(rows, key=lambda r: abs(r["t"] - t))


def _rate(a, b, key):
    dt = b["t"] - a["t"]
    return (b[key] - a[key]) / dt if dt > 0 else 0.0


def run_scale(client, n, campaign, *, idle_secs=600, ready_timeout=1800, rep=1):
    ad = ADAPTERS[client](client)
    ok, why = ad.available()
    if not ok:
        raise BenchError(f"{client}: {why}")
    stamp = time.strftime("%Y%m%d-%H%M%S")
    run_id = f"{stamp}-scale-n{n}-{client}-r{rep}"
    prof = PROFILES / run_id
    kill_strays()
    wait_port_free(SEEDER_PORT)
    wait_port_free(TRACKER_PORT)
    tracker = MockTracker(TRACKER_PORT, RUNS / "trackerlogs" / f"{run_id}.jsonl")
    mset, data_dir = ensure_scale_set(max(n, 500) if n <= 500 else n, tracker.announce_url)
    mset = mset[:n]
    rec = {"kind": "scale", "campaign": campaign, "client": client, "version": ad.version(), "n": n, "rep": rep,
           "run_id": run_id, "start": now_iso(), "loadavg_start": loadavg(), "status": "error",
           "data_bytes": sum(m["size"] for m in mset), "idle_secs": idle_secs}
    rows = []
    log(f"=== scale N={n} / {client} (load {rec['loadavg_start']})")
    try:
        rmtree(prof)
        tracker.start()
        cfg = LaunchCfg(prof, SEEDER_PORT, API_PORT[client], "local", iface=default_iface())
        t_launch = time.time()
        ad.launch(cfg, [(m["torrent"], data_dir) for m in mset])
        t0 = time.time()
        sampler = ProcSampler(ad.pid)
        rec["add_secs"] = round(t0 - t_launch, 1)
        _wait_ready(ad, sampler, t0, rows, n, ready_timeout)
        ready_row = rows[-1]
        t_ready_abs = time.time()
        rec["ready_secs"] = ready_row["t"]
        rec["rss_peak_load_kb"] = max(r["rss_kb"] for r in rows)
        rec["foot_peak_load_kb"] = max(r["foot_kb"] for r in rows)
        rec["cpu_s_load"] = ready_row["cpu_s"]
        log(f"all {n} seeding after {ready_row['t']:.0f}s; rss={ready_row['rss_kb'] / 1024:.0f}MB "
            f"peak-load={rec['rss_peak_load_kb'] / 1024:.0f}MB foot={ready_row['foot_kb'] / 1024:.0f}MB")
        tracker.marker("idle-start")
        # ---- idle phase: NO API polling (it would perturb CPU / wake-ups); external sampling only
        base = len(rows)
        next_t = time.time()
        t_idle0 = rows[-1]["t"]
        while time.time() - t_ready_abs < idle_secs:
            next_t = tick(next_t, 2.0)
            smp = sampler.sample()
            if not smp["alive"]:
                raise BenchError("client died while idle")
            rows.append(make_row(t0, smp, None, {"phase": "idle"}))
            if len(rows) % 60 == 0:
                log(f"  idle t+{time.time() - t_ready_abs:.0f}s rss={rows[-1]['rss_kb'] / 1024:.0f}MB cpu={rows[-1]['cpu_s']:.1f}s")
        idle = rows[base - 1:]
        last = idle[-1]
        at120 = _at(idle, t_idle0 + 120)
        for tag, r in (("ready", ready_row), ("2min", at120), ("10min", last)):
            rec[f"rss_{tag}_kb"] = r["rss_kb"]
            rec[f"foot_{tag}_kb"] = r["foot_kb"]
        rec["rss_idle_max_kb"] = max(r["rss_kb"] for r in idle)
        rec["idle_window_s"] = round(last["t"] - t_idle0)
        rec["cpu_pct_idle"] = round(100 * _rate(idle[0], last, "cpu_s"), 3)
        rec["cpu_pct_idle_after2min"] = round(100 * _rate(at120, last, "cpu_s"), 3)
        rec["pkg_wkups_per_s"] = round(_rate(idle[0], last, "pkg_wkups"), 2)
        rec["intr_wkups_per_s"] = round(_rate(idle[0], last, "intr_wkups"), 2)
        rec["pkg_wkups_per_s_after2min"] = round(_rate(at120, last, "pkg_wkups"), 2)
        rec["intr_wkups_per_s_after2min"] = round(_rate(at120, last, "intr_wkups"), 2)
        t_idle_end = time.time()
        # ---- cross-checks after the idle window
        rec["top_idlew"] = top_idlew(ad.pid, 20)
        (RUNS / "footprint").mkdir(parents=True, exist_ok=True)
        footprint_report(ad.pid, RUNS / "footprint" / f"{run_id}.txt")
        st = safe_poll(ad)
        if st:
            rec["final_n_seeding"], rec["final_n"] = st["n_seeding"], st["n"]
        t_stop = time.time()
        rec["exit_code"] = ad.stop()
        rec["stop_secs"] = round(time.time() - t_stop, 2)
        rec["status"] = "ok"
    except BaseException as e:
        rec["error"] = repr(e)
        raise
    finally:
        ad.kill()
        tracker.stop()
        try:
            rec["tracker_setup"] = tracker.announce_stats(t_to=locals().get("t_ready_abs"))
            rec["tracker_idle"] = tracker.announce_stats(t_from=locals().get("t_ready_abs"), t_to=locals().get("t_idle_end"))
        except Exception as e:
            rec["tracker_error"] = repr(e)
        if rows:
            rec["csv"] = write_csv(run_id, rows)
        if rec["status"] == "ok":
            rmtree(prof)
        append_result(rec)
    log(f"N={n} {client}: rss ready/2min/10min = {rec.get('rss_ready_kb', 0) / 1024:.0f}/"
        f"{rec.get('rss_2min_kb', 0) / 1024:.0f}/{rec.get('rss_10min_kb', 0) / 1024:.0f} MB, "
        f"footprint 10min={rec.get('foot_10min_kb', 0) / 1024:.0f} MB, idle cpu={rec.get('cpu_pct_idle')}% "
        f"wkups={rec.get('pkg_wkups_per_s')}/s, announces idle={rec.get('tracker_idle', {}).get('announces')}")
    return rec


# ------------------------------------------------------------------ 3. hot variant
def run_hot(client, n, campaign, *, leecher_client="transmission", n_leechers=3, n_files=10, rounds=10,
            ready_timeout=1800, rep=1, settle_secs=20, round_timeout=240):
    ad = ADAPTERS[client](client)
    ok, why = ad.available()
    if not ok:
        raise BenchError(f"{client}: {why}")
    lad = ADAPTERS[leecher_client]
    ok, why = lad("x").available()
    if not ok:
        raise BenchError(f"leecher {leecher_client}: {why}")
    stamp = time.strftime("%Y%m%d-%H%M%S")
    run_id = f"{stamp}-hot-n{n}-{client}-r{rep}"
    prof, ldl_root = PROFILES / run_id, DOWNLOADS / run_id
    kill_strays()
    wait_port_free(SEEDER_PORT)
    wait_port_free(TRACKER_PORT)
    iface = default_iface()
    ip = lan_ip(iface)
    tracker = MockTracker(TRACKER_PORT, RUNS / "trackerlogs" / f"{run_id}.jsonl", peers=[f"{ip}:{SEEDER_PORT}"])
    mset, data_dir = ensure_scale_set(500, tracker.announce_url)
    mset = mset[:n]
    hot = sorted(mset, key=lambda m: -m["size"])[:n_files]
    rec = {"kind": "hot", "campaign": campaign, "client": client, "version": ad.version(), "n": n, "rep": rep,
           "leecher_client": leecher_client, "leecher_version": lad("x").version(), "n_leechers": n_leechers,
           "n_files": n_files, "rounds": rounds, "run_id": run_id, "start": now_iso(), "loadavg_start": loadavg(),
           "status": "error", "hot_bytes_per_leecher": sum(m["size"] for m in hot), "peer_ip": ip}
    rows, round_stats = [], []
    log(f"=== hot N={n} seeder={client} leechers={n_leechers}x{leecher_client} (load {rec['loadavg_start']})")
    try:
        rmtree(prof)
        rmtree(ldl_root)
        tracker.start()
        cfg = LaunchCfg(prof, SEEDER_PORT, API_PORT[client], "local", iface=iface)
        ad.launch(cfg, [(m["torrent"], data_dir) for m in mset])
        t0 = time.time()
        sampler = ProcSampler(ad.pid)
        _wait_ready(ad, sampler, t0, rows, n, ready_timeout)
        log(f"seeder ready after {rows[-1]['t']:.0f}s")
        # baseline idle (no polling)
        next_t = time.time()
        t_end = time.time() + settle_secs
        while time.time() < t_end:
            next_t = tick(next_t, 1.0)
            rows.append(make_row(t0, sampler.sample(), None, {"phase": "baseline"}))
        rec["rss_baseline_kb"], rec["foot_baseline_kb"] = rows[-1]["rss_kb"], rows[-1]["foot_kb"]
        hot_start_row = rows[-1]
        for rnd in range(1, rounds + 1):
            leechers = []
            try:
                for i in range(n_leechers):
                    la = lad(f"leech{i}")
                    lcfg = LaunchCfg(PROFILES / f"{run_id}-L{i}", 51501 + i, API_PORT[leecher_client] + 10 + i if API_PORT[leecher_client] else 0,
                                     "local", iface=iface)
                    la.launch(lcfg, [(m["torrent"], ldl_root / f"L{i}") for m in hot])
                    leechers.append(la)
                tr0 = time.time()
                next_t = tr0
                done_t = [None] * n_leechers
                first_byte, prev_total, peak_1s, up_peak = None, 0, 0, 0
                last_prog = tr0
                while True:
                    next_t = tick(next_t, 1.0)
                    if not all(la.alive() for la in leechers):
                        raise BenchError("a leecher process died (killed externally?)")
                    sts = [safe_poll(la) for la in leechers]
                    sst = safe_poll(ad)
                    smp = sampler.sample()
                    if not smp["alive"]:
                        raise BenchError("seeder died during hot phase")
                    now = time.time() - tr0
                    tot = sum(s["done_bytes"] for s in sts if s)
                    if tot > 0 and first_byte is None:
                        first_byte = now
                    active = tot > prev_total
                    if active:
                        last_prog = time.time()
                    elif time.time() - last_prog > 60:
                        raise BenchError(f"hot round {rnd}: no leecher progress for 60 s")
                    peak_1s = max(peak_1s, tot - prev_total)
                    prev_total = tot
                    if sst:
                        up_peak = max(up_peak, sst["up_rate"])
                    for i, s in enumerate(sts):
                        if s and s["complete"] and done_t[i] is None:
                            done_t[i] = now
                    rows.append(make_row(t0, smp, sst, {"phase": "hot", "round": rnd, "leech_done_bytes": tot, "active": int(active)}))
                    if all(d is not None for d in done_t):
                        break
                    if now > round_timeout:
                        raise BenchError(f"hot round {rnd} timed out: {[s and s['done_bytes'] for s in sts]}")
                window = max(done_t) - (first_byte or 0)
                round_stats.append({"round": rnd, "total_bytes": tot, "first_byte_s": first_byte, "done_s": max(done_t),
                                    "window_s": window, "Bps_mean": tot / window if window > 0 else None,
                                    "Bps_peak_1s": peak_1s, "seeder_reported_up_peak_Bps": up_peak})
                log(f"  round {rnd}: {tot / 1e6:.0f} MB in {window:.1f}s = {(tot / window / 1e6) if window > 0 else 0:.0f} MB/s "
                    f"(peak1s {peak_1s / 1e6:.0f} MB/s), seeder rss {rows[-1]['rss_kb'] / 1024:.0f}MB")
            finally:
                for la in leechers:
                    try:
                        la.stop(15)
                    except Exception:
                        pass
                    la.kill()
                for i in range(n_leechers):
                    rmtree(PROFILES / f"{run_id}-L{i}")
                rmtree(ldl_root)
        hot_end_row = rows[-1]
        # after: settle and measure memory returned
        next_t = time.time()
        t_end = time.time() + settle_secs
        while time.time() < t_end:
            next_t = tick(next_t, 1.0)
            rows.append(make_row(t0, sampler.sample(), None, {"phase": "after"}))
        hot_rows = [r for r in rows if r.get("phase") == "hot"]
        rec["rss_after_kb"], rec["foot_after_kb"] = rows[-1]["rss_kb"], rows[-1]["foot_kb"]
        rec["rss_hot_peak_kb"] = max(r["rss_kb"] for r in hot_rows)
        rec["foot_hot_peak_kb"] = max(r["foot_kb"] for r in hot_rows)
        rec["rss_hot_median_kb"] = med([r["rss_kb"] for r in hot_rows])
        rec["foot_hot_median_kb"] = med([r["foot_kb"] for r in hot_rows])
        wall = sum(r["window_s"] for r in round_stats)
        rec["rounds_data"] = round_stats
        rec["upload_Bps_mean"] = sum(r["total_bytes"] for r in round_stats) / wall if wall else None
        rec["upload_Bps_mean_median_round"] = med([r["Bps_mean"] for r in round_stats])
        rec["upload_Bps_peak_1s"] = max(r["Bps_peak_1s"] for r in round_stats)
        rec["seeder_reported_up_peak_Bps"] = max(r["seeder_reported_up_peak_Bps"] for r in round_stats)
        rec["bytes_uploaded"] = sum(r["total_bytes"] for r in round_stats)
        # seeder CPU while serving: cpu delta over hot rows only (excludes baseline/after)
        cpu_hot = sum(b["cpu_s"] - a["cpu_s"] for a, b in zip(rows, rows[1:]) if b.get("phase") == "hot")
        t_hot = sum(b["t"] - a["t"] for a, b in zip(rows, rows[1:]) if b.get("phase") == "hot")
        cpu_act = sum(b["cpu_s"] - a["cpu_s"] for a, b in zip(rows, rows[1:]) if b.get("active"))
        t_act = sum(b["t"] - a["t"] for a, b in zip(rows, rows[1:]) if b.get("active"))
        rec["cpu_s_hot"] = round(cpu_hot, 2)
        rec["hot_seconds"] = round(t_hot, 1)
        rec["active_seconds"] = round(t_act, 1)
        rec["cpu_pct_hot"] = round(100 * cpu_act / t_act, 1) if t_act else None     # while bytes are flowing
        rec["cpu_pct_hot_incl_setup"] = round(100 * cpu_hot / t_hot, 1) if t_hot else None
        rec["cpu_s_per_GB_served"] = round(cpu_hot / (rec["bytes_uploaded"] / 1e9), 2) if rec["bytes_uploaded"] else None
        rec["exit_code"] = ad.stop()
        rec["status"] = "ok"
    except BaseException as e:
        rec["error"] = repr(e)
        raise
    finally:
        ad.kill()
        tracker.stop()
        rmtree(ldl_root)
        try:
            rec["tracker"] = tracker.announce_stats()
        except Exception as e:
            rec["tracker_error"] = repr(e)
        if rows:
            rec["csv"] = write_csv(run_id, rows)
        if rec["status"] == "ok" and os.environ.get("BENCH_KEEP_PROFILE") != "1":   # =1 keeps stderr.log (TRS_UTP_TRACE output)
            rmtree(prof)
        append_result(rec)
    log(f"hot N={n} {client}: mean {(rec.get('upload_Bps_mean') or 0) / 1e6:.0f} MB/s, peak {(rec.get('upload_Bps_peak_1s') or 0) / 1e6:.0f} MB/s, "
        f"cpu {rec.get('cpu_pct_hot')}%, rss hot-peak {rec.get('rss_hot_peak_kb', 0) / 1024:.0f}MB")
    return rec
