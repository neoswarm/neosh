//! What a token *is*. Never what colour it is.
//!
//! # Why this exists at all
//!
//! `neosh/src/markdown.rs` used to say, and was right to say, that there is no syntax highlighting
//! in this workspace: "doing it properly means a grammar per language, and doing it improperly — a
//! regex over keywords — colours the wrong words in exactly the code you are reading closely." The
//! objection was never to highlighting. It was to *guessing*. So this crate does it properly, and
//! is the only crate that knows a grammar exists, the same way `neosh-script` is the only one that
//! knows deno does.
//!
//! # What comes out
//!
//! Byte ranges and a highlight group name, per line: `(from, to, "Syntax.Keyword")`. Nothing here
//! has ever seen a colour. syntect ships thirty-odd themes and this crate loads none of them —
//! [`neosh_core::palette`] owns what a keyword looks like, exactly as it owns what a border looks
//! like, and a user who repaints `Syntax.String` repaints every language at once. A highlighter
//! that carried its own theme would be a second palette to keep in step with the first, and the
//! two would disagree the first time somebody switched to the light variant.
//!
//! # Seven groups, out of several thousand scopes
//!
//! A Sublime grammar distinguishes `variable.parameter` from `variable.other.member` from
//! `variable.language`. That is the right resolution for an editor you live in and the wrong one
//! for twelve lines of diff inside a chat window: past about seven colours the reader stops
//! decoding and starts skimming, and every extra hue costs the ones already there. So the whole
//! scope space collapses onto [`GROUPS`], and the mapping is a table of prefixes rather than a
//! selector language, because a selector language is the thing you need when you are matching
//! *themes*, and this is matching seven buckets.
//!
//! # Cost, and what stops it
//!
//! A card is highlighted once, when it is written, and a transcript rebuild highlights what it
//! rebuilds — through [`spans`]'s memo, so the same diff is never parsed twice while it is
//! remembered. That is fine for a diff and ruinous for a minified bundle somebody pasted, so
//! [`LIMITS`] refuses anything past them and the caller draws plain text — which is what it drew
//! before this crate existed, and is never worse than nothing.
//!
//! What it costs is mostly the regex engine: `fancy-regex` backtracks through grammars written for
//! Oniguruma, at roughly a hundred and fifty microseconds a line of TypeScript, and the first line
//! of a language also compiles that language's patterns. A long conversation is a few thousand
//! lines of diff, which is most of a second spent on the loop that answers keys — so a rebuild
//! asks under [`deferring`], draws what is not remembered plain, and hands the rest to a thread
//! ([`Pending::run`]) that fills the memo for the redraw that follows.

use std::cell::RefCell;
use std::sync::OnceLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, ScopeStackOp, SyntaxReference, SyntaxSet};

/// Every group this crate can name. `neosh-core`'s palette defines all of them.
///
/// Public so the palette's own test can assert the two lists agree — a group named here and
/// undefined there does not fail anywhere at runtime, it just renders as plain text, which looks
/// exactly like a language nobody wrote a grammar for.
pub const GROUPS: &[&str] = &[
    "Syntax.Text",
    "Syntax.Comment",
    "Syntax.Keyword",
    "Syntax.String",
    "Syntax.Number",
    "Syntax.Function",
    "Syntax.Type",
    "Syntax.Constant",
    "Syntax.Punct",
];

/// The plain group, which is also what "no opinion" means.
const TEXT: &str = "Syntax.Text";
const PUNCT: &str = "Syntax.Punct";

/// Past any of these, the answer is "draw it plain".
///
/// Not a performance tuning knob so much as a promise: highlighting happens on the host loop, in
/// the middle of a turn, and a pathological input must cost a bounded amount of it rather than
/// however long a grammar takes to give up. The line bound is the one that actually fires — a
/// minified bundle is one line of two hundred kilobytes, and it is the backtracking on *that* line
/// that would stall, not the file's total size.
pub struct Limits {
    pub bytes: usize,
    pub lines: usize,
    pub line_bytes: usize,
}

pub const LIMITS: Limits = Limits { bytes: 256 * 1024, lines: 4_000, line_bytes: 4 * 1024 };

/// One line's worth of highlighting: `(from, to, group)`, in byte offsets into that line.
///
/// Byte offsets and not columns, because that is what an extmark takes and what the rest of this
/// workspace means by a column on the wire. Display width is answered in `neosh-tui`, once.
pub type Spans = Vec<(usize, usize, &'static str)>;

/// A language, resolved. Cheap to copy and valid for the life of the process.
#[derive(Clone, Copy)]
pub struct Language(&'static SyntaxReference);

impl Language {
    /// What the grammar calls itself — `Rust`, `TypeScript`. For a card that wants to say so.
    pub fn name(&self) -> &'static str {
        &self.0.name
    }
}

impl std::fmt::Debug for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Language").field(&self.0.name).finish()
    }
}

/// The grammar set, built once.
///
/// `two-face` rather than syntect's bundled defaults, for one specific reason: Sublime's default
/// package has no TypeScript, and this is a workspace whose plugins are written in it. Once you are
/// adding a grammar bundle you may as well have the whole one.
fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

/// Load the grammars now, on a thread of their own, and compile the ones people write in.
///
/// They are loaded the first time anything asks, which left the cost on whatever happened to ask
/// first: the first code block of an answer, or the first switch into a conversation with a diff
/// in it, on the loop that answers keys. Loading the set is only half of it, too — syntect
/// deserialises a grammar's contexts and compiles its patterns the first time a line in that
/// language is parsed, which is a tenth of a second for TypeScript and was paid by the first
/// card of every workspace. So a few lines that reach the common constructs are parsed here, in
/// each language a diff is likely to be in. A caller asking while this is still going waits for
/// nothing: the compiled patterns are shared, and whichever thread gets to one first compiles it.
pub fn warm() {
    let _ = std::thread::Builder::new().name("neosh-syntax".into()).spawn(|| {
        let _ = syntaxes();
        for token in WARM {
            let Some(lang) = of_token(token) else { continue };
            let mut h = Incremental::new(lang);
            for line in SAMPLE.lines() {
                h.line(line);
            }
        }
    });
}

/// The languages [`warm`] compiles, most likely first.
const WARM: &[&str] = &[
    "tsx", "ts", "rs", "js", "jsx", "md", "json", "sh", "py", "go", "swift", "css", "html", "toml",
    "yaml", "kt", "java", "c", "cpp", "rb", "sql",
];

/// Something like code in most of them: enough to walk a grammar through imports, declarations,
/// strings, templates, comments, calls and markup, which is where its patterns live.
const SAMPLE: &str = r#"import { a, type B } from "./b";
// a comment, and /* another */
export async function f<T>(x: number, y = 'y'): Promise<string> {
  const s = `${x} and ${y}`; let n = 0x1F + 2.5e3;
  if (x > 1 && !y) { return s.trim(); } else { throw new Error("no"); }
}
const C = () => <div className="x" onClick={() => f(1)}>{n}</div>;
fn main() -> Result<(), Box<dyn Error>> { let v: Vec<u8> = vec![1]; println!("{v:?}"); Ok(()) }
def g(self, *args, **kw): return [i for i in args if i]  # py
func h(s string) (int, error) { return len(s), nil }
# Heading
- [x] item with `code` and **bold**
{"key": [1, true, null], "o": {"p": "q"}}
"#;

/// The language a file of this name is written in.
///
/// Extension first and then the whole file name, so `Cargo.toml` resolves by its extension and
/// `Makefile`, which has none, resolves by being called that.
pub fn of_path(path: &str) -> Option<Language> {
    let set = syntaxes();
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let by_ext = name.rsplit_once('.').and_then(|(_, ext)| set.find_syntax_by_extension(ext));
    by_ext.or_else(|| set.find_syntax_by_extension(name)).map(Language)
}

/// The language a fence names: ```` ```rust ````, ```` ```ts ````, ```` ```sh ````.
///
/// Tried as an extension, then as a name, then as a token — which between them cover what people
/// actually write on a fence, since `ts` is an extension, `TypeScript` is a name and `typescript`
/// is neither.
pub fn of_token(token: &str) -> Option<Language> {
    let set = syntaxes();
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    set.find_syntax_by_extension(token)
        .or_else(|| set.find_syntax_by_name(token))
        .or_else(|| set.find_syntax_by_token(token))
        .map(Language)
}

/// Highlight `text`, one [`Spans`] per line.
///
/// The result has exactly as many entries as `text` has lines, so a caller can zip it against its
/// own rows without checking. Returns `None` past [`LIMITS`], which means "draw it plain" and not
/// "something went wrong": a diff of a minified file is still a diff.
///
/// The whole text is parsed in one pass even when the caller will only draw part of it, because a
/// grammar is a state machine and line forty of a block comment is only a comment if lines one
/// through thirty-nine were parsed first. Highlighting a folded diff line by line is how you get a
/// card where half a doc comment is grey and the other half is code.
pub fn spans(lang: Language, text: &str) -> Option<Vec<Spans>> {
    let lines: Vec<&str> = split(text);
    if text.len() > LIMITS.bytes
        || lines.len() > LIMITS.lines
        || lines.iter().any(|l| l.len() > LIMITS.line_bytes)
    {
        return None;
    }
    let key = memo_key(lang, text);
    if let Some(hit) = MEMO.lock().ok().and_then(|m| m.get(key)) {
        return Some(hit);
    }
    if defer(lang, text, key) {
        return None;
    }
    let mut h = Incremental::new(lang);
    let out: Vec<Spans> = lines.iter().map(|line| h.line(line)).collect();
    if let Ok(mut m) = MEMO.lock() {
        m.put(key, out.clone());
    }
    Some(out)
}

/// What [`spans`] answered recently, by language and text.
///
/// A transcript is rebuilt from its messages whenever a conversation is arrived in or the window
/// changes width, and every diff in it is highlighted again each time — the same text, the same
/// answer. Bounded by the *lines* it holds, oldest out first, because that is what its memory is:
/// it was 512 entries, and one long conversation is a hundred and fifty of them, so switching
/// between three of them evicted each one before it was come back to and every switch was a cold
/// one. Sixty thousand lines is a few dozen long conversations and a few megabytes.
static MEMO: std::sync::Mutex<Memo> = std::sync::Mutex::new(Memo::new());

const MEMO_LINES: usize = 60_000;

struct Memo {
    map: Option<std::collections::HashMap<u64, Vec<Spans>>>,
    order: std::collections::VecDeque<u64>,
    lines: usize,
}

impl Memo {
    const fn new() -> Self {
        Self { map: None, order: std::collections::VecDeque::new(), lines: 0 }
    }

    fn get(&self, key: u64) -> Option<Vec<Spans>> {
        self.map.as_ref()?.get(&key).cloned()
    }

    fn has(&self, key: u64) -> bool {
        self.map.as_ref().is_some_and(|m| m.contains_key(&key))
    }

    fn put(&mut self, key: u64, value: Vec<Spans>) {
        let map = self.map.get_or_insert_with(Default::default);
        let n = value.len().max(1);
        match map.insert(key, value) {
            Some(old) => self.lines = self.lines - old.len().max(1) + n,
            None => {
                self.order.push_back(key);
                self.lines += n;
            }
        }
        while self.lines > MEMO_LINES && self.order.len() > 1 {
            if let Some(old) = self.order.pop_front()
                && let Some(gone) = map.remove(&old)
            {
                self.lines -= gone.len().max(1);
            }
        }
    }
}

// Highlighting asked for while [`deferring`], waiting to be done somewhere else.
//
// Thread-local because it is a property of the call in progress on *this* thread — a rebuild on
// the host loop — and not of the memo, which every thread shares: a thread filling the memo in
// the background must parse what it is asked rather than defer it to itself.
thread_local! {
    static DEFERRED: RefCell<Option<Vec<Job>>> = const { RefCell::new(None) };
}

/// One piece of text waiting to be highlighted.
struct Job {
    lang: Language,
    text: String,
    key: u64,
}

/// What a [`deferring`] call left unhighlighted, in the order it was asked for.
#[derive(Default)]
pub struct Pending {
    jobs: Vec<Job>,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// The last `n` asked for, taken off this one. A transcript is built top to bottom and read at
    /// the bottom, so these are the ones on screen.
    pub fn split_newest(&mut self, n: usize) -> Pending {
        let at = self.jobs.len().saturating_sub(n);
        Pending { jobs: self.jobs.split_off(at) }
    }

    /// Highlight all of it into the memo, newest first, so the next [`spans`] for any of it is a
    /// lookup. Slow — this is the work deferring moved — so it belongs on a thread of its own.
    pub fn run(self) {
        for job in self.jobs.into_iter().rev() {
            if MEMO.lock().is_ok_and(|m| m.has(job.key)) {
                continue;
            }
            let mut h = Incremental::new(job.lang);
            let out: Vec<Spans> = split(&job.text).iter().map(|line| h.line(line)).collect();
            if let Ok(mut m) = MEMO.lock() {
                m.put(job.key, out);
            }
        }
    }
}

/// Run `f` with every highlight the memo does not already hold drawn plain, and say which.
///
/// For a caller that would rather draw *now* and colour in a moment than make somebody wait for
/// the colour: [`spans`] answers `None` for anything it would have had to parse — exactly the
/// "draw it plain" answer it gives past [`LIMITS`], so no caller needs a second path — and the
/// texts come back as a [`Pending`] to [`run`](Pending::run) elsewhere. Nests: an inner call
/// collects its own, and the outer one's carry on afterwards.
pub fn deferring<R>(f: impl FnOnce() -> R) -> (R, Pending) {
    struct Restore(Option<Vec<Job>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let outer = self.0.take();
            DEFERRED.with(|d| *d.borrow_mut() = outer);
        }
    }
    let outer = DEFERRED.with(|d| d.borrow_mut().replace(Vec::new()));
    let restore = Restore(outer);
    let out = f();
    let jobs = DEFERRED.with(|d| d.borrow_mut().take()).unwrap_or_default();
    drop(restore);
    (out, Pending { jobs })
}

/// Note a miss for the [`deferring`] call in progress, if there is one. `true` when there was,
/// which means the caller should answer "plain" rather than parse.
fn defer(lang: Language, text: &str, key: u64) -> bool {
    DEFERRED.with(|d| match d.borrow_mut().as_mut() {
        Some(jobs) => {
            if !jobs.iter().any(|j| j.key == key) {
                jobs.push(Job { lang, text: text.to_string(), key });
            }
            true
        }
        None => false,
    })
}

fn memo_key(lang: Language, text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    lang.0.name.hash(&mut h);
    text.hash(&mut h);
    h.finish()
}

/// A highlighter part-way through a piece of code.
///
/// The same parse [`spans`] does, one line at a time, for the caller that only ever *has* one line
/// at a time — a fenced block in an answer that is still streaming. Keeping the parser between
/// lines is the whole of it: a fence is drawn as it arrives, and a highlighter restarted per line
/// would forget it was inside a string every time a newline went past.
///
/// `Clone`, because a streaming caller keeps one for the lines it has banked and highlights the
/// half-arrived line after them on a copy: a partial line fed to the real one would leave it parsed
/// past text that is about to change.
#[derive(Clone)]
pub struct Incremental {
    state: ParseState,
    stack: ScopeStack,
}

impl std::fmt::Debug for Incremental {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Incremental").finish_non_exhaustive()
    }
}

impl Incremental {
    pub fn new(lang: Language) -> Self {
        Self { state: ParseState::new(lang.0), stack: ScopeStack::new() }
    }

    /// The next line, without its newline. Returns what to colour on it.
    ///
    /// A line past [`LIMITS::line_bytes`] is skipped rather than parsed — and skipped *without*
    /// advancing the parser, since feeding it half-parsed would leave the state wrong for every
    /// line after it. It comes back plain, which is what a line nobody can read comes back as.
    pub fn line(&mut self, text: &str) -> Spans {
        if text.len() > LIMITS.line_bytes {
            return Vec::new();
        }
        // syntect wants the newline: several grammars close a construct on it, and a line fed
        // without one leaves the parser thinking the construct is still open.
        let owned = format!("{text}\n");
        // A grammar that cannot parse its own language is not something to fail a card over.
        let Ok(ops) = self.state.parse_line(&owned, syntaxes()) else { return Vec::new() };
        row(&mut self.stack, &ops, text.len())
    }
}

/// One line's spans, walking the stack operations the parser emitted for it.
///
/// The operations arrive as `(byte offset, push/pop)`. Everything between one offset and the next
/// is whatever the stack said in between, so this closes a span at each offset, applies the
/// operation, and opens the next — and coalesces neighbours that resolved to the same group, since
/// a `meta.` scope pushed and popped around a word would otherwise cut it into three spans that
/// are all the same colour.
fn row(stack: &mut ScopeStack, ops: &[(usize, ScopeStackOp)], len: usize) -> Spans {
    let mut out: Spans = Vec::new();
    let mut at = 0usize;
    let push = |out: &mut Spans, from: usize, to: usize, group: &'static str| {
        if from >= to {
            return;
        }
        match out.last_mut() {
            Some(last) if last.1 == from && last.2 == group => last.1 = to,
            _ => out.push((from, to, group)),
        }
    };
    for (offset, op) in ops {
        let to = (*offset).min(len);
        push(&mut out, at, to, group_of(stack));
        at = at.max(to);
        // A malformed op is the grammar's problem and not the card's; the stack simply does not
        // move and the rest of the line is highlighted with what it already said.
        let _ = stack.apply(op);
    }
    push(&mut out, at, len, group_of(stack));
    out.retain(|(_, _, g)| *g != TEXT);
    out
}

/// Which of the seven a scope stack resolves to.
///
/// Read from the top down, because the top is the most specific thing the grammar decided. Two
/// wrinkles, both of which are visible the moment you get them wrong:
///
/// - **Punctuation only wins if nothing else does.** The quotes around a string are pushed as
///   `punctuation.definition.string.begin` *on top of* `string.quoted.double`, so a naive
///   top-of-stack read paints the quotes one colour and their contents another — which looks like
///   a rendering bug rather than a decision. So punctuation is remembered and only used if the
///   whole stack turns up nothing else.
/// - **The table is ordered longest-prefix first**, so `entity.name.type` is a type before
///   `entity` is anything, and `constant.numeric` is a number before `constant` is a constant.
fn group_of(stack: &ScopeStack) -> &'static str {
    let mut punct = false;
    for scope in stack.as_slice().iter().rev() {
        match group_of_scope(*scope) {
            Some(PUNCT) => punct = true,
            Some(g) => return g,
            None => {}
        }
    }
    if punct { PUNCT } else { TEXT }
}

/// Every scope prefix that means something here, longest first.
///
/// A table rather than syntect's selector matching, which is built for themes deciding between
/// dozens of overlapping rules. There are seven buckets; the shape that fits seven buckets is a
/// list of prefixes read in order.
const SCOPES: &[(&str, &str)] = &[
    ("comment", "Syntax.Comment"),
    ("string", "Syntax.String"),
    ("constant.numeric", "Syntax.Number"),
    ("constant.character.escape", "Syntax.Constant"),
    ("constant.language", "Syntax.Constant"),
    ("constant.other", "Syntax.Constant"),
    ("constant", "Syntax.Constant"),
    ("entity.name.function", "Syntax.Function"),
    ("entity.name.type", "Syntax.Type"),
    ("entity.name.class", "Syntax.Type"),
    ("entity.name.struct", "Syntax.Type"),
    ("entity.name.enum", "Syntax.Type"),
    ("entity.name.interface", "Syntax.Type"),
    ("entity.name.trait", "Syntax.Type"),
    ("entity.name.namespace", "Syntax.Type"),
    ("entity.name.module", "Syntax.Type"),
    // A tag is the keyword of a markup language: `<div>` is to HTML what `if` is to Rust.
    ("entity.name.tag", "Syntax.Keyword"),
    ("entity.other.attribute-name", "Syntax.Type"),
    ("entity.other.inherited-class", "Syntax.Type"),
    ("entity.name", "Syntax.Function"),
    ("variable.function", "Syntax.Function"),
    ("variable.annotation", "Syntax.Constant"),
    ("variable.language", "Syntax.Keyword"),
    ("support.function", "Syntax.Function"),
    ("support.macro", "Syntax.Function"),
    ("support.class", "Syntax.Type"),
    ("support.type", "Syntax.Type"),
    ("support.constant", "Syntax.Constant"),
    // `fn`, `let`, `class`, `const`: the grammar files these under storage rather than keyword,
    // and to a reader they are the same word class. Rust's grammar also files `usize` and `u8`
    // here, under the very same `storage.type.rust` as `let` — so a primitive reads as a keyword
    // rather than a type, and no table could tell them apart, because the grammar did not.
    ("storage", "Syntax.Keyword"),
    // An operator is punctuation that happens to be spelled with letters sometimes. Quiet, so the
    // words that control the flow of the code stay the loud ones: a line where `=` and `if` are
    // the same colour has no shape to skim.
    ("keyword.operator", "Syntax.Punct"),
    ("keyword", "Syntax.Keyword"),
    ("meta.annotation", "Syntax.Constant"),
    ("meta.attribute", "Syntax.Constant"),
    ("punctuation", "Syntax.Punct"),
];

fn group_of_scope(scope: Scope) -> Option<&'static str> {
    // Asked for every scope on the stack at every token of every line, so it is a comparison of
    // two packed integers rather than of two strings: `is_prefix_of` is atom-wise, which is
    // exactly "equal, or this followed by a dot". Spelled as strings it was an allocation for the
    // scope's name and another for each prefix tried — most of the time a diff took to colour.
    static TABLE: OnceLock<Vec<(Scope, &'static str)>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            SCOPES
                .iter()
                .filter_map(|(prefix, group)| Scope::new(prefix).ok().map(|s| (s, *group)))
                .collect()
        })
        .iter()
        .find(|(prefix, _)| prefix.is_prefix_of(scope))
        .map(|(_, group)| *group)
}

/// The lines of `text`, where the empty string is no lines rather than one empty one.
///
/// The same convention `neosh/src/diff.rs` uses, and for the same reason: a caller zipping spans
/// against rows must get the same count from both.
fn split(text: &str) -> Vec<&str> {
    if text.is_empty() { Vec::new() } else { text.lines().collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups(lang: &str, code: &str) -> Vec<Vec<(&'static str, String)>> {
        let l = of_token(lang).expect("a grammar for the test language");
        spans(l, code)
            .expect("under the limits")
            .iter()
            .zip(code.lines())
            .map(|(spans, line)| {
                spans.iter().map(|(a, b, g)| (*g, line[*a..*b].to_string())).collect()
            })
            .collect()
    }

    fn find<'a>(rows: &'a [Vec<(&'static str, String)>], text: &str) -> Option<&'a str> {
        rows.iter().flatten().find(|(_, t)| t.trim() == text).map(|(g, _)| *g)
    }

    #[test]
    fn a_language_resolves_from_a_fence_token_a_name_or_an_extension() {
        assert_eq!(of_token("rs").map(|l| l.name()), Some("Rust"));
        assert_eq!(of_token("rust").map(|l| l.name()), Some("Rust"));
        assert_eq!(of_token("Rust").map(|l| l.name()), Some("Rust"));
        assert!(of_token("").is_none());
        assert!(of_token("no-such-language").is_none());
    }

    #[test]
    fn a_language_resolves_from_a_path_including_one_with_no_extension() {
        assert_eq!(of_path("crates/neosh/src/cards.rs").map(|l| l.name()), Some("Rust"));
        assert_eq!(of_path("/tmp/x/Cargo.toml").map(|l| l.name()), Some("TOML"));
        assert!(of_path("Makefile").is_some());
        assert!(of_path("some/file.qqzz").is_none());
    }

    #[test]
    fn typescript_resolves_because_the_plugins_are_written_in_it() {
        // Sublime's default package has no TypeScript grammar, which is the entire reason this
        // crate carries `two-face` rather than syntect's bundled defaults.
        assert!(of_token("ts").is_some(), "no TypeScript grammar");
        let rows = groups("ts", "const x: number = 1;\n");
        assert_eq!(find(&rows, "const"), Some("Syntax.Keyword"));
        assert_eq!(find(&rows, "1"), Some("Syntax.Number"));
    }

    #[test]
    fn the_seven_buckets_catch_what_a_line_of_rust_is_made_of() {
        let rows = groups(
            "rust",
            "// a note\nfn take(n: usize) -> String {\n    String::from(\"hi\")\n}\n",
        );
        assert_eq!(find(&rows, "// a note"), Some("Syntax.Comment"));
        assert_eq!(find(&rows, "fn"), Some("Syntax.Keyword"));
        assert_eq!(find(&rows, "take"), Some("Syntax.Function"));
        // `usize` comes back a keyword and not a type, and that is the grammar's answer rather
        // than a bug here: Rust's syntax scopes `let` and `usize` identically. Asserted so the
        // next person to notice it reads this instead of trying to fix it.
        assert_eq!(find(&rows, "usize"), Some("Syntax.Keyword"));
        let rows = groups("rust", "let v: Vec<u8> = f();\n");
        assert_eq!(find(&rows, "Vec"), Some("Syntax.Type"));
        assert_eq!(find(&rows, "="), Some("Syntax.Punct"), "an operator is not a keyword");
    }

    #[test]
    fn a_strings_quotes_are_part_of_the_string_and_not_punctuation() {
        // The wrinkle this whole `group_of` exists for: the quote is pushed as a punctuation scope
        // on top of the string scope, and reading the top of the stack naively paints the two ends
        // of a literal a different colour from its middle.
        let rows = groups("rust", "let s = \"hello\";\n");
        let whole: String = rows[0]
            .iter()
            .filter(|(g, _)| *g == "Syntax.String")
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(whole, "\"hello\"", "{:?}", rows[0]);
    }

    #[test]
    fn a_block_comment_stays_a_comment_on_its_second_line() {
        // The reason the whole text is parsed in one pass rather than line by line: line two is
        // only a comment because line one opened one.
        let rows = groups("rust", "/* one\n   two */\nfn f() {}\n");
        assert_eq!(rows[1].first().map(|(g, _)| *g), Some("Syntax.Comment"), "{:?}", rows[1]);
    }

    #[test]
    fn plain_text_produces_no_spans_at_all() {
        // `Syntax.Text` is the absence of an opinion, and an absence should not cost a mark per
        // word — a card of ordinary output would otherwise carry hundreds of them.
        let l = of_token("txt").or_else(|| of_token("Plain Text")).expect("plain text");
        let rows = spans(l, "just some words\n").expect("under the limits");
        assert!(rows[0].is_empty(), "{:?}", rows[0]);
    }

    #[test]
    fn every_group_produced_is_one_the_palette_knows() {
        let rows = groups("rust", "// c\nfn f(n: u8) -> &'static str { \"s\" }\n");
        for (g, _) in rows.iter().flatten() {
            assert!(GROUPS.contains(g), "{g} is not in GROUPS");
        }
    }

    #[test]
    fn there_is_one_row_of_spans_for_every_line_of_input() {
        let l = of_token("rust").expect("rust");
        assert_eq!(spans(l, "a\nb\nc").expect("spans").len(), 3);
        assert_eq!(spans(l, "").expect("spans").len(), 0, "\"\" is no lines, not one empty one");
        assert_eq!(spans(l, "a\n").expect("spans").len(), 1);
    }

    #[test]
    fn a_line_too_long_to_parse_cheaply_is_refused_rather_than_parsed() {
        // A minified bundle in a diff. Drawing it plain is what happened before this crate; taking
        // an unbounded amount of the host loop to draw it in colour is not.
        let l = of_token("js").expect("javascript");
        let long = format!("var x = \"{}\";", "a".repeat(LIMITS.line_bytes));
        assert!(spans(l, &long).is_none());
    }

    #[test]
    fn deferring_draws_plain_now_and_the_same_colours_once_run() {
        let l = of_token("rust").expect("rust");
        // Text nothing else in this test binary asks for, so the memo cannot already hold it.
        let code = "fn deferred_only_here(n: u8) -> u8 { n + 1 } // once\n";
        let (now, pending) = deferring(|| spans(l, code));
        assert!(now.is_none(), "a miss under deferring is drawn plain");
        assert_eq!(pending.len(), 1);
        // Asked twice in one rebuild, it is still one job.
        let (_, twice) = deferring(|| (spans(l, "let twice = 1;\n"), spans(l, "let twice = 1;\n")));
        assert_eq!(twice.len(), 1);
        pending.run();
        let (later, none) = deferring(|| spans(l, code));
        assert!(none.is_empty(), "what was run is remembered");
        let mut h = Incremental::new(l);
        let direct: Vec<Spans> = code.lines().map(|line| h.line(line)).collect();
        assert_eq!(later, Some(direct));
        // And outside a deferring call nothing is deferred at all.
        assert!(spans(l, "let outside = 2;\n").is_some());
    }

    #[test]
    fn the_newest_jobs_split_off_the_end() {
        let l = of_token("rust").expect("rust");
        let (_, mut pending) = deferring(|| {
            for i in 0..5 {
                let _ = spans(l, &format!("let split_{i} = {i};\n"));
            }
        });
        let newest = pending.split_newest(2);
        assert_eq!((pending.len(), newest.len()), (3, 2));
    }

    #[test]
    fn the_memo_is_bounded_by_the_lines_it_holds() {
        let mut m = Memo::new();
        let row = |n: usize| vec![Vec::new(); n];
        m.put(1, row(MEMO_LINES - 10));
        m.put(2, row(5));
        assert!(m.has(1) && m.has(2));
        m.put(3, row(20));
        assert!(!m.has(1), "the oldest goes first");
        assert!(m.has(2) && m.has(3));
        assert_eq!(m.lines, 25);
    }

    #[test]
    fn spans_are_byte_ranges_that_land_on_character_boundaries() {
        // Columns on the wire are UTF-8 byte offsets, and a span that cuts a multi-byte character
        // in half panics the moment somebody slices a line with it.
        let l = of_token("rust").expect("rust");
        let code = "let s = \"héllo wörld\"; // ünicode\n";
        for (a, b, _) in spans(l, code).expect("spans").iter().flatten() {
            assert!(code.is_char_boundary(*a) && code.is_char_boundary(*b), "{a}..{b}");
        }
    }
}

