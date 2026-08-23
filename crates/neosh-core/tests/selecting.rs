//! A selection is the anchor and the cursor, and both ends of it are drawn.
//!
//! Two calls move a cursor and they mean different things about the anchor: `WinMotion` throws the
//! selection away unless it is told to extend, and `WinSetCursor` leaves the anchor exactly where
//! it is — which is what makes a *jump* with a selection running take the text it jumped over. That
//! difference is deliberate. What is not is a difference in whether the highlight is redrawn: a
//! range that has moved and a highlight that has not is the previous selection, still on screen
//! while a copy takes a different one.
//!
//! Asserted on what a frontend is *told*, because that is the whole of the bug. The anchor and the
//! cursor were always right; nothing said so.

use neosh_core::Editor;
use neosh_proto::{
    ApiCall, ApiOk, BufferId, Dock, Gravity, PluginId, SelectShape, UiEvent, WindowLayout,
};

const LINES: [&str; 4] = ["alpha", "beta", "gamma", "delta"];

/// A buffer of four rows, a window onto it, and a running tally of which rows are painted.
struct Screen {
    editor: Editor,
    plugin: PluginId,
    buf: BufferId,
    win: neosh_proto::WindowId,
    /// One flag per row, kept the way a mirror keeps it: by splicing every `BufferLines` in.
    rows: Vec<bool>,
}

impl Screen {
    fn new() -> Self {
        let mut editor = Editor::new();
        let plugin = PluginId::from("test");
        let ApiOk::Buf { buf } = editor
            .apply(&plugin, ApiCall::BufCreate {
                name: Some("t".into()),
                scratch: true,
                kind: None,
            })
            .expect("buffer")
        else {
            panic!("a buffer")
        };
        editor
            .apply(&plugin, ApiCall::BufSetLines {
                buf,
                start: 0,
                end: -1,
                lines: LINES.iter().map(|s| s.to_string()).collect(),
            })
            .expect("lines");
        let ApiOk::Win { win } = editor
            .apply(&plugin, ApiCall::WinOpen {
                buf,
                layout: WindowLayout::Docked {
                    dock: Dock::Main,
                    size: None,
                    gravity: Gravity::Start,
                    wrap: None,
                },
                view: None,
            })
            .expect("window")
        else {
            panic!("a window")
        };
        let mut s = Screen { editor, plugin, buf, win, rows: Vec::new() };
        s.drain();
        s
    }

    fn call(&mut self, call: ApiCall) {
        self.editor.apply(&self.plugin.clone(), call).expect("applied");
        self.drain();
    }

    fn cursor(&mut self, row: u32, col: u32) {
        self.call(ApiCall::WinSetCursor { win: self.win, row, col });
    }

    fn select(&mut self, on: bool) {
        self.call(ApiCall::WinSelect { win: self.win, on });
    }

    fn shape(&mut self, shape: SelectShape) {
        self.call(ApiCall::WinSelectShape { win: self.win, shape });
    }

    /// What `y` would copy.
    fn selection(&mut self) -> String {
        let win = self.win;
        match self.editor.apply(&self.plugin.clone(), ApiCall::WinSelection { win }) {
            Ok(ApiOk::Text { text }) => text,
            other => panic!("selection: {other:?}"),
        }
    }

    fn drain(&mut self) {
        for e in self.editor.drain_events() {
            let UiEvent::BufferLines { buf, start, old_end, lines } = e else { continue };
            if buf != self.buf {
                continue;
            }
            let start = (start as usize).min(self.rows.len());
            let end = (old_end as usize).clamp(start, self.rows.len());
            let new: Vec<bool> = lines
                .iter()
                .map(|l| l.marks.iter().any(|m| m.opts.hl_group.as_deref() == Some("Visual")))
                .collect();
            self.rows.splice(start..end, new);
        }
    }

    fn painted(&self) -> Vec<u32> {
        self.rows.iter().enumerate().filter(|(_, on)| **on).map(|(i, _)| i as u32).collect()
    }
}

/// The one that was wrong: a jump extends the selection, so it has to redraw it.
///
/// In the transcript reader every way of getting somewhere except `hjkl` arrives here — `^U`, `^D`,
/// `{`, `[`, a search hit. Without this, `v` and then a screen upwards selected six lines, showed
/// none of them, and copied all six.
#[test]
fn moving_the_cursor_redraws_the_selection_it_is_one_end_of() {
    let mut s = Screen::new();

    // At the end of the last row, so that extending upwards takes the whole of it — a selection
    // that stops at a row's first column contains none of that row, and is drawn as none of it.
    s.cursor(3, LINES[3].len() as u32);
    s.select(true);
    assert!(s.painted().is_empty(), "nothing between the two ends yet");

    // The jump. The anchor stays on row 3 — that is what separates this from `WinMotion` — so the
    // selection now runs over three rows and all three have to be drawn.
    s.cursor(1, 0);
    assert_eq!(s.painted(), vec![1, 2, 3], "the rows it jumped over");

    // And back, which is the same rule the other way: a highlight that can only grow is a selection
    // you cannot make smaller once you have overshot.
    s.cursor(2, 0);
    assert_eq!(s.painted(), vec![2, 3], "and the ones it came back over stop being drawn");
}

/// Dropping the selection takes the highlight with it, whatever moved the cursor last.
#[test]
fn giving_up_a_selection_clears_what_was_drawn_for_it() {
    let mut s = Screen::new();

    s.cursor(0, 0);
    s.select(true);
    s.cursor(2, 3);
    assert!(!s.painted().is_empty(), "something is drawn");

    s.select(false);
    assert!(s.painted().is_empty(), "and nothing is once it is dropped");
}

/// Moving the cursor with no selection running draws nothing. The repaint is about the selection,
/// not about the cursor: without the guard, every keystroke in the composer would rewrite its row.
#[test]
fn moving_the_cursor_without_a_selection_paints_nothing() {
    let mut s = Screen::new();

    s.cursor(0, 0);
    s.cursor(3, 2);
    assert!(s.painted().is_empty(), "there is no selection to draw");
}

// ---------------------------------------------------------------------------
// What the two ends mean
// ---------------------------------------------------------------------------

/// A selection is two positions, and the shape is the only thing that can say whether the character
/// the cursor is on is in it.
///
/// A text field's is not: the cursor sits *between* characters there, and one that swallowed the
/// character to its right would delete a letter nobody highlighted. A normal mode's is — and the
/// two conventions differ by exactly the one character somebody pressing `v` then `y` was trying
/// to copy, which is why it answered "nothing to copy" at them.
#[test]
fn an_inclusive_selection_contains_the_character_the_cursor_is_on() {
    let mut s = Screen::new();
    s.cursor(0, 0);
    s.select(true);

    assert_eq!(s.selection(), "", "exclusive, and both ends in the same place, is nothing");
    s.shape(SelectShape::Inclusive);
    assert_eq!(s.selection(), "a", "inclusive, it is the character under the cursor");
    assert_eq!(s.painted(), vec![0], "and it is drawn as one");

    s.cursor(0, 2);
    assert_eq!(s.selection(), "alp", "and grows to include whichever one it lands on");
}

/// Linewise is state on the window, not an invariant the caller re-establishes after every motion.
///
/// It used to be the latter: three API calls to move an anchor that is only settable by standing on
/// it, run again after every key. Extending *upwards* was what it was wrong for — the anchor stayed
/// at the start of the row `V` was pressed on, which makes that row the end of a backwards range
/// that stops at its first column, so the one row you definitely meant was the one that dropped out.
#[test]
fn linewise_takes_whole_rows_in_whichever_direction_it_runs() {
    let mut s = Screen::new();
    s.cursor(2, 3);
    s.select(true);
    s.shape(SelectShape::Line);

    assert_eq!(s.selection(), "gamma", "the whole of the row it started on");
    assert_eq!(s.painted(), vec![2]);

    s.cursor(1, 2);
    assert_eq!(s.selection(), "beta\ngamma", "upwards, and neither row is half in it");
    assert_eq!(s.painted(), vec![1, 2]);

    s.cursor(3, 1);
    assert_eq!(s.selection(), "gamma\ndelta", "and downwards from where it was anchored");
    assert_eq!(s.painted(), vec![2, 3]);
}

/// Dropping a selection drops its shape with it. A window left `Line` after the one linewise
/// selection it ever had would draw the next `v` as whole rows.
#[test]
fn the_shape_goes_when_the_selection_does() {
    let mut s = Screen::new();
    s.cursor(1, 0);
    s.select(true);
    s.shape(SelectShape::Line);
    assert_eq!(s.selection(), "beta");

    s.select(false);
    s.cursor(2, 0);
    s.select(true);
    assert_eq!(s.selection(), "", "back to a text field's answer until something says otherwise");
}
