# 0054 — An agent that speaks when nobody asked

## What happened

From a real conversation, and reproduced from the transcripts both sides kept.

The agent backgrounded a build and said:

```text
Waiting on the build. I'll report the test results as soon as they land.
```

And then nothing. Minutes of nothing, with the build long finished. Typing `what happened?` made a
whole answer appear at once — an answer to a question nobody had just asked — and *that* message
was never answered at all. Typing again did the same thing. Three messages in a row read as sent,
were drawn in the transcript, and produced an assistant message consisting of one empty thinking
block:

```json
{"role": "assistant", "content": [{"type": "thinking", "text": ""}]}
```

`claude`'s own transcript for the same conversation says exactly what was going on:

```text
05:21:30.188 enqueue 'resume'      05:21:30.188 dequeue
05:21:37.709 enqueue 'resume'      05:21:44.836 dequeue     ← seven seconds in the queue
05:21:44.872 enqueue 'open pr'     05:21:52.576 dequeue
```

and, twenty minutes earlier, the thing that started it:

```text
05:20:07.341 enqueue '<task-notification> … <status>completed</status>'
05:20:07.351 dequeue
```

## What it actually is

**`claude` runs turns nobody asked for.** A backgrounded command finishing is enqueued as a message
to itself and answered, and the answer is a whole turn on stdout — `system`/`init`, thinking, text,
`result` — with no prompt anywhere near it. Driven under a pty it is plainly visible:

```text
  0.0s >>> SENT  "background this, then say OK"
  5.0s result           ← the turn we asked for, over
 29.7s system  subtype=task_notification
 29.8s system  subtype=init                       ← a turn we did not ask for, beginning
 32.0s result  "The background command finished (exit 0) — it printed DONEDONE."
```

Nothing in neosh was reading at 29.8s. `Live::lines` was read inside `run_turn` and nowhere else,
so between turns the pipe had no reader at all. That is two failures, not one, and the second is
much worse than the first:

- **The turn is invisible.** It sits in the pipe. The agent said it would report back, it did
  report back, and the screen never changed.
- **The next turn eats it.** Send anything and `run_turn` starts reading again, straight into the
  buffered turn — which is why it arrived "all at once" — and then reads that turn's `result` and
  ends on it. The message you actually typed was still in the CLI's own queue. The turn was over
  before it was asked, and what got written to the transcript was the shape of a stream that had
  begun a thinking block and been cut off: `{"type": "thinking", "text": ""}`, no signature, no
  text, nothing on screen.

Left long enough there is a third: a pipe with no reader fills, and then the CLI blocks mid-sentence
with nothing anywhere to say why.

## The decision

**The reader outlives the turn, because the process does.** ADR 0041 established that a vendor CLI
outlives the turn and that an abandoned turn therefore has to be *drained*. This is the same fact
one step further on: it also **speaks** after the turn, so a reader that only exists during one is a
reader that is missing by definition. Stdout is now read by a task that lives as long as the child,
pushing lines into an unbounded channel the turn takes from. Nothing is lost, and nothing can block
the CLI by not being read.

**A turn ends on its own `result`, not on the first one it sees.** `system`/`init` opens a turn and
`result` closes one, for turns the CLI starts as much as for ours. The prompt goes out only once
everything already in the pipe has been read, and the turn then waits for as many endings as stand
between here and its own — one, or two when it is queued behind something already running. A
`result` belonging to somebody else is a line to forward and not the end of an answer.

**A line read past the end of a turn belongs to the next one.** There is exactly one moment a turn
reads past its own ending: having seen `result` it asks how full the context is and waits for the
answer. Anything else arriving in that window is the beginning of something, not the end of what has
already been said, so it is held and handed to whoever reads next.

**And it is said the moment it happens.** The reader is the only thing looking at the pipe between
turns, so it is the only thing that can notice. When it sees a turn begin with nothing attached, the
workspace is told — `Unasked`, out of band, like the quota poll and for the same reason: there is no
stream for it to come back on. The host opens a turn in that conversation to hold it.

That turn **says nothing**. `AgentDriver::listen_only` marks it, the driver skips the prompt and
reads until whatever is running stops, and the agent pushes no user message — a turn opened to hold
an answer has no question above it, and an empty message there is both a blank row the transcript
claims you sent and a question the driver would go and ask.

Everything else follows from it being an ordinary turn: the spinner runs, cards draw, `TurnEnded`
fires, and a conversation you were not looking at when the build landed gets its `unread` mark. See
ADR 0042.

## What this is not

**It is not a general "agents may talk whenever they like" channel.** The signal is one bit — *this
conversation's agent has started saying something* — and everything after it is the ordinary turn
path. A driver with no such behaviour implements neither method and is unaffected.

**It is not carried on the wire.** `listen_only` is a call on the driver rather than a field on
`TurnRequest`, because it is true of exactly the turn about to start and of nothing else, and a
value that outlives the thing it describes is a value that comes back wrong.

**`codex` and ACP are not covered.** Both can plausibly do the same thing and neither has been shown
to. The mechanism is in `AgentDriver`, so covering one is implementing two methods rather than
designing this again.

## The counting, and how it degrades

`init` is emitted at the top of every turn `claude` runs — ours included — and the boundary is a
*flag* rather than a counter on purpose. A count that drifts upward is a turn that waits for an
ending that never comes; a flag is wrong by at most one ending, and an installed CLI that stopped
emitting `init` altogether would put the driver back exactly where it was before this change rather
than somewhere new.
