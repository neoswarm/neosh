//! Agent-domain wire types: messages, turns, tools, permissions.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::{SessionId, ToolCallId, TurnId};
use crate::provider::BackgroundTask;

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
    /// A picture, as somewhere to find it rather than as the picture.
    ///
    /// The bytes are not in here and that is deliberate. A screenshot is a megabyte of base64, a
    /// conversation holds as many as you paste into it, and a message is written to the session
    /// file, read back on every attach, and sent down the socket to whoever is looking — so the
    /// bytes would be copied on every one of those and the transcript on disk would be mostly
    /// pictures. They are written once into the workspace's own directory and every message that
    /// mentions them says where; a driver reads the file when it builds its request, which is the
    /// only moment anything actually needs them.
    Image {
        /// Where the bytes are, absolute. Written by the workspace, read by a driver.
        path: String,
        /// `image/png`, `image/jpeg`, `image/gif` or `image/webp` — the four every provider we
        /// target accepts. Read off the bytes rather than off the file name, which is a claim.
        media_type: String,
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
    /// What to call the directory this conversation is in.
    ///
    /// The last segment of the path is the obvious answer and is wrong for the case that matters:
    /// a worktree is named by whoever created it, and a list of projects reading `wt-fe3c0d93`
    /// tells you nothing about which repository or branch that is. A checkout is named by the
    /// repository it belongs to, and a linked worktree adds the branch — `neosh · fix/thing` —
    /// because that is what a person means when they say which one they are in.
    ///
    /// Computed by the host so every frontend and plugin agrees, the same as `label`.
    #[serde(default)]
    pub project: String,
    /// The main checkout of the repository `cwd` is in, when it is in one.
    ///
    /// This is what lets a list *group* rather than merely label: `project` says `neosh ·
    /// fix/thing` about a linked worktree, but a string made for reading is not a key — the only
    /// way to find the worktree's siblings in it is to parse a display format back apart. The
    /// sidebar nests a worktree under the project it is a worktree *of*, and this is the field
    /// that says which one that is. Equal to `cwd` in the main checkout itself; `None` outside a
    /// repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    /// The branch checked out at `cwd` when the host last looked, `None` on a detached head or
    /// outside a repository. A label, not a fact about *now*: it is refreshed when the directory
    /// is next visited, not on every `git switch` someone runs in a shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
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
    /// How full the context window was on the most recent request: input plus cache, for that one
    /// turn.
    ///
    /// Separate from `usage`, which is cumulative and therefore useless for this: a conversation
    /// that has spent 400k tokens across twenty turns is not 400k tokens *long*. The number people
    /// want is how much room is left, and only the last request knows it.
    #[serde(default)]
    #[ts(type = "number")]
    pub context_tokens: u64,
    /// The window those tokens are a fraction of, **as the driver reports it**.
    ///
    /// `None` when nothing has said, and then a catalogue figure is the best available guess. It is
    /// only a guess: a vendor CLI's window is whatever that CLI decided, which is not always what a
    /// catalogue says the model has. `claude` running `claude-haiku-4-5` answers `maxTokens:
    /// 200000` while neosh's own entry for the selected model said a million — so the meter was
    /// dividing a number it half-knew by a denominator it had invented, and read `2%` where the
    /// agent itself would have said `8%`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null")]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub is_active: bool,
    /// A turn finished here while you were looking at something else.
    ///
    /// The one thing a list of conversations cannot say without being told. A turn takes minutes,
    /// you switch away, and the row it left behind is indistinguishable from the twelve above it —
    /// so the answer you were waiting for is found by opening conversations until one of them has
    /// it. This is the mark that makes it findable, and it is *news* rather than state: it is set
    /// when a turn ends somewhere you were not, and cleared the moment you arrive.
    ///
    /// Off screen is the whole test. A turn that ended in the conversation on screen has already
    /// been seen — including when nothing was attached, because reattaching lands you in that
    /// conversation with the answer in it.
    #[serde(default)]
    pub unread: bool,
    /// What is still running here that no turn is waiting for.
    ///
    /// The other half of [`Self::unread`]. That one says a turn *finished* somewhere you were not
    /// looking; this says one did not finish everything it started — a shell the agent put in the
    /// background, a sub-agent it let go of — and that the conversation is still doing something
    /// even though it has told you it is done.
    ///
    /// A level: whatever the driver last said is running, verbatim, and empty means nothing is.
    /// Kept on the conversation rather than the turn because the turn is over — that is the case
    /// it exists for — and because there is more than one list that has to agree about it. See
    /// [`crate::Activity::Background`], including why it is never persisted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background: Vec<BackgroundTask>,
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
    /// What this conversation lets the agent do without asking.
    ///
    /// A property of the conversation and not of the process, because that is how it is used: a
    /// throwaway question in someone else's repository and the branch you have been on all week are
    /// not the same risk, and a mode that followed whichever one you looked at last would be a
    /// setting you could only trust by re-reading it. It is saved with the conversation for the
    /// same reason — coming back to a conversation should mean coming back to how it was set up.
    ///
    /// `None` means the conversation has never been given one and takes whatever `config.toml`
    /// says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
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
    /// Allow everything, without asking.
    ///
    /// What `config.toml` starts you on, and what a conversation inherits when nobody has said
    /// otherwise — because the alternative people actually reached was approving reflexively, which
    /// is worse: it looks like consent and is not. So it is named plainly, said in the status line
    /// the whole time it is on, and one keystroke from being turned off.
    ///
    /// Workspace containment still applies, in this mode as in every other: a path outside the
    /// workspace is refused whatever the mode says, because "full access" is about not being asked,
    /// not about reaching the rest of the disk.
    ///
    /// It is deliberately **not** this enum's [`Default`]. That one answers "a field was missing
    /// from a payload", and the safe answer to a question nobody asked is still [`Self::Ask`]. The
    /// default a person means lives in [`crate::PermissionMode`]'s caller — `PermissionsConfig` —
    /// where it is a decision rather than a gap.
    Allow,
    /// Refuse everything not explicitly allowed.
    Deny,
}

impl PermissionMode {
    /// The word to show a person.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AllowListed => "allow-listed",
            Self::Allow => "full access",
            Self::Deny => "deny",
        }
    }

    /// The order they run in when cycling, loosest first, so the direction is predictable.
    pub const ORDER: [Self; 4] = [Self::Allow, Self::AllowListed, Self::Ask, Self::Deny];
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

/// One answer a permission request offers.
///
/// Agent drivers word their own options — "Yes, and don't ask again for `cargo test`" is a
/// sentence only the agent that ran it can write. neosh's own tools offer none, and the prompt
/// falls back to yes and no.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct PermissionOption {
    /// Opaque, and handed straight back to whoever asked. Never parsed.
    pub id: String,
    /// What the option says, in the asker's own words.
    pub label: String,
    pub kind: PermissionOptionKind,
}

/// What taking an option would mean, independent of how it is worded.
///
/// The wording is the agent's and is shown; this is what lets a UI sort the safe answer to the top,
/// colour the dangerous one, and pick a default without reading English.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl PermissionOptionKind {
    pub fn allows(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }
}

// ---------------------------------------------------------------------------
// Questions
// ---------------------------------------------------------------------------

/// One thing an agent wants to know before it carries on, and the answers it suggests.
///
/// Deliberately not a [`PermissionOption`] list. A permission request asks *may this happen* and
/// every answer to it is a yes or a no; this asks *which of these* — several times over, sometimes
/// with more than one answer each, sometimes with an answer nobody listed. Squeezing it into the
/// permission family would mean a prompt whose "options" are not decisions about the thing being
/// asked, and a policy layer that could answer `full access` to a question about which database to
/// use. See ADR 0043.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct UserQuestion {
    /// The question, and its identity.
    ///
    /// One field for both on purpose. The `claude` CLI looks an answer up *by the text of the
    /// question it answers* — the reply is a map keyed on this string — so a generated id would be
    /// an answer to a question that does not exist, and the turn would come back saying nobody
    /// answered. Whatever asks, the rule is the same: the question is the key.
    pub question: String,
    /// Two or three words over the question — `Auth method`, `Library`, `Approach`. A chip rather
    /// than a sentence, and it is what a folded card says the question was about.
    #[serde(default)]
    pub header: String,
    pub options: Vec<QuestionOption>,
    /// Whether more than one option may be taken.
    #[serde(default)]
    pub multi_select: bool,
}

/// One answer on offer.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct QuestionOption {
    /// What the option is called. **This is what is sent back** — the agent matches on the label
    /// it wrote, not on a position in a list, so a client that reorders the rows still answers the
    /// question it was asked.
    pub label: String,
    /// What taking it would mean. Shown under the label, dimmed; often the part that decides it.
    #[serde(default)]
    pub description: String,
}

/// What somebody said, for one question.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct QuestionAnswer {
    /// Which question this answers, by its [`UserQuestion::question`] text.
    pub question: String,
    /// The labels taken, in the order they were taken. Exactly one for a single-select question.
    ///
    /// An answer somebody *typed* is one entry that is not any of the offered labels, and that is
    /// not a special case on the wire: to the agent, "Postgres" and "neither, use what is already
    /// there" are the same kind of thing — what the user said.
    pub answers: Vec<String>,
    /// Whether [`Self::answers`] was typed rather than chosen. Carried so a transcript can draw it
    /// differently; nothing sent to the agent depends on it.
    #[serde(default)]
    pub custom: bool,
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
    /// Fired when an agent wants an answer from the person, rather than permission from the
    /// policy. A blocking hook here answers it; a veto — or no blocking hook at all — is "nobody
    /// answered", which the agent is told plainly rather than being left to time out.
    AskUser,
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
        /// What the asker said it was about to do, in its own words, when that is more than the
        /// capability can carry. An agent driver's `Run cargo test --workspace` is one line the
        /// user can act on; `Exec` plus a truncated command is not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        title: Option<String>,
        /// The answers the asker offers. Empty when the only answers are yes and no, which is the
        /// case for every built-in tool.
        #[serde(default)]
        options: Vec<PermissionOption>,
        /// Which of `options` was taken, by `id`. Set by the hook, in a [`HookOutcome::Modify`]
        /// carrying this payload back — that is how an answer richer than yes-or-no gets home.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        chosen: Option<String>,
    },
    /// An agent is asking the person a question. See [`UserQuestion`].
    AskUser {
        /// Which conversation is asking. A workspace runs several turns at once and only one of
        /// them is on screen, so a panel that assumed "the one in front of you" would put another
        /// conversation's question over the one you are reading.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        session: Option<SessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        turn: Option<TurnId>,
        /// Where the turn is running, so a question about a file can be read against the right
        /// tree.
        #[serde(default)]
        cwd: String,
        questions: Vec<UserQuestion>,
        /// The answers. Set by the hook, in a [`HookOutcome::Modify`] carrying this payload back —
        /// the same way [`Self::PermissionPre::chosen`] comes home.
        ///
        /// `None` means nobody has answered yet. An empty list is not the same thing and is read as
        /// a refusal: it is what a hook that ran and declined to ask anybody returns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        answers: Option<Vec<QuestionAnswer>>,
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


