//! The claude driver against a stand-in CLI, on the one thing unit tests cannot reach: what the
//! *process* is left holding when a turn ends early.
//!
//! One `claude` holds one conversation and outlives every turn in it, which is the whole reason the
//! driver keeps it — and it is also the hazard. A turn that is walked away from (`<Esc>`, or
//! switching to another conversation while this one works) leaves the CLI mid-answer with the rest
//! of that answer still in the pipe. Nothing else reads that pipe. So the next turn does, and
//! reports the tail of the abandoned turn as its own reply.
//!
//! That is not a hypothetical: it is `Compaction canceled.` arriving under a message about
//! something else, before the answer that message was waiting for.
//!
//! A stand-in rather than the real binary for the reason the codex tests give: the real one needs a
//! login, and the machine without one is exactly the machine CI is.

#![cfg(unix)]

use futures::StreamExt;
use neosh_provider::Provider;
use neosh_provider::drivers::ClaudeCliProvider;
use neosh_proto::{
    AuthRef, ContentBlock, DriverKind, InstanceConfig, InstanceId, Message, ModelSelection,
    ProviderEvent, Role, SessionId, TurnRequest,
};
use tokio_util::sync::CancellationToken;

/// A `claude --print --input-format stream-json` that answers in two halves.
///
/// The gap is the point. The first turn says `FIRST-HEAD`, pauses long enough to be abandoned, and
/// only then says `FIRST-TAIL` and closes with its `result`. Whoever ends up reading those last
/// three lines is what these tests are about. The second turn says `SECOND` and closes.
///
/// Control requests are read and ignored — an interrupt this stand-in cannot honour mid-`sleep` is
/// the realistic case anyway, and `get_context_usage` is answered so a turn does not sit out its
/// deadline waiting for a number nobody asserts on.
const FAKE: &str = r#"#!/bin/sh
n=0
say() { printf '%s\n' "$1"; }
while IFS= read -r line; do
  case "$line" in
    *'"subtype":"get_context_usage"'*)
      say '{"type":"control_response","response":{"subtype":"success","request_id":"x","response":{"totalTokens":10,"maxTokens":100}}}' ;;
    *'"type":"user"'*)
      n=$((n+1))
      if [ "$n" = 1 ]; then
        say '{"type":"stream_event","session_id":"s","event":{"type":"message_start","message":{"model":"m","usage":{}}}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"FIRST-HEAD"}}}'
        sleep 1
        say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"FIRST-TAIL"}}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_stop","index":0}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"message_stop"}}'
        say '{"type":"result","subtype":"success","is_error":false,"result":"FIRST-TAIL","session_id":"s"}'
      else
        say '{"type":"stream_event","session_id":"s","event":{"type":"message_start","message":{"model":"m","usage":{}}}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"SECOND"}}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_stop","index":0}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"message_stop"}}'
        say '{"type":"result","subtype":"success","is_error":false,"result":"SECOND","session_id":"s"}'
      fi ;;
  esac
done
"#;

struct Fake {
    dir: std::path::PathBuf,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fake {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("neosh-claude-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("sandbox");
        let script = dir.join("claude");
        std::fs::write(&script, FAKE).expect("script");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Self { dir }
    }

    fn program(&self) -> String {
        self.dir.join("claude").display().to_string()
    }
}

fn inst() -> InstanceConfig {
    InstanceConfig {
        id: InstanceId::from("claude-cli"),
        driver: DriverKind::from("claude-cli"),
        display_name: "Claude".into(),
        base_url: None,
        brand: None,
        auth: AuthRef::Inherited,
        models: vec![],
        extra_headers: vec![],
    }
}

/// One conversation throughout: the process is per conversation, and sharing it is the hazard.
fn req(dir: &std::path::Path, text: &str) -> TurnRequest {
    TurnRequest {
        resume: None,
        conversation: SessionId::from("c1"),
        cwd: dir.to_path_buf(),
        selection: ModelSelection {
            instance: InstanceId::from("claude-cli"),
            model: "claude-opus-5".into(),
            options: vec![],
        },
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }],
        tools: vec![],
        max_output_tokens: None,
    }
}

fn text_of(events: &[ProviderEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn what_an_abandoned_turn_was_still_saying_is_not_the_next_turn_s_answer() {
    let fake = Fake::new("drain");
    let p = ClaudeCliProvider::new(fake.program());

    // Walked away from exactly the way the agent does on `<Esc>`: the token is cancelled and the
    // stream is dropped in the same breath, so the driver's next `send` has nowhere to go.
    let token = CancellationToken::new();
    let mut first = p.stream(&inst(), req(&fake.dir, "one"), token.clone());
    let mut head = false;
    while let Some(ev) = first.next().await {
        if matches!(&ev, ProviderEvent::TextDelta { text, .. } if text.contains("FIRST-HEAD")) {
            head = true;
            break;
        }
    }
    assert!(head, "the stand-in never got as far as its first half");
    token.cancel();
    drop(first);

    let got: Vec<ProviderEvent> =
        p.stream(&inst(), req(&fake.dir, "two"), CancellationToken::new()).collect().await;
    let said = text_of(&got);

    assert!(
        !said.contains("FIRST-TAIL"),
        "the rest of the abandoned turn was read as this one's reply\n{said:?}"
    );
    assert!(said.contains("SECOND"), "and this turn's own answer never arrived\n{said:?}");
}
