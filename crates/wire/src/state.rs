//! Tiny per-peer protocol state (choke/interest flags and negotiated extensions).

use crate::extended::{ExtendedHandshake, MAX_REMOTE_CLIENT_LEN};
use crate::handshake::Reserved;
use crate::message::Message;

/// The remote peer's client string (`v`), truncated to 32 bytes, stored inline so an
/// idle peer needs no extra allocation for it.
// size: 33 bytes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteClient {
    len: u8,
    bytes: [u8; MAX_REMOTE_CLIENT_LEN],
}

impl RemoteClient {
    fn new(src: &[u8]) -> Self {
        let n = src.len().min(MAX_REMOTE_CLIENT_LEN);
        let mut bytes = [0u8; MAX_REMOTE_CLIENT_LEN];
        bytes[..n].copy_from_slice(&src[..n]);
        Self {
            len: n as u8,
            bytes,
        }
    }

    /// Raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }

    /// Lossy UTF-8 for display.
    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(self.as_bytes()).into_owned()
    }
}

/// Protocol state for one connection after the handshake.
///
/// Per-peer memory budget (SPEC 5.2; target <= 2 KiB per idle peer, socket buffers
/// excluded): this struct is 56 bytes; a [`crate::Decoder`] is 24 bytes; an idle
/// [`crate::Encoder`] is 32 bytes once [`crate::Encoder::release_if_idle`] has released its
/// buffer; the peer's [`crate::Bitfield`] is 24 bytes + `ceil(pieces / 8)` and should
/// be dropped for peers that are not being downloaded from. Metadata and PEX state
/// exist only transiently. See `README.md` for the full table.
// size: 56 bytes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerState {
    /// We are choking the peer (initial: true).
    pub am_choking: bool,
    /// We are interested in the peer (initial: false).
    pub am_interested: bool,
    /// The peer is choking us (initial: true).
    pub peer_choking: bool,
    /// The peer is interested in us (initial: false).
    pub peer_interested: bool,
    /// Reserved bytes from the peer's handshake.
    pub remote_reserved: Reserved,
    /// Peer's `ut_metadata` id (0 = unsupported).
    pub remote_ut_metadata: u8,
    /// Peer's `ut_pex` id (0 = unsupported).
    pub remote_ut_pex: u8,
    /// Peer's `reqq` (0 = unknown).
    pub remote_reqq: u16,
    /// Peer's advertised listen port (0 = unknown).
    pub remote_listen_port: u16,
    /// Peer's advertised metadata size (0 = unknown).
    pub remote_metadata_size: u32,
    /// Peer's client string, if sent.
    pub remote_client: Option<RemoteClient>,
}

impl PeerState {
    /// Fresh state after a handshake with `remote_reserved`.
    pub fn new(remote_reserved: Reserved) -> Self {
        Self {
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            remote_reserved,
            remote_ut_metadata: 0,
            remote_ut_pex: 0,
            remote_reqq: 0,
            remote_listen_port: 0,
            remote_metadata_size: 0,
            remote_client: None,
        }
    }

    /// Update flags for a message received from the peer.
    pub fn on_received<D>(&mut self, m: &Message<D>) {
        match m {
            Message::Choke => self.peer_choking = true,
            Message::Unchoke => self.peer_choking = false,
            Message::Interested => self.peer_interested = true,
            Message::NotInterested => self.peer_interested = false,
            _ => {}
        }
    }

    /// Update flags for a message we are sending.
    pub fn on_sent<D>(&mut self, m: &Message<D>) {
        match m {
            Message::Choke => self.am_choking = true,
            Message::Unchoke => self.am_choking = false,
            Message::Interested => self.am_interested = true,
            Message::NotInterested => self.am_interested = false,
            _ => {}
        }
    }

    /// Record the peer's extended handshake.
    pub fn apply_extended_handshake(&mut self, h: &ExtendedHandshake<'_>) {
        self.remote_ut_metadata = h.ut_metadata.unwrap_or(0);
        self.remote_ut_pex = h.ut_pex.unwrap_or(0);
        self.remote_reqq = h.reqq.map_or(0, |r| r.min(u32::from(u16::MAX)) as u16);
        self.remote_listen_port = h.listen_port.unwrap_or(0);
        self.remote_metadata_size = h.metadata_size.unwrap_or(0);
        self.remote_client = h.client.map(RemoteClient::new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_budget() {
        assert!(
            core::mem::size_of::<PeerState>() <= 80,
            "{}",
            core::mem::size_of::<PeerState>()
        );
        assert!(core::mem::size_of::<crate::Decoder>() <= 32);
        assert!(core::mem::size_of::<crate::Encoder>() <= 32);
        assert!(core::mem::size_of::<crate::Bitfield>() <= 24);
        assert!(core::mem::size_of::<crate::Handshake>() <= 48);
    }

    #[test]
    fn flags() {
        let mut s = PeerState::new(Reserved::none());
        assert!(s.am_choking && s.peer_choking && !s.am_interested && !s.peer_interested);
        s.on_received(&Message::<&[u8]>::Unchoke);
        s.on_received(&Message::<&[u8]>::Interested);
        s.on_sent(&Message::<&[u8]>::Unchoke);
        s.on_sent(&Message::<&[u8]>::Interested);
        assert!(!s.peer_choking && s.peer_interested && !s.am_choking && s.am_interested);
        s.on_received(&Message::<&[u8]>::Choke);
        s.on_received(&Message::<&[u8]>::NotInterested);
        assert!(s.peer_choking && !s.peer_interested);
    }

    #[test]
    fn apply_handshake() {
        let payload = b"d1:md11:ut_metadatai3e6:ut_pexi4ee13:metadata_sizei999e1:pi6881e4:reqqi70000e1:v40:LongClientNameThatIsDefinitelyMoreThan32e";
        let h = ExtendedHandshake::parse(payload).unwrap();
        let mut s = PeerState::new(Reserved::none());
        s.apply_extended_handshake(&h);
        assert_eq!((s.remote_ut_metadata, s.remote_ut_pex), (3, 4));
        assert_eq!(s.remote_reqq, u16::MAX);
        assert_eq!(s.remote_listen_port, 6881);
        assert_eq!(s.remote_metadata_size, 999);
        assert_eq!(s.remote_client.unwrap().as_bytes().len(), 32);
    }
}
