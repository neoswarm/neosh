//! Built-in provider instances, and the models each of them offers.
//!
//! # Endpoints here, models in a file
//!
//! An instance — a base URL, a credential, a brand — changes when somebody writes a driver, so it
//! is code. A *model* changes when a vendor ships one, which is weekly, so the models are
//! `models.json` and [`crate::manifest`]: compiled in, with t3code's news about the vendor CLIs laid
//! over it by a running workspace, and read through the functions below. Adding a rung on a ladder,
//! a price or a CLI floor is a line in that file; a model the CLIs started serving is usually no
//! edit at all.
//!
//! Every OpenAI-compatible endpoint also answers `GET /v1/models`, so for most providers the file
//! is only the *seed*: [`merge`] lets the endpoint decide what exists and the file decide how it
//! reads.

use neosh_proto::{
    AuthRef, Brand, InstanceConfig, ModelCapabilities, ModelInfo, OptionChoice, OptionSelection,
    ProviderOptionDescriptor, ProviderOptionValue,
};

use crate::manifest;

/// A reasoning ladder, for a driver that *discovered* one.
///
/// The picker keys off `"effort"` being the same knob everywhere, and a second spelling of it
/// invented in a driver is a knob that does not carry over when you switch models.
pub fn effort_levels(levels: &[&str], default: &str) -> ProviderOptionDescriptor {
    select_of(
        "effort",
        "Effort",
        "How much reasoning the model spends before answering.",
        levels.iter().map(|l| ((*l).to_string(), level_label(l), None)).collect(),
        default,
    )
}

fn select_of(
    id: &str,
    label: &str,
    description: &str,
    choices: Vec<(String, String, Option<String>)>,
    default: &str,
) -> ProviderOptionDescriptor {
    ProviderOptionDescriptor::Select {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        options: choices
            .into_iter()
            .map(|(id, label, description)| OptionChoice {
                is_default: id == default,
                id,
                label,
                description,
            })
            .collect(),
        current_value: Some(default.into()),
        prompt_injected_values: Vec::new(),
    }
}

/// How a reasoning level reads in a list.
///
/// Title case for almost all of them, and a lookup for the ones where it is wrong: `xhigh` is a
/// contraction, and "Xhigh" reads as a typo in a column of English words. Shared with the drivers
/// that *discover* their ladders, so a level `codex` reported and one written down are spelled the
/// same way on screen.
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

/// Answer length, which every GPT-5-shaped endpoint takes and nothing else does.
pub fn verbosity_levels() -> ProviderOptionDescriptor {
    select_of(
        "verbosity",
        "Verbosity",
        "How much answer to write, independently of how hard it thought.",
        vec![
            ("low".into(), "Low".into(), Some("Terse.".into())),
            ("medium".into(), "Medium".into(), None),
            ("high".into(), "High".into(), Some("Explains itself.".into())),
        ],
        "medium",
    )
}

/// What Gemini accepts for a model id nobody wrote down, from the `google` catalogue's rules.
///
/// The 3 line takes `thinkingLevel`, a named rung, and the 2.5 line `thinkingBudget`, of which the
/// only value worth a picker is "none" — so a switch, except on 2.5 Pro, whose budget cannot be
/// zero. Rules rather than code, so a generation released this morning gets its ladder from a
/// manifest edit.
pub fn gemini_knobs(id: &str) -> Vec<ProviderOptionDescriptor> {
    manifest::knobs_for("google", id)
}

/// Anthropic's API models, with the option descriptors each generation actually accepts.
///
/// Newest first, which decides two things at once: what a picker draws at the top, and — through
/// [`crate::ProviderRegistry::default_selection`], which takes the first live entry — what a fresh
/// install with nothing configured lands on.
pub fn anthropic_models() -> Vec<ModelInfo> {
    manifest::models("anthropic")
}

/// Models reachable through the `claude` CLI.
///
/// The CLI has no "list models" call — `--model` takes a slug and resolves it itself — so this is
/// the catalogue, and [`crate::drivers::claude_cli`] marks what the installed version is too old
/// for (`min_version`) rather than hiding it. The words `ultrathink` and `ultracode` are rungs here
/// and nowhere on `anthropic`: they are read by the harness, not the model.
pub fn claude_cli_models() -> Vec<ModelInfo> {
    manifest::models("claude-cli")
}

/// Models reachable through the `codex` CLI, for when `model/list` cannot answer.
pub fn codex_cli_models() -> Vec<ModelInfo> {
    manifest::models("codex-cli")
}

/// Seeds for the `agy` CLI, replaced by `agy models` when it answers. Reasoning effort is part of
/// the id there rather than a knob beside it, because that is how Antigravity publishes them.
pub fn antigravity_models() -> Vec<ModelInfo> {
    manifest::models("antigravity")
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
                out.pricing = m.pricing.or(seed.pricing);
                // An endpoint that listed its own parameters has said something the catalogue can
                // only have guessed, so it wins. One that said nothing leaves the seeds standing —
                // silence is not a claim that the model has no knobs.
                if !m.capabilities.option_descriptors.is_empty() {
                    out.capabilities.option_descriptors = m.capabilities.option_descriptors.clone();
                    out.capabilities.thinking = m.capabilities.thinking;
                }
                // Taken whole rather than `or`-ed with the seed's: whether this machine can run it
                // is the driver's answer and only the driver's, and a seed is a catalogue — it has
                // never heard of the `claude` you have installed. `or` would also make the reason
                // sticky, so a model that started working after `claude update` would keep telling
                // you to run it.
                out.unavailable = m.unavailable.clone();
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

/// Give a discovered model nobody described the knobs its catalogue's rules say it has.
///
/// `/v1/models` names a model and says nothing about what it accepts, so a generation that shipped
/// this morning arrived with no `^E` at all until somebody wrote it down. A rule — `gpt-6` takes
/// this ladder — is written once per generation in `models.json`, and every model of it that an
/// endpoint returns gets the ladder from the first day. Only where nothing else has spoken: a seed
/// or an endpoint that listed its own parameters has said more than a prefix can.
pub fn with_rules(instance: &str, mut models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    for m in &mut models {
        if !m.capabilities.option_descriptors.is_empty() {
            continue;
        }
        let knobs = manifest::knobs_for(instance, m.id.as_ref());
        if knobs.is_empty() {
            continue;
        }
        m.capabilities.thinking = knobs.iter().any(|d| manifest::is_reasoning(d.id()));
        m.capabilities.option_descriptors = knobs;
    }
    models
}

fn instance(
    id: &str,
    driver: &str,
    name: &str,
    base_url: Option<&str>,
    auth: AuthRef,
) -> InstanceConfig {
    InstanceConfig {
        id: id.into(),
        driver: driver.into(),
        display_name: name.into(),
        base_url: base_url.map(str::to_string),
        brand: brand_for(id),
        auth,
        // By id, so a provider gains seeds — or a catalogue of its own — from a manifest edit.
        models: manifest::models(id),
        extra_headers: Vec::new(),
    }
}

fn env(var: &str) -> AuthRef {
    AuthRef::Env { var: var.into() }
}

/// A plan: a vendor CLI that carries its own login.
fn cli(program: &str, login: &str) -> AuthRef {
    AuthRef::Cli { program: program.into(), login: Some(login.into()), retired: None }
}

/// A plan the vendor has stopped serving, and the sentence saying so.
///
/// Listed rather than deleted, for the reason an uninstalled CLI is listed: a provider that simply
/// disappeared asks "where did it go" instead of answering it, and the answer here is one most
/// people will otherwise go and look for in our bug tracker. The entry is not usable, so nothing
/// defaults into it, and it is not *disabled* either — a plan can end for one tier and not another,
/// and an account the vendor still serves must still be able to pick a model on it.
fn retired_cli(program: &str, note: &str) -> AuthRef {
    AuthRef::Cli { program: program.into(), login: None, retired: Some(note.into()) }
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
        // Antigravity is Google's, and wears its mark — but not its glyph: two rails drawn
        // identically in one picker is a rail you have to read the label of.
        "antigravity" => (None, "\u{25c7}", "A", "Brand.Google"),
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
        instance("claude-cli", "claude-cli", "Claude", None, cli("claude", "claude login")),
        instance("codex-cli", "codex-cli", "Codex", None, cli("codex", "codex login")),
        // Antigravity carries its own Google login, including the access a Google AI
        // subscription comes with — so it belongs with the plans, not with the API keys.
        instance("antigravity", "antigravity", "Antigravity", None, cli("agy", "agy")),
        // Everything below speaks the Agent Client Protocol, so they are one driver pointed at
        // three programs. A fourth is a line here, not a file.
        instance("cursor-cli", "cursor-cli", "Cursor", None, cli("cursor-agent", "cursor-agent login")),
        instance("grok-cli", "grok-cli", "Grok", None, cli("grok", "grok login")),
        // Google stopped serving Gemini CLI for individual accounts on 18 June 2026 — Code Assist
        // for individuals, AI Pro and AI Ultra all at once — and withdrew "Login with Google"
        // along with them; Code Assist Standard and Enterprise are unchanged, and everyone else is
        // pointed at Antigravity. Which is why this is a note and not a deletion: the `gemini` on
        // an Enterprise machine still works, and telling that user their CLI is missing would be
        // the second wrong answer in a row. Not to be confused with the earlier crackdown on
        // third-party software *borrowing* Gemini CLI's OAuth credentials — that never described
        // us, because what is spawned here is Google's own binary doing its own login.
        instance("gemini-cli", "gemini-cli", "Gemini", None, retired_cli(
            "gemini",
            // Front-loaded on purpose. Only the row under the cursor wraps its detail and this row
            // is never under the cursor, so what a picker shows of this is the first thirty-odd
            // columns of it — which have to be a whole clause rather than the first half of one.
            // `^S` on the provider is where the rest of it is said.
            "Individual accounts ended 18 June 2026 — Code Assist Standard and Enterprise still \
             work. Use a Gemini API key instead.",
        )),
        // --- API keys: billed per token --------------------------------
        instance("anthropic", "anthropic", "Anthropic", Some("https://api.anthropic.com"), env("ANTHROPIC_API_KEY")),
        instance("google", "google", "Google Gemini", Some("https://generativelanguage.googleapis.com/v1beta"), env("GEMINI_API_KEY")),
        // --- OpenAI-compatible endpoints -------------------------------
        instance("openai", "openai-compat", "OpenAI", Some("https://api.openai.com/v1"), env("OPENAI_API_KEY")),
        instance("openrouter", "openai-compat", "OpenRouter", Some("https://openrouter.ai/api/v1"), env("OPENROUTER_API_KEY")),
        instance("groq", "openai-compat", "Groq", Some("https://api.groq.com/openai/v1"), env("GROQ_API_KEY")),
        instance("deepseek", "openai-compat", "DeepSeek", Some("https://api.deepseek.com/v1"), env("DEEPSEEK_API_KEY")),
        instance("xai", "openai-compat", "xAI", Some("https://api.x.ai/v1"), env("XAI_API_KEY")),
        instance("mistral", "openai-compat", "Mistral", Some("https://api.mistral.ai/v1"), env("MISTRAL_API_KEY")),
        instance("together", "openai-compat", "Together", Some("https://api.together.xyz/v1"), env("TOGETHER_API_KEY")),
        instance("fireworks", "openai-compat", "Fireworks", Some("https://api.fireworks.ai/inference/v1"), env("FIREWORKS_API_KEY")),
        instance("cerebras", "openai-compat", "Cerebras", Some("https://api.cerebras.ai/v1"), env("CEREBRAS_API_KEY")),
        // --- local endpoints, no credentials ---------------------------
        instance("ollama", "openai-compat", "Ollama (local)", Some("http://localhost:11434/v1"), AuthRef::None),
        instance("llamacpp", "openai-compat", "llama.cpp (local)", Some("http://localhost:8080/v1"), AuthRef::None),
        instance("lmstudio", "openai-compat", "LM Studio (local)", Some("http://localhost:1234/v1"), AuthRef::None),
        instance("vllm", "openai-compat", "vLLM (local)", Some("http://localhost:8000/v1"), AuthRef::None),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::ModelTier;

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
        let seeds = manifest::models("openai");
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

    /// A field the merge does not carry is a field that comes back wrong, and this one is the
    /// difference between a row that says "run `claude update`" and a row that looks ordinary and
    /// fails the turn. The seed cannot supply it — a catalogue has never heard of the CLI you have
    /// installed — so the driver's answer is taken whole, including its absence.
    #[test]
    fn why_a_model_cannot_be_run_survives_the_merge() {
        let seeds = manifest::models("openai");
        let known = seeds[0].id.clone();
        let mut said = ModelInfo::undescribed(known.0.as_str(), known.0.as_str());
        said.unavailable = Some("needs a newer CLI".into());
        let merged = merge(vec![said], &seeds);
        assert_eq!(merged[0].unavailable.as_deref(), Some("needs a newer CLI"));
        assert_eq!(merged[0].display_name, seeds[0].display_name, "and it is still described");

        // And it goes away again when the driver stops saying it, or `claude update` would fix the
        // model and leave the sentence behind.
        let quiet = ModelInfo::undescribed(known.0.as_str(), known.0.as_str());
        assert_eq!(merge(vec![quiet], &seeds)[0].unavailable, None);
    }

    #[test]
    fn a_seeded_model_the_endpoint_does_not_serve_is_not_offered() {
        // The endpoint is the authority on what your account can reach. Keeping a seed it did not
        // return would offer a model whose first turn is a 404.
        let seeds = manifest::models("openai");
        let discovered = vec![ModelInfo::undescribed("only-this", "only-this")];
        let merged = merge(discovered, &seeds);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id.0, "only-this");
    }

    #[test]
    fn nothing_from_the_endpoint_leaves_the_seeds_standing() {
        // Offline, or no key. Falling through to an empty list would make the provider look broken
        // rather than unauthenticated.
        let seeds = manifest::models("mistral");
        assert_eq!(merge(vec![], &seeds).len(), seeds.len());
    }


    #[test]
    fn a_plan_the_vendor_withdrew_is_still_in_the_catalogue() {
        // Deleting the entry is the tempting fix and the wrong one twice over: Code Assist Standard
        // and Enterprise still reach `gemini`, and somebody whose provider silently disappeared
        // between two releases has nowhere to read why.
        let g = builtin_instances().into_iter().find(|i| i.id.0 == "gemini-cli").unwrap();
        let neosh_proto::AuthRef::Cli { retired, .. } = &g.auth else {
            panic!("gemini-cli is still a plan");
        };
        let note = retired.as_deref().expect("says why it is no longer served");
        assert!(note.contains("18 June 2026"), "a retirement without a date is a rumour");
        assert!(!g.models.is_empty(), "and still lists what it offered");

        let source = crate::credentials::credentials().source(&g);
        assert!(!source.is_usable(), "so nothing defaults into it");
        assert_eq!(source.summary(&g.auth), note, "and the note is what every reader shows");
    }

    #[test]
    fn a_plan_that_is_merely_uninstalled_is_not_reported_as_retired() {
        // The two states share a heading and need opposite sentences, so the one that is somebody's
        // own machine must not start reading as the one that is Google's decision.
        for id in ["claude-cli", "codex-cli", "cursor-cli", "grok-cli"] {
            let inst = builtin_instances().into_iter().find(|i| i.id.0 == id).unwrap();
            let neosh_proto::AuthRef::Cli { retired, login, .. } = &inst.auth else {
                panic!("{id} is a plan");
            };
            assert!(retired.is_none(), "{id} is still served");
            assert!(login.is_some(), "{id} needs the command that signs you in");
        }
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

    /// The first row of these lists is two things at once: what `^P` opens on, and what a fresh
    /// install talks to. Both want the same answer — the newest thing the vendor serves — and the
    /// list read as a capability ladder instead, which put Opus 5 above the Fable 5.1 that
    /// supersedes it and made the newest model in the catalogue the second row of a list whose
    /// whole job is saying what is current.
    #[test]
    fn the_newest_model_is_the_first_row() {
        for (who, models) in [("anthropic", anthropic_models()), ("claude-cli", claude_cli_models())]
        {
            let first = models.first().expect("a catalogue with models in it");
            assert_eq!(first.id.as_ref(), "claude-fable-5-1", "{who} leads with the newest");
            assert!(!first.legacy, "{who}: a superseded model is never the top row");
        }
    }

    /// A live model buried under superseded ones is a live model that
    /// [`crate::ProviderRegistry::default_selection`] walks past a handful of dead ids to reach,
    /// and — with the fold in the picker keyed on `legacy` rather than on position — a list whose
    /// order stops meaning anything at the point the first old one appears.
    #[test]
    fn nothing_current_is_listed_below_something_superseded_it_replaced() {
        for (who, models) in [("anthropic", anthropic_models()), ("claude-cli", claude_cli_models())]
        {
            let mut seen_legacy: Option<String> = None;
            for m in &models {
                if m.legacy {
                    seen_legacy.get_or_insert_with(|| m.id.to_string());
                } else if let Some(older) = &seen_legacy {
                    // Only within a line: Haiku 4.5 is current and genuinely older than every 5.
                    let same_line = models
                        .iter()
                        .any(|o| o.legacy && o.family == m.family && o.id.as_ref() == older.as_str());
                    assert!(
                        !same_line,
                        "{who}: {} is current and sits below {older}, which it replaced",
                        m.id
                    );
                }
            }
        }
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
    fn a_cache_hit_costs_what_the_vendor_says_it_costs() {
        // Not a formula. Anthropic's pricing table carries a footnote — "cache hits and refreshes
        // on Claude Fable 5.1 and Claude Mythos 5.1 are priced at 0.025x the base input price. All
        // other models use the standard 0.1x multiplier" — and a flat 0.1 made the most expensive
        // model in the catalogue read four times its real cache cost in `^L` and in the sidebar.
        let by_id = |want: &str| {
            anthropic_models().into_iter().find(|m| m.id.as_ref() == want).expect("in the catalogue")
        };
        let fable = by_id("claude-fable-5-1").pricing.expect("priced");
        assert_eq!(fable.input_per_mtok, 10.0);
        assert_eq!(fable.cache_read_per_mtok, 0.25, "Fable 5.1 reads from cache at 0.025x");

        // The rule the exception is an exception to, checked on both sides so a future edit cannot
        // quietly apply the discount everywhere.
        let opus = by_id("claude-opus-5").pricing.expect("priced");
        assert_eq!(opus.cache_read_per_mtok, 0.50, "Opus 5 is still the standard 0.1x");
        let fable_5 = by_id("claude-fable-5").pricing.expect("priced");
        assert_eq!(fable_5.cache_read_per_mtok, 1.0, "Fable 5 did not get 5.1's cache price");
    }

    #[test]
    fn every_instance_says_what_you_are_spending() {
        // The split the flat list could not make. A plan and a key are different things to buy,
        // bill and revoke, and a picker that shows them as one set cannot tell you which one the
        // next turn spends.
        for inst in builtin_instances() {
            let kind = inst.auth.account_kind();
            match inst.id.as_ref() {
                "claude-cli" | "codex-cli" | "cursor-cli" | "grok-cli" | "gemini-cli"
                | "antigravity" => {
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

    /// A generation nobody has written a row for yet still arrives with its ladder, because the
    /// rule for its prefix is in the catalogue — and a model somebody *did* describe keeps what
    /// they said.
    #[test]
    fn a_discovered_model_gets_its_generations_knobs_from_the_rules() {
        let found = with_rules("openai", vec![ModelInfo::undescribed("gpt-6-nova", "GPT-6 Nova")]);
        let ids: Vec<&str> = found[0].capabilities.option_descriptors.iter().map(|d| d.id()).collect();
        assert_eq!(ids, vec!["effort"], "the gpt-6 rule, not the gpt-5 one");
        assert!(found[0].capabilities.thinking);

        let unknown = with_rules("openai", vec![ModelInfo::undescribed("o-something", "?")]);
        assert!(unknown[0].capabilities.option_descriptors.is_empty(), "no rule, no invented knob");

        let pro = with_rules("google", vec![ModelInfo::undescribed("gemini-2.5-pro-002", "?")]);
        assert!(pro[0].capabilities.option_descriptors.is_empty(), "2.5 Pro rejects the switch");
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

    /// GPT-6 is the first generation here to *lose* a rung as well as gain one, so it cannot inherit
    /// the GPT-5 set by prefix — and the two knobs it would inherit are two parameters the endpoint
    /// documents no support for, which is a 400 after the question has been typed rather than a
    /// dial that reads slightly wrong. `reasoning.effort supports low, medium, high, xhigh, and
    /// max`, and nothing on the page mentions `verbosity`.
    #[test]
    fn gpt_6_takes_max_and_neither_minimal_nor_verbosity() {
        let astra = manifest::models("openai")
            .into_iter()
            .find(|m| m.id.0 == "gpt-6-astra")
            .expect("Astra is seeded so it is visible before a key is present");

        let ids: Vec<&str> =
            astra.capabilities.option_descriptors.iter().map(|d| d.id()).collect();
        assert_eq!(ids, vec!["effort"], "the only knob it advertises");

        let ProviderOptionDescriptor::Select { options, .. } =
            &astra.capabilities.option_descriptors[0]
        else {
            panic!("expected an effort select");
        };
        let levels: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(levels, vec!["low", "medium", "high", "xhigh", "max"]);
        assert!(astra.capabilities.thinking, "an effort ladder is a model that reasons");

        // The generation below it keeps both, so this is a difference and not a rewrite.
        let sol = manifest::models("openai")
            .into_iter()
            .find(|m| m.id.0 == "gpt-5.6-sol")
            .expect("Sol is still current");
        let sol_ids: Vec<&str> = sol.capabilities.option_descriptors.iter().map(|d| d.id()).collect();
        assert_eq!(sol_ids, vec!["effort", "verbosity"]);
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
        let openai = find(&manifest::models("openai"), "gpt-5.6-sol");
        let effort = knobs(&openai).into_iter().find(|(id, _)| id == "effort").expect("a ladder");
        assert!(effort.1.contains(&"minimal".to_string()), "OpenAI's starts below low");
        assert!(!effort.1.contains(&"max".to_string()), "and does not borrow Anthropic's top rung");
        assert!(knobs(&openai).iter().any(|(id, _)| id == "verbosity"));
        assert!(openai.capabilities.thinking, "a model with a ladder reasons, by definition");

        // Gemini changed the shape of the question between generations, so the two are different
        // knobs rather than one knob with a translation layer nobody can see.
        let three = find(&manifest::models("google"), "gemini-3.1-pro-preview");
        assert_eq!(knobs(&three)[0].0, "thinking_level");
        let flash = find(&manifest::models("google"), "gemini-2.5-flash");
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
