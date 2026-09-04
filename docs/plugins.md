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

# Declared up front, and enforced. Reading git state needs nothing; changing it needs this.
#
#   tools           register a tool the model can call
#   providers       register a model provider driver
#   hooks_blocking  take a hook that can rewrite or veto (an observer needs nothing)
#   raw_cells       claim a raw cell surface
#   vcs_write       branch, checkout, stage, commit, worktree
#
# Everything else — windows, buffers, keys, floats, options, vars — needs nothing declared: it is
# no more privileged than what the person sitting there can already do.
permissions = ["vcs_write"]

# Plugins this one builds on. Each loads and activates first, `import … from "plugin:<name>"`
# resolves to it, and a name nothing provides fails at startup with the name in the message.
requires = ["sidebar"]
# Soft ordering: load after these if they are present, without needing them.
after = ["usage"]

# What this plugin offers others — listed by `ext.points()` and the plugins panel, and how a
# contribution to a point nobody reads gets reported instead of silently ignored.
[provides]
points = ["acme.tasks.section", "acme.tasks.action", "acme.tasks.decoration"]
kinds = ["acme.tasks"]
vars = ["acme.task.due"]

# Omit for a plugin that loads at startup. Present, it is held until one of these fires: the
# command is registered on its behalf and the press replayed once it is up.
[activation]
on_command = ["acme.tasks.toggle"]
on_event = ["neosh.win.enter"]
on_kind = ["neosh.sidebar"]
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

export async function deactivate() {}   // optional: runs on unload and at shutdown, bounded to a second
```

`neosh init` writes the API types next to your config, emitted by the binary you are running, so
`npx tsc --noEmit` checks against the exact version you have. **Plugins are transpiled, not
type-checked, at load** — run `tsc` yourself; it is the only thing that catches a type error before
it becomes a runtime one.

---

## Shipping one, and installing somebody else's

A plugin is published by being a git repository with a `plugin.toml` at its root. There is no
registry: a URL is already a globally unique name, and it works for a fork, a private repository and
the branch you are writing.

```sh
neosh plugin add https://github.com/someone/neosh-thing   # or git@host:someone/thing
neosh plugin list                                         # everything that loads here
neosh plugin update                                       # ff-only pull, all of them
neosh plugin remove thing                                 # asks first
```

`add` clones, reads the manifest with the same code startup reads it with, and only then moves it
into place — so a plugin built against another protocol version, or missing the file its `entry`
names, fails here with a sentence rather than at your next startup as a line in a log.

The directory is named by `name` in the manifest, not by the URL: a plugin's id is that name, and
two directories claiming one id would make which of them wins depend on how a filesystem happened to
list them.

**Two places, and they mean different things.** `~/.config/neosh/plugins/` is yours — put a plugin
you are *writing* there and it is discovered as it is, so every save is live. `neosh plugin remove`
will not touch it, because `add` did not put it there. Installed plugins live under your data
directory and are checkouts neosh manages. `plugin_dirs` in `config.toml` adds any other directory.

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
const branch = await neosh.gen.field(
  'Return JSON with one key: branch.\n\nUser message:\n' + text,
  'branch',
);
```

Nothing here enters session history, so asking for a commit message does not change what the agent
believes it was asked to do. `gen.json` tolerates what models actually return — code fences, a
"Sure!" preamble.

`gen.field` is that plus the one thing `gen.json` cannot do: when the answer is a single value, a
model asked for `{"branch": …}` sometimes replies `fix/composer-paste` and nothing else. It did the
work and left the envelope off, and `gen.json` rejects it. Name the key and both are the same
answer. For one value only — a commit message is a subject *and* a body, and there is no telling
which half a lone paragraph is — and only for a value you would know was wrong on sight. A branch
name is that. A thread title is any short line, and so is a refusal or a driver's own error
message; where nothing tells an answer from a remark, the envelope is the evidence and `gen.json`
is the call.

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

## Building on another plugin

Neovim's `require("nvim-tree.api")`, in two spellings.

```ts
// Typed and free of round trips, for a plugin you `requires` in the manifest. Module-level state
// is shared: this is the very module the host activated, not a second copy of its source.
import { api as sidebar } from "plugin:sidebar";
const row = sidebar.cursor();            // the Target under the sidebar's cursor, or null
for (const t of sidebar.rows()) { /* … */ }

// Through the host, for a plugin you would rather not depend on, or for the host's own commands.
// Whatever the handler returned, as JSON; `null` for one that returned nothing; rejects with the
// handler's error, or `not found`.
const row = await neosh.cmd.call<Target | null>("sidebar.cursor");
```

A command *returns* now: `cmd.register("acme.count", () => ({ n }))` is a question anybody can ask
with `cmd.call`, and `cmd.exec` is still the key press that does not wait. Registrations stay data
so they can be listed and disabled; *queries* get an answer, because faking one out of an event, a
correlation id and a reply command is a worse RPC written once per plugin.

`init.ts` loads before every plugin, so a static `plugin:` import there would find nothing. Use a
dynamic one once the workspace is up:

```ts
neosh.event.on("neosh.ready", async () => {
  const { api } = await import("plugin:sidebar");
});
```

Type-checking resolves `plugin:<name>` against `plugins/<name>/main.ts` beside your config, the
plugins `neosh plugin add` installed, and — for the bundled ones — `types/builtin/<name>/main.ts`,
written by `neosh init` and refreshed at startup from the binary you are running.

**Who wins a name.** One rule for keys, commands and highlights: a bundled plugin offers *defaults*,
a plugin you installed is a *choice*, your own `init.ts` is the *last word*. A lower tier never
takes a key, a command or a colour a higher one holds — silently, because a default finding its
key taken is the ordinary case — and within a tier the later registration wins, which is load
order. A command name a higher tier holds is not refused to the lower one: the registration is
kept in the wings and comes back when the higher tier lets go, so a panel's `activate` never fails
over a verb the user had already decided to own.

---

## Putting something in somebody else's panel

Four mechanisms, and between them you should not have to fork a bundled plugin to change it.

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

### Contribute rows, verbs and marks

A contribution point is a name a plugin agrees to read. The sidebar reads three:

```ts
// Rows in the column. Re-contributing under the same id replaces, so this is also how you update.
await neosh.ext.contribute("sidebar.section", "todo", {
  title: "ACME",
  before: "add",                                  // a slot — projects, add, archived — or another
                                                  // section's id; `at: "above" | "below"` is coarser
  rows: [{ text: "Ship the thing", command: "acme.open", args: ["thing"] }],
});

// A mark on a row the panel already draws, keyed by what the row is about. The git plugin's stat
// strip is exactly this. The name is clipped to make room for the badge; `hl` colours a
// row the panel left plain; `right` replaces the count or the age on a row that is not busy.
await neosh.ext.contribute("sidebar.decoration", `prs:${cwd}`, {
  target: { project: cwd },                       // or { session: id }
  badge: { text: "2 PRs", hl: "Accent" },
});

// A badge is often several facts rather than one, and a strip drawn in a single colour is a string
// you read rather than a row you glance at. `parts` is the same badge in as many colours as it has
// things to say — what `↓2 ↑5 ~7` on a project row is — and a part with no `hl` is a separator.
await neosh.ext.contribute("sidebar.decoration", `ci:${cwd}`, {
  target: { project: cwd },
  badge: { parts: [{ text: "✓12", hl: "Git.Added" }, { text: " " }, { text: "✗1", hl: "Git.Conflict" }] },
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
`ext.onChange`, and that is the whole protocol. Declare the points you read under `[provides]` so
`ext.points()` can say who reads what, and a contribution to a point nobody reads is reported at
startup with the nearest real one — `did you mean "sidebar.section"?`.

### Follow its cursor

The sidebar says where it is: `sidebar.cursor` on the event bus on every move, with the row under
the cursor as `data`; `sidebar.cursor` and `sidebar.rows` as commands for `cmd.call`; `cursor()`
and `rows()` on `plugin:sidebar`.

### A panel of your own, on `ListPanel`

Everything above, for the price of a kind and a `rows` function:

```ts
import { ListPanel } from "@neosh/api/ui";

const panel = await ListPanel.create<Task>(neosh, {
  kind: "acme.tasks",
  dock: "right",
  size: () => 30,
  rows: () => tasks.map((t) => ({ text: ` ▸ ${t.title}`, value: t })),
  key: (t) => ({ task: t.id }),          // what decorations target, and what anchors the cursor
  kindOf: () => "task",                  // what a contributed action's `on` may name
  onOpen: (t) => openTask(t),
});
subscriptions.push({ dispose: () => panel.dispose() });
```

That is a buffer of kind `acme.tasks` in a dock; `acme.tasks.down`, `.up`, `.first`, `.last`,
`.open`, `.leave`, `.toggle`, `.focus`, `.refresh`, `.cursor` and `.rows` as commands, bound at
`buf_kind` scope so `^Z` lists them and `init.ts` moves them; `acme.tasks.section`, `.action` and
`.decoration` read exactly as the sidebar reads its own; the cursor published as the buffer var
`cursor` and the event `acme.tasks.cursor`; and a redraw when any of it changes. The shared pickers
publish kinds too — `neosh.picker`, `neosh.confirm`, `neosh.prompt` — so a key bound against one
binds inside every picker at once.

### Say that something happened

```ts
await neosh.event.emit("acme.indexed", { files: 412 });
neosh.event.on("acme.indexed", (e) => { /* e.data, e.from */ });
```

Broadcast, plugin-defined, and with no reply by construction — an emitter that could be blocked is an
emitter with every listener on its critical path. When you need an answer, register a command or read
a contribution point. `from` is stamped by the host and cannot be forged.

The workspace's own events travel the same way, `from: "neosh"` — Neovim's autocmds:

| event | data |
|---|---|
| `neosh.ready` | once every plugin has loaded, and again after a reload |
| `neosh.win.enter`, `neosh.win.leave`, `neosh.win.open` | `{ win, buf, kind }` |
| `neosh.win.close` | `{ win }` |
| `neosh.cursor` | `{ win, row, col }` |
| `neosh.mode` | `{ mode }` |
| `neosh.viewport` | `{ win, width, height }` — the one size only the frontend knows |

`event.on(name, cb, { kind: "neosh.sidebar" })` keeps only the events about one kind. The host's
`focus.onChange`, `onViewAttached` and `onShutdown` are the same facts as listeners.

### Colour a window, or ship a theme

```ts
// Your own groups: `default` is `:hi default` — define only if nobody has, so init.ts wins
// whichever of you loaded first. A group is yours until you reset it or your plugin unloads,
// when it goes back to the theme's or away.
await neosh.hl.define("Acme.Due", { link: "Status.Unread" }, { default: true });
const { resolved } = await neosh.hl.get("Normal");     // read a colour rather than guess at it
neosh.hl.onChange(({ names }) => { /* a theme switch lists them all */ });

// One window, or every window of a kind — Neovim's `winhighlight`. Nothing else on screen changes.
await neosh.hl.define("Acme.Panel", { bg: { kind: "rgb", r: 26, g: 27, b: 38 } });
await neosh.win.setHighlights({ kind: "neosh.sidebar" }, { Normal: "Acme.Panel" });

// A theme is a contribution: listed beside `dark` and `light` on `ui.theme`, applied when chosen,
// gone with the plugin. Groups it does not name come from `base`.
await neosh.ext.contribute("ui.theme", "gruvbox", {
  base: "dark",
  groups: { Comment: { fg: { kind: "rgb", r: 146, g: 131, b: 116 } }, "Gruv.Extra": { link: "Normal" } },
});
```

### A var about a buffer or a window

`vars` has two more scopes, `{ scope: "buffer", buf }` and `{ scope: "window", win }` — Neovim's
`b:` and `w:`. In memory only, dropped with the buffer or window, and whoever was watching is told.
The answer to "a var write is a file write": what changes per keystroke is about a buffer or a
window, and those are never written down.

### See what is there

`neosh.ext.points()` lists every point with who reads and who writes it; `neosh.ext.plugins()`
every plugin with its manifest and what became of it — loaded, held, failed; `hl.list()` every
group with its owner. `plugins.list` in `^K` draws all of it: the `:checkhealth` of this workspace.

---

## Saying something to the person

Three different things, and they are three calls rather than three levels of one. `MessageLevel`
says how bad something is; which of these you reach for says whether the user asked for it, which is
what decides where it goes and how long it lives.

```ts
neosh.notify("copied /home/me/proj");            // a reply to a key they just pressed
neosh.progress("acme.index", "indexing…");       // a state, replaced in place
neosh.done("acme.index");                        // …and taken away when it finishes
await neosh.alert("acme", "index is stale", { session });  // news they did not ask for
```

**`notify` is a reply.** Feedback for the keystroke that caused it. It does not stack — a second
replaces the first, because two keys pressed quickly are two keys and the answer you want is the one
for the second — it lives about six seconds, and it never leaves the terminal.

The commonest mistake is using it for something already on screen. If the row moved, the panel
closed or the footer changed, the user can see it; saying so in the corner is the same fact twice,
and a corner that usually restates something visible is a corner people stop reading. If the fact
has a place on screen, put it right there instead — that is what the git plugin does with the branch
segment rather than announcing every checkout.

**`progress` is a state, not a message.** Keyed, so writing the same key again replaces the row and
`done` takes it away. `pulling…` used to be pushed onto the message stack and so was the `up to
date` that superseded it, which is how one pull drew two rows. Put `done` in a `finally` — a row
nobody finishes is dropped after a minute, and relying on that is a row that lies for a minute.

**`alert` is news.** Drawn in the corner *and*, if the host works out that nobody is looking, raised
outside the terminal as a real notification. Whether that second part happens is the host's decision
and never yours: only it knows which conversation is on screen, which terminals are attached and
whether any of them has focus. Pass `session` when it is about one — that is what the "can they see
this already" test is asked against.

It needs `notify` in your `plugin.toml`, and is rejected with `not permitted` otherwise. Drawing in
the corner stays free, like every other kind of drawing; being able to interrupt somebody who is in
another application is a capability.

```toml
permissions = ["notify"]
```

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
but not what the prompt looks like.

---

## Terminals

A workspace can have several terminals attached and they are not copies of each other. Each is
somewhere: its own conversation on screen, its own scroll position, its own composer, its own
panels. What they share is the work — the conversations, the turns running in them, and everything
your plugin registered.

**Most plugins need to know none of this.** A window you open is routed for you: a float anchored
to a window goes where that window is, a buffer only one terminal is showing names it, and
otherwise it is the terminal whose key press is running. So a picker, a preview or a status panel
opened in answer to a key lands where the person who pressed it is looking, without a line about
views.

The one thing that does have to say is a **dock**, because one that exists once exists in one
terminal. Open one per view:

```ts
const panels = new Map<ViewId, Panel>();

neosh.view.onOpen(async (view) => {          // and for the terminals already here
  panels.set(view, await makePanel(neosh.view.at(view)));
});
neosh.view.onClose((view) => {               // its windows are already gone
  panels.delete(view);
});
```

`neosh.view.at(id)` is the whole `neosh` namespace bound to one terminal — every call on it is the
call it always was, except that a window opened through it lands there. Inside a command handler
you are handed one already, as the third argument:

```ts
await neosh.cmd.register("mine.panel", async (args, key, here) => {
  const buf = await neosh.buf.create({ name: "[mine]", scratch: true });
  await here.win.open(buf, "right", { size: 40 });   // this terminal
});
```

`key.view` is the terminal the key was pressed in, `neosh.view.list()` is every terminal and what
each is looking at, and `session.onChange` says which one moved. If you are drawing something a
person has to answer, that last one is what tells you which screen to put it on.

## Panes and tabs

A terminal's main region is divided into **panes**, and a tab is a tree of them. A pane is *where*,
not *what*: it owns no buffer and draws nothing. You split one, then dock windows into it — which is
why splitting a chat and splitting anything else are the same two calls.

```ts
const here = await neosh.pane.active();
const right = await neosh.pane.split(here, "right");
await neosh.win.open(buf, "main", { pane: right });          // fills it
await neosh.win.open(field, "bottom", { pane: right, size: 3 });  // a field at its foot
await neosh.pane.focus(right);
```

`win.open` takes the same four edges whether or not you name a pane — with one, the edge is measured
against the pane's rectangle; without, against the screen's. That is the whole difference, and it is
why nothing needed a second geometry vocabulary. **Omit `pane` for anything that belongs to the
terminal rather than to one split**: a sidebar docked into a pane is a sidebar that disappears when
you close that split.

A window that names no pane and asks for `"main"` lands in the pane the person is *looking at* — the
same rule as a float that names no view, one level down. So an existing plugin that opens a main
window keeps working and keeps landing somewhere sensible.

```ts
await neosh.pane.focusDir("left");         // null at the edge; there is no wrap-around
await neosh.pane.resize(here, "right", 10); // weight units, not cells
await neosh.pane.swap(a, b);                // how a pane is moved: widths belong to the slots
await neosh.pane.close(here);               // rejects on the last pane of the last tab

const { tabs, active } = await neosh.tab.list();
await neosh.tab.create({ title: "review", activate: false });
await neosh.tab.rename(tabs[0].id, null);   // back to being named by what is in it
```

Sizes are **weights**, never cells: an even split is `WEIGHT` each, so a layout means the same thing
at every terminal size and survives somebody dragging the window's corner. Splitting never nests a
row inside a row — three panes side by side are one split of three — so a resize divides space you
can actually see.

Everything the workspace's own `^W` keys do is these calls and nothing else. `^Wv` is
`pane.split(await pane.active(), "right")`; there is no private path, so a different window manager
is a plugin rather than a fork. The tab strip is an ordinary buffer of kind `neosh.tabline`: bind
keys against it, recolour it with `win.setHighlights`, or turn ours off and dock your own `"top"`
window in its place.

## Conversations

```ts
await neosh.session.list();                          // what the user works in
await neosh.session.list({ includeArchived: true }); // plus what they put away
await neosh.session.archive(id);                     // reversible, keeps everything
await neosh.session.archive(id, false);              // back, and to the top of the list
await neosh.session.close(id);                       // deletes the file. No undo.
```

### Driving one you are not looking at

`agent.command` does something *to* a named conversation — the same vocabulary `swarm.command`
carries to another machine, pointed at one here. Omit the id for the conversation on screen.

```ts
const id = await neosh.agent.command({ command: "new_session", title: "the tests" });
await neosh.agent.command({ command: "send", text: "run the suite and report" }, id!);
await neosh.agent.command({ command: "interrupt" }, id!);
await neosh.agent.command({ command: "set_model", instance: "anthropic", model: "…" }, id!);
```

That plus `onTurnEnd` — which says which conversation ended — is an orchestrator: fan work out over
several conversations, join on their endings, and the screen never moves. `session.switch` before
each message is the thing this replaced; it works, and it drags the transcript out from under
whoever is reading it.

### Deciding who answers

`turn.route` fires once a turn knows what it is going to say and before it knows who to say it to.
A blocking hook there may re-point it, or refuse it with a reason the transcript prints.

```ts
await neosh.hook.register("turn_route", (p) => {
  if (p.hook !== "turn_route") return { action: "continue" };
  if (!p.text.startsWith("quick:")) return { action: "continue" };
  return {
    action: "modify",
    payload: { ...p, selection: { instance: "local", model: "small", options: [] } },
  };
}, { blocking: true });
```

`selection` may be `null` — a conversation with no model chosen is a routable state, not an error,
so a router may supply one. Needs `hooks_blocking` in the manifest.

---

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
