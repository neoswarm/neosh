/**
 * `@neosh/api/ui` — widgets built on the public API.
 *
 * Nothing here is privileged. Every line uses the same surface a third-party plugin has, which is
 * the point: if a picker could not be written this way, the API would be missing something. The
 * model switcher, the branch picker and the command palette are all this file plus a list.
 *
 * Import as:
 *
 * ```ts
 * import { picker, confirm, prompt } from "@neosh/api/ui";
 * ```
 */

import { byteLength, byteOffsets, clipToWidth, padToWidth, width } from "@neosh/api";
import type {
  BufferId,
  Disposable,
  DrawnMark,
  DrawnRow,
  FileChange,
  FileState,
  FloatOptions,
  KeyContext,
  MarkOptions,
  Neosh,
  WindowId,
} from "@neosh/api";

// ---------------------------------------------------------------------------
// Fuzzy matching
// ---------------------------------------------------------------------------

/** Where a query matched, so the caller can highlight it. */
export interface Match {
  score: number;
  /**
   * Indices of matched **code points**, ascending — not byte offsets and not UTF-16 indices.
   *
   * Code points because that is the unit a user perceives as "a character"; convert with
   * `byteOffsets` before handing them to `ns.mark`, which speaks bytes.
   */
  positions: number[];
}

/**
 * Subsequence match with a bias toward starts of words and runs of adjacent characters.
 *
 * Deliberately simple and deliberately *stable*: a picker that reorders under your fingers as you
 * type one more character is worse than one that ranks imperfectly.
 */
export function fuzzy(candidate: string, query: string): Match | null {
  if (query === "") return { score: 0, positions: [] };

  // Code points, not UTF-16 units: indexing a string directly splits an emoji in half and reports
  // a position no later stage can use.
  const hay = Array.from(candidate.toLowerCase());
  const needle = Array.from(query.toLowerCase());
  const positions: number[] = [];
  let score = 0;
  let from = 0;
  let previous = -2;

  for (const ch of needle) {
    if (ch === " ") continue; // spaces separate terms rather than needing to match
    const at = hay.indexOf(ch, from);
    if (at < 0) return null;
    positions.push(at);
    // Adjacent characters are worth much more than scattered ones, and a match at a word boundary
    // more still — "gs" should find "git status" ahead of "goals".
    if (at === previous + 1) score += 8;
    if (at === 0 || /[\s/_.\-]/.test(hay[at - 1] ?? "")) score += 6;
    score += 1;
    previous = at;
    from = at + 1;
  }
  // Shorter candidates win ties: an exact short name should beat a long one that contains it.
  score -= Math.floor(hay.length / 32);
  return { score, positions };
}

// ---------------------------------------------------------------------------
// Picker
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Widget keys
// ---------------------------------------------------------------------------

/**
 * What a key means to a widget.
 *
 * Named actions rather than keys, so a binding is a setting: `ui.keys.next` is a space-separated
 * list in Neovim notation, and adding `<C-j>` to it is one line of config rather than a fork of
 * this file.
 */
export type WidgetAction =
  | "next"
  | "prev"
  | "page_down"
  | "page_up"
  | "first"
  | "last"
  | "accept"
  | "dismiss"
  | "complete"
  | "clear"
  | "delete_word"
  /** Move to the next pane of a two-pane widget — the provider rail and the list beside it. */
  | "pane_next"
  | "pane_prev";

const ACTIONS: WidgetAction[] = [
  "next", "prev", "page_down", "page_up", "first", "last",
  "accept", "dismiss", "complete", "clear", "delete_word",
  "pane_next", "pane_prev",
];

/** One parsed key: a code kind, the character if it is one, and the modifiers that must match. */
interface KeySpec {
  kind: string;
  c?: string;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  /** The notation it came from, so it can be bound window-scoped as well as matched. */
  lhs: string;
}

/**
 * Parse one key in Neovim notation.
 *
 * The same notation `keymap.set` takes, so what a user writes in `ui.keys.next` looks like every
 * other binding they have written. Unparseable entries are dropped rather than thrown: a typo in a
 * setting should cost you that key, not the picker.
 */
function parseKey(spec: string): KeySpec | null {
  const trimmed = spec.trim();
  if (trimmed === "") return null;
  if (!trimmed.startsWith("<") || !trimmed.endsWith(">")) {
    return trimmed.length === 1
      ? { kind: "char", c: trimmed, ctrl: false, alt: false, shift: false, lhs: trimmed }
      : null;
  }
  let inner = trimmed.slice(1, -1);
  const out: KeySpec = { kind: "", ctrl: false, alt: false, shift: false, lhs: trimmed };
  for (;;) {
    const m = /^([CSAMD])-/i.exec(inner);
    if (!m) break;
    const which = m[1]!.toUpperCase();
    if (which === "C") out.ctrl = true;
    else if (which === "S") out.shift = true;
    else if (which === "A" || which === "M") out.alt = true;
    inner = inner.slice(2);
  }
  const named: Record<string, string> = {
    cr: "enter", enter: "enter", tab: "tab", bs: "backspace", del: "delete", esc: "esc",
    up: "up", down: "down", left: "left", right: "right", home: "home", end: "end",
    pageup: "page_up", pagedown: "page_down", space: "char", lt: "char", insert: "insert",
  };
  const lower = inner.toLowerCase();
  if (lower in named) {
    out.kind = named[lower]!;
    if (lower === "space") out.c = " ";
    if (lower === "lt") out.c = "<";
    // `<S-Tab>` is not tab with a modifier — a terminal sends a wholly different sequence, which
    // arrives as its own code. Written the way everyone writes it, matched the way it arrives.
    if (out.kind === "tab" && out.shift) {
      out.kind = "back_tab";
      out.shift = false;
    }
    return out;
  }
  if (inner.length === 1) {
    out.kind = "char";
    out.c = inner;
    return out;
  }
  return null;
}

function matches(spec: KeySpec, key: KeyContext["key"]): boolean {
  if (key.code.kind !== spec.kind) return false;
  if (spec.kind === "char") {
    const c = key.code.kind === "char" ? key.code.c : "";
    // Case-insensitively for control chords, because a terminal reports `<C-N>` and `<C-n>`
    // differently depending on whether shift happened to be down.
    if (spec.ctrl ? c.toLowerCase() !== (spec.c ?? "").toLowerCase() : c !== spec.c) return false;
  }
  if (key.mods.ctrl !== spec.ctrl || key.mods.alt !== spec.alt) return false;
  // Shift is only required when asked for: a terminal sets it for capital letters and for nothing
  // else consistently, so demanding its absence would break `<C-N>`.
  return !spec.shift || key.mods.shift;
}

let bindings: Map<WidgetAction, KeySpec[]> | null = null;

/**
 * Read the key settings once per session, and again whenever one changes.
 *
 * Cached because a picker asks this on every keystroke and an option read is a round trip to the
 * host. Invalidated by the change event rather than by a timer, so a rebinding takes effect on the
 * next key rather than the next minute.
 */
async function widgetKeys(neosh: Neosh): Promise<Map<WidgetAction, KeySpec[]>> {
  if (bindings) return bindings;
  const built = new Map<WidgetAction, KeySpec[]>();
  await Promise.all(
    ACTIONS.map(async (action) => {
      const raw = await neosh.opt.get<string>(`ui.keys.${action}`).catch(() => "");
      const specs = (raw ?? "")
        .split(/\s+/)
        .map(parseKey)
        .filter((k): k is KeySpec => k !== null);
      built.set(action, specs);
    }),
  );
  bindings = built;
  return built;
}

let watching = false;

/**
 * Start following `ui.keys.*` changes. Idempotent; called by every widget before it opens.
 *
 * The listener is never disposed on purpose — it belongs to the module, not to the widget that
 * happened to open first, and disposing it when that widget closed would leave the cache stale for
 * everyone else.
 */
function watchKeys(neosh: Neosh): void {
  if (watching) return;
  watching = true;
  neosh.opt.onChange((e) => {
    if (e.name.startsWith("ui.keys.")) bindings = null;
  });
}

/**
 * How to write the key bound to an action, the way a person would press it.
 *
 * Read from the setting rather than hard-coded, so a rebinding shows up in the affordance too —
 * the whole point of putting the key on screen is that it is the key that works.
 */
function keyLabel(keys: Map<WidgetAction, KeySpec[]>, action: WidgetAction): string {
  const spec = keys.get(action)?.[0];
  if (!spec) return "";
  const pretty: Record<string, string> = {
    "<Tab>": "⇥",
    "<S-Tab>": "⇧⇥",
    "<Left>": "←",
    "<Right>": "→",
    "<Up>": "↑",
    "<Down>": "↓",
    "<CR>": "↵",
    "<Esc>": "esc",
  };
  return pretty[spec.lhs] ?? spec.lhs.replace(/^<C-(.)>$/, "^$1").replace(/^<|>$/g, "");
}

/**
 * Which action a key press means, or `null` if it is ordinary input.
 *
 * `only` is the set of actions the asking widget can actually carry out, and it is not an
 * optimisation. Two actions share `<Tab>` on purpose — `complete` in a field with suggestions
 * under it, `pane_next` in a widget with two panes — on the reasoning that no widget has both. It
 * does not have both, but it does *resolve* both: without this filter the first one in `ACTIONS`
 * wins for everybody, and `<Tab>` in the model picker resolved to a completion the picker has no
 * case for, so it silently did nothing at all and the second pane had no way in.
 */
function actionFor(
  keys: Map<WidgetAction, KeySpec[]>,
  key: KeyContext["key"],
  only?: readonly WidgetAction[],
): WidgetAction | null {
  for (const action of only ?? ACTIONS) {
    for (const spec of keys.get(action) ?? []) {
      if (matches(spec, key)) return action;
    }
  }
  return null;
}

/**
 * The key strip a single-pane picker gets unless the caller writes its own.
 *
 * Built from the bindings rather than written out, because the point of putting a key on screen is
 * that it is the key that works — a legend that goes stale the moment somebody rebinds `ui.keys.*`
 * is worse than no legend, since it is believed.
 */
function defaultHints(keys: Map<WidgetAction, KeySpec[]>, filtering: boolean): string {
  const parts = [
    `${keyLabel(keys, "accept")} choose`,
    `${keyLabel(keys, "prev")}/${keyLabel(keys, "next")} move`,
  ];
  if (filtering) parts.push("type to filter");
  parts.push(`${keyLabel(keys, "dismiss")} close`);
  return parts.join("   ");
}

/** What a two-pane widget answers to. Notably not `complete`, whose key it shares. */
const RAIL_ACTIONS: readonly WidgetAction[] = [
  "dismiss", "accept", "pane_next", "pane_prev", "next", "prev",
  "page_down", "page_up", "first", "last", "clear", "delete_word",
];

export interface PickerItem<T> {
  /** What the user reads and what the filter matches against. */
  label: string;
  /** Dimmed, right of the label. A path, a description, a model id. */
  detail?: string;
  /** Extra text the filter should match but that is not shown. */
  keywords?: string;
  /**
   * One column of glyph before the label, coloured by `hl`.
   *
   * What it is for is telling *kinds* of row apart at a glance — a branch from a machine from a
   * directory — in a list where the labels alone read as one undifferentiated column. A picker
   * where only some rows have one still aligns: the ones without get the same gutter, so the
   * labels line up and the icons read as a column rather than as ragged punctuation.
   *
   * **One column.** The runtime cannot measure display width — only `neosh-tui` can — so a
   * two-column emoji here shifts that row's label right by one. Highlighting stays correct
   * regardless, because every offset is computed in bytes from the text actually written.
   */
  icon?: string;
  /**
   * The highlight group for `icon`. A palette name — `Git.Branch`, `Diagnostic.Ok`,
   * `Sidebar.Remote` — never a colour, so a row follows the theme like everything else.
   */
  hl?: string;
  value: T;
}

export interface PickerOptions<T> {
  title?: string;
  /** Shown when the list is empty or nothing matches. */
  placeholder?: string;
  /** Start with the cursor on this index. */
  selected?: number;
  /** Rows of list to show, before the title and filter lines. */
  height?: number;
  width?: number;
  /**
   * Called whenever the highlighted row changes. Use for a live preview — an "intelligence" picker
   * that describes each effort level, a branch picker that shows the tip commit.
   */
  onHighlight?(item: PickerItem<T>, index: number): void;
  /**
   * Where the rows come from, when they depend on what has been typed.
   *
   * With a source, the list is *replaced* on every keystroke instead of fuzzy-filtered — which is
   * what you want when the candidates cannot all be fetched up front: a directory listing, a
   * search, anything that asks something else a question. Without one, `items` is filtered locally.
   *
   * Calls are debounced and the answer is dropped if you have typed again since, so a slow source
   * cannot make the list flicker backwards.
   */
  source?(query: string): Promise<PickerItem<T>[]>;
  /**
   * What the filter starts with.
   *
   * A function is asked at the last possible moment — once the picker is on screen and holds the
   * keyboard — rather than when it was called. That matters for anything completing a field that
   * is still being typed into: opening this takes several round-trips, every keystroke in that
   * window goes to the field, and a picker seeded with what was there *before* them shows an
   * unfiltered list with the wrong row under the accept key. Typing `/compact` quickly and
   * pressing `↵` ran the first command in the list, which was not `compact`.
   */
  query?: string | (() => string);
  /**
   * Whether there is a filter line at all. On by default.
   *
   * Off for a list you pick from rather than search — a yes/no question does not have a text field
   * in it, and drawing one invites the reader to type into something that will not answer.
   */
  filter?: boolean;
  /**
   * What `accept` resolves to when nothing is highlighted, given what was typed.
   *
   * Lets a picker double as a field: a path picker offers completions but must still accept a path
   * you typed in full, which is not one of them.
   */
  freeform?(query: string): T | null;
  /**
   * The key strip at the foot. One line.
   *
   * Written out by default, from the keys actually bound, so a rebinding shows up here rather than
   * making the row a lie. Pass `""` for a picker with nothing worth saying — a two-row yes/no
   * question is not helped by being told that `↵` chooses.
   */
  hints?: string;
  /**
   * Where the float goes. The middle of the screen unless you say otherwise.
   *
   * `{ kind: "dock", dock: "bottom" }` with a negative row offset is how a *completion* is placed:
   * against the message field, above it, rather than over the transcript it has nothing to do with.
   */
  anchor?: FloatOptions["anchor"];
  /** Shifted from the anchor. Negative rows go up. */
  offset?: { row: number; col: number };
  /**
   * How the filter matches.
   *
   * `"fuzzy"` — the default — scores a subsequence match over the label *and* `keywords`, which is
   * what you want in a search box: you are looking for something and half-remember a word from its
   * description.
   *
   * `"name"` matches the start of the label first, then anywhere in it, and never looks at
   * `keywords` at all. That is what a **command menu** needs, and the difference is not cosmetic.
   * Typing `compact` into a fuzzy list of every command matches half of them — `c`…`o`…`m`…`p`…
   * are letters that occur in that order in almost any English sentence, and descriptions are
   * sentences — so the top row is something unrelated and `↵` runs it. A menu whose accept key
   * does an arbitrary thing is worse than one with no matching at all.
   */
  match?: "fuzzy" | "name";
  /**
   * The filter changed.
   *
   * The hook that lets a picker stay in step with something outside itself — a completion that
   * mirrors what is typed back into the composer, so the field says what you typed and dismissing
   * the list leaves it there.
   */
  onQuery?(query: string): void;
  /**
   * Keys `onKey` wants that a *binding elsewhere* would otherwise take.
   *
   * The raw capture only receives what no keymap claimed, so a caller reaching for `<C-x>` gets
   * nothing — that key belongs to the composer. Listing it here binds it to this widget for as long
   * as the widget is open, and drops it again with the window.
   *
   * A picker with a filter should only ever ask for chords. A bare letter taken here is a letter the
   * filter can never contain, which is how a list of conversations ends up unable to search for one
   * with an `x` in its name.
   */
  ownKeys?: readonly string[];
  /**
   * A key the caller wants for itself, checked before the widget's own.
   *
   * Return `"close"` to dismiss, `"reload"` to re-read the rows, `"handled"` to redraw, and nothing
   * at all to let the widget have the key. This is what puts a verb on a row — unarchive, delete,
   * rename — without a second modal in front of the list you were reading.
   *
   * `"reload"` re-runs `source` when there is one, and otherwise re-ranks `items`: the very array
   * you passed in, so a caller that mutates it in place gets a list that has caught up with what it
   * just did. A row acted on is a row that has to leave, and closing the list and opening it again
   * loses your place in it.
   */
  onKey?(
    key: KeyContext,
    ctx: { item: T | undefined; query: string },
  ): Promise<"handled" | "reload" | "close" | undefined> | "handled" | "reload" | "close" | undefined;
}

const NS = "neosh.ui.picker";

/**
 * The row marker, and its width **in bytes**.
 *
 * `\u276f` is one column wide but three bytes in UTF-8, so the two markers are the same width on
 * screen and different lengths in the buffer. Every mark offset past the marker is computed from
 * the actual prefix rather than from its display width — the exact confusion this API's byte
 * columns exist to prevent.
 */
const CURSOR_MARKER = "\u276f ";
const BLANK_MARKER = "  ";

/**
 * A filterable list in a float. Resolves to the chosen value, or `null` if dismissed.
 *
 * Keys: printable characters filter, `<BS>` deletes, `↑`/`↓` and `<C-p>`/`<C-n>` move, `<CR>`
 * accepts, `<Esc>` dismisses. The float is **modal**, so nothing else reaches the workspace while
 * it is up — `^N` over an open picker used to start a new conversation behind it — bar the keys in
 * `ui.modal_escape_keys`, which is `<C-q>` and `<C-r>` unless you have said otherwise.
 *
 * Only one picker may be open per plugin at a time; opening a second dismisses the first, because
 * two modal lists competing for the keyboard is never what was meant.
 */
export async function picker<T>(
  neosh: Neosh,
  items: PickerItem<T>[],
  opts: PickerOptions<T> = {},
): Promise<T | null> {
  const height = Math.max(1, opts.height ?? 12);
  const width = Math.max(20, opts.width ?? 64);
  const filtering = opts.filter !== false;

  const buf = await neosh.buf.create({ name: `[${opts.title ?? "picker"}]`, scratch: true });
  const ns = await neosh.ns.create(NS);

  watchKeys(neosh);
  const keys = await widgetKeys(neosh);
  // A picker with no keys on it is a modal you have to guess your way out of. Every one of these
  // is a list you arrived at by pressing something, and none of them said what to press next.
  const hints = opts.hints ?? defaultHints(keys, filtering);

  const win = await neosh.float.open(buf, {
    anchor: opts.anchor ?? { kind: "screen" },
    offset: opts.offset,
    width: { kind: "fixed", n: width },
    // Exactly the rows that get drawn: the list, plus a filter line when there is one, a title
    // when there is one, and the key strip unless it was waived.
    height: {
      kind: "fixed",
      n: height + (filtering ? 1 : 0) + (opts.title ? 1 : 0) + (hints === "" ? 0 : 1),
    },
    border: "rounded",
    focusable: true,
    closeOnBlur: true,
    // Modal: nothing global resolves while this is up. Shadowing the keys a widget wants — which
    // is what `bindWidgetKeys` does and still does — only ever covered the keys it uses; `^T`,
    // `^G`, `^L` and the rest fell straight through and opened a second panel behind this one,
    // with focus somewhere neither of them expected. `^Q` and `^R` still work, so a widget that
    // fails to bind a way out is never a terminal somebody has to kill: see
    // `ui.modal_escape_keys`.
    modal: true,
    z: 200,
  });

  let query = typeof opts.query === "string" ? opts.query : "";
  let cursor = Math.min(Math.max(0, opts.selected ?? 0), Math.max(0, items.length - 1));
  let visible = items.map((item, index) => ({ item, index, positions: [] as number[] }));
  let top = 0;

  const rank = (pool: PickerItem<T>[], q: string) => {
    const scored = pool
      .map((item, index) => {
        if (opts.match === "name") return byName(item, index, q);
        const hay = item.keywords ? `${item.label} ${item.keywords}` : item.label;
        const m = fuzzy(hay, q);
        return m ? { item, index, positions: m.positions, score: m.score } : null;
      })
      .filter((x): x is NonNullable<typeof x> => x !== null);
    // Stable within equal scores: the original order carries meaning (recency, configured order),
    // and re-sorting it away makes the list jump for no visible reason.
    scored.sort((a, b) => b.score - a.score || a.index - b.index);
    return scored;
  };

  const refilter = () => {
    visible = rank(items, query);
    cursor = Math.min(cursor, Math.max(0, visible.length - 1));
  };

  /**
   * A row matched by the start of its name, then by any part of it.
   *
   * Two tiers and nothing else, because the whole point of this mode is that it is *predictable*:
   * you can tell from what you have typed which row you are about to accept. Longer names score
   * lower within a tier so an exact `git.diff` outranks `git.diff.staged`, and the highlighted
   * positions are the run that matched, which is where the eye is already looking.
   */
  function byName(item: PickerItem<T>, index: number, q: string) {
    if (!q) return { item, index, positions: [] as number[], score: 0 };
    const label = item.label.toLowerCase();
    const needle = q.toLowerCase();
    const at = label.indexOf(needle);
    if (at < 0) return null;
    const positions = Array.from({ length: needle.length }, (_, k) => at + k);
    return { item, index, positions, score: (at === 0 ? 1000 : 500) - label.length };
  }

  // Which fetch is current. A slow source answering after you have typed again would replace a
  // newer list with an older one, and the row under your finger would change out from under it.
  let generation = 0;
  // The fetch in flight, so accepting can wait for it. Typing faster than the source answers is
  // ordinary — anyone pasting a path does it — and taking the row that was under the cursor two
  // keystrokes ago is not a near miss, it is a different directory.
  let inflight: Promise<void> = Promise.resolve();
  const refetch = () => {
    if (!opts.source) {
      refilter();
      return inflight;
    }
    const mine = ++generation;
    inflight = opts
      .source(query)
      .catch(() => [] as PickerItem<T>[])
      .then((fetched) => {
        if (mine !== generation) return;
        // Already the answer to this query, so ranked against nothing rather than filtered twice.
        visible = fetched.map((item, index) => ({ item, index, positions: [] as number[] }));
        cursor = Math.min(cursor, Math.max(0, visible.length - 1));
      });
    return inflight;
  };

  const render = async () => {
    // A burst of keys — anyone typing at speed, or a paste — arrives as several invocations of the
    // key command at once, and any of them may be the one that accepts and closes. Whichever runs
    // next would then be drawing into a window that is gone, which is not an error worth reporting
    // to the user: it is this widget racing itself.
    if (closed) return;
    // Keep the cursor on screen without recentring on every keystroke.
    if (cursor < top) top = cursor;
    if (cursor >= top + height) top = cursor - height + 1;

    const lines: string[] = [];
    if (opts.title) lines.push(opts.title);
    if (filtering) lines.push(`> ${query}`);

    // The width the float was asked for *is* the width of its text: a border is drawn outside it,
    // which is why `columnWidth` subtracts one from each side before opening a panel as wide as the
    // dock. Measuring against anything else puts the last visible character on both lines — the
    // continuation says `/ finds` under a row that already ended in `/`.
    const inner = Math.max(8, width);
    let window = visible.slice(top, top + height);
    // One gutter for the whole list, or none at all. Giving it only to the rows that asked for an
    // icon would step every other label one column left, which reads as a list that cannot decide
    // where its left margin is.
    const gutter = window.some((r) => r.item.icon) ? 2 : 0;
    const iconFor = (item: PickerItem<T>) =>
      gutter === 0 ? "" : item.icon ? `${item.icon} ` : "  ";
    const textOf = (row: typeof window[number], on: boolean) =>
      `${on ? "❯ " : "  "}${iconFor(row.item)}${row.item.label}${row.item.detail ? `  ${row.item.detail}` : ""}`;

    // The row under the cursor says all of itself. A picker is a list of things you are choosing
    // between, and two rows whose difference is past the right edge are two rows you cannot choose
    // between — which is exactly what a long model id, a path, or a description does here. The
    // rest goes underneath, only while the cursor is on it, so the list is still a list.
    const on = visible[cursor];
    /** The cursor row's first line, which is re-broken at a word when the row has to continue. */
    let firstLine = on ? textOf(on, true) : "";
    let rest: string[] = [];
    if (on && columnsOf(firstLine) > inner) {
      const head = `❯ ${iconFor(on.item)}${on.item.label}`;
      const detail = on.item.detail ? `  ${on.item.detail}` : "";
      // Only the detail is re-broken. The label is what the filter matched and what the match
      // highlight is measured against, so it stays exactly where it was written; a row whose label
      // alone overflows falls back to continuing from wherever the edge cut it.
      if (detail !== "" && columnsOf(head) + 8 < inner) {
        const [take, remainder] = takeWords(detail, inner - columnsOf(head));
        firstLine = `${head}${take}`;
        rest = remainder.trim() === "" ? [] : wrapToWidth(remainder.trim(), inner - 4);
      } else {
        rest = overflowOf(clipToWidth(firstLine, inner), firstLine, inner - 4);
      }
    }
    // The unfolded row pays for its own continuation out of the list's rows rather than out of the
    // float's height: the window was sized for `height` lines and growing past it would push the
    // key strip off the bottom, which is the row that says how to get out.
    if (rest.length > 0) {
      const room = Math.max(1, height - rest.length);
      if (cursor < top) top = cursor;
      if (cursor >= top + room) top = cursor - room + 1;
      window = visible.slice(top, top + room);
    }

    if (window.length === 0) {
      lines.push(`  ${opts.placeholder ?? "no matches"}`);
    }
    /** Which of `lines` each visible row starts on, relative to the first list row. */
    const lineFor: number[] = [];
    const firstListLine = lines.length;
    for (const row of window) {
      const isCursor = row.index === visible[cursor]?.index;
      lineFor.push(lines.length - firstListLine);
      lines.push(isCursor ? firstLine : textOf(row, false));
      if (!isCursor) continue;
      for (const line of rest) lines.push(`    ${line}`);
    }
    // Pushed onto the last row rather than floated: the float is sized for it, and a strip that
    // moved up as the list shortened would be a strip you have to look for.
    //
    // The placeholder takes a row of the list's own space, so an empty list has used one of the
    // `height` rows and not none. Counting it as none put the strip one row past the bottom of a
    // float sized for exactly `height`, where it was silently clipped — leaving the empty state,
    // the one state where you most want to be told what the keys do, as the only one with no keys
    // on it.
    // Lines, not rows: the row under the cursor is more than one of them when it has unfolded, and
    // counting rows here would leave the strip that many lines low — off the bottom of a float
    // sized for exactly `height`.
    const listRows = Math.max(1, lines.length - firstListLine);
    const hintLine = hints === "" ? -1 : lines.length + Math.max(0, height - listRows);
    if (hintLine >= 0) {
      while (lines.length < hintLine) lines.push("");
      lines.push(` ${hints}`);
    }
    // Marks are collected against their row and handed over with the text, in one call. Set one at
    // a time they were a sequence the frontend could draw the middle of: the moment the clear had
    // landed and the marks had not, every row drew unmarked — in `Normal`, which is near-white.
    const drawn: DrawnRow[] = lines.map((text) => ({ text, marks: [] }));
    const mark = (line: number, col: number, o: MarkOptions) => {
      drawn[line]?.marks!.push({ col, opts: o });
    };

    if (hintLine >= 0) {
      mark(hintLine, 0, { hlGroup: "Sidebar.Dim", endCol: byteLength(lines[hintLine] ?? "") });
    }
    const listTop = (opts.title ? 1 : 0) + (filtering ? 1 : 0);
    for (let i = 0; i < window.length; i++) {
      const row = window[i]!;
      const line = listTop + (lineFor[i] ?? i);
      const isCursor = row.index === visible[cursor]?.index;
      const eol = byteLength(lines[line] ?? "");
      if (isCursor) {
        mark(line, 0, { hlGroup: "Picker.Selected", endCol: eol });
        // The band covers the continuation too, so an unfolded row reads as one row that is
        // several lines tall rather than as a selected row with loose text under it.
        for (let k = 1; k <= rest.length; k++) {
          mark(line + k, 0, { hlGroup: "Picker.Selected", endCol: byteLength(lines[line + k] ?? "") });
        }
      }
      const icon = iconFor(row.item);
      const marker = byteLength(isCursor ? CURSOR_MARKER : BLANK_MARKER);
      // The icon is painted before the match runs, and at no priority: a match highlight over the
      // label is the thing the eye is looking for, and nothing here should be able to outrank it.
      if (row.item.icon && row.item.hl) {
        mark(line, marker, {
          hlGroup: row.item.hl,
          endCol: marker + byteLength(row.item.icon),
        });
      }
      const prefix = marker + byteLength(icon);
      const offsets = byteOffsets(row.item.label);
      for (const at of row.positions) {
        const start = offsets[at];
        const end = offsets[at + 1];
        // A match position past the label came from `keywords`, which is filtered on but not
        // shown; there is nothing on screen to highlight.
        if (start === undefined || end === undefined) continue;
        mark(line, prefix + start, {
          hlGroup: "Picker.Match",
          endCol: prefix + end,
          priority: 200,
        });
      }
      if (row.item.detail) {
        mark(line, prefix + byteLength(row.item.label), {
          hlGroup: "Picker.Detail",
          endCol: eol,
        });
      }
    }
    if (opts.title) {
      mark(0, 0, { hlGroup: "Float.Title", endCol: byteLength(opts.title) });
    }
    await neosh.buf.render(buf, ns, 0, -1, drawn);

    // The caret goes where you are typing. Without this the terminal cursor stays parked at the
    // top-left of the float and the field reads as inert — you type and nothing appears to be
    // listening, even though the text is right there. With no filter there is nowhere to type, so
    // it marks the row instead.
    await neosh.win.setCursor(
      win,
      filtering ? (opts.title ? 1 : 0) : listTop + (lineFor[cursor - top] ?? cursor - top),
      filtering ? byteLength(`> ${query}`) : 0,
    );
  };

  let settle: (v: T | null) => void = () => {};
  const done = new Promise<T | null>((resolve) => {
    settle = resolve;
  });

  const command = `${NS}.key.${++pickerSeq}`;
  const disposers: Disposable[] = [];
  let closed = false;
  const close = async (value: T | null) => {
    if (closed) return;
    closed = true;
    for (const d of disposers) d.dispose();
    await neosh.win.close(win).catch(() => {});
    settle(value);
  };

  const last = () => Math.max(0, visible.length - 1);

  /**
   * Change the filter, and tell whoever is watching.
   *
   * One place, because four separate cases used to do it themselves and each one had to remember
   * the refetch. A completion also has to say so outwards, so the composer it is completing keeps
   * showing what has been typed.
   */
  const retype = async (next: string) => {
    query = next;
    await refetch();
    opts.onQuery?.(query);
  };

  disposers.push(
    await neosh.cmd.register(command, async (_args, key) => {
      if (!key || closed) return;
      const before = cursor;
      // The caller first: a verb it put on a key is a verb about the row under the cursor, and the
      // widget's own handling of that key would be about the filter.
      const outcome = await opts.onKey?.(key, { item: visible[cursor]?.item.value, query });
      if (closed) return;
      if (outcome === "close") {
        await close(null);
        return;
      }
      if (outcome === "reload") {
        await refetch();
        await render();
        return;
      }
      if (outcome === "handled") {
        await render();
        return;
      }
      // Settings next, so a rebound key wins over what the widget would otherwise do with it.
      const action = actionFor(keys, key.key);
      switch (action) {
        case "dismiss":
          await close(null);
          return;
        case "accept": {
          // Settle whatever is in flight first, so this takes the row for what is actually typed.
          await inflight;
          const chosen = visible[cursor];
          if (chosen) {
            await close(chosen.item.value);
            return;
          }
          // Nothing highlighted: a picker that is also a field accepts what you typed.
          await close(opts.freeform?.(query) ?? null);
          return;
        }
        case "next":
          cursor = Math.min(last(), cursor + 1);
          break;
        case "prev":
          cursor = Math.max(0, cursor - 1);
          break;
        case "page_down":
          cursor = Math.min(last(), cursor + height);
          break;
        case "page_up":
          cursor = Math.max(0, cursor - height);
          break;
        case "first":
          cursor = 0;
          break;
        case "last":
          cursor = last();
          break;
        case "clear":
          await retype("");
          break;
        case "delete_word":
          await retype(dropSegment(query));
          break;
        case "complete": {
          // Take the highlighted row into the field without accepting it — how you walk into a
          // directory one segment at a time.
          await inflight;
          const chosen = visible[cursor];
          if (!chosen || !opts.freeform) return;
          cursor = 0;
          await retype(chosen.item.label);
          break;
        }
        default: {
          if (!filtering) return;
          if (key.key.code.kind === "backspace") {
            // Backspacing off the end of an empty filter dismisses, so a completion feels like
            // part of the field rather than a window in front of it. Only where there is something
            // outside to fall back to — a picker with nowhere to put the keystroke keeps it.
            if (!query && opts.onQuery) {
              await close(null);
              return;
            }
            await retype(query.slice(0, -1));
            break;
          }
          if (key.key.code.kind !== "char" || key.key.mods.ctrl || key.key.mods.alt) return;
          cursor = 0;
          await retype(query + key.key.code.c);
          break;
        }
      }
      await render();
      if (cursor !== before && opts.onHighlight) {
        const row = visible[cursor];
        if (row) opts.onHighlight(row.item, row.index);
      }
    }, { desc: "picker key" }),
  );

  await neosh.focus.push(win);
  disposers.push(await neosh.keymap.capture(win, command));
  // `<C-c>` is bound globally to `interrupt`, which would arm "press again to quit" while a picker
  // is on screen — the one moment the user obviously meant "close this". A window-scoped binding
  // outranks the global one and is dropped with the window.
  await bindWidgetKeys(neosh, win, command, keys, opts.ownKeys ?? []);
  // Now, and not before: everything typed while this was being built went somewhere, and if it
  // went into the field this is completing then it is part of the query. Asked after the keyboard
  // is ours, so there is no further keystroke to miss.
  if (typeof opts.query === "function") query = opts.query();
  await refetch();
  await render();
  if (opts.onHighlight) {
    const row = visible[cursor];
    if (row) opts.onHighlight(row.item, row.index);
  }

  return done;
}

let pickerSeq = 0;

/**
 * Delete one word, treating a path separator as a boundary.
 *
 * `/home/me/src/` becomes `/home/me/`, not `/home/me/src`. Deleting the separator and stopping
 * would mean pressing the key twice per directory, which is not what anyone means by "back one".
 */
function dropSegment(text: string): string {
  const trimmed = text.replace(/[\s/]+$/, "");
  const at = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf(" "));
  return at < 0 ? "" : trimmed.slice(0, at + 1);
}

// ---------------------------------------------------------------------------
// Derived widgets
// ---------------------------------------------------------------------------

export interface ConfirmOptions {
  /** What the affirming answer says. A verb — `Delete`, `Remove`, `Sign out` — never `OK`. */
  yes?: string;
  /** What the answer that changes nothing says. */
  no?: string;
  /**
   * Under the question, dimmed: what exactly is at stake, and what to do instead.
   *
   * The part that makes a dialog worth stopping for. "Are you sure?" is a speed bump you learn to
   * clear without reading; "4 messages, in neosh · main" and "archiving keeps it" is a question you
   * can answer without leaving the dialog to go and check.
   */
  detail?: string[];
  /**
   * The affirming answer cannot be undone.
   *
   * Starts on the answer that changes nothing — `<CR>` is reflex by the second time you have seen a
   * dialog, and a reflex must not delete anything — and draws the other one in the theme's error
   * colour, so the row that destroys something does not look like the row beside it.
   */
  dangerous?: boolean;
  width?: number;
}

const CONFIRM_NS = "neosh.ui.confirm";

/**
 * Ask a yes/no question, and mean it. Resolves `false` when dismissed.
 *
 * A dialog rather than a two-row picker, because the picker's title is one clipped line and the
 * question is the whole point: what is about to happen, to what, and what the alternative is. It
 * wraps, it carries `detail`, and when the answer is destructive it says so in colour rather than
 * relying on you to read a verb.
 *
 * `y` and `n` answer it outright, `↑`/`↓` and `j`/`k` move between the two, `<CR>` takes the one
 * under the cursor, and `<Esc>` — like every other float in the workspace — means no.
 */
export async function confirm(
  neosh: Neosh,
  question: string,
  opts: ConfirmOptions = {},
): Promise<boolean> {
  const yes = opts.yes ?? "Yes";
  const no = opts.no ?? "No";
  const width = Math.min(78, Math.max(36, opts.width ?? 58));
  const asked = wrapText(question, width - 2);
  const detail = (opts.detail ?? []).flatMap((d) => wrapText(d, width - 2));
  const answers = [yes, no];
  const strip = clipTo(
    `y ${yes.toLowerCase()}   n ${no.toLowerCase()}   ↵ choose   esc cancel`,
    width - 2,
  );

  // On the answer that changes nothing, when the other one cannot be taken back.
  let cursor = opts.dangerous ? 1 : 0;

  const buf = await neosh.buf.create({ name: "[confirm]", scratch: true });
  const ns = await neosh.ns.create(CONFIRM_NS);

  watchKeys(neosh);
  const keys = await widgetKeys(neosh);

  /** Where each part of the dialog starts, so the marks do not have to count rows twice. */
  const detailAt = asked.length + 1;
  const answersAt = detailAt + (detail.length === 0 ? 0 : detail.length + 1);
  const stripAt = answersAt + answers.length + 1;

  const win = await neosh.float.open(buf, {
    anchor: { kind: "screen" },
    width: { kind: "fixed", n: width },
    height: { kind: "fixed", n: stripAt + 1 },
    border: "rounded",
    focusable: true,
    closeOnBlur: true,
    // Modal: nothing global resolves while this is up. Shadowing the keys a widget wants — which
    // is what `bindWidgetKeys` does and still does — only ever covered the keys it uses; `^T`,
    // `^G`, `^L` and the rest fell straight through and opened a second panel behind this one,
    // with focus somewhere neither of them expected. `^Q` and `^R` still work, so a widget that
    // fails to bind a way out is never a terminal somebody has to kill: see
    // `ui.modal_escape_keys`.
    modal: true,
    // Above a picker: this is asked *from* one often enough that being drawn under it would make
    // the workspace look wedged.
    z: 260,
  });

  const render = async () => {
    const lines: string[] = asked.map((l) => ` ${l}`);
    lines.push("");
    if (detail.length > 0) {
      lines.push(...detail.map((l) => ` ${l}`));
      lines.push("");
    }
    answers.forEach((a, i) => lines.push(`${i === cursor ? `${CURSOR_MARKER}` : BLANK_MARKER}${a}`));
    lines.push("");
    lines.push(` ${strip}`);
    // Text and marks together, in one call: this is redrawn on every keystroke, and a repaint the
    // frontend can draw the middle of is one that flashes — unmarked rows draw in `Normal`. It also
    // takes care of the older half of the same bug, that a mark whose line was replaced under it
    // clamps rather than dies, so rows would end up wearing the colours of the ones before.
    const drawn: DrawnRow[] = lines.map((text) => ({ text, marks: [] }));
    const mark = (line: number, col: number, o: MarkOptions) => {
      drawn[line]?.marks!.push({ col, opts: o });
    };

    for (let i = 0; i < asked.length; i++) {
      mark(i, 0, { hlGroup: "Title", endCol: byteLength(lines[i] ?? "") });
    }
    for (let i = 0; i < detail.length; i++) {
      mark(detailAt + i, 0, {
        hlGroup: "Comment",
        endCol: byteLength(lines[detailAt + i] ?? ""),
      });
    }
    for (let i = 0; i < answers.length; i++) {
      const line = answersAt + i;
      // The band is the row's *background* rather than a group across its bytes, so the answer keeps
      // whatever colour said what it was. A ranged group here would leave the destructive row
      // looking exactly like the safe one for as long as the cursor is on it.
      if (i === cursor) {
        mark(line, 0, { lineHlGroup: "Picker.Selected" });
      }
      if (opts.dangerous && i === 0) {
        mark(line, byteLength(BLANK_MARKER), {
          hlGroup: "Diagnostic.Error",
          endCol: byteLength(lines[line] ?? ""),
        });
      }
    }
    mark(stripAt, 0, { hlGroup: "Sidebar.Dim", endCol: byteLength(lines[stripAt] ?? "") });
    await neosh.buf.render(buf, ns, 0, -1, drawn);
    // The caret marks the answer, since there is nothing here to type into.
    await neosh.win.setCursor(win, answersAt + cursor, 0);
  };

  let settle: (v: boolean) => void = () => {};
  const done = new Promise<boolean>((resolve) => {
    settle = resolve;
  });

  const command = `${CONFIRM_NS}.key.${++pickerSeq}`;
  const disposers: Disposable[] = [];
  let closed = false;
  const close = async (value: boolean) => {
    if (closed) return;
    closed = true;
    for (const d of disposers) d.dispose();
    await neosh.win.close(win).catch(() => {});
    settle(value);
  };

  disposers.push(
    await neosh.cmd.register(command, async (_args, key) => {
      if (!key || closed) return;
      switch (actionFor(keys, key.key)) {
        case "dismiss":
          await close(false);
          return;
        case "accept":
          await close(cursor === 0);
          return;
        case "next":
        case "last":
          cursor = answers.length - 1;
          break;
        case "prev":
        case "first":
          cursor = 0;
          break;
        default: {
          if (key.key.code.kind !== "char" || key.key.mods.ctrl || key.key.mods.alt) return;
          switch (key.key.code.c.toLowerCase()) {
            // Answering outright, which is what anyone who has read the question wants. Nothing here
            // filters, so the letters are free — and `y`/`n` are the two nobody has to be told.
            case "y":
              await close(true);
              return;
            case "n":
            case "q":
              await close(false);
              return;
            case "j":
              cursor = answers.length - 1;
              break;
            case "k":
              cursor = 0;
              break;
            default:
              return;
          }
        }
      }
      await render();
    }, { desc: "confirm key" }),
  );

  await neosh.focus.push(win);
  disposers.push(await neosh.keymap.capture(win, command));
  await bindWidgetKeys(neosh, win, command, keys);
  await render();
  return done;
}

/**
 * Ask before something that cannot be undone — unless the user has said not to.
 *
 * One place, so `ui.confirm_destructive` means the same thing everywhere and a plugin does not have
 * to know the option exists. The bar is *irreversible*, not merely significant: closing a panel,
 * switching a model or archiving a conversation asks nothing, because you can put those back. A
 * dialog charged for a reversible action is what teaches people to clear dialogs without reading
 * them, which is how the one that matters stops working.
 */
export async function confirmDestructive(
  neosh: Neosh,
  question: string,
  opts: Omit<ConfirmOptions, "dangerous"> = {},
): Promise<boolean> {
  const ask = (await neosh.opt.get<boolean>("ui.confirm_destructive").catch(() => true)) ?? true;
  if (!ask) return true;
  return confirm(neosh, question, { ...opts, dangerous: true });
}

/**
 * Break text into lines of at most `width` characters.
 *
 * Counted in characters and not columns: measuring display width is the frontend's job, and this
 * feeds a float that is sized with a column to spare. A word longer than the line is left long and
 * clipped where it is drawn, by the one thing that knows where its own edge is.
 */
function wrapText(text: string, width: number): string[] {
  const out: string[] = [];
  for (const paragraph of text.split("\n")) {
    let line = "";
    for (const word of paragraph.split(/\s+/).filter((w) => w !== "")) {
      const next = line === "" ? word : `${line} ${word}`;
      if (line !== "" && Array.from(next).length > width) {
        out.push(line);
        line = word;
      } else {
        line = next;
      }
    }
    out.push(line);
  }
  return out;
}

function clipTo(text: string, width: number): string {
  const chars = Array.from(text);
  return chars.length <= width ? text : `${chars.slice(0, Math.max(1, width - 1)).join("")}…`;
}

// ---------------------------------------------------------------------------
// Showing the rest of a row
// ---------------------------------------------------------------------------

/**
 * Break `text` into lines of at most `columns` **display columns**, splitting words that are wider
 * than the column rather than letting them overflow it.
 *
 * The difference from {@link wrapText} is the whole reason both exist. That one counts code points
 * and leaves a long word long, because it feeds a float that is sized with a column to spare and
 * the frontend clips the overhang. This one feeds a *column of a row* — a description beside a
 * label, a continuation under a title — where an overhanging word does not get clipped, it pushes
 * whatever is to its right off the edge and takes the alignment of every row below it with it. A
 * URL, a path or a `--flag=value` is routinely wider than the column it lands in, so this is the
 * common case and not the exotic one.
 *
 * Measured with {@link width}, which is the op the renderer itself uses — so a CJK label and an
 * ASCII one wrap at the same visual place.
 */
export function wrapToWidth(text: string, columns: number): string[] {
  const limit = Math.max(1, Math.floor(columns));
  const out: string[] = [];
  for (const paragraph of text.split("\n")) {
    let line = "";
    for (const word of paragraph.split(/\s+/).filter((w) => w !== "")) {
      const next = line === "" ? word : `${line} ${word}`;
      if (width(next) <= limit) {
        line = next;
        continue;
      }
      if (line !== "") {
        out.push(line);
        line = "";
      }
      // A word that cannot fit a line of its own is cut across as many as it needs. Cutting is
      // right here and wrong in prose: this is an identifier, not a sentence, and the alternative
      // is not a nicer break but a row that is silently wider than its column.
      let rest = word;
      while (width(rest) > limit) {
        const head = clipToWidth(rest, limit);
        if (head === "") break;
        out.push(head);
        rest = rest.slice(head.length);
      }
      line = rest;
    }
    out.push(line);
  }
  return out.length > 0 ? out : [""];
}

/**
 * The part of `full` that `shown` did not get to say, as lines of `columns` columns.
 *
 * `shown` is the row as it was actually written — clipped, and ending in an ellipsis if it was.
 * The ellipsis is dropped before comparing, so what comes back starts exactly where the visible
 * text stopped, and the two read as one sentence broken across a line.
 *
 * Empty when nothing was lost, which is the common case and has to cost nothing: a list of short
 * rows must not grow a blank line under the cursor.
 */
export function overflowOf(shown: string, full: string, columns: number): string[] {
  if (full === "" || columns < 1) return [];
  // Everything before the ellipsis is what actually reached the screen. What comes *after* it is
  // decoration that survived the clip — an unread dot, a count, a state glyph pinned to the end of
  // the row — and it is not part of the text, so it is neither compared against `full` nor said
  // again underneath.
  const cut = shown.lastIndexOf("…");
  const visible = cut >= 0 ? shown.slice(0, cut) : shown;
  // Nothing was lost. The common case, and it has to cost nothing: a list of short rows must not
  // grow a blank line under the cursor.
  if (visible.startsWith(full)) return [];
  // A `full` that is not a continuation of what is on screen means the caller built the two
  // differently rather than by clipping one from the other. The whole of it is then the honest
  // thing to show, because there is no prefix to trust.
  const rest = full.startsWith(visible) ? full.slice(visible.length) : full;
  if (rest.trim() === "") return [];
  return wrapToWidth(rest.replace(/^\s+/, ""), columns);
}

/**
 * As much of `text` as fits `columns` **without breaking a word**, and the rest of it.
 *
 * What a row's first line needs. {@link overflowOf} continues from wherever the clip landed, which
 * is right when the clip is somebody else's — the row is already drawn and ends in an ellipsis that
 * says so. When the row is *ours* to lay out, breaking `work` into `wor` and `k` is a choice, and a
 * bad one: the eye stops at the ragged edge and the reader spends a beat rejoining a word instead of
 * reading the sentence.
 *
 * A single word wider than the column is cut anyway. There is nothing else to do with it, and the
 * alternative — a first line left empty — is worse.
 */
export function takeWords(text: string, columns: number): [string, string] {
  const limit = Math.max(1, Math.floor(columns));
  if (width(text) <= limit) return [text, ""];
  // Split keeping the separators, so what is left over is the exact remainder of the original
  // rather than a rejoin of it: two spaces after a full stop stay two spaces.
  const parts = text.split(/(\s+)/);
  let head = "";
  for (const part of parts) {
    const next = head + part;
    if (width(next.trimEnd()) > limit) break;
    head = next;
  }
  if (head.trim() === "") head = clipToWidth(text, limit);
  return [head.trimEnd(), text.slice(head.length)];
}

/**
 * {@link width}, under a name nothing shadows.
 *
 * `picker` binds `width` to its own float's column count, which is a number — so calling the
 * measuring function inside it is a type error at best and a silent wrong answer at worst.
 */
const columnsOf = width;

/** How many columns of leading space `text` has. */
function leading(text: string): number {
  return text.length - text.replace(/^ +/, "").length;
}

/**
 * Type a directory path, with completion as you go.
 *
 * The list under the field is the directories that match what you have typed so far, refreshed on
 * every keystroke. `<Tab>` takes the highlighted one into the field so you can keep going, the
 * movement keys pick a different one, and `<CR>` accepts either the highlighted row or exactly what
 * you typed — because the directory you want may not be one this offers.
 *
 * `<C-w>` deletes a whole segment rather than a character, which is the difference between walking
 * back up a tree and holding backspace.
 *
 * Resolves to the path, or `null` if dismissed.
 */
export async function pathPicker(
  neosh: Neosh,
  title: string,
  opts: { initial?: string; width?: number; height?: number } = {},
): Promise<string | null> {
  return picker<string>(neosh, [], {
    title,
    width: Math.max(40, opts.width ?? 72),
    height: Math.max(4, opts.height ?? 10),
    query: opts.initial ?? "",
    placeholder: "no directory matches — <CR> takes what you typed",
    source: async (query) => {
      const paths = await neosh.path.complete(query).catch(() => []);
      return paths.map((path) => ({ label: path, value: path }));
    },
    // What you typed, when it is not one of the offered rows. A completion list that refuses a path
    // it did not think of is a list that gets in the way.
    freeform: (query) => (query.trim() === "" ? null : query.trim()),
  });
}

/**
 * A single-line text field. Resolves to the text, or `null` if dismissed.
 *
 * Built on the same capture the picker uses, so it behaves the same way: bindings still win, and
 * `<Esc>` gets you out.
 */
export async function prompt(
  neosh: Neosh,
  title: string,
  opts: { initial?: string; width?: number } = {},
): Promise<string | null> {
  const width = Math.max(24, opts.width ?? 60);
  const buf = await neosh.buf.create({ name: "[prompt]", scratch: true });
  const win = await neosh.float.open(buf, {
    anchor: { kind: "screen" },
    width: { kind: "fixed", n: width },
    height: { kind: "fixed", n: 2 },
    border: "rounded",
    focusable: true,
    closeOnBlur: true,
    // Modal: nothing global resolves while this is up. Shadowing the keys a widget wants — which
    // is what `bindWidgetKeys` does and still does — only ever covered the keys it uses; `^T`,
    // `^G`, `^L` and the rest fell straight through and opened a second panel behind this one,
    // with focus somewhere neither of them expected. `^Q` and `^R` still work, so a widget that
    // fails to bind a way out is never a terminal somebody has to kill: see
    // `ui.modal_escape_keys`.
    modal: true,
    z: 200,
  });

  watchKeys(neosh);
  const keys = await widgetKeys(neosh);

  let text = opts.initial ?? "";
  const render = async () => {
    await neosh.buf.setLines(buf, 0, -1, [title, `> ${text}`]);
    // The caret belongs where the typing goes. Left at the origin it sits on the title, and the
    // field reads as inert no matter what appears in it.
    await neosh.win.setCursor(win, 1, byteLength(`> ${text}`));
  };

  let settle: (v: string | null) => void = () => {};
  const done = new Promise<string | null>((resolve) => {
    settle = resolve;
  });

  const command = `neosh.ui.prompt.key.${++pickerSeq}`;
  const disposers: Disposable[] = [];
  let closed = false;
  const close = async (value: string | null) => {
    if (closed) return;
    closed = true;
    for (const d of disposers) d.dispose();
    await neosh.win.close(win).catch(() => {});
    settle(value);
  };

  disposers.push(
    await neosh.cmd.register(command, async (_args, key: KeyContext | undefined) => {
      if (!key) return;
      switch (actionFor(keys, key.key)) {
        case "dismiss":
          await close(null);
          return;
        case "accept":
          await close(text);
          return;
        case "clear":
          text = "";
          break;
        case "delete_word":
          text = dropSegment(text);
          break;
        default: {
          if (key.key.code.kind === "backspace") {
            text = text.slice(0, -1);
            break;
          }
          if (key.key.code.kind !== "char" || key.key.mods.ctrl || key.key.mods.alt) return;
          text += key.key.code.c;
          break;
        }
      }
      await render();
    }, { desc: "prompt key" }),
  );

  await neosh.focus.push(win);
  disposers.push(await neosh.keymap.capture(win, command));
  await bindWidgetKeys(neosh, win, command, keys);
  await render();
  return done;
}

/**
 * Route every widget key to the widget's own handler while its window is focused.
 *
 * A raw capture only receives what *no binding claimed*. That used to be the whole story, and it
 * meant `<C-n>` never reached a picker: `<C-n>` is bound globally to "new conversation", so you
 * would be typing a filter and suddenly find yourself in a new conversation with the picker gone.
 *
 * Window-scoped bindings outrank global ones and are dropped with the window, so a widget owns its
 * keys for exactly as long as it is on screen and not one keystroke longer. Modes are enumerated
 * because a binding is per mode and a picker can be opened from any of them.
 *
 * These floats are also `modal`, which is the other half and the half this could not be: a widget
 * can name the keys it *wants* and never the ones it merely does not want to happen. Both are
 * needed — modality stops `<C-t>` opening a panel behind the picker, and these bindings are what
 * point `<C-n>` at "next" rather than at nothing.
 */
async function bindWidgetKeys(
  neosh: Neosh,
  win: WindowId,
  command: string,
  keys: Map<WidgetAction, KeySpec[]>,
  extra: readonly string[] = [],
): Promise<void> {
  const lhs = new Set<string>(extra);
  for (const specs of keys.values()) {
    for (const spec of specs) lhs.add(spec.lhs);
  }
  for (const mode of ["normal", "insert", "visual", "chat"] as const) {
    for (const key of lhs) {
      // A key that will not parse on the host side is not worth failing the whole widget over; the
      // capture still delivers it if nothing else claimed it.
      await neosh.keymap.set(mode, key, command, { scope: { kind: "window", win } }).catch(() => {});
    }
  }
}

/**
 * Highlight groups the widgets here use, linked to ones a theme already defines.
 *
 * Call once from your plugin's `activate` if you want them; they are links, not colors, so an
 * unknown theme still renders something sensible.
 */
export async function defineHighlights(_neosh: Neosh): Promise<void> {
  // Nothing to define any more: the groups below are part of the core palette, so they resolve in
  // every theme and follow a theme switch without this library hearing about it. Kept as a no-op
  // because plugins call it, and because a future widget may need a group of its own.
}

/** Re-exported so a caller can type a picker's list without importing from two places. */
export type { BufferId, WindowId };

// ---------------------------------------------------------------------------
// Version-control display
// ---------------------------------------------------------------------------

/**
 * The single letter git itself uses for a state.
 *
 * Written out rather than derived from the enum name, because two of them do not agree with their
 * spelling: untracked is `?`, not `U`, and `U` means *unmerged*. A sidebar that labels every new
 * file `U` is telling the user they have a merge conflict.
 */
export function stateLetter(state: FileState | null | undefined): string {
  switch (state) {
    case "added": return "A";
    case "modified": return "M";
    case "deleted": return "D";
    case "renamed": return "R";
    case "copied": return "C";
    case "type_changed": return "T";
    case "conflicted": return "U";
    case "untracked": return "?";
    case "ignored": return "!";
    default: return " ";
  }
}

/** The two-column `XY` prefix git status prints: staged state, then working-tree state. */
export function statusPrefix(change: FileChange): string {
  return `${stateLetter(change.staged)}${stateLetter(change.unstaged)}`;
}

/**
 * Fit a path into `width` display columns, keeping the end.
 *
 * The tail is what identifies a file; the head is what repeats. Measured in columns rather than
 * characters, so a path with CJK in it does not overflow a narrow panel.
 *
 * Git reports untracked *directories* with a trailing slash — `crates/` — which `basename` turns
 * into an empty string. Kept here so every caller gets that right.
 */
export function shortenPath(path: string, width: number): string {
  const trimmed = path.endsWith("/") ? path.slice(0, -1) : path;
  const suffix = path.endsWith("/") ? "/" : "";
  const chars = Array.from(trimmed);
  const budget = Math.max(4, width - suffix.length);
  if (chars.length <= budget) return trimmed + suffix;
  return `\u2026${chars.slice(-(budget - 1)).join("")}${suffix}`;
}

// ---------------------------------------------------------------------------
// Motion
// ---------------------------------------------------------------------------

/**
 * Ambient motion, on one shared clock.
 *
 * The rule this follows is not a terminal compromise — it is the same rule the web app states
 * outright: *no continuously repainting animations*. Its four animations are duty-cycled with
 * `steps()` so a 2-second pulse is a dozen discrete frames rather than 240. A terminal is that
 * model already: discrete cells, discrete frames, and a diffing renderer that writes only what
 * changed.
 *
 * So: **one clock**, module-global and therefore shared by every plugin, ticking at
 * {@link TICK_MS}. Every spinner in the session is on the same frame and every pulse toggles in the
 * same repaint, rather than each row owning a timer and smearing writes across the second.
 *
 * Measured cost of the whole system in a real terminal: ~1.3% of one core and 0.6 KiB/s at 80 ms,
 * because the renderer sends only changed cells. That holds over SSH.
 */

/** One frame of the shared clock. 100 ms × 10 braille frames is exactly the web's 1000 ms spin. */
export const TICK_MS = 100;

/** The spinner the web app uses 28 times over, as braille. */
const BRAILLE = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/** For a terminal that cannot draw braille. Four frames at 250 ms is the same 1000 ms cycle. */
const ASCII = ["|", "/", "-", "\\"];

let ticks = 0;
let handle: number | null = null;
const listeners = new Set<() => void>();
let ascii = false;
let enabled = true;

/**
 * Subscribe to the shared clock.
 *
 * The clock starts on the first subscriber and stops on the last, so an idle session has no timer
 * at all — and unloading a plugin, which disposes its subscriptions, takes its share with it.
 *
 * Push the result into `ctx.subscriptions`. A subscriber that is never disposed keeps the clock
 * running for the life of the session.
 */
export function onTick(cb: () => void): Disposable {
  listeners.add(cb);
  if (handle === null) {
    handle = setInterval(() => {
      ticks++;
      for (const l of listeners) {
        try {
          l();
        } catch {
          // One misbehaving subscriber must not stop every other spinner in the session.
        }
      }
    }, TICK_MS) as unknown as number;
  }
  return {
    dispose() {
      listeners.delete(cb);
      if (listeners.size === 0 && handle !== null) {
        clearInterval(handle);
        handle = null;
      }
    },
  };
}

/**
 * The current spinner frame.
 *
 * Read it, do not own it: every caller reading the same function on the same clock is what keeps
 * two spinners on screen from wobbling against each other.
 */
export function spinnerFrame(): string {
  if (!enabled) return ascii ? "*" : "•";
  const frames = ascii ? ASCII : BRAILLE;
  const divisor = ascii ? Math.round(250 / TICK_MS) : 1;
  return frames[Math.floor(ticks / divisor) % frames.length] ?? frames[0]!;
}

/**
 * The 1 Hz duty cycle behind every pulsing dot: one second bright, one second dim.
 *
 * Two states, never a ramp — the web's `status-pulse` quantizes to exactly two opacities and this
 * is the same thing with the `dim` attribute. Returning a boolean rather than a glyph keeps the
 * cell width and the accessible text stable while only the attribute changes.
 */
export function pulseBright(): boolean {
  if (!enabled) return true;
  return Math.floor(ticks / Math.round(1000 / TICK_MS)) % 2 === 0;
}

/** Highlight group for a pulsing indicator, for use as `hlGroup`. */
export function pulseHl(bright: string, dim = "Comment"): string {
  return pulseBright() ? bright : dim;
}

/**
 * Configure motion for this session.
 *
 * Called once by whoever owns the setting — the bundled plugins read `ui.motion` and
 * `ui.ascii_only`. With motion off, `spinnerFrame` returns a single static glyph and `pulseBright`
 * is always true, so callers need no branch of their own.
 */
export function configureMotion(opts: { enabled?: boolean; ascii?: boolean }): void {
  if (opts.enabled !== undefined) enabled = opts.enabled;
  if (opts.ascii !== undefined) ascii = opts.ascii;
}

export function motionEnabled(): boolean {
  return enabled;
}

/** Elapsed time as the web app words it: `12s`, `1m 04s`, `1h 02m`. */
export function elapsed(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  if (total < 60) return `${total}s`;
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  if (minutes < 60) return `${minutes}m ${String(seconds).padStart(2, "0")}s`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${String(minutes % 60).padStart(2, "0")}m`;
}

/**
 * A horizontal meter, as cells.
 *
 * Used for the context-window gauge and for anything with a fraction. Repaint only when the filled
 * count changes: a meter that rewrites itself every tick to draw the same thing is the exact cost
 * this module exists to avoid.
 */
export function meter(fraction: number, width: number, opts: { ascii?: boolean } = {}): string {
  const w = Math.max(1, width);
  const clamped = Math.min(1, Math.max(0, Number.isFinite(fraction) ? fraction : 0));
  const filled = Math.round(clamped * w);
  const [on, off] = opts.ascii ?? ascii ? ["#", "-"] : ["█", "░"];
  return on!.repeat(filled) + off!.repeat(w - filled);
}

/** `1.2k`, `45.3k`, `1.8M` — the compact forms a status line has room for. */
export function compact(n: number): string {
  const v = Math.max(0, Math.round(n));
  if (v < 1000) return String(v);
  if (v < 1_000_000) {
    const k = v / 1000;
    return `${k < 10 ? k.toFixed(1) : Math.round(k)}k`;
  }
  const m = v / 1_000_000;
  return `${m < 10 ? m.toFixed(1) : Math.round(m)}M`;
}

/** `$0.0042`, `$1.23`, `$12.30` — enough precision to be useful at both ends. */
export function money(usd: number): string {
  if (!Number.isFinite(usd) || usd <= 0) return "$0";
  if (usd < 0.01) return `$${usd.toFixed(4)}`;
  if (usd < 1) return `$${usd.toFixed(3)}`;
  return `$${usd.toFixed(2)}`;
}

// ---------------------------------------------------------------------------
// Cursored list
// ---------------------------------------------------------------------------

/**
 * A row in a {@link CursoredList}.
 *
 * `right` is rendered flush against the pane's right edge by the frontend, which is the only thing
 * that knows how wide the pane is or how many columns a character occupies. Building that alignment
 * here would mean measuring display width in a plugin, which is exactly what the byte-offset
 * protocol exists to prevent.
 */
export interface ListRow<T = unknown> {
  text: string;
  /**
   * The whole of what this row says, when `text` is a clipped version of it.
   *
   * A panel is a column of fixed width and a conversation title is not, so rows get cut — and a row
   * cut at the edge is a row you cannot read, which in a list of conversations means you cannot
   * tell two of them apart. Set this to the row as it *would* have been written with unlimited
   * room, and the list shows the rest of it on continuation lines whenever the cursor is on that
   * row, folding back to one line the moment the cursor leaves.
   *
   * The cursor is what asks. Only one row can be under it, so only one row is ever more than a line
   * tall, and the column stays scannable — which is the thing a list is for and the thing that
   * wrapping everything all the time takes away.
   *
   * Include the same prefix `text` has: the marker, the glyph, the indent. What comes back is the
   * difference between the two, so `text: "  ▸ some long ti…"` with `full: "  ▸ some long title"`
   * continues with `tle` and not with the glyph again. Leave out anything the clip *kept* — a
   * trailing unread dot, a count — since that is already on screen and saying it twice under the
   * row is how a marker stops meaning anything. Nothing happens without
   * {@link CursoredListOptions.width}, because nothing here can measure a column it was not told
   * about.
   */
  full?: string;
  /**
   * Keep this row unfolded whether or not the cursor is on it.
   *
   * The cursor asking is the rule, and this is the one exception worth having: the conversation
   * you are *in* is not a row you are considering, it is where you are, and a panel that abbreviates
   * it to `Fix the login re…` until you happen to move the cursor onto it is abbreviating the one
   * row you already know you want. Needs {@link ListRow.full} and
   * {@link CursoredListOptions.width}, like every other unfolding here. Use it for a row that is
   * *current*, never for one that is merely long — every row that opts in is a row the column can
   * no longer be scanned down.
   */
  expand?: boolean;
  /**
   * The column continuation lines line up under. Defaults to the row's own indent plus two.
   *
   * Set it where the default would be wrong — a row whose text starts with a marker and a glyph
   * has its *content* several columns in from its indent, and continuations that ignore that read
   * as a second row rather than as the rest of this one.
   */
  indent?: number;
  /** Highlight group for the whole row. Link to one the theme defines. */
  hl?: string;
  /**
   * Highlights for pieces of the row, over the top of `hl`: a favourite marker, a state glyph, a
   * matched substring. `from`/`to` are UTF-8 byte offsets into `text` — the same unit every column
   * on the wire uses — so build them with {@link byteLength} rather than `.length`.
   */
  spans?: Array<{ from: number; to: number; hl: string }>;
  /** Flush-right status: a timestamp, a count, a state word. */
  right?: { text: string; hl?: string };
  /** Rows that cannot be landed on: headings, separators, blanks. */
  inert?: boolean;
  value?: T;
}

export interface CursoredListOptions {
  /** Highlight for the row the cursor is on. Defaults to the theme's `Sidebar.Selected`. */
  cursorHl?: string;
  /** Called after every move, with the row landed on. */
  onMove?(index: number): void;
  /**
   * How many columns the panel has, asked at render time.
   *
   * Asked rather than passed once, because a panel is resized and a width captured at construction
   * is a width that goes stale on the first drag. Without it {@link ListRow.full} does nothing:
   * a plugin cannot measure the dock it was given, and guessing is how a continuation line ends up
   * wrapping one column past the edge for the rest of the session.
   */
  width?(): number;
}

/**
 * The list behaviour every panel in a workspace needs, in one place.
 *
 * Owns a cursor that skips inert rows, renders text and marks together, and keeps the cursor on
 * screen. It does **not** own a window, a buffer or any keys — a docked sidebar and a floating
 * picker want different chrome and different bindings, and the part worth sharing is the part
 * below.
 */
export class CursoredList<T = unknown> {
  private rows: ListRow<T>[] = [];
  private cursor = 0;

  constructor(
    private readonly neosh: Neosh,
    private readonly buf: BufferId,
    private readonly ns: number,
    private readonly opts: CursoredListOptions = {},
  ) {}

  get index(): number {
    return this.cursor;
  }

  get current(): ListRow<T> | undefined {
    return this.rows[this.cursor];
  }

  get value(): T | undefined {
    return this.rows[this.cursor]?.value;
  }

  get length(): number {
    return this.rows.length;
  }

  /**
   * Replace the contents, keeping the cursor where it makes sense.
   *
   * Anchored to `value` identity rather than index: a list that reorders under a running turn would
   * otherwise move the selection out from under the user's next keystroke.
   */
  setRows(rows: ListRow<T>[], sameAs?: (a: T, b: T) => boolean): void {
    const previous = this.rows[this.cursor]?.value;
    this.rows = rows;
    if (previous !== undefined && sameAs) {
      const found = rows.findIndex((r) => r.value !== undefined && sameAs(r.value, previous));
      if (found >= 0) {
        this.cursor = found;
        return;
      }
    }
    this.clamp();
  }

  private clamp(): void {
    if (this.rows.length === 0) {
      this.cursor = 0;
      return;
    }
    this.cursor = Math.min(Math.max(0, this.cursor), this.rows.length - 1);
    if (this.rows[this.cursor]?.inert) {
      const after = this.rows.findIndex((r, i) => i >= this.cursor && !r.inert);
      const anywhere = this.rows.findIndex((r) => !r.inert);
      this.cursor = after >= 0 ? after : Math.max(0, anywhere);
    }
  }

  /**
   * Move by `delta` selectable rows.
   *
   * Wraps by default, because a list you cannot get back to the top of is a chore. A page step
   * passes `wrap: false`: `^D` at the foot of the list means "there is no more", and one that
   * silently reappears at the top is a keypress that loses your place in a column you were
   * reading downwards.
   *
   * `delta` counts rows you can land on, not buffer rows — headings, rules and blanks are not
   * places, so `5j` past two separators moves five conversations rather than three.
   */
  move(delta: number, opts: { wrap?: boolean } = {}): void {
    if (this.rows.length === 0 || delta === 0) return;
    const wrap = opts.wrap ?? true;
    const step = delta < 0 ? -1 : 1;
    let remaining = Math.abs(delta);
    let i = this.cursor;
    let landed = -1;
    // Bounded by the whole list per row asked for: an all-inert list has nowhere to go, and a
    // wrapping search for a row that does not exist would otherwise spin.
    const limit = this.rows.length * Math.abs(delta) + this.rows.length;
    for (let n = 0; n < limit && remaining > 0; n++) {
      let next = i + step;
      if (next < 0 || next >= this.rows.length) {
        if (!wrap) break;
        next = (next + this.rows.length) % this.rows.length;
      }
      i = next;
      if (!this.rows[i]?.inert) {
        landed = i;
        remaining--;
      }
    }
    if (landed < 0) return;
    this.cursor = landed;
    this.opts.onMove?.(landed);
  }

  /**
   * The first or last row you can land on — `gg` and `G`.
   *
   * A separate verb from a large `move`, because "the end" is a place and "a hundred rows down"
   * is a guess about where the end is.
   */
  toEnd(which: "first" | "last"): void {
    const found = which === "first"
      ? this.rows.findIndex((r) => !r.inert)
      : this.rows.reduce((acc, r, i) => (r.inert ? acc : i), -1);
    if (found < 0) return;
    this.cursor = found;
    this.opts.onMove?.(found);
  }

  /** The n-th row you can land on, 1-based, the way `5G` counts. */
  nth(n: number): void {
    let seen = 0;
    for (const [i, row] of this.rows.entries()) {
      if (row.inert) continue;
      seen += 1;
      if (seen === n) {
        this.cursor = i;
        this.opts.onMove?.(i);
        return;
      }
    }
    this.toEnd("last");
  }

  /** Put the cursor on the first row whose value matches. */
  select(match: (value: T) => boolean): boolean {
    const found = this.rows.findIndex((r) => r.value !== undefined && match(r.value));
    if (found < 0) return false;
    this.cursor = found;
    return true;
  }

  /**
   * Write the rows and their marks.
   *
   * `showCursor` is false when the panel does not have the keyboard: a cursor drawn on an unfocused
   * list claims an attention it does not have, and two visible cursors is worse than none.
   *
   * `pinned` is how many rows at the end sit against the *bottom edge* rather than after the
   * content: a gauge or a key strip is a foot, and a foot that floats halfway up a half-empty
   * column is chrome that moves every time the list above it grows by one. It costs blank lines
   * and nothing else — when the list is longer than the window there is no room to pin anything
   * to, and the rows go back to being the end of the list, which is where scrolling will find
   * them.
   */
  async render(
    opts: { showCursor?: boolean; win?: WindowId; pinned?: number } = {},
  ): Promise<void> {
    const cursorHl = this.opts.cursorHl ?? "Sidebar.Selected";
    const columns = this.opts.width?.() ?? 0;
    const drawn: DrawnRow[] = [];
    /** Which buffer line each row starts on. Not the row's index once anything has unfolded. */
    const lineOf: number[] = [];
    /** How tall the cursor's row turned out, so scrolling can keep all of it on screen. */
    let block = 1;

    this.rows.forEach((row, i) => {
      lineOf[i] = drawn.length;
      const eol = byteLength(row.text);
      const onCursor = opts.showCursor !== false && i === this.cursor;
      const marks: DrawnMark[] = [];
      if (onCursor && eol > 0) {
        marks.push({ col: 0, opts: { hlGroup: cursorHl, endCol: eol, priority: 200 } });
      }
      if (row.hl && eol > 0) {
        marks.push({ col: 0, opts: { hlGroup: row.hl, endCol: eol } });
      }
      // Above the row's own highlight, below the cursor's 200: a marker keeps its colour on an
      // ordinary row and yields to the selection, which is the row the eye is already on.
      for (const s of row.spans ?? []) {
        if (s.to <= s.from || s.from >= eol) continue;
        marks.push({
          col: s.from,
          opts: { hlGroup: s.hl, endCol: Math.min(s.to, eol), priority: 100 },
        });
      }
      if (row.right) {
        marks.push({
          col: 0,
          opts: {
            virtText: [{ text: row.right.text, hlGroup: row.right.hl ?? "Comment" }],
            virtTextPos: "right",
          },
        });
      }
      drawn.push({ text: row.text, marks });

      // The rest of a row that did not fit: under the cursor, or on a row that asked to stay open.
      if ((!onCursor && !row.expand) || row.full === undefined || columns < 1) return;
      const indent = Math.max(0, Math.min(row.indent ?? leading(row.text) + 2, columns - 8));
      const rest = overflowOf(row.text, row.full, columns - indent);
      // Only the cursor's block is scrolled to. A row that is permanently open is not somewhere
      // the cursor is, so counting its height here would scroll the panel to a row nobody moved to.
      if (onCursor) block = 1 + rest.length;
      for (const line of rest) {
        const text = `${" ".repeat(indent)}${line}`;
        const end = byteLength(text);
        const cont: DrawnMark[] = [];
        // The band runs the whole block. A continuation drawn on the terminal background reads as
        // a separate row that happens to be indented, which is the one thing it must not be. Only
        // where there is a band to run: an always-open row that is not under the cursor gets its
        // own colour and nothing else, or every one of them would look selected.
        if (onCursor) cont.push({ col: 0, opts: { hlGroup: cursorHl, endCol: end, priority: 200 } });
        if (row.hl) cont.push({ col: 0, opts: { hlGroup: row.hl, endCol: end } });
        drawn.push({ text, marks: cont });
      }
    });

    // The foot, against the bottom edge. Measured after everything is drawn rather than counted in
    // rows, because an unfolded row is several lines tall and padding by row count would leave the
    // strip one line short for every row that happened to be open.
    const pinned = Math.min(Math.max(0, opts.pinned ?? 0), this.rows.length);
    // One round trip, used twice — and skipped when neither the foot nor the cursor needs it, since
    // this runs on a tick while a turn is in flight and an unfocused panel that pins nothing has
    // nothing to measure.
    const needed = pinned > 0 || opts.showCursor !== false;
    const view = opts.win === undefined || !needed
      ? null
      : await this.neosh.win.viewport(opts.win).catch(() => null);
    if (pinned > 0) {
      const at = lineOf[this.rows.length - pinned] ?? drawn.length;
      const filler = (view?.height ?? 0) - drawn.length;
      if (filler > 0) {
        drawn.splice(at, 0, ...Array.from({ length: filler }, () => ({ text: "", marks: [] })));
        for (let i = this.rows.length - pinned; i < this.rows.length; i++) {
          lineOf[i] = (lineOf[i] ?? 0) + filler;
        }
      }
    }

    // One call, not one per mark. A panel that wrote its text, cleared its namespace and then set
    // its marks a call at a time was observable halfway through — and a frame landing after the
    // clear drew every row unmarked, which is `Normal`, which is near-white. That is what a sidebar
    // full of running agents was flashing.
    await this.neosh.buf.render(this.buf, this.ns, 0, -1, drawn);

    // Keep the cursor on screen without recentring on every keystroke. In buffer lines, not row
    // indices: an unfolded row is several lines tall and the two stopped agreeing the moment one
    // was — which would scroll to the wrong place by however many rows above it had ever unfolded.
    if (opts.win !== undefined && opts.showCursor !== false) {
      const v = view;
      if (v && v.height > 0) {
        const at = lineOf[this.cursor] ?? this.cursor;
        // The whole of the row, not only the line it starts on: scrolling until the *first* line
        // is visible leaves the continuation you unfolded it for below the bottom edge.
        const end = at + Math.min(block, v.height) - 1;
        const top = v.top_line;
        if (at < top) await this.neosh.win.scrollTo(opts.win, at);
        else if (end >= top + v.height) {
          await this.neosh.win.scrollTo(opts.win, end - v.height + 1);
        }
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Two-pane picker
// ---------------------------------------------------------------------------

/**
 * One entry in the left rail: a provider, an account, a category.
 *
 * The mark is drawn in its own highlight group, which is how a rail of eleven providers stays
 * legible — you find the one you want by shape and colour before you have read a word.
 */
export interface RailItem<G> {
  /** A brand mark, one or two columns. Drawn in `mark.hl`. */
  mark?: { text: string; hl?: string };
  label: string;
  /** A state marker at the right edge — signed in, needs a key, unavailable. */
  badge?: { text: string; hl?: string };
  /** Heading this sits under. Groups appear in the order their first member does. */
  group?: string;
  /** Listed, dimmed, and skipped by the cursor. For a provider whose driver is missing. */
  disabled?: boolean;
  value: G;
}

/** One row of the right-hand list. */
export interface PaneItem<T> extends PickerItem<T> {
  /** A middle column, aligned across rows: a tier, a size, a state. */
  badge?: { text: string; hl?: string };
  /**
   * A collapsible section this row belongs to.
   *
   * Rows in a section are hidden behind a header that says how many there are. What it is for:
   * last year's models, which you want reachable and do not want in the way.
   */
  section?: string;
  disabled?: boolean;
}

export interface RailPickerOptions<G, T> {
  title?: string;
  /** Columns for the rail. The list takes the rest. */
  railWidth?: number;
  /** Total inner width. */
  width?: number;
  /** Rows of body. */
  height?: number;
  rail: RailItem<G>[];
  /** Which rail entry to open on. */
  railAt?: number;
  /** The rows for a rail entry. Called on open and on every rail move. */
  items(group: G): Promise<PaneItem<T>[]>;
  /** Which row to start on, given the rows. */
  itemAt?(items: PaneItem<T>[]): number;
  /** The key strip at the foot. Keep it to one line. */
  hints?: string;
  /**
   * What the two panes are called, for the affordance in the title row.
   *
   * `"providers"` and `"models"` rather than `"panes"` and `"list"`, wherever the caller knows —
   * a key labelled with what it reaches is a key somebody presses.
   */
  railLabel?: string;
  paneLabel?: string;
  /**
   * Shown in place of the list when there is nothing in it.
   *
   * Not decoration: an empty pane beside a populated rail reads as a broken widget, and the two
   * reasons it can be empty — your filter matched nothing, or this provider serves nothing until
   * it can authenticate — need different things done about them.
   */
  placeholder?: string;
  /**
   * Keys `onKey` wants that a *binding elsewhere* would otherwise take.
   *
   * The raw capture only receives what no keymap claimed, so a caller reaching for `<C-s>` gets
   * nothing — that key belongs to the transcript reader. Listing it here binds it to this widget
   * for as long as the widget is open, the same way its own movement keys are bound.
   *
   * Bare letters do not need this and should not use it: they reach `onKey` already, and taking
   * one means it can never be typed into the filter — which is how a model picker ends up unable
   * to search for "sonnet".
   */
  ownKeys?: readonly string[];
  /**
   * A key the caller wants for itself, checked before the widget's own.
   *
   * Return `"close"` to dismiss, `"reload"` to re-fetch the current list, `"handled"` to redraw,
   * and nothing at all to let the widget have the key. This is what lets a model picker put "sign
   * in" and "set effort" on keys without a second modal.
   */
  onKey?(
    key: KeyContext,
    ctx: { rail: G | undefined; item: T | undefined },
  ): Promise<"handled" | "reload" | "close" | undefined> | "handled" | "reload" | "close" | undefined;
}

const RAIL_NS = "neosh.ui.rail";

/**
 * A picker in two panes: a rail of categories, and the rows belonging to the selected one.
 *
 * The shape exists because one flat list cannot answer two questions at once. "Which provider" and
 * "which model" are different choices with different cardinalities — a dozen providers, hundreds of
 * models — and flattening them produces a list where the thing you want is thirty rows below a
 * provider you do not use.
 *
 * Both panes are one buffer with a rule down the middle rather than two floats, because two floats
 * cannot be kept adjacent without each of them knowing the other's width, and neither of them may
 * measure anything. One buffer makes the rule a character, which is a thing a plugin can place.
 *
 * Typing filters the list. `pane_next`/`pane_prev` — `<Tab>`/`<S-Tab>` and `←`/`→` by default —
 * move between the panes, and the rail's own entries are reachable with the same movement keys as
 * the list, so nothing here needs a mouse or a chord you have to be told about.
 */
export async function railPicker<G, T>(
  neosh: Neosh,
  opts: RailPickerOptions<G, T>,
): Promise<T | null> {
  const height = Math.max(3, opts.height ?? 14);
  const total = Math.max(40, opts.width ?? 84);
  const railWidth = Math.min(Math.max(12, opts.railWidth ?? 22), total - 24);
  const paneWidth = total - railWidth - 1; // the rule takes a column

  const buf = await neosh.buf.create({ name: `[${opts.title ?? "select"}]`, scratch: true });
  const ns = await neosh.ns.create(RAIL_NS);
  const win = await neosh.float.open(buf, {
    anchor: { kind: "screen" },
    width: { kind: "fixed", n: total },
    // title, filter, rule, body, rule, hints
    height: { kind: "fixed", n: height + 5 },
    border: "rounded",
    focusable: true,
    closeOnBlur: true,
    // Modal: nothing global resolves while this is up. Shadowing the keys a widget wants — which
    // is what `bindWidgetKeys` does and still does — only ever covered the keys it uses; `^T`,
    // `^G`, `^L` and the rest fell straight through and opened a second panel behind this one,
    // with focus somewhere neither of them expected. `^Q` and `^R` still work, so a widget that
    // fails to bind a way out is never a terminal somebody has to kill: see
    // `ui.modal_escape_keys`.
    modal: true,
    z: 200,
  });

  watchKeys(neosh);
  const keys = await widgetKeys(neosh);
  const railLabel = opts.railLabel ?? "panes";
  const paneLabel = opts.paneLabel ?? "list";

  // ---- rail rows, headings interleaved ------------------------------------
  type RailRow =
    | { kind: "heading"; text: string }
    | { kind: "entry"; item: RailItem<G>; index: number };

  const railRows: RailRow[] = [];
  {
    let group: string | undefined;
    opts.rail.forEach((item, index) => {
      if (item.group !== undefined && item.group !== group) {
        group = item.group;
        railRows.push({ kind: "heading", text: item.group });
      }
      railRows.push({ kind: "entry", item, index });
    });
  }
  const selectableRail = railRows
    .map((r, at) => (r.kind === "entry" && !r.item.disabled ? at : -1))
    .filter((at) => at >= 0);

  let railAt =
    selectableRail.find(
      (at) => (railRows[at] as { index: number }).index === (opts.railAt ?? 0),
    ) ?? selectableRail[0] ?? 0;
  let railTop = 0;

  // ---- pane state ----------------------------------------------------------
  let all: PaneItem<T>[] = [];
  let open = new Set<string>();
  let query = "";
  let cursor = 0;
  let paneTop = 0;
  /** In the rail, or in the list. */
  let focus: "rail" | "pane" = "pane";

  type PaneRow =
    | { kind: "section"; name: string; count: number }
    | { kind: "item"; item: PaneItem<T>; positions: number[] };

  let rows: PaneRow[] = [];

  const rebuild = () => {
    const ranked = query === ""
      ? all.map((item) => ({ item, positions: [] as number[], score: 0 }))
      : all
          .map((item) => {
            const hay = item.keywords ? `${item.label} ${item.keywords}` : item.label;
            const m = fuzzy(hay, query);
            return m ? { item, positions: m.positions, score: m.score } : null;
          })
          .filter((x): x is NonNullable<typeof x> => x !== null)
          .sort((a, b) => b.score - a.score);

    const out: PaneRow[] = [];
    const sections = new Map<string, number>();
    for (const r of ranked) {
      const section = r.item.section;
      if (section === undefined) {
        out.push({ kind: "item", item: r.item, positions: r.positions });
        continue;
      }
      sections.set(section, (sections.get(section) ?? 0) + 1);
    }
    // Filtering opens every section: a row you cannot see is a row your query did not find, and
    // "no matches" while the match sits folded away is the worst possible answer.
    for (const [name, count] of sections) {
      const expanded = open.has(name) || query !== "";
      out.push({ kind: "section", name, count });
      if (!expanded) continue;
      for (const r of ranked) {
        if (r.item.section === name) out.push({ kind: "item", item: r.item, positions: r.positions });
      }
    }
    rows = out;
    cursor = Math.min(cursor, Math.max(0, rows.length - 1));
  };

  // Bumped on every rail move, so a slow load cannot land after a faster one that came later.
  let generation = 0;

  const load = async () => {
    const mine = ++generation;
    const entry = railRows[railAt];
    const got = entry?.kind === "entry" ? await opts.items(entry.item.value).catch(() => []) : [];
    // Holding a movement key starts one load per row, and they finish in whatever order the
    // network allows — so the last *answer* is routinely not the last *question*. Without this,
    // pressing down eight times lands you on a provider showing the empty list of a provider you
    // passed through on the way, which reads as "this one has no models".
    if (mine !== generation) return;
    all = got;
    rebuild();
    cursor = Math.max(0, Math.min(opts.itemAt?.(all) ?? 0, Math.max(0, rows.length - 1)));
    // Land on something selectable rather than on a section header.
    if (rows[cursor]?.kind !== "item") cursor = rows.findIndex((r) => r.kind === "item");
    if (cursor < 0) cursor = 0;
    paneTop = 0;
  };

  // ---- drawing -------------------------------------------------------------
  const RULE_V = "│";
  const railCell = (row: RailRow | undefined, on: boolean): { text: string; marks: Mark[] } => {
    const marks: Mark[] = [];
    if (!row) return { text: padToWidth("", railWidth), marks };
    if (row.kind === "heading") {
      const text = padToWidth(` ${row.text}`, railWidth);
      marks.push({ from: 0, to: byteLength(text), hl: "Sidebar.Heading" });
      return { text, marks };
    }
    const { item } = row;
    const mark = item.mark?.text ?? " ";
    const badge = item.badge?.text ?? "";
    const room = railWidth - 3 - width(mark) - width(badge) - 1;
    const label = clipToWidth(item.label, Math.max(1, room));
    const head = `${on ? "❯" : " "} ${mark} `;
    const body = `${head}${label}`;
    const text = padToWidth(body, railWidth - width(badge) - 1) + badge + " ";
    if (item.mark) {
      const at = byteLength(`${on ? "❯" : " "} `);
      marks.push({ from: at, to: at + byteLength(mark), hl: item.mark.hl ?? "Normal" });
    }
    if (item.badge) {
      const at = byteLength(padToWidth(body, railWidth - width(badge) - 1));
      marks.push({ from: at, to: at + byteLength(badge), hl: item.badge.hl ?? "Comment" });
    }
    if (on) {
      marks.push({ from: 0, to: byteLength(text), hl: "Picker.Selected", priority: 1 });
    } else if (item.disabled) {
      marks.push({ from: 0, to: byteLength(text), hl: "Comment" });
    }
    return { text, marks };
  };

  /**
   * One row of the list, and — when the cursor is on it — the rest of what it had to say.
   *
   * `more` is what makes the three columns bearable. A model row is a name, a rung and a sentence
   * about it inside 45% of half a float, so the sentence is the first thing cut and the sentence is
   * the whole reason a catalogue of forty models is choosable at all. It goes under the row rather
   * than beside it, because the columns are what let you read *down* the list and widening one for
   * the row you are on would move the other two every time you pressed a key.
   */
  const paneCell = (
    row: PaneRow | undefined,
    on: boolean,
  ): { text: string; marks: Mark[]; more: string[] } => {
    const marks: Mark[] = [];
    const more: string[] = [];
    if (!row) return { text: "", marks, more };
    if (row.kind === "section") {
      const text = `   ${open.has(row.name) || query !== "" ? "▾" : "▸"} ${row.count} ${row.name}`;
      marks.push({ from: 0, to: byteLength(text), hl: on ? "Picker.Selected" : "Comment" });
      return { text, marks, more };
    }
    const { item } = row;
    const badge = item.badge?.text ?? "";
    const detail = item.detail ?? "";
    // Three columns: label, badge, detail. The badge column is fixed so the rungs of the ladder
    // line up — reading down it is how you see the shape of what a provider offers.
    const badgeWidth = badge === "" ? 0 : 10;
    const labelWidth = Math.max(10, Math.floor((paneWidth - 3 - badgeWidth) * 0.45));
    const head = on ? " ❯ " : "   ";
    const room = paneWidth - width(head) - labelWidth - badgeWidth;
    // On the cursor's row the columns are broken at words and continued underneath; everywhere else
    // they are clipped, because a list of forty models is read by running an eye down it.
    const [labelHead, labelRest] = on
      ? takeWords(item.label, labelWidth)
      : [clipToWidth(item.label, labelWidth), ""];
    const [detailHead, detailRest] = on
      ? takeWords(detail, Math.max(0, room))
      : [clipToWidth(detail, Math.max(0, room)), ""];
    const label = padToWidth(labelHead, labelWidth);
    const badgeCell = badgeWidth === 0 ? "" : padToWidth(clipToWidth(badge, badgeWidth), badgeWidth);
    const text = `${head}${label}${badgeCell}${detailHead}`;

    const labelAt = byteLength(head);
    const offsets = byteOffsets(item.label);
    for (const at of row.positions) {
      const from = offsets[at];
      const to = offsets[at + 1];
      if (from === undefined || to === undefined) continue;
      marks.push({ from: labelAt + from, to: labelAt + to, hl: "Picker.Match", priority: 200 });
    }
    if (badgeWidth > 0) {
      const at = labelAt + byteLength(label);
      marks.push({ from: at, to: at + byteLength(badgeCell), hl: item.badge?.hl ?? "Comment" });
    }
    if (detailHead !== "") {
      const at = labelAt + byteLength(label) + byteLength(badgeCell);
      marks.push({ from: at, to: byteLength(text), hl: "Picker.Detail" });
    }
    if (item.disabled) marks.push({ from: 0, to: byteLength(text), hl: "Comment" });
    if (on) marks.push({ from: 0, to: byteLength(text), hl: "Picker.Selected", priority: 1 });

    if (on) {
      // The name first, if the name itself did not fit — an id is what you are choosing by, and
      // half of one is not a thing you can choose by.
      const nameCol = width(head);
      if (labelRest.trim() !== "") {
        for (const line of wrapToWidth(labelRest.trim(), Math.max(8, paneWidth - nameCol - 2))) {
          more.push(`${" ".repeat(nameCol)}${line}`);
        }
      }
      const detailCol = nameCol + labelWidth + badgeWidth;
      if (detailRest.trim() !== "") {
        for (const line of wrapToWidth(detailRest.trim(), Math.max(8, paneWidth - detailCol - 1))) {
          more.push(`${" ".repeat(detailCol)}${line}`);
        }
      }
    }
    return { text, marks, more };
  };

  interface Mark {
    from: number;
    to: number;
    hl: string;
    priority?: number;
  }

  const render = async () => {
    // Each pane scrolls on its own: moving down a long model list must not scroll the rail out
    // from under the provider you are looking at.
    if (railAt < railTop) railTop = railAt;
    if (railAt >= railTop + height) railTop = railAt - height + 1;
    // The unfolded row is paid for out of the list rather than out of the float: growing the float
    // instead would move the key strip under the reader's eyes on every keystroke, and the strip
    // is the row that says how to get out.
    const unfolded = focus === "pane" ? paneCell(rows[cursor], true).more.length : 0;
    const room = Math.max(1, height - unfolded);
    if (cursor < paneTop) paneTop = cursor;
    if (cursor >= paneTop + room) paneTop = cursor - room + 1;

    const lines: string[] = [];
    const marks: { line: number; mark: Mark }[] = [];

    // The title row, and at its right edge the key for *the pane you are not in*. Live rather
    // than a legend: the shortcut row below can only say the key exists, and a two-pane widget
    // whose second pane has no visible way in is a pane nobody finds.
    const title = opts.title ?? "";
    const cross = focus === "pane" ? `${keyLabel(keys, "pane_prev")} ${railLabel}` : `${keyLabel(keys, "pane_next")} ${paneLabel}`;
    const gap = Math.max(1, total - width(title) - width(cross));
    lines.push(`${title}${" ".repeat(gap)}${cross}`);
    marks.push({ line: 0, mark: { from: 0, to: byteLength(title), hl: "Float.Title" } });
    marks.push({
      line: 0,
      mark: {
        from: byteLength(title) + gap,
        to: byteLength(lines[0]!),
        hl: "Composer.HintKey",
      },
    });
    lines.push(`> ${query}`);
    lines.push("─".repeat(railWidth) + "┬" + "─".repeat(paneWidth));
    marks.push({ line: 2, mark: { from: 0, to: byteLength(lines[2]!), hl: "Separator" } });

    const empty = rows.length === 0
      ? `   ${query === "" ? (opts.placeholder ?? "nothing here") : "no matches"}`
      : null;

    // The two columns are filled independently and then zipped, because they no longer advance
    // together: the pane's cursor row is several lines tall when it has unfolded, and the rail
    // beside it must keep listing providers one per line rather than skipping the ones the
    // unfolded row happened to sit across.
    const paneLines: { text: string; marks: Mark[] }[] = [];
    if (empty !== null) {
      paneLines.push({ text: empty, marks: [{ from: 0, to: byteLength(empty), hl: "Comment" }] });
    } else {
      for (let i = paneTop; i < rows.length && paneLines.length < height; i++) {
        const on = focus === "pane" && i === cursor;
        const cell = paneCell(rows[i], on);
        paneLines.push({ text: cell.text, marks: cell.marks });
        if (!on) continue;
        for (const text of cell.more) {
          paneLines.push({
            text,
            marks: [{ from: 0, to: byteLength(text), hl: "Picker.Selected", priority: 1 }],
          });
        }
      }
    }

    for (let i = 0; i < height; i++) {
      const left = railCell(railRows[railTop + i], focus === "rail" && railTop + i === railAt);
      const right = paneLines[i] ?? { text: "", marks: [] as Mark[] };
      const line = lines.length;
      lines.push(`${left.text}${RULE_V}${right.text}`);
      for (const m of left.marks) marks.push({ line, mark: m });
      const shift = byteLength(left.text) + byteLength(RULE_V);
      marks.push({ line, mark: { from: byteLength(left.text), to: shift, hl: "Separator" } });
      for (const m of right.marks) {
        marks.push({ line, mark: { from: m.from + shift, to: m.to + shift, hl: m.hl, priority: m.priority } });
      }
    }

    lines.push("─".repeat(railWidth) + "┴" + "─".repeat(paneWidth));
    marks.push({
      line: lines.length - 1,
      mark: { from: 0, to: byteLength(lines[lines.length - 1]!), hl: "Separator" },
    });
    const hints = opts.hints ?? "↵ use   ⇥ panes   ^N/^P move   esc close";
    lines.push(` ${hints}`);
    marks.push({
      line: lines.length - 1,
      mark: { from: 0, to: byteLength(lines[lines.length - 1]!), hl: "Sidebar.Dim" },
    });

    // The marks were already collected against their rows; they now travel with them. One call
    // rather than one per mark, so there is no frame in which the text has arrived and the colour
    // has not — an unmarked row draws in `Normal`, which is near-white.
    const drawn: DrawnRow[] = lines.map((text) => ({ text, marks: [] }));
    for (const { line, mark } of marks) {
      drawn[line]?.marks!.push({
        col: mark.from,
        opts: { hlGroup: mark.hl, endCol: mark.to, priority: mark.priority ?? 0 },
      });
    }
    await neosh.buf.render(buf, ns, 0, -1, drawn);
    // The caret belongs where the typing goes. Parked at the origin, the filter reads as inert.
    await neosh.win.setCursor(win, 1, byteLength(`> ${query}`));
  };

  // ---- keys ----------------------------------------------------------------
  let settle: (v: T | null) => void = () => {};
  const done = new Promise<T | null>((resolve) => {
    settle = resolve;
  });

  const command = `${RAIL_NS}.key.${++pickerSeq}`;
  const disposers: Disposable[] = [];
  let closed = false;
  const close = async (value: T | null) => {
    if (closed) return;
    closed = true;
    for (const d of disposers) d.dispose();
    await neosh.win.close(win).catch(() => {});
    settle(value);
  };

  const moveRail = async (delta: number) => {
    const at = selectableRail.indexOf(railAt);
    const next = selectableRail[Math.min(selectableRail.length - 1, Math.max(0, at + delta))];
    if (next === undefined || next === railAt) return;
    railAt = next;
    // The filter belongs to the list that was showing. Carrying it to a different provider hides
    // most of what you just switched to, for a reason that has scrolled off the screen.
    query = "";
    await load();
  };

  /**
   * Move the list cursor, stepping over rows that cannot be landed on.
   *
   * Section headers *are* landable — `↵` on one folds it — so the only thing skipped is a row the
   * caller marked unusable. Skipping in the direction of travel, and giving up at the end rather
   * than reversing, so holding a movement key never bounces.
   */
  const movePane = (delta: number) => {
    const step = delta > 0 ? 1 : -1;
    let at = cursor;
    for (let n = 0; n < Math.abs(delta); n++) {
      let candidate = at + step;
      while (candidate >= 0 && candidate < rows.length) {
        const row = rows[candidate];
        if (row && !(row.kind === "item" && row.item.disabled)) break;
        candidate += step;
      }
      if (candidate < 0 || candidate >= rows.length) break;
      at = candidate;
    }
    cursor = Math.max(0, Math.min(Math.max(0, rows.length - 1), at));
  };

  const currentItem = (): PaneItem<T> | undefined => {
    const row = rows[cursor];
    return row?.kind === "item" ? row.item : undefined;
  };

  disposers.push(
    await neosh.cmd.register(command, async (_args, key) => {
      if (!key) return;
      const railEntry = railRows[railAt];
      const outcome = await opts.onKey?.(key, {
        rail: railEntry?.kind === "entry" ? railEntry.item.value : undefined,
        item: currentItem()?.value,
      });
      if (outcome === "close") {
        await close(null);
        return;
      }
      if (outcome === "reload") {
        await load();
        await render();
        return;
      }
      if (outcome === "handled") {
        await render();
        return;
      }

      switch (actionFor(keys, key.key, RAIL_ACTIONS)) {
        case "dismiss":
          await close(null);
          return;
        case "accept": {
          const row = rows[cursor];
          if (focus === "rail") {
            focus = "pane";
            break;
          }
          if (row?.kind === "section") {
            if (open.has(row.name)) open.delete(row.name);
            else open.add(row.name);
            rebuild();
            break;
          }
          const item = row?.kind === "item" ? row.item : undefined;
          if (!item || item.disabled) return;
          await close(item.value);
          return;
        }
        case "pane_next":
          focus = "pane";
          break;
        case "pane_prev":
          focus = "rail";
          break;
        case "next":
          if (focus === "rail") await moveRail(1);
          else movePane(1);
          break;
        case "prev":
          if (focus === "rail") await moveRail(-1);
          else movePane(-1);
          break;
        case "page_down":
          if (focus === "pane") movePane(height);
          break;
        case "page_up":
          if (focus === "pane") movePane(-height);
          break;
        case "first":
          if (focus === "pane") cursor = Math.max(0, rows.findIndex((r) => r.kind === "item"));
          break;
        case "last":
          if (focus === "pane") cursor = rows.length - 1;
          break;
        case "clear":
          query = "";
          rebuild();
          break;
        case "delete_word":
          query = dropSegment(query);
          rebuild();
          break;
        default: {
          if (key.key.code.kind === "backspace") {
            query = query.slice(0, -1);
            rebuild();
            break;
          }
          if (key.key.code.kind !== "char" || key.key.mods.ctrl || key.key.mods.alt) return;
          // Typing is about the list, wherever the focus is: nobody switches panes first.
          focus = "pane";
          query += key.key.code.c;
          cursor = 0;
          rebuild();
          if (rows[cursor]?.kind !== "item") movePane(1);
          break;
        }
      }
      await render();
    }, { desc: "rail picker key" }),
  );

  await neosh.focus.push(win);
  disposers.push(await neosh.keymap.capture(win, command));
  await bindWidgetKeys(neosh, win, command, keys, opts.ownKeys ?? []);
  await load();
  await render();
  return done;
}
