"""Paths, logging, process supervision and per-process resource sampling."""
import atexit
import ctypes
import ctypes.util
import json
import os
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

BENCH = Path(__file__).resolve().parents[2]            # .../bench
REPO = BENCH.parent
RUNS = BENCH / "runs"                                   # gitignored
PROFILES = RUNS / "profiles"                            # isolated client profiles (never ~/Library)
DOWNLOADS = BENCH / "downloads"                         # gitignored, wiped after each run
TORRENTS = BENCH / "torrents"
RESULTS = RUNS / "results.jsonl"                        # one JSON record per benchmark run
PROBE = Path(os.environ.get("PROBE", REPO / "target" / "release" / "tracker-probe"))
QBT_DEFAULT = Path(shutil.which("qbittorrent-nox") or "qbittorrent-nox")  # or set QBT=/path/to/qbittorrent-nox


def log(msg):
    print(time.strftime("[%H:%M:%S] ") + str(msg), file=sys.stderr, flush=True)


class BenchError(Exception):
    pass


# ---------------------------------------------------------------- process supervision
_children = {}          # pid -> Popen
_lock = threading.Lock()


def spawn(cmd, *, stdout=None, stderr=None, env=None, cwd=None):
    """Start a child in its own session/process group so cleanup can kill the whole tree."""
    p = subprocess.Popen(cmd, stdout=stdout, stderr=stderr, stdin=subprocess.DEVNULL, env=env,
                         cwd=cwd, start_new_session=True)
    with _lock:
        _children[p.pid] = p
    return p


def kill_group(p, sig=signal.SIGKILL):
    try:
        os.killpg(p.pid, sig)
    except (ProcessLookupError, PermissionError):
        pass
    except OSError:
        try:
            p.send_signal(sig)
        except Exception:
            pass


def kill_all():
    with _lock:
        procs = list(_children.values())
    for p in procs:
        if p.poll() is None:
            kill_group(p, signal.SIGTERM)
    deadline = time.time() + 3
    for p in procs:
        while p.poll() is None and time.time() < deadline:
            time.sleep(0.05)
        if p.poll() is None:
            kill_group(p, signal.SIGKILL)
        try:
            p.wait(timeout=2)
        except Exception:
            pass


def _on_signal(signum, _frame):
    # Raise in the main thread so `finally:` blocks (client kill, download-dir deletion) run;
    # atexit then kills anything still alive.
    log(f"signal {signum}: aborting run, cleaning up")
    kill_all()
    raise KeyboardInterrupt


def install_cleanup():
    atexit.register(kill_all)
    for s in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(s, _on_signal)


def wait_exit(p, timeout):
    try:
        return p.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        return None


def stray_bench_processes():
    """PIDs of processes whose command line points into our isolated profile dir (leftovers)."""
    out = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True).stdout
    mine = os.getpid()
    res = []
    for line in out.splitlines():
        pid_s, _, cmd = line.strip().partition(" ")
        if str(PROFILES) in cmd and int(pid_s) != mine:
            res.append((int(pid_s), cmd))
    return res


def kill_strays():
    for pid, cmd in stray_bench_processes():
        log(f"killing stray bench process {pid}: {cmd[:100]}")
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass


# ---------------------------------------------------------------- misc helpers
def rmtree(p):
    shutil.rmtree(p, ignore_errors=True)


def free_port_check(port):
    s = socket.socket()
    try:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("0.0.0.0", port))
        return True
    except OSError:
        return False
    finally:
        s.close()


def wait_port_free(port, timeout=20):
    end = time.time() + timeout
    while time.time() < end:
        if free_port_check(port):
            return
        time.sleep(0.5)
    raise BenchError(f"port {port} still in use (stray process?)")


def default_iface():
    out = subprocess.run(["route", "-n", "get", "default"], capture_output=True, text=True).stdout
    for line in out.splitlines():
        if "interface:" in line:
            return line.split()[-1]
    return None


def lan_ip(iface=None):
    iface = iface or default_iface()
    if iface:
        r = subprocess.run(["ipconfig", "getifaddr", iface], capture_output=True, text=True)
        if r.stdout.strip():
            return r.stdout.strip()
        # `ipconfig getifaddr` needs the configd mach service, which is missing in some sandboxed
        # sessions; `ifconfig` works everywhere.
        for line in subprocess.run(["ifconfig", iface], capture_output=True, text=True).stdout.splitlines():
            parts = line.split()
            if len(parts) >= 2 and parts[0] == "inet":
                return parts[1]
    return "127.0.0.1"


def now_iso():
    return time.strftime("%Y-%m-%dT%H:%M:%S%z")


def loadavg():
    return [round(x, 2) for x in os.getloadavg()]


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


CMDLINE = ""


def append_result(rec):
    RUNS.mkdir(parents=True, exist_ok=True)
    rec.setdefault("cmd", CMDLINE)
    with open(RESULTS, "a") as f:
        f.write(json.dumps(rec, sort_keys=True) + "\n")


def load_results():
    if not RESULTS.exists():
        return []
    out = []
    for line in RESULTS.read_text().splitlines():
        line = line.strip()
        if line:
            out.append(json.loads(line))
    return out


# ---------------------------------------------------------------- resource sampling
_libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)


class _TB(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_tb = _TB()
_libc.mach_timebase_info(ctypes.byref(_tb))
_NS = _tb.numer / _tb.denom / 1e9      # mach absolute time -> seconds


def rusage(pid):
    """proc_pid_rusage(RUSAGE_INFO_V4) for a same-user process.

    Returns cpu seconds (user+sys), idle wake-ups, interrupt wake-ups, phys_footprint (the number
    Activity Monitor / `footprint` show) - the same counters `powermetrics` uses for wake-ups,
    but available without sudo. None if the process is gone."""
    buf = (ctypes.c_uint64 * 64)()
    if _libc.proc_pid_rusage(pid, 4, ctypes.byref(buf)) != 0:
        return None
    # rusage_info_v4: uuid[16], user_time, system_time, pkg_idle_wkups, interrupt_wkups,
    #                 pageins, wired, resident, phys_footprint, ...
    return {"user": buf[2] * _NS, "sys": buf[3] * _NS, "pkg_idle_wkups": buf[4],
            "intr_wkups": buf[5], "resident": buf[8], "footprint": buf[9]}


def ps_table():
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,rss="], capture_output=True, text=True).stdout
    tab = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 3:
            tab[int(parts[0])] = (int(parts[1]), int(parts[2]))
    return tab


class ProcSampler:
    """Samples a process tree: `ps -o rss` (resident, KiB), phys_footprint, CPU seconds and wake-ups.

    CPU/wake-up counters of children that exited are retained so totals never go backwards."""

    def __init__(self, root_pid):
        self.root = root_pid
        self.last = {}                      # pid -> (cpu, pkg, intr)
        self.dead = [0.0, 0, 0]

    def tree(self):
        tab = ps_table()
        pids = {self.root} if self.root in tab else set()
        changed = True
        while changed:
            changed = False
            for pid, (ppid, _) in tab.items():
                if ppid in pids and pid not in pids:
                    pids.add(pid)
                    changed = True
        return pids, tab

    def sample(self):
        pids, tab = self.tree()
        rss = sum(tab[p][1] for p in pids)
        foot = 0
        cur = {}
        for p in pids:
            r = rusage(p)
            if r:
                foot += r["footprint"]
                cur[p] = (r["user"] + r["sys"], r["pkg_idle_wkups"], r["intr_wkups"])
        for p, v in self.last.items():
            if p not in cur:
                for i in range(3):
                    self.dead[i] += v[i]
        self.last = cur
        cpu = self.dead[0] + sum(v[0] for v in cur.values())
        pkg = self.dead[1] + sum(v[1] for v in cur.values())
        intr = self.dead[2] + sum(v[2] for v in cur.values())
        return {"t": time.time(), "rss_kb": rss, "foot_kb": foot // 1024, "cpu_s": cpu,
                "pkg_wkups": pkg, "intr_wkups": intr, "nproc": len(pids), "alive": bool(pids)}


def top_idlew(pid, secs=20):
    """`top -l 2 -s secs -pid` second sample: (cpu%, idlew, power) or None. Cross-check only."""
    r = subprocess.run(["top", "-l", "2", "-s", str(secs), "-pid", str(pid), "-stats", "pid,cpu,idlew,power"],
                       capture_output=True, text=True)
    rows = [l.split() for l in r.stdout.splitlines() if l.strip().startswith(str(pid) + " ")]
    if len(rows) >= 2:
        try:
            num = lambda x: float(x.rstrip("+-"))
            return {"cpu_pct": num(rows[-1][1]), "idlew_in_window": num(rows[-1][2]), "window_s": secs,
                    "idlew_per_s": round(num(rows[-1][2]) / secs, 2), "power": num(rows[-1][3])}
        except ValueError:
            return None
    return None


def footprint_report(pid, outfile):
    r = subprocess.run(["footprint", "-p", str(pid)], capture_output=True, text=True)
    Path(outfile).write_text(r.stdout + r.stderr)
