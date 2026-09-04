/**
 * What the forge knows, on the rows you are already looking at.
 *
 * A branch tells you nothing about whether anybody has looked at it. `feature/sidebar-git-controls`
 * with three commits on it reads identically whether it is a pull request that went green an hour
 * ago, one whose checks are failing, one nobody has opened yet, and one that merged last week and
 * whose worktree you have simply not deleted. Those are four different things to do next, and until
 * this the panel drew them as one row.
 *
 * # One call, every row
 *
 * Pull requests come back per **repository**, keyed by head branch, so a repository with six
 * worktrees is one request and six lookups. That is what makes this affordable on a timer at all:
 * per-row it would be six network round trips every couple of minutes, forever, for a panel most
 * people are not looking at.
 *
 * # Only what is news
 *
 * The badge is the number in the state's colour, and a check mark **only when there is something to
 * say** — `✗2` when checks are failing, one animated glyph while they are running, and nothing at
 * all when they passed. That is the transcript's rule about tool calls applied one panel along: a
 * mark that is always there is a mark you stop seeing, and the green tick beside every merged pull
 * request from the last six months is exactly that.
 *
 * The one thing here that moves is checks that are still running, and it moves for the same reason
 * a fetch does — it is work happening on somebody else's machine, and a still row and a wedged one
 * are otherwise identical. A pull request's *state* never moves: `open` is true until somebody
 * reviews it and `merged` is true for ever.
 *
 * # Failing is a state, not an event
 *
 * `gh` missing, signed out, no remote on a forge. Every one of those is permanent until a person
 * does something, and a workspace that says so every two minutes is a workspace people turn off. It
 * is said **once per reason** and then filed; `git.pulls` re-asks and reports it out loud, because
 * that is somebody pressing a key and expecting an answer.
 */

import type { PluginContext, PullRequest } from "@neosh/api";

const POINT_DECORATION = "sidebar.decoration";
/** The badge id, per project. Namespaced so it merges beside the dirty strip rather than replacing it. */
const idFor = (cwd: string) => `pr:${cwd}`;

/** What we know about one repository's pull requests, and when we last knew it. */
interface Known {
  pulls: PullRequest[];
  /** Epoch ms of the last successful answer. */
  at: number;
  /** Why the last ask failed, if it did. Kept so it can be said once rather than every tick. */
  error: string | null;
}

export interface Pulls {
  /**
   * The pull request for `branch`, asked about from any checkout of its repository.
   *
   * **A `cwd` rather than a repository root**, because the caller almost never has one: a
   * conversation stands in a *worktree*, and `git status` there reports that worktree as the root.
   * Keyed on the repository the pull requests actually belong to, that lookup misses every time —
   * silently, on exactly the rows a worktree panel exists to draw. The map from one to the other is
   * already built here, so it is answered here.
   */
  forBranch(cwd: string, branch: string): PullRequest | null;
  /** Ask the forge again for every repository on the panel. `loud` reports failure. */
  refresh(opts?: { loud?: boolean }): Promise<void>;
}

export async function installPulls({ neosh, subscriptions }: PluginContext): Promise<Pulls> {
  await neosh.opt.declare({
    name: "git.pulls",
    type: { type: "bool" },
    default: true,
    description:
      "Show the pull request on a project or worktree row — its number, its state, and its " +
      "checks when they are failing or still running. Asks `gh`, so it needs the GitHub CLI " +
      "signed in; without one the rows simply carry no pull request.",
  });
  await neosh.opt.declare({
    name: "git.pulls.interval",
    type: { type: "int", min: 0, max: 86400 },
    default: 180,
    description:
      "Seconds between asking the forge what has changed. A pull request moves on somebody " +
      "else's schedule, so unlike the working tree there is nothing local to notice it. 0 asks " +
      "only when you press the key.",
  });

  /** By repository root, because that is the unit `gh` answers about. */
  const known = new Map<string, Known>();
  /** Which reasons have already been said, so a permanent state is said once. */
  const complained = new Set<string>();
  /** Repositories with an ask in flight, so a tick cannot stack up requests on a slow network. */
  const asking = new Set<string>();
  /**
   * Which repository each checkout belongs to.
   *
   * The whole reason [`Pulls.forBranch`] takes a `cwd`: a worktree's `git status` calls the worktree
   * the root, and the pull requests are filed under the repository it is a tree of.
   */
  const rootOf = new Map<string, string>();

  const settings = { on: true, interval: 180, ascii: false, nerd: false };
  const reload = async () => {
    const [on, interval, ascii, nerd] = await Promise.all([
      neosh.opt.get<boolean>("git.pulls").catch(() => true),
      neosh.opt.get<number>("git.pulls.interval").catch(() => 180),
      neosh.opt.get<boolean>("ui.ascii_only").catch(() => false),
      neosh.opt.get<boolean>("ui.nerd_font").catch(() => false),
    ]);
    settings.on = on ?? true;
    settings.interval = interval ?? 180;
    settings.ascii = ascii ?? false;
    settings.nerd = nerd ?? false;
  };
  await reload();

  /**
   * Which repositories to ask about.
   *
   * The **repository roots** of the projects on the panel, deduplicated — six worktrees of one
   * checkout are one entry here, which is the whole economy of this file. Read from the sidebar's
   * own list for the same reason the dirty strip is: which places you work in is a fact about the
   * workspace rather than about the panel that wrote it down.
   */
  const repositories = async (): Promise<Map<string, string[]>> => {
    const listed = await neosh.vars
      .get<unknown>({ scope: "global" }, "sidebar.projects")
      .catch(() => null);
    const cwds = Array.isArray(listed)
      ? listed.filter((p): p is string => typeof p === "string")
      : [];
    /** Repository root to the checkouts under it. */
    const byRoot = new Map<string, string[]>();
    for (const cwd of cwds) {
      const trees = await neosh.git.worktrees({ cwd }).catch(() => []);
      const root = trees.find((t) => t.is_main)?.path ?? trees[0]?.path;
      if (!root) continue;
      rootOf.set(cwd, root);
      const under = byRoot.get(root);
      if (under) under.push(cwd);
      else byRoot.set(root, [cwd]);
    }
    return byRoot;
  };

  /** What each checkout has checked out, so a pull request can be matched to a row. */
  const branchOf = async (cwd: string): Promise<string | null> => {
    const status = await neosh.git.status({ cwd }).catch(() => null);
    return status?.repo.branch ?? null;
  };

  const draw = async () => {
    if (!settings.on) {
      for (const cwds of (await repositories()).values()) {
        for (const cwd of cwds) await neosh.ext.remove(POINT_DECORATION, idFor(cwd)).catch(() => {});
      }
      return;
    }
    for (const [root, cwds] of await repositories()) {
      const pulls = known.get(root)?.pulls ?? [];
      for (const cwd of cwds) {
        const branch = await branchOf(cwd);
        const pr = branch ? pulls.find((p) => p.branch === branch) ?? null : null;
        if (!pr) {
          // Withdrawn rather than drawn empty. A row with nothing to say about a pull request is a
          // row about a branch, which is most of them.
          await neosh.ext.remove(POINT_DECORATION, idFor(cwd)).catch(() => {});
          continue;
        }
        const parts = badge(pr, settings);
        await neosh.ext.contribute(POINT_DECORATION, idFor(cwd), {
          target: { project: cwd },
          // The number alone is the short form. Which pull request it is survives a narrow row;
          // whether its checks are red is the thing that can wait for a wider one.
          badge: { parts, short: parts.slice(0, 1) },
        }, {
          // Ahead of the dirty strip, which contributes at the default priority. A failing check is
          // bigger news than an untracked file, so it is the part that keeps its place when the two
          // are merged onto one row.
          priority: -5,
        }).catch(() => {});
      }
    }
  };

  const refresh = async (opts: { loud?: boolean } = {}) => {
    if (!settings.on && !opts.loud) return;
    for (const root of (await repositories()).keys()) {
      if (asking.has(root)) continue;
      asking.add(root);
      try {
        const pulls = await neosh.git.pulls({ cwd: root });
        known.set(root, { pulls, at: Date.now(), error: null });
        complained.delete(root);
      } catch (e) {
        const why = String(e);
        known.set(root, { pulls: known.get(root)?.pulls ?? [], at: 0, error: why });
        // Once per repository per reason. No `gh`, signed out and no remote are all permanent until
        // somebody does something about them, and a toast every three minutes about a program that
        // is still not installed is how people learn to ignore this corner of the screen.
        if (opts.loud) {
          neosh.notify(why, "warn");
        } else if (!complained.has(root)) {
          complained.add(root);
          neosh.log.info(`no pull requests for ${root}: ${why}`);
        }
      } finally {
        asking.delete(root);
      }
    }
    await draw();
  };

  subscriptions.push(
    await neosh.cmd.register("git.pulls", () => refresh({ loud: true }), {
      desc: "Ask the forge about this project's pull requests",
    }),
  );

  subscriptions.push(neosh.opt.onChange((e) => {
    if (
      e.name === "git.pulls" || e.name === "git.pulls.interval" ||
      e.name === "ui.ascii_only" || e.name === "ui.nerd_font"
    ) void reload().then(draw);
  }));
  // A conversation started somewhere new is a repository we have never asked about.
  subscriptions.push(neosh.vars.onChange((e) => {
    if (e.scope.scope === "global" && e.key === "sidebar.projects") void refresh();
  }));
  // A turn that pushed is a pull request that may have just appeared.
  subscriptions.push(neosh.agent.onTurnEnd(() => void draw()));

  // Checked every half minute against the interval rather than scheduled at it, so changing the
  // setting takes effect without restarting a timer and a machine that was asleep asks once when it
  // wakes rather than banking up what it missed.
  subscriptions.push(neosh.timer.every(30_000, () => {
    if (settings.interval <= 0) return;
    const stale = [...known.values()].every((k) => Date.now() - k.at >= settings.interval * 1000);
    if (known.size === 0 || stale) void refresh();
  }));

  // Not at `activate`: opening a workspace already starts a conversation and loads a dozen plugins,
  // and a network round trip through another program in the middle of that is a slower start for a
  // number nobody has looked at yet.
  neosh.event.on("neosh.ready", () => void refresh());

  return {
    // Falling back to `cwd` itself covers the main checkout before the first sweep has run, which
    // is the one case where the map is empty and the answer is still knowable.
    forBranch: (cwd, branch) =>
      known.get(rootOf.get(cwd) ?? cwd)?.pulls.find((p) => p.branch === branch) ?? null,
    refresh,
  };
}

/**
 * A pull request as a run of coloured parts, worst first, nothing said about what is fine.
 *
 * `#86` in the state's colour is the whole badge most of the time. A check mark is added **only
 * when it is news** — the transcript's rule for tool calls, one panel along: a `✓` beside every
 * merged pull request from the last six months is a column of marks nobody reads, and the one time
 * something is red it has to be the thing your eye lands on.
 */
export function badge(
  pr: PullRequest,
  glyphs: { ascii: boolean; nerd: boolean },
): Array<{ text: string; hl?: string }> {
  const parts: Array<{ text: string; hl?: string }> = [
    { text: `${mark(pr, glyphs)}${pr.number}`, hl: stateHl(pr.state) },
  ];
  if (pr.checks === "failing") {
    // The count, because one failing job out of nine is a different afternoon from nine out of
    // nine, and the number costs one column.
    parts.push({ text: " " });
    parts.push({
      text: `${glyphs.ascii ? "x" : "✗"}${pr.checks_failed > 1 ? pr.checks_failed : ""}`,
      hl: "Forge.ChecksFailed",
    });
  } else if (pr.checks === "pending") {
    // One glyph, which is what `Forge.ChecksRunning` requires: the frontend replaces the run with
    // successive frames and clips each to the width underneath, so a single column can never shift
    // anything to the right of it.
    parts.push({ text: " " });
    parts.push({ text: glyphs.ascii ? "*" : "•", hl: "Forge.ChecksRunning" });
  }
  return parts;
}

/**
 * The mark before the number.
 *
 * `#` everywhere a font cannot be trusted, because `#86` is what a pull request is called in prose,
 * in a commit message and in the forge's own UI — and a glyph nobody recognises is a worse label
 * than the punctuation everybody already reads. With a Nerd Font there are marks that say *merged*
 * and *closed* without colour, which is the accessibility half of the palette's own rule that
 * meaning survives losing colour.
 */
function mark(pr: PullRequest, glyphs: { ascii: boolean; nerd: boolean }): string {
  if (!glyphs.nerd) return "#";
  switch (pr.state) {
    case "merged":
      return " ";
    case "closed":
      return " ";
    case "draft":
      return " ";
    default:
      return " ";
  }
}

export function stateHl(state: PullRequest["state"]): string {
  switch (state) {
    case "draft":
      return "Forge.Draft";
    case "merged":
      return "Forge.Merged";
    case "closed":
      return "Forge.Closed";
    default:
      return "Forge.Open";
  }
}

/**
 * What the checks say, in the fewest columns that are still true — or nothing.
 *
 * **Nothing once it is merged or closed.** A merged pull request's checks are history: they passed,
 * that is why it merged, and `✓ checks passing` beside `#86 merged` is the least interesting
 * sentence on the screen — spending sixteen columns of a thirty-four-column panel to say it clipped
 * the state it was qualifying down to `#86 mer…`, which is the one word on that row that matters.
 *
 * And nothing when a repository runs no checks, because it has told you nothing and a tick would
 * say it had.
 */
export function checksWord(pr: PullRequest, ascii: boolean): string | null {
  if (pr.state === "merged" || pr.state === "closed") return null;
  switch (pr.checks) {
    case "failing":
      return `${ascii ? "x" : "✗"}${pr.checks_failed > 1 ? pr.checks_failed : ""} failing`;
    case "pending":
      return "running";
    case "passing":
      return `${ascii ? "ok" : "✓"} passing`;
    default:
      return null;
  }
}
