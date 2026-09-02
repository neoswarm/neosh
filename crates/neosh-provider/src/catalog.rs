//! Built-in instance presets and the Anthropic model catalog.
//!
//! # Why presets rather than a hardcoded model list
//!
//! Model lineups change faster than releases do, so a baked-in list is wrong within weeks. Every
//! OpenAI-compatible endpoint answers `GET /v1/models`, so most of this file is *endpoints* —
//! base URL, credential variable, display name — and the models are discovered at runtime.
//!
//! The Anthropic catalog below is the exception: it is written out because the per-model option
//! descriptors (which effort levels exist, whether fast mode is available, whether thinking can be
//! disabled) are not discoverable from any endpoint, and getting them wrong produces a 400 rather
//! than a graceful degradation.

use neosh_proto::{
    AuthRef, Brand, InstanceConfig, ModelCapabilities, ModelId, ModelInfo, ModelTier, OptionChoice,
    OptionSelection, Pricing, ProviderOptionDescriptor, ProviderOptionValue,
};

/// Reasoning-effort levels available on a given model generation.
///
/// `pub` under a longer name because a driver that *discovered* a ladder needs to build the same
/// descriptor: the picker keys off `"effort"` being the same knob everywhere, and a second spelling
/// of it invented in a driver is a knob that does not carry over when you switch models.
pub fn effort_levels(levels: &[&str], default: &str) -> ProviderOptionDescriptor {
    effort_with(levels, default, &[])
}

fn effort(levels: &[&str], default: &str) -> ProviderOptionDescriptor {
    effort_levels(levels, default)
}

/// A level bought with a word in the prompt rather than with a request parameter.
///
/// The id **is** the word. That is what makes injection generic: nothing between here and the
/// driver has to hold a table of magic strings — the value the picker stored is the thing that gets
/// written into the message, and a provider plugin that invents its own gets the same treatment
/// with no host change. See [`crate::prompt_injections`].
struct Magic {
    /// Also the word. `"ultrathink"`.
    id: &'static str,
    label: &'static str,
    description: &'static str,
}

/// A ladder, plus any levels that are spelled rather than sent.
///
/// The spelled ones sort last because they are past the top of the scale: `max` is the most a
/// parameter can ask for, and these are what you say when that was not enough.
fn effort_with(levels: &[&str], default: &str, magic: &[Magic]) -> ProviderOptionDescriptor {
    let mut options: Vec<OptionChoice> = levels
        .iter()
        .map(|l| OptionChoice {
            id: (*l).into(),
            label: level_label(l),
            description: None,
            is_default: *l == default,
        })
        .collect();
    options.extend(magic.iter().map(|m| OptionChoice {
        id: m.id.into(),
        label: m.label.into(),
        description: Some(m.description.into()),
        is_default: false,
    }));
    ProviderOptionDescriptor::Select {
        id: "effort".into(),
        label: "Effort".into(),
        description: Some("How much reasoning the model spends before answering.".into()),
        options,
        current_value: Some(default.into()),
        prompt_injected_values: magic.iter().map(|m| m.id.to_string()).collect(),
    }
}

/// A select over anything, for the knobs that are not the reasoning ladder.
fn select(
    id: &str,
    label: &str,
    description: &str,
    choices: &[(&str, &str, Option<&str>)],
    default: &str,
) -> ProviderOptionDescriptor {
    ProviderOptionDescriptor::Select {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        options: choices
            .iter()
            .map(|(id, label, desc)| OptionChoice {
                id: (*id).into(),
                label: (*label).into(),
                description: desc.map(str::to_string),
                is_default: *id == default,
            })
            .collect(),
        current_value: Some(default.into()),
        prompt_injected_values: Vec::new(),
    }
}

fn boolean(id: &str, label: &str, desc: &str) -> ProviderOptionDescriptor {
    ProviderOptionDescriptor::Boolean {
        id: id.into(),
        label: label.into(),
        description: Some(desc.into()),
        current_value: false,
        prompt_injected_word: None,
    }
}

/// How a reasoning level reads in a list.
///
/// Title case for almost all of them, and a lookup for the ones where it is wrong: `xhigh` is a
/// contraction, and "Xhigh" reads as a typo in a column of English words. Shared with the drivers
/// that *discover* their ladders, so a level `codex` reported and one written down here are spelled
/// the same way on screen.
pub fn level_label(id: &str) -> String {
    match id {
        "xhigh" => "X-high".into(),
        "ultra" => "Ultra".into(),
        other => title_case(other),
    }
}

/// `low` -> `Low`. Shared so a discovered effort level reads like a written-down one.
pub fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// The option values a model starts with: each select's default, each boolean off.
pub fn default_options(caps: &ModelCapabilities) -> Vec<OptionSelection> {
    caps.option_descriptors
        .iter()
        .filter_map(|d| match d {
            ProviderOptionDescriptor::Select { id, options, current_value, .. } => {
                let v = current_value
                    .clone()
                    .or_else(|| options.iter().find(|o| o.is_default).map(|o| o.id.clone()))?;
                Some(OptionSelection { id: id.clone(), value: ProviderOptionValue::Text(v) })
            }
            ProviderOptionDescriptor::Boolean { id, current_value, .. } => {
                Some(OptionSelection { id: id.clone(), value: ProviderOptionValue::Flag(*current_value) })
            }
        })
        .collect()
}

const EFFORT_5: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const EFFORT_46: &[&str] = &["low", "medium", "high", "max"];

/// What `claude` reads out of a message rather than off its command line.
///
/// **The harness's words, not the model's**, which is why they are on the CLI's catalogue and not
/// on Anthropic's. `api.anthropic.com` serves a model: it has `output_config.effort` and no opinion
/// about English, so a message beginning "ultrathink" is a message beginning with a word. The CLI
/// is a program wrapped around that model, and these are two of the things it looks for.
///
/// Advertising them where they do not work would be worse than not having them: the picker would
/// offer a level that silently does nothing, on the one provider where the same word does something
/// real — and "it worked yesterday" would be a difference of instance nobody can see.
///
/// # Why both of them are rungs
///
/// `ultracode` used to be a switch of its own, on the reasoning that orchestration is not depth and
/// a control that does two things says one. Which is true of the *mechanism* and wrong about the
/// choice: nobody sets a reasoning level and then separately decides how hard the harness should
/// work, they pick one thing off one scale and get on with it — and a switch sitting under the
/// ladder, drawn `off / on`, is a second question about the same decision. Every reference
/// implementation that has shipped this puts it on the ladder for that reason.
///
/// The cost is real and is the point: these are mutually exclusive now, because one message
/// carries one of these words. "Ultracode at max effort" is no longer expressible — it was, and
/// almost nobody meant it.
const CLAUDE_WORDS: &[Magic] = &[
    Magic {
        id: "ultracode",
        label: "Ultracode",
        description: "Let it orchestrate sub-agents by default. Slower, and thorough.",
    },
    Magic {
        id: "ultrathink",
        label: "Ultrathink",
        description: "Past the top of the ladder — said in the message, not sent as a level.",
    },
];

/// One entry in the Anthropic catalogue.
///
/// `family` and `tier` are what make "the best Opus" and "one rung down" answerable. They are
/// written here rather than parsed out of the id, because the moment a vendor changes its naming
/// scheme a parser puts a model on the wrong rung — and the failure is a turn billed at the wrong
/// price, not an error.
#[allow(clippy::too_many_arguments)]
fn claude(
    id: &str,
    name: &str,
    family: &str,
    tier: ModelTier,
    tagline: &str,
    ctx: u64,
    max_out: u64,
    price: (f64, f64),
    efforts: Option<&[&str]>,
    fast_mode: bool,
    legacy: bool,
) -> ModelInfo {
    let mut descriptors = Vec::new();
    if let Some(levels) = efforts {
        descriptors.push(effort(levels, "high"));
    }
    if fast_mode {
        descriptors.push(boolean(
            "fast_mode",
            "Fast mode",
            "Same model, higher output rate, premium pricing.",
        ));
    }
    ModelInfo {
        id: ModelId::from(id),
        display_name: name.into(),
        context_window: Some(ctx),
        max_output_tokens: Some(max_out),
        capabilities: ModelCapabilities {
            tools: true,
            vision: true,
            streaming: true,
            thinking: efforts.is_some(),
            prompt_caching: true,
            option_descriptors: descriptors,
        },
        pricing: Some(Pricing {
            input_per_mtok: price.0,
            output_per_mtok: price.1,
            cache_read_per_mtok: price.0 * 0.1,
            cache_write_per_mtok: price.0 * 1.25,
        }),
        family: Some(family.into()),
        tier: Some(tier),
        legacy,
        tagline: Some(tagline.into()),
    }
}

/// Anthropic models, with the option descriptors each generation actually accepts.
///
/// The effort/thinking split is load-bearing: `budget_tokens` is rejected outright on the 5 and
/// 4.7+ generations, and `effort` gained `xhigh` there, so a single flattened option set would
/// produce 400s on half the catalog.
pub fn anthropic_models() -> Vec<ModelInfo> {
    use ModelTier::{Balanced, Fast, Frontier};
    const OPUS: &str = "Most capable for complex work";
    const SONNET: &str = "Best for everyday tasks";
    // What the vendor says Fable is for. It read "Long-form writing and voice" here, which is a
    // guess from the name — the line is the top of the ladder for reasoning and long-horizon
    // agentic work, and it is the one row whose price says so.
    const FABLE: &str = "Deepest reasoning and long-running work";
    vec![
        claude("claude-opus-5", "Claude Opus 5", "opus", Frontier, OPUS, 1_000_000, 128_000, (5.0, 25.0), Some(EFFORT_5), true, false),
        claude("claude-fable-5-1", "Claude Fable 5.1", "fable", Frontier, FABLE, 1_000_000, 128_000, (10.0, 50.0), Some(EFFORT_5), false, false),
        claude("claude-fable-5", "Claude Fable 5", "fable", Frontier, FABLE, 1_000_000, 128_000, (10.0, 50.0), Some(EFFORT_5), false, true),
        claude("claude-sonnet-5", "Claude Sonnet 5", "sonnet", Balanced, SONNET, 1_000_000, 128_000, (2.0, 10.0), Some(EFFORT_5), false, false),
        claude("claude-opus-4-8", "Claude Opus 4.8", "opus", Frontier, OPUS, 1_000_000, 128_000, (5.0, 25.0), Some(EFFORT_5), true, true),
        claude("claude-opus-4-7", "Claude Opus 4.7", "opus", Frontier, OPUS, 1_000_000, 128_000, (5.0, 25.0), Some(EFFORT_5), false, true),
        claude("claude-opus-4-6", "Claude Opus 4.6", "opus", Frontier, OPUS, 1_000_000, 128_000, (5.0, 25.0), Some(EFFORT_46), false, true),
        claude("claude-sonnet-4-6", "Claude Sonnet 4.6", "sonnet", Balanced, SONNET, 1_000_000, 128_000, (3.0, 15.0), Some(EFFORT_46), false, true),
        claude("claude-haiku-4-5", "Claude Haiku 4.5", "haiku", Fast, "Fastest, for quick answers", 200_000, 64_000, (1.0, 5.0), None, false, false),
    ]
}

/// A select over how much context to buy: the 1M window is opt-in and billed differently.
///
/// Written as an option rather than as two catalogue rows, because it is the same model — a row
/// each would double the list and put the decision in the wrong place, which is beside the
/// reasoning effort, not among the products.
fn context_window(long_by_default: bool) -> ProviderOptionDescriptor {
    ProviderOptionDescriptor::Select {
        id: "context".into(),
        label: "Context".into(),
        description: Some("How much of the conversation the model can see at once.".into()),
        options: vec![
            OptionChoice {
                id: "200k".into(),
                label: "200k".into(),
                description: None,
                is_default: !long_by_default,
            },
            OptionChoice {
                id: "1m".into(),
                label: "1M".into(),
                description: Some("Premium pricing past 200k.".into()),
                is_default: long_by_default,
            },
        ],
        current_value: Some(if long_by_default { "1m".into() } else { "200k".into() }),
        prompt_injected_values: Vec::new(),
    }
}

/// What each `claude` model needs from the CLI, beyond its name.
///
/// `(id, name, family, tier, tagline, context, efforts, fast_mode, long_context, legacy)`.
type ClaudeCliEntry = (
    &'static str,
    &'static str,
    &'static str,
    ModelTier,
    &'static str,
    u64,
    Option<&'static [&'static str]>,
    bool,
    Option<bool>,
    bool,
);

/// Models reachable through the `claude` CLI.
///
/// # Why this is written down rather than discovered
///
/// The CLI has no "list models" call — `--model` takes a slug or an alias and resolves it itself,
/// and there is no way to ask what it would accept. So this is the catalogue, and
/// [`crate::drivers::claude_cli`] filters it against the version that is actually installed: an
/// older CLI rejects a model it has never heard of, and offering it anyway turns a switch into a
/// failed turn.
///
/// # Slugs, not aliases
///
/// This used to be three aliases — `opus`, `sonnet`, `haiku` — on the reasoning that an alias
/// never goes stale. It does not, but it also cannot say what it resolves to, cannot carry a
/// version's own option set, and hides every model that is not the newest in its line. Pinning
/// slugs and letting `model.line opus` do the resolving separates the two jobs: the catalogue says
/// what exists, and the ladder says which one is current.
pub fn claude_cli_models() -> Vec<ModelInfo> {
    use ModelTier::{Balanced, Fast, Frontier};
    const OPUS: &str = "Most capable for complex work";
    const SONNET: &str = "Best for everyday tasks";
    const FABLE: &str = "Deepest reasoning and long-running work";
    const ENTRIES: &[ClaudeCliEntry] = &[
        ("claude-opus-5", "Claude Opus 5", "opus", Frontier, OPUS, 1_000_000, Some(EFFORT_5), true, Some(true), false),
        ("claude-fable-5-1", "Claude Fable 5.1", "fable", Frontier, FABLE, 1_000_000, Some(EFFORT_5), false, Some(true), false),
        ("claude-fable-5", "Claude Fable 5", "fable", Frontier, FABLE, 1_000_000, Some(EFFORT_5), false, Some(true), true),
        ("claude-sonnet-5", "Claude Sonnet 5", "sonnet", Balanced, SONNET, 1_000_000, Some(EFFORT_5), false, Some(false), false),
        ("claude-haiku-4-5", "Claude Haiku 4.5", "haiku", Fast, "Fastest, for quick answers", 200_000, None, false, None, false),
        ("claude-opus-4-8", "Claude Opus 4.8", "opus", Frontier, OPUS, 1_000_000, Some(EFFORT_5), true, None, true),
        ("claude-opus-4-7", "Claude Opus 4.7", "opus", Frontier, OPUS, 1_000_000, Some(EFFORT_5), false, None, true),
        ("claude-opus-4-6", "Claude Opus 4.6", "opus", Frontier, OPUS, 1_000_000, Some(EFFORT_46), false, None, true),
        ("claude-opus-4-5", "Claude Opus 4.5", "opus", Frontier, OPUS, 200_000, Some(EFFORT_46), false, None, true),
        ("claude-sonnet-4-6", "Claude Sonnet 4.6", "sonnet", Balanced, SONNET, 1_000_000, Some(EFFORT_46), false, Some(false), true),
    ];

    ENTRIES
        .iter()
        .map(|(id, name, family, tier, tagline, ctx, efforts, fast, long, legacy)| {
            let mut descriptors = Vec::new();
            if let Some(levels) = efforts {
                descriptors.push(effort_with(levels, "high", CLAUDE_WORDS));
            } else {
                descriptors.push(boolean(
                    "thinking",
                    "Thinking",
                    "Let it reason before answering.",
                ));
            }
            if *fast {
                descriptors.push(boolean(
                    "fast_mode",
                    "Fast mode",
                    "Same model, higher output rate, premium pricing.",
                ));
            }
            if let Some(long_by_default) = *long {
                descriptors.push(context_window(long_by_default));
            }
            let mut m = ModelInfo::undescribed(*id, *name);
            m.context_window = Some(*ctx);
            m.capabilities = ModelCapabilities {
                tools: true,
                vision: true,
                streaming: true,
                thinking: efforts.is_some(),
                prompt_caching: true,
                option_descriptors: descriptors,
            };
            m.family = Some((*family).to_string());
            m.tier = Some(*tier);
            m.tagline = Some((*tagline).to_string());
            m.legacy = *legacy;
            m
        })
        .collect()
}

/// A model in a seed list: enough to pick one, not enough to pretend it is a full description.
///
/// `(id, display name, family, tier, tagline, superseded)`.
type Seed = (&'static str, &'static str, &'static str, ModelTier, &'static str, bool);

/// Turn a seed list into models, asking `knobs` what each one accepts.
///
/// Per model rather than per provider, because that is how the ladders actually differ: one
/// generation gained a level the one before it rejects, and a small model in the same lineup has no
/// reasoning knob at all.
fn seeded(seeds: &[Seed], knobs: fn(&str) -> Vec<ProviderOptionDescriptor>) -> Vec<ModelInfo> {
    seeds
        .iter()
        .map(|(id, name, family, tier, tagline, legacy)| {
            let descriptors = knobs(id);
            let mut m = ModelInfo::undescribed(*id, *name);
            m.capabilities = ModelCapabilities {
                tools: true,
                vision: true,
                streaming: true,
                thinking: descriptors.iter().any(|d| is_reasoning(d.id())),
                prompt_caching: false,
                option_descriptors: descriptors,
            };
            m.family = Some((*family).to_string());
            m.tier = Some(*tier);
            m.tagline = Some((*tagline).to_string());
            m.legacy = *legacy;
            m
        })
        .collect()
}

/// Whether a knob is about *reasoning* rather than about the shape of the answer.
///
/// One list, because three vendors spell the same idea three ways and `ModelCapabilities::thinking`
/// is a single bit that a picker uses to decide whether a model reasons at all.
fn is_reasoning(id: &str) -> bool {
    matches!(id, "effort" | "thinking" | "thinking_level" | "thinking_budget")
}

/// A model with no seeded knobs. Most endpoints, most of the time — see [`seed_models`].
fn no_knobs(_: &str) -> Vec<ProviderOptionDescriptor> {
    Vec::new()
}

/// Answer length, which every GPT-5-shaped endpoint takes and nothing else does.
pub fn verbosity_levels() -> ProviderOptionDescriptor {
    select(
        "verbosity",
        "Verbosity",
        "How much answer to write, independently of how hard it thought.",
        &[("low", "Low", Some("Terse.")), ("medium", "Medium", None), ("high", "High", Some("Explains itself."))],
        "medium",
    )
}

/// What OpenAI's own endpoint accepts beside the messages.
///
/// The ladder is `reasoning_effort`, and it is *not* Anthropic's: it starts at `minimal`, which has
/// no equivalent there, and the top rung differs by generation. Written down rather than guessed
/// from the id, because the failure mode of guessing is a 400 on send rather than a knob that reads
/// slightly wrong — the same reason the Anthropic catalogue is written down at all.
fn openai_knobs(id: &str) -> Vec<ProviderOptionDescriptor> {
    let levels: &[&str] = if id.starts_with("gpt-5.6") {
        &["minimal", "low", "medium", "high", "xhigh"]
    } else if id.starts_with("gpt-5") {
        &["minimal", "low", "medium", "high"]
    } else {
        return Vec::new();
    };
    vec![effort(levels, "medium"), verbosity_levels()]
}

/// What Gemini accepts, which changed shape between generations.
///
/// The 3 line takes `thinkingLevel`, a named rung. The 2.5 line takes `thinkingBudget`, a token
/// count — and the only value of it worth putting in a picker is "none", because every other number
/// is a guess about a turn that has not happened yet. So the 2.5 models get a switch and the 3 line
/// gets a ladder, which is what each of them actually has.
///
/// 2.5 Pro is deliberately blank: its budget cannot be set to zero, and offering a switch that the
/// endpoint rejects is worse than offering nothing.
pub fn gemini_knobs(id: &str) -> Vec<ProviderOptionDescriptor> {
    if id.starts_with("gemini-3") {
        return vec![select(
            "thinking_level",
            "Thinking",
            "How much reasoning the model spends before answering.",
            &[("low", "Low", None), ("high", "High", None)],
            "high",
        )];
    }
    if id.starts_with("gemini-2.5") && !id.contains("pro") {
        let mut on = boolean("thinking", "Thinking", "Let it reason before answering.");
        if let ProviderOptionDescriptor::Boolean { current_value, .. } = &mut on {
            *current_value = true;
        }
        return vec![on];
    }
    Vec::new()
}

/// What a provider offers, written down, so its models are visible before you have a key.
///
/// # Why this exists at all
///
/// Every OpenAI-shaped endpoint answers `GET /v1/models`, and discovery is still how the real list
/// is found. But discovery needs a key — so the picker used to show an empty pane for every
/// provider you had not signed into yet, which is exactly backwards: the list is *how you decide
/// whether to sign in*. A provider whose models you cannot see is a provider you cannot evaluate.
///
/// These are seeds, not truth. The moment a key is present, [`merge`] lets the endpoint's own
/// answer decide what exists, and keeps only the description from here — because names, rungs and
/// taglines are not in any `/v1/models` response and are what make the list readable.
///
/// Where the exact id is not certain, the vendor's stable alias is used in preference to a pinned
/// version, for the same reason `claude-cli` lists `opus` rather than a dated id: an alias that
/// resolves to something is better than an id that resolves to a 404.
fn seed_models(instance: &str) -> Vec<ModelInfo> {
    use ModelTier::{Balanced, Fast, Frontier};
    // Which knobs a seeded model has is a fact about the *vendor*, so it is chosen here rather than
    // written on every row: an endpoint either takes `reasoning_effort` or it does not, and the
    // twelve rows below would otherwise repeat the same answer twelve times and disagree on the
    // thirteenth.
    let knobs: fn(&str) -> Vec<ProviderOptionDescriptor> = match instance {
        "openai" => openai_knobs,
        "google" => gemini_knobs,
        // Nothing for xAI, DeepSeek or Mistral, and it is not an omission. None of the three
        // documents a reasoning knob on the models seeded here — Grok's is on the `mini` line only,
        // DeepSeek's reasoner has no dial at all, and Mistral's models do not reason. A knob
        // invented for them would be a 400 on the first turn, which is the failure this whole file
        // is written out longhand to avoid. Where an endpoint *does* say, the driver reads it back:
        // see `openai_compat`'s discovery.
        _ => no_knobs,
    };
    seeded(match instance {
        "openai" => &[
            ("gpt-5.6-sol", "GPT-5.6 Sol", "sol", Frontier, "Frontier agentic coding", false),
            ("gpt-5.6-terra", "GPT-5.6 Terra", "terra", Balanced, "Balanced, for everyday work", false),
            ("gpt-5.6-luna", "GPT-5.6 Luna", "luna", Fast, "Fast and affordable", false),
            ("gpt-5.5", "GPT-5.5", "gpt", Frontier, "Complex coding and long-running work", true),
            ("gpt-5.4", "GPT-5.4", "gpt", Balanced, "Strong for everyday coding", true),
            ("gpt-5.4-mini", "GPT-5.4 Mini", "gpt", Fast, "Small, quick, cheap", true),
        ],
        // The Flash line moves fastest of anything seeded here — three releases in three months —
        // which is the argument for seeds being a floor rather than the list: the moment a key is
        // present, `/v1/models` decides, and a row here only has to be reachable and readable.
        "google" => &[
            ("gemini-3.1-pro-preview", "Gemini 3.1 Pro", "gemini-pro", Frontier, "Most capable Gemini", false),
            ("gemini-3.8-flash", "Gemini 3.8 Flash", "gemini-flash", Balanced, "Quick, for everyday work", false),
            ("gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite", "gemini-flash-lite", Fast, "Cheapest and fastest", false),
            ("gemini-3.7-flash", "Gemini 3.7 Flash", "gemini-flash", Balanced, "Previous generation", true),
            ("gemini-2.5-pro", "Gemini 2.5 Pro", "gemini-pro", Frontier, "Previous generation", true),
            ("gemini-2.5-flash", "Gemini 2.5 Flash", "gemini-flash", Balanced, "Previous generation", true),
        ],
        "xai" => &[
            ("grok-4.6", "Grok 4.6", "grok", Frontier, "Latest Grok", false),
            ("grok-4.5", "Grok 4.5", "grok", Balanced, "Previous generation", true),
            ("grok-4.3", "Grok 4.3", "grok", Fast, "Older and cheaper", true),
        ],
        // Stable aliases: DeepSeek points these at whatever is current, so they do not go stale.
        "deepseek" => &[
            ("deepseek-reasoner", "DeepSeek Reasoner", "deepseek", Frontier, "Thinks before it answers", false),
            ("deepseek-chat", "DeepSeek Chat", "deepseek", Balanced, "General purpose", false),
        ],
        "mistral" => &[
            ("mistral-large-latest", "Mistral Large", "mistral", Frontier, "Most capable", false),
            ("mistral-medium-latest", "Mistral Medium", "mistral", Balanced, "General purpose", false),
            ("mistral-small-latest", "Mistral Small", "mistral", Fast, "Quick and cheap", false),
            ("codestral-latest", "Codestral", "codestral", Balanced, "Tuned for code", false),
        ],
        // Deliberately nothing for Groq, Cerebras, Together, Fireworks and the local endpoints:
        // they serve other people's models and the lineup changes weekly, so a written-down list
        // would be wrong faster than it was useful. Discovery is the honest answer there, and the
        // picker says so rather than showing an empty pane with no explanation.
        _ => &[],
    }, knobs)
}

/// Let a discovered list decide what exists, and a seed list decide how it reads.
///
/// An endpoint knows what it serves and nothing about how to describe it: `/v1/models` returns ids
/// and little else. The seeds know the display name, the rung and the one-line description, and
/// nothing about what your account can actually reach. Each is authoritative over its own half.
///
/// Seeded models sort first and in the order written, so the list opens on the handful worth
/// choosing between rather than on whatever an endpoint happened to return first — which for
/// OpenRouter is four hundred models beginning with `aion-labs`.
pub fn merge(discovered: Vec<ModelInfo>, seeds: &[ModelInfo]) -> Vec<ModelInfo> {
    if discovered.is_empty() {
        return seeds.to_vec();
    }
    let mut described: Vec<ModelInfo> = Vec::new();
    let mut rest: Vec<ModelInfo> = Vec::new();
    for m in discovered {
        match seeds.iter().find(|s| s.id == m.id) {
            // The seed's description, but the endpoint's capabilities: whether *this* deployment
            // supports tools is a fact about the deployment, not about the model.
            Some(seed) => {
                let mut out = seed.clone();
                out.context_window = m.context_window.or(seed.context_window);
                out.max_output_tokens = m.max_output_tokens.or(seed.max_output_tokens);
                out.pricing = m.pricing.clone().or_else(|| seed.pricing.clone());
                // An endpoint that listed its own parameters has said something the catalogue can
                // only have guessed, so it wins. One that said nothing leaves the seeds standing —
                // silence is not a claim that the model has no knobs.
                if !m.capabilities.option_descriptors.is_empty() {
                    out.capabilities.option_descriptors = m.capabilities.option_descriptors.clone();
                    out.capabilities.thinking = m.capabilities.thinking;
                }
                described.push(out);
            }
            None => rest.push(m),
        }
    }
    // Written order, not discovery order.
    described.sort_by_key(|m| seeds.iter().position(|s| s.id == m.id).unwrap_or(usize::MAX));
    described.extend(rest);
    described
}


/// Reasoning-effort levels `codex` accepts.
///
/// Its own ladder, not the API's: the CLI passes this straight through to `model_reasoning_effort`,
/// and a level it does not recognise is a config error rather than a graceful downgrade.
const EFFORT_CODEX: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];

/// Models reachable through the `codex` CLI.
///
/// Ids rather than aliases, because unlike `claude`, `codex` has no alias to pin: `-m` takes a
/// slug. Written out with rungs so the ladder works here too — this is the same catalogue the CLI
/// ships, reduced to the part a picker needs.
pub fn codex_cli_models() -> Vec<ModelInfo> {
    use ModelTier::{Balanced, Fast, Frontier};
    [
        ("gpt-5.6-sol", "GPT-5.6 Sol", "gpt-5.6", Frontier, "Frontier agentic coding", false),
        ("gpt-5.6-terra", "GPT-5.6 Terra", "gpt-5.6", Balanced, "Balanced, for everyday work", false),
        ("gpt-5.6-luna", "GPT-5.6 Luna", "gpt-5.6", Fast, "Fast and affordable", false),
        ("gpt-5.5", "GPT-5.5", "gpt-5", Frontier, "Complex coding and long-running work", true),
        ("gpt-5.4", "GPT-5.4", "gpt-5", Balanced, "Strong for everyday coding", true),
        ("gpt-5.4-mini", "GPT-5.4 Mini", "gpt-5", Fast, "Small, quick, cheap", true),
    ]
    .into_iter()
    .map(|(id, name, family, tier, tagline, legacy)| {
        let mut m = ModelInfo::undescribed(id, name);
        m.capabilities = ModelCapabilities {
            tools: true,
            vision: true,
            streaming: true,
            thinking: true,
            prompt_caching: true,
            option_descriptors: vec![effort(EFFORT_CODEX, "medium")],
        };
        m.family = Some(family.to_string());
        m.tier = Some(tier);
        m.tagline = Some(tagline.to_string());
        m.legacy = legacy;
        m
    })
    .collect()
}


/// Models reachable through an ACP agent.
///
/// Written down rather than discovered, because ACP has no "list models" call: the agent tells you
/// what it has in `session/new`, which is a poor place to be finding out. `session/set_model` is
/// how one is chosen, and an agent that only has one simply refuses the call and carries on — so a
/// wrong entry here degrades to "the agent used its default", not to an error.
fn acp_models(entries: &[(&str, &str, ModelTier, &str)]) -> Vec<ModelInfo> {
    entries
        .iter()
        .map(|(id, name, tier, tagline)| {
            let mut m = ModelInfo::undescribed(*id, *name);
            m.capabilities = ModelCapabilities {
                tools: true,
                vision: true,
                streaming: true,
                thinking: false,
                prompt_caching: false,
                option_descriptors: Vec::new(),
            };
            m.family = Some("agent".to_string());
            m.tier = Some(*tier);
            m.tagline = Some((*tagline).to_string());
            m
        })
        .collect()
}

fn instance(
    id: &str,
    driver: &str,
    name: &str,
    base_url: Option<&str>,
    auth: AuthRef,
    models: Vec<ModelInfo>,
) -> InstanceConfig {
    InstanceConfig {
        id: id.into(),
        driver: driver.into(),
        display_name: name.into(),
        base_url: base_url.map(str::to_string),
        brand: brand_for(id),
        auth,
        models,
        extra_headers: Vec::new(),
    }
}

fn env(var: &str) -> AuthRef {
    AuthRef::Env { var: var.into() }
}

/// A plan: a vendor CLI that carries its own login.
fn cli(program: &str, login: &str) -> AuthRef {
    AuthRef::Cli { program: program.into(), login: Some(login.into()) }
}

/// How each provider is drawn.
///
/// Three marks per provider, because a terminal is not one thing: a Nerd Font glyph where the font
/// has one, geometry where it does not, and a letter under `ui.ascii_only`. The colour is a
/// highlight group rather than a value, so a theme restyles every mark at once and a 16-colour
/// terminal still gets something.
///
/// Keyed by instance id rather than driver: `openai-compat` serves nine vendors, and drawing nine
/// different companies with one mark is the same as drawing none.
fn brand_for(id: &str) -> Option<Brand> {
    // Nerd Font code points, all from the `nf-` ranges shipped since v3. Where a vendor has no
    // glyph the field is `None` and the Unicode mark is used at every level — better a consistent
    // shape than a box.
    let (nerd, mark, ascii, hl) = match id {
        "claude-cli" | "anthropic" => (Some("\u{f0e79}"), "\u{2733}", "A", "Brand.Anthropic"),
        "codex-cli" | "openai" => (Some("\u{f0c9b}"), "\u{2b22}", "O", "Brand.OpenAI"),
        "gemini-cli" | "google" => (Some("\u{f05d1}"), "\u{25c6}", "G", "Brand.Google"),
        "openrouter" => (None, "\u{2b21}", "R", "Brand.OpenRouter"),
        "groq" => (None, "\u{25b6}", "Q", "Brand.Groq"),
        "deepseek" => (None, "\u{25c9}", "D", "Brand.DeepSeek"),
        "grok-cli" | "xai" => (Some("\u{f099}"), "\u{2715}", "X", "Brand.XAI"),
        "cursor-cli" => (None, "\u{25e9}", "C", "Brand.Cursor"),
        "mistral" => (None, "\u{25a4}", "M", "Brand.Mistral"),
        "together" => (None, "\u{2b1f}", "T", "Brand.Together"),
        "fireworks" => (None, "\u{2748}", "F", "Brand.Fireworks"),
        "cerebras" => (None, "\u{25d4}", "C", "Brand.Cerebras"),
        "ollama" => (Some("\u{f0f3b}"), "\u{25b2}", "L", "Brand.Local"),
        "llamacpp" | "lmstudio" | "vllm" => (None, "\u{25b3}", "L", "Brand.Local"),
        _ => return None,
    };
    Some(Brand::new(nerd, mark, ascii, hl))
}

/// Every instance neosh knows about out of the box.
///
/// Ordering matters: [`crate::ProviderRegistry::default_selection`] takes the first *usable* one,
/// and `claude-cli` is first because it needs no API key at all — a fresh install is useful
/// immediately if the user already has the `claude` CLI logged in.
///
/// Everything from `openai` down is one driver, `openai-compat`, pointed at different base URLs.
/// Adding a vendor that speaks the OpenAI shape is a line in this list, or a line in the user's
/// config — never code.
pub fn builtin_instances() -> Vec<InstanceConfig> {
    vec![
        // --- plans: a subscription you already pay for -----------------
        instance("claude-cli", "claude-cli", "Claude", None, cli("claude", "claude login"), claude_cli_models()),
        instance("codex-cli", "codex-cli", "Codex", None, cli("codex", "codex login"), codex_cli_models()),
        // Everything below speaks the Agent Client Protocol, so they are one driver pointed at
        // three programs. A fourth is a line here, not a file.
        instance("cursor-cli", "cursor-cli", "Cursor", None, cli("cursor-agent", "cursor-agent login"), acp_models(&[
            ("gpt-5.6-sol", "GPT-5.6 Sol", ModelTier::Frontier, "Frontier agentic coding"),
            ("claude-opus-5", "Claude Opus 5", ModelTier::Frontier, "Most capable for complex work"),
            ("claude-sonnet-5", "Claude Sonnet 5", ModelTier::Balanced, "Best for everyday tasks"),
            ("composer-1", "Composer", ModelTier::Fast, "Cursor's own, tuned for speed"),
        ])),
        instance("grok-cli", "grok-cli", "Grok", None, cli("grok", "grok login"), acp_models(&[
            ("grok-4.6", "Grok 4.6", ModelTier::Frontier, "Latest Grok"),
            ("grok-4.5", "Grok 4.5", ModelTier::Balanced, "Previous generation"),
        ])),
        instance("gemini-cli", "gemini-cli", "Gemini", None, cli("gemini", "gemini auth login"), acp_models(&[
            ("gemini-3.1-pro-preview", "Gemini 3.1 Pro", ModelTier::Frontier, "Most capable Gemini"),
            ("gemini-3.8-flash", "Gemini 3.8 Flash", ModelTier::Balanced, "Quick, for everyday work"),
            ("gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite", ModelTier::Fast, "Cheapest and fastest"),
        ])),
        // --- API keys: billed per token --------------------------------
        instance("anthropic", "anthropic", "Anthropic", Some("https://api.anthropic.com"), env("ANTHROPIC_API_KEY"), anthropic_models()),
        instance("google", "google", "Google Gemini", Some("https://generativelanguage.googleapis.com/v1beta"), env("GEMINI_API_KEY"), seed_models("google")),
        // --- OpenAI-compatible endpoints -------------------------------
        instance("openai", "openai-compat", "OpenAI", Some("https://api.openai.com/v1"), env("OPENAI_API_KEY"), seed_models("openai")),
        instance("openrouter", "openai-compat", "OpenRouter", Some("https://openrouter.ai/api/v1"), env("OPENROUTER_API_KEY"), vec![]),
        instance("groq", "openai-compat", "Groq", Some("https://api.groq.com/openai/v1"), env("GROQ_API_KEY"), vec![]),
        instance("deepseek", "openai-compat", "DeepSeek", Some("https://api.deepseek.com/v1"), env("DEEPSEEK_API_KEY"), seed_models("deepseek")),
        instance("xai", "openai-compat", "xAI", Some("https://api.x.ai/v1"), env("XAI_API_KEY"), seed_models("xai")),
        instance("mistral", "openai-compat", "Mistral", Some("https://api.mistral.ai/v1"), env("MISTRAL_API_KEY"), seed_models("mistral")),
        instance("together", "openai-compat", "Together", Some("https://api.together.xyz/v1"), env("TOGETHER_API_KEY"), vec![]),
        instance("fireworks", "openai-compat", "Fireworks", Some("https://api.fireworks.ai/inference/v1"), env("FIREWORKS_API_KEY"), vec![]),
        instance("cerebras", "openai-compat", "Cerebras", Some("https://api.cerebras.ai/v1"), env("CEREBRAS_API_KEY"), vec![]),
        // --- local endpoints, no credentials ---------------------------
        instance("ollama", "openai-compat", "Ollama (local)", Some("http://localhost:11434/v1"), AuthRef::None, vec![]),
        instance("llamacpp", "openai-compat", "llama.cpp (local)", Some("http://localhost:8080/v1"), AuthRef::None, vec![]),
        instance("lmstudio", "openai-compat", "LM Studio (local)", Some("http://localhost:1234/v1"), AuthRef::None, vec![]),
        instance("vllm", "openai-compat", "vLLM (local)", Some("http://localhost:8000/v1"), AuthRef::None, vec![]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_you_have_not_signed_into_still_lists_what_it_offers() {
        // The picker used to show an empty pane for every provider without a key, which is exactly
        // backwards: the list of models is *how you decide whether to sign in*.
        for id in ["openai", "google", "xai", "deepseek", "mistral"] {
            let inst = builtin_instances().into_iter().find(|i| i.id.0 == id).unwrap();
            assert!(!inst.models.is_empty(), "{id} lists nothing before it has a key");
            assert!(
                inst.models.iter().all(|m| m.tier.is_some() && m.family.is_some()),
                "{id} has a model off the ladder, so upgrade and downgrade would skip it"
            );
        }
    }

    #[test]
    fn discovery_says_what_exists_and_the_catalogue_says_how_it_reads() {
        let seeds = seed_models("openai");
        let known = seeds[0].id.clone();
        let discovered = vec![
            ModelInfo::undescribed(known.0.as_str(), known.0.as_str()),
            ModelInfo::undescribed("something-new", "something-new"),
        ];
        let merged = merge(discovered, &seeds);

        assert_eq!(merged.len(), 2, "nothing invented, nothing dropped");
        assert_eq!(merged[0].id, known, "a described model sorts first");
        assert_eq!(merged[0].display_name, seeds[0].display_name, "and keeps its name");
        assert!(merged[0].tier.is_some(), "and its rung");
        assert_eq!(merged[1].id.0, "something-new", "and a model we had not heard of survives");
    }

    #[test]
    fn a_seeded_model_the_endpoint_does_not_serve_is_not_offered() {
        // The endpoint is the authority on what your account can reach. Keeping a seed it did not
        // return would offer a model whose first turn is a 404.
        let seeds = seed_models("openai");
        let discovered = vec![ModelInfo::undescribed("only-this", "only-this")];
        let merged = merge(discovered, &seeds);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id.0, "only-this");
    }

    #[test]
    fn nothing_from_the_endpoint_leaves_the_seeds_standing() {
        // Offline, or no key. Falling through to an empty list would make the provider look broken
        // rather than unauthenticated.
        let seeds = seed_models("mistral");
        assert_eq!(merge(vec![], &seeds).len(), seeds.len());
    }


    #[test]
    fn every_builtin_instance_has_a_unique_id() {
        let all = builtin_instances();
        let mut ids: Vec<_> = all.iter().map(|i| i.id.0.clone()).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate instance ids would silently shadow each other");
    }

    #[test]
    fn zero_config_instance_comes_first() {
        let all = builtin_instances();
        assert_eq!(all[0].id.0, "claude-cli");
        assert_eq!(
            all[0].auth.account_kind(),
            neosh_proto::AccountKind::Plan,
            "a fresh install must land on a plan, not on something that needs a key"
        );
        assert!(!all[0].models.is_empty(), "needs models so default_selection can resolve");
    }

    #[test]
    fn one_driver_serves_every_openai_compatible_vendor() {
        let compat: Vec<_> =
            builtin_instances().into_iter().filter(|i| i.driver.0 == "openai-compat").collect();
        assert!(compat.len() >= 10, "expected a broad preset list, got {}", compat.len());
        for i in &compat {
            assert!(i.base_url.is_some(), "{} needs a base URL", i.id);
        }
    }

    #[test]
    fn every_instance_says_what_you_are_spending() {
        // The split the flat list could not make. A plan and a key are different things to buy,
        // bill and revoke, and a picker that shows them as one set cannot tell you which one the
        // next turn spends.
        for inst in builtin_instances() {
            let kind = inst.auth.account_kind();
            match inst.id.as_ref() {
                "claude-cli" | "codex-cli" | "cursor-cli" | "grok-cli" | "gemini-cli" => {
                    assert_eq!(kind, neosh_proto::AccountKind::Plan, "{}", inst.id)
                }
                "ollama" | "llamacpp" | "lmstudio" | "vllm" => {
                    assert_eq!(kind, neosh_proto::AccountKind::Local, "{}", inst.id)
                }
                _ => assert_eq!(kind, neosh_proto::AccountKind::ApiKey, "{}", inst.id),
            }
        }
    }

    #[test]
    fn every_instance_has_something_to_draw() {
        // A rail of provider marks with one blank in it reads as a bug, not as a provider without
        // a logo.
        for inst in builtin_instances() {
            let brand = inst.brand.as_ref().unwrap_or_else(|| panic!("{} has no brand", inst.id));
            assert!(!brand.mark.is_empty(), "{}", inst.id);
            assert!(!brand.ascii.is_empty(), "{}", inst.id);
            assert!(brand.hl.starts_with("Brand."), "{} must theme through a group", inst.id);
        }
    }

    #[test]
    fn every_catalogued_model_is_on_the_ladder() {
        // `family` and `tier` are what make "the best Opus" and "one rung down" answerable. A model
        // missing them is invisible to both.
        for m in anthropic_models().into_iter().chain(claude_cli_models()) {
            assert!(m.family.is_some(), "{} has no family", m.id);
            assert!(m.tier.is_some(), "{} has no tier", m.id);
            assert!(m.tagline.is_some(), "{} has nothing to say for itself", m.id);
        }
    }

    #[test]
    fn the_cli_aliases_cover_all_three_rungs() {
        // The plan is the zero-setup path, so "one step down" has to work there before anywhere.
        let tiers: Vec<_> = claude_cli_models().iter().filter_map(|m| m.tier).collect();
        for rung in ModelTier::LADDER {
            assert!(tiers.contains(&rung), "no {rung:?} available on the CLI plan");
        }
    }

    #[test]
    fn a_superseded_model_is_folded_away_but_not_removed() {
        // You go looking for last year's model to reproduce something, which is exactly when
        // deleting it from the catalogue would be worst.
        let all = anthropic_models();
        assert!(all.iter().any(|m| m.legacy), "nothing is marked superseded");
        assert!(
            all.iter().filter(|m| m.family.as_deref() == Some("opus") && !m.legacy).count() == 1,
            "exactly one current model per line, or 'the best Opus' has no answer"
        );
    }

    #[test]
    fn local_instances_require_no_credentials() {
        for id in ["ollama", "llamacpp", "lmstudio", "vllm"] {
            let inst = builtin_instances().into_iter().find(|i| i.id.0 == id).unwrap();
            assert_eq!(inst.auth, AuthRef::None, "{id} must work without an API key");
        }
    }

    #[test]
    fn effort_levels_match_each_model_generation() {
        let models = anthropic_models();
        let find = |id: &str| models.iter().find(|m| m.id.0 == id).unwrap().clone();

        let opus5 = find("claude-opus-5");
        let ProviderOptionDescriptor::Select { options, .. } =
            &opus5.capabilities.option_descriptors[0]
        else {
            panic!("expected an effort select");
        };
        assert!(options.iter().any(|o| o.id == "xhigh"), "the 5 generation has xhigh");

        let opus46 = find("claude-opus-4-6");
        let ProviderOptionDescriptor::Select { options, .. } =
            &opus46.capabilities.option_descriptors[0]
        else {
            panic!("expected an effort select");
        };
        assert!(!options.iter().any(|o| o.id == "xhigh"), "4.6 predates xhigh");
    }

    #[test]
    fn fast_mode_is_offered_only_where_it_exists() {
        let models = anthropic_models();
        let has_fast = |id: &str| {
            models
                .iter()
                .find(|m| m.id.0 == id)
                .unwrap()
                .capabilities
                .option_descriptors
                .iter()
                .any(|d| d.id() == "fast_mode")
        };
        assert!(has_fast("claude-opus-5"));
        assert!(has_fast("claude-opus-4-8"));
        assert!(!has_fast("claude-sonnet-5"));
        assert!(!has_fast("claude-haiku-4-5"));
    }

    /// Every knob a model advertises, as `(id, values)`.
    fn knobs(m: &ModelInfo) -> Vec<(String, Vec<String>)> {
        m.capabilities
            .option_descriptors
            .iter()
            .map(|d| match d {
                ProviderOptionDescriptor::Select { id, options, .. } => {
                    (id.clone(), options.iter().map(|o| o.id.clone()).collect())
                }
                ProviderOptionDescriptor::Boolean { id, .. } => (id.clone(), Vec::new()),
            })
            .collect()
    }

    fn find(models: &[ModelInfo], id: &str) -> ModelInfo {
        models.iter().find(|m| m.id.0 == id).unwrap_or_else(|| panic!("no {id}")).clone()
    }

    #[test]
    fn a_reasoning_ladder_is_not_an_anthropic_privilege() {
        // The knob existed and only one vendor's models carried it, so `^E` on anything else said
        // "no options to set" — about models that document a reasoning parameter in their first
        // paragraph. Each of these is spelled the way its own endpoint spells it.
        let openai = find(&seed_models("openai"), "gpt-5.6-sol");
        let effort = knobs(&openai).into_iter().find(|(id, _)| id == "effort").expect("a ladder");
        assert!(effort.1.contains(&"minimal".to_string()), "OpenAI's starts below low");
        assert!(!effort.1.contains(&"max".to_string()), "and does not borrow Anthropic's top rung");
        assert!(knobs(&openai).iter().any(|(id, _)| id == "verbosity"));
        assert!(openai.capabilities.thinking, "a model with a ladder reasons, by definition");

        // Gemini changed the shape of the question between generations, so the two are different
        // knobs rather than one knob with a translation layer nobody can see.
        let three = find(&seed_models("google"), "gemini-3.1-pro-preview");
        assert_eq!(knobs(&three)[0].0, "thinking_level");
        let flash = find(&seed_models("google"), "gemini-2.5-flash");
        assert_eq!(knobs(&flash)[0].0, "thinking");
    }

    #[test]
    fn a_word_is_offered_only_where_something_reads_it() {
        // `ultrathink` is the harness's, not the model's. On the CLI it buys depth; sent to
        // `api.anthropic.com` as `output_config.effort` it is a 400, and put in a message there it
        // is a word. So the same lineup has it on one instance and not on the other.
        let cli = find(&claude_cli_models(), "claude-opus-5");
        let ProviderOptionDescriptor::Select { options, prompt_injected_values, .. } =
            &cli.capabilities.option_descriptors[0]
        else {
            panic!("expected an effort select");
        };
        assert_eq!(
            prompt_injected_values,
            &["ultracode".to_string(), "ultrathink".to_string()],
            "both words are rungs on the one ladder"
        );
        let tail: Vec<&str> = options.iter().rev().take(2).map(|o| o.id.as_str()).collect();
        assert_eq!(tail, ["ultrathink", "ultracode"], "past the top rung, in order");
        // And nowhere else. A switch left behind would be a second control for a choice the ladder
        // now owns, and turning it on beside `max` would put a word in the message that the level
        // beside it contradicts.
        assert!(
            !cli.capabilities.option_descriptors.iter().any(|d| matches!(
                d,
                ProviderOptionDescriptor::Boolean { prompt_injected_word: Some(_), .. }
            )),
            "no spoken switch survives beside the ladder"
        );

        let api = find(&anthropic_models(), "claude-opus-5");
        for d in &api.capabilities.option_descriptors {
            if let ProviderOptionDescriptor::Select { prompt_injected_values, .. } = d {
                assert!(prompt_injected_values.is_empty(), "the API reads no words");
            }
        }
    }

    #[test]
    fn a_spelled_level_never_becomes_the_value_that_gets_sent() {
        // `default_options` is what a freshly chosen model starts with. Starting on a level that
        // has to be rewritten before it can be sent would mean every first turn went through the
        // injection path, which is the path with the interesting bugs in it.
        let cli = find(&claude_cli_models(), "claude-opus-5");
        let opts = default_options(&cli.capabilities);
        assert_eq!(
            opts.iter().find(|o| o.id == "effort").map(|o| &o.value),
            Some(&ProviderOptionValue::Text("high".into()))
        );
    }

    #[test]
    fn a_model_without_thinking_advertises_no_effort_knob() {
        let haiku =
            anthropic_models().into_iter().find(|m| m.id.0 == "claude-haiku-4-5").unwrap();
        assert!(!haiku.capabilities.thinking);
        assert!(haiku.capabilities.option_descriptors.is_empty());
    }

    #[test]
    fn default_options_pick_each_select_default_and_leave_booleans_off() {
        let opus5 = anthropic_models().into_iter().find(|m| m.id.0 == "claude-opus-5").unwrap();
        let opts = default_options(&opus5.capabilities);
        let effort = opts.iter().find(|o| o.id == "effort").unwrap();
        assert_eq!(effort.value, ProviderOptionValue::Text("high".into()));
        let fast = opts.iter().find(|o| o.id == "fast_mode").unwrap();
        assert_eq!(fast.value, ProviderOptionValue::Flag(false));
    }
}
