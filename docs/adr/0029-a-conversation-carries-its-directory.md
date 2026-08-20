# 0029 — A conversation carries its directory, and everything follows it

**Status:** accepted

## Context

A conversation has had a `cwd` since the session store existed. The sidebar grouped by it, so the
panel showed projects rather than a flat list of threads, and `neosh.git` resolved against it, so
the footer showed the right branch.

None of that reached the agent. The permission layer's workspace root was fixed at launch from the
process's own directory, so `read_file("src/main.rs")` in any project but the one neosh was started
in came back "outside the workspace" — true, and useless. And `TurnRequest` had no directory at all,
so the vendor CLI drivers spawned their child in whatever directory the shell happened to be in.

That last one is the whole problem. `claude -p` and `codex exec` are *agents*: they read files, run
commands, and look at git, all of it relative to where they were started. A driver that ignores the
conversation's project is not slightly wrong about a path — it is answering questions about a
different repository, confidently, while the sidebar says otherwise. The grouping was decoration.

## Decision

**The conversation is the unit of "where", and every consumer follows it.**

Four things move when the active conversation changes, and they move through one function so it
cannot be three out of four again:

- the git repository `neosh.git` answers about,
- the root [`PermissionLayer`] resolves relative paths against,
- the directory the welcome block names,
- and `self.cwd`, which is what a new conversation inherits.

The fifth is the drivers, and they are told *per turn* rather than held anywhere:
`TurnRequest.cwd`. A driver is a value shared across conversations — one `ClaudeCliProvider` serves
every session — so a field on the provider would be a mutable "current directory" with exactly the
races that implies. A turn already carries its model, its messages and its tools; where it happens
is the same kind of fact.

Empty means "wherever the host process is", which is what a driver with no opinion did before the
field existed. That keeps every third-party provider plugin working unchanged.

**A worktree is a project.** Not a mode, not a setting on a conversation — a directory with
conversations in it, which the sidebar already knows how to show. Everything a worktree needs was
therefore already built; what was missing was a way to make one without leaving what you were
doing, which is one row in the picker `^N` now opens.

Worktrees go under `worktree.root`, defaulting to `~/.nsh`, laid out `<root>/<repo>/<branch>`.
A directory of neosh's own rather than a sibling of the repository: a worktree is not part of the
project you are working on, and littering its parent is how people end up with checkouts they
cannot account for. The option is declared by the **host**, not by the git plugin that reads it,
for two reasons — it is a place neosh writes on your disk, which is the host's business to answer
for, and its default has to be absolute. A plugin has no way to find your home directory, and a
`~` that only some readers expand is worse than no `~` at all.

## Consequences

`^N` asks a question it did not used to ask, and only where the question has more than one answer:
in a git repository, with the git plugin loaded. `Here` is selected, so the old behaviour is
`^N ⏎`, and `session.new.here` skips the question for anyone who wants their key back.

Restoring conversations at startup now puts you in the active one's project rather than in whatever
directory the shell was sitting in. That is a behaviour change and the right one: reopening the
conversation you were last in should put you back where it was.

The permission layer's allow-lists survive a move. They are about *what kind of thing* the agent may
do, not about which tree it does it in, and clearing them on every switch would silently re-prompt
for things already permitted.
