//! What a client sees when it attaches to a workspace that was already running.
//!
//! The UI protocol is deltas, which is right for a frontend that has been listening since the
//! process started and useless for one that connects half a day in. [`Editor::republish`] is the
//! other half of that bargain — and the only way to know it holds is to fold its output into a
//! `Mirror` that has never seen anything, and compare that against the mirror of a frontend that
//! was there the whole time.
//!
//! Which is what this does. It lives in `neosh-tui` rather than in `neosh-core` because the mirror
//! is the thing being asserted about, and the mirror is the frontend's.

use neosh_core::Editor;
use neosh_proto::{ApiCall, BufferId, Dock, ExtmarkOpts, Gravity, PluginId, UiEvent, WindowLayout};
use neosh_tui::Mirror;

fn plugin() -> PluginId {
    PluginId::from("test")
}

/// An editor with a couple of buffers, a couple of windows, marks, a scroll and a focus — roughly
/// the shape a workspace is in once somebody has been talking to it.
fn a_workspace() -> (Editor, BufferId, neosh_proto::WindowId) {
    let mut e = Editor::new();
    let p = plugin();

    let chat = match e.apply(&p, ApiCall::BufCreate { name: Some("[chat]".into()), scratch: true }) {
        Ok(neosh_proto::ApiOk::Buf { buf }) => buf,
        other => panic!("{other:?}"),
    };
    let composer = match e.apply(&p, ApiCall::BufCreate { name: Some("[composer]".into()), scratch: true })
    {
        Ok(neosh_proto::ApiOk::Buf { buf }) => buf,
        other => panic!("{other:?}"),
    };
    e.apply(&p, ApiCall::BufSetLines {
        buf: chat,
        start: 0,
        end: -1,
        lines: (1..=40).map(|i| format!("line {i}")).collect(),
    })
    .expect("lines");
    e.apply(&p, ApiCall::BufSetLines {
        buf: composer,
        start: 0,
        end: -1,
        lines: vec!["a half-typed question".into()],
    })
    .expect("lines");

    let ns = match e.apply(&p, ApiCall::NsCreate { name: "chat".into() }) {
        Ok(neosh_proto::ApiOk::Ns { ns }) => ns,
        other => panic!("{other:?}"),
    };
    e.apply(&p, ApiCall::MarkSet {
        ns,
        buf: chat,
        row: 3,
        col: 0,
        opts: ExtmarkOpts {
            end_col: Some(4),
            hl_group: Some("Agent.User".into()),
            ..Default::default()
        },
    })
    .expect("mark");
    // A band, which is the mark that has to survive this trip for a diff to still look like one.
    e.apply(&p, ApiCall::MarkSet {
        ns,
        buf: chat,
        row: 5,
        col: 0,
        opts: ExtmarkOpts { line_hl_group: Some("Diff.AddLine".into()), ..Default::default() },
    })
    .expect("band");

    let main = match e.apply(&p, ApiCall::WinOpen {
        buf: chat,
        layout: WindowLayout::Docked {
            dock: Dock::Main,
            size: None,
            gravity: Gravity::End,
            wrap: Some(true),
        },
    }) {
        Ok(neosh_proto::ApiOk::Win { win }) => win,
        other => panic!("{other:?}"),
    };
    let bottom = match e.apply(&p, ApiCall::WinOpen {
        buf: composer,
        layout: WindowLayout::Docked {
            dock: Dock::Bottom,
            size: Some(3),
            gravity: Gravity::Start,
            wrap: Some(true),
        },
    }) {
        Ok(neosh_proto::ApiOk::Win { win }) => win,
        other => panic!("{other:?}"),
    };

    e.apply(&p, ApiCall::WinScrollTo { win: main, top_line: 12 }).expect("scroll");
    e.apply(&p, ApiCall::WinSetCursor { win: bottom, row: 0, col: 5 }).expect("cursor");
    e.apply(&p, ApiCall::FocusPush { win: bottom }).expect("focus");
    (e, chat, main)
}

/// Fold everything the editor has queued into a mirror, as a frontend would.
fn drain_into(e: &mut Editor, m: &mut Mirror) {
    for ev in e.drain_ui() {
        m.apply(ev);
    }
    m.apply(UiEvent::Flush);
}

#[test]
fn a_client_attaching_late_ends_up_with_the_mirror_it_would_have_had_all_along() {
    let (mut e, _, _) = a_workspace();

    // The frontend that was there from the start.
    let mut early = Mirror::new();
    drain_into(&mut e, &mut early);

    // And one that connects now, to a workspace mid-conversation.
    e.republish();
    let mut late = Mirror::new();
    drain_into(&mut e, &mut late);

    assert_eq!(late.protocol_version, early.protocol_version);
    assert_eq!(late.focus, early.focus, "who has the keyboard");
    assert_eq!(late.window_order, early.window_order, "and in what order they lay out");
    assert_eq!(late.windows.len(), early.windows.len());
    for (id, w) in &early.windows {
        let l = late.windows.get(id).unwrap_or_else(|| panic!("window {id:?} is missing"));
        assert_eq!(l.buf, w.buf, "{id:?}");
        assert_eq!(l.layout, w.layout, "{id:?}");
        assert_eq!(l.cursor, w.cursor, "{id:?}");
        assert_eq!(l.top_line, w.top_line, "{id:?}: where it is scrolled to");
    }
    assert_eq!(late.buffers.len(), early.buffers.len());
    for (id, b) in &early.buffers {
        let l = late.buffers.get(id).unwrap_or_else(|| panic!("buffer {id:?} is missing"));
        assert_eq!(l.name, b.name, "{id:?}");
        assert_eq!(
            l.lines.iter().map(|r| &r.text).collect::<Vec<_>>(),
            b.lines.iter().map(|r| &r.text).collect::<Vec<_>>(),
            "{id:?}"
        );
    }
    assert_eq!(late.highlights.len(), early.highlights.len(), "the whole palette, again");
    assert!(!late.highlights.is_empty());
}

#[test]
fn the_marks_come_back_with_their_lines_and_not_a_frame_later() {
    // Text and its annotations travel in one payload — the protocol has no way to send one without
    // the other, and republishing must not be the exception that invents one.
    let (mut e, chat, _) = a_workspace();
    e.republish();
    let mut late = Mirror::new();
    drain_into(&mut e, &mut late);

    let b = late.buffers.get(&chat).expect("the transcript");
    let marked = &b.lines[3];
    assert!(
        marked.marks.iter().any(|m| m.opts.hl_group.as_deref() == Some("Agent.User")),
        "{:?}",
        marked.marks
    );
    let banded = &b.lines[5];
    assert!(
        banded.marks.iter().any(|m| m.opts.line_hl_group.as_deref() == Some("Diff.AddLine")),
        "a diff band survives a reattach: {:?}",
        banded.marks
    );
}

#[test]
fn republishing_twice_does_not_double_anything() {
    // A client that attaches, drops and attaches again is the ordinary case, not the exotic one.
    let (mut e, chat, _) = a_workspace();
    let mut m = Mirror::new();
    drain_into(&mut e, &mut m);
    let before = m.buffers[&chat].lines.len();

    for _ in 0..2 {
        e.republish();
        let mut fresh = Mirror::new();
        drain_into(&mut e, &mut fresh);
        assert_eq!(fresh.buffers[&chat].lines.len(), before);
        assert_eq!(fresh.windows.len(), m.windows.len());
    }
}

#[test]
fn what_happened_after_the_republish_still_arrives_as_an_ordinary_delta() {
    // Republishing is a catch-up, not a mode. The stream goes back to being deltas immediately, or
    // the first thing typed after attaching would need a second full send.
    let (mut e, chat, _) = a_workspace();
    e.republish();
    let mut late = Mirror::new();
    drain_into(&mut e, &mut late);

    e.apply(&plugin(), ApiCall::BufSetLines {
        buf: chat,
        start: 0,
        end: 1,
        lines: vec!["LINE one, changed".into()],
    })
    .expect("edit");
    drain_into(&mut e, &mut late);
    assert!(late.buffers[&chat].lines[0].text.starts_with("LINE"), "{:?}", late.buffers[&chat].lines[0]);
}
