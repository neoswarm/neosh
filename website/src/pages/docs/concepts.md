---
layout: ../../layouts/Docs.astro
title: Concepts
description: The five ideas the whole workspace is built on. Five minutes, and everything else in these docs will make sense.
---

## The workspace is a process

`neosh` attaches your terminal to the workspace for your config directory, starting one if there is none. Closing a terminal detaches; every turn keeps running. `neosh stop` is what ends a workspace.

Several terminals can attach to the same workspace at once. Each has a view of its own, with its own conversation on screen, its own scroll and its own panels, while the conversations and the turns running in them are shared. Leave mid answer, come back later, and the answer is still arriving.

## Conversations and turns

A conversation belongs to a directory, and that directory is where its work happens: the files the agent reads, the commands it runs, the repository git answers about.

- `⏎` sends a message. While a turn is running it steers instead: the message is taken in at the agent's next gap.
- `Esc` interrupts the turn. The agent is asked to stop, so the conversation survives it.
- A turn that finishes while you are elsewhere marks its conversation unread. Arriving clears the mark; there is nothing to dismiss.
- Switching conversations is never refused. Turns keep running where they are.

### The transcript keeps time

Every turn carries the time in the right margin of the blank row above its question:

```
                                                     8 Sep 14:32  ·  4m 12s
▌ why is the composer eating pasted newlines

  Looking at the paste path…
```

When it was asked, and — once it has ended — how long it took. The newest turn also says how long ago it finished (`·  12m ago`), which is what you want when you walk back to a terminal that has been sitting there; it is kept true while it is the newest, and settles the moment you ask something below it. Every turn above says only things that are true forever, because those rows are written once and nothing comes back to them.

The date leads the margin only when the day changed since the turn above — a whole afternoon's work is one date and a column of clock times — and the year only when that changed too. `chat.clock = "12h"` writes `2:32pm` instead.

It is drawn beside the transcript rather than into it, so `y`, `ym` and `ya` copy what was said and never what neosh wrote in the margin about it. `chat.times = false` turns it off.

Durations read the same wherever the workspace prints one: seconds under a minute, `4m 12s` under an hour, `2h 30m` past that — on the working line while a turn runs, on a tool card, on a conversation's row in the sidebar, and here.

## Projects and worktrees

The sidebar (`^T`) groups conversations by project. A worktree nests under the repository it belongs to, named by its branch, so four scratch trees of one repository are one project, not four.

`^N` starts a conversation and asks where: here, a fresh worktree, an existing one, another machine, or elsewhere. A worktree you did not name gets its branch named from your first message.

## Permissions and questions

Each conversation owns its permission mode: full access, ask, allow listed, or deny. `⇧⇥` cycles it, the choice is saved with the conversation, and it takes effect on the turn that is running.

Anything irreversible asks first, and nothing reversible does. When an agent has a question for you, it appears as a panel over the composer: pick an option, or just type, since typing is answering.

## Everything is a plugin

Every feature ships on the same public API a third party plugin uses. The chat, the sidebar, git, the palette: all plugins, all defaults, and a default is something you turn off.

That means the workspace is yours to reshape. Disable any bundled piece with `plugins.disabled`, replace it with a plugin of your own, or rebind any key from `init.ts`. [Plugins](/docs/plugins) shows how.
