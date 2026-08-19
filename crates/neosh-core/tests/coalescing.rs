//! Streaming produces one buffer edit per token. The frontend must not.
//!
//! This is the deterministic half of a property the end-to-end test can only observe by wall clock:
//! the *merge* happens in `push_ui` and does not depend on how fast the machine is, so it is
//! asserted here where a loaded CI box cannot make it flap.

use neosh_core::Editor;
use neosh_proto::{ApiCall, PluginId, UiEvent};

fn appends(events: &[UiEvent], buf: neosh_proto::BufferId) -> Vec<Vec<String>> {
    events
        .iter()
        .filter_map(|e| match e {
            UiEvent::BufferLines { buf: b, lines, .. } if *b == buf => {
                Some(lines.iter().map(|l| l.text.clone()).collect())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_burst_of_token_appends_becomes_one_frame() {
    let mut e = Editor::new();
    let plugin = PluginId::from("test");
    let buf = e.create_buffer("[chat]");
    e.drain_ui();

    for token in ["All ", "done", "."] {
        e.apply(&plugin, ApiCall::BufAppendText { buf, text: token.into() }).expect("append");
    }

    let frames = appends(&e.drain_ui(), buf);
    assert_eq!(frames.len(), 1, "three appends, one frame: {frames:?}");
    assert_eq!(frames[0], vec!["All done."], "carrying the final text");
}

#[test]
fn an_edit_outside_the_pending_region_starts_a_new_frame() {
    // The merge is only sound when the new edit lands inside what the previous event already
    // rewrote. Collapsing anything further would leave the mirror needing a splice it never got.
    let mut e = Editor::new();
    let plugin = PluginId::from("test");
    let buf = e.create_buffer("[chat]");
    e.apply(&plugin, ApiCall::BufSetLines {
        buf,
        start: 0,
        end: -1,
        lines: vec!["one".into(), "two".into(), "three".into()],
    })
    .expect("seed");
    e.drain_ui();

    e.apply(&plugin, ApiCall::BufSetLines { buf, start: 0, end: 1, lines: vec!["ONE".into()] })
        .expect("edit");
    e.apply(&plugin, ApiCall::BufSetLines { buf, start: 2, end: 3, lines: vec!["THREE".into()] })
        .expect("edit");

    let frames = appends(&e.drain_ui(), buf);
    assert_eq!(frames.len(), 2, "two disjoint edits stay two frames: {frames:?}");
}

#[test]
fn edits_to_two_buffers_never_merge_into_each_other() {
    let mut e = Editor::new();
    let plugin = PluginId::from("test");
    let chat = e.create_buffer("[chat]");
    let composer = e.create_buffer("[composer]");
    e.drain_ui();

    e.apply(&plugin, ApiCall::BufAppendText { buf: chat, text: "a".into() }).expect("append");
    e.apply(&plugin, ApiCall::BufAppendText { buf: composer, text: "b".into() }).expect("append");

    let ui = e.drain_ui();
    assert_eq!(appends(&ui, chat).len(), 1);
    assert_eq!(appends(&ui, composer).len(), 1);
}
