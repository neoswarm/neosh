# 0031 — A turn says what it is doing, including when somebody else is doing it

**Status:** accepted

## Context

Three things were wrong at once, and they read to a user as one complaint: *the chat does not say
what is happening.*

**The working line went away at the first token.** `open_answer_row` called `end_working()`, on the
reasonable-sounding grounds that the working line is the last row of the transcript and an answer
has to be written somewhere. The effect was that a turn which wrote a sentence and then went off to
run four more tools looked *finished* in the chat while the footer and the sidebar both said it was
still going. Two parts of the same screen disagreeing about whether the program is busy is worse
than either answer on its own.

**A tool card said almost nothing.** `⏵ read_file  .env` and, under it, the first line of whatever
came back. No indication of whether the call was still running, no result for the calls that a
delegating driver ran, and an absolute path clipped to the part you already knew.

**And an agent driver's tools were invisible.** This is the one that mattered most, because
`claude-cli` is the default provider — it needs no API key, so it is what a fresh install uses.
Capturing a real session showed why:

```
stream_event  message_start          ← first inner message
stream_event  content_block_start    ← index 0, thinking
stream_event  content_block_start    ← index 1, tool_use "Read"
stream_event  message_delta          ← stop_reason: tool_use
stream_event  message_stop
user                                 ← the tool_result. Dropped on the floor.
stream_event  message_start          ← second inner message
stream_event  content_block_start    ← index 0 again, thinking
stream_event  content_block_start    ← index 1 again, text
```

Two failures fall out of that shape:

1. **`{"type":"user"}` lines were not parsed at all.** `claude_cli_line` handled `stream_event` and
   nothing else, so every tool result the CLI produced was discarded. The transcript showed a
   column of calls and not one answer, which reads exactly like a tool that hung.
2. **`TurnAssembler` keyed blocks by index alone**, which is safe for a model API — one message per
   stream — and wrong for an agent driver. The second message's `content_block_start` at index 1
   *overwrote the first message's tool call*. The call, its arguments, and the fact that it had
   ever happened were gone from the conversation by the time the turn ended.

That second one is a data-loss bug, not a display bug. It had been there since the driver was
written, and it is invisible from inside: everything still renders, there is simply less of it.

## Decision

**A block index is only unique within one message.** Blocks are keyed by `(message, index)` and held
in the order they opened. `message_start` after the first bumps the message counter. Nothing else in
the assembler changes shape.

**A tool result is a first-class provider event.** `ProviderEvent::ToolResult { id, content,
is_error }`. No model API sends one — the model *asks* for a tool and the client answers — but an
agent driver is both ends at once, so it is the only thing that can report a call finishing.

**A delegated session is recorded in the shape it actually had.** `TurnAssembler::finish` returns
`Vec<Message>`: assistant, the tool results as their own user message, assistant again. Squashing a
result into the assistant message that asked for it produces an array no API will accept, which
matters the moment you switch that conversation to a different model.

**A driver that runs its own loop reports its calls as they happen.** `ToolReady` and `ToolResult`
become `AgentEvent::ToolStarted` and `ToolFinished` when `delegates_agent_loop()`, rather than
being deferred to the end of the stream the way our own calls are — those are ours to schedule, and
these have already started. The same guard stops us re-running a call the CLI already ran because
its last message happened to stop on `tool_use`.

**The working line lives under the answer, not instead of it.** It is removed only by the turn
ending. The two paths that append to the buffer's *last* line — raw mode and streamed reasoning —
take it out of the way and put it back through one helper, `below_working`; everything else writes
at `chat_end`, which has always meant "above the working line".

**The card is a dot, a name, and a subject.** The dot pulses while the call runs and settles green
or red when it comes back — colour rather than shape, because a glyph that changes to mean
"finished" makes the column impossible to scan. The name is whatever the tool calls itself. A path
is relative to the conversation's directory. The whole line is clipped to the transcript's width,
because a card that wraps puts `n .cla…)` on a row of its own, which is worse than saying less.

One renderer, used by both the live stream and the replay. A conversation that changes shape when
you switch away and back reads as two different programs having written it — and the replay knows
something the live path does not have to remember: a call with a result somewhere after it is
finished, and one without is still running. The dot's colour is derived, not stored.

## Consequences

- `chat.tool_output_lines` (default 3) is how much of a result to show. `0` counts it instead. An
  error always shows at least its first line, because the setting is about noise and not about
  hiding failures.
- `finish()` returning `Vec<Message>` touches every caller. There were three.
- `end_working` no longer clears `streaming`. It used to, because the only caller was starting a
  fresh answer; with the working line coming and going underneath a stream, clearing it there
  restarted the answer on every chunk.
- The fixture for all of this is a real captured `claude -p` session, not a hand-written one. Both
  bugs were in the *shape* of what the CLI emits, and a fixture written from the same assumptions
  as the parser would have reproduced the assumptions rather than the CLI.
- Still not done, and deliberately: a plugin cannot yet register its own renderer for a tool. The
  displayed name and subject come from the call itself, which is enough to be right by default and
  not enough to be customised per tool. That wants an API and its own ADR — rendering happens
  synchronously while a stream is arriving, so it cannot be an async round-trip into a plugin.
