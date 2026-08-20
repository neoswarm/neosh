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
    ContentBlock, HookName, HookOutcome, HookPayload, Message, MessageLevel, ModelSelection, Role,
    SessionId, StopReason, ToolCall, ToolCallId, ToolResult, TurnId, TurnRequest, Usage,
};
use neosh_provider::ProviderRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::hooks::{self, HookRegistry};
use crate::permission::PermissionLayer;
use crate::session::Session;
use crate::store::SessionStore;
use crate::tools::{ToolCtx, ToolHandler, ToolRegistry};
use crate::turn::{TurnAssembler, TurnUpdate};
use crate::PluginBridge;

/// Upper bound on tool rounds within one turn.
///
/// A model that keeps calling tools forever is a real failure mode; without a bound it burns the
/// user's budget silently. Hitting it ends the turn with an explicit reason rather than a hang.
const MAX_TOOL_ROUNDS: usize = 32;

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
    /// A message typed while the turn was running has been taken into it.
    ///
    /// Emitted at the moment it enters the conversation rather than when it was typed, because
    /// until then it is a draft the user can still change their mind about — and a transcript that
    /// shows a question the model has not been told about is a transcript that is lying.
    Steered { session: SessionId, turn: TurnId, text: String },
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
            | Self::Steered { session, .. }
            | Self::Notice { session, .. } => session,
        }
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
    events: mpsc::UnboundedSender<AgentEvent>,
}

impl Agent {
    pub fn new(
        session: Session,
        permissions: Arc<PermissionLayer>,
        providers: ProviderRegistry,
    ) -> (Self, mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                sessions: Arc::new(std::sync::Mutex::new(SessionStore::new(session))),
                tools: Arc::new(std::sync::RwLock::new(ToolRegistry::new())),
                hooks: Arc::new(std::sync::RwLock::new(HookRegistry::new())),
                providers: Arc::new(std::sync::RwLock::new(providers)),
                permissions: Arc::new(std::sync::RwLock::new(permissions)),
                events: tx,
            },
            rx,
        )
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
        // A dropped receiver means the host is shutting down; losing UI events then is fine.
        let _ = self.events.send(ev);
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
    pub fn steer(&self, session: &SessionId, text: String) {
        self.with(session, |s| s.steering.push(text));
    }

    /// Whether anything is waiting to be said.
    pub fn steering_count(&self, session: &SessionId) -> usize {
        self.with(session, |s| s.steering.len()).unwrap_or(0)
    }

    /// What is waiting, without taking it. For drawing it.
    pub fn steering_texts(&self, session: &SessionId) -> Vec<String> {
        self.with(session, |s| s.steering.clone()).unwrap_or_default()
    }

    /// Take what is waiting, leaving the queue empty.
    pub fn take_steering(&self, session: &SessionId) -> Vec<String> {
        self.with(session, |s| std::mem::take(&mut s.steering)).unwrap_or_default()
    }

    /// The policy, rooted at the directory a turn is running in.
    ///
    /// The shared layer's root follows the conversation on screen, which is right for a plugin
    /// asking a question and wrong for a turn: the turn may not be the one on screen, and by the
    /// time it finishes it may not be the one on screen either.
    fn permissions_in(&self, cwd: &Path) -> Arc<PermissionLayer> {
        let base = self.permissions();
        if cwd.as_os_str().is_empty() || base.workspace() == cwd {
            return base;
        }
        Arc::new(base.rooted_at(cwd))
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

        let request = TurnRequest {
            selection,
            cwd: self.session().cwd.clone(),
            system,
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: prompt }],
            }],
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

        let (message, stop, _usage) = assembler.finish();
        let text: String = message
            .content
            .iter()
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
        match self.permissions().check(&capability) {
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
    pub async fn run_turn(&self, bridge: &dyn PluginBridge, text: String) -> TurnOutcome {
        self.run_turn_with(bridge, text, CancellationToken::new()).await
    }

    /// The same, with a caller-supplied cancellation token.
    ///
    /// The host owns the token so it can interrupt a turn without taking the lock the turn itself
    /// holds — otherwise `<Esc>` would have to wait for the thing it is trying to stop.
    pub async fn run_turn_with(
        &self,
        bridge: &dyn PluginBridge,
        text: String,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        let session = self.sessions().active_id().clone();
        self.run_turn_in(bridge, session, text, cancel).await
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
        text: String,
        cancel: CancellationToken,
    ) -> TurnOutcome {
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
            return end(self, StopReason::Cancelled, Usage::default());
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
        self.with(&session, |s| s.push_user_text(text));

        let selection = self.with(&session, |s| s.selection.clone()).flatten();
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
            let Some((cwd, system, messages)) =
                self.with(&session, |s| (s.cwd.clone(), s.system.clone(), s.messages.clone()))
            else {
                // Closed while its own turn was running. There is nowhere to put the answer, so
                // this is a cancellation rather than an error to report into a transcript that no
                // longer exists.
                stop = StopReason::Cancelled;
                break;
            };
            let request = TurnRequest {
                selection: selection.clone(),
                cwd: cwd.clone(),
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
                        // Handled after the stream closes, so tools run in the order the model
                        // produced them rather than the order their JSON happened to finish.
                        TurnUpdate::ToolReady { .. } | TurnUpdate::ToolMalformed { .. } => {}
                    }
                }
            }
            drop(stream);

            let cancelled = cancel.is_cancelled();
            let calls = assembler.tool_calls();
            let (message, this_stop, usage) = assembler.finish();
            total.merge(&usage);
            self.with(&session, |s| {
                s.add_usage(&usage);
                s.push_assistant(message);
            });

            if cancelled {
                stop = StopReason::Cancelled;
                break;
            }
            stop = this_stop;
            if !matches!(stop, StopReason::ToolUse) || calls.is_empty() {
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
            self.with(&session, |s| s.push_tool_results(results));
            // The gap between rounds: the model is about to be asked again anyway, so anything
            // typed since the last one rides along with the tool results.
            self.take_steering_into(&session, &turn);

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
    fn take_steering_into(&self, session: &SessionId, turn: &TurnId) -> bool {
        let queued = self.take_steering(session);
        if queued.is_empty() {
            return false;
        }
        for text in queued {
            self.with(session, |s| s.push_user_text(text.clone()));
            self.emit(AgentEvent::Steered {
                session: session.clone(),
                turn: turn.clone(),
                text,
            });
        }
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
                        permissions: self.permissions_in(cwd),
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

/// A lock on the store, presented as the active session.
///
/// Exists so that `agent.session().messages` keeps meaning "the conversation on screen" after the
/// store gained the ability to hold several. Without it every call site would have to say
/// `agent.sessions().active()` and the borrow would be the caller's problem.
pub struct ActiveSession<'a>(std::sync::MutexGuard<'a, SessionStore>);

impl std::ops::Deref for ActiveSession<'_> {
    type Target = Session;

    fn deref(&self) -> &Session {
        self.0.active()
    }
}

impl std::ops::DerefMut for ActiveSession<'_> {
    fn deref_mut(&mut self) -> &mut Session {
        self.0.active_mut()
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
        let turn = self.turn.clone();
        let (outcome, _payload) = hooks::run_blocking(
            self.hooks.clone(),
            self.bridge,
            HookName::PermissionPre,
            HookPayload::PermissionPre { turn, capability: capability.clone() },
        )
        .await;
        match outcome {
            HookOutcome::Veto { reason } => neosh_proto::PermissionDecision::Deny { reason },
            // `Continue` from a hook that was asked "may this happen" is a yes. `Modify` is not
            // meaningful for a yes/no question and is read the same way.
            _ => neosh_proto::PermissionDecision::Allow,
        }
    }
}
