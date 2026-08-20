# 0030 — A turn belongs to a conversation, not to the program

**Status:** accepted

## Context

Everything about a turn was singular. The host held one `active_turn: Option<CancellationToken>`.
The agent held one `steering: Vec<String>`. Every read and write inside the turn loop went through
`self.session()`, which means *the conversation on screen*. `AgentEvent` said which turn an event
belonged to and never which conversation.

That last omission is what made the rest inevitable. If a `Token` event does not say where it came
from, the only thing the host can do with it is append it to whatever transcript is open — so a turn
running in a conversation you are not looking at would write its answer into the one you are. Rather
than fix the event, we forbade the situation: switching, closing and archiving all returned `Busy`
while a turn was running, with the message *"a turn is running — interrupt it first"*.

The comment on that check was honest about what it was for:

> The turn writes into the chat buffer as it streams; switching underneath it would interleave one
> conversation's output into another's transcript.

True, and the wrong conclusion. It made a model that takes four minutes to answer into four minutes
in which the workspace holds one conversation — you cannot read the thread next to it, cannot start
the unrelated thing you thought of while waiting, and cannot go and look at yesterday's answer. A
workspace that runs one thing at a time is a terminal with extra steps, and `neosh` groups
conversations by project on the assumption that you have several.

It also produced a subtler wrong answer. `PermissionLayer` has one workspace root, moved by
[ADR 0029](0029-a-conversation-carries-its-directory.md) whenever the active conversation changes.
With one turn at a time that is exactly right. With two, `src/main.rs` inside a running turn would
resolve against whichever project you had most recently *looked at*.

## Decision

**A turn is addressed by conversation, at every layer.**

- `Agent::run_turn_in(bridge, session, text, cancel)` is the real entry point. It reaches its
  conversation by id through `SessionStore::get_mut`, so switching does not move it, and a
  conversation closed underneath it reads as a cancellation — there is nowhere left to put the
  answer. `run_turn_with` remains, meaning "in the one on screen".
- **Every `AgentEvent` variant carries its `SessionId`**, and so do the six `PluginEvent` turn
  variants. A plugin drawing a transcript has the same problem the host had, and needs the same
  answer.
- **Steering lives on the `Session`.** What you type belongs to the conversation you typed it in,
  not to whichever turn reaches a gap first.
- **The host keeps a map, not an `Option`**: `turns: HashMap<SessionId, CancellationToken>` and
  `rounds: HashMap<SessionId, Round>`. `<Esc>` and `^C` cancel the turn of the conversation you are
  looking at, which is the only one you could have meant.
- **A turn's permission layer is rooted at its own conversation** — `PermissionLayer::rooted_at` —
  rather than at the shared root, which follows the screen.
- **Switching and archiving are never refused. Closing cancels.** Closing is the one case where
  there is about to be nowhere to put the answer.

The host now separates two jobs that used to be one. *Bookkeeping* — what a conversation has said,
whether its turn is still running, what to persist — happens for every event whatever is on screen.
*Drawing* happens only for events from the conversation you are looking at. One `on_screen` boolean
at the top of `handle_agent_event`, and the interleaving the `Busy` check existed to prevent is
prevented by construction instead.

## The round that is not in the messages yet

`enter_session` rebuilds the transcript from `session.messages`, deliberately: the messages are the
truth, and replaying a log of UI edits would drift the moment anything else touched the buffer.

But a round's assistant message only lands in `messages` when its stream closes. Switching into a
conversation halfway through an answer would therefore show the question, no answer, and then the
second half of the answer arriving out of nowhere — the failure looks like corruption, not like a
missing feature.

So the host keeps, per running conversation, the text streamed in the round now in progress, and
`replay_round` puts it back on entry. It is cleared the moment that round's message lands — which is
`ToolStarted`, or the end of the turn — because from then on it would be a second copy of something
already recorded, and a second copy is a copy that can go stale.

Text or the working line, never both, because that is exactly what the live path does: the first
token takes the working line away. The elapsed clock is per conversation too, so coming back shows
the time the turn has actually been running rather than restarting at zero.

## Consequences

- Several conversations answer at once. The sidebar already drew this — `◍` and an elapsed clock on
  any working row, not just the active one — it was simply unreachable.
- `PluginEvent::Token` and friends gained a field. Additive for a listener that ignores it, and
  required for one that draws.
- The steering queue is no longer visible process-wide. `agent.steering_count()` became
  `agent.steering_count(&session)`, which is a better question anyway.
- A turn whose conversation is closed is cancelled rather than orphaned. Archiving is not closing
  and does not stop anything: putting a conversation away is about the list you read, not the work.
- Nothing here makes *one* conversation run two turns. Sending while one is running is still
  steering, and still the right answer — a follow-up that starts a second turn has lost the chance
  to change what the first one does.
