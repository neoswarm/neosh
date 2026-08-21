# 0039 — A conversation outlives the agent holding it

**Status:** accepted

## Context

An agent driver does not send the conversation. `claude` in stream-json mode takes one prompt at a
time and keeps the history on its own side; `codex app-server` and ACP do the same. What comes back
is a handle — a session id — and the deal is that handing it over next time is what makes the second
turn a continuation of the first rather than a fresh start.

neosh honoured half of that deal. `ClaudeCliProvider` kept the handle in `Live::session`, in memory,
inside the conversation's slot, and used it when it had to replace the process mid-run: a change of
directory or of permission mode relaunches the CLI, and the replacement carried `--resume` so the
turn continued where it left off. Within one workspace this was invisible and correct.

Across two, it was neither. Nothing wrote the handle down. Stop the workspace, start it again, open
a conversation with a hundred messages in it, and the driver had no handle to offer — so it spawned
a CLI that had never heard of any of it and told it only the newest thing you said. The transcript
on screen was complete. The agent's memory was one line long. **Neither side reports this**, because
from the driver's point of view a new conversation is a perfectly ordinary thing to start.

What made it visible was `/compact` finishing in three seconds. Compaction is a summarisation call:
measured against a real CLI it takes fourteen seconds on a twenty-thousand-token conversation and
longer on a large one. Three seconds is the CLI answering `Error: No messages to compact` — the only
externally visible symptom of an agent that had forgotten everything, and it reads as a feature that
does not work rather than as a conversation that is gone.

## Decision

**The handle belongs to the conversation.** `Session::resume` holds what the driver called this
conversation, it is persisted in the session file beside the messages, and it is handed back on
every turn as `TurnRequest::resume`. A driver that keeps state on its own side reads that field
instead of hoping a process from earlier is still around.

**The driver says the handle out loud.** `Activity::Resume { token }` is how it travels — the same
family as everything else a driver's loop reports about itself, and for the same reason: it is not
content, `TurnAssembler` never reads it, and the host is what keeps it. Said whenever it changes
rather than once, because resuming does not always keep the id it was given, and a conversation
filed under an id the CLI has stopped using resumes into a history missing everything since.

**A handle that no longer resolves is thrown away, not reported.** The CLI says
`No conversation found with session ID` on an ordinary `result` line with `is_error`. The driver
recognises that one sentence, forwards nothing, and the turn is run again from the top without the
handle. The history is already lost at that point — the user's own `~/.claude` was cleared, or the
project moved — and failing the turn as well would add a second loss to the first. Retried exactly
once, so a driver that says it about every attempt cannot loop.

## Consequences

A conversation reopened next week is the conversation, not a transcript of one. `/compact` compacts.
The context meter, which reads the CLI's own accounting, is about the conversation you are looking
at rather than about a session that has just started.

`Activity::Resume` is deliberately not claude-specific: `codex`'s conversation id and ACP's session
are the same shape of value with the same lifetime problem, and they can be moved onto it without a
second mechanism. They have not been yet — ACP is the case that needs thinking about, because its
session lives on the agent's side and an agent that is not running has no session to resume.

What is stored is an opaque handle to history the vendor CLI already keeps on disk. It is not a
secret and it is not content, but it does point at a transcript: `sessions.rs` was already the file
holding everything you said, and this does not widen what is on disk.
