/**
 * The approval prompt.
 *
 * `permissions.mode = "ask"` is the default, and without something answering, "ask" means "refuse":
 * every gated tool call comes back as an error the model cannot act on and the user never sees a
 * question. This is the half that asks.
 *
 * It is a **blocking** hook, so the rule from ADR 0009 applies — a blocking hook that does not
 * answer in time is a veto. A permission prompt that failed open would be worse than no prompt at
 * all, so the timeout is generous enough for a human and the failure direction is "no".
 *
 * Remembering an answer is deliberately per-session and in memory. Writing "always allow" to disk
 * is how a policy quietly becomes permanent, and the file to edit for that is `config.toml`.
 */

import type { Capability, HookOutcome, Neosh, PluginContext } from "@neosh/api";
import { picker } from "@neosh/api/ui";

/** How long the user has. Past this the hook times out, which the host reads as a refusal. */
const ASK_TIMEOUT_MS = 120_000;

type Answer = "once" | "session" | "no";

export async function activate({ neosh, subscriptions }: PluginContext) {
  await neosh.opt.declare({
    name: "approvals.remember",
    type: { type: "bool" },
    default: true,
    description:
      'Offer "allow for this session". The answer is held in memory only — making it permanent is a line in config.toml, not a keystroke.',
  });

  // Capabilities allowed for the rest of this session, by their exact description. Keyed on the
  // rendered description rather than the object so `read_file` of two different paths are two
  // different decisions.
  const allowed = new Set<string>();
  subscriptions.push(neosh.session.onChange(() => allowed.clear()));

  subscriptions.push(
    await neosh.hook.register(
      "permission_pre",
      async (payload): Promise<HookOutcome> => {
        if (payload.hook !== "permission_pre") return { action: "continue" };
        const key = describe(payload.capability);

        if (allowed.has(key)) return { action: "continue" };

        const remember = (await neosh.opt.get<boolean>("approvals.remember")) ?? true;
        const options: Array<{ label: string; detail?: string; value: Answer }> = [
          { label: "Allow once", value: "once" },
          ...(remember
            ? [{ label: "Allow for this session", detail: "forgotten when you switch conversation", value: "session" as Answer }]
            : []),
          { label: "Deny", value: "no" },
        ];

        const answer = await picker(neosh, options, {
          title: key,
          width: Math.max(40, Math.min(88, key.length + 8)),
          height: options.length,
        });

        if (answer === "session") {
          allowed.add(key);
          return { action: "continue" };
        }
        if (answer === "once") return { action: "continue" };
        // Dismissing is a no. A prompt you can escape into an allow is not a prompt.
        return { action: "veto", reason: answer === null ? "not approved" : "denied" };
      },
      { blocking: true, timeoutMs: ASK_TIMEOUT_MS },
    ),
  );
}

/** What the user is actually being asked, in one line. */
function describe(c: Capability): string {
  switch (c.kind) {
    case "read_file": return `Read ${c.path}?`;
    case "write_file": return `Write ${c.path}?`;
    case "exec": return `Run ${c.command}?`;
    case "network": return `Connect to ${c.host}?`;
  }
}

/** Re-exported so the type-checker keeps the hook signature honest. */
export type { Neosh };
