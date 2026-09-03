//! The Antigravity driver against a real `agy`.
//!
//! Skipped unless `agy` is on `PATH` *and* signed in, because neither is something a checkout can
//! provide: the CLI is a 180 MB vendor download and the login is a person's Google account. What
//! it buys when it does run is the only thing the unit tests cannot — that the envelope, the
//! flags and the event names in this driver are the ones the CLI actually uses, rather than the
//! ones it used when the driver was written.
//!
//! Run it with `agy` on `PATH`:
//!
//! ```sh
//! cargo test -p neosh-provider --test antigravity_stream_json -- --ignored --nocapture
//! ```

use futures::StreamExt;
use neosh_proto::{
    ContentBlock, InstanceConfig, InstanceId, Message, ModelId, ModelSelection, ProviderEvent, Role,
    SessionId, TurnRequest,
};
use neosh_provider::Provider;
use neosh_provider::drivers::{AgentDriver, AntigravityProvider};

fn instance() -> InstanceConfig {
    InstanceConfig {
        id: InstanceId::from("antigravity"),
        driver: neosh_proto::DriverKind::from("antigravity"),
        display_name: "Antigravity".into(),
        base_url: None,
        brand: None,
        auth: neosh_proto::AuthRef::Cli {
            program: "agy".into(),
            login: Some("agy".into()),
            retired: None,
        },
        models: Vec::new(),
        extra_headers: Vec::new(),
    }
}

fn request(text: &str, dir: &std::path::Path) -> TurnRequest {
    TurnRequest {
        selection: ModelSelection {
            instance: InstanceId::from("antigravity"),
            model: ModelId::from("gemini-3.8-flash-low"),
            options: Vec::new(),
        },
        conversation: SessionId("antigravity-e2e".into()),
        cwd: dir.to_path_buf(),
        resume: None,
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }],
        tools: Vec::new(),
        max_output_tokens: None,
    }
}

#[tokio::test]
#[ignore = "needs the `agy` CLI on PATH and a Google account signed into it"]
async fn a_real_turn_streams_text_and_ends_with_usage() {
    let driver = AntigravityProvider::default();
    assert!(driver.available(), "`agy` is not on PATH");

    let dir = std::env::temp_dir().join("neosh-antigravity-e2e");
    std::fs::create_dir_all(&dir).expect("scratch directory");

    let mut stream = driver.stream(
        &instance(),
        request("Reply with exactly: PONG", &dir),
        tokio_util::sync::CancellationToken::new(),
    );

    let mut text = String::new();
    let mut ended = false;
    let mut usage = neosh_proto::Usage::default();
    let mut resumed: Option<String> = None;
    while let Some(ev) = stream.next().await {
        match ev {
            ProviderEvent::TextDelta { text: t, .. } => text.push_str(&t),
            ProviderEvent::Activity { activity: neosh_proto::Activity::Resume { token } } => {
                resumed = Some(token);
            }
            ProviderEvent::MessageDelta { usage: u, .. } => usage = u,
            ProviderEvent::MessageStop => ended = true,
            ProviderEvent::Error { message, .. } => panic!("the turn failed: {message}"),
            _ => {}
        }
    }

    assert!(ended, "the stream never reported the turn ending");
    assert!(text.contains("PONG"), "no answer came back, got {text:?}");
    // If this is zero the `usage` mapping has drifted, which is invisible in the transcript and
    // shows up as a context meter that never moves.
    assert!(usage.input_tokens > 0, "no token accounting arrived: {usage:?}");
    // Without this a conversation reopened after a restart starts an agent that has never heard
    // of it — full transcript, empty model, nothing on screen saying so.
    assert!(resumed.is_some(), "the CLI's own conversation id was never reported");

    driver.close_conversation(&SessionId("antigravity-e2e".into()));
}
