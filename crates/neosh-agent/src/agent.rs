//! The agent loop.
//!
//! One turn is: ask the model, stream what it says, run any tools it asked for, feed the results
//! back, repeat until it stops asking. Everything interesting happens at the seams:
//!
//! * **Every tool call passes `tool.pre` before it runs.** A blocking hook can rewrite the
//!   arguments or refuse the call. A refusal is reported back to the model as an error
//!   `tool_result`, not swallowed — the model needs to know it was blocked so it can adapt, and a
//!   turn that silently loses a tool call is indistinguishable from a broken tool.
//! * **Cancellation is checked between every step**, so `<Esc>` interrupts a turn mid-stream
//!   rather than at the next convenient boundary.
//! * **A driver that runs its own loop gets no tool list.** Sending ours to an agent driver would
//!   advertise tools it cannot call.

use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use neosh_proto::{
    Activity, ContentBlock, HookName, HookOutcome, HookPayload, Message, MessageLevel,
    ModelSelection, Role, SessionId, StopReason, ToolCall, ToolCallId, ToolResult, TurnId,
    TurnRequest, Usage,
};
use neosh_provider::ProviderRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::hooks::{self, HookRegistry};
use crate::permission::PermissionLayer;
use crate::session::{Prompt, Session};
use crate::store::SessionStore;
use crate::tools::{ToolCtx, ToolHandler, ToolRegistry};
use crate::turn::{TurnAssembler, TurnUpdate};
use crate::PluginBridge;

/// Upper bound on tool rounds within one turn.
///
/// A model that keeps calling tools forever is a real failure mode; without a bound it burns the
/// user's budget silently. Hitting it ends the turn with an explicit reason rather than a hang.
const MAX_TOOL_ROUNDS: usize = 32;

/// How a turn ends when a blocking hook said no.
///
/// Not [`StopReason::Cancelled`], which is what `<Esc>` means. The two used to be the same value,
/// so a turn a policy plugin refused was reported to everything downstream as one the person had
/// interrupted — which is a different fact with a different remedy, and the reason the plugin gave
/// was thrown away on the way past. It travels in the explanation, where a transcript can print it
/// under the question it refused.
///
/// `category` says who, because "refused" from a model and "refused" from a budget plugin are
/// answered by different people.
fn refused(by: &str, reason: String) -> StopReason {
    StopReason::Refusal { category: Some(by.to_string()), explanation: Some(reason) }
}

/// Everything the host needs to render or forward.
///
/// Every variant says which conversation it came from. Turns run per conversation and several can
/// be in flight at once, so a receiver that assumed "the current one" would write one
/// conversation's answer into another's transcript — which is exactly the bug switching used to be
/// refused in order to avoid.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TurnStarted { session: SessionId, turn: TurnId },
    Token { session: SessionId, turn: TurnId, text: String },
    Thinking { session: SessionId, turn: TurnId, text: String },
    ToolStarted { session: SessionId, turn: TurnId, call: ToolCall },
    ToolFinished { session: SessionId, turn: TurnId, call: ToolCall, result: ToolResult },
    TurnEnded { session: SessionId, turn: TurnId, stop_reason: StopReason, usage: Usage },
    /// What the turn has produced so far is now in the conversation's messages.
    ///
    /// The host keeps its own record of a running turn, because until this lands there is nothing
    /// to rebuild a transcript from — and for an agent driver, which streams its whole loop down
    /// one connection, that is the entire turn rather than the tail of one round. This is the
    /// moment that record stops being the truth and starts being a duplicate of it.
    Committed { session: SessionId, turn: TurnId },
    /// A message typed while the turn was running has been taken into it.
    ///
    /// Emitted at the moment it enters the conversation rather than when it was typed, because
    /// until then it is a draft the user can still change their mind about — and a transcript that
    /// shows a question the model has not been told about is a transcript that is lying.
    Steered { session: SessionId, turn: TurnId, prompt: Prompt },
    /// The driver's own loop reported on itself — a sub-agent, a plan, a compaction, the commands
    /// it accepts. See [`neosh_proto::Activity`].
    ///
    /// Not folded into the other variants and not persisted with the messages: this is a live
    /// account of *how* the answer is being produced, and it is true only while it is happening. A
    /// plan from yesterday's turn read back out of a transcript would be a checklist for work that
    /// is already finished.
    Activity { session: SessionId, turn: TurnId, activity: Activity },
    Notice { session: SessionId, level: MessageLevel, text: String },
}

impl AgentEvent {
    /// Which conversation this belongs to, for a receiver deciding whether to draw it.
    pub fn session(&self) -> &SessionId {
        match self {
            Self::TurnStarted { session, .. }
            | Self::Token { session, .. }
            | Self::Thinking { session, .. }
            | Self::ToolStarted { session, .. }
            | Self::ToolFinished { session, .. }
            | Self::TurnEnded { session, .. }
            | Self::Committed { session, .. }
            | Self::Steered { session, .. }
            | Self::Activity { session, .. }
            | Self::Notice { session, .. } => session,
        }
    }
}

/// Every view of a workspace gets every event.
///
/// A workspace outlives the terminal that opened it, so it can have more than one view — and each
/// of them has to see the whole stream. A view that missed one `ToolFinished` draws a card that
/// never stops spinning, and nothing afterwards puts it right.
///
/// A list of channels rather than a `broadcast`, whose lagging consumers drop the *oldest* event
/// and say nothing about it — silently, and exactly under load, which is when a turn is producing
/// the most. Unbounded for the reason the single channel always was: the alternative is an agent
/// loop that blocks on the slowest terminal attached to it.
#[derive(Default)]
pub struct Fanout {
    views: std::sync::Mutex<Vec<mpsc::UnboundedSender<AgentEvent>>>,
}

impl Fanout {
    /// Everything from here on.
    ///
    /// Deliberately not a replay: what a view connecting late needs is the *state*, which is
    /// [`neosh_core::Editor::republish`]'s job, not a second copy of the deltas that built it.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<AgentEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.views.lock().expect("fanout lock poisoned").push(tx);
        rx
    }

    /// How many views are listening.
    ///
    /// Zero is an ordinary state, not a shutdown: every terminal has been closed and the turns are
    /// still running. Sending into none of them is what that costs, and it is the point.
    pub fn views(&self) -> usize {
        self.views.lock().expect("fanout lock poisoned").len()
    }

    fn send(&self, ev: AgentEvent) {
        let mut views = self.views.lock().expect("fanout lock poisoned");
        // A closed channel is a view that has gone away. Pruning on send is what stops the list
        // growing for the lifetime of the workspace, and it costs nothing when nobody has left.
        views.retain(|tx| tx.send(ev.clone()).is_ok());
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub turn: TurnId,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

/// Shared, independently-lockable state.
///
/// The registries and the session sit behind their own locks rather than inside one `&mut Agent`
/// because a turn runs for minutes and, while it runs, plugins keep calling the API — a hook that
/// asked `tool.list()` mid-veto would deadlock against the very turn it was inspecting. Every
/// critical section here is short and never spans an `await`.
pub struct Agent {
    sessions: Arc<std::sync::Mutex<SessionStore>>,
    tools: Arc<std::sync::RwLock<ToolRegistry>>,
    hooks: Arc<std::sync::RwLock<HookRegistry>>,
    providers: Arc<std::sync::RwLock<ProviderRegistry>>,
    /// Swappable, because a config reload can tighten permissions and a layer fixed at launch
    /// would report success while changing nothing.
    permissions: Arc<std::sync::RwLock<Arc<PermissionLayer>>>,
    events: Fanout,
}

impl Agent {
    pub fn new(
        session: Session,
        permissions: Arc<PermissionLayer>,
        providers: ProviderRegistry,
    ) -> (Self, mpsc::UnboundedReceiver<AgentEvent>) {
        let events = Fanout::default();
        // The caller's own stream is an ordinary subscription. There is nothing special about
        // being first, which is what makes a second view a second call rather than a second shape.
        let rx = events.subscribe();
        (
            Self {
                sessions: Arc::new(std::sync::Mutex::new(SessionStore::new(session))),
                tools: Arc::new(std::sync::RwLock::new(ToolRegistry::new())),
                hooks: Arc::new(std::sync::RwLock::new(HookRegistry::new())),
                providers: Arc::new(std::sync::RwLock::new(providers)),
                permissions: Arc::new(std::sync::RwLock::new(permissions)),
                events,
            },
            rx,
        )
    }

    /// Another view of this workspace, from now on.
    ///
    /// Takes `&self`, so a view can attach while a turn is running — which is the case that
    /// matters, since a turn running is the reason to open a second window.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<AgentEvent> {
        self.events.subscribe()
    }

    /// How many views are listening. Zero while turns run is ordinary. See [`Fanout::views`].
    pub fn views(&self) -> usize {
        self.events.views()
    }

    // ---- shared state -------------------------------------------------
    //
    // Guards are handed out rather than the values, so callers cannot accidentally hold one across
    // an await.

    /// The conversation currently on screen.
    ///
    /// Returns a guard that derefs to the active [`Session`], so every existing caller reads and
    /// writes "the conversation" without knowing there are others. Switching swaps what this
    /// points at; nothing else has to change.
    pub fn session(&self) -> ActiveSession<'_> {
        ActiveSession(self.sessions.lock().expect("session lock poisoned"))
    }

    /// Every conversation. Held for as short a time as `session()` and never across an await.
    pub fn sessions(&self) -> std::sync::MutexGuard<'_, SessionStore> {
        self.sessions.lock().expect("session lock poisoned")
    }

    pub fn tools(&self) -> std::sync::RwLockReadGuard<'_, ToolRegistry> {
        self.tools.read().expect("tool registry poisoned")
    }

    pub fn tools_mut(&self) -> std::sync::RwLockWriteGuard<'_, ToolRegistry> {
        self.tools.write().expect("tool registry poisoned")
    }

    pub fn hooks(&self) -> std::sync::RwLockReadGuard<'_, HookRegistry> {
        self.hooks.read().expect("hook registry poisoned")
    }

    pub fn hooks_mut(&self) -> std::sync::RwLockWriteGuard<'_, HookRegistry> {
        self.hooks.write().expect("hook registry poisoned")
    }

    pub fn providers(&self) -> std::sync::RwLockReadGuard<'_, ProviderRegistry> {
        self.providers.read().expect("provider registry poisoned")
    }

    pub fn providers_mut(&self) -> std::sync::RwLockWriteGuard<'_, ProviderRegistry> {
        self.providers.write().expect("provider registry poisoned")
    }

    /// Forget everything a plugin registered.
    pub fn permissions(&self) -> Arc<PermissionLayer> {
        self.permissions.read().expect("permission lock poisoned").clone()
    }

    /// Replace the permission layer. Takes effect on the next check, including inside a turn that
    /// is already running.
    pub fn set_permissions(&self, layer: PermissionLayer) {
        *self.permissions.write().expect("permission lock poisoned") = Arc::new(layer);
    }

    pub fn remove_plugin(&self, plugin: &neosh_proto::PluginId) {
        self.tools_mut().remove_plugin(plugin);
        self.hooks_mut().remove_plugin(plugin);
    }

    fn emit(&self, ev: AgentEvent) {
        // No views is not an error: every terminal may have been closed while this turn runs on.
        // Dropped receivers are pruned by the send itself.
        self.events.send(ev);
    }

    /// Reach one conversation by id, whether or not it is the one on screen.
    ///
    /// `None` means it has been closed. That can happen while its own turn is still running, and
    /// it is a reason for the turn to stop rather than a reason to panic.
    fn with<T>(&self, id: &SessionId, f: impl FnOnce(&mut Session) -> T) -> Option<T> {
        let mut store = self.sessions();
        store.get_mut(id).map(f)
    }

    /// Say something to a turn that is already running.
    ///
    /// It is taken into the conversation at the next gap — between one round of tool calls and the
    /// next, or in place of the turn ending. That is what makes it *steering* rather than a queued
    /// follow-up: the model sees it while it is still working, and can change what it does next.
    ///
    /// Not injected into the provider stream, because a stream in flight cannot be interrupted
    /// without discarding what has already been generated. The gap between rounds is the earliest
    /// honest moment.
    ///
    /// Addressed to a conversation, because what you type belongs to the one you typed it in — not
    /// to whichever turn happens to reach a gap first.
    pub fn steer(&self, session: &SessionId, prompt: Prompt) {
        self.with(session, |s| s.steering.push(prompt));
    }

    /// Whether anything is waiting to be said.
    pub fn steering_count(&self, session: &SessionId) -> usize {
        self.with(session, |s| s.steering.len()).unwrap_or(0)
    }

    /// What is waiting, without taking it. For drawing it.
    pub fn steering_queue(&self, session: &SessionId) -> Vec<Prompt> {
        self.with(session, |s| s.steering.clone()).unwrap_or_default()
    }

    /// Take what is waiting, leaving the queue empty.
    pub fn take_steering(&self, session: &SessionId) -> Vec<Prompt> {
        self.with(session, |s| std::mem::take(&mut s.steering)).unwrap_or_default()
    }

    /// Take the most recently queued message back out, if there is one.
    ///
    /// The last rather than the first, because it is the one you just typed and therefore the one
    /// you meant to change. Queued messages are taken into the turn oldest-first, so this can race
    /// a gap and find the queue already empty — which is why it answers with what it removed
    /// instead of a count somebody would have to check twice.
    pub fn unqueue_last(&self, session: &SessionId) -> Option<Prompt> {
        self.with(session, |s| s.steering.pop()).flatten()
    }

    /// The policy a turn runs under: the conversation's mode, rooted at the conversation's
    /// directory.
    ///
    /// Both halves are per conversation for the same reason. The shared layer follows whichever
    /// conversation is *on screen*, which is right for a plugin asking a question and wrong for a
    /// turn — the turn may not be the one on screen, and by the time it finishes it may not be the
    /// one on screen either. A mode taken from the screen would mean looking at a read-only
    /// conversation while another one is mid-edit silently changed what that other one was allowed
    /// to do.
    ///
    /// A conversation with no mode of its own takes the shared one, which is where `config.toml`
    /// lands. That is what keeps the configured default a default rather than a one-time value
    /// stamped into every conversation ever created.
    fn permissions_for(&self, session: &SessionId, cwd: &Path) -> Arc<PermissionLayer> {
        let base = self.permissions();
        let mode = self.with(session, |s| s.permission_mode).flatten();
        let rooted = !cwd.as_os_str().is_empty() && base.workspace() != cwd;
        match (mode.filter(|m| *m != base.mode()), rooted) {
            (None, false) => base,
            (None, true) => Arc::new(base.rooted_at(cwd)),
            (Some(m), false) => Arc::new((*base).clone().with_mode(m)),
            (Some(m), true) => Arc::new(base.rooted_at(cwd).with_mode(m)),
        }
    }

    /// What this conversation lets the agent do without asking, resolved.
    ///
    /// The conversation's own answer when it has one, the configured one otherwise — never `None`,
    /// because every caller of this wants a mode and not a question about whether one was set.
    ///
    /// The layer's own mode is the *configured* one and stays that way, whatever any conversation
    /// is set to. That is what makes the fallback a fallback: put one conversation in `deny` while
    /// the shared layer follows it, and the next conversation you open — which has never been given
    /// a mode and so falls back — inherits `deny` from a setting that was never about it.
    pub fn permission_mode(&self, session: &SessionId) -> neosh_proto::PermissionMode {
        self.with(session, |s| s.permission_mode).flatten().unwrap_or_else(|| self.permissions().mode())
    }

    /// Give this conversation a mode of its own, taking effect on the turn already running.
    pub fn set_permission_mode(&self, session: &SessionId, mode: neosh_proto::PermissionMode) {
        self.with(session, |s| s.permission_mode = Some(mode));
    }

    /// The policy a question asked *outside* any turn is answered by: the most recent
    /// conversation.
    ///
    /// A plugin calling `neosh.permit` is not in a turn and has no conversation of its own. With
    /// one terminal that was "the one being looked at"; with several there is no such thing, so it
    /// is the one most recently arrived in anywhere — which is the same answer whenever it
    /// mattered, and an answer at all when nothing is attached.
    fn permissions_here(&self) -> Arc<PermissionLayer> {
        let here = self.sessions().current_id().clone();
        let base = self.permissions();
        match self.with(&here, |s| s.permission_mode).flatten().filter(|m| *m != base.mode()) {
            None => base,
            Some(m) => Arc::new((*base).clone().with_mode(m)),
        }
    }

    pub fn selection(&self) -> Option<ModelSelection> {
        self.session().selection.clone()
    }

    pub fn set_selection(&self, selection: ModelSelection) {
        self.session().selection = Some(selection);
    }


    /// Run a prompt through a model and return its text, touching nothing else.
    ///
    /// This is the engine behind branch names, commit messages and thread titles. It deliberately
    /// shares almost nothing with [`Self::run_turn_with`]: no session history is read or written,
    /// no tools are advertised, no hooks fire and no `AgentEvent` is emitted. A commit-message
    /// request that appended to the conversation would change what the agent believes it was asked
    /// to do — and a user who then asks "what were we doing?" would get the commit message back.
    pub async fn complete(
        &self,
        prompt: String,
        system: Option<String>,
        selection: Option<ModelSelection>,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let selection = selection
            .or_else(|| self.selection())
            .ok_or_else(|| "no model selected".to_string())?;

        let resolved = {
            let reg = self.providers();
            reg.resolve(&selection).map(|(p, i)| (p, i.clone()))
        };
        let (provider, instance) = resolved.ok_or_else(|| {
            format!("no driver for instance {} — is the plugin that provides it loaded?", selection.instance)
        })?;

        // A knob spelled into the prompt applies here too. A branch name generated at `ultrathink`
        // is silly, but a *level the endpoint would reject* is not silly, it is a failed request —
        // and this path shares the conversation's selection, so it inherits whatever was chosen
        // there.
        let mut selection = selection;
        let words = neosh_provider::prompt_injections(&instance.models, &mut selection);
        let mut messages =
            vec![Message { role: Role::User, content: vec![ContentBlock::Text { text: prompt }] }];
        neosh_provider::inject(&mut messages, &words);

        let request = TurnRequest {
            selection,
            // A one-shot generation is nobody's conversation: it borrows the model and the
            // directory and leaves nothing behind, so a driver that keeps state per conversation
            // has nothing to keep.
            conversation: SessionId::from(""),
            cwd: self.session().cwd.clone(),
            // Nobody's conversation, so there is nothing to pick up and nothing to leave behind.
            resume: None,
            system,
            messages,
            tools: Vec::new(),
            max_output_tokens: None,
        };

        let mut assembler = TurnAssembler::new();
        let mut stream = provider.stream(&instance, request, cancel.clone());
        let mut failure: Option<String> = None;
        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                ev = stream.next() => ev,
            };
            let Some(ev) = next else { break };
            for update in assembler.push(ev) {
                if let TurnUpdate::Error { message, .. } = update {
                    // Keep reading: a provider may report a recoverable error mid-stream and then
                    // continue. Only an empty result makes this the answer.
                    failure.get_or_insert(message);
                }
            }
        }
        drop(stream);

        if cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }

        let (messages, stop, _usage) = assembler.finish();
        let text: String = messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        if text.trim().is_empty() {
            return Err(match (failure, stop) {
                (Some(e), _) => e,
                (None, StopReason::Error { message }) => message,
                (None, StopReason::Refusal { .. }) => "the model refused".to_string(),
                _ => "the model returned nothing".to_string(),
            });
        }
        Ok(text)
    }

    /// Decide a capability, asking the user when the policy says to.
    ///
    /// The single entry point for "may this happen". Both callers reach it — a built-in tool
    /// through `ToolCtx::permit`, and a plugin through `neosh.permit` — so `ask` mode means the
    /// same thing whichever side of the boundary the request came from. Two implementations of this
    /// is how one of them silently stops asking.
    pub async fn decide(
        &self,
        bridge: &dyn PluginBridge,
        capability: neosh_proto::Capability,
        turn: Option<TurnId>,
    ) -> neosh_proto::PermissionDecision {
        match self.permissions_here().check(&capability) {
            neosh_proto::PermissionDecision::Prompt => {
                let hooks: Vec<_> =
                    self.hooks().blocking(HookName::PermissionPre).cloned().collect();
                if hooks.is_empty() {
                    // Fail closed. An unanswerable request that ran anyway would make `ask` mode a
                    // lie, and the message says what to do about it.
                    return neosh_proto::PermissionDecision::Deny {
                        reason: "needs approval, and nothing is able to ask".into(),
                    };
                }
                crate::tools::Approver::ask(&HookApprover { hooks, bridge, turn }, capability).await
            }
            other => other,
        }
    }

    /// Run one user turn to completion in the conversation on screen.
    pub async fn run_turn(&self, bridge: &dyn PluginBridge, prompt: impl Into<Prompt>) -> TurnOutcome {
        self.run_turn_with(bridge, prompt, CancellationToken::new()).await
    }

    /// The same, with a caller-supplied cancellation token.
    ///
    /// The host owns the token so it can interrupt a turn without taking the lock the turn itself
    /// holds — otherwise `<Esc>` would have to wait for the thing it is trying to stop.
    pub async fn run_turn_with(
        &self,
        bridge: &dyn PluginBridge,
        prompt: impl Into<Prompt>,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        let session = self.sessions().current_id().clone();
        self.run_turn_in(bridge, session, prompt.into(), cancel).await
    }

    /// Run one user turn to completion **in a named conversation**.
    ///
    /// Every read and write goes through the id rather than through "the active session", so the
    /// turn is unaffected by switching, and several turns can be in flight in different
    /// conversations at once. The conversation may even be closed underneath it; that reads as a
    /// cancellation, because there is nowhere left to put the answer.
    pub async fn run_turn_in(
        &self,
        bridge: &dyn PluginBridge,
        session: SessionId,
        prompt: Prompt,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        let Prompt { text, images } = prompt;
        let turn = TurnId::new();
        self.with(&session, |s| s.active_turn = Some(turn.clone()));

        let end = |me: &Self, stop: StopReason, usage: Usage| {
            me.emit(AgentEvent::TurnEnded {
                session: session.clone(),
                turn: turn.clone(),
                stop_reason: stop.clone(),
                usage,
            });
            TurnOutcome { turn: turn.clone(), stop_reason: stop, usage }
        };

        // --- turn.pre: a plugin may rewrite or refuse the prompt ---------
        let turn_pre: Vec<_> = self.hooks().blocking(HookName::TurnPre).cloned().collect();
        let (outcome, payload) = hooks::run_blocking(
            turn_pre,
            bridge,
            HookName::TurnPre,
            HookPayload::TurnPre { turn: turn.clone(), text: text.clone() },
        )
        .await;
        if let HookOutcome::Veto { reason } = outcome {
            self.with(&session, |s| s.active_turn = None);
            self.emit(AgentEvent::Notice {
                session: session.clone(),
                level: MessageLevel::Warn,
                text: format!("turn blocked: {reason}"),
            });
            return end(self, refused("a plugin", reason), Usage::default());
        }
        let text = match payload {
            HookPayload::TurnPre { text, .. } => text,
            _ => text,
        };
        self.observe(bridge, HookName::TurnPre, HookPayload::TurnPre {
            turn: turn.clone(),
            text: text.clone(),
        });

        self.emit(AgentEvent::TurnStarted { session: session.clone(), turn: turn.clone() });
        // The hook may have rewritten the words; what was attached is not its business and rides
        // through untouched.
        //
        // Nothing to push when there is nothing to say. A turn opened to hold something the agent
        // has *already* started saying — a backgrounded command finishing, and the agent answering
        // its own notification about it — is a real turn with a real answer in it and no question
        // above it. An empty user message there is a blank row in the transcript and, worse, a
        // question the driver would go and ask.
        let asking = !text.is_empty() || !images.is_empty();
        if asking {
            self.with(&session, |s| s.push_user(&Prompt { text: text.clone(), images }));
        }

        // --- turn.route: a plugin may say who answers ---------------------
        //
        // After the words are settled, because routing on the prompt means routing on the prompt
        // that will actually be sent; and before the driver is resolved, because that is the last
        // moment the answer is still a question. See [`HookPayload::TurnRoute`].
        let selection = self.with(&session, |s| s.selection.clone()).flatten();
        let turn_route: Vec<_> = self.hooks().blocking(HookName::TurnRoute).cloned().collect();
        let (outcome, payload) = hooks::run_blocking(
            turn_route,
            bridge,
            HookName::TurnRoute,
            HookPayload::TurnRoute {
                turn: turn.clone(),
                text: text.clone(),
                session: Some(session.clone()),
                selection: selection.clone(),
            },
        )
        .await;
        if let HookOutcome::Veto { reason } = outcome {
            self.with(&session, |s| s.active_turn = None);
            self.emit(AgentEvent::Notice {
                session: session.clone(),
                level: MessageLevel::Warn,
                text: format!("turn refused by a router: {reason}"),
            });
            return end(self, refused("a router", reason), Usage::default());
        }
        let selection = match payload {
            HookPayload::TurnRoute { selection, .. } => selection,
            _ => selection,
        };
        self.observe(bridge, HookName::TurnRoute, HookPayload::TurnRoute {
            turn: turn.clone(),
            text,
            session: Some(session.clone()),
            selection: selection.clone(),
        });
        // Written back, so the rest of the workspace agrees with the turn: the footer, the model
        // strip and a `^P` opened mid-answer all read the conversation rather than this loop, and a
        // routed turn that left them saying the old model would be reporting the wrong one for as
        // long as it ran.
        if selection != self.with(&session, |s| s.selection.clone()).flatten() {
            let chosen = selection.clone();
            self.with(&session, |s| s.selection = chosen);
        }

        let Some(selection) = selection else {
            self.with(&session, |s| s.active_turn = None);
            return end(
                self,
                StopReason::Error { message: "no model selected".into() },
                Usage::default(),
            );
        };

        let mut total = Usage::default();
        let mut stop = StopReason::EndTurn;

        for round in 0..MAX_TOOL_ROUNDS {
            if cancel.is_cancelled() {
                stop = StopReason::Cancelled;
                break;
            }

            // Clone out of the registry so the lock is not held while the turn runs.
            let resolved = {
                let reg = self.providers();
                reg.resolve(&selection).map(|(p, i)| (p, i.clone()))
            };
            let Some((provider, instance)) = resolved else {
                stop = StopReason::Error {
                    message: format!(
                        "no driver for instance {} — is the plugin that provides it loaded?",
                        selection.instance
                    ),
                };
                break;
            };

            // One guard, not two: temporaries in a struct literal live to the end of the
            // statement, so taking the session lock twice here would deadlock against itself.
            let Some((cwd, system, mut messages, resume)) = self.with(&session, |s| {
                (s.cwd.clone(), s.system.clone(), s.messages.clone(), s.resume.clone())
            }) else {
                // Closed while its own turn was running. There is nowhere to put the answer, so
                // this is a cancellation rather than an error to report into a transcript that no
                // longer exists.
                stop = StopReason::Cancelled;
                break;
            };
            // Options that are said rather than sent. A vendor gating depth on a word in the
            // message — `ultrathink`, and everything that has copied it — is still a setting: you
            // choose it in the same picker, it shows in the same strip, and it applies to the same
            // turn. It just cannot travel as a parameter, so it travels in the message this turn is
            // about to send, and *only* that copy: the transcript keeps what you actually typed, and
            // the round trips after a tool call do not each say it again. See
            // `neosh_provider::injection`.
            let mut selection = selection.clone();
            let words = neosh_provider::prompt_injections(&instance.models, &mut selection);
            // The first round only. A tool result travels as a user message, so every round after
            // this one has a "last user message" that nobody typed — putting the word on that would
            // say it again after every tool call, each time as a fresh instruction.
            if round == 0 {
                neosh_provider::inject(&mut messages, &words);
            }

            let request = TurnRequest {
                selection,
                conversation: session.clone(),
                cwd: cwd.clone(),
                // What this driver called the conversation last time it ran one, which is the only
                // thing standing between reopening it and an agent that has forgotten it.
                resume,
                system,
                messages,
                // An agent driver brings its own tools; advertising ours would list tools it
                // cannot call.
                tools: if provider.delegates_agent_loop() {
                    Vec::new()
                } else {
                    self.tools().defs()
                },
                max_output_tokens: None,
            };

            let mut assembler = TurnAssembler::new();
            let mut stream = provider.stream(&instance, request, cancel.clone());

            // Only for a driver that runs its own loop. It calls tools we never see, so its stream
            // is the only place the calls exist — and reporting them as they happen is the
            // difference between a transcript that says what is going on and one that catches up
            // at the end. Kept so a result can be matched back to the call it answers.
            let reporting = provider.delegates_agent_loop();
            let mut announced: std::collections::HashMap<ToolCallId, ToolCall> =
                std::collections::HashMap::new();

            loop {
                let next = tokio::select! {
                    biased;
                    () = cancel.cancelled() => None,
                    ev = stream.next() => ev,
                };
                let Some(ev) = next else { break };
                for update in assembler.push(ev) {
                    match update {
                        TurnUpdate::Text { text } => self.emit(AgentEvent::Token {
                            session: session.clone(),
                            turn: turn.clone(),
                            text,
                        }),
                        TurnUpdate::Thinking { text } => self.emit(AgentEvent::Thinking {
                            session: session.clone(),
                            turn: turn.clone(),
                            text,
                        }),
                        TurnUpdate::Error { message, .. } => self.emit(AgentEvent::Notice {
                            session: session.clone(),
                            level: MessageLevel::Error,
                            text: message,
                        }),
                        // A call we are going to run ourselves is handled after the stream
                        // closes, so tools run in the order the model produced them rather than
                        // the order their JSON happened to finish. A call somebody else is
                        // running has already started, and saying so later would be a lie about
                        // when it happened.
                        TurnUpdate::ToolReady { id, name, input } if reporting => {
                            let call =
                                ToolCall { id: id.clone(), turn: turn.clone(), name, input };
                            announced.insert(id, call.clone());
                            self.emit(AgentEvent::ToolStarted {
                                session: session.clone(),
                                turn: turn.clone(),
                                call,
                            });
                        }
                        TurnUpdate::ToolResult { id, content, is_error } => {
                            // Only a delegating driver sends these; our own tools report through
                            // `run_tool`, which knows a great deal more about the call.
                            if let Some(call) = announced.remove(&id) {
                                self.emit(AgentEvent::ToolFinished {
                                    session: session.clone(),
                                    turn: turn.clone(),
                                    call,
                                    result: ToolResult { content, is_error },
                                });
                            }
                        }
                        // Every driver, not only a delegating one: a model driver that grows a
                        // plan tomorrow should need no change here.
                        TurnUpdate::Activity { activity } => self.emit(AgentEvent::Activity {
                            session: session.clone(),
                            turn: turn.clone(),
                            activity,
                        }),
                        TurnUpdate::ToolReady { .. } | TurnUpdate::ToolMalformed { .. } => {}
                    }
                }
            }
            drop(stream);

            let cancelled = cancel.is_cancelled();
            let calls = assembler.tool_calls();
            let (produced, this_stop, usage) = assembler.finish();
            total.merge(&usage);
            let recorded = !produced.is_empty();
            self.with(&session, |s| {
                s.add_usage(&usage);
                for m in produced {
                    s.push_message(m);
                }
            });
            if recorded {
                self.emit(AgentEvent::Committed {
                    session: session.clone(),
                    turn: turn.clone(),
                });
            }

            if cancelled {
                stop = StopReason::Cancelled;
                break;
            }
            stop = this_stop;
            // A driver that runs its own loop has already run these. Executing them again because
            // its last message happened to stop on `tool_use` would call every tool twice.
            if reporting || !matches!(stop, StopReason::ToolUse) || calls.is_empty() {
                // Nothing left to do — unless somebody typed while it was working. Carrying on
                // rather than ending is the whole point: a follow-up that starts a new turn has
                // lost the chance to change what this one does.
                if cancel.is_cancelled() || !self.take_steering_into(&session, &turn) {
                    break;
                }
                continue;
            }

            let mut results = Vec::new();
            for (id, name, input) in calls {
                if cancel.is_cancelled() {
                    break;
                }
                let call = ToolCall { id: id.clone(), turn: turn.clone(), name, input };
                results.push((id, self.run_tool(bridge, &session, &cwd, call, &cancel).await));
            }
            let recorded = !results.is_empty();
            self.with(&session, |s| s.push_tool_results(results));
            if recorded {
                self.emit(AgentEvent::Committed {
                    session: session.clone(),
                    turn: turn.clone(),
                });
            }
            // The gap between rounds: the model is about to be asked again anyway, so anything
            // typed since the last one rides along with the tool results.
            //
            // Unless there is no next round. Taking it in here is a promise that the model will
            // be asked, and a cancelled turn breaks at the top of the loop instead — so this
            // pushed the message into the conversation, drew it in the transcript as a question,
            // and then ended without ever putting it to anybody. Left in the queue it is picked
            // up by the leftover path when the turn ends, which starts a turn that does ask it.
            if !cancel.is_cancelled() {
                self.take_steering_into(&session, &turn);
            }

            if round + 1 == MAX_TOOL_ROUNDS {
                stop = StopReason::Error {
                    message: format!("stopped after {MAX_TOOL_ROUNDS} tool rounds in one turn"),
                };
            }
        }

        self.with(&session, |s| s.active_turn = None);
        self.observe(bridge, HookName::TurnPost, HookPayload::TurnPost {
            turn: turn.clone(),
            stop_reason: stop.clone(),
            usage: total,
        });
        end(self, stop, total)
    }

    /// Move anything queued into the conversation. Returns whether there was anything.
    ///
    /// Everything waiting becomes *one* message, which is not a tidying-up: a driver that keeps
    /// the conversation on its own side is handed the newest user message and nothing else, so two
    /// things queued into the same gap meant the older one was drawn in the transcript as a
    /// question and then never asked. It is the same join the end-of-turn path does with what is
    /// left over, for the same reason — the queue is unsent conversation, and it goes in the order
    /// it was typed.
    fn take_steering_into(&self, session: &SessionId, turn: &TurnId) -> bool {
        let queued = self.take_steering(session);
        if queued.is_empty() {
            return false;
        }
        let prompt = Prompt {
            text: queued.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n\n"),
            images: queued.into_iter().flat_map(|p| p.images).collect(),
        };
        self.with(session, |s| s.push_user(&prompt));
        // Drawn at the moment it enters the conversation, with whatever came with it — the picture
        // is part of the question, and a transcript showing the sentence without it is showing
        // half of what was asked.
        self.emit(AgentEvent::Steered {
            session: session.clone(),
            turn: turn.clone(),
            prompt,
        });
        true
    }

    /// Run one tool call, through the hook and permission gates.
    async fn run_tool(
        &self,
        bridge: &dyn PluginBridge,
        session: &SessionId,
        cwd: &Path,
        call: ToolCall,
        cancel: &CancellationToken,
    ) -> ToolResult {
        // Observers hear about the *attempt*, before any blocking hook can refuse it. An audit
        // plugin that only saw calls which survived policy would be blind to exactly the events it
        // exists to record.
        self.observe(bridge, HookName::ToolPre, HookPayload::ToolPre { call: call.clone() });

        // --- tool.pre: rewrite or refuse --------------------------------
        let tool_pre: Vec<_> = self.hooks().blocking(HookName::ToolPre).cloned().collect();
        let (outcome, payload) = hooks::run_blocking(
            tool_pre,
            bridge,
            HookName::ToolPre,
            HookPayload::ToolPre { call: call.clone() },
        )
        .await;

        if let HookOutcome::Veto { reason } = outcome {
            let result = ToolResult::error(format!("blocked by policy: {reason}"));
            // The model is told, so it can explain or route around the refusal rather than
            // silently losing the call.
            self.emit(AgentEvent::ToolFinished {
                session: session.clone(),
                turn: call.turn.clone(),
                call: call.clone(),
                result: result.clone(),
            });
            self.observe(bridge, HookName::ToolPost, HookPayload::ToolPost {
                call,
                result: result.clone(),
            });
            return result;
        }

        // A `tool.pre` hook may have rewritten the arguments.
        let call = match payload {
            HookPayload::ToolPre { call } => call,
            _ => call,
        };

        self.emit(AgentEvent::ToolStarted {
            session: session.clone(),
            turn: call.turn.clone(),
            call: call.clone(),
        });

        // Resolve the handler and release the registry lock before running it: a tool that
        // calls back into the API must not find the registry it came from locked.
        let handler = self.tools().get(&call.name).map(|e| e.handler.clone());
        let result = match handler {
            None => ToolResult::error(format!("no such tool: {}", call.name)),
            Some(handler) => match handler {
                ToolHandler::Builtin(t) => {
                    let approver = HookApprover {
                        hooks: self.hooks().blocking(HookName::PermissionPre).cloned().collect(),
                        bridge,
                        turn: Some(call.turn.clone()),
                    };
                    let ctx = ToolCtx {
                        // Rooted at the conversation this turn belongs to, not at whichever one is
                        // on screen: `src/main.rs` has to mean the same file for the whole turn,
                        // however much switching happens while it runs.
                        permissions: self.permissions_for(session, cwd),
                        // Only offer to ask if something is actually listening; otherwise `permit`
                        // would wait on a hook nobody registered.
                        approve: (!approver.hooks.is_empty()).then_some(&approver as &dyn crate::tools::Approver),
                    };
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => ToolResult::error("interrupted"),
                        r = t.run(call.input.clone(), &ctx) => r,
                    }
                }
                ToolHandler::Plugin(p) => tokio::select! {
                    biased;
                    () = cancel.cancelled() => ToolResult::error("interrupted"),
                    r = bridge.run_tool(&p, &call.name, call.input.clone()) => r,
                },
            },
        };

        self.emit(AgentEvent::ToolFinished {
            session: session.clone(),
            turn: call.turn.clone(),
            call: call.clone(),
            result: result.clone(),
        });
        self.observe(bridge, HookName::ToolPost, HookPayload::ToolPost {
            call,
            result: result.clone(),
        });
        result
    }

    /// Fire a hook at its observers. Never awaited, so an observer cannot slow a turn down.
    fn observe(&self, bridge: &dyn PluginBridge, hook: HookName, payload: HookPayload) {
        let regs: Vec<_> = self.hooks().observers(hook).cloned().collect();
        for r in regs {
            bridge.notify(&r.plugin, neosh_proto::PluginEvent::HookObserved {
                hook,
                payload: payload.clone(),
            });
        }
    }
}

/// Convenience for callers that only want the final text of a turn.
pub fn assistant_text(session: &Session) -> String {
    session
        .messages
        .iter()
        .rev()
        .find(|m| m.role == neosh_proto::Role::Assistant)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    neosh_proto::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Re-exported so hosts can build a `ToolCallId` when synthesizing calls in tests.
pub type CallId = ToolCallId;

/// A lock on the store, presented as the most recent conversation.
///
/// Exists so that `agent.session().messages` reads as one conversation after the store gained the
/// ability to hold several. Without it every call site would have to say
/// `agent.sessions().current()` and the borrow would be the caller's problem.
///
/// It is *not* "the conversation on screen" — there may be several screens, each somewhere else,
/// and which one a caller means is a question only the host can answer. Anything that has a view
/// to ask should ask it and reach the session by id.
pub struct ActiveSession<'a>(std::sync::MutexGuard<'a, SessionStore>);

impl std::ops::Deref for ActiveSession<'_> {
    type Target = Session;

    fn deref(&self) -> &Session {
        self.0.current()
    }
}

impl std::ops::DerefMut for ActiveSession<'_> {
    fn deref_mut(&mut self) -> &mut Session {
        self.0.current_mut()
    }
}

/// Turns "the policy wants the user asked" into an answer, by asking a plugin.
///
/// The `permission_pre` hook is a *blocking* hook, so the same rule applies as everywhere else: a
/// hook that does not answer in time is a veto. A permission prompt that failed open would be worse
/// than having no prompt at all.
struct HookApprover<'a> {
    hooks: Vec<crate::hooks::HookReg>,
    bridge: &'a dyn PluginBridge,
    /// Which turn is asking, so a prompt can say what it is for. `None` when a plugin asked
    /// outside a turn.
    turn: Option<TurnId>,
}

#[async_trait::async_trait]
impl crate::tools::Approver for HookApprover<'_> {
    async fn ask(&self, capability: neosh_proto::Capability) -> neosh_proto::PermissionDecision {
        let (outcome, _) = ask_hooks(
            self.hooks.clone(),
            self.bridge,
            HookPayload::PermissionPre {
                turn: self.turn.clone(),
                capability,
                title: None,
                // A built-in tool offers none: the question is "may this happen", and the answers
                // to that are yes and no.
                options: Vec::new(),
                chosen: None,
            },
        )
        .await;
        match outcome {
            HookOutcome::Veto { reason } => neosh_proto::PermissionDecision::Deny { reason },
            // `Continue` from a hook that was asked "may this happen" is a yes. `Modify` is read
            // the same way here, because a yes/no question has nothing else to carry back.
            _ => neosh_proto::PermissionDecision::Allow,
        }
    }
}

/// Run the `permission_pre` hooks and read back the payload they may have changed.
async fn ask_hooks(
    regs: Vec<crate::hooks::HookReg>,
    bridge: &dyn PluginBridge,
    payload: HookPayload,
) -> (HookOutcome, Option<String>) {
    let (outcome, payload) =
        hooks::run_blocking(regs, bridge, HookName::PermissionPre, payload).await;
    let chosen = match payload {
        HookPayload::PermissionPre { chosen, .. } => chosen,
        _ => None,
    };
    (outcome, chosen)
}

/// Sends an agent driver's *question* to whatever is listening for one.
///
/// Deliberately not [`DriverAsker`] with a wider payload, and the difference is one line: **policy
/// is never consulted.** A permission request asks whether something may happen, which
/// [`PermissionLayer`] exists to answer without a person; a question asks which of several things
/// the person wants, and there is no mode, rule or allow-list that knows. Routing questions through
/// the permission layer is what made `AskUserQuestion` fail silently — the configured default is
/// full access, so the layer said yes to a question, the driver sent a yes with no answers in it,
/// and the agent was told its user had ignored it. See ADR 0043.
///
/// The other half of the difference is what "no" means. A permission hook that times out is a veto
/// and the action is refused, because failing open on a permission is unsafe. A question hook that
/// times out is *nobody answered*, which is not a failure at all — it is a fact the agent is told
/// in a sentence, and it goes on from there.
pub struct DriverQuestioner {
    hooks: Arc<std::sync::RwLock<crate::hooks::HookRegistry>>,
    bridge: Arc<dyn PluginBridge>,
}

impl std::fmt::Debug for DriverQuestioner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DriverQuestioner")
    }
}

impl DriverQuestioner {
    pub fn new(agent: &Agent, bridge: Arc<dyn PluginBridge>) -> Self {
        Self { hooks: agent.hooks.clone(), bridge }
    }
}

#[async_trait::async_trait]
impl neosh_provider::ask::QuestionAsker for DriverQuestioner {
    async fn ask(
        &self,
        request: neosh_provider::ask::QuestionRequest,
    ) -> neosh_provider::ask::QuestionAnswers {
        use neosh_provider::ask::QuestionAnswers;

        let regs: Vec<_> = {
            let guard = self.hooks.read().expect("hook lock poisoned");
            guard.blocking(HookName::AskUser).cloned().collect()
        };
        // No panel, no plugin, no terminal attached. Said plainly rather than left to time out: a
        // turn that hangs for the hook's whole timeout on a workspace nobody is looking at is a
        // turn that looks wedged, and the honest answer is available immediately.
        if regs.is_empty() {
            return QuestionAnswers::nobody();
        }

        let (outcome, payload) = hooks::run_blocking(
            regs,
            self.bridge.as_ref(),
            HookName::AskUser,
            HookPayload::AskUser {
                session: Some(request.conversation),
                turn: None,
                cwd: request.cwd.display().to_string(),
                questions: request.questions,
                answers: None,
            },
        )
        .await;
        if let HookOutcome::Veto { reason } = outcome {
            return QuestionAnswers::Cancelled { reason };
        }
        match payload {
            // The answers come home the way a chosen permission option does: in a `Modify` that
            // carries the payload back with the field filled in.
            HookPayload::AskUser { answers: Some(answers), .. } if !answers.is_empty() => {
                QuestionAnswers::Answered(answers)
            }
            // Ran, and answered nothing. Not the same as no hook at all, and the agent is told the
            // difference: this one was shown to somebody.
            _ => QuestionAnswers::dismissed(),
        }
    }
}

/// Sends an agent driver's permission request to the same prompt a tool call reaches.
///
/// The driver has a question and no idea who answers it; this has the hook registry, the plugin
/// bridge and the permission layer, and no idea what ACP is. That separation is the point — see
/// ADR 0032.
///
/// Policy is consulted first, exactly as [`crate::tools::ToolCtx::permit`] does for a built-in
/// tool. Without that, `full access` would still open a prompt, and a workspace read would open one
/// too — for something neosh's own `read_file` does without asking. Only a `Prompt` reaches a
/// person, which is what makes the two paths one behaviour rather than two that resemble each
/// other.
pub struct DriverAsker {
    hooks: Arc<std::sync::RwLock<crate::hooks::HookRegistry>>,
    permissions: Arc<std::sync::RwLock<Arc<PermissionLayer>>>,
    bridge: Arc<dyn PluginBridge>,
}

impl std::fmt::Debug for DriverAsker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DriverAsker")
    }
}

impl DriverAsker {
    pub fn new(agent: &Agent, bridge: Arc<dyn PluginBridge>) -> Self {
        Self { hooks: agent.hooks.clone(), permissions: agent.permissions.clone(), bridge }
    }
}

#[async_trait::async_trait]
impl neosh_provider::approval::PermissionAsker for DriverAsker {
    async fn ask(
        &self,
        request: neosh_provider::approval::PermissionRequest,
    ) -> neosh_provider::approval::PermissionAnswer {
        use neosh_provider::approval::PermissionAnswer;

        let layer = {
            let guard = self.permissions.read().expect("permission lock poisoned");
            guard.rooted_at(&request.cwd)
        };
        match layer.check(&request.capability) {
            neosh_proto::PermissionDecision::Allow => return PermissionAnswer::Allow,
            neosh_proto::PermissionDecision::Deny { .. } => return PermissionAnswer::Deny,
            neosh_proto::PermissionDecision::Prompt => {}
        }

        let regs: Vec<_> = {
            let guard = self.hooks.read().expect("hook lock poisoned");
            guard.blocking(HookName::PermissionPre).cloned().collect()
        };
        // Fail closed, and for the same reason a hook that times out is a veto: an agent whose
        // request nobody can hear must not proceed as though somebody said yes.
        if regs.is_empty() {
            return PermissionAnswer::Deny;
        }

        let (outcome, chosen) = ask_hooks(
            regs,
            self.bridge.as_ref(),
            HookPayload::PermissionPre {
                turn: None,
                capability: request.capability,
                title: Some(request.title),
                options: request.options,
                chosen: None,
            },
        )
        .await;
        match outcome {
            HookOutcome::Veto { .. } => PermissionAnswer::Deny,
            // An option the prompt named is taken verbatim — that is the whole reason the agent's
            // own options travelled out there. A yes with nothing named is still a yes.
            _ => chosen.map_or(PermissionAnswer::Allow, PermissionAnswer::Option),
        }
    }
}
