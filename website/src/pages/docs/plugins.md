---
layout: ../../layouts/Docs.astro
title: Writing a plugin
description: A plugin is a directory with a manifest and a TypeScript module. Drop it in, it runs, and it gets the same API every built-in feature is written on.
---

> In a hurry? [Docs for agents](/docs/agents) sets up your coding agent to write plugins for you, with one command.

## Your first plugin

Create a folder under `~/.config/neosh/plugins/`:

```
my-plugin/
├── plugin.toml
└── main.ts
```

```toml
name = "my-plugin"
version = "0.1.0"
entry = "main.ts"
description = "Says hello"
```

```ts
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh, subscriptions }: PluginContext) {
  const d = await neosh.cmd.register("my.hello", () => neosh.notify("hi"), {
    desc: "Say hello",
  });
  subscriptions.push(d);            // disposed when the plugin unloads
  await neosh.keymap.set("chat", "<C-h>", "my.hello");
}

export async function deactivate() {}   // optional: runs on unload and shutdown
```

Reload with `^R` and press `^H`. Plugins in your config directory are discovered as they are, so every save is live.

Plugins are transpiled, not type-checked, at load. `neosh init` writes the API types next to your config, emitted by the binary you are running, so `npx tsc --noEmit` checks against the exact version you have; run it, since it is the only thing that catches a type error before it becomes a runtime one.

## The manifest, in full

```toml
name = "acme-tasks"
version = "0.1.0"
entry = "main.ts"
description = "A task panel"

# Declared up front, and enforced. Everything not listed here (windows, buffers,
# keys, floats, options, vars, drawing, reading) needs nothing declared: it is no
# more privileged than what the person sitting there can already do.
permissions = ["vcs_write"]

# Plugins this one builds on. Each loads and activates first, and
# `import … from "plugin:<name>"` resolves to it. A name nothing provides
# fails at startup with the name in the message.
requires = ["sidebar"]
# Soft ordering: load after these if present, without needing them.
after = ["usage"]

# What this plugin offers others: listed by `ext.points()`, and how a
# contribution to a point nobody reads gets reported instead of ignored.
[provides]
points = ["acme.tasks.section", "acme.tasks.action", "acme.tasks.decoration"]
kinds = ["acme.tasks"]
vars = ["acme.task.due"]

# Omit for a plugin that loads at startup. Present, it is held until one of
# these fires; the command is registered on its behalf and the press replayed.
[activation]
on_command = ["acme.tasks.toggle"]
on_event = ["neosh.win.enter"]
on_kind = ["neosh.sidebar"]
```

### Permissions

| Permission | Grants |
| --- | --- |
| `tools` | Register a tool the model can call |
| `providers` | Register a model provider driver |
| `hooks_blocking` | Take a hook that can rewrite or veto. An observer needs nothing |
| `raw_cells` | Claim a raw cell surface |
| `vcs_write` | Branch, checkout, stage, commit, worktree |
| `notify` | Raise an alert outside the terminal. Drawing in the corner stays free |

These are plugin permissions, not the agent's permission mode. Those govern what the model may do; the model reaches git and the rest through registered tools, gated like every tool.

## What the runtime gives you

A bare `deno_core` sandbox: the ECMAScript built-ins, `console`, `queueMicrotask`, and timers. No `fetch`, no `TextEncoder`, no filesystem, no subprocess. Anything with an effect goes through the neosh API, which is what makes hooks and permissions mean something.

Imports resolve for relative paths, `@neosh/api`, its submodules, and `plugin:<name>`. There is no package resolution; bundle third-party code into your plugin.

Prefer `neosh.timer` over the timer globals: the globals cannot know who called them, so they are not cancelled when your plugin unloads, and an interval that outlives its plugin doubles up on every reload.

```ts
const stop = neosh.timer.every(1000, () => { /* … */ });
subscriptions.push(stop);
const redraw = neosh.timer.debounce(150, () => render());
```

## Who wins a name

One rule for keys, commands and highlight groups: a bundled plugin offers defaults, a plugin you installed is a choice, and your own `init.ts` is the last word. A lower tier never takes what a higher one holds, and within a tier the later registration wins. A command name a higher tier holds is not refused to the lower one: the registration waits in the wings and comes back when the higher tier lets go.

## Installing and publishing

A plugin is published by being a git repository with a `plugin.toml` at its root. There is no registry; a URL is already a globally unique name, and it works for a fork, a private repository and the branch you are writing.

```sh
neosh plugin add https://github.com/someone/neosh-thing
neosh plugin list
neosh plugin update      # fast-forward pull, all of them
neosh plugin remove thing
```

`add` clones, validates the manifest with the same code startup uses, and only then moves it into place, so a broken plugin fails now with a sentence rather than at your next startup as a log line.

Two places, and they mean different things: `~/.config/neosh/plugins/` is yours, for plugins you are writing, live on every save and never touched by `plugin remove`. Installed plugins live under the data directory as checkouts neosh manages. `plugin_dirs` in `config.toml` adds any other directory.

## The worked examples

The sidebar, the model switcher, git, the palette and approvals live in `plugins/builtin/` in the repository. They are ordinary plugins that happen to be embedded in the binary: no private API, no exemptions, and CI type-checks them against the published `@neosh/api`. Read them; they are the reference for everything the API can do.

Built one? Put it in the [showcase](/plugins). One file, one pull request.

## Next

- [The plugin API](/docs/plugin-api) walks every namespace.
- [Panels and extension points](/docs/panels) is how plugins compose.
- [Docs for agents](/docs/agents) if an agent is writing the plugin with you.
