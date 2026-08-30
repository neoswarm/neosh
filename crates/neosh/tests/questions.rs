//! Questions, end to end.
//!
//! An agent asking *you* something — `claude`'s `AskUserQuestion` — is not a permission request,
//! and for a while neosh treated it as one. The configured default is full access, so the policy
//! layer answered before anybody was asked, the driver sent back a bare yes carrying no answers,
//! and the CLI told its own model *"The user did not answer the questions."* A turn asked a
//! question, never drew it, and then reported that you had ignored it.
//!
//! These tests drive the real panel, through the same door a driver's question comes through:
//! `neosh.ask` raises the `ask_user` hook, the bundled `questions` plugin draws it, and the answers
//! come back as the values the agent would be sent. Whether they then reach `claude` in the right
//! shape is [`neosh_provider::drivers::claude_control`]'s own unit tests, which are written against
//! a captured request from a real install.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

const BUDGET: Duration = Duration::from_secs(30);

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
        let root = std::env::temp_dir().join(format!("neosh-ask-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["config", "state", "work"] {
            std::fs::create_dir_all(root.join(d)).expect("dirs");
        }
        Self { root }
    }

    fn write_config(&self, contents: &str) {
        std::fs::write(self.root.join("config/config.toml"), contents).expect("config");
    }

    /// A plugin that asks, so a test can raise the same hook a driver raises.
    fn install_asker(&self, source: &str) {
        let dir = self.root.join("config/plugins/asker");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("plugin.toml"),
            "name = \"asker\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
        )
        .expect("manifest");
        std::fs::write(dir.join("main.ts"), source).expect("plugin");
    }

    fn start(&self) -> Session {
        let child = Command::new(env!("CARGO_BIN_EXE_neosh"))
            .args(["--ui-protocol", "stdio"])
            .arg("--config-dir")
            .arg(self.root.join("config"))
            .arg("--cwd")
            .arg(self.root.join("work"))
            .args(["--mock-script", &fixture().display().to_string()])
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

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello_turn.jsonl")
}

/// Two questions: one that takes a single answer and one that takes several.
const ASKER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("t.ask", async () => {
    const answers = await neosh.ask([
      {
        question: "Which database should this use?",
        header: "Database",
        multi_select: false,
        options: [
          { label: "Postgres", description: "relational, what you already run" },
          { label: "SQLite", description: "one file, no server" },
          { label: "DynamoDB", description: "managed, pay per request" },
        ],
      },
      {
        question: "Which of these should be enabled?",
        header: "Features",
        multi_select: true,
        options: [
          { label: "Logging", description: "structured, to stderr" },
          { label: "Metrics", description: "a prometheus endpoint" },
          { label: "Tracing", description: "otel spans" },
        ],
      },
    ]);
    neosh.notify(`answered: ${answers === null ? "nothing" : JSON.stringify(answers)}`);
  });
  await neosh.cmd.register("t.ask.one", async () => {
    const answers = await neosh.ask([
      {
        question: "Tabs or spaces?",
        header: "Indentation",
        multi_select: false,
        options: [
          { label: "Tabs", description: "tab characters" },
          { label: "Spaces", description: "space characters" },
        ],
      },
    ]);
    neosh.notify(`answered: ${answers === null ? "nothing" : JSON.stringify(answers)}`);
  });
  // One question whose descriptions are longer than any column can hold, for the panel's rule that
  // the row under the cursor says all of itself.
  await neosh.cmd.register("t.ask.long", async () => {
    const answers = await neosh.ask([
      {
        question: "Which database should this use?",
        header: "Database",
        multi_select: false,
        options: [
          {
            label: "Postgres",
            description: "the primary write database, the one everything else already points at and the one a fixture suite must never be aimed anywhere-near",
          },
          {
            label: "SQLite",
            description: "one file and no server, which stops being a simplification the moment a second process wants to write to it concurrently-somehow",
          },
        ],
      },
    ]);
    neosh.notify(`answered: ${answers === null ? "nothing" : JSON.stringify(answers)}`);
  });
  // Toggles: to the other conversation if there is one, and to a new one if there is not. Enough
  // for a test to move the screen off a question and back onto it.
  await neosh.cmd.register("t.switch", async () => {
    const list = await neosh.session.list();
    const other = list.find((x) => !x.is_active);
    if (other) await neosh.session.switch(other.id);
    else await neosh.session.create({ activate: true });
  });
  neosh.notify("asker ready");
}
"#;

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
        s.send(r#"{"type":"ready","width":100,"height":30}"#);
        s
    }

    fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{line}").expect("write");
        stdin.flush().expect("flush");
    }

    fn key(&mut self, c: &str) {
        self.send(&format!(
            r#"{{"type":"key","key":{{"code":{{"kind":"char","c":"{c}"}},"mods":{{}}}}}}"#
        ));
    }

    fn special(&mut self, kind: &str) {
        self.send(&format!(r#"{{"type":"key","key":{{"code":{{"kind":"{kind}"}},"mods":{{}}}}}}"#));
    }

    fn command(&mut self, name: &str) {
        self.send(&format!(r#"{{"type":"command","name":"{name}"}}"#));
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

    fn wait_for(&mut self, needle: &str) {
        let want = needle.to_string();
        if !self.pump(move |s| s.saw(&want)) {
            panic!("timed out waiting for {needle:?}\n{}", self.transcript());
        }
    }

    fn drain_for(&mut self, how_long: Duration) {
        let deadline = Instant::now() + how_long;
        while let Ok(line) =
            self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                self.events.push(v);
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }

    fn texts(&self) -> Vec<String> {
        self.events
            .iter()
            .flat_map(|e| match e["type"].as_str() {
                Some("buffer_lines") => e["lines"]
                    .as_array()
                    .map(|a| {
                        a.iter().filter_map(|l| l["text"].as_str().map(str::to_string)).collect()
                    })
                    .unwrap_or_default(),
                Some("message") => {
                    e["text"].as_str().map(|t| vec![t.to_string()]).unwrap_or_default()
                }
                _ => Vec::new(),
            })
            .collect()
    }

    /// Every highlight group put on any row, so a test can assert on colour rather than on text.
    fn groups(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| e["type"] == "buffer_lines")
            .filter_map(|e| e["lines"].as_array())
            .flatten()
            .filter_map(|l| l["marks"].as_array())
            .flatten()
            .filter_map(|m| {
                m["hl_group"].as_str().or_else(|| m["line_hl_group"].as_str()).map(str::to_string)
            })
            .collect()
    }

    /// The rows drawn into the question panel, and nothing drawn anywhere else.
    ///
    /// The composer holds the same sentence — the panel mirrors what you type into it, because the
    /// composer is the field this screen actually has — so a test about what *the panel* shows has
    /// to say which buffer it means, or it passes on the composer's copy of the text and never
    /// looks at the row it was written about.
    fn panel(&self) -> Vec<String> {
        let bufs: Vec<Value> = self
            .events
            .iter()
            .filter(|e| e["type"] == "buffer_opened" && e["name"].as_str() == Some("[question]"))
            .map(|e| e["buf"].clone())
            .collect();
        self.events
            .iter()
            .filter(|e| e["type"] == "buffer_lines" && bufs.contains(&e["buf"]))
            .filter_map(|e| e["lines"].as_array())
            .flatten()
            .filter_map(|l| l["text"].as_str().map(str::to_string))
            .collect()
    }

    fn saw(&self, needle: &str) -> bool {
        self.texts().iter().any(|l| l.contains(needle))
    }

    /// How many rows containing `needle` have been drawn, ever.
    ///
    /// What "it was drawn again" is asserted with. [`Self::saw`] scans everything the session has
    /// ever sent, so it cannot tell a panel that came back from one that is merely remembered.
    fn count(&self, needle: &str) -> usize {
        self.texts().iter().filter(|l| l.contains(needle)).count()
    }

    /// The most recent version of the row containing `needle`.
    ///
    /// The panel is redrawn whole on every keystroke, so a row appears many times and only the last
    /// one is what is on screen. Anything asserting about what a row looks like *now* has to say so
    /// — scanning every row ever sent finds the version from three keystrokes ago and passes or
    /// fails on that.
    fn latest(&self, needle: &str) -> Option<String> {
        self.texts().into_iter().filter(|l| l.contains(needle)).next_back()
    }

    fn transcript(&self) -> String {
        let mut s = String::from("--- text seen ---\n");
        for l in self.texts() {
            s.push_str(&format!("  {l}\n"));
        }
        s
    }
}

/// The panel, with the first question on it.
fn open(name: &str) -> Session {
    let sb = Sandbox::new(name);
    sb.install_asker(ASKER);
    let mut s = sb.start();
    s.wait_for("asker ready");
    s.command("t.ask");
    s.wait_for("Which database should this use?");
    // The sandbox has to outlive the session, and both are dropped at the end of the test.
    std::mem::forget(sb);
    s
}

#[test]
fn a_question_is_drawn_with_its_options_and_what_each_one_means() {
    let s = open("draws");
    assert!(s.saw("Database"), "the header names what it is about\n{}", s.transcript());
    assert!(s.saw("Postgres"), "{}", s.transcript());
    assert!(
        s.saw("relational, what you already run"),
        "the description is what decides it\n{}",
        s.transcript()
    );
    // Two questions, so the panel says which one this is.
    assert!(s.saw("1/2"), "{}", s.transcript());
}

#[test]
fn choosing_one_answers_it_and_moves_on_to_the_next() {
    let mut s = open("advance");
    s.special("enter"); // the cursor starts on the first option
    s.wait_for("Which of these should be enabled?");
    assert!(s.saw("2/2"), "{}", s.transcript());
}

#[test]
fn a_digit_takes_the_option_it_is_beside() {
    let mut s = open("digit");
    s.key("2"); // SQLite
    s.wait_for("Which of these should be enabled?");
    // Multi-select: tick two, then confirm.
    s.key("1");
    s.key("3");
    s.special("enter");
    s.wait_for(r#""answers":["SQLite"]"#);
    assert!(
        s.saw(r#""answers":["Logging","Tracing"]"#),
        "a question that takes several gives back several\n{}",
        s.transcript()
    );
}

#[test]
fn a_multi_select_question_does_not_answer_itself_on_the_first_tick() {
    // The bug a single-select's auto-advance invites: ticking one box and being carried off the
    // question before ticking the second.
    let mut s = open("multi");
    s.key("1");
    s.wait_for("Which of these should be enabled?");
    s.key("1"); // Logging
    s.drain_for(Duration::from_millis(600));
    assert!(
        !s.saw("answered:"),
        "ticking a box is not answering the question\n{}",
        s.transcript()
    );
    s.special("enter");
    s.wait_for(r#""answers":["Logging"]"#);
}

#[test]
fn typing_is_the_answer_nobody_listed() {
    // The option people reach for most, and the one a list of buttons cannot hold.
    let mut s = open("typed");
    for c in ["n", "e", "i", "t", "h", "e", "r"] {
        s.key(c);
    }
    s.wait_for("neither");
    s.special("enter");
    s.wait_for("Which of these should be enabled?");
    s.key("2");
    s.special("enter");
    s.wait_for(r#""answers":["neither"]"#);
    assert!(s.saw(r#""custom":true"#), "and it is marked as typed\n{}", s.transcript());
}

#[test]
fn the_shortcuts_go_away_the_moment_a_digit_becomes_a_character() {
    // The panel cannot have it both ways, so it says which way it is: while nothing is typed the
    // rows wear their numbers, and once something is they do not.
    let mut s = open("badges");
    assert!(
        s.latest("Postgres").is_some_and(|l| l.trim_end().ends_with('1')),
        "with nothing typed the rows wear their numbers\n{:?}",
        s.latest("Postgres"),
    );
    s.key("x");
    s.wait_for("✎ x");
    s.drain_for(Duration::from_millis(300));
    assert!(
        s.latest("Postgres").is_some_and(|l| !l.trim_end().ends_with('1')),
        "a badge still saying 1 is a promise the keyboard has stopped keeping\n{:?}",
        s.latest("Postgres"),
    );
    // And a digit now types itself rather than choosing.
    s.key("2");
    s.wait_for("✎ x2");
}

#[test]
fn dismissing_answers_nothing_rather_than_answering_the_first_row() {
    let mut s = open("escape");
    s.special("esc");
    s.wait_for("answered: nothing");
}

#[test]
fn you_can_go_back_and_change_an_answer() {
    let mut s = open("back");
    s.key("1"); // Postgres
    s.wait_for("2/2");
    // `<S-Tab>` is one key press with no modifier on it — a terminal delivers it as its own
    // code, which is what the keymap parser binds against.
    s.special("back_tab");
    s.wait_for("1/2");
    s.key("2"); // SQLite instead
    s.wait_for("2/2");
    s.key("2");
    s.special("enter");
    s.wait_for(r#""answers":["SQLite"]"#);
}

#[test]
fn the_row_under_the_cursor_is_banded_rather_than_repainted() {
    // A ranged group across a selected row replaces every colour on it, so the row that is
    // highlighted becomes the only one that has stopped saying what its parts are.
    let mut s = open("band");
    let groups = s.groups();
    assert!(groups.iter().any(|g| g == "Question.Cursor"), "{groups:?}");
    assert!(groups.iter().any(|g| g == "Question.Detail"), "{groups:?}");
    assert!(groups.iter().any(|g| g == "Question.Header"), "{groups:?}");
    s.drain_for(Duration::from_millis(1));
}

#[test]
fn with_the_panel_off_a_question_is_answered_by_nobody_rather_than_hanging() {
    // Fail closed, but not fail silent: the agent is told, in a sentence it can act on, and the
    // turn does not sit there until a timeout it has no way to know about.
    let sb = Sandbox::new("nopanel");
    sb.write_config("[options]\n\"plugins.disabled\" = [\"questions\"]\n");
    sb.install_asker(ASKER);
    let mut s = sb.start();
    s.wait_for("asker ready");
    s.command("t.ask.one");
    s.wait_for("answered: nothing");
    assert!(!s.saw("Tabs or spaces?"), "nothing drew it\n{}", s.transcript());
}

#[test]
fn a_question_for_a_conversation_you_are_not_looking_at_waits_rather_than_taking_the_screen() {
    // Several turns run at once and only one of them is on screen. A panel that opened wherever a
    // question happened to come from would put another conversation's question over the one you are
    // reading — and hand it the keys you were typing at something else.
    let sb = Sandbox::new("elsewhere");
    sb.install_asker(ASKER);
    let mut s = sb.start();
    s.wait_for("asker ready");

    s.command("t.ask.one");
    s.wait_for("Tabs or spaces?");

    s.command("t.switch");
    s.wait_for("conversation is asking");
    s.drain_for(Duration::from_millis(500));
    // The keyboard is the conversation you are looking at's again. This is the harm the queue
    // exists to prevent: `2` here is a message somebody is typing, not an answer to a question
    // asked somewhere else.
    let quiet = s.count("Tabs or spaces?");
    s.key("2");
    s.drain_for(Duration::from_millis(500));
    assert!(!s.saw("answered:"), "nothing was answered on your behalf\n{}", s.transcript());
    assert_eq!(
        s.count("Tabs or spaces?"),
        quiet,
        "and the panel is not being redrawn over the conversation you switched to\n{}",
        s.transcript()
    );

    // And it is there again when you go back, still unanswered.
    s.command("t.switch");
    s.pump(move |x| x.count("Tabs or spaces?") > quiet + 1);
    assert!(
        s.count("Tabs or spaces?") > quiet + 1,
        "switching back puts the question in front of you again\n{}",
        s.transcript()
    );
    s.key("2");
    s.wait_for(r#""answers":["Spaces"]"#);
}

#[test]
fn dismissing_leaves_what_you_had_typed_where_you_typed_it() {
    // A question lands over a sentence somebody was half-way through, they decide not to answer it,
    // and the panel goes. Wiping the composer on the way out would eat a message for the crime of
    // being interrupted.
    let mut s = open("keeps");
    for c in ["h", "a", "l", "f"] {
        s.key(c);
    }
    s.wait_for("✎ half");
    s.special("esc");
    s.wait_for("answered: nothing");
    s.drain_for(Duration::from_millis(400));
    assert!(
        s.latest("half").is_some_and(|l| l.contains("half")),
        "what was typed is still in the composer\n{}",
        s.transcript()
    );
}

/// The panel, with one question on it, raised by `command`.
fn open_with(name: &str, command: &str, question: &str) -> Session {
    let sb = Sandbox::new(name);
    sb.install_asker(ASKER);
    let mut s = sb.start();
    s.wait_for("asker ready");
    s.command(command);
    s.wait_for(question);
    std::mem::forget(sb);
    s
}

/// The row under the cursor says all of itself, and the rows either side of it do not.
///
/// A description is the *only* thing that says what taking an option would mean, and it is the
/// first thing a two-column row inside a bordered float runs out of room for. Clipped, four options
/// are four sentences that stop at the same word and the answer is a guess — which is the failure
/// this panel exists to prevent, arrived at from the other direction.
#[test]
fn the_option_under_the_cursor_says_all_of_itself() {
    let mut s = open_with("expand", "t.ask.long", "Which database should this use?");

    // The cursor starts on the first option, so the end of its description is on screen — which it
    // can only be by having been continued onto another line.
    assert!(
        s.pump(|s| s.saw("anywhere-near")),
        "the whole description of the option under the cursor is drawn\n{}",
        s.transcript()
    );
    // And the one below it is still one row. Asserted before moving, because `saw` scans everything
    // the session has ever drawn and cannot tell a row that folded back from one never unfolded.
    assert!(
        !s.saw("concurrently-somehow"),
        "an option the cursor is not on stays a row\n{}",
        s.transcript()
    );

    // Moving swaps which of them is spelled out.
    s.special("down");
    s.wait_for("concurrently-somehow");
}

/// There is a row for the answer nobody listed, and it is a row rather than a rumour.
///
/// Typing has always been how you answer a question none of the options answer, and for as long as
/// the only thing that said so was one phrase in a dim strip at the foot, it was a capability
/// nobody had: nothing on the panel looked like a field, so four options read as four options and a
/// dead end.
#[test]
fn the_row_at_the_foot_takes_an_answer_nobody_listed() {
    let mut s = open_with("other", "t.ask.one", "Tabs or spaces?");
    assert!(s.saw("Other"), "the row is on the panel\n{}", s.transcript());
    assert!(
        s.saw("type an answer of your own"),
        "and it says what it is for\n{}",
        s.transcript()
    );

    // `0` is the digit the options deliberately do not use: on a numbered list it already reads as
    // *none of them*, which is what this row is.
    s.key("0");
    s.wait_for("send this answer");

    for c in "two spaces".chars() {
        s.key(&c.to_string());
    }
    s.special("enter");
    s.wait_for(r#""answers":["two spaces"]"#);
    assert!(
        s.saw(r#""custom":true"#),
        "an answer of your own is marked as one\n{}",
        s.transcript()
    );
}

/// Backspacing off the front of an empty field puts you back among the options.
///
/// The one key people already reach for, and the reason the field can be entered without it being a
/// trap: a panel you can get into and not out of is worse than one with no field at all.
#[test]
fn backspacing_off_an_empty_answer_goes_back_to_the_options() {
    let mut s = open_with("unwrite", "t.ask.one", "Tabs or spaces?");
    s.key("0");
    s.wait_for("send this answer");
    s.special("backspace");
    // The shortcuts come back, which is the panel saying a digit is a digit again.
    s.wait_for("1-2 pick");
    s.key("1");
    s.wait_for(r#""answers":["Tabs"]"#);
}

/// An answer longer than the row is wide is still an answer you can read back.
///
/// The row that takes an answer of your own was drawn with the same `clip` as an option's label:
/// past the width of the panel it became the first sixty characters and an ellipsis, and stayed
/// that way however much more was typed. The caret stopped moving, every word after it went
/// somewhere invisible, and the only way to find out what you had written was to send it — on the
/// one row of this panel whose entire job is to show you what you are writing.
#[test]
fn an_answer_longer_than_the_row_is_shown_whole() {
    let mut s = open_with("longanswer", "t.ask.one", "Tabs or spaces?");
    let answer = "spaces everywhere except in the makefiles, where a tab is the syntax and a space \
                  is a build that fails, and this is the zzz-end-of-it";
    for c in answer.chars() {
        s.key(&c.to_string());
    }
    assert!(
        s.pump(|x| x.panel().iter().any(|l| l.contains("zzz-end-of-it"))),
        "the end of what is being typed is on the panel\n{}",
        s.transcript()
    );
    // And nothing of it was thrown away to make it fit: the field wraps, it does not clip.
    assert!(
        !s.panel().iter().any(|l| l.contains("makefiles") && l.contains('\u{2026}')),
        "the field wraps rather than ending in an ellipsis\n{}",
        s.transcript()
    );
    s.special("enter");
    s.wait_for(&format!("\"answers\":[\"{answer}\"]"));
}

/// What you were half-way through writing survives looking at another conversation.
///
/// Which question you are on and which row the cursor is on already did — they live on the ask,
/// which outlives the panel — but what was in the field lived in the panel, and the panel is closed
/// and built again from nothing every time the screen moves to another conversation. Go and check
/// something in the conversation next door and the sentence you were writing was gone, with no key
/// having been pressed that could have deleted it.
#[test]
fn what_you_were_writing_survives_looking_at_another_conversation() {
    let sb = Sandbox::new("keeps-writing");
    sb.install_asker(ASKER);
    let mut s = sb.start();
    s.wait_for("asker ready");
    s.command("t.ask.one");
    s.wait_for("Tabs or spaces?");
    for c in "half a thought".chars() {
        s.key(&c.to_string());
    }
    s.wait_for("\u{270e} half a thought");

    s.command("t.switch");
    s.wait_for("conversation is asking");
    s.drain_for(Duration::from_millis(400));

    // Counted with the panel gone, so what is waited for below is the panel *coming back* rather
    // than the last of the redraws it made on its way out.
    let written = s.count("\u{270e} half a thought");
    s.command("t.switch");
    assert!(
        s.pump(move |x| x.count("\u{270e} half a thought") > written),
        "coming back puts you back in the sentence\n{}",
        s.transcript()
    );
    // And it is still the answer, not merely still on screen.
    s.special("enter");
    s.wait_for("\"answers\":[\"half a thought\"]");
    assert!(s.saw(r#""custom":true"#), "and it is marked as typed\n{}", s.transcript());
}
