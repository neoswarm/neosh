//! The codex driver against a stand-in `app-server`.
//!
//! The unit tests in the driver check the *mapping* — one notification in, the right events out.
//! This checks the part they cannot: that the conversation on the wire is one a real server would
//! recognise. Handshake, `thread/start`, `turn/start`, an approval that blocks the turn until it is
//! answered, and `turn/interrupt`.
//!
//! A stand-in rather than the real binary because `codex app-server` needs a login, and the machine
//! where that is missing is exactly the machine CI is. The script speaks the protocol back; if
//! neosh stopped sending one of these, or sent it in the wrong order, the script would answer
//! nothing and the turn would end with no message rather than with an answer.

#![cfg(unix)]

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use neosh_provider::approval::{PermissionAnswer, PermissionAsker, PermissionRequest};
use neosh_provider::drivers::{AgentDriver, CodexCliProvider};
use neosh_proto::{
    AuthRef, ContentBlock, DriverKind, InstanceConfig, InstanceId, Message, ModelSelection,
    ProviderEvent, Role, SessionId, TurnRequest,
};
use neosh_provider::Provider;
use tokio_util::sync::CancellationToken;

mod support;

/// A `codex app-server` that answers, in the order a real one would.
///
/// Request ids are not matched, they are counted: neosh numbers them from one in a fixed order, so
/// `initialize` is 1, `thread/start` is 2 and the first `turn/start` is 3. A change to that order
/// is a change to the handshake, which is the thing worth failing on.
const FAKE: &str = r#"#!/bin/sh
# argv[1] is `app-server`; anything else means neosh called the wrong subcommand.
[ "$1" = "app-server" ] || exit 64
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      echo '{"jsonrpc":"2.0","id":1,"result":{"userAgent":"fake"}}' ;;
    *'"method":"initialized"'*)
      : ;;
    *'"method":"thread/start"'*)
      echo '{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"t1"}}}' ;;
    *'"method":"turn/start"'*)
      echo '{"jsonrpc":"2.0","id":3,"result":{"turn":{"id":"u1","status":"inProgress"}}}'
      echo '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"t1","turn":{"id":"u1","status":"inProgress"}}}'
      echo '{"jsonrpc":"2.0","id":100,"method":"item/commandExecution/requestApproval","params":{"threadId":"t1","turnId":"u1","itemId":"c1","startedAtMs":1,"command":"cargo test","reason":"outside the sandbox"}}' ;;
    *'"method":"turn/interrupt"'*)
      echo '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u1","status":"cancelled"}}}' ;;
    *'"decision"'*)
      printf '%s\n' "$line" > "$0.decision"
      echo '{"jsonrpc":"2.0","method":"item/started","params":{"item":{"type":"agentMessage","id":"a1","text":""}}}'
      echo '{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"itemId":"a1","delta":"ran it"}}'
      echo '{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"agentMessage","id":"a1","text":"ran it"}}}'
      echo '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u1","status":"completed"}}}' ;;
  esac
done
"#;

/// Somebody who always gives the same answer, and remembers what they were asked.
#[derive(Debug)]
struct Answering {
    answer: PermissionAnswer,
    seen: Arc<Mutex<Vec<PermissionRequest>>>,
}

#[async_trait::async_trait]
impl PermissionAsker for Answering {
    async fn ask(&self, request: PermissionRequest) -> PermissionAnswer {
        self.seen.lock().expect("seen lock poisoned").push(request);
        self.answer.clone()
    }
}

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
        let dir = std::env::temp_dir().join(format!("neosh-codex-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("sandbox");
        support::write_executable(&dir.join("codex"), FAKE);
        Self { dir }
    }

    fn program(&self) -> String {
        self.dir.join("codex").display().to_string()
    }

    fn decision(&self) -> String {
        // Beside the script rather than at a path handed over in the environment: these tests run
        // in one process, and a shared variable is one test answering another test's question.
        std::fs::read_to_string(self.dir.join("codex.decision")).unwrap_or_default()
    }
}

fn inst() -> InstanceConfig {
    InstanceConfig {
        id: InstanceId::from("codex-cli"),
        driver: DriverKind::from("codex-cli"),
        display_name: "Codex".into(),
        base_url: None,
        brand: None,
        auth: AuthRef::Inherited,
        models: vec![],
        extra_headers: vec![],
    }
}

fn req(dir: &std::path::Path) -> TurnRequest {
    TurnRequest {
        resume: None,
        conversation: SessionId::from("c1"),
        cwd: dir.to_path_buf(),
        selection: ModelSelection {
            instance: InstanceId::from("codex-cli"),
            model: "gpt-5.6".into(),
            options: vec![],
        },
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "run the tests".into() }],
            at: None,
        }],
        tools: vec![],
        max_output_tokens: None,
    }
}

#[tokio::test]
async fn a_turn_gets_through_the_handshake_and_comes_back_with_an_answer() {
    let fake = Fake::new("turn");
    let p = CodexCliProvider::new(fake.program());
    let seen = Arc::new(Mutex::new(Vec::new()));
    p.set_permission_asker(Arc::new(Answering {
        answer: PermissionAnswer::Allow,
        seen: seen.clone(),
    }));

    let got: Vec<ProviderEvent> =
        p.stream(&inst(), req(&fake.dir), CancellationToken::new()).collect().await;

    // The whole point: nothing here is reachable without `initialize`, `initialized`,
    // `thread/start` and `turn/start` having gone out in that order and been answered.
    assert!(
        got.iter()
            .any(|e| matches!(e, ProviderEvent::TextDelta { text, .. } if text == "ran it")),
        "{got:?}"
    );
    assert!(got.contains(&ProviderEvent::MessageStop), "{got:?}");
    assert!(
        !got.iter().any(|e| matches!(e, ProviderEvent::Error { .. })),
        "a turn that worked reports nothing wrong\n{got:?}"
    );
}

#[tokio::test]
async fn an_approval_stops_the_turn_until_somebody_answers_it() {
    let fake = Fake::new("approve");
    let p = CodexCliProvider::new(fake.program());
    let seen = Arc::new(Mutex::new(Vec::new()));
    p.set_permission_asker(Arc::new(Answering {
        answer: PermissionAnswer::Allow,
        seen: seen.clone(),
    }));

    let got: Vec<ProviderEvent> =
        p.stream(&inst(), req(&fake.dir), CancellationToken::new()).collect().await;

    // The question reached a person, with the command on it rather than a paraphrase.
    let asked = seen.lock().expect("seen lock poisoned").clone();
    assert_eq!(asked.len(), 1, "asked exactly once\n{asked:?}");
    assert!(asked[0].title.contains("cargo test"), "{}", asked[0].title);

    // And the answer went back in the protocol's own vocabulary — the server sends the decisions it
    // accepts as a schema, so a client that invented a spelling would be refused.
    assert!(fake.decision().contains("\"accept\""), "{:?}", fake.decision());
    assert!(
        got.iter()
            .any(|e| matches!(e, ProviderEvent::TextDelta { text, .. } if text == "ran it")),
        "and the turn carried on afterwards\n{got:?}"
    );
}

#[tokio::test]
async fn nobody_to_ask_means_no_and_not_a_hang() {
    let fake = Fake::new("nobody");
    let p = CodexCliProvider::new(fake.program());
    // No asker. The server asks anyway — a real one would not, because the approval policy would be
    // `never`, but a driver that hung here would hang forever and this is the cheap way to prove it
    // does not.
    let got: Vec<ProviderEvent> =
        p.stream(&inst(), req(&fake.dir), CancellationToken::new()).collect().await;
    assert!(fake.decision().contains("\"decline\""), "{:?}", fake.decision());
    assert!(!got.is_empty(), "the turn ended rather than waiting for nobody");
}

#[tokio::test]
async fn cancelling_asks_the_turn_to_stop_rather_than_killing_the_thread() {
    let fake = Fake::new("interrupt");
    let p = CodexCliProvider::new(fake.program());
    let cancel = CancellationToken::new();
    // Nobody answers the approval, so the turn is genuinely blocked when the cancel arrives —
    // which is the state `<Esc>` is actually pressed in.
    let seen = Arc::new(Mutex::new(Vec::new()));
    p.set_permission_asker(Arc::new(Slow { seen: seen.clone() }));

    let stream = p.stream(&inst(), req(&fake.dir), cancel.clone());
    let waiting = tokio::spawn(async move { stream.collect::<Vec<_>>().await });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    cancel.cancel();

    let got = tokio::time::timeout(std::time::Duration::from_secs(10), waiting)
        .await
        .expect("the turn ended rather than hanging")
        .expect("the task did not panic");

    // `turn/completed` came back because the server was *asked* to stop. A driver that killed the
    // process would end the stream too — and would have taken the thread with it, so the next turn
    // would resume into a history that never heard of this one.
    assert!(
        got.iter().any(|e| matches!(e, ProviderEvent::MessageStop)),
        "the server said the turn was over\n{got:?}"
    );
}

/// An asker that never answers, so the turn is blocked when the cancellation arrives.
#[derive(Debug)]
struct Slow {
    seen: Arc<Mutex<Vec<PermissionRequest>>>,
}

#[async_trait::async_trait]
impl PermissionAsker for Slow {
    async fn ask(&self, request: PermissionRequest) -> PermissionAnswer {
        self.seen.lock().expect("seen lock poisoned").push(request);
        std::future::pending().await
    }
}

// Child lifecycle can arrive before its parent announces the child. It must never
// reset the parent's assembler, finish its turn, or spill into the next answer.
#[tokio::test]
async fn child_and_stale_turn_events_cannot_finish_or_contaminate_the_parent() {
    let fake = Fake::new("routed-turns");
    let script = r#"#!/bin/sh
n=0
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) echo '{"id":1,"result":{}}' ;;
    *'"method":"thread/start"'*) echo '{"id":2,"result":{"thread":{"id":"t1"}}}' ;;
    *'"method":"turn/start"'*)
      n=$((n + 1))
      id=$((n + 2))
      echo '{"method":"turn/started","params":{"threadId":"child","turn":{"id":"child-turn"}}}'
      echo '{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"old-turn","status":"completed"}}}'
      printf '{"method":"turn/started","params":{"threadId":"t1","turn":{"id":"u%s"}}}\n' "$n"
      printf '{"id":%s,"result":{"turn":{"id":"u%s"}}}\n' "$id" "$n"
      printf '{"method":"item/agentMessage/delta","params":{"threadId":"t1","turnId":"u%s","itemId":"a1","delta":"answer %s"}}\n' "$n" "$n"
      echo '{"method":"turn/started","params":{"threadId":"child","turn":{"id":"child-turn"}}}'
      echo '{"method":"item/agentMessage/delta","params":{"threadId":"child","turnId":"child-turn","itemId":"a1","delta":"child text"}}'
      echo '{"method":"error","params":{"threadId":"child","turnId":"child-turn","message":"child failure"}}'
      echo '{"method":"thread/tokenUsage/updated","params":{"threadId":"child","turnId":"child-turn","tokenUsage":{"last":{"inputTokens":999},"total":{"totalTokens":999},"modelContextWindow":1000}}}'
      echo '{"method":"item/started","params":{"threadId":"child","turnId":"child-turn","item":{"type":"commandExecution","id":"child-command","command":"child command"}}}'
      printf '{"method":"item/started","params":{"threadId":"t1","turnId":"u%s","item":{"type":"collabAgentToolCall","id":"parent-task","prompt":"delegate work"}}}\n' "$n"
      printf '{"method":"item/completed","params":{"threadId":"t1","turnId":"u%s","item":{"type":"collabAgentToolCall","id":"parent-task","status":"completed"}}}\n' "$n"
      echo '{"method":"turn/completed","params":{"threadId":"child","turn":{"id":"child-turn","status":"completed"}}}'
      printf '{"method":"item/agentMessage/delta","params":{"threadId":"t1","turnId":"u%s","itemId":"a1","delta":" finished"}}\n' "$n"
      printf '{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u%s","status":"completed"}}}\n' "$n" ;;
  esac
done
"#;
    support::write_executable(&fake.dir.join("codex"), script);
    let p = CodexCliProvider::new(fake.program());
    for n in 1..=2 {
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            p.stream(&inst(), req(&fake.dir), CancellationToken::new())
                .collect::<Vec<_>>(),
        )
        .await
        .expect("parent turn finishes");
        let text: String = got
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, format!("answer {n} finished"), "{got:?}");
        assert!(
            got.iter().any(|e| matches!(e, ProviderEvent::Activity {
            activity: neosh_proto::Activity::TaskStarted { task, .. }
        } if task.0 == "parent-task")),
            "{got:?}"
        );
        assert!(
            got.iter().any(|e| matches!(e, ProviderEvent::Activity {
            activity: neosh_proto::Activity::TaskEnded { task, .. }
        } if task.0 == "parent-task")),
            "{got:?}"
        );
        assert!(
            !got.iter().any(|e| matches!(
                e,
                ProviderEvent::Activity {
                    activity: neosh_proto::Activity::Context { .. }
                } | ProviderEvent::BlockStart {
                    block: neosh_proto::BlockStartKind::ToolUse { .. },
                    ..
                }
            )),
            "child context and commands stay out of the parent: {got:?}"
        );
        assert_eq!(
            got.iter()
                .filter(|e| matches!(e, ProviderEvent::MessageStart { .. }))
                .count(),
            1,
            "{got:?}"
        );
        assert_eq!(
            got.iter()
                .filter(|e| matches!(e, ProviderEvent::MessageStop))
                .count(),
            1,
            "{got:?}"
        );
    }
    p.shutdown(&SessionId::from("c1"));
}

const INTERRUPT_FAKE: &str = r#"#!/bin/sh
n=0
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id" ;;
    *'"method":"thread/start"'*) printf '{"id":%s,"result":{"thread":{"id":"t1"}}}\n' "$id" ;;
    *'"method":"turn/start"'*)
      n=$((n + 1))
      if [ "$n" = 1 ]; then
        touch "$0.starting"
        if [ -f "$0.delay" ]; then
          while [ ! -f "$0.release" ]; do sleep 0.01; done
        fi
      fi
      printf '{"id":%s,"result":{"turn":{"id":"u%s"}}}\n' "$id" "$n"
      printf '{"method":"turn/started","params":{"threadId":"t1","turn":{"id":"u%s"}}}\n' "$n"
      if [ "$n" != 1 ]; then
        printf '{"method":"item/agentMessage/delta","params":{"threadId":"t1","turnId":"u%s","itemId":"a2","delta":"new answer"}}\n' "$n"
        printf '{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u%s","status":"completed"}}}\n' "$n"
      fi ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' "$line" > "$0.interrupted"
      echo '{"method":"item/agentMessage/delta","params":{"threadId":"t1","turnId":"u1","itemId":"a1","delta":"old answer"}}'
      echo '{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u1","status":"interrupted"}}}' ;;
  esac
done
"#;

#[tokio::test]
async fn dropping_a_stream_drains_the_interrupted_turn_before_the_next_prompt() {
    let fake = Fake::new("dropped-stream");
    support::write_executable(&fake.dir.join("codex"), INTERRUPT_FAKE);
    let p = CodexCliProvider::new(fake.program());
    let mut stream = p.stream(&inst(), req(&fake.dir), CancellationToken::new());
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("turn started");
    assert!(matches!(first, Some(ProviderEvent::MessageStart { .. })));
    drop(stream);
    let got = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        p.stream(&inst(), req(&fake.dir), CancellationToken::new())
            .collect::<Vec<_>>(),
    )
    .await
    .expect("next turn finishes after draining the abandoned turn");
    assert!(
        fake.dir.join("codex.interrupted").exists(),
        "asked the old turn to stop"
    );
    let text: String = got
        .iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "new answer", "{got:?}");
    p.shutdown(&SessionId::from("c1"));
}

#[tokio::test]
async fn cancellation_before_the_start_reply_interrupts_when_the_id_arrives() {
    let fake = Fake::new("early-cancel");
    support::write_executable(&fake.dir.join("codex"), INTERRUPT_FAKE);
    std::fs::write(fake.dir.join("codex.delay"), "").expect("delay marker");
    let p = CodexCliProvider::new(fake.program());
    let cancel = CancellationToken::new();
    let stream = p.stream(&inst(), req(&fake.dir), cancel.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !fake.dir.join("codex.starting").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("server received turn/start");
    cancel.cancel();
    std::fs::write(fake.dir.join("codex.release"), "").expect("release start reply");
    let got = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stream.collect::<Vec<_>>(),
    )
    .await
    .expect("interrupt sent once the delayed reply identifies the turn");
    let interrupt =
        std::fs::read_to_string(fake.dir.join("codex.interrupted")).expect("interrupted");
    assert!(interrupt.contains("\"turnId\":\"u1\""), "{interrupt}");
    assert!(got.contains(&ProviderEvent::MessageStop), "{got:?}");
    p.shutdown(&SessionId::from("c1"));
}

#[tokio::test]
async fn a_rejected_turn_start_reports_an_error_instead_of_waiting_forever() {
    let fake = Fake::new("rejected-start");
    let script = INTERRUPT_FAKE.replace(
        "n=$((n + 1))",
        r#"n=$((n + 1))
      if [ "$n" = 1 ]; then
        printf '{"id":%s,"error":{"code":-32602,"message":"model unavailable"}}\n' "$id"
        continue
      fi"#,
    );
    support::write_executable(&fake.dir.join("codex"), &script);
    let p = CodexCliProvider::new(fake.program());
    let got = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        p.stream(&inst(), req(&fake.dir), CancellationToken::new())
            .collect::<Vec<_>>(),
    )
    .await
    .expect("start rejection ends the stream");
    assert!(got.iter().any(|e| matches!(e, ProviderEvent::Error { message, .. } if message.contains("model unavailable"))), "{got:?}");
    assert!(
        !got.contains(&ProviderEvent::MessageStop),
        "a rejected start is not a completed turn"
    );
    let next = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        p.stream(&inst(), req(&fake.dir), CancellationToken::new())
            .collect::<Vec<_>>(),
    )
    .await
    .expect("the same thread accepts the next prompt");
    assert!(
        next.iter()
            .any(|e| matches!(e, ProviderEvent::TextDelta { text, .. } if text == "new answer")),
        "{next:?}"
    );
    assert!(
        !next
            .iter()
            .any(|e| matches!(e, ProviderEvent::Error { .. })),
        "{next:?}"
    );
    p.shutdown(&SessionId::from("c1"));
}

#[tokio::test]
async fn speed_can_be_enabled_and_disabled_on_the_same_conversation() {
    let fake = Fake::new("speed");
    let script = INTERRUPT_FAKE
        .replace(
            "n=$((n + 1))",
            "n=$((n + 1))\n      printf '%s\\n' \"$line\" >> \"$0.turns\"",
        )
        .replace("if [ \"$n\" != 1 ]; then", "if true; then");
    support::write_executable(&fake.dir.join("codex"), &script);
    let p = CodexCliProvider::new(fake.program());
    let mut instance = inst();
    let mut model: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/codex_model_sol.json")).expect("fixture");
    model["defaultServiceTier"] = serde_json::json!("priority");
    instance
        .models
        .push(neosh_provider::drivers::codex_cli::codex_model(&model).expect("model"));
    for tier in [None, Some("default"), Some("priority"), Some("default")] {
        let mut request = req(&fake.dir);
        request.selection.model = "gpt-5.6-sol".into();
        if let Some(tier) = tier {
            request
                .selection
                .options
                .push(neosh_proto::OptionSelection {
                    id: "service_tier".into(),
                    value: neosh_proto::ProviderOptionValue::Text(tier.into()),
                });
        }
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            p.stream(&instance, request, CancellationToken::new())
                .collect::<Vec<_>>(),
        )
        .await
        .expect("speed change finishes");
        assert!(got.contains(&ProviderEvent::MessageStop), "{got:?}");
        assert!(
            !got.iter().any(|e| matches!(e, ProviderEvent::Error { .. })),
            "{got:?}"
        );
    }
    let sent = std::fs::read_to_string(fake.dir.join("codex.turns")).expect("recorded turns");
    let tiers: Vec<String> = sent
        .lines()
        .map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).expect("request");
            assert_eq!(v["params"]["threadId"], "t1");
            v["params"]["serviceTier"]
                .as_str()
                .expect("explicit tier")
                .to_string()
        })
        .collect();
    assert_eq!(tiers, ["priority", "default", "priority", "default"]);
    p.shutdown(&SessionId::from("c1"));
}
