//! A running node: the listener, the diallers, and the peer table between them.
//!
//! The shape is one task per connection plus one loop that owns everything shared. Connections
//! push what they read into a single channel and the loop is the only thing that touches the peer
//! table, so there is no lock anywhere in here — a swarm where two peers arriving at once can
//! interleave their effects on a shared map is a swarm with a bug you reproduce once a fortnight.
//!
//! The host talks to the loop the same way: [`SwarmRequest`] in, [`SwarmEvent`] out, both over
//! channels. That is what makes the node testable without a host and the host testable without a
//! network.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use neosh_proto::{
    AgentCommand, AgentSummary, AscpMessage, NodeCapabilities, NodeId, NodeInfo, Refusal,
    RemoteProject, SessionId, StreamEvent,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::handshake::{accept, dial};
use crate::identity::Identity;
use crate::wire::{read_message, write_message};

/// How a node is set up. Everything here comes from `[swarm]` in the user's config.
#[derive(Debug, Clone)]
pub struct SwarmConfig {
    /// What to call this machine on other people's screens. The hostname unless somebody chose.
    pub name: String,
    /// Where to listen. `None` means dial-only — a laptop that joins other machines without
    /// being joinable, which is the right setting for anything that moves between networks.
    pub listen: Option<SocketAddr>,
    /// Machines to keep a connection to.
    pub peers: Vec<PeerAddress>,
    pub accepts_commands: bool,
    pub accepts_approvals: bool,
    /// How often to say "still here". Also the reconnect interval, which is deliberate: those are
    /// the same number in the sense that matters, which is how long a board may be wrong for.
    pub heartbeat: Duration,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            name: "neosh".into(),
            listen: None,
            peers: Vec::new(),
            accepts_commands: true,
            accepts_approvals: false,
            heartbeat: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddress {
    pub addr: String,
    /// The id we expect to find there, when the user wrote one down.
    ///
    /// Optional because an address is how you *reach* a node and its key is how you *know* it: the
    /// allow-list is what authorises, and this only catches "I meant the other machine" earlier
    /// than the allow-list would.
    pub expect: Option<NodeId>,
}

/// Something the host wants the swarm to do.
#[derive(Debug, Clone)]
pub enum SwarmRequest {
    /// Local agents changed. Fanned out to every connected peer.
    Publish { full: bool, agents: Vec<AgentSummary>, gone: Vec<SessionId> },
    /// What this node offers, for the next `Presence`.
    Projects { projects: Vec<RemoteProject> },
    /// Ask a peer to do something with one of its agents.
    Command { node: NodeId, id: String, session: SessionId, command: AgentCommand },
    /// Answer a request that came in.
    Answer { node: NodeId, id: String, result: Result<Option<SessionId>, Refusal> },
    Subscribe { node: NodeId, session: SessionId },
    Unsubscribe { node: NodeId, session: SessionId },
    /// Something happened in a local session. Sent only to peers that subscribed to it.
    Stream { session: SessionId, event: StreamEvent },
    Shutdown,
}

/// Something the host needs to know.
#[derive(Debug, Clone)]
pub enum SwarmEvent {
    PeerUp { node: NodeInfo, capabilities: NodeCapabilities },
    /// A peer went away. `reason` is for the log and for a board that says why.
    PeerDown { node: NodeId, reason: String },
    Inventory { node: NodeId, full: bool, agents: Vec<AgentSummary>, gone: Vec<SessionId> },
    Stream { node: NodeId, session: SessionId, event: StreamEvent },
    /// A peer asked us to do something. The host must answer with [`SwarmRequest::Answer`].
    Command { node: NodeId, id: String, session: SessionId, command: AgentCommand },
    /// A command we sent has been answered. Exactly one of these per command, ever.
    Answer { node: NodeId, id: String, result: Result<Option<SessionId>, Refusal> },
    /// A peer wants to watch one of our sessions, so the host should start sending its events.
    Subscribed { node: NodeId, session: SessionId },
    Unsubscribed { node: NodeId, session: SessionId },
}

/// What the host holds.
pub struct SwarmHandle {
    tx: mpsc::UnboundedSender<SwarmRequest>,
    id: NodeId,
}

impl SwarmHandle {
    pub fn id(&self) -> &NodeId {
        &self.id
    }

    /// Fire and forget. A closed channel means the swarm task is gone, which is not something the
    /// caller can do anything about and not a reason to fail whatever it was doing.
    pub fn send(&self, req: SwarmRequest) {
        let _ = self.tx.send(req);
    }
}

/// One connected peer, from the loop's point of view.
struct Connected {
    info: NodeInfo,
    capabilities: NodeCapabilities,
    out: mpsc::UnboundedSender<AscpMessage>,
    /// Sessions of *ours* this peer is watching.
    ///
    /// Held per peer rather than per session because that is the direction the question is asked
    /// in: when a local turn produces a token, the thing to answer is "who wants this", and a set
    /// per peer answers it with one pass over a handful of peers.
    watching: std::collections::HashSet<SessionId>,
}

/// What a connection task reports back to the loop.
enum Wire {
    Up {
        info: NodeInfo,
        capabilities: NodeCapabilities,
        out: mpsc::UnboundedSender<AscpMessage>,
        /// Whether this end opened the connection. Decides which of two simultaneous connections
        /// survives; see the tie-break where this is read.
        dialled: bool,
    },
    Message { node: NodeId, message: AscpMessage },
    Down { node: Option<NodeId>, reason: String },
}

/// Start a node. Returns the handle the host keeps and the events it should read.
///
/// The identity is shared rather than owned because both the listener and every dialler need it,
/// and it is immutable once loaded — the allow-list is decided by config, and changing it means a
/// reload, which restarts this.
pub fn spawn(
    identity: Arc<Identity>,
    config: SwarmConfig,
    version: String,
) -> (SwarmHandle, mpsc::UnboundedReceiver<SwarmEvent>) {
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();
    let id = identity.id().clone();
    tokio::spawn(run(identity, config, version, req_rx, evt_tx));
    (SwarmHandle { tx: req_tx, id }, evt_rx)
}

async fn run(
    identity: Arc<Identity>,
    config: SwarmConfig,
    version: String,
    mut requests: mpsc::UnboundedReceiver<SwarmRequest>,
    events: mpsc::UnboundedSender<SwarmEvent>,
) {
    let info = NodeInfo {
        id: identity.id().clone(),
        name: config.name.clone(),
        os: std::env::consts::OS.to_string(),
        version,
    };
    let mut caps = NodeCapabilities {
        accepts_commands: config.accepts_commands,
        accepts_approvals: config.accepts_approvals,
        streams: true,
        projects: Vec::new(),
    };

    let (wire_tx, mut wire_rx) = mpsc::unbounded_channel::<Wire>();

    if let Some(addr) = config.listen {
        match TcpListener::bind(addr).await {
            Ok(sock) => {
                tracing::info!(%addr, node = %identity.id().short(), "swarm listening");
                tokio::spawn(listen(sock, identity.clone(), info.clone(), caps.clone(), wire_tx.clone()));
            }
            // Not fatal. A machine that cannot listen can still dial, and refusing to start the
            // workspace because a port was taken would be a swarm feature breaking the editor.
            Err(e) => tracing::warn!(%addr, "swarm could not listen: {e}"),
        }
    }

    for peer in &config.peers {
        tokio::spawn(keep_dialled(
            peer.clone(),
            identity.clone(),
            info.clone(),
            caps.clone(),
            wire_tx.clone(),
            config.heartbeat,
        ));
    }

    let mut peers: HashMap<NodeId, Connected> = HashMap::new();
    // The last full picture of our own agents, kept so a peer that connects five minutes in gets a
    // snapshot rather than whatever delta happens next — which for an idle workspace is nothing at
    // all, so the board would stay empty until somebody typed.
    let mut mine: Vec<AgentSummary> = Vec::new();
    let mut seq: u64 = 0;
    let mut beat = tokio::time::interval(config.heartbeat);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(req) = requests.recv() => {
                match req {
                    SwarmRequest::Shutdown => {
                        for (_, p) in peers.iter() {
                            let _ = p.out.send(AscpMessage::Goodbye { message: None });
                        }
                        return;
                    }
                    SwarmRequest::Publish { full, agents, gone } => {
                        if full {
                            mine = agents.clone();
                        } else {
                            merge(&mut mine, &agents, &gone);
                        }
                        seq += 1;
                        let msg = AscpMessage::Inventory { seq, full, agents, gone };
                        for p in peers.values() {
                            let _ = p.out.send(msg.clone());
                        }
                    }
                    SwarmRequest::Projects { projects } => caps.projects = projects,
                    SwarmRequest::Command { node, id, session, command } => {
                        match peers.get(&node) {
                            Some(p) => {
                                let _ = p.out.send(AscpMessage::Command { id, session, command });
                            }
                            // Answered locally rather than dropped: a command with no reply is a
                            // keystroke you cannot tell from a slow network.
                            None => {
                                let _ = events.send(SwarmEvent::PeerDown {
                                    node,
                                    reason: "not connected".into(),
                                });
                            }
                        }
                    }
                    SwarmRequest::Answer { node, id, result } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(match result {
                                Ok(session) => AscpMessage::Ack { id, session },
                                Err(refusal) => AscpMessage::Refused { id, refusal },
                            });
                        }
                    }
                    SwarmRequest::Subscribe { node, session } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(AscpMessage::Subscribe { session });
                        }
                    }
                    SwarmRequest::Unsubscribe { node, session } => {
                        if let Some(p) = peers.get(&node) {
                            let _ = p.out.send(AscpMessage::Unsubscribe { session });
                        }
                    }
                    SwarmRequest::Stream { session, event } => {
                        // Only to peers that asked. This is the message family that would otherwise
                        // be one per token per peer, and the subscription is the whole of what
                        // keeps a twenty-machine swarm quiet.
                        for p in peers.values() {
                            if p.watching.contains(&session) {
                                let _ = p.out.send(AscpMessage::Stream {
                                    session: session.clone(),
                                    event: event.clone(),
                                });
                            }
                        }
                    }
                }
            }
            Some(w) = wire_rx.recv() => match w {
                Wire::Up { info: peer_info, capabilities, out, dialled } => {
                    let id = peer_info.id.clone();
                    // Two nodes that both dial each other end up with two connections, and both of
                    // them work. Something has to drop one, and *both ends have to drop the same
                    // one* — the first version of this kept whichever arrived later, which each
                    // side judged independently and therefore differently: each kept a socket the
                    // other had just abandoned, saw it close, reconnected, and did it again. It
                    // looked like a flapping network and was arithmetic.
                    //
                    // So the survivor is decided by something both sides compute identically: the
                    // connection dialled by whichever node has the smaller id. No clocks, no
                    // ordering, no negotiation.
                    if peers.contains_key(&id) {
                        let mine_wins = if dialled {
                            identity.id() < &id
                        } else {
                            &id < identity.id()
                        };
                        if !mine_wins {
                            // Close it quietly. Not a `PeerDown`: the peer is not going anywhere,
                            // this is one of two doors to it being shut.
                            drop(out);
                            continue;
                        }
                        if let Some(old) = peers.remove(&id) {
                            drop(old);
                        }
                    }
                    seq += 1;
                    let _ = out.send(AscpMessage::Inventory {
                        seq,
                        full: true,
                        agents: mine.clone(),
                        gone: Vec::new(),
                    });
                    peers.insert(id, Connected {
                        info: peer_info.clone(),
                        capabilities: capabilities.clone(),
                        out,
                        watching: Default::default(),
                    });
                    let _ = events.send(SwarmEvent::PeerUp { node: peer_info, capabilities });
                }
                Wire::Down { node, reason } => {
                    // Only if this *is* the connection we are using. The losing half of a
                    // simultaneous open reports itself down a moment after being dropped, and
                    // acting on that would take the winning connection out with it.
                    if let Some(id) = node
                        && peers.get(&id).is_some_and(|p| p.out.is_closed())
                    {
                        peers.remove(&id);
                        let _ = events.send(SwarmEvent::PeerDown { node: id, reason });
                    }
                }
                Wire::Message { node, message } => match message {
                    AscpMessage::Presence { node: peer_info, capabilities, .. } => {
                        if let Some(p) = peers.get_mut(&node) {
                            p.info = peer_info.clone();
                            p.capabilities = capabilities.clone();
                        }
                        let _ = events.send(SwarmEvent::PeerUp { node: peer_info, capabilities });
                    }
                    AscpMessage::Inventory { full, agents, gone, .. } => {
                        let _ = events.send(SwarmEvent::Inventory { node, full, agents, gone });
                    }
                    AscpMessage::Subscribe { session } => {
                        if let Some(p) = peers.get_mut(&node) {
                            p.watching.insert(session.clone());
                        }
                        let _ = events.send(SwarmEvent::Subscribed { node, session });
                    }
                    AscpMessage::Unsubscribe { session } => {
                        if let Some(p) = peers.get_mut(&node) {
                            p.watching.remove(&session);
                        }
                        let _ = events.send(SwarmEvent::Unsubscribed { node, session });
                    }
                    AscpMessage::Stream { session, event } => {
                        let _ = events.send(SwarmEvent::Stream { node, session, event });
                    }
                    AscpMessage::Command { id, session, command } => {
                        // Refused here rather than passed up when this node does not accept them
                        // at all: the host should not have to re-derive a policy it already
                        // declared, and a refusal that never reaches the caller is a hang.
                        let allowed = if matches!(command, AgentCommand::Approve { .. }) {
                            caps.accepts_commands && caps.accepts_approvals
                        } else {
                            caps.accepts_commands
                        };
                        if !allowed {
                            if let Some(p) = peers.get(&node) {
                                let _ = p.out.send(AscpMessage::Refused {
                                    id,
                                    refusal: Refusal::NotPermitted {
                                        what: "this node does not accept that".into(),
                                    },
                                });
                            }
                        } else {
                            let _ = events.send(SwarmEvent::Command { node, id, session, command });
                        }
                    }
                    AscpMessage::Ack { id, session } => {
                        let _ = events.send(SwarmEvent::Answer {
                            node,
                            id,
                            result: Ok(session),
                        });
                    }
                    AscpMessage::Refused { id, refusal } => {
                        let _ = events.send(SwarmEvent::Answer {
                            node,
                            id,
                            result: Err(refusal),
                        });
                    }
                    AscpMessage::Goodbye { message } => {
                        if peers.remove(&node).is_some() {
                            let _ = events.send(SwarmEvent::PeerDown {
                                node,
                                reason: message.unwrap_or_else(|| "said goodbye".into()),
                            });
                        }
                    }
                    // Handshake messages after the handshake are noise. Not an error: a peer
                    // reconnecting can legitimately have one in flight.
                    other => tracing::debug!(?other, "unexpected message after handshake"),
                },
            },
            _ = beat.tick() => {
                seq += 1;
                let msg = AscpMessage::Presence {
                    node: info.clone(),
                    capabilities: caps.clone(),
                    seq,
                };
                for p in peers.values() {
                    let _ = p.out.send(msg.clone());
                }
            }
            else => return,
        }
    }
}

/// Apply a delta to the local picture.
fn merge(mine: &mut Vec<AgentSummary>, agents: &[AgentSummary], gone: &[SessionId]) {
    mine.retain(|a| !gone.contains(&a.session));
    for a in agents {
        match mine.iter_mut().find(|m| m.session == a.session) {
            Some(m) => *m = a.clone(),
            None => mine.push(a.clone()),
        }
    }
}

async fn listen(
    sock: TcpListener,
    identity: Arc<Identity>,
    info: NodeInfo,
    caps: NodeCapabilities,
    wire: mpsc::UnboundedSender<Wire>,
) {
    loop {
        let Ok((stream, addr)) = sock.accept().await else { continue };
        let (identity, info, caps, wire) =
            (identity.clone(), info.clone(), caps.clone(), wire.clone());
        tokio::spawn(async move {
            let mut stream = stream;
            match accept(&mut stream, &identity, &info, &caps).await {
                Ok(peer) => serve(stream, peer, wire, false).await,
                // At `debug`, not `warn`: on a network anybody can reach, a refused handshake is
                // the system working. A `warn` per port scan is a log nobody reads.
                Err(e) => tracing::debug!(%addr, "swarm refused an inbound connection: {e}"),
            }
        });
    }
}

/// Keep one configured peer connected, retrying for as long as the node runs.
async fn keep_dialled(
    peer: PeerAddress,
    identity: Arc<Identity>,
    info: NodeInfo,
    caps: NodeCapabilities,
    wire: mpsc::UnboundedSender<Wire>,
    every: Duration,
) {
    loop {
        match TcpStream::connect(&peer.addr).await {
            Ok(mut stream) => match dial(&mut stream, &identity, &info, &caps).await {
                Ok(found) => {
                    // The address said one machine and another answered. Worth refusing rather
                    // than accepting quietly: the allow-list would let it through, and "my laptop
                    // is showing the build server's agents" is a confusing way to find out that a
                    // DHCP lease moved.
                    if let Some(want) = &peer.expect
                        && &found.node.id != want
                    {
                        tracing::warn!(
                            addr = %peer.addr,
                            "expected node {} there, found {}",
                            want.short(),
                            found.node.id.short()
                        );
                    } else {
                        serve(stream, found, wire.clone(), true).await;
                    }
                }
                Err(e) => tracing::debug!(addr = %peer.addr, "swarm handshake failed: {e}"),
            },
            Err(e) => tracing::debug!(addr = %peer.addr, "swarm could not connect: {e}"),
        }
        // Whether it failed to connect or connected and later dropped, the answer is the same and
        // the delay stops a machine that is off from being a busy loop.
        tokio::time::sleep(every).await;
    }
}

/// Pump one authenticated connection until it ends.
async fn serve(
    stream: TcpStream,
    peer: crate::Peer,
    wire: mpsc::UnboundedSender<Wire>,
    dialled: bool,
) {
    let id = peer.node.id.clone();
    let (mut read, mut write) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<AscpMessage>();

    // Writing happens in its own task so a slow peer cannot block the loop that feeds every other
    // peer. The channel is unbounded, which is a decision worth naming: bounding it would mean the
    // loop blocking on a stalled socket, which is exactly what this exists to avoid, and the
    // failure mode of unbounded — memory growing for a peer that never reads — is bounded in
    // practice by the connection dropping.
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_message(&mut write, &msg).await.is_err() {
                break;
            }
        }
        let _ = write.shutdown().await;
    });

    let _ = wire.send(Wire::Up {
        info: peer.node,
        capabilities: peer.capabilities,
        out: out_tx,
        dialled,
    });

    let reason = loop {
        match read_message(&mut read).await {
            Ok(Some(message)) => {
                if wire.send(Wire::Message { node: id.clone(), message }).is_err() {
                    break "the swarm stopped".to_string();
                }
            }
            Ok(None) => break "connection closed".to_string(),
            Err(e) => break format!("{e}"),
        }
    };
    writer.abort();
    let _ = wire.send(Wire::Down { node: Some(id), reason });
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{AgentState, ProjectKey};

    fn summary(id: &str) -> AgentSummary {
        AgentSummary {
            session: SessionId(id.into()),
            project: ProjectKey("git:example.com/x".into()),
            project_name: "x".into(),
            cwd: "/w/x".into(),
            label: id.into(),
            state: AgentState::Idle,
            message_count: 0,
            model: None,
            turn_started_at: None,
            updated_at: 0,
            usage: Default::default(),
        }
    }

    #[test]
    fn a_delta_adds_updates_and_removes() {
        let mut mine = vec![summary("a"), summary("b")];
        let mut updated = summary("a");
        updated.label = "renamed".into();
        merge(&mut mine, &[updated, summary("c")], &[SessionId("b".into())]);

        let labels: Vec<&str> = mine.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, vec!["renamed", "c"]);
    }

    /// A removal and an addition in one delta must not depend on which is applied first.
    #[test]
    fn a_session_removed_and_re_added_in_one_delta_survives() {
        let mut mine = vec![summary("a")];
        merge(&mut mine, &[summary("a")], &[SessionId("a".into())]);
        assert_eq!(mine.len(), 1, "the addition is the newer fact");
    }
}
