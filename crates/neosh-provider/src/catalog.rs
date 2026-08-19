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
    AuthRef, InstanceConfig, ModelCapabilities, ModelId, ModelInfo, OptionChoice, OptionSelection,
    ProviderOptionValue, Pricing, ProviderOptionDescriptor,
};

/// Reasoning-effort levels available on a given model generation.
fn effort(levels: &[&str], default: &str) -> ProviderOptionDescriptor {
    ProviderOptionDescriptor::Select {
        id: "effort".into(),
        label: "Effort".into(),
        description: Some("How much reasoning the model spends before answering.".into()),
        options: levels
            .iter()
            .map(|l| OptionChoice {
                id: (*l).into(),
                label: title_case(l),
                description: None,
                is_default: *l == default,
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
    }
}

fn title_case(s: &str) -> String {
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

fn claude(
    id: &str,
    name: &str,
    ctx: u64,
    max_out: u64,
    price: (f64, f64),
    efforts: Option<&[&str]>,
    fast_mode: bool,
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
    }
}

/// Anthropic models, with the option descriptors each generation actually accepts.
///
/// The effort/thinking split is load-bearing: `budget_tokens` is rejected outright on the 5 and
/// 4.7+ generations, and `effort` gained `xhigh` there, so a single flattened option set would
/// produce 400s on half the catalog.
pub fn anthropic_models() -> Vec<ModelInfo> {
    vec![
        claude("claude-opus-5", "Claude Opus 5", 1_000_000, 128_000, (5.0, 25.0), Some(EFFORT_5), true),
        claude("claude-fable-5", "Claude Fable 5", 1_000_000, 128_000, (10.0, 50.0), Some(EFFORT_5), false),
        claude("claude-sonnet-5", "Claude Sonnet 5", 1_000_000, 128_000, (3.0, 15.0), Some(EFFORT_5), false),
        claude("claude-opus-4-8", "Claude Opus 4.8", 1_000_000, 128_000, (5.0, 25.0), Some(EFFORT_5), true),
        claude("claude-opus-4-7", "Claude Opus 4.7", 1_000_000, 128_000, (5.0, 25.0), Some(EFFORT_5), false),
        claude("claude-opus-4-6", "Claude Opus 4.6", 1_000_000, 128_000, (5.0, 25.0), Some(EFFORT_46), false),
        claude("claude-sonnet-4-6", "Claude Sonnet 4.6", 1_000_000, 128_000, (3.0, 15.0), Some(EFFORT_46), false),
        claude("claude-haiku-4-5", "Claude Haiku 4.5", 200_000, 64_000, (1.0, 5.0), None, false),
    ]
}

/// Models reachable through the `claude` CLI.
///
/// The CLI accepts aliases and resolves them itself, so this is a short list of aliases rather than
/// pinned ids — it stays correct as the CLI updates.
pub fn claude_cli_models() -> Vec<ModelInfo> {
    ["opus", "sonnet", "haiku"]
        .iter()
        .map(|alias| ModelInfo {
            id: ModelId::from(*alias),
            display_name: format!("Claude {} (via CLI)", title_case(alias)),
            context_window: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities {
                tools: true,
                vision: true,
                streaming: true,
                thinking: true,
                prompt_caching: true,
                option_descriptors: vec![effort(EFFORT_5, "high")],
            },
            pricing: None,
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
        auth,
        models,
        extra_headers: Vec::new(),
    }
}

fn env(var: &str) -> AuthRef {
    AuthRef::Env { var: var.into() }
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
        instance("claude-cli", "claude-cli", "Claude (CLI login)", None, AuthRef::Inherited, claude_cli_models()),
        instance("anthropic", "anthropic", "Anthropic", Some("https://api.anthropic.com"), env("ANTHROPIC_API_KEY"), anthropic_models()),
        instance("google", "google", "Google Gemini", Some("https://generativelanguage.googleapis.com/v1beta"), env("GEMINI_API_KEY"), vec![]),
        // --- OpenAI-compatible endpoints -------------------------------
        instance("openai", "openai-compat", "OpenAI", Some("https://api.openai.com/v1"), env("OPENAI_API_KEY"), vec![]),
        instance("openrouter", "openai-compat", "OpenRouter", Some("https://openrouter.ai/api/v1"), env("OPENROUTER_API_KEY"), vec![]),
        instance("groq", "openai-compat", "Groq", Some("https://api.groq.com/openai/v1"), env("GROQ_API_KEY"), vec![]),
        instance("deepseek", "openai-compat", "DeepSeek", Some("https://api.deepseek.com/v1"), env("DEEPSEEK_API_KEY"), vec![]),
        instance("xai", "openai-compat", "xAI", Some("https://api.x.ai/v1"), env("XAI_API_KEY"), vec![]),
        instance("mistral", "openai-compat", "Mistral", Some("https://api.mistral.ai/v1"), env("MISTRAL_API_KEY"), vec![]),
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
        assert_eq!(all[0].auth, AuthRef::Inherited, "must not require an API key");
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
