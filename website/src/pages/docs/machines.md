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

Steering an agent is a message — and answering its *questions* is one too. Approving one, or changing what it may do without asking, is a write to this machine's disk. Opening a shell is a prompt with your credentials at it and nothing above it to say no. Each is a different kind of yes, so each is asked for separately and the last two are off by default. A build machine that should be watched and not touched sets `accepts_commands = false` and is visible, read-only, to everyone.

Whatever a machine advertises, it checks again every time: a capability list is a courtesy to the other end's menus, never the enforcement.

## What you see

**A project is one row, wherever its checkouts are**, and every other computer that has it is a **block** inside it. Projects are matched by their normalised git remote rather than by path, so `/Users/me/dev/neosh` here and `/home/me/src/neosh` there are one repository:

```
 PROJECTS                       ^T
──────────────────────────────────
 ▾ neosh                         6
   ▸ Chasing the flake         12m
   ▾ ⎇ fix/composer-paste        2
     · Try the grapheme path
   ▾ @lb ⎇ main ≠ 4f3a1c2        2
     · Rework the tab bar       3m
     ▾ ⎇ fix/tab-strip           1
       · Nearly there           1h

 + Add project                  ^O

 @lb LINUX-BOX
──────────────────────────────────
 ▾ api ⎇ main                    2
   · Rate limiting              2h
   ▾ ⎇ fix/burst                 1
     · Try a token bucket      now
 ▸ infra                         4

 @ms MAC-STUDIO            offline
──────────────────────────────────
 ▸ design-site                   1
```

**Yours first, then one section per computer.** `PROJECTS` is what has a checkout on this disk. After it, every paired machine has a section of its own — its code and name, and a rule under them in its colour — holding the repositories that are **only** over there: `api` and `infra` are on `linux-box` and nowhere here. In a machine's section the machine goes without saying, so those rows are just the name and the branch its checkout is on, with its conversations and worktrees indented beneath it. The heading is a row you can stand on: `↵` folds the whole machine away, `c` and `C` connect and disconnect, `t` opens a shell in its home, and `n` starts a conversation somewhere on it. When a machine cannot be reached its heading says so — `offline`, `connecting`, `not allowed yet` — and its colour goes dim.

Inside one of your own projects, your own conversations and worktrees come first. Then each other machine that also has it is a row like a worktree — its **code**, its branch, and its name when there is room — with its conversations and its worktrees indented under it, the `·` of each conversation and the `⎇` of each worktree **in that machine's colour**, so wherever the cursor is, which computer the row is on is the colour of its mark. Another machine's work is never mixed into your own list: what it is working on was written against the code *it* has, and a row beside yours would say otherwise. `api` is a repository this machine has no clone of at all, which is still somewhere you can see and start work.

**When its checkout is on a different commit from yours**, its row says so: `≠ 4f3a1c2`, in amber. When they match, or the other machine runs a neosh too old to say, nothing is added. Both sides read their commit straight off the repository's files, with no `git` process behind it.

**Every machine has a colour of its own** — blue for the first, then violet, teal, orange, pink and lime — and it is the same colour in `^J`, in `^N`'s "which computer" and on the settings page. The colour follows the machine, not the order you paired them in, so adding a third computer never repaints the first two.

The code in front of it says the rest, and **its colour says whether you can reach it**:

| | |
| --- | --- |
| the machine's colour | connected — you can open it, steer it, start things on it |
| amber, pulsing | being dialled, or waiting for somebody to allow this computer over there |
| dim | nothing is dialling it — everything of that machine's dims with it |

The code is worked out from the machine's name — `ms` for `mac-studio`, `lb` for `linux-box`, the next candidate when two would clash — and `@` on a row about that machine, `^T` in `^J`, or the *Computers* page in settings changes it to one of your own (up to four letters or digits, kept in the `swarm.codes` workspace var). `@` because in a terminal that is already the word for *over there, on that host*: anyone who has typed `ssh you@box` reads it that way.

**A row on a machine you can reach is drawn like one of yours.** Only a machine that is not connected greys its rows, because that is the one case where the row is something you can read about and not act on.

Which computer by its full name, and what the link is doing in words, is on the **key card** — pause on the row and it appears beside the panel: `on mac-studio · connected`.

Rows for a machine that has gone quiet stay where they are. They were real a moment ago and probably still are; what changed is that you cannot reach them, and a list that silently shortens is one you cannot tell from a list that never had them.

**`↵` on a conversation over there opens it as a conversation** — in the pane, drawn exactly as one of yours is: its history as a transcript with the same cards, then its turns live, tool calls and all. **The composer is the ordinary composer**: what you type is sent to that machine and answered by the agent running there, `Esc` asks its turn to stop, and a message typed at that machine's own keyboard appears here as it is asked. The row stays lit in the project panel while you are in it, and the status line says `@lb linux-box` in that machine's colour, because the composer looks the same either way and something on screen has to say which computer your message goes to.

It is still that machine's conversation. It runs there, with that machine's files, model and permissions; nothing about it is saved here, and it does not appear among your own conversations — its row is in the machine's section, where it always was. Pictures you attach stay on this computer (the words are sent). If the machine is not connected, sending says so and leaves what you wrote in the field. An older neosh on the other end streams the words but not the tool calls; those appear when its turn ends, when the whole conversation is fetched again.

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

With a machine paired that has this project, `^N` asks **which computer** first — this one, or any of them, each with its marker and the reason it cannot be used when it cannot. `This computer` is the first row and the cursor starts on it, so `^N ⏎` is what it always was. Pick another and you get the path field pointed at that machine, which is the same thing typing `linux-box:` into it would have got you.

With no machine paired the question is not asked at all, and `^N` is the single key it has always been. Nor is it asked when the machines you have are connected but none of them has a checkout of the repository you are in: there is one possible answer, and a panel that appears to confirm it is a keypress spent on nothing.

`n` on a project that is only on other computers skips straight to it: the row already knows which machines have it and where on each, so with one there is nothing to ask.

## What moves, and what does not

An agent belongs to the machine it was started on. Its files, shell and credentials are there, so what travels is descriptions of agents and requests to their owners, never the agent itself. The owner may refuse anything, whatever it advertised.

## Networking

Plain TCP, and no NAT traversal of its own. On a LAN it needs nothing; across the internet, run Tailscale, Nebula, NetBird or ZeroTier underneath and use whatever address that gives you. Hole punching is a hard problem those projects have solved properly.
