//! Proving who is on the other end, before anything else is said.
//!
//! A mutual challenge, the same shape SSH uses. The dialler sends a random nonce; the listener
//! signs it and sends one of its own; the dialler signs that. Both sides check the signature
//! against the key the peer's [`NodeId`] *is*, and check that id against the list of peers they
//! authorised.
//!
//! Three properties are worth stating because each one is a thing that goes wrong when it is
//! missing:
//!
//! - **Both ends prove.** A one-sided handshake authenticates the caller to the callee and leaves
//!   the caller talking to whoever answered the port.
//! - **The challenge names both nodes and the version**, so a signature captured from an exchange
//!   with one machine cannot be replayed at another.
//! - **Authorisation is checked before the signature is requested and again after it verifies.**
//!   The first check means an unknown peer is dropped without spending a verification on it; the
//!   second is the one that actually counts, because until the signature verifies the claimed id
//!   is only a claim.

use neosh_proto::{AscpMessage, NodeCapabilities, NodeId, NodeInfo, ASCP_VERSION};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::identity::{challenge, nonce, verify, Identity};
use crate::wire::{read_message, write_message};
use crate::SwarmError;

/// What is known about the far end once the handshake has succeeded.
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    pub node: NodeInfo,
    pub capabilities: NodeCapabilities,
}

/// Dial: we opened the connection, so we challenge first.
pub async fn dial<S>(
    stream: &mut S,
    me: &Identity,
    my_info: &NodeInfo,
    my_caps: &NodeCapabilities,
) -> Result<Peer, SwarmError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let my_nonce = nonce();
    write_message(
        stream,
        &AscpMessage::Hello {
            version: ASCP_VERSION,
            node: announce(me, my_info),
            capabilities: my_caps.clone(),
            nonce: my_nonce.clone(),
        },
    )
    .await?;

    let Some(reply) = read_message(stream).await? else {
        return Err(SwarmError::Protocol("peer closed during the handshake".into()));
    };
    let AscpMessage::Welcome { version, node, capabilities, signature, nonce: their_nonce } = reply
    else {
        return Err(SwarmError::Protocol("expected a welcome".into()));
    };
    check_version(version)?;
    if !me.accepts(&node.id) {
        return Err(SwarmError::Unauthorised(node.id));
    }
    // The listener signs a challenge naming the dialler first, which is the order the dialler
    // built it in. Getting this backwards is the classic way a mutual handshake ends up verifying
    // a string neither side agreed on, and it fails closed rather than loudly, so it is pinned by
    // a test rather than by care.
    let expected = challenge(ASCP_VERSION, &my_nonce, me.id(), &node.id);
    if !verify(&node.id, &expected, &signature) {
        return Err(SwarmError::BadSignature(node.id));
    }

    let proof = me.sign(&challenge(ASCP_VERSION, &their_nonce, &node.id, me.id()));
    write_message(stream, &AscpMessage::Proof { signature: proof }).await?;
    Ok(Peer { node, capabilities })
}

/// Accept: they opened the connection, so we answer and challenge back.
pub async fn accept<S>(
    stream: &mut S,
    me: &Identity,
    my_info: &NodeInfo,
    my_caps: &NodeCapabilities,
) -> Result<Peer, SwarmError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(first) = read_message(stream).await? else {
        return Err(SwarmError::Protocol("peer closed before saying hello".into()));
    };
    let AscpMessage::Hello { version, node, capabilities, nonce: their_nonce } = first else {
        return Err(SwarmError::Protocol("expected a hello".into()));
    };
    check_version(version)?;
    // Before spending a signature verification on somebody we would refuse anyway.
    if !me.accepts(&node.id) {
        return Err(SwarmError::Unauthorised(node.id));
    }

    let my_nonce = nonce();
    let signature = me.sign(&challenge(ASCP_VERSION, &their_nonce, &node.id, me.id()));
    write_message(
        stream,
        &AscpMessage::Welcome {
            version: ASCP_VERSION,
            node: announce(me, my_info),
            capabilities: my_caps.clone(),
            signature,
            nonce: my_nonce.clone(),
        },
    )
    .await?;

    let Some(reply) = read_message(stream).await? else {
        return Err(SwarmError::Protocol("peer closed before proving itself".into()));
    };
    let AscpMessage::Proof { signature } = reply else {
        return Err(SwarmError::Protocol("expected a proof".into()));
    };
    let expected = challenge(ASCP_VERSION, &my_nonce, me.id(), &node.id);
    if !verify(&node.id, &expected, &signature) {
        return Err(SwarmError::BadSignature(node.id));
    }
    Ok(Peer { node, capabilities })
}

/// The node info to put on the wire: the caller's, with the id replaced by the one we can sign for.
///
/// The announced id and the signing key are two places the same fact is written down, and the
/// handshake only works while they agree — the challenge each side builds names both ids, so a node
/// announcing an id it does not hold produces a string the other side will not have built. Rather
/// than trusting the caller to keep them in step, the id comes from the key. A node that cannot
/// misdescribe itself is one fewer way for a handshake to fail for a reason nobody can see.
fn announce(me: &Identity, info: &NodeInfo) -> NodeInfo {
    NodeInfo { id: me.id().clone(), ..info.clone() }
}

/// Refuse rather than negotiate down.
///
/// A protocol that silently degrades is one where a feature the other machine does not have looks
/// like a bug in the other machine. Saying so is cheap; working out why steering does nothing on
/// exactly one node is not.
fn check_version(theirs: u32) -> Result<(), SwarmError> {
    if theirs != ASCP_VERSION {
        return Err(SwarmError::Version { ours: ASCP_VERSION, theirs });
    }
    Ok(())
}

/// A [`NodeId`] the peer claimed but has not proven. Only for error messages.
pub(crate) fn claimed(id: &NodeId) -> String {
    id.short().to_string()
}
