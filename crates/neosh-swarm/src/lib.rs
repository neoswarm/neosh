//! ASCP, the transport half: identity, framing, and the handshake.
//!
//! The message types are in `neosh-proto` — they are wire types like every other wire type, and
//! having them there is what lets the TypeScript side be generated from the same definitions and
//! the spec be generated from the implementation rather than written beside it.
//!
//! What lives here is everything a node needs to *be* one:
//!
//! - [`Identity`] — the ed25519 keypair a node is named by, and the list of peers it accepts.
//! - [`wire`] — length-prefixed JSON frames.
//! - [`handshake`] — the mutual challenge that turns a socket into an authenticated peer.
//!
//! Deliberately not here: NAT traversal, discovery, and anything resembling a control plane. ASCP
//! runs over whatever network you have. On a LAN that is nothing; across the internet it is
//! Tailscale, Nebula or the like, which have already solved hole-punching properly and would not be
//! improved by an agent workspace having a go at it.

pub mod allow;
pub mod handshake;
pub mod identity;
pub mod node;
pub mod wire;

pub use allow::{Allowed, Peer as AllowedPeer, Source};
pub use handshake::{accept, dial, Peer, Stranger};
pub use identity::Identity;
pub use node::{spawn, PeerAddress, SwarmConfig, SwarmEvent, SwarmHandle, SwarmRequest};

/// Ask what machine is at an address, without joining it.
///
/// The first half of pairing. A node presents its identity to anything that connects — as an SSH
/// server presents a host key — so this comes back with a name and a fingerprint that were
/// *proven*, not claimed, which is what makes it safe to put in front of somebody and ask.
///
/// Deliberately not `dial`: this hangs up immediately and never joins. The caller has learnt who is
/// there and nothing has been authorised.
pub async fn probe(
    addr: &str,
    me: &Identity,
    my_info: &neosh_proto::NodeInfo,
    my_caps: &neosh_proto::NodeCapabilities,
) -> Result<neosh_proto::NodeInfo, SwarmError> {
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    // An empty list refuses everybody, which is exactly what makes the answer a `Stranger` — the
    // only outcome this cares about, and the one carrying what we came for.
    let empty = Allowed::load(None);
    match dial(&mut stream, me, &empty, my_info, my_caps).await {
        Err(SwarmError::Stranger(s)) => Ok(s.node),
        // Only possible if the peer is somehow already on an empty list, which it cannot be.
        Ok(peer) => Ok(peer.node),
        Err(e) => Err(e),
    }
}

use neosh_proto::NodeId;

#[derive(Debug, thiserror::Error)]
pub enum SwarmError {
    #[error("swarm i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("swarm encoding: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("swarm protocol: {0}")]
    Protocol(String),
    #[error("swarm identity: {0}")]
    BadIdentity(String),
    /// A machine that proved who it is and is not on the list.
    ///
    /// Not `Unauthorised` with an id attached, because the difference is what a caller does next:
    /// this carries the peer's *proven* name and fingerprint, which is exactly what a person needs
    /// to be shown before deciding whether to add it. Boxed because it is much larger than the
    /// other variants and this enum is returned by value from every handshake.
    #[error("{} ({}) is not a machine this one has been paired with", .0.node.name, .0.node.id.short())]
    Stranger(Box<handshake::Stranger>),
    /// The peer claimed an id it could not prove. Distinct from `Unauthorised`, and much more
    /// serious: an authorised id that fails its own challenge means somebody is impersonating a
    /// machine you trust.
    #[error("node {} could not prove it holds its key", handshake::claimed(.0))]
    BadSignature(NodeId),
    #[error("this node speaks ASCP {ours}, the peer speaks {theirs}")]
    Version { ours: u32, theirs: u32 },
}
