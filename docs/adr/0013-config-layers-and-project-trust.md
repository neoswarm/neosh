# 0013 — Two config layers, and project config is trust-gated

**Status:** Accepted.

## Context

ADR 0012 makes `init.ts` the configuration. Two things still need deciding.

First: is there anything a declarative file should own? "Set my model" should not require writing a
program, and a settings screen needs a file it can rewrite without reformatting someone's code.

Second, and more serious: **project-local configuration is remote code execution.** You clone a
repository, start your editor in it, and its config runs. Vim shipped `exrc` doing exactly that and
spent two decades walking it back.

## Decision: two layers, split by capability

| | `config.toml` | `init.ts` |
|---|---|---|
| Contains | data: values, names, paths | code: anything conditional |
| Can express | a subset | everything |
| Machine-writable | yes | no |
| Safe to apply from an untrusted source | partly (see below) | never |

Nothing in `config.toml` can do something `init.ts` cannot — `neosh.opt.set` reaches every option
the file does. The file exists for the two reasons above, not as a parallel mechanism.

Precedence, later winning: defaults → `config.toml` → `.neosh/config.toml` → `init.ts` →
`.neosh/init.ts` → command-line flags. Flags last, because a `--model` that lost to a config file is
an hour of someone's life.

## Decision: data applies, code requires trust

Project config lives in `<workspace>/.neosh/`. It splits on whether an entry can execute code or
grant capability:

**Applies immediately, no trust needed:**

- `model` — can only name a provider instance the user already configured. The worst a hostile
  repository achieves is picking a different one of *your* models.
- `[permissions]` — may only make things **stricter**. `mode` is taken only if it is a narrowing;
  `allow_commands` and `allow_hosts` are intersected with the global lists, so a repository cannot
  grant itself `curl`.

**Held until `neosh trust`:**

- `.neosh/init.ts` — arbitrary code.
- `plugin_dirs` — arbitrary code, one indirection away.
- `providers` — names a base URL that whole conversations get sent to.
- `[options]` — reaches anything any plugin declared.
- `system_prompt` — the argument for holding this one is the least obvious, so: the agent reads the
  repository's files anyway, and an `AGENTS.md` is accepted practice. But a *system* prompt occupies
  a structurally stronger position than any file content, and the cost of holding it is that a repo
  must ask once. That is a cheap insurance premium.

Held entries are reported in the UI naming what was withheld, not silently dropped. A security
control nobody can see is a bug report waiting to happen.

## Trust is keyed on content, not path

The trust store maps a canonicalized project path to a SHA-256 digest over **every file under
`.neosh/`**, each contributing its relative path, length and bytes.

The first version hashed exactly `config.toml` and `init.ts`, which was a hole found by review:
`.neosh/init.ts` can `import "./setup.ts"`, and that file was then free to change without revoking
trust — precisely the `git pull` scenario the content hash exists to catch. Anything reachable from
a trusted entry point has to be covered, and the only way to be sure is to cover the directory.

The walk uses `symlink_metadata` and stops at depth 8, so a symlink loop cannot turn startup into a
filesystem crawl. `neosh trust` prints every covered file, not just the entry points — approving
what you have not been shown is not approval.

Path-only allow-listing is the failure mode: you audit a repository once, and every `git pull`
afterwards runs whatever arrived. Hashing the contents makes revocation automatic. Editing a trusted
file downgrades it to `Stale`, which is reported differently from never-trusted — "this changed
since you approved it" is a materially different message from "this is new".

`neosh trust` prints the files before recording the decision. A confirmation prompt with nothing on
screen is a rubber stamp.

A corrupt trust store resets to "nothing is trusted" rather than being fatal. That is the safe
reading, and refusing to start would strand the user with no way to fix it.

## Paths

XDG, four roots: config (what you wrote), data (what a manager installed), state (the trust store),
cache (derived). A single `~/.neosh` mixing hand-edited files with a plugin cache is a directory
nobody can back up correctly.

Every root is overridable by environment variable, and `--config-dir` overrides the config root.
That is not a convenience — the end-to-end tests need a config directory that is not the developer's
own, and a config system you cannot point elsewhere is one you cannot test.

`--clean` reads nothing and writes nothing. The bug-report flag.

## Consequences

- A repository can pin its model in a committed file with no friction, which is the common case.
- The dangerous cases require one command, once, and again after the file changes.
- The rules live in a struct definition (`ProjectConfig`) that someone can read to answer "what can
  a repo I cloned say about my editor", rather than being spread across the loader.
