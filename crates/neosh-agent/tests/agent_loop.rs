//! The agent loop, end to end, against a scripted provider.
//!
//! This is the Phase 0 definition of done at the agent layer: a plugin-registered tool is called by
//! the model and executed over the bridge, and a blocking `tool.pre` hook can stop a call from ever
//! happening. No network, no API key, no timing dependence.

use std::sync::{Arc, Mutex};

use neosh_agent::{Agent, AgentEvent, PermissionLayer, PluginBridge, Prompt, Session};
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
        brand: None,
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

/// A driver that runs its own loop, which is the only kind that reports tool calls as they happen.
///
/// `claude`, `codex`, anything speaking ACP: the calls are made in another process, so the stream
/// is the only place they exist and [`AgentEvent::ToolStarted`] is said for each one as it is
/// announced. Whether the matching [`AgentEvent::ToolFinished`] is always said is what the test
/// below is about, so this has to be a delegating driver — a model driver never populates that
/// bookkeeping at all.
struct Delegating(MockProvider);

#[async_trait::async_trait]
impl neosh_provider::Provider for Delegating {
    fn driver(&self) -> DriverKind {
        self.0.driver()
    }

    fn delegates_agent_loop(&self) -> bool {
        true
    }

    fn stream(
        &self,
        instance: &InstanceConfig,
        request: neosh_proto::TurnRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> neosh_provider::ProviderStream {
        self.0.stream(instance, request, cancel)
    }
}

/// An agent whose driver runs its own loop and replays exactly the given turns.
fn agent_with_delegating_script(
    script: Vec<Vec<neosh_proto::ProviderEvent>>,
) -> (Agent, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
    let mut providers = ProviderRegistry::new();
    providers.register_driver(Arc::new(Delegating(MockProvider::new(script))));
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

    let outcome = agent.run_turn(&bridge, "say hello").await;

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

    agent.run_turn(&bridge, "say hello").await;

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

    agent.run_turn(&bridge, "say hello").await;

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

    agent.run_turn(&TestBridge::default(), "hi").await;

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

    let outcome = agent.run_turn(&bridge, "do something").await;
    // A refusal, not a cancellation. `<Esc>` is what `Cancelled` means, and reporting a policy
    // veto with the same value told everything downstream that the person had interrupted the
    // turn — and dropped the reason the plugin gave for stopping it.
    assert_eq!(
        outcome.stop_reason,
        StopReason::Refusal {
            category: Some("a plugin".into()),
            explanation: Some("rate limited locally".into()),
        },
        "the veto reason travels with the outcome"
    );
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
    let outcome = agent.run_turn_with(&bridge, "hi", token).await;

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

    agent.run_turn(&TestBridge::default(), "go").await;

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
    agent.run_turn(&bridge, "hi").await;

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
    agent.run_turn(&bridge, "one").await;
    agent.run_turn(&bridge, "two").await;

    let usage = agent.session().usage;
    assert_eq!(usage.input_tokens, 200, "two turns, summed");
    assert_eq!(usage.output_tokens, 20);
}

// ---------------------------------------------------------------------------
// A turn belongs to a conversation
// ---------------------------------------------------------------------------

/// One scripted turn that says `text` and stops.
fn text_script(text: &str) -> Vec<neosh_proto::ProviderEvent> {
    MockProvider::text_turn(&[text])
}

#[tokio::test]
async fn a_turn_runs_in_the_conversation_it_names_not_in_whichever_is_on_screen() {
    // The whole point of addressing a turn by id: you can switch away from it, and it keeps
    // writing where it started rather than into whatever you are now reading.
    let (agent, mut rx) = agent_with_script(vec![text_script("answered")]);
    let on_screen = agent.sessions().current_id().clone();

    let mut other = Session::new(std::env::temp_dir());
    other.selection = agent.selection();
    let elsewhere = other.id.clone();
    agent.sessions().insert(other);

    agent
        .run_turn_in(
            &TestBridge::default(),
            elsewhere.clone(),
            "ask the other one".into(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

    let store = agent.sessions();
    assert_eq!(store.current_id(), &on_screen, "running a turn does not move anybody");
    assert!(
        store.get(&on_screen).expect("still there").messages.is_empty(),
        "the conversation on screen was not part of this and must be untouched"
    );
    let ran_in = store.get(&elsewhere).expect("still there");
    assert_eq!(ran_in.messages.len(), 2, "the question and the answer landed where they belong");
    drop(store);

    assert!(
        drain(&mut rx).iter().all(|e| e.session() == &elsewhere),
        "every event says which conversation it came from, and it is not the one on screen"
    );
}

#[tokio::test]
async fn a_call_the_driver_never_answered_is_closed_out_rather_than_left_running() {
    // A card with no result does not sit still — it spins, and goes on counting, for as long as
    // the conversation is open. And it does it again after a restart: the call is in the messages
    // with no result beside it, and a result-less call is what a rebuild draws as still running.
    // So a turn that walked away from a call has to say so before it ends.
    //
    // A stream that announces two calls, answers the first, and closes.
    let script = vec![vec![
        neosh_proto::ProviderEvent::MessageStart {
            model: Some("mock".into()),
            usage: neosh_proto::Usage::default(),
        },
        neosh_proto::ProviderEvent::BlockStart {
            index: 0,
            block: neosh_proto::BlockStartKind::ToolUse {
                id: neosh_proto::ToolCallId("answered".into()),
                name: "Bash".into(),
            },
        },
        neosh_proto::ProviderEvent::ToolInputDelta { index: 0, partial_json: "{}".into() },
        neosh_proto::ProviderEvent::BlockStop { index: 0 },
        neosh_proto::ProviderEvent::ToolResult {
            id: neosh_proto::ToolCallId("answered".into()),
            content: "ok".into(),
            is_error: false,
        },
        neosh_proto::ProviderEvent::BlockStart {
            index: 1,
            block: neosh_proto::BlockStartKind::ToolUse {
                id: neosh_proto::ToolCallId("abandoned".into()),
                name: "Bash".into(),
            },
        },
        neosh_proto::ProviderEvent::ToolInputDelta { index: 1, partial_json: "{}".into() },
        neosh_proto::ProviderEvent::BlockStop { index: 1 },
        neosh_proto::ProviderEvent::MessageStop,
    ]];
    let (agent, mut rx) = agent_with_delegating_script(script);
    let here = agent.sessions().current_id().clone();
    let bridge = TestBridge::default();
    agent.run_turn(&bridge, "go on then").await;

    let events = drain(&mut rx);
    let started: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolStarted { call, .. } => Some(call.id.0.clone()),
            _ => None,
        })
        .collect();
    let finished: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolFinished { call, result, .. } => {
                Some((call.id.0.clone(), result.is_error))
            }
            _ => None,
        })
        .collect();
    assert_eq!(started, vec!["answered".to_string(), "abandoned".to_string()]);
    assert_eq!(
        finished,
        vec![("answered".to_string(), false), ("abandoned".to_string(), true)],
        "every call that was announced came back, and the one nobody answered says it failed"
    );

    // And the conversation agrees, or switching away and back would draw the spinner again from
    // messages that never got the result the event carried.
    let store = agent.sessions();
    let s = store.get(&here).expect("the conversation is still there");
    let answered = s.messages.iter().flat_map(|m| &m.content).any(|b| {
        matches!(b, neosh_proto::ContentBlock::ToolResult { tool_use_id, is_error, .. }
            if tool_use_id.0 == "abandoned" && *is_error)
    });
    assert!(answered, "the unanswered call has a result in the transcript too");
}

#[tokio::test]
async fn steering_is_taken_by_the_conversation_it_was_typed_in() {
    // A single queue would hand what you typed here to whichever turn reached a gap first.
    let (agent, _rx) = agent_with_script(vec![]);
    let here = agent.sessions().current_id().clone();
    let there = {
        let s = Session::new(std::env::temp_dir());
        let id = s.id.clone();
        agent.sessions().insert(s);
        id
    };

    agent.steer(&here, "wait, not that file".into());
    agent.steer(&there, "and rename it too".into());

    assert_eq!(agent.steering_count(&here), 1);
    assert_eq!(agent.steering_count(&there), 1);
    assert_eq!(agent.take_steering(&here), vec![Prompt::text("wait, not that file")]);
    assert_eq!(
        agent.steering_queue(&there),
        vec![Prompt::text("and rename it too")],
        "taking one conversation's queue must not empty another's"
    );
}

// ---- several views of one workspace ---------------------------------------
//
// The workspace is a process and the terminal is a viewer of it (ADR 0036), so there can be more
// than one viewer. Each needs the *whole* stream: a view that missed one `ToolFinished` is a view
// with a card that never stops spinning, and nothing later puts it right.

/// Two views of one workspace see exactly the same turn.
#[tokio::test]
async fn every_view_sees_the_whole_turn() {
    let (agent, mut first) = agent_with_tool_turn();
    let mut second = agent.subscribe();
    assert_eq!(agent.views(), 2);

    agent.run_turn(&TestBridge::default(), "say hello").await;

    let a = drain(&mut first);
    let b = drain(&mut second);
    assert!(!a.is_empty(), "a turn produced no events at all");
    assert_eq!(a, b, "two views of one workspace disagreed about what happened");
}

/// A view that has gone away does not take the others with it, and does not accumulate.
#[tokio::test]
async fn a_view_that_closed_does_not_take_the_others_with_it() {
    let (agent, first) = agent_with_tool_turn();
    let mut second = agent.subscribe();
    assert_eq!(agent.views(), 2);

    // The terminal was closed. The workspace, and the turn, carry on.
    drop(first);

    agent.run_turn(&TestBridge::default(), "say hello").await;

    assert!(!drain(&mut second).is_empty(), "the surviving view saw nothing");
    assert_eq!(agent.views(), 1, "a closed view is pruned by the send that found it closed");
}

/// Every terminal being closed is an ordinary state, not a shutdown.
#[tokio::test]
async fn a_turn_runs_with_nobody_watching() {
    let (agent, rx) = agent_with_tool_turn();
    drop(rx);

    let outcome = agent.run_turn(&TestBridge::default(), "say hello").await;

    assert_eq!(agent.views(), 0);
    assert_eq!(outcome.stop_reason, StopReason::EndTurn, "the turn ran to the end anyway");
}

/// A view opened while the workspace is busy gets what happens next, not a replay.
///
/// The state it is missing arrives as `Editor::republish`, which is a different question from this
/// one: a second copy of the deltas that built a transcript would draw it twice.
#[tokio::test]
async fn a_view_that_arrives_late_gets_what_happens_next() {
    let (agent, mut first) = agent_with_script(vec![
        MockProvider::text_turn(&["before"]),
        MockProvider::text_turn(&["after"]),
    ]);
    let bridge = TestBridge::default();

    agent.run_turn(&bridge, "one").await;
    let early = drain(&mut first);
    assert!(!early.is_empty());

    let mut late = agent.subscribe();
    agent.run_turn(&bridge, "two").await;

    let seen = drain(&mut late);
    assert_eq!(seen, drain(&mut first), "from the moment it attached, both views agree");
    assert!(
        seen.iter().all(|e| !matches!(e, AgentEvent::Token { text, .. } if text == "before")),
        "a late view was replayed a turn that had already finished"
    );
}
