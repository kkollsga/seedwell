#!/usr/bin/env python3 -I
"""Turn a tracker-side pcap into tracker-probe capture records (JSONL), so `tracker-probe check/analyze`
can judge what a REAL tracker (opentracker, chihaya, aquatic) actually received.

  pcap2jsonl.py capture.pcap TRACKER_IP:PORT label [INTERVAL [MIN_INTERVAL]] > out.jsonl
  env LABELS='198.51.100.20=seedwell,198.51.100.21=transmission' labels records by client address instead of `label`

* HTTP: every `GET /announce?...` / `/scrape?...` request in a TCP segment to TRACKER_IP:PORT becomes a
  record with the raw query string and the request headers in order (as `tracker-probe proxy` logs them).
* UDP (BEP 15): every announce packet (action 1) and scrape (action 2) to TRACKER_IP:PORT becomes a record
  whose `query` is synthesised in a fixed order from the packet fields (info_hash/peer_id percent-encoded),
  with `proto: udp` in `headers` so reports can tell them apart. Connect packets are not records.
INTERVAL / MIN_INTERVAL: what the tracker is configured to answer (requests only are decoded, so the
response the detector needs for its interval rules is attached synthetically; take the values from a curl
of the tracker).  Only requests are decoded; the client address is the IPv4 source address.
"""
import json
import struct
import sys
from urllib.parse import quote_from_bytes, unquote_to_bytes

EVENTS = {0: None, 1: "completed", 2: "started", 3: "stopped"}


def packets(path):
    with open(path, "rb") as f:
        data = f.read()
    magic = data[:4]
    if magic in (b"\xd4\xc3\xb2\xa1", b"\x4d\x3c\xb2\xa1"):
        end = "<"
    elif magic in (b"\xa1\xb2\xc3\xd4", b"\xa1\xb2\x3c\x4d"):
        end = ">"
    else:
        raise SystemExit("not a pcap file (pcapng is not supported)")
    nano = magic in (b"\x4d\x3c\xb2\xa1", b"\xa1\xb2\x3c\x4d")
    link = struct.unpack(end + "I", data[20:24])[0]
    off = 24
    while off + 16 <= len(data):
        sec, frac, caplen, _ = struct.unpack(end + "IIII", data[off:off + 16])
        off += 16
        pkt = data[off:off + caplen]
        off += caplen
        ts_ms = sec * 1000 + (frac // 1_000_000 if nano else frac // 1000)
        yield ts_ms, link, pkt


def ip_payload(link, pkt):
    if link == 1:
        if len(pkt) < 14 or pkt[12:14] != b"\x08\x00":
            return None
        return pkt[14:]
    if link == 113:
        if len(pkt) < 16 or pkt[14:16] != b"\x08\x00":
            return None
        return pkt[16:]
    if link == 276:
        if len(pkt) < 20 or pkt[0:2] != b"\x08\x00":
            return None
        return pkt[20:]
    if link == 101:
        return pkt
    return None


INTERVALS = {}
import os
LABELS = dict(kv.split('=', 1) for kv in os.environ.get('LABELS', '').split(',') if '=' in kv)


def fix_params(rec):
    """Fill `dec` (hex for info_hash/peer_id, decoded text otherwise) and the hex info_hash/peer_id like the proxy does."""
    for p in rec["params"]:
        raw = unquote_to_bytes(p["v"])
        if p["k"] in ("info_hash", "peer_id"):
            p["dec"] = raw.hex()
            rec[p["k"]] = raw.hex()
        else:
            p["dec"] = raw.decode("latin-1")
    if INTERVALS.get("i") and rec["kind"] == "announce":
        rec["upstream_status"] = 200
        rec["response"] = {"interval": INTERVALS["i"]}
        if INTERVALS.get("m"):
            rec["response"]["min_interval"] = INTERVALS["m"]
    return rec


def main():
    path, target, label = sys.argv[1:4]
    interval = int(sys.argv[4]) if len(sys.argv) > 4 else None
    min_interval = int(sys.argv[5]) if len(sys.argv) > 5 else None
    INTERVALS["i"], INTERVALS["m"] = interval, min_interval
    tip, tport = target.rsplit(":", 1)
    tport = int(tport)
    out = sys.stdout
    for ts_ms, link, pkt in packets(path):
        ip = ip_payload(link, pkt)
        if not ip or len(ip) < 20 or ip[0] >> 4 != 4:
            continue
        ihl = (ip[0] & 0xF) * 4
        proto = ip[9]
        src = ".".join(map(str, ip[12:16]))
        dst = ".".join(map(str, ip[16:20]))
        if dst != tip:
            continue
        l4 = ip[ihl:]
        if proto == 6 and len(l4) >= 20:
            sport, dport = struct.unpack(">HH", l4[:4])
            if dport != tport:
                continue
            body = l4[(l4[12] >> 4) * 4:]
            if not (body.startswith(b"GET /") and b"\r\n\r\n" in body):
                continue
            head = body.split(b"\r\n\r\n", 1)[0].decode("latin-1")
            lines = head.split("\r\n")
            parts = lines[0].split(" ")
            target_path = parts[1]
            path_only, _, query = target_path.partition("?")
            headers = [tuple(h.split(": ", 1)) for h in lines[1:] if ": " in h]
            kind = "scrape" if path_only.endswith("/scrape") else "announce" if path_only.endswith("/announce") else "other"
            rec = {"kind": kind, "ts_ms": ts_ms, "label": LABELS.get(src, label), "remote": f"{src}:{sport}", "local": f"{dst}:{dport}",
                   "id": "tracker", "upstream": "tracker", "method": "GET", "path": path_only, "query": query,
                   "params": [], "headers": [list(h) for h in headers]}
            params = []
            for kv in query.split("&"):
                k, _, v = kv.partition("=")
                params.append({"k": k, "v": v})
            rec["params"] = params
            out.write(json.dumps(fix_params(rec)) + "\n")
        elif proto == 17 and len(l4) >= 8:
            sport, dport = struct.unpack(">HH", l4[:4])
            if dport != tport:
                continue
            u = l4[8:]
            if len(u) < 16:
                continue
            action = struct.unpack(">I", u[8:12])[0]
            if action == 1 and len(u) >= 98:
                ih, pid = u[16:36], u[36:56]
                down, left, up, ev, ipv4, key, numwant, port = struct.unpack(">QQQIIIiH", u[56:98])
                q = f"info_hash={quote_from_bytes(ih)}&peer_id={quote_from_bytes(pid)}&port={port}&uploaded={up}&downloaded={down}&left={left}&numwant={numwant}&key={key:08X}&compact=1"
                if EVENTS.get(ev):
                    q += f"&event={EVENTS[ev]}"
                if ipv4:
                    q += "&ip=" + ".".join(str(b) for b in struct.pack(">I", ipv4))
                kind = "announce"
            elif action == 2 and len(u) >= 36:
                q = "info_hash=" + quote_from_bytes(u[16:36])
                kind = "scrape"
            else:
                continue
            params = [{"k": kv.partition("=")[0], "v": kv.partition("=")[2]} for kv in q.split("&")]
            rec = {"kind": kind, "ts_ms": ts_ms, "label": LABELS.get(src, label), "remote": f"{src}:{sport}", "local": f"{dst}:{dport}",
                   "id": "tracker", "upstream": "tracker", "method": "UDP", "path": "/announce" if kind == "announce" else "/scrape",
                   "query": q, "params": params, "headers": [["proto", "udp"]]}
            out.write(json.dumps(fix_params(rec)) + "\n")


if __name__ == "__main__":
    main()
