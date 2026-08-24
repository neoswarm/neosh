# 0039 — The archive is a place you go, and the question is worth asking

**Status:** accepted — extended by [0059](0059-an-archive-is-something-you-can-empty.md), which
makes the place a panel rather than a picker, gives it marks and a verb that empties it, and lets it
see the conversations the restore cap had been hiding from it.

Supersedes the last section of [0023](0023-archive-not-delete.md), which put the archive at the foot
of the sidebar and made the delete dialog conditional.

## Context

ADR 0023 got the verbs right — `x` archives, `X` deletes — and then put what you had archived in the
same column as what you had not. A foldable `ARCHIVED` section at the foot of the panel, shut by
default, one dim row per conversation once you opened it.

Two things were wrong with that, and both only show up after a few weeks of use.

**The panel is the list you work in.** Everything on it is something you might switch to now. A
section of things you have deliberately finished with is, by construction, the only part of that
column that is never the answer — and it grows without limit, so the longer you use the workspace
the larger the part of the panel that cannot help you. The section starting shut does not fix it:
the heading, its count and its blank line are still there, still saying "there is more of this",
below the thing you were actually looking for.

**A flat list of dim rows is not a way to find anything.** Once there are twenty of them, the thing
that would make the archive useful is a filter — and a panel that is 34 columns wide and has no text
field is the wrong shape for one. The archive was reachable and unusable, which is the specific
failure mode of putting a feature somewhere rather than building it.

Separately, `X` asked *conditionally*: a conversation with messages got a dialog, an empty one did
not. The reasoning in 0023 was that a dialog for something with nothing to lose is friction that
teaches you to dismiss dialogs. That is true about *reversible* actions and wrong here. What it
actually produced was a key whose behaviour depends on state you cannot see from the row it is
pointed at — so the one press in ten that did stop and ask was the one your fingers were already
through, which is exactly the reflex the condition existed to avoid.

## Decision

**The archive is not in the panel.** No section, no rows. What is left is one row — `┈ Archived`,
with a count — and only while there is something behind it. A door, not a drawer left open.

**It opens as a picker**, on `a` in the panel and `<C-f>` from anywhere, listing what you have put
away with the project it came from, how many messages it holds and when it was archived. It filters,
because that is what a list you go looking in needs. `↵` restores and switches to it, `^U` puts one
back without going there — tidying and travelling are different intentions — and `^X` deletes, from
the one place where deleting is a thing anyone is actually doing.

**Everything that cannot be undone asks, unconditionally.** Deleting a conversation, removing a
worktree, forgetting a key, taking back a model you added. No exceptions for the cheap cases: the
value of the question is that it is always there, and a dialog you can predict is a dialog you read.
`ui.confirm_destructive = false` remains the way out, and it is a setting, not a guess the program
makes on your behalf.

**And the dialog says what is at stake.** `confirm` is a widget of its own rather than a two-row
picker with the question crammed into a one-line title: it wraps, it carries `detail` lines — how
many messages, which project, what the alternative is — the destructive answer is drawn in the
theme's error colour, and `y`/`n` answer it outright. The cursor still starts on the answer that
changes nothing.

## Consequences

- **Archiving is still free.** It asks nothing and it is still `x`. The bar for a dialog is
  *irreversible*, and this ADR does not lower it — it removes the exceptions on the other side.
- **`u` is gone from the panel**, because there is nothing archived in the panel to press it on. The
  `session.unarchive` command is unchanged, and `^U` in the archive is the key that replaces it.
- **The dialog composes over a picker.** Deleting from the archive puts one float over another,
  which works because focus is a stack and the archive is still there underneath when you answer.
- **Two additions to the public API**, because this had to be built on it like anything else:
  `picker` grew `onKey`/`ownKeys` — the pattern `railPicker` already had, so a list can carry verbs —
  and `MarkOptions` grew `lineHlGroup`, so a plugin can band a row *under* its text instead of
  painting over it. The selected answer in a dialog is the first thing that needed both.
- **An archive with nothing to reclaim it is still an archive.** 0023's note stands: nothing sweeps
  it, and deleting someone's history on a timer remains the move neither ADR is willing to make.
  What is new is that emptying it by hand is now something you can actually do.

## What was added afterwards: a project outlives the conversations in it

The panel had no list of projects. It had a list of *conversations*, grouped by the directory each
one named, with pinned directories seeded in so that a favourite with nothing in it still drew a
row. Everything else about a project was inferred from what was inside it.

Which meant deleting was two things at once. `X` on the last conversation in a directory removed the
conversation, and then — with nothing left to infer the heading from — removed the directory too.
You cleared out a month of finished threads in a repository you work in every day and the repository
left the panel with them: no row, no `n`, nothing to start the next conversation from except `^O`
and typing the path again. The pinned exception is the tell. Somebody had already hit this, and the
fix was a special case for favourites rather than the observation that a project is a place you
work rather than a property of the work in it.

**The list is written down.** `sidebar.projects`, a workspace var, is now the panel's list rather
than an index into one. A directory joins it by being added, by a conversation being started there,
or by anybody setting a project var on it, and it leaves it by exactly one route — see below. The
draw path also writes `sidebar.name` for every project that has a conversation to read a name off,
because the name comes from the host by way of a conversation and a project with none has nobody to
ask: without it, emptying a worktree renamed it from `neosh (feat/thing)` to `wt-fe3c0d93` at the
moment you were least likely to recognise it.

**`X` on a heading removes the project**, and it is the only thing that does. That verb did not
exist — there was nothing to remove, because the list removed itself — and a list that can only grow
is the other half of this being a list at all. It is the key that already means *for good* on the
row below, saying the same thing about the row it is on.

It asks by 0039's rule and not by a rule of its own. An empty project is a row and no files: nothing
is at stake, `o` puts it back, and a dialog there is the kind you learn to clear without reading.
With conversations still in it, removing the project is deleting every one of them, so it asks like
the delete it is — how many, how many of those you would still have found in your list, and that
`x` archives instead. The conversations go first and the project only goes if all of them did — a
project removed anyway would take a row off the list that still had something in it.

### Consequences

- **A directory you visited once is a row until you say otherwise.** That is the trade: the old
  behaviour cleaned up after itself and took your projects with it. `X` is the broom.
- **Removing a var is not an announcement.** The arrangement adds any directory somebody sets a
  project var on, which is how a plugin can pin something this panel has never seen — so a *removal*
  had to stop counting, or `forget` would put the project straight back as it cleared it.
- **Only this panel's vars go.** Another plugin's note about the directory is that plugin's to keep,
  and a directory it still cares about comes back the next time it says so.

## What was corrected afterwards: closing the last conversation, and what an empty workspace is

The paragraph above used to end differently. It said the store *refuses* to delete the very last
conversation in a workspace, and treated that as the reason a project only goes if all of its
conversations did. That refusal reached the screen as `invalid argument: the last session cannot be
closed`, and it was wrong twice over.

**It is wrong as a rule.** Archiving the only conversation you have already worked — the host made
somewhere to land first, because "there is nowhere to go" is a bad answer to "I am finished with
this". Deleting it, which is the same sentence said harder, was refused. Two verbs on `x` and `X` of
the same row, disagreeing about whether that row was allowed to be the last one.

**It was wrong more often than "the last one" suggests.** Stepping away lands in another *open*
conversation and skips archived ones, so a workspace whose every other conversation was archived was
as alone as one with none: twenty conversations, and you still could not close the one you were in.

### A placeholder is not a conversation

The obvious fix — mint an ordinary empty conversation to land in — trades the error for a different
lie. You cleared the workspace out and it says you have one conversation, in a project, which you
never started; `X` on that project deletes it and gets another one; the panel can never be empty
because emptying it creates the thing that fills it.

So the landing place is marked: `Session::ephemeral`. It is the active session — the composer types
into it, the transcript draws into it, `^P` picks its model — and it is in no list, in no project,
and never written to disk. `SessionStore::list` leaves it out archived or not, `save_all` skips it
and never names it as the active one, and `switch` *drops* it rather than parking it, because the
moment you are somewhere else it is an empty conversation nobody asked for.

It stops being one the moment anything is said in it. `push_user` clears the flag — there rather
than at any of the call sites, because there is more than one way to speak into a session (the
composer, a steering message taken in mid-turn, a peer over ASCP) and the one that forgot would
leave a conversation with messages in it that no list ever showed.

**An empty workspace therefore looks empty.** No rows in the panel, and the transcript — the only
surface with room to say anything — says what to press: `↵` starts one here, `^O` adds a project,
`^T` is the panel, `^F` is what you archived. That last one matters, because "everything is
archived" is the most likely way to arrive here and the archive is the one place the panel does not
show. Restoring into it works the same way: with something saved but nothing open to land in, the
session the store was constructed with becomes the placeholder instead of being removed.

**Where you land decides where "here" is.** Closing or archiving the conversation you are in now
calls `work_in` on the one you landed in, the same as switching always did, so the banner names that
directory and the file tools are pointed at it. Without it both verbs left the workspace working in
the directory of a conversation that no longer existed.

### Consequences

- **`X` on the last project now finishes.** Its conversations go, the placeholder you land in is in
  no project, and the panel is left with nothing but `+ Add project` — which is what you asked for
  and what the old refusal made impossible.
- **The placeholder is not somewhere you can go back to.** Switch away and it is gone; there is
  nothing to return to, and nothing was lost. That is the same statement as it not being in the
  list, said about the store rather than about the panel.
- **A fresh start is still a conversation.** The session a new workspace is constructed with is not
  a placeholder: nothing was cleared out, and `neosh` in a directory you have never opened should
  read as a conversation waiting rather than as a workspace you emptied.
