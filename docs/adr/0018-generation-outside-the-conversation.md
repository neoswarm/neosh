# 0018 — Generated names and messages never enter the conversation

**Status:** Accepted.

## Context

A workspace like this generates a lot of small text with a model: branch names, commit messages,
thread titles, pull-request descriptions. All of it needs a provider round trip. None of it is part
of the conversation.

The tempting implementation is to reuse `agent.send`. It is already there, it already streams, it
already handles providers.

It is also wrong in a way that is invisible until it bites. Asking for a commit message through
`agent.send` appends "You write git commit messages…" and the model's answer to the session. The
next thing the user asks is answered by a model that believes it was recently asked to write a
commit message. Worse, "what were we doing?" now returns the commit message.

## Decision

`neosh.gen.complete(prompt)` and `neosh.gen.json(prompt)`: one prompt, one answer, sharing almost
nothing with the turn loop. No session history is read or written, no tools are advertised, no hooks
fire, no `AgentEvent` is emitted.

Two pieces of policy live in the core rather than in each caller:

**Model choice.** `gen.model` picks what these use, falling back to the conversation's own model. It
belongs here because "use something cheap for the small stuff" is a setting every generating plugin
wants and none should reimplement. Naming a branch does not need a frontier model.

**Finding the JSON.** Asking for JSON gets you JSON most of the time. The rest of the time it is
fenced in ` ```json `, prefixed with "Sure!", or both. `gen.json` tolerates all of it. The scan is
for a *balanced bracket pair that parses*, tried from each candidate start — not "first `{` to last
`}`", which fails on prose containing `{placeholder}` before the real object, and not "first
balanced pair", which fails on the same input for the same reason.

## The prompts are settings, not constants

The prompts themselves live in the bundled `git` plugin, as declared options, in two layers:

- `git.branch.instructions` — appended to the default. The common case: "start with the Jira key".
- `git.branch.prompt` — replaces it wholesale. The escape hatch.

Instructions apply on top of a replaced prompt too, so a user with their own prompt can still layer a
per-project rule from `.neosh/config.toml`.

Keeping these in a plugin rather than the core is the same call as ADR 0016: the *capability* is a
model round trip, and that is core; the *prompt* is interface, and interface belongs where a user can
replace it — including by disabling the plugin and shipping their own.

## Consequences

- Anything can generate text without polluting the conversation, including plugins that have nothing
  to do with git.
- A model that returns prose instead of JSON produces an error naming what it returned, truncated on
  a character boundary — not a parse panic and not a silent empty string.
- One-shot generation is a spawned task, like every git call, because a model round trip on the host
  loop would freeze the interface for seconds.
