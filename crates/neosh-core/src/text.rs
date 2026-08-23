//! Moving and editing text.
//!
//! Everything here is a pure function over lines and a `(row, byte_col)` cursor. Nothing allocates
//! a buffer, touches a mark or emits an event: [`plan`] returns the range replacement an edit
//! amounts to, and the caller feeds it to [`crate::buffer::Buffer::set_lines`] so mark survival
//! stays in exactly one place.
//!
//! # Columns are byte offsets, motion is by grapheme
//!
//! Those are different things and both are necessary. A column is a UTF-8 byte offset because that
//! is the protocol, and it keeps display-width math in the frontend where the pane width is known.
//! But pressing `←` once must not land inside a character — so motion steps by *grapheme cluster*,
//! which is neither a byte nor a `char`: `é` written as `e` + U+0301 is two `char`s and one
//! cluster, and a flag emoji is four. Stepping by `char` breaks both.
//!
//! # Vertical motion remembers a grapheme index, not a byte column
//!
//! Moving down a line and back up must return you to where you started even when the line between
//! is shorter. Byte columns cannot do that job — the same byte offset is a different place on a
//! line with different content — so the remembered column is a cluster index, which is content
//! independent enough to be right in every case that does not involve mixed character widths.
//! Neovim remembers a display column, which we cannot: the core does not know how wide anything is.

use neosh_proto::{CursorMotion as Motion, TextEdit as Edit};
use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::Line;

/// An edit expressed as the range replacement it is.
///
/// `lines[start..end]` becomes `text`. That shape exists so the caller can hand it straight to
/// `set_lines`, which is the one place that knows what happens to the marks on those lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditPlan {
    pub start: u32,
    pub end: u32,
    pub text: Vec<String>,
    pub cursor: (u32, u32),
}

// ---------------------------------------------------------------------------
// Motion
// ---------------------------------------------------------------------------

/// Resolve a motion.
///
/// `goal` is the remembered cluster index for vertical motion: pass the previous return value
/// back in, and pass `None` after any horizontal motion or edit. Returns the new cursor and the
/// goal to remember.
pub fn resolve(
    lines: &[Line],
    at: (u32, u32),
    motion: Motion,
    goal: Option<u32>,
) -> ((u32, u32), Option<u32>) {
    if lines.is_empty() {
        return ((0, 0), None);
    }
    let last = lines.len() as u32 - 1;
    let (row, col) = clamp(lines, at);
    let text = line(lines, row);

    match motion {
        Motion::Left => match prev_grapheme(text, col) {
            Some(c) => ((row, c), None),
            // Wrapping to the end of the line above is what makes backspace-by-motion and `←`
            // agree; a cursor that stops dead at column zero is a cursor you fight.
            None if row > 0 => ((row - 1, len(line(lines, row - 1))), None),
            None => ((row, 0), None),
        },
        Motion::Right => match next_grapheme(text, col) {
            Some(c) => ((row, c), None),
            None if row < last => ((row + 1, 0), None),
            None => ((row, len(text)), None),
        },
        Motion::Up if row == 0 => ((0, 0), None),
        Motion::Up => vertical(lines, row - 1, col, text, goal),
        Motion::Down if row == last => ((row, len(text)), None),
        Motion::Down => vertical(lines, row + 1, col, text, goal),
        Motion::LineStart => ((row, 0), None),
        Motion::LineEnd => ((row, len(text)), None),
        Motion::WordLeft => match word_start_before(text, col) {
            Some(c) => ((row, c), None),
            None if col > 0 => ((row, 0), None),
            None if row > 0 => ((row - 1, len(line(lines, row - 1))), None),
            None => ((row, 0), None),
        },
        Motion::WordRight => match word_start_after(text, col) {
            Some(c) => ((row, c), None),
            None if col < len(text) => ((row, len(text)), None),
            None if row < last => ((row + 1, 0), None),
            None => ((row, len(text)), None),
        },
        Motion::BufStart => ((0, 0), None),
        Motion::BufEnd => ((last, len(line(lines, last))), None),
    }
}

/// Up or down: keep the remembered cluster index, clamped to the target line.
fn vertical(
    lines: &[Line],
    to: u32,
    col: u32,
    from_text: &str,
    goal: Option<u32>,
) -> ((u32, u32), Option<u32>) {
    let want = goal.unwrap_or_else(|| cluster_index(from_text, col));
    ((to, cluster_col(line(lines, to), want)), Some(want))
}

/// Put a cursor somewhere real: on a line that exists, and on a grapheme boundary.
///
/// Anything can hand us a position — a plugin, a restored session, a click. Snapping here means no
/// caller has to, and means an off-boundary column can never reach an edit.
pub fn clamp(lines: &[Line], at: (u32, u32)) -> (u32, u32) {
    if lines.is_empty() {
        return (0, 0);
    }
    let row = at.0.min(lines.len() as u32 - 1);
    let text = line(lines, row);
    let col = at.1.min(len(text));
    (row, snap(text, col))
}

/// The nearest grapheme boundary at or before `col`.
fn snap(text: &str, col: u32) -> u32 {
    let c = col as usize;
    if c >= text.len() || text.is_char_boundary(c) && is_boundary(text, c) {
        return col.min(text.len() as u32);
    }
    // Ascending by construction, so the last one at or before `c` is the boundary we want.
    text.grapheme_indices(true).map(|(i, _)| i).rfind(|i| *i <= c).unwrap_or(0) as u32
}

fn is_boundary(text: &str, at: usize) -> bool {
    at == 0 || at == text.len() || text.grapheme_indices(true).any(|(i, _)| i == at)
}

fn line(lines: &[Line], row: u32) -> &str {
    lines.get(row as usize).map(|l| l.text.as_str()).unwrap_or("")
}

fn len(text: &str) -> u32 {
    text.len() as u32
}

/// The byte offset one grapheme past `col`, or the end of the line if there is none.
///
/// What "the character the cursor is on" ends at, which is the whole of the difference between an
/// inclusive selection and an exclusive one.
pub fn after(text: &str, col: u32) -> u32 {
    next_grapheme(text, col).unwrap_or_else(|| len(text))
}

fn prev_grapheme(text: &str, col: u32) -> Option<u32> {
    text.grapheme_indices(true).map(|(i, _)| i as u32).rfind(|i| *i < col)
}

fn next_grapheme(text: &str, col: u32) -> Option<u32> {
    text.grapheme_indices(true)
        .map(|(i, g)| (i + g.len()) as u32)
        .find(|i| *i > col)
        .filter(|i| *i <= len(text))
}

fn cluster_index(text: &str, col: u32) -> u32 {
    text.grapheme_indices(true).take_while(|(i, _)| (*i as u32) < col).count() as u32
}

fn cluster_col(text: &str, index: u32) -> u32 {
    text.grapheme_indices(true)
        .nth(index as usize)
        .map(|(i, _)| i as u32)
        .unwrap_or_else(|| len(text))
}

/// Word bounds that are not whitespace and not punctuation runs on their own.
///
/// `split_word_bound_indices` gives the Unicode segmentation, which already handles CJK, contracted
/// forms and scripts without spaces. Filtering to alphanumeric-leading bounds is what makes `^←`
/// skip over `", "` in one press rather than three, which is the behaviour every editor has.
fn words(text: &str) -> impl Iterator<Item = u32> + '_ {
    text.split_word_bound_indices()
        .filter(|(_, w)| w.chars().any(|c| c.is_alphanumeric() || c == '_'))
        .map(|(i, _)| i as u32)
}

fn word_start_before(text: &str, col: u32) -> Option<u32> {
    words(text).filter(|i| *i < col).last()
}

fn word_start_after(text: &str, col: u32) -> Option<u32> {
    words(text).find(|i| *i > col)
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

/// What an edit does, as a range replacement. `None` when it would change nothing.
pub fn plan(lines: &[Line], at: (u32, u32), edit: &Edit) -> Option<EditPlan> {
    let (row, col) = clamp(lines, at);
    match edit {
        Edit::Insert { text } => Some(insert(lines, (row, col), text)),
        Edit::DeleteBack => {
            let (from, _) = resolve(lines, (row, col), Motion::Left, None);
            cut(lines, from, (row, col))
        }
        Edit::DeleteForward => {
            let (to, _) = resolve(lines, (row, col), Motion::Right, None);
            cut(lines, (row, col), to)
        }
        Edit::DeleteWordBack => {
            let (from, _) = resolve(lines, (row, col), Motion::WordLeft, None);
            cut(lines, from, (row, col))
        }
        Edit::DeleteWordForward => {
            let (to, _) = resolve(lines, (row, col), Motion::WordRight, None);
            cut(lines, (row, col), to)
        }
        Edit::DeleteToLineStart => cut(lines, (row, 0), (row, col)),
        Edit::DeleteToLineEnd => {
            // At the end of a line this joins the next one, which is what `^K` does everywhere and
            // the only way to remove a blank line without moving first.
            let end = len(line(lines, row));
            if col >= end {
                let (to, _) = resolve(lines, (row, col), Motion::Right, None);
                cut(lines, (row, col), to)
            } else {
                cut(lines, (row, col), (row, end))
            }
        }
        Edit::DeleteRange { from, to } => {
            let (a, b) = ordered(clamp(lines, *from), clamp(lines, *to));
            cut(lines, a, b)
        }
        // Resolved by the editor, which is the only thing that knows where the anchor is.
        Edit::DeleteSelection => None,
    }
}

fn insert(lines: &[Line], at: (u32, u32), text: &str) -> EditPlan {
    let (row, col) = at;
    let current = line(lines, row);
    let (before, after) = current.split_at(col as usize);

    let mut pieces: Vec<String> = text.split('\n').map(|p| p.replace('\r', "")).collect();
    // Cursor lands after the inserted text, which for a multi-line paste is on the last piece.
    let cursor = if pieces.len() == 1 {
        (row, col + pieces[0].len() as u32)
    } else {
        (row + pieces.len() as u32 - 1, pieces[pieces.len() - 1].len() as u32)
    };

    let first = pieces.first_mut().expect("split always yields one piece");
    *first = format!("{before}{first}");
    let last = pieces.last_mut().expect("split always yields one piece");
    last.push_str(after);

    EditPlan { start: row, end: row + 1, text: pieces, cursor }
}

/// Remove everything between two positions. `None` when they are the same place.
fn cut(lines: &[Line], from: (u32, u32), to: (u32, u32)) -> Option<EditPlan> {
    let (from, to) = ordered(from, to);
    if from == to {
        return None;
    }
    let head = &line(lines, from.0)[..from.1 as usize];
    let tail = &line(lines, to.0)[to.1 as usize..];
    Some(EditPlan {
        start: from.0,
        end: to.0 + 1,
        text: vec![format!("{head}{tail}")],
        cursor: from,
    })
}

fn ordered(a: (u32, u32), b: (u32, u32)) -> ((u32, u32), (u32, u32)) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The text between two positions, newline-joined. Used for yanking a selection.
pub fn slice(lines: &[Line], from: (u32, u32), to: (u32, u32)) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let (from, to) = ordered(clamp(lines, from), clamp(lines, to));
    if from.0 == to.0 {
        return line(lines, from.0)[from.1 as usize..to.1 as usize].to_string();
    }
    let mut out = String::from(&line(lines, from.0)[from.1 as usize..]);
    for row in from.0 + 1..to.0 {
        out.push('\n');
        out.push_str(line(lines, row));
    }
    out.push('\n');
    out.push_str(&line(lines, to.0)[..to.1 as usize]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(text: &[&str]) -> Vec<Line> {
        text.iter().map(|t| Line::new(*t)).collect()
    }

    fn go(lines: &[Line], at: (u32, u32), m: Motion) -> (u32, u32) {
        resolve(lines, at, m, None).0
    }

    // ---- horizontal motion ------------------------------------------------

    #[test]
    fn left_and_right_step_over_a_whole_grapheme_cluster() {
        // "é" as e + combining acute is two chars and three bytes; stepping by `char` would leave
        // the cursor between the letter and its accent, which is not a place.
        let b = buf(&["ae\u{301}b"]);
        assert_eq!(go(&b, (0, 1), Motion::Right), (0, 4), "past the whole cluster");
        assert_eq!(go(&b, (0, 4), Motion::Left), (0, 1), "and back to where it started");
    }

    #[test]
    fn an_emoji_sequence_is_one_step_in_each_direction() {
        let b = buf(&["a\u{1f469}\u{200d}\u{1f4bb}b"]);
        let past = go(&b, (0, 1), Motion::Right);
        assert_eq!(past, (0, 1 + "\u{1f469}\u{200d}\u{1f4bb}".len() as u32));
        assert_eq!(go(&b, past, Motion::Left), (0, 1));
    }

    #[test]
    fn left_at_the_start_of_a_line_goes_to_the_end_of_the_one_above() {
        let b = buf(&["ab", "cd"]);
        assert_eq!(go(&b, (1, 0), Motion::Left), (0, 2));
    }

    #[test]
    fn right_at_the_end_of_a_line_goes_to_the_start_of_the_next() {
        let b = buf(&["ab", "cd"]);
        assert_eq!(go(&b, (0, 2), Motion::Right), (1, 0));
    }

    #[test]
    fn neither_end_of_the_buffer_moves_anywhere() {
        let b = buf(&["ab"]);
        assert_eq!(go(&b, (0, 0), Motion::Left), (0, 0));
        assert_eq!(go(&b, (0, 2), Motion::Right), (0, 2));
    }

    // ---- vertical motion --------------------------------------------------

    #[test]
    fn going_down_past_a_short_line_and_back_up_returns_to_the_same_column() {
        // The whole reason a remembered column exists. Without it the cursor collapses to the
        // short line's width and stays there.
        let b = buf(&["0123456789", "ab", "0123456789"]);
        let (down, goal) = resolve(&b, (0, 8), Motion::Down, None);
        assert_eq!(down, (1, 2), "clamped to the short line");
        let (further, goal) = resolve(&b, down, Motion::Down, goal);
        assert_eq!(further, (2, 8), "and restored on the long one");
        let (back, _) = resolve(&b, further, Motion::Up, goal);
        assert_eq!(back, (1, 2));
    }

    #[test]
    fn the_remembered_column_is_a_cluster_index_not_a_byte_offset() {
        // Byte 4 is the fifth *byte* of the accented line and the fifth character of the plain
        // one. Keeping bytes would land the cursor two characters adrift.
        let b = buf(&["e\u{301}e\u{301}xy", "abcdef"]);
        let (down, _) = resolve(&b, (0, 6), Motion::Down, None);
        assert_eq!(down, (1, 2), "three clusters in, wherever that is in bytes");
    }

    #[test]
    fn up_from_the_first_line_and_down_from_the_last_go_to_the_ends() {
        let b = buf(&["ab", "cdef"]);
        assert_eq!(go(&b, (0, 1), Motion::Up), (0, 0));
        assert_eq!(go(&b, (1, 1), Motion::Down), (1, 4));
    }

    // ---- word motion ------------------------------------------------------

    #[test]
    fn word_motion_steps_over_punctuation_in_one_press() {
        let b = buf(&["fix(core): the thing"]);
        assert_eq!(go(&b, (0, 0), Motion::WordRight), (0, 4), "over `(` to `core`");
        assert_eq!(go(&b, (0, 4), Motion::WordRight), (0, 11), "over `): ` to `the`");
        assert_eq!(go(&b, (0, 11), Motion::WordLeft), (0, 4));
    }

    #[test]
    fn word_motion_crosses_lines_once_there_is_nothing_left_on_this_one() {
        let b = buf(&["one", "two"]);
        assert_eq!(go(&b, (0, 0), Motion::WordRight), (0, 3), "first to the end of the line");
        assert_eq!(go(&b, (0, 3), Motion::WordRight), (1, 0), "then onto the next");
        assert_eq!(go(&b, (1, 0), Motion::WordLeft), (0, 3));
    }

    #[test]
    fn word_motion_treats_cjk_as_words_rather_than_one_long_run() {
        let b = buf(&["\u{4eca}\u{65e5}\u{306f} world"]);
        let first = go(&b, (0, 0), Motion::WordRight);
        assert!(first > (0, 0) && first < (0, 10), "moved, but not to the end: {first:?}");
    }

    // ---- clamping ---------------------------------------------------------

    #[test]
    fn a_column_inside_a_character_snaps_to_its_start() {
        let b = buf(&["e\u{301}x"]);
        assert_eq!(clamp(&b, (0, 2)), (0, 0), "byte 2 is inside the combining mark");
    }

    #[test]
    fn a_row_past_the_end_lands_on_the_last_line() {
        let b = buf(&["ab", "cd"]);
        assert_eq!(clamp(&b, (9, 9)), (1, 2));
    }

    // ---- editing ----------------------------------------------------------

    #[test]
    fn typing_inserts_at_the_cursor_rather_than_at_the_end() {
        let b = buf(&["helo"]);
        let p = plan(&b, (0, 3), &Edit::Insert { text: "l".into() }).expect("an edit");
        assert_eq!(p.text, vec!["hello"]);
        assert_eq!(p.cursor, (0, 4));
    }

    #[test]
    fn inserting_a_newline_splits_the_line_and_lands_on_the_second_half() {
        let b = buf(&["abcd"]);
        let p = plan(&b, (0, 2), &Edit::Insert { text: "\n".into() }).expect("an edit");
        assert_eq!(p.text, vec!["ab", "cd"]);
        assert_eq!(p.cursor, (1, 0));
    }

    #[test]
    fn pasting_several_lines_leaves_the_cursor_after_the_last_of_them() {
        let b = buf(&["[]"]);
        let p = plan(&b, (0, 1), &Edit::Insert { text: "one\ntwo\nthree".into() }).expect("an edit");
        assert_eq!(p.text, vec!["[one", "two", "three]"]);
        assert_eq!(p.cursor, (2, 5));
    }

    #[test]
    fn a_pasted_crlf_does_not_leave_carriage_returns_in_the_buffer() {
        let b = buf(&[""]);
        let p = plan(&b, (0, 0), &Edit::Insert { text: "a\r\nb".into() }).expect("an edit");
        assert_eq!(p.text, vec!["a", "b"]);
    }

    #[test]
    fn backspace_removes_one_cluster_not_one_byte() {
        // `a` + `e` + a two-byte combining acute: four bytes, two clusters.
        let b = buf(&["ae\u{301}"]);
        let p = plan(&b, (0, 4), &Edit::DeleteBack).expect("an edit");
        assert_eq!(p.text, vec!["a"]);
        assert_eq!(p.cursor, (0, 1));
    }

    #[test]
    fn backspace_at_the_start_of_a_line_joins_it_to_the_one_above() {
        let b = buf(&["ab", "cd"]);
        let p = plan(&b, (1, 0), &Edit::DeleteBack).expect("an edit");
        assert_eq!(p.start, 0);
        assert_eq!(p.end, 2);
        assert_eq!(p.text, vec!["abcd"]);
        assert_eq!(p.cursor, (0, 2));
    }

    #[test]
    fn backspace_at_the_very_beginning_changes_nothing() {
        let b = buf(&["ab"]);
        assert_eq!(plan(&b, (0, 0), &Edit::DeleteBack), None);
    }

    #[test]
    fn delete_forward_at_the_end_of_a_line_pulls_the_next_one_up() {
        let b = buf(&["ab", "cd"]);
        let p = plan(&b, (0, 2), &Edit::DeleteForward).expect("an edit");
        assert_eq!(p.text, vec!["abcd"]);
        assert_eq!(p.cursor, (0, 2));
    }

    #[test]
    fn deleting_a_word_backwards_takes_the_punctuation_with_it() {
        let b = buf(&["fix(core): thing"]);
        let p = plan(&b, (0, 16), &Edit::DeleteWordBack).expect("an edit");
        assert_eq!(p.text, vec!["fix(core): "]);
    }

    #[test]
    fn kill_to_line_end_at_the_end_of_a_line_joins_the_next() {
        let b = buf(&["ab", "cd"]);
        let p = plan(&b, (0, 2), &Edit::DeleteToLineEnd).expect("an edit");
        assert_eq!(p.text, vec!["abcd"]);
    }

    #[test]
    fn kill_to_line_start_leaves_what_is_after_the_cursor() {
        let b = buf(&["hello there"]);
        let p = plan(&b, (0, 6), &Edit::DeleteToLineStart).expect("an edit");
        assert_eq!(p.text, vec!["there"]);
        assert_eq!(p.cursor, (0, 0));
    }

    #[test]
    fn deleting_a_range_spanning_lines_leaves_one_line() {
        let b = buf(&["one", "two", "three"]);
        let p = plan(&b, (0, 0), &Edit::DeleteRange { from: (0, 1), to: (2, 2) })
            .expect("an edit");
        assert_eq!(p.start, 0);
        assert_eq!(p.end, 3);
        assert_eq!(p.text, vec!["oree"]);
        assert_eq!(p.cursor, (0, 1));
    }

    #[test]
    fn a_range_given_backwards_deletes_the_same_thing() {
        let b = buf(&["one", "two"]);
        let forwards = plan(&b, (0, 0), &Edit::DeleteRange { from: (0, 1), to: (1, 1) });
        let backwards = plan(&b, (0, 0), &Edit::DeleteRange { from: (1, 1), to: (0, 1) });
        assert_eq!(forwards, backwards);
    }

    #[test]
    fn an_empty_range_is_not_an_edit() {
        let b = buf(&["one"]);
        assert_eq!(plan(&b, (0, 0), &Edit::DeleteRange { from: (0, 1), to: (0, 1) }), None);
    }

    // ---- slicing ----------------------------------------------------------

    #[test]
    fn slicing_within_one_line_is_the_substring() {
        let b = buf(&["hello there"]);
        assert_eq!(slice(&b, (0, 6), (0, 11)), "there");
    }

    #[test]
    fn slicing_across_lines_joins_them_with_newlines() {
        let b = buf(&["one", "two", "three"]);
        assert_eq!(slice(&b, (0, 1), (2, 2)), "ne\ntwo\nth");
    }

    #[test]
    fn slicing_backwards_gives_the_same_text() {
        let b = buf(&["one", "two"]);
        assert_eq!(slice(&b, (1, 1), (0, 1)), slice(&b, (0, 1), (1, 1)));
    }

    #[test]
    fn nothing_at_all_is_survivable() {
        let empty: Vec<Line> = Vec::new();
        assert_eq!(resolve(&empty, (0, 0), Motion::Down, None).0, (0, 0));
        assert_eq!(slice(&empty, (0, 0), (5, 5)), "");
        assert_eq!(clamp(&empty, (3, 3)), (0, 0));
    }
}
