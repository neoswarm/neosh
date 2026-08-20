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
 * "git.branch.prefix" = "feature/"
 * "git.branch.instructions" = "Start with the Jira key when the message mentions one."
 * ```
 *
 * Nothing in this file is privileged. It is an ordinary plugin over `neosh.git` and `neosh.gen`,
 * and replacing it with your own is `plugins.disabled = ["git"]` plus a plugin directory.
 */

import { byteLength } from "@neosh/api";
import type { CommitInfo, Neosh, PluginContext, RepoStatus } from "@neosh/api";
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
- Describe the work in the message, not the act of asking for it.
- 2 to 6 words, lowercase, hyphen separated.
- No ticket prefixes, no punctuation, no trailing slash.
- Prefer the noun the change is about over the verb used to request it.`;

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
        'Prepended to every generated branch name, e.g. "feature/". Applied after slugging, so it survives verbatim.',
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
      (args: string[]) => newWorktree(neosh, args),
      "Create a worktree and start working in it — `git.worktree.new <branch> [path]`",
    ],
    ["git.worktree.remove", () => removeWorktree(neosh), "Remove a worktree"],
  ];
  for (const [name, fn, desc] of cmds) {
    await neosh.cmd.register(name, fn, { desc });
  }

  await neosh.keymap.set("chat", "<C-g>", "git.status", { desc: "Git status" });
  await neosh.keymap.set("chat", "<C-d>", "git.diff", { desc: "Show what changed" });

  // Which branch you are on belongs beside the model: both answer "where is what I type going".
  const footer = async () => {
    const status = await neosh.git.status().catch(() => null);
    if (!status) {
      await neosh.status.clear("branch");
      return;
    }
    const head = status.repo.detached
      ? `detached ${status.repo.head ?? ""}`.trim()
      : (status.repo.branch ?? "no branch");
    const dirty = status.changes.length;
    await neosh.status.set("branch", {
      text: `${head}${dirty ? ` ●${dirty}` : ""}`,
      hl: dirty ? "Git.Modified" : "Git.Branch",
      priority: 20,
    });
  };
  await footer();
  subscriptions.push(neosh.agent.onTurnEnd(() => void footer()));
  subscriptions.push(neosh.session.onChange(() => void footer()));
  // The working tree changes whenever anything writes a file, including the agent.
  subscriptions.push(neosh.timer.every(5000, () => void footer()));
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
    neosh.notify(`on ${chosen}`);
  } catch (e) {
    // Almost always uncommitted changes that would be overwritten. git's own message says which
    // files, and is more useful than anything this plugin could invent.
    neosh.notify(String(e), "error");
  }
}

async function newBranch(neosh: Neosh): Promise<void> {
  const description = await prompt(neosh, "What are you about to work on?", { width: 76 });
  if (!description || !description.trim()) return;

  neosh.notify("naming the branch…");
  let name: string;
  try {
    const answer = await neosh.gen.json<{ branch?: string }>(
      `${await promptFor(neosh, "branch", BRANCH_PROMPT)}\n\nUser message:\n${description}`,
    );
    if (typeof answer.branch !== "string" || answer.branch.trim() === "") {
      throw new Error("the model returned no branch name");
    }
    name = answer.branch;
  } catch (e) {
    neosh.notify(`could not name the branch: ${e}`, "error");
    return;
  }

  const prefix = (await neosh.opt.get<string>("git.branch.prefix")) ?? "";
  // Slugged here rather than in the host, so the name shown for approval below is exactly the
  // refname that will be created. "Why is my branch called that" is a question best answered
  // before the branch exists.
  const full = `${prefix}${slug(name)}`;

  const taken = new Set((await neosh.git.branches({ includeRemote: true })).map((b) => b.name));
  const unique = dedupe(full, taken);

  const edited = await prompt(neosh, "Branch name", { initial: unique, width: 76 });
  if (!edited || !edited.trim()) return;

  try {
    await neosh.git.createBranch(edited.trim());
    neosh.notify(`on ${edited.trim()}`);
  } catch (e) {
    neosh.notify(String(e), "error");
  }
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

  neosh.notify("writing the commit message…");
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

/**
 * Make a worktree and start working in it.
 *
 * **One question.** It used to ask twice — the branch, then the path, prefilled with the answer it
 * had already worked out. That second prompt was the whole point of `worktree.root` being a
 * setting, asked again: somebody who has configured where their worktrees go has said where their
 * worktrees go. The path is still reachable — `git.worktree.new <branch> <path>` takes it — but
 * from the palette or a key, the location follows from the configuration and the message says
 * where it landed.
 *
 * Landing in it is the point. Creating a directory you then have to go and find is not a feature,
 * it is a chore with extra steps.
 */
async function newWorktree(neosh: Neosh, args: string[] = []): Promise<void> {
  const status = await repoStatus(neosh);
  if (!status) return;

  const asked = args[0] ?? (await prompt(neosh, "Branch for the new worktree", { width: 70 }));
  if (!asked || !asked.trim()) return;
  const name = slug(asked);
  const where = (args[1] ?? (await worktreePath(neosh, status.repo.root, name))).trim();
  if (where === "") return;

  const taken = new Set((await neosh.git.branches({ includeRemote: true })).map((b) => b.name));
  const create = !taken.has(name);

  try {
    await neosh.git.addWorktree(where, name, { create });
  } catch (e) {
    neosh.notify(String(e), "error");
    return;
  }
  await neosh.session.create({ cwd: where });
  neosh.notify(`${create ? "branched" : "checked out"} ${name} in ${where}`);
}

/**
 * Where a worktree for `branch` goes.
 *
 * `<root>/<repo>/<branch>` under `worktree.root`, which is `~/.nsh` unless configured — a
 * directory of neosh's own rather than a sibling of the repository, because a worktree is not part
 * of the project you are working on and littering its parent with `foo-worktrees/` is how people
 * end up with checkouts they cannot account for. The repository name is a level of its own so two
 * projects with a `main` branch do not collide.
 *
 * An empty `worktree.root` restores the sibling layout, for anyone who wants their trees next to
 * the thing they are trees of.
 *
 * Slashes in a branch become dashes: `feat/thing` is one directory, not two, because the directory
 * is a name and not a path.
 */
async function worktreePath(neosh: Neosh, repoRoot: string, branch: string): Promise<string> {
  const leaf = branch.replace(/\//g, "-");
  const repoName = repoRoot.split("/").filter(Boolean).pop() ?? "repo";
  const configured = ((await neosh.opt.get<string>("worktree.root")) ?? "").trim();
  if (configured === "") return `${parentOf(repoRoot)}/${repoName}-worktrees/${leaf}`;
  const base = configured.startsWith("/") ? configured : `${repoRoot}/${configured}`;
  return `${base}/${repoName}/${leaf}`;
}

async function removeWorktree(neosh: Neosh): Promise<void> {
  const trees = (await neosh.git.worktrees().catch(() => [])).filter((t) => !t.is_main && !t.is_current);
  if (trees.length === 0) {
    neosh.notify("nothing to remove — the main worktree and the one you are in are off limits");
    return;
  }
  const chosen = await picker(
    neosh,
    trees.map((t) => ({ label: t.branch ?? t.path, detail: t.path, value: t })),
    { title: "Remove worktree", width: 78 },
  );
  if (!chosen) return;
  // A worktree is a directory with your work in it. The same gate as deleting a conversation, and
  // the same reason: `git worktree remove` does not put it back.
  if (!(await confirmDestructive(neosh, `Remove the worktree at ${chosen.path}?`, {
    yes: "Remove",
    no: "Keep",
  }))) {
    return;
  }

  try {
    await neosh.git.removeWorktree(chosen.path);
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
