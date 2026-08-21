# 0039 — The archive is a place you go, and the question is worth asking

**Status:** accepted

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
