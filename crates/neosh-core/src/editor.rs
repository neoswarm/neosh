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

use neosh_proto::{
    ApiCall, ApiError, ApiOk, ApiResult, BufferId, Contribution, ExtmarkId, ExtmarkOpts,
    FloatConfig, KeyContext, KeyPress, KeymapEntry, KeymapScope, MessageLevel, Mode, NamespaceId,
    OnDelete, OptionValue, PluginId, Rect, SurfaceId, TextEdit, UiEvent, VirtTextPos, WindowId,
    WindowLayout,
};

use crate::buffer::{Buffer, LineEdit};
use crate::focus::FocusStack;
use crate::highlight::HighlightRegistry;
use crate::keymap::{Binding, KeyResolution, KeymapTable, format_keys};
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
    UnhandledKey { key: KeyPress, mode: Mode },
    /// A declared option was set or reset. The host acts on the ones it owns and broadcasts all of
    /// them, so a plugin reacting to a setting uses the same mechanism the core does.
    OptionChanged { name: String, value: OptionValue },
    /// A contribution point gained or lost an item. Broadcast by the host so whoever renders the
    /// point redraws, including for a plugin that loaded long after the panel first drew.
    ContributionsChanged { point: String },
}

#[derive(Debug, Clone)]
struct CommandReg {
    plugin: PluginId,
    desc: Option<String>,
}

#[derive(Debug, Default)]
pub struct Editor {
    buffers: HashMap<BufferId, Buffer>,
    windows: HashMap<WindowId, Window>,
    namespaces: HashMap<NamespaceId, String>,
    highlights: HighlightRegistry,
    focus: FocusStack,
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
    /// Plugins that ship with neosh. Their keymaps are *defaults*, and a default that overwrites a
    /// choice is not a default — `init.ts` runs before plugin discovery, so without this every
    /// bundled plugin would silently take a key the user's configuration had just bound.
    bundled: HashSet<String>,
    /// Where the selection highlight is drawn.
    ///
    /// Reserved at construction rather than exposed, so a selection is an ordinary extmark from the
    /// frontend's point of view and no rendering code had to learn a new concept. One namespace for
    /// all windows: two views of one buffer both selecting is a real but rare case, and the last
    /// refresh winning beats a per-window namespace nobody can clear.
    selection_ns: NamespaceId,

    mode: Mode,
    /// Keys held while a multi-key sequence is still ambiguous.
    pending_keys: Vec<KeyPress>,

    ui: Vec<UiEvent>,
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
            mode: Mode::Normal,
            next_buf: 1,
            next_win: 1,
            // 1 is taken by the selection namespace below.
            next_ns: 2,
            next_surface: 1,
            selection_ns: NamespaceId(1),
            ..Default::default()
        };
        e.namespaces.insert(e.selection_ns, "neosh.selection".to_string());
        e.ui.push(UiEvent::Init { protocol_version: neosh_proto::PROTOCOL_VERSION });
        for (name, def) in e.highlights.iter() {
            e.ui.push(UiEvent::HighlightDefined { name: name.clone(), def: def.clone() });
        }
        e
    }

    /// Say that a plugin ships with neosh, so its keymaps behave as defaults.
    pub fn mark_bundled(&mut self, plugin: &PluginId) {
        self.bundled.insert(plugin.0.clone());
    }

    /// Whether this call is UI-domain and belongs to the core.
    ///
    /// The host uses this to split synchronous UI work from asynchronous agent work without the
    /// plugin API having two shapes.
    pub fn handles(call: &ApiCall) -> bool {
        !matches!(
            call,
            ApiCall::AgentSend { .. }
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
                | ApiCall::GitStatus
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
                | ApiCall::GitAddWorktree { .. }
                | ApiCall::GitRemoveWorktree { .. }
                | ApiCall::GenComplete { .. }
                | ApiCall::SessionList { .. }
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
        )
    }

    // ---- draining -------------------------------------------------------

    /// Take the queued UI events. The host calls this on a ~16 ms deadline armed by the first
    /// mutation, then appends [`UiEvent::Flush`].
    pub fn drain_ui(&mut self) -> Vec<UiEvent> {
        std::mem::take(&mut self.ui)
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
        true
    }

    /// Turn motion on or off. Returns whether anything changed.
    pub fn set_motion(&mut self, on: bool) -> bool {
        if !self.highlights.set_motion(on) {
            return false;
        }
        self.republish_highlights();
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
    pub fn republish(&mut self) {
        self.push_ui(UiEvent::Init { protocol_version: neosh_proto::PROTOCOL_VERSION });
        self.republish_highlights();

        let mut buffers: Vec<BufferId> = self.buffers.keys().copied().collect();
        buffers.sort_by_key(|b| b.0);
        for buf in buffers {
            let Some(b) = self.buffers.get(&buf) else { continue };
            let (name, count) = (b.name.clone(), b.line_count());
            self.push_ui(UiEvent::BufferOpened { buf, name });
            // `old_end: u32::MAX` — "however many rows you have, they are these now". Not `0`,
            // which would be an insertion at the top: correct for a mirror that has nothing and
            // silently doubling for one that already has the state. A mirror that already has it
            // is the ordinary case now that a workspace can have several views: a terminal
            // joining republishes to *all* of them, which is what brings the ones already here
            // back into step. Not `count` either, because a mirror can be holding more rows than
            // the buffer now has. The mirror clamps the range, so this is exactly "replace it all"
            // at either end.
            let lines = self.buffers[&buf].render_range(0, count);
            self.push_ui(UiEvent::BufferLines { buf, start: 0, old_end: u32::MAX, lines });
        }

        let mut windows: Vec<WindowId> = self.windows.keys().copied().collect();
        windows.sort_by_key(|w| w.0);
        for win in windows {
            let Some(w) = self.windows.get(&win) else { continue };
            let (buf, layout, cursor, top) = (w.buf, w.layout.clone(), w.cursor, w.top_line);
            self.push_ui(UiEvent::WindowOpened { win, buf, layout });
            self.push_ui(UiEvent::CursorMoved { win, row: cursor.0, col: cursor.1 });
            self.push_ui(UiEvent::ScrollTo { win, top_line: top });
        }

        let mut surfaces: Vec<SurfaceId> = self.surfaces.keys().copied().collect();
        surfaces.sort_by_key(|s| s.0);
        for surface in surfaces {
            let Some((win, rect)) = self.surfaces.get(&surface).copied() else { continue };
            self.push_ui(UiEvent::SurfaceClaimed { surface, win, rect });
        }

        self.push_ui(UiEvent::FocusChanged { win: self.focus.current() });
    }

    fn republish_highlights(&mut self) {
        let defs: Vec<_> = self.highlights.iter().map(|(n, d)| (n.clone(), d.clone())).collect();
        for (name, def) in defs {
            self.push_ui(UiEvent::HighlightDefined { name, def });
        }
    }

    /// Queue a UI event, coalescing consecutive edits to the same buffer region.
    ///
    /// Streaming produces one `BufferLines` per token, all rewriting the same final line. Without
    /// this merge the frontend would receive hundreds of redundant events per frame; with it, a
    /// burst collapses to a single event carrying the final text.
    fn push_ui(&mut self, ev: UiEvent) {
        if let UiEvent::BufferLines { buf, start, old_end, lines } = &ev
            && let Some(UiEvent::BufferLines {
                buf: pbuf,
                start: pstart,
                lines: plines,
                ..
            }) = self.ui.last_mut()
            && pbuf == buf
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
        self.ui.push(ev);
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

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.pending_keys.clear();
    }

    pub fn focused(&self) -> Option<WindowId> {
        self.focus.current()
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

    pub fn open_window(&mut self, buf: BufferId, layout: WindowLayout) -> WindowId {
        let id = WindowId(self.next_win);
        self.next_win += 1;
        self.windows.insert(id, Window::new(id, buf, layout.clone()));
        self.push_ui(UiEvent::WindowOpened { win: id, buf, layout });
        id
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
    fn active_scopes(&self) -> Vec<KeymapScope> {
        let mut scopes = Vec::new();
        if let Some(win) = self.focus.current() {
            scopes.push(KeymapScope::Window { win });
            if let Some(w) = self.windows.get(&win) {
                scopes.push(KeymapScope::Buffer { buf: w.buf });
                if let Some(kind) = self.buffers.get(&w.buf).and_then(|b| b.kind.clone()) {
                    scopes.push(KeymapScope::BufKind { name: kind });
                }
            }
        }
        if !self.focus_is_modal() {
            scopes.push(KeymapScope::Global);
        }
        scopes
    }

    /// Whether the focused window has declared itself modal. See [`FloatConfig::modal`].
    fn focus_is_modal(&self) -> bool {
        self.focus
            .current()
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
    pub fn feed_key(&mut self, key: KeyPress) -> KeyResolution {
        self.pending_keys.push(key.clone());
        let scopes = self.active_scopes();
        let seq = self.pending_keys.clone();
        let mut res = self.keymaps.resolve(self.mode, &scopes, &seq);
        // A modal drops `Global` from its scopes, so the escape hatches have to be let back in by
        // name. Consulted only once the panel's own scopes have declined the key: a modal that
        // binds `<C-r>` for something of its own keeps it, which is the same rule every other
        // scope follows.
        if matches!(res, KeyResolution::Unhandled)
            && self.focus_is_modal()
            && self.is_modal_escape(&seq)
        {
            res = self.keymaps.resolve(self.mode, &[KeymapScope::Global], &seq);
        }
        match &res {
            KeyResolution::Pending => {}
            KeyResolution::Matched { command, .. } => {
                self.pending_keys.clear();
                let cmd = command.clone();
                let ctx = KeyContext { key, mode: self.mode, win: self.focus.current() };
                self.invoke_command(&cmd, Vec::new(), Some(ctx));
            }
            KeyResolution::Unhandled => {
                // Every key the abandoned prefix was holding, in order, then the one that broke it.
                //
                // Dropping the prefix instead would mean that with `gd` bound, typing `gx` produces
                // `x` — the composer eats characters and there is no way to tell why.
                let held = std::mem::take(&mut self.pending_keys);
                for k in held {
                    self.unclaimed(k);
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
    pub fn flush_pending(&mut self) -> bool {
        let held = std::mem::take(&mut self.pending_keys);
        if held.is_empty() {
            return false;
        }
        for k in held {
            self.unclaimed(k);
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
    fn unclaimed(&mut self, key: KeyPress) {
        let captured = self.focus.current().and_then(|w| self.captures.get(&w).cloned());
        match captured {
            Some(command) => {
                let ctx = KeyContext { key, mode: self.mode, win: self.focus.current() };
                self.invoke_command(&command, Vec::new(), Some(ctx));
            }
            None if self.focus_is_modal() => {}
            None => self.effects.push(CoreEffect::UnhandledKey { key, mode: self.mode }),
        }
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
            None => self.push_ui(UiEvent::Message {
                level: MessageLevel::Error,
                text: format!("no such command: {name}"),
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
    }

    // ---- the dispatcher --------------------------------------------------

    pub fn apply(&mut self, plugin: &PluginId, call: ApiCall) -> ApiResult {
        match call {
            // ---- buffers ---------------------------------------------
            ApiCall::BufCreate { name, kind, .. } => {
                let name = name.unwrap_or_else(|| format!("[scratch {}]", self.next_buf));
                let buf = self.create_buffer(&name);
                if kind.is_some() {
                    self.buf_mut(buf)?.kind = kind;
                }
                Ok(ApiOk::Buf { buf })
            }
            ApiCall::BufSetKind { buf, kind } => {
                self.buf_mut(buf)?.kind = kind;
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
            ApiCall::WinOpen { buf, layout } => {
                self.buf(buf)?;
                Ok(ApiOk::Win { win: self.open_window(buf, layout) })
            }
            ApiCall::FloatOpen { buf, config } => {
                self.buf(buf)?;
                let focusable = config.focusable;
                let win = self.open_window(buf, WindowLayout::Float { config });
                if focusable {
                    self.focus.push(win);
                    self.push_ui(UiEvent::FocusChanged { win: Some(win) });
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
                self.win(win)?;
                self.windows.remove(&win);
                self.focus.remove(win);
                self.surfaces.retain(|_, (w, _)| *w != win);
                // A capture outliving its window would send every unbound key to a command whose
                // picker is gone — the keyboard would appear to stop working.
                self.captures.remove(&win);
                // Same for the keys a widget claimed while it was up. They are never resolved once
                // the window cannot be focused, but they would still be listed forever.
                self.keymaps.remove_window(win);
                self.push_ui(UiEvent::WindowClosed { win });
                let now = self.focus.current();
                self.push_ui(UiEvent::FocusChanged { win: now });
                Ok(ApiOk::Unit)
            }
            ApiCall::WinSetBuf { win, buf } => {
                self.buf(buf)?;
                self.win_mut(win)?.buf = buf;
                self.push_ui(UiEvent::WindowBuffer { win, buf });
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
                self.refresh_selection(win);
                Ok(ApiOk::Unit)
            }
            ApiCall::WinSelection { win } => {
                let w = self.win(win)?;
                let (buf, cursor, anchor) = (w.buf, w.cursor, w.anchor);
                let text = match (anchor, self.buffers.get(&buf)) {
                    (Some(a), Some(b)) => text::slice(b.lines(), a, cursor),
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
                let focused = self.focus.current();
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
            ApiCall::HlDefine { name, def } => {
                self.highlights.define(name.clone(), def.clone());
                self.push_ui(UiEvent::HighlightDefined { name, def });
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
                if let Some(existing) = self.commands.get(&name)
                    && &existing.plugin != plugin
                {
                    return Err(ApiError::InvalidArgument {
                        message: format!(
                            "command {name:?} is already registered by {}",
                            existing.plugin
                        ),
                    });
                }
                self.commands.insert(name, CommandReg { plugin: plugin.clone(), desc });
                Ok(ApiOk::Unit)
            }
            ApiCall::CmdUnregister { name } => {
                if self.commands.get(&name).is_some_and(|r| &r.plugin == plugin) {
                    self.commands.remove(&name);
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
                let took = self.keymaps.set(
                    mode,
                    scope.unwrap_or(KeymapScope::Global),
                    &lhs,
                    Binding { command: command.clone(), desc, owner: Some(plugin.0.clone()) },
                    &self.bundled,
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
                self.keymaps.del(mode, scope.unwrap_or(KeymapScope::Global), &lhs)?;
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
                self.win(win)?;
                self.focus.push(win);
                self.push_ui(UiEvent::FocusChanged { win: Some(win) });
                Ok(ApiOk::Unit)
            }
            ApiCall::FocusPop => {
                let popped = self.focus.pop();
                // A float that asked to close on blur is destroyed rather than merely hidden,
                // otherwise pickers accumulate invisibly.
                if let Some(w) = popped
                    && self.windows.get(&w).is_some_and(Window::close_on_blur)
                {
                    self.windows.remove(&w);
                    self.captures.remove(&w);
                    self.push_ui(UiEvent::WindowClosed { win: w });
                }
                let now = self.focus.current();
                self.push_ui(UiEvent::FocusChanged { win: now });
                Ok(ApiOk::Unit)
            }
            ApiCall::FocusCurrent => Ok(ApiOk::FocusedWin { win: self.focus.current() }),

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
            ApiCall::Notify { level, message } => {
                self.push_ui(UiEvent::Message { level, text: message });
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

        let selected = anchor.filter(|a| *a != at);
        let edit = match edit {
            TextEdit::DeleteSelection => match selected {
                Some(a) => TextEdit::DeleteRange { from: a, to: at },
                // Not an error: `<Del>` with nothing selected is an ordinary keystroke that this
                // window happens to have nothing to do with.
                None => return Ok(()),
            },
            TextEdit::Insert { text } => {
                if let Some(a) = selected {
                    self.apply_plan(win, buf, at, &TextEdit::DeleteRange { from: a, to: at })?;
                }
                TextEdit::Insert { text }
            }
            other => other,
        };

        let at = self.win(win)?.cursor;
        self.apply_plan(win, buf, at, &edit)?;
        let w = self.win_mut(win)?;
        w.anchor = None;
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
        let (buf, cursor, anchor) = (w.buf, w.cursor, w.anchor);
        let ns = self.selection_ns;

        let Some(b) = self.buffers.get_mut(&buf) else { return };
        let count = b.line_count();
        let (cleared_start, cleared_end) = b.clear_marks(ns, 0, count);

        let mut touched = (cleared_start, cleared_end);
        if let Some(a) = anchor.filter(|a| *a != cursor) {
            let (from, to) = if a <= cursor { (a, cursor) } else { (cursor, a) };
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
        ApiCall::RtpList => "rtp.list",
        ApiCall::GitStatus => "git.status",
        ApiCall::GitBranches { .. } => "git.branches",
        ApiCall::GitWorktrees { .. } => "git.worktrees",
        ApiCall::GitLog { .. } => "git.log",
        ApiCall::GitDiff { .. } => "git.diff",
        ApiCall::GitDefaultBranch => "git.defaultBranch",
        ApiCall::GitCreateBranch { .. } => "git.createBranch",
        ApiCall::GitCheckout { .. } => "git.checkout",
        ApiCall::GitStage { .. } => "git.stage",
        ApiCall::GitUnstage { .. } => "git.unstage",
        ApiCall::GitCommit { .. } => "git.commit",
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
