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

use crate::allow::Allowed;
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
    /// Machines to keep a connection to. Added to by pairing while the node runs.
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
    /// Authorise a machine and start keeping a connection to it, without a restart.
    ///
    /// `addr` is absent for one that dialled us: it knows where we are, and the address it happened
    /// to arrive from is not necessarily one we could dial back on.
    Pair { id: NodeId, name: String, addr: Option<String> },
    /// Withdraw authorisation. The connection, if there is one, goes with it.
    Unpair { id: NodeId },
    /// Dial a down peer again now, resetting its backoff, rather than waiting the delay out.
    Redial { id: NodeId },
    /// Close the connection to a peer and stop dialling it, keeping the pairing.
    ///
    /// [`SwarmRequest::Redial`] is the way back. The peer may still dial in — it is still on the
    /// allow-list — and a restart dials it again; this is for this workspace's lifetime only.
    Disconnect { id: NodeId },
    Shutdown,
}

/// Something the host needs to know.
#[derive(Debug, Clone)]
pub enum SwarmEvent {
    /// The listening socket is bound, on this address. Said once, at start.
    Listening { addr: SocketAddr },
    /// The listening socket could not be bound — most often a port another workspace holds.
    /// Dialling still works; a board should say the machine cannot be *reached*, not that the
    /// swarm is broken.
    ListenFailed { addr: SocketAddr, error: String },
    PeerUp { node: NodeInfo, capabilities: NodeCapabilities },
    /// A peer went away. `reason` is for the log and for a board that says why. `will_retry` says
    /// whether a dialler is still working on it — the difference between "reconnecting" and
    /// "gone", which is the whole of what a person would do differently.
    PeerDown { node: NodeId, reason: String, will_retry: bool },
    /// A dial did not produce a connection. `attempt` counts since the last success, so a board
    /// can say `try 4`; `node` is absent for an address whose machine we have never identified.
    DialFailed { node: Option<NodeId>, addr: String, attempt: u32, error: String },
    Inventory { node: NodeId, full: bool, agents: Vec<AgentSummary>, gone: Vec<SessionId> },
    Stream { node: NodeId, session: SessionId, event: StreamEvent },
    /// A peer asked us to do something. The host must answer with [`SwarmRequest::Answer`].
    Command { node: NodeId, id: String, session: SessionId, command: AgentCommand },
    /// A command we sent has been answered. Exactly one of these per command, ever.
    Answer { node: NodeId, id: String, result: Result<Option<SessionId>, Refusal> },
    /// A machine proved who it is and is not one we have paired with.
    ///
    /// The beginning of pairing rather than the end of a connection. Reported for both directions:
    /// a machine we dialled and did not recognise, and one that dialled us. Either way the id here
    /// has been *proven*, which is what makes it safe to show somebody and ask.
    Stranger { node: NodeInfo, dialled: bool },
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
        /// The **only** sender for this connection once the loop holds it: `serve` gives its own
        /// clone up at announce time, so the loop dropping this is what actually closes the
        /// writer, sends the FIN, and lets both ends unwind. A serve that kept a clone was a
        /// connection nothing could close — dropped peers stayed as idle sockets for ever.
        out: mpsc::UnboundedSender<AscpMessage>,
        /// Whether this end opened the connection. Decides which of two simultaneous connections
        /// survives; see the tie-break where this is read.
        dialled: bool,
        /// The address this end dialled, when it did — how the loop learns which address a node
        /// id lives at, for a `Redial` or `Disconnect` aimed at a peer the allow-list only knows
        /// by key.
        addr: Option<String>,
    },
    Message { node: NodeId, message: AscpMessage },
    Down { node: Option<NodeId>, reason: String },
    Stranger { node: NodeInfo, dialled: bool },
}

/// Start a node. Returns the handle the host keeps and the events it should read.
///
/// The identity is shared rather than owned because both the listener and every dialler need it,
/// and it is immutable once loaded — the allow-list is decided by config, and changing it means a
/// reload, which restarts this.
pub fn spawn(
    identity: Arc<Identity>,
    allowed: Allowed,
    config: SwarmConfig,
    version: String,
) -> (SwarmHandle, mpsc::UnboundedReceiver<SwarmEvent>) {
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();
    let id = identity.id().clone();
    tokio::spawn(run(identity, allowed, config, version, req_rx, evt_tx));
    (SwarmHandle { tx: req_tx, id }, evt_rx)
}

async fn run(
    identity: Arc<Identity>,
    allowed: Allowed,
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
                // The socket's own answer, not the config's: `:0` binds a port the config
                // never named, and the board should show the one a peer could actually dial.
                let bound = sock.local_addr().unwrap_or(addr);
                tracing::info!(addr = %bound, node = %identity.id().short(), "swarm listening");
                let _ = events.send(SwarmEvent::Listening { addr: bound });
                tokio::spawn(listen(
                    sock,
                    identity.clone(),
                    allowed.clone(),
                    info.clone(),
                    caps.clone(),
                    wire_tx.clone(),
                ));
            }
            // Not fatal. A machine that cannot listen can still dial, and refusing to start the
            // workspace because a port was taken would be a swarm feature breaking the editor.
            Err(e) => {
                tracing::warn!(%addr, "swarm could not listen: {e}");
                let _ = events.send(SwarmEvent::ListenFailed { addr, error: e.to_string() });
            }
        }
    }

    // One dialler task per address, and pairing adds more. Each is cancelled by its peer being
    // unpaired, which is why the handles are kept rather than detached and forgotten.
    let mut diallers: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    // A macro rather than a closure: a closure would have to borrow `caps`, which the loop below
    // also assigns to when the host reports its projects, and the borrow checker is right to
    // object. Everything here is cloned at spawn time anyway.
    macro_rules! start_dialling {
        ($peer:expr) => {{
            let peer: PeerAddress = $peer;
            if !diallers.contains_key(&peer.addr) {
                let addr = peer.addr.clone();
                let handle = tokio::spawn(keep_dialled(
                    peer,
                    identity.clone(),
                    allowed.clone(),
                    info.clone(),
                    caps.clone(),
                    wire_tx.clone(),
                    events.clone(),
                    config.heartbeat,
                ));
                diallers.insert(addr, handle);
            }
        }};
    }
    for peer in config.peers.clone() {
        start_dialling!(peer);
    }

    let mut peers: HashMap<NodeId, Connected> = HashMap::new();
    // Disconnected by the person, and not to be readmitted until they say so: a peer that dials
    // *us* would otherwise be back on its next heartbeat, which makes "disconnect" a button that
    // works for at most ten seconds. Still on the allow-list — this is "leave me alone for now",
    // and unpairing is the other, stronger verb.
    let mut paused: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    // Where each node was last reachable, learned from the dial that found it. The allow-list is
    // the first answer; this is the fallback for a peer it only knows by key.
    let mut addr_of: HashMap<NodeId, String> = HashMap::new();
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
                    SwarmRequest::Pair { id, name, addr } => {
                        paused.remove(&id);
                        allowed.add(id.clone(), crate::allow::Peer {
                            name,
                            addr: addr.clone(),
                            source: crate::allow::Source::Paired,
                        });
                        // Only dial if we were told where. A machine that dialled us will dial
                        // again on its own schedule, and inventing an address for it would mean
                        // connecting to whatever else is on that host and port.
                        if let Some(addr) = addr {
                            start_dialling!(PeerAddress { addr, expect: Some(id) });
                        }
                    }
                    SwarmRequest::Unpair { id } => {
                        paused.remove(&id);
                        let addr = allowed.get(&id).and_then(|p| p.addr);
                        let _ = allowed.remove(&id);
                        if let Some(addr) = addr
                            && let Some(task) = diallers.remove(&addr)
                        {
                            // Stop reconnecting to a machine we have just stopped trusting.
                            // Without this it would keep dialling and keep being refused, for ever.
                            task.abort();
                        }
                        if let Some(p) = peers.remove(&id) {
                            let _ = p.out.send(AscpMessage::Goodbye {
                                message: Some("no longer paired".into()),
                            });
                            let _ = events.send(SwarmEvent::PeerDown {
                                node: id,
                                reason: "unpaired".into(),
                                will_retry: false,
                            });
                        }
                    }
                    SwarmRequest::Redial { id } => {
                        // Whatever a disconnect held back is welcome again — for a peer that
                        // dials us, this is the whole of what "reconnect" can do.
                        paused.remove(&id);
                        // Already connected means there is nothing to hurry. Otherwise restart the
                        // dialler: a fresh task begins with a fresh backoff, which is the whole
                        // point of somebody pressing the key.
                        if !peers.contains_key(&id)
                            && let Some(addr) = allowed
                                .get(&id)
                                .and_then(|p| p.addr)
                                .or_else(|| addr_of.get(&id).cloned())
                        {
                            if let Some(task) = diallers.remove(&addr) {
                                task.abort();
                            }
                            start_dialling!(PeerAddress { addr, expect: Some(id) });
                        }
                    }
                    SwarmRequest::Disconnect { id } => {
                        paused.insert(id.clone());
                        if let Some(addr) = allowed
                            .get(&id)
                            .and_then(|p| p.addr)
                            .or_else(|| addr_of.get(&id).cloned())
                            && let Some(task) = diallers.remove(&addr)
                        {
                            task.abort();
                        }
                        if let Some(p) = peers.remove(&id) {
                            let _ = p.out.send(AscpMessage::Goodbye {
                                message: Some("disconnected".into()),
                            });
                        }
                        // Said even when there was no live connection: a peer mid-retry that is
                        // told to stop has still changed state, and a board that keeps saying
                        // "reconnecting" about a dialler that is gone is lying.
                        let _ = events.send(SwarmEvent::PeerDown {
                            node: id,
                            reason: "disconnected by you".into(),
                            will_retry: false,
                        });
                    }
                    SwarmRequest::Command { node, id, session, command } => {
                        match peers.get(&node) {
                            Some(p) => {
                                let _ = p.out.send(AscpMessage::Command { id, session, command });
                            }
                            // Answered locally rather than dropped: a command with no reply is a
                            // keystroke you cannot tell from a slow network.
                            None => {
                                let will_retry = allowed
                                    .get(&node)
                                    .and_then(|p| p.addr)
                                    .is_some_and(|a| diallers.contains_key(&a));
                                let _ = events.send(SwarmEvent::PeerDown {
                                    node,
                                    reason: "not connected".into(),
                                    will_retry,
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
                Wire::Up { info: peer_info, capabilities, out, dialled, addr } => {
                    let id = peer_info.id.clone();
                    if let Some(addr) = addr {
                        addr_of.insert(id.clone(), addr);
                    }
                    // Disconnected by the person, and it dialled back in. Shut the door quietly:
                    // no `PeerDown`, because it never came up.
                    if paused.contains(&id) {
                        drop(out);
                        continue;
                    }
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
                Wire::Stranger { node, dialled } => {
                    let _ = events.send(SwarmEvent::Stranger { node, dialled });
                }
                Wire::Down { node, reason } => {
                    // Only if this *is* the connection we are using. The losing half of a
                    // simultaneous open reports itself down a moment after being dropped, and
                    // acting on that would take the winning connection out with it.
                    if let Some(id) = node
                        && peers.get(&id).is_some_and(|p| p.out.is_closed())
                    {
                        peers.remove(&id);
                        let will_retry = allowed
                            .get(&id)
                            .and_then(|p| p.addr)
                            .is_some_and(|a| diallers.contains_key(&a));
                        let _ = events.send(SwarmEvent::PeerDown { node: id, reason, will_retry });
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
                            let will_retry = allowed
                                .get(&node)
                                .and_then(|p| p.addr)
                                .is_some_and(|a| diallers.contains_key(&a));
                            let _ = events.send(SwarmEvent::PeerDown {
                                node,
                                reason: message.unwrap_or_else(|| "said goodbye".into()),
                                will_retry,
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
    allowed: Allowed,
    info: NodeInfo,
    caps: NodeCapabilities,
    wire: mpsc::UnboundedSender<Wire>,
) {
    loop {
        let Ok((stream, addr)) = sock.accept().await else { continue };
        let (identity, allowed, info, caps, wire) =
            (identity.clone(), allowed.clone(), info.clone(), caps.clone(), wire.clone());
        tokio::spawn(async move {
            let mut stream = stream;
            match accept(&mut stream, &identity, &allowed, &info, &caps).await {
                // No address: where an inbound connection happened to arrive from is not
                // necessarily somewhere it could be dialled back on.
                Ok(peer) => {
                    serve(stream, peer, wire, false, None).await;
                }
                // A machine that proved itself and is not on the list is somebody asking to pair,
                // not an error. It reaches the host, which can offer to add it.
                Err(crate::SwarmError::Stranger(s)) => {
                    let _ = wire.send(Wire::Stranger { node: s.node, dialled: false });
                }
                // At `debug`, not `warn`: on a network anybody can reach, a refused handshake is
                // the system working. A `warn` per port scan is a log nobody reads.
                Err(e) => tracing::debug!(%addr, "swarm refused an inbound connection: {e}"),
            }
        });
    }
}

/// Keep one configured peer connected, retrying for as long as the node runs.
///
/// The retry schedule backs off — 1s, 2s, 4s… capped at the heartbeat or 30s, whichever is longer
/// — and resets the moment a connection actually serves, so a machine that reboots is back within
/// seconds while one that is gone for the weekend is asked about twice a minute. Every failed dial
/// is reported with its attempt number, which is what lets a board say `try 4` instead of drawing
/// a spinner that never resolves.
async fn keep_dialled(
    peer: PeerAddress,
    identity: Arc<Identity>,
    allowed: Allowed,
    info: NodeInfo,
    caps: NodeCapabilities,
    wire: mpsc::UnboundedSender<Wire>,
    events: mpsc::UnboundedSender<SwarmEvent>,
    every: Duration,
) {
    enum Outcome {
        /// Connected, accepted, and served until it ended.
        Served,
        /// Reached the machine, and it has not allowed this one (yet).
        Refused,
        /// Reached a machine we have not paired with. What "add a computer" is waiting for.
        Stranger,
        Failed(String),
    }

    let cap = every.max(Duration::from_secs(30));
    let mut attempt: u32 = 0;
    loop {
        let outcome = match TcpStream::connect(&peer.addr).await {
            Ok(mut stream) => match dial(&mut stream, &identity, &allowed, &info, &caps).await {
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
                        Outcome::Failed(format!(
                            "another machine answered ({})",
                            found.node.id.short()
                        ))
                    } else if serve(stream, found, wire.clone(), true, Some(peer.addr.clone())).await
                    {
                        Outcome::Served
                    } else {
                        Outcome::Refused
                    }
                }
                Err(crate::SwarmError::Stranger(s)) => {
                    let _ = wire.send(Wire::Stranger { node: s.node, dialled: true });
                    Outcome::Stranger
                }
                Err(e) => {
                    tracing::debug!(addr = %peer.addr, "swarm handshake failed: {e}");
                    Outcome::Failed(e.to_string())
                }
            },
            Err(e) => {
                tracing::debug!(addr = %peer.addr, "swarm could not connect: {e}");
                Outcome::Failed(e.to_string())
            }
        };

        let delay = match outcome {
            // It connected and served. Whatever ended it may be a blip, so the next dial is
            // nearly immediate and the backoff starts over.
            Outcome::Served => {
                attempt = 0;
                Duration::from_secs(1)
            }
            // The machine is *there*, so backing off would only slow down the pairing it is
            // probably in the middle of; the flat heartbeat is the pace. Refused is still worth a
            // row on the board — "it has not allowed this machine" names the key to press, over
            // there, where a person is presumably sitting.
            Outcome::Refused => {
                attempt = 0;
                let _ = events.send(SwarmEvent::DialFailed {
                    node: peer.expect.clone(),
                    addr: peer.addr.clone(),
                    attempt: 0,
                    error: "reached it — it has not allowed this machine yet".into(),
                });
                every
            }
            Outcome::Stranger => {
                attempt = 0;
                every
            }
            Outcome::Failed(error) => {
                attempt = attempt.saturating_add(1);
                let _ = events.send(SwarmEvent::DialFailed {
                    node: peer.expect.clone(),
                    addr: peer.addr.clone(),
                    attempt,
                    error,
                });
                cap.min(Duration::from_secs(1u64 << (attempt - 1).min(6)))
            }
        };
        tokio::time::sleep(delay).await;
    }
}

/// Pump one authenticated connection until it ends.
///
/// Returns whether the connection was ever announced — for a dialler, whether the far end accepted
/// us. `false` from a dial means "reached it, and it has not allowed this machine", which is a
/// different fact from the address being dead and is paced differently by the caller.
async fn serve(
    stream: TcpStream,
    peer: crate::Peer,
    wire: mpsc::UnboundedSender<Wire>,
    dialled: bool,
    addr: Option<String>,
) -> bool {
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

    // The listener has decided — it checked the list before returning — so it is connected now.
    //
    // The dialler has not. It finished *its* half: it verified the peer and found it on its own
    // list, and sent a proof the peer has not yet judged. If the peer does not have *us* on its
    // list, it hangs up a moment later. Announcing a connection at this point produces "linux-box
    // joined the swarm" immediately followed by "lost linux-box" every time pairing is half done,
    // which is exactly when a person most needs to be told something true.
    //
    // So a dialler waits for one message. The listener's node sends a full inventory the instant it
    // accepts, so a real connection announces itself within a round trip, and a refused one never
    // announces at all.
    //
    // The sender is *moved* into the announce rather than cloned, so once the loop has it, it has
    // the only one — and the loop dropping it is what closes the writer, sends the FIN and lets
    // both ends unwind. A serve that kept a clone was a connection nothing could ever close: a
    // dropped peer stayed as an idle socket, and its dialler stayed wedged inside it.
    let mut announce = Some(out_tx);
    if !dialled {
        let _ = wire.send(Wire::Up {
            info: peer.node.clone(),
            capabilities: peer.capabilities.clone(),
            out: announce.take().expect("first take"),
            dialled,
            addr: addr.clone(),
        });
    }

    let reason = loop {
        match read_message(&mut read).await {
            Ok(Some(message)) => {
                if let Some(out) = announce.take() {
                    let _ = wire.send(Wire::Up {
                        info: peer.node.clone(),
                        capabilities: peer.capabilities.clone(),
                        out,
                        dialled,
                        addr: addr.clone(),
                    });
                }
                if wire.send(Wire::Message { node: id.clone(), message }).is_err() {
                    break "the swarm stopped".to_string();
                }
            }
            Ok(None) => break "connection closed".to_string(),
            Err(e) => break format!("{e}"),
        }
    };
    // Nothing to report down if nothing was ever reported up: a dialler the far end refused is a
    // stranger, which was already said, not a peer that went away.
    if announce.is_some() {
        writer.abort();
        return false;
    }
    writer.abort();
    let _ = wire.send(Wire::Down { node: Some(id), reason });
    true
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
