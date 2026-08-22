/**
 * Everything the chosen model can be told, on one surface.
 *
 * # Why this is not a picker
 *
 * It used to be two: a list of knobs, and then a list of that knob's values. Which meant the answer
 * to "what can I change about this model" was three keystrokes and a modal you had to back out of
 * to see the other knob — and, on the common path, no answer at all: with exactly one knob the
 * outer list was skipped, so `^E` opened a list of effort levels and nothing anywhere said that
 * fast mode or the context window existed. A setting you cannot see is a setting you do not have.
 *
 * So: every knob at once, one row each, values laid out along the row with the current one lit.
 * `h`/`l` moves along a row and `j`/`k` between them — the arrows do the same — and what you are
 * looking at *is* the state: there is no "current value" to go and read somewhere else.
 *
 * # Why it is a panel and not a private key handler
 *
 * Because ADR 0040 says so, and the sidebar proved why: every key here is an ordinary binding
 * pointed at a named command, scoped to this buffer's **kind**. `^Z` lists them, `^K` runs them,
 * and `init.ts` can move any of them. A `switch` on the key inside this file is exactly the thing
 * that rule replaced.
 *
 * # What is drawn, and why it moves
 *
 * A value's prominence is its position on its own ladder — `Option.Step1`…`Step5`, which is weight
 * rather than hue, because a reasoning level is a *scale* and the palette keeps its three hues for
 * states. The exception is a level that is not on the ladder at all: one bought by putting a word
 * in the message rather than by sending a parameter. That gets `Option.Beyond`, the one group in
 * the theme that animates a rainbow — because what it has to say is "this is not one of the normal
 * settings", and no amount of amber says that.
 */

import type {
  Disposable,
  FloatOptions,
  ModelEntry,
  ModelSelection,
  Neosh,
  OptionSelection,
  ProviderOptionDescriptor,
} from "@neosh/api";
import { byteLength, clipToWidth, padToWidth, width } from "@neosh/api";

const KIND = "neosh.model.options";
const NS = "neosh.model.options";

/** One value a knob can take. */
interface Choice {
  id: string;
  label: string;
  detail: string;
  /** Applied by saying it in the message rather than by sending it. */
  spoken: boolean;
}

/** One knob, flattened so a boolean and a select draw identically. */
interface Row {
  id: string;
  label: string;
  description: string;
  choices: Choice[];
  /** Which choice is in effect. */
  at: number;
  /** Booleans come back as flags, selects as strings. */
  boolean: boolean;
}

interface Sheet {
  win: number;
  buf: number;
  ns: number;
  rows: Row[];
  /** Which row the keys act on. */
  cursor: number;
  selection: ModelSelection;
  title: string;
  /** Inside the border. What the description line has to fit in. */
  inner: number;
  /** What the float was opened with, so `configure` can change one field without losing the rest. */
  config: FloatOptions;
  /** Whether anything spoken is in effect, so the border is only redone when the answer changes. */
  beyond: boolean;
  close(): Promise<void>;
}

let sheet: Sheet | null = null;

/**
 * The keys, in the order they were pressed.
 *
 * Every verb here is several round trips — set the lines, clear the marks, set a dozen more, hand
 * the selection back to the agent — and the host dispatches each key as its own command *without
 * waiting for the last one to finish*. Three keys in flight at once therefore finish in length
 * order rather than press order, and the shortest verb is `done`: press `→ → ⏎` quickly and the
 * panel shuts first, leaving two redraws to fail against a window that is already gone. Chaining
 * them costs nothing — they were serial in effect anyway, because they all touch one buffer — and
 * it is the same reason the sidebar serialises its own redraws.
 */
let queue: Promise<void> = Promise.resolve();

/**
 * Turn what a driver said into rows.
 *
 * A boolean becomes a two-value row, because "off / on" laid out like every other ladder is one
 * fewer shape to learn than a checkbox that behaves differently from the row above it — and it
 * gives the spoken switches somewhere to put their word.
 */
function rowsFrom(
  descriptors: readonly ProviderOptionDescriptor[],
  chosen: readonly OptionSelection[],
): Row[] {
  return descriptors.map((d) => {
    const value = chosen.find((o) => o.id === d.id)?.value;
    if (d.type === "boolean") {
      const on = typeof value === "boolean" ? value : d.current_value;
      // The word, if this switch is one. A switch that is a word is not a parameter at all — see
      // `prompt_injected_word` — so it wears the same mark as a spoken level.
      const spoken = typeof d.prompt_injected_word === "string";
      return {
        id: d.id,
        label: d.label,
        description: d.description ?? "",
        choices: [
          { id: "off", label: "off", detail: "", spoken: false },
          { id: "on", label: "on", detail: "", spoken },
        ],
        at: on ? 1 : 0,
        boolean: true,
      };
    }
    const injected = d.prompt_injected_values ?? [];
    const current =
      (typeof value === "string" ? value : undefined) ??
      d.current_value ??
      d.options.find((o) => o.is_default)?.id;
    const choices = d.options.map((o) => ({
      id: o.id,
      label: o.label,
      // What the *driver* said about this level, and nothing invented. A value with nothing to
      // say falls through to what the knob says, which is better than a row that reads "the
      // default" where the sentence explaining the control used to be.
      detail: o.description ?? "",
      spoken: injected.includes(o.id),
    }));
    return {
      id: d.id,
      label: d.label,
      description: d.description ?? "",
      choices,
      at: Math.max(0, choices.findIndex((c) => c.id === current)),
      boolean: false,
    };
  });
}

/**
 * Which step of the five-rung ramp a value sits on.
 *
 * Computed from where it is in *its own* ladder rather than from its name, because the ladders are
 * different lengths — five levels on Claude, three on an aggregator, two on Gemini — and a `high`
 * that is the top rung of one and the middle of another should not be drawn the same weight.
 */
function step(at: number, of: number): string {
  if (of <= 1) return "Option.Step3";
  const rung = Math.round((at / (of - 1)) * 4) + 1;
  return `Option.Step${Math.min(5, Math.max(1, rung))}`;
}

/**
 * The group a value is drawn in, given whether it is the one in effect.
 *
 * The ramp is measured over the levels that are *sendable*, so a ladder with a spoken level bolted
 * past the top of it does not compress: five rungs stay five rungs, and the word on the end is
 * drawn as the thing off the scale that it is.
 */
function groupFor(row: Row, at: number, live: boolean, focused: boolean): string {
  if (!live) return "Option.Unset";
  const choice = row.choices[at]!;
  if (choice.spoken) return "Option.Beyond";
  // The row the keys are on says so, and says it where you are looking: at the value they would
  // move. A ladder with one rung lit reads identically whether or not `←→` do anything to it —
  // which is how a live control came to look like a summary, and why `↵` on it felt like a key
  // that does nothing. The ramp is still on every other row, where it is a statement about the
  // setting rather than about the cursor.
  if (focused) return "Option.Cursor";
  // A switch is the ends of the scale and nothing in between, said explicitly rather than left to
  // the ramp: a spoken switch has one ordinary value and one word, and a ladder of length one puts
  // its only rung in the middle — so `off` on the switch beside it came out brighter than `off` on
  // an ordinary one, for no reason a reader could ever recover.
  if (row.boolean) return at === 0 ? "Option.Step1" : "Option.Step5";
  const ladder = row.choices.filter((c) => !c.spoken);
  return step(ladder.findIndex((c) => c.id === choice.id), ladder.length);
}

/** One row's worth of text, and where every value landed in it. */
interface Painted {
  text: string;
  /** `[start, end]` byte offsets, one per choice. */
  spans: [number, number][];
  /** The two chevrons, same shape, so the caller can colour them by whether they can move. */
  rails: [[number, number], [number, number]];
}

/**
 * A row, with the rail that says it is one.
 *
 * The chevrons are the whole reason this function changed. Every row here is a horizontal control
 * and nothing on screen said so: `↵` was the only key that visibly did anything, it means "done",
 * and a panel whose one working key closes it is a panel that does nothing. `‹` and `›` are on
 * every row — the shape is learnt once — and they light on the row the keys act on, dimming at
 * whichever end you have reached, so where you are on the ladder is readable without counting.
 */
function paint(row: Row, labelWidth: number, mark: string, glyph: string): Painted {
  let text = `${mark}${padToWidth(row.label, labelWidth)}  `;
  const leftStart = byteLength(text);
  text += "‹";
  const left: [number, number] = [leftStart, byteLength(text)];
  const spans: [number, number][] = [];
  for (const choice of row.choices) {
    const label = choice.spoken ? `${glyph}${choice.label}` : choice.label;
    const start = byteLength(text);
    text += ` ${label} `;
    spans.push([start, byteLength(text)]);
  }
  const rightStart = byteLength(text);
  text += "›";
  return { text, spans, rails: [left, [rightStart, byteLength(text)]] };
}

async function draw(neosh: Neosh, s: Sheet, hints: string): Promise<void> {
  // The panel this frame is about may have been dismissed while the frame was being computed —
  // every line of it is a round trip. Drawing into a window that is gone is this widget racing
  // itself, not something the user did wrong.
  if (sheet !== s) return;
  const glyph = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false)) ? "*" : "✦ ";
  const labelWidth = Math.max(...s.rows.map((r) => width(r.label)));
  const painted = s.rows.map((r, i) => paint(r, labelWidth, i === s.cursor ? "❯ " : "  ", glyph));

  const here = s.rows[s.cursor];
  // A description line that appeared and disappeared with the cursor would move every row below it
  // on every keystroke, so it has a permanent home at the foot. What it says is whatever the driver
  // said about the value under the cursor, falling back to what it said about the knob.
  // Clipped rather than allowed to size the float: this line changes with every keystroke, and a
  // panel that got wider when the cursor moved onto the row with the long sentence would be a panel
  // that never stops moving.
  const said = clipToWidth(here?.choices[here.at]?.detail || here?.description || "", s.inner - 3);
  const lines = [...painted.map((p) => p.text), "", `  ${said}`, ` ${hints}`];
  await neosh.buf.setLines(s.buf, 0, -1, lines);

  // The whole surface says when the next turn is not an ordinary one. A word in the message is
  // invisible everywhere else — it never reaches the transcript, and the panel it was chosen in
  // will have been shut for an hour by the time it matters — so while one is in effect the border
  // carries the same rainbow the value does. Reconfigured only when the answer *changes*: this is
  // a round trip, and one per keystroke on a control that already makes several is how a panel
  // starts lagging behind the keys being pressed. Clock-driven rather than a one-shot, so it is
  // safe against a redraw that mints every mark again (see ADR 0045).
  const beyond = s.rows.some((r) => r.choices[r.at]?.spoken);
  if (beyond !== s.beyond) {
    s.beyond = beyond;
    await neosh.float
      .configure(s.win, { ...s.config, borderHl: beyond ? "Option.Beyond" : undefined })
      .catch(() => {});
  }

  // Cleared before anything is drawn, every frame. A mark clamps rather than dies when the line
  // under it is replaced, so a row rewritten on every keypress otherwise accumulates them and ends
  // up painted by the narrowest one from three keystrokes ago.
  await neosh.ns.clear(s.ns, s.buf);
  for (let i = 0; i < s.rows.length; i++) {
    const row = s.rows[i]!;
    const p = painted[i]!;
    if (i === s.cursor) {
      await neosh.ns.mark(s.ns, s.buf, i, 0, { hlGroup: "Comment", endCol: byteLength("❯ ") });
    }
    await neosh.ns.mark(s.ns, s.buf, i, byteLength(i === s.cursor ? "❯ " : "  "), {
      hlGroup: i === s.cursor ? "Title" : "Comment",
      endCol: byteLength(p.text.slice(0, p.spans[0]?.[0] ?? 0)),
    });
    for (let c = 0; c < row.choices.length; c++) {
      const [start, end] = p.spans[c]!;
      const live = c === row.at;
      // One mark, not a band under a colour: the renderer takes the highest-priority ranged group
      // covering a character and stops, so two of them here would draw one and silently drop the
      // other. `Option.Step*` carries the band as well as the step for exactly that reason.
      await neosh.ns.mark(s.ns, s.buf, i, start, {
        hlGroup: groupFor(row, c, live, i === s.cursor),
        endCol: end,
      });
    }
    // Lit on the row the keys act on, and only at the end there is somewhere to go. A chevron that
    // stayed bright at `max` would promise a level that is not there.
    const ends = [row.at > 0, row.at < row.choices.length - 1];
    for (let r = 0; r < 2; r++) {
      const [start, end] = p.rails[r]!;
      await neosh.ns.mark(s.ns, s.buf, i, start, {
        hlGroup: i === s.cursor && ends[r] ? "Option.RailLive" : "Option.Rail",
        endCol: end,
      });
    }
  }
  const detail = s.rows.length + 1;
  await neosh.ns.mark(s.ns, s.buf, detail, 0, {
    // What a spoken level says about itself is said in the register it belongs to. The sentence
    // under a rainbow value is the one explaining why it is not a level, and drawing it in the
    // same grey as "how much reasoning the model spends" buries the difference.
    hlGroup: here?.choices[here.at]?.spoken ? "Option.Beyond" : "Picker.Detail",
    endCol: byteLength(lines[detail] ?? ""),
  });
  await neosh.ns.mark(s.ns, s.buf, detail + 1, 0, {
    hlGroup: "Sidebar.Dim",
    endCol: byteLength(lines[detail + 1] ?? ""),
  });
  await neosh.win.setCursor(s.win, s.cursor, 0);
}

/**
 * Put the whole sheet into effect.
 *
 * Every keystroke, rather than on the way out. Changing a reasoning level is reversible — you can
 * see what it is and put it back — and "everything irreversible asks, and nothing reversible does"
 * cuts both ways: a control that only takes effect when you press the right key to leave is a
 * control you have to be taught. The footer moves as you move, which is the whole feedback loop.
 */
async function apply(
  neosh: Neosh,
  s: Sheet,
  refresh: (flash?: boolean) => Promise<void>,
): Promise<void> {
  const options: OptionSelection[] = s.rows.map((row) => {
    const choice = row.choices[row.at]!;
    return {
      id: row.id,
      value: row.boolean ? row.at === 1 : choice.id,
    };
  });
  s.selection = { ...s.selection, options };
  await neosh.agent.setSelection(s.selection);
  await refresh();
}

/**
 * The keys, as words.
 *
 * The key that *changes something* first: it used to be third, behind `↵`, which closes. Read in
 * order, the old line taught you that the way to use this panel was to dismiss it.
 */
function hintsFor(): string {
  // Letters rather than arrows, and not because the arrows do not work — they are bound too. The
  // row is a promise about a keyboard, and `h j k l` is a promise every keyboard keeps.
  return "h l change   j k knob   ⇥ cycle   ↵ done   esc close";
}

export async function installOptions(
  { neosh, subscriptions }: { neosh: Neosh; subscriptions: Disposable[] },
  refresh: (flash?: boolean) => Promise<void>,
): Promise<void> {
  const scope = { kind: "buf_kind", name: KIND } as const;

  /** Register a verb and bind it inside the sheet. See the sidebar's `verb` — same reasoning. */
  const verb = async (
    name: string,
    keys: string[],
    desc: string,
    fn: (s: Sheet) => Promise<void> | void,
  ): Promise<void> => {
    subscriptions.push(
      await neosh.cmd.register(name, () => {
        const mine = queue.then(async () => {
          if (!sheet) return;
          await fn(sheet);
        });
        // One verb that fails must not stop the ones behind it: the panel would look alive and
        // ignore every key from then on, which is worse than the failure it is hiding.
        queue = mine.catch((e: unknown) => neosh.notify(String(e), "warn"));
        return mine;
      }, { desc }),
    );
    for (const key of keys) {
      await neosh.keymap.set("chat", key, name, { scope, desc });
    }
  };

  const redraw = async (s: Sheet) => draw(neosh, s, hintsFor());

  const move = async (s: Sheet, delta: number) => {
    s.cursor = Math.min(s.rows.length - 1, Math.max(0, s.cursor + delta));
    await redraw(s);
  };

  /**
   * Along the current row.
   *
   * `wrap` is the difference between the arrows and the cycle key, and it is deliberate: an arrow
   * that teleported from `max` back to `low` would be the same surprise as a downgrade key that
   * lands on the most expensive model in the catalogue, which `ModelTier::step` refuses to be. A
   * key whose whole job is "the next one" may wrap, because that is what it says it does.
   */
  const shift = async (s: Sheet, delta: number, wrap: boolean) => {
    const row = s.rows[s.cursor];
    if (!row) return;
    const n = row.choices.length;
    row.at = wrap
      ? (row.at + delta + n) % n
      : Math.min(n - 1, Math.max(0, row.at + delta));
    // Drawn before it is applied. Both are round trips and the panel is where you are looking, so
    // putting the several calls that update the *strip* first makes every keypress wait on work
    // whose result is behind the window you are reading — and keys arriving meanwhile queue up
    // against a panel that has not caught up with the last one.
    await redraw(s);
    await apply(neosh, s, refresh);
  };

  // No `^N`/`^P` here, and none needed: the float is **modal**, so nothing global resolves while
  // it is up. This used to be a list of keys deliberately left unbound so that `^N` and `^P` kept
  // working — which was the wrong half of the problem. Shadowing a key is how a panel keeps the
  // one key it needs; it does nothing about the dozen it does not, and `^T`, `^G`, `^L` and the
  // rest all fell through and opened something behind this. A control you are in the middle of
  // using owns the keyboard until you leave it.
  await verb(`${NS}.next`, ["j", "<Down>"], "Next knob", (s) => move(s, 1));
  await verb(`${NS}.prev`, ["k", "<Up>"], "Previous knob", (s) => move(s, -1));
  await verb(`${NS}.higher`, ["l", "<Right>"], "More", (s) => shift(s, 1, false));
  await verb(`${NS}.lower`, ["h", "<Left>"], "Less", (s) => shift(s, -1, false));
  await verb(`${NS}.cycle`, ["<Tab>", "<Space>"], "The next value along", (s) => shift(s, 1, true));
  // `^E` closes it too, the way `^S` leaves reading the transcript. A key that opens a surface and
  // cannot shut it is one you have to remember a second key for — and with the panel modal, the
  // global `^E` no longer arrives to do it.
  await verb(
    `${NS}.close`,
    ["<Esc>", "q", "<C-c>", "<C-e>"],
    "Back to the composer",
    (s) => s.close(),
  );
  // Nothing left to apply — every change is already in effect — so what this adds is where the
  // acknowledgement *goes*: the panel shuts and the strip it moved to flashes, which is the answer
  // to "fine, but where does that live now". Deliberately not a flash inside the panel: anything
  // that holds the window open after the key that dismissed it is a window still eating keys, and
  // a swallowed `^P` costs more than a lit row is worth.
  await verb(`${NS}.done`, ["<CR>"], "Done", async (s) => {
    await s.close();
    await refresh(true);
  });

  subscriptions.push({ dispose: () => void sheet?.close() });
}

/**
 * Open the sheet for whatever model this conversation is on.
 *
 * Reads the descriptors afresh every time rather than caching them: they arrive from the *driver*,
 * and a driver that learned what it accepts by asking — `codex` does — reports a different set
 * after a refresh than it did at startup.
 */
export async function openOptions(neosh: Neosh): Promise<void> {
  if (sheet) {
    await sheet.close();
    return;
  }
  const current = await neosh.agent.selection().catch(() => null);
  if (!current) {
    neosh.notify("no model is selected", "warn");
    return;
  }
  const entries = await neosh.agent.listModels(current.instance).catch(() => [] as ModelEntry[]);
  const model = entries.find((e) => e.model.id === current.model)?.model;
  const descriptors = model?.capabilities?.option_descriptors ?? [];
  if (descriptors.length === 0) {
    // Which model, because the answer to "why not" is almost always "not that one" — a small model
    // in a lineup whose big one has five levels, or an endpoint that told us only an id.
    neosh.notify(`${model?.display_name ?? current.model} has nothing to set`, "info");
    return;
  }

  const rows = rowsFrom(descriptors, current.options ?? []);
  const buf = await neosh.buf.create({
    name: "[model options]",
    scratch: true,
    kind: KIND,
  });
  const ns = await neosh.ns.create(NS);
  // Wide enough for the longest ladder plus its labels, and no wider: this is a control panel, not
  // a page, and a float that spans the terminal reads as something you have to finish reading.
  const labelWidth = Math.max(...rows.map((r) => width(r.label)));
  const widest = Math.max(
    // `+ 2` for the rail: `‹` and `›` are part of the row, and a ladder measured without them is
    // one whose last chevron falls off the right-hand edge on the widest row.
    ...rows.map((r) =>
      r.choices.reduce((n, c) => n + width(c.label) + (c.spoken ? 5 : 2) + 1, 0) + 2
    ),
    width(hintsFor()),
  );
  const inner = Math.min(96, labelWidth + widest + 8);
  const config: FloatOptions = {
    anchor: { kind: "screen" },
    width: { kind: "max", n: inner },
    height: { kind: "fixed", n: rows.length + 3 },
    border: "rounded",
    title: ` ${model?.display_name ?? current.model} `,
    focusable: true,
    closeOnBlur: true,
    // Nothing else reaches the keyboard while this is up. It is a control sheet you are in the
    // middle of using: `^N` over it opened a new conversation *behind* it, `^T` opened the project
    // panel and left focus somewhere neither of them expected, and the panel stayed on screen
    // through both. `^Q` and `^R` still work — see `ui.modal_escape_keys` — so this can never be
    // a terminal somebody has to kill.
    modal: true,
    z: 200,
  };
  const win = await neosh.float.open(buf, config);
  await neosh.focus.push(win);

  const s: Sheet = {
    win,
    buf,
    ns,
    rows,
    cursor: 0,
    selection: current,
    title: model?.display_name ?? String(current.model),
    inner,
    config,
    // What the border currently says, not what the rows currently are — so the first `draw` finds
    // them different and lights it, which is what opening straight onto `ultracode` needs.
    beyond: false,
    close: async () => {
      if (sheet !== s) return;
      sheet = null;
      await neosh.focus.pop().catch(() => {});
      await neosh.win.close(win).catch(() => {});
    },
  };
  sheet = s;
  await draw(neosh, s, hintsFor());
}
