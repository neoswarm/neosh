# 0052 — A question is the last thing until something answers it

## What happened

Send a message while a turn is still winding down and this is what the transcript did with it:

```text
▌ what does this crate do

It parses the wire format and hands the events to the assembler.

▌ and the tests?          ← what you just typed

  ✔ read the parser
  ▸ check the assembler
  □ write it up

  +12 −3 in 4 files
```

The question you had just asked was not the last thing on screen. Two blocks belonging to the
*previous* exchange sat underneath it, and nothing ever answered it.

Three separate mistakes met to produce that, and each one is worth writing down because each looked
correct on its own.

**The composer decided "is a turn running" from a map the host emptied too early.** `<Esc>` removed
the conversation's entry at the moment it *asked* the turn to stop, not when the turn stopped. A
turn winds down for as long as its driver takes to get back to a turn boundary — for a vendor CLI
that is the whole drain described in ADR 0041 — and in that window the workspace believed nothing
was running. A message typed there started a **second** turn in the same conversation. Then the
first turn's `TurnEnded` arrived, removed the entry by conversation because it had no way to tell
whose ending it was, and took the second turn's cancellation token and its `Round` with it. What
that looked like: a turn nothing could interrupt, a spinner frozen at whatever second it had
reached, and a third `⏎` starting yet another turn on a driver that keeps one process per
conversation.

**A message taken in at the final gap was drawn as asked and then not asked.** After the last round
of tools the loop takes anything queued and carries on — that is what makes steering steering. But
the same call sat after the tool results with no cancellation check, so a cancelled turn pushed the
message into the conversation, emitted `Steered`, drew it in the transcript, and then broke out of
the loop at the top. From the outside that is indistinguishable from an agent that read your message
and ignored it, which is the failure ADR 0041 already fixed once for a different reason.

**The turn's closing rows were appended at the end of the buffer.** The plan, what was left running
and the diff summary are drawn when a turn ends, and "the end" was the end of the buffer. With a
question steered in after the last thing the turn said, the end of the buffer is *below* that
question.

Underneath all three: reading the transcript and coming back left the window where you had been
reading. `chat_top` is set by every motion in the reader and nothing put it back, so `^S`, a look
around, `^S` again, and the next thing you typed arrived below the screen with older text still
showing. Sending a *new* turn reset it; steering into a running one did not.

## The decision

**A question is the last thing in the transcript until something answers it.** One `Option<u32>`
holds the row where such a question starts. It is set when a question is drawn and cleared by the
first thing drawn under it, so it is exactly "the transcript ends with something you asked and
nothing else". Two things read it.

The turn's closing rows go *there* rather than at the end — spliced above the question, each one
pushing it further down. They are about the answer they close, and the end of the buffer stopped
being that the moment a later question landed. A tool result that comes back afterwards moves the
row with it, the same way it already moves every card below it: a call landing above a question does
not answer it.

And a question still unanswered when the turn ends says so, in a row of its own. That is the other
half, and without it moving the plan only made the silence tidier: a bare question reads exactly
like one still being thought about. What the row says comes from the stop reason — the driver's
error where there is one, the model's explanation where it refused, `interrupted` where you pressed
`<Esc>`, and otherwise that the turn ended there. It is a row of the transcript rather than a
notification because it is *about* that question, and a toast that has faded is not an answer to
"why is nothing happening".

**An entry in the running-turns map lives until `TurnEnded` arrives.** `<Esc>` sets a flag on it
instead of removing it. The map is then exactly "turns the host is waiting to hear the end of",
every entry is removed by the ending it belongs to, and the second-turn collision cannot be
expressed. A message typed while a turn winds down is queued and becomes the next turn through the
leftover path that already existed for messages queued past the last gap.

**Steering does not take a message it cannot deliver.** The tool-gap call is guarded on
cancellation. Left in the queue, the message is picked up when the turn ends and starts a turn that
does ask it.

**A plugin veto is a refusal, not a cancellation.** They were the same value, so a turn a policy
plugin refused was reported downstream as one the person had interrupted, and the reason the plugin
gave was dropped on the way past. It now travels in the stop reason, which is what lets the row
above print it.

**Leaving the reader returns to the newest.** The composer is the bottom of the transcript, and
every other way into a conversation already resets the offset for the same reason. Sending resets it
too, whether it starts a turn or joins one.

## Consequences

`<Esc>` twice in a row says "interrupted" once. The second press is still swallowed — it is spent
either way — but only the press that stopped something is news.

A conversation whose driver never reports a turn ending keeps its entry, and so keeps saying it is
working. That was true before this change for every turn that was not cancelled; what is new is that
a cancelled one behaves the same way. The honest report is the one that says a turn is still winding
down when it is, and the dishonest one was the map that emptied early.
