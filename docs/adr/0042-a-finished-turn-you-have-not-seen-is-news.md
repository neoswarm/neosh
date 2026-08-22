# 0042 — A finished turn you have not seen is news

**Status:** accepted

## Context

The panel says what is happening: a spinner while a turn is running, an elapsed clock next to it, a
dot on the conversations doing work somewhere else. All of it is about the *present*, and all of it
stops the moment the work does.

Which leaves the one state the panel never had a word for. A turn takes minutes. You start it, you
go and work in another conversation — that is the whole reason the workspace holds several — and
some time later the agent finishes. The row it was on stops spinning and becomes an ordinary row:
same colour, same glyph, same shape as the eleven around it, distinguished only by an age in the
right-hand column that every other row also has. The answer you were waiting for is now found by
opening conversations until one of them has it.

It gets worse the better the rest of the workspace works. The daemon means turns survive the
terminal closing, so the finish can happen while nothing is attached at all; ASCP means some of
those rows are conversations on another machine. The more work you can start and walk away from,
the more of it you can lose track of — and "lose track of" here means the agent did what you asked
and you never read it.

The alternative people reach for is a notification: a toast, a bell, a line in the status bar. It is
the wrong shape. A toast is gone in four seconds and the wait is measured in minutes, so it is
precisely the message you are not there for. A bell says something finished and not which thing. And
neither survives closing the terminal, which is the case that needs it most.

## Decision

**A conversation carries whether you have seen how its last turn ended.** `SessionInfo::unread`,
set by the host when a turn ends in a conversation that is not the one on screen, and cleared when
you arrive in it.

**Off screen is the whole test.** A turn ending in the conversation you are looking at is not news —
it is the screen in front of you. That includes the case where nothing is attached: reattaching
lands you in that conversation with the answer already in it, so it has been seen by the time it
could have been missed.

**Arriving is the gesture that reads it.** There is no "mark as read" key and no dismissal. The
store clears the flag on `switch`, and the host clears it on every other way into a conversation —
a new one, the one you land on after closing or archiving, the one a restored workspace opens. A
mark you have to clear by hand is a second chore attached to the first one.

**It is on the conversation, not in the panel.** The sidebar, a status segment and a picker are
three different lists of the same conversations, and a mark each of them worked out for itself is
three answers to one question. It is also persisted, because the wait a restart makes longer is
exactly the wait this is for.

**It looks like attention, and it does not move.** `Status.Unread` is the amber the palette already
uses for *act now*, bold, sharing a hue with `Status.Approval` because they are asking the same
thing. The whole row wears it, not just the glyph: a single coloured character five columns in is
findable if you already know it is there, which is the one thing you cannot assume about a
conversation you have forgotten you started. And it does not shimmer or pulse — motion is reserved
for "something is happening you cannot see", and this is the opposite: nothing is happening, it has
already happened, and it will still be true in an hour.

**A folded project says how much of it is unseen**, as a dot after its name — `● ` or `●3` — because
folding is the thing that hid the rows. With the project open, the conversations say it themselves
and the project row says nothing: the same fact on two adjacent lines is how a panel teaches you to
stop reading it.

## Consequences

- **The mark is at most one per conversation.** Two turns finishing while you were away is still one
  row asking to be read, because the thing you do about it is the same either way. A count of
  unread *turns* would be a number nobody acts on differently.
- **An interrupted turn counts.** `TurnEnded` is `TurnEnded`, and a turn that stopped early
  somewhere you were not is a thing you probably want to know about. If interrupting from another
  conversation ever becomes ordinary, this is the line to revisit.
- **Remote agents do not have one yet.** `AgentState` still says only `Running`, `Idle` or
  `Blocked`. A conversation on another machine has an owner who may well have read it already, and
  "unseen by whom" is a question ASCP has no answer for — so the mark stays local until it does.
- **A plugin can do something else with it entirely.** It is a field on the public `SessionInfo`, so
  a status segment counting unread conversations, a picker that sorts them first, or a notifier
  which does send a system notification, are all things a third party writes without an op of its
  own.
