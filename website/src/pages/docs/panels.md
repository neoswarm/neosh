---
layout: ../../layouts/Docs.astro
title: Panels and extension points
description: A panel is a surface, not a program. Anything a bundled plugin draws, yours can add to, take keys in, recolour, and replace, without forking it.
---

## The idea

Four mechanisms, and between them you should not have to fork a bundled plugin to change it: bind a key by buffer kind, contribute rows and verbs through named points, recolour through owned highlight groups, and hear what happens on the event bus. A new panel of your own gets all four from `ListPanel` for the price of a kind and a rows function.

## Bind a key inside a panel you did not open

A panel declares what it is with a buffer kind, and you bind against the kind rather than a window id that is private and changes on every toggle:

```ts
// In your init.ts. `x` archives by default; this replaces it, in the sidebar only.
await neosh.keymap.set("chat", "x", "acme.mine", {
  scope: { kind: "buf_kind", name: "neosh.sidebar" },
});
```

Resolution is window, buffer, kind, then global, first match winning. Your binding is ordinary: `^Z` lists it under the panel's section, and the panel's default loses to it.

## Contribute rows, verbs and marks

A contribution point is a name a plugin agrees to read. The sidebar reads three, and they are the pattern every panel follows:

```ts
// Rows in the column. Re-contributing under the same id replaces, so this is also the update.
await neosh.ext.contribute("sidebar.section", "todo", {
  title: "ACME",
  before: "add",                       // a named slot, or another section's id
  rows: [{ text: "Ship the thing", command: "acme.open", args: ["thing"] }],
});

// A mark on a row the panel already draws, keyed by what the row is about.
// Data, never a row-renderer callback: one slow decorator would be a panel that lags on j.
await neosh.ext.contribute("sidebar.decoration", `prs:${cwd}`, {
  target: { project: cwd },            // or { session: id }
  badge: { text: "2 PRs", hl: "Accent" },
});

// A verb on a row. The panel binds the key and invokes your command with the row under the cursor.
await neosh.ext.contribute("sidebar.action", "touch", {
  key: "t",
  label: "touch",
  command: "acme.touch",
  on: "session",                       // or "project", "any", "custom", "custom:<section id>"
});
```

`on: "custom:todo"` is a verb about **your** rows — the ones the `todo` section above put there — and it is what you want nearly every time. Bare `custom` is every contributed row in the column, which stops meaning "mine" the moment a second plugin has a block in it. Several plugins may ask for the same key on their own rows and all of them are right: the panel binds it once and sends the press to whichever of you the cursor is actually over, and the key strip at the foot prints the one that would fire.

Your contributions go when your plugin does, so `plugins.disabled` takes your rows with it. Declare the points you read under `[provides]` in your manifest; a contribution to a point nobody reads is reported at startup with the nearest real name, `did you mean "sidebar.section"?`.

Reading a point in a panel of your own is `ext.list(point)` plus a redraw on `ext.onChange`, and that is the whole protocol.

## A panel of your own

`ListPanel` gives you everything above for a kind and a `rows` function:

```ts
import { ListPanel } from "@neosh/api/ui";

const panel = await ListPanel.create<Task>(neosh, {
  kind: "acme.tasks",
  dock: "right",
  size: () => 30,
  rows: () => tasks.map((t) => ({ text: ` ▸ ${t.title}`, value: t })),
  key: (t) => ({ task: t.id }),        // what decorations target
  kindOf: () => "task",                // what a contributed action's `on` may name
  onOpen: (t) => openTask(t),
});
subscriptions.push({ dispose: () => panel.dispose() });
```

That is a buffer of kind `acme.tasks` in a dock; `acme.tasks.down`, `.up`, `.open`, `.toggle`, `.focus`, `.refresh`, `.cursor` and `.rows` as commands bound at kind scope so `init.ts` can move them; `acme.tasks.section`, `.action` and `.decoration` read exactly as the sidebar reads its own; the cursor published as a buffer var and an event; and a redraw when any of it changes.

## The widgets

`@neosh/api/ui` is built entirely on the public API; nothing in it is privileged.

```ts
import { picker, prompt, confirm, confirmDestructive, railPicker } from "@neosh/api/ui";

const branch = await picker(neosh, rows, { title: "Switch branch", width: 76 });
// null when dismissed. `source` makes it a finder that re-queries per keystroke;
// `freeform` accepts what was typed; `onKey` + `ownKeys` put verbs on rows.

const name = await prompt(neosh, "Branch name", { initial: suggested });

if (!(await confirmDestructive(neosh, `Delete ${name}?`, {
  yes: "Delete",
  no: "Keep",
  detail: [`${count} messages, in ${project}.`, "Archiving keeps every word of it."],
}))) return;
```

`confirmDestructive` reads `ui.confirm_destructive`, starts the cursor on the answer that changes nothing, and draws the other in the error colour, so the setting means one thing everywhere without your plugin knowing it exists. The bar is irreversible, not merely significant: closing a panel asks nothing, deleting a file always asks.

`railPicker` is the two-pane shape the model switcher is built from, for when one list would answer two questions at once. Widget keys come from `ui.keys.*` and are claimed window-scoped while open; you get that for free.

## Building on another plugin

```ts
// Typed and free of round trips, for a plugin you `requires` in the manifest.
// This is the very module the host activated, not a second copy of its source.
import { api as sidebar } from "plugin:sidebar";
const row = sidebar.cursor();

// Through the host, for a plugin you would rather not depend on.
const row2 = await neosh.cmd.call<Target | null>("sidebar.cursor");
```

`init.ts` loads before every plugin, so a static `plugin:` import there finds nothing; use a dynamic import inside `neosh.event.on("neosh.ready", …)`.

## Terminals, docks and modality

Most windows are routed for you: a float anchored to a window goes where that window is, and anything opened in answer to a key lands where the key was pressed. A dock is the exception, because one that exists once exists in one terminal:

```ts
const panels = new Map<ViewId, Panel>();
neosh.view.onOpen(async (view) => panels.set(view, await makePanel(neosh.view.at(view))));
neosh.view.onClose((view) => panels.delete(view));
```

Inside a command handler the third argument is already the API bound to the right terminal: `here.win.open(...)` lands where the person pressing the key is looking.

A panel you are in the middle of using can take the keyboard: `FloatConfig::modal` takes global keys out of the chain, and a key nothing claimed is swallowed rather than reaching the composer behind the float. A modal that borrows a global key to open itself owes a binding to close itself.

## Health

`neosh.ext.points()` lists every point with who reads and writes it; `neosh.ext.plugins()` every plugin with its manifest and what became of it, loaded, held or failed; `hl.list()` every group with its owner. `plugins.list` in `^K` draws all of it: the checkhealth of this workspace.
