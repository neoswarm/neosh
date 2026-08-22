/**
 * The question panel — what an agent gets when it asks *you* something.
 *
 * # What this is not
 *
 * It is not an approval prompt, and the difference is the whole reason this file exists. An
 * approval asks *may this happen*, every answer to it is a yes or a no, and policy can answer it
 * without waking anybody — which is exactly what `permissions.mode = allow` is for. A question asks
 * *which of these*, several times over, sometimes with more than one answer each and sometimes with
 * an answer nobody listed; and there is no mode, rule or allow-list that knows which database you
 * want.
 *
 * Routed through the permission path — which is where it was — a question did not merely read
 * badly. The configured default is full access, so the layer said yes before anybody was asked, the
 * driver sent back a bare yes carrying no answers, and `claude` reported to its own model: *"The
 * user did not answer the questions."* A turn asked you something, never showed it to you, and then
 * told the agent you had ignored it. See ADR 0043.
 *
 * # The shape of it
 *
 * **One question at a time.** A panel showing four at once is a form, and a form over a transcript
 * is a modal you have to finish before you can read what prompted it. The rail at the top right
 * says how many there are and which one this is, and `⇧⇥` goes back to change an earlier answer.
 *
 * **The composer is the field.** Typing is not filtering, it is *answering* — the option nobody
 * listed, which is the one people reach for most. It goes into the composer, where you can see it,
 * because that is the field on this screen and a second one drawn inside a float is a field you
 * have to notice. While there is something typed the numbered shortcuts go away, which is how the
 * panel says a digit is a character now. See ADR 0037.
 *
 * **Every key is a named command.** `question.accept`, `question.next` — so `F1` lists them and an
 * `init.ts` can move them, and a third party can bind their own against the `neosh.question` buffer
 * kind without forking this. Only the printable keys come through the raw capture, and only because
 * they are text rather than verbs. See ADR 0040.
 *
 * **A question belongs to its conversation.** Several turns run at once, and one of them asking is
 * not a reason to take the screen away from the one you are reading. A question for a conversation
 * that is not on screen waits, says so in the status line, and opens the moment you switch to it.
 */

import type {
  HookOutcome,
  HookPayload,
  Neosh,
  PluginContext,
  QuestionAnswer,
  SessionId,
  UserQuestion,
} from "@neosh/api";
import { byteLength } from "@neosh/api";
import type { DrawnMark, DrawnRow } from "@neosh/api";

/**
 * How long a question may sit unanswered.
 *
 * Long, and deliberately much longer than the approval prompt's two minutes. An approval is asked
 * about something already in flight and you are at the keyboard; a question is asked *of* you and
 * the honest answer is often "let me go and look". Nothing is lost by waiting — the agent is
 * blocked either way — and `<Esc>` ends it the moment you want it ended.
 *
 * It is still finite, because a blocking hook is what the host waits on: past this the host reads
 * it as a refusal and the agent is told nobody answered, which is true.
 */
const ASK_TIMEOUT_MS = 30 * 60 * 1000;

/** What the buffer declares itself to be. Bind against this to take a key in the panel. */
const KIND = "neosh.question";

/** The most options a digit can reach. Ten would be `0`, which reads as "none of them". */
const SHORTCUTS = 9;

/** How much of the row the label column may take before the description gives up its place. */
const LABEL_MAX = 28;

/**
 * The characters the panel is drawn with, at whatever fidelity the terminal has.
 *
 * A ticked box and an untouched one have to be the same width or the labels beside them step
 * sideways as you tick things — which is why the ASCII pair is `[x]`/`[ ]` and not `[x]`/`[]`.
 */
type Glyphs = {
  cursor: string;
  blank: string;
  radioOn: string;
  radioOff: string;
  boxOn: string;
  boxOff: string;
  dotHere: string;
  dotDone: string;
  dotTodo: string;
  pen: string;
};

const UNICODE: Glyphs = {
  cursor: "\u276f",
  blank: " ",
  radioOn: "\u25c9",
  radioOff: "\u25cb",
  boxOn: "\u2611",
  boxOff: "\u2610",
  dotHere: "\u25c9",
  dotDone: "\u25cf",
  dotTodo: "\u25cb",
  pen: "\u270e",
};

const ASCII: Glyphs = {
  cursor: ">",
  blank: " ",
  radioOn: "(*)",
  radioOff: "( )",
  boxOn: "[x]",
  boxOff: "[ ]",
  dotHere: "@",
  dotDone: "*",
  dotTodo: ".",
  pen: "?",
};

/**
 * Display columns, as closely as anything outside the frontend can know.
 *
 * Code points, not bytes — every layout sum here is about *where a thing lands on screen*, and
 * `\u276f` is one column and three bytes. Padding computed in bytes is how the row with the cursor
 * on it ends up two columns shorter than every other row, which is what this panel did until the
 * first screenshot of it. Byte offsets are still what the marks are built from; the two units have
 * different jobs and this is the one that is about width.
 */
function cols(s: string): number {
  return [...s].length;
}

/** `text`, padded on the right to `n` columns. */
function pad(text: string, n: number): string {
  return text + " ".repeat(Math.max(0, n - cols(text)));
}

type Draft = {
  /** Labels taken, in the order they were taken. */
  picked: string[];
  /** What was typed instead. Non-empty means the options are not the answer. */
  typed: string;
};

/** One question the panel has been asked and not yet answered. */
type Ask = {
  session: SessionId | null;
  questions: UserQuestion[];
  /** By question text, which is the key the agent looks answers up under. */
  drafts: Map<string, Draft>;
  /** Which question is on screen. */
  at: number;
  /** Which option is under the cursor, for the question on screen. */
  cursor: number;
  /** Called once, with the answers or `null` for "nobody answered". */
  settle: (answers: QuestionAnswer[] | null) => void;
  settled: boolean;
};

export async function activate({ neosh, subscriptions }: PluginContext) {
  /** Everything asked and not answered, oldest first. */
  const queue: Ask[] = [];
  /** The one on screen, if any. */
  let open: Open | null = null;
  /**
   * Which conversation is being looked at, or `null` while that is still being asked.
   *
   * `null` shows everything rather than nothing. Every `await` before the hook is registered is a
   * window in which a question arrives and finds no blocking hook — which is answered as "nobody
   * answered", instantly and wrongly — so the hook goes on first and this is filled in behind it.
   * A question in that window belongs to whichever conversation is on screen, because on a
   * workspace this young there is only one.
   */
  let active: SessionId | null = null;

  /** Whichever queued question belongs to the conversation being looked at. */
  const showable = () =>
    queue.find((a) => a.session === null || active === null || a.session === active) ?? null;

  // What is in the composer, mirrored — there is no call that reads it, only an event that says it
  // changed. Kept only while nothing is on screen: once the panel is up it is the panel writing the
  // composer, and every change from then on is an echo of something it just did.
  let draft = "";
  subscriptions.push(
    neosh.agent.onComposerChange(({ text }) => {
      if (!open) draft = text;
    }),
  );

  /**
   * Put the panel in step with the queue: open the one for this conversation, close what is not.
   *
   * One function rather than a call at every site that changes either, because the two things that
   * move — which conversation you are looking at, and what is waiting — change independently and
   * every combination of them has to end up here.
   */
  // One at a time. `sync` awaits a window opening and several round trips inside it, and two
  // callers arriving in that gap — a question landing while a conversation switch is still being
  // handled — would both find no panel open and both draw one, leaving two panels registering the
  // same command names against one keyboard.
  let running: Promise<void> = Promise.resolve();
  const sync = () => {
    running = running.then(step, step);
    return running;
  };

  const step = async () => {
    const want = showable();
    if (open && open.ask !== want) {
      await open.close();
      open = null;
    }
    if (want && !open) {
      // Whatever was half-typed becomes the seed of the answer. A question that landed over a
      // sentence somebody was writing should take that sentence, not throw it away — it is very
      // often exactly what they were about to say.
      const seed = draft;
      draft = "";
      open = await draw(neosh, want, seed, () => {
        // Answered, dismissed, or gone. Either way it leaves the queue and the next one — which
        // may belong to another conversation entirely — gets its turn at the screen.
        const i = queue.indexOf(want);
        if (i >= 0) queue.splice(i, 1);
        open = null;
        void sync();
      });
    }
    await elsewhere(neosh, queue, open?.ask ?? null);
  };

  // First, before anything that awaits. A question arriving while this plugin is still starting up
  // finds no blocking hook, and "no blocking hook" is answered as "nobody answered" — instantly,
  // and to an agent that was never given the chance to be told otherwise.
  subscriptions.push(
    await neosh.hook.register(
      "ask_user",
      async (payload): Promise<HookOutcome> => {
        if (payload.hook !== "ask_user") return { action: "continue" };
        if (payload.questions.length === 0) return { action: "continue" };

        const ask: Ask = {
          session: payload.session ?? null,
          questions: payload.questions,
          drafts: new Map(),
          at: 0,
          cursor: 0,
          settle: () => {},
          settled: false,
        };
        const answered = new Promise<QuestionAnswer[] | null>((resolve) => {
          ask.settle = resolve;
        });
        queue.push(ask);
        void sync();

        const answers = await answered;
        // Nothing, rather than an empty list: the two are different on the wire and the difference
        // is the sentence the agent is given. A veto's reason is passed to the model verbatim.
        if (!answers || answers.length === 0) {
          return {
            action: "veto",
            reason: "The user dismissed the question without answering.",
          };
        }
        const next: HookPayload = { ...payload, answers };
        return { action: "modify", payload: next };
      },
      { blocking: true, timeoutMs: ASK_TIMEOUT_MS },
    ),
  );

  // The colours, after the hook rather than before it. A group defined late still colours a mark
  // that is already on screen — the theme is consulted when the row is drawn — so nothing is lost
  // by not making a question wait for fourteen round trips.
  await defineGroups(neosh);
  active = await neosh.session.current().then((s) => s.id).catch(() => null);
  void sync();

  subscriptions.push(
    neosh.session.onChange(({ session }) => {
      active = session;
      void sync();
    }),
  );

  // A turn that ends is a turn that is not waiting on an answer any more — it was interrupted, or
  // it failed, and the driver has already told the agent so. Without this the panel would sit there
  // with nothing behind it: the question it belongs to has no reader left, and answering it would
  // write into a pipe that has moved on.
  subscriptions.push(
    neosh.agent.onTurnEnd(({ session }) => {
      let moved = false;
      for (const ask of [...queue]) {
        if (ask.session !== session) continue;
        finish(ask, null);
        const i = queue.indexOf(ask);
        if (i >= 0) queue.splice(i, 1);
        moved = true;
      }
      if (moved) void sync();
    }),
  );

  subscriptions.push({
    dispose() {
      // Unloading with a question on screen must not leave a turn blocked on a panel that no
      // longer has anybody drawing it.
      for (const ask of queue.splice(0)) finish(ask, null);
      void open?.close();
      open = null;
      void neosh.status.clear("question");
    },
  });
}

/** Settle an ask exactly once. */
function finish(ask: Ask, answers: QuestionAnswer[] | null): void {
  if (ask.settled) return;
  ask.settled = true;
  ask.settle(answers);
}

/**
 * Tell the footer about a question that is *not* on this screen.
 *
 * The one case where a status segment is the whole of the UI: a conversation you are not looking at
 * has stopped and is waiting on you, and nothing else on screen would ever say so. A row that named
 * the conversation would be better and cannot be — the footer has one line and it is shared — so it
 * says how many, and `^T` is where you go.
 */
async function elsewhere(neosh: Neosh, queue: Ask[], shown: Ask | null): Promise<void> {
  const waiting = queue.filter((a) => a !== shown).length;
  if (waiting === 0) {
    await neosh.status.clear("question").catch(() => {});
    return;
  }
  await neosh.status
    .set("question", {
      text: waiting === 1 ? "1 conversation is asking" : `${waiting} conversations are asking`,
      keys: "^T",
      hl: "Question.Waiting",
      priority: 40,
    })
    .catch(() => {});
}

// ---------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------

interface Open {
  ask: Ask;
  close(): Promise<void>;
}

/**
 * Put one ask on screen and drive it until it is answered or dismissed.
 *
 * Returns as soon as it is drawn — the answering happens on keys, and `done` is called when the ask
 * has settled so the caller can move on to the next one.
 */
async function draw(neosh: Neosh, ask: Ask, seed: string, done: () => void): Promise<Open> {
  const buf = await neosh.buf.create({ name: "[question]", scratch: true, kind: KIND });
  const ns = await neosh.ns.create("neosh.questions");

  // As wide as the conversation, so the panel reads as part of the column it interrupts rather than
  // as a box that landed on it. Asked rather than computed: only the frontend knows how much room
  // the docks left, and `fill` would span the sidebar too.
  const width = await columnWidth(neosh);
  // The setting, not a guess about the terminal: `ui.ascii_only` is what the user says when their
  // font cannot draw a ballot box, and every other part of neosh reads the same option.
  const glyphs = (await neosh.opt.get<boolean>("ui.ascii_only").catch(() => false))
    ? ASCII
    : UNICODE;

  const win = await neosh.float.open(buf, {
    anchor: { kind: "dock", dock: "bottom" },
    width: { kind: "fixed", n: width },
    height: { kind: "auto" },
    border: "rounded",
    borderHl: "Question.Border",
    focusable: true,
    // Managed here, not by blur. Focus moves for a dozen ordinary reasons and a question that
    // vanished when it did would be a question you cannot answer and cannot get back.
    closeOnBlur: false,
    z: 180,
  });

  /** What is in the composer, which is the answer nobody listed. */
  let typed = "";
  let closed = false;

  const q = () => ask.questions[ask.at]!;
  const draftOf = (question: UserQuestion): Draft =>
    ask.drafts.get(question.question) ?? { picked: [], typed: "" };

  const render = async () => {
    if (closed) return;
    const rows = compose(ask, typed, width, glyphs);
    await neosh.buf.render(buf, ns, 0, -1, rows.rows).catch(() => {});
    // The caret goes where you are acting: on the row under the cursor, and at the end of what you
    // have typed once you are typing. Without it the terminal's caret parks in the corner of a
    // panel that is plainly listening, and a field with no insertion point in it reads as inert.
    await neosh.win.setCursor(win, rows.caret, rows.caretCol).catch(() => {});
  };

  const close = async () => {
    if (closed) return;
    closed = true;
    for (const d of disposers) d.dispose();
    await neosh.focus.pop().catch(() => {});
    await neosh.win.close(win).catch(() => {});
    // What is in the composer is deliberately left there. An answer that was *taken* has already
    // been cleared by whoever took it; anything still sitting there is a sentence somebody typed
    // and did not send — often the one they were half-way through when the question landed on top
    // of it — and a panel that wiped the composer on its way out would be a panel that ate a
    // message for the crime of being interrupted.
  };

  const settle = async (answers: QuestionAnswer[] | null) => {
    finish(ask, answers);
    await close();
    done();
  };

  /**
   * Take the answer to the question on screen, and go on to the next one.
   *
   * The one place an answer is written down, so "what counts as answered" has a single definition:
   * something typed beats something chosen, because typing over a selection is how you change your
   * mind, and a question with neither is not answered at all.
   */
  const advance = async (): Promise<void> => {
    const question = q();
    const answer = typed.trim()
      ? { question: question.question, answers: [typed.trim()], custom: true }
      : {
        question: question.question,
        answers: draftOf(question).picked,
        custom: false,
      };
    if (answer.answers.length === 0) return;
    ask.drafts.set(question.question, {
      picked: answer.custom ? [] : answer.answers,
      typed: answer.custom ? answer.answers[0]! : "",
    });
    if (typed) {
      typed = "";
      await neosh.agent.setDraft("").catch(() => {});
    }

    // The first one still unanswered, which is not always the next one along: `⇧⇥` goes back to
    // change an earlier answer, and finishing that one should return you to where you were rather
    // than walk you forward through questions you have already done.
    const pending = ask.questions.findIndex((qq) => resolve(ask, qq).length === 0);
    if (pending < 0) {
      await settle(
        ask.questions.map((qq) => {
          const answers = resolve(ask, qq);
          return { question: qq.question, answers, custom: draftOf(qq).typed.length > 0 };
        }),
      );
      return;
    }
    ask.at = pending;
    ask.cursor = 0;
    await render();
  };

  const move = async (delta: number) => {
    const n = q().options.length;
    if (n === 0) return;
    ask.cursor = (ask.cursor + delta + n) % n;
    await render();
  };

  /** Take an option, by index. Single-select answers with it; multi-select toggles it. */
  const take = async (index: number) => {
    const question = q();
    const option = question.options[index];
    if (!option) return;
    // Typing wins over the rows while there is anything typed — but reaching for a row is a plain
    // statement that the rows are the answer after all, so it clears what was typed rather than
    // being ignored underneath it.
    if (typed) {
      typed = "";
      await neosh.agent.setDraft("").catch(() => {});
    }
    ask.cursor = index;
    const draft = draftOf(question);
    if (question.multi_select) {
      const picked = draft.picked.includes(option.label)
        ? draft.picked.filter((l) => l !== option.label)
        : [...draft.picked, option.label];
      ask.drafts.set(question.question, { picked, typed: "" });
      await render();
      return;
    }
    ask.drafts.set(question.question, { picked: [option.label], typed: "" });
    await advance();
  };

  const retype = async (next: string) => {
    typed = next;
    await neosh.agent.setDraft(next).catch(() => {});
    await render();
  };

  const disposers: Array<{ dispose(): void }> = [];
  const cmd = async (name: string, desc: string, fn: () => Promise<void>) => {
    disposers.push(await neosh.cmd.register(name, fn, { desc }));
  };

  await cmd("question.accept", "Answer this question and go on to the next", async () => {
    if (typed.trim()) {
      await advance();
      return;
    }
    // Nothing ticked, on a question that takes one answer: `↵` takes the row under the cursor,
    // because that is what a cursor on a row means. Arrow-then-enter is how the rows are chosen
    // without reaching for a digit, and a panel where it did nothing would be one where the cursor
    // was decoration.
    if (!q().multi_select) {
      await take(ask.cursor);
      return;
    }
    if (resolve(ask, q()).length > 0) {
      await advance();
      return;
    }
    // A question that takes several, with none of them ticked, is not answered — and `↵` has to
    // say so rather than do nothing, because a key that appears inert is one you press again.
    neosh.notify("tick at least one, or type your own answer", "info");
  });
  await cmd("question.next", "Next option", () => move(1));
  await cmd("question.prev", "Previous option", () => move(-1));
  await cmd("question.toggle", "Tick the option under the cursor", () => take(ask.cursor));
  await cmd("question.back", "Back to the previous question", async () => {
    if (ask.at === 0) return;
    ask.at -= 1;
    // Onto whatever was answered there, so going back and pressing `↵` re-affirms rather than
    // silently changing the answer to whichever row happened to be first.
    const picked = draftOf(q()).picked[0];
    ask.cursor = Math.max(0, q().options.findIndex((o) => o.label === picked));
    const was = draftOf(q()).typed;
    if (was !== typed) await retype(was);
    else await render();
  });
  await cmd("question.dismiss", "Dismiss the question without answering", () => settle(null));

  const scope = { scope: { kind: "buf_kind", name: KIND } as const };
  // In both modes, because `^S` puts the editor in `Normal` and a question that arrives while you
  // are reading the transcript is still a question. Every one of these is a *chord or a named key*
  // — never a printable character, which has to stay available for the answer you type.
  for (const mode of ["chat", "normal"] as const) {
    for (const [lhs, command, desc] of [
      ["<CR>", "question.accept", "Answer"],
      ["<Down>", "question.next", "Next option"],
      ["<Up>", "question.prev", "Previous option"],
      ["<C-n>", "question.next", "Next option"],
      ["<C-p>", "question.prev", "Previous option"],
      ["<Tab>", "question.toggle", "Tick this option"],
      ["<S-Tab>", "question.back", "Previous question"],
      ["<Esc>", "question.dismiss", "Dismiss"],
    ] as const) {
      await neosh.keymap.set(mode, lhs, command, { ...scope, desc });
      disposers.push({
        dispose: () => void neosh.keymap.del(mode, lhs, scope.scope).catch(() => {}),
      });
    }
  }

  // Everything nothing else claimed, which here means the text of an answer. Digits are the one
  // ambiguous case and the panel resolves it in the open: while nothing has been typed they are
  // shortcuts and the rows wear their numbers, and the moment something has been they are
  // characters and the numbers are gone.
  const sink = "neosh.questions.key";
  disposers.push(
    await neosh.cmd.register(sink, async (_args, key) => {
      if (!key || closed) return;
      const code = key.key.code;
      if (code.kind === "backspace") {
        if (typed) await retype(typed.slice(0, -1));
        return;
      }
      if (code.kind !== "char" || key.key.mods.alt) return;
      if (key.key.mods.ctrl) {
        if (code.c === "u") await retype("");
        else if (code.c === "w") await retype(dropWord(typed));
        return;
      }
      const c = code.c;
      if (!typed && c >= "1" && c <= "9") {
        await take(Number(c) - 1);
        return;
      }
      await retype(typed + c);
    }, { desc: "question: a character of your own answer" }),
  );

  await neosh.focus.push(win);
  disposers.push(await neosh.keymap.capture(win, sink));
  // `^U` and `^W` belong to the composer, and the capture only ever receives what no binding
  // claimed — so without these two the field they act on would stop answering to them mid-sentence.
  // Window-scoped, so they go back to the composer with the window.
  for (const lhs of ["<C-u>", "<C-w>"] as const) {
    const scoped = { kind: "window", win } as const;
    await neosh.keymap.set("chat", lhs, sink, { scope: scoped, desc: "Erase what you typed" });
    disposers.push({
      dispose: () => void neosh.keymap.del("chat", lhs, scoped).catch(() => {}),
    });
  }

  // Seeded from whatever was already half-typed when the question arrived, rather than thrown
  // away: the panel appeared over a field somebody was using, and what they had written is very
  // often the answer — it is the sentence they were about to send.
  typed = seed;
  await render();

  return { ask, close };
}

/** Erase back to the start of the last word, as `^W` does everywhere else. */
function dropWord(text: string): string {
  const trimmed = text.replace(/\s+$/, "");
  const at = trimmed.lastIndexOf(" ");
  return at < 0 ? "" : trimmed.slice(0, at + 1);
}

/**
 * How wide the conversation column is.
 *
 * The main dock's own width, minus the border this panel draws. `Extent::Fill` is the screen and
 * would run under the sidebar; a fixed guess is wrong on every terminal but the one it was written
 * on.
 */
async function columnWidth(neosh: Neosh): Promise<number> {
  const windows = await neosh.win.list().catch(() => []);
  const main = windows.find(
    (w) => w.layout.kind === "docked" && w.layout.dock === "main",
  );
  const port = main ? await neosh.win.viewport(main.win).catch(() => null) : null;
  const outer = port?.width ?? 76;
  return Math.max(28, Math.min(110, outer - 2));
}

/** The labels standing as the answer to `question`, chosen or typed. */
function resolve(ask: Ask, question: UserQuestion): string[] {
  const draft = ask.drafts.get(question.question);
  if (!draft) return [];
  if (draft.typed.trim()) return [draft.typed.trim()];
  return draft.picked;
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/** A mark, built where the text was so the offsets are the text's own. */
function mark(col: number, end: number, hl: string, priority?: number): DrawnMark {
  return { col, opts: { hlGroup: hl, endCol: end, ...(priority ? { priority } : {}) } };
}

/**
 * The whole panel, as rows.
 *
 * A pure function of the ask, what has been typed and how much room there is, which is what makes
 * it testable and what keeps a redraw from depending on the order the last one happened in.
 *
 * Two units, and they are not interchangeable. Every *layout* sum is in [`cols`] — code points,
 * the closest thing to display width available outside the frontend — because a row's padding is
 * about where things land on screen. Every *mark* is in bytes, computed from the string actually
 * written, because that is the unit every column on the wire uses.
 */
function compose(
  ask: Ask,
  typed: string,
  width: number,
  g: Glyphs,
): { rows: DrawnRow[]; caret: number; caretCol: number } {
  const question = ask.questions[ask.at]!;
  const custom = typed.trim().length > 0;
  const picked = new Set(ask.drafts.get(question.question)?.picked ?? []);
  const rows: DrawnRow[] = [];
  /** The left margin, shared by every row so the panel has one edge rather than several. */
  const LEFT = 2;
  const room = width - LEFT * 2;

  // --- the header: what this is about, and how far through you are -----------
  const header = clip(question.header.trim() || "Question", Math.max(8, room - 16));
  const rail = progress(ask, g);
  const gap = Math.max(1, room - cols(header) - cols(rail));
  const headText = `  ${header}${" ".repeat(gap)}${rail}`;
  const headMarks: DrawnMark[] = [
    // Shimmers, because the agent has stopped and is waiting on you. The one thing on this panel
    // that moves, and it is the thing that says why the panel is here at all.
    mark(LEFT, LEFT + byteLength(header), "Question.Header"),
  ];
  if (rail) {
    headMarks.push(mark(byteLength(headText) - byteLength(rail), byteLength(headText), "Question.Rail"));
  }
  rows.push({ text: headText, marks: headMarks });
  rows.push({ text: "" });

  // --- the question itself ---------------------------------------------------
  for (const line of wrap(question.question, room)) {
    const text = `  ${line}`;
    rows.push({ text, marks: [mark(LEFT, byteLength(text), "Question.Text")] });
  }
  rows.push({ text: "" });

  // --- the options -----------------------------------------------------------
  const [on, off] = question.multi_select ? [g.boxOn, g.boxOff] : [g.radioOn, g.radioOff];
  // One gutter for the whole list — the marker, a space, the box, a space — so the labels line up
  // whether or not the cursor is on the row.
  const gutter = cols(g.cursor) + 1 + cols(on) + 1;
  const labelWidth = Math.min(
    LABEL_MAX,
    question.options.reduce((w, o) => Math.max(w, cols(o.label)), 0),
  );
  const shortcuts = !custom && question.options.length > 1;
  const badgeWidth = shortcuts ? cols(String(Math.min(SHORTCUTS, question.options.length))) : 0;

  let caret = rows.length;
  let caretCol = 0;
  question.options.forEach((option, i) => {
    const here = i === ask.cursor;
    if (here) caret = rows.length;
    const taken = !custom && picked.has(option.label);
    const lead = pad(`${here ? g.cursor : g.blank} ${taken ? on : off} `, gutter);
    const label = pad(clip(option.label, labelWidth), labelWidth);
    const badge = shortcuts && i < SHORTCUTS ? String(i + 1) : "";
    // What is left for the description, once the row has paid for its gutter, its label column and
    // the shortcut hanging off the right edge.
    const forDetail = room - gutter - labelWidth - 2 - (badgeWidth ? badgeWidth + 2 : 0);
    const detail = option.description.trim();
    const shown = detail && forDetail >= 8 ? clip(detail, forDetail) : "";
    const body = shown ? `${label}  ${shown}` : label.trimEnd();
    // Right-aligned against the panel's own edge, so the shortcuts read as a column rather than as
    // punctuation trailing each row by a different amount.
    const filler = badge ? " ".repeat(Math.max(1, room - gutter - cols(body) - cols(badge))) : "";
    const text = `  ${lead}${body}${filler}${badge}`;

    const marks: DrawnMark[] = [];
    // The band goes on first and everything else is patched over it, so a highlighted row keeps
    // saying what its parts are instead of becoming one flat colour. See the `line_hl_group` rule.
    if (here) marks.push({ col: 0, opts: { lineHlGroup: "Question.Cursor" } });
    const leadAt = LEFT;
    marks.push(mark(leadAt, leadAt + byteLength(lead), taken ? "Question.Taken" : "Question.Box", 100));
    const labelAt = leadAt + byteLength(lead);
    marks.push(mark(
      labelAt,
      labelAt + byteLength(label.trimEnd()),
      custom ? "Question.Muted" : "Question.Option",
      100,
    ));
    if (shown) {
      const detailAt = labelAt + byteLength(label) + 2;
      marks.push(mark(detailAt, detailAt + byteLength(shown), "Question.Detail", 100));
    }
    if (badge) {
      marks.push(mark(byteLength(text) - byteLength(badge), byteLength(text), "Question.Shortcut", 100));
    }
    rows.push({ text, marks });
  });

  // --- what you typed instead ------------------------------------------------
  if (custom) {
    rows.push({ text: "" });
    const shown = clip(typed.trim(), Math.max(8, room - cols(g.pen) - 1));
    const text = `  ${g.pen} ${shown}`;
    caret = rows.length;
    caretCol = byteLength(text);
    rows.push({
      text,
      marks: [
        { col: 0, opts: { lineHlGroup: "Question.Cursor" } },
        mark(LEFT, LEFT + byteLength(g.pen), "Question.Taken", 100),
        mark(LEFT + byteLength(`${g.pen} `), byteLength(text), "Question.Custom", 100),
      ],
    });
  }

  // --- the keys --------------------------------------------------------------
  rows.push({ text: "" });
  const hint = `  ${clip(hints(question, ask, custom, room), room)}`;
  rows.push({ text: hint, marks: [mark(LEFT, byteLength(hint), "Question.Hint")] });
  return { rows, caret, caretCol };
}

/** `●●○` — how many questions there are and which one this is, when there is more than one. */
function progress(ask: Ask, g: Glyphs): string {
  if (ask.questions.length < 2) return "";
  const dots = ask.questions
    .map((q, i) => (i === ask.at ? g.dotHere : resolve(ask, q).length > 0 ? g.dotDone : g.dotTodo))
    .join("");
  return `${dots}  ${ask.at + 1}/${ask.questions.length}`;
}

/**
 * The key strip, which changes with what the question is and what you have done to it.
 *
 * Fitted rather than clipped. A strip that runs off the edge loses whichever key happens to be last
 * — `esc dismiss`, as it turned out, on the one panel you most need to be told how to leave — so
 * the parts are ranked and the least useful ones drop out until the rest fits. What is left is
 * always a whole phrase.
 */
function hints(question: UserQuestion, ask: Ask, custom: boolean, room: number): string {
  /** `[what it says, how readily it goes]`, lower going last. */
  const parts: Array<[string, number]> = custom
    ? [["\u21b5 send this answer", 0], ["\u232b back to the options", 2], ["esc dismiss", 1]]
    : [
      [question.multi_select ? "\u21e5 tick" : "\u21b5 choose", 0],
      ...(question.multi_select ? [["\u21b5 done", 0] as [string, number]] : []),
      ["\u2191\u2193 move", 3],
      ...(question.options.length > 1
        ? [[`1-${Math.min(SHORTCUTS, question.options.length)} pick`, 4] as [string, number]]
        : []),
      ["type your own", 2],
      ...(ask.at > 0 ? [["\u21e7\u21e5 back", 5] as [string, number]] : []),
      ["esc dismiss", 1],
    ];

  // Widest set first, dropping a whole rank at a time. Rank 0 is what the panel cannot be used
  // without, so it is what is left when nothing else fits — clipped, if it comes to that, but never
  // silently missing the key that closes the thing.
  const ranks = [...new Set(parts.map(([, rank]) => rank))].sort((a, b) => b - a);
  for (const cut of ranks) {
    const line = parts.filter(([, rank]) => rank <= cut).map(([text]) => text).join("   ");
    if (cols(line) <= room || cut === 0) return line;
  }
  return parts.map(([text]) => text).join("   ");
}

/** `text`, or as much of it as fits with an ellipsis. By code point, never by byte. */
function clip(text: string, room: number): string {
  const chars = [...text];
  if (chars.length <= room) return text;
  return `${chars.slice(0, Math.max(1, room - 1)).join("")}…`;
}

/**
 * Break a sentence onto lines of at most `width`.
 *
 * By code point, which is an approximation of display width and is the only one available here —
 * the frontend is the thing that can measure. Erring narrow is the safe direction: a question that
 * wraps a column early still reads, and one clipped at the right edge loses its last word.
 */
function wrap(text: string, width: number): string[] {
  const out: string[] = [];
  for (const paragraph of text.split("\n")) {
    let line = "";
    for (const word of paragraph.split(/\s+/).filter(Boolean)) {
      if (line && [...line].length + 1 + [...word].length > width) {
        out.push(line);
        line = word;
      } else {
        line = line ? `${line} ${word}` : word;
      }
    }
    out.push(line);
  }
  return out.length > 0 ? out : [""];
}

/**
 * The panel's colours, as links.
 *
 * Every one of them points at a group the palette already defines, so the panel follows the theme
 * without owning a single colour — and every one is a name an `init.ts` can redefine, which is what
 * makes the look of this replaceable without replacing the plugin.
 */
async function defineGroups(neosh: Neosh): Promise<void> {
  const groups: Array<[string, string]> = [
    // Shimmers: the agent has stopped, and this is what is stopping it.
    ["Question.Header", "Agent.ToolLive"],
    ["Question.Rail", "Question.Header"],
    ["Question.Border", "Float.Border"],
    ["Question.Text", "Agent.Assistant"],
    ["Question.Option", "Agent.Assistant"],
    ["Question.Detail", "Picker.Detail"],
    ["Question.Cursor", "Picker.Selected"],
    ["Question.Box", "Comment"],
    ["Question.Taken", "Diagnostic.Ok"],
    ["Question.Shortcut", "Composer.HintKey"],
    ["Question.Custom", "Status.Input"],
    ["Question.Muted", "Comment"],
    ["Question.Hint", "Composer.Hint"],
    ["Question.Waiting", "Status.Pending"],
  ];
  for (const [name, to] of groups) {
    await neosh.hl.define(name, { link: to }).catch(() => {});
  }
}

/** Re-exported so the type-checker keeps the hook signature honest. */
export type { Neosh };
