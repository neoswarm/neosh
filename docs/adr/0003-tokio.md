# 0003 — tokio as the async runtime

**Status:** Accepted. Pre-decided in the brief.

## Decision

tokio, multi-threaded runtime, with one current-thread runtime pinned to the plugin thread.

## Why

Everything this program does while it is interesting is I/O: SSE streams, subprocess pipes,
terminal input, plugin RPC. tokio is what the surrounding ecosystem (reqwest, deno_core, crossterm's
`EventStream`) already expects, and mixing runtimes is a category of bug not worth inviting.

`select!` with a pinned `Sleep` is also what makes redraw coalescing expressible in a few lines:
arm a deadline on the first mutation, and let everything until it fires land in one frame. An idle
session parks in `select!` and uses no CPU, which a frame loop cannot claim.

## The one place it is not multi-threaded

`JsRuntime` is `!Send`, so the plugin runtime gets a dedicated thread with a current-thread runtime
and communicates over channels. That is not a workaround — it is the same shape an out-of-process
plugin has, which is why one protocol serves both.
