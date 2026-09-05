---
layout: ../../layouts/Docs.astro
title: Options
description: Every setting is declared, typed, and owned. You cannot set one that does not exist, and you cannot set the wrong type. Both are errors that name the problem.
---

## How options work

Set them in `config.toml` under `[options]`, or from code:

```ts
await neosh.opt.set("chat.show_thinking", true);
const on = await neosh.opt.get<boolean>("chat.show_thinking");
await neosh.opt.all();                    // everything declared, with types and defaults
neosh.opt.onChange((e) => { /* fires for every option, not just yours */ });
```

Plugins declare their own options through the same call the built-ins use, and a declared option is settable from `config.toml` like any other, including by a user who set it before the plugin loaded. `opt.all()` is always the complete, current list; the tables below are the ones that ship.

## Agent

| Option | Default | Effect |
| --- | --- | --- |
| `agent.model` | `""` | `instance/model`. Empty picks the first that works |
| `agent.system_prompt` | `""` | Replaces the built-in prompt when set |
| `gen.model` | `""` | Model for branch names, commit messages and titles. Empty uses the conversation's model. Separate on purpose: naming a branch does not need a frontier model |

## Chat

| Option | Default | Effect |
| --- | --- | --- |
| `chat.show_thinking` | `false` | Stream reasoning into the chat buffer |
| `chat.show_tools` | `true` | Show a line when a tool runs. Errors always show |
| `chat.tool_output_lines` | `3` | How much of a tool result to show under it. `0` gives the line count and nothing else |
| `chat.markdown` | `true` | Render answers as they arrive. `false` shows exactly what the model sent |
| `chat.preview_lines` | | How many rows a tool card opens to while the cursor is in it |

## UI

| Option | Default | Effect |
| --- | --- | --- |
| `ui.theme` | `"dark"` | Or `"light"`, or any theme a plugin contributed |
| `ui.motion` | `true` | Text that moves while something is happening. Off removes the movement and keeps the colour |
| `ui.hints` | `false` | A shortcut row under the composer. Off, the sidebar says the same keys |
| `ui.confirm_destructive` | `true` | Ask before anything irreversible. Turning it off turns off every such dialog, everywhere |
| `ui.ascii_only` | `false` | For terminals without a decent font |
| `ui.nerd_font` | `false` | Brand glyphs for providers, where the font has them. Deliberately not auto-detected |

## Keys

| Option | Default | Effect |
| --- | --- | --- |
| `mapleader` | `"\"` | Set it before you use `<leader>` in a binding |
| `timeoutlen` | `500` | How long to wait for the rest of a key sequence, in milliseconds |

Every picker, prompt and completion list reads the `ui.keys.*` family, in Neovim notation, with more than one key allowed per action:

```toml
[options]
"ui.keys.next" = "<C-n> <Down> <C-j>"
"ui.keys.prev" = "<C-p> <Up> <C-k>"
"ui.keys.page_down" = "<PageDown>"
"ui.keys.page_up" = "<PageUp>"
"ui.keys.first" = "<C-Home>"
"ui.keys.last" = "<C-End>"
"ui.keys.accept" = "<CR>"
"ui.keys.dismiss" = "<Esc> <C-c>"
"ui.keys.complete" = "<Tab> <Right>"
"ui.keys.clear" = "<C-u>"
"ui.keys.delete_word" = "<C-BS>"
"ui.keys.window_prefix" = "<C-w>"
"ui.keys.hint_delay" = 300
```

A widget claims these window-scoped while it is open, so they outrank global bindings and are released the moment it closes. That is what makes `^N` move in a picker even though `^N` is otherwise "new conversation".

The last two are the window prefix and how long you have to hold it before it lists what follows. `ui.keys.hint_delay = 0` shows the list at once; a bigger number keeps it out of the way of anyone who already knows the letters. `ui.keys.delete_word` moved off `<C-w>` when the prefix took it — a key cannot both delete a word and wait to see whether a window verb follows, because the waiting is what makes every word-delete feel late. Set them to each other's values if you would rather have it the old way round.

## Sidebar

| Option | Default | Effect |
| --- | --- | --- |
| `sidebar.open` | `true` | Show it at startup |
| `sidebar.width` | `34` | Also what `>` `<` `=` in the panel adjust, so the key and the file say the same number |
| `sidebar.hints` | `true` | The contextual key strip at the foot of the panel |
| `sidebar.refresh_ms` | `4000` | A workspace re-read per tick |

## The repository block

| Option | Default | Effect |
| --- | --- | --- |
| `git.sidebar` | `true` | Mark each project row with what has drifted from the remote, what is dirty, and which pull request its branch is |
| `git.fetch.interval` | `180` | Seconds between asking the remote what is new, so `↓3` means three waiting for you rather than three as of whenever this checkout last spoke to a server. `0` never asks on its own, and the block then says how old its numbers are. Only the repository of the conversation you are in, and only when its branch tracks one |

| `git.pulls` | `true` | The pull request on a project or worktree row: its number, its state, and its checks when they are failing or still running. Asks `gh`, so it needs the GitHub CLI signed in |
| `git.pulls.interval` | `180` | Seconds between asking the forge. A pull request moves on somebody else's schedule, so unlike the working tree there is nothing local to notice it. `0` asks only when you press the key |

## Worktrees

| Option | Default | Effect |
| --- | --- | --- |
| `worktree.root` | `"~/.nsh"` | Where new worktrees go, as `<root>/<repo>/<branch>`. A relative value keeps them inside the repository as `<repo>/<value>/<branch>`, with the `.gitignore` entry written for you. `""` restores the sibling layout |
| `clone.root` | `"~/.nsh/repos"` | Where `^O` clones a repository, as `<root>/<owner>/<repo>`. The first destination offered; folders you clone into through `Somewhere else…` are remembered and offered beside it. Separate from `worktree.root`, which spends `<root>/<repo>/` on one repository's branches |

## Git prompts

Every generated name or message has a prompt behind it, and every prompt is a setting in two layers: `*.instructions` is appended to the built-in prompt, which is the common case, and `*.prompt` replaces it entirely. Instructions still apply on top of a replaced prompt, so a per-project rule layers onto your own.

```toml
[options]
"git.branch.prefix" = "feature/"
"git.branch.instructions" = "Start with the Jira key when the message mentions one."
"git.commit.instructions" = "Follow Conventional Commits."
```

## Usage

| Option | Default | Effect |
| --- | --- | --- |
| `usage.sidebar` | `true` | The plan strip at the foot of the sidebar |
| `usage.warn_at` | `90` | Say something the first time a limit passes this. `0` to never |
| `usage.days` | `30` | The span the `^L` panel opens on |
| `usage.poll` | `true` | Ask the vendor while nothing is running. See [Models](/docs/models) |
| `usage.show_tokens` | `true` | The running token total, beside the context meter |

## Archive

| Option | Effect |
| --- | --- |
| `archive.sidebar` | A count row in the project panel. Off by default: nothing archived is on screen until you ask |
| `archive.sort`, `archive.group` | The panel's ordering and grouping. `s` and `S` in it change the same settings |
| `archive.width`, `archive.height` | The popup's size |
| `archive.auto_days` | Archive what has been idle this long. Reversible and free, so it may happen on its own |
| `archive.retention_days` | What counts as old. Only ever counts; `archive.sweep` is the same number with a person behind it |
| `archive.remind` | Say so once a day when old conversations accumulate |

Nothing in the archive deletes on a timer.

## Plugins

| Option | Default | Effect |
| --- | --- | --- |
| `plugins.disabled` | `[]` | Bundled plugins not to load. Their keys, rows and hints go with them |
