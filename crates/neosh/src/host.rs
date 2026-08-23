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

use neosh_agent::{Agent, AgentEvent, PluginBridge, Prompt};
use neosh_core::{CoreEffect, Editor};
use neosh_proto::{
    ApiCall, ApiError, ApiOk, ApiResponse, ApiResult, BufferId, ContentBlock, CursorMotion, Dock,
    Gravity,
    InputEvent,
    KeyCode, MessageLevel, Mode, OptionSpec, OptionType, OptionValue, PluginEvent, PluginId,
    PluginInbound, PluginOutbound, PluginResponse, RequestId, SelectShape, TextEdit, UiEvent,
    VirtTextPos, WindowId, WindowLayout,
};
use neosh_provider::catalog;
use neosh_script::{ScriptInbound, ScriptOutbound};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation;

use crate::bridge::{PluginProvider, ScriptBridge};
use crate::cards::{self, Glyphs, ToolState};
use crate::config::{Config, Resolved};
use crate::frontend::Frontend;
use crate::services::Services;
use crate::vim;

/// How long mutations accumulate before a frame is emitted.
const COALESCE: Duration = Duration::from_millis(16);

/// The plugin id the host registers its own things under.
///
/// Not a special case in the core: the built-in UI, commands, keymaps and options are all owned by
/// a plugin that happens to be the host. If any of them needed a path around the registry, the API
/// would be missing something.
const BUILTIN: &str = "neosh";

/// Broadcast once every plugin has loaded. See [`Host::announce_ready`].
const READY_EVENT: &str = "neosh.ready";

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
    /// Commands a frontend asked for before anything had registered them.
    ///
    /// Held for the same reason `deferred` and `reload_pending` are: a command belongs to a plugin,
    /// plugins load in parallel with the terminal becoming usable, and for the first moment of a
    /// session `session.new` is a name nothing answers to. "No such command" is a wrong answer to
    /// a right question — and once startup has settled, a name still nobody claims is genuinely
    /// unknown and is reported exactly as before.
    commands: Vec<(String, Vec<String>)>,
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

/// What the transcript reader is waiting for, after a key that cannot act on its own.
///
/// One press each, and the key that follows spends it whatever that key turns out to be — a
/// mistyped second key ends the sequence rather than leaving the next unrelated press to be
/// swallowed by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// `g`, waiting for `g`, `e`, `E`, `_` or `v`.
    G,
    /// `z`, waiting for where on the screen to put the cursor's line.
    Z,
    /// `y`, waiting for what to copy — one of the transcript's own, or a motion.
    Yank,
    /// `f`, `F`, `t` or `T`, waiting for a character. Takes the next key *literally*, which is why
    /// it is answered before the count and before every table: `f5` finds a five.
    Find { till: bool, back: bool },
    /// `i` or `a`, waiting for which text object. `yank` is the difference between `viw` and `yiw`.
    Object { around: bool, yank: bool },
}

/// Which text object a key names.
fn object_for(ch: &str) -> Option<vim::Object> {
    Some(match ch {
        "w" => vim::Object::Word { big: false },
        "W" => vim::Object::Word { big: true },
        "p" => vim::Object::Paragraph,
        "\"" => vim::Object::Quote('"'),
        "'" => vim::Object::Quote('\''),
        "`" => vim::Object::Quote('`'),
        "(" | ")" | "b" => vim::Object::Pair('(', ')'),
        "[" | "]" => vim::Object::Pair('[', ']'),
        "{" | "}" | "B" => vim::Object::Pair('{', '}'),
        "<" | ">" => vim::Object::Pair('<', '>'),
        _ => return None,
    })
}

pub struct Host {
    editor: Editor,
    agent: Arc<Agent>,
    bridge: Arc<ScriptBridge>,
    frontend: Box<dyn Frontend>,

    chat: BufferId,
    chat_win: WindowId,
    /// Where the transcript is scrolled to, as the host intends it. `None` follows the newest.
    ///
    /// Not read back from the frontend's report: two page-ups inside one 16 ms coalescing window
    /// would both compute from the same stale value and the second would do nothing. The frontend
    /// reports what it drew; this is what we asked for.
    ///
    /// Following the tail is `None` and the first row is `Some(0)`, which are two different places
    /// — the frontend draws the last screenful for one and the first for the other. Sharing zero
    /// between them made the top of a long answer unreachable: `^U` up to it asked for row zero,
    /// which was read as "follow", and the transcript jumped back to the end.
    chat_top: Option<u32>,
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
    /// Drivers that run their own agent loop, kept so the permission mode can be mirrored into
    /// them and a request they cannot answer alone can reach a person. See ADR 0032.
    agent_drivers: Vec<Arc<dyn neosh_provider::drivers::AgentDriver>>,
    status: BufferId,
    status_ns: neosh_proto::NamespaceId,
    /// Where the transcript's own highlights live.
    ///
    /// Separate from the status line's so that redrawing the working line — which happens several
    /// times a second — cannot clear a mark on a message from ten turns ago.
    chat_ns: neosh_proto::NamespaceId,
    /// Whether the working line is currently the last row of the chat buffer.
    ///
    /// A fact about the buffer on screen, not about a turn: what the line *says* lives on the
    /// [`Round`], because the turn it reports on may be running in a conversation you are not
    /// looking at.
    working: bool,
    /// What plugins put in the status line, by key.
    ///
    /// A `BTreeMap` so iteration order is deterministic; the render sorts by priority anyway, but a
    /// stable base ordering is what stops two segments with equal priority swapping places between
    /// redraws.
    status_segments: std::collections::BTreeMap<String, neosh_proto::StatusSegment>,

    /// One entry per conversation with a turn in flight.
    ///
    /// A map rather than an `Option`, because a turn belongs to a conversation and not to the
    /// program. Switching used to be refused while one ran; now switching is just switching, and
    /// `<Esc>` cancels the turn of the conversation you are looking at.
    ///
    /// **An entry lives until [`AgentEvent::TurnEnded`] arrives, not until the turn is cancelled.**
    /// See [`Running::cancelling`] for what removing it early cost.
    turns: std::collections::HashMap<neosh_proto::SessionId, Running>,
    /// What each conversation with a turn in flight is doing, and what it has said so far in the
    /// round it is in the middle of.
    ///
    /// Kept for every running conversation, on screen or not: a round's text only reaches
    /// `session.messages` when its stream closes, so a transcript rebuilt from messages alone would
    /// show a conversation that is visibly working and has said nothing.
    rounds: std::collections::HashMap<neosh_proto::SessionId, Round>,
    /// What the mode was before `^S`, to be put back on the way out. Stored rather than assumed to
    /// be `Chat`: the door into reading is bound in every mode, so it is reachable from any of
    /// them, and a mode restored to a guess is a keyboard that quietly changed under you.
    mode_before_reading: Mode,
    /// What was in the composer when a change was last announced.
    ///
    /// Kept so [`neosh_proto::PluginEvent::ComposerChanged`] is about the draft rather than about
    /// the redraw.
    draft: String,
    /// How many rows of footer — the plan, and what is out — sit *under* the working line.
    ///
    /// Non-zero also means there is a blank line above the working line, which is the only other
    /// thing anyone needs to know to find either end of the block.
    ///
    /// Part of the working line and not of the transcript, because both are *state*: they say what
    /// is true now, not what happened. Torn down and rebuilt together by
    /// [`Self::below_working`], which is what keeps "the footer is the last rows of the buffer"
    /// true without anything having to remember where it is.
    plan_rows: u32,
    /// What each driver said it accepts as a slash command, by conversation.
    ///
    /// Learned at the driver's handshake, not configured: which commands exist depends on the
    /// install — project commands, plugin commands and MCP prompts all count — so any list written
    /// down here would be wrong on the first machine that had one of its own. Outlives the turn
    /// that learned it, because the menu is opened between turns — and outlives the *workspace*
    /// through [`Host::remembered_commands`], because it is also opened before them. A driver only
    /// says this once its process is up, which is on the first turn; a conversation you have just
    /// opened has not had one, and every command the agent accepts would be missing from the menu
    /// at exactly the moment somebody typed `/` looking for it.
    driver_commands:
        std::collections::HashMap<neosh_proto::SessionId, Vec<neosh_proto::DriverCommand>>,
    /// Every tool card in the transcript on screen, in row order.
    ///
    /// A body goes *under its own header*, not at the end of the transcript: an agent driver runs
    /// calls in parallel, so the result of the first can arrive after the header of the third, and
    /// appended it would sit under the wrong card. Inserting it moves every row below, which is
    /// why each card's row lives here rather than with whoever drew it. Rebuilt on every switch,
    /// because the transcript is redrawn from scratch and every row moves.
    cards: Vec<Card>,
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
    /// What the transcript is waiting for, when a key has been pressed that cannot act on its own.
    ///
    /// Cleared by the key that follows, whatever it is, so a mistyped second key ends the sequence
    /// instead of leaving the next unrelated press to be swallowed by it.
    reading_pending: Option<Pending>,
    /// A count typed before a motion in the transcript — `5j`, `12G`.
    ///
    /// Kept as the digits rather than a number so that it can be shown while it is half typed: a
    /// count is two keystrokes with nothing between them, and the first one is otherwise a press
    /// that appears to have done nothing at all. Ended by whatever motion spends it, and by any
    /// key that is not one.
    reading_count: String,
    /// What the running selection means. `Exclusive` when there is none.
    ///
    /// The core's, not a flag of our own: linewise used to be an invariant this end re-established
    /// after every motion — three API calls to move an anchor that is only settable by standing on
    /// it — and every path that moved the cursor without knowing to run it left the wrong rows lit.
    reading_shape: SelectShape,
    /// The column `j` and `k` are trying to get back to, as a grapheme-cluster index.
    ///
    /// Ours rather than the window's, because reading resolves its own motions: `$` then `j` then
    /// `j` walks down the right-hand edge of the transcript in Vim, and a column recomputed from
    /// wherever the cursor landed on a short row cannot do that.
    reading_goal: Option<u32>,
    /// The last `f`/`F`/`t`/`T`, so `;` and `,` have something to repeat.
    reading_find: Option<(char, bool, bool)>,
    /// The selection `gv` puts back: its two ends and its shape, kept when one is given up.
    reading_last_select: Option<((u32, u32), (u32, u32), SelectShape)>,
    /// The search being typed in the transcript, if one is.
    search: Option<SearchPrompt>,
    /// The last thing searched for, so `n` still works after the prompt has closed.
    searched: String,
    /// Which way it was going, because `n` means "onwards" rather than "forwards": after `?foo`,
    /// `n` is up the transcript and `N` is down it.
    searched_back: bool,
    /// Highlights for search hits. Its own namespace so clearing them cannot take the transcript's
    /// own colour with it.
    search_ns: neosh_proto::NamespaceId,
    /// Where a question sits that nothing has answered yet, as the first row of its block.
    ///
    /// Set when a question is drawn and cleared by the first thing drawn under it, so it is
    /// exactly "the transcript ends with something you asked and nothing else". Two things need
    /// that fact and both of them were bugs.
    ///
    /// A turn's *closing* rows — its plan, what it left running, what it changed — are about the
    /// answer they close. Appended at the end of the buffer they landed **under** a question
    /// steered in after the last of that answer, so a message you had just typed was not the last
    /// thing in the transcript: two blocks belonging to the previous exchange sat below it. They
    /// go here instead, which is where the turn's own output stopped.
    ///
    /// And a question still unanswered when the turn ends got no reply at all — which is a thing
    /// to *say*, because a question with nothing under it is indistinguishable from one still
    /// being thought about. See [`Host::close_unanswered`].
    unanswered: Option<u32>,
    /// Set while output is streaming, so chunks append rather than starting a new line.
    streaming: Option<Stream>,
    /// The answer currently being written, and where it sits in the transcript.
    answer: Option<Answer>,
    startup: Startup,
    /// Set by the `stop` command — and by `quit`, when this host *is* the process; the run loop
    /// notices and shuts down cleanly.
    quitting: bool,
    /// What `quit` means for this host.
    ///
    /// Two different things, and they used to be the same thing because there was only ever one
    /// place to be. In a workspace serving a terminal, `^Q` closes the *terminal*: the turns keep
    /// running, the plugins stay loaded, and reattaching puts you back where you were. Stopping
    /// the workspace is a thing you say on purpose, with `neosh stop`.
    on_quit: OnQuit,
    /// Set by `quit` while serving; the run loop sends the viewer away and carries on.
    /// The terminal that asked to leave, if one has. `^Q` closes *that* window and no other.
    ///
    /// Which view is not a guess from whoever spoke most recently: input arrives already tagged
    /// with the view it came from, and this is set while that very event is being handled.
    detaching: Option<crate::views::ViewId>,
    /// The view whose input is being handled right now.
    ///
    /// [`crate::views::ViewId::LOCAL`] between events and in a process that is its own terminal.
    /// A command run from a plugin or a signal is nobody's key press, and lands here as the local
    /// view — which in a served workspace names no terminal, and so closes none.
    from_view: crate::views::ViewId,
    /// Where to report what this workspace is holding, when it is a workspace rather than a
    /// process somebody is watching.
    live: Option<std::sync::Arc<crate::daemon::Live>>,
    /// When that report was last made, so a streaming turn does not remake it per frame.
    live_at: Option<std::time::Instant>,
    /// How many turns were running when it was made. A change here skips the rate limit: whether
    /// something is running is the one thing a status query is actually about, and answering it a
    /// second late is answering the wrong question.
    live_turns: usize,
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
    /// Pictures attached to a message that has not been sent yet, per conversation.
    ///
    /// Per conversation for the reason the draft is: `^T` away mid-sentence and back again has to
    /// find what you were writing where you left it, and an image you pasted is part of what you
    /// were writing. Emptied when the message goes.
    attached: std::collections::HashMap<neosh_proto::SessionId, Vec<crate::images::Attachment>>,
    /// What plugins remember between runs: pinned projects, fold state, whatever a panel needs to
    /// still be arranged the way you left it.
    pstate: crate::pstate::PluginState,
    /// What *everybody* remembers about a project or a conversation — favourites, colours, tags.
    ///
    /// Separate from `pstate` because it is separate in kind: state is private to whoever wrote it,
    /// a var describes a thing and is shared. See [`crate::vars`].
    vars: crate::vars::Vars,
    /// What the other machines are running. Empty and inert unless `[swarm]` names a peer.
    swarm: crate::swarm::Swarm,
    /// The running node, if there is one. `None` when the swarm is off, which is the default.
    swarm_node: Option<neosh_swarm::SwarmHandle>,
    /// Commands sent to peers that have not been answered yet, and who to settle when they are.
    swarm_pending: std::collections::HashMap<String, (PluginId, RequestId)>,
    swarm_next_command: u64,
    /// Local conversations somebody on another machine is watching.
    ///
    /// Kept so the turn path can ask "is anyone looking" with a hash lookup rather than building a
    /// wire event per token and handing it to the swarm to throw away. On a workspace nobody is
    /// watching — which is most of them, most of the time — streaming costs one set lookup per
    /// token and nothing else.
    swarm_watched: std::collections::HashSet<neosh_proto::SessionId>,
    /// Machines that proved who they are and are not paired with.
    ///
    /// Kept rather than only notified about: pairing is a decision a person makes at their own
    /// pace, and a machine that asked while you were reading something else should still be
    /// waiting in the list when you look. Reset on restart, because a request that old is one to
    /// make again.
    swarm_strangers: std::collections::HashMap<neosh_proto::NodeId, neosh_proto::SwarmStranger>,
    /// The authorisation list, shared with the running node so pairing needs no restart.
    swarm_allowed: neosh_swarm::Allowed,
    /// The node's events, held here between construction and the loop taking them.
    swarm_events: Option<tokio::sync::mpsc::UnboundedReceiver<neosh_swarm::SwarmEvent>>,
    /// What each account has left on its plan, kept between the moments anything says so.
    ///
    /// In the host and not in a plugin for the reason every capability here is: it reads a
    /// credential and it outlives every window that draws it. What a plugin gets is the numbers.
    quota: crate::quota::QuotaStore,
    /// Where a poll's answer comes back to. Polls are network round trips and process spawns, so
    /// they cannot happen on the host loop — the single writer for the editor — and cannot be
    /// awaited by whoever asked, because nobody asked: the interval did.
    quota_tx: tokio::sync::mpsc::UnboundedSender<crate::quota::Polled>,
    quota_rx: Option<tokio::sync::mpsc::UnboundedReceiver<crate::quota::Polled>>,
    /// Set by a turn ending, cleared by the poll that follows it. See [`crate::quota::AFTER_TURN`].
    quota_due: bool,
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
    /// What to call each directory a conversation lives in — and what checkout it is, so a list
    /// can group worktrees under the repository they belong to. Worked out once when the
    /// directory is first seen.
    projects: std::collections::HashMap<std::path::PathBuf, ProjectFacts>,
    /// What each directory is called *across machines*. See [`neosh_proto::ProjectKey`].
    ///
    /// Separate from `projects`, which is a label for this screen. This is an identity, resolved
    /// from the git remote once when a directory is first seen and remembered, because a workspace
    /// that ran `git remote get-url` on every inventory publish would run it several times a
    /// second while a turn is going.
    project_keys: std::collections::HashMap<std::path::PathBuf, neosh_proto::ProjectKey>,
    /// What to call this machine to other machines.
    swarm_name: String,
    /// A key being typed in, if one is.
    ///
    /// The host collects this itself rather than lending a plugin a text field, because a plugin
    /// field is a buffer, a buffer is drawn, and drawing it would put the key in a `UiEvent` — over
    /// a process boundary, into whatever the frontend logs. Here the value never leaves this
    /// struct: what gets drawn is a row of bullets.
    secret: Option<SecretPrompt>,
}

/// What `quit` does.
///
/// Not a setting: it follows from whether this host is the process you are looking at or a
/// workspace something else is looking at, and nothing else gets a vote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OnQuit {
    /// This host is the program. `quit` ends it.
    Stop,
    /// This host is a workspace with a viewer. `quit` sends the viewer away.
    Detach,
}

/// A turn in flight, in whichever conversation it belongs to.
///
/// The working line exists because the gap between pressing Enter and the first token is the
/// longest silence in the program, and a still screen and a wedged process look identical. It
/// carries an elapsed clock for the same reason: "working" with no number is indistinguishable
/// from "stuck".
///
/// This is per conversation rather than per screen, so that switching away from a turn and back
/// shows the same clock it would have shown had you never left.
struct Round {
    /// The word this turn is using. Chosen once per turn — a verb that changes every tick is a
    /// slot machine, and you end up watching it instead of thinking.
    verb: &'static str,
    started: std::time::Instant,
    /// What it is waiting on, when that is something other than the model.
    note: Option<String>,
    /// Everything this turn has produced that the conversation does not hold yet.
    ///
    /// Cleared the moment the turn's messages land in the conversation, because from then on the
    /// messages are the truth and this would be a second copy of them. How long that takes is the
    /// whole reason this exists: for a model driver it is the end of each round, but an agent
    /// driver runs its entire loop inside one stream and commits nothing until the turn is over.
    /// Without a record of its own, switching away from such a turn and back left you looking at
    /// your own question and nothing else.
    said: Vec<Said>,
    /// What this turn has changed so far, per file, for the summary under its answer.
    ///
    /// Apart from `said`, which is cleared every time the conversation takes the round's messages
    /// — a model driver does that at the end of every round, and the summary is about the turn.
    changes: Vec<cards::FileStat>,
    /// The agent's own checklist, as of its last report.
    ///
    /// Whole, not patched: the driver sends the whole list every time it changes, and a plan
    /// assembled from increments is only right if every one of them arrived.
    plan: Vec<neosh_proto::PlanStep>,
    /// The sub-agents this turn has out, in the order they started.
    ///
    /// Kept after they finish rather than removed, so a late report about one that already ended
    /// lands somewhere instead of resurrecting it.
    tasks: Vec<Task>,
}

/// One sub-agent, from the turn's point of view.
///
/// What it *found* is not here: that comes back as the result of the call that spawned it, on that
/// call's own card, which is where a reader is already looking. This is the part the card cannot
/// answer while it is still out — whether it is still going, and how long it has been.
struct Task {
    id: neosh_proto::TaskId,
    title: String,
    role: Option<String>,
    /// When this turn heard about it. Not when it started: a conversation resumed with a task
    /// already running hears about it at the first report, and a clock from then is honest about
    /// what it is measuring in a way one counting from an invented start would not be.
    since: std::time::Instant,
    status: neosh_proto::TaskStatus,
}

impl Task {
    /// Whether it belongs in the footer. Running, obviously — and backgrounded, because
    /// "something is still going that nothing is waiting for" is the one people ask about.
    fn listed(&self) -> bool {
        matches!(
            self.status,
            neosh_proto::TaskStatus::Running | neosh_proto::TaskStatus::Backgrounded
        )
    }
}

/// One call a card is holding: what was asked, and what came back.
struct Leg {
    id: neosh_proto::ToolCallId,
    name: String,
    input: serde_json::Value,
    result: Option<neosh_proto::ToolResult>,
    /// When the call started, for a card that was here to see it start.
    ///
    /// `None` for one rebuilt from the conversation: the messages say what happened and not when,
    /// so a restored transcript has no clock on it. Inventing one from the moment the buffer was
    /// drawn would put a number that means "how long ago you pressed `^T`" where a duration goes.
    started: Option<std::time::Instant>,
    /// How long it took, once it came back.
    took: Option<std::time::Duration>,
    /// What a failed call exited with, when its output said.
    exit: Option<i64>,
}

/// A tool card on screen: where it is, and what it shows.
///
/// What it shows is kept so it can be drawn again — folded or opened from the transcript, or
/// finished when the call comes back — without going looking for the call in the conversation,
/// which for a running agent turn does not hold it yet.
///
/// Usually one call. A *run* of calls that only looked at things — reads, greps, listings, one
/// after another with nothing drawn between them — shares a card, and the card is one row saying
/// what they all looked at. See [`cards::group_header`]. Nothing else ever shares: a command's
/// output and an edit's diff are the answer, and there is no summarising them into a list of
/// names.
struct Card {
    /// One, or a run of read-only calls, in the order they were made.
    legs: Vec<Leg>,
    /// The header's row.
    row: u32,
    /// How many rows the body takes under it.
    body: u32,
    /// Whether the body is the whole of it or the first few rows.
    expanded: bool,
    /// Whether this card has already had its moment.
    ///
    /// A landing is an event, and an event happens once. Without this the row would light up again
    /// every time the card is redrawn for any other reason — opening it, folding it, the clock on
    /// the row above it ticking — and a flash that fires on a redraw is a flash that means
    /// "something was drawn", which is not news.
    flashed: bool,
}

impl Card {
    fn new(leg: Leg, row: u32) -> Self {
        Self { legs: vec![leg], row, body: 0, expanded: false, flashed: false }
    }

    /// Whether this card holds a call answering to `id`, and which one.
    fn leg_of(&self, id: &neosh_proto::ToolCallId) -> Option<usize> {
        self.legs.iter().rposition(|l| l.id == *id)
    }

    /// Whether every call on this card has come back.
    fn settled(&self) -> bool {
        self.legs.iter().all(|l| l.result.is_some())
    }

    /// Whether a call `input` could join this card.
    ///
    /// Both sides have to be calls that only looked at something, and the card has to have room
    /// for the idea — a card whose body is already on screen is a card the reader has opened, and
    /// growing it under them would move what they were reading.
    fn takes(&self, input: &serde_json::Value) -> bool {
        !self.expanded
            && self.legs.last().is_some_and(|l| cards::groups_with(&l.input, input))
    }
}

/// A turn the host started and has not yet seen end.
struct Running {
    cancel: CancellationToken,
    /// The turn has been asked to stop — `<Esc>`, or a peer's `Interrupt` — and is winding down.
    ///
    /// It is a flag rather than the absence of an entry, and that is the whole point. Cancelling
    /// used to *remove* the entry, so "is a turn running in this conversation" went false while the
    /// turn was still ending: a message typed in that gap started a **second** turn in the same
    /// conversation, and then the first turn's `TurnEnded` — which removes by conversation, having
    /// no way to tell whose ending it is — tore down the second one's cancellation token and its
    /// [`Round`]. What that looked like was a turn nothing could interrupt, a spinner frozen at
    /// whatever second it had reached, and a third `\u{23ce}` starting yet another turn on a vendor
    /// CLI that keeps one process per conversation.
    ///
    /// Kept, the map is exactly "turns the host is waiting to hear the end of", every entry is
    /// removed by the ending it belongs to, and a message typed while a turn winds down is queued
    /// and started by that ending — which is what the leftover-steering path already did.
    cancelling: bool,
}

/// One thing a running turn has produced, in the order it produced it.
///
/// The same three shapes [`transcript`] draws from a finished conversation, which is the point:
/// a round replayed from here and the same round replayed from messages a second later have to
/// look identical, or switching conversations would rewrite what you had already read.
enum Said {
    Text(String),
    Tool { call: neosh_proto::ToolCall, result: Option<neosh_proto::ToolResult> },
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
/// A search being typed in the transcript.
///
/// It borrows the composer to be typed into, the same way entering a key does, because the
/// alternative is a float over the thing you are searching — which hides the answer at the moment
/// it appears.
struct SearchPrompt {
    query: String,
    /// Opened with `?` rather than `/`, so `n` goes backwards.
    back: bool,
    /// Where the cursor was when it opened. Esc puts it back, because an abandoned search should
    /// leave you where you were rather than wherever the incremental match wandered to.
    from: (u32, u32),
    /// The draft that was in the composer, put back afterwards.
    draft: String,
}

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
        let (quota_tx, quota_rx) = tokio::sync::mpsc::unbounded_channel();

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
                wrap: None,
            });

        // Docked before the composer, so it takes the bottom-most row and the composer sits above
        // it. Without a status line the startup screen renders literally nothing — two empty
        // buffers — and "it opened an empty terminal" is indistinguishable from "it crashed".
        let status = editor.create_buffer("[status]");
        editor.open_window(status, WindowLayout::Docked { dock: Dock::Bottom, size: Some(1), gravity: Gravity::Start, wrap: None });

        let composer = editor.create_buffer("[composer]");
        // Four rows at rest: a rule, up to three lines of what you are writing, and the shortcut
        // row that lives on the last of them as a virtual line. The rule is what makes the composer
        // read as a field rather than as the last paragraph of the transcript. `wrap` is what makes
        // a long prompt fold and the window grow to show it, instead of running off the right edge.
        let composer_win =
            editor.open_window(composer, WindowLayout::Docked { dock: Dock::Bottom, size: Some(4), gravity: Gravity::Start, wrap: Some(true) });
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
        let search_ns = namespace("neosh.search");

        let mut host = Self {
            editor,
            agent,
            bridge,
            frontend,
            chat,
            chat_win,
            chat_top: None,
            composer,
            composer_win,
            status,
            status_ns,
            chat_ns,
            composer_ns,
            working: false,
            status_segments: Default::default(),
            hints: Default::default(),
            welcome_rows: 0,
            selection_pinned: false,
            agent_drivers: Vec::new(),
            turns: std::collections::HashMap::new(),
            rounds: std::collections::HashMap::new(),
            mode_before_reading: Mode::Chat,
            draft: String::new(),
            plan_rows: 0,
            driver_commands: std::collections::HashMap::new(),
            cards: Vec::new(),
            keys_pending: false,
            keys_touched: false,
            reading: false,
            reading_pending: None,
            reading_count: String::new(),
            reading_shape: SelectShape::Exclusive,
            reading_goal: None,
            reading_find: None,
            reading_last_select: None,
            search: None,
            searched: String::new(),
            searched_back: false,
            search_ns,
            unanswered: None,
            streaming: None,
            answer: None,
            startup: Startup::default(),
            quitting: false,
            on_quit: OnQuit::Stop,
            detaching: None,
            from_view: crate::views::ViewId::LOCAL,
            live: None,
            live_at: None,
            live_turns: 0,
            quit_armed: None,
            plugin_drivers: Default::default(),
            plugin_permissions: Default::default(),
            model_cache: Default::default(),
            state_dir: None,
            attached: Default::default(),
            pstate: crate::pstate::PluginState::new(None),
            vars: crate::vars::Vars::new(None),
            swarm: Default::default(),
            swarm_node: None,
            swarm_pending: Default::default(),
            swarm_next_command: 0,
            swarm_watched: Default::default(),
            swarm_strangers: Default::default(),
            swarm_allowed: neosh_swarm::Allowed::load(None),
            swarm_events: None,
            quota: crate::quota::QuotaStore::new(None),
            quota_tx,
            quota_rx: Some(quota_rx),
            quota_due: false,
            repos: Default::default(),
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            projects: Default::default(),
            project_keys: Default::default(),
            swarm_name: machine_name(),
            secret: None,
        };
        // Before anything reads one. Conversations are restored — and therefore drawn — before the
        // configuration is resolved, and an option that has not been declared yet reads as the
        // zero of its type: `chat.show_tools` came back false, so a restart showed a transcript
        // with every tool card missing and only the errors left, which is the one thing that
        // setting is documented not to do. `apply_config` declares them again on every reload,
        // which is what keeps a rebound default from surviving the config that replaced it.
        host.declare_builtin_options();
        host
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
                | ApiCall::GitWorktrees { .. }
                | ApiCall::GitLog { .. }
                | ApiCall::GitDiff { .. }
                | ApiCall::GitDefaultBranch
                | ApiCall::GitCreateBranch { .. }
                | ApiCall::GitCheckout { .. }
                | ApiCall::GitStage { .. }
                | ApiCall::GitUnstage { .. }
                | ApiCall::GitCommit { .. }
                | ApiCall::GitPull { .. }
                | ApiCall::GitAddWorktree { .. }
                | ApiCall::GitRemoveWorktree { .. }
                | ApiCall::GenComplete { .. }
                | ApiCall::AgentListModels { .. }
                | ApiCall::PermissionCheck { .. }
                | ApiCall::PathComplete { .. }
                | ApiCall::AskUser { .. }
                | ApiCall::UsageHistory { .. }
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

    /// What a plugin must have declared in its `plugin.toml` to make this call, if anything.
    ///
    /// Most of the API needs nothing: drawing a window, reading a buffer and binding a key are no
    /// more privileged than what the person sitting there can already do. These four are the ones
    /// that are *about* something other than the screen, and each was already declarable —
    /// [`neosh_proto::PluginPermission`] has listed them since it existed, `neosh plugins` prints
    /// them, and a user reading a manifest was entitled to believe them.
    ///
    /// They were not checked. Only `vcs_write` ever was, which made the rest documentation that
    /// looked like a boundary: a plugin declaring nothing could register a tool the model calls,
    /// take a blocking hook and veto every turn, or stand in as a model provider. That is the wrong
    /// way round for a workspace that runs other people's code and, on the swarm, takes commands
    /// from other machines.
    ///
    /// A non-blocking hook needs nothing: an observer cannot refuse anything, which is exactly the
    /// distinction `hooks_blocking` names.
    fn permission_for(call: &ApiCall) -> Option<neosh_proto::PluginPermission> {
        use neosh_proto::PluginPermission as P;
        match call {
            ApiCall::ToolRegister { .. } => Some(P::Tools),
            ApiCall::HookRegister { blocking: true, .. } => Some(P::HooksBlocking),
            ApiCall::ProviderRegisterDriver { .. } => Some(P::Providers),
            ApiCall::SurfaceClaim { .. } => Some(P::RawCells),
            _ => None,
        }
    }

    /// Whether `plugin` declared `want`.
    ///
    /// The host's own calls are not a plugin's and are not gated: `BUILTIN` is the binary drawing
    /// its own UI, and it has no manifest to declare anything in.
    fn may(&self, plugin: &PluginId, want: neosh_proto::PluginPermission) -> bool {
        plugin.0 == BUILTIN
            || self.plugin_permissions.get(plugin).is_some_and(|p| p.contains(&want))
    }

    /// Route a plugin call: UI-domain calls to the core, agent-domain calls to the agent layer.
    async fn dispatch(&mut self, plugin: &PluginId, call: ApiCall) -> ApiResult {
        if let Some(want) = Self::permission_for(&call)
            && !self.may(plugin, want)
        {
            let name = serde_json::to_value(want)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{want:?}"));
            // Says the fix, not just the refusal. A plugin author reading this in a log has one
            // line to act on rather than a capability name and a guess about where it goes.
            return Err(ApiError::NotPermitted {
                capability: format!(
                    "{name} (plugin {plugin} did not declare it \u{2014} add \"{name}\" to the \
                     permissions list in its plugin.toml)"
                ),
            });
        }
        if Editor::handles(&call) {
            let r = self.editor.apply(plugin, call);
            self.drain_effects();
            return r;
        }
        self.agent_call(plugin, call).await
    }

    async fn agent_call(&mut self, plugin: &PluginId, call: ApiCall) -> ApiResult {
        match call {
            ApiCall::AgentSend { text, images } => {
                // A plugin's own images go on the row first, so they travel with whatever is
                // already there and in the order they were added — the send is one message.
                for path in &images {
                    if let Err(e) = self.attach(Some(std::path::Path::new(path))) {
                        return Err(ApiError::InvalidArgument { message: e });
                    }
                }
                self.start_turn(text);
                Ok(ApiOk::Unit)
            }
            ApiCall::AgentCommand { session, command } => {
                let session = session.unwrap_or_else(|| self.active_session());
                let session = self
                    .apply_agent_command(&session, command)
                    .map_err(|r| match r {
                        neosh_proto::Refusal::NoSuchAgent => ApiError::NotFound {
                            what: format!("conversation {session}"),
                        },
                        neosh_proto::Refusal::NotPermitted { what } => {
                            ApiError::Denied { reason: what }
                        }
                        neosh_proto::Refusal::Busy { message }
                        | neosh_proto::Refusal::Failed { message } => {
                            ApiError::InvalidArgument { message }
                        }
                    })?;
                Ok(ApiOk::MaybeSession { session })
            }
            ApiCall::ChatAttach { path } => {
                let got = self
                    .attach(path.as_deref().map(std::path::Path::new))
                    .map_err(|message| ApiError::InvalidArgument { message })?;
                Ok(ApiOk::Attachments { attachments: vec![info_of(&got)] })
            }
            ApiCall::ChatAttachments => Ok(ApiOk::Attachments {
                attachments: self
                    .attached
                    .get(&self.active_session())
                    .map(|v| v.iter().map(info_of).collect())
                    .unwrap_or_default(),
            }),
            ApiCall::ChatDetach { index } => {
                let here = self.active_session();
                let row = self.attached.entry(here).or_default();
                // The newest by default: it is the one you just added, and therefore the one you
                // meant. Out of range is not an error — the row may have gone out with a send
                // between asking and answering, and there is nothing to put right.
                let gone = match index {
                    Some(i) if (i as usize) < row.len() => Some(row.remove(i as usize)),
                    Some(_) => None,
                    None => row.pop(),
                };
                self.refresh_composer();
                Ok(ApiOk::Attachments {
                    attachments: gone.iter().map(info_of).collect(),
                })
            }
            ApiCall::ChatDetachAll => {
                let here = self.active_session();
                let gone = self.attached.remove(&here).unwrap_or_default();
                self.refresh_composer();
                Ok(ApiOk::Attachments { attachments: gone.iter().map(info_of).collect() })
            }
            ApiCall::AgentCancel => {
                self.cancel_turn_here();
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
                self.select_model(selection);
                Ok(ApiOk::Unit)
            }
            ApiCall::AgentDriverCommands => {
                let here = self.active_session();
                let commands = match self.driver_commands.get(&here) {
                    Some(c) => c.clone(),
                    // Not this conversation's own answer, but the last one given for the same
                    // driver in the same project — which is what its handshake is about to say
                    // again. Being one install-change out of date is the cost; the alternative is a
                    // menu that knows none of the agent's commands until you have already sent
                    // something, which is the state the menu is least useful in.
                    None => self.remembered_commands(),
                };
                Ok(ApiOk::DriverCommands { commands })
            }
            ApiCall::ChatSetDraft { text } => {
                self.set_composer(&text);
                self.refresh_composer();
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
            // ---- shared variables ---------------------------------------
            // Not keyed by the caller, which is the entire difference from state above: a var
            // describes a project or a conversation, and a second panel that could not see the
            // first one's favourites would be a second panel nobody can switch to.
            ApiCall::VarGet { scope, key } => {
                Ok(ApiOk::Json { value: self.vars.get(&scope, &key) })
            }
            ApiCall::VarSet { scope, key, value } => {
                self.vars.set(&scope, key.clone(), value.clone());
                self.bridge.broadcast(PluginEvent::VarChanged {
                    scope,
                    key,
                    value: Some(value),
                });
                Ok(ApiOk::Unit)
            }
            ApiCall::VarDelete { scope, key } => {
                self.vars.delete(&scope, &key);
                self.bridge.broadcast(PluginEvent::VarChanged { scope, key, value: None });
                Ok(ApiOk::Unit)
            }
            ApiCall::VarAll { scope } => Ok(ApiOk::Vars { vars: self.vars.all(&scope) }),
            // ---- the swarm ----------------------------------------------
            ApiCall::SwarmSelf => Ok(ApiOk::SwarmSelf { node: self.swarm_self() }),
            ApiCall::SwarmNodes => Ok(ApiOk::SwarmNodes {
                nodes: self
                    .swarm
                    .peers()
                    .map(|p| neosh_proto::SwarmNode {
                        info: p.info.clone(),
                        capabilities: p.capabilities.clone(),
                        up: p.up,
                        reason: p.reason.clone(),
                        agents: p.agents.clone(),
                    })
                    .collect(),
            }),
            ApiCall::SwarmAgents => Ok(ApiOk::SwarmAgents {
                agents: self
                    .swarm
                    .agents()
                    .map(|(node, agent)| neosh_proto::SwarmAgent {
                        node: node.clone(),
                        agent: agent.clone(),
                    })
                    .collect(),
            }),
            ApiCall::SwarmHostsOf { project } => Ok(ApiOk::Names {
                names: self.swarm.hosts_of(&project).into_iter().map(str::to_string).collect(),
            }),
            ApiCall::SwarmSubscribe { node, session } => {
                match &self.swarm_node {
                    Some(h) => {
                        h.send(neosh_swarm::SwarmRequest::Subscribe { node, session });
                        Ok(ApiOk::Unit)
                    }
                    None => Err(ApiError::NotFound { what: "the swarm is not running".into() }),
                }
            }
            ApiCall::SwarmUnsubscribe { node, session } => {
                if let Some(h) = &self.swarm_node {
                    h.send(neosh_swarm::SwarmRequest::Unsubscribe { node, session });
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::SwarmStrangers => Ok(ApiOk::SwarmStrangers {
                strangers: self.swarm_strangers.values().cloned().collect(),
            }),
            // ---- the plan ------------------------------------------------
            // Answered from what the store kept, which is what makes this safe to call on every
            // redraw. Nothing here asks a vendor anything except `QuotaRefresh`, and that one
            // answers before its own answer arrives.
            ApiCall::QuotaList => Ok(ApiOk::Quotas { quotas: self.quota.list() }),
            ApiCall::QuotaRefresh { instance } => {
                self.refresh_quota(instance);
                Ok(ApiOk::Unit)
            }
            // A plugin publishing for a provider it registered. Its own instances only: a plugin
            // that could report for `claude-cli` could put any number it liked in the strip, and
            // the strip is a thing people are going to trust before spending an afternoon.
            ApiCall::QuotaReport { snapshot } => {
                if !self.plugin_drivers_own(&plugin, &snapshot.instance) {
                    return Err(ApiError::NotPermitted {
                        capability: format!(
                            "quota.report for {} (plugin {} did not register a driver serving it)",
                            snapshot.instance, plugin.0
                        ),
                    });
                }
                self.take_quota(
                    snapshot.instance.clone(),
                    neosh_proto::QuotaSource::Plugin,
                    neosh_provider::quota::Quota {
                        plan: snapshot.plan,
                        windows: snapshot.windows,
                        credits: snapshot.credits,
                    },
                );
                Ok(ApiOk::Unit)
            }
            ApiCall::QuotaHistory { instance, since, until } => Ok(ApiOk::QuotaHistory {
                samples: self.quota.history(instance.as_ref(), since, until),
            }),
            // `UsageHistory` is not here: it reads thousands of files somebody else wrote, so it
            // goes through `spawn_slow` with git and generation, for the same reason they do.
            ApiCall::SwarmPair { node, name, addr } => {
                let Some(h) = &self.swarm_node else {
                    return Err(ApiError::NotFound { what: "the swarm is not running".into() });
                };
                // Whatever it called itself wins over whatever we were told to call it: a machine
                // knows its own name, and a label typed into a dialog is a guess made before
                // anybody had asked.
                let name = self
                    .swarm_strangers
                    .get(&node)
                    .map(|s| s.info.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or(name);
                let addr = addr.or_else(|| {
                    self.swarm_strangers.get(&node).and_then(|s| s.addr.clone())
                });
                h.send(neosh_swarm::SwarmRequest::Pair {
                    id: node.clone(),
                    name: name.clone(),
                    addr,
                });
                self.swarm_strangers.remove(&node);
                self.editor_message(MessageLevel::Info, format!("paired with {name}"));
                self.bridge.broadcast(PluginEvent::SwarmChanged);
                Ok(ApiOk::Unit)
            }
            ApiCall::SwarmUnpair { node } => {
                let Some(h) = &self.swarm_node else {
                    return Err(ApiError::NotFound { what: "the swarm is not running".into() });
                };
                // Refused rather than silently ignored for a machine the config declared: that
                // line would come back on the next reload, and a removal that undoes itself is
                // worse than one that says why it cannot.
                if self
                    .swarm_allowed
                    .get(&node)
                    .is_some_and(|p| p.source == neosh_swarm::Source::Config)
                {
                    return Err(ApiError::Denied {
                        reason: "that machine is in your config file; remove its `[[swarm.peers]]` entry"
                            .into(),
                    });
                }
                h.send(neosh_swarm::SwarmRequest::Unpair { id: node });
                self.bridge.broadcast(PluginEvent::SwarmChanged);
                Ok(ApiOk::Unit)
            }
            // Answered off the loop: it opens a socket to somewhere that may not answer.
            ApiCall::SwarmProbe { .. } => Err(ApiError::Internal {
                message: "swarm.probe is answered asynchronously".into(),
            }),
            // Taken over before dispatch, because its answer comes from another machine.
            ApiCall::SwarmCommand { .. } => {
                Err(ApiError::Internal { message: "swarm.command is answered asynchronously".into() })
            }
            // ---- events -------------------------------------------------
            // `from` is stamped here rather than taken from the call, so "who said this" is one of
            // the few things in a plugin message that cannot be forged.
            ApiCall::EventEmit { name, data } => {
                self.bridge.broadcast(PluginEvent::Event {
                    name,
                    data,
                    from: plugin.0.clone(),
                });
                Ok(ApiOk::Unit)
            }
            // ---- sessions ----------------------------------------------
            ApiCall::SessionList { include_archived } => {
                let list = self.agent.sessions().list_with(include_archived);
                Ok(ApiOk::Sessions {
                    sessions: list.into_iter().map(|s| self.named(s)).collect(),
                })
            }
            ApiCall::SessionCurrent => {
                let info = {
                    let store = self.agent.sessions();
                    let mut i = store.active().info();
                    i.is_active = true;
                    i
                };
                Ok(ApiOk::Session { session: self.named(info) })
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
                let info = self.named(info);
                if activate {
                    // Before the redraw: the banner names the directory, and the file tools have
                    // to be pointed at it before the first turn rather than after it.
                    let here = { self.agent.sessions().active().cwd.clone() };
                    self.work_in(here).await;
                    self.enter_session();
                }
                self.persist_sessions();
                Ok(ApiOk::Session { session: info })
            }
            // Switching is never refused, including while a turn is running. A turn belongs to its
            // conversation: it keeps streaming into that one's transcript, and what you see is
            // rebuilt from the conversation you switched to. Waiting for a model to finish before
            // you may look at something else is the workspace refusing to be a workspace.
            ApiCall::SessionSwitch { session } => {
                self.agent
                    .sessions()
                    .switch(&session)
                    .map_err(|e| ApiError::NotFound { what: e.to_string() })?;
                let cwd = { self.agent.sessions().active().cwd.clone() };
                self.work_in(cwd).await;
                self.enter_session();
                Ok(ApiOk::Unit)
            }
            ApiCall::SessionClose { session } => {
                let closing_active = self.agent.sessions().active_id() == &session;
                // Closing the conversation you are in has to land you somewhere, and the store
                // will not step out of the last one on its own. Same line as the archive path
                // below and for the same reason: being unable to delete the conversation you have
                // finished with, because it is the only one, is not a rule anybody wanted.
                if closing_active {
                    self.ensure_somewhere_to_go(&session);
                }
                // Closing is the one case that does stop a turn: there is about to be nowhere to
                // put its answer, and a stream writing into a conversation that no longer exists is
                // a subprocess nobody can reach.
                // Removed rather than marked: the conversation itself is going, so there is
                // nothing left for its ending to be about and nothing that can start another turn
                // in it. `TurnEnded` arriving later finds no entry, which is the right no-op.
                if let Some(t) = self.turns.remove(&session) {
                    t.cancel.cancel();
                }
                self.rounds.remove(&session);
                self.driver_commands.remove(&session);
                // A vendor CLI held open for this conversation has nothing left to answer. Left
                // alone it would sit there for the rest of the program, holding a conversation
                // whose transcript is about to be deleted from disk.
                for d in &self.agent_drivers {
                    d.close_conversation(&session);
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
                // Whatever anybody tagged this conversation with goes too. Left behind it is a row
                // in `vars.json` per conversation ever deleted, and a future id collision inherits
                // somebody's stale colour.
                self.vars.forget_session(&session);
                if closing_active {
                    // Where you landed is a conversation of its own, and it may be in another
                    // directory: the banner names one, the file tools are pointed at one, and
                    // both were still the deleted conversation's until this said otherwise.
                    let here = { self.agent.sessions().active().cwd.clone() };
                    self.work_in(here).await;
                    self.enter_session();
                }
                self.persist_sessions();
                Ok(ApiOk::Unit)
            }
            // Archiving does not stop a turn. Putting a conversation away is about the list you
            // read, not about the work: it keeps its messages, and it keeps whatever it was in the
            // middle of saying.
            ApiCall::SessionArchive { session, archived } => {
                let leaving = archived && self.agent.sessions().active_id() == &session;
                // Archiving the only conversation you have is a reasonable thing to want, and
                // "there is nowhere to go" is a bad answer to it. Somewhere to go is one line.
                if leaving {
                    self.ensure_somewhere_to_go(&session);
                }
                let at = now_secs();
                self.agent.sessions().archive(&session, archived, at).map_err(|e| match e {
                    neosh_agent::store::StoreError::LastSession => {
                        ApiError::InvalidArgument { message: e.to_string() }
                    }
                    other => ApiError::NotFound { what: other.to_string() },
                })?;
                if leaving {
                    let here = { self.agent.sessions().active().cwd.clone() };
                    self.work_in(here).await;
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
                Ok(ApiOk::PermissionMode { mode: self.agent.permission_mode(&self.active_session()) })
            }
            ApiCall::PermissionSetMode { mode } => {
                // The conversation's, and saved with it. A mode is a property of what you are
                // working on — a scratch question in someone else's checkout and the branch you
                // have been on all week are not the same risk — and one that reset every time you
                // reopened the workspace would be a setting you had to re-make every morning.
                self.set_permission_mode(mode);
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
            ApiCall::ProviderRegisterDriver { driver, instances, agent_loop } => {
                let provider = Arc::new(PluginProvider::new(
                    driver.clone(),
                    plugin.clone(),
                    self.bridge.clone(),
                    agent_loop,
                ));
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
                // An `agent.model` held for this instance is applied *now*, not when the last
                // plugin finishes loading. The two are not the same moment: a plugin that has
                // registered its provider is a plugin whose model can be talked to, and every
                // other plugin still starting up is beside the point. Between them the selection
                // is the fallback — so a turn sent in that gap was answered by a model the user
                // did not choose, and nothing on screen said so.
                self.apply_pending_model_if_servable();
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

    /// The conversation on screen. Cloned rather than borrowed: everything that asks this goes on
    /// to touch the store again, and holding the lock across that is how a deadlock is written.
    fn active_session(&self) -> neosh_proto::SessionId {
        self.agent.sessions().active_id().clone()
    }

    /// Send what is in the composer: the words, and whatever is on the attachment row.
    ///
    /// The row is emptied here rather than by whoever put something on it, because this is the
    /// moment the attachment stops being a draft — steered into a running turn or starting a new
    /// one, either way it has been said and is not yours to take back any more.
    fn start_turn(&mut self, text: String) {
        let here = self.active_session();
        let images = self.take_attachments(&here);
        self.start_turn_in(here, Prompt { text, images });
    }

    /// Take the attachment row for a conversation, as the blocks it becomes.
    fn take_attachments(&mut self, session: &neosh_proto::SessionId) -> Vec<ContentBlock> {
        self.attached.remove(session).unwrap_or_default().iter().map(|a| a.block()).collect()
    }

    /// Where attached images are kept. Beside the conversations they belong to, so clearing the
    /// state directory clears both and neither outlives the other.
    ///
    /// Under `--clean` there is no state directory at all, and a temporary one is the honest
    /// answer: nothing is being kept, which is what `--clean` means, and an image you paste still
    /// has to survive as far as the request.
    fn image_store(&self) -> std::path::PathBuf {
        match &self.state_dir {
            Some(dir) => dir.join("images"),
            None => std::env::temp_dir().join(format!("neosh-images-{}", std::process::id())),
        }
    }

    /// Put an image on the composer's attachment row.
    ///
    /// `path` names a file; `None` is the clipboard. Answers with what it attached, so the caller
    /// can say so — a chip appearing at the bottom of the screen is not enough on its own when the
    /// thing you pressed might equally have found nothing.
    fn attach(&mut self, path: Option<&std::path::Path>) -> Result<crate::images::Attachment, String> {
        let store = self.image_store();
        let got = match path {
            Some(p) => crate::images::from_path(&store, p),
            None => crate::images::from_clipboard(&store),
        }?;
        let here = self.active_session();
        self.attached.entry(here).or_default().push(got.clone());
        self.refresh_composer();
        Ok(got)
    }

    /// Whether the conversation on screen has an image attached that has not been sent.
    fn has_attachment(&self) -> bool {
        self.attached.get(&self.active_session()).is_some_and(|v| !v.is_empty())
    }

    /// Take the last attached image back off, saying which one went.
    ///
    /// Said out loud because the chip it removes may have been the only one on screen: a row that
    /// silently becomes no row is indistinguishable from a key that did nothing.
    fn drop_attachment(&mut self) {
        let here = self.active_session();
        match self.attached.entry(here).or_default().pop() {
            None => self.editor_message(MessageLevel::Info, "nothing attached"),
            Some(a) => {
                self.editor_message(MessageLevel::Info, format!("took off {}", a.label()));
                self.refresh_composer();
            }
        }
    }

    /// Start a turn in a named conversation, drawing it only if it is the one on screen.
    ///
    /// Named rather than implied, because the caller is not always the keyboard: a turn that ends
    /// with something still queued starts the next one, and by then you may be reading something
    /// else entirely.
    fn start_turn_in(&mut self, session: neosh_proto::SessionId, prompt: Prompt) {
        let on_screen = self.active_session() == session;
        // Typing while it works is steering, not an error. The old behaviour — refuse, and make you
        // wait with the sentence already written — is the one moment in the program where you know
        // exactly what you want to say and cannot say it.
        //
        // A turn that is *winding down* after `<Esc>` counts as running for exactly the same
        // reason: its process has not finished with the conversation yet, so this is queued and
        // becomes the next turn through the leftover path in `TurnEnded`. Starting one here
        // instead would put two turns on a driver that keeps one process per conversation.
        if self.turns.contains_key(&session) {
            self.agent.steer(&session, prompt);
            if on_screen {
                // Sending is asking to see what happens, whether it starts a turn or joins one.
                // Without this a message steered in while the transcript was scrolled back — which
                // is where reading it leaves you — landed below the screen, and what you could
                // still see was older text with nothing new in it.
                self.scroll_chat(0);
                self.draw_working();
                self.refresh_composer();
            }
            return;
        }
        let token = CancellationToken::new();
        self.turns
            .insert(session.clone(), Running { cancel: token.clone(), cancelling: false });
        // Not random: the turn id would be ideal but is not known yet, and a clock read is the one
        // varying thing already to hand. Per turn, so it does not change under you mid-answer.
        let verb = VERBS
            .get(now_secs().unsigned_abs() as usize % VERBS.len())
            .copied()
            .unwrap_or("Working");
        self.rounds.insert(session.clone(), Round {
            verb,
            started: std::time::Instant::now(),
            note: None,
            said: Vec::new(),
            changes: Vec::new(),
            plan: Vec::new(),
            tasks: Vec::new(),
        });
        // Stamped here rather than inside the agent so `Session` stays clock-free and testable.
        // The turn id itself is set by the agent a moment later; a list only reads this while the
        // turn id is present, so the ordering is not observable.
        if let Some(s) = self.agent.sessions().get_mut(&session) {
            s.turn_started_at = Some(now_secs());
        }
        if on_screen {
            // Asking a question means you want to see the answer: scrolled-back state does not
            // survive sending, or the reply arrives somewhere off screen.
            self.scroll_chat(0);
            self.chat_question(&prompt);
            self.begin_working();
            // `\u{23ce}` steers from here until the turn is over, and the empty field is where that
            // has to be said — it is the one thing on screen you are looking at when you press it.
            self.refresh_composer();
        }

        let agent = self.agent.clone();
        let bridge = self.bridge.clone();
        tokio::spawn(async move {
            agent.run_turn_in(&*bridge, session, prompt, token).await;
        });
    }

    /// Ask the turn in the conversation on screen to stop, if there is one.
    ///
    /// Answers whether there was a turn to interrupt — which is what tells `<Esc>` it has been
    /// spent, so a second press does not fall through to clearing the draft. The entry stays until
    /// the turn actually ends; see [`Running::cancelling`].
    /// `None` when there was no turn here; `Some(true)` when this press is what stopped it, and
    /// `Some(false)` when it was already winding down — the key is spent either way, but only one
    /// of them is news worth saying out loud.
    fn cancel_turn_here(&mut self) -> Option<bool> {
        let here = self.active_session();
        let t = self.turns.get_mut(&here)?;
        if t.cancelling {
            return Some(false);
        }
        t.cancelling = true;
        t.cancel.cancel();
        Some(true)
    }

    /// Make sure leaving `session` has somewhere to land, minting a placeholder if it does not.
    ///
    /// A conversation you are finished with is a conversation you can close, and that includes the
    /// last one: "the last session cannot be closed" is the store refusing the only thing left to
    /// do in a workspace you have just cleared out. The store always has an active conversation —
    /// there has to be somewhere for the next message to go — so "nothing left" is spelled as a
    /// [placeholder](neosh_agent::Session::ephemeral): the composer types into it and the
    /// transcript shows the welcome, but no list contains it and nothing is written to disk. An
    /// emptied workspace should look empty, and one conversation named after nothing is not empty.
    ///
    /// Asked of [`list`](neosh_agent::store::SessionStore::list), which leaves out what is
    /// archived, because landing in something you deliberately put away is the one destination
    /// that is definitely wrong — a workspace whose every other conversation is archived is as
    /// alone as one with no other conversation at all.
    fn ensure_somewhere_to_go(&mut self, leaving: &neosh_proto::SessionId) {
        let alone = { self.agent.sessions().list().iter().all(|s| &s.id == leaving) };
        if alone {
            // In the directory you are leaving rather than the one neosh was started in: the next
            // thing you type is almost certainly about the project you were just in, and a
            // placeholder somewhere else would silently move the file tools with it.
            let id = self.new_session_in(self.cwd.clone());
            if let Some(s) = self.agent.sessions().get_mut(&id) {
                s.ephemeral = true;
            }
        }
    }

    /// Start an empty conversation without switching to it, and say which one it is.
    ///
    /// Exists so that closing or archiving the only conversation you have has somewhere to land.
    /// It inherits the model and system prompt for the same reason a new conversation does:
    /// changing what you are talking to as a side effect of tidying up would be a strange thing to
    /// do.
    fn new_session_in(&mut self, cwd: std::path::PathBuf) -> neosh_proto::SessionId {
        self.new_session_titled(cwd, None)
    }

    /// The same, with a name given up front.
    ///
    /// A caller that is *making* conversations — a peer over ASCP, an orchestrator plugin fanning
    /// work out — knows what each one is for at the moment it asks for it, and that is the only
    /// moment it is cheap to say. The name was accepted on the wire and dropped on the floor, so
    /// three conversations started by one command came back labelled by whatever their first
    /// message happened to begin with.
    fn new_session_titled(
        &mut self,
        cwd: std::path::PathBuf,
        title: Option<String>,
    ) -> neosh_proto::SessionId {
        let mut session = neosh_agent::Session::new(cwd);
        session.created_at = now_secs();
        session.updated_at = session.created_at;
        session.selection = self.agent.selection();
        session.system = self.agent.session().system.clone();
        // Blank is not a name. An empty string would render as a row with nothing on it, which is
        // worse than the label a first message gives it.
        session.title = title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let id = session.id.clone();
        self.agent.sessions().insert(session);
        id
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
                CoreEffect::ContributionsChanged { point } => {
                    // Broadcast for the same reason: the plugin that *renders* a point is never the
                    // one that just contributed to it, and it is the one that has to redraw.
                    self.bridge.broadcast(PluginEvent::ContributionsChanged { point });
                }
            }
        }
    }

    // ---- the swarm -------------------------------------------------------

    /// Start a node, if the configuration asks for one.
    ///
    /// Off unless somebody said otherwise: a workspace must not open a port because it was
    /// upgraded. Every failure here is a warning rather than an error — a swarm that cannot start
    /// is a feature that is unavailable, not a reason the editor should refuse to run.
    fn start_swarm(&mut self, cfg: &crate::config::SwarmConfig) {
        if cfg.peers.is_empty() && cfg.listen.is_none() {
            return;
        }
        let Some(state) = self.state_dir.clone() else {
            self.editor_message(
                MessageLevel::Warn,
                "the swarm needs somewhere to keep its key, and --clean has nowhere",
            );
            return;
        };

        let path = neosh_swarm::identity::key_path(&state);
        let identity = match neosh_swarm::Identity::load_or_create(&path) {
            Ok(i) => i,
            Err(e) => {
                self.editor_message(MessageLevel::Error, format!("swarm identity: {e}"));
                return;
            }
        };

        // Whatever pairing has remembered, plus whatever the config declared. The two are kept
        // apart by `Source`, so a machine added here can be removed from the UI and one written by
        // hand cannot — that line would simply come back on the next reload.
        let allowed = neosh_swarm::Allowed::load(Some(&state));
        let mut peers = Vec::new();
        for (id, p) in allowed.list() {
            if let Some(addr) = p.addr {
                peers.push(neosh_swarm::PeerAddress { addr, expect: Some(id) });
            }
        }
        for p in &cfg.peers {
            let name = p.name.clone().unwrap_or_else(|| p.addr.clone());
            match &p.id {
                Some(id) => {
                    let id = neosh_proto::NodeId(id.clone());
                    allowed.add(id.clone(), neosh_swarm::AllowedPeer {
                        name,
                        addr: Some(p.addr.clone()),
                        source: neosh_swarm::Source::Config,
                    });
                    peers.push(neosh_swarm::PeerAddress {
                        addr: p.addr.clone(),
                        expect: Some(id),
                    });
                }
                // An address with no key is not an authorisation. Dialled anyway, so the machine
                // there shows up as somebody asking to pair — which is the useful thing to do with
                // a half-written entry, rather than refusing to look at it.
                None => {
                    peers.push(neosh_swarm::PeerAddress { addr: p.addr.clone(), expect: None });
                    self.editor_message(
                        MessageLevel::Info,
                        format!("swarm peer {} has no id yet — ^J to pair with it", p.addr),
                    );
                }
            }
        }
        self.swarm_allowed = allowed.clone();

        let listen = match cfg.listen.as_deref().map(str::parse::<std::net::SocketAddr>) {
            Some(Ok(a)) => Some(a),
            Some(Err(e)) => {
                self.editor_message(
                    MessageLevel::Warn,
                    format!("swarm.listen is not an address: {e}"),
                );
                None
            }
            None => None,
        };

        if let Some(name) = &cfg.name {
            self.swarm_name = name.clone();
        }
        let node_cfg = neosh_swarm::SwarmConfig {
            name: self.swarm_name.clone(),
            listen,
            peers,
            accepts_commands: cfg.accepts_commands,
            accepts_approvals: cfg.accepts_approvals,
            heartbeat: Duration::from_secs(cfg.heartbeat_secs.max(1)),
        };
        let (handle, events) = neosh_swarm::spawn(
            std::sync::Arc::new(identity),
            allowed,
            node_cfg,
            env!("CARGO_PKG_VERSION").to_string(),
        );
        self.editor_message(
            MessageLevel::Info,
            format!("swarm: this node is {}", handle.id().short()),
        );
        self.swarm_node = Some(handle);
        self.swarm_events = Some(events);
    }

    fn swarm_self(&self) -> Option<neosh_proto::NodeInfo> {
        let h = self.swarm_node.as_ref()?;
        Some(neosh_proto::NodeInfo {
            id: h.id().clone(),
            name: self.swarm_name.clone(),
            os: std::env::consts::OS.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }

    /// Send a command to a peer and remember who is waiting for the answer.
    fn begin_swarm_command(
        &mut self,
        node: neosh_proto::NodeId,
        session: neosh_proto::SessionId,
        command: neosh_proto::AgentCommand,
        waiting: (PluginId, RequestId),
    ) {
        // Addressed at this machine, which is a thing to *do* rather than a thing to dial. A
        // fleet plugin iterating over `swarm.nodes()` reaches its own node like any other, and
        // sending ourselves a packet to ask ourselves a question would need the swarm to be
        // running before a plugin could steer the conversation it is sitting in.
        if self.swarm_self().is_some_and(|me| me.id == node) {
            let answer = self
                .apply_agent_command(&session, command)
                .map(|_| ApiOk::Unit)
                .map_err(|r| ApiError::Denied { reason: format!("{r:?}") });
            self.answer_swarm(waiting, answer);
            return;
        }
        let Some(handle) = &self.swarm_node else {
            self.answer_swarm(waiting, Err(ApiError::NotFound {
                what: "the swarm is not running".into(),
            }));
            return;
        };
        // Ids are per host rather than random: a counter is enough to correlate within one
        // connection, and it makes a packet capture readable.
        self.swarm_next_command += 1;
        let id = format!("c{}", self.swarm_next_command);
        self.swarm_pending.insert(id.clone(), waiting);
        handle.send(neosh_swarm::SwarmRequest::Command { node, id, session, command });
    }

    /// Find out what machine is at an address, off the loop.
    ///
    /// Given a short deadline of its own: the address came from somebody typing, and a wrong one is
    /// the ordinary case rather than the exceptional one. A dialog that hangs for the TCP default
    /// on a typo is a dialog people learn to be afraid of.
    fn begin_swarm_probe(&mut self, addr: String, waiting: (PluginId, RequestId)) {
        let Some(info) = self.swarm_self() else {
            self.answer_swarm(waiting, Err(ApiError::NotFound {
                what: "the swarm is not running".into(),
            }));
            return;
        };
        let Some(state) = self.state_dir.clone() else {
            self.answer_swarm(waiting, Err(ApiError::NotFound { what: "no state directory".into() }));
            return;
        };
        let caps = neosh_proto::NodeCapabilities {
            accepts_commands: false,
            accepts_approvals: false,
            streams: false,
            projects: Vec::new(),
        };
        let bridge = self.bridge.clone();
        let (plugin, id) = waiting;
        tokio::spawn(async move {
            let result = async {
                let identity =
                    neosh_swarm::Identity::load_or_create(&neosh_swarm::identity::key_path(&state))
                        .map_err(|e| ApiError::Internal { message: e.to_string() })?;
                let found = tokio::time::timeout(
                    Duration::from_secs(5),
                    neosh_swarm::probe(&addr, &identity, &info, &caps),
                )
                .await
                .map_err(|_| ApiError::NotFound {
                    what: format!("nothing answered at {addr}"),
                })?
                .map_err(|e| ApiError::NotFound { what: format!("{addr}: {e}") })?;
                Ok(ApiOk::SwarmSelf { node: Some(found) })
            }
            .await;
            let response: ApiResponse = result.into();
            let _ = bridge.script().send(ScriptInbound::Plugin {
                plugin,
                msg: PluginInbound::Response { id, response },
            });
        });
    }

    fn answer_swarm(&self, waiting: (PluginId, RequestId), result: Result<ApiOk, ApiError>) {
        let (plugin, id) = waiting;
        let response: ApiResponse = result.into();
        let _ = self.bridge.script().send(ScriptInbound::Plugin {
            plugin,
            msg: PluginInbound::Response { id, response },
        });
    }

    /// Send one event to whoever is watching this conversation.
    ///
    /// The node drops it if nobody is; the set here is the cheaper half of the same question, asked
    /// before the event is built.
    fn stream_out(&self, session: &neosh_proto::SessionId, event: neosh_proto::StreamEvent) {
        if let Some(h) = &self.swarm_node {
            h.send(neosh_swarm::SwarmRequest::Stream { session: session.clone(), event });
        }
    }

    /// Whether anything would come of streaming this conversation.
    fn watched(&self, session: &neosh_proto::SessionId) -> bool {
        self.swarm_node.is_some() && self.swarm_watched.contains(session)
    }

    /// Everything this machine is running, as the wire describes it.
    ///
    /// Recomputed whole rather than tracked incrementally. A workspace has tens of conversations,
    /// not thousands, and a delta scheme whose bookkeeping can drift is a board that is subtly
    /// wrong on the machine you are not sitting at — which is the one case you cannot check.
    fn local_inventory(&mut self) -> Vec<neosh_proto::AgentSummary> {
        let sessions = self.agent.sessions().list_with(false);
        sessions
            .into_iter()
            .map(|s| {
                let info = self.named(s);
                let key = self.project_key(std::path::Path::new(&info.cwd));
                let model = self.model_for(&info.id);
                crate::swarm::summarise(&info, key, model)
            })
            .collect()
    }

    /// What a directory is called across machines. See [`neosh_proto::ProjectKey`].
    ///
    /// Cached with the repository, because it is one `git remote get-url` and the answer does not
    /// change while a checkout is open.
    fn project_key(&mut self, cwd: &std::path::Path) -> neosh_proto::ProjectKey {
        if let Some(key) = self.project_keys.get(cwd) {
            return key.clone();
        }
        // Not cached: a directory whose remote has not been read yet would otherwise keep the
        // guess for ever, and `remember_repo` fills the real answer in a moment later.
        neosh_proto::ProjectKey::from_dir_name(
            &cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        )
    }

    fn model_for(&self, _session: &neosh_proto::SessionId) -> Option<String> {
        self.agent.selection().map(|s| s.model.to_string())
    }

    /// The checkouts this machine can start something in, for a peer's "new conversation over
    /// there" menu.
    ///
    /// Derived from where conversations already are rather than from a scan of the disk: a list of
    /// every git repository on somebody's laptop is both slow to produce and more than a peer needs
    /// to know.
    fn local_projects(&self, agents: &[neosh_proto::AgentSummary]) -> Vec<neosh_proto::RemoteProject> {
        let mut out: Vec<neosh_proto::RemoteProject> = Vec::new();
        for a in agents {
            if out.iter().any(|p| p.cwd == a.cwd) {
                continue;
            }
            out.push(neosh_proto::RemoteProject {
                key: a.project.clone(),
                name: a.project_name.clone(),
                cwd: a.cwd.clone(),
                active: true,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Tell every peer what changed here. Cheap enough to call on any session event.
    pub fn publish_inventory(&mut self) {
        if self.swarm_node.is_none() {
            return;
        }
        let agents = self.local_inventory();
        let mut keys = std::collections::HashMap::new();
        for a in &agents {
            keys.insert(a.project.clone(), a.project_name.clone());
        }
        self.swarm.set_local_projects(keys);
        let projects = self.local_projects(&agents);
        if let Some(h) = &self.swarm_node {
            h.send(neosh_swarm::SwarmRequest::Publish { full: true, agents, gone: Vec::new() });
            h.send(neosh_swarm::SwarmRequest::Projects { projects });
        }
    }

    /// One event from the swarm.
    pub fn on_swarm_event(&mut self, event: neosh_swarm::SwarmEvent) {
        use neosh_swarm::SwarmEvent as E;
        match event {
            E::PeerUp { node, capabilities } => {
                let first = self.swarm.peer(&node.id).is_none_or(|p| !p.up);
                self.swarm.peer_up(node.clone(), capabilities);
                if first {
                    self.editor_message(
                        MessageLevel::Info,
                        format!("{} joined the swarm", node.name),
                    );
                    // It has just connected and knows nothing about us until we say so.
                    self.publish_inventory();
                }
                self.bridge.broadcast(PluginEvent::SwarmChanged);
            }
            E::PeerDown { node, reason } => {
                let name = self
                    .swarm
                    .peer(&node)
                    .map(|p| p.info.name.clone())
                    .unwrap_or_else(|| node.short().to_string());
                self.swarm.peer_down(&node, reason);
                self.editor_message(MessageLevel::Warn, format!("lost {name}"));
                // Anything still waiting on that machine will never be answered by it.
                let orphaned: Vec<String> = self.swarm_pending.keys().cloned().collect();
                for id in orphaned {
                    if let Some(waiting) = self.swarm_pending.remove(&id) {
                        self.answer_swarm(waiting, Err(ApiError::NotFound {
                            what: format!("{name} went away before answering"),
                        }));
                    }
                }
                self.bridge.broadcast(PluginEvent::SwarmChanged);
            }
            E::Inventory { node, full, agents, gone } => {
                self.swarm.inventory(&node, full, agents, gone);
                self.bridge.broadcast(PluginEvent::SwarmChanged);
            }
            E::Stream { node, session, event } => {
                self.bridge.broadcast(PluginEvent::SwarmStream { node, session, event });
            }
            E::Stranger { node, dialled } => {
                let first = !self.swarm_strangers.contains_key(&node.id);
                // The address only means anything when we were the one dialling. For a machine
                // that dialled us, where it happened to arrive from is not somewhere we could
                // usefully dial back.
                let addr = dialled
                    .then(|| self.swarm_allowed.get(&node.id).and_then(|p| p.addr))
                    .flatten();
                let name = node.name.clone();
                self.swarm_strangers.insert(
                    node.id.clone(),
                    neosh_proto::SwarmStranger { info: node, dialled, addr },
                );
                // Said once. A machine that is dialling us every ten seconds must not produce a
                // notification every ten seconds.
                if first && !dialled {
                    self.editor_message(
                        MessageLevel::Info,
                        format!("{name} wants to join this workspace — ^J to allow it"),
                    );
                }
                self.bridge.broadcast(PluginEvent::SwarmChanged);
            }
            E::Answer { id, result, .. } => {
                let (ok, message) = match result {
                    Ok(_) => (true, String::new()),
                    Err(r) => (false, refusal_text(&r)),
                };
                self.on_swarm_reply(&id, ok, message);
            }
            E::Subscribed { session, .. } => {
                let fresh = self.swarm_watched.insert(session.clone());
                // Everything said so far, once. A watcher that only received deltas would show an
                // empty conversation until the next token — which for an idle agent is never.
                if fresh {
                    let messages = self
                        .agent
                        .sessions()
                        .get(&session)
                        .map(|s| s.messages.clone())
                        .unwrap_or_default();
                    self.stream_out(&session, neosh_proto::StreamEvent::History { messages });
                }
            }
            E::Unsubscribed { session, .. } => {
                // Not removed from the set: another peer may still be watching, and the node knows
                // who wants what. Cleared when nobody does, which the node cannot tell us without a
                // count — so this errs towards sending events nobody reads rather than towards a
                // conversation that silently stops streaming.
                let _ = session;
            }
            E::Command { node, id, session, command } => {
                self.run_swarm_command(node, id, session, command);
            }
        }
    }

    /// A peer asked us to do something. Answer exactly once, whatever happens.
    fn run_swarm_command(
        &mut self,
        node: neosh_proto::NodeId,
        id: String,
        session: neosh_proto::SessionId,
        command: neosh_proto::AgentCommand,
    ) {
        let result = self.apply_agent_command(&session, command);
        if let Some(h) = &self.swarm_node {
            h.send(neosh_swarm::SwarmRequest::Answer { node, id, result });
        }
    }

    /// Do one [`neosh_proto::AgentCommand`] to a conversation on this machine.
    ///
    /// The single implementation behind both ways of asking: a peer over ASCP, and a plugin here
    /// through [`ApiCall::AgentCommand`]. It was written for the first and reachable only from it,
    /// which is how the swarm API came to be able to do things to a conversation that the local
    /// API could not — see the docs on `ApiCall::AgentCommand`. One body means the two cannot
    /// drift, and that a refusal is refused for the same reason on both paths.
    ///
    /// Answers with the conversation it acted on when that is news, which is only
    /// [`neosh_proto::AgentCommand::NewSession`].
    fn apply_agent_command(
        &mut self,
        session: &neosh_proto::SessionId,
        command: neosh_proto::AgentCommand,
    ) -> Result<Option<neosh_proto::SessionId>, neosh_proto::Refusal> {
        use neosh_proto::AgentCommand as C;
        let session = session.clone();
        let known = self.agent.sessions().list_with(true).iter().any(|s| s.id == session);
        match command {
            C::NewSession { cwd, title } => {
                let dir = cwd
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| self.cwd.clone());
                if dir.is_dir() {
                    // The one just made, not whatever is on screen here: `new_session_titled`
                    // inserts without switching, so the active id is the conversation this machine
                    // happens to be looking at and the caller would be handed somebody else's.
                    let id = self.new_session_titled(dir, title);
                    self.publish_inventory();
                    Ok(Some(id))
                } else {
                    Err(neosh_proto::Refusal::Failed {
                        message: format!("{} is not a directory here", dir.display()),
                    })
                }
            }
            _ if !known => Err(neosh_proto::Refusal::NoSuchAgent),
            // Words only. A peer on another machine naming a path would be naming a file on
            // *its* disk, and this side would read whatever happened to be at that path here.
            C::Send { text } => {
                self.start_turn_in(session.clone(), Prompt::text(text));
                Ok(None)
            }
            C::Interrupt => {
                // The same idempotent ask `<Esc>` makes, and for the same reason: the entry stays
                // until the turn ends, so two peers interrupting once each is one interruption.
                if let Some(t) = self.turns.get_mut(&session)
                    && !t.cancelling
                {
                    t.cancelling = true;
                    t.cancel.cancel();
                }
                Ok(None)
            }
            C::Rename { title } => {
                let _ = self.agent.sessions().rename(&session, title);
                self.publish_inventory();
                Ok(None)
            }
            C::Archive { archived } => {
                let at = now_secs();
                let _ = self.agent.sessions().archive(&session, archived, at);
                self.publish_inventory();
                Ok(None)
            }
            C::SetModel { instance, model } => {
                let selection = neosh_proto::ModelSelection {
                    instance: neosh_proto::InstanceId(instance),
                    model: neosh_proto::ModelId(model),
                    // Not carried across. Reasoning effort and the rest are per-model settings the
                    // far end chose against *its* catalogue, and applying them to a model this
                    // machine resolved separately is how a remote switch quietly changes something
                    // nobody asked it to.
                    options: Default::default(),
                };
                // Checked here rather than taken on trust: the far end is naming a model from *its*
                // catalogue, and this machine may not have that provider configured at all. A
                // selection nothing can resolve would leave the conversation unable to take a turn,
                // with nothing on screen to say why.
                if self.agent.providers().resolve(&selection).is_none() {
                    Err(neosh_proto::Refusal::Failed {
                        message: format!(
                            "{}/{} is not a model this machine has",
                            selection.instance, selection.model
                        ),
                    })
                } else {
                    if let Some(s) = self.agent.sessions().get_mut(&session) {
                        s.selection = Some(selection.clone());
                    }
                    // A running turn is told, the same as it would be for `^P` here: the agent
                    // thinks the rest of it with the new model rather than finishing with the old.
                    for d in &self.agent_drivers {
                        d.set_model(&session, &selection);
                    }
                    self.publish_inventory();
                    Ok(None)
                }
            }
            // Declared in the protocol and refused by name, because on this host the prompt does
            // not belong to the host. Approvals are a *blocking plugin hook* — the approvals plugin
            // owns the question and the answer, and the host has nothing to say yes with. Answering
            // this properly means a plugin registering as the swarm's approver, which is a surface
            // that does not exist yet. A silent success would be a remote machine believing it had
            // authorised a write to this filesystem when it had not, so it says no instead.
            C::Approve { .. } => Err(neosh_proto::Refusal::NotPermitted {
                what: "approvals are answered by a plugin on this machine, not over the swarm"
                    .into(),
            }),
        }
    }

    /// Settle a command we sent, when the far end answers.
    pub fn on_swarm_reply(&mut self, id: &str, ok: bool, message: String) {
        let Some(waiting) = self.swarm_pending.remove(id) else { return };
        let result = if ok {
            Ok(ApiOk::Unit)
        } else {
            Err(ApiError::Denied { reason: message })
        };
        self.answer_swarm(waiting, result);
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
                self.select_model(selection);
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
            "worktree.root" => {
                // Expanded here, once, so nothing downstream has to know what `~` means. A path
                // half the readers expand is a path that works until the day it does not.
                let raw = value.as_str().unwrap_or_default();
                if let Some(full) = expand_tilde(raw)
                    && full != raw
                {
                    self.set_option_or_defer(
                        "worktree.root".into(),
                        OptionValue::Str(full),
                    );
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

    /// How wide the status strip is, or zero if the frontend has not said yet.
    fn status_width(&self) -> usize {
        self.editor
            .windows()
            .find(|w| w.buf == self.status)
            .and_then(|w| w.viewport.map(|v| v.width as usize))
            .unwrap_or(0)
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
            // And which *kind* of reading. A selection is the one state here where the same key
            // does a different thing — `y` copies rows rather than characters, `esc` gives the
            // selection up rather than the mode — and Vim says so in the same place for the same
            // reason. `reading` when there is none, because that is the mode, not the absence of
            // a selection.
            match (self.chat_anchored(), self.reading_shape) {
                (true, SelectShape::Line) => "visual line",
                (true, _) => "visual",
                _ => "reading",
            }
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

        // Drop what does not fit, least important first.
        //
        // Both sides at once, because the two halves are competing for one line and the loser is
        // whichever segment is worth least, not whichever side it happens to be on. Without this,
        // a wide left half and a right half that no longer fits means the right half is written
        // straight over the end of the left one — two segments in the same columns, which reads
        // as a corrupted line rather than as a full one.
        //
        // Nothing is truncated. Half a token count is a wrong token count, and a bar cut short is
        // a bar reading a level nothing is at.
        {
            let width = self.status_width();
            // The mode word, the space each side of the line, and the single column that keeps the
            // two halves from touching.
            let mut used = display_width(mode) + 3;
            let cost = |seg: &neosh_proto::StatusSegment| {
                2 + display_width(&seg.text)
                    + seg
                        .keys
                        .as_deref()
                        .filter(|k| !k.is_empty())
                        .map_or(0, |k| 1 + display_width(k))
            };
            let mut order: Vec<(i32, String)> =
                left.iter().chain(right.iter()).map(|(p, k, _)| (*p, k.clone())).collect();
            // Least important last in the strip, so least important first out of it.
            order.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
            let mut dropped: std::collections::HashSet<String> = Default::default();
            for (_, key) in &order {
                let seg = left
                    .iter()
                    .chain(right.iter())
                    .find(|(_, k, _)| k == key)
                    .map(|(_, _, s)| s)
                    .expect("came from those lists");
                used += cost(seg);
            }
            for (_, key) in order.iter() {
                if width == 0 || used <= width {
                    break;
                }
                let seg = left
                    .iter()
                    .chain(right.iter())
                    .find(|(_, k, _)| k == key)
                    .map(|(_, _, s)| s)
                    .expect("came from those lists");
                used -= cost(seg);
                dropped.insert(key.clone());
            }
            left.retain(|(_, k, _)| !dropped.contains(k));
            right.retain(|(_, k, _)| !dropped.contains(k));
        }

        // Marks are built alongside the text so a segment's colour cannot drift from its columns.
        let mut text = String::from(" ");
        let mut marks: Vec<(usize, usize, String)> = Vec::new();
        let push = |text: &mut String, marks: &mut Vec<(usize, usize, String)>, s: &str, hl: &str| {
            let from = text.len();
            text.push_str(s);
            marks.push((from, text.len(), hl.to_string()));
        };

        let segment =
            |text: &mut String, marks: &mut Vec<(usize, usize, String)>, seg: &neosh_proto::StatusSegment| {
                text.push_str("  ");
                push(text, marks, &seg.text, seg.hl.as_deref().unwrap_or("Status.Line"));
                // The key that changes it, right after it. A key is only memorable once it has
                // been seen next to the thing it does; a legend somewhere else is a second place
                // to look.
                if let Some(keys) = seg.keys.as_deref().filter(|k| !k.is_empty()) {
                    text.push(' ');
                    push(text, marks, keys, "Composer.HintKey");
                }
            };

        push(&mut text, &mut marks, mode, "Status.Line");
        for (_, _, seg) in left.iter() {
            segment(&mut text, &mut marks, seg);
        }
        if self.status_segments.is_empty() {
            // Before any plugin has contributed — and under `--clean`, where none will.
            push(&mut text, &mut marks, "  ^C quit", "Status.Line");
        }

        // Right-aligned segments actually go to the right edge. They used to be appended after the
        // left ones and were therefore right-aligned in name only — which is the sort of thing that
        // looks fine until a second plugin uses it and both end up in the middle.
        if !right.is_empty() {
            let mut tail = String::new();
            let mut tail_marks: Vec<(usize, usize, String)> = Vec::new();
            for (_, _, seg) in right.iter() {
                segment(&mut tail, &mut tail_marks, seg);
            }
            let width = self.status_width();
            let used = display_width(&text) + display_width(&tail) + 1;
            if width > used {
                text.push_str(&" ".repeat(width - used));
            }
            let base = text.len();
            text.push_str(&tail);
            marks.extend(tail_marks.into_iter().map(|(a, b, hl)| (base + a, base + b, hl)));
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
                    line_hl_group: None,
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
        let ascii = self.option_bool("ui.ascii_only");
        let text = self.composer_text();
        // Every path that changes the draft comes through here, which is what makes this the one
        // place to say so. Compared rather than fired blind: this also runs on a resize and on a
        // redraw, and a completion menu that reopened every time the window changed width would be
        // unusable.
        //
        // And not while the field is lent to something else. A search and a key prompt both type
        // into this buffer, and what is in it then is *not* the draft — the `/` that starts a
        // search is a search, and a completion menu that opened on top of it would take the
        // keyboard away from the thing the user had just started.
        let borrowed = self.secret.is_some() || self.search.is_some();
        if self.draft != text && !borrowed {
            self.draft.clone_from(&text);
            self.bridge.broadcast(PluginEvent::ComposerChanged { text: text.clone() });
        }
        let lines = self.editor.buffer(self.composer).map(|b| b.line_count()).unwrap_or(1).max(1);
        let width = self
            .editor
            .window(self.composer_win)
            .and_then(|w| w.viewport.map(|v| v.width as usize))
            .unwrap_or(80);

        // A blank row, then the rule. Both drawn above the first line, so they move with the field
        // rather than being rows the transcript has to remember not to write on.
        //
        // The blank is not decoration. Without it the last line of an answer sits directly on the
        // rule, and the eye reads the two as one block — the end of what was said and the edge of
        // where you say things run together, which is the single worst place in this layout to
        // save a row.
        self.composer_mark(0, VirtTextPos::Above, vec![chunk("", "Composer.Rule")]);
        let rule = if ascii { "-" } else { "\u{2500}" };
        self.composer_mark(0, VirtTextPos::Above, vec![chunk(rule.repeat(width), "Composer.Rule")]);

        // What you typed while it was working, waiting for a gap to be said in — between the rule
        // and the field, which is where it belongs and not where it was.
        //
        // Under the field it sat below the shortcut row, in the strip the eye has learned to read
        // as chrome, one row above the status line: a sentence of yours, filed with the furniture.
        // Above the field it is the last thing between the transcript and what you are typing,
        // which is exactly what it is — the queue is *unsent conversation*, and it reads in order
        // with everything else that has been said.
        //
        // Drawn after the rule so it lands under it: marks at the same position stack in the order
        // they were set.
        // What has been attached but not yet sent, between the rule and the queue. Above the
        // field for the reason the queue is: it is part of the message you are writing, not
        // chrome — and it is the one part of that message you cannot read off the field itself.
        let attached: Vec<String> = self
            .attached
            .get(&self.active_session())
            .map(|v| v.iter().map(|a| a.label()).collect())
            .unwrap_or_default();
        for (i, label) in attached.iter().enumerate() {
            let mut v = vec![
                chunk("  ", "Composer.Hint"),
                // Labelled once, like the queue: a column of the same word is not the part you
                // are reading.
                chunk(if i == 0 { "attached " } else { "         " }, "Status.Pending"),
                chunk(label.clone(), "Composer.Hint"),
            ];
            if i + 1 == attached.len() {
                v.push(chunk("  ", "Composer.Hint"));
                v.push(chunk(if ascii { "Bksp" } else { "\u{232b}" }, "Composer.HintKey"));
                v.push(chunk(" take off", "Composer.Hint"));
            }
            self.composer_mark(0, VirtTextPos::Above, v);
        }

        let queued = self.agent.steering_queue(&self.active_session());
        let take = queued.len().min(3);
        for (i, text) in queued.iter().map(|p| &p.text).take(take).enumerate() {
            let more = queued.len() - take;
            let mut v = vec![
                chunk("  ", "Composer.Hint"),
                // Only the first row is labelled. Three rows each saying "queued" is a column of
                // the same word, and the word is not the part you are checking.
                chunk(if i == 0 { "queued " } else { "       " }, "Status.Pending"),
            ];
            let one = text.lines().next().unwrap_or("").trim();
            let tail = if i + 1 == take && more > 0 { format!("  (+{more} more)") } else { String::new() };
            let key = "^Y";
            // *What happens*, not what you do. `edit` is the operation and "I want that sentence
            // back" is the thought, and the two only look the same to somebody who already knew
            // the key was there.
            let undo = " take back";
            // The label, the gap either side of it, and the words after it, so a long message is
            // clipped rather than pushing the key that undoes it off the edge.
            let room = width
                .saturating_sub(9 + tail.chars().count() + display_width(key) + 2 + undo.len());
            let body: String = one.chars().take(room).collect();
            v.push(chunk(format!("{body}{tail}"), "Composer.Hint"));
            // On the last row, so it is next to the message it would take back rather than
            // attached to the oldest one.
            if i + 1 == take {
                v.push(chunk("  ", "Composer.Hint"));
                v.push(chunk(key, "Composer.HintKey"));
                v.push(chunk(undo, "Composer.Hint"));
            }
            self.composer_mark(0, VirtTextPos::Above, v);
        }

        // The prompt, spliced in at column zero. The caret is pushed past it by the frontend, which
        // is the only place that knows how wide it drew.
        let caret = if ascii { "> " } else { "\u{203a} " };

        // While a key or a search is being typed the composer is borrowed and says so itself, so
        // it gets the rule above it and nothing else: a prompt glyph in front of `/Haiku` would
        // read as part of the search, and shortcuts that no longer work, advertised under a field
        // where every keystroke is captured, are exactly the wrong thing to be reading.
        //
        // The rule stays because it is the layout. Dropping it moves every row of the transcript
        // by one the moment you press `/`, which is the moment you least want the screen to jump.
        if self.secret.is_some() || self.search.is_some() {
            return;
        }

        self.composer_mark(0, VirtTextPos::Inline, vec![chunk(caret, "Composer.Prompt")]);

        if text.is_empty() {
            // While a turn is running, `\u{23ce}` does something else, and an empty field is the one
            // place you are looking when you are about to press it. "Ask anything" there invites
            // you to do the thing you cannot do and says nothing about the thing you can — which
            // is how somebody presses it, watches the sentence vanish into a queue they did not
            // know existed, and has no idea what just happened to it.
            let words = if self.turns.contains_key(&self.active_session()) {
                "Steer it \u{2014} taken in at the next gap"
            } else {
                "Ask anything, or run a command"
            };
            self.composer_mark(
                0,
                VirtTextPos::Eol,
                vec![chunk(words, "Composer.Placeholder")],
            );
        }

        let row = lines.saturating_sub(1);
        let indent = caret.chars().count();
        let pad = " ".repeat(indent);

        // Reading is the one place a shortcut row earns its line, whatever `ui.hints` says. The
        // keys mean something different here, they are not written down anywhere else, and the
        // composer is not being typed into — so the row is free and the question it answers is
        // live.
        if self.reading {
            let mut v = vec![chunk(pad, "Composer.Hint")];
            v.extend(self.reading_hints(width.saturating_sub(indent)));
            self.composer_mark(row, VirtTextPos::Below, v);
            return;
        }

        // The shortcut row goes under the last line you have written — and steps aside once you are
        // writing enough that it would cost you a line to look at.
        if lines <= 2
            && let Some(chunks) = self.hint_row(width.saturating_sub(indent))
        {
            let mut v = vec![chunk(pad, "Composer.Hint")];
            v.extend(chunks);
            self.composer_mark(row, VirtTextPos::Below, v);
        }
    }

    /// What the keys do while reading, as many as fit.
    ///
    /// A pending operator takes the row over entirely: once `y` is down, the only useful thing to
    /// know is what the second key can be, and leaving the general list up would be answering a
    /// question nobody is asking any more.
    fn reading_hints(&self, width: usize) -> Vec<neosh_proto::VirtChunk> {
        // The card under the cursor, if it is one that has something to open.
        let card = self
            .card_at(self.chat_cursor().0)
            .map(|i| &self.cards[i])
            .filter(|c| c.settled())
            .map(|c| c.expanded);
        let tab = self.glyphs().tab;
        let mut pairs: Vec<(&str, &str)> = match self.reading_pending {
            Some(Pending::Yank) => vec![
                ("y", "line"),
                ("c", "code block"),
                ("m", "this turn"),
                ("a", "everything"),
                ("i", "inside"),
                ("w $ j", "motion"),
            ],
            Some(Pending::G) => vec![("g", "top"), ("e", "word end back"), ("v", "reselect")],
            Some(Pending::Z) => {
                vec![("z", "centre"), ("t", "top"), ("b", "bottom"), ("a", "card")]
            }
            // Waiting for a literal character, so listing keys would be a lie about what the next
            // press does. The word is the whole message.
            Some(Pending::Find { .. }) => vec![("", "a character")],
            Some(Pending::Object { .. }) => {
                vec![("w", "word"), ("p", "paragraph"), ("\" ' `", "quotes"), ("( [ {", "brackets")]
            }
            None if self.chat_anchored() => vec![
                ("y", "copy"),
                ("hjkl", "extend"),
                match self.reading_shape {
                    SelectShape::Line => ("v", "characters"),
                    _ => ("V", "whole lines"),
                },
                ("o", "other end"),
                ("iw", "word"),
                ("esc", "drop"),
            ],
            None => vec![
                ("y", "copy"),
                ("v", "select"),
                ("[ ]", "turn"),
                ("{ }", "block"),
                ("/", "find"),
                ("i", "write"),
                ("esc", "leave"),
            ],
        };
        // First, because it is about the row you are on and the others are about everywhere.
        if self.reading_pending.is_none()
            && !self.chat_anchored()
            && let Some(open) = card
        {
            pairs.insert(0, (tab, if open { "fold" } else { "expand" }));
        }
        let mut out: Vec<neosh_proto::VirtChunk> = Vec::new();
        let mut used = 0usize;
        if self.reading_pending.is_some() {
            let head = "waiting…  ";
            used += display_width(head);
            out.push(chunk(head, "Status.Pending"));
        }
        // A count that has been typed and not yet spent. It goes first because it is about the key
        // you are in the middle of pressing, and without it the digit is a press with no effect —
        // which is indistinguishable from a key that does nothing.
        if !self.reading_count.is_empty() {
            let head = format!("{}  ", self.reading_count);
            used += display_width(&head);
            out.push(chunk(head, "Accent"));
        }
        for (keys, label) in pairs {
            let gap = if out.len() > usize::from(self.reading_pending.is_some()) { 2 } else { 0 };
            let cost = gap + display_width(keys) + 1 + display_width(label);
            if used + cost > width {
                break;
            }
            if gap > 0 {
                out.push(chunk("  ", "Composer.Hint"));
            }
            out.push(chunk(keys.to_string(), "Composer.HintKey"));
            out.push(chunk(format!(" {label}"), "Composer.Hint"));
            used += cost;
        }
        out
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
                line_hl_group: None,
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
        if !self.editor.options().bool("ui.hints").unwrap_or(false) {
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

        // Whether this is a workspace or an empty one. A placeholder is what you land in after
        // closing the last conversation — or after reopening a workspace whose every conversation
        // was archived — and the transcript is the only thing on screen with room to say what to
        // press next, because the panel beside it is, correctly, blank.
        let empty = !self.agent.sessions().any_open();
        let ascii = self.option_bool("ui.ascii_only");
        let width = self.chat_width();
        let mut rows: Vec<String> = vec![String::new()];
        let mut marks: Vec<Mark> = Vec::new();
        let mut say = |rows: &mut Vec<String>, text: String, spans: Vec<(usize, usize, &'static str)>| {
            let at = rows.len() as u32;
            marks.extend(spans.into_iter().map(|(a, b, g)| Mark::at(at, a, b, g)));
            rows.push(text);
        };
        // The word, where there is room for it and a terminal that can draw it. Narrower than
        // the word and it wraps into noise; in ASCII, half its glyphs would be boxes.
        if !ascii && width >= crate::logo::WIDTH + 2 {
            for i in 0..crate::logo::ROWS.len() {
                let (text, spans) = crate::logo::row(i, 2);
                say(&mut rows, text, spans);
            }
            let tag = "  a terminal-first agent workspace".to_string();
            let len = tag.len();
            say(&mut rows, tag, vec![(0, len, "Comment")]);
        } else {
            let tag = "  neosh \u{2014} a terminal-first agent workspace".to_string();
            let len = tag.len();
            say(&mut rows, tag, vec![(2, 2 + "neosh".len(), "Accent"), (2 + "neosh".len(), len, "Comment")]);
        }
        rows.push(String::new());
        say(&mut rows, format!("  model      {model}"), vec![(0, 13, "Comment")]);
        say(&mut rows, format!("  directory  {}", tilde(&self.cwd)), vec![(0, 13, "Comment")]);
        rows.push(String::new());
        // Said only when it is true: there is no conversation anywhere, so nothing else on screen
        // is going to tell you that typing is how one starts.
        if empty {
            // Short enough to be one row. Nothing here wraps the welcome, so a sentence longer than
            // the chat is a second line starting at column zero, under an indented one — and the
            // row of keys below already says how to add a project, which is the rest of it.
            let line = "  Nothing open. What you type here starts a conversation.".to_string();
            let len = line.len();
            say(&mut rows, line, vec![(0, len, "Comment")]);
            rows.push(String::new());
        }
        // Only when there is something to say. On a working setup the welcome should be the word,
        // a few short lines, and then out of the way.
        if selection.as_ref().is_some_and(|s| !self.selection_is_usable(s)) {
            let warn = "  This model cannot authenticate yet. Press ^P to pick another, or add a \
                        key there."
                .to_string();
            let len = warn.len();
            say(&mut rows, warn, vec![(0, len, "Diagnostic.Warn")]);
            rows.push(String::new());
        }
        // The keys worth knowing before the first question — and the one most people never find,
        // which is that the transcript is a place you can go. The rest of the table is on `^Z`.
        let keys: [(&str, &str); 5] = if empty {
            // A different five, because four of the everyday ones are about a conversation and
            // there is not one. What is worth knowing here is how to get one.
            [
                (if ascii { "Enter" } else { "\u{23ce}" }, "start"),
                ("^O", "add project"),
                ("^T", "projects"),
                ("^F", "archived"),
                ("^Z", "every key"),
            ]
        } else {
            [
                (if ascii { "Enter" } else { "\u{23ce}" }, "send"),
                ("^S", "browse & copy"),
                ("^P", "model"),
                ("^T", "projects"),
                ("^Z", "every key"),
            ]
        };
        let mut line = String::from("  ");
        let mut spans = Vec::new();
        for (i, (key, label)) in keys.iter().enumerate() {
            let sep = if i > 0 { "   " } else { "" };
            // As many as fit, in the order they are worth knowing. A row of hints that wraps is
            // two rows of hints, the second of which starts mid-phrase — and the whole point of
            // this line is to be readable at a glance. `^S` is second because it is the one most
            // people never find; the ones that get dropped first are the ones `^Z` also names.
            let would = sep.len() + key.chars().count() + 1 + label.chars().count();
            if i > 0 && line.chars().count() + would > width {
                break;
            }
            line.push_str(sep);
            let from = line.len();
            line.push_str(key);
            spans.push((from, line.len(), "Key"));
            let from = line.len();
            line.push(' ');
            line.push_str(label);
            spans.push((from, line.len(), "Comment"));
        }
        say(&mut rows, line, spans);
        rows.push(String::new());

        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.chat,
            start: Some(0),
            end: Some(replacing.max(0) as u32),
        });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.chat,
            start: 0,
            end: replacing,
            lines: rows.clone(),
        });
        self.welcome_rows = rows.len();
        self.draw_marks(&marks, 0);
    }

    /// Push the permission mode out to every driver that runs its own agent loop.
    ///
    /// Called from everywhere the mode can change, which now includes a config reload. It used to
    /// be only the API call, so `permissions.mode` in `config.toml` reached the built-in tools and
    /// no further, and the drivers stayed on whatever they were told at startup.
    fn sync_agent_drivers(&self) {
        let mode = self.agent.permission_mode(&self.active_session());
        for d in &self.agent_drivers {
            d.set_permission_mode(mode);
        }
    }

    /// Set the mode for the conversation on screen, and tell everything that has to be told.
    ///
    /// The conversation holds it; nothing else does. What the agent drivers hold is a *copy* — they
    /// police their own tool calls inside their own process, and there is no way to reach the
    /// permission layer from in there — so they are told, every time, from here. The footer is told
    /// for the ordinary reason: it is how anybody knows.
    fn set_permission_mode(&mut self, mode: neosh_proto::PermissionMode) {
        let here = self.active_session();
        self.agent.set_permission_mode(&here, mode);
        self.sync_agent_drivers();
        self.refresh_status();
        self.persist_sessions();
    }

    /// Serve a terminal rather than be one: `quit` sends the viewer away, and what this workspace
    /// is holding is reported to `live` for `neosh status` to read.
    pub fn serve(&mut self, live: std::sync::Arc<crate::daemon::Live>) {
        self.on_quit = OnQuit::Detach;
        self.live = Some(live);
    }

    /// Tell whoever is asking what is running here.
    ///
    /// A status query has to be answerable *while* the host is busy, because that is exactly when
    /// somebody asks — so it is a snapshot the accept loop can read rather than a question put to
    /// the run loop, and this is what keeps the snapshot true.
    ///
    /// Called from every path that could have changed the answer, and rate-limited to once a
    /// second rather than reasoned about: a streaming turn flushes a frame every few milliseconds
    /// and each report locks the session store and formats a line per running turn. A second is
    /// finer than any of these numbers are quoted at.
    fn report_live(&mut self) {
        if self.live.is_none() {
            return;
        }
        let fresh = self.live_at.is_some_and(|at| at.elapsed() < Duration::from_secs(1));
        if fresh && self.rounds.len() == self.live_turns {
            return;
        }
        self.live_at = Some(std::time::Instant::now());
        self.live_turns = self.rounds.len();
        self.report_live_now();
    }

    /// The report itself, whatever was asked a moment ago.
    ///
    /// Separate so the paths where the answer has certainly changed — a viewer arriving or
    /// leaving — do not silently do nothing because a frame went past half a second earlier.
    fn report_live_now(&self) {
        let Some(live) = &self.live else { return };
        let store = self.agent.sessions();
        let running: Vec<neosh_proto::RunningTurn> = self
            .rounds
            .iter()
            .filter_map(|(id, round)| {
                let s = store.get(id)?;
                Some(neosh_proto::RunningTurn {
                    session: id.clone(),
                    label: s.label(),
                    cwd: s.cwd.display().to_string(),
                    elapsed_secs: round.started.elapsed().as_secs(),
                })
            })
            .collect();
        let conversations = store.iter().filter(|s| !s.archived).count();
        drop(store);
        live.report(conversations, running);
    }

    /// Take ownership of the agent drivers, so the mode can be mirrored into them later and a
    /// request one of them cannot answer alone can reach a person.
    pub fn adopt_agent_drivers(
        &mut self,
        drivers: Vec<Arc<dyn neosh_provider::drivers::AgentDriver>>,
    ) {
        let asker: Arc<dyn neosh_provider::approval::PermissionAsker> =
            Arc::new(neosh_agent::DriverAsker::new(&self.agent, self.bridge.clone()));
        // The other half of "a request one of them cannot answer alone": not whether something may
        // happen, but which of several things you want. Policy has no answer to that one, so it
        // goes somewhere else entirely — see [`neosh_agent::DriverQuestioner`].
        let questioner: Arc<dyn neosh_provider::ask::QuestionAsker> =
            Arc::new(neosh_agent::DriverQuestioner::new(&self.agent, self.bridge.clone()));
        for d in &drivers {
            d.set_permission_asker(asker.clone());
            d.set_question_asker(questioner.clone());
        }
        self.agent_drivers = drivers;
        self.sync_agent_drivers();
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
        self.select_model(next);
        self.editor_message(MessageLevel::Info, note);
        self.refresh_status();
    }

    /// Set the model this conversation will use, and tell everyone.
    ///
    /// Every path that changes a selection goes through here. There are five of them — a plugin
    /// asking, `agent.model` resolving, a stored model that cannot authenticate being moved off, a
    /// provider registering late, and the first usable one at startup — and only the first is a
    /// deliberate act by somebody who could plausibly redraw their own footer. The other four used
    /// to change the model silently, leaving the strip naming a model that was no longer in use and
    /// a context meter measuring against the wrong window.
    fn select_model(&mut self, selection: neosh_proto::ModelSelection) {
        self.agent.set_selection(selection.clone());
        // Into a turn that is already running, where the driver can say so. Picking a model
        // mid-turn is something you do *because* of how the turn is going, and one that waited for
        // the next turn would arrive after you had already given up on this one.
        let here = self.active_session();
        for d in &self.agent_drivers {
            d.set_model(&here, &selection);
        }
        self.bridge.broadcast(PluginEvent::SelectionChanged { selection });
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

    fn option_usize(&self, name: &str) -> usize {
        self.editor.options().int(name).unwrap_or(0).max(0) as usize
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
            if self.cancel_turn_here() == Some(true) {
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

            // Nothing left to erase, and a chip above the field: the next backspace takes that
            // off. It is how a chip row behaves everywhere else, it needs no modifier anybody's
            // terminal has an opinion about, and "that was the wrong screenshot" is one key away
            // from having pasted it.
            KeyCode::Backspace if self.composer_text().is_empty() && self.has_attachment() => {
                self.drop_attachment()
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
    // ---- reading the transcript ------------------------------------------
    //
    // A modal reader over the chat buffer. The keys are vi's, chosen rather than invented: `j` and
    // `y` mean here what they mean everywhere else, and the two things this transcript has that a
    // file does not — turns and code blocks — get keys of their own rather than being approximated
    // with line numbers.

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
        self.reading_pending = None;
        self.reading_shape = SelectShape::Exclusive;
        self.reading_goal = None;
        // And the keymap has to be told, or the sentence above is only true of the keys nobody
        // else wanted. Reading's keys arrive through [`Self::handle_unbound_key`], which is the
        // *last* thing consulted — so `^D` scrolled half a screen only until the git plugin bound
        // it in `chat` mode, after which it opened a diff, and `^B` toggled the sidebar instead of
        // going back a page, and `^E` opened the effort picker instead of moving a line. Half of a
        // documented vim vocabulary, quietly taken by whatever loaded first.
        //
        // `Normal` rather than a `Reading` mode of its own: the keys `chat` carries are the ones
        // that compose a message, and none of them means anything here. What stays bound is the
        // set the host puts in *every* mode — `^C`, `^Q`, `^R`, `^S` — which is exactly the set you
        // want to keep working somewhere you did not intend to be. A plugin that wants a key while
        // reading binds it in `normal`, which needs nothing new.
        self.mode_before_reading = self.editor.mode();
        self.editor.set_mode(Mode::Normal);
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::FocusPush { win: self.chat_win });
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.chat_win, on: false });
        // Start at the newest thing said rather than at line one: you came here to take something
        // out of the answer you are looking at.
        let lines = self.chat_lines();
        let last = lines.len().saturating_sub(1) as u32;
        // A block, because the cursor is now *on* a character rather than between two and every
        // key is a verb. It is also the only thing on screen that says so before you press one.
        let _ = self.editor.apply(&plugin, ApiCall::WinCursorShape {
            win: self.chat_win,
            shape: neosh_proto::CursorShape::Block,
        });
        self.reading_jump((last, vim::first_non_blank(lines.last().map(String::as_str).unwrap_or(""))));
        self.refresh_status();
        self.refresh_composer();
    }

    fn leave_reading(&mut self) {
        if !self.reading {
            return;
        }
        self.reading = false;
        self.reading_pending = None;
        self.reading_count.clear();
        self.reading_shape = SelectShape::Exclusive;
        self.reading_goal = None;
        self.editor.set_mode(self.mode_before_reading);
        self.clear_matches();
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::WinCursorShape {
            win: self.chat_win,
            shape: neosh_proto::CursorShape::Bar,
        });
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.chat_win, on: false });
        let _ = self.editor.apply(&plugin, ApiCall::FocusPop);
        // Back to following the newest, because that is what the composer is the bottom of.
        //
        // Reading is a place *in* the transcript and the scroll offset is how it gets there —
        // `reading_jump` sets it on every motion. Left behind, coming back out put you in the
        // composer with the window still parked a hundred rows up: the next thing you typed, the
        // spinner, and the whole answer all arrived below the screen, and what you could still
        // see was old text that had not changed. Every other way *into* a conversation resets
        // this for the same reason — see `enter_session`.
        self.scroll_chat(0);
        self.refresh_status();
        self.refresh_composer();
    }

    /// Keys while reading the transcript.
    ///
    /// Vim's, and resolved by [`crate::vim`] rather than by the core's [`CursorMotion`]: those are
    /// a text field's motions, and a mode where the cursor sits *on* a character rather than
    /// between two needs the other set of answers to `h` at column zero, to `$`, and to what a
    /// selection with one end on the cursor contains. Motion extends a running selection without
    /// being told to, which is what makes `v` and then three `j`s three lines.
    fn reading_key(&mut self, key: neosh_proto::KeyPress) {
        if self.search.is_some() {
            self.search_key(key);
            return;
        }
        let ch = match &key.code {
            KeyCode::Char { c } => c.as_str(),
            _ => "",
        };

        // `f`, `F`, `t` and `T` take the next keystroke as a *character* rather than as a command:
        // `f5` finds a five and `fj` finds a `j`. Before the count and before every table below,
        // each of which would otherwise claim it first.
        if let Some(Pending::Find { till, back }) = self.reading_pending {
            self.reading_pending = None;
            let count = self.take_reading_count().unwrap_or(1);
            if let Some(c) = ch.chars().next() {
                self.reading_find = Some((c, till, back));
                self.reading_motion(vim::Motion::Find { ch: c, till, back }, count);
            }
            self.refresh_composer();
            return;
        }

        // A count, typed before the motion it belongs to. Digits first, because `0` is also a
        // motion: it is the start of the line when nothing is pending and part of the number when
        // something is, which is what it means everywhere else these keys come from.
        if !key.mods.ctrl
            && !key.mods.alt
            && let Some(d) = ch.chars().next().filter(char::is_ascii_digit)
            && !(d == '0' && self.reading_count.is_empty())
        {
            // Three digits is already more rows than any transcript has on screen, and it is what
            // stops a leant-on key growing a string without end.
            if self.reading_count.len() < 3 {
                self.reading_count.push(d);
            }
            self.refresh_composer();
            return;
        }
        // Whatever was typed, the motion that follows is the only thing that spends it — so take it
        // here and let every arm below run with it already gone. A key that is not a motion
        // therefore ends the count, which is what makes a half-typed one abandonable.
        let typed = self.take_reading_count();
        let count = typed.unwrap_or(1);

        // Chords first — before the pending pair and before the plain motions, because a chord is
        // a different key and not a modified one. Read after them, `^B` was matched by the `b` arm
        // of the motion table and moved a word left, `^Y` opened a pending yank, and `^E` moved a
        // word right: three of the six scroll keys taken by the unmodified letters they happen to
        // share. `<C-d>`/`<C-u>` are half a screen because that is what they are everywhere; a full
        // screen with no overlap loses the line you were reading.
        //
        // A screen is counted in *buffer rows on the screen*, which the frontend reports and the
        // window's height is not: a transcript of paragraphs draws six rows in thirty cells, and
        // half a screen taken as half the height is four screenfuls of scrolling.
        let page = self.reading_page();
        let n = count as i64;
        if key.mods.ctrl {
            match ch {
                "d" => return self.reading_by(n * ((page as i64) + 1) / 2),
                "u" => return self.reading_by(-(n * ((page as i64) + 1) / 2)),
                "f" => return self.reading_by(n * page as i64),
                "b" => return self.reading_by(-(n * page as i64)),
                "e" => return self.reading_scroll(n),
                "y" => return self.reading_scroll(-n),
                // The one Vim key that genuinely is not here. Said rather than swallowed, because
                // a chord that does nothing at all reads as a broken keyboard.
                "v" => {
                    return self.editor_message(
                        MessageLevel::Info,
                        "no blockwise selection here — `v` takes characters and `V` whole lines",
                    );
                }
                _ => {}
            }
        }
        // A chord this mode does not define does *nothing*. Falling through, `^A` was read as `a`
        // and put you back in the composer, `^W` as `w` and moved a word — a key that means
        // something else here quietly meaning the letter it is made of. Alt as well as ctrl, and
        // deliberately not shift: shift *is* a modifier on a motion here, and holding it is how a
        // selection is extended.
        if key.mods.ctrl || key.mods.alt {
            return;
        }

        // A pending first key consumes exactly one more press, whatever it is. Anything else and a
        // mistyped `y` would sit there waiting to eat an unrelated keystroke later.
        if let Some(first) = self.reading_pending.take() {
            self.refresh_composer();
            self.reading_pair(first, ch, typed);
            return;
        }

        // The keys that cannot act on their own.
        //
        // `y` is the only one of them that is two things: an operator when nothing is selected, and
        // "copy this" when something is. `g` and `z` mean the same either way, and gating them
        // alongside it made both dead — `v` then `gg` could not extend to the top of the transcript
        // and `zz` could not recentre a selection you were in the middle of making. `i` and `a` are
        // the other way round: outside a selection they are how you get back to the composer, and
        // only inside one are they the front half of a text object.
        let opener = match ch {
            "g" => Some(Pending::G),
            "z" => Some(Pending::Z),
            "y" if !self.chat_anchored() => Some(Pending::Yank),
            "f" => Some(Pending::Find { till: false, back: false }),
            "F" => Some(Pending::Find { till: false, back: true }),
            "t" => Some(Pending::Find { till: true, back: false }),
            "T" => Some(Pending::Find { till: true, back: true }),
            "i" | "a" if self.chat_anchored() => {
                Some(Pending::Object { around: ch == "a", yank: false })
            }
            _ => None,
        };
        if let Some(p) = opener {
            self.reading_pending = Some(p);
            // Put the count back for the key that finishes the pair. `5gg` is one motion with a
            // number in front of it, and the first `g` is not the thing that spends it.
            if let Some(n) = typed {
                self.reading_count = n.to_string();
            }
            self.refresh_composer();
            return;
        }

        // `G` with a count in front of it is a *row* — `12G` — and it is the one motion here where
        // the number is the destination rather than how many times to go. Before the table, because
        // afterwards it is a repetition like everything else in it.
        if ch == "G"
            && let Some(n) = typed
        {
            return self.reading_motion(vim::Motion::Row(n.saturating_sub(1)), 1);
        }

        use vim::Motion as M;
        let motion = match (&key.code, ch) {
            (KeyCode::Left, _) | (_, "h") => Some(M::Left),
            (KeyCode::Right, _) | (_, "l") | (_, " ") => Some(M::Right),
            (KeyCode::Up, _) | (_, "k") => Some(M::Up),
            (KeyCode::Down, _) | (_, "j") => Some(M::Down),
            (_, "w") => Some(M::WordRight { big: false }),
            (_, "W") => Some(M::WordRight { big: true }),
            (_, "b") => Some(M::WordLeft { big: false }),
            (_, "B") => Some(M::WordLeft { big: true }),
            (_, "e") => Some(M::WordEnd { big: false }),
            (_, "E") => Some(M::WordEnd { big: true }),
            (KeyCode::Home, _) | (_, "0") => Some(M::LineStart),
            (_, "^") => Some(M::FirstNonBlank),
            (KeyCode::End, _) | (_, "$") => Some(M::LineEnd),
            (_, "G") => Some(M::BufEnd),
            (_, "%") => Some(M::MatchPair),
            // `;` repeats the last `f`/`t` and `,` repeats it the other way. Nothing to repeat is
            // not an error — it is a key pressed before the one it belongs to.
            (_, ";") => self.reading_find.map(|(c, till, back)| M::Find { ch: c, till, back }),
            (_, ",") => self.reading_find.map(|(c, till, back)| M::Find { ch: c, till, back: !back }),
            _ => None,
        };
        if let Some(m) = motion {
            // Shift on an arrow starts a selection the way it does in a text field. Only ever the
            // arrows and the ends: `h` and `H` are different keys, not one key and a modifier.
            if key.mods.shift && !self.chat_anchored() {
                self.begin_select(SelectShape::Inclusive);
            }
            return self.reading_motion(m, count);
        }

        match (&key.code, ch) {
            (KeyCode::PageDown, _) => self.reading_by(n * page as i64),
            (KeyCode::PageUp, _) => self.reading_by(-(n * page as i64)),

            // The two things a transcript has that a file does not.
            (_, "]") => self.reading_to_turn(1),
            (_, "[") => self.reading_to_turn(-1),
            (_, "}") => self.reading_to_block(1),
            (_, "{") => self.reading_to_block(-1),

            // Where the window is, rather than where the text is.
            (_, "H") => self.reading_screen_row(count.saturating_sub(1)),
            (_, "M") => self.reading_screen_row(page / 2),
            (_, "L") => self.reading_screen_row(page.saturating_sub(count.max(1))),

            (_, "v") => self.select_as(SelectShape::Inclusive),
            (_, "V") => self.select_as(SelectShape::Line),
            // Vim's `o`: the anchor becomes the cursor and the cursor the anchor, so the end you
            // are extending is the other one. Without it a selection begun in the wrong direction
            // is a selection to start again.
            (_, "o") if self.chat_anchored() => self.swap_ends(),

            // A folded card opens; an open one folds. `Tab` because it is the key that toggles a
            // fold in every file tree and outliner; `za` for the Vim hands. See ADR 0033.
            (KeyCode::Tab, _) => self.toggle_card_here(),

            (_, "/") => self.begin_search(false),
            (_, "?") => self.begin_search(true),
            (_, "n") => self.search_step(true),
            (_, "N") => self.search_step(false),
            // The word under the cursor, searched for without typing it.
            (_, "*") => self.search_word(true),
            (_, "#") => self.search_word(false),

            // Yank with a selection running takes the selection, and leaves — you came here to
            // take something, and staying would mean pressing Esc every single time. Without one,
            // `y` has already been held as a pending operator above.
            (_, "y") if self.copy_selection(false) => self.leave_reading(),
            // Anchored, but over nothing at all — which an inclusive selection cannot be, since it
            // always contains the character the cursor is on. Only a `V` on an empty row gets here.
            (_, "y") => {
                self.editor_message(MessageLevel::Warn, "nothing to copy — the selection is empty")
            }
            (_, "Y") => self.yank_line(),

            // Back to typing, at the key every editor uses for it, because the usual reason to
            // stop reading is that you have thought of what to say.
            (KeyCode::Enter, _) | (_, "i") | (_, "a") | (_, "o") => self.leave_reading(),
            // Esc gives up one thing per press, in the order you stopped wanting them: the
            // selection, then the search highlight, then the mode. A running selection is the most
            // local of the three and the one you are most likely to have changed your mind about —
            // half way up a turn, having decided you wanted a different piece of it. Leaving
            // outright there is a keystroke that undoes the wrong amount: the transcript is the
            // thing you were still reading.
            (KeyCode::Esc, _) if self.chat_anchored() => self.drop_selection(),
            (KeyCode::Esc, _) if !self.searched.is_empty() => {
                self.searched.clear();
                self.clear_matches();
                self.refresh_composer();
            }
            (KeyCode::Esc, _) | (_, "q") => self.leave_reading(),
            _ => {}
        }
    }

    /// The second key of a pair: `gg`, `ge`, `gv`, `zz`, the `y` operator and a text object.
    ///
    /// `typed` is the count the *first* key put back for this one — `5gg` is one motion with a
    /// number in front of it, and the `g` that opened the pair is not what spends it.
    fn reading_pair(&mut self, first: Pending, ch: &str, typed: Option<u32>) {
        // A key with no character on it — `Esc`, an arrow, `PgDn` — ends the sequence and does
        // nothing else. Quietly: pressing `Esc` because you have changed your mind about a yank
        // should not be answered with a sentence about what `y` takes.
        if ch.is_empty() {
            return;
        }
        let count = typed.unwrap_or(1);
        let page = self.reading_page();
        use vim::Motion as M;
        match (first, ch) {
            (Pending::G, "g") => match typed {
                Some(n) => self.reading_motion(M::Row(n.saturating_sub(1)), 1),
                None => self.reading_motion(M::BufStart, 1),
            },
            (Pending::G, "e") => self.reading_motion(M::WordEndBack { big: false }, count),
            (Pending::G, "E") => self.reading_motion(M::WordEndBack { big: true }, count),
            (Pending::G, "_") => self.reading_motion(M::LastNonBlank, count),
            // The selection you last gave up, back again. What makes `Esc` cheap: dropping one to
            // look at something is otherwise dropping one you have to make again.
            (Pending::G, "v") => self.reselect(),

            // Where the cursor's line sits on screen. `zz` is the one worth having: it is how you
            // read an answer that ends at the bottom edge.
            (Pending::Z, "z" | ".") => self.place_cursor_line(page / 2),
            (Pending::Z, "t") => self.place_cursor_line(0),
            (Pending::Z, "b" | "-") => self.place_cursor_line(page.saturating_sub(1)),
            (Pending::Z, "a") => self.toggle_card_here(),

            (Pending::Yank, "y") => self.yank_line(),
            (Pending::Yank, "c") => self.yank_code(),
            (Pending::Yank, "m" | "t") => self.yank_turn(),
            (Pending::Yank, "a") => {
                self.select_all(self.chat_win);
                if self.copy_selection(false) {
                    self.leave_reading();
                }
            }
            // `yi"`, `yiw`. Only the `i` half: `ya` is already "copy the whole transcript", which
            // is documented, bound and older than this — so `yaw` is the one shape of this that
            // cannot be had, and `viwy` is how you get it.
            (Pending::Yank, "i") => {
                self.reading_pending = Some(Pending::Object { around: false, yank: true });
                self.refresh_composer();
            }
            // `y` over a motion: `yw`, `y$`, `y5j`, `yG`. Whatever the motion covers, from where
            // the cursor is to where it would have gone.
            (Pending::Yank, _) => match self.yank_motion_for(ch) {
                Some(m) => self.yank_over(m, count),
                None => self.editor_message(
                    MessageLevel::Info,
                    "y then a motion, or one of y c m a — `?` lists them",
                ),
            },

            (Pending::Object { around, yank }, _) => {
                if let Some(obj) = object_for(ch) {
                    self.apply_object(obj, around, yank);
                }
            }
            // Answered above, before the count and before every table: it takes a literal
            // character, so it can never reach here.
            (Pending::Find { .. }, _) => {}
            _ => {}
        }
    }

    /// The motions `y` will take as its second key.
    ///
    /// Not every motion in the table: `yy` is the line and `yc`, `ym` and `ya` are the transcript's
    /// own three, so the letters they use are theirs. Everything here is a key that means the same
    /// after `y` as it does on its own.
    fn yank_motion_for(&self, ch: &str) -> Option<vim::Motion> {
        use vim::Motion as M;
        Some(match ch {
            "w" => M::WordRight { big: false },
            "W" => M::WordRight { big: true },
            "b" => M::WordLeft { big: false },
            "B" => M::WordLeft { big: true },
            "e" => M::WordEnd { big: false },
            "E" => M::WordEnd { big: true },
            "h" => M::Left,
            "l" | " " => M::Right,
            "j" => M::Down,
            "k" => M::Up,
            "0" => M::LineStart,
            "^" => M::FirstNonBlank,
            "$" => M::LineEnd,
            "G" => M::BufEnd,
            "%" => M::MatchPair,
            ";" => {
                let (c, till, back) = self.reading_find?;
                M::Find { ch: c, till, back }
            }
            _ => return None,
        })
    }

    /// How many rows of transcript are on screen.
    /// How wide the transcript is, for the one thing that has to fit on a single row.
    ///
    /// Everything else in the transcript is prose and the frontend wraps it. A tool card is not
    /// prose — it is one line whose job is to be scannable, and a card that wraps puts `n .cla…)`
    /// on a row of its own, which is worse than saying less.
    fn chat_width(&self) -> usize {
        self.editor
            .window(self.chat_win)
            .and_then(|w| w.viewport.map(|v| v.width as usize))
            .unwrap_or(80)
            .max(24)
    }

    fn chat_height(&self) -> u32 {
        self.editor
            .window(self.chat_win)
            .and_then(|w| w.viewport.map(|v| v.height as u32))
            .unwrap_or(20)
            .max(1)
    }

    /// The transcript, as plain rows.
    fn chat_lines(&self) -> Vec<String> {
        self.editor
            .buffer(self.chat)
            .map(|b| b.lines().iter().map(|l| l.text.clone()).collect())
            .unwrap_or_default()
    }

    /// Go to a place in the transcript, extending a running selection over what was jumped.
    fn reading_jump(&mut self, at: (u32, u32)) {
        self.reading_goal = None;
        self.place_cursor(at);
    }

    /// The count typed before a motion, if one was, and forget it.
    ///
    /// `None` rather than `1` for "nobody typed one", because the two are different keys: `G` is
    /// the end of the transcript and `1G` is its first row.
    fn take_reading_count(&mut self) -> Option<u32> {
        let typed = std::mem::take(&mut self.reading_count);
        typed.parse::<u32>().ok().filter(|n| *n > 0)
    }

    fn chat_cursor(&self) -> (u32, u32) {
        self.editor.window(self.chat_win).map(|w| w.cursor).unwrap_or((0, 0))
    }

    /// Put the cursor somewhere, keeping everything that follows it in step.
    ///
    /// `WinSetCursor` rather than `WinMotion`: the anchor is the selection's other end and it has
    /// to stay exactly where it is, which is what makes a jump take the text it jumped over. The
    /// core repaints the highlight either way — it did not always, and every jump the reader makes
    /// extended a selection that could not be seen until the next `hjkl`.
    fn place_cursor(&mut self, at: (u32, u32)) {
        let at = vim::clamp(&self.chat_lines(), at);
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinSetCursor {
            win: self.chat_win,
            row: at.0,
            col: at.1,
        });
        self.follow_cursor();
        // The hint row says what the row under the cursor can do, so it moves with it.
        self.refresh_composer();
    }

    /// One motion, `count` times.
    ///
    /// Repeated rather than computed: `5j` is five of what `j` does, including what it does at the
    /// ends and to the column it remembers. A motion that stops moving stops the count with it, so
    /// `50j` at the bottom of the transcript is one press that goes nowhere rather than fifty.
    fn reading_motion(&mut self, m: vim::Motion, count: u32) {
        let lines = self.chat_lines();
        let mut at = self.chat_cursor();
        let mut goal = self.reading_goal;
        // `$` is the third motion that leaves a column behind: it remembers "the end of the row",
        // so `$jj` walks down the right-hand edge of the text the way it does in Vim.
        let vertical = matches!(m, vim::Motion::Up | vim::Motion::Down | vim::Motion::LineEnd);
        // The ends of the transcript and an absolute row are destinations, and going to one five
        // times is going to it.
        let once = matches!(m, vim::Motion::BufStart | vim::Motion::BufEnd | vim::Motion::Row(_));
        for _ in 0..if once { 1 } else { count.max(1) } {
            let step = vim::resolve(&lines, at, m, goal);
            at = step.at;
            goal = step.goal;
            if !step.moved {
                break;
            }
        }
        // Cleared by anything horizontal, kept by `j` and `k`. Without it, going down past a short
        // row and back up leaves the cursor at that row's width and it never finds its place again.
        self.reading_goal = vertical.then_some(goal).flatten();
        self.place_cursor(at);
    }

    /// Move by whole rows, keeping the column — what `^D`, `^U` and the page keys do.
    ///
    /// Expressed as that many `j`s rather than as arithmetic on a row number, so paging clamps,
    /// remembers a column and lands on a real position by exactly the same rules one keypress does.
    fn reading_by(&mut self, delta: i64) {
        if delta == 0 {
            return;
        }
        let m = if delta > 0 { vim::Motion::Down } else { vim::Motion::Up };
        self.reading_motion(m, delta.unsigned_abs().min(u32::MAX as u64) as u32);
    }

    /// `^E` and `^Y`: move the *window*, and take the cursor along only if it would fall off.
    ///
    /// The other five scroll keys move the cursor and let the view follow. These two are the pair
    /// that does it the other way round, which is the whole reason to have them.
    fn reading_scroll(&mut self, delta: i64) {
        let last = self.chat_lines().len().saturating_sub(1) as i64;
        let page = self.reading_page() as i64;
        let next = (self.chat_showing() as i64 + delta).clamp(0, last.max(0));
        self.chat_top = Some(next as u32);
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinScrollTo {
            win: self.chat_win,
            top_line: Some(next as u32),
        });
        let row = self.chat_cursor().0 as i64;
        let want = row.clamp(next, (next + page - 1).min(last).max(next));
        if want != row {
            self.reading_by(want - row);
        } else {
            self.refresh_composer();
        }
    }

    /// `H`, `M` and `L`: a row counted from the top of the *window* rather than of the transcript.
    fn reading_screen_row(&mut self, offset: u32) {
        let lines = self.chat_lines();
        let last = lines.len().saturating_sub(1) as u32;
        let page = self.reading_page();
        let row = self.chat_showing().saturating_add(offset.min(page.saturating_sub(1))).min(last);
        let col = vim::first_non_blank(lines.get(row as usize).map(String::as_str).unwrap_or(""));
        self.reading_goal = None;
        self.place_cursor((row, col));
    }

    /// Buffer rows on the screen, which is what a page is here.
    ///
    /// Reported by the frontend, because it is the only thing that knows: one buffer row of a
    /// wrapped transcript is several rows of screen, and the window's height counts the wrong one.
    /// Falling back to the height is what an unmeasured window gets, and it is never worse than
    /// what this used to do unconditionally.
    fn reading_page(&self) -> u32 {
        self.editor
            .window(self.chat_win)
            .and_then(|w| w.viewport)
            .map(|v| v.rows)
            .filter(|r| *r > 0)
            .unwrap_or_else(|| self.chat_height())
            .max(1)
    }

    /// The first buffer row on the screen right now.
    fn chat_showing(&self) -> u32 {
        self.chat_top.unwrap_or_else(|| {
            self.editor
                .window(self.chat_win)
                .and_then(|w| w.viewport)
                .map(|v| v.top_line)
                .unwrap_or(0)
        })
    }

    /// `*` and `#`: search for the word under the cursor without typing it.
    fn search_word(&mut self, forward: bool) {
        let lines = self.chat_lines();
        let Some(word) = vim::word_under(&lines, self.chat_cursor()) else {
            return self.editor_message(MessageLevel::Info, "no word under the cursor");
        };
        self.searched = word;
        self.searched_back = !forward;
        let query = self.searched.clone();
        self.highlight_matches(&query);
        match self.find_from(&query, self.chat_cursor(), forward, false) {
            Some(at) => self.reading_jump(at),
            None => self.editor_message(MessageLevel::Warn, format!("only match for {query:?}")),
        }
    }

    /// Rows where a turn begins: the first line of a question, marked by the bar down its left.
    ///
    /// Read off the buffer rather than remembered alongside it. A remembered list is one more
    /// thing to keep true across a session switch, an answer being rewritten as it settles, and a
    /// transcript rebuilt from stored messages — and it would be wrong in exactly the cases where
    /// somebody is scrolling back through a long conversation looking for something.
    fn turn_rows(&self) -> Vec<u32> {
        let bar = self.glyphs().bar;
        let lines = self.chat_lines();
        let mut out = Vec::new();
        let mut inside = false;
        for (i, line) in lines.iter().enumerate() {
            let is_bar = line.starts_with(bar);
            if is_bar && !inside {
                out.push(i as u32);
            }
            inside = is_bar;
        }
        out
    }

    fn reading_to_turn(&mut self, dir: i32) {
        let here = self.chat_cursor().0;
        let rows = self.turn_rows();
        let next = if dir > 0 {
            rows.into_iter().find(|r| *r > here)
        } else {
            rows.into_iter().rev().find(|r| *r < here)
        };
        match next {
            Some(row) => self.reading_jump((row, 0)),
            None => self.editor_message(
                MessageLevel::Info,
                if dir > 0 { "no later turn" } else { "no earlier turn" },
            ),
        }
    }

    /// The next or previous blank-line boundary.
    ///
    /// Which in this buffer is a paragraph, a list, a table and a code block all at once, because
    /// the markdown renderer separates every block with one — so `}` steps through an answer the
    /// way its author wrote it rather than by a fixed number of rows.
    fn reading_to_block(&mut self, dir: i32) {
        let lines = self.chat_lines();
        let last = lines.len().saturating_sub(1) as i64;
        let blank = |i: i64| lines.get(i as usize).is_none_or(|l| l.trim().is_empty());
        let mut at = self.chat_cursor().0 as i64;
        let step = dir as i64;
        // Off whatever run of blanks we are standing in first, then on to the next one, so holding
        // the key walks block by block instead of stalling in the gap between two.
        while at + step >= 0 && at + step <= last && blank(at + step) == blank(at) {
            at += step;
        }
        at = (at + step).clamp(0, last.max(0));
        while at + step >= 0 && at + step <= last && blank(at) {
            at += step;
        }
        self.reading_jump((at.clamp(0, last.max(0)) as u32, 0));
    }

    /// Scroll so the cursor's line lands `at` rows down the window.
    fn place_cursor_line(&mut self, at: u32) {
        let row = self.chat_cursor().0;
        let top = row.saturating_sub(at);
        self.chat_top = Some(top);
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinScrollTo {
            win: self.chat_win,
            top_line: Some(top),
        });
    }

    /// `v` and `V`: start a selection of that shape, change a running one into it, or — pressed on
    /// the shape it already is — drop it.
    ///
    /// Vim's, and not out of nostalgia: a second `v` meaning "no longer selecting" is what every
    /// hand expects, and having `V` restart from one line instead would throw away the four you had
    /// just extended over.
    fn select_as(&mut self, shape: SelectShape) {
        match (self.chat_anchored(), self.reading_shape == shape) {
            (true, true) => self.drop_selection(),
            // Between the two shapes, keeping both ends: `v` narrows a linewise selection back to
            // where the cursor actually is, and `V` widens the one you have to whole lines.
            (true, false) => self.set_shape(shape),
            (false, _) => self.begin_select(shape),
        }
        self.refresh_composer();
    }

    /// Anchor a selection of this shape where the cursor is.
    fn begin_select(&mut self, shape: SelectShape) {
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.chat_win, on: true });
        self.set_shape(shape);
    }

    fn set_shape(&mut self, shape: SelectShape) {
        self.reading_shape = shape;
        let _ = self
            .editor
            .apply(&PluginId::from(BUILTIN), ApiCall::WinSelectShape { win: self.chat_win, shape });
    }

    /// Give up the selection, keeping it for `gv`.
    ///
    /// Kept rather than forgotten because `Esc` is the cheap key here: you drop a selection to look
    /// at something and want it back, and one you have to make again is one you do not drop.
    fn drop_selection(&mut self) {
        if let Some(w) = self.editor.window(self.chat_win)
            && let Some(anchor) = w.anchor
        {
            self.reading_last_select = Some((anchor, w.cursor, self.reading_shape));
        }
        self.reading_shape = SelectShape::Exclusive;
        let _ = self
            .editor
            .apply(&PluginId::from(BUILTIN), ApiCall::WinSelect { win: self.chat_win, on: false });
        self.refresh_composer();
    }

    /// `gv`: the selection you last gave up, back where it was.
    fn reselect(&mut self) {
        let Some((anchor, cursor, shape)) = self.reading_last_select else {
            return self.editor_message(MessageLevel::Info, "nothing selected here yet");
        };
        let lines = self.chat_lines();
        // The transcript may be shorter than it was — a conversation switch, a card that folded.
        // Clamping is what keeps `gv` from naming rows that are now somebody else's.
        self.select_between(vim::clamp(&lines, anchor), vim::clamp(&lines, cursor), shape);
    }

    /// `o`: the anchor becomes the cursor and the cursor the anchor.
    fn swap_ends(&mut self) {
        let Some(w) = self.editor.window(self.chat_win) else { return };
        let (Some(anchor), cursor) = (w.anchor, w.cursor) else { return };
        self.select_between(cursor, anchor, self.reading_shape);
    }

    /// Put a selection of `shape` between two positions, cursor at the second.
    ///
    /// Three calls, because the anchor is only settable by standing on it — which is the whole of
    /// the public API for a selection and exactly what a plugin doing this would have to write.
    fn select_between(&mut self, anchor: (u32, u32), cursor: (u32, u32), shape: SelectShape) {
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::WinSetCursor {
            win: self.chat_win,
            row: anchor.0,
            col: anchor.1,
        });
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.chat_win, on: true });
        self.set_shape(shape);
        self.reading_goal = None;
        self.place_cursor(cursor);
    }

    /// `iw`, `a"`, `ip`: select what the object names, or copy it.
    fn apply_object(&mut self, obj: vim::Object, around: bool, yank: bool) {
        let lines = self.chat_lines();
        let at = self.chat_cursor();
        let Some((from, to)) = vim::object(&lines, at, obj, around) else {
            return self.editor_message(MessageLevel::Info, "the cursor is not in one of those");
        };
        if yank {
            self.yank_text(from, to, true, "it");
        } else {
            self.select_between(from, to, SelectShape::Inclusive);
        }
    }

    /// `yw`, `y$`, `y5j`: copy what a motion covers.
    fn yank_over(&mut self, m: vim::Motion, count: u32) {
        let lines = self.chat_lines();
        let from = self.chat_cursor();
        let mut at = from;
        let mut goal = None;
        let mut inclusive = false;
        let once = matches!(m, vim::Motion::BufStart | vim::Motion::BufEnd | vim::Motion::Row(_));
        for _ in 0..if once { 1 } else { count.max(1) } {
            let step = vim::resolve(&lines, at, m, goal);
            at = step.at;
            goal = step.goal;
            inclusive = step.inclusive;
            if !step.moved {
                break;
            }
        }
        // Vim's rule, and it is not a detail: a motion that spans rows takes whole ones. `yj` is
        // two lines rather than two ragged halves of them, and `yG` is the rest of the transcript
        // rather than the rest of this row and then all of the last one.
        let linewise = matches!(
            m,
            vim::Motion::Up | vim::Motion::Down | vim::Motion::Row(_) | vim::Motion::BufStart | vim::Motion::BufEnd
        );
        if linewise {
            let (a, b) = (from.0.min(at.0), from.0.max(at.0));
            self.yank_rows(a..b + 1, if a == b { "the line" } else { "the lines" }, false);
        } else {
            self.yank_text(from, at, inclusive, "it");
        }
    }

    /// Copy a range of the transcript, and say how much.
    fn yank_text(&mut self, from: (u32, u32), to: (u32, u32), inclusive: bool, what: &str) {
        let lines = self.chat_lines();
        let (a, mut b) = if from <= to { (from, to) } else { (to, from) };
        if inclusive {
            let text = lines.get(b.0 as usize).map(String::as_str).unwrap_or("");
            b.1 = neosh_core::text::after(text, b.1);
        }
        let text = vim::slice(&lines, a, b);
        if text.is_empty() {
            return self.editor_message(MessageLevel::Warn, "nothing to copy");
        }
        let n = text.lines().count().max(1);
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::ClipboardWrite { text });
        self.editor_message(
            MessageLevel::Info,
            format!("copied {what} — {n} line{}", if n == 1 { "" } else { "s" }),
        );
    }

    /// Copy some rows of the transcript, and say how many.
    fn yank_rows(&mut self, rows: std::ops::Range<u32>, what: &str, strip: bool) {
        let lines = self.chat_lines();
        let taken: Vec<String> = lines
            .iter()
            .skip(rows.start as usize)
            .take(rows.len())
            .map(|l| if strip { l.strip_prefix("  ").unwrap_or(l).to_string() } else { l.clone() })
            .collect();
        let text = taken.join("\n");
        if text.trim().is_empty() {
            self.editor_message(MessageLevel::Warn, format!("nothing to copy — no {what} here"));
            return;
        }
        let n = taken.len();
        let _ = self
            .editor
            .apply(&PluginId::from(BUILTIN), ApiCall::ClipboardWrite { text });
        self.editor_message(
            MessageLevel::Info,
            format!("copied {what} — {n} line{}", if n == 1 { "" } else { "s" }),
        );
    }

    fn yank_line(&mut self) {
        let row = self.chat_cursor().0;
        self.yank_rows(row..row + 1, "the line", false);
    }

    /// Copy the code block the cursor is in, without its indent.
    ///
    /// Found from the marks rather than from the text: a fenced block's rows are the ones the
    /// renderer coloured as code across their whole width, which is a fact it already recorded and
    /// which no amount of looking at the characters could recover — the back-ticks are gone by
    /// the time this buffer exists, because they were punctuation for the parser.
    fn yank_code(&mut self) {
        let Some(rows) = self.code_block_at(self.chat_cursor().0) else {
            self.editor_message(MessageLevel::Warn, "the cursor is not in a code block");
            return;
        };
        self.yank_rows(rows, "the code block", true);
    }

    /// Whether a row is the language line the renderer draws above a fenced block.
    fn is_fence_header_row(buf: &neosh_core::buffer::Buffer, row: u32) -> bool {
        buf.render_range(row, row + 1).first().is_some_and(|r| {
            r.marks.iter().any(|m| m.opts.hl_group.as_deref() == Some("Markdown.Fence"))
        })
    }

    fn code_block_at(&self, row: u32) -> Option<std::ops::Range<u32>> {
        let buf = self.editor.buffer(self.chat)?;
        let count = buf.line_count();
        if row >= count {
            return None;
        }
        // A row is code when a mark covers the whole of it with the code group. Sub-ranges are
        // inline back-ticks, which are part of a sentence and not a block to take away.
        let is_code = |i: u32| -> bool {
            buf.render_range(i, i + 1).first().is_some_and(|r| {
                !r.text.is_empty()
                    && r.marks.iter().any(|m| {
                        m.opts.hl_group.as_deref() == Some("Markdown.Code")
                            && m.col == 0
                            && m.opts.end_col == Some(r.text.len() as u32)
                    })
            })
        };
        // Standing on the language header is standing on the block: it is drawn as part of it, it
        // is the first row of it you can see, and it is where the cursor lands coming down from
        // above. Refusing there would be pedantry about a distinction only the parser cares about.
        let row = if is_code(row) {
            row
        } else if Self::is_fence_header_row(buf, row) && row + 1 < count && is_code(row + 1) {
            row + 1
        } else {
            return None;
        };
        let mut from = row;
        while from > 0 && is_code(from - 1) {
            from -= 1;
        }
        let mut to = row + 1;
        while to < count && is_code(to) {
            to += 1;
        }
        Some(from..to)
    }

    /// Copy the whole exchange the cursor is in — the question and everything it produced.

    fn yank_turn(&mut self) {
        let here = self.chat_cursor().0;
        let rows = self.turn_rows();
        let from = rows.iter().rev().find(|r| **r <= here).copied().unwrap_or(0);
        let to = rows
            .iter()
            .find(|r| **r > here)
            .copied()
            .unwrap_or_else(|| self.chat_lines().len() as u32);
        self.yank_rows(from..to, "the turn", false);
    }

    // ---- searching -------------------------------------------------------

    /// Start typing a search, borrowing the composer for it.
    fn begin_search(&mut self, back: bool) {
        if self.search.is_some() {
            return;
        }
        self.search = Some(SearchPrompt {
            query: String::new(),
            back,
            from: self.chat_cursor(),
            draft: self.composer_text(),
        });
        self.render_search();
        self.refresh_status();
    }

    fn render_search(&mut self) {
        let Some(p) = self.search.as_ref() else { return };
        let line = format!("{}{}", if p.back { "?" } else { "/" }, p.query);
        self.set_composer(&line);
    }

    fn search_key(&mut self, key: neosh_proto::KeyPress) {
        let Some(p) = self.search.as_mut() else { return };
        match key.code {
            KeyCode::Esc => return self.finish_search(false),
            KeyCode::Enter => return self.finish_search(true),
            KeyCode::Backspace => {
                if p.query.is_empty() {
                    return self.finish_search(false);
                }
                let keep = p.query.graphemes(true).count().saturating_sub(1);
                p.query = p.query.graphemes(true).take(keep).collect();
            }
            KeyCode::Char { ref c } if key.mods.ctrl => match c.as_str() {
                "u" => p.query.clear(),
                "c" => return self.finish_search(false),
                _ => return,
            },
            KeyCode::Char { ref c } if !key.mods.alt => p.query.push_str(c),
            _ => return,
        }
        self.render_search();
        // Incremental: the hits light up as you type, so a search that was going to find nothing
        // says so before you have finished typing it.
        let (query, back, from) = {
            let p = self.search.as_ref().expect("checked above");
            (p.query.clone(), p.back, p.from)
        };
        self.highlight_matches(&query);
        if !query.is_empty() && let Some(at) = self.find_from(&query, from, !back, true) {
            self.reading_jump(at);
        }
    }

    fn finish_search(&mut self, submit: bool) {
        let Some(p) = self.search.take() else { return };
        self.set_composer(&p.draft);
        if submit && !p.query.is_empty() {
            self.searched = p.query.clone();
            self.searched_back = p.back;
            self.highlight_matches(&p.query);
            match self.find_from(&p.query, p.from, !p.back, true) {
                Some(at) => self.reading_jump(at),
                None => self.editor_message(MessageLevel::Warn, format!("no match for {:?}", p.query)),
            }
        } else {
            self.clear_matches();
            self.reading_jump(p.from);
        }
        self.refresh_status();
        self.refresh_composer();
    }

    /// `n` and `N`: on to the next hit for whatever was last searched for.
    fn search_step(&mut self, same_direction: bool) {
        if self.searched.is_empty() {
            self.editor_message(MessageLevel::Info, "nothing to search for yet — `/` starts one");
            return;
        }
        let query = self.searched.clone();
        // `n` is *onwards*, not forwards: after `?foo` it goes on up the transcript and `N` comes
        // back down it. Fixed at `forward` either way, `?` found one match and then walked away
        // from every other one.
        let forward = same_direction != self.searched_back;
        match self.find_from(&query, self.chat_cursor(), forward, false) {
            Some(at) => self.reading_jump(at),
            None => self.editor_message(MessageLevel::Warn, format!("no more matches for {query:?}")),
        }
    }

    /// The next match, wrapping round the end.
    ///
    /// `include_here` is the difference between typing a search — which should land on the match
    /// under the cursor if there is one — and pressing `n`, which must move.
    fn find_from(
        &self,
        query: &str,
        from: (u32, u32),
        forward: bool,
        include_here: bool,
    ) -> Option<(u32, u32)> {
        let lines = self.chat_lines();
        if lines.is_empty() || query.is_empty() {
            return None;
        }
        let needle = query.to_lowercase();
        let n = lines.len();
        // Case-insensitive unless the query has a capital in it, which is `smartcase` and is what
        // everybody has turned on anyway.
        let cased = query.chars().any(char::is_uppercase);
        let hits = |row: usize| -> Vec<u32> {
            let hay = if cased { lines[row].clone() } else { lines[row].to_lowercase() };
            let pat = if cased { query } else { needle.as_str() };
            let mut out = Vec::new();
            let mut at = 0;
            while let Some(i) = hay[at..].find(pat) {
                out.push((at + i) as u32);
                at += i + pat.len().max(1);
            }
            out
        };
        for step in 0..=n {
            let row = if forward {
                (from.0 as usize + step) % n
            } else {
                (from.0 as usize + n - (step % n)) % n
            };
            let cols = hits(row);
            let found = if step == 0 {
                let here = from.1;
                if forward {
                    cols.into_iter().find(|c| if include_here { *c >= here } else { *c > here })
                } else {
                    cols.into_iter().rev().find(|c| if include_here { *c <= here } else { *c < here })
                }
            } else if forward {
                cols.into_iter().next()
            } else {
                cols.into_iter().next_back()
            };
            if let Some(col) = found {
                return Some((row as u32, col));
            }
        }
        None
    }

    /// Colour every hit, so the shape of where a word appears is visible without walking to each.
    fn highlight_matches(&mut self, query: &str) {
        self.clear_matches();
        if query.is_empty() {
            return;
        }
        let lines = self.chat_lines();
        let cased = query.chars().any(char::is_uppercase);
        let needle = query.to_lowercase();
        let plugin = PluginId::from(BUILTIN);
        // Bounded, because a one-letter query against a long transcript is thousands of marks and
        // every one of them travels to the frontend. The cursor still walks to every hit.
        let mut left = 500usize;
        for (row, line) in lines.iter().enumerate() {
            let hay = if cased { line.clone() } else { line.to_lowercase() };
            let pat = if cased { query } else { needle.as_str() };
            let mut at = 0usize;
            while let Some(i) = hay[at..].find(pat) {
                let from = at + i;
                let _ = self.editor.apply(&plugin, ApiCall::MarkSet {
                    ns: self.search_ns,
                    buf: self.chat,
                    row: row as u32,
                    col: from as u32,
                    opts: neosh_proto::ExtmarkOpts {
                        end_col: Some((from + pat.len()) as u32),
                        hl_group: Some("Search".into()),
                        line_hl_group: None,
                        virt_text: Vec::new(),
                        virt_text_pos: VirtTextPos::Eol,
                        on_delete: neosh_proto::OnDelete::Invalidate,
                        priority: 50,
                    },
                });
                left -= 1;
                if left == 0 {
                    return;
                }
                at = from + pat.len().max(1);
            }
        }
    }

    fn clear_matches(&mut self) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::MarkClear {
            ns: self.search_ns,
            buf: self.chat,
            start: None,
            end: None,
        });
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
        // Buffer rows on the screen, which after wrapping is not the window's height — the
        // frontend counts them and reports them back, and taking the height instead is what made
        // this believe a cursor was visible while it was four rows below the bottom edge.
        //
        // A first approximation and not the last word: the frontend bends the scroll it is given
        // so the caret is on screen whatever this says, and reports what it actually drew. What
        // this is for is naming the right *region* — a cursor five hundred rows up is not
        // something the drawing end should be asked to walk to.
        let rows = if v.rows > 0 { v.rows } else { (v.height as u32).max(1) };
        let lines = self.editor.buffer(self.chat).map(|b| b.line_count()).unwrap_or(0);
        // `None` means "following the tail", which is a different place from line zero — and the
        // first row of the transcript is a place the reader arrives at, so it has to be sayable.
        // Sent as a bare `0` it was read as "follow" instead, and `^U` up to the top of a long
        // answer put the window back at the bottom of it, cursor off screen, with nothing on the
        // way there to say what had happened.
        let showing = self.chat_top.unwrap_or_else(|| lines.saturating_sub(rows));
        let next = if row < showing {
            row
        } else if row >= showing + rows {
            row + 1 - rows
        } else {
            return;
        };
        self.chat_top = Some(next);
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinScrollTo {
            win: self.chat_win,
            top_line: Some(next),
        });
    }

    /// Redraw everything that is about the conversation you are now in.
    ///
    /// Called after every switch, create and close. The transcript is rebuilt from the session's
    /// messages rather than replayed from the UI events that first produced it: the messages are
    /// the truth, and a switch that reconstructed the buffer from a log of edits would drift the
    /// moment anything else touched it.
    /// Draw the conversation on screen again from its messages, and touch nothing else.
    ///
    /// The buffer half of [`Self::enter_session`], for the times the *replay* has changed its mind
    /// about a conversation you are already in. The live path only ever appends, so anything it has
    /// already drawn stays drawn — which is right for a transcript, and wrong for the one row in it
    /// that is a statement about the conversation's tail rather than about something that happened.
    ///
    /// Deliberately not `enter_session`: that clears the unread mark, leaves reading mode, and
    /// resets the working line and the scroll, all of which are correct on arrival and destructive
    /// in the middle of a conversation you never left.
    fn redraw_transcript(&mut self) {
        let ascii = self.option_bool("ui.ascii_only");
        let limits = self.limits();
        let width = self.chat_width();
        let show_tools = self.option_bool("chat.show_tools");
        let here = self.active_session();
        let running = self.turns.contains_key(&here);
        let (lines, marks) = {
            let store = self.agent.sessions();
            let s = store.active();
            let (lines, marks, _) = transcript(s, ascii, limits, width, show_tools, running);
            (lines, marks)
        };
        let plugin = PluginId::from(BUILTIN);
        // Marks first, and all of them: a mark clamps rather than dies when the line under it is
        // replaced, so a rebuild that skipped this would leave every previous mark stacked on the
        // rows that happen to survive.
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
        self.draw_marks(&marks, 0);
    }

    fn enter_session(&mut self) {
        // First, and while the transcript being read is still the one on screen. Reading is a place
        // *in* a conversation — a cursor at a row, a selection between two of them, a search
        // highlight — and every one of those addresses text that is about to be replaced by
        // somebody else's. Left running, the mode survives into a conversation it was never opened
        // on, and the rows it names there are whatever happens to be at those numbers.
        self.leave_reading();
        // Arriving is reading. Every way into a conversation comes through here — switching, a new
        // one, the one you land on after closing or archiving, and the one a restored workspace
        // opens on — so this is the one place that has to clear the mark, and a flag left set on
        // the conversation you are sitting in would say "new" about the answer on screen.
        let arrived = {
            let mut store = self.agent.sessions();
            let here = store.active_id().clone();
            store.mark_read(&here)
        };
        if arrived {
            self.persist_sessions();
        }
        // Not closed: the buffer is about to be replaced wholesale, so settling an answer into rows
        // that are on their way out would write into the conversation being left behind.
        self.answer = None;
        self.streaming = None;
        // A row in the transcript being replaced, so it names nothing after this. The rebuild
        // below draws the conversation from its messages, where an unanswered question is simply
        // the last one in the list.
        self.unanswered = None;
        // Both together: the footer is part of the working line, and a row count left behind from
        // the conversation being left would tell the next `chat_end` to insert inside a buffer
        // that no longer has those rows.
        self.working = false;
        self.plan_rows = 0;
        self.chat_top = None;
        let ascii = self.option_bool("ui.ascii_only");
        let limits = self.limits();
        let width = self.chat_width();
        let show_tools = self.option_bool("chat.show_tools");
        let here = self.active_session();
        let running = self.turns.contains_key(&here);
        let (lines, marks, cards, label) = {
            let store = self.agent.sessions();
            let s = store.active();
            let (lines, marks, cards) = transcript(s, ascii, limits, width, show_tools, running);
            (lines, marks, cards, s.label())
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
        self.draw_marks(&marks, 0);
        // Where the window *is*, and not only what this end believes about it. `chat_top = None`
        // says "following the tail"; the window has to be told, because it is still sitting at
        // whatever row the last conversation was scrolled to — and a scroll offset is kept rather
        // than derived, precisely so that a client attaching later can be told it. Read a long
        // answer with `^S`, go up, come out, switch project: the new conversation was drawn from
        // row two hundred of a transcript that no longer exists, which is to say it was blank.
        //
        // The cursor with it. Nothing draws a caret in the transcript while the composer has focus,
        // but every question the reader asks — which card is under it, which turn, which block — is
        // asked of a row number, and one left over from a longer conversation is off the end of a
        // shorter one.
        let _ = self
            .editor
            .apply(&plugin, ApiCall::WinScrollTo { win: self.chat_win, top_line: None });
        let _ = self
            .editor
            .apply(&plugin, ApiCall::WinSetCursor { win: self.chat_win, row: 0, col: 0 });
        let _ = self.editor.apply(&plugin, ApiCall::WinSelect { win: self.chat_win, on: false });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetName {
            buf: self.chat,
            name: format!("[chat] {label}"),
        });
        // Nothing of the previous conversation's welcome survives into this one: the buffer was
        // just replaced wholesale, so what `welcome_rows` remembered is about text that is gone.
        self.welcome_rows = 0;
        // The rows the redraw just put the cards on. Everything recorded before the switch
        // addressed a buffer that no longer exists.
        self.cards = cards;
        self.ensure_usable_selection();
        // The mode belongs to the conversation, and a driver holds one mode for its whole process
        // rather than one per conversation — so arriving somewhere has to say which. Without it, a
        // conversation set to `ask` runs its next tool call under the full access of the one you
        // just left.
        self.sync_agent_drivers();
        self.draw_welcome();
        self.replay_round();
        self.set_composer("");
        self.refresh_status();
        self.bridge.broadcast(PluginEvent::SessionChanged {
            session: self.agent.sessions().active_id().clone(),
        });
    }

    /// The settings that say how much of a body to show.
    fn limits(&self) -> cards::Limits {
        cards::Limits {
            diff_lines: self.option_usize("chat.diff_lines"),
            output_lines: self.option_usize("chat.tool_output_lines"),
        }
    }

    /// The directory paths in the transcript are shown relative to: the conversation's.
    fn root(&self) -> std::path::PathBuf {
        let store = self.agent.sessions();
        store.active().cwd.clone()
    }

    /// Draw one tool call's card — the header, with nothing under it yet — and remember where it
    /// went.
    ///
    /// `None` when tool cards are switched off, which is also the answer to "which card do I
    /// finish when it comes back": there is none.
    ///
    /// One function for the live path and the replay, because the two have to agree. They used to
    /// be two pieces of near-identical code, and near-identical is how a card acquires a blank line
    /// in one of them and not the other.
    fn draw_tool_card(&mut self, call: &neosh_proto::ToolCall, _state: ToolState) -> Option<u32> {
        if !self.option_bool("chat.show_tools") {
            return None;
        }
        let leg = Leg {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
            result: None,
            started: Some(std::time::Instant::now()),
            took: None,
            exit: None,
        };
        // Onto the card above, when there is one and it is still the last thing in the transcript.
        // *Still the last thing* is the whole condition: a run is calls with nothing between them,
        // and anything at all having been drawn since — a sentence, a body, another kind of call —
        // means the reader has been told something else and the run is over.
        if let Some(i) = self.open_run()
            && self.cards[i].takes(&call.input)
        {
            self.cards[i].legs.push(leg);
            let row = self.cards[i].row;
            let header = self.card_header(i);
            self.replace_row(row, &header);
            return Some(row);
        }
        let g = self.glyphs();
        let root = self.root();
        let width = self.chat_width();
        let head = cards::Head {
            name: &leg.name,
            input: &leg.input,
            state: ToolState::Running,
            took: None,
            exit: None,
            output: None,
        };
        let card = cards::header(&g, &head, &root, width);
        // No blank row above it. Air used to go between one action and the next on the argument
        // that it turns the transcript into a list — but a card is already a header at column zero
        // with its body indented under a corner, and a run of actions reads as a list without
        // paying a row for every one of them. Prose still gets its blank, because a paragraph and
        // a command are different kinds of thing; two commands are not.
        let row = self.chat_push(vec![card.text.clone()]);
        self.chat_row(row, &card);
        self.cards.push(Card::new(leg, row));
        Some(row)
    }

    /// The card a new call could join: the last one drawn, if nothing has been drawn since.
    fn open_run(&self) -> Option<usize> {
        let i = self.cards.len().checked_sub(1)?;
        let c = &self.cards[i];
        (i64::from(c.row + c.body + 1) == self.chat_end()).then_some(i)
    }

    /// Put one row back where it was, marks and all. Nothing below moves.
    fn replace_row(&mut self, row: u32, r: &cards::Row) {
        let plugin = PluginId::from(BUILTIN);
        // Cleared first: a mark clamps rather than dies when the line under it is replaced, and at
        // equal priority the narrower one wins — so a row rewritten every tick ends up painted by
        // the shortest label it ever had.
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
            lines: vec![r.text.clone()],
        });
        self.chat_row(row, r);
    }

    /// The call came back: settle the dot, put the stats on the header and the body under it.
    ///
    /// Every call gets an answer under it, not just the failures. A card that says what ran and
    /// nothing about what came back is a card you have to take on trust, and "it ran" is not the
    /// thing you wanted to know.
    ///
    /// Errors show even with tool cards off — at the end of the transcript, there being no card
    /// to put them under: a tool that failed silently is a debugging session, and the setting is
    /// about noise, not about hiding failures.
    fn finish_card(&mut self, id: &neosh_proto::ToolCallId, result: &neosh_proto::ToolResult) {
        let found = self
            .cards
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, c)| c.leg_of(id).map(|k| (i, k)));
        let Some((i, k)) = found else {
            if result.is_error || self.option_bool("chat.show_tools") {
                let g = self.glyphs();
                let limits = self.limits();
                let width = self.chat_width();
                let rows = cards::body(&g, &serde_json::Value::Null, result, limits, false, width);
                for r in rows {
                    let row = self.chat_push(vec![r.text.clone()]);
                    self.chat_row(row, &r);
                }
            }
            return;
        };
        // Already answered. A replay can meet the same result twice — once in the messages and
        // once in the round's own record — and a second body would say it happened twice.
        if self.cards[i].legs[k].result.is_some() {
            return;
        }
        let leg = &mut self.cards[i].legs[k];
        leg.took = leg.started.map(|s| s.elapsed());
        leg.exit = result.is_error.then(|| cards::exit_code(&result.content)).flatten();
        leg.result = Some(result.clone());
        self.redraw_card(i);
    }

    /// How one call is going, which is what a card's mark says. Derived rather than stored: a call
    /// with a result is finished and one without is running, and there is no third source of truth
    /// to fall out of step with.
    fn leg_state(leg: &Leg) -> ToolState {
        match &leg.result {
            Some(r) if r.is_error => ToolState::Failed,
            Some(_) => ToolState::Done,
            None => ToolState::Running,
        }
    }

    /// Every call on card `i`, as the renderer wants them.
    fn card_heads(card: &Card) -> Vec<cards::Head<'_>> {
        card.legs
            .iter()
            .map(|leg| cards::Head {
                name: &leg.name,
                input: &leg.input,
                state: Self::leg_state(leg),
                // A finished call keeps whatever it took; a running one is timed from its start,
                // so the number on screen is the one a clock on the wall would give. Whether
                // either is worth printing is the renderer's decision, in one place, for both.
                took: leg.took.or_else(|| leg.started.map(|s| s.elapsed())),
                exit: leg.exit,
                output: leg.result.as_ref().map(|r| lines_in(&r.content)),
            })
            .collect()
    }

    /// Card `i`'s header line, as it should read now.
    fn card_header(&self, i: usize) -> cards::Row {
        let g = self.glyphs();
        let root = self.root();
        let width = self.chat_width();
        let heads = Self::card_heads(&self.cards[i]);
        match heads.as_slice() {
            [one] => cards::header(&g, one, &root, width),
            many => cards::group_header(&g, many, &root, width),
        }
    }

    /// Advance the clock on every call that has not come back.
    ///
    /// One row replaced by one row, so nothing below moves. [`Self::redraw_card`] cannot be used
    /// for this: it settles the open answer, because a body appearing shifts every row after it —
    /// and doing that once a second would end the answer a dozen times while it was still
    /// arriving.
    fn tick_cards(&mut self) {
        let live: Vec<usize> = self
            .cards
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.legs.iter().any(|l| l.result.is_none() && l.started.is_some())
            })
            .map(|(i, _)| i)
            .collect();
        for i in live {
            let card = self.card_header(i);
            let row = self.cards[i].row;
            self.replace_row(row, &card);
        }
    }

    /// Draw card `i` again from what it knows: the header in its final state, the body folded or
    /// open as the card says. Rows below move to make room, and the registry moves with them.
    fn redraw_card(&mut self, i: usize) {
        let g = self.glyphs();
        let width = self.chat_width();
        let limits = self.limits();
        let (row, old_body) = (self.cards[i].row, self.cards[i].body);
        let header = self.card_header(i);
        let root = self.root();
        let card = &self.cards[i];
        let body = match card.legs.as_slice() {
            // One call: whatever it came back with, folded or opened.
            [leg] => match &leg.result {
                Some(r) => cards::body(&g, &leg.input, r, limits, card.expanded, width),
                None => Vec::new(),
            },
            // A run: nothing until it is opened, and then a row per call. What the fold exists to
            // keep out of the transcript is the *contents* of the six files, so opening a run
            // gives back the six names in full and stops there.
            _ => cards::group_body(&g, &Self::card_heads(card), &root, card.expanded, width),
        };
        // Anything else writing into the transcript ends the answer: its rows are addressed by
        // range, and rows moving underneath it would make "the end of the answer" ambiguous. An
        // open answer is always below every card — it was opened after them — so settling it
        // moves nothing recorded here.
        self.close_answer();
        self.streaming = None;
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.chat,
            start: Some(row),
            end: Some(row + 1 + old_body),
        });
        let mut lines = vec![header.text.clone()];
        lines.extend(body.iter().map(|r| r.text.clone()));
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.chat,
            start: row as i64,
            end: (row + 1 + old_body) as i64,
            lines,
        });
        self.chat_row(row, &header);
        for (k, r) in body.iter().enumerate() {
            self.chat_row(row + 1 + k as u32, r);
        }
        // The one redraw that is an arrival rather than a redraw.
        //
        // Every mark on this row was just cleared and re-set, so the band is a mark the frontend
        // has never seen — which is exactly what it keys the one-shot from. Guarded by `flashed`
        // rather than by "is this the first time we settled", because the two are the same fact and
        // only one of them survives a card being opened and folded again.
        if self.cards[i].settled() && !self.cards[i].flashed {
            self.cards[i].flashed = true;
            self.chat_band(row, "Agent.ToolLanded");
        }
        let new_body = body.len() as u32;
        self.cards[i].body = new_body;
        let delta = i64::from(new_body) - i64::from(old_body);
        if delta != 0 {
            for c in self.cards.iter_mut().filter(|c| c.row > row) {
                c.row = (i64::from(c.row) + delta).max(0) as u32;
            }
            // And the trailing question moves with them. A call can come back *after* something
            // was steered in — a driver runs them in parallel — and a body opening above the
            // question shifts it down like every other row below the card. Left unshifted, the
            // turn's closing rows would be spliced into the middle of the question they belong
            // above. Not cleared: a tool result landing above it does not answer it.
            if let Some(u) = self.unanswered.filter(|u| *u > row) {
                self.unanswered = Some((i64::from(u) + delta).max(0) as u32);
            }
        }
    }

    /// The card whose header or body is on `row`.
    fn card_at(&self, row: u32) -> Option<usize> {
        self.cards.iter().position(|c| row >= c.row && row <= c.row + c.body)
    }

    /// Open or fold the card under the cursor, while reading the transcript.
    ///
    /// The cursor goes back to the header either way: it is the one row of the card that is still
    /// where it was, and folding from the middle of a body would otherwise leave the cursor on
    /// whatever moved up into its place.
    fn toggle_card_here(&mut self) {
        let here = self.chat_cursor().0;
        let Some(i) = self.card_at(here) else {
            self.editor_message(MessageLevel::Info, "the cursor is not on a tool call");
            return;
        };
        if !self.cards[i].settled() {
            self.editor_message(MessageLevel::Info, "still running \u{2014} nothing to show yet");
            return;
        }
        self.cards[i].expanded = !self.cards[i].expanded;
        self.redraw_card(i);
        let row = self.cards[i].row;
        self.reading_jump((row, 0));
        self.refresh_composer();
    }

    /// What the turn changed, under its answer.
    ///
    /// From the turn's own tool calls rather than from `git diff`, which would count work done
    /// outside this turn — by you, or by a turn in another conversation on the same checkout.
    fn draw_plan(&mut self, steps: &[neosh_proto::PlanStep], at: Option<u32>) -> u32 {
        if steps.is_empty() || !self.option_bool("chat.show_plan") {
            return 0;
        }
        // Whole. This copy is history — the answer to "what did it decide to do", read back later
        // — and abridging that would leave a transcript with a hole nothing can open.
        let rows = cards::plan(&self.glyphs(), steps, self.chat_width(), None);
        self.place_block(at, &rows)
    }

    /// What the turn changed, under its answer.
    ///
    /// From the turn's own tool calls rather than from `git diff`, which would count work done
    /// outside this turn — by you, or by a turn in another conversation on the same checkout.
    fn draw_summary(&mut self, files: &[cards::FileStat], at: Option<u32>) -> u32 {
        if files.is_empty() {
            return 0;
        }
        let root = self.root();
        let width = self.chat_width();
        let rows = cards::summary(files, &root, width);
        self.place_block(at, &rows)
    }

    /// Say that a question got no reply, when the turn it went into has ended and nothing was
    /// drawn under it.
    ///
    /// `at` is [`Self::unanswered`] as it stands after the turn's closing rows have been placed:
    /// `None` means something *was* said, and there is nothing to explain.
    ///
    /// This is the other half of the bug that put a plan under a message you had just typed.
    /// Moving the plan above it left the question genuinely last — and genuinely bare, which is
    /// the state that reads exactly like a turn still thinking. A message can reach a driver at
    /// the final gap of a turn and be answered by nothing at all: the vendor CLI has already
    /// emitted its result, the round it starts produces no content, and the turn ends. Silence
    /// there is the one outcome the transcript must not render as a question hanging in the air.
    ///
    /// It is a row of the transcript rather than a notification because it is *about* that
    /// question, and a toast that has faded is not an answer to "why is nothing happening".
    fn close_unanswered(&mut self, at: Option<u32>, stop: &neosh_proto::StopReason) {
        use neosh_proto::StopReason as S;
        if at.is_none() {
            return;
        }
        let (text, hl) = match stop {
            // What went wrong, where the question it went wrong on is. A notice says the same
            // thing for a few seconds and then the transcript is back to saying nothing.
            S::Error { message } => (message.clone(), "Diagnostic.Error"),
            S::Refusal { explanation, .. } => (
                explanation.clone().unwrap_or_else(|| "the model declined to answer".into()),
                "Diagnostic.Warn",
            ),
            // You are the one who stopped it, so this is a note rather than a complaint — but it
            // is still worth a row: without one, an interrupted question and a question nobody
            // has got to yet look identical.
            S::Cancelled => ("interrupted".into(), "Agent.Usage"),
            S::MaxTokens => ("the reply hit the output limit".into(), "Diagnostic.Warn"),
            _ => ("no reply \u{2014} the turn ended here".into(), "Diagnostic.Hint"),
        };
        // Through the same placement every other block uses, so it gets one blank line above it
        // and not two — the question already ends with one.
        self.place_block(None, &[cards::Row::plain(text, hl)]);
    }

    /// Put a block of already-rendered rows into the transcript, with their highlights, and answer
    /// with how many rows it took.
    ///
    /// `at` is a row to splice above, or `None` for the end. A row rather than always the end
    /// because a turn's closing rows belong to the answer they close: with a question steered in
    /// after that answer, the end of the buffer is *below* something nobody has answered yet. See
    /// [`Self::unanswered`].
    ///
    /// A blank line above unless there already is one, for the same reason a question gets one: two
    /// blank lines reads as a gap somebody forgot to fill, and none reads as one paragraph.
    fn place_block(&mut self, at: Option<u32>, rows: &[cards::Row]) -> u32 {
        if rows.is_empty() {
            return 0;
        }
        let end = at.map_or_else(|| self.chat_end(), i64::from);
        let gap = end > 0
            && self
                .editor
                .buffer(self.chat)
                .map(|b| {
                    b.get_lines((end - 1) as u32, end as u32).iter().any(|l| !l.trim().is_empty())
                })
                .unwrap_or(false);
        let mut lines: Vec<String> = Vec::new();
        if gap {
            lines.push(String::new());
        }
        lines.extend(rows.iter().map(|r| r.text.clone()));
        let n = lines.len() as u32;
        let first = match at {
            None => self.chat_push(lines),
            Some(row) => self.chat_splice(row, lines),
        };
        let start = first + u32::from(gap);
        for (k, r) in rows.iter().enumerate() {
            self.chat_row(start + k as u32, r);
        }
        n
    }

    /// Insert lines into the middle of the transcript, moving everything below down.
    ///
    /// The registry of card rows moves with them, exactly as it does when a card body opens — a
    /// row index that is not maintained is a row index that eventually addresses somebody else's
    /// text.
    fn chat_splice(&mut self, at: u32, lines: Vec<String>) -> u32 {
        self.close_answer();
        let n = lines.len() as u32;
        if n == 0 {
            return at;
        }
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::BufSetLines {
            buf: self.chat,
            start: i64::from(at),
            end: i64::from(at),
            lines,
        });
        for c in self.cards.iter_mut().filter(|c| c.row >= at) {
            c.row += n;
        }
        if let Some(u) = self.unanswered.filter(|u| *u >= at) {
            self.unanswered = Some(u + n);
        }
        self.streaming = None;
        at
    }

    /// Put back what a turn already said, for a conversation that is still mid-turn.
    ///
    /// The transcript is rebuilt from messages, and what the running turn has produced is not in
    /// them yet. For a model driver that gap is one round; for an agent driver — `claude`, `codex`,
    /// anything speaking ACP — it is the *entire turn*, because such a driver streams its whole
    /// loop down one connection and neosh records none of it until that connection closes. So this
    /// is not a nicety for the last paragraph. Without it, switching away from a running agent turn
    /// and back showed the question and a spinner, and nothing the agent had done in between.
    ///
    /// The working line goes back too, because a turn that is still running should look like one
    /// wherever you are reading it.
    fn replay_round(&mut self) {
        let here = self.active_session();
        if !self.turns.contains_key(&here) {
            return;
        }
        let Some(said) = self.rounds.get_mut(&here).map(|r| std::mem::take(&mut r.said)) else {
            return;
        };
        for item in &said {
            match item {
                Said::Text(text) if text.is_empty() => {}
                // Left open on purpose when it is the last thing: the turn may still be writing
                // this very paragraph, and the next token has to continue it rather than start a
                // second answer underneath.
                Said::Text(text) => self.answer_text(text),
                Said::Tool { call, result } => {
                    // A call the conversation already holds was drawn by `transcript`, which left
                    // it in `cards`. Drawing a second card would say it happened twice — and for a
                    // model driver it always does, because the call lands in the messages the
                    // moment the stream closes and the tool starts running after that.
                    if !self.cards.iter().any(|c| c.leg_of(&call.id).is_some()) {
                        self.draw_tool_card(call, ToolState::Running);
                    }
                    // Whether the card came from the messages or was just drawn, a result the
                    // messages do not hold yet goes under it here. One they do hold was drawn with
                    // it, and `finish_card` leaves an answered card alone.
                    if let Some(r) = result {
                        self.finish_card(&call.id, r);
                    }
                }
            }
        }
        if let Some(r) = self.rounds.get_mut(&here) {
            r.said = said;
        }
        self.begin_working();
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
        // Something is being said under the last question, so it is not the end of the transcript
        // any more. `chat_question` sets this again for the question it draws, which is why this
        // clear is safe to make unconditionally here.
        self.unanswered = None;
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

    /// A blank row at the end of the transcript, unless there already is one.
    fn chat_gap(&mut self) {
        let at = self.chat_end();
        let blank = at <= 0
            || self
                .editor
                .buffer(self.chat)
                .map(|b| b.get_lines((at - 1) as u32, at as u32).iter().all(|l| l.trim().is_empty()))
                .unwrap_or(true);
        if !blank {
            self.chat_push(vec![String::new()]);
        }
    }

    /// Where new content goes: the end, or the line above the working line when there is one.
    fn chat_end(&self) -> i64 {
        let n = self.editor.buffer(self.chat).map(|b| b.line_count() as i64).unwrap_or(0);
        if self.working { (n - i64::from(self.working_rows())).max(0) } else { n }
    }

    /// How many rows the working line and everything under it take, blank line included.
    fn working_rows(&self) -> u32 {
        if !self.working {
            return 0;
        }
        // The gap above, the line itself, and the footer under it.
        u32::from(self.plan_rows > 0) + 1 + self.plan_rows
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
                line_hl_group: None,
                virt_text: vec![],
                virt_text_pos: neosh_proto::VirtTextPos::Eol,
                on_delete: neosh_proto::OnDelete::Clamp,
                priority: 0,
            },
        });
    }

    /// Draw a rebuilt transcript's marks, `by` rows down from where they were computed.
    fn draw_marks(&mut self, marks: &[Mark], by: u32) {
        for m in marks.iter().map(|m| m.shifted(by)) {
            match m.span {
                Some((from, to)) => self.chat_mark(m.row, from, to, m.hl),
                None => self.chat_band(m.row, m.hl),
            }
        }
    }

    /// Put a band behind a whole transcript row, out to whatever edge it is drawn against.
    ///
    /// A point mark rather than a ranged one: what it covers is the row, and giving it an end
    /// column would be saying the band stops somewhere, which is the bug this exists to fix.
    fn chat_band(&mut self, row: u32, hl: &str) {
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::MarkSet {
            ns: self.chat_ns,
            buf: self.chat,
            row,
            col: 0,
            opts: neosh_proto::ExtmarkOpts {
                end_col: None,
                hl_group: None,
                line_hl_group: Some(hl.to_string()),
                virt_text: vec![],
                virt_text_pos: neosh_proto::VirtTextPos::Eol,
                on_delete: neosh_proto::OnDelete::Clamp,
                priority: 0,
            },
        });
    }

    /// Draw one card row at `row`: its spans, and the band behind it.
    fn chat_row(&mut self, row: u32, r: &cards::Row) {
        for (from, to, hl) in &r.spans {
            self.chat_mark(row, *from, *to, hl);
        }
        if let Some(band) = r.band {
            self.chat_band(row, band);
        }
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
            // Raw mode appends to the buffer's *last* line, so the working line has to stop being
            // it. It goes back underneath as soon as the chunk has landed.
            self.below_working(|me| {
                if me.streaming != Some(Stream::Text) {
                    me.open_answer_row();
                    me.streaming = Some(Stream::Text);
                }
                let _ = me
                    .editor
                    .apply(&plugin, ApiCall::BufAppendText { buf: me.chat, text: text.into() });
            });
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

    /// Do something that appends to the last line of the transcript, with the working line taken
    /// out of the way and put back afterwards.
    ///
    /// `BufAppendText` means "the last line" by definition, so anything using it has to own that
    /// line for the duration. Everything else writes at [`Self::chat_end`] and needs none of this.
    fn below_working(&mut self, f: impl FnOnce(&mut Self)) {
        let had = self.working;
        if had {
            self.end_working();
        }
        f(self);
        if had {
            self.begin_working();
        }
    }

    /// Open the row an answer starts on, after a blank line separating it from what came before.
    ///
    /// Above the working line, not at the end of the buffer: the turn is still running, and a
    /// spinner stranded above the answer it is producing says the wrong thing about which of them
    /// is still happening.
    fn open_answer_row(&mut self) -> u32 {
        // An answer is arriving under the question, which is the thing that answers it. Written
        // directly rather than through `chat_push`, so it has to say this for itself.
        self.unanswered = None;
        let n = self.chat_end();
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
    ///
    /// Answers with the first row of the block it drew, which is where the turn's closing rows go
    /// when it turns out nothing was said after it.
    fn chat_question(&mut self, prompt: &Prompt) -> u32 {
        let text = &prompt.text;
        let glyph = self.glyphs().bar;
        let mut rows = Vec::new();
        // A gap above, unless there already is one. Whatever came before was the end of the last
        // answer — often a tool card, which has no trailing blank of its own — and a new question
        // butted straight against it reads as another line of that answer rather than the start of
        // something you said.
        // The line above where this is about to land, which is not the last line of the buffer
        // while a turn is working: the spinner is down there and is not what came before.
        let end = self.chat_end();
        let gap = end > 0
            && self
                .editor
                .buffer(self.chat)
                .map(|b| {
                    b.get_lines((end - 1) as u32, end as u32)
                        .iter()
                        .any(|l| !l.trim().is_empty())
                })
                .unwrap_or(false);
        if gap {
            rows.push(String::new());
        }
        // The pictures first, on rows of their own, because that is the order they are in the
        // message and a question is easier to read when what it is pointing at is above it.
        for b in &prompt.images {
            if let ContentBlock::Image { path, media_type } = b {
                rows.push(format!("{glyph} {}", image_row(path, media_type)));
            }
        }
        for line in text.lines() {
            rows.push(format!("{glyph} {line}"));
        }
        if rows.is_empty() || rows.iter().all(|r| r.trim().is_empty()) {
            rows.push(glyph.to_string());
        }
        rows.push(String::new());
        let body = rows.len() as u32 - 1 - u32::from(gap);
        let at = self.chat_push(rows);
        let first = at + u32::from(gap);
        for i in 0..body {
            self.chat_mark(first + i, 0, glyph.len(), "Agent.User");
        }
        // Nothing has answered it yet, by definition — this is the last thing in the transcript.
        // Whatever is drawn next clears this; if the turn ends and it is still set, the question
        // got no reply. See [`Self::unanswered`].
        self.unanswered = Some(at);
        at
    }

    /// Put the working line at the end of the transcript, for the conversation on screen.
    ///
    /// Says nothing about starting a turn: the turn's own state is the [`Round`], which exists
    /// whether or not you are looking at it. This is only the line.
    ///
    /// Written directly rather than through [`Self::chat_push`], which settles the answer above
    /// it. The working line is not content — it sits *after* an answer that is still being
    /// written, and closing that answer in order to say it is still going would be a strange thing
    /// to do.
    ///
    /// It goes *first* of the block, above the plan and above what is out. All three are state,
    /// but only one of them changes every tick: what it is doing right now is what you are
    /// watching, and putting it under a checklist means it lands on a different row every time the
    /// checklist grows a step. The rows above it are the transcript, which is settled, so the
    /// first row of the block is the one place in the buffer that stays put.
    fn begin_working(&mut self) {
        if self.working {
            return;
        }
        let rows = self.footer();
        let at = self.chat_end();
        // A blank line between what has been said and what is true now, for the same reason a
        // question gets one: butted up against the last tool card, the block reads as part of its
        // output. Only when there is a block — a working line on its own is one row, and one row
        // does not need announcing.
        let gap = !rows.is_empty();
        let mut lines: Vec<String> = Vec::new();
        if gap {
            lines.push(String::new());
        }
        lines.push(String::new());
        lines.extend(rows.iter().map(|r| r.text.clone()));
        let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::BufSetLines {
            buf: self.chat,
            start: at,
            end: at,
            lines,
        });
        self.plan_rows = rows.len() as u32;
        self.working = true;
        let first = at as u32 + u32::from(gap) + 1;
        for (k, r) in rows.iter().enumerate() {
            self.chat_row(first + k as u32, r);
        }
        self.draw_working();
    }

    /// The rows under the working line: the plan, then what is still out.
    ///
    /// A pure function of the round, drawn from scratch every time. There is no incremental path
    /// on purpose — it is a few rows at the very bottom of the buffer, and a diff against the
    /// previous version would be a second place for "where is the footer" to be wrong.
    fn footer(&self) -> Vec<cards::Row> {
        let Some(r) = self.rounds.get(&self.active_session()) else { return Vec::new() };
        let g = self.glyphs();
        let width = self.chat_width();
        let mut rows = if self.option_bool("chat.show_plan") {
            let limit = self.option_usize("chat.plan_rows");
            cards::plan(&g, &r.plan, width, Some(limit))
        } else {
            Vec::new()
        };
        let out: Vec<cards::TaskRow> = r
            .tasks
            .iter()
            .filter(|t| t.listed())
            .map(|t| cards::TaskRow {
                title: &t.title,
                role: t.role.as_deref(),
                since: Some(t.since.elapsed()),
                live: t.status == neosh_proto::TaskStatus::Running,
            })
            .collect();
        if !out.is_empty() {
            rows.extend(cards::tasks(&g, &out, width));
        }
        rows
    }

    /// What is still out when the turn stops waiting for it, as rows to leave behind.
    fn tasks_left_running(&self, session: &neosh_proto::SessionId) -> Vec<cards::Row> {
        let Some(r) = self.rounds.get(session) else { return Vec::new() };
        let left: Vec<cards::TaskRow> = r
            .tasks
            .iter()
            .filter(|t| t.listed())
            .map(|t| cards::TaskRow {
                title: &t.title,
                role: t.role.as_deref(),
                // No clock. It is still running and this row is not: a frozen number beside
                // something that is still going is worse than no number.
                since: None,
                live: false,
            })
            .collect();
        if left.is_empty() {
            return Vec::new();
        }
        cards::tasks(&self.glyphs(), &left, self.chat_width())
    }

    /// Move the clock on anything in the footer that has one.
    ///
    /// Only sub-agents do — a plan step has no duration — so this is skipped entirely when nothing
    /// is out, which is almost always. Unlike [`Self::tick_cards`] the whole footer is laid out
    /// again rather than one row replaced: it is the last few rows of the buffer, and there is
    /// nothing below it to disturb.
    fn tick_footer(&mut self) {
        let ticking = self
            .rounds
            .get(&self.active_session())
            .is_some_and(|r| r.tasks.iter().any(Task::listed));
        if ticking {
            self.redraw_footer();
        }
    }

    /// The plan or the list of what is out has changed, so the footer has to be laid out again.
    ///
    /// Torn down and rebuilt rather than patched, which is what [`Self::below_working`] already
    /// does for anything that writes into the transcript while a turn runs.
    fn redraw_footer(&mut self) {
        if !self.working {
            return;
        }
        self.below_working(|_| {});
    }

    /// Redraw the working line in place.
    ///
    /// Cheap enough to call on every token: it is one `set_lines` over a single row, which the
    /// core coalesces with everything else in the same 16 ms frame. The *movement* costs nothing
    /// here at all — the shimmer is the frontend's, running at its own rate over whatever this
    /// last wrote.
    fn draw_working(&mut self) {
        if !self.working {
            return;
        }
        let here = self.active_session();
        let Some(w) = self.rounds.get(&here) else { return };
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
        match self.agent.steering_count(&here) {
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
        // Cleared first, and this is not tidiness. A mark clamps rather than dies when the line
        // under it is replaced, so this row accumulated one pair per tick — and where two marks
        // overlap at equal priority the *narrower* one wins, so the tail of a shorter label from
        // three seconds ago went on painting over the label being drawn now. What that looked like
        // was the thing this line exists to prevent: `/compact` sat there with its note in the dead
        // grey of finished text, not moving, exactly like a hang.
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.chat,
            start: Some(row),
            end: Some(row + 1),
        });
        // Only the label moves. A clock and a hint sweeping along with it would turn a status line
        // into a light show, and the thing you actually want to read is the number.
        let label_end = glyph.len() + 1 + label.len() + "\u{2026}".len();
        // The sweep, note or no note. `Status.Pending` is for waiting on something outside the
        // program, and a note on this row is never that: it is the turn's own loop saying what it
        // is doing — compacting, running a sub-agent — which is the same "working" the verb means,
        // said more precisely. Swapping the sweep for the slower pulse there took the movement away
        // from the *longest* silence in the program, which is the one that most needs it.
        let hl = "Status.Streaming";
        self.chat_mark(row, 0, label_end, hl);
        self.chat_mark(row, label_end, text.len(), "Agent.Usage");
    }

    /// Take the working line away.
    ///
    /// Removed rather than blanked: it was never content, and a transcript that accumulates one
    /// empty row per turn is a transcript with a growing hole in it.
    fn end_working(&mut self) {
        if !self.working {
            return;
        }
        let row = self.working_row();
        let first = row.saturating_sub(u32::from(self.plan_rows > 0));
        let last = row + self.plan_rows;
        self.working = false;
        self.plan_rows = 0;
        let plugin = PluginId::from(BUILTIN);
        let _ = self.editor.apply(&plugin, ApiCall::MarkClear {
            ns: self.chat_ns,
            buf: self.chat,
            start: Some(first),
            end: Some(last + 1),
        });
        let _ = self.editor.apply(&plugin, ApiCall::BufSetLines {
            buf: self.chat,
            start: first as i64,
            end: last as i64 + 1,
            lines: vec![],
        });
    }

    /// Which row the working line is on, while there is one.
    ///
    /// Counted back from the end rather than remembered, so it is right whatever else wrote to the
    /// buffer: the block is always the last rows of it, and the working line is the first row of
    /// the block that is not the blank above it.
    fn working_row(&self) -> u32 {
        let n = self.editor.buffer(self.chat).map(|b| b.line_count()).unwrap_or(0);
        n.saturating_sub(1 + self.plan_rows)
    }

    fn stream_into_chat(&mut self, kind: Stream, text: &str) {
        self.below_working(|me| me.stream_into_chat_now(kind, text));
    }

    fn stream_into_chat_now(&mut self, kind: Stream, text: &str) {
        let plugin = PluginId::from(BUILTIN);
        // Reasoning under the question is the turn answering it, even when the answer proper has
        // not started. Another direct writer, so it says so for itself.
        self.unanswered = None;
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

    /// Route one thing a turn did.
    ///
    /// Two jobs, and they are deliberately separate. *Bookkeeping* — what a conversation has said
    /// so far, whether its turn is still running, what to persist — happens for every event
    /// whatever is on screen. *Drawing* happens only when the event came from the conversation you
    /// are looking at. That split is what lets several turns run at once without one of them
    /// writing into another's transcript.
    fn handle_agent_event(&mut self, ev: AgentEvent) {
        let on_screen = &self.active_session() == ev.session();
        match ev {
            AgentEvent::TurnStarted { session, turn } => {
                // Write down that a turn is in flight, and clear whatever the last one left. This
                // has to reach the disk *now* rather than at the next commit: the window it covers
                // starts here, and a note written after the first tool round would miss every turn
                // that was killed before it got that far — which is most of a turn's first minute.
                //
                // Starting a turn is also what ends the previous one's mark. The row is red because
                // the last turn here never finished; it stops being the last turn at this moment,
                // and the transcript keeps the evidence either way.
                let was_cut_off = match self.agent.sessions().get_mut(&session) {
                    Some(sess) => {
                        sess.running_since = Some(now_secs());
                        std::mem::take(&mut sess.interrupted)
                    }
                    None => false,
                };
                self.persist_session(&session);
                // The line saying the last turn was killed is now a lie, and the live path cannot
                // take back something it has drawn — so the replay draws it again without. Done
                // here rather than when the message was pushed because everything up to and
                // including the new question is in `messages` by now and nothing of the answer has
                // been drawn yet, which is the one moment a rebuild costs nothing.
                if was_cut_off && on_screen {
                    self.redraw_transcript();
                }
                // Out to anybody watching from another machine, before the local broadcast: the
                // two are independent, and a watcher waiting on a plugin round trip would see the
                // turn start late for no reason.
                if self.watched(&session) {
                    self.stream_out(
                        &session,
                        neosh_proto::StreamEvent::TurnStarted { turn: turn.0.clone() },
                    );
                }
                self.bridge.broadcast(PluginEvent::TurnStarted { session, turn });
            }
            AgentEvent::Token { session, turn, text } => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    match r.said.last_mut() {
                        Some(Said::Text(t)) => t.push_str(&text),
                        _ => r.said.push(Said::Text(text.clone())),
                    }
                }
                if on_screen {
                    self.answer_text(&text);
                }
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::Token {
                        turn: turn.0.clone(),
                        text: text.clone(),
                    });
                }
                self.bridge.broadcast(PluginEvent::Token { session, turn, text });
            }
            AgentEvent::Thinking { session, turn, text } => {
                // Not accumulated, for the same reason `transcript` does not replay it: reasoning
                // is shown live and reprinting it on a switch would bury the answer under the
                // working out.
                if on_screen && self.option_bool("chat.show_thinking") {
                    self.stream_into_chat(Stream::Thinking, &text);
                }
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::Thinking {
                        turn: turn.0.clone(),
                        text: text.clone(),
                    });
                }
                self.bridge.broadcast(PluginEvent::ThinkingToken { session, turn, text });
            }
            AgentEvent::ToolStarted { session, turn, call } => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    r.note = Some(format!("Running {}", call.name));
                    r.said.push(Said::Tool { call: call.clone(), result: None });
                }
                if on_screen {
                    self.draw_tool_card(&call, ToolState::Running);
                    // The tool is what the turn is doing now, so the working line should say so
                    // rather than keep claiming the model is thinking.
                    self.draw_working();
                }
                self.bridge.broadcast(PluginEvent::ToolStarted { session, turn, call });
            }
            AgentEvent::ToolFinished { session, turn, call, result } => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    r.note = None;
                    // The call this answers, so a replay of the round draws the pair rather than a
                    // dot that is still pulsing over a tool that came back minutes ago.
                    for said in r.said.iter_mut().rev() {
                        if let Said::Tool { call: c, result: slot } = said
                            && c.id == call.id
                        {
                            *slot = Some(result.clone());
                            break;
                        }
                    }
                }
                // Counted whether or not anyone is looking: the summary is drawn when the turn
                // ends, wherever you are by then.
                if !result.is_error
                    && let Some(r) = self.rounds.get_mut(&session)
                {
                    cards::tally(&mut r.changes, &cards::edits_of(&call.input));
                }
                if on_screen {
                    self.finish_card(&call.id, &result);
                    self.draw_working();
                }
                self.bridge.broadcast(PluginEvent::ToolFinished { session, turn, call, result });
            }
            AgentEvent::TurnEnded { session, turn, stop_reason, usage } => {
                if on_screen {
                    self.close_answer();
                    // Read before the footer goes away with the working line, and drawn again
                    // after: the footer is state, and this is the copy that stays. What the turn
                    // set out to do and how much of it it got through is the one thing a finished
                    // turn leaves behind that its answer usually does not say.
                    let plan =
                        self.rounds.get(&session).map(|r| r.plan.clone()).unwrap_or_default();
                    // Anything the turn is walking away from. A backgrounded sub-agent is still
                    // running when the answer arrives, and the footer it was listed in goes away
                    // with the working line — so without this the one case people actually ask
                    // about, "is something still going?", is the one that leaves no trace.
                    let left = self.tasks_left_running(&session);
                    let changes =
                        self.rounds.get(&session).map(|r| r.changes.clone()).unwrap_or_default();
                    // Where the turn's own closing rows belong. `None` is the end of the buffer,
                    // which is right whenever the last thing there is something the turn said.
                    // When it is a question steered in after that — taken in at the final gap, and
                    // answered by nothing — the end of the buffer is *below* it, and a plan and a
                    // diff summary appended there put two blocks of the previous exchange under a
                    // message you had just typed. Taken rather than read, because these rows are
                    // what stops it being the last thing in the transcript.
                    let mut at = self.unanswered.take();
                    // The working block sits below all of this, so taking it away first moves
                    // none of the rows named here.
                    self.end_working();
                    self.streaming = None;
                    // One after another, each below the last: every block that lands pushes the
                    // question — and so the row the next one goes above — further down.
                    let n = self.draw_plan(&plan, at);
                    at = at.map(|row| row + n);
                    let n = self.place_block(at, &left);
                    at = at.map(|row| row + n);
                    let n = self.draw_summary(&changes, at);
                    at = at.map(|row| row + n);
                    self.close_unanswered(at, &stop_reason);
                }
                // Queued after the last gap this turn had. It is a question that was asked and not
                // yet answered, so it becomes the next turn rather than being quietly dropped.
                let leftover = self.agent.take_steering(&session);
                self.turns.remove(&session);
                self.rounds.remove(&session);
                if let Some(s) = self.agent.sessions().get_mut(&session) {
                    s.updated_at = now_secs();
                    // The turn ended, so there is nothing for a later process to notice. Cleared
                    // here for *every* ending — finished, errored, interrupted — because the note
                    // means "no one ended this", and all three of those are someone ending it.
                    s.running_since = None;
                    // News, and only where you were not looking. A turn that ended in the
                    // conversation on screen has been seen by definition — including with nothing
                    // attached, because reattaching lands you in that conversation with the answer
                    // already in it. Anywhere else, the row is the only thing that will ever
                    // mention it, so it has to say so until you go there. See ADR 0042.
                    s.unread = !on_screen;
                }
                self.persist_sessions();
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::TurnEnded {
                        turn: turn.0.clone(),
                        stop_reason: stop_reason.clone(),
                        usage: usage.clone(),
                    });
                }
                self.bridge.broadcast(PluginEvent::TurnEnded {
                    session: session.clone(),
                    turn,
                    stop_reason,
                    usage,
                });
                // The one moment these numbers definitely moved. Asked for rather than done here:
                // the vendor's own accounting lags the end of the stream by a second or two, and a
                // poll at zero reliably returns the figure from *before* the turn — which draws as
                // a turn that cost nothing at all.
                self.quota_due = true;
                if !leftover.is_empty() {
                    self.start_turn_in(session, Prompt {
                        text: leftover
                            .iter()
                            .map(|p| p.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n"),
                        images: leftover.into_iter().flat_map(|p| p.images).collect(),
                    });
                }
                // The field says what `\u{23ce}` does, and what it does just changed back.
                self.refresh_composer();
            }
            // What the turn produced is in the conversation now, so the round's copy of it would
            // only go stale — and drawing both on a switch would say everything twice.
            //
            // And it is the moment to write it down. A turn is minutes of work and dozens of tool
            // rounds, and until this the only saves were at either end of it: the conversation on
            // disk held the message you typed and nothing after it, for as long as the turn ran.
            // Anything that took the workspace out without warning — a crash, an OOM kill, a
            // reboot — lost the whole answer and left the transcript ending on your own question.
            // A round boundary is what the agent itself resumes from, so it is the right grain to
            // save at, and one conversation is written rather than every conversation in the
            // workspace: `persist_sessions` several times a minute for the sake of the one that
            // changed is a hundred files rewritten to record one.
            AgentEvent::Committed { session, .. } => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    r.said.clear();
                }
                self.persist_session(&session);
            }
            AgentEvent::Steered { prompt, .. } => {
                // Drawn now rather than when it was typed: until the model has been told, a
                // transcript showing the question is a transcript that is lying about what was
                // asked. Off screen it needs no drawing at all — it is in the conversation's
                // messages, which is what a switch back rebuilds from.
                if on_screen {
                    self.chat_question(&prompt);
                    self.draw_working();
                    self.refresh_composer();
                    // Where it landed is where you should be. It was drawn above the working
                    // line, which is off the bottom of a transcript you had scrolled back in.
                    self.scroll_chat(0);
                }
            }
            AgentEvent::Activity { session, turn, activity } => {
                // A driver's account of its own loop, carried separately from `Token` here for the
                // same reason it is locally: it is not a content block. See ADR 0034.
                if self.watched(&session) {
                    self.stream_out(&session, neosh_proto::StreamEvent::Activity {
                        turn: turn.0.clone(),
                        activity: activity.clone(),
                    });
                }
                self.handle_activity(session, turn, activity, on_screen);
            }
            AgentEvent::Notice { level, text, .. } => self.editor_message(level, text),
        }
    }

    /// Route one thing the driver's own loop said about itself.
    ///
    /// Split from [`Self::handle_agent_event`] for the same reason it exists at all: none of this
    /// is part of the message being produced, and folding it in would mean every arm of that match
    /// having to decide whether it was looking at content or at commentary.
    ///
    /// The same bookkeeping-versus-drawing split applies. A conversation off screen still learns
    /// which sub-agents it has out and what its plan is, because switching to it should show what
    /// is true — not what was true when you last looked.
    fn handle_activity(
        &mut self,
        session: neosh_proto::SessionId,
        turn: neosh_proto::TurnId,
        activity: neosh_proto::Activity,
        on_screen: bool,
    ) {
        use neosh_proto::{Activity, TaskStatus};
        // Out to plugins before it is drawn, and whatever it is. A meter, a plan view and a status
        // line all want this and none of them caused it — and it is the only thing that moves
        // *during* a turn, which for an agent driver can be twenty minutes long.
        self.bridge.broadcast(PluginEvent::Activity {
            session: session.clone(),
            turn,
            activity: activity.clone(),
        });
        match activity {
            Activity::Plan { steps } => {
                let Some(r) = self.rounds.get_mut(&session) else { return };
                if r.plan == steps {
                    return;
                }
                r.plan = steps;
                if on_screen {
                    self.redraw_footer();
                }
            }
            // Announced twice on purpose by the driver — once as a snapshot of what is running,
            // once when this turn's own call starts one — so this is written to be idempotent.
            // The second telling knows more (which call it belongs to, which kind of agent it is)
            // and is allowed to fill in what the first could not.
            Activity::TaskStarted { task, title, role, .. } => {
                let Some(r) = self.rounds.get_mut(&session) else { return };
                match r.tasks.iter_mut().find(|t| t.id == task) {
                    Some(t) => {
                        t.title = title;
                        if role.is_some() {
                            t.role = role;
                        }
                    }
                    None => r.tasks.push(Task {
                        id: task,
                        title,
                        role,
                        since: std::time::Instant::now(),
                        status: TaskStatus::Running,
                    }),
                }
                if on_screen {
                    self.redraw_footer();
                }
            }
            // What it is up to, in the line that is already on screen for it. Nothing is inserted:
            // a sub-agent that reports every few seconds would otherwise push the answer it is
            // helping to write off the top of the screen.
            Activity::TaskProgress { task, summary, last_tool, .. } => {
                let Some(r) = self.rounds.get_mut(&session) else { return };
                let Some(t) = r.tasks.iter_mut().find(|t| t.id == task) else { return };
                if let Some(text) = summary.or(last_tool) {
                    t.title = text;
                }
                if on_screen {
                    self.redraw_footer();
                }
            }
            Activity::TaskEnded { task, status, .. } => {
                let Some(r) = self.rounds.get_mut(&session) else { return };
                let Some(t) = r.tasks.iter_mut().find(|t| t.id == task) else { return };
                if t.status == status {
                    return;
                }
                t.status = status;
                if on_screen {
                    self.redraw_footer();
                }
            }
            // The several seconds where the driver answers nothing at all. Without this it is
            // indistinguishable from a hang, which is exactly what `/compact` looked like.
            Activity::Compacting => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    r.note = Some("Compacting the conversation".into());
                }
                if on_screen {
                    self.draw_working();
                }
            }
            Activity::Compacted { before, after } => {
                if let Some(r) = self.rounds.get_mut(&session) {
                    r.note = None;
                    // The plan was built from calls the driver can no longer see. Keeping it would
                    // be a checklist for work nothing is going to pick up.
                    r.plan.clear();
                }
                // What the window holds *now*. Compaction is the one moment the number moves
                // without a request being made, and the card said `20.3k → 1.6k` while the meter
                // under it went on reporting the number from before — for the rest of the
                // conversation, because a driver that has answered "how full is it" once turns the
                // estimate in `add_usage` off, and the next answer only arrives with the next
                // request. The driver said `post_tokens`; this is what it is for.
                if let Some(used) = after {
                    let window = self
                        .agent
                        .sessions()
                        .get(&session)
                        .and_then(|s| s.context_window)
                        .unwrap_or(0);
                    if let Some(s) = self.agent.sessions().get_mut(&session) {
                        s.set_context(used, window);
                    }
                    self.persist_sessions();
                }
                if on_screen {
                    let r = cards::compaction(&self.glyphs(), before, after);
                    let row = self.chat_push(vec![r.text.clone()]);
                    self.chat_row(row, &r);
                    self.redraw_footer();
                    self.draw_working();
                }
            }
            Activity::Commands { commands } => {
                self.remember_commands(&session, &commands);
                self.driver_commands.insert(session, commands);
            }
            // What the driver calls this conversation, written down where it will still be after
            // the process that said it has gone. Persisted rather than held: the value is only
            // worth anything on a run that is not this one.
            Activity::Resume { token } => {
                let changed = match self.agent.sessions().get_mut(&session) {
                    Some(s) if s.resume.as_deref() != Some(token.as_str()) => {
                        s.resume = Some(token);
                        true
                    }
                    _ => false,
                };
                if changed {
                    self.persist_sessions();
                }
            }
            // The driver's own answer to "how full is it", which beats anything derived. A prompt
            // size counts one request; this counts the conversation the agent is holding, and it
            // brings the window with it — a vendor CLI's window is whatever that CLI decided, not
            // whatever a catalogue says the model has.
            Activity::Context { used, total } => {
                if let Some(s) = self.agent.sessions().get_mut(&session) {
                    s.set_context(used, total);
                }
                self.persist_sessions();
            }
            // Attributed to the *conversation's* instance, not to whatever the picker is showing.
            // A workspace runs several conversations at once and they need not be on the same
            // provider, so reading the active selection here would file `codex`'s weekly figure
            // under the account you happen to have open.
            Activity::Quota { plan, windows, credits } => {
                let Some(instance) = self.instance_of(&session) else { return };
                self.take_quota(
                    instance,
                    neosh_proto::QuotaSource::Turn,
                    neosh_provider::quota::Quota { plan, windows, credits },
                );
            }
        }
    }

    /// Which configured instance a conversation is talking to.
    ///
    /// The conversation's own selection first, and the workspace's only as the fallback for one
    /// that has not chosen. Getting this the other way round attributes every driver's report to
    /// whichever row the model picker was last left on.
    fn instance_of(
        &self,
        session: &neosh_proto::SessionId,
    ) -> Option<neosh_proto::InstanceId> {
        let chosen = {
            let store = self.agent.sessions();
            store.get(session).and_then(|s| s.selection.as_ref().map(|sel| sel.instance.clone()))
        };
        chosen.or_else(|| self.agent.selection().map(|sel| sel.instance))
    }

    /// Put a snapshot in the store, and tell everyone if it changed anything.
    ///
    /// One door, whatever the source: a plugin's provider, a mid-turn line and a poll all arrive
    /// here. That is what makes a third-party provider's plan appear in the sidebar strip without
    /// the sidebar having heard of that provider — the strip listens for the event, not the vendor.
    fn take_quota(
        &mut self,
        instance: neosh_proto::InstanceId,
        source: neosh_proto::QuotaSource,
        quota: neosh_provider::quota::Quota,
    ) {
        let Some(snapshot) = self.quota.apply(instance, source, quota, now_secs()) else { return };
        self.bridge.broadcast(PluginEvent::Quota { snapshot });
    }

    /// Whether a plugin registered the driver serving this instance.
    ///
    /// The gate on [`ApiCall::QuotaReport`]. Without it any plugin could publish any number for
    /// `claude-cli`, and this strip is a thing people are going to look at before deciding whether
    /// to spend an afternoon on something — a figure it is possible to spoof is a figure that
    /// should not be drawn as fact.
    fn plugin_drivers_own(
        &self,
        plugin: &PluginId,
        instance: &neosh_proto::InstanceId,
    ) -> bool {
        let Some(drivers) = self.plugin_drivers.get(plugin) else { return false };
        self.agent
            .providers()
            .instances()
            .any(|cfg| &cfg.id == instance && drivers.contains(&cfg.driver))
    }

    /// Every instance this machine could actually ask, right now.
    ///
    /// Built from the configured instances rather than from a written list of vendors, so an
    /// instance the user named something else — a second `claude-cli` pointed at another account —
    /// is polled too. Whether it *can* answer is decided in the poll itself, which is where the
    /// filesystem and `PATH` checks belong.
    fn quota_targets(&self) -> Vec<crate::quota::Target> {
        self.agent
            .providers()
            .usable_instances()
            .filter_map(|cfg| {
                let which = neosh_provider::quota::pollable(cfg.driver.0.as_str())?;
                // The program to run is the one the instance says it authenticates with, which is
                // also the one that owns the login being read. A default here would poll the
                // `codex` on `PATH` for an instance pointed at a different build of it.
                let program = match &cfg.auth {
                    neosh_proto::AuthRef::Cli { program, .. } => program.clone(),
                    _ => return None,
                };
                Some(crate::quota::Target { instance: cfg.id.clone(), which, program })
            })
            .collect()
    }

    /// Ask, off the host loop.
    ///
    /// Nothing is awaited here. A poll is a network round trip and a process spawn, and the host
    /// loop is the single writer for the editor — awaiting one inside it freezes typing for as long
    /// as somebody else's server takes to answer.
    fn refresh_quota(&self, only: Option<neosh_proto::InstanceId>) {
        // A poll reads the vendor CLI's own login to ask about the vendor's own account. That is
        // the same trade the CLI drivers already make — neosh is useful the minute it is installed
        // because it reuses a login you have — but it is somebody else's credential store, so it is
        // a setting rather than a decision made on their behalf. Off, the gauges show what the last
        // turn reported and say how long ago that was.
        if self.editor.options().bool("usage.poll") == Some(false) {
            return;
        }
        let mut targets = self.quota_targets();
        if let Some(id) = only {
            targets.retain(|t| t.instance == id);
        }
        if targets.is_empty() {
            return;
        }
        let tx = self.quota_tx.clone();
        tokio::spawn(crate::quota::poll(targets, tx));
    }

    async fn handle_script_out(&mut self, out: ScriptOutbound) {
        match out {
            ScriptOutbound::Loaded { plugin, error } => {
                match error {
                    None => tracing::info!(%plugin, "plugin loaded"),
                    Some(e) => {
                        tracing::error!(%plugin, "failed to activate: {e}");
                        self.editor_message(MessageLevel::Error, format!("{plugin}: {e}"));
                        // It threw *during* activation, which means it may already have armed
                        // timers and registered commands, tools and hooks — and is on the
                        // broadcast list, having been put there when its load was asked for.
                        // Nothing else will ever clean those up: reload skips a plugin that is not
                        // loaded, so every reload would stack another copy.
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
                    // Answered when the owning machine answers, not now. Every ASCP command is
                    // replied to exactly once, so this settles — unless the peer has gone, which
                    // the swarm reports as a disconnect and which settles it too.
                    // Dials an address that may not answer, so it cannot run on the loop.
                    if let ApiCall::SwarmProbe { addr } = &call {
                        self.begin_swarm_probe(addr.clone(), (plugin.clone(), id.clone()));
                        return;
                    }
                    if let ApiCall::SwarmCommand { node, session, command } = &call {
                        self.begin_swarm_command(
                            node.clone(),
                            session.clone(),
                            command.clone(),
                            (plugin.clone(), id.clone()),
                        );
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
        // A search being typed takes the keyboard the same way, and for a milder version of the
        // same reason: `n` is a letter in most words and it is also the key for "next match".
        if self.search.is_some() {
            match input {
                InputEvent::Key { key } => self.search_key(key),
                InputEvent::Paste { text } => {
                    if let Some(p) = self.search.as_mut() {
                        p.query.push_str(text.trim());
                    }
                    self.render_search();
                    let (query, back, from) = {
                        let p = self.search.as_ref().expect("checked above");
                        (p.query.clone(), p.back, p.from)
                    };
                    self.highlight_matches(&query);
                    if let Some(at) = self.find_from(&query, from, !back, true) {
                        self.reading_jump(at);
                    }
                }
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
                // Unless it is a picture. Dragging a file onto a terminal window pastes its path,
                // which is the only gesture a terminal has for "this file" — so a paste that is
                // one line, is a path, exists, and turns out to hold an image becomes an
                // attachment instead of a line of text nobody wanted typed out. Everything that
                // is not all four of those is text, because swallowing something somebody meant
                // to type is much worse than making them press a key.
                if let Some(path) = crate::images::pasted_path(&text) {
                    match self.attach(Some(&path)) {
                        // Nothing said about it. The chip that just appeared above the field *is*
                        // the report, and a line saying the same words in the same glance is the
                        // kind of noise that teaches people not to read either one.
                        Ok(_) => return true,
                        // It looked like an image and was not one. Fall through: what was pasted
                        // is a path, and a path is perfectly good text.
                        Err(e) => tracing::debug!("not attaching {}: {e}", path.display()),
                    }
                }
                self.compose(TextEdit::Insert { text });
            }
            InputEvent::ViewportChanged { win, width, height, top_line, rows } => {
                let was = self.chat_width();
                self.editor.set_viewport(win, neosh_core::Viewport { width, height, top_line, rows });
                // What the frontend *drew*, which is not always what was asked for: a window whose
                // scroll had to bend to keep its cursor on screen is showing a different first row.
                // Taking it back means the next page starts from where the screen is rather than
                // from where the core last guessed, which with wrapped rows are different places.
                if win == self.chat_win && self.chat_top.is_some() {
                    self.chat_top = Some(top_line);
                }
                // This is where the host *learns* a width — `Resize` is the terminal's outer size
                // and the transcript is some part of it. So anything drawn to a width has to be
                // drawn again here, or a narrow terminal joining a wide workspace keeps the wide
                // one's welcome until something unrelated happens to redraw it.
                if win == self.chat_win && self.chat_width() != was {
                    self.draw_welcome();
                }
            }
            InputEvent::Attached { .. } => {
                // A client that has never seen any of this. Whatever was queued was queued for
                // whoever was looking before — and nobody may have been — so it is dropped rather
                // than sent to a mirror it makes no sense against, and the whole state is said
                // again in its place. See `Editor::republish`.
                let _ = self.editor.drain_ui();
                self.editor.republish();
                self.draw_welcome();
                // Every plugin holding a surface has to paint it again: the editor forwards cells
                // and does not keep them, so a claim is all that could be republished.
                self.bridge.broadcast(neosh_proto::PluginEvent::ViewAttached);
                // Certainly changed, and not subject to the once-a-second rule: "is anyone
                // watching" is the one field of the report a viewer arriving is entirely about.
                self.report_live_now();
            }
            InputEvent::Ready { .. } | InputEvent::Resize { .. } => {
                // A resize changes nothing the core owns; the frontend re-lays-out from the
                // declarative geometry it already has. Redraw so it takes effect.
                let _ = self.editor.apply(&PluginId::from(BUILTIN), ApiCall::WinScrollTo {
                    win: self.composer_win,
                    top_line: Some(0),
                });
                // Except the welcome, which is drawn to a width: the word at the top of it has to
                // fit or not be there.
                self.draw_welcome();
            }
            InputEvent::Command { name, args } => {
                // Not yet registered, and something is still loading: held rather than refused.
                // See `Startup::commands`.
                if self.startup_in_flight() && !self.editor.has_command(&name) {
                    self.startup.commands.push((name, args));
                    return true;
                }
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
        // Anything worth drawing is something that might have changed what this workspace is
        // holding — a turn started, a conversation opened, an answer landed.
        self.report_live();
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
        mut input_rx: mpsc::UnboundedReceiver<(crate::views::ViewId, InputEvent)>,
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

        // Taken out of the host so the loop can own it. `None` when `[swarm]` names nobody, in
        // which case `recv` on it is never ready and this costs a branch that is never taken.
        let mut swarm_rx = self.swarm_events.take();

        // What a plan has left. A fourth deadline rather than a share of any of the others, for the
        // same reason they are all separate: it answers a different question, on a scale of minutes
        // rather than frames, and the answer is a network round trip. Armed immediately so the
        // first screen has real numbers on it — a gauge that only appears after five minutes of
        // sitting there is one nobody knows exists.
        let mut quota_rx = self.quota_rx.take();
        let plan = tokio::time::sleep(crate::quota::AFTER_TURN);
        tokio::pin!(plan);
        // How many times in a row a poll has come back with nothing. Doubles the wait each time.
        let mut quota_backoff: u32 = 0;

        loop {
            tokio::select! {
                Some(out) = script_rx.recv() => self.handle_script_out(out).await,
                Some(ev) = async {
                    match swarm_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        // Never ready, rather than ready-with-nothing: the latter would spin.
                        None => std::future::pending().await,
                    }
                } => self.on_swarm_event(ev),
                Some(ev) = agent_rx.recv() => self.handle_agent_event(ev),
                Some((view, input)) = input_rx.recv() => {
                    // A repaint changes nothing, so it must not go through the input path: the
                    // frontend is asking to draw the same state again while something on it moves.
                    // Answering with an empty batch is what makes an animation unable to
                    // desynchronise from the text it is drawn over.
                    if matches!(input, InputEvent::Repaint) {
                        self.frontend.send(vec![UiEvent::Flush]).await?;
                        continue;
                    }
                    // Set for the whole of handling, so anything this key reaches — a command, a
                    // keymap, a plugin — can be answered in the terminal it was pressed in.
                    self.from_view = view;
                    let carry_on = self.handle_input(input);
                    self.from_view = crate::views::ViewId::LOCAL;
                    if !carry_on {
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
                    self.tick_cards();
                    self.tick_footer();
                    self.report_live();
                }
                () = &mut keys, if waiting => {
                    waiting = false;
                    self.keys_pending = false;
                    if self.editor.flush_pending() {
                        self.drain_effects();
                    }
                }
                Some(polled) = async {
                    match quota_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        // Never ready, rather than ready-with-nothing: the latter would spin.
                        None => std::future::pending().await,
                    }
                } => {
                    match polled.quota {
                        Some(quota) => {
                            quota_backoff = 0;
                            self.take_quota(
                                polled.instance,
                                neosh_proto::QuotaSource::Poll,
                                quota,
                            );
                        }
                        // Asked and not answered. Slow down, and slow down hard when the vendor was
                        // explicit about it: the endpoint that reports rate limits has one of its
                        // own, and a poller that keeps its cadence through a 429 is a gauge added
                        // to avoid hitting limits that is itself hitting one.
                        None => {
                            quota_backoff = if polled.throttled {
                                (quota_backoff + 1).max(3)
                            } else {
                                quota_backoff + 1
                            };
                            let wait = crate::quota::IDLE_INTERVAL
                                .saturating_mul(1u32 << quota_backoff.min(6))
                                .min(crate::quota::MAX_BACKOFF);
                            plan.as_mut().reset(Instant::now() + wait);
                        }
                    }
                }
                () = &mut plan => {
                    self.refresh_quota(None);
                    // Reset unconditionally, and to the idle interval whichever branch armed it.
                    // A turn ending shortens the *next* wait only; leaving it short afterwards
                    // would poll every five seconds forever after the first turn.
                    self.quota_due = false;
                    plan.as_mut().reset(Instant::now() + crate::quota::IDLE_INTERVAL);
                }
                else => break,
            }

            if self.quitting {
                break;
            }
            // The viewer asked to leave. The workspace does not: this is the whole difference
            // between closing a window and stopping the work in it.
            if let Some(view) = self.detaching.take() {
                self.persist_sessions();
                self.flush().await?;
                self.frontend.detach(view).await?;
                self.report_live_now();
            }
            if std::mem::take(&mut self.keys_touched) {
                waiting = self.keys_pending;
                if waiting {
                    let ms = self.editor.options().int("timeoutlen").unwrap_or(500).max(0) as u64;
                    keys.as_mut().reset(Instant::now() + Duration::from_millis(ms));
                }
            }
            if self.working && !ticking {
                clock.as_mut().reset(Instant::now() + Duration::from_secs(1));
                ticking = true;
            }
            // A turn is the only thing that moves these numbers by an amount worth redrawing for,
            // so the poll that matters is the one just after one ends.
            if std::mem::take(&mut self.quota_due) {
                plan.as_mut().reset(Instant::now() + crate::quota::AFTER_TURN);
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
        self.vars = crate::vars::Vars::new(Some(dir.clone()));
        // The sampled percentages, which are the only record anywhere of what a plan looked like an
        // hour ago. Loaded here rather than at construction because this is where the workspace
        // finds out it has a state directory at all — under `--clean` it never does, and the
        // history is then in memory for as long as the process lives and nowhere afterwards.
        self.quota = crate::quota::QuotaStore::new(Some(&dir));

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
            match target.filter(|id| store.switch(id).is_ok()) {
                Some(_) => {
                    let _ = store.remove(&placeholder);
                }
                // Nothing to land in, so what you land in is not a conversation either: the
                // session the store was constructed with becomes the placeholder, which is what
                // makes "everything is archived" look like an empty workspace rather than like a
                // workspace with one conversation in it that you never started.
                None => {
                    if let Some(s) = store.get_mut(&placeholder) {
                        s.ephemeral = true;
                    }
                }
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
        // And the one that is active decides where "here" is, which is not necessarily where neosh
        // was started: reopening the last conversation you were in should put you back in *its*
        // project, not in whichever directory the shell happened to be sitting in.
        let here = { self.agent.sessions().active().cwd.clone() };
        if here.is_dir() {
            self.work_in(here).await;
        }
        self.enter_session();
        tracing::info!("restored {restored} conversation(s)");
    }

    /// Persist every conversation. Cheap enough to call after each turn; a no-op under `--clean`.
    /// Where the last-known command list for a conversation's driver and project is filed.
    ///
    /// By instance and directory rather than by conversation, because that is what the answer
    /// actually depends on: `claude` in one repository accepts that repository's project commands
    /// and its MCP prompts, and every conversation in it gets the same list. Keyed by conversation
    /// it would be learned again from scratch for each new one, which is precisely the case this
    /// exists for.
    fn commands_key(&self, session: &neosh_proto::SessionId) -> Option<String> {
        let store = self.agent.sessions();
        let s = store.get(session)?;
        let instance = s
            .selection
            .as_ref()
            .map(|sel| sel.instance.clone())
            .or_else(|| self.agent.selection().map(|sel| sel.instance))?;
        Some(format!("commands:{instance}:{}", s.cwd.display()))
    }

    /// Keep what a driver said it accepts, for the conversations that have not asked it yet.
    fn remember_commands(
        &mut self,
        session: &neosh_proto::SessionId,
        commands: &[neosh_proto::DriverCommand],
    ) {
        // An empty list is a driver that answered nothing, not a driver that accepts nothing.
        // Writing it down would replace a good list with a blank one.
        if commands.is_empty() {
            return;
        }
        let Some(key) = self.commands_key(session) else { return };
        let Ok(value) = serde_json::to_value(commands) else { return };
        if self.pstate.get(BUILTIN, &key) == value {
            return;
        }
        self.pstate.set(BUILTIN, key, value);
    }

    /// What this driver accepted last time it ran here, for a conversation that has not run yet.
    fn remembered_commands(&mut self) -> Vec<neosh_proto::DriverCommand> {
        let here = self.active_session();
        let Some(key) = self.commands_key(&here) else { return Vec::new() };
        serde_json::from_value(self.pstate.get(BUILTIN, &key)).unwrap_or_default()
    }

    /// Write one conversation, now.
    ///
    /// The cheap half of [`Self::persist_sessions`], for the places that fire *inside* a turn.
    /// Ordering is not at stake — `order.json` records which conversations exist and which one you
    /// were in, and neither changes because a tool round finished.
    ///
    /// A [placeholder](neosh_agent::Session::ephemeral) is skipped, the same as in `save_all`: no
    /// caller can reach here with one today, because saying anything is what stops a session being
    /// one and a turn cannot start before that — but the rule is about what may be written down,
    /// not about who happens to be calling.
    fn persist_session(&self, id: &neosh_proto::SessionId) {
        let Some(dir) = &self.state_dir else { return };
        let store = self.agent.sessions();
        let Some(s) = store.get(id).filter(|s| !s.ephemeral) else { return };
        if let Err(e) = crate::sessions::save(dir, s) {
            tracing::warn!("could not save conversation {}: {e}", id.0);
        }
    }

    fn persist_sessions(&self) {
        let Some(dir) = &self.state_dir else { return };
        if let Err(e) = crate::sessions::save_all(dir, &self.agent.sessions()) {
            // Reported once, not per turn: a full disk should not turn into a message on every
            // keystroke, and losing history is not a reason to stop working.
            tracing::warn!("could not save conversations: {e}");
        }
    }

    pub async fn discover_repo(&mut self, cwd: &std::path::Path) {
        self.work_in(cwd.to_path_buf()).await;
    }

    /// Look a directory up once and remember the answer, including "not a repository".
    ///
    /// The negative is cached too: a plain directory would otherwise cost a `git rev-parse` on
    /// every status refresh forever.
    /// Point everything that means "here" at a directory.
    ///
    /// Called on every switch and on every new conversation, because *here* is a property of the
    /// conversation. Three things follow it and each was pinned to the launch directory before:
    /// the git repository plugins ask about, the root the agent's file tools resolve against, and
    /// the directory the welcome banner names.
    ///
    /// The vendor CLI drivers are the fourth, and they are told per turn rather than held here —
    /// see [`neosh_proto::TurnRequest::cwd`].
    async fn work_in(&mut self, dir: std::path::PathBuf) {
        self.cwd = dir.clone();
        let mut layer = (*self.agent.permissions()).clone();
        layer.set_workspace(dir.clone());
        self.agent.set_permissions(layer);
        self.remember_repo(dir).await;
    }

    async fn remember_repo(&mut self, dir: std::path::PathBuf) {
        if self.repos.contains_key(&dir) {
            return;
        }
        let found = neosh_vcs::Git::discover(&dir).await;
        if let Some(g) = &found {
            tracing::info!(dir = %dir.display(), root = %g.root().display(), "git repository");
            // Asked once, when the directory is first seen, and remembered. A label is read on
            // every redraw of the panel and is not worth a `rev-parse` each time.
            let (branch, main) = g.identity().await;
            self.projects.insert(
                dir.clone(),
                ProjectFacts {
                    name: project_name(&dir, branch.clone(), main.clone()),
                    repo_root: main.map(|p| p.display().to_string()),
                    branch,
                },
            );
            // Asked here for the same reason and at the same time as the name: once per directory,
            // and the answer does not change while a checkout is open.
            if let Some(key) = g.origin_url().await.and_then(|u| neosh_proto::ProjectKey::from_remote(&u)) {
                self.project_keys.insert(dir.clone(), key);
            }
        }
        self.repos.insert(dir, found);
    }

    /// Stamp the display name onto a conversation's info.
    ///
    /// Done here rather than in the session store because naming a checkout means having looked at
    /// the repository, which is the host's job — the store holds conversations and knows nothing
    /// about git.
    fn named(&self, mut info: neosh_proto::SessionInfo) -> neosh_proto::SessionInfo {
        if let Some(facts) = self.projects.get(std::path::Path::new(&info.cwd)) {
            info.project = facts.name.clone();
            info.repo_root = facts.repo_root.clone();
            info.branch = facts.branch.clone();
        }
        info
    }

    pub fn begin_startup(&mut self, boot: Bootstrap) {
        self.seed_hints();
        self.draw_welcome();
        self.refresh_status();
        self.refresh_composer();
        self.startup.reload = Some(boot.reload);
        self.startup.base_instances =
            self.agent.providers().instances().cloned().collect();
        // Started once, at boot, and never on reload: the allow-list and the listening socket are
        // decided by config, and re-reading them would mean tearing down live connections whenever
        // somebody pressed `^R`. Changing the swarm is a restart, and saying so is kinder than a
        // reload that half-works.
        let swarm = boot.resolved.swarm.clone();
        self.apply_config(boot.resolved, boot.cli_model, boot.cli_plugin_dirs);
        self.start_swarm(&swarm);
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
            ("quit", "Close this terminal. Whatever is running keeps running"),
            ("stop", "Stop the workspace, and everything running in it"),
            ("interrupt", "Stop what is running, or leave"),
            ("chat.scroll_up", "Scroll the transcript up a screen"),
            ("chat.scroll_down", "Scroll the transcript down a screen"),
            ("chat.scroll_end", "Jump to the end of the transcript"),
            (
                "chat.read",
                "Read the transcript: hjkl move, [ ] turns, { } blocks, / finds, yc takes the code",
            ),
            ("chat.toggle_card", "Open or fold the tool card under the cursor, while reading"),
            (
                "chat.queue.edit",
                "Take the last thing you queued back into the composer, to change it or drop it",
            ),
            ("chat.image.paste", "Attach the image on the clipboard"),
            ("chat.image.drop", "Take the last attached image back off"),
            ("chat.image.clear", "Take every attached image off"),
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
        // Two lists, and the split is the point. **A mode's keys are the mode's keys**: while you
        // are reading the transcript, `^X` cutting a composer you cannot see and `^Y` pulling a
        // queued message into it are not things you asked for — they are the previous mode still
        // holding the keyboard. Everything that acts on the composer or scrolls the transcript
        // *without* the cursor is therefore bound in `Chat` alone, and reading answers those keys
        // itself, in the way this mode means them.
        //
        // What is left is the escape hatches, and only those. A config that fails to load can put
        // you somewhere unexpected, which is exactly when you need to reload, quit, interrupt, or
        // get into the transcript — so those four are bound everywhere and nothing else is.
        for (lhs, command, desc) in [
            ("<C-r>", "config.reload", "Reload configuration"),
            ("<C-q>", "quit", "Close neosh"),
            // Bound because it is the first thing anyone tries. In raw mode the terminal does
            // not turn this into SIGINT, so without a binding it does nothing at all — and a
            // program you cannot get out of is not one people give a second chance.
            ("<C-c>", "interrupt", "Cancel, clear, or quit"),
            // Reading the transcript is a separate place, and this is the door to it.
            ("<C-s>", "chat.read", "Read, select and copy the transcript"),
        ] {
            for mode in [Mode::Normal, Mode::Insert, Mode::Visual, Mode::Chat] {
                let _ = self.editor.apply(&plugin, ApiCall::KeymapSet {
                    mode,
                    lhs: lhs.to_string(),
                    command: command.to_string(),
                    scope: None,
                    desc: Some(desc.to_string()),
                });
            }
        }
        for (lhs, command, desc) in [
            // Scrolling that leaves the cursor behind, which is what you want when the composer
            // has it. Reading binds the same two keys to a screen of *motion*, because there the
            // cursor is the thing you are moving.
            ("<PageUp>", "chat.scroll_up", "Scroll up"),
            ("<PageDown>", "chat.scroll_down", "Scroll down"),
            ("<C-End>", "chat.scroll_end", "Back to the newest message"),
            ("<C-x>", "edit.cut", "Cut the selection"),
            ("<C-a>", "edit.select_all", "Select everything"),
            // `^Y`, because it is readline's: `^U` and `^W` kill, `^Y` brings back what was
            // killed, and those two are already the composer's. Sending while a turn runs is a
            // kill of the same kind — the sentence leaves the field — so the key that undoes it is
            // the one a terminal's fingers already know.
            //
            // It used to be `⇧↑`, which needs an arrow, and a keyboard without arrows could not
            // reach it at all. It also took `⇧↑` away from the composer, where it is how you select
            // upward through a pasted snippet — the field handles it and never saw it.
            ("<C-y>", "chat.queue.edit", "Take the last queued message back"),
            // The key a terminal cannot deliver an image with, bound to the only thing that can.
            // Bracketed paste is a *text* protocol — `⌘V` on a screenshot arrives as nothing at
            // all — so pasting a picture has to be a key that goes and asks the clipboard, and
            // `^V` is the one every hand is already on. It stays free for that: nothing else in
            // chat wants it, and the composer has never had a use for it.
            ("<C-v>", "chat.image.paste", "Attach the image on the clipboard"),
            // Taking one back off is `⌫` on an empty composer, handled below rather than bound
            // here. It was `⌥V`, which is not a key: a Mac terminal turns Option-v into `√`
            // unless its owner has been into the settings, and a keyboard whose Alt is a layer
            // toggle cannot send it at all. `chat.image.drop` is still a command, so `^K` runs it
            // and `init.ts` can put it back on a chord of your own.
        ] {
            let _ = self.editor.apply(&plugin, ApiCall::KeymapSet {
                mode: Mode::Chat,
                lhs: lhs.to_string(),
                command: command.to_string(),
                scope: None,
                desc: Some(desc.to_string()),
            });
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
        // A mode that only reached the built-in tools would leave an agent driver enforcing the
        // one it was told at startup, under a footer saying otherwise.
        self.sync_agent_drivers();
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
        if let Some(stopped) = self.cancel_turn_here() {
            self.quit_armed = None;
            if stopped {
                self.editor_message(MessageLevel::Info, "interrupted");
            }
            return;
        }
        if !self.composer_text().is_empty() {
            self.set_composer("");
            self.quit_armed = None;
            return;
        }
        let armed = self.quit_armed.is_some_and(|at| at.elapsed() < ARM_WINDOW);
        if armed {
            self.leave();
            return;
        }
        self.quit_armed = Some(std::time::Instant::now());
        self.editor_message(MessageLevel::Info, match self.on_quit {
            OnQuit::Stop => "press ^C again to quit, or ^Q",
            OnQuit::Detach => "press ^C again to close this terminal, or ^Q",
        });
    }

    /// What `quit` does here — from the key, from the command, from the last rung of `^C`.
    ///
    /// One door, because there is only one door. A workspace is a process with turns running in it
    /// and possibly other terminals looking at it, so the key that ends *this* terminal must not be
    /// able to end that. `^C^C` set `quitting` directly and took the workspace with it: every
    /// conversation, in every project, on a key whose first four meanings — copy, leave reading,
    /// interrupt, clear the draft — are all local. ADR 0036's rule that `^Q` must never stop a
    /// workspace was never really about `^Q`; it is about every way out there is.
    fn leave(&mut self) {
        match self.on_quit {
            OnQuit::Stop => self.quitting = true,
            OnQuit::Detach => self.detaching = Some(self.from_view),
        }
    }

    /// Page the transcript. `0` returns to following the tail.
    ///
    /// Following the newest is `None` rather than a row, which is why jumping to the end is a reset
    /// rather than a seek: the renderer already shows the last screenful when nothing has scrolled.
    /// Row zero is `Some(0)` and means the top — paging up to it used to ask for "follow" and land
    /// back at the end, one page short of the beginning every time.
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
            0 => None,
            d if d < 0 => {
                // From "following", scrolling up starts from where the tail actually begins.
                let from = current.unwrap_or_else(|| lines.saturating_sub(page));
                Some(from.saturating_sub(page))
            }
            _ => match current {
                // Already at the end, and there is nowhere past it to scroll into.
                None => None,
                Some(c) => {
                    let next = c.saturating_add(page);
                    // Past the end means back to following, so a user cannot scroll into blank
                    // space.
                    if next + page >= lines { None } else { Some(next) }
                }
            },
        };
        self.chat_top = top;
        let _ = self.editor.apply(&plugin, ApiCall::WinScrollTo { win, top_line: top });
    }

    fn run_builtin(&mut self, name: &str, args: Vec<String>) {
        match name {
            "config.reload" => self.reload_config(),
            "quit" => self.leave(),
            "stop" => self.quitting = true,
            "interrupt" => self.interrupt(),
            "chat.scroll_up" => self.scroll_chat(-1),
            "chat.scroll_down" => self.scroll_chat(1),
            "chat.scroll_end" => self.scroll_chat(0),
            // A toggle, because it is the door and a door opens both ways. Pressed while already
            // reading it used to do nothing at all, which for the one key bound in *every* mode is
            // the wrong answer — that set exists so there is always something that works.
            "chat.read" => {
                if self.reading {
                    self.leave_reading();
                } else {
                    self.enter_reading();
                }
            }
            // Sending while a turn runs *queues*, which is the right default and is also the one
            // send you cannot take back by pressing anything. Enter is a decision made in half a
            // second about a sentence you were still writing; this is the other half of it.
            //
            // Back into the composer rather than simply dropped, because "I queued the wrong thing"
            // and "I queued the right thing badly" are the same keystroke away from each other, and
            // a key that deleted your sentence to fix a typo in it would be a worse trade than
            // having no key at all. Whatever is already in the composer keeps its place after it.
            "chat.queue.edit" => {
                let here = self.active_session();
                match self.agent.unqueue_last(&here) {
                    None => self.editor_message(MessageLevel::Info, "nothing queued"),
                    Some(prompt) => {
                        let draft = self.composer_text();
                        let joined = match draft.trim().is_empty() {
                            true => prompt.text,
                            false => format!("{}\n{draft}", prompt.text),
                        };
                        // What was attached to it comes back too, on the row it came off. A
                        // sentence you can edit and a picture you cannot put back would be half
                        // an undo.
                        if !prompt.images.is_empty() {
                            let store = self.image_store();
                            let row = self.attached.entry(here).or_default();
                            for b in &prompt.images {
                                if let ContentBlock::Image { path, .. } = b
                                    && let Ok(a) =
                                        crate::images::from_path(&store, std::path::Path::new(path))
                                {
                                    row.push(a);
                                }
                            }
                        }
                        self.set_composer(&joined);
                        self.draw_working();
                        self.refresh_composer();
                    }
                }
            }
            // Nothing on the clipboard is the common case rather than a failure, so it is said the
            // same way "nothing queued" is: a line, not an error.
            // Only the failure is worth a line: a chip appearing above the composer says the
            // other thing better than a sentence could. "Nothing on the clipboard" has nothing to
            // show for itself, and is the answer people actually need explaining.
            "chat.image.paste" => {
                if let Err(e) = self.attach(None) {
                    self.editor_message(MessageLevel::Warn, e);
                }
            }
            "chat.image.drop" => self.drop_attachment(),
            "chat.image.clear" => {
                let here = self.active_session();
                let n = self.attached.remove(&here).unwrap_or_default().len();
                self.editor_message(
                    MessageLevel::Info,
                    match n {
                        0 => "nothing attached".to_string(),
                        1 => "took off 1 image".to_string(),
                        n => format!("took off {n} images"),
                    },
                );
                self.refresh_composer();
            }
            "chat.toggle_card" => {
                if self.reading {
                    self.toggle_card_here();
                } else {
                    self.editor_message(MessageLevel::Info, "^S first: cards open while reading");
                }
            }
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
                name: "chat.show_plan".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(true),
                description: Some(
                    "Show the agent's own checklist above the working line while it works, and \
                     leave the final one under its answer. Only drivers that keep a plan have one."
                        .into(),
                ),
            },
            OptionSpec {
                name: "chat.plan_rows".into(),
                ty: OptionType::Int { min: Some(0), max: Some(50) },
                default: OptionValue::Int(5),
                description: Some(
                    "How many rows the checklist may take above the working line. Past it, what is \
                     finished and what is not started yet each become a count and the step in hand \
                     stays. `0` shows every step. The copy left under the answer is always whole."
                        .into(),
                ),
            },
            OptionSpec {
                name: "chat.tool_output_lines".into(),
                ty: OptionType::Int { min: Some(0), max: Some(200) },
                default: OptionValue::Int(3),
                description: Some(
                    "How much of what a tool came back with to show under it. `0` shows only how \
                     many lines there were. An error always shows at least its first line."
                        .into(),
                ),
            },
            OptionSpec {
                name: "chat.diff_lines".into(),
                ty: OptionType::Int { min: Some(0), max: Some(500) },
                default: OptionValue::Int(12),
                description: Some(
                    "How many rows of an edit's diff to show under its card before it folds. `0` \
                     leaves only the +N -N on the header. Any card opens with Tab while reading."
                        .into(),
                ),
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
            // the same thing — `<C-n>` and `<Down>` both move down, and adding a third is a
            // setting rather than a fork. Declared by the host rather than by whichever plugin
            // happens to open a picker first, because two plugins declaring the same option name
            // is an error and every plugin uses this widget library.
            //
            // **The chord comes first, and the arrow second.** The first key in the list is the
            // one a widget prints on its hint row, and a legend is a promise about a keyboard:
            // every terminal sends `^N`, while an arrow is a key some keyboards only have on a
            // layer and some remote sessions mangle. Both work — this is the order they are
            // *offered* in, not the set.
            OptionSpec {
                name: "ui.keys.next".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-n> <Down> <C-j>".into()),
                description: Some("Move down in a picker or completion list.".into()),
            },
            OptionSpec {
                name: "ui.keys.prev".into(),
                ty: OptionType::Str,
                default: OptionValue::Str("<C-p> <Up> <C-k>".into()),
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
            // Where worktrees go. Host-owned rather than the git plugin's, for two reasons: it is
            // a place neosh writes on your disk, which is the host's business to answer for, and
            // the default has to be an absolute path — a plugin has no way to find your home
            // directory, and a `~` that only some readers expand is worse than no `~` at all.
            OptionSpec {
                name: "worktree.root".into(),
                ty: OptionType::Str,
                default: OptionValue::Str(default_worktree_root()),
                description: Some(
                    "Where `git.worktree.new` puts a new worktree, as `<root>/<repo>/<branch>`. \
                     A leading `~` is expanded. A relative path is inside the repository itself — \
                     `.worktrees` puts them at `<repo>/.worktrees/<branch>`, kept out of `git \
                     status` for you. Empty means beside the repository instead, in \
                     `<repo>-worktrees/`."
                        .into(),
                ),
            },
            // Display settings every widget reads. Declared here rather than by whichever plugin
            // happens to load first: two plugins declaring one name is an error, and a plugin that
            // is switched off must not take the setting away from everything else — which is
            // exactly what happened when the sidebar owned these and `plugins.disabled` was used.
            // Off by default. It read as a good idea and was not: nearly every key on it is
            // already in the sidebar's own footer, two rows below, and the ones that are not are
            // on the help key. What it actually did was take a row off the transcript and press
            // the composer against the status strip.
            OptionSpec {
                name: "ui.hints".into(),
                ty: OptionType::Bool,
                default: OptionValue::Bool(false),
                description: Some(
                    "Show a shortcut row under the composer. Off by default: the sidebar's footer \
                     carries the same keys and `^Z` carries all of them, so the row mostly cost \
                     the transcript a line."
                        .into(),
                ),
            },
            // The way out of a panel that has taken the keyboard. A modal float stops global
            // bindings resolving — which is what makes `^N` over an open picker do nothing instead
            // of opening a conversation behind it — so these are named to survive that. A list
            // rather than a constant because they are keys, and every key here is the user's; empty
            // it and the only way out of a panel is the way the panel gives you.
            OptionSpec {
                name: "ui.modal_escape_keys".into(),
                ty: OptionType::List,
                default: OptionValue::List(vec!["<C-q>".into(), "<C-r>".into()]),
                description: Some(
                    "Keys that still reach the workspace while a modal panel is open. Quit and \
                     reload by default, so a panel that fails to bind a way out is never a \
                     terminal you have to kill."
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
                self.run_held_commands();
                // Before a queued reload, which tears all of this down and builds it again — and
                // says so a second time when *it* settles.
                self.announce_ready();
                self.run_pending_reload();
            }
        }
    }

    /// Whether anything is still being loaded — a config script or a plugin.
    fn startup_in_flight(&self) -> bool {
        self.startup.active.is_some() || self.startup.loading > 0 || !self.startup.discovered
    }

    /// Run the commands that arrived before anything had registered them.
    ///
    /// In the order they were asked for, which is the only order that can be right: two of them
    /// are a person pressing two keys.
    fn run_held_commands(&mut self) {
        for (name, args) in std::mem::take(&mut self.startup.commands) {
            self.editor.exec_command(&name, args);
            self.drain_effects();
        }
    }

    /// Say that the workspace is up.
    ///
    /// A plugin's own `activate` returning is not this: the other plugins are still loading
    /// alongside it, so a key `^E` is bound to is a key nothing has bound yet, a contribution
    /// point another plugin fills is still empty, and an `agent.model` naming a provider nobody
    /// has registered is still held. Anything that depends on the *rest* of the workspace waits
    /// for `neosh.ready` instead of guessing.
    ///
    /// An ordinary event on the ordinary bus rather than a new wire type — a plugin listens for it
    /// with the same `neosh.event.on` it uses for anything else, and `from` is `neosh` because the
    /// host is what said it. It is announced again after a reload, which is the same fact about the
    /// same workspace being true a second time.
    fn announce_ready(&mut self) {
        self.bridge.broadcast(PluginEvent::Event {
            name: READY_EVENT.to_string(),
            data: None,
            from: BUILTIN.to_string(),
        });
    }

    /// Apply a held `agent.model` the moment something can serve it.
    ///
    /// Not a replacement for [`Self::retry_pending_model`], which is where a spec that nothing
    /// *ever* serves is finally reported and where the fallback is chosen. This one only ever
    /// succeeds: it is called on every provider registration, and a spec that still resolves to
    /// nothing is left held for the end of startup exactly as before.
    fn apply_pending_model_if_servable(&mut self) {
        let Some(spec) = self.startup.pending_model.clone() else { return };
        // The read guard is released at the end of this statement, before `apply_option` takes
        // `&mut self`.
        let servable = Config::parse_model(&spec).is_some_and(|(instance, model)| {
            self.agent
                .providers()
                .resolve(&neosh_proto::ModelSelection {
                    instance: instance.into(),
                    model: neosh_proto::ModelId::from(model),
                    options: Vec::new(),
                })
                .is_some()
        });
        if servable {
            self.apply_option("agent.model", &OptionValue::Str(spec));
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
                    self.select_model(s);
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
        if self.agent.selection().is_none() {
            // The read guard is released before anything takes `&mut self`.
            let first = self.agent.providers().default_selection();
            if let Some(s) = first {
                self.select_model(s);
            }
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
            // Its keymaps are defaults from here on: they will not take a key something else has
            // already bound, which is the only way `init.ts` running first can also mean it wins.
            self.editor.mark_bundled(&id);
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
            self.run_held_commands();
            self.announce_ready();
            // Nothing to wait for — a reload queued during config activation runs now.
            self.run_pending_reload();
        }
        self.refresh_status();
    }

    /// Ask the runtime to import and activate one entry module.
    ///
    /// A failure here is reported and skipped: one broken plugin — or one broken config script —
    /// must not stop the editor from starting.
    ///
    /// The plugin joins the broadcast list *here*, when its load is asked for, rather than when
    /// its `Loaded` comes back. Those are not the same moment: `activate` runs — and registers
    /// listeners — before the host is told it finished, and several plugins activate in the gap.
    /// Anything broadcast in that window used to be delivered to whoever happened to have been
    /// confirmed already, which made the answer depend on how many plugins were installed and in
    /// what order they finished. It cost a held `[options]` value its `option_changed`, and the
    /// only reason it had never been seen is that the plugin declaring one was usually last.
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
            Ok(()) => {
                // Reachable from here, not from its `Loaded`. See [`Self::load_script`].
                self.bridge.register_loaded(id.clone());
                true
            }
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
pub fn install_builtin_providers(
    agent: &Agent,
) -> Vec<Arc<dyn neosh_provider::drivers::AgentDriver>> {
    let mut out: Vec<Arc<dyn neosh_provider::drivers::AgentDriver>> = Vec::new();
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
        let claude = Arc::new(neosh_provider::drivers::ClaudeCliProvider::default());
        if claude.available() {
            reg.register_driver(claude.clone());
            out.push(claude);
        }
        let codex = Arc::new(neosh_provider::drivers::CodexCliProvider::default());
        if codex.available() {
            reg.register_driver(codex.clone());
            out.push(codex);
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
        ApiCall::GitBranches { include_remote, cwd } => {
            svc.git_branches(include_remote, cwd).await
        }
        ApiCall::GitWorktrees { cwd } => svc.git_worktrees(cwd).await,
        ApiCall::GitLog { limit } => svc.git_log(limit).await,
        ApiCall::GitDiff { target, stat } => svc.git_diff(target, stat).await,
        ApiCall::GitDefaultBranch => svc.git_default_branch().await,
        ApiCall::GitCreateBranch { name, from } => svc.git_create_branch(name, from).await,
        ApiCall::GitCheckout { rev } => svc.git_checkout(rev).await,
        ApiCall::GitStage { paths } => svc.git_stage(paths).await,
        ApiCall::GitUnstage { paths } => svc.git_unstage(paths).await,
        ApiCall::GitCommit { message } => svc.git_commit(message).await,
        ApiCall::GitPull { cwd } => svc.git_pull(cwd).await,
        ApiCall::GitAddWorktree { path, branch, create, cwd } => {
            svc.git_add_worktree(path, branch, create, cwd).await
        }
        ApiCall::GitRemoveWorktree { path, force, cwd } => {
            svc.git_remove_worktree(path, force, cwd).await
        }
        ApiCall::GenComplete { prompt, system, json, selection } => {
            svc.gen_complete(prompt, system, json, selection).await
        }
        // Off the host loop for the same reason as git: discovery is a network round trip per
        // configured provider, and doing it inline freezes typing for as long as it takes.
        ApiCall::AgentListModels { instance, refresh } => svc.list_models(instance, refresh).await,
        ApiCall::PermissionCheck { capability } => svc.permission_check(capability).await,
        ApiCall::AskUser { questions } => svc.ask_user(questions).await,
        ApiCall::UsageHistory { since, until, resolution, time_zone } => {
            svc.usage_history(since, until, resolution, time_zone).await
        }
        other => Err(ApiError::Internal {
            message: format!("{other:?} is not a background call"),
        }),
    }
}

/// Seconds since the epoch, or 0 if the clock is before it.
///
/// Zero rather than a panic: a wrong timestamp sorts a list oddly, and refusing to create a session
/// because the system clock is unset would be a much worse trade.
/// How many lines a tool wrote, as the card counts them.
///
/// Trailing blank lines are not lines anybody read. A `Read` whose output ends with a newline —
/// which almost every file does — would otherwise be reported as one line longer than it is, and a
/// card that is out by one about something this easy to check is a card nothing else on it is
/// believed.
fn lines_in(content: &str) -> usize {
    let body = content.trim_end();
    if body.is_empty() { 0 } else { body.lines().count() }
}

/// A refusal, in words a person can act on.
fn refusal_text(r: &neosh_proto::Refusal) -> String {
    use neosh_proto::Refusal as R;
    match r {
        R::NoSuchAgent => "that conversation is not there any more".into(),
        R::NotPermitted { what } => what.clone(),
        R::Busy { message } | R::Failed { message } => message.clone(),
    }
}

/// What to call this machine when nobody has said.
///
/// The hostname, which is what a person would answer if asked which computer they were at. Read
/// from the environment rather than a syscall: `HOSTNAME` and `COMPUTERNAME` cover unix shells and
/// Windows, and "neosh" is a name rather than a failure for the case where neither is set.
fn machine_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .map(|h| h.split('.').next().unwrap_or(&h).to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "neosh".to_string())
}

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
fn transcript(
    session: &neosh_agent::Session,
    ascii: bool,
    limits: cards::Limits,
    width: usize,
    show_tools: bool,
    running: bool,
) -> (Vec<String>, Vec<Mark>, Vec<Card>) {
    use neosh_proto::{ContentBlock, Role};

    let g = Glyphs::new(ascii);
    let root = session.cwd.as_path();
    // Which calls have an answer, whether it was a failure, what it exited with, and how much it
    // came back with. Gathered first because a result always comes *after* the call it answers, and
    // a dot that only turns green on the second pass over the buffer is a dot that flickers on
    // every switch. The other three are here for the same reason: they all belong on the header,
    // which is written before the result carrying them has been reached.
    type Answer = (bool, Option<i64>, usize);
    let answered: std::collections::HashMap<&neosh_proto::ToolCallId, Answer> = session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, is_error, content } => Some((
                tool_use_id,
                (
                    *is_error,
                    is_error.then(|| cards::exit_code(content)).flatten(),
                    lines_in(content),
                ),
            )),
            _ => None,
        })
        .collect();
    let mut lines: Vec<String> = Vec::new();
    let mut marks: Vec<Mark> = Vec::new();
    let mut cards: Vec<Card> = Vec::new();
    // What the turn being read has changed, for the summary that closes it.
    let mut changes: Vec<cards::FileStat> = Vec::new();

    // Rows go in at `at` — under a card's header, usually — and everything below moves down, the
    // marks and the cards with it. The same shape the live path has, for the same reason: an agent
    // runs calls in parallel, and a result appended at the end lands under the wrong card.
    fn insert(
        lines: &mut Vec<String>,
        marks: &mut Vec<Mark>,
        cards: &mut [Card],
        at: u32,
        rows: Vec<cards::Row>,
    ) {
        let n = rows.len() as u32;
        if n == 0 {
            return;
        }
        for m in marks.iter_mut().filter(|m| m.row >= at) {
            m.row += n;
        }
        for c in cards.iter_mut().filter(|c| c.row >= at) {
            c.row += n;
        }
        for (k, r) in rows.into_iter().enumerate() {
            let row = at + k as u32;
            marks.extend(Mark::of(row, &r));
            lines.insert(row as usize, r.text);
        }
    }
    // A card's header, from what the conversation says about every call on it.
    //
    // The results are read out of `answered` rather than off the legs: a result block always comes
    // *after* the call it answers, and a header that only settled on the second pass would show
    // every finished call as running for one frame of every switch.
    fn header_of(
        g: &Glyphs,
        card: &Card,
        answered: &std::collections::HashMap<&neosh_proto::ToolCallId, (bool, Option<i64>, usize)>,
        root: &std::path::Path,
        width: usize,
    ) -> cards::Row {
        let heads: Vec<cards::Head> = card
            .legs
            .iter()
            .map(|leg| {
                let (state, exit, output) = match answered.get(&leg.id) {
                    Some((true, exit, n)) => (ToolState::Failed, *exit, Some(*n)),
                    Some((false, _, n)) => (ToolState::Done, None, Some(*n)),
                    None => (ToolState::Running, None, None),
                };
                cards::Head {
                    name: &leg.name,
                    input: &leg.input,
                    state,
                    took: None,
                    exit,
                    output,
                }
            })
            .collect();
        match heads.as_slice() {
            [one] => cards::header(g, one, root, width),
            many => cards::group_header(g, many, root, width),
        }
    }
    // A gap, unless there already is one: what came before was often a tool card, which has no
    // trailing blank of its own, and the next thing butted against it reads as part of it.
    #[allow(clippy::items_after_statements)]
    fn gap(lines: &mut Vec<String>) {
        if lines.last().is_some_and(|l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
    }
    let summarise = |lines: &mut Vec<String>, marks: &mut Vec<Mark>, changes: &mut Vec<cards::FileStat>| {
        let rows = cards::summary(changes, root, width);
        changes.clear();
        if rows.is_empty() {
            return;
        }
        gap(lines);
        for r in rows {
            let row = lines.len() as u32;
            marks.extend(Mark::of(row, &r));
            lines.push(r.text);
        }
    };

    // Whether the row just written was an attached image, which is the one case where the text
    // under it must *not* get a blank line of its own: the picture and the sentence are one
    // question, and a gap between them reads as two.
    let mut after_image = false;
    for message in &session.messages {
        for block in &message.content {
            let was_image = std::mem::take(&mut after_image);
            match block {
                ContentBlock::Image { path, media_type } if message.role == Role::User => {
                    // Images come first in a user message, so this is where the turn boundary is.
                    if !was_image {
                        summarise(&mut lines, &mut marks, &mut changes);
                        gap(&mut lines);
                    }
                    marks.push(Mark::at(lines.len() as u32, 0, g.bar.len(), "Agent.User"));
                    lines.push(format!("{} {}", g.bar, image_row(path, media_type)));
                    after_image = true;
                }
                ContentBlock::Text { text } if message.role == Role::User => {
                    // A question starts a turn, so the one before it is over.
                    if !was_image {
                        summarise(&mut lines, &mut marks, &mut changes);
                        gap(&mut lines);
                    }
                    for line in text.lines() {
                        marks.push(Mark::at(lines.len() as u32, 0, g.bar.len(), "Agent.User"));
                        lines.push(format!("{} {line}", g.bar));
                    }
                    lines.push(String::new());
                }
                ContentBlock::Text { text } => {
                    // Through the same renderer the live stream uses, so switching away and back
                    // does not change what an answer looks like — and behind the same blank row,
                    // which `open_answer_row` puts there live and this used to leave out. Nothing
                    // showed while a card had no air of its own; the moment one did, a sentence
                    // after a card was the only thing on the screen still butted against it.
                    gap(&mut lines);
                    let at = lines.len() as u32;
                    let (rendered, styled) = crate::markdown::render(text);
                    marks.extend(
                        styled.into_iter().map(|(r, a, b, g)| Mark::at(at + r as u32, a, b, g)),
                    );
                    lines.extend(rendered);
                    lines.push(String::new());
                }
                // Gated on the same setting the live path uses. Drawn unconditionally, switching
                // conversations with tool cards off made them all appear.
                ContentBlock::ToolUse { id, name, input } if show_tools => {
                    // The mark says how it went, which the conversation already records: a call
                    // with a result somewhere after it is finished, and one without is still
                    // running. Nothing here has to remember anything.
                    let exit = match answered.get(id) {
                        Some((true, exit, _)) => *exit,
                        _ => None,
                    };
                    let leg = Leg {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        // The result is filled in when its block is reached, which is what puts a
                        // body under the right header. The *header* cannot wait for that — it is
                        // written now, from `answered`, or a restored transcript would show every
                        // call as running until the second pass.
                        result: None,
                        // Nothing here watched the call happen, so nothing here can say how long it
                        // took. A restored transcript is a record, not a replay.
                        started: None,
                        took: None,
                        exit,
                    };
                    // Onto the run above when there is one and nothing has been drawn since — the
                    // same rule the live path uses, because the two have to produce the same rows.
                    let open = cards
                        .last()
                        .filter(|c| c.row + c.body + 1 == lines.len() as u32)
                        .is_some_and(|c| c.takes(input));
                    let at = if open {
                        let i = cards.len() - 1;
                        cards[i].legs.push(leg);
                        cards[i].row
                    } else {
                        // No gap, exactly as the live path draws it: the two have to produce the
                        // same rows, or switching away and back re-spaces the conversation.
                        let at = lines.len() as u32;
                        cards.push(Card::new(leg, at));
                        lines.push(String::new());
                        at
                    };
                    let i = cards.len() - 1;
                    let row = header_of(&g, &cards[i], &answered, root, width);
                    marks.retain(|m| m.row != at);
                    marks.extend(Mark::of(at, &row));
                    lines[at as usize] = row.text;
                }
                ContentBlock::ToolUse { .. } => {}
                // Errors show even with tool cards off, exactly as they do live: the setting is
                // about noise, not about hiding failures.
                ContentBlock::ToolResult { tool_use_id, is_error, content }
                    if *is_error || show_tools =>
                {
                    let result = neosh_proto::ToolResult {
                        content: content.clone(),
                        is_error: *is_error,
                    };
                    let found = cards
                        .iter()
                        .enumerate()
                        .rev()
                        .find_map(|(i, c)| c.leg_of(tool_use_id).map(|k| (i, k)));
                    match found {
                        // Under its own header, however many other calls came between.
                        Some((i, k)) => {
                            if !*is_error {
                                cards::tally(&mut changes, &cards::edits_of(&cards[i].legs[k].input));
                            }
                            // A run holds its calls' answers and shows none of them: it is one row
                            // until somebody opens it, exactly as it is live.
                            let rows = if cards[i].legs.len() > 1 {
                                Vec::new()
                            } else {
                                cards::body(&g, &cards[i].legs[k].input, &result, limits, false, width)
                            };
                            let n = rows.len() as u32;
                            let at = cards[i].row + 1;
                            insert(&mut lines, &mut marks, &mut cards, at, rows);
                            cards[i].legs[k].result = Some(result);
                            cards[i].body = n;
                        }
                        // No card to go under — cards are off and this is an error — so at the
                        // end, which is where the live path puts it too.
                        None => {
                            let rows = cards::body(
                                &g,
                                &serde_json::Value::Null,
                                &result,
                                limits,
                                false,
                                width,
                            );
                            for r in rows {
                                let row = lines.len() as u32;
                                marks.extend(Mark::of(row, &r));
                                lines.push(r.text);
                            }
                        }
                    }
                }
                ContentBlock::ToolResult { .. } => {}
                // An image on an *assistant* message, which nothing produces today. Silently
                // dropped rather than drawn: a row saying "[png]" under an answer nobody attached
                // one to would be the transcript inventing something.
                ContentBlock::Image { .. } => {}
                // Reasoning is not replayed: it is shown live when `chat.show_thinking` is on, and
                // reprinting it on every switch would bury the answer under the working out.
                ContentBlock::Thinking { .. } => {}
            }
        }
    }
    // The last turn is over too — unless it is still going, in which case its summary is drawn
    // when it ends, by whoever is looking then.
    if !running {
        summarise(&mut lines, &mut marks, &mut changes);
    }
    // Where it stopped, when nothing stopped it.
    //
    // The sidebar can say *which* conversation was cut off; only the transcript can say where. What
    // the agent had finished is above this line and is real; what it was in the middle of is not
    // anywhere, and without a line saying so the conversation simply ends — on a half-written
    // sentence, or on a tool call with no result, which reads exactly like an agent that gave up.
    //
    // Not drawn for a turn that is running: `running` is a turn in flight *now*, in this process,
    // and this mark is about one that was in flight in a process that is gone.
    if session.interrupted && !running {
        gap(&mut lines);
        let row = lines.len() as u32;
        let r = cards::Row::plain(
            format!("{} the workspace stopped while this turn was running", g.fail),
            "Diagnostic.Error",
        );
        marks.extend(Mark::of(row, &r));
        lines.push(r.text);
    }
    (lines, marks, cards)
}

/// One highlight in a rebuilt transcript, before it becomes an extmark.
///
/// `span` is `None` for a band — a group behind the whole row rather than behind a range of bytes
/// on it. The two travel in one list because they are written together and have to arrive together:
/// a diff row whose band was applied on a later pass would be green a frame after it was green.
#[derive(Clone, Copy)]
struct Mark {
    row: u32,
    span: Option<(usize, usize)>,
    hl: &'static str,
}

impl Mark {
    fn at(row: u32, from: usize, to: usize, hl: &'static str) -> Self {
        Self { row, span: Some((from, to)), hl }
    }

    /// Every mark a card row carries, ready to be shifted onto whatever row it lands on.
    fn of(row: u32, r: &cards::Row) -> Vec<Self> {
        r.spans
            .iter()
            .map(|(a, b, h)| Self::at(row, *a, *b, h))
            .chain(r.band.map(|h| Self { row, span: None, hl: h }))
            .collect()
    }

    fn shifted(self, by: u32) -> Self {
        Self { row: self.row + by, ..self }
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

/// What the host learned about a directory the first time it looked: the display name, and the
/// checkout facts the name was made from. Kept whole because [`SessionInfo`] carries both — the
/// name for reading, the root for grouping — and deriving one from the other means parsing a
/// display format back apart.
///
/// [`SessionInfo`]: neosh_proto::SessionInfo
struct ProjectFacts {
    name: String,
    /// The main checkout's path; `None` outside a repository.
    repo_root: Option<String>,
    /// The branch at that directory when it was first seen; `None` detached.
    branch: Option<String>,
}

/// What to call a checkout.
///
/// The last segment of the path is what everything used to use, and it is wrong for exactly the
/// case a project list exists to make sense of: a worktree is named by whoever created it, so a
/// panel reading `wt-fe3c0d93` says nothing about which repository or branch that is — and
/// somebody looking at it reasonably wonders what an unrelated tool's directory is doing in their
/// workspace.
///
/// So: a checkout is named after the repository it belongs to, and a *linked* worktree adds its
/// branch. `neosh` for the original, `neosh · fix/thing` for a worktree of it. Anything that is not
/// a repository keeps its directory name, which is all there is to go on.
fn project_name(
    dir: &std::path::Path,
    branch: Option<String>,
    main: Option<std::path::PathBuf>,
) -> String {
    let leaf = |p: &std::path::Path| {
        p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
    };
    let Some(main) = main else { return leaf(dir) };
    let repo = leaf(&main);
    // The original checkout is just the repository. Adding its branch would put a word that
    // changes on every `git switch` next to a name that never does.
    if same_dir(dir, &main) {
        return repo;
    }
    match branch {
        Some(b) => format!("{repo} \u{b7} {b}"),
        // A detached head has no name of its own; the directory somebody chose is the next best
        // thing, and better than showing a bare hash.
        None => format!("{repo} \u{b7} {}", leaf(dir)),
    }
}

/// The one row an attached image gets in a transcript.
///
/// A terminal cannot show you the picture, so what it shows instead has to be the two things you
/// would use to tell one attachment from another: what kind it is, and what it was called. The
/// name is the file's, which for an image that came off the clipboard is a uuid nobody chose —
/// so it is only worth saying when somebody did choose it.
fn image_row(path: &str, media_type: &str) -> String {
    let kind = media_type.strip_prefix("image/").unwrap_or(media_type);
    let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("");
    // A uuid is 36 characters of nothing. Anything else is a name somebody gave the file.
    let named = stem.len() != 36 || !stem.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    match named && !stem.is_empty() {
        true => format!("[{kind} \u{b7} {stem}]"),
        false => format!("[{kind}]"),
    }
}

/// What the API says about an attachment.
fn info_of(a: &crate::images::Attachment) -> neosh_proto::AttachmentInfo {
    neosh_proto::AttachmentInfo {
        path: a.path.display().to_string(),
        media_type: a.media_type.clone(),
        width: a.width,
        height: a.height,
        bytes: a.bytes.min(u32::MAX as usize) as u32,
    }
}

/// Whether two paths are the same directory, following symlinks where they resolve.
fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The other direction: `~/x` back into an absolute path.
///
/// Returns nothing when there is no home directory to substitute, which leaves the path exactly as
/// written — a literal directory called `~` is a strange thing to have, but inventing a path is
/// worse than honouring one.
fn expand_tilde(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix('~')?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        // `~other/x` is another user's home, which is not ours to resolve.
        return None;
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(format!("{}{rest}", home.to_string_lossy()))
}

/// Where worktrees go unless told otherwise.
///
/// `~/.nsh`, expanded at declare time so the default is already absolute — an option whose default
/// value is a string only some readers understand is a trap for the first plugin that reads it
/// without thinking.
fn default_worktree_root() -> String {
    expand_tilde("~/.nsh").unwrap_or_else(|| ".nsh".to_string())
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
