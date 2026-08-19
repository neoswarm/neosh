/**
 * The model switcher, and the reasoning-effort switcher next to it.
 *
 * Both are the same thing twice: a picker over what the *driver* said it supports. The effort
 * levels are not hard-coded here — they arrive as `ProviderOptionDescriptor`s attached to the
 * chosen model, so a provider plugin that invents a new knob gets a working picker for it with no
 * change to this file. That is the whole reason that descriptor type exists.
 */

import type {
  ModelSelection,
  Neosh,
  OptionSelection,
  PluginContext,
  ProviderOptionDescriptor,
} from "@neosh/api";
import { defineHighlights, picker } from "@neosh/api/ui";

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
    await neosh.status.set("model", {
      text: `${selection.model}${options ? ` ${options}` : ""}`,
      hl: "Accent",
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

async function pickModel(neosh: Neosh): Promise<void> {
  // The host pairs each model with the instance that serves it. Guessing that here would be wrong
  // whenever two instances offer the same model id — and wrong quietly, by sending the next turn to
  // a different endpoint.
  const [entries, current] = await Promise.all([
    neosh.agent.listModels(),
    neosh.agent.selection(),
  ]);
  if (entries.length === 0) {
    neosh.notify("no models are reachable — try `neosh --list-models`", "warn");
    return;
  }

  const showPricing = await neosh.opt.get<boolean>("model.picker.show_pricing");
  const items = entries.map(({ instance, model }) => {
    const bits: string[] = [instance];
    if (model.capabilities?.thinking) bits.push("thinking");
    if (showPricing && model.pricing) {
      bits.push(`$${model.pricing.input_per_mtok}/$${model.pricing.output_per_mtok} per Mtok`);
    }
    return {
      label: model.display_name,
      detail: bits.join("  ·  "),
      keywords: `${model.id} ${instance}`,
      value: { model, instance },
    };
  });

  const at = current
    ? items.findIndex((i) => i.value.model.id === current.model && i.value.instance === current.instance)
    : 0;

  const chosen = await picker(neosh, items, {
    title: "Model",
    selected: Math.max(0, at),
    width: 72,
  });
  if (!chosen) return;

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
