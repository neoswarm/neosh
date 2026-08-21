//! The word at the top of an empty transcript.
//!
//! Block letters in the font every Neovim dashboard uses, because that is the shape people mean
//! when they say "the logo", and lit like brushed metal: a vertical grey ramp across the faces of
//! the letters, bright near the top edge and falling off downward, with the box-drawing shadow a
//! single dark grey underneath. Greys rather than the accent hue, so the one coloured thing on the
//! welcome screen is still the key that does something.
//!
//! Colour lives in the palette (`Logo.Face0` … `Logo.Face5`, `Logo.Edge`) and is down-converted at
//! the terminal like every other group — on a 256-colour terminal the ramp lands on the grey
//! steps, on sixteen colours it is white and grey, and on `ui.ascii_only` the word is set in plain
//! text because half of these glyphs would be boxes.

/// The letters. Six rows, forty-three columns.
pub const ROWS: [&str; 6] = [
    "\u{2588}\u{2588}\u{2588}\u{2557}   \u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2557}  \u{2588}\u{2588}\u{2557}",
    "\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}  \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d}\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}",
    "\u{2588}\u{2588}\u{2554}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}  \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}",
    "\u{2588}\u{2588}\u{2551}\u{255a}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{255d}  \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}\u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2551}",
    "\u{2588}\u{2588}\u{2551} \u{255a}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}\u{255a}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255d}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}",
    "\u{255a}\u{2550}\u{255d}  \u{255a}\u{2550}\u{2550}\u{2550}\u{255d}\u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d} \u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d} \u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}  \u{255a}\u{2550}\u{255d}",
];

/// Columns the word occupies. Every glyph in it is one cell wide.
pub const WIDTH: usize = 43;

/// The face groups, top row first.
pub const FACES: [&str; 6] =
    ["Logo.Face0", "Logo.Face1", "Logo.Face2", "Logo.Face3", "Logo.Face4", "Logo.Face5"];

/// The shadow group.
pub const EDGE: &str = "Logo.Edge";

/// One row of the word, indented by `indent` columns, with the byte spans to colour.
///
/// The face of a letter is every run of `█`; everything else that is not a space is shadow. Spans
/// are byte offsets, as every column in this program is, and a `█` is three of them.
pub fn row(i: usize, indent: usize) -> (String, Vec<(usize, usize, &'static str)>) {
    let text = ROWS.get(i).copied().unwrap_or_default();
    let face = FACES.get(i).copied().unwrap_or(EDGE);
    let pad = " ".repeat(indent);
    let mut out = pad.clone();
    let mut spans: Vec<(usize, usize, &'static str)> = Vec::new();
    let mut run: Option<(usize, &'static str)> = None;
    for ch in text.chars() {
        let group = match ch {
            '\u{2588}' => Some(face),
            ' ' => None,
            _ => Some(EDGE),
        };
        let at = out.len();
        if run.map(|(_, g)| g) != group {
            if let Some((from, g)) = run.take() {
                spans.push((from, at, g));
            }
            if let Some(g) = group {
                run = Some((at, g));
            }
        }
        out.push(ch);
    }
    if let Some((from, g)) = run {
        spans.push((from, out.len(), g));
    }
    (out, spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn every_row_is_the_width_it_claims() {
        for r in ROWS {
            assert_eq!(r.width(), WIDTH, "{r}");
        }
    }

    #[test]
    fn faces_and_edges_are_coloured_separately_and_spaces_not_at_all() {
        let (text, spans) = row(0, 2);
        assert!(text.starts_with("  \u{2588}"));
        // The first run is face, the `╗` after it is edge, and the gap between N's legs has no span.
        assert_eq!(spans[0].2, "Logo.Face0");
        assert_eq!(spans[1].2, EDGE);
        assert_eq!(spans[0].1, spans[1].0, "the edge starts where the face stops");
        let covered: usize = spans.iter().map(|(a, b, _)| b - a).sum();
        let spaces = text.chars().filter(|c| *c == ' ').count();
        assert_eq!(covered + spaces, text.len(), "everything but the spaces is coloured");
    }

    #[test]
    fn a_row_that_does_not_exist_is_empty_rather_than_a_panic() {
        let (text, spans) = row(99, 0);
        assert!(text.is_empty());
        assert!(spans.is_empty());
    }
}
