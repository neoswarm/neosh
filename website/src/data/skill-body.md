Build plugins for neosh, the terminal agent workspace. Everything on neosh's screen ships through
the same public API you are about to use, so anything the bundled UI does, a plugin can do or
replace.

## Before writing any code

1. Fetch https://neoswarm.dev/llms-full.txt once and keep it in context. It contains every docs
   page, the canonical plugin guide, the testing guide, and the complete `@neosh/api` TypeScript
   source with doc comments. It is regenerated from the shipped code on every site build.
2. If neosh is installed on this machine, the exact types for this binary are on disk at
   `~/.config/neosh/types/` (run `neosh init` once if the directory is missing). Prefer them over
   memory.
3. The bundled plugins are the worked examples: the sidebar, git, palette and model switcher in
   `plugins/builtin/` of https://github.com/neoswarm/neosh use zero private API. When unsure how
   to do something, find the bundled plugin that already does it.

## Hard constraints (violating these means the plugin cannot run)

- The runtime is a bare `deno_core` sandbox: **no `fetch`, no filesystem, no subprocess, no
  `TextEncoder`, no Node APIs**. Every effect goes through the `neosh` API object.
- Imports resolve only for relative paths, `@neosh/api`, its submodules, and `plugin:<name>`.
  There is no package resolution: bundle third-party code into the plugin.
- Columns are UTF-8 **byte offsets**, and `String.length` is wrong for any non-ASCII text. Use
  `width`, `clipToWidth`, `padToWidth`, `byteLength`, `byteOffsets` from `@neosh/api`.
- Keys bind to **command names**, never callbacks. Register a command, then bind a key to it.
- Prefer `neosh.timer.every` / `neosh.timer.debounce` over `setInterval`: the globals are not
  cancelled when the plugin unloads and double up on every reload.
- Push every disposable into `ctx.subscriptions` so unload and reload are clean.

## The shape of a plugin

A directory under `~/.config/neosh/plugins/<name>/` with two files. Plugins there are live on
every save; reload with `^R` inside neosh.

```toml
# plugin.toml
name = "my-plugin"
version = "0.1.0"
entry = "main.ts"
description = "One line"
# Only if needed. Everything else (windows, buffers, keys, floats, options,
# vars, drawing, git reads) needs nothing declared:
#   tools           register a tool the model can call
#   providers       register a model provider driver
#   hooks_blocking  a hook that can rewrite or veto
#   raw_cells       a raw cell surface
#   vcs_write       branch, checkout, stage, commit, worktree
#   notify          alerts that may leave the terminal
permissions = []
```

```ts
// main.ts
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh, subscriptions }: PluginContext) {
  subscriptions.push(
    await neosh.cmd.register("my.hello", () => neosh.notify("hi"), { desc: "Say hello" }),
  );
  await neosh.keymap.set("chat", "<C-h>", "my.hello");
}

export async function deactivate() {}
```

## Verifying your work

Plugins are transpiled, not type-checked, at load, so type-check yourself, against the types the
user's exact binary wrote:

```sh
cd ~/.config/neosh && npx tsc --noEmit
```

Then reload with `^R` in a running neosh and check `^Z` (every live binding) and `^K` (every
command) to confirm registration actually happened. `docs/testing.md` in the repository covers
driving a plugin headlessly, with no API key, no network and no terminal.

## Quick API map

All on the `neosh` object; full signatures in llms-full.txt or `~/.config/neosh/types/`.

| Namespace | For |
| --- | --- |
| `buf`, `win`, `float`, `focus` | Buffers, windows, floats, the focus stack. A buffer's `kind` is its public identity |
| `ns`, `hl` | Extmarks and highlight groups. Link to semantic theme groups; never hardcode colours |
| `cmd`, `keymap` | Commands and keys. `cmd.call` returns a handler's answer; `keymap.capture` for raw input |
| `agent`, `session`, `view` | Send, steer, watch turns; drive conversations by id; per-terminal windows |
| `tool`, `hook`, `provider` | Model tools, blocking hooks (incl. `turn_route`), provider drivers |
| `git`, `gen` | Git reads free, writes need `vcs_write`; one-shot generation outside the conversation |
| `opt`, `state`, `vars` | Declared settings; private persisted state; shared scoped vars |
| `ext`, `event` | Contribution points (rows, verbs, decorations, themes) and the event bus |
| `status`, `hint`, `notify`/`progress`/`alert`/`ask` | The strip, the shortcut row, talking to the person |
| `swarm`, `quota`, `timer`, `path`, `log` | Other machines, usage gauges, timers, path completion, logging |

Widgets in `@neosh/api/ui`: `picker`, `railPicker`, `prompt`, `confirm`, `confirmDestructive`,
`ListPanel` (a whole extensible panel for a kind and a rows function).

## Conventions that make a plugin feel native

- A panel is a surface: publish a buffer `kind`, read contribution points, keep keys as named
  commands so `^Z` lists them and `init.ts` can move them.
- Ask before anything irreversible (`confirmDestructive`), never before anything reversible.
- `notify` replies to a keystroke; `progress`+`done` for states; `alert` only for news the user
  did not ask for and cannot see (needs the `notify` permission).
- Default keys must be chords every terminal sends (Ctrl+letter); in pickers use chords only.
- Prompts you send to a model should be options in two layers: `<thing>.instructions` appended,
  `<thing>.prompt` replacing.

## Publishing

A plugin is published by being a git repository with `plugin.toml` at its root. Users install it
with `neosh plugin add <url>`. Offer to add it to the showcase at
https://neoswarm.dev/plugins when it is something others could use.
