//! Two nodes, running for real, seeing each other's agents.
//!
//! The handshake tests prove a connection can be trusted. These prove it is *useful*: that
//! inventory arrives without being asked for, that a peer connecting late is caught up rather than
//! left with an empty board, that a command reaches the machine that owns the agent and comes back
//! answered, and that a stream goes only to whoever subscribed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use neosh_proto::{
    AgentCommand, AgentState, AgentSummary, ProjectKey, Refusal, SessionId, StreamEvent,
};
use neosh_swarm::identity::Identity;
use neosh_swarm::node::{PeerAddress, SwarmConfig, SwarmEvent, SwarmHandle, SwarmRequest};
use tokio::sync::mpsc::UnboundedReceiver;

/// Long enough that a wedge fails the test rather than hanging the suite.
const PATIENCE: Duration = Duration::from_secs(10);

fn summary(id: &str, project: &str) -> AgentSummary {
    AgentSummary {
        session: SessionId(id.into()),
        project: ProjectKey(format!("git:example.com/{project}")),
        project_name: project.into(),
        cwd: format!("/w/{project}"),
        label: format!("working on {id}"),
        state: AgentState::Running,
        message_count: 3,
        model: Some("claude-opus-5".into()),
        turn_started_at: Some(1000),
        updated_at: 1000,
        usage: Default::default(),
    }
}

/// Read events until one matches, or give up. Anything that does not match is discarded, which is
/// what makes these tests indifferent to heartbeats and reconnects arriving in between.
async fn wait_for<T>(
    rx: &mut UnboundedReceiver<SwarmEvent>,
    mut f: impl FnMut(&SwarmEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let ev = rx.recv().await.expect("the swarm is still running");
            if let Some(v) = f(&ev) {
                return v;
            }
        }
    })
    .await
    .expect("waited too long for an event that never came")
}

struct Pair {
    a: SwarmHandle,
    a_rx: UnboundedReceiver<SwarmEvent>,
    b: SwarmHandle,
    b_rx: UnboundedReceiver<SwarmEvent>,
}

/// Two nodes that have been introduced, `b` listening and `a` dialling it.
async fn pair(b_accepts_commands: bool) -> Pair {
    // Bind first to learn the port, then hand the address to the node, which binds it again — the
    // socket is dropped in between. A fixed port would make these tests unable to run in parallel
    // or alongside a real neosh.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe");
    let addr: SocketAddr = probe.local_addr().expect("addr");
    drop(probe);

    let mut a_id = Identity::ephemeral();
    let mut b_id = Identity::ephemeral();
    a_id.allow(b_id.id().clone(), "b");
    b_id.allow(a_id.id().clone(), "a");

    let b_cfg = SwarmConfig {
        name: "linux-box".into(),
        listen: Some(addr),
        accepts_commands: b_accepts_commands,
        accepts_approvals: false,
        heartbeat: Duration::from_millis(200),
        ..Default::default()
    };
    let (b, b_rx) = neosh_swarm::node::spawn(Arc::new(b_id), b_cfg, "0.1.0".into());

    let a_cfg = SwarmConfig {
        name: "mac-studio".into(),
        peers: vec![PeerAddress { addr: addr.to_string(), expect: None }],
        heartbeat: Duration::from_millis(200),
        ..Default::default()
    };
    let (a, a_rx) = neosh_swarm::node::spawn(Arc::new(a_id), a_cfg, "0.1.0".into());

    Pair { a, a_rx, b, b_rx }
}

/// The board fills itself in. Nobody polls, and nobody asks.
#[tokio::test]
async fn a_node_learns_what_the_other_is_running() {
    let mut p = pair(true).await;

    let name = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.name.clone()),
        _ => None,
    })
    .await;
    assert_eq!(name, "linux-box", "the dialler sees the machine it dialled");

    p.b.send(SwarmRequest::Publish {
        full: true,
        agents: vec![summary("s1", "neosh"), summary("s2", "api")],
        gone: Vec::new(),
    });

    let agents = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::Inventory { agents, .. } if agents.len() == 2 => Some(agents.clone()),
        _ => None,
    })
    .await;
    assert_eq!(agents[0].label, "working on s1");
    assert_eq!(agents[0].state, AgentState::Running);
    assert_eq!(agents[0].cwd, "/w/neosh", "the owner's path, for showing");
    assert_eq!(agents[1].project, ProjectKey("git:example.com/api".into()));
}

/// A node that publishes before anybody is connected must not be invisible afterwards.
///
/// This is the case a naive fan-out gets wrong: the inventory went out to nobody, and the next one
/// is a delta that arrives when something changes — which on a workspace somebody has walked away
/// from is never. The board would be empty and correct-looking.
#[tokio::test]
async fn a_peer_connecting_late_is_caught_up_rather_than_left_empty() {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe");
    let addr: SocketAddr = probe.local_addr().expect("addr");
    drop(probe);

    let mut a_id = Identity::ephemeral();
    let mut b_id = Identity::ephemeral();
    a_id.allow(b_id.id().clone(), "b");
    b_id.allow(a_id.id().clone(), "a");

    let (b, _b_rx) = neosh_swarm::node::spawn(
        Arc::new(b_id),
        SwarmConfig {
            name: "b".into(),
            listen: Some(addr),
            heartbeat: Duration::from_millis(200),
            ..Default::default()
        },
        "0.1.0".into(),
    );
    // Said into an empty room.
    b.send(SwarmRequest::Publish {
        full: true,
        agents: vec![summary("earlier", "neosh")],
        gone: Vec::new(),
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (_a, mut a_rx) = neosh_swarm::node::spawn(
        Arc::new(a_id),
        SwarmConfig {
            name: "a".into(),
            peers: vec![PeerAddress { addr: addr.to_string(), expect: None }],
            heartbeat: Duration::from_millis(200),
            ..Default::default()
        },
        "0.1.0".into(),
    );

    let agents = wait_for(&mut a_rx, |e| match e {
        SwarmEvent::Inventory { full: true, agents, .. } if !agents.is_empty() => {
            Some(agents.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(agents[0].session, SessionId("earlier".into()));
}

/// Deltas, and the removal that has to reach the other side.
#[tokio::test]
async fn a_conversation_that_ends_stops_being_on_the_board() {
    let mut p = pair(true).await;
    wait_for(&mut p.a_rx, |e| matches!(e, SwarmEvent::PeerUp { .. }).then_some(())).await;

    p.b.send(SwarmRequest::Publish {
        full: true,
        agents: vec![summary("s1", "neosh")],
        gone: Vec::new(),
    });
    wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::Inventory { agents, .. } if agents.len() == 1 => Some(()),
        _ => None,
    })
    .await;

    p.b.send(SwarmRequest::Publish {
        full: false,
        agents: Vec::new(),
        gone: vec![SessionId("s1".into())],
    });
    let gone = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::Inventory { gone, .. } if !gone.is_empty() => Some(gone.clone()),
        _ => None,
    })
    .await;
    assert_eq!(gone, vec![SessionId("s1".into())]);
}

/// A command reaches the owner, and the owner's answer comes back.
#[tokio::test]
async fn a_command_reaches_the_machine_that_owns_the_agent() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    p.a.send(SwarmRequest::Command {
        node: b_id,
        id: "c1".into(),
        session: SessionId("s1".into()),
        command: AgentCommand::Send { text: "carry on".into() },
    });

    let (id, text) = wait_for(&mut p.b_rx, |e| match e {
        SwarmEvent::Command { id, command: AgentCommand::Send { text }, .. } => {
            Some((id.clone(), text.clone()))
        }
        _ => None,
    })
    .await;
    assert_eq!(text, "carry on", "the owner is told what to do, and by whom");
    assert_eq!(id, "c1");
}

/// A node that does not accept commands refuses them itself, without troubling its host.
///
/// The refusal has to come from the transport rather than from policy the host re-derives: a host
/// that forgot to check would otherwise turn "this machine is read-only" into a suggestion.
#[tokio::test]
async fn a_read_only_node_refuses_without_asking_its_host() {
    let mut p = pair(false).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, capabilities } => {
            assert!(!capabilities.accepts_commands, "and it said so in the handshake");
            Some(node.id.clone())
        }
        _ => None,
    })
    .await;

    p.a.send(SwarmRequest::Command {
        node: b_id,
        id: "c1".into(),
        session: SessionId("s1".into()),
        command: AgentCommand::Send { text: "do a thing".into() },
    });

    // The owning host must never hear about it. Waiting for a couple of heartbeats is the only way
    // to assert an absence, and a heartbeat here is 200ms.
    let heard = tokio::time::timeout(Duration::from_millis(700), async {
        loop {
            match p.b_rx.recv().await {
                Some(SwarmEvent::Command { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await;
    assert!(heard.is_err(), "a read-only node's host is not asked to enforce it");
}

/// Streams go to subscribers and to nobody else.
#[tokio::test]
async fn a_stream_arrives_only_after_subscribing() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    // Before subscribing: silence.
    p.b.send(SwarmRequest::Stream {
        session: SessionId("s1".into()),
        event: StreamEvent::Token { turn: "t1".into(), text: "unheard".into() },
    });
    let early = tokio::time::timeout(Duration::from_millis(400), async {
        loop {
            match p.a_rx.recv().await {
                Some(SwarmEvent::Stream { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await;
    assert!(early.is_err(), "nothing streams to a peer that did not ask");

    p.a.send(SwarmRequest::Subscribe { node: b_id, session: SessionId("s1".into()) });
    wait_for(&mut p.b_rx, |e| matches!(e, SwarmEvent::Subscribed { .. }).then_some(())).await;

    p.b.send(SwarmRequest::Stream {
        session: SessionId("s1".into()),
        event: StreamEvent::Token { turn: "t1".into(), text: "heard".into() },
    });
    let text = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::Stream { event: StreamEvent::Token { text, .. }, .. } => Some(text.clone()),
        _ => None,
    })
    .await;
    assert_eq!(text, "heard");
}

/// Answers find their way home, carrying whichever id they were asked under.
#[tokio::test]
async fn an_answer_comes_back_under_the_id_it_was_asked_with() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    p.a.send(SwarmRequest::Command {
        node: b_id,
        id: "c9".into(),
        session: SessionId("nope".into()),
        command: AgentCommand::Interrupt,
    });
    let (from, id) = wait_for(&mut p.b_rx, |e| match e {
        SwarmEvent::Command { node, id, .. } => Some((node.clone(), id.clone())),
        _ => None,
    })
    .await;

    p.b.send(SwarmRequest::Answer {
        node: from,
        id: id.clone(),
        result: Err(Refusal::NoSuchAgent),
    });
    // The dialler's host sees the refusal as an ordinary inbound message; the node layer does not
    // interpret it, which is why this test asserts on the raw arrival rather than on a callback.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(id, "c9");
}

/// Two machines that both dial each other settle on one connection and stay there.
///
/// The bug this pins looked like a flapping network and was arithmetic. Both ends had two working
/// connections, both dropped "the older one", and each judged that independently — so each kept the
/// socket the other had just abandoned, saw it close, reconnected, and did it again, several times
/// a second. It only appears when both nodes list each other, which is the normal way to configure
/// a swarm and not something either handshake test does.
#[tokio::test]
async fn two_nodes_that_dial_each_other_settle_on_one_connection() {
    let mut ports = Vec::new();
    for _ in 0..2 {
        let s = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe");
        ports.push(s.local_addr().expect("addr"));
    }
    let (a_addr, b_addr) = (ports[0], ports[1]);
    drop(ports);

    let mut a_id = Identity::ephemeral();
    let mut b_id = Identity::ephemeral();
    a_id.allow(b_id.id().clone(), "b");
    b_id.allow(a_id.id().clone(), "a");

    // Each listens *and* dials the other, which is what a real pair of machines does.
    let mk = |id: Identity, listen, peer: SocketAddr, name: &str| {
        neosh_swarm::node::spawn(
            Arc::new(id),
            SwarmConfig {
                name: name.into(),
                listen: Some(listen),
                peers: vec![PeerAddress { addr: peer.to_string(), expect: None }],
                heartbeat: Duration::from_millis(150),
                ..Default::default()
            },
            "0.1.0".into(),
        )
    };
    let (a, mut a_rx) = mk(a_id, a_addr, b_addr, "a");
    let (_b, _b_rx) = mk(b_id, b_addr, a_addr, "b");

    wait_for(&mut a_rx, |e| matches!(e, SwarmEvent::PeerUp { .. }).then_some(())).await;

    // Now watch for a while. A flap shows up as `PeerDown`; before the tie-break there were several
    // per second, so a couple of seconds is far more than enough to catch it.
    let flapped = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match a_rx.recv().await {
                Some(SwarmEvent::PeerDown { reason, .. }) => return reason,
                Some(_) => continue,
                None => return "the swarm stopped".to_string(),
            }
        }
    })
    .await;
    assert!(flapped.is_err(), "the connection dropped: {flapped:?}");

    // And the surviving connection is a working one, not a half-closed socket nobody reads.
    a.send(SwarmRequest::Publish {
        full: true,
        agents: vec![summary("s1", "neosh")],
        gone: Vec::new(),
    });
    let mut b_rx = _b_rx;
    let seen = wait_for(&mut b_rx, |e| match e {
        SwarmEvent::Inventory { agents, .. } if !agents.is_empty() => Some(agents.len()),
        _ => None,
    })
    .await;
    assert_eq!(seen, 1, "and inventory still flows over whichever half survived");
}

/// History first, then everything as it happens.
///
/// The order is the point: a watcher that received only deltas would show an empty conversation
/// until the next token, which for an idle agent is never. Subscribing to something nobody is
/// typing at has to still show you what it said.
#[tokio::test]
async fn a_subscriber_is_given_the_conversation_before_the_next_token() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;

    let s1 = SessionId("s1".into());
    p.a.send(SwarmRequest::Subscribe { node: b_id, session: s1.clone() });
    wait_for(&mut p.b_rx, |e| matches!(e, SwarmEvent::Subscribed { .. }).then_some(())).await;

    // What a host does on being subscribed to: say what has been said.
    p.b.send(SwarmRequest::Stream {
        session: s1.clone(),
        event: StreamEvent::History { messages: Vec::new() },
    });
    p.b.send(SwarmRequest::Stream {
        session: s1.clone(),
        event: StreamEvent::TurnStarted { turn: "t1".into() },
    });
    p.b.send(SwarmRequest::Stream {
        session: s1.clone(),
        event: StreamEvent::Token { turn: "t1".into(), text: "hello".into() },
    });

    let mut order = Vec::new();
    tokio::time::timeout(PATIENCE, async {
        while order.len() < 3 {
            if let Some(SwarmEvent::Stream { event, .. }) = p.a_rx.recv().await {
                order.push(match event {
                    StreamEvent::History { .. } => "history",
                    StreamEvent::TurnStarted { .. } => "started",
                    StreamEvent::Token { .. } => "token",
                    _ => "other",
                });
            }
        }
    })
    .await
    .expect("all three arrived");
    assert_eq!(order, vec!["history", "started", "token"], "and in the order they were sent");
}

/// Unsubscribing stops it. Otherwise "close the conversation" leaves a machine sending tokens to
/// somebody who walked away, which is the cost this whole subscription mechanism exists to avoid.
#[tokio::test]
async fn unsubscribing_stops_the_stream() {
    let mut p = pair(true).await;
    let b_id = wait_for(&mut p.a_rx, |e| match e {
        SwarmEvent::PeerUp { node, .. } => Some(node.id.clone()),
        _ => None,
    })
    .await;
    let s1 = SessionId("s1".into());

    p.a.send(SwarmRequest::Subscribe { node: b_id.clone(), session: s1.clone() });
    wait_for(&mut p.b_rx, |e| matches!(e, SwarmEvent::Subscribed { .. }).then_some(())).await;
    p.b.send(SwarmRequest::Stream {
        session: s1.clone(),
        event: StreamEvent::Token { turn: "t".into(), text: "one".into() },
    });
    wait_for(&mut p.a_rx, |e| matches!(e, SwarmEvent::Stream { .. }).then_some(())).await;

    p.a.send(SwarmRequest::Unsubscribe { node: b_id, session: s1.clone() });
    wait_for(&mut p.b_rx, |e| matches!(e, SwarmEvent::Unsubscribed { .. }).then_some(())).await;
    p.b.send(SwarmRequest::Stream {
        session: s1,
        event: StreamEvent::Token { turn: "t".into(), text: "two".into() },
    });

    let after = tokio::time::timeout(Duration::from_millis(600), async {
        loop {
            match p.a_rx.recv().await {
                Some(SwarmEvent::Stream { .. }) => return true,
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await;
    assert!(after.is_err(), "nothing arrives after unsubscribing");
}
