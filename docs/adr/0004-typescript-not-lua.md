# 0004 — TypeScript, not Lua, for plugins

**Status:** Accepted.

## Context

The brief specified TypeScript. That was later reopened — Neovim uses Lua, the project's stated
model is "Neovim's extensibility", and the question was whether the language choice should follow.

## Decision

TypeScript.

## Why

**The hot path here is asynchronous.** Neovim's plugin hot path is synchronous buffer callbacks,
which is precisely Lua's strength. Ours is streaming SSE, JSON-RPC, subprocess I/O and MCP
handshakes. In Lua we would hand-build a coroutine scheduler and a promise-alike before the first
plugin ran; in JavaScript that is the language.

**The brief requires a typed API surface** ("drift here kills plugin ecosystems"). TypeScript gives
that natively and mechanically — `tsc --noEmit` is a build step. LuaCATS annotations are a linting
convention that nothing enforces at publish time.

**The tool ecosystem is JSON-shaped.** Tool definitions are JSON Schema; MCP is JSON-RPC. A plugin
language whose native data model is JSON removes a translation layer from the part of the system
plugins touch most.

**Neovim's flexibility is not Lua.** It is the API surface and the event model: everything public,
hooks on everything, no private escape hatch. That is what we copied — see the shape of `ApiCall`,
which the host's own chat UI is written against. The language was never the load-bearing part.

## What this costs, and what we do about it

Lua's real advantages are embed size and cold start. A `deno_core` isolate is heavier on both. The
mitigations are a startup snapshot (deferred; the runtime is not yet slow enough to justify it) and
keeping the op surface to two functions so the embedding stays cheap to reason about.

## Consequence

Because the frontend is also just a protocol consumer (ADR 0006), a TypeScript *frontend* is
possible later with no core change. That was a factor: choosing Lua would have made the plugin
language and any future non-terminal frontend two different ecosystems.
