//! One namespace for every tool, whatever implements it.
//!
//! Built-in, plugin and (later) MCP tools land in the same registry with the same [`ToolDef`]
//! shape. That is what lets a policy hook be written once: it intercepts `read_file` without
//! caring that it is Rust, or `deploy` without caring that it is TypeScript in another process.

use std::collections::BTreeMap;
use std::sync::Arc;

use neosh_proto::{
    ApiError, Capability, PermissionDecision, PluginId, ToolDef, ToolResult, ToolSource,
};

use crate::permission::PermissionLayer;

/// What a built-in tool gets to work with.
pub struct ToolCtx<'a> {
    pub permissions: Arc<PermissionLayer>,
    /// Asks the user, when the policy says to ask.
    ///
    /// A tool cannot do this itself: it holds no bridge, and the *decision* is policy while the
    /// *asking* is interface. `None` means nobody is listening — in tests, or in a session with no
    /// approval plugin — and a request nobody can answer is refused rather than allowed.
    ///
    /// Borrowed rather than shared: it lives exactly as long as the call it is gating, and an
    /// approver that outlived its turn could answer for a conversation that has moved on.
    pub approve: Option<&'a dyn Approver>,
}

/// Turns "the policy wants the user asked" into an answer.
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    async fn ask(&self, capability: Capability) -> PermissionDecision;
}

impl ToolCtx<'_> {
    /// Check a capability, asking the user if the policy says to.
    ///
    /// Every tool goes through this rather than calling `permissions.check` directly, so that
    /// "needs approval" is one behaviour in one place instead of each tool inventing its own.
    pub async fn permit(&self, capability: Capability) -> PermissionDecision {
        match self.permissions.check(&capability) {
            PermissionDecision::Prompt => match self.approve {
                Some(a) => a.ask(capability).await,
                // Fail closed. An unanswerable request that ran anyway would make `ask` mode a lie.
                None => PermissionDecision::Deny {
                    reason: "needs approval, and nothing is able to ask".into(),
                },
            },
            other => other,
        }
    }
}

#[async_trait::async_trait]
pub trait BuiltinTool: Send + Sync {
    fn def(&self) -> ToolDef;
    async fn run(&self, input: serde_json::Value, ctx: &ToolCtx<'_>) -> ToolResult;
}

#[derive(Clone)]
pub enum ToolHandler {
    Builtin(Arc<dyn BuiltinTool>),
    /// Executed by a plugin over the RPC boundary.
    Plugin(PluginId),
}

impl std::fmt::Debug for ToolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Builtin(_) => f.write_str("Builtin"),
            Self::Plugin(p) => write!(f, "Plugin({p})"),
        }
    }
}

#[derive(Clone)]
pub struct ToolEntry {
    pub def: ToolDef,
    pub handler: ToolHandler,
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolEntry>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_builtin(&mut self, tool: Arc<dyn BuiltinTool>) {
        let def = tool.def();
        self.tools
            .insert(def.name.clone(), ToolEntry { def, handler: ToolHandler::Builtin(tool) });
    }

    /// Register a plugin tool.
    ///
    /// Name collisions are refused rather than silently resolved, because "whichever plugin loaded
    /// last wins" is impossible to debug from the outside.
    pub fn register_plugin(
        &mut self,
        plugin: &PluginId,
        mut def: ToolDef,
    ) -> Result<(), ApiError> {
        if let Some(existing) = self.tools.get(&def.name)
            && existing.handler_owner().as_ref() != Some(plugin)
        {
            return Err(ApiError::InvalidArgument {
                message: format!(
                    "tool {:?} is already registered by {}",
                    def.name,
                    match &existing.def.source {
                        ToolSource::Builtin => "the host".to_string(),
                        ToolSource::Plugin { plugin } => plugin.clone(),
                        ToolSource::Mcp { server } => format!("MCP server {server}"),
                    }
                ),
            });
        }
        def.source = ToolSource::Plugin { plugin: plugin.0.clone() };
        self.tools.insert(def.name.clone(), ToolEntry {
            def,
            handler: ToolHandler::Plugin(plugin.clone()),
        });
        Ok(())
    }

    pub fn unregister(&mut self, plugin: &PluginId, name: &str) {
        if self.tools.get(name).and_then(ToolEntry::handler_owner).as_ref() == Some(plugin) {
            self.tools.remove(name);
        }
    }

    pub fn remove_plugin(&mut self, plugin: &PluginId) {
        self.tools.retain(|_, e| e.handler_owner().as_ref() != Some(plugin));
    }

    pub fn get(&self, name: &str) -> Option<&ToolEntry> {
        self.tools.get(name)
    }

    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools.values().map(|e| e.def.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl ToolEntry {
    fn handler_owner(&self) -> Option<PluginId> {
        match &self.handler {
            ToolHandler::Plugin(p) => Some(p.clone()),
            ToolHandler::Builtin(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Built-ins
// ---------------------------------------------------------------------------

/// Read a file from the workspace.
///
/// The Phase 0 built-in. Every effect it has goes through [`PermissionLayer`] first, so it is also
/// the worked example of how a tool is supposed to behave.
pub struct ReadFile;

#[async_trait::async_trait]
impl BuiltinTool for ReadFile {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "read_file".into(),
            description: "Read a UTF-8 text file from the workspace. Paths are relative to the \
                           workspace root."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative path to the file.",
                    },
                    "max_bytes": {
                        "type": "integer",
                        "description": "Truncate after this many bytes. Defaults to 256000.",
                    },
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
            source: ToolSource::Builtin,
        }
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolCtx<'_>) -> ToolResult {
        let Some(path) = input.get("path").and_then(|p| p.as_str()) else {
            return ToolResult::error("read_file requires a string `path`");
        };
        let max = input.get("max_bytes").and_then(|m| m.as_u64()).unwrap_or(256_000) as usize;

        let cap = Capability::ReadFile { path: path.to_string() };
        match ctx.permit(cap).await {
            PermissionDecision::Deny { reason } => {
                return ToolResult::error(format!("permission denied: {reason}"));
            }
            // `permit` has already asked whoever can be asked, so reaching here means nobody
            // could. Refusing is the only safe reading.
            PermissionDecision::Prompt => {
                return ToolResult::error(format!("reading {path} needs approval"));
            }
            PermissionDecision::Allow => {}
        }

        let Some(resolved) = ctx.permissions.resolve_path(path) else {
            return ToolResult::error(format!("{path} is outside the workspace"));
        };

        match tokio::fs::read(&resolved).await {
            Ok(bytes) => {
                let truncated = bytes.len() > max;
                let slice = &bytes[..bytes.len().min(max)];
                let mut text = String::from_utf8_lossy(slice).into_owned();
                if truncated {
                    text.push_str(&format!(
                        "\n\n[truncated: {} of {} bytes shown]",
                        slice.len(),
                        bytes.len()
                    ));
                }
                ToolResult::ok(text)
            }
            Err(e) => ToolResult::error(format!("could not read {path}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::PermissionMode;
    use serde_json::json;

    /// No approver: these tests are about the policy, and "nobody can be asked" is the state a
    /// session without an approval plugin is in.
    fn ctx(root: &std::path::Path) -> ToolCtx<'static> {
        ToolCtx { permissions: Arc::new(PermissionLayer::new(root)), approve: None }
    }

    fn plugin_def(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
            source: ToolSource::Builtin,
        }
    }

    #[test]
    fn builtin_and_plugin_tools_share_one_namespace() {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(ReadFile));
        r.register_plugin(&PluginId::from("hello"), plugin_def("greet")).unwrap();
        let names: Vec<_> = r.defs().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["greet", "read_file"]);
    }

    #[test]
    fn registering_a_plugin_tool_stamps_its_source() {
        let mut r = ToolRegistry::new();
        r.register_plugin(&PluginId::from("hello"), plugin_def("greet")).unwrap();
        assert_eq!(r.get("greet").unwrap().def.source, ToolSource::Plugin {
            plugin: "hello".into()
        });
    }

    #[test]
    fn a_name_collision_is_refused_rather_than_silently_overriding() {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(ReadFile));
        let err = r.register_plugin(&PluginId::from("evil"), plugin_def("read_file"));
        assert!(err.is_err(), "a plugin must not be able to shadow a built-in");
        // Re-registering your own tool is fine, so a plugin can hot-reload.
        r.register_plugin(&PluginId::from("hello"), plugin_def("greet")).unwrap();
        assert!(r.register_plugin(&PluginId::from("hello"), plugin_def("greet")).is_ok());
    }

    #[test]
    fn unloading_a_plugin_removes_only_its_tools() {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(ReadFile));
        r.register_plugin(&PluginId::from("a"), plugin_def("ta")).unwrap();
        r.register_plugin(&PluginId::from("b"), plugin_def("tb")).unwrap();
        r.remove_plugin(&PluginId::from("a"));
        let names: Vec<_> = r.defs().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["read_file", "tb"]);
    }

    #[tokio::test]
    async fn read_file_returns_contents_inside_the_workspace() {
        let dir = std::env::temp_dir().join(format!("neosh-tool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hello.txt"), "contents").unwrap();

        let r = ReadFile.run(json!({"path": "hello.txt"}), &ctx(&dir)).await;
        assert!(!r.is_error);
        assert_eq!(r.content, "contents");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_file_refuses_to_escape_the_workspace() {
        let dir = std::env::temp_dir().join(format!("neosh-tool-esc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = ReadFile.run(json!({"path": "../../etc/passwd"}), &ctx(&dir)).await;
        assert!(r.is_error);
        assert!(r.content.contains("permission denied"), "got {:?}", r.content);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_file_truncates_rather_than_flooding_the_context() {
        let dir = std::env::temp_dir().join(format!("neosh-tool-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("big.txt"), "x".repeat(1000)).unwrap();
        let r = ReadFile.run(json!({"path": "big.txt", "max_bytes": 100}), &ctx(&dir)).await;
        assert!(!r.is_error);
        assert!(r.content.contains("truncated"));
        assert!(r.content.len() < 300);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_file_reports_a_missing_argument_instead_of_panicking() {
        let r = ReadFile.run(json!({}), &ctx(std::path::Path::new("/tmp"))).await;
        assert!(r.is_error);
    }

    /// An approver that always says yes, for the tests that need one.
    struct Yes;

    #[async_trait::async_trait]
    impl Approver for Yes {
        async fn ask(&self, _c: Capability) -> PermissionDecision {
            PermissionDecision::Allow
        }
    }

    #[tokio::test]
    async fn a_capability_the_policy_wants_asked_about_is_refused_when_nobody_can_ask() {
        // Fail closed. `ask` mode with nothing listening must not become `allow` mode.
        let dir = std::env::temp_dir();
        let c = ToolCtx {
            permissions: Arc::new(PermissionLayer::new(&dir).with_mode(PermissionMode::Ask)),
            approve: None,
        };
        let d = c.permit(Capability::Exec { command: "rm -rf /".into() }).await;
        assert!(matches!(d, PermissionDecision::Deny { .. }), "got {d:?}");
    }

    #[tokio::test]
    async fn an_approver_can_turn_a_prompt_into_an_allow() {
        let dir = std::env::temp_dir();
        let yes = Yes;
        let c = ToolCtx {
            permissions: Arc::new(PermissionLayer::new(&dir).with_mode(PermissionMode::Ask)),
            approve: Some(&yes),
        };
        let d = c.permit(Capability::Exec { command: "ls".into() }).await;
        assert_eq!(d, PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn a_decision_the_policy_already_made_never_reaches_the_approver() {
        // Asking about something the config already allowed is a prompt the user cannot act on
        // meaningfully, and asking about something it denied is worse.
        let dir = std::env::temp_dir();
        let yes = Yes;
        let c = ToolCtx {
            permissions: Arc::new(PermissionLayer::new(&dir).with_mode(PermissionMode::Deny)),
            approve: Some(&yes),
        };
        let d = c.permit(Capability::Exec { command: "ls".into() }).await;
        assert!(matches!(d, PermissionDecision::Deny { .. }), "the approver cannot override policy");
    }

    #[tokio::test]
    async fn deny_mode_blocks_the_builtin() {
        let dir = std::env::temp_dir();
        let c = ToolCtx {
            permissions: Arc::new(PermissionLayer::new(&dir).with_mode(PermissionMode::Deny)),
            approve: None,
        };
        let r = ReadFile.run(json!({"path": "any.txt"}), &c).await;
        assert!(r.is_error);
    }
}
