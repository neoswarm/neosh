/**
 * The command palette.
 *
 * One key that reaches everything, which is the point of the pattern: a workspace accumulates more
 * actions than a keyboard has comfortable chords, and the alternative to a palette is a menu bar
 * nobody reads or a cheat sheet nobody opens.
 *
 * It searches what the *registry* knows rather than a hand-written list, so a command registered by
 * a plugin loaded five minutes ago is findable without this file changing. Conversations are in the
 * same list because "go to the thing I was doing" and "run the thing" are the same intent arriving
 * through the same key.
 */

import type { KeymapEntry, Neosh, PluginContext } from "@neosh/api";
import { byteLength } from "@neosh/api";
import { picker } from "@neosh/api/ui";

type Entry =
  | { kind: "command"; name: string }
  | { kind: "session"; id: string };

export async function activate({ neosh, subscriptions }: PluginContext) {
  subscriptions.push(
    await neosh.cmd.register("commandPalette.toggle", () => open(neosh), {
      desc: "Search commands and conversations",
    }),
  );
  subscriptions.push(
    await neosh.cmd.register("help.keys", () => showKeys(neosh), {
      desc: "Show every key binding",
    }),
  );
  // Registered once rather than per overlay: a command name is global, and re-registering it on
  // every `<C-z>` would leave the previous window's handler shadowed and its float on screen.
  subscriptions.push(
    await neosh.cmd.register("help.keys.key", () => void dismissKeys?.(), {
      desc: "Dismiss the key list",
    }),
  );

  // `:checkhealth`: every plugin, what became of it, and what each one put in the registries.
  // No default key — `^K` runs it by name.
  subscriptions.push(
    await neosh.cmd.register("plugins.list", () => showPlugins(neosh), {
      desc: "Every plugin: loaded, held or failed, and what each registered",
    }),
  );

  await neosh.keymap.set("chat", "<C-k>", "commandPalette.toggle", { desc: "Command palette" });
  // `^Z` rather than `<F1>`. A function key is not a key everybody has: Apple's top row is
  // brightness and volume until somebody goes into the settings, so the key that lists every key
  // was the one key a new Mac could not press — and a keyboard whose F-row lives on a layer is in
  // the same position. `^Z` is the one chord chat mode had left, every terminal delivers it
  // identically, and raw mode means it cannot suspend anything. Rebind it like any other default.
  await neosh.keymap.set("chat", "<C-z>", "help.keys", { desc: "Key bindings" });

  // Last on the row, and the way out of it: whatever else got dropped for width is in here.
  await neosh.hint.set("commands", { keys: "^K", label: "commands", priority: 30 });
  await neosh.hint.set("keys", { keys: "^Z", label: "keys", priority: 31 });
}

async function open(neosh: Neosh): Promise<void> {
  const [commands, keymaps, sessions, current] = await Promise.all([
    neosh.cmd.list(),
    neosh.keymap.list("chat"),
    neosh.session.list().catch(() => []),
    neosh.session.current().catch(() => null),
  ]);

  // A command's binding is what makes the palette teach rather than merely dispatch: you look
  // something up twice and the third time you use the key.
  const keyFor = new Map<string, string>();
  for (const k of keymaps) {
    if (!keyFor.has(k.command)) keyFor.set(k.command, k.lhs);
  }

  const items = [
    // Conversations first: switching is the most frequent thing anyone does here, and the current
    // one is excluded because "switch to where I already am" is not an action.
    ...sessions
      .filter((s) => s.id !== current?.id)
      .map((s) => ({
        label: s.label,
        detail: "conversation",
        keywords: s.cwd,
        value: { kind: "session", id: s.id } as Entry,
      })),
    ...commands
      // The palette's own entry would be a loop, and the key handlers registered by widgets are
      // internal plumbing rather than things to invoke.
      .filter((c) => c.name !== "commandPalette.toggle" && !c.name.includes(".key"))
      .map((c) => ({
        label: c.name,
        detail: [keyFor.get(c.name), c.desc].filter(Boolean).join("   "),
        keywords: `${c.desc ?? ""} ${c.plugin}`,
        value: { kind: "command", name: c.name } as Entry,
      })),
  ];

  const chosen = await picker(neosh, items, {
    title: "Go to",
    placeholder: "nothing matches",
    width: 78,
    height: 14,
  });
  if (!chosen) return;

  try {
    if (chosen.kind === "session") await neosh.session.switch(chosen.id);
    else await neosh.cmd.exec(chosen.name);
  } catch (e) {
    neosh.notify(String(e), "warn");
  }
}

/**
 * Every binding, grouped by mode.
 *
 * Read from the registry rather than written down, so it cannot drift — and so a plugin's keys
 * appear here without that plugin knowing this exists. It closes on any key, because a help window
 * you have to work out how to dismiss is a help window that taught you the wrong thing first.
 */
async function showKeys(neosh: Neosh): Promise<void> {
  const modes = ["chat", "normal", "insert", "visual"] as const;
  const commands = await neosh.cmd.list();
  const describe = new Map(commands.map((c) => [c.name, c.desc ?? ""]));

  // Which panel you asked from, if you asked from one.
  //
  // `?` in the sidebar means "what can I do *here*", and a list that answers with thirty global
  // bindings and puts the panel's own keys below the fold has answered a different question. The
  // window is found by kind rather than by anything the panel told us, so this works for a panel
  // this plugin has never heard of.
  const here = (await neosh.win.list().catch(() => []))
    .find((w) => w.focused)?.kind ?? null;

  interface Row {
    text: string;
    /** Byte range of the key itself, so it can be highlighted apart from its description. */
    key?: [number, number];
    heading?: boolean;
  }
  const rows: Row[] = [];

  /** One group of bindings under a heading, keys padded to a common column. */
  const emit = (title: string, maps: KeymapEntry[]) => {
    if (maps.length === 0) return;
    if (rows.length > 0) rows.push({ text: "" });
    rows.push({ text: title, heading: true });
    const width = Math.max(...maps.map((m) => m.lhs.length));
    for (const m of [...maps].sort((a, b) => a.lhs.localeCompare(b.lhs))) {
      const note = m.desc ?? describe.get(m.command) ?? m.command;
      rows.push({ text: `  ${m.lhs.padEnd(width)}  ${note}`, key: [2, 2 + byteLength(m.lhs)] });
    }
  };

  for (const mode of modes) {
    const maps = await neosh.keymap.list(mode);
    if (maps.length === 0) continue;
    // Bindings scoped to a buffer kind get a section of their own, named after the kind.
    //
    // Not cosmetic: a panel's keys are ordinary bindings now, so `^N` is legitimately two things —
    // "new conversation" everywhere and "next row" in the sidebar — and one flat list showing the
    // same key twice with no way to tell which is which is worse than not listing them. The heading
    // is where the answer goes, and it is read from the binding rather than written down, so a
    // panel somebody else wrote gets a section here without knowing this exists.
    const byKind = new Map<string, KeymapEntry[]>();
    const plain: KeymapEntry[] = [];
    for (const m of maps) {
      if (m.scope.kind === "buf_kind") {
        const list = byKind.get(m.scope.name) ?? [];
        list.push(m);
        byKind.set(m.scope.name, list);
      } else {
        plain.push(m);
      }
    }
    // The panel you are standing in goes first; everything else keeps its alphabetical place.
    const ordered = [...byKind].sort((a, b) =>
      a[0] === here ? -1 : b[0] === here ? 1 : a[0].localeCompare(b[0])
    );
    const first = ordered.filter(([kind]) => kind === here);
    for (const [kind, list] of first) emit(`${mode.toUpperCase()}  ·  ${kind}`, list);
    emit(mode.toUpperCase(), plain);
    for (const [kind, list] of ordered.filter(([kind]) => kind !== here)) {
      emit(`${mode.toUpperCase()}  ·  ${kind}`, list);
    }
  }

  // What is left: keys that are not bindings at all, but what the *host* does with a key nothing
  // claimed. The registry cannot know these, so this is the one place they are written down.
  //
  // The project panel used to be on this list and no longer is — its keys became ordinary bindings,
  // so they come out of the registry above, complete with whatever a third party has added to them.
  // That is the difference this section is now measuring: everything above is discovered,
  // everything below had to be remembered.
  for (const [title, keys] of [["COMPOSER", COMPOSER_KEYS], [
    "READING THE TRANSCRIPT",
    READING_KEYS,
  ]] as const) {
    rows.push({ text: "" });
    rows.push({ text: title, heading: true });
    for (const [lhs, note] of keys) {
      rows.push({ text: `  ${lhs.padEnd(9)}  ${note}`, key: [2, 2 + byteLength(lhs)] });
    }
  }

  if (rows.length === 0) rows.push({ text: "no bindings" });
  rows.push({ text: "" });
  rows.push({ text: "  any key closes this", heading: false });

  // A second `<C-z>` replaces the first window rather than stacking one behind it.
  await dismissKeys?.();

  const buf = await neosh.buf.create({ name: "[keys]", scratch: true });
  const ns = await neosh.ns.create("neosh.help");
  await neosh.buf.setLines(buf, 0, -1, rows.map((r) => r.text));
  for (let i = 0; i < rows.length; i++) {
    const r = rows[i]!;
    if (r.heading) {
      await neosh.ns.mark(ns, buf, i, 0, { hlGroup: "Title", endCol: byteLength(r.text) });
    } else if (r.key) {
      await neosh.ns.mark(ns, buf, i, r.key[0], { hlGroup: "Key", endCol: r.key[1] });
    }
  }

  const win = await neosh.float.open(buf, {
    anchor: { kind: "screen" },
    width: { kind: "max", n: 64 },
    height: { kind: "max", n: 24 },
    border: "rounded",
    title: " keys ",
    closeOnBlur: true,
    focusable: true,
    // Above the panels, because this is asked *from* one. `?` in a panel is the key that answers
    // "what do the keys here do", and every panel worth asking it in is a float of its own — so at
    // the default depth the answer opened underneath the question and read as nothing happening.
    z: 300,
  });
  await neosh.focus.push(win);

  const capture = await neosh.keymap.capture(win, "help.keys.key").catch(() => null);
  dismissKeys = async () => {
    dismissKeys = null;
    capture?.dispose();
    await neosh.focus.pop().catch(() => {});
    await neosh.win.close(win).catch(() => {});
  };
}

/** Closes whichever key list is open, if any. Set while one is on screen and cleared as it goes. */
let dismissKeys: (() => Promise<void>) | null = null;

/**
 * What the project panel's captured keys do.
 *
 * A capture is not a keymap, so `keymap.list` cannot see these — and a help screen that omits the
 * keys for the thing on the left of the screen is worse than no help screen.
 */
/**
 * The composer's own keys.
 *
 * Also not keymaps: they are what the host does with a key nothing claimed, which is the only way
 * a text field can have a hundred behaviours without taking a hundred names out of the keymap
 * namespace. Written down here because a text field whose keys are undiscoverable is a text box.
 */
const COMPOSER_KEYS: [string, string][] = [
  ["← →", "by character"],
  ["^← ^→", "by word"],
  ["↑ ↓", "between lines, or scroll the transcript"],
  ["Home End", "start and end of the line"],
  ["^Home ^End", "start and end of the draft"],
  ["Shift+…", "any motion, extending a selection"],
  ["S-CR", "new line instead of sending"],
  ["^W", "delete the word behind the cursor"],
  ["^U", "delete back to the start of the line"],
  ["^A", "select everything"],
  ["^C", "copy the selection — or clear the draft when there is none"],
  ["^X", "cut the selection"],
];

/** What the keys mean once you are in the transcript. */
const READING_KEYS: [string, string][] = [
  ["hjkl", "move, and so do the arrows"],
  ["w b", "by word"],
  ["0 $", "start and end of the line"],
  ["g G", "top and bottom"],
  ["v", "start selecting; motions extend it"],
  ["a", "select the whole transcript"],
  ["y", "copy and leave"],
  ["Esc q", "leave"],
];



/**
 * The plugins panel: one row per plugin, and on `↵` what that plugin put on the surface.
 *
 * Drawn from the registries — `cmd.list`, `keymap.list`, `hl.list`, `ext.points` — rather than
 * from anything a plugin says about itself, so a plugin that registered a command it does not
 * mention in its manifest is listed with it anyway. The manifest's `provides` is shown beside,
 * which is how a point nobody reads and a reader nobody declared both become visible.
 */
async function showPlugins(neosh: Neosh): Promise<void> {
  const plugins = await neosh.ext.plugins();
  const mark = (state: string) =>
    state === "loaded" ? { icon: "●", hl: "Diagnostic.Ok" }
    : state === "held" ? { icon: "○", hl: "Sidebar.Dim" }
    : { icon: "✗", hl: "Diagnostic.Error" };
  const chosen = await picker<string>(
    neosh,
    plugins.map((p) => ({
      label: p.name,
      detail: `${p.state}${p.bundled ? " · bundled" : ""}${p.manifest.description ? ` · ${p.manifest.description}` : ""}`,
      keywords: p.state,
      ...mark(p.state),
      value: p.name,
    })),
    { title: "Plugins", width: 80 },
  );
  if (chosen === null) return;
  const info = plugins.find((p) => p.name === chosen);
  if (!info) return;

  const [commands, keymaps, groups, points] = await Promise.all([
    neosh.cmd.list(),
    neosh.keymap.list(),
    neosh.hl.list(),
    neosh.ext.points(),
  ]);
  const mine = commands.filter((c) => c.plugin === chosen).map((c) => c.name);
  const keys = keymaps.filter((k) => mine.includes(k.command));
  const colours = groups.filter((g) => g.owner === chosen).map((g) => g.name);
  const reads = points.filter((p) => p.readers.includes(chosen)).map((p) => p.point);
  const writes = points.filter((p) => p.contributors.includes(chosen)).map((p) => p.point);

  const rows: Array<{ label: string; detail?: string; hl?: string; icon?: string }> = [];
  const section = (title: string, items: string[], detail?: (s: string) => string | undefined) => {
    if (items.length === 0) return;
    rows.push({ label: title, hl: "Sidebar.Heading", icon: " " });
    for (const it of items) rows.push({ label: `  ${it}`, detail: detail?.(it) });
  };
  if (info.error) rows.push({ label: info.error, hl: "Diagnostic.Error", icon: "✗" });
  // Every list on a manifest is optional on the wire — absent when empty.
  const m = info.manifest;
  const list = (v: string[] | null | undefined) => v ?? [];
  const act = m.activation ?? {};
  const provides = m.provides ?? {};
  section("manifest", [
    `version ${m.version}`,
    ...(list(m.requires).length ? [`requires ${list(m.requires).join(", ")}`] : []),
    ...(list(m.after).length ? [`after ${list(m.after).join(", ")}`] : []),
    ...(list(m.permissions).length ? [`permissions ${list(m.permissions).join(", ")}`] : []),
    ...(list(act.on_command).length ? [`loads on command ${list(act.on_command).join(", ")}`] : []),
    ...(list(act.on_event).length ? [`loads on event ${list(act.on_event).join(", ")}`] : []),
    ...(list(act.on_kind).length ? [`loads on kind ${list(act.on_kind).join(", ")}`] : []),
  ]);
  section("kinds", list(provides.kinds));
  section("points read", reads);
  section("points written", writes, (p) => {
    const r = points.find((q) => q.point === p);
    return r && r.readers.length === 0 ? "nothing reads this" : undefined;
  });
  section("vars", list(provides.vars));
  section("highlights", colours);
  // Last, because it is the longest: a panel has forty verbs and one manifest.
  section("commands", mine, (name) => {
    const bound = keys.filter((k) => k.command === name).map((k) => k.lhs);
    return bound.length ? bound.join("  ") : undefined;
  });
  if (rows.length === 0) rows.push({ label: "registered nothing", hl: "Sidebar.Dim" });

  await picker<number>(
    neosh,
    rows.map((r, i) => ({ label: r.label, detail: r.detail, hl: r.hl, icon: r.icon, value: i })),
    { title: chosen, width: 80, height: 24, filter: false, hints: "" },
  );
}
