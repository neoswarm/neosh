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

## Three kinds of trust

```toml
accepts_commands  = true   # may peers steer agents here at all
accepts_approvals = false  # may peers answer permission prompts here
accepts_shells    = false  # may peers open a shell here
```

Steering an agent is a message. Approving one is a write to this machine's disk. Opening a shell is a prompt with your credentials at it and nothing above it to say no. Each is a different kind of yes, so each is asked for separately and the last two are off by default. A build machine that should be watched and not touched sets `accepts_commands = false` and is visible, read-only, to everyone.

Whatever a machine advertises, it checks again every time: a capability list is a courtesy to the other end's menus, never the enforcement.

## What you see

**A project is one row, wherever its checkouts are.** Projects are matched by their normalised git remote rather than by path, so `/Users/me/dev/neosh` here and `/home/me/src/neosh` there are one repository — one row, with everything under it:

```
 PROJECTS
──────────────────────────────────
 ▾ neosh                         6
   ▸ Chasing the flake         12m
   · Rework the tab bar        @
   ▾ ⎇ fix/composer-paste        2
     · Try the grapheme path
   ▾ ⎇ fix/tab-strip @           1
     · Nearly there            @
 ▾ api @                         3
   · Rate limiting             @
```

Its conversations here and its conversations over there sit in the same list under it. Every other checkout — a worktree here, a worktree there, the main checkout there — is a row one level down, named by its branch. `api` is a repository this machine has no clone of at all, which is still somewhere you can see and start work.

The marker is one column of **`@`**, and its **colour is the link**:

| | |
| --- | --- |
| green | connected — you can open it, steer it, start things on it |
| amber, pulsing | being dialled, or waiting for somebody to allow this computer over there |
| dim | nothing is dialling it |

`@` rather than a cloud, because in a terminal that is already the word for it: anyone who has typed `ssh you@box` reads `@` as *over there, on that host*, while a cloud reads as a cloud *service*, which is the wrong idea about a laptop on the same desk.

A repository row wears one only when the repository is **not** here as well. What is true of nearly every row in a swarm of two machines does not earn a column on each of them; where it is news is a repository you have not cloned, which is the row you would otherwise press `↵` on expecting your own files.

Which computer, and what the link is doing in words, is on the **key card** — pause on the row and it appears beside the panel: `on mac-studio · connected`. The name is not on the row itself, because a block of twenty rows that are all on `mac-studio` does not need to say so twenty times, and the columns it used to take are the ones the conversation's own name needs.

Rows for a machine that has gone quiet stay where they are. They were real a moment ago and probably still are; what changed is that you cannot reach them, and a list that silently shortens is one you cannot tell from a list that never had them.

`↵` on a remote conversation opens it: its history, then everything as it happens. `i` says something to it and `^C` asks its turn to stop.

Three more keys work on any row that is about another machine, so the verbs are beside the thing they act on rather than behind `^J`:

| Key | Does |
| --- | --- |
| `t` | A **terminal** in this project — here, or on the machine the row is on. See below |
| `c` | Connect, or reconnect, to that machine |
| `C` | Disconnect from it. `c` takes it back |

`^J` is still where a machine is added, renamed or removed.

## A shell over there

`t` on a row, or `swarm.shell` from `^K`, opens a tab with a **shell on that machine**, in that project's directory. It is an ordinary terminal pane: every key goes to the child, `^C` interrupts *it*, `^D` ends its input, and `<C-w>` is the way out. The shell runs there and the terminal emulator runs here — which is how ssh is arranged, and for the same reasons: full-screen programs work, and the side that is drawing is the side that knows how wide it is.

It is **off by default** and asked for separately from steering:

```toml
[swarm]
accepts_commands = true    # peers may steer agents here
accepts_shells   = true    # peers may open a shell here
```

Starting an agent over there is a thing with a permission layer over it, a transcript, and somebody who can read afterwards what it did. A pty is a prompt — your shell, your environment, your credentials, and nothing above it to say no. The machine it runs on says so on screen each time one is opened, and closes every one of them when the link that asked for it goes away.

## Starting something over there

With a machine paired that has this project, `^N` asks **which computer** first — this one, or any of them, each with its marker and the reason it cannot be used when it cannot. `This computer` is the first row and the cursor starts on it, so `^N ⏎` is what it always was. Pick another and you get the path field pointed at that machine, which is the same thing typing `linux-box:` into it would have got you.

With no machine paired the question is not asked at all, and `^N` is the single key it has always been. Nor is it asked when the machines you have are connected but none of them has a checkout of the repository you are in: there is one possible answer, and a panel that appears to confirm it is a keypress spent on nothing.

`n` on a project that is only on other computers skips straight to it: the row already knows which machines have it and where on each, so with one there is nothing to ask.

## What moves, and what does not

An agent belongs to the machine it was started on. Its files, shell and credentials are there, so what travels is descriptions of agents and requests to their owners, never the agent itself. The owner may refuse anything, whatever it advertised.

## Networking

Plain TCP, and no NAT traversal of its own. On a LAN it needs nothing; across the internet, run Tailscale, Nebula, NetBird or ZeroTier underneath and use whatever address that gives you. Hole punching is a hard problem those projects have solved properly.
