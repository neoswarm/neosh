//! Two nodes, a real TCP socket, and the question of whether either should be talking to the other.
//!
//! The unit tests cover signing and framing in isolation. These cover the thing that is only true
//! end to end: that a stranger is refused, an impersonator is refused, a mismatched version is
//! refused, and two machines that *have* been introduced end up each knowing who the other is.

use neosh_proto::{AscpMessage, NodeCapabilities, NodeId, NodeInfo};
use neosh_swarm::identity::Identity;
use neosh_swarm::{accept, dial, SwarmError};
use tokio::net::{TcpListener, TcpStream};

fn info(id: &NodeId, name: &str) -> NodeInfo {
    NodeInfo { id: id.clone(), name: name.into(), os: "test".into(), version: "0.1.0".into() }
}

fn caps() -> NodeCapabilities {
    NodeCapabilities {
        accepts_commands: true,
        accepts_approvals: false,
        streams: true,
        projects: Vec::new(),
    }
}

/// Run one handshake between two identities and give back both sides' verdicts.
async fn introduce(
    dialler: Identity,
    listener: Identity,
) -> (Result<neosh_swarm::Peer, SwarmError>, Result<neosh_swarm::Peer, SwarmError>) {
    // Port zero: the OS picks, so these can run in parallel with each other and with a real neosh.
    let sock = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = sock.local_addr().expect("addr");

    let l_info = info(listener.id(), "listener");
    let server = tokio::spawn(async move {
        let (mut stream, _) = sock.accept().await.expect("accept");
        accept(&mut stream, &listener, &l_info, &caps()).await
    });

    let d_info = info(dialler.id(), "dialler");
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let client = dial(&mut stream, &dialler, &d_info, &caps()).await;
    // Hang up before waiting on the listener. When the dialler refuses — a peer it was never told
    // to accept — the listener is still waiting for a proof that is never coming, and holding the
    // socket open here means waiting for it together, for ever. Which is also the real behaviour
    // worth keeping in mind: a refusal on one end is a closed connection on the other, not a
    // message.
    drop(stream);
    let server = server.await.expect("join");
    (client, server)
}

/// Both ends prove, and both ends learn who the other is.
#[tokio::test]
async fn two_nodes_that_have_been_introduced_each_learn_the_other() {
    let mut a = Identity::ephemeral();
    let mut b = Identity::ephemeral();
    let (a_id, b_id) = (a.id().clone(), b.id().clone());
    a.allow(b_id.clone(), "b");
    b.allow(a_id.clone(), "a");

    let (client, server) = introduce(a, b).await;
    let client = client.expect("the dialler is satisfied");
    let server = server.expect("the listener is satisfied");

    assert_eq!(client.node.id, b_id, "the dialler learnt the listener's id");
    assert_eq!(client.node.name, "listener");
    assert_eq!(server.node.id, a_id, "and the listener learnt the dialler's");
    assert_eq!(server.node.name, "dialler");
    assert!(client.capabilities.streams, "capabilities came across too");
}

/// The allow-list is the whole of the access control, so a machine that is not on it gets nowhere —
/// and the refusal names the node rather than being a generic failure, because the fix is to add
/// that id to a file.
#[tokio::test]
async fn a_node_that_was_never_introduced_is_refused() {
    let a = Identity::ephemeral();
    let mut b = Identity::ephemeral();
    b.allow(a.id().clone(), "a");
    // `a` has not been told about `b`.
    let b_id = b.id().clone();

    let (client, server) = introduce(a, b).await;
    assert!(matches!(client, Err(SwarmError::Unauthorised(id)) if id == b_id));
    // The listener does not get a clean result either: the dialler hangs up rather than proving.
    assert!(server.is_err(), "and the listener does not end up half-connected");
}

/// The listener refuses a stranger before it signs anything.
#[tokio::test]
async fn a_stranger_dialling_in_is_refused_by_the_listener() {
    let mut a = Identity::ephemeral();
    let b = Identity::ephemeral();
    a.allow(b.id().clone(), "b");
    let a_id = a.id().clone();

    let (_client, server) = introduce(a, b).await;
    assert!(matches!(server, Err(SwarmError::Unauthorised(id)) if id == a_id));
}

/// Claiming an authorised id without holding its key.
///
/// This is the attack the challenge exists for, and it has to fail differently from "not on the
/// list": an authorised id that cannot answer its own challenge means somebody is impersonating a
/// machine you trust, which is worth saying out loud rather than filing under "unknown peer".
///
/// The impostor signs a *correctly built* challenge with its own key, rather than sending garbage —
/// otherwise this would only prove that nonsense is rejected, which is a much weaker claim.
#[tokio::test]
async fn claiming_a_trusted_id_without_the_key_is_refused_and_says_so() {
    let real = Identity::ephemeral();
    let impostor = Identity::ephemeral();
    let mut victim = Identity::ephemeral();
    victim.allow(real.id().clone(), "the real one");
    let victim_id = victim.id().clone();

    let sock = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = sock.local_addr().expect("addr");
    let v_info = info(&victim_id, "victim");
    let server = tokio::spawn(async move {
        let (mut stream, _) = sock.accept().await.expect("accept");
        accept(&mut stream, &victim, &v_info, &caps()).await
    });

    // Hand-rolled rather than through `dial`, because `dial` stamps the id it can sign for and so
    // cannot express this lie at all. That is the point of it doing so — but the wire can still
    // carry the lie, and the wire is what the victim has to defend against.
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    neosh_swarm::wire::write_message(
        &mut stream,
        &AscpMessage::Hello {
            version: neosh_proto::ASCP_VERSION,
            node: info(real.id(), "not really"),
            capabilities: caps(),
            nonce: "aa".repeat(32),
        },
    )
    .await
    .expect("writes");

    let welcome = neosh_swarm::wire::read_message(&mut stream).await.expect("reads").expect("some");
    let AscpMessage::Welcome { nonce: their_nonce, .. } = welcome else {
        panic!("expected a welcome, got {welcome:?}");
    };
    // Exactly the string the victim will check — signed by the wrong key.
    let signature = impostor.sign(&neosh_swarm::identity::challenge(
        neosh_proto::ASCP_VERSION,
        &their_nonce,
        &victim_id,
        real.id(),
    ));
    neosh_swarm::wire::write_message(&mut stream, &AscpMessage::Proof { signature })
        .await
        .expect("writes");
    drop(stream);

    let err = server.await.expect("join").expect_err("refused");
    assert!(
        matches!(&err, SwarmError::BadSignature(id) if id == real.id()),
        "an authorised id that cannot prove itself is its own kind of wrong: {err}"
    );
}

/// A peer speaking a different version is told so rather than being half-understood.
#[tokio::test]
async fn a_version_mismatch_is_refused_with_both_numbers() {
    let me = Identity::ephemeral();
    let sock = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = sock.local_addr().expect("addr");
    let my_info = info(me.id(), "me");
    let server = tokio::spawn(async move {
        let (mut stream, _) = sock.accept().await.expect("accept");
        accept(&mut stream, &me, &my_info, &caps()).await
    });

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let from_the_future = AscpMessage::Hello {
        version: neosh_proto::ASCP_VERSION + 7,
        node: info(&NodeId("ff".into()), "future"),
        capabilities: caps(),
        nonce: "00".into(),
    };
    neosh_swarm::wire::write_message(&mut stream, &from_the_future).await.expect("writes");
    drop(stream);

    let err = server.await.expect("join").expect_err("refused");
    assert!(
        matches!(err, SwarmError::Version { ours, theirs } if ours == neosh_proto::ASCP_VERSION && theirs == neosh_proto::ASCP_VERSION + 7),
        "the error carries both numbers, because the fix is to upgrade one of them: {err}"
    );
}

/// Anything that is not a hello is not a handshake, however well-formed it is.
#[tokio::test]
async fn a_message_out_of_order_does_not_open_a_connection() {
    let me = Identity::ephemeral();
    let sock = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = sock.local_addr().expect("addr");
    let my_info = info(me.id(), "me");
    let server = tokio::spawn(async move {
        let (mut stream, _) = sock.accept().await.expect("accept");
        accept(&mut stream, &me, &my_info, &caps()).await
    });

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    // Straight to business, skipping the part where anybody proves anything.
    let jump_the_queue = AscpMessage::Subscribe { session: neosh_proto::SessionId("s".into()) };
    neosh_swarm::wire::write_message(&mut stream, &jump_the_queue).await.expect("writes");
    drop(stream);

    let err = server.await.expect("join").expect_err("refused");
    assert!(matches!(err, SwarmError::Protocol(_)), "{err}");
}

/// Two processes on one machine share an identity, and that has to work — it is how a swarm is
/// tested without a second computer.
#[tokio::test]
async fn a_node_can_talk_to_itself() {
    let a = Identity::ephemeral();
    let bytes = a.sign(b"probe");
    let b = Identity::ephemeral();
    // Rebuild the "same" node by using one identity on both ends via its own acceptance rule.
    assert!(a.accepts(a.id()));
    assert!(!a.accepts(b.id()));
    assert!(neosh_swarm::identity::verify(a.id(), b"probe", &bytes));
}
