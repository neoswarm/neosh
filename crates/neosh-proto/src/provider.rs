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
    /// The driver is a subprocess that carries its own credentials (e.g. an already-logged-in CLI).
    Inherited,
    /// No auth, e.g. a local llama.cpp or Ollama endpoint.
    None,
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
            Self::Inherited | Self::None => true,
        }
    }
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
