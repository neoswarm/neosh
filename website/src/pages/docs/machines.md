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
4. The other machine is told that somebody is asking — see [below](#when-a-computer-asks) — and somebody there presses `^J` and allows it.

Two confirmations, one per machine, because authorising a computer to steer your agents is a decision each side gets to make. Nothing restarts, and nothing is written to your `config.toml`: paired machines live in the state directory, and `^X` in the list removes one — from the list at once, rather than leaving it there saying it is waiting. An address that turns out to be this computer is refused rather than paired with itself.

### When a computer asks

A request to join is said on the machine being asked twice, and the second one lasts. It is a **notification** — *A computer wants to join: linux-box wants to join this workspace — ^J to allow it* — which reaches you the way a finished turn does, in whichever window you are in or on the desktop when none is attached (`notify.when` calls it `pairing`). And it is a **row** at the very top of the project panel, `? linux-box wants to join`, pulsing, with `^J` on its right, for as long as the request stands. `^J` is the key either way, and it sits at the foot of the project panel as `^J computers` for anyone who has never pressed it.

Only a real request is announced. Looking at an address before pressing *Add* — which is how the name and fingerprint above are read — asks nothing of the far end, and nothing is shown there. That look used to be the one announcement a request got, a toast for six seconds before anybody had pressed anything, and every dial after it was kept quiet; a request that arrived while you were elsewhere was simply gone.

| Where | Key | Does |
| --- | --- | --- |
| the machine asked | `↵` | Allow it |
| the machine asked | `^X` | Turn it away: it leaves the list and is not announced again until somebody over there asks again |
| the machine asking | `^R` | Ask again. The other machine is told afresh, notification and all, and this one says `asked linux-box again — ^J over there allows mac-studio` |

A request nobody repeats drops off by itself after a minute and a half: a machine that has this one paired dials every few seconds, so one that has gone quiet has given up.

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

Three keys on any row:

| Key | Does |
| --- | --- |
| `↵` | Move this window to that computer — see [one window per computer](#one-window-per-computer) |
| `^R` | Dial it again now, rather than waiting out the retry delay. On a machine that has not allowed this one yet, ask again |
| `^D` | Disconnect: close the link and stop dialling, keeping the pairing. `^R` takes it back |

Disconnect holds in both directions, so a peer that dials in is turned away until you say otherwise, and it lasts until a reconnect or a restart. Unpairing with `^X` is the stronger verb, for a machine you are done with rather than done with for now.

## Three kinds of trust

```toml
accepts_commands  = true   # may peers steer agents here at all
accepts_approvals = false  # may peers answer permission prompts here
accepts_shells    = false  # may peers open a shell here
```

Steering an agent is a message — and answering its *questions* is one too. Approving one, or changing what it may do without asking, is a write to this machine's disk. Opening a shell is a prompt with your credentials at it and nothing above it to say no. Each is a different kind of yes, so each is asked for separately and the last two are off by default. A build machine that should be watched and not touched sets `accepts_commands = false` and is visible, read-only, to everyone.

Whatever a machine advertises, it checks again every time: a capability list is a courtesy to the other end's menus, never the enforcement.

## What you see

**A terminal is for one computer, and its project panel is that computer's list.** In a window for this one, `PROJECTS` is what has a checkout on this disk and nothing else: another machine's work is not mixed into it, not hung under it, and not in a section below it. There was once a window for every computer at once, and it was a column in which nothing said which rows were yours.

A window for another computer is drawn as though it were the one you are sitting at:

```
 PROJECTS  @lb linux-box        ^T
──────────────────────────────────
 ▾ api ⎇ main                    2
   · Rate limiting              2h
   ▾ ⎇ fix/burst                 1
     · Try a token bucket      now
 ▸ infra                         4

 + Add project                  ^O
```

Which computer is said once, beside the heading — its code in its colour, its name dim — and when it cannot be reached the right-hand side says so instead: `offline`, `connecting`, `not allowed yet`. The heading is a row you can stand on: `c` and `C` connect and disconnect, `t` opens a shell in its home, and `n` starts a conversation somewhere on it. Each row below is a repository and the branch its checkout is on, with its conversations and worktrees indented beneath it, in the colours your own rows wear. `+ Add project` adds one **on that machine**. `^J` and `/device` are how a window moves from one computer to another — [below](#one-window-per-computer).

**Every machine has a colour of its own** — blue for the first, then violet, teal, orange, pink and lime — and it is the same colour in `^J`, in the *which computer?* a new terminal asks, on the status line and on the settings page. The colour follows the machine, not the order you paired them in, so adding a third computer never repaints the first two.

The code in front of it says the rest, and **its colour says whether you can reach it**:

| | |
| --- | --- |
| the machine's colour | connected — you can open it, steer it, start things on it |
| amber, pulsing | being dialled, or waiting for somebody to allow this computer over there |
| dim | nothing is dialling it — everything of that machine's dims with it |

The code is worked out from the machine's name — `ms` for `mac-studio`, `lb` for `linux-box`, the next candidate when two would clash, the same in every window — and `@` on a row about that machine, `^T` in `^J`, or the *Computers* page in settings changes it to one of your own (up to four letters or digits, kept in the `swarm.codes` workspace var). `@` because in a terminal that is already the word for *over there, on that host*: anyone who has typed `ssh you@box` reads it that way.

**A row on a machine you can reach is drawn like one of yours.** Only a machine that is not connected greys its rows, because that is the one case where the row is something you can read about and not act on.

Which computer by its full name, and what the link is doing in words, is on the **key card** — pause on the row and it appears beside the panel: `on mac-studio · connected`.

Rows for a machine that has gone quiet stay where they are. They were real a moment ago and probably still are; what changed is that you cannot reach them, and a list that silently shortens is one you cannot tell from a list that never had them.

**`↵` on a conversation over there opens it as a conversation** — in the pane, drawn exactly as one of yours is: its history as a transcript with the same cards, then its turns live, tool calls and all. **The composer is the ordinary composer**: what you type is sent to that machine and answered by the agent running there, `Esc` asks its turn to stop, and a message typed at that machine's own keyboard appears here as it is asked. The row stays lit in the project panel while you are in it, and the status line says `@lb linux-box` in that machine's colour, because the composer looks the same either way and something on screen has to say which computer your message goes to.

It is still that machine's conversation. It runs there, with that machine's files, model and permissions; nothing about it is saved here, and it does not appear among your own conversations — its row is in that machine's list, in a window for that machine. Pictures you attach stay on this computer (the words are sent). If the machine is not connected, sending says so and leaves what you wrote in the field. An older neosh on the other end streams the words but not the tool calls; those appear when its turn ends, when the whole conversation is fetched again.

**And the rest of the keyboard works on it too**, each key doing to that conversation what it does to one of yours — over there:

| Key | In another machine's conversation |
| --- | --- |
| `⏎` `Esc` | Send, steer, and interrupt, as above |
| `^P` | Pick the model **that machine** uses for it — from **its** providers and **its** models, the list its own `^P` shows, not this machine's. The footer shows the model it is using, and follows if somebody changes it at that keyboard |
| `^E` | That model's settings — effort, thinking and the rest — as that machine offers them, applied over there |
| `⇧⇥` | Its permission mode — only when that machine sets `accepts_approvals`, since full access is every future prompt answered yes. Otherwise it says so and changes nothing |
| `^N` | A new conversation **on that machine**, starting from this one's directory: `^N ⏎` is a conversation beside this one, as it is on yours |
| `<C-w>T` `<C-w>S` | A shell **on that machine**, in this conversation's directory. Needs `accepts_shells` there, like `t` |
| rename, archive | Renamed or archived there. Deleting is refused: it is that machine's to do |

The footer's context meter is that conversation's too, read off the machine it runs on. Signing in to a provider (`^S` in the picker) is refused here, because the providers in the list are that machine's and so is the key: sign in there. All of this needs a current neosh at both ends; an older one on the other machine still shows the name of its model, but `^P` lists this machine's.

**When its agent asks you something, it asks you here.** A question — *which database?* — opens the same panel a question from one of your own agents does, in the pane that is showing the conversation, and the answer goes back to the agent that asked. It is offered to that machine's own screen at the same moment; whichever is answered first is the answer, and the other panel takes itself down. A **permission prompt** comes across the same way when that machine sets `accepts_approvals`, because a yes is a write to its disk. When it does not, the prompt is named in the corner — `linux-box is asking: Run cargo publish?` — and answered there.

Three more keys work on any row that is about another machine, so the verbs are beside the thing they act on rather than behind `^J`:

| Key | Does |
| --- | --- |
| `t` | A **terminal** in this project — here, or on the machine the row is on. See below |
| `c` | Connect, or reconnect, to that machine |
| `C` | Disconnect from it. `c` takes it back |
| `@` | Choose the short code that machine is drawn with |

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

`^N` starts where the window is, and never asks *which computer*: the terminal already said. In a window for linux-box it is a conversation on linux-box, and in a conversation over there `^N ⏎` is a new one beside it.

**A project over there asks what a project here asks.** `n` on one of its rows, or `^N` in one of its conversations, offers the same rows in the same order — here, a new worktree named for you, one kept inside the project, one you name, a tree it already has, somewhere else — and every one of them is answered by that machine: its worktrees, its branch names, its `worktree.root`, the tree made on its disk. `p` pulls over there. What lands is that machine's conversation, opened here.

The path field — `^N`'s *Another directory…*, and `^O` — completes on the window's computer. Typing a machine's name and a colon, `linux-box:`, completes on that one instead (the hint strip says so when a machine is paired), and an address after it, `linux-box:https://github.com/owner/repo`, clones onto it. In a window for that machine, `^O` is already pointed there and a pasted address clones there.

All of this needs a current neosh at both ends. An older one on the other machine still takes a conversation in a directory you name, and says nothing about worktrees.

## One window per computer

A terminal is *for* one machine — a terminal on the build box beside one on your laptop. Its project panel shows only that machine, it opens in that machine's newest conversation, and `^N` starts there.

```bash
neosh ls                  # every computer, its code, whether it is up, and which one `neosh` opens on
neosh connect lb          # a terminal on linux-box — by its code, name, the start of its name, or id
neosh connect here        # this computer
neosh connect             # ask, whatever the setting says
```

`neosh connect` settles the name and waits for the link *before* taking the screen, so a name that matches nothing, or a machine that has not allowed this one, is a sentence in your shell rather than an empty window. A machine nobody is dialling is dialled.

Plain `neosh` asks **which computer?** whenever another one is paired, whether or not it is connected yet: this computer first, then each paired machine in its colour, redrawing as they connect — choosing one still being dialled waits for it. `↵` opens, `d` opens *and* makes it the default so the question is not asked again, and `esc` is this computer. With nothing paired there is nothing to ask, and the terminal is this computer's.

There is no answer for every computer at once any more. `all` — in `swarm.device`, `neosh connect all` or `/device all` — is read as asking, the nearest thing to not having picked one.

The default is `swarm.device` — `ask`, `here`, or a computer's name or code — and lives on the **Computers** page of `/settings`, where `↵` offers the list:

```toml
[options]
"swarm.device" = "linux-box"
```

Inside, `/device` asks again for this terminal, and `/device lb` and `/device here` answer without asking. So does `^J`: `↵` on a machine moves this window to it — dialling it if nothing is, and landing in its newest conversation — and `↵` on **This computer** brings it back. The row for the computer this window is for says `this window`. A window for another computer looks like your own: the heading is `PROJECTS`, with `@lb linux-box` dim beside it, its projects are drawn the way yours are, and `+ Add project` adds one on that machine. Nothing stops when a window is pointed elsewhere: a turn on a machine this window is not for keeps running, and its notifications still arrive.

For a plugin it is `neosh.view.device()`, `neosh.view.setDevice()` and `neosh.view.onDevice()` — the same answer the project panel and `^N` read.

## What moves, and what does not

An agent belongs to the machine it was started on. Its files, shell and credentials are there, so what travels is descriptions of agents and requests to their owners, never the agent itself. The owner may refuse anything, whatever it advertised.

## Networking

Plain TCP, and no NAT traversal of its own. On a LAN it needs nothing; across the internet, run Tailscale, Nebula, NetBird or ZeroTier underneath and use whatever address that gives you. Hole punching is a hard problem those projects have solved properly.
