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
    Capability, HookName, HookPayload, Message, PermissionDecision, PermissionMode, SessionInfo,
    ToolDef, ToolResult,
};
use crate::ids::{
    BufferId, ExtmarkId, NamespaceId, RequestId, SessionId, StreamId, SurfaceId, WindowId,
};
use crate::options::{OptionEntry, OptionSpec, OptionValue};
use crate::provider::{
    CredentialInfo, DriverKind, InstanceConfig, InstanceId, ModelEntry, ModelSelection,
    ProviderEvent,
};
use crate::ui::{
    CursorMotion, ExtmarkInfo, ExtmarkOpts, FloatConfig, HighlightDef, KeyPress, MessageLevel,
    Rect, SurfaceCell, TextEdit, WindowLayout,
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

/// Scope a keymap applies at. Resolution order is float/window -> buffer -> global; the first
/// binding found wins, which is what stops every plugin fighting over `<Esc>`.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum KeymapScope {
    Global,
    Buffer { buf: BufferId },
    Window { win: WindowId },
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
    /// Append to the final line without resending it. The streaming fast path.
    BufAppendText {
        buf: BufferId,
        text: String,
    },
    BufSetName {
        buf: BufferId,
        name: String,
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
    WinScrollTo {
        win: WindowId,
        top_line: u32,
    },

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
    },
    GitWorktrees,
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
    GitAddWorktree {
        path: String,
        branch: String,
        #[serde(default)]
        create: bool,
    },
    GitRemoveWorktree {
        path: String,
        #[serde(default)]
        force: bool,
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
    Keymaps { keymaps: Vec<KeymapEntry> },
    Permission { decision: PermissionDecision },
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
}

/// One piece of the status line.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct StatusSegment {
    pub text: String,
    /// The key that changes this, written the way the user would press it — `^P`, `F1`.
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
