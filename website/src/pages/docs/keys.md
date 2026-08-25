---
layout: ../../layouts/Docs.astro
title: Keys
description: The short version of every binding that ships. ^Z lists the live registry, and every key here is an ordinary binding your init.ts can move.
---

Every default is a key your terminal already sends: Ctrl with a letter, `⇧⇥`, `⏎`, `⌫`, `Esc`. Arrows and `PgUp`/`PgDn` work wherever they mean something and are never the only way to do anything.

## Chat

| Key | Does |
| --- | --- |
| `⏎` | Send. While a turn runs, steer it |
| `⇧⏎` | Newline, so a pasted snippet stays one message |
| `Esc` | Interrupt the turn. The conversation survives it |
| `^P` | Pick a model, mid turn too |
| `^E` | Every option this model has, applied as you move |
| `⇧⇥` | Permission mode for this conversation |
| `^N` | New conversation, with a choice of where |
| `^T` | Projects and conversations |
| `^K` | Command palette |
| `^S` | Read the transcript in normal mode |
| `^V` | Attach the image on the clipboard |
| `^G` | Git status |
| `^D` | Show what changed |
| `^L` | What the plan has left |
| `^B` | Toggle the sidebar |
| `^F` | The archive |
| `^J` | The computers in this workspace |
| `^R` | Reload configuration |
| `^Q` | Close this terminal. Everything keeps running |
| `^Z` | Every binding, live |

`/` completes a command by name. Dragging an image onto the terminal attaches it.

## Reading the transcript

`^S` puts the transcript in normal mode. The motions are Vim's and take counts: `5j`, `12G`, `^D`, `w`, `f`, `/` and `n`, `v` to select, text objects like `iw` and `i(`. Nothing edits; the transcript is an artefact you take pieces out of.

The copies specific to a transcript:

| Key | Copies |
| --- | --- |
| `y` | The selection |
| `yc` | The code block the cursor is in, without its indent |
| `ym` | The whole turn, question and answer |
| `yp` | This conversation's directory |
| `ya` | The entire transcript |

`[` and `]` jump by turn, `c` and `C` by tool call, and the card the cursor is in opens itself. `^S` again, or `i`, goes back to the composer.

## The project panel

`^T`, then Vim motions with counts. `↵` opens, `n` starts a conversation in the project under the cursor, `r` renames, `f` pins, `x` archives, `X` deletes, `y` copies the row's directory. `?` shows the keys for the row you are on.

## Answering a question

When an agent asks you something, a panel opens over the composer. `↵` takes the option under the cursor, `1` to `9` jump to one, `⇥` ticks on a multi-select, and typing is answering: your text goes into the composer and is sent instead. `Esc` tells the agent nobody answered, which it can act on.

## Rebinding

Every key is a binding to a command name in one table:

```ts
await neosh.keymap.set("chat", "<C-l>", "chat.clear");
await neosh.keymap.del("chat", "<C-o>");
```

The full table lives in [AGENTS.md](https://github.com/neoswarm/neosh/blob/main/AGENTS.md) in the repository.
