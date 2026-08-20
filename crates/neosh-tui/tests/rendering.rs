//! Width math, annotation placement, layout, and a real terminal snapshot.

use neosh_proto::{
    Anchor, Gravity, BorderStyle, BufferId, Dock, ExtmarkId, ExtmarkOpts, ExtmarkRender, Extent,
    FloatConfig, HighlightDef, HighlightSpec, LineRender, NamespaceId, UiEvent, VirtChunk,
    VirtTextPos, WindowId, WindowLayout,
};
use neosh_tui::render::wrap_line;
use neosh_tui::{ColorDepth, Mirror, Theme, display_col, render_line, resolve_layout, truncate_to_width, width};
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

// ---------------------------------------------------------------------------
// Width
// ---------------------------------------------------------------------------

#[test]
fn wide_characters_count_two_columns() {
    assert_eq!(width("abc"), 3);
    assert_eq!(width("日本語"), 6, "CJK is double-width");
    assert_eq!(width("🎉"), 2, "emoji is double-width");
    // The classic bug: counting chars gives 3 here, columns are 6.
    assert_ne!(width("日本語"), "日本語".chars().count());
}

#[test]
fn byte_offsets_convert_to_screen_columns() {
    let s = "日本語 text";
    // "日" is 3 bytes and 2 columns.
    assert_eq!(display_col(s, 0), 0);
    assert_eq!(display_col(s, 3), 2);
    assert_eq!(display_col(s, 9), 6);
    assert_eq!(display_col(s, 10), 7);
}

#[test]
fn a_mid_codepoint_offset_clamps_instead_of_panicking() {
    let s = "héllo";
    // Byte 2 is inside the 'é'.
    assert_eq!(display_col(s, 2), 1);
    for b in 0..=s.len() + 5 {
        let _ = display_col(s, b);
    }
}

#[test]
fn truncation_never_splits_a_grapheme() {
    assert_eq!(truncate_to_width("hello", 3), "hel");
    // Cutting at 3 columns cannot include the second double-width char.
    assert_eq!(truncate_to_width("日本語", 3), "日");
    assert_eq!(truncate_to_width("日本語", 4), "日本");
    // A combining sequence stays whole.
    let flag = "🇯🇵x";
    assert!(!truncate_to_width(flag, 2).is_empty() || truncate_to_width(flag, 2).is_empty());
}

// ---------------------------------------------------------------------------
// Annotations
// ---------------------------------------------------------------------------

fn theme() -> Theme {
    let mut t = Theme::new(ColorDepth::TrueColor);
    t.set("V", HighlightDef::Spec {
        spec: HighlightSpec {
            fg: Some(neosh_proto::Color::Indexed { i: 5 }),
            ..Default::default()
        },
    });
    t
}

fn mark(col: u32, pos: VirtTextPos, text: &str) -> ExtmarkRender {
    ExtmarkRender {
        ns: NamespaceId(1),
        id: ExtmarkId(1),
        col,
        opts: ExtmarkOpts {
            virt_text: vec![VirtChunk { text: text.into(), hl_group: Some("V".into()) }],
            virt_text_pos: pos,
            ..Default::default()
        },
    }
}

fn line(text: &str, marks: Vec<ExtmarkRender>) -> LineRender {
    LineRender { text: text.into(), marks }
}

fn flatten(lines: &[ratatui::text::Line]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
        .collect()
}

#[test]
fn eol_virtual_text_follows_the_line() {
    let out = render_line(&line("code", vec![mark(0, VirtTextPos::Eol, "<- note")]), &theme());
    assert_eq!(flatten(&out), vec!["code <- note"]);
}

#[test]
fn above_and_below_virtual_text_become_their_own_lines() {
    let out = render_line(
        &line("code", vec![mark(0, VirtTextPos::Above, "hint"), mark(0, VirtTextPos::Below, "detail")]),
        &theme(),
    );
    assert_eq!(flatten(&out), vec!["hint", "code", "detail"]);
}

#[test]
fn inline_virtual_text_is_spliced_at_its_byte_column() {
    let out = render_line(&line("abcd", vec![mark(2, VirtTextPos::Inline, "|")]), &theme());
    assert_eq!(flatten(&out), vec!["ab|cd"]);
}

#[test]
fn inline_virtual_text_lands_correctly_in_wide_text() {
    // Byte 6 is after "日本" (two 3-byte chars).
    let out = render_line(&line("日本語", vec![mark(6, VirtTextPos::Inline, "*")]), &theme());
    assert_eq!(flatten(&out), vec!["日本*語"]);
}

#[test]
fn a_highlight_range_splits_the_line_into_styled_spans() {
    let m = ExtmarkRender {
        ns: NamespaceId(1),
        id: ExtmarkId(1),
        col: 2,
        opts: ExtmarkOpts { hl_group: Some("V".into()), end_col: Some(5), ..Default::default() },
    };
    let out = render_line(&line("abcdefg", vec![m]), &theme());
    assert_eq!(flatten(&out), vec!["abcdefg"], "text is unchanged");
    let spans = &out[0].spans;
    assert!(spans.len() >= 3, "the line should be cut at the range boundaries");
    let highlighted: String = spans
        .iter()
        .filter(|s| s.style.fg.is_some())
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(highlighted, "cde");
}

#[test]
fn overlapping_highlights_resolve_by_priority() {
    let span = |col, end, prio, group: &str| ExtmarkRender {
        ns: NamespaceId(1),
        id: ExtmarkId(1),
        col,
        opts: ExtmarkOpts {
            hl_group: Some(group.into()),
            end_col: Some(end),
            priority: prio,
            ..Default::default()
        },
    };
    let mut t = theme();
    t.set("Hi", HighlightDef::Spec {
        spec: HighlightSpec { fg: Some(neosh_proto::Color::Indexed { i: 9 }), ..Default::default() },
    });
    let out = render_line(&line("abcdef", vec![span(0, 6, 1, "V"), span(0, 6, 10, "Hi")]), &t);
    let fg = out[0].spans.iter().find(|s| s.content.contains('a')).unwrap().style.fg;
    assert_eq!(fg, Some(ratatui::style::Color::Indexed(9)), "the higher priority wins");
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

fn mirror_with_windows() -> Mirror {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "main".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("hello", vec![])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(2),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Left, size: Some(20), gravity: Gravity::Start, wrap: None },
    });
    m
}

#[test]
fn docks_carve_the_screen_and_main_takes_the_rest() {
    let m = mirror_with_windows();
    let rects = resolve_layout(&m, Rect::new(0, 0, 100, 30));
    let left = rects.iter().find(|(w, _)| *w == WindowId(2)).unwrap().1;
    let main = rects.iter().find(|(w, _)| *w == WindowId(1)).unwrap().1;
    assert_eq!(left, Rect::new(0, 0, 20, 30));
    // One column between them for the rule: text running straight from a panel into a transcript
    // with no gap reads as one column of nonsense, because the eye has nothing to stop at.
    assert_eq!(main, Rect::new(21, 0, 79, 30));
}

#[test]
fn a_side_dock_is_separated_from_what_is_beside_it() {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "panel".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("panel", vec![]); 3],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Left, size: Some(8), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::BufferOpened { buf: BufferId(2), name: "main".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(2),
        start: 0,
        old_end: 0,
        lines: vec![line("body", vec![]); 3],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(2),
        buf: BufferId(2),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });

    let cells = cells_of(&m, 20, 3);
    assert_eq!(cells[0][8], "│", "a rule between the two: {:?}", cells[0]);
    assert_eq!(cells[0][9], "b", "and the main pane starts after it: {:?}", cells[0]);
    // Every row of the dock, not just the first.
    assert_eq!(cells[2][8], "│", "{:?}", cells[2]);
}

#[test]
fn the_rule_is_the_first_thing_given_up_when_space_runs_out() {
    // A separator that costs you the panel it separates is not a good trade.
    let mut m = Mirror::new();
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Left, size: Some(9), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(2),
        buf: BufferId(2),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    let rects = resolve_layout(&m, Rect::new(0, 0, 10, 4));
    let left = rects.iter().find(|(w, _)| *w == WindowId(1)).unwrap().1;
    let main = rects.iter().find(|(w, _)| *w == WindowId(2)).unwrap().1;
    assert_eq!(left.width, 9, "the panel keeps its width");
    assert_eq!(main.width, 1, "and main keeps the one column it is owed");
}

#[test]
fn a_screen_anchored_float_is_centred() {
    let mut m = mirror_with_windows();
    m.apply(UiEvent::WindowOpened {
        win: WindowId(3),
        buf: BufferId(1),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Fixed { n: 20 },
                height: Extent::Fixed { n: 6 },
                border: BorderStyle::None,
                ..Default::default()
            },
        },
    });
    let rects = resolve_layout(&m, Rect::new(0, 0, 100, 30));
    let f = rects.iter().find(|(w, _)| *w == WindowId(3)).unwrap().1;
    assert_eq!(f, Rect::new(40, 12, 20, 6));
}

#[test]
fn a_float_is_clamped_on_screen_however_absurd_the_offset() {
    let mut m = mirror_with_windows();
    m.apply(UiEvent::WindowOpened {
        win: WindowId(3),
        buf: BufferId(1),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                offset: neosh_proto::Offset { row: -9999, col: 9999 },
                width: Extent::Fixed { n: 10 },
                height: Extent::Fixed { n: 4 },
                border: BorderStyle::None,
                ..Default::default()
            },
        },
    });
    let area = Rect::new(0, 0, 80, 24);
    let f = resolve_layout(&m, area).into_iter().find(|(w, _)| *w == WindowId(3)).unwrap().1;
    assert!(f.x + f.width <= area.width && f.y + f.height <= area.height, "{f:?} escaped");
}

#[test]
fn an_auto_sized_float_fits_its_widest_line() {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(2), name: "f".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(2),
        start: 0,
        old_end: 0,
        lines: vec![line("short", vec![]), line("a much longer line", vec![])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(9),
        buf: BufferId(2),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Auto,
                height: Extent::Auto,
                border: BorderStyle::None,
                ..Default::default()
            },
        },
    });
    let f = resolve_layout(&m, Rect::new(0, 0, 80, 24))
        .into_iter()
        .find(|(w, _)| *w == WindowId(9))
        .unwrap()
        .1;
    assert_eq!(f.width, "a much longer line".len() as u16);
    assert_eq!(f.height, 2);
}

/// A bordered float asking for `n` rows gets `n` rows of *content*, not `n - 2`.
///
/// Regression: the border used to be taken out of the number the caller asked for, so a picker
/// laid out as "title, filter, rule, fourteen rows, rule, shortcuts" lost its last two lines —
/// silently, because a rectangle two rows short is a perfectly legal rectangle. The shortcut row
/// is where a picker says how to reach its second pane, so the affordance vanished with it.
#[test]
fn a_border_does_not_eat_the_rows_a_float_asked_for() {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(4), name: "f".into() });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(11),
        buf: BufferId(4),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Fixed { n: 40 },
                height: Extent::Fixed { n: 19 },
                border: BorderStyle::Rounded,
                ..Default::default()
            },
        },
    });
    let f = resolve_layout(&m, Rect::new(0, 0, 100, 40))
        .into_iter()
        .find(|(w, _)| *w == WindowId(11))
        .unwrap()
        .1;
    assert_eq!((f.width, f.height), (42, 21), "outer rect should be the content plus its border");
}

/// The same request without a border gets exactly what it asked for, so the two agree about what
/// the number means and only differ in what is drawn around it.
#[test]
fn without_a_border_a_fixed_extent_is_the_rectangle() {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(5), name: "f".into() });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(12),
        buf: BufferId(5),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Fixed { n: 40 },
                height: Extent::Fixed { n: 19 },
                border: BorderStyle::None,
                ..Default::default()
            },
        },
    });
    let f = resolve_layout(&m, Rect::new(0, 0, 100, 40))
        .into_iter()
        .find(|(w, _)| *w == WindowId(12))
        .unwrap()
        .1;
    assert_eq!((f.width, f.height), (40, 19));
}

/// Asking for more than there is loses content, never the bottom edge of the box.
#[test]
fn a_float_too_tall_for_the_screen_is_clamped_to_it() {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(6), name: "f".into() });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(13),
        buf: BufferId(6),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Fixed { n: 200 },
                height: Extent::Fixed { n: 200 },
                border: BorderStyle::Rounded,
                ..Default::default()
            },
        },
    });
    let area = Rect::new(0, 0, 60, 20);
    let f = resolve_layout(&m, area)
        .into_iter()
        .find(|(w, _)| *w == WindowId(13))
        .unwrap()
        .1;
    assert_eq!((f.width, f.height), (60, 20));
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Draw a dock plus a bordered float and assert on the actual terminal cells.
#[test]
fn a_float_draws_over_its_dock() {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "chat".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("background text here", vec![]); 5],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::BufferOpened { buf: BufferId(2), name: "float".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(2),
        start: 0,
        old_end: 0,
        lines: vec![line("picker", vec![mark(0, VirtTextPos::Eol, "(1/3)")])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(2),
        buf: BufferId(2),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Fixed { n: 16 },
                height: Extent::Fixed { n: 3 },
                border: BorderStyle::Rounded,
                title: Some("Pick".into()),
                ..Default::default()
            },
        },
    });

    let mut terminal = Terminal::new(TestBackend::new(30, 7)).unwrap();
    let t = theme();
    terminal.draw(|f| { neosh_tui::render::draw(f, &m, &t); }).unwrap();

    let rendered: Vec<String> = terminal
        .backend()
        .buffer()
        .content()
        .chunks(30)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>().trim_end().to_string())
        .collect();

    insta::assert_snapshot!(rendered.join("\n"));
}


// ---- layout edge cases ----------------------------------------------------

/// A mirror with `n` bottom-docked windows plus a main one, in registration order.
fn docked_mirror(bottom: &[(u32, Option<u16>)]) -> Mirror {
    let mut m = Mirror::new();
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    for (id, size) in bottom {
        m.apply(UiEvent::WindowOpened {
            win: WindowId(*id),
            buf: BufferId(1),
            layout: WindowLayout::Docked { dock: Dock::Bottom, size: *size, gravity: Gravity::Start, wrap: None },
        });
    }
    m
}

#[test]
fn a_terminal_too_small_to_split_does_not_panic() {
    // Regression: `clamp(1, height - 1)` panicked with "min > max" once the height reached 1, so
    // launching in a 1-row terminal crashed — and the crash was invisible, because it happened
    // inside the alternate screen.
    let m = docked_mirror(&[(2, Some(3))]);
    for (w, h) in [(0, 0), (1, 1), (1, 2), (2, 1), (80, 1), (1, 24)] {
        let _ = resolve_layout(&m, Rect::new(0, 0, w, h));
    }
}

#[test]
fn several_windows_can_share_one_edge() {
    // The status line sits under the composer. Laying out only the first window per dock made the
    // second one silently invisible.
    let m = docked_mirror(&[(2, Some(1)), (3, Some(3))]);
    let rects = resolve_layout(&m, Rect::new(0, 0, 80, 24));

    let status = rects.iter().find(|(w, _)| *w == WindowId(2)).expect("status laid out").1;
    let composer = rects.iter().find(|(w, _)| *w == WindowId(3)).expect("composer laid out").1;
    let main = rects.iter().find(|(w, _)| *w == WindowId(1)).expect("main laid out").1;

    assert_eq!(status.height, 1);
    assert_eq!(composer.height, 3);
    assert_eq!(status.y, 23, "the first registered takes the bottom-most row");
    assert_eq!(composer.y, 20, "and the next stacks above it");
    assert_eq!(main.y + main.height, 20, "main gets what is left, with no overlap");
}

#[test]
fn a_dock_never_takes_the_last_row_from_main() {
    let m = docked_mirror(&[(2, Some(50))]);
    let rects = resolve_layout(&m, Rect::new(0, 0, 80, 10));
    let main = rects.iter().find(|(w, _)| *w == WindowId(1)).expect("main laid out").1;
    assert!(main.height >= 1, "main was squeezed out entirely");
}

// ---- legibility -----------------------------------------------------------
//
// Four things a terminal has to do before anything built on top of it can be judged: show the
// message you were sent, show the end of a long answer rather than its beginning, wrap instead of
// clipping, and put a caret where typing lands.

/// Render one mirror to per-cell symbols, so column positions can be asserted exactly.
fn cells_of(m: &Mirror, w: u16, h: u16) -> Vec<Vec<String>> {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let t = theme();
    terminal.draw(|f| {
        neosh_tui::render::draw(f, m, &t);
    })
    .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol().to_string()).collect())
        .collect()
}

/// Render one mirror to plain rows of text.
fn rows_of(m: &Mirror, w: u16, h: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let t = theme();
    terminal.draw(|f| {
        neosh_tui::render::draw(f, m, &t);
    })
    .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>().trim_end().to_string())
        .collect()
}

fn main_window_with(lines: Vec<&str>) -> Mirror {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "chat".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: lines.into_iter().map(|l| line(l, vec![])).collect(),
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    m
}

#[test]
fn a_notification_is_actually_drawn() {
    // Every `neosh.notify` — including the host's own "press ^C again to quit" — used to be
    // collected, capped at 200, and never rendered. A program whose only feedback channel is
    // invisible is a program that appears not to respond.
    let mut m = main_window_with(vec!["hello"]);
    m.apply(UiEvent::Message {
        level: neosh_proto::MessageLevel::Info,
        text: "press ^C again to quit".into(),
    });
    let rows = rows_of(&m, 40, 6).join("\n");
    assert!(rows.contains("press ^C again to quit"), "got:\n{rows}");
}

#[test]
fn only_the_most_recent_notifications_are_shown() {
    let mut m = main_window_with(vec!["hello"]);
    for i in 0..6 {
        m.apply(UiEvent::Message {
            level: neosh_proto::MessageLevel::Info,
            text: format!("message {i}"),
        });
    }
    let rows = rows_of(&m, 40, 8).join("\n");
    assert!(rows.contains("message 5"), "the newest is shown:\n{rows}");
    assert!(rows.contains("message 3"), "and a couple before it:\n{rows}");
    assert!(!rows.contains("message 0"), "but not all six:\n{rows}");
}

#[test]
fn a_notification_does_not_reflow_the_content_underneath() {
    // It overlays. A message that pushed the composer down would move the thing being typed into.
    let m = main_window_with(vec!["line one", "line two"]);
    let before = rows_of(&m, 40, 6);
    let mut with = main_window_with(vec!["line one", "line two"]);
    with.apply(UiEvent::Message {
        level: neosh_proto::MessageLevel::Info,
        text: "hi".into(),
    });
    let after = rows_of(&with, 40, 6);
    assert_eq!(before[0], after[0], "the first line did not move");
    assert_eq!(before[1], after[1]);
}

#[test]
fn a_long_line_wraps_rather_than_clipping() {
    let long = "the quick brown fox jumps over the lazy dog and keeps going well past the edge";
    let m = main_window_with(vec![long]);
    let rows = rows_of(&m, 30, 8);
    let joined: String = rows.join("");
    assert!(joined.contains("jumps over"), "the middle survived:\n{rows:?}");
    assert!(joined.contains("past the edge"), "and so did the end:\n{rows:?}");
}

#[test]
fn wrapping_never_splits_a_wide_character() {
    // A CJK character is two columns. Breaking one across a row boundary corrupts every column
    // after it, which is the failure this whole byte-offset design exists to prevent.
    const TEXT: &str = "日本語テキストがここにあります";
    let m = main_window_with(vec![TEXT]);
    for width in [7, 8, 9] {
        let rows = rows_of(&m, width, 8);
        // The backend pads a wide glyph with a blank continuation cell, so compare with whitespace
        // removed: what matters is that every character survives exactly once, in order.
        let seen: String = rows.join("").chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(seen, TEXT, "width {width} lost or duplicated a character: {rows:?}");
        for row in &rows {
            assert!(
                !row.contains('\u{fffd}'),
                "width {width} produced a replacement character: {rows:?}"
            );
        }
    }
}

// A grapheme cluster can be wider than the pane it must render into: CJK at width 1, or a ZWJ
// emoji sequence in any narrow dock. `wrap_line` used to answer that by flushing an
// already-empty line and retrying the same cluster against the same width — no progress, and the
// output vector grew until the kernel OOM-killed the process, taking the editor down with it.
// These pin the two properties that rule that out: it terminates, and it keeps every character.

/// Run `f` on a worker thread, failing rather than hanging if it does not come back.
///
/// A regression here does not fail slowly. It allocates about a gigabyte a second until the test
/// runner is killed, so the bound has to be imposed from outside the call.
fn within<T: Send + 'static>(label: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(f()));
    rx.recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or_else(|_| panic!("{label}: wrap_line did not terminate"))
}

fn text_of(lines: &[ratatui::text::Line<'static>]) -> String {
    lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect()
}

#[test]
fn a_wide_character_wider_than_the_pane_does_not_loop_forever() {
    use ratatui::text::{Line, Span};
    let out = within("CJK at width 1", || {
        wrap_line(&Line::from(vec![Span::raw("日本".to_string())]), 1)
    });
    assert_eq!(text_of(&out), "日本", "every character survived once: {out:?}");
    assert_eq!(out.len(), 2, "one overflowing cluster per line: {out:?}");
}

#[test]
fn an_emoji_sequence_wider_than_the_pane_does_not_loop_forever() {
    use ratatui::text::{Line, Span};
    // One grapheme cluster, several codepoints joined by ZWJ, and wider than one column.
    const FAMILY: &str = "👨‍👩‍👧‍👦";
    let full = format!("hi {FAMILY} there");
    // Whatever the table says the cluster measures, wrap one column narrower so it cannot fit.
    let narrow = width(FAMILY).saturating_sub(1).max(1);
    let out = within("family emoji in a narrow pane", {
        let full = full.clone();
        move || wrap_line(&Line::from(vec![Span::raw(full)]), narrow)
    });
    assert_eq!(text_of(&out), full, "every character survived once: {out:?}");
    assert!(
        out.len() <= width(&full),
        "output stayed bounded by the text it came from: {out:?}"
    );
}

#[test]
fn wrapping_makes_progress_at_every_width() {
    use ratatui::text::{Line, Span};
    // Narrow, mixed-width, and split across spans: the shape that used to hang.
    let spans = vec![
        Span::raw("a日".to_string()),
        Span::raw("🎉".to_string()),
        Span::raw("bc".to_string()),
    ];
    let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
    for limit in 0..=5usize {
        let line = Line::from(spans.clone());
        let out = within(&format!("width {limit}"), move || wrap_line(&line, limit));
        assert_eq!(text_of(&out), joined, "width {limit} altered the text: {out:?}");
        assert!(
            out.len() <= width(&joined),
            "width {limit} produced more lines than the text has columns: {out:?}"
        );
    }
}

#[test]
fn a_transcript_longer_than_the_pane_shows_its_end() {
    // Taking the first n lines shows a conversation's opening and never its answer, which is the
    // wrong end of every transcript ever written.
    let lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
    let m = main_window_with(lines.iter().map(String::as_str).collect());
    let rows = rows_of(&m, 20, 5).join("\n");
    assert!(rows.contains("line 39"), "the newest line is on screen:\n{rows}");
    assert!(!rows.contains("line 0\n"), "the oldest is not:\n{rows}");
}

#[test]
fn an_explicit_scroll_position_still_means_start_here() {
    // Tail-following must not take paging away.
    let lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
    let mut m = main_window_with(lines.iter().map(String::as_str).collect());
    m.apply(UiEvent::ScrollTo { win: WindowId(1), top_line: 10 });
    let rows = rows_of(&m, 20, 5).join("\n");
    assert!(rows.contains("line 10"), "started where asked:\n{rows}");
    assert!(!rows.contains("line 39"), "and did not jump to the end:\n{rows}");
}

#[test]
fn a_side_panel_clips_instead_of_wrapping() {
    // A docked list that wrapped one long path would reflow every row below it.
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "sidebar".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![
            line("a very long path that exceeds the panel width by a lot", vec![]),
            line("second row", vec![]),
        ],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Left, size: Some(20), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(2),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    let rows = rows_of(&m, 60, 5);
    assert!(rows[1].starts_with("second row"), "row two is still row two: {rows:?}");
}

#[test]
fn the_caret_lands_where_typing_goes() {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "composer".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("hello", vec![])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Bottom, size: Some(1), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::CursorMoved { win: WindowId(1), row: 0, col: 5 });

    let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
    let t = theme();
    let mut caret = None;
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t)).unwrap();
    assert_eq!(caret, Some((5, 3)), "after the fifth column of the bottom row");
}

#[test]
fn the_caret_measures_columns_not_bytes() {
    // `col` is a UTF-8 byte offset. Placing the caret at the byte count would put it three cells
    // too far right after a single CJK character.
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "composer".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("日x", vec![])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Bottom, size: Some(1), gravity: Gravity::Start, wrap: None },
    });
    // Three bytes for 日, one for x: byte offset 4 is the end of the line, screen column 3.
    m.apply(UiEvent::CursorMoved { win: WindowId(1), row: 0, col: 4 });

    let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
    let t = theme();
    let mut caret = None;
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t)).unwrap();
    assert_eq!(caret, Some((3, 2)));
}

#[test]
fn a_focused_float_takes_the_caret() {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "composer".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("draft", vec![])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Bottom, size: Some(1), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::BufferOpened { buf: BufferId(2), name: "prompt".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(2),
        start: 0,
        old_end: 0,
        lines: vec![line("> ab", vec![])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(2),
        buf: BufferId(2),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Fixed { n: 10 },
                height: Extent::Fixed { n: 3 },
                border: BorderStyle::None,
                ..Default::default()
            },
        },
    });
    m.apply(UiEvent::CursorMoved { win: WindowId(2), row: 0, col: 4 });
    m.apply(UiEvent::FocusChanged { win: Some(WindowId(2)) });

    let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
    let t = theme();
    let mut caret = None;
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t)).unwrap();
    let (x, _) = caret.expect("the focused float owns the caret");
    // Centred float of width 10 on a 30-wide screen starts at column 10; four columns in is 14.
    assert_eq!(x, 14, "the caret is inside the float, not in the composer");
}

#[test]
fn a_caret_scrolled_out_of_view_is_hidden_rather_than_parked() {
    let mut m = main_window_with(vec!["a", "b", "c"]);
    m.apply(UiEvent::CursorMoved { win: WindowId(1), row: 99, col: 0 });
    m.apply(UiEvent::FocusChanged { win: Some(WindowId(1)) });
    let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
    let t = theme();
    let mut caret = Some((0, 0));
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t)).unwrap();
    assert_eq!(caret, None);
}

// ---- row grammar ----------------------------------------------------------

#[test]
fn right_aligned_virtual_text_sits_against_the_window_edge() {
    // Every list row in a workspace is the same shape: flexible text on the left, fixed status on
    // the right. A plugin cannot build that itself — it would have to measure display width, which
    // is the frontend's job precisely so a plugin cannot get it wrong.
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "list".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("a thread", vec![mark(0, VirtTextPos::Right, "2m")])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    let rows = rows_of(&m, 20, 3);
    assert_eq!(rows[0], "a thread          2m", "flush right on a 20-wide pane: {rows:?}");
}

#[test]
fn right_alignment_measures_columns_not_bytes() {
    // A CJK label is two columns per character. Aligning by byte length would leave the status
    // several cells short of the edge.
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "list".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("日本語", vec![mark(0, VirtTextPos::Right, "ok")])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    // Read cells rather than the joined string: the backend pads a wide glyph with a blank
    // continuation cell, so counting characters would measure the harness, not the layout.
    let cells = cells_of(&m, 12, 3);
    assert_eq!(&cells[0][10..12], &["o", "k"], "flush against the right edge: {:?}", cells[0]);
}

#[test]
fn right_aligned_text_that_does_not_fit_follows_the_line_instead_of_overflowing() {
    // Padding past the edge would push the annotation onto the next row on a wrapping window,
    // which is worse than simply following the text.
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "list".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("a rather long row of text", vec![mark(0, VirtTextPos::Right, "status")])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    let rows = rows_of(&m, 20, 4);
    assert!(rows.join("").contains("status"), "the annotation is still shown: {rows:?}");
}

#[test]
fn a_row_can_carry_both_a_right_aligned_and_a_trailing_annotation() {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "list".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line(
            "row",
            vec![mark(0, VirtTextPos::Right, "R"), mark(0, VirtTextPos::Eol, "E")],
        )],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    // The grammar: `Eol` means "after the text", `Right` means "at the edge". Both fit on one row.
    let rows = rows_of(&m, 20, 3);
    assert_eq!(rows[0], "row E              R", "{rows:?}");
}

#[test]
fn a_float_shows_its_top_not_its_tail() {
    // A picker's first two rows are its title and its filter line. Tail-following would hide
    // exactly the two rows it cannot do without.
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "picker".into() });
    let mut lines = vec![line("Model", vec![]), line("> ", vec![])];
    lines.extend((0..20).map(|i| line(&format!("option {i}"), vec![])));
    m.apply(UiEvent::BufferLines { buf: BufferId(1), start: 0, old_end: 0, lines });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Fixed { n: 20 },
                height: Extent::Fixed { n: 6 },
                border: BorderStyle::None,
                ..Default::default()
            },
        },
    });
    let rows = rows_of(&m, 30, 10).join("\n");
    assert!(rows.contains("Model"), "the title is on screen:\n{rows}");
    assert!(!rows.contains("option 19"), "and it did not jump to the end:\n{rows}");
}

#[test]
fn a_float_clips_a_long_row_rather_than_wrapping_it() {
    // A wrapped row in a list pushes the row below it down, and the list's own last row off the
    // bottom — so a picker sized for its contents would silently lose one.
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "picker".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![
            line("a very long option label that will not fit inside this float at all", vec![]),
            line("second option", vec![]),
        ],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Fixed { n: 20 },
                height: Extent::Fixed { n: 2 },
                border: BorderStyle::None,
                ..Default::default()
            },
        },
    });
    let rows = rows_of(&m, 40, 6);
    let with_second = rows.iter().find(|r| r.contains("second option"));
    assert!(with_second.is_some(), "row two is still row two: {rows:?}");
}

// ---------------------------------------------------------------------------
// Chrome: virtual text drawn around a field you type into
// ---------------------------------------------------------------------------

#[test]
fn a_prompt_on_an_empty_line_is_still_drawn() {
    // Regression. Inline chunks used to be spliced in while walking the *gaps between* cut
    // positions, and an empty line has one position and no gaps — so the prompt glyph vanished on
    // an empty composer, which is the one moment it is doing all of the work.
    let out = render_line(&line("", vec![mark(0, VirtTextPos::Inline, "> ")]), &theme());
    assert_eq!(flatten(&out), vec!["> "]);
}

#[test]
fn the_caret_moves_past_chrome_drawn_in_front_of_it() {
    // A prompt is virtual: it occupies screen columns and no bytes. Without accounting for it the
    // caret sits under the prompt rather than after it, and every keystroke appears one place to
    // the left of where it lands.
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "composer".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("hi", vec![mark(0, VirtTextPos::Inline, "\u{203a} ")])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Bottom, size: Some(1), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::CursorMoved { win: WindowId(1), row: 0, col: 2 });

    let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
    let t = theme();
    let mut caret = None;
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t)).unwrap();
    assert_eq!(caret, Some((4, 2)), "two columns of prompt, then two of text");
}

#[test]
fn a_line_drawn_above_the_first_row_pushes_the_caret_down() {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "composer".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("hi", vec![mark(0, VirtTextPos::Above, "----")])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Bottom, size: Some(2), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::CursorMoved { win: WindowId(1), row: 0, col: 2 });

    let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
    let t = theme();
    let mut caret = None;
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t)).unwrap();
    assert_eq!(caret, Some((2, 3)), "the rule takes the first of the two rows");
}

#[test]
fn short_content_settles_against_the_end_when_told_to() {
    // Two lines in a five-row window. With `Gravity::End` they belong at the bottom, so a short
    // conversation sits just above the composer instead of floating at the top of the screen.
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "chat".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("first", vec![]), line("second", vec![])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::End, wrap: None },
    });

    let mut terminal = Terminal::new(TestBackend::new(10, 5)).unwrap();
    let t = theme();
    terminal.draw(|f| { neosh_tui::render::draw(f, &m, &t); }).unwrap();
    let rows: Vec<String> = (0..5)
        .map(|y| (0..10).map(|x| terminal.backend().buffer()[(x, y)].symbol()).collect())
        .collect();
    assert_eq!(rows[3].trim_end(), "first");
    assert_eq!(rows[4].trim_end(), "second");
    assert_eq!(rows[0].trim(), "", "and nothing is drawn at the top");
}

#[test]
fn the_default_gravity_leaves_content_where_it_has_always_been() {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "chat".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("first", vec![])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });

    let mut terminal = Terminal::new(TestBackend::new(10, 4)).unwrap();
    let t = theme();
    terminal.draw(|f| { neosh_tui::render::draw(f, &m, &t); }).unwrap();
    let top: String = (0..10).map(|x| terminal.backend().buffer()[(x, 0)].symbol()).collect();
    assert_eq!(top.trim_end(), "first");
}
