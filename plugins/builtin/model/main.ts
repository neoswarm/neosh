/**
 * The model switcher, and the reasoning-effort switcher next to it.
 *
 * Both are the same thing twice: a picker over what the *driver* said it supports. The effort
 * levels are not hard-coded here — they arrive as `ProviderOptionDescriptor`s attached to the
 * chosen model, so a provider plugin that invents a new knob gets a working picker for it with no
 * change to this file. That is the whole reason that descriptor type exists.
 */

import type {
  CredentialInfo,
  CredentialSource,
  ModelEntry,
  ModelSelection,
  Neosh,
  OptionSelection,
  PluginContext,
  ProviderOptionDescriptor,
} from "@neosh/api";
import { confirmDestructive, defineHighlights, picker } from "@neosh/api/ui";

type SelectDescriptor = Extract<ProviderOptionDescriptor, { type: "select" }>;

export async function activate({ neosh, subscriptions }: PluginContext) {
  await defineHighlights(neosh);

  await neosh.opt.declare({
    name: "model.picker.show_pricing",
    type: { type: "bool" },
    default: true,
    description: "Show cost per million tokens beside each model, where the provider reports it.",
  });

  await neosh.cmd.register("model.pick", () => pickModel(neosh), {
    desc: "Choose the model for this conversation",
  });
  await neosh.cmd.register("model.options", () => pickOptions(neosh), {
    desc: "Choose reasoning effort and other per-model options",
  });
  await neosh.cmd.register("provider.auth", () => pickProvider(neosh), {
    desc: "Sign in to a provider, or forget a key",
  });

  await neosh.keymap.set("chat", "<C-p>", "model.pick", { desc: "Pick a model" });
  await neosh.keymap.set("chat", "<C-e>", "model.options", { desc: "Pick reasoning effort" });

  // The footer. The model and its options belong beside the composer rather than on a settings
  // page: they are the two things you change mid-conversation.
  const footer = async () => {
    const selection = await neosh.agent.selection().catch(() => null);
    if (!selection) {
      await neosh.status.set("model", { text: "no model", hl: "Diagnostic.Warn", priority: 10 });
      return;
    }
    const options = (selection.options ?? []).map((o) => String(o.value)).join(" ");
    // Whether the next turn can authenticate belongs beside the model name, not in the error it
    // would otherwise become. "opus · no key" is something you fix before you have typed the
    // question; a failure on send is something you fix after losing your train of thought.
    const cred = (await neosh.agent.credentials().catch(() => []))
      .find((c) => c.instance === selection.instance);
    const unusable = cred?.source.kind === "missing";
    await neosh.status.set("model", {
      text: `${selection.model}${options ? ` ${options}` : ""}${unusable ? "  no key" : ""}`,
      hl: unusable ? "Diagnostic.Warn" : "Accent",
      priority: 10,
    });
  };
  await footer();
  subscriptions.push(neosh.session.onChange(() => void footer()));
  subscriptions.push(neosh.agent.onTurnEnd(() => void footer()));
  subscriptions.push(
    neosh.opt.onChange((e) => {
      if (e.name === "agent.model") void footer();
    }),
  );
  // Set after every switch too, since `setSelection` does not fire an option change.
  refreshFooter = footer;
}

/** Set by `activate`, so the pickers can update the footer the moment a choice lands. */
let refreshFooter: () => Promise<void> = async () => {};

/**
 * How an instance's key situation reads in a list.
 *
 * Said in words rather than shown as a state icon, because the useful distinction is not
 * "authenticated or not" — it is *which* of four things is true, and each one implies a different
 * next action.
 */
function describe(source: CredentialSource): string {
  switch (source.kind) {
    case "env":
      return `$${source.var}`;
    case "keychain":
      return "keychain";
    case "session":
      return "this session only";
    case "command":
      return "credential helper";
    case "inherited":
      // The whole reason this instance is first in the catalogue, and the answer to "why does it
      // say I have no API key" — you do not need one.
      return "your CLI login — no key needed";
    case "not_needed":
      return "local — no key needed";
    case "missing":
      return "needs a key";
  }
}

/** Instances by id, so a model row can say what will happen when you pick it. */
async function credentialMap(neosh: Neosh): Promise<Map<string, CredentialInfo>> {
  const rows = await neosh.agent.credentials().catch(() => [] as CredentialInfo[]);
  return new Map(rows.map((r) => [r.instance, r]));
}

/** A model row, or the row that offers to sign you in to a provider with no key. */
type Pick =
  | { kind: "model"; instance: string; model: ModelEntry["model"] }
  | { kind: "signin"; instance: string };

async function pickModel(neosh: Neosh): Promise<void> {
  // The host pairs each model with the instance that serves it. Guessing that here would be wrong
  // whenever two instances offer the same model id — and wrong quietly, by sending the next turn to
  // a different endpoint.
  const [entries, current, creds] = await Promise.all([
    neosh.agent.listModels(),
    neosh.agent.selection(),
    credentialMap(neosh),
  ]);

  const showPricing = await neosh.opt.get<boolean>("model.picker.show_pricing");
  const ready = (instance: string) => creds.get(instance)?.source.kind !== "missing";

  const models = entries.map(({ instance, model }) => {
    const cred = creds.get(instance);
    const bits: string[] = [instance];
    if (cred) bits.push(describe(cred.source));
    if (model.capabilities?.thinking) bits.push("thinking");
    if (showPricing && model.pricing) {
      bits.push(`$${model.pricing.input_per_mtok}/$${model.pricing.output_per_mtok} per Mtok`);
    }
    return {
      label: model.display_name,
      detail: bits.join("  ·  "),
      keywords: `${model.id} ${instance}`,
      value: { kind: "model", instance, model } as Pick,
      // Everything you can actually talk to, first. A list that opens on a model which will fail
      // on send is a list that taught you the wrong thing about your own setup.
      rank: ready(instance) ? 0 : 1,
    };
  });

  // An endpoint with no key discovers no models, so without these rows OpenAI is not merely
  // unusable — it is invisible, and there is nowhere to go to fix that.
  const signin = [...creds.values()]
    .filter((c) => c.driver_available && c.accepts_key && c.source.kind === "missing")
    .filter((c) => !entries.some((e) => e.instance === c.instance))
    .map((c) => ({
      label: `Sign in to ${c.display_name}…`,
      detail: "enter an API key",
      keywords: c.instance,
      value: { kind: "signin", instance: c.instance } as Pick,
      rank: 2,
    }));

  const items = [...models, ...signin].sort((a, b) => a.rank - b.rank);
  if (items.length === 0) {
    neosh.notify("no models are reachable — try `neosh --list-models`", "warn");
    return;
  }

  const at = current
    ? items.findIndex(
        (i) =>
          i.value.kind === "model" &&
          i.value.model.id === current.model &&
          i.value.instance === current.instance,
      )
    : 0;

  const chosen = await picker(neosh, items, {
    title: "Model",
    selected: Math.max(0, at),
    width: 76,
  });
  if (!chosen) return;

  if (chosen.kind === "signin") {
    if (!(await signIn(neosh, chosen.instance, creds.get(chosen.instance)))) return;
    // Straight back to the list, which now has the models that endpoint serves in it. Making you
    // press the key again would be making you ask twice for one thing.
    await pickModel(neosh);
    return;
  }

  // Picked something that cannot authenticate: offer the key now rather than at send time, when
  // the failure is a wall of red between you and the question you were asking.
  if (!ready(chosen.instance)) {
    const cred = creds.get(chosen.instance);
    if (cred?.accepts_key && !(await signIn(neosh, chosen.instance, cred))) return;
  }

  // Carry over option values that still exist on the new model. Switching between two thinking
  // models should keep your effort level; switching to one without the knob must drop it rather
  // than send a value the driver will reject.
  const descriptors = chosen.model.capabilities?.option_descriptors ?? [];
  const keep = (current?.options ?? []).filter((o) => descriptors.some((d) => d.id === o.id));

  await neosh.agent.setSelection({
    instance: chosen.instance,
    model: chosen.model.id,
    options: keep,
  });
  await refreshFooter();
  neosh.notify(`model: ${chosen.instance}/${chosen.model.display_name}`);
}

/**
 * Collect a key for one instance.
 *
 * The typing happens in the host — this call does not settle until the prompt closes, and what
 * comes back is whether a key was stored, never the key. Afterwards the endpoint is re-queried,
 * because the reason it listed nothing a moment ago was that it could not authenticate.
 */
async function signIn(
  neosh: Neosh,
  instance: string,
  cred: CredentialInfo | undefined,
): Promise<boolean> {
  if (cred && !cred.accepts_key) {
    neosh.notify(`${cred.display_name} signs in on its own — there is nowhere to put a key`, "warn");
    return false;
  }
  const stored = await neosh.agent
    .setCredential(instance, { replace: true })
    .catch((e: unknown) => {
      neosh.notify(String(e), "warn");
      return false;
    });
  if (!stored) return false;
  await neosh.agent.listModels(instance, { refresh: true }).catch(() => []);
  return true;
}

/**
 * Every configured provider and the state of its key.
 *
 * A settings page for the one setting people get stuck on. It lists instances that serve no models
 * yet, which is exactly the state you are in when you have not signed in — and therefore the state
 * the model picker cannot help you out of on its own.
 */
async function pickProvider(neosh: Neosh): Promise<void> {
  const rows = (await neosh.agent.credentials()).filter((c) => c.driver_available);
  if (rows.length === 0) {
    neosh.notify("no providers are configured", "warn");
    return;
  }
  const chosen = await picker(
    neosh,
    rows.map((c) => ({
      label: c.display_name,
      detail: `${c.instance}  ·  ${describe(c.source)}`,
      keywords: c.instance,
      value: c,
    })),
    { title: "Providers", width: 76 },
  );
  if (!chosen) return;

  if (!chosen.accepts_key) {
    neosh.notify(`${chosen.display_name}: ${describe(chosen.source)}`, "info");
    return;
  }
  if (chosen.source.kind === "missing") {
    await signIn(neosh, chosen.instance, chosen);
    return;
  }

  // It already has one, so the two things you could mean are "replace it" and "get rid of it".
  const action = await picker<"replace" | "forget">(
    neosh,
    [
      { label: "Enter a new key", detail: `replaces ${describe(chosen.source)}`, value: "replace" },
      { label: "Forget the stored key", detail: "removes it from the keychain too", value: "forget" },
    ],
    { title: chosen.display_name, width: 64, height: 2 },
  );
  if (action === null) return;
  if (action === "replace") {
    await signIn(neosh, chosen.instance, chosen);
    return;
  }
  const ok = await confirmDestructive(
    neosh,
    `Forget the key for ${chosen.display_name}?`,
    { yes: "Forget", no: "Keep" },
  );
  if (!ok) return;
  await neosh.agent.forgetCredential(chosen.instance);
  await refreshFooter();
}

async function pickOptions(neosh: Neosh): Promise<void> {
  const current = await neosh.agent.selection();
  if (!current) {
    neosh.notify("no model selected", "warn");
    return;
  }
  const entries = await neosh.agent.listModels(current.instance);
  const descriptors =
    entries.find((e) => e.model.id === current.model)?.model.capabilities?.option_descriptors ?? [];
  if (descriptors.length === 0) {
    neosh.notify(`${current.model} has no options to set`);
    return;
  }

  // One knob goes straight to its values; more than one asks which knob first. Keeping the common
  // case — reasoning effort, and nothing else — a single keystroke is worth the branch.
  const descriptor =
    descriptors.length === 1
      ? descriptors[0]
      : await picker(
          neosh,
          descriptors.map((d) => ({ label: d.label, detail: d.description ?? "", value: d })),
          { title: `${current.model} options`, width: 64 },
        );
  if (!descriptor) return;

  if (descriptor.type === "boolean") {
    const chosen = await picker<boolean>(
      neosh,
      [
        { label: "On", value: true },
        { label: "Off", value: false },
      ],
      { title: descriptor.label, height: 2, width: 40 },
    );
    if (chosen === null) return;
    await applyOption(neosh, current, descriptor.id, chosen);
    neosh.notify(`${descriptor.label}: ${chosen ? "on" : "off"}`);
    return;
  }

  const select: SelectDescriptor = descriptor;
  const currentValue =
    current.options?.find((o) => o.id === select.id)?.value ??
    select.current_value ??
    select.options.find((o) => o.is_default)?.id;

  const items = select.options.map((o) => ({
    label: o.label,
    detail: o.description ?? (o.is_default ? "default" : ""),
    value: o.id,
  }));

  const chosen = await picker(neosh, items, {
    title: select.label,
    selected: Math.max(0, items.findIndex((i) => i.value === currentValue)),
    width: 64,
  });
  if (chosen === null) return;
  await applyOption(neosh, current, select.id, chosen);
  neosh.notify(`${select.label}: ${select.options.find((o) => o.id === chosen)?.label ?? chosen}`);
}

async function applyOption(
  neosh: Neosh,
  current: ModelSelection,
  id: string,
  value: string | boolean,
): Promise<void> {
  const options: OptionSelection[] = (current.options ?? []).filter((o) => o.id !== id);
  options.push({ id, value });
  await neosh.agent.setSelection({ ...current, options });
  await refreshFooter();
}
