---
layout: ../../layouts/Docs.astro
title: Start Guide
description: neosh is a terminal workspace for running coding agents, built the way Neovim is built. This guide takes you from install to your first conversation.
---

## Install

One command builds neosh from source and puts it on your path:

```sh
curl -fsSL https://neoswarm.dev/install.sh | sh
```

Homebrew, Cargo and a plain source build are covered in [Installation](/docs/installation).

## Start the workspace

```sh
neosh
```

That starts a workspace and attaches your terminal to it. No API key is needed: an existing `claude` login works as is, and hundreds of models are reachable before you configure anything.

Three things worth knowing on day one:

1. The workspace is a process, and your terminal is a view of it. Closing the terminal detaches, turns keep running, and running `neosh` again puts you back.
2. `neosh stop` is what actually ends a workspace. `^Q` never can.
3. `neosh init` writes a starter config you can edit later. You do not need it to begin.

## Your first conversation

1. Type a message and press `⏎`. That starts a turn.
2. While the turn runs, `⏎` steers it: your message is held and taken in at the agent's next gap.
3. `Esc` interrupts. The agent is asked to stop, so the conversation survives it.
4. `^P` picks a different model, even mid turn.
5. `^N` starts a new conversation. In a repository it asks where: right here, a fresh worktree, an existing one, another machine, or somewhere else.

A worktree you did not name gets its branch named from your first message, so `fix/composer-paste-truncation` appears instead of a random placeholder.

## Find your way around

| Key | Opens |
| --- | --- |
| `^T` | Projects and conversations |
| `^K` | The command palette |
| `^S` | The transcript in normal mode, for moving and copying with Vim motions |
| `^Z` | Every key binding, read from the live registry |

## Next steps

- [Concepts](/docs/concepts) explains the workspace model in five minutes.
- [Keys](/docs/keys) is every binding that ships — all of them defaults you can move.
- [Scripting](/docs/scripting) is `neosh agent`: running many conversations at once, from a shell or another coding agent.
- [Configuration](/docs/configuration) covers `init.ts` and `config.toml`.
- [Plugins](/docs/plugins) shows how to write and install one.
