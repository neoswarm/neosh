# 0041 — An agent belongs to its machine, and the swarm is a view of it

**Status:** accepted

Introduces ASCP. Extends [0036](0036-the-workspace-outlives-the-terminal.md), which separated the
workspace from the terminal viewing it, to the case where the workspaces are on different computers.

## Context

One person, several machines, agents on all of them. Today that is several workspaces, and knowing
what the laptop is doing means walking over to the laptop.

The tempting design is to make agents mobile — one workspace, agents that run wherever there is
capacity. It is wrong, and worth saying why before anything else: an agent's value is bound to a
filesystem, a shell, a set of credentials and a checkout. A conversation that could migrate is one
whose `cwd` means something different afterwards, whose tool calls edit a different copy of the
repository, and whose "run the tests" ran them somewhere you were not looking. The thing people
actually want is not a mobile agent; it is to *see* and *steer* an agent that is firmly somewhere
else.

The second temptation is to solve the network. Getting two machines to talk across the internet is
a genuinely hard problem — NAT, CGNAT, hole-punching, relays — and it is a solved one:
[Tailscale](https://tailscale.com/blog/nat-traversal-improvements-pt-1) spent years on it and
shipped peer relays in 2026; iroh rebuilt it on QUIC; Nebula does it with lighthouses. An agent
workspace that reimplemented that would be doing three mature projects' job badly instead of its
own.

The third is to trust the network. An overlay network gets packets to you. It does not decide who
may steer your agent, and a protocol that assumed otherwise would be one where joining a Tailnet is
the same as handing out shell access.

## Decision

**An agent belongs to the node it was started on, for the whole of its life.** What travels is a
*description* of an agent and a *request* addressed to its owner. Every request may be refused, and
the owner enforces regardless of what it advertised.

**ASCP is plain TCP over whatever network you have.** On a LAN, nothing. Across the internet, an
overlay underneath. `docs/ascp/SPEC.md` says so normatively, because an implementation that added
its own hole-punching would be a different protocol.

**Authorisation is ASCP's own, and is a list you wrote.** A node's ed25519 public key *is* its id,
so there is no lookup between "who you say you are" and "the key that proves it" and nothing to
poison. No CA: for one person's several computers, that is infrastructure in exchange for nothing.
The handshake is a mutual challenge whose signed string names the nonce, both ids and the version,
so a signature cannot be replayed between machines.

**Steering and approving are different kinds of trust.** `accepts_commands` and `accepts_approvals`
are separate settings and the second is off by default: steering an agent is a message, approving a
tool call is a write to that machine's disk.

**Projects are matched by normalised git remote.** The path is different on every machine, so
comparing paths answers the wrong question and comparing directory names says two unrelated `api`
directories are one project. A directory with no remote falls back to its name and reports itself
as a guess, because a guess about identity should look like one.

**Streams are subscribed, everything else is not.** Presence and inventory are cheap and
unconditional. Tokens are one message per token per watcher, so they flow only to whoever asked —
which is what keeps a twenty-machine swarm quiet while still letting one remote conversation read
exactly like a local one.

**The capability is a crate; the interface is a plugin.** Per [ADR 0016](0016-capabilities-in-core-interface-in-plugins.md):
`neosh-swarm` owns sockets and signatures, the host owns the picture, and every decision about what
to draw is the sidebar plugin's, made against `neosh.swarm.*` — a surface a third party could write
a different board on.

## Consequences

**Remote conversations sit under the same project as local ones**, dimmed, with the host on the end
of the row, rather than in a section of their own. They are not a different kind of thing; they are
the same work on a different computer, and a separate `REMOTE` block would make you check two places
for one project. A project row carries the names of the other machines that have it.

**A peer that goes quiet keeps its rows**, marked unreachable. They were real a moment ago and are
probably still running; what changed is that we cannot see them. A board that empties a machine's
rows on a wifi hiccup is one that makes you think work was lost. Those rows leave the *actionable*
list, so nothing offers you a verb that cannot reach anything.

**Both ends dialling each other is the normal configuration, and it needed a tie-break.** Two nodes
that list each other end up with two working connections and something has to drop one — and both
ends must drop *the same one*. The first implementation kept whichever arrived later, judged
independently on each side, so each kept the socket the other had just abandoned, saw it close,
reconnected, and did it again several times a second. It looked like a flapping network and was
arithmetic. The survivor is now the connection dialled by whichever node has the smaller id: no
clocks, no ordering, no negotiation, and both sides compute it identically. This only appears with a
real pair of machines, which is why it survived a full handshake test suite and died the first time
two workspaces were started for real.

**`neosh swarm-id` exists because a public key cannot be read out of a private one by looking at
it.** Setting up a swarm means each machine having the other's id, and a swarm you cannot join
without writing a script is a swarm nobody joins.

**Changing `[swarm]` takes a restart, not `^R`.** The allow-list and the listening socket are decided
at boot; re-reading them on reload would tear down live connections whenever somebody changed a
colour. Saying so is kinder than a reload that half-works.

**`SetModel` and `Approve` are declared and refused by name.** They are in the protocol because the
shape is right and not yet carried out by this host. Refusing explicitly is the honest answer — a
silent success would be a remote machine believing it had approved a tool call when it had not.
