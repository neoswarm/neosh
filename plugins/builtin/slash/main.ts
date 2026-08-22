/**
 * The slash menu.
 *
 * Two vocabularies meet in one composer and nobody should have to know which is which. `/compact`
 * belongs to the driver — `claude` implements it, neosh has never heard of it — and `/model`
 * belongs to neosh. They are typed the same way because from where you are sitting they are the
 * same thing: a short name for something you want to happen.
 *
 * So this asks both. neosh's commands come from the registry, which is why one registered by a
 * plugin loaded five minutes ago is in the list. The driver's come from the driver, at its
 * handshake, which is the only place they could come from — which commands a `claude` install
 * accepts depends on its project directory, its plugins and its MCP servers, and a list written
 * down here would be wrong on the first machine that had one of its own.
 *
 * # It is a completion, not a dialogue
 *
 * The first version of this was an ordinary picker: a modal list in the middle of the screen, its
 * own filter box, fuzzy-matched over every command's *description*. Three things were wrong with
 * that, and they compounded.
 *
 * The composer kept the `/` that opened it and nothing else, so what you typed went somewhere you
 * could not see. The filter matched descriptions, so `compact` — six letters that occur in that
 * order in a great many English sentences — matched a dozen unrelated commands and put one of them
 * under the cursor. And `↵` *ran* whatever was under the cursor. Typing `/compact` and pressing
 * enter therefore ran something else entirely, silently, while `/compact` itself was not in the
 * list at all: a driver only reports its commands after the conversation has run a turn, and a
 * fresh conversation has not.
 *
 * What replaces it holds to one rule: **the composer is the field, and this is a suggestion about
 * what is in it.** What you type goes into the composer and the menu follows. Matching is on the
 * *name*, anchored at the start, never on descriptions. Backspacing past the `/` dismisses it. And
 * when nothing matches, `↵` sends what you typed, verbatim — because a name this list has never
 * heard of is not a mistake, it is a command the driver knows about and neosh has not been told
 * about yet. A conversation that has not run a turn knows *none* of the driver's commands, so on a
 * fresh conversation that is the only way `/compact` can work at all.
 *
 * What happens on accept differs by which vocabulary the row came from, and that is the whole of
 * the difference:
 *
 * * a neosh command runs, and the draft is cleared, because it was never a message;
 * * a driver command with no argument is *sent*, because it is a message and it is finished;
 * * one that takes an argument is written into the composer, because it is not.
 */

import type { Neosh, PluginContext } from "@neosh/api";
import { picker } from "@neosh/api/ui";

type Entry =
  | { kind: "command"; name: string }
  | { kind: "driver"; name: string; takesArgument: boolean }
  | { kind: "verbatim"; text: string };

/** Commands that are plumbing rather than something to invoke. */
const HIDDEN = /\.key$|^slash\./;

/**
 * What the composer has to look like for the menu to be about it: a slash, then a command name,
 * and nothing else. A space ends it — by then you are typing an argument, and the menu has no
 * opinion about arguments — and so does a second line.
 */
const TOKEN = /^\/([\w.:-]*)$/;

export async function activate({ neosh, subscriptions }: PluginContext) {
  let draft = "";
  let open = false;

  // Opened deliberately — from `^K`, from a binding, from another plugin — starting from whatever
  // command name is already half-typed. Marked open for the same reason the draft path is: the menu
  // writes the composer back, and a second one opening on its own echo would take the keyboard from
  // the first.
  // The query is asked for rather than passed: this takes several round-trips to open, and
  // everything typed in that window lands in the composer. Read at the start, the menu comes up
  // unfiltered with a row nobody aimed at under `↵` — which is how typing `/compact` at speed ran
  // the first command in the list instead. Read at the end, what you typed is the filter.
  const start = async () => {
    if (open) return;
    open = true;
    await show(neosh, () => TOKEN.exec(draft)?.[1] ?? "", () => (open = false));
  };

  subscriptions.push(
    await neosh.cmd.register("slash.open", () => start(), {
      desc: "Run a command by name, neosh's or the agent's",
    }),
  );

  // Opened by the draft rather than by a key. `/` has to stay a character you can type — a binding
  // on it would mean no message could ever begin with one — so the menu follows what is in the
  // composer.
  //
  // Only on the *bare* slash, and not again while it is up. Once it is open it is the menu that
  // owns the keyboard and the menu that writes the composer back, so every keystroke from then on
  // arrives here as an echo of something this plugin just did. Re-opening on those is a loop.
  subscriptions.push(
    neosh.agent.onComposerChange(({ text }) => {
      draft = text;
      if (text === "/") void start();
    }),
  );
}

async function show(neosh: Neosh, query: () => string, done: () => void): Promise<void> {
  const [commands, keymaps, driver] = await Promise.all([
    neosh.cmd.list(),
    neosh.keymap.list("chat"),
    neosh.agent.driverCommands().catch(() => []),
  ]);

  const keyFor = new Map<string, string>();
  for (const k of keymaps) {
    if (!keyFor.has(k.command)) keyFor.set(k.command, k.lhs);
  }

  const items = [
    // The agent's first. They are the ones you cannot find any other way — every neosh command is
    // also in `^K`, and none of the agent's is.
    ...driver.map((d) => ({
      label: d.name,
      detail: [d.description, d.argument_hint].filter(Boolean).join("   ") || "the agent's",
      value: {
        kind: "driver",
        name: d.name,
        takesArgument: Boolean(d.argument_hint),
      } as Entry,
    })),
    ...commands
      .filter((c) => !HIDDEN.test(c.name))
      .map((c) => ({
        label: c.name,
        detail: [keyFor.get(c.name), c.desc].filter(Boolean).join("   "),
        value: { kind: "command", name: c.name } as Entry,
      })),
  ];

  const chosen = await picker(neosh, items, {
    title: "Run",
    // Anchored to the field it is completing and lifted clear of it, the way every completion
    // menu in every editor is placed. In the middle of the screen it covered the answer you were
    // reading and pointed at nothing.
    anchor: { kind: "dock", dock: "bottom" },
    match: "name",
    query,
    // The composer is the field. Everything typed here goes back into it, so the message says what
    // you typed and escaping the menu leaves it exactly there — mid-word, ready to keep going.
    onQuery: (q) => void neosh.agent.setDraft(`/${q}`),
    // What `↵` does when nothing is highlighted, which here means nothing matched. Without it the
    // accept key would do nothing at all on exactly the input that most needs it to work.
    freeform: (q) => (q ? ({ kind: "verbatim", text: `/${q}` } as Entry) : null),
    placeholder: "not one of ours \u2014 \u21b5 sends it to the agent as typed",
    width: 78,
    // Short. It is a hint about a word you are half-way through typing, not a catalogue — `^K` is
    // the catalogue — and a menu that eats a third of the transcript to say so is one you close
    // before reading.
    height: 8,
    hints: "↵ run   ^P/^N move   type to filter   esc keep typing",
  });
  done();

  // Escaped, or filtered down to nothing. Either way what was typed is in the composer, where it
  // was being typed, and it stays there: it may have been the first word of an ordinary sentence.
  if (!chosen) return;

  try {
    switch (chosen.kind) {
      case "command":
        await neosh.agent.setDraft("");
        await neosh.cmd.exec(chosen.name);
        return;
      // Finished being typed, so it goes. Leaving a complete command sitting in the composer for a
      // second confirmation is a keystroke that asks nothing: you chose it from a list. One that
      // takes an argument is *not* finished, and stays where you can add it.
      case "driver":
        if (chosen.takesArgument) {
          await neosh.agent.setDraft(`/${chosen.name} `);
          return;
        }
        await neosh.agent.setDraft("");
        await neosh.agent.send(`/${chosen.name}`);
        return;
      // Not in the list, which is not the same as wrong: the driver is the one that gets to say
      // whether it knows this name, and it will say so in its own words.
      case "verbatim":
        await neosh.agent.setDraft("");
        await neosh.agent.send(chosen.text);
        return;
    }
  } catch (e) {
    neosh.notify(String(e), "warn");
  }
}
