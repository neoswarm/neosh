/**
 * What this is costing, on three timescales.
 *
 * **Context** is this request: how full the window was on the last one, and it is the number that
 * changes what you do next — at 90% the useful move is a new conversation, and finding that out
 * from a truncation error is finding out too late.
 *
 * **Tokens** is this conversation: a running total, about cost rather than about room.
 *
 * **The plan** is your week. A rolling allowance the vendor enforces and will not itemise, which is
 * the only one of the three that can stop you working — and the only one that is *reported* rather
 * than counted. It goes at the foot of the sidebar, because unlike the other two it is not about
 * the conversation you are in: it is the same number whichever one you open, and it is the thing
 * you want to have seen before starting something long.
 *
 * They are deliberately never added together, and never drawn as one bar. A conversation that has
 * spent 400k tokens across twenty turns is not 400k tokens *long*; a percentage of an opaque
 * allowance is not a token count; and a footer that implied either would be wrong in the direction
 * that makes people plan around a number that means nothing.
 *
 * # Everything here is ordinary API
 *
 * The strip is a `sidebar.section` contribution, so it can be turned off and something else put
 * there. The panel is a buffer with a `kind`, so its keys are bindable and a plugin can add rows to
 * it. Every key is a named command, so `^Z` lists them and `init.ts` can move them. Nothing in this
 * file calls anything a third party could not.
 */

import type {
  Brand,
  CredentialInfo,
  ModelEntry,
  Neosh,
  PluginContext,
  QuotaSnapshot,
  QuotaWindow,
  SessionInfo,
  UsageBucket,
  UsageHistory,
  UsageResolution,
  ViewId,
} from "@neosh/api";
import { byteLength } from "@neosh/api";
import {
  compact,
  CursoredList,
  elapsed,
  type ListRow,
  meter,
  money,
  onTick,
} from "@neosh/api/ui";

const NS = "usage";
/** What the panel's buffer says it is. Everything a third party binds or finds hangs off this. */
const KIND = "neosh.usage";
/** Where somebody else's rows go, in the panel. */
const POINT_ROW = "usage.row";

export async function activate(ctx: PluginContext) {
  await declareOptions(ctx.neosh);
  await installFooter(ctx);
  await installStrip(ctx);
  await installPanel(ctx);
}

async function declareOptions(neosh: Neosh) {
  await neosh.opt.declare({
    name: "usage.show_tokens",
    type: { type: "bool" },
    default: true,
    description: "Show the running token total beside the context meter.",
  });
  await neosh.opt.declare({
    name: "usage.sidebar",
    type: { type: "bool" },
    default: true,
    description: "Show what the plan has left at the foot of the sidebar.",
  });
  await neosh.opt.declare({
    name: "usage.sidebar.style",
    type: { type: "str" },
    default: "one",
    description:
      "How much of the plan the sidebar spends rows on. `one` is a row per account — the limit " +
      "that would refuse the next request, plus any other that is already critical. `windows` is " +
      "a row per limit. `full` adds the account row and the sentence under it. `<Tab>` on a plan " +
      "row steps through them, and `^L` is the whole of it whichever is set.",
  });
  await neosh.opt.declare({
    name: "usage.poll",
    type: { type: "bool" },
    default: true,
    description:
      "Ask the provider what the plan has left while nothing is running. Reads the vendor CLI's " +
      "own login to do it; off, the gauges show what the last turn reported and say how old that is.",
  });
  await neosh.opt.declare({
    name: "usage.warn_at",
    type: { type: "int" },
    default: 90,
    description: "Say something the first time an allowance passes this percentage. 0 to never.",
  });
  await neosh.opt.declare({
    name: "usage.days",
    type: { type: "int" },
    default: 30,
    description: "How many days of history the usage panel opens on.",
  });
}

/* -------------------------------------------------------------------------- */
/* The strip at the foot of the sidebar                                       */
/* -------------------------------------------------------------------------- */

/**
 * What the plan has left, as rows somebody else draws.
 *
 * Contributed rather than drawn, which is what makes it removable: `usage.sidebar = false` takes it
 * out, a sidebar of your own that never reads `sidebar.section` never shows it, and a plugin that
 * wants these numbers somewhere else calls `quota.list()` and draws its own.
 *
 * One row per window, because there is usually more than one and they say different things: a
 * session limit that is nearly full comes back in an hour, and a weekly one that is nearly full
 * does not. Collapsing them to "the worst" would hide exactly that difference.
 */
async function installStrip({ neosh, subscriptions }: PluginContext) {
  /** What was last contributed, so an unchanged redraw is not a round trip. */
  let drawn = "";
  /** Windows already complained about, keyed by instance and window. Cleared when one resets. */
  const warned = new Set<string>();
  /**
   * Who each instance is, for the account row.
   *
   * Cached because this runs on a tick and the answer changes when somebody signs in, not several
   * times a second. Refreshed on the events that can change it rather than polled.
   */
  let who: CredentialInfo[] = [];
  const reload = async () => {
    who = await neosh.agent.credentials().catch(() => [] as CredentialInfo[]);
  };

  const refresh = async () => {
    const on = (await neosh.opt.get<boolean>("usage.sidebar").catch(() => true)) ?? true;
    if (!on) {
      if (drawn !== "") {
        drawn = "";
        await neosh.ext.remove("sidebar.section", "plan").catch(() => {});
      }
      return;
    }
    const [quotas, ascii, nerd, width, style] = await Promise.all([
      neosh.quota.list().catch(() => [] as QuotaSnapshot[]),
      neosh.opt.get<boolean>("ui.ascii_only").catch(() => false),
      neosh.opt.get<boolean>("ui.nerd_font").catch(() => false),
      // The column this has to fit in. Read rather than assumed: the sidebar clips a contributed
      // row to its own width and then draws `right` over the top of whatever survived, so a row
      // built to a guessed width does not wrap — it collides, and the countdown lands on the last
      // letter of the label.
      neosh.opt.get<number>("sidebar.width").catch(() => 34),
      neosh.opt.get<string>("usage.sidebar.style").catch(() => "one"),
    ]);
    // An account we cannot name yet. Reaching a quota for an instance that is not in the credential
    // list means the list is stale — a provider registered late, or somebody has just signed in —
    // and drawing `claude-cli` where `Claude` belongs for the rest of the session is the failure
    // this avoids. Once per staleness, not once per frame: `who` is replaced before the next one.
    if (quotas.some((q) => !who.some((c) => c.instance === q.instance))) await reload();
    const glyphs = { ascii: ascii ?? false, nerd: nerd ?? false };
    const item = section(quotas, who, glyphs, width ?? 34, Date.now() / 1000, styleOf(style));
    // Compared as JSON rather than by identity: this runs on a tick, and re-contributing the same
    // rows makes the sidebar rebuild its whole list — cursor, scroll and all — several times a
    // second, for no change anybody can see.
    const key = JSON.stringify(item);
    if (key === drawn) return;
    drawn = key;
    if (!item) {
      await neosh.ext.remove("sidebar.section", "plan").catch(() => {});
      return;
    }
    await neosh.ext.contribute("sidebar.section", "plan", item, { priority: -10 });
  };

  // Stepping the density, and taking the strip off the column altogether. Commands rather than
  // config-file lines: `^K` runs them, `^Z` lists the key, and `init.ts` moves it — the same way
  // every other verb here works. They write the *setting*, so `config.toml` decides where you
  // start and the key decides where you are.
  subscriptions.push(
    await neosh.cmd.register(`${NS}.sidebar.cycle`, async () => {
      const now = styleOf(await neosh.opt.get<string>("usage.sidebar.style").catch(() => "one"));
      const next = STYLES[(STYLES.indexOf(now) + 1) % STYLES.length] ?? "one";
      await neosh.opt.set("usage.sidebar.style", next);
    }, { desc: "How much of the plan the sidebar shows" }),
  );
  subscriptions.push(
    await neosh.cmd.register(`${NS}.sidebar.toggle`, async () => {
      const on = (await neosh.opt.get<boolean>("usage.sidebar").catch(() => true)) ?? true;
      await neosh.opt.set("usage.sidebar", !on);
    }, { desc: "Show or hide the plan at the foot of the sidebar" }),
  );
  // And a key on the rows themselves. `<Tab>` because that is what opens and folds a thing you are
  // standing on everywhere else in neosh, and `on: "custom"` so it is a verb about *these* rows
  // rather than one advertised over every conversation in the panel.
  await neosh.ext.contribute("sidebar.action", "plan.detail", {
    key: "<Tab>",
    label: "plan detail",
    command: `${NS}.sidebar.cycle`,
    on: "custom",
  }).catch(() => {});
  subscriptions.push({
    dispose: () => void neosh.ext.remove("sidebar.action", "plan.detail").catch(() => {}),
  });

  await reload();
  await refresh();
  subscriptions.push(neosh.quota.onChange((s) => {
    void refresh();
    void announce(neosh, s, warned);
  }));
  subscriptions.push(neosh.opt.onChange((e) => {
    if (
      e.name === "usage.sidebar" || e.name === "usage.sidebar.style" ||
      e.name === "ui.ascii_only" || e.name === "ui.nerd_font" || e.name === "sidebar.width"
    ) void refresh();
  }));
  // A selection change is the cheap signal that the provider list may have moved — signing in is
  // what puts a name and a mark on an account that had neither, and it is usually followed by
  // choosing something on it. `refresh` also reloads on its own when a quota turns up for an
  // instance it cannot name, so a sign-in nobody selected after is caught within a tick either way.
  subscriptions.push(neosh.agent.onSelectionChange(() => void reload().then(refresh)));
  // A countdown is the only thing here that moves without anything happening, and it moves once a
  // minute. The tick is already running for the working line; `refresh` bails on an unchanged
  // render, so this costs a string compare.
  subscriptions.push(onTick(() => void refresh()));
}

/** A contributed sidebar row, in the shape `sidebar.section` takes. */
interface StripRow {
  text: string;
  hl?: string;
  /** Pieces of the row in their own group, over the top of `hl`: the provider's mark. */
  spans?: Array<{ from: number; to: number; hl: string }>;
  right?: { text: string; hl?: string };
  command?: string;
}

/**
 * What the sidebar is handed.
 *
 * `null` when there is nothing to say — no rows rather than a heading over an empty block, because
 * a section that is always there and usually empty is a section people stop looking at.
 *
 * # Shape
 *
 * An account, then its limits, then one sentence. The account row is the part that used to be
 * missing entirely: three bars and no name is a column that answers "how much is left" without
 * ever saying *of what*, and on a machine with both a Claude plan and a Codex one it was two sets
 * of identical-looking bars stacked on top of each other. It carries the same mark and the same
 * brand colour the model picker uses, so the thing you chose in `^P` and the thing being spent
 * here are visibly one thing.
 *
 * The sentence is the other half. A bar that fills as an allowance is *spent* and a countdown with
 * no verb on it are both readable and neither is unambiguous — `12%` and `3h` do not say which way
 * round they go. One line in words, about the limit that actually binds, says both: how much is
 * left, and when it comes back.
 */
/**
 * How much of the column the plan is allowed to be.
 *
 * A plan with three limits and a name and a sentence under it is five rows, which is most of a
 * short sidebar spent on a number that changes twice an hour. `one` is the answer to the question
 * people actually have — *may I start something long* — and the rest is a keypress away.
 *
 * Ordered, because `<Tab>` steps along them.
 */
const STYLES = ["one", "windows", "full"] as const;
type Style = (typeof STYLES)[number];

export function styleOf(value: unknown): Style {
  return STYLES.includes(value as Style) ? value as Style : "one";
}

/**
 * The limits worth a row when there is only room for the answer.
 *
 * The one that binds, because it is the one that would refuse the next request — and anything else
 * already critical, because a weekly cap at 94% is news whether or not the session limit is the one
 * about to stop you. Never more than that: this is the summary, and a summary that grows back to
 * the full list is not one.
 */
function summarised(q: QuotaSnapshot): QuotaWindow[] {
  const first = q.windows.find((w) => w.active) ?? binding(q.windows);
  const out = first ? [first] : [];
  for (const w of q.windows) {
    if (w === first) continue;
    if (w.severity === "critical" || w.severity === "exhausted") out.push(w);
  }
  return out;
}

function section(
  quotas: QuotaSnapshot[],
  who: CredentialInfo[],
  glyphs: { ascii: boolean; nerd: boolean },
  width: number,
  now: number,
  style: Style,
) {
  const rows: StripRow[] = [];
  // What the sidebar leaves for the text (its one-column margin at each edge), less the widest
  // countdown and a space to keep it off the label.
  const room = Math.max(14, width - 2 - 5);
  const accounts = quotas.filter((q) => q.windows.length > 0);

  for (const q of accounts) {
    // Air between accounts. Two of them run together into one block of bars otherwise, and the
    // second account's name reads as another limit belonging to the first. Not in `one`, where an
    // account *is* a row and a blank line between two of them costs more than it separates.
    if (rows.length > 0 && style !== "one") rows.push({ text: "" });
    // Whose allowance this is. Always in `full`; in `windows` only when there is more than one
    // account, since with a single one it is a row that says what the mark on the bar already
    // says; never in `one`, where an account is a row.
    if (style === "full" || (style === "windows" && accounts.length > 1)) {
      rows.push(accountRow(q, who, glyphs, room));
    }
    const windows = style === "one" ? summarised(q) : q.windows;
    const cred = who.find((c) => c.instance === q.instance);
    // The mark goes on the bar itself when there is no account row above it to carry it: a row of
    // brand colour with no brand on it is a coloured bar nobody can attribute.
    const brand = style === "one" ? `${markFor(cred?.brand, glyphs)} ` : "";
    // A `▸` says which limit binds, and is worth two columns only when there is more than one
    // limit on screen to distinguish it from.
    const marker = windows.length > 1 ? MARKER : 0;
    // Widest label first, so every bar in an account starts in the same column — a bar whose left
    // edge moves from row to row cannot be compared with the one above it, which is the only thing
    // a stack of bars is for. In `one` the binding row is labelled by the *account* rather than the
    // window, because `Session` with no account row above it says whose session to nobody, and the
    // name is what two of these rows are told apart by. The rows a critical window adds keep their
    // own labels — they sit under a named one.
    const labels = windows.map((w, i) =>
      style === "one" && i === 0
        ? clip(cred?.display_name || q.instance, Math.max(4, room - cells(brand)))
        : compactLabel(w, room - cells(brand)),
    );
    const labelWidth = Math.min(
      Math.max(...labels.map(cells), 0),
      Math.max(6, room - cells(brand) - marker - BAR_MIN - PCT - 1),
    );
    const bar = Math.max(
      BAR_MIN,
      Math.min(10, room - cells(brand) - marker - labelWidth - PCT - 1),
    );
    // The mark keeps its own colour. The row is graded by severity — that is what a bar is for —
    // but the one thing on it that says *whose* allowance this is has a brand colour of its own,
    // and Anthropic's orange on the `✳` is how you know which of two accounts you are looking at
    // without reading a word. The theme owns the colour: `Brand.Anthropic` is a group, never a
    // value, so a palette that wants a different orange changes it in one place.
    const mark = brand === ""
      ? undefined
      : [{ from: 0, to: byteLength(brand.trimEnd()), hl: cred?.brand?.hl ?? "Sidebar.Heading" }];
    for (const [i, w] of windows.entries()) {
      rows.push({
        text: brand + windowRow(w, labels[i] ?? w.label, labelWidth, bar, glyphs.ascii, marker > 0),
        hl: severityGroup(w),
        spans: mark,
        right: { text: countdown(w, now), hl: "Sidebar.Dim" },
        // Every row opens the panel, so there is no row here you can land on and press `↵` on to
        // no effect — which is the thing that teaches people the column is not interactive.
        command: `${NS}.panel`,
      });
    }
    // The sentence under the bars, and the credit line: both are explanations, and an explanation
    // is the first thing to give up when the column is being asked to say less.
    if (style === "full") {
      const said = plainly(q, now, room);
      if (said) rows.push({ ...said, command: `${NS}.panel` });
    }
    if (style !== "one") {
      const credit = creditRow(q);
      if (credit) rows.push({ ...credit, command: `${NS}.panel` });
    }
  }
  if (rows.length === 0) return null;
  // The way in, on the heading. Every row here opens the panel too, but a key you can press from
  // the composer without going to the sidebar first is a different affordance from a row you have
  // to arrive at — and it is the one somebody who has never focused this column will find.
  return { title: "PLAN", hint: "^L", at: "below" as const, rows };
}

/**
 * Whose allowance this is: the provider's mark, its name, and what the plan is called.
 *
 * The mark and the colour come from the same [`Brand`] the model picker draws, at whatever fidelity
 * the terminal has — a Nerd Font glyph, geometry, or a letter under `ui.ascii_only`. Drawing our
 * own would mean two pictures of Anthropic in one program that disagree.
 *
 * The name is the *instance's* display name rather than the vendor's, because that is what the
 * picker calls it and what a second account of the same vendor would be distinguished by. Falling
 * back to the instance id is deliberate: `claude-cli` is worse than `Claude` and far better than a
 * blank row, and it happens only before the credential list has arrived.
 */
function accountRow(
  q: QuotaSnapshot,
  who: CredentialInfo[],
  glyphs: { ascii: boolean; nerd: boolean },
  room: number,
): StripRow {
  const cred = who.find((c) => c.instance === q.instance);
  const name = cred?.display_name || q.instance;
  const mark = markFor(cred?.brand, glyphs);
  return {
    text: `${mark} ${clip(name, Math.max(4, room - 2))}`,
    // The brand colour on the whole row, not just the mark: a contributed row carries one highlight
    // group, and of the two ways to spend it, the one that makes the account legible at a glance
    // beats a coloured glyph beside grey text.
    hl: cred?.brand?.hl ?? "Sidebar.Heading",
    right: q.plan ? { text: q.plan, hl: "Sidebar.Dim" } : undefined,
  };
}

/** The mark for a provider, at whatever fidelity this terminal has. See the model picker's copy. */
function markFor(b: Brand | null | undefined, glyphs: { ascii: boolean; nerd: boolean }): string {
  if (!b) return glyphs.ascii ? "?" : "·";
  if (glyphs.ascii) return b.ascii;
  return (glyphs.nerd && b.nerd) || b.mark;
}

/**
 * How many cells a bar in a narrow column is.
 *
 * Six to ten, because the useful resolution here is about an eighth: plenty of room, getting full,
 * nearly out. The exact figure is the number beside it, and a thirty-cell bar in a column this
 * narrow would leave no room for the label that says which allowance it is.
 */
const BAR_MIN = 5;
/** `▸ `, or the two spaces that keep an unmarked row in the same column as a marked one. */
const MARKER = 2;
/** ` 100%` — never given up, at any width. A bar with no number on it is a picture. */
const PCT = 5;

/**
 * A limit's name, shortened only as far as it has to be, and never past the part that identifies it.
 *
 * The old row put the bar first and clipped whatever was left of the label, which turned
 * `Weekly · Opus 5` into `Weekly ·…` — every character that distinguishes the per-model cap from
 * the plain weekly one, gone, leaving two rows with the same name and different numbers. So the
 * ladder goes the other way: give up the separator, then abbreviate the *base*, and only then clip
 * — and clip the scope, because `Wk Opus…` still says which two things it is about and
 * `Weekly ·…` says neither.
 */
function compactLabel(w: QuotaWindow, room: number): string {
  // The space between the label and the bar is part of the row too. Forgetting it is one column of
  // overflow at exactly the width where the label was already the thing being squeezed.
  const most = Math.max(6, room - MARKER - BAR_MIN - PCT - 1);
  if (cells(w.label) <= most) return w.label;
  const scope = w.scope ?? "";
  // Not a scoped limit, just a long name from a provider nobody here has heard of.
  if (scope === "") return clip(w.label, most);
  const base = w.label.slice(0, w.label.length - scope.length).replace(/[\s·]+$/, "");
  for (const b of [base, base.replace(/^Weekly$/i, "Wk")]) {
    if (cells(`${b} ${scope}`) <= most) return `${b} ${scope}`;
  }
  const short = base.replace(/^Weekly$/i, "Wk");
  return clip(`${short} ${scope}`, most);
}

function windowRow(
  w: QuotaWindow,
  label: string,
  labelWidth: number,
  bar: number,
  ascii: boolean,
  marked = true,
): string {
  // The limit that would actually refuse the next request, marked the way the panel marks it. Two
  // spaces otherwise, so the labels stay in one column — and nothing at all when there is only one
  // limit on the strip, since a marker that is on every row it can be on marks nothing and costs
  // two columns of a label that is being clipped.
  const marker = !marked ? "" : w.active ? (ascii ? "> " : "▸ ") : "  ";
  const pct = `${Math.round(w.used_percent)}%`.padStart(PCT);
  const name = clip(label, labelWidth);
  return `${marker}${name}${" ".repeat(Math.max(0, labelWidth - cells(name)))} ${
    meter(w.used_percent / 100, bar, { ascii })
  }${pct}`;
}

/**
 * How long a string is, counted the way {@link clip} counts it.
 *
 * Code points rather than UTF-16 units, so a label and the padding computed for it agree. Not
 * display width — that question is `neosh-tui`'s and is not answerable here — but the labels this
 * pads are limit names, and the alternative is a `.length` that disagrees with the clip on the
 * same string.
 */
function cells(s: string): number {
  return [...s].length;
}

/**
 * The one row here that is a sentence.
 *
 * Everything above it is a measurement, and a measurement has a direction you have to already know:
 * a bar that fills as an allowance is spent reads as "how much is left" to about half of everyone,
 * and a bare `3h` beside it could as easily be how long it has been running. This says which way
 * round both go, about the window that is actually doing the limiting — which is the only one of
 * the three that answers "may I start something long".
 *
 * It never names the window. The `▸` above already did, and a strip this narrow cannot afford to
 * say `Weekly · Opus 5` twice.
 */
function plainly(q: QuotaSnapshot, now: number, room: number): StripRow | null {
  const w = q.windows.find((x) => x.active) ?? binding(q.windows);
  if (!w) return null;
  const back = typeof w.resets_at === "number" ? countdown(w, now) : "";
  const left = Math.max(0, 100 - Math.round(w.used_percent));
  const text = left === 0
    ? back
      ? `used up · back in ${back}`
      : "used up"
    : back
      ? `${left}% left · back in ${back}`
      : `${left}% left`;
  return {
    text: clip(text, room),
    // Dim rather than graded: this is the explanation of the rows above, and an explanation that
    // shouts is one more red thing to work out the meaning of. The row it explains carries the
    // grade.
    hl: w.severity === "exhausted" ? "Diagnostic.Error" : "Sidebar.Dim",
  };
}

/** The fullest window, for a provider that did not say which of its limits binds. */
function binding(windows: QuotaWindow[]): QuotaWindow | undefined {
  return windows.reduce<QuotaWindow | undefined>(
    (best, w) => (!best || w.used_percent > best.used_percent ? w : best),
    undefined,
  );
}

/**
 * How many cells the context meter in the footer is.
 *
 * Eight, because the useful resolution there is about an eighth: plenty of room, getting full,
 * nearly out. The exact figure is the number beside it. The plan strip sizes its own bars from the
 * column it was given (see [`BAR_MIN`]) rather than sharing this, because the footer's width is the
 * terminal's and the sidebar's is a setting.
 */
const CELLS = 8;

/**
 * The group a row wears.
 *
 * Graded by the vendor, not by a threshold of ours — 94% of a weekly window is `critical` and 55%
 * of a session window is `normal`, and a client that decided at 80% for both would be shouting
 * about the wrong one. The binding window is drawn at full weight even when it is not yet worrying,
 * because "this is the one that will stop you" is worth saying before it does.
 */
function severityGroup(w: QuotaWindow): string {
  switch (w.severity) {
    case "exhausted":
      return "Diagnostic.Error";
    case "critical":
      return "Meter.Full";
    case "warn":
      return "Meter.Warn";
    default:
      return w.active ? "Sidebar.Heading" : "Sidebar.Dim";
  }
}

/** `2h`, `41m`, `now`. Empty for a window that does not roll over. */
function countdown(w: QuotaWindow, now: number): string {
  if (typeof w.resets_at !== "number") return "";
  const left = w.resets_at - now;
  if (left <= 0) return "now";
  if (left < 3600) return `${Math.max(1, Math.round(left / 60))}m`;
  if (left < 86_400) return `${Math.round(left / 3600)}h`;
  return `${Math.round(left / 86_400)}d`;
}

/**
 * Extra usage, when there is any to say.
 *
 * The interesting part is whether it is *on*: it is the difference between running out meaning
 * "this stops" and running out meaning "this starts costing money", and a strip that did not say
 * which would be describing two very different situations identically.
 */
function creditRow(q: QuotaSnapshot) {
  const c = q.credits;
  if (!c) return null;
  if (!c.enabled) {
    // Only worth a row once a limit is close. Said all the time it is noise; said at 95% it is the
    // answer to the question the row above just raised.
    const tight = q.windows.some((w) => w.used_percent >= 90);
    if (!tight) return null;
    return { text: "extra usage off", hl: "Sidebar.Dim" as const, right: undefined };
  }
  const balance = typeof c.balance === "string" ? c.balance : "";
  return {
    text: `extra usage${balance ? ` ${balance}` : ""}`,
    hl: "Sidebar.Dim" as const,
    right: typeof c.used_percent === "number"
      ? { text: `${Math.round(c.used_percent)}%`, hl: "Sidebar.Dim" }
      : undefined,
  };
}

/**
 * Say something the first time an allowance crosses the line.
 *
 * Once per window per crossing, which is what `warned` is for: these arrive on every turn boundary
 * and on every poll, and a notification per report is a workspace you learn to ignore. The set
 * clears for a window that has dropped back under, so the *next* time it climbs is news again.
 */
async function announce(neosh: Neosh, s: QuotaSnapshot, warned: Set<string>) {
  const at = (await neosh.opt.get<number>("usage.warn_at").catch(() => 90)) ?? 90;
  if (at <= 0) return;

  // Bookkeeping for every window, because a window that has dropped back under has to become news
  // again — but at most one thing *said*, however many crossed at once. An account whose session
  // and weekly limits are both tight is one situation, and two lines about it at the same instant
  // read as two problems.
  let worst: QuotaWindow | null = null;
  for (const w of s.windows) {
    const key = `${s.instance}/${w.id}`;
    if (w.used_percent < at) {
      warned.delete(key);
      continue;
    }
    if (warned.has(key)) continue;
    warned.add(key);
    if (!worst || w.used_percent > worst.used_percent) worst = w;
  }
  if (!worst) return;

  const left = Math.max(0, 100 - Math.round(worst.used_percent));
  const when = typeof worst.resets_at === "number"
    ? `, back in ${countdown(worst, Date.now() / 1000)}`
    : "";
  // The others are counted rather than named. "and 1 other limit" is the difference between
  // knowing this is the worst of several and thinking it is the only one.
  const others = s.windows.filter((w) => w !== worst && w.used_percent >= at).length;
  const also = others > 0 ? ` (and ${others} other limit${others > 1 ? "s" : ""})` : "";
  neosh.notify(
    left === 0
      ? `${worst.label} is used up${when}${also}`
      : `${left}% of your ${worst.label.toLowerCase()} allowance left${when}${also}`,
    worst.severity === "exhausted" ? "error" : "warn",
  );
}

/* -------------------------------------------------------------------------- */
/* The panel                                                                  */
/* -------------------------------------------------------------------------- */

/** What the panel is showing, which every key changes some part of. */
interface View {
  /** How many days back. `1` switches the resolution to hours, because a day of daily buckets is
   * one column. */
  days: number;
  /** Tokens or their money-equivalent. Two different questions about the same buckets: which model
   * did the *work*, and which model cost the money — and on a lineup with a 30x price spread those
   * are different answers. */
  metric: "tokens" | "cost";
  history: UsageHistory | null;
  quotas: QuotaSnapshot[];
  /** Percentages over time, for the gauges' sparklines. */
  samples: Array<{ at: number; instance: string; window: string; used_percent: number }>;
  loading: boolean;
  /** Whether this panel has asked the provider since it opened. Distinguishes "nobody has said"
   * from "we asked and got nowhere", which are different situations with different answers. */
  asked: boolean;
  /** What the scan could not read, drawn as a caveat rather than swallowed. */
  note: string | null;
  /**
   * Who each instance is, so a gauge can be headed by a provider rather than by an id.
   *
   * `claude-cli` is a configuration key. It was what this panel drew, and on a machine with two
   * accounts it made the one thing the reader needs to tell them apart — which vendor, which plan —
   * the one thing neither block said.
   */
  who: CredentialInfo[];
}

const WINDOWS = [1, 7, 30, 90];

async function installPanel({ neosh, subscriptions }: PluginContext) {
  let buf: number | null = null;
  let win: number | null = null;
  /**
   * Which terminal the panel is on.
   *
   * One panel, moved rather than duplicated. A plan meter is a thing you glance at and dismiss, and
   * a second copy of it in another window is one more thing to close — so `^L` in a terminal that
   * is not showing it takes it there. What it must not do is what a single window handle did
   * before views existed: push focus onto somebody else's screen and leave this one blank.
   */
  let on: ViewId | null = null;
  let ns: number | null = null;
  let list: CursoredList<null> | null = null;
  const view: View = {
    days: 30,
    metric: "cost",
    history: null,
    quotas: [],
    samples: [],
    loading: false,
    asked: false,
    note: null,
    who: [],
  };

  const load = async () => {
    view.loading = true;
    await draw();
    const until = Math.floor(Date.now() / 1000);
    const since = until - view.days * 86_400;
    const resolution: UsageResolution = view.days <= 1 ? "hour" : "day";
    const [history, quotas, samples, who] = await Promise.all([
      neosh.quota.usage({ since, until, resolution }).catch(() => null),
      neosh.quota.list().catch(() => [] as QuotaSnapshot[]),
      // The sparklines want the reset period, not the chart's span: a weekly window drawn over
      // ninety days is a flat line with four cliffs in it, which says nothing about this week.
      neosh.quota.history({ since: until - 7 * 86_400, until }).catch(() => []),
      neosh.agent.credentials().catch(() => [] as CredentialInfo[]),
    ]);
    view.history = history;
    view.who = who;
    // Merged, never assigned. This read started seconds ago — the transcript scan is the slow part
    // — and a snapshot that arrived by event in the meantime is newer than anything in it. Assigned,
    // a poll landing during the scan was drawn for an instant and then replaced by the empty list
    // this call had read before it landed, and the panel said nothing had reported an allowance
    // while the sidebar three columns away was showing three.
    view.quotas = freshest(view.quotas, quotas);
    view.samples = samples;
    view.note = history ? scanNote(history) : "could not read the transcripts";
    view.loading = false;
    await draw();
  };

  const draw = async () => {
    if (buf === null || ns === null || list === null) return;
    const cols = Math.max(40, await panelWidth(neosh, win));
    const [ascii, nerd] = await Promise.all([
      neosh.opt.get<boolean>("ui.ascii_only").catch(() => false),
      neosh.opt.get<boolean>("ui.nerd_font").catch(() => false),
    ]);
    const glyphs = { ascii: ascii ?? false, nerd: nerd ?? false };
    const rows = await body(neosh, view, cols, glyphs);
    list.setRows(rows);
    await list.render({ win: win ?? undefined });
  };

  const open = async (where?: ViewId) => {
    if (win !== null && (where === undefined || on === where)) {
      await neosh.focus.push(win);
      return;
    }
    // Asked for from a terminal that is not the one it is on. Taken there rather than opened twice.
    if (win !== null) await close();
    on = where ?? null;
    // The span it opens on is a setting, snapped to one the keys can also reach — otherwise `]`
    // from a configured 45 days lands on whichever of the four it decides is next, and the number
    // you set is one you can never get back to.
    const configured = (await neosh.opt.get<number>("usage.days").catch(() => 30)) ?? 30;
    view.days = WINDOWS.reduce((best, d) =>
      Math.abs(d - configured) < Math.abs(best - configured) ? d : best
    , WINDOWS[0]!);
    buf ??= await neosh.buf.create({ name: "[usage]", scratch: true, kind: KIND });
    ns ??= await neosh.ns.create(`${NS}.panel`);
    // A float rather than the main dock. Taking the dock would put the transcript away to show a
    // chart, and this is a thing you glance at between turns — not a place you go and come back
    // from. Wide, because a chart narrower than its span is a chart with columns missing.
    // Named rather than derived: the buffer is reused between openings, so "the terminal showing
    // it" is the one it was last on rather than the one asking for it now.
    win = await (on === null ? neosh : neosh.view.at(on)).float.open(buf, {
      anchor: { kind: "screen" },
      width: { kind: "max", n: 104 },
      height: { kind: "max", n: 34 },
      border: "rounded",
      title: " usage ",
      focusable: true,
    });
    list = new CursoredList<null>(neosh, buf, ns);
    await neosh.focus.push(win);
    await draw();
    // Ask while the scan runs. Opening this is as good a statement of "I want to know now" as
    // pressing `r`, and the answer arrives through `onChange` rather than through the load — which
    // is why the two writers below have to agree about which of them is fresher.
    void neosh.quota.refresh().catch(() => {});
    view.asked = true;
    await load();
  };

  const close = async () => {
    if (win === null) return;
    const w = win;
    win = null;
    on = null;
    list = null;
    await neosh.focus.pop().catch(() => {});
    await neosh.win.close(w).catch(() => {});
  };

  // Every key is a named command, so `^Z` lists them and `init.ts` can move them. A `switch` on the
  // key inside a handler is the thing this replaced — see ADR 0040.
  const cmd = async (name: string, desc: string, fn: () => void | Promise<void>) => {
    subscriptions.push(await neosh.cmd.register(`${NS}.${name}`, fn, { desc }));
  };

  subscriptions.push(
    await neosh.cmd.register(`${NS}.panel`, (_args, key) => open(key?.view), {
      desc: "What the plan has left, and where the week went",
    }),
  );
  await cmd("panel.close", "Close the usage panel", close);
  await cmd("panel.refresh", "Ask the provider again", async () => {
    await neosh.quota.refresh().catch(() => {});
    view.asked = true;
    await load();
  });
  await cmd("panel.tokens", "Show tokens rather than cost", async () => {
    view.metric = "tokens";
    await draw();
  });
  await cmd("panel.cost", "Show cost rather than tokens", async () => {
    view.metric = "cost";
    await draw();
  });
  await cmd("panel.metric", "Swap between tokens and cost", async () => {
    view.metric = view.metric === "cost" ? "tokens" : "cost";
    await draw();
  });
  await cmd("panel.wider", "A longer span", async () => {
    const i = WINDOWS.indexOf(view.days);
    view.days = WINDOWS[Math.min(WINDOWS.length - 1, i + 1)] ?? view.days;
    await load();
  });
  await cmd("panel.narrower", "A shorter span", async () => {
    const i = WINDOWS.indexOf(view.days);
    view.days = WINDOWS[Math.max(0, i - 1)] ?? view.days;
    await load();
  });
  for (const days of WINDOWS) {
    await cmd(`panel.span.${days}`, `${days === 1 ? "The last day" : `${days} days`}`, async () => {
      view.days = days;
      await load();
    });
  }
  await cmd("panel.down", "Move down", async () => {
    list?.move(1);
    await list?.render({ win: win ?? undefined });
  });
  await cmd("panel.up", "Move up", async () => {
    list?.move(-1);
    await list?.render({ win: win ?? undefined });
  });
  await cmd("report", "What this conversation has used", () => report(neosh));

  const scope = { kind: "buf_kind", name: KIND } as const;
  const key = async (lhs: string, name: string, desc?: string) => {
    await neosh.keymap.set("chat", lhs, `${NS}.${name}`, { scope, desc });
  };
  // At chat scope, because this is the way *in*. Binding it inside the panel as well would be a
  // key that reopens what is already open.
  await neosh.keymap.set("chat", "<C-l>", `${NS}.panel`, { desc: "Plan usage and history" });
  await key("q", "panel.close");
  await key("<Esc>", "panel.close");
  await key("<C-c>", "panel.close");
  await key("r", "panel.refresh", "Ask the provider again");
  await key("t", "panel.tokens", "Tokens");
  await key("c", "panel.cost", "Cost");
  await key("<Tab>", "panel.metric", "Tokens or cost");
  await key("j", "panel.down");
  await key("k", "panel.up");
  await key("<Down>", "panel.down");
  await key("<Up>", "panel.up");
  await key("]", "panel.wider", "A longer span");
  await key("[", "panel.narrower", "A shorter span");
  for (const [lhs, days] of [["1", 1], ["7", 7], ["3", 30], ["9", 90]] as const) {
    await key(lhs, `panel.span.${days}`);
  }

  // A number that moved while the panel is open is a panel that is wrong. Only the gauges are
  // refreshed on this: the history comes from files and re-scanning them on every turn boundary
  // would be a panel that stutters.
  subscriptions.push(neosh.quota.onChange(async (s) => {
    if (win === null) return;
    view.quotas = freshest(view.quotas, [s]);
    await draw();
  }));
  subscriptions.push({ dispose: () => void close() });
}

/**
 * Two sets of snapshots, keeping whichever account is the later observation.
 *
 * Per instance rather than per set: a slow read may be newer for one account and older for another,
 * and taking the newer *set* would throw away the good half of it.
 */
function freshest(a: QuotaSnapshot[], b: QuotaSnapshot[]): QuotaSnapshot[] {
  const by = new Map<string, QuotaSnapshot>();
  for (const s of [...a, ...b]) {
    const at = by.get(s.instance);
    if (!at || s.observed_at >= at.observed_at) by.set(s.instance, s);
  }
  // Sorted, so the order does not depend on which of the two writers happened to run first — a
  // panel whose gauges swap places when a poll lands is one you cannot read while it updates.
  return [...by.values()].sort((x, y) => x.instance.localeCompare(y.instance));
}

/**
 * How wide the panel is.
 *
 * Asked of the window rather than assumed, because this is a docked main window and the terminal is
 * whatever size the terminal is. A chart built to a guessed width wraps, and a wrapped chart is not
 * a chart — it is two rows of glyphs that no longer line up with their axis.
 */
async function panelWidth(neosh: Neosh, win: number | null): Promise<number> {
  if (win === null) return 80;
  const vp = await neosh.win.viewport(win).catch(() => null);
  return vp?.width ?? 80;
}

/* -------------------------------------------------------------------------- */
/* Drawing it                                                                 */
/* -------------------------------------------------------------------------- */

/**
 * The whole panel, top to bottom.
 *
 * Gauges first, because they are the thing that can stop you and the reason you pressed the key.
 * The chart second, because "where did it go" is a question you only ask once you have seen the
 * answer to "how much is left".
 */
async function body(
  neosh: Neosh,
  view: View,
  cols: number,
  glyphs: { ascii: boolean; nerd: boolean },
): Promise<ListRow<null>[]> {
  const { ascii } = glyphs;
  const rows: ListRow<null>[] = [];
  const rule = () => rows.push({ text: "─".repeat(cols), hl: "Separator", inert: true });
  const blank = () => rows.push({ text: "", inert: true });
  const head = (text: string, right?: string) =>
    rows.push({
      text: ` ${text}`,
      hl: "Sidebar.Heading",
      right: right ? { text: `${right} `, hl: "Comment" } : undefined,
      inert: true,
    });

  const now = Date.now() / 1000;
  // `r refresh`, not `usage.panel.refresh · r`. A command name in a heading is the program telling
  // you what it calls something rather than what you can do with it.
  head("YOUR PLAN", view.quotas.length ? "r refresh" : undefined);
  rule();
  if (view.quotas.length > 0) {
    // What these numbers *are*, which no amount of care with the bars can say on its own. A rolling
    // allowance is not a balance and not a token count: it refills on a clock, the vendor decides
    // how big it is and will not itemise it, and a turn you ran in the vendor's own CLI spent it
    // too. Everything below is a percentage of something opaque, and saying so once is what stops
    // people planning around it as though it were money.
    rows.push({
      text: "   Rolling allowances your plan enforces. They refill on a clock, and the vendor",
      hl: "Comment",
      inert: true,
    });
    rows.push({
      text: "   never says how big they are — so these are percentages, never token counts.",
      hl: "Comment",
      inert: true,
    });
    blank();
  }
  if (view.quotas.length === 0) {
    // Two different situations, and only one of them is about to change on its own. A workspace
    // that has never been told is waiting for a turn; one that asked and got nowhere is waiting
    // for a network, a login, or an endpoint that has told it to ask less often — and saying
    // "nothing has reported yet" to the second is an invitation to press `r` forever.
    rows.push({
      text: view.asked
        ? "   asked, and could not get an answer"
        : "   nothing has reported an allowance yet",
      hl: "Comment",
      inert: true,
    });
    rows.push({
      text: view.asked
        ? "   a turn will report it; r asks again"
        : "   run a turn, or press r to ask now",
      hl: "Comment",
      inert: true,
    });
  }
  for (const q of view.quotas) {
    rows.push(...gaugeBlock(q, view.who, view.samples, cols, glyphs, now));
  }

  blank();
  const label = view.days === 1 ? "THE LAST DAY, BY HOUR" : `THE LAST ${view.days} DAYS`;
  head(label, `${view.metric} · ⇥`);
  rule();
  if (view.loading) {
    rows.push({ text: "   reading the transcripts…", hl: "Comment", inert: true });
  } else if (!view.history || view.history.buckets.length === 0) {
    rows.push({ text: "   nothing in this span", hl: "Comment", inert: true });
  } else {
    rows.push(...chart(view.history, view.metric, cols, ascii));
    blank();
    rows.push(...breakdown(view.history, view.metric, cols));
  }
  if (view.note) {
    blank();
    rows.push({ text: `   ${view.note}`, hl: "Diagnostic.Warn", inert: true });
  }

  // Anything a plugin contributed, last, under its own rule. Rows rather than a callback, so a
  // contributor that is not loaded is simply absent rather than an error at draw time.
  const extra = await neosh.ext
    .list<{ text: string; hl?: string; right?: string; command?: string }>(POINT_ROW)
    .catch(() => []);
  if (extra.length > 0) {
    blank();
    head("ALSO");
    rule();
    for (const c of extra) {
      if (typeof c.item?.text !== "string") continue;
      rows.push({
        text: `   ${c.item.text}`,
        hl: c.item.hl,
        right: c.item.right ? { text: `${c.item.right} `, hl: "Comment" } : undefined,
        inert: typeof c.item.command !== "string",
      });
    }
  }

  blank();
  rows.push({ text: ` ${hints()}`, hl: "Comment", inert: true });
  return rows;
}

function hints(): string {
  return "1 7 3 9 span   [ ] wider narrower   ⇥ tokens/cost   r refresh   q close";
}

/**
 * One account: its plan, its windows, and how each has moved.
 *
 * The sparkline is the part the strip cannot show. A window at 94% is one fact; a window that was
 * at 40% two hours ago is a different situation from one that has been at 92% all week, and only
 * the second of those is a reason to stop.
 */
function gaugeBlock(
  q: QuotaSnapshot,
  who: CredentialInfo[],
  samples: View["samples"],
  cols: number,
  glyphs: { ascii: boolean; nerd: boolean },
  now: number,
): ListRow<null>[] {
  const { ascii } = glyphs;
  const rows: ListRow<null>[] = [];
  const age = now - q.observed_at;
  // The provider, drawn the way the model picker draws it: same mark, same brand colour, same name.
  // This row used to be the instance id, which is a configuration key — and on a machine with a
  // Claude plan and a Codex one it made the single thing that tells the two blocks apart the one
  // thing neither of them said.
  const cred = who.find((c) => c.instance === q.instance);
  rows.push({
    text: `   ${markFor(cred?.brand, glyphs)} ${cred?.display_name || q.instance}${
      q.plan ? `  ${q.plan}` : ""
    }`,
    hl: cred?.brand?.hl ?? "Sidebar.Heading",
    // How old the number is, always. A percentage with no age on it is one people trust for longer
    // than they should — and at rest, with polling off, "an hour ago" is the whole answer.
    right: { text: `${freshness(age, q.source)} `, hl: "Comment" },
    inert: true,
  });

  const SPARK = 16;
  const labelWidth = Math.max(...q.windows.map((w) => byteLength(w.label)), 8);
  // Everything on the row that is not the bar: the indent and marker, the label, the percentage,
  // the sparkline and the longest countdown. Subtracted rather than estimated, because the bar is
  // the only elastic thing here and anything the arithmetic forgets is drawn off the right edge of
  // the float — where it is not clipped so much as simply gone, which is how `resets in 8h` became
  // the word `resets`.
  const FIXED = 5 + labelWidth + 2 + 6 + 2 + SPARK + 2 + 18;
  const barWidth = Math.max(8, Math.min(40, cols - FIXED));

  // What each column is, once, over the columns themselves.
  //
  // Every part of the row below is a measurement with a direction you have to already know: a bar
  // that fills as an allowance is *spent*, a sparkline that is not labelled could be any span, and
  // a bare `3h` is as easily "running for" as "back in". Naming them costs one dim row per account
  // and is the difference between a display you read and one you interpret.
  rows.push({
    text: `     ${"limit".padEnd(labelWidth)}  ${"used".padEnd(barWidth)}      ${
      "last 7 days".padEnd(SPARK)
    }  refills`,
    hl: "Comment",
    inert: true,
  });

  for (const w of q.windows) {
    const pct = Math.round(w.used_percent);
    const bar = meter(w.used_percent / 100, barWidth, { ascii });
    const spark = sparkline(
      samples.filter((s) => s.instance === q.instance && s.window === w.id).map((s) => s.used_percent),
      SPARK,
      ascii,
    );
    const marker = w.active ? (ascii ? ">" : "▸") : " ";
    const head = `   ${marker} ${w.label.padEnd(labelWidth)}  `;
    // Everything inline, and no right-aligned column. A `right` here is drawn as virtual text after
    // the row's own text, so a row this wide pushes it past the edge — and the one thing on it that
    // is worth having when it is nearly full is when it comes back.
    const text = `${head}${bar} ${String(pct).padStart(3)}%  ${spark}  ${resetsWord(w, now)}`;
    // The bar and its percentage carry the severity colour; the label, the spark and the countdown
    // stay in the text colour, so a block of four rows does not read as four alarms.
    const barFrom = byteLength(head);
    rows.push({
      text,
      hl: w.active ? undefined : "Comment",
      spans: [{ from: barFrom, to: barFrom + byteLength(bar) + 5, hl: severityGroup(w) }],
      inert: true,
    });
  }
  const credit = creditRow(q);
  if (credit) {
    rows.push({
      text: `     ${credit.text}`,
      hl: "Comment",
      right: credit.right ? { text: `${credit.right.text} `, hl: "Comment" } : undefined,
      inert: true,
    });
  }
  rows.push({ text: "", inert: true });
  return rows;
}

/** `just now`, `4m ago`, `2h ago (last turn)`. */
function freshness(age: number, source: QuotaSnapshot["source"]): string {
  const how = source === "turn" ? " · last turn" : source === "plugin" ? " · plugin" : "";
  if (age < 90) return `just now${how}`;
  if (age < 3600) return `${Math.round(age / 60)}m ago${how}`;
  if (age < 86_400) return `${Math.round(age / 3600)}h ago${how}`;
  return `${Math.round(age / 86_400)}d ago${how}`;
}

/** `resets in 2h 14m`, `resets at the top of the hour`, or nothing. */
function resetsWord(w: QuotaWindow, now: number): string {
  if (typeof w.resets_at !== "number") return "";
  const left = w.resets_at - now;
  if (left <= 0) return "resetting";
  // Days, once there are any. `elapsed` counts a turn, which never runs for a week, so it tops out
  // at hours — and a weekly allowance came back as `resets in 164h 34m`, a number you have to do
  // arithmetic on to find out it means Tuesday.
  if (left >= 86_400) {
    const days = Math.floor(left / 86_400);
    const hours = Math.floor((left % 86_400) / 3600);
    return `resets in ${days}d${hours > 0 ? ` ${hours}h` : ""}`;
  }
  return `resets in ${elapsed(left * 1000).replace(/ \d+s$/, "")}`;
}

/**
 * A line, in one row.
 *
 * Eight block heights, scaled to 0-100 rather than to the data's own range: a window that has been
 * between 91% and 94% all day is a *flat line near the top*, and auto-scaling it would draw a
 * dramatic climb out of three percentage points. The one thing this chart is for is the shape of an
 * allowance being used up, and that shape only means anything against the whole allowance.
 */
function sparkline(values: number[], width: number, ascii: boolean): string {
  if (values.length === 0) return " ".repeat(width);
  const glyphs = ascii ? [".", ".", ":", ":", "|", "|", "|", "|"] : ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
  // The most recent `width` points, because the right-hand end is the part that is still true.
  const tail = values.slice(-width);
  const out = tail.map((v) => {
    const i = Math.min(glyphs.length - 1, Math.max(0, Math.floor((v / 100) * glyphs.length)));
    return glyphs[i] ?? glyphs[0]!;
  });
  // Padded on the *left*, so the newest point is always in the same column whether there are three
  // samples or thirty. Padded right, the line would slide across the row as history accumulated.
  return " ".repeat(Math.max(0, width - out.length)) + out.join("");
}

/**
 * The history, as bars.
 *
 * Vertical, not horizontal, and the reason is the axis: the useful comparison is between *periods*
 * — was yesterday heavier than today — and periods read left to right. A horizontal bar per day is
 * a list you have to scan down to compare, which is the one thing a chart is supposed to save you.
 *
 * Eight rows of half-blocks gives sixteen levels in eight terminal rows, which is enough resolution
 * to see a spike and cheap enough to redraw on every keystroke.
 */
const CHART_ROWS = 8;

function chart(
  history: UsageHistory,
  metric: View["metric"],
  cols: number,
  ascii: boolean,
): ListRow<null>[] {
  const span = history.resolution === "hour" ? 3_600 : 86_400;
  // The grid is the *scan's* grid, anchored on a boundary it actually produced.
  //
  // A day starts at local midnight, and only the scan knows which zone that was — so a bucket for
  // the 21st is stamped 22:00 UTC on the 20th two zones east of Greenwich. Re-deriving the grid
  // here by flooring to a UTC day put every one of those in the previous column, which is a chart
  // whose last day is yesterday and whose every label is off by one.
  const anchor = history.buckets[0]?.start ?? alignDown(history.since, span);
  const gridAt = (t: number) => anchor + Math.floor((t - anchor) / span) * span;

  // Every period in the span, including the empty ones. A chart drawn only from the periods that
  // had activity is a chart with no gaps in it — which is the same picture as working every single
  // day, and the opposite of the truth.
  const periods: number[] = [];
  for (let t = gridAt(history.since); t < history.until; t += span) periods.push(t);

  const gutter = 9;
  const plotWidth = Math.max(8, cols - gutter - 2);
  // How many columns one period gets. A day drawn one cell wide in a plot seventy cells across is
  // technically a bar chart and reads as a rendering fault — so a short span spends the room it
  // has, up to a width past which a bar stops looking like a measurement and starts looking like a
  // block. The last column of a wide bar is left blank as the gap between periods.
  const cell = Math.max(1, Math.min(7, Math.floor(plotWidth / Math.max(1, periods.length))));
  const ink = cell > 1 ? cell - 1 : 1;
  // Whole periods only. Half a bar at the left edge is a period the reader has no way to know is
  // half there, and it is always the oldest one — the one a partial scan would also have lost.
  const fit = Math.max(1, Math.floor(plotWidth / cell));
  const shown = periods.slice(-fit);
  const dropped = periods.length - shown.length;

  const byPeriod = new Map<number, UsageBucket[]>();
  for (const b of history.buckets) {
    // Already on the grid, so this is a lookup key and not a second flooring. Snapped anyway, so a
    // scan that ever returns an unaligned bucket lands in a column rather than in nothing.
    const key = gridAt(b.start);
    const at = byPeriod.get(key);
    if (at) at.push(b);
    else byPeriod.set(key, [b]);
  }
  const valueOf = (t: number) =>
    (byPeriod.get(t) ?? []).reduce((sum, b) => sum + amount(b, metric), 0);

  const values = shown.map(valueOf);
  const peak = Math.max(...values, 0);
  const rows: ListRow<null>[] = [];
  if (peak <= 0) {
    // "Nothing happened" and "nothing here has a price" are different facts, and only one of them
    // is about you. A span with eight thousand calls in it and no published rate for any of the
    // models drew a flat "nothing in this span" directly above a table listing every one of them —
    // which reads as a bug in the chart, and is worse than that: it is the panel disagreeing with
    // itself about whether the week happened.
    if (metric === "cost" && history.buckets.length > 0) {
      return [
        { text: "   no published rate for anything in this span", hl: "Comment", inert: true },
        { text: "   ⇥ for tokens, which are counted either way", hl: "Comment", inert: true },
      ];
    }
    return [{ text: "   nothing in this span", hl: "Comment", inert: true }];
  }

  // Two levels per row: a full block and a half block. The alternative is eight rows of eight
  // levels, which cannot draw anything smaller than an eighth of the peak — and on a week with one
  // heavy day, that is every other day drawn as zero.
  const levels = CHART_ROWS * 2;
  const heights = values.map((v) => Math.round((v / peak) * levels));
  const [full, half, empty] = ascii ? ["#", "=", " "] : ["█", "▄", " "];

  for (let row = CHART_ROWS - 1; row >= 0; row--) {
    const cells = heights.map((h) => {
      const filled = h - row * 2;
      const glyph = filled >= 2 ? full! : filled === 1 ? half! : empty!;
      return glyph.repeat(ink) + " ".repeat(cell - ink);
    });
    // The axis labels sit in the gutter: the peak on the top row and zero on the bottom, which is
    // the least a reader needs to turn a shape back into a number.
    const label = row === CHART_ROWS - 1
      ? formatted(peak, metric).padStart(gutter - 1)
      : row === 0
      ? formatted(0, metric).padStart(gutter - 1)
      : "".padStart(gutter - 1);
    rows.push({
      text: `${label} ${cells.join("")}`,
      spans: [{ from: gutter, to: gutter + byteLength(cells.join("")), hl: "Meter.Fill" }],
      hl: "Comment",
      inert: true,
    });
  }
  rows.push({
    text: `${"".padStart(gutter - 1)} ${"─".repeat(shown.length * cell)}`,
    hl: "Separator",
    inert: true,
  });
  rows.push({
    text: `${"".padStart(gutter - 1)} ${axis(shown, span, shown.length * cell)}`,
    hl: "Comment",
    inert: true,
  });
  if (dropped > 0) {
    // Said, never silent. A chart that quietly dropped the oldest columns reads as a span that
    // started later than it did.
    rows.push({
      text: `   the oldest ${dropped} ${span === 3_600 ? "hours" : "days"} do not fit this window`,
      hl: "Comment",
      inert: true,
    });
  }
  return rows;
}

/**
 * The label strip under the bars.
 *
 * Three labels at most — the ends and the middle — and every one of them is dropped rather than
 * drawn over its neighbour. Labelling every column needs four characters per column and there is
 * one; labelling none leaves a shape with no idea when it happened; overlapping them produces
 * `13120/8`, which is worse than either because it looks like a date.
 *
 * Right-hand end first, because it is the one that is always worth having: it says how recent the
 * newest column is, which is the difference between a chart of this week and a chart of last.
 */
function axis(periods: number[], span: number, width: number): string {
  const out: string[] = new Array(width).fill(" ");
  if (periods.length === 0) return out.join("");

  const label = (t: number) => {
    const d = new Date(t * 1000);
    return span === 3_600
      ? `${String(d.getHours()).padStart(2, "0")}h`
      : `${d.getDate()}/${d.getMonth() + 1}`;
  };
  /** Write it only if every cell it needs is still blank, plus a space either side. */
  const put = (at: number, text: string): boolean => {
    const start = Math.min(Math.max(0, at), width - text.length);
    if (start < 0) return false;
    for (let i = start - 1; i <= start + text.length; i++) {
      if (i >= 0 && i < width && out[i] !== " ") return false;
    }
    for (let i = 0; i < text.length; i++) out[start + i] = text[i]!;
    return true;
  };

  const last = label(periods[periods.length - 1]!);
  put(width - last.length, last);
  if (periods.length > 1) put(0, label(periods[0]!));
  if (periods.length > 4) {
    const mid = label(periods[Math.floor(periods.length / 2)]!);
    put(Math.floor((width - mid.length) / 2), mid);
  }
  return out.join("");
}

/**
 * Which models the span went on, biggest first.
 *
 * By model rather than by day, because it is the actionable cut: "Opus was 80% of the cost" is
 * something you can do something about, and "Tuesday was busy" is not.
 */
function breakdown(history: UsageHistory, metric: View["metric"], cols: number): ListRow<null>[] {
  const totals = new Map<string, { instance: string; model: string; value: number; usage: UsageBucket["usage"]; requests: number; priced: boolean }>();
  for (const b of history.buckets) {
    const key = `${b.instance}/${b.model}`;
    const at = totals.get(key) ?? {
      instance: b.instance,
      model: b.model,
      value: 0,
      usage: { input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0, thinking_tokens: 0 },
      requests: 0,
      priced: true,
    };
    at.value += amount(b, metric);
    at.usage.input_tokens += b.usage.input_tokens;
    at.usage.output_tokens += b.usage.output_tokens;
    at.usage.cache_read_tokens += b.usage.cache_read_tokens;
    at.usage.cache_write_tokens += b.usage.cache_write_tokens;
    at.requests += b.requests;
    at.priced &&= b.basis === "priced";
    totals.set(key, at);
  }
  const ranked = [...totals.values()].sort((a, b) => b.value - a.value);
  if (ranked.length === 0) return [];

  const grand = ranked.reduce((s, r) => s + r.value, 0);
  const rows: ListRow<null>[] = [];
  const nameWidth = Math.min(34, Math.max(...ranked.map((r) => byteLength(r.model)), 10));
  for (const r of ranked.slice(0, 12)) {
    const share = grand > 0 ? r.value / grand : 0;
    // A share bar rather than a second number: the eye reads twelve bars as a ranking and twelve
    // percentages as twelve things to compare one at a time.
    const bar = meter(share, 10, {});
    const cost = r.priced ? formatted(r.value, metric) : `${formatted(r.value, metric)}*`;
    rows.push({
      text: `   ${clip(r.model, nameWidth).padEnd(nameWidth)}  ${bar} ${cost.padStart(9)}  ${String(r.requests).padStart(5)} calls`,
      hl: "Comment",
      right: {
        text: `${compact(r.usage.cache_read_tokens)} cached `,
        hl: "Comment",
      },
      inert: true,
    });
  }
  if (ranked.length > 12) {
    rows.push({ text: `   and ${ranked.length - 12} more`, hl: "Comment", inert: true });
  }
  // The cache line, because it is the difference between a span that is expensive and one that only
  // looks it: a week that is 90% cache reads cost a fraction of what its token count implies.
  const saved = history.buckets.reduce((s, b) => s + b.cache_savings_usd, 0);
  if (saved > 0 && metric === "cost") {
    rows.push({ text: "", inert: true });
    rows.push({
      text: `   caching saved ${money(saved)} of it`,
      hl: "Diagnostic.Ok",
      inert: true,
    });
  }
  if (!history.fully_priced && metric === "cost") {
    rows.push({
      text: "   * no published rate for this model; its tokens count, its cost does not",
      hl: "Comment",
      inert: true,
    });
  }
  void cols;
  return rows;
}

function amount(b: UsageBucket, metric: View["metric"]): number {
  if (metric === "cost") return b.cost_usd;
  const u = b.usage;
  // Thinking tokens are reported *inside* output and must never be added on top.
  return u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens;
}

function formatted(v: number, metric: View["metric"]): string {
  return metric === "cost" ? money(v) : compact(v);
}

function alignDown(t: number, span: number): number {
  return Math.floor(t / span) * span;
}

function clip(s: string, n: number): string {
  const chars = [...s];
  return chars.length <= n ? s : `${chars.slice(0, Math.max(1, n - 1)).join("")}…`;
}

/** What the scan could not read, in one line, or nothing when it read everything. */
function scanNote(history: UsageHistory): string | null {
  const bad = history.sources.filter((s) => s.status === "partial" || s.status === "failed");
  if (bad.length === 0) return null;
  const first = bad[0]!;
  return first.message ?? `could not fully read ${first.path}`;
}

/* -------------------------------------------------------------------------- */
/* The status line: this request, and this conversation                        */
/* -------------------------------------------------------------------------- */

/**
 * The two numbers that are about the conversation you are in.
 *
 * Separate from the strip above because they answer a different question and change on a different
 * clock: these move with every turn in *this* conversation, the plan moves with every turn in every
 * conversation on the account.
 */
async function installFooter({ neosh, subscriptions }: PluginContext) {
  const refresh = async () => {
    const session = await neosh.session.current().catch(() => null);
    if (!session) return;
    await drawContext(neosh, session);
    await drawTokens(neosh, session);
  };

  await refresh();
  // Not only at turn end. An agent driver reports how full its context is while it works, and a
  // meter that waited for the turn to finish would be answering "should I start a new
  // conversation?" only once it was too late to matter for this one.
  subscriptions.push(
    neosh.agent.onActivity((e) => {
      if (e.activity.kind === "context" || e.activity.kind === "compacted") void refresh();
    }),
  );
  // Turn end is when the running total changes, and for a model driver the only time either can.
  subscriptions.push(neosh.agent.onTurnEnd(() => void refresh()));
  subscriptions.push(neosh.session.onChange(() => void refresh()));
  // The model is half of the meter: the same conversation is 40% of one window and 8% of another.
  // The *selection*, not the `agent.model` option — a model that resolves late, or one swapped out
  // because it could not authenticate, changes the denominator without anybody setting anything.
  subscriptions.push(neosh.agent.onSelectionChange(() => void refresh()));
  subscriptions.push(
    neosh.opt.onChange((e) => {
      if (e.name === "usage.show_tokens" || e.name === "ui.ascii_only") void refresh();
    }),
  );
}

/**
 * The window those tokens are a fraction of.
 *
 * The driver's own figure first, and it is not a close call. A catalogue says what a *model* has;
 * a vendor CLI's window is whatever that CLI decided, and the two disagree — `claude` running
 * `claude-haiku-4-5` reports 200k where the catalogue entry for the selected model said a million.
 * Dividing by the wrong one is how a meter reads `2%` where the agent itself would say `8%`.
 *
 * The catalogue is the fallback, for a model driver and for a conversation that has not run a turn
 * yet. Nothing at all is the third case, and then the number is shown without a percentage rather
 * than against a denominator somebody made up.
 */
async function contextWindow(neosh: Neosh, session: SessionInfo): Promise<number | undefined> {
  if (session.context_window) return session.context_window;
  const selection = await neosh.agent.selection().catch(() => null);
  if (!selection) return undefined;
  const entries = await neosh.agent
    .listModels(selection.instance)
    .catch(() => [] as ModelEntry[]);
  const model = entries.find((e) => e.model.id === selection.model);
  return model?.model.context_window ?? undefined;
}

/**
 * The context meter: a bar, then the percentage, then what it is a percentage of.
 *
 * Drawn even at zero, which is the change that matters. It used to appear only once a turn had
 * been spent, so the one moment you could not see how much room a model has was *before* choosing
 * what to do with it — and a model with a 200k window and one with a million are different tools.
 *
 * One colour for the whole segment rather than a lit part and a dim part: a status segment carries
 * one highlight, and two glyphs of very different weight already say where the fill ends. The
 * colour is then free to say the thing colour is good at, which is how worried to be.
 */
async function drawContext(neosh: Neosh, session: SessionInfo) {
  const used = session.context_tokens ?? 0;
  const window = await contextWindow(neosh, session);
  // Nothing to be a percentage of. Better to say the number than to invent a denominator.
  if (!window) {
    if (used > 0) {
      await neosh.status.set("context", { text: `${short(used)} ctx`, priority: 20 });
    } else {
      await neosh.status.clear("context");
    }
    return;
  }
  const ascii = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ?? false;
  const fraction = Math.min(1, used / window);
  const percent = fraction * 100;
  await neosh.status.set("context", {
    text: `${meter(fraction, CELLS, { ascii })} ${percent.toFixed(0)}% of ${short(window)}`,
    hl: percent > 90 ? "Meter.Full" : percent > 70 ? "Meter.Warn" : "Comment",
    priority: 20,
  });
}

async function drawTokens(neosh: Neosh, session: SessionInfo) {
  const on = (await neosh.opt.get<boolean>("usage.show_tokens").catch(() => true)) ?? true;
  const u = session.usage;
  const total = u.input_tokens + u.output_tokens;
  if (!on || total === 0) {
    await neosh.status.clear("tokens");
    return;
  }
  await neosh.status.set("tokens", {
    text: `↑${short(u.input_tokens)} ↓${short(u.output_tokens)}`,
    priority: 21,
  });
}

/** A token count at a glance: `1.2k`, `340k`, `1.4M`. */
function short(n: number): string {
  if (n < 1000) return String(n);
  if (n < 10_000) return `${(n / 1000).toFixed(1)}k`;
  if (n < 1_000_000) return `${Math.round(n / 1000)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

/**
 * The long version, for when the footer's two numbers are not enough.
 *
 * Cache reads are broken out because they are the difference between a conversation that is
 * expensive and one that merely looks it: a turn whose prompt is 90% cache read costs a tenth of
 * what the raw input number suggests.
 */
async function report(neosh: Neosh): Promise<void> {
  const session = await neosh.session.current().catch(() => null);
  if (!session) {
    neosh.notify("no conversation", "warn");
    return;
  }
  const u = session.usage;
  const window = await contextWindow(neosh, session);
  const used = session.context_tokens ?? 0;
  const lines = [
    `context   ${short(used)}${window ? ` of ${short(window)}  (${((used / window) * 100).toFixed(1)}%)` : ""}`,
    `input     ${short(u.input_tokens)}`,
    `output    ${short(u.output_tokens)}`,
    `cache     ${short(u.cache_read_tokens)} read, ${short(u.cache_write_tokens)} written`,
  ];
  if (u.thinking_tokens > 0) lines.push(`thinking  ${short(u.thinking_tokens)}`);
  neosh.notify(lines.join("   ·   "));
}
