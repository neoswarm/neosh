# 0015 — Rust-owned timers, and reload as teardown plus replay

**Status:** Accepted. Seven defects found by adversarial review after implementation; all fixed,
each with a regression test. They are recorded inline below rather than in a changelog, because in
every case the fix is only comprehensible next to the reasoning it corrects.

Two changes that look unrelated and are not: both exist because a configuration you cannot iterate
on is a configuration nobody writes.

## Part 1 — Timers

### Context

The plugin runtime is a bare `deno_core`: ECMAScript built-ins, `console`, `queueMicrotask`,
`Deno.core`, and nothing else. That sandbox is a feature — no `fetch`, no filesystem, so everything
with an effect goes through the neosh API where hooks and permissions can see it.

But it also means no `setTimeout`. Debouncing has no spelling, and "re-render 150 ms after the tokens
stop" is the single most common thing a UI plugin needs. Worse, `setTimeout` being *absent* is not a
thing anyone thinks to check for; it fails as `undefined is not a function` deep inside a plugin.

### Decision

**Rust owns the deadlines; JavaScript owns the callbacks.**

A `BinaryHeap<Reverse<Scheduled>>` keyed by deadline, plus a `live: HashMap<u64, Option<Duration>>`
mapping handle to repeat interval. Two sync ops arm and cancel. Timers surface through the *existing*
`op_neosh_next`, as a `{type:"timer", ids:[…]}` wake alongside host messages, and `bootstrap.js`
dispatches them to stored callbacks.

The obvious alternative — one suspended async op per timer — loses in two specific ways:

- **Shutdown.** A pending op is work in the event loop. `setInterval(f, 20)` would keep handing it
  work forever and the runtime would never drain. Here, `clear_all()` on close is one line, and the
  process exits immediately with an hour-long `setTimeout` outstanding (there is a test asserting
  the elapsed time).
- **Cancellation.** `clearTimeout` would mean reaching into a future the host cannot name. Here it
  is a `HashMap::remove`.

Cancellation removes from `live`, not from the heap. Heaps have no cheap removal; a cancelled entry
costs one skipped pop when it surfaces.

### The drive loop, and why it waits twice

This is the part that would be a hang if it were wrong.

The runtime's loop selects between the host inbox and `js.run_event_loop`. When JS goes quiet it has
to block on the inbox — otherwise it spins — but blocking *unconditionally* means a `setTimeout` in
an otherwise idle session does not fire until the user happens to press a key.

So when JS goes idle the loop waits on the inbox **or** the next deadline, whichever comes first.
And `op_neosh_next` parks on the same deadline independently.

That redundancy is deliberate. Whether `run_event_loop` returns while an op is pending is
version-dependent, and the comments already in that file disagree with each other about it because
the behaviour changed under us once. Either mechanism alone would work under one reading and hang
under the other. Both together work under both. A hung timer is not a bug anyone reports as a hung
timer — they report that the editor feels broken sometimes.

### The floor on repeating timers

`setInterval(f, 0)` reschedules at `now + 0`, which is immediately due again — so the drain loop
never terminates and the runtime thread wedges, taking the process with it. One line of ordinary
plugin JavaScript.

Repeating timers are therefore clamped to a 1 ms floor at registration. Browsers clamp for the same
reason (to 4 ms, after nesting). The clamp is what makes `at > now` true by construction, which is
what actually terminates the loop — the guarantee is structural rather than a bound on iterations.
One-shots are not clamped: `setTimeout(f, 0)` should be as soon as possible.

This was found by review, after the feature was written and tested. The tests it already had all
used positive delays.

### Two APIs, on purpose

`globalThis.setTimeout` exists because it is what every JavaScript author reaches for and what
ported code expects. It is not plugin-scoped: the runtime has no way to know who called it.

`neosh.timer.after/every/debounce` returns a `Disposable`, tracks handles per plugin, and cancels
them in `__teardown` — *before* the other disposers run, so a timer cannot fire into a context that
is halfway torn down. This is the one to use, and the reason it exists is Part 2: without it, every
reload would leave the previous generation's intervals running.

`debounce` is included rather than left as an exercise because coalescing a burst is the actual
use case, and everyone writes the same six lines slightly wrong.

Intervals reschedule from the moment they are *dispatched*, not from the missed deadline, so a slow
callback cannot build a backlog that all lands at once when the plugin comes up for air.

## Part 2 — Reload

### Context

"Restart neosh" is a bad answer for iterating on a config file. Neovim users expect
`:source $MYVIMRC`.

### Decision

`config.reload` tears down everything configuration produced and builds it again from the files on
disk. Whole-world, not incremental.

Diffing two configurations to apply the delta is a class of bug that only shows up in states nobody
tested. "Re-read my configuration" is a semantic people can predict.

The sequence:

1. **Read the files first.** A config that no longer parses leaves the running one untouched. Tearing
   down a working configuration and having nothing to put back is the one outcome a reload must
   never produce.
2. Unload every loaded plugin — `Unload` to the runtime, `remove_plugin` on the editor and the
   agent, `forget` on the bridge.
3. **Reset every modified option owned by `neosh`.** This is what makes deleting a line from
   `init.ts` actually remove its effect. A reload that only ever *adds* is precisely why people give
   up and restart — and it is a real weakness of `:source init.lua`.
4. Bump the generation, then run the same `apply_config` startup does. Sharing that path is the
   point: if reload took a different route, the two would drift and "works on a fresh start" would
   stop meaning anything.

`apply_config` applies the **whole** configuration: options and config scripts, and also provider
instances, the permission layer, and the host's own commands and default keybindings. The first
version applied only the first two and reported success, which is worse than not reloading — a
tightened `[permissions]` block that appears to take effect and does not is a security bug wearing
a confirmation message.

Provider instances are rebuilt from a base snapshot rather than added to. That is the only way an
instance you *delete* from `config.toml` disappears, and the only way an override reverts to the
built-in when you remove it.

The built-in commands are re-registered on every application, not just the first. A config that
rebinds `<C-r>` replaces the default binding; unloading that config removes the binding; and
without re-registering, the key that reloads your config would be the key that stopped working.

### The generation counter

deno_core caches modules by URL. `import("file:///…/init.ts")` a second time returns the cached
module and re-runs *nothing*. A reload built naively reports success and changes nothing, which is
worse than having no reload.

So entry modules load as `file:///…/init.ts?v=N`. Two details make that actually work:

- The loader **strips the query** before touching the filesystem. It is a cache key, not a path.
- The loader **propagates the query to relative imports**. Without this a config split across files
  would reload its entry point and keep running the previous generation's helpers — a reload that
  half works, which is the worst of the three options. There is a test that edits only a helper.

`@neosh/api` is deliberately *not* versioned this way: it resolves to a fixed `neosh:api` URL and
stays shared across generations. Its module-level plugin registry is keyed by plugin id and cleaned
by `__teardown`, so sharing it is correct — and reloading it would orphan the state of plugins that
had not been torn down.

### Built-in commands are ordinary commands

`config.reload` and `quit` are registered through `ApiCall::CmdRegister` under the plugin id
`neosh`, with default `<C-r>` / `<C-q>` bindings set through `ApiCall::KeymapSet`. They appear in
`cmd.list()`, can be invoked by a plugin, and are replaced by binding the same key in your config.

The host answers them itself instead of forwarding to a JS plugin that does not exist — that is the
only special case, and it is in dispatch, not in the registry.

This is the thesis applied to the host's own UI: if `quit` had lived somewhere private,
`cmd.list()` would be a partial view and rebinding it would be impossible. (Before this, incidentally,
there was no way out of the terminal UI but SIGINT.)

### Teardown covers failed activations too

A plugin whose `activate` throws never entered the loaded list, so reload skipped it — while its
timers, commands, tools and hooks, registered before the throw, kept running. Each reload stacked
another copy. A reviewer reproduced it: an interval armed by a config that then threw went from 20
firings per two seconds to 39, 60 and 80 across three reloads, with every previous generation still
firing.

The failure path now runs the same teardown as unload. There is a regression test.

## What reload does not do

**Buffers and windows a plugin created survive.** A float opened during `activate` will open a
second time on reload. Closing them would mean the core tracking provenance for every object, and
Neovim's `:source` does not do it either — but it is a real annoyance, and the fix is for plugins to
use `ctx.subscriptions`.

**An in-flight turn is not cancelled.** The agent keeps streaming into a chat buffer that survives
the reload.

**deno_core's module map keeps every generation.** The cache bust is what makes reload work, and
nothing evicts the previous URL. For config-sized modules this is kilobytes per reload. The
loader's own source-map table *is* pruned to one entry per file, because that one was ours to fix.

Both are recorded here rather than in an issue because they are the predictable complaints, and
the answer to each is "yes, known, here is the trade".
