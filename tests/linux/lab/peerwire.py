#!/usr/bin/env python3 -I
"""Print what each peer said in its BitTorrent handshake and BEP 10 extended handshake, from a pcap.

  peerwire.py capture.pcap [client-ip ...]

For every source address (TCP flows reassembled in capture order; uTP datagrams scanned as raw payload) it prints
the 8 reserved bytes (decoded: BEP 5 DHT, BEP 6 fast, BEP 10 extension protocol, BEP 7 v6 and the MSE/other bits),
the info-hash, the peer id and the decoded extended handshake (`v`, `m`, `reqq`, `p`, `metadata_size`, `yourip`).
Only peers' own words are shown; nothing here modifies traffic.
"""
import re
import struct
import sys
from collections import defaultdict

sys.path.insert(0, __file__.rsplit("/", 1)[0])


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


def packets(path):
    data = open(path, "rb").read()
    end = "<" if data[:4] in (b"\xd4\xc3\xb2\xa1", b"\x4d\x3c\xb2\xa1") else ">"
    link = struct.unpack(end + "I", data[20:24])[0]
    off = 24
    while off + 16 <= len(data):
        _, _, caplen, _ = struct.unpack(end + "IIII", data[off:off + 16])
        off += 16
        pkt = data[off:off + caplen]
        off += caplen
        ip = pkt[14:] if link == 1 else pkt[16:] if link == 113 else pkt[20:] if link == 276 else pkt
        if len(ip) < 20 or ip[0] >> 4 != 4:
            continue
        ihl = (ip[0] & 15) * 4
        proto = ip[9]
        src = ".".join(map(str, ip[12:16]))
        dst = ".".join(map(str, ip[16:20]))
        l4 = ip[ihl:]
        if proto == 6 and len(l4) >= 20:
            sp, dp, seq = struct.unpack(">HHI", l4[:8])
            yield "tcp", src, sp, dst, dp, seq, l4[(l4[12] >> 4) * 4:]
        elif proto == 17 and len(l4) >= 8:
            sp, dp = struct.unpack(">HH", l4[:4])
            yield "udp", src, sp, dst, dp, 0, l4[8:]


RES = {0: [(0x01, "BEP 5 DHT?(byte0 0x01=azureus)")],
       5: [(0x10, "BEP 10 extension protocol"), (0x08, "BEP 9?")],
       7: [(0x01, "BEP 5 DHT"), (0x02, "BEP 6?"), (0x04, "BEP 6 fast"), (0x08, "BEP 7 v6")]}


def describe_reserved(r):
    bits = []
    if r[5] & 0x10:
        bits.append("BEP 10 extension protocol")
    if r[7] & 0x01:
        bits.append("BEP 5 DHT")
    if r[7] & 0x04:
        bits.append("BEP 6 fast extension")
    if r[7] & 0x08:
        bits.append("BEP 7 IPv6 / v2 hybrid?")
    if r[7] & 0x10:
        bits.append("(bit 0x10 of last byte)")
    other = [i for i, b in enumerate(r) if b and not (i == 5 and b == 0x10) and not (i == 7 and b & 0x1D == b)]
    return bits, other


def scan(stream, label, seen):
    i = stream.find(b"\x13BitTorrent protocol")
    if i < 0 or len(stream) < i + 68:
        return
    reserved, ih, pid = stream[i + 20:i + 28], stream[i + 28:i + 48], stream[i + 48:i + 68]
    key = (label, ih)
    if key in seen:
        return
    seen.add(key)
    bits, other = describe_reserved(reserved)
    print(f"{label}")
    print(f"  handshake: reserved={reserved.hex()}  [{', '.join(bits) or 'none'}]{'  other nonzero bytes at ' + str(other) if other else ''}")
    print(f"             info_hash={ih.hex()}  peer_id={pid!r}")
    rest = stream[i + 68:]
    # messages: <len:4><id:1>...; the extended handshake is id 20, ext id 0
    j = 0
    while j + 5 <= len(rest) and j < 4096:
        n = struct.unpack(">I", rest[j:j + 4])[0]
        if n == 0:
            j += 4
            continue
        if n > 1 << 20 or j + 4 + n > len(rest):
            break
        mid = rest[j + 4]
        if mid == 20 and rest[j + 5] == 0:
            try:
                d, _ = bdecode(rest[j + 6:j + 4 + n])
            except Exception as e:  # noqa: BLE001
                print("  extended handshake: undecodable", e)
                break
            show = {}
            for k, v in d.items():
                k = k.decode("latin-1")
                if k == "m":
                    show["m"] = {kk.decode("latin-1"): vv for kk, vv in v.items()}
                elif k == "yourip":
                    show[k] = ".".join(str(b) for b in v) if len(v) == 4 else v.hex()
                elif isinstance(v, bytes):
                    show[k] = v.decode("latin-1")
                else:
                    show[k] = v
            print(f"  extended handshake (BEP 10): {show}")
            return
        j += 4 + n
    print("  (no extended handshake seen in the first bytes)")


def main():
    path = sys.argv[1]
    want = set(sys.argv[2:])
    flows = defaultdict(list)
    seen = set()
    for proto, src, sp, dst, dp, seq, payload in packets(path):
        if not payload:
            continue
        if want and src not in want:
            continue
        if proto == "tcp":
            flows[(src, sp, dst, dp)].append((seq, payload))
        else:
            if b"BitTorrent protocol" in payload or b"1:v" in payload:
                pass
    for (src, sp, dst, dp), segs in sorted(flows.items()):
        segs.sort()
        buf = b""
        nxt = None
        for seq, p in segs:
            if nxt is None or seq == nxt:
                buf += p
                nxt = seq + len(p)
            elif seq > nxt:
                break
        scan(buf, f"{src}:{sp} -> {dst}:{dp} (tcp)", seen)


if __name__ == "__main__":
    main()
