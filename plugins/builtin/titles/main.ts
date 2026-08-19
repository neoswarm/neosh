/**
 * Conversation titles, written by a model.
 *
 * A thread list is only navigable if the rows say what they are. Falling back to the first message
 * gets you a long way — it is what the label does when there is no title — but the first message is
 * often "have a look at this" and the conversation turns out to be about something else.
 *
 * So: after the first exchange settles, ask for a title. Once, per conversation, never again unless
 * asked, and never while a turn is running. The prompt is a setting, like every other prompt here.
 */

import type { Neosh, PluginContext, SessionId } from "@neosh/api";

const DEFAULT_PROMPT = `Generate a title that will help someone recognise this conversation weeks later.
Return a JSON object with exactly one key: title.

Rules:
- 3 to 8 words, under 40 characters.
- A compact noun phrase, or a clear action phrase.
- Name the subject and what the user wants, not the process used to get there.
- Do not claim the work is finished.
- Do not copy and truncate the user's message.
- No quotes, no trailing punctuation, no project name that a sidebar already shows.`;

export async function activate({ neosh, subscriptions }: PluginContext) {
  await neosh.opt.declare({
    name: "session.autotitle",
    type: { type: "bool" },
    default: true,
    description:
      "Name a conversation after its first exchange. Uses `gen.model`, so point that at something cheap.",
  });
  await neosh.opt.declare({
    name: "session.title.prompt",
    type: { type: "str" },
    default: "",
    description:
      "Replaces the titling prompt entirely. Empty uses the built-in one. It must ask for JSON with a `title` key.",
  });
  await neosh.opt.declare({
    name: "session.title.instructions",
    type: { type: "str" },
    default: "",
    description: "Appended to the titling prompt. The usual way to add a house rule.",
  });

  // Conversations already asked about. A failed attempt counts: retrying every turn would turn one
  // bad answer into a request per message, which is the expensive kind of bug.
  const attempted = new Set<SessionId>();

  subscriptions.push(
    await neosh.cmd.register("session.retitle", () => retitle(neosh, attempted, true), {
      desc: "Rename this conversation from what it is about",
    }),
  );

  subscriptions.push(
    neosh.agent.onTurnEnd(() => {
      void retitle(neosh, attempted, false);
    }),
  );
}

async function retitle(neosh: Neosh, attempted: Set<SessionId>, forced: boolean): Promise<void> {
  if (!forced && !((await neosh.opt.get<boolean>("session.autotitle")) ?? true)) return;

  const current = await neosh.session.current().catch(() => null);
  if (!current) return;
  if (!forced) {
    // A title someone set by hand is theirs. And one exchange is the earliest point at which there
    // is anything to name.
    if (current.title || attempted.has(current.id)) return;
    if (current.message_count < 2) return;
  }
  attempted.add(current.id);

  const messages = await neosh.session.messages().catch(() => []);
  const transcript = messages
    .slice(0, 6)
    .flatMap((m) =>
      m.content.flatMap((b) => (b.type === "text" ? [`${m.role}: ${b.text}`] : [])),
    )
    .join("\n\n")
    .slice(0, 6000);
  if (transcript.trim() === "") return;

  const override = ((await neosh.opt.get<string>("session.title.prompt")) ?? "").trim();
  const extra = ((await neosh.opt.get<string>("session.title.instructions")) ?? "").trim();
  const base = override || DEFAULT_PROMPT;
  const prompt = `${extra ? `${base}\n\nAdditional instructions:\n${extra}` : base}\n\nConversation:\n${transcript}`;

  try {
    const answer = await neosh.gen.json<{ title?: string }>(prompt);
    const title = (answer.title ?? "").trim().replace(/^["']|["']$/g, "");
    if (title === "") return;
    // The conversation may have been switched away from while the model was thinking; renaming by
    // id rather than "the current one" is what stops the answer landing on the wrong thread.
    await neosh.session.rename(current.id, title.slice(0, 60));
  } catch (e) {
    // Not worth a message: failing to name a conversation costs the user nothing, and a popup
    // every time a cheap model hiccups would cost them plenty.
    neosh.log.info(`could not title the conversation: ${e}`);
  }
}
