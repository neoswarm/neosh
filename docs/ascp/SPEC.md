# ASCP — Agent Swarm Control Protocol

**Version 0.** Draft: the message types are settled enough to implement against and not yet
stable enough to promise.

One person, several computers, agents running on all of them. ASCP is what makes that one
workspace: every node knows what every other node is running, and can ask a node to do something
with an agent it owns.

The normative definition of every message is
[`crates/neosh-proto/src/ascp.rs`](../../crates/neosh-proto/src/ascp.rs). The TypeScript types under
`plugins/api/src/generated/` are emitted from it, and CI fails on drift — so this document describes
a protocol that exists rather than one somebody intends to build.

---

## 1. What ASCP is not

**It does not move agents.** An agent belongs to the node it was started on for the whole of its
life. Its files are there, its shell is there, its credentials are there; a conversation that could
migrate would be one whose `cwd` means something different afterwards. What travels is a
*description* of an agent and *requests* addressed to its owner, and the owner may always refuse.

**It does not do NAT traversal.** ASCP is an ordinary TCP protocol. On a LAN it needs nothing;
across the internet you run an overlay network — Tailscale, Nebula, NetBird, ZeroTier — underneath
it, and ASCP sees ordinary addresses. Hole-punching is a hard problem several mature projects have
already solved, and an agent protocol that reimplemented it would be doing their job badly instead
of its own.

**It does not trust the network.** An overlay gets packets to you; it does not decide who may steer
your agent. Authorisation is ASCP's own, and stays ASCP's own even when the transport is encrypted.

**It has no control plane.** No coordinator, no directory, no registry. A node knows the peers it
was told about, and that is all there is.

---

## 2. Identity and authorisation

Every node has an ed25519 keypair. The public half, hex-encoded, **is** the `NodeId`.

That equivalence is the point: there is no lookup between "who you say you are" and "the key that
proves it", so there is nothing to poison. There is no certificate authority, because for a
workspace that is one person's several computers a CA is infrastructure to run in exchange for
nothing.

**Authorisation is a list you wrote.** A peer whose id is not in the local list is refused during
the handshake, before it can say anything else. Adding a machine to a swarm is adding a line;
removing one is deleting a line.

The key is generated on first use — a protocol whose first step is "run keygen" is one people do not
reach the second step of — and stored `0600`. A key file found with looser permissions is **refused,
not used**: a private key the rest of the machine can read is not a private key, and continuing
would mean the authorisation list is guarding nothing.

---

## 3. Framing

Four bytes of big-endian length, then that many bytes of UTF-8 JSON. One message per frame.

Chosen over anything cleverer for one reason: you can read a capture of it. A swarm protocol is
debugged across machines you are not sitting at, and a binary encoding turns "what did it actually
send" into a tooling problem at precisely the moment you have no tooling.

Frames are capped at **8 MiB**. The cap is not tidiness — the length arrives *before* the handshake
finishes, so without it four bytes from anything that can reach the port is a request to allocate
arbitrary memory. A frame over the cap fails the connection rather than being skipped: a peer that
sent it is broken or hostile, and reading past it means trusting the same field again.

---

## 4. Handshake

Mutual challenge–response, the shape SSH uses. The dialler goes first.

```
dialler                                   listener
   |  Hello    { version, node, caps, nonce_d }   ->  |
   |                                                   |  reject unless node.id is authorised
   |  <-  Welcome { version, node, caps, sig_l, nonce_l }
   |  verify sig_l over challenge(nonce_d)            |
   |  reject unless node.id is authorised             |
   |  Proof    { sig_d }                          ->  |
   |                                                   |  verify sig_d over challenge(nonce_l)
   |            ...symmetric from here...              |
```

The signed string is

```
ascp/<version>\n<nonce>\n<dialler-id>\n<listener-id>
```

Signing a bare nonce would prove only that somebody holds a key. Signing *this* proves they meant to
authenticate to **this** node, for **this** protocol version — so a signature captured from one
exchange cannot be replayed into another between different machines.

Three properties, each of which is a thing that goes wrong when it is missing:

- **Both ends prove.** A one-sided handshake authenticates the caller to the callee and leaves the
  caller talking to whoever answered the port.
- **The challenge names both nodes and the version**, per above.
- **Authorisation is checked twice** — before a signature is requested, to avoid spending
  verification on a peer that would be refused anyway, and after it verifies, which is the check
  that counts, because until then the claimed id is only a claim.

**Version mismatch is refused, not negotiated.** A protocol that silently degrades is one where a
feature the other machine lacks looks like a bug in the other machine.

Two refusals are deliberately distinct. `Unauthorised` is the ordinary answer to a stranger.
`BadSignature` means an id *on your list* could not answer its own challenge — somebody is
impersonating a machine you trust, which deserves to be said out loud rather than filed under
"unknown peer".

---

## 5. Messages

After the handshake the connection is symmetric: either side may send anything. Which side dialled
is an accident of who booted first.

| Family | Volume | When |
|---|---|---|
| `Presence` | one per node per heartbeat | always |
| `Inventory` | one per change | always |
| `Command` / `Ack` / `Refused` | one per meaningful keystroke | on demand |
| `Subscribe` / `Stream` | **one per token** | only while subscribed |

That last row is the whole performance argument. Twenty machines streaming every token to everyone
is a great deal of traffic to render a board nobody is reading. Inventory is cheap and
unconditional; a stream is something you ask for when you open a conversation and drop when you
leave it.

### Presence

Heartbeat, plus anything about the node that can change while it runs. Carries a `seq` that is
**monotonic per node, not a timestamp** — clocks on different machines disagree by more than the
interval between two heartbeats, and "newest wins" decided by wall clock is how a stale inventory
permanently beats a fresh one.

### Inventory

What a node is running, as `AgentSummary` values. `full: true` is a complete snapshot that replaces
whatever the receiver had; otherwise `agents` are additions-or-updates and `gone` are removals. A
node sends a full snapshot on connect and after any change it could not express as a delta.

An agent is addressed as **`(node, session)`**. A session id is unique on its own node and nowhere
else; two machines restoring from one backup would otherwise be a collision away from steering each
other's work.

`cwd` is a path **on the owning node**. It is for showing and for handing back in `NewSession` —
never for opening locally, where it may not exist at all.

### Command

Every command is a *request*, answered exactly once with `Ack` or `Refused`. A command that could
time out silently is a keystroke you cannot tell from a dropped connection.

| Command | |
|---|---|
| `Send` | Say something. While a turn runs this steers it, as typing there would |
| `Interrupt` | Ask the turn to stop; the conversation survives |
| `Approve` | Answer a permission prompt |
| `SetModel` | Change the model for the next turn, and tell a running one |
| `Rename`, `Archive` | |
| `NewSession` | Start a conversation on that node, in a `cwd` it told you about |

`Approve` is the sharpest thing in the protocol: approving a tool call on another machine authorises
a write to *that machine's* filesystem. Sound when the machines are all yours, and something a node
must be able to say no to — hence `NodeCapabilities::accepts_approvals`, separate from
`accepts_commands`, because steering an agent is a message and approving one is a write.

Capabilities are advisory **to the caller** — they let a board grey out a verb rather than offering
one that will be refused. The owner enforces regardless, because a capability list is something the
other side could have lied about.

### Stream

What a subscriber sees while watching one agent: `History` once on subscribe, then `Token`,
`Thinking`, `Activity`, `TurnStarted` / `TurnEnded`, and `Blocked` when it needs a person.

`History` exists because a subscriber that received only deltas would show an empty conversation
until the next token, which for an idle agent is never.

`Activity` is carried separately from `Token` for the same reason it is locally: a driver's account
of its own loop is not a content block.

---

## 6. Project identity across machines

The hard part of "this project is on three computers" is that the path is not the same on any of
them — `/Users/me/dev/neosh`, `/home/me/src/neosh`, `C:\code\neosh`. Comparing paths answers the
wrong question; comparing directory names says two unrelated `api` directories are one project.

So a `ProjectKey` is the repository's origin URL, normalised:

```
git@github.com:neoswarm/neosh.git      ┐
https://github.com/neoswarm/neosh      ├─►  git:github.com/neoswarm/neosh
ssh://git@github.com:22/neosh.git      ┘
```

Normalisation strips the transport, any credentials, the port and a trailing `.git`. It does **not**
fold case: forges differ on whether they care, and folding would merge two repositories a
case-sensitive host considers separate.

A directory with no remote falls back to `dir:<basename>`, and `ProjectKey::is_certain()` is false
for it. That is deliberate — it is a guess, and a guess about identity should be visibly a guess
rather than quietly wrong.

Worktrees of one repository share a key. They are the same project checked out twice, which is what
lets a board say "neosh: on mac-studio and linux-box" rather than listing four rows that are all
really one thing.

---

## 7. Conformance

An implementation is ASCP-0 conformant if it:

1. Refuses any peer whose `NodeId` is not locally authorised, at the handshake.
2. Verifies the peer's signature over the challenge string in §4, exactly.
3. Refuses a `version` it does not equal.
4. Refuses a frame larger than 8 MiB without allocating it.
5. Answers every `Command` exactly once with `Ack` or `Refused`.
6. Never acts on a command it did not authorise, whatever it advertised in `NodeCapabilities`.
7. Sends a full `Inventory` on connect.
8. Sends `Stream` only for sessions the peer has subscribed to.

Points 1, 2 and 6 are the security-relevant ones. An implementation failing any of them is not a
degraded ASCP node; it is an open door.

---

## 8. Test vectors

[`vectors/`](vectors/) holds a canonical encoding of every message here, generated by
`crates/neosh-proto/tests/ascp_vectors.rs` rather than typed out beside the prose — so a change to a
message type that nobody updated this document for shows up as a diff. That test also asserts they
still match, which is what makes them a drift check rather than a script.

Regenerate with `NEOSH_WRITE_VECTORS=1 cargo test -p neosh-proto --test ascp_vectors`.

## 9. Where this lives

The protocol has a repository of its own: **<https://github.com/neoswarm/ascp>**, with this
specification, a conformance checklist, the generated types and the vectors. An implementation in
another language should start there rather than vendoring an editor to get a schema.

This copy is the one the reference implementation is checked against, and the two are kept in step.

## 10. What is not implemented here

Declared in the protocol and refused by name on this node, because refusing is the honest answer and
a silent success is not:

- **`AgentCommand::Approve`.** On neosh a permission prompt is a *blocking plugin hook* — the
  approvals plugin owns both the question and the answer, and the host has nothing to say yes with.
  Answering it properly means a plugin registering as the swarm's approver, which is a surface that
  does not exist yet. Until it does, approving a tool call from another machine is refused with a
  reason rather than accepted and dropped.
- **`AgentState::Blocked`** is never reported for the same reason: the host does not know a prompt
  is pending, because the plugin holding it has not been asked.

Both are in the protocol because the shape is right and another implementation may well have the
prompt in a place that can answer.
