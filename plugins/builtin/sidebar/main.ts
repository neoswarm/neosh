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
 * off, and a sidebar of your own loads after this one and wins.
 */

import { byteLength } from "@neosh/api";
import type { Disposable, Neosh, PluginContext, SessionInfo, WindowId, WorktreeInfo } from "@neosh/api";
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
  prompt,
  pulseBright,
  spinnerFrame,
} from "@neosh/api/ui";

/** What a row points at, so the cursor survives the list being rebuilt underneath it. */
type Target =
  | { kind: "project"; cwd: string }
  | { kind: "session"; id: string; cwd: string; archived: boolean }
  /** The row that opens another directory. A row rather than only a key, because a verb nobody
   * can see is a verb nobody uses. */
  | { kind: "add" }
  /** The heading of the archived section, which folds. Only present when there is one. */
  | { kind: "archive" };

/** A project as the panel thinks of it: a directory, and what is going on in it. */
interface Project {
  cwd: string;
  name: string;
  favorite: boolean;
  sessions: SessionInfo[];
}

const NS = "neosh.sidebar";

/** Persisted arrangement. Small enough that all three are read once and written on a keystroke. */
const KEY_FAVORITES = "favorites";
const KEY_ORDER = "order";
const KEY_FOLDED = "folded";
const KEY_ARCHIVE_OPEN = "archiveOpen";

export async function activate({ neosh, subscriptions }: PluginContext) {
  await declareOptions(neosh);

  const applyMotion = async () => {
    configureMotion({
      enabled: (await neosh.opt.get<boolean>("ui.motion")) ?? true,
      ascii: (await neosh.opt.get<boolean>("ui.ascii_only")) ?? false,
    });
  };
  await applyMotion();

  // How you left it. Read once; every later write goes to memory and to disk together, so a redraw
  // never waits on the filesystem.
  const arrangement = new Arrangement(neosh);
  await arrangement.load();

  const buf = await neosh.buf.create({ name: "[sidebar]", scratch: true });
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
 * Kept in plugin state rather than in options on purpose: an option is configuration *you* write,
 * and an editor that rewrites your config file because you pressed `f` is one you stop trusting
 * with the file.
 */
class Arrangement {
  private favorites: string[] = [];
  private order: string[] = [];
  private folded = new Set<string>();
  /** The archive starts shut. It is where things go to stop being in your way. */
  private archiveOpen = false;

  constructor(private readonly neosh: Neosh) {}

  async load(): Promise<void> {
    const [favorites, order, folded, archiveOpen] = await Promise.all([
      this.neosh.state.get<string[]>(KEY_FAVORITES),
      this.neosh.state.get<string[]>(KEY_ORDER),
      this.neosh.state.get<string[]>(KEY_FOLDED),
      this.neosh.state.get<boolean>(KEY_ARCHIVE_OPEN),
    ]);
    this.favorites = strings(favorites);
    this.order = strings(order);
    this.folded = new Set(strings(folded));
    this.archiveOpen = archiveOpen === true;
  }

  isArchiveOpen(): boolean {
    return this.archiveOpen;
  }

  async toggleArchive(): Promise<void> {
    this.archiveOpen = !this.archiveOpen;
    await this.neosh.state.set(KEY_ARCHIVE_OPEN, this.archiveOpen);
  }

  /** Open the archive without a keystroke, so unarchiving from elsewhere is visible. */
  openArchive(): void {
    if (this.archiveOpen) return;
    this.archiveOpen = true;
    void this.neosh.state.set(KEY_ARCHIVE_OPEN, true);
  }

  /** Pinned directories, in the order they should appear. Also the seed for projects with no conversations. */
  pinned(): string[] {
    return [...this.favorites];
  }

  isFavorite(cwd: string): boolean {
    return this.favorites.includes(cwd);
  }

  isFolded(cwd: string): boolean {
    return this.folded.has(cwd);
  }

  /** Where a project sorts. Anything you have never moved sorts after everything you have. */
  rank(cwd: string): number {
    const i = this.order.indexOf(cwd);
    return i < 0 ? Number.MAX_SAFE_INTEGER : i;
  }

  async toggleFavorite(cwd: string): Promise<boolean> {
    const now = this.favorites.includes(cwd);
    this.favorites = now ? this.favorites.filter((p) => p !== cwd) : [...this.favorites, cwd];
    await this.neosh.state.set(KEY_FAVORITES, this.favorites);
    return !now;
  }

  async toggleFold(cwd: string): Promise<void> {
    if (this.folded.has(cwd)) this.folded.delete(cwd);
    else this.folded.add(cwd);
    await this.neosh.state.set(KEY_FOLDED, [...this.folded]);
  }

  /** Open a project without a keystroke — used when jumping to the conversation you are in. */
  unfold(cwd: string): void {
    if (!this.folded.delete(cwd)) return;
    void this.neosh.state.set(KEY_FOLDED, [...this.folded]);
  }

  /**
   * Move a project one place within its own group.
   *
   * The displayed order is written back wholesale first. Without that, the first `K` on a project
   * you have never moved would reorder against an empty list and jump somewhere you did not ask
   * for — the list has to mean what is on screen before it can be rearranged.
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

    this.order = groups.flatMap((g) => (g === group ? moved : g.map((p) => p.cwd)));
    await this.neosh.state.set(KEY_ORDER, this.order);
    return true;
  }
}

function strings(v: unknown): string[] {
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
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
    return group(sessions, arrangement);
  };

  w.subscriptions.push(
    await neosh.cmd.register(`${NS}.key`, async (_args, k) => {
      if (!k) return;
      const code = k.key.code;
      const target = list.value;

      switch (code.kind) {
        case "esc":
          await w.leave();
          return;
        case "up":
          list.move(-1);
          break;
        case "down":
          list.move(1);
          break;
        case "enter":
          await activateTarget(neosh, arrangement, target, w);
          return;
        case "char": {
          const c = code.c;
          if (k.key.mods.ctrl) {
            if (c === "n") list.move(1);
            else if (c === "p") list.move(-1);
            else if (c === "c") await w.leave();
            else return;
            break;
          }
          switch (c) {
            case " ":
              if (target?.kind === "archive") {
                await arrangement.toggleArchive();
                break;
              }
              if (target?.kind !== "project") return;
              await arrangement.toggleFold(target.cwd);
              break;
            case "j":
              list.move(1);
              break;
            case "k":
              list.move(-1);
              break;
            // Shift moves the thing rather than the cursor — the one convention every list that
            // can be rearranged already shares.
            //
            // From a conversation row it moves the project that conversation is in. Requiring the
            // cursor to be on the heading first made the feature invisible: you are looking at the
            // project when you are looking at what is inside it.
            case "J":
            case "K": {
              const cwd = owningProject(target);
              if (cwd === null) return;
              if (!(await arrangement.move(await groups(), cwd, c === "J" ? 1 : -1))) return;
              break;
            }
            case "f": {
              if (!target || target.kind === "add" || target.kind === "archive") return;
              const cwd = target.cwd;
              const now = await arrangement.toggleFavorite(cwd);
              neosh.notify(now ? `favourited ${short(cwd)}` : `unfavourited ${short(cwd)}`, "info");
              break;
            }
            case "q":
              await w.leave();
              return;
            case "n": {
              // In the project you are looking at, which is the whole reason the cursor is there.
              const cwd = owningProject(target) ?? undefined;
              await neosh.session.create(cwd ? { cwd } : undefined);
              await w.leave();
              return;
            }
            // The everyday verb, and it is reversible. Archiving takes a conversation out of the
            // list without taking anything away, which is what people were reaching for `x` to do
            // before `x` deleted things.
            case "x":
            case "u": {
              if (target?.kind !== "session") return;
              if (target.archived) arrangement.openArchive();
              await setArchived(neosh, target.id, !target.archived);
              break;
            }
            // Shifted, because this is the one that cannot be undone.
            case "X": {
              if (target?.kind !== "session") return;
              await deleteSession(neosh, target.id);
              break;
            }
            case "r":
              if (target?.kind !== "session") return;
              await renameSession(neosh, target.id);
              break;
            case "o":
              await w.leave();
              await neosh.cmd.exec("project.open").catch(() => {});
              return;
            case "?":
              await neosh.cmd.exec("help.keys").catch(() => {});
              return;
            default:
              return;
          }
          break;
        }
        default:
          return;
      }
      await w.draw();
    }, { desc: "Sidebar key" }),
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
      arrangement.openArchive();
      await setArchived(neosh, id, false);
    }, { desc: "Bring an archived conversation back" }),
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

  await neosh.hint.set("sessions", { keys: "^T", label: "conversations", priority: 20 });
  await neosh.hint.set("new", { keys: "^N", label: "new", priority: 21 });
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
    | { kind: "tree"; path: string; label: string };
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
  if (target.kind === "add") {
    await w.leave();
    await neosh.cmd.exec("project.open").catch(() => {});
    return;
  }
  if (target.kind === "archive") {
    await arrangement.toggleArchive();
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
    // Opening something you put away is a statement that you want it back. Leaving it archived
    // while you work in it would mean the list you are looking at does not contain the
    // conversation you are in.
    if (target.archived) await neosh.session.archive(target.id, false);
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
 * reversible by pressing the same key again — charging for it would teach you to dismiss dialogs,
 * which is exactly the habit that makes the delete dialog useless.
 */
async function setArchived(neosh: Neosh, session: string, archived: boolean): Promise<void> {
  try {
    await neosh.session.archive(session, archived);
    neosh.notify(archived ? "archived — `u` brings it back" : "unarchived");
  } catch (e) {
    neosh.notify(String(e), "warn");
  }
}

/**
 * Delete a conversation, asking first when there is something to lose.
 *
 * The file goes from disk and there is no undo, so this is the one verb in the panel that asks. It
 * asks *conditionally*: a conversation you have not said anything in yet has nothing to lose, and
 * a dialog for it is friction that teaches you to dismiss dialogs.
 */
async function deleteSession(neosh: Neosh, session: string): Promise<void> {
  const info = (await neosh.session.list({ includeArchived: true }).catch(() => [] as SessionInfo[]))
    .find((s) => s.id === session);
  if (info && info.message_count > 0) {
    const what = info.message_count === 1 ? "message" : "messages";
    const ok = await confirmDestructive(
      neosh,
      `Delete "${clip(info.label, 40)}" and its ${info.message_count} ${what}? Archiving keeps it.`,
      { yes: "Delete", no: "Keep" },
    );
    if (!ok) return;
  }
  try {
    await neosh.session.close(session);
  } catch (e) {
    neosh.notify(String(e), "warn");
  }
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
function group(sessions: SessionInfo[], arrangement: Arrangement): Project[][] {
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
  const projects = group(sessions, arrangement).flat();

  rows.push(...heading("PROJECTS", opts.width));
  for (const p of projects) {
    rows.push(projectRow(p, arrangement, opts, now));
    if (arrangement.isFolded(p.cwd)) continue;
    for (const s of p.sessions) rows.push(sessionRow(s, now, opts));
    if (p.sessions.length === 0) {
      rows.push({ text: "       nothing here yet", hl: "Sidebar.Dim", inert: true });
    }
  }

  rows.push(blank());
  rows.push({
    text: "   + Add project",
    hl: "Accent",
    value: { kind: "add" },
  });

  // Only when there is one. An empty section is a permanent reminder of a feature you are not
  // using, which is the cost of putting every feature on screen forever.
  if (archived.length > 0) {
    const open = arrangement.isArchiveOpen();
    const arrow = opts.ascii ? (open ? "v" : ">") : open ? "▾" : "▸";
    rows.push(blank());
    rows.push({
      text: ` ${arrow} ARCHIVED`,
      hl: "Sidebar.Heading",
      right: { text: `${archived.length} `, hl: "Sidebar.Dim" },
      value: { kind: "archive" },
    });
    if (open) {
      for (const s of archived) rows.push(archivedRow(s, opts));
    }
  }

  if (opts.hints) rows.push(...hints(opts));

  return { rows, running };
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

  return {
    text: ` ${heart} ${arrow} ${clip(p.name, opts.width - 8)}`,
    hl: here ? "Directory" : "Sidebar.Dim",
    spans: p.favorite ? [{ from: 1, to: 1 + byteLength(heart), hl: "Sidebar.Favorite" }] : undefined,
    right,
    value: { kind: "project", cwd: p.cwd },
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
    value: { kind: "session", id: s.id, cwd: s.cwd, archived: false },
  };
}

/**
 * An archived conversation: dim, with the project it came from instead of a clock.
 *
 * The project is what you need here and nowhere else — the archive is one flat list, so a name on
 * its own does not say which checkout it belongs to.
 */
function archivedRow(s: SessionInfo, opts: DrawOptions): ListRow<Target> {
  const project = s.project || basename(s.cwd);
  const width = Math.max(8, opts.width - 10 - project.length);
  return {
    text: `     ${opts.ascii ? "-" : "┈"} ${clip(s.label, width)}`,
    hl: "Sidebar.Dim",
    right: { text: `${project} `, hl: "Comment" },
    value: { kind: "session", id: s.id, cwd: s.cwd, archived: true },
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
  const target = opts.selected;
  const kind = target?.kind;
  const archived = target?.kind === "session" && target.archived;
  const lines = !opts.focused
    ? ["^T projects  ^N new   ^O add", "^K palette   ^B hide  F1 keys"]
    : kind === "project"
      ? ["↵ fold   f ♥      JK move", "n new    o add    ? keys"]
      : archived
        ? ["↵ open    u unarchive", "X delete  ? keys"]
        : kind === "session"
          ? ["↵ open   r rename   x archive", "JK move  X delete   ? keys"]
          : kind === "archive"
            ? ["↵ show what is put away", "? keys"]
            : ["↵ add project    ? keys", "esc back"];

  return [
    blank(),
    { text: "─".repeat(Math.max(1, opts.width)), hl: "Separator", inert: true },
    ...lines.map((text) => ({ text: ` ${text}`, hl: "Sidebar.Dim", inert: true })),
  ];
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
