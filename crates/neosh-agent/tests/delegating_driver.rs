//! What comes back when the model runs its own tools.
//!
//! `claude -p --output-format stream-json` replays a whole CLI session down one stream: several
//! `message_start`s, each numbering its content blocks from zero, with the tool results arriving
//! as `user` lines in between. Both of those broke assumptions that were safe for a model API and
//! wrong for an agent — so the fixture here is a real captured session, not a hand-written one.

use neosh_agent::turn::{TurnAssembler, TurnUpdate};
use neosh_proto::{ContentBlock, Role};
use neosh_provider::sse::{ClaudeState, claude_cli_line};

/// One real `claude -p` session: read a file, then answer from it.
fn replay() -> (TurnAssembler, Vec<TurnUpdate>) {
    let raw = include_str!("fixtures/claude_cli_tool_turn.jsonl");
    let mut a = TurnAssembler::new();
    let mut state = ClaudeState::default();
    let mut updates = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).expect("fixture is JSON");
        for ev in claude_cli_line(&v, &mut state) {
            updates.extend(a.push(ev));
        }
    }
    (a, updates)
}

#[test]
fn a_second_message_does_not_overwrite_the_first_ones_tool_call() {
    // Block indices restart at zero per message. Keyed by index alone, the second message's text
    // block lands on top of the first message's tool call and the transcript loses the tool
    // entirely — a turn that visibly read a file and has no record of having done so.
    let (a, _) = replay();
    let calls = a.tool_calls();
    assert_eq!(calls.len(), 1, "the call the CLI made survived the message that followed it");
    assert_eq!(calls[0].1, "Read");
    assert!(
        calls[0].2.get("file_path").and_then(|v| v.as_str()).is_some_and(|p| p.ends_with(".txt")),
        "with its arguments intact: {:?}",
        calls[0].2
    );
}

#[test]
fn the_result_the_cli_produced_is_reported_rather_than_dropped() {
    // The CLI runs the tool itself and reports the result on a `user` line. Ignoring those leaves
    // a transcript full of calls and no answers, which reads as a tool that hung.
    let (_, updates) = replay();
    let result = updates
        .iter()
        .find_map(|u| match u {
            TurnUpdate::ToolResult { id, content, is_error } => Some((id, content, is_error)),
            _ => None,
        })
        .expect("the tool result reached the assembler");
    assert!(!result.2, "it succeeded");
    assert!(result.1.contains("hello"), "carrying what the tool actually returned: {}", result.1);

    let started = updates.iter().any(|u| matches!(u, TurnUpdate::ToolReady { .. }));
    assert!(started, "and the call it answers was announced while it was happening");
}

#[test]
fn a_delegated_session_is_recorded_in_the_shape_it_actually_had() {
    // Assistant, then the tool result as its own user message, then assistant again. Squashing the
    // result into the assistant message that asked for it produces an array no API will accept —
    // which matters the moment you switch this conversation to a different model.
    let (a, _) = replay();
    let msgs = a.messages();
    let roles: Vec<Role> = msgs.iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::Assistant, Role::User, Role::Assistant], "{msgs:#?}");

    assert!(
        msgs[0].content.iter().any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "Read")),
        "the call is in the first message"
    );
    assert!(
        matches!(&msgs[1].content[..], [ContentBlock::ToolResult { .. }]),
        "the result is a user message of its own"
    );
    assert!(
        msgs[2].content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("lines"))),
        "and the answer came after it"
    );
}
