//! Markdown for a transcript that is still being written.
//!
//! # The constraint this is built around
//!
//! An answer arrives a few characters at a time, and the whole thing has to look right after every
//! one of them. Re-parsing the entire answer per token is quadratic in the length of the answer,
//! which is fine for a paragraph and hopeless for the thousand-line one somebody actually cares
//! about — the tail of a long answer is exactly where it would start stuttering.
//!
//! So: **a block is settled once it cannot change.** A paragraph followed by a blank line, a
//! heading with its newline, a fence with its closing back-ticks. Settled blocks are rendered once
//! and never looked at again. Only the trailing partial block is re-rendered per tick, and it is
//! bounded by the size of one block rather than by the size of the answer.
//!
//! # What it renders, and what it does not
//!
//! Headings, lists, block quotes, rules, fenced and indented code, and the inline run of
//! `**bold**` / `*italic*` / `` `code` `` / `[text](url)`. Markers are removed rather than styled:
//! a terminal can show bold, so showing the asterisks *and* the bold is showing the same thing
//! twice.
//!
//! There is deliberately no syntax highlighting inside code fences. Doing it properly means a
//! grammar per language, and doing it improperly — a regex over keywords — colours the wrong words
//! in exactly the code you are reading closely. A fenced block gets one colour and its language in
//! the corner, which is honest about what is known.
//!
//! Tables are not rendered as tables. Column widths depend on the terminal, the content and the
//! wrap policy, and a table that reflows badly is harder to read than the pipes it came from.

/// One styled span of one rendered row: `(row, byte from, byte to, highlight group)`.
///
/// Rows are relative to the start of the patch; byte offsets are into the row's own text, because
/// that is what the extmark API takes and converting anywhere else would be a second place for
/// column maths to go wrong.
pub type Mark = (usize, usize, usize, &'static str);

/// Rows to replace, and how to colour them.
#[derive(Debug, Default, PartialEq)]
pub struct Patch {
    /// First row of the answer that this replaces. Everything from here to the end of the answer
    /// goes; `lines` takes its place.
    pub from: usize,
    pub lines: Vec<String>,
    pub marks: Vec<Mark>,
}

/// Render a complete piece of markdown in one go.
///
/// For a stored message, which is finished by definition. Goes through the same [`Md`] the live
/// stream does rather than a second renderer, because two renderers is how a reopened conversation
/// ends up not looking like the one you were just in.
pub fn render(text: &str) -> (Vec<String>, Vec<Mark>) {
    let mut md = Md::new();
    let mut lines: Vec<String> = Vec::new();
    let mut marks: Vec<Mark> = Vec::new();
    for patch in [md.push(text), md.finish()] {
        lines.truncate(patch.from);
        marks.retain(|m: &Mark| m.0 < patch.from);
        marks.extend(patch.marks.into_iter().map(|(r, a, b, g)| (r + patch.from, a, b, g)));
        lines.extend(patch.lines);
    }
    (lines, marks)
}

/// One answer, rendered as it arrives.
#[derive(Debug, Default)]
pub struct Md {
    /// Everything handed to us, verbatim. The source of truth — rendering is a pure function of
    /// this, so a bug in the incremental path can always be checked against a full re-render.
    raw: String,
    /// Bytes of `raw` that belong to blocks which can no longer change.
    settled_bytes: usize,
    /// Rows those settled blocks produced.
    settled_rows: usize,
    /// Whether an odd number of fence markers have been seen in the settled region. Carried, not
    /// recomputed: it is the one piece of parser state that spans a block boundary.
    in_fence: bool,
}

impl Md {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed more of the answer, and get back the rows that changed.
    pub fn push(&mut self, text: &str) -> Patch {
        self.raw.push_str(text);
        self.repaint(false)
    }

    /// The answer is complete: settle the trailing block even without a blank line after it.
    pub fn finish(&mut self) -> Patch {
        self.repaint(true)
    }

    fn repaint(&mut self, complete: bool) -> Patch {
        let from = self.settled_rows;
        let tail = &self.raw[self.settled_bytes..];

        // How much of the tail is finished. When the answer is over, all of it is.
        let done = if complete { tail.len() } else { settled_len(tail, self.in_fence) };

        let mut lines = Vec::new();
        let mut marks = Vec::new();
        let mut fence = self.in_fence;
        render_into(&tail[..done], &mut fence, &mut lines, &mut marks);
        let settled_rows = lines.len();

        // The unfinished remainder, drawn but not banked. Rendered with the same function as
        // everything else — a separate "partial" renderer is how the last paragraph of an answer
        // ends up looking different from the rest of it.
        let mut tail_fence = fence;
        render_into(&tail[done..], &mut tail_fence, &mut lines, &mut marks);

        self.settled_bytes += done;
        self.settled_rows += settled_rows;
        self.in_fence = fence;

        Patch { from, lines, marks }
    }
}

/// How many bytes from the start of `tail` belong to blocks that can no longer change.
///
/// A block ends at a blank line outside a fence, or at a fence's closing marker. Nothing else
/// counts: a heading is only settled once its newline has arrived, because until then the next
/// character could still be part of it.
fn settled_len(tail: &str, mut in_fence: bool) -> usize {
    let mut boundary = 0;
    let mut at = 0;
    for line in tail.split_inclusive('\n') {
        let complete = line.ends_with('\n');
        at += line.len();
        if !complete {
            break;
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if is_fence(trimmed) {
            in_fence = !in_fence;
            if !in_fence {
                boundary = at;
            }
            continue;
        }
        if !in_fence && trimmed.trim().is_empty() {
            boundary = at;
        }
    }
    boundary
}

fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// Render markdown into rows, carrying fence state in and out.
fn render_into(text: &str, in_fence: &mut bool, lines: &mut Vec<String>, marks: &mut Vec<Mark>) {
    if text.is_empty() {
        return;
    }
    // `split_inclusive` keeps a trailing partial line, which is exactly what a streaming tail is.
    for raw_line in text.split_inclusive('\n') {
        let line = raw_line.trim_end_matches(['\n', '\r']);
        let row = lines.len();

        if is_fence(line) {
            let language = line.trim_start().trim_start_matches(['`', '~']).trim();
            if *in_fence {
                *in_fence = false;
                // The closing marker is not drawn: it was punctuation for the parser.
                continue;
            }
            *in_fence = true;
            if language.is_empty() {
                continue;
            }
            let text = format!("  {language}");
            marks.push((row, 0, text.len(), "Markdown.Fence"));
            lines.push(text);
            continue;
        }

        if *in_fence {
            let text = format!("  {line}");
            marks.push((row, 0, text.len(), "Markdown.Code"));
            lines.push(text);
            continue;
        }

        lines.push(render_block_line(line, row, marks));
    }
}

/// One non-fenced line: its block shape, then its inline runs.
fn render_block_line(line: &str, row: usize, marks: &mut Vec<Mark>) -> String {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();

    // A thematic break. Drawn as one, at a fixed width the frontend will not wrap — the terminal's
    // width is not known here, and a rule that wraps is two rules.
    if is_rule(trimmed) {
        let text = "  \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}".to_string();
        marks.push((row, 0, text.len(), "Markdown.Rule"));
        return text;
    }

    // ATX heading. The hashes go: they were the markup, and a terminal can show a heading.
    if let Some(rest) = heading(trimmed) {
        let mut text = String::new();
        let body = inline(rest, row, text.len(), marks);
        text.push_str(&body);
        marks.push((row, 0, text.len(), "Markdown.Heading"));
        return text;
    }

    if let Some(rest) = trimmed.strip_prefix("> ").or_else(|| trimmed.strip_prefix('>')) {
        let bar = "\u{2502} ";
        let mut text = format!("{}{bar}", " ".repeat(indent));
        marks.push((row, indent, indent + bar.len(), "Markdown.Quote"));
        let at = text.len();
        text.push_str(&inline(rest, row, at, marks));
        return text;
    }

    if let Some((marker, rest)) = list_item(trimmed) {
        let mut text = format!("{}{marker}", " ".repeat(indent + 2));
        marks.push((row, indent + 2, text.len(), "Markdown.Bullet"));
        let at = text.len();
        text.push_str(&inline(rest, row, at, marks));
        return text;
    }

    let mut text = " ".repeat(indent);
    let at = text.len();
    text.push_str(&inline(trimmed, row, at, marks));
    text
}

fn is_rule(line: &str) -> bool {
    let t = line.trim_end();
    t.len() >= 3
        && (t.chars().all(|c| c == '-')
            || t.chars().all(|c| c == '*')
            || t.chars().all(|c| c == '_'))
}

/// The text of an ATX heading, if this is one.
fn heading(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&hashes).then(|| line[hashes..].trim_start()).filter(|r| !r.is_empty())
}

/// The bullet to draw and the text after it, if this is a list item.
fn list_item(line: &str) -> Option<(String, &str)> {
    for lead in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(lead) {
            return Some(("\u{2022} ".to_string(), rest));
        }
    }
    // `1. ` / `12) `. The number is kept: it is information, unlike a bullet character.
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 9 {
        let rest = &line[digits..];
        for lead in [". ", ") "] {
            if let Some(after) = rest.strip_prefix(lead) {
                return Some((format!("{}. ", &line[..digits]), after));
            }
        }
    }
    None
}

/// Inline runs: bold, italic, code, links. Returns the text with the markers removed.
///
/// `at` is where this text will start in the finished row, so the marks it produces are already in
/// the row's coordinates and the caller never has to shift them.
fn inline(text: &str, row: usize, at: usize, marks: &mut Vec<Mark>) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;

    while i < bytes.len() {
        // Code first: nothing inside back-ticks is markup, which is the whole point of them.
        if bytes[i] == b'`'
            && let Some(end) = find(text, i + 1, "`")
        {
            let from = at + out.len();
            out.push_str(&text[i + 1..end]);
            marks.push((row, from, at + out.len(), "Markdown.Code"));
            i = end + 1;
            continue;
        }
        if text[i..].starts_with("**")
            && let Some(end) = find(text, i + 2, "**")
        {
            let from = at + out.len();
            out.push_str(&inline(&text[i + 2..end], row, from, marks));
            marks.push((row, from, at + out.len(), "Markdown.Bold"));
            i = end + 2;
            continue;
        }
        // Single `*` only when it is not the start of `**` — checked above — and not a bare
        // asterisk with spaces around it, which is far more often multiplication than emphasis.
        if (bytes[i] == b'*' || bytes[i] == b'_')
            // Not the leftover half of an unclosed `**`: treating that as italic swallows both
            // asterisks and emphasises nothing, which is what half a bold run looks like on every
            // token of a stream.
            && !text[i..].starts_with("**")
            && !text[i..].starts_with("__")
            && bytes.get(i + 1).is_some_and(|c| !c.is_ascii_whitespace())
            && let Some(end) = find(text, i + 1, &text[i..i + 1])
        {
            let from = at + out.len();
            out.push_str(&inline(&text[i + 1..end], row, from, marks));
            marks.push((row, from, at + out.len(), "Markdown.Italic"));
            i = end + 1;
            continue;
        }
        // `[text](url)`. The label is kept and the target dropped: a URL in the middle of a
        // sentence costs more room than it is worth, and it is one keystroke away in the raw text.
        if bytes[i] == b'['
            && let Some(close) = find(text, i + 1, "]")
            && text[close + 1..].starts_with('(')
            && let Some(paren) = find(text, close + 2, ")")
        {
            let from = at + out.len();
            out.push_str(&text[i + 1..close]);
            marks.push((row, from, at + out.len(), "Markdown.Link"));
            i = paren + 1;
            continue;
        }

        // Advance by a whole character. Slicing at a byte that is not a boundary panics, and a
        // multi-byte character in an answer is the ordinary case rather than the exotic one.
        let step = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&text[i..i + step]);
        i += step;
    }
    out
}

/// The next occurrence of `needle` at or after `from`, as a byte offset into `text`.
fn find(text: &str, from: usize, needle: &str) -> Option<usize> {
    text.get(from..).and_then(|rest| rest.find(needle)).map(|at| from + at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the transcript ends up holding: rows, and the marks that survived on them.
    ///
    /// Applying a patch means dropping everything from `from` onward — rows *and* their marks — and
    /// putting the patch in its place. The host does exactly this, so a helper that did anything
    /// else would be testing a different program.
    #[derive(Default)]
    struct Rendered {
        lines: Vec<String>,
        marks: Vec<Mark>,
    }

    impl Rendered {
        fn apply(&mut self, p: Patch) {
            self.lines.truncate(p.from);
            self.marks.retain(|m| m.0 < p.from);
            self.marks.extend(p.marks.into_iter().map(|(r, a, b, g)| (r + p.from, a, b, g)));
            self.lines.extend(p.lines);
        }
    }

    fn all(text: &str) -> Rendered {
        let mut md = Md::new();
        let mut out = Rendered::default();
        out.apply(md.push(text));
        out.apply(md.finish());
        out
    }

    fn lines(text: &str) -> Vec<String> {
        all(text).lines
    }

    fn groups(text: &str) -> Vec<&'static str> {
        all(text).marks.into_iter().map(|m| m.3).collect()
    }

    #[test]
    fn a_heading_loses_its_hashes_and_keeps_its_words() {
        assert_eq!(lines("## Getting started\n"), vec!["Getting started"]);
        assert!(groups("## Getting started\n").contains(&"Markdown.Heading"));
        // Seven hashes is not a heading in any dialect, and a line starting with them is text.
        assert_eq!(lines("####### nope\n"), vec!["####### nope"]);
    }

    #[test]
    fn a_bullet_becomes_a_bullet() {
        assert_eq!(lines("- one\n- two\n"), vec!["  \u{2022} one", "  \u{2022} two"]);
    }

    #[test]
    fn a_numbered_list_keeps_its_numbers() {
        // The number is information; the bullet character is not. Renumbering silently would make
        // an answer that says "see step 3" wrong.
        assert_eq!(lines("1. first\n7. seventh\n"), vec!["  1. first", "  7. seventh"]);
    }

    #[test]
    fn a_fence_shows_its_language_and_drops_its_backticks() {
        let got = lines("```rust\nfn main() {}\n```\n");
        assert_eq!(got, vec!["  rust", "  fn main() {}"]);
        assert!(groups("```rust\nlet x = 1;\n```\n").contains(&"Markdown.Code"));
    }

    #[test]
    fn nothing_inside_a_fence_is_markup() {
        // The classic failure: a shell snippet full of `*` and `#` rendered as headings and italics.
        let got = lines("```\n# not a heading\nls *.rs\n```\n");
        assert_eq!(got, vec!["  # not a heading", "  ls *.rs"]);
    }

    #[test]
    fn inline_markers_are_removed_rather_than_shown() {
        assert_eq!(lines("a **bold** word\n"), vec!["a bold word"]);
        assert_eq!(lines("a `code` word\n"), vec!["a code word"]);
        assert_eq!(lines("a *slanted* word\n"), vec!["a slanted word"]);
        assert_eq!(lines("see [the docs](https://example.invalid)\n"), vec!["see the docs"]);
    }

    #[test]
    fn a_mark_covers_the_text_it_styles_and_not_the_markers() {
        let p = all("say **this** now\n");
        assert_eq!(p.lines, vec!["say this now"]);
        let bold = p.marks.iter().find(|m| m.3 == "Markdown.Bold").expect("bold was marked");
        assert_eq!(&p.lines[bold.0][bold.1..bold.2], "this");
    }

    #[test]
    fn a_marker_inside_backticks_stays_literal() {
        assert_eq!(lines("use `a * b` here\n"), vec!["use a * b here"]);
    }

    #[test]
    fn an_unclosed_marker_is_left_alone() {
        // Half a bold run is what every stream looks like for a moment. Rendering it as bold and
        // then un-bolding it when the next character arrives is a flicker on every token.
        assert_eq!(lines("this is **not closed\n"), vec!["this is **not closed"]);
    }

    #[test]
    fn multiplication_is_not_emphasis() {
        assert_eq!(lines("2 * 3 * 4\n"), vec!["2 * 3 * 4"]);
    }

    #[test]
    fn a_multibyte_answer_does_not_split_a_character() {
        let got = lines("日本語 **太字** \u{1f389}\n");
        assert_eq!(got, vec!["日本語 太字 \u{1f389}"]);
    }

    // ---- the incremental half ---------------------------------------------

    #[test]
    fn streaming_a_character_at_a_time_ends_up_where_one_shot_does() {
        // The property that matters: whatever the incremental path does, it has to agree with a
        // full re-render, because the full re-render is what a reopened conversation shows.
        let source = "# Title\n\nSome **bold** text.\n\n- one\n- two\n\n```rust\nfn f() {}\n```\n\nEnd.\n";
        let mut md = Md::new();
        let mut got = Rendered::default();
        for c in source.chars() {
            got.apply(md.push(&c.to_string()));
        }
        got.apply(md.finish());
        let want = all(source);
        assert_eq!(got.lines, want.lines);
        assert_eq!(got.marks, want.marks, "and the colours land in the same places");
    }

    #[test]
    fn a_stored_message_renders_the_same_as_a_streamed_one() {
        // Switching conversations must not change what an answer looks like.
        let source = "## Heading\n\nA **word**.\n\n```sh\nls\n```\n";
        let (lines, marks) = render(source);
        let want = all(source);
        assert_eq!(lines, want.lines);
        assert_eq!(marks, want.marks);
    }

    #[test]
    fn a_finished_block_is_never_rendered_again() {
        // The whole reason this type exists. Without it, an answer costs O(n²) to display and the
        // stutter shows up exactly where answers are longest.
        let mut md = Md::new();
        md.push("first paragraph\n\n");
        let p = md.push("second");
        assert_eq!(p.from, 2, "the first paragraph and its blank line are behind us");
        assert_eq!(p.lines, vec!["second"], "and only the new block is redrawn");
    }

    #[test]
    fn an_open_fence_holds_the_boundary_until_it_closes() {
        // A blank line inside a fence is not a block boundary. Settling there would let the rest of
        // the code block be re-parsed as prose.
        let mut md = Md::new();
        md.push("```\nfirst\n\nsecond\n");
        let p = md.push("third\n");
        assert_eq!(p.from, 0, "nothing has been settled yet");
        assert!(p.lines.iter().any(|l| l.contains("third")));
        assert!(p.lines.iter().all(|l| l.starts_with("  ")), "all of it is still code: {:?}", p.lines);
    }

    #[test]
    fn a_partial_line_is_drawn_before_its_newline_arrives() {
        // Otherwise the answer appears one line behind what has actually been said, which reads as
        // lag on every single line.
        let mut md = Md::new();
        let p = md.push("half a sen");
        assert_eq!(p.lines, vec!["half a sen"]);
    }

    #[test]
    fn finishing_settles_a_block_with_no_blank_line_after_it() {
        let mut md = Md::new();
        md.push("the end");
        let p = md.finish();
        assert_eq!(p.lines, vec!["the end"]);
        assert_eq!(md.finish(), Patch { from: 1, lines: vec![], marks: vec![] }, "and it stays settled");
    }
}
