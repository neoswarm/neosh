/**
 * The archive: what you have finished with, and how you finally get rid of it.
 *
 * The workspace has two verbs — `x` puts a conversation away, `X` destroys it — and the archive
 * was taken out of the sidebar, because a list of things you are deliberately done with
 * is the one part of that column that is never the answer. Both decisions then said the same thing
 * about the other end: nothing sweeps it, an archive is a place things accumulate, and deleting
 * somebody's history on a timer is a move this workspace has never been willing to make.
 *
 * That is still true and this plugin does not change it. What it changes is the half those
 * decisions left as a note: **emptying it by hand was one conversation at a time**. A picker with `^X` on a
 * row is a fine way to throw away the thing you just regretted and a terrible way to deal with two
 * hundred of them, which is what an archive of a workspace you have used for a year actually is.
 *
 * So the archive is a panel rather than a picker, and everything a panel is owed it has:
 *
 * - **Every key is an ordinary binding** on the buffer kind `neosh.archive`, pointed at a command
 *   with a name. `^Z` lists them, `^K` runs them, one line in `init.ts` moves any of them. There is
 *   no private switch on `KeyContext` here — the picker's `ownKeys`/`onKey` pair, which is what
 *   this replaced, is exactly the shape the surface rule exists to stop.
 * - **`archive.action` is a contribution point.** Say a key, a label and a command, and it is a
 *   verb on these rows — invoked with the marked conversations, or with the row under the cursor
 *   when nothing is marked, so a plugin never has to track the cursor itself.
 * - **The rows are a list you move in**: `j`/`k` take a count, `^D`/`^U` are half of
 *   the panel's real height, `12G` is a row, and a half-typed count is on screen.
 *
 * And it can be marked. `<Space>` ticks a row, `a` ticks everything showing, and every verb here —
 * put back, delete, copy — acts on the ticked set or, when nothing is ticked, on the row under the
 * cursor. `^X` empties the archive outright: everything *currently listed*, which is the filter's
 * whole point, so narrowing to one project and pressing it deletes that project's finished
 * conversations and nothing else. It asks, it says how many and how much is in them, and the
 * question is charged for the one reason that matters — it cannot be undone.
 *
 * Two things here are about conversations you have never seen. `RESTORE_LIMIT` means a workspace
 * loads the most recent few hundred conversations and leaves the rest on disk, in no list at all —
 * so an archive built on `session.list` would have reported a number that was not the number and
 * emptied itself down to a directory that was still full. It reads `session.stored` as well, which
 * is the directory, and the host brings one in from disk the moment any verb names it.
 *
 * Nothing here deletes on a timer. `archive.auto_days` will *archive* an idle conversation, because
 * archiving is reversible and free and that is the test that matters; `archive.retention_days` only
 * ever says how much is old, and `archive.sweep` is the key you press when you agree. A reminder is
 * not a policy.
 *
 * It is a plugin. `plugins.disabled = ["archive"]` turns it off, and an archive of your own that
 * loads afterwards and binds `^F` wins.
 */

import { byteLength } from "@neosh/api";
import type {
  BufferId,
  Disposable,
  Message,
  Neosh,
  PluginContext,
  SessionInfo,
  WindowId,
} from "@neosh/api";
import { confirmDestructive, CursoredList, type ListRow, prompt } from "@neosh/api/ui";

const NS = "neosh.archive";
/** What this panel's buffer says it is. Everything a third party binds or finds hangs off this. */
const KIND = "neosh.archive";

/**
 * A verb on these rows, contributed by somebody else.
 *
 * The same shape `sidebar.action` takes, and for the same reason: a key press carries no arguments
 * of its own, so the command is invoked with the conversations it is about — every marked one, or
 * the row under the cursor when nothing is marked. A plugin that had to track the cursor separately
 * would be one race away from deleting the wrong thing.
 */
const POINT_ACTION = "archive.action";

interface ActionItem {
  /** Key notation, as `keymap.set` takes it: `d`, `<C-y>`, `gd`. */
  key: string;
  /** What it does, for the hint strip and for `^Z`. */
  label: string;
  command: string;
}

/** What a row points at, so the cursor survives the list being rebuilt underneath it. */
type Target = { kind: "session"; id: string };

/** How the list is ordered. The default is when you put it away, which is how you remember it. */
type Sort = "archived" | "used" | "name" | "project" | "size";
/** What the headings are, if there are any. */
type Group = "none" | "project" | "age";

const SORTS: Sort[] = ["archived", "used", "name", "project", "size"];
const GROUPS: Group[] = ["project", "age", "none"];

const SORT_LABEL: Record<Sort, string> = {
  archived: "put away",
  used: "last used",
  name: "name",
  project: "project",
  size: "size",
};
const GROUP_LABEL: Record<Group, string> = {
  none: "flat",
  project: "by project",
  age: "by age",
};

export async function activate({ neosh, subscriptions }: PluginContext) {
  await declareOptions(neosh);

  const panel = new Panel(neosh);
  subscriptions.push({ dispose: () => void panel.close() });

  await registerCommands(neosh, subscriptions, panel);
  await bindKeys(neosh, subscriptions, panel);
  await contributeToSidebar(neosh, subscriptions);

  // Housekeeping, once the workspace has settled rather than in the middle of starting it: the
  // first thing a new terminal does is draw a transcript, and a notice that lands before there is
  // anywhere to put it is a notice nobody sees. Then daily, because both of the questions it asks
  // are about days.
  //
  // Not held in `subscriptions` for its own sake — the runtime cancels a plugin's timers when it
  // unloads — but the count row it refreshes is, and that is what the sidebar reads.
  neosh.timer.after(4000, () => void housekeeping(neosh));
  subscriptions.push(neosh.timer.every(6 * 60 * 60 * 1000, () => void housekeeping(neosh)));

  // The count in the sidebar follows the conversations rather than whoever happened to change one.
  //
  // Both halves are needed. `session.onChange` is *switching*, and archiving something you are not
  // looking at does not switch anything — so the door would appear a redraw late, or not at all.
  // The poll is the same period the panel beside it already redraws on, and it costs one `list`
  // call: the expensive half of the count, what is on disk and not loaded, cannot change without
  // this process being told. Nothing is contributed unless the number moved, because a contribution
  // is a broadcast and re-announcing an unchanged row would redraw every panel reading this point.
  const period = (await neosh.opt.get<number>("sidebar.refresh_ms").catch(() => 4000)) ?? 4000;
  subscriptions.push(neosh.session.onChange(() => void refreshSidebarRow(neosh)));
  subscriptions.push(neosh.timer.every(period, () => void refreshSidebarRow(neosh)));
  subscriptions.push(neosh.opt.onChange((e) => {
    if (e.name === "archive.sidebar") void refreshSidebarRow(neosh);
  }));
  await refreshSidebarRow(neosh);
}

async function declareOptions(neosh: Neosh): Promise<void> {
  await neosh.opt.declare({
    name: "archive.sort",
    type: { type: "enum", values: SORTS },
    default: "archived",
    description:
      "How the archive is ordered: when you put it away, when you last used it, its name, its project, or how much is in it. `s` in the panel cycles it.",
  });
  await neosh.opt.declare({
    name: "archive.group",
    type: { type: "enum", values: GROUPS },
    default: "project",
    description:
      "What the headings in the archive are: the project each conversation came from, how long ago it was put away, or nothing at all. `S` in the panel cycles it.",
  });
  await neosh.opt.declare({
    name: "archive.width",
    type: { type: "int", min: 40, max: 200 },
    default: 92,
    description: "How wide the archive panel is, at most. It is clamped to the terminal.",
  });
  await neosh.opt.declare({
    name: "archive.height",
    type: { type: "int", min: 8, max: 60 },
    default: 20,
    description: "How many rows of archive to show at once, at most.",
  });
  await neosh.opt.declare({
    name: "archive.sidebar",
    type: { type: "bool" },
    default: false,
    description:
      "Put a row in the project panel saying how many conversations are archived. Off: the archive is a popup you open with `^F`, and a permanent line about what you have finished with is a line that column cannot spare.",
  });
  await neosh.opt.declare({
    name: "archive.auto_days",
    type: { type: "int", min: 0, max: 3650 },
    default: 0,
    description:
      "Archive a conversation nothing has happened in for this many days. `0` is off. Archiving keeps every message and `^F` puts one back, which is why this is allowed to happen on its own — deleting on a timer is not.",
  });
  await neosh.opt.declare({
    name: "archive.retention_days",
    type: { type: "int", min: 0, max: 3650 },
    default: 0,
    description:
      "How old an archived conversation has to be before neosh will mention it. `0` is off. Nothing is ever deleted by this — it is what `archive.sweep` and the reminder count.",
  });
  await neosh.opt.declare({
    name: "archive.remind",
    type: { type: "bool" },
    default: true,
    description:
      "Say so, once a day, when there are archived conversations older than `archive.retention_days`. Off leaves the number to the panel.",
  });
}

// ---------------------------------------------------------------------------
// What is in the archive
// ---------------------------------------------------------------------------

/** An archived conversation, and whether this workspace is actually holding it. */
interface Entry {
  info: SessionInfo;
  /**
   * In the store, rather than only on disk.
   *
   * Drawn nowhere and used for nothing but the count in the header. Every verb works either way —
   * the host brings a conversation in from its file the moment one names it — and a row that said
   * which side of a cap it happened to fall on would be an implementation detail with a glyph.
   */
  loaded: boolean;
}

/**
 * Archived conversations this workspace has never loaded, as of the last full scan.
 *
 * The count in the sidebar follows every conversation change, and reading the *directory* on every
 * one of them would be a parse of every file you own each time a turn ends. So the expensive half
 * of the answer is remembered and the cheap half is asked again: the store is authoritative about
 * everything it holds, and what it does not hold cannot change without this process being told.
 */
let cold = 0;

/**
 * Everything archived, from both places it can be.
 *
 * The store is a window onto the sessions directory and not the whole of it: past `RESTORE_LIMIT`
 * a conversation is a file this workspace has never read. `session.stored` is that directory. Where
 * the two disagree the store wins, because it is the one with the unsaved half of today in it.
 */
async function collect(neosh: Neosh): Promise<Entry[]> {
  const [live, disk] = await Promise.all([
    neosh.session.list({ includeArchived: true }).catch(() => [] as SessionInfo[]),
    neosh.session.stored().catch(() => [] as SessionInfo[]),
  ]);
  const loaded = new Set(live.map((s) => s.id));
  const byId = new Map<string, SessionInfo>();
  for (const s of disk) byId.set(s.id, s);
  for (const s of live) byId.set(s.id, s);
  const all = [...byId.values()]
    .filter((s) => s.archived)
    .map((info) => ({ info, loaded: loaded.has(info.id) }));
  cold = all.filter((e) => !e.loaded).length;
  return all;
}

/** How many are archived, without reading the disk. See {@link cold}. */
async function archivedCount(neosh: Neosh): Promise<number> {
  const live = await neosh.session
    .list({ includeArchived: true })
    .catch(() => [] as SessionInfo[]);
  return live.filter((s) => s.archived).length + cold;
}

/** Seconds since the epoch, the unit every timestamp on the wire is in. */
function now(): number {
  return Math.floor(Date.now() / 1000);
}

/** When a conversation was put away, falling back to when it was last touched. */
function putAway(s: SessionInfo): number {
  return s.archived_at ?? s.updated_at;
}

function matches(e: Entry, filter: string): boolean {
  if (filter === "") return true;
  const hay = `${e.info.label} ${e.info.project} ${e.info.cwd}`.toLowerCase();
  // Every word, in any order: `neosh login` finds the login conversation in the neosh checkout
  // without caring which of the two you remembered first.
  return filter.toLowerCase().split(/\s+/).filter((w) => w !== "").every((w) => hay.includes(w));
}

function sorted(entries: Entry[], by: Sort): Entry[] {
  const out = [...entries];
  switch (by) {
    case "used":
      out.sort((a, b) => b.info.updated_at - a.info.updated_at);
      break;
    case "name":
      out.sort((a, b) => a.info.label.localeCompare(b.info.label));
      break;
    case "project":
      out.sort((a, b) =>
        projectOf(a.info).localeCompare(projectOf(b.info)) || putAway(b.info) - putAway(a.info)
      );
      break;
    case "size":
      out.sort((a, b) => b.info.message_count - a.info.message_count);
      break;
    // Most recently put away first: the one you want back is usually the one you last regretted.
    default:
      out.sort((a, b) => putAway(b.info) - putAway(a.info));
  }
  return out;
}

function projectOf(s: SessionInfo): string {
  return s.project || basename(s.cwd) || s.cwd;
}

/**
 * Which heading a conversation goes under.
 *
 * The age buckets are deliberately coarse. The question a heading answers here is "is this from
 * this week or from another era", and a heading per day turns a list of forty into a list of
 * seventy.
 */
function headingOf(e: Entry, group: Group, at: number): string | null {
  if (group === "none") return null;
  if (group === "project") return projectOf(e.info);
  const days = (at - putAway(e.info)) / 86400;
  if (days < 1) return "Today";
  if (days < 7) return "This week";
  if (days < 31) return "This month";
  if (days < 366) return "This year";
  return "Older";
}

/**
 * The rows under their headings, each heading appearing exactly once.
 *
 * The order of the *groups* is the order the sort put their first member in, which is what makes
 * grouping and sorting compose instead of fighting: ordered by when you put things away and grouped
 * by project, the project you last finished something in is at the top, and each project's own rows
 * are still newest first. Doing it a row at a time — start a new heading whenever this row's differs
 * from the last one's — is the version that looks right on a sorted-by-project list and prints
 * `neosh`, `website`, `neosh`, `scratch`, `neosh` on every other one.
 */
function grouped(
  shown: Entry[],
  group: Group,
  at: number,
): Array<{ heading: string | null; entries: Entry[] }> {
  if (group === "none") return [{ heading: null, entries: shown }];
  const order: Array<{ heading: string; entries: Entry[] }> = [];
  const index = new Map<string, { heading: string; entries: Entry[] }>();
  for (const e of shown) {
    const heading = headingOf(e, group, at) ?? "";
    let bucket = index.get(heading);
    if (bucket === undefined) {
      bucket = { heading, entries: [] };
      index.set(heading, bucket);
      order.push(bucket);
    }
    bucket.entries.push(e);
  }
  return order;
}

// ---------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------

/**
 * The window, the rows in it, and what is ticked.
 *
 * A class rather than a closure over ten `let`s because every command registered below needs to
 * reach the same one, and a panel that is opened and closed as often as `^F` is pressed is a thing
 * whose lifetime is worth naming.
 */
class Panel {
  private win: WindowId | null = null;
  private buf: BufferId | null = null;
  private list: CursoredList<Target> | null = null;
  private entries: Entry[] = [];
  private shown: Entry[] = [];
  /** Ticked conversations, by id. Kept across a refresh; dropped when the panel closes. */
  readonly marks = new Set<string>();
  filter = "";
  /** Typed before a motion and consumed by it. On screen while it is half typed. */
  readonly count = { pending: "" };
  private width = 92;
  private height = 20;
  private ascii = false;
  private drawing = false;
  private again = false;
  private actions: ActionItem[] = [];

  constructor(private readonly neosh: Neosh) {}

  isOpen(): boolean {
    return this.win !== null;
  }

  /** The row under the cursor, whether or not it is ticked. */
  cursorEntry(): Entry | null {
    const id = this.list?.value?.id;
    return this.shown.find((e) => e.info.id === id) ?? null;
  }

  /**
   * What a verb is about: everything ticked, or the row under the cursor.
   *
   * One rule for every verb in the panel, including contributed ones. The alternative — some keys
   * take the marks and some take the cursor — is a set of keys you have to remember the arity of,
   * which is the thing marks were supposed to remove.
   */
  selection(): Entry[] {
    if (this.marks.size > 0) {
      const marked = this.shown.filter((e) => this.marks.has(e.info.id));
      // Ticked rows the filter is currently hiding still count: you ticked them, and a filter is a
      // way of finding things rather than a way of un-ticking them.
      const hidden = this.entries.filter(
        (e) => this.marks.has(e.info.id) && !this.shown.includes(e),
      );
      return [...marked, ...hidden];
    }
    const one = this.cursorEntry();
    return one ? [one] : [];
  }

  /** Everything the panel is currently listing — what `^X` means by "the archive". */
  listed(): Entry[] {
    return [...this.shown];
  }

  filtered(): boolean {
    return this.filter !== "";
  }

  /**
   * How many rows the panel needs: its chrome, its headings, its rows, and one to unfold into.
   *
   * The last one is the row under the cursor becoming two lines when its title did not fit, which
   * is a line the panel has to have somewhere to put or every long title scrolls the header away.
   */
  private fits(group: Group): number {
    const at = now();
    const headings = group === "none"
      ? 0
      : new Set(this.entries.map((e) => headingOf(e, group, at))).size;
    // header, blank, headings, rows, blank, two lines of keys, one to unfold into.
    const needed = 2 + headings + this.entries.length + 3 + 1;
    return Math.max(10, Math.min(this.height + 8, needed));
  }

  async open(): Promise<void> {
    if (this.win !== null) {
      await this.close();
      return;
    }
    const [width, height, ascii] = await Promise.all([
      this.neosh.opt.get<number>("archive.width"),
      this.neosh.opt.get<number>("archive.height"),
      this.neosh.opt.get<boolean>("ui.ascii_only"),
    ]);
    this.width = width ?? 92;
    this.height = height ?? 20;
    this.ascii = ascii ?? false;

    const group = (await this.neosh.opt.get<Group>("archive.group")) ?? "project";
    this.entries = await collect(this.neosh);
    if (this.entries.length === 0) {
      this.neosh.notify("nothing is archived — `x` on a conversation puts it here", "info");
      return;
    }

    const buf = await this.neosh.buf.create({ name: "[archive]", scratch: true, kind: KIND });
    const ns = await this.neosh.ns.create(NS);
    this.buf = buf;
    // The width the list measures continuation lines against is the *content* width: an extent
    // measures content and the frontend draws the border around it, so nothing here subtracts two.
    this.list = new CursoredList<Target>(this.neosh, buf, ns, { width: () => this.width });

    const win = await this.neosh.float.open(buf, {
      anchor: { kind: "screen" },
      // Fixed rather than `max`, which measures the *content*: a panel whose width was the length
      // of its longest title would be a different width every time you filtered it, and the column
      // its right-hand counts line up in would move with it.
      width: { kind: "fixed", n: this.width },
      // Sized to what there is to show, and never taller than `archive.height` allows.
      //
      // A float cannot be resized without being reopened, and reopening gives the window a new id
      // and drops the keyboard — so the height is decided once, here, from the rows that exist.
      // Asking for more than the content needs is not free: the foot is pinned to the bottom edge,
      // so the buffer is padded out to the window's height, and a buffer one line longer than the
      // window it is drawn in scrolls — which on a panel means the header quietly leaving the top
      // of it on the first keypress. Filtering only ever shortens the list, and `pinned` is what
      // holds the strip down when it does.
      height: { kind: "max", n: this.fits(group) },
      border: "rounded",
      title: " Archived ",
      focusable: true,
      // Not on blur: a confirmation opens over this and takes the keyboard, and a panel that shut
      // itself the moment it was asked a question would answer it into an empty screen.
      closeOnBlur: false,
      // Nothing else reaches the keyboard while this is up. It is a panel you are in the middle of
      // using, and `^N` over it opening a conversation behind it is exactly what modality exists to stop.
      // `^Q` and `^R` still resolve — see `ui.modal_escape_keys` — so this can never be a terminal
      // somebody has to kill, and `^F` closes it from inside because a modal that borrows a global
      // key to open itself owes a binding to shut itself.
      modal: true,
      z: 200,
    });
    this.win = win;
    await this.neosh.focus.push(win);
    this.actions = (await this.neosh.ext.list<ActionItem>(POINT_ACTION).catch(() => []))
      .map((c) => c.item)
      .filter((a): a is ActionItem => !!a?.key && !!a.command);
    await this.draw();
  }

  async close(): Promise<void> {
    const win = this.win;
    if (win === null) return;
    this.win = null;
    this.marks.clear();
    this.filter = "";
    this.count.pending = "";
    await this.neosh.focus.pop().catch(() => {});
    await this.neosh.win.close(win).catch(() => {});
    this.buf = null;
    this.list = null;
  }

  /** Re-read what is archived. Called after anything that changed it. */
  async refresh(): Promise<void> {
    this.entries = await collect(this.neosh);
    // A tick on a conversation that is no longer archived is a tick on nothing, and leaving it
    // would mean the next verb quietly acted on a row that is not on screen.
    const live = new Set(this.entries.map((e) => e.info.id));
    for (const id of [...this.marks]) if (!live.has(id)) this.marks.delete(id);
    await refreshSidebarRow(this.neosh);
    if (this.entries.length === 0) {
      await this.close();
      return;
    }
    await this.draw();
  }

  /** How many rows of panel there are, for the two keys that mean "a screen". */
  async rows(): Promise<number> {
    if (this.win === null) return this.height;
    const view = await this.neosh.win.viewport(this.win).catch(() => null);
    // Less the header and the pinned foot, so `^D` is half of the list rather than half of the box.
    return Math.max(2, (view?.height ?? this.height) - 4);
  }

  move(delta: number, opts: { wrap?: boolean } = {}): void {
    this.list?.move(delta, opts);
  }

  toEnd(which: "first" | "last"): void {
    this.list?.toEnd(which);
  }

  nth(n: number): void {
    this.list?.nth(n);
  }

  async draw(): Promise<void> {
    if (this.win === null || this.list === null) return;
    if (this.drawing) {
      this.again = true;
      return;
    }
    this.drawing = true;
    try {
      do {
        this.again = false;
        const [sort, group, view] = await Promise.all([
          this.neosh.opt.get<Sort>("archive.sort"),
          this.neosh.opt.get<Group>("archive.group"),
          this.neosh.win.viewport(this.win).catch(() => null),
        ]);
        // What the panel actually got, which on a terminal narrower than `archive.width` is not
        // what it asked for. Clipping to the number we wanted rather than the number we have is how
        // a title runs into the border on a small window.
        if (view && view.width > 0) this.width = view.width;
        const at = now();
        this.shown = sorted(this.entries.filter((e) => matches(e, this.filter)), sort ?? "archived");
        const built = this.build(this.shown, group ?? "project", at, sort ?? "archived");
        this.list.setRows(built, (a, b) => a.id === b.id);
        await this.list.render({ showCursor: true, win: this.win, pinned: 3 });
      } while (this.again);
    } finally {
      this.drawing = false;
    }
  }

  /** The header, the headings, the conversations, and the keys. */
  private build(shown: Entry[], group: Group, at: number, sort: Sort): ListRow<Target>[] {
    const rows: ListRow<Target>[] = [];
    const glyph = this.ascii
      ? { tick: "*", blank: " ", rule: "-" }
      : { tick: "✓", blank: " ", rule: "┈" };

    rows.push(this.header(shown, sort, group));
    rows.push({ text: "", inert: true });

    for (const bucket of grouped(shown, group, at)) {
      if (bucket.heading !== null) {
        rows.push({
          text: ` ${clip(bucket.heading, this.width - 3)}`,
          hl: "Sidebar.Heading",
          inert: true,
        });
      }
      for (const e of bucket.entries) rows.push(this.row(e, at, glyph));
    }

    if (shown.length === 0) {
      rows.push({
        text: `  nothing archived matches "${clip(this.filter, 40)}"`,
        hl: "Sidebar.Dim",
        inert: true,
      });
    }

    rows.push({ text: "", inert: true });
    for (const line of this.hints()) {
      rows.push({ text: ` ${clip(line, this.width - 2)}`, hl: "Sidebar.Dim", inert: true });
    }
    return rows;
  }

  private header(shown: Entry[], sort: Sort, group: Group): ListRow<Target> {
    const total = this.entries.length;
    const parts = [`${total} archived`];
    if (this.filter !== "") parts.push(`${shown.length} matching "${clip(this.filter, 24)}"`);
    if (this.marks.size > 0) parts.push(`${this.marks.size} marked`);
    // Said once, and only when it is true: past the restore cap a conversation is a file this
    // workspace has never read, and a count that quietly differed from the sidebar's would be the
    // kind of discrepancy people spend an afternoon on.
    const cold = this.entries.filter((e) => !e.loaded).length;
    if (cold > 0) parts.push(`${cold} on disk only`);
    const right = this.count.pending !== ""
      ? `${this.count.pending}`
      : `${SORT_LABEL[sort]} · ${GROUP_LABEL[group]} `;
    return {
      // Measured against what is on the right rather than a number that was true when it was
      // written: the right-hand text is drawn as virtual text and pushes nothing, so a header
      // clipped to a guess is a header that runs underneath it.
      text: ` ${clip(parts.join("  ·  "), Math.max(12, this.width - right.length - 3))}`,
      hl: "Sidebar.Heading",
      right: { text: right, hl: "Sidebar.Dim" },
      inert: true,
    };
  }

  private row(
    e: Entry,
    at: number,
    glyph: { tick: string; blank: string; rule: string },
  ): ListRow<Target> {
    const marked = this.marks.has(e.info.id);
    const count = e.info.message_count;
    const when = ago(at - putAway(e.info));
    const right = `${count} ${count === 1 ? "msg" : "msgs"}${when === "" ? "" : ` · ${when}`} `;
    const prefix = ` ${marked ? glyph.tick : glyph.blank} `;
    // The room a label has is what is left after the marker and the right-hand column, which is
    // drawn as virtual text and therefore does not push anything: measured, not guessed.
    const room = Math.max(8, this.width - byteLength(prefix) - right.length - 2);
    const full = `${prefix}${e.info.label}`;
    return {
      text: `${prefix}${clip(e.info.label, room)}`,
      // The rest of a clipped title, while the cursor is on it. A list of conversations whose
      // titles all end in `…` is a list you cannot tell two rows of apart.
      full,
      indent: byteLength(prefix),
      right: { text: right, hl: "Sidebar.Dim" },
      spans: marked
        ? [{ from: 1, to: 1 + byteLength(glyph.tick), hl: "Status.Ok" }]
        : undefined,
      value: { kind: "session", id: e.info.id },
    };
  }

  /**
   * The keys, at the foot, changing with what is ticked.
   *
   * Written out rather than inferred from the keymap because these are the panel's own bindings and
   * it knows them — but the *verbs* on the second row are read off the contributions, so a plugin
   * that adds a key to this panel gets it advertised rather than having to document it elsewhere.
   */
  private hints(): string[] {
    const many = this.marks.size > 0;
    const what = many ? `${this.marks.size} marked` : "this one";
    const contributed = this.actions.map((a) => `${a.key} ${a.label}`).join("   ");
    return [
      `↵ open   u put back (${what})   X delete (${what})   space mark   a all`,
      `/ filter   s sort   S group   e copy   ^X empty${
        this.filtered() ? " what is shown" : ""
      }   esc close${contributed === "" ? "" : `   ${contributed}`}`,
    ];
  }
}

// ---------------------------------------------------------------------------
// Verbs
// ---------------------------------------------------------------------------

async function registerCommands(
  neosh: Neosh,
  subscriptions: Disposable[],
  panel: Panel,
): Promise<void> {
  const keep = async (name: string, desc: string, fn: () => Promise<void> | void) => {
    subscriptions.push(await neosh.cmd.register(name, () => fn(), { desc }));
  };

  await keep("archive.open", "What you have archived", () => panel.open());
  // The name the sidebar used to answer to, kept because anybody's `init.ts` may be pointed at it.
  await keep("session.archived", "Browse what you have archived", () => panel.open());
  await keep(
    "archive.sweep",
    "Delete everything archived longer ago than `archive.retention_days`",
    () => sweep(neosh),
  );
  await keep(
    "archive.tidy",
    "Archive conversations idle longer than `archive.auto_days`",
    async () => {
      await autoArchive(neosh, true);
    },
  );
}

/**
 * Register one verb, and bind it inside this panel.
 *
 * Every key here goes through this, which is the point: `^Z` lists them, `^K` runs them, and
 * `keymap.set("chat", "d", "…", { scope: { kind: "buf_kind", name: "neosh.archive" } })` from
 * anybody's `init.ts` replaces one. The scope is the buffer *kind* rather than the window, because
 * the window is made and destroyed every time the panel is opened and a binding on it would go with
 * it.
 */
async function bindKeys(
  neosh: Neosh,
  subscriptions: Disposable[],
  panel: Panel,
): Promise<void> {
  const scope = { kind: "buf_kind", name: KIND } as const;

  const verb = async (
    name: string,
    keys: string | string[] | null,
    desc: string,
    fn: () => Promise<void> | void,
    opts: { redraw?: boolean } = {},
  ): Promise<void> => {
    subscriptions.push(await neosh.cmd.register(name, async () => {
      await fn();
      // A count belongs to the motion typed straight after it. Anything else ends it — otherwise a
      // `5` you thought better of sits there and turns the next `j` into five.
      panel.count.pending = "";
      if (opts.redraw !== false) await panel.draw();
    }, { desc }));
    for (const key of keys === null ? [] : typeof keys === "string" ? [keys] : keys) {
      await neosh.keymap.set("chat", key, name, { scope, desc });
    }
  };

  /** Take the count that was typed, if one was, and forget it. */
  const take = (fallback = 1): number => {
    const n = Number.parseInt(panel.count.pending, 10);
    panel.count.pending = "";
    return Number.isFinite(n) && n > 0 ? Math.min(n, 999) : fallback;
  };
  const typed = (): number | null => {
    const n = Number.parseInt(panel.count.pending, 10);
    panel.count.pending = "";
    return Number.isFinite(n) && n > 0 ? n : null;
  };

  // ---- moving ----
  await verb(`${NS}.down`, ["j", "<Down>", "<C-n>"], "Next row", () => panel.move(take()));
  await verb(`${NS}.up`, ["k", "<Up>", "<C-p>"], "Previous row", () => panel.move(-take()));

  const by = (fraction: number, sign: 1 | -1) => async (): Promise<void> => {
    const rows = await panel.rows();
    const step = Math.max(1, Math.floor(rows * fraction)) * take();
    // No wrapping on a page step: `^D` at the foot means there is no more of it, and a cursor that
    // reappears at the top has thrown away the place you were reading from.
    panel.move(sign * step, { wrap: false });
  };
  await verb(`${NS}.half.down`, "<C-d>", "Half a screen down", by(0.5, 1));
  await verb(`${NS}.half.up`, "<C-u>", "Half a screen up", by(0.5, -1));
  await verb(`${NS}.page.down`, "<PageDown>", "A screen down", by(1, 1));
  await verb(`${NS}.page.up`, "<PageUp>", "A screen up", by(1, -1));

  await verb(`${NS}.top`, "gg", "The first row, or the n-th with a count", () => {
    const n = typed();
    if (n === null) panel.toEnd("first");
    else panel.nth(n);
  });
  await verb(`${NS}.bottom`, "G", "The last row, or the n-th with a count", () => {
    const n = typed();
    if (n === null) panel.toEnd("last");
    else panel.nth(n);
  });

  // One command for all ten digits, reading which it was off the key that ran it — so `^Z` lists it
  // once and `init.ts` can move it, rather than ten near-identical verbs.
  subscriptions.push(await neosh.cmd.register(`${NS}.count`, async (_args, key) => {
    const code = key?.key.code;
    if (code?.kind !== "char") return;
    if (panel.count.pending === "" && code.c === "0") return;
    if (panel.count.pending.length < 3) panel.count.pending += code.c;
    await panel.draw();
  }, { desc: "Begin a count for the next motion" }));
  for (const digit of "0123456789") {
    await neosh.keymap.set("chat", digit, `${NS}.count`, {
      scope,
      desc: "Count for the next motion",
    });
  }

  // ---- marking ----
  await verb(`${NS}.mark`, ["<Space>", "<Tab>"], "Tick or untick this conversation", () => {
    // The row under the cursor, never the selection: ticking is about one row, and a `<Space>` that
    // toggled the first of ten already-marked rows would be a key that undid somebody else's work.
    const id = panel.cursorEntry()?.info.id ?? null;
    if (id === null) return;
    if (panel.marks.has(id)) panel.marks.delete(id);
    else panel.marks.add(id);
    // Down a row afterwards, so ticking a run of them is one key repeated rather than two
    // alternating. `<Space>` at the foot of the list stays there rather than wrapping to the top.
    panel.move(1, { wrap: false });
  });
  await verb(`${NS}.mark.all`, "a", "Tick everything showing, or untick it", () => {
    const shown = panel.listed();
    const all = shown.length > 0 && shown.every((e) => panel.marks.has(e.info.id));
    if (all) for (const e of shown) panel.marks.delete(e.info.id);
    else for (const e of shown) panel.marks.add(e.info.id);
  });

  // ---- what to do with them ----
  await verb(`${NS}.open`, "<CR>", "Put this back and go to it", async () => {
    // `↵` restores *and* opens, because opening something you put away is a statement that you want
    // it back: leaving it archived while you work in it would mean the list you look at does not
    // contain the conversation you are in.
    const one = panel.cursorEntry();
    if (!one) return;
    try {
      await neosh.session.archive(one.info.id, false);
      await neosh.session.switch(one.info.id);
      await panel.close();
      await refreshSidebarRow(neosh);
      neosh.notify(`restored "${clip(one.info.label, 40)}"`);
    } catch (e) {
      neosh.notify(String(e), "warn");
    }
  }, { redraw: false });

  // `u`, and deliberately not `^U` as well. The picker had `^U` for this because a bare
  // letter there is a letter the filter can never contain; in a panel whose filter is a prompt the
  // letter is free, and `^U` has a job — half a screen — that every list in the
  // workspace gives it. Binding both would have quietly taken the motion away, in the one panel most likely
  // to be longer than a screen.
  await verb(`${NS}.restore`, "u", "Put these back, without going there", async () => {
    // Tidying and travelling are different intentions, and being thrown into a conversation per row
    // you restore is not tidying.
    const chosen = panel.selection();
    if (chosen.length === 0) return;
    let done = 0;
    for (const e of chosen) {
      try {
        await neosh.session.archive(e.info.id, false);
        done += 1;
      } catch (err) {
        neosh.notify(String(err), "warn");
        break;
      }
    }
    panel.marks.clear();
    if (done > 0) {
      neosh.notify(done === 1 ? "put back" : `put ${done} conversations back`);
    }
    await panel.refresh();
  }, { redraw: false });

  await verb(`${NS}.delete`, "X", "Delete these, for good", async () => {
    const chosen = panel.selection();
    if (chosen.length === 0) return;
    if (!(await confirmDelete(neosh, chosen, "delete"))) return;
    await destroy(neosh, chosen);
    panel.marks.clear();
    await panel.refresh();
  }, { redraw: false });

  await verb(`${NS}.empty`, "<C-x>", "Empty the archive", async () => {
    const chosen = panel.listed();
    if (chosen.length === 0) return;
    if (!(await confirmDelete(neosh, chosen, panel.filtered() ? "empty-filtered" : "empty"))) return;
    await destroy(neosh, chosen);
    panel.marks.clear();
    await panel.refresh();
  }, { redraw: false });

  await verb(`${NS}.copy`, "e", "Copy these conversations as markdown", async () => {
    const chosen = panel.selection();
    if (chosen.length === 0) return;
    await copyTranscripts(neosh, chosen);
  }, { redraw: false });

  await verb(`${NS}.copy.path`, "y", "Copy this conversation's directory", async () => {
    const one = panel.cursorEntry();
    if (!one) return;
    await neosh.edit.copy(one.info.cwd);
    neosh.notify(`copied ${one.info.cwd}`);
  }, { redraw: false });

  // ---- how the list is shown ----
  //
  // A prompt rather than typing straight into the panel. Every letter here is a verb — `a`, `e`,
  // `u`, `y` — and a filter that took them would be a filter that could not contain them; shadowing
  // twenty-six bindings for the duration of a search is the kind of mode that goes wrong once and
  // then goes wrong every time.
  await verb(`${NS}.filter`, "/", "Filter the list", async () => {
    const typedIn = await prompt(neosh, "Filter the archive", { initial: panel.filter, width: 56 });
    if (typedIn === null) return;
    panel.filter = typedIn.trim();
  });

  await verb(`${NS}.sort`, "s", "The next ordering along", async () => {
    const at = SORTS.indexOf((await neosh.opt.get<Sort>("archive.sort")) ?? "archived");
    await neosh.opt.set("archive.sort", SORTS[(at + 1) % SORTS.length]!);
  });
  await verb(`${NS}.group`, "S", "The next grouping along", async () => {
    const at = GROUPS.indexOf((await neosh.opt.get<Group>("archive.group")) ?? "project");
    await neosh.opt.set("archive.group", GROUPS[(at + 1) % GROUPS.length]!);
  });

  await verb(`${NS}.help`, "?", "The keys for this panel", async () => {
    await neosh.cmd.exec("help.keys").catch(() => {});
  }, { redraw: false });

  // ---- leaving ----
  //
  // One thing per press, the way `<Esc>` works in the transcript: what you have ticked, then what
  // you have filtered to, then the panel. A key that threw all three away at once would mean a
  // mistyped filter cost you a selection you had spent a minute making.
  await verb(`${NS}.escape`, "<Esc>", "Drop the marks, the filter, then close", async () => {
    if (panel.marks.size > 0) {
      panel.marks.clear();
      return;
    }
    if (panel.filter !== "") {
      panel.filter = "";
      return;
    }
    await panel.close();
  });
  await verb(`${NS}.close`, ["q", "<C-c>", "<C-f>"], "Close the archive", () => panel.close(), {
    redraw: false,
  });

  // Where the keys nothing claimed go: nowhere. Not a handler — a sink. The float is modal, so an
  // unbound key is swallowed rather than reaching the composer; this is what stops one that a
  // *nearer* scope would otherwise leak.
  subscriptions.push(await neosh.cmd.register(`${NS}.key`, () => {}, {
    desc: "Swallow an unbound key in the archive",
  }));

  // The way in, from anywhere. `^F` has always been the archive's key and it toggles, because a key
  // that opens a modal and cannot shut it is a key you have to remember a second one for.
  await neosh.keymap.set("chat", "<C-f>", "archive.open", {
    desc: "Archived conversations",
  });

  await bindContributed(neosh, subscriptions, panel, scope);
}

/**
 * Keys somebody else put on these rows.
 *
 * Bound here rather than by the contributor, so the command is invoked with the conversations it is
 * about and a plugin never has to track this panel's cursor. Re-read when the contributions change,
 * because a plugin that loads after this panel has drawn would otherwise have a key nobody sees
 * until the next restart.
 */
async function bindContributed(
  neosh: Neosh,
  subscriptions: Disposable[],
  panel: Panel,
  scope: { kind: "buf_kind"; name: string },
): Promise<void> {
  /** The keys and commands from the last pass, so a re-read replaces rather than accumulates. */
  let bound: Array<{ key: string; cmd: Disposable }> = [];

  const apply = async () => {
    const items = (await neosh.ext.list<ActionItem>(POINT_ACTION).catch(() => []))
      .map((c) => c.item)
      .filter((a): a is ActionItem => !!a?.key && !!a.command);
    for (const previous of bound) {
      await neosh.keymap.del("chat", previous.key, scope).catch(() => {});
      previous.cmd.dispose();
    }
    bound = [];
    for (const action of items) {
      const name = `${NS}.custom.${action.command}`;
      const cmd = await neosh.cmd.register(name, async () => {
        // The conversations it is about, as arguments. A contributed verb never has to find this
        // panel's cursor, which is the whole reason the key is bound here rather than by the
        // plugin that wanted it.
        const ids = panel.selection().map((e) => e.info.id);
        await neosh.cmd.exec(action.command, ids).catch((e) => neosh.notify(String(e), "warn"));
        await panel.refresh();
      }, { desc: action.label });
      await neosh.keymap.set("chat", action.key, name, { scope, desc: action.label });
      bound.push({ key: action.key, cmd });
    }
  };

  await apply();
  subscriptions.push(neosh.ext.onChange((e) => {
    if (e.point === POINT_ACTION) void apply();
  }));
  subscriptions.push({
    dispose: () => {
      for (const previous of bound) previous.cmd.dispose();
    },
  });
}

// ---------------------------------------------------------------------------
// Deleting, and being asked about it
// ---------------------------------------------------------------------------

/**
 * The question, with what is at stake in it.
 *
 * The rule: everything irreversible asks, unconditionally, and the value of the question is
 * that it is always there. So it says how many, how much is in them and where they came from —
 * "Are you sure?" is a speed bump you learn to clear without reading, and a number is not.
 */
async function confirmDelete(
  neosh: Neosh,
  chosen: Entry[],
  shape: "delete" | "empty" | "empty-filtered",
): Promise<boolean> {
  const n = chosen.length;
  const messages = chosen.reduce((sum, e) => sum + e.info.message_count, 0);
  const projects = new Set(chosen.map((e) => projectOf(e.info))).size;
  const oldest = chosen.reduce((old, e) => Math.min(old, putAway(e.info)), Number.MAX_SAFE_INTEGER);
  const span = ago(now() - oldest);

  const question = shape === "delete"
    ? (n === 1
      ? `Delete "${clip(chosen[0]!.info.label, 44)}"?`
      : `Delete ${n} archived conversations?`)
    : shape === "empty-filtered"
    ? `Delete all ${n} archived conversations shown?`
    : `Empty the archive — all ${n} conversations?`;

  const detail = [
    `${messages} ${messages === 1 ? "message" : "messages"} in ${
      n === 1 ? "it" : "them"
    }, from ${projects} ${projects === 1 ? "project" : "projects"}${
      span === "" ? "" : `, going back ${span}`
    }.`,
    n === 1
      ? "It goes from disk, and there is no undo."
      : "They go from disk, and there is no undo.",
    "`u` puts one back into your list instead, and costs nothing.",
  ];

  return confirmDestructive(neosh, question, { yes: "Delete", no: "Keep", detail });
}

/** Delete them, saying what went and stopping at the first thing that would not. */
async function destroy(neosh: Neosh, chosen: Entry[]): Promise<number> {
  let done = 0;
  for (const e of chosen) {
    try {
      await neosh.session.close(e.info.id);
      done += 1;
    } catch (err) {
      // Named, because "12 of 40" with no reason is a state nobody can act on. The commonest cause
      // is a conversation that stopped being archived while the panel was open.
      neosh.notify(`stopped after ${done}: ${String(err)}`, "warn");
      return done;
    }
  }
  if (done > 0) neosh.notify(done === 1 ? "deleted" : `deleted ${done} conversations`);
  return done;
}

// ---------------------------------------------------------------------------
// Getting a piece of it out
// ---------------------------------------------------------------------------

/**
 * Copy what was said, as markdown.
 *
 * The clipboard rather than a file, and the reason is the same one that makes `Clipboard` an OSC
 * escape: the workspace may be on another machine entirely, so a path it writes to is a path on the
 * wrong computer. This is the transcript reader's `ya` aimed at conversations you are about to
 * throw away — which is the last moment anybody would want it.
 */
async function copyTranscripts(neosh: Neosh, chosen: Entry[]): Promise<void> {
  const parts: string[] = [];
  for (const e of chosen) {
    const messages = await neosh.session.messages(e.info.id).catch(() => [] as Message[]);
    parts.push(render(e.info, messages));
  }
  const text = parts.join("\n\n---\n\n");
  if (text.trim() === "") {
    neosh.notify("nothing was said in that", "info");
    return;
  }
  await neosh.edit.copy(text);
  const n = chosen.length;
  neosh.notify(n === 1 ? "copied the conversation" : `copied ${n} conversations`);
}

function render(info: SessionInfo, messages: Message[]): string {
  const out = [`# ${info.label}`, "", `_${info.project || info.cwd} · ${info.message_count} messages_`, ""];
  for (const m of messages) {
    const text = m.content
      .map((b) => {
        switch (b.type) {
          case "text":
            return b.text;
          // Left out on purpose: reasoning is the model's working, a tool result is often thousands
          // of lines of one, and what somebody wants out of a conversation is what was said in it.
          case "tool_use":
            return `> ran \`${b.name}\``;
          default:
            return "";
        }
      })
      .filter((t) => t !== "")
      .join("\n\n");
    if (text === "") continue;
    out.push(`## ${m.role === "user" ? "You" : m.role === "assistant" ? "Agent" : m.role}`, "", text, "");
  }
  return out.join("\n");
}

// ---------------------------------------------------------------------------
// Housekeeping
// ---------------------------------------------------------------------------

/**
 * What the workspace does about the archive on its own, which is: archive, and mention.
 *
 * `archive.auto_days` puts an idle conversation away. That is allowed to happen without being asked
 * because archiving is reversible and free — it is the *test* for whether a dialog is owed, applied
 * from the other side — and the alert says how many so it is never something you find out about by
 * noticing a row has gone.
 *
 * `archive.retention_days` deletes nothing, ever. It says how much of the archive is old and points
 * at the key that empties it. The workspace refuses to delete history on a timer, and a
 * setting that quietly reversed that would be the program deciding something that is not its to
 * decide. `archive.sweep` is the same number with a person behind it.
 */
async function housekeeping(neosh: Neosh): Promise<void> {
  // The reminder waits for a pass that did nothing else. A notice is a *reply* and does not stack,
  // so two sent a moment apart is one notice: `archived 2 conversations idle for 7+ days` arrived
  // and was immediately painted over by `1 is older than 30 days`, which is the less useful of the
  // two by a distance — one is something that just happened to your list, the other is a standing
  // fact that will still be true in six hours when this runs again.
  const moved = await autoArchive(neosh, false);
  if (moved === 0) await remind(neosh);
  await refreshSidebarRow(neosh);
}

async function autoArchive(neosh: Neosh, spoken: boolean): Promise<number> {
  const days = (await neosh.opt.get<number>("archive.auto_days").catch(() => 0)) ?? 0;
  if (days <= 0) {
    if (spoken) neosh.notify("`archive.auto_days` is off", "info");
    return 0;
  }
  const cutoff = now() - days * 86400;
  const sessions = await neosh.session.list().catch(() => [] as SessionInfo[]);
  const stale = sessions.filter(
    (s) =>
      !s.archived &&
      !s.is_active &&
      // Never one that is working, and never one that is waiting on you: idleness is measured in
      // when something last happened, and a turn in flight is something happening now.
      !s.active_turn &&
      !s.unread &&
      s.message_count > 0 &&
      s.updated_at < cutoff,
  );
  if (stale.length === 0) {
    if (spoken) neosh.notify(`nothing has been idle for ${days} days`, "info");
    return 0;
  }
  let done = 0;
  for (const s of stale) {
    try {
      await neosh.session.archive(s.id, true);
      done += 1;
    } catch {
      // One that refuses is one the store had a reason to keep — the last open conversation, most
      // likely. Not worth a warning per conversation on a housekeeping pass.
    }
  }
  if (done > 0) {
    neosh.notify(
      `archived ${done} ${done === 1 ? "conversation" : "conversations"} idle for ${days}+ days — \`^F\` has them`,
    );
  }
  return done;
}

async function remind(neosh: Neosh): Promise<void> {
  const [days, on] = await Promise.all([
    neosh.opt.get<number>("archive.retention_days").catch(() => 0),
    neosh.opt.get<boolean>("archive.remind").catch(() => true),
  ]);
  if ((days ?? 0) <= 0 || on === false) return;
  const old = (await collect(neosh)).filter((e) => putAway(e.info) < now() - (days ?? 0) * 86400);
  if (old.length === 0) return;
  neosh.notify(
    `${old.length} archived ${old.length === 1 ? "conversation is" : "conversations are"} older than ${days} days — \`^F\`, then \`^X\``,
    "info",
  );
}

/** The by-hand version of the thing this plugin will not do on a timer. */
async function sweep(neosh: Neosh): Promise<void> {
  const days = (await neosh.opt.get<number>("archive.retention_days").catch(() => 0)) ?? 0;
  if (days <= 0) {
    neosh.notify("set `archive.retention_days` first — this deletes what is older than it", "warn");
    return;
  }
  const old = (await collect(neosh)).filter((e) => putAway(e.info) < now() - days * 86400);
  if (old.length === 0) {
    neosh.notify(`nothing has been archived longer than ${days} days`, "info");
    return;
  }
  if (!(await confirmDelete(neosh, old, "empty-filtered"))) return;
  await destroy(neosh, old);
  announced = null;
  await refreshSidebarRow(neosh);
}

// ---------------------------------------------------------------------------
// The door in the sidebar
// ---------------------------------------------------------------------------

const POINT_SECTION = "sidebar.section";
const POINT_SIDEBAR_ACTION = "sidebar.action";

/**
 * `a` on every row of the project panel — and, only if you ask for it, a row saying how many.
 *
 * The row is **off**. It used to be a door rather than a drawer left open, and it is still the
 * smallest possible version of one: a single dim line, and only while there is something behind it.
 * But the column it sits in is your conversations, the thing it announces is by definition the
 * thing you are finished with, and a permanent line about what you are *not* doing is a line the
 * panel cannot spare — which is the same argument that took the section out, applied to
 * what it left behind.
 *
 * What replaces it is what the archive was always reached by: `^F` from anywhere, `a` from the
 * panel — advertised in the panel's key strip, so it is not a key you have to have been told about
 * — and `^K`. `archive.sidebar = true` puts the row back for anybody who wants the count on screen.
 *
 * Both are contributed rather than drawn by the sidebar, which is the surface claim tested on
 * ourselves: the panel that owns the archive is not the panel that says there is one. Turn this
 * plugin off and both go with it, rather than leaving a row pointing at a command that has gone.
 */
async function contributeToSidebar(neosh: Neosh, subscriptions: Disposable[]): Promise<void> {
  await neosh.ext.contribute(POINT_SIDEBAR_ACTION, "browse", {
    key: "a",
    label: "archive",
    command: "archive.open",
    on: "any",
  });
  subscriptions.push({
    dispose: () => void neosh.ext.remove(POINT_SIDEBAR_ACTION, "browse").catch(() => {}),
  });
  subscriptions.push({
    dispose: () => void neosh.ext.remove(POINT_SECTION, "archive").catch(() => {}),
  });
}

/**
 * How many there are, in the sidebar — when it was asked for, and nothing at all when there are
 * none.
 *
 * Off unless `archive.sidebar` says otherwise; see {@link contributeToSidebar}. `null` rather than
 * `0` for "nothing announced yet", so turning the option off at runtime withdraws the row instead
 * of matching a remembered count and returning early.
 */
let announced: number | null = null;

async function refreshSidebarRow(neosh: Neosh): Promise<void> {
  const [ascii, wanted] = await Promise.all([
    neosh.opt.get<boolean>("ui.ascii_only").catch(() => false),
    neosh.opt.get<boolean>("archive.sidebar").catch(() => false),
  ]);
  const count = wanted === true ? await archivedCount(neosh) : 0;
  if (count === announced) return;
  announced = count;
  if (count === 0) {
    await neosh.ext.remove(POINT_SECTION, "archive").catch(() => {});
    return;
  }
  await neosh.ext.contribute(POINT_SECTION, "archive", {
    at: "below",
    rows: [{
      text: ` ${ascii ? "-" : "┈"} Archived`,
      hl: "Sidebar.Dim",
      right: { text: `${count}  ^F `, hl: "Sidebar.Dim" },
      command: "archive.open",
    }],
  }, { priority: 20 });
}

// ---------------------------------------------------------------------------
// Small things
// ---------------------------------------------------------------------------

function basename(path: string): string {
  const at = path.replace(/\/+$/, "").lastIndexOf("/");
  return at < 0 ? path : path.slice(at + 1);
}

/** Clip to `n` characters, with an ellipsis where something was taken off. */
function clip(text: string, n: number): string {
  const chars = Array.from(text);
  if (chars.length <= n) return text;
  return `${chars.slice(0, Math.max(1, n - 1)).join("")}…`;
}

/** How long ago, in one short word. Empty for anything in the last minute. */
function ago(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 60) return "";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d`;
  const weeks = Math.floor(days / 7);
  if (weeks < 9) return `${weeks}w`;
  const months = Math.floor(days / 30);
  if (months < 12) return `${months}mo`;
  return `${Math.floor(days / 365)}y`;
}
