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
 *   at a command with a name. `^Z` lists them, the palette runs them, and one line in your
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
 * favourites rather than each starting empty.
 */

import { byteLength, projectScope } from "@neosh/api";
import type {
  AgentSummary,
  Contribution,
  Disposable,
  DrawnRow,
  Neosh,
  PluginContext,
  NodeInfo,
  SessionInfo,
  Message,
  SwarmAgent,
  SwarmNode,
  SwarmStranger,
  BufferId,
  VarScope,
  ViewId,
  WindowId,
  WorktreeInfo,
} from "@neosh/api";
import {
  confirm,
  confirmDestructive,
  configureMotion,
  CursoredList,
  type Decoration,
  type DecorationItem,
  badgeWidth as badgeColumns,
  decorateRow,
  elapsed,
  fitBadge,
  fuzzy,
  type ListRow,
  type Match,
  mergeDecorations,
  placeSections,
  type SectionItem,
  sectionRows as contributedRows,
  money,
  onTick,
  pathPicker,
  picker,
  type PickerItem,
  prompt,
  pulseBright,
  pulseHl,
  spinnerFrame,
} from "@neosh/api/ui";

/** What a row points at, so the cursor survives the list being rebuilt underneath it. */
type Target =
  | { kind: "project"; cwd: string }
  | { kind: "session"; id: string; cwd: string }
  /** The row that opens another directory. A row rather than only a key, because a verb nobody
   * can see is a verb nobody uses. */
  | { kind: "add" }
  /**
   * A row somebody else contributed. `command` runs on `↵`, with `args` as given.
   *
   * `section` is which contribution it came from, and it is what lets a verb belong to *one*
   * plugin's block rather than to every contributed row in the column — see {@link applies}.
   */
  | { kind: "custom"; command?: string; args?: string[]; section?: string }
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
/**
 * Something to put *on* a row this panel already draws — a git badge on a project, a colour on a
 * conversation — keyed by what the row is about rather than where it is.
 *
 * The third kind of thing a plugin can do to a panel it did not write, and the one the original
 * contribution vocabulary left out: a section is rows of your own, an action is a verb on ours, and this is a mark on ours. Pure
 * data, merged at draw time, so a decorator costs the panel nothing on the paint path and can be
 * listed and disabled like any contribution. It is nvim-tree's decorator API as a contribution.
 */
const POINT_DECORATION = "sidebar.decoration";

/**
 * The blocks this panel draws, in order, for a section to sit `before` or `after`.
 *
 * Published rather than implied: `above`/`below` gave a contributor two slots and no way to land
 * between two of anybody else's sections. A section may also name another section's id.
 */
const SLOTS = ["projects", "add"] as const;
type Slot = (typeof SLOTS)[number];

/** The key a decoration is filed under: what the row is *about*. */
function targetKey(t: Target | undefined): string | null {
  if (!t) return null;
  if (t.kind === "project") return `project:${t.cwd}`;
  if (t.kind === "session") return `session:${t.id}`;
  return null;
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
  /** What it does, for the hint strip and for `^Z`. */
  label: string;
  command: string;
  /**
   * Which rows it applies to. Defaults to `any`.
   *
   * `custom` is a row somebody contributed through `sidebar.section` — usually the contributor's
   * own. Without it a plugin can put rows in this column and then has nowhere to put a key that
   * belongs to them: `any` binds the key on every row in the panel and advertises it on all of
   * them, which is a verb about the plan gauge appearing while the cursor is on a conversation.
   *
   * `custom:<section id>` goes one step narrower and names *which* contributed block — `custom:git`
   * is a verb about the git rows and nowhere else. Bare `custom` is every contributed row in the
   * column, which stops being what anybody means the moment there are two blocks in it: the plan
   * and git both wanting `<Tab>` on their own rows is two correct requests and one key, and the
   * panel is what resolves it — see {@link applies}.
   */
  on?: string;
}

/** A project as the panel thinks of it: a directory, and what is going on in it. */
interface Project {
  cwd: string;
  name: string;
  favorite: boolean;
  sessions: SessionInfo[];
  /** The cross-machine identity, for matching against what other computers have. */
  key: string;
  /**
   * Linked worktrees of this checkout, nested rather than listed beside it.
   *
   * A worktree used to be a top-level project — correct by the old rule that a worktree is a
   * project, and wrong as a *list*: four scratch trees of one repository read as four unrelated projects,
   * and the thing they have in common is the thing the column no longer said. The name carried
   * the relationship (`neosh · brisk-otter`) precisely because the structure did not. Each entry
   * is an ordinary {@link Project} — its own cwd, its own fold and rank vars — whose `worktrees`
   * is always empty, because git does not nest them either.
   */
  worktrees: Project[];
}

const NS = "neosh.sidebar";
/** What this panel's buffer says it is. Everything a third party binds or finds hangs off this. */
const KIND = "neosh.sidebar";

/** Emitted on every cursor move, with the row under it as `data` — a {@link Target} or `null`. */
const EVENT_CURSOR = "sidebar.cursor";

/**
 * What this panel offers a plugin that imports it: `import { api } from "plugin:sidebar"`.
 *
 * The same answers are on the commands `sidebar.cursor` and `sidebar.rows` for a plugin that would
 * rather `cmd.call` than depend on us; this is the typed, zero-round-trip form for one that has
 * put `requires = ["sidebar"]` in its manifest. Filled in by `activate`, so a caller that reads it
 * before then sees a panel that is not open and has no rows — which is true.
 */
export const api = {
  /** The buffer kind, for `keymap.set` at `buf_kind` scope and `win.ofKind`. */
  kind: KIND,
  /** The contribution points this panel reads. */
  points: { section: POINT_SECTION, action: POINT_ACTION, decoration: POINT_DECORATION },
  /** The blocks a section may sit `before` or `after`, in the order they are drawn. */
  slots: SLOTS as readonly string[],
  /** The row under the cursor, or `null` when the panel is closed or on nothing. */
  cursor: (): Target | null => null,
  /** Every row that can be landed on, top to bottom. */
  rows: (): Target[] => [],
  /** Redraw now rather than on the next tick. */
  refresh: async (): Promise<void> => {},
};

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
 * The projects, in workspace scope. This list *is* what the panel shows.
 *
 * It used to be an index — a hint, with the real list derived from the conversations that happened
 * to live somewhere. That is the bug this replaced: deleting the last conversation in a directory
 * deleted the directory too, so clearing out a project you had spent a month in removed it from the
 * panel and there was nothing left to start the next conversation from. A project is a place you
 * work, not a property of the work in it.
 *
 * Kept current from three ends — every conversation's directory goes in, so does any directory
 * somebody sets a project var on, and so does anything added by hand — and taken out of by exactly
 * one thing, which is asking for it to go. See [`Arrangement.forget`].
 */
const VAR_KNOWN = "sidebar.projects";
/**
 * What a project is called, remembered for when there is nothing in it to ask.
 *
 * The name comes from the host, which reads it off the repository — `neosh (feat/thing)` for a
 * worktree rather than the directory's own `wt-fe3c0d93` — and it arrives stamped on a conversation.
 * A project with no conversations left has nobody to ask, and falling back to the basename renames
 * a worktree you have used for a fortnight into something you do not recognise the moment you empty
 * it. Written when it is known, read when it is not.
 */
const VAR_NAME = "sidebar.name";
/**
 * The checkout a directory is a linked worktree of, for when there is nothing in it to ask.
 *
 * Which tree belongs to which repository is read off a conversation's `repo_root`, so a worktree
 * you have emptied has nobody to say it — and without this it stops being a worktree the moment it
 * stops having conversations, jumps out of the repository it belongs to and lands at the top level
 * as a project of its own. Which is the worktree-nesting rule, arrived at from the other
 * direction: a tree does not become a project by being idle.
 */
const VAR_ROOT = "sidebar.root";
/**
 * Which conversations have stopped and are waiting on an answer.
 *
 * Written by whatever serves `ask_user` — the bundled `questions` plugin, or yours instead of it —
 * and read here, which is the whole reason it is a var and not that plugin's private state. Nothing
 * else in this column can be worked out: a conversation blocked on a question still has a turn in
 * flight, so `active_turn` is set and the row it draws is the spinner, identical to the twenty above
 * it that are actually thinking. An answer nobody is going to give looks like work in progress until
 * you happen to open it.
 *
 * So it outranks *working* wherever the two meet below, and it wears `Status.Pending` — the
 * palette's own group for waiting on something outside the program, which is what the footer's
 * `Question.Waiting` links to, so the two say the same thing in the same colour without this panel
 * having to know the other one exists.
 */
const VAR_ASKING = "question.asking";
/**
 * The same, for a permission prompt. Written by whatever serves `permission_pre` — the bundled
 * `approvals` plugin — and folded into the same set: a conversation blocked on "may I run this" is
 * blocked exactly as one blocked on "which database", and drew as an ordinary spinner until this
 * was read.
 */
const VAR_PERMITTING = "permission.asking";

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

  // Read once and then kept current from `vars.onChange`, like the arrangement and for the same
  // reason: a draw runs on a 100 ms tick while a turn is in flight and must not spend a round trip
  // per frame asking a question whose answer changes twice an hour.
  const waiting = {
    questions: asked(
      await neosh.vars.get<string[]>({ scope: "global" }, VAR_ASKING).catch(() => null),
    ),
    permissions: asked(
      await neosh.vars.get<string[]>({ scope: "global" }, VAR_PERMITTING).catch(() => null),
    ),
  };
  let asking = new Set([...waiting.questions, ...waiting.permissions]);

  // One panel per terminal.
  //
  // A workspace can have several and they are not copies of each other: which conversation is
  // open, which row the cursor is on, whether the column is showing at all. All of that is
  // navigation, and navigation is what a view *is* — so the buffer is per view too, since the
  // cursor and the unfolded row are drawn into its text.
  const panels = new Map<ViewId, Panel>();

  const makePanel = async (view: ViewId): Promise<Panel> => {
    // The kind is what makes this panel something other plugins can act on: it is the scope their
    // keymaps bind at and the handle `win.ofKind` finds it by. One argument, and the difference
    // between a panel you can extend and one you can only replace.
    const buf = await neosh.buf.create({ name: "[sidebar]", scratch: true, kind: KIND });
    const ns = await neosh.ns.create(NS);
    // In this terminal, and only this one. Everything else on `here` is the call it always was.
    const here = neosh.view.at(view);
    // The panel's own width, as of the last frame. The list needs it to unfold the row under the
    // cursor, and reading the option again per render would be a round trip inside a redraw.
    let panelWidth = 34;
    const list = new CursoredList<Target>(here, buf, ns, {
      width: () => panelWidth,
      // Said out loud on every move, so a plugin can follow the cursor without polling — a
      // preview of the conversation under it, a status segment naming the project. One event per
      // keystroke, which is what the composer already costs.
      onMove: () => void neosh.event.emit(EVENT_CURSOR, list.value ?? null),
    });
    let win: WindowId | null = null;
    let focused = false;
    // Typed before a motion and consumed by it. Shared with the key table and with the draw, which
    // is what puts it on screen while it is half typed.
    const count = { pending: "" };
    let capture: Disposable | null = null;
    let running = false;

    // Serialises redraws. Two overlapping refreshes interleave their `setLines` and `mark` calls
    // and leave highlights pointing at rows that have already been replaced.
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
            neosh.opt.get<number>("sidebar.width").catch(() => null),
            neosh.opt.get<boolean>("ui.ascii_only").catch(() => null),
            neosh.opt.get<boolean>("sidebar.hints").catch(() => null),
          ]);
          panelWidth = width ?? 34;
          const built = await collect(here, arrangement, {
            width: panelWidth,
            ascii: ascii ?? false,
            hints: hints ?? true,
            focused,
            selected: list.value,
            actions: actions(),
            asking,
            count: count.pending,
            // Filled in by `collect`, which is what reads the swarm and the decorations.
            decorations: new Map(),
            remote: new Map(),
            hosts: new Map(),
          });
          running = built.running;
          list.setRows(built.rows, same);
          await list.render({
            showCursor: focused,
            win: win ?? undefined,
            pinned: built.pinned,
          });
        } while (again);
      } finally {
        drawing = false;
      }
    };

    /**
     * The open in flight, so two callers cannot each make a window.
     *
     * `win` is assigned two `await`s after it is tested — a host round trip for the width, then
     * the window itself — so `if (win === null)` is a check whose answer is stale by the time it
     * is acted on. Two calls in that gap both saw `null`, both opened a column, and `win` kept
     * only the second: the first stayed on screen and could never be closed again, because
     * `close` closes `win`. Two sidebars, from pressing `^B` or `^T` twice quickly, or from a key
     * arriving while `view.onOpen` was still building the panel.
     *
     * Concurrent callers await the same open rather than starting another. They all wanted the
     * same end state and now they all get it — which is why this returns the promise instead of
     * dropping the second call, whose caller is about to `focus` something.
     */
    let opening: Promise<void> | null = null;

    const open = (): Promise<void> => {
      if (opening) return opening;
      const run = (async () => {
        if (win === null) {
          // Read defensively: a reload takes every declared option away and puts it back, and this
          // can be called from a view arriving in the gap. A panel that throws there used to take
          // the whole plugin runtime with it.
          const width = (await neosh.opt.get<number>("sidebar.width").catch(() => null)) ?? 34;
          win = await here.win.open(buf, "left", { size: width });
        }
        await draw();
        // And again once the frontend has said how tall the panel turned out to be. The foot is
        // held against the bottom edge, which is a measurement — and on the first frame there is
        // nothing to measure yet, so without this the strip sits under the last project until
        // something else happens to redraw.
        // Not held in `subscriptions`: the panel is opened and closed as often as `^B` is pressed,
        // and the runtime cancels a plugin's timers when it unloads anyway.
        neosh.timer.after(120, () => void draw());
      })();
      opening = run.finally(() => {
        opening = null;
      });
      return opening;
    };

    const leave = async () => {
      if (!focused) return;
      focused = false;
      capture?.dispose();
      capture = null;
      await here.focus.pop().catch(() => {});
      await draw();
    };

    const close = async () => {
      // Let an open that is still in flight finish first. `^B` twice quickly is open-then-close,
      // and a close that reads `win` while the open has not assigned it yet sees `null`, returns
      // having done nothing, and leaves the column that arrives a moment later on screen — with
      // the panel now believing it is shut. The next `^B` opens a second one.
      if (opening) await opening.catch(() => {});
      if (win === null) return;
      const w = win;
      win = null;
      await leave();
      await neosh.win.close(w).catch(() => {});
    };

    const enter = async () => {
      await open();
      if (win === null || focused) return;
      focused = true;
      await neosh.focus.push(win);
      // Every key the bindings did not claim comes here, which is what makes `j`, `f` and `?` mean
      // something in the panel while `^Q` still quits.
      capture = await neosh.keymap.capture(win, `${NS}.key`).catch(() => null);
      // Land on the conversation you are in, not wherever the cursor happened to be — and *this*
      // terminal's, which is the whole reason the panel is per view.
      const current = await here.session.current().catch(() => null);
      if (current) {
        // Unfold the project it lives in, or "go to where I am" lands on a collapsed heading. For
        // a conversation in a worktree that is two folds: the repository's, then the worktree's.
        if (current.repo_root) arrangement.unfold(current.repo_root);
        arrangement.unfold(current.cwd);
        list.select((t) => t.kind === "session" && t.id === current.id);
      }
      await draw();
    };

    return {
      view,
      here,
      buf,
      list,
      count,
      draw,
      open,
      close,
      enter,
      leave,
      isOpen: () => win !== null,
      isRunning: () => running,
      width: () => panelWidth,
      setWidth: (n: number) => {
        panelWidth = n;
        if (win !== null) void neosh.win.resize(win, n).then(draw).catch(() => {});
      },
      height: async () => {
        if (win === null) return 20;
        const v = await neosh.win.viewport(win).catch(() => null);
        return v?.height ?? 20;
      },
      dispose: () => {
        capture?.dispose();
        capture = null;
        // Not closed here: the terminal has gone and the host took its windows with it. Closing a
        // window that is already gone is an error message about nothing.
        win = null;
      },
    };
  };

  /** The panel in a named terminal, or the one being served when nothing is named. */
  const panel = (view?: ViewId): Panel | null => {
    if (view !== undefined) return panels.get(view) ?? null;
    // A command run by name rather than by key — `^K`, a plugin — names no terminal. There is
    // usually one, and when there is not, the one that has a window open is the one somebody is
    // looking at.
    const all = [...panels.values()];
    return all.find((p) => p.isOpen()) ?? all[0] ?? null;
  };

  const each = (fn: (p: Panel) => unknown) => {
    for (const p of panels.values()) void fn(p);
  };

  const drawAll = () => each((p) => p.draw());
  // The module-level `api`, filled in now that there is something behind it. "The" cursor with
  // several terminals is the served panel's — the same answer every by-name command gives.
  api.cursor = () => panel()?.list.value ?? null;
  api.rows = () => panel()?.list.values ?? [];
  api.refresh = async () => drawAll();

  // The verbs other plugins put on our rows. Bound here rather than by them so the row under the
  // cursor can be handed along; see `installActions`.
  const installed = installActions(neosh, panel, drawAll);
  const actions = installed.actions;
  subscriptions.push(installed.dispose);

  await registerCommands({ neosh, subscriptions, arrangement, panel, each });

  // Text changes: a turn started or ended, the conversation changed, a setting moved.
  subscriptions.push(neosh.agent.onTurnStart(drawAll));
  subscriptions.push(neosh.agent.onTurnEnd(drawAll));
  subscriptions.push(neosh.agent.onToolStart(drawAll));
  subscriptions.push(neosh.session.onChange(drawAll));
  // What a conversation is still running after its turn ended. The only state on these rows that
  // moves without a turn starting or ending — that is the whole point of it — so nothing else this
  // panel already listens to would ever bring it. Filtered to the one kind, because a driver
  // reports its context size every few seconds and redrawing the column for that is a column that
  // redraws for nothing.
  subscriptions.push(
    neosh.agent.onActivity((e) => {
      if (e.activity.kind === "background") drawAll();
    }),
  );
  // Somebody pinned, folded or tagged a project — possibly us, possibly a plugin that has never
  // heard of this one. Both arrive here, and the arrangement takes them in the same way.
  subscriptions.push(
    neosh.vars.onChange((e) => {
      if (arrangement.observe(e.scope, e.key, e.value)) drawAll();
      // Somebody started waiting on an answer, or stopped. Deleting the var is how "nobody is
      // asking" arrives, and it comes through here with `value` unset rather than as an empty list.
      if (e.scope.scope === "global" && (e.key === VAR_ASKING || e.key === VAR_PERMITTING)) {
        if (e.key === VAR_ASKING) waiting.questions = asked(e.value);
        else waiting.permissions = asked(e.value);
        asking = new Set([...waiting.questions, ...waiting.permissions]);
        drawAll();
      }
    }),
  );
  // Rows somebody contributed came or went. Redrawing on this is what stops a plugin that loads
  // after us contributing rows nobody sees until the next unrelated refresh.
  subscriptions.push(
    neosh.ext.onChange((e) => {
      if (e.point === POINT_SECTION || e.point === POINT_DECORATION) drawAll();
    }),
  );
  subscriptions.push(
    neosh.opt.onChange((e) => {
      if (e.name === "ui.motion" || e.name === "ui.ascii_only") {
        void applyMotion().then(drawAll);
      } else if (e.name === "sidebar.width") {
        // In place. Reopening was how this used to work, and it gave the panel a new window id and
        // dropped whatever had the keyboard — so resizing from inside the panel threw the cursor
        // back to the composer on every press.
        const n = typeof e.value === "number" ? e.value : null;
        if (n !== null) each((p) => p.setWidth(n));
      } else if (e.name.startsWith("sidebar.") || e.name === "ui.theme") {
        drawAll();
      }
    }),
  );

  // Attributes change: the shared 100 ms clock, and only while a turn is in flight. An idle
  // workspace has no timer at all.
  subscriptions.push(
    onTick(() => {
      each((p) => {
        if (p.isOpen() && p.isRunning()) void p.draw();
      });
    }),
  );

  await installFooter(neosh, subscriptions);

  const period = (await neosh.opt.get<number>("sidebar.refresh_ms")) ?? 4000;
  subscriptions.push(neosh.timer.every(period, drawAll));

  // One panel per terminal, and one for every terminal that was already here when this plugin
  // loaded. `sidebar.open` decides whether it starts showing, per view rather than once for the
  // workspace: `^B` in one window is `^B` in that window.
  const startOpen = (await neosh.opt.get<boolean>("sidebar.open")) ?? true;
  // Panels being built, keyed by the terminal they are for. The same check-then-act as `open`, one
  // level up and worse: `panels.set` lands after `makePanel` resolves, so two events for one view
  // in that gap built two complete panels — two buffers, two windows — and the map kept the
  // second. Reserved *synchronously*, before the first `await`, which is what makes the test and
  // the claim one step.
  const making = new Map<ViewId, Promise<Panel>>();
  subscriptions.push(
    neosh.view.onOpen((view) => {
      void (async () => {
        if (panels.has(view) || making.has(view)) return;
        const building = makePanel(view);
        making.set(view, building);
        let made: Panel;
        try {
          made = await building;
        } finally {
          making.delete(view);
        }
        panels.set(view, made);
        if (startOpen) await made.open();
      })().catch((e: unknown) => {
        // A panel that cannot be built is one terminal without a column, not a workspace without
        // plugins. Unhandled, the rejection stops the runtime and takes every other plugin with
        // it — which is what a reload racing this used to do.
        panels.delete(view);
        neosh.log.warn(`sidebar: ${String(e)}`);
      });
    }),
  );
  subscriptions.push(
    neosh.view.onClose((view) => {
      // The windows went with the terminal; what is left is what we were keeping about them, which
      // would otherwise be a capture pointed at a window that no longer exists.
      panels.get(view)?.dispose();
      panels.delete(view);
    }),
  );
  // And when *we* are the ones going away, which is not the same thing and is the one this panel
  // got wrong. `dispose` deliberately does not close a window, because its only caller was a
  // terminal that had already gone and taken its windows with it. A reload is the opposite: every
  // terminal is still there, still showing a column that nothing is going to take off the screen,
  // and nothing disposed the panels on this path at all. So `^R` left the old sidebar standing,
  // the reloaded plugin's `view.onOpen` built a second one beside it, and the workspace had two —
  // one of them drawn by code that no longer existed and answering no keys.
  //
  // Closed rather than merely forgotten, and fire-and-forget because a `Disposable` is
  // synchronous: the op is in flight before the runtime tears down, and a failure here means the
  // window was going anyway.
  subscriptions.push({
    dispose: () => {
      for (const p of panels.values()) {
        void p.close().catch(() => {});
        p.dispose();
      }
      panels.clear();
    },
  });
}

/**
 * Whether two rows are the *same row*, so the cursor survives the list being rebuilt under it.
 *
 * The panel redraws on a tick and re-contributed sections replace themselves wholesale, so this is
 * what stands between "the row I was on moved" and "I am somewhere else now". Every kind is
 * compared by what identifies it rather than by what it says: a conversation by its id, a project
 * by its directory, a remote by the pair that is unique across machines.
 *
 * **A contributed row is identified by its section and its verb.** It used to fall through to
 * `a.kind === b.kind`, which made every custom row in the column identical to every other — so on
 * the next redraw the cursor snapped to the *first* one, wherever it had been. Two rows of the plan
 * strip were enough to see it; a block whose second row is the button and whose first row is a
 * label made it a button you could not reach, because the cursor was back on the label before you
 * pressed the key.
 */
function same(a: Target, b: Target): boolean {
  if (a.kind === "remote") {
    return b.kind === "remote" && a.node === b.node && a.session === b.session;
  }
  if (a.kind === "session") return b.kind === "session" && a.id === b.id;
  if (a.kind === "project") return b.kind === "project" && a.cwd === b.cwd;
  if (a.kind === "custom") {
    // The args too: a section whose rows all run one command with a different argument each — a
    // list of branches, a list of pull requests — is exactly the case the command alone cannot tell
    // apart, and it is a common enough shape to be worth the join.
    return b.kind === "custom" && a.section === b.section && a.command === b.command &&
      (a.args ?? []).join(" ") === (b.args ?? []).join(" ");
  }
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
    // Written back whenever the startup list added to it — including the very first start, when
    // nothing was stored. The var is the list other plugins are told to read (a decorator asks it
    // which projects to decorate), and a list that only reached the disk once a *second* project
    // appeared was a list that said nothing on the day it mattered most.
    if (this.known.length !== strings(stored).length) {
      await this.neosh.vars.set({ scope: "global" }, VAR_KNOWN, this.known);
    }
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
    //
    // A var being *removed* is not somebody saying something about a project — it is the last thing
    // `forget` does, and treating it as an announcement would put the project straight back.
    if (value !== null && value !== undefined && !this.known.includes(scope.cwd)) {
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

  /** Every project, whether or not there is anything going on in it. The panel's list. */
  all(): string[] {
    return [...this.known];
  }

  /** What this project was called the last time anything knew. `null` when nothing ever did. */
  name(cwd: string): string | null {
    const v = this.cache.get(cwd)?.[VAR_NAME];
    return typeof v === "string" && v !== "" ? v : null;
  }

  /** The checkout this one is a worktree of, if anything ever said so. */
  root(cwd: string): string | null {
    const v = this.cache.get(cwd)?.[VAR_ROOT];
    return typeof v === "string" && v !== "" && v !== cwd ? v : null;
  }

  /**
   * Write down what the host calls a project, for when it is empty.
   *
   * Called from the draw path with every project that has a conversation to read a name off, so it
   * has to be free when nothing has changed — which it is: the cache is checked first and the
   * common case is a comparison per project per frame.
   */
  remember(named: Array<{ cwd: string; name: string; root?: string }>): void {
    for (const { cwd, name, root } of named) {
      if (name !== "" && this.name(cwd) !== name) void this.set(cwd, VAR_NAME, name);
      if (root !== undefined && root !== cwd && this.root(cwd) !== root) {
        void this.set(cwd, VAR_ROOT, root);
      }
    }
  }

  /**
   * Take a project out of the list.
   *
   * The one thing that removes one, and the reason the list can be trusted to survive its
   * conversations. Only this panel's own vars go: another plugin's note about the directory is that
   * plugin's to keep, and a directory it still cares about comes back the next time it says so.
   */
  async forget(cwd: string): Promise<void> {
    this.known = this.known.filter((c) => c !== cwd);
    this.cache.delete(cwd);
    await this.neosh.vars.set({ scope: "global" }, VAR_KNOWN, this.known);
    await Promise.all(
      [VAR_FAVORITE, VAR_RANK, VAR_FOLDED, VAR_NAME, VAR_ROOT].map((key) =>
        this.neosh.vars.remove(projectScope(cwd), key).catch(() => {})
      ),
    );
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
    // A worktree moves among its siblings under the same repository; a project among its group.
    const group: Project[] | undefined =
      groups.flat().find((p) => p.worktrees.some((t) => t.cwd === cwd))?.worktrees ??
        groups.find((g) => g.some((p) => p.cwd === cwd));
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

/**
 * One terminal's panel.
 *
 * Everything here is per view because all of it is navigation: which row the cursor is on, whether
 * the column is showing, how wide it is, a count half typed. The buffer is per view too, since the
 * cursor and the unfolded row are drawn into its text — two terminals reading one project list is
 * the same list rendered twice, and `j` in one must not move the other.
 */
interface Panel {
  readonly view: ViewId;
  /** The whole API, bound to this terminal. A window opened through it lands here. */
  readonly here: Neosh;
  readonly buf: BufferId;
  readonly list: CursoredList<Target>;
  /**
   * A count typed before a motion — `5j`, `12G`.
   *
   * State between two keystrokes, so it lives beside the panel rather than in the key table, and
   * it is drawn in the hint strip: a count you cannot see is a keypress that appears to have done
   * nothing at all.
   */
  readonly count: { pending: string };
  draw(): Promise<void>;
  open(): Promise<void>;
  close(): Promise<void>;
  enter(): Promise<void>;
  leave(): Promise<void>;
  isOpen(): boolean;
  /** Whether anything in this panel's list has a turn in flight, for the animation tick. */
  isRunning(): boolean;
  width(): number;
  setWidth(n: number): void;
  /** How many rows of panel there are, for the two keys that mean "a screen". */
  height(): Promise<number>;
  dispose(): void;
}

interface Wiring {
  neosh: Neosh;
  subscriptions: PluginContext["subscriptions"];
  arrangement: Arrangement;
  /**
   * The panel a key was pressed in.
   *
   * Every verb in this file starts here. A command run by name rather than by key names no
   * terminal, and then it is whichever one has the panel open — there is usually exactly one, and
   * "the one somebody is looking at" is the only answer that is ever right.
   */
  panel(view?: ViewId): Panel | null;
  /** Do something in every terminal's panel. */
  each(fn: (p: Panel) => unknown): void;
}

async function registerCommands(w: Wiring): Promise<void> {
  const { neosh, arrangement } = w;

  /** Rebuild the grouping the arrangement reorders against, without redrawing. */
  const groups = async (): Promise<Project[][]> => {
    const sessions = await neosh.session.list().catch(() => [] as SessionInfo[]);
    return group(sessions, arrangement, new Map());
  };

  /**
   * Register one verb, and bind it inside this panel.
   *
   * Every key in this list goes through here, which is the point: there is no private key handler
   * any more, so `^Z` lists the panel's keys with the rest, `^K` runs them, and
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
    fn: (p: Panel, target: Target | undefined, args: string[]) => Promise<void> | void,
    opts: { redraw?: boolean } = {},
  ): Promise<void> => {
    w.subscriptions.push(
      // The panel the key was pressed in. Every verb below acts on one terminal's column — its
      // cursor, its fold state, its half-typed count — and with several open, "the panel" is not
      // an answer. A command run by name rather than by key names no terminal and gets whichever
      // one is showing.
      await neosh.cmd.register(name, async (args, key) => {
        const p = w.panel(key?.view);
        if (p === null) return;
        await fn(p, p.list.value, args);
        // A count belongs to the motion typed straight after it. Anything else ends it — otherwise
        // a `5` you thought better of sits there and turns the next `j` into five.
        p.count.pending = "";
        if (opts.redraw !== false) await p.draw();
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
  //
  // Vim's motions, because this is a cursor in a column in a terminal and that is the vocabulary
  // everybody's hands already have. A count works the way it does there — `5j` is five rows, `3G`
  // is the third — and it counts *rows you can land on*, so the headings and rules the cursor
  // already skips do not silently eat two of the five.
  const scope = { kind: "buf_kind", name: KIND } as const;
  const also = async (key: string, command: string, desc: string): Promise<void> => {
    await neosh.keymap.set("chat", key, command, { scope, desc });
  };

  /** Take the count that was typed in one panel, if one was, and forget it. */
  const take = (p: Panel, fallback = 1): number => {
    const n = Number.parseInt(p.count.pending, 10);
    p.count.pending = "";
    // Bounded, because the count is typed and `999999j` is a keystroke that would walk the list a
    // million times before drawing anything.
    return Number.isFinite(n) && n > 0 ? Math.min(n, 999) : fallback;
  };

  await verb(`${NS}.down`, "j", "Next row", (p) => void p.list.move(take(p)));
  await verb(`${NS}.up`, "k", "Previous row", (p) => void p.list.move(-take(p)));
  await also("<Down>", `${NS}.down`, "Next row");
  await also("<Up>", `${NS}.up`, "Previous row");
  await also("<C-n>", `${NS}.down`, "Next row");
  await also("<C-p>", `${NS}.up`, "Previous row");

  // A screen is however tall the panel turned out to be, which only the frontend knows — asked for
  // rather than assumed, the same way every other display measurement here is. Half a screen for
  // `^D`/`^U` because that is what it is everywhere, and a whole one loses the row you were on.
  const by = (fraction: number, sign: 1 | -1) => async (p: Panel): Promise<void> => {
    const rows = Math.max(2, await p.height());
    const step = Math.max(1, Math.floor(rows * fraction)) * take(p);
    // No wrapping on a page step: `^D` at the foot of the list means there is no more of it, and a
    // cursor that reappears at the top has thrown away the place you were reading from.
    p.list.move(sign * step, { wrap: false });
  };
  await verb(`${NS}.half.down`, "<C-d>", "Half a screen down", by(0.5, 1));
  await verb(`${NS}.half.up`, "<C-u>", "Half a screen up", by(0.5, -1));
  // A whole screen is `PgUp`/`PgDn` and deliberately *not* `^F`/`^B`. Those two are `^F` archive
  // and `^B` hide the panel — both of which somebody in this panel is more likely to want than a
  // page step in a column that is rarely taller than one screen, and a key that means something
  // else only while the cursor happens to be here is the worst kind of key.
  await verb(`${NS}.page.down`, "<PageDown>", "A screen down", by(1, 1));
  await verb(`${NS}.page.up`, "<PageUp>", "A screen up", by(1, -1));

  /** The count, when a verb wants the number itself rather than a repetition. */
  const typed = (p: Panel): number | null => {
    const n = Number.parseInt(p.count.pending, 10);
    p.count.pending = "";
    return Number.isFinite(n) && n > 0 ? n : null;
  };
  // With a count these are a *row* — `5gg` and `5G` are both the fifth — and without one they are
  // the two ends, exactly as in Vim.
  await verb(`${NS}.top`, "gg", "The first row, or the n-th with a count", (p) => {
    const n = typed(p);
    if (n === null) p.list.toEnd("first");
    else p.list.nth(n);
  });
  await verb(`${NS}.bottom`, "G", "The last row, or the n-th with a count", (p) => {
    const n = typed(p);
    if (n === null) p.list.toEnd("last");
    else p.list.nth(n);
  });

  /**
   * A digit, before a motion.
   *
   * One command for all ten, reading which digit it was off the key that ran it — so `^Z` lists it
   * once and `init.ts` can move it, rather than ten near-identical verbs. A leading `0` is not a
   * count anywhere and is left to whatever else might want the key.
   */
  w.subscriptions.push(
    await neosh.cmd.register(`${NS}.count`, async (_args, key) => {
      const p = w.panel(key?.view);
      const code = key?.key.code;
      if (p === null || code?.kind !== "char") return;
      if (p.count.pending === "" && code.c === "0") return;
      // Bounded while it is being typed, not only when it is read: three digits is more rows than
      // this panel will ever have, and it stops a leant-on key growing a string forever.
      if (p.count.pending.length < 3) p.count.pending += code.c;
      // Drawn straight away — the strip is the only thing saying the digit landed anywhere.
      await p.draw();
    }, { desc: "Begin a count for the next motion" }),
  );
  for (const digit of "0123456789") {
    await also(digit, `${NS}.count`, "Count for the next motion");
  }

  // ---- how wide the column is ----
  //
  // A resize rather than a reopen: reopening gives the window a new id and drops whatever had the
  // keyboard, so the panel would throw you back to the composer on every press. The width is the
  // *setting*, so it is the same number `config.toml` sets and one place decides it.
  const resize = (delta: number) => async (p: Panel): Promise<void> => {
    const now = (await neosh.opt.get<number>("sidebar.width")) ?? 34;
    const want = Math.max(16, Math.min(120, now + delta * take(p)));
    if (want !== now) await neosh.opt.set("sidebar.width", want);
  };
  await verb(`${NS}.wider`, ">", "Widen the panel", resize(2), { redraw: false });
  await verb(`${NS}.narrower`, "<lt>", "Narrow the panel", resize(-2), { redraw: false });
  await verb(`${NS}.width.reset`, "=", "Back to the default width", async () => {
    await neosh.opt.reset("sidebar.width");
  }, { redraw: false });

  // ---- leaving ----
  await verb(`${NS}.leave`, "<Esc>", "Back to the composer", (p) => p.leave(), { redraw: false });
  await neosh.keymap.set("chat", "q", `${NS}.leave`, { scope: { kind: "buf_kind", name: KIND } });
  await neosh.keymap.set("chat", "<C-c>", `${NS}.leave`, { scope: { kind: "buf_kind", name: KIND } });

  // ---- the row under the cursor ----
  await verb(
    `${NS}.select`,
    "<CR>",
    "Open a conversation, fold a project, or run the row",
    (p, target) => activateTarget(neosh, arrangement, target, p),
    { redraw: false },
  );
  await verb(`${NS}.fold`, "<Space>", "Fold or unfold this project", async (p, target) => {
    if (target?.kind !== "project") return;
    await arrangement.toggleFold(target.cwd);
  });
  await verb(`${NS}.favorite`, "f", "Pin this project to the top", async (p, target) => {
    let cwd = owningProject(target);
    if (cwd === null) return;
    // Pinning is about the top of the column, and a worktree is never at the top of the column —
    // `f` on one pins the repository it belongs to, the same way `f` on a conversation pins the
    // project it is in.
    const inside = (await neosh.session.list().catch(() => [] as SessionInfo[]))
      .find((s) => s.cwd === cwd && s.repo_root && s.repo_root !== s.cwd);
    if (inside?.repo_root) cwd = inside.repo_root;
    // No message. The row moves to the top of the column and gains a pin, in the panel the cursor
    // is already in — saying so in the corner as well is the same fact twice, and a corner that is
    // usually restating something visible is one people stop reading.
    await arrangement.toggleFavorite(cwd);
  });

  // Shift moves the thing rather than the cursor — the one convention every list that can be
  // rearranged already shares.
  //
  // From a conversation row it moves the project that conversation is in. Requiring the cursor to
  // be on the heading first made the feature invisible: you are looking at the project when you are
  // looking at what is inside it.
  const reorder = (delta: number) => async (_p: Panel, target: Target | undefined) => {
    const cwd = owningProject(target);
    if (cwd === null) return;
    await arrangement.move(await groups(), cwd, delta);
  };
  await verb(`${NS}.move.down`, "J", "Move this project down", reorder(1));
  await verb(`${NS}.move.up`, "K", "Move this project up", reorder(-1));

  await verb(`${NS}.rename`, "r", "Rename this conversation", async (p, target) => {
    if (target?.kind !== "session") return;
    await renameSession(neosh, target.id);
  });
  // The panel's own copy verb, same letter the transcript uses. What a row is *at* is the thing
  // you paste into a terminal, an editor or a message, and retyping a generated worktree path is
  // the kind of chore this panel exists to remove.
  await verb(`${NS}.copy.path`, "y", "Copy this row's directory", async (p, target) => {
    const cwd = owningProject(target);
    if (cwd === null) return;
    await neosh.edit.copy(cwd);
    neosh.notify(`copied ${cwd}`);
  }, { redraw: false });
  // The everyday verb, and it is reversible. Archiving takes a conversation out of the list without
  // taking anything away, which is what people were reaching for `x` to do before `x` deleted
  // things. Where it goes is `a`, not four dim rows at the foot of this panel.
  await verb(`${NS}.archive`, "x", "Archive this conversation", async (p, target) => {
    if (target?.kind !== "session") return;
    await setArchived(neosh, target.id, true);
  });
  // Shifted, because this is the one that cannot be undone.
  //
  // One key, two rows, and the same sentence: take this off the list for good. On a heading that is
  // the project — which is the verb that had been missing entirely, and the reason the panel used
  // to remove a project *for* you the moment you deleted the last thing in it.
  await verb(
    `${NS}.delete`,
    "X",
    "Delete this conversation, or remove this project",
    async (p, target) => {
      if (target?.kind === "project") {
        await removeProject(neosh, arrangement, target.cwd);
        return;
      }
      if (target?.kind !== "session") return;
      await deleteSession(neosh, target.id);
    },
  );
  await verb(`${NS}.new`, "n", "New conversation in this project", async (p, target) => {
    // The same question `^N` asks, about the project you are looking at — which is the whole reason
    // the cursor is there. It used to create one outright, and that was the bug: `n` and `^N` were
    // one letter and a modifier apart and did visibly different things, so the only way to know
    // which one you wanted was to have already learned that they differ. Here is still the first
    // row, so `n ⏎` is what `n` always did.
    const cwd = owningProject(target) ?? undefined;
    await p.leave();
    await newConversation(neosh, arrangement, cwd);
  }, { redraw: false });

  // ---- doors out of the panel ----
  //
  // `a` is not here. What you have archived is the `archive` plugin's panel, and the key on these
  // rows that opens it is an `archive.action` contribution — which is this panel's own extension
  // point, used by somebody else, rather than a door drawn in by hand. Turn that plugin off and the
  // key goes with it, instead of pointing at a command that is no longer there.
  await verb(`${NS}.add`, "o", "Add a project", async (p) => {
    await p.leave();
    await neosh.cmd.exec("project.open").catch(() => {});
  }, { redraw: false });
  await verb(`${NS}.help`, "?", "The keys for this row", async (p) => {
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
    await neosh.cmd.register("sidebar.toggle", async (_args, key) => {
      // Toggling visibility, not focus: `^T` is how you get in. In *this* terminal: `^B` is about
      // the column in front of you and says nothing about anybody else's.
      const p = w.panel(key?.view);
      if (p === null) return;
      if (p.isOpen()) await p.close();
      else await p.open();
    }, { desc: "Show or hide the sidebar" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("sidebar.focus", async (_args, key) => {
      await w.panel(key?.view)?.enter();
    }, { desc: "Move into the project list" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("sidebar.refresh", async (_args, key) => {
      // By name and from no key, this means "the panel is stale" rather than "this one is", so it
      // redraws every terminal's.
      if (key) await w.panel(key.view)?.draw();
      else w.each((p) => p.draw());
    }, { desc: "Redraw the sidebar now" }),
  );
  // The two questions a plugin on top of this panel asks, as commands so `cmd.call` answers them
  // without a dependency. What a key press passes as arguments, a call gets as the answer.
  w.subscriptions.push(
    await neosh.cmd.register("sidebar.cursor", (_args, key) => {
      // The terminal the call came from, when it came from one; the served panel's otherwise.
      return w.panel(key?.view)?.list.value ?? null;
    }, { desc: "The row under the sidebar's cursor" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("sidebar.rows", (_args, key) => {
      return w.panel(key?.view)?.list.values ?? [];
    }, { desc: "Every row in the sidebar you can land on" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register(
      "session.new",
      (args) => newConversation(neosh, arrangement, args[0]),
      { desc: "Start a new conversation — here, or in a worktree of its own" },
    ),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.new.here", async (p) => {
      await neosh.session.create();
    }, { desc: "Start a new conversation in this project, without asking where" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("session.copy.path", async (p) => {
      // The conversation's directory — which in a worktree is the worktree, and that is the point:
      // the path you want on the clipboard is the one your shell should cd to.
      const current = await neosh.session.current().catch(() => null);
      if (!current) return;
      await neosh.edit.copy(current.cwd);
      neosh.notify(`copied ${current.cwd}`);
    }, { desc: "Copy this conversation's directory to the clipboard" }),
  );
  // An Alt chord, which the default-binding rule keeps out of the defaults — on a Mac out of the
  // box `⌥Y` is `¥`, a key neosh never receives — with the one exception it makes for arrows: bound where
  // it means something, and never the only way. Chat mode has no Ctrl chord left to give this, and
  // every terminal-sendable route exists beside it — `yp` in the reader, `y` on any row of this
  // panel, `^K` and `/copy` by name — so a terminal that sends Alt gets a key in the composer and
  // one that does not has lost nothing.
  await neosh.keymap.set("chat", "<A-y>", "session.copy.path", {
    desc: "Copy this conversation's directory",
  });
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
  w.subscriptions.push(
    await neosh.cmd.register("project.remove", async (args, key) => {
      // The row under the cursor when there is one, so the palette entry does something useful from
      // inside the panel and a plugin can still name a directory outright.
      const here = w.panel(key?.view);
      const cwd = args[0]?.trim() || owningProject(here?.list.value);
      if (!cwd) {
        neosh.notify("project.remove needs a directory", "warn");
        return;
      }
      await removeProject(neosh, arrangement, cwd);
      // Every terminal's, because a project going away is a row missing from all of them.
      w.each((p) => p.draw());
    }, { desc: "Take a project off the list" }),
  );

  await neosh.keymap.set("chat", "<C-b>", "sidebar.toggle", { desc: "Toggle the sidebar" });
  await neosh.keymap.set("chat", "<C-t>", "sidebar.focus", { desc: "Projects and conversations" });
  await neosh.keymap.set("chat", "<C-n>", "session.new", { desc: "New conversation" });
  await neosh.keymap.set("chat", "<C-o>", "project.open", { desc: "Add a project" });

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
      await openRemote(neosh, w.subscriptions, node, session);
    }, { desc: "Watch a conversation on another computer" }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("swarm.nodes", () => showNodes(neosh), {
      desc: "The computers in this workspace",
    }),
  );
  w.subscriptions.push(
    await neosh.cmd.register("swarm.add", () => addComputer(neosh), {
      desc: "Add a computer by its address",
    }),
  );
  // `^J` rather than a letter, because everything in chat mode has to be a chord — the composer is
  // the field, and a bare key would be a character you can no longer type.
  await neosh.keymap.set("chat", "<C-j>", "swarm.nodes", { desc: "Computers" });
  // Any change over there is a redraw here. Without it the panel is right only as often as its
  // four-second refresh, which is a long time to watch a spinner that has already stopped.
  w.subscriptions.push(neosh.swarm.onChange(() => w.each((p) => p.draw())));

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
 * They are real bindings all the same. `^Z` lists them with the contributed label, `^K` runs the
 * command, and a user who dislikes the key rebinds it exactly as they would one of ours.
 */
function installActions(
  neosh: Neosh,
  panel: (view?: ViewId) => Panel | null,
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
    const reserved = await reservedKeys(neosh);

    // Every action still gets a command of its own — `^K` runs it by name whether or not it won a
    // key — but the *binding* is per key, not per action. Two plugins may each want `<Tab>` on
    // their own rows and both are right; one `keymap.set` per action made that the second one
    // overwriting the first, so whichever loaded last owned the key everywhere and the other
    // plugin's rows had a key that did nothing. One binding, and the row under the cursor decides
    // which of them it means.
    const sharing = new Map<string, Array<Contribution & { item: ActionItem }>>();
    for (const c of valid) {
      const name = `${NS}.action.${c.plugin}.${c.id}`;
      const command = await neosh.cmd.register(name, async (_args, key) => {
        // The row under *this* terminal's cursor. A contributed verb is pressed in one panel, and
        // there may be three of them open on three different rows.
        const target = panel(key?.view)?.list.value;
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
      if (reserved.has(c.item.key)) {
        neosh.log.warn(
          `${c.plugin} asked for '${c.item.key}' in the sidebar, which is already a panel key`,
        );
        continue;
      }
      const share = sharing.get(c.item.key);
      if (share) share.push(c);
      else sharing.set(c.item.key, [c]);
    }

    for (const [key, claimants] of sharing) {
      // The narrow verbs first: a verb about the row you are standing on beats one about every row,
      // which is the same ranking the hint strip prints them in and for the same reason. A key that
      // meant "cycle the plan detail" wherever the cursor was would be a key that did the wrong
      // thing on somebody else's block.
      const ranked = [
        ...claimants.filter((c) => (c.item.on ?? "any") !== "any"),
        ...claimants.filter((c) => (c.item.on ?? "any") === "any"),
      ];
      const name = ranked.length === 1 && ranked[0]
        ? `${NS}.action.${ranked[0].plugin}.${ranked[0].id}`
        : `${NS}.action.key.${key}`;
      if (ranked.length > 1) {
        const command = await neosh.cmd.register(name, async (_args, k) => {
          const target = panel(k?.view)?.list.value;
          const hit = ranked.find((c) => applies(c.item.on ?? "any", target));
          if (!hit) return;
          await neosh.cmd.exec(hit.item.command, argsFor(target)).catch((e: unknown) => {
            neosh.notify(String(e), "warn");
          });
          onChange();
        }, { desc: ranked.map((c) => c.item.label).join(" / ") }).catch(() => null);
        if (command) registered.push(command);
      }
      const label = ranked[0]?.item.label ?? key;
      await neosh.keymap.set("chat", key, name, { scope, desc: label }).catch(() => {});
      bound.push({ key, command: name });
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

/**
 * The keys this panel has already spoken for. A contribution asking for one of these is a bug in it.
 *
 * Read off the registry rather than kept in a list here: the list this replaced was missing
 * `gg`, `G`, `^D`, `^U`, `⇥` and every digit — keys the panel binds a few hundred lines up — so a
 * contributed `G` silently replaced *go to the bottom* with no warning to anybody. A fact about
 * which keys are bound is a fact the keymap table already has.
 */
async function reservedKeys(neosh: Neosh): Promise<Set<string>> {
  const all = await neosh.keymap.list("chat").catch(() => []);
  return new Set(
    all
      .filter((k) =>
        k.scope.kind === "buf_kind" && k.scope.name === KIND &&
        !k.command.startsWith(`${NS}.action.`)
      )
      .map((k) => k.lhs),
  );
}

/**
 * Whether a contributed verb is about the row under the cursor.
 *
 * `custom:<section id>` is the narrow form and the one most contributors want: a verb about *my*
 * block rather than about every block anybody has put in this column. Bare `custom` is still every
 * contributed row, because a verb about contributions in general is a real thing to want — but it
 * is not what "a key on my rows" means, and until this existed it was the only spelling available.
 * Two plugins each asking for `<Tab>` on their own rows were one binding, one winner and one plugin
 * whose key quietly did nothing.
 */
function applies(on: string, target: Target | undefined): boolean {
  if (on === "any") return target !== undefined;
  if (on.startsWith("custom:")) {
    return target?.kind === "custom" && target.section === on.slice("custom:".length);
  }
  return target?.kind === on;
}

/** The row under the cursor, as arguments a command can act on. */
function argsFor(target: Target | undefined): string[] {
  if (!target) return [];
  if (target.kind === "session") return ["session", target.cwd, target.id];
  if (target.kind === "project") return ["project", target.cwd];
  // Which block it came from, so a verb bound across several sections can tell them apart without
  // the panel having to hand over the row's own command as well.
  if (target.kind === "custom") return ["custom", target.section ?? ""];
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
 * **`base` is which project is being asked about**, and it is the whole reason this takes an
 * argument: `^N` means the conversation you are in, and `n` in the panel means the row the cursor
 * is on, which is very often a different repository with different branches. Asking about the
 * active conversation's checkout from a row pointing somewhere else would offer worktrees that
 * have nothing to do with what was aimed at.
 *
 * Outside a repository the question has one answer, so it is not asked: `^N` stays a single key
 * everywhere the choice would be theatre. Inside one, `Here` is selected, so `^N ⏎` is what `^N`
 * always did.
 *
 * **The worktrees it offers are the ones on the panel's list**, which is what `arrangement` is for
 * here. Every tree `git worktree list` knows is not the same set: a scratch branch you finished
 * with in March is still a checkout on disk long after `X` took it off the list, so this picker
 * was offering back, under "an existing one", every project you had ever removed — twenty rows of
 * finished work above the three you are actually in, and choosing one put it straight back in the
 * sidebar. The list is the workspace's answer to *which places do I work in*, and
 * this question is asking exactly that. A worktree neosh has never heard of is not on it either,
 * which is correct rather than a casualty: `Another directory…` lists every tree git knows and is
 * how a directory joins the list in the first place.
 */
async function newConversation(
  neosh: Neosh,
  arrangement: Arrangement,
  base?: string,
): Promise<void> {
  const current = await neosh.session.current().catch(() => null);
  const here = base ?? current?.cwd;
  // Empty outside a repository, which is also how this knows there is nothing to ask about: a
  // worktree is a git idea, and a directory that is not a checkout has exactly one answer to
  // "where does this conversation go".
  const trees = await neosh.git.worktrees(here ? { cwd: here } : undefined)
    .catch(() => [] as WorktreeInfo[]);
  // The git plugin may also be switched off, in which case the rows that lead into it would lead
  // nowhere. Offering an action that cannot happen is worse than not offering it.
  const commands = new Set((await neosh.cmd.list().catch(() => [])).map((c) => c.name));
  const canBranch = trees.length > 0 && commands.has("git.worktree.new.auto");
  const canInside = trees.length > 0 && commands.has("git.worktree.new.inside");
  const canName = trees.length > 0 && commands.has("git.worktree.new");
  // On the list, and not merely on disk. `here` is dropped because you are already in it.
  const known = new Set(arrangement.all());
  const others = trees.filter((t) => t.path !== here && known.has(t.path));

  if (!canBranch && !canInside && !canName && others.length === 0) {
    await neosh.session.create(here ? { cwd: here } : undefined);
    return;
  }

  const rows: Array<PickerItem<Where>> = [
    {
      label: "Here",
      detail: here ?? "this project",
      icon: "●",
      hl: "Diagnostic.Info",
      value: { kind: "here" },
    },
  ];
  // First, and above the one that asks for a name, because it is the answer more often: a branch
  // named before the work is a decision made at the worst possible moment, and the whole reason
  // this row exists is that having to make it is what stops people starting.
  if (canBranch) {
    rows.push({
      label: "In a new worktree",
      detail: "a clean branch, named for you, nothing to answer",
      keywords: "branch worktree scratch new fresh",
      icon: "+",
      hl: "Diagnostic.Ok",
      value: { kind: "scratch" },
    });
  }
  if (canInside) {
    // Where it will land, said in the row: a choice between two places is only a choice if both
    // rows name theirs. A relative `worktree.root` renames this directory, so it is read rather
    // than assumed.
    const configured = ((await neosh.opt.get<string>("worktree.root").catch(() => "")) ?? "")
      .trim();
    const dir = configured !== "" && !configured.startsWith("/")
      ? configured.replace(/\/+$/, "")
      : ".worktrees";
    rows.push({
      label: "In a new worktree, in this project",
      detail: `kept in ${dir}/ — travels with the repository`,
      keywords: "branch worktree inside project local",
      icon: "⌂",
      hl: "Accent",
      value: { kind: "inside" },
    });
  }
  if (canName) {
    rows.push({
      label: "In a new worktree, named…",
      detail: "a branch of its own, checked out somewhere else",
      keywords: "branch worktree name",
      icon: "+",
      hl: "Sidebar.Dim",
      value: { kind: "new" },
    });
  }
  // Somewhere else, before the list of trees rather than after it. Not because it is the commonest
  // answer — it is not — but because the list above it is unbounded, and a verb that sits under an
  // unbounded list is a verb whose distance from the cursor depends on how long you have used the
  // program. It is also the row that says the field completes paths, and a sentence about how to
  // type is no use once you have already scrolled past it looking for somewhere to type.
  rows.push(elsewhereRow("Another directory…"));
  for (const t of others) {
    const label = t.branch ?? t.head ?? t.path;
    rows.push({
      label,
      detail: `worktree  ·  ${t.path}`,
      keywords: `${t.path} worktree`,
      icon: "⎇",
      hl: "Git.Branch",
      value: { kind: "tree", path: t.path, label },
    });
  }

  const chosen = await whereTo(neosh, rows, { title: "New conversation" });
  if (chosen === null) return;
  // Every row below that reaches for git names the repository it is about. `^N` from a
  // conversation and `n` from a row on a different project are the same code path, and the only
  // thing that tells them apart is this argument.
  const at = here ? [here] : [];
  switch (chosen.kind) {
    case "here":
      await neosh.session.create(here ? { cwd: here } : undefined);
      return;
    case "scratch":
      await neosh.cmd.exec("git.worktree.new.auto", at)
        .catch((e: unknown) => neosh.notify(String(e), "warn"));
      return;
    case "inside":
      await neosh.cmd.exec("git.worktree.new.inside", at)
        .catch((e: unknown) => neosh.notify(String(e), "warn"));
      return;
    case "new":
      // Positionally: branch, path, cwd. The first two are what the command asks for when they are
      // missing, which is exactly what this row means.
      await neosh.cmd.exec("git.worktree.new", here ? ["", "", here] : [])
        .catch((e: unknown) => neosh.notify(String(e), "warn"));
      return;
    case "tree":
      // No message: creating a conversation switches to it, so the transcript is empty, the
      // composer has the keyboard and the sidebar row is selected. All three say it already.
      await neosh.session.create({ cwd: chosen.path });
      return;
    case "host":
      await startThere(neosh, chosen);
      return;
    case "elsewhere": {
      const path = chosen.path ?? (await pathPicker(neosh, "New conversation"));
      if (path === null || path.trim() === "") return;
      try {
        await neosh.session.create({ cwd: path.trim() });
      } catch (e) {
        neosh.notify(String(e), "warn");
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Where — one field for every answer to "which directory, on which computer"
// ---------------------------------------------------------------------------

/**
 * Every answer `^N` and `^O` accept.
 *
 * `elsewhere` carries its path when the field already produced one, and leaves it off when the row
 * was chosen from the menu — the difference between "you typed a directory" and "you asked to be
 * asked for one".
 */
type Where =
  | { kind: "here" }
  | { kind: "scratch" }
  | { kind: "inside" }
  | { kind: "new" }
  | { kind: "elsewhere"; path?: string }
  | { kind: "tree"; path: string; label: string }
  /** On another computer, in one of its directories. */
  | { kind: "host"; node: string; cwd: string; label: string }
  /** Look at one machine's directories rather than every machine's projects. */
  | { kind: "machine"; node: string; name: string }
  /** A machine that is on the list and cannot be used yet. `↵` says which of those it is. */
  | { kind: "unreachable"; name: string; why: string };

/** The row that drops through to a path, wherever a caller wants it in the order. */
function elsewhereRow(label: string): PickerItem<Where> {
  return {
    label,
    // What the field does, said on the row that is about the field. The alternative was a hint
    // strip nobody reads until they already know what they are looking for.
    detail: "or type a path — `/`, `~` and `./` complete as you go",
    keywords: "path folder directory type browse elsewhere",
    icon: "…",
    hl: "Sidebar.Dim",
    value: { kind: "elsewhere" },
  };
}

/**
 * What the field is completing against, read off what has been typed.
 *
 * Three states and one field, because the alternative is a mode — and a mode is a thing you have to
 * be in before you can type, which is the entire complaint this replaced. A path is a path the
 * moment it starts like one; `<machine>:` is scp's syntax and means there what it means here, which
 * is the only reason it is worth having a syntax at all.
 */
type Field =
  | { kind: "rows" }
  | { kind: "local"; path: string }
  | { kind: "remote"; node: SwarmNode; path: string };

/** Whether a query has started to look like a path rather than a search. */
function looksLikePath(q: string): boolean {
  return q.startsWith("/") || q.startsWith("~") || q.startsWith("./") || q.startsWith("../");
}

/**
 * Split `linux-box:/home/me/src` into the machine and the path.
 *
 * Matched on what the machine is *called here* — the alias if you set one, since that is the name
 * every panel shows and therefore the only one you could have read off the screen — and on the
 * start of its id, which is what you have when two machines share a name. A colon in a query that
 * names no machine is not a host prefix: it is a directory with a colon in it, which is legal on
 * every filesystem this runs on.
 */
function readField(query: string, nodes: SwarmNode[]): Field {
  const at = query.indexOf(":");
  if (at > 0) {
    const who = query.slice(0, at).toLowerCase();
    const node = nodes.find((n) =>
      n.info.name.toLowerCase() === who || n.info.id.toLowerCase().startsWith(who)
    );
    if (node) return { kind: "remote", node, path: query.slice(at + 1) };
  }
  return looksLikePath(query) ? { kind: "local", path: query } : { kind: "rows" };
}

/** How a machine's rows are prefixed, and what `<Tab>` puts back in the field. */
function hostPrefix(node: SwarmNode): string {
  // The name, unless it has a colon or a space in it — in which case the id is the only thing that
  // round-trips through a field whose separator is a colon.
  return /[:\s]/.test(node.info.name) ? node.info.id.slice(0, 8) : node.info.name;
}

/**
 * The machines, as rows you can go into.
 *
 * Every paired machine, including the ones that cannot be used — greyed, with the reason on them.
 * The list used to skip anything not `up` and not accepting commands, which is right about what can
 * be *done* and wrong about what should be *said*: somebody who has just paired a computer and is
 * looking for it finds an empty list and nothing anywhere to explain it, which reads as the feature
 * not existing rather than as the machine not being ready.
 */
function machineRows(nodes: SwarmNode[], ascii: boolean): PickerItem<Where>[] {
  const out: PickerItem<Where>[] = [];
  for (const n of nodes) {
    const usable = n.up && n.capabilities.accepts_commands;
    const projects = n.capabilities.projects;
    const conversations = projects.reduce((sum, p) => sum + p.sessions, 0);
    const working = projects.reduce((sum, p) => sum + p.running, 0);
    const state = n.link.state === "up"
      ? [
        `${projects.length} ${projects.length === 1 ? "project" : "projects"}`,
        conversations > 0 ? `${conversations} open` : "",
        working > 0 ? `${working} working` : "",
        n.capabilities.accepts_commands ? "" : "read-only — nothing can be started there",
      ].filter(Boolean).join("  ·  ")
      : n.link.state === "waiting"
        ? "has not allowed this computer yet — ^J there"
        : n.link.state === "down"
          ? "disconnected — ^J to reconnect"
          : "connecting…";
    const [icon, hl] = n.link.state === "up"
      ? usable ? [ascii ? "*" : "●", "Diagnostic.Ok"] : [ascii ? "-" : "◐", "Sidebar.Dim"]
      : n.link.state === "waiting"
        ? [ascii ? "!" : "◍", pulseHl("Status.Pending")]
        : n.link.state === "down"
          ? [ascii ? "-" : "○", "Sidebar.Dim"]
          : [spinnerFrame(), "Status.Streaming"];
    out.push({
      label: n.info.name,
      // The prefix on every row, so the syntax is learned by reading rather than by being told.
      detail: `${state}   ${hostPrefix(n)}:`,
      keywords: `${n.info.id} ${n.info.os} computer machine remote host ${hostPrefix(n)}`,
      icon,
      hl,
      value: usable
        ? { kind: "machine", node: n.info.id, name: hostPrefix(n) }
        // Nothing to go into, so `↵` says why rather than opening an empty list — and stays here,
        // because being told a machine is not ready is not a reason to lose the list you were in.
        : { kind: "unreachable", name: n.info.name, why: state },
    });
  }
  return out;
}

/** One machine's directories, once you are inside it. */
function projectRows(node: SwarmNode, ascii: boolean): PickerItem<Where>[] {
  const prefix = hostPrefix(node);
  return node.capabilities.projects.map((p) => ({
    // The prefixed path, because `<Tab>` puts a row's *label* back in the field and a bare name
    // would take the field somewhere that is not a directory on any machine.
    label: `${prefix}:${p.cwd}`,
    detail: [
      p.name,
      p.sessions > 0
        ? `${p.sessions} ${p.sessions === 1 ? "conversation" : "conversations"}`
        : "nothing open",
      p.running > 0 ? `${p.running} working` : "",
    ].filter(Boolean).join("  ·  "),
    keywords: `${p.name} ${p.key} remote`,
    // Filled when somebody is working in it, hollow when nobody is. The one thing the flag could
    // not say while every project on the list was one with a conversation in it.
    icon: p.active ? (ascii ? "*" : "●") : (ascii ? "-" : "○"),
    hl: p.active ? "Diagnostic.Ok" : "Sidebar.Dim",
    value: {
      kind: "host" as const,
      node: node.info.id,
      cwd: p.cwd,
      label: `${p.name} on ${node.info.name}`,
    },
  }));
}

/**
 * The one question both `^N` and `^O` ask: which directory, on which computer.
 *
 * A picker whose filter line **is** the field. Type nothing and it is the menu the caller passed;
 * type `/`, `~` or `./` and it is a directory completer; type `linux-box:` and it is a directory
 * completer pointed at that machine. Nothing has to be navigated to first, which was the whole
 * complaint: the path was a row at the bottom of a list, under an unbounded number of worktrees,
 * and reaching it meant arrowing past everything the program had ever been told about.
 *
 * `<Tab>` walks into the highlighted directory and `↵` takes it, which is `pathPicker`'s bargain
 * and is why a completion row's label has to be the whole path.
 *
 * Choosing a machine reopens this seeded with `<machine>:` rather than descending in place: the
 * picker owns its own filter line, and a second level whose rows lie about what is in the field is
 * a `<Tab>` that jumps somewhere nobody asked for.
 */
async function whereTo(
  neosh: Neosh,
  base: PickerItem<Where>[],
  opts: { title: string; seed?: string },
): Promise<Where | null> {
  const ascii = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false;
  let seed = opts.seed ?? "";

  /**
   * The machines, and the rows built from them. Reread rather than captured, because the list is
   * live: a peer connecting, or gaining a project, while this is open has to change the rows under
   * the cursor. Captured once, `subscribe` would re-rank the same stale array for ever and the
   * subscription would be a promise the picker was not keeping.
   */
  let nodes: SwarmNode[] = [];
  // Mutated in place, never reassigned: `reload` re-ranks the very array the picker was handed,
  // so a fresh array here would be a list that never caught up with anything.
  const rows: PickerItem<Where>[] = [];
  const refill = async () => {
    const me = (await neosh.swarm.self().catch(() => null))?.id ?? null;
    // This machine is in `nodes()` like any other — a fleet plugin walking the list reaches its
    // own node, which is deliberate — and it is the one row that must not be here: every other row
    // in this picker already means "on this computer", and `mymac:/tmp` would otherwise start a
    // local conversation by the remote path and then try to *watch* it over a socket to ourselves.
    nodes = (await neosh.swarm.nodes().catch(() => [] as SwarmNode[]))
      .filter((n) => n.info.id !== me);
    // No `Add a computer…` row, deliberately. The swarm is on by default, so on every
    // single-machine workspace — which is most of them — that row would be a permanent line in a
    // menu pressed many times a day, about a thing its reader is not doing. It is the archive-count
    // argument one panel along. This picker lists the computers you *have*; getting one is `^J`,
    // which has a key of its own and a line in the legend.
    const machines = machineRows(nodes, ascii);
    rows.splice(0, rows.length, ...base, ...machines);
    return machines.length > 0;
  };

  // The last refusal reported, so a folder macOS is hiding is said once and not once per keystroke.
  let saidDenied: string | null = null;

  for (;;) {
    const anyMachines = await refill();

    const chosen = await picker<Where>(neosh, rows, {
      title: opts.title,
      width: 84,
      height: 14,
      query: seed,
      placeholder: "nothing matches — <CR> takes the path you typed",
      hints: anyMachines
        ? "↵ choose   ⇥ into a directory   / a path   name: another computer"
        : "↵ choose   ⇥ into a directory   / or ~ for a path",
      source: async (query) => {
        const field = readField(query, nodes);
        if (field.kind === "rows") {
          // Our own ranking, because a `source` replaces the picker's filtering wholesale. Same
          // fuzzy scorer the picker uses, over the same label-and-keywords, so a query that is not
          // a path behaves exactly as it would with no source at all.
          if (query.trim() === "") return rows;
          return rows
            .map((item) => ({ item, m: fuzzy(`${item.label} ${item.keywords ?? ""}`, query) }))
            .filter((r): r is { item: PickerItem<Where>; m: Match } => r.m !== null)
            .sort((a, b) => b.m.score - a.m.score)
            .map((r) => r.item);
        }
        if (field.kind === "local") {
          const answer = await neosh.path
            .complete(field.path)
            .catch(() => ({ paths: [] as string[], denied: undefined }));
          // Same rule the remote branch below follows: a directory the OS refused is said out
          // loud, never drawn as a directory with nothing in it. Once per reason — this runs on
          // every keystroke.
          if (answer.denied && answer.denied !== saidDenied) {
            saidDenied = answer.denied;
            neosh.notify(answer.denied, "warn");
          } else if (!answer.denied) {
            saidDenied = null;
          }
          return answer.paths.map((path) => ({
            label: path,
            icon: ascii ? ">" : "▸",
            hl: "Sidebar.Dim",
            value: { kind: "elsewhere" as const, path },
          }));
        }
        const prefix = hostPrefix(field.node);
        // Nothing typed after the colon: what that machine offers, which is a short list of real
        // places rather than the contents of `/`. Typing on turns it into a directory listing.
        //
        // A machine with nothing on that list falls through to its home directory rather than
        // showing an empty one — a computer you have just paired has never had a project on it,
        // and "no rows" is the answer least likely to be read as "and here is where to start".
        const offered = field.path === "" ? projectRows(field.node, ascii) : [];
        if (offered.length > 0) return offered;
        const paths = await neosh.swarm
          .browse(field.node.info.id, field.path === "" ? "~/" : field.path)
          .catch((e) => {
            // A peer that went away mid-keystroke, or one too old to answer. Said out loud rather
            // than drawn as an empty directory, which is the one reading that is never true.
            neosh.notify(String(e), "warn");
            return [] as string[];
          });
        return paths.map((path) => ({
          label: `${prefix}:${path}`,
          detail: field.node.info.name,
          icon: ascii ? ">" : "▸",
          hl: "Sidebar.Remote",
          value: {
            kind: "host" as const,
            node: field.node.info.id,
            cwd: path,
            label: `${path} on ${field.node.info.name}`,
          },
        }));
      },
      // What `↵` means when the field holds something no row offered — which for a directory you
      // know the name of is the ordinary case, not the exception.
      freeform: (query) => {
        const field = readField(query, nodes);
        if (field.kind === "local") return { kind: "elsewhere", path: query.trim() };
        if (field.kind === "remote" && field.path.trim() !== "") {
          return {
            kind: "host",
            node: field.node.info.id,
            cwd: field.path.trim(),
            label: `${field.path.trim()} on ${field.node.info.name}`,
          };
        }
        return null;
      },
      // A machine connecting, or gaining a project, while the list is open. Same reason the
      // computers panel subscribes: a row that only changes on the next press is a row that was
      // wrong when it was read. `refill` first, or the reload re-ranks the same stale array.
      subscribe: (reload) => neosh.swarm.onChange(() => void refill().then(reload)),
    });

    if (chosen === null) return null;
    if (chosen.kind === "machine") {
      // Back into the same picker with the machine's prefix already typed, which is exactly the
      // state somebody would have reached by typing it — so the two routes cannot diverge.
      seed = `${chosen.name}:`;
      continue;
    }
    if (chosen.kind === "unreachable") {
      neosh.notify(`${chosen.name}: ${chosen.why}`, "info");
      seed = "";
      continue;
    }
    return chosen;
  }
}

/** Start a conversation on another machine, and go and look at it. */
async function startThere(
  neosh: Neosh,
  chosen: { node: string; cwd: string; label: string },
): Promise<void> {
  // Started *there*. The agent runs on that machine, against that machine's files, which is the
  // point — this is not a way to work on a remote directory from here.
  try {
    const session = await neosh.swarm.command(chosen.node, "", {
      command: "new_session",
      cwd: chosen.cwd,
      title: null,
    });
    // Opened, not merely reported. Starting a conversation here switches to it, and one started
    // over there that only produced a toast was the same key doing visibly less for the same
    // intention — you had to go and find, in `^J`, the thing you had just made.
    if (session) {
      await neosh.cmd.exec("swarm.open", [chosen.node, session]).catch(() => {});
      return;
    }
    neosh.notify(`started ${chosen.label}`);
  } catch (e) {
    neosh.notify(String(e), "warn");
  }
}

async function activateTarget(
  neosh: Neosh,
  arrangement: Arrangement,
  target: Target | undefined,
  p: Panel,
): Promise<void> {
  if (!target) return;
  // Somebody else's row. Nothing here knows what it does, which is the point — the contribution
  // named a command and this invokes it.
  if (target.kind === "custom") {
    if (!target.command) return;
    await neosh.cmd.exec(target.command, target.args).catch((e: unknown) => {
      neosh.notify(String(e), "warn");
    });
    await p.draw();
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
    await p.leave();
    await neosh.cmd.exec("project.open").catch(() => {});
    return;
  }
  if (target.kind === "project") {
    // A pinned project with nothing in it yet: `↵` should start something, not fold an empty list.
    // A repository whose conversations are all in its worktrees is not that — the row has children
    // to fold, which is what `s.repo_root === target.cwd` finds.
    const sessions = await neosh.session.list().catch(() => [] as SessionInfo[]);
    if (!sessions.some((s) => s.cwd === target.cwd || s.repo_root === target.cwd)) {
      await p.here.session.create({ cwd: target.cwd });
      await p.leave();
      return;
    }
    await arrangement.toggleFold(target.cwd);
    await p.draw();
    return;
  }
  try {
    await p.here.session.switch(target.id);
  } catch (e) {
    neosh.notify(String(e), "warn");
    return;
  }
  await p.leave();
}

/** The project a row belongs to — its own, or the one its conversation is in. */
function owningProject(target: Target | undefined): string | null {
  if (!target) return null;
  return target.kind === "project" || target.kind === "session" ? target.cwd : null;
}

/**
 * Which directory to add — here, or on another computer.
 *
 * **The path is the first thing now, not the last.** This was a list of the current repository's
 * worktrees with `Type a path…` under it, and a path is what somebody pressing this key almost
 * always has: the row they wanted sat at the bottom of a list they had to read to the end of every
 * time, and the list was of the one place they were already in. So the filter line *is* the path
 * field from the first keystroke — `/`, `~` and `./` complete directories as you go — and the
 * worktrees are suggestions under it rather than a menu in front of it.
 *
 * And the machines are on it, because "add a project" never had a reason to mean "on this
 * computer". `linux-box:` completes over there and `↵` starts the conversation there; the row for
 * it arrives in the panel the way every other remote conversation's does.
 *
 * Answers `null` for anything that was not a local directory — including a remote one, which has
 * been acted on by then. The caller's job is to open a path here, and a path on somebody else's
 * disk is not one.
 */
async function chooseDirectory(neosh: Neosh): Promise<string | null> {
  const [worktrees, sessions] = await Promise.all([
    neosh.git.worktrees().catch(() => []),
    neosh.session.list().catch(() => [] as SessionInfo[]),
  ]);
  const open = new Set(sessions.map((s) => s.cwd));

  const rows: PickerItem<Where>[] = [elsewhereRow("Type a path…")];
  for (const t of worktrees.filter((w) => !open.has(w.path))) {
    rows.push({
      // The path, not the basename: `<Tab>` puts a row's label back in the field, and a bare
      // directory name there is a path relative to somewhere nobody chose.
      label: t.path,
      detail: [basename(t.path), t.branch ?? ""].filter(Boolean).join("  ·  "),
      keywords: `${basename(t.path)} worktree`,
      icon: "⎇",
      hl: "Git.Branch",
      value: { kind: "elsewhere", path: t.path },
    });
  }

  const chosen = await whereTo(neosh, rows, { title: "Add project" });
  if (chosen === null) return null;
  if (chosen.kind === "host") {
    await startThere(neosh, chosen);
    return null;
  }
  if (chosen.kind !== "elsewhere") return null;
  // Chosen from the menu rather than typed: that row means "ask me", and this is the asking.
  return chosen.path ?? (await pathPicker(neosh, "Add project"));
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
 * Take a project off the list.
 *
 * Empty, this asks nothing: no file moves, the row goes, and `o` puts the directory back in one
 * keystroke — a dialog for that is the kind you learn to clear without reading. With conversations
 * still in it, removing the project is deleting every one of them, so it asks as the delete it is
 * and says how many and where they would otherwise go.
 *
 * The conversations go first and the project only goes if they all did: a project removed anyway
 * would take a row that still had something in it off the list.
 *
 * Removing the *last* project used to be the one case this could not finish: the store refused to
 * delete the very last conversation in the workspace. It no longer does — what you land in is a
 * placeholder, which is in no list and so in no project — so `X` here can leave the panel with
 * nothing but `+ Add project` on it.
 */
async function removeProject(
  neosh: Neosh,
  arrangement: Arrangement,
  cwd: string,
): Promise<void> {
  const inside = (await neosh.session.list({ includeArchived: true }).catch(() => [] as SessionInfo[]))
    .filter((s) => s.cwd === cwd);
  const name = inside[0]?.project || arrangement.name(cwd) || basename(cwd);

  if (inside.length > 0) {
    const many = inside.length === 1 ? "conversation" : `${inside.length} conversations`;
    const ok = await confirmDestructive(neosh, `Remove "${clip(name, 40)}" and its ${many}?`, {
      yes: "Delete",
      no: "Keep",
      detail: [
        atStake(inside),
        "`x` on a conversation archives it instead, and an archived one is already out of the way.",
      ],
    });
    if (!ok) return;
    for (const s of inside) {
      try {
        await neosh.session.close(s.id);
      } catch (e) {
        neosh.notify(String(e), "warn");
        return;
      }
    }
  }

  await arrangement.forget(cwd);
  neosh.notify(`removed ${short(cwd)} — \`o\` adds it back`);
}

/**
 * How much is about to go, in a sentence.
 *
 * What is at stake is the point of the question, and the number that says it is how many of these
 * you would still have found in the panel: something you archived a month ago going is a different
 * size of loss from a conversation you were in this morning.
 */
function atStake(inside: SessionInfo[]): string {
  const archived = inside.filter((s) => s.archived).length;
  if (inside.length === 1) {
    return archived === 1
      ? "It is archived, and it goes from disk with the project."
      : "It goes from disk with the project.";
  }
  if (archived === inside.length) return `All ${inside.length} are archived, and all of them go from disk.`;
  if (archived === 0) return `All ${inside.length} go from disk.`;
  const live = inside.length - archived;
  return `${live} of them ${live === 1 ? "is" : "are"} still in your list, and all ${inside.length} go from disk.`;
}

/**
 * The computers in this workspace: yours, theirs, and the ones asking to join.
 *
 * A picker rather than a panel section, for the reason the archive is: it is a thing you look at
 * when you want it, and rows that are only occasionally the answer do not belong in the column you
 * work in.
 *
 * Machines asking to join sit at the top, because they are the only rows that are waiting on you.
 * Your own identity is at the foot, where it is out of the way but always somewhere you can point
 * at when the other machine asks who you are.
 */
async function showNodes(neosh: Neosh): Promise<void> {
  const me = await neosh.swarm.self().catch(() => null);
  if (!me) {
    // On by default, so off means somebody turned it off — or `--clean`, which has nowhere to
    // keep the key that *is* this machine's identity.
    neosh.notify(
      "the swarm is off — remove `enabled = false` from `[swarm]`, or leave --clean",
      "info",
    );
    return;
  }
  // What the listener managed, said on this machine's own row: an address a peer could dial, or
  // why there is not one. Written by the host as a workspace var, because it is a fact about the
  // workspace and not about whichever panel happened to ask.
  const listening = await neosh.vars
    .get<{ addr: string | null; error?: string }>({ scope: "global" }, "swarm.listen")
    .catch(() => null);
  const ascii = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false;

  type Row =
    | { kind: "node"; node: SwarmNode }
    | { kind: "stranger"; stranger: SwarmStranger }
    | { kind: "add" }
    | { kind: "self" };

  // The last answer from the host, so a spinner frame costs a redraw rather than two round trips.
  // At 80ms a tick, rebuilding these over the API would be the most talkative thing in the
  // workspace, to animate one glyph.
  let nodes: SwarmNode[] = [];
  let strangers: SwarmStranger[] = [];

  /** Whether anything on screen is mid-dial, and therefore whether the tick has any work. */
  const spinning = () =>
    nodes.some((n) => n.link.state === "connecting" || n.link.state === "retrying");

  const build = (): PickerItem<Row>[] => {
    const rows: PickerItem<Row>[] = [];

    for (const s of strangers) {
      rows.push({
        label: `${s.info.name}`,
        detail: s.dialled
          ? `found at ${s.addr ?? "an address you gave"}  ·  ${fingerprint(s.info.id)}  ·  ↵ to add`
          : `wants to join  ·  ${fingerprint(s.info.id)}  ·  ↵ to allow`,
        keywords: `${s.info.id} pending join pair new`,
        // A machine asking to join is the one row here that wants answering, so it wears the
        // colour the workspace already uses for *act now* and pulses on the same 1 Hz duty cycle
        // as every other waiting indicator. Nothing else in this list moves unless it is working.
        icon: "?",
        hl: pulseHl("Status.Pending"),
        value: { kind: "stranger", stranger: s },
      });
    }

    for (const n of nodes) {
      const running = n.agents.filter((a) => a.state === "running").length;
      // One sentence per link state, because they are different answers to "why is it not here" —
      // and the fourth of them is the reason this reads the way it does. `waiting` is the far half
      // of pairing: reached, proven, and not yet allowed over there. It used to arrive as
      // `connecting` with attempt 0, which printed a bare `connecting…` and threw the only useful
      // sentence away, so adding a computer looked like a network fault for as long as anybody
      // was willing to watch it.
      const state = n.link.state === "up"
        ? `${n.agents.length} ${n.agents.length === 1 ? "conversation" : "conversations"}`
        : n.link.state === "waiting"
          // Reads on from the label, which is the machine's name — naming it again here gave
          // `linux-box  allow this computer on linux-box`. What the sentence has to carry is the
          // key and the fact that it is pressed somewhere else.
          ? "has not allowed this computer yet — ^J there"
          : n.link.state === "connecting"
            ? n.link.attempt > 0
              ? `connecting — try ${n.link.attempt}, ${n.reason ?? "no answer"}`
              : "connecting…"
            : n.link.state === "retrying"
              ? n.link.attempt > 0
                ? `reconnecting — try ${n.link.attempt}`
                : "reconnecting…"
              : `disconnected — ${n.reason ?? "it dials in"}  ·  ^R reconnects`;
      // The glyph says what the row is doing; the sentence says what to do about it. A spinner
      // only ever means "this machine is working on it" — never over `waiting`, where nothing on
      // this computer is trying and the motion would be a promise nobody is keeping.
      const [icon, hl] = n.link.state === "up"
        ? [ascii ? "*" : "●", "Diagnostic.Ok"]
        : n.link.state === "waiting"
          ? [ascii ? "!" : "◍", pulseHl("Status.Pending")]
          : n.link.state === "down"
            ? [ascii ? "-" : "○", "Sidebar.Dim"]
            : [spinnerFrame(), "Status.Streaming"];
      rows.push({
        label: n.info.name,
        detail: [
          state,
          running > 0 ? `${running} working` : "",
          n.up && !n.capabilities.accepts_commands ? "read-only" : "",
          n.info.os,
          fingerprint(n.info.id),
        ].filter(Boolean).join("  ·  "),
        keywords: `${n.info.os} ${n.info.id} ${n.link.state}`,
        icon,
        hl,
        value: { kind: "node", node: n },
      });
    }

    rows.push({
      // The `+` is the icon now that this list has a gutter, not the first character of the label:
      // with both it read `+ + Add a computer…`.
      label: "Add a computer…",
      detail: "its address on your network, or through Tailscale",
      keywords: "pair join new machine host",
      icon: "+",
      hl: "Sidebar.Dim",
      value: { kind: "add" },
    });
    rows.push({
      label: `This computer  ·  ${me.name}`,
      // The fingerprint is what somebody at the other machine compares against. Shown here so
      // "what is my id" never means leaving the program to run a command. Beside it, whether
      // this machine can be dialled at all — the first question when adding it from over there.
      detail: [
        fingerprint(me.id),
        listening?.addr
          ? `listening on ${listening.addr}`
          : listening?.error
            ? `not listening — ${listening.error}`
            : "dial-only",
        "^Y copies the full id",
      ].join("  ·  "),
      keywords: `self me ${me.id}`,
      value: { kind: "self" },
    });
    return rows;
  };

  /** Ask the host again. Everything else here draws from what this last brought back. */
  const fetch = async () => {
    [nodes, strangers] = await Promise.all([
      neosh.swarm.nodes().catch(() => []),
      neosh.swarm.strangers().catch(() => []),
    ]);
  };

  const redraw = () => {
    items.splice(0, items.length, ...build());
  };
  const refill = async () => {
    await fetch();
    redraw();
  };

  await fetch();
  const items: PickerItem<Row>[] = build();

  const chosen = await picker(neosh, items, {
    title: "Computers",
    width: 84,
    height: 14,
    hints: "↵ open   ^E rename   ^R reconnect   ^D disconnect   ^X remove   ^Y my id",
    // The list keeps up while it is open: a machine connecting, dropping, or moving from
    // "connecting" to a row of conversations changes under the cursor rather than on reopen.
    subscribe: (reload) => {
      const changed = neosh.swarm.onChange(() => {
        void refill().then(reload);
      });
      // And the spinner turns on the shared clock, off what the last change brought back rather
      // than off a fresh pair of API calls: a dial takes seconds, the clock ticks twelve times a
      // second, and asking the host what it knows on every frame to move one glyph would make
      // this panel the noisiest thing in the workspace. Nothing ticks while nothing is dialling,
      // so a list of connected machines costs exactly as much as it did before.
      const ticked = onTick(() => {
        if (!spinning()) return;
        redraw();
        reload();
      });
      return {
        dispose() {
          changed.dispose();
          ticked.dispose();
        },
      };
    },
    // Chords, because every bare letter a picker takes is a letter its filter can never contain.
    ownKeys: ["<C-y>", "<C-x>", "<C-r>", "<C-d>", "<C-e>"],
    async onKey(key, ctx) {
      if (key.key.code.kind !== "char" || !key.key.mods.ctrl) return;
      switch (key.key.code.c.toLowerCase()) {
        case "y":
          await neosh.edit.copy(me.id);
          neosh.notify("copied this computer's id");
          return "handled";
        case "r": {
          const row = ctx.item;
          if (row?.kind !== "node" || row.node.link.state === "up") return "handled";
          await neosh.swarm.reconnect(row.node.info.id).catch((e) => neosh.notify(String(e), "warn"));
          neosh.notify(`dialling ${row.node.info.name}…`);
          await refill();
          return "reload";
        }
        case "d": {
          const row = ctx.item;
          if (row?.kind !== "node" || row.node.link.state === "down") return "handled";
          await neosh.swarm.disconnect(row.node.info.id).catch((e) => neosh.notify(String(e), "warn"));
          neosh.notify(`disconnected ${row.node.info.name} — ^R takes it back`);
          await refill();
          return "reload";
        }
        case "e": {
          const row = ctx.item;
          if (row?.kind !== "node") return "handled";
          // Prefilled with what the row says, so this is an edit rather than a blank field you
          // have to remember the old name to fill in. Clearing it is how you go back to the
          // hostname, which is why an empty answer is not the same as `Esc`.
          const next = await prompt(neosh, `Call ${row.node.info.name}`, {
            initial: row.node.info.name,
            width: 60,
          });
          if (next === null) return "handled";
          try {
            await neosh.swarm.rename(row.node.info.id, next.trim() === "" ? null : next.trim());
            await refill();
            return "reload";
          } catch (e) {
            // Almost always "that one is in your config file", which names the field to change.
            neosh.notify(String(e), "warn");
            return "handled";
          }
        }
        case "x": {
          const row = ctx.item;
          if (row?.kind !== "node") return "handled";
          try {
            await neosh.swarm.unpair(row.node.info.id);
            neosh.notify(`removed ${row.node.info.name}`);
            await refill();
            return "reload";
          } catch (e) {
            // Almost always "that one is in your config file", which names the fix.
            neosh.notify(String(e), "warn");
            return "handled";
          }
        }
        default:
          return;
      }
    },
  });
  if (chosen === null) return;

  switch (chosen.kind) {
    case "self":
      await neosh.edit.copy(me.id);
      neosh.notify("copied this computer's id");
      return;
    case "add":
      await addComputer(neosh);
      return;
    case "stranger": {
      const s = chosen.stranger;
      // Asked even though we already know who it is, because knowing who it is *is* the question:
      // the fingerprint on screen has to be the one the person at the other machine is reading out.
      const ok = await confirm(neosh, `Add ${s.info.name}?`, {
        yes: "Add",
        no: "Not now",
        detail: [
          `${s.info.os}  ·  neosh ${s.info.version}`,
          fingerprint(s.info.id),
          "Check that fingerprint matches what the other computer shows under `This computer`.",
        ],
      });
      if (!ok) return;
      await neosh.swarm.pair(s.info.id, { name: s.info.name, addr: s.addr ?? undefined });
      return;
    }
    case "node": {
      // A row you cannot open should say why in the terms the row is in. "Nothing running" is true
      // of a machine that has not allowed this one yet, and is not what is wrong with it.
      if (chosen.node.link.state === "waiting") {
        neosh.notify(
          `${chosen.node.info.name} has not allowed this computer yet — press ^J there and add `
            + `${me.name}`,
          "info",
        );
        return;
      }
      if (!chosen.node.up) {
        neosh.notify(
          `${chosen.node.info.name} is not connected — ${chosen.node.reason ?? "still dialling"}`,
          "info",
        );
        return;
      }
      const newest = [...chosen.node.agents].sort((a, b) => b.updated_at - a.updated_at)[0];
      if (!newest) {
        neosh.notify(`${chosen.node.info.name} has nothing running`, "info");
        return;
      }
      await neosh.cmd.exec("swarm.open", [chosen.node.info.id, newest.session]).catch(() => {});
    }
  }
}

// ---------------------------------------------------------------------------
// Watching a conversation on another computer
// ---------------------------------------------------------------------------

/** The buffer kind the remote view publishes, so its keys are ordinary bindings. */
const VIEW_KIND = "neosh.swarm.view";

/** At most one open at a time, because there is one screen. */
let openView: { close: () => Promise<void>; node: string; session: string } | null = null;

/**
 * Watch a conversation on another machine.
 *
 * A float rather than the chat pane, and that is a decision rather than a shortcut. The chat pane
 * is *your* conversation: it is where your composer sends, what `^S` reads, and what the permission
 * mode belongs to. Putting somebody else's conversation there would mean every one of those
 * questions has two answers, and the one you get depends on what you last pressed. A window that is
 * plainly a window onto another machine cannot be confused for the thing you are working in.
 *
 * What it is *not* is read-only. `i` steers it, `^C` interrupts it — because "feels like it is on
 * this computer" is a claim about what you can do, not only about what you can see.
 */
async function openRemote(
  neosh: Neosh,
  subscriptions: PluginContext["subscriptions"],
  node: string,
  session: string,
): Promise<void> {
  // Switching from one remote conversation to another drops the first subscription. Otherwise a
  // morning of looking at machines leaves every one of them streaming every token here.
  if (openView) await openView.close();

  const who = (await neosh.swarm.nodes().catch(() => []))
    .find((n) => n.info.id === node);
  const agent = who?.agents.find((a) => a.session === session);
  const name = who?.info.name ?? node.slice(0, 8);

  const buf = await neosh.buf.create({
    name: `[${name}] ${agent?.label ?? "conversation"}`,
    scratch: true,
    kind: VIEW_KIND,
  });
  const ns = await neosh.ns.create("neosh.swarm.view");
  const lines: string[] = [
    `  watching ${name}${agent ? `  ·  ${agent.project_name}` : ""}`,
    `  ${agent?.cwd ?? ""}`,
    "",
    "  waiting for this machine to say what has happened so far…",
  ];
  await neosh.buf.setLines(buf, 0, -1, lines);

  const win = await neosh.float.open(buf, {
    anchor: { kind: "screen" },
    width: { kind: "max", n: 96 },
    height: { kind: "max", n: 28 },
    border: "rounded",
    title: ` ${name} `,
    focusable: true,
  });
  await neosh.focus.push(win);

  const header = lines.slice(0, 3);
  let body: string[] = [];
  /** Whether the last thing appended is an assistant turn still being written into. */
  let streaming = false;

  const redraw = async () => {
    const all = [...header, ...body];
    // One call. This redraws per token, so a repaint the frontend could draw halfway through — text
    // written, marks not yet — would be a transcript strobing white for the whole of an answer.
    const drawn: DrawnRow[] = all.map((text) => ({ text, marks: [] }));
    const mark = (line: number, hl: string, text: string) => {
      drawn[line]?.marks!.push({ col: 0, opts: { hlGroup: hl, endCol: byteLength(text) } });
    };

    mark(0, "Title", all[0] ?? "");
    if (all[1]) mark(1, "Comment", all[1]);
    for (let i = header.length; i < all.length; i++) {
      const text = all[i] ?? "";
      if (text.startsWith("  › ")) {
        mark(i, "Accent", text);
      } else if (text.startsWith("  · ")) {
        mark(i, "Comment", text);
      }
    }
    await neosh.buf.render(buf, ns, 0, -1, drawn);
    // The newest line, not the oldest: a transcript you have just opened should be showing the end
    // of the conversation, which is the part that is still happening.
    await neosh.win.scrollTo(win, Math.max(0, all.length - 24));
  };

  const sub = neosh.swarm.onStream(async (e) => {
    if (e.node !== node || e.session !== session) return;
    const ev = e.event;
    switch (ev.event) {
      case "history":
        body = renderMessages(ev.messages);
        streaming = false;
        break;
      case "turn_started":
        streaming = false;
        break;
      case "token": {
        if (!streaming) {
          body.push("");
          streaming = true;
        }
        // Appended to the last row, and split on newlines, because tokens are provider-sized rather
        // than line-sized and a naive push makes one row per chunk.
        const last = body.pop() ?? "";
        const merged = (last + ev.text).split("\n");
        body.push(...merged);
        break;
      }
      case "thinking":
        break;
      case "activity":
        break;
      case "turn_ended":
        streaming = false;
        body.push("");
        break;
      case "blocked":
        body.push(`  · waiting for somebody at ${name}: ${ev.prompt}`);
        break;
    }
    await redraw();
  });

  const close = async () => {
    if (openView?.session !== session) return;
    openView = null;
    sub.dispose();
    await neosh.swarm.unsubscribe(node, session).catch(() => {});
    await neosh.focus.pop().catch(() => {});
    await neosh.win.close(win).catch(() => {});
  };
  openView = { close, node, session };
  subscriptions.push({ dispose: () => void close() });

  await installViewKeys(neosh, subscriptions, () => openView);
  try {
    await neosh.swarm.subscribe(node, session);
  } catch (e) {
    neosh.notify(String(e), "warn");
    await close();
  }
}

/**
 * The keys inside a remote view.
 *
 * Registered once and bound at buffer-kind scope, so `^Z` lists them and `init.ts` can move them —
 * the same deal every other panel key gets. Registered lazily rather than at activation because
 * most workspaces never open one.
 */
let viewKeysInstalled = false;
async function installViewKeys(
  neosh: Neosh,
  subscriptions: PluginContext["subscriptions"],
  current: () => { node: string; session: string; close: () => Promise<void> } | null,
): Promise<void> {
  if (viewKeysInstalled) return;
  viewKeysInstalled = true;
  const scope = { kind: "buf_kind", name: VIEW_KIND } as const;

  const bind = async (name: string, key: string, desc: string, fn: () => Promise<void>) => {
    subscriptions.push(await neosh.cmd.register(name, fn, { desc }));
    await neosh.keymap.set("chat", key, name, { scope, desc });
  };

  await bind("swarm.view.close", "<Esc>", "Stop watching", async () => {
    await current()?.close();
  });
  await neosh.keymap.set("chat", "q", "swarm.view.close", { scope });

  await bind("swarm.view.steer", "i", "Say something to this agent", async () => {
    const view = current();
    if (!view) return;
    const text = await prompt(neosh, "Say something", { width: 72 });
    if (text === null || text.trim() === "") return;
    try {
      await neosh.swarm.command(view.node, view.session, { command: "send", text });
    } catch (e) {
      // The owner refused — read-only, or the conversation went away. Its words, not ours.
      neosh.notify(String(e), "warn");
    }
  });

  await bind("swarm.view.interrupt", "<C-c>", "Ask this turn to stop", async () => {
    const view = current();
    if (!view) return;
    try {
      await neosh.swarm.command(view.node, view.session, { command: "interrupt" });
      neosh.notify("asked it to stop");
    } catch (e) {
      neosh.notify(String(e), "warn");
    }
  });
}

/** Messages as rows. Deliberately plain: this is a window onto somewhere else, not a transcript. */
function renderMessages(messages: Message[]): string[] {
  const out: string[] = [];
  for (const m of messages) {
    for (const block of m.content) {
      if (block.type === "text") {
        if (m.role === "user") {
          out.push(`  › ${block.text.split("\n")[0] ?? ""}`);
          for (const rest of block.text.split("\n").slice(1)) out.push(`    ${rest}`);
        } else {
          for (const line of block.text.split("\n")) out.push(`  ${line}`);
        }
      } else if (block.type === "tool_use") {
        out.push(`  · ${block.name}`);
      }
    }
    out.push("");
  }
  return out;
}

/**
 * Add a machine by dialling it.
 *
 * The address is all you type. Everything else — its name, its fingerprint, whether it is even a
 * neosh — is read off the machine itself, because a public key typed from memory is a public key
 * typed wrong, and because the point of showing a fingerprint is that it came from the far end
 * rather than from the same person who typed the address.
 */
async function addComputer(neosh: Neosh): Promise<void> {
  const addr = await prompt(neosh, "Add a computer — hostname:7717", { width: 60 });
  if (addr === null || addr.trim() === "") return;
  // A port is easy to forget and there is only one sensible default.
  const target = addr.includes(":") ? addr.trim() : `${addr.trim()}:7717`;

  let found;
  // A dial across a network somebody just typed the address of is the one thing here that can take
  // long enough to look broken — a wrong hostname sits on a TCP timeout — so it turns while it
  // waits. Progress is keyed and replaced in place, which is what makes this one line rather than
  // a column of `asking…`.
  const turning = onTick(() => {
    neosh.progress("swarm.probe", `${spinnerFrame()} asking ${target}…`);
  });
  try {
    // A state that stops being true the moment the far end answers or does not.
    neosh.progress("swarm.probe", `${spinnerFrame()} asking ${target}…`);
    found = await neosh.swarm.probe(target);
  } catch (e) {
    // `String(e)` here would read `NeoshError: not found: …: swarm i/o: Connection refused (os
    // error 61)`, which names three layers of plumbing before it gets to the part a person can act
    // on. What went wrong is that nothing answered, and what to check is spelling and whether the
    // other neosh is running.
    neosh.notify(
      `nothing answered at ${target} — is neosh running there, with `
        + "`listen` set in its `[swarm]`?",
      "warn",
    );
    neosh.log.warn(`swarm probe of ${target} failed: ${String(e)}`);
    return;
  } finally {
    turning.dispose();
    neosh.done("swarm.probe");
  }

  const ok = await confirm(neosh, `Add ${found.name}?`, {
    yes: "Add",
    no: "Cancel",
    detail: [
      `${target}  ·  ${found.os}  ·  neosh ${found.version}`,
      fingerprint(found.id),
      "Check that fingerprint matches what that computer shows under `This computer`.",
    ],
  });
  if (!ok) return;

  await neosh.swarm.pair(found.id, { name: found.name, addr: target });
  // Both halves have to happen, and only one of them is ours. Saying so now saves the ten minutes
  // otherwise spent wondering why the machine is listed and permanently unreachable.
  neosh.notify(`${found.name} added — now allow this computer over there, with ^J`);
  // And back to the panel, which is where that second half becomes visible: the new row sits on
  // `allow this computer on <name>` until somebody does it and then turns over to its
  // conversations, live, without a key being pressed here. Dropping the user back at the composer
  // instead is what made pairing feel like it had silently failed — the one thing worth watching
  // was on a panel they had just been taken out of.
  await showNodes(neosh);
}

/**
 * A node id, in groups, for reading aloud.
 *
 * Sixty-four hex characters is not something a person can compare. Four groups of four from the
 * front is — and it is the *front* rather than a hash of the whole because that is what both
 * machines display, so the two are comparable by eye.
 */
function fingerprint(id: string): string {
  const head = id.slice(0, 16);
  return (head.match(/.{1,4}/g) ?? [head]).join(" ");
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
 * A project is a directory you have worked in — added by hand, or arrived at because a conversation
 * was started there — and it stays one until you remove it. Every known directory is seeded, empty
 * or not, which is the whole of the fix for a project vanishing when you cleared out its
 * conversations: the list is written down (see [`VAR_KNOWN`]) rather than inferred from what
 * happens to be in it, and `X` on the heading is the only thing that shortens it.
 */
function group(
  sessions: SessionInfo[],
  arrangement: Arrangement,
  keys: Map<string, string>,
): Project[][] {
  const by = new Map<string, SessionInfo[]>();
  for (const cwd of arrangement.all()) by.set(cwd, []);
  for (const s of sessions) {
    const existing = by.get(s.cwd);
    if (existing) existing.push(s);
    else by.set(s.cwd, [s]);
  }

  // Which checkout each directory is a linked worktree of, learned from its conversations — and
  // from what was written down the last time one of them said so, because a project stays on the
  // list after its conversations have gone and an emptied worktree is still a worktree.
  const rootOf = new Map<string, string>();
  for (const s of sessions) {
    if (s.repo_root && s.repo_root !== s.cwd && !rootOf.has(s.cwd)) {
      rootOf.set(s.cwd, s.repo_root);
    }
  }
  for (const cwd of by.keys()) {
    const remembered = arrangement.root(cwd);
    if (remembered && !rootOf.has(cwd)) rootOf.set(cwd, remembered);
  }

  // `session.list()` is most-recently-used first, so index in it is recency. Anything you have
  // never dragged sorts by that, after everything you have.
  const recency = new Map<string, number>();
  sessions.forEach((s, i) => {
    if (!recency.has(s.cwd)) recency.set(s.cwd, i);
  });

  const make = (cwd: string, list: SessionInfo[]): Project => ({
    cwd,
    // What the host decided to call it. For a worktree that is the repository and the branch,
    // rather than whatever the directory happens to be named — a row reading `wt-fe3c0d93`
    // tells you nothing about which checkout it is, and reads as somebody else's directory having
    // wandered into your workspace. With nothing in it there is nobody to ask, so the last answer
    // is kept: emptying a project must not also rename it.
    name: list[0]?.project || arrangement.name(cwd) || basename(cwd),
    favorite: arrangement.isFavorite(cwd),
    sessions: list,
    key: keys.get(cwd) ?? `dir:${basename(cwd)}`,
    worktrees: [],
  });

  const tops = new Map<string, Project>();
  const linked: Project[] = [];
  for (const [cwd, list] of by) {
    const root = rootOf.get(cwd);
    if (!root) {
      tops.set(cwd, make(cwd, list));
      continue;
    }
    // Nested under the repository, the repository's name is said by the row above — what is left
    // to say is which tree this one is. The branch, like the host's own label; the directory only
    // on a detached head, where it is all there is.
    linked.push({ ...make(cwd, list), name: list[0]?.branch || basename(cwd) });
  }
  for (const child of linked) {
    const root = rootOf.get(child.cwd)!;
    // A repository whose every conversation is in a worktree still has a row of its own — the
    // worktrees have to hang from something, and the checkout is a real place `↵` can start a
    // conversation in.
    const parent = tops.get(root) ?? make(root, []);
    tops.set(root, parent);
    parent.worktrees.push(child);
  }

  // A project's recency is its newest conversation *anywhere in it* — a repository whose only
  // activity is in a worktree should float with that work, not sink because its own checkout is
  // quiet.
  const newest = (p: Project): number =>
    Math.min(
      recency.get(p.cwd) ?? Number.MAX_SAFE_INTEGER,
      ...p.worktrees.map((t) => recency.get(t.cwd) ?? Number.MAX_SAFE_INTEGER),
    );

  const sort = (a: Project, b: Project) => {
    const ra = arrangement.rank(a.cwd);
    const rb = arrangement.rank(b.cwd);
    if (ra !== rb) return ra - rb;
    const fa = newest(a);
    const fb = newest(b);
    return fa !== fb ? fa - fb : a.name.localeCompare(b.name);
  };

  const projects = [...tops.values()];
  for (const p of projects) p.worktrees.sort(sort);
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
  /** Which conversations are waiting on an answer from you. See [`VAR_ASKING`]. */
  asking: Set<string>;
  /** Marks other plugins put on our rows, by [`targetKey`]. See [`POINT_DECORATION`]. */
  decorations: Map<string, Decoration>;
  /** A count typed but not yet spent, drawn at the foot the way Vim draws it. */
  count: string;
  /** Conversations on other computers, grouped by the project key they share with ours. */
  remote: Map<string, SwarmAgent[]>;
  /** Which other machines have each project, by key. */
  hosts: Map<string, string[]>;
}

/**
 * The waiting set, out of whatever was in the var.
 *
 * Checked rather than trusted, like every contribution: this is JSON written by a plugin this one
 * has never heard of, and a panel that throws on it is a panel a third party can take off the screen
 * by writing a string where a list goes.
 */
function asked(value: unknown): Set<string> {
  if (!Array.isArray(value)) return new Set();
  return new Set(value.filter((v): v is string => typeof v === "string"));
}

async function collect(
  neosh: Neosh,
  arrangement: Arrangement,
  opts: DrawOptions,
): Promise<{ rows: ListRow<Target>[]; running: boolean; pinned: number }> {
  const rows: ListRow<Target>[] = [];

  // Failures absorbed: a panel that renders an exception instead of your conversations is worse
  // than one showing less.
  const all = await neosh.session
    .list({ includeArchived: true })
    .catch(() => [] as SessionInfo[]);
  const sessions = all.filter((s) => !s.archived);
  const running = sessions.some((s) => s.active_turn);
  const now = Date.now();
  // One list, favourites first. A separate `FAVORITES` section splits a short list in half and
  // makes you check two places for the same kind of thing; the star says which is which without
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
  // Marks other plugins put on our rows. Read every frame for the reason sections are: a
  // decorator re-contributes in place when its data changes, and the read is one call.
  const decorations = mergeDecorations(
    await neosh.ext.list<DecorationItem>(POINT_DECORATION).catch(() => []),
  );
  opts = { ...opts, remote, hosts, decorations };
  const projects = group(sessions, arrangement, keys).flat();

  // A directory that turned up in the conversation list and we had not seen before. Noted rather
  // than fetched inline: the draw runs on a tick and must not wait on a round trip per project.
  void arrangement.note(all.map((s) => s.cwd));
  // And what the host calls it, and which checkout a tree hangs off, so the row still says both
  // once the last conversation in it has gone.
  arrangement.remember([
    ...projects.filter((p) => p.sessions.length > 0),
    ...projects.flatMap((p) =>
      p.worktrees.filter((t) => t.sessions.length > 0).map((t) => ({ ...t, root: p.cwd }))
    ),
  ]);

  // Rows other plugins own. Read every frame rather than cached, because a contribution is replaced
  // in place when its author's data changes and re-reading is one call.
  const sections = await neosh.ext.list<SectionItem>(POINT_SECTION).catch(() => []);
  const order = placeSections(SLOTS, sections);
  const section = (c: Contribution & { item: SectionItem }) =>
    rows.push(...contributedRows<Target>(c, {
      width: opts.width,
      custom: (command, args, id) => ({ kind: "custom", command, args, section: id }),
    }));

  // The blocks this panel draws, each the same shape: so that a section can sit before or after
  // any of them by name, and the foot is whatever lands after the last one.
  const blocks: Record<Slot, () => void> = {
    projects: () => {
      // How to make the column wider, on the heading of the column it makes wider — the idiom the
      // plan block already uses for `^L`, and it costs no row of a panel whose rows are its whole
      // job. It has to be *somewhere*: a project name clipped to `neosh-websit…` is a row asking
      // to be widened, and `>` was bound, documented and invisible — advertised on contributed
      // rows only, which is the one kind of row most people never stand on. Only while the panel
      // has the keyboard, because that is when the key does anything.
      rows.push(...heading("PROJECTS", opts.width, opts.focused ? "<> width" : undefined));
      for (const p of projects) {
        rows.push(projectRow(p, arrangement, opts, now));
        if (arrangement.isFolded(p.cwd)) continue;
        for (const s of p.sessions) rows.push(sessionRow(s, now, opts));
        // Its worktrees, inside it. Each is a project row one level down — its own fold, its own
        // rank, `n` makes another conversation in it — because a worktree of a repository is not a
        // neighbour of the repository, and the column should say so.
        for (const t of p.worktrees) {
          rows.push(worktreeRow(t, arrangement, opts, now));
          if (arrangement.isFolded(t.cwd)) continue;
          for (const s of t.sessions) rows.push(sessionRow(s, now, opts, 1));
        }
        // The same project, being worked on elsewhere. Under the same heading rather than in a
        // section of their own: they are not a different kind of thing, they are the same work on
        // a different computer, and a separate `REMOTE` block would make you check two places for
        // one project.
        for (const r of remote.get(p.key) ?? []) rows.push(remoteRow(r, opts, now));
        if (
          p.sessions.length === 0 && p.worktrees.length === 0 &&
          (remote.get(p.key) ?? []).length === 0
        ) {
          rows.push({ text: "     nothing here yet", hl: "Sidebar.Dim", inert: true });
        }
      }

      // Projects that exist only on other machines. Without these, a repository you have not
      // cloned here is invisible — and "which computers is this on" cannot answer "not this one".
      for (const [key, list] of remote) {
        if (projects.some((p) => p.key === key)) continue;
        const first = list[0];
        if (!first) continue;
        rows.push({
          text: ` ${opts.ascii ? "~" : "▹"} ${clip(first.agent.project_name, opts.width - 10)}`,
          hl: "Sidebar.Remote",
          right: { text: `${clip((hosts.get(key) ?? []).join(" "), 12)} `, hl: "Sidebar.Remote" },
          inert: true,
        });
        for (const r of list) rows.push(remoteRow(r, opts, now));
      }
    },
    add: () => {
      rows.push(blank());
      rows.push({
        text: " + Add project",
        hl: "Accent",
        value: { kind: "add" },
      });
    },
    // No `archived` block any more: what you have put away is the archive plugin's panel, and its
    // row in this column arrives as a `sidebar.section` contribution like any third party's.
  };

  // Everything after the last of our own blocks is the panel's foot: rows somebody contributed
  // below the list, and the key strip. Counted so the list can hold them against the bottom edge —
  // a plan gauge that sits under the last project is in a different place every time a project is
  // added or folded, which is the one thing a status strip must not be.
  let body = 0;
  for (const entry of order) {
    if (typeof entry === "string") {
      blocks[entry]();
      body = rows.length;
    } else {
      section(entry);
    }
  }

  if (opts.hints) rows.push(...hints(opts));

  return { rows, running, pinned: rows.length - body };
}

/** A section heading, with a rule under it — what turns a column of text into sections. */
function heading(text: string, width: number, hint?: string): ListRow<Target>[] {
  return [
    {
      text: ` ${text}`,
      hl: "Sidebar.Heading",
      // Dim, and on the heading rather than beside the title: it is an answer to "and then what",
      // which is a question you ask after reading the section, not while finding it.
      right: hint ? { text: `${hint} `, hl: "Sidebar.Dim" } : undefined,
      inert: true,
    },
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
  // "Inside it" includes its worktrees: a repository whose only running turn is in a scratch tree
  // is still a repository where something is happening, and the folded count has to count what
  // folding hid.
  const within = [...p.sessions, ...p.worktrees.flatMap((t) => t.sessions)];
  const busy = within.find((s) => s.active_turn);
  const here = p.sessions.some((s) => s.is_active);
  // Something in here has stopped and is waiting on an answer. It is still a turn in flight, so
  // `busy` finds it too — this is what decides which of the two the row says. Over `within` for the
  // reason the count is: a question asked in a scratch tree of this repository is a question in
  // this repository, and folding is what hid the row that would otherwise say so.
  const waiting = within.some((s) => opts.asking.has(s.id));

  // The count is the useful thing when a project is folded, and the elapsed time is the useful
  // thing when something inside it is working. Never both — there is one column.
  const right = busy
    ? { text: `${turnFor(busy, now)} `, hl: waiting ? "Status.Pending" : "Status.Working" }
    : within.length > 0
      ? { text: `${within.length} `, hl: "Sidebar.Dim" }
      : { text: "" };

  // The star sits directly after the name, and costs nothing on a project that has not got one.
  // It used to be a fixed column *before* the fold arrow, which meant every project name in the
  // narrowest panel in the workspace paid two columns, permanently, for a mark on three or four
  // rows — and the right-hand edge is not the answer either: that column already belongs to the
  // count and the elapsed time, which is a number you read against the rows above and below it.
  // Attached to the name it is a property of the thing it is beside, which is what it is. It keeps
  // its own highlight because the world has already decided what colour a favourite is — a grey
  // star is a star you have to decode.
  //
  // A star rather than a heart, and the reason is font fallback: `♥` is U+2665, which Unicode
  // classifies as an emoji even though its default presentation is text. A terminal that has a
  // colour-emoji font installed routes it there, and what comes back is somebody else's artwork
  // at somebody else's weight — commonly an outline, which is what a filled glyph is not. Every
  // heart codepoint has that problem. `★` is U+2605, `Emoji=No`, so no terminal has any reason to
  // leave the font it is drawing the rest of the row in.
  const star = p.favorite ? (opts.ascii ? " *" : " ★") : "";

  // Which other computers have this project. The whole reason a project key is a normalised git
  // remote rather than a path: on two machines the path is different and this is the same.
  const elsewhere = opts.hosts.get(p.key) ?? [];

  // What is finished and unseen inside a project you have folded shut. Only when folded, because
  // folding is the thing that hid it: with the project open the conversation says so on its own
  // row, and saying it twice on two adjacent lines is how a panel teaches you to stop reading it.
  //
  // It sits after the name rather than in the right-hand column: that column is already spoken for
  // by the elapsed time of whatever is running, and a project can perfectly well have one turn
  // still going and another that finished an hour ago.
  // A question in there outranks it, and for the same reason it does on the conversation's own row:
  // one of them is news that will keep, and the other is a turn that has stopped until you answer.
  // Only one mark, because there is one column and two would be a puzzle rather than a summary.
  // A third thing the fold can be hiding, and it sits between the other two: a question stops the
  // workspace until you answer, a killed turn lost work, and an unread answer is waiting patiently.
  const cut = folded && !waiting
    ? within.filter((s) => !s.active_turn && s.interrupted).length
    : 0;
  const unseen = folded && !waiting && cut === 0
    ? within.filter((s) => !s.active_turn && s.unread).length
    : 0;
  // Fourth and last rung: a folded project hiding a conversation that is still running something.
  // Below all three, because a question stops the workspace, a killed turn lost work, an unread
  // answer is waiting — and this is none of those. Counted over the whole group, conversations
  // included, because folded is exactly the state where the rows that know are the ones you
  // cannot see.
  const busyBg = folded && !waiting && cut === 0 && unseen === 0
    ? within.filter((s) => !s.active_turn && (s.background?.length ?? 0) > 0).length
    : 0;
  // Whichever rung is being reported, drawn the same way and told apart by the glyph and the
  // colour. One column, one mark — two would be a puzzle rather than a summary.
  const [count, dot, dotHl] = cut > 0
    ? [cut, opts.ascii ? "x" : "✗", "Diagnostic.Error"]
    : unseen > 0
    ? [unseen, opts.ascii ? "!" : "●", "Status.Unread"]
    // Hollow where the other two are solid: the difference between something here for you and
    // something here still happening.
    : [busyBg, opts.ascii ? "o" : "○", "Status.Monitoring"];
  const mark = folded && waiting
    ? " ?"
    : count === 0
    ? ""
    : count === 1
    ? ` ${dot}`
    : ` ${dot}${count}`;
  const markHl = folded && waiting ? "Status.Pending" : dotHl;

  // The star's two columns come off the name that is about to carry it, so a favourite and the
  // project under it still end in the same place — clipping the name is what a panel this narrow
  // does, and letting the mark run two columns past everything else is not.
  const target: Target = { kind: "project", cwd: p.cwd };
  // Everything the name and the badge have to share. The badge gives up its least important parts
  // before a single character comes off the name — a project you cannot read the name of is not a
  // row you can use, and how far behind the remote it is keeps until you widen the panel with `>`.
  const room = opts.width - 8 - byteLength(mark) - byteLength(star) - (elsewhere.length ? 8 : 0);
  const decoration = fitBadge(
    opts.decorations.get(targetKey(target) ?? ""),
    room,
    Array.from(p.name).length,
  );
  const name = clip(p.name, Math.max(6, room - badgeColumns(decoration)));
  const spans: Array<{ from: number; to: number; hl: string }> = [];
  if (star !== "") {
    const from = byteLength(` ${arrow} ${name}`);
    spans.push({ from, to: from + byteLength(star), hl: "Sidebar.Favorite" });
  }
  if (mark !== "") {
    const from = byteLength(` ${arrow} ${name}${star}`);
    spans.push({ from, to: from + byteLength(mark), hl: markHl });
  }
  return decorateRow({
    text: ` ${arrow} ${name}${star}${mark}`,
    // A project's name is a directory name, and directory names are long. Clipped it is the same
    // eight characters as the three others you have open beside it, which is the panel failing at
    // the one thing it is for. The star and the unread dot are left off deliberately: they
    // survived the clip and are already on the row.
    full: ` ${arrow} ${p.name}`,
    indent: 3,
    // `here` is this panel's opinion; everything else is a decorator's to colour.
    hl: here ? "Directory" : decoration?.hl ?? "Sidebar.Dim",
    spans: spans.length > 0 ? spans : undefined,
    right: elsewhere.length > 0
      // The machines take the column the count would have used. A project that is in two places is
      // a more useful thing to know than how many conversations are in it here.
      ? { text: `${clip(elsewhere.join(" "), 14)} `, hl: "Sidebar.Remote" }
      : right,
    value: target,
  }, decoration, Boolean(busy) || elsewhere.length > 0);
}

/**
 * A worktree, one level inside the repository it is a tree of.
 *
 * The same kind of row as a project — same `Target`, so folding, `n`, `f` and `J`/`K` need no
 * second code path — drawn at the indent of the conversations beside it, because that is the
 * claim the nesting makes: this belongs to the row above. No star column; pinning is the
 * repository's, and a second ragged column of stars is what the alignment here pays for.
 */
function worktreeRow(
  p: Project,
  arrangement: Arrangement,
  opts: DrawOptions,
  now: number,
): ListRow<Target> {
  const folded = arrangement.isFolded(p.cwd);
  const arrow = opts.ascii ? (folded ? ">" : "v") : folded ? "▸" : "▾";
  const busy = p.sessions.find((s) => s.active_turn);
  const here = p.sessions.some((s) => s.is_active);
  const right = busy
    ? { text: `${turnFor(busy, now)} `, hl: "Status.Working" }
    : p.sessions.length > 0
      ? { text: `${p.sessions.length} `, hl: "Sidebar.Dim" }
      : { text: "" };
  const cut = folded ? p.sessions.filter((s) => !s.active_turn && s.interrupted).length : 0;
  const unseen = folded && cut === 0
    ? p.sessions.filter((s) => !s.active_turn && s.unread).length
    : 0;
  const mark = cut > 0
    ? cut === 1
      ? opts.ascii ? " x" : " ✗"
      : opts.ascii ? ` x${cut}` : ` ✗${cut}`
    : unseen === 0 ? "" : unseen === 1
      ? opts.ascii ? " !" : " ●"
      : opts.ascii ? ` !${unseen}` : ` ●${unseen}`;
  // The branch glyph, in the branch colour — what says "this row is a checkout" at a glance, so
  // the name can be just the branch. No ASCII stand-in earns its column, so ASCII goes without.
  const glyph = opts.ascii ? "" : "⎇ ";
  // One step in from its repository's arrow, and the step is two columns — the same one a
  // conversation takes from the project it is in. Three, when the star was on the left, made the
  // nesting read as two levels where there is one.
  const pad = "   ";
  const target: Target = { kind: "project", cwd: p.cwd };
  // As on a project row, and two columns tighter: the branch name is what this row is, so the
  // badge is what gives way. See `projectRow`.
  const room = opts.width - 8 - byteLength(glyph) - byteLength(mark);
  const decoration = fitBadge(
    opts.decorations.get(targetKey(target) ?? ""),
    room,
    Array.from(p.name).length,
  );
  const name = clip(p.name, Math.max(6, room - badgeColumns(decoration)));
  const spans: Array<{ from: number; to: number; hl: string }> = [];
  if (glyph !== "") {
    const at = byteLength(`${pad}${arrow} `);
    spans.push({ from: at, to: at + byteLength(glyph), hl: "Git.Branch" });
  }
  if (mark !== "") {
    const at = byteLength(`${pad}${arrow} ${glyph}${name}`);
    spans.push({
      from: at,
      to: at + byteLength(mark),
      hl: cut > 0 ? "Diagnostic.Error" : "Status.Unread",
    });
  }
  return decorateRow({
    text: `${pad}${arrow} ${glyph}${name}${mark}`,
    // A branch name is as long as somebody made it, and this row is two columns narrower than a
    // project's. The unread mark is left off the unfolded form: it survived the clip and is
    // already on the row.
    full: `${pad}${arrow} ${glyph}${p.name}`,
    indent: byteLength(`${pad}${arrow} `),
    hl: here ? "Directory" : decoration?.hl ?? "Sidebar.Dim",
    spans: spans.length > 0 ? spans : undefined,
    right,
    value: target,
  }, decoration, Boolean(busy));
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
    const width = Math.max(8, opts.width - 8 - host.length);
    return {
      text: `   ${glyph} ${clip(r.agent.label, width)}`,
      full: `   ${glyph} ${r.agent.label}`,
      indent: 5,
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

function sessionRow(
  s: SessionInfo,
  now: number,
  opts: DrawOptions,
  depth = 0,
): ListRow<Target> {
  // The agent has stopped and is waiting on you. First, because it is the only state here that a
  // turn being in flight does not already describe: the turn *is* in flight — blocked on the
  // question — so without this the row is a spinner, indistinguishable from one that is thinking,
  // and the way you find it is by opening conversations until one of them asks you something.
  const asking = opts.asking.has(s.id);
  // Working is the only state that moves, and only where the work is. An idle row's status is
  // deliberately static: twenty animating rows carry no more information than one and cost twenty
  // times the attention.
  const working = !asking && Boolean(s.active_turn);
  // Finished while you were somewhere else. The one row in this column that is asking for
  // something, so it is the one row that gets the attention colour — and it does not move, because
  // it is not going to stop being true on its own and a mark that pulses forever is a mark you
  // learn to look past. It goes away by being opened.
  const unread = !working && !asking && !s.interrupted && s.unread;
  // The turn that was running here never ended, because the workspace it was running in stopped:
  // the machine was shut down, the process was killed. It outranks `unread` — a turn that finished
  // while you were away and a turn that was killed are both news, and only one of them lost work —
  // and it does not move, because nothing is happening here: it already happened.
  const interrupted = !working && !asking && s.interrupted;
  // Something the agent started and stopped waiting for — a shell it put in the background, a
  // sub-agent it let go of. Last in the order on purpose: every state above it is a block or news
  // — and a killed turn is both — while this is neither. It is the answer to "it said it was done,
  // is it?", which is only a question once nothing louder is true.
  const running = !working && !asking && !interrupted && !unread &&
    (s.background?.length ?? 0) > 0;
  const glyph = asking
    // A question mark, in both alphabets. Every other glyph here has an ASCII understudy because
    // the Unicode one is prettier; this one is already the character that means what it means, and
    // a shape people have to learn would be worse in either.
    ? "?"
    : working
      ? s.is_active
        ? spinnerFrame()
        : opts.ascii ? "*" : "◍"
      // The same mark a tool call that failed wears, and for the same reason: this did not finish.
      // A conversation is the largest thing in the workspace that can fail to finish.
      : interrupted
        ? opts.ascii ? "x" : "✗"
      : unread
        ? opts.ascii ? "!" : "●"
        : running
          // Hollow where unread is solid: the same size of mark, and the difference between them
          // is the difference between "there is something here for you" and "there is something
          // here still happening".
          ? opts.ascii ? "o" : "○"
          : s.is_active
            ? opts.ascii ? ">" : "▸"
            : " ";
  // The clock, and it keeps running while the question sits there: one asked four minutes ago and
  // one asked while you were reading this row are not the same news. Off the turn rather than off
  // `working`, because a question can be raised by a plugin with no turn behind it at all — and
  // then the honest number is when the conversation last moved, not a turn that never started.
  const right = s.active_turn
    ? turnFor(s, now)
    : s.updated_at > 0 ? ago(now / 1000 - s.updated_at) : "";
  // Two more columns per level: a conversation inside a worktree sits inside the worktree's row
  // the way the worktree sits inside its repository's.
  const pad = "   " + "  ".repeat(depth);
  const target: Target = { kind: "session", id: s.id, cwd: s.cwd };
  // What is left for the title, counted rather than guessed at. The indent, the glyph and the
  // space after it come off the front; the age column and the space keeping it off the panel edge
  // come off the end, plus one column of air so a title cannot run into a timestamp. The old
  // number was a constant two columns larger than the prefix it stood for, which is two characters
  // of every title in the panel spent on nothing.
  const room = Math.max(
    8,
    opts.width - pad.length - 2 - (right === "" ? 0 : right.length + 2) -
      badgeColumns(opts.decorations.get(targetKey(target) ?? "")),
  );
  return decorateRow({
    text: `${pad}${glyph} ${clip(s.label, room)}`,
    // The title is the only thing telling two conversations in one project apart, and a generated
    // title is a sentence rather than a word — so the column cuts it at about the point where it
    // was going to say which of them this is.
    full: `${pad}${glyph} ${s.label}`,
    // The conversation you are in is not a row you are considering, it is where you are — so it
    // says its whole name whether or not the cursor is on it. Everything else in this column is
    // scannable because it is one line each; the current row is the one place that trade is wrong,
    // and there is only ever one of it.
    expand: s.is_active,
    indent: pad.length + 2,
    hl: asking
      // The palette's group for waiting on something outside the program — which here is a person,
      // and the reason it is the one row in this column that moves. `Status.Unread` deliberately
      // does not: that is news, and news does not stop being true if you ignore it. This is a
      // *block*, it ends the moment you answer, and until then nothing in the workspace goes on.
      ? "Status.Pending"
      : working
        ? s.is_active && pulseBright() ? "Status.Working" : "Status.Monitoring"
        // Red, and the palette's red for something that went wrong rather than a status colour of
        // its own: work was lost here. The whole row again, for the reason the unread mark takes
        // the whole row.
        : interrupted
          ? "Diagnostic.Error"
        : unread
          // The whole row, not only the dot: a single coloured character five columns in is findable
          // if you already know it is there, which is the one thing you cannot assume about the
          // conversation you have forgotten you started.
          ? "Status.Unread"
          : running
            // The colour the panel already uses for work happening somewhere you are not, dimmed
            // and — the whole point — still. Motion here would be a promise that you have to do
            // something, and you do not: it finishes whether or not you look.
            ? "Status.Monitoring"
            : s.is_active
              ? "Accent"
              : undefined,
    // A trailing space so the age does not sit flush against the panel's edge rule.
    right: {
      text: right === "" ? "" : `${right} `,
      hl: asking
        ? "Status.Pending"
        : working
          ? "Status.Working"
          : interrupted
            ? "Diagnostic.Error"
            : unread
            ? "Status.Unread"
            : running ? "Status.Monitoring" : "Sidebar.Dim",
    },
    value: target,
  }, opts.decorations.get(targetKey(target) ?? ""), Boolean(s.active_turn));
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
  const lines = strip(
    !opts.focused
      // `^O` is not here and `+ Add project` is a row you can see: a key strip has two lines, and
      // the verb with a row of its own is the one that can afford to give up its place on them.
      ? ["^T projects", "^N new", "^F archive", "^K palette", "^B hide", "^Z keys"]
      : kind === "custom"
        ? ["↵ open it", "esc back", "? keys"]
      : kind === "project"
        // `X` is on the strip because a list you cannot shorten is a list that grows forever, and
        // a project that outlives its conversations — which is the point of it — has to have a way
        // off.
        ? ["↵ fold", "f ★", "JK move", "n new", "y path", "X remove", "? keys"]
        : kind === "session"
          ? ["↵ open", "r rename", "x archive", "X delete", "y path", "? keys"]
          : ["↵ add project", "esc back", "? keys"],
    opts.width,
  );

  // Contributed verbs get their own line rather than being squeezed onto ours, because ours are
  // laid out in columns that a third party's label of unknown length would break — and because a
  // key nobody can see is a key nobody presses, which is the whole argument for this strip.
  const mine = opts.focused ? contributedHint(opts) : "";

  return [
    blank(),
    { text: "─".repeat(Math.max(1, opts.width)), hl: "Separator", inert: true },
    // Half a motion, at the foot, where Vim puts it: without it a count is a keypress with no
    // effect until the next one, which is indistinguishable from a key that does nothing. On the
    // first key line rather than on the rule above it — the rule is a row of full width, and a
    // flush-right virtual text has nowhere to sit on a row that is already full.
    ...lines.map((text, i) => ({
      text: ` ${text}`,
      hl: "Sidebar.Dim",
      right: i === 0 && opts.count !== "" ? { text: `${opts.count} `, hl: "Accent" } : undefined,
      inert: true,
    })),
    ...(mine === "" ? [] : [{ text: ` ${mine}`, hl: "Sidebar.Dim", inert: true }]),
  ];
}

/**
 * A key strip packed into the column it is actually drawn in.
 *
 * These lines used to be written out by hand with their columns lined up by eye, which is a layout
 * that is right at exactly one width: the project row's second line came to thirty-six columns in
 * a panel that is thirty-four wide by default, and what fell off the end was `? keys` — clipped to
 * `? k`. A strip that truncates its own escape hatch is worse than one that never mentioned it.
 *
 * So: verbs in order of how much you need them, packed greedily onto two lines, and whatever does
 * not fit is dropped. The **last** cell is the way to everything dropped — `? keys`, or `^Z keys`
 * when the panel does not have the keyboard — so if anything was dropped it takes the final slot.
 * Greedy rather than even columns because these cells are three columns wide (`f ★`) and eight
 * (`X remove`), and a grid sized for the widest fits four fewer verbs than the panel has room for.
 */
function strip(cells: string[], width: number): string[] {
  const cols = (s: string) => Array.from(s).length;
  const room = Math.max(1, width - 1);
  const last = cells[cells.length - 1] ?? "";
  const lines: string[] = [];
  let placed = 0;
  for (const cell of cells) {
    const open = lines.length === 0 ? "" : lines[lines.length - 1] ?? "";
    if (open !== "" && cols(`${open}  ${cell}`) <= room) {
      lines[lines.length - 1] = `${open}  ${cell}`;
    } else if (lines.length < 2 && cols(cell) <= room) {
      lines.push(cell);
    } else {
      break;
    }
    placed++;
  }
  // Anything left unplaced means the strip is incomplete, so its last line has to end at the way in
  // to the rest. Verbs come off the tail to make room for it rather than it being appended, because
  // appending is exactly what overflowed — this is counting the same columns the loop above did.
  if (placed < cells.length && lines.length > 0) {
    const at = lines.length - 1;
    let tail = lines[at] ?? "";
    while (tail !== "" && cols(`${tail}  ${last}`) > room) {
      const cut = tail.lastIndexOf("  ");
      tail = cut < 0 ? "" : tail.slice(0, cut);
    }
    lines[at] = tail === "" ? last : `${tail}  ${last}`;
  }
  return lines;
}

/** The contributed verbs that apply to the row under the cursor, clipped to the column. */
function contributedHint(opts: DrawOptions): string {
  const applicable = opts.actions.filter((a) => applies(a.on ?? "any", opts.selected));
  if (applicable.length === 0) return "";
  // The verbs about *this row* first, then the ones about any of them.
  //
  // One line, in a column that is 34 wide by default, and a third plugin's verb is what takes it
  // past the edge. Which one gets clipped is therefore a decision this makes rather than one the
  // order plugins happened to load in makes for it — and a key that only applies to the row you are
  // standing on is the one that has to survive: a verb about every row is a verb you will see again
  // the moment you move.
  const ranked = [
    ...applicable.filter((a) => (a.on ?? "any") !== "any"),
    ...applicable.filter((a) => (a.on ?? "any") === "any"),
  ];
  // One line per key, and the line is what that key would actually do here.
  //
  // Several plugins may want one key on their own rows and the binding sends it to whichever of
  // them the cursor is over — so a strip that printed all of them would advertise two meanings for
  // one press and be wrong about at least one. The ranking above is the same one the dispatcher
  // uses, so the survivor is the verb that fires.
  const seen = new Set<string>();
  const cells: string[] = [];
  for (const a of ranked) {
    if (seen.has(a.key)) continue;
    seen.add(a.key);
    cells.push(`${a.key} ${a.label}`);
  }
  return clip(cells.join("   "), Math.max(4, opts.width - 2));
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
