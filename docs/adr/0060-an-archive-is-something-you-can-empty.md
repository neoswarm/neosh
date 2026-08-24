# 0060 — An archive is something you can empty

**Status:** accepted

Finishes what [0023](0023-archive-not-delete.md) and [0039](0039-the-archive-is-a-place-you-go.md)
both wrote down as a note and neither built: *emptying it by hand is now something you can actually
do*. Neither of their rules is reversed. Nothing here deletes anything on a timer.

## Context

The verbs were right. `x` puts a conversation away and asks nothing, because archiving is
reversible; `X` destroys one and always asks, because it is not. 0039 took the archive out of the
sidebar — a list of things you are deliberately done with is the one part of that column that is
never the answer — and made it a picker you go to, with `^U` to put a row back and `^X` to delete
one.

Three things about that only show up after a year of use.

**A picker is a fine way to deal with one conversation and a hopeless way to deal with two
hundred.** `^X` on a row, read a dialog, answer it, find the next row: that is the correct interface
for the thing you archived this morning and regret, and it is forty minutes of dialogs for a
workspace you have used since last spring. There was no verb for "all of these", and the number of
conversations an archive holds only ever goes up. 0023 said an archive is a place things accumulate
and that nothing reclaims it, which is true and was never meant to mean *nothing can*.

**A picker cannot be extended, and this one was made of the thing ADR 0040 exists to stop.** The
archive's keys were `ownKeys` plus an `onKey` callback that switched on `key.key.code.c` — a private
key handler four hundred lines into the sidebar's source. `^Z` could not list those keys, `^K` could
not run them, `init.ts` could not move them, and a plugin that wanted one verb on these rows had to
fork the panel that drew them. Every argument in 0040 applies here word for word; the archive was
simply written before it.

**And it could not see the whole archive.** `RESTORE_LIMIT` caps what a workspace loads to the two
hundred most recent conversations, deliberately — 0023's "a cap rather than a sweep", because
deleting somebody's history at startup because they have a lot of it would be a spectacular thing
for an editor to do. But everything in the workspace asks `session.list`, which is the *store*, so
past that cap a perfectly real conversation is a file that nothing can show, nothing can open and
nothing can delete. A "delete everything archived" built on `session.list` would have reported that
it had emptied a directory it had not touched.

## Decision

**The archive is a panel, and it is a plugin of its own.** `plugins/builtin/archive/`, buffer kind
`neosh.archive`, opened by `^F` from anywhere and by `a` in the project panel. It is a list you move
in by [0049](0049-a-list-is-a-place-you-move-in.md): `j`/`k` take a count, `^D`/`^U` are half of the
panel's real height, `12G` is a row, a half-typed count is on screen, and the row under the cursor
unfolds so a clipped title is readable. It is **modal** by
[0047](0047-a-panel-that-has-the-keyboard.md) — `^N` over it must not open a conversation behind it
— and `^F` closes it, because a modal that borrows a global key to open itself owes a binding to
shut itself.

**Every key in it is an ordinary binding pointed at a named command.** There is no `onKey` and no
switch on a `KeyContext`. `^Z` lists them, `^K` runs them, one line in `init.ts` moves any of them,
and `archive.action` is a contribution point so a third party can put a verb on these rows without
forking the panel — invoked with the conversations it is about, so the contributor never has to
find our cursor.

**Rows can be ticked, and a verb is about what is ticked.** `<Space>` (or `⇥`) ticks and steps down,
`a` ticks everything showing, and put-back, delete and copy all act on the ticked set or — when
nothing is ticked — on the row under the cursor. One rule for every verb including contributed ones,
because a set of keys whose arity you have to remember is the thing marks were supposed to remove.
The key strip says which it currently means: `X delete (3 marked)`.

**`^X` empties it.** Not the row: everything the panel is *currently listing*, which is what makes
the filter worth having — narrow to one project, or to `worktree`, and `^X` is that and nothing
else. It asks, by 0039's rule, and the question says how many conversations, how many messages, how
many projects and how far back they go, with the destructive answer in the error colour and the
cursor starting on `Keep`.

**Sorting, grouping and filtering are settings, and the keys move the setting.** `archive.sort`
(when you put it away, when you last used it, name, project, size) and `archive.group` (by project,
by age, flat) are what `s` and `S` cycle, so `config.toml` and the keys are talking about the same
number — the way `sidebar.width` and `>` already do. Grouping puts each heading in exactly once, in
the order the sort put its first member: sorted by when you put things away and grouped by project,
the project you last finished something in is at the top and each project's own rows are still
newest first.

**`/` filters through a prompt rather than by typing into the panel.** Every letter here is a verb —
`a`, `e`, `u`, `y`, `s` — and a filter that took them would be a filter that can never contain them.
Shadowing twenty-six bindings for the duration of a search is a mode that goes wrong once and then
goes wrong every time.

**Housekeeping archives and reports; it never deletes.** `archive.auto_days` puts a conversation
nothing has happened in for that long *away* — allowed to happen on its own precisely because it is
reversible and free, which is 0023's own test for whether a thing needs a dialog, and it says how
many it moved. `archive.retention_days` deletes nothing ever: it is the number `archive.sweep`
uses and the number a once-a-day reminder counts, and both of those end with a person pressing a
key. Deleting history on a timer remains the move 0023 and 0039 refuse, and a setting that quietly
reversed that would be the program deciding something that is not its to decide.

**`e` copies the conversations as markdown**, to the clipboard rather than to a file — for the
reason `Clipboard` is OSC 52 and not a clipboard library: the workspace may be running on the big
machine while you are sitting at a laptop, so a path it writes to is a path on the wrong computer.
It is the transcript reader's `ya` aimed at conversations you are about to throw away, which is the
last moment anybody would want it.

**And the whole directory is in scope.** `session.stored` is a new call that reads the sessions
directory rather than the store, answering with the same `SessionInfo` so one renderer draws both
kinds of row; the header says how many are `on disk only` when any are. Every verb that names a
conversation — open it, put it back, read it, delete it — brings it in from its file first if the
store has never heard of it. That is `Host::hydrate`, and it is why a row from before the cap
behaves like any other row instead of answering "no such session" to four keys out of five.

## What this took out of the sidebar

**The `┈ Archived` row is gone, and that is the last of it.** 0039 took the section out and left one
dim line with a count behind, on the grounds that a door is the least a panel can spend on saying
where the archive is. The argument that removed the section removes the line as well: the column is
your *conversations*, the thing that row announces is by definition the thing you are finished with,
and it is on screen every second of every day to say so. Nothing about the archive is drawn until
you ask for it — `^F` from anywhere, `a` from the project panel and advertised in its key strip,
`^K` by name. `archive.sidebar = true` puts the count back, because somebody will want it and a
default is something you turn off.

The `a`, and the row when it is asked for, are `sidebar.action` and `sidebar.section` contributions
from the archive plugin. That is ADR 0040's claim tested on ourselves: the panel that *owns* the
archive is not the panel that says there is one, `plugins.disabled = ["archive"]` takes both with it
rather than leaving a row pointing at a command that has gone, and `a` had to come out of the
sidebar's `RESERVED` set — the list of letters a contribution may not take — because it is no longer
a key the sidebar has any claim on.

## What it exposed

**A bordered float was reporting its outer box as its viewport.** `Extent` already says an extent
measures *content* and the border is chrome the frontend adds afterwards, and says why: so that no
caller has to subtract two from every number it computes, because the day one of them forgets, the
last line of a list is silently not drawn. The geometry going back the other way did not follow that
rule, so a panel that padded its key strip down to the height it had been told it had came out two
lines longer than the window it was drawn in — and the strip was off the bottom of it. The frontend
now reports the content rectangle. Nothing depended on the old numbers; every float in the tree
computes its own layout.

## Consequences

- **`^F` and `a` mean what they meant.** This is a bigger panel behind the same two keys, and `↵`
  still restores-and-opens because opening something you put away is a statement that you want it
  back. Putting one back without going there is now `u` rather than `^U`: a bare letter is free in a
  panel whose filter is a prompt, and `^U` has a job here — half a screen — that 0049 gives every
  list in the workspace, in the one panel most likely to be longer than one.
- **`session.archived` is still a command name.** It opens the panel. Anybody's `init.ts` may be
  pointed at it, and a rename would have been a breakage for nothing.
- **The count in the project panel, when it is turned on, is polled rather than derived.** It
  follows `sidebar.refresh_ms` and one `session.list` call, because archiving something you are
  *not* looking at switches nothing and `session.onChange` would never fire. Nothing is contributed
  unless the number moved — a contribution is a broadcast, and re-announcing an unchanged row would
  redraw every panel reading that point.
- **With the row off, the archive is discoverable only by its keys.** That is the trade, and it is
  why `^F` stays in the project panel's own key strip and in the empty-workspace text — "everything
  is archived" is the likeliest way to arrive at an empty workspace, and the one place the panel
  does not show is exactly the one it has to name.
- **An archive can now be emptied by accident in one keystroke and a `y`.** That is the trade this
  makes, and it is why the question is unconditional, says what is at stake in numbers, and starts
  on the answer that changes nothing. `ui.confirm_destructive = false` still turns it off, and it is
  still the user's setting rather than the program's guess.
- **What is still not built:** an export to a file, because a plugin cannot write one and giving
  every plugin a filesystem is not a thing to add in passing; and any form of automatic deletion,
  for the reason two ADRs have now given.
