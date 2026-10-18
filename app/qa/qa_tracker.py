#!/usr/bin/env python3
"""A tiny local HTTP tracker for Seedwell QA (loopback only).

    qa_tracker.py --port 7101 [--mode ok|unregistered|down|slow] [--interval 1800] [--log FILE]

ok: valid empty peer list. unregistered: `failure reason: Unregistered torrent` (the Dead group).
down: answers 503. Counts announces per hour into the log (one line per request)."""
import argparse, sys, time
from urllib.parse import urlsplit, parse_qs
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def benc(v):
    if isinstance(v, int):
        return b"i%de" % v
    if isinstance(v, bytes):
        return b"%d:%s" % (len(v), v)
    if isinstance(v, str):
        return benc(v.encode())
    if isinstance(v, dict):
        return b"d" + b"".join(benc(k) + benc(v[k]) for k in sorted(v)) + b"e"
    raise TypeError


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--mode", default="ok")
    ap.add_argument("--interval", type=int, default=1800)
    ap.add_argument("--log", default="")
    a = ap.parse_args()
    log = open(a.log, "a", buffering=1) if a.log else sys.stderr

    class H(BaseHTTPRequestHandler):
        def do_GET(self):
            q = parse_qs(urlsplit(self.path).query)
            ev = ",".join(f"{k}={q[k][0]}" for k in ("event", "left", "downloaded") if k in q)  # never the passkey
            log.write(f"{time.time():.0f} {a.port} {self.path[:60]} {ev}\n")
            if a.mode == "down":
                self.send_response(503)
                self.send_header("Retry-After", "600")
                self.end_headers()
                return
            if a.mode == "unregistered":
                body = benc({"failure reason": "Unregistered torrent"})
            else:
                body = benc({"interval": a.interval, "min interval": min(a.interval, 900), "complete": 0, "incomplete": 0, "peers": b""})
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    ThreadingHTTPServer(("127.0.0.1", a.port), H).serve_forever()


if __name__ == "__main__":
    main()
