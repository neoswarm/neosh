# 0023 — Archiving is the everyday verb; deleting is the exception

**Status:** accepted

## Context

The sidebar had one way to get a conversation out of your list: `x`, which called `session.close`,
which removed the file from disk. There is no undo. To take the edge off, it asked first — but only
when the conversation had messages in it, because a dialog for something with nothing to lose is
friction that teaches you to dismiss dialogs.

That is a well-tuned version of the wrong design. The thing people press `x` for is "I am done with
this", which is a statement about attention, not about the value of what was said. A workspace that
answers it by destroying a week of work is a workspace where the tidying key is one you approach
carefully — so you stop pressing it, and the list stops being tidy.

## Decision

`x` archives. `X` deletes.

Archiving sets a flag. Every message stays, the file stays, `session.list({ includeArchived: true })`
still finds it, and pressing `u` puts it back at the top of the list — at the top, because you
unarchived it in order to look at it.

Archiving asks nothing. Deleting still asks, and its dialog now says that archiving keeps it.

`SessionList` grew `include_archived`, defaulting to false, so every existing caller keeps showing
the list the user works in. Filtering in the store rather than at each call site is deliberate: the
caller that forgets to filter is the one that ships.

## The two states that must not exist

**Active and archived at once.** You would be typing into a conversation the list does not show.
Archiving the active one steps away first — to the most recently used *unarchived* other, because
landing in something else you deliberately put away is the one destination that is definitely wrong.

**Archived and nowhere to go.** Archiving the only conversation you have is a reasonable thing to
want. The store refuses it (it cannot read a clock or a working directory, so it cannot invent a
replacement); the host answers by starting an empty conversation first. A restart never lands you in
an archived conversation either, for the same reason — that would make archiving look like it had
not worked.

## Consequences

- **Nothing is reclaimed.** An archive is a place things accumulate. `RESTORE_LIMIT` still caps what
  is loaded at startup, and the rest sit on disk. A sweep — "delete archived things older than N" —
  is a thing someone will want and is not built, because deleting someone's history on a timer is
  precisely the move this ADR exists to avoid.
- **`session.close` is unchanged and still means delete.** Archiving is a new verb beside it, not a
  redefinition of an old one, so a plugin that meant delete still gets delete.
- **The archived section only appears when there is something in it**, and starts shut. An empty
  section is a permanent reminder of a feature you are not using.
