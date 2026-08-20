# 0022 — A key is typed into the host, and never crosses a boundary

**Status:** accepted

## Context

Until now the only way to authenticate was to export an environment variable before starting neosh.
That is fine for a machine you set up once and wrong for every other case: the first thing a new
user does is pick Opus from the model list, and the first thing neosh does is fail on send with
`ANTHROPIC_API_KEY is not set` — a message about a variable, delivered at the moment they were
asking a question, about a list that had already offered them the thing that could not work.

So a key has to be enterable from inside the program. The constraint that governs how is one line in
the brief and it is not negotiable: **secrets never touch disk in plaintext and never appear in logs
or event payloads.**

That rules out the obvious implementation. `prompt()` in `@neosh/api/ui` is a perfectly good text
field, and every character typed into it goes into a buffer, and every buffer is sent to the
frontend as a `UiEvent`. Under `--ui-protocol=stdio` that is a pipe to another process; in the
terminal frontend it is a struct that anything could log. A key typed into a plugin field has
already leaked by the time the plugin sees it.

## Decision

Three things, in the order they matter.

**1. The host reads the keystrokes.** `ApiCall::ProviderSetCredential { instance }` does not open a
plugin widget. It puts the *host* into a state where every key — bindings included, before the
keymap table — accumulates into a `String` the host owns, and what gets drawn is a row of bullets.
The call does not settle until the prompt closes, and what it answers is a boolean: whether a key
was stored. There is deliberately **no API call that returns a secret**, so a plugin cannot leak one
because it cannot read one.

**2. Four places a key can come from, in one order.** A key typed in this session, then the OS
keychain, then a credential helper named in config, then the environment. Typed wins because you
typed it after seeing what the environment gave you.

**3. The keychain is a subprocess, not a library.** `secret-tool` (libsecret) and `security` (macOS)
are already installed, already unlocked with the login session, and already audited. The secret goes
over the helper's **stdin**, never its argv, so it is not visible in `ps`.

`AuthRef` gained a `Command { argv }` variant alongside `Env`: the git-credential-helper pattern, so
`pass`, `op`, `bw` and `gopass` all work without neosh knowing any of them exist.

## Why not a plugin field with a `mask: true` option

Because masking is a rendering decision and the leak is a transport one. `setLines(buf, ["••••"])`
draws bullets, but the plugin still holds the value in a JS string, in a runtime the host does not
control the lifetime of, and any plugin that wanted to could keep it. Making the *host* the only
thing that ever holds the characters is the difference between a convention and a property.

The cost is real: this is the one modal in neosh that a plugin cannot re-skin, and it is the one
place where the "everything is on the public API" rule bends. It bends the right way — a plugin can
*ask* for a key to be collected, it just cannot be the thing that collects it.

## Why not a library keyring

`keyring-rs` would work and would pull a dependency tree in to do, less portably, what a program on
`$PATH` already does — and on the platforms where the library is a stub it shells out anyway. The
subprocess also fails gracefully in the case that actually happens: `secret-tool` installed but no
session bus, over SSH. A key that cannot be persisted is still kept for the run, and neosh says
which happened rather than pretending.

## Consequences

- **A key entered over SSH without a keyring is gone at exit.** That is stated at the moment it
  happens, with the two ways to fix it — install a helper, or point `auth` at a command.
- **Looking a key up costs a process launch.** Every answer is cached, *including the misses*, and
  instances with nowhere to put a key are never looked up at all. The first cut cached only hits,
  which put a dozen `secret-tool` launches on the redraw path and took startup from a tenth of a
  second to a timeout. The test that caught it is a regression test now.
- **`neosh --list-models` reports provenance**, not just whether a variable is set, because "it
  works and I do not know why" and "it works because the key is in my keychain" are different states
  to be in.
- **The status line says `no key` beside the model name.** The point is to move the discovery from
  after you have typed the question to before.
