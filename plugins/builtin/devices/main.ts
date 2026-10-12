/**
 * Which computer a terminal is for.
 *
 * # A window is for a computer
 *
 * A swarm puts several machines in one workspace, and the workspace draws all of them to everyone.
 * That is right for a list and wrong for a window you opened to work on the build box: its project
 * panel, the conversation it lands in and where `^N` starts were about every machine at once. So
 * each terminal carries an answer — `view.device` — and this plugin is what gives it one:
 *
 * * `neosh connect lb` arrives with the answer already given, and this lands it there: the newest
 *   conversation on that machine, opened as a conversation.
 * * `neosh` arrives with none, and `swarm.device` decides. `ask`, the default, is a question —
 *   *which computer?* — asked only when there is a second computer to answer it with, because a
 *   question with one possible answer is a keypress spent confirming it.
 * * `/device` asks again from inside, and `/device lb` answers without asking.
 *
 * What it narrows is what a terminal *shows* and where it *starts*, never what exists: a turn on a
 * machine this window is not for keeps running, and every computer is one `/device all` away.
 *
 * # Not the sidebar's
 *
 * The question is here rather than in the panel because a terminal with the sidebar turned off is
 * still a terminal on a computer, and because the answer is the workspace's — `view.device`, which
 * the panel, `^N` and anybody's plugin read — rather than a mode one panel is in.
 */

import type {
  KeyContext,
  Neosh,
  NodeInfo,
  PluginContext,
  SwarmNode,
  ViewDevice,
  ViewId,
} from "@neosh/api";
import { picker, type PickerItem } from "@neosh/api/ui";

/** What a terminal arriving without a name opens on. */
const OPTION = "swarm.device";
/** The last answer to the question, so the cursor starts where you went last time. */
const VAR_LAST = "swarm.device.last";
/** How long a terminal sent to a computer waits for it before saying it could not get there. */
const REACH_MS = 20_000;
/** How long a computer that is up gets to say what it has before it counts as having nothing. */
const INVENTORY_MS = 3_000;

export async function activate({ neosh, subscriptions }: PluginContext) {
  await neosh.opt.declare({
    name: OPTION,
    type: { type: "str" },
    default: "ask",
    description:
      "Which computer a new terminal opens on: `ask` asks whenever another computer is connected, `all` shows every computer at once, `here` is this one, and a computer's name or code (`lb`) is that one. `neosh connect <name>` and `/device` choose for one terminal whatever this says.",
  });

  // Which landing each terminal is on, so a second answer cancels the first: a terminal pointed at
  // the build box and then, before it connected, at this machine must not be yanked back to the
  // build box when the link finally comes up.
  const landing = new Map<ViewId, number>();

  const land = async (view: ViewId, device: ViewDevice | null) => {
    const ticket = (landing.get(view) ?? 0) + 1;
    landing.set(view, ticket);
    const live = () => landing.get(view) === ticket;
    const here = neosh.view.at(view);
    const current = await here.session.current().catch(() => null);
    if (!live()) return;
    switch (device?.kind) {
      case "here": {
        // Only when it is somewhere else. A terminal already in a conversation of yours stays in
        // it: which one is a choice somebody may already have made.
        if (!current?.mirror) return;
        const mine = (await neosh.session.list().catch(() => [])).find((s) => !s.mirror);
        if (mine && live()) await here.session.switch(mine.id).catch(() => {});
        return;
      }
      case "node":
        await toNode(neosh, view, device.node, current?.mirror?.node ?? null, live);
        return;
      default:
        // Every computer, or not decided yet: wherever it is is somewhere it may be.
        return;
    }
  };

  /** What to do with a terminal that has just arrived, or was here before this plugin loaded. */
  const arrive = async (view: ViewId) => {
    const device = await neosh.view.device(view).catch(() => null);
    if (device?.kind === "ask") {
      await ask(neosh, view, { force: true });
      return;
    }
    if (device !== null) {
      // Chosen before it arrived — `neosh connect lb` — so nothing announced it.
      await land(view, device);
      return;
    }
    const setting = ((await neosh.opt.get<string>(OPTION).catch(() => "ask")) ?? "ask").trim();
    if (setting === "" || setting.toLowerCase() === "ask") {
      await ask(neosh, view, { force: false });
      return;
    }
    const world = await survey(neosh);
    const chosen = resolve(world, setting);
    if (typeof chosen === "string") {
      neosh.notify(`${OPTION}: ${chosen}`, "warn");
      return;
    }
    await neosh.view.setDevice(chosen, view).catch(() => {});
  };

  subscriptions.push(neosh.view.onOpen((view) => void arrive(view)));
  subscriptions.push(neosh.view.onClose((view) => void landing.delete(view)));
  subscriptions.push(neosh.view.onDevice(({ view, device }) => void land(view, device)));

  subscriptions.push(
    await neosh.cmd.register("device", async (args, key) => {
      const view = await viewOf(neosh, key);
      if (view === null) return;
      const query = args.join(" ").trim();
      if (query === "") {
        await ask(neosh, view, { force: true });
        return;
      }
      const chosen = resolve(await survey(neosh), query);
      if (typeof chosen === "string") {
        neosh.notify(chosen, "warn");
        return;
      }
      await neosh.view.setDevice(chosen, view);
    }, { desc: "Which computer this terminal is for — /device lb, /device all, or ask" }),
  );
  subscriptions.push(
    await neosh.cmd.register("device.default", async () => {
      return await chooseDefault(neosh);
    }, { desc: "Which computer a new terminal opens on, or whether it asks" }),
  );
  // So the settings panel offers the list rather than a text field you have to know the answer to.
  subscriptions.push(
    await neosh.ext.contribute("settings.choose", OPTION, { option: OPTION, command: "device.default" }),
  );
}

// ---------------------------------------------------------------------------
// What there is
// ---------------------------------------------------------------------------

interface World {
  me: NodeInfo | null;
  peers: SwarmNode[];
  /** The short code each is drawn with in the project panel — `lb` — by node id. */
  codes: Record<string, string>;
  /** The colour each wears there, as a highlight group, by node id. */
  colors: Record<string, string>;
  /** How many conversations this computer has. */
  local: number;
}

async function survey(neosh: Neosh): Promise<World> {
  const [me, nodes, codes, colors, sessions] = await Promise.all([
    neosh.swarm.self().catch(() => null),
    neosh.swarm.nodes().catch(() => [] as SwarmNode[]),
    // The panel's own answers, so `@lb` here is `@lb` there. Absent when the sidebar is off, and
    // a list without codes is still a list.
    neosh.cmd.call<Record<string, string> | null>("swarm.codes").catch(() => null),
    neosh.cmd.call<Record<string, string> | null>("swarm.colors").catch(() => null),
    neosh.session.list().catch(() => []),
  ]);
  return {
    me,
    peers: nodes
      .filter((n) => n.info.id !== me?.id)
      .sort((a, b) => a.info.name.localeCompare(b.info.name)),
    codes: codes ?? {},
    colors: colors ?? {},
    local: sessions.length,
  };
}

/**
 * What a name means, or a sentence saying why it means nothing.
 *
 * The same reading `neosh connect` gives it, in the same order: the words, then a code — chosen to
 * be unique — then a name, an id, and the start of a name.
 */
function resolve(w: World, query: string): ViewDevice | string {
  const q = query.trim().replace(/^@/, "");
  const lower = q.toLowerCase();
  if (lower === "" || lower === "ask") return { kind: "ask" };
  if (["all", "*", "every", "everything", "everywhere"].includes(lower)) return { kind: "all" };
  if (["here", "local", "this", "localhost", "."].includes(lower)) return { kind: "here" };
  if (w.me && (w.me.name.toLowerCase() === lower || w.me.id === q)) return { kind: "here" };
  const pick = (hits: SwarmNode[], how: string): ViewDevice | string | null => {
    if (hits.length === 0) return null;
    if (hits.length === 1) return { kind: "node", node: hits[0]!.info.id };
    return `${hits.length} computers ${how} "${q}": ${
      hits.map((n) => `${n.info.name} (${n.info.id.slice(0, 8)})`).join(", ")
    }`;
  };
  return pick(w.peers.filter((n) => w.codes[n.info.id]?.toLowerCase() === lower), "are drawn as") ??
    pick(w.peers.filter((n) => n.info.name.toLowerCase() === lower), "are called") ??
    (q.length >= 4 ? pick(w.peers.filter((n) => n.info.id.startsWith(q)), "have an id starting") : null) ??
    pick(w.peers.filter((n) => n.info.name.toLowerCase().startsWith(lower)), "have a name starting") ??
    (w.peers.length === 0
      ? `no computer called "${q}" — none is paired with this one yet, and ^J adds one`
      : `no computer called "${q}" — there is ${
        w.peers.map((n) => w.codes[n.info.id] ? `${n.info.name} (@${w.codes[n.info.id]})` : n.info.name)
          .join(", ")
      }, and all and here`);
}

/** How the setting spells a choice: words a person can read in `config.toml`. */
function spell(w: World, d: ViewDevice): string {
  if (d.kind !== "node") return d.kind;
  const n = w.peers.find((p) => p.info.id === d.node);
  if (!n) return d.node;
  // The name unless another machine shares it, when only the id is unambiguous.
  return w.peers.filter((p) => p.info.name.toLowerCase() === n.info.name.toLowerCase()).length > 1
    ? n.info.id
    : n.info.name;
}

/** The answer the cursor should start on: what was chosen last, as `VAR_LAST` holds it. */
function sameDevice(a: ViewDevice | null, b: ViewDevice | null): boolean {
  if (!a || !b || a.kind !== b.kind) return false;
  return a.kind !== "node" || (b.kind === "node" && a.node === b.node);
}

function state(n: SwarmNode): string {
  switch (n.link.state) {
    case "up":
      return "connected";
    case "connecting":
    case "retrying":
      return "connecting…";
    case "waiting":
      return "has not allowed this computer yet";
    default:
      return n.reason ? `offline — ${n.reason}` : "offline";
  }
}

function count(n: number): string {
  return n === 0 ? "nothing yet" : n === 1 ? "1 conversation" : `${n} conversations`;
}

async function viewOf(neosh: Neosh, key: KeyContext | undefined): Promise<ViewId | null> {
  if (key) return key.view;
  return (await neosh.view.current().catch(() => null))?.view ?? null;
}

// ---------------------------------------------------------------------------
// The question
// ---------------------------------------------------------------------------

/**
 * Ask which computer a terminal is for.
 *
 * `force` is somebody asking for the question — `neosh connect`, `/device` — and is asked whatever
 * there is. Without it this is a terminal arriving, and it is asked only when there is a second
 * computer that is up or on its way up: a list of one is a keypress spent confirming it, and a
 * list of machines that are all off is a question whose only answer is this one.
 *
 * Every paired machine is a row, including the ones that are not there yet — the picker's rule
 * everywhere in this workspace, because a computer missing from the list reads as the feature not
 * existing. They redraw as they connect. **All computers** is offered only once there is more than
 * one to be all of.
 */
async function ask(neosh: Neosh, view: ViewId, opts: { force: boolean }): Promise<void> {
  let world = await survey(neosh);
  const coming = (n: SwarmNode) =>
    n.link.state === "up" || n.link.state === "connecting" || n.link.state === "retrying";
  if (!opts.force && !world.peers.some(coming)) return;
  if (world.peers.length === 0) {
    neosh.notify("This is the only computer in the workspace — ^J adds another", "info");
    await neosh.view.setDevice({ kind: "all" }, view).catch(() => {});
    return;
  }
  const here = neosh.view.at(view);
  // Where this terminal is, when it is somewhere — `/device` from inside is "move me", and the row
  // you are on now is where moving starts from. A terminal just arriving starts on the last answer.
  const now = await neosh.view.device(view).catch(() => null);
  const last = now && now.kind !== "ask"
    ? now
    : await neosh.vars.get<ViewDevice>({ scope: "global" }, VAR_LAST).catch(() => null);
  const ascii = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false;

  const items: PickerItem<ViewDevice>[] = [];
  const fill = () => {
    const up = world.peers.filter((n) => n.link.state === "up");
    const rows: PickerItem<ViewDevice>[] = [{
      label: world.me?.name ?? "This computer",
      detail: `this computer  ·  ${count(world.local)}`,
      icon: ascii ? "*" : "●",
      hl: "Diagnostic.Ok",
      value: { kind: "here" },
    }];
    for (const n of world.peers) {
      const code = world.codes[n.info.id];
      rows.push({
        label: n.info.name,
        detail: [code ? `@${code}` : null, state(n), n.link.state === "up" ? count(n.agents.length) : null]
          .filter(Boolean).join("  ·  "),
        icon: "@",
        // Its own colour while it can be reached, so the row you pick is the colour of the block
        // it is in the project panel; dim while it cannot.
        hl: n.link.state === "up"
          ? world.colors[n.info.id] ?? "Swarm.Up"
          : n.link.state === "waiting"
          ? "Swarm.Waiting"
          : n.link.state === "down"
          ? "Swarm.Down"
          : "Swarm.Linking",
        value: { kind: "node", node: n.info.id },
      });
    }
    if (up.length > 0) {
      rows.push({
        label: "All computers",
        detail: `this one and ${up.length === 1 ? up[0]!.info.name : `${up.length} others`}, together`,
        icon: ascii ? "+" : "◆",
        hl: "Accent",
        value: { kind: "all" },
      });
    }
    // In place, because `reload` re-reads the very array the picker was handed.
    items.splice(0, items.length, ...rows);
  };
  fill();
  const selected = Math.max(0, items.findIndex((i) => sameDevice(i.value, last)));

  // What a key other than `↵` decided, read once the picker has closed.
  let taken: ViewDevice | null = null;
  let always = false;
  const digits = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];
  const chosen = await picker<ViewDevice>(here, items, {
    title: "Which computer?",
    filter: false,
    width: 68,
    // Room for every row it can grow to: `All computers` arrives with the first machine to connect.
    height: Math.min(12, world.peers.length + 2),
    selected,
    hints: "↵ open   d always this one   esc all of them",
    ownKeys: ["d", ...digits],
    onKey(key, ctx) {
      if (key.key.code.kind !== "char" || key.key.mods.ctrl || key.key.mods.alt) return;
      const c = key.key.code.c;
      if (c === "d" && ctx.item) {
        taken = ctx.item;
        always = true;
        return "close";
      }
      const at = digits.indexOf(c);
      if (at >= 0 && items[at]) {
        taken = items[at]!.value;
        return "close";
      }
      return "handled";
    },
    subscribe: (reload) =>
      neosh.swarm.onChange(() => {
        void survey(neosh).then((w) => {
          world = w;
          fill();
          reload();
        });
      }),
  });
  const answer: ViewDevice = chosen ?? taken ?? { kind: "all" };
  // Escape is an answer too — every computer, which is the workspace as it always was — and it is
  // recorded as one, so the question is not asked again of this terminal by a reload.
  await neosh.view.setDevice(answer, view).catch((e: unknown) => neosh.notify(String(e), "warn"));
  if (chosen !== null || taken !== null) {
    await neosh.vars.set({ scope: "global" }, VAR_LAST, answer).catch(() => {});
  }
  if (always) {
    const value = spell(world, answer);
    try {
      await neosh.opt.save(OPTION, value);
      neosh.notify(
        `New terminals open on ${describe(world, answer)} from now on — /device or neosh connect still choose`,
      );
    } catch (e) {
      neosh.notify(String(e), "warn");
    }
  }
}

function describe(w: World, d: ViewDevice): string {
  switch (d.kind) {
    case "all":
      return "every computer";
    case "here":
      return "this computer";
    case "ask":
      return "a question";
    case "node":
      return w.peers.find((n) => n.info.id === d.node)?.info.name ?? d.node;
  }
}

/**
 * Choose what `swarm.device` says, from a list rather than a text field you have to already know
 * the answer to. Answers whether it changed, which is what the settings panel redraws on.
 */
async function chooseDefault(neosh: Neosh): Promise<boolean> {
  const world = await survey(neosh);
  const now = ((await neosh.opt.get<string>(OPTION).catch(() => "ask")) ?? "ask").trim();
  const current = resolve(world, now);
  type Choice = { value: string };
  const rows: PickerItem<Choice>[] = [
    {
      label: "Ask every time",
      detail: "whenever another computer is connected",
      icon: "?",
      hl: "Accent",
      value: { value: "ask" },
    },
    {
      label: "Every computer",
      detail: "one workspace, every machine in it",
      icon: "◆",
      hl: "Accent",
      value: { value: "all" },
    },
    {
      label: world.me?.name ?? "This computer",
      detail: "this computer",
      icon: "●",
      hl: "Diagnostic.Ok",
      value: { value: "here" },
    },
    ...world.peers.map((n) => ({
      label: n.info.name,
      detail: [world.codes[n.info.id] ? `@${world.codes[n.info.id]}` : null, state(n)].filter(Boolean)
        .join("  ·  "),
      icon: "@",
      hl: world.colors[n.info.id] ?? "Swarm.Up",
      value: { value: spell(world, { kind: "node", node: n.info.id }) },
    })),
  ];
  const at = rows.findIndex((r) => {
    const d = resolve(world, r.value.value);
    return typeof d !== "string" && typeof current !== "string" && sameDevice(d, current);
  });
  const chosen = await picker<Choice>(neosh, rows, {
    title: "New terminals open on",
    filter: false,
    width: 64,
    height: Math.min(12, rows.length),
    selected: Math.max(0, at),
    hints: "",
  });
  if (chosen === null || chosen.value === now) return false;
  try {
    await neosh.opt.save(OPTION, chosen.value);
    return true;
  } catch (e) {
    neosh.notify(String(e), "warn");
    return false;
  }
}

// ---------------------------------------------------------------------------
// Getting there
// ---------------------------------------------------------------------------

/**
 * Put a terminal in a conversation on another computer: the newest one there, opened as a
 * conversation.
 *
 * Waits for the link, because a terminal is very often pointed at a machine in the second its
 * workspace started dialling it — and dials it, once, when nothing is: asking for a terminal on a
 * machine is asking to be connected to it. A machine that is up and has nothing on it says so and
 * stays put; `^N` in that terminal starts on it.
 */
async function toNode(
  neosh: Neosh,
  view: ViewId,
  node: string,
  inMirrorOf: string | null,
  live: () => boolean,
): Promise<void> {
  // Already there: which conversation on it is somebody's choice.
  if (inMirrorOf === node) return;
  const started = Date.now();
  let upSince: number | null = null;
  let redialled = false;
  while (live()) {
    const n = (await neosh.swarm.nodes().catch(() => [] as SwarmNode[])).find((p) => p.info.id === node);
    if (!live()) return;
    if (!n) {
      neosh.notify("that computer is no longer paired with this one", "warn");
      return;
    }
    if (n.link.state === "waiting") {
      neosh.notify(`${n.info.name} has not allowed this computer yet — ^J over there allows it`, "warn");
      return;
    }
    if (n.link.state === "down" && !redialled) {
      redialled = true;
      await neosh.swarm.reconnect(node).catch(() => {});
    }
    if (n.link.state === "up") {
      upSince ??= Date.now();
      const newest = [...n.agents].sort((a, b) => b.updated_at - a.updated_at)[0];
      if (newest) {
        await neosh.swarm.mirror(node, newest.session, { view })
          .catch((e: unknown) => neosh.notify(String(e).replace(/^Error:\s*/, ""), "warn"));
        return;
      }
      // Up, and the inventory that says what it has follows the link by a moment.
      if (Date.now() - upSince > INVENTORY_MS) {
        neosh.notify(`${n.info.name} has no conversations yet — ^N starts one there`);
        return;
      }
    }
    if (Date.now() - started > REACH_MS) {
      neosh.notify(`could not reach ${n.info.name} — ${state(n)}`, "warn");
      return;
    }
    await changeOr(neosh, 500);
  }
}

/** Until the swarm next says something, or `ms` pass. */
function changeOr(neosh: Neosh, ms: number): Promise<void> {
  return new Promise((done) => {
    let finished = false;
    const finish = () => {
      if (finished) return;
      finished = true;
      sub.dispose();
      timer.dispose();
      done();
    };
    const sub = neosh.swarm.onChange(finish);
    const timer = neosh.timer.after(ms, finish);
  });
}
