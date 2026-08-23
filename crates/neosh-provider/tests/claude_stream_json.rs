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

mod support;

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
        Self::running(name, FAKE)
    }

    fn running(name: &str, script: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("neosh-claude-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("sandbox");
        support::write_executable(&dir.join("claude"), script);
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

/// A `claude` that keeps talking after the turn is over, which is what one holding a backgrounded
/// shell does.
///
/// Every line here was captured off `claude 2.1.240`, in this order. The first turn starts a
/// detached `sleep`, answers, and closes with its `result`. Some time later the shell exits — and
/// the CLI does two things nothing was waiting for: it reports the task, and then it *runs a turn
/// of its own* about it, with a fresh `init`, an assistant message and a second `result`. All of it
/// lands in a pipe nobody is reading, and sits there until the next question is asked.
const CHATTY: &str = r#"#!/bin/sh
n=0
say() { printf '%s\n' "$1"; }
while IFS= read -r line; do
  case "$line" in
    *'"subtype":"get_context_usage"'*)
      say '{"type":"control_response","response":{"subtype":"success","request_id":"x","response":{"totalTokens":10,"maxTokens":100}}}' ;;
    *'"type":"user"'*)
      n=$((n+1))
      if [ "$n" = 1 ]; then
        say '{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bcuwuj1ft","task_type":"local_bash","description":"npm run build"}]}'
        say '{"type":"system","subtype":"task_started","task_id":"bcuwuj1ft","tool_use_id":"t1","description":"npm run build","is_backgrounded":true,"task_type":"local_bash"}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"message_start","message":{"model":"m","usage":{}}}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"STARTED"}}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_stop","index":0}}'
        say '{"type":"stream_event","session_id":"s","event":{"type":"message_stop"}}'
        say '{"type":"result","subtype":"success","is_error":false,"result":"STARTED","session_id":"s"}'
        # Detached, and that is the whole point: the shell exits well after the turn it was started
        # in has been read to its end and let go of. Everything below lands in a pipe nobody holds.
        (
          sleep 2
          say '{"type":"system","subtype":"background_tasks_changed","tasks":[]}'
          say '{"type":"system","subtype":"task_updated","task_id":"bcuwuj1ft","patch":{"status":"completed","end_time":1787457600637}}'
          say '{"type":"system","subtype":"task_notification","task_id":"bcuwuj1ft","status":"completed","summary":"npm run build"}'
          # The turn the CLI then runs at itself about the shell exiting.
          say '{"type":"system","subtype":"init","session_id":"s","slash_commands":[]}'
          say '{"type":"stream_event","session_id":"s","event":{"type":"message_start","message":{"model":"m","usage":{}}}}'
          say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}'
          say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"UNASKED"}}}'
          say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_stop","index":0}}'
          say '{"type":"stream_event","session_id":"s","event":{"type":"message_stop"}}'
          say '{"type":"result","subtype":"success","is_error":false,"result":"UNASKED","session_id":"s"}'
        ) &
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

fn sets_of(events: &[ProviderEvent]) -> Vec<Vec<neosh_proto::BackgroundTask>> {
    events
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::Activity { activity: neosh_proto::Activity::Background { tasks }, .. } => {
                Some(tasks.clone())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_turn_that_walked_away_from_a_shell_is_told_when_the_shell_finishes() {
    let fake = Fake::running("background", CHATTY);
    let p = ClaudeCliProvider::new(fake.program());

    let first: Vec<ProviderEvent> =
        p.stream(&inst(), req(&fake.dir, "one"), CancellationToken::new()).collect().await;
    assert_eq!(
        sets_of(&first),
        vec![
            // The reset a freshly started process owes its receiver. Nothing is running yet, and
            // nothing will say so — the set is only reported when it changes.
            vec![],
            vec![neosh_proto::BackgroundTask {
                id: neosh_proto::TaskId("bcuwuj1ft".into()),
                title: "npm run build".into(),
                kind: Some("local_bash".into()),
            }],
        ],
        "the turn ends knowing one thing is still going"
    );
    assert!(text_of(&first).contains("STARTED"));

    // Long enough for the shell to have exited and the CLI to have said so — and to have talked to
    // itself about it. All of that is now in the pipe, ahead of the prompt this turn is about to
    // send.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let second: Vec<ProviderEvent> =
        p.stream(&inst(), req(&fake.dir, "two"), CancellationToken::new()).collect().await;
    let said = text_of(&second);

    assert_eq!(
        sets_of(&second).last().map(Vec::len),
        Some(0),
        "the empty set is what puts the indicator out, and it is on the far side of a `result`"
    );
    // Both halves of the mis-attribution the drain exists to stop. The stray `result` ended the
    // turn before the CLI had read the prompt, so this one used to be `UNASKED` and nothing else —
    // the model's reaction to a shell exiting, drawn as the answer to a question about something
    // else — with the real reply arriving one turn late.
    assert!(
        !said.contains("UNASKED"),
        "a turn nobody asked for was read as this one's answer\n{said:?}"
    );
    assert!(said.contains("SECOND"), "and this turn's own answer never arrived\n{said:?}");
}

/// A `claude` that asks something and *waits*, which is what the control protocol always was.
///
/// The CLI blocks on every `control_request` it sends until a `control_response` carrying that id
/// comes back, so this stand-in does too: it says nothing more until it is answered, and the turn
/// only ends because something answered it.
///
/// Two requests, and neither of them is `AskUserQuestion`. The first is `ExitPlanMode`, captured
/// off `claude 2.1.237` — `requires_user_interaction`, and a `plan` where a question path would
/// look for `questions`. The second is a subtype that does not exist, standing in for the one the
/// next release invents.
const WAITING: &str = r##"#!/bin/sh
n=0
say() { printf '%s\n' "$1"; }
finish() {
  say '{"type":"stream_event","session_id":"s","event":{"type":"message_start","message":{"model":"m","usage":{}}}}'
  say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}'
  say "{\"type\":\"stream_event\",\"session_id\":\"s\",\"event\":{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"$1\"}}}"
  say '{"type":"stream_event","session_id":"s","event":{"type":"content_block_stop","index":0}}'
  say '{"type":"stream_event","session_id":"s","event":{"type":"message_stop"}}'
  say "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"$1\",\"session_id\":\"s\"}"
}
while IFS= read -r line; do
  case "$line" in
    *'"subtype":"get_context_usage"'*)
      say '{"type":"control_response","response":{"subtype":"success","request_id":"x","response":{"totalTokens":10,"maxTokens":100}}}' ;;
    *'"type":"control_response"'*)
      case "$line" in
        *'"request_id":"plan-1"'*) finish ANSWERED-PLAN ;;
        *'"request_id":"odd-1"'*) finish ANSWERED-ODD ;;
      esac ;;
    *'"type":"user"'*)
      n=$((n+1))
      if [ "$n" = 1 ]; then
        say '{"type":"control_request","request_id":"plan-1","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","display_name":"ExitPlanMode","input":{"plan":"# A plan\n","planFilePath":"/tmp/p.md"},"tool_use_id":"toolu_plan","requires_user_interaction":true}}'
      else
        say '{"type":"control_request","request_id":"odd-1","request":{"subtype":"something_invented_later"}}'
      fi ;;
  esac
done
"##;

/// The timeout is the assertion. A request nothing answers is not a turn that ends badly, it is a
/// turn that does not end — which on screen is a spinner over an empty conversation for as long as
/// the workspace is up, and in a test is a run that hangs rather than fails.
async fn answered(p: &ClaudeCliProvider, fake: &Fake, text: &str) -> String {
    let stream = p.stream(&inst(), req(&fake.dir, text), CancellationToken::new());
    let turn = stream.collect::<Vec<_>>();
    let got = tokio::time::timeout(std::time::Duration::from_secs(20), turn)
        .await
        .expect("the turn never ended: the CLI asked something nothing replied to");
    text_of(&got)
}

#[tokio::test]
async fn a_request_to_leave_plan_mode_is_answered_rather_than_dropped() {
    // The hang, end to end. `requires_user_interaction` was read as "this is a question", so the
    // permission path stood aside; the question path wanted a `questions` array `ExitPlanMode`
    // does not have, so it stood aside too. Nothing wrote a line back, and the CLI is entitled to
    // wait forever — 94 minutes, in the conversation this was found in.
    let fake = Fake::running("waiting", WAITING);
    // One provider for both turns, because one `claude` holds one conversation: a second provider
    // would be a second process, back at its first question.
    let p = ClaudeCliProvider::new(fake.program());
    assert!(
        answered(&p, &fake, "one").await.contains("ANSWERED-PLAN"),
        "nothing replied to the CLI's plan-mode request"
    );

    // And the backstop under it, for the subtype this was not written against. A refusal ends the
    // turn badly; silence does not end it at all.
    assert!(
        answered(&p, &fake, "two").await.contains("ANSWERED-ODD"),
        "a control request nothing understood was dropped instead of refused"
    );
}
