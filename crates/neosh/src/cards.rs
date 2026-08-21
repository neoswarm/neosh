//! Tool cards: what a call was, and what it did.
//!
//! A card is a header row and, once the call has come back, a body under it. The header is a state
//! dot, the tool's name, its subject and — for a call that changed a file — how much it changed.
//! The body is the change itself for an edit, and the first few lines of whatever came back for
//! everything else. At the end of a turn, the cards add up to one more row: which files changed,
//! and by how much.
//!
//! One module, used by both the live stream and the replay from stored messages. The two have to
//! agree to the character, or switching conversations would rewrite what you had already read.
//! Everything here is a pure function of the call, its result and a few settings, so that is easy
//! to keep true.
//!
//! What counts as an edit is read off the call's *arguments*, not its name: `old_string` and
//! `new_string`, an `edits` list of them, `old_text`/`new_text`, or `content` headed for a path.
//! Those are the shapes every editing tool in practice takes, and reading them means a tool a
//! plugin registered tomorrow gets a diff without asking for one. Stats are computed from those
//! arguments too, rather than from `git diff` — which would count work done outside this turn, by
//! you, or by a turn in another conversation on the same checkout.

use neosh_proto::{PlanState, PlanStep};

use crate::diff;

/// One highlighted span of a row: byte range and group.
pub type Span = (usize, usize, &'static str);

/// A row of the transcript: what it says, what to colour on it, and what colour it is *behind*.
///
/// The band is separate from the spans, and it has to be. A line a diff added is green and the code
/// on it is still syntax-coloured — those are two facts about the same characters, and a span can
/// only carry one. So the band is a property of the row, the frontend paints it out to its own
/// right edge, and every span on the row is patched over the top of it. See
/// [`neosh_proto::ExtmarkOpts::line_hl_group`].
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Row {
    pub text: String,
    pub spans: Vec<Span>,
    /// The group behind the whole row. `None` for everything that is not a changed line of a diff.
    pub band: Option<&'static str>,
}

impl Row {
    /// A row that is all one colour, which most of them are.
    pub fn plain(text: impl Into<String>, hl: &'static str) -> Self {
        let text = text.into();
        let len = text.len();
        Self { text, spans: vec![(0, len, hl)], band: None }
    }

    /// A row whose pieces are coloured separately.
    pub fn new(text: impl Into<String>, spans: Vec<Span>) -> Self {
        Self { text: text.into(), spans, band: None }
    }

    fn behind(mut self, band: &'static str) -> Self {
        self.band = Some(band);
        self
    }
}

/// The characters the transcript is drawn with, at whatever fidelity the terminal has.
///
/// One place, because the live stream and the replay have to agree: a conversation that changes
/// shape when you switch away and back reads as two different programs having written it.
pub struct Glyphs {
    /// The bar down the left of a question.
    pub bar: &'static str,
    /// The state of a tool call: one dot, which changes colour rather than shape. Shape says
    /// *what*, colour says *how it went* — a glyph that changes to mean "finished" makes the
    /// column impossible to scan.
    pub dot: &'static str,
    /// The rule down the left of what a tool came back with.
    ///
    /// A rule and not an elbow. An elbow marks where a body *starts*, which is the one thing a
    /// body's position under its header already says; it leaves the other forty rows unmarked, so
    /// a long result has no visible extent and reads as loose text that happens to be indented.
    /// A rule answers the question that is actually open — how far does this go — and costs three
    /// columns less, which on a diff is three columns of code.
    pub rule: &'static str,
    /// Unchanged lines a diff is not showing.
    pub fold: &'static str,
    /// The join between two edits to the same file in one call.
    pub between: &'static str,
    /// The working line's mark.
    pub work: &'static str,
    /// The key that opens a folded card, as the trailer names it.
    pub tab: &'static str,
    /// A plan step that is finished, one that is in hand, one that is not started.
    pub done: &'static str,
    pub doing: &'static str,
    pub todo: &'static str,
    /// A sub-agent, in the list of what is out.
    pub branch: &'static str,
}

impl Glyphs {
    pub fn new(ascii: bool) -> Self {
        if ascii {
            Self {
                bar: "|",
                dot: "*",
                rule: "|",
                fold: ":",
                between: "...",
                work: "*",
                tab: "Tab",
                done: "[x]",
                doing: "[>]",
                todo: "[ ]",
                branch: "->",
            }
        } else {
            Self {
                bar: "\u{258c}",
                dot: "\u{23fa}",
                rule: "\u{2502}",
                fold: "\u{22ee}",
                between: "\u{22ef}",
                work: "\u{2733}",
                tab: "\u{21e5}",
                done: "\u{2714}",
                doing: "\u{25b8}",
                todo: "\u{25a1}",
                branch: "\u{21b3}",
            }
        }
    }

    /// What every row of a card body starts with: two columns of indent, the rule, one space.
    ///
    /// The same on the first row as on the last, which is the whole point of it being a rule.
    fn margin(&self) -> String {
        format!("  {} ", self.rule)
    }
}

/// How a tool call is going, which is the only thing the dot beside it says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolState {
    Running,
    Done,
    Failed,
}

impl ToolState {
    pub fn hl(self) -> &'static str {
        match self {
            Self::Running => "Agent.ToolRunning",
            Self::Done => "Agent.ToolDone",
            Self::Failed => "Agent.ToolFailed",
        }
    }
}

/// How much of a body to show before it folds.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Rows of a diff.
    pub diff_lines: usize,
    /// Lines of any other result.
    pub output_lines: usize,
}

// ---------------------------------------------------------------------------
// What a call changes
// ---------------------------------------------------------------------------

/// What an edit-shaped call does to one file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Change {
    /// As the call named it. Empty when it did not.
    pub path: String,
    pub diff: diff::Diff,
}

/// Lines added and removed, summed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    pub added: usize,
    pub removed: usize,
}

impl Stats {
    pub fn of(changes: &[Change]) -> Self {
        changes.iter().fold(Self::default(), |s, c| Self {
            added: s.added + c.diff.added,
            removed: s.removed + c.diff.removed,
        })
    }

    fn is_empty(self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

/// The keys a tool call keeps its path under, in the order they are tried.
const PATH_KEYS: &[&str] = &["file_path", "path", "file", "abs_path", "filePath", "notebook_path"];

fn path_of(map: &serde_json::Map<String, serde_json::Value>) -> String {
    PATH_KEYS
        .iter()
        .find_map(|k| map.get(*k).and_then(|v| v.as_str()))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The edits a call describes, if it is that kind of call.
///
/// Empty for anything else, which is how a `Read` or a shell command is told apart from an edit
/// without a list of tool names to keep up to date.
pub fn edits_of(input: &serde_json::Value) -> Vec<Change> {
    let Some(map) = input.as_object() else { return Vec::new() };
    let path = path_of(map);
    let pair = |m: &serde_json::Map<String, serde_json::Value>| -> Option<diff::Diff> {
        for (o, n) in [("old_string", "new_string"), ("old_text", "new_text"), ("oldText", "newText")]
        {
            if let (Some(old), Some(new)) =
                (m.get(o).and_then(|v| v.as_str()), m.get(n).and_then(|v| v.as_str()))
            {
                return Some(diff::lines(old, new));
            }
        }
        None
    };
    if let Some(d) = pair(map) {
        return vec![Change { path, diff: d }];
    }
    // A patch, which some agents report instead of the two texts. Already a diff; see
    // [`diff::unified`].
    if let Some(patch) = map.get("diff").and_then(|v| v.as_str()) {
        return vec![Change { path, diff: diff::unified(patch) }];
    }
    // A list of them, one per file, which is the shape `codex` uses for a multi-file patch.
    if let Some(list) = map.get("changes").and_then(|c| c.as_array()) {
        let out: Vec<Change> = list
            .iter()
            .filter_map(|c| c.as_object())
            .filter_map(|c| {
                let patch = c.get("diff").and_then(|v| v.as_str())?;
                let p = path_of(c);
                Some(Change {
                    path: if p.is_empty() { path.clone() } else { p },
                    diff: diff::unified(patch),
                })
            })
            .collect();
        if !out.is_empty() {
            return out;
        }
    }
    if let Some(list) = map.get("edits").and_then(|e| e.as_array()) {
        let out: Vec<Change> = list
            .iter()
            .filter_map(|e| e.as_object())
            .filter_map(|e| {
                let d = pair(e)?;
                let p = path_of(e);
                Some(Change { path: if p.is_empty() { path.clone() } else { p }, diff: d })
            })
            .collect();
        if !out.is_empty() {
            return out;
        }
    }
    // A whole file, written. There is no "before" to diff against, so every line is new — which
    // is also what it looks like in the file, and the number is the number you would check.
    if !path.is_empty()
        && let Some(content) = map.get("content").and_then(|v| v.as_str())
    {
        return vec![Change { path, diff: diff::whole_file(content) }];
    }
    Vec::new()
}

/// What happened to one file over a turn.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileStat {
    pub path: String,
    pub added: usize,
    pub removed: usize,
}

/// Add a call's changes to a turn's tally, one entry per file, in the order files were first
/// touched.
pub fn tally(into: &mut Vec<FileStat>, changes: &[Change]) {
    for c in changes {
        match into.iter_mut().find(|f| f.path == c.path) {
            Some(f) => {
                f.added += c.diff.added;
                f.removed += c.diff.removed;
            }
            None => into.push(FileStat {
                path: c.path.clone(),
                added: c.diff.added,
                removed: c.diff.removed,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// The header
// ---------------------------------------------------------------------------

/// What sort of thing a call is, from what it was handed rather than what it is called.
///
/// Read off the arguments for the same reason [`edits_of`] is: a tool a plugin registers tomorrow
/// is a `Run` because it was given a command, not because somebody added its name to a list in
/// here. The name on the card stays whatever the tool calls itself — this only decides what colour
/// it is, so that a column of calls can be scanned for "what did it *touch*" without inventing a
/// second vocabulary for the one the model is already using.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Read,
    Edit,
    Run,
    Net,
    Other,
}

impl Kind {
    pub fn of(input: &serde_json::Value) -> Self {
        let Some(map) = input.as_object() else { return Self::Other };
        // Edits first: a write is a `content` and a path, which would otherwise read as a read.
        if !edits_of(input).is_empty() {
            return Self::Edit;
        }
        if map.contains_key("command") {
            return Self::Run;
        }
        if map.contains_key("url") {
            return Self::Net;
        }
        if ["path", "file_path", "file", "pattern", "query"].iter().any(|k| map.contains_key(*k)) {
            return Self::Read;
        }
        Self::Other
    }

    fn hl(self) -> &'static str {
        match self {
            Self::Read => "Agent.ToolRead",
            Self::Edit => "Agent.ToolEdit",
            Self::Run => "Agent.ToolRun",
            Self::Net => "Agent.ToolNet",
            Self::Other => "Agent.Tool",
        }
    }
}

/// Everything a header says that is not in the call's arguments.
///
/// A struct rather than five positional parameters, because three of them are `Option`s of the
/// same shape and a call site that gets two of them the wrong way round still compiles.
pub struct Head<'a> {
    pub name: &'a str,
    pub input: &'a serde_json::Value,
    pub state: ToolState,
    /// How long it has been running, or how long it took.
    ///
    /// `None` when nothing timed it, which is every card rebuilt from the conversation: what was
    /// said is on disk and how long it took is not. A stopwatch is a property of watching
    /// something happen, and reopening a conversation is not watching it happen again.
    pub took: Option<std::time::Duration>,
    /// The status a failed call reported, when its output opened by saying one.
    pub exit: Option<i64>,
    /// How many lines the call came back with. `None` until it has come back.
    ///
    /// On the header rather than only under it, because a call whose body is [folded away
    /// entirely](looks_at_something) has nothing under it to count — and a card that says a file
    /// was read without saying how much of it there was is a card you have to open to learn the one
    /// thing you would have guessed from the body's height.
    pub output: Option<usize>,
}

/// Whether a call is one that *looked at* something rather than changing it.
///
/// The rule that decides how much room its card gets. A read, a grep, a listing: the answer to
/// these is almost always "yes, and it was what you would expect", and three lines of the file
/// under every one of them is how a turn that read six files fills a screen with the parts of them
/// nobody asked about. What you wanted was the *list of files it looked at*, which is exactly the
/// column of headers those bodies were pushing apart.
///
/// So a call like this folds to its header, with the size of what came back on the end of it, and
/// `⇥` while reading still opens the whole thing. A command's output stays, because for a command
/// the output *is* the answer; an edit's diff stays, because the diff is what changed; and a
/// failure always shows, in every mode, because the setting is about noise and not about hiding
/// failures.
pub fn looks_at_something(input: &serde_json::Value) -> bool {
    Kind::of(input) == Kind::Read
}

/// Below this, a call did not take any time worth reporting.
///
/// A duration answers "how long did I wait", and for a call that came back before you could look
/// at it the answer is "you didn't". Printing `0ms` beside every read and every grep would put a
/// number on the one thing about them nobody wondered, and bury the `40s` that mattered in a
/// column of them. Roughly where a delay stops being instant and starts being a pause.
const NOTICEABLE_MS: u128 = 100;

/// How long a call took, at the least precision that is still an answer.
///
/// Milliseconds under a second, because the interesting fact about a fast call is *how* fast; one
/// decimal under a minute, because the second digit changes while you read it; and no decimals at
/// all past that, because by then the number is a complaint rather than a measurement.
pub fn took(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{:.1}s", d.as_secs_f64());
    }
    format!("{}m {:02}s", secs / 60, secs % 60)
}

/// The status a shell tool reported, when what it handed back opens by saying so.
///
/// `Exit code 101` is the first line of a failed command in every driver here, and it is the number
/// you would go looking for. Read out of the text rather than carried alongside it, because no
/// protocol in this workspace has a field for it: [`neosh_proto::ToolResult`] says *whether* a call
/// failed and never *how*. Nothing is invented when the convention does not hold — the card says
/// less, which is the only honest thing it can do.
pub fn exit_code(content: &str) -> Option<i64> {
    let rest = content.trim_start().strip_prefix("Exit code ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The header line of a tool card: a state dot, the tool's name, what it is about, and a tail of
/// whatever else is known — what an edit changed, what a command exited with, how long it took.
///
/// `⏺ Edit  src/main.rs  +3 -1  0.1s`. One line however large the arguments are — a card that
/// grows with its input buries the answer it was gathered for, and the arguments are in the
/// conversation anyway.
///
/// The subject sits after the name with a gap, rather than inside brackets. `Edit(src/main.rs)`
/// reads as a function call, which is what it is on the wire and not what it is on screen: what
/// happened is that a file was edited, and the brackets are two characters of punctuation spent
/// making a fact look like syntax. Colour already separates the two, and the gap is what a column
/// of these needs to be scannable — the eye finds the second column, not the open bracket.
///
/// The tail runs after the subject rather than against the right margin. Right-aligned it would
/// read better and be wrong the first time the window is resized: these rows are written once, and
/// only the ones still running are ever written again.
///
/// The name is whatever the tool calls itself. Mapping `Read` to something friendlier would be
/// inventing a second vocabulary for the one the model is already using, and the moment they
/// disagree the transcript is describing a tool that does not exist.
///
/// The stats appear only on a call that finished well: a failed edit changed nothing, and a
/// running one has not changed anything yet.
pub fn header(g: &Glyphs, head: &Head, root: &std::path::Path, width: usize) -> Row {
    let stats = match head.state {
        ToolState::Done => Stats::of(&edits_of(head.input)),
        _ => Stats::default(),
    };
    // Right of the subject, in the order you would ask about it: what it changed, how it ended,
    // how long it took. Grouped, because `+3 -1` is one fact said in two colours and the pieces of
    // it should not drift apart.
    let mut tail: Vec<Vec<(String, &'static str)>> = Vec::new();
    if !stats.is_empty() {
        tail.push(vec![
            (format!("+{}", stats.added), "Diff.Add"),
            (format!("-{}", stats.removed), "Diff.Delete"),
        ]);
    }
    if let Some(code) = head.exit {
        tail.push(vec![(format!("exit {code}"), "Agent.ToolFailed")]);
    }
    // Only where the body is not going to say it. Under a card that shows its output this would be
    // a number counting rows the reader can already see.
    if let Some(n) = head
        .output
        .filter(|n| *n > 0 && head.state == ToolState::Done && looks_at_something(head.input))
    {
        let word = if n == 1 { "line" } else { "lines" };
        tail.push(vec![(format!("{n} {word}"), "Agent.Usage")]);
    }
    if let Some(d) = head.took.filter(|d| d.as_millis() >= NOTICEABLE_MS) {
        tail.push(vec![(took(d), "Agent.Usage")]);
    }
    let tail_width: usize = tail
        .iter()
        .map(|g| 2 + g.iter().map(|(t, _)| t.chars().count()).sum::<usize>() + g.len() - 1)
        .sum();

    let name = head.name;
    // What is left for the subject once the dot, the name, the gap and the tail have had their
    // share.
    let fixed = g.dot.chars().count() + 1 + name.chars().count() + 2 + tail_width;
    let room = width.saturating_sub(fixed).max(8);
    let subject = subject_of(head.input, root, room);
    let mut text = if subject.is_empty() {
        format!("{} {name}", g.dot)
    } else {
        format!("{} {name}  {subject}", g.dot)
    };
    let after_dot = g.dot.len();
    let name_end = after_dot + 1 + name.len();
    let mut spans: Vec<Span> =
        vec![(0, after_dot, head.state.hl()), (after_dot, name_end, Kind::of(head.input).hl())];
    // The subject is the part that moves. A band travelling along the command you are waiting on
    // is the difference between "still going" and "wedged", and on a card with no body yet it is
    // the only thing that could say which. It settles the moment the call lands.
    if name_end < text.len() {
        let hl =
            if head.state == ToolState::Running { "Agent.ToolLive" } else { "Agent.Usage" };
        spans.push((name_end, text.len(), hl));
    }
    for group in tail {
        text.push_str("  ");
        for (i, (piece, hl)) in group.iter().enumerate() {
            if i > 0 {
                text.push(' ');
            }
            let from = text.len();
            text.push_str(piece);
            spans.push((from, text.len(), hl));
        }
    }
    Row::new(text, spans)
}

/// The agent's own checklist, drawn.
///
/// Above the working line rather than in the flow of the transcript, and redrawn whole every time
/// it changes, because a plan is **state and not history**: the version from two tool calls ago is
/// not something to scroll back to, it is a wrong answer to "what is left". It is committed into
/// the transcript exactly once, when the turn ends and it stops being state.
///
/// Three marks and three weights. Which line is in hand should be answerable without reading all
/// of them — so the finished ones recede, the next ones are quiet, and the one being worked on
/// sweeps, because from the plan's point of view that is what a running call looks like.
pub fn plan(g: &Glyphs, steps: &[PlanStep], width: usize) -> Vec<Row> {
    steps
        .iter()
        .map(|step| {
            let (mark, hl) = match step.state {
                PlanState::Done => (g.done, "Agent.PlanDone"),
                PlanState::Active => (g.doing, "Agent.PlanActive"),
                PlanState::Pending => (g.todo, "Agent.PlanTodo"),
            };
            let room = width.saturating_sub(mark.chars().count() + 3).max(8);
            let text = format!("  {mark} {}", clip(step.text.trim(), room));
            let from = 2 + mark.len() + 1;
            let end = text.len();
            Row::new(text, vec![(2, from - 1, hl), (from, end, hl)])
        })
        .collect()
}

/// One sub-agent, as the footer lists it.
pub struct TaskRow<'a> {
    pub title: &'a str,
    /// Which kind it is, when the driver distinguishes them.
    pub role: Option<&'a str>,
    /// How long it has been out, for one this turn watched start.
    pub since: Option<std::time::Duration>,
    /// Whether the turn is still waiting on it. A backgrounded one is listed and still, because
    /// "something is running that I am not waiting for" is the thing worth being told.
    pub live: bool,
}

/// What is out, drawn under the plan.
///
/// A sub-agent's *findings* come back on the card of the call that spawned it — that is where they
/// belong and where they stay. This is the other question, the one the card cannot answer until it
/// is over: is it still going, and how long has it been.
pub fn tasks(g: &Glyphs, rows: &[TaskRow], width: usize) -> Vec<Row> {
    rows.iter()
        .map(|t| {
            let mut tail = String::new();
            if let Some(role) = t.role.filter(|r| !r.is_empty()) {
                tail.push_str(&format!("  \u{b7}  {role}"));
            }
            if let Some(d) = t.since.filter(|d| d.as_millis() >= NOTICEABLE_MS) {
                tail.push_str(&format!("  \u{b7}  {}", took(d)));
            }
            let room =
                width.saturating_sub(g.branch.chars().count() + 3 + tail.chars().count()).max(8);
            let head = format!("  {} {}", g.branch, clip(t.title.trim(), room));
            let hl = if t.live { "Agent.TaskLive" } else { "Agent.TaskIdle" };
            let text = format!("{head}{tail}");
            Row::new(text, vec![
                (2, head.len(), hl),
                (head.len(), head.len() + tail.len(), "Agent.Usage"),
            ])
        })
        .collect()
}

/// A compaction, once it has landed, as the one row it leaves behind.
///
/// Worth a row of its own rather than a note that disappears: the conversation the agent is holding
/// was just replaced by a summary of itself, and that is the single most useful thing to be able to
/// scroll back to when an answer later seems to have forgotten something.
pub fn compaction(g: &Glyphs, before: Option<u64>, after: Option<u64>) -> Row {
    let mut text = format!("{} Compacted the conversation", g.dot);
    let span = text.len();
    if let (Some(a), Some(b)) = (before, after) {
        text.push_str(&format!("  {} \u{2192} {}", tokens(a), tokens(b)));
    }
    let end = text.len();
    Row::new(text, vec![(0, g.dot.len(), "Agent.ToolDone"), (span, end, "Agent.Usage")])
}

/// A token count, short enough to sit at the end of a line.
fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// What a tool call is *about*, from whichever of its arguments says so.
///
/// A path is shown relative to the conversation's directory. An absolute path is mostly the part
/// you already know, and once it is clipped to fit, the part you already know is *all* you can
/// see — which is how a card ends up saying `/home/you/projects/thing/crates/neosh/src/…`.
pub fn subject_of(input: &serde_json::Value, root: &std::path::Path, room: usize) -> String {
    const KEYS: &[&str] =
        &["path", "file_path", "file", "pattern", "query", "command", "url", "prompt"];
    let Some(map) = input.as_object() else { return String::new() };
    for key in KEYS {
        if let Some(v) = map.get(*key).and_then(|v| v.as_str()) {
            let one = v.lines().next().unwrap_or(v).trim();
            if one.is_empty() {
                continue;
            }
            return clip(&relative_to(one, root), room);
        }
    }
    String::new()
}

/// A path inside the conversation's directory, said the short way.
pub fn relative_to(value: &str, root: &std::path::Path) -> String {
    if root.as_os_str().is_empty() {
        return value.to_string();
    }
    std::path::Path::new(value)
        .strip_prefix(root)
        .ok()
        .filter(|r| !r.as_os_str().is_empty())
        .map(|r| r.display().to_string())
        .unwrap_or_else(|| value.to_string())
}

/// `text`, or as much of it as fits in `room` with an ellipsis.
///
/// By character, not byte: slicing a multi-byte boundary panics, and the frontend measures display
/// width anyway.
fn clip(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_string();
    }
    let clipped: String = text.chars().take(room.saturating_sub(1)).collect();
    format!("{clipped}\u{2026}")
}

// ---------------------------------------------------------------------------
// The body
// ---------------------------------------------------------------------------

/// What goes under a header once the call has come back.
///
/// For an edit that worked, the change itself — a rule down the left, a line number where one is
/// known, a sign, and the code, syntax-coloured, on a band that says which way it went:
///
/// ```text
///   │ 142    impl Session {
///   │ 143  - let x = 1;
///   │ 143  + let x = 2;
///   │ ⋮ 4 unchanged
/// ```
///
/// For anything else — and for an edit that failed, whose error is the thing to read — what came
/// back, under the same rule:
///
/// ```text
///   │ hello
///   │ world
///   │ … +2 more lines (^S ⇥ to expand)
/// ```
///
/// Folded to `limits` rows unless `expanded`, because the point is to see that something happened
/// and roughly what — not to reprint a file into the middle of the conversation that asked about
/// it. The trailer says how much was left out and how to see it: a result that is silently cut is
/// a result you would believe. An error ignores the limit being zero — a failure nobody can see is
/// a debugging session, and the setting is about noise rather than about hiding failures.
pub fn body(
    g: &Glyphs,
    input: &serde_json::Value,
    result: &neosh_proto::ToolResult,
    limits: Limits,
    expanded: bool,
    width: usize,
) -> Vec<Row> {
    if !result.is_error {
        let changes = edits_of(input);
        if !changes.is_empty() {
            return diff_rows(g, &changes, limits.diff_lines, expanded, width);
        }
        // Folded to the header alone. The count is already on it, and `⇥` opens this.
        if !expanded && looks_at_something(input) {
            return Vec::new();
        }
    }
    result_rows(g, result, limits.output_lines, expanded, width)
}

/// The trailer under a folded body.
fn trailer(g: &Glyphs, margin: &str, rest: usize, what: &str) -> Row {
    let word = if rest == 1 { what.trim_end_matches('s').to_string() } else { what.to_string() };
    Row::plain(
        format!("{margin}\u{2026} +{rest} more {word} (^S {} to expand)", g.tab),
        "Agent.Usage",
    )
}

/// What a tool wrote, however much of it fits.
///
/// Tabs become spaces on the way in. A terminal advances to the next tab stop and a buffer line
/// does not, so a `Read` whose output is `     1\thello` arrives on screen as `1hello` — the one
/// place a tool's output is *deliberately* aligned is the one place it collapses.
fn result_rows(
    g: &Glyphs,
    result: &neosh_proto::ToolResult,
    limit: usize,
    expanded: bool,
    width: usize,
) -> Vec<Row> {
    let hl = if result.is_error { "Agent.ToolError" } else { "Agent.Usage" };
    let margin = g.margin();

    let body = result.content.trim_end();
    if body.trim().is_empty() {
        let word = if result.is_error { "failed" } else { "done" };
        return vec![Row::plain(format!("{margin}{word}"), hl)];
    }

    let total = body.lines().count();
    let limit = if expanded {
        total
    } else if result.is_error {
        limit.max(1)
    } else {
        limit
    };
    if limit == 0 {
        let word = if total == 1 { "line" } else { "lines" };
        return vec![Row::plain(format!("{margin}{total} {word}"), hl)];
    }

    let room = width.saturating_sub(margin.chars().count()).max(8);
    let mut out = Vec::new();
    for line in body.lines().take(limit) {
        out.push(Row::plain(format!("{margin}{}", clip(&tabs(line), room)), hl));
    }
    let rest = total.saturating_sub(limit);
    if rest > 0 {
        out.push(trailer(g, &margin, rest, "lines"));
    }
    out
}

/// One row of a diff body, before the margin and the wrapping.
enum Piece {
    /// The `at`th line of the `change`th change.
    Line { change: usize, at: usize },
    /// `n` unchanged lines not shown.
    Gap(usize),
    /// Between two edits to the same file.
    Break,
}

/// How a diff line is drawn: its sign, the band behind it, and what colours the sign.
fn look(op: diff::Op) -> (&'static str, Option<&'static str>, &'static str, &'static str) {
    match op {
        diff::Op::Add => ("+", Some("Diff.AddLine"), "Diff.SignAdd", "Diff.GutterAdd"),
        diff::Op::Del => ("-", Some("Diff.DelLine"), "Diff.SignDel", "Diff.GutterDel"),
        diff::Op::Keep => (" ", None, "Agent.Usage", "Diff.Gutter"),
    }
}

/// The change itself.
///
/// Three columns and then the code: the rule, the line number if anyone knew it, and the sign. The
/// sign stays even though the band already says the same thing, because the band is the first thing
/// to go — on a sixteen-colour terminal, under `NO_COLOR`, in a screenshot somebody pasted into a
/// ticket — and a diff whose two halves are told apart by nothing at all is not a diff. It is the
/// same rule the rest of the palette follows: meaning survives losing colour.
///
/// The code is highlighted per *change* rather than per line, in one pass over every line the change
/// touches, added and removed together. A grammar is a state machine; line two of a doc comment is
/// only a comment because line one opened one, and highlighting the visible rows one at a time is
/// how half a comment ends up grey and the other half looks like code.
fn diff_rows(
    g: &Glyphs,
    changes: &[Change],
    limit: usize,
    expanded: bool,
    width: usize,
) -> Vec<Row> {
    // The rows to show: every line of every change, with the unchanged stretches folded away
    // unless expanded.
    let mut pieces: Vec<Piece> = Vec::new();
    for (c, change) in changes.iter().enumerate() {
        if c > 0 {
            pieces.push(Piece::Break);
        }
        if expanded {
            pieces.extend((0..change.diff.lines.len()).map(|at| Piece::Line { change: c, at }));
            continue;
        }
        // `fold` works on the change's own lines, so a kept row has to be found again by identity
        // of position — it is the next line that has not been used yet.
        let rows = diff::fold(&change.diff, 2);
        let mut at = 0usize;
        let last = rows.len().saturating_sub(1);
        for (i, row) in rows.iter().enumerate() {
            match row {
                diff::Row::Line(_) => {
                    pieces.push(Piece::Line { change: c, at });
                    at += 1;
                }
                // A gap at either end says only that the file goes on, which a file does.
                diff::Row::Gap(n) => {
                    if i != 0 && i != last {
                        pieces.push(Piece::Gap(*n));
                    }
                    at += n;
                }
            }
        }
    }
    if !expanded && limit == 0 {
        return Vec::new();
    }
    let shown = if expanded { pieces.len() } else { limit.min(pieces.len()) };

    let painted: Vec<Option<Vec<neosh_syntax::Spans>>> = changes.iter().map(paint).collect();
    // One width for the whole card, from every change in it, so the sign column does not step
    // sideways halfway down. Zero when nothing knew its line number, and then the column is not
    // there at all rather than being a stripe of blanks.
    let digits = changes
        .iter()
        .flat_map(|c| c.diff.lines.iter())
        .filter_map(|l| l.at)
        .max()
        .map(|n| n.to_string().len())
        .unwrap_or(0);

    let margin = g.margin();
    let mut out: Vec<Row> = Vec::new();
    for piece in pieces.iter().take(shown) {
        match piece {
            Piece::Line { change, at } => {
                let line = &changes[*change].diff.lines[*at];
                let code = painted[*change].as_ref().and_then(|rows| rows.get(*at));
                out.extend(code_rows(&margin, digits, line, code, width));
            }
            Piece::Gap(n) => out.push(Row::plain(
                format!("{margin}{} {n} unchanged", g.fold),
                "Agent.Usage",
            )),
            Piece::Break => {
                out.push(Row::plain(format!("{margin}{}", g.between), "Agent.Usage"))
            }
        }
    }
    // What the limit cut, not what the fold hid: context beyond two lines is the file going on,
    // and a trailer counting it would say "more" under a diff whose every change is on screen.
    let rest: usize = pieces[shown..]
        .iter()
        .map(|p| match p {
            Piece::Line { .. } => 1,
            Piece::Gap(n) => *n,
            Piece::Break => 0,
        })
        .sum();
    if rest > 0 {
        out.push(trailer(g, &margin, rest, "lines"));
    }
    out
}

/// Highlight one change, if there is a grammar for what it is a change to.
///
/// Every line of it in one pass — added, removed and kept, in the order the diff has them. That is
/// not quite the text of either file, and it is the closest a diff comes to being one: the two
/// versions of a changed line sit next to each other, so a construct opened before the change and
/// closed after it is still open in between. The alternative, parsing the old lines and the new
/// lines as two files, needs two passes to say the same thing and gets the interleaving wrong at
/// every boundary.
fn paint(change: &Change) -> Option<Vec<neosh_syntax::Spans>> {
    let lang = neosh_syntax::of_path(&change.path)?;
    let mut text = String::new();
    for line in &change.diff.lines {
        text.push_str(&tabs(&line.text));
        text.push('\n');
    }
    let rows = neosh_syntax::spans(lang, &text)?;
    (rows.len() == change.diff.lines.len()).then_some(rows)
}

/// One line of a diff, as however many screen rows it takes.
///
/// Long lines wrap rather than being cut. A clipped line of prose has lost an adjective; a clipped
/// line of code has lost the half of it you were about to read, and there is no scrolling sideways
/// to get it back. The continuation carries the band and the rule and nothing else — no repeated
/// sign, no repeated number, because both of those are facts about *a line* and this is still the
/// same one.
fn code_rows(
    margin: &str,
    digits: usize,
    line: &diff::Line,
    code: Option<&neosh_syntax::Spans>,
    width: usize,
) -> Vec<Row> {
    let (sign, band, sign_hl, gutter_hl) = look(line.op);
    let text = tabs(&line.text);

    // The fixed part of every row: rule, number, sign. Built once, since the continuation rows
    // need the same width of it in blanks.
    let head = |number: bool| -> (String, Vec<Span>) {
        let mut out = margin.to_string();
        let mut spans = vec![(0, out.len(), "Agent.Usage")];
        if digits > 0 {
            let from = out.len();
            match line.at.filter(|_| number) {
                Some(n) => out.push_str(&format!("{n:>digits$} ")),
                None => out.push_str(&" ".repeat(digits + 1)),
            }
            spans.push((from, out.len(), gutter_hl));
        }
        let from = out.len();
        out.push_str(if number { sign } else { " " });
        out.push(' ');
        spans.push((from, out.len(), sign_hl));
        (out, spans)
    };

    let (first, _) = head(true);
    let room = width.saturating_sub(first.chars().count()).max(16);
    let mut out = Vec::new();
    for (i, (from, chunk)) in split_at_width(&text, room).into_iter().enumerate() {
        let (prefix, mut spans) = head(i == 0);
        let at = prefix.len();
        let mut row = prefix;
        row.push_str(&chunk);
        match code {
            Some(all) => spans.extend(shift(all, from, from + chunk.len(), at)),
            None => spans.push((at, row.len(), "Syntax.Text")),
        }
        out.push(match band {
            Some(b) => Row::new(row, spans).behind(b),
            None => Row::new(row, spans),
        });
    }
    out
}

/// The syntax spans covering `from..to`, moved to sit at `at` in a row that begins there.
fn shift(spans: &neosh_syntax::Spans, from: usize, to: usize, at: usize) -> Vec<Span> {
    spans
        .iter()
        .filter(|(a, b, _)| *b > from && *a < to)
        .map(|(a, b, g)| (at + a.max(&from) - from, at + b.min(&to) - from, *g))
        .collect()
}

/// Break `text` into chunks of at most `room` characters, keeping where each began.
///
/// At a space when there is one near the end of the chunk, and anywhere otherwise. Code is mostly
/// not prose and a hard break through the middle of an identifier is honest about that — but the
/// line most likely to be too long for a card is a doc comment, and `steering it if a messag` /
/// `e arrives` is a sentence somebody has to reassemble. A third of the chunk is how far back it
/// will look: further, and a line of dense code with one early space wraps at column twenty.
///
/// By character rather than by display width, which is the same approximation [`clip`] has always
/// made here and for the same reason: the frontend owns width, wraps whatever still overflows, and
/// re-wrapping something already close to right is invisible. Doing it here at all is what buys the
/// continuation rows their indent — a line the frontend wraps starts again at column zero, under
/// the rule rather than beside it.
fn split_at_width(text: &str, room: usize) -> Vec<(usize, String)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    if chars.len() <= room {
        return vec![(0, text.to_string())];
    }
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < chars.len() {
        let end = (at + room).min(chars.len());
        // `cut` is always past `at`: the space found is at or after `floor`, and `floor` is never
        // less than `at`. Which is what stops this looping on a line it cannot break.
        let mut cut = end;
        if end < chars.len() {
            let floor = at + room * 2 / 3;
            if let Some(i) = (floor..end).rev().find(|i| chars[*i].1 == ' ') {
                cut = i + 1;
            }
        }
        let from = chars[at].0;
        let to = chars.get(cut).map_or(text.len(), |(i, _)| *i);
        out.push((from, text[from..to].to_string()));
        at = cut;
    }
    out
}

/// Tabs, as the spaces a buffer line can actually hold.
///
/// Four, which is what the tab stop is in most of the code anybody is diffing. A buffer line is
/// characters and a terminal's tab is a *motion*, so the two cannot both be right; the one that
/// can be stored is the one that survives being scrolled, searched and copied out of `^S`.
fn tabs(text: &str) -> String {
    if text.contains('\t') { text.replace('\t', "    ") } else { text.to_string() }
}

// ---------------------------------------------------------------------------
// The summary
// ---------------------------------------------------------------------------

/// What a turn changed, added up.
///
/// ```text
///   changed 2 files  +117 -40
///     crates/neosh/src/host.rs  +100 -38
///     crates/neosh/src/diff.rs  +17 -2
/// ```
///
/// Nothing at all when nothing changed: a turn that only read and answered has no summary to
/// give, and a row saying so would be a row on every turn.
pub fn summary(files: &[FileStat], root: &std::path::Path, width: usize) -> Vec<Row> {
    if files.is_empty() {
        return Vec::new();
    }
    let (added, removed) =
        files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    let stat_row = |lead: String, added: usize, removed: usize| -> Row {
        let mut text = lead;
        let from = text.len() + 2;
        text.push_str(&format!("  +{added} -{removed}"));
        let plus = from + format!("+{added}").len();
        let end = text.len();
        Row::new(text, vec![
            (0, from, "Agent.Usage"),
            (from, plus, "Diff.Add"),
            (plus + 1, end, "Diff.Delete"),
        ])
    };
    let word = if files.len() == 1 { "file" } else { "files" };
    let mut out = vec![stat_row(format!("  changed {} {word}", files.len()), added, removed)];
    for f in files {
        let tail = format!("  +{} -{}", f.added, f.removed).chars().count();
        let room = width.saturating_sub(4 + tail).max(8);
        let path = if f.path.is_empty() { "(unnamed)".to_string() } else { clip(&relative_to(&f.path, root), room) };
        out.push(stat_row(format!("    {path}"), f.added, f.removed));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn g() -> Glyphs {
        Glyphs::new(false)
    }

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from("/work")
    }

    #[test]
    fn a_running_calls_subject_is_the_part_that_moves() {
        let input = json!({"command": "cargo test"});
        let spans = header_spans(&g(), &head("Bash", &input, ToolState::Running), &root(), 80);
        assert!(
            spans.iter().any(|(_, _, hl)| *hl == "Agent.ToolLive"),
            "the command shimmers while the call is out\n{spans:?}"
        );
        let done = header_spans(&g(), &head("Bash", &input, ToolState::Done), &root(), 80);
        assert!(
            !done.iter().any(|(_, _, hl)| *hl == "Agent.ToolLive"),
            "and settles the moment it lands\n{done:?}"
        );
    }

    #[test]
    fn what_a_call_touched_is_read_off_its_arguments_not_its_name() {
        // Not one of these names is in the renderer. A plugin's tool gets the same colours for
        // the same reason its edits get a diff: it said what it was given.
        for (name, input, want) in [
            ("Bash", json!({"command": "ls"}), Kind::Run),
            ("shell", json!({"command": "ls"}), Kind::Run),
            ("Read", json!({"file_path": "/work/a.rs"}), Kind::Read),
            ("Grep", json!({"pattern": "fn main"}), Kind::Read),
            ("Edit", json!({"file_path": "a", "old_string": "x", "new_string": "y"}), Kind::Edit),
            ("Write", json!({"file_path": "a", "content": "x"}), Kind::Edit),
            ("WebFetch", json!({"url": "https://example.com"}), Kind::Net),
            ("Agent", json!({"description": "look around"}), Kind::Other),
        ] {
            assert_eq!(Kind::of(&input), want, "{name}");
            let spans = header_spans(&g(), &head(name, &input, ToolState::Done), &root(), 80);
            assert!(
                spans.iter().any(|(_, _, hl)| *hl == want.hl()),
                "{name} wears {}\n{spans:?}",
                want.hl()
            );
        }
    }

    #[test]
    fn a_call_that_took_no_time_is_not_timed() {
        use std::time::Duration;
        let input = json!({"file_path": "/work/a.rs"});
        let quick = Head {
            name: "Read",
            input: &input,
            state: ToolState::Done,
            took: Some(Duration::from_millis(4)),
            exit: None,
            output: None,
        };
        let text = header_text(&g(), &quick, &root(), 80);
        assert_eq!(text, "\u{23fa} Read  a.rs", "{text:?}");
        let slow = Head { took: Some(Duration::from_millis(100)), ..quick };
        let text = header_text(&g(), &slow, &root(), 80);
        assert!(text.ends_with("  100ms"), "{text:?}");
    }

    #[test]
    fn a_duration_is_said_at_the_precision_that_answers_the_question() {
        use std::time::Duration;
        assert_eq!(took(Duration::from_millis(7)), "7ms");
        assert_eq!(took(Duration::from_millis(999)), "999ms");
        assert_eq!(took(Duration::from_millis(1040)), "1.0s");
        assert_eq!(took(Duration::from_millis(59_900)), "59.9s");
        assert_eq!(took(Duration::from_secs(61)), "1m 01s");
        assert_eq!(took(Duration::from_secs(3661)), "61m 01s");
    }

    #[test]
    fn a_status_is_read_out_of_the_output_or_not_claimed_at_all() {
        assert_eq!(exit_code("Exit code 101"), Some(101));
        assert_eq!(exit_code("\nExit code 2\nerror: no"), Some(2));
        assert_eq!(exit_code("exit code 2"), None, "the convention is capitalised");
        assert_eq!(exit_code("Exit code without a number"), None);
        assert_eq!(exit_code("error: could not compile"), None);
    }

    #[test]
    fn the_tail_says_what_changed_then_how_it_ended_then_how_long() {
        let input = json!({"file_path": "/work/a.rs", "old_string": "a\nb", "new_string": "a\nc"});
        let full = Head {
            name: "Edit",
            input: &input,
            state: ToolState::Done,
            took: Some(std::time::Duration::from_millis(120)),
            exit: Some(1),
            output: None,
        };
        let text = header_text(&g(), &full, &root(), 90);
        assert!(text.ends_with("+1 -1  exit 1  120ms"), "{text:?}");
        // And nothing is claimed that nobody measured: a card rebuilt from the conversation.
        let bare = header_text(&g(), &head("Edit", &input, ToolState::Done), &root(), 90);
        assert!(bare.ends_with("+1 -1"), "{bare:?}");
    }

    /// A header's non-argument half, with nothing timed. The clock and the status have their own
    /// tests; these are about what the line says.
    fn head<'a>(
        name: &'a str,
        input: &'a serde_json::Value,
        state: ToolState,
    ) -> Head<'a> {
        Head { name, input, state, took: None, exit: None, output: None }
    }

    fn header_text(g: &Glyphs, head: &Head, root: &std::path::Path, width: usize) -> String {
        header(g, head, root, width).text
    }

    fn header_spans(g: &Glyphs, head: &Head, root: &std::path::Path, width: usize) -> Vec<Span> {
        header(g, head, root, width).spans
    }

    fn texts(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn an_edit_is_recognised_by_its_arguments_and_not_its_name() {
        let input = json!({ "file_path": "/work/a.rs", "old_string": "a\nb", "new_string": "a\nc" });
        let changes = edits_of(&input);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "/work/a.rs");
        assert_eq!((changes[0].diff.added, changes[0].diff.removed), (1, 1));

        assert!(edits_of(&json!({ "file_path": "/work/a.rs" })).is_empty(), "a read is not an edit");
        assert!(edits_of(&json!({ "command": "ls" })).is_empty());
    }

    #[test]
    fn a_list_of_edits_is_every_one_of_them() {
        let input = json!({
            "file_path": "/work/a.rs",
            "edits": [
                { "old_string": "x", "new_string": "y" },
                { "old_string": "p\nq", "new_string": "p" },
            ]
        });
        let changes = edits_of(&input);
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().all(|c| c.path == "/work/a.rs"));
        assert_eq!(Stats::of(&changes), Stats { added: 1, removed: 2 });
    }

    #[test]
    fn a_write_is_all_additions() {
        let input = json!({ "path": "/work/new.txt", "content": "one\ntwo\nthree" });
        let changes = edits_of(&input);
        assert_eq!(Stats::of(&changes), Stats { added: 3, removed: 0 });
        // And content without a path is not a write of anything.
        assert!(edits_of(&json!({ "content": "one" })).is_empty());
    }

    #[test]
    fn acp_shaped_edits_count_too() {
        let input = json!({ "path": "/work/a.rs", "old_text": "a", "new_text": "b\nc" });
        assert_eq!(Stats::of(&edits_of(&input)), Stats { added: 2, removed: 1 });
    }

    #[test]
    fn the_header_carries_stats_only_once_the_edit_has_landed() {
        let input = json!({ "file_path": "/work/a.rs", "old_string": "a", "new_string": "b\nc" });
        let running = header_text(&g(), &head("Edit", &input, ToolState::Running), &root(), 80);
        assert_eq!(running, "\u{23fa} Edit  a.rs");
        let failed = header_text(&g(), &head("Edit", &input, ToolState::Failed), &root(), 80);
        assert_eq!(failed, "\u{23fa} Edit  a.rs", "a failed edit changed nothing");
        let row = header(&g(), &head("Edit", &input, ToolState::Done), &root(), 80);
        let (done, spans) = (row.text.clone(), row.spans.clone());
        assert_eq!(done, "\u{23fa} Edit  a.rs  +2 -1");
        let groups: Vec<&str> = spans.iter().map(|s| s.2).collect();
        assert!(groups.contains(&"Diff.Add") && groups.contains(&"Diff.Delete"), "{spans:?}");
        // The spans land on the numbers, not around them.
        let add = spans.iter().find(|s| s.2 == "Diff.Add").unwrap();
        assert_eq!(&done[add.0..add.1], "+2");
        let del = spans.iter().find(|s| s.2 == "Diff.Delete").unwrap();
        assert_eq!(&done[del.0..del.1], "-1");
    }

    #[test]
    fn a_read_has_no_stats_however_it_went() {
        let input = json!({ "file_path": "/work/a.rs" });
        let done = header_text(&g(), &head("Read", &input, ToolState::Done), &root(), 80);
        assert_eq!(done, "\u{23fa} Read  a.rs");
    }

    #[test]
    fn the_header_makes_room_for_the_stats_before_clipping_the_subject() {
        let input = json!({
            "file_path": "/work/a/very/long/path/that/goes/on/and/on/forever/main.rs",
            "old_string": "a", "new_string": "b"
        });
        let done = header_text(&g(), &head("Edit", &input, ToolState::Done), &root(), 40);
        assert!(done.ends_with("  +1 -1"), "{done}");
        assert!(done.chars().count() <= 40, "{done}");
    }

    #[test]
    fn an_edit_body_is_the_diff_with_the_unchanged_middle_folded() {
        let old: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l10\n", "L10\n");
        let input = json!({ "file_path": "/work/a.rs", "old_string": old, "new_string": new });
        let result = neosh_proto::ToolResult::ok("ok");
        let rows = body(&g(), &input, &result, Limits { diff_lines: 12, output_lines: 3 }, false, 80);
        let t = texts(&rows);
        assert_eq!(t[0], "  \u{2502}   l8", "context first, no gap row at the top: {t:?}");
        assert!(t.iter().any(|r| r.ends_with("- l10")), "{t:?}");
        assert!(t.iter().any(|r| r.ends_with("+ L10")), "{t:?}");
        assert!(!t.iter().any(|r| r.contains("unchanged")), "no gap at either end: {t:?}");
        assert_eq!(t.len(), 6, "every change is on screen, so nothing is 'more': {t:?}");
        // Which way a line went is carried by the band behind it, not by the colour of its text
        // — that is what leaves the foreground free for the syntax colours on top.
        assert_eq!(rows.iter().filter(|r| r.band == Some("Diff.AddLine")).count(), 1);
        assert_eq!(rows.iter().filter(|r| r.band == Some("Diff.DelLine")).count(), 1);
        assert_eq!(rows.iter().filter(|r| r.band.is_some()).count(), 2, "context is not banded");

        // Cut by the limit, and the trailer counts exactly what the limit cut.
        let rows = body(&g(), &input, &result, Limits { diff_lines: 3, output_lines: 3 }, false, 80);
        let t = texts(&rows);
        assert_eq!(t.len(), 4, "{t:?}");
        assert!(t[3].contains("+3 more lines"), "{t:?}");
        assert!(t[3].contains("(^S \u{21e5} to expand)"), "{t:?}");
    }

    #[test]
    fn a_gap_in_the_middle_is_a_row_and_counts_for_what_it_hides() {
        let old: String = (1..=30).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l5\n", "L5\n").replace("l25\n", "L25\n");
        let input = json!({ "file_path": "/work/a.rs", "old_string": old, "new_string": new });
        let result = neosh_proto::ToolResult::ok("ok");
        let rows = body(&g(), &input, &result, Limits { diff_lines: 50, output_lines: 3 }, false, 80);
        let t = texts(&rows);
        assert!(t.iter().any(|r| r.ends_with("\u{22ee} 15 unchanged")), "{t:?}");
        // Folded at the first change: the gap and everything after it are what is left.
        let rows = body(&g(), &input, &result, Limits { diff_lines: 5, output_lines: 3 }, false, 80);
        let t = texts(&rows);
        assert!(t.last().unwrap().contains("+22 more lines"), "{t:?}");
    }

    #[test]
    fn an_expanded_edit_body_is_the_whole_diff_and_no_trailer() {
        let old: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l10\n", "L10\n");
        let input = json!({ "file_path": "/work/a.rs", "old_string": old, "new_string": new });
        let result = neosh_proto::ToolResult::ok("ok");
        let rows = body(&g(), &input, &result, Limits { diff_lines: 2, output_lines: 3 }, true, 80);
        assert_eq!(rows.len(), 21, "{:?}", texts(&rows));
        assert!(!texts(&rows).iter().any(|r| r.contains("more lines")));
    }

    #[test]
    fn a_short_edit_body_fits_and_says_nothing_about_more() {
        let input = json!({ "file_path": "/work/a.rs", "old_string": "a\nb", "new_string": "a\nc" });
        let result = neosh_proto::ToolResult::ok("ok");
        let rows = body(&g(), &input, &result, Limits { diff_lines: 12, output_lines: 3 }, false, 80);
        assert_eq!(texts(&rows), vec!["  \u{2502}   a", "  \u{2502} - b", "  \u{2502} + c"]);
    }

    #[test]
    fn a_failed_edit_shows_its_error_not_a_diff() {
        let input = json!({ "file_path": "/work/a.rs", "old_string": "a", "new_string": "b" });
        let result = neosh_proto::ToolResult::error("old_string not found");
        let rows = body(&g(), &input, &result, Limits { diff_lines: 12, output_lines: 0 }, false, 80);
        assert_eq!(texts(&rows), vec!["  \u{2502} old_string not found"]);
        assert_eq!(rows[0].spans[0].2, "Agent.ToolError");
    }

    #[test]
    fn a_result_folds_to_the_limit_and_expands_past_it() {
        let result = neosh_proto::ToolResult::ok("one\ntwo\nthree\nfour");
        let input = json!({ "command": "seq" });
        let folded = body(&g(), &input, &result, Limits { diff_lines: 12, output_lines: 2 }, false, 80);
        let t = texts(&folded);
        assert_eq!(t.len(), 3);
        assert!(t[2].contains("+2 more lines"), "{t:?}");
        let open = body(&g(), &input, &result, Limits { diff_lines: 12, output_lines: 2 }, true, 80);
        assert_eq!(texts(&open).len(), 4);
    }

    #[test]
    fn the_code_in_a_diff_is_syntax_coloured_and_the_band_is_left_to_say_which_way() {
        // The whole point of the split. Before this, a changed line was green *text*; now it is a
        // green *row*, and the text on it says what the code says.
        let input = json!({
            "file_path": "/work/a.rs",
            "old_string": "let x = 1;",
            "new_string": "// two\nlet x = \"two\";",
        });
        let result = neosh_proto::ToolResult::ok("ok");
        let rows = body(&g(), &input, &result, Limits { diff_lines: 12, output_lines: 3 }, false, 80);
        let groups: Vec<&str> = rows.iter().flat_map(|r| &r.spans).map(|s| s.2).collect();
        assert!(groups.contains(&"Syntax.Keyword"), "`let` is a keyword: {groups:?}");
        assert!(groups.contains(&"Syntax.Comment"), "`// two` is a comment: {groups:?}");
        assert!(groups.contains(&"Syntax.String"), "`\"two\"` is a string: {groups:?}");
        // And not one of those spans carries a colour of its own for the diff: the row does.
        assert!(!groups.contains(&"Diff.Add"), "the band says added, not the text: {groups:?}");
        assert!(rows.iter().any(|r| r.band == Some("Diff.AddLine")));
    }

    #[test]
    fn a_file_with_no_grammar_is_still_a_diff() {
        // Every path through here has to survive `of_path` coming back with nothing, which is what
        // happens for an extension nobody wrote a grammar for — and for an edit that never named a
        // path at all, which is most of what ACP sends.
        for path in ["/work/notes.qqzz", ""] {
            let input =
                json!({ "file_path": path, "old_string": "alpha", "new_string": "beta" });
            let result = neosh_proto::ToolResult::ok("ok");
            let rows =
                body(&g(), &input, &result, Limits { diff_lines: 12, output_lines: 3 }, false, 80);
            let t = texts(&rows);
            assert!(t.iter().any(|r| r.ends_with("- alpha")), "{path:?}: {t:?}");
            assert!(t.iter().any(|r| r.ends_with("+ beta")), "{path:?}: {t:?}");
        }
    }

    #[test]
    fn a_patch_that_knows_its_line_numbers_shows_them_and_an_edit_that_does_not_shows_none() {
        let patch = json!({
            "file_path": "/work/a.rs",
            "diff": "@@ -142,3 +142,3 @@\n fn f() {\n-    let x = 1;\n+    let x = 2;\n",
        });
        let result = neosh_proto::ToolResult::ok("ok");
        let rows = body(&g(), &patch, &result, Limits { diff_lines: 12, output_lines: 3 }, false, 80);
        let t = texts(&rows);
        assert!(t.iter().any(|r| r.contains("142   fn f() {")), "{t:?}");
        assert!(t.iter().any(|r| r.contains("143 - ")), "the removed line is numbered in the old file: {t:?}");
        assert!(t.iter().any(|r| r.contains("143 + ")), "{t:?}");

        // The same change said as two strings knows nothing about where it sits, and the column is
        // not there at all rather than being a stripe of blanks.
        let strings = json!({
            "file_path": "/work/a.rs",
            "old_string": "    let x = 1;",
            "new_string": "    let x = 2;",
        });
        let rows =
            body(&g(), &strings, &result, Limits { diff_lines: 12, output_lines: 3 }, false, 80);
        assert_eq!(texts(&rows), vec!["  \u{2502} -     let x = 1;", "  \u{2502} +     let x = 2;"]);
    }

    #[test]
    fn a_long_line_wraps_under_itself_and_keeps_its_band() {
        // Clipped, this is the half of the line you were about to read. There is no scrolling
        // sideways in a transcript, so the other half would simply be gone.
        let long = format!("let s = \"{}\";", "x".repeat(120));
        let input = json!({ "file_path": "/work/a.rs", "old_string": "", "new_string": long });
        let result = neosh_proto::ToolResult::ok("ok");
        let rows = body(&g(), &input, &result, Limits { diff_lines: 12, output_lines: 3 }, true, 60);
        assert!(rows.len() > 1, "it wrapped: {:?}", texts(&rows));
        assert!(rows.iter().all(|r| r.band == Some("Diff.AddLine")), "every row of it is added");
        assert!(rows.iter().all(|r| r.text.chars().count() <= 60), "{:?}", texts(&rows));
        // A line with nowhere sensible to break is broken anywhere rather than not at all.
        assert!(rows.len() >= 3, "a 120-character token still wraps: {:?}", texts(&rows));
        // The sign is on the first row only: it is a fact about the line, and this is still it.
        let signed = rows.iter().filter(|r| r.text.contains("+ ")).count();
        assert_eq!(signed, 1, "{:?}", texts(&rows));
        // And nothing of the line was dropped on the way.
        let joined: String = rows
            .iter()
            // Past the margin and the sign column, which is the same width on every row of it.
            .map(|r| r.text.chars().skip(6).collect::<String>())
            .collect::<Vec<_>>()
            .join("");
        assert!(joined.contains(&"x".repeat(120)), "{joined:?}");
    }

    #[test]
    fn a_wrapped_comment_breaks_between_words_when_there_is_a_word_boundary_to_take() {
        // `steering it if a messag` / `e arrives` is a sentence somebody has to put back together.
        let line = "    /// Run one turn to completion, steering it if a message arrives here.";
        let input = json!({ "file_path": "/work/a.rs", "old_string": "", "new_string": line });
        let result = neosh_proto::ToolResult::ok("ok");
        let rows = body(&g(), &input, &result, Limits { diff_lines: 12, output_lines: 3 }, true, 50);
        let t = texts(&rows);
        assert!(t.len() > 1, "{t:?}");
        for row in &t[..t.len() - 1] {
            assert!(row.ends_with(' '), "each row but the last ends at a word: {t:?}");
        }
    }

    #[test]
    fn a_tab_in_what_a_tool_printed_becomes_the_spaces_it_meant() {
        // A terminal's tab is a motion to the next stop; a buffer line is characters. `Read`
        // output of `     1\thello` arrived on screen as `1hello` — the one place a tool aligns
        // its output deliberately was the one place the alignment collapsed.
        let result = neosh_proto::ToolResult::ok("     1\timpl Session {");
        // Opened, because a read folds to its header alone and the body under test is the part you
        // get by pressing `⇥`.
        let rows = body(&g(), &json!({"file_path": "/work/a.rs"}), &result,
            Limits { diff_lines: 12, output_lines: 3 }, true, 80);
        assert_eq!(texts(&rows), vec!["  \u{2502}      1    impl Session {"]);
    }

    /// A turn that reads six files should read as six rows, not as thirty.
    #[test]
    fn a_call_that_only_looked_at_something_folds_to_its_header_and_says_how_much() {
        let input = json!({"file_path": "/work/a.rs"});
        let result = neosh_proto::ToolResult::ok("one\ntwo\nthree\nfour\n");
        let limits = Limits { diff_lines: 12, output_lines: 3 };
        assert!(
            body(&g(), &input, &result, limits, false, 80).is_empty(),
            "nothing under it"
        );
        // The count is what says there is anything to open, and it counts what a person would:
        // four lines and a trailing newline is four lines.
        let head = Head { output: Some(4), ..head("Read", &input, ToolState::Done) };
        assert!(header_text(&g(), &head, &root(), 80).ends_with("  4 lines"));

        // And `⇥` still gives you the whole thing.
        let open = body(&g(), &input, &result, limits, true, 80);
        assert_eq!(open.len(), 4, "{:?}", texts(&open));
    }

    /// The fold is about noise, and a failure is not noise.
    #[test]
    fn a_look_that_failed_still_shows_what_it_said() {
        let input = json!({"path": "/work/gone.rs"});
        let result = neosh_proto::ToolResult::error("no such file");
        let rows = body(&g(), &input, &result, Limits { diff_lines: 12, output_lines: 3 }, false, 80);
        assert_eq!(texts(&rows), vec!["  \u{2502} no such file"]);
    }

    /// A command's output *is* its answer, so it keeps its body — and its header does not repeat
    /// the count of rows you can already see.
    #[test]
    fn a_command_keeps_what_it_printed() {
        let input = json!({"command": "cargo test"});
        let result = neosh_proto::ToolResult::ok("running 2 tests\nok\nok");
        let rows = body(&g(), &input, &result, Limits { diff_lines: 12, output_lines: 3 }, false, 80);
        assert_eq!(rows.len(), 3, "{:?}", texts(&rows));
        let head = Head { output: Some(3), ..head("Bash", &input, ToolState::Done) };
        let text = header_text(&g(), &head, &root(), 80);
        assert!(!text.contains("3 lines"), "{text:?}");
    }

    #[test]
    fn a_limit_of_zero_counts_a_result_and_hides_a_diff() {
        let result = neosh_proto::ToolResult::ok("one\ntwo");
        let rows = body(&g(), &json!({}), &result, Limits { diff_lines: 0, output_lines: 0 }, false, 80);
        assert_eq!(texts(&rows), vec!["  \u{2502} 2 lines"]);
        let input = json!({ "file_path": "/work/a.rs", "old_string": "a", "new_string": "b" });
        let rows = body(&g(), &input, &result, Limits { diff_lines: 0, output_lines: 3 }, false, 80);
        assert!(rows.is_empty(), "the header already has the stats: {:?}", texts(&rows));
    }

    #[test]
    fn a_turn_tally_is_per_file_in_order_of_first_touch() {
        let mut files = Vec::new();
        tally(&mut files, &edits_of(&json!({ "file_path": "/work/b.rs", "old_string": "1", "new_string": "2" })));
        tally(&mut files, &edits_of(&json!({ "file_path": "/work/a.rs", "content": "x\ny" })));
        tally(&mut files, &edits_of(&json!({ "file_path": "/work/b.rs", "old_string": "q\nr", "new_string": "" })));
        assert_eq!(files, vec![
            FileStat { path: "/work/b.rs".into(), added: 1, removed: 3 },
            FileStat { path: "/work/a.rs".into(), added: 2, removed: 0 },
        ]);
    }

    #[test]
    fn the_summary_names_every_file_and_adds_them_up() {
        let files = vec![
            FileStat { path: "/work/b.rs".into(), added: 1, removed: 3 },
            FileStat { path: "/work/a.rs".into(), added: 2, removed: 0 },
        ];
        let rows = summary(&files, &root(), 80);
        assert_eq!(texts(&rows), vec![
            "  changed 2 files  +3 -3",
            "    b.rs  +1 -3",
            "    a.rs  +2 -0",
        ]);
        let add = rows[0].spans.iter().find(|s| s.2 == "Diff.Add").unwrap();
        assert_eq!(&rows[0].text[add.0..add.1], "+3");
        assert!(summary(&[], &root(), 80).is_empty(), "nothing changed, nothing to say");
        let one = summary(&files[..1], &root(), 80);
        assert_eq!(one[0].text, "  changed 1 file  +1 -3");
    }

    #[test]
    fn ascii_glyphs_name_the_key_in_words() {
        let g = Glyphs::new(true);
        let result = neosh_proto::ToolResult::ok("one\ntwo");
        let rows = body(&g, &json!({}), &result, Limits { diff_lines: 12, output_lines: 1 }, false, 80);
        assert!(rows[1].text.contains("(^S Tab to expand)"), "{:?}", texts(&rows));
    }
}
