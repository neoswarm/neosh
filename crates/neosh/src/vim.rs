//! Normal-mode motion, over rows of plain text.
//!
//! Pure functions over `&[String]` and a `(row, byte_col)` cursor: nothing here touches a window,
//! a buffer or an event. That is what makes it testable, and it is also the honest shape of the
//! thing — a plugin doing this would read the rows with `buf.lines` and put the cursor back with
//! `win.setCursor`, which is exactly what the caller does.
//!
//! # Why not [`neosh_proto::CursorMotion`]
//!
//! Those are a *text field's* motions and they are right for one: the cursor sits between two
//! characters, `←` at the start of a line wraps to the end of the one above, and `End` goes one
//! past the last character because that is where the next thing you type goes. Normal mode is the
//! other set of answers to all three. The cursor is *on* a character, `h` stops dead at the first
//! column rather than leaving the line, and `$` is the last character rather than the space after
//! it — which matters the moment a selection includes the character the cursor is on, because the
//! two conventions differ by exactly the one character you were trying to copy.
//!
//! # Columns are byte offsets and motion is by grapheme
//!
//! The protocol's rule, and it survives here: every position returned is a UTF-8 byte offset on a
//! grapheme boundary. Word classification looks at `char`s, because a word boundary is a change of
//! character class and a combining mark has no class of its own — so every result is snapped back
//! to a cluster boundary before it leaves.

use unicode_segmentation::UnicodeSegmentation;

/// Where to go. One press of the key; the caller applies the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    /// `w` / `W`: the start of the next word.
    WordRight { big: bool },
    /// `b` / `B`: the start of the previous word.
    WordLeft { big: bool },
    /// `e` / `E`: the end of this word, or of the next one if already on it.
    WordEnd { big: bool },
    /// `ge` / `gE`: the end of the previous word.
    WordEndBack { big: bool },
    /// `0`.
    LineStart,
    /// `^`.
    FirstNonBlank,
    /// `$`.
    LineEnd,
    /// `g_`.
    LastNonBlank,
    /// `f` / `F` / `t` / `T`.
    Find { ch: char, till: bool, back: bool },
    /// `%`.
    MatchPair,
    /// `gg` / `G`, and `12G`. Zero-based; the caller has already taken the one off.
    Row(u32),
    BufStart,
    BufEnd,
}

/// A resolved motion.
pub struct Step {
    pub at: (u32, u32),
    /// The remembered column for `j`/`k`, as a grapheme-cluster index. Pass it back in.
    pub goal: Option<u32>,
    /// Whether an operator using this motion takes the character it lands on.
    ///
    /// Vim's exclusive/inclusive distinction, and it is not decoration: `yw` from the start of a
    /// word must not take the first letter of the next one, while `ye` must take the last letter
    /// of this one. Both land in the same sort of place; only this says what that place means.
    pub inclusive: bool,
    /// Whether the motion moved at all. A key that does nothing is worth knowing about: a count
    /// spent on `5j` at the bottom of the transcript should stop rather than beep five times.
    pub moved: bool,
}

/// A remembered column of "wherever this row ends", which is what `$` leaves behind.
pub const END: u32 = u32::MAX;

/// Character classes, Vim's. Blank, punctuation, word — and for a `W`-style motion only the first
/// two, because a big word is "everything that is not a space".
fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        2
    } else {
        1
    }
}

fn row_text(lines: &[String], row: u32) -> &str {
    lines.get(row as usize).map(String::as_str).unwrap_or("")
}

fn last_row(lines: &[String]) -> u32 {
    (lines.len() as u32).saturating_sub(1)
}

/// The nearest grapheme boundary at or before `col`.
fn snap(text: &str, col: usize) -> u32 {
    if col >= text.len() {
        return text.len() as u32;
    }
    text.grapheme_indices(true).map(|(i, _)| i).take_while(|i| *i <= col).last().unwrap_or(0) as u32
}

/// The byte offset of the last grapheme on a row, or zero on an empty one.
///
/// Where `$` goes, and the furthest right a normal-mode cursor may be. A text field's `End` is one
/// column past this, which is the whole of the difference between the two.
pub fn line_end(text: &str) -> u32 {
    text.grapheme_indices(true).map(|(i, _)| i as u32).next_back().unwrap_or(0)
}

/// The first non-blank on a row, or the last grapheme if the row is nothing but blanks.
pub fn first_non_blank(text: &str) -> u32 {
    text.char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i as u32)
        .unwrap_or_else(|| line_end(text))
}

fn last_non_blank(text: &str) -> u32 {
    text.char_indices()
        .filter(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i as u32)
        .next_back()
        .unwrap_or(0)
}

/// Put a position somewhere a normal-mode cursor can actually be.
///
/// On a row that exists, on a grapheme boundary, and never past the last character of a non-empty
/// row. That last clause is the one that matters: a cursor parked one column past the end draws a
/// block over a space nobody can see, and every selection taken from there is one character long
/// and empty.
pub fn clamp(lines: &[String], at: (u32, u32)) -> (u32, u32) {
    if lines.is_empty() {
        return (0, 0);
    }
    let row = at.0.min(last_row(lines));
    let text = row_text(lines, row);
    (row, snap(text, (at.1 as usize).min(line_end(text) as usize)))
}

/// The grapheme-cluster index of a byte column, for remembering a column across a vertical motion.
fn cluster_of(text: &str, col: u32) -> u32 {
    text.grapheme_indices(true).take_while(|(i, _)| (*i as u32) < col).count() as u32
}

/// The byte column a grapheme-cluster index lands on, clamped to the row's last character.
///
/// [`END`] is not a column: it is what `$` remembers, and it means the last character of whichever
/// row the cursor arrives on rather than a fixed distance along it.
fn col_of(text: &str, cluster: u32) -> u32 {
    if cluster == END {
        return line_end(text);
    }
    text.grapheme_indices(true)
        .nth(cluster as usize)
        .map(|(i, _)| i as u32)
        .unwrap_or_else(|| line_end(text))
}

/// One press of a motion key.
pub fn resolve(lines: &[String], at: (u32, u32), m: Motion, goal: Option<u32>) -> Step {
    if lines.is_empty() {
        return Step { at: (0, 0), goal: None, inclusive: false, moved: false };
    }
    let at = clamp(lines, at);
    let (row, col) = at;
    let text = row_text(lines, row);
    let last = last_row(lines);

    let step = |to: (u32, u32), inclusive: bool| Step {
        at: clamp(lines, to),
        goal: None,
        inclusive,
        moved: clamp(lines, to) != at,
    };

    match m {
        // Deliberately no wrapping onto the row above or below. A text field's `←` wraps because
        // the character before the first one really is the newline; `h` does not, because in a
        // mode where every key is a verb, one that quietly changes which line you are operating on
        // is the one you cannot use with an operator.
        Motion::Left => {
            let to = text
                .grapheme_indices(true)
                .map(|(i, _)| i as u32)
                .rfind(|i| *i < col)
                .unwrap_or(0);
            step((row, to), false)
        }
        Motion::Right => {
            let to = text
                .grapheme_indices(true)
                .map(|(i, _)| i as u32)
                .find(|i| *i > col)
                .unwrap_or(col);
            step((row, to), false)
        }
        Motion::Up | Motion::Down => {
            let want = goal.unwrap_or_else(|| cluster_of(text, col));
            let to = match m {
                Motion::Up => row.checked_sub(1),
                _ => (row < last).then_some(row + 1),
            };
            let Some(to) = to else {
                return Step { at, goal: Some(want), inclusive: false, moved: false };
            };
            Step {
                at: (to, col_of(row_text(lines, to), want)),
                goal: Some(want),
                inclusive: false,
                moved: true,
            }
        }
        Motion::LineStart => step((row, 0), false),
        Motion::FirstNonBlank => step((row, first_non_blank(text)), false),
        // Inclusive, which is what makes `y$` take the last character rather than stop one short.
        // And it remembers a column of [`END`] rather than none: `$` then `j` walks down the
        // right-hand edge of the text, which is the one thing `$` is for on more than one row.
        Motion::LineEnd => Step {
            at: (row, line_end(text)),
            goal: Some(END),
            inclusive: true,
            moved: line_end(text) != col,
        },
        Motion::LastNonBlank => step((row, last_non_blank(text)), true),
        Motion::WordRight { big } => step(word_right(lines, at, big), false),
        Motion::WordLeft { big } => step(word_left(lines, at, big), false),
        Motion::WordEnd { big } => step(word_end(lines, at, big), true),
        Motion::WordEndBack { big } => step(word_end_back(lines, at, big), true),
        Motion::Find { ch, till, back } => match find_char(text, col, ch, till, back) {
            Some(to) => step((row, to), !back),
            None => Step { at, goal: None, inclusive: !back, moved: false },
        },
        Motion::MatchPair => match match_pair(lines, at) {
            Some(to) => step(to, true),
            None => Step { at, goal: None, inclusive: true, moved: false },
        },
        // A row rather than a repetition, and it lands on the first non-blank the way every
        // line-addressing motion in vi does.
        Motion::Row(to) => {
            let to = to.min(last);
            step((to, first_non_blank(row_text(lines, to))), false)
        }
        Motion::BufStart => step((0, first_non_blank(row_text(lines, 0))), false),
        Motion::BufEnd => step((last, first_non_blank(row_text(lines, last))), false),
    }
}

// ---------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------

/// The characters of a row as `(byte offset, class)`, which is all any word motion needs.
fn classes(text: &str, big: bool) -> Vec<(u32, u8)> {
    text.char_indices().map(|(i, c)| (i as u32, class(c, big))).collect()
}

fn word_right(lines: &[String], at: (u32, u32), big: bool) -> (u32, u32) {
    let (row, col) = at;
    let last = last_row(lines);
    let here = classes(row_text(lines, row), big);
    let i = here.iter().position(|(o, _)| *o == col).unwrap_or(here.len());
    let start = here.get(i).map(|(_, c)| *c).unwrap_or(0);
    let mut j = i;
    // Off the end of the word the cursor is in...
    if start != 0 {
        while here.get(j).map(|(_, c)| *c) == Some(start) {
            j += 1;
        }
    }
    // ...and over whatever blanks follow it.
    while here.get(j).map(|(_, c)| *c) == Some(0) {
        j += 1;
    }
    if let Some((o, _)) = here.get(j) {
        return (row, *o);
    }
    // Nothing more on this row. An empty row is a word of its own, which is Vim's rule and is what
    // makes `w` walk *out* of a paragraph rather than straight over the gap after it.
    if row < last {
        return (row + 1, first_non_blank(row_text(lines, row + 1)));
    }
    (row, line_end(row_text(lines, row)))
}

fn word_left(lines: &[String], at: (u32, u32), big: bool) -> (u32, u32) {
    let (mut row, col) = at;
    let here = classes(row_text(lines, row), big);
    let i = here.iter().position(|(o, _)| *o == col).unwrap_or(here.len());
    let mut j = i;
    // Back over blanks, then to the front of whatever word we landed in.
    while j > 0 && here.get(j - 1).map(|(_, c)| *c) == Some(0) {
        j -= 1;
    }
    if j == 0 {
        // Nothing left on this row. The previous row's last word, or its start if it is blank.
        while row > 0 {
            row -= 1;
            let text = row_text(lines, row);
            if text.trim().is_empty() {
                return (row, 0);
            }
            let prev = classes(text, big);
            let mut k = prev.len();
            while k > 0 && prev.get(k - 1).map(|(_, c)| *c) == Some(0) {
                k -= 1;
            }
            if k == 0 {
                continue;
            }
            let cls = prev[k - 1].1;
            while k > 1 && prev[k - 2].1 == cls {
                k -= 1;
            }
            return (row, prev[k - 1].0);
        }
        return (0, 0);
    }
    let cls = here[j - 1].1;
    while j > 1 && here[j - 2].1 == cls {
        j -= 1;
    }
    (row, here[j - 1].0)
}

fn word_end(lines: &[String], at: (u32, u32), big: bool) -> (u32, u32) {
    let (mut row, col) = at;
    let last = last_row(lines);
    let mut from = col;
    loop {
        let here = classes(row_text(lines, row), big);
        let start = if row == at.0 {
            here.iter().position(|(o, _)| *o == from).map(|i| i + 1).unwrap_or(0)
        } else {
            0
        };
        let mut j = start;
        while here.get(j).map(|(_, c)| *c) == Some(0) {
            j += 1;
        }
        if let Some((_, cls)) = here.get(j).copied() {
            let mut k = j;
            while here.get(k + 1).map(|(_, c)| *c) == Some(cls) {
                k += 1;
            }
            return (row, here[k].0);
        }
        if row == last {
            return (row, line_end(row_text(lines, row)));
        }
        row += 1;
        from = 0;
    }
}

fn word_end_back(lines: &[String], at: (u32, u32), big: bool) -> (u32, u32) {
    let (mut row, col) = at;
    let mut from = Some(col);
    loop {
        let here = classes(row_text(lines, row), big);
        let mut j = match from {
            Some(c) => here.iter().position(|(o, _)| *o == c).unwrap_or(here.len()),
            None => here.len(),
        };
        while j > 0 && here.get(j - 1).map(|(_, c)| *c) == Some(0) {
            j -= 1;
        }
        if j > 0 {
            return (row, here[j - 1].0);
        }
        if row == 0 {
            return (0, 0);
        }
        row -= 1;
        from = None;
    }
}

// ---------------------------------------------------------------------------
// f / F / t / T and %
// ---------------------------------------------------------------------------

fn find_char(text: &str, col: u32, ch: char, till: bool, back: bool) -> Option<u32> {
    let hits: Vec<u32> = text.char_indices().filter(|(_, c)| *c == ch).map(|(i, _)| i as u32).collect();
    // `t` starts from one past where `f` would, or `;` on a `t` never moves: the cursor is already
    // sitting next to the character it stopped before.
    let found = if back {
        hits.iter().copied().rfind(|i| *i < col)?
    } else {
        hits.iter().copied().find(|i| *i > col)?
    };
    if !till {
        return Some(found);
    }
    let step = |from: u32, forward: bool| -> Option<u32> {
        if forward {
            text.grapheme_indices(true).map(|(i, _)| i as u32).rfind(|i| *i < from)
        } else {
            text.grapheme_indices(true).map(|(i, _)| i as u32).find(|i| *i > from)
        }
    };
    step(found, !back)
}

const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

/// `%`: the bracket matching the first one at or after the cursor on this row.
fn match_pair(lines: &[String], at: (u32, u32)) -> Option<(u32, u32)> {
    let text = row_text(lines, at.0);
    let (col, ch) = text
        .char_indices()
        .find(|(i, c)| *i as u32 >= at.1 && PAIRS.iter().any(|(o, c2)| c == o || c == c2))
        .map(|(i, c)| (i as u32, c))?;
    let (open, close, forward) = PAIRS
        .iter()
        .find_map(|(o, c)| {
            if ch == *o {
                Some((*o, *c, true))
            } else if ch == *c {
                Some((*o, *c, false))
            } else {
                None
            }
        })?;

    let mut depth = 0i32;
    let mut row = at.0 as i64;
    let mut from = Some(col);
    loop {
        let text = row_text(lines, row as u32);
        let mut chars: Vec<(u32, char)> =
            text.char_indices().map(|(i, c)| (i as u32, c)).collect();
        if !forward {
            chars.reverse();
        }
        for (i, c) in chars {
            match from {
                Some(f) if (forward && i < f) || (!forward && i > f) => continue,
                _ => {}
            }
            if c == open {
                depth += if forward { 1 } else { -1 };
            } else if c == close {
                depth += if forward { -1 } else { 1 };
            } else {
                continue;
            }
            if depth == 0 {
                return Some((row as u32, i));
            }
        }
        from = None;
        row += if forward { 1 } else { -1 };
        if row < 0 || row > last_row(lines) as i64 {
            return None;
        }
    }
}

// ---------------------------------------------------------------------------
// Text objects
// ---------------------------------------------------------------------------

/// What `iw`, `a"`, `ip` and friends name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Object {
    Word { big: bool },
    Paragraph,
    Quote(char),
    Pair(char, char),
}

/// The two ends of a text object, both *inclusive* — the second is the position of the last
/// character in it, which is where a visual-mode cursor goes.
///
/// `None` when the cursor is not in one: outside every quote on the row, or on a bracket with no
/// partner. Silence is the right answer there — Vim does nothing too, and guessing at the nearest
/// one is how `ci"` eats the wrong string.
pub fn object(
    lines: &[String],
    at: (u32, u32),
    obj: Object,
    around: bool,
) -> Option<((u32, u32), (u32, u32))> {
    if lines.is_empty() {
        return None;
    }
    let (row, col) = clamp(lines, at);
    match obj {
        Object::Word { big } => {
            let text = row_text(lines, row);
            let here = classes(text, big);
            let i = here.iter().position(|(o, _)| *o == col)?;
            let cls = here[i].1;
            let (mut a, mut b) = (i, i);
            while a > 0 && here[a - 1].1 == cls {
                a -= 1;
            }
            while b + 1 < here.len() && here[b + 1].1 == cls {
                b += 1;
            }
            if around {
                // Trailing whitespace, or leading if there is none — which is Vim's rule and the
                // reason `daw` on the last word of a line does not leave a dangling space.
                let mut c = b;
                while c + 1 < here.len() && here[c + 1].1 == 0 {
                    c += 1;
                }
                if c == b && cls != 0 {
                    while a > 0 && here[a - 1].1 == 0 {
                        a -= 1;
                    }
                } else {
                    b = c;
                }
            }
            Some(((row, here[a].0), (row, here[b].0)))
        }
        Object::Paragraph => {
            let blank = |r: u32| row_text(lines, r).trim().is_empty();
            let inside = !blank(row);
            let (mut a, mut b) = (row, row);
            while a > 0 && blank(a - 1) == !inside {
                a -= 1;
            }
            while b < last_row(lines) && blank(b + 1) == !inside {
                b += 1;
            }
            if around {
                while b < last_row(lines) && blank(b + 1) == inside {
                    b += 1;
                }
            }
            Some(((a, 0), (b, line_end(row_text(lines, b)))))
        }
        Object::Quote(q) => {
            let text = row_text(lines, row);
            let hits: Vec<u32> =
                text.char_indices().filter(|(_, c)| *c == q).map(|(i, _)| i as u32).collect();
            // Pairs, in order. The cursor is in the one whose pair straddles it, or — Vim's own
            // convenience — in the next one along if it is sitting before them all.
            let pair = hits
                .chunks(2)
                .filter(|p| p.len() == 2)
                .find(|p| col <= p[1])
                .map(|p| (p[0], p[1]))?;
            if around {
                return Some(((row, pair.0), (row, pair.1)));
            }
            let inner_a = text.char_indices().map(|(i, _)| i as u32).find(|i| *i > pair.0)?;
            let inner_b = text.char_indices().map(|(i, _)| i as u32).rfind(|i| *i < pair.1)?;
            (inner_a <= inner_b).then_some(((row, inner_a), (row, inner_b)))
        }
        Object::Pair(open, close) => {
            let (a, b) = enclosing(lines, (row, col), open, close)?;
            if around {
                return Some((a, b));
            }
            let after_a = step_right(lines, a)?;
            let before_b = step_left(lines, b)?;
            (after_a <= before_b).then_some((after_a, before_b))
        }
    }
}

fn step_right(lines: &[String], at: (u32, u32)) -> Option<(u32, u32)> {
    let text = row_text(lines, at.0);
    match text.grapheme_indices(true).map(|(i, _)| i as u32).find(|i| *i > at.1) {
        Some(c) => Some((at.0, c)),
        None if at.0 < last_row(lines) => Some((at.0 + 1, 0)),
        None => None,
    }
}

fn step_left(lines: &[String], at: (u32, u32)) -> Option<(u32, u32)> {
    let text = row_text(lines, at.0);
    match text.grapheme_indices(true).map(|(i, _)| i as u32).rfind(|i| *i < at.1) {
        Some(c) => Some((at.0, c)),
        None if at.0 > 0 => Some((at.0 - 1, line_end(row_text(lines, at.0 - 1)))),
        None => None,
    }
}

/// The innermost `open`/`close` pair containing (or starting at) the cursor.
fn enclosing(
    lines: &[String],
    at: (u32, u32),
    open: char,
    close: char,
) -> Option<((u32, u32), (u32, u32))> {
    // On the opening bracket itself, that is the pair. Anywhere else — inside it, or on its
    // closing partner — walk backwards to the one that is still unmatched.
    let on_open = row_text(lines, at.0)
        .char_indices()
        .any(|(i, c)| i as u32 == at.1 && c == open);
    let start = if on_open { at } else { scan(lines, at, open, close, false)? };
    let end = scan(lines, start, open, close, true)?;
    Some((start, end))
}

/// Walk out to the unmatched `open` before `at`, or in to the `close` that balances it.
fn scan(
    lines: &[String],
    at: (u32, u32),
    open: char,
    close: char,
    forward: bool,
) -> Option<(u32, u32)> {
    let mut depth = 0i32;
    let mut row = at.0 as i64;
    let mut from = Some(at.1);
    loop {
        let text = row_text(lines, row as u32);
        let mut chars: Vec<(u32, char)> = text.char_indices().map(|(i, c)| (i as u32, c)).collect();
        if !forward {
            chars.reverse();
        }
        for (i, c) in chars {
            match from {
                Some(f) if forward && i <= f => continue,
                Some(f) if !forward && i >= f => continue,
                _ => {}
            }
            if forward {
                if c == open {
                    depth += 1;
                } else if c == close {
                    if depth == 0 {
                        return Some((row as u32, i));
                    }
                    depth -= 1;
                }
            } else if c == close {
                depth += 1;
            } else if c == open {
                if depth == 0 {
                    return Some((row as u32, i));
                }
                depth -= 1;
            }
        }
        from = None;
        row += if forward { 1 } else { -1 };
        if row < 0 || row > last_row(lines) as i64 {
            return None;
        }
    }
}

/// The word under the cursor, for `*` and `#`.
///
/// The one *at or after* the cursor on its row, which is Vim's rule: pressing `*` in the leading
/// indent of a line searches for the first word on it rather than doing nothing.
pub fn word_under(lines: &[String], at: (u32, u32)) -> Option<String> {
    let (row, col) = clamp(lines, at);
    let text = row_text(lines, row);
    let here = classes(text, false);
    let i = here.iter().position(|(o, c)| *o >= col && *c == 2)?;
    let mut a = i;
    let mut b = i;
    while a > 0 && here[a - 1].1 == 2 {
        a -= 1;
    }
    while b + 1 < here.len() && here[b + 1].1 == 2 {
        b += 1;
    }
    let end = here.get(b + 1).map(|(o, _)| *o as usize).unwrap_or(text.len());
    Some(text[here[a].0 as usize..end].to_string())
}

/// The text between two positions, `to` exclusive.
///
/// The counterpart to the core's `text::slice`, over plain rows: reading resolves its own motions,
/// so it has to be able to say what a pair of them covers without going back through a buffer.
pub fn slice(lines: &[String], from: (u32, u32), to: (u32, u32)) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let (from, to) = if from <= to { (from, to) } else { (to, from) };
    let cut = |row: u32, a: u32, b: u32| -> String {
        let text = row_text(lines, row);
        let a = (a as usize).min(text.len());
        let b = (b as usize).clamp(a, text.len());
        text[a..b].to_string()
    };
    if from.0 == to.0 {
        return cut(from.0, from.1, to.1);
    }
    let mut out = cut(from.0, from.1, u32::MAX);
    for row in from.0 + 1..to.0 {
        out.push('\n');
        out.push_str(row_text(lines, row));
    }
    out.push('\n');
    out.push_str(&cut(to.0, 0, to.1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|r| (*r).to_string()).collect()
    }

    fn go(lines: &[String], at: (u32, u32), m: Motion) -> (u32, u32) {
        resolve(lines, at, m, None).at
    }

    // ---- the cursor is on a character -------------------------------------

    /// The one thing that separates these motions from a text field's, and the reason they exist.
    ///
    /// `$` in a field is the space *after* the last character, because that is where the next thing
    /// you type goes. In normal mode it is the last character itself — and with a selection that
    /// includes the character the cursor is on, the two conventions differ by exactly the one
    /// character you were trying to copy.
    #[test]
    fn the_end_of_a_line_is_its_last_character_not_the_space_after_it() {
        let b = buf(&["abc"]);
        assert_eq!(go(&b, (0, 0), Motion::LineEnd), (0, 2), "on the `c`, not past it");
        assert_eq!(clamp(&b, (0, 9)), (0, 2), "and nothing can park past it either");
        assert_eq!(clamp(&buf(&[""]), (0, 4)), (0, 0), "an empty row is column zero");
    }

    /// `h` and `l` stay on their row. A field's `←` wraps because the character before the first
    /// one really is the newline; here, a motion that quietly changes which line an operator is
    /// about is a motion you cannot use with one.
    #[test]
    fn horizontal_motion_does_not_leave_the_row() {
        let b = buf(&["ab", "cd"]);
        assert_eq!(go(&b, (1, 0), Motion::Left), (1, 0), "stops dead at column zero");
        assert_eq!(go(&b, (0, 1), Motion::Right), (0, 1), "and at the last character");
    }

    #[test]
    fn vertical_motion_remembers_the_column_it_wanted() {
        let b = buf(&["aaaaa", "b", "ccccc"]);
        let down = resolve(&b, (0, 4), Motion::Down, None);
        assert_eq!(down.at, (1, 0), "clamped to the short row");
        let again = resolve(&b, down.at, Motion::Down, down.goal);
        assert_eq!(again.at, (2, 4), "and back out to where it started");
    }

    /// `$` remembers "the end", not a column — so it walks the right-hand edge of ragged text.
    #[test]
    fn dollar_then_down_lands_on_each_row_s_own_end() {
        let b = buf(&["aaaaa", "bb", "ccccccc"]);
        let end = resolve(&b, (0, 0), Motion::LineEnd, None);
        assert_eq!(end.at, (0, 4));
        let down = resolve(&b, end.at, Motion::Down, end.goal);
        assert_eq!(down.at, (1, 1), "the end of the short row");
        let down = resolve(&b, down.at, Motion::Down, down.goal);
        assert_eq!(down.at, (2, 6), "and the end of the long one, not two columns into it");
    }

    // ---- words -------------------------------------------------------------

    #[test]
    fn word_motion_treats_punctuation_as_its_own_word() {
        let b = buf(&["foo.bar baz"]);
        assert_eq!(go(&b, (0, 0), Motion::WordRight { big: false }), (0, 3), "onto the dot");
        assert_eq!(go(&b, (0, 3), Motion::WordRight { big: false }), (0, 4), "and off it again");
        // A big word is everything that is not a space, which is the whole point of having both.
        assert_eq!(go(&b, (0, 0), Motion::WordRight { big: true }), (0, 8), "straight to `baz`");
    }

    #[test]
    fn word_end_lands_on_the_last_letter_and_word_back_on_the_first() {
        let b = buf(&["alpha beta"]);
        assert_eq!(go(&b, (0, 0), Motion::WordEnd { big: false }), (0, 4), "the `a` of alpha");
        assert_eq!(go(&b, (0, 4), Motion::WordEnd { big: false }), (0, 9), "then the `a` of beta");
        assert_eq!(go(&b, (0, 9), Motion::WordLeft { big: false }), (0, 6), "back to `beta`");
        assert_eq!(go(&b, (0, 6), Motion::WordEndBack { big: false }), (0, 4), "and `ge` to alpha's end");
    }

    /// An empty row is a word of its own — Vim's rule, and what makes `w` walk *out* of a paragraph
    /// rather than skipping the gap after it as if it were not there.
    #[test]
    fn word_motion_crosses_rows_and_stops_on_a_blank_one() {
        let b = buf(&["one", "", "two"]);
        assert_eq!(go(&b, (0, 0), Motion::WordRight { big: false }), (1, 0));
        assert_eq!(go(&b, (1, 0), Motion::WordRight { big: false }), (2, 0));
        assert_eq!(go(&b, (2, 0), Motion::WordLeft { big: false }), (1, 0));
    }

    // ---- f / t / % ---------------------------------------------------------

    #[test]
    fn find_lands_on_the_character_and_till_lands_beside_it() {
        let b = buf(&["a(b)c"]);
        assert_eq!(go(&b, (0, 0), Motion::Find { ch: ')', till: false, back: false }), (0, 3));
        assert_eq!(go(&b, (0, 0), Motion::Find { ch: ')', till: true, back: false }), (0, 2));
        assert_eq!(go(&b, (0, 4), Motion::Find { ch: '(', till: false, back: true }), (0, 1));
        assert_eq!(go(&b, (0, 4), Motion::Find { ch: '(', till: true, back: true }), (0, 2));
        // Not there is not an error, and above all is not a move to somewhere else.
        let miss = resolve(&b, (0, 0), Motion::Find { ch: 'z', till: false, back: false }, None);
        assert!(!miss.moved, "a character that is not on the row moves nothing");
    }

    #[test]
    fn percent_matches_across_rows_and_counts_nesting() {
        let b = buf(&["fn a() {", "    if b() {", "    }", "}"]);
        assert_eq!(go(&b, (0, 7), Motion::MatchPair), (3, 0), "the brace that closes the function");
        assert_eq!(go(&b, (3, 0), Motion::MatchPair), (0, 7), "and back again");
        // From anywhere on the row, not just from the bracket: `%` finds the first one after the
        // cursor, which is what makes it usable from the start of a line.
        assert_eq!(go(&b, (0, 0), Motion::MatchPair), (0, 5), "the `(` closes at the `)`");
    }

    // ---- text objects ------------------------------------------------------

    #[test]
    fn iw_is_the_word_and_aw_takes_the_space_after_it() {
        let b = buf(&["one two three"]);
        let inner = object(&b, (0, 5), Object::Word { big: false }, false).expect("in a word");
        assert_eq!(inner, ((0, 4), (0, 6)), "`two`, both ends inclusive");
        let around = object(&b, (0, 5), Object::Word { big: false }, true).expect("in a word");
        assert_eq!(around, ((0, 4), (0, 7)), "and the space after it");
    }

    #[test]
    fn quotes_and_brackets_name_what_is_inside_them() {
        let b = buf(&["say(\"hello there\")"]);
        let inner = object(&b, (0, 8), Object::Quote('"'), false).expect("inside the string");
        assert_eq!(&"say(\"hello there\")"[inner.0.1 as usize..=inner.1.1 as usize], "hello there");
        let around = object(&b, (0, 8), Object::Quote('"'), true).expect("inside the string");
        assert_eq!(&"say(\"hello there\")"[around.0.1 as usize..=around.1.1 as usize], "\"hello there\"");
        let pair = object(&b, (0, 8), Object::Pair('(', ')'), false).expect("inside the call");
        assert_eq!(&"say(\"hello there\")"[pair.0.1 as usize..=pair.1.1 as usize], "\"hello there\"");
    }

    /// Nothing is the right answer when the cursor is not in one. Guessing at the nearest is how
    /// `ci"` eats the wrong string.
    #[test]
    fn a_text_object_the_cursor_is_not_in_is_no_object_at_all() {
        let b = buf(&["plain text"]);
        assert!(object(&b, (0, 3), Object::Quote('"'), false).is_none());
        assert!(object(&b, (0, 3), Object::Pair('{', '}'), true).is_none());
    }

    #[test]
    fn a_paragraph_is_what_is_between_two_blank_rows() {
        let b = buf(&["a", "b", "", "c"]);
        assert_eq!(object(&b, (0, 0), Object::Paragraph, false), Some(((0, 0), (1, 0))));
        assert_eq!(object(&b, (0, 0), Object::Paragraph, true), Some(((0, 0), (2, 0))), "and the gap");
    }

    // ---- graphemes ---------------------------------------------------------

    /// Every position that leaves here is a byte offset on a cluster boundary, including the ones
    /// word classification produced — a combining mark is not a character class and stopping on one
    /// would put the cursor between a letter and its accent.
    #[test]
    fn every_position_lands_on_a_grapheme_boundary() {
        let b = buf(&["ae\u{301}b x"]);
        let text = &b[0];
        for at in [(0, 0), (0, 1), (0, 4)] {
            for m in [
                Motion::Right,
                Motion::Left,
                Motion::LineEnd,
                Motion::WordRight { big: false },
                Motion::WordEnd { big: false },
            ] {
                let to = go(&b, at, m);
                assert!(text.is_char_boundary(to.1 as usize), "{m:?} from {at:?} → {to:?}");
                assert_eq!(snap(text, to.1 as usize), to.1, "{m:?} from {at:?} is on a cluster");
            }
        }
    }

    #[test]
    fn slicing_two_positions_is_the_text_between_them() {
        let b = buf(&["hello", "world"]);
        assert_eq!(slice(&b, (0, 1), (0, 4)), "ell");
        assert_eq!(slice(&b, (0, 3), (1, 2)), "lo\nwo");
        assert_eq!(slice(&b, (1, 2), (0, 3)), "lo\nwo", "either way round");
    }

    #[test]
    fn the_word_under_the_cursor_is_the_next_one_when_there_is_none() {
        let b = buf(&["   hello world"]);
        assert_eq!(word_under(&b, (0, 0)).as_deref(), Some("hello"), "from the indent");
        assert_eq!(word_under(&b, (0, 10)).as_deref(), Some("world"));
        assert_eq!(word_under(&buf(&["   "]), (0, 0)), None);
    }
}
