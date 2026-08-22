//! The agent domain: sessions, turns, tools, permissions and the loop that ties them together.
//!
//! This crate is deliberately ignorant of *how* plugins are executed. It talks to them through
//! [`PluginBridge`], which the host implements over whichever transport a plugin happens to use —
//! the in-process TypeScript runtime today, JSON-RPC over stdio or MCP later. That is what keeps
//! "a tool implemented by an out-of-process plugin" from being a special case in the agent loop.

pub mod agent;
pub mod hooks;
pub mod permission;
pub mod session;
pub mod store;
pub mod tools;
pub mod turn;

pub use agent::{Agent, AgentEvent, DriverAsker, DriverQuestioner, Fanout, TurnOutcome};
pub use hooks::HookRegistry;
pub use permission::PermissionLayer;
pub use session::{Prompt, Session};
pub use tools::{BuiltinTool, ToolCtx, ToolRegistry};
pub use turn::{TurnAssembler, TurnUpdate};

use neosh_proto::{HookName, HookOutcome, HookPayload, PluginEvent, PluginId, ToolResult};

/// How the agent reaches plugins.
///
/// Implemented by the host. Every method is transport-agnostic on purpose: an implementation that
/// speaks to a subprocess over JSON-RPC satisfies this exactly as well as one that calls into an
/// embedded runtime, which is what makes the plugin boundary real rather than nominal.
#[async_trait::async_trait]
pub trait PluginBridge: Send + Sync {
    /// Run a tool a plugin registered.
    async fn run_tool(
        &self,
        plugin: &PluginId,
        name: &str,
        input: serde_json::Value,
    ) -> ToolResult;

    /// Ask a blocking hook. Callers apply the timeout; this must not impose its own.
    async fn call_hook(
        &self,
        plugin: &PluginId,
        hook: HookName,
        payload: HookPayload,
    ) -> HookOutcome;

    /// Fire-and-forget to every plugin.
    fn broadcast(&self, event: PluginEvent);

    /// Fire-and-forget to one plugin.
    fn notify(&self, plugin: &PluginId, event: PluginEvent);
}
