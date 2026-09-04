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
 * branch called `brisk-otter-k3f9`, because a name chosen before the work is a decision made at the
 * worst possible moment. The first message sent in it is that decision, arriving on its own, so
 * the branch is renamed from it — `fix/composer-paste-truncation` — and never touched again.
 *
 * Nothing in this file is privileged. It is an ordinary plugin over `neosh.git` and `neosh.gen`,
 * and replacing it with your own is `plugins.disabled = ["git"]` plus a plugin directory.
 */

import { byteLength, CLONE_EVENT, sessionScope } from "@neosh/api";
import type {
  CloneProgress,
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
  meter,
  onTick,
  pager,
  pathPicker,
  picker,
  type PickerItem,
  prompt,
  spinnerFrame,
  statusPrefix,
} from "@neosh/api/ui";
import { configureMotion } from "@neosh/api/ui";
import { installPulls } from "./pulls.ts";
import { installGitSection, statParts } from "./sidebar.ts";

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

export async function activate(ctx: PluginContext) {
  const { neosh, subscriptions } = ctx;
  await defineHighlights(neosh);

  // The shared clock is module-global *per plugin*, so a spinner drawn here has to be told what
  // this terminal can render even though the sidebar has already told its own copy.
  configureMotion({
    enabled: (await neosh.opt.get<boolean>("ui.motion").catch(() => true)) ?? true,
    ascii: (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false,
  });
  subscriptions.push(neosh.opt.onChange((e) => {
    if (e.name !== "ui.motion" && e.name !== "ui.ascii_only") return;
    void (async () => {
      configureMotion({
        enabled: (await neosh.opt.get<boolean>("ui.motion").catch(() => true)) ?? true,
        ascii: (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false,
      });
    })();
  }));

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
    [
      "git.worktree.move",
      // Same shape one argument further: the tree, then where it goes. Both optional and both
      // asked for when missing, so this is the palette's verb, the panel's verb and a script's
      // verb without any of them needing a form of its own.
      (args: string[]) =>
        moveWorktree(neosh, { path: arg(args, 0), dest: arg(args, 1), cwd: arg(args, 2) }),
      "Move a worktree somewhere else — `git.worktree.move [path] [dest] [cwd]`",
    ],
    // The sidebar hands a row over as `(kind, cwd, …)`, which is not the shape the commands above
    // take from the palette. Two small verbs translate rather than every command growing a second
    // calling convention.
    [
      "git.sidebar.pull",
      // A project or conversation row arrives as `(kind, cwd, …)`; a *contributed* row arrives as
      // `("custom", <section>)`, where the second slot is a section id and emphatically not a path.
      // Read as one, `p` on the git block tried to pull a repository called `git` and reported that
      // it could not find one — which is a true sentence about a question nobody asked. A row of
      // ours means the conversation's own checkout, which is what no `cwd` at all already says.
      (args: string[]) => pull(neosh, arg(args, 0) === "custom" ? undefined : arg(args, 1)),
      "Pull the repository of the sidebar row under the cursor",
    ],
    [
      "git.sidebar.worktree.remove",
      (args: string[]) => removeWorktree(neosh, { path: arg(args, 1), cwd: arg(args, 1) }),
      "Remove the worktree of the sidebar row under the cursor",
    ],
    [
      "git.sidebar.worktree.move",
      (args: string[]) => moveWorktree(neosh, { path: arg(args, 1), cwd: arg(args, 1) }),
      "Move the worktree of the sidebar row under the cursor",
    ],
  ];
  for (const [name, fn, desc] of cmds) {
    await neosh.cmd.register(name, fn, { desc });
  }

  // Registered on its own rather than in the table above, because it *answers*: `cmd.call` gives a
  // caller back what the handler returned, and the sidebar needs the directory this landed in so
  // it can open the project. The table is typed `Promise<void>` and every verb in it is a thing
  // you do rather than a thing you ask.
  await neosh.cmd.register(
    "git.clone",
    // `?? ""` rather than a non-null assertion: no URL at all is a real call somebody will make —
    // `git.clone` from the palette with nothing typed after it — and it is answered with the usage
    // line rather than a crash.
    (args: string[]) => cloneRepository(neosh, arg(args, 0) ?? "", arg(args, 1)),
    {
      desc:
        "Clone a repository and open it — `git.clone <url> [destination]`. Asks where without one",
    },
  );

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
  // `m` for move, on the rows it can mean something on. A plain letter rather than a chord for the
  // reason `d` and `p` are: the panel is not a text field, and the letters that read as the verb
  // are the ones worth spending. Nothing global — a verb about the row under the cursor asked from
  // a conversation has no row to be about, and `^K` runs `git.worktree.move` by name.
  await neosh.ext.contribute("sidebar.action", "worktree-move", {
    key: "m",
    label: "move worktree",
    command: "git.sidebar.worktree.move",
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
  // The first thing asked in a scratch worktree is what its branch should have been called.
  //
  // Before the reads below for the same reason they register their own listeners first, and with
  // more at stake: what follows is a `git status` per project in the sidebar, run one after
  // another, and a workspace with a dozen of them on a slow disk spends seconds here. A turn
  // started in that window is the *first* turn in a new worktree — the one turn this exists for —
  // and a turn that starts before the listener does is a turn nobody hears.
  subscriptions.push(
    neosh.agent.onTurnStart((e) => void nameScratchBranch(neosh, e.session as SessionId)),
  );
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
  //
  // **Handed the status rather than reading one.** This used to run `git status` on its own five-
  // second timer, and so did the sidebar block below — two subprocesses that stat the entire
  // working tree, a few hundred milliseconds apart, for the same answer, for ever. Worse than the
  // cost: they were two answers, so for a few seconds after every pull the footer and the panel
  // disagreed about how far behind you were. One read, everybody told.
  const footer = async (status: RepoStatus | null) => {
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

  // The block at the top of the sidebar: which branch, what has drifted, and the one key that does
  // something about it. Contributed, so `plugins.disabled = ["git"]` takes it away with everything
  // else here and a panel that is not ours picks it up unchanged.
  //
  // It also owns the reading. Whoever polls `git status` has to do it on a timer — the agent writes
  // files all through a turn — and having *two* things poll it is two subprocesses for one answer
  // and two answers for one question. So the block reads, and the footer above is told; a git block
  // turned off with `git.sidebar = false` still reads, because the reading was never the block's.
  // What the forge knows, on the same rows. Its own module because it is a different server, a
  // different failure mode and a much slower clock: a working tree changes while you watch and a
  // pull request changes while somebody else does.
  const pulls = await installPulls(ctx);

  const section = await installGitSection(ctx, {
    pulls,
    onStatus: (status) => void footer(status),
    // A pull moves HEAD, and the project rows carry a badge built from a different call than the
    // one the block just refreshed.
    onMoved: () => void decorate(),
  });
  // What anything moving `HEAD` calls. The block re-reads and hands the answer on, so the footer
  // follows without this having to know it exists.
  headMoved = () => void section.refresh();
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

  // A repository mid-refactor has more changed files than any panel is tall, and every one past the
  // bottom edge used to be drawn nowhere with nothing on screen to say the list went on. The height
  // is a ceiling and the panel scrolls; the `Math.min(24, lines.length)` this used to compute was a
  // guess at a number only the frontend has.
  await pager(neosh, lines, { title: " git status ", width: 76, height: 24, kind: KIND_STATUS });
}

/** The status panel's kind, so `?` lists its keys and somebody else can add one. */
const KIND_STATUS = "neosh.git.status";

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
    headMoved();
  } catch (e) {
    // Almost always uncommitted changes that would be overwritten. git's own message says which
    // files, and is more useful than anything this plugin could invent.
    neosh.notify(String(e), "error");
  }
}

/**
 * Everything that draws a branch name, redrawn now.
 *
 * Set by `activate`. Anything that moves `HEAD` calls it instead of announcing the move in the
 * corner: the branch already has two places on screen, and a toast saying what they say is the same
 * fact printed three times. Without it both are up to five seconds stale — which is why the toast
 * was there, and is the thing to fix rather than to caption.
 *
 * One hook rather than one per panel, because it is one `git status`: the sidebar block re-reads
 * and hands the answer to the footer, so a third thing wanting a branch name is a subscriber rather
 * than another subprocess.
 */
let headMoved: () => void = () => {};

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
    headMoved();
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
 *
 * `gen.field` rather than `gen.json`, because a model asked for `{"branch": …}` answers with a
 * bare `fix/tab-strip-missing-in-worktree` about one time in ten — the name it was asked for,
 * without the envelope. Read as JSON that was a failed request and a worktree left on
 * `warm-juniper` for good, with the name it should have had sitting in the reply and nothing
 * anywhere saying so.
 */
async function nameBranch(neosh: Neosh, description: string, cwd?: string): Promise<string> {
  const branch = await neosh.gen.field(
    `${await promptFor(neosh, "branch", BRANCH_PROMPT)}\n\nUser message:\n${description}`,
    "branch",
    await branchModel(neosh),
  );
  const name = slug(branch);
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

  // Which way a diff line went is a background, so the group bands the whole row.
  const marks = parts.map((line) => {
    const hl = diffHl(line);
    return hl ? [{ col: 0, opts: { hlGroup: hl, endCol: byteLength(line) } }] : [];
  });

  // A diff is the panel most reliably longer than a screen — thirty rows was never the size of a
  // patch, it was the size of the box we were prepared to draw one in.
  await pager(neosh, parts, {
    title: ` ${chosen.path} `,
    width: 100,
    height: 30,
    kind: KIND_DIFF,
    marks,
  });
}

/** The diff panel's kind. */
const KIND_DIFF = "neosh.git.diff";

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
 * real and its name is two words and a tag, and the decision is deferred rather than skipped:
 * the first message sent in the tree is what it should have been called, so that is what it gets
 * called. See [`nameScratchBranch`].
 *
 * **A name that is free is checked, not assumed.** Both namespaces a worktree lands in — the
 * branches and the directories beside it — because the second outlives the first by design, and a
 * key that works nineteen times and then says `already exists` is the shape that bug had.
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

  // A worktree occupies two namespaces and the branch list is only one of them. See `occupied`.
  const parent = await worktreeParent(neosh, root, spec.inside);
  const here = spec.path ? new Set<string>() : await occupied(neosh, parent);

  const asked = spec.branch ??
    (spec.auto
      ? scratchName(taken, here)
      : await prompt(neosh, "Branch for the new worktree", { width: 70 }));
  if (!asked || !asked.trim()) return;
  const name = slug(asked);
  const where = (spec.path ?? worktreeDir(parent, name, here)).trim();
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
    // The one write in this file that changes a branch name without going through a key. The host
    // puts the *project row* right on its own; everything drawing the branch of the conversation
    // you are in has to be told, or the block at the top of the panel says `stout-falcon` for up to
    // five seconds after the sidebar row beside it has already caught up.
    headMoved();
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
 * Two words and a tag that are not a branch yet.
 *
 * An adjective and a noun, because a name you can say out loud is a name you can find again in a
 * list of eight of them — `brisk-otter` is a thing you remember starting, `wt-3` is not, and a
 * timestamp is neither. Both lists are short, concrete and unambiguous when spoken.
 *
 * The two words alone are 1,672 names, which sounds like plenty and is not: thirty trees in, a
 * repository is about one in four to have already drawn the same name twice, and what that looks
 * like from the keyboard is a key that fails with `already exists` some of the time and works the
 * rest — which is worse than one that always does. So the words carry a four-character tag —
 * `brisk-otter-k3f9` — which is what actually keeps them apart, and the words are what keep them
 * *readable*. Together that is about 1.5 billion, which is not a secret and is not meant to be one:
 * it is enough that nobody ever meets the same name twice, and short enough to type.
 *
 * The alphabet leaves out `0`, `1`, `i`, `l` and `o`, because a name is a thing you read off one
 * screen and type into another.
 *
 * Collisions are still *checked* rather than hoped away — the caller has the branch list and the
 * directory listing in its hand already, and those are the two namespaces a worktree occupies. The
 * counter suffix is the floor: it never loops forever, and `brisk-otter-k3f9-2` is still a name.
 */
function scratchName(...taken: ReadonlySet<string>[]): string {
  const free = (name: string) => !taken.some((t) => t.has(name));
  for (let i = 0; i < 50; i++) {
    const name = `${pick(SCRATCH_ADJECTIVES)}-${pick(SCRATCH_NOUNS)}-${tag()}`;
    if (free(name)) return name;
  }
  const base = `${pick(SCRATCH_ADJECTIVES)}-${pick(SCRATCH_NOUNS)}-${tag()}`;
  for (let n = 2; ; n++) if (free(`${base}-${n}`)) return `${base}-${n}`;
}

function pick<T>(xs: readonly T[]): T {
  return xs[Math.floor(Math.random() * xs.length)]!;
}

/** Four characters of the alphabet above. */
function tag(): string {
  return Array.from({ length: 4 }, () => pick(SCRATCH_ALPHABET)).join("");
}

const SCRATCH_ALPHABET = "23456789abcdefghjkmnpqrstuvwxyz".split("");

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
  const layouts = await worktreeLayouts(neosh, repoRoot, branch);
  const want = inside ? "inside" : "configured";
  // The list is deduplicated, so the layout asked for may have been folded into an earlier one
  // that lands in the same place — a relative `worktree.root` is both `configured` and `inside`.
  // Falling back to the head of the list is that same answer under its other name.
  return (layouts.find((l) => l.kind === want) ?? layouts[0]!).path;
}

/** One place a worktree can live, as both a path and a row somebody can be offered. */
interface Layout {
  kind: "configured" | "inside" | "beside";
  label: string;
  detail: string;
  icon: string;
  hl: string;
  path: string;
}

/**
 * Every default place this repository's worktrees go, in the order the "where?" question offers
 * them.
 *
 * One list, read twice. {@link worktreePath} asks it where a *new* tree lands, and
 * {@link moveWorktree} draws it as a menu for one that already exists — which is the whole reason
 * it is a list rather than three branches of an `if`. The alternative was what this replaced: the
 * layouts computed here and *described*, separately, in the panel's own rows, so a fourth place to
 * put a worktree would have had to be added in two files that never reference each other.
 *
 * **Deduplicated by path, first one wins.** The three layouts are not always three places: a
 * relative `worktree.root` makes `configured` and `inside` the same directory, and an empty one
 * makes `configured` and `beside` the same. A menu offering the same destination twice under two
 * names is a menu where picking the wrong row is impossible to notice, and the row that survives
 * is the one whose name matches what the configuration actually says.
 *
 * Slashes in a branch become dashes throughout: `feat/thing` is one directory, not two, because
 * the directory is a name and not a path.
 */
async function worktreeLayouts(
  neosh: Neosh,
  repoRoot: string,
  branch: string,
): Promise<Layout[]> {
  const leaf = branch.replace(/\//g, "-");
  const repoName = repoRoot.split("/").filter(Boolean).pop() ?? "repo";
  const configured = ((await neosh.opt.get<string>("worktree.root")) ?? "").trim();
  const dir = insideDir(configured);

  const all: Layout[] = [
    {
      kind: "configured",
      label: "Where worktrees go",
      // What `worktree.root` currently says, rather than a description of what it could say: a row
      // that names the setting is a row you have to go and read the setting to understand.
      detail: configured === ""
        ? "beside the repository — `worktree.root` is unset"
        : `under ${configured}`,
      icon: "+",
      hl: "Diagnostic.Ok",
      path: configured === ""
        ? `${parentOf(repoRoot)}/${repoName}-worktrees/${leaf}`
        : configured.startsWith("/")
        ? `${configured}/${repoName}/${leaf}`
        : `${repoRoot}/${configured}/${leaf}`,
    },
    {
      kind: "inside",
      label: "In this project",
      detail: `kept in ${dir}/ — travels with the repository`,
      icon: "⌂",
      hl: "Accent",
      path: `${repoRoot}/${dir}/${leaf}`,
    },
    {
      kind: "beside",
      label: "Beside the repository",
      detail: `a sibling of ${repoName}/`,
      icon: "⎇",
      hl: "Sidebar.Dim",
      path: `${parentOf(repoRoot)}/${repoName}-worktrees/${leaf}`,
    },
  ];

  const seen = new Set<string>();
  return all.filter((l) => {
    if (seen.has(l.path)) return false;
    seen.add(l.path);
    return true;
  });
}

/**
 * The directory this repository's worktrees go in, rather than the tree itself.
 *
 * Asked of {@link worktreePath} rather than worked out again, so the layouts stay one list: the
 * leaf is a placeholder because every layout puts the tree directly in the directory this wants.
 * Two callers want the parent and not the path — {@link occupied}, which asks what is in there,
 * and {@link worktreeDir}, which has to know before it knows the name.
 */
async function worktreeParent(neosh: Neosh, repoRoot: string, inside = false): Promise<string> {
  return parentOf(await worktreePath(neosh, repoRoot, "leaf", inside));
}

/**
 * The directory names already in use where a worktree would land.
 *
 * **A directory outlives the branch it was named after.** A scratch tree is created at
 * `.worktrees/brisk-otter` on a branch of the same name, and then the first message renames the
 * branch to `fix/the-login` and deliberately leaves the directory where it is — moving it would
 * invalidate the conversation's `cwd`. So `git branch` stops mentioning `brisk-otter` while the
 * directory sits there forever, and a branch list is not an answer to "is this name free": the
 * next tree to land on those two words picks a name git is happy with and a path git refuses, and
 * `git worktree add` fails with `already exists` on a name nothing on screen said was taken.
 *
 * Asked of the filesystem rather than of `git worktree list`, because a tree removed by hand, or
 * pruned, leaves a directory that git no longer lists and `add` still trips over.
 *
 * Best-effort, and capped by the completion limit at two hundred entries — so this narrows the
 * window rather than closing it, and the tag in {@link scratchName} is what carries the guarantee.
 * A parent that does not exist yet is an empty list, which is the right answer.
 */
async function occupied(neosh: Neosh, parent: string): Promise<Set<string>> {
  const answer = await neosh.path.complete(`${parent}/`).catch(() => ({ paths: [] as string[] }));
  return new Set(
    answer.paths.map((p) => p.replace(/\/+$/, "").split("/").pop() ?? "").filter((n) => n !== ""),
  );
}

/**
 * Where a worktree for `branch` goes, given what is already in `parent`.
 *
 * The directory is *only* a place. The branch is what you named and what you will look for
 * afterwards; where its checkout sits is this plugin's business, so a name already taken on disk
 * gets the counter rather than the failure — `feat/thing` in `feat-thing-2` is a working tree on
 * the branch you asked for, and `fatal: 'feat-thing' already exists` is a directory you now have to
 * go and look at before you can start.
 *
 * Not the same decision as {@link moveWorktree}'s, which reports git's refusal and should: a
 * destination you picked off a menu is one you meant, and quietly landing beside it is the answer
 * to a question nobody asked.
 */
function worktreeDir(parent: string, branch: string, taken: Set<string>): string {
  return `${parent}/${dedupe(branch.replace(/\//g, "-"), taken)}`;
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

/**
 * Move a worktree somewhere else, asking where the way `^N` asks where.
 *
 * The question is the same question — a worktree lands in one of a few places, and those places
 * have names — so it is asked with the same rows, out of {@link worktreeLayouts}, plus the path
 * field for a destination nobody listed. What it is *not* is a second vocabulary: somebody who has
 * learnt that "In this project" means `.worktrees/` when they make a tree has learnt what it means
 * when they move one.
 *
 * `path` given is the sidebar's flow — `m` on the row *is* the pointing — and `dest` given as well
 * is the scripted one, which asks nothing. Without either it is the palette's flow and both are
 * pickers. The move runs from the main checkout for the reason removal does: git will not saw off
 * the branch it is standing on, and the conversation this runs in may be standing in the tree.
 *
 * **Where it is now is a row, drawn as such and declining to be picked.** Hiding it would leave a
 * menu of two places for a repository that has three, which reads as the third one not existing —
 * and the question somebody presses `m` to answer is *which of these am I in*.
 *
 * No confirmation. A move is reversible by pressing `m` again, and a dialog charged for something
 * you can undo is what teaches people to clear dialogs without reading them. What it is not
 * allowed to do is happen while an agent is working in the tree, and that is the host's to refuse
 * — it holds the conversations and knows which of them has a turn in flight.
 */
async function moveWorktree(
  neosh: Neosh,
  spec: { path?: string; dest?: string; cwd?: string } = {},
): Promise<void> {
  const all = await neosh.git.worktrees(spec.cwd ? { cwd: spec.cwd } : undefined).catch(() => []);
  const main = all.find((t) => t.is_main)?.path;
  const movable = all.filter((t) => !t.is_main);

  let tree: WorktreeInfo | undefined;
  if (spec.path) {
    const named = all.find((t) => t.path === spec.path);
    if (!named) {
      neosh.notify(`no worktree at ${spec.path}`, "warn");
      return;
    }
    // The repository itself. `git worktree move` refuses it, and it should: the main checkout is
    // where the `.git` directory lives, and moving that is not this feature.
    if (named.is_main) {
      neosh.notify("this is the repository itself — `m` moves a worktree row", "warn");
      return;
    }
    tree = named;
  } else {
    if (movable.length === 0) {
      neosh.notify("nothing to move — this repository has only its main checkout");
      return;
    }
    tree = await picker(
      neosh,
      movable.map((t) => ({
        label: t.branch ?? t.path,
        detail: t.path,
        icon: "⎇",
        hl: "Git.Branch",
        value: t,
      })),
      { title: "Move worktree", width: 78 },
    ) ?? undefined;
  }
  if (!tree) return;
  const from = tree.path;

  let dest = spec.dest;
  if (!dest) {
    const branch = tree.branch ?? basename(from);
    const layouts = await worktreeLayouts(neosh, main ?? from, branch);
    const rows: Array<PickerItem<Layout | { kind: "elsewhere" }>> = layouts.map((l) => ({
      // "where it is now" goes in the *label*, not after the path. Both are one row and the row
      // is a float's width, so something is getting clipped — and a clipped path still reads as a
      // path while a clipped sentence is gone. The label is also what the eye lands on, which is
      // where the one row you must not pick should say so.
      label: l.path === from ? `${l.label} — where it is now` : l.label,
      // The resolved path, and only that. A choice between places is only a choice if each row
      // says where it goes — but the *path* is that sentence: `…/work/.worktrees/crisp-yarrow`
      // already says it is in the project, and a row that then adds "kept in .worktrees/" is one
      // that wraps onto a second line to repeat itself. The description each layout carries is
      // still what the row is *called*, and still matches the filter.
      detail: l.path,
      keywords: `${l.path} ${l.detail}`,
      icon: l.path === from ? "●" : l.icon,
      hl: l.path === from ? "Sidebar.Dim" : l.hl,
      value: l,
    }));
    rows.push({
      label: "Somewhere else…",
      detail: "type a path — completes as you go",
      keywords: "path directory elsewhere custom rename",
      icon: "/",
      hl: "Status.Input",
      value: { kind: "elsewhere" },
    });

    const chosen = await picker(neosh, rows, {
      title: `Move ${tree.branch ?? basename(from)}`,
      width: 78,
    }).catch(() => null);
    if (!chosen) return;
    if (chosen.kind === "elsewhere") {
      // Seeded with the whole path rather than its parent, so `<CR>` on an untouched field is a
      // no-op and editing the tail renames the directory. `^W` walks back up a segment, which is
      // how this field is also the way to move it somewhere unrelated.
      const typed = await pathPicker(neosh, "Move worktree to", { initial: from, width: 78 });
      if (typed === null || typed.trim() === "") return;
      dest = typed.trim();
    } else {
      dest = chosen.path;
    }
  }

  if (dest === from) {
    neosh.notify("already there");
    return;
  }

  neosh.progress("git.worktree.move", "moving…");
  try {
    await neosh.git.moveWorktree(from, dest, main ? { cwd: main } : undefined);
  } catch (e) {
    // A destination that exists, a tree git has locked, a turn running in it — the host and git
    // each word their own refusal better than anything this plugin could invent from the outside.
    neosh.notify(String(e), "error");
    return;
  } finally {
    neosh.done("git.worktree.move");
  }
  await landed(neosh, dest);
  neosh.notify(`moved to ${dest}`);
}

/**
 * Light the row it landed on, once.
 *
 * The panel redraws on its own the moment the conversations move, so the row is *correct* without
 * this — and a row that is merely correct is one you have to go and find, having pressed a key
 * whose whole effect happened in a directory you cannot see. `Agent.ToolLanded` is the same
 * argument one surface along: half of watching something happen is watching it land.
 *
 * A decoration rather than a redraw of our own, because this plugin does not own that panel; and
 * withdrawn on a timer just past the flash, because a decoration left behind is a row that lights
 * up again every time anything else redraws it.
 */
async function landed(neosh: Neosh, cwd: string): Promise<void> {
  if (!(await ensureMovedGroup(neosh))) return;
  const id = `moved:${cwd}`;
  await neosh.ext.contribute("sidebar.decoration", id, {
    target: { project: cwd },
    hl: HL_MOVED,
  }).catch(() => {});
  neosh.timer.after(FLASH_MS + 250, () => {
    void neosh.ext.remove("sidebar.decoration", id).catch(() => {});
  });
}

const HL_MOVED = "Git.Moved";
/** Long enough to be seen from the other side of the panel, short enough not to be a state. */
const FLASH_MS = 420;

/**
 * Define the group the flash rides on, colour and all, from the palette rather than from here.
 *
 * A flash is a property of a *group*, and the frontend lifts a run's foreground toward white for
 * as long as it lasts — so the group needs a real colour underneath or the row draws in `Normal`
 * for the third of a second it is lit, which is the near-white flicker a panel full of running
 * agents used to have. It cannot be a `link` either: a link resolves to somebody else's spec, and
 * this needs that spec *plus* one field.
 *
 * So the colour is read out of `Diagnostic.Ok` — the green the "where?" question already draws the
 * row that makes something in — and re-read whenever the theme moves. `default: true`, so a user's
 * `init.ts` still wins. `false` when the palette cannot answer, and then there is simply no
 * decoration: a flash is the least important thing in this operation.
 */
async function ensureMovedGroup(neosh: Neosh): Promise<boolean> {
  const base = await neosh.hl.get("Diagnostic.Ok").then((h) => h.resolved).catch(() => null);
  if (!base) return false;
  return await neosh.hl
    .define(HL_MOVED, { ...base, animate: { kind: "flash", ms: FLASH_MS } }, { default: true })
    .then(() => true)
    .catch(() => false);
}

/** The last segment of a path — a worktree's directory name, when it has no branch to be named by. */
function basename(path: string): string {
  return path.replace(/\/+$/, "").split("/").filter(Boolean).pop() ?? path;
}

function parentOf(path: string): string {
  const at = path.replace(/\/+$/, "").lastIndexOf("/");
  return at <= 0 ? "/" : path.slice(0, at);
}

// ---------------------------------------------------------------------------
// Cloning — the other way a project arrives
// ---------------------------------------------------------------------------

/** Where a clone has been put before, remembered so the next one can offer it. */
const VAR_CLONE_LOCATIONS = "clone.locations";

/**
 * How many remembered locations are kept.
 *
 * A list that only grows is a list nobody curates, and a destination picker twenty rows long is
 * exactly what `^O` was rescued from. The most recent handful are the ones anybody means.
 * `clone.root` is never in here — it is a setting, and a setting does not age out.
 */
const CLONE_LOCATION_LIMIT = 6;

/**
 * The parts of an address a destination is built from.
 *
 * Derived here rather than passed in, so `git.clone <url>` is a complete call from the palette, a
 * key, or a script. The sidebar has already parsed the same thing to draw its row and could have
 * handed it over — but an argument list of four positional strings is a worse public verb than one
 * that reads its own URL, and the two parses agreeing is then not something anybody has to keep
 * true.
 */
function readAddress(url: string): { owner: string; repo: string; host: string } {
  const trimmed = url.replace(/\.git$/, "").replace(/\/+$/, "");
  // `https://host/…` and `ssh://git@host/…`, then the `git@host:…` spelling. `file:///srv/x` has
  // no host at all — `[^/]+` finds nothing after `//` — which is the right answer rather than a
  // gap: there is no server in it.
  const host = /^[a-z][a-z0-9+.-]*:\/\/(?:[^/@]*@)?([^/]+)/i.exec(trimmed)?.[1] ??
    /^[^/@]+@([^:/]+)[:/]/.exec(trimmed)?.[1] ?? "";
  const parts = trimmed
    .replace(/^[a-z][a-z0-9+.-]*:\/\//i, "")
    .replace(/^[^/@]*@/, "")
    .split(/[/:]/)
    .filter(Boolean);
  // The host is a segment too, and it is not an owner. Dropped only when there *is* one, so the
  // first directory of a `file://` path is not mistaken for a server and thrown away.
  if (host && parts[0] === host) parts.shift();
  const repo = parts.pop() ?? "repo";
  return { owner: parts.pop() ?? "", repo, host };
}

/**
 * Clone a repository, asking where it goes, and answer with the directory it landed in.
 *
 * `into` skips the asking. That is what makes this callable from a script: `git.clone <url>` opens
 * a picker, and a picker is a thing only a person at a terminal can answer — so a caller with no
 * screen names the destination and this writes it without a question. Every other path through
 * here asks.
 *
 * **It always asks.** A clone writes a whole history onto your disk, and where that happens is a
 * thing people care about differently — one root for work, another for things they are only
 * reading, a directory an editor is already pointed at. Choosing silently and reporting it
 * afterwards means the first thing anybody does with this is go and find where it put something.
 * The default is the first row and `↵` takes it, so caring is optional and knowing is not.
 *
 * The rows are `clone.root` first, then everywhere you have cloned to before, then
 * `Somewhere else…`. What you pick through that last one is remembered, and that is the whole of
 * "adding a location": no list to curate, no setting to find — clone somewhere once and it is a
 * row from then on.
 *
 * A destination that already exists is **shown and refused**, never hidden. The directory being
 * there is very often the answer to "why is this repository not in my sidebar", and a row that
 * quietly disappears cannot say so.
 *
 * Answers `null` for a clone that was cancelled or failed. The caller opens what comes back, and
 * opening a directory that is not there is a second error about the first one.
 */
async function cloneRepository(
  neosh: Neosh,
  address: string,
  into?: string,
): Promise<string | null> {
  const url = (address ?? "").trim();
  if (url === "") {
    neosh.notify("git.clone needs a repository URL — `git.clone <url> [destination]`", "warn");
    return null;
  }
  const { owner, repo, host } = readAddress(url);
  const name = owner ? `${owner}/${repo}` : repo;
  const ascii = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false;

  // Told where, so nothing is asked. The script path: a picker is a thing only a person at a
  // terminal can answer, and a call that opens one and waits is a call that never returns for a
  // caller with no screen.
  if (into && into.trim() !== "") {
    const at = into.trim().replace(/\/+$/, "");
    if (await directoryExists(neosh, at)) {
      neosh.notify(`${at} is already there — nothing was cloned`, "warn");
      return null;
    }
    if (!await runClone(neosh, url, at, name, ascii)) return null;
    await neosh.session.create({ cwd: at }).catch((e: unknown) => neosh.notify(String(e), "warn"));
    return at;
  }

  const root = ((await neosh.opt.get<string>("clone.root").catch(() => "")) ?? "").trim();
  const remembered =
    (await neosh.vars.get<string[]>({ scope: "global" }, VAR_CLONE_LOCATIONS).catch(() => null)) ??
      [];

  // `clone.root` nests by owner, because it is the one directory that collects repositories from
  // everywhere and `api` is the name of a great many of them. A location you chose yourself does
  // not: you picked `~/work` meaning "put it in here", and `~/work/owner/repo` answers a question
  // you did not ask.
  const destinations = [
    ...(root ? [{ at: join(root, owner, repo), from: root, owned: true }] : []),
    ...remembered.filter((l) => l && l !== root).map((l) => ({
      at: join(l, "", repo),
      from: l,
      owned: false,
    })),
  ];
  const taken = await Promise.all(destinations.map((d) => directoryExists(neosh, d.at)));

  type Dest = { kind: "at"; path: string } | { kind: "ask" };
  const rows: PickerItem<Dest>[] = destinations.map((d, i) => ({
    label: d.at,
    detail: taken[i]
      ? "already there — pick another"
      : d.owned
      ? "clone.root"
      : "you cloned here before",
    keywords: `${d.from} ${repo}`,
    icon: taken[i] ? (ascii ? "!" : "•") : ascii ? "v" : "⤓",
    hl: taken[i] ? "Sidebar.Dim" : "Diagnostic.Ok",
    value: { kind: "at" as const, path: d.at },
  }));
  rows.push({
    label: "Somewhere else…",
    detail: "pick a folder — it is remembered and offered next time",
    keywords: "path folder directory browse elsewhere new location",
    icon: "…",
    hl: "Sidebar.Dim",
    value: { kind: "ask" as const },
  });

  const picked = await picker<Dest>(neosh, rows, {
    title: `Clone ${name}`,
    width: 84,
    height: Math.min(12, rows.length + 3),
    hints: `${host ? `from ${host}   ` : ""}↵ clone here   Esc cancel`,
  });
  if (picked === null) return null;

  let at: string;
  let learn: string | null = null;
  if (picked.kind === "at") {
    at = picked.path;
  } else {
    // The *folder it goes in*, not the destination itself: `<folder>/<repo>` is what gets made, so
    // the question has one answer rather than two — and that answer is exactly the thing worth
    // remembering for next time.
    const folder = await pathPicker(neosh, `Clone ${repo} into which folder?`, {
      initial: root ? `${root}/` : "~/",
    });
    if (folder === null || folder.trim() === "") return null;
    learn = folder.trim().replace(/\/+$/, "");
    at = join(learn, "", repo);
  }

  if (await directoryExists(neosh, at)) {
    // Said here rather than left to git, which reports it as a `fatal:` about a destination path —
    // accurate, and reads to somebody who just pressed `↵` on a menu row as though the clone went
    // wrong rather than as though that row was already taken.
    neosh.notify(`${at} is already there — nothing was cloned`, "warn");
    return null;
  }

  const ok = await runClone(neosh, url, at, name, ascii);
  if (!ok) return null;
  if (learn) await rememberLocation(neosh, remembered, learn, root);

  // Landing in it is the point, exactly as it is for a worktree. A verb that fetches a repository
  // and then leaves you where you were has done the work and withheld the reason for it — you
  // would go and find the thing you just asked for, which is the complaint `swarm.command` fixed
  // one panel along. Done here rather than by the caller so that `^K git.clone` and `^O` behave
  // the same; the sidebar therefore takes the path as *already opened* and does not open it again.
  await neosh.session.create({ cwd: at }).catch((e: unknown) => neosh.notify(String(e), "warn"));
  return at;
}

/** `<root>/<owner>/<repo>`, skipping the middle when there is no owner to put there. */
function join(root: string, owner: string, repo: string): string {
  const base = root.replace(/\/+$/, "");
  return owner ? `${base}/${owner}/${repo}` : `${base}/${repo}`;
}

/**
 * Fetch the repository, drawing how far it has got, and say whether it arrived.
 *
 * **A progress row, not a panel.** This is the mechanism the workspace already has for this shape
 * of thing — keyed, replaced in place, because a clone is a *state* and not a series of messages —
 * and a float of its own would be the notice system rebuilt one plugin along, with a modal holding
 * the keyboard for however long a large repository takes. Nothing here takes the keyboard: the
 * picker has closed, the composer has focus, and the row updates beside whatever you do next.
 *
 * The bar is the honest part. git reports a percentage for the phases that have a total and a bare
 * count for the ones that do not, so `Receiving objects` draws a meter and `Enumerating objects`
 * draws a spinner — never a bar creeping along on an invented denominator, which is the one thing
 * a progress display must not do. The phases are git's own words, so what is on screen is what
 * `git clone` would have said in a shell.
 */
async function runClone(
  neosh: Neosh,
  url: string,
  at: string,
  name: string,
  ascii: boolean,
): Promise<boolean> {
  const key = `git.clone.${at}`;
  let phase = "Connecting";
  let percent: number | null = null;

  const draw = () => {
    const tail = percent === null
      ? `${spinnerFrame()}  ${phase}`
      : `${meter(percent / 100, 12, { ascii })} ${String(percent).padStart(3)}%  ${phase}`;
    neosh.progress(key, `${ascii ? "v" : "⤓"} ${name}   ${tail}`);
  };
  draw();

  // Only this clone's events. Two can be running — a second `^O` while the first is still going —
  // and a row fed by both would report whichever spoke last under the other's name.
  const watching = neosh.event.on(CLONE_EVENT, (e) => {
    const p = e.data as CloneProgress | undefined;
    if (!p || p.path !== at || p.done) return;
    phase = p.phase;
    percent = typeof p.percent === "number" ? p.percent : null;
    draw();
  });
  // The spinner's own clock. Without it a phase with no percentage is a frozen glyph for as long
  // as that phase lasts, which reads as a workspace that has stopped rather than one waiting on a
  // server.
  const ticking = onTick(() => {
    if (percent === null) draw();
  });

  try {
    await neosh.git.clone(url, at);
    neosh.notify(`cloned ${name} into ${at}`);
    return true;
  } catch (e) {
    // git's own last line — `Repository not found`, `Permission denied (publickey)`, `could not
    // resolve host`. Each is a different thing for the reader to go and do, and "clone failed" is
    // none of them.
    neosh.notify(`could not clone ${name} — ${String(e)}`, "warn");
    return false;
  } finally {
    watching.dispose();
    ticking.dispose();
    neosh.done(key);
  }
}

/**
 * Whether `path` is a directory that is already there.
 *
 * Asked through `path.complete`, which answers with *full* paths and a trailing slash — so "does
 * this exist" is an exact match inside the completion of its own name, and needs no call of its
 * own. Matching the whole string matters: completing `…/neosh` also returns `…/neosh-web/`, and a
 * prefix test would report the wrong directory as taken.
 *
 * Only ever `false` on failure. The clone itself refuses a non-empty directory by name, so being
 * wrong here costs a keystroke rather than somebody's work.
 */
async function directoryExists(neosh: Neosh, path: string): Promise<boolean> {
  const answer = await neosh.path.complete(path).catch(() => ({ paths: [] as string[] }));
  const want = path.replace(/\/+$/, "");
  return answer.paths.some((p) => p.replace(/\/+$/, "") === want);
}

/** Put a location at the front of the remembered list, deduplicated and bounded. */
async function rememberLocation(
  neosh: Neosh,
  had: string[],
  add: string,
  root: string,
): Promise<void> {
  // Never `clone.root`: it is already the first row, from the setting, and a copy of it here would
  // be a second row saying the same thing that no longer moves when the setting does.
  if (add === root) return;
  const next = [add, ...had.filter((l) => l && l !== add)].slice(0, CLONE_LOCATION_LIMIT);
  await neosh.vars.set({ scope: "global" }, VAR_CLONE_LOCATIONS, next).catch(() => {});
}
