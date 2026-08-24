//! The host side of the plugin boundary.
//!
//! Implements [`PluginBridge`] over the TypeScript runtime, and — via [`PluginProvider`] — lets a
//! plugin implement a model provider. Nothing here is specific to TypeScript: every method turns a
//! host intention into a [`PluginInbound`] message and waits for a [`PluginOutbound`] answer, which
//! is exactly what a JSON-RPC subprocess or an MCP server would receive.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use neosh_agent::PluginBridge;
use neosh_proto::{
    DriverKind, HookName, HookOutcome, HookPayload, InstanceConfig, PluginEvent, PluginId,
    PluginInbound, PluginRequest, ProviderEvent, RequestId, StreamId, ToolResult, TurnRequest,
};
use neosh_provider::{Provider, ProviderStream};
use neosh_script::{ScriptInbound, ScriptRuntime};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// A plugin tool that never answers must not hang the turn forever.
const TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// A command asked for its answer and never giving one must not hang the caller forever either.
///
/// Longer than a tool's because a command is allowed to open a picker and wait for a person —
/// `cmd.call("session.new")` is a legitimate thing to want the result of — and a person is slower
/// than a model.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Default)]
struct Pending {
    tools: HashMap<RequestId, oneshot::Sender<ToolResult>>,
    hooks: HashMap<RequestId, oneshot::Sender<HookOutcome>>,
    accepts: HashMap<RequestId, oneshot::Sender<()>>,
    /// Callers waiting on what a command returns. See [`ScriptBridge::call_command`].
    commands: HashMap<RequestId, oneshot::Sender<Result<serde_json::Value, String>>>,
    /// Sinks for in-flight plugin-driven provider streams.
    streams: HashMap<StreamId, mpsc::Sender<ProviderEvent>>,
}

pub struct ScriptBridge {
    script: ScriptRuntime,
    pending: Mutex<Pending>,
    loaded: Mutex<Vec<PluginId>>,
}

impl ScriptBridge {
    pub fn new(script: ScriptRuntime) -> Self {
        Self { script, pending: Mutex::new(Pending::default()), loaded: Mutex::new(Vec::new()) }
    }

    pub fn script(&self) -> &ScriptRuntime {
        &self.script
    }

    pub fn register_loaded(&self, plugin: PluginId) {
        let mut l = self.loaded.lock().expect("bridge lock poisoned");
        if !l.contains(&plugin) {
            l.push(plugin);
        }
    }

    pub fn loaded(&self) -> Vec<PluginId> {
        self.loaded.lock().expect("bridge lock poisoned").clone()
    }

    /// Stop broadcasting to a plugin that is going away.
    ///
    /// Without this, a reloaded plugin would be listed twice and receive every event twice.
    pub fn forget(&self, plugin: &PluginId) {
        self.loaded.lock().expect("bridge lock poisoned").retain(|p| p != plugin);
    }

    fn send(&self, plugin: &PluginId, msg: PluginInbound) {
        if let Err(e) = self.script.send(ScriptInbound::Plugin { plugin: plugin.clone(), msg }) {
            tracing::warn!(%plugin, "{e}");
        }
    }

    /// Deliver a plugin's answer to whoever is waiting for it.
    pub fn resolve_tool(&self, id: &RequestId, result: ToolResult) {
        if let Some(tx) = self.pending.lock().expect("bridge lock poisoned").tools.remove(id) {
            let _ = tx.send(result);
        }
    }

    pub fn resolve_hook(&self, id: &RequestId, outcome: HookOutcome) {
        if let Some(tx) = self.pending.lock().expect("bridge lock poisoned").hooks.remove(id) {
            let _ = tx.send(outcome);
        }
    }

    pub fn resolve_accept(&self, id: &RequestId) {
        if let Some(tx) = self.pending.lock().expect("bridge lock poisoned").accepts.remove(id) {
            let _ = tx.send(());
        }
    }

    pub fn resolve_command(&self, id: &RequestId, value: serde_json::Value) {
        if let Some(tx) = self.pending.lock().expect("bridge lock poisoned").commands.remove(id) {
            let _ = tx.send(Ok(value));
        }
    }

    /// Run a command in the plugin that owns it and wait for what the handler returns.
    ///
    /// The question form of [`PluginEvent::CommandInvoked`]. A handler that throws fails the call
    /// with its message; one that never answers fails it after [`COMMAND_TIMEOUT`]; a plugin that
    /// is unloaded mid-call fails it at once, because the channel it would have answered on is
    /// dropped with the waiter.
    pub async fn call_command(
        &self,
        plugin: &PluginId,
        name: &str,
        args: Vec<String>,
    ) -> Result<serde_json::Value, String> {
        let id = RequestId::new();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("bridge lock poisoned").commands.insert(id.clone(), tx);
        self.send(plugin, PluginInbound::Request {
            id: id.clone(),
            request: PluginRequest::Command { name: name.to_string(), args },
        });
        match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(format!("plugin {plugin} did not answer {name}")),
            Err(_) => {
                self.pending.lock().expect("bridge lock poisoned").commands.remove(&id);
                Err(format!(
                    "command {name} from {plugin} did not answer within {}s",
                    COMMAND_TIMEOUT.as_secs()
                ))
            }
        }
    }

    /// A plugin reported an error instead of an answer. Fail the waiter rather than leaving it to
    /// time out, so the reason reaches the user promptly.
    pub fn fail(&self, id: &RequestId, message: String) {
        let mut p = self.pending.lock().expect("bridge lock poisoned");
        if let Some(tx) = p.tools.remove(id) {
            let _ = tx.send(ToolResult::error(message.clone()));
        }
        if let Some(tx) = p.hooks.remove(id) {
            let _ = tx.send(HookOutcome::Veto { reason: message.clone() });
        }
        if let Some(tx) = p.commands.remove(id) {
            let _ = tx.send(Err(message.clone()));
        }
        p.accepts.remove(id);
    }

    /// Forward one event from a plugin-implemented provider.
    pub fn emit_provider(&self, stream: &StreamId, event: ProviderEvent) {
        let sink = {
            let p = self.pending.lock().expect("bridge lock poisoned");
            p.streams.get(stream).cloned()
        };
        let done = matches!(event, ProviderEvent::MessageStop | ProviderEvent::Error { .. });
        if let Some(tx) = sink {
            let _ = tx.try_send(event);
        }
        if done {
            self.pending.lock().expect("bridge lock poisoned").streams.remove(stream);
        }
    }
}

#[async_trait::async_trait]
impl PluginBridge for ScriptBridge {
    async fn run_tool(
        &self,
        plugin: &PluginId,
        name: &str,
        input: serde_json::Value,
    ) -> ToolResult {
        let id = RequestId::new();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("bridge lock poisoned").tools.insert(id.clone(), tx);
        self.send(plugin, PluginInbound::Request {
            id: id.clone(),
            request: PluginRequest::RunTool { name: name.to_string(), input },
        });

        match tokio::time::timeout(TOOL_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => ToolResult::error(format!("plugin {plugin} did not answer")),
            Err(_) => {
                self.pending.lock().expect("bridge lock poisoned").tools.remove(&id);
                ToolResult::error(format!(
                    "tool {name} from {plugin} timed out after {}s",
                    TOOL_TIMEOUT.as_secs()
                ))
            }
        }
    }

    async fn call_hook(
        &self,
        plugin: &PluginId,
        hook: HookName,
        payload: HookPayload,
    ) -> HookOutcome {
        let id = RequestId::new();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("bridge lock poisoned").hooks.insert(id.clone(), tx);
        self.send(plugin, PluginInbound::Request {
            id,
            request: PluginRequest::Hook { hook, payload },
        });
        // The caller imposes the timeout; a dropped channel means the plugin died mid-question,
        // which is exactly the case where continuing would defeat the point of a policy hook.
        rx.await.unwrap_or_else(|_| HookOutcome::Veto {
            reason: format!("{plugin} disappeared while answering {hook:?}"),
        })
    }

    fn broadcast(&self, event: PluginEvent) {
        for p in self.loaded() {
            self.send(&p, PluginInbound::Event { event: event.clone() });
        }
    }

    fn notify(&self, plugin: &PluginId, event: PluginEvent) {
        self.send(plugin, PluginInbound::Event { event });
    }
}

/// A model provider implemented by a plugin.
///
/// A stream cannot be returned across the RPC boundary, so the plugin acknowledges the request and
/// then *pushes* events tagged with a [`StreamId`]. This type turns that push back into a stream,
/// which is why the agent loop cannot tell a plugin provider from a built-in one.
pub struct PluginProvider {
    driver: DriverKind,
    plugin: PluginId,
    bridge: Arc<ScriptBridge>,
    /// Declared at registration. See [`ApiCall::ProviderRegisterDriver`] for why a plugin gets to
    /// say this at all.
    agent_loop: bool,
}

impl PluginProvider {
    pub fn new(
        driver: DriverKind,
        plugin: PluginId,
        bridge: Arc<ScriptBridge>,
        agent_loop: bool,
    ) -> Self {
        Self { driver, plugin, bridge, agent_loop }
    }
}

#[async_trait::async_trait]
impl Provider for PluginProvider {
    fn driver(&self) -> DriverKind {
        self.driver.clone()
    }

    fn delegates_agent_loop(&self) -> bool {
        self.agent_loop
    }

    async fn list_models(
        &self,
        instance: &InstanceConfig,
    ) -> Result<Vec<neosh_proto::ModelInfo>, neosh_provider::ProviderError> {
        Ok(instance.models.clone())
    }

    fn stream(
        &self,
        _instance: &InstanceConfig,
        request: TurnRequest,
        cancel: CancellationToken,
    ) -> ProviderStream {
        let stream_id = StreamId::new();
        let (tx, rx) = mpsc::channel::<ProviderEvent>(256);
        self.bridge
            .pending
            .lock()
            .expect("bridge lock poisoned")
            .streams
            .insert(stream_id.clone(), tx);

        let request_id = RequestId::new();
        self.bridge.send(&self.plugin, PluginInbound::Request {
            id: request_id,
            request: PluginRequest::ProviderStream { stream: stream_id.clone(), request },
        });

        // Cancellation has to reach the plugin, or it keeps generating into a dropped channel.
        let bridge = self.bridge.clone();
        let plugin = self.plugin.clone();
        let sid = stream_id.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            bridge.notify(&plugin, PluginEvent::ProviderCancel { stream: sid.clone() });
            bridge.pending.lock().expect("bridge lock poisoned").streams.remove(&sid);
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}
