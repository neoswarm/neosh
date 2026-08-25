---
layout: ../../layouts/Docs.astro
title: Configuration
description: Your config is a plugin with the whole API, and a TOML file sits beside it for plain values. This page covers the files, the load order, and how reloading works.
---

## Getting a config

```sh
neosh init
```

That writes a starter config, a `tsconfig.json` and the API types, then tells you the path. Open `init.ts` and start editing.

## The one thing to understand

Your config is a plugin. `init.ts` gets the entire public API, the same one every plugin uses, with nothing held back. There is no config-only API, no list of "supported config options" to memorize, and nothing a plugin can do that your config cannot.

If `init.ts` throws, neosh still starts. The error is reported and startup continues, because you need the editor to fix the editor.

## Where things live

The same roots Neovim uses, so symlinking the config directory into a dotfiles repo works exactly as you would expect:

```
~/.config/neosh/            $XDG_CONFIG_HOME, or $NEOSH_CONFIG_DIR
├── init.ts                 your config, the important file
├── config.toml             plain values, the non-code path
├── tsconfig.json           generated, so your editor knows the API
├── types/                  generated API types for this exact binary
└── plugins/<name>/         drop a plugin here and it loads

~/.local/share/neosh/       installed plugins, managed by neosh
~/.local/state/neosh/       sessions, plugin state, trust.json

<your project>/.neosh/      per-project config
├── config.toml
└── init.ts
```

```sh
neosh paths     # what those resolve to here, and which exist
```

All overridable with `NEOSH_CONFIG_DIR`, `NEOSH_DATA_DIR`, `NEOSH_STATE_DIR`, `NEOSH_CACHE_DIR`, or `--config-dir`. `neosh --clean` reads and writes nothing at all, which is the right mode for reporting a bug: it answers "is this neosh or is it my setup".

## init.ts

```ts
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.opt.set("agent.model", "claude-cli/sonnet");

  await neosh.cmd.register("chat.clear", async () => { /* … */ }, {
    desc: "Clear the conversation",
  });
  await neosh.keymap.set("chat", "<C-l>", "chat.clear");
}
```

Keys bind to command names, never to callbacks. That indirection is what makes every binding listable with `^Z`, remappable, and callable by other plugins. [Keymaps](/docs/keymaps) covers the details.

Your config runs before plugin discovery, which is what lets it decide what else loads:

```ts
await neosh.rtp.add("~/src/my-plugins");   // another plugin directory
```

## config.toml

For setting values without writing code. It expresses a subset of what `init.ts` can; it exists so "use this model" is one line, and so a settings UI has a file it can rewrite.

```toml
[agent]
model = "anthropic/claude-opus-5"

[permissions]
mode = "ask"                 # ask | allow_listed | deny
allow_commands = ["cargo"]

[options]
"chat.show_thinking" = true
"sidebar.width" = 34

[[providers]]
id = "local"
driver = "openai-compat"
display_name = "llama.cpp"
base_url = "http://localhost:8080/v1"
auth = { kind = "env", var = "MY_API_KEY" }

plugin_dirs = ["~/src/neosh-plugins"]
```

Unknown keys are an error, not ignored. A typo that is silently skipped would look exactly like a setting that does not work.

`auth` names where a key is, never a key: `{ kind = "env", var = "…" }`, `{ kind = "command", argv = [...] }`, `{ kind = "cli", program = "codex", login = "codex login" }` for a plan behind a vendor CLI, or `{ kind = "none" }` for a local endpoint. Nothing writes a secret to this file, and nothing reads one from it.

Every key under `[options]` is documented in [Options](/docs/options).

## Per project

`<project>/.neosh/config.toml` is committable and applies to anyone who opens the repository. Because a config file in a repository you cloned is code someone else wrote, it is split by what it can do:

- **Applies immediately**: `model`, since it can only name a provider you already configured, and `[permissions]`, but only to make things stricter. A repository cannot grant itself `curl`.
- **Waits for `neosh trust`**: `.neosh/init.ts`, `plugin_dirs`, `providers`, `[options]`, `system_prompt`. Anything that runs code or widens what the agent may do.

```sh
neosh trust            # prints the files, then approves them
neosh trust --list
neosh trust --revoke
neosh init --project   # scaffolds .neosh/ in the current directory
```

Trust is keyed on the contents of every file under `.neosh/`, not on the path. Editing any of them, including a module `init.ts` imports, revokes it automatically, so a `git pull` cannot quietly change what runs on your machine. You are told, and you re-approve after reading the change.

## Precedence

Later wins:

1. Built-in defaults
2. `~/.config/neosh/config.toml`
3. `<project>/.neosh/config.toml`
4. `~/.config/neosh/init.ts`
5. `<project>/.neosh/init.ts`, if trusted
6. Command-line flags

Flags last, so `--model` always takes effect.

## Reloading

`^R` reloads without a restart:

1. Every layer is re-read from disk.
2. Every plugin is unloaded, and settings reset to their defaults first, so deleting a line actually removes its effect.
3. If the new config does not parse, the running one is left alone and you are told why.

Two things reload does not do yet: close what a plugin opened during `activate` (use `ctx.subscriptions` for anything that should not outlive your plugin), and interrupt an in-flight turn, which keeps streaming.

## Type checking

The types in `types/` were written by the binary you are running and are refreshed at startup if they drift, so they always describe the version you actually have:

```sh
cd ~/.config/neosh && npx tsc --noEmit
```

Plugins are transpiled, not type-checked, at load. Running `tsc` yourself is the only thing that catches a type error before it becomes a runtime one.
