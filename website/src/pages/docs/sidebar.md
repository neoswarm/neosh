---
layout: ../../layouts/Docs.astro
title: Sidebar and projects
description: Projects, conversations, worktrees and the archive. What the panel shows, where its settings live, and the one verb that deletes.
---

## Projects

The sidebar groups conversations by the directory they were opened in. There is no registry to maintain: opening a conversation somewhere else is how a second project appears. `^O` adds one by path, with completion as you type — or by repository address, which clones it: paste `https://github.com/owner/repo`, the `git@` spelling, or just `owner/repo`, and it asks where it should go, remembers the folders you pick, and leaves you in the new project. `f` pins one to the top, `J`/`K` reorder, and `r` renames a conversation.

A conversation's directory is where its work happens, not just where it is filed: the repository git answers about, the root the agent's file tools resolve against, and the directory the model's own agent is started in. Switching conversations moves all of it.

The arrangement (pins, order, folds) is saved under `~/.local/state/neosh/plugin-state/`, not in your config, because an editor that rewrites the file you hand-edited because you pressed a key is one you stop trusting with it.

```toml
[options]
"sidebar.open" = true        # show it at startup
"sidebar.width" = 34         # also what > < = in the panel adjust
"sidebar.hints" = true       # the key strip at the foot of the panel
"sidebar.refresh_ms" = 4000
```

## The repository block

At the top of the column, above the projects: which branch the conversation you are in is on, what has drifted from the remote, and one key that does something about it.

```
 GIT                           ^G
──────────────────────────────────
 main ↓3 ~1 ?1
 ↓ pull 3 commits             now
```

The second row is a button, and its label is rebuilt from the state every time it is drawn, so it is never pointed at something that would do nothing:

| It says | Because | `↵` does |
|---|---|---|
| `↓ pull 3 commits` | three commits are waiting for you | fast-forwards, and says what git said |
| `⇄ diverged — 2 up, 3 down` | both sides have moved, so a plain pull cannot just fast-forward | asks **rebase or merge**, in those words, and does the one you pick |
| `↻ check for changes` | nothing is waiting as of the last time we asked | asks the remote again |
| `→ no upstream` | this branch tracks nothing, so there is nothing to be behind | nothing — it is a statement, not a verb |

`⇥` on a git row steps the block between `one` (the two rows above) and `full`, which adds the upstream it tracks and a line about the working tree. `^G` opens the full status, and every row in the block opens it on `↵`… except the button, which is the button.

### The numbers are asked for, not remembered

`↑` and `↓` come from `git status`, which compares your branch with the copy of the remote **sitting on your disk** — so drawn without ever fetching, `↓0` means "nothing had arrived as of whenever this checkout last spoke to a server", which after an afternoon's work is a sentence about breakfast. So neosh fetches: on a slow timer, and when you arrive in a conversation.

```toml
[options]
"git.sidebar" = true          # the block itself
"git.sidebar.style" = "one"   # "one" or "full" — ⇥ steps it
"git.fetch.interval" = 180    # seconds; 0 never asks on its own
```

Only the repository of the conversation you are in, and only when its branch tracks a remote one. The dim column at the right of the button is how old the numbers are — `now`, `12m`, or `⚠ offline` when the last ask failed. With `git.fetch.interval = 0` nothing leaves your machine unless you press the key, and that column is what tells you the numbers are old rather than untrue.

While a fetch or a pull is out, the row spins and its text sweeps. Nothing else in the block moves: `↓3` is news, not something happening, and a row that blinks about news you have already read costs attention every time it changes.

## Worktrees

A worktree is a second checkout of the same repository on a different branch, and neosh treats one as a project: it nests in the sidebar under the repository it is a tree of, named by its branch, with its own conversations, fold and order.

`^N` asks where a new conversation goes when there is something to ask:

```
New conversation
❯ ● Here                                 /home/you/work/project
  + In a new worktree                    a clean branch, named for you
  ⌂ In a new worktree, in this project   kept in .worktrees/
  + In a new worktree, named…            a branch of its own
  ⎇ fix/thing                            worktree  ·  ~/.nsh/project/fix-thing
  … Another directory…                   somewhere else entirely
```

`Here` is selected, so `^N ⏎` is what `^N` always did, and outside a repository the question is not asked at all. `n` in the project panel asks the same question about the project under the cursor.

The second row is the one you want most days: a clean branch named for you (`brisk-otter-k3f9` — two words you can say out loud and a tag so no two trees ever collide), renamed automatically from your first message in it, so it becomes `fix/composer-paste-truncation` without you ever naming it.

Where trees land is one setting:

```toml
[options]
"worktree.root" = "~/.nsh"        # the default: <root>/<repo>/<branch>
"worktree.root" = ".worktrees"    # relative: kept inside the repository
"worktree.root" = ""              # sibling of the repository
```

With a relative root, neosh writes the directory into the repository's `.gitignore` for you, so an in-repo checkout never sits in `git status` as one giant untracked directory.

On a worktree's sidebar row: `y` copies its path for the shell you are about to `cd` in, `p` pulls its repository from the remote, and `d` removes the checkout from disk. The branch stays, it asks first, and it tells you how many conversations go with it.

## Generated branch names and commit messages

`git.branch.new` has a model name the branch and shows you the name before creating it; `git.commit` writes a message from the staged diff and shows it before committing. Every prompt behind them is a setting, in two layers: `*.instructions` appends to the built-in prompt, `*.prompt` replaces it.

```toml
[options]
"git.branch.prefix" = "feature/"
"git.branch.instructions" = "Start with the Jira key when the message mentions one."
"git.commit.instructions" = "Follow Conventional Commits."
"gen.model" = "anthropic/claude-haiku-4-5"   # what writes names and messages
```

## Archiving

`x` archives. Everything is kept, every message and the file on disk; it simply leaves the panel. It asks nothing, because it takes nothing away. Archived conversations are not rows in the sidebar at all: `^F` from anywhere, or `a` in the panel, opens the archive as a popup of its own, with filtering, ticking, restore and export. `archive.sidebar = true` puts a count row back for anybody who wants one.

`X` deletes: the file goes and there is no undo, so it always asks, in numbers, saying how many messages and which project. `ui.confirm_destructive = false` turns off every such dialog, everywhere.

Three time settings, none of which deletes on a timer: `archive.auto_days` archives what has gone idle (reversible, so it may happen on its own), `archive.retention_days` only ever counts, and `archive.sweep` is the same number with a person behind it.

## Confirmations

Anything that cannot be undone asks first, and nothing reversible does, with no exceptions on either side: a dialog that appears for some deletes and not others is a key you cannot predict, and one charged for an undoable action teaches you to dismiss dialogs. The cursor starts on the answer that changes nothing, the destructive answer wears the error colour, and the question says what is at stake. `y` and `n` answer outright, `Esc` means no.

## The plan strip

The foot of the sidebar keeps one row per usage limit that matters, with `⇥` stepping up how much is shown. `^L` opens the full panel. Settings and details are in [Models](/docs/models).
