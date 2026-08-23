# 0056 — An agent that speaks when nobody asked

**Status:** accepted

## Context

ADR 0054 established the fact: `claude` runs turns nobody asked for. A backgrounded shell finishes
minutes after the turn that launched it has ended, and the CLI enqueues a notification to itself and
*answers it* — a fresh `init`, an assistant message, a `result` — with no prompt anywhere near it.

It fixed the half that was corrupting things. `drain_between_turns` reads the pipe before the prompt
goes in and splits what it finds: what the CLI said about the *conversation* is forwarded, and the
unasked turn's own content is dropped, because an answer to a question that was never put must not
be drawn under the one that was.

And it named what it did not fix, in its own Consequences:

> The indicator is correct at every turn boundary and is put right by the next turn. It is **not**
> yet live while you sit there: a shell that finishes while nothing is being asked is not heard about
> until the next question, because between turns nothing reads the pipe.
>
> Closing that gap is not a bigger version of this change, it is a different one. […] An out-of-band,
> conversation-scoped stream from provider to host is its own decision and gets its own ADR.

This is that ADR, and the gap is not small. What the user actually reports is not a stale indicator:

```text
Waiting on the build. I'll report the test results as soon as they land.
```

and then nothing, for as long as they leave it. The agent said it would report back. It *did* report
back — that report is the unasked turn — and 0054's decision is to drop it. The transcript is
correct and the screen is silent, which from the keyboard is indistinguishable from a hang.

Two more things were still wrong underneath, and both are about the pipe having no reader.

**A pipe nobody reads fills.** `Live::lines` was read inside `run_turn` and nowhere else. A CLI that
talks to itself for long enough between turns eventually blocks on a write, mid-sentence, with
nothing anywhere to say why.

**The drain cannot cover a turn that has not finished.** `drain_between_turns` takes what has
already *arrived*. An unasked turn still in flight when the prompt goes in is one the CLI queues the
prompt behind — and the first `result` after that is theirs, not ours. From a real conversation:

```
05:21:37.709 enqueue 'resume'      05:21:44.836 dequeue     ← seven seconds in the CLI's queue
```

and in neosh's transcript, the message that spent those seconds queued:

```json
{"role": "assistant", "content": [{"type": "thinking", "text": ""}]}
```

Three in a row went that way. Each was drawn as sent, none was ever asked, and each came back as the
shape of a stream that had opened a thinking block and been cut off.

## Decision

### The reader outlives the turn, because the process does

ADR 0041 established that a vendor CLI outlives the turn and that an abandoned turn therefore has to
be *drained*. This is the same fact one step further on: it also **speaks** after the turn, so a
reader that only exists during one is missing by definition.

Stdout is read by a task that lives as long as the child, into an unbounded channel the turn takes
from. Nothing is lost, nothing can block the CLI by not being read, and `drain_between_turns` gets
cheaper on the way — `try_recv` on a channel that is already being filled, rather than a zero
timeout on a pipe that may have bytes in it the parser has not seen.

### A turn ends on its own `result`

`system`/`init` opens a turn and `result` closes one, for turns the CLI starts of its own accord as
much as for ours. So a turn counts what it is **owed**: its own ending, plus the one belonging to a
turn still running when the prompt went in.

A flag rather than a counter, deliberately. A count that drifts upward is a turn waiting for an
ending that never comes; a flag is wrong by at most one ending, and an install that stopped emitting
`init` altogether would put the driver back exactly where it was rather than somewhere new.

The one window in which a turn reads past its own ending — having seen `result` it asks how full the
context is and waits for the answer — hands what it finds to whoever reads next rather than folding
it into an answer already given.

### And it is said the moment it happens

The reader is the only thing looking at the pipe between turns, so it is the only thing that can
notice. `Unasked` is where it says so: out of band, conversation-scoped, like the quota poll and for
the same reason — there is no turn for it to come back through. The host opens a turn in that
conversation to hold it.

That turn **says nothing**. `AgentDriver::listen_only` marks it; the driver skips the prompt and
reads until whatever is running stops; the agent pushes no user message. A turn opened to hold an
answer has no question above it, and an empty message there is both a blank row the transcript
claims you sent and a question the driver would go and ask.

It is also the one turn that does **not** drain on the way in. Dropping the unasked turn's content
is right when it would land under a question it does not answer, and exactly wrong when the turn was
opened to give it somewhere of its own.

Everything else follows from it being an ordinary turn: the spinner runs, cards draw, `TurnEnded`
fires, `Background` is put right, and a conversation you were not looking at when the build landed
gets its `unread` mark (ADR 0042).

### Reporting it is not allowed to depend on timing

The reader cannot report every `init` it sees — an ordinary turn opens with one too, and that one is
being read. So it reports only what nothing is reading, and when something *is*, leaves a note that
whoever is reading clears by getting to that line. A note still there when the last reader goes is a
turn that was begun and never heard, and it is reported on the way out.

Without that, an `init` landing in the moment between a turn's last read and its bookkeeping was a
turn nobody would ever hear about — and, since 0054's drain drops it, one whose content was then
gone. The reader count is a count rather than a flag for the neighbouring reason: a turn that starts
while the last one is still winding down must not be un-marked by the older one's way out.

## Consequences

The gap ADR 0054 named is closed for `claude`. A shell that finishes while you sit there is heard
about while you sit there.

**One case still shows the unasked turn's content under your question**, and it is the one the drain
cannot reach: you typed *while* an unasked turn was in flight, so a turn was attached and no
listening turn was opened for it. It is drawn in the order it happened, above your answer, rather
than dropped — losing it is the failure this change exists to fix, and 0054's mis-attribution
argument is about content that has somewhere better to be. Here there is nowhere better.

**`listen_only` is a call on the driver rather than a field on `TurnRequest`.** It is true of exactly
the turn about to start and of nothing else, and a value that outlives the thing it describes is a
value that comes back wrong (ADR 0036).

**`codex` and ACP are not covered.** Both can plausibly do the same thing and neither has been shown
to. `codex`'s `thread/status/changed` is already a level pushed independently of any turn bookend,
which is the signal this would be built on. The mechanism lives on `AgentDriver`, so covering one is
implementing two methods rather than designing this again.

**A turn that produced nothing is still a turn.** A listening turn opened for something that turns
out to be over already ends immediately, having drawn nothing but a moment of spinner. That is the
cost of the report being cheap and idempotent, and it is paid on a path that only runs when the CLI
has actually started talking.
