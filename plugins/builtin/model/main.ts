/**
 * The model switcher, and the reasoning-effort switcher next to it.
 *
 * Both are the same thing twice: a picker over what the *driver* said it supports. The effort
 * levels are not hard-coded here — they arrive as `ProviderOptionDescriptor`s attached to the
 * chosen model, so a provider plugin that invents a new knob gets a working picker for it with no
 * change to this file. That is the whole reason that descriptor type exists.
 */

import type {
  AccountKind,
  CredentialInfo,
  CredentialSource,
  ModelEntry,
  ModelSelection,
  ModelTier,
  Neosh,
  OptionSelection,
  PluginContext,
  ProviderOptionDescriptor,
} from "@neosh/api";
import type { RailItem } from "@neosh/api/ui";
import { confirmDestructive, defineHighlights, picker, railPicker } from "@neosh/api/ui";

type SelectDescriptor = Extract<ProviderOptionDescriptor, { type: "select" }>;

export async function activate({ neosh, subscriptions }: PluginContext) {
  await defineHighlights(neosh);

  await neosh.opt.declare({
    name: "model.picker.show_pricing",
    type: { type: "bool" },
    default: true,
    description: "Show cost per million tokens beside each model, where the provider reports it.",
  });

  await neosh.cmd.register("model.pick", () => pickModel(neosh), {
    desc: "Choose the model for this conversation",
  });
  await neosh.cmd.register("model.options", () => pickOptions(neosh), {
    desc: "Choose reasoning effort and other per-model options",
  });
  await neosh.cmd.register("provider.auth", () => pickProvider(neosh), {
    desc: "Sign in to a provider, or forget a key",
  });
  await neosh.cmd.register("model.upgrade", () => step(neosh, 1), {
    desc: "Move up one rung — to the more capable model this provider serves",
  });
  await neosh.cmd.register("model.downgrade", () => step(neosh, -1), {
    desc: "Move down one rung — cheaper and quicker",
  });
  await neosh.cmd.register("model.line", (args) => useLine(neosh, args[0]), {
    desc: "Switch to the current model in a line — `model.line opus`",
  });

  await neosh.keymap.set("chat", "<C-p>", "model.pick", { desc: "Pick a model" });
  await neosh.keymap.set("chat", "<C-e>", "model.options", { desc: "Pick reasoning effort" });
  // Alt-arrows because they are unclaimed in every mode and mean "along an axis" in most editors.
  // Rebind like anything else: every one of these is an ordinary command in the same table your
  // own bindings live in.
  await neosh.keymap.set("chat", "<A-Up>", "model.upgrade", { desc: "A more capable model" });
  await neosh.keymap.set("chat", "<A-Down>", "model.downgrade", { desc: "A cheaper model" });

  // The footer. The model and its options belong beside the composer rather than on a settings
  // page: they are the two things you change mid-conversation.
  const footer = async () => {
    const selection = await neosh.agent.selection().catch(() => null);
    if (!selection) {
      await neosh.status.set("model", { text: "no model", hl: "Diagnostic.Warn", priority: 10 });
      return;
    }
    const options = (selection.options ?? []).map((o) => String(o.value)).join(" ");
    // Whether the next turn can authenticate belongs beside the model name, not in the error it
    // would otherwise become. "opus · no key" is something you fix before you have typed the
    // question; a failure on send is something you fix after losing your train of thought.
    const cred = (await neosh.agent.credentials().catch(() => []))
      .find((c) => c.instance === selection.instance);
    const unusable = cred?.source.kind === "missing";
    await neosh.status.set("model", {
      text: `${selection.model}${options ? ` ${options}` : ""}${unusable ? "  no key" : ""}`,
      hl: unusable ? "Diagnostic.Warn" : "Accent",
      priority: 10,
    });
  };
  await footer();
  subscriptions.push(neosh.session.onChange(() => void footer()));
  subscriptions.push(neosh.agent.onTurnEnd(() => void footer()));
  subscriptions.push(
    neosh.opt.onChange((e) => {
      if (e.name === "agent.model") void footer();
    }),
  );
  // Set after every switch too, since `setSelection` does not fire an option change.
  refreshFooter = footer;
}

/** Set by `activate`, so the pickers can update the footer the moment a choice lands. */
let refreshFooter: () => Promise<void> = async () => {};

/**
 * How an instance's key situation reads in a list.
 *
 * Said in words rather than shown as a state icon, because the useful distinction is not
 * "authenticated or not" — it is *which* of four things is true, and each one implies a different
 * next action.
 */
function describe(source: CredentialSource): string {
  switch (source.kind) {
    // A plan. There is no key, which is the answer to "why does it say I have no API key".
    case "plan":
      return `your ${source.via} login`;
    case "plan_missing":
      return source.hint
        ? `${source.program} is not installed — then \`${source.hint}\``
        : `${source.program} is not installed`;
    case "inherited":
      return "signs in on its own";
    // A key, and which of the four places it is coming from.
    case "env":
      return `$${source.var}`;
    case "keychain":
      return "keychain";
    case "session":
      return "this session only";
    case "command":
      return "credential helper";
    case "not_needed":
      return "no key needed";
    case "missing":
      return "needs a key";
  }
}

async function pickModel(neosh: Neosh): Promise<void> {
  const [creds, current, ascii, nerd] = await Promise.all([
    neosh.agent.credentials().catch(() => [] as CredentialInfo[]),
    neosh.agent.selection().catch(() => null),
    // Defensively: a display setting that cannot be read must not stop the picker opening. It is
    // decoration, and failing closed here would mean no way to change models at all.
    neosh.opt.get<boolean>("ui.ascii_only").catch(() => false),
    neosh.opt.get<boolean>("ui.nerd_font").catch(() => false),
  ]);
  if (creds.length === 0) {
    neosh.notify("no providers are configured", "warn");
    return;
  }
  const showPricing =
    (await neosh.opt.get<boolean>("model.picker.show_pricing").catch(() => true)) ?? true;
  const glyphs = { ascii: ascii ?? false, nerd: nerd ?? false };

  // Grouped by what a turn spends. A plan you already pay for and a key billed per token are
  // different decisions, and a flat list makes you read every row to tell which is which.
  const order: AccountKind[] = ["plan", "api_key", "local"];
  const rail: RailItem<CredentialInfo>[] = [];
  for (const account of order) {
    for (const c of creds.filter((x) => x.account === account)) {
      rail.push({
        mark: { text: markFor(c, glyphs), hl: c.brand?.hl ?? "Comment" },
        label: c.display_name,
        badge: badgeFor(c, glyphs),
        group: headingFor(account),
        // Listed, not hidden: a provider that vanished because its CLI is missing is a question
        // with nowhere to ask it.
        disabled: !c.driver_available,
        value: c,
      });
    }
  }

  const startAt = Math.max(0, rail.findIndex((r) => r.value.instance === current?.instance));

  const chosen = await railPicker<CredentialInfo, ModelEntry>(neosh, {
    title: "Model",
    width: 88,
    railWidth: 24,
    height: 14,
    rail,
    railAt: startAt,
    hints: "↵ use   ⇥ providers   s sign in   e effort   ^N/^P move   esc close",
    placeholder: "nothing here yet — `s` to sign in",
    async items(c) {
      if (!c.driver_available) return [];
      const entries = await neosh.agent.listModels(c.instance).catch(() => [] as ModelEntry[]);
      return entries.map((e) => ({
        label: e.model.display_name,
        badge: e.model.tier ? { text: tierLabel(e.model.tier), hl: tierHl(e.model.tier) } : undefined,
        detail: describeModel(e, showPricing),
        keywords: `${e.model.id} ${e.model.family ?? ""}`,
        // Last year's models are reachable and out of the way. You go looking for one to
        // reproduce something, which is exactly when hiding it outright would be worst.
        section: e.model.legacy ? "superseded" : undefined,
        value: e,
      }));
    },
    itemAt: (items) => Math.max(0, items.findIndex((i) => i.value.model.id === current?.model)),
    async onKey(key, ctx) {
      if (key.key.code.kind !== "char" || key.key.mods.ctrl || key.key.mods.alt) return;
      if (key.key.code.c === "s") {
        const c = ctx.rail;
        if (!c) return "handled";
        if (!c.accepts_key) {
          neosh.notify(`${c.display_name}: ${describe(c.source)}`, "info");
          return "handled";
        }
        return (await signIn(neosh, c.instance, c)) ? "reload" : "handled";
      }
      return undefined;
    },
  });
  if (!chosen) return;

  if (!creds.find((c) => c.instance === chosen.instance)?.source.kind.startsWith("plan")) {
    const cred = creds.find((c) => c.instance === chosen.instance);
    if (cred && cred.source.kind === "missing" && cred.accepts_key) {
      if (!(await signIn(neosh, chosen.instance, cred))) return;
    }
  }
  await use(neosh, chosen, current);
}

/**
 * Move one rung along the capability ladder, within the provider you are already using.
 *
 * The provider is held fixed on purpose. "Give me something cheaper" is a question about the model;
 * answering it by also moving you to a different endpoint — with different billing, possibly a
 * different key — would be answering a question nobody asked.
 *
 * No wrapping. At the top, `upgrade` says so rather than dropping you to the cheapest thing in the
 * catalogue, which is the kind of surprise that stops people using a key at all.
 */
async function step(neosh: Neosh, delta: number): Promise<void> {
  const current = await neosh.agent.selection().catch(() => null);
  if (!current) {
    neosh.notify("no model is selected", "warn");
    return;
  }
  const entries = await neosh.agent.listModels(current.instance).catch(() => [] as ModelEntry[]);
  const here = entries.find((e) => e.model.id === current.model);
  const from = here?.model.tier;
  if (!from) {
    neosh.notify(`${current.model} is not on the ladder — nothing to step to`, "warn");
    return;
  }
  const rungs: ModelTier[] = ["fast", "balanced", "frontier"];
  const at = rungs.indexOf(from) + delta;
  const want = rungs[at];
  if (!want) {
    neosh.notify(delta > 0 ? "already the most capable one here" : "already the quickest one here");
    return;
  }
  const next = best(entries.filter((e) => e.model.tier === want));
  if (!next) {
    neosh.notify(`${current.instance} has nothing on the ${tierLabel(want)} rung`, "warn");
    return;
  }
  await use(neosh, next, current);
}

/**
 * Switch to the current model in a named line — `model.line opus`.
 *
 * A *line* is a product across versions; the best one in it is the newest that has not been
 * superseded. That is the thing people mean by "use Opus": not a pinned id that goes stale, and not
 * a picker they have to read.
 *
 * Looks in the provider you are on first, then anywhere reachable, so this works whether your Opus
 * comes from a plan or a key.
 */
async function useLine(neosh: Neosh, line: string | undefined): Promise<void> {
  if (!line) {
    neosh.notify("model.line needs a line — try `model.line opus`", "warn");
    return;
  }
  const current = await neosh.agent.selection().catch(() => null);
  const wanted = line.trim().toLowerCase();
  const matching = (entries: ModelEntry[]) =>
    entries.filter((e) => (e.model.family ?? "").toLowerCase() === wanted);

  let found = current
    ? best(matching(await neosh.agent.listModels(current.instance).catch(() => [])))
    : undefined;
  if (!found) found = best(matching(await neosh.agent.listModels().catch(() => [])));
  if (!found) {
    neosh.notify(`nothing reachable is in the "${line}" line`, "warn");
    return;
  }
  await use(neosh, found, current);
}

/**
 * The one to use out of a set: current before superseded, then the most capable.
 *
 * Order within a rung is the catalogue's, which lists newest first — so "current" needs no version
 * parsing, which is the thing that goes wrong the moment a vendor renames something.
 */
function best(entries: ModelEntry[]): ModelEntry | undefined {
  const live = entries.filter((e) => !e.model.legacy);
  const pool = live.length > 0 ? live : entries;
  const rank = (e: ModelEntry) =>
    e.model.tier === "frontier" ? 2 : e.model.tier === "balanced" ? 1 : 0;
  return pool.reduce<ModelEntry | undefined>(
    (win, e) => (win === undefined || rank(e) > rank(win) ? e : win),
    undefined,
  );
}

/** Put a selection into effect, carrying over the option values the new model also has. */
async function use(
  neosh: Neosh,
  chosen: ModelEntry,
  current: ModelSelection | null,
): Promise<void> {
  // Switching between two thinking models should keep your effort level; switching to one without
  // the knob must drop it rather than send a value the driver will reject.
  const descriptors = chosen.model.capabilities?.option_descriptors ?? [];
  const keep = (current?.options ?? []).filter((o) => descriptors.some((d) => d.id === o.id));
  await neosh.agent.setSelection({
    instance: chosen.instance,
    model: chosen.model.id,
    options: keep,
  });
  await refreshFooter();
  neosh.notify(`model: ${chosen.instance}/${chosen.model.display_name}`);
}

function headingFor(account: AccountKind): string {
  if (account === "plan") return "PLANS";
  return account === "api_key" ? "API KEYS" : "LOCAL";
}

/**
 * The mark for a provider, at whatever fidelity this terminal has.
 *
 * Three levels rather than one, because a terminal is not one thing: a Nerd Font has a real brand
 * glyph, a bare Unicode terminal has geometry, `ui.ascii_only` has a letter. Choosing badly is
 * worse than choosing plainly — a box glyph where a logo should be reads as a broken program.
 */
function markFor(c: CredentialInfo, glyphs: { ascii: boolean; nerd: boolean }): string {
  const b = c.brand;
  if (!b) return glyphs.ascii ? "?" : "·";
  if (glyphs.ascii) return b.ascii;
  return (glyphs.nerd && b.nerd) || b.mark;
}

/** The state marker at the right edge of the rail. One column, four meanings. */
function badgeFor(
  c: CredentialInfo,
  glyphs: { ascii: boolean },
): { text: string; hl: string } | undefined {
  if (!c.driver_available) return { text: glyphs.ascii ? "-" : "⨯", hl: "Comment" };
  switch (c.source.kind) {
    case "missing":
    case "plan_missing":
      return { text: "!", hl: "Account.Missing" };
    case "not_needed":
      return undefined;
    case "plan":
      return { text: glyphs.ascii ? "*" : "✓", hl: "Account.Plan" };
    default:
      return { text: glyphs.ascii ? "+" : "✓", hl: "Account.Key" };
  }
}

/** The rung, as a word. Read down this column and you see the shape of what a provider offers. */
function tierLabel(tier: ModelTier): string {
  if (tier === "frontier") return "Frontier";
  return tier === "balanced" ? "Balanced" : "Fast";
}

function tierHl(tier: ModelTier): string {
  if (tier === "frontier") return "Accent";
  return tier === "balanced" ? "Status.Working" : "Status.Done";
}

/** What a model row says about itself, beyond its name and rung. */
function describeModel(e: ModelEntry, showPricing: boolean): string {
  const bits: string[] = [];
  if (e.model.tagline) bits.push(e.model.tagline);
  if (showPricing && e.model.pricing) {
    bits.push(`$${e.model.pricing.input_per_mtok}/$${e.model.pricing.output_per_mtok}`);
  }
  if (bits.length === 0) bits.push(String(e.model.id));
  return bits.join("  ·  ");
}

/**
 * Collect a key for one instance.
 *
 * The typing happens in the host — this call does not settle until the prompt closes, and what
 * comes back is whether a key was stored, never the key. Afterwards the endpoint is re-queried,
 * because the reason it listed nothing a moment ago was that it could not authenticate.
 */
async function signIn(
  neosh: Neosh,
  instance: string,
  cred: CredentialInfo | undefined,
): Promise<boolean> {
  if (cred && !cred.accepts_key) {
    neosh.notify(`${cred.display_name} signs in on its own — there is nowhere to put a key`, "warn");
    return false;
  }
  const stored = await neosh.agent
    .setCredential(instance, { replace: true })
    .catch((e: unknown) => {
      neosh.notify(String(e), "warn");
      return false;
    });
  if (!stored) return false;
  await neosh.agent.listModels(instance, { refresh: true }).catch(() => []);
  return true;
}

/**
 * Every configured provider and the state of its account.
 *
 * The same information the model picker's rail carries, as a page you can go to directly — for
 * replacing a key, forgetting one, or finding out why a provider you expected is not offering
 * anything.
 */
async function pickProvider(neosh: Neosh): Promise<void> {
  const rows = await neosh.agent.credentials();
  if (rows.length === 0) {
    neosh.notify("no providers are configured", "warn");
    return;
  }
  // Unavailable drivers are listed rather than hidden. The `claude` CLI not being installed is the
  // single most likely reason a subscription does not show up, and a provider that has silently
  // vanished from the list is a question with nowhere to ask it.
  const order = ["plan", "api_key", "local"] as const;
  const sorted = order.flatMap((account) =>
    rows
      .filter((c) => c.account === account)
      .sort((a, b) => Number(b.driver_available) - Number(a.driver_available)),
  );
  const chosen = await picker(
    neosh,
    sorted.map((c) => ({
      label: c.display_name,
      detail: c.driver_available
        ? `${headingFor(c.account).toLowerCase()}  ·  ${describe(c.source)}`
        : `${c.instance}  ·  its driver is not available here`,
      keywords: c.instance,
      value: c,
    })),
    { title: "Providers", width: 76 },
  );
  if (!chosen) return;

  if (!chosen.driver_available) {
    neosh.notify(
      chosen.driver === "claude-cli"
        ? "the `claude` CLI is not on your PATH — install it to use your Claude subscription"
        : `${chosen.display_name}: no plugin provides the "${chosen.driver}" driver`,
      "warn",
    );
    return;
  }
  if (chosen.source.kind === "plan_missing") {
    const { program, hint } = chosen.source;
    neosh.notify(
      hint
        ? `${program} is not installed. Install it, then run \`${hint}\`.`
        : `${program} is not installed.`,
      "warn",
    );
    return;
  }
  if (!chosen.accepts_key) {
    neosh.notify(`${chosen.display_name}: ${describe(chosen.source)}`, "info");
    return;
  }
  if (chosen.source.kind === "missing") {
    await signIn(neosh, chosen.instance, chosen);
    return;
  }

  // It already has one, so the two things you could mean are "replace it" and "get rid of it".
  const action = await picker<"replace" | "forget">(
    neosh,
    [
      { label: "Enter a new key", detail: `replaces ${describe(chosen.source)}`, value: "replace" },
      { label: "Forget the stored key", detail: "removes it from the keychain too", value: "forget" },
    ],
    { title: chosen.display_name, width: 64, height: 2 },
  );
  if (action === null) return;
  if (action === "replace") {
    await signIn(neosh, chosen.instance, chosen);
    return;
  }
  const ok = await confirmDestructive(neosh, `Forget the key for ${chosen.display_name}?`, {
    yes: "Forget",
    no: "Keep",
  });
  if (!ok) return;
  await neosh.agent.forgetCredential(chosen.instance);
  await refreshFooter();
}

async function pickOptions(neosh: Neosh): Promise<void> {
  const current = await neosh.agent.selection();
  if (!current) {
    neosh.notify("no model selected", "warn");
    return;
  }
  const entries = await neosh.agent.listModels(current.instance);
  const descriptors =
    entries.find((e) => e.model.id === current.model)?.model.capabilities?.option_descriptors ?? [];
  if (descriptors.length === 0) {
    neosh.notify(`${current.model} has no options to set`);
    return;
  }

  // One knob goes straight to its values; more than one asks which knob first. Keeping the common
  // case — reasoning effort, and nothing else — a single keystroke is worth the branch.
  const descriptor =
    descriptors.length === 1
      ? descriptors[0]
      : await picker(
          neosh,
          descriptors.map((d) => ({ label: d.label, detail: d.description ?? "", value: d })),
          { title: `${current.model} options`, width: 64 },
        );
  if (!descriptor) return;

  if (descriptor.type === "boolean") {
    const chosen = await picker<boolean>(
      neosh,
      [
        { label: "On", value: true },
        { label: "Off", value: false },
      ],
      { title: descriptor.label, height: 2, width: 40 },
    );
    if (chosen === null) return;
    await applyOption(neosh, current, descriptor.id, chosen);
    neosh.notify(`${descriptor.label}: ${chosen ? "on" : "off"}`);
    return;
  }

  const select: SelectDescriptor = descriptor;
  const currentValue =
    current.options?.find((o) => o.id === select.id)?.value ??
    select.current_value ??
    select.options.find((o) => o.is_default)?.id;

  const items = select.options.map((o) => ({
    label: o.label,
    detail: o.description ?? (o.is_default ? "default" : ""),
    value: o.id,
  }));

  const chosen = await picker(neosh, items, {
    title: select.label,
    selected: Math.max(0, items.findIndex((i) => i.value === currentValue)),
    width: 64,
  });
  if (chosen === null) return;
  await applyOption(neosh, current, select.id, chosen);
  neosh.notify(`${select.label}: ${select.options.find((o) => o.id === chosen)?.label ?? chosen}`);
}

async function applyOption(
  neosh: Neosh,
  current: ModelSelection,
  id: string,
  value: string | boolean,
): Promise<void> {
  const options: OptionSelection[] = (current.options ?? []).filter((o) => o.id !== id);
  options.push({ id, value });
  await neosh.agent.setSelection({ ...current, options });
  await refreshFooter();
}
