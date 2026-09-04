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

#[test]
fn at_equal_priority_the_narrower_highlight_wins() {
    // A fenced block is code and this word in it is a keyword: both are true, both are set at the
    // priority a renderer uses for ordinary colouring, and the specific one is the one worth
    // saying. Resolving it by which was set last makes the answer depend on a caller's loop order.
    let span = |col, end, group: &str| ExtmarkRender {
        ns: NamespaceId(1),
        id: ExtmarkId(1),
        col,
        opts: ExtmarkOpts {
            hl_group: Some(group.into()),
            end_col: Some(end),
            ..Default::default()
        },
    };
    let mut t = theme();
    t.set("Narrow", HighlightDef::Spec {
        spec: HighlightSpec { fg: Some(neosh_proto::Color::Indexed { i: 9 }), ..Default::default() },
    });
    let narrow = neosh_proto::Color::Indexed { i: 9 };
    // Whichever order they arrive in.
    for marks in [
        vec![span(0, 6, "V"), span(2, 4, "Narrow")],
        vec![span(2, 4, "Narrow"), span(0, 6, "V")],
    ] {
        let out = render_line(&line("abcdef", marks), &t);
        let at = |c: char| {
            out[0].spans.iter().find(|s| s.content.contains(c)).unwrap().style.fg
        };
        assert_eq!(at('c'), Some(ratatui::style::Color::Indexed(9)), "{narrow:?}");
        assert_ne!(at('a'), Some(ratatui::style::Color::Indexed(9)), "outside it, the broad one");
    }
}

#[test]
fn a_row_background_sits_under_every_highlight_on_the_row() {
    // The rule the whole diff redesign rests on: a line a diff added is green *and* the code on it
    // is syntax-coloured. If the band replaced the token colours the diff would be back to being
    // two colours of text, and if a token colour replaced the band the line would be striped.
    let band = ExtmarkRender {
        ns: NamespaceId(1),
        id: ExtmarkId(1),
        col: 0,
        opts: ExtmarkOpts { line_hl_group: Some("Band".into()), ..Default::default() },
    };
    let word = ExtmarkRender {
        ns: NamespaceId(1),
        id: ExtmarkId(2),
        col: 2,
        opts: ExtmarkOpts {
            hl_group: Some("Word".into()),
            end_col: Some(5),
            ..Default::default()
        },
    };
    let mut t = theme();
    t.set("Band", HighlightDef::Spec {
        spec: HighlightSpec {
            bg: Some(neosh_proto::Color::Indexed { i: 22 }),
            ..Default::default()
        },
    });
    t.set("Word", HighlightDef::Spec {
        spec: HighlightSpec {
            fg: Some(neosh_proto::Color::Indexed { i: 9 }),
            ..Default::default()
        },
    });
    let out = render_line(&line("abcdefg", vec![band, word]), &t);
    let bg = ratatui::style::Color::Indexed(22);
    for s in &out[0].spans {
        assert_eq!(s.style.bg, Some(bg), "every span keeps the band: {:?}", s.content);
    }
    let coloured = out[0].spans.iter().find(|s| s.content.contains('c')).unwrap();
    assert_eq!(coloured.style.fg, Some(ratatui::style::Color::Indexed(9)), "and its own colour");
}

#[test]
fn a_banded_row_carries_its_background_across_a_wrap() {
    // A band that stops halfway down a wrapped line reads as two lines, one of which changed.
    let mut t = theme();
    t.set("Band", HighlightDef::Spec {
        spec: HighlightSpec {
            bg: Some(neosh_proto::Color::Indexed { i: 22 }),
            ..Default::default()
        },
    });
    let band = ExtmarkRender {
        ns: NamespaceId(1),
        id: ExtmarkId(1),
        col: 0,
        opts: ExtmarkOpts { line_hl_group: Some("Band".into()), ..Default::default() },
    };
    let out = render_line(&line("abcdefghij", vec![band]), &t);
    let wrapped = neosh_tui::render::wrap_line(&out[0], 4);
    assert!(wrapped.len() > 1, "it wrapped");
    let bg = ratatui::style::Color::Indexed(22);
    for l in &wrapped {
        assert_eq!(l.style.bg, Some(bg), "{l:?}");
    }
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(2),
        buf: BufferId(1),
        layout: WindowLayout::Docked { pane: None, dock: Dock::Left, size: Some(20), gravity: Gravity::Start, wrap: None },
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Left, size: Some(8), gravity: Gravity::Start, wrap: None },
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Left, size: Some(9), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(2),
        buf: BufferId(2),
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
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

/// A completion is placed by naming the dock it completes, and nothing else.
///
/// The float's *bottom* edge meets the bottom strip, not its top-left corner. Anchoring the corner
/// would make every caller subtract its own height first — and the day one of them is a row out,
/// the menu sits on top of the field it is a menu for, which is the one place it must never be.
#[test]
fn a_dock_anchored_float_sits_flush_against_the_dock_whatever_its_height() {
    let mut m = mirror_with_windows();
    m.apply(UiEvent::WindowOpened {
        win: WindowId(3),
        buf: BufferId(1),
        layout: WindowLayout::Docked { pane: None,
            dock: Dock::Bottom,
            size: Some(4),
            gravity: Gravity::Start,
            wrap: None,
        },
    });
    let float = |n: u16| WindowLayout::Float {
        config: FloatConfig {
            anchor: Anchor::Dock { dock: Dock::Bottom },
            width: Extent::Fixed { n: 30 },
            height: Extent::Fixed { n },
            border: BorderStyle::None,
            ..Default::default()
        },
    };
    m.apply(UiEvent::WindowOpened { win: WindowId(4), buf: BufferId(1), layout: float(6) });
    let rects = resolve_layout(&m, Rect::new(0, 0, 100, 30));
    let bottom = rects.iter().find(|(w, _)| *w == WindowId(3)).unwrap().1;
    let f = rects.iter().find(|(w, _)| *w == WindowId(4)).unwrap().1;
    assert_eq!(f.y + f.height, bottom.y, "the float's foot meets the strip's head");
    let main = rects.iter().find(|(w, _)| *w == WindowId(1)).unwrap().1;
    assert_eq!(f.x, main.x, "and its left edge lines up with what it is completing");

    // Same anchor, taller list: it grows upwards, and the edge that touches stays touching.
    m.apply(UiEvent::WindowOpened { win: WindowId(5), buf: BufferId(1), layout: float(12) });
    let rects = resolve_layout(&m, Rect::new(0, 0, 100, 30));
    let tall = rects.iter().find(|(w, _)| *w == WindowId(5)).unwrap().1;
    assert_eq!(tall.y + tall.height, bottom.y, "however many rows it turned out to need");
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    for (id, size) in bottom {
        m.apply(UiEvent::WindowOpened {
            win: WindowId(*id),
            buf: BufferId(1),
            layout: WindowLayout::Docked { pane: None, dock: Dock::Bottom, size: *size, gravity: Gravity::Start, wrap: None },
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    m
}

#[test]
fn a_notification_is_actually_drawn() {
    // Every `neosh.notify` — including the host's own "press ^C again to quit" — used to be
    // collected, capped at 200, and never rendered. A program whose only feedback channel is
    // invisible is a program that appears not to respond.
    let mut m = main_window_with(vec!["hello"]);
    m.apply(notice("press ^C again to quit", neosh_proto::NoticeKind::Reply));
    let rows = rows_of(&m, 40, 6).join("\n");
    assert!(rows.contains("press ^C again to quit"), "got:\n{rows}");
}

/// One message, whatever kind it is, with the clock stamped on receipt.
fn notice(text: &str, kind: neosh_proto::NoticeKind) -> UiEvent {
    UiEvent::Message {
        level: neosh_proto::MessageLevel::Info,
        text: text.into(),
        kind,
        key: None,
    }
}

#[test]
fn only_the_most_recent_notifications_are_shown() {
    // News, which is the kind that stacks. A reply replaces the one before it — see below — so
    // six of those would be one row and this would be asserting the wrong thing.
    let mut m = main_window_with(vec!["hello"]);
    for i in 0..6 {
        m.apply(notice(&format!("message {i}"), neosh_proto::NoticeKind::Alert));
    }
    let rows = rows_of(&m, 40, 8).join("\n");
    assert!(rows.contains("message 5"), "the newest is shown:\n{rows}");
    assert!(rows.contains("message 3"), "and a couple before it:\n{rows}");
    assert!(!rows.contains("message 0"), "but not all six:\n{rows}");
}

/// Feedback for a key you pressed is about *that* key, so the previous one is about a key you have
/// already moved on from.
#[test]
fn a_reply_does_not_stack_on_the_reply_before_it() {
    let mut m = main_window_with(vec!["hello"]);
    m.apply(notice("copied /a", neosh_proto::NoticeKind::Reply));
    m.apply(notice("copied /b", neosh_proto::NoticeKind::Reply));
    let rows = rows_of(&m, 40, 8).join("\n");
    assert!(rows.contains("copied /b"), "the answer to the key you just pressed:\n{rows}");
    assert!(!rows.contains("copied /a"), "and not the one before it:\n{rows}");
}

/// Progress draws above the answers, because it is what the program is *doing* and it will change
/// on its own. It has no timer: it lives until something takes it away.
#[test]
fn progress_is_drawn_and_is_taken_away_by_name() {
    let mut m = main_window_with(vec!["hello"]);
    m.apply(UiEvent::Message {
        level: neosh_proto::MessageLevel::Info,
        text: "pulling\u{2026}".into(),
        kind: neosh_proto::NoticeKind::Progress,
        key: Some("git.pull".into()),
    });
    let rows = rows_of(&m, 40, 8).join("\n");
    assert!(rows.contains("pulling"), "got:\n{rows}");

    m.apply(UiEvent::ProgressDone { key: "git.pull".into() });
    let rows = rows_of(&m, 40, 8).join("\n");
    assert!(!rows.contains("pulling"), "gone when it finished:\n{rows}");
}

#[test]
fn a_notification_does_not_reflow_the_content_underneath() {
    // It overlays. A message that pushed the composer down would move the thing being typed into.
    let m = main_window_with(vec!["line one", "line two"]);
    let before = rows_of(&m, 40, 6);
    let mut with = main_window_with(vec!["line one", "line two"]);
    with.apply(notice("hi", neosh_proto::NoticeKind::Reply));
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
    m.apply(UiEvent::ScrollTo { win: WindowId(1), top_line: Some(10) });
    let rows = rows_of(&m, 20, 5).join("\n");
    assert!(rows.contains("line 10"), "started where asked:\n{rows}");
    assert!(!rows.contains("line 39"), "and did not jump to the end:\n{rows}");
}

/// Row zero is a place, and asking for it is not asking to follow the tail.
///
/// The two shared an encoding, so the top of a transcript was the one row in it nothing could
/// reach: `^U` up to the first line asked for `0`, which was read as "unscrolled", and the window
/// drew the last screenful instead. Reading upwards ended at the bottom.
#[test]
fn scrolling_to_the_first_row_shows_the_first_row() {
    let lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
    let mut m = main_window_with(lines.iter().map(String::as_str).collect());
    m.apply(UiEvent::ScrollTo { win: WindowId(1), top_line: Some(0) });
    let rows = rows_of(&m, 20, 5).join("\n");
    assert!(rows.contains("line 0"), "the top of the transcript:\n{rows}");
    assert!(!rows.contains("line 39"), "and not the bottom of it:\n{rows}");
}

/// And giving the position back is still how you follow the newest.
#[test]
fn an_unscrolled_transcript_is_back_at_the_end() {
    let lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
    let mut m = main_window_with(lines.iter().map(String::as_str).collect());
    m.apply(UiEvent::ScrollTo { win: WindowId(1), top_line: Some(0) });
    m.apply(UiEvent::ScrollTo { win: WindowId(1), top_line: None });
    let rows = rows_of(&m, 20, 5).join("\n");
    assert!(rows.contains("line 39"), "the newest line is on screen:\n{rows}");
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Left, size: Some(20), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(2),
        buf: BufferId(1),
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Bottom, size: Some(1), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::CursorMoved { win: WindowId(1), row: 0, col: 5 });

    let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
    let t = theme();
    let mut caret = None;
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t).caret).unwrap();
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Bottom, size: Some(1), gravity: Gravity::Start, wrap: None },
    });
    // Three bytes for 日, one for x: byte offset 4 is the end of the line, screen column 3.
    m.apply(UiEvent::CursorMoved { win: WindowId(1), row: 0, col: 4 });

    let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
    let t = theme();
    let mut caret = None;
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t).caret).unwrap();
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Bottom, size: Some(1), gravity: Gravity::Start, wrap: None },
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
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t).caret).unwrap();
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
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t).caret).unwrap();
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Bottom, size: Some(1), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::CursorMoved { win: WindowId(1), row: 0, col: 2 });

    let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
    let t = theme();
    let mut caret = None;
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t).caret).unwrap();
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Bottom, size: Some(2), gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::CursorMoved { win: WindowId(1), row: 0, col: 2 });

    let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
    let t = theme();
    let mut caret = None;
    terminal.draw(|f| caret = neosh_tui::render::draw(f, &m, &t).caret).unwrap();
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::End, wrap: None },
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
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });

    let mut terminal = Terminal::new(TestBackend::new(10, 4)).unwrap();
    let t = theme();
    terminal.draw(|f| { neosh_tui::render::draw(f, &m, &t); }).unwrap();
    let top: String = (0..10).map(|x| terminal.backend().buffer()[(x, 0)].symbol()).collect();
    assert_eq!(top.trim_end(), "first");
}

// ---------------------------------------------------------------------------
// The caret on a window that wraps
// ---------------------------------------------------------------------------

/// A transcript of paragraphs, and a caret that has to be on the row it is on.
///
/// The bug this is here for: the caret worked its screen row out as `cursor_row - top_line`, in
/// buffer rows, while the draw counted wrapped rows. The two agree until the first line long enough
/// to wrap and then diverge by one row per wrap — so a few paragraphs into a transcript the caret
/// was drawn above where the character it was on had been drawn, and a few more in it was outside
/// the window and therefore hidden. Which reads, exactly, as a mode with no cursor in it.
fn wrapped_transcript(cursor: (u32, u32)) -> Mirror {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "[chat]".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        // Three rows of twenty columns each, in a window ten wide: six screen rows.
        lines: vec![
            line("aaaaaaaaaabbbbbbbbbb", vec![]),
            line("ccccccccccdddddddddd", vec![]),
            line("eeeeeeeeeeffffffffff", vec![]),
        ],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked { pane: None, dock: Dock::Main, size: None, gravity: Gravity::Start, wrap: None },
    });
    m.apply(UiEvent::ScrollTo { win: WindowId(1), top_line: Some(0) });
    m.apply(UiEvent::CursorMoved { win: WindowId(1), row: cursor.0, col: cursor.1 });
    m.apply(UiEvent::FocusChanged { win: Some(WindowId(1)) });
    m
}

fn drawn(m: &Mirror, w: u16, h: u16) -> neosh_tui::render::Drawn {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let t = theme();
    let mut out = neosh_tui::render::Drawn::default();
    terminal.draw(|f| out = neosh_tui::render::draw(f, m, &t)).unwrap();
    out
}

#[test]
fn the_caret_counts_the_rows_the_screen_has_rather_than_the_ones_the_buffer_has() {
    // Row 2 column 0 is the fifth *screen* row: two buffer rows of two segments each above it.
    let m = wrapped_transcript((2, 0));
    assert_eq!(drawn(&m, 10, 6).caret, Some((0, 4)), "row four, not row two");
    // And halfway along it is the sixth, because the caret wraps with the text it is on.
    let m = wrapped_transcript((2, 15));
    assert_eq!(drawn(&m, 10, 6).caret, Some((5, 5)));
}

/// The cursor is never off the window it is in.
///
/// `top_line` counts buffer rows, so a scroll that put the cursor on screen by that arithmetic can
/// still leave it below the bottom edge once anything wraps — and a caret the renderer cannot place
/// is one it turns off. The frontend is the only thing that knows how tall a row turned out to be,
/// so keeping the cursor visible is its job and not the core's.
#[test]
fn a_wrapping_window_bends_its_scroll_to_keep_the_cursor_on_screen() {
    // Three rows asked for from the top, in a window three tall: only the first one and a half fit,
    // and the cursor is on the last.
    let m = wrapped_transcript((2, 0));
    let d = drawn(&m, 10, 3);
    assert!(d.caret.is_some(), "the caret is somewhere on the screen, not hidden");
    assert_eq!(d.caret, Some((0, 2)), "on the bottom row of the window");
    // And it says what it actually drew, which is not what it was asked for. Everything that pages
    // by a screenful reads this: `top_line` of zero here would mean "half a screen" was six rows.
    assert_eq!(d.tops, vec![(WindowId(1), (1, 2))], "from row one, and two buffer rows of it");
}

/// A caret that would land outside is hidden rather than parked in a corner — but only when the
/// window genuinely cannot show it, which for a cursor is never.
#[test]
fn a_window_with_no_cursor_in_it_still_reports_what_it_drew() {
    let mut m = wrapped_transcript((0, 0));
    m.apply(UiEvent::FocusChanged { win: None });
    let d = drawn(&m, 10, 6);
    assert_eq!(d.tops, vec![(WindowId(1), (0, 3))], "all three rows, from the first");
}

/// The block caret is a cell the frontend paints, not something the terminal is trusted to draw.
///
/// A terminal's own caret is a thin bar in most of them, and the one thing this has to do is be
/// findable in a screenful of prose. The terminal is still told where it is and what shape to be —
/// this is what makes it *visible*.
#[test]
fn a_block_caret_is_painted_over_the_character_it_is_on() {
    let mut m = wrapped_transcript((0, 3));
    m.apply(UiEvent::CursorShapeChanged { win: WindowId(1), shape: neosh_proto::CursorShape::Block });
    let mut terminal = Terminal::new(TestBackend::new(10, 6)).unwrap();
    let t = theme();
    let mut out = neosh_tui::render::Drawn::default();
    terminal.draw(|f| out = neosh_tui::render::draw(f, &m, &t)).unwrap();
    let (x, y) = out.caret.expect("a caret");
    let cell = terminal.backend().buffer().cell((x, y)).expect("a cell").clone();
    assert!(
        cell.modifier.contains(ratatui::style::Modifier::REVERSED),
        "the cell under the caret is turned inside out: {cell:?}"
    );
    assert_eq!(cell.symbol(), "a", "and it is still the character that was there");
}

// ---------------------------------------------------------------------------
// Panes
// ---------------------------------------------------------------------------
//
// The main region divided by a tab's pane tree. What these are checking is the arithmetic — that
// the pieces tile the region exactly, that the separators are where the panes are not, and that a
// pane belonging to a tab you are not on is simply absent rather than drawn somewhere plausible.

use neosh_proto::{PaneChild, PaneId, PaneNode, SplitDir, TabId, TabInfo};

fn pane_win(win: u32, pane: u32, dock: Dock, size: Option<u16>) -> UiEvent {
    UiEvent::WindowOpened {
        win: WindowId(win),
        buf: BufferId(1),
        layout: WindowLayout::Docked {
            pane: Some(PaneId(pane)),
            dock,
            size,
            gravity: Gravity::Start,
            wrap: None,
        },
    }
}

fn split(dir: SplitDir, children: Vec<PaneNode>) -> PaneNode {
    PaneNode::Split { dir, children: children.into_iter().map(PaneChild::new).collect() }
}

fn leaf(n: u32) -> PaneNode {
    PaneNode::Leaf { pane: PaneId(n) }
}

/// A mirror with one tab whose root is `root`, and a buffer to put in windows.
fn paned(root: PaneNode) -> Mirror {
    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "buf".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("x", vec![]); 4],
    });
    let active = TabId(1);
    m.apply(UiEvent::PanesChanged {
        tabs: vec![TabInfo { id: active, title: None, root, active_pane: PaneId(1), group: None }],
        active,
    });
    m
}

#[test]
fn a_single_pane_is_the_whole_main_region() {
    let mut m = paned(leaf(1));
    m.apply(pane_win(1, 1, Dock::Main, None));
    let rects = resolve_layout(&m, Rect::new(0, 0, 80, 24));
    assert_eq!(rects.iter().find(|(w, _)| *w == WindowId(1)).unwrap().1, Rect::new(0, 0, 80, 24));
}

/// Side by side, with one column of separator between them, and the two halves plus the rule
/// adding up to exactly the width — no unpainted column down the right-hand edge.
#[test]
fn a_row_splits_the_width_and_leaves_a_column_between() {
    let mut m = paned(split(SplitDir::Row, vec![leaf(1), leaf(2)]));
    m.apply(pane_win(1, 1, Dock::Main, None));
    m.apply(pane_win(2, 2, Dock::Main, None));
    let rects = resolve_layout(&m, Rect::new(0, 0, 81, 24));
    let a = rects.iter().find(|(w, _)| *w == WindowId(1)).unwrap().1;
    let b = rects.iter().find(|(w, _)| *w == WindowId(2)).unwrap().1;
    assert_eq!(a, Rect::new(0, 0, 40, 24));
    assert_eq!(b, Rect::new(41, 0, 40, 24));
    assert_eq!(a.width + 1 + b.width, 81, "the panes and the rule tile the region exactly");
}

#[test]
fn a_column_splits_the_height() {
    let mut m = paned(split(SplitDir::Column, vec![leaf(1), leaf(2)]));
    m.apply(pane_win(1, 1, Dock::Main, None));
    m.apply(pane_win(2, 2, Dock::Main, None));
    let rects = resolve_layout(&m, Rect::new(0, 0, 80, 25));
    let a = rects.iter().find(|(w, _)| *w == WindowId(1)).unwrap().1;
    let b = rects.iter().find(|(w, _)| *w == WindowId(2)).unwrap().1;
    assert_eq!(a, Rect::new(0, 0, 80, 12));
    assert_eq!(b, Rect::new(0, 13, 80, 12));
}

/// Odd widths must not evaporate. Three panes across 80 columns is 78 for content, which does not
/// divide by three evenly — and the leftover has to land somewhere rather than nowhere.
#[test]
fn an_uneven_division_still_tiles_the_region() {
    let mut m = paned(split(SplitDir::Row, vec![leaf(1), leaf(2), leaf(3)]));
    for (w, p) in [(1, 1), (2, 2), (3, 3)] {
        m.apply(pane_win(w, p, Dock::Main, None));
    }
    let rects = resolve_layout(&m, Rect::new(0, 0, 80, 24));
    let mut widths: Vec<u16> = (1..=3)
        .map(|w| rects.iter().find(|(id, _)| *id == WindowId(w)).unwrap().1.width)
        .collect();
    assert_eq!(widths.iter().sum::<u16>() + 2, 80, "content plus two rules is the whole width");
    widths.sort();
    assert!(widths[2] - widths[0] <= 1, "and the remainder is spread, not dumped: {widths:?}");
}

/// The composer moving inside the pane is the whole point of per-pane docks: two chats side by
/// side are two transcripts and two fields, each pair inside its own half.
#[test]
fn a_dock_inside_a_pane_is_measured_against_the_pane() {
    let mut m = paned(split(SplitDir::Row, vec![leaf(1), leaf(2)]));
    m.apply(pane_win(1, 1, Dock::Main, None));
    m.apply(pane_win(2, 1, Dock::Bottom, Some(4)));
    m.apply(pane_win(3, 2, Dock::Main, None));
    let rects = resolve_layout(&m, Rect::new(0, 0, 81, 24));
    let transcript = rects.iter().find(|(w, _)| *w == WindowId(1)).unwrap().1;
    let composer = rects.iter().find(|(w, _)| *w == WindowId(2)).unwrap().1;
    let other = rects.iter().find(|(w, _)| *w == WindowId(3)).unwrap().1;
    assert_eq!(composer, Rect::new(0, 20, 40, 4), "at the foot of its own pane, not the screen");
    assert_eq!(transcript, Rect::new(0, 0, 40, 20));
    assert_eq!(other, Rect::new(41, 0, 40, 24), "and the pane next door is untouched");
}

/// Switching tabs moves no windows. It changes which pane ids have a rectangle, and a window in
/// a pane that is not on screen gets none — which is what makes a tab switch free.
#[test]
fn a_window_in_another_tab_is_not_drawn() {
    let mut m = paned(leaf(1));
    m.apply(pane_win(1, 1, Dock::Main, None));
    m.apply(pane_win(2, 9, Dock::Main, None));
    let rects = resolve_layout(&m, Rect::new(0, 0, 80, 24));
    assert!(rects.iter().any(|(w, _)| *w == WindowId(1)));
    assert!(
        !rects.iter().any(|(w, _)| *w == WindowId(2)),
        "a pane the active tab does not contain has no rectangle at all"
    );
}

/// A screen dock is carved before the panes, so the sidebar is beside the whole split rather than
/// inside the first pane of it.
#[test]
fn screen_docks_are_outside_the_panes() {
    let mut m = paned(split(SplitDir::Row, vec![leaf(1), leaf(2)]));
    m.apply(UiEvent::WindowOpened {
        win: WindowId(9),
        buf: BufferId(1),
        layout: WindowLayout::Docked {
            pane: None,
            dock: Dock::Left,
            size: Some(20),
            gravity: Gravity::Start,
            wrap: None,
        },
    });
    m.apply(pane_win(1, 1, Dock::Main, None));
    m.apply(pane_win(2, 2, Dock::Main, None));
    let rects = resolve_layout(&m, Rect::new(0, 0, 102, 24));
    let sidebar = rects.iter().find(|(w, _)| *w == WindowId(9)).unwrap().1;
    let a = rects.iter().find(|(w, _)| *w == WindowId(1)).unwrap().1;
    let b = rects.iter().find(|(w, _)| *w == WindowId(2)).unwrap().1;
    assert_eq!(sidebar, Rect::new(0, 0, 20, 24));
    assert_eq!(a.x, 21, "the panes start after the sidebar and its rule");
    assert_eq!(b.x + b.width, 102, "and the last one reaches the right edge");
}

/// The tab bar: a one-row strip at the top of the main region, to the right of the sidebar rather
/// than across it.
#[test]
fn a_top_dock_sits_inside_the_main_region() {
    let mut m = paned(leaf(1));
    m.apply(UiEvent::WindowOpened {
        win: WindowId(9),
        buf: BufferId(1),
        layout: WindowLayout::Docked {
            pane: None,
            dock: Dock::Left,
            size: Some(20),
            gravity: Gravity::Start,
            wrap: None,
        },
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(8),
        buf: BufferId(1),
        layout: WindowLayout::Docked {
            pane: None,
            dock: Dock::Top,
            size: Some(1),
            gravity: Gravity::Start,
            wrap: None,
        },
    });
    m.apply(pane_win(1, 1, Dock::Main, None));
    let rects = resolve_layout(&m, Rect::new(0, 0, 102, 24));
    let bar = rects.iter().find(|(w, _)| *w == WindowId(8)).unwrap().1;
    let pane = rects.iter().find(|(w, _)| *w == WindowId(1)).unwrap().1;
    assert_eq!(bar, Rect::new(21, 0, 81, 1), "beside the sidebar, not over it");
    assert_eq!(pane, Rect::new(21, 1, 81, 23), "and the panes start under it");
}

/// A plugin that opens a `Dock::Main` window without naming a pane lands where the person is
/// looking — the same rule as a float that names no view, one level down.
#[test]
fn a_window_that_names_no_pane_lands_in_the_active_one() {
    let mut m = paned(split(SplitDir::Row, vec![leaf(1), leaf(2)]));
    m.apply(UiEvent::PanesChanged {
        tabs: vec![TabInfo {
            id: TabId(1),
            title: None,
            root: split(SplitDir::Row, vec![leaf(1), leaf(2)]),
            active_pane: PaneId(2),
            group: None,
        }],
        active: TabId(1),
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(5),
        buf: BufferId(1),
        layout: WindowLayout::Docked {
            pane: None,
            dock: Dock::Main,
            size: None,
            gravity: Gravity::Start,
            wrap: None,
        },
    });
    let rects = resolve_layout(&m, Rect::new(0, 0, 81, 24));
    assert_eq!(
        rects.iter().find(|(w, _)| *w == WindowId(5)).unwrap().1,
        Rect::new(41, 0, 40, 24),
        "the second pane, which is the active one"
    );
}

/// Nothing may panic or loop at any size, including sizes smaller than the layout needs. A pane
/// that cannot fit gets no rectangle; it never gets a zero-width one.
#[test]
fn a_deep_layout_survives_every_terminal_size() {
    let root = split(SplitDir::Row, vec![
        leaf(1),
        split(SplitDir::Column, vec![leaf(2), split(SplitDir::Row, vec![leaf(3), leaf(4)])]),
        leaf(5),
    ]);
    let mut m = paned(root);
    for p in 1..=5 {
        m.apply(pane_win(p, p, Dock::Main, None));
        m.apply(pane_win(p + 10, p, Dock::Bottom, Some(3)));
    }
    for w in 0..40u16 {
        for h in 0..20u16 {
            let rects = resolve_layout(&m, Rect::new(0, 0, w, h));
            for (win, r) in &rects {
                assert!(
                    r.x + r.width <= w && r.y + r.height <= h,
                    "window {win:?} at {r:?} escapes a {w}x{h} screen"
                );
                assert!(r.width > 0 && r.height > 0, "window {win:?} was given nothing: {r:?}");
            }
        }
    }
}

/// A float is drawn over a surface, not under it.
///
/// A surface belongs to its window, so it is painted in that window's place in the paint order.
/// Drawn in one pass at the end instead, every surface was on top of everything — so a terminal
/// pane filling a rectangle covered any float above it, and the panel telling you how to get *out*
/// of a full-screen program was painted over by the program.
#[test]
fn a_float_draws_over_a_surface() {
    use neosh_proto::{Rect as PRect, SurfaceCell, SurfaceId};

    let mut m = Mirror::new();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "under".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: vec![line("", vec![]); 6],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Docked {
            pane: None,
            dock: Dock::Main,
            size: None,
            gravity: Gravity::Start,
            wrap: None,
        },
    });
    // A surface covering the whole window, as a shell's would.
    m.apply(UiEvent::SurfaceClaimed {
        surface: SurfaceId(1),
        win: WindowId(1),
        rect: PRect { row: 0, col: 0, width: 20, height: 6 },
    });
    let mut cells = Vec::new();
    for row in 0..6u16 {
        for col in 0..20u16 {
            cells.push(SurfaceCell {
                row,
                col,
                grapheme: "#".into(),
                fg: None,
                bg: None,
                attrs: Default::default(),
            });
        }
    }
    m.apply(UiEvent::SurfaceCells { surface: SurfaceId(1), cells });

    // And a float over it, as the prefix panel is.
    m.apply(UiEvent::BufferOpened { buf: BufferId(2), name: "over".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(2),
        start: 0,
        old_end: 0,
        lines: vec![line("PANEL", vec![])],
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(2),
        buf: BufferId(2),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Fixed { n: 5 },
                height: Extent::Fixed { n: 1 },
                border: BorderStyle::None,
                ..Default::default()
            },
        },
    });

    let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
    let t = theme();
    terminal.draw(|f| { neosh_tui::render::draw(f, &m, &t); }).unwrap();
    let screen: String = {
        let buf = terminal.backend().buffer();
        (0..6)
            .flat_map(|y| (0..20).map(move |x| (x, y)))
            .filter_map(|(x, y)| buf.cell((x, y)).map(|c| c.symbol().to_string()))
            .collect()
    };
    assert!(screen.contains("PANEL"), "the float is on top of the surface:\n{screen}");
    assert!(screen.contains('#'), "and the surface is still drawn where the float is not");
}

// ---------------------------------------------------------------------------
// A panel that is showing less than it has
// ---------------------------------------------------------------------------
//
// A float is *asked* for a height and given whatever the screen has, so the rows past the bottom
// edge are content that exists and is drawn nowhere. Before this there was nothing on screen to
// tell that apart from a panel that simply ended there — the two were identical, and the second is
// what everybody read.

/// A float of `lines` rows, `n` of them on screen, scrolled to `top`.
fn overflowing(lines: usize, n: u16, top: Option<u32>) -> Mirror {
    let mut m = Mirror::default();
    m.apply(UiEvent::BufferOpened { buf: BufferId(1), name: "[long]".into() });
    m.apply(UiEvent::BufferLines {
        buf: BufferId(1),
        start: 0,
        old_end: 0,
        lines: (0..lines).map(|i| line(&format!("row {i}"), vec![])).collect(),
    });
    m.apply(UiEvent::WindowOpened {
        win: WindowId(1),
        buf: BufferId(1),
        layout: WindowLayout::Float {
            config: FloatConfig {
                anchor: Anchor::Screen,
                width: Extent::Fixed { n: 10 },
                height: Extent::Fixed { n },
                border: BorderStyle::Rounded,
                ..Default::default()
            },
        },
    });
    if let Some(t) = top {
        m.apply(UiEvent::ScrollTo { win: WindowId(1), top_line: Some(t) });
    }
    m
}

/// The column the bar is drawn in, and which rows of it are thumb.
fn thumb_rows(m: &Mirror, w: u16, h: u16) -> Vec<usize> {
    let cells = cells_of(m, w, h);
    // The float is centred, so its top-right corner is wherever it landed rather than on row zero.
    let x = cells
        .iter()
        .find_map(|row| row.iter().position(|c| c == "╮"))
        .expect("a bordered float");
    cells.iter().enumerate().filter(|(_, row)| row[x] == "┃").map(|(y, _)| y).collect()
}

#[test]
fn a_float_that_fits_has_no_bar_on_it() {
    // Four rows in a panel with room for four. There is nothing to say, and a bar that is always
    // there is a bar nobody reads.
    let m = overflowing(4, 4, None);
    assert!(thumb_rows(&m, 20, 10).is_empty());
}

#[test]
fn a_float_showing_less_than_it_has_says_so_on_its_right_border() {
    // Forty rows in a panel with room for eight: a thumb of one row, at the top, in the border
    // column — not in a column of content, which every panel would then be one narrower for.
    let m = overflowing(40, 8, None);
    let thumb = thumb_rows(&m, 20, 14);
    assert_eq!(thumb.len(), 1, "one row of forty is one row of eight, rounded up");
    let top = *thumb.first().expect("a thumb");
    assert!(top >= 1, "inside the border, not on its corner");

    // And it moves. Scrolled to the end it is against the bottom of the track, which is the whole
    // of what a scrollbar is for: measured off `rendered` alone — which begins at `top_line` — the
    // thumb sat at the top of every panel and *grew* as you scrolled, saying the opposite.
    let end = thumb_rows(&overflowing(40, 8, Some(32)), 20, 14);
    assert!(end[0] > top, "the thumb moves down as the panel does\n{top} then {}", end[0]);
}

#[test]
fn the_bar_is_never_the_whole_track_while_anything_is_hidden() {
    // Nine rows in a panel with room for eight. Rounded honestly the thumb is the whole bar, and a
    // full bar is what a panel with nothing hidden looks like — so the last row of the track is
    // kept back to say there is one more.
    let m = overflowing(9, 8, None);
    let thumb = thumb_rows(&m, 20, 14);
    assert!(!thumb.is_empty(), "there is a row below the edge, so there is a bar");
    assert!(thumb.len() < 8, "and it does not fill the track\n{thumb:?}");
}
