# 0057 — A request nobody answers is a turn that never ends

**Status:** accepted

## Context

Reported as *"it did not update or anything… it's been running for more than 1.5 hours and
nothing"*. The footer said `Running ExitPlanMode… 93m 15s`, with `esc to interrupt` beside it, over
a conversation that had drawn its last row an hour and a half earlier.

The process was alive and idle. Its own transcript ended on the `tool_use`, with no `tool_result`
after it and no write to the file since:

```text
== assistant 2026-08-23T09:29:45.571Z
  tool_use: ExitPlanMode toolu_01Vbso… {"plan": "# Independent views: a terminal is a place you are…
```

`ExitPlanMode` asks before it acts, and it asks over the control protocol — **even under
`--dangerously-skip-permissions`**, which is the flag full access is launched with. Driven by hand,
with the same flags and the stream held open, the CLI sends this and then says nothing at all:

```json
{"type":"control_request","request_id":"48d09a8a…","request":{
  "subtype":"can_use_tool","tool_name":"ExitPlanMode",
  "input":{"plan":"…","planFilePath":"…"},
  "tool_use_id":"toolu_01Vbso…","requires_user_interaction":true}}
```

ADR 0043 established that a question is not a permission, and split the two paths. It split them on
two different tests, and that is the bug:

- `can_use_tool` refused anything `is_question` claimed, and `is_question` was true on
  `requires_user_interaction` **alone**.
- `ask_user_question` claimed only a request carrying an `input.questions` array.

`ExitPlanMode` sets the flag and carries a *plan*. So the permission path stood aside for the
question path, the question path declined it for want of a question, and the line fell past both
into handlers that only match `control_response`. Nothing wrote a reply. The CLI is entitled to wait
forever for one, and did.

The flag was never wrong about what it says. It says *something here has to reach a person* — which
is true of a plan, a permission and a question alike — and it was read as *this is a question*. The
comment on `is_question` even said so: "it is set for anything that must reach a person, whatever it
ends up being called."

## Decision

### Whether it is a question is decided once

`can_use_tool` no longer re-decides it. It asks the other function:

```rust
if ask_user_question(v).is_some() {
    return None;
}
```

One parse, one owner. The two classifiers cannot disagree about a request because only one of them
has an opinion, and what makes something a question here is what it always should have been: that
there is a question in it. `is_question` survives as a *precondition* of `ask_user_question` — the
flag and the tool name are still how a renamed tool and an older install are recognised — and is
private, because on its own it decides nothing.

This is also what the pre-existing test
`a_request_with_no_answerable_question_in_it_is_not_one` claimed and did not check. Its comment said
a dialog with no rows "goes down the ordinary path and is refused like anything else"; it asserted
only that the question path declined it. The permission path had declined it too.

### And nothing the CLI asks can go unanswered

Fixing the classifier fixes `ExitPlanMode`. It does not fix the shape of the failure, which is that
a line no handler claimed is silently discarded — and on this protocol, discarding a request is not
ignoring an event, it is stopping a turn.

So the reader ends with a backstop. Any `control_request` that reached the bottom is refused out
loud, by request id:

```json
{"type":"control_response","response":{"subtype":"error","request_id":"…",
 "error":"neosh does not handle something_invented_later"}}
```

Deliberately last and deliberately dumb. It is for the subtype the next release of the CLI invents,
and the right fix for any particular one of them is a handler above it, not a better sentence here.
A refusal ends a turn badly and visibly; silence does not end it at all, and only one of those is
recoverable from the keyboard.

## Consequences

**Leaving plan mode works, and it is an ordinary permission.** Verified against the real CLI: the
`allow` neosh already builds — `behavior`, `decisionClassification`, `toolUseID` — is answered with
`User has approved exiting plan mode.` Under full access, policy says yes and nobody is interrupted;
under `ask`, it is a prompt like any other, and the plan is what the prompt is about.

**Every tool that must reach a person is now claimed by exactly one path**, and that is asserted
rather than assumed. `ExitPlanMode` was simply the first such tool to exist; the flag will be set by
the next one too, and it will land in the permission path instead of nowhere.

**The class of bug is closed, not just the instance.** `a_request_to_leave_plan_mode_is_answered_rather_than_dropped`
drives a stand-in CLI that behaves the way the real one does — it blocks until it is answered — so
the test for an unanswered request is a turn that never ends. Without the fix it fails on a 20-second
timeout, which is the symptom, scaled down.

**The backstop is a refusal, not an answer.** A subtype neosh does not implement still fails; it
fails in the turn, where the model can say so, instead of in a pipe. If that refusal ever turns out
to be the wrong answer for some future request, the evidence will be a turn that reported something
rather than a workspace somebody restarted.
