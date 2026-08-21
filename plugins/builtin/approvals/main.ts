/**
 * The approval prompt.
 *
 * Without something answering, `ask` means "refuse": every gated tool call comes back as an error
 * the model cannot act on and the user never sees a question. This is the half that asks.
 *
 * `permissions.mode` starts at `allow` — see `PermissionsConfig::default` for why — so on a fresh
 * install this hook never fires. That is not a reason for it to be quieter: the mode is a *per
 * conversation* setting, saved with the conversation, so any one of them may be back in `ask` and
 * the footer is what says which.
 *
 * It is a **blocking** hook, so the rule from ADR 0009 applies — a blocking hook that does not
 * answer in time is a veto. A permission prompt that failed open would be worse than no prompt at
 * all, so the timeout is generous enough for a human and the failure direction is "no".
 *
 * Remembering an answer is deliberately per-session and in memory. Writing "always allow" to disk
 * is how a policy quietly becomes permanent, and the file to edit for that is `config.toml`.
 */

import type {
  Capability,
  HookOutcome,
  HookPayload,
  Neosh,
  PermissionMode,
  PermissionOption,
  PluginContext,
} from "@neosh/api";
import { picker } from "@neosh/api/ui";

/** How long the user has. Past this the hook times out, which the host reads as a refusal. */
const ASK_TIMEOUT_MS = 120_000;

/** One row of the prompt: what it says, and what answering it means. */
type Choice = {
  label: string;
  detail?: string;
  /** An option the asker itself offered, handed back by id. */
  value: { kind: "option"; id: string } | { kind: "once" } | { kind: "session" } | { kind: "no" };
};

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

  // What the agent may do without asking, in the footer, with the key that changes it. On screen
  // the whole time, in every mode: it is a per-conversation setting now, so "what am I in" is a
  // question with a different answer in the next conversation along, and a row that only appeared
  // for the unusual values would be a row you had to remember the absence of.
  const showMode = async () => {
    const mode = await neosh.permission.mode().catch(() => null);
    if (!mode) return;
    await neosh.status.set("mode", {
      text: label(mode),
      keys: "⇧⇥",
      // Not a warning any more, because it is the state you are in by default and a footer that
      // cries wolf on every conversation is a footer nobody reads. Still its own colour, so the
      // row answers "what am I in" at a glance rather than by being read.
      hl: mode === "allow" ? "Agent.ToolEdit" : mode === "deny" ? "Comment" : "Status.Line",
      priority: 5,
    });
  };
  // Switching conversations changes the mode, because the mode belongs to the conversation. The
  // footer has to be told, or it keeps saying what the conversation you just left was set to —
  // which is the one kind of wrong this row must never be.
  subscriptions.push(neosh.session.onChange(() => {
    allowed.clear();
    void showMode();
  }));

  await neosh.cmd.register("permission.cycle", async () => {
    const order: PermissionMode[] = ["ask", "allow_listed", "allow", "deny"];
    const now = await neosh.permission.mode().catch(() => "ask" as PermissionMode);
    const next = order[(order.indexOf(now) + 1) % order.length] ?? "ask";
    await neosh.permission.setMode(next);
    await showMode();
    neosh.notify(`permissions: ${label(next)}`, "info");
  }, { desc: "Cycle what the agent may do without asking" });
  await neosh.cmd.register("permission.pick", () => pickMode(neosh, showMode), {
    desc: "Choose what the agent may do without asking",
  });
  // Shift-Tab, which is what codex uses for the same idea and what people's fingers already do.
  // Not a control key: every free one is one somebody's `init.ts` has taken, and this is the sort
  // of binding that should not need arguing about.
  await neosh.keymap.set("chat", "<S-Tab>", "permission.pick", { desc: "Permission mode" });
  await showMode();

  subscriptions.push(
    await neosh.hook.register(
      "permission_pre",
      async (payload): Promise<HookOutcome> => {
        if (payload.hook !== "permission_pre") return { action: "continue" };
        // The asker's own sentence when there is one. An agent driver writes a better line than
        // anything reconstructible from the capability — it knows what it is doing and why.
        const key = payload.title ?? describe(payload.capability);

        if (allowed.has(key)) return { action: "continue" };

        const remember = (await neosh.opt.get<boolean>("approvals.remember")) ?? true;
        const choices = offered(payload.options, remember);

        const answer = await picker(neosh, choices, {
          title: key,
          width: Math.max(40, Math.min(88, key.length + 8)),
          height: choices.length,
        });

        // Dismissing is a no. A prompt you can escape into an allow is not a prompt.
        if (answer === null) return { action: "veto", reason: "not approved" };
        switch (answer.kind) {
          case "session":
            allowed.add(key);
            return { action: "continue" };
          case "once":
            return { action: "continue" };
          case "no":
            return { action: "veto", reason: "denied" };
          case "option": {
            // Back through the payload, because "allow, and stop asking about `cargo`" is a thing
            // only the agent that offered it can act on — a bare yes would throw that away.
            const chosen: HookPayload = { ...payload, chosen: answer.id };
            const taken = payload.options.find((o) => o.id === answer.id);
            return taken && !allows(taken)
              ? { action: "veto", reason: taken.label }
              : { action: "modify", payload: chosen };
          }
        }
      },
      { blocking: true, timeoutMs: ASK_TIMEOUT_MS },
    ),
  );
}

/** Whether an option is one of the ways of saying yes. */
function allows(o: PermissionOption): boolean {
  return o.kind === "allow_once" || o.kind === "allow_always";
}

/**
 * The rows to show, given what the asker offered.
 *
 * When it offered nothing — every built-in tool — the question is yes or no and these are neosh's
 * own three answers. When it offered something, those are the rows: an agent that can say "allow,
 * and don't ask again for this command" is offering something neosh cannot reproduce from a yes,
 * and replacing its wording with a generic one would be answering a different question.
 *
 * "Allow for this session" rides along either way. It is neosh's, not the agent's, and it is the
 * one answer that holds across a driver that forgets everything between turns.
 */
function offered(options: PermissionOption[], remember: boolean): Choice[] {
  const session: Choice[] = remember
    ? [{
      label: "Allow for this session",
      detail: "forgotten when you switch conversation",
      value: { kind: "session" },
    }]
    : [];
  if (options.length === 0) {
    return [
      { label: "Allow once", value: { kind: "once" } },
      ...session,
      { label: "Deny", value: { kind: "no" } },
    ];
  }
  // The agent's order, with the allows first: a list that opens on "reject always" is a list where
  // the fast answer is the destructive one.
  const rows = [...options].sort((a, b) => Number(allows(b)) - Number(allows(a)));
  const yes = rows.findIndex(allows);
  const out: Choice[] = rows.map((o) => ({
    label: o.label,
    value: { kind: "option", id: o.id },
  }));
  // After the last "allow", so the session answer sits with the other ways of saying yes.
  const at = yes < 0 ? out.length : rows.filter(allows).length;
  out.splice(at, 0, ...session);
  return out;
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
    {
      label: "Full access",
      detail: "no prompts — still confined to this workspace",
      value: "allow",
    },
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
  neosh.notify(`permissions: ${label(chosen)}`, "info");
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
