/**
 * Settings: every option in the workspace on one screen, saved as you change it.
 *
 * # Built from the registry, not from a list
 *
 * Every setting neosh has is an option somebody *declared* — `neosh.opt.declare`, with a type, a
 * default and a sentence — and a plugin's are declared through exactly the same call. So this panel
 * does not know what settings exist. It asks (`opt.all()`), groups them by the namespace in front of
 * the dot, and draws a control for each from its declared type: a switch for a `bool`, a ladder for
 * an `enum`, a number you can nudge for an `int`, a field for a `str`. A plugin you installed ten
 * minutes ago has a section here with nobody writing glue, which is the whole of the design.
 *
 * What is curated sits on top as **pages** — `settings.page` contributions, a title and a list of
 * option names. `General` is one of ours: the handful of settings people actually go looking for,
 * under names that say what they do rather than what they are called. A plugin can add a page of its
 * own the same way, and one that names an option nobody declared simply shows fewer rows.
 *
 * # Saved, and saved into the file that is yours
 *
 * A change is **applied and written** in one step — `opt.save`, which sets the value and puts it in
 * `[options]` in `config.toml` without touching a comment or a line of anything else in there. The
 * reason is the rule the model sheet already follows: nothing reversible asks, and a control that
 * only takes effect when you press the right key to leave is a control you have to be taught. `r`
 * takes a value back to its default and its line back out of the file.
 *
 * # A panel, not a program
 *
 * Every key is an ordinary binding on the buffer kind `neosh.settings`, pointed at a named command,
 * so `^Z` lists them and `init.ts` moves them. The float is modal — nothing global reaches the
 * keyboard while it is up — and it is reached from `,` in the project panel, `/settings` in the
 * composer and `^K` by name, because every Ctrl-letter a terminal sends is already somebody's.
 */

import type {
  BufferId,
  Disposable,
  FloatOptions,
  KeyContext,
  KeymapEntry,
  Neosh,
  OptionEntry,
  OptionValue,
  PluginContext,
  WindowId,
} from "@neosh/api";
import { byteLength, clipToWidth, padToWidth, width } from "@neosh/api";
import { onPaste, prettyKey, prompt, wrapToWidth } from "@neosh/api/ui";

const KIND = "neosh.settings";
const KIND_TEXT = "neosh.settings.text";
const NS = "neosh.settings";

/**
 * A curated page: a title, and the options on it in the order they should be read.
 *
 * `label` is what the row says instead of the option's name, which is how `agent.append_prompt`
 * becomes *Appended to every message* on the page people open to find it.
 */
const POINT_PAGE = "settings.page";
interface PageItem {
  title: string;
  options: Array<string | { name: string; label?: string }>;
}

/** What a namespace is called as a section, where the prefix alone would not say it. */
const TITLES: Record<string, string> = {
  agent: "Agent",
  chat: "Chat",
  ui: "Interface",
  sidebar: "Sidebar",
  git: "Git",
  worktree: "Worktrees",
  clone: "Cloning",
  gen: "Generation",
  notify: "Notifications",
  archive: "Archive",
  usage: "Usage",
  plugins: "Plugins",
  update: "Updates",
  session: "Conversations",
  model: "Models",
  approvals: "Approvals",
  questions: "Questions",
  editor: "Editor",
};

/** The namespaces in the order a person would look through them; anything else follows by name. */
const ORDER = ["agent", "chat", "ui", "sidebar", "git", "worktree", "clone", "notify", "gen"];

/** Options whose value is prose, edited in a field that takes more than one line. */
const PROSE = new Set(["agent.append_prompt", "agent.system_prompt"]);

type Row =
  | { kind: "option"; name: string; label: string }
  | { kind: "key"; lhs: string; command: string; desc: string }
  | {
    kind: "machine";
    id: string;
    name: string;
    code: string;
    /** The group the project panel draws this machine in. */
    color: string;
    up: boolean;
    state: string;
  };

interface Section {
  id: string;
  title: string;
  rows: Row[];
  /** Said under the list when nothing is selected worth describing. */
  note?: string;
}

interface Sheet {
  win: WindowId;
  buf: BufferId;
  ns: number;
  sections: Section[];
  /** Which section is showing. */
  at: number;
  /** The row the keys act on, remembered per section so switching back lands where you were. */
  cursors: Map<string, number>;
  /** The first list row on screen. */
  top: number;
  /** Inside the border, as the frontend last reported it. */
  cols: number;
  rows: number;
  entries: Map<string, OptionEntry>;
  /** Said once per open when saving fails, so a `--clean` run is not a toast per keystroke. */
  warned: boolean;
  close(): Promise<void>;
}

let sheet: Sheet | null = null;
/**
 * The keys, in the order they were pressed. Every verb here is several round trips and the host
 * dispatches each key without waiting for the last, so without a chain `l l l` fast finishes in
 * length order rather than press order — the same reason the model sheet chains its own.
 */
let queue: Promise<void> = Promise.resolve();
let textSeq = 0;

export async function activate({ neosh, subscriptions }: PluginContext) {
  await defineHighlights(neosh);

  // The page people open this to find. Ours, contributed like anybody's, so it can be replaced or
  // joined by a plugin that knows better what its users go looking for.
  subscriptions.push(
    await neosh.ext.contribute(POINT_PAGE, "general", {
      title: "General",
      options: [
        { name: "agent.append_prompt", label: "Appended to every message" },
        { name: "git.pull.auto", label: "Keep main checkouts up to date" },
        { name: "git.fetch.interval", label: "Check remotes every (seconds)" },
        { name: "agent.model", label: "Model" },
        { name: "gen.model", label: "Model for names and titles" },
        { name: "ui.theme", label: "Theme" },
        { name: "ui.confirm_destructive", label: "Ask before deleting" },
        { name: "chat.show_thinking", label: "Show the model's reasoning" },
        { name: "sidebar.width", label: "Sidebar width" },
        { name: "worktree.root", label: "Where worktrees go" },
      ],
    } satisfies PageItem, { priority: 100 }),
  );

  await installKeys(neosh, subscriptions);

  subscriptions.push(
    await neosh.cmd.register("settings.open", async (args) => {
      await open(neosh, args[0]);
    }, { desc: "Settings — every option, saved to config.toml as you change it" }),
  );

  // A value that moved while the panel is up — `>` widening the sidebar, a plugin setting its own —
  // is the panel's news too: what is on screen has to be what is in effect.
  subscriptions.push(neosh.opt.onChange((e) => {
    const s = sheet;
    if (!s) return;
    void neosh.opt.entry(e.name).then((entry) => {
      if (entry && sheet === s) {
        s.entries.set(e.name, entry);
        void draw(neosh, s);
      }
    }).catch(() => {});
  }));
  // A page came or went while the panel is open.
  subscriptions.push(neosh.ext.onChange((e) => {
    if (e.point === POINT_PAGE && sheet) void rebuild(neosh, sheet);
  }));
  // An option *declared* after the panel opened — a plugin still loading, one held until a command
  // woke it. Declaring says nothing on the bus, so the open panel asks: when the workspace says it
  // has finished loading, and on a slow clock while it is up, rebuilding only when the set of names
  // actually moved. Without this a panel opened in the first second of a workspace was missing the
  // settings of every plugin that had not got there yet, for as long as it stayed open.
  const recount = async () => {
    const s = sheet;
    if (!s) return;
    const names = (await neosh.opt.all().catch(() => [] as OptionEntry[])).map((e) => e.name);
    if (sheet !== s) return;
    if (names.length !== s.entries.size || names.some((n) => !s.entries.has(n))) {
      await rebuild(neosh, s);
    }
  };
  subscriptions.push(neosh.event.on("neosh.ready", () => void recount()));
  subscriptions.push(neosh.timer.every(1500, () => void recount()));
  subscriptions.push(neosh.event.on("neosh.viewport", (e) => {
    const d = e.data as { win?: WindowId; width?: number; height?: number } | null;
    const s = sheet;
    if (!s || d?.win !== s.win) return;
    if (typeof d.width === "number") s.cols = d.width;
    if (typeof d.height === "number") s.rows = d.height;
    void draw(neosh, s);
  }));

  // The way in, on the strip over the composer — low in the order, so it is the first hint to give
  // way on a narrow terminal, and there on every other.
  await neosh.hint.set("settings", { keys: "/settings", label: "settings", priority: 60 })
    .catch(() => {});

  subscriptions.push({ dispose: () => void sheet?.close() });
}

// ---------------------------------------------------------------------------
// What is on the screen
// ---------------------------------------------------------------------------

/** Every section, worked out afresh: the options that exist now, the pages, the machines, the keys. */
async function sections(neosh: Neosh, entries: Map<string, OptionEntry>): Promise<Section[]> {
  const out: Section[] = [];

  const pages = await neosh.ext.list<PageItem>(POINT_PAGE).catch(() => []);
  for (const page of pages) {
    const item = page.item;
    if (!item || typeof item.title !== "string" || !Array.isArray(item.options)) continue;
    const rows: Row[] = [];
    for (const o of item.options) {
      const name = typeof o === "string" ? o : o?.name;
      if (typeof name !== "string" || !entries.has(name)) continue;
      const label = typeof o === "string" ? undefined : o.label;
      rows.push({ kind: "option", name, label: label || humanize(name, true) });
    }
    if (rows.length > 0) out.push({ id: `page:${page.plugin}:${page.id}`, title: item.title, rows });
  }

  // The other computers, when there are any: what each is drawn as in the project panel, and a way
  // to change it. Asked of the sidebar by command rather than worked out here, so the two can never
  // disagree about which machine is `@ms`.
  const nodes = await neosh.swarm.nodes().catch(() => []);
  const me = (await neosh.swarm.self().catch(() => null))?.id;
  const peers = nodes.filter((n) => n.info.id !== me);
  if (peers.length > 0) {
    const codes = await neosh.cmd.call<Record<string, string> | null>("swarm.codes").catch(() => null);
    const colors = await neosh.cmd.call<Record<string, string> | null>("swarm.colors").catch(() => null);
    out.push({
      id: "computers",
      title: "Computers",
      rows: peers.map((n) => ({
        kind: "machine",
        id: n.info.id,
        name: n.info.name,
        code: codes?.[n.info.id] ?? "",
        color: colors?.[n.info.id] ?? "Swarm.Up",
        up: n.link.state === "up",
        state: n.link.state === "up"
          ? "connected"
          : n.link.state === "waiting"
          ? "has not allowed this computer yet"
          : n.link.state === "down"
          ? "not connected"
          : "connecting",
      })),
      note: "↵ changes the code a computer is drawn with — the @ms on its rows. ^J adds, renames and removes computers.",
    });
  }

  // Everything else, by namespace. Every option appears in exactly one of these whether or not a
  // page also lists it: a page is a shortcut, and the section is where the option *lives*.
  const byPrefix = new Map<string, string[]>();
  for (const name of [...entries.keys()].sort()) {
    // A name with no namespace — Vim's own `mapleader`, `timeoutlen` — is the editor's, and a
    // section each would be a rail of one-row pages.
    const prefix = name.includes(".") ? name.split(".")[0] ?? name : "editor";
    const list = byPrefix.get(prefix) ?? [];
    list.push(name);
    byPrefix.set(prefix, list);
  }
  const prefixes = [...byPrefix.keys()].sort((a, b) => {
    const ra = ORDER.indexOf(a);
    const rb = ORDER.indexOf(b);
    if (ra !== rb) return (ra < 0 ? ORDER.length : ra) - (rb < 0 ? ORDER.length : rb);
    return a.localeCompare(b);
  });
  for (const prefix of prefixes) {
    out.push({
      id: `ns:${prefix}`,
      title: TITLES[prefix] ?? capitalise(prefix),
      rows: (byPrefix.get(prefix) ?? []).map((name) => ({
        kind: "option",
        name,
        label: humanize(name, !name.includes(".")),
      })),
    });
  }

  // The keys, read out of the live keymap — so a rebinding in `init.ts` is what this says.
  const keys = await neosh.keymap.list("chat").catch(() => [] as KeymapEntry[]);
  const global = keys
    .filter((k) => k.scope.kind === "global" && k.desc)
    .sort((a, b) => keyOrder(a.lhs) - keyOrder(b.lhs) || a.lhs.localeCompare(b.lhs));
  if (global.length > 0) {
    out.push({
      id: "keys",
      title: "Keys",
      rows: global.map((k) => ({ kind: "key", lhs: k.lhs, command: k.command, desc: k.desc ?? "" })),
      note: "↵ runs it. Rebind any of these with keymap.set in init.ts — ^Z lists every scope.",
    });
  }
  return out;
}

/** Chords first, then the rest: the composer's keys are the ones worth reading down. */
function keyOrder(lhs: string): number {
  if (/^<C-/.test(lhs)) return 0;
  if (/^<[SA]-/.test(lhs)) return 1;
  return 2;
}

/**
 * An option's name as a label.
 *
 * In its own namespace's section the namespace is the heading, so `git.fetch.interval` is
 * *Fetch interval*; on a page it may be anybody's, so the namespace stays on.
 */
function humanize(name: string, whole: boolean): string {
  const parts = name.split(".");
  const rest = whole || parts.length === 1 ? parts : parts.slice(1);
  return capitalise(rest.join(" ").replace(/_/g, " "));
}

function capitalise(s: string): string {
  return s === "" ? s : s[0]!.toUpperCase() + s.slice(1);
}

/** One row's text, and the runs in it to colour. */
interface Painted {
  text: string;
  spans: Array<[number, number, string]>;
}

/** The control for an option: what it is set to, drawn by what it can be set to. */
function control(e: OptionEntry, room: number, focused: boolean): Painted {
  const spans: Array<[number, number, string]> = [];
  let text = "";
  const push = (s: string, hl?: string) => {
    const from = byteLength(text);
    text += s;
    if (hl) spans.push([from, byteLength(text), hl]);
  };
  const rail = (live: boolean) => (focused && live ? "Option.RailLive" : "Option.Rail");
  const ladder = (values: string[], at: number) => {
    push("‹", rail(at > 0));
    values.forEach((v, i) => {
      const lit = i === at;
      push(` ${v} `, lit ? (focused ? "Option.Cursor" : "Settings.Chosen") : "Option.Unset");
    });
    push("›", rail(at < values.length - 1));
  };
  switch (e.type.type) {
    case "bool": {
      ladder(["off", "on"], e.value === true ? 1 : 0);
      break;
    }
    case "enum": {
      const values = e.type.values;
      const at = Math.max(0, values.indexOf(String(e.value)));
      const all = values.reduce((n, v) => n + width(v) + 2, 2);
      if (all <= room) ladder(values, at);
      else {
        // Too many to lay out: the one in effect, and where it is among them.
        push("‹", rail(at > 0));
        push(` ${values[at] ?? ""} `, focused ? "Option.Cursor" : "Settings.Chosen");
        push("›", rail(at < values.length - 1));
        push(`  ${at + 1}/${values.length}`, "Comment");
      }
      break;
    }
    case "int":
    case "float": {
      const n = Number(e.value);
      const t = e.type;
      const min = t.type === "int" ? t.min ?? -Infinity : -Infinity;
      const max = t.type === "int" ? t.max ?? Infinity : Infinity;
      push("‹", rail(n > min));
      push(` ${String(e.value)} `, focused ? "Option.Cursor" : "Settings.Chosen");
      push("›", rail(n < max));
      break;
    }
    case "str": {
      const v = String(e.value ?? "");
      if (v === "") push("(empty)", "Comment");
      else {
        const first = v.split("\n")[0] ?? "";
        const more = v.includes("\n") ? ` +${v.split("\n").length - 1} lines` : "";
        const fit = Math.max(4, room - width(more));
        push(width(first) > fit ? `${clipToWidth(first, fit - 1)}…` : first, "Settings.Text");
        if (more) push(more, "Comment");
      }
      break;
    }
    case "list": {
      const items = Array.isArray(e.value) ? e.value.map(String) : [];
      push(items.length === 0 ? "(none)" : clipToWidth(items.join(", "), room), items.length === 0 ? "Comment" : "Settings.Text");
      break;
    }
    default: {
      push(clipToWidth(JSON.stringify(e.value), room), "Settings.Text");
    }
  }
  return { text, spans };
}

/** Whether an option is edited by typing rather than by moving along it. */
function typed(e: OptionEntry): boolean {
  return !["bool", "enum"].includes(e.type.type);
}

async function draw(neosh: Neosh, s: Sheet): Promise<void> {
  if (sheet !== s) return;
  const W = Math.max(40, s.cols);
  const H = Math.max(8, s.rows);
  const section = s.sections[s.at];
  const rows = section?.rows ?? [];
  const cursor = Math.min(rows.length - 1, Math.max(0, s.cursors.get(section?.id ?? "") ?? 0));

  const railW = Math.min(22, Math.max(12, ...s.sections.map((x) => width(x.title) + 4)));
  const listW = W - railW - 1;
  // The foot: a rule, two lines about the row under the cursor, and one saying what it is.
  const bodyH = Math.max(1, H - 4);
  if (cursor < s.top) s.top = cursor;
  if (cursor >= s.top + bodyH) s.top = cursor - bodyH + 1;
  s.top = Math.max(0, Math.min(s.top, Math.max(0, rows.length - bodyH)));

  const labelW = Math.min(
    Math.floor(listW * 0.5),
    Math.max(8, ...rows.map((r) => width(r.kind === "option" ? r.label : r.kind === "key" ? prettyKey(r.lhs) : r.name))),
  );

  const lines: string[] = [];
  const marks: Array<[number, number, number, string]> = [];
  for (let i = 0; i < bodyH; i++) {
    // The rail.
    const sec = s.sections[i];
    let line = "";
    if (sec) {
      const here = i === s.at;
      const text = padToWidth(`${here ? " ▌ " : "   "}${sec.title}`, railW);
      line = text;
      if (here) {
        marks.push([i, 0, byteLength(" ▌"), "Settings.Bar"]);
        marks.push([i, byteLength(" ▌ "), byteLength(text), "Settings.Section"]);
      } else {
        marks.push([i, 0, byteLength(text), "Settings.Rail"]);
      }
    } else {
      line = " ".repeat(railW);
    }
    const at = byteLength(line);
    line += "│";
    marks.push([i, at, byteLength(line), "Separator"]);

    // The list.
    const index = s.top + i;
    const row = rows[index];
    if (!row) {
      if (i === 0 && rows.length === 0) {
        const from = byteLength(line);
        line += "  nothing here";
        marks.push([i, from, byteLength(line), "Comment"]);
      }
      lines.push(line);
      continue;
    }
    const focused = index === cursor;
    const lead = byteLength(line);
    const entry = row.kind === "option" ? s.entries.get(row.name) : undefined;
    const modified = entry?.modified ?? false;
    line += focused ? " ❯" : "  ";
    marks.push([i, lead, byteLength(line), "Settings.Pointer"]);
    const dotAt = byteLength(line);
    line += modified ? "•" : " ";
    if (modified) marks.push([i, dotAt, byteLength(line), "Settings.Modified"]);
    line += " ";
    const labelText = row.kind === "option"
      ? row.label
      : row.kind === "key"
      ? prettyKey(row.lhs)
      : row.name;
    const labelAt = byteLength(line);
    line += padToWidth(clipToWidth(labelText, labelW), labelW);
    marks.push([
      i,
      labelAt,
      byteLength(line),
      row.kind === "key" ? "Settings.Key" : focused ? "Settings.LabelFocused" : "Settings.Label",
    ]);
    line += "  ";
    const room = Math.max(4, listW - 4 - labelW - 2 - 1);
    const base = byteLength(line);
    if (row.kind === "option" && entry) {
      const painted = control(entry, room, focused);
      line += painted.text;
      for (const [from, to, hl] of painted.spans) marks.push([i, base + from, base + to, hl]);
      // On the row the keys are on, what `↵` would do to a value you type rather than slide.
      if (focused && typed(entry) && width(painted.text) + 8 <= room) {
        const from = byteLength(line);
        line += "  ↵ edit";
        marks.push([i, from, byteLength(line), "Comment"]);
      }
    } else if (row.kind === "key") {
      const text = clipToWidth(row.desc, room);
      line += text;
      marks.push([i, base, byteLength(line), "Comment"]);
    } else if (row.kind === "machine") {
      const code = `@${row.code}`;
      line += code;
      marks.push([i, base, byteLength(line), row.up ? row.color : "Swarm.Down"]);
      const from = byteLength(line);
      line += `  ${clipToWidth(row.state, Math.max(0, room - width(code) - 2))}`;
      marks.push([i, from, byteLength(line), "Comment"]);
    }
    lines.push(line);
  }

  // The rule, joined to the rail's edge.
  const rule = "─".repeat(railW) + "┴" + "─".repeat(Math.max(0, listW));
  lines.push(rule);
  marks.push([lines.length - 1, 0, byteLength(rule), "Separator"]);

  // What the row under the cursor is, in two lines.
  const row = rows[cursor];
  const about: Array<[string, string]> = [];
  if (row?.kind === "option") {
    const e = s.entries.get(row.name);
    const said = wrapToWidth(e?.description ?? "", W - 3);
    const meta = [
      row.name,
      e ? `default ${show(e.default)}` : "",
      e?.modified ? "changed — r puts the default back" : "",
      e && e.owner !== "neosh" ? `from ${e.owner}` : "",
    ].filter(Boolean).join("  ·  ");
    about.push([said[0] ?? "", "Settings.Help"]);
    // A third line of description is rarer than it is long: the second ends in `…` rather than the
    // panel growing a row every time the cursor lands on a chatty option.
    about.push([said.length > 2 ? `${clipToWidth(said[1]!, W - 5)} …` : said[1] ?? "", "Settings.Help"]);
    about.push([meta, "Comment"]);
  } else {
    const note = wrapToWidth(section?.note ?? "", W - 3);
    about.push([note[0] ?? "", "Settings.Help"]);
    about.push([note[1] ?? "", "Settings.Help"]);
    about.push(["", "Comment"]);
  }
  for (const [text, hl] of about) {
    const line = ` ${clipToWidth(text, W - 2)}`;
    lines.push(line);
    marks.push([lines.length - 1, 1, byteLength(line), hl]);
  }

  await neosh.buf.setLines(s.buf, 0, -1, lines);
  await neosh.ns.clear(s.ns, s.buf);
  for (const [r, from, to, hl] of marks) {
    if (to > from) await neosh.ns.mark(s.ns, s.buf, r, from, { hlGroup: hl, endCol: to });
  }
  await neosh.win.setCursor(s.win, Math.max(0, cursor - s.top), 0).catch(() => {});
}

function show(v: OptionValue): string {
  if (typeof v === "string") return v === "" ? "empty" : clipToWidth(v.split("\n")[0] ?? "", 24);
  if (Array.isArray(v)) return v.length === 0 ? "none" : v.join(", ");
  if (typeof v === "object" && v !== null) return "{…}";
  return String(v);
}

// ---------------------------------------------------------------------------
// Opening and closing
// ---------------------------------------------------------------------------

async function loadEntries(neosh: Neosh): Promise<Map<string, OptionEntry>> {
  const all = await neosh.opt.all().catch(() => [] as OptionEntry[]);
  return new Map(all.map((e) => [e.name, e]));
}

/** Read everything again — options, pages, machines, keys — keeping which section is showing. */
async function rebuild(neosh: Neosh, s: Sheet): Promise<void> {
  const id = s.sections[s.at]?.id;
  s.entries = await loadEntries(neosh);
  const found = s.sections.find((x) => x.id === "found");
  s.sections = await sections(neosh, s.entries);
  if (found) s.sections.unshift(found);
  s.at = Math.max(0, s.sections.findIndex((x) => x.id === id));
  await draw(neosh, s);
}

/**
 * Open the panel. `on` names a section to start on — its title, `computers`, `keys` — or, failing
 * that, is taken as something to search for, so `/settings git` and `settings.open prompt` both land
 * somewhere useful.
 */
async function open(neosh: Neosh, on?: string): Promise<void> {
  if (sheet) {
    await sheet.close();
    return;
  }
  const entries = await loadEntries(neosh);
  const all = await sections(neosh, entries);
  const buf = await neosh.buf.create({ name: "[settings]", scratch: true, kind: KIND });
  const ns = await neosh.ns.create(NS);
  const config: FloatOptions = {
    anchor: { kind: "screen" },
    width: { kind: "max", n: 100 },
    height: { kind: "max", n: 26 },
    border: "rounded",
    borderHl: "Settings.Border",
    title: " Settings ",
    footer: " ⇥ section   j k move   h l change   ↵ edit   r default   / find   esc close ",
    focusable: true,
    // Not on blur: editing a value opens a field over this one, and a panel that shut itself the
    // moment its own field took the keyboard would lose your place on every edit.
    closeOnBlur: false,
    modal: true,
    z: 190,
  };
  const win = await neosh.float.open(buf, config);
  await neosh.focus.push(win);
  const s: Sheet = {
    win,
    buf,
    ns,
    sections: all,
    at: 0,
    cursors: new Map(),
    top: 0,
    cols: 100,
    rows: 26,
    entries,
    warned: false,
    close: async () => {
      if (sheet !== s) return;
      sheet = null;
      await neosh.focus.pop().catch(() => {});
      await neosh.win.close(win).catch(() => {});
    },
  };
  sheet = s;
  const wanted = on?.trim().toLowerCase();
  if (wanted) {
    const i = s.sections.findIndex((x) => x.title.toLowerCase() === wanted || x.id.endsWith(`:${wanted}`) || x.id === wanted);
    if (i >= 0) s.at = i;
    else search(s, wanted);
  }
  const v = await neosh.win.viewport(win).catch(() => null);
  if (v) {
    s.cols = v.width;
    s.rows = v.height;
  }
  await draw(neosh, s);
}

/** Every option matching all of the words, as a section of its own at the top. */
function search(s: Sheet, query: string): void {
  s.sections = s.sections.filter((x) => x.id !== "found");
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) {
    s.at = 0;
    return;
  }
  const seen = new Set<string>();
  const rows: Row[] = [];
  for (const section of s.sections) {
    for (const row of section.rows) {
      if (row.kind !== "option" || seen.has(row.name)) continue;
      const e = s.entries.get(row.name);
      const hay = `${row.name} ${row.label} ${e?.description ?? ""}`.toLowerCase();
      if (words.every((w) => hay.includes(w))) {
        seen.add(row.name);
        rows.push(row);
      }
    }
  }
  s.sections.unshift({
    id: "found",
    title: `“${clipToWidth(query, 12)}”`,
    rows,
    note: rows.length === 0 ? "Nothing matches every word. Esc, then / to search again." : undefined,
  });
  s.at = 0;
  s.cursors.set("found", 0);
  s.top = 0;
}

// ---------------------------------------------------------------------------
// Changing things
// ---------------------------------------------------------------------------

/**
 * Put a value into effect and into `config.toml`.
 *
 * The default is written as *no line at all*, so a setting moved back to where it started leaves
 * the file as it found it. Where there is no file to write — `--clean` — the value is still set for
 * this run, and that is said once rather than on every key.
 */
async function commit(neosh: Neosh, s: Sheet, name: string, value: OptionValue | undefined): Promise<void> {
  const e = s.entries.get(name);
  const back = value === undefined || (e && JSON.stringify(value) === JSON.stringify(e.default));
  try {
    await neosh.opt.save(name, back ? undefined : value);
  } catch (err) {
    try {
      if (back) await neosh.opt.reset(name);
      else await neosh.opt.set(name, value as OptionValue);
      if (!s.warned) {
        s.warned = true;
        neosh.notify(`set for this run only — ${String(err).replace(/^Error:\s*/, "")}`, "warn");
      }
    } catch (again) {
      neosh.notify(String(again).replace(/^Error:\s*/, ""), "warn");
    }
  }
  const fresh = await neosh.opt.entry(name).catch(() => null);
  if (fresh) s.entries.set(name, fresh);
}

/** The row the keys are on, and the option behind it when it is one. */
function current(s: Sheet): { row: Row | undefined; entry: OptionEntry | undefined } {
  const section = s.sections[s.at];
  const row = section?.rows[s.cursors.get(section.id) ?? 0];
  return { row, entry: row?.kind === "option" ? s.entries.get(row.name) : undefined };
}

/** Along the row: the next value, the previous one, a nudge of a number. */
async function shift(neosh: Neosh, s: Sheet, delta: number, wrap: boolean): Promise<void> {
  const { entry } = current(s);
  if (!entry) return;
  const t = entry.type;
  if (t.type === "bool") {
    const next = wrap ? !entry.value : delta > 0;
    if (next !== entry.value) await commit(neosh, s, entry.name, next);
  } else if (t.type === "enum") {
    const n = t.values.length;
    const at = Math.max(0, t.values.indexOf(String(entry.value)));
    const to = wrap ? (at + delta + n) % n : Math.min(n - 1, Math.max(0, at + delta));
    if (to !== at) await commit(neosh, s, entry.name, t.values[to]!);
  } else if (t.type === "int") {
    const v = Number(entry.value);
    // A step the size of the number: one column of a sidebar, ten seconds of a fetch interval.
    const step = Math.abs(v) >= 1000 ? 100 : Math.abs(v) >= 100 ? 10 : 1;
    const next = Math.min(t.max ?? Infinity, Math.max(t.min ?? -Infinity, v + delta * step));
    if (next !== v) await commit(neosh, s, entry.name, next);
  } else if (t.type === "float") {
    const v = Number(entry.value);
    const next = Math.round((v + delta * 0.1) * 100) / 100;
    await commit(neosh, s, entry.name, next);
  }
  await draw(neosh, s);
}

/** `↵`: whatever the row does — toggle, cycle, edit, run, rename. */
async function activateRow(neosh: Neosh, s: Sheet): Promise<void> {
  const { row, entry } = current(s);
  if (!row) return;
  if (row.kind === "key") {
    await s.close();
    await neosh.cmd.exec(row.command).catch((e: unknown) => neosh.notify(String(e), "warn"));
    return;
  }
  if (row.kind === "machine") {
    const changed = await neosh.cmd.call<boolean>("swarm.code", [row.id]).catch(() => false);
    if (changed && sheet === s) await rebuild(neosh, s);
    return;
  }
  if (!entry) return;
  const t = entry.type;
  if (t.type === "bool" || t.type === "enum") {
    await shift(neosh, s, 1, true);
    return;
  }
  const label = row.label;
  let value: OptionValue | undefined;
  if (t.type === "str" && (PROSE.has(entry.name) || String(entry.value).includes("\n"))) {
    const text = await editText(neosh, ` ${label} `, String(entry.value ?? ""));
    if (text === null) return;
    value = text;
  } else {
    const initial = t.type === "list"
      ? (Array.isArray(entry.value) ? entry.value.join(", ") : "")
      : t.type === "json"
      ? JSON.stringify(entry.value)
      : String(entry.value ?? "");
    const hint = t.type === "list" ? " — separate with commas" : t.type === "json" ? " — as JSON" : "";
    const answer = await prompt(neosh, `${label}${hint}`, { initial, width: Math.min(90, Math.max(50, s.cols - 10)) });
    if (answer === null) return;
    const parsed = parse(entry, answer);
    if (parsed.error) {
      neosh.notify(parsed.error, "warn");
      return;
    }
    value = parsed.value;
  }
  if (sheet !== s) return;
  await commit(neosh, s, entry.name, value);
  await draw(neosh, s);
}

/** What was typed, as the option's type — or why it is not one. */
function parse(e: OptionEntry, text: string): { value?: OptionValue; error?: string } {
  const t = e.type;
  const trimmed = text.trim();
  switch (t.type) {
    case "int": {
      if (!/^-?\d+$/.test(trimmed)) return { error: `${e.name} is a whole number` };
      const n = Number(trimmed);
      if (t.min != null && n < t.min) return { error: `${e.name} is at least ${t.min}` };
      if (t.max != null && n > t.max) return { error: `${e.name} is at most ${t.max}` };
      return { value: n };
    }
    case "float": {
      const n = Number(trimmed);
      return Number.isFinite(n) && trimmed !== "" ? { value: n } : { error: `${e.name} is a number` };
    }
    case "list":
      return { value: trimmed === "" ? [] : trimmed.split(",").map((x) => x.trim()).filter(Boolean) };
    case "json": {
      try {
        const v = JSON.parse(trimmed === "" ? "{}" : trimmed);
        if (typeof v !== "object" || v === null || Array.isArray(v)) {
          return { error: `${e.name} is a JSON object — {…}` };
        }
        return { value: v as { [key: string]: unknown } };
      } catch (err) {
        return { error: `not JSON: ${String(err).replace(/^SyntaxError:\s*/, "")}` };
      }
    }
    default:
      return { value: text };
  }
}

// ---------------------------------------------------------------------------
// A field that takes more than one line
// ---------------------------------------------------------------------------

/**
 * Edit prose: a float that is a text field, several lines tall.
 *
 * The text is a real buffer edited through the same verbs the composer is — `edit.apply`,
 * `edit.move` — so a word is a word here exactly as it is there, and the grapheme and width rules
 * live in one place. `⏎` is a new line, because what goes in here is instructions and instructions
 * have paragraphs; `esc` keeps what you wrote, since losing a paragraph to the key people press to
 * leave is the worst thing this field could do; `^C` is the way out that throws it away. A paste
 * arrives whole, newlines and all.
 */
async function editText(neosh: Neosh, title: string, initial: string): Promise<string | null> {
  const buf = await neosh.buf.create({ name: "[settings text]", scratch: true, kind: KIND_TEXT });
  await neosh.buf.setLines(buf, 0, -1, initial.split("\n"));
  const lines = Math.min(14, Math.max(6, initial.split("\n").length + 2));
  const win = await neosh.float.open(buf, {
    anchor: { kind: "screen" },
    // Fixed rather than `max`, which fits the text: a field as wide as what is already in it is a
    // field with nowhere to type.
    width: { kind: "fixed", n: 76 },
    height: { kind: "fixed", n: lines },
    border: "rounded",
    borderHl: "Settings.Border",
    title,
    footer: " ⏎ new line   esc save   ^C discard ",
    focusable: true,
    closeOnBlur: false,
    modal: true,
    z: 220,
  });
  await neosh.edit.cursorShape(win, "bar").catch(() => {});
  await neosh.edit.move(win, "buf_end").catch(() => {});

  let settle: (v: string | null) => void = () => {};
  const done = new Promise<string | null>((resolve) => (settle = resolve));
  const disposers: Disposable[] = [];
  let closed = false;
  const close = async (keep: boolean) => {
    if (closed) return;
    closed = true;
    const text = keep ? (await neosh.buf.getLines(buf, 0, -1).catch(() => [] as string[])).join("\n") : null;
    for (const d of disposers) d.dispose();
    await neosh.win.close(win).catch(() => {});
    // Trailing blank lines are the Enter you pressed before `esc`, not something you meant to send.
    settle(text === null ? null : text.replace(/\s+$/, ""));
  };

  const command = `${NS}.text.key.${++textSeq}`;
  const edit = (e: Parameters<Neosh["edit"]["apply"]>[1]) => neosh.edit.apply(win, e).catch(() => {});
  const move = (m: Parameters<Neosh["edit"]["move"]>[1]) => neosh.edit.move(win, m).catch(() => {});
  disposers.push(
    await neosh.cmd.register(command, async (_args, key: KeyContext | undefined) => {
      if (!key || closed) return;
      const code = key.key.code;
      const { ctrl, alt } = key.key.mods;
      switch (code.kind) {
        case "esc":
          return close(true);
        case "enter":
          return edit({ kind: "insert", text: "\n" });
        case "tab":
          return edit({ kind: "insert", text: "  " });
        case "backspace":
          return edit({ kind: ctrl || alt ? "delete_word_back" : "delete_back" });
        case "delete":
          return edit({ kind: ctrl || alt ? "delete_word_forward" : "delete_forward" });
        case "left":
          return move(ctrl || alt ? "word_left" : "left");
        case "right":
          return move(ctrl || alt ? "word_right" : "right");
        case "up":
          return move("up");
        case "down":
          return move("down");
        case "home":
          return move(ctrl ? "buf_start" : "line_start");
        case "end":
          return move(ctrl ? "buf_end" : "line_end");
        case "char": {
          if (ctrl) {
            switch (code.c.toLowerCase()) {
              case "c":
                return close(false);
              case "s":
                return close(true);
              case "a":
                return move("line_start");
              case "e":
                return move("line_end");
              case "u":
                return edit({ kind: "delete_to_line_start" });
              case "k":
                return edit({ kind: "delete_to_line_end" });
              case "h":
                return edit({ kind: "delete_back" });
              default:
                return;
            }
          }
          if (alt) return;
          return edit({ kind: "insert", text: code.c });
        }
        default:
          return;
      }
    }, { desc: "settings text key" }),
  );
  disposers.push(onPaste(neosh, win, (text) => {
    if (!closed) void edit({ kind: "insert", text: text.replace(/\r\n?/g, "\n") });
  }));
  await neosh.focus.push(win);
  disposers.push(await neosh.keymap.capture(win, command));
  // Every key this field reads, bound on its window so nothing nearer takes one first.
  const keys = [
    "<CR>", "<BS>", "<Del>", "<Tab>", "<Esc>", "<Left>", "<Right>", "<Up>", "<Down>", "<Home>",
    "<End>", "<C-Left>", "<C-Right>", "<C-Home>", "<C-End>", "<C-BS>", "<C-c>", "<C-s>", "<C-a>",
    "<C-e>", "<C-u>", "<C-k>", "<C-h>",
  ];
  for (const mode of ["chat", "normal", "insert", "visual"] as const) {
    for (const lhs of keys) {
      await neosh.keymap.set(mode, lhs, command, { scope: { kind: "window", win } }).catch(() => {});
    }
  }
  return done;
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/** Every key in the panel, as an ordinary binding on its kind — `^Z` lists them, `init.ts` moves them. */
async function installKeys(neosh: Neosh, subscriptions: Disposable[]): Promise<void> {
  const scope = { kind: "buf_kind", name: KIND } as const;
  const verb = async (
    name: string,
    keys: string[],
    desc: string,
    fn: (s: Sheet) => Promise<void> | void,
  ): Promise<void> => {
    subscriptions.push(
      await neosh.cmd.register(name, () => {
        const mine = queue.then(async () => {
          if (!sheet) return;
          await fn(sheet);
        });
        queue = mine.catch((e: unknown) => neosh.notify(String(e), "warn"));
        return mine;
      }, { desc }),
    );
    for (const mode of ["chat", "normal"] as const) {
      for (const key of keys) await neosh.keymap.set(mode, key, name, { scope, desc });
    }
  };

  const rowsOf = (s: Sheet) => s.sections[s.at]?.rows ?? [];
  const moveBy = async (s: Sheet, delta: number) => {
    const id = s.sections[s.at]?.id ?? "";
    const n = rowsOf(s).length;
    if (n === 0) return;
    const at = s.cursors.get(id) ?? 0;
    s.cursors.set(id, Math.min(n - 1, Math.max(0, at + delta)));
    await draw(neosh, s);
  };
  const section = async (s: Sheet, delta: number) => {
    const n = s.sections.length;
    if (n === 0) return;
    s.at = (s.at + delta + n) % n;
    s.top = 0;
    await draw(neosh, s);
  };

  await verb(`${NS}.down`, ["j", "<Down>", "<C-n>"], "Next setting", (s) => moveBy(s, 1));
  await verb(`${NS}.up`, ["k", "<Up>", "<C-p>"], "Previous setting", (s) => moveBy(s, -1));
  const half = (s: Sheet) => Math.max(1, Math.floor((s.rows - 4) / 2));
  await verb(`${NS}.half.down`, ["<C-d>", "<PageDown>"], "Half a screen down", (s) => moveBy(s, half(s)));
  await verb(`${NS}.half.up`, ["<C-u>", "<PageUp>"], "Half a screen up", (s) => moveBy(s, -half(s)));
  await verb(`${NS}.first`, ["gg", "<Home>"], "The first setting", (s) => moveBy(s, -9999));
  await verb(`${NS}.last`, ["G", "<End>"], "The last setting", (s) => moveBy(s, 9999));
  await verb(`${NS}.section.next`, ["<Tab>", "]", "J"], "Next section", (s) => section(s, 1));
  await verb(`${NS}.section.prev`, ["<S-Tab>", "[", "K"], "Previous section", (s) => section(s, -1));
  await verb(`${NS}.more`, ["l", "<Right>"], "The next value, or more", (s) => shift(neosh, s, 1, false));
  await verb(`${NS}.less`, ["h", "<Left>"], "The previous value, or less", (s) => shift(neosh, s, -1, false));
  await verb(`${NS}.cycle`, ["<Space>"], "The next value along", (s) => shift(neosh, s, 1, true));
  await verb(`${NS}.activate`, ["<CR>"], "Change this setting", (s) => activateRow(neosh, s));
  await verb(`${NS}.reset`, ["r"], "Put this setting back to its default", async (s) => {
    const { entry } = current(s);
    if (!entry || !entry.modified) return;
    await commit(neosh, s, entry.name, undefined);
    await draw(neosh, s);
  });
  await verb(`${NS}.copy`, ["y"], "Copy this setting's name", async (s) => {
    const { row } = current(s);
    const text = row?.kind === "option" ? row.name : row?.kind === "key" ? row.command : row?.kind === "machine" ? row.id : null;
    if (!text) return;
    await neosh.edit.copy(text);
    neosh.notify(`copied ${text}`);
  });
  await verb(`${NS}.find`, ["/"], "Find a setting", async (s) => {
    const query = await prompt(neosh, "Find a setting — every word, in any order", { width: 60 });
    if (query === null || sheet !== s) return;
    search(s, query);
    await draw(neosh, s);
  });
  await verb(`${NS}.close`, ["<Esc>", "q", "<C-c>"], "Close settings", async (s) => {
    // A search is the first thing `esc` takes away, the panel the second — one thing per press.
    if (s.sections[0]?.id === "found") {
      s.sections.shift();
      s.at = Math.max(0, s.at - 1);
      if (s.at === 0) s.at = 0;
      await draw(neosh, s);
      return;
    }
    await s.close();
  });
}

/**
 * The panel's colours, as links to what the theme already has — so a theme that never heard of
 * this panel still draws it in its own colours, and `default: true` so `init.ts` wins.
 */
async function defineHighlights(neosh: Neosh): Promise<void> {
  const links: Array<[string, string]> = [
    ["Settings.Border", "Float.Border"],
    ["Settings.Rail", "Comment"],
    ["Settings.Section", "Title"],
    ["Settings.Bar", "Accent"],
    ["Settings.Pointer", "Accent"],
    ["Settings.Label", "Normal"],
    ["Settings.LabelFocused", "Title"],
    ["Settings.Modified", "Accent"],
    ["Settings.Chosen", "Option.Step5"],
    ["Settings.Text", "Syntax.String"],
    ["Settings.Key", "Key"],
    ["Settings.Help", "Picker.Detail"],
  ];
  for (const [name, to] of links) {
    await neosh.hl.define(name, { link: to }, { default: true }).catch(() => {});
  }
}
