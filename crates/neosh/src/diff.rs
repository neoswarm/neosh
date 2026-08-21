//! Line diffs for the transcript.
//!
//! Small and deliberately unclever. What gets diffed here is the `old_string` and `new_string` of
//! an edit, or the before-and-after text an agent reports for a file — a few dozen lines, almost
//! always with a long common prefix and suffix. So: trim the ends that match, and run a plain LCS
//! on what is left. A real diff library earns its weight on whole repositories; in the middle of a
//! chat it would be a dependency for the sake of a few hundred cells of table.

/// What happened to one line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Keep,
    Add,
    Del,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Line {
    pub op: Op,
    pub text: String,
}

#[derive(Default, Clone, PartialEq, Eq, Debug)]
pub struct Diff {
    pub lines: Vec<Line>,
    pub added: usize,
    pub removed: usize,
}

/// A row of a diff with its unchanged stretches folded away.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Row {
    Line(Line),
    /// This many unchanged lines are not shown.
    Gap(usize),
}

/// Above this many table cells the two sides are treated as wholly different rather than aligned.
///
/// The bound is about not stalling the host loop on a pathological input — two thousand lines each
/// side is already more than anyone reads in a card — and a wholesale replace is still a correct
/// diff, merely a less pretty one.
const MAX_CELLS: usize = 4_000_000;

/// Read a unified diff, which is already the answer.
///
/// Some agents report an edit as before-and-after text and some report a patch. `codex` reports a
/// patch, and reconstructing two files from it in order to diff them again would be doing the work
/// twice and getting a worse answer the second time — the patch says exactly which lines moved,
/// and that is the whole content of a [`Diff`].
///
/// Everything outside a hunk is skipped: `---`, `+++`, `diff --git`, `index`. A `\ No newline at
/// end of file` marker is skipped too, since it describes the line above rather than being one.
/// Anything before the first `@@` is header whatever it looks like, which is what keeps a `+++`
/// line out of the added count.
pub fn unified(patch: &str) -> Diff {
    let mut out = Diff::default();
    let mut in_hunk = false;
    for line in patch.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk || line.starts_with('\\') {
            continue;
        }
        let (op, text) = match line.as_bytes().first() {
            Some(b'+') => (Op::Add, &line[1..]),
            Some(b'-') => (Op::Del, &line[1..]),
            Some(b' ') => (Op::Keep, &line[1..]),
            // A bare empty line inside a hunk is a context line whose single space was trimmed on
            // the way here, which every second tool does. Treating it as the end of the hunk would
            // drop everything after it.
            None => (Op::Keep, line),
            // Anything else ends the hunk: a second `diff --git` header, trailing prose.
            _ => {
                in_hunk = false;
                continue;
            }
        };
        match op {
            Op::Add => out.added += 1,
            Op::Del => out.removed += 1,
            Op::Keep => {}
        }
        out.lines.push(Line { op, text: text.to_string() });
    }
    out
}

/// Diff two texts line by line.
pub fn lines(old: &str, new: &str) -> Diff {
    let a = split(old);
    let b = split(new);

    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }

    let mut out: Vec<Line> = Vec::with_capacity(a.len().max(b.len()));
    out.extend(a[..pre].iter().map(|t| keep(t)));
    out.extend(align(&a[pre..a.len() - suf], &b[pre..b.len() - suf]));
    out.extend(a[a.len() - suf..].iter().map(|t| keep(t)));

    let added = out.iter().filter(|l| l.op == Op::Add).count();
    let removed = out.iter().filter(|l| l.op == Op::Del).count();
    Diff { lines: out, added, removed }
}

/// Fold unchanged runs longer than `2 * context` into a [`Row::Gap`], leaving `context` lines of
/// them on either side of every change.
///
/// Unchanged lines at the very start and end are kept only up to `context` too: they were shown
/// to say where a change sits, and a change at line forty of an unchanged fifty says that with the
/// two lines beside it.
pub fn fold(diff: &Diff, context: usize) -> Vec<Row> {
    let n = diff.lines.len();
    // Which rows to keep: any change, and `context` rows either side of it.
    let mut keep_row = vec![false; n];
    for (i, l) in diff.lines.iter().enumerate() {
        if l.op == Op::Keep {
            continue;
        }
        let from = i.saturating_sub(context);
        let to = (i + context + 1).min(n);
        for k in keep_row.iter_mut().take(to).skip(from) {
            *k = true;
        }
    }
    let mut out = Vec::new();
    let mut hidden = 0usize;
    for (i, l) in diff.lines.iter().enumerate() {
        if keep_row[i] {
            if hidden > 0 {
                out.push(Row::Gap(hidden));
                hidden = 0;
            }
            out.push(Row::Line(l.clone()));
        } else {
            hidden += 1;
        }
    }
    if hidden > 0 {
        out.push(Row::Gap(hidden));
    }
    out
}

fn split(text: &str) -> Vec<&str> {
    if text.is_empty() { Vec::new() } else { text.lines().collect() }
}

fn keep(text: &str) -> Line {
    Line { op: Op::Keep, text: text.to_string() }
}

/// LCS alignment of the two middles. Falls back to "all of one, then all of the other" when the
/// table would be too big, which is still a correct diff.
fn align(a: &[&str], b: &[&str]) -> Vec<Line> {
    let (n, m) = (a.len(), b.len());
    let mut out = Vec::with_capacity(n + m);
    if n == 0 || m == 0 || (n + 1).saturating_mul(m + 1) > MAX_CELLS {
        out.extend(a.iter().map(|t| Line { op: Op::Del, text: t.to_string() }));
        out.extend(b.iter().map(|t| Line { op: Op::Add, text: t.to_string() }));
        return out;
    }
    let w = m + 1;
    let mut t = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i * w + j] = if a[i] == b[j] {
                t[(i + 1) * w + j + 1] + 1
            } else {
                t[(i + 1) * w + j].max(t[i * w + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(keep(a[i]));
            i += 1;
            j += 1;
        } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
            out.push(Line { op: Op::Del, text: a[i].to_string() });
            i += 1;
        } else {
            out.push(Line { op: Op::Add, text: b[j].to_string() });
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|t| Line { op: Op::Del, text: t.to_string() }));
    out.extend(b[j..].iter().map(|t| Line { op: Op::Add, text: t.to_string() }));
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_unified_patch_is_read_rather_than_reconstructed() {
        // What `codex` sends for an edit. Reconstructing the two files from this and diffing them
        // again would be doing the work twice and getting a worse answer: the patch already says
        // exactly which lines moved.
        let d = unified(
            "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,4 +1,4 @@\n fn main() {\n-    println!(\"hi\");\n+    println!(\"hello\");\n+    // and again\n }\n",
        );
        assert_eq!(d.added, 2, "the `+++` header is not an added line");
        assert_eq!(d.removed, 1);
        assert_eq!(d.lines.first().map(|l| l.op), Some(Op::Keep));
        assert_eq!(d.lines[1], Line { op: Op::Del, text: "    println!(\"hi\");".into() });
    }

    #[test]
    fn a_patch_with_nothing_in_it_counts_nothing() {
        // Headers only, which is what a rename or a mode change looks like.
        let d = unified("diff --git a/x b/y\nsimilarity index 100%\nrename from x\nrename to y\n");
        assert_eq!((d.added, d.removed), (0, 0));
        assert!(d.lines.is_empty());
    }
    use super::*;

    fn ops(d: &Diff) -> String {
        d.lines
            .iter()
            .map(|l| match l.op {
                Op::Keep => ' ',
                Op::Add => '+',
                Op::Del => '-',
            })
            .collect()
    }

    #[test]
    fn a_changed_line_in_the_middle_is_one_removal_and_one_addition() {
        let d = lines("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(ops(&d), " -+ ");
        assert_eq!((d.added, d.removed), (1, 1));
    }

    #[test]
    fn identical_texts_change_nothing() {
        let d = lines("x\ny", "x\ny");
        assert_eq!(ops(&d), "  ");
        assert_eq!((d.added, d.removed), (0, 0));
    }

    #[test]
    fn an_empty_old_side_is_all_additions_and_not_one_blank_removal() {
        let d = lines("", "one\ntwo");
        assert_eq!(ops(&d), "++");
        assert_eq!(d.removed, 0, "\"\" is no lines, not one empty line");
    }

    #[test]
    fn an_insertion_keeps_its_neighbours() {
        let d = lines("a\nc", "a\nb\nc");
        assert_eq!(ops(&d), " + ");
        assert_eq!(d.lines[1].text, "b");
    }

    #[test]
    fn a_moved_block_aligns_on_the_longest_common_run() {
        let d = lines("1\n2\n3\n4", "3\n4\n1\n2");
        // Either of two alignments is acceptable; both have two adds and two dels.
        assert_eq!((d.added, d.removed), (2, 2));
    }

    #[test]
    fn folding_leaves_context_either_side_and_hides_the_rest() {
        let old: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l10\n", "L10\n");
        let rows = fold(&lines(&old, &new), 2);
        assert!(matches!(rows.first(), Some(Row::Gap(7))), "{rows:?}");
        assert!(matches!(rows.last(), Some(Row::Gap(8))), "{rows:?}");
        let shown: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Line(l) => Some(l.text.as_str()),
                Row::Gap(_) => None,
            })
            .collect();
        assert_eq!(shown, vec!["l8", "l9", "l10", "L10", "l11", "l12"]);
    }

    #[test]
    fn folding_a_diff_with_no_changes_is_one_gap() {
        let rows = fold(&lines("a\nb", "a\nb"), 2);
        assert_eq!(rows, vec![Row::Gap(2)]);
    }

    #[test]
    fn an_enormous_pair_still_comes_back_as_a_diff() {
        let a: String = (0..3000).map(|i| format!("a{i}\n")).collect();
        let b: String = (0..3000).map(|i| format!("b{i}\n")).collect();
        let d = lines(&a, &b);
        assert_eq!((d.added, d.removed), (3000, 3000));
    }
}
