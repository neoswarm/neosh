//! A panel repaint is one mutation, or it is visible halfway through.
//!
//! The frontend draws on a coalescing deadline that knows nothing about how far through a repaint a
//! plugin is. A panel that writes its text, clears its namespace and then sets its marks one call at
//! a time is therefore observable in two broken states: rows wearing the clamped remains of the last
//! repaint, and — after the clear — rows wearing no marks at all, which draw in `Normal`. On a
//! sidebar redrawing ten times a second that is a dim column flashing bright white.
//!
//! `BufRender` is the whole repaint in one call. These tests assert that no drain can see part of
//! one.

use neosh_core::Editor;
use neosh_proto::{
    ApiCall, ApiOk, ExtmarkOpts, LineDraw, MarkDraw, NamespaceId, PluginId, UiEvent,
};

fn ns(e: &mut Editor, plugin: &PluginId, name: &str) -> NamespaceId {
    match e.apply(plugin, ApiCall::NsCreate { name: name.into() }).expect("ns") {
        ApiOk::Ns { ns } => ns,
        other => panic!("expected a namespace, got {other:?}"),
    }
}

fn banded(text: &str, group: &str) -> LineDraw {
    LineDraw {
        text: text.into(),
        marks: vec![MarkDraw {
            col: 0,
            opts: ExtmarkOpts {
                hl_group: Some(group.into()),
                end_col: Some(text.len() as u32),
                ..Default::default()
            },
        }],
    }
}

/// Every row of every `BufferLines` event for `buf`, as (text, number of marks).
fn rows(events: &[UiEvent], buf: neosh_proto::BufferId) -> Vec<(String, usize)> {
    events
        .iter()
        .filter_map(|e| match e {
            UiEvent::BufferLines { buf: b, lines, .. } if *b == buf => Some(lines),
            _ => None,
        })
        .flatten()
        .map(|l| (l.text.clone(), l.marks.len()))
        .collect()
}

fn panel() -> Vec<LineDraw> {
    (0..6).map(|i| banded(&format!("     * conversation {i}"), "Status.Monitoring")).collect()
}

#[test]
fn a_repaint_is_one_event_and_never_a_row_without_its_marks() {
    let mut e = Editor::new();
    let plugin = PluginId::from("sidebar");
    let buf = e.create_buffer("[sidebar]");
    let sidebar = ns(&mut e, &plugin, "neosh.sidebar");

    // Ten frames, drained between each, exactly as the host's deadline would.
    for _ in 0..10 {
        e.apply(&plugin, ApiCall::BufRender {
            buf,
            ns: sidebar,
            start: 0,
            end: -1,
            lines: panel(),
        })
        .expect("render");

        let drained = e.drain_events();
        let events = drained
            .iter()
            .filter(|ev| matches!(ev, UiEvent::BufferLines { buf: b, .. } if *b == buf))
            .count();
        assert_eq!(events, 1, "a repaint is one event, not one per mark: {drained:?}");

        let drawn = rows(&drained, buf);
        assert_eq!(drawn.len(), 6, "every row travels: {drawn:?}");
        assert!(
            drawn.iter().all(|(_, marks)| *marks == 1),
            "a row arrived without its highlight and would draw in Normal: {drawn:?}",
        );
    }
}

#[test]
fn a_repaint_does_not_inherit_the_marks_of_the_one_before_it() {
    // `set_lines` carries a replaced line's marks onto its positional counterpart — the rule that
    // makes streaming work. Without the clear, a panel redrawing in place would accumulate one mark
    // per frame, and at equal priority the narrowest of them wins: a row painted by whatever was
    // shortest several seconds ago.
    let mut e = Editor::new();
    let plugin = PluginId::from("sidebar");
    let buf = e.create_buffer("[sidebar]");
    let sidebar = ns(&mut e, &plugin, "neosh.sidebar");

    for _ in 0..20 {
        e.apply(&plugin, ApiCall::BufRender {
            buf,
            ns: sidebar,
            start: 0,
            end: -1,
            lines: panel(),
        })
        .expect("render");
    }

    let drawn = rows(&e.drain_events(), buf);
    assert!(
        drawn.iter().all(|(_, marks)| *marks == 1),
        "twenty frames left more than one mark on a row: {drawn:?}",
    );
}

#[test]
fn another_namespace_survives_the_panel_underneath_it_redrawing() {
    // The selection is drawn in its own namespace. A repaint of the rows it sits on must not be
    // able to take it off — otherwise every panel would have to know about every overlay on it.
    let mut e = Editor::new();
    let plugin = PluginId::from("sidebar");
    let buf = e.create_buffer("[sidebar]");
    let sidebar = ns(&mut e, &plugin, "neosh.sidebar");
    let overlay = ns(&mut e, &plugin, "neosh.selection");

    e.apply(&plugin, ApiCall::BufRender { buf, ns: sidebar, start: 0, end: -1, lines: panel() })
        .expect("render");
    e.apply(&plugin, ApiCall::MarkSet {
        ns: overlay,
        buf,
        row: 2,
        col: 0,
        opts: ExtmarkOpts { hl_group: Some("Search".into()), end_col: Some(4), ..Default::default() },
    })
    .expect("overlay");
    e.drain_events();

    e.apply(&plugin, ApiCall::BufRender { buf, ns: sidebar, start: 0, end: -1, lines: panel() })
        .expect("redraw");

    let drawn = rows(&e.drain_events(), buf);
    assert_eq!(drawn[2].1, 2, "the overlay was cleared by somebody else's repaint: {drawn:?}");
    assert_eq!(drawn[0].1, 1, "an untouched row grew a mark: {drawn:?}");
}

#[test]
fn a_partial_repaint_leaves_the_rows_it_did_not_write_alone() {
    let mut e = Editor::new();
    let plugin = PluginId::from("sidebar");
    let buf = e.create_buffer("[sidebar]");
    let sidebar = ns(&mut e, &plugin, "neosh.sidebar");

    e.apply(&plugin, ApiCall::BufRender { buf, ns: sidebar, start: 0, end: -1, lines: panel() })
        .expect("render");
    e.drain_events();

    e.apply(&plugin, ApiCall::BufRender {
        buf,
        ns: sidebar,
        start: 1,
        end: 2,
        lines: vec![banded("     * conversation 1 (working)", "Status.Working")],
    })
    .expect("one row");

    let drawn = rows(&e.drain_events(), buf);
    assert_eq!(drawn.len(), 1, "only the row that changed travels: {drawn:?}");
    assert_eq!(drawn[0].0, "     * conversation 1 (working)");
    assert_eq!(drawn[0].1, 1);

    let all = match e.apply(&plugin, ApiCall::MarkAll { ns: sidebar, buf }).expect("marks") {
        ApiOk::Marks { marks } => marks,
        other => panic!("{other:?}"),
    };
    assert_eq!(all.len(), 6, "a one-row repaint disturbed the other five: {all:?}");
}
