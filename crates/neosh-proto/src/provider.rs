//! Model and provider wire types.
//!
//! # Drivers are open, not enumerated
//!
//! [`DriverKind`] is a free-form slug, not a closed enum. A driver is *registered* at runtime — by
//! the host for the built-ins, or by a plugin via [`crate::api::ApiCall::ProviderRegisterDriver`].
//! Adding a model vendor therefore never requires a core change, which is the whole point.
//!
//! # Instances vs drivers
//!
//! A [`DriverKind`] is an implementation (`openai-compat`, `anthropic`, `claude-cli`). An
//! [`InstanceId`] is a *configured* one: base URL, credentials, model list. That split is what
//! makes "OpenRouter and Groq and a local llama.cpp, all at once" a configuration question rather
//! than a code question — they are three instances of one driver.
//!
//! # Per-model options are declared by the driver
//!
//! Reasoning effort, thinking budgets, fast mode: every vendor spells these differently and the set
//! changes per model. Rather than hardcode a union of all of them, a driver advertises
//! [`ProviderOptionDescriptor`]s per model and the UI renders them generically. A new vendor knob
//! shows up in the picker with no UI change.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::agent::{Message, StopReason, ToolDef, Usage};
use crate::ids::{StreamId, ToolCallId};

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

macro_rules! slug {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        #[serde(transparent)]
        #[ts(export)]
        pub struct $name(pub String);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl From<&str> for $name {
            fn from(v: &str) -> Self { Self(v.to_string()) }
        }
        impl From<String> for $name {
            fn from(v: String) -> Self { Self(v) }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str { &self.0 }
        }
    };
}

slug!(
    /// Names a driver implementation, e.g. `anthropic`, `openai-compat`, `claude-cli`.
    ///
    /// Intentionally open. See the module docs.
    DriverKind
);
slug!(
    /// Names a *configured* driver: credentials, base URL, model list.
    InstanceId
);
slug!(
    /// A model identifier as the provider spells it, e.g. `claude-opus-5`, `gpt-5`, `llama3.3:70b`.
    ModelId
);

// ---------------------------------------------------------------------------
// Per-model options
// ---------------------------------------------------------------------------

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct OptionChoice {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub is_default: bool,
}

/// One knob a model exposes.
///
/// The UI renders these without knowing what any of them mean, so a driver can add
/// `{"id":"fast_mode"}` and have it appear in the model picker with no host change.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ProviderOptionDescriptor {
    Select {
        id: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        options: Vec<OptionChoice>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_value: Option<String>,
        /// Values that are applied by prefixing the *prompt* rather than by sending a request
        /// parameter. Some vendors gate reasoning depth on magic words in the user message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        prompt_injected_values: Vec<String>,
    },
    Boolean {
        id: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default)]
        current_value: bool,
    },
}

impl ProviderOptionDescriptor {
    pub fn id(&self) -> &str {
        match self {
            Self::Select { id, .. } | Self::Boolean { id, .. } => id,
        }
    }
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(untagged)]
#[ts(export)]
pub enum ProviderOptionValue {
    Text(String),
    Flag(bool),
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct OptionSelection {
    pub id: String,
    pub value: ProviderOptionValue,
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct ModelCapabilities {
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub thinking: bool,
    #[serde(default)]
    pub prompt_caching: bool,
    /// Knobs this specific model exposes. See [`ProviderOptionDescriptor`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub option_descriptors: Vec<ProviderOptionDescriptor>,
}

/// Cost per million tokens, for budget display. `None` where unknown (local models, unlisted
/// endpoints) — the UI must render "unknown" rather than guessing zero.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[ts(export)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    #[serde(default)]
    pub cache_read_per_mtok: f64,
    #[serde(default)]
    pub cache_write_per_mtok: f64,
}

/// Where a model sits on the capability ladder.
///
/// Three rungs, because every lineup worth switching between has three: Opus/Sonnet/Haiku,
/// gpt-5/mini/nano, Pro/Flash/Flash-Lite. This is what makes "give me the next one down" a question
/// with an answer, across catalogues that share no naming convention at all.
///
/// It is a *statement about the model*, supplied by whoever describes it — a catalogue entry, a
/// discovery response, a provider plugin. Nothing infers it from an id, because id conventions are
/// exactly the thing that differs.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ModelTier {
    /// Cheapest and quickest. For things you would not think hard about yourself.
    Fast,
    /// The everyday one.
    Balanced,
    /// The most capable thing this provider sells.
    Frontier,
}

impl ModelTier {
    /// Low to high, so a ladder can be walked.
    pub const LADDER: [ModelTier; 3] = [Self::Fast, Self::Balanced, Self::Frontier];

    pub fn rank(self) -> u8 {
        match self {
            Self::Fast => 0,
            Self::Balanced => 1,
            Self::Frontier => 2,
        }
    }

    /// One rung along, or `None` at the end. No wrapping: a "downgrade" that lands on the most
    /// expensive model in the catalogue is a keystroke you learn not to press.
    pub fn step(self, delta: i8) -> Option<Self> {
        let at = self.rank() as i8 + delta;
        (0..3).contains(&at).then(|| Self::LADDER[at as usize])
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Balanced => "Balanced",
            Self::Frontier => "Frontier",
        }
    }
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct ModelInfo {
    pub id: ModelId,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null")]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,
    /// The lineup: `"opus"`, `"sonnet"`, `"gpt-5"`. Models sharing a family are the same product at
    /// different versions, which is what "the best one in that line" means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<ModelTier>,
    /// Superseded, but still reachable. Folded away in a picker rather than removed, because the
    /// reason you go looking for last year's model is usually that you need to reproduce something.
    #[serde(default)]
    pub legacy: bool,
    /// One line on what it is for. `"Most capable for complex work"`, not `"128k context"` — the
    /// numbers are already columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
}

impl ModelInfo {
    /// The blanks an endpoint that told us only a name leaves behind.
    ///
    /// Deliberately not `Default`: a `ModelInfo` with no id is not a thing, and the family, tier
    /// and tagline are exactly the fields a discovery response cannot supply — they are editorial,
    /// and inferring them from an id would be guessing in the one place a wrong guess sends a turn
    /// to a model you did not ask for.
    pub fn undescribed(id: impl Into<ModelId>, display_name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            context_window: None,
            max_output_tokens: None,
            capabilities: ModelCapabilities::default(),
            pricing: None,
            family: None,
            tier: None,
            legacy: false,
            tagline: None,
        }
    }
}

/// A model together with the instance that serves it.
///
/// The pairing is not decoration. A model id is unique per instance, not globally — two instances
/// can both serve `gpt-4o`, and one of them may be a local endpoint. A picker that returned bare
/// [`ModelInfo`] would have to guess the owner, and guessing wrong sends the conversation to the
/// wrong endpoint with no visible error.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct ModelEntry {
    pub instance: InstanceId,
    pub model: ModelInfo,
}

/// A model, qualified by the instance that serves it, plus the option values in effect.
///
/// Hot-swappable mid-session: the next turn uses whatever this says.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct ModelSelection {
    pub instance: InstanceId,
    pub model: ModelId,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<OptionSelection>,
}

impl ModelSelection {
    pub fn option_str(&self, id: &str) -> Option<&str> {
        self.options.iter().find(|o| o.id == id).and_then(|o| match &o.value {
            ProviderOptionValue::Text(s) => Some(s.as_str()),
            ProviderOptionValue::Flag(_) => None,
        })
    }

    pub fn option_flag(&self, id: &str) -> Option<bool> {
        self.options.iter().find(|o| o.id == id).and_then(|o| match &o.value {
            ProviderOptionValue::Flag(b) => Some(*b),
            ProviderOptionValue::Text(_) => None,
        })
    }
}

/// How a driver authenticates. Never carries a secret value — only where to find one.
///
/// Secrets are resolved at request time from the environment or an OS keychain and are structurally
/// excluded from every type that crosses a process boundary.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum AuthRef {
    /// Read the key from this environment variable.
    Env { var: String },
    /// Run a program and read the key from its standard output.
    ///
    /// The git-credential-helper pattern, and the reason neosh needs no opinion about your password
    /// manager: `["pass", "show", "anthropic/key"]`, `["op", "read", "op://…"]`, `["secret-tool",
    /// "lookup", …]` all work, and none of them puts a key in a file neosh wrote.
    Command { argv: Vec<String> },
    /// A **plan**: a vendor CLI you are already signed in to, which carries its own credentials.
    ///
    /// `program` is what must be on `$PATH`; `login` is the command that signs you in, quoted back
    /// at you when it is missing. neosh never holds a token for these — that is the point. An
    /// account subscription and an API key are different things to buy, bill and revoke, and a
    /// picker that lists them as one flat set cannot tell you which one you are about to spend.
    Cli {
        program: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        login: Option<String>,
    },
    /// The driver authenticates itself and will not say how.
    ///
    /// The door for a plan that is not a CLI — a plugin that runs its own OAuth flow registers a
    /// driver with this and neither the core nor the picker needs to learn anything new.
    Inherited,
    /// No auth, e.g. a local llama.cpp or Ollama endpoint.
    None,
}

/// What you are spending when you use an instance.
///
/// The one distinction the old flat list could not make. Everything else in a picker — ordering,
/// grouping, which sign-in verb to offer, whether to show a price — follows from this.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AccountKind {
    /// A subscription you already pay for. No key, nothing for neosh to store.
    Plan,
    /// Billed per token against a key you supply.
    ApiKey,
    /// Something on this machine. Costs nothing and needs nothing.
    Local,
}

impl AccountKind {
    /// The heading a picker files this under.
    pub fn heading(self) -> &'static str {
        match self {
            Self::Plan => "PLANS",
            Self::ApiKey => "API KEYS",
            Self::Local => "LOCAL",
        }
    }
}

impl AuthRef {
    /// Which of the three kinds of account this is.
    pub fn account_kind(&self) -> AccountKind {
        match self {
            Self::Cli { .. } | Self::Inherited => AccountKind::Plan,
            Self::Env { .. } | Self::Command { .. } => AccountKind::ApiKey,
            Self::None => AccountKind::Local,
        }
    }
}

/// How a provider is drawn: a mark, and a highlight group carrying its colour.
///
/// Three marks rather than one because a terminal is not one thing. A Nerd Font has a real brand
/// glyph; a bare Unicode terminal has geometry; `ui.ascii_only` has a letter. Choosing between them
/// is the frontend's job, like every other display decision — this type only says what the choices
/// are.
///
/// The colour is a **highlight group name**, never a colour value, so a theme can restyle every
/// provider mark without anything here changing and a 16-colour terminal still gets something.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct Brand {
    /// Nerd Font glyph, used when `ui.nerd_font` is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nerd: Option<String>,
    /// Plain Unicode mark. Works in any terminal with a modern font.
    pub mark: String,
    /// One or two ASCII characters, for `ui.ascii_only`.
    pub ascii: String,
    /// Highlight group carrying the brand colour, e.g. `Brand.Anthropic`.
    pub hl: String,
}

impl Brand {
    pub fn new(nerd: Option<&str>, mark: &str, ascii: &str, hl: &str) -> Self {
        Self {
            nerd: nerd.map(str::to_string),
            mark: mark.into(),
            ascii: ascii.into(),
            hl: hl.into(),
        }
    }
}

impl AuthRef {
    /// Whether the credential this names is actually available right now.
    ///
    /// `Inherited` is assumed available: whether a logged-in CLI exists cannot be established
    /// without running it, and treating it as unavailable would rule out the one path that needs no
    /// setup at all.
    pub fn is_available(&self) -> bool {
        match self {
            Self::Env { var } => std::env::var_os(var).is_some_and(|v| !v.is_empty()),
            // A helper is assumed to work for the same reason `Inherited` is: finding out costs a
            // subprocess, and doing that for every instance every time a picker opens would make
            // opening a picker slow in proportion to how well configured you are.
            Self::Command { argv } => !argv.is_empty(),
            Self::Cli { .. } | Self::Inherited | Self::None => true,
        }
    }
}

/// Where the key for an instance is coming from.
///
/// Reported so a settings list can say *why* something works, which is the difference between "it
/// is signed in" and "it is signed in and will still be tomorrow".
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum CredentialSource {
    /// A vendor CLI carries the login. `via` is the program.
    ///
    /// Whether you are *signed in* to it is deliberately not claimed: finding out means running it,
    /// and a picker that lied either way would be worse than one that says where to look.
    Plan { via: String },
    /// A plan whose CLI is not installed. `hint` is the command that would fix it.
    PlanMissing {
        program: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    /// This environment variable, which is set.
    Env { var: String },
    /// The OS keychain, via its command-line helper. Survives a restart.
    Keychain,
    /// Typed in this session and held in memory only. Gone when neosh exits.
    Session,
    /// A helper program named in configuration.
    Command,
    /// The driver brings its own login and will not say how.
    Inherited,
    /// The endpoint needs no key at all.
    NotNeeded,
    /// Nothing to authenticate with.
    Missing,
}

impl CredentialSource {
    /// How this reads in one short phrase, next to a provider's name.
    ///
    /// Lives here rather than in each caller because the same sentence has to appear in the model
    /// picker, the welcome block and `--list-models`, and three copies of it is three chances for
    /// one of them to still say "no API key" about an account that is on a plan — which is the
    /// exact confusion the account split exists to remove.
    pub fn summary(&self, auth: &AuthRef) -> String {
        match self {
            Self::Plan { via } => format!("your {via} login"),
            Self::PlanMissing { program, hint } => match hint {
                Some(cmd) => format!("{program} is not installed — then `{cmd}`"),
                None => format!("{program} is not installed"),
            },
            Self::Env { var } => format!("${var}"),
            Self::Keychain => "key in the keychain".into(),
            Self::Session => "key entered this run".into(),
            Self::Command => "credential helper".into(),
            Self::Inherited => "uses an existing login".into(),
            Self::NotNeeded => "no key needed".into(),
            Self::Missing => match auth {
                AuthRef::Env { var } => format!("no key — ${var} is not set"),
                _ => "no key".into(),
            },
        }
    }

    /// Whether a turn sent to this instance can authenticate.
    pub fn is_usable(&self) -> bool {
        !matches!(self, Self::Missing | Self::PlanMissing { .. })
    }

    /// Whether it will still be there after a restart.
    pub fn is_durable(&self) -> bool {
        !matches!(self, Self::Session | Self::Missing | Self::PlanMissing { .. })
    }
}

/// An instance and the state of its credential. **Never carries the secret itself.**
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct CredentialInfo {
    pub instance: InstanceId,
    pub display_name: String,
    pub driver: DriverKind,
    pub source: CredentialSource,
    /// A plan, a key, or something local. What a picker groups by.
    pub account: AccountKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brand: Option<Brand>,
    /// Whether this instance's driver is registered, so the entry is worth offering at all.
    pub driver_available: bool,
    /// Whether a key can be typed in for it. False for a CLI login or a local endpoint, where
    /// asking for one would be asking for something that does nothing.
    pub accepts_key: bool,
}

/// A configured driver.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct InstanceConfig {
    pub id: InstanceId,
    pub driver: DriverKind,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default = "auth_none")]
    pub auth: AuthRef,
    /// How to draw it. `None` falls back to a generic mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brand: Option<Brand>,
    /// Models this instance serves. Empty means "ask the endpoint at runtime".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_headers: Vec<(String, String)>,
}

fn auth_none() -> AuthRef {
    AuthRef::None
}

// ---------------------------------------------------------------------------
// Requests and streaming
// ---------------------------------------------------------------------------

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct TurnRequest {
    pub selection: ModelSelection,
    /// The directory this turn happens in.
    ///
    /// The conversation's, not the process's. A vendor CLI is an agent that reads files, runs
    /// commands and looks at git — all of it relative to where it was started — so a driver that
    /// spawns it in whatever directory neosh happened to be launched from is asking about the
    /// wrong repository. Every conversation carries a project; this is how a driver finds out
    /// which one.
    ///
    /// Empty means "wherever the host process is", which is what a driver with no opinion did
    /// before this field existed.
    #[serde(default)]
    #[ts(type = "string")]
    pub cwd: std::path::PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null")]
    pub max_output_tokens: Option<u64>,
}

/// Normalized streaming events.
///
/// These mirror the Anthropic Messages SSE shape because it is the most expressive of the wire
/// formats we target (separate thinking blocks, incremental tool-input JSON). Every other driver
/// adapts *into* this, so the agent loop has exactly one stream shape to handle.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ProviderEvent {
    MessageStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default)]
        usage: Usage,
    },
    /// A new content block opened at `index`.
    BlockStart {
        index: u32,
        block: BlockStartKind,
    },
    TextDelta {
        index: u32,
        text: String,
    },
    ThinkingDelta {
        index: u32,
        text: String,
    },
    /// Opaque thinking signature, streamed separately by some providers.
    SignatureDelta {
        index: u32,
        signature: String,
    },
    /// Incremental JSON for a tool call's arguments. Accumulate and parse once at `BlockStop` —
    /// never string-match the partial text.
    ToolInputDelta {
        index: u32,
        partial_json: String,
    },
    BlockStop {
        index: u32,
    },
    MessageDelta {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
        #[serde(default)]
        usage: Usage,
    },
    MessageStop,
    /// Transport or provider error. Terminal for the stream.
    Error {
        message: String,
        #[serde(default)]
        retryable: bool,
    },
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum BlockStartKind {
    Text,
    Thinking,
    ToolUse { id: ToolCallId, name: String },
}

/// Emitted by a *plugin-implemented* provider, pushed rather than returned.
///
/// A plugin cannot return a stream across the RPC boundary, so it answers the stream request
/// immediately and then pushes events tagged with the [`StreamId`] until it sends `MessageStop`.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct ProviderEmit {
    pub stream: StreamId,
    pub event: ProviderEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_does_not_wrap() {
        // A "downgrade" that teleports you to the most expensive model in the catalogue is a
        // keystroke you learn not to press.
        assert_eq!(ModelTier::Fast.step(-1), None);
        assert_eq!(ModelTier::Frontier.step(1), None);
        assert_eq!(ModelTier::Balanced.step(-1), Some(ModelTier::Fast));
        assert_eq!(ModelTier::Balanced.step(1), Some(ModelTier::Frontier));
    }

    #[test]
    fn a_plan_is_not_a_key_and_a_key_is_not_a_plan() {
        assert_eq!(
            AuthRef::Cli { program: "claude".into(), login: None }.account_kind(),
            AccountKind::Plan
        );
        assert_eq!(AuthRef::Inherited.account_kind(), AccountKind::Plan);
        assert_eq!(AuthRef::Env { var: "K".into() }.account_kind(), AccountKind::ApiKey);
        assert_eq!(AuthRef::Command { argv: vec![] }.account_kind(), AccountKind::ApiKey);
        assert_eq!(AuthRef::None.account_kind(), AccountKind::Local);
    }

    #[test]
    fn a_plan_source_never_claims_a_key() {
        // Nothing that describes a plan may serialise a value: there is none, and a field that
        // could hold one is a field someone will fill in.
        let json = serde_json::to_string(&CredentialSource::Plan { via: "claude".into() })
            .expect("serialises");
        assert_eq!(json, r#"{"kind":"plan","via":"claude"}"#);
    }
}
