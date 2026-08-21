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

pub mod handshake;
pub mod identity;
pub mod node;
pub mod wire;

pub use handshake::{accept, dial, Peer};
pub use identity::Identity;
pub use node::{spawn, PeerAddress, SwarmConfig, SwarmEvent, SwarmHandle, SwarmRequest};

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
    /// The peer is not on this node's list. The ordinary answer to a stranger, not a failure.
    #[error("node {} is not one this machine has been told to accept", handshake::claimed(.0))]
    Unauthorised(NodeId),
    /// The peer claimed an id it could not prove. Distinct from `Unauthorised`, and much more
    /// serious: an authorised id that fails its own challenge means somebody is impersonating a
    /// machine you trust.
    #[error("node {} could not prove it holds its key", handshake::claimed(.0))]
    BadSignature(NodeId),
    #[error("this node speaks ASCP {ours}, the peer speaks {theirs}")]
    Version { ours: u32, theirs: u32 },
}
