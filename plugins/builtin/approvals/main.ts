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

import type { Capability, HookOutcome, Neosh, PermissionMode, PluginContext } from "@neosh/api";
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

  // What the agent may do without asking, in the footer, with the key that changes it. It belongs
  // on screen the whole time it is anything other than the default: "full access" is a state you
  // should never be in by accident, and the only way to guarantee that is to keep saying so.
  const showMode = async () => {
    const mode = await neosh.permission.mode().catch(() => null);
    if (!mode) return;
    await neosh.status.set("mode", {
      text: label(mode),
      keys: "^Y",
      hl: mode === "allow" ? "Diagnostic.Warn" : mode === "deny" ? "Comment" : "Status.Line",
      priority: 5,
    });
  };
  await neosh.cmd.register("permission.cycle", async () => {
    const order: PermissionMode[] = ["ask", "allow_listed", "allow", "deny"];
    const now = await neosh.permission.mode().catch(() => "ask" as PermissionMode);
    const next = order[(order.indexOf(now) + 1) % order.length] ?? "ask";
    await neosh.permission.setMode(next);
    await showMode();
    neosh.notify(`permissions: ${label(next)}`, next === "allow" ? "warn" : "info");
  }, { desc: "Cycle what the agent may do without asking" });
  await neosh.cmd.register("permission.pick", () => pickMode(neosh, showMode), {
    desc: "Choose what the agent may do without asking",
  });
  await neosh.keymap.set("chat", "<C-y>", "permission.pick", { desc: "Permission mode" });
  await showMode();

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

/** The word for a mode, as it reads in the footer. */
function label(mode: PermissionMode): string {
  switch (mode) {
    case "allow": return "full access";
    case "allow_listed": return "allow-listed";
    case "deny": return "deny";
    default: return "ask";
  }
}

/**
 * Choose a mode from a list rather than cycling blindly past the dangerous one.
 *
 * Cycling is bound too, for people who know the order — but the default key opens this, because
 * "full access" is one keystroke away from "ask" in any cycle, and arriving there by holding a key
 * down is precisely the accident worth designing against.
 */
async function pickMode(neosh: Neosh, after: () => Promise<void>): Promise<void> {
  const now = await neosh.permission.mode().catch(() => "ask" as PermissionMode);
  const rows: Array<{ label: string; detail: string; value: PermissionMode }> = [
    { label: "Ask", detail: "prompt before writing, running or connecting", value: "ask" },
    { label: "Allow-listed", detail: "only what config.toml already permits", value: "allow_listed" },
    { label: "Full access", detail: "no prompts — still confined to this workspace", value: "allow" },
    { label: "Deny", detail: "refuse everything; read-only", value: "deny" },
  ];
  const chosen = await picker(neosh, rows, {
    title: "Permissions",
    width: 62,
    height: rows.length,
    selected: Math.max(0, rows.findIndex((r) => r.value === now)),
  });
  if (!chosen || chosen === now) return;
  await neosh.permission.setMode(chosen);
  await after();
  neosh.notify(`permissions: ${label(chosen)}`, chosen === "allow" ? "warn" : "info");
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
