# 0042 — However many terminals you open, you see everything

**Status:** accepted

## Context

ADR 0036 split the workspace from the terminal and then allowed exactly one terminal. A second
`neosh` took the workspace over; the first was told `DetachReason::TakenOver` and exited with a
sentence.

That was written down at the time as "the only honest thing this shape can do": there is one
`Editor`, so there is one cursor, one scroll position and one terminal size, and fanning that out to
two windows of different sizes draws the wrong layout in at least one of them.

The reasoning was sound and the conclusion was wrong, because of what it costs in the case it
actually happens. **The reason to open a second window is that a turn is running in the first.** You
want to keep watching it and go and look at something else — and the one behaviour guaranteed to
lose sight of the turn is the one that closes the window it was running in. ADR 0036 named this
itself: the ask it came from was two things, work that survives the terminal *and* however many
windows you open you see everything, and it shipped the first.

It also mis-stated the obstacle. Layout was never the problem: ADR 0006 puts measuring, wrapping and
display-width maths in the frontend, so two terminals of different sizes each resolve the same
declarative geometry at their own size, correctly, with no help from anybody. The shared thing is
much smaller and much more specific — the handful of places the *host* renders to a width because
what it is drawing is one row and has to stay one row.

## Decision

**Several terminals, all mirrors of one workspace.** Attaching joins; it does not take over. Every
attached terminal shows the same thing, live. Typing in either is typing in both, because there is
one composer. `^Q` closes the terminal it was pressed in and leaves the others alone.

This is a smaller claim than independent windows, and it is deliberately the smaller one, because it
is the part that is correct without a protocol change. What it is not is below.

**A view is a fact about connections, not a thing on the wire.** `ViewId` lives in the binary. The
client already knows which one it is by virtue of being it, so telling it a number buys nothing and
costs one more field to keep in step across an upgrade. What did have to change is internal: the
host's input channel carries `(ViewId, InputEvent)` and `Frontend::detach` names a view.

**`^Q` is answered from the tag, not from who spoke last.** The daemon knows which connection an
event arrived on; the host sets `from_view` for the whole of handling that event. The tempting
cheap version — "detach whichever view most recently sent input" — is wrong under exactly the
conditions this feature exists for: the host can be behind the channel, so the last event *sent* is
not the event being *handled*, and the failure mode is closing somebody else's window.

**The workspace is the size of the smallest attached terminal.** Not because layout needs it —
layout is the frontend's — but because a tool card is one row of buffer *content*, shared by every
view. Measured for the widest terminal it wraps in the narrow one, and a card that wraps puts
`n .cla…)` on a row of its own, which is the exact failure `chat_width` exists to prevent. A big
window showing a narrower transcript is a cost you can see and reason about; a corrupted one is not.

Sizes are reconciled **on every change including a view leaving**. Nobody reports a size when
somebody *else* closes a window, so a workspace pinned narrow by a terminal that has gone would
stay narrow for as long as it runs. The same is true of `ViewportChanged` per window, which is
merged the same way and re-derived when the set of views changes.

**Republish goes to everyone, and therefore had to become idempotent.** A terminal joining makes the
host say its whole state again; sending that to every view is what brings the ones already attached
back into step, and it means "somebody opened a window" is also the moment a view that had drifted
is repaired. That required a one-line fix with a real bug behind it: `republish` spliced its buffers
in at `old_end: 0`, an *insertion*, correct only against a mirror that has nothing. Sent to a
populated mirror it doubled every buffer — the logo twice, the transcript twice. `u32::MAX` means
"however many rows you have, they are these now"; the mirror already clamps, so it is a replacement
at one end and an insertion at the other. The existing test missed it by folding each republish into
a *fresh* mirror, which is the one case that was never broken.

**A width the host learns is a width it has to redraw for.** `ViewportChanged` is where the host
finds out how wide the transcript is — `Resize` is the terminal's outer size, and the transcript is
some part of it. Anything drawn to a width is now drawn again there, or a narrow terminal joining a
wide workspace keeps the wide one's welcome until something unrelated happens to repaint it.

**The event stream fans out.** `Agent` held one `mpsc::UnboundedSender`; it now holds a `Fanout`,
and `Agent::new`'s receiver is an ordinary subscription with nothing special about being first.
Deliberately not a `broadcast` channel: its lagging consumers drop the *oldest* event, silently, and
under load — which is when a turn is producing the most. A view that missed one `ToolFinished` draws
a card that never stops spinning, and nothing afterwards puts it right. An unbounded queue per view
grows instead, which is a failure you can see.

## Consequences

- **Zero views is an ordinary state, not a shutdown.** It already was — closing the last terminal
  leaves the turns running — and now the code says so in one place instead of implying it in
  several.
- `DetachReason::TakenOver` is still on the wire and nothing sends it. Left alone: removing it is a
  protocol change for no benefit, and a client from an older neosh may still receive one.
- **The views share a cursor.** Scrolling the transcript in one scrolls it in the other, because
  there is one `Editor`. For a mirror that is the definition rather than a defect, and for anything
  else it is what the next ADR is for.
- **Idle is measured from the last terminal leaving**, not from any terminal leaving, which is the
  question `neosh status` was always asking.

## What this still does not do

Superseded by [0059](0059-a-terminal-is-a-place-you-are.md), which is the rest of it: a view per
terminal, so a second window is somewhere else rather than a copy. Three things were named here as
what it would take, and all three are what that ADR did — `Host` split into what the workspace owns
and what a terminal owns, "the conversation on screen" moved out of the session store, and an
`ApiCall` that names the view it draws into.

Two claims of this ADR did not survive it, and 0059 says why: **the views share a cursor** (they do
not; a window has always been where a cursor lives, and a window belongs to a view), and **the
workspace is the size of the smallest attached terminal** (it is not; that existed because a tool
card was one row of content every view shared, and a transcript belongs to a view now).

What does survive is everything about *connections*: attaching joins rather than taking over, `^Q`
is answered from the tag the input arrived with, `republish` is idempotent because it goes to a
mirror that may already have the state, and the event stream fans out to a queue per view rather
than through a `broadcast` channel that drops the oldest under load.
