# 0054 — A turn can end while its work does not, and a turn that is waiting should say so

**Status:** accepted

## Context

`Bash(run_in_background: true)` starts a command and does not wait for it. The agent gets a handle
back, says what it did, and the turn ends — with `npm run build` still running. The same is true of
a sub-agent that was let go of, and of anything a driver leaves watching something.

neosh drew none of that. A turn ends, `end_working` takes the footer away with the working line, and
what is left on screen is an answer: *done*. There was one exception, `tasks_left_running`, and it
was built out of the wrong material — it read the turn's own list of sub-agents, which is destroyed
by `rounds.remove(&session)` two statements later — so it was a static row, correct at the instant
it was drawn and never again, and no list of conversations mentioned it at all. Nothing anywhere
answered the only question there is here: *it says it is done — is it?*

Underneath that were two defects, and the second is the serious one.

### The level was read as a fan-out of edges

`claude` reports background work twice, on purpose. There are **edges** — `task_started`,
`task_updated`, `task_notification` — and there is a **level**, `background_tasks_changed`, which
carries the whole live set every time membership changes. Its own documentation says which to use
and why:

> consumers that only need 'is background work running' should replace their set with each payload
> rather than pairing edges, so a missed bookend cannot wedge a stale running indicator.

neosh read the level as a fan-out of `TaskStarted`, one per entry — purely additive. So the payload
whose entire meaning is *the set is now empty* produced no events at all, and an indicator switched
on by the first payload had nothing that could ever switch it off. The vendor wrote that sentence
about exactly the mistake that was in the tree.

### The process kept talking and nothing was listening

A turn is read until its `result`, and then the loop breaks. That is right for content and wrong for
this: a backgrounded shell finishes minutes later, and *after the turn boundary* is the only place
it can possibly report. Driving `claude 2.1.240` with a persistent process shows what actually
arrives there — and it is more than three system lines:

```
   4.24  system/background_tasks_changed ["bf0z4m34m"]
   4.24  system/task_started  is_backgrounded=true  task_type=local_bash
   5.45  result/success                     <- turn one ends; neosh stops reading
  21.05  result/success                     <- turn two, asked and answered
  29.25  system/background_tasks_changed []
  29.25  system/task_updated  status=completed
  29.25  system/task_notification  status=completed
  29.28  system/init                        <- a turn nobody asked for
  30.90  assistant ["thinking"], ["text"]
  30.97  result/success
```

The CLI does not merely report the shell exiting. It *starts a turn about it* — a fresh `init`, an
assistant message, a second `result` — because that is what "you'll be notified when it completes"
means on its side. Four results for three prompts.

All of it sits in the pipe. The next question is asked, and the loop reads that first: the stray
`result` ends the turn before the CLI has seen the prompt, and what is drawn under what you typed is
the model's reaction to a shell exiting. Your actual answer arrives one turn late. This is ADR 0041's
abandoned-turn corruption with a different cause — and unlike that one it needs no `<Esc>`, no
switching away, and no mistake by anybody. Backgrounding one command is enough.

### What the others do

`claude` also emits `system/turn_duration` carrying `pending_background_agent_count` and
`pending_workflow_count`, described as what "the REPL renders the 'Done in Ns' / 'Waiting for N
agents' line" from. Its own front end treats *the turn ending* and *the work ending* as two
different events, and prints the difference.

`codex` reaches the same place by another road. `thread/status/changed` is a level —
`idle | active{activeFlags} | systemError | notLoaded` — pushed independently of any turn bookend,
and long-lived processes live in a `process/*` family keyed by a client-supplied handle that belongs
to neither a turn nor a thread (`thread/backgroundTerminals/clean` is how a host disposes of them).
Different vocabulary, identical conclusion: **what is running is state, not an event stream, and it
does not belong to a turn.**

## Decision

### It is a level, and it replaces

`Activity::Background { tasks: Vec<BackgroundTask> }` carries the whole live set. A receiver swaps
its set for the payload. Empty is a value and means nothing is running — it is what clears the mark
— and an *absent* `tasks` is not an empty one, because a line we cannot read must not be able to
claim the set is empty.

The edge family stays and keeps its job: `TaskStarted`/`TaskEnded` are what this turn spawned and is
waiting on, and they are what the footer lists while it works. Both are kept because they answer
different questions and because codex has no level signal — dropping the edges would trade one
driver's accuracy for another's silence.

`TaskStarted` gains `backgrounded`, read from `is_backgrounded`. The *later* move to the background
already arrived as a status patch and was already handled; the commoner half — a shell launched
detached — is only ever said here, so the two halves of one idea were being read in two places and
the commoner one was missed.

### It belongs to the conversation

`Session::background`, not `Round::tasks`. The round is thrown away when the turn ends, which is the
exact moment this becomes the thing worth knowing. On the conversation it survives the turn, it is
on `SessionInfo` so every list agrees, and it is republished — ADR 0036's rule, that a value which
is only forwarded is a value that comes back wrong.

A process that has just started is running nothing and will never say so — the set is reported only
when it *changes* — so the driver emits an empty one on every spawn. Without it, a conversation whose
CLI was replaced mid-way (a model change, a crash) goes on drawing shells that died with the old
process, and nothing will ever change the set to correct it.

**It is never written to disk.** The level is per *process*: the driver says nothing at startup, so
a receiver resets to empty when the process (re)starts and waits for the next change. Persisted, a
workspace that stopped while a shell was running would come back reporting it forever — the same
argument that put `question.asking` in a var announced empty at startup.

### What it looks like

A finished turn that left something behind says so, under its answer: **Still running in the
background · 2 things**, and the rows for them. The sidebar gives such a conversation a hollow `○`
in `Status.Monitoring` — the dim active colour the panel already uses for work happening somewhere
you are not — and a folded project carries a count of what it is hiding.

Both are **still**. Motion in this workspace means *act now* (ADR 0045), and `Status.Pending` pulses
because a question stops the moment you answer it. This is the opposite: it finishes whether or not
you look, and a transcript that throbs at you over work you were told to ignore is the version of
this nobody wants.

The order on a row is asking → working → unread → running, and running is last on purpose. Every
state above it is either a block or news; this is neither. It is only a question once nothing louder
is true.

### The pipe is read before the prompt goes in

`drain_between_turns` runs at the top of every turn, before `say`. It takes what has already arrived
— a zero timeout, so a quiet process costs one poll — and splits it. What the CLI said about the
*conversation* is forwarded: those are level and edge facts, idempotent and keyed by task, and they
are the whole reason the indicator can be put right at all. The unasked turn's own content is
dropped — its `init`, its assistant blocks, its `result`. Dropped rather than drawn, because it is
an answer to a question that was never put and showing it under the one that was is the
mis-attribution this exists to stop. The CLI keeps it in its own history, so the model has not
forgotten it; only the transcript has.

### The other half: a turn that has not finished and is not doing anything

`Background` is work that outlived the turn. The mirror case is a turn that has *not* ended and has
nothing to show for itself — and on screen the two are the same problem, which is a workspace that
will not say what is going on.

`claude` sitting in retry backoff after a 529 is the one that matters. Several minutes, no tokens, no
card, nothing: indistinguishable from a wedged process, and what sends people to `^C` on a turn that
was about to come back. The CLI has always said so, on `system/api_retry` — "an API request failed
with a retryable error and will be retried after a delay", with `attempt`, `max_retries`,
`error_status` and `retry_delay_ms`. Nothing read it.

`Activity::Waiting { what, retry_in_ms }` carries it. `what` is already a sentence, because only the
driver knows what it is waiting on and a vocabulary of reasons invented here would be wrong for the
first driver that waits on something new. It goes on the working line — a state, not an event; over
in seconds or minutes; a transcript row about it would still be there next week saying nothing —
beside the turn's own clock, which is the half that makes a wait bearable.

It ends by being overtaken, and there is no `Waited` to match it: a driver that has stopped waiting
gets on with the thing it was waiting to do, and that is already an event. The next token is the
usual one. `Round::note_is_wait` is what keeps that from clearing a *tool's* note instead — a driver
that speaks while a command is still running would otherwise wipe `Running cargo test` off the line
and put the generic verb back, which is the label flicker a fixed per-turn verb exists to avoid.

`Compacting` is deliberately not folded into this. It is a wait with consequences — the plan is
dropped, the meter moves — and a note cannot carry them.

Two signals were looked at and left alone. `system/turn_duration` carries the counts behind
Claude Code's own "Waiting for N agents" line, and `system/task_summary` is a live phrase describing
what the agent is doing — but neither arrives on `--output-format stream-json`; probing the real CLI
returns only `init`, `status`, `thinking_tokens` and the task family. They are host-channel events,
and reading them would mean a transport neosh does not use.

## Consequences

The indicator is correct at every turn boundary and is put right by the next turn. It is **not** yet
live while you sit there: a shell that finishes while nothing is being asked is not heard about until
the next question, because between turns nothing reads the pipe.

Closing that gap is not a bigger version of this change, it is a different one. It needs the `Live`
handle to be readable while no turn holds it — a reader that yields the lock the moment a turn wants
it — and somewhere for the events to go, which today does not exist: `ProviderEvent` reaches the host
only through a turn's channel. An out-of-band, conversation-scoped stream from provider to host is
its own decision and gets its own ADR.

`api_retry` is read out of the CLI's schema rather than off a capture — a retry is not something a
test can arrange on demand — so the parse is written to survive being wrong about the shape: every
field is optional and a line with none of them still produces the one thing worth saying, which is
that it is waiting rather than stuck.

`local_bash` and friends are shown verbatim in the `role` column. They are the driver's words for the
kind of thing it is, and a name we invented for a kind we have never seen would be wrong on the first
install that had one.
