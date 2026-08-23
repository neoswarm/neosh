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
    e.drain_events();

    for token in ["All ", "done", "."] {
        e.apply(&plugin, ApiCall::BufAppendText { buf, text: token.into() }).expect("append");
    }

    let frames = appends(&e.drain_events(), buf);
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
    e.drain_events();

    e.apply(&plugin, ApiCall::BufSetLines { buf, start: 0, end: 1, lines: vec!["ONE".into()] })
        .expect("edit");
    e.apply(&plugin, ApiCall::BufSetLines { buf, start: 2, end: 3, lines: vec!["THREE".into()] })
        .expect("edit");

    let frames = appends(&e.drain_events(), buf);
    assert_eq!(frames.len(), 2, "two disjoint edits stay two frames: {frames:?}");
}

#[test]
fn edits_to_two_buffers_never_merge_into_each_other() {
    let mut e = Editor::new();
    let plugin = PluginId::from("test");
    let chat = e.create_buffer("[chat]");
    let composer = e.create_buffer("[composer]");
    e.drain_events();

    e.apply(&plugin, ApiCall::BufAppendText { buf: chat, text: "a".into() }).expect("append");
    e.apply(&plugin, ApiCall::BufAppendText { buf: composer, text: "b".into() }).expect("append");

    let ui = e.drain_events();
    assert_eq!(appends(&ui, chat).len(), 1);
    assert_eq!(appends(&ui, composer).len(), 1);
}

/// Turning motion off strips the movement and keeps the colour.
///
/// Both halves matter. A setting that made the working indicator *disappear* rather than hold
/// still would be a setting nobody uses twice; and a frontend that had to know about `ui.motion`
/// would need one more concept for every future frontend to reimplement.
#[test]
fn motion_can_be_switched_off_without_the_text_going_with_it() {
    use neosh_proto::{HighlightDef, UiEvent};

    let mut editor = Editor::new();
    let animated = |editor: &mut Editor| -> (bool, bool) {
        let mut moves = false;
        let mut coloured = false;
        for ev in editor.drain_events() {
            if let UiEvent::HighlightDefined { name, def } = ev
                && name == "Status.Streaming"
                && let HighlightDef::Spec { spec } = def
            {
                moves = spec.animate.is_some();
                coloured = spec.fg.is_some();
            }
        }
        (moves, coloured)
    };

    assert_eq!(animated(&mut editor), (true, true), "it moves by default");

    assert!(editor.set_motion(false), "the change is reported, so the caller can skip a redraw");
    assert_eq!(animated(&mut editor), (false, true), "still says something, just holds still");
    assert!(!editor.set_motion(false), "setting it twice changes nothing");

    assert!(editor.set_motion(true));
    assert_eq!(animated(&mut editor), (true, true), "and back");
}
