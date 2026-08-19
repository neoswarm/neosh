# 0016 — Capabilities in the core, interface in plugins

**Status:** Accepted.

## Context

The target feature set is a full agent workspace: a sidebar, a model switcher, a reasoning-effort
switcher, a command palette, a branch toolbar, generated branch names and commit messages, diffs,
worktrees, threads. The question is where each of those lives.

The project's thesis says "everything is a plugin". Taken naively that is wrong in a specific and
important way: the plugin runtime is a bare `deno_core` with no filesystem, no network and no
process access. A sidebar written as a plugin cannot run `git status`. So "make it a plugin" is only
an answer once the core has grown the verb the plugin needs.

The failure mode on the other side is worse and more common: build the sidebar into the host because
it needs git, and now the sidebar is unrepleaceable, its layout is not yours, and the next person who
wants a different one has to fork the editor.

## Decision

Split on **capability versus interface**, not on feature.

**The core owns capabilities** — anything requiring an ability the sandbox deliberately withholds:

| Namespace | Why it cannot be a plugin |
|---|---|
| `neosh.git` | Process access. Also: one implementation means a sidebar, a branch picker and a commit UI agree about what "dirty" means. |
| `neosh.gen` | A provider round trip, outside session history. |
| `neosh.opt`, `neosh.keymap`, `neosh.buf`, … | Shared state with a single writer. |

**Plugins own the interface** — every pixel and every key. The sidebar, the model switcher, the
effort switcher and the git actions ship *as plugins*, embedded in the binary but otherwise
ordinary: a `plugin.toml`, an `activate`, and nothing in their source that a third party could not
write. They are discovered through the same path as a plugin on disk, they are listed by
`cmd.list()`, their keys are rebindable, and `plugins.disabled = ["sidebar"]` turns one off.

**Shared widgets are a library, not a feature.** Every switcher in the product is the same control: a
filterable list with a preview. Written once as `@neosh/api/ui` — a TypeScript module importable by
any plugin, implemented entirely on the public API. Not in the core, because a picker is not a
capability; not copy-pasted into each plugin, because five inconsistent pickers is what that
produces.

## Why bundling them in the binary is not cheating

A bundled plugin is embedded source with a `neosh:builtin/<name>` URL instead of a `file://` one.
It gets no extra API, no private call, and no exemption from the manifest. The test that keeps this
honest is `scripts/check.sh`: the bundled plugins are type-checked against the *published* `@neosh/api`,
so the moment one of them reaches for something not public, the build fails.

That is the point of building them this way. Writing the sidebar as a plugin is how we found that
the API had no way to receive a raw keystroke (ADR 0017) and no way for a frontend to invoke a
command by name. Both were missing for *everyone*; building the sidebar into the host would have
hidden both.

## Consequences

- Supporting a new vendor is a plugin. A different sidebar is a plugin. A different picker is a
  library. None of them is a fork.
- The core grows verbs, not screens. When a feature does not fit, the question is "what capability
  is missing", not "where in the host does this go".
- Bundled plugins are constrained to a single entry module, because a `neosh:` URL cannot resolve a
  relative import. Cheaper than teaching the loader a second module system.
- Repository *writes* are gated per plugin, from the manifest (`permissions = ["vcs_write"]`), not by
  the agent's permission mode. Those modes govern what the **model** may do; a user pressing Enter in
  a branch picker is not the agent acting. The model reaches git the same way it reaches anything —
  through a registered tool, gated like every tool.
