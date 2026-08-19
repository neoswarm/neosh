//! The agent loop, end to end, against a scripted provider.
//!
//! This is the Phase 0 definition of done at the agent layer: a plugin-registered tool is called by
//! the model and executed over the bridge, and a blocking `tool.pre` hook can stop a call from ever
//! happening. No network, no API key, no timing dependence.

use std::sync::{Arc, Mutex};

use neosh_agent::{Agent, AgentEvent, PermissionLayer, PluginBridge, Session};
use neosh_proto::{
    AuthRef, DriverKind, HookName, HookOutcome, HookPayload, InstanceConfig, InstanceId, ModelId,
    ModelSelection, PluginEvent, PluginId, StopReason, ToolDef, ToolResult, ToolSource,
};
use neosh_provider::{ProviderRegistry, drivers::MockProvider};

/// Records what the host asked plugins to do, and answers with whatever the test configured.
#[derive(Default)]
struct TestBridge {
    tool_calls: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    hook_answer: Mutex<Option<HookOutcome>>,
    observed: Arc<Mutex<Vec<HookName>>>,
}

#[async_trait::async_trait]
impl PluginBridge for TestBridge {
    async fn run_tool(
        &self,
        _p: &PluginId,
        name: &str,
        input: serde_json::Value,
    ) -> ToolResult {
        self.tool_calls.lock().unwrap().push((name.to_string(), input.clone()));
        ToolResult::ok(format!("ran {name} with {input}"))
    }

    async fn call_hook(&self, _p: &PluginId, _h: HookName, _pl: HookPayload) -> HookOutcome {
        self.hook_answer.lock().unwrap().clone().unwrap_or(HookOutcome::Continue)
    }

    fn broadcast(&self, _e: PluginEvent) {}

    fn notify(&self, _p: &PluginId, e: PluginEvent) {
        if let PluginEvent::HookObserved { hook, .. } = e {
            self.observed.lock().unwrap().push(hook);
        }
    }
}

fn mock_instance() -> InstanceConfig {
    InstanceConfig {
        id: InstanceId::from("mock"),
        driver: DriverKind::from("mock"),
        display_name: "Mock".into(),
        base_url: None,
        auth: AuthRef::None,
        models: vec![],
        extra_headers: vec![],
    }
}

fn plugin_tool() -> ToolDef {
    ToolDef {
        name: "greet".into(),
        description: "Greet someone".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"who": {"type": "string"}},
            "required": ["who"],
        }),
        source: ToolSource::Builtin,
    }
}

/// Build an agent whose provider will make one tool call and then answer.
fn agent_with_tool_turn() -> (Agent, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
    let mut providers = ProviderRegistry::new();
    providers.register_driver(Arc::new(MockProvider::new(vec![
        MockProvider::tool_turn("call_1", "greet", r#"{"who":"world"}"#),
        MockProvider::text_turn(&["Done", "!"]),
    ])));
    providers.add_instance(mock_instance());

    let mut session = Session::new(std::env::temp_dir());
    session.selection = Some(ModelSelection {
        instance: InstanceId::from("mock"),
        model: ModelId::from("mock"),
        options: vec![],
    });

    let perms = Arc::new(PermissionLayer::new(std::env::temp_dir()));
    let (agent, rx) = Agent::new(session, perms, providers);
    agent.tools_mut().register_plugin(&PluginId::from("hello"), plugin_tool()).unwrap();
    (agent, rx)
}

/// An agent whose provider replays exactly the given turns.
fn agent_with_script(
    script: Vec<Vec<neosh_proto::ProviderEvent>>,
) -> (Agent, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
    let mut providers = ProviderRegistry::new();
    providers.register_driver(Arc::new(MockProvider::new(script)));
    providers.add_instance(mock_instance());

    let mut session = Session::new(std::env::temp_dir());
    session.selection = Some(ModelSelection {
        instance: InstanceId::from("mock"),
        model: ModelId::from("mock"),
        options: vec![],
    });
    let perms = Arc::new(PermissionLayer::new(std::env::temp_dir()));
    Agent::new(session, perms, providers)
}

fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
}

/// DoD 4: a tool registered by a plugin is callable by the agent.
#[tokio::test]
async fn a_plugin_registered_tool_is_called_over_the_bridge() {
    let (agent, mut rx) = agent_with_tool_turn();
    let bridge = TestBridge::default();

    let outcome = agent.run_turn(&bridge, "say hello".into()).await;

    let calls = bridge.tool_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "the plugin tool should have run exactly once");
    assert_eq!(calls[0].0, "greet");
    assert_eq!(calls[0].1, serde_json::json!({"who": "world"}));
    assert_eq!(outcome.stop_reason, StopReason::EndTurn);

    // The tool result was fed back and the model produced a final answer.
    assert_eq!(neosh_agent::agent::assistant_text(&agent.session()), "Done!");

    let events = drain(&mut rx);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolStarted { .. })));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolFinished { .. })));
}

/// DoD 5: a blocking `tool.pre` hook can veto a tool call.
#[tokio::test]
async fn a_blocking_pre_hook_vetoes_the_call_and_the_tool_never_runs() {
    let (agent, _rx) = agent_with_tool_turn();
    agent.hooks_mut().register(HookName::ToolPre, &PluginId::from("policy"), true, None);

    let bridge = TestBridge::default();
    *bridge.hook_answer.lock().unwrap() =
        Some(HookOutcome::Veto { reason: "greeting is not allowed".into() });

    agent.run_turn(&bridge, "say hello".into()).await;

    assert!(
        bridge.tool_calls.lock().unwrap().is_empty(),
        "a vetoed tool must never execute"
    );

    // The model is told it was blocked, rather than the call vanishing.
    let blocked = agent.session().messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, neosh_proto::ContentBlock::ToolResult { content, is_error: true, .. }
                if content.contains("greeting is not allowed"))
        })
    });
    assert!(blocked, "the veto reason must reach the model as an error tool_result");
}

/// A non-blocking hook observes the same call but cannot stop it.
#[tokio::test]
async fn an_observing_hook_sees_the_call_but_cannot_stop_it() {
    let (agent, _rx) = agent_with_tool_turn();
    // An audit plugin registers on both ends of the call.
    agent.hooks_mut().register(HookName::ToolPre, &PluginId::from("audit"), false, None);
    agent.hooks_mut().register(HookName::ToolPost, &PluginId::from("audit"), false, None);

    let bridge = TestBridge::default();
    // Even though it would veto if asked, it is never asked.
    *bridge.hook_answer.lock().unwrap() =
        Some(HookOutcome::Veto { reason: "should be ignored".into() });

    agent.run_turn(&bridge, "say hello".into()).await;

    assert_eq!(bridge.tool_calls.lock().unwrap().len(), 1, "observers cannot veto");
    let observed = bridge.observed.lock().unwrap().clone();
    assert!(
        observed.contains(&HookName::ToolPre),
        "an observer must hear about the attempt, not only the result"
    );
    assert!(observed.contains(&HookName::ToolPost));
}

/// DoD 6, at the agent layer: text arrives incrementally, not as one blob at the end.
#[tokio::test]
async fn assistant_text_streams_as_separate_token_events() {
    let mut providers = ProviderRegistry::new();
    providers.register_driver(Arc::new(MockProvider::new(vec![MockProvider::text_turn(&[
        "The ", "quick ", "brown ", "fox",
    ])])));
    providers.add_instance(mock_instance());

    let mut session = Session::new(std::env::temp_dir());
    session.selection = Some(ModelSelection {
        instance: InstanceId::from("mock"),
        model: ModelId::from("mock"),
        options: vec![],
    });
    let (agent, mut rx) =
        Agent::new(session, Arc::new(PermissionLayer::new(std::env::temp_dir())), providers);

    agent.run_turn(&TestBridge::default(), "hi".into()).await;

    let tokens: Vec<String> = drain(&mut rx)
        .into_iter()
        .filter_map(|e| match e {
            AgentEvent::Token { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(tokens, vec!["The ", "quick ", "brown ", "fox"]);
    assert_eq!(neosh_agent::agent::assistant_text(&agent.session()), "The quick brown fox");
}

/// A `turn.pre` veto stops the turn before any provider request is made.
#[tokio::test]
async fn a_turn_can_be_vetoed_before_it_reaches_the_model() {
    let (agent, mut rx) = agent_with_tool_turn();
    agent.hooks_mut().register(HookName::TurnPre, &PluginId::from("policy"), true, None);

    let bridge = TestBridge::default();
    *bridge.hook_answer.lock().unwrap() =
        Some(HookOutcome::Veto { reason: "rate limited locally".into() });

    let outcome = agent.run_turn(&bridge, "do something".into()).await;
    assert_eq!(outcome.stop_reason, StopReason::Cancelled);
    assert!(agent.session().messages.is_empty(), "nothing should be recorded");
    assert!(drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::Notice { .. })));
}

/// Cancellation is owned by the caller, so `<Esc>` can interrupt without waiting on the turn.
#[tokio::test]
async fn a_cancelled_token_ends_the_turn_as_cancelled_and_runs_no_tools() {
    let (agent, _rx) = agent_with_tool_turn();
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();

    let bridge = TestBridge::default();
    let outcome = agent.run_turn_with(&bridge, "hi".into(), token).await;

    assert_eq!(outcome.stop_reason, StopReason::Cancelled);
    assert!(bridge.tool_calls.lock().unwrap().is_empty(), "no work should have started");
}

/// A model asking for a tool that does not exist gets a usable error, not a crash.
#[tokio::test]
async fn an_unknown_tool_returns_an_error_result_to_the_model() {
    let mut providers = ProviderRegistry::new();
    providers.register_driver(Arc::new(MockProvider::new(vec![
        MockProvider::tool_turn("c1", "does_not_exist", "{}"),
        MockProvider::text_turn(&["sorry"]),
    ])));
    providers.add_instance(mock_instance());

    let mut session = Session::new(std::env::temp_dir());
    session.selection = Some(ModelSelection {
        instance: InstanceId::from("mock"),
        model: ModelId::from("mock"),
        options: vec![],
    });
    let (agent, _rx) =
        Agent::new(session, Arc::new(PermissionLayer::new(std::env::temp_dir())), providers);

    agent.run_turn(&TestBridge::default(), "go".into()).await;

    let told = agent.session().messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, neosh_proto::ContentBlock::ToolResult { content, is_error: true, .. }
                if content.contains("no such tool"))
        })
    });
    assert!(told);
}

#[tokio::test]
async fn usage_is_counted_once_per_turn_not_once_per_event() {
    // Providers report usage *cumulatively* within a turn — `message_start` carries the input count
    // and `message_delta` repeats it with the output count filled in. Summing those would report
    // double, and a cost display that is wrong by an integer factor is worse than none.
    use neosh_proto::{BlockStartKind, ProviderEvent, StopReason, Usage};

    let script = vec![vec![
        ProviderEvent::MessageStart {
            model: Some("mock".into()),
            usage: Usage { input_tokens: 1200, ..Default::default() },
        },
        ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text },
        ProviderEvent::TextDelta { index: 0, text: "hello".into() },
        ProviderEvent::BlockStop { index: 0 },
        ProviderEvent::MessageDelta {
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage { input_tokens: 1200, output_tokens: 340, ..Default::default() },
        },
        ProviderEvent::MessageStop,
    ]];

    let (agent, _rx) = agent_with_script(script);
    let bridge = TestBridge::default();
    agent.run_turn(&bridge, "hi".into()).await;

    let usage = agent.session().usage;
    assert_eq!(usage.input_tokens, 1200, "counted once, not once per event");
    assert_eq!(usage.output_tokens, 340);
}

#[tokio::test]
async fn usage_accumulates_across_turns() {
    use neosh_proto::{BlockStartKind, ProviderEvent, StopReason, Usage};

    let turn = || {
        vec![
            ProviderEvent::MessageStart {
                model: Some("mock".into()),
                usage: Usage { input_tokens: 100, ..Default::default() },
            },
            ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text },
            ProviderEvent::TextDelta { index: 0, text: "ok".into() },
            ProviderEvent::BlockStop { index: 0 },
            ProviderEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage { input_tokens: 100, output_tokens: 10, ..Default::default() },
            },
            ProviderEvent::MessageStop,
        ]
    };

    let (agent, _rx) = agent_with_script(vec![turn(), turn()]);
    let bridge = TestBridge::default();
    agent.run_turn(&bridge, "one".into()).await;
    agent.run_turn(&bridge, "two".into()).await;

    let usage = agent.session().usage;
    assert_eq!(usage.input_tokens, 200, "two turns, summed");
    assert_eq!(usage.output_tokens, 20);
}
