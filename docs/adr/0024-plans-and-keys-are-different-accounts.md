# 0024 — A plan and an API key are different accounts

**Status:** accepted

## Context

Every provider was an `InstanceConfig` in one flat list. `claude-cli` — which uses a subscription you
already pay for and needs no key at all — sat in that list beside `anthropic`, which is billed per
token against a key you supply. Nothing in the type said which was which.

The consequence was not cosmetic. Someone picking "Claude Opus 5" from the model list got
`ANTHROPIC_API_KEY is not set` on send, while three rows above sat "Claude Opus (via CLI)" which
would have worked immediately. The list had offered them the thing that could not work, said nothing
about why, and put the fix somewhere else entirely. "It says I have no API key" was an entirely
reasonable thing to conclude.

## Decision

Two things.

**`AuthRef::Cli { program, login }`** — a plan behind a vendor CLI. neosh holds no token, checks only
that `program` is on `$PATH`, and quotes `login` when it is not. `AuthRef::Inherited` stays as the
door for a plan that is *not* a CLI: a plugin that runs its own OAuth registers a driver with it, and
neither the core nor any picker learns anything new.

**`AccountKind`** — `Plan | ApiKey | Local`, derived from the `AuthRef`. Everything else follows from
it: how a picker groups, which sign-in verb to offer, whether to show a price.

`CredentialSource` grew `Plan { via }` and `PlanMissing { program, hint }` to match.

## What is deliberately not claimed

**Whether you are signed in to the CLI.** Finding out means running it, and running a subprocess per
provider every time a picker opens makes opening a picker slow in proportion to how well configured
you are. Whether the program *exists* is cheap and is a different question with a different fix —
"install codex" and "run `codex login`" are not the same problem, and a picker that conflated them
would send you to the wrong page.

So the rail says `your claude login`, not `signed in ✓`. It says where to look, which is true, rather
than what it found, which it did not look for.

## Consequences

- **Only one plan ships**, `claude-cli`. Model lists exist for `codex` and `gemini` and are not
  shipped, because the drivers behind them are not written and could not be tested here. Listing a
  provider neosh cannot drive would be a worse lie than listing none. `AuthRef::Cli` works for any
  program, so a config entry or a plugin adds one without touching this.
- **A plan cannot take a key.** `accepts_key` is false for `Cli` and `Inherited`, so the sign-in
  prompt refuses rather than collecting something it would throw away.
- **`ready_instances` consults the credential store**, not the `AuthRef` alone, so an instance that
  only looked unusable because the environment was empty stops looking unusable the moment you sign
  in — without a restart.
