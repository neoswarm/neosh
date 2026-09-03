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
  Disposable,
  ModelInfo,
  CredentialSource,
  ModelEntry,
  ModelSelection,
  ModelTier,
  Neosh,
  OptionSelection,
  PluginContext,
  ProviderOptionDescriptor,
} from "@neosh/api";
import type { PaneItem, RailItem } from "@neosh/api/ui";
import { confirmDestructive, defineHighlights, picker, prompt, railPicker } from "@neosh/api/ui";
import { installOptions, openOptions } from "./options.ts";

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
  await neosh.cmd.register("model.options", () => openOptions(neosh), {
    desc: "Reasoning effort, and everything else this model can be told",
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
  // The rungs have no default key. They had `⌥↑` and `⌥↓`, which read well and are not keys a
  // terminal can be relied on to send: a Mac terminal turns Option-arrow into a word motion or an
  // escape sequence depending on a setting its owner has probably never opened, and a keyboard
  // whose arrows live on a layer cannot hold Alt and reach them at all. Chat mode has no chord
  // left to move them to, and inventing a two-key prefix for two rungs is a worse trade than the
  // one row of `^P` they are already on.
  //
  // They stay ordinary commands, which is the part that matters: `^K` runs them by name, and one
  // line in `init.ts` puts them on whichever key your keyboard actually has.
  //
  //     await neosh.keymap.set("chat", "<A-Up>", "model.upgrade");

  // Deliberately *not* on the shortcut row. The key lives beside the model name in the footer,
  // where the thing it changes is already being read — saying it twice costs a row and teaches
  // nothing the first place did not.

  // The footer. The model and its options belong beside the composer rather than on a settings
  // page: they are the two things you change mid-conversation — and each carries its own key,
  // because a key is only memorable next to the thing it changes.
  /**
   * `flash` lights the options segment for a moment before settling.
   *
   * The "it moved there" flash the palette reserves reverse for, and it is here rather than in the
   * panel that was just dismissed for two reasons: it is where the setting now lives, which is the
   * thing worth teaching, and a panel held open to be looked at after the key that closed it is a
   * panel still swallowing the next key.
   */
  const footer = async (flash = false) => {
    const selection = await neosh.agent.selection().catch(() => null);
    if (!selection) {
      await neosh.status.set("model", {
        text: "no model",
        keys: "^P",
        hl: "Diagnostic.Warn",
        align: "right",
        priority: 10,
      });
      await neosh.status.clear("effort");
      return;
    }
    // Whether the next turn can authenticate belongs beside the model name, not in the error it
    // would otherwise become. "opus · no key" is something you fix before you have typed the
    // question; a failure on send is something you fix after losing your train of thought.
    //
    // Together with the catalogue, because both are wanted before the strip is written and
    // neither depends on the other — asked one after the other, the model name appeared a round
    // trip later than it had to for the sake of a shorter spelling of itself.
    const [creds, entries] = await Promise.all([
      neosh.agent.credentials().catch(() => []),
      neosh.agent.listModels(selection.instance).catch(() => [] as ModelEntry[]),
    ]);
    const cred = creds.find((c) => c.instance === selection.instance);
    const unusable = cred?.source.kind === "missing";
    const info = entries.find((e) => e.model.id === selection.model)?.model;
    // Right-hand end, where pi puts it: the left of the strip is what the conversation is
    // spending, which changes constantly, and the right is what it is spending it on, which does
    // not. Two things that move at different rates should not be interleaved.
    //
    // `short` is the id with the vendor's half of it off — `claude-opus-4-5` is `opus-4-5`, and
    // `anthropic/claude-opus-5` is `opus-5`. This is the widest thing on the strip and it used to
    // cost its full width or nothing, which on a narrow terminal meant the segments after it went
    // instead. Which vendor you are on is the part of that string you already knew.
    const brief = withoutVendor(selection.model, info?.family ?? undefined);
    await neosh.status.set("model", {
      text: `${selection.model}${unusable ? "  no key" : ""}`,
      short: brief ? `${brief}${unusable ? "  no key" : ""}` : undefined,
      keys: "^P",
      hl: unusable ? "Diagnostic.Warn" : "Accent",
      align: "right",
      priority: 10,
    });

    // Its own segment, with its own key. Glued onto the model name they read as one word, and the
    // one you want to change is whichever you are not looking at.
    const descriptors = info?.capabilities?.option_descriptors ?? [];
    const options = summarise(selection.options ?? [], descriptors);
    if (options) {
      await neosh.status.set("effort", {
        text: options,
        keys: "^E",
        // A setting that is not a level — one bought by saying a word rather than by sending a
        // parameter — wears the group that animates, so the strip itself says the next turn is not
        // an ordinary one. It is the only place that *can* say it: the word never appears in the
        // transcript, and the picker it was chosen in has been shut for an hour.
        hl: flash
          ? "Option.Chosen"
          : beyond(selection.options ?? [], descriptors)
          ? "Option.Beyond"
          : undefined,
        align: "right",
        priority: 11,
      });
      // Long enough to be seen, short enough that nobody waits for it. Settling is an ordinary
      // redraw, so whatever the strip should say — including a rainbow, if what you chose is a word
      // rather than a level — is said by the same code that would have said it anyway.
      if (flash) flashOff?.dispose();
      if (flash) flashOff = neosh.timer.after(320, () => void footer());
    } else {
      await neosh.status.clear("effort");
    }
  };
  await footer();
  // The sheet's own keys, bound to this plugin's buffer kind rather than handled privately — so
  // `^Z` lists them and `init.ts` can move any of them. See `options.ts`.
  await installOptions({ neosh, subscriptions }, (flash) => footer(flash));
  subscriptions.push(neosh.session.onChange(() => void footer()));
  subscriptions.push(neosh.agent.onTurnEnd(() => void footer()));
  // Whoever changed it — this plugin, a stored model that would not authenticate, or a provider
  // registering late and finally serving the model somebody asked for in their config.
  subscriptions.push(neosh.agent.onSelectionChange(() => void footer()));
  refreshFooter = footer;
}

/** Set by `activate`, so the pickers can update the footer the moment a choice lands. */
let refreshFooter: () => Promise<void> = async () => {};

/** The pending "settle back to normal" after a flash, so two in a row do not fight. */
let flashOff: Disposable | null = null;

/**
 * The option values, as a line short enough to sit in a footer.
 *
 * A boolean that is off is not shown at all — `fast_mode false` says nothing you did not already
 * assume, and putting the word "false" in a status strip beside a model name reads as an error.
 * One that is on is shown by name, because that *is* the news. Selects show their value, since a
 * select always has one and which one it is always matters.
 */
function summarise(
  chosen: readonly OptionSelection[],
  descriptors: readonly ProviderOptionDescriptor[],
): string {
  const out: string[] = [];
  for (const d of descriptors) {
    const value = chosen.find((o) => o.id === d.id)?.value;
    if (d.type === "boolean") {
      if (value === true) out.push(d.label.toLowerCase());
    } else if (typeof value === "string") {
      out.push(d.options.find((o) => o.id === value)?.label ?? value);
    }
  }
  return out.join(" ");
}

/**
 * Whether anything in effect is applied by *saying* it rather than by sending it.
 *
 * Asked of the descriptors rather than of the value's name, because which words a vendor reads is
 * the vendor's business: this file has never heard of "ultrathink" and does not need to.
 */
function beyond(
  chosen: readonly OptionSelection[],
  descriptors: readonly ProviderOptionDescriptor[],
): boolean {
  return descriptors.some((d) => {
    const value = chosen.find((o) => o.id === d.id)?.value;
    return d.type === "boolean"
      ? value === true && typeof d.prompt_injected_word === "string"
      : typeof value === "string" && (d.prompt_injected_values ?? []).includes(value);
  });
}

/**
 * The model id with the vendor's half of it taken off — `claude-opus-4-5` becomes `opus-4-5`.
 *
 * The strip's short form for the model segment. Cut at the *family*, which the catalogue already
 * says (`"opus"`, `"sonnet"`, `"gpt-5"`), rather than at a prefix this file guesses at: a list of
 * vendor spellings to strip is a list that is wrong about the next provider somebody registers,
 * and the whole point of the catalogue is that a plugin's models arrive described.
 *
 * Nothing when there is no shorter honest version of the name — no family, a family that is not in
 * the id, or an id that is already only its family. The segment is then dropped whole if it comes
 * to that, which is the right answer: half a model id names a different model.
 */
function withoutVendor(id: string, family?: string): string | undefined {
  // A namespaced id is a vendor and a name, and the vendor is the part being dropped either way.
  const bare = id.slice(id.lastIndexOf("/") + 1);
  if (!family) return bare === id ? undefined : bare;
  const at = bare.indexOf(family);
  if (at <= 0) return bare === id ? undefined : bare;
  const cut = bare.slice(at);
  return cut === id ? undefined : cut;
}

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
    // Already a whole sentence, and deliberately: what a retired plan needs said is the date, the
    // accounts it still serves and where to go instead, and none of that can be assembled here.
    case "plan_retired":
      return source.note;
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
        // Selectable even when its driver is missing. Looking at what a provider offers is how you
        // decide whether to install it, and a row you cannot land on is a question with nowhere to
        // ask it. What cannot be chosen is a *model* on it, which is where the refusal belongs.
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
    railLabel: "providers",
    paneLabel: "models",
    // Chords, not bare letters. Bare letters were nicer to read and made it impossible to type
    // "sonnet" into the filter: the `s` signed you in instead. Anything the picker takes for
    // itself is a letter the filter can never contain.
    ownKeys: ["<C-s>", "<C-r>", "<C-a>", "<C-d>"],
    hints: "↵ use   ^S sign in   ^R refresh   ^A add   ^D remove   esc close",
    placeholder: "nothing here yet — `s` to sign in, or `n` to name a model yourself",
    async items(c) {
      // Listed even when the driver is missing. What a provider offers is how you decide whether
      // installing its CLI is worth the afternoon, and an empty pane answers that with silence.
      const entries = await neosh.agent.listModels(c.instance).catch(() => [] as ModelEntry[]);
      // Why nothing here can be chosen, at the top, where it will be read before the list it is
      // about. `⨯` on the rail says *that* it is unavailable; this says what to do.
      //
      // A retired plan gets the same row and keeps its models selectable, which is the one place
      // the two states differ. A missing CLI means nothing below can run; a plan the vendor
      // withdrew from one tier still runs for the tiers it did not, and we cannot tell from here
      // which one is reading. Disabling the list would be this program guessing about somebody
      // else's billing and taking their working setup away on the guess.
      const retired = c.source.kind === "plan_retired";
      const blocked: PaneItem<ModelEntry>[] = c.driver_available && !retired
        ? []
        : [{
            label: retired ? "no longer served" : "unavailable",
            detail: describe(c.source),
            disabled: true,
            value: undefined as unknown as ModelEntry,
          }];
      // Anything the user named themselves goes next: they typed it because the list did not have
      // it, and burying it under the list that did not have it would be a poor joke.
      const named: PaneItem<ModelEntry>[] = (await custom(neosh, c.instance)).map((m) => ({
        label: m.display_name,
        badge: { text: "yours", hl: "Comment" },
        disabled: !c.driver_available,
        detail: m.id,
        keywords: m.id,
        value: { instance: c.instance, model: m } as ModelEntry,
      }));
      return blocked.concat(named).concat(entries.map((e) => ({
        label: e.model.display_name,
        badge: e.model.tier ? { text: tierLabel(e.model.tier), hl: tierHl(e.model.tier) } : undefined,
        detail: describeModel(e, showPricing),
        disabled: !c.driver_available,
        keywords: `${e.model.id} ${e.model.family ?? ""}`,
        // Last year's models are reachable and out of the way. You go looking for one to
        // reproduce something, which is exactly when hiding it outright would be worst.
        section: e.model.legacy ? "superseded" : undefined,
        value: e,
      })));
    },
    // Guarded, because not every row carries a model: the "why this is unavailable" row has no
    // value at all, and reaching through it is how a picker crashes on open.
    itemAt: (items) =>
      Math.max(0, items.findIndex((i) => i.value?.model?.id === current?.model)),
    async onKey(key, ctx) {
      if (key.key.code.kind !== "char" || !key.key.mods.ctrl || key.key.mods.alt) return;
      const c = ctx.rail;
      const pressed = key.key.code.c.toLowerCase();
      if (pressed === "s") {
        if (!c) return "handled";
        // Where the rest of a retired plan's sentence is said. The row in the pane is clipped to
        // whatever the detail column has room for, and this key — which the hint row already
        // advertises — is the one thing you would press on a provider that will not answer.
        if (c.source.kind === "plan_retired") {
          neosh.notify(`${c.display_name}: ${c.source.note}`, "warn");
          return "handled";
        }
        if (!c.accepts_key) {
          neosh.notify(`${c.display_name}: ${describe(c.source)}`, "info");
          return "handled";
        }
        return (await signIn(neosh, c.instance, c)) ? "reload" : "handled";
      }
      // Ask the endpoint again. Discovery is cached for the session, which is right — it is a
      // network round trip per provider — and wrong the moment you add a key, get given access to
      // a new model, or start the local server the list was empty because of.
      if (pressed === "r") {
        if (!c) return "handled";
        neosh.notify(`${c.display_name}: asking again…`);
        await neosh.agent.listModels(c.instance, { refresh: true }).catch(() => []);
        return "reload";
      }
      // A model the catalogue has not heard of. Vendors ship faster than any list is updated, and
      // "I know the id, let me type it" is the difference between waiting for a release and not.
      if (pressed === "a") {
        if (!c) return "handled";
        return (await addModel(neosh, c)) ? "reload" : "handled";
      }
      // And out again. Anything you can add you must be able to take back — a typo in an id is the
      // most likely thing to be in this list, and a wrong entry that cannot be removed is a row
      // you have to read past forever.
      if (pressed === "d") {
        const id = ctx.item?.model?.id;
        if (!c || !id) return "handled";
        if (!(await custom(neosh, c.instance)).some((m) => m.id === id)) {
          neosh.notify(
            `${id} is in ${c.display_name}'s own catalogue — there is nothing of yours to remove`,
            "info",
          );
          return "handled";
        }
        return (await removeModel(neosh, c.instance, id)) ? "reload" : "handled";
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
  // A plan whose CLI is not installed is not broken — it is one `brew install` away, and the
  // detail line says which. `⨯` is for a provider nothing can serve at all.
  if (!c.driver_available) {
    const fixable = c.source.kind === "plan_missing";
    return { text: fixable ? "!" : glyphs.ascii ? "-" : "⨯", hl: fixable ? "Account.Missing" : "Comment" };
  }
  switch (c.source.kind) {
    case "missing":
    case "plan_missing":
      return { text: "!", hl: "Account.Missing" };
    // Not `!`, which means "you can fix this". Nobody here can: the vendor stopped serving it, and
    // the only thing to do about it is read the row.
    case "plan_retired":
      return { text: glyphs.ascii ? "-" : "⨯", hl: "Comment" };
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
 * Models the user named themselves, for one provider.
 *
 * Kept in plugin state rather than in configuration, because it is a note about *this machine's*
 * access — "my org has early access to this id" — not a decision worth committing to a repository.
 * Stored per instance so the same id can mean different things on two endpoints.
 */
async function custom(neosh: Neosh, instance: string): Promise<ModelInfo[]> {
  const all = await neosh.state.get<Record<string, ModelInfo[]>>("custom").catch(() => null);
  return all?.[instance] ?? [];
}

/**
 * Add a model the catalogue has never heard of.
 *
 * Two questions, because the id and the name are different things: the id is what goes on the wire
 * and has to be exact, and the name is what you will read in a list forever afterwards. Defaulting
 * the name to the id means answering the second one is optional.
 *
 * Nothing validates the id — nothing here *can*. Whether an endpoint serves it is a question only
 * the endpoint can answer, and it answers it on the first turn.
 */
async function addModel(neosh: Neosh, c: CredentialInfo): Promise<boolean> {
  const id = await prompt(neosh, `Model id on ${c.display_name}`, { width: 60 });
  if (!id || !id.trim()) return false;
  const name = await prompt(neosh, "Show it as", { initial: id.trim(), width: 60 });

  const all =
    (await neosh.state.get<Record<string, ModelInfo[]>>("custom").catch(() => null)) ?? {};
  const mine = (all[c.instance] ?? []).filter((m) => m.id !== id.trim());
  mine.unshift({
    id: id.trim(),
    display_name: (name ?? id).trim() || id.trim(),
    capabilities: {
      tools: true,
      vision: false,
      streaming: true,
      thinking: false,
      prompt_caching: false,
      option_descriptors: [],
    },
    legacy: false,
  } as ModelInfo);
  all[c.instance] = mine;
  await neosh.state.set("custom", all);
  neosh.notify(`added ${c.instance}/${id.trim()}`);
  return true;
}

/**
 * Take back a model you named yourself.
 *
 * Only ever removes from your own list. A model the provider serves is not yours to delete, and
 * the refusal says which of the two this row is rather than silently doing nothing.
 */
async function removeModel(neosh: Neosh, instance: string, id: string): Promise<boolean> {
  const current = await neosh.agent.selection().catch(() => null);
  const inUse = current?.instance === instance && current.model === id;
  const ok = await confirmDestructive(neosh, `Remove ${id} from ${instance}?`, {
    yes: "Remove",
    no: "Keep",
    detail: [
      "You added this one by hand, so nothing else will offer it back.",
      ...(inUse ? ["It is the model in use — you will have to pick another before sending."] : []),
    ],
  });
  if (!ok) return false;

  const all =
    (await neosh.state.get<Record<string, ModelInfo[]>>("custom").catch(() => null)) ?? {};
  const mine = (all[instance] ?? []).filter((m) => m.id !== id);
  if (mine.length === 0) delete all[instance];
  else all[instance] = mine;
  await neosh.state.set("custom", all);
  neosh.notify(`removed ${instance}/${id}`);

  // If it was the one in use, the conversation is now pointed at a model nothing lists. Said again
  // here rather than only in the dialog: the dialog is gone by the time it matters.
  if (inUse) {
    neosh.notify(`${id} was the model in use — pick another before sending`, "warn");
  }
  return true;
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
  // Before the `accepts_key` branch below, which would say the same sentence at `info`. This is
  // not an ordinary fact about a working provider — it is the reason the one you just chose will
  // not answer — and the two read very differently going past.
  if (chosen.source.kind === "plan_retired") {
    neosh.notify(`${chosen.display_name}: ${chosen.source.note}`, "warn");
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
    detail: [
      "It goes from the keychain too, so this machine will not have it again until you paste it.",
      `${chosen.display_name} will stop offering models until you sign in.`,
    ],
  });
  if (!ok) return;
  await neosh.agent.forgetCredential(chosen.instance);
  await refreshFooter();
}

