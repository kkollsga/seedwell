#!/usr/bin/env python3 -I
"""Local swarm lab (run on the Mac; everything happens in containers on the docker network `trslab`).

  swarm.py up                      create the network, start trackers and clients
  swarm.py matrix [--trackers ot,ch,aq] [--protos http,udp] [--size-mb 24]
                                   for every (tracker, protocol) cell: Seedwell seeds -> Transmission + qBittorrent
                                   leech, Transmission seeds -> Seedwell leeches, qBittorrent seeds -> Seedwell leeches;
                                   verifies sha256 of every copy
  swarm.py transports              TCP/uTP interop (iptables counters tell which transport carried the bytes)
  swarm.py collect OUTDIR          tracker-side pcaps -> JSONL -> tracker-probe check/analyze, stats, logs
  swarm.py down

Tracker addresses are RFC 5737 documentation addresses (198.51.100.0/24) on purpose: Seedwell's production
destination policy refuses loopback and private-network trackers.  Needs the images trs-lab and trs-trackers
(see lab/Dockerfile and lab/Dockerfile.trackers).
"""
import hashlib
import json
import os
import subprocess
import sys
import time

NET, SUBNET = "trslab", "198.51.100.0/24"
NODES = {  # name: (image, ip)
    "ot": ("trs-lab", "198.51.100.10"),
    "ch": ("trs-trackers", "198.51.100.11"),
    "aq": ("trs-trackers", "198.51.100.12"),
    "sw": ("trs-lab", "198.51.100.20"),
    "tr": ("trs-lab", "198.51.100.21"),
    "qb": ("trs-lab", "198.51.100.22"),
}
SW_PORT, TR_PORT, QB_PORT = 51500, 51413, 51600
TRACKER_URL = {  # (tracker, proto): announce URL
    ("ot", "http"): "http://198.51.100.10:6969/announce", ("ot", "udp"): "udp://198.51.100.10:6969/announce",
    ("ch", "http"): "http://198.51.100.11:6969/announce", ("ch", "udp"): "udp://198.51.100.11:6969/announce",
    ("aq", "http"): "http://198.51.100.12:3000/announce", ("aq", "udp"): "udp://198.51.100.12:3001/announce",
}
SW_ENV = "export HOME=/tmp/home-sw TRS_FORBID_REAL_HOME=1; mkdir -p $HOME; "
SW_STATE = ["/data/sw/state"]  # a fresh state dir per client (re)start: Seedwell's crash-loop guard holds trackers after 3 launches in 10 min


def swc():
    return SW_ENV + f"seedwell --socket {SW_STATE[0]}/control.sock "

TRC = "transmission-remote 127.0.0.1:9091 "
QBC = "curl -s http://127.0.0.1:8080"
RESULTS = []


def run(args, check=True, timeout=300, inp=None):
    r = subprocess.run(args, capture_output=True, text=True, timeout=timeout, input=inp)
    if check and r.returncode != 0:
        raise RuntimeError(f"{' '.join(args)[:200]} -> {r.returncode}\n{r.stdout[-800:]}\n{r.stderr[-800:]}")
    return r.stdout + (r.stderr if not check else "")


def ex(node, cmd, check=True, timeout=300):
    return run(["docker", "exec", f"lab-{node}", "bash", "-c", cmd], check=check, timeout=timeout).strip()


def exd(node, cmd):
    run(["docker", "exec", "-d", f"lab-{node}", "bash", "-c", cmd])


def log(*a):
    print(time.strftime("[%H:%M:%S]"), *a, flush=True)


# ---- bencode (just enough to compute an info-hash) -----------------------------------------------
def bdecode(b, i=0):
    c = b[i:i + 1]
    if c == b"i":
        j = b.index(b"e", i)
        return int(b[i + 1:j]), j + 1
    if c == b"l":
        i += 1
        out = []
        while b[i:i + 1] != b"e":
            v, i = bdecode(b, i)
            out.append(v)
        return out, i + 1
    if c == b"d":
        i += 1
        out = {}
        while b[i:i + 1] != b"e":
            k, i = bdecode(b, i)
            v, i = bdecode(b, i)
            out[k] = v
        return out, i + 1
    j = b.index(b":", i)
    n = int(b[i:j])
    return b[j + 1:j + 1 + n], j + 1 + n


def info_hash(path):
    b = open(path, "rb").read()
    # locate the raw info dictionary
    i = b.index(b"4:info") + 6
    _, j = bdecode(b, i)
    return hashlib.sha1(b[i:j]).hexdigest()


# ---- lifecycle ------------------------------------------------------------------------------------
def up():
    if run(["docker", "network", "ls", "-q", "--filter", f"name=^{NET}$"]).strip() == "":
        run(["docker", "network", "create", "--subnet", SUBNET, NET])
    for n, (img, ip) in NODES.items():
        run(["docker", "rm", "-f", f"lab-{n}"], check=False)
        run(["docker", "run", "-d", "--name", f"lab-{n}", "--network", NET, "--ip", ip, "--cap-add", "NET_ADMIN",
             "--cap-add", "NET_RAW", "--sysctl", "net.ipv6.conf.all.disable_ipv6=1",
             *(["--security-opt", "seccomp=unconfined"] if n == "aq" else []),  # aquatic_http needs io_uring (glommio)
             img, "sleep", "infinity"])
        ex(n, "mkdir -p /data /out")
    # ---- trackers
    ex("ot", "touch /tmp/whitelist.txt; chmod 644 /tmp/whitelist.txt; printf 'listen.tcp_udp 0.0.0.0:6969\\ntracker.rootdir /\\naccess.stats 127.0.0.1\\naccess.whitelist /tmp/whitelist.txt\\n' > /tmp/ot.conf")
    exd("ot", "opentracker -f /tmp/ot.conf > /out/ot.log 2>&1")
    ch = """chihaya:
  announce_interval: "30m"
  min_announce_interval: "15m"
  metrics_addr: "0.0.0.0:6880"
  http: {addr: "0.0.0.0:6969", read_timeout: "5s", write_timeout: "5s", enable_keepalive: true, idle_timeout: "30s", announce_routes: ["/announce"], scrape_routes: ["/scrape"], max_numwant: 100, default_numwant: 50, max_scrape_infohashes: 50}
  udp: {addr: "0.0.0.0:6969", max_clock_skew: "10s", private_key: "seedwell-lab-not-a-secret", max_numwant: 100, default_numwant: 50, max_scrape_infohashes: 50}
  storage: {name: "memory", config: {gc_interval: "3m", peer_lifetime: "31m", shard_count: 1024, prometheus_reporting_interval: "1s"}}
"""
    run(["docker", "exec", "-i", "lab-ch", "bash", "-c", "cat > /tmp/ch.yaml"], inp=ch)
    exd("ch", "chihaya --config /tmp/ch.yaml > /out/ch.log 2>&1")
    http = 'socket_workers = 1\nswarm_workers = 1\nlog_level = "info"\n[network]\naddress = "0.0.0.0:3000"\nkeep_alive = true\n[protocol]\nmax_peers = 50\npeer_announce_interval = 120\n[cleaning]\ntorrent_cleaning_interval = 30\nconnection_cleaning_interval = 60\nmax_peer_age = 1800\nmax_connection_idle = 180\n[privileges]\ndrop_privileges = false\n[access_list]\nmode = "off"\npath = "./access-list.txt"\n[metrics]\nrun_prometheus_endpoint = true\nprometheus_endpoint_address = "0.0.0.0:9000"\ntorrent_count_update_interval = 10\n'
    udp = 'socket_workers = 1\nlog_level = "info"\n[network]\naddress = "0.0.0.0:3001"\n[protocol]\nmax_response_peers = 50\npeer_announce_interval = 900\n[statistics]\ninterval = 10\nprint_to_stdout = true\n[cleaning]\ntorrent_cleaning_interval = 30\nmax_connection_age = 60\nmax_peer_age = 1200\n[privileges]\ndrop_privileges = false\n'
    run(["docker", "exec", "-i", "lab-aq", "bash", "-c", "cat > /tmp/aqh.toml"], inp=http)
    run(["docker", "exec", "-i", "lab-aq", "bash", "-c", "cat > /tmp/aqu.toml"], inp=udp)
    exd("aq", "cd /tmp && aquatic_http -c /tmp/aqh.toml > /out/aq-http.log 2>&1")
    exd("aq", "cd /tmp && aquatic_udp -c /tmp/aqu.toml > /out/aq-udp.log 2>&1")
    for n, ports in (("ot", "port 6969"), ("ch", "port 6969"), ("aq", "port 3000 or port 3001")):
        exd(n, f"tcpdump -i eth0 -n -s0 -U -w /out/{n}.pcap '{ports}' 2>/out/{n}.tcpdump.err")
    time.sleep(2)
    # ---- clients
    for n in ("sw", "tr", "qb"):
        for chain, d in (("INPUT", "-s"), ("OUTPUT", "-d")):
            for proto in ("tcp", "udp"):  # byte counters for peer traffic (peers are the three client IPs)
                for peer in ("198.51.100.20", "198.51.100.21", "198.51.100.22"):
                    ex(n, f"iptables -A {chain} -p {proto} {d} {peer}")
    start_clients(utp_sw=True, tr_utp=True, qb_proto="TCP_AND_UTP")
    time.sleep(2)
    log("lab is up")


def start_clients(utp_sw, tr_utp, qb_proto):
    ex("sw", "pkill -x seedwell; while pgrep -x seedwell >/dev/null; do sleep 0.5; done; : > /out/sw.out", check=False)
    SW_STATE[0] = f"/data/sw/state-{int(time.time())}"
    ex("sw", f"echo {SW_STATE[0]} > /data/sw-current")
    ex("sw", f"mkdir -p {SW_STATE[0]} && printf '[network]\\npex = false\\nlsd = false\\nutp = {str(utp_sw).lower()}\\nipv6 = false\\n' > {SW_STATE[0]}/config.toml")
    exd("sw", SW_ENV + f"seedwell daemon --state-dir {SW_STATE[0]} --download-dir /data/sw/dl --listen-port {SW_PORT} --no-dht --permission full --max-active 100 --shutdown-secs 30 >> /out/sw.out 2>> /out/sw.err")
    ex("tr", "pkill -x transmission-da; while pgrep -x transmission-da >/dev/null; do sleep 0.5; done", check=False)
    settings = {"dht-enabled": False, "pex-enabled": False, "lpd-enabled": False, "utp-enabled": tr_utp, "preferred_transport": "utp" if tr_utp else "tcp",
                "peer-port": TR_PORT, "peer-port-random-on-start": False, "rpc-bind-address": "127.0.0.1", "rpc-authentication-required": False,
                "rpc-host-whitelist-enabled": False, "rpc-whitelist-enabled": False, "download-dir": "/data/tr/dl", "incomplete-dir-enabled": False,
                "download-queue-enabled": False, "seed-queue-enabled": False, "queue-stalled-enabled": False, "ratio-limit-enabled": False,
                "idle-seeding-limit-enabled": False, "encryption": 1, "port-forwarding-enabled": False, "message-level": 2, "umask": "022"}
    ex("tr", "mkdir -p /data/tr/cfg /data/tr/dl /data/tr/seed")
    run(["docker", "exec", "-i", "lab-tr", "bash", "-c", "cat > /data/tr/cfg/settings.json"], inp=json.dumps(settings))
    exd("tr", "transmission-daemon -g /data/tr/cfg -f --no-auth --log-level=debug --logfile /out/tr.log > /out/tr.out 2>&1")
    ex("qb", "pkill -x qbittorrent-nox; while pgrep -x qbittorrent-nox >/dev/null; do sleep 0.5; done", check=False)
    conf = f"""[LegalNotice]
Accepted=true

[BitTorrent]
Session\\Port={QB_PORT}
Session\\DefaultSavePath=/data/qb/dl
Session\\DHTEnabled=false
Session\\PeXEnabled=false
Session\\LSDEnabled=false
Session\\BTProtocol={qb_proto}
Session\\QueueingSystemEnabled=false
Session\\UseUPnP=false
Session\\GlobalMaxRatio=-1
Session\\Encryption=0

[Preferences]
WebUI\\Port=8080
WebUI\\LocalHostAuth=false
WebUI\\HostHeaderValidation=false
WebUI\\CSRFProtection=false
WebUI\\Address=127.0.0.1
General\\Locale=en
"""
    ex("qb", "mkdir -p /data/qb/qBittorrent/config /data/qb/dl")
    run(["docker", "exec", "-i", "lab-qb", "bash", "-c", "cat > /data/qb/qBittorrent/config/qBittorrent.conf"], inp=conf)
    exd("qb", "qbittorrent-nox --profile=/data/qb --webui-port=8080 > /out/qb.out 2>&1")
    for i in range(30):
        ok_tr = "VERSION" in ex("tr", TRC + "-si", check=False)
        ok_qb = ex("qb", QBC + "/api/v2/app/version", check=False).startswith("v")
        ok_sw = "ready" in ex("sw", "cat /out/sw.out", check=False)
        if ok_tr and ok_qb and ok_sw:
            return
        time.sleep(1)
    raise RuntimeError(f"clients not up: tr={ok_tr} qb={ok_qb} sw={ok_sw}")


# ---- helpers on clients ---------------------------------------------------------------------------------
def make_data(node, path, mb):
    ex(node, f"mkdir -p $(dirname {path}) && head -c {mb * 1024 * 1024} /dev/urandom > {path} && sha256sum {path} | cut -d' ' -f1")
    return ex(node, f"sha256sum {path} | cut -d' ' -f1")


def make_torrent(node, path, announce, out):
    ex(node, f"transmission-create -o {out} -t '{announce}' -c 'seedwell lab' {path}")
    local = f"/tmp/trs-lab-{os.path.basename(out)}"
    run(["docker", "cp", f"lab-{node}:{out}", local])
    return local


def put(node, local, remote):
    run(["docker", "cp", local, f"lab-{node}:{remote}"])


def add(node, tfile, savepath, seed):
    """Add the .torrent at tfile (inside the container) with its data at savepath (seed) or to download into savepath."""
    if node == "sw":
        ex("sw", swc() + f"add {tfile} --save-path {savepath}")
    elif node == "tr":
        ex("tr", TRC + f"-a {tfile} -w {savepath}")
    elif node == "qb":
        skip = "true" if seed else "false"
        ex("qb", f"{QBC}/api/v2/torrents/add -F torrents=@{tfile} -F savepath={savepath} -F skip_checking={skip} -F autoTMM=false")


def progress(node, ih):
    try:
        if node == "sw":
            j = json.loads(ex("sw", swc() + "list --json"))
            for t in j["transfers"]:
                if t["id"].startswith(ih):
                    return t["progress"]
            return 0.0
        if node == "tr":
            out = ex("tr", TRC + f"-t {ih} -i")
            for line in out.splitlines():
                if "Percent Done" in line:
                    return float(line.split(":")[1].strip().rstrip("%")) / 100
            return 0.0
        if node == "qb":
            j = json.loads(ex("qb", f"{QBC}/api/v2/torrents/info?hashes={ih}"))
            return j[0]["progress"] if j else 0.0
    except Exception:
        return 0.0


def wait_done(node, ih, secs=240):
    t0 = time.time()
    while time.time() - t0 < secs:
        if progress(node, ih) >= 0.9999:
            return time.time() - t0
        time.sleep(1)
    return None


def peer_bytes(node):
    out = ex(node, "iptables -L INPUT -v -n -x; iptables -L OUTPUT -v -n -x")
    res = {"in_tcp": 0, "in_udp": 0, "out_tcp": 0, "out_udp": 0}
    chain = None
    for line in out.splitlines():
        if line.startswith("Chain INPUT"):
            chain = "in"
        elif line.startswith("Chain OUTPUT"):
            chain = "out"
        else:
            p = line.split()
            # columns: pkts bytes [target] prot opt in out source destination (target is blank for counter-only rules)
            if len(p) >= 8 and p[0].isdigit():
                proto = p[2] if p[2] in ("tcp", "udp") else p[3] if len(p) > 3 and p[3] in ("tcp", "udp") else None
                if proto and not (p[2] == "DROP" or p[2] == "ACCEPT"):
                    res[f"{chain}_{proto}"] += int(p[1])
    return res


def find_sha(node, name):
    out = ex(node, f"f=$(find /data/{node} -type f -name '{name}' -not -path '*/.incomplete/*' -size +0 | head -3); for x in $f; do sha256sum $x; done", check=False)
    return [l.split()[0] for l in out.splitlines() if l.strip()]


def scenario(cell, tag, seeder, leechers, mb, timeout=240):
    """seeder holds the data; leechers download from the tracker-provided peers. Returns a result dict."""
    tracker, proto = cell
    name = f"{tag}-{tracker}-{proto}-{int(time.time()) % 100000}.bin"
    seeddir = {"sw": "/data/sw/seed", "tr": "/data/tr/seed", "qb": "/data/qb/seed"}[seeder]
    sha = make_data(seeder, f"{seeddir}/{name}", mb)
    tfile_local = make_torrent(seeder, f"{seeddir}/{name}", TRACKER_URL[cell], f"/tmp/{name}.torrent")
    ih = info_hash(tfile_local)
    if tracker == "ot":  # Debian's opentracker is built with a white list: authorise the info-hash (SIGHUP re-reads it)
        ex("ot", f"echo {ih} >> /tmp/whitelist.txt; pkill -HUP -x opentracker")
        time.sleep(1)
    for n in {seeder, *leechers}:
        put(n, tfile_local, f"/tmp/{name}.torrent")
    t_start = time.time()
    add(seeder, f"/tmp/{name}.torrent", seeddir, True)
    time.sleep(4)  # the seeder announces first: later announcers get it from the tracker
    before = {n: peer_bytes(n) for n in ("sw", "tr", "qb")}
    for n in leechers:
        add(n, f"/tmp/{name}.torrent", {"sw": "/data/sw/dl", "tr": "/data/tr/dl", "qb": "/data/qb/dl"}[n], False)
    res = {"cell": f"{tracker}/{proto}", "scenario": f"{seeder} seeds -> {','.join(leechers)}", "hash": ih[:8], "mb": mb}
    for n in leechers:
        secs = wait_done(n, ih, timeout)
        shas = []
        for _ in range(20):  # Seedwell reports 100 % a moment before the files leave .incomplete
            shas = find_sha(n, name) if secs is not None else []
            if shas:
                break
            time.sleep(1)
        res[n] = {"done_s": None if secs is None else round(secs, 1), "sha_ok": bool(shas) and all(s == sha for s in shas)}
    after = {n: peer_bytes(n) for n in ("sw", "tr", "qb")}
    res["bytes"] = {n: {k: after[n][k] - before[n][k] for k in after[n]} for n in after}
    log(json.dumps({k: v for k, v in res.items() if k != "bytes"}))
    RESULTS.append(res)
    return res


def matrix(trackers, protos, mb):
    for t in trackers:
        for p in protos:
            cell = (t, p)
            scenario(cell, "sws", "sw", ["tr", "qb"], mb)
            scenario(cell, "trs", "tr", ["sw"], mb)
            scenario(cell, "qbs", "qb", ["sw"], mb)
    json.dump(RESULTS, open("/tmp/trs-lab-matrix.json", "w"), indent=1)


def transports(mb):
    """TCP-only, uTP-only, mixed: which transport carried the bytes, per peer pair."""
    out = []
    cell = ("ot", "http")
    # (label, seedwell utp, transmission utp, qb proto, iptables: drop TCP between all clients)
    for label, sw_utp, tr_utp, qb, drop_tcp in (
        ("tcp-only (uTP off everywhere)", False, False, "TCP", False),
        ("utp-only (TCP dropped by iptables, uTP on)", True, True, "UTP", True),
        ("utp preferred, TCP allowed", True, True, "TCP_AND_UTP", False),
        ("sw tcp-only vs qb/tr utp-only: must still talk (sw dials tcp -> expect no transfer if peer is uTP-only)", False, True, "UTP", False),
    ):
        for n in ("sw", "tr", "qb"):
            ex(n, "iptables -F INPUT; iptables -F OUTPUT", check=False)
            for chain, dflag in (("INPUT", "-s"), ("OUTPUT", "-d")):
                for proto in ("tcp", "udp"):
                    for peer in ("198.51.100.20", "198.51.100.21", "198.51.100.22"):
                        ex(n, f"iptables -A {chain} -p {proto} {dflag} {peer}")
            if drop_tcp:  # force uTP: nothing may reach a peer-wire TCP port
                for prt in (SW_PORT, TR_PORT, QB_PORT):
                    ex(n, f"iptables -I INPUT 1 -p tcp --dport {prt} -j DROP")
        start_clients(sw_utp, tr_utp, qb)
        log("transport scenario:", label)
        r1 = scenario(cell, "tp-sws", "sw", ["tr", "qb"], mb, timeout=90)
        r2 = scenario(cell, "tp-trs", "tr", ["sw"], mb, timeout=90)
        r3 = scenario(cell, "tp-qbs", "qb", ["sw"], mb, timeout=90)
        out.append({"label": label, "runs": [r1, r2, r3]})
    json.dump(out, open("/tmp/trs-lab-transports.json", "w"), indent=1)


def handshake(outdir):
    """Capture the peer-wire handshakes (TCP) of all three clients: Seedwell dials Transmission and qBittorrent and is dialled by them."""
    os.makedirs(outdir, exist_ok=True)
    start_clients(False, False, "TCP")
    exd("sw", "tcpdump -i eth0 -n -s0 -U -w /out/peerwire.pcap 'host 198.51.100.21 or host 198.51.100.22' 2>/out/peerwire.err")
    time.sleep(1)
    cell = ("ot", "http")
    scenario(cell, "hs-sws", "sw", ["tr", "qb"], 4, timeout=60)
    scenario(cell, "hs-trs", "tr", ["sw"], 4, timeout=60)
    scenario(cell, "hs-qbs", "qb", ["sw"], 4, timeout=60)
    time.sleep(2)
    ex("sw", "pkill tcpdump", check=False)
    time.sleep(1)
    run(["docker", "cp", "lab-sw:/out/peerwire.pcap", os.path.join(outdir, "peerwire.pcap")])
    json.dump(RESULTS, open(os.path.join(outdir, "handshake-runs.json"), "w"), indent=1)


def collect(outdir):
    """Tracker-side evidence: pcaps (requests as the tracker saw them), logs and the trackers' own counters."""
    os.makedirs(outdir, exist_ok=True)
    for n in ("ot", "ch", "aq"):
        ex(n, "pkill tcpdump", check=False)
    time.sleep(1)
    for n in ("ot", "ch", "aq"):
        for f in ex(n, "ls /out").split():
            run(["docker", "cp", f"lab-{n}:/out/{f}", os.path.join(outdir, f)], check=False)
    open(os.path.join(outdir, "ot-stats.xml"), "w").write(ex("ot", "curl -s 'http://127.0.0.1:6969/stats?mode=everything'; curl -s 'http://127.0.0.1:6969/stats?mode=tcp4'; curl -s 'http://127.0.0.1:6969/stats?mode=udp4'; curl -s 'http://127.0.0.1:6969/stats?mode=scrp'", check=False))
    open(os.path.join(outdir, "ch-metrics.txt"), "w").write(ex("ch", "curl -s http://127.0.0.1:6880/metrics | grep -E '^chihaya_(http|udp)_(announces|scrapes|requests)|^chihaya_storage' ", check=False))
    open(os.path.join(outdir, "aq-metrics.txt"), "w").write(ex("aq", "curl -s http://127.0.0.1:9000/metrics | grep -E 'aquatic_(requests|responses|peers|torrents)' | head -60", check=False))
    for n in ("sw", "tr", "qb"):
        for f in ex(n, "ls /out").split():
            run(["docker", "cp", f"lab-{n}:/out/{f}", os.path.join(outdir, f"{n}-{f}")], check=False)


def down():
    for n in NODES:
        run(["docker", "rm", "-f", f"lab-{n}"], check=False)
    run(["docker", "network", "rm", NET], check=False)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] != "up":
        cur = ex("sw", "cat /data/sw-current", check=False)
        if cur.startswith("/data/sw/"):
            SW_STATE[0] = cur
    cmd = sys.argv[1] if len(sys.argv) > 1 else "help"
    opts = dict(zip(sys.argv[2::2], sys.argv[3::2]))
    if cmd == "up":
        up()
    elif cmd == "reset":  # default client configuration: uTP on everywhere, TCP allowed
        for n in ("sw", "tr", "qb"):
            ex(n, "iptables -F INPUT; iptables -F OUTPUT", check=False)
            for chain, d in (("INPUT", "-s"), ("OUTPUT", "-d")):
                for proto in ("tcp", "udp"):
                    for peer in ("198.51.100.20", "198.51.100.21", "198.51.100.22"):
                        ex(n, f"iptables -A {chain} -p {proto} {d} {peer}")
        start_clients(utp_sw=True, tr_utp=True, qb_proto="TCP_AND_UTP")
    elif cmd == "matrix":
        matrix(opts.get("--trackers", "ot,ch,aq").split(","), opts.get("--protos", "http,udp").split(","), int(opts.get("--size-mb", 24)))
    elif cmd == "transports":
        transports(int(opts.get("--size-mb", 24)))
    elif cmd == "handshake":
        handshake(sys.argv[2])
    elif cmd == "collect":
        collect(sys.argv[2])
    elif cmd == "down":
        down()
    else:
        print(__doc__)
