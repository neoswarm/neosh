/**
 * The sidebar: your projects, the conversations inside them, and which one is working.
 *
 * Two sections and nothing else. Favourites on top because the projects you actually live in are a
 * short list and a long one buries them; everything else under `PROJECTS`, most recently used
 * first, foldable so a project you are not in costs one line instead of ten.
 *
 * It is deliberately *not* a dashboard. Changed files, the model and the token meter all used to be
 * here and all of them are better in the footer, which is always visible and does not push your
 * conversations off the bottom of the screen. A panel that shows six kinds of thing teaches you to
 * stop reading it.
 *
 * The bottom four lines are the keys for whatever the cursor is on, and they change with it. That
 * is the part worth defending: a terminal UI that hides its verbs is a terminal UI you use three of.
 *
 * Motion is frugal — one shared clock in `@neosh/api/ui`, running only while a turn is in flight,
 * and only the working row animates. Continuous repaint costs power for no information.
 *
 * It is a plugin. Everything here is on the public API, `plugins.disabled = ["sidebar"]` turns it
 * off, and a sidebar of your own loads after this one and wins. But replacing it is the *last*
 * resort rather than the first, and three things are here so that it usually is not necessary:
 *
 * - **Every key in this panel is an ordinary binding**, on the buffer kind `neosh.sidebar`, pointed
 *   at a command with a name. `F1` lists them, the palette runs them, and one line in your
 *   `init.ts` moves any of them. There is no private switch statement any more — there was, and
 *   what it meant was that adding one key to this panel meant forking all twelve hundred lines of
 *   it.
 * - **`sidebar.section` is a contribution point.** Anything you can describe as rows appears in the
 *   column, in the position you ask for, invoking your commands.
 * - **`sidebar.action` is a verb on a row.** Say the key, the label and the command; this panel
 *   binds it, shows it in the hint strip when it applies, and invokes your command with the row
 *   under the cursor as arguments.
 *
 * What a project *is* — pinned, ordered, folded — lives in shared project vars rather than in this
 * plugin's private state, so a panel of your own, a status segment or a picker all read the same
 * favourites rather than each starting empty. See ADR 0040.
 */

import { byteLength, projectScope } from "@neosh/api";
import type {
  AgentSummary,
  Contribution,
  Disposable,
  Neosh,
  PluginContext,
  NodeInfo,
  SessionInfo,
  SwarmAgent,
  VarScope,
  WindowId,
  WorktreeInfo,
} from "@neosh/api";
import {
  confirmDestructive,
  configureMotion,
  CursoredList,
  elapsed,
  type ListRow,
  money,
  onTick,
  pathPicker,
  picker,
  type PickerItem,
  prompt,
  pulseBright,
  spinnerFrame,
} from "@neosh/api/ui";

/** What a row points at, so the cursor survives the list being rebuilt underneath it. */
type Target =
  | { kind: "project"; cwd: string }
  | { kind: "session"; id: string; cwd: string }
  /** The row that opens another directory. A row rather than only a key, because a verb nobody
   * can see is a verb nobody uses. */
  | { kind: "add" }
  /** The way into what you have put away. Only present when there is something in it. */
  | { kind: "browse"; count: number }
  /** A row somebody else contributed. `command` runs on `↵`, with `args` as given. */
  | { kind: "custom"; command?: string; args?: string[] }
  /** A conversation on another computer. Addressed as `(node, session)`; the session id alone is
   * unique only on its own machine. */
  | { kind: "remote"; node: string; session: string; cwd: string; host: string };

/**
 * What the sidebar reads to find out what is not its own.
 *
 * Two points rather than one because they are answers to different questions. A *section* is
 * content — rows in the column, which the contributor owns and re-contributes when its data
 * changes. An *action* is a verb — a key on a row this panel already draws, which this panel binds
 * and invokes on the contributor's behalf so that the row under the cursor can be passed along.
 */
const POINT_SECTION = "sidebar.section";
const POINT_ACTION = "sidebar.action";

/** A block of rows somebody else owns. Contributed as data, so it survives being listed and disabled. */
interface SectionItem {
  /** Drawn as a heading with a rule under it, matching `PROJECTS`. Omit for rows with no heading. */
  title?: string;
  /** Where it goes relative to the project list. Defaults to `below`. */
  at?: "above" | "below";
  rows?: Array<{
    text: string;
    hl?: string;
    right?: { text: string; hl?: string };
    /** Run on `↵`. Without one the row is inert — a label rather than a verb. */
    command?: string;
    args?: string[];
  }>;
}

/**
 * A verb on a row, contributed by somebody else.
 *
 * The command is invoked with the row under the cursor as arguments — `[kind, cwd]` for a project,
 * `[kind, cwd, sessionId]` for a conversation — because a key press carries no arguments of its
 * own and a plugin that had to track the cursor separately would be one race away from acting on
 * the wrong row.
 */
interface ActionItem {
  /** Key notation, as `keymap.set` takes it: `d`, `<C-y>`, `gd`. */
  key: string;
  /** What it does, for the hint strip and for `F1`. */
  label: string;
  command: string;
  /** Which rows it applies to. Defaults to `any`. */
  on?: "project" | "session" | "any";
}

/** A project as the panel thinks of it: a directory, and what is going on in it. */
interface Project {
  cwd: string;
  name: string;
  favorite: boolean;
  sessions: SessionInfo[];
  /** The cross-machine identity, for matching against what other computers have. */
  key: string;
}

const NS = "neosh.sidebar";
/** What this panel's buffer says it is. Everything a third party binds or finds hangs off this. */
const KIND = "neosh.sidebar";

/**
 * Project vars, which are shared rather than ours.
 *
 * These used to be three arrays in this plugin's private state, which worked exactly as long as
 * this was the only panel: a sidebar of your own started with no favourites, and pinning a project
 * here was invisible to a status segment that wanted to say so. They are namespaced `sidebar.*`
 * because that is what a var key is expected to look like, not because they belong to us — anybody
 * may set them, and this panel redraws when they do.
 */
const VAR_FAVORITE = "sidebar.favorite";
const VAR_RANK = "sidebar.rank";
const VAR_FOLDED = "sidebar.folded";
/**
 * The directories this panel knows about, in workspace scope.
 *
 * An index rather than a second source of truth: a project with no conversation in it has nothing
 * to derive its existence from, so pinning one has to be written down somewhere. Kept current from
 * both ends — every conversation's directory goes in, and so does any directory somebody sets a
 * project var on, which is how a project this panel has never seen becomes a row in it.
 */
const VAR_KNOWN = "sidebar.projects";

export async function activate({ neosh, subscriptions }: PluginContext) {
  await declareOptions(neosh);

  const applyMotion = async () => {
    configureMotion({
      enabled: (await neosh.opt.get<boolean>("ui.motion")) ?? true,
      ascii: (await neosh.opt.get<boolean>("ui.ascii_only")) ?? false,
    });
  };
  await applyMotion();

  // How you left it. Read once into memory; every later write goes to the cache and to the shared
  // var store together, so a redraw never waits on a round trip.
  const arrangement = new Arrangement(neosh);
  await arrangement.load(
    (await neosh.session.list({ includeArchived: true }).catch(() => [] as SessionInfo[]))
      .map((s) => s.cwd),
  );

  // The kind is what makes this panel something other plugins can act on: it is the scope their
  // keymaps bind at and the handle `win.ofKind` finds it by. One argument, and the difference
  // between a panel you can extend and one you can only replace.
  const buf = await neosh.buf.create({ name: "[sidebar]", scratch: true, kind: KIND });
  const ns = await neosh.ns.create(NS);
  const list = new CursoredList<Target>(neosh, buf, ns);
  let win: WindowId | null = null;
  let focused = false;
  let capture: Disposable | null = null;
  let running = false;

  // Serialises redraws. Two overlapping refreshes interleave their `setLines` and `mark` calls and
  // leave highlights pointing at rows that have already been replaced.
  let drawing = false;
  let again = false;

  const draw = async () => {
    if (win === null) return;
    if (drawing) {
      again = true;
      return;
    }
    drawing = true;
    try {
      do {
        again = false;
        // Read per frame rather than cached at load: a setting arrives from `config.toml` after
        // this plugin has declared it, and a value captured at activation would be the default
        // forever.
        const [width, ascii, hints] = await Promise.all([
          neosh.opt.get<number>("sidebar.width"),
          neosh.opt.get<boolean>("ui.ascii_only"),
          neosh.opt.get<boolean>("sidebar.hints"),
        ]);
        const built = await collect(neosh, arrangement, {
          width: width ?? 34,
          ascii: ascii ?? false,
          hints: hints ?? true,
          focused,
          selected: list.value,
          actions: actions(),
          // Filled in by `collect`, which is what reads the swarm.
          remote: new Map(),
          hosts: new Map(),
        });
        running = built.running;
        list.setRows(built.rows, same);
        await list.render({ showCursor: focused, win: win ?? undefined });
      } while (again);
    } finally {
      drawing = false;
    }
  };

  const open = async () => {
    if (win !== null) return;
    const width = (await neosh.opt.get<number>("sidebar.width")) ?? 34;
    win = await neosh.win.open(buf, "left", { size: width });
    await draw();
  };

  const close = async () => {
    if (win === null) return;
    const w = win;
    win = null;
    await leave();
    await neosh.win.close(w).catch(() => {});
  };

  const leave = async () => {
    if (!focused) return;
    focused = false;
    capture?.dispose();
    capture = null;
    await neosh.focus.pop().catch(() => {});
    await draw();
  };

  const enter = async () => {
    await open();
    if (win === null || focused) return;
    focused = true;
    await neosh.focus.push(win);
    // Every key the bindings did not claim comes here, which is what makes `j`, `f` and `?` mean
    // something in the panel while `^Q` still quits.
    capture = await neosh.keymap.capture(win, `${NS}.key`).catch(() => null);
    // Land on the conversation you are in, not wherever the cursor happened to be.
    const current = await neosh.session.current().catch(() => null);
    if (current) {
      // Unfold the project it lives in, or "go to where I am" lands on a collapsed heading.
      arrangement.unfold(current.cwd);
      list.select((t) => t.kind === "session" && t.id === current.id);
    }
    await draw();
  };

  // The verbs other plugins put on our rows. Bound here rather than by them so the row under the
  // cursor can be handed along; see `installActions`.
  const installed = installActions(neosh, list, () => void draw());
  const actions = installed.actions;
  subscriptions.push(installed.dispose);

  await registerCommands({
    neosh,
    subscriptions,
    list,
    arrangement,
    draw,
    open,
    close,
    enter,
    leave,
    isOpen: () => win !== null,
  });

  // Text changes: a turn started or ended, the conversation changed, a setting moved.
  subscriptions.push(neosh.agent.onTurnStart(() => void draw()));
  subscriptions.push(neosh.agent.onTurnEnd(() => void draw()));
  subscriptions.push(neosh.agent.onToolStart(() => void draw()));
  subscriptions.push(neosh.session.onChange(() => void draw()));
  // Somebody pinned, folded or tagged a project — possibly us, possibly a plugin that has never
  // heard of this one. Both arrive here, and the arrangement takes them in the same way.
  subscriptions.push(
    neosh.vars.onChange((e) => {
      if (arrangement.observe(e.scope, e.key, e.value)) void draw();
    }),
  );
  // Rows somebody contributed came or went. Redrawing on this is what stops a plugin that loads
  // after us contributing rows nobody sees until the next unrelated refresh.
  subscriptions.push(
    neosh.ext.onChange((e) => {
      if (e.point === POINT_SECTION) void draw();
    }),
  );
  subscriptions.push(
    neosh.opt.onChange((e) => {
      if (e.name === "ui.motion" || e.name === "ui.ascii_only") {
        void applyMotion().then(draw);
      } else if (e.name === "sidebar.width" && win !== null) {
        // Reopening is the only way to change a docked size today.
        void close().then(open);
      } else if (e.name.startsWith("sidebar.") || e.name === "ui.theme") {
        void draw();
      }
    }),
  );

  // Attributes change: the shared 100 ms clock, and only while a turn is in flight. An idle
  // workspace has no timer at all.
  subscriptions.push(
    onTick(() => {
      if (win !== null && running) void draw();
    }),
  );

  await installFooter(neosh, subscriptions);

  const period = (await neosh.opt.get<number>("sidebar.refresh_ms")) ?? 4000;
  subscriptions.push(neosh.timer.every(period, () => void draw()));

  if (await neosh.opt.get<boolean>("sidebar.open")) await open();
}

function same(a: Target, b: Target): boolean {
  if (a.kind === "remote") {
    return b.kind === "remote" && a.node === b.node && a.session === b.session;
  }
  if (a.kind === "session") return b.kind === "session" && a.id === b.id;
  if (a.kind === "project") return b.kind === "project" && a.cwd === b.cwd;
  return a.kind === b.kind;
}

async function declareOptions(neosh: Neosh): Promise<void> {
  await neosh.opt.declare({
    name: "sidebar.open",
    type: { type: "bool" },
    default: true,
    description: "Show the sidebar at startup.",
  });
  await neosh.opt.declare({
    name: "sidebar.width",
    type: { type: "int", min: 16, max: 120 },
    default: 34,
    description: "Sidebar width in columns.",
  });
  await neosh.opt.declare({
    name: "sidebar.hints",
    type: { type: "bool" },
    default: true,
    description:
      "Show the keys for whatever the cursor is on, at the foot of the panel. Turn it off once they are in your fingers.",
  });
  await neosh.opt.declare({
    name: "sidebar.refresh_ms",
    type: { type: "int", min: 200, max: 60000 },
    default: 4000,
    description: "How often to re-read the workspace when nothing has told us it changed.",
  });
}

// ---------------------------------------------------------------------------
// Arrangement
// ---------------------------------------------------------------------------

/**
 * Which projects are pinned, what order you put them in, and which ones are folded.
 *
 * All of it in **project vars**, which is the difference between an arrangement and a private
 * arrangement: a var is scoped to the project it describes and anybody may read or write it, so a
 * second panel sees the same favourites and setting `sidebar.favorite` from your own plugin is a
 * one-line way to pin something.
 *
 * Cached in memory and kept current from `vars.onChange`, because a panel redraws several times a
 * second while a turn is running and a round trip per project per frame is not a thing to pay for.
 * The cache is safe precisely *because* the change event is broadcast: every write, ours or
 * anybody's, comes back through the same door.
 *
 * Not options, for the reason it has never been options: an option is configuration *you* write,
 * and an editor that rewrites your config file because you pressed `f` is one you stop trusting
 * with the file.
 */
class Arrangement {
  /** Every project we know of, and its vars. The cache is the read path; vars are the truth. */
  private cache = new Map<string, Record<string, unknown>>();
  private known: string[] = [];

  constructor(private readonly neosh: Neosh) {}

  async load(cwds: string[]): Promise<void> {
    const stored = await this.neosh.vars
      .get<string[]>({ scope: "global" }, VAR_KNOWN)
      .catch(() => null);
    this.known = unique([...strings(stored), ...cwds]);
    const all = await Promise.all(
      this.known.map((cwd) =>
        this.neosh.vars.all(projectScope(cwd)).catch(() => ({} as Record<string, unknown>))
      ),
    );
    this.known.forEach((cwd, i) => this.cache.set(cwd, all[i] ?? {}));
    if (stored === null) await this.migrate();
  }

  /**
   * Bring across an arrangement made before any of this was shared.
   *
   * These three lived in this plugin's private state until project vars existed. Losing somebody's
   * pins because the store moved underneath them is not an acceptable way to ship an improvement,
   * so the old keys are read once — on the one startup where the index does not exist yet — and
   * written into vars. The old state is left where it is rather than deleted: it costs a few bytes,
   * and a downgrade that finds it still there is a downgrade that still works.
   */
  private async migrate(): Promise<void> {
    const [favorites, order, folded] = await Promise.all([
      this.neosh.state.get<string[]>("favorites").catch(() => null),
      this.neosh.state.get<string[]>("order").catch(() => null),
      this.neosh.state.get<string[]>("folded").catch(() => null),
    ]);
    const pins = strings(favorites);
    const ranks = strings(order);
    const shut = strings(folded);
    if (pins.length === 0 && ranks.length === 0 && shut.length === 0) return;

    await this.note(unique([...pins, ...ranks, ...shut]));
    await Promise.all([
      ...pins.map((cwd) => this.set(cwd, VAR_FAVORITE, true)),
      ...ranks.map((cwd, i) => this.set(cwd, VAR_RANK, i)),
      ...shut.map((cwd) => this.set(cwd, VAR_FOLDED, true)),
    ]);
    this.neosh.log.info(`brought ${this.known.length} project arrangements across to shared vars`);
  }

  /**
   * Take in a change somebody made — including our own writes, which arrive by the same route.
   *
   * Answers whether it was about a project at all, so a caller can skip a redraw for a var that has
   * nothing to do with this panel.
   */
  observe(scope: VarScope, key: string, value: unknown): boolean {
    if (scope.scope !== "project") return false;
    const entry = this.cache.get(scope.cwd) ?? {};
    if (value === null || value === undefined) delete entry[key];
    else entry[key] = value;
    this.cache.set(scope.cwd, entry);
    // A project somebody else has just said something about is a project this panel should be
    // showing. Without this, pinning a directory from another plugin sets a var nothing ever reads.
    if (!this.known.includes(scope.cwd)) {
      this.known.push(scope.cwd);
      void this.neosh.vars.set({ scope: "global" }, VAR_KNOWN, this.known);
    }
    return true;
  }

  /** Remember directories that turned up in the conversation list. */
  async note(cwds: string[]): Promise<void> {
    const fresh = cwds.filter((c) => !this.known.includes(c));
    if (fresh.length === 0) return;
    this.known.push(...fresh);
    await Promise.all(
      fresh.map(async (cwd) => {
        this.cache.set(
          cwd,
          await this.neosh.vars.all(projectScope(cwd)).catch(() => ({})),
        );
      }),
    );
    await this.neosh.vars.set({ scope: "global" }, VAR_KNOWN, this.known);
  }

  /** Pinned directories. Also the seed for projects with no conversations. */
  pinned(): string[] {
    return this.known.filter((cwd) => this.isFavorite(cwd));
  }

  isFavorite(cwd: string): boolean {
    return this.cache.get(cwd)?.[VAR_FAVORITE] === true;
  }

  isFolded(cwd: string): boolean {
    return this.cache.get(cwd)?.[VAR_FOLDED] === true;
  }

  /** Where a project sorts. Anything you have never moved sorts after everything you have. */
  rank(cwd: string): number {
    const v = this.cache.get(cwd)?.[VAR_RANK];
    return typeof v === "number" && Number.isFinite(v) ? v : Number.MAX_SAFE_INTEGER;
  }

  async toggleFavorite(cwd: string): Promise<boolean> {
    const next = !this.isFavorite(cwd);
    await this.set(cwd, VAR_FAVORITE, next ? true : null);
    return next;
  }

  async toggleFold(cwd: string): Promise<void> {
    await this.set(cwd, VAR_FOLDED, this.isFolded(cwd) ? null : true);
  }

  /** Open a project without a keystroke — used when jumping to the conversation you are in. */
  unfold(cwd: string): void {
    if (!this.isFolded(cwd)) return;
    void this.set(cwd, VAR_FOLDED, null);
  }

  /**
   * Move a project one place within its own group.
   *
   * Every rank in the group is written back, not just the two that swapped. Without that, the first
   * `K` on a project you have never moved would reorder against ranks that do not exist and jump
   * somewhere you did not ask for — the list has to mean what is on screen before it can be
   * rearranged.
   *
   * No wrapping: a reorder that teleports the thing you are dragging to the far end is a reorder
   * you undo.
   */
  async move(groups: Project[][], cwd: string, delta: number): Promise<boolean> {
    const group = groups.find((g) => g.some((p) => p.cwd === cwd));
    if (!group) return false;
    const from = group.findIndex((p) => p.cwd === cwd);
    const to = from + delta;
    if (to < 0 || to >= group.length) return false;

    const moved = group.map((p) => p.cwd);
    const [held] = moved.splice(from, 1);
    moved.splice(to, 0, held!);
    await Promise.all(moved.map((c, i) => this.set(c, VAR_RANK, i)));
    return true;
  }

  /** One write, straight through the cache so the next frame is right without waiting for the event. */
  private async set(cwd: string, key: string, value: unknown): Promise<void> {
    const entry = this.cache.get(cwd) ?? {};
    if (value === null) {
      delete entry[key];
      this.cache.set(cwd, entry);
      await this.neosh.vars.remove(projectScope(cwd), key);
      return;
    }
    entry[key] = value;
    this.cache.set(cwd, entry);
    await this.neosh.vars.set(projectScope(cwd), key, value);
  }
}

function strings(v: unknown): string[] {
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
}

function unique(v: string[]): string[] {
  return [...new Set(v)];
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

interface Wiring {
  neosh: Neosh;
  subscriptions: PluginContext["subscriptions"];
  list: CursoredList<Target>;
  arrangement: Arrangement;
  draw(): Promise<void>;
  open(): Promise<void>;
  close(): Promise<void>;
  enter(): Promise<void>;
  leave(): Promise<void>;
  isOpen(): boolean;
}

async function registerCommands(w: Wiring): Promise<void> {
  const { neosh, list, arrangement } = w;

  /** Rebuild the grouping the arrangement reorders against, without redrawing. */
  const groups = async (): Promise<Project[][]> => {
    const sessions = await neosh.session.list().catch(() => [] as SessionInfo[]);
    return group(sessions, arrangement, new Map());
  };

  /**
   * Register one verb, and bind it inside this panel.
   *
   * Every key in this list goes through here, which is the point: there is no private key handler
   * any more, so `F1` lists the panel's keys with the rest, `^K` runs them, and
   * `keymap.set("chat", "d", "…", { scope: { kind: "buf_kind", name: "neosh.sidebar" } })` from
   * anybody's `init.ts` replaces one. The scope is the buffer kind rather than the window, because
   * the window is opened and closed as you toggle the panel and a binding on it would go with it.
   *
   * `redraw` defaults to true because most verbs change what the column says. The ones that leave
   * the panel — or open a picker that redraws on the way back — pass false, since drawing into a
   * window that is losing focus is a frame of the wrong thing.
   */
  const verb = async (
    name: string,
    key: string | null,
    desc: string,
    fn: (target: Target | undefined, args: string[]) => Promise<void> | void,
    opts: { redraw?: boolean } = {},
  ): Promise<void> => {
    w.subscriptions.push(
      await neosh.cmd.register(name, async (args) => {
        await fn(list.value, args);
        if (opts.redraw !== false) await w.draw();
      }, { desc }),
    );
    if (key !== null) {
      await neosh.keymap.set("chat", key, name, {
        scope: { kind: "buf_kind", name: KIND },
        desc,
      });
    }
  };

  // ---- moving ----
  await verb(`${NS}.down`, "j", "Next row", () => void list.move(1));
  await verb(`${NS}.up`, "k", "Previous row", () => void list.move(-1));
  await neosh.keymap.set("chat", "<Down>", `${NS}.down`, { scope: { kind: "buf_kind", name: KIND } });
  await neosh.keymap.set("chat", "<Up>", `${NS}.up`, { scope: { kind: "buf_kind", name: KIND } });
  await neosh.keymap.set("chat", "<C-n>", `${NS}.down`, { scope: { kind: "buf_kind", name: KIND } });
  await neosh.keymap.set("chat", "<C-p>", `${NS}.up`, { scope: { kind: "buf_kind", name: KIND } });

  // ---- leaving ----
  await verb(`${NS}.leave`, "<Esc>", "Back to the composer", () => w.leave(), { redraw: false });
  await neosh.keymap.set("chat", "q", `${NS}.leave`, { scope: { kind: "buf_kind", name: KIND } });
  await neosh.keymap.set("chat", "<C-c>", `${NS}.leave`, { scope: { kind: "buf_kind", name: KIND } });

  // ---- the row under the cursor ----
  await verb(
    `${NS}.select`,
    "<CR>",
    "Open a conversation, fold a project, or run the row",
    (target) => activateTarget(neosh, arrangement, target, w),
    { redraw: false },
  );
  await verb(`${NS}.fold`, "<Space>", "Fold or unfold this project", async (target) => {
    if (target?.kind !== "project") return;
    await arrangement.toggleFold(target.cwd);
  });
  await verb(`${NS}.favorite`, "f", "Pin this project to the top", async (target) => {
    const cwd = owningProject(target);
    if (cwd === null) return;
    const now = await arrangement.toggleFavorite(cwd);
    neosh.notify(now ? `favourited ${short(cwd)}` : `unfavourited ${short(cwd)}`, "info");
  });

  // Shift moves the thing rather than the cursor — the one convention every list that can be
  // rearranged already shares.
  //
  // From a conversation row it moves the project that conversation is in. Requiring the cursor to
  // be on the heading first made the feature invisible: you are looking at the project when you are
  // looking at what is inside it.
  const reorder = (delta: number) => async (target: Target | undefined) => {
    const cwd = owningProject(target);
    if (cwd === null) return;
    await arrangement.move(await groups(), cwd, delta);
  };
  await verb(`${NS}.move.down`, "J", "Move this project down", reorder(1));
  await verb(`${NS}.move.up`, "K", "Move this project up", reorder(-1));

  await verb(`${NS}.rename`, "r", "Rename this conversation", async (target) => {
    if (target?.kind !== "session") return;
    await renameSession(neosh, target.id);
  });
  // The everyday verb, and it is reversible. Archiving takes a conversation out of the list without
  // taking anything away, which is what people were reaching for `x` to do before `x` deleted
  // things. Where it goes is `a`, not four dim rows at the foot of this panel.
  await verb(`${NS}.archive`, "x", "Archive this conversation", async (target) => {
    if (target?.kind !== "session") return;
    await setArchived(neosh, target.id, true);
  });
  // Shifted, because this is the one that cannot be undone.
  await verb(`${NS}.delete`, "X", "Delete this conversation permanently", async (target) => {
    if (target?.kind !== "session") return;
    await deleteSession(neosh, target.id);
  });
  await verb(`${NS}.new`, "n", "New conversation in this project", async (target) => {
    // In the project you are looking at, which is the whole reason the cursor is there.
    const cwd = owningProject(target) ?? undefined;
    await neosh.session.create(cwd ? { cwd } : undefined);
    await w.leave();
  }, { redraw: false });

  // ---- doors out of the panel ----
  await verb(`${NS}.browse`, "a", "What you have archived", async () => {
    await browseArchive(neosh);
    await w.draw();
  }, { redraw: false });
  await verb(`${NS}.add`, "o", "Add a project", async () => {
    await w.leave();
    await neosh.cmd.exec("project.open").catch(() => {});
  }, { redraw: false });
  await verb(`${NS}.help`, "?", "The keys for this row", async () => {
    await neosh.cmd.exec("help.keys").catch(() => {});
  }, { redraw: false });

  /**
   * Where the keys nothing claimed go: nowhere.
   *
   * Not a handler — a sink. Without it an unbound letter in this panel falls through as unhandled
   * and the chat frontend types it into the composer, so pressing `z` here would silently start
   * writing a message. Every key that *does* something is a binding above; this is only what stops
   * the rest from doing something somewhere else.
   */
  w.subscriptions.push(
    await neosh.cmd.register(`${NS}.key`, () => {}, { desc: "Swallow an unbound key in the panel" }),
  );

  w.subscriptions.push(
    await neosh.cmd.register("sidebar.toggle", async () => {
      // Toggling visibility, not focus: `^T` is how you get in.
      if (w.isOpen()) await w.close();
      else await w.open();
    }, { desc: "Show or hide the sidebar" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("sidebar.focus", w.enter, { desc: "Move into the project list" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("sidebar.refresh", w.draw, { desc: "Redraw the sidebar now" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.new", () => newConversation(neosh), {
      desc: "Start a new conversation — here, or in a worktree of its own",
    }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.new.here", async () => {
      await neosh.session.create();
    }, { desc: "Start a new conversation in this project, without asking where" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.close", async (args) => {
      // Through the same gate as the key, so the palette cannot become a way around the question.
      const id = args[0] ?? (await neosh.session.current().catch(() => null))?.id;
      if (id) await deleteSession(neosh, id);
    }, { desc: "Delete a conversation permanently" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.archive", async (args) => {
      const id = args[0] ?? (await neosh.session.current().catch(() => null))?.id;
      if (id) await setArchived(neosh, id, true);
    }, { desc: "Archive a conversation — it keeps everything and can come back" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.unarchive", async (args) => {
      const id = args[0];
      if (!id) {
        neosh.notify("session.unarchive needs a conversation id", "warn");
        return;
      }
      await setArchived(neosh, id, false);
    }, { desc: "Bring an archived conversation back" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.archived", async () => {
      await browseArchive(neosh);
      await w.draw();
    }, { desc: "Browse what you have archived" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("project.open", async (args) => {
      // Without this, a second project only ever appears by way of a worktree — which makes the
      // whole panel a thread list with extra steps.
      const path = args[0] ?? (await chooseDirectory(neosh));
      if (path === null || path.trim() === "") return;
      try {
        await neosh.session.create({ cwd: path.trim() });
      } catch (e) {
        // Almost always a path that is not there. The host's message names it, which is more
        // useful than anything this plugin could invent.
        neosh.notify(String(e), "warn");
      }
    }, { desc: "Add a project — start a conversation in another directory" }),
  );

  await neosh.keymap.set("chat", "<C-b>", "sidebar.toggle", { desc: "Toggle the sidebar" });
  await neosh.keymap.set("chat", "<C-t>", "sidebar.focus", { desc: "Projects and conversations" });
  await neosh.keymap.set("chat", "<C-n>", "session.new", { desc: "New conversation" });
  await neosh.keymap.set("chat", "<C-o>", "project.open", { desc: "Add a project" });
  // The archive is a place you go, not a section you scroll past. It has a key of its own so that
  // taking it out of the panel does not make it something you have to remember a command for.
  await neosh.keymap.set("chat", "<C-f>", "session.archived", {
    desc: "Archived conversations",
  });

  // Watching a conversation on another machine.
  //
  // Not "switching" — there is nothing here to switch to, because the agent belongs to the computer
  // it was started on and always will. Subscribing is what makes it read like a local one: history
  // first, then everything as it happens.
  w.subscriptions.push(
    await neosh.cmd.register("swarm.open", async (args) => {
      const [node, session] = args;
      if (!node || !session) {
        neosh.notify("swarm.open needs a node and a conversation", "warn");
        return;
      }
      try {
        await neosh.swarm.subscribe(node, session);
        const who = (await neosh.swarm.nodes().catch(() => []))
          .find((n) => n.info.id === node)?.info.name ?? node.slice(0, 8);
        neosh.notify(`watching a conversation on ${who}`);
      } catch (e) {
        neosh.notify(String(e), "warn");
      }
    }, { desc: "Watch a conversation on another computer" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("swarm.nodes", () => showNodes(neosh), {
      desc: "The computers in this workspace",
    }),
  );
  // Any change over there is a redraw here. Without it the panel is right only as often as its
  // four-second refresh, which is a long time to watch a spinner that has already stopped.
  w.subscriptions.push(neosh.swarm.onChange(() => void w.draw()));

  await neosh.hint.set("sessions", { keys: "^T", label: "conversations", priority: 20 });
  await neosh.hint.set("new", { keys: "^N", label: "new", priority: 21 });
}

/**
 * Bind the verbs other plugins contributed, and rebind them when the set changes.
 *
 * The panel binds these rather than the contributor doing it, and that is deliberate: a key press
 * carries no arguments, so a plugin that bound `d` itself would have no way to learn which row the
 * cursor was on except by tracking every move — one race away from acting on the wrong
 * conversation. Going through here, the command is invoked with the row as arguments and the
 * contributor never has to know the panel has a cursor at all.
 *
 * They are real bindings all the same. `F1` lists them with the contributed label, `^K` runs the
 * command, and a user who dislikes the key rebinds it exactly as they would one of ours.
 */
function installActions(
  neosh: Neosh,
  list: CursoredList<Target>,
  onChange: () => void,
): { actions: () => ActionItem[]; dispose: Disposable } {
  const scope = { kind: "buf_kind", name: KIND } as const;
  let current: Array<Contribution & { item: ActionItem }> = [];
  let bound: Array<{ key: string; command: string }> = [];
  let registered: Disposable[] = [];

  const sync = async () => {
    const got = await neosh.ext.list<ActionItem>(POINT_ACTION).catch(() => []);
    const valid = got.filter((c) =>
      typeof c.item?.key === "string" && typeof c.item?.command === "string"
    );

    // Everything from the previous round goes first. An action that was withdrawn must take its key
    // *and* its command with it, or the panel keeps a binding pointing at a wrapper around a
    // command whose plugin has gone.
    for (const b of bound) await neosh.keymap.del("chat", b.key, scope).catch(() => {});
    for (const d of registered) d.dispose();
    bound = [];
    registered = [];

    for (const c of valid) {
      const name = `${NS}.action.${c.plugin}.${c.id}`;
      const command = await neosh.cmd.register(name, async () => {
        const target = list.value;
        if (!applies(c.item.on ?? "any", target)) return;
        await neosh.cmd.exec(c.item.command, argsFor(target)).catch((e: unknown) => {
          neosh.notify(String(e), "warn");
        });
        onChange();
      }, { desc: c.item.label }).catch(() => null);
      if (command) registered.push(command);
      // A contributed key does not get to take one of ours. `keymap.set` already refuses to let a
      // *bundled* default overwrite somebody's choice, but here the bundled plugin is the one doing
      // the binding on a third party's behalf, so that rule points the wrong way and the check is
      // ours to make. The command stays registered either way — `^K` still runs it, and the
      // contributor is told which key it did not get.
      if (RESERVED.has(c.item.key)) {
        neosh.log.warn(
          `${c.plugin} asked for '${c.item.key}' in the sidebar, which is already a panel key`,
        );
        continue;
      }
      await neosh.keymap.set("chat", c.item.key, name, { scope, desc: c.item.label })
        .catch(() => {});
      bound.push({ key: c.item.key, command: name });
    }
    current = valid;
    onChange();
  };

  void sync();
  const sub = neosh.ext.onChange((e) => {
    if (e.point === POINT_ACTION) void sync();
  });

  return {
    actions: () => current.map((c) => c.item),
    dispose: {
      dispose() {
        sub.dispose();
        for (const b of bound) void neosh.keymap.del("chat", b.key, scope).catch(() => {});
        for (const d of registered) d.dispose();
      },
    },
  };
}

/** The keys this panel has already spoken for. A contribution asking for one of these is a bug in it. */
const RESERVED = new Set([
  "j", "k", "q", "f", "J", "K", "r", "x", "X", "n", "a", "o", "?", " ",
  "<Esc>", "<CR>", "<Up>", "<Down>", "<Space>", "<C-n>", "<C-p>", "<C-c>",
]);

function applies(on: "project" | "session" | "any", target: Target | undefined): boolean {
  if (on === "any") return target !== undefined;
  return target?.kind === on;
}

/** The row under the cursor, as arguments a command can act on. */
function argsFor(target: Target | undefined): string[] {
  if (!target) return [];
  if (target.kind === "session") return ["session", target.cwd, target.id];
  if (target.kind === "project") return ["project", target.cwd];
  return [target.kind];
}

/**
 * Start a conversation, asking where when there is something to ask.
 *
 * A conversation belongs to a directory — that is what makes the sidebar a list of projects rather
 * than a list of threads — and in a git repository "where" has a real answer that is not always
 * "here". A branch you want to work on without disturbing what is checked out is a *worktree*, and
 * having to create one, find it, and then open a conversation in it is three steps for one
 * intention.
 *
 * Outside a repository the question has one answer, so it is not asked: `^N` stays a single key
 * everywhere the choice would be theatre. Inside one, `Here` is selected, so `^N ⏎` is what `^N`
 * always did.
 */
async function newConversation(neosh: Neosh): Promise<void> {
  const current = await neosh.session.current().catch(() => null);
  const here = current?.cwd;
  // Empty outside a repository, which is also how this knows there is nothing to ask about: a
  // worktree is a git idea, and a directory that is not a checkout has exactly one answer to
  // "where does this conversation go".
  const trees = await neosh.git.worktrees().catch(() => [] as WorktreeInfo[]);
  // The git plugin may also be switched off, in which case the rows that lead into it would lead
  // nowhere. Offering an action that cannot happen is worse than not offering it.
  const commands = new Set((await neosh.cmd.list().catch(() => [])).map((c) => c.name));
  const canBranch = trees.length > 0 && commands.has("git.worktree.new");
  const others = trees.filter((t) => t.path !== here);

  if (!canBranch && others.length === 0) {
    await neosh.session.create();
    return;
  }

  type Where =
    | { kind: "here" }
    | { kind: "new" }
    | { kind: "elsewhere" }
    | { kind: "tree"; path: string; label: string }
    /** On another computer, in a checkout that machine told us it has. */
    | { kind: "host"; node: string; cwd: string; label: string };
  const rows: Array<{ label: string; detail?: string; keywords?: string; value: Where }> = [
    { label: "Here", detail: here ?? "this project", value: { kind: "here" } },
  ];
  if (canBranch) {
    rows.push({
      label: "In a new worktree…",
      detail: "a branch of its own, checked out somewhere else",
      keywords: "branch worktree",
      value: { kind: "new" },
    });
  }
  for (const t of others) {
    const label = t.branch ?? t.head ?? t.path;
    rows.push({
      label,
      detail: `worktree  ·  ${t.path}`,
      keywords: `${t.path} worktree`,
      value: { kind: "tree", path: t.path, label },
    });
  }
  // The other computers, and the checkouts each of them offered in its handshake. A machine that
  // does not accept commands is left out rather than shown and refused — a row that cannot work is
  // worse than no row.
  for (const n of await neosh.swarm.nodes().catch(() => [])) {
    if (!n.up || !n.capabilities.accepts_commands) continue;
    for (const project of n.capabilities.projects) {
      rows.push({
        label: `${project.name}`,
        detail: `on ${n.info.name}  ·  ${project.cwd}`,
        keywords: `${n.info.name} ${project.cwd} remote host computer`,
        value: {
          kind: "host",
          node: n.info.id,
          cwd: project.cwd,
          label: `${project.name} on ${n.info.name}`,
        },
      });
    }
  }

  rows.push({
    label: "Another directory…",
    detail: "somewhere else entirely",
    keywords: "project folder",
    value: { kind: "elsewhere" },
  });

  const chosen = await picker(neosh, rows, { title: "New conversation", width: 76 });
  if (chosen === null) return;
  switch (chosen.kind) {
    case "here":
      await neosh.session.create();
      return;
    case "new":
      await neosh.cmd.exec("git.worktree.new").catch((e: unknown) => neosh.notify(String(e), "warn"));
      return;
    case "tree":
      await neosh.session.create({ cwd: chosen.path });
      neosh.notify(`new conversation in ${chosen.label}`);
      return;
    case "host":
      // Started *there*. The agent will run on that machine, against that machine's files, which
      // is the point — this is not a way to work on a remote directory from here.
      try {
        await neosh.swarm.command(chosen.node, "", {
          command: "new_session",
          cwd: chosen.cwd,
          title: null,
        });
        neosh.notify(`started ${chosen.label}`);
      } catch (e) {
        neosh.notify(String(e), "warn");
      }
      return;
    case "elsewhere":
      await neosh.cmd.exec("project.open").catch(() => {});
  }
}

async function activateTarget(
  neosh: Neosh,
  arrangement: Arrangement,
  target: Target | undefined,
  w: Wiring,
): Promise<void> {
  if (!target) return;
  // Somebody else's row. Nothing here knows what it does, which is the point — the contribution
  // named a command and this invokes it.
  if (target.kind === "custom") {
    if (!target.command) return;
    await neosh.cmd.exec(target.command, target.args).catch((e: unknown) => {
      neosh.notify(String(e), "warn");
    });
    await w.draw();
    return;
  }
  // A conversation on another computer. There is nothing to switch to here — it belongs to that
  // machine — so opening it means watching it, which is what `swarm.open` does.
  if (target.kind === "remote") {
    await neosh.cmd.exec("swarm.open", [target.node, target.session]).catch((e: unknown) => {
      neosh.notify(String(e), "warn");
    });
    return;
  }
  if (target.kind === "add") {
    await w.leave();
    await neosh.cmd.exec("project.open").catch(() => {});
    return;
  }
  if (target.kind === "browse") {
    await browseArchive(neosh);
    await w.draw();
    return;
  }
  if (target.kind === "project") {
    // A pinned project with nothing in it yet: `↵` should start something, not fold an empty list.
    const sessions = await neosh.session.list().catch(() => [] as SessionInfo[]);
    if (!sessions.some((s) => s.cwd === target.cwd)) {
      await neosh.session.create({ cwd: target.cwd });
      await w.leave();
      return;
    }
    await arrangement.toggleFold(target.cwd);
    await w.draw();
    return;
  }
  try {
    await neosh.session.switch(target.id);
  } catch (e) {
    neosh.notify(String(e), "warn");
    return;
  }
  await w.leave();
}

/** The project a row belongs to — its own, or the one its conversation is in. */
function owningProject(target: Target | undefined): string | null {
  if (!target) return null;
  return target.kind === "project" || target.kind === "session" ? target.cwd : null;
}

/**
 * Which directory to add.
 *
 * Offers the worktrees of the repository you are in before asking you to type, because that is
 * where the next project usually is and a path typed from memory is a path typed wrong. The
 * runtime has no filesystem, so there is no directory browser to offer — the last entry drops
 * through to a text field, which the host validates.
 */
async function chooseDirectory(neosh: Neosh): Promise<string | null> {
  const [worktrees, sessions] = await Promise.all([
    neosh.git.worktrees().catch(() => []),
    neosh.session.list().catch(() => [] as SessionInfo[]),
  ]);
  const open = new Set(sessions.map((s) => s.cwd));
  const candidates = worktrees.filter((t) => !open.has(t.path));
  if (candidates.length === 0) return pathPicker(neosh, "Add project");

  const TYPE = "\u0000type";
  const chosen = await picker(
    neosh,
    [
      ...candidates.map((t) => ({
        label: basename(t.path),
        detail: [t.branch ?? "", t.path].filter(Boolean).join("  ·  "),
        keywords: t.path,
        value: t.path,
      })),
      { label: "Type a path…", value: TYPE },
    ],
    { title: "Add project", width: 76 },
  );
  if (chosen === null) return null;
  return chosen === TYPE ? pathPicker(neosh, "Add project") : chosen;
}

/**
 * Put a conversation away, or bring it back.
 *
 * Asks nothing, on purpose. A confirmation is the price of an irreversible action, and this one is
 * reversible by one key from the archive — charging for it would teach you to dismiss dialogs,
 * which is exactly the habit that makes the delete dialog useless.
 */
async function setArchived(neosh: Neosh, session: string, archived: boolean): Promise<void> {
  try {
    await neosh.session.archive(session, archived);
    neosh.notify(archived ? "archived — `a` in the panel finds it" : "unarchived");
  } catch (e) {
    neosh.notify(String(e), "warn");
  }
}

/**
 * Delete a conversation, having asked. Answers whether it went.
 *
 * The file goes from disk and there is no undo, so this always stops and asks — including for a
 * conversation with nothing in it yet. It used to skip the question there, on the grounds that a
 * dialog for something with nothing to lose is friction; what that actually bought was a key whose
 * behaviour depended on state you cannot see from the row, so the one time it did ask was the one
 * time your fingers were already through it.
 *
 * `ui.confirm_destructive = false` is still the way out, and it is a setting rather than a guess.
 */
async function deleteSession(neosh: Neosh, session: string): Promise<boolean> {
  const info = (await neosh.session.list({ includeArchived: true }).catch(() => [] as SessionInfo[]))
    .find((s) => s.id === session);
  const count = info?.message_count ?? 0;
  const detail = [
    count === 0
      ? "Nothing has been said in it yet."
      : `${count} ${count === 1 ? "message" : "messages"}, in ${info?.project || basename(info?.cwd ?? "")}.`,
    info?.archived
      ? "It is archived, so leaving it here costs nothing."
      : "Archiving keeps every word of it and takes it out of your list.",
  ];
  const ok = await confirmDestructive(
    neosh,
    `Delete "${clip(info?.label ?? "this conversation", 48)}"?`,
    { yes: "Delete", no: "Keep", detail },
  );
  if (!ok) return false;
  try {
    await neosh.session.close(session);
    return true;
  } catch (e) {
    neosh.notify(String(e), "warn");
    return false;
  }
}

/**
 * What you have put away.
 *
 * A place you go rather than a section you scroll past. The archive used to be four dim rows at the
 * foot of the panel, which is the worst of both: in the way of the list you work in, and too small
 * to actually find anything in once there were twenty of them. As a picker it filters, it says which
 * project each one came from, and the verbs that only make sense here — put it back, or finally
 * throw it away — live on keys here instead of on the everyday list.
 *
 * `↵` restores *and* opens, because opening something you put away is a statement that you want it
 * back: leaving it archived while you work in it would mean the list you look at does not contain
 * the conversation you are in.
 */
async function browseArchive(neosh: Neosh): Promise<void> {
  const rows = async (): Promise<PickerItem<SessionInfo>[]> => {
    const all = await neosh.session
      .list({ includeArchived: true })
      .catch(() => [] as SessionInfo[]);
    const now = Date.now() / 1000;
    return all
      .filter((s) => s.archived)
      // Most recently put away first: the one you want back is usually the one you last regretted.
      .sort((a, b) => (b.archived_at ?? b.updated_at) - (a.archived_at ?? a.updated_at))
      .map((s) => {
        const count = s.message_count;
        const when = ago(now - (s.archived_at ?? s.updated_at));
        return {
          label: clip(s.label, 44),
          detail: [
            s.project || basename(s.cwd),
            `${count} ${count === 1 ? "message" : "messages"}`,
            when === "" ? "" : `archived ${when}`,
          ].filter((p) => p !== "").join("  ·  "),
          // Filtered on but not shown: you look for a conversation by what it was about or which
          // checkout it was in, and the path is the half of that the label never carries.
          keywords: s.cwd,
          value: s,
        };
      });
  };

  const items = await rows();
  if (items.length === 0) {
    neosh.notify("nothing is archived — `x` on a conversation puts it here", "info");
    return;
  }
  // Mutated in place rather than rebuilt, because the picker ranks the array it was handed: acting
  // on a row and saying `reload` is how the list catches up without losing your place in it.
  const refill = async () => {
    items.splice(0, items.length, ...(await rows()));
  };

  const chosen = await picker(neosh, items, {
    title: "Archived conversations",
    width: 82,
    height: 12,
    placeholder: "nothing archived matches that",
    hints: "↵ restore   ^U put back   ^X delete   esc close",
    // Chords, because every bare letter this takes is a letter its filter can never contain — and
    // both of these are bound elsewhere, so they have to be claimed to arrive at all.
    ownKeys: ["<C-u>", "<C-x>"],
    async onKey(key, ctx) {
      if (key.key.code.kind !== "char" || !key.key.mods.ctrl || key.key.mods.alt) return;
      const s = ctx.item;
      if (!s) return "handled";
      switch (key.key.code.c.toLowerCase()) {
        // Back in the list, without going there. The difference from `↵`: you are tidying, not
        // switching, and being thrown into a conversation per row you restore is not tidying.
        case "u":
          await setArchived(neosh, s.id, false);
          await refill();
          return items.length === 0 ? "close" : "reload";
        case "x": {
          if (!(await deleteSession(neosh, s.id))) return "handled";
          await refill();
          return items.length === 0 ? "close" : "reload";
        }
        default:
          return;
      }
    },
  });
  if (chosen === null) return;

  try {
    await neosh.session.archive(chosen.id, false);
    await neosh.session.switch(chosen.id);
    neosh.notify(`restored "${clip(chosen.label, 40)}"`);
  } catch (e) {
    neosh.notify(String(e), "warn");
  }
}

/**
 * The computers in this workspace, and what each is running.
 *
 * A picker rather than a panel section, for the reason the archive is: it is a thing you look at
 * when you want it, and rows that are only occasionally the answer do not belong in the column you
 * work in. `↵` on a machine watches its most recent conversation.
 */
async function showNodes(neosh: Neosh): Promise<void> {
  const nodes = await neosh.swarm.nodes().catch(() => []);
  const me = await neosh.swarm.self().catch(() => null);
  if (nodes.length === 0) {
    neosh.notify(
      me ? "no other computers have joined yet" : "the swarm is off — see `[swarm]` in your config",
      "info",
    );
    return;
  }

  const chosen = await picker(
    neosh,
    nodes.map((n) => {
      const running = n.agents.filter((a) => a.state === "running").length;
      return {
        label: n.info.name,
        detail: [
          n.up ? `${n.agents.length} conversations` : `unreachable — ${n.reason ?? "no answer"}`,
          running > 0 ? `${running} working` : "",
          n.info.os,
          // The short id, so two machines with the same hostname are still tellable apart.
          n.info.id.slice(0, 8),
        ].filter(Boolean).join("  ·  "),
        keywords: `${n.info.os} ${n.info.id}`,
        value: n,
      };
    }),
    { title: "Computers", width: 76 },
  );
  if (chosen === null) return;
  const newest = [...chosen.agents].sort((a, b) => b.updated_at - a.updated_at)[0];
  if (!newest) {
    neosh.notify(`${chosen.info.name} has nothing running`, "info");
    return;
  }
  await neosh.cmd.exec("swarm.open", [chosen.info.id, newest.session]).catch(() => {});
}

async function renameSession(neosh: Neosh, session: string): Promise<void> {
  const current = (await neosh.session.list()).find((s) => s.id === session);
  const next = await prompt(neosh, "Rename conversation", {
    initial: current?.title ?? "",
    width: 60,
  });
  if (next === null) return;
  await neosh.session.rename(session, next.trim() === "" ? null : next);
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

/**
 * Projects, as two ordered groups: pinned, then the rest.
 *
 * A project is a directory some conversation lives in — there is no registry to keep in sync, and
 * opening a conversation in another checkout is what makes a second project appear. Pinned
 * directories are seeded even with nothing in them, so unpinning is the only way a favourite
 * disappears.
 */
function group(
  sessions: SessionInfo[],
  arrangement: Arrangement,
  keys: Map<string, string>,
): Project[][] {
  const by = new Map<string, SessionInfo[]>();
  for (const cwd of arrangement.pinned()) by.set(cwd, []);
  for (const s of sessions) {
    const existing = by.get(s.cwd);
    if (existing) existing.push(s);
    else by.set(s.cwd, [s]);
  }

  // `session.list()` is most-recently-used first, so index in it is recency. Anything you have
  // never dragged sorts by that, after everything you have.
  const recency = new Map<string, number>();
  sessions.forEach((s, i) => {
    if (!recency.has(s.cwd)) recency.set(s.cwd, i);
  });

  const projects: Project[] = [...by.entries()].map(([cwd, list]) => ({
    cwd,
    // What the host decided to call it. For a worktree that is the repository and the branch,
    // rather than whatever the directory happens to be named — a row reading `wt-fe3c0d93`
    // tells you nothing about which checkout it is, and reads as somebody else's directory having
    // wandered into your workspace.
    name: list[0]?.project || basename(cwd),
    favorite: arrangement.isFavorite(cwd),
    sessions: list,
    key: keys.get(cwd) ?? `dir:${basename(cwd)}`,
  }));

  const sort = (a: Project, b: Project) => {
    const ra = arrangement.rank(a.cwd);
    const rb = arrangement.rank(b.cwd);
    if (ra !== rb) return ra - rb;
    const fa = recency.get(a.cwd) ?? Number.MAX_SAFE_INTEGER;
    const fb = recency.get(b.cwd) ?? Number.MAX_SAFE_INTEGER;
    return fa !== fb ? fa - fb : a.name.localeCompare(b.name);
  };

  return [
    projects.filter((p) => p.favorite).sort(sort),
    projects.filter((p) => !p.favorite).sort(sort),
  ];
}

interface DrawOptions {
  width: number;
  ascii: boolean;
  hints: boolean;
  focused: boolean;
  selected: Target | undefined;
  /** The verbs other plugins put on our rows, for the hint strip. */
  actions: ActionItem[];
  /** Conversations on other computers, grouped by the project key they share with ours. */
  remote: Map<string, SwarmAgent[]>;
  /** Which other machines have each project, by key. */
  hosts: Map<string, string[]>;
}

async function collect(
  neosh: Neosh,
  arrangement: Arrangement,
  opts: DrawOptions,
): Promise<{ rows: ListRow<Target>[]; running: boolean }> {
  const rows: ListRow<Target>[] = [];

  // Failures absorbed: a panel that renders an exception instead of your conversations is worse
  // than one showing less.
  const all = await neosh.session
    .list({ includeArchived: true })
    .catch(() => [] as SessionInfo[]);
  const sessions = all.filter((s) => !s.archived);
  const archived = all.filter((s) => s.archived);
  const running = sessions.some((s) => s.active_turn);
  const now = Date.now();
  // One list, favourites first. A separate `FAVORITES` section splits a short list in half and
  // makes you check two places for the same kind of thing; the heart says which is which without
  // costing a heading, a rule and a blank line.
  // What the other computers are running. One call; empty and harmless on a single machine.
  const swarm = await neosh.swarm.agents().catch(() => [] as SwarmAgent[]);
  const remote = new Map<string, SwarmAgent[]>();
  const hosts = new Map<string, string[]>();
  for (const r of swarm) {
    const list = remote.get(r.agent.project) ?? [];
    list.push(r);
    remote.set(r.agent.project, list);
    const names = hosts.get(r.agent.project) ?? [];
    if (!names.includes(r.node.name)) names.push(r.node.name);
    hosts.set(r.agent.project, names.sort());
  }
  // A local conversation tells us its project key indirectly: the host stamps the same key on both
  // sides, so a cwd here and a cwd there meet on the key rather than on the path.
  const keys = new Map<string, string>();
  for (const r of swarm) {
    if (!keys.has(r.agent.cwd)) keys.set(r.agent.cwd, r.agent.project);
  }
  opts = { ...opts, remote, hosts };
  const projects = group(sessions, arrangement, keys).flat();

  // A directory that turned up in the conversation list and we had not seen before. Noted rather
  // than fetched inline: the draw runs on a tick and must not wait on a round trip per project.
  void arrangement.note(all.map((s) => s.cwd));

  // Rows other plugins own. Read every frame rather than cached, because a contribution is replaced
  // in place when its author's data changes and re-reading is one call.
  const sections = await neosh.ext.list<SectionItem>(POINT_SECTION).catch(() => []);
  rows.push(...sectionRows(sections, "above", opts));

  rows.push(...heading("PROJECTS", opts.width));
  for (const p of projects) {
    rows.push(projectRow(p, arrangement, opts, now));
    if (arrangement.isFolded(p.cwd)) continue;
    for (const s of p.sessions) rows.push(sessionRow(s, now, opts));
    // The same project, being worked on elsewhere. Under the same heading rather than in a section
    // of their own: they are not a different kind of thing, they are the same work on a different
    // computer, and a separate `REMOTE` block would make you check two places for one project.
    for (const r of remote.get(p.key) ?? []) rows.push(remoteRow(r, opts, now));
    if (p.sessions.length === 0 && (remote.get(p.key) ?? []).length === 0) {
      rows.push({ text: "       nothing here yet", hl: "Sidebar.Dim", inert: true });
    }
  }

  // Projects that exist only on other machines. Without these, a repository you have not cloned
  // here is invisible — and "which computers is this on" cannot answer "not this one".
  for (const [key, list] of remote) {
    if (projects.some((p) => p.key === key)) continue;
    const first = list[0];
    if (!first) continue;
    rows.push({
      text: `   ${opts.ascii ? "~" : "▹"} ${clip(first.agent.project_name, opts.width - 12)}`,
      hl: "Sidebar.Remote",
      right: { text: `${clip((hosts.get(key) ?? []).join(" "), 12)} `, hl: "Sidebar.Remote" },
      inert: true,
    });
    for (const r of list) rows.push(remoteRow(r, opts, now));
  }

  rows.push(blank());
  rows.push({
    text: "   + Add project",
    hl: "Accent",
    value: { kind: "add" },
  });

  // One row, and only while there is something in it. What used to be here was a foldable section
  // with every archived conversation under it — which put the things you have finished with in the
  // same column as the things you are working on, and grew without limit. The point of archiving is
  // that it goes away; a door is the most this panel should spend on saying where.
  if (archived.length > 0) {
    rows.push({
      text: `   ${opts.ascii ? "-" : "┈"} Archived`,
      hl: "Sidebar.Dim",
      right: { text: `${archived.length} `, hl: "Sidebar.Dim" },
      value: { kind: "browse", count: archived.length },
    });
  }

  rows.push(...sectionRows(sections, "below", opts));

  if (opts.hints) rows.push(...hints(opts));

  return { rows, running };
}

/**
 * Rows somebody else contributed, in the half of the column they asked for.
 *
 * Every field is checked rather than trusted. A contribution is JSON from a plugin this one has
 * never heard of, and a panel that throws on a missing `text` is a panel a third party can break by
 * getting one row wrong — which would make contributing feel like a risk rather than the ordinary
 * way to add something.
 */
function sectionRows(
  sections: Array<Contribution & { item: SectionItem }>,
  at: "above" | "below",
  opts: DrawOptions,
): ListRow<Target>[] {
  const rows: ListRow<Target>[] = [];
  for (const c of sections) {
    if ((c.item?.at ?? "below") !== at) continue;
    const contributed = Array.isArray(c.item?.rows) ? c.item.rows : [];
    if (contributed.length === 0 && !c.item?.title) continue;

    rows.push(blank());
    if (typeof c.item.title === "string" && c.item.title !== "") {
      rows.push(...heading(clip(c.item.title, opts.width - 2), opts.width));
    }
    for (const r of contributed) {
      if (typeof r?.text !== "string") continue;
      rows.push({
        text: `   ${clip(r.text, Math.max(4, opts.width - 6))}`,
        hl: typeof r.hl === "string" ? r.hl : undefined,
        right: typeof r.right?.text === "string"
          ? { text: `${r.right.text} `, hl: r.right.hl }
          : undefined,
        // No command means a label. A row you can land on that does nothing when you press `↵` is
        // worse than one the cursor skips.
        inert: typeof r.command !== "string",
        value: typeof r.command === "string"
          ? { kind: "custom", command: r.command, args: strings(r.args) }
          : undefined,
      });
    }
  }
  return rows;
}

/**
 * A section heading, with a rule under it.
 *
 * The rule is what turns a column of text into sections. Without it every row reads at the same
 * weight and the eye has to parse the words to find the structure — which is exactly the work a
 * layout is supposed to have already done.
 */
function heading(text: string, width: number): ListRow<Target>[] {
  return [
    { text: ` ${text}`, hl: "Sidebar.Heading", inert: true },
    { text: "─".repeat(Math.max(1, width)), hl: "Separator", inert: true },
  ];
}

function blank(): ListRow<Target> {
  return { text: "", inert: true };
}

function projectRow(
  p: Project,
  arrangement: Arrangement,
  opts: DrawOptions,
  now: number,
): ListRow<Target> {
  const folded = arrangement.isFolded(p.cwd);
  const arrow = opts.ascii ? (folded ? ">" : "v") : folded ? "▸" : "▾";
  const busy = p.sessions.find((s) => s.active_turn);
  const here = p.sessions.some((s) => s.is_active);

  // The count is the useful thing when a project is folded, and the elapsed time is the useful
  // thing when something inside it is working. Never both — there is one column.
  const right = busy
    ? { text: `${turnFor(busy, now)} `, hl: "Status.Working" }
    : p.sessions.length > 0
      ? { text: `${p.sessions.length} `, hl: "Sidebar.Dim" }
      : { text: "" };

  // The heart sits before the fold arrow rather than after the name: it is what you scan the
  // column for, and a marker at the ragged right edge is not a column. It gets its own highlight
  // because a heart is red — a grey one reads as a heart that has stopped.
  const heart = p.favorite ? (opts.ascii ? "*" : "♥") : " ";

  // Which other computers have this project. The whole reason a project key is a normalised git
  // remote rather than a path: on two machines the path is different and this is the same.
  const elsewhere = opts.hosts.get(p.key) ?? [];
  const name = clip(p.name, Math.max(6, opts.width - 8 - (elsewhere.length ? 8 : 0)));
  return {
    text: ` ${heart} ${arrow} ${name}`,
    hl: here ? "Directory" : "Sidebar.Dim",
    spans: p.favorite ? [{ from: 1, to: 1 + byteLength(heart), hl: "Sidebar.Favorite" }] : undefined,
    right: elsewhere.length > 0
      // The machines take the column the count would have used. A project that is in two places is
      // a more useful thing to know than how many conversations are in it here.
      ? { text: `${clip(elsewhere.join(" "), 14)} `, hl: "Sidebar.Remote" }
      : right,
    value: { kind: "project", cwd: p.cwd },
  };
}

/**
 * A conversation on another computer.
 *
 * Indented with its project's own, because that is the claim: it is the same project, being worked
 * on somewhere else. The host name is what makes it honest — it reads as one list and says, per
 * row, which machine the work is actually happening on.
 */
function remoteRow(r: SwarmAgent, opts: DrawOptions, now: number): ListRow<Target> {
    const working = r.agent.state === "running";
    const glyph = working ? (opts.ascii ? "*" : "◍") : opts.ascii ? "." : "·";
    const host = clip(r.node.name, 12);
    const width = Math.max(8, opts.width - 11 - host.length);
    return {
      text: `     ${glyph} ${clip(r.agent.label, width)}`,
      hl: working ? "Status.Monitoring" : "Sidebar.Remote",
      right: { text: `${host} `, hl: "Sidebar.Remote" },
      value: {
        kind: "remote",
        node: r.node.id,
        session: r.agent.session,
        cwd: r.agent.cwd,
        host: r.node.name,
      },
    };
}

function sessionRow(s: SessionInfo, now: number, opts: DrawOptions): ListRow<Target> {
  // Working is the only state that moves, and only where the work is. An idle row's status is
  // deliberately static: twenty animating rows carry no more information than one and cost twenty
  // times the attention.
  const working = Boolean(s.active_turn);
  const glyph = working
    ? s.is_active
      ? spinnerFrame()
      : opts.ascii ? "*" : "◍"
    : s.is_active
      ? opts.ascii ? ">" : "▸"
      : " ";
  const right = working ? turnFor(s, now) : s.updated_at > 0 ? ago(now / 1000 - s.updated_at) : "";
  return {
    text: `     ${glyph} ${clip(s.label, Math.max(8, opts.width - 9 - right.length))}`,
    hl: working
      ? s.is_active && pulseBright() ? "Status.Working" : "Status.Monitoring"
      : s.is_active
        ? "Accent"
        : undefined,
    // A trailing space so the age does not sit flush against the panel's edge rule.
    right: {
      text: right === "" ? "" : `${right} `,
      hl: working ? "Status.Working" : "Sidebar.Dim",
    },
    value: { kind: "session", id: s.id, cwd: s.cwd },
  };
}

/**
 * How long the running turn has been running.
 *
 * A conversation that says it is working without saying for how long is indistinguishable from one
 * that is wedged, which is the moment you reach for `^C` and lose the answer. The host stamps the
 * start, so this survives the panel being closed and reopened mid-turn — a timer counted in here
 * would restart at zero and quietly lie.
 */
function turnFor(s: SessionInfo, now: number): string {
  const started = s.turn_started_at;
  if (!started || started <= 0) return "…";
  return elapsed(Math.max(0, now - started * 1000));
}

/**
 * The keys for whatever the cursor is on.
 *
 * Contextual rather than a fixed cheat sheet: `x` closes a conversation and means nothing on a
 * project, and a list of verbs that do not all apply is a list you learn to distrust. Everything
 * here is also reachable from `?`, which is the escape hatch when the panel is too narrow.
 */
function hints(opts: DrawOptions): ListRow<Target>[] {
  const kind = opts.selected?.kind;
  const lines = !opts.focused
    // `^O` is not here and `+ Add project` is a row you can see: a key strip has two lines, and the
    // verb with a row of its own is the one that can afford to give up its place on them.
    ? ["^T projects  ^N new   ^F archive", "^K palette   ^B hide  F1 keys"]
    : kind === "project"
      ? ["↵ fold   f ♥       JK move", "n new    a archive  ? keys"]
      : kind === "session"
        ? ["↵ open   r rename   x archive", "X delete a archive   ? keys"]
        : kind === "browse"
          ? ["↵ what you have put away", "? keys"]
          : ["↵ add project    a archive", "esc back         ? keys"];

  // Contributed verbs get their own line rather than being squeezed onto ours, because ours are
  // laid out in columns that a third party's label of unknown length would break — and because a
  // key nobody can see is a key nobody presses, which is the whole argument for this strip.
  const mine = opts.focused ? contributedHint(opts) : "";

  return [
    blank(),
    { text: "─".repeat(Math.max(1, opts.width)), hl: "Separator", inert: true },
    ...lines.map((text) => ({ text: ` ${text}`, hl: "Sidebar.Dim", inert: true })),
    ...(mine === "" ? [] : [{ text: ` ${mine}`, hl: "Sidebar.Dim", inert: true }]),
  ];
}

/** The contributed verbs that apply to the row under the cursor, clipped to the column. */
function contributedHint(opts: DrawOptions): string {
  const applicable = opts.actions.filter((a) => applies(a.on ?? "any", opts.selected));
  if (applicable.length === 0) return "";
  return clip(
    applicable.map((a) => `${a.key} ${a.label}`).join("   "),
    Math.max(4, opts.width - 2),
  );
}

/**
 * The always-visible numbers: how long the current turn has run, how full the window is, and what
 * it has cost.
 *
 * These live in the footer rather than the panel because the panel can be hidden and these are the
 * things you want while you are typing. They are also the reason the sidebar no longer carries a
 * usage block — one home per number.
 */
async function installFooter(neosh: Neosh, subscriptions: PluginContext["subscriptions"]) {
  const refresh = async () => {
    const [current, selection] = await Promise.all([
      neosh.session.current().catch(() => null),
      neosh.agent.selection().catch(() => null),
    ]);
    if (!current) return;

    if (current.active_turn) {
      await neosh.status.set("turn", {
        text: `${spinnerFrame()} ${turnFor(current, Date.now())}`,
        // The group that sweeps. Nothing here drives the animation — the frontend does, at its own
        // rate, which is why this can be set once per second and still look continuous.
        hl: "Status.Streaming",
        align: "right",
        priority: 5,
      });
    } else {
      await neosh.status.clear("turn");
    }

    const model = selection
      ? (await neosh.agent.listModels(selection.instance).catch(() => []))
          .find((e) => e.model.id === selection.model)?.model
      : undefined;
    const u = current.usage;
    // The context meter is *not* here. It was, and so was one in the usage plugin, and for as long
    // as no model in the catalogue reported a window only one of them ever drew — so two gauges of
    // the same thing, disagreeing about what "used" means, sat in the same footer the moment one
    // did. The usage plugin owns it: it is named for the job and it reads the context the last
    // request actually carried, rather than adding up totals that count a cached prompt twice.
    if (u.input_tokens + u.output_tokens === 0) {
      await neosh.status.clear("cost");
      return;
    }

    const p = model?.pricing;
    if (!p) return;
    // Cache reads are billed at their own rate, which is the whole reason to count them separately:
    // a session that looks expensive by token count is often cheap because most of it was a hit.
    const cost =
      (u.input_tokens * p.input_per_mtok +
        u.output_tokens * p.output_per_mtok +
        u.cache_read_tokens * p.cache_read_per_mtok +
        u.cache_write_tokens * p.cache_write_per_mtok) /
      1_000_000;
    await neosh.status.set("cost", {
      text: money(cost),
      hl: "Comment",
      align: "right",
      priority: 12,
    });
  };

  await refresh();
  subscriptions.push(neosh.agent.onTurnStart(() => void refresh()));
  subscriptions.push(neosh.agent.onTurnEnd(() => void refresh()));
  subscriptions.push(neosh.session.onChange(() => void refresh()));
  // The cost depends on the model's prices, so a switch changes it without a turn happening.
  subscriptions.push(neosh.agent.onSelectionChange(() => void refresh()));
  // The clock only ticks while something is running, so this costs nothing at rest.
  subscriptions.push(onTick(() => void refresh()));
}

function basename(cwd: string): string {
  return cwd.split("/").filter(Boolean).pop() ?? cwd;
}

function short(cwd: string): string {
  return basename(cwd);
}

function clip(s: string, n: number): string {
  const chars = Array.from(s);
  return chars.length <= n ? s : `${chars.slice(0, Math.max(1, n - 1)).join("")}…`;
}

function ago(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "";
  if (seconds < 60) return "now";
  return elapsed(seconds * 1000).replace(/ \d+s$/, "").replace(/ \d+m$/, "");
}
