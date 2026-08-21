# 0038 — A conversation owns its permissions, and starts with all of them

**Status:** accepted

## Context

The permission mode was one value for the whole process. `permissions.mode` in `config.toml` set it,
`⇧⇥` changed it, and every conversation in the workspace shared whatever it currently was. Two
things were wrong with that, and they pull in opposite directions.

**A workspace is not one conversation.** ADR 0029 established that much: you have a branch you are
working on, a review you are half-way through, and a question you asked yesterday. Those are not the
same risk. A throwaway question in somebody else's checkout and the branch you have been on all week
want different answers, and a mode that followed whichever conversation you looked at last was a
setting you could only trust by re-reading it — which nobody does, because it is in the footer and
the footer is furniture.

**And `ask` was the default, which trained the wrong reflex.** The mode existed to make you think
about a gated action. What it actually produced was a prompt between the agent and every edit, in a
workspace where you had already decided to let an agent edit your code — so the prompt got dismissed
without reading, several hundred times a day, and by the time one of them was worth reading the
habit of not reading it was fully formed. `PermissionMode::Allow`'s own doc comment said people
"were going to get it one way or another — by approving reflexively, which is worse: it looks like
consent and is not". That was true of `Allow`. It was also, unnoticed, an argument against `Ask`.

## Decision

**The mode belongs to the conversation, and is saved with it.** `Session::permission_mode` is an
`Option<PermissionMode>`, persisted in the session file, and `⇧⇥` sets it on the conversation you
are in. Switching conversations moves to that conversation's mode; coming back tomorrow comes back
to it.

**`None` is not a mode.** It means this conversation has never been given one, and it falls back to
the configured default. That is what keeps the default a *default*: change `permissions.mode` in
`config.toml` and it reaches every conversation that never overrode it, rather than being frozen
into each one at the moment it was created.

The shared `PermissionLayer` therefore keeps the *configured* mode and is never written by `⇧⇥`.
This is the whole of the difference between a fallback that works and one that does not: if the
shared layer followed the active conversation, then putting one conversation into `deny` would mean
the next conversation you opened — which has no mode, and so falls back — inherited `deny` from a
decision that was never about it. And the same mechanism in the other direction carries full access
into a conversation somebody had deliberately locked down.

**The configured default is `allow`.** Named plainly as "full access", in the footer, in every mode
and not only the surprising ones, with the key that changes it beside it. Workspace containment is
unchanged and unconditional: a path outside the workspace is refused in every mode, because full
access is about not being asked, not about reaching the rest of the disk.

**A turn runs under its own conversation's mode.** `Agent::permissions_for(session, cwd)` resolves
both the mode and the root per turn, for the same reason ADR 0029 made the root per turn: several
conversations run at once, and one of them is not entitled to the settings of whichever one is on
screen. A question asked *outside* any turn — a plugin's `neosh.permit` — is answered by the
conversation being looked at, because that is the only conversation such a question can be about.

## Consequences

An agent driver holds one mode for its process rather than one per conversation, and there is no way
to reach the permission layer from inside a spawned turn — so the mode is mirrored into every agent
driver, and now has to be mirrored again on every conversation switch. That is a copy, and a copy is
a thing that can go stale; it is pushed from exactly one place (`Host::set_permission_mode`, plus the
switch) so that there is one place to look when it does.

`PermissionMode::default()` stays `Ask`. That default answers "a field was missing from a payload",
and the safe answer to a question nobody asked is still to ask. The default a *person* means lives
in `PermissionsConfig::default`, where it is a decision rather than a gap.

The approval prompt does not go away and is not quieter. It is one keystroke from being back, it is
per conversation, and the conversation you turned it on for keeps it.
