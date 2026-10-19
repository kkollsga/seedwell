//! BEP 10 extension protocol: extended handshake and message dispatch.

use crate::bencode::{self, Val};
use crate::error::{Error, Result};
use crate::metadata::MetadataMessage;
use crate::pex::PexMessage;
use bytes::BufMut;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Extension id we assign to `ut_metadata` in our own handshake (what peers send us).
pub const LOCAL_UT_METADATA_ID: u8 = 1;
/// Extension id we assign to `ut_pex`.
pub const LOCAL_UT_PEX_ID: u8 = 2;
/// Default `reqq` we advertise: how many outstanding requests we accept from a peer.
///
/// A leecher never queues more than this, and Transmission re-fills its queue only twice a second, so it
/// is also the ceiling of what one connection can fetch: `reqq * 16 KiB / 0.5 s` (250 meant 8 MB/s per
/// connection, 1000 means 32 MB/s). A queued request costs us 24 bytes while it waits (the data is read
/// ahead only a little, see `peer.rs`).
pub const DEFAULT_REQQ: u32 = 1000;
/// At most this many extra (unknown) extension names are written or kept per handshake.
pub const MAX_EXTRA_EXTENSIONS: usize = 8;
/// Longest extension name we write.
pub const MAX_EXTENSION_NAME: usize = 32;
/// The identity string prefix. There is deliberately no way to change it.
pub const CLIENT_NAME: &str = "Seedwell";
/// Longest `v` string retained from a peer (longer ones are truncated).
pub const MAX_REMOTE_CLIENT_LEN: usize = 32;

/// Which optional extensions this torrent may use.
///
/// The engine builds this from its PEX rule (`pex = global && (!private || tracker allows it)`,
/// SPEC 7B.4). PEX is off for private torrents unless the user allowed it for their tracker
/// (BEP 27 says off; most private trackers forbid it); when it is off we neither advertise nor accept it.
// size: 2 bytes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtPolicy {
    /// Advertise and accept `ut_pex`.
    pub allow_pex: bool,
    /// Advertise and accept `ut_metadata`.
    pub allow_metadata: bool,
}

impl ExtPolicy {
    /// Policy for a torrent: PEX off when `private` (the engine turns it on again per tracker).
    pub const fn for_torrent(private: bool) -> Self {
        Self {
            allow_pex: !private,
            allow_metadata: true,
        }
    }
}

/// An extended handshake (message id 20, extension id 0), borrowing its strings.
///
/// Decoded from a peer with [`ExtendedHandshake::parse`], or written for ourselves
/// with [`LocalExtendedHandshake::encode_payload`].
// size: 80 bytes
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExtendedHandshake<'a> {
    /// Peer's id for `ut_metadata` (what *we* must put in messages we send it).
    pub ut_metadata: Option<u8>,
    /// Peer's id for `ut_pex`.
    pub ut_pex: Option<u8>,
    /// `v`: client name and version, raw bytes (not necessarily UTF-8).
    pub client: Option<&'a [u8]>,
    /// `reqq`: max outstanding requests the peer accepts.
    pub reqq: Option<u32>,
    /// `p`: the peer's listen port.
    pub listen_port: Option<u16>,
    /// `yourip`: our address as the peer sees it.
    pub your_ip: Option<IpAddr>,
    /// `metadata_size` in bytes.
    pub metadata_size: Option<u32>,
    /// `upload_only` (BEP 21).
    pub upload_only: Option<bool>,
    /// Raw `m` dictionary span, for [`ExtendedHandshake::unknown_extensions`].
    m_raw: Option<&'a [u8]>,
}

impl<'a> ExtendedHandshake<'a> {
    /// Parse the payload of extended message id 0.
    ///
    /// Structural bencode errors fail; individual fields with the wrong type or an
    /// out-of-range value are ignored, as real clients are sloppy.
    pub fn parse(payload: &'a [u8]) -> Result<Self> {
        let (d, _) = bencode::dict_prefix(payload)?;
        let mut h = Self::default();
        for (k, v) in d.iter() {
            match (k, v) {
                (b"m", Val::Dict(raw)) => {
                    h.m_raw = Some(raw);
                    let m = bencode::Dict(raw);
                    h.ut_metadata = m.get(b"ut_metadata").and_then(ext_id);
                    h.ut_pex = m.get(b"ut_pex").and_then(ext_id);
                }
                (b"v", Val::Bytes(b)) => {
                    h.client = Some(b.get(..MAX_REMOTE_CLIENT_LEN).unwrap_or(b));
                }
                (b"reqq", v) => h.reqq = v.as_u32(),
                (b"p", v) => h.listen_port = v.as_int().and_then(|i| u16::try_from(i).ok()),
                (b"yourip", Val::Bytes(b)) => h.your_ip = ip_from_bytes(b),
                (b"metadata_size", v) => h.metadata_size = v.as_u32(),
                (b"upload_only", v) => h.upload_only = v.as_int().map(|i| i != 0),
                _ => {}
            }
        }
        Ok(h)
    }

    /// Extensions the peer announced that we do not implement, as `(name, id)`
    /// (id 0 means "disabled" and is skipped). Capped at [`MAX_EXTRA_EXTENSIONS`].
    pub fn unknown_extensions(&self) -> impl Iterator<Item = (&'a [u8], u8)> + 'a {
        self.m_raw
            .into_iter()
            .flat_map(|raw| bencode::Dict(raw).iter())
            .filter(|(k, _)| *k != b"ut_metadata" && *k != b"ut_pex")
            .filter_map(|(k, v)| ext_id(v).map(|id| (k, id)))
            .take(MAX_EXTRA_EXTENSIONS)
    }

    /// Peer's `v` as lossy UTF-8, for logs and UI.
    pub fn client_lossy(&self) -> Option<std::borrow::Cow<'a, str>> {
        self.client.map(String::from_utf8_lossy)
    }
}

fn ext_id(v: Val<'_>) -> Option<u8> {
    v.as_int()
        .and_then(|i| u8::try_from(i).ok())
        .filter(|&i| i != 0)
}

fn ip_from_bytes(b: &[u8]) -> Option<IpAddr> {
    match b.len() {
        4 => Some(IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3]))),
        16 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(b);
            Some(IpAddr::V6(Ipv6Addr::from(o)))
        }
        _ => None,
    }
}

/// Parameters for our own extended handshake.
///
/// The `v` field is always `"Seedwell <version>"`: only the version part comes from
/// the caller, and it is sanitised to `[A-Za-z0-9._+-]` (max 24 chars). Pretending to
/// be another client is a hard non-goal (SPEC 7B.1).
#[derive(Clone, Copy, Debug)]
pub struct LocalExtendedHandshake<'a> {
    /// Our version, e.g. `"1.0.0"`.
    pub version: &'a str,
    /// Which extensions to advertise.
    pub policy: ExtPolicy,
    /// `reqq`.
    pub reqq: u32,
    /// `p`: our listen port, if listening.
    pub listen_port: Option<u16>,
    /// `yourip`: the remote's address as we see it.
    pub your_ip: Option<IpAddr>,
    /// `metadata_size` if we have the metadata (so peers can fetch it).
    pub metadata_size: Option<u32>,
    /// `upload_only`: set when we are seeding.
    pub upload_only: bool,
    /// Extra `m` entries to advertise (name, id); capped at [`MAX_EXTRA_EXTENSIONS`].
    pub extra: &'a [(&'a str, u8)],
}

impl<'a> LocalExtendedHandshake<'a> {
    /// Handshake with defaults for `version` and `policy`.
    pub fn new(version: &'a str, policy: ExtPolicy) -> Self {
        Self {
            version,
            policy,
            reqq: DEFAULT_REQQ,
            listen_port: None,
            your_ip: None,
            metadata_size: None,
            upload_only: false,
            extra: &[],
        }
    }

    /// Our identity string, `"Seedwell <version>"`.
    pub fn client_string(&self) -> String {
        let mut s = String::from(CLIENT_NAME);
        s.push(' ');
        s.extend(
            self.version
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
                .take(24),
        );
        s
    }

    /// Write the bencoded payload (keys sorted, as the spec requires).
    pub fn encode_payload<B: BufMut>(&self, out: &mut B) {
        use bencode::{put_bytes, put_kv_int};
        out.put_u8(b'd');
        // Sorted order: m, metadata_size, p, reqq, upload_only, v, yourip.
        let mut m: [(&str, u8); MAX_EXTRA_EXTENSIONS + 2] = [("", 0); MAX_EXTRA_EXTENSIONS + 2];
        let mut n = 0;
        if self.policy.allow_metadata {
            m[n] = ("ut_metadata", LOCAL_UT_METADATA_ID);
            n += 1;
        }
        if self.policy.allow_pex {
            m[n] = ("ut_pex", LOCAL_UT_PEX_ID);
            n += 1;
        }
        for &(name, id) in self.extra.iter().take(MAX_EXTRA_EXTENSIONS) {
            if id != 0 && !name.is_empty() && name.len() <= MAX_EXTENSION_NAME && n < m.len() {
                m[n] = (name, id);
                n += 1;
            }
        }
        if let Some(live) = m.get_mut(..n) {
            live.sort_unstable_by(|a, b| a.0.cmp(b.0));
        }
        out.put_slice(b"1:md");
        for &(name, id) in m.iter().take(n) {
            put_kv_int(out, name, i64::from(id));
        }
        out.put_u8(b'e');
        if let (true, Some(sz)) = (self.policy.allow_metadata, self.metadata_size) {
            put_kv_int(out, "metadata_size", i64::from(sz));
        }
        if let Some(p) = self.listen_port {
            put_kv_int(out, "p", i64::from(p));
        }
        put_kv_int(out, "reqq", i64::from(self.reqq));
        if self.upload_only {
            put_kv_int(out, "upload_only", 1);
        }
        put_bytes(out, b"v");
        put_bytes(out, self.client_string().as_bytes());
        if let Some(ip) = self.your_ip {
            put_bytes(out, b"yourip");
            match ip {
                IpAddr::V4(a) => put_bytes(out, &a.octets()),
                IpAddr::V6(a) => put_bytes(out, &a.octets()),
            }
        }
        out.put_u8(b'e');
    }
}

/// A decoded extended message, classified by the id *we* assigned in our handshake.
#[derive(Debug, PartialEq, Eq)]
pub enum ExtendedMessage<'a> {
    /// Extension id 0.
    Handshake(ExtendedHandshake<'a>),
    /// `ut_metadata` (BEP 9).
    Metadata(MetadataMessage<'a>),
    /// `ut_pex` (BEP 11).
    Pex(PexMessage<'a>),
    /// An extension we did not register; payload passed through untouched.
    Unknown {
        /// Extension id from the frame.
        id: u8,
        /// Raw payload.
        payload: &'a [u8],
    },
}

impl<'a> ExtendedMessage<'a> {
    /// Classify and parse an incoming extended frame.
    ///
    /// Returns [`Error::ExtensionDisabled`] if the peer uses an extension that
    /// `policy` disabled (notably PEX on private torrents); the engine should ignore
    /// the frame (or drop a persistent offender).
    pub fn parse(id: u8, payload: &'a [u8], policy: ExtPolicy) -> Result<Self> {
        match id {
            0 => Ok(Self::Handshake(ExtendedHandshake::parse(payload)?)),
            LOCAL_UT_METADATA_ID if policy.allow_metadata => {
                Ok(Self::Metadata(MetadataMessage::parse(payload)?))
            }
            LOCAL_UT_METADATA_ID => Err(Error::ExtensionDisabled("ut_metadata")),
            LOCAL_UT_PEX_ID if policy.allow_pex => Ok(Self::Pex(PexMessage::parse(payload)?)),
            LOCAL_UT_PEX_ID => Err(Error::ExtensionDisabled("ut_pex")),
            _ => Ok(Self::Unknown { id, payload }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(policy: ExtPolicy) -> Vec<u8> {
        let mut h = LocalExtendedHandshake::new("0.1.0", policy);
        h.listen_port = Some(6881);
        h.metadata_size = Some(31235);
        h.your_ip = Some("1.2.3.4".parse().unwrap());
        let mut v = Vec::new();
        h.encode_payload(&mut v);
        v
    }

    #[test]
    fn byte_exact_public() {
        let v = local(ExtPolicy::for_torrent(false));
        let exp = b"d1:md11:ut_metadatai1e6:ut_pexi2ee13:metadata_sizei31235e1:pi6881e4:reqqi1000e1:v14:Seedwell 0.1.06:yourip4:\x01\x02\x03\x04e";
        assert_eq!(String::from_utf8_lossy(&v), String::from_utf8_lossy(exp));
        assert_eq!(v, exp);
    }

    #[test]
    fn private_torrent_never_advertises_pex() {
        let v = local(ExtPolicy::for_torrent(true));
        assert!(!v.windows(6).any(|w| w == b"ut_pex"));
        let h = ExtendedHandshake::parse(&v).unwrap();
        assert_eq!(h.ut_pex, None);
        assert_eq!(h.ut_metadata, Some(LOCAL_UT_METADATA_ID));
    }

    #[test]
    fn roundtrip_local_to_remote_view() {
        let mut l = LocalExtendedHandshake::new("1.2.3", ExtPolicy::for_torrent(false));
        l.upload_only = true;
        l.listen_port = Some(51413);
        l.your_ip = Some("2001:db8::1".parse().unwrap());
        l.extra = &[("lt_donthave", 7), ("a_first", 9), ("", 3), ("zero", 0)];
        let mut v = Vec::new();
        l.encode_payload(&mut v);
        let h = ExtendedHandshake::parse(&v).unwrap();
        assert_eq!(h.ut_metadata, Some(1));
        assert_eq!(h.ut_pex, Some(2));
        assert_eq!(h.reqq, Some(1000));
        assert_eq!(h.listen_port, Some(51413));
        assert_eq!(h.upload_only, Some(true));
        assert_eq!(h.your_ip, Some("2001:db8::1".parse().unwrap()));
        assert_eq!(h.metadata_size, None);
        assert_eq!(h.client, Some(&b"Seedwell 1.2.3"[..]));
        assert_eq!(h.client_lossy().unwrap(), "Seedwell 1.2.3");
        let extra: Vec<_> = h.unknown_extensions().collect();
        assert_eq!(extra, vec![(&b"a_first"[..], 9), (&b"lt_donthave"[..], 7)]);
        // Our own output is canonical bencode: keys strictly sorted.
        let (d, _) = bencode::dict_prefix(&v).unwrap();
        let keys: Vec<_> = d.iter().map(|(k, _)| k.to_vec()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn identity_cannot_be_overridden() {
        // Whatever the caller passes, the string starts with "Seedwell " and contains
        // only the sanitised version characters.
        for (ver, want) in [
            ("1.0.0", "Seedwell 1.0.0"),
            ("qBittorrent/4.6.0", "Seedwell qBittorrent4.6.0"),
            ("1.0 \r\n\0é", "Seedwell 1.0"),
            ("", "Seedwell "),
        ] {
            let h = LocalExtendedHandshake::new(ver, ExtPolicy::for_torrent(false));
            assert_eq!(h.client_string(), want);
        }
        let long = "9".repeat(100);
        let h = LocalExtendedHandshake::new(&long, ExtPolicy::for_torrent(false));
        assert_eq!(h.client_string().len(), "Seedwell ".len() + 24);
    }

    #[test]
    fn parses_real_world_style_handshakes() {
        // libtorrent-like.
        let p = b"d1:md11:lt_donthavei7e10:share_modei8e11:upload_onlyi3e12:ut_holepunchi4e11:ut_metadatai2e6:ut_pexi1ee13:metadata_sizei31235e1:pi6881e4:reqqi500e11:upload_onlyi0e1:v17:libtorrent 2.0.106:yourip4:\x7f\x00\x00\x01e";
        let h = ExtendedHandshake::parse(p).unwrap();
        assert_eq!((h.ut_metadata, h.ut_pex), (Some(2), Some(1)));
        assert_eq!(h.metadata_size, Some(31235));
        assert_eq!(h.reqq, Some(500));
        assert_eq!(h.your_ip, Some("127.0.0.1".parse().unwrap()));
        let names: Vec<_> = h.unknown_extensions().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            vec![
                &b"lt_donthave"[..],
                b"share_mode",
                b"upload_only",
                b"ut_holepunch"
            ]
        );
    }

    #[test]
    fn sloppy_fields_ignored_structure_errors_fail() {
        let h = ExtendedHandshake::parse(
            b"d1:pi70000e4:reqqi-1e13:metadata_size3:abc6:yourip3:abc1:v i5ee",
        );
        assert!(h.is_err()); // "1:v " then int key is structurally invalid.
        let h =
            ExtendedHandshake::parse(b"d1:pi70000e4:reqqi-1e13:metadata_size3:abc6:yourip3:abce")
                .unwrap();
        assert_eq!(h, ExtendedHandshake::default());
        let h = ExtendedHandshake::parse(b"d1:md11:ut_metadatai0e6:ut_pexi256eee").unwrap();
        assert_eq!((h.ut_metadata, h.ut_pex), (None, None));
        assert!(ExtendedHandshake::parse(b"").is_err());
        assert!(ExtendedHandshake::parse(b"i1e").is_err());
        assert!(ExtendedHandshake::parse(b"d1:m").is_err());
        let h = ExtendedHandshake::parse(b"d1:v5:abcdee").unwrap();
        assert_eq!(h.client, Some(&b"abcde"[..]));
        let long = format!("d1:v100:{}e", "x".repeat(100));
        let h = ExtendedHandshake::parse(long.as_bytes()).unwrap();
        assert_eq!(h.client.unwrap().len(), MAX_REMOTE_CLIENT_LEN);
    }

    #[test]
    fn dispatch() {
        let pub_ = ExtPolicy::for_torrent(false);
        let priv_ = ExtPolicy::for_torrent(true);
        assert!(matches!(
            ExtendedMessage::parse(0, b"de", pub_),
            Ok(ExtendedMessage::Handshake(_))
        ));
        assert!(matches!(
            ExtendedMessage::parse(LOCAL_UT_METADATA_ID, b"d8:msg_typei0e5:piecei0ee", pub_),
            Ok(ExtendedMessage::Metadata(MetadataMessage::Request {
                piece: 0
            }))
        ));
        assert!(matches!(
            ExtendedMessage::parse(LOCAL_UT_PEX_ID, b"de", pub_),
            Ok(ExtendedMessage::Pex(_))
        ));
        assert_eq!(
            ExtendedMessage::parse(LOCAL_UT_PEX_ID, b"de", priv_).unwrap_err(),
            Error::ExtensionDisabled("ut_pex")
        );
        assert_eq!(
            ExtendedMessage::parse(
                LOCAL_UT_METADATA_ID,
                b"de",
                ExtPolicy {
                    allow_pex: true,
                    allow_metadata: false
                }
            )
            .unwrap_err(),
            Error::ExtensionDisabled("ut_metadata")
        );
        // Unknown ids pass through untouched, even when not bencode.
        assert_eq!(
            ExtendedMessage::parse(77, b"\xff\x00raw", pub_).unwrap(),
            ExtendedMessage::Unknown {
                id: 77,
                payload: b"\xff\x00raw"
            }
        );
    }

    #[test]
    fn garbage_never_panics() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 48) as usize;
            let mut v: Vec<u8> = (0..n)
                .map(|i| b"di0e:lm1357"[((x >> (i % 50)) % 11) as usize])
                .collect();
            if x & 1 == 0 && !v.is_empty() {
                v[0] = b'd';
            }
            for id in [0u8, 1, 2, 9] {
                let _ = ExtendedMessage::parse(id, &v, ExtPolicy::for_torrent(false));
            }
        }
    }
}
