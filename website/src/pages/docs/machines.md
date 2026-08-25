---
layout: ../../layouts/Docs.astro
title: Machines
description: Several computers, one workspace. Every one knows what the others are running, and can ask them to do something. On by default, and joining still takes a yes on both machines.
---

The swarm is on by default: this machine has an identity, dials the machines it is paired with, and listens on `0.0.0.0:7717`, so adding it from another computer works before anything is configured. What stays consent is membership. A machine not on the allow-list gets nowhere regardless of what it can reach.

```toml
[swarm]
enabled = false    # the whole switch
listen  = ""       # or just dial-only: joins machines without being joinable
```

## Pair two machines

1. Press `^J` on either machine and pick **Add a computer**.
2. Type the other machine's address. You are shown what is actually there: its name, version and key fingerprint, read from the far end rather than typed by you.

   ```
   Add linux-box?

   127.0.0.1:7739 · macos · neosh 0.1.0
   1ca3 c856 8f4c 6584
   Check that fingerprint matches what that computer shows under `This computer`.
   ```

3. Check the fingerprint against what that computer shows under **This computer**, then confirm.
4. The other machine announces that somebody is asking, and somebody there presses `^J` and allows it.

Two confirmations, one per machine, because authorising a computer to steer your agents is a decision each side gets to make. Nothing restarts, and nothing is written to your `config.toml`: paired machines live in the state directory, and `^X` in the list removes one.

## Or by hand

`neosh swarm-id` prints this machine's identity and the lines to paste elsewhere:

```toml
[swarm]
name = "mac-studio"           # what the others call you; the hostname if unset
listen = "100.71.4.9:7717"    # omit to be dial-only

[[swarm.peers]]
addr = "linux-box:7717"
id   = "453ff38823de6515…"    # from `neosh swarm-id` over there
name = "linux-box"
```

`id` is the authorisation. An address says where to look; the key says who it is, and a machine that cannot prove that key is refused however it reached you. A node is its ed25519 public key, and the network is never trusted to say who may steer an agent.

A peer entry you wrote by hand cannot be removed with `^X`, because that line would come back on the next reload. Changing `listen`, `name` or the trust settings takes a restart; pairing itself takes effect immediately.

## Watching a connection

Every paired machine is a row in `^J` from the first frame, and the row says where the link stands: `connecting…` for one being dialled that has never answered, with a `try 4` once it has failed a few times, `reconnecting` for one that was here and dropped, `disconnected` for one nothing is dialling, and the conversation count for one that is up. Reconnects back off from a second up to half a minute and reset the moment a dial lands, so a machine that reboots is back in seconds. The list updates live while it is open, and the `This computer` row says whether this machine is `listening`, `not listening`, or `dial-only`.

Two verbs on any row:

| Key | Does |
| --- | --- |
| `^R` | Dial it again now, rather than waiting out the retry delay |
| `^D` | Disconnect: close the link and stop dialling, keeping the pairing. `^R` takes it back |

Disconnect holds in both directions, so a peer that dials in is turned away until you say otherwise, and it lasts until a reconnect or a restart. Unpairing with `^X` is the stronger verb, for a machine you are done with rather than done with for now.

## Two kinds of trust

```toml
accepts_commands = true    # may peers steer agents here at all
accepts_approvals = false  # may peers answer permission prompts here
```

Steering an agent is a message. Approving one is a write to this machine's disk, which is why it is separate and off by default. A build machine that should be watched and not touched sets `accepts_commands = false` and is visible, read-only, to everyone.

## What you see

Conversations on other machines appear in the sidebar under the same project as yours, dimmed, with the host at the end of the row. Projects are matched by normalised git remote, not by path, so `/Users/me/dev/neosh` here and `/home/me/src/neosh` there are one project.

`↵` on a remote conversation opens it: its history, then everything as it happens. `i` says something to it and `^C` asks its turn to stop. `^N` offers the other machines' checkouts alongside your worktrees, so "start something over there" is one key.

## What moves, and what does not

An agent belongs to the machine it was started on. Its files, shell and credentials are there, so what travels is descriptions of agents and requests to their owners, never the agent itself. The owner may refuse anything, whatever it advertised.

## Networking

Plain TCP, and no NAT traversal of its own. On a LAN it needs nothing; across the internet, run Tailscale, Nebula, NetBird or ZeroTier underneath and use whatever address that gives you. Hole punching is a hard problem those projects have solved properly.
