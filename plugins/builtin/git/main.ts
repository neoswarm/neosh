/**
 * Git actions: pick a branch, start one named after what you are about to do, commit with a
 * written message.
 *
 * **Every prompt here is a setting.** The defaults below are what you get out of the box; each one
 * has a `.prompt` option that replaces it wholesale and an `.instructions` option that appends to
 * it. That split matters — appending is what you want nine times out of ten ("always prefix with
 * the ticket number"), and replacing is the escape hatch for when it is not.
 *
 * ```toml
 * [options]
 * "git.branch.instructions" = "Start with the Jira key when the message mentions one."
 * "git.branch.model" = "anthropic/claude-haiku-4-5-20251001"  # empty uses the model you talk to
 * "git.branch.auto" = false                            # keep the scratch name
 * ```
 *
 * **A worktree names itself once you have said what it is for.** `git.worktree.new.auto` creates a
 * branch called `brisk-otter`, because a name chosen before the work is a decision made at the
 * worst possible moment. The first message sent in it is that decision, arriving on its own, so
 * the branch is renamed from it — `fix/composer-paste-truncation` — and never touched again.
 *
 * Nothing in this file is privileged. It is an ordinary plugin over `neosh.git` and `neosh.gen`,
 * and replacing it with your own is `plugins.disabled = ["git"]` plus a plugin directory.
 */

import { byteLength, sessionScope } from "@neosh/api";
import type {
  CommitInfo,
  ModelSelection,
  Neosh,
  PluginContext,
  RepoStatus,
  SessionId,
  WorktreeInfo,
} from "@neosh/api";
import {
  confirm,
  confirmDestructive,
  defineHighlights,
  picker,
  prompt,
  statusPrefix,
} from "@neosh/api/ui";

// ---------------------------------------------------------------------------
// Default prompts
// ---------------------------------------------------------------------------

const BRANCH_PROMPT = `You generate git branch names.
Return a JSON object with exactly one key: branch.

Rules:
- Start with the type of work, as a prefix ending in a slash. Use exactly one of:
  feature/ for something new, fix/ for a defect, refactor/ for a change that keeps behaviour,
  chore/ for maintenance, docs/ for documentation, test/ for tests, perf/ for a speed-up.
- Choose the type from what the work is, not from the words used to ask for it: "the sidebar
  flickers" is fix/, "add a sidebar" is feature/.
- After the prefix, 2 to 6 words describing the work, lowercase, hyphen separated.
- Prefer the noun the change is about over the verb used to request it.
- No ticket keys, no punctuation, no trailing slash, nothing after the words.

Examples:
- "the composer eats the last character when you paste" -> fix/composer-paste-truncation
- "let people pin a project to the top" -> feature/pin-project
- "bump the deno version" -> chore/bump-deno`;

const COMMIT_PROMPT = `You write git commit messages.
Return a JSON object with keys: subject, body.

Rules:
- subject is imperative, at most 72 characters, with no trailing period.
- body is either an empty string or short bullet points explaining why, not what.
- Describe the primary user-visible or developer-visible change.
- Do not mention the diff format, the number of files, or that you are an assistant.`;

export async function activate({ neosh, subscriptions }: PluginContext) {
  await defineHighlights(neosh);

  for (const spec of [
    {
      name: "git.branch.prompt",
      description:
        "Replaces the branch-naming prompt entirely. Empty uses the built-in one. It must ask for JSON with a `branch` key.",
    },
    {
      name: "git.branch.instructions",
      description: "Appended to the branch-naming prompt. The usual way to add a house rule.",
    },
    {
      name: "git.commit.prompt",
      description:
        "Replaces the commit-message prompt entirely. Empty uses the built-in one. It must ask for JSON with `subject` and `body` keys.",
    },
    {
      name: "git.commit.instructions",
      description: "Appended to the commit-message prompt. Put your convention here.",
    },
    {
      name: "git.branch.prefix",
      description:
        'Prepended to a generated branch name that has no prefix of its own, e.g. "feature/". \
Applied after slugging, so it survives verbatim — and skipped when the model already chose a \
type, which the built-in prompt asks it to, so setting this does not produce "feature/fix/thing".',
    },
    {
      name: "git.branch.model",
      description:
        "Model for naming branches, as `instance/model`. Empty falls back to `gen.model`, and \
then to the model the conversation is using — so out of the box a branch is named by whatever you \
are already talking to.",
    },
  ]) {
    await neosh.opt.declare({
      name: spec.name,
      type: { type: "str" },
      default: "",
      description: spec.description,
    });
  }

  await neosh.opt.declare({
    name: "git.branch.auto",
    type: { type: "bool" },
    default: true,
    description:
      "Name an auto-created worktree's branch from the first message sent in it. Off leaves the \
two-word scratch name it was created with.",
  });
  await neosh.opt.declare({
    name: "git.commit.confirm",
    type: { type: "bool" },
    default: true,
    description: "Show the written commit message for approval before committing.",
  });
  await neosh.opt.declare({
    name: "git.diff.max_bytes",
    type: { type: "int", min: 1000, max: 400000 },
    default: 40000,
    description:
      "Largest patch sent to the model when writing a commit message. A large refactor is truncated rather than rejected.",
  });

  const cmds: Array<[string, (args: string[]) => Promise<void>, string]> = [
    ["git.status", () => showStatus(neosh), "Show working tree status"],
    ["git.branch.switch", () => switchBranch(neosh), "Switch to another branch"],
    ["git.branch.new", () => newBranch(neosh), "Start a branch named after what you describe"],
    ["git.commit", () => commit(neosh), "Stage everything and commit with a written message"],
    ["git.diff", () => showDiff(neosh), "Show what changed"],
    ["git.worktree.list", () => pickWorktree(neosh), "Open a conversation in another worktree"],
    [
      "git.worktree.new",
      // Empty is absent. A caller filling a later slot has to pass something for the earlier ones,
      // and `""` meaning "a branch called nothing" turns "ask me" into a silent no-op.
      (args: string[]) =>
        newWorktree(neosh, { branch: arg(args, 0), path: arg(args, 1), cwd: arg(args, 2) }),
      "Create a worktree and start working in it — `git.worktree.new <branch> [path] [cwd]`",
    ],
    [
      "git.worktree.new.auto",
      (args: string[]) => newWorktree(neosh, { cwd: arg(args, 0), auto: true }),
      "A worktree on a branch named for you, with nothing to answer — `git.worktree.new.auto [cwd]`",
    ],
    [
      "git.worktree.new.inside",
      (args: string[]) => newWorktree(neosh, { cwd: arg(args, 0), auto: true, inside: true }),
      "A worktree kept inside the repository, on a branch named for you — `git.worktree.new.inside [cwd]`",
    ],
    [
      "git.pull",
      (args: string[]) => pull(neosh, arg(args, 0)),
      "Pull from the remote — `git.pull [cwd]`",
    ],
    [
      "git.worktree.remove",
      // With a path, that worktree; without one, a picker. The path form is what a panel row
      // needs — a picker opened from the row you were already on asks you to point twice.
      (args: string[]) => removeWorktree(neosh, { path: arg(args, 0), cwd: arg(args, 1) }),
      "Remove a worktree — `git.worktree.remove [path] [cwd]`",
    ],
    // The sidebar hands a row over as `(kind, cwd, …)`, which is not the shape the commands above
    // take from the palette. Two small verbs translate rather than every command growing a second
    // calling convention.
    [
      "git.sidebar.pull",
      (args: string[]) => pull(neosh, arg(args, 1)),
      "Pull the repository of the sidebar row under the cursor",
    ],
    [
      "git.sidebar.worktree.remove",
      (args: string[]) => removeWorktree(neosh, { path: arg(args, 1), cwd: arg(args, 1) }),
      "Remove the worktree of the sidebar row under the cursor",
    ],
  ];
  for (const [name, fn, desc] of cmds) {
    await neosh.cmd.register(name, fn, { desc });
  }

  // Verbs on the sidebar's rows. Contributed rather than baked into the panel, because that is
  // what the contribution point is for — a sidebar that is not ours picks these up unchanged, and
  // `plugins.disabled = ["git"]` takes the keys away with the plugin.
  await neosh.ext.contribute("sidebar.action", "pull", {
    key: "p",
    label: "pull",
    command: "git.sidebar.pull",
    on: "any",
  });
  await neosh.ext.contribute("sidebar.action", "worktree-remove", {
    key: "d",
    label: "remove worktree",
    command: "git.sidebar.worktree.remove",
    on: "project",
  });

  await neosh.keymap.set("chat", "<C-g>", "git.status", { desc: "Git status" });
  await neosh.keymap.set("chat", "<C-d>", "git.diff", { desc: "Show what changed" });

  // Where this checkout stands, on the project's own row. A decoration rather than a section of
  // our own: the row already exists, the sidebar already knows how to draw a mark on it, and the
  // whole point of the point is that this plugin never imports the panel. Keyed by the project's
  // directory — which is a worktree's directory too, so a nested tree reports its own branch's
  // divergence rather than its repository's — and withdrawn when there is nothing to say, because
  // a `0` on every row is a column of noise.
  const decorate = async () => {
    const projects = await neosh.vars
      .get<unknown>({ scope: "global" }, "sidebar.projects")
      .catch(() => null);
    const cwds = Array.isArray(projects)
      ? projects.filter((p): p is string => typeof p === "string")
      : [];
    const ascii = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false;
    for (const cwd of cwds) {
      const status = await neosh.git.status({ cwd }).catch(() => null);
      const parts = status ? statParts(status, ascii) : [];
      if (parts.length === 0) {
        await neosh.ext.remove("sidebar.decoration", `dirty:${cwd}`).catch(() => {});
        continue;
      }
      await neosh.ext.contribute("sidebar.decoration", `dirty:${cwd}`, {
        target: { project: cwd },
        // The strip is ordered worst-first, so what to give up on a narrow row is already decided:
        // everything but the head of it. A project name the panel had to clip to make room for
        // `?7` is a row that gave up what it is to say something you did not ask for.
        badge: { parts, short: parts.slice(0, 1) },
      }).catch(() => {});
    }
  };
  // Listeners before the first read: the sidebar writes `sidebar.projects` from its first draw,
  // which can land while this plugin is still awaiting its own `git status`, and a change that
  // arrives before the listener exists is a change nobody hears.
  subscriptions.push(neosh.agent.onTurnEnd(() => void decorate()));
  subscriptions.push(neosh.session.onChange(() => void decorate()));
  subscriptions.push(
    neosh.vars.onChange((e) => {
      if (e.scope.scope === "global" && e.key === "sidebar.projects") void decorate();
    }),
  );
  // And once the workspace is up: the project list is somebody else's, so it is read when they
  // have had their say rather than when we have had ours.
  neosh.event.on("neosh.ready", () => void decorate());
  await decorate();

  // Which branch you are on belongs beside the model: both answer "where is what I type going".
  // The same vocabulary as the panel's badge, in one colour rather than several — a status segment
  // is one span, and the strip is short enough that the worst thing true of it is the right answer
  // to what colour it should be.
  const footer = async () => {
    const status = await neosh.git.status().catch(() => null);
    if (!status) {
      await neosh.status.clear("branch");
      return;
    }
    const ascii = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false;
    const head = status.repo.detached
      ? `detached ${status.repo.head ?? ""}`.trim()
      : (status.repo.branch ?? "no branch");
    const parts = statParts(status, ascii);
    const stats = parts.map((p) => p.text).join("");
    // The strip is ordered worst-first, so its head is both the colour the segment should be and
    // the one stat worth keeping when the terminal is too narrow for all of them. A segment with
    // no short form is dropped whole — which is how the branch name itself disappeared off a
    // 110-column footer the moment this said four things instead of one.
    const worst = parts[0];
    await neosh.status.set("branch", {
      text: `${head}${stats === "" ? "" : ` ${stats}`}`,
      short: worst ? `${head} ${worst.text}` : head,
      hl: worst?.hl ?? "Git.Branch",
      priority: 20,
    });
  };
  refreshFooter = () => void footer();
  await footer();
  subscriptions.push(neosh.agent.onTurnEnd(() => void footer()));
  subscriptions.push(neosh.session.onChange(() => void footer()));
  // The first thing asked in a scratch worktree is what its branch should have been called.
  subscriptions.push(
    neosh.agent.onTurnStart((e) => void nameScratchBranch(neosh, e.session as SessionId)),
  );
  // The working tree changes whenever anything writes a file, including the agent.
  subscriptions.push(neosh.timer.every(5000, () => void footer()));
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
function statParts(status: RepoStatus, ascii: boolean): Array<{ text: string; hl?: string }> {
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
// Commands
// ---------------------------------------------------------------------------

async function showStatus(neosh: Neosh): Promise<void> {
  const status = await repoStatus(neosh);
  if (!status) return;

  const lines = [describeHead(status)];
  if (status.changes.length === 0) {
    lines.push("", "clean");
  } else {
    lines.push("");
    for (const c of status.changes) {
      const rename = c.from ? `  ← ${c.from}` : "";
      lines.push(`${statusPrefix(c)}  ${c.path}${rename}`);
    }
  }

  const buf = await neosh.buf.create({ name: "[git status]", scratch: true });
  await neosh.buf.setLines(buf, 0, -1, lines);
  await neosh.float.open(buf, {
    anchor: { kind: "screen" },
    width: { kind: "fixed", n: 76 },
    height: { kind: "fixed", n: Math.min(24, lines.length) },
    border: "rounded",
    closeOnBlur: true,
  });
}

async function switchBranch(neosh: Neosh): Promise<void> {
  const branches = await neosh.git.branches().catch((e) => {
    neosh.notify(String(e), "error");
    return [];
  });
  if (branches.length === 0) return;

  const chosen = await picker(
    neosh,
    branches.map((b) => ({
      label: b.name,
      detail: [b.is_head ? "current" : "", tracking(b.ahead, b.behind), b.subject ?? ""]
        .filter(Boolean)
        .join("  ·  "),
      value: b.name,
    })),
    { title: "Switch branch", width: 76 },
  );
  if (!chosen) return;

  try {
    await neosh.git.checkout(chosen);
    refreshFooter();
  } catch (e) {
    // Almost always uncommitted changes that would be overwritten. git's own message says which
    // files, and is more useful than anything this plugin could invent.
    neosh.notify(String(e), "error");
  }
}

/**
 * Redraw the branch segment, now.
 *
 * Set by `activate`. Anything that moves `HEAD` calls it instead of announcing the move in the
 * corner: the branch already has a place on screen, and a toast saying what the footer says is the
 * same fact printed twice. Without this the footer is up to five seconds stale — which is why the
 * toast was there, and is the thing to fix rather than to caption.
 */
let refreshFooter: () => void = () => {};

async function newBranch(neosh: Neosh): Promise<void> {
  const description = await prompt(neosh, "What are you about to work on?", { width: 76 });
  if (!description || !description.trim()) return;

  // Progress rather than a message: it is a state that stops being true the moment the model
  // answers, and pushed onto the stack it sat *above* the row that superseded it.
  neosh.progress("git.name", "naming the branch…");
  let unique: string;
  try {
    unique = await nameBranch(neosh, description);
  } catch (e) {
    neosh.notify(`could not name the branch: ${e}`, "error");
    return;
  } finally {
    neosh.done("git.name");
  }

  const edited = await prompt(neosh, "Branch name", { initial: unique, width: 76 });
  if (!edited || !edited.trim()) return;

  try {
    await neosh.git.createBranch(edited.trim());
    refreshFooter();
  } catch (e) {
    neosh.notify(String(e), "error");
  }
}

/**
 * A refname for `description`: generated, slugged, prefixed and not already taken.
 *
 * Every step of it happens here rather than in the host so that the name a caller shows for
 * approval is exactly the name that will exist. "Why is my branch called that" is a question best
 * answered before the branch does.
 *
 * Throws rather than returning a fallback. There is no useful default branch name, and the two
 * callers want opposite things when this fails — one tells you, the other says nothing — which is
 * a decision for them and not for this.
 */
async function nameBranch(neosh: Neosh, description: string, cwd?: string): Promise<string> {
  const answer = await neosh.gen.json<{ branch?: string }>(
    `${await promptFor(neosh, "branch", BRANCH_PROMPT)}\n\nUser message:\n${description}`,
    await branchModel(neosh),
  );
  if (typeof answer.branch !== "string" || answer.branch.trim() === "") {
    throw new Error("the model returned no branch name");
  }
  const name = slug(answer.branch);
  const prefix = (await neosh.opt.get<string>("git.branch.prefix")) ?? "";
  // Skipped when the model already chose a type. The built-in prompt asks for `fix/`, `feature/`
  // and the rest, so a `git.branch.prefix` applied unconditionally on top of it would read
  // `feature/fix/composer-paste` — a prefix nobody meant and a type that is now wrong. Somebody
  // who wants the prefix *always* has the prompt: it is a setting for the same reason.
  const full = name.includes("/") ? name : `${prefix}${name}`;

  const taken = new Set(
    (await neosh.git.branches({ includeRemote: true, ...(cwd ? { cwd } : {}) }).catch(() => []))
      .flatMap((b) => [b.name, b.name.replace(/^[^/]+\//, "")]),
  );
  return dedupe(full, taken);
}

/**
 * Which model names a branch, if anyone said.
 *
 * Three rungs and this is only the first: `git.branch.model`, then `gen.model`, then the model the
 * conversation is using — and the last two are the host's, which is why an unset option returns
 * nothing at all rather than a guess. Out of the box that means your own model names the branch,
 * which is the right default for a workspace where the cheap model is not configured and the only
 * alternative would be failing.
 */
async function branchModel(
  neosh: Neosh,
): Promise<{ selection: ModelSelection } | undefined> {
  const spec = ((await neosh.opt.get<string>("git.branch.model")) ?? "").trim();
  const cut = spec.indexOf("/");
  if (cut <= 0 || cut === spec.length - 1) return undefined;
  return {
    selection: {
      instance: spec.slice(0, cut) as ModelSelection["instance"],
      model: spec.slice(cut + 1) as ModelSelection["model"],
    },
  };
}

async function commit(neosh: Neosh): Promise<void> {
  const status = await repoStatus(neosh);
  if (!status) return;
  if (status.changes.length === 0) {
    neosh.notify("nothing to commit");
    return;
  }

  const conflicted = status.changes.filter(
    (c) => c.staged === "conflicted" || c.unstaged === "conflicted",
  );
  if (conflicted.length > 0) {
    // Committing a conflict marker is a mistake that survives into history.
    neosh.notify(`resolve ${conflicted.length} conflicted file(s) first`, "error");
    return;
  }

  const anythingStaged = status.changes.some((c) => c.staged);
  if (!anythingStaged) {
    if (!(await confirm(neosh, "Nothing is staged. Stage everything?"))) return;
    await neosh.git.stage();
  }

  const limit = (await neosh.opt.get<number>("git.diff.max_bytes")) ?? 40000;
  const [patch, stat] = await Promise.all([
    neosh.git.diff({ kind: "staged" }),
    neosh.git.diff({ kind: "staged" }, { stat: true }),
  ]);
  if (patch.trim() === "") {
    neosh.notify("nothing staged to commit");
    return;
  }

  neosh.progress("git.commit", "writing the commit message…");
  let message: string;
  try {
    const answer = await neosh.gen.json<{ subject?: string; body?: string }>(
      [
        await promptFor(neosh, "commit", COMMIT_PROMPT),
        "",
        `Branch: ${status.repo.branch ?? "(detached)"}`,
        "",
        "Staged files:",
        stat.trim(),
        "",
        "Staged patch:",
        truncate(patch, limit),
      ].join("\n"),
    );
    const subject = (answer.subject ?? "").trim();
    if (subject === "") throw new Error("the model returned no subject");
    const body = (answer.body ?? "").trim();
    message = body ? `${subject}\n\n${body}` : subject;
  } catch (e) {
    neosh.notify(`could not write a commit message: ${e}`, "error");
    return;
  } finally {
    // Before the confirmation prompt, not after the commit: what the row claims is that a model is
    // writing, and it stops being that as soon as one has.
    neosh.done("git.commit");
  }

  if (await neosh.opt.get<boolean>("git.commit.confirm")) {
    const first = message.split("\n")[0] ?? message;
    const edited = await prompt(neosh, "Commit message", { initial: first, width: 76 });
    if (edited === null) return;
    const rest = message.split("\n").slice(1).join("\n");
    message = rest.trim() ? `${edited}\n${rest}` : edited;
  }

  try {
    const made: CommitInfo = await neosh.git.commit(message);
    neosh.notify(`committed ${made.short}: ${made.subject}`);
  } catch (e) {
    neosh.notify(String(e), "error");
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Resolve the prompt for one generation kind.
 *
 * `<kind>.prompt` replaces the default; `<kind>.instructions` is appended to whichever is in use —
 * including a replaced one, so a user with their own prompt can still layer a per-project rule on
 * top of it from `.neosh/config.toml`.
 */
async function promptFor(neosh: Neosh, kind: "branch" | "commit", fallback: string): Promise<string> {
  const override = ((await neosh.opt.get<string>(`git.${kind}.prompt`)) ?? "").trim();
  const extra = ((await neosh.opt.get<string>(`git.${kind}.instructions`)) ?? "").trim();
  const base = override || fallback;
  return extra ? `${base}\n\nAdditional instructions:\n${extra}` : base;
}

async function repoStatus(neosh: Neosh): Promise<RepoStatus | null> {
  try {
    return await neosh.git.status();
  } catch (e) {
    neosh.notify(String(e), "warn");
    return null;
  }
}

function describeHead(status: RepoStatus): string {
  const head = status.repo.detached
    ? `detached at ${status.repo.head ?? "?"}`
    : (status.repo.branch ?? "no branch");
  const track = tracking(status.repo.ahead, status.repo.behind);
  return track ? `${head}  ${track}` : head;
}

function tracking(ahead: number, behind: number): string {
  const bits: string[] = [];
  if (ahead) bits.push(`↑${ahead}`);
  if (behind) bits.push(`↓${behind}`);
  return bits.join(" ");
}

/**
 * Make a model's answer into something `git switch -c` accepts.
 *
 * The host slugs too, but doing it here means the name the user is shown for approval is the name
 * the branch will actually have.
 */
export function slug(raw: string): string {
  // Dots survive, because `feat/v1.2` is a name someone means. That makes the rules below live
  // rather than decorative: git rejects `..`, a component starting or ending with `.`, and a
  // component ending in `.lock`.
  const mapped = Array.from(raw.toLowerCase())
    .map((ch) => (/[a-z0-9/_.]/.test(ch) ? ch : "-"))
    .join("");
  const parts = mapped
    .split("/")
    .map((p) =>
      p
        .replace(/-+/g, "-")
        .replace(/\.{2,}/g, ".")
        // Trim *before* stripping `.lock`, or trailing punctuation from the model
        // ("login.lock!!" -> "login.lock-") hides the suffix from the pattern. Then trim again,
        // because stripping can expose a new trailing dot.
        .replace(/^[-.]+|[-.]+$/g, "")
        .replace(/(\.lock)+$/, "")
        .replace(/[-.]+$/, ""),
    )
    .filter((p) => p !== "");
  return parts.join("/") || "work";
}

export function dedupe(name: string, taken: Set<string>): string {
  if (!taken.has(name)) return name;
  for (let n = 2; n < 1000; n++) {
    const candidate = `${name}-${n}`;
    if (!taken.has(candidate)) return candidate;
  }
  return name;
}

/** Keep the *end* of a patch: the last hunks are the ones a truncated diff most needs. */
function truncate(patch: string, limit: number): string {
  if (patch.length <= limit) return patch;
  return `[earlier hunks truncated]\n\n${patch.slice(-limit)}`;
}

// ---------------------------------------------------------------------------
// Diffs
// ---------------------------------------------------------------------------

/**
 * What changed, one file at a time.
 *
 * Whole-repository diffs are unreadable in a terminal pane and the interesting question is almost
 * always "what happened to *this* file". So: pick a file, read its patch. Staged and unstaged are
 * shown together because that is how you actually look at a working tree — the split matters when
 * you commit, not when you read.
 */
async function showDiff(neosh: Neosh): Promise<void> {
  const status = await repoStatus(neosh);
  if (!status) return;
  if (status.changes.length === 0) {
    neosh.notify("nothing has changed");
    return;
  }

  const chosen = await picker(
    neosh,
    status.changes.map((c) => ({
      label: c.path,
      detail: [c.staged ? `staged ${c.staged}` : "", c.unstaged ?? ""].filter(Boolean).join("  ·  "),
      value: c,
    })),
    { title: "Changed files", width: 78 },
  );
  if (!chosen) return;

  // Both halves, labelled, so a file that is staged *and* edited again shows what each contains
  // rather than silently picking one.
  const parts: string[] = [];
  for (const [label, target] of [
    ["staged", { kind: "staged" as const }],
    ["unstaged", { kind: "unstaged" as const }],
  ]) {
    const patch = await neosh.git.diff(target as never).catch(() => "");
    const forFile = filterToFile(patch, chosen.path);
    if (forFile.trim()) {
      parts.push(`── ${label} ──`, ...forFile.split("\n"));
    }
  }
  if (parts.length === 0) {
    // Untracked files have no diff; showing "nothing" would look like a bug.
    parts.push(
      chosen.unstaged === "untracked"
        ? `${chosen.path} is untracked — there is nothing to diff against yet.`
        : `no textual diff for ${chosen.path}`,
    );
  }

  const buf = await neosh.buf.create({ name: `[diff] ${chosen.path}`, scratch: true });
  await neosh.buf.setLines(buf, 0, -1, parts);
  const ns = await neosh.ns.create("neosh.git.diff");
  for (let i = 0; i < parts.length; i++) {
    const hl = diffHl(parts[i] ?? "");
    if (hl) {
      await neosh.ns.mark(ns, buf, i, 0, { hlGroup: hl, endCol: byteLength(parts[i] ?? "") });
    }
  }

  const win = await neosh.float.open(buf, {
    anchor: { kind: "screen" },
    width: { kind: "max", n: 100 },
    height: { kind: "max", n: 30 },
    border: "rounded",
    title: ` ${chosen.path} `,
    closeOnBlur: true,
    focusable: true,
  });
  await neosh.focus.push(win);
}

/**
 * Keep only the hunk belonging to one file.
 *
 * `git diff -- <path>` would be a second subprocess and would need the path escaped; the patch is
 * already in hand, and its `diff --git` markers are unambiguous.
 */
function filterToFile(patch: string, path: string): string {
  const out: string[] = [];
  let inFile = false;
  for (const line of patch.split("\n")) {
    if (line.startsWith("diff --git ")) {
      inFile = line.includes(` b/${path}`) || line.endsWith(`/${path}`);
      if (inFile) out.push(line);
      continue;
    }
    if (inFile) out.push(line);
  }
  return out.join("\n");
}

function diffHl(line: string): string | undefined {
  if (line.startsWith("diff --git ") || line.startsWith("── ")) return "Diff.Header";
  if (line.startsWith("@@")) return "Diff.Hunk";
  // `+++`/`---` are file headers, not content, and colouring them as additions and deletions is
  // the single most common thing terminal diff viewers get wrong.
  if (line.startsWith("+++") || line.startsWith("---")) return "Diff.Header";
  if (line.startsWith("+")) return "Diff.Add";
  if (line.startsWith("-")) return "Diff.Delete";
  return undefined;
}

// ---------------------------------------------------------------------------
// Worktrees
// ---------------------------------------------------------------------------

/**
 * Switch to another worktree by opening a conversation in it.
 *
 * There is no "current directory" to change — a conversation carries its own, and `neosh.git`
 * answers about whichever one is active. That is what makes a worktree a place you can *be* rather
 * than a directory you happen to have created: the sidebar groups by it, the footer shows its
 * branch, and the agent's tools run in it.
 */
async function pickWorktree(neosh: Neosh): Promise<void> {
  const trees = await neosh.git.worktrees().catch((e) => {
    neosh.notify(String(e), "warn");
    return [];
  });
  if (trees.length === 0) return;
  if (trees.length === 1) {
    neosh.notify("this repository has one worktree — `git.worktree.new` makes another");
    return;
  }

  const chosen = await picker(
    neosh,
    trees.map((t) => ({
      label: t.branch ?? t.head ?? t.path,
      detail: [t.is_current ? "current" : "", t.is_main ? "main" : "", t.locked ? "locked" : "", t.path]
        .filter(Boolean)
        .join("  ·  "),
      keywords: t.path,
      value: t,
    })),
    { title: "Worktrees", width: 78 },
  );
  if (!chosen) return;

  // An existing conversation in that tree is almost always the one you meant; a new one otherwise.
  const existing = (await neosh.session.list()).find((s) => s.cwd === chosen.path);
  if (existing) {
    await neosh.session.switch(existing.id);
    neosh.notify(`in ${chosen.branch ?? chosen.path}`);
    return;
  }
  await neosh.session.create({ cwd: chosen.path });
  neosh.notify(`new conversation in ${chosen.branch ?? chosen.path}`);
}

/** One positional argument, with blank treated as not given. */
function arg(args: string[], at: number): string | undefined {
  const v = args[at]?.trim();
  return v ? v : undefined;
}

/** What a worktree is being made of, and what to call it. */
interface WorktreeSpec {
  /** The branch. Asked for when absent — unless `auto`, which invents one. */
  branch?: string;
  /** Where it lands. Follows from `worktree.root` when absent. */
  path?: string;
  /** Which repository. The conversation's own when absent. */
  cwd?: string;
  /** Name it rather than ask. See {@link scratchName}. */
  auto?: boolean;
  /**
   * Inside the repository, whatever `worktree.root` says.
   *
   * The per-call form of a relative root: the picker offers "in this project" as a *choice*, and a
   * choice that only works after editing config.toml is a row that lies to everyone who has not.
   * A relative `worktree.root` still names the directory; absent one, `.worktrees` is the word
   * every tool that does this has already agreed on.
   */
  inside?: boolean;
}

/**
 * Make a worktree and start working in it.
 *
 * **One question at most, and `auto` makes it none.** It used to ask twice — the branch, then the
 * path, prefilled with the answer it had already worked out. That second prompt was the whole point
 * of `worktree.root` being a setting, asked again: somebody who has configured where their
 * worktrees go has said where their worktrees go. The path is still reachable —
 * `git.worktree.new <branch> <path>` takes it — but from the palette or a key, the location follows
 * from the configuration and the message says where it landed.
 *
 * `auto` drops the last question too. Naming a branch before you know what the work is is a
 * decision you are not yet equipped to make, and every one of those is a reason not to start —
 * which is the opposite of what a key for "somewhere clean to try this" is for. So the branch is
 * real and its name is two words from a list, and the decision is deferred rather than skipped:
 * the first message sent in the tree is what it should have been called, so that is what it gets
 * called. See [`nameScratchBranch`].
 *
 * Landing in it is the point. Creating a directory you then have to go and find is not a feature,
 * it is a chore with extra steps.
 */
async function newWorktree(neosh: Neosh, spec: WorktreeSpec = {}): Promise<void> {
  const root = await repoRoot(neosh, spec.cwd);
  if (root === null) return;

  // Remote names count. `origin/fix-thing` with no local branch means `git worktree add` should
  // check the existing one out rather than fail trying to create it, and for a generated name it
  // means one that is free here but taken upstream is not offered.
  const taken = new Set(
    (await neosh.git.branches({ includeRemote: true, cwd: spec.cwd }).catch(() => []))
      .flatMap((b) => [b.name, b.name.replace(/^[^/]+\//, "")]),
  );

  const asked = spec.branch ??
    (spec.auto
      ? scratchName(taken)
      : await prompt(neosh, "Branch for the new worktree", { width: 70 }));
  if (!asked || !asked.trim()) return;
  const name = slug(asked);
  const where = (spec.path ?? (await worktreePath(neosh, root, name, spec.inside))).trim();
  if (where === "") return;

  const create = !taken.has(name);

  try {
    await neosh.git.addWorktree(where, name, { create, cwd: spec.cwd });
  } catch (e) {
    neosh.notify(String(e), "error");
    return;
  }
  const session = await neosh.session.create({ cwd: where });
  // Only a name *nobody chose* is one this plugin may replace later. A branch somebody typed is
  // theirs, and `git.worktree.new feat/thing` must not be quietly renamed the moment a message is
  // sent — so the mark goes on here, where the difference is still known, rather than being
  // guessed at afterwards from the shape of the name.
  if (spec.auto && create) {
    await neosh.vars.set(sessionScope(session.id), VAR_SCRATCH, name)
      .catch((e: unknown) => neosh.log.info(`could not mark ${name} as scratch: ${e}`));
  }
  neosh.notify(`${create ? "branched" : "checked out"} ${name} in ${where}`);
}

/**
 * The branch this conversation's worktree was created on, while that is still a name nobody chose.
 *
 * A var rather than a pattern match on the name. t3code recognises its own temporary branches by
 * their shape — `t3code/<8 hex>` — which works until somebody's real branch happens to look like
 * one, and which cannot tell `brisk-otter` that we generated from `brisk-otter` that you typed.
 * Writing it down at the one moment the difference is known costs a key and removes the guess.
 *
 * Session-scoped, so it is deleted with the conversation, and removed the moment the branch is
 * named — after which this worktree is an ordinary worktree and nothing here will touch it again.
 */
const VAR_SCRATCH = "git.branch.scratch";

/**
 * Name the branch of a scratch worktree after the first thing asked in it.
 *
 * The alternative is asking before the work starts, and a branch named before you know what the
 * work is is a decision made at the worst possible moment — which is the whole reason
 * `git.worktree.new.auto` exists and gives you `brisk-otter` instead. This is the other half:
 * `brisk-otter` is a fine thing to *start* on and a poor thing to find in `git branch` next week.
 *
 * On turn start rather than turn end, because the panel is drawn all through a turn and the row
 * you are watching should say what you asked for, not what a word list picked. `git branch -m` is
 * one ref write, so doing it under a running agent changes nothing about the files it is editing.
 *
 * **One attempt, ever.** The mark is removed before the model is asked, so a cheap model that
 * hiccups costs one request rather than one per message for the life of the conversation — the
 * rule the titles plugin arrived at the same way. Too *early* is not a failure and does not spend
 * it: a turn with nothing to read yet leaves the mark alone and waits for the next one.
 */
async function nameScratchBranch(neosh: Neosh, session: SessionId): Promise<void> {
  if (!((await neosh.opt.get<boolean>("git.branch.auto")) ?? true)) return;
  const scope = sessionScope(session);
  const scratch = await neosh.vars.get<string>(scope, VAR_SCRATCH).catch(() => null);
  if (!scratch) return;

  const info = (await neosh.session.list().catch(() => [])).find((x) => x.id === session);
  // Somebody renamed it themselves, or moved off it. Either way the name is theirs now.
  if (!info || info.branch !== scratch) {
    await neosh.vars.remove(scope, VAR_SCRATCH).catch(() => {});
    return;
  }

  const messages = await neosh.session.messages(session).catch(() => []);
  const asked = messages
    .filter((m) => m.role === "user")
    .flatMap((m) => m.content.flatMap((b) => (b.type === "text" ? [b.text] : [])))
    .join("\n\n")
    .trim()
    .slice(0, 4000);
  if (asked === "") return;

  await neosh.vars.remove(scope, VAR_SCRATCH).catch(() => {});
  try {
    const named = await nameBranch(neosh, asked, info.cwd);
    if (named === scratch) return;
    await neosh.git.renameBranch(scratch, named, { cwd: info.cwd });
    neosh.notify(`branch is ${named}`);
  } catch (e) {
    // Not worth interrupting for: the worktree works, the branch has a name, and a popup every
    // time a model hiccups would cost more than the name it failed to improve.
    neosh.log.info(`could not name ${scratch}: ${e}`);
  }
}

/**
 * The original checkout of the repository at `cwd`.
 *
 * Not `git status`, which is a subprocess that stats every file in the tree to answer a question
 * about the *repository* — and not `status.repo.root`, which in a linked worktree is that
 * worktree. A new worktree of `feat-thing` belongs beside the others under the repository's name,
 * not under `feat-thing`'s, and `is_main` is what says which one that is.
 */
async function repoRoot(neosh: Neosh, cwd?: string): Promise<string | null> {
  const trees = await neosh.git.worktrees(cwd ? { cwd } : undefined).catch((e: unknown) => {
    neosh.notify(String(e), "warn");
    return [] as WorktreeInfo[];
  });
  return trees.find((t) => t.is_main)?.path ?? trees[0]?.path ?? null;
}

/**
 * Two words that are not a branch yet.
 *
 * An adjective and a noun, because a name you can say out loud is a name you can find again in a
 * list of eight of them — `brisk-otter` is a thing you remember starting, `wt-3` is not, and a
 * timestamp is neither. Both lists are short, concrete and unambiguous when spoken.
 *
 * Collisions are checked rather than hoped away: with a few dozen worktrees the birthday problem
 * is real, and the caller has the branch list in its hand already. The counter suffix is the floor
 * — it never loops forever, and `brisk-otter-2` is still a name.
 */
function scratchName(taken: ReadonlySet<string>): string {
  const pick = <T>(xs: readonly T[]): T => xs[Math.floor(Math.random() * xs.length)]!;
  for (let i = 0; i < 50; i++) {
    const name = `${pick(SCRATCH_ADJECTIVES)}-${pick(SCRATCH_NOUNS)}`;
    if (!taken.has(name)) return name;
  }
  const base = `${pick(SCRATCH_ADJECTIVES)}-${pick(SCRATCH_NOUNS)}`;
  for (let n = 2; ; n++) if (!taken.has(`${base}-${n}`)) return `${base}-${n}`;
}

const SCRATCH_ADJECTIVES = [
  "amber", "brisk", "calm", "clever", "coral", "crisp", "dapper", "eager", "fleet", "gentle",
  "glad", "golden", "hardy", "keen", "lively", "lucid", "mellow", "merry", "nimble", "noble",
  "placid", "plucky", "quiet", "rapid", "ruby", "sage", "sleek", "solar", "spry", "stout",
  "sunny", "swift", "tidy", "vivid", "warm", "wily", "witty", "zesty",
] as const;

const SCRATCH_NOUNS = [
  "alder", "anchor", "badger", "beacon", "birch", "brook", "cedar", "comet", "coral", "crane",
  "delta", "ember", "falcon", "fern", "finch", "garnet", "harbor", "heron", "ibis", "juniper",
  "kestrel", "lantern", "lark", "linden", "marlin", "meadow", "mesa", "nimbus", "otter", "pike",
  "quartz", "raven", "reef", "ridge", "sable", "sparrow", "summit", "thistle", "tundra", "vale",
  "walrus", "willow", "yarrow", "zephyr",
] as const;

/**
 * Where a worktree for `branch` goes.
 *
 * `<root>/<repo>/<branch>` under `worktree.root`, which is `~/.nsh` unless configured — a
 * directory of neosh's own rather than a sibling of the repository, because a worktree is not part
 * of the project you are working on and littering its parent with `foo-worktrees/` is how people
 * end up with checkouts they cannot account for. The repository name is a level of its own so two
 * projects with a `main` branch do not collide.
 *
 * A *relative* root is inside the repository — `worktree.root = ".worktrees"` puts every tree at
 * `<repo>/.worktrees/<branch>` — and there the repository's name is a level of noise rather than a
 * disambiguator: nothing but this repository's worktrees can land in its own directory. The host
 * keeps an in-tree checkout out of `git status` by writing the directory into the repository's
 * `.gitignore`, so choosing this layout does not mean reading past an untracked directory forever
 * — in this clone or anyone else's.
 *
 * An empty `worktree.root` restores the sibling layout, for anyone who wants their trees next to
 * the thing they are trees of.
 *
 * Slashes in a branch become dashes: `feat/thing` is one directory, not two, because the directory
 * is a name and not a path.
 */
async function worktreePath(
  neosh: Neosh,
  repoRoot: string,
  branch: string,
  inside = false,
): Promise<string> {
  const leaf = branch.replace(/\//g, "-");
  const repoName = repoRoot.split("/").filter(Boolean).pop() ?? "repo";
  const configured = ((await neosh.opt.get<string>("worktree.root")) ?? "").trim();
  // Asked to stay inside regardless of what is configured. A relative root still names the
  // directory; anything else falls back to the conventional one.
  if (inside) return `${repoRoot}/${insideDir(configured)}/${leaf}`;
  if (configured === "") return `${parentOf(repoRoot)}/${repoName}-worktrees/${leaf}`;
  if (configured.startsWith("/")) return `${configured}/${repoName}/${leaf}`;
  return `${repoRoot}/${configured}/${leaf}`;
}

/** The in-repository directory worktrees go in: a relative `worktree.root`, else `.worktrees`. */
function insideDir(configured: string): string {
  const c = configured.trim();
  return c !== "" && !c.startsWith("/") ? c.replace(/\/+$/, "") : ".worktrees";
}

/**
 * `git pull`, saying what happened.
 *
 * The summary is git's own — "Already up to date." and a fast-forward range are different answers,
 * and a command that swallows both teaches you to run it twice to be sure.
 */
async function pull(neosh: Neosh, cwd?: string): Promise<void> {
  neosh.progress("git.pull", "pulling…");
  try {
    const summary = await neosh.git.pull(cwd ? { cwd } : undefined);
    neosh.notify(summary);
  } catch (e) {
    // Diverged branches, no remote, auth — git's message names it, and inventing a friendlier one
    // here would mean guessing which of those it was.
    neosh.notify(String(e), "error");
  } finally {
    neosh.done("git.pull");
  }
}

/**
 * Remove a worktree, with or without being told which.
 *
 * `path` given is the sidebar's flow — `d` on the row *is* the pointing — and a picker opened from
 * a row you were already on asks you to point twice. Without one it is the palette's flow, and the
 * picker is the pointing. Either way the removal runs from the main checkout: `git worktree
 * remove` refuses to saw off the branch it is sitting on, and the conversation this command runs
 * in may be sitting in exactly the tree being removed.
 */
async function removeWorktree(
  neosh: Neosh,
  spec: { path?: string; cwd?: string } = {},
): Promise<void> {
  const all = await neosh.git.worktrees(spec.cwd ? { cwd: spec.cwd } : undefined).catch(() => []);
  const main = all.find((t) => t.is_main)?.path;
  const removable = all.filter((t) => !t.is_main && !t.is_current);

  let chosen: WorktreeInfo | undefined;
  if (spec.path) {
    const named = all.find((t) => t.path === spec.path);
    if (!named) {
      neosh.notify(`no worktree at ${spec.path}`, "warn");
      return;
    }
    if (named.is_main) {
      neosh.notify("this is the repository itself — `d` removes a worktree row", "warn");
      return;
    }
    if (named.is_current) {
      neosh.notify("you are in this worktree — switch to another conversation first", "warn");
      return;
    }
    chosen = named;
  } else {
    if (removable.length === 0) {
      neosh.notify("nothing to remove — the main worktree and the one you are in are off limits");
      return;
    }
    chosen = await picker(
      neosh,
      removable.map((t) => ({ label: t.branch ?? t.path, detail: t.path, value: t })),
      { title: "Remove worktree", width: 78 },
    ) ?? undefined;
  }
  if (!chosen) return;
  // The conversations that go with it. They are deleted below, and a dialog that does not mention
  // them is a dialog you agreed to something else in: removing a worktree is a git operation, and
  // losing what you talked about in it is not.
  const doomed = (await neosh.session.list().catch(() => []))
    .filter((s) => s.cwd === chosen.path && !s.is_active).length;
  // A worktree is a directory with your work in it. The same gate as deleting a conversation, and
  // the same reason: `git worktree remove` does not put it back.
  if (!(await confirmDestructive(neosh, `Remove the worktree at ${chosen.path}?`, {
    yes: "Remove",
    no: "Keep",
    detail: [
      chosen.branch ? `The branch ${chosen.branch} stays; the checkout goes.` : "The checkout goes.",
      ...(doomed > 0
        ? [`${doomed} ${doomed === 1 ? "conversation" : "conversations"} in it will be deleted too.`]
        : []),
    ],
  }))) {
    return;
  }

  try {
    await neosh.git.removeWorktree(chosen.path, main ? { cwd: main } : undefined);
  } catch (e) {
    // git refuses when the tree has changes, which is the right answer and a better message than
    // anything this plugin could invent.
    neosh.notify(String(e), "error");
    return;
  }
  // Conversations in a directory that no longer exists are dead weight in the sidebar.
  for (const s of await neosh.session.list()) {
    if (s.cwd === chosen.path && !s.is_active) {
      await neosh.session.close(s.id).catch(() => {});
    }
  }
  neosh.notify(`removed ${chosen.path}`);
}

function parentOf(path: string): string {
  const at = path.replace(/\/+$/, "").lastIndexOf("/");
  return at <= 0 ? "/" : path.slice(0, at);
}
