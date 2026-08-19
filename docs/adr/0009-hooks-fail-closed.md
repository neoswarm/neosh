# 0009 — Blocking hooks fail closed; observers cannot veto

**Status:** Accepted.

## Context

"A plugin must be able to intercept, modify, or veto a tool call. This is how policy, audit, and
permission plugins get written." Interception implies the agent loop *waits* on a plugin, which
means a plugin can now stall or crash the loop.

## Decision

Hooks declare `blocking` at registration.

- **`blocking: false` (default)** — a pure observer. Fired and forgotten; its return value is
  ignored and it is never awaited.
- **`blocking: true`** — awaited with a timeout. May `continue`, `modify` the payload, or `veto`.

And: **a blocking hook that does not answer within its timeout is treated as a veto.**

## Why timeouts veto rather than continue

The alternative is a permission layer that stops working precisely when something is wrong with it.
A wedged policy plugin failing open is worse than the agent refusing to act — the refusal is visible
and recoverable, the silent bypass is neither.

Observers cannot cause this, because they are never awaited. That is the point of the split: an
audit or telemetry plugin is *structurally incapable* of wedging the agent loop, rather than merely
advised not to.

## Three details that follow

**A veto short-circuits.** Once an action is refused there is no reason to keep asking, and later
hooks must not be able to overturn a refusal.

**A veto is reported to the model** as an error `tool_result`, not swallowed. The model needs to
know it was blocked so it can explain or route around it; a turn that silently loses a tool call is
indistinguishable from a broken tool.

**A `modify` that cannot be decoded becomes a veto.** Silently ignoring an unusable payload would
look exactly like the modification was applied — the worst failure mode for a plugin whose job is
rewriting arguments.

## Observers see attempts, not just outcomes

`tool.pre` fires at its observers *before* blocking hooks run, so an audit plugin records calls that
were subsequently refused. An audit log that only contains calls which survived policy is blind to
the events it exists to record. (This was found by a test, not by design — the first implementation
only fired `tool.post` observers.)

## Lock discipline

`run_blocking` takes an owned `Vec<HookReg>` rather than borrowing the registry. Borrowing would
hold the registry lock across every hook call, so a plugin registering a hook from inside a hook
would deadlock — and the resulting future would not be `Send`, so the turn could not be spawned.
