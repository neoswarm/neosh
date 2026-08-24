//! The single writer.
//!
//! One [`Editor`] owns every piece of UI-domain state. Mutations arrive as [`ApiCall`]s from a
//! channel, are applied synchronously here, and produce [`UiEvent`]s that the host drains on a
//! coalescing deadline. There is no lock, no interior mutability and no `async` in this type —
//! which is what makes the whole thing unit-testable without a runtime, and what guarantees that
//! two plugins mutating the same buffer see a serialized, well-defined order.
//!
//! Agent-domain calls (`Agent*`, `Tool*`, `Hook*`, `Provider*`, permissions) are *not* handled
//! here; [`Editor::handles`] identifies them so the host can route them to the async agent layer.
//! The plugin sees one API regardless.

use std::collections::{HashMap, HashSet};

use std::collections::BTreeMap;

use neosh_proto::{
    ApiCall, ApiError, ApiOk, ApiResult, BufferId, Contribution, ExtmarkId, ExtmarkOpts,
    FloatConfig, HlTarget, KeyContext, KeyPress, KeymapEntry, KeymapScope, MessageLevel, Mode,
    NamespaceId, NoticeKind, OnDelete, OptionValue, PluginId, Rect, SelectShape, SurfaceId,
    TextEdit, UiEvent, ViewId, VirtTextPos, WindowId, WindowLayout,
};

use crate::buffer::{Buffer, LineEdit};
use crate::focus::FocusStack;
use crate::highlight::{HighlightRegistry, Restored};
use crate::keymap::{Binding, KeyResolution, KeymapTable, Tier, format_keys};
use crate::options::OptionRegistry;
use crate::text;
use crate::window::{Viewport, Window};

/// The keys that reach the workspace even under a modal float, when nothing has said otherwise.
///
/// Quit and reload configuration: the two things you need when a panel is wrong. `ui.modal_escape_keys`
/// replaces this, including with an empty list — see [`Editor::is_modal_escape`].
const MODAL_ESCAPES: &[&str] = &["<C-q>", "<C-r>"];

/// Work the core cannot do itself, drained by the host after every apply.
#[derive(Debug, Clone, PartialEq)]
pub enum CoreEffect {
    /// Route a command to the plugin that registered it.
    InvokeCommand {
        plugin: PluginId,
        name: String,
        args: Vec<String>,
        key: Option<KeyContext>,
    },
    /// A key reached the bottom of the focus stack unclaimed. `<Esc>` arriving here is how an
    /// agent turn gets interrupted without any plugin having to cooperate.
    ///
    /// Carries the terminal it was pressed in, because what the host does with it — put a
    /// character in the composer, scroll the transcript, interrupt the turn on screen — is about
    /// one of them and not about the workspace.
    UnhandledKey { key: KeyPress, mode: Mode, view: ViewId },
    /// A declared option was set or reset. The host acts on the ones it owns and broadcasts all of
    /// them, so a plugin reacting to a setting uses the same mechanism the core does.
    OptionChanged { name: String, value: OptionValue },
    /// A contribution point gained or lost an item. Broadcast by the host so whoever renders the
    /// point redraws, including for a plugin that loaded long after the panel first drew.
    ContributionsChanged { point: String },
    /// Highlight groups changed — defined, reset, or replaced by a theme. Broadcast so a plugin
    /// that cached a colour reads it again.
    HighlightsChanged { names: Vec<String> },
    /// A buffer of this kind now exists. What wakes a plugin whose manifest says `on_kind`.
    KindSeen { kind: String },
}

#[derive(Debug, Clone)]
struct CommandReg {
    plugin: PluginId,
    desc: Option<String>,
}

/// One terminal's place in the workspace.
///
/// Buffers are not in here and never will be: what the agent produced is the workspace's, so a
/// conversation open in two terminals is one transcript with two cursors on it. What *is* in here
/// is everything that answers "where am I" — which window has the keyboard, what the keyboard is
/// currently for, and how far through a chord you are.
///
/// Keymaps, commands, options and highlights are shared for the same reason a buffer is: a binding
/// is registered once and means the same thing wherever you press it. Only where you pressed it
/// differs.
#[derive(Debug)]
struct ViewState {
    focus: FocusStack,
    mode: Mode,
    /// Keys held while a multi-key sequence is still ambiguous.
    ///
    /// Per view, because a half-typed chord is a fact about one keyboard. Shared, `g` typed here
    /// and `g` typed there would make `gg`.
    pending_keys: Vec<KeyPress>,
    /// The window that has this view's keys when nothing has pushed focus — its composer. What
    /// lets a binding on the `neosh.composer` kind resolve at rest. See [`Editor::set_home`].
    home: Option<WindowId>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self { focus: FocusStack::new(), mode: Mode::Normal, pending_keys: Vec::new(), home: None }
    }
}

#[derive(Debug, Default)]
pub struct Editor {
    buffers: HashMap<BufferId, Buffer>,
    windows: HashMap<WindowId, Window>,
    namespaces: HashMap<NamespaceId, String>,
    highlights: HighlightRegistry,
    /// Where each terminal is. Created on demand, so a view nothing has drawn into yet is an empty
    /// place rather than an error, and dropped by [`Editor::close_view`] when its last terminal
    /// goes.
    views: HashMap<ViewId, ViewState>,
    keymaps: KeymapTable,
    commands: HashMap<String, CommandReg>,
    options: OptionRegistry,
    /// Claimed surfaces, and where each one sits. The rectangle is kept, not merely
    /// forwarded, so a client attaching to a workspace that is already running can be told the
    /// surfaces exist. Their *cells* are not kept — see [`Editor::republish`].
    surfaces: HashMap<SurfaceId, (WindowId, Rect)>,
    /// Which plugins asked to hear about a buffer's changes.
    attached: HashMap<BufferId, HashSet<PluginId>>,
    /// Windows that want the keys nothing else claimed, and the command to send them to.
    captures: HashMap<WindowId, String>,
    /// What plugins have put on each other's contribution points, by point name.
    ///
    /// Lives beside commands and keymaps rather than in the host because it is the same kind of
    /// thing: a registration owned by a plugin, that has to disappear when the plugin does. A
    /// contribution the sidebar still renders after its author was unloaded is a row that invokes a
    /// command which no longer exists.
    contributions: HashMap<String, Vec<Contribution>>,
    /// Group-name remaps per window and per buffer kind — Neovim's `winhighlight` — with who set
    /// each, so they go when the plugin does. See [`ApiCall::WinSetHighlights`].
    win_hl: HashMap<WindowId, (String, BTreeMap<String, String>)>,
    kind_hl: HashMap<String, (String, BTreeMap<String, String>)>,
    /// Who outranks whom. A bundled plugin's registrations are *defaults*, `init.ts`'s are the
    /// last word, and a default that overwrites a choice is not a default — `init.ts` runs before
    /// plugin discovery — so without this every plugin would silently take a key the user's
    /// configuration had just bound. One rule for keys, commands and highlights; see [`Tier`].
    tiers: HashMap<String, Tier>,
    /// Commands a higher tier has taken the name of, so that unregistering the winner gives the
    /// name back rather than leaving it unbound. See [`ApiCall::CmdRegister`].
    shadowed: HashMap<String, Vec<CommandReg>>,
    /// Where the selection highlight is drawn.
    ///
    /// Reserved at construction rather than exposed, so a selection is an ordinary extmark from the
    /// frontend's point of view and no rendering code had to learn a new concept. One namespace for
    /// all windows: two views of one buffer both selecting is a real but rare case, and the last
    /// refresh winning beats a per-window namespace nobody can clear.
    selection_ns: NamespaceId,

    /// The frame being built, each event tagged with the view it is about.
    ///
    /// `None` is everybody: buffer contents, highlights, messages. Anything shaped like a window
    /// belongs to one view and is drawn only in the terminals looking at it.
    ui: Vec<(Option<ViewId>, UiEvent)>,
    effects: Vec<CoreEffect>,

    next_buf: u32,
    next_win: u32,
    next_ns: u32,
    next_surface: u32,
}

impl Editor {
    pub fn new() -> Self {
        let mut e = Self {
            highlights: HighlightRegistry::new(),
            next_buf: 1,
            next_win: 1,
            // 1 is taken by the selection namespace below.
            next_ns: 2,
            next_surface: 1,
            selection_ns: NamespaceId(1),
            ..Default::default()
        };
        e.namespaces.insert(e.selection_ns, "neosh.selection".to_string());
        e.ui.push((None, UiEvent::Init { protocol_version: neosh_proto::PROTOCOL_VERSION }));
        for (name, def) in e.highlights.iter() {
            e.ui.push((None, UiEvent::HighlightDefined { name: name.clone(), def: def.clone() }));
        }
        e
    }

    // ---- views -----------------------------------------------------------

    /// One terminal's place, made if it is new.
    fn view(&mut self, view: ViewId) -> &mut ViewState {
        self.views.entry(view).or_default()
    }

    /// What a view has, without making one. A question about a terminal that is not there has the
    /// same answer as one about a terminal that has done nothing yet.
    fn peek(&self, view: ViewId) -> Option<&ViewState> {
        self.views.get(&view)
    }

    /// Hand one view's windows to another id.
    ///
    /// For a terminal reattaching to a workspace nobody was watching: the screen it left is still
    /// there, mid-answer, and giving it to the connection that has just arrived is how it comes
    /// back to exactly what it was rather than to a transcript rebuilt from messages, which is
    /// missing everything the running turn has said and not yet committed.
    pub fn rehome(&mut self, from: ViewId, to: ViewId) {
        if from == to {
            return;
        }
        for w in self.windows.values_mut().filter(|w| w.view == from) {
            w.view = to;
        }
        if let Some(state) = self.views.remove(&from) {
            self.views.insert(to, state);
        }
    }

    /// Close every window in a view except the ones showing `keep`.
    ///
    /// For a screen nobody is attached to any more that is being kept for whoever comes back: the
    /// host's own chrome stays, because that is what "come back to where you were" means, and
    /// everything a plugin put there goes — it is about to be told the view closed, and a panel
    /// left behind would still be on screen when the plugin opens its replacement.
    pub fn close_others_in(&mut self, view: ViewId, keep: &[BufferId]) {
        let doomed: Vec<WindowId> = self
            .windows
            .values()
            .filter(|w| w.view == view && !keep.contains(&w.buf))
            .map(|w| w.id)
            .collect();
        for win in doomed {
            self.windows.remove(&win);
            self.captures.remove(&win);
            self.keymaps.remove_window(win);
            self.surfaces.retain(|_, (w, _)| *w != win);
            if let Some(v) = self.views.get_mut(&view) {
                v.focus.remove(win);
            }
            self.push_ui_in(view, UiEvent::WindowClosed { win });
        }
    }

    /// Forget a view and everything it had open.
    ///
    /// Its windows close — a window belongs to exactly one view, so nothing else is looking at
    /// them — and its buffers do not: those are the workspace's, and the conversation in one of
    /// them may be on screen in the terminal next door.
    pub fn close_view(&mut self, view: ViewId) {
        let mine: Vec<WindowId> = self
            .windows
            .values()
            .filter(|w| w.view == view)
            .map(|w| w.id)
            .collect();
        for win in mine {
            self.windows.remove(&win);
            self.captures.remove(&win);
            self.surfaces.retain(|_, (w, _)| *w != win);
            self.push_ui_in(view, UiEvent::WindowClosed { win });
        }
        self.views.remove(&view);
    }

    /// Say that a plugin ships with neosh, so its keymaps behave as defaults.
    pub fn mark_bundled(&mut self, plugin: &PluginId) {
        self.tiers.insert(plugin.0.clone(), Tier::Builtin);
    }

    /// Say that a plugin is the user's own configuration, so its choices outrank every plugin's.
    pub fn mark_user(&mut self, plugin: &PluginId) {
        self.tiers.insert(plugin.0.clone(), Tier::User);
    }

    pub fn tier(&self, plugin: &str) -> Tier {
        self.tiers.get(plugin).copied().unwrap_or_default()
    }

    /// Every point anything has contributed to, with who contributed. Sorted, for a listing.
    pub fn contributors(&self) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = self
            .contributions
            .iter()
            .filter(|(_, list)| !list.is_empty())
            .map(|(point, list)| {
                let mut who: Vec<String> = list.iter().map(|c| c.plugin.clone()).collect();
                who.sort();
                who.dedup();
                (point.clone(), who)
            })
            .collect();
        out.sort();
        out
    }

    /// Which plugin registered a command, if anything has.
    pub fn command_owner(&self, name: &str) -> Option<PluginId> {
        self.commands.get(name).map(|r| r.plugin.clone())
    }

    /// Whether this call is UI-domain and belongs to the core.
    ///
    /// The host uses this to split synchronous UI work from asynchronous agent work without the
    /// plugin API having two shapes.
    pub fn handles(call: &ApiCall) -> bool {
        !matches!(
            call,
            ApiCall::AgentSend { .. }
                | ApiCall::AgentCommand { .. }
                | ApiCall::AgentCancel
                | ApiCall::AgentGetSelection
                | ApiCall::AgentSetSelection { .. }
                | ApiCall::AgentListModels { .. }
                | ApiCall::AgentListInstances
                | ApiCall::AgentDriverCommands
                | ApiCall::ChatSetDraft { .. }
                | ApiCall::ChatAttach { .. }
                | ApiCall::ChatAttachments
                | ApiCall::ChatDetach { .. }
                | ApiCall::ChatDetachAll
                | ApiCall::ToolRegister { .. }
                | ApiCall::ToolUnregister { .. }
                | ApiCall::ToolList
                | ApiCall::HookRegister { .. }
                | ApiCall::HookUnregister { .. }
                | ApiCall::ProviderRegisterDriver { .. }
                | ApiCall::ProviderEmit { .. }
                | ApiCall::PermissionCheck { .. }
                | ApiCall::AskUser { .. }
                | ApiCall::PermissionGetMode
                | ApiCall::PermissionSetMode { .. }
                | ApiCall::RtpAdd { .. }
                | ApiCall::RtpList
                | ApiCall::PathComplete { .. }
                | ApiCall::GitStatus { .. }
                | ApiCall::GitBranches { .. }
                | ApiCall::GitWorktrees { .. }
                | ApiCall::GitLog { .. }
                | ApiCall::GitDiff { .. }
                | ApiCall::GitDefaultBranch
                | ApiCall::GitCreateBranch { .. }
                | ApiCall::GitCheckout { .. }
                | ApiCall::GitRenameBranch { .. }
                | ApiCall::GitStage { .. }
                | ApiCall::GitUnstage { .. }
                | ApiCall::GitCommit { .. }
                | ApiCall::GitPull { .. }
                | ApiCall::GitAddWorktree { .. }
                | ApiCall::GitRemoveWorktree { .. }
                | ApiCall::GenComplete { .. }
                | ApiCall::SessionList { .. }
                | ApiCall::ViewList
                | ApiCall::SessionCurrent
                | ApiCall::SessionNew { .. }
                | ApiCall::SessionSwitch { .. }
                | ApiCall::SessionClose { .. }
                | ApiCall::SessionRename { .. }
                | ApiCall::SessionArchive { .. }
                | ApiCall::ProviderCredentials
                | ApiCall::ProviderSetCredential { .. }
                | ApiCall::ProviderForgetCredential { .. }
                | ApiCall::SessionMessages { .. }
                | ApiCall::SessionsStored
                | ApiCall::StatusSet { .. }
                | ApiCall::StatusClear { .. }
                | ApiCall::HintSet { .. }
                | ApiCall::HintClear { .. }
                | ApiCall::StateGet { .. }
                | ApiCall::StateSet { .. }
                | ApiCall::StateDelete { .. }
                // Vars are persisted and scoped to conversations and projects, neither of which
                // the core knows about. Contributions are *not* here: they are a registration
                // owned by a plugin, which is core work, the same as a command or a keymap.
                | ApiCall::VarGet { .. }
                | ApiCall::VarSet { .. }
                | ApiCall::VarDelete { .. }
                | ApiCall::VarAll { .. }
                // Emitting is a broadcast to every plugin, and the bridge is what holds them.
                | ApiCall::EventEmit { .. }
                // A call that waits for a plugin's answer: the core knows who owns the name
                // (`command_owner`), the host is what can ask and wait.
                | ApiCall::CmdCall { .. }
                // Who *reads* a point is in the manifests, which the host holds; the core only
                // knows who wrote to one (`contributors`).
                | ApiCall::ExtPoints
                | ApiCall::PluginList
                // The swarm is sockets and other machines. None of it is UI state.
                | ApiCall::SwarmSelf
                | ApiCall::SwarmNodes
                | ApiCall::SwarmAgents
                | ApiCall::SwarmHostsOf { .. }
                | ApiCall::SwarmCommand { .. }
                | ApiCall::SwarmSubscribe { .. }
                | ApiCall::SwarmUnsubscribe { .. }
                | ApiCall::SwarmProbe { .. }
                | ApiCall::SwarmPair { .. }
                | ApiCall::SwarmUnpair { .. }
                | ApiCall::SwarmStrangers
                // The plan's allowance: credentials, other programs' transcripts and a clock.
                // Nothing here is UI state, and the store outlives every window that draws it.
                | ApiCall::QuotaList
                | ApiCall::QuotaRefresh { .. }
                | ApiCall::QuotaReport { .. }
                | ApiCall::QuotaHistory { .. }
                | ApiCall::UsageHistory { .. }
                // Whether an alert may leave the terminal depends on which conversation is on
                // screen, which views are attached and whether any of them has focus. The core
                // knows none of those: a view is a socket and focus is a fact about somebody
                // else's window manager. It draws the corner and the host decides the rest.
                // See ADR 0057.
                | ApiCall::Alert { .. }
        )
    }

    // ---- draining -------------------------------------------------------

    /// Take the frame so far — every event, each tagged with the view it is about, `None` meaning
    /// everybody. The host calls this on a ~16 ms deadline armed by the first mutation, then
    /// appends [`UiEvent::Flush`].
    pub fn drain_ui(&mut self) -> Vec<(Option<ViewId>, UiEvent)> {
        std::mem::take(&mut self.ui)
    }

    /// The frame's events with their tags dropped.
    ///
    /// For a caller that is one terminal by construction — the tests, and anything asking what was
    /// drawn rather than where it went.
    pub fn drain_events(&mut self) -> Vec<UiEvent> {
        self.drain_ui().into_iter().map(|(_, ev)| ev).collect()
    }

    /// Put back events that were drained and not used.
    ///
    /// The one caller is a terminal attaching: the frame in hand is dropped and said again in
    /// full, but only the part of it that was for the terminal arriving. What was already queued
    /// for somebody else is still owed to them.
    pub fn requeue(&mut self, events: Vec<(Option<ViewId>, UiEvent)>) {
        let rest = std::mem::replace(&mut self.ui, events);
        self.ui.extend(rest);
    }

    pub fn drain_effects(&mut self) -> Vec<CoreEffect> {
        std::mem::take(&mut self.effects)
    }

    pub fn has_pending_ui(&self) -> bool {
        !self.ui.is_empty()
    }

    /// Swap the colour theme, re-announcing every group the frontend needs to know about.
    ///
    /// Returns false when nothing changed. Highlights are pushed rather than pulled, so a switch
    /// has to re-emit: a frontend holds a copy and has no way to ask for one.
    pub fn set_theme(&mut self, variant: crate::palette::Variant) -> bool {
        if !self.highlights.set_variant(variant) {
            return false;
        }
        self.republish_highlights();
        self.announce_all_highlights();
        true
    }

    /// Use a theme somebody contributed. See [`HighlightRegistry::set_custom`].
    pub fn set_custom_theme(
        &mut self,
        name: &str,
        base: crate::palette::Variant,
        groups: Vec<(String, neosh_proto::HighlightDef)>,
    ) -> bool {
        let before: Vec<String> = self.highlights.iter().map(|(n, _)| n.clone()).collect();
        if !self.highlights.set_custom(name, base, groups) {
            return false;
        }
        // Groups the previous theme had and this one does not are cleared on the frontend too,
        // or a link into one of them keeps resolving to a colour the theme no longer has.
        for name in before {
            if self.highlights.get(&name).is_none() {
                self.push_ui(UiEvent::HighlightCleared { name });
            }
        }
        self.republish_highlights();
        self.announce_all_highlights();
        true
    }

    /// The contributed theme in use, if any.
    pub fn custom_theme(&self) -> Option<&str> {
        self.highlights.custom()
    }

    fn announce_all_highlights(&mut self) {
        let mut names: Vec<String> = self.highlights.iter().map(|(n, _)| n.clone()).collect();
        names.sort();
        self.effects.push(CoreEffect::HighlightsChanged { names });
    }

    /// Everything a window's groups are read through: its kind's remap under its own.
    fn window_highlights(&self, win: WindowId) -> BTreeMap<String, String> {
        let Some(w) = self.windows.get(&win) else { return BTreeMap::new() };
        let mut map = self
            .buffers
            .get(&w.buf)
            .and_then(|b| b.kind.as_deref())
            .and_then(|k| self.kind_hl.get(k))
            .map(|(_, m)| m.clone())
            .unwrap_or_default();
        if let Some((_, own)) = self.win_hl.get(&win) {
            map.extend(own.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        map
    }

    /// Say again how a window reads its groups. Called whenever what feeds into it moved: the
    /// remap itself, the window's buffer, or that buffer's kind.
    fn republish_window_highlights(&mut self, win: WindowId) {
        if !self.windows.contains_key(&win) {
            return;
        }
        let map = self.window_highlights(win);
        self.push_ui(UiEvent::WindowHighlights { win, map });
    }

    /// Every window showing `buf`, for when the buffer's kind changes under them.
    fn republish_highlights_of_buffer(&mut self, buf: BufferId) {
        let wins: Vec<WindowId> =
            self.windows.values().filter(|w| w.buf == buf).map(|w| w.id).collect();
        for win in wins {
            self.republish_window_highlights(win);
        }
    }

    /// Turn motion on or off. Returns whether anything changed.
    pub fn set_motion(&mut self, on: bool) -> bool {
        if !self.highlights.set_motion(on) {
            return false;
        }
        self.republish_highlights();
        self.announce_all_highlights();
        true
    }

    /// Say everything, to a frontend that has never heard any of it.
    ///
    /// The protocol is deltas, which is the right shape for a frontend that has been listening
    /// since the process started and the wrong shape for one that attaches to a workspace already
    /// half a day into its work. This is the other half of that bargain: the state is all here, so
    /// it can be said again in full, and a client that folds this in has exactly the mirror a
    /// client that had been there all along would have.
    ///
    /// Emitted in the order a frontend needs it — highlights before the buffers that reference
    /// them, buffers before the windows that show them — and windows in id order, which is the
    /// order they were opened in, because a frontend that lays out by arrival order would
    /// otherwise get a different screen every time somebody reattached.
    ///
    /// **A surface's cells are not republished.** The editor forwards them and does not keep them:
    /// they are a grid a plugin owns, and re-emitting a copy would mean holding a second one that
    /// is wrong the moment the plugin draws again. The claim is republished, and
    /// [`crate::CoreEffect::ViewAttached`] tells the plugin to paint.
    /// Said *to one view*, because half of it is about one view: which windows are open, where
    /// their cursors are, what has the keyboard. The buffers are the workspace's and are said again
    /// regardless — a terminal joining is also the moment every other terminal is brought back
    /// into step, and a mirror that had drifted is repaired by hearing the state a second time.
    pub fn republish(&mut self, view: ViewId) {
        self.push_ui_in(view, UiEvent::Init { protocol_version: neosh_proto::PROTOCOL_VERSION });
        self.republish_highlights();

        // What this terminal can see, and nothing else. Every buffer used to be said to everyone,
        // which was right when a buffer was the workspace's — and is a transcript, a composer and
        // a status line per terminal now, so it put the conversation you are reading and the
        // sentence you are half-way through typing into the mirror of the window next door.
        //
        // A buffer with no window anywhere is included: its edits went to every terminal attached
        // at the time, which this one was not, and a plugin filling a buffer before opening a
        // window on it is ordinary.
        let mut buffers: Vec<BufferId> = self
            .buffers
            .keys()
            .copied()
            .filter(|buf| {
                let mut windows = self.windows.values().filter(|w| w.buf == *buf);
                match windows.next() {
                    None => true,
                    Some(first) => first.view == view || windows.any(|w| w.view == view),
                }
            })
            .collect();
        buffers.sort_by_key(|b| b.0);
        for buf in buffers {
            let Some(b) = self.buffers.get(&buf) else { continue };
            let (name, count) = (b.name.clone(), b.line_count());
            self.push_ui_in(view, UiEvent::BufferOpened { buf, name });
            // `old_end: u32::MAX` — "however many rows you have, they are these now". Not `0`,
            // which would be an insertion at the top: correct for a mirror that has nothing and
            // silently doubling for one that already has the state. A mirror that already has it
            // is the ordinary case now that a workspace can have several views: a terminal
            // joining republishes to *all* of them, which is what brings the ones already here
            // back into step. Not `count` either, because a mirror can be holding more rows than
            // the buffer now has. The mirror clamps the range, so this is exactly "replace it all"
            // at either end.
            let lines = self.buffers[&buf].render_range(0, count);
            self.push_ui_in(view, UiEvent::BufferLines { buf, start: 0, old_end: u32::MAX, lines });
        }

        // This view's windows and no others. A terminal has no use for the geometry of a panel
        // open in the terminal next door, and telling it would put a window on its screen that
        // nothing there can close.
        let mut windows: Vec<WindowId> = self
            .windows
            .values()
            .filter(|w| w.view == view)
            .map(|w| w.id)
            .collect();
        windows.sort_by_key(|w| w.0);
        for win in windows {
            let Some(w) = self.windows.get(&win) else { continue };
            let (buf, layout, cursor, top) = (w.buf, w.layout.clone(), w.cursor, w.top_line);
            let shape = w.cursor_shape;
            self.push_ui_in(view, UiEvent::WindowOpened { win, buf, layout });
            self.push_ui_in(view, UiEvent::CursorMoved { win, row: cursor.0, col: cursor.1 });
            self.push_ui_in(view, UiEvent::ScrollTo { win, top_line: top });
            self.push_ui_in(view, UiEvent::CursorShapeChanged { win, shape });
            let map = self.window_highlights(win);
            if !map.is_empty() {
                self.push_ui_in(view, UiEvent::WindowHighlights { win, map });
            }
        }

        let mut surfaces: Vec<SurfaceId> = self
            .surfaces
            .iter()
            .filter(|(_, (win, _))| self.windows.get(win).is_some_and(|w| w.view == view))
            .map(|(s, _)| *s)
            .collect();
        surfaces.sort_by_key(|s| s.0);
        for surface in surfaces {
            let Some((win, rect)) = self.surfaces.get(&surface).copied() else { continue };
            self.push_ui_in(view, UiEvent::SurfaceClaimed { surface, win, rect });
        }

        let focus = self.peek(view).and_then(|v| v.focus.current());
        self.push_ui_in(view, UiEvent::FocusChanged { win: focus });
    }

    fn republish_highlights(&mut self) {
        let defs: Vec<_> = self.highlights.iter().map(|(n, d)| (n.clone(), d.clone())).collect();
        for (name, def) in defs {
            self.push_ui(UiEvent::HighlightDefined { name, def });
        }
    }

    /// Queue a UI event, working out for itself which terminals it is about.
    ///
    /// Anything shaped like a window names one, and a window belongs to exactly one view — so the
    /// answer is read off the window rather than passed in at two dozen call sites, where the one
    /// that was forgotten would be a panel drawn on somebody else's screen. The events that name
    /// no window are the workspace's: buffer contents, highlights, messages, the clipboard.
    ///
    /// Two cases cannot be derived and say so by calling [`Editor::push_ui_in`]: a window that has
    /// just been *closed* is no longer in the map to be asked, and focus becoming `None` names no
    /// window at all.
    ///
    /// Consecutive edits to the same buffer region are coalesced. Streaming produces one
    /// `BufferLines` per token, all rewriting the same final line; without this merge the frontend
    /// would receive hundreds of redundant events per frame.
    fn push_ui(&mut self, ev: UiEvent) {
        let view = self.about(&ev);
        self.queue(view, ev);
    }

    /// Queue a UI event about a terminal that has to be named — because the window it concerns has
    /// gone, or because there is no window in it to read the answer off.
    fn push_ui_in(&mut self, view: ViewId, ev: UiEvent) {
        self.queue(Some(view), ev);
    }

    /// Which terminals an event is about. `None` is everybody.
    fn about(&self, ev: &UiEvent) -> Option<ViewId> {
        let win = match ev {
            UiEvent::WindowOpened { win, .. }
            | UiEvent::WindowConfigured { win, .. }
            | UiEvent::WindowBuffer { win, .. }
            | UiEvent::WindowClosed { win }
            | UiEvent::CursorMoved { win, .. }
            | UiEvent::CursorShapeChanged { win, .. }
            | UiEvent::ScrollTo { win, .. }
            | UiEvent::WindowHighlights { win, .. }
            | UiEvent::SurfaceClaimed { win, .. } => *win,
            UiEvent::FocusChanged { win: Some(win) } => *win,
            UiEvent::SurfaceCells { surface, .. } | UiEvent::SurfaceReleased { surface } => {
                match self.surfaces.get(surface) {
                    Some((win, _)) => *win,
                    None => return None,
                }
            }
            // A buffer is the workspace's, but with one window on it there is exactly one terminal
            // that can see it — and a transcript is per view, so that is nearly all of them. Sent
            // to everybody, every terminal would hold a copy of every other terminal's
            // conversation, and a frontend looking a buffer up by name would find three called
            // `[chat]`.
            //
            // Zero windows or several, and it goes to everybody: a plugin that fills a buffer
            // before opening a window on it is ordinary, and a mirror missing the rows it was
            // filled with would draw an empty panel.
            UiEvent::BufferOpened { buf, .. }
            | UiEvent::BufferLines { buf, .. }
            | UiEvent::BufferClosed { buf } => {
                let mut showing = self.windows.values().filter(|w| w.buf == *buf);
                return match (showing.next(), showing.next()) {
                    (Some(w), None) => Some(w.view),
                    _ => None,
                };
            }
            _ => return None,
        };
        self.windows.get(&win).map(|w| w.view)
    }

    fn queue(&mut self, view: Option<ViewId>, ev: UiEvent) {
        if let UiEvent::BufferLines { buf, start, old_end, lines } = &ev
            && let Some((pview, UiEvent::BufferLines {
                buf: pbuf,
                start: pstart,
                lines: plines,
                ..
            })) = self.ui.last_mut()
            && pbuf == buf
            && *pview == view
        {
            // Mergeable only when the new edit lands entirely inside the region the previous
            // event already rewrote; otherwise the mirror would need both splices.
            let prev_end = *pstart + plines.len() as u32;
            if *start >= *pstart && *old_end <= prev_end {
                let lo = (*start - *pstart) as usize;
                let hi = (*old_end - *pstart) as usize;
                plines.splice(lo..hi, lines.clone());
                return;
            }
        }
        self.ui.push((view, ev));
    }

    fn emit_edit(&mut self, buf: BufferId, edit: LineEdit) {
        self.push_ui(UiEvent::BufferLines {
            buf,
            start: edit.start,
            old_end: edit.old_end,
            lines: edit.lines,
        });
    }

    /// Re-send a single row, used after a mark changes without the text changing.
    fn emit_row(&mut self, buf: BufferId, row: u32) {
        let Some(b) = self.buffers.get(&buf) else { return };
        let lines = b.render_range(row, row + 1);
        if lines.is_empty() {
            return;
        }
        self.push_ui(UiEvent::BufferLines { buf, start: row, old_end: row + 1, lines });
    }

    fn emit_rows(&mut self, buf: BufferId, start: u32, end: u32) {
        let Some(b) = self.buffers.get(&buf) else { return };
        let lines = b.render_range(start, end);
        self.push_ui(UiEvent::BufferLines { buf, start, old_end: end, lines });
    }

    // ---- accessors used by the host and by tests -------------------------

    pub fn buffer(&self, id: BufferId) -> Option<&Buffer> {
        self.buffers.get(&id)
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    /// Every open window, for the rare caller that needs to find one by what it shows.
    pub fn windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.values()
    }

    /// What the keyboard is for, in one terminal.
    ///
    /// Per view because a mode is what the *keyboard* is doing: reading the transcript here while
    /// typing a message there is two people's worth of one workspace, and one `Mode` between them
    /// would make `j` a letter in whichever terminal lost the argument.
    pub fn mode(&self, view: ViewId) -> Mode {
        self.peek(view).map_or(Mode::Normal, |v| v.mode)
    }

    pub fn set_mode(&mut self, view: ViewId, mode: Mode) {
        let v = self.view(view);
        v.mode = mode;
        v.pending_keys.clear();
    }

    pub fn focused(&self, view: ViewId) -> Option<WindowId> {
        self.peek(view).and_then(|v| v.focus.current())
    }

    /// Which terminal a window is in. `None` for a window that has been closed.
    pub fn view_of(&self, win: WindowId) -> Option<ViewId> {
        self.windows.get(&win).map(|w| w.view)
    }

    /// Say which window has a view's keys when nothing holds focus — its composer.
    ///
    /// The focus stack is for panels and floats that *take* the keyboard; at rest, nothing is on
    /// it and the keys go to the field you type in. That field is a buffer with a kind, and a
    /// key bound against `neosh.composer` has to resolve at rest, which means resolution needs a
    /// window to read the kind off. Window- and buffer-scoped bindings on it resolve too.
    pub fn set_home(&mut self, view: ViewId, win: Option<WindowId>) {
        self.view(view).home = win;
    }

    fn home(&self, view: ViewId) -> Option<WindowId> {
        self.peek(view).and_then(|v| v.home)
    }

    pub fn options(&self) -> &OptionRegistry {
        &self.options
    }

    pub fn highlights(&self) -> &HighlightRegistry {
        &self.highlights
    }

    /// Create a buffer outside the plugin API, for host-owned UI such as the chat pane.
    pub fn create_buffer(&mut self, name: &str) -> BufferId {
        let id = BufferId(self.next_buf);
        self.next_buf += 1;
        self.buffers.insert(id, Buffer::new(id, name));
        self.push_ui(UiEvent::BufferOpened { buf: id, name: name.to_string() });
        id
    }

    /// A host-owned buffer with a published kind.
    ///
    /// The transcript, the composer and the status line are the three surfaces a plugin most
    /// wants to put a key on or find, and they had no kind — so the host's own UI was the one
    /// part of the workspace ADR 0040's promise did not reach. `neosh.transcript`,
    /// `neosh.composer`, `neosh.status`: bind at `buf_kind` scope, find with `win.ofKind`,
    /// remap with `win.setHighlights`, like any panel.
    pub fn create_buffer_of_kind(&mut self, name: &str, kind: &str) -> BufferId {
        let id = self.create_buffer(name);
        if let Some(b) = self.buffers.get_mut(&id) {
            b.kind = Some(kind.to_string());
        }
        id
    }

    /// Open a window in the one view a lone process has. See [`Editor::apply`] for why the two
    /// spellings exist.
    pub fn open_window(&mut self, buf: BufferId, layout: WindowLayout) -> WindowId {
        self.open_window_in(ViewId::LOCAL, buf, layout)
    }

    /// Open a window in a named terminal.
    pub fn open_window_in(
        &mut self,
        view: ViewId,
        buf: BufferId,
        layout: WindowLayout,
    ) -> WindowId {
        let id = WindowId(self.next_win);
        self.next_win += 1;
        // Before the window, and to this view alone: a buffer's rows are routed to whoever has a
        // window on it, so everything written into this one while another terminal was the only
        // one showing it never came here. Costs one event on a rare path and removes the whole
        // class of "the panel opened empty in the second window".
        self.show_buffer_in(view, buf);
        self.windows.insert(id, Window::new(id, view, buf, layout.clone()));
        // Made even if nothing focuses it, so that "which views are there" is answered by the
        // same map whether a terminal has taken a key yet or only been drawn into.
        self.views.entry(view).or_default();
        self.push_ui_in(view, UiEvent::WindowOpened { win: id, buf, layout });
        // A remap on the buffer's kind applies to this window from its first frame.
        let map = self.window_highlights(id);
        if !map.is_empty() {
            self.push_ui_in(view, UiEvent::WindowHighlights { win: id, map });
        }
        id
    }

    /// Say a buffer's whole contents to one terminal, for one that is about to start showing it.
    ///
    /// Only when somebody else is already showing it. A buffer with no window has had every one of
    /// its edits broadcast — that is what [`Editor::about`] does with one nobody can see — so the
    /// arriving terminal already has them, and saying them again would be a "replace everything"
    /// for content the mirror is holding correctly.
    fn show_buffer_in(&mut self, view: ViewId, buf: BufferId) {
        if !self.windows.values().any(|w| w.buf == buf) {
            return;
        }
        let Some(b) = self.buffers.get(&buf) else { return };
        let (name, count) = (b.name.clone(), b.line_count());
        let lines = b.render_range(0, count);
        self.push_ui_in(view, UiEvent::BufferOpened { buf, name });
        self.push_ui_in(view, UiEvent::BufferLines { buf, start: 0, old_end: u32::MAX, lines });
    }

    /// Record realized geometry reported by the frontend.
    pub fn set_viewport(&mut self, win: WindowId, vp: Viewport) {
        if let Some(w) = self.windows.get_mut(&win) {
            w.viewport = Some(vp);
        }
    }

    pub fn plugins_attached_to(&self, buf: BufferId) -> impl Iterator<Item = &PluginId> {
        self.attached.get(&buf).into_iter().flatten()
    }

    // ---- key input -------------------------------------------------------

    /// Scopes to consult, most specific first.
    ///
    /// Kind sits between buffer and global, and that ordering is the point of it. A binding on
    /// *this* buffer is a statement about one thing on screen and has to beat one about every
    /// sidebar there will ever be; a binding on every sidebar has to beat one about the whole
    /// workspace, or a panel could never take a key back from a global default.
    fn active_scopes(&self, view: ViewId) -> Vec<KeymapScope> {
        let mut scopes = Vec::new();
        // Nothing holding focus means the view's home window — its composer — has the keys, and
        // a binding on *its* kind has to resolve there or `neosh.composer` is a kind nothing can
        // bind against.
        if let Some(win) = self.focused(view).or_else(|| self.home(view)) {
            scopes.push(KeymapScope::Window { win });
            if let Some(w) = self.windows.get(&win) {
                scopes.push(KeymapScope::Buffer { buf: w.buf });
                if let Some(kind) = self.buffers.get(&w.buf).and_then(|b| b.kind.clone()) {
                    scopes.push(KeymapScope::BufKind { name: kind });
                }
            }
        }
        if !self.focus_is_modal(view) {
            scopes.push(KeymapScope::Global);
        }
        scopes
    }

    /// Whether the focused window has declared itself modal. See [`FloatConfig::modal`].
    fn focus_is_modal(&self, view: ViewId) -> bool {
        self.focused(view)
            .and_then(|win| self.windows.get(&win))
            .is_some_and(crate::window::Window::modal)
    }

    /// The keys that resolve globally even under a modal.
    ///
    /// Read out of `ui.modal_escape_keys` on every press rather than cached, because it is a
    /// setting and `^R` reloads settings — and a stale copy of *this* list is the one that cannot
    /// be corrected without restarting, since the key to reload is on it.
    fn is_modal_escape(&self, seq: &[KeyPress]) -> bool {
        let keys = match self.options.get("ui.modal_escape_keys") {
            Some(neosh_proto::OptionValue::List(keys)) => keys.clone(),
            // Absent is not the same as empty. Absent means nothing has *declared* the option —
            // a bare `Editor`, a frontend that never registered it — and falling through to "no
            // escapes" there would make the one thing standing between a buggy panel and a
            // terminal you have to kill contingent on somebody having remembered to declare a
            // default. An empty list is honoured, because that is a person saying so.
            _ => MODAL_ESCAPES.iter().map(|k| (*k).to_string()).collect(),
        };
        keys.iter()
            .filter_map(|lhs| crate::keymap::parse_keys(&self.with_leader(lhs)).ok())
            .any(|k| k == seq)
    }

    /// Whatever `mapleader` is, or Neovim's default.
    fn leader(&self) -> String {
        self.options
            .str("mapleader")
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "\\".to_string())
    }

    fn with_leader(&self, lhs: &str) -> String {
        crate::keymap::expand_leader(lhs, &self.leader())
    }

    /// Feed one key press.
    ///
    /// Routing is resolved entirely from the core's own tables — no round trip into a plugin
    /// runtime — so a keystroke never waits on plugin latency to find out where it goes.
    pub fn feed_key(&mut self, view: ViewId, key: KeyPress) -> KeyResolution {
        self.view(view).pending_keys.push(key.clone());
        let scopes = self.active_scopes(view);
        let mode = self.mode(view);
        let seq = self.view(view).pending_keys.clone();
        let mut res = self.keymaps.resolve(mode, &scopes, &seq);
        // A modal drops `Global` from its scopes, so the escape hatches have to be let back in by
        // name. Consulted only once the panel's own scopes have declined the key: a modal that
        // binds `<C-r>` for something of its own keeps it, which is the same rule every other
        // scope follows.
        if matches!(res, KeyResolution::Unhandled)
            && self.focus_is_modal(view)
            && self.is_modal_escape(&seq)
        {
            res = self.keymaps.resolve(mode, &[KeymapScope::Global], &seq);
        }
        match &res {
            KeyResolution::Pending => {}
            KeyResolution::Matched { command, .. } => {
                self.view(view).pending_keys.clear();
                let cmd = command.clone();
                let ctx =
                    KeyContext { key, mode, view, win: self.focused(view) };
                self.invoke_command(&cmd, Vec::new(), Some(ctx));
            }
            KeyResolution::Unhandled => {
                // Every key the abandoned prefix was holding, in order, then the one that broke it.
                //
                // Dropping the prefix instead would mean that with `gd` bound, typing `gx` produces
                // `x` — the composer eats characters and there is no way to tell why.
                let held = std::mem::take(&mut self.view(view).pending_keys);
                for k in held {
                    self.unclaimed(view, k);
                }
            }
        }
        res
    }

    /// Abandon a half-typed sequence, replaying what it was holding as ordinary input.
    ///
    /// The other half of `Pending`: without a way out, binding a sequence whose prefix is a
    /// printable character makes that character untypeable. The host calls this on a `timeoutlen`
    /// deadline, which is how Neovim resolves the same ambiguity.
    pub fn flush_pending(&mut self, view: ViewId) -> bool {
        let held = std::mem::take(&mut self.view(view).pending_keys);
        if held.is_empty() {
            return false;
        }
        for k in held {
            self.unclaimed(view, k);
        }
        true
    }

    /// A key no binding wanted.
    ///
    /// A capture claims what the keymaps did not, so a picker gets its filter keys while `<C-q>`
    /// still quits. Checked here rather than before `resolve` for exactly that reason: a capture
    /// that swallowed every binding would be a trap.
    ///
    /// Under a [modal](neosh_proto::FloatConfig::modal) with no capture the key stops here. It has
    /// already been offered to the panel's own scopes and to the escape list and neither wanted
    /// it; the only place left is the host, which would put a character in the composer behind the
    /// float or read `<Esc>` as "interrupt the turn". Typing into a field you cannot see is worse
    /// than a key that does nothing.
    fn unclaimed(&mut self, view: ViewId, key: KeyPress) {
        let focus = self.focused(view);
        let mode = self.mode(view);
        let captured = focus.and_then(|w| self.captures.get(&w).cloned());
        match captured {
            Some(command) => {
                let ctx = KeyContext { key, mode, view, win: focus };
                self.invoke_command(&command, Vec::new(), Some(ctx));
            }
            None if self.focus_is_modal(view) => {}
            None => self.effects.push(CoreEffect::UnhandledKey { key, mode, view }),
        }
    }

    /// Whether anything has registered this name.
    ///
    /// Asked by the host before it decides that a command nothing answers to is *unknown* rather
    /// than merely not loaded yet — plugins register theirs as they activate, and for the first
    /// moment of a session most of them have not.
    pub fn has_command(&self, name: &str) -> bool {
        self.commands.contains_key(name)
    }

    /// Run a registered command by name, as a frontend's menu or palette entry does.
    ///
    /// Public because a frontend that can only send keys cannot have a button — every entry in a
    /// menu is a command, and synthesising whatever key it happens to be bound to breaks the moment
    /// the user rebinds it.
    pub fn exec_command(&mut self, name: &str, args: Vec<String>) {
        self.invoke_command(name, args, None);
    }

    fn invoke_command(&mut self, name: &str, args: Vec<String>, key: Option<KeyContext>) {
        match self.commands.get(name) {
            Some(reg) => self.effects.push(CoreEffect::InvokeCommand {
                plugin: reg.plugin.clone(),
                name: name.to_string(),
                args,
                key,
            }),
            // A key you pressed, and the answer is that it is bound to nothing that exists.
            None => self.push_ui(UiEvent::Message {
                level: MessageLevel::Error,
                text: format!("no such command: {name}"),
                kind: NoticeKind::Reply,
                key: None,
            }),
        }
    }

    /// Drop everything a plugin registered. Called on unload or crash so a dead plugin cannot keep
    /// owning commands and keys.
    pub fn remove_plugin(&mut self, plugin: &PluginId) {
        // Captures name a command; when the command goes, so must the capture, or every unbound key
        // resolves to "no such command" for as long as the window stays focused.
        let gone: Vec<String> = self
            .commands
            .iter()
            .filter(|(_, r)| &r.plugin == plugin)
            .map(|(n, _)| n.clone())
            .collect();
        self.captures.retain(|_, cmd| !gone.contains(cmd));
        self.commands.retain(|_, r| &r.plugin != plugin);
        // Names it had taken go back to whoever it took them from; its own waiting registrations
        // go away.
        for name in gone {
            if let Some(back) = self.shadowed.get_mut(&name).and_then(|v| v.pop()) {
                self.commands.insert(name, back);
            }
        }
        for list in self.shadowed.values_mut() {
            list.retain(|r| &r.plugin != plugin);
        }
        self.keymaps.remove_owner(&plugin.0);
        self.options.remove_owner(&plugin.0);
        // Its rows go with it. A contribution outliving its author is a row in somebody's panel
        // pointing at a command that no longer exists — which is also what makes
        // `plugins.disabled` mean what it says for a plugin that only ever contributed.
        let mut emptied = Vec::new();
        for (point, list) in self.contributions.iter_mut() {
            let before = list.len();
            list.retain(|c| c.plugin != plugin.0);
            if list.len() != before {
                emptied.push(point.clone());
            }
        }
        for point in emptied {
            self.effects.push(CoreEffect::ContributionsChanged { point });
        }
        for set in self.attached.values_mut() {
            set.remove(plugin);
        }
        // Its colours. A group a disabled plugin defined used to outlive it for the session —
        // and an override deleted from `init.ts` survived `^R` — because nothing here knew whose
        // it was.
        let restored = self.highlights.remove_owner(&plugin.0);
        if !restored.is_empty() {
            let names = restored.iter().map(|(n, _)| n.clone()).collect();
            for (name, what) in restored {
                match what {
                    Restored::Theme(def) => self.push_ui(UiEvent::HighlightDefined { name, def }),
                    Restored::Cleared => self.push_ui(UiEvent::HighlightCleared { name }),
                }
            }
            self.effects.push(CoreEffect::HighlightsChanged { names });
        }
        let wins: Vec<WindowId> = self
            .win_hl
            .iter()
            .filter(|(_, (o, _))| o == &plugin.0)
            .map(|(w, _)| *w)
            .collect();
        let kinds: Vec<String> = self
            .kind_hl
            .iter()
            .filter(|(_, (o, _))| o == &plugin.0)
            .map(|(k, _)| k.clone())
            .collect();
        if !wins.is_empty() || !kinds.is_empty() {
            self.win_hl.retain(|_, (o, _)| o != &plugin.0);
            self.kind_hl.retain(|_, (o, _)| o != &plugin.0);
            let mut affected: Vec<WindowId> = wins;
            for w in self.windows.values() {
                let kind = self.buffers.get(&w.buf).and_then(|b| b.kind.as_deref());
                if kind.is_some_and(|k| kinds.iter().any(|x| x == k)) {
                    affected.push(w.id);
                }
            }
            affected.sort_by_key(|w| w.0);
            affected.dedup();
            for win in affected {
                self.republish_window_highlights(win);
            }
        }
    }

    // ---- the dispatcher --------------------------------------------------

    /// Apply a call on behalf of the one view a lone process has.
    ///
    /// The great majority of callers are the host drawing its own chrome and tests driving the
    /// editor directly, and both of those are one terminal. Anything that has to answer *which*
    /// terminal — a plugin's float, a key press — calls [`Editor::apply_in`], and the split is
    /// deliberately visible so that a call site which ought to have said is greppable rather than
    /// silently correct-looking.
    pub fn apply(&mut self, plugin: &PluginId, call: ApiCall) -> ApiResult {
        self.apply_in(ViewId::LOCAL, plugin, call)
    }

    /// Apply a call on behalf of the terminal that caused it.
    ///
    /// The view matters for the handful of calls that are about a *place* rather than about a
    /// thing: opening a window, taking or giving up focus, asking what has it. Everything else —
    /// every buffer call, every mark, every option — is the workspace's and would give the same
    /// answer whoever asked.
    pub fn apply_in(&mut self, view: ViewId, plugin: &PluginId, call: ApiCall) -> ApiResult {
        match call {
            // ---- buffers ---------------------------------------------
            ApiCall::BufCreate { name, kind, .. } => {
                let name = name.unwrap_or_else(|| format!("[scratch {}]", self.next_buf));
                let buf = self.create_buffer(&name);
                if let Some(kind) = kind {
                    self.buf_mut(buf)?.kind = Some(kind.clone());
                    self.effects.push(CoreEffect::KindSeen { kind });
                }
                Ok(ApiOk::Buf { buf })
            }
            ApiCall::BufSetKind { buf, kind } => {
                self.buf_mut(buf)?.kind = kind.clone();
                if let Some(kind) = kind {
                    self.effects.push(CoreEffect::KindSeen { kind });
                }
                // A kind remap follows the kind: windows on this buffer read their groups
                // differently from here on.
                self.republish_highlights_of_buffer(buf);
                Ok(ApiOk::Unit)
            }
            ApiCall::BufGetKind { buf } => {
                Ok(ApiOk::MaybeText { text: self.buf(buf)?.kind.clone() })
            }
            ApiCall::BufLineCount { buf } => {
                Ok(ApiOk::Count { n: self.buf(buf)?.line_count() })
            }
            ApiCall::BufGetLines { buf, start, end } => {
                let b = self.buf(buf)?;
                let (s, e) = (b.resolve_index(start), b.resolve_index(end));
                Ok(ApiOk::Lines { lines: b.get_lines(s, e) })
            }
            ApiCall::BufSetLines { buf, start, end, lines } => {
                let b = self.buf_mut(buf)?;
                let (s, e) = (b.resolve_index(start), b.resolve_index(end));
                let edit = b.set_lines(s, e, lines);
                self.emit_edit(buf, edit);
                Ok(ApiOk::Unit)
            }
            ApiCall::BufRender { buf, ns, start, end, lines } => {
                self.ns(ns)?;
                let b = self.buf_mut(buf)?;
                let (s, e) = (b.resolve_index(start), b.resolve_index(end));
                let edit = b.set_lines(s, e, lines.iter().map(|l| l.text.clone()).collect());
                let (from, count) = (edit.start, lines.len() as u32);
                // The namespace's own marks over the rows just written. `set_lines` carries marks
                // onto the positional counterpart of each replaced line — which is what makes
                // streaming work and what would otherwise leave a repainted row wearing the
                // clamped remains of the last repaint.
                b.clear_marks(ns, from, from + count);
                for (i, line) in lines.iter().enumerate() {
                    for m in &line.marks {
                        // Rows are the ones just written and columns are clamped, so this cannot
                        // fail; `?` rather than a discard so a future range check is not silent.
                        b.set_mark(ns, from + i as u32, m.col, m.opts.clone())?;
                    }
                }
                // One event, carrying the finished rows. Emitted after every mutation rather than
                // per step: a repaint that is observable halfway through is the bug this call
                // exists to remove.
                let rendered = b.render_range(from, from + count);
                self.push_ui(UiEvent::BufferLines {
                    buf,
                    start: from,
                    old_end: edit.old_end,
                    lines: rendered,
                });
                Ok(ApiOk::Unit)
            }
            ApiCall::BufAppendText { buf, text } => {
                let edit = self.buf_mut(buf)?.append_text(&text);
                self.emit_edit(buf, edit);
                Ok(ApiOk::Unit)
            }
            ApiCall::BufSetName { buf, name } => {
                self.buf_mut(buf)?.name = name.clone();
                self.push_ui(UiEvent::BufferOpened { buf, name });
                Ok(ApiOk::Unit)
            }
            ApiCall::BufAttach { buf } => {
                self.buf(buf)?;
                self.attached.entry(buf).or_default().insert(plugin.clone());
                Ok(ApiOk::Unit)
            }
            ApiCall::BufDetach { buf } => {
                if let Some(s) = self.attached.get_mut(&buf) {
                    s.remove(plugin);
                }
                Ok(ApiOk::Unit)
            }

            // ---- windows & floats ------------------------------------
            ApiCall::WinOpen { buf, layout, view: named } => {
                self.buf(buf)?;
                let view = named.unwrap_or(view);
                Ok(ApiOk::Win { win: self.open_window_in(view, buf, layout) })
            }
            ApiCall::FloatOpen { buf, config, view: named } => {
                self.buf(buf)?;
                let view = named.unwrap_or(view);
                let focusable = config.focusable;
                let win = self.open_window_in(view, buf, WindowLayout::Float { config });
                if focusable {
                    self.view(view).focus.push(win);
                    self.push_ui_in(view, UiEvent::FocusChanged { win: Some(win) });
                }
                Ok(ApiOk::Win { win })
            }
            ApiCall::FloatConfigure { win, config } => {
                self.win_mut(win)?.layout = WindowLayout::Float { config: config.clone() };
                self.push_ui(UiEvent::WindowConfigured {
                    win,
                    layout: WindowLayout::Float { config },
                });
                Ok(ApiOk::Unit)
            }
            ApiCall::WinResize { win, size } => {
                let layout = match &self.win(win)?.layout {
                    WindowLayout::Docked { dock, gravity, wrap, .. } => WindowLayout::Docked {
                        dock: *dock,
                        size,
                        gravity: *gravity,
                        wrap: *wrap,
                    },
                    WindowLayout::Float { .. } => {
                        return Err(ApiError::InvalidArgument {
                            message: "win_resize is for docked windows; a float is configured with float_configure".into(),
                        });
                    }
                };
                self.win_mut(win)?.layout = layout.clone();
                self.push_ui(UiEvent::WindowConfigured { win, layout });
                Ok(ApiOk::Unit)
            }
            ApiCall::WinClose { win } => {
                // The window's own view, not the caller's: closing something is a statement about
                // where it *is*, and a plugin tidying up after a terminal that has gone is the
                // ordinary case rather than a mistake.
                let home = self.win(win)?.view;
                self.windows.remove(&win);
                self.view(home).focus.remove(win);
                self.surfaces.retain(|_, (w, _)| *w != win);
                // A capture outliving its window would send every unbound key to a command whose
                // picker is gone — the keyboard would appear to stop working.
                self.captures.remove(&win);
                // Same for the keys a widget claimed while it was up. They are never resolved once
                // the window cannot be focused, but they would still be listed forever.
                self.keymaps.remove_window(win);
                self.win_hl.remove(&win);
                self.push_ui_in(home, UiEvent::WindowClosed { win });
                let now = self.focused(home);
                self.push_ui_in(home, UiEvent::FocusChanged { win: now });
                Ok(ApiOk::Unit)
            }
            ApiCall::WinSetBuf { win, buf } => {
                self.buf(buf)?;
                let home = self.win(win)?.view;
                self.win_mut(win)?.buf = buf;
                // Same reason as opening one: this terminal may never have been sent the rows.
                self.show_buffer_in(home, buf);
                self.push_ui_in(home, UiEvent::WindowBuffer { win, buf });
                self.republish_window_highlights(win);
                Ok(ApiOk::Unit)
            }
            ApiCall::WinGetCursor { win } => {
                let (row, col) = self.win(win)?.cursor;
                Ok(ApiOk::Cursor { row, col })
            }
            ApiCall::WinSetCursor { win, row, col } => {
                self.win_mut(win)?.cursor = (row, col);
                self.push_ui(UiEvent::CursorMoved { win, row, col });
                // A selection is the anchor *and* the cursor, so moving one of them moves it — and
                // a highlight that is not redrawn is the previous selection, still on screen while
                // `y` copies a different one. Deliberately without touching the anchor, which is
                // what separates this from `WinMotion`: a jump with a selection running takes the
                // text it jumped over. Only `WinMotion` repainted, so every jump the transcript
                // reader makes — `^U`, `{`, `[`, a search hit — extended a selection that could
                // not be seen until the next `hjkl`.
                self.refresh_selection(win);
                Ok(ApiOk::Unit)
            }
            ApiCall::WinGetViewport { win } => {
                let w = self.win(win)?;
                let line_count =
                    self.buffers.get(&w.buf).map(|b| b.line_count() as u32).unwrap_or(0);
                Ok(ApiOk::Viewport {
                    viewport: w.viewport.map(|v| neosh_proto::Viewport {
                        width: v.width,
                        height: v.height,
                        top_line: v.top_line,
                        rows: v.rows,
                        line_count,
                    }),
                })
            }
            // ---- editing ------------------------------------------------
            ApiCall::WinMotion { win, motion, select } => {
                let w = self.win(win)?;
                let (buf, at, goal, anchor) = (w.buf, w.cursor, w.goal_col, w.anchor);
                let (cursor, goal) = match self.buffers.get(&buf) {
                    Some(b) => text::resolve(b.lines(), at, motion, goal),
                    None => (at, None),
                };
                let w = self.win_mut(win)?;
                w.cursor = cursor;
                w.goal_col = goal;
                // Shift-and-arrow: the first extending motion anchors where you were, and any
                // motion without shift throws the selection away.
                w.anchor = select.then(|| anchor.unwrap_or(at));
                self.push_ui(UiEvent::CursorMoved { win, row: cursor.0, col: cursor.1 });
                self.refresh_selection(win);
                Ok(ApiOk::Unit)
            }
            ApiCall::WinEdit { win, edit } => {
                self.edit(win, edit)?;
                Ok(ApiOk::Unit)
            }
            ApiCall::WinSelect { win, on } => {
                let w = self.win_mut(win)?;
                w.anchor = on.then_some(w.cursor);
                // Dropping a selection drops its shape with it. A window left `Line` after the one
                // linewise selection it ever had would draw the next `v` as whole rows.
                if !on {
                    w.select_shape = SelectShape::Exclusive;
                }
                self.refresh_selection(win);
                Ok(ApiOk::Unit)
            }
            ApiCall::WinSelectShape { win, shape } => {
                self.win_mut(win)?.select_shape = shape;
                self.refresh_selection(win);
                Ok(ApiOk::Unit)
            }
            ApiCall::WinCursorShape { win, shape } => {
                let w = self.win_mut(win)?;
                if w.cursor_shape == shape {
                    return Ok(ApiOk::Unit);
                }
                w.cursor_shape = shape;
                self.push_ui(UiEvent::CursorShapeChanged { win, shape });
                Ok(ApiOk::Unit)
            }
            ApiCall::WinSelection { win } => {
                let w = self.win(win)?;
                let (buf, cursor, anchor, shape) = (w.buf, w.cursor, w.anchor, w.select_shape);
                let text = match (anchor, self.buffers.get(&buf)) {
                    (Some(a), Some(b)) => match selected_range(b.lines(), a, cursor, shape) {
                        Some((from, to)) => text::slice(b.lines(), from, to),
                        None => String::new(),
                    },
                    _ => String::new(),
                };
                Ok(ApiOk::Text { text })
            }
            ApiCall::ClipboardWrite { text } => {
                // Nothing here can reach a clipboard; the frontend owns the terminal this has to be
                // written to. Emitting it keeps the core free of I/O and makes copying work over a
                // pipe, which is the case a clipboard library gets wrong.
                self.push_ui(UiEvent::Clipboard { text });
                Ok(ApiOk::Unit)
            }
            ApiCall::WinScrollTo { win, top_line } => {
                self.win(win)?;
                // Remembered, not merely forwarded: it is where the window *is*, and a client
                // attaching later has no other way to be told.
                if let Some(w) = self.windows.get_mut(&win) {
                    w.top_line = top_line;
                }
                self.push_ui(UiEvent::ScrollTo { win, top_line });
                Ok(ApiOk::Unit)
            }
            ApiCall::WinList => {
                let focused = self.focused(view);
                let mut windows: Vec<neosh_proto::WindowInfo> = self
                    .windows
                    .values()
                    .map(|w| neosh_proto::WindowInfo {
                        win: w.id,
                        buf: w.buf,
                        kind: self.buffers.get(&w.buf).and_then(|b| b.kind.clone()),
                        name: self
                            .buffers
                            .get(&w.buf)
                            .map(|b| b.name.clone())
                            .unwrap_or_default(),
                        layout: w.layout.clone(),
                        focused: focused == Some(w.id),
                    })
                    .collect();
                // By id, so "the sidebar" is the same row on two consecutive calls. A `HashMap`
                // iteration order would make a caller that takes the first match of a kind pick a
                // different window each time there are two.
                windows.sort_by_key(|w| w.win.0);
                Ok(ApiOk::Windows { windows })
            }

            // ---- extmarks --------------------------------------------
            ApiCall::NsCreate { name } => {
                let ns = NamespaceId(self.next_ns);
                self.next_ns += 1;
                self.namespaces.insert(ns, name);
                Ok(ApiOk::Ns { ns })
            }
            ApiCall::MarkSet { ns, buf, row, col, opts } => {
                self.ns(ns)?;
                let id = self.buf_mut(buf)?.set_mark(ns, row, col, opts)?;
                self.emit_row(buf, row);
                Ok(ApiOk::Mark { id })
            }
            ApiCall::MarkDel { ns, buf, id } => {
                self.ns(ns)?;
                if let Some(row) = self.buf_mut(buf)?.del_mark(ns, id) {
                    self.emit_row(buf, row);
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::MarkClear { ns, buf, start, end } => {
                self.ns(ns)?;
                let b = self.buf_mut(buf)?;
                let end = end.unwrap_or_else(|| b.line_count());
                let (s, e) = b.clear_marks(ns, start.unwrap_or(0), end);
                self.emit_rows(buf, s, e);
                Ok(ApiOk::Unit)
            }
            ApiCall::MarkGet { ns, buf, id } => {
                self.ns(ns)?;
                Ok(ApiOk::MarkInfo { info: self.buf(buf)?.get_mark(ns, id) })
            }
            ApiCall::MarkAll { ns, buf } => {
                self.ns(ns)?;
                Ok(ApiOk::Marks { marks: self.buf(buf)?.all_marks(ns) })
            }

            // ---- highlights -------------------------------------------
            ApiCall::HlDefine { name, def, default } => {
                // The same rule as a key: a plugin's colour does not overwrite the user's, and a
                // bundled plugin's does not overwrite a plugin's. Silently, like a default key
                // that finds its key taken, because the ordinary case is exactly that.
                if let Some(owner) = self.highlights.owner(&name)
                    && owner != plugin.0
                    && self.tier(owner) > self.tier(&plugin.0)
                {
                    tracing::debug!(%plugin, name, owner, "highlight owned by a higher tier; kept");
                    return Ok(ApiOk::Unit);
                }
                if self.highlights.define(name.clone(), def.clone(), &plugin.0, default) {
                    self.push_ui(UiEvent::HighlightDefined { name: name.clone(), def });
                    self.effects.push(CoreEffect::HighlightsChanged { names: vec![name] });
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::HlGet { name } => Ok(ApiOk::Highlight {
                def: self.highlights.get(&name).cloned(),
                resolved: self.highlights.resolve(&name),
            }),
            ApiCall::HlList => {
                let mut groups: Vec<neosh_proto::HighlightEntry> = self
                    .highlights
                    .iter()
                    .map(|(name, def)| neosh_proto::HighlightEntry {
                        name: name.clone(),
                        def: def.clone(),
                        owner: self.highlights.owner(name).map(str::to_string),
                    })
                    .collect();
                groups.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(ApiOk::Highlights { groups })
            }
            ApiCall::HlReset { name } => {
                // Your own, or nothing: resetting a group another plugin owns would be the one
                // way left to take a colour off somebody without being able to be listed doing it.
                if self.highlights.owner(&name).is_some_and(|o| o != plugin.0) {
                    return Err(ApiError::InvalidArgument {
                        message: format!(
                            "highlight {name:?} is defined by {}",
                            self.highlights.owner(&name).unwrap_or_default()
                        ),
                    });
                }
                match self.highlights.reset(&name) {
                    Some(Restored::Theme(def)) => {
                        self.push_ui(UiEvent::HighlightDefined { name: name.clone(), def });
                    }
                    Some(Restored::Cleared) => {
                        self.push_ui(UiEvent::HighlightCleared { name: name.clone() });
                    }
                    None => return Ok(ApiOk::Unit),
                }
                self.effects.push(CoreEffect::HighlightsChanged { names: vec![name] });
                Ok(ApiOk::Unit)
            }
            ApiCall::WinSetHighlights { target, map } => {
                match target {
                    HlTarget::Window { win } => {
                        self.win(win)?;
                        if let Some((owner, _)) = self.win_hl.get(&win)
                            && owner != &plugin.0
                        {
                            return Err(ApiError::InvalidArgument {
                                message: format!("window {win}'s highlights are set by {owner}"),
                            });
                        }
                        if map.is_empty() {
                            self.win_hl.remove(&win);
                        } else {
                            self.win_hl.insert(win, (plugin.0.clone(), map));
                        }
                        self.republish_window_highlights(win);
                    }
                    HlTarget::Kind { name } => {
                        if let Some((owner, _)) = self.kind_hl.get(&name)
                            && owner != &plugin.0
                        {
                            return Err(ApiError::InvalidArgument {
                                message: format!("highlights for kind {name:?} are set by {owner}"),
                            });
                        }
                        if map.is_empty() {
                            self.kind_hl.remove(&name);
                        } else {
                            self.kind_hl.insert(name.clone(), (plugin.0.clone(), map));
                        }
                        let wins: Vec<WindowId> = self
                            .windows
                            .values()
                            .filter(|w| {
                                self.buffers.get(&w.buf).and_then(|b| b.kind.as_deref())
                                    == Some(name.as_str())
                            })
                            .map(|w| w.id)
                            .collect();
                        for win in wins {
                            self.republish_window_highlights(win);
                        }
                    }
                }
                Ok(ApiOk::Unit)
            }

            // ---- raw cells --------------------------------------------
            ApiCall::SurfaceClaim { win, rect } => {
                self.win(win)?;
                let surface = SurfaceId(self.next_surface);
                self.next_surface += 1;
                self.surfaces.insert(surface, (win, rect));
                self.push_ui(UiEvent::SurfaceClaimed { surface, win, rect });
                Ok(ApiOk::Surface { surface })
            }
            ApiCall::SurfacePut { surface, cells } => {
                if !self.surfaces.contains_key(&surface) {
                    return Err(ApiError::NotFound { what: format!("surface {surface}") });
                }
                self.push_ui(UiEvent::SurfaceCells { surface, cells });
                Ok(ApiOk::Unit)
            }
            ApiCall::SurfaceRelease { surface } => {
                self.surfaces.remove(&surface);
                self.push_ui(UiEvent::SurfaceReleased { surface });
                Ok(ApiOk::Unit)
            }

            // ---- commands & keymaps -----------------------------------
            ApiCall::CmdRegister { name, desc } => {
                let reg = CommandReg { plugin: plugin.clone(), desc };
                match self.commands.get(&name) {
                    Some(existing) if existing.plugin == *plugin => {}
                    Some(existing) => {
                        let (mine, theirs) = (self.tier(&plugin.0), self.tier(&existing.plugin.0));
                        if mine > theirs {
                            // The user's `init.ts` registering `sidebar.toggle` before the
                            // sidebar loads: the user wins, and the sidebar's registration is
                            // kept in the wings rather than refused, so its `activate` does not
                            // fail over a name it was always going to lose.
                            let was = self.commands.insert(name.clone(), reg);
                            self.shadowed.entry(name).or_default().extend(was);
                            return Ok(ApiOk::Unit);
                        }
                        if mine < theirs {
                            self.shadowed.entry(name).or_default().push(reg);
                            return Ok(ApiOk::Unit);
                        }
                        return Err(ApiError::InvalidArgument {
                            message: format!(
                                "command {name:?} is already registered by {}",
                                existing.plugin
                            ),
                        });
                    }
                    None => {}
                }
                self.commands.insert(name, reg);
                Ok(ApiOk::Unit)
            }
            ApiCall::CmdUnregister { name } => {
                if self.commands.get(&name).is_some_and(|r| &r.plugin == plugin) {
                    self.commands.remove(&name);
                    // The name goes back to whoever it was taken from.
                    if let Some(back) = self.shadowed.get_mut(&name).and_then(|v| v.pop()) {
                        self.commands.insert(name.clone(), back);
                    }
                } else if let Some(list) = self.shadowed.get_mut(&name) {
                    list.retain(|r| &r.plugin != plugin);
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::CmdExec { name, args } => {
                if !self.commands.contains_key(&name) {
                    return Err(ApiError::NotFound { what: format!("command {name}") });
                }
                self.invoke_command(&name, args, None);
                Ok(ApiOk::Unit)
            }
            ApiCall::CmdList => {
                let mut commands: Vec<_> = self
                    .commands
                    .iter()
                    .map(|(name, reg)| neosh_proto::CommandEntry {
                        name: name.clone(),
                        desc: reg.desc.clone(),
                        plugin: reg.plugin.0.clone(),
                    })
                    .collect();
                commands.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(ApiOk::Commands { commands })
            }
            ApiCall::KeymapSet { mode, lhs, command, scope, desc } => {
                let lhs = self.with_leader(&lhs);
                let tiers = &self.tiers;
                let tier = |o: &str| tiers.get(o).copied().unwrap_or_default();
                let took = self.keymaps.set(
                    mode,
                    scope.unwrap_or(KeymapScope::Global),
                    &lhs,
                    Binding { command: command.clone(), desc, owner: Some(plugin.0.clone()) },
                    &tier,
                )?;
                if !took {
                    // Not an error: a bundled plugin offering a default for a key somebody has
                    // already taken is the ordinary case, and failing the call would make every
                    // such plugin log a warning on a perfectly good configuration.
                    tracing::debug!(%plugin, lhs, command, "key already bound; default not applied");
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::KeymapDel { mode, lhs, scope } => {
                let lhs = self.with_leader(&lhs);
                let tiers = &self.tiers;
                let tier = |o: &str| tiers.get(o).copied().unwrap_or_default();
                // A plugin may unbind its own keys, another plugin's, and a bundled default —
                // never the user's. The same rule as binding one.
                let went = self.keymaps.del_by(
                    mode,
                    scope.unwrap_or(KeymapScope::Global),
                    &lhs,
                    &plugin.0,
                    &tier,
                )?;
                if !went {
                    tracing::debug!(%plugin, lhs, "key bound by a higher tier; not unbound");
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::KeymapCapture { win, command } => {
                // Fail rather than silently never firing: a capture on a window that does not exist
                // is a plugin bug, and the symptom otherwise is "my picker ignores every key".
                self.win(win)?;
                self.captures.insert(win, command);
                Ok(ApiOk::Unit)
            }
            ApiCall::KeymapRelease { win } => {
                self.captures.remove(&win);
                Ok(ApiOk::Unit)
            }
            ApiCall::KeymapList { mode } => Ok(ApiOk::Keymaps {
                keymaps: self
                    .keymaps
                    .list(mode)
                    .into_iter()
                    .map(|(mode, scope, seq, b)| KeymapEntry {
                        mode,
                        lhs: format_keys(&seq),
                        command: b.command,
                        scope,
                        desc: b.desc,
                    })
                    .collect(),
            }),

            // ---- focus -------------------------------------------------
            ApiCall::FocusPush { win } => {
                // Focus follows the window rather than the asker. A plugin raising a panel it
                // opened in another terminal is giving *that* keyboard to it, which is the only
                // reading that leaves both screens describing something true.
                let home = self.win(win)?.view;
                self.view(home).focus.push(win);
                self.push_ui_in(home, UiEvent::FocusChanged { win: Some(win) });
                Ok(ApiOk::Unit)
            }
            ApiCall::FocusPop => {
                let popped = self.view(view).focus.pop();
                // A float that asked to close on blur is destroyed rather than merely hidden,
                // otherwise pickers accumulate invisibly.
                if let Some(w) = popped
                    && self.windows.get(&w).is_some_and(Window::close_on_blur)
                {
                    self.windows.remove(&w);
                    self.captures.remove(&w);
                    self.push_ui_in(view, UiEvent::WindowClosed { win: w });
                }
                let now = self.focused(view);
                self.push_ui_in(view, UiEvent::FocusChanged { win: now });
                Ok(ApiOk::Unit)
            }
            ApiCall::FocusCurrent => Ok(ApiOk::FocusedWin { win: self.focused(view) }),

            // ---- options -----------------------------------------------
            ApiCall::OptDeclare { spec } => {
                self.options.declare(&plugin.0, spec)?;
                Ok(ApiOk::Unit)
            }
            ApiCall::OptSet { name, value } => {
                let value = self.options.set(&name, value)?;
                self.effects.push(CoreEffect::OptionChanged { name, value });
                Ok(ApiOk::Unit)
            }
            ApiCall::OptReset { name } => {
                let value = self.options.reset(&name)?;
                self.effects.push(CoreEffect::OptionChanged { name, value });
                Ok(ApiOk::Unit)
            }
            ApiCall::OptGet { name } => Ok(ApiOk::Option { entry: self.options.entry(&name) }),
            ApiCall::OptAll => Ok(ApiOk::Options { options: self.options.all() }),

            // ---- contributions -----------------------------------------
            ApiCall::ExtContribute { point, id, item, priority } => {
                let entry = Contribution {
                    point: point.clone(),
                    id,
                    plugin: plugin.0.clone(),
                    item,
                    priority,
                };
                let list = self.contributions.entry(point.clone()).or_default();
                // Keyed by (plugin, id), so re-contributing under the same id is an update rather
                // than a duplicate row — the same rule `status.set` follows, and for the same
                // reason: a panel refreshing what it offers must not have to withdraw first.
                match list.iter_mut().find(|c| c.plugin == entry.plugin && c.id == entry.id) {
                    Some(existing) => *existing = entry,
                    None => list.push(entry),
                }
                Self::sort_contributions(list);
                self.effects.push(CoreEffect::ContributionsChanged { point });
                Ok(ApiOk::Unit)
            }
            ApiCall::ExtRemove { point, id } => {
                let mut changed = false;
                if let Some(list) = self.contributions.get_mut(&point) {
                    let before = list.len();
                    list.retain(|c| !(c.plugin == plugin.0 && c.id == id));
                    changed = list.len() != before;
                }
                if changed {
                    self.effects.push(CoreEffect::ContributionsChanged { point });
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::ExtList { point } => Ok(ApiOk::Contributions {
                contributions: self.contributions.get(&point).cloned().unwrap_or_default(),
            }),

            // ---- misc --------------------------------------------------
            ApiCall::Log { level, message } => {
                match level {
                    MessageLevel::Error => tracing::error!(plugin = %plugin, "{message}"),
                    MessageLevel::Warn => tracing::warn!(plugin = %plugin, "{message}"),
                    MessageLevel::Info => tracing::info!(plugin = %plugin, "{message}"),
                }
                Ok(ApiOk::Unit)
            }
            ApiCall::Notify { level, message, kind, key } => {
                // A progress row without a key is a row nothing can ever replace or take away,
                // which is the one failure mode `Progress` exists to remove. Demoted rather than
                // refused: the caller had something to say and the corner is still the place for
                // it. See ADR 0057.
                let kind = match (kind, &key) {
                    (NoticeKind::Progress, None) => NoticeKind::Reply,
                    (k, _) => k,
                };
                self.push_ui(UiEvent::Message { level, text: message, kind, key });
                Ok(ApiOk::Unit)
            }
            ApiCall::NotifyDone { key } => {
                self.push_ui(UiEvent::ProgressDone { key });
                Ok(ApiOk::Unit)
            }

            other => Err(ApiError::Internal {
                message: format!(
                    "{} is an agent-domain call and must be routed to the host, not the editor",
                    call_name(&other)
                ),
            }),
        }
    }

    /// Highest priority first, then by plugin and id.
    ///
    /// The tiebreak is what stops a panel's rows shuffling between restarts: without it the order
    /// of two equal-priority contributions is the order their plugins happened to activate in,
    /// which changes with a directory listing.
    fn sort_contributions(list: &mut [Contribution]) {
        list.sort_by(|a, b| {
            b.priority.cmp(&a.priority).then_with(|| a.plugin.cmp(&b.plugin)).then_with(|| a.id.cmp(&b.id))
        });
    }

    // ---- lookup helpers --------------------------------------------------

    fn buf(&self, id: BufferId) -> Result<&Buffer, ApiError> {
        self.buffers.get(&id).ok_or_else(|| ApiError::NotFound { what: format!("buffer {id}") })
    }

    /// Apply one edit at a window's cursor.
    ///
    /// Typing over a selection replaces it, which is why this is one function rather than two arms:
    /// the delete and the insert have to agree about where the cursor ended up in between.
    fn edit(&mut self, win: WindowId, edit: TextEdit) -> Result<(), ApiError> {
        let w = self.win(win)?;
        let (buf, at, anchor) = (w.buf, w.cursor, w.anchor);

        let shape = w.select_shape;
        let selected = anchor
            .and_then(|a| self.buffers.get(&buf).and_then(|b| selected_range(b.lines(), a, at, shape)));
        let edit = match edit {
            TextEdit::DeleteSelection => match selected {
                Some((from, to)) => TextEdit::DeleteRange { from, to },
                // Not an error: `<Del>` with nothing selected is an ordinary keystroke that this
                // window happens to have nothing to do with.
                None => return Ok(()),
            },
            TextEdit::Insert { text } => {
                if let Some((from, to)) = selected {
                    self.apply_plan(win, buf, at, &TextEdit::DeleteRange { from, to })?;
                }
                TextEdit::Insert { text }
            }
            other => other,
        };

        let at = self.win(win)?.cursor;
        self.apply_plan(win, buf, at, &edit)?;
        let w = self.win_mut(win)?;
        w.anchor = None;
        w.select_shape = SelectShape::Exclusive;
        w.goal_col = None;
        self.refresh_selection(win);
        Ok(())
    }

    fn apply_plan(
        &mut self,
        win: WindowId,
        buf: BufferId,
        at: (u32, u32),
        edit: &TextEdit,
    ) -> Result<(), ApiError> {
        let Some(plan) = self.buffers.get(&buf).and_then(|b| text::plan(b.lines(), at, edit))
        else {
            return Ok(());
        };
        let applied = self.buf_mut(buf)?.set_lines(plan.start, plan.end, plan.text);
        self.emit_edit(buf, applied);
        let w = self.win_mut(win)?;
        w.cursor = plan.cursor;
        self.push_ui(UiEvent::CursorMoved { win, row: plan.cursor.0, col: plan.cursor.1 });
        Ok(())
    }

    /// Redraw the selection highlight for a window.
    ///
    /// Cleared and rebuilt rather than diffed: a selection is at most a screenful of marks, and the
    /// bookkeeping to move them correctly under an edit is exactly the bug class marks-on-lines
    /// exists to avoid.
    fn refresh_selection(&mut self, win: WindowId) {
        let Ok(w) = self.win(win) else { return };
        let (buf, cursor, anchor, shape) = (w.buf, w.cursor, w.anchor, w.select_shape);
        let ns = self.selection_ns;

        let Some(b) = self.buffers.get_mut(&buf) else { return };
        let count = b.line_count();
        let (cleared_start, cleared_end) = b.clear_marks(ns, 0, count);

        let mut touched = (cleared_start, cleared_end);
        if let Some((from, to)) = anchor.and_then(|a| selected_range(b.lines(), a, cursor, shape)) {
            for row in from.0..=to.0.min(count.saturating_sub(1)) {
                let line_len = b.lines().get(row as usize).map(|l| l.text.len() as u32).unwrap_or(0);
                let start = if row == from.0 { from.1 } else { 0 };
                // One past the end of the text on every line but the last, so a selection that
                // swallowed a line break looks like it did.
                let end = if row == to.0 { to.1 } else { line_len };
                if start >= end && !(row < to.0) {
                    continue;
                }
                let _ = b.set_mark(ns, row, start, ExtmarkOpts {
                    end_col: Some(end.max(start)),
                    hl_group: Some("Visual".into()),
                    line_hl_group: None,
                    virt_text: Vec::new(),
                    virt_text_pos: VirtTextPos::Eol,
                    on_delete: OnDelete::Invalidate,
                    priority: 100,
                });
            }
            touched = (touched.0.min(from.0), touched.1.max(to.0 + 1));
        }

        if touched.0 < touched.1 {
            self.emit_rows(buf, touched.0, touched.1.min(count));
        }
    }

    fn buf_mut(&mut self, id: BufferId) -> Result<&mut Buffer, ApiError> {
        self.buffers.get_mut(&id).ok_or_else(|| ApiError::NotFound { what: format!("buffer {id}") })
    }

    fn win(&self, id: WindowId) -> Result<&Window, ApiError> {
        self.windows.get(&id).ok_or_else(|| ApiError::NotFound { what: format!("window {id}") })
    }

    fn win_mut(&mut self, id: WindowId) -> Result<&mut Window, ApiError> {
        self.windows.get_mut(&id).ok_or_else(|| ApiError::NotFound { what: format!("window {id}") })
    }

    fn ns(&self, id: NamespaceId) -> Result<(), ApiError> {
        if self.namespaces.contains_key(&id) {
            Ok(())
        } else {
            Err(ApiError::NotFound { what: format!("namespace {id}") })
        }
    }
}

fn call_name(call: &ApiCall) -> &'static str {
    match call {
        ApiCall::AgentSend { .. } => "agent.send",
        ApiCall::AgentCancel => "agent.cancel",
        ApiCall::AgentGetSelection => "agent.getSelection",
        ApiCall::AgentSetSelection { .. } => "agent.setSelection",
        ApiCall::AgentListModels { .. } => "agent.listModels",
        ApiCall::AgentListInstances => "agent.listInstances",
        ApiCall::ToolRegister { .. } => "tool.register",
        ApiCall::ToolUnregister { .. } => "tool.unregister",
        ApiCall::ToolList => "tool.list",
        ApiCall::HookRegister { .. } => "hook.register",
        ApiCall::HookUnregister { .. } => "hook.unregister",
        ApiCall::ProviderRegisterDriver { .. } => "provider.registerDriver",
        ApiCall::ProviderEmit { .. } => "provider.emit",
        ApiCall::PermissionCheck { .. } => "permission.check",
        ApiCall::RtpAdd { .. } => "rtp.add",
        ApiCall::KeymapCapture { .. } => "keymap.capture",
        ApiCall::KeymapRelease { .. } => "keymap.release",
        ApiCall::WinGetViewport { .. } => "win.getViewport",
        ApiCall::WinSelectShape { .. } => "win.selectShape",
        ApiCall::WinCursorShape { .. } => "win.cursorShape",
        ApiCall::RtpList => "rtp.list",
        ApiCall::GitStatus { .. } => "git.status",
        ApiCall::GitBranches { .. } => "git.branches",
        ApiCall::GitWorktrees { .. } => "git.worktrees",
        ApiCall::GitLog { .. } => "git.log",
        ApiCall::GitDiff { .. } => "git.diff",
        ApiCall::GitDefaultBranch => "git.defaultBranch",
        ApiCall::GitCreateBranch { .. } => "git.createBranch",
        ApiCall::GitRenameBranch { .. } => "git.renameBranch",
        ApiCall::GitCheckout { .. } => "git.checkout",
        ApiCall::GitStage { .. } => "git.stage",
        ApiCall::GitUnstage { .. } => "git.unstage",
        ApiCall::GitCommit { .. } => "git.commit",
        ApiCall::GitPull { .. } => "git.pull",
        ApiCall::GitAddWorktree { .. } => "git.addWorktree",
        ApiCall::GitRemoveWorktree { .. } => "git.removeWorktree",
        ApiCall::GenComplete { .. } => "gen.complete",
        ApiCall::PermissionGetMode => "permission.mode",
        ApiCall::PermissionSetMode { .. } => "permission.setMode",
        ApiCall::StatusSet { .. } => "status.set",
        ApiCall::HintSet { .. } => "hint.set",
        ApiCall::HintClear { .. } => "hint.clear",
        ApiCall::StatusClear { .. } => "status.clear",
        ApiCall::SessionList { .. } => "session.list",
        ApiCall::ViewList => "view.list",
        ApiCall::SessionCurrent => "session.current",
        ApiCall::SessionNew { .. } => "session.new",
        ApiCall::SessionSwitch { .. } => "session.switch",
        ApiCall::SessionClose { .. } => "session.close",
        ApiCall::SessionRename { .. } => "session.rename",
        ApiCall::SessionArchive { .. } => "session.archive",
        ApiCall::ProviderCredentials => "provider.credentials",
        ApiCall::ProviderSetCredential { .. } => "provider.set_credential",
        ApiCall::ProviderForgetCredential { .. } => "provider.forget_credential",
        ApiCall::SessionMessages { .. } => "session.messages",
        _ => "call",
    }
}

/// Convenience for tests and for the host's own setup code.
pub fn float(anchor: neosh_proto::Anchor) -> FloatConfig {
    FloatConfig { anchor, ..Default::default() }
}

/// Marker so `ExtmarkId` is re-exported where callers expect it.
pub type MarkId = ExtmarkId;

/// The two ends of a selection, in order, with its shape applied.
///
/// `None` when nothing is selected — which is not the same thing for every shape. Exclusive, the
/// anchor and the cursor being in the same place selects nothing, because the cursor sits *between*
/// characters and there is no character between one place and itself. Inclusive and linewise, the
/// same two positions select the character or the row the cursor is on: `v` and then `y` copies a
/// letter, and `V` and then `y` copies a line. Answering "nothing" there is what made `v` `y` print
/// "the selection is empty" at somebody who had just pressed the two keys that mean "copy this".
fn selected_range(
    lines: &[crate::buffer::Line],
    anchor: (u32, u32),
    cursor: (u32, u32),
    shape: SelectShape,
) -> Option<((u32, u32), (u32, u32))> {
    let (from, to) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
    match shape {
        SelectShape::Exclusive => (from != to).then_some((from, to)),
        // One grapheme past the far end, which is what makes the character the cursor is on part
        // of what is selected. Past the last one on the row, the end of the row — the line break
        // is not a character you can be on.
        SelectShape::Inclusive => {
            let text = lines.get(to.0 as usize).map(|l| l.text.as_str()).unwrap_or("");
            let end = text::after(text, to.1);
            Some((from, (to.0, end)))
        }
        // Whole rows, in whichever direction the selection runs. The anchor has to move too — a
        // range that starts halfway along the first row is a range whose first row is half in it,
        // which extending *upwards* is exactly what produced: the one line you definitely meant
        // was the one that dropped out.
        SelectShape::Line => {
            let end = lines.get(to.0 as usize).map(|l| l.text.len() as u32).unwrap_or(0);
            Some(((from.0, 0), (to.0, end)))
        }
    }
}
