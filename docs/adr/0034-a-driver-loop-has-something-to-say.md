# 0034 — A driver's loop has something to say, and somewhere to say it

**Status:** accepted

## Context

Two things were true at once and neither was written down anywhere.

The first: **`ProviderEvent` describes a model, and an agent driver is not one.** Its variants —
`MessageStart`, `BlockStart`, `TextDelta`, `MessageStop` — are the Anthropic Messages SSE shape,
which is exactly right for a model, because a model emits content blocks and nothing else. But
`claude`, `codex exec` and every ACP agent are *running a loop*: they spawn sub-agents, keep a plan,
compact their own context, and know which commands they will accept. None of that is a content
block. There were two ways to force it into one — invent assistant text nobody wrote, or drop it —
and neosh dropped it, in a single line that even said so:

```rust
assert_eq!(events.len(), 7, "non-model event families must be ignored, not mapped");
```

So a turn could spend four minutes with three sub-agents out and a nine-step plan, and the
transcript showed a spinner. `/compact` — twenty seconds during which the CLI answers nothing at all
— was indistinguishable from a hang, and the CLI's own list of commands, which it sends unprompted
at every handshake, was read past.

The second: **the process only existed for the length of one answer.** `claude` was spawned per
turn and reattached next time with `--resume <id>`. That works, and it makes three things
impossible rather than merely unimplemented:

- `^P` mid-turn. The model is a command-line argument, so the earliest a new one could apply was the
  next spawn — which is after the turn you wanted to change.
- `⇧⇥` mid-turn, for the same reason.
- `Esc` that stops rather than kills. Killing the child takes the conversation with it: the CLI is
  holding the history, and the next `--resume` lands in a session that has never heard of the work
  it interrupted.

It also had a bug nobody had hit yet. `ClaudeCliProvider` is one object for the whole program and
held **one** session id. Two conversations on `claude-cli` would have resumed into each other's
history, and neither would have said anything was wrong.

## Decision

### One process per conversation

The CLI is started once and kept. Each turn is another message written to the stdin it is already
waiting on. This is what the Claude Agent SDK does with the same binary — so the shape is not
exotic, it is the supported one — and it is the only shape in which the control protocol means
anything, because there is no session to control when the process is younger than the question.

`TurnRequest` gained `conversation: SessionId`. A driver that keeps anything between turns needs it
and cannot derive it; several conversations run at once by design, so "the current one" is not
something a driver may assume. It is required rather than defaulted, because a driver silently
keying everything under one invented id is the bug this replaces.

What still costs a new process is gathered in one type, `Launch`: the directory, the reasoning
effort, the settings blob, and whether it was started with `--dangerously-skip-permissions`. Those
are the arguments with no control request behind them. The last is not a guess — asking for
`bypassPermissions` over the control protocol is refused in as many words:

> Cannot set permission mode to bypassPermissions because the session was not launched with
> --dangerously-skip-permissions

So crossing into or out of full access is the one mode change that costs a process, and the other
three are free. A replacement carries `--resume` with the id the old process reported, so the
conversation outlives the process holding it.

### `Esc` interrupts

Cancellation sends `{"subtype":"interrupt"}` and keeps reading. The CLI answers with a `result` and
goes back to waiting; five seconds later, if it has not, it is killed and the slot emptied. Verified
against the real binary: a turn cut off mid-answer, then a follow-up in the same conversation that
knew what it had been asked and how far it had got.

### `^P` and `⇧⇥` reach a running turn

The turn holds the only handle to the CLI's stdin, so a second writer would interleave two JSON
lines into one. Instead each conversation carries a `Tune` — a pending model, a pending mode — and a
`Notify`. The setter writes and wakes; the turn loop takes it between lines. `Notify` rather than
polling, because the case that matters is a model that has been thinking silently for half a minute,
which is exactly when you reach for the key.

### `Activity`: a second event family

`ProviderEvent::Activity { activity }` carries what a driver says about its own loop: `TaskStarted`,
`TaskProgress`, `TaskEnded`, `Plan`, `Compacting`, `Compacted`, `Commands`, `Context`. Nothing in
`TurnAssembler` reads it, which is the point — a driver can start sending one without the shape of
the messages it produces changing at all.

This is a decision about **shape, not access**. Every one of these already arrived on the transport
the driver was using. For `claude` they are `{"type":"system",…}` lines on the same stdout the
content blocks come down, and the plan is read out of ordinary tool calls. They needed somewhere to
go, not a channel.

The plan is the awkward one. There is no plan channel: a plan is a *tool the agent calls*, and the
list is whatever those calls have added up to. Two shapes are supported because which one an install
has is not something neosh gets to decide — `TodoWrite`, which carries the whole list every time,
and the `TaskCreate`/`TaskUpdate` pair that replaced it in recent versions and carries one step at a
time. `TaskCreate` does not say which number it got; the *result* does (`"Task #2 created
successfully: …"`), so a create is held until its result comes back. That is the CLI's own numbering
rather than an invented one, which matters because `TaskUpdate` refers to steps by it.

### The plan and the swarm are state, so they live in the footer

Both are drawn under the working line, at the very bottom of the buffer, and all three are torn down
and rebuilt together. That is not a layout preference: a plan is **state, not history**. The version
from two tool calls ago is not something to scroll back to, it is a wrong answer to "what is left".

The working line leads the block rather than following it. Everything in the block is state, but
only one row of it changes every tick, and it is the one you are watching — so it goes where the
transcript above it holds it still. Under a checklist it moved down a row every time the agent added
a step, which is the one thing a line you are staring at must not do.

And it is **abridged**. A checklist is redrawn whenever it changes and only ever grows, so a
twelve-step plan takes twelve rows for the rest of the turn — eleven of which are not the answer to
"what is it doing now". Past `chat.plan_rows` it says the same thing in fewer rows: what is finished
becomes a count, because done work is not part of "what is left"; the step in hand and the few after
it stay steps, because they are; and the tail becomes a count as well. The copy committed when the
turn ends is never abridged — that one is history, and a count there is a hole nothing can open.

Two things survive the turn, and each for a reason:

- **The final plan** is committed into the transcript once, where it stops being state and becomes
  what happened — what the turn set out to do and how much of it it got through, which its answer
  usually does not say.
- **Anything still out.** A backgrounded sub-agent is still running when the answer arrives, and it
  would otherwise vanish with the footer — losing the one case people actually ask about. It is
  written without a clock: a frozen number beside something that is still going is worse than none.

A sub-agent's *findings* are not in the footer. They come back as the result of the call that
spawned it, on that call's own card, where a reader is already looking. The footer answers the other
question, the one the card cannot answer until it is over: is it still going, and how long has it
been.

### Slash commands are a plugin

Two vocabularies meet in one composer and nobody should have to know which is which. `/compact`
belongs to the driver; `/model` belongs to neosh. So `plugins/builtin/slash` asks both, and does
different things with the answers: a neosh command runs and the draft is cleared, because it was
never a message; a driver command is written into the composer, because it is one.

Building it as a plugin needed two additions to the public API, and that is the test the thesis
sets: `PluginEvent::ComposerChanged` says what has been typed, and `ApiCall::ChatSetDraft` puts the
answer back. Between them they are completion of any kind — a `/` menu, an `@file` menu, a path
menu — and none of it needed a private hook. `ApiCall::AgentDriverCommands` is the third: reported
by the driver at its handshake, never configured, because which commands a `claude` install accepts
depends on its project directory, its plugins and its MCP servers.

The menu is opened by the draft rather than by a key. Binding `/` would mean no message could ever
begin with one.

### Codex speaks `app-server`, not `exec`

Both are the same binary. `codex exec --json` is a one-shot: prompt in, a line per thing that
happened, exit. `codex app-server` is JSON-RPC on stdio, and neosh was **already spawning one** —
`model/list` has no `exec` equivalent, so the transport was there and was being used for one call
and thrown away. Four things follow from using it for the turn as well, and none of them were
possible before:

1. **The answer streams.** `exec` reports an assistant message whole, on completion. The driver's
   own doc comment said so: *"there are **no token deltas** — an assistant message arrives whole"*.
   `app-server` sends `item/agentMessage/delta` and `item/reasoning/textDelta`.
2. **`ask` means ask.** `exec` is non-interactive, so neosh's `ask` mode could only fail closed —
   a turn that stopped at the first write. `app-server` sends
   `item/commandExecution/requestApproval` and `item/fileChange/requestApproval` as requests and
   blocks on the reply, which reaches the same person an ACP prompt does. ADR 0032 planned exactly
   this and could not have it.
3. **`turn/interrupt`.** Killing `exec` ends the process holding the thread.
4. **It says what it is doing.** `turn/plan/updated` is a real checklist. `thread/tokenUsage/updated`
   carries `modelContextWindow` — how big this model's window is, is the vendor's business, and
   codex is the one being told. `collabAgentToolCall` and `subAgentActivity` are the swarm.

And codex needs no relaunch logic at all, unlike `claude`: the model, the effort, the sandbox, the
approval policy and the directory are all parameters of `turn/start`, so a change to any of them
takes effect on the next turn with nothing to restart. The only thing a process holds is the thread.

Two smaller decisions fell out of it. The exit code of a failed command is not part of the
protocol's idea of a result, so it is written into the result text in the `Exit code N` convention
`cards::exit_code` already reads. And codex reports an edit as a **unified diff** rather than as
before-and-after text, so `diff::unified` reads the patch directly: reconstructing two files from it
in order to diff them again would be doing the work twice and getting a worse answer the second
time, since the patch already says exactly which lines moved.

The approval wording is neosh's, and that is a deliberate exception. ACP and the `claude` control
protocol both send the *sentences* they want offered — "allow, and don't ask again for `cargo`" —
and those are offered verbatim, because an agent that words an option means something specific by
it. Codex sends the decisions it accepts as a schema and no sentences at all, so there is nothing to
pass through. The three offered are the ones that mean something in every case; the
amendment-carrying decisions are codex negotiating with its own policy files and are not a question
for a person.

Codex could not be verified end to end here — `app-server` needs a login and the machine did not
have one. The handshake, `thread/start`, `turn/start` and the notification envelope are a real
capture; the answer path is written against the upstream protocol schema and pinned by a stand-in
server that speaks it back, which also covers the approval round-trip and the interrupt.

### ACP keeps its shape, and stops dropping things

ACP still spawns per turn, and can afford to: its session lives on the *agent's* side and
`session/load` puts it back. What was actually missing was smaller and worse:

- **`tool_call_update` was dropped entirely**, with a comment explaining that a closed block cannot
  be reopened. True, and beside the point — a result is not a block. Every ACP tool card sat there
  with a pulsing dot over a call that had come back.
- **`plan` and `available_commands_update` were dropped**, with a comment saying they were "about
  the agent's own UI". They were, right up until there was somewhere to put them.
- **Cancelling killed the child.** It now sends `session/cancel` first and gives the agent two
  seconds to answer its outstanding `session/prompt`, which is what lets it finish the file it was
  halfway through and record the turn in its own session. Killing is the fallback, not the plan.

A permission prompt is now raced against the cancellation in every driver that asks one. A prompt
with nobody at the keyboard is exactly when somebody reaches for `<Esc>`, and a turn that could only
be interrupted *between* questions could not be interrupted at all while one was open. The
stand-in-server test covers it, because that is a case no real agent would have to be asked to
produce.

## Consequences

- A turn that produces no message at all now says what happened. `/compact` is three status lines, a
  boundary, and a `result` whose `result` field carries the only sentence anybody wrote; read as "no
  message", it arrived as a question with a blank under it, which is what a crash looks like. The
  synthesised message opens with a `message_start` — without it, block 0 lands on top of block 0 of
  the previous turn and replaces it.
- Turns in one conversation are serialised by the lock on its process. They shared one stdin already;
  this makes it true rather than hoped.
- A conversation being deleted now shuts its CLI down, via `AgentDriver::close_conversation`. Left
  alone it would sit there for the rest of the program holding a transcript that had been erased.
- Every driver now reports a plan and a command list, and every driver's tool cards get a result.
  The three transports differ in what else they can do, and the differences are protocol facts
  rather than gaps: `claude` can change its model mid-turn and codex cannot; codex can stream a
  command's output as it runs and `claude` cannot; ACP has neither and has the cleanest resume.
- The CLI session id still lives only in memory, so a neosh restart starts a fresh process. That is a
  much smaller hole than it was — the process now survives every turn rather than none — but it is
  still a hole.

## What this turned up on the way

A latent ordering bug, which needed a crowd to show itself and got one when this added an eighth
bundled plugin. `activate` registers a plugin's listeners; the host is told it finished some
milliseconds later; several plugins are in that gap at once. A plugin joined the broadcast list at
the *second* moment, so anything broadcast in the gap reached whoever happened to have been confirmed
already — an answer that depended on how many plugins were installed and in what order they
finished. It cost a held `[options]` value its `option_changed`, and the only reason it had never
been seen is that the plugin declaring one was usually last. Plugins now join the list when their
load is asked for.
