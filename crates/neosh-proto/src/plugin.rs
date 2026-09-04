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
use crate::api::{ApiCall, ApiResponse, KeyContext, VarScope};
use crate::ascp::{NodeId, StreamEvent};
use crate::ids::{BufferId, RequestId, SessionId, StreamId, TurnId, ViewId, WindowId};
use crate::options::OptionValue;
use crate::provider::{Activity, ModelSelection, TurnRequest};

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
    ///
    /// `view` is which terminal it is about, when the plugin means a particular one — and it means
    /// a particular one whenever the answer would differ between screens, which is most of the
    /// interesting calls: what conversation is on screen, what is in the composer, where to put a
    /// window. On the envelope rather than in the call because it is the same fact for all of
    /// them, exactly as "which terminal was that key pressed in" is: a call comes *from*
    /// somewhere.
    ///
    /// `None` is "wherever this ought to go", which the host works out — from the window a call
    /// names, from the buffer only one terminal is showing, and otherwise from the terminal being
    /// served. That is what makes a plugin written before views existed still land in the right
    /// place.
    Call {
        id: RequestId,
        call: ApiCall,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<ViewId>,
    },
    /// Fire-and-forget. Used for mutations on the streaming path so a plugin appending tokens is
    /// not round-trip bound. Ordering is still guaranteed: calls from one plugin apply in the
    /// order issued.
    Notify {
        call: ApiCall,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<ViewId>,
    },
    /// Answering a [`PluginInbound::Request`].
    Response { id: RequestId, response: PluginResponse },
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum PluginResponse {
    Tool { result: ToolResult },
    Hook { outcome: HookOutcome },
    /// What a command handler returned, answering a [`PluginRequest::Command`].
    Command {
        #[ts(type = "unknown")]
        value: serde_json::Value,
    },
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
    /// Run a command this plugin registered and answer with what it returned. The request form of
    /// [`PluginEvent::CommandInvoked`], sent for [`ApiCall::CmdCall`] rather than a key press.
    Command {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
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
    /// A terminal arrived, with a screen of its own.
    ///
    /// Two things need it. A plugin that owns a **dock** has to open one panel per view — a
    /// sidebar that exists once exists in one terminal — and this is when. And a plugin that draws
    /// a [`crate::SurfaceId`] has to paint it again: the core forwards a surface's cells and does
    /// not keep a copy, so the claim can be re-announced to a new client and the contents cannot.
    /// Anything drawn into a buffer is republished without the plugin's help.
    ///
    /// Sent for a terminal reattaching to a workspace that was already running, too — it is given
    /// the screen the last one left, and the id it is given is new.
    ViewAttached { view: ViewId },
    /// A terminal went away and its screen with it.
    ///
    /// Its windows are already closed; what a plugin has to do is let go of whatever it was
    /// keeping about that panel, or it holds a window id that names nothing for as long as the
    /// workspace runs.
    ViewClosed { view: ViewId },
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
    /// A terminal is looking at a different conversation — switched, created, or the previous one
    /// closed.
    ///
    /// Broadcast, because a thread list, a status line and a usage meter all need to know, and none
    /// of them is the one that caused it.
    ///
    /// **Which terminal**, because "the active conversation" is not one thing any more: a
    /// workspace can have several and each is somewhere. A plugin that has to put something in
    /// front of the person reading a particular conversation — a question waiting to be answered —
    /// needs to know which screen that is.
    SessionChanged {
        session: SessionId,
        view: ViewId,
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
    /// What a driver's own loop said about itself — a sub-agent, a plan, a compaction, how full
    /// its context is. See [`crate::Activity`].
    ///
    /// Broadcast for the same reason the turn events are: a status line, a plan view and a usage
    /// meter all want it and none of them caused it. It is also the only signal that moves *during*
    /// a turn — everything else a plugin can hear about usage arrives when the turn ends, which for
    /// an agent driver can be twenty minutes after the number changed.
    Activity {
        session: SessionId,
        turn: TurnId,
        activity: Activity,
    },
    /// What is in the composer now, after a keystroke, a paste, or a conversation switch.
    ///
    /// Every change, including the empty one that follows sending. Completion is the reason it
    /// exists: a `/` menu, a path menu, an `@file` menu are all the same shape — watch the draft,
    /// offer something, put the answer back with [`crate::ApiCall::ChatSetDraft`] — and none of
    /// them can be written without knowing what has been typed. Frequent, but no more so than
    /// [`Self::Token`], which is already sent per chunk of a streaming answer.
    ComposerChanged {
        text: String,
    },
    /// Something a plugin said happened. See [`crate::ApiCall::EventEmit`].
    ///
    /// Broadcast to everyone, including the emitter — filtering by name is the listener's job and
    /// doing it here would mean the core keeping a subscription table it can only ever guess is
    /// current. `from` is stamped by the host, so "who said this" is not something a plugin can lie
    /// about.
    Event {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(type = "unknown")]
        data: Option<serde_json::Value>,
        from: String,
    },
    /// A shared variable changed, whoever changed it. `value` is absent when it was deleted.
    ///
    /// Broadcast for the reason every var exists: the plugin that writes `favorite` on a project is
    /// hardly ever the only one drawing it.
    VarChanged {
        scope: VarScope,
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(type = "unknown")]
        value: Option<serde_json::Value>,
    },
    /// A project's directory is now somewhere else — a worktree that was moved.
    ///
    /// Said rather than derived, because a directory is an *identity* here and not a coordinate:
    /// project vars are keyed by it, a panel's list is a list of them, and a plugin that had to
    /// work out "the thing at `/a` is the thing now at `/b`" from a conversation whose `cwd`
    /// changed would be guessing at exactly the moment it must not. The host has already moved
    /// everything it owns — the conversations, the project vars, the facts every list draws — so
    /// what this is for is the lists somebody else keeps.
    ///
    /// A re-key and not a remove-and-add, which is why one event carries both ends: told
    /// separately, a panel would drop the row and put a stranger back, losing its place in an
    /// order you arranged by hand.
    ProjectMoved {
        from: String,
        to: String,
    },
    /// Highlight groups were defined, reset, or replaced by a theme switch.
    ///
    /// Broadcast, because the plugin that cached a colour — a panel that computed a blend, a
    /// status segment that read `Normal` — is never the one that changed it. `names` is every
    /// group that moved; a theme switch lists them all.
    HighlightChanged {
        names: Vec<String>,
    },
    /// Somebody added to or withdrew from a contribution point.
    ///
    /// The signal a panel redraws on. Without it, a plugin that loads after the sidebar has drawn
    /// contributes rows nobody will look at until the next unrelated refresh — which on a quiet
    /// workspace is four seconds of a panel that is missing half of itself.
    ContributionsChanged {
        point: String,
    },
    /// A machine in the swarm came up, went away, or changed what it is running.
    ///
    /// One event for all three because a board redraws the same way for each, and a plugin that
    /// wanted to distinguish them can look at what it was given.
    SwarmChanged,
    /// Something happened in a remote conversation this node subscribed to.
    ///
    /// Only for sessions asked for with [`crate::ApiCall::SwarmSubscribe`] — a workspace that
    /// received every token from every machine would be one you could hear from the fan.
    SwarmStream {
        node: NodeId,
        session: SessionId,
        event: StreamEvent,
    },
    /// The account's allowance changed, or was asked about and answered.
    ///
    /// Sent to every plugin, whatever caused it: a driver reporting mid-turn, a poll landing, a
    /// plugin publishing its own provider's. A gauge then has one thing to listen to instead of
    /// knowing which vendors exist — which is what makes a third-party provider's plan appear in
    /// the sidebar strip without the sidebar being changed.
    Quota {
        snapshot: crate::quota::QuotaSnapshot,
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
    /// Raise a notification the user sees outside the terminal.
    ///
    /// Drawing in the corner is free, like every other kind of drawing — `neosh.notify` needs
    /// nothing. This is the other thing: a plugin that can put a notification on somebody's
    /// desktop while they are in another application is a plugin that can interrupt them, and that
    /// is exactly the kind of capability the manifest exists to make you agree to in advance.
    Notify,
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
    /// Plugins this one cannot run without.
    ///
    /// Each is loaded and activated first, `import ... from "plugin:<name>"` resolves to its entry
    /// module, and a name nothing provides is an error with the name in it rather than an
    /// `undefined` three calls later. A cycle is reported and neither side loads. Load order used
    /// to be a filesystem sort that the code leaned on; this is the same order, said out loud.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Plugins to load after if they are present, without needing them.
    ///
    /// For "my section goes under theirs" and "my default key should lose to theirs": a soft
    /// ordering, satisfied by an absent plugin, that does not make the other plugin a dependency.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// What this plugin offers for others to build on — the contribution points it reads, the
    /// buffer kinds it publishes, the shared vars it writes. Declared so they can be listed, and
    /// so a contribution to a point nobody reads can be reported instead of silently ignored.
    #[serde(default, skip_serializing_if = "PluginProvides::is_empty")]
    pub provides: PluginProvides,
    /// When to load, for a plugin that need not be there from the start.
    ///
    /// Absent means at startup, as every plugin always has. Present, the plugin is held until one
    /// of its triggers fires: one of `on_command` is run (the name is registered on its behalf
    /// and the press replayed once it is up), one of `on_event` is emitted, or a buffer of one of
    /// `on_kind` appears. lazy.nvim's `cmd`/`event`/`ft`. A plugin something eager `requires` is
    /// loaded eagerly whatever this says.
    #[serde(default, skip_serializing_if = "PluginActivation::is_empty")]
    pub activation: PluginActivation,
}

/// The triggers that wake a lazily loaded plugin. See [`PluginManifest::activation`].
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct PluginActivation {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_event: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_kind: Vec<String>,
}

impl PluginActivation {
    pub fn is_empty(&self) -> bool {
        self.on_command.is_empty() && self.on_event.is_empty() && self.on_kind.is_empty()
    }
}

/// The names a plugin publishes for other plugins to use. See [`PluginManifest::provides`].
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct PluginProvides {
    /// Contribution points this plugin reads (`sidebar.section`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub points: Vec<String>,
    /// Buffer kinds it creates (`neosh.sidebar`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    /// Shared vars it writes (`sidebar.favorite`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vars: Vec<String>,
}

impl PluginProvides {
    pub fn is_empty(&self) -> bool {
        self.points.is_empty() && self.kinds.is_empty() && self.vars.is_empty()
    }
}

fn default_api_version() -> u32 {
    crate::PROTOCOL_VERSION
}
