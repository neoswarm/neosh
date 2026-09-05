/**
 * Where this checkout stands — on the row it is about, and nowhere else.
 *
 * This file used to draw a block: a `GIT` heading, the branch, what had drifted, and a verb row,
 * pinned above the projects. It is now the headless half of that — the part that keeps the numbers
 * true — because the visible half was saying a second time what the panel already says once.
 *
 * # Why the block went
 *
 * A project row already carries this. `statParts` puts `✗2 ↓3 ↑1 ~4 ?7` after the name, each fact
 * in its own colour, and `pulls.ts` puts `#86` after that in the colour of what the pull request
 * *is* — `Forge.Merged`, `Forge.Open`, `Forge.Draft`, `Forge.Closed`. Both are keyed by the
 * project's directory, which is a worktree's directory too, so six trees of one checkout each
 * report their own branch rather than sharing their repository's.
 *
 * So the block was a fourth and fifth row about **one** of the rows below it. That is worse than
 * redundant on a column whose whole job is your conversations: it was the *only* thing here that
 * could disagree with itself, because the block read `git status` for the conversation you were in
 * while the rows read one per project, on two clocks, and a fetch that moved one moved the other a
 * beat later. Two answers to one question, on screen at once.
 *
 * It was also wrong about scope in a way no amount of drawing fixes: a contribution is
 * workspace-wide and a conversation is per view, so two panes reading two repositories saw one
 * block, about whichever conversation the workspace happened to call current. The rows have never
 * had that problem — a row is about a directory, and there is one of them per directory.
 *
 * # What could not go with it
 *
 * **The fetch.** `ahead` and `behind` come off `git status --branch`, which compares HEAD with the
 * remote-tracking ref **sitting on this disk** — so `↓0` drawn without ever fetching means "nothing
 * had arrived as of whenever this checkout last spoke to a server", which after an afternoon is a
 * sentence about breakfast. Deleting the block and keeping the badge would have left every `↓3` in
 * the panel a claim nothing could make true. So the timer, the arrive-somewhere fetch and
 * `git.fetch.interval` all stay exactly as they were; they simply have no rows of their own now.
 *
 * **The choice.** A fast-forward asks nothing, and a **diverged** branch is the other thing
 * entirely: the two answers do different things to your own commits, one of them rewrites them, and
 * neither is guessable from a row. That picker lived on the block's verb row, which means deleting
 * the block would have quietly demoted `p` from *name the choice* to *run `git pull` and report
 * whatever it says about it*. It moved onto the key instead — see {@link pullRepository}, which is
 * what `p` on any row and `git.pull` from the palette both call now.
 *
 * **The *as of*.** The freshness column — `now`, `12m`, `⚠ offline` — was the block's answer to the
 * rule that a number about a remote without an as-of is not a number about a remote. What replaces
 * it is narrower and lives where the numbers do: {@link fetchTrouble} records the checkouts whose
 * last fetch failed, and the row badge grows one dim `⚠` for those. `now` and `12m` are not worth a
 * column on a row — a fetch every three minutes is fresh enough that saying so is noise — but
 * "these numbers are as of whenever the network last worked" is exactly the case you cannot afford
 * to draw silently.
 */

import type { Neosh, PluginContext, RepoStatus } from "@neosh/api";
import type { Pulls } from "./pulls.ts";
import { confirm, picker } from "@neosh/api/ui";

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

/**
 * The checkouts whose last fetch did not reach the remote.
 *
 * Module state rather than a field on the service, because the thing that *writes* it is the fetch
 * — which runs for the conversation you are in — and the thing that *reads* it is the row badge,
 * which is built per project by a different file on a different clock. A `Map` keyed by directory is
 * the smallest thing both can hold, and an entry is removed the moment a fetch succeeds, so a
 * network that comes back clears the mark without anybody being told.
 */
const fetchTrouble = new Map<string, string>();

/** Whether this checkout's numbers are as of a fetch that failed. */
export function fetchFailed(cwd: string): boolean {
  return fetchTrouble.has(cwd);
}

// ---------------------------------------------------------------------------
// Bringing a branch up to date
// ---------------------------------------------------------------------------

/**
 * Bring a checkout up to date, by whichever route its state calls for.
 *
 * Fast-forward is the ordinary case and asks nothing: it adds commits and changes no history, and a
 * dialog in front of the one verb this exists for is what teaches people to clear dialogs without
 * reading them. A **diverged** branch is the other thing entirely — the two answers do different
 * things to your own commits, one of them rewrites them, and neither is guessable from a row. So
 * that one asks, and it asks by *naming the choice* rather than by offering yes and no to a
 * question it never stated.
 *
 * Nothing waiting is not a failure either. `p` on a row that is level with its remote means "go and
 * look", because the number it is pointed at is only ever as fresh as the last fetch — so that case
 * fetches rather than reporting `Already up to date.` about a comparison with this disk.
 *
 * It reads its own status rather than being handed one. That is what lets the same function serve
 * `p` on any row, `git.pull` from the palette and a script calling it by name: the alternative is a
 * verb that works on the conversation you are in and silently does the wrong repository from a row.
 */
export async function pullRepository(
  neosh: Neosh,
  cwd?: string,
  opts: { fetched?: boolean } = {},
): Promise<void> {
  const where = cwd ? { cwd } : undefined;
  const status = await neosh.git.status(where).catch(() => null);
  if (!status) {
    neosh.notify("not a git repository", "warn");
    return;
  }
  if (!status.repo.upstream) {
    // A statement rather than a verb: a branch tracking nothing is not behind anything, and there
    // is no route from here to up-to-date that this key could take on its own.
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
    // "Nothing waiting" is a claim about the remote-tracking ref **on this disk**, so cold — from
    // the palette, from a script, from a row nobody has fetched for — it does not mean nothing is
    // waiting. It means nobody has looked.
    //
    // The block never had this problem: it drew from a status it had just fetched, so `behind === 0`
    // there really was "up to date, and the honest verb is go and look". Moved onto a command that
    // anyone can call at any moment, that same branch turned `git.pull` into a fetch — the remote's
    // commits stayed on the remote and the caller was told nothing was there.
    //
    // So look, and then do what was asked. Once: the second pass carries `fetched` and stops rather
    // than fetching again, which is what keeps a repository that is genuinely level from bouncing
    // between the two halves of this branch.
    if (opts.fetched) {
      neosh.notify("already up to date");
      return;
    }
    const fresh = await fetchRepository(neosh, cwd, { loud: true });
    // Could not reach the remote. `fetchRepository` has already said so and filed it, and guessing
    // at a pull on top of that would only produce git's version of the same complaint.
    if (!fresh) return;
    return await pullRepository(neosh, cwd, { fetched: true });
  }

  // A rebase replays commits over a tree that is being edited underneath it. git refuses on a dirty
  // tree anyway, and saying so before spending the round trip is a better answer than git's, which
  // arrives after the fetch and names a file rather than the decision.
  if (rebase && status.changes.some((c) => c.staged || c.unstaged)) {
    if (
      !(await confirm(neosh, "This working tree has uncommitted changes. Try to rebase anyway?"))
    ) return;
  }

  neosh.progress("git.pull", rebase ? "rebasing…" : "pulling…");
  try {
    const summary = await neosh.git.pull({ ...(where ?? {}), rebase });
    // A pull *is* a fetch, so it settles the trouble mark too: whatever these numbers are, they
    // came off the remote a moment ago.
    if (cwd) fetchTrouble.delete(cwd);
    neosh.notify(short(summary));
  } catch (e) {
    // git's own message names it — diverged, no remote, an unresolved rebase — and inventing a
    // friendlier one here would mean guessing which of those it was. Loud: it is the answer to a
    // key somebody just pressed.
    neosh.notify(String(e), "error");
  } finally {
    neosh.done("git.pull");
  }
}

/**
 * Ask a remote what is new.
 *
 * Gated as a write for the *network* rather than for the working tree: nothing in the tree moves,
 * and it contacts a server and writes refs. Safe on a timer only because the host closes stdin and
 * unsets every askpass, so no credentials is a failure in a moment rather than a hang nobody can
 * see — and a failure is **filed**, not announced, because a workspace fetching every three minutes
 * on a train would otherwise be a toast every three minutes about a network you know is not there.
 */
export async function fetchRepository(
  neosh: Neosh,
  cwd?: string,
  opts: { loud?: boolean } = {},
): Promise<RepoStatus | null> {
  const where = cwd ? { cwd } : undefined;
  try {
    const fresh = await neosh.git.fetch(where);
    if (cwd) fetchTrouble.delete(cwd);
    return fresh;
  } catch (e) {
    if (cwd) fetchTrouble.set(cwd, short(String(e)));
    if (opts.loud) neosh.notify(String(e), "error");
    return null;
  }
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

interface State {
  /** The checkout being reported on, so a stale answer arriving after a switch is discarded. */
  cwd: string | null;
  status: RepoStatus | null;
  /**
   * When the remote was last *asked*, answer or not.
   *
   * Counted from the ask rather than the answer, so a machine with no network settles into one
   * attempt per interval instead of one per tick for ever — "never succeeded" and "due" were
   * otherwise the same question.
   */
  askedAt: number | null;
  /** A fetch is out, so a second one must not start behind it. */
  busy: boolean;
}

export interface GitStatus {
  /** Ask git where we stand. Call after anything that moves HEAD. */
  refresh(): Promise<void>;
  /** Contact the remote. `loud` reports failure rather than filing it. */
  fetch(opts?: { loud?: boolean }): Promise<void>;
}

export interface GitStatusHooks {
  /**
   * The working tree, every time it is read — including `null` for "not in a repository".
   *
   * This exists so there is **one** `git status` in the plugin rather than one per thing that draws
   * a branch name. It is a subprocess that stats the whole tree, it has to be re-run every few
   * seconds because an agent writes files, and the footer segment wants exactly the same answer at
   * exactly the same moments.
   */
  onStatus: (status: RepoStatus | null) => void;
  /**
   * Where this checkout stands has changed, for anything reading it through a *different* call.
   *
   * The project rows carry a badge built from a `git.status` per project, which nothing here
   * refreshes — so after a fetch found three commits, the row for that very directory would go on
   * saying nothing until its own clock came round.
   */
  onMoved: () => void;
}

/**
 * Keep the current checkout's numbers true, and never draw anything.
 *
 * What it owns is two clocks and the option that governs the slow one. Everything visible about
 * git in the sidebar is a decoration on a row somebody else draws, which is the whole point of the
 * point: this plugin does not import the panel, and a sidebar that is not ours picks up every one
 * of these marks unchanged.
 */
export async function installGitStatus(
  ctx: PluginContext,
  hooks: GitStatusHooks & {
    /**
     * What the forge knows. Passed in rather than read here because it is a different server on a
     * much slower clock, and one cache for the whole plugin is what keeps a repository with six
     * worktrees to one request rather than seven.
     */
    pulls: Pulls;
  },
): Promise<GitStatus> {
  const { neosh, subscriptions } = ctx;
  await declareOptions(neosh);

  const state: State = { cwd: null, status: null, askedAt: null, busy: false };

  /** Settings, in memory. Read once and kept current from `opt.onChange`. */
  const settings = { interval: 180 };
  const reload = async () => {
    settings.interval = (await neosh.opt.get<number>("git.fetch.interval").catch(() => 180)) ?? 180;
  };
  await reload();

  const refresh = async () => {
    const current = await neosh.session.current().catch(() => null);
    const cwd = current?.cwd ?? null;
    if (cwd !== state.cwd) {
      // A different repository entirely. Everything below is about the last one.
      state.cwd = cwd;
      state.status = null;
      state.askedAt = null;
    }
    state.status = await neosh.git.status().catch(() => null);
    // Everything else that draws a branch name, off the same read. See {@link GitStatusHooks}.
    hooks.onStatus(state.status);
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
    state.busy = true;
    // Stamped before the attempt, not after it. A fetch that takes forty seconds against an
    // unreachable host and then fails would otherwise be immediately due again.
    state.askedAt = Date.now();
    try {
      const fresh = await fetchRepository(neosh, at ?? undefined, opts);
      // The conversation moved while we were out. Its answer is about a checkout nobody is looking
      // at, and applying it would put another repository's numbers under this one's name.
      if (at !== state.cwd || !fresh) return;
      state.status = fresh;
      hooks.onStatus(fresh);
      // And the project rows, which read it through a call of their own.
      hooks.onMoved();
    } finally {
      state.busy = false;
      // A failed fetch is news to the badge too: the mark it grows is the only thing left saying
      // these numbers are as of whenever the network last worked.
      hooks.onMoved();
    }
  };

  // ---- commands -----------------------------------------------------------
  //
  // Every verb below is registered by name, so `^Z` lists them, `^K` runs them, `init.ts` moves
  // them and a script reaches them through `neosh agent run`.
  const cmds: Array<[string, () => Promise<void>, string]> = [
    ["git.fetch", () => fetch({ loud: true }), "Ask the remote what is new"],
    [
      // Copies rather than opens: there is no browser capability, and on the machine a coding agent
      // usually runs on there is no browser either.
      "git.pull.copy",
      async () => {
        const branch = state.status?.repo.branch;
        const pr = state.cwd && branch ? hooks.pulls.forBranch(state.cwd, branch) : null;
        if (!pr?.url) {
          neosh.notify("no pull request for this branch", "warn");
          return;
        }
        await neosh.edit.copy(pr.url);
        neosh.notify(`copied #${pr.number}`);
      },
      "Copy the URL of this branch's pull request",
    ],
    [
      "git.sidebar.toggle",
      async () => {
        const on = (await neosh.opt.get<boolean>("git.sidebar").catch(() => true)) ?? true;
        await neosh.opt.set("git.sidebar", !on);
      },
      "Show or hide git's marks on the sidebar's rows",
    ],
  ];
  for (const [name, fn, desc] of cmds) {
    subscriptions.push(await neosh.cmd.register(name, fn, { desc }));
  }

  // ---- keeping it true ----------------------------------------------------

  subscriptions.push(neosh.opt.onChange((e) => {
    if (e.name === "git.fetch.interval") void reload();
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

  /** Whether the remote is due to be asked again. `interval = 0` is never. */
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
      "Mark the sidebar's project rows with what has drifted from the remote, what is dirty, and " +
      "which pull request the branch is.",
  });
  await neosh.opt.declare({
    name: "git.fetch.interval",
    type: { type: "int", min: 0, max: 86400 },
    default: 180,
    description:
      "Seconds between asking the remote what is new, so that `↓3` means three waiting for you " +
      "rather than three as of whenever this checkout last spoke to a server. 0 never asks on its " +
      "own, and a row whose last fetch failed is marked. Only ever the repository of the " +
      "conversation you are in, and only when its branch tracks one.",
  });
}

/** `1 commit`, `3 commits`. A row that says `1 commits` is a row somebody stopped proofreading. */
function count(n: number, noun: string): string {
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
}

/**
 * One line of whatever git or the host said.
 *
 * Errors arrive as several lines with a prefix on them and summaries arrive as a sentence; a
 * notification is one line either way, and the first line is the one that names the thing.
 */
function short(text: string): string {
  const first = text.split("\n").map((l) => l.trim()).find((l) => l !== "") ?? text;
  return first.replace(/^Error:\s*/i, "");
}
