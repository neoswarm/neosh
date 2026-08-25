//! An agent driver's permission request, from the driver to the prompt and back.
//!
//! The thing under test is the seam. A driver that runs its own loop has calls neosh's tool
//! registry never sees, so the only thing standing between them and the workspace is the answer to
//! the question the *agent* asks — and until [`DriverAsker`] existed, that answer came from the
//! permission mode alone. `ask`, the default, meant "no", silently, whatever the user would have
//! said if anybody had put the question to them.

use std::sync::{Arc, Mutex};

use neosh_agent::{Agent, DriverAsker, PermissionLayer, PluginBridge, Session};
use neosh_proto::{
    Capability, HookName, HookOutcome, HookPayload, PermissionMode, PermissionOption,
    PermissionOptionKind, PluginEvent, PluginId, ToolResult,
};
use neosh_provider::ProviderRegistry;
use neosh_provider::approval::{PermissionAnswer, PermissionAsker, PermissionRequest};

/// A plugin that answers `permission_pre` however the test says, and records what it was shown.
#[derive(Default)]
struct Prompt {
    answer: Mutex<Option<HookOutcome>>,
    seen: Arc<Mutex<Vec<HookPayload>>>,
}

#[async_trait::async_trait]
impl PluginBridge for Prompt {
    async fn run_tool(&self, _p: &PluginId, _n: &str, _i: serde_json::Value) -> ToolResult {
        ToolResult::ok("")
    }

    async fn call_hook(&self, _p: &PluginId, _h: HookName, pl: HookPayload) -> HookOutcome {
        self.seen.lock().expect("seen lock poisoned").push(pl);
        self.answer.lock().expect("answer lock poisoned").clone().unwrap_or(HookOutcome::Continue)
    }

    fn broadcast(&self, _e: PluginEvent) {}
    fn notify(&self, _p: &PluginId, _e: PluginEvent) {}
}

/// An agent in `mode`, with a `permission_pre` prompt registered unless `listening` is false.
fn agent_with(
    mode: PermissionMode,
    listening: bool,
    answer: Option<HookOutcome>,
) -> (DriverAsker, Arc<Mutex<Vec<HookPayload>>>) {
    let session = Session::new(std::env::temp_dir().join("neosh-driver-approvals"));
    let (agent, _rx) = Agent::new(
        session,
        Arc::new(PermissionLayer::new(std::env::temp_dir().join("neosh-driver-approvals")).with_mode(mode)),
        ProviderRegistry::new(),
    );
    if listening {
        agent.hooks_mut().register(
            HookName::PermissionPre,
            &PluginId::from("approvals"),
            true,
            Some(5_000),
        );
    }
    let prompt = Prompt { answer: Mutex::new(answer), ..Default::default() };
    let seen = prompt.seen.clone();
    (DriverAsker::new(&agent, Arc::new(prompt)), seen)
}

fn request(capability: Capability) -> PermissionRequest {
    PermissionRequest {
        title: "Run cargo test --workspace".into(),
        capability,
        options: vec![
            PermissionOption {
                id: "yes-always".into(),
                label: "Yes, and don't ask again for cargo".into(),
                kind: PermissionOptionKind::AllowAlways,
            },
            PermissionOption {
                id: "yes".into(),
                label: "Yes".into(),
                kind: PermissionOptionKind::AllowOnce,
            },
        ],
        cwd: std::env::temp_dir().join("neosh-driver-approvals"),
        // Nobody in particular. What this carries is checked where it matters — a prompt waiting
        // in a conversation you are not looking at — and none of these tests is about that.
        conversation: None,
    }
}

fn exec() -> Capability {
    Capability::Exec { command: "cargo test --workspace".into() }
}

#[tokio::test]
async fn ask_mode_asks_a_person_instead_of_answering_for_them() {
    // The bug: this returned "no" without anything reaching a prompt, so the default mode made
    // every agent driver useless and looked like the agent had decided to stop.
    let (asker, seen) = agent_with(PermissionMode::Ask, true, None);
    assert_eq!(asker.ask(request(exec())).await, PermissionAnswer::Allow);
    let seen = seen.lock().expect("seen lock poisoned");
    assert_eq!(seen.len(), 1, "the question was actually put to somebody");
}

#[tokio::test]
async fn the_agents_own_words_and_options_reach_the_prompt() {
    let (asker, seen) = agent_with(PermissionMode::Ask, true, None);
    let _ = asker.ask(request(exec())).await;
    let seen = seen.lock().expect("seen lock poisoned");
    let HookPayload::PermissionPre { title, options, capability, .. } = &seen[0] else {
        panic!("wrong payload: {:?}", seen[0]);
    };
    assert_eq!(title.as_deref(), Some("Run cargo test --workspace"));
    assert_eq!(capability, &exec());
    assert_eq!(
        options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(),
        ["Yes, and don't ask again for cargo", "Yes"],
        "verbatim: an option the agent worded is one only the agent can act on"
    );
}

#[tokio::test]
async fn the_option_the_person_picked_is_the_one_that_goes_back() {
    let chosen = HookOutcome::Modify {
        payload: serde_json::to_value(HookPayload::PermissionPre {
            capability: exec(),
            turn: None,
            session: None,
            title: Some("Run cargo test --workspace".into()),
            options: vec![],
            chosen: Some("yes-always".into()),
        })
        .expect("payload serialises"),
    };
    let (asker, _) = agent_with(PermissionMode::Ask, true, Some(chosen));
    assert_eq!(
        asker.ask(request(exec())).await,
        PermissionAnswer::Option("yes-always".into()),
        "not flattened to a bare yes, which would throw the 'and don't ask again' away"
    );
}

#[tokio::test]
async fn a_veto_is_a_refusal() {
    let veto = HookOutcome::Veto { reason: "denied".into() };
    let (asker, _) = agent_with(PermissionMode::Ask, true, Some(veto));
    assert_eq!(asker.ask(request(exec())).await, PermissionAnswer::Deny);
}

#[tokio::test]
async fn policy_answers_first_so_full_access_never_opens_a_prompt() {
    let (asker, seen) = agent_with(PermissionMode::Allow, true, None);
    assert_eq!(asker.ask(request(exec())).await, PermissionAnswer::Allow);
    assert!(seen.lock().expect("seen lock poisoned").is_empty(), "nobody was disturbed");
}

#[tokio::test]
async fn deny_mode_refuses_without_asking_either() {
    let (asker, seen) = agent_with(PermissionMode::Deny, true, None);
    assert_eq!(asker.ask(request(exec())).await, PermissionAnswer::Deny);
    assert!(seen.lock().expect("seen lock poisoned").is_empty());
}

#[tokio::test]
async fn a_workspace_read_is_allowed_without_a_prompt_exactly_as_read_file_is() {
    // Otherwise an agent driver would prompt for something neosh's own `read_file` does silently,
    // which is two behaviours for one question.
    let (asker, seen) = agent_with(PermissionMode::Ask, true, None);
    let cap = Capability::ReadFile { path: "src/main.rs".into() };
    assert_eq!(asker.ask(request(cap)).await, PermissionAnswer::Allow);
    assert!(seen.lock().expect("seen lock poisoned").is_empty());
}

#[tokio::test]
async fn a_path_outside_the_workspace_is_refused_in_every_mode() {
    for mode in [PermissionMode::Ask, PermissionMode::AllowListed, PermissionMode::Allow] {
        let (asker, seen) = agent_with(mode, true, None);
        let cap = Capability::ReadFile { path: "../../etc/passwd".into() };
        assert_eq!(asker.ask(request(cap)).await, PermissionAnswer::Deny, "{mode:?}");
        assert!(seen.lock().expect("seen lock poisoned").is_empty(), "{mode:?} did not even ask");
    }
}

#[tokio::test]
async fn with_nothing_listening_it_fails_closed() {
    // Same direction a blocking hook times out in: a request nobody can hear must not proceed as
    // though somebody said yes.
    let (asker, _) = agent_with(PermissionMode::Ask, false, None);
    assert_eq!(asker.ask(request(exec())).await, PermissionAnswer::Deny);
}
