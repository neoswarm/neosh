/**
 * What the conversation is spending.
 *
 * Two numbers, and they answer different questions. **Context** is how full the window was on the
 * most recent request, and it is the one that changes what you do next: at 90% the useful move is
 * to start a new conversation, and finding that out from a truncation error is finding out too
 * late. **Tokens** is the running total, which is about cost rather than about room.
 *
 * They are deliberately not added together. A conversation that has spent 400k tokens across twenty
 * turns is not 400k tokens *long*, and a footer that implied it was would be wrong in the direction
 * that makes people worry.
 */

import type { ModelEntry, Neosh, PluginContext, SessionInfo } from "@neosh/api";

export async function activate({ neosh, subscriptions }: PluginContext) {
  await neosh.opt.declare({
    name: "usage.show_tokens",
    type: { type: "bool" },
    default: true,
    description: "Show the running token total beside the context meter.",
  });

  await neosh.cmd.register("usage.report", () => report(neosh), {
    desc: "What this conversation has used",
  });

  const refresh = async () => {
    const session = await neosh.session.current().catch(() => null);
    if (!session) return;
    await drawContext(neosh, session);
    await drawTokens(neosh, session);
  };

  await refresh();
  // Turn end is when both numbers change, and the only time either can.
  subscriptions.push(neosh.agent.onTurnEnd(() => void refresh()));
  subscriptions.push(neosh.session.onChange(() => void refresh()));
  subscriptions.push(
    neosh.opt.onChange((e) => {
      if (e.name === "usage.show_tokens") void refresh();
    }),
  );
}

/** The window this model has, or nothing if the catalogue does not say. */
async function contextWindow(neosh: Neosh): Promise<number | undefined> {
  const selection = await neosh.agent.selection().catch(() => null);
  if (!selection) return undefined;
  const entries = await neosh.agent
    .listModels(selection.instance)
    .catch(() => [] as ModelEntry[]);
  const model = entries.find((e) => e.model.id === selection.model);
  return model?.model.context_window ?? undefined;
}

async function drawContext(neosh: Neosh, session: SessionInfo) {
  const used = session.context_tokens ?? 0;
  const window = await contextWindow(neosh);
  // Nothing to be a percentage of. Better to say the number than to invent a denominator.
  if (!window) {
    if (used > 0) {
      await neosh.status.set("context", { text: `${short(used)} ctx`, priority: 20 });
    } else {
      await neosh.status.clear("context");
    }
    return;
  }
  if (used === 0) {
    await neosh.status.clear("context");
    return;
  }
  const percent = Math.min(100, (used / window) * 100);
  await neosh.status.set("context", {
    text: `${percent.toFixed(0)}% of ${short(window)}`,
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
  const window = await contextWindow(neosh);
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
