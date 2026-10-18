//! KRPC (BEP 5) wire codec: a purpose-built, allocation-free bencode reader and a
//! canonical (sorted-key) writer.
//!
//! Why not `trs-bencode`: KRPC messages are tiny, flat and must be parsed from
//! hostile UDP with zero allocation, tolerant of unknown keys and unsorted dicts.
//! A 300-line streaming reader gives us that without coupling the DHT's hot path
//! to a general-purpose tree decoder.

use crate::id::Id;
use std::net::{Ipv4Addr, SocketAddrV4};

/// Length of one compact node entry (id + IPv4 + port).
pub const NODE_LEN: usize = 26;
/// Length of one compact peer entry (IPv4 + port).
pub const PEER_LEN: usize = 6;
/// Longest transaction id we accept from the wire.
pub const MAX_TID: usize = 16;
/// Longest announce token we remember from other nodes.
pub const MAX_TOKEN: usize = 32;

const MAX_DEPTH: u32 = 8;

/// Why a datagram was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Input ended mid-value.
    Truncated,
    /// Not valid bencode (or too deeply nested).
    Syntax,
    /// A required key is absent.
    Missing(&'static str),
    /// A key is present with the wrong type or an invalid value.
    BadField(&'static str),
}

/// A rejected datagram plus whatever we learned before failing, so the caller can send an error reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rejected<'a> {
    /// What went wrong.
    pub err: ParseError,
    /// Transaction id, if it was read before the failure.
    pub tid: Option<&'a [u8]>,
    /// True if the message claimed to be a query (`y = q`).
    pub was_query: bool,
}

/// A parsed KRPC message borrowing from the datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message<'a> {
    /// Transaction id (opaque, echoed in replies).
    pub tid: &'a [u8],
    /// Client version string (`v`), if present.
    pub version: Option<&'a [u8]>,
    /// BEP 43: sender is read-only and must not be added to routing tables.
    pub read_only: bool,
    /// BEP 42: our address as seen by the sender (responses only).
    pub ip: Option<SocketAddrV4>,
    /// Query, response or error.
    pub body: Body<'a>,
}

/// Message kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Body<'a> {
    /// `y = q`.
    Query(Query<'a>),
    /// `y = r`.
    Response(Response<'a>),
    /// `y = e`.
    Error(ErrorMsg<'a>),
}

/// An incoming query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Query<'a> {
    /// `ping`.
    Ping {
        /// Sender id.
        id: Id,
    },
    /// `find_node`.
    FindNode {
        /// Sender id.
        id: Id,
        /// Id being searched for.
        target: Id,
    },
    /// `get_peers`.
    GetPeers {
        /// Sender id.
        id: Id,
        /// Torrent info hash.
        info_hash: Id,
    },
    /// `announce_peer`.
    AnnouncePeer {
        /// Sender id.
        id: Id,
        /// Torrent info hash.
        info_hash: Id,
        /// Announced TCP/uTP port (ignored if `implied_port`).
        port: u16,
        /// Token we handed out in a `get_peers` response.
        token: &'a [u8],
        /// Use the UDP source port instead of `port`.
        implied_port: bool,
    },
    /// Any method we do not implement (BEP 44 `get`/`put`, `sample_infohashes`, ...).
    Unknown {
        /// The method name.
        method: &'a [u8],
    },
}

/// A response. Its meaning depends on the query it answers, so all optional fields are exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Response<'a> {
    /// Responder id.
    pub id: Id,
    /// `token` (get_peers).
    pub token: Option<&'a [u8]>,
    /// Raw compact IPv4 node list (26 bytes each).
    pub nodes: &'a [u8],
    /// Raw concatenated `values` list items (validated bencode strings).
    values_raw: &'a [u8],
}

impl<'a> Response<'a> {
    /// Iterates compact IPv4 nodes; a trailing partial entry is ignored.
    pub fn nodes(&self) -> impl Iterator<Item = (Id, SocketAddrV4)> + 'a {
        self.nodes.chunks_exact(NODE_LEN).map(|c| {
            let mut id = [0u8; 20];
            id.copy_from_slice(&c[..20]);
            (id, decode_addr(&c[20..26]))
        })
    }

    /// Iterates IPv4 peers from `values`; entries that are not 6 bytes (e.g. IPv6) are skipped.
    pub fn values(&self) -> impl Iterator<Item = SocketAddrV4> + 'a {
        let mut cur = Cur {
            buf: self.values_raw,
            pos: 0,
        };
        std::iter::from_fn(move || {
            while cur.pos < cur.buf.len() {
                let s = cur.bytes().ok()?;
                if s.len() == PEER_LEN {
                    return Some(decode_addr(s));
                }
            }
            None
        })
    }
}

/// An error message (`y = e`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorMsg<'a> {
    /// Numeric code (201 generic, 202 server, 203 protocol, 204 method unknown).
    pub code: i64,
    /// Human-readable text.
    pub message: &'a [u8],
}

/// Decodes a 6-byte compact address. `b` must be exactly 6 bytes.
fn decode_addr(b: &[u8]) -> SocketAddrV4 {
    SocketAddrV4::new(
        Ipv4Addr::new(b[0], b[1], b[2], b[3]),
        u16::from_be_bytes([b[4], b[5]]),
    )
}

/// Appends a 6-byte compact address.
pub fn encode_addr(a: &SocketAddrV4, out: &mut Vec<u8>) {
    out.extend_from_slice(&a.ip().octets());
    out.extend_from_slice(&a.port().to_be_bytes());
}

/// Appends a 26-byte compact node entry.
pub fn encode_node(id: &Id, a: &SocketAddrV4, out: &mut Vec<u8>) {
    out.extend_from_slice(id);
    encode_addr(a, out);
}

// ---------------------------------------------------------------- reader

struct Cur<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn peek(&self) -> Result<u8, ParseError> {
        self.buf.get(self.pos).copied().ok_or(ParseError::Truncated)
    }

    fn expect(&mut self, c: u8) -> Result<(), ParseError> {
        if self.peek()? == c {
            self.pos += 1;
            Ok(())
        } else {
            Err(ParseError::Syntax)
        }
    }

    fn digits(&mut self) -> Result<u64, ParseError> {
        let start = self.pos;
        let mut v: u64 = 0;
        while let Some(&c) = self.buf.get(self.pos) {
            if !c.is_ascii_digit() {
                break;
            }
            v = v
                .checked_mul(10)
                .and_then(|v| v.checked_add((c - b'0') as u64))
                .ok_or(ParseError::Syntax)?;
            self.pos += 1;
        }
        if self.pos == start {
            return Err(if self.pos >= self.buf.len() {
                ParseError::Truncated
            } else {
                ParseError::Syntax
            });
        }
        Ok(v)
    }

    fn int(&mut self) -> Result<i64, ParseError> {
        self.expect(b'i')?;
        let neg = self.peek()? == b'-';
        if neg {
            self.pos += 1;
        }
        let v = self.digits()?;
        self.expect(b'e')?;
        let v = i64::try_from(v).map_err(|_| ParseError::Syntax)?;
        Ok(if neg { -v } else { v })
    }

    fn bytes(&mut self) -> Result<&'a [u8], ParseError> {
        let len = self.digits()?;
        self.expect(b':')?;
        let len = usize::try_from(len).map_err(|_| ParseError::Syntax)?;
        let end = self.pos.checked_add(len).ok_or(ParseError::Syntax)?;
        let s = self.buf.get(self.pos..end).ok_or(ParseError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    /// Skips one value of any type, bounded by `MAX_DEPTH`.
    fn skip(&mut self, depth: u32) -> Result<(), ParseError> {
        if depth > MAX_DEPTH {
            return Err(ParseError::Syntax);
        }
        match self.peek()? {
            b'i' => self.int().map(|_| ()),
            b'0'..=b'9' => self.bytes().map(|_| ()),
            b'l' => {
                self.pos += 1;
                while self.peek()? != b'e' {
                    self.skip(depth + 1)?;
                }
                self.pos += 1;
                Ok(())
            }
            b'd' => {
                self.pos += 1;
                while self.peek()? != b'e' {
                    self.bytes()?;
                    self.skip(depth + 1)?;
                }
                self.pos += 1;
                Ok(())
            }
            _ => Err(ParseError::Syntax),
        }
    }

    fn id(&mut self, field: &'static str) -> Result<Id, ParseError> {
        let b = self.bytes().map_err(|_| ParseError::BadField(field))?;
        <Id>::try_from(b).map_err(|_| ParseError::BadField(field))
    }
}

#[derive(Default)]
struct Args<'a> {
    id: Option<Id>,
    target: Option<Id>,
    info_hash: Option<Id>,
    port: Option<i64>,
    token: Option<&'a [u8]>,
    implied_port: bool,
    nodes: &'a [u8],
    values: Option<&'a [u8]>,
}

fn parse_args<'a>(cur: &mut Cur<'a>) -> Result<Args<'a>, ParseError> {
    cur.expect(b'd')?;
    let mut a = Args::default();
    while cur.peek()? != b'e' {
        let key = cur.bytes()?;
        match key {
            b"id" => a.id = Some(cur.id("id")?),
            b"target" => a.target = Some(cur.id("target")?),
            b"info_hash" => a.info_hash = Some(cur.id("info_hash")?),
            b"port" => a.port = Some(cur.int().map_err(|_| ParseError::BadField("port"))?),
            b"implied_port" => {
                a.implied_port = cur
                    .int()
                    .map_err(|_| ParseError::BadField("implied_port"))?
                    != 0
            }
            b"token" => a.token = Some(cur.bytes().map_err(|_| ParseError::BadField("token"))?),
            b"nodes" => a.nodes = cur.bytes().map_err(|_| ParseError::BadField("nodes"))?,
            b"values" => {
                cur.expect(b'l')
                    .map_err(|_| ParseError::BadField("values"))?;
                let start = cur.pos;
                while cur.peek()? != b'e' {
                    cur.bytes().map_err(|_| ParseError::BadField("values"))?;
                }
                a.values = Some(&cur.buf[start..cur.pos]);
                cur.pos += 1;
            }
            _ => cur.skip(1)?,
        }
    }
    cur.pos += 1;
    Ok(a)
}

/// Parses a KRPC datagram. Never panics; trailing bytes after the top-level dict are ignored.
pub fn parse(buf: &[u8]) -> Result<Message<'_>, Rejected<'_>> {
    let mut tid = None;
    let mut was_query = false;
    parse_inner(buf, &mut tid, &mut was_query).map_err(|err| Rejected {
        err,
        tid,
        was_query,
    })
}

fn parse_inner<'a>(
    buf: &'a [u8],
    tid_out: &mut Option<&'a [u8]>,
    was_query: &mut bool,
) -> Result<Message<'a>, ParseError> {
    let mut cur = Cur { buf, pos: 0 };
    cur.expect(b'd')?;
    let mut y: Option<&[u8]> = None;
    let mut q: Option<&[u8]> = None;
    let mut a: Option<Args> = None;
    let mut r: Option<Args> = None;
    let mut e: Option<ErrorMsg> = None;
    let mut version = None;
    let mut read_only = false;
    let mut ip = None;
    while cur.peek()? != b'e' {
        let key = cur.bytes()?;
        match key {
            b"t" => {
                let t = cur.bytes().map_err(|_| ParseError::BadField("t"))?;
                *tid_out = Some(t);
            }
            b"y" => {
                let v = cur.bytes().map_err(|_| ParseError::BadField("y"))?;
                *was_query = v == b"q";
                y = Some(v);
            }
            b"q" => q = Some(cur.bytes().map_err(|_| ParseError::BadField("q"))?),
            b"a" => a = Some(parse_args(&mut cur)?),
            b"r" => r = Some(parse_args(&mut cur)?),
            b"e" => {
                cur.expect(b'l')?;
                let mut code = 0;
                let mut message: &[u8] = b"";
                let mut i = 0;
                while cur.peek()? != b'e' {
                    match (i, cur.peek()?) {
                        (0, b'i') => code = cur.int()?,
                        (1, b'0'..=b'9') => message = cur.bytes()?,
                        _ => cur.skip(2)?,
                    }
                    i += 1;
                }
                cur.pos += 1;
                e = Some(ErrorMsg { code, message });
            }
            b"v" => {
                if cur.peek()?.is_ascii_digit() {
                    version = Some(cur.bytes()?);
                } else {
                    cur.skip(1)?;
                }
            }
            b"ro" => {
                if cur.peek()? == b'i' {
                    read_only = cur.int()? != 0;
                } else {
                    cur.skip(1)?;
                }
            }
            b"ip" => {
                if cur.peek()?.is_ascii_digit() {
                    let b = cur.bytes()?;
                    if b.len() == PEER_LEN {
                        ip = Some(decode_addr(b));
                    }
                } else {
                    cur.skip(1)?;
                }
            }
            _ => cur.skip(1)?,
        }
    }
    let tid = tid_out.ok_or(ParseError::Missing("t"))?;
    if tid.len() > MAX_TID {
        return Err(ParseError::BadField("t"));
    }
    let body = match y.ok_or(ParseError::Missing("y"))? {
        b"q" => {
            let method = q.ok_or(ParseError::Missing("q"))?;
            let a = a.ok_or(ParseError::Missing("a"))?;
            let id = a.id.ok_or(ParseError::Missing("id"))?;
            Body::Query(match method {
                b"ping" => Query::Ping { id },
                b"find_node" => Query::FindNode {
                    id,
                    target: a.target.ok_or(ParseError::Missing("target"))?,
                },
                b"get_peers" => Query::GetPeers {
                    id,
                    info_hash: a.info_hash.ok_or(ParseError::Missing("info_hash"))?,
                },
                b"announce_peer" => {
                    let info_hash = a.info_hash.ok_or(ParseError::Missing("info_hash"))?;
                    let token = a.token.ok_or(ParseError::Missing("token"))?;
                    let port = match a.port {
                        Some(p) => u16::try_from(p).map_err(|_| ParseError::BadField("port"))?,
                        None if a.implied_port => 0,
                        None => return Err(ParseError::Missing("port")),
                    };
                    Query::AnnouncePeer {
                        id,
                        info_hash,
                        port,
                        token,
                        implied_port: a.implied_port,
                    }
                }
                _ => Query::Unknown { method },
            })
        }
        b"r" => {
            let r = r.ok_or(ParseError::Missing("r"))?;
            Body::Response(Response {
                id: r.id.ok_or(ParseError::Missing("id"))?,
                token: r.token,
                nodes: r.nodes,
                values_raw: r.values.unwrap_or(b""),
            })
        }
        b"e" => Body::Error(e.ok_or(ParseError::Missing("e"))?),
        _ => return Err(ParseError::BadField("y")),
    };
    Ok(Message {
        tid,
        version,
        read_only,
        ip,
        body,
    })
}

// ---------------------------------------------------------------- writer

/// Fields common to every outgoing message.
#[derive(Clone, Copy, Debug)]
pub struct Envelope<'a> {
    /// Transaction id.
    pub tid: &'a [u8],
    /// Version string for `v`, if any.
    pub version: Option<&'a [u8]>,
    /// Emit `ro = 1` (BEP 43, queries only).
    pub read_only: bool,
    /// Emit `ip` (BEP 42, responses only).
    pub ip: Option<SocketAddrV4>,
}

/// An outgoing query.
#[derive(Clone, Copy, Debug)]
pub enum QueryOut<'a> {
    /// `ping`.
    Ping {
        /// Our id.
        id: &'a Id,
    },
    /// `find_node`.
    FindNode {
        /// Our id.
        id: &'a Id,
        /// Search target.
        target: &'a Id,
    },
    /// `get_peers`.
    GetPeers {
        /// Our id.
        id: &'a Id,
        /// Info hash.
        info_hash: &'a Id,
    },
    /// `announce_peer`.
    AnnouncePeer {
        /// Our id.
        id: &'a Id,
        /// Info hash.
        info_hash: &'a Id,
        /// Our listening port.
        port: u16,
        /// Token from the target's `get_peers` response.
        token: &'a [u8],
    },
}

/// An outgoing response.
#[derive(Clone, Copy, Debug)]
pub struct ResponseOut<'a> {
    /// Our id.
    pub id: &'a Id,
    /// Token (get_peers).
    pub token: Option<&'a [u8]>,
    /// Pre-encoded compact nodes (26 bytes each); omitted when empty.
    pub nodes: &'a [u8],
    /// Peers for `values`; omitted when empty.
    pub values: &'a [SocketAddrV4],
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_len(out, b.len());
    out.extend_from_slice(b);
}

fn put_len(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(n.to_string().as_bytes());
    out.push(b':');
}

fn put_key(out: &mut Vec<u8>, k: &str, v: &[u8]) {
    put_bytes(out, k.as_bytes());
    put_bytes(out, v);
}

fn put_int(out: &mut Vec<u8>, k: &str, v: i64) {
    put_bytes(out, k.as_bytes());
    out.push(b'i');
    out.extend_from_slice(v.to_string().as_bytes());
    out.push(b'e');
}

/// Appends a query. Keys are emitted in sorted order, so output is canonical bencode.
pub fn encode_query(out: &mut Vec<u8>, env: &Envelope<'_>, q: &QueryOut<'_>) {
    out.push(b'd');
    put_bytes(out, b"a");
    out.push(b'd');
    let method: &str = match q {
        QueryOut::Ping { id } => {
            put_key(out, "id", *id);
            "ping"
        }
        QueryOut::FindNode { id, target } => {
            put_key(out, "id", *id);
            put_key(out, "target", *target);
            "find_node"
        }
        QueryOut::GetPeers { id, info_hash } => {
            put_key(out, "id", *id);
            put_key(out, "info_hash", *info_hash);
            "get_peers"
        }
        QueryOut::AnnouncePeer {
            id,
            info_hash,
            port,
            token,
        } => {
            put_key(out, "id", *id);
            put_key(out, "info_hash", *info_hash);
            put_int(out, "port", i64::from(*port));
            put_key(out, "token", token);
            "announce_peer"
        }
    };
    out.push(b'e');
    put_key(out, "q", method.as_bytes());
    if env.read_only {
        put_int(out, "ro", 1);
    }
    put_key(out, "t", env.tid);
    if let Some(v) = env.version {
        put_key(out, "v", v);
    }
    put_key(out, "y", b"q");
    out.push(b'e');
}

/// Appends a response.
pub fn encode_response(out: &mut Vec<u8>, env: &Envelope<'_>, r: &ResponseOut<'_>) {
    out.push(b'd');
    if let Some(ip) = &env.ip {
        let mut b = Vec::with_capacity(PEER_LEN);
        encode_addr(ip, &mut b);
        put_key(out, "ip", &b);
    }
    put_bytes(out, b"r");
    out.push(b'd');
    put_key(out, "id", r.id);
    if !r.nodes.is_empty() {
        put_key(out, "nodes", r.nodes);
    }
    if let Some(t) = r.token {
        put_key(out, "token", t);
    }
    if !r.values.is_empty() {
        put_bytes(out, b"values");
        out.push(b'l');
        for v in r.values {
            put_len(out, PEER_LEN);
            encode_addr(v, out);
        }
        out.push(b'e');
    }
    out.push(b'e');
    put_key(out, "t", env.tid);
    if let Some(v) = env.version {
        put_key(out, "v", v);
    }
    put_key(out, "y", b"r");
    out.push(b'e');
}

/// Appends an error message.
pub fn encode_error(out: &mut Vec<u8>, env: &Envelope<'_>, code: i64, message: &str) {
    out.extend_from_slice(b"d1:eli");
    out.extend_from_slice(code.to_string().as_bytes());
    out.push(b'e');
    put_bytes(out, message.as_bytes());
    out.push(b'e');
    put_key(out, "t", env.tid);
    if let Some(v) = env.version {
        put_key(out, "v", v);
    }
    put_key(out, "y", b"e");
    out.push(b'e');
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID_A: &Id = b"abcdefghij0123456789";
    const ID_B: &Id = b"mnopqrstuvwxyz123456";
    const ENV: Envelope<'static> = Envelope {
        tid: b"aa",
        version: None,
        read_only: false,
        ip: None,
    };

    fn q(q: &QueryOut) -> Vec<u8> {
        let mut o = Vec::new();
        encode_query(&mut o, &ENV, q);
        o
    }

    // ---- BEP 5 byte-exact examples

    #[test]
    fn ping_query_vector() {
        let want = b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe";
        assert_eq!(q(&QueryOut::Ping { id: ID_A }), want);
        let m = parse(want).unwrap();
        assert_eq!(m.tid, b"aa");
        assert_eq!(m.body, Body::Query(Query::Ping { id: *ID_A }));
    }

    #[test]
    fn ping_response_vector() {
        let want = b"d1:rd2:id20:mnopqrstuvwxyz123456e1:t2:aa1:y1:re";
        let mut o = Vec::new();
        encode_response(
            &mut o,
            &ENV,
            &ResponseOut {
                id: ID_B,
                token: None,
                nodes: b"",
                values: &[],
            },
        );
        assert_eq!(o, want);
        let m = parse(want).unwrap();
        match m.body {
            Body::Response(r) => assert_eq!(r.id, *ID_B),
            _ => panic!(),
        }
    }

    #[test]
    fn find_node_query_vector() {
        let want = b"d1:ad2:id20:abcdefghij01234567896:target20:mnopqrstuvwxyz123456e1:q9:find_node1:t2:aa1:y1:qe";
        assert_eq!(
            q(&QueryOut::FindNode {
                id: ID_A,
                target: ID_B
            }),
            want
        );
        let m = parse(want).unwrap();
        assert_eq!(
            m.body,
            Body::Query(Query::FindNode {
                id: *ID_A,
                target: *ID_B
            })
        );
    }

    #[test]
    fn find_node_response_vector() {
        // 26 bytes of compact node info: 20 id bytes + 192.168.1.1:6881
        let mut node = Vec::new();
        encode_node(
            b"0123456789abcdefghij",
            &SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 6881),
            &mut node,
        );
        let mut want = b"d1:rd2:id20:abcdefghij01234567895:nodes26:".to_vec();
        want.extend_from_slice(&node);
        want.extend_from_slice(b"e1:t2:aa1:y1:re");
        let mut o = Vec::new();
        encode_response(
            &mut o,
            &ENV,
            &ResponseOut {
                id: ID_A,
                token: None,
                nodes: &node,
                values: &[],
            },
        );
        assert_eq!(o, want);
        let m = parse(&want).unwrap();
        let Body::Response(r) = m.body else { panic!() };
        let n: Vec<_> = r.nodes().collect();
        assert_eq!(n.len(), 1);
        assert_eq!(&n[0].0, b"0123456789abcdefghij");
        assert_eq!(
            n[0].1,
            SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 6881)
        );
    }

    #[test]
    fn get_peers_query_vector() {
        let want = b"d1:ad2:id20:abcdefghij01234567899:info_hash20:mnopqrstuvwxyz123456e1:q9:get_peers1:t2:aa1:y1:qe";
        assert_eq!(
            q(&QueryOut::GetPeers {
                id: ID_A,
                info_hash: ID_B
            }),
            want
        );
        let m = parse(want).unwrap();
        assert_eq!(
            m.body,
            Body::Query(Query::GetPeers {
                id: *ID_A,
                info_hash: *ID_B
            })
        );
    }

    #[test]
    fn get_peers_response_values_vector() {
        // BEP 5 uses 6-byte opaque strings "axje.u" and "idhtnm" as peer values.
        let want = b"d1:rd2:id20:abcdefghij01234567895:token8:aoeusnth6:valuesl6:axje.u6:idhtnmee1:t2:aa1:y1:re";
        let peers = [decode_addr(b"axje.u"), decode_addr(b"idhtnm")];
        let mut o = Vec::new();
        encode_response(
            &mut o,
            &ENV,
            &ResponseOut {
                id: ID_A,
                token: Some(b"aoeusnth"),
                nodes: b"",
                values: &peers,
            },
        );
        assert_eq!(o, want);
        let m = parse(want).unwrap();
        let Body::Response(r) = m.body else { panic!() };
        assert_eq!(r.token, Some(&b"aoeusnth"[..]));
        assert_eq!(r.values().collect::<Vec<_>>(), peers.to_vec());
    }

    #[test]
    fn get_peers_response_nodes_vector() {
        let mut node = Vec::new();
        encode_node(
            b"0123456789abcdefghij",
            &SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 1),
            &mut node,
        );
        let mut want = b"d1:rd2:id20:abcdefghij01234567895:nodes26:".to_vec();
        want.extend_from_slice(&node);
        want.extend_from_slice(b"5:token8:aoeusnthe1:t2:aa1:y1:re");
        let mut o = Vec::new();
        encode_response(
            &mut o,
            &ENV,
            &ResponseOut {
                id: ID_A,
                token: Some(b"aoeusnth"),
                nodes: &node,
                values: &[],
            },
        );
        assert_eq!(o, want);
        let Body::Response(r) = parse(&want).unwrap().body else {
            panic!()
        };
        assert_eq!(r.nodes().count(), 1);
        assert_eq!(r.values().count(), 0);
    }

    #[test]
    fn announce_peer_query_vector() {
        let want = b"d1:ad2:id20:abcdefghij01234567899:info_hash20:mnopqrstuvwxyz1234564:porti6881e5:token8:aoeusnthe1:q13:announce_peer1:t2:aa1:y1:qe";
        let o = q(&QueryOut::AnnouncePeer {
            id: ID_A,
            info_hash: ID_B,
            port: 6881,
            token: b"aoeusnth",
        });
        assert_eq!(o, want);
        let m = parse(want).unwrap();
        assert_eq!(
            m.body,
            Body::Query(Query::AnnouncePeer {
                id: *ID_A,
                info_hash: *ID_B,
                port: 6881,
                token: b"aoeusnth",
                implied_port: false
            })
        );
    }

    #[test]
    fn announce_peer_implied_port_parses() {
        let w = b"d1:ad2:id20:abcdefghij012345678912:implied_porti1e9:info_hash20:mnopqrstuvwxyz1234564:porti1e5:token8:aoeusnthe1:q13:announce_peer1:t2:aa1:y1:qe";
        let m = parse(w).unwrap();
        assert!(matches!(
            m.body,
            Body::Query(Query::AnnouncePeer {
                implied_port: true,
                ..
            })
        ));
    }

    #[test]
    fn error_vector() {
        let want = b"d1:eli201e23:A Generic Error Ocurrede1:t2:aa1:y1:ee";
        let mut o = Vec::new();
        encode_error(&mut o, &ENV, 201, "A Generic Error Ocurred");
        assert_eq!(o, want);
        let m = parse(want).unwrap();
        assert_eq!(
            m.body,
            Body::Error(ErrorMsg {
                code: 201,
                message: b"A Generic Error Ocurred"
            })
        );
    }

    // ---- extensions

    #[test]
    fn read_only_version_and_ip() {
        let env = Envelope {
            tid: b"zz",
            version: Some(b"TD\x00\x01"),
            read_only: true,
            ip: None,
        };
        let mut o = Vec::new();
        encode_query(&mut o, &env, &QueryOut::Ping { id: ID_A });
        assert_eq!(
            &o[..],
            &b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping2:roi1e1:t2:zz1:v4:TD\x00\x011:y1:qe"[..]
        );
        let m = parse(&o).unwrap();
        assert!(m.read_only);
        assert_eq!(m.version, Some(&b"TD\x00\x01"[..]));

        let env = Envelope {
            tid: b"zz",
            version: None,
            read_only: false,
            ip: Some(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 80)),
        };
        let mut o = Vec::new();
        encode_response(
            &mut o,
            &env,
            &ResponseOut {
                id: ID_A,
                token: None,
                nodes: b"",
                values: &[],
            },
        );
        let m = parse(&o).unwrap();
        assert_eq!(m.ip, Some(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 80)));
    }

    #[test]
    fn unsorted_keys_and_unknown_keys_tolerated() {
        let w = b"d1:y1:q1:t2:aa4:zzzzl3:fooi5ee1:q4:ping1:ad2:id20:abcdefghij01234567893:xxxi1eee";
        let m = parse(w).unwrap();
        assert_eq!(m.body, Body::Query(Query::Ping { id: *ID_A }));
    }

    #[test]
    fn unknown_method() {
        let w = b"d1:ad2:id20:abcdefghij0123456789e1:q3:get1:t2:aa1:y1:qe";
        let m = parse(w).unwrap();
        assert_eq!(m.body, Body::Query(Query::Unknown { method: b"get" }));
    }

    #[test]
    fn ipv6_values_skipped() {
        let mut w = b"d1:rd2:id20:abcdefghij01234567895:token1:x6:valuesl18:".to_vec();
        w.extend_from_slice(&[0u8; 18]);
        w.extend_from_slice(b"6:axje.ue e1:t2:aa1:y1:re");
        w.retain(|b| *b != b' ');
        let m = parse(&w).unwrap();
        let Body::Response(r) = m.body else { panic!() };
        assert_eq!(r.values().count(), 1);
    }

    // ---- rejection

    #[test]
    fn malformed_inputs_rejected_with_context() {
        // missing target keeps tid and query flag so caller can answer 203
        let r =
            parse(b"d1:ad2:id20:abcdefghij0123456789e1:q9:find_node1:t2:aa1:y1:qe").unwrap_err();
        assert_eq!(r.err, ParseError::Missing("target"));
        assert_eq!(r.tid, Some(&b"aa"[..]));
        assert!(r.was_query);
        // wrong id length
        let r = parse(b"d1:ad2:id3:abce1:q4:ping1:t2:aa1:y1:qe").unwrap_err();
        assert_eq!(r.err, ParseError::BadField("id"));
        // port out of range
        let r = parse(b"d1:ad2:id20:abcdefghij01234567899:info_hash20:mnopqrstuvwxyz1234564:porti70000e5:token1:xe1:q13:announce_peer1:t2:aa1:y1:qe").unwrap_err();
        assert_eq!(r.err, ParseError::BadField("port"));
        // no transaction id
        assert!(parse(b"d1:y1:qe").is_err());
        // not a dict / empty / truncated
        assert_eq!(parse(b"").unwrap_err().err, ParseError::Truncated);
        assert!(parse(b"i5e").is_err());
        assert!(parse(b"d1:ad2:id20:abcdefghij01234").is_err());
        // huge length prefix must not allocate or overflow
        assert!(parse(b"d1:t99999999999999999999:").is_err());
        assert!(parse(b"d1:t18446744073709551615:x").is_err());
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let mut w = b"d1:z".to_vec();
        w.extend(std::iter::repeat_n(b'l', 5000));
        assert_eq!(parse(&w).unwrap_err().err, ParseError::Syntax);
    }

    #[test]
    fn long_tid_rejected() {
        let mut w = b"d1:t40:".to_vec();
        w.extend_from_slice(&[b'x'; 40]);
        w.extend_from_slice(b"1:y1:ee");
        assert!(parse(&w).is_err());
    }

    #[test]
    fn nodes_with_partial_tail_ignored() {
        let mut w = b"d1:rd2:id20:abcdefghij01234567895:nodes30:".to_vec();
        w.extend_from_slice(&[7u8; 30]);
        w.extend_from_slice(b"e1:t2:aa1:y1:re");
        let Body::Response(r) = parse(&w).unwrap().body else {
            panic!()
        };
        assert_eq!(r.nodes().count(), 1);
    }

    #[test]
    fn never_panics_on_truncations_and_mutations() {
        let base = b"d1:ad2:id20:abcdefghij01234567899:info_hash20:mnopqrstuvwxyz1234564:porti6881e5:token8:aoeusnthe1:q13:announce_peer1:t2:aa1:y1:qe";
        for n in 0..base.len() {
            let _ = parse(&base[..n]);
        }
        for i in 0..base.len() {
            for b in [0u8, b'e', b'i', b'd', b'l', b'9', b':', 0xff] {
                let mut m = base.to_vec();
                m[i] = b;
                let _ = parse(&m);
            }
        }
    }
}
