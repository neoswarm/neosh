//! Reading the transcript: the modal reader over the chat buffer.
//!
//! The point of the mode is that the answer is the artefact. Everything here is about getting a
//! piece of it out — a line, a code block, a whole exchange — or getting to the piece you want:
//! turn by turn, block by block, or by searching for it.
//!
//! Driven through the stdio protocol, so these assert on what a *frontend* is told. The clipboard
//! write is a `UiEvent`; if it stops being emitted these fail.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BUDGET: Duration = Duration::from_secs(20);

struct Sandbox {
    root: PathBuf,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("neosh-read-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["config", "state", "work"] {
            std::fs::create_dir_all(root.join(d)).expect("dirs");
        }
        Self { root }
    }

    /// A conversation with one markdown answer in it: a heading, a list, a fenced block of Rust,
    /// and a closing sentence. Enough shape that "the block under the cursor" is a real question.
    fn start(&self) -> Session {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/markdown_turn.jsonl");
        let child = Command::new(env!("CARGO_BIN_EXE_neosh"))
            .args(["--ui-protocol", "stdio"])
            .arg("--config-dir")
            .arg(self.root.join("config"))
            .arg("--cwd")
            .arg(self.root.join("work"))
            .args(["--mock-script", &fixture.display().to_string()])
            .args(["--model", "mock/mock"])
            .env("NEOSH_STATE_DIR", self.root.join("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start");
        Session::wrap(child)
    }
}

struct Session {
    child: Child,
    lines: Receiver<String>,
    events: Vec<Value>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Session {
    fn wrap(mut child: Child) -> Self {
        let out = BufReader::new(child.stdout.take().expect("stdout"));
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in out.lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut s = Self { child, lines, events: Vec::new() };
        s.send(&json!({"type": "ready", "width": 100, "height": 30}));
        s
    }

    fn send(&mut self, v: &Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{v}").expect("write");
        stdin.flush().expect("flush");
    }

    fn press(&mut self, code: Value, mods: &[&str]) {
        let m: serde_json::Map<String, Value> =
            mods.iter().map(|k| ((*k).to_string(), json!(true))).collect();
        self.send(&json!({"type": "key", "key": {"code": code, "mods": m}}));
    }

    fn ch(&mut self, c: &str) {
        self.press(json!({"kind": "char", "c": c}), &[]);
    }

    fn keys(&mut self, text: &str) {
        for c in text.chars() {
            self.ch(&c.to_string());
        }
    }

    fn special(&mut self, kind: &str) {
        self.press(json!({"kind": kind}), &[]);
    }

    fn pump(&mut self, pred: impl Fn(&Session) -> bool) -> bool {
        let deadline = Instant::now() + BUDGET;
        loop {
            if pred(self) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        self.events.push(v);
                    }
                }
                Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout) => {
                    return pred(self);
                }
            }
        }
    }

    fn buffer_named(&self, name: &str) -> Option<u64> {
        self.events.iter().find_map(|e| {
            (e["type"] == "buffer_opened" && e["name"] == name).then(|| e["buf"].as_u64())?
        })
    }

    fn lines_of(&self, buf: u64) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = e["start"].as_u64().unwrap_or(0) as usize;
            let old_end = e["old_end"].as_i64().unwrap_or(0);
            let end = if old_end < 0 { lines.len() } else { (old_end as usize).min(lines.len()) };
            let new: Vec<String> = e["lines"]
                .as_array()
                .map(|a| {
                    a.iter().map(|l| l["text"].as_str().unwrap_or("").to_string()).collect()
                })
                .unwrap_or_default();
            let start = start.min(lines.len());
            lines.splice(start..end.max(start), new);
        }
        lines
    }

    fn chat(&self) -> Vec<String> {
        let name = self.events.iter().find_map(|e| {
            let n = e["name"].as_str()?;
            (e["type"] == "buffer_opened" && n.starts_with("[chat]")).then(|| n.to_string())
        });
        name.and_then(|n| self.buffer_named(&n)).map(|b| self.lines_of(b)).unwrap_or_default()
    }

    fn composer(&self) -> Vec<String> {
        self.buffer_named("[composer]").map(|b| self.lines_of(b)).unwrap_or_default()
    }

    fn copied(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| e["type"] == "clipboard")
            .filter_map(|e| e["text"].as_str().map(str::to_string))
            .collect()
    }

    fn last_copy(&self) -> String {
        self.copied().last().cloned().unwrap_or_default()
    }

    fn messages(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| e["type"] == "message")
            .filter_map(|e| e["text"].as_str().map(str::to_string))
            .collect()
    }

    /// Where the cursor is in the transcript, from the events the frontend was sent.
    fn chat_cursor(&self) -> Option<u64> {
        let chat_win = self.events.iter().find_map(|e| {
            let n = e["name"].as_str()?;
            (e["type"] == "buffer_opened" && n.starts_with("[chat]")).then_some(())?;
            let buf = e["buf"].as_u64()?;
            self.events.iter().find_map(|w| {
                (w["type"] == "window_opened" && w["buf"].as_u64() == Some(buf))
                    .then(|| w["win"].as_u64())?
            })
        })?;
        self.events
            .iter()
            .rev()
            .find(|e| e["type"] == "cursor_moved" && e["win"].as_u64() == Some(chat_win))
            .and_then(|e| e["row"].as_u64())
    }

    /// To the top of the transcript, waiting until the cursor is actually there.
    ///
    /// The waiting is the point: `pump` stops as soon as its predicate holds, so asking whether
    /// the cursor exists right after a keypress answers from the events already read — which is
    /// where it was *before* the key. Every assertion about a motion has to name where it expects
    /// to end up.
    fn to_top(&mut self) {
        self.ch("g");
        self.ch("g");
        assert!(self.pump(|s| s.chat_cursor() == Some(0)), "at the top: {:?}", self.chat_cursor());
    }

    fn ready(&mut self) {
        assert!(self.pump(|s| s.buffer_named("[composer]").is_some()), "the composer exists");
    }

    /// Ask the question the fixture answers, wait for the whole answer, then move into it.
    fn answered_and_reading(&mut self) {
        self.ready();
        self.keys("go");
        self.special("enter");
        assert!(
            self.pump(|s| s.chat().iter().any(|l| l.contains("That is all."))),
            "the answer landed\n{:?}",
            self.chat()
        );
        self.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
        assert!(
            self.pump(|s| s.buffer("[status]").iter().any(|l| l.contains("reading"))),
            "and the keyboard moved into it"
        );
    }

    fn buffer(&self, name: &str) -> Vec<String> {
        self.buffer_named(name).map(|b| self.lines_of(b)).unwrap_or_default()
    }

    /// How many rows of the transcript are lines of `file`, which is what a read's card shows
    /// when it is showing anything at all.
    fn showing(&self, file: &str) -> usize {
        self.chat().iter().filter(|l| l.contains(&format!("{file} line "))).count()
    }
}

/// A conversation with two tool cards in it, with an answer between them so they are two cards
/// rather than one run of reads.
impl Sandbox {
    fn with_cards(&self) -> Session {
        self.cards_at(None)
    }

    /// The same, with the mock answering slowly enough that the turn is still going while keys
    /// are being pressed — which is the only way to arrange "something arrived while you were
    /// reading" without a real model and a real wait.
    fn with_slow_cards(&self) -> Session {
        self.cards_at(Some("120"))
    }

    fn cards_at(&self, pace: Option<&str>) -> Session {
        for (name, n) in [("one.txt", 40), ("two.txt", 40)] {
            let body: String =
                (1..=n).map(|i| format!("{name} line {i}\n")).collect::<Vec<_>>().concat();
            std::fs::write(self.root.join("work").join(name), body).expect("fixture file");
        }
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tool_cards.jsonl");
        let child = Command::new(env!("CARGO_BIN_EXE_neosh"))
            .args(["--ui-protocol", "stdio"])
            .arg("--config-dir")
            .arg(self.root.join("config"))
            .arg("--cwd")
            .arg(self.root.join("work"))
            .args(["--mock-script", &fixture.display().to_string()])
            .args(["--model", "mock/mock"])
            .env("NEOSH_STATE_DIR", self.root.join("state"))
            .envs(pace.map(|ms| ("NEOSH_MOCK_DELAY_MS", ms)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start");
        Session::wrap(child)
    }
}

// ---------------------------------------------------------------------------
// Getting a piece of the answer out
// ---------------------------------------------------------------------------

/// A count before a motion, the way it works everywhere these keys come from.
///
/// `5j` is five lines and `3G` is the third one — the count is a repetition on a motion and a
/// destination on `G`, which is the distinction the digits are for. `0` is the start of the line
/// when nothing has been typed and part of the number when something has.
#[test]
fn a_count_repeats_a_motion_and_places_a_jump() {
    let sb = Sandbox::new("counts");
    let mut s = sb.start();
    s.answered_and_reading();
    s.to_top();

    s.ch("5");
    // A digit on its own moves nothing and says so: the strip is the only thing that can tell you
    // the first of two keystrokes landed anywhere.
    assert!(
        s.pump(|s| s.chat_cursor() == Some(0)),
        "nothing has moved yet: {:?}",
        s.chat_cursor()
    );
    s.ch("j");
    assert!(
        s.pump(|s| s.chat_cursor() == Some(5)),
        "five lines, not one: {:?}",
        s.chat_cursor()
    );

    // And the count is spent — the next `j` is one line, not another five.
    s.ch("j");
    assert!(s.pump(|s| s.chat_cursor() == Some(6)), "one line: {:?}", s.chat_cursor());

    // `G` with a number in front of it is a row rather than that many of anything.
    s.ch("3");
    s.ch("G");
    assert!(s.pump(|s| s.chat_cursor() == Some(2)), "the third row: {:?}", s.chat_cursor());

    // And `gg` takes one too.
    s.ch("4");
    s.ch("g");
    s.ch("g");
    assert!(s.pump(|s| s.chat_cursor() == Some(3)), "the fourth row: {:?}", s.chat_cursor());
}

/// `yc` takes the code and nothing else: not the language line above it, not the indent the
/// renderer added, not the sentence after it.
///
/// This is the one people came for. An answer with a shell command in it is worthless if getting
/// the command means selecting it by hand across a wrapped line.
#[test]
fn yank_code_takes_the_block_without_its_chrome() {
    let sb = Sandbox::new("yankcode");
    let mut s = sb.start();
    s.answered_and_reading();

    // Onto the code by searching for it, which is also the honest way to find it.
    s.ch("/");
    s.keys("fn main");
    s.special("enter");
    assert!(s.pump(|s| s.composer() == vec![""]), "the search prompt gave the composer back");

    s.ch("y");
    s.ch("c");
    assert!(s.pump(|s| !s.copied().is_empty()), "something was copied\n{:?}", s.messages());

    let got = s.last_copy();
    assert!(got.starts_with("fn main"), "no indent, no language line: {got:?}");
    assert!(got.contains("## not a heading"), "the whole block: {got:?}");
    assert!(!got.contains("rust"), "the language line is chrome: {got:?}");
    assert!(!got.contains("That is all"), "and it stops at the end of the block: {got:?}");
}

/// Standing on the language line is standing on the block. It is drawn as part of it and it is
/// where the cursor lands coming down from above; refusing there is pedantry about a distinction
/// only the parser cares about.
#[test]
fn yank_code_works_from_the_language_line_too() {
    let sb = Sandbox::new("yankfence");
    let mut s = sb.start();
    s.answered_and_reading();

    s.ch("/");
    s.keys("fn main");
    s.special("enter");
    s.ch("k"); // up onto `  rust`
    s.ch("y");
    s.ch("c");
    assert!(s.pump(|s| !s.copied().is_empty()), "something was copied\n{:?}", s.messages());
    assert!(s.last_copy().starts_with("fn main"), "{:?}", s.last_copy());
}

/// And it says so rather than copying the wrong thing when there is no block to take.
#[test]
fn yank_code_outside_a_block_copies_nothing_and_says_why() {
    let sb = Sandbox::new("yanknocode");
    let mut s = sb.start();
    s.answered_and_reading();

    s.to_top();
    s.ch("y");
    s.ch("c");
    assert!(
        s.pump(|s| s.messages().iter().any(|m| m.contains("not in a code block"))),
        "{:?}",
        s.messages()
    );
    assert!(s.copied().is_empty(), "and nothing went to the clipboard: {:?}", s.copied());
}

#[test]
fn yank_line_takes_the_line_under_the_cursor() {
    let sb = Sandbox::new("yankline");
    let mut s = sb.start();
    s.answered_and_reading();

    s.ch("/");
    s.keys("Three things");
    s.special("enter");
    s.ch("y");
    s.ch("y");
    assert!(s.pump(|s| !s.copied().is_empty()), "copied\n{:?}", s.messages());
    assert_eq!(s.last_copy(), "Three things:");
}

/// `ym` takes the exchange: what you asked, and everything it produced.
#[test]
fn yank_turn_takes_the_question_and_its_whole_answer() {
    let sb = Sandbox::new("yankturn");
    let mut s = sb.start();
    s.answered_and_reading();

    s.ch("y");
    s.ch("m");
    assert!(s.pump(|s| !s.copied().is_empty()), "copied\n{:?}", s.messages());
    let got = s.last_copy();
    assert!(got.contains("go"), "the question is in it: {got:?}");
    assert!(got.contains("What I found"), "and the start of the answer: {got:?}");
    assert!(got.contains("That is all."), "and the end of it: {got:?}");
}

/// `yp` takes the conversation's directory — in a worktree, the worktree's path — which is the
/// other thing you take out of a conversation: what your shell wants to `cd` to. The one `y`
/// target that is not a piece of the transcript, and the route a Mac takes — `⌥Y` in chat is `¥`
/// there, and is allowed as a default only because this exists.
#[test]
fn yank_path_takes_the_conversations_directory() {
    let sb = Sandbox::new("yankpath");
    let mut s = sb.start();
    s.answered_and_reading();

    s.ch("y");
    s.ch("p");
    assert!(s.pump(|s| !s.copied().is_empty()), "copied\n{:?}", s.messages());
    let want = sb.root.join("work");
    let got = PathBuf::from(s.last_copy());
    // Canonicalised on both sides: a temp dir on macOS is a symlink into `/private`.
    assert_eq!(
        got.canonicalize().unwrap_or(got.clone()),
        want.canonicalize().unwrap_or(want.clone()),
        "the directory, not a row of the transcript: {got:?}"
    );
    assert!(
        s.pump(|s| s.messages().iter().any(|m| m.starts_with("copied ") && !m.contains("line"))),
        "and it says which path, not how many lines\n{:?}",
        s.messages()
    );
}

// ---------------------------------------------------------------------------
// Getting to the piece you want
// ---------------------------------------------------------------------------

/// `[` and `]` step between turns, which is the thing a transcript has that a file does not.
#[test]
fn square_brackets_step_between_turns() {
    let sb = Sandbox::new("turnstep");
    let mut s = sb.start();
    s.answered_and_reading();

    // Back to the start of this turn — the line with the question on it.
    s.ch("[");
    assert!(
        s.pump(|s| {
            let row = s.chat_cursor().unwrap_or(u64::MAX) as usize;
            s.chat().get(row).is_some_and(|l| l.contains("go"))
        }),
        "the cursor is on the question\nrow {:?} of {:?}",
        s.chat_cursor(),
        s.chat()
    );

    // There is no later turn, and it says so rather than moving somewhere arbitrary.
    s.ch("]");
    assert!(s.pump(|s| s.messages().iter().any(|m| m.contains("no later turn"))), "{:?}", s.messages());
}

/// `{` and `}` step block to block, which in this buffer is exactly how the answer was written:
/// the markdown renderer puts a blank line between every block it draws.
#[test]
fn braces_step_between_blocks() {
    let sb = Sandbox::new("blockstep");
    let mut s = sb.start();
    s.answered_and_reading();

    s.to_top();
    let mut seen: Vec<String> = Vec::new();
    let mut at = 0u64;
    for _ in 0..12 {
        s.ch("}");
        // Wait for the cursor to be somewhere new, or for the end of the transcript to say there
        // is nowhere new to be.
        let last = s.chat().len().saturating_sub(1) as u64;
        assert!(s.pump(|s| s.chat_cursor() != Some(at) || at == last));
        at = s.chat_cursor().unwrap_or(at);
        if let Some(text) = s.chat().get(at as usize) {
            seen.push(text.clone());
        }
    }
    assert!(
        seen.iter().all(|l| !l.trim().is_empty()),
        "every stop is on content, never in the gap between two blocks: {seen:?}"
    );
    assert!(
        seen.iter().any(|l| l.contains("Three things")) && seen.iter().any(|l| l.contains("That is all")),
        "and it walks the whole answer: {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// Searching
// ---------------------------------------------------------------------------

#[test]
fn a_search_borrows_the_composer_and_gives_it_back() {
    let sb = Sandbox::new("searchdraft");
    let mut s = sb.start();
    s.ready();
    s.keys("draft");
    assert!(s.pump(|s| s.composer() == vec!["draft"]));

    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    s.ch("/");
    s.keys("neosh");
    assert!(
        s.pump(|s| s.composer() == vec!["/neosh"]),
        "what you are searching for is where you can see it: {:?}",
        s.composer()
    );

    s.special("esc");
    assert!(
        s.pump(|s| s.composer() == vec!["draft"]),
        "and the sentence you were half way through is still there: {:?}",
        s.composer()
    );
}

/// The match under the cursor when you press Enter is the one you land on. `n` must then *move* —
/// a "next match" that stays put is the single most annoying way for a search to be wrong.
#[test]
fn n_moves_on_rather_than_finding_the_match_it_is_already_on() {
    let sb = Sandbox::new("searchnext");
    let mut s = sb.start();
    s.answered_and_reading();

    s.to_top();
    s.ch("/");
    s.keys("the");
    s.special("enter");
    assert!(s.pump(|s| s.chat_cursor().is_some()));
    let first = s.chat_cursor().unwrap_or(0);

    s.ch("n");
    assert!(
        s.pump(|s| s.chat_cursor() != Some(first)),
        "n went somewhere else: still row {:?}",
        s.chat_cursor()
    );
}

#[test]
fn a_search_with_no_match_says_so_and_leaves_you_where_you_were() {
    let sb = Sandbox::new("searchmiss");
    let mut s = sb.start();
    s.answered_and_reading();

    s.to_top();
    let before = s.chat_cursor();

    s.ch("/");
    s.keys("xyzzyplugh");
    s.special("enter");
    assert!(
        s.pump(|s| s.messages().iter().any(|m| m.contains("no match"))),
        "{:?}",
        s.messages()
    );
    assert_eq!(s.chat_cursor(), before, "and it did not wander off");
}

/// Lower case matches anything, a capital means you meant it. `smartcase`, which is what everyone
/// turns on anyway.
#[test]
fn a_capital_in_the_query_makes_the_search_case_sensitive() {
    let sb = Sandbox::new("smartcase");
    let mut s = sb.start();
    s.answered_and_reading();

    // `What I found` is a heading; `what` in lower case still reaches it.
    s.to_top();
    s.ch("/");
    s.keys("what i found");
    s.special("enter");
    assert!(
        s.pump(|s| {
            let row = s.chat_cursor().unwrap_or(u64::MAX) as usize;
            s.chat().get(row).is_some_and(|l| l.contains("What I found"))
        }),
        "lower case found it: {:?}",
        s.chat_cursor()
    );

    // With a capital that does not match, it must not.
    s.ch("/");
    s.keys("WHAT I FOUND");
    s.special("enter");
    assert!(
        s.pump(|s| s.messages().iter().any(|m| m.contains("no match"))),
        "{:?}",
        s.messages()
    );
}

// ---------------------------------------------------------------------------
// The mode itself
// ---------------------------------------------------------------------------

/// The keys mean something different here and they are not written down anywhere else, so the row
/// under the composer says what they are — whatever `ui.hints` is set to, since that setting is
/// about the *typing* shortcuts and this is not typing.
#[test]
fn a_chord_while_reading_is_not_the_letter_it_is_made_of() {
    // Two ways a reading key was quietly taken. `^D` is bound to `git.diff` in `chat` mode, and
    // reading's keys arrive only through the *unbound* path — so it opened a diff instead of
    // moving half a screen. And `^B` did reach `reading_key`, but was matched by the `b` arm of
    // the motion table on the way past, so it moved a word left. Half of a documented vim
    // vocabulary, taken by a plugin and by a letter.
    let sb = Sandbox::new("chords");
    let mut s = sb.start();
    s.answered_and_reading();
    s.to_top();

    // Down half a screen. The transcript is longer than the window, so the row has to change.
    s.press(json!({"kind": "char", "c": "d"}), &["ctrl"]);
    assert!(
        s.pump(|s| s.chat_cursor().is_some_and(|r| r > 0)),
        "^D moved rather than opening a diff\n{:?}",
        s.messages()
    );
    let down = s.chat_cursor().unwrap_or(0);

    // Back a screen — not a word left, which would leave the cursor on the same row.
    s.press(json!({"kind": "char", "c": "b"}), &["ctrl"]);
    assert!(
        s.pump(|s| s.chat_cursor().is_some_and(|r| r < down)),
        "^B went back a screen rather than a word\n{down} -> {:?}",
        s.chat_cursor()
    );

    // And `^Y`, whose letter is the yank prefix, scrolls the window rather than opening one.
    // Vim's pair with `^E`: the other five scroll keys move the cursor and let the view follow,
    // and these two are the ones that do it the other way round.
    s.chat_viewport(100, 6);
    for _ in 0..3 {
        s.press(json!({"kind": "char", "c": "d"}), &["ctrl"]);
    }
    assert!(
        s.pump(|s| s.chat_scrolls().last().copied().flatten().is_some_and(|t| t > 0)),
        "the window is somewhere with room above it: {:?}",
        s.chat_scrolls()
    );
    s.press(json!({"kind": "char", "c": "y"}), &["ctrl"]);
    // A one-row step *up*, which is the only thing here that produces one: every `^D` before it
    // moved half a screen down. Read as the last two scrolls rather than against a number captured
    // beforehand, because the events already read are the ones from before the key was pressed.
    assert!(
        s.pump(|s| matches!(s.chat_scrolls().as_slice(), [.., Some(a), Some(b)] if b + 1 == *a)),
        "^Y drew from one row earlier: {:?}",
        s.chat_scrolls()
    );
    // And it was not the front half of a yank: the `y` after it is still waiting to be told what
    // to copy, which is exactly what it would not be if `^Y` had already opened one.
    s.ch("y");
    assert!(s.pump(|_| false) || s.copied().is_empty(), "nothing was copied: {:?}", s.copied());
}

#[test]
fn a_mode_gets_its_own_keys_and_nobody_elses() {
    // The rule, said plainly: while you are here, `^X` cutting a composer you cannot see and `^A`
    // selecting it are not things you asked for — they are the mode you left still holding the
    // keyboard. Only the escape hatches are bound everywhere.
    let sb = Sandbox::new("modekeys");
    let mut s = sb.start();
    s.answered_and_reading();
    s.to_top();
    let before = s.chat();

    // `^A`, `^X` and `^W` are the composer's, and `Chat`'s alone. `^Y` is *both* — the queue's
    // there and a line up here — which is the whole point of a mode: the same key, asked of a
    // different place, and only one answer arrives.
    for c in ["a", "x", "w"] {
        s.press(json!({"kind": "char", "c": c}), &["ctrl"]);
    }
    assert!(
        s.pump(|s| s.chat_cursor() == Some(0)),
        "none of them moved the cursor\n{:?}",
        s.chat_cursor()
    );
    assert!(
        s.buffer("[status]").iter().any(|l| l.contains("reading")),
        "and none of them took the keyboard back\n{:?}",
        s.buffer("[status]")
    );
    assert_eq!(s.chat(), before, "and none of them changed the transcript");
}

#[test]
fn reading_says_what_its_keys_do() {
    let sb = Sandbox::new("readhints");
    let mut s = sb.start();
    s.ready();
    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    assert!(
        s.pump(|s| s.composer_hints().iter().any(|h| h.contains("copy"))
            && s.composer_hints().iter().any(|h| h.contains("find"))),
        "{:?}",
        s.composer_hints()
    );

    // And once an operator is down, the row is about what can follow it. Anything else would be
    // answering a question nobody is asking any more.
    s.ch("y");
    assert!(
        s.pump(|s| s.composer_hints().iter().any(|h| h.contains("code block"))),
        "{:?}",
        s.composer_hints()
    );
}

/// `i` is how you stop reading and start writing, because the usual reason to stop reading is that
/// you have thought of what to say.
#[test]
fn i_gives_the_keyboard_back_to_the_composer() {
    let sb = Sandbox::new("readi");
    let mut s = sb.start();
    s.ready();
    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    assert!(s.pump(|s| s.buffer("[status]").iter().any(|l| l.contains("reading"))));

    s.ch("i");
    s.ch("j");
    assert!(
        s.pump(|s| s.composer() == vec!["j"]),
        "`j` is a motion while reading and a letter once you are back: {:?}",
        s.composer()
    );
}

// ---------------------------------------------------------------------------
// Selecting
// ---------------------------------------------------------------------------

/// A selection extended by a *jump* is drawn, not merely recorded.
///
/// Every way of getting somewhere in a transcript except `hjkl` is a jump: `^U` and `^D`, `[` and
/// `]`, `{` and `}`, a search hit. Only `WinMotion` repainted the highlight, so `v` and then a
/// screen upwards selected six lines and showed none of them — and `y` then copied text that had
/// never been on screen. Which is worse than no selection at all: it is one you cannot check.
#[test]
fn a_jump_that_extends_a_selection_paints_it() {
    let sb = Sandbox::new("jumppaint");
    let mut s = sb.start();
    s.answered_and_reading();

    s.ch("v");
    assert!(s.pump(|s| s.composer_hints().iter().any(|h| h.contains("extend"))), "selecting");
    s.press(json!({"kind": "char", "c": "u"}), &["ctrl"]);

    assert!(
        s.pump(|s| s.chat_selected().len() > 1),
        "a screen upwards is painted: {:?}",
        s.chat_selected()
    );
    // And what it copies is what it showed.
    let painted = s.chat_selected();
    s.ch("y");
    assert!(s.pump(|s| !s.copied().is_empty()), "and it copies\n{:?}", s.messages());
    assert_eq!(
        s.last_copy().lines().count(),
        painted.len(),
        "the copy is the highlight: {:?} vs {:?}",
        s.last_copy(),
        painted
    );
}

/// `V` takes whole lines in *both* directions.
///
/// Underneath there is one selection and it is two positions, so going up from `V` used to leave
/// the anchor at the start of the line you pressed it on and put the cursor above it — making that
/// line the far end of a backwards range that stopped at its own first column. The one line you
/// definitely meant was the one that dropped out.
#[test]
fn linewise_takes_whole_lines_going_upwards() {
    let sb = Sandbox::new("linewiseup");
    let mut s = sb.start();
    s.answered_and_reading();

    // Onto a line with words on it, so "the whole line" is a claim with something to check. Named
    // rather than read back afterwards: `pump` stops as soon as its predicate holds, so asking
    // where the cursor is straight after a key answers from the events already in hand — which is
    // where it was *before* it moved.
    let text = s.chat();
    let at = text
        .iter()
        .position(|l| l.contains("Three things"))
        .expect("the fixture says it") as u64;
    let here = text[at as usize].clone();
    s.ch("/");
    s.keys("Three things");
    s.special("enter");
    assert!(s.pump(|s| s.chat_cursor() == Some(at)), "on it: {:?}", s.chat_cursor());

    s.ch("V");
    assert!(
        s.pump(|s| s.chat_selected().contains(&(at as usize))),
        "the line it was pressed on: {at} vs {:?} / {:?}",
        s.chat_selected(),
        s.chat_cursor()
    );
    s.ch("k");
    s.ch("k");
    assert!(
        s.pump(|s| s.chat_selected().len() == 3),
        "and the two above it, whole: {:?}",
        s.chat_selected()
    );

    s.ch("y");
    assert!(s.pump(|s| !s.copied().is_empty()), "copied\n{:?}", s.messages());
    let got = s.last_copy();
    assert!(
        got.ends_with(here.trim_end()),
        "the line `V` was pressed on is still the end of it: {got:?} / {here:?}"
    );
    assert_eq!(got.lines().count(), 3, "three whole lines: {got:?}");
    assert_eq!(got.lines().next(), Some(text[at as usize - 2].as_str()), "from the top of one");
}

/// `Esc` gives up one thing per press, and a running selection is the first of them.
///
/// Changing your mind half way up a turn is the ordinary case — you meant a different piece of it —
/// and leaving the transcript outright there undoes the wrong amount of work.
#[test]
fn esc_drops_the_selection_before_it_leaves() {
    let sb = Sandbox::new("escdrop");
    let mut s = sb.start();
    s.answered_and_reading();

    s.ch("v");
    s.ch("k");
    s.ch("k");
    assert!(s.pump(|s| !s.chat_selected().is_empty()), "something is selected");

    s.special("esc");
    assert!(s.pump(|s| s.chat_selected().is_empty()), "the first press drops it");
    assert!(s.is_reading(), "and leaves you where you were: {:?}", s.buffer("[status]"));

    s.special("esc");
    assert!(s.pump(|s| !s.is_reading()), "the second one leaves: {:?}", s.buffer("[status]"));
}

/// `gg` extends whenever a selection is running, exactly as `k` does.
///
/// It was gated with the `y` operator — which does need to know, because `y` is two keys in one —
/// and so `v` then `gg` was a dead key, with no message to say why.
#[test]
fn gg_extends_a_running_selection_to_the_top() {
    let sb = Sandbox::new("ggextend");
    let mut s = sb.start();
    s.answered_and_reading();

    s.ch("v");
    s.ch("g");
    s.ch("g");
    assert!(s.pump(|s| s.chat_cursor() == Some(0)), "at the top: {:?}", s.chat_cursor());
    assert!(
        s.pump(|s| !s.chat_selected().is_empty()),
        "with everything on the way selected: {:?}",
        s.chat_selected()
    );

    s.ch("y");
    assert!(s.pump(|s| !s.copied().is_empty()), "and it copies\n{:?}", s.messages());
    assert!(
        s.last_copy().contains("What I found"),
        "from the first line of the transcript: {:?}",
        s.last_copy()
    );
}

/// `v` and then `y` copies the character the cursor is on, because that is what is selected.
///
/// The cursor in a mode where every key is a verb sits *on* a character rather than between two, so
/// a selection with both ends in the same place contains one. Answered exclusively — the text
/// field's convention — the two keys that mean "copy this" printed "nothing to copy" at somebody
/// who had just pressed them. See `SelectShape`.
#[test]
fn a_selection_includes_the_character_the_cursor_is_on() {
    let sb = Sandbox::new("emptyyank");
    let mut s = sb.start();
    s.answered_and_reading();

    // Somewhere with text on it, named rather than assumed: the transcript opens with a welcome
    // block whose rows are nobody's business but the renderer's.
    let (row, first) = s.first_row_with_text();
    s.go_to_row(row);

    s.ch("v");
    s.ch("y");
    assert!(s.pump(|s| !s.copied().is_empty()), "it copied something: {:?}", s.messages());
    assert_eq!(s.last_copy(), first, "the one character under the cursor");
    assert!(
        !s.messages().iter().any(|m| m.contains("nothing to copy")),
        "and did not claim there was nothing there: {:?}",
        s.messages()
    );
}

/// A selection over nothing at all still says so — which linewise on a blank row is.
#[test]
fn yanking_an_empty_selection_says_there_is_nothing_in_it() {
    let sb = Sandbox::new("emptyline");
    let mut s = sb.start();
    s.answered_and_reading();
    // A row with nothing on it — the markdown renderer separates every block with one.
    let row = s.first_blank_row();
    s.go_to_row(row);

    s.ch("V");
    s.ch("y");
    assert!(
        s.pump(|s| s.messages().iter().any(|m| m.contains("nothing to copy"))),
        "{:?}",
        s.messages()
    );
    assert!(s.copied().is_empty(), "and nothing went to the clipboard");
}

/// `^S` is the door, and a door opens both ways.
#[test]
fn the_key_that_opens_the_transcript_closes_it() {
    let sb = Sandbox::new("readtoggle");
    let mut s = sb.start();
    s.answered_and_reading();

    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    assert!(s.pump(|s| !s.is_reading()), "and back out: {:?}", s.buffer("[status]"));
}

// ---------------------------------------------------------------------------
// Leaving the conversation you were reading
// ---------------------------------------------------------------------------

/// Switching conversation says where the transcript window now *is*.
///
/// The scroll offset is kept rather than derived — that is what lets a client attaching later be
/// told it — so the host believing "back at the end" is not the same as the window being there.
/// Read a long answer, go up, come out, switch: the next conversation was drawn from a row of a
/// transcript that no longer existed, which on a short one means it was drawn blank.
#[test]
fn switching_conversation_puts_the_transcript_back_at_the_end() {
    let sb = Sandbox::new("switchscroll");
    let mut s = sb.start();
    s.answered_and_reading();
    s.chat_viewport(100, 6);
    s.to_top();
    assert!(s.pump(|s| s.chat_scrolls().last() == Some(&Some(0))), "at the top");

    // Somewhere that is not the end, and not the beginning either.
    s.ch("j");
    s.ch("j");
    let before = s.chat_scrolls().len();

    s.run_command("session.new", move |s| {
        s.chat_scrolls().len() > before && s.chat_scrolls().last() == Some(&None)
    });
}

/// Reading upwards ends at the top of the transcript, and not at the bottom of it.
///
/// Following the newest was encoded as row zero, so the first row of a conversation was the one
/// place in it nothing could ask for: `^U` up to the top asked for `0`, which was read as "follow
/// the tail", and the window drew the last screenful instead — cursor off screen, and nothing on
/// the way there to say what had happened. The two are separate states now, and only one of them
/// is a row.
#[test]
fn paging_up_to_the_top_stays_at_the_top() {
    let sb = Sandbox::new("readtotop");
    let mut s = sb.start();
    s.answered_and_reading();
    s.chat_viewport(100, 6);

    // Half a screen at a time, the way `^U` does it, until the cursor can go no further up.
    for _ in 0..40 {
        s.press(json!({"kind": "char", "c": "u"}), &["ctrl"]);
    }
    // Pumped on the scroll rather than on the cursor: the two are one batch and the cursor moves
    // first, so stopping at it answers from the events read *before* the window was told anything.
    assert!(
        s.pump(|s| s.chat_scrolls().last() == Some(&Some(0))),
        "the window was told to draw from the first row, not put back at the end: {:?}",
        s.chat_scrolls()
    );
    assert_eq!(s.chat_cursor(), Some(0), "and the cursor is on it");
}

/// And it takes the reader out with it.
///
/// Reading is a place *in* a conversation: a cursor at a row, a selection between two of them.
/// Every one of those addresses text that the switch is about to replace with somebody else's, and
/// a selection left running over it is a highlight in mid-air.
#[test]
fn switching_conversation_leaves_the_transcript_reader() {
    let sb = Sandbox::new("switchread");
    let mut s = sb.start();
    s.answered_and_reading();
    s.ch("v");
    s.ch("k");
    assert!(s.pump(|s| !s.chat_selected().is_empty()), "something is selected");

    s.run_command("session.new", |s| !s.is_reading());
    assert!(
        s.pump(|s| s.chat_selected().is_empty()),
        "and nothing is selected in the new one: {:?}",
        s.chat_selected()
    );
}

impl Session {
    /// Which rows of the transcript are painted as selected, by the marks the frontend was sent.
    ///
    /// Off the wire rather than off the anchor, because the anchor was never the bug: a selection
    /// whose two ends are right and whose highlight is three keystrokes stale looks exactly like a
    /// key that does not work, right up until `y` copies something you cannot see.
    fn chat_selected(&self) -> Vec<usize> {
        let Some(buf) = self.chat_buf() else { return Vec::new() };
        let mut rows: Vec<bool> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = (e["start"].as_u64().unwrap_or(0) as usize).min(rows.len());
            let old_end = e["old_end"].as_i64().unwrap_or(0);
            let end = if old_end < 0 { rows.len() } else { (old_end as usize).min(rows.len()) };
            let new: Vec<bool> = e["lines"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|l| {
                            l["marks"].as_array().is_some_and(|ms| {
                                ms.iter().any(|m| m["hl_group"] == "Visual")
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            rows.splice(start..end.max(start), new);
        }
        rows.iter().enumerate().filter(|(_, on)| **on).map(|(i, _)| i).collect()
    }

    fn chat_buf(&self) -> Option<u64> {
        let name = self.events.iter().find_map(|e| {
            let n = e["name"].as_str()?;
            (e["type"] == "buffer_opened" && n.starts_with("[chat]")).then(|| n.to_string())
        })?;
        self.buffer_named(&name)
    }

    fn chat_win(&self) -> Option<u64> {
        let buf = self.chat_buf()?;
        self.events.iter().find_map(|w| {
            (w["type"] == "window_opened" && w["buf"].as_u64() == Some(buf))
                .then(|| w["win"].as_u64())?
        })
    }

    /// Every row the transcript window has been told to start drawing at, in order.
    ///
    /// `None` is unscrolled — the frontend's own answer, which for a transcript is its last
    /// screenful. A row of `Some(0)` is the top and is a different place, which is the whole point:
    /// they shared an encoding, and the first row of a long answer was unreachable.
    fn chat_scrolls(&self) -> Vec<Option<u64>> {
        let Some(win) = self.chat_win() else { return Vec::new() };
        self.events
            .iter()
            .filter(|e| e["type"] == "scroll_to" && e["win"].as_u64() == Some(win))
            .map(|e| e["top_line"].as_u64())
            .collect()
    }

    /// Tell the core how tall the transcript really is.
    ///
    /// A stdio frontend has no geometry of its own, so nothing reports this unless a test does —
    /// and keeping the cursor on screen is the one thing the reader cannot do without it. Left
    /// unsaid, every motion here scrolls nothing at all and the assertions are about a window that
    /// was never laid out.
    fn chat_viewport(&mut self, width: u16, height: u16) {
        assert!(self.pump(|s| s.chat_win().is_some()), "the transcript window exists");
        let win = self.chat_win().expect("the transcript window");
        self.send(
            // `rows` is how many *buffer* rows are on the screen, which a real frontend counts
            // after wrapping. These fixtures are narrow enough not to wrap, so it is the height.
            &json!({"type": "viewport_changed", "win": win, "width": width, "height": height,
                    "top_line": 0, "rows": height}),
        );
    }

    /// Run a registered command by name — the way a menu entry, a palette row or another plugin
    /// does — and wait until it has actually happened.
    ///
    /// Sent again rather than once, because switching conversation is a *plugin* command: the
    /// sidebar owns the list, so it owns the verbs on it, and a plugin registers its commands some
    /// time after the frontend has a screen. Sending it before then is answered with "no such
    /// command" — a real answer, and not the one under test. There is nothing on the wire that says
    /// "the registry has this now", so the only honest signal is the effect itself.
    fn run_command(&mut self, name: &str, done: impl Fn(&Session) -> bool) {
        let deadline = Instant::now() + BUDGET;
        while Instant::now() < deadline {
            self.send(&json!({"type": "command", "name": name, "args": []}));
            let attempt = (Instant::now() + Duration::from_millis(500)).min(deadline);
            loop {
                if done(self) {
                    return;
                }
                let left = attempt.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match self.lines.recv_timeout(left) {
                    Ok(line) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&line) {
                            self.events.push(v);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        panic!("{name}: the workspace went away")
                    }
                }
            }
        }
        assert!(done(self), "{name} never took effect");
    }

    /// Whether the keyboard is in the transcript at all.
    ///
    /// Either word: the strip says `visual` while a selection is running, which is a *kind* of
    /// reading and not the absence of it. Matching only `reading` made "wait until the reader has
    /// closed" true the moment somebody pressed `v`.
    /// The first transcript row with something on it, and the first character of it.
    ///
    /// Named rather than assumed, because the transcript opens with a welcome block whose rows are
    /// the renderer's business and change whenever it does.
    fn first_row_with_text(&self) -> (u64, String) {
        let lines = self.chat();
        let (i, line) = lines
            .iter()
            .enumerate()
            .find(|(_, l)| l.chars().next().is_some_and(|c| c.is_alphanumeric()))
            .expect("the transcript has words in it");
        (i as u64, line.chars().next().expect("checked").to_string())
    }

    fn first_blank_row(&self) -> u64 {
        self.chat().iter().position(|l| l.is_empty()).expect("the transcript has a gap in it") as u64
    }

    /// `12G`, and wait until the cursor is actually on the twelfth row.
    fn go_to_row(&mut self, row: u64) {
        for d in (row + 1).to_string().chars() {
            self.ch(&d.to_string());
        }
        self.ch("G");
        assert!(
            self.pump(|s| s.chat_cursor() == Some(row)),
            "on row {row}: {:?}",
            self.chat_cursor()
        );
    }

    fn is_reading(&self) -> bool {
        self.buffer("[status]").iter().any(|l| l.contains("reading") || l.contains("visual"))
    }

    /// The virtual text under the composer — the shortcut row.
    fn composer_hints(&self) -> Vec<String> {
        let Some(buf) = self.buffer_named("[composer]") else { return Vec::new() };
        self.events
            .iter()
            .rev()
            .find(|e| e["type"] == "buffer_lines" && e["buf"].as_u64() == Some(buf))
            .and_then(|e| e["lines"].as_array())
            .map(|ls| {
                ls.iter()
                    .filter_map(|l| l["marks"].as_array())
                    .flatten()
                    .filter(|m| m["virt_text_pos"] == "below")
                    .filter_map(|m| m["virt_text"].as_array())
                    .map(|chunks| {
                        chunks
                            .iter()
                            .filter_map(|c| c["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Visual mode
// ---------------------------------------------------------------------------

/// A selection is a place you are, and you can see where in it you are.
///
/// Two things at once, and both were missing. The strip says `visual` rather than `reading`, because
/// a selection is the one state here where the same keys do different things — `y` copies rows
/// rather than a message, `esc` gives the selection up rather than the mode. And the caret is a
/// *block*: the cursor sits on a character now rather than between two, and a bar between two
/// characters is a lie about what the next key will act on.
#[test]
fn a_running_selection_says_so_and_the_caret_is_a_block() {
    let sb = Sandbox::new("visualsays");
    let mut s = sb.start();
    s.answered_and_reading();
    assert!(
        s.pump(|s| s.cursor_shape() == Some("block".into())),
        "reading asks for a block caret: {:?}",
        s.cursor_shape()
    );

    s.ch("v");
    assert!(
        s.pump(|s| s.buffer("[status]").iter().any(|l| l.contains("visual"))),
        "the strip names it: {:?}",
        s.buffer("[status]")
    );
    s.ch("V");
    assert!(
        s.pump(|s| s.buffer("[status]").iter().any(|l| l.contains("visual line"))),
        "and names which kind: {:?}",
        s.buffer("[status]")
    );

    s.special("esc");
    s.special("esc");
    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    assert!(
        s.pump(|s| s.cursor_shape() == Some("bar".into())),
        "and the composer gets its bar back: {:?}",
        s.cursor_shape()
    );
}

/// `V` takes whole rows in whichever direction it is extended, including upwards.
///
/// Not an invariant re-established after every motion — the shape belongs to the selection, so
/// there is no frame in which the wrong rows are lit and no motion that can forget to run it.
#[test]
fn linewise_takes_whole_rows_in_either_direction() {
    let sb = Sandbox::new("visualline");
    let mut s = sb.start();
    s.answered_and_reading();
    let row = s.first_row_with_text();
    s.go_to_row(row.0 + 2);

    s.ch("V");
    s.ch("k");
    s.ch("k");
    assert!(
        s.pump(|s| s.chat_selected() == vec![row.0 as usize, row.0 as usize + 1, row.0 as usize + 2]),
        "the row it started on and the two above it, whole: {:?}",
        s.chat_selected()
    );

    s.ch("y");
    assert!(s.pump(|s| !s.copied().is_empty()), "copied: {:?}", s.messages());
    let copied = s.last_copy();
    assert_eq!(copied.lines().count(), 3, "three whole lines: {copied:?}");
    assert!(copied.starts_with(&row.1), "starting at the first of them: {copied:?}");
}

/// `o` puts you on the other end of what you have, which is the difference between fixing a
/// selection and making it again.
#[test]
fn o_swaps_the_ends_of_a_selection() {
    let sb = Sandbox::new("visualswap");
    let mut s = sb.start();
    s.answered_and_reading();
    let (row, _) = s.first_row_with_text();
    s.go_to_row(row + 3);

    s.ch("V");
    s.ch("k");
    assert!(s.pump(|s| s.chat_cursor() == Some(row + 2)), "extended upwards");
    s.ch("o");
    assert!(
        s.pump(|s| s.chat_cursor() == Some(row + 3)),
        "and now on the end it started from: {:?}",
        s.chat_cursor()
    );
    // Both rows are still in it — this moves which end you are on, not what is selected.
    s.ch("j");
    assert!(
        s.pump(|s| s.chat_selected().len() == 3),
        "extending the other way adds a row: {:?}",
        s.chat_selected()
    );
}

/// `iw` inside a selection is the word, not the letter `i` doing what it does outside one.
#[test]
fn a_text_object_selects_what_it_names() {
    let sb = Sandbox::new("visualobj");
    let mut s = sb.start();
    s.answered_and_reading();
    // The fenced block's opening line, which has words and brackets on one row.
    s.search_for("fn main");
    // `^` is the first non-blank, which on a fenced row is what `0` is not: the renderer indents
    // code by two.
    s.ch("^");
    s.ch("w");

    s.ch("v");
    s.ch("i");
    s.ch("w");
    s.ch("y");
    assert!(s.pump(|s| !s.copied().is_empty()), "it copied: {:?}", s.messages());
    assert_eq!(s.last_copy(), "main", "the whole word under the cursor");
}

/// The bracket keys. `%` walks to the partner and back, which is how you find the end of a block
/// in an answer you did not write.
#[test]
fn percent_goes_to_the_matching_bracket() {
    let sb = Sandbox::new("visualpct");
    let mut s = sb.start();
    s.answered_and_reading();
    s.search_for("fn main");
    // The brace that opens the body, so the partner is two rows down rather than on this one.
    s.ch("$");
    let at = s.chat_cursor().expect("on the fence");

    s.ch("%");
    assert!(
        s.pump(|s| s.chat_cursor().is_some_and(|r| r > at)),
        "onto the brace that closes it: {at:?} -> {:?}",
        s.chat_cursor()
    );
    let closed = s.chat_cursor().expect("moved");
    s.ch("%");
    assert!(s.pump(|s| s.chat_cursor() == Some(at)), "and back: {closed} -> {:?}", s.chat_cursor());
}

/// `f` takes the next keystroke as a character rather than as a command, including a digit.
#[test]
fn f_finds_a_character_and_semicolon_finds_the_next_one() {
    let sb = Sandbox::new("visualfind");
    let mut s = sb.start();
    s.answered_and_reading();
    s.search_for("fn main");
    s.ch("0");
    let row = s.chat_cursor().expect("on the row");

    s.ch("f");
    s.ch("(");
    s.ch("v");
    s.ch("y");
    assert!(s.pump(|s| !s.copied().is_empty()), "copied: {:?}", s.messages());
    assert_eq!(s.last_copy(), "(", "`f(` lands on the bracket");
    assert_eq!(s.chat_cursor(), Some(row), "without leaving the row");
}

/// A count is spent by the motion after it, and `y` over a motion copies what the motion covers.
#[test]
fn yank_takes_a_motion_and_a_count() {
    let sb = Sandbox::new("visualyw");
    let mut s = sb.start();
    s.answered_and_reading();
    s.search_for("fn main");
    s.ch("^");

    // Two words from the first one. Exclusive, the way `w` is for an operator: `y2w` stops at the
    // start of the third word rather than taking its first letter with it.
    s.ch("y");
    s.ch("2");
    s.ch("w");
    assert!(s.pump(|s| !s.copied().is_empty()), "copied: {:?}", s.messages());
    let got = s.last_copy();
    assert_eq!(got, "fn main", "the two words, and not a character of the third: {got:?}");
}

/// `gv` puts back what `Esc` gave up. Without it, dropping a selection to look at something is
/// dropping one you have to make again.
#[test]
fn gv_brings_the_last_selection_back() {
    let sb = Sandbox::new("visualgv");
    let mut s = sb.start();
    s.answered_and_reading();
    let (row, _) = s.first_row_with_text();
    s.go_to_row(row + 2);

    s.ch("V");
    s.ch("k");
    assert!(s.pump(|s| s.chat_selected().len() == 2), "two rows: {:?}", s.chat_selected());
    s.special("esc");
    assert!(s.pump(|s| s.chat_selected().is_empty()), "given up: {:?}", s.chat_selected());

    s.ch("g");
    s.ch("v");
    assert!(
        s.pump(|s| s.chat_selected().len() == 2),
        "and back, the same two rows: {:?}",
        s.chat_selected()
    );
}

impl Session {
    /// What the frontend was last told the caret should look like.
    fn cursor_shape(&self) -> Option<String> {
        let win = self.chat_win()?;
        self.events
            .iter()
            .rev()
            .find(|e| e["type"] == "cursor_shape_changed" && e["win"].as_u64() == Some(win))
            .and_then(|e| e["shape"].as_str().map(str::to_string))
    }

    /// `/text<cr>`, and wait until the cursor is on a row that contains it.
    fn search_for(&mut self, text: &str) {
        self.ch("/");
        self.keys(text);
        self.special("enter");
        assert!(
            self.pump(|s| {
                let Some(row) = s.chat_cursor() else { return false };
                s.chat().get(row as usize).is_some_and(|l| l.contains(text))
            }),
            "searched to a row containing {text:?}: {:?}",
            self.chat_cursor()
        );
    }
}


// ---------------------------------------------------------------------------
// Watching what a turn did
// ---------------------------------------------------------------------------

/// `c` steps to the next tool call, and the card you land on opens itself.
///
/// The transcript's third structure, after the turn and the block: `[`/`]` is a whole turn and
/// `{`/`}` is one block for a whole run of cards, so before this there was no way to walk what a
/// turn *did* one call at a time. And a card the cursor is in shows what came back — the list
/// rule, one surface along — so `c c c` reads the work rather than the headers over it.
#[test]
fn c_steps_call_to_call_and_the_card_you_are_on_opens_itself() {
    let sb = Sandbox::new("cards-step");
    let mut s = sb.with_cards();
    s.ready();
    s.keys("go");
    s.special("enter");
    assert!(
        s.pump(|s| s.chat().iter().any(|l| l.contains("All done."))),
        "the whole turn landed\n{:?}",
        s.chat()
    );
    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    assert!(s.pump(|s| s.buffer("[status]").iter().any(|l| l.contains("reading"))));

    // Folded, a read is its header and the count on it. Nothing of either file is on screen.
    assert_eq!((s.showing("one.txt"), s.showing("two.txt")), (0, 0), "{:?}", s.chat());

    // `C` goes back to the last card, which is the second one.
    s.ch("C");
    assert!(
        s.pump(|s| s.showing("two.txt") > 0),
        "the card the cursor arrived in shows what came back\n{:?}",
        s.chat()
    );
    assert_eq!(s.showing("one.txt"), 0, "and only that one — the cursor is what asks");

    // And back one more, which folds the one it left.
    s.ch("C");
    assert!(
        s.pump(|s| s.showing("one.txt") > 0 && s.showing("two.txt") == 0),
        "stepping off gives the rows back\n{:?}",
        s.chat()
    );
}

/// A preview is bounded, and `⇥` is how you say you want the rest of it — and want to keep it.
///
/// The two halves are one decision: before this, `⇥` was the only way to see a body at all, so
/// "show me" and "keep it" could not be told apart. Now the cursor says the first and the key says
/// the second.
#[test]
fn tab_keeps_a_card_open_after_the_cursor_has_gone() {
    let sb = Sandbox::new("cards-pin");
    let mut s = sb.with_cards();
    s.ready();
    s.keys("go");
    s.special("enter");
    assert!(s.pump(|s| s.chat().iter().any(|l| l.contains("All done."))));
    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    assert!(s.pump(|s| s.buffer("[status]").iter().any(|l| l.contains("reading"))));

    s.ch("C");
    assert!(s.pump(|s| s.showing("two.txt") > 0), "{:?}", s.chat());
    let previewed = s.showing("two.txt");
    assert!(previewed < 40, "a preview is bounded: {previewed} of 40 rows");

    s.special("tab");
    assert!(
        s.pump(|s| s.showing("two.txt") == 40),
        "and Tab is all of it: {}\n{:?}",
        s.showing("two.txt"),
        s.chat()
    );

    // Off it, and it stays. That is what pinning is for.
    s.ch("C");
    assert!(
        s.pump(|s| s.showing("one.txt") > 0),
        "the first card opened on arrival\n{:?}",
        s.chat()
    );
    assert_eq!(s.showing("two.txt"), 40, "and the pinned one is still open behind us");
}

/// Leaving reading folds what the cursor was previewing, and keeps what you pinned.
///
/// A preview is the cursor's, and there is no cursor in the composer.
#[test]
fn leaving_reading_folds_the_preview_and_keeps_the_pin() {
    let sb = Sandbox::new("cards-leave");
    let mut s = sb.with_cards();
    s.ready();
    s.keys("go");
    s.special("enter");
    assert!(s.pump(|s| s.chat().iter().any(|l| l.contains("All done."))));
    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    assert!(s.pump(|s| s.buffer("[status]").iter().any(|l| l.contains("reading"))));

    s.ch("C");
    assert!(s.pump(|s| s.showing("two.txt") > 0));
    s.special("esc");
    assert!(
        s.pump(|s| s.showing("two.txt") == 0),
        "the rows go back when the cursor does\n{:?}",
        s.chat()
    );
}


/// A turn writes below where you are reading, and reading the last row means reading what arrives.
///
/// Reading is a place *in* the transcript — the scroll offset is pinned by every motion — so
/// before this an answer produced while `^S` was on landed off the bottom of the window with
/// nothing saying so, and watching a turn meant leaving reading and coming back for every call.
#[test]
fn reading_the_last_row_keeps_reading_it() {
    let sb = Sandbox::new("cards-tail");
    let mut s = sb.with_slow_cards();
    s.ready();
    s.keys("go");
    s.special("enter");
    // In while the turn is still going: `^S` starts at the newest row, which is the end.
    assert!(
        s.pump(|s| s.chat().iter().any(|l| l.contains("Looking at the first one"))),
        "the turn started\n{:?}",
        s.chat()
    );
    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    assert!(s.pump(|s| s.buffer("[status]").iter().any(|l| l.contains("reading"))));

    assert!(
        s.pump(|s| s.chat().iter().any(|l| l.contains("All done."))),
        "the rest of it arrived\n{:?}",
        s.chat()
    );
    let last = s.chat().len().saturating_sub(1) as u64;
    assert!(
        s.pump(|s| s.chat_cursor() == Some(s.chat().len().saturating_sub(1) as u64)),
        "the reader was carried to the end: at {:?} of {last}",
        s.chat_cursor()
    );
    assert!(s.buffer("[status]").iter().any(|l| l.contains("reading")), "and is still reading");
}

/// And reading anywhere else does not move, however much arrives below it.
///
/// The other half, and the one that makes the first half safe: half way up a diff you are reading
/// is exactly where you must still be when the next tool call lands.
#[test]
fn reading_anywhere_else_stays_exactly_where_it_is() {
    let sb = Sandbox::new("cards-anchor");
    let mut s = sb.with_slow_cards();
    s.ready();
    s.keys("go");
    s.special("enter");
    assert!(s.pump(|s| s.chat().iter().any(|l| l.contains("Looking at the first one"))));
    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    assert!(s.pump(|s| s.buffer("[status]").iter().any(|l| l.contains("reading"))));
    s.to_top();

    assert!(
        s.pump(|s| s.chat().iter().any(|l| l.contains("All done."))),
        "the rest of it arrived\n{:?}",
        s.chat()
    );
    assert_eq!(s.chat_cursor(), Some(0), "and it left the reader where the reader was");
}
