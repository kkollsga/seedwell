//! Prints the size of every per-peer type (documented in README.md).
use trs_wire::*;
fn main() {
    macro_rules! s {
        ($t:ty) => {
            println!(
                "{:<28} {:>4} bytes",
                stringify!($t),
                core::mem::size_of::<$t>()
            );
        };
    }
    s!(PeerState);
    s!(Decoder);
    s!(Limits);
    s!(Encoder);
    s!(Bitfield);
    s!(Handshake);
    s!(Reserved);
    s!(BlockRequest);
    s!(RequestWindow);
    s!(RemoteClient);
    s!(ExtPolicy);
    s!(MetadataAssembler);
    s!(ExtendedHandshake<'static>);
    s!(PexMessage<'static>);
    s!(OwnedMessage);
    s!(MessageRef<'static>);
}
