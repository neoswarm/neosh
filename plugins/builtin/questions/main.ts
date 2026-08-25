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
 * told the agent you had ignored it.
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
 * panel says a digit is a character now.
 *
 * **Every key is a named command.** `question.accept`, `question.next` — so `^Z` lists them and an
 * `init.ts` can move them, and a third party can bind their own against the `neosh.question` buffer
 * kind without forking this. Only the printable keys come through the raw capture, and only because
 * they are text rather than verbs.
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
  ViewId,
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

/**
 * Which conversations have stopped and are waiting on an answer, in a workspace var.
 *
 * Written here because this is the only thing that knows, and shared because it is not about this
 * plugin: "somebody is waiting on you in *that* conversation" is a fact about the conversation, and
 * every list of conversations wants it. The footer can only ever say *how many* — it is one line and
 * it is shared — so without this the answer to "which one" is opening conversations until you find
 * it, which is the same hunt `SessionInfo::unread` exists to end.
 *
 * A conversation that is asking looks exactly like one that is thinking: the turn is still in
 * flight, blocked on the hook below, so `active_turn` is set and every panel draws a spinner. That
 * is the bug this fixes, and it is why anything reading this must let it outrank *working*.
 *
 * The value is the session ids, newest last. It is on disk with the rest of `vars.json` and the
 * queue is in memory, so a workspace that stopped with a question on it would come back claiming
 * one is still open — [`announce`] therefore writes once at activation, when the queue is empty,
 * and that is what clears it.
 */
const VAR_ASKING = "question.asking";

/** How much of the row the label column may take before the description gives up its place. */
const LABEL_MAX = 28;

/**
 * The label on the row that takes an answer of your own.
 *
 * One word, and a conventional one, because it shares the label column with the options: anything
 * longer widens that column on every question, including the `Yes`/`No` ones where the whole row is
 * four characters and the description is what you are reading.
 */
const OTHER = "Other";

/**
 * How many lines of the answer you are writing the panel will show at once.
 *
 * A field, not a paragraph: what is being typed goes on as many lines as it takes and the panel
 * follows the *end* of it, because the end is where the caret is and the caret is what you are
 * looking at. Unbounded, a long answer would grow the float until it had eaten the conversation it
 * is a question about; clipped to one line — which is what this was — everything past the width of
 * the row is an ellipsis, and typing into a field that stopped showing what you were typing is the
 * one thing a field must never do.
 */
const FIELD_LINES = 6;

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
  /**
   * What is in the field, and whether the field is open at all.
   *
   * On the ask rather than in the panel, because the panel is closed and built again from nothing
   * every time you look at another conversation — and a sentence somebody is half-way through
   * writing belongs to the question, not to the float that happened to be drawing it. Separate
   * from `drafts`, which is what has been *answered*: a half-written sentence is not an answer,
   * and one filed as though it were would have the panel tick the question off and walk past it.
   */
  typing: string;
  writing: boolean;
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
   * Where each terminal is.
   *
   * "The conversation being looked at" used to be one session id, because there was one screen. A
   * workspace can have several and each is somewhere, so a question is put in front of a terminal
   * that is reading *its* conversation — and if none is, it waits, and the footer says how many
   * are waiting.
   *
   * Empty until the first refresh, which is deliberate: every `await` before the hook is
   * registered is a window in which a question arrives and finds no blocking hook — answered as
   * "nobody answered", instantly and wrongly — so the hook goes on first and this is filled in
   * behind it.
   */
  let where = new Map<ViewId, SessionId>();
  const relocate = async () => {
    const views = await neosh.view.list().catch(() => []);
    where = new Map(views.map((v) => [v.view, v.session]));
  };

  /**
   * The first queued question there is a terminal to show it in, and which terminal that is.
   *
   * A question raised with no conversation of its own — a plugin's `neosh.ask` — belongs wherever
   * you are, so it takes the terminal being served if there is one and any terminal otherwise.
   */
  const showable = (): { ask: Ask; view: ViewId } | null => {
    for (const ask of queue) {
      if (ask.session === null) {
        const anywhere = [...where.keys()][0];
        if (anywhere !== undefined) return { ask, view: anywhere };
        continue;
      }
      for (const [view, session] of where) {
        if (session === ask.session) return { ask, view };
      }
    }
    return null;
  };

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
    // Closed when the question changes *or* when the terminal reading it does — somebody switched
    // away, and a panel left on their screen is a question they are no longer being asked.
    if (open && (open.ask !== want?.ask || open.view !== want.view)) {
      await open.close();
      open = null;
    }
    if (want && !open) {
      // Whatever was half-typed becomes the seed of the answer. A question that landed over a
      // sentence somebody was writing should take that sentence, not throw it away — it is very
      // often exactly what they were about to say.
      const seed = draft;
      draft = "";
      // In the terminal reading the conversation that asked, which is what makes a question
      // findable in a workspace with three windows open on three different things.
      open = await draw(neosh.view.at(want.view), want.view, want.ask, seed, () => {
        // Answered, dismissed, or gone. Either way it leaves the queue and the next one — which
        // may belong to another conversation entirely — gets its turn at the screen.
        const i = queue.indexOf(want.ask);
        if (i >= 0) queue.splice(i, 1);
        open = null;
        void sync();
      });
    }
    await elsewhere(neosh, queue, open?.ask ?? null);
    await announce();
  };

  /**
   * Say which conversations are waiting, for anything that draws a list of them.
   *
   * `null` to start rather than an empty string, so the first call writes whatever the queue is —
   * including the empty one, which is how a var left behind by a workspace that stopped mid-question
   * is cleared. Diffed rather than written every time because a var is a file: `sync` runs on every
   * conversation switch and most of them change nothing here.
   */
  let announced: string | null = null;
  const announce = async () => {
    // A question raised by something with no conversation of its own belongs to no row, so it is
    // the footer's business and not this one's.
    const ids = [...new Set(queue.map((a) => a.session).filter((s): s is SessionId => Boolean(s)))];
    const next = ids.join("\n");
    if (announced === next) return;
    announced = next;
    await (ids.length === 0
      ? neosh.vars.remove({ scope: "global" }, VAR_ASKING)
      : neosh.vars.set({ scope: "global" }, VAR_ASKING, ids)).catch(() => {});
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
          typing: "",
          writing: false,
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
  await relocate();
  void sync();

  subscriptions.push(
    neosh.session.onChange(({ session, view }) => {
      where.set(view, session);
      void sync();
    }),
  );
  // A terminal arriving is a screen a waiting question may now have. One leaving is a panel that
  // has gone with it, and a question that has to go back in the queue.
  subscriptions.push(
    neosh.view.onOpen(() => void relocate().then(sync)),
  );
  subscriptions.push(
    neosh.view.onClose((view) => {
      where.delete(view);
      if (open?.view === view) open = null;
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
      // And take the mark off the rows. Unloading this plugin unblocks every turn it was holding,
      // so a conversation still flagged as asking would be one nothing is going to ask again.
      void neosh.vars.remove({ scope: "global" }, VAR_ASKING).catch(() => {});
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
  /** Which terminal it is on. A question is answered once, on one screen. */
  view: ViewId;
  close(): Promise<void>;
}

/**
 * Put one ask on screen and drive it until it is answered or dismissed.
 *
 * Returns as soon as it is drawn — the answering happens on keys, and `done` is called when the ask
 * has settled so the caller can move on to the next one.
 */
async function draw(
  neosh: Neosh,
  view: ViewId,
  ask: Ask,
  seed: string,
  done: () => void,
): Promise<Open> {
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
  /**
   * Whether the panel is being written into rather than chosen from.
   *
   * Explicit rather than `typed !== ""`, and the difference is the row at the foot of the list. You
   * get here by landing on that row and pressing `↵`, which has to show you an empty field to type
   * into — derived from the text, the field would not appear until after the first character, so
   * the key would look like it did nothing and the thing it opened would look like a typo.
   */
  let writing = false;
  let closed = false;

  const q = () => ask.questions[ask.at]!;
  const draftOf = (question: UserQuestion): Draft =>
    ask.drafts.get(question.question) ?? { picked: [], typed: "" };

  const render = async () => {
    if (closed) return;
    const rows = compose(ask, typed, writing, width, glyphs);
    await neosh.buf.render(buf, ns, 0, -1, rows.rows).catch(() => {});
    // The caret goes where you are acting: on the row under the cursor, and at the end of what you
    // have typed once you are typing. Without it the terminal's caret parks in the corner of a
    // panel that is plainly listening, and a field with no insertion point in it reads as inert.
    await neosh.win.setCursor(win, rows.caret, rows.caretCol).catch(() => {});
  };

  const close = async () => {
    if (closed) return;
    closed = true;
    // Onto the ask, before the panel goes. Looking at another conversation closes this panel and
    // builds a new one on the way back — the cursor and which question you were on already survive
    // that, and what you were writing is the one part of it you would notice losing.
    ask.typing = typed;
    ask.writing = writing;
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
    writing = false;

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

  /**
   * Move the cursor, over the options *and* the row that lets you write your own.
   *
   * `+ 1` because that row is real: it takes the cursor, it wraps round to the first option, and it
   * is reachable with the same key as everything else. A row you can see and cannot arrive at is
   * worse than no row.
   */
  const move = async (delta: number) => {
    const n = q().options.length + 1;
    // Moving off the field is how you go back to the options without erasing what you wrote — it
    // is still there when you come back to the row.
    if (writing) writing = false;
    ask.cursor = (ask.cursor + delta + n) % n;
    await render();
  };

  /** Start writing an answer of your own, with the cursor parked on the row that is the field. */
  const write = async () => {
    ask.cursor = q().options.length;
    writing = true;
    await render();
  };

  /**
   * Stop writing, and go back to choosing.
   *
   * What was typed stays typed. Backspacing past the first character is how you change your mind
   * about writing at all, and one that threw the sentence away would make `⌫` a key you have to be
   * careful with — on a panel whose entire job is to be answered quickly.
   */
  const unwrite = async () => {
    writing = false;
    await render();
  };

  /**
   * Take an option, by index. Single-select answers with it; multi-select toggles it.
   *
   * The index one past the last option is the row that lets you write your own, which is a row and
   * so answers to every key the rows answer to.
   */
  const take = async (index: number) => {
    const question = q();
    if (index === question.options.length) {
      await write();
      return;
    }
    const option = question.options[index];
    if (!option) return;
    writing = false;
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
    if (writing) {
      if (typed.trim()) {
        await advance();
        return;
      }
      // An empty field and `↵`. Saying so beats sending nothing: an answer of "" is one the agent
      // reads as an answer, and a key that silently does nothing is one you press again harder.
      neosh.notify("type an answer, or ⌫ back to the options", "info");
      return;
    }
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
    // Back onto a question you answered in your own words puts you back in the field with those
    // words in it, rather than on a list of options you had already decided against.
    writing = was !== "";
    if (writing) ask.cursor = q().options.length;
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
        // Back off the end of an empty field and you are choosing again. The one key that leaves
        // the field, and the one people already reach for.
        if (!typed) await unwrite();
        else await retype(typed.slice(0, -1));
        return;
      }
      if (code.kind !== "char" || key.key.mods.alt) return;
      if (key.key.mods.ctrl) {
        if (code.c === "u") await retype("");
        else if (code.c === "w") await retype(dropWord(typed));
        return;
      }
      const c = code.c;
      // Digits are shortcuts until something is being written, and characters afterwards — which
      // the panel says out loud by taking the numbers off the rows. `0` is the row at the foot:
      // on a numbered list it already reads as *none of them*, which is what that row is.
      if (!writing && c >= "0" && c <= "9") {
        await take(c === "0" ? q().options.length : Number(c) - 1);
        return;
      }
      // The first character of an answer is also the thing that starts one. Typing has always been
      // how you say something the options do not, and it stays that way — the row at the foot is
      // how you *find out* that it is, not a gate in front of it.
      if (!writing) {
        ask.cursor = q().options.length;
        writing = true;
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

  // What was in the field when this question was last on screen, if it has been: coming back to a
  // conversation you left mid-sentence puts you back in the sentence. Otherwise, whatever was
  // already half-typed when the question arrived, rather than thrown away — the panel appeared
  // over a field somebody was using, and what they had written is very often the answer.
  typed = ask.typing || seed;
  writing = ask.writing || typed.trim() !== "";
  if (writing) ask.cursor = q().options.length;
  // And the composer says the same thing, because switching conversations emptied it. The panel
  // mirrors the answer into the composer so you can see it in the field this screen actually has;
  // restoring one without the other is arriving at two answers that disagree.
  if (typed !== "") await neosh.agent.setDraft(typed).catch(() => {});
  await render();

  return { ask, view, close };
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
  writing: boolean,
  width: number,
  g: Glyphs,
): { rows: DrawnRow[]; caret: number; caretCol: number } {
  const question = ask.questions[ask.at]!;
  const custom = writing;
  /** The row after the last option: the answer nobody listed. */
  const customAt = question.options.length;
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
  // The row at the foot is one of the rows, so its label is one of the labels the column has to
  // fit. Left out, a question whose options are `Yes` and `No` gives the column three columns and
  // `Other` arrives clipped — a row offering to take your answer, unable to say so.
  const labelWidth = Math.min(
    LABEL_MAX,
    question.options.reduce((w, o) => Math.max(w, cols(o.label)), cols(OTHER)),
  );
  const shortcuts = !custom && question.options.length > 1;
  const badgeWidth = shortcuts ? cols(String(Math.min(SHORTCUTS, question.options.length))) : 0;

  let caret = rows.length;
  let caretCol = 0;
  question.options.forEach((option, i) => {
    const here = !custom && i === ask.cursor;
    if (here) caret = rows.length;
    const taken = !custom && picked.has(option.label);
    const lead = pad(`${here ? g.cursor : g.blank} ${taken ? on : off} `, gutter);
    const badge = shortcuts && i < SHORTCUTS ? String(i + 1) : "";
    // What is left for the description, once the row has paid for its gutter, its label column and
    // the shortcut hanging off the right edge.
    const forDetail = room - gutter - labelWidth - 2 - (badgeWidth ? badgeWidth + 2 : 0);
    const detail = option.description.trim();

    // The row under the cursor says all of itself, on as many lines as that takes, and folds back
    // to one the moment the cursor leaves.
    //
    // This is the whole point of the panel. An option's description is the *only* thing that says
    // what taking it would mean — "the primary write database" against "the read replica" — and it
    // is the first thing a two-column row inside a bordered float runs out of room for. Clipped, an
    // agent asking which of four things you want is a list of four sentences that all stop at the
    // same word, and the answer is a guess. It goes under the row rather than in a float over it
    // because there is nowhere else to put it: this panel is already a float over the composer, and
    // a second one on top would cover the options it is describing.
    //
    // Only the cursor's row, and only ever one of them, so the list is still a list you can read
    // down. The columns hold their places on the continuation lines — a description that reflowed
    // under the label would read as a new option.
    const labelLines = here ? wrapCols(option.label, labelWidth) : [clip(option.label, labelWidth)];
    const detailLines = detail === "" || forDetail < 8
      ? []
      : here
        ? wrapCols(detail, forDetail)
        : [clip(detail, forDetail)];

    const lines = Math.max(labelLines.length, detailLines.length, 1);
    for (let n = 0; n < lines; n++) {
      const label = pad(labelLines[n] ?? "", labelWidth);
      const shown = detailLines[n] ?? "";
      const body = shown ? `${label}  ${shown}` : label.trimEnd();
      // The gutter is paid for on every line, and drawn on the first: the marker and the box are
      // what the row *is*, and repeating them down the side would read as one option per line.
      const run = n === 0 ? lead : " ".repeat(gutter);
      const tail = n === 0 ? badge : "";
      // Right-aligned against the panel's own edge, so the shortcuts read as a column rather than
      // as punctuation trailing each row by a different amount.
      const filler = tail ? " ".repeat(Math.max(1, room - gutter - cols(body) - cols(tail))) : "";
      const text = `  ${run}${body}${filler}${tail}`;

      const marks: DrawnMark[] = [];
      // The band goes on first and everything else is patched over it, so a highlighted row keeps
      // saying what its parts are instead of becoming one flat colour. See the `line_hl_group`
      // rule. Every line of an unfolded row wears it, or the rest of the description reads as
      // something that fell out from under the row rather than as part of it.
      if (here) marks.push({ col: 0, opts: { lineHlGroup: "Question.Cursor" } });
      const leadAt = LEFT;
      if (n === 0) {
        marks.push(mark(leadAt, leadAt + byteLength(lead), taken ? "Question.Taken" : "Question.Box", 100));
      }
      const labelAt = leadAt + byteLength(run);
      if (label.trimEnd() !== "") {
        marks.push(mark(
          labelAt,
          labelAt + byteLength(label.trimEnd()),
          custom ? "Question.Muted" : "Question.Option",
          100,
        ));
      }
      if (shown) {
        const detailAt = labelAt + byteLength(label) + 2;
        marks.push(mark(detailAt, detailAt + byteLength(shown), "Question.Detail", 100));
      }
      if (tail) {
        marks.push(mark(byteLength(text) - byteLength(tail), byteLength(text), "Question.Shortcut", 100));
      }
      rows.push({ text, marks });
    }
  });

  // --- the answer nobody listed ----------------------------------------------
  //
  // A row, not a note in the key strip. Typing has always been how you answer a question none of
  // the options answer, and a capability whose entire advertisement is one phrase in a dim strip at
  // the bottom is a capability nobody has: there is nothing on screen that looks like a field, so
  // the panel reads as four options and a dead end. It is the last row because that is where "none
  // of these" belongs, it takes the cursor like any other row, and `↵` on it starts the answer.
  //
  // `0` is its shortcut, which is the digit the option rows deliberately do not use: on a numbered
  // list `0` already reads as *none of them*, and that is exactly what this row is.
  {
    const here = custom || ask.cursor === customAt;
    const lead = pad(`${here ? g.cursor : g.blank} ${g.pen} `, gutter);
    const badge = shortcuts ? "0" : "";
    const forDetail = room - gutter - labelWidth - 2 - (badgeWidth ? badgeWidth + 2 : 0);
    if (custom) {
      // Once you are writing, the row *is* the field: what is in it is your answer, it takes the
      // whole width because a sentence is not a label, and it takes as many lines as the sentence
      // takes. Clipped to the one row — which is what it did — an answer longer than the panel is
      // wide became the first sixty characters and an ellipsis, and stayed that way however much
      // more you typed: the caret stopped moving, the words went nowhere visible, and the only way
      // to read back what you had written was to send it.
      //
      // The *end* is what is kept when there is more of it than [`FIELD_LINES`] can hold, because
      // the end is where the caret is. A field that scrolled the other way would be one that shows
      // you everything except the word you are typing.
      const lines = wrapField(typed, Math.max(8, room - gutter - 2));
      const shown = lines.slice(Math.max(0, lines.length - FIELD_LINES));
      shown.forEach((line, n) => {
        // The gutter is paid for on every line and drawn on the first, as the option rows do it:
        // repeating the pen down the side would read as one answer per line.
        const run = n === 0 ? lead : " ".repeat(gutter);
        const text = `  ${run}${line}`;
        const marks: DrawnMark[] = [{ col: 0, opts: { lineHlGroup: "Question.Cursor" } }];
        if (n === 0) marks.push(mark(LEFT, LEFT + byteLength(lead), "Question.Taken", 100));
        const bodyAt = LEFT + byteLength(run);
        if (line !== "") marks.push(mark(bodyAt, byteLength(text), "Question.Custom", 100));
        if (n === shown.length - 1) {
          // The caret sits at the end of what has been typed, which is the one place on this panel
          // where a terminal cursor means "the keyboard goes here".
          caret = rows.length;
          caretCol = bodyAt + byteLength(line);
        }
        rows.push({ text, marks });
      });
    } else {
      const label = pad(clip(OTHER, labelWidth), labelWidth);
      const hint = forDetail >= 8 ? clip("type an answer of your own", forDetail) : "";
      const body = hint ? `${label}  ${hint}` : label.trimEnd();
      const filler = badge ? " ".repeat(Math.max(1, room - gutter - cols(body) - cols(badge))) : "";
      const text = `  ${lead}${body}${filler}${badge}`;

      const marks: DrawnMark[] = [mark(LEFT, LEFT + byteLength(lead), "Question.Box", 100)];
      if (here) marks.unshift({ col: 0, opts: { lineHlGroup: "Question.Cursor" } });
      const bodyAt = LEFT + byteLength(lead);
      marks.push(mark(bodyAt, bodyAt + byteLength(clip(OTHER, labelWidth)), "Question.Option", 100));
      const hintAt = bodyAt + byteLength(label) + 2;
      if (byteLength(text) > hintAt) {
        marks.push(mark(hintAt, byteLength(text), "Question.Detail", 100));
      }
      if (here) caret = rows.length;
      if (badge) {
        marks.push(
          mark(byteLength(text) - byteLength(badge), byteLength(text), "Question.Shortcut", 100),
        );
      }
      rows.push({ text, marks });
    }
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
  const onCustom = ask.cursor === question.options.length;
  const parts: Array<[string, number]> = custom
    ? [["\u21b5 send this answer", 0], ["\u232b back to the options", 1], ["esc dismiss", 1]]
    : onCustom
    // On the row at the foot, the only thing worth saying is what it does. The keys that move
    // between rows are the ones you just used to get here.
    ? [
      ["\u21b5 write your own answer", 0],
      ["\u2191\u2193 move", 3],
      ...(ask.at > 0 ? [["\u21e7\u21e5 back", 5] as [string, number]] : []),
      ["esc dismiss", 1],
    ]
    : [
      [question.multi_select ? "\u21e5 tick" : "\u21b5 choose", 0],
      ...(question.multi_select ? [["\u21b5 done", 0] as [string, number]] : []),
      ["\u2191\u2193 move", 3],
      ...(question.options.length > 1
        ? [[`1-${Math.min(SHORTCUTS, question.options.length)} pick`, 4] as [string, number]]
        : []),
      ["0 write your own", 2],
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
 * Break `text` onto lines of at most `width`, cutting a word that is wider than the column.
 *
 * The difference from [`wrap`] is where the result lands. That one fills the panel, which is as
 * wide as the conversation, so a long word simply runs to the next line. This one fills a *column*
 * beside another column — and a word wider than it does not overhang harmlessly, it pushes the
 * description sideways and takes the alignment of every line below it with it. Option labels are
 * routinely identifiers, paths or flags, which is to say routinely longer than 28 columns.
 */
function wrapCols(text: string, width: number): string[] {
  const limit = Math.max(1, width);
  const out: string[] = [];
  let line = "";
  for (const word of text.trim().split(/\s+/).filter(Boolean)) {
    const next = line ? `${line} ${word}` : word;
    if (cols(next) <= limit) {
      line = next;
      continue;
    }
    if (line) {
      out.push(line);
      line = "";
    }
    let rest = word;
    while (cols(rest) > limit) {
      const chars = [...rest];
      out.push(chars.slice(0, limit).join(""));
      rest = chars.slice(limit).join("");
    }
    line = rest;
  }
  if (line) out.push(line);
  return out.length > 0 ? out : [""];
}

/**
 * Break what is being *typed* onto lines of at most `width`.
 *
 * The third of these, and it exists because the other two rewrite what they wrap. [`wrap`] and
 * [`wrapCols`] split on whitespace and join with single spaces, which is right for a label or a
 * sentence that arrived finished and wrong for a field: the two spaces somebody typed would come
 * back as one, a trailing space would vanish the moment it was typed and reappear with the next
 * word, and the caret — which is placed at the end of the last line — would sit somewhere other
 * than where the next character is going to land.
 *
 * So every character survives, and the space a line breaks at stays on the end of the line it broke
 * from: the lines joined back together are exactly what was typed. Words are still kept whole where
 * one fits, and one that does not is cut, because a word wider than the field has to go somewhere.
 */
function wrapField(text: string, width: number): string[] {
  const limit = Math.max(1, width);
  const out: string[] = [];
  let rest = text;
  while (cols(rest) > limit) {
    const chars = [...rest];
    // The last space that would still leave something before it — a break at the very front would
    // push the whole line down and make no progress.
    let at = -1;
    for (let i = limit - 1; i > 0; i--) {
      if (chars[i] === " ") {
        at = i;
        break;
      }
    }
    const cut = at > 0 ? at + 1 : limit;
    out.push(chars.slice(0, cut).join(""));
    rest = chars.slice(cut).join("");
  }
  out.push(rest);
  return out;
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
