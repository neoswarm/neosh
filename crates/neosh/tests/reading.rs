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
}

// ---------------------------------------------------------------------------
// Getting a piece of the answer out
// ---------------------------------------------------------------------------

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

    // And `^Y`, whose letter is the yank prefix, is a line up rather than a pending operator.
    s.press(json!({"kind": "char", "c": "d"}), &["ctrl"]);
    assert!(s.pump(|s| s.chat_cursor().is_some_and(|r| r > 0)), "somewhere with room above");
    let at = s.chat_cursor().unwrap_or(0);
    s.press(json!({"kind": "char", "c": "y"}), &["ctrl"]);
    assert!(
        s.pump(|s| s.chat_cursor() == Some(at - 1)),
        "^Y moved one line up\n{at} -> {:?}",
        s.chat_cursor()
    );
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

impl Session {
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
