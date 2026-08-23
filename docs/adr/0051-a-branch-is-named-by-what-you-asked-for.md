# 0051 — A branch is named by what you asked for

## What happened

`^N` → *In a new worktree* gives you a checkout on a branch called `spry-reef`. That is deliberate
and ADR 0046 argues for it: naming a branch before the work is a decision made at the worst possible
moment, and every one of those is a reason not to start. Two words from a word list are a fine thing
to *begin* on.

They are a poor thing to find a week later. `git branch` in a repository somebody has been working
in reads:

```text
  amber-heron
  brisk-otter
  hardy-quartz
  spry-reef
* wily-nimbus
```

Five branches, four of them somebody else's problem, and nothing on the list says which one is the
rate limiter. The panel is no better — a worktree row *is* its branch — so the column whose job is
to tell you where your work is says `wily-nimbus` about all of it. The name was never wrong; it was
simply chosen before there was anything to choose it from.

The information arrives about ninety seconds later, on its own, and nobody has to be asked for it:
the first message sent in that worktree is exactly the description a branch name is made of. t3code
does this — a temporary `t3code/<8 hex>` branch, regenerated and renamed on the first turn — and it
is right for the reason the scratch name is right. Both halves are the same argument: do not ask for
a decision at the moment you cannot make it, and do not throw the decision away when it arrives.

## The decision

**A worktree names itself from the first thing asked in it.** `git.worktree.new.auto` and
`git.worktree.new.inside` create the branch they always did; the first turn started in that
conversation renames it — `wily-nimbus` becomes `fix/composer-paste-truncation` — and nothing here
touches it again.

**Only a name nobody chose.** The mark is a session var, `git.branch.scratch`, written at the one
moment the difference is known: the plugin that generated `wily-nimbus` says so. t3code recognises
its own temporary branches by their *shape*, which works until a real branch happens to look like
one and which cannot tell `brisk-otter` that we generated from `brisk-otter` that you typed. Writing
it down costs one key and removes the guess. It is removed the moment the branch is named, after
which this is an ordinary worktree.

**At turn start, not turn end.** The panel is drawn all the way through a turn, and the row you are
watching should say what you asked for rather than what a word list picked. `git branch -m` is one
ref write: the working tree is untouched, so it is safe under a running agent and safe on a branch
with uncommitted work on it — which is the realistic state at the only moment there is enough
information to name the branch.

**One attempt, ever.** The mark is removed *before* the model is asked, so a cheap model that
hiccups costs one request rather than one per message for the life of the conversation. This is the
rule the titles plugin arrived at from the same direction. Too *early* is not a failure and does not
spend it: a turn with nothing readable yet leaves the mark alone.

**The type is the model's, and the prefix is for a name that arrived without one.** The built-in
prompt asks for `feature/`, `fix/`, `chore/`, `refactor/`, `docs/`, `test/` or `perf/`, chosen from
what the work *is* rather than from the words used to ask for it. `git.branch.prefix` therefore
applies only when the generated name has no namespace of its own — a prefix bolted on
unconditionally reads `feature/fix/the-login-redirect`, which is a namespace nobody meant under a
type that is now wrong. Somebody who wants the prefix always has the prompt, which is a setting for
exactly this reason.

**Every part of it is a setting, and every default is the one you already have.**
`git.branch.prompt` replaces the prompt, `git.branch.instructions` appends to it,
`git.branch.model` names the model, `git.branch.auto` turns the whole thing off. An unset
`git.branch.model` falls to `gen.model` and then to the model the conversation is using — the
plugin returns *no selection at all* rather than guessing, because the fallback is the host's and
duplicating it is how the two come to disagree. Out of the box a branch is named by whatever you are
already talking to, which is the right answer in a workspace where the cheap model was never
configured and the alternative is failing.

**The host puts its own label right.** This is the part that was not obvious. `ProjectFacts` — the
display name, the repository root and the branch — is asked once, the first time the host sees a
directory, and read on every redraw of every panel. That is correct: a label is not worth a
subprocess per frame. It is wrong for exactly one moment, and this feature is that moment. So
`GitRenameBranch` is handled *on the host loop* rather than through `spawn_slow`, alone among the
git writes: it re-reads the facts for every directory that was on the old branch and announces it,
and without that the panel says `wily-nimbus` under a conversation working on
`fix/composer-paste-truncation` until the workspace is restarted. ADR 0036's rule from the other
end — a value the host *kept* has to be put right by whatever changed it.

Matched on the cached branch rather than on the directory the git call ran in, because one rename
can move several rows: `cwd` names a checkout, and the branch may be checked out in a worktree that
is a different directory from it.

## What this does not do

**It does not rename the directory.** `~/.nsh/neosh/wily-nimbus` stays `wily-nimbus` while the
branch becomes `fix/composer-paste-truncation`. Moving it would invalidate the conversation's `cwd`,
the agent's own resolved paths and any process it has open — a much larger promise than a better
name is worth. The panel names the row by its branch, so the stale directory is visible only to
somebody looking at the path, and `y` still copies a path that works.

**It does not make a branch the default.** `^N` still asks where, and `Here` is still the selected
row. This changes what the worktree rows *give* you, not which one is chosen for you.

**It does not touch a branch you named.** `git.worktree.new feat/thing` is yours, is never marked,
and is never renamed.
