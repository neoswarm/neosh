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

A picker can also carry *verbs*. `onKey` is checked before the widget's own handling, and `ownKeys`
claims keys a binding elsewhere would otherwise take — the archive browser puts "put it back" and
"delete it" on rows this way, rather than opening a second modal to ask which you meant:

```ts
const chosen = await picker(neosh, items, {
  hints: "↵ restore   ^U put back   ^X delete   esc close",
  ownKeys: ["<C-u>", "<C-x>"],           // both are bound elsewhere; claim them or they never arrive
  async onKey(key, ctx) {
    if (key.key.code.kind !== "char" || !key.key.mods.ctrl) return;
    if (key.key.code.c === "u") {
      await putBack(ctx.item);
      items.splice(0, items.length, ...(await rows()));   // mutate the array you passed in
      return "reload";                                     // …and the list catches up in place
    }
  },
});
```

Return `"close"`, `"reload"`, `"handled"`, or nothing at all to let the widget have the key. Chords
only in a picker that filters: every bare letter you take is a letter the filter can never contain.

Widget keys come from `ui.keys.*` and are claimed window-scoped while the widget is open, so they
outrank global bindings and are released with the window. You get that for free by using these
widgets; there is nothing to declare.

### `prompt` and `confirm`

```ts
const name = await prompt(neosh, "Branch name", { initial: suggested });
const ok = await confirm(neosh, "Stage everything?");

// For anything that cannot be undone. Reads `ui.confirm_destructive`, starts the cursor on the
// answer that changes nothing, and draws the other one in the theme's error colour — so use this
// rather than `confirm` and the setting means one thing everywhere without your plugin knowing it
// exists.
if (!(await confirmDestructive(neosh, `Delete ${name}?`, {
  yes: "Delete",
  no: "Keep",
  detail: [`${count} messages, in ${project}.`, "Archiving keeps every word of it."],
}))) return;
```

The bar is *irreversible*, not merely significant. Closing a panel or switching a model asks
nothing, because you can put those back. On the other side of that line there are no exceptions:
ask every time, including for the cheap case, because a dialog that appears only sometimes is one
the user's fingers are already through by the time it matters.

`detail` is what makes it worth stopping for. "Are you sure?" is a speed bump; the number of
messages, the project they are in and what the alternative is can be answered without leaving the
dialog to go and check. `y`/`n` answer it outright, and the question wraps rather than being clipped
to one line.

### `railPicker`

Two panes: a rail of categories, and the rows belonging to whichever one is selected. What the model
switcher is built from.

```ts
const chosen = await railPicker<Provider, Model>(neosh, {
  title: "Model",
  rail: providers.map((p) => ({
    mark: { text: "✳", hl: "Brand.Anthropic" },   // drawn in its own colour
    badge: { text: "✓", hl: "Account.Plan" },     // one column, at the right edge
    group: "PLANS",                                    // headings, in first-seen order
    label: p.name,
    value: p,
  })),
  items: async (p) => rowsFor(p),        // called on open and on every rail move
  hints: "↵ use   ⇥ panes   esc close",
  placeholder: "nothing here yet",
  onKey: (key, ctx) => (key.key.code.c === "s" ? signIn(ctx.rail) : undefined),
});
```

Reach for it when one list would be answering two questions at once. "Which provider" and "which
model" have different cardinalities — a dozen against hundreds — and flattening them puts the thing
you want thirty rows under something you do not use.

A `PaneItem` may name a `section`, which folds its rows behind a count and opens automatically while
a filter is active: "no matches" with the match folded away is the worst possible answer.

Both panes are one buffer with a rule down the middle, not two floats. Two floats cannot be kept
adjacent without each of them knowing the other's width, and neither of them may measure anything.

### Helpers

`fuzzy`, `stateLetter`, `statusPrefix`, `shortenPath`, and `defineHighlights` (which links
`PickerSelected` and `PickerMatch` to groups a theme already defines).

---

## Columns are UTF-8 byte offsets

Every column in this API is a byte offset, matching Neovim and for the same reason: it keeps
display-width math in one place, the frontend.

### Measuring, when you are laying one out

```ts
import { width, clipToWidth, padToWidth } from "@neosh/api";

width("日本")            // 4 columns, 2 code points
padToWidth(name, 20) + "│"  // a rule that lines up
```

Use these for anything with a column in it. JavaScript offers `String.length` (UTF-16 units) and
`Array.from(s).length` (code points), and **both are wrong** for the text a model produces: two CJK
characters are 2 code points and 4 columns, a waving hand is 1 and 2, a combining accent adds a code
point and no column. Padding with either draws a ragged rule the first time a non-ASCII name appears
in your list.

They are synchronous — ops, not host calls — so calling one per row costs nothing, and they use the
same measurement the renderer does, so your layout and the frame agree by construction.

## Text that moves

A highlight group may carry an animation, and the frontend animates whatever does:

```ts
await neosh.hl.define("MyPlugin.Working", { link: "Status.Streaming" });  // sweeps
await neosh.hl.define("MyPlugin.Waiting", { link: "Status.Pending" });    // pulses
```

Set the group once and stop thinking about it: nothing in your plugin drives the movement, there is
no timer to own, and it costs you no API calls at all. Animating text yourself would mean re-setting
an extmark per character per tick — hundreds of calls a second across the runtime boundary to move a
highlight two columns.

Motion is reserved, by convention, for *something is happening and you cannot see it yet*. A screen
where several things move is a screen where none of them means anything. `ui.motion = false` removes
the movement and keeps the colour, and your plugin needs no code for that case.

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

A selection is two positions, and the *shape* is what says whether the character the cursor is on is
in it:

```ts
await neosh.edit.selectShape(win, "inclusive"); // the cursor is on a character, and it is selected
await neosh.edit.selectShape(win, "line");      // whole rows, in whichever direction it runs
await neosh.edit.cursorShape(win, "block");     // and the caret says so before any key is pressed
```

`exclusive` is the default and is what a text field wants: the cursor sits *between* two characters,
so one that swallowed the character to its right would delete a letter nobody highlighted. A normal
mode wants the other answer — `v` then `y` copies a letter rather than reporting an empty selection
— and a `line` selection snaps both ends to whole rows, including when it is extended upwards.
Dropping a selection puts the shape back to `exclusive`.

`cursorShape` is per window. A block caret is painted by the frontend over the character it is on
(reverse video unless a theme defines `Cursor`) *and* asked of the terminal, which is what a screen
reader follows. Give it back when your mode ends: a shell that inherits a block cursor because
something exited mid-mode is a terminal that looks broken.

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

Use it for arrangement, not configuration. Which sections a panel of yours has folded belongs here;
a *setting* belongs in `neosh.opt`, because an option is something the user wrote in `config.toml`
and a plugin that rewrites that file because someone pressed a key is one they stop trusting with
it. And nothing secret belongs here: it is plain JSON on disk.

## What everybody remembers — `neosh.vars`

The same thing, minus the privacy, plus a scope. A var describes the workspace, a conversation or a
project, and anyone may read or write it.

```ts
import { projectScope, sessionScope } from "@neosh/api";

await neosh.vars.set(projectScope(cwd), "sidebar.favorite", true);
const pinned = await neosh.vars.get<boolean>(projectScope(cwd), "sidebar.favorite");
const all = await neosh.vars.all(projectScope(cwd));   // one round trip, the whole project

neosh.vars.onChange((e) => {                            // whoever changed it, including you
  if (e.scope.scope === "project") redraw();
});
```

The line between this and `state` is who has a reason to look. A fold set is yours; "this project is
a favourite" is not, and while it was, a second panel started with no favourites and there was no
way to tell it about them. Pinning a project from your own plugin is now the three lines above.

Namespace your keys — `sidebar.favorite`, `acme.colour` — for the reason options are namespaced.
Nothing stops two plugins choosing `colour`; a prefix is what makes them not want to.

Default to `state` and reach for `vars` when somebody else genuinely needs the value. A workspace
where every plugin writes its scratch into a shared table is one where nobody can rename anything.
And note that a var write is a *file* write: right for a keystroke like `f`, wrong for anything that
happens on every cursor move — publish that as an event instead.

---

## Putting something in somebody else's panel

Three mechanisms, and between them you should not have to fork a bundled plugin to change it. See
[ADR 0040](adr/0040-a-panel-is-a-surface-not-a-program.md).

### Bind a key inside a panel you did not open

A panel declares what it is with a buffer *kind*, and you bind against the kind rather than against
a window whose id is private to whoever opened it and changes every time it is toggled.

```ts
// In your init.ts. `x` archives a conversation by default; this replaces it.
await neosh.keymap.set("chat", "x", "acme.mine", {
  scope: { kind: "buf_kind", name: "neosh.sidebar" },
  desc: "Do it my way",
});
```

Resolution is window → buffer → kind → global, first match winning. Your binding is an ordinary one:
`^Z` lists it under the panel's section, `^K` runs the command, and the panel's own default loses to
it because a default that overwrites a choice is not a default.

Publish a kind for anything of yours that is more than a scratch buffer — it is one argument, and it
is the difference between a panel somebody can extend and one they can only replace:

```ts
const buf = await neosh.buf.create({ name: "[tasks]", scratch: true, kind: "acme.tasks" });
const open = await neosh.win.ofKind("neosh.sidebar");   // find somebody else's, too
```

### Contribute rows and verbs

A contribution point is a name a plugin agrees to read. The sidebar reads two:

```ts
// Rows in the column. Re-contributing under the same id replaces, so this is also how you update.
await neosh.ext.contribute("sidebar.section", "todo", {
  title: "ACME",
  at: "below",                                    // or "above" the project list
  rows: [{ text: "Ship the thing", command: "acme.open", args: ["thing"] }],
});

// A verb on a row. The panel binds the key and invokes your command with the row under the cursor:
// ["session", cwd, id] or ["project", cwd].
await neosh.ext.contribute("sidebar.action", "touch", {
  key: "t",
  label: "touch",
  command: "acme.touch",
  on: "session",                                   // or "project", or "any"
});
```

Your contributions go when your plugin does, so `plugins.disabled` takes your rows with it. A point
is just a string: reading one in a panel of your own is `ext.list(point)` plus a redraw on
`ext.onChange`, and that is the whole protocol.

### Say that something happened

```ts
await neosh.event.emit("acme.indexed", { files: 412 });
neosh.event.on("acme.indexed", (e) => { /* e.data, e.from */ });
```

Broadcast, plugin-defined, and with no reply by construction — an emitter that could be blocked is an
emitter with every listener on its critical path. When you need an answer, register a command or read
a contribution point. `from` is stamped by the host and cannot be forged.

---

## Keys, and the one thing you cannot do

Your plugin can find out whether a provider is authenticated, and can ask for a key to be entered.
It cannot read one.

```ts
for (const c of await neosh.agent.credentials()) {
  // c.source.kind: "env" | "keychain" | "session" | "command" | "inherited" | "not_needed" | "missing"
  // c.accepts_key: false for a CLI login or a local endpoint — nowhere to put one
}

// Settles when the prompt closes. `true` means a key was stored; never the key itself.
const ok = await neosh.agent.setCredential("anthropic", { replace: true });
await neosh.agent.forgetCredential("anthropic");
```

`setCredential` does **not** open a widget of yours, and there is no `mask: true` on `prompt` for
you to reach for. The host reads the keystrokes itself, because a plugin field is a buffer, a buffer
is drawn, and drawing it would put the key in a `UiEvent` — across a process boundary, into whatever
the frontend logs. Masking is a rendering decision; the leak is a transport one.

The consequence to design around: this is the one modal you cannot re-skin. You can decide *when* to
ask and what to do afterwards — the model switcher re-queries the endpoint and reopens its list —
but not what the prompt looks like. [ADR 0022](adr/0022-credentials-never-cross-the-plugin-boundary.md)
records why that trade was made.

---

## Conversations

```ts
await neosh.session.list();                          // what the user works in
await neosh.session.list({ includeArchived: true }); // plus what they put away
await neosh.session.archive(id);                     // reversible, keeps everything
await neosh.session.archive(id, false);              // back, and to the top of the list
await neosh.session.close(id);                       // deletes the file. No undo.
```

`archive` and `close` are different verbs on purpose. If you are writing the thing a user presses
when they are done with a conversation, it is `archive`; `close` is for when they have said they
mean it. Gate `close` behind `confirmDestructive` and say that archiving keeps it.

Nothing archived appears in the sidebar — `session.archived` is the command that opens it, and
`list({ includeArchived: true })` is how you find them yourself. A panel of your own should do the
same: the list someone works in is the list of things they might switch to now.

---

## What the runtime gives you

A bare `deno_core`: the ECMAScript built-ins, `console`, `queueMicrotask`, `Deno.core`, and timers.
No `fetch`, no `TextEncoder`, no filesystem, no subprocess. Anything with an effect goes through the
neosh API — which is what makes hooks and permissions mean something.

Imports resolve for relative paths, `@neosh/api`, and its submodules. There is no package
resolution; bundle third-party code into your plugin.

See [testing.md](testing.md) for driving a plugin with no API key, no network and no terminal.
