/**
 * Where this checkout stands, in the column you are already looking at.
 *
 * The sidebar's own description is "where you are, what changed, and what is answering", and until
 * this block existed it answered two of those three about git and the third not at all: a project
 * row carried a strip of counts, and nothing in the workspace said **which branch** you were on
 * unless the footer happened to have room for it, or offered any way to **get the latest changes**
 * without leaving for a shell.
 *
 * # Three facts and one verb
 *
 * The branch, what has drifted, and what to do about it. The verb is the point: one row whose label
 * is whatever is actually worth pressing right now — `pull 3 commits` when you are behind, `rebase
 * or merge` when the branch has gone both ways, and `check for changes` when there is nothing to
 * say. A panel with a fixed `pull` button that is a no-op nine times out of ten teaches people not
 * to press it.
 *
 * # The fetch is the whole reason this is honest
 *
 * `ahead` and `behind` come off `git status --branch`, which compares HEAD with the remote-tracking
 * ref **sitting on this disk**. Drawn without ever fetching, `↓0` means "nothing had arrived as of
 * whenever this checkout last spoke to a remote", which after an afternoon's work is a sentence
 * about breakfast. So this fetches, on a slow timer and when you arrive somewhere, and every number
 * it draws is one it went and asked for. `git.fetch.interval = 0` turns that off for anybody who
 * would rather their workspace never touched the network on its own; the numbers then say when they
 * were last true, which is what the freshness column at the right of the verb row is for.
 *
 * # Motion, and where it stops
 *
 * A fetch is seconds of nothing on screen against somebody else's server — the one case in this
 * whole file where a still panel and a wedged one are indistinguishable, and therefore the one
 * thing here that is allowed to move. It gets both halves of the workspace's vocabulary: a spinner
 * glyph, so the row changes shape at the edge of vision, and `Git.Fetching`, which sweeps a band
 * along the label the frontend's own clock rather than ours.
 *
 * Nothing else moves, and the restraint is the design rather than an omission. `↓3 waiting` is news
 * exactly like an unread conversation is news: it is true until you act, it is not *happening*, and
 * a row that blinks about it costs attention every time it changes without ever having anything new
 * to say. Motion here means "something is going on that you cannot see" and it is worth having only
 * because it is never spent on anything else.
 *
 * # It is about the conversation you are in
 *
 * Which is the same scope, and the same compromise, as the branch segment in the footer: a
 * contribution is workspace-wide data and a conversation is per view, so two panes reading two
 * repositories see one block, about whichever conversation the workspace calls current. Fixing that
 * properly means a contribution point that can be answered per view, which is a change to the panel
 * vocabulary rather than to this file.
 */

import { byteLength } from "@neosh/api";
import type { Disposable, Neosh, PluginContext, PullRequest, RepoStatus } from "@neosh/api";
import { checksWord, type Pulls, stateHl } from "./pulls.ts";
import { confirm, onTick, picker, spinnerFrame } from "@neosh/api/ui";

/** The contribution id, and therefore what `on: "custom:git"` names. */
const SECTION = "git";
const POINT_SECTION = "sidebar.section";
const POINT_ACTION = "sidebar.action";

/**
 * How much of the column this block is allowed to be, and the ladder `<Tab>` steps along.
 *
 * The same three-name vocabulary the plan strip uses, for the same reason it has one: a sidebar's
 * rows are its whole job, and a block that is always four rows tall is four rows of somebody's
 * conversations spent on a fact that changes twice an hour. `one` is the answer to the question
 * people actually have — *am I up to date, and what do I press* — and the rest is a keypress away.
 */
const STYLES = ["one", "full"] as const;
type Style = (typeof STYLES)[number];

export function styleOf(value: unknown): Style {
  return STYLES.includes(value as Style) ? (value as Style) : "one";
}

/** Which glyphs this terminal can be trusted with. */
interface Glyphs {
  ascii: boolean;
}

function marks(g: Glyphs) {
  return {
    pull: g.ascii ? "v" : "↓",
    push: g.ascii ? "^" : "↑",
    diverged: g.ascii ? "><" : "⇄",
    check: g.ascii ? "*" : "↻",
    ok: g.ascii ? "=" : "✓",
    bad: g.ascii ? "!" : "✗",
    upstream: g.ascii ? "->" : "→",
  };
}

// ---------------------------------------------------------------------------
// Where a checkout stands, in five columns or fewer per fact
// ---------------------------------------------------------------------------

/**
 * A repository's state as a run of coloured stats, worst first, nothing said about zero.
 *
 * It replaced a single amber `●3`. A dot with a number on it is one fact — *something* here has
 * been touched — drawn in the one colour the panel uses for "act now", on the rows you look at
 * most; and the thing it did not say is the thing you actually want off a project row, which is
 * whether this checkout has drifted from the remote. Ahead and behind are already in `RepoStatus`,
 * from the branch header `git status --porcelain=v2 --branch` prints, so this costs no extra call.
 *
 * The order is what would stop you: a conflict is a tree you cannot commit from, behind is work
 * you have not got yet, ahead is work nobody else has, and the two dirty counts are yours and
 * undecided. Each fact keeps its own colour — a strip drawn in one colour is a string you read
 * rather than a row you glance at — and every one of them is a glyph and a number, so the whole
 * strip is at most a handful of columns even when all five are true.
 *
 * `↑` and `↓` are the direction the *commits* would travel, which is the way every git prompt in
 * the world draws it: `↓2` is two waiting for you.
 */
export function statParts(status: RepoStatus, ascii: boolean): Array<{ text: string; hl?: string }> {
  const parts: Array<{ text: string; hl?: string }> = [];
  const say = (text: string, hl: string) => {
    // The separator is a part of its own with no highlight: it belongs to neither side, and giving
    // it to the run before it would colour a column that has nothing in it.
    if (parts.length > 0) parts.push({ text: " " });
    parts.push({ text, hl });
  };
  const is = (c: (typeof status.changes)[number], state: string) =>
    c.staged === state || c.unstaged === state;
  const conflicted = status.changes.filter((c) => is(c, "conflicted")).length;
  const untracked = status.changes.filter((c) => !is(c, "conflicted") && is(c, "untracked")).length;
  const dirty = status.changes.length - conflicted - untracked;

  if (conflicted > 0) say(`${ascii ? "!" : "✗"}${conflicted}`, "Git.Conflict");
  if (status.repo.behind > 0) say(`${ascii ? "v" : "↓"}${status.repo.behind}`, "Git.Behind");
  if (status.repo.ahead > 0) say(`${ascii ? "^" : "↑"}${status.repo.ahead}`, "Git.Ahead");
  if (dirty > 0) say(`~${dirty}`, "Git.Modified");
  if (untracked > 0) say(`?${untracked}`, "Git.Untracked");
  return parts;
}

// ---------------------------------------------------------------------------
// What the block is currently about
// ---------------------------------------------------------------------------

/**
 * What is happening to this repository right now, if anything.
 *
 * `null` is the ordinary case and the one with no motion in it. The other two are the only states
 * in this file that animate, and they are both "we are talking to a server and cannot say how long
 * for" — which is the whole licence for motion in this workspace.
 */
type Busy = null | "fetching" | "pulling" | "rebasing";

interface State {
  /** The checkout being reported on, so a stale answer arriving after a switch is discarded. */
  cwd: string | null;
  status: RepoStatus | null;
  /**
   * The pull request for the branch this checkout has out, if the forge has one.
   *
   * Held rather than looked up at draw time because the lookup crosses a module and a cache, and
   * this block redraws ten times a second while a fetch is out. Refreshed with the status.
   */
  pull: PullRequest | null;
  busy: Busy;
  /** When the remote last *answered*, in epoch ms. `null` is "never, this session". */
  checkedAt: number | null;
  /**
   * When the remote was last *asked*, answer or not.
   *
   * Two timestamps because they drive two different things and a failed ask has to move exactly one
   * of them. `checkedAt` is what the freshness column reports, so a fetch that failed must not
   * touch it or the row says `now` over numbers that are an hour old. This one is what decides
   * whether to ask again — and without it a laptop with no network was a fetch attempt every thirty
   * seconds for ever, because "never succeeded" and "due" were the same question.
   */
  askedAt: number | null;
  /**
   * What went wrong last time we asked, in git's own words.
   *
   * Kept rather than announced. A workspace fetching every three minutes on a train would otherwise
   * be a toast every three minutes about a network you already know is not there, and the useful
   * version of that news is one quiet word in a column you are looking at anyway.
   */
  error: string | null;
  /**
   * What git said about the last pull, and until when to keep saying it.
   *
   * A pull that answers `Already up to date.` and one that fast-forwards eleven commits are
   * different answers, and a button that returns to looking exactly as it did before is a button
   * you press twice to find out whether the first press worked.
   */
  said: { text: string; until: number } | null;
}

/** How long a pull's summary stays on the row before the block goes back to its steady state. */
const SAID_MS = 6000;

export interface GitSection {
  /** Ask git where we stand and redraw. Call after anything that moves HEAD. */
  refresh(): Promise<void>;
  /** Contact the remote, then redraw. `loud` reports failure rather than filing it. */
  fetch(opts?: { loud?: boolean }): Promise<void>;
}

export interface GitSectionHooks {
  /**
   * The working tree, every time it is read — including `null` for "not in a repository".
   *
   * This exists so there is **one** `git status` in the plugin rather than one per thing that draws
   * a branch name. It is a subprocess that stats the whole tree, it has to be re-run every few
   * seconds because an agent writes files, and the block and the footer segment want exactly the
   * same answer at exactly the same moments — so the block reads it and everybody else is told.
   */
  onStatus: (status: RepoStatus | null) => void;
  /**
   * Where this checkout stands has changed, for anything reading it through a *different* call.
   *
   * The project rows carry a badge built from a `git.status` per project, which nothing above
   * refreshes — so after a fetch found three commits the block said `↓3` and the row for the very
   * same directory, two lines below it, said nothing. Two answers to one question, on screen at
   * once, is worse than either being late.
   */
  onMoved: () => void;
}

export async function installGitSection(
  ctx: PluginContext,
  hooks: GitSectionHooks & {
    /**
     * What the forge knows, so the block can name the pull request for the branch it is drawing.
     *
     * Passed in rather than read here because it is a different server on a much slower clock, and
     * because one cache for the whole plugin is what keeps a repository with six worktrees to one
     * request rather than seven.
     */
    pulls: Pulls;
  },
): Promise<GitSection> {
  const { neosh, subscriptions } = ctx;
  await declareOptions(neosh);

  const state: State = {
    cwd: null,
    status: null,
    pull: null,
    busy: null,
    checkedAt: null,
    askedAt: null,
    error: null,
    said: null,
  };

  /**
   * Settings, in memory.
   *
   * Read once and kept current from `opt.onChange`. The alternative is five round trips per redraw,
   * and this redraws ten times a second for as long as a fetch is out.
   */
  const settings = {
    on: true,
    style: "one" as Style,
    width: 34,
    ascii: false,
    interval: 180,
  };
  const reload = async () => {
    const [on, style, width, ascii, interval] = await Promise.all([
      neosh.opt.get<boolean>("git.sidebar").catch(() => true),
      neosh.opt.get<string>("git.sidebar.style").catch(() => "one"),
      neosh.opt.get<number>("sidebar.width").catch(() => 34),
      neosh.opt.get<boolean>("ui.ascii_only").catch(() => false),
      neosh.opt.get<number>("git.fetch.interval").catch(() => 180),
    ]);
    settings.on = on ?? true;
    settings.style = styleOf(style);
    settings.width = width ?? 34;
    settings.ascii = ascii ?? false;
    settings.interval = interval ?? 180;
  };
  await reload();

  /** What was last contributed, so an unchanged redraw is not a round trip. */
  let drawn = "";
  const draw = async () => {
    if (!settings.on || !state.status) {
      if (drawn !== "") {
        drawn = "";
        await neosh.ext.remove(POINT_SECTION, SECTION).catch(() => {});
      }
      return;
    }
    const item = section(state, settings, Date.now());
    // Compared as JSON rather than by identity: this runs on a tick while a fetch is out, and
    // re-contributing identical rows makes every sidebar in the workspace rebuild its whole list —
    // cursor, scroll and all — several times a second for a change nobody can see.
    const key = JSON.stringify(item);
    if (key === drawn) return;
    drawn = key;
    await neosh.ext.contribute(POINT_SECTION, SECTION, item, { priority: -5 }).catch(() => {});
  };

  /**
   * The clock, subscribed to only while something is out.
   *
   * A block with no motion in it has no reason to be woken ten times a second, and the honest way
   * to say that is to not be on the clock at all rather than to be on it and return early.
   */
  let ticking: Disposable | null = null;
  const motion = () => {
    const wanted = state.busy !== null;
    if (wanted && !ticking) ticking = onTick(() => void draw());
    else if (!wanted && ticking) {
      ticking.dispose();
      ticking = null;
    }
  };
  subscriptions.push({ dispose: () => ticking?.dispose() });

  const refresh = async () => {
    const current = await neosh.session.current().catch(() => null);
    const cwd = current?.cwd ?? null;
    if (cwd !== state.cwd) {
      // A different repository entirely. Everything below is about the last one — including how
      // long ago it was checked, which drawn against a checkout we have never fetched would be a
      // freshness column reporting somebody else's freshness.
      state.cwd = cwd;
      state.status = null;
      state.checkedAt = null;
      state.askedAt = null;
      state.error = null;
      state.said = null;
      state.pull = null;
    }
    state.status = await neosh.git.status().catch(() => null);
    // Which pull request this branch is, out of the cache the whole plugin shares. A lookup rather
    // than a request: whoever holds that cache decides when to ask the forge again, on a clock far
    // slower than this one.
    const branch = state.status?.repo.branch;
    state.pull = cwd && branch ? hooks.pulls.forBranch(cwd, branch) : null;
    // Everything else that draws a branch name, off the same read. See {@link GitSectionHooks}.
    hooks.onStatus(state.status);
    await draw();
  };

  const fetch = async (opts: { loud?: boolean } = {}) => {
    if (state.busy) return;
    const at = state.cwd;
    const status = state.status;
    // Nothing to compare against. A branch with no upstream is not behind anything, and fetching on
    // a timer to find that out again is a connection to somebody's server for no answer.
    if (!status || !status.repo.upstream) {
      if (opts.loud) neosh.notify("this branch is not tracking a remote branch", "warn");
      return;
    }
    state.busy = "fetching";
    // Stamped before the attempt, not after it. A fetch that takes forty seconds against an
    // unreachable host and then fails would otherwise be immediately due again.
    state.askedAt = Date.now();
    motion();
    await draw();
    try {
      const fresh = await neosh.git.fetch();
      // The conversation moved while we were out. Its answer is about a checkout nobody is looking
      // at, and applying it would put another repository's numbers under this one's branch name.
      if (at !== state.cwd) return;
      state.status = fresh;
      state.checkedAt = Date.now();
      state.error = null;
      // A fetch answers with a whole status, and `behind` in it is the number the footer's badge
      // draws too. Told rather than left to be re-read on the next tick, or the two disagree for
      // five seconds every time.
      hooks.onStatus(fresh);
      // And the project rows, which read it through a call of their own and would otherwise say
      // nothing about a divergence the block two lines above them has just found.
      hooks.onMoved();
    } catch (e) {
      if (at !== state.cwd) return;
      state.error = short(String(e));
      // Loud only when somebody asked. On the timer this is a train going through a tunnel, and the
      // right amount to say about that is the one dim word the row already carries.
      if (opts.loud) neosh.notify(String(e), "error");
    } finally {
      state.busy = null;
      motion();
      await draw();
    }
  };

  /**
   * Bring this branch up to date, by whichever route the state calls for.
   *
   * Fast-forward is the ordinary case and asks nothing: it adds commits and changes no history, and
   * a dialog in front of the one button this block exists for is what teaches people to clear
   * dialogs without reading them. A **diverged** branch is the other thing entirely — the two
   * answers do different things to your own commits, one of them rewrites them, and neither is
   * guessable from the row. So that one asks, and it asks by naming the choice rather than by
   * offering yes and no to a question it never stated.
   */
  const pull = async () => {
    if (state.busy) return;
    const status = state.status;
    if (!status) return;
    if (!status.repo.upstream) {
      neosh.notify("this branch is not tracking a remote branch", "warn");
      return;
    }
    const { ahead, behind } = status.repo;

    let rebase = false;
    if (ahead > 0 && behind > 0) {
      const chosen = await picker(
        neosh,
        [
          {
            label: "Rebase",
            detail: `replay your ${count(ahead, "commit")} on top of the ${
              count(behind, "commit")
            } that arrived`,
            value: "rebase",
          },
          {
            label: "Merge",
            detail: "bring them together in a merge commit, keeping both histories as they are",
            value: "merge",
          },
        ],
        {
          title: `${status.repo.branch ?? "HEAD"} has gone both ways`,
          width: 76,
        },
      );
      if (!chosen) return;
      rebase = chosen === "rebase";
    } else if (behind === 0) {
      // Nothing waiting as of the last fetch — so the useful reading of this key is "go and look",
      // which is what the row said it would do.
      await fetch({ loud: true });
      return;
    }

    // A rebase replays commits over a tree that is being edited underneath it. git refuses on a
    // dirty tree anyway, and saying so before spending the round trip is a better answer than
    // git's, which arrives after the fetch and names a file rather than the decision.
    if (rebase && status.changes.some((c) => c.staged || c.unstaged)) {
      if (
        !(await confirm(
          neosh,
          "This working tree has uncommitted changes. Try to rebase anyway?",
        ))
      ) return;
    }

    const at = state.cwd;
    state.busy = rebase ? "rebasing" : "pulling";
    motion();
    await draw();
    try {
      const summary = await neosh.git.pull({ rebase });
      if (at === state.cwd) {
        state.said = { text: short(summary), until: Date.now() + SAID_MS };
        // A pull *is* a fetch, so it settles both clocks: the numbers under it came off the remote
        // a moment ago, and asking again thirty seconds later would be a round trip for nothing.
        state.checkedAt = Date.now();
        state.askedAt = state.checkedAt;
        state.error = null;
      }
    } catch (e) {
      // git's own message names it — diverged, no remote, an unresolved rebase — and inventing a
      // friendlier one here would mean guessing which of those it was. This one is loud: it is the
      // answer to a key somebody just pressed.
      neosh.notify(String(e), "error");
    } finally {
      state.busy = null;
      motion();
      state.status = await neosh.git.status().catch(() => state.status);
      await draw();
      // The branch may have moved, so everything drawing a branch name has to hear about it.
      hooks.onMoved();
      // The summary is on the row for a few seconds and then is not. Nothing else would take it
      // off, and a block still saying `Fast-forward` an hour later is a block reporting the past
      // as the present.
      neosh.timer.after(SAID_MS + 100, () => {
        if (state.said && state.said.until <= Date.now()) {
          state.said = null;
          void draw();
        }
      });
    }
  };

  // ---- commands -----------------------------------------------------------
  //
  // Every key below points at one of these by name, so `^Z` lists them, `^K` runs them, `init.ts`
  // moves them and a script reaches them through `neosh agent run` — the same deal every other verb
  // in the workspace gets, and the reason none of this needs an escape hatch of its own.
  const cmds: Array<[string, () => Promise<void>, string]> = [
    ["git.fetch", () => fetch({ loud: true }), "Ask the remote what is new"],
    [
      "git.sidebar.action",
      () => pull(),
      "The sidebar's git verb: pull, choose how to reconcile, or go and look",
    ],
    [
      // Copies rather than opens, for the reason on [`pullRow`]: there is no browser capability,
      // and on the machine a coding agent usually runs on there is no browser either.
      "git.pull.copy",
      async () => {
        const url = state.pull?.url;
        if (!url) {
          neosh.notify("no pull request for this branch", "warn");
          return;
        }
        await neosh.edit.copy(url);
        neosh.notify(`copied #${state.pull?.number}`);
      },
      "Copy the URL of this branch's pull request",
    ],
    [
      "git.sidebar.cycle",
      async () => {
        const next = STYLES[(STYLES.indexOf(settings.style) + 1) % STYLES.length] ?? "one";
        await neosh.opt.set("git.sidebar.style", next);
      },
      "How much of the repository the sidebar shows",
    ],
    [
      "git.sidebar.toggle",
      async () => {
        await neosh.opt.set("git.sidebar", !settings.on);
      },
      "Show or hide the repository block in the sidebar",
    ],
  ];
  for (const [name, fn, desc] of cmds) {
    subscriptions.push(await neosh.cmd.register(name, fn, { desc }));
  }

  // `<Tab>` because that is what opens and folds the thing you are standing on everywhere else in
  // neosh, and `custom:git` so it is a verb about *these* rows — the plan strip wants the same key
  // on its own, and both are right. Without the scope one of the two silently lost the key.
  await neosh.ext.contribute(POINT_ACTION, "git.detail", {
    key: "<Tab>",
    label: "detail",
    command: "git.sidebar.cycle",
    on: `custom:${SECTION}`,
  }).catch(() => {});
  subscriptions.push({
    dispose: () => void neosh.ext.remove(POINT_ACTION, "git.detail").catch(() => {}),
  });

  // ---- keeping it true ----------------------------------------------------

  subscriptions.push(neosh.opt.onChange((e) => {
    if (
      e.name === "git.sidebar" || e.name === "git.sidebar.style" ||
      e.name === "sidebar.width" || e.name === "ui.ascii_only" || e.name === "git.fetch.interval"
    ) void reload().then(draw);
  }));
  // Arriving somewhere is the moment its numbers matter and the moment they are most likely stale.
  subscriptions.push(neosh.session.onChange(() =>
    void refresh().then(() => {
      if (due()) void fetch();
    })
  ));
  // Anything the agent did to the working tree.
  subscriptions.push(neosh.agent.onTurnEnd(() => void refresh()));
  // The working tree changes whenever anything writes a file, agent or not, and `git status` is
  // local and cheap. The *fetch* is on its own much slower clock below.
  subscriptions.push(neosh.timer.every(5000, () => void refresh()));

  /**
   * Whether the remote is due to be asked again. `interval = 0` is never.
   *
   * Counted from the last *ask* rather than the last answer, so a machine with no network settles
   * into one attempt per interval instead of one per tick for ever.
   */
  const due = () =>
    settings.interval > 0 &&
    (state.askedAt === null || Date.now() - state.askedAt >= settings.interval * 1000);

  // Checked every half minute rather than scheduled at the interval, so changing the setting takes
  // effect without restarting a timer, and so a machine that was asleep for an hour asks once when
  // it wakes rather than banking up the fetches it missed.
  subscriptions.push(neosh.timer.every(30_000, () => {
    if (due()) void fetch();
  }));

  await refresh();
  // Not at `activate`: the first thing a workspace does on opening is start a conversation and load
  // eleven plugins, and a network round trip in the middle of that is a second of a slower start
  // for a number nobody has looked at yet.
  neosh.event.on("neosh.ready", () => {
    if (due()) void fetch();
  });

  return { refresh, fetch };
}

async function declareOptions(neosh: Neosh) {
  await neosh.opt.declare({
    name: "git.sidebar",
    type: { type: "bool" },
    default: true,
    description:
      "Show the branch, what has drifted from the remote, and what to press about it at the top " +
      "of the sidebar.",
  });
  await neosh.opt.declare({
    name: "git.sidebar.style",
    type: { type: "str" },
    default: "one",
    description:
      "How much of the column the repository block spends. `one` is the branch and one verb; " +
      "`full` adds the upstream it tracks and the working tree. `<Tab>` on a git row steps " +
      "between them.",
  });
  await neosh.opt.declare({
    name: "git.fetch.interval",
    type: { type: "int", min: 0, max: 86400 },
    default: 180,
    description:
      "Seconds between asking the remote what is new, so that `↓3` means three waiting for you " +
      "rather than three as of whenever this checkout last spoke to a server. 0 never asks on its " +
      "own, and the sidebar then says how old its numbers are. Only ever the repository of the " +
      "conversation you are in, and only when its branch tracks one.",
  });
}

// ---------------------------------------------------------------------------
// The rows
// ---------------------------------------------------------------------------

interface StripRow {
  text: string;
  hl?: string;
  spans?: Array<{ from: number; to: number; hl: string }>;
  right?: { text: string; hl?: string };
  command?: string;
  args?: string[];
}

interface Settings {
  style: Style;
  width: number;
  ascii: boolean;
  interval: number;
}

/**
 * What the sidebar is handed.
 *
 * Two rows in `one`: what you are on, and what to do about it. The second is not decoration — it is
 * the only route in this workspace to `git pull` that does not involve leaving for a shell, and its
 * label is rebuilt from the state every draw so that the key is never pointed at something that
 * would do nothing.
 */
function section(state: State, settings: Settings, now: number) {
  const status = state.status;
  if (!status) return null;
  const g = marks({ ascii: settings.ascii });
  const rows: StripRow[] = [];
  // What the sidebar leaves for our text: its own one-column margin at each edge, the margin
  // `sectionRows` adds, and the widest freshness column — which is drawn *over* the row rather than
  // after it, so a row built to the full width does not wrap, it collides.
  const room = Math.max(12, settings.width - 4 - 6);

  // ---- the branch --------------------------------------------------------
  //
  // The one fact this block exists for, and the one nothing else in the workspace reliably says:
  // the footer's branch segment is the first thing dropped when the terminal is narrow.
  const head = status.repo.detached
    ? `detached ${status.repo.head ?? ""}`.trim()
    : (status.repo.branch ?? "no branch");
  const stats = statParts(status, settings.ascii);
  const statText = stats.map((p) => p.text).join("");
  // The name yields to the counts rather than the other way round — the same bargain a decorated
  // row makes, and for the same reason: a branch clipped to `fix/composer-pas…` is still the branch
  // you recognise, and `↓3` with a digit missing is a lie.
  const name = clip(head, Math.max(6, room - cells(statText) - 1));
  const branchText = statText === "" ? name : `${name} ${statText}`;
  let at = byteLength(name) + 1;
  const spans: Array<{ from: number; to: number; hl: string }> = [];
  for (const part of stats) {
    const to = at + byteLength(part.text);
    if (part.hl) spans.push({ from: at, to, hl: part.hl });
    at = to;
  }
  rows.push({
    text: branchText,
    hl: "Git.Branch",
    spans: spans.length > 0 ? spans : undefined,
    // Every row in the block opens the status float, so there is no row here you can land on and
    // press `↵` on to no effect — which is the thing that teaches people a column is not interactive.
    command: "git.status",
  });

  // ---- the pull request --------------------------------------------------
  //
  // In **both** styles when there is one, unlike everything else that `full` adds. It is the one
  // row here that is not derivable from this disk — the branch, the counts and the working tree are
  // all things you could have worked out, and whether anybody has reviewed your work is not — and
  // it costs a row only on the branches that have one, which is never the busy case.
  if (state.pull) rows.push(pullRow(state.pull, room, settings.ascii));

  if (settings.style === "full") {
    rows.push({
      text: `${g.upstream} ${clip(status.repo.upstream ?? "no upstream", room - 2)}`,
      hl: status.repo.upstream ? "Comment" : "Git.Synced",
      command: "git.status",
    });
    rows.push(workingTreeRow(status, room, settings.ascii));
  }

  rows.push(verbRow(state, settings, now, room, g));
  return { title: "GIT", hint: "^G", before: "projects", rows };
}

/**
 * The pull request this branch is, and what its checks say.
 *
 * `#86 open` with the state's colour on the whole row, and the checks on the right — where the
 * freshness column sits on the verb row, because both answer "how much should I trust this". The
 * check word is dropped entirely when there are no checks configured: a repository that runs none
 * has told you nothing, and "checks passing" over it would be a sentence nobody wrote.
 *
 * `↵` copies the URL. Not *opens* it: neosh has no browser capability and inventing one for this
 * would be a new outward-facing verb for a row — and on the machine this most often runs on, a big
 * one over SSH, opening a browser would open it in the wrong place anyway. Copying reaches the
 * laptop you are sitting at, which is the same reason the clipboard is OSC 52.
 */
function pullRow(pr: PullRequest, room: number, ascii: boolean): StripRow {
  const said = checksWord(pr, ascii);
  // The right column is measured out of the room first, as everywhere else in this block: it is
  // drawn *over* the row rather than after it, so a row built to the full width collides with it.
  const right = said ? { text: said, hl: checksHl(pr) } : undefined;
  const label = `#${pr.number} ${pr.state}`;
  return {
    text: clip(label, Math.max(8, room - (said ? cells(said) + 1 : 0))),
    hl: stateHl(pr.state),
    right,
    command: "git.pull.copy",
  };
}

/** The colour of what the checks say. Nothing to say is the row's own colour rather than a third. */
function checksHl(pr: PullRequest): string {
  switch (pr.checks) {
    case "failing":
      return "Forge.ChecksFailed";
    case "pending":
      return "Forge.ChecksRunning";
    default:
      return "Sidebar.Dim";
  }
}

/**
 * The working tree, in words rather than in the counts the branch row already carries.
 *
 * `~4 ?1` is on the row above and is a glance; this is the sentence, and it says the one thing the
 * counts cannot — whether any of it is *staged*, which is the difference between a tree you can
 * commit from and one you have to think about first.
 */
function workingTreeRow(status: RepoStatus, room: number, ascii: boolean): StripRow {
  const staged = status.changes.filter((c) => c.staged).length;
  const total = status.changes.length;
  if (total === 0) {
    return { text: `${ascii ? "=" : "✓"} clean`, hl: "Git.Synced", command: "git.status" };
  }
  const said = staged === 0
    ? `${count(total, "change")}, none staged`
    : staged === total
    ? `${count(total, "change")}, all staged`
    : `${count(total, "change")}, ${staged} staged`;
  return { text: clip(said, room), hl: "Git.Modified", command: "git.status" };
}

/**
 * The one row that does something, labelled with whatever is actually worth doing.
 *
 * A fixed `pull` that is a no-op most of the time is a button people learn to ignore; this one says
 * `pull 3 commits` when there are three, names the choice when the branch has gone both ways, and
 * offers to go and look when there is nothing waiting — which is the honest verb for a number that
 * is only as fresh as the last fetch.
 */
function verbRow(
  state: State,
  settings: Settings,
  now: number,
  room: number,
  g: ReturnType<typeof marks>,
): StripRow {
  const command = "git.sidebar.action";

  // Working. The only rows here that move, and they move because there is a server at the other end
  // of them and no way to say how long it will take.
  if (state.busy) {
    const said = state.busy === "fetching"
      ? "checking the remote…"
      : state.busy === "rebasing"
      ? "rebasing…"
      : "pulling…";
    // Still landable, and still pointed at the same command — which declines while something is
    // out. A row that goes inert under the cursor is a row the cursor is pushed off, so pressing
    // the button would move you somewhere else for as long as the button was working.
    return { text: `${spinnerFrame()} ${clip(said, room - 2)}`, hl: "Git.Fetching", command };
  }

  // What just happened, for a few seconds. A button that goes back to looking exactly as it did is
  // one you press twice to find out whether the first press worked.
  if (state.said && state.said.until > now) {
    return {
      text: `${g.ok} ${clip(state.said.text, room - 2)}`,
      hl: "Status.Done",
      command,
    };
  }

  const status = state.status;
  const { ahead, behind } = status?.repo ?? { ahead: 0, behind: 0 };
  const freshness = { text: freshText(state, settings, now), hl: "Sidebar.Dim" };

  if (!status?.repo.upstream) {
    // Not a failure and not something to fix — plenty of branches track nothing. Said quietly, and
    // pointed at the status rather than at a verb that would only refuse: there is nothing to pull
    // from, and a button whose whole behaviour is an apology is worse than a sentence.
    return { text: `${g.upstream} no upstream`, hl: "Git.Synced", command: "git.status" };
  }
  if (ahead > 0 && behind > 0) {
    return {
      text: `${g.diverged} ${clip(`diverged — ${ahead} up, ${behind} down`, room - 2)}`,
      hl: "Git.Diverged",
      right: freshness,
      command,
    };
  }
  if (behind > 0) {
    return {
      text: `${g.pull} ${clip(`pull ${count(behind, "commit")}`, room - 2)}`,
      hl: "Git.Behind",
      right: freshness,
      command,
    };
  }
  return {
    text: `${g.check} ${clip("check for changes", room - 2)}`,
    hl: "Git.Synced",
    right: freshness,
    command,
  };
}

/**
 * How old the numbers beside it are.
 *
 * The column that makes the block honest rather than merely confident. `↓0` is a claim about a
 * remote, and a claim about a remote has an *as of* — without one, a panel that has not fetched
 * since Tuesday and one that fetched nine seconds ago draw the same row. `offline` is what a failed
 * ask looks like: the same shape as an age, because what it is saying is the same thing — the
 * numbers are older than they look.
 */
function freshText(state: State, settings: Settings, now: number): string {
  if (state.error) return settings.ascii ? "offline" : "⚠ offline";
  if (state.checkedAt === null) return settings.interval > 0 ? "…" : "never";
  const secs = Math.max(0, Math.round((now - state.checkedAt) / 1000));
  if (secs < 45) return "now";
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${mins}m`;
  const hours = Math.round(mins / 60);
  return hours < 48 ? `${hours}h` : `${Math.round(hours / 24)}d`;
}

// ---------------------------------------------------------------------------
// Small things
// ---------------------------------------------------------------------------

/** `1 commit`, `3 commits`. A row that says `1 commits` is a row somebody stopped proofreading. */
function count(n: number, noun: string): string {
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
}

/**
 * Code points, not display width.
 *
 * That question is `neosh-tui`'s and is not answerable here. What this pads and clips are branch
 * names and short English phrases, and the alternative is a `.length` in UTF-16 units that
 * disagrees with the clip applied to the same string.
 */
function cells(s: string): number {
  return [...s].length;
}

function clip(s: string, n: number): string {
  const chars = [...s];
  return chars.length <= Math.max(1, n) ? s : `${chars.slice(0, Math.max(1, n - 1)).join("")}…`;
}

/**
 * One line of whatever git or the host said.
 *
 * Errors arrive as several lines with a prefix on them and summaries arrive as a sentence; a row is
 * one line either way, and the first line is the one that names the thing.
 */
function short(text: string): string {
  const first = text.split("\n").map((l) => l.trim()).find((l) => l !== "") ?? text;
  return first.replace(/^Error:\s*/i, "");
}
