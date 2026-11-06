#!/usr/bin/env python3
"""A loopback SOCKS5 proxy for QA (RFC 1928 with RFC 1929 username and password): the torrent proxy's stand-in for
NordVPN's servers in an isolated run (the core reads them from <state>/qa/proxy-servers.txt). It answers the
handshake the core tests a key and a server with, on 127.0.0.1 and ::1 at one port; a CONNECT is refused (QA sends no
torrent through it). Never anything but loopback.

    fake_socks5.py <qa-root> <port> <username> <password>

The handshake waits `<qa-root>/socks-delay-v4` (or `-v6`) seconds before it answers, read at each connection, so a
script makes a server slow (over 1.5 s) or fast while it runs; ending the process takes both servers down.
"""
import os
import socket
import socketserver
import sys
import threading
import time

ROOT, PORT, USER, PASS = sys.argv[1], int(sys.argv[2]), sys.argv[3].encode(), sys.argv[4].encode()


def delay(label):
    try:
        with open(os.path.join(ROOT, f"socks-delay-{label}")) as f:
            return max(0.0, float(f.read().strip() or 0))
    except (OSError, ValueError):
        return 0.0


def recv(s, n):
    out = b""
    while len(out) < n:
        chunk = s.recv(n - len(out))
        if not chunk:
            raise OSError("closed")
        out += chunk
    return out


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        s = self.request
        s.settimeout(15)
        try:
            ver, n = recv(s, 2)
            methods = recv(s, n)
            time.sleep(delay(self.server.label))
            if ver != 5 or 2 not in methods:
                s.sendall(b"\x05\xff")
                return
            s.sendall(b"\x05\x02")
            recv(s, 1)
            user = recv(s, recv(s, 1)[0])
            pw = recv(s, recv(s, 1)[0])
            ok = user == USER and pw == PASS
            s.sendall(b"\x01\x00" if ok else b"\x01\x01")
            if ok and s.recv(4):
                s.sendall(b"\x05\x05\x00\x01" + b"\x00" * 6)  # CONNECT: refused
        except (OSError, ValueError):
            pass


class V4(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
    label = "v4"


class V6(V4):
    address_family = socket.AF_INET6
    label = "v6"


servers = [V4(("127.0.0.1", PORT), Handler)]
try:
    servers.append(V6(("::1", PORT), Handler))
except OSError as e:
    print(f"no IPv6 loopback: {e}", file=sys.stderr)
for srv in servers[1:]:
    threading.Thread(target=srv.serve_forever, daemon=True).start()
servers[0].serve_forever()
