//! plugin -> core: the entire public API surface, as data.
//!
//! Every capability neosh has is reachable through [`ApiCall`]. The host's own built-in UI is
//! written against this same enum — there is no private side door. If something cannot be expressed
//! here, that is a bug in this file, not a reason to reach into the core.
//!
//! In-process these are passed as Rust values; out-of-process they are the `params` of a JSON-RPC
//! call. Making them one serializable type is what keeps those two paths honest.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::agent::{
    Capability, HookName, HookPayload, Message, PermissionDecision, PermissionMode, QuestionAnswer,
    SessionInfo, ToolDef, ToolResult, UserQuestion,
};
use crate::ascp::{AgentCommand, AgentSummary, NodeCapabilities, NodeId, NodeInfo, ProjectKey};
use crate::ids::{
    BufferId, ExtmarkId, NamespaceId, RequestId, SessionId, StreamId, SurfaceId, WindowId,
};
use crate::options::{OptionEntry, OptionSpec, OptionValue};
use crate::provider::{
    CredentialInfo, DriverCommand, DriverKind, InstanceConfig, InstanceId, ModelEntry,
    ModelSelection, ProviderEvent,
};
use crate::quota::{QuotaSample, QuotaSnapshot, UsageHistory, UsageResolution};
use crate::ui::{
    CursorMotion, ExtmarkInfo, ExtmarkOpts, FloatConfig, HighlightDef, KeyPress, LineDraw,
    MessageLevel, Rect, SurfaceCell, TextEdit, WindowLayout,
};
use crate::vcs::{BranchInfo, CommitInfo, DiffTarget, RepoStatus, WorktreeInfo};

/// Enough history for a commit-message prompt without turning one keystroke into a full `git log`.
fn default_log_limit() -> u32 {
    20
}

/// Serde default for flags that are on unless asked otherwise.
fn yes() -> bool {
    true
}

/// Editing mode, for keymap scoping.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Visual,
    /// Composing a message to the agent.
    Chat,
}

/// Scope a keymap applies at. Resolution order is window -> buffer -> kind -> global; the first
/// binding found wins, which is what stops every plugin fighting over `<Esc>`.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum KeymapScope {
    Global,
    Buffer {
        buf: BufferId,
    },
    Window {
        win: WindowId,
    },
    /// Every window showing a buffer of this [`ApiCall::BufSetKind`] kind, including ones that do
    /// not exist yet.
    ///
    /// This is the scope a third party binds at. A panel's window id is private to the plugin that
    /// opened it and changes every time the panel is closed and reopened, so `Window` can only ever
    /// be claimed by the owner; a *kind* is a name the owner publishes once and anybody can map
    /// against. It is Neovim's `FileType` autocmd plus a buffer-local map, minus the autocmd —
    /// binding `d` on `neosh.sidebar` is a statement about sidebars, not about a window that
    /// happens to be open now.
    BufKind {
        /// The kind, not the discriminant — `name` because `kind` is already the tag this enum is
        /// serialized under.
        name: String,
    },
}

/// What a shared variable is *about*.
///
/// Deliberately not a free-form string. A scope that was `"project:/home/me/x"` would be one typo
/// away from a value nothing ever reads again, and the host is the only thing that can say whether a
/// conversation still exists — which is what lets it throw away the vars of one that does not.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
#[serde(tag = "scope", rename_all = "snake_case")]
#[ts(export)]
pub enum VarScope {
    /// The workspace. Survives every conversation in it.
    Global,
    /// One conversation. Deleted with it.
    Session { session: SessionId },
    /// A directory some conversation lives in — what the sidebar calls a project. Keyed by path
    /// rather than by an id because a project has no identity beyond where it is.
    Project { cwd: String },
}

/// One item somebody put on a contribution point.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct Contribution {
    pub point: String,
    pub id: String,
    /// Which plugin contributed it. Stamped by the host, so a row in a panel can always be traced
    /// to the thing that put it there — and so `plugins.disabled` takes its rows with it.
    pub plugin: String,
    #[ts(type = "unknown")]
    pub item: serde_json::Value,
    pub priority: i32,
}

/// A node, as a plugin sees it: what it is, and what it has.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct SwarmNode {
    pub info: NodeInfo,
    pub capabilities: NodeCapabilities,
    /// `false` for a machine we have lost touch with. Its agents are still described, because they
    /// were real a moment ago and probably still are — what changed is that we cannot see them.
    pub up: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub agents: Vec<AgentSummary>,
}

/// One remote agent, with the node it belongs to attached.
///
/// Flattened for the common case, which is drawing a list: an agent is addressed as `(node,
/// session)`, and a row that carried only the session would be a row you cannot act on.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct SwarmAgent {
    pub node: NodeInfo,
    pub agent: AgentSummary,
}

/// A machine that proved who it is and has not been paired with.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct SwarmStranger {
    pub info: NodeInfo,
    /// `true` when we found it by dialling an address, `false` when it dialled us.
    ///
    /// Decides what to ask. One is "this is what is at that address — add it?", the other is
    /// "this machine wants to join — allow it?", and they are different questions even though the
    /// button does the same thing.
    pub dialled: bool,
    /// Where it was reached, when we were the one dialling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
}

/// A window as somebody else sees it.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct WindowInfo {
    pub win: WindowId,
    pub buf: BufferId,
    /// What the buffer in it declared itself to be, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub name: String,
    pub layout: WindowLayout,
    /// Whether this is the window keys are going to.
    #[serde(default)]
    pub focused: bool,
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "call", rename_all = "snake_case")]
#[ts(export)]
pub enum ApiCall {
    // ---- buffers -------------------------------------------------------
    BufCreate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default)]
        scratch: bool,
        /// What this buffer *is*, so that somebody else can say something about it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    BufLineCount {
        buf: BufferId,
    },
    BufGetLines {
        buf: BufferId,
        #[ts(type = "number")]
        start: i64,
        /// Exclusive. Negative indices are `len + 1 + i`, as in Neovim: `-1` is one past the last
        /// line, so `0..-1` is the whole buffer.
        #[ts(type = "number")]
        end: i64,
    },
    /// Range replacement. The *only* way text changes.
    ///
    /// There is deliberately no "set the whole buffer" call: streaming a response must not resend
    /// the document once per token.
    BufSetLines {
        buf: BufferId,
        #[ts(type = "number")]
        start: i64,
        #[ts(type = "number")]
        end: i64,
        lines: Vec<String>,
    },
    /// Replace a range of lines **and** this namespace's marks on them, in one call.
    ///
    /// The atomic form of the sequence every panel used to write by hand: `BufSetLines`, then
    /// `MarkClear`, then one `MarkSet` per mark. That sequence is correct at rest and wrong in
    /// flight — the frontend draws on a coalescing deadline that knows nothing about how far
    /// through a repaint a plugin is, so a frame landing after the clear draws every row with no
    /// marks at all, in `Normal`. A dim sidebar becomes bright white for a frame, which is what a
    /// panel redrawing ten times a second looks like when agents are running.
    ///
    /// [`UiEvent::BufferLines`] already promises that text and its marks travel together. This is
    /// the same promise on the way in: one call, one mutation, one event, and no window in which
    /// half of a repaint is observable.
    ///
    /// `start`/`end` are resolved like [`ApiCall::BufSetLines`]'s, and the clear covers exactly the
    /// rows written — a partial repaint does not disturb the marks on rows it did not touch. Marks
    /// belonging to other namespaces are left alone, so a selection drawn in its own namespace
    /// survives the panel underneath it being redrawn.
    BufRender {
        buf: BufferId,
        ns: NamespaceId,
        #[ts(type = "number")]
        start: i64,
        #[ts(type = "number")]
        end: i64,
        lines: Vec<LineDraw>,
    },
    /// Append to the final line without resending it. The streaming fast path.
    BufAppendText {
        buf: BufferId,
        text: String,
    },
    BufSetName {
        buf: BufferId,
        name: String,
    },
    /// Declare what a buffer is — `neosh.sidebar`, `neosh.transcript`, `acme.tasks`.
    ///
    /// A kind is the handle everything else in the extension surface hangs off: it is what
    /// [`KeymapScope::Kind`] binds against and what [`ApiCall::WinList`] reports, so publishing one
    /// is how a panel becomes something a third party can map keys into and find on screen. Reverse
    /// domain by convention; nothing enforces it.
    BufSetKind {
        buf: BufferId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    BufGetKind {
        buf: BufferId,
    },
    /// Subscribe to changes. Delivered as [`crate::plugin::PluginEvent::BufferChanged`].
    BufAttach {
        buf: BufferId,
    },
    BufDetach {
        buf: BufferId,
    },

    // ---- windows -------------------------------------------------------
    WinOpen {
        buf: BufferId,
        layout: WindowLayout,
    },
    WinClose {
        win: WindowId,
    },
    /// Change how wide (or tall) a docked window is, without closing it.
    ///
    /// Reopening was the only way to resize a dock, and reopening is not the same thing: the window
    /// id changes, whatever had the keyboard loses it, and a panel resized from inside itself would
    /// throw you back to the composer on every press. `None` gives the dock back its default
    /// extent. A float is configured through [`ApiCall::FloatConfigure`] and is refused here — the
    /// two have nothing in common but the word.
    WinResize {
        win: WindowId,
        size: Option<u16>,
    },
    WinSetBuf {
        win: WindowId,
        buf: BufferId,
    },
    WinGetCursor {
        win: WindowId,
    },
    WinSetCursor {
        win: WindowId,
        row: u32,
        col: u32,
    },
    /// How big this window actually is, in cells, and where it is scrolled to.
    ///
    /// The only way a plugin learns real geometry. Everything about display width is resolved by
    /// the frontend, so a plugin sizing a meter or deciding what to drop at 60 columns asks for the
    /// answer rather than computing one it cannot compute correctly.
    WinGetViewport {
        win: WindowId,
    },
    /// Put a buffer row at the top of a window, or give the scroll position back.
    ///
    /// `None` means *unscrolled* — the frontend's own answer, which for the transcript is the last
    /// screenful and for everything else is the first. It is a different place from `Some(0)`, and
    /// the difference is the whole point: while the two shared an encoding, scrolling a transcript
    /// to its first row asked for the state that draws its last one, so reaching the top of a long
    /// answer showed the bottom of it.
    WinScrollTo {
        win: WindowId,
        top_line: Option<u32>,
    },
    /// Every window that is open, what is in it, and where it sits.
    ///
    /// The counterpart to [`ApiCall::BufSetKind`]: kinds make panels nameable, this makes them
    /// findable. Without it a plugin can only ever act on windows it opened itself, which means the
    /// only way to extend somebody else's panel is to replace it.
    WinList,

    // ---- editing -------------------------------------------------------
    // Motions and edits are verbs the core resolves, not positions the caller computes. Two
    // reasons: grapheme and word boundaries are genuinely hard and nobody should have to get them
    // right twice, and a composer, a plugin's text field and a future normal mode all behaving the
    // same way is only possible if they are all asking the same question.
    /// Move a window's cursor.
    ///
    /// `select` extends a selection from wherever it was anchored, dropping an anchor first if
    /// there was none — the shift-and-arrow behaviour of every editor. Without it, the selection
    /// is cleared, which is the other half of that behaviour.
    WinMotion {
        win: WindowId,
        motion: CursorMotion,
        #[serde(default)]
        select: bool,
    },
    /// Edit at a window's cursor.
    WinEdit {
        win: WindowId,
        edit: TextEdit,
    },
    /// Anchor a selection at the current cursor, or drop the one there is.
    WinSelect {
        win: WindowId,
        /// `false` clears. `true` anchors here even if something was already selected.
        on: bool,
    },
    /// What is selected in this window. Empty when nothing is.
    WinSelection {
        win: WindowId,
    },
    /// Put text on the system clipboard.
    ///
    /// A capability rather than something a plugin could do itself: the runtime has no terminal
    /// and no processes, and the frontend is the only thing holding the stream the escape has to
    /// be written to.
    ClipboardWrite {
        text: String,
    },

    // ---- floats --------------------------------------------------------
    FloatOpen {
        buf: BufferId,
        config: FloatConfig,
    },
    FloatConfigure {
        win: WindowId,
        config: FloatConfig,
    },

    // ---- extmarks ------------------------------------------------------
    NsCreate {
        name: String,
    },
    MarkSet {
        ns: NamespaceId,
        buf: BufferId,
        row: u32,
        /// UTF-8 byte offset into the line, not a character or display column.
        col: u32,
        opts: ExtmarkOpts,
    },
    MarkDel {
        ns: NamespaceId,
        buf: BufferId,
        id: ExtmarkId,
    },
    MarkClear {
        ns: NamespaceId,
        buf: BufferId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end: Option<u32>,
    },
    MarkGet {
        ns: NamespaceId,
        buf: BufferId,
        id: ExtmarkId,
    },
    MarkAll {
        ns: NamespaceId,
        buf: BufferId,
    },

    // ---- highlights ----------------------------------------------------
    HlDefine {
        name: String,
        def: HighlightDef,
    },

    // ---- raw cells -----------------------------------------------------
    SurfaceClaim {
        win: WindowId,
        rect: Rect,
    },
    SurfacePut {
        surface: SurfaceId,
        cells: Vec<SurfaceCell>,
    },
    SurfaceRelease {
        surface: SurfaceId,
    },

    // ---- commands & keymaps --------------------------------------------
    CmdRegister {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        desc: Option<String>,
    },
    CmdUnregister {
        name: String,
    },
    CmdExec {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
    CmdList,
    KeymapSet {
        mode: Mode,
        /// Neovim-style notation: `<C-x>`, `<Esc>`, `<CR>`, `gd`.
        lhs: String,
        /// Command name to invoke. Plugins bind keys to commands, never to raw callbacks, so that
        /// every binding is discoverable and remappable by the user.
        command: String,
        #[serde(default)]
        scope: Option<KeymapScope>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        desc: Option<String>,
    },
    KeymapDel {
        mode: Mode,
        lhs: String,
        #[serde(default)]
        scope: Option<KeymapScope>,
    },
    /// While `win` is focused, send every key the keymaps did not claim to `command`, as a
    /// [`KeyContext`].
    ///
    /// The escape hatch for a widget that needs raw input — a filter box, a text field, a modal
    /// list. Bindings still win, so global keys keep working while a picker is open; only the keys
    /// nothing else wanted are captured. This is the same fallback the host uses for its own
    /// composer, made available rather than kept private.
    KeymapCapture {
        win: WindowId,
        command: String,
    },
    KeymapRelease {
        win: WindowId,
    },
    KeymapList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<Mode>,
    },

    // ---- focus ---------------------------------------------------------
    FocusPush {
        win: WindowId,
    },
    FocusPop,
    FocusCurrent,

    // ---- agent ---------------------------------------------------------
    AgentSend {
        text: String,
        /// Files to send with it, by path. Copied into the workspace's own directory on the way
        /// through, so a caller may hand over a temporary file and forget about it.
        ///
        /// Anything already on the composer's attachment row goes too — this is *extra*, for a
        /// plugin that has produced an image of its own rather than one somebody pasted.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    AgentCancel,
    AgentGetSelection,
    /// Hot-swap the model mid-session. Takes effect on the next turn.
    AgentSetSelection {
        selection: ModelSelection,
    },
    AgentListModels {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<InstanceId>,
        /// Re-query the endpoints instead of using this session's cache.
        ///
        /// Discovery is a network round trip per instance. Doing it on every keystroke that opens a
        /// model picker is seconds of nothing happening, so the answer is cached for the session
        /// and this is how you ask for a fresh one.
        #[serde(default)]
        refresh: bool,
    },
    AgentListInstances,
    /// What the driver behind this conversation accepts as a slash command.
    ///
    /// Reported by the driver at its handshake rather than configured, because which commands exist
    /// depends on the install: `claude` counts project `.claude/commands/`, plugin commands and MCP
    /// prompts among its own, and an ACP agent sends whatever its vendor built in. Empty for a
    /// driver that has none and for a conversation that has not run a turn yet — there has been
    /// nothing to ask.
    AgentDriverCommands,
    /// Attach an image to whatever is about to be sent.
    ///
    /// `path` names a file; `None` means "whatever image is on the system clipboard", which is the
    /// one a key is bound to and the one a terminal cannot deliver any other way — bracketed paste
    /// is a text protocol and a PNG will never arrive down it.
    ///
    /// The bytes are copied into the workspace's directory, sniffed for what they actually are and
    /// shrunk if they are enormous. The answer says what was attached, so a caller can report it.
    ChatAttach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// What is attached to the composer right now, oldest first.
    ChatAttachments,
    /// Take something off the attachment row.
    ///
    /// `index` names one; `None` takes off the newest, which is the one you just added and
    /// therefore the one you meant. See [`ApiCall::ChatDetachAll`] for the whole row.
    ChatDetach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<u32>,
    },
    ChatDetachAll,
    /// Replace what is in the composer.
    ///
    /// The composer is the host's, not a buffer a plugin may write to directly: it has a cursor, a
    /// height that follows its content, and a draft that is parked and restored when conversations
    /// change. This is the one supported way to put words in somebody's mouth, and it exists so
    /// completion — of a command, a path, a file — can be a plugin.
    ChatSetDraft {
        text: String,
    },

    // ---- credentials ---------------------------------------------------
    // Three verbs, and *none of them carries a key*. Listing reports where a key comes from, never
    // what it is; setting one hands the job to the host, which reads the keystrokes itself and puts
    // the value straight into a secret store. Nothing a plugin can call returns a secret, so no
    // plugin can leak one — which is a property of the protocol rather than of plugin authors
    // being careful.
    ProviderCredentials,
    /// Ask the host to collect a key for `instance` from the keyboard.
    ///
    /// The host owns this prompt because a plugin one would put the key in a buffer, and a buffer
    /// is drawn — the value would cross the frontend boundary in a `UiEvent` and land in whatever
    /// the frontend logs. Answered when the prompt closes: `true` if a key was stored.
    ProviderSetCredential {
        instance: InstanceId,
        /// Overwrite a key that is already there without asking. Off by default.
        #[serde(default)]
        replace: bool,
    },
    ProviderForgetCredential {
        instance: InstanceId,
    },

    // ---- sessions ------------------------------------------------------
    // A workspace is not one conversation. These are the verbs a thread list needs; what it looks
    // like is a plugin's business.
    SessionList {
        /// Include conversations that have been put away. Off by default, so every existing caller
        /// keeps showing the list the user actually works in.
        #[serde(default)]
        include_archived: bool,
    },
    SessionCurrent,
    /// Start a conversation. `cwd` defaults to the one neosh was started in, so opening a session
    /// in another checkout is one argument rather than another process.
    SessionNew {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Switch to it as well. `false` is for restoring a workspace.
        #[serde(default = "yes")]
        activate: bool,
    },
    SessionSwitch {
        session: SessionId,
    },
    /// Close a conversation. Closing the active one moves to the most recently used other; closing
    /// the last one is an error, because there is always somewhere for the next thing you type.
    SessionClose {
        session: SessionId,
    },
    SessionRename {
        session: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Put a conversation away, or bring it back.
    ///
    /// Distinct from [`SessionClose`](Self::SessionClose), which deletes the file. Archiving the
    /// active conversation moves you to the most recently used other one, exactly as closing does,
    /// because there is always somewhere for the next thing you type.
    SessionArchive {
        session: SessionId,
        #[serde(default = "yes")]
        archived: bool,
    },
    /// The conversation itself, for a transcript view that wants to render it rather than replay
    /// the UI events that built it.
    SessionMessages {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
    },

    // ---- tools ---------------------------------------------------------
    /// Register a tool. Lands in the same namespace and shape as a built-in or MCP tool.
    ToolRegister {
        def: ToolDef,
    },
    ToolUnregister {
        name: String,
    },
    ToolList,

    // ---- hooks ---------------------------------------------------------
    HookRegister {
        hook: HookName,
        /// Non-blocking hooks are pure observers and cannot veto — that is what stops an audit
        /// plugin from wedging the agent loop. Blocking hooks are awaited, and a timeout is
        /// treated as a veto so a permission plugin fails closed.
        #[serde(default)]
        blocking: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(type = "number | null")]
        timeout_ms: Option<u64>,
    },
    HookUnregister {
        hook: HookName,
    },

    // ---- providers -----------------------------------------------------
    /// Register a model provider implemented in the plugin.
    ///
    /// This is the extensibility thesis applied to the model layer: supporting a new vendor is a
    /// plugin, not a core change.
    ProviderRegisterDriver {
        driver: DriverKind,
        instances: Vec<InstanceConfig>,
        /// Whether this driver runs its own agent loop, like `claude` or anything speaking ACP.
        ///
        /// It changes what the host does with the stream, not how the stream is produced: an agent
        /// driver is sent no tool list, because it brings its own, and neosh's tool registry, hooks
        /// and permission layer therefore do not gate what it does. Saying so is the honest option
        /// — a plugin shipping an agent driver that claimed to be a model driver would have the
        /// host run its tool calls a second time.
        #[serde(default)]
        agent_loop: bool,
    },
    /// Push one event for an in-flight plugin-implemented stream. See
    /// [`crate::provider::ProviderEmit`].
    ProviderEmit {
        stream: StreamId,
        event: ProviderEvent,
    },

    // ---- permissions ---------------------------------------------------
    /// Ask the host whether a capability is allowed. Plugins performing side effects must call this
    /// rather than assume.
    /// What the permission mode is right now.
    PermissionGetMode,
    /// Change it for this session. Not written to configuration: a mode you turned on to get
    /// through one task should not still be on next week.
    PermissionSetMode {
        mode: PermissionMode,
    },
    PermissionCheck {
        capability: Capability,
    },

    // ---- questions -----------------------------------------------------
    /// Ask the person a question, and wait for the answer.
    ///
    /// The same door an agent driver's `AskUserQuestion` comes through, and deliberately so: a
    /// plugin that needs to know which of three things you meant should get the panel the agent
    /// gets, not a picker of its own that looks nearly like it. Whatever serves the
    /// [`HookName::AskUser`](crate::HookName::AskUser) hook draws it — the bundled panel by
    /// default, and whatever loads after it otherwise.
    ///
    /// Answers `None` when nobody answered: dismissed, timed out, or no panel at all. That is not
    /// an error, and a caller that treats it as one is a caller that will report a failure every
    /// time somebody presses `<Esc>`.
    AskUser {
        questions: Vec<UserQuestion>,
    },

    // ---- options -------------------------------------------------------
    /// Declare an option. The built-in options are declared through this same call at startup, so
    /// there is nothing a plugin's settings cannot do that neosh's own can.
    OptDeclare {
        spec: OptionSpec,
    },
    /// Set a declared option. Undeclared names and wrong types are errors, never silent no-ops.
    OptSet {
        name: String,
        value: OptionValue,
    },
    OptGet {
        name: String,
    },
    /// Restore an option to its declared default.
    OptReset {
        name: String,
    },
    OptAll,

    // ---- plugin state --------------------------------------------------
    // Small facts a plugin needs to remember across restarts: which projects are pinned, what order
    // the user dragged them into, which sections are folded. Options are the wrong home for these —
    // an option is configuration the user writes, and silently rewriting someone's config file
    // because they pressed a key is not a thing an editor should do.
    //
    // The store is keyed by the calling plugin, which the host knows and the caller cannot forge,
    // so one plugin cannot read or clobber another's state.
    /// Read a value. `null` when nothing was stored, which is indistinguishable from having stored
    /// `null` — a distinction no caller has wanted.
    StateGet {
        key: String,
    },
    StateSet {
        key: String,
        #[ts(type = "unknown")]
        value: serde_json::Value,
    },
    StateDelete {
        key: String,
    },

    // ---- contributions -------------------------------------------------
    // How one plugin adds something to another plugin's surface without either of them knowing the
    // other exists.
    //
    // A *point* is a name a host plugin agrees to read — `sidebar.section`, `palette.entry`,
    // `project.action` — and a contribution is a JSON item on it, usually carrying the name of a
    // command to run. That indirection is the whole trick: the sidebar renders rows it did not
    // write and invokes commands it has never heard of, and neither side imports the other.
    //
    // It is deliberately data rather than a callback. A contribution survives being listed by the
    // palette, described in `^Z`, and disabled by the user, none of which is possible for a
    // function pointer held inside somebody's closure.
    /// Add an item to a point, replacing whatever this plugin had put there under the same `id`.
    ExtContribute {
        point: String,
        id: String,
        #[ts(type = "unknown")]
        item: serde_json::Value,
        /// Where it sorts among the contributions on this point. Ties break on plugin id, so the
        /// order is stable across restarts rather than being load order.
        #[serde(default)]
        priority: i32,
    },
    /// Withdraw one. Silently fine if it was never there.
    ExtRemove {
        point: String,
        id: String,
    },
    /// Everything contributed to a point, in priority order, whoever contributed it.
    ExtList {
        point: String,
    },

    // ---- events --------------------------------------------------------
    /// Say that something happened, to whoever cares.
    ///
    /// Plugin-defined and broadcast, the equivalent of Neovim's `User` autocmd. The core neither
    /// knows nor validates what the names mean; it only guarantees that a plugin cannot forge
    /// `from`, so a listener can tell who said it.
    ///
    /// Fire and forget on purpose. An emitter that could be blocked, or could read a reply, would
    /// make every listener part of the emitter's critical path — which is how an extension surface
    /// turns into a place where one slow plugin wedges the panel. Something that needs an answer
    /// asks for one with a command or a contribution point.
    EventEmit {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(type = "unknown")]
        data: Option<serde_json::Value>,
    },

    // ---- shared variables ----------------------------------------------
    // The other half of plugin state: what a plugin remembers *about something*, where the point is
    // that everybody can see it.
    //
    // `State` is private by construction and that is right for a panel's fold set. It is wrong for
    // "this project is a favourite", which four plugins have an opinion about and which stopped
    // being the sidebar's business the moment a second panel existed. A var is scoped to the thing
    // it describes — the workspace, a conversation, a project — namespaced by convention, readable
    // and writable by anyone, and persisted.
    VarGet {
        scope: VarScope,
        key: String,
    },
    VarSet {
        scope: VarScope,
        key: String,
        #[ts(type = "unknown")]
        value: serde_json::Value,
    },
    VarDelete {
        scope: VarScope,
        key: String,
    },
    /// Everything set on one scope. The way a panel reads its whole arrangement in one call rather
    /// than one round trip per project.
    VarAll {
        scope: VarScope,
    },

    // ---- the swarm -----------------------------------------------------
    // Other machines. The capability is in the host because it needs sockets and signatures; every
    // decision about what to *show* is a plugin's, which is why this surface is descriptions and
    // requests rather than anything that draws.
    /// This node: who we are, and whether the swarm is running at all.
    SwarmSelf,
    /// Every node we know of, up or down, with what it is running.
    SwarmNodes,
    /// Every agent on every peer that is reachable, flattened.
    SwarmAgents,
    /// Which machines have a project, by its cross-machine key. Empty when it is only here.
    SwarmHostsOf {
        project: ProjectKey,
    },
    /// Ask a node to do something with one of its agents.
    ///
    /// Settles when the owner answers — every command is answered exactly once, so this cannot
    /// hang on a peer that simply ignored it, only on one that has gone, which times out.
    SwarmCommand {
        node: NodeId,
        session: SessionId,
        command: AgentCommand,
    },
    /// Watch a remote conversation: history now, then everything as it happens.
    ///
    /// Delivered as [`crate::plugin::PluginEvent::SwarmStream`]. Idempotent, and worth dropping
    /// when you stop looking — a subscription is the difference between a quiet swarm and one where
    /// every machine sends every token to everyone.
    SwarmSubscribe {
        node: NodeId,
        session: SessionId,
    },
    SwarmUnsubscribe {
        node: NodeId,
        session: SessionId,
    },
    /// Dial an address and report what machine is there, without joining it.
    ///
    /// The first half of pairing. A node presents its identity to anyone who asks, as an SSH server
    /// presents a host key, so this answers with a *proven* name and fingerprint — which is what a
    /// person needs to be shown before deciding.
    SwarmProbe {
        addr: String,
    },
    /// Authorise a machine, and dial it from now on.
    ///
    /// Takes effect immediately: no restart, and nothing written to the user's config file. Paired
    /// machines live in the state directory, because an editor that edits your `config.toml`
    /// because you pressed a key is one you stop trusting with the file.
    SwarmPair {
        node: NodeId,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addr: Option<String>,
    },
    /// Withdraw authorisation, and stop dialling.
    SwarmUnpair {
        node: NodeId,
    },
    /// Machines that have asked to join, or that we found and have not added.
    SwarmStrangers,

    // ---- the plan ------------------------------------------------------
    // What the account has left, and what it has already spent. The capability is in the host
    // because it reads credentials and other programs' transcripts; every decision about what to
    // *draw* is a plugin's, which is why this surface is numbers and nothing else.
    /// The latest snapshot for every instance that has one, newest fact first.
    ///
    /// Answers from what the host kept rather than by asking the vendor, so it costs nothing and is
    /// safe to call on every redraw. [`QuotaSnapshot::observed_at`] is how stale each one is;
    /// [`ApiCall::QuotaRefresh`] is how to make it less so.
    QuotaList,
    /// Ask the vendor now, for one instance or for every instance that can be asked.
    ///
    /// Answers as soon as the request is *made*, not when it lands — a poll is a network round trip
    /// and a redraw that awaited it would be a panel that opens late. The answer arrives as
    /// [`crate::plugin::PluginEvent::Quota`], the same way an unprompted one does, so a caller
    /// needs one code path rather than two.
    QuotaRefresh {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<InstanceId>,
    },
    /// Publish a snapshot for an instance this plugin's own driver serves.
    ///
    /// The other half of [`ApiCall::ProviderRegisterDriver`]: a provider written as a plugin knows
    /// its own vendor's allowance and nothing in the host does. Reported this way it is kept,
    /// sampled, broadcast and drawn exactly like a built-in one — which is the whole test, since
    /// the built-in sources publish through the same store.
    QuotaReport {
        snapshot: QuotaSnapshot,
    },
    /// Every percentage this workspace has seen, so a gauge can be a line.
    ///
    /// Bounded by time rather than by count: the interesting window is "this reset period", and a
    /// caller that asked for the last N samples would get a different span depending on how long
    /// neosh happened to be running.
    QuotaHistory {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<InstanceId>,
        /// Unix seconds, inclusive.
        #[ts(type = "number")]
        since: i64,
        /// Unix seconds, exclusive.
        #[ts(type = "number")]
        until: i64,
    },
    /// Tokens and their money-equivalent over a span, read out of the vendor CLIs' own transcripts.
    ///
    /// Not from neosh's conversations: a turn run in `claude` directly spent the same allowance and
    /// a history that could not see it would be answering a different question from the one asked.
    /// Expensive — it reads files — so it is a call a panel makes when it opens, never a redraw.
    UsageHistory {
        /// Unix seconds, inclusive.
        #[ts(type = "number")]
        since: i64,
        /// Unix seconds, exclusive.
        #[ts(type = "number")]
        until: i64,
        resolution: UsageResolution,
        /// IANA zone to bucket days in. An offset is wrong for any span crossing a DST boundary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time_zone: Option<String>,
    },

    // ---- runtime path --------------------------------------------------
    /// Add a directory to search for plugins.
    ///
    /// Only meaningful before plugin discovery runs, which is why the user config activates first:
    /// it is the thing that gets to decide what else loads.
    RtpAdd {
        path: String,
    },
    RtpList,

    // ---- paths ---------------------------------------------------------
    /// Directories whose path begins with `prefix`, for completing a typed path.
    ///
    /// The script runtime has no filesystem, and a path field without completion is a path field
    /// you type wrong. Deliberately narrow: directory *names* only, never file contents and never a
    /// recursive walk, so the answer is one `read_dir` and cannot become a way to read the disk.
    PathComplete {
        prefix: String,
    },

    // ---- version control -----------------------------------------------
    // The script runtime has no process access, so `git` lives here. One vocabulary means a
    // sidebar, a branch picker and a commit UI are three plugins over one implementation rather
    // than three shell-outs with three parsers.
    GitStatus,
    GitBranches {
        #[serde(default)]
        include_remote: bool,
        /// Which checkout to ask. `None` is the one this conversation is in, which is what almost
        /// every caller means. See [`ApiCall::GitWorktrees`] for why the others exist.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Every checkout of the repository containing `cwd`.
    ///
    /// `cwd` is `None` for "the repository this conversation is in", which is what a status bar or
    /// a branch picker wants. A *panel* wants the other answer: the sidebar lists several projects
    /// at once and offers a new conversation in whichever one the cursor is on, and asking about
    /// the active conversation's repository there would offer somebody the worktrees of a
    /// repository they are not looking at. A row that names the wrong branch is worse than no row.
    GitWorktrees {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    GitLog {
        #[serde(default = "default_log_limit")]
        limit: u32,
    },
    GitDiff {
        #[serde(default)]
        target: DiffTarget,
        /// `--stat` instead of the patch. A prompt builder wants this; a diff view does not.
        #[serde(default)]
        stat: bool,
    },
    /// What this branch would merge into: `origin/HEAD`, else `main`/`master`.
    GitDefaultBranch,
    GitCreateBranch {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    GitCheckout {
        rev: String,
    },
    /// Move a branch to another name — `git branch -m`.
    ///
    /// One ref write, so it is safe on the branch a worktree has checked out and safe while an
    /// agent is mid-turn in that worktree: nothing about the files changes. What *does* change is
    /// the label every project list is drawing, which is why the host refreshes what it knows
    /// about the directory here rather than leaving a panel to say the old name until restart.
    GitRenameBranch {
        old: String,
        new: String,
        /// The checkout the branch belongs to. `git branch -m` runs *inside* a repository, and a
        /// caller renaming the branch of a worktree other than the active conversation's has to
        /// say which — the same reason [`Self::GitAddWorktree`] takes one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Empty `paths` stages everything, matching `git add .` from the root.
    GitStage {
        #[serde(default)]
        paths: Vec<String>,
    },
    GitUnstage {
        #[serde(default)]
        paths: Vec<String>,
    },
    GitCommit {
        message: String,
    },
    /// `git pull` in a repository, answering with what git said about it.
    ///
    /// The summary comes back rather than being swallowed, because "Already up to date" and
    /// "Fast-forwarded to …" are different answers to the question the caller asked, and a `Unit`
    /// would flatten both into silence.
    GitPull {
        /// The repository to pull in. The conversation's own when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    GitAddWorktree {
        path: String,
        branch: String,
        #[serde(default)]
        create: bool,
        /// The repository to add it to. `git worktree add` runs *inside* a checkout, so a caller
        /// making a worktree of a project other than the active one has to say which.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    GitRemoveWorktree {
        path: String,
        #[serde(default)]
        force: bool,
        /// The repository the worktree belongs to. `git worktree remove` has to run from a
        /// checkout *other* than the one being removed, and the conversation the caller is in may
        /// be standing in exactly that one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },

    // ---- one-shot generation -------------------------------------------
    /// Run a prompt through a model *outside* the conversation.
    ///
    /// Branch names, commit messages, thread titles and PR bodies are all this call. Keeping it
    /// separate from [`ApiCall::AgentSend`] is the point: none of it may enter session history, or
    /// asking for a commit message would change what the agent believes it was asked to do.
    GenComplete {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system: Option<String>,
        /// Ask for JSON and parse it host-side, tolerating the code fences models wrap it in.
        #[serde(default)]
        json: bool,
        /// Defaults to `gen.model` if set, else the session's own selection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selection: Option<ModelSelection>,
    },

    // ---- status line ---------------------------------------------------
    /// Contribute a segment to the status line.
    ///
    /// This is the composer footer: model, reasoning effort, branch, context meter. The host owns
    /// the *strip* — one line, always visible, never scrolled — and plugins own everything in it,
    /// which is the split that lets the model switcher and the git plugin each put something there
    /// without either knowing the other exists.
    ///
    /// Setting the same `key` again replaces that segment, so a plugin updating a meter does not
    /// have to clear first and cannot leave two.
    StatusSet {
        key: String,
        segment: StatusSegment,
    },
    StatusClear {
        key: String,
    },
    /// Put one shortcut on the row under the composer, or replace what is there under this key.
    HintSet {
        key: String,
        hint: Hint,
    },
    HintClear {
        key: String,
    },

    // ---- misc ----------------------------------------------------------
    Log {
        level: MessageLevel,
        message: String,
    },
    /// Transient message in the UI. Not a buffer, so it never enters conversation history.
    Notify {
        level: MessageLevel,
        message: String,
    },
}

/// Successful results. One variant per shape rather than a bag of JSON, so the TS side gets real
/// discriminated unions.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "ok", rename_all = "snake_case")]
#[ts(export)]
pub enum ApiOk {
    Unit,
    Bool { value: bool },
    Buf { buf: BufferId },
    Win { win: WindowId },
    Ns { ns: NamespaceId },
    Mark { id: ExtmarkId },
    Surface { surface: SurfaceId },
    Lines { lines: Vec<String> },
    Count { n: u32 },
    Cursor { row: u32, col: u32 },
    MarkInfo { info: Option<ExtmarkInfo> },
    Marks { marks: Vec<ExtmarkInfo> },
    Models { models: Vec<ModelEntry> },
    Instances { instances: Vec<InstanceConfig> },
    Selection { selection: Option<ModelSelection> },
    Tools { tools: Vec<ToolDef> },
    Names { names: Vec<String> },
    Commands { commands: Vec<CommandEntry> },
    DriverCommands { commands: Vec<DriverCommand> },
    Keymaps { keymaps: Vec<KeymapEntry> },
    Permission { decision: PermissionDecision },
    /// What somebody said to an [`ApiCall::AskUser`]. `None` is "nobody answered".
    Answers {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answers: Option<Vec<QuestionAnswer>>,
    },
    PermissionMode { mode: PermissionMode },
    FocusedWin { win: Option<WindowId> },
    Option { entry: Option<OptionEntry> },
    Options { options: Vec<OptionEntry> },
    Paths { paths: Vec<String> },
    /// `None` until the frontend has drawn the window at least once.
    Viewport { viewport: Option<Viewport> },
    Sessions { sessions: Vec<SessionInfo> },
    Credentials { credentials: Vec<CredentialInfo> },
    Session { session: SessionInfo },
    Messages { messages: Vec<Message> },
    // ---- version control ----
    Status { status: RepoStatus },
    Branches { branches: Vec<BranchInfo> },
    Worktrees { worktrees: Vec<WorktreeInfo> },
    Commits { commits: Vec<CommitInfo> },
    Commit { commit: CommitInfo },
    Text { text: String },
    /// A value that may legitimately be absent, such as a default branch in a repository with no
    /// remote. Distinct from `Text { text: "" }`, which would be a real empty answer.
    MaybeText { text: Option<String> },
    Json {
        #[ts(type = "unknown")]
        value: serde_json::Value,
    },
    /// A JSON object, as `VarAll` answers. Distinct from `Json` so the TS side gets a map rather
    /// than `unknown` for the one call whose answer is always a bag of keys.
    Vars {
        #[ts(type = "Record<string, unknown>")]
        vars: serde_json::Map<String, serde_json::Value>,
    },
    Contributions { contributions: Vec<Contribution> },
    Windows { windows: Vec<WindowInfo> },
    Attachments { attachments: Vec<AttachmentInfo> },
    // ---- the swarm ----
    SwarmSelf { node: Option<NodeInfo> },
    SwarmNodes { nodes: Vec<SwarmNode> },
    SwarmAgents { agents: Vec<SwarmAgent> },
    SwarmStrangers { strangers: Vec<SwarmStranger> },
    // ---- the plan ----
    Quotas { quotas: Vec<QuotaSnapshot> },
    QuotaHistory { samples: Vec<QuotaSample> },
    UsageHistory { history: UsageHistory },
}

/// An image the composer is holding, as far as anything outside the host is concerned.
///
/// The size is in here because it is the part you cannot see for yourself: an attachment is a row
/// that says "png", and whether that row is forty kilobytes or four megabytes is the whole of what
/// you would want to know before sending it.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct AttachmentInfo {
    /// Where the bytes are, in the workspace's directory.
    pub path: String,
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`, read off the bytes.
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    /// On disk, after any shrinking.
    pub bytes: u32,
}

/// One piece of the status line.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct StatusSegment {
    pub text: String,
    /// The key that changes this, written the way the user would press it — `^P`, `^Z`.
    ///
    /// Beside what it acts on rather than in a list of its own, because a key is only memorable
    /// once you have seen it next to the thing it does. A separate legend is a second place to
    /// look, and the thing being looked up is right there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hl: Option<String>,
    #[serde(default)]
    pub align: StatusAlign,
    /// Lower sorts first within its side. Ties break by key, so the order is stable across redraws
    /// rather than depending on which plugin happened to load first.
    #[serde(default)]
    pub priority: i32,
}

/// One shortcut, as it reads on the row under the composer.
///
/// Split into the key and what it does rather than one pre-formatted string, because the row has to
/// survive a narrow terminal: labels are dropped before keys are, and that decision cannot be made
/// once the two have been glued together. The key is written the way the user would press it —
/// `^P`, `⏎`, `esc` — which is the frontend's spelling of a binding, not the core's `<C-p>`.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct Hint {
    pub keys: String,
    pub label: String,
    /// Lower sorts first. Ties break by key, so the row does not reshuffle when a plugin reloads.
    #[serde(default)]
    pub priority: i32,
}

#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum StatusAlign {
    #[default]
    Left,
    Right,
}

/// A window's resolved geometry.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct Viewport {
    pub width: u16,
    pub height: u16,
    pub top_line: u32,
    /// Lines in the buffer, so a caller can page without a second round trip.
    pub line_count: u32,
}

/// A registered command, as listed for a palette or `:help`-style listing.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct CommandEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    /// Which plugin owns it, so a palette can group or attribute entries.
    pub plugin: String,
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct KeymapEntry {
    pub mode: Mode,
    pub lhs: String,
    pub command: String,
    pub scope: KeymapScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
}

/// Why a call failed.
///
/// Distinguishing these matters: a plugin should retry a `Busy`, surface a `Denied` to the user,
/// and treat `InvalidArgument` as its own bug.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum ApiError {
    /// The referenced buffer/window/mark does not exist, or was closed.
    NotFound { what: String },
    InvalidArgument { message: String },
    /// A permission check or a blocking hook refused.
    Denied { reason: String },
    /// The plugin lacks a capability it did not declare in its manifest.
    NotPermitted { capability: String },
    Busy { message: String },
    Internal { message: String },
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { what } => write!(f, "not found: {what}"),
            Self::InvalidArgument { message } => write!(f, "invalid argument: {message}"),
            Self::Denied { reason } => write!(f, "denied: {reason}"),
            Self::NotPermitted { capability } => write!(f, "not permitted: {capability}"),
            Self::Busy { message } => write!(f, "busy: {message}"),
            Self::Internal { message } => write!(f, "internal error: {message}"),
        }
    }
}

impl std::error::Error for ApiError {}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "status", rename_all = "snake_case")]
#[ts(export)]
pub enum ApiResponse {
    Ok { value: ApiOk },
    Err { error: ApiError },
}

impl From<Result<ApiOk, ApiError>> for ApiResponse {
    fn from(r: Result<ApiOk, ApiError>) -> Self {
        match r {
            Ok(value) => Self::Ok { value },
            Err(error) => Self::Err { error },
        }
    }
}

/// Re-exported here so `api` is a complete description of the plugin-facing surface.
pub use crate::agent::HookOutcome;

/// Convenience alias used across the host.
pub type ApiResult = Result<ApiOk, ApiError>;

/// Identifies which hook payload a plugin is being asked about, paired with a request id so the
/// host can match the answer.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct HookInvocation {
    pub request: RequestId,
    pub hook: HookName,
    pub payload: HookPayload,
}

/// A tool the host wants the plugin to run.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct ToolInvocation {
    pub request: RequestId,
    pub name: String,
    #[ts(type = "unknown")]
    pub input: serde_json::Value,
}

/// Result of a plugin-run tool.
pub type ToolInvocationResult = ToolResult;

/// A key the host has already routed to this plugin's command. Included so a plugin can implement
/// modal behaviour without re-deriving routing.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct KeyContext {
    pub key: KeyPress,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub win: Option<WindowId>,
}
