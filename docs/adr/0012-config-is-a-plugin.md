# 0012 — The user's configuration is a plugin

**Status:** Accepted.

## Context

neosh needs what `init.lua` is to Neovim: a file the user writes that shapes the editor before
anything else runs. The question is what language it is in, and whether it is a distinct mechanism
from plugins.

The tempting answer is a second, "simpler" language for config — TOML for everything, or a small
DSL, on the theory that most people only want to set a model and a keybinding.

## Decision

**`~/.config/neosh/init.ts` is a plugin.** It is loaded first, under the reserved id `user`, with
the entire public API. There is no config-only API and no plugin-only API. It is TypeScript, the
same language plugins are written in.

Concretely, the host's own startup sequence is:

1. declare the built-in options, via `ApiCall::OptDeclare` under the plugin id `neosh`
2. apply `config.toml`
3. activate `init.ts`, then a trusted `.neosh/init.ts` (ADR 0013)
4. apply command-line flags
5. discover and activate plugins from the runtime path

Step 5 is after step 3 on purpose: `neosh.rtp.add()` is how a config decides what else loads, which
is only meaningful if config runs first.

## Why not a second config language

Vim's actual lesson is not "Lua is good". It is that **two languages is one too many**. vimscript
for config and C for anything real meant every capability had to be exposed twice, the exposures
drifted, and the gap between "what you can configure" and "what a plugin can do" became the
ecosystem's permanent frustration. Neovim's fix was not choosing Lua; it was making `init.lua`
ordinary Lua with unrestricted `vim.api` access.

This project's premise is stronger than Neovim's — "if we need a private escape hatch, the API is
wrong". A config language that can express less than a plugin *is* that escape hatch, pointed the
other way.

So the test for any proposed config feature is: can `init.ts` already do it through the public API?
If yes, it needs no config feature. If no, the public API is missing something, and that is the bug.

## What this costs, honestly

**A JS runtime boots at startup even to set one value.** Accepted: it boots anyway for plugins, and
the option registry means "just set a value" has `config.toml` (ADR 0013) as a non-code path.

**A config error is a runtime error, not a parse error.** Mitigated three ways: `init.ts` failing is
reported and skipped rather than fatal — locking someone out of their editor over a config typo is
unforgivable, because the editor is how they would fix it; option names are validated against
declarations, so the most common mistake fails loudly; and `neosh init` writes a `tsconfig.json`
plus the API types so mistakes surface in the editor before neosh ever runs.

**TypeScript is a bigger language than Lua.** True, and the reason it wins anyway is in ADR 0004:
the hot path here is asynchronous, and the tool ecosystem is JSON-shaped.

## The types are emitted by the binary

`neosh init` writes `types/` containing the very `@neosh/api` source the host executes, embedded at
build time, and a `tsconfig.json` mapping `@neosh/api` to it. Startup refreshes that directory if it
exists and has drifted.

This is the part that makes TypeScript worth the weight. The config gets completion and type
checking against the exact binary in use — not against a published package that may be a version
behind. An upgrade cannot leave someone type-checking against an API that no longer exists, because
there is no second copy to go stale.

## Consequences

- Everything reachable from a plugin is reachable from a config, permanently, without anyone
  maintaining a list.
- A settings UI is a plugin. A plugin manager is a plugin. Neither needs core support.
- Config reload is not implemented. It needs plugin unload/reload, which Phase 0 does not have.
  Restarting is the answer for now, and it is recorded here so it is a known gap, not a surprise.
