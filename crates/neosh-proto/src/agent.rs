//! Agent-domain wire types: messages, turns, tools, permissions.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::{SessionId, ToolCallId, TurnId};

// ---------------------------------------------------------------------------
// Conversation
// ---------------------------------------------------------------------------

#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Role {
    User,
    Assistant,
}

/// A piece of a message.
///
/// `Thinking` is a first-class variant rather than something folded into `Text`, because a UI must
/// be able to render or collapse reasoning independently, and because thinking blocks have to be
/// replayed back to the provider unchanged on the next turn.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        /// Opaque provider signature. Must be echoed back verbatim or the provider rejects the turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolUse {
        id: ToolCallId,
        name: String,
        #[ts(type = "unknown")]
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: ToolCallId,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

/// Why a turn stopped.
///
/// `Refusal` is explicit because current models return HTTP 200 with a refusal stop reason rather
/// than an error — code that only checks for transport failures silently renders an empty turn.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        category: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
    },
    /// The user or a plugin interrupted the turn.
    Cancelled,
    Error {
        message: String,
    },
}

/// Token accounting, surfaced so a plugin can render a budget without scraping.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct Usage {
    #[serde(default)]
    #[ts(type = "number")]
    pub input_tokens: u64,
    #[serde(default)]
    #[ts(type = "number")]
    pub output_tokens: u64,
    #[serde(default)]
    #[ts(type = "number")]
    pub cache_read_tokens: u64,
    #[serde(default)]
    #[ts(type = "number")]
    pub cache_write_tokens: u64,
    /// Thinking tokens, where the provider reports them separately.
    #[serde(default)]
    #[ts(type = "number")]
    pub thinking_tokens: u64,
}

impl Usage {
    pub fn merge(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.max(other.input_tokens);
        self.output_tokens = self.output_tokens.max(other.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.max(other.cache_read_tokens);
        self.cache_write_tokens = self.cache_write_tokens.max(other.cache_write_tokens);
        self.thinking_tokens = self.thinking_tokens.max(other.thinking_tokens);
    }
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct SessionInfo {
    pub id: SessionId,
    pub cwd: String,
    pub message_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn: Option<TurnId>,
    /// Seconds since the epoch, stamped when the running turn began.
    ///
    /// A list that says a conversation is working should say for how long — "thinking" with no
    /// clock is indistinguishable from "wedged". Only meaningful while `active_turn` is set; the
    /// value is deliberately left behind afterwards rather than cleared, because nothing reads it
    /// then and clearing it would be one more write on the turn-end path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null")]
    pub turn_started_at: Option<i64>,
    /// Set by the user or by generation. `None` until something names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What to show in a list: the title, else the opening of the first user message. Computed by
    /// the host so every frontend and plugin agrees on the fallback.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    #[ts(type = "number")]
    pub created_at: i64,
    #[serde(default)]
    #[ts(type = "number")]
    pub updated_at: i64,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub is_active: bool,
    /// Put away rather than thrown away.
    ///
    /// An archived conversation keeps every message and can be brought back; it is simply not in
    /// the list you look at every day. It exists because the alternative verb people were reaching
    /// for was delete, and delete is the one thing in a workspace that cannot be undone.
    #[serde(default)]
    pub archived: bool,
    /// Seconds since the epoch, stamped when it was archived. `None` while it is in the open list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null")]
    pub archived_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Where a tool came from.
///
/// Built-in, plugin and MCP tools land in one namespace with one shape, so that a policy hook does
/// not have to care which kind it is intercepting.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum ToolSource {
    Builtin,
    Plugin { plugin: String },
    /// Reserved: MCP servers register here without any other change to this type.
    Mcp { server: String },
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema. Passed to the provider verbatim.
    #[ts(type = "Record<string, unknown>")]
    pub input_schema: serde_json::Value,
    pub source: ToolSource,
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct ToolResult {
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
    }
}

/// A tool call in flight, as seen by hooks.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub turn: TurnId,
    pub name: String,
    #[ts(type = "unknown")]
    pub input: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum PermissionMode {
    /// Prompt the user. The default, and the only safe one for an agent that edits files.
    #[default]
    Ask,
    /// Allow anything matching the allow-list, prompt otherwise.
    AllowListed,
    /// Refuse everything not explicitly allowed.
    Deny,
}

/// What an action wants to do. Checked before any filesystem, network or exec effect.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum Capability {
    ReadFile { path: String },
    WriteFile { path: String },
    Exec { command: String },
    Network { host: String },
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum PermissionDecision {
    Allow,
    Deny { reason: String },
    /// Needs a user answer. The host surfaces this to whatever UI is listening.
    Prompt,
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// Pre/post hook points. Every meaningful action has one.
///
/// This is how policy, audit and permission plugins are written without the core knowing they
/// exist.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum HookName {
    /// Fired before a tool executes. A blocking hook here can rewrite the input or veto the call.
    ToolPre,
    ToolPost,
    TurnPre,
    TurnPost,
    BufPreChange,
    PermissionPre,
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "hook", rename_all = "snake_case")]
#[ts(export)]
pub enum HookPayload {
    ToolPre {
        call: ToolCall,
    },
    ToolPost {
        call: ToolCall,
        result: ToolResult,
    },
    TurnPre {
        turn: TurnId,
        text: String,
    },
    TurnPost {
        turn: TurnId,
        stop_reason: StopReason,
        usage: Usage,
    },
    BufPreChange {
        buf: crate::ids::BufferId,
        start: u32,
        old_end: u32,
        new_len: u32,
    },
    PermissionPre {
        capability: Capability,
        turn: Option<TurnId>,
    },
}

/// What a hook decided.
///
/// A hook registered as non-blocking is a pure observer and its outcome is ignored — that is
/// deliberate, so an audit plugin can never wedge the agent loop. A *blocking* hook is awaited with
/// a timeout, and **a timeout is treated as [`HookOutcome::Veto`]**: a permission plugin that hangs
/// must fail closed, not open.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[serde(tag = "action", rename_all = "snake_case")]
#[ts(export)]
pub enum HookOutcome {
    /// The default, so a hook that answers with nothing usable does not accidentally veto.
    #[default]
    Continue,
    /// Replace the payload and carry on. Used to redact arguments or rewrite a path.
    Modify {
        #[ts(type = "unknown")]
        payload: serde_json::Value,
    },
    Veto {
        reason: String,
    },
}


