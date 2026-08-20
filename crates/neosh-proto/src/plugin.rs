//! The plugin transport envelope, and the plugin manifest.
//!
//! The channel is *bidirectional*, and that is not incidental. A plugin calls into the host
//! ([`PluginOutbound::Call`]) but the host also calls into the plugin
//! ([`PluginInbound::Request`]) — to run a tool it registered, to ask a hook whether a tool call
//! may proceed, or to drive a provider it implements. Anything less than a full duplex RPC and
//! "veto a tool call" or "implement a model provider" would need a private back door.
//!
//! The in-process TypeScript runtime and a future out-of-process JSON-RPC plugin exchange exactly
//! these messages. MCP servers are a specialization of the same door: a `RunTool` request over
//! stdio with a different framing.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::agent::{HookName, HookOutcome, HookPayload, StopReason, ToolCall, ToolResult, Usage};
use crate::api::{ApiCall, ApiResponse, KeyContext};
use crate::ids::{BufferId, RequestId, SessionId, StreamId, TurnId, WindowId};
use crate::options::OptionValue;
use crate::provider::{ModelSelection, TurnRequest};

// ---------------------------------------------------------------------------
// plugin -> host
// ---------------------------------------------------------------------------

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum PluginOutbound {
    /// Sent once at startup. The host refuses to proceed on a protocol major mismatch rather than
    /// letting a stale plugin half-work.
    Ready { protocol_version: u32 },
    /// A call that wants an answer.
    Call { id: RequestId, call: ApiCall },
    /// Fire-and-forget. Used for mutations on the streaming path so a plugin appending tokens is
    /// not round-trip bound. Ordering is still guaranteed: calls from one plugin apply in the
    /// order issued.
    Notify { call: ApiCall },
    /// Answering a [`PluginInbound::Request`].
    Response { id: RequestId, response: PluginResponse },
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum PluginResponse {
    Tool { result: ToolResult },
    Hook { outcome: HookOutcome },
    /// The plugin accepted a provider stream and will push
    /// [`ApiCall::ProviderEmit`](crate::api::ApiCall::ProviderEmit) until `MessageStop`.
    ProviderAccepted,
    Error { message: String },
}

// ---------------------------------------------------------------------------
// host -> plugin
// ---------------------------------------------------------------------------

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum PluginInbound {
    /// Answering a [`PluginOutbound::Call`].
    Response { id: RequestId, response: ApiResponse },
    Request { id: RequestId, request: PluginRequest },
    Event { event: PluginEvent },
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum PluginRequest {
    /// Run a tool this plugin registered.
    RunTool {
        name: String,
        #[ts(type = "unknown")]
        input: serde_json::Value,
    },
    /// Ask a hook. Only sent for hooks registered as blocking; observers get
    /// [`PluginEvent::HookObserved`] instead and cannot influence the outcome.
    Hook { hook: HookName, payload: HookPayload },
    /// Drive a provider this plugin registered. The plugin answers
    /// [`PluginResponse::ProviderAccepted`] immediately and then pushes events tagged with
    /// `stream`, because a stream cannot be returned across the RPC boundary.
    ProviderStream { stream: StreamId, request: TurnRequest },
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum PluginEvent {
    /// A command this plugin registered was invoked, by name, key, or another plugin.
    CommandInvoked {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<KeyContext>,
    },
    /// A buffer the plugin attached to changed. Mirrors the splice the core performed.
    BufferChanged {
        buf: BufferId,
        start: u32,
        old_end: u32,
        new_end: u32,
    },
    /// A turn has begun.
    ///
    /// Every turn event says which conversation it belongs to. A workspace runs several at once,
    /// so a plugin that assumed "the one on screen" would attribute one conversation's answer to
    /// another — and by the time a turn ends, the conversation it ran in may not be on screen at
    /// all.
    TurnStarted {
        session: SessionId,
        turn: TurnId,
    },
    /// One streamed chunk of assistant text. Chunks are provider-sized, not character-sized.
    Token {
        session: SessionId,
        turn: TurnId,
        text: String,
    },
    /// One streamed chunk of reasoning, where the provider exposes it.
    ThinkingToken {
        session: SessionId,
        turn: TurnId,
        text: String,
    },
    /// A tool is about to run. Distinct from the `tool_pre` hook: that one is asked *whether* the
    /// call may proceed and can veto it, this one is told that it is happening. A transcript wants
    /// the telling, not the asking.
    ToolStarted {
        session: SessionId,
        turn: TurnId,
        call: ToolCall,
    },
    ToolFinished {
        session: SessionId,
        turn: TurnId,
        call: ToolCall,
        result: ToolResult,
    },
    TurnEnded {
        session: SessionId,
        turn: TurnId,
        stop_reason: StopReason,
        usage: Usage,
    },
    /// Fired for non-blocking hook registrations. Observe-only by construction.
    HookObserved {
        hook: HookName,
        payload: HookPayload,
    },
    FocusChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        win: Option<WindowId>,
    },
    /// A declared option changed, whoever changed it. Broadcast, because an option is a shared
    /// setting: the plugin that owns `ui.theme` is rarely the one that sets it.
    OptionChanged {
        name: String,
        value: OptionValue,
    },
    /// The active conversation changed — switched, created, or the previous one closed.
    ///
    /// Broadcast, because a thread list, a status line and a usage meter all need to know, and none
    /// of them is the one that caused it.
    SessionChanged {
        session: SessionId,
    },
    /// The model this conversation will use changed, whoever changed it.
    ///
    /// Broadcast for the same reason [`Self::SessionChanged`] is, and it was missing for exactly
    /// as long as only one plugin cared. It is not the same thing as `agent.model` changing:
    /// that option is a *preference*, and the selection also moves when a conversation is restored,
    /// when a stored model turns out not to authenticate, and when a provider registers late and
    /// the model somebody asked for finally becomes reachable. Every one of those leaves a footer
    /// naming the wrong model and a context meter measuring against the wrong window.
    SelectionChanged {
        selection: ModelSelection,
    },
    /// Stop producing events for this stream and release its resources.
    ProviderCancel {
        stream: StreamId,
    },
    /// Last message a plugin receives. `deactivate` runs, then the runtime is torn down.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// What a plugin declares it needs.
///
/// Phase 0 records and reports these; enforcement lands with the sandboxing work. Declaring them
/// now means the manifest format does not change when enforcement arrives.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum PluginPermission {
    /// Register tools the agent can call.
    Tools,
    /// Register blocking hooks, which can veto.
    HooksBlocking,
    /// Register a model provider.
    Providers,
    /// Read files through the host.
    FsRead,
    /// Write files through the host.
    FsWrite,
    /// Make network requests through the host.
    Network,
    /// Claim raw cell surfaces.
    RawCells,
    /// Change the repository: branch, checkout, stage, commit, worktree.
    ///
    /// Reading git state needs nothing — it is no more sensitive than reading a buffer. Writing is
    /// declared, so `neosh plugins` can answer "what can this thing do to my repository" from the
    /// manifest rather than from reading its source.
    ///
    /// This is a *plugin* permission, deliberately not the agent's permission mode. Those govern
    /// what the **model** may do, and the model does not reach git through this call — it reaches
    /// it through a registered tool, which is gated like every other tool. Making a user pressing
    /// Enter in a branch picker ask the agent's policy for permission would be a category error.
    VcsWrite,
}

/// `plugin.toml`, next to the plugin's entry point.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    /// Entry module, relative to the plugin directory.
    pub entry: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Protocol major this plugin was built against. Checked at load.
    #[serde(default = "default_api_version")]
    pub api_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<PluginPermission>,
}

fn default_api_version() -> u32 {
    crate::PROTOCOL_VERSION
}
