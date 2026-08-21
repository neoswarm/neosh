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
 * What happens on accept differs, and that is the whole of the difference:
 *
 * * a neosh command runs, and the draft is cleared, because it was never a message;
 * * a driver command is *written into the composer*, because it is a message — one the driver will
 *   recognise when it arrives — and because it may still want an argument.
 */

import type { Neosh, PluginContext } from "@neosh/api";
import { picker } from "@neosh/api/ui";

type Entry =
  | { kind: "command"; name: string }
  | { kind: "driver"; name: string; takesArgument: boolean };

/** Commands that are plumbing rather than something to invoke. */
const HIDDEN = /\.key$|^slash\./;

export async function activate({ neosh, subscriptions }: PluginContext) {
  subscriptions.push(
    await neosh.cmd.register("slash.open", () => open(neosh), {
      desc: "Run a command by name, neosh's or the agent's",
    }),
  );

  // Opened by the draft rather than by a key. `/` has to stay a character you can type — a
  // binding on it would mean no message could ever begin with one — so the menu follows what is
  // in the composer: a lone slash, and nothing else yet.
  subscriptions.push(
    neosh.agent.onComposerChange(({ text }) => {
      if (text === "/") void open(neosh);
    }),
  );
}

async function open(neosh: Neosh): Promise<void> {
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
      label: `/${d.name}`,
      detail: [d.description, d.argument_hint].filter(Boolean).join("   ") || "the agent's",
      keywords: "agent driver",
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
        keywords: `${c.desc ?? ""} ${c.plugin}`,
        value: { kind: "command", name: c.name } as Entry,
      })),
  ];

  const chosen = await picker(neosh, items, {
    title: "Run",
    placeholder: "nothing matches",
    width: 78,
    height: 14,
  });

  // Escaped. The slash that opened this is still in the composer and stays there: it may have been
  // the first character of an ordinary sentence.
  if (!chosen) return;

  try {
    if (chosen.kind === "command") {
      await neosh.agent.setDraft("");
      await neosh.cmd.exec(chosen.name);
      return;
    }
    // Left in the composer rather than sent. One that takes an argument is not finished being
    // typed, and even one that does not deserves the half-second in which you can see what you are
    // about to run.
    await neosh.agent.setDraft(`/${chosen.name}${chosen.takesArgument ? " " : ""}`);
  } catch (e) {
    neosh.notify(String(e), "warn");
  }
}
