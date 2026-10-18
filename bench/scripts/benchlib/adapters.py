"""Pluggable client adapters: Transmission, qBittorrent, TorrentRS (external binary).

An adapter owns one client *process* with an isolated profile directory.  The runners only use:

    a = ADAPTERS[name](label)
    a.available()                      -> (bool, reason)
    a.version()                        -> str
    a.launch(cfg, [(torrent, dl_dir)]) -> starts the process and adds the torrents (does not wait for checks)
    a.pid                              -> root pid (for the resource sampler)
    a.poll()                           -> Stats dict (aggregated over all torrents)
    a.stop()                           -> graceful shutdown, returns exit code (None if it had to be killed)
    a.kill()                           -> hard kill (cleanup)

To add a client: subclass `Adapter`, implement the methods, and register it in ADAPTERS.
"""
import dataclasses
import json
import os
import re
import signal
import subprocess
import threading
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path

from .util import QBT_DEFAULT, BenchError, kill_group, log, spawn, wait_exit


@dataclasses.dataclass
class LaunchCfg:
    profile_dir: Path
    listen_port: int
    api_port: int = 0
    mode: str = "internet"            # "internet": DHT/PEX/LSD/port-mapping on.  "local": loopback tracker only
    iface: str | None = None          # local mode: bind interface (single-homed, avoids duplicate announces)
    peer_limit_global: int = 200
    peer_limit_torrent: int = 100
    up_limit_kbps: int = 0            # 0 = unlimited


def empty_stats():
    return {"n": 0, "n_seeding": 0, "n_downloading": 0, "n_checking": 0, "n_error": 0,
            "done_bytes": 0, "total_bytes": 0, "down_rate": 0, "up_rate": 0, "peers": 0,
            "state": "idle", "complete": False}


def finish_stats(s):
    if s["n_checking"]:
        s["state"] = "checking"
    elif s["n_error"]:
        s["state"] = "error"
    elif s["n"] and s["n_seeding"] == s["n"]:
        s["state"] = "seeding"
    elif s["n"]:
        s["state"] = "downloading"
    s["complete"] = bool(s["n"] and s["n_seeding"] == s["n"] and s["n_checking"] == 0)
    return s


def _http(url, data=None, headers=None, timeout=30, method=None):
    req = urllib.request.Request(url, data=data, headers=headers or {}, method=method)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read()


class Adapter:
    name = "?"

    def __init__(self, label=""):
        self.label = label or self.name
        self.proc = None
        self.cfg = None

    @property
    def pid(self):
        return self.proc.pid if self.proc else None

    def available(self):
        return True, ""

    def version(self):
        return "?"

    def launch(self, cfg, torrents):
        raise NotImplementedError

    def poll(self):
        raise NotImplementedError

    def stop(self, timeout=30):
        raise NotImplementedError

    def kill(self):
        if self.proc and self.proc.poll() is None:
            kill_group(self.proc)
            wait_exit(self.proc, 5)

    def alive(self):
        return bool(self.proc and self.proc.poll() is None)

    def _open_logs(self, cfg):
        cfg.profile_dir.mkdir(parents=True, exist_ok=True)
        return open(cfg.profile_dir / "stdout.log", "ab")


# =============================================================================== Transmission
class Transmission(Adapter):
    """transmission-daemon (libtransmission 4.x, same engine as Transmission.app) driven over JSON-RPC.
    Own config dir via -g: ~/Library/Preferences/org.m0k.transmission.plist is never read or written."""
    name = "transmission"

    def __init__(self, label=""):
        super().__init__(label)
        self.daemon = os.environ.get("TR_DAEMON") or _which("transmission-daemon")
        self.sid = ""
        self.rpc = ""

    def available(self):
        return (True, "") if self.daemon else (False, "transmission-daemon not found (brew install transmission-cli)")

    def version(self):
        r = subprocess.run([self.daemon, "--version"], capture_output=True, text=True)
        m = re.search(r"(\d+\.\d+\.\d+)", r.stdout + r.stderr)
        return "transmission-daemon " + (m.group(1) if m else "?")

    def _settings(self, cfg, dl):
        local = cfg.mode == "local"
        s = {
            "alt-speed-enabled": False, "blocklist-enabled": False, "dht-enabled": not local,
            "download-dir": str(dl), "download-queue-enabled": False, "encryption": 1,
            "idle-seeding-limit-enabled": False, "incomplete-dir-enabled": False, "lpd-enabled": not local,
            "message-level": 1, "peer-limit-global": cfg.peer_limit_global,
            "peer-limit-per-torrent": cfg.peer_limit_torrent, "peer-port": cfg.listen_port,
            "peer-port-random-on-start": False, "pex-enabled": not local,
            "port-forwarding-enabled": not local, "queue-stalled-enabled": False,
            "ratio-limit-enabled": False, "rpc-authentication-required": False,
            "rpc-bind-address": "127.0.0.1", "rpc-enabled": True, "rpc-host-whitelist-enabled": False,
            "rpc-port": cfg.api_port, "rpc-whitelist": "127.0.0.1", "rpc-whitelist-enabled": True,
            "script-torrent-done-enabled": False, "seed-queue-enabled": False,
            "speed-limit-down-enabled": False, "speed-limit-up-enabled": cfg.up_limit_kbps > 0,
            "speed-limit-up": cfg.up_limit_kbps or 100, "start-added-torrents": True,
            "trash-original-torrent-files": False, "umask": "022", "utp-enabled": True,
        }
        (cfg.profile_dir / "settings.json").write_text(json.dumps(s, indent=2))

    def _call(self, method, args=None, timeout=60):
        body = json.dumps({"method": method, "arguments": args or {}}).encode()
        for _ in range(2):
            try:
                out = _http(self.rpc, body, {"X-Transmission-Session-Id": self.sid}, timeout)
                return json.loads(out)
            except urllib.error.HTTPError as e:
                if e.code == 409:
                    self.sid = e.headers.get("X-Transmission-Session-Id", "")
                    continue
                raise
        raise BenchError("transmission rpc: session id handshake failed")

    def launch(self, cfg, torrents):
        self.cfg = cfg
        self.rpc = f"http://127.0.0.1:{cfg.api_port}/transmission/rpc"
        dl0 = torrents[0][1] if torrents else cfg.profile_dir / "dl"
        Path(dl0).mkdir(parents=True, exist_ok=True)
        cfg.profile_dir.mkdir(parents=True, exist_ok=True)
        self._settings(cfg, dl0)
        out = self._open_logs(cfg)
        self.proc = spawn([self.daemon, "--foreground", "--config-dir", str(cfg.profile_dir), "--no-auth",
                           "--log-level=" + os.environ.get("BENCH_TR_LOGLEVEL", "error"), "--logfile", str(cfg.profile_dir / "daemon.log")],
                          stdout=out, stderr=out)
        for _ in range(120):
            if self.proc.poll() is not None:
                raise BenchError(f"transmission-daemon exited early, see {cfg.profile_dir}/stdout.log")
            try:
                self._call("session-get", {"fields": ["version"]}, 3)
                break
            except Exception:
                time.sleep(0.5)
        else:
            raise BenchError("transmission rpc did not come up")
        for tpath, dl in torrents:
            Path(dl).mkdir(parents=True, exist_ok=True)
            r = self._call("torrent-add", {"filename": str(tpath), "download-dir": str(dl), "paused": False})
            if r.get("result") != "success":
                raise BenchError(f"torrent-add {tpath}: {r.get('result')}")

    def poll(self):
        r = self._call("torrent-get", {"fields": ["status", "sizeWhenDone", "leftUntilDone", "rateDownload",
                                                   "rateUpload", "peersConnected", "error"]})
        s = empty_stats()
        for t in r["arguments"]["torrents"]:
            s["n"] += 1
            s["total_bytes"] += t["sizeWhenDone"]
            s["done_bytes"] += t["sizeWhenDone"] - t["leftUntilDone"]
            s["down_rate"] += t["rateDownload"]
            s["up_rate"] += t["rateUpload"]
            s["peers"] += t["peersConnected"]
            st = t["status"]
            if st in (1, 2):
                s["n_checking"] += 1
            elif t["error"] in (1, 2, 3) and st == 0:
                s["n_error"] += 1
            elif t["leftUntilDone"] == 0 and st in (5, 6):
                s["n_seeding"] += 1
            else:
                s["n_downloading"] += 1
        return finish_stats(s)

    def stop(self, timeout=30):
        try:
            self._call("session-close", timeout=10)
        except Exception:
            self.proc.send_signal(signal.SIGTERM)
        rc = wait_exit(self.proc, timeout)
        if rc is None:
            log("transmission did not exit gracefully; killing")
            self.kill()
        return rc


# =============================================================================== qBittorrent
class QBittorrent(Adapter):
    """qbittorrent-nox 5.x (libtorrent 2.0) driven through the WebUI API (localhost, no auth)."""
    name = "qbittorrent"

    def __init__(self, label=""):
        super().__init__(label)
        self.bin = os.environ.get("QBT", str(QBT_DEFAULT))
        self.api = ""

    def available(self):
        if os.access(self.bin, os.X_OK):
            return True, ""
        return False, f"qbittorrent-nox not found at {self.bin} (set QBT=/path/to/qbittorrent-nox, qBittorrent 5.2.3)"

    def version(self):
        r = subprocess.run([self.bin, "--version"], capture_output=True, text=True)
        m = re.search(r"v?(\d+\.\d+\.\d+)", r.stdout + r.stderr)
        return "qbittorrent-nox " + (m.group(1) if m else "?")

    def _ini(self, cfg, dl):
        local = cfg.mode == "local"
        c = cfg.profile_dir / "qBittorrent" / "config"
        c.mkdir(parents=True, exist_ok=True)
        b = lambda v: str(v).lower()
        lines = [
            "[LegalNotice]", "Accepted=true", "", "[BitTorrent]",
            f"Session\\DHTEnabled={b(not local)}", f"Session\\PeXEnabled={b(not local)}",
            f"Session\\LSDEnabled={b(not local)}", f"Session\\Port={cfg.listen_port}",
            "Session\\UseRandomPort=false", "Session\\QueueingSystemEnabled=false",
            f"Session\\DefaultSavePath={dl}", "Session\\GlobalDLSpeedLimit=0",
            f"Session\\GlobalUPSpeedLimit={cfg.up_limit_kbps}",
            f"Session\\MaxConnections={cfg.peer_limit_global}",
            f"Session\\MaxConnectionsPerTorrent={cfg.peer_limit_torrent}",
            "Session\\AddTorrentStopped=false",
        ]
        if local:
            # libtorrent refuses loopback trackers unless SSRF mitigation is off; binding to the
            # default-route interface mimics a single-homed machine (no duplicate announces from lo0).
            lines.append("Session\\SSRFMitigation=false")
            lines.append("Session\\MultiConnectionsPerIp=true")      # all local leechers share the seeder's IP
            if cfg.iface:
                lines += [f"Session\\Interface={cfg.iface}", f"Session\\InterfaceName={cfg.iface}"]
        lines += ["", "[Network]", f"PortForwardingEnabled={b(not local)}", "",
                  "[Preferences]", "WebUI\\Address=127.0.0.1", f"WebUI\\Port={cfg.api_port}",
                  "WebUI\\LocalHostAuth=false", "WebUI\\CSRFProtection=false",
                  "WebUI\\HostHeaderValidation=false", "WebUI\\ClickjackingProtection=false",
                  "General\\UpdateCheck=false", ""]
        (c / "qBittorrent.ini").write_text("\n".join(lines))

    def _get(self, path, timeout=60):
        return json.loads(_http(self.api + path, timeout=timeout))

    def _add_files(self, files, dl):
        boundary = uuid.uuid4().hex
        parts = []

        def field(k, v):
            parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'.encode())
        field("savepath", str(dl))
        field("stopped", "false")
        field("paused", "false")
        for f in files:
            parts.append((f'--{boundary}\r\nContent-Disposition: form-data; name="torrents"; filename="{Path(f).name}"\r\n'
                          'Content-Type: application/x-bittorrent\r\n\r\n').encode() + Path(f).read_bytes() + b"\r\n")
        parts.append(f"--{boundary}--\r\n".encode())
        out = _http(self.api + "/torrents/add", b"".join(parts),
                    {"Content-Type": f"multipart/form-data; boundary={boundary}"}, 120)
        if b"Fails" in out:
            raise BenchError(f"qbittorrent add failed: {out[:100]!r}")

    def launch(self, cfg, torrents):
        self.cfg = cfg
        self.api = f"http://127.0.0.1:{cfg.api_port}/api/v2"
        dl0 = torrents[0][1] if torrents else cfg.profile_dir / "dl"
        Path(dl0).mkdir(parents=True, exist_ok=True)
        self._ini(cfg, dl0)
        out = self._open_logs(cfg)
        self.proc = spawn([self.bin, f"--profile={cfg.profile_dir}", f"--webui-port={cfg.api_port}",
                           "--confirm-legal-notice"], stdout=out, stderr=out)
        for _ in range(160):
            if self.proc.poll() is not None:
                raise BenchError(f"qbittorrent-nox exited early, see {cfg.profile_dir}/stdout.log")
            try:
                _http(self.api + "/app/version", timeout=3)
                break
            except Exception:
                time.sleep(0.5)
        else:
            raise BenchError("qbittorrent WebUI did not come up")
        groups = {}
        for t, dl in torrents:
            groups.setdefault(str(dl), []).append(t)
        for dl, files in groups.items():
            Path(dl).mkdir(parents=True, exist_ok=True)
            for i in range(0, len(files), 25):
                self._add_files(files[i:i + 25], dl)

    def poll(self):
        s = empty_stats()
        for t in self._get("/torrents/info"):
            s["n"] += 1
            s["total_bytes"] += t["size"]
            s["done_bytes"] += t["size"] - t["amount_left"]
            s["down_rate"] += t["dlspeed"]
            s["up_rate"] += t["upspeed"]
            s["peers"] += t.get("num_seeds", 0) + t.get("num_leechs", 0)
            st = t["state"]
            if st.startswith("checking"):
                s["n_checking"] += 1
            elif st in ("error", "missingFiles"):
                s["n_error"] += 1
            elif t["amount_left"] == 0 and not st.startswith(("stopped", "paused")):
                s["n_seeding"] += 1
            else:
                s["n_downloading"] += 1
        return finish_stats(s)

    def stop(self, timeout=30):
        try:
            _http(self.api + "/app/shutdown", b"", timeout=10, method="POST")
        except Exception:
            self.proc.send_signal(signal.SIGTERM)
        rc = wait_exit(self.proc, timeout)
        if rc is None:
            log("qbittorrent-nox did not exit gracefully; killing")
            self.kill()
        return rc


# =============================================================================== TorrentRS
class TorrentRS(Adapter):
    """External TorrentRS bench binary; contract documented in bench/README.md ("TorrentRS adapter contract").

        $TRS_BENCH_BIN add FILE.torrent [--add FILE.torrent ...] --download-dir DIR --state-dir DIR
                        --listen-port P [--no-dht --no-pex --no-lsd --no-portmap]
    prints one JSON line per second on stdout and keeps running (seeding) until SIGTERM."""
    name = "torrentrs"

    def __init__(self, label=""):
        super().__init__(label)
        self.bin = os.environ.get("TRS_BENCH_BIN", "")
        self.last = None
        self.lines = 0
        self._thr = None
        self.n_added = 0

    def available(self):
        if not self.bin:
            return False, "TRS_BENCH_BIN not set"
        if not os.access(self.bin, os.X_OK):
            return False, f"TRS_BENCH_BIN={self.bin} is not executable"
        return True, ""

    def version(self):
        r = subprocess.run([self.bin, "--version"], capture_output=True, text=True)
        return (r.stdout.strip() or r.stderr.strip() or "torrentrs (no --version)").splitlines()[0]

    def launch(self, cfg, torrents):
        self.cfg = cfg
        if not torrents:
            raise BenchError("torrentrs adapter needs at least one torrent at launch")
        dirs = {str(d) for _, d in torrents}
        if len(dirs) != 1:
            raise BenchError("torrentrs adapter: all torrents of one process share one --download-dir")
        dl = dirs.pop()
        Path(dl).mkdir(parents=True, exist_ok=True)
        cfg.profile_dir.mkdir(parents=True, exist_ok=True)
        cmd = [self.bin, "add", str(torrents[0][0])]
        for t, _ in torrents[1:]:
            cmd += ["--add", str(t)]
        cmd += ["--download-dir", dl, "--state-dir", str(cfg.profile_dir), "--listen-port", str(cfg.listen_port)]
        if cfg.mode == "local":
            cmd += ["--no-dht", "--no-pex", "--no-lsd", "--no-portmap"]
            # Seedwell's defaults (uTP and encryption on) are what ships; BENCH_TRS_NO_UTP=1 turns uTP off
            # for a TCP-only comparison.
            if os.environ.get("BENCH_TRS_NO_UTP") == "1":
                cmd += ["--no-utp"]
        self.n_added = len(torrents)
        err = open(cfg.profile_dir / "stderr.log", "ab")
        self.proc = spawn(cmd, stdout=subprocess.PIPE, stderr=err)
        self._thr = threading.Thread(target=self._reader, daemon=True)
        self._thr.start()

    def _reader(self):
        for raw in self.proc.stdout:
            try:
                self.last = json.loads(raw)
                self.lines += 1
            except ValueError:
                pass

    def poll(self):
        s = empty_stats()
        j = self.last
        if not j:
            s["n"] = self.n_added
            s["state"] = "starting"
            return s
        n = j.get("torrents", self.n_added)
        s["n"] = n
        s["done_bytes"] = j.get("done_bytes", 0)
        s["total_bytes"] = j.get("total_bytes", 0)
        s["down_rate"] = j.get("down_rate", 0)
        s["up_rate"] = j.get("up_rate", 0)
        s["peers"] = j.get("peers", 0)
        st = j.get("state", "")
        if "seeding" in j:
            s["n_seeding"] = j["seeding"]
            s["n_checking"] = j.get("checking", 0)
        elif st == "seeding":
            s["n_seeding"] = n
        elif st == "checking":
            s["n_checking"] = n
        elif st == "error":
            s["n_error"] = n
        s["n_downloading"] = max(0, n - s["n_seeding"] - s["n_checking"] - s["n_error"])
        return finish_stats(s)

    def stop(self, timeout=30):
        self.proc.send_signal(signal.SIGTERM)
        rc = wait_exit(self.proc, timeout)
        if rc is None:
            log("torrentrs did not exit within the timeout after SIGTERM; killing")
            self.kill()
        return rc


def _which(cmd):
    for d in os.environ.get("PATH", "").split(":") + ["/opt/homebrew/bin", "/usr/local/bin"]:
        p = os.path.join(d, cmd)
        if os.access(p, os.X_OK):
            return p
    return None


ADAPTERS = {"transmission": Transmission, "qbittorrent": QBittorrent, "torrentrs": TorrentRS}
