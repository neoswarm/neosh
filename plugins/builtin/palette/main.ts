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

import type { Neosh, PluginContext } from "@neosh/api";
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
  // every `<F1>` would leave the previous window's handler shadowed and its float on screen.
  subscriptions.push(
    await neosh.cmd.register("help.keys.key", () => void dismissKeys?.(), {
      desc: "Dismiss the key list",
    }),
  );

  await neosh.keymap.set("chat", "<C-k>", "commandPalette.toggle", { desc: "Command palette" });
  await neosh.keymap.set("chat", "<F1>", "help.keys", { desc: "Key bindings" });

  // Last on the row, and the way out of it: whatever else got dropped for width is in here.
  await neosh.hint.set("commands", { keys: "^K", label: "commands", priority: 30 });
  await neosh.hint.set("keys", { keys: "F1", label: "keys", priority: 31 });
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

  interface Row {
    text: string;
    /** Byte range of the key itself, so it can be highlighted apart from its description. */
    key?: [number, number];
    heading?: boolean;
  }
  const rows: Row[] = [];

  for (const mode of modes) {
    const maps = await neosh.keymap.list(mode);
    if (maps.length === 0) continue;
    if (rows.length > 0) rows.push({ text: "" });
    rows.push({ text: mode.toUpperCase(), heading: true });
    const width = Math.max(...maps.map((m) => m.lhs.length));
    for (const m of [...maps].sort((a, b) => a.lhs.localeCompare(b.lhs))) {
      const note = m.desc ?? describe.get(m.command) ?? m.command;
      const lhs = m.lhs.padEnd(width);
      rows.push({ text: `  ${lhs}  ${note}`, key: [2, 2 + byteLength(m.lhs)] });
    }
  }

  // The panel's own keys are a capture rather than a keymap, so the registry cannot know them and
  // this is the one place they are written down.
  for (const [title, keys] of [["PROJECT PANEL", PANEL_KEYS], ["COMPOSER", COMPOSER_KEYS], [
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

  // A second `<F1>` replaces the first window rather than stacking one behind it.
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

const PANEL_KEYS: [string, string][] = [
  ["j / k", "move"],
  ["Enter", "open a conversation, or fold a project"],
  ["Space", "fold or unfold a project"],
  ["f", "pin a project to the top"],
  ["J / K", "move a project up or down"],
  ["n", "new conversation in this project"],
  ["o", "open another project"],
  ["r", "rename a conversation"],
  ["x", "close a conversation"],
  ["?", "this list"],
  ["Esc", "back to the composer"],
];
