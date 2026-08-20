//! The host: one loop that owns the core and routes everything else.
//!
//! Four things arrive asynchronously — plugin messages, agent events, frontend input, and a redraw
//! deadline — and exactly one task applies them to [`Editor`]. That single-writer arrangement is
//! what makes ordering well-defined when two plugins mutate the same buffer, without a lock
//! anywhere in the core.
//!
//! Redraws are coalesced, not scheduled. The first mutation arms a ~16 ms deadline; everything
//! until then lands in the same batch. An idle session arms nothing and blocks in `select!`, so it
//! costs no CPU, and a burst of streamed tokens produces one frame rather than one per token.

use std::sync::Arc;
use std::time::Duration;

use neosh_agent::{Agent, AgentEvent, PluginBridge};
use neosh_core::{CoreEffect, Editor};
use neosh_proto::{
    ApiCall, ApiError, ApiOk, ApiResponse, ApiResult, BufferId, CursorMotion, Dock, Gravity,
    InputEvent,
    KeyCode, MessageLevel, Mode, OptionSpec, OptionType, OptionValue, PluginEvent, PluginId,
    PluginInbound, PluginOutbound, PluginResponse, RequestId, TextEdit, UiEvent, VirtTextPos,
    WindowId, WindowLayout,
};
use neosh_provider::catalog;
use neosh_script::{ScriptInbound, ScriptOutbound};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation;

use crate::bridge::{PluginProvider, ScriptBridge};
use crate::config::{Config, Resolved};
use crate::frontend::Frontend;
use crate::services::Services;

/// How long mutations accumulate before a frame is emitted.
const COALESCE: Duration = Duration::from_millis(16);

/// The plugin id the host registers its own things under.
///
/// Not a special case in the core: the built-in UI, commands, keymaps and options are all owned by
/// a plugin that happens to be the host. If any of them needed a path around the registry, the API
/// would be missing something.
const BUILTIN: &str = "neosh";

/// What the host is streaming into the chat buffer right now.
///
/// Distinguishing the two matters because switching between them has to break the line; appending
/// reasoning onto the end of a sentence of prose would be unreadable.
#[derive(Copy, Clone, PartialEq)]
enum Stream {
    Text,
    Thinking,
}

/// One answer being written into the transcript.
///
/// The rows it occupies are `start .. start + rows`, and nothing else ever writes inside that
/// range: everything appends after it, and the working line is gone by the time the first token
/// arrives. That is what lets a patch be applied as a range replacement rather than by finding the
/// text again — which is the same reasoning as the working line having no row index, arrived at the
/// other way round.
struct Answer {
    md: crate::markdown::Md,
    start: u32,
    rows: u32,
}

/// Startup, as a state machine.
///
/// Config runs before plugin discovery, so `init.ts` gets to decide what else loads — the same
/// relationship `init.lua` has with a Neovim plugin manager. Activation is asynchronous, so the
/// remaining steps hang off the `Loaded` message rather than running straight through.
#[derive(Default)]
struct Startup {
    /// Config scripts still to activate, innermost last.
    scripts: std::collections::VecDeque<(PluginId, std::path::PathBuf)>,
    /// The config script currently activating.
    active: Option<PluginId>,
    /// Directories to search once config has had its say.
    plugin_dirs: Vec<std::path::PathBuf>,
    /// Applied after config, so an explicit flag beats a config file.
    cli_model: Option<String>,
    /// `--plugin-dir`, kept so a reload does not silently drop it.
    cli_plugin_dirs: Vec<std::path::PathBuf>,
    /// Options naming something not declared yet — almost always a plugin's own setting. Retried
    /// each time a plugin finishes loading.
    deferred: Vec<(String, OptionValue)>,
    /// Plugin activations still outstanding, so we know when to give up on `deferred`.
    loading: usize,
    /// True once discovery has run; `rtp.add` after this point cannot take effect.
    discovered: bool,
    /// `--clean`: do not load the plugins embedded in the binary either.
    no_builtin_plugins: bool,
    /// Bumped on every reload and appended to entry URLs as `?v=N`.
    ///
    /// deno_core caches modules by URL, so without this a reload would dutifully re-import the
    /// same file and re-run the *previous* code — a reload that reports success and changes
    /// nothing, which is worse than no reload at all.
    generation: u32,
    /// Re-reads every config layer from disk. Owned by the caller, because the trust store and the
    /// XDG paths are not the host's business.
    reload: Option<Box<dyn Fn() -> anyhow::Result<Resolved>>>,
    /// An `agent.model` naming an instance nothing serves *yet*.
    ///
    /// Provider instances can come from plugins, and plugins load after configuration is applied.
    /// Reporting "no provider serves lab/brainy" at the moment the option is set would be wrong for
    /// the entirely ordinary case of a config selecting a model that a plugin is about to register.
    /// Held and retried, then reported only if it still does not resolve once everything has loaded
    /// — the same shape as a held option value.
    pending_model: Option<String>,
    /// A reload asked for while something was still loading.
    ///
    /// Queued rather than refused. Startup is asynchronous and now involves several bundled plugins,
    /// so "still loading, try again" is a real window a user can land in by pressing `<C-r>` shortly
    /// after launch — and telling someone to press a key again is a worse answer than remembering
    /// that they pressed it.
    reload_pending: bool,
    /// Provider instances that exist before any configuration is applied — the built-in catalogue,
    /// or the mock in tests.
    ///
    /// Reapplying configuration rebuilds from this rather than adding to whatever is there, which
    /// is the only way an instance you *deleted* from `config.toml` disappears, and the only way an
    /// override reverts to the built-in when you remove it.
    base_instances: Vec<neosh_proto::InstanceConfig>,
}

/// Everything the host needs to start, and to start again.
pub struct Bootstrap {
    pub resolved: Resolved,
    /// Called again on `config.reload`, so a reload reads the files as they are now rather than
    /// replaying what was parsed at launch.
    pub reload: Box<dyn Fn() -> anyhow::Result<Resolved>>,
    pub cli_model: Option<String>,
    pub cli_plugin_dirs: Vec<std::path::PathBuf>,
}

pub struct Host {
    editor: Editor,
    agent: Arc<Agent>,
    bridge: Arc<ScriptBridge>,
    frontend: Box<dyn Frontend>,

    chat: BufferId,
    chat_win: WindowId,
    /// Where the transcript is scrolled to, as the host intends it.
    ///
    /// Not read back from the frontend's report: two page-ups inside one 16 ms coalescing window
    /// would both compute from the same stale value and the second would do nothing. The frontend
    /// reports what it drew; this is what we asked for.
    chat_top: u32,
    composer: BufferId,
    composer_win: WindowId,
    /// Marks for the chrome around the composer: the prompt, the placeholder, the shortcut row.
    /// Its own namespace so redrawing it cannot disturb anything else on that buffer.
    composer_ns: neosh_proto::NamespaceId,
    /// What the row under the composer says, keyed by whoever put it there.
    hints: std::collections::BTreeMap<String, neosh_proto::Hint>,
    /// How many leading transcript rows the welcome block last occupied, so redrawing it replaces
    /// its own lines and a conversation is recognisable as "not that".
    welcome_rows: usize,
    /// Whether `agent.model` chose the current selection. A chosen model is never silently
    /// replaced; a remembered one is.
    selection_pinned: bool,
    /// ACP drivers, kept so the permission mode can be mirrored into them.
    acp_drivers: Vec<Arc<neosh_provider::drivers::AcpProvider>>,
    status: BufferId,
    status_ns: neosh_proto::NamespaceId,
    /// Where the transcript's own highlights live.
    ///
    /// Separate from the status line's so that redrawing the working line — which happens several
    /// times a second — cannot clear a mark on a message from ten turns ago.
    chat_ns: neosh_proto::NamespaceId,
    /// The row the working line is on while a turn is in flight, and what it says.
    ///
    /// Held rather than searched for: the transcript grows underneath it as tools report in, and
    /// scanning for "the line that looks like a spinner" is how you end up rewriting a message.
    working: Option<Working>,
    /// What plugins put in the status line, by key.
    ///
    /// A `BTreeMap` so iteration order is deterministic; the render sorts by priority anyway, but a
    /// stable base ordering is what stops two segments with equal priority swapping places between
    /// redraws.
    status_segments: std::collections::BTreeMap<String, neosh_proto::StatusSegment>,

    active_turn: Option<CancellationToken>,
    /// Set while a multi-key sequence is half typed, so the loop knows to arm the timeout that
    /// eventually gives up on it.
    keys_pending: bool,
    /// A key was just fed, so the sequence timeout needs restarting.
    ///
    /// Every key restarts it, as in Neovim: the wait is for *the next* key, not for the whole
    /// sequence. Arming once at the start of a chord gives you less time for each key you type,
    /// which is exactly backwards.
    keys_touched: bool,
    /// Whether the keyboard is in the transcript rather than the composer.
    ///
    /// A place, not a focus: `j` moves there and types here, and one flag is a great deal easier to
    /// reason about than inferring intent from which window happens to be focused.
    reading: bool,
    /// Set while output is streaming, so chunks append rather than starting a new line.
    streaming: Option<Stream>,
    /// The answer currently being written, and where it sits in the transcript.
    answer: Option<Answer>,
    startup: Startup,
    /// Set by the `quit` command; the run loop notices and shuts down cleanly.
    quitting: bool,
    /// When `<C-c>` was last pressed with nothing left to cancel.
    ///
    /// Quitting on the first press would be wrong: `<C-c>` is muscle memory, and in a workspace
    /// holding an unsent draft the cost of getting it wrong is losing that draft. Quitting only on
    /// a *second* press within a couple of seconds is the pattern people already know.
    quit_armed: Option<std::time::Instant>,
    /// Driver kinds each plugin registered, so unloading takes them with it.
    plugin_drivers: std::collections::HashMap<PluginId, Vec<neosh_proto::DriverKind>>,
    /// What each loaded plugin declared in its manifest.
    ///
    /// Consulted for capabilities enforced per plugin rather than per model action — repository
    /// writes today. Cleared and rebuilt on reload, so removing a permission from a manifest takes
    /// it away rather than leaving the previous grant in place.
    plugin_permissions: std::collections::HashMap<PluginId, Vec<neosh_proto::PluginPermission>>,
    /// Models discovered from each endpoint, for the life of the session. Cleared on reload,
    /// because a reload may have added, removed or repointed an instance.
    model_cache: crate::services::ModelCache,
    /// Where conversations are saved. `None` under `--clean`, which reads and writes nothing.
    state_dir: Option<std::path::PathBuf>,
    /// What plugins remember between runs: pinned projects, fold state, whatever a panel needs to
    /// still be arranged the way you left it.
    pstate: crate::pstate::PluginState,
    /// Repositories by directory, discovered on demand.
    ///
    /// Keyed rather than singular because the *conversation* decides which repository is in play:
    /// a session opened in another checkout — or another worktree of the same one — must report
    /// that tree's branch and changes, or "switch to my other worktree" means nothing. Cached
    /// because `git rev-parse` on every status refresh is a subprocess per keystroke, and a
    /// directory does not stop being a repository while we are looking at it.
    repos: std::collections::HashMap<std::path::PathBuf, Option<neosh_vcs::Git>>,
    /// Where neosh was started, and the fallback for a session that names nowhere.
    cwd: std::path::PathBuf,
    /// A key being typed in, if one is.
    ///
    /// The host collects this itself rather than lending a plugin a text field, because a plugin
    /// field is a buffer, a buffer is drawn, and drawing it would put the key in a `UiEvent` — over
    /// a process boundary, into whatever the frontend logs. Here the value never leaves this
    /// struct: what gets drawn is a row of bullets.
    secret: Option<SecretPrompt>,
}

/// What a tool call is about, in a few words, for the row that announces it.
///
/// Best-effort by design: tool inputs are whatever schema the tool declared, and guessing wrongly
/// costs nothing because the name is already there. The keys tried are the ones every file and
/// shell tool in practice uses.
fn tool_subject(call: &neosh_proto::ToolCall) -> String {
    subject_of(&call.name, &call.input)
}

/// One line saying what a tool call came back with.
///
/// The *shape* of the result, not the result: a card that inlined four hundred lines of file would
/// bury the answer it was gathered for. What is useful at a glance is "it worked, and roughly how
/// much came back", with the exception of an error — where the first line is the whole point.
fn tool_summary(result: &neosh_proto::ToolResult) -> String {
    let text = result.content.trim();
    if text.is_empty() {
        return if result.is_error { "failed".into() } else { "done".into() };
    }
    let first = text.lines().next().unwrap_or("").trim();
    let lines = text.lines().count();
    // Errors say what went wrong; successes say how much there is, once there is more than a line
    // of it. By character, not byte: slicing a multi-byte boundary panics.
    let clipped: String = first.chars().take(96).collect();
    let ellipsis = if first.chars().count() > 96 { "\u{2026}" } else { "" };
    if result.is_error || lines <= 1 {
        format!("{clipped}{ellipsis}")
    } else {
        format!("{clipped}{ellipsis}  ({lines} lines)")
    }
}

/// What a tool call is *about*, from whichever of its arguments says so.
fn subject_of(_name: &str, input: &serde_json::Value) -> String {
    const KEYS: &[&str] = &["path", "file_path", "file", "pattern", "query", "command", "url"];
    let Some(map) = input.as_object() else { return String::new() };
    for key in KEYS {
        if let Some(v) = map.get(*key).and_then(|v| v.as_str()) {
            let one = v.lines().next().unwrap_or(v).trim();
            if one.is_empty() {
                continue;
            }
            // By character, not byte: slicing a multi-byte boundary panics, and the frontend
            // measures display width anyway.
            let clipped: String = one.chars().take(48).collect();
            let ellipsis = if one.chars().count() > 48 { "\u{2026}" } else { "" };
            return format!("  {clipped}{ellipsis}");
        }
    }
    String::new()
}

/// The line that says a turn is in flight.
///
/// It exists because the gap between pressing Enter and the first token is the longest silence in
/// the program, and a still screen and a wedged process look identical. It carries an elapsed
/// clock for the same reason: "working" with no number is indistinguishable from "stuck".
struct Working {
    /// The word this turn is using. Chosen once per turn — a verb that changes every tick is a
    /// slot machine, and you end up watching it instead of thinking.
    verb: &'static str,
    started: std::time::Instant,
    /// What it is waiting on, when that is something other than the model.
    note: Option<String>,
}

// Deliberately no row index. The working line is *always the last line of the transcript* and
// everything else inserts above it, which makes "where is it" a question with one answer instead of
// a number to keep in step with four different append paths. The first version tracked a row and
// went wrong under load, in the one way that is hard to see: a stale index deletes the wrong line.

// A token counter belongs here too, and is deliberately absent: usage is only reported at the end
// of a turn, and a count derived from characters would be a number that looks measured and is not.

/// Words a turn can be doing.
///
/// Deliberately a small list of ordinary present participles. The point is a word that reads as
/// activity without claiming anything about what the model is actually doing, which nobody knows.
const VERBS: &[&str] = &[
    "Thinking", "Working", "Pondering", "Considering", "Reasoning", "Puzzling", "Weighing",
    "Mulling", "Deliberating", "Reckoning", "Chewing", "Turning it over",
];

/// A key being typed in, and who is waiting to hear how it went.
struct SecretPrompt {
    instance: neosh_proto::InstanceId,
    label: String,
    /// What has been typed. Zeroed when the prompt closes, so it does not sit in freed memory.
    value: String,
    /// The draft that was in the composer, put back afterwards. Borrowing the composer costs
    /// nothing as long as it is given back.
    draft: String,
    /// The call to answer when this resolves, if a plugin asked for it.
    waiting: Option<(PluginId, RequestId)>,
}

impl Drop for SecretPrompt {
    fn drop(&mut self) {
        use secrecy::zeroize::Zeroize;
        self.value.zeroize();
    }
}

impl Host {
    pub fn new(
        agent: Arc<Agent>,
        bridge: Arc<ScriptBridge>,
        frontend: Box<dyn Frontend>,
    ) -> Self {
        let mut editor = Editor::new();

        let chat = editor.create_buffer("[chat]");
        let chat_win =
            editor.open_window(chat, WindowLayout::Docked {
                dock: Dock::Main,
                size: None,
                // A conversation settles against the field you answer it in. Anchored to the top,
                // a two-line exchange floats at the ceiling with a screen of nothing under it, and
                // the eye has to travel the whole window between what was said and where you say
                // the next thing.
                gravity: Gravity::End,
            });

        // Docked before the composer, so it takes the bottom-most row and the composer sits above
        // it. Without a status line the startup screen renders literally nothing — two empty
        // buffers — and "it opened an empty terminal" is indistinguishable from "it crashed".
        let status = editor.create_buffer("[status]");
        editor.open_window(status, WindowLayout::Docked { dock: Dock::Bottom, size: Some(1), gravity: Gravity::Start });

        let composer = editor.create_buffer("[composer]");
        // Four rows: a rule, up to three lines of what you are writing, and the shortcut row that
        // lives on the last of them as a virtual line. The rule is what makes the composer read as
        // a field rather than as the last paragraph of the transcript.
        let composer_win =
            editor.open_window(composer, WindowLayout::Docked { dock: Dock::Bottom, size: Some(4), gravity: Gravity::Start });
        editor.set_mode(Mode::Chat);

        let mut namespace = |name: &str| {
            match editor.apply(&PluginId::from(BUILTIN), ApiCall::NsCreate { name: name.into() }) {
                Ok(ApiOk::Ns { ns }) => ns,
                _ => neosh_proto::NamespaceId(0),
            }
        };
        let status_ns = namespace("neosh.status");
        let chat_ns = namespace("neosh.chat");
        let composer_ns = namespace("neosh.composer");

        Self {
            editor,
            agent,
            bridge,
            frontend,
            chat,
            chat_win,
            chat_top: 0,
            composer,
            composer_win,
            status,
            status_ns,
            chat_ns,
            composer_ns,
            working: None,
            status_segments: Default::default(),
            hints: Default::default(),
            welcome_rows: 0,
            selection_pinned: false,
            acp_drivers: Vec::new(),
            active_turn: None,
            keys_pending: false,
            keys_touched: false,
            reading: false,
            streaming: None,
            answer: None,
            startup: Startup::default(),
            quitting: false,
            quit_armed: None,
            plugin_drivers: Default::default(),
            plugin_permissions: Default::default(),
            model_cache: Default::default(),
            state_dir: None,
            pstate: crate::pstate::PluginState::new(None),
            repos: Default::default(),
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            secret: None,
        }
    }

    /// Everything a spawned call needs, snapshotted at the moment of the call.
    ///
    /// `gen.model` is read here rather than stored, so changing it in `config.toml` and reloading
    /// takes effect on the next generation without the host caching a stale copy.
    fn services(&self, plugin: &PluginId) -> Services {
        // The active conversation's directory, so git answers about the tree you are working in.
        let cwd = {
            let store = self.agent.sessions();
            store.active().cwd.clone()
        };
        let git = self.repos.get(&cwd).cloned().flatten();
        Services {
            agent: self.agent.clone(),
            git,
            cwd,
            gen_model: self.editor.options().str("gen.model").unwrap_or_default().to_string(),
            may_write_vcs: self
                .plugin_permissions
                .get(plugin)
                .is_some_and(|p| p.contains(&neosh_proto::PluginPermission::VcsWrite)),
            caller: plugin.0.clone(),
            bridge: self.bridge.clone(),
            model_cache: self.model_cache.clone(),
        }
    }

    /// Run a slow call off the host loop, answering the plugin when it finishes.
    ///
    /// The host loop is the single writer for the editor. Awaiting `git status` inside it freezes
    /// typing for the length of a subprocess; awaiting a model round-trip freezes it for seconds.
    /// Returning `true` means this call has been taken over and must not also be dispatched.
    fn spawn_slow(&mut self, plugin: &PluginId, id: Option<RequestId>, call: &ApiCall) -> bool {
        let svc = self.services(plugin);
        let plugin = plugin.clone();
        let call = call.clone();
        if !matches!(
            call,
            ApiCall::GitStatus
                | ApiCall::GitBranches { .. }
                | ApiCall::GitWorktrees
                | ApiCall::GitLog { .. }
                | ApiCall::GitDiff { .. }
                | ApiCall::GitDefaultBranch
                | ApiCall::GitCreateBranch { .. }
                | ApiCall::GitCheckout { .. }
                | ApiCall::GitStage { .. }
                | ApiCall::GitUnstage { .. }
                | ApiCall::GitCommit { .. }
                | ApiCall::GitAddWorktree { .. }
                | ApiCall::GitRemoveWorktree { .. }
                | ApiCall::GenComplete { .. }
                | ApiCall::AgentListModels { .. }
                | ApiCall::PermissionCheck { .. }
                | ApiCall::PathComplete { .. }
        ) {
            return false;
        }
        let bridge = self.bridge.clone();
        tokio::spawn(async move {
            let result = run_slow(svc, call).await;
            match id {
                Some(id) => {
                    let response: ApiResponse = result.into();
                    let _ = bridge.script().send(ScriptInbound::Plugin {
                        plugin,
                        msg: PluginInbound::Response { id, response },
                    });
                }
                // A notify has nowhere to return an error, so surface it rather than losing it.
                None => {
                    if let Err(e) = result {
                        tracing::warn!(%plugin, "{e}");
                    }
                }
            }
        });
        true
    }

    fn chat_line(&mut self, text: impl Into<String>) {
        // Through the same insertion point as everything else, so a line written mid-turn lands
        // above the working line rather than under it.
        self.chat_push(vec![text.into()]);
    }

    fn composer_text(&self) -> String {
        self.editor
            .buffer(self.composer)
            .map(|b| b.get_lines(0, b.line_count()).join("\n"))
            .unwrap_or_default()
    }

    fn set_composer(&mut self, text: &str) {
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.composer,
            start: 0,
            end: -1,
            lines: vec![text.to_string()],
        });
        // Keep the caret at the end of what has been typed. Columns are UTF-8 byte offsets, so this
        // is the byte length — not the character count, which would put the caret inside a
        // multi-byte character the moment anyone types anything but ASCII.
        let last = text.lines().count().saturating_sub(1) as u32;
        let col = text.rsplit('\n').next().unwrap_or("").len() as u32;
        let _ = self.editor.apply(&plugin, ApiCall::WinSetCursor {
            win: self.composer_win,
            row: last,
            col,
        });
        // A selection over text that no longer exists is a highlight in mid-air.
        let _ = self
            .editor
            .apply(&plugin, ApiCall::WinSelect { win: self.composer_win, on: false });
        // The placeholder appears and disappears with the text, so the chrome has to follow every
        // write and not just every keystroke.
        self.refresh_composer();
    }

    /// Route a plugin call: UI-domain calls to the core, agent-domain calls to the agent layer.
    async fn dispatch(&mut self, plugin: &PluginId, call: ApiCall) -> ApiResult {
        if Editor::handles(&call) {
            let r = self.editor.apply(plugin, call);
            self.drain_effects();
            return r;
        }
        self.agent_call(plugin, call).await
    }

    async fn agent_call(&mut self, plugin: &PluginId, call: ApiCall) -> ApiResult {
        match call {
            ApiCall::AgentSend { text } => {
                self.start_turn(text);
                Ok(ApiOk::Unit)
            }
            ApiCall::AgentCancel => {
                if let Some(t) = self.active_turn.take() {
                    t.cancel();
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::AgentGetSelection => {
                Ok(ApiOk::Selection { selection: self.agent.selection() })
            }
            ApiCall::AgentSetSelection { selection } => {
                if self.agent.providers().resolve(&selection).is_none() {
                    return Err(ApiError::NotFound {
                        what: format!("no driver serves instance {}", selection.instance),
                    });
                }
                self.agent.set_selection(selection);
                Ok(ApiOk::Unit)
            }
            ApiCall::AgentListInstances => Ok(ApiOk::Instances {
                instances: self.agent.providers().usable_instances().cloned().collect(),
            }),
            // ---- plugin state ------------------------------------------
            // Keyed by the caller, which the plugin does not get to choose. Cheap enough to answer
            // on the host loop: reads come from memory after the first touch, and a write is one
            // small file that a panel only produces when you press a key.
            ApiCall::StateGet { key } => {
                Ok(ApiOk::Json { value: self.pstate.get(&plugin.0, &key) })
            }
            ApiCall::StateSet { key, value } => {
                self.pstate.set(&plugin.0, key, value);
                Ok(ApiOk::Unit)
            }
            ApiCall::StateDelete { key } => {
                self.pstate.delete(&plugin.0, &key);
                Ok(ApiOk::Unit)
            }
            // ---- sessions ----------------------------------------------
            ApiCall::SessionList { include_archived } => Ok(ApiOk::Sessions {
                sessions: self.agent.sessions().list_with(include_archived),
            }),
            ApiCall::SessionCurrent => {
                let info = {
                    let store = self.agent.sessions();
                    let mut i = store.active().info();
                    i.is_active = true;
                    i
                };
                Ok(ApiOk::Session { session: info })
            }
            ApiCall::SessionNew { cwd, title, activate } => {
                let cwd = cwd.map(std::path::PathBuf::from).unwrap_or_else(|| self.cwd.clone());
                // Canonicalised so two names for one directory do not become two projects.
                let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
                if !cwd.is_dir() {
                    return Err(ApiError::NotFound { what: format!("directory {}", cwd.display()) });
                }
                self.remember_repo(cwd.clone()).await;
                let mut session = neosh_agent::Session::new(cwd);
                session.title = title.filter(|t| !t.trim().is_empty());
                session.created_at = now_secs();
                session.updated_at = session.created_at;
                // A new conversation inherits the model you are using. Starting it on whatever the
                // catalogue lists first would silently change what you are talking to.
                session.selection = self.agent.selection();
                session.system = self.agent.session().system.clone();
                let id = session.id.clone();

                let info = {
                    let mut store = self.agent.sessions();
                    store.insert(session);
                    if activate {
                        store.switch(&id).map_err(|e| ApiError::NotFound { what: e.to_string() })?;
                    }
                    // The session was just inserted, so this cannot miss; the `?` is here rather
                    // than an unwrap because a future refactor could make it miss.
                    let mut i = store
                        .get(&id)
                        .map(|s| s.info())
                        .ok_or_else(|| ApiError::Internal { message: "session vanished".into() })?;
                    i.is_active = store.active_id() == &id;
                    i
                };
                if activate {
                    self.enter_session();
                }
                self.persist_sessions();
                Ok(ApiOk::Session { session: info })
            }
            ApiCall::SessionSwitch { session } => {
                if self.active_turn.is_some() {
                    // The turn writes into the chat buffer as it streams; switching underneath it
                    // would interleave one conversation's output into another's transcript.
                    return Err(ApiError::Busy {
                        message: "a turn is running — interrupt it first".into(),
                    });
                }
                self.agent
                    .sessions()
                    .switch(&session)
                    .map_err(|e| ApiError::NotFound { what: e.to_string() })?;
                let cwd = { self.agent.sessions().active().cwd.clone() };
                self.remember_repo(cwd).await;
                self.enter_session();
                Ok(ApiOk::Unit)
            }
            ApiCall::SessionClose { session } => {
                let closing_active = self.agent.sessions().active_id() == &session;
                if closing_active && self.active_turn.is_some() {
                    return Err(ApiError::Busy {
                        message: "a turn is running — interrupt it first".into(),
                    });
                }
                self.agent.sessions().remove(&session).map_err(|e| match e {
                    neosh_agent::store::StoreError::LastSession => {
                        ApiError::InvalidArgument { message: e.to_string() }
                    }
                    other => ApiError::NotFound { what: other.to_string() },
                })?;
                if let Some(dir) = &self.state_dir {
                    crate::sessions::forget(dir, &session);
                }
                if closing_active {
                    self.enter_session();
                }
                self.persist_sessions();
                Ok(ApiOk::Unit)
            }
            ApiCall::SessionArchive { session, archived } => {
                let leaving = archived && self.agent.sessions().active_id() == &session;
                if leaving && self.active_turn.is_some() {
                    return Err(ApiError::Busy {
                        message: "a turn is running — interrupt it first".into(),
                    });
                }
                // Archiving the only conversation you have is a reasonable thing to want, and
                // "there is nowhere to go" is a bad answer to it. Somewhere to go is one line.
                if leaving && self.agent.sessions().list().len() <= 1 {
                    self.new_session_in(self.cwd.clone());
                }
                let at = now_secs();
                self.agent.sessions().archive(&session, archived, at).map_err(|e| match e {
                    neosh_agent::store::StoreError::LastSession => {
                        ApiError::InvalidArgument { message: e.to_string() }
                    }
                    other => ApiError::NotFound { what: other.to_string() },
                })?;
                if leaving {
                    self.enter_session();
                }
                self.persist_sessions();
                Ok(ApiOk::Unit)
            }
            // ---- credentials -------------------------------------------
            // The list says where each key comes from and never what it is; `ProviderSetCredential`
            // is answered elsewhere, because the host has to read the keyboard before it can reply.
            ApiCall::ProviderCredentials => {
                Ok(ApiOk::Credentials { credentials: self.agent.providers().credentials() })
            }
            ApiCall::ProviderForgetCredential { instance } => {
                neosh_provider::credentials::credentials().forget(&instance);
                self.editor_message(MessageLevel::Info, format!("forgot the key for {instance}"));
                Ok(ApiOk::Unit)
            }
            ApiCall::ProviderSetCredential { .. } => Err(ApiError::Internal {
                message: "credential prompts are answered by the host loop".into(),
            }),
            ApiCall::SessionRename { session, title } => {
                self.agent
                    .sessions()
                    .rename(&session, title)
                    .map_err(|e| ApiError::NotFound { what: e.to_string() })?;
                self.refresh_status();
                self.persist_sessions();
                Ok(ApiOk::Unit)
            }
            ApiCall::SessionMessages { session } => {
                let store = self.agent.sessions();
                let target = match &session {
                    Some(id) => store.get(id),
                    None => Some(store.active()),
                };
                match target {
                    Some(s) => Ok(ApiOk::Messages { messages: s.messages.clone() }),
                    None => Err(ApiError::NotFound {
                        what: format!("session {}", session.map(|s| s.0).unwrap_or_default()),
                    }),
                }
            }

            ApiCall::PermissionGetMode => {
                Ok(ApiOk::PermissionMode { mode: self.agent.permissions().mode() })
            }
            ApiCall::PermissionSetMode { mode } => {
                // For this session only. A mode you switched on to get through one task should not
                // still be on next week, and writing it to a file is how that happens.
                let layer = (*self.agent.permissions()).clone().with_mode(mode);
                self.agent.set_permissions(layer);
                self.sync_acp_mode();
                self.refresh_status();
                Ok(ApiOk::PermissionMode { mode })
            }
            ApiCall::HintSet { key, hint } => {
                self.hints.insert(format!("{plugin}:{key}"), hint);
                self.refresh_composer();
                Ok(ApiOk::Unit)
            }
            ApiCall::HintClear { key } => {
                self.hints.remove(&format!("{plugin}:{key}"));
                self.refresh_composer();
                Ok(ApiOk::Unit)
            }

            ApiCall::StatusSet { key, segment } => {
                // Namespaced by plugin, so two plugins choosing the same key cannot fight and
                // unloading one takes only its own segments.
                self.status_segments.insert(format!("{plugin}:{key}"), segment);
                self.refresh_status();
                Ok(ApiOk::Unit)
            }
            ApiCall::StatusClear { key } => {
                self.status_segments.remove(&format!("{plugin}:{key}"));
                self.refresh_status();
                Ok(ApiOk::Unit)
            }

            ApiCall::ToolRegister { def } => {
                self.agent.tools_mut().register_plugin(plugin, def)?;
                Ok(ApiOk::Unit)
            }
            ApiCall::ToolUnregister { name } => {
                self.agent.tools_mut().unregister(plugin, &name);
                Ok(ApiOk::Unit)
            }
            ApiCall::ToolList => Ok(ApiOk::Tools { tools: self.agent.tools().defs() }),
            ApiCall::HookRegister { hook, blocking, timeout_ms } => {
                self.agent.hooks_mut().register(hook, plugin, blocking, timeout_ms);
                Ok(ApiOk::Unit)
            }
            ApiCall::HookUnregister { hook } => {
                self.agent.hooks_mut().unregister(hook, plugin);
                Ok(ApiOk::Unit)
            }
            ApiCall::ProviderRegisterDriver { driver, instances } => {
                let provider =
                    Arc::new(PluginProvider::new(driver.clone(), plugin.clone(), self.bridge.clone()));
                {
                    let mut reg = self.agent.providers_mut();
                    reg.register_driver(provider);
                    for i in instances {
                        reg.add_instance(i);
                    }
                }
                // Remembered so unloading the plugin takes its driver with it. A driver whose
                // plugin is gone answers every request with a dead channel.
                self.plugin_drivers.entry(plugin.clone()).or_default().push(driver);
                Ok(ApiOk::Unit)
            }
            ApiCall::ProviderEmit { stream, event } => {
                self.bridge.emit_provider(&stream, event);
                Ok(ApiOk::Unit)
            }

            ApiCall::RtpAdd { path } => {
                let expanded =
                    std::path::PathBuf::from(shellexpand::tilde(&path).into_owned());
                if self.startup.discovered {
                    // Silently ignoring this would leave the user with a plugin that never loads
                    // and no reason why.
                    return Err(ApiError::InvalidArgument {
                        message: format!(
                            "rtp.add({path:?}) came after plugin discovery; call it from your \
                             config, which runs first"
                        ),
                    });
                }
                if !self.startup.plugin_dirs.contains(&expanded) {
                    self.startup.plugin_dirs.push(expanded);
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::RtpList => Ok(ApiOk::Paths {
                paths: self
                    .startup
                    .plugin_dirs
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect(),
            }),
            other => Err(ApiError::Internal {
                message: format!("unroutable call: {other:?}"),
            }),
        }
    }

    fn start_turn(&mut self, text: String) {
        // Typing while it works is steering, not an error. The old behaviour — refuse, and make you
        // wait with the sentence already written — is the one moment in the program where you know
        // exactly what you want to say and cannot say it.
        if self.active_turn.is_some() {
            self.agent.steer(text);
            self.draw_working();
            self.refresh_composer();
            return;
        }
        let token = CancellationToken::new();
        self.active_turn = Some(token.clone());
        // Stamped here rather than inside the agent so `Session` stays clock-free and testable.
        // The turn id itself is set by the agent a moment later; a list only reads this while the
        // turn id is present, so the ordering is not observable.
        self.agent.session().turn_started_at = Some(now_secs());
        // Asking a question means you want to see the answer: scrolled-back state does not survive
        // sending, or the reply arrives somewhere off screen.
        self.scroll_chat(0);
        self.chat_question(&text);
        self.begin_working();

        let agent = self.agent.clone();
        let bridge = self.bridge.clone();
        tokio::spawn(async move {
            agent.run_turn_with(&*bridge, text, token).await;
        });
    }

    /// Start an empty conversation without switching to it.
    ///
    /// Exists so that archiving the only conversation you have has somewhere to land. It inherits
    /// the model and system prompt for the same reason a new conversation does: changing what you
    /// are talking to as a side effect of tidying up would be a strange thing to do.
    fn new_session_in(&mut self, cwd: std::path::PathBuf) {
        let mut session = neosh_agent::Session::new(cwd);
        session.created_at = now_secs();
        session.updated_at = session.created_at;
        session.selection = self.agent.selection();
        session.system = self.agent.session().system.clone();
        self.agent.sessions().insert(session);
    }

    fn editor_message(&mut self, level: MessageLevel, text: impl Into<String>) {
        let _ = self
            .editor
            .apply(&PluginId::from(BUILTIN), ApiCall::Notify { level, message: text.into() });
    }

    fn drain_effects(&mut self) {
        for effect in self.editor.drain_effects() {
            match effect {
                CoreEffect::InvokeCommand { plugin, name, args, key } => {
                    // The host registers its own commands through the same registry a plugin uses,
                    // so they are listable and rebindable like any other. It just answers them
                    // itself rather than sending them to a JS plugin that does not exist.
                    if plugin.0 == BUILTIN {
                        self.run_builtin(&name, args);
                    } else {
                        self.bridge.notify(&plugin, PluginEvent::CommandInvoked { name, args, key });
                    }
                }
                CoreEffect::UnhandledKey { key, mode } => self.handle_unbound_key(key, mode),
                CoreEffect::OptionChanged { name, value } => {
                    self.apply_option(&name, &value);
                    // Broadcast, not just to the owner: a setting is shared state, and the plugin
                    // that declared `ui.theme` is rarely the one that changes it.
                    self.bridge.broadcast(PluginEvent::OptionChanged { name, value });
                }
            }
        }
    }

    /// Act on an option the host itself owns. Everything else is a plugin's business.
    fn apply_option(&mut self, name: &str, value: &OptionValue) {
        match name {
            "agent.model" => {
                let Some(spec) = value.as_str().filter(|s| !s.is_empty()) else { return };
                let Some((instance, model)) = Config::parse_model(spec) else {
                    self.editor_message(
                        MessageLevel::Error,
                        format!("agent.model: {spec:?} is not `instance/model`"),
                    );
                    return;
                };
                let selection = neosh_proto::ModelSelection {
                    instance: instance.into(),
                    model: neosh_proto::ModelId::from(model),
                    options: Vec::new(),
                };
                if self.agent.providers().resolve(&selection).is_none() {
                    if self.startup.loading > 0 || !self.startup.discovered {
                        self.startup.pending_model = Some(spec.to_string());
                    } else {
                        self.editor_message(
                            MessageLevel::Error,
                            format!(
                                "agent.model: no provider serves {spec}. `neosh --list-models` \
                                 shows what is reachable."
                            ),
                        );
                    }
                    return;
                }
                self.startup.pending_model = None;
                self.agent.set_selection(selection);
                // Chosen, not remembered: from here on the fallback leaves it alone even if it
                // cannot authenticate, because the error is more use than a substitution.
                self.selection_pinned = true;
                self.refresh_status();
                self.draw_welcome();
            }
            "ui.theme" => {
                let Some(name) = value.as_str() else { return };
                let Some(variant) = neosh_core::palette::Variant::parse(name) else {
                    // Unreachable through the option registry, which validates the enum; reachable
                    // if the host ever sets this itself.
                    self.editor_message(
                        MessageLevel::Error,
                        format!("ui.theme: {name:?} is not a theme"),
                    );
                    return;
                };
                if self.editor.set_theme(variant) {
                    self.refresh_status();
                }
            }
            "ui.hints" => self.refresh_composer(),
            "ui.motion" => {
                // Applied by stripping the animation from the groups that have one, so a frontend
                // needs no concept of the setting: it animates what carries an animation, and with
                // motion off nothing does.
                if self.editor.set_motion(value.as_bool().unwrap_or(true)) {
                    self.refresh_status();
                }
            }
            "agent.system_prompt" => {
                let text = value.as_str().unwrap_or_default();
                self.agent.session().system =
                    if text.is_empty() { None } else { Some(text.to_string()) };
            }
            _ => {}
        }
    }

    /// Redraw the status line.
    ///
    /// Rewritten wholesale rather than patched: it is one short line, and a diffing status bar is a
    /// famous source of stale text that nobody notices until a screenshot.
    /// Redraw the status line from the host's own facts plus whatever plugins contributed.
    ///
    /// The host owns the strip — one line, always visible, never scrolled — and plugins own what is
    /// in it. That split is what lets the model switcher and the git plugin each put something
    /// there without either knowing the other exists, and it is why the footer is not hard-coded
    /// here.
    fn refresh_status(&mut self) {
        // Reading is not a `Mode` — it is a place the keyboard is, and the whole reason it shows
        // here is that a key doing something different needs to say so before you press it.
        let mode = if self.secret.is_some() {
            // Named in the status line because it is the one state where a keystroke does not do
            // what it usually does, and you should be able to see that before you press one.
            "key"
        } else if self.reading {
            "reading"
        } else {
            match self.editor.mode() {
                Mode::Chat => "chat",
                Mode::Normal => "normal",
                Mode::Insert => "insert",
                Mode::Visual => "visual",
            }
        };

        let mut left: Vec<(i32, String, neosh_proto::StatusSegment)> = Vec::new();
        let mut right: Vec<(i32, String, neosh_proto::StatusSegment)> = Vec::new();
        for (key, seg) in &self.status_segments {
            let entry = (seg.priority, key.clone(), seg.clone());
            match seg.align {
                neosh_proto::StatusAlign::Left => left.push(entry),
                neosh_proto::StatusAlign::Right => right.push(entry),
            }
        }
        // Ties break by key so the order is stable across redraws rather than depending on which
        // plugin happened to load first.
        left.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        right.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        // Marks are built alongside the text so a segment's colour cannot drift from its columns.
        let mut text = String::from(" ");
        let mut marks: Vec<(usize, usize, String)> = Vec::new();
        let push = |text: &mut String, marks: &mut Vec<(usize, usize, String)>, s: &str, hl: &str| {
            let from = text.len();
            text.push_str(s);
            marks.push((from, text.len(), hl.to_string()));
        };

        push(&mut text, &mut marks, mode, "Status.Line");
        for (_, _, seg) in left.iter().chain(right.iter()) {
            text.push_str("  ");
            push(&mut text, &mut marks, &seg.text, seg.hl.as_deref().unwrap_or("Status.Line"));
            // The key that changes it, right after it. A key is only memorable once it has been
            // seen next to the thing it does; a legend somewhere else is a second place to look.
            if let Some(keys) = seg.keys.as_deref().filter(|k| !k.is_empty()) {
                text.push(' ');
                push(&mut text, &mut marks, keys, "Composer.HintKey");
            }
        }
        if self.status_segments.is_empty() {
            // Before any plugin has contributed — and under `--clean`, where none will.
            push(&mut text, &mut marks, "  ^C quit", "Status.Line");
        }
        text.push(' ');

        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.status,
            start: 0,
            end: -1,
            lines: vec![text.clone()],
        });
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.status_ns,
            buf: self.status,
            start: None,
            end: None,
        });
        for (from, to, hl) in marks {
            let _ = self.editor.apply(&plugin, ApiCall::MarkSet {
                ns: self.status_ns,
                buf: self.status,
                row: 0,
                col: from as u32,
                opts: neosh_proto::ExtmarkOpts {
                    end_col: Some(to as u32),
                    hl_group: Some(hl),
                    virt_text: vec![],
                    virt_text_pos: neosh_proto::VirtTextPos::Eol,
                    on_delete: neosh_proto::OnDelete::Clamp,
                    priority: 0,
                },
            });
        }
    }

    /// Redraw the chrome around the composer: the rule, the prompt, the placeholder, the shortcuts.
    ///
    /// All of it is virtual text rather than characters in the buffer, which is the only version
    /// that is correct: the composer's contents *are* the message, so a `› ` written into it would
    /// be sent to the model, and a placeholder written into it would be sent the moment someone
    /// pressed Enter without looking. Virtual text is drawn and never read back.
    fn refresh_composer(&mut self) {
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.composer_ns,
            buf: self.composer,
            start: None,
            end: None,
        });
        if self.secret.is_some() {
            // While a key is being typed the composer is borrowed and says so itself. Shortcuts
            // that no longer work, advertised under a field where every keystroke is captured, is
            // exactly the wrong thing to be reading.
            return;
        }

        let ascii = self.option_bool("ui.ascii_only");
        let text = self.composer_text();
        let lines = self.editor.buffer(self.composer).map(|b| b.line_count()).unwrap_or(1).max(1);
        let width = self
            .editor
            .window(self.composer_win)
            .and_then(|w| w.viewport.map(|v| v.width as usize))
            .unwrap_or(80);

        // The rule. Drawn above the first line, so it moves with the field rather than being a row
        // the transcript has to remember not to write on.
        let rule = if ascii { "-" } else { "\u{2500}" };
        self.composer_mark(0, VirtTextPos::Above, vec![chunk(rule.repeat(width), "Composer.Rule")]);

        // The prompt, spliced in at column zero. The caret is pushed past it by the frontend, which
        // is the only place that knows how wide it drew.
        let caret = if ascii { "> " } else { "\u{203a} " };
        self.composer_mark(0, VirtTextPos::Inline, vec![chunk(caret, "Composer.Prompt")]);

        if text.is_empty() {
            self.composer_mark(
                0,
                VirtTextPos::Eol,
                vec![chunk("Ask anything, or run a command", "Composer.Placeholder")],
            );
        }

        let row = lines.saturating_sub(1);
        let indent = caret.chars().count();
        let pad = " ".repeat(indent);

        // What you typed while it was working, waiting for a gap to be said in. This outranks the
        // shortcut row: an unsent sentence of yours is more urgent than a list of keys.
        let queued = self.agent.steering_texts();
        for (i, text) in queued.iter().take(2).enumerate() {
            let more = queued.len().saturating_sub(2);
            let tail = if i == 1 && more > 0 { format!("  (+{more} more)") } else { String::new() };
            let one = text.lines().next().unwrap_or("").trim();
            let body: String = one.chars().take(width.saturating_sub(indent + 10)).collect();
            self.composer_mark(row, VirtTextPos::Below, vec![
                chunk(pad.clone(), "Composer.Hint"),
                chunk("queued ", "Status.Pending"),
                chunk(format!("{body}{tail}"), "Composer.Hint"),
            ]);
        }

        // The shortcut row goes under the last line you have written — and steps aside once you are
        // writing enough, or have enough queued, that it would cost you a line to look at.
        if lines <= 2 && queued.is_empty()
            && let Some(chunks) = self.hint_row(width.saturating_sub(indent))
        {
            let mut v = vec![chunk(pad, "Composer.Hint")];
            v.extend(chunks);
            self.composer_mark(row, VirtTextPos::Below, v);
        }
    }

    /// One virtual-text mark on the composer.
    fn composer_mark(&mut self, row: u32, pos: VirtTextPos, virt_text: Vec<neosh_proto::VirtChunk>) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::MarkSet {
            ns: self.composer_ns,
            buf: self.composer,
            row,
            col: 0,
            opts: neosh_proto::ExtmarkOpts {
                end_col: None,
                hl_group: None,
                virt_text,
                virt_text_pos: pos,
                on_delete: neosh_proto::OnDelete::Clamp,
                priority: 0,
            },
        });
    }

    /// The shortcuts, as many as fit, in the order whoever registered them asked for.
    ///
    /// Truncation drops whole hints from the end rather than cutting one in half, because half a
    /// shortcut is worse than no shortcut: it reads as a key that exists and does something else.
    /// Nothing is ever dropped silently to nothing — a row too narrow for even one hint simply
    /// does not appear, and the full list is still a keypress away.
    fn hint_row(&self, width: usize) -> Option<Vec<neosh_proto::VirtChunk>> {
        if !self.editor.options().bool("ui.hints").unwrap_or(true) {
            return None;
        }
        let mut hints: Vec<(&String, &neosh_proto::Hint)> = self.hints.iter().collect();
        hints.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

        let mut out: Vec<neosh_proto::VirtChunk> = Vec::new();
        let mut used = 0usize;
        for (_, h) in hints {
            let gap = if out.is_empty() { 0 } else { 2 };
            let cost = gap + display_width(&h.keys) + 1 + display_width(&h.label);
            if used + cost > width {
                break;
            }
            if gap > 0 {
                out.push(chunk("  ", "Composer.Hint"));
            }
            out.push(chunk(h.keys.clone(), "Composer.HintKey"));
            out.push(chunk(format!(" {}", h.label), "Composer.Hint"));
            used += cost;
        }
        (!out.is_empty()).then_some(out)
    }

    /// What the transcript says before anything has happened.
    ///
    /// Redrawn rather than written once, because the two facts worth putting here — which model
    /// answers, and whether it can — are not known when the first frame goes up: providers register
    /// during startup and a plugin may add one later still. `welcome_rows` is how it knows what it
    /// last wrote, so a redraw replaces its own lines and never a real turn's.
    fn draw_welcome(&mut self) {
        // Anything else in the buffer is a conversation. Leave it alone. "Empty" has to allow for
        // the one blank line a buffer always has, which is not the same as no lines at all.
        let blank = self
            .editor
            .buffer(self.chat)
            .map(|b| b.line_count() <= 1 && b.get_lines(0, 1).iter().all(|l| l.is_empty()))
            .unwrap_or(true);
        let n = self.editor.buffer(self.chat).map(|b| b.line_count()).unwrap_or(0) as usize;
        if !blank && n != self.welcome_rows {
            return;
        }
        let replacing = if blank { n as i64 } else { self.welcome_rows as i64 };

        let selection = self.agent.selection();
        let account = selection.as_ref().and_then(|s| {
            let reg = self.agent.providers();
            let inst = reg.instances().find(|i| i.id == s.instance)?;
            Some(neosh_provider::credentials::credentials().source(inst).summary(&inst.auth))
        });
        let model = match &selection {
            Some(s) => match account {
                Some(a) => format!("{}/{}  \u{b7}  {a}", s.instance, s.model),
                None => format!("{}/{}", s.instance, s.model),
            },
            None => "nothing configured \u{2014} run `neosh --list-models`".to_string(),
        };

        let mut rows = vec![
            String::new(),
            "  neosh \u{2014} a terminal-first agent workspace".to_string(),
            String::new(),
            format!("  model      {model}"),
            format!("  directory  {}", tilde(&self.cwd)),
            String::new(),
        ];
        // Only when there is something to say. On a working setup the welcome should be four short
        // lines and then out of the way.
        if selection.as_ref().is_some_and(|s| !self.selection_is_usable(s)) {
            rows.push("  This model cannot authenticate yet. Press ^P to pick another, or add a \
                       key there."
                .to_string());
            rows.push(String::new());
        }

        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.chat,
            start: 0,
            end: replacing,
            lines: rows.clone(),
        });
        self.welcome_rows = rows.len();
        self.chat_mark(1, 2, 2 + "neosh".len(), "Accent");
        self.chat_mark(1, 2 + "neosh".len(), rows[1].len(), "Comment");
        for row in [3u32, 4] {
            self.chat_mark(row, 0, 13, "Comment");
        }
        if rows.len() > 6 {
            self.chat_mark(6, 0, rows[6].len(), "Diagnostic.Warn");
        }
    }

    /// Drivers that answer their own permission prompts need to know what the mode is.
    fn sync_acp_mode(&self) {
        let mode = self.agent.permissions().mode();
        for d in &self.acp_drivers {
            d.set_mode(mode);
        }
    }

    /// Take ownership of the ACP drivers, so the mode can be mirrored into them later.
    pub fn adopt_acp_drivers(&mut self, drivers: Vec<Arc<neosh_provider::drivers::AcpProvider>>) {
        self.acp_drivers = drivers;
        self.sync_acp_mode();
    }

    /// The characters this terminal gets to draw the transcript with.
    fn glyphs(&self) -> Glyphs {
        Glyphs::new(self.option_bool("ui.ascii_only"))
    }

    /// Move off a model that cannot authenticate, if there is somewhere sensible to move to.
    ///
    /// The case this exists for: a conversation started months ago against an API-key provider is
    /// reopened on a machine with no key, and every launch greets you with "no key" over a model
    /// you never chose today. A stored selection is a *record of what was used*, not an instruction
    /// — so when it has stopped working, following it is the wrong kind of faithful.
    ///
    /// What `agent.model` says is a different matter and is left alone: that is a person telling
    /// neosh which model to use, and quietly using another one would be worse than the error.
    ///
    /// The replacement keeps the product line where it can — `anthropic/claude-opus-5` becomes the
    /// Opus you have on a plan, not whatever sorts first — because "same model, different way of
    /// paying for it" is a switch nobody needs to think about, and any other switch is one they do.
    fn ensure_usable_selection(&mut self) {
        if self.selection_pinned {
            return;
        }
        let Some(current) = self.agent.selection() else { return };
        if self.selection_is_usable(&current) {
            return;
        }

        let replacement = {
            let reg = self.agent.providers();
            let family = reg
                .instances()
                .find(|i| i.id == current.instance)
                .and_then(|i| i.models.iter().find(|m| m.id == current.model))
                .and_then(|m| m.family.clone());
            let same_line = family.as_ref().and_then(|f| {
                reg.ready_instances().find_map(|i| {
                    let m = i
                        .models
                        .iter()
                        .filter(|m| m.family.as_ref() == Some(f) && !m.legacy)
                        .max_by_key(|m| m.tier.map(|t| t.rank()).unwrap_or(0))?;
                    Some(neosh_proto::ModelSelection {
                        instance: i.id.clone(),
                        model: m.id.clone(),
                        options: catalog::default_options(&m.capabilities),
                    })
                })
            });
            same_line.or_else(|| reg.default_selection())
        };

        let Some(next) = replacement else {
            // Nothing works. Say so once, here, rather than letting the first message find out.
            self.editor_message(
                MessageLevel::Warn,
                format!(
                    "{} cannot authenticate, and nothing else is configured. Press ^P to add a key.",
                    current.instance
                ),
            );
            return;
        };

        let note = format!(
            "{}/{} cannot authenticate \u{2014} using {}/{} instead.",
            current.instance, current.model, next.instance, next.model
        );
        self.agent.set_selection(next);
        self.editor_message(MessageLevel::Info, note);
        self.refresh_status();
    }

    /// Whether a turn sent with this selection could authenticate.
    fn selection_is_usable(&self, s: &neosh_proto::ModelSelection) -> bool {
        let reg = self.agent.providers();
        reg.instances()
            .find(|i| i.id == s.instance)
            .is_some_and(|i| neosh_provider::credentials::credentials().source(i).is_usable())
    }

    /// The shortcuts the host itself owns. Everything else on that row comes from a plugin.
    fn seed_hints(&mut self) {
        let ascii = self.option_bool("ui.ascii_only");
        let (send, newline) =
            if ascii { ("Enter", "S-Enter") } else { ("\u{23ce}", "\u{21e7}\u{23ce}") };
        self.hints.insert(format!("{BUILTIN}:send"), neosh_proto::Hint {
            keys: send.into(),
            label: "send".into(),
            priority: 0,
        });
        self.hints.insert(format!("{BUILTIN}:newline"), neosh_proto::Hint {
            keys: newline.into(),
            label: "newline".into(),
            priority: 1,
        });
    }

    fn option_bool(&self, name: &str) -> bool {
        self.editor.options().bool(name).unwrap_or(false)
    }

    /// Keys nothing claimed.
    ///
    /// In chat mode this is the composer, which is a real text field rather than a string things
    /// get appended to: the cursor is somewhere, motions move it, and edits happen where it is.
    /// `<Esc>` always interrupts, in every mode, because a key that stops the thing that is running
    /// must not depend on where you happen to be.
    fn handle_unbound_key(&mut self, key: neosh_proto::KeyPress, mode: Mode) {
        if self.reading {
            self.reading_key(key);
            return;
        }
        if key.code == KeyCode::Esc {
            if let Some(t) = self.active_turn.take() {
                t.cancel();
                self.editor_message(MessageLevel::Info, "interrupted");
            }
            return;
        }
        if mode != Mode::Chat {
            return;
        }

        let m = key.mods;
        match key.code {
            // A newline where a send would be: the composer is multi-line, and pasting a snippet
            // into it should not fire off half a question.
            KeyCode::Enter if m.shift || m.alt => self.compose(TextEdit::Insert { text: "\n".into() }),
            KeyCode::Enter => {
                let text = self.composer_text();
                if text.trim().is_empty() {
                    return;
                }
                self.set_composer("");
                self.start_turn(text);
            }

            KeyCode::Backspace if m.ctrl || m.alt => self.compose(TextEdit::DeleteWordBack),
            KeyCode::Backspace => self.compose(TextEdit::DeleteBack),
            KeyCode::Delete if m.ctrl || m.alt => self.compose(TextEdit::DeleteWordForward),
            KeyCode::Delete => self.compose(TextEdit::DeleteForward),

            KeyCode::Left if m.ctrl || m.alt => self.move_composer(CursorMotion::WordLeft, m.shift),
            KeyCode::Left => self.move_composer(CursorMotion::Left, m.shift),
            KeyCode::Right if m.ctrl || m.alt => {
                self.move_composer(CursorMotion::WordRight, m.shift)
            }
            KeyCode::Right => self.move_composer(CursorMotion::Right, m.shift),

            // Vertical keys do double duty, decided by where the cursor already is. On the first
            // line of a one-line composer — which is nearly always — `↑` is how you look back at
            // what was said, and making you learn a second key for that would be absurd.
            KeyCode::Up if self.composer_cursor().0 == 0 && !m.shift => self.scroll_chat(-1),
            KeyCode::Up => self.move_composer(CursorMotion::Up, m.shift),
            KeyCode::Down if self.composer_at_last_line() && !m.shift => self.scroll_chat(1),
            KeyCode::Down => self.move_composer(CursorMotion::Down, m.shift),

            KeyCode::Home if m.ctrl => self.move_composer(CursorMotion::BufStart, m.shift),
            KeyCode::Home => self.move_composer(CursorMotion::LineStart, m.shift),
            KeyCode::End if m.ctrl => self.move_composer(CursorMotion::BufEnd, m.shift),
            KeyCode::End => self.move_composer(CursorMotion::LineEnd, m.shift),

            // Readline, because this is a terminal and these are the keys terminal fingers have.
            KeyCode::Char { ref c } if m.ctrl => match c.as_str() {
                "a" => self.move_composer(CursorMotion::LineStart, false),
                "u" => self.compose(TextEdit::DeleteToLineStart),
                "w" => self.compose(TextEdit::DeleteWordBack),
                _ => {}
            },
            // A key press carries a grapheme, not a `char`: a dead-key accent and the letter it
            // lands on arrive together, and inserting them separately would put the cursor between
            // a letter and its own diacritic.
            KeyCode::Char { ref c } if !m.alt => {
                self.compose(TextEdit::Insert { text: c.clone() })
            }
            _ => {}
        }
    }

    fn compose(&mut self, edit: TextEdit) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinEdit {
            win: self.composer_win,
            edit,
        });
    }

    fn move_composer(&mut self, motion: CursorMotion, select: bool) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinMotion {
            win: self.composer_win,
            motion,
            select,
        });
    }

    fn composer_cursor(&self) -> (u32, u32) {
        self.editor.window(self.composer_win).map(|w| w.cursor).unwrap_or((0, 0))
    }

    fn composer_at_last_line(&self) -> bool {
        let last = self
            .editor
            .buffer(self.composer)
            .map(|b| b.line_count().saturating_sub(1))
            .unwrap_or(0);
        self.composer_cursor().0 >= last
    }

    /// What is selected in a window, or nothing.
    fn selection(&mut self, win: WindowId) -> String {
        match self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinSelection { win }) {
            Ok(ApiOk::Text { text }) => text,
            _ => String::new(),
        }
    }

    /// Copy what is selected, in whichever half of the screen you are looking at.
    ///
    /// Returns whether there was anything to copy, so `<C-c>` can fall through to interrupting when
    /// there is not — which is the behaviour every terminal has and the reason this key can carry
    /// both jobs without ever being ambiguous.
    fn copy_selection(&mut self, cut: bool) -> bool {
        let win = if self.reading { self.chat_win } else { self.composer_win };
        let text = self.selection(win);
        if text.is_empty() {
            return false;
        }
        let lines = text.lines().count();
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::ClipboardWrite { text });
        if cut && !self.reading {
            let _ = self.editor.apply(&plugin, ApiCall::WinEdit {
                win,
                edit: TextEdit::DeleteSelection,
            });
        } else {
            let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win, on: false });
        }
        self.editor_message(
            MessageLevel::Info,
            format!("{} {lines} line{}", if cut { "cut" } else { "copied" }, if lines == 1 { "" } else { "s" }),
        );
        true
    }

    // ---- typing in a key -------------------------------------------------

    /// Start collecting a key for `instance`, borrowing the composer to do it.
    ///
    /// Returns the error a caller should be told about rather than opening a prompt that cannot
    /// lead anywhere: a CLI login and a local endpoint have nothing to do with a key, and asking
    /// for one would be asking for something that gets thrown away.
    fn begin_secret(
        &mut self,
        instance: neosh_proto::InstanceId,
        replace: bool,
        waiting: Option<(PluginId, RequestId)>,
    ) -> Result<(), ApiError> {
        if self.secret.is_some() {
            return Err(ApiError::Busy { message: "a key is already being entered".into() });
        }
        let (label, accepts, present) = {
            let providers = self.agent.providers();
            let cfg = providers
                .instance(&instance)
                .ok_or_else(|| ApiError::NotFound { what: format!("instance {instance}") })?;
            let store = neosh_provider::credentials::credentials();
            let source = store.source(cfg);
            (
                cfg.display_name.clone(),
                !matches!(cfg.auth, neosh_proto::AuthRef::Inherited | neosh_proto::AuthRef::None),
                source.is_usable(),
            )
        };
        if !accepts {
            return Err(ApiError::InvalidArgument {
                message: format!("{instance} signs in on its own — it has nowhere to put a key"),
            });
        }
        if present && !replace {
            return Err(ApiError::InvalidArgument {
                message: format!("{instance} already has a key — pass `replace` to change it"),
            });
        }

        // Leave the transcript first, so the caret is where the bullets are.
        self.leave_reading();
        let draft = self.composer_text();
        self.secret = Some(SecretPrompt { instance, label, value: String::new(), draft, waiting });
        self.chat_line("");
        self.chat_line("Paste or type the key, then Enter. Esc cancels. Nothing is echoed.");
        self.render_secret();
        self.refresh_status();
        Ok(())
    }

    /// Draw the prompt: a label, and one bullet per grapheme.
    ///
    /// Bullets rather than nothing at all, because a paste that silently did not arrive is the
    /// failure people actually hit. Bullets rather than the value, because this line is a buffer
    /// and a buffer is sent to the frontend.
    fn render_secret(&mut self) {
        let Some(p) = self.secret.as_ref() else { return };
        let n = p.value.graphemes(true).count();
        // Capped: a 200-character key would push the label off a narrow composer. The overflow
        // count is what tells you a long paste landed.
        let shown = n.min(32);
        let mut mask = "\u{2022}".repeat(shown);
        if n > shown {
            mask.push_str(&format!(" +{}", n - shown));
        }
        let line = format!("key for {}  {mask}", p.label);
        self.set_composer(&line);
    }

    /// Every key while a key is being typed.
    ///
    /// Deliberately reached *before* the keymaps: while this prompt is up no binding fires, so a
    /// stray `n` in the middle of a pasted key cannot start a conversation.
    fn secret_key(&mut self, key: neosh_proto::KeyPress) {
        let Some(p) = self.secret.as_mut() else { return };
        match key.code {
            KeyCode::Esc => {
                self.finish_secret(false);
                return;
            }
            KeyCode::Enter => {
                self.finish_secret(true);
                return;
            }
            KeyCode::Backspace => {
                // By grapheme, like everything else here that steps through text.
                let keep = p.value.graphemes(true).count().saturating_sub(1);
                p.value = p.value.graphemes(true).take(keep).collect();
            }
            KeyCode::Char { ref c } if key.mods.ctrl => match c.as_str() {
                "u" => p.value.clear(),
                "c" => {
                    self.finish_secret(false);
                    return;
                }
                _ => return,
            },
            KeyCode::Char { ref c } if !key.mods.alt => p.value.push_str(c),
            _ => return,
        }
        self.render_secret();
    }

    /// Pasting is how a key actually arrives.
    ///
    /// Surrounding whitespace is stripped: a key copied out of a web page brings a newline with it,
    /// and a trailing newline in a bearer token is a 401 that looks exactly like a wrong key.
    fn secret_paste(&mut self, text: &str) {
        let Some(p) = self.secret.as_mut() else { return };
        p.value.push_str(text.trim());
        self.render_secret();
    }

    /// Store what was typed, or throw it away, and give the composer back.
    fn finish_secret(&mut self, submit: bool) {
        let Some(mut p) = self.secret.take() else { return };
        let mut stored = false;
        if submit {
            let value = std::mem::take(&mut p.value);
            let trimmed = value.trim();
            if trimmed.is_empty() {
                self.editor_message(MessageLevel::Warn, "nothing entered");
            } else {
                let secret = secrecy::SecretString::from(trimmed);
                let where_it_went =
                    neosh_provider::credentials::credentials().set(&p.instance, secret);
                stored = true;
                let label = p.label.clone();
                match where_it_went {
                    neosh_provider::credentials::Stored::Keychain => self.editor_message(
                        MessageLevel::Info,
                        format!("{label}: key saved to the keychain"),
                    ),
                    // Still usable, just not tomorrow — which is worth saying plainly rather than
                    // letting it be a surprise at the next start.
                    neosh_provider::credentials::Stored::Session => self.editor_message(
                        MessageLevel::Warn,
                        format!(
                            "{label}: key kept for this run only — no keychain available. \
                             Install `secret-tool`, or point `auth` at a command in config.toml."
                        ),
                    ),
                }
            }
        }
        let draft = std::mem::take(&mut p.draft);
        self.set_composer(&draft);
        self.refresh_status();
        if let Some((plugin, id)) = p.waiting.take() {
            let response: ApiResponse = Ok(ApiOk::Bool { value: stored }).into();
            let _ = self.bridge.script().send(ScriptInbound::Plugin {
                plugin,
                msg: PluginInbound::Response { id, response },
            });
        }
        // `p` drops here, which zeroes whatever is left of the value.
    }

    /// Move into the transcript to read, navigate and select it.
    ///
    /// The composer is where you type; this is where you go to take something out. It is a separate
    /// place rather than a focus change because the keys mean different things there — `j` moves
    /// rather than types — and pretending otherwise is how modeless editors end up with twelve
    /// chords nobody remembers.
    fn enter_reading(&mut self) {
        if self.reading {
            return;
        }
        self.reading = true;
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::FocusPush { win: self.chat_win });
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.chat_win, on: false });
        self.refresh_status();
        self.editor_message(
            MessageLevel::Info,
            "reading — hjkl/arrows move, v selects, y copies, Esc leaves",
        );
    }

    fn leave_reading(&mut self) {
        if !self.reading {
            return;
        }
        self.reading = false;
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.chat_win, on: false });
        let _ = self.editor.apply(&plugin, ApiCall::FocusPop);
        self.refresh_status();
    }

    /// Keys while reading the transcript. Deliberately the vi set, plus the arrows.
    ///
    /// Motion extends the selection whenever one is running, so `v` then `j j j` selects three
    /// lines — the modal behaviour, rather than requiring shift to be held for the whole journey.
    fn reading_key(&mut self, key: neosh_proto::KeyPress) {
        let ch = match &key.code {
            KeyCode::Char { c } => c.as_str(),
            _ => "",
        };
        let motion = match (&key.code, ch) {
            (KeyCode::Left, _) | (_, "h") => Some(CursorMotion::Left),
            (KeyCode::Right, _) | (_, "l") => Some(CursorMotion::Right),
            (KeyCode::Up, _) | (_, "k") => Some(CursorMotion::Up),
            (KeyCode::Down, _) | (_, "j") => Some(CursorMotion::Down),
            (_, "b") | (_, "B") => Some(CursorMotion::WordLeft),
            (_, "w") | (_, "W") => Some(CursorMotion::WordRight),
            (KeyCode::Home, _) | (_, "0") => Some(CursorMotion::LineStart),
            (KeyCode::End, _) | (_, "$") => Some(CursorMotion::LineEnd),
            (_, "g") => Some(CursorMotion::BufStart),
            (_, "G") => Some(CursorMotion::BufEnd),
            _ => None,
        };
        if let Some(motion) = motion {
            let select = self.chat_anchored() || key.mods.shift;
            let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinMotion {
                win: self.chat_win,
                motion,
                select,
            });
            self.follow_cursor();
            return;
        }

        match (&key.code, ch) {
            (KeyCode::Esc, _) | (_, "q") => self.leave_reading(),
            (_, "v") | (_, "V") => {
                let on = !self.chat_anchored();
                let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinSelect {
                    win: self.chat_win,
                    on,
                });
            }
            (_, "a") => self.select_all(self.chat_win),
            // Leaving on a successful yank: you came here to take something, and staying would mean
            // pressing Esc every single time.
            (_, "y") if self.copy_selection(false) => self.leave_reading(),
            _ => {}
        }
    }

    fn chat_anchored(&self) -> bool {
        self.editor.window(self.chat_win).is_some_and(|w| w.anchor.is_some())
    }

    fn select_all(&mut self, win: WindowId) {
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::WinMotion {
            win,
            motion: CursorMotion::BufStart,
            select: false,
        });
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win, on: true });
        let _ = self.editor.apply(&plugin, ApiCall::WinMotion {
            win,
            motion: CursorMotion::BufEnd,
            select: true,
        });
        self.follow_cursor();
    }

    /// Keep the cursor on screen while reading. Scrolling is the frontend's, but *what* to show is
    /// ours: the cursor moving out of the viewport with nothing following it is how a reader gets
    /// lost in their own transcript.
    fn follow_cursor(&mut self) {
        let Some(w) = self.editor.window(self.chat_win) else { return };
        let row = w.cursor.0;
        let Some(v) = w.viewport else { return };
        let height = (v.height as u32).max(1);
        let top = self.chat_top;
        let lines = self.editor.buffer(self.chat).map(|b| b.line_count()).unwrap_or(0);
        // `chat_top == 0` means "following the tail", which is a different place from line zero.
        let showing = if top == 0 { lines.saturating_sub(height) } else { top };
        let next = if row < showing {
            row
        } else if row >= showing + height {
            row + 1 - height
        } else {
            return;
        };
        self.chat_top = next.max(1);
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinScrollTo {
            win: self.chat_win,
            top_line: next,
        });
    }

    /// Redraw everything that is about the conversation you are now in.
    ///
    /// Called after every switch, create and close. The transcript is rebuilt from the session's
    /// messages rather than replayed from the UI events that first produced it: the messages are
    /// the truth, and a switch that reconstructed the buffer from a log of edits would drift the
    /// moment anything else touched it.
    fn enter_session(&mut self) {
        // Not closed: the buffer is about to be replaced wholesale, so settling an answer into rows
        // that are on their way out would write into the conversation being left behind.
        self.answer = None;
        self.streaming = None;
        self.chat_top = 0;
        let ascii = self.option_bool("ui.ascii_only");
        let (lines, marks, label) = {
            let store = self.agent.sessions();
            let s = store.active();
            let (lines, marks) = transcript(s, ascii);
            (lines, marks, s.label())
        };
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.chat,
            start: None,
            end: None,
        });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.chat,
            start: 0,
            end: -1,
            lines,
        });
        for (row, from, to, hl) in marks {
            self.chat_mark(row, from, to, hl);
        }
        let _ = self.editor.apply(&plugin, ApiCall::BufSetName {
            buf: self.chat,
            name: format!("[chat] {label}"),
        });
        // Nothing of the previous conversation's welcome survives into this one: the buffer was
        // just replaced wholesale, so what `welcome_rows` remembered is about text that is gone.
        self.welcome_rows = 0;
        self.ensure_usable_selection();
        self.draw_welcome();
        self.set_composer("");
        self.refresh_status();
        self.bridge.broadcast(PluginEvent::SessionChanged {
            session: self.agent.sessions().active_id().clone(),
        });
    }

    /// Append streamed output, starting a new line when the kind of output changes.
    // ---- the transcript --------------------------------------------------

    /// Add lines to the end of the transcript, returning the row the first one landed on.
    ///
    /// "The end" means above the working line when there is one. It says what is happening *now*,
    /// so anything that has already happened belongs above it — a tool card appended underneath
    /// would leave the spinner stranded in the middle of the turn it is reporting on.
    fn chat_push(&mut self, lines: Vec<String>) -> u32 {
        // Anything else writing into the transcript ends the answer: its rows are addressed by
        // range, and something landing after them would make "the end of the answer" ambiguous.
        self.close_answer();
        let plugin = PluginId::from(BUILTIN);
        let at = self.chat_end();
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.chat,
            start: at,
            end: at,
            lines,
        });
        self.streaming = None;
        at.max(0) as u32
    }

    /// Where new content goes: the end, or the line above the working line when there is one.
    fn chat_end(&self) -> i64 {
        let n = self.editor.buffer(self.chat).map(|b| b.line_count() as i64).unwrap_or(0);
        if self.working.is_some() { (n - 1).max(0) } else { n }
    }

    /// Highlight a span of one transcript row.
    fn chat_mark(&mut self, row: u32, from: usize, to: usize, hl: &str) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::MarkSet {
            ns: self.chat_ns,
            buf: self.chat,
            row,
            col: from as u32,
            opts: neosh_proto::ExtmarkOpts {
                end_col: Some(to as u32),
                hl_group: Some(hl.to_string()),
                virt_text: vec![],
                virt_text_pos: neosh_proto::VirtTextPos::Eol,
                on_delete: neosh_proto::OnDelete::Clamp,
                priority: 0,
            },
        });
    }

    /// Put the next piece of an answer into the transcript.
    ///
    /// Markdown is rendered as it arrives rather than after the fact, and the cost of that is a
    /// range replacement of the trailing block per tick instead of an append. Bounded by the size
    /// of one block, not by the size of the answer — see [`crate::markdown`] for why that
    /// distinction is the whole design.
    ///
    /// With `chat.markdown` off this is the old append: text goes in exactly as it came out, which
    /// is what you want when the question is "what did the model actually say".
    fn answer_text(&mut self, text: &str) {
        let plugin = PluginId::from(BUILTIN);
        if !self.option_bool("chat.markdown") {
            if self.streaming != Some(Stream::Text) {
                self.open_answer_row();
                self.streaming = Some(Stream::Text);
            }
            let _ = self
                .editor
                .apply(&plugin, ApiCall::BufAppendText { buf: self.chat, text: text.into() });
            return;
        }

        if self.answer.is_none() {
            let start = self.open_answer_row();
            self.answer = Some(Answer { md: crate::markdown::Md::new(), start, rows: 1 });
        }
        let Some(mut a) = self.answer.take() else { return };
        let patch = a.md.push(text);
        self.apply_patch(&mut a, patch);
        self.answer = Some(a);
    }

    /// The answer is over: settle its last block and stop treating the tail as provisional.
    fn close_answer(&mut self) {
        let Some(mut a) = self.answer.take() else { return };
        let patch = a.md.finish();
        self.apply_patch(&mut a, patch);
        self.streaming = None;
    }

    /// Open the row an answer starts on, after a blank line separating it from what came before.
    ///
    /// The working line goes first, and it has to: it is the last line of the transcript, and this
    /// writes at the end.
    fn open_answer_row(&mut self) -> u32 {
        self.end_working();
        let n = self.editor.buffer(self.chat).map(|b| b.line_count() as i64).unwrap_or(0);
        // A blank line between the last thing and the answer — unless there already is one. A tool
        // card butted straight up against the sentence that follows it reads as one paragraph, and
        // two blank lines reads as a gap somebody forgot to fill.
        let blank_above = n <= 0
            || self
                .editor
                .buffer(self.chat)
                .map(|b| b.get_lines((n - 1) as u32, n as u32).iter().all(|l| l.trim().is_empty()))
                .unwrap_or(true);
        let rows = if blank_above { 1 } else { 2 };
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::BufSetLines {
            buf: self.chat,
            start: n,
            end: n,
            lines: vec![String::new(); rows],
        });
        (n.max(0) as u32) + (rows as u32 - 1)
    }

    /// Replace the unsettled tail of an answer with its freshly rendered form.
    fn apply_patch(&mut self, a: &mut Answer, patch: crate::markdown::Patch) {
        let at = a.start + patch.from as u32;
        let end = a.start + a.rows;
        let rows = patch.lines.len();
        let plugin = PluginId::from(BUILTIN);
        // Settling a block that was already drawn correctly renders the same rows again. Writing
        // them costs a frame on the wire and changes nothing on screen, which is the definition of
        // a wasted frame — and `finish` does exactly this at the end of every answer.
        let unchanged = self
            .editor
            .buffer(self.chat)
            .map(|b| b.get_lines(at, end.max(at)) == patch.lines)
            .unwrap_or(false);
        if unchanged {
            a.rows = patch.from as u32 + rows as u32;
            return;
        }
        // Marks go with the rows they were on. Left behind, a mark from the previous render of the
        // trailing block would clamp onto whatever replaced it — which is how a half-written bold
        // run ends up colouring the wrong words for the rest of the answer.
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.chat,
            start: Some(at),
            end: Some(end.max(at)),
        });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.chat,
            start: at as i64,
            end: end.max(at) as i64,
            lines: patch.lines,
        });
        for (row, from, to, hl) in patch.marks {
            self.chat_mark(at + row as u32, from, to, hl);
        }
        a.rows = patch.from as u32 + rows as u32;
    }

    /// Draw a question the way a transcript should: framed, so the eye can find where a turn began.
    ///
    /// A bar down the left rather than a `>` prefix, because a multi-line question wrapped by the
    /// frontend loses a prefix on every line but the first, and the thing you scan for when
    /// scrolling back is *where the turns start*.
    fn chat_question(&mut self, text: &str) {
        let glyph = self.glyphs().bar;
        let mut rows = Vec::new();
        for line in text.lines() {
            rows.push(format!("{glyph} {line}"));
        }
        if rows.is_empty() {
            rows.push(glyph.to_string());
        }
        rows.push(String::new());
        let at = self.chat_push(rows);
        let lines = text.lines().count().max(1) as u32;
        for i in 0..lines {
            self.chat_mark(at + i, 0, glyph.len(), "Agent.User");
        }
    }

    /// Begin the working line, and pick this turn's word for it.
    fn begin_working(&mut self) {
        // Not random: the turn id would be ideal but is not known yet, and a clock read is the one
        // varying thing already to hand. Per turn, so it does not change under you mid-answer.
        let verb = VERBS
            .get(now_secs().unsigned_abs() as usize % VERBS.len())
            .copied()
            .unwrap_or("Working");
        self.chat_push(vec![String::new()]);
        self.working = Some(Working { verb, started: std::time::Instant::now(), note: None });
        self.draw_working();
    }

    /// Redraw the working line in place.
    ///
    /// Cheap enough to call on every token: it is one `set_lines` over a single row, which the
    /// core coalesces with everything else in the same 16 ms frame. The *movement* costs nothing
    /// here at all — the shimmer is the frontend's, running at its own rate over whatever this
    /// last wrote.
    fn draw_working(&mut self) {
        let Some(w) = self.working.as_ref() else { return };
        let (verb, note) = (w.verb, w.note.clone());
        let secs = w.started.elapsed().as_secs();
        let row = self.working_row();

        let ascii = self.option_bool("ui.ascii_only");
        let glyph = Glyphs::new(ascii).work;
        let elapsed =
            if secs >= 60 { format!("{}m {}s", secs / 60, secs % 60) } else { format!("{secs}s") };

        let label = note.as_deref().unwrap_or(verb);
        let mut text = format!("{glyph} {label}\u{2026}  {elapsed}");
        // What you typed while it was working, and the fact that it has not been said yet.
        match self.agent.steering_count() {
            0 => {}
            1 => text.push_str("  \u{b7}  1 queued"),
            n => text.push_str(&format!("  \u{b7}  {n} queued")),
        }
        text.push_str("  \u{b7}  esc to interrupt");

        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.chat,
            start: row as i64,
            end: row as i64 + 1,
            lines: vec![text.clone()],
        });
        // Only the label moves. A clock and a hint sweeping along with it would turn a status line
        // into a light show, and the thing you actually want to read is the number.
        let label_end = glyph.len() + 1 + label.len() + "\u{2026}".len();
        let hl = if note.is_some() { "Status.Pending" } else { "Status.Streaming" };
        self.chat_mark(row, 0, label_end, hl);
        self.chat_mark(row, label_end, text.len(), "Agent.Usage");
    }

    /// Take the working line away.
    ///
    /// Removed rather than blanked: it was never content, and a transcript that accumulates one
    /// empty row per turn is a transcript with a growing hole in it.
    fn end_working(&mut self) {
        if self.working.is_none() {
            return;
        }
        let row = self.working_row();
        self.working = None;
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.chat,
            start: Some(row),
            end: Some(row + 1),
        });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.chat,
            start: row as i64,
            end: row as i64 + 1,
            lines: vec![],
        });
        self.streaming = None;
    }

    /// The last line of the transcript, which is where the working line lives while there is one.
    fn working_row(&self) -> u32 {
        self.editor
            .buffer(self.chat)
            .map(|b| b.line_count().saturating_sub(1))
            .unwrap_or(0)
    }

    fn stream_into_chat(&mut self, kind: Stream, text: &str) {
        let plugin = PluginId::from(BUILTIN);
        if self.streaming != Some(kind) {
            self.close_answer();
            // Text is appended to the buffer's *last* line, so the working line has to stop being
            // it before anything streams. This is the one rule that keeps the invariant true.
            self.end_working();
            let n = self.editor.buffer(self.chat).map(|b| b.line_count() as i64).unwrap_or(0);
            let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
                buf: self.chat,
                start: n,
                end: n,
                lines: vec![String::new()],
            });
            self.streaming = Some(kind);
        }
        let _ = self.editor.apply(&plugin, ApiCall::BufAppendText {
            buf: self.chat,
            text: text.to_string(),
        });
    }

    fn handle_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TurnStarted { turn } => {
                self.bridge.broadcast(PluginEvent::TurnStarted { turn });
            }
            AgentEvent::Token { turn, text } => {
                self.answer_text(&text);
                self.bridge.broadcast(PluginEvent::Token { turn, text });
            }
            AgentEvent::Thinking { turn, text } => {
                if self.option_bool("chat.show_thinking") {
                    self.stream_into_chat(Stream::Thinking, &text);
                }
                self.bridge.broadcast(PluginEvent::ThinkingToken { turn, text });
            }
            AgentEvent::ToolStarted { turn, call } => {
                if self.option_bool("chat.show_tools") {
                    let glyph = self.glyphs().tool;
                    let text = format!("  {glyph} {}{}", call.name, tool_subject(&call));
                    let row = self.chat_push(vec![text.clone()]);
                    self.chat_mark(row, 0, 2 + glyph.len() + 1 + call.name.len(), "Agent.Tool");
                    self.chat_mark(
                        row,
                        2 + glyph.len() + 1 + call.name.len(),
                        text.len(),
                        "Agent.Usage",
                    );
                    // The tool is what the turn is doing now, so the working line should say so
                    // rather than keep claiming the model is thinking.
                    if let Some(w) = self.working.as_mut() {
                        w.note = Some(format!("Running {}", call.name));
                    }
                    self.draw_working();
                }
                self.bridge.broadcast(PluginEvent::ToolStarted { turn, call });
            }
            AgentEvent::ToolFinished { turn, call, result } => {
                // Every call gets an answer under it, not just the failures. A card that says what
                // ran and nothing about what came back is a card you have to take on trust, and
                // "it ran" is not the thing you wanted to know.
                //
                // Errors show even with tool cards off: a tool that failed silently is a debugging
                // session, and the setting is about noise, not about hiding failures.
                if result.is_error || self.option_bool("chat.show_tools") {
                    let g = self.glyphs();
                    let glyph = if result.is_error { g.fail } else { g.result };
                    let summary = tool_summary(&result);
                    let text = format!("    {glyph} {summary}");
                    let row = self.chat_push(vec![text.clone()]);
                    let hl = if result.is_error { "Agent.ToolError" } else { "Agent.Usage" };
                    self.chat_mark(row, 0, text.len(), hl);
                }
                if let Some(w) = self.working.as_mut() {
                    w.note = None;
                }
                self.draw_working();
                self.bridge.broadcast(PluginEvent::ToolFinished { turn, call, result });
            }
            AgentEvent::TurnEnded { turn, stop_reason, usage } => {
                self.close_answer();
                self.end_working();
                // Queued after the last gap this turn had. It is a question that was asked and not
                // yet answered, so it becomes the next turn rather than being quietly dropped.
                let leftover = self.agent.take_steering();
                self.active_turn = None;
                {
                    let mut s = self.agent.session();
                    s.updated_at = now_secs();
                }
                self.persist_sessions();
                self.streaming = None;
                self.bridge.broadcast(PluginEvent::TurnEnded { turn, stop_reason, usage });
                if !leftover.is_empty() {
                    self.start_turn(leftover.join("\n"));
                }
            }
            AgentEvent::Steered { text, .. } => {
                // Drawn now rather than when it was typed: until the model has been told, a
                // transcript showing the question is a transcript that is lying about what was
                // asked.
                self.chat_question(&text);
                self.draw_working();
                self.refresh_composer();
            }
            AgentEvent::Notice { level, text } => self.editor_message(level, text),
        }
    }

    async fn handle_script_out(&mut self, out: ScriptOutbound) {
        match out {
            ScriptOutbound::Loaded { plugin, error } => {
                match error {
                    None => {
                        self.bridge.register_loaded(plugin.clone());
                        tracing::info!(%plugin, "plugin loaded");
                    }
                    Some(e) => {
                        tracing::error!(%plugin, "failed to activate: {e}");
                        self.editor_message(MessageLevel::Error, format!("{plugin}: {e}"));
                        // It threw *during* activation, which means it may already have armed
                        // timers and registered commands, tools and hooks. Nothing else will ever
                        // clean those up: a plugin that failed to load never enters the loaded
                        // list, so reload would skip it and every reload would stack another copy.
                        self.tear_down_plugin(&plugin);
                    }
                }
                self.advance_startup(&plugin);
            }
            ScriptOutbound::Log { level, message } => match level {
                MessageLevel::Error => tracing::error!("plugin runtime: {message}"),
                MessageLevel::Warn => tracing::warn!("plugin runtime: {message}"),
                MessageLevel::Info => tracing::info!("plugin runtime: {message}"),
            },
            ScriptOutbound::Plugin { plugin, msg } => match msg {
                PluginOutbound::Ready { .. } => {}
                PluginOutbound::Call { id, call } => {
                    // Answered when the prompt closes rather than now: the host has to read the
                    // keyboard before it knows whether a key was entered, and the plugin is
                    // waiting to hear exactly that.
                    if let ApiCall::ProviderSetCredential { instance, replace } = &call {
                        let waiting = Some((plugin.clone(), id.clone()));
                        if let Err(e) =
                            self.begin_secret(instance.clone(), *replace, waiting)
                        {
                            let response: ApiResponse = Err(e).into();
                            let _ = self.bridge.script().send(ScriptInbound::Plugin {
                                plugin,
                                msg: PluginInbound::Response { id, response },
                            });
                        }
                        return;
                    }
                    if self.spawn_slow(&plugin, Some(id.clone()), &call) {
                        return;
                    }
                    let response: ApiResponse = self.dispatch(&plugin, call).await.into();
                    let _ = self.bridge.script().send(ScriptInbound::Plugin {
                        plugin,
                        msg: PluginInbound::Response { id, response },
                    });
                }
                PluginOutbound::Notify { call } => {
                    if let ApiCall::ProviderSetCredential { instance, replace } = &call {
                        if let Err(e) = self.begin_secret(instance.clone(), *replace, None) {
                            tracing::warn!(%plugin, "{e}");
                        }
                        return;
                    }
                    if self.spawn_slow(&plugin, None, &call) {
                        return;
                    }
                    if let Err(e) = self.dispatch(&plugin, call).await {
                        // Fire-and-forget calls have nowhere to return an error, so surface it
                        // rather than letting it vanish.
                        tracing::warn!(%plugin, "{e}");
                    }
                }
                PluginOutbound::Response { id, response } => match response {
                    PluginResponse::Tool { result } => self.bridge.resolve_tool(&id, result),
                    PluginResponse::Hook { outcome } => self.bridge.resolve_hook(&id, outcome),
                    PluginResponse::ProviderAccepted => self.bridge.resolve_accept(&id),
                    PluginResponse::Error { message } => {
                        tracing::warn!(%plugin, "{message}");
                        self.bridge.fail(&id, message);
                    }
                },
            },
        }
    }

    fn handle_input(&mut self, input: InputEvent) -> bool {
        // A key being typed in takes the keyboard whole, bindings included. Routing it through the
        // keymaps first would mean a key containing `n` — every base64 key does — firing whatever
        // `n` is bound to, halfway through the paste.
        if self.secret.is_some() {
            match input {
                InputEvent::Key { key } => self.secret_key(key),
                InputEvent::Paste { text } => self.secret_paste(&text),
                other => return self.handle_input_uncaptured(other),
            }
            return true;
        }
        let handled = self.handle_input_uncaptured(input);
        self.refresh_composer();
        handled
    }

    fn handle_input_uncaptured(&mut self, input: InputEvent) -> bool {
        match input {
            InputEvent::Key { key } => {
                let res = self.editor.feed_key(key);
                self.keys_pending = matches!(res, neosh_core::KeyResolution::Pending);
                self.keys_touched = true;
                self.drain_effects();
            }
            InputEvent::Paste { text } => {
                // At the cursor, replacing whatever is selected — the same edit typing performs,
                // because a paste that always lands at the end is not a paste.
                if self.reading {
                    self.leave_reading();
                }
                self.compose(TextEdit::Insert { text });
            }
            InputEvent::ViewportChanged { win, width, height, top_line } => {
                self.editor.set_viewport(win, neosh_core::Viewport { width, height, top_line });
            }
            InputEvent::Ready { .. } | InputEvent::Resize { .. } => {
                // A resize changes nothing the core owns; the frontend re-lays-out from the
                // declarative geometry it already has. Redraw so it takes effect.
                let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinScrollTo {
                    win: self.composer_win,
                    top_line: 0,
                });
            }
            InputEvent::Command { name, args } => {
                // Through the core's registry, so an unknown name reports "no such command" in the
                // UI exactly as a bad keybinding does.
                self.editor.exec_command(&name, args);
                self.drain_effects();
            }
            // Answered in the run loop, which is the only place with a frontend to draw to.
            InputEvent::Repaint => {}
            InputEvent::Disconnected => return false,
        }
        true
    }

    /// Emit one coalesced frame.
    async fn flush(&mut self) -> anyhow::Result<()> {
        let mut batch = self.editor.drain_ui();
        if batch.is_empty() {
            return Ok(());
        }
        batch.push(UiEvent::Flush);
        self.frontend.send(batch).await
    }

    pub async fn run(
        mut self,
        mut script_rx: mpsc::UnboundedReceiver<ScriptOutbound>,
        mut agent_rx: mpsc::UnboundedReceiver<AgentEvent>,
        mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    ) -> anyhow::Result<()> {
        // Send the initial state before anything else, so the frontend has a screen immediately.
        self.flush().await?;

        // A single long sleep, reset on demand. Creating a new one per iteration would restart the
        // window on every mutation and a fast stream would never reach its deadline.
        let idle = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(idle);
        let mut armed = false;

        // The other deadline: a half-typed binding that never gets its second key. Separate from
        // the redraw window because the two answer different questions and share no timing.
        let keys = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(keys);
        let mut waiting = false;

        // The elapsed clock on the working line. A third deadline rather than a share of the redraw
        // window, because it answers a different question: the redraw window asks "how long may a
        // burst of edits be batched", this asks "how often does a number that nobody edited change".
        // Armed only while a turn is in flight, so an idle workspace has no timer at all.
        let clock = tokio::time::sleep(Duration::from_secs(86_400));
        tokio::pin!(clock);
        let mut ticking = false;

        loop {
            tokio::select! {
                Some(out) = script_rx.recv() => self.handle_script_out(out).await,
                Some(ev) = agent_rx.recv() => self.handle_agent_event(ev),
                Some(input) = input_rx.recv() => {
                    // A repaint changes nothing, so it must not go through the input path: the
                    // frontend is asking to draw the same state again while something on it moves.
                    // Answering with an empty batch is what makes an animation unable to
                    // desynchronise from the text it is drawn over.
                    if matches!(input, InputEvent::Repaint) {
                        self.frontend.send(vec![UiEvent::Flush]).await?;
                        continue;
                    }
                    if !self.handle_input(input) {
                        break;
                    }
                }
                () = &mut idle, if armed => {
                    armed = false;
                    self.flush().await?;
                }
                () = &mut clock, if ticking => {
                    ticking = false;
                    self.draw_working();
                }
                () = &mut keys, if waiting => {
                    waiting = false;
                    self.keys_pending = false;
                    if self.editor.flush_pending() {
                        self.drain_effects();
                    }
                }
                else => break,
            }

            if self.quitting {
                break;
            }
            if std::mem::take(&mut self.keys_touched) {
                waiting = self.keys_pending;
                if waiting {
                    let ms = self.editor.options().int("timeoutlen").unwrap_or(500).max(0) as u64;
                    keys.as_mut().reset(Instant::now() + Duration::from_millis(ms));
                }
            }
            if self.working.is_some() && !ticking {
                clock.as_mut().reset(Instant::now() + Duration::from_secs(1));
                ticking = true;
            }
            if self.editor.has_pending_ui() && !armed {
                idle.as_mut().reset(Instant::now() + COALESCE);
                armed = true;
            }
        }

        // Last thing before the screen goes: whatever was said this session is on disk.
        self.persist_sessions();
        self.flush().await?;
        self.frontend.send(vec![UiEvent::Shutdown]).await?;
        self.frontend.shutdown().await?;
        Ok(())
    }

    // ---- startup --------------------------------------------------------

    /// Kick off startup: declare built-in options, apply config, then run the config scripts.
    ///
    /// Plugin discovery deliberately waits until the config scripts have activated, because
    /// `rtp.add` is how a config says what else should load.
    /// Find the repository we are running in, once.
    ///
    /// Called before startup so the status line and any plugin that asks on activation get a real
    /// answer rather than "no repository" followed by a correction. Running outside a repository is
    /// an ordinary state, not a failure.
    /// Restore saved conversations, and remember where to write them.
    ///
    /// Called before startup so the first thing on screen is the conversation you were in, not an
    /// empty one that gets replaced a moment later.
    pub async fn restore_sessions(&mut self, state_dir: Option<std::path::PathBuf>) {
        let Some(dir) = state_dir else { return };
        self.state_dir = Some(dir.clone());
        self.pstate = crate::pstate::PluginState::new(Some(dir.clone()));

        let (saved, active) = crate::sessions::load(&dir);
        if saved.is_empty() {
            return;
        }
        let restored = saved.len();
        {
            let mut store = self.agent.sessions();
            // The session the store was constructed with is empty and unsaved; replacing the
            // ordering below drops it from the list, but it must not linger as a phantom "New
            // conversation" beside the ones the user actually had.
            let placeholder = store.active_id().clone();
            let order: Vec<_> = saved.iter().map(|s| s.id.clone()).collect();
            for s in saved {
                store.insert(s);
            }
            // Never an archived one: with every conversation put away, the empty placeholder is
            // the right place to land, and the list is empty because that is what you asked for.
            let target = active.or_else(|| {
                order.iter().find(|id| store.get(id).is_some_and(|s| !s.archived)).cloned()
            });
            if let Some(id) = target
                && store.switch(&id).is_ok()
            {
                let _ = store.remove(&placeholder);
            }
            store.set_order(order);
        }
        // Every restored conversation may live in a different checkout — that is what makes a
        // second project, or a worktree, appear in the sidebar.
        let dirs: Vec<_> = {
            let store = self.agent.sessions();
            store.iter().map(|s| s.cwd.clone()).collect()
        };
        for dir in dirs {
            self.remember_repo(dir).await;
        }
        self.enter_session();
        tracing::info!("restored {restored} conversation(s)");
    }

    /// Persist every conversation. Cheap enough to call after each turn; a no-op under `--clean`.
    fn persist_sessions(&self) {
        let Some(dir) = &self.state_dir else { return };
        if let Err(e) = crate::sessions::save_all(dir, &self.agent.sessions()) {
            // Reported once, not per turn: a full disk should not turn into a message on every
            // keystroke, and losing history is not a reason to stop working.
            tracing::warn!("could not save conversations: {e}");
        }
    }

    pub async fn discover_repo(&mut self, cwd: &std::path::Path) {
        self.cwd = cwd.to_path_buf();
        self.remember_repo(cwd.to_path_buf()).await;
    }

    /// Look a directory up once and remember the answer, including "not a repository".
    ///
    /// The negative is cached too: a plain directory would otherwise cost a `git rev-parse` on
    /// every status refresh forever.
    async fn remember_repo(&mut self, dir: std::path::PathBuf) {
        if self.repos.contains_key(&dir) {
            return;
        }
        let found = neosh_vcs::Git::discover(&dir).await;
        if let Some(g) = &found {
            tracing::info!(dir = %dir.display(), root = %g.root().display(), "git repository");
        }
        self.repos.insert(dir, found);
    }

    pub fn begin_startup(&mut self, boot: Bootstrap) {
        self.seed_hints();
        self.draw_welcome();
        self.refresh_status();
        self.refresh_composer();
        self.startup.reload = Some(boot.reload);
        self.startup.base_instances =
            self.agent.providers().instances().cloned().collect();
        self.apply_config(boot.resolved, boot.cli_model, boot.cli_plugin_dirs);
    }

    /// Apply one resolved configuration. Shared by startup and reload — if reload took a different
    /// path, the two would drift and "works on a fresh start" would stop meaning anything.
    fn apply_config(
        &mut self,
        resolved: Resolved,
        cli_model: Option<String>,
        cli_plugin_dirs: Vec<std::path::PathBuf>,
    ) {
        for note in &resolved.notes {
            self.editor_message(note.level, note.text.clone());
        }

        // Re-established on every application, not just the first. A config that rebinds `<C-r>`
        // replaces the default binding; without re-registering, the reload that config triggers
        // would leave the key bound to nothing once the old config's keymaps were removed — and
        // the only way to reload is the key that just stopped working.
        self.declare_builtin_options();
        self.register_builtin_commands();

        // Providers and permissions are part of the configuration too. Applying only options and
        // scripts is what made `config.reload` report success while silently ignoring both.
        self.apply_providers(resolved.providers.clone());
        self.apply_permissions(&resolved.permissions);

        // `[agent]` in config.toml is spelled as the options it sets, so there is exactly one path
        // from "a value the user wrote" to "a value that took effect".
        let mut values = Vec::new();
        if let Some(m) = resolved.model {
            values.push(("agent.model".to_string(), OptionValue::Str(m)));
        }
        if let Some(p) = resolved.system_prompt {
            values.push(("agent.system_prompt".to_string(), OptionValue::Str(p)));
        }
        values.extend(resolved.options);
        for (name, value) in values {
            self.set_option_or_defer(name, value);
        }

        self.startup.cli_plugin_dirs = cli_plugin_dirs;
        self.startup.no_builtin_plugins = resolved.no_builtin_plugins;
        self.startup.plugin_dirs = resolved.plugin_dirs;
        self.startup.plugin_dirs.extend(self.startup.cli_plugin_dirs.clone());
        self.startup.cli_model = cli_model;
        self.startup.scripts = resolved.init_scripts.into();
        self.startup.discovered = false;

        self.next_config_script();
    }

    /// neosh's own commands.
    ///
    /// Registered through `ApiCall::CmdRegister` under the plugin id `neosh`, so they show up in
    /// `cmd.list()`, can be rebound with `keymap.set`, and can be invoked by a plugin — exactly
    /// like anyone else's. The default bindings are ordinary bindings; setting the same key in
    /// your config replaces them.
    fn register_builtin_commands(&mut self) {
        let plugin = PluginId::from(BUILTIN);
        let commands = [
            ("config.reload", "Re-read config.toml and init.ts, and reload every plugin"),
            ("quit", "Close neosh"),
            ("interrupt", "Stop what is running, or leave"),
            ("chat.scroll_up", "Scroll the transcript up a screen"),
            ("chat.scroll_down", "Scroll the transcript down a screen"),
            ("chat.scroll_end", "Jump to the end of the transcript"),
            ("chat.read", "Move into the transcript to navigate, select and copy it"),
            ("edit.copy", "Copy what is selected"),
            ("edit.cut", "Cut what is selected"),
            ("edit.select_all", "Select everything here"),
            ("edit.newline", "Break the line instead of sending"),
            // Built in rather than left to the model plugin: with `--clean`, or with plugins
            // disabled, this is the only way to authenticate, and "install a plugin first" is a
            // poor answer to "it says I have no API key".
            (
                "provider.key",
                "Enter an API key — for the named instance, or the one this model belongs to",
            ),
        ];
        for (name, desc) in commands {
            let _ = self.editor.apply(&plugin, ApiCall::CmdRegister {
                name: name.to_string(),
                desc: Some(desc.to_string()),
            });
        }
        // In every mode: a config that fails to load can leave you somewhere unexpected, and that
        // is precisely when you need to be able to reload.
        for mode in [Mode::Normal, Mode::Insert, Mode::Visual, Mode::Chat] {
            for (lhs, command, desc) in [
                ("<C-r>", "config.reload", "Reload configuration"),
                ("<C-q>", "quit", "Close neosh"),
                // Bound because it is the first thing anyone tries. In raw mode the terminal does
                // not turn this into SIGINT, so without a binding it does nothing at all — and a
                // program you cannot get out of is not one people give a second chance.
                ("<C-c>", "interrupt", "Cancel, clear, or quit"),
                ("<PageUp>", "chat.scroll_up", "Scroll up"),
                ("<PageDown>", "chat.scroll_down", "Scroll down"),
                ("<C-End>", "chat.scroll_end", "Back to the newest message"),
                // Reading the transcript is a separate place, and this is the door to it. Bound in
                // every mode for the same reason reload is: you want it most when you are somewhere
                // you did not intend to be.
                ("<C-s>", "chat.read", "Read, select and copy the transcript"),
                ("<C-x>", "edit.cut", "Cut the selection"),
                ("<C-a>", "edit.select_all", "Select everything"),
            ] {
                let _ = self.editor.apply(&plugin, ApiCall::KeymapSet {
                    mode,
                    lhs: lhs.to_string(),
                    command: command.to_string(),
                    scope: None,
                    desc: Some(desc.to_string()),
                });
            }
        }
    }

    /// Rebuild the instance list from the base plus whatever configuration names now.
    fn apply_providers(&mut self, from_config: Vec<neosh_proto::InstanceConfig>) {
        let mut reg = self.agent.providers_mut();
        reg.clear_instances();
        for i in self.startup.base_instances.clone() {
            reg.add_instance(i);
        }
        for i in from_config {
            reg.add_instance(i);
        }
    }

    /// Rebuild the permission layer.
    ///
    /// The workspace root comes from the layer being replaced rather than being re-derived: it is
    /// fixed for the life of the process, and a reload must not be able to move it.
    fn apply_permissions(&mut self, cfg: &crate::config::PermissionsConfig) {
        let workspace = self.agent.permissions().workspace().to_path_buf();
        let mut layer = neosh_agent::PermissionLayer::new(workspace).with_mode(cfg.mode);
        for c in &cfg.allow_commands {
            layer = layer.allow_command(c);
        }
        for h in &cfg.allow_hosts {
            layer = layer.allow_host(h);
        }
        for r in &cfg.allow_roots {
            layer = layer.allow_root(r);
        }
        self.agent.set_permissions(layer);
    }

    /// Everything that has to happen when a plugin stops being loaded, whether it is being
    /// reloaded or its `activate` threw partway through registering things.
    fn tear_down_plugin(&mut self, id: &PluginId) {
        let _ = self.bridge.script().send(ScriptInbound::Unload { plugin: id.clone() });
        self.editor.remove_plugin(id);
        self.agent.remove_plugin(id);
        self.bridge.forget(id);
        // Dropped with the plugin, so a reload that removes `vcs_write` from a manifest actually
        // removes the capability rather than leaving the previous grant behind.
        self.plugin_permissions.remove(id);
        // A segment outliving its plugin would leave a stale model name in the footer after a
        // reload removed the plugin that put it there.
        let prefix = format!("{id}:");
        self.status_segments.retain(|k, _| !k.starts_with(&prefix));
        // Same for its shortcuts: a row advertising `^P model` after the model plugin was unloaded
        // is a key that does nothing, which is worse than a row with one fewer entry on it.
        self.hints.retain(|k, _| !k.starts_with(&prefix));
        if let Some(drivers) = self.plugin_drivers.remove(id) {
            let mut reg = self.agent.providers_mut();
            for d in drivers {
                reg.remove_driver(&d);
            }
        }
    }

    /// `<C-c>`: stop the most local thing that is happening, and only leave if nothing is.
    ///
    /// The cascade matters more than any individual step. A key that sometimes quits and sometimes
    /// cancels is only tolerable if "quit" is always the *last* resort and always announced first.
    fn interrupt(&mut self) {
        const ARM_WINDOW: Duration = Duration::from_secs(3);

        // With something selected, `^C` copies — the behaviour of every terminal, and unambiguous
        // because the two cases cannot both be true. Without this the key that means "copy"
        // everywhere else in your day means "throw away what you were writing" here.
        if self.copy_selection(false) {
            self.quit_armed = None;
            return;
        }
        if self.reading {
            self.leave_reading();
            self.quit_armed = None;
            return;
        }
        if let Some(t) = self.active_turn.take() {
            t.cancel();
            self.quit_armed = None;
            self.editor_message(MessageLevel::Info, "interrupted");
            return;
        }
        if !self.composer_text().is_empty() {
            self.set_composer("");
            self.quit_armed = None;
            return;
        }
        let armed = self.quit_armed.is_some_and(|at| at.elapsed() < ARM_WINDOW);
        if armed {
            self.quitting = true;
            return;
        }
        self.quit_armed = Some(std::time::Instant::now());
        self.editor_message(MessageLevel::Info, "press ^C again to quit, or ^Q");
    }

    /// Page the transcript. `0` returns to following the tail.
    ///
    /// `top_line == 0` is the "follow the newest" state, which is why jumping to the end is a reset
    /// rather than a seek: the renderer already shows the last screenful when nothing has scrolled.
    fn scroll_chat(&mut self, direction: i32) {
        let plugin = PluginId::from(BUILTIN);
        let lines = self.editor.buffer(self.chat).map(|b| b.line_count() as u32).unwrap_or(0);
        let win = self.chat_win;
        let page = self
            .editor
            .window(win)
            .and_then(|w| w.viewport)
            .map(|v| v.height as u32)
            .unwrap_or(10)
            .max(1);
        let current = self.chat_top;

        let top = match direction {
            0 => 0,
            d if d < 0 => {
                // From "following", scrolling up starts from where the tail actually begins.
                let from = if current == 0 { lines.saturating_sub(page) } else { current };
                from.saturating_sub(page)
            }
            _ => {
                let next = current.saturating_add(page);
                // Past the end means back to following, so a user cannot scroll into blank space.
                if current == 0 || next + page >= lines { 0 } else { next }
            }
        };
        self.chat_top = top;
        let _ = self.editor.apply(&plugin, ApiCall::WinScrollTo { win, top_line: top });
    }

    fn run_builtin(&mut self, name: &str, args: Vec<String>) {
        match name {
            "config.reload" => self.reload_config(),
            "quit" => self.quitting = true,
            "interrupt" => self.interrupt(),
            "chat.scroll_up" => self.scroll_chat(-1),
            "chat.scroll_down" => self.scroll_chat(1),
            "chat.scroll_end" => self.scroll_chat(0),
            "chat.read" => self.enter_reading(),
            "edit.copy" => {
                if !self.copy_selection(false) {
                    self.editor_message(MessageLevel::Warn, "nothing selected");
                }
            }
            "edit.cut" => {
                if !self.copy_selection(true) {
                    self.editor_message(MessageLevel::Warn, "nothing selected");
                }
            }
            "edit.select_all" => {
                let win = if self.reading { self.chat_win } else { self.composer_win };
                self.select_all(win);
            }
            "edit.newline" => self.compose(TextEdit::Insert { text: "\n".into() }),
            "provider.key" => {
                // Named, or else the instance serving the model you are on — the one you would be
                // asked about, and the only one this could mean without a picker to choose from.
                let target = args
                    .first()
                    .map(|a| neosh_proto::InstanceId::from(a.as_str()))
                    .or_else(|| self.agent.selection().map(|s| s.instance));
                match target {
                    Some(instance) => {
                        if let Err(e) = self.begin_secret(instance, true, None) {
                            self.editor_message(MessageLevel::Warn, e.to_string());
                        }
                    }
                    None => self.editor_message(MessageLevel::Warn, "no model is selected"),
                }
            }
            other => tracing::warn!("no built-in command {other}"),
        }
    }

    /// Tear down everything configuration produced, then build it again from the files on disk.
    ///
    /// Deliberately whole-world rather than incremental: "re-read my configuration" is a semantic
    /// people can predict, and diffing two configurations to apply the delta is a source of bugs
    /// that only appear in the states nobody tested.
    fn reload_config(&mut self) {
        if self.startup.active.is_some() || self.startup.loading > 0 {
            if !self.startup.reload_pending {
                self.startup.reload_pending = true;
                self.editor_message(MessageLevel::Info, "still loading; reloading when it settles");
            }
            return;
        }
        let Some(reload) = self.startup.reload.as_ref() else {
            self.editor_message(MessageLevel::Error, "this session cannot reload configuration");
            return;
        };

        // Read the files *first*. A config that no longer parses should leave the running one
        // alone rather than tearing it down and having nothing to put back.
        let resolved = match reload() {
            Ok(r) => r,
            Err(e) => {
                self.editor_message(MessageLevel::Error, format!("reload failed, keeping the current config: {e}"));
                return;
            }
        };

        for id in self.bridge.loaded() {
            self.tear_down_plugin(&id);
        }
        // A reload may have added, removed or repointed a provider instance, so what was discovered
        // from the old set is no longer an answer to anything.
        self.model_cache.lock().expect("model cache poisoned").clear();

        // Reset the settings the old configuration changed, so deleting a line from `init.ts`
        // actually removes its effect. Sourcing a config that only ever *adds* is the thing that
        // makes people restart instead of reloading.
        let modified: Vec<String> = self
            .editor
            .options()
            .all()
            .into_iter()
            .filter(|o| o.modified && o.owner == BUILTIN)
            .map(|o| o.name)
            .collect();
        for name in modified {
            let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::OptReset { name });
        }
        self.drain_effects();

        self.startup.generation += 1;
        self.startup.deferred.clear();
        let model = self.startup.cli_model.clone();
        let dirs = self.startup.cli_plugin_dirs.clone();
        self.apply_config(resolved, model, dirs);
        self.editor_message(MessageLevel::Info, "configuration reloaded");
    }

    /// The options neosh itself owns.
    ///
    /// Declared through `ApiCall::OptDeclare` under the plugin id `neosh`, exactly as a plugin
    /// declares its own. If this ever needed a private path, the option API would be wrong.
    fn declare_builtin_options(&mut self) {
        let specs = [
            OptionSpec {
                name: "agent.model".into(),
                ty: OptionType::Str,
                default: OptionValue::Str(String::new()),
                description: Some(
                    "Model to use, as `instance/model`. Empty picks the first one that works."
                        .into(),
                ),
            },
            OptionSpec {
                name: "agent.system_prompt".into(),
                ty: OptionType::Str,
                default: OptionValue::Str(String::new()),
                description: Some("Replaces the built-in system prompt when set.".into()),
            },
            OptionSpec {
                name: "chat.markdown".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Render answers as markdown: headings, lists, code blocks, and inline \
                     emphasis with its markers removed. Off shows exactly what the model sent."
                        .into(),
                ),
            },
            OptionSpec {
                name: "chat.show_thinking".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(false),
                description: Some("Stream the model's reasoning into the chat buffer.".into()),
            },
            OptionSpec {
                name: "chat.show_tools".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some("Show a line in chat when a tool runs. Errors always show.".into()),
            },
            OptionSpec {
                name: "mapleader".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("\\".into()),
                description: Some(
                    "What `<leader>` means in a key binding. Read when the binding is made, as in \
                     Neovim — set it in `init.ts` before you use it."
                        .into(),
                ),
            },
            OptionSpec {
                name: "timeoutlen".into(),
                ty: OptionType::Int { min: Some(0), max: Some(10_000) },
                default: OptionValue::Int(500),
                description: Some(
                    "Milliseconds to wait for the rest of a multi-key binding before giving up and \
                     treating what was typed as ordinary input. Without this, binding a sequence \
                     whose first key is a printable character makes that character untypeable."
                        .into(),
                ),
            },
            // Widget keys. Space-separated lists in Neovim notation, so more than one key can mean
            // the same thing — `<Down>` and `<C-n>` both move down, and adding a third is a
            // setting rather than a fork. Declared by the host rather than by whichever plugin
            // happens to open a picker first, because two plugins declaring the same option name
            // is an error and every plugin uses this widget library.
            OptionSpec {
                name: "ui.keys.next".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<Down> <C-n> <C-j>".into()),
                description: Some("Move down in a picker or completion list.".into()),
            },
            OptionSpec {
                name: "ui.keys.prev".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<Up> <C-p> <C-k>".into()),
                description: Some("Move up in a picker or completion list.".into()),
            },
            OptionSpec {
                name: "ui.keys.page_down".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<PageDown>".into()),
                description: Some(
                    "Move down a screenful in a picker. Deliberately not `<C-d>` by default: a \
                     widget claims its keys while it is open, and taking a chord you use elsewhere \
                     for something you do rarely is a bad trade you only notice later."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.page_up".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<PageUp>".into()),
                description: Some("Move up a screenful in a picker.".into()),
            },
            OptionSpec {
                name: "ui.keys.first".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-Home>".into()),
                description: Some("Jump to the first row of a picker.".into()),
            },
            OptionSpec {
                name: "ui.keys.last".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-End>".into()),
                description: Some("Jump to the last row of a picker.".into()),
            },
            OptionSpec {
                name: "ui.keys.accept".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<CR>".into()),
                description: Some("Take the highlighted row, or what you typed.".into()),
            },
            OptionSpec {
                name: "ui.keys.dismiss".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<Esc> <C-c>".into()),
                description: Some("Close a picker or prompt without choosing.".into()),
            },
            OptionSpec {
                name: "ui.keys.complete".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<Tab> <Right>".into()),
                description: Some(
                    "Take the highlighted completion into the field without accepting it — how you \
                     descend a directory a segment at a time."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.clear".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-u>".into()),
                description: Some(
                    "Empty the filter or field. `<C-u>` because that is what it does in every \
                     other text field a terminal has ever shown you."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.delete_word".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-w>".into()),
                description: Some(
                    "Delete the word behind the cursor — a path segment, in a path field.".into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.pane_next".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<Tab> <Right>".into()),
                description: Some(
                    "Move to the next pane of a two-pane widget — from the provider rail to the \
                     models beside it. Shares `<Tab>` with `complete`, which is safe because no \
                     widget has both: a rail has nothing to complete and a path field has no rail."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.keys.pane_prev".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<S-Tab> <Left>".into()),
                description: Some("Move back a pane in a two-pane widget.".into()),
            },
            // Display settings every widget reads. Declared here rather than by whichever plugin
            // happens to load first: two plugins declaring one name is an error, and a plugin that
            // is switched off must not take the setting away from everything else — which is
            // exactly what happened when the sidebar owned these and `plugins.disabled` was used.
            OptionSpec {
                name: "ui.hints".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Show the shortcut row under the composer. Off gives the transcript the row \
                     back; the full list is still on the help key."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.ascii_only".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(false),
                description: Some(
                    "Use ASCII for glyphs, spinners and provider marks, for terminals without a \
                     decent font."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.motion".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Animate spinners and status pulses. Measured at ~1% of one core and under \
                     1 KiB/s, so this is on even over SSH; turn it off if you prefer a still \
                     screen."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.nerd_font".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(false),
                description: Some(
                    "Use Nerd Font glyphs where there is one — provider logos, chiefly. Off by \
                     default and not detected: a terminal cannot be asked what its font contains, \
                     and guessing wrong draws a row of boxes, which reads as a broken program \
                     rather than as a missing font."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.confirm_destructive".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Ask before anything that cannot be undone — deleting a conversation, removing \
                     a worktree. Turn it off once the keys are in your fingers; nothing that only \
                     changes what is on screen asks in the first place."
                        .into(),
                ),
            },
            OptionSpec {
                name: "ui.theme".into(),
                ty: OptionType::Enum { values: vec!["dark".into(), "light".into()] },
                default: OptionValue::Str("dark".into()),
                description: Some(
                    "Colour theme. Not auto-detected: no terminal reliably reports whether its \
                     background is light or dark, and guessing is wrong half the time."
                        .into(),
                ),
            },
            OptionSpec {
                name: "plugins.disabled".into(),
                ty: OptionType::List,
                default: OptionValue::List(Vec::new()),
                description: Some(
                    "Names of bundled plugins not to load, e.g. [\"sidebar\"]. Yours is not a \
                     bundled plugin; remove it from the plugin directory instead."
                        .into(),
                ),
            },
            OptionSpec {
                name: "gen.model".into(),
                ty: OptionType::Str,
                default: OptionValue::Str(String::new()),
                description: Some(
                    "Model for one-shot generation — branch names, commit messages, titles — as                      `instance/model`. Empty uses the conversation's model. Point it at something                      cheap; naming a branch does not need a frontier model."
                        .into(),
                ),
            },
        ];
        let plugin = PluginId::from(BUILTIN);
        for spec in specs {
            let name = spec.name.clone();
            if let Err(e) = self.editor.apply(&plugin, ApiCall::OptDeclare { spec }) {
                // Only reachable if two built-ins share a name, which is a bug here, not a user
                // error — but it must not be silent.
                tracing::error!("built-in option {name} could not be declared: {e:?}");
            }
        }
    }

    /// Set an option now, or hold it until whoever declares it has loaded.
    fn set_option_or_defer(&mut self, name: String, value: OptionValue) {
        if self.editor.options().get(&name).is_none() {
            self.startup.deferred.push((name, value));
            return;
        }
        let r = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::OptSet {
            name: name.clone(),
            value,
        });
        match r {
            Ok(_) => self.drain_effects(),
            Err(e) => self.editor_message(MessageLevel::Error, describe_api_error(&name, &e)),
        }
    }

    /// Retry held option values now that another plugin has declared its own.
    fn retry_deferred_options(&mut self) {
        let pending = std::mem::take(&mut self.startup.deferred);
        for (name, value) in pending {
            self.set_option_or_defer(name, value);
        }
    }

    /// Load the next config script, or move on to plugins when there are none left.
    fn next_config_script(&mut self) {
        match self.startup.scripts.pop_front() {
            Some((id, path)) => {
                self.startup.active = Some(id.clone());
                if !self.load_script(&id, &path) {
                    self.startup.active = None;
                    self.next_config_script();
                }
            }
            None => self.discover_plugins(),
        }
    }

    /// One `Loaded` arrived. Advance whichever phase it belongs to.
    fn advance_startup(&mut self, plugin: &PluginId) {
        if self.startup.active.as_ref() == Some(plugin) {
            self.startup.active = None;
            // A config script may have declared options of its own.
            self.retry_deferred_options();
            self.next_config_script();
            return;
        }
        if self.startup.loading > 0 {
            self.startup.loading -= 1;
            self.retry_deferred_options();
            if self.startup.loading == 0 {
                self.retry_pending_model();
                self.report_unknown_options();
                self.run_pending_reload();
            }
        }
    }

    /// Re-apply an `agent.model` that named an instance a plugin had not registered yet, then say
    /// so if there is still nothing to talk to.
    fn retry_pending_model(&mut self) {
        if let Some(spec) = self.startup.pending_model.take() {
            self.apply_option("agent.model", &OptionValue::Str(spec));
        }
        if self.agent.selection().is_none() {
            // A plugin may have registered the first usable instance a moment ago. The read guard
            // is released before anything takes `&mut self`.
            let fallback = self.agent.providers().default_selection();
            match fallback {
                Some(s) => {
                    self.agent.set_selection(s);
                    self.refresh_status();
                }
                None => self.editor_message(
                    MessageLevel::Warn,
                    "no usable model. Set `agent.model`, or run `neosh --list-models`.",
                ),
            }
        }
        // Last, because it can only judge a selection once every driver that might serve it has
        // registered — a plugin provider arriving late is exactly the case that would otherwise be
        // mistaken for "this model does not work".
        self.ensure_usable_selection();
        self.draw_welcome();
        self.refresh_composer();
    }

    /// Run a reload that arrived while startup was still in flight.
    fn run_pending_reload(&mut self) {
        if std::mem::take(&mut self.startup.reload_pending) {
            self.reload_config();
        }
    }

    /// Anything still deferred once every plugin has loaded names an option that does not exist.
    fn report_unknown_options(&mut self) {
        let leftover = std::mem::take(&mut self.startup.deferred);
        for (name, value) in leftover {
            self.editor_message(
                MessageLevel::Warn,
                format!(
                    "option {name:?} = {} is not declared by anything. Is the plugin that \
                     provides it installed?",
                    value.display()
                ),
            );
        }
    }

    /// Config has had its say; apply the flags that outrank it and load plugins.
    fn discover_plugins(&mut self) {
        self.startup.discovered = true;

        // Last, so `--model` beats every file. Nothing is more annoying than a flag that lost.
        if let Some(m) = self.startup.cli_model.take() {
            self.set_option_or_defer("agent.model".into(), OptionValue::Str(m));
        }
        if self.agent.selection().is_none()
            && let Some(s) = self.agent.providers().default_selection()
        {
            self.agent.set_selection(s);
        }
        // The "no usable model" warning waits until plugins have loaded: a session whose only
        // provider comes from a plugin has nothing to select at this point, and warning here would
        // be a false alarm the user cannot act on.

        // Bundled plugins load first, so a user plugin registering the same command or key wins by
        // loading after. Disabling one is `plugins.disabled = ["sidebar"]` rather than deleting a
        // file the user does not own.
        let disabled: Vec<String> = self
            .editor
            .options()
            .get("plugins.disabled")
            .and_then(|v| match v {
                OptionValue::List(items) => Some(items.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let builtins =
            if self.startup.no_builtin_plugins { Vec::new() } else { neosh_script::builtin_plugins() };
        for builtin in builtins {
            if disabled.iter().any(|d| d == &builtin.name) {
                tracing::info!(plugin = %builtin.name, "bundled plugin disabled by config");
                continue;
            }
            let id = PluginId(builtin.name.clone());
            match toml::from_str::<neosh_proto::PluginManifest>(&builtin.manifest) {
                Ok(m) => {
                    self.plugin_permissions.insert(id.clone(), m.permissions);
                }
                Err(e) => {
                    // A bundled manifest that does not parse is a build mistake. Skip it rather
                    // than loading code whose declared capabilities are unknown.
                    tracing::error!(plugin = %builtin.name, "bundled manifest is invalid: {e}");
                    continue;
                }
            }
            if self.load_url(&id, &builtin.url) {
                self.startup.loading += 1;
            }
        }

        let dirs = std::mem::take(&mut self.startup.plugin_dirs);
        let mut seen = std::collections::HashSet::new();
        for dir in dirs {
            if !seen.insert(dir.clone()) {
                continue;
            }
            let (found, errors) = neosh_script::discover(&dir);
            for e in errors {
                tracing::error!("plugin discovery: {e}");
                self.editor_message(MessageLevel::Error, e.to_string());
            }
            for plugin in found {
                self.plugin_permissions
                    .insert(plugin.id.clone(), plugin.manifest.permissions.clone());
                if self.load_script(&plugin.id, &plugin.entry) {
                    self.startup.loading += 1;
                }
            }
        }

        if self.startup.loading == 0 {
            self.retry_pending_model();
            self.report_unknown_options();
            // Nothing to wait for — a reload queued during config activation runs now.
            self.run_pending_reload();
        }
        self.refresh_status();
    }

    /// Ask the runtime to import and activate one entry module.
    ///
    /// A failure here is reported and skipped: one broken plugin — or one broken config script —
    /// must not stop the editor from starting.
    fn load_script(&mut self, id: &PluginId, entry: &std::path::Path) -> bool {
        let Ok(mut url) = url::Url::from_file_path(entry) else {
            self.editor_message(
                MessageLevel::Error,
                format!("{}: not an absolute path", entry.display()),
            );
            return false;
        };
        // On a reload, the generation makes this a URL deno_core has not cached, so the file is
        // read again. The loader strips it before touching the filesystem and propagates it to
        // relative imports.
        if self.startup.generation > 0 {
            url.set_query(Some(&format!("v={}", self.startup.generation)));
        }
        self.load_url(id, url.as_str())
    }

    /// Load an entry module by URL, whether it lives on disk or inside the binary.
    fn load_url(&mut self, id: &PluginId, url: &str) -> bool {
        let url = if self.startup.generation > 0 && !url.contains("?v=") {
            format!("{url}?v={}", self.startup.generation)
        } else {
            url.to_string()
        };
        match self.bridge.script().send(ScriptInbound::Load {
            plugin: id.clone(),
            url,
            config: serde_json::Value::Object(Default::default()),
            version: neosh_proto::PROTOCOL_VERSION,
        }) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(plugin = %id, "{e}");
                false
            }
        }
    }

}

/// Turn a refused option into something a user can act on.
fn describe_api_error(name: &str, e: &ApiError) -> String {
    match e {
        ApiError::NotFound { what } | ApiError::InvalidArgument { message: what } => what.clone(),
        other => format!("option {name}: {other:?}"),
    }
}

/// Register the built-in provider drivers and instances.
///
/// A free function rather than a method: listing models must not require a frontend, and a
/// frontend must not be required to know about providers.
/// Register every driver neosh ships, and hand back the ACP ones.
///
/// They come back because they need telling when the permission mode changes: an ACP agent asks
/// its *client* for permission, and the driver has to answer synchronously with no way to reach
/// the permission layer from inside a spawned turn. Mirroring the mode into the driver is the
/// narrowest thing that works — see [`neosh_provider::drivers::acp`] for why that is a stated
/// limitation rather than a design.
pub fn install_builtin_providers(agent: &Agent) -> Vec<Arc<neosh_provider::drivers::AcpProvider>> {
    let mut out = Vec::new();
    {
        let mut reg = agent.providers_mut();
        reg.register_driver(Arc::new(neosh_provider::drivers::AnthropicProvider));
        reg.register_driver(Arc::new(neosh_provider::drivers::OpenAiCompatProvider));
        reg.register_driver(Arc::new(neosh_provider::drivers::GoogleProvider));
        // Only when the CLI is actually there. It needs no key, so it is first in the catalogue
        // and would otherwise be what a fresh install defaults to — and every turn would fail with
        // "no such file", on the one path that is supposed to work with no setup at all. The
        // instance stays configured either way, so a settings list can say the driver is missing
        // rather than the provider simply not existing.
        let claude = neosh_provider::drivers::ClaudeCliProvider::default();
        if claude.available() {
            reg.register_driver(Arc::new(claude));
        }
        let codex = neosh_provider::drivers::CodexCliProvider::default();
        if codex.available() {
            reg.register_driver(Arc::new(codex));
        }
        // One driver, three programs: they all speak the Agent Client Protocol. Registered only
        // where the program exists, for the same reason as the other two — a provider whose every
        // turn fails with "no such file" is worse than one that is not offered.
        for (kind, program, args) in [
            ("cursor-cli", "cursor-agent", &["--acp"][..]),
            ("grok-cli", "grok", &["--acp"][..]),
            ("gemini-cli", "gemini", &["--experimental-acp"][..]),
        ] {
            let acp = Arc::new(neosh_provider::drivers::AcpProvider::new(kind, program, args));
            if acp.available() {
                reg.register_driver(acp.clone());
                out.push(acp);
            }
        }
        for i in catalog::builtin_instances() {
            reg.add_instance(i);
        }
    }
    out
}

/// Route one slow call to its service.
///
/// Free function rather than a method: it runs on a spawned task with no access to `Host`, which
/// is exactly the property that makes it safe to run while the host loop keeps drawing.
async fn run_slow(svc: Services, call: ApiCall) -> ApiResult {
    match call {
        ApiCall::PathComplete { prefix } => svc.path_complete(prefix).await,
        ApiCall::GitStatus => svc.git_status().await,
        ApiCall::GitBranches { include_remote } => svc.git_branches(include_remote).await,
        ApiCall::GitWorktrees => svc.git_worktrees().await,
        ApiCall::GitLog { limit } => svc.git_log(limit).await,
        ApiCall::GitDiff { target, stat } => svc.git_diff(target, stat).await,
        ApiCall::GitDefaultBranch => svc.git_default_branch().await,
        ApiCall::GitCreateBranch { name, from } => svc.git_create_branch(name, from).await,
        ApiCall::GitCheckout { rev } => svc.git_checkout(rev).await,
        ApiCall::GitStage { paths } => svc.git_stage(paths).await,
        ApiCall::GitUnstage { paths } => svc.git_unstage(paths).await,
        ApiCall::GitCommit { message } => svc.git_commit(message).await,
        ApiCall::GitAddWorktree { path, branch, create } => {
            svc.git_add_worktree(path, branch, create).await
        }
        ApiCall::GitRemoveWorktree { path, force } => svc.git_remove_worktree(path, force).await,
        ApiCall::GenComplete { prompt, system, json, selection } => {
            svc.gen_complete(prompt, system, json, selection).await
        }
        // Off the host loop for the same reason as git: discovery is a network round trip per
        // configured provider, and doing it inline freezes typing for as long as it takes.
        ApiCall::AgentListModels { instance, refresh } => svc.list_models(instance, refresh).await,
        ApiCall::PermissionCheck { capability } => svc.permission_check(capability).await,
        other => Err(ApiError::Internal {
            message: format!("{other:?} is not a background call"),
        }),
    }
}

/// Seconds since the epoch, or 0 if the clock is before it.
///
/// Zero rather than a panic: a wrong timestamp sorts a list oddly, and refusing to create a session
/// because the system clock is unset would be a much worse trade.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Render a conversation as chat-buffer lines.
///
/// Deliberately the *same* shape the live stream produces, so switching away and back does not
/// change what a turn looks like.
fn transcript(session: &neosh_agent::Session, ascii: bool) -> (Vec<String>, Vec<Mark>) {
    use neosh_proto::{ContentBlock, Role};

    let g = Glyphs::new(ascii);
    let mut lines: Vec<String> = Vec::new();
    let mut marks: Vec<Mark> = Vec::new();
    for message in &session.messages {
        for block in &message.content {
            match block {
                ContentBlock::Text { text } if message.role == Role::User => {
                    for line in text.lines() {
                        marks.push((lines.len() as u32, 0, g.bar.len(), "Agent.User"));
                        lines.push(format!("{} {line}", g.bar));
                    }
                    lines.push(String::new());
                }
                ContentBlock::Text { text } => {
                    // Through the same renderer the live stream uses, so switching away and back
                    // does not change what an answer looks like.
                    let at = lines.len() as u32;
                    let (rendered, styled) = crate::markdown::render(text);
                    marks.extend(styled.into_iter().map(|(r, a, b, g)| (at + r as u32, a, b, g)));
                    lines.extend(rendered);
                    lines.push(String::new());
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    let subject = subject_of(name, input);
                    let text = format!("  {} {name}{subject}", g.tool);
                    let split = 2 + g.tool.len() + 1 + name.len();
                    marks.push((lines.len() as u32, 0, split, "Agent.Tool"));
                    marks.push((lines.len() as u32, split, text.len(), "Agent.Usage"));
                    lines.push(text);
                }
                ContentBlock::ToolResult { is_error, content, .. } => {
                    let glyph = if *is_error { g.fail } else { g.result };
                    let summary = tool_summary(&neosh_proto::ToolResult {
                        content: content.clone(),
                        is_error: *is_error,
                    });
                    let text = format!("    {glyph} {summary}");
                    let hl = if *is_error { "Agent.ToolError" } else { "Agent.Usage" };
                    marks.push((lines.len() as u32, 0, text.len(), hl));
                    lines.push(text);
                }
                // Reasoning is not replayed: it is shown live when `chat.show_thinking` is on, and
                // reprinting it on every switch would bury the answer under the working out.
                ContentBlock::Thinking { .. } => {}
            }
        }
    }
    (lines, marks)
}

/// One highlighted span of the transcript: row, byte range, group.
type Mark = (u32, usize, usize, &'static str);

/// The characters the transcript is drawn with, at whatever fidelity the terminal has.
///
/// One place, because the live stream and the replay have to agree: a conversation that changes
/// shape when you switch away and back reads as two different programs having written it.
struct Glyphs {
    /// The bar down the left of a question.
    bar: &'static str,
    /// A tool being called.
    tool: &'static str,
    /// A tool that failed.
    fail: &'static str,
    /// What a tool came back with.
    result: &'static str,
    /// The working line's mark.
    work: &'static str,
}

impl Glyphs {
    fn new(ascii: bool) -> Self {
        if ascii {
            Self { bar: "|", tool: ">", fail: "x", result: "`-", work: "*" }
        } else {
            Self {
                bar: "\u{258c}",
                tool: "\u{23f5}",
                fail: "\u{2717}",
                result: "\u{2514}",
                work: "\u{2733}",
            }
        }
    }
}

/// A virtual-text chunk, which is three words of ceremony often enough to be worth a name.
fn chunk(text: impl Into<String>, hl: &str) -> neosh_proto::VirtChunk {
    neosh_proto::VirtChunk { text: text.into(), hl_group: Some(hl.to_string()) }
}

/// Screen columns a string occupies.
///
/// The host almost never needs this — columns are byte offsets and width is the frontend's job —
/// but the shortcut row is laid out here, and a row measured in `char`s wraps the moment a hint
/// uses `⇧` or `⏎`.
fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// A path with the home directory written the way people say it.
///
/// Cosmetic, and worth it: the welcome block is four lines and one of them should not be an
/// absolute path wide enough to wrap on its own.
fn tilde(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            let home = home.to_string_lossy().to_string();
            match text.strip_prefix(&home) {
                Some(rest) if rest.is_empty() => "~".to_string(),
                Some(rest) if rest.starts_with('/') => format!("~{rest}"),
                _ => text,
            }
        }
        _ => text,
    }
}
