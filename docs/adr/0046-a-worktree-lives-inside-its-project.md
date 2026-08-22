# 0046 — A worktree lives inside its project

**Status:** accepted

Refines [0029](0029-a-conversation-carries-its-directory.md), which said "a worktree is a
project", and [0042](0042-somewhere-clean-to-try-this.md), which made getting one free.

## Context

0042 made worktrees cheap enough to use casually, and casual use is what found the two problems.

**Four scratch trees of one repository read as four unrelated projects.** "A worktree is a
project" was the right call about *capability* — a worktree needs its own conversations, its own
branch in the footer, its own fold — and the wrong call about *layout*. The sidebar listed
`neosh`, `neosh · brisk-otter`, `neosh · swift-thistle` and `neosh · fix/thing` as peers, and the
name was carrying the relationship precisely because the structure did not: every row had to
repeat the repository so that a reader could reassemble, in their head, the tree the panel had
flattened. The thing the trees have in common is the thing the column never said.

**Worktrees elsewhere are worktrees you forget.** `worktree.root` defaults to a directory of
neosh's own, and that remains right — a worktree is not part of the project, and littering the
repository's parent is how checkouts stop being accounted for. But plenty of people keep their
trees *inside* the repository, `.worktrees/`, where `rm -rf` of the project takes everything with
it and nothing lives outside the tree they already back up. The relative-path case half worked:
`worktree.root = ".worktrees"` resolved against the repository root, then appended a `<repo>`
level that is pure noise inside the repository it names — and git reported the new checkout as one
giant untracked directory in every `git status`, forever, because nothing excluded it.

## Decision

**The repository is the row; its worktrees nest under it, named by their branch.** `SessionInfo`
grows `repo_root` — the main checkout's path, equal to `cwd` in the main checkout itself — and
`branch`, because the sidebar was grouping on a *display string* and a string made for reading is
not a key: the only way to find `neosh · fix/thing`'s siblings in it is to parse a format back
apart. The host already had both in hand when it built the name (`remember_repo` calls
`identity()`) and threw them away.

A nested worktree is an ordinary `Project` to the panel — its own cwd, so its own fold, rank and
`n` — and its row carries the branch alone, because the repository's name is said once by the row
above. Counts fold upward: a folded repository counts and marks everything inside it, worktrees
included, and its recency is its newest conversation *anywhere in it*, so a repository whose only
activity is in a scratch tree floats with that work. `f` on a worktree pins the repository, the
same way `f` on a conversation pins the project it is in — pinning is about the top of the column,
and a worktree is never at the top of the column. A repository whose every conversation is in its
worktrees still gets a row of its own to hang them from; `↵` on it folds, and `n` on it is how a
conversation starts in the main checkout — which is also the answer to "go back to main and pull":
the checkout is one row up from the tree you were in, never elsewhere in the list.

**A relative `worktree.root` is inside the repository, laid out `<repo>/<configured>/<branch>`.**
No `<repo>` level — inside the repository, nothing else's worktrees can land there, so the
disambiguator disambiguates nothing. And `git worktree add` into the repository writes the
worktrees' directory into the repository's **`.gitignore`** — `/.worktrees/`, the parent, one line
covering every tree after it — or every `git status` reads as one giant untracked directory. The
tracked file rather than `.git/info/exclude`, which was the first version of this: the exclude
file is per clone, so a colleague pulling the layout — or you, on the next machine — rediscovered
the noise the entry existed to prevent. Dirtying the working tree is the honest cost, and it is
one line that commits with the work. Nothing is written when `git check-ignore` says the path is
already covered — the line from last time, a broader rule, a global ignore — so opting out is
writing your own rule. It is written in `neosh-vcs`, against the *main* checkout resolved from
`--git-common-dir` rather than against whichever worktree the command ran from, and best-effort:
the worktree exists by the time it runs, and failing the call over a status-noise fix would report
an operation as failed after it worked.

## Consequences

The `project` display string is unchanged — `neosh · fix/thing` still names a worktree conversation
everywhere a flat label is the right shape: the archive picker, another machine's inventory, a
title bar. Structure where there is structure, the string where there is a string.

A pinned worktree directory with no conversations has nobody to say which repository it belongs
to, and stays a top-level row until a conversation gives it one. Graceful, and honest: the panel
nests on knowledge, not on guesses.

`repo_root` is stamped when a directory is first seen and cached, like the name it feeds. A
repository moved on disk mid-session keeps its old grouping until the workspace looks again, which
is the existing bargain for every fact in that cache.

Sub-directory conversations group under their repository too, labelled by branch — the same
approximation `project_name` has always made. The alternative is a third field that says "linked
worktree" as distinct from "inside the main checkout", and nothing yet needs the distinction badly
enough to pay for it.
