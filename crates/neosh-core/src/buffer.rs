//! Buffers: text plus positional annotations.
//!
//! # Marks live on lines
//!
//! The obvious design is a sorted map keyed by `(row, col)` plus a fix-up pass on every edit that
//! shifts every mark below the change. That is O(marks) per edit, and since streaming output edits
//! the buffer once per token, it degrades exactly when the editor is busiest — the same failure the
//! naive design has, arriving as a performance cliff instead of a correctness bug.
//!
//! Here, a mark is stored *inside* the [`Line`] it annotates:
//!
//! ```text
//! Vec<Line>  where  Line { text: String, marks: SmallVec<[Mark; 1]> }
//! ```
//!
//! [`Buffer::set_lines`] is a `Vec::splice`. Marks on surviving lines move with their line for
//! free — there is no shift pass to get wrong, and "virtual text survives an append above it" is a
//! structural property rather than something maintained by careful code. A mark's row is simply its
//! line's index.
//!
//! The cost is that finding a mark by id is a scan. That is the right trade for an agent UI: the
//! renderer only ever asks "what marks are on the lines I am about to draw", which this layout
//! makes trivial, while by-id lookup is a rare plugin operation. When buffers grow large enough for
//! the scan to matter, the replacement is a marker tree — see `docs/adr/0007`.

use neosh_proto::{
    ApiError, BufferId, ExtmarkId, ExtmarkInfo, ExtmarkOpts, ExtmarkRender, LineRender,
    NamespaceId, OnDelete,
};
use smallvec::SmallVec;

/// A positional annotation attached to a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub id: ExtmarkId,
    pub ns: NamespaceId,
    /// UTF-8 byte offset into the line's text. Never a character or display column.
    pub col: u32,
    pub opts: ExtmarkOpts,
    /// Set when the text this mark annotated was deleted and its policy was
    /// [`OnDelete::Invalidate`]. Invalid marks stop rendering but remain queryable so the owning
    /// plugin can notice and clean up.
    pub invalid: bool,
}

impl Mark {
    fn render(&self) -> ExtmarkRender {
        ExtmarkRender { ns: self.ns, id: self.id, col: self.col, opts: self.opts.clone() }
    }
}

/// One line: its text, and everything annotating it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub marks: SmallVec<[Mark; 1]>,
}

impl Line {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), marks: SmallVec::new() }
    }

    fn render(&self) -> LineRender {
        LineRender {
            text: self.text.clone(),
            marks: self.marks.iter().filter(|m| !m.invalid).map(Mark::render).collect(),
        }
    }

    /// Clamp a byte offset to a valid UTF-8 boundary at or before `col`.
    ///
    /// Truncating mid-codepoint would produce a column the frontend cannot convert to a display
    /// position, so this always lands on a boundary.
    fn clamp_col(&self, col: u32) -> u32 {
        let len = self.text.len();
        let mut c = (col as usize).min(len);
        while c > 0 && !self.text.is_char_boundary(c) {
            c -= 1;
        }
        c as u32
    }
}

/// The result of a mutation, describing exactly the splice that was performed so the frontend can
/// replay it against its mirror.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineEdit {
    pub start: u32,
    pub old_end: u32,
    pub lines: Vec<LineRender>,
}

#[derive(Clone, Debug)]
pub struct Buffer {
    pub id: BufferId,
    pub name: String,
    /// What this buffer *is* — `neosh.sidebar`, `neosh.transcript`, somebody else's `acme.tasks`.
    ///
    /// A name rather than a flag because its whole purpose is to be said by one plugin and heard by
    /// another: it is what `KeymapScope::BufKind` binds against and what `win.list` reports, and so
    /// it is the difference between a panel a third party can extend and one they can only replace.
    /// Neovim calls this a filetype and hangs half its ecosystem off it.
    pub kind: Option<String>,
    /// A buffer always has at least one line, mirroring how every real editor models an empty file.
    lines: Vec<Line>,
    next_mark: u32,
}

impl Buffer {
    pub fn new(id: BufferId, name: impl Into<String>) -> Self {
        Self { id, name: name.into(), kind: None, lines: vec![Line::default()], next_mark: 1 }
    }

    pub fn line_count(&self) -> u32 {
        self.lines.len() as u32
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    pub fn text(&self) -> String {
        self.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    /// Resolve a possibly-negative index against the current line count.
    ///
    /// Negative indices are `len + 1 + i`, matching Neovim's `nvim_buf_set_lines`: **`-1` is the
    /// index one past the last line**, so `set_lines(0, -1, …)` replaces the whole buffer and
    /// `set_lines(-1, -1, …)` appends. To address the last line itself, use `-2..-1`.
    ///
    /// Reading `-1` as "the last line" instead is a subtle and destructive difference: a caller
    /// writing `set_lines(0, -1, [text])` to replace the buffer would silently *insert* on every
    /// call, and the buffer would grow by one line each time.
    pub fn resolve_index(&self, i: i64) -> u32 {
        let n = self.lines.len() as i64;
        let r = if i < 0 { n + 1 + i } else { i };
        r.clamp(0, n) as u32
    }

    pub fn get_lines(&self, start: u32, end: u32) -> Vec<String> {
        let start = (start as usize).min(self.lines.len());
        let end = (end as usize).clamp(start, self.lines.len());
        self.lines[start..end].iter().map(|l| l.text.clone()).collect()
    }

    /// Replace lines `[start, end)` with `new_text`.
    ///
    /// Mark survival rules, which exist so that streaming works:
    ///
    /// * Lines outside `[start, end)` are untouched; their marks move with them because they *are*
    ///   their marks' storage. This is what makes "append above" safe.
    /// * A replaced line that has a positional counterpart in `new_text` (same offset from `start`)
    ///   keeps its marks, with columns clamped into the new text. Appending a token to the last
    ///   line is exactly this case — without the rule, every mark on that line would die once per
    ///   token.
    /// * A replaced line with no counterpart is an orphan, and its marks follow their
    ///   [`OnDelete`] policy.
    pub fn set_lines(&mut self, start: u32, end: u32, new_text: Vec<String>) -> LineEdit {
        let len = self.lines.len();
        let start = (start as usize).min(len);
        let end = (end as usize).clamp(start, len);

        let mut new_lines: Vec<Line> = new_text.into_iter().map(Line::new).collect();
        let mut orphans: Vec<Mark> = Vec::new();

        // Carry marks from each replaced line onto its positional counterpart.
        for (offset, old) in self.lines[start..end].iter_mut().enumerate() {
            let marks = std::mem::take(&mut old.marks);
            match new_lines.get_mut(offset) {
                Some(dst) => {
                    for mut m in marks {
                        m.col = dst.clamp_col(m.col);
                        if let Some(ec) = m.opts.end_col {
                            m.opts.end_col = Some(dst.clamp_col(ec).max(m.col));
                        }
                        dst.marks.push(m);
                    }
                }
                None => orphans.extend(marks),
            }
        }

        let inserted = new_lines.len();
        self.lines.splice(start..end, new_lines);

        // A buffer is never truly empty.
        if self.lines.is_empty() {
            self.lines.push(Line::default());
        }

        if !orphans.is_empty() {
            let landing = start.min(self.lines.len().saturating_sub(1));
            let dst = &mut self.lines[landing];
            let mut carried: SmallVec<[Mark; 1]> = SmallVec::new();
            for mut m in orphans {
                match m.opts.on_delete {
                    OnDelete::Clamp => m.col = 0,
                    OnDelete::Invalidate => {
                        m.col = 0;
                        m.invalid = true;
                    }
                }
                m.opts.end_col = None;
                carried.push(m);
            }
            dst.marks.extend(carried);
        }

        // Report the region that actually changed. When orphans landed on a line outside the
        // spliced region we widen the report rather than emit a second event, so the frontend
        // applies one splice.
        let reported_start = start;
        let reported_new_end = (start + inserted).max(reported_start);
        let reported_new_end = reported_new_end.min(self.lines.len());
        LineEdit {
            start: reported_start as u32,
            old_end: end as u32,
            lines: self.lines[reported_start..reported_new_end].iter().map(Line::render).collect(),
        }
    }

    /// Append to the final line without resending it.
    ///
    /// The streaming fast path: a token arrives, one line is rewritten, and every mark on that line
    /// keeps its position.
    pub fn append_text(&mut self, text: &str) -> LineEdit {
        // Text may contain newlines; split so the buffer stays line-structured.
        let last = self.lines.len().saturating_sub(1);
        let mut combined = self.lines[last].text.clone();
        combined.push_str(text);
        let pieces: Vec<String> = combined.split('\n').map(str::to_string).collect();
        self.set_lines(last as u32, last as u32 + 1, pieces)
    }

    // ---- marks ---------------------------------------------------------

    pub fn set_mark(
        &mut self,
        ns: NamespaceId,
        row: u32,
        col: u32,
        opts: ExtmarkOpts,
    ) -> Result<ExtmarkId, ApiError> {
        let row = row as usize;
        let n = self.lines.len();
        let line = self.lines.get_mut(row).ok_or_else(|| ApiError::InvalidArgument {
            message: format!("row {row} out of range (buffer has {n} lines)"),
        })?;
        let id = ExtmarkId(self.next_mark);
        self.next_mark += 1;
        let col = line.clamp_col(col);
        let mut opts = opts;
        if let Some(ec) = opts.end_col {
            opts.end_col = Some(line.clamp_col(ec).max(col));
        }
        line.marks.push(Mark { id, ns, col, opts, invalid: false });
        Ok(id)
    }

    /// Locate a mark. O(lines) by construction — see the module docs.
    pub fn get_mark(&self, ns: NamespaceId, id: ExtmarkId) -> Option<ExtmarkInfo> {
        self.lines.iter().enumerate().find_map(|(row, line)| {
            line.marks.iter().find(|m| m.id == id && m.ns == ns).map(|m| ExtmarkInfo {
                id: m.id,
                ns: m.ns,
                row: row as u32,
                col: m.col,
                invalid: m.invalid,
                opts: m.opts.clone(),
            })
        })
    }

    pub fn all_marks(&self, ns: NamespaceId) -> Vec<ExtmarkInfo> {
        self.lines
            .iter()
            .enumerate()
            .flat_map(|(row, line)| {
                line.marks.iter().filter(move |m| m.ns == ns).map(move |m| ExtmarkInfo {
                    id: m.id,
                    ns: m.ns,
                    row: row as u32,
                    col: m.col,
                    invalid: m.invalid,
                    opts: m.opts.clone(),
                })
            })
            .collect()
    }

    /// Remove a mark. Returns the row it was on, so the caller can emit a redraw for just that line.
    pub fn del_mark(&mut self, ns: NamespaceId, id: ExtmarkId) -> Option<u32> {
        for (row, line) in self.lines.iter_mut().enumerate() {
            if let Some(pos) = line.marks.iter().position(|m| m.id == id && m.ns == ns) {
                line.marks.remove(pos);
                return Some(row as u32);
            }
        }
        None
    }

    /// Clear a namespace's marks over a row range. Returns the affected range for redraw.
    pub fn clear_marks(&mut self, ns: NamespaceId, start: u32, end: u32) -> (u32, u32) {
        let start = (start as usize).min(self.lines.len());
        let end = (end as usize).clamp(start, self.lines.len());
        for line in &mut self.lines[start..end] {
            line.marks.retain(|m| m.ns != ns);
        }
        (start as u32, end as u32)
    }

    /// Render a row range for the wire.
    pub fn render_range(&self, start: u32, end: u32) -> Vec<LineRender> {
        let start = (start as usize).min(self.lines.len());
        let end = (end as usize).clamp(start, self.lines.len());
        self.lines[start..end].iter().map(Line::render).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{VirtChunk, VirtTextPos};

    fn vt(text: &str) -> ExtmarkOpts {
        ExtmarkOpts {
            virt_text: vec![VirtChunk { text: text.into(), hl_group: Some("Test".into()) }],
            virt_text_pos: VirtTextPos::Eol,
            ..Default::default()
        }
    }

    fn buf_with(lines: &[&str]) -> Buffer {
        let mut b = Buffer::new(BufferId(1), "test");
        b.set_lines(0, 1, lines.iter().map(|s| s.to_string()).collect());
        b
    }

    /// DoD item 3: virtual text must survive an append *above* it.
    #[test]
    fn mark_survives_insert_above() {
        let mut b = buf_with(&["alpha", "beta", "gamma"]);
        let ns = NamespaceId(1);
        let id = b.set_mark(ns, 2, 0, vt("<- gamma")).unwrap();
        assert_eq!(b.get_mark(ns, id).unwrap().row, 2);

        b.set_lines(0, 0, vec!["inserted".into()]);
        let info = b.get_mark(ns, id).expect("mark must survive an insert above it");
        assert_eq!(info.row, 3, "mark should shift down with its line");
        assert!(!info.invalid);
        assert_eq!(info.opts.virt_text[0].text, "<- gamma");
    }

    #[test]
    fn mark_survives_many_inserts_above() {
        let mut b = buf_with(&["target"]);
        let ns = NamespaceId(1);
        let id = b.set_mark(ns, 0, 0, vt("x")).unwrap();
        for i in 0..500 {
            b.set_lines(0, 0, vec![format!("line {i}")]);
        }
        assert_eq!(b.get_mark(ns, id).unwrap().row, 500);
    }

    #[test]
    fn mark_shifts_up_when_lines_above_are_deleted() {
        let mut b = buf_with(&["a", "b", "c", "d"]);
        let ns = NamespaceId(1);
        let id = b.set_mark(ns, 3, 0, vt("d")).unwrap();
        b.set_lines(0, 2, vec![]);
        assert_eq!(b.get_mark(ns, id).unwrap().row, 1);
    }

    /// DoD item 6: appending a token must not destroy marks on the line being appended to.
    #[test]
    fn mark_survives_streaming_append_to_same_line() {
        let mut b = buf_with(&["assistant: "]);
        let ns = NamespaceId(1);
        let id = b.set_mark(ns, 0, 0, vt("[streaming]")).unwrap();

        for tok in ["Hel", "lo", " wo", "rld"] {
            b.append_text(tok);
            let info = b.get_mark(ns, id).expect("mark must survive each token");
            assert!(!info.invalid);
            assert_eq!(info.row, 0);
        }
        assert_eq!(b.text(), "assistant: Hello world");
    }

    #[test]
    fn append_text_splits_on_newlines() {
        let mut b = buf_with(&["one"]);
        b.append_text(" two\nthree\nfour");
        assert_eq!(b.line_count(), 3);
        assert_eq!(b.get_lines(0, 3), vec!["one two", "three", "four"]);
    }

    #[test]
    fn replaced_line_keeps_marks_with_clamped_column() {
        let mut b = buf_with(&["a long line of text"]);
        let ns = NamespaceId(1);
        let id = b.set_mark(ns, 0, 15, vt("here")).unwrap();
        b.set_lines(0, 1, vec!["short".into()]);
        let info = b.get_mark(ns, id).unwrap();
        assert_eq!(info.col, 5, "column clamps to the new line length");
        assert!(!info.invalid);
    }

    #[test]
    fn orphaned_mark_respects_invalidate_policy() {
        let mut b = buf_with(&["a", "b", "c"]);
        let ns = NamespaceId(1);
        let mut o = vt("doomed");
        o.on_delete = OnDelete::Invalidate;
        let id = b.set_mark(ns, 1, 0, o).unwrap();

        b.set_lines(1, 2, vec![]);
        let info = b.get_mark(ns, id).expect("invalidated marks stay queryable");
        assert!(info.invalid, "policy was Invalidate");
    }

    #[test]
    fn orphaned_mark_respects_clamp_policy() {
        let mut b = buf_with(&["a", "b", "c"]);
        let ns = NamespaceId(1);
        let id = b.set_mark(ns, 1, 0, vt("kept")).unwrap(); // default policy is Clamp
        b.set_lines(1, 2, vec![]);
        let info = b.get_mark(ns, id).unwrap();
        assert!(!info.invalid, "policy was Clamp");
        assert_eq!(info.row, 1);
    }

    #[test]
    fn invalid_marks_are_not_rendered_but_valid_ones_are() {
        let mut b = buf_with(&["x", "y"]);
        let ns = NamespaceId(1);
        let mut o = vt("gone");
        o.on_delete = OnDelete::Invalidate;
        b.set_mark(ns, 1, 0, o).unwrap();
        b.set_mark(ns, 0, 0, vt("shown")).unwrap();
        b.set_lines(1, 2, vec![]);

        let rendered = b.render_range(0, b.line_count());
        let all: Vec<_> = rendered.iter().flat_map(|l| l.marks.iter()).collect();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].opts.virt_text[0].text, "shown");
    }

    #[test]
    fn columns_clamp_to_utf8_boundaries() {
        // "héllo" — the 'é' occupies bytes 1..3, so byte offset 2 is mid-codepoint.
        let mut b = buf_with(&["héllo"]);
        let ns = NamespaceId(1);
        let id = b.set_mark(ns, 0, 2, vt("mid")).unwrap();
        let col = b.get_mark(ns, id).unwrap().col;
        assert_eq!(col, 1, "must land on a char boundary, not split the codepoint");
        assert!(b.lines()[0].text.is_char_boundary(col as usize));
    }

    #[test]
    fn cjk_and_emoji_columns_stay_on_boundaries() {
        let mut b = buf_with(&["日本語 🎉 text"]);
        let ns = NamespaceId(1);
        for col in 0..30u32 {
            let id = b.set_mark(ns, 0, col, vt("m")).unwrap();
            let got = b.get_mark(ns, id).unwrap().col as usize;
            assert!(b.lines()[0].text.is_char_boundary(got), "col {col} -> {got} not a boundary");
        }
    }

    #[test]
    fn negative_indices_follow_the_neovim_rule() {
        let b = buf_with(&["a", "b", "c"]);
        assert_eq!(b.resolve_index(-1), 3, "-1 is one past the end, not the last line");
        assert_eq!(b.resolve_index(-2), 2, "-2 addresses the last line");
        assert_eq!(b.resolve_index(0), 0);
        assert_eq!(b.resolve_index(99), 3, "clamps to line count");
        assert_eq!(b.resolve_index(-99), 0);
    }

    /// The bug this rule exists to prevent: replacing the whole buffer must replace it, every time.
    #[test]
    fn replacing_the_whole_buffer_repeatedly_does_not_grow_it() {
        let mut b = buf_with(&["initial"]);
        for i in 0..5 {
            let (s, e) = (b.resolve_index(0), b.resolve_index(-1));
            b.set_lines(s, e, vec![format!("take {i}")]);
            assert_eq!(b.line_count(), 1, "buffer grew on iteration {i}: {:?}", b.text());
        }
        assert_eq!(b.text(), "take 4");
    }

    #[test]
    fn appending_at_minus_one_adds_to_the_end() {
        let mut b = buf_with(&["a", "b"]);
        let n = b.resolve_index(-1);
        b.set_lines(n, n, vec!["c".into()]);
        assert_eq!(b.get_lines(0, b.line_count()), vec!["a", "b", "c"]);
    }

    #[test]
    fn clearing_a_namespace_leaves_other_namespaces_alone() {
        let mut b = buf_with(&["a", "b"]);
        let (mine, theirs) = (NamespaceId(1), NamespaceId(2));
        let keep = b.set_mark(theirs, 0, 0, vt("theirs")).unwrap();
        b.set_mark(mine, 0, 0, vt("mine")).unwrap();
        b.clear_marks(mine, 0, b.line_count());
        assert!(b.all_marks(mine).is_empty());
        assert!(b.get_mark(theirs, keep).is_some());
    }

    #[test]
    fn buffer_never_becomes_empty() {
        let mut b = buf_with(&["only"]);
        b.set_lines(0, 1, vec![]);
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.get_lines(0, 1), vec![""]);
    }

    #[test]
    fn edit_report_describes_the_exact_splice() {
        let mut b = buf_with(&["a", "b", "c"]);
        let edit = b.set_lines(1, 2, vec!["x".into(), "y".into()]);
        assert_eq!(edit.start, 1);
        assert_eq!(edit.old_end, 2);
        assert_eq!(edit.lines.len(), 2);
        assert_eq!(edit.lines[0].text, "x");
        // Replaying the splice on a mirror must reproduce the buffer.
        let mut mirror = vec!["a".to_string(), "b".into(), "c".into()];
        mirror.splice(
            edit.start as usize..edit.old_end as usize,
            edit.lines.iter().map(|l| l.text.clone()),
        );
        assert_eq!(mirror, b.get_lines(0, b.line_count()));
    }
}
