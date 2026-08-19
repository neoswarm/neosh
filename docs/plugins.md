# Writing a plugin

A plugin is a directory with a `plugin.toml` and an entry module. Drop it in
`~/.config/neosh/plugins/` and it loads.

```
my-plugin/
├── plugin.toml
└── main.ts
```

```toml
name = "my-plugin"
version = "0.1.0"
entry = "main.ts"
description = "…"

# Declared up front. Reading git state needs nothing; changing it needs this.
permissions = ["vcs_write"]
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

export function deactivate() {}     // optional
```

`neosh init` writes the API types next to your config, emitted by the binary you are running, so
`npx tsc --noEmit` checks against the exact version you have. **Plugins are transpiled, not
type-checked, at load** — run `tsc` yourself; it is the only thing that catches a type error before
it becomes a runtime one.

---

## The bundled plugins are the worked examples

The sidebar, the model switcher and the git actions live in `plugins/builtin/`. They are ordinary
plugins that happen to be embedded in the binary — no private API, no exemptions, and CI type-checks
them against the published `@neosh/api`. Read them; they are the reference for everything below.

---

## `@neosh/api/ui`

Widgets, built entirely on the public API. Nothing here is privileged — vendor your own copy and it
behaves identically.

### `picker`

```ts
import { picker } from "@neosh/api/ui";

const branch = await picker(neosh, branches.map((b) => ({
  label: b.name,
  detail: b.subject ?? "",
  keywords: b.upstream ?? "",     // matched by the filter, not shown
  value: b.name,
})), { title: "Switch branch", width: 76 });

if (branch !== null) await neosh.git.checkout(branch);
```

Resolves to the chosen value, or `null` if dismissed. Typing filters, `↑`/`↓` and `<C-p>`/`<C-n>`
move, `<C-u>` clears the filter, `<CR>` accepts, `<Esc>` dismisses. Everything else falls through to
your bindings, so `<C-q>` still quits while a picker is open.

`onHighlight` fires as the cursor moves — use it for a live preview.

A picker can be a *finder* rather than a filter — pass `source` and the rows are replaced from it on
every keystroke instead of being fuzzy-filtered locally. That is what you want when the candidates
cannot all be fetched up front:

```ts
const dir = await picker<string>(neosh, [], {
  title: "Add project",
  source: async (query) => (await neosh.path.complete(query)).map((p) => ({ label: p, value: p })),
  freeform: (query) => query.trim() || null,   // accept what was typed, not just what was offered
});
```

Calls are generation-checked, so a slow source answering late cannot replace a newer list with an
older one; and `accept` waits for the fetch in flight, so typing faster than the source answers
takes the row for what you actually typed.

`pathPicker` is that, pre-wired for directories.

Widget keys come from `ui.keys.*` and are claimed window-scoped while the widget is open, so they
outrank global bindings and are released with the window. You get that for free by using these
widgets; there is nothing to declare.

### `prompt` and `confirm`

```ts
const name = await prompt(neosh, "Branch name", { initial: suggested });
const ok = await confirm(neosh, "Stage everything?");

// For anything that cannot be undone. Reads `ui.confirm_destructive`, and starts the cursor on the
// answer that changes nothing — so use this rather than `confirm` and the setting means one thing
// everywhere without your plugin knowing it exists.
if (!(await confirmDestructive(neosh, `Delete ${name}?`, { yes: "Delete", no: "Keep" }))) return;
```

The bar is *irreversible*, not merely significant. Closing a panel or switching a model asks
nothing, because you can put those back.

### Helpers

`fuzzy`, `stateLetter`, `statusPrefix`, `shortenPath`, and `defineHighlights` (which links
`PickerSelected` and `PickerMatch` to groups a theme already defines).

---

## Columns are UTF-8 byte offsets

Every column in this API is a byte offset, matching Neovim and for the same reason: it keeps
display-width math in one place, the frontend.

JavaScript's `.length` is UTF-16 code units. It agrees with bytes only for ASCII, so using it to
place a highlight on a line containing an emoji or any CJK puts the mark in the wrong column — or
inside a character.

```ts
import { byteLength, byteOffsets } from "@neosh/api";

await neosh.ns.mark(ns, buf, row, 0, { hlGroup: "Title", endCol: byteLength(line) });
```

`byteOffsets(s)[i]` is the column of the `i`th code point, with a final entry holding the total.
Match with `Array.from(s)` to get code-point indices, then convert.

---

## Raw keys

Keys bind to command *names*, never callbacks — that is what makes every binding listable and
remappable. For a widget that needs raw input, claim what nothing else wanted:

```ts
const win = await neosh.float.open(buf, { focusable: true, closeOnBlur: true });
await neosh.focus.push(win);
const release = await neosh.keymap.capture(win, "my.key");
```

While `win` is focused, keys no binding claimed are sent to `my.key` with a `KeyContext`. Bindings
still win, so you cannot accidentally take away the key that quits. The capture is released when the
window closes, when a `close_on_blur` float is dismissed, or when your plugin unloads.

`lhs` takes Neovim notation, including multi-key sequences and `<leader>`, which is substituted from
`mapleader` at the moment you call `set`. A sequence the user starts and does not finish is replayed
as ordinary input after `timeoutlen`, so binding `gd` does not make `g` untypeable.

---

## Git

```ts
const status = await neosh.git.status();   // rejects with not_found outside a repository
await neosh.git.branches({ includeRemote: true });
await neosh.git.diff({ kind: "staged" }, { stat: true });
await neosh.git.commit("fix: the thing");  // needs `vcs_write`
```

Reads need nothing declared. Writes need `permissions = ["vcs_write"]` in your manifest — a *plugin*
permission, not the agent's permission mode. Those govern what the model may do; the model reaches
git through a registered tool, gated like every tool.

## Generation

```ts
const { branch } = await neosh.gen.json<{ branch: string }>(
  'Return JSON with one key: branch.\n\nUser message:\n' + text,
);
```

Nothing here enters session history, so asking for a commit message does not change what the agent
believes it was asked to do. `gen.json` tolerates what models actually return — code fences, a
"Sure!" preamble.

Make your prompts options rather than constants, in two layers: `<thing>.instructions` appended to
your default, `<thing>.prompt` replacing it. That is what the bundled `git` plugin does, and it is
why "always prefix branches with the ticket number" is one line of `config.toml` rather than a fork.

---

## Moving and editing text

Your plugin's text field should behave like the composer, and it does — they ask the same question:

```ts
await neosh.edit.move(win, "word_left", { select: true });
await neosh.edit.apply(win, { kind: "insert", text: "hello" });
await neosh.edit.apply(win, { kind: "delete_word_back" });
const chosen = await neosh.edit.selection(win);
await neosh.edit.copy(chosen);
```

Motions are verbs the core resolves rather than positions you compute, because grapheme and word
boundaries are genuinely hard and nobody should have to get them right twice. `move` with
`select: true` extends a selection from wherever it was anchored — anchoring first if nothing was —
and without it the selection is dropped. That is shift-and-arrow, in one call.

Selection is drawn as an extmark in a namespace the core reserves, linked to `Visual`, so your theme
already styles it and no rendering code had to learn a new concept.

`copy` goes out as OSC 52 through the frontend, which is the only thing holding a terminal. That is
also why it works over SSH, where a clipboard library would be talking to the wrong machine.

---

## What your plugin remembers

The runtime has no filesystem, so anything that has to survive a restart goes through the host:

```ts
await neosh.state.set("favorites", ["/home/me/project"]);
const pinned = (await neosh.state.get<string[]>("favorites")) ?? [];
await neosh.state.remove("favorites");
```

Keyed by your plugin id — which the host knows and you cannot forge — so no other plugin can read or
clobber it. `get` returns `null` when nothing was stored. Written atomically, one JSON object per
plugin under `$STATE/plugin-state/`.

Use it for arrangement, not configuration. Which projects the user pinned and what order they
dragged them into belongs here; a *setting* belongs in `neosh.opt`, because an option is something
the user wrote in `config.toml` and a plugin that rewrites that file because someone pressed a key
is one they stop trusting with it. And nothing secret belongs here: it is plain JSON on disk.

---

## What the runtime gives you

A bare `deno_core`: the ECMAScript built-ins, `console`, `queueMicrotask`, `Deno.core`, and timers.
No `fetch`, no `TextEncoder`, no filesystem, no subprocess. Anything with an effect goes through the
neosh API — which is what makes hooks and permissions mean something.

Imports resolve for relative paths, `@neosh/api`, and its submodules. There is no package
resolution; bundle third-party code into your plugin.

See [testing.md](testing.md) for driving a plugin with no API key, no network and no terminal.
