# 0042 — Somewhere clean to try this, without naming it first

**Status:** accepted

Refines [0029](0029-a-conversation-carries-its-directory.md), which made a conversation belong to a
directory, and the `^N` picker that came with it.

## Context

Two complaints about starting a conversation, which turn out to be the same complaint.

**`n` and `^N` did different things.** `^N` in the chat opened a picker asking where the
conversation goes. `n` in the project panel created one immediately in the project under the
cursor. One letter and a modifier apart, and the only way to learn that they differ is to be
surprised by it once — press the one you thought was the careful one and find a conversation
already made. There is no reading of "new conversation" under which the panel's version should
answer fewer questions than the chat's; if anything the panel is where you are *browsing* and least
committed.

**Naming a branch is a question asked at the worst possible moment.** The picker's worktree row
prompted for a branch. That is one field, and it is the field standing between "I want to try
something without disturbing this" and trying it. You do not know what the work is yet — that is
why you are opening a clean tree — so the name is either a guess you will fix later or a reason to
close the picker and keep working where you are. Every tool in this space that people describe as
pleasant to reach for makes this free: a directory appears, it has a name, you are in it.

The two are the same complaint because both are the program asking for a commitment before the
work, when it could ask afterwards or not at all.

## Decision

**One question, asked the same way from both keys.** `n` in the panel opens the picker `^N` opens.
`Here` is the first row in both, so `n ⏎` and `^N ⏎` do what those keys always did, and nothing is
created until a row is chosen.

**The picker is about a project, not about you.** `session.new` takes the directory it is asking
about — the conversation's own for `^N`, the row under the cursor for `n`. This is not cosmetic:
the rows in that picker are *that repository's* worktrees, and a panel listing four projects would
otherwise offer the branches of whichever conversation happened to be active. A row naming the
wrong branch is worse than no row.

That required `git_worktrees`, `git_branches` and `git_add_worktree` to take an optional `cwd`.
Absent means the conversation's own, which is what a status bar or a branch picker means and what
every existing caller keeps getting. Discovery is one `rev-parse`, so a caller naming a directory
per call is cheaper than the host keeping a table of open repositories — and the table would be
stale the moment the cursor moved anyway.

**A worktree you do not have to name.** The first worktree row creates one outright: a generated
two-word branch, under `worktree.root` as always, and you land in it. `git.worktree.new.auto`. The
row that asks for a name stays, below it, because knowing the name is a real state to be in — it is
just not the common one.

The name is an adjective and a noun — `brisk-otter`, `swift-thistle` — because a name you can say
out loud is a name you can find again in a list of eight of them. `wt-3` is not, and a timestamp is
neither. Collisions are checked against the branch list the caller already has in hand rather than
hoped away; with a few dozen trees the birthday problem is real. Renaming later is `git branch -m`,
like any branch.

**Rows of a picker can carry an icon and a highlight group.** `PickerItem.icon` and `.hl`, on the
shared widget rather than in this plugin, because the problem is not specific to this list: a
column of labels where some rows are places, some are machines and some are actions reads as one
undifferentiated column. A group name and never a colour, so a row follows the theme like
everything else. The gutter is all-or-nothing per list — giving it only to rows that asked for an
icon would step every other label one column left.

## Consequences

Starting a clean branch is `^N ↓ ⏎`, and there is no field in it.

`n` is one keystroke longer than it was for the person who always wanted "here, now". That is the
trade, and it is the right one: the extra keystroke is `⏎` on a preselected row, and what it buys
is that the two keys named after the same thing do the same thing.

An optional `cwd` on three git calls is a widening of the API surface, and the honest cost is that
"which repository" is now a question every caller could ask and most should not. The default is the
one they already meant, so the failure mode is verbosity rather than wrongness.

The generated names are English words, and a repository whose branches are already `amber-*` will
occasionally collide — which is checked, and falls back to a counter. What is *not* handled is a
name that is free locally and taken on a remote nobody has fetched; that surfaces as git refusing
the branch, with git's message, which is the same thing that happens when you type one.
