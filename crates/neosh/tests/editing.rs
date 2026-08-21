//! The composer is a text field, not a string things get appended to.
//!
//! Before this existed, typing put a character at the end of the buffer and backspace took one off
//! it. There was no cursor, so there was nothing to move, nothing to select, and nothing to copy —
//! which is a strange thing for a program whose whole output is text you want to keep.
//!
//! Driven through the stdio protocol, so these assert on what a *frontend* is told rather than on
//! internal state: the clipboard write is a `UiEvent`, and if it stops being emitted these fail.

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
        let root = std::env::temp_dir().join(format!("neosh-edit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["config", "state", "work"] {
            std::fs::create_dir_all(root.join(d)).expect("dirs");
        }
        Self { root }
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

    /// One key. `mods` is any subset of `ctrl`, `alt`, `shift`.
    fn press(&mut self, code: Value, mods: &[&str]) {
        let m: serde_json::Map<String, Value> =
            mods.iter().map(|k| ((*k).to_string(), json!(true))).collect();
        self.send(&json!({"type": "key", "key": {"code": code, "mods": m}}));
    }

    fn ch(&mut self, c: &str) {
        self.press(json!({"kind": "char", "c": c}), &[]);
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.ch(&c.to_string());
        }
    }

    fn special(&mut self, kind: &str, mods: &[&str]) {
        self.press(json!({"kind": kind}), mods);
    }

    fn command(&mut self, name: &str) {
        self.send(&json!({"type": "command", "name": name, "args": []}));
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

    /// A buffer's contents, folded the way a frontend folds splices.
    fn buffer(&self, name: &str) -> Vec<String> {
        let Some(buf) = self.buffer_named(name) else { return Vec::new() };
        let mut lines: Vec<String> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = e["start"].as_i64().unwrap_or(0);
            let old_end = e["old_end"].as_i64().unwrap_or(start);
            let new: Vec<String> = e["lines"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l["text"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let s = start.clamp(0, lines.len() as i64) as usize;
            let en =
                if old_end < 0 { lines.len() } else { (old_end as usize).clamp(s, lines.len()) };
            lines.splice(s..en, new);
        }
        lines
    }

    fn composer(&self) -> Vec<String> {
        self.buffer("[composer]")
    }

    /// The transcript. Named `[chat] <title>`, so matched by prefix — the title changes as soon as
    /// the conversation gets one.
    fn chat(&self) -> Vec<String> {
        let name = self.events.iter().find_map(|e| {
            let n = e["name"].as_str()?;
            (e["type"] == "buffer_opened" && n.starts_with("[chat]")).then(|| n.to_string())
        });
        name.map(|n| self.buffer(&n)).unwrap_or_default()
    }

    /// Everything the core asked the frontend to put on the clipboard.
    fn copied(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| e["type"] == "clipboard")
            .filter_map(|e| e["text"].as_str().map(str::to_string))
            .collect()
    }

    fn wait_composer(&mut self, want: &[&str]) {
        if !self.pump(|s| s.composer() == want) {
            panic!("composer never became {want:?}\n  it is {:?}", self.composer());
        }
    }

    /// Transient messages, which is how a test plugin reports that it ran.
    fn messages(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| e["type"] == "message")
            .filter_map(|e| e["text"].as_str().map(str::to_string))
            .collect()
    }

    fn ready(&mut self) {
        assert!(
            self.pump(|s| s.buffer_named("[composer]").is_some()),
            "the composer exists",
        );
    }
}

// ---------------------------------------------------------------------------
// Motion and editing
// ---------------------------------------------------------------------------

#[test]
fn typing_goes_in_at_the_cursor_rather_than_at_the_end() {
    let sb = Sandbox::new("insert");
    let mut s = sb.start();
    s.ready();

    s.type_text("helo");
    s.wait_composer(&["helo"]);
    s.special("left", &[]);
    s.ch("l");
    s.wait_composer(&["hello"]);
}

#[test]
fn backspace_takes_the_character_before_the_cursor() {
    let sb = Sandbox::new("backspace");
    let mut s = sb.start();
    s.ready();

    s.type_text("abcd");
    s.wait_composer(&["abcd"]);
    s.special("left", &[]);
    s.special("backspace", &[]);
    s.wait_composer(&["abd"]);
}

#[test]
fn home_and_end_reach_both_ends_of_the_line() {
    let sb = Sandbox::new("homeend");
    let mut s = sb.start();
    s.ready();

    s.type_text("world");
    s.wait_composer(&["world"]);
    s.special("home", &[]);
    s.type_text("hello ");
    s.wait_composer(&["hello world"]);
    s.special("end", &[]);
    s.ch("!");
    s.wait_composer(&["hello world!"]);
}

#[test]
fn control_arrows_move_a_word_at_a_time() {
    let sb = Sandbox::new("word");
    let mut s = sb.start();
    s.ready();

    s.type_text("alpha beta gamma");
    s.wait_composer(&["alpha beta gamma"]);
    // Two words back from the end lands at the start of `beta`, punctuation and space included in
    // the one press — the behaviour every editor has and the reason this is not char-at-a-time.
    s.special("left", &["ctrl"]);
    s.special("left", &["ctrl"]);
    s.ch("-");
    s.wait_composer(&["alpha -beta gamma"]);
}

#[test]
fn control_w_deletes_the_word_behind_the_cursor() {
    let sb = Sandbox::new("ctrlw");
    let mut s = sb.start();
    s.ready();

    s.type_text("one two three");
    s.wait_composer(&["one two three"]);
    s.press(json!({"kind": "char", "c": "w"}), &["ctrl"]);
    s.wait_composer(&["one two "]);
}

#[test]
fn control_u_clears_back_to_the_start_of_the_line() {
    let sb = Sandbox::new("ctrlu");
    let mut s = sb.start();
    s.ready();

    s.type_text("throw this away");
    s.wait_composer(&["throw this away"]);
    s.press(json!({"kind": "char", "c": "u"}), &["ctrl"]);
    s.wait_composer(&[""]);
}

#[test]
fn shift_enter_breaks_the_line_instead_of_sending() {
    // A composer you cannot put a second line in is a composer you paste code into and immediately
    // regret.
    let sb = Sandbox::new("newline");
    let mut s = sb.start();
    s.ready();

    s.type_text("first");
    s.wait_composer(&["first"]);
    s.special("enter", &["shift"]);
    s.type_text("second");
    s.wait_composer(&["first", "second"]);
}

#[test]
fn a_multi_line_paste_lands_at_the_cursor_and_stays_one_message() {
    let sb = Sandbox::new("paste");
    let mut s = sb.start();
    s.ready();

    s.type_text("[]");
    s.wait_composer(&["[]"]);
    s.special("left", &[]);
    s.send(&json!({"type": "paste", "text": "one\ntwo"}));
    s.wait_composer(&["[one", "two]"]);
}

#[test]
fn up_and_down_move_between_the_composers_lines_before_they_scroll() {
    let sb = Sandbox::new("vertical");
    let mut s = sb.start();
    s.ready();

    s.type_text("top");
    s.special("enter", &["shift"]);
    s.type_text("bottom");
    s.wait_composer(&["top", "bottom"]);

    // From the second line, `↑` is a motion. Typing proves where the cursor ended up.
    s.special("up", &[]);
    s.special("end", &[]);
    s.ch("!");
    s.wait_composer(&["top!", "bottom"]);
}

// ---------------------------------------------------------------------------
// Selecting and copying
// ---------------------------------------------------------------------------

#[test]
fn shift_and_arrow_selects_and_control_c_copies_it() {
    let sb = Sandbox::new("copy");
    let mut s = sb.start();
    s.ready();

    s.type_text("keep this");
    s.wait_composer(&["keep this"]);
    for _ in 0..4 {
        s.special("left", &["shift"]);
    }
    s.press(json!({"kind": "char", "c": "c"}), &["ctrl"]);

    assert!(
        s.pump(|s| s.copied().iter().any(|t| t == "this")),
        "the selection reached the clipboard, got {:?}",
        s.copied()
    );
    // And `^C` did *not* also throw the draft away, which is what it does with nothing selected.
    assert_eq!(s.composer(), vec!["keep this"]);
}

#[test]
fn control_c_with_nothing_selected_still_clears_the_draft() {
    // The other half of the same key. These two cannot both be true, which is what makes one key
    // able to carry both jobs.
    let sb = Sandbox::new("clear");
    let mut s = sb.start();
    s.ready();

    s.type_text("draft");
    s.wait_composer(&["draft"]);
    s.press(json!({"kind": "char", "c": "c"}), &["ctrl"]);
    s.wait_composer(&[""]);
    assert!(s.copied().is_empty(), "nothing was copied: {:?}", s.copied());
}

#[test]
fn control_x_cuts_the_selection_out_of_the_composer() {
    let sb = Sandbox::new("cut");
    let mut s = sb.start();
    s.ready();

    s.type_text("cut me");
    s.wait_composer(&["cut me"]);
    for _ in 0..3 {
        s.special("left", &["shift"]);
    }
    s.press(json!({"kind": "char", "c": "x"}), &["ctrl"]);

    s.wait_composer(&["cut"]);
    assert!(s.copied().iter().any(|t| t == " me"), "and it was copied: {:?}", s.copied());
}

#[test]
fn select_all_then_copy_takes_the_whole_draft() {
    let sb = Sandbox::new("selectall");
    let mut s = sb.start();
    s.ready();

    s.type_text("all of it");
    s.special("enter", &["shift"]);
    s.type_text("both lines");
    s.wait_composer(&["all of it", "both lines"]);

    s.press(json!({"kind": "char", "c": "a"}), &["ctrl"]);
    s.command("edit.copy");
    assert!(
        s.pump(|s| s.copied().iter().any(|t| t == "all of it\nboth lines")),
        "the whole thing, newline and all: {:?}",
        s.copied()
    );
}

// ---------------------------------------------------------------------------
// Reading the transcript
// ---------------------------------------------------------------------------

#[test]
fn you_can_move_into_the_transcript_and_copy_a_line_out_of_it() {
    // The thing the model said is the thing you actually want to keep, and before this there was
    // no way to reach it: the keyboard could only ever be in the composer.
    let sb = Sandbox::new("read");
    let mut s = sb.start();
    s.ready();
    assert!(s.pump(|s| s.chat().iter().any(|l| l.contains("neosh"))), "the greeting is there");

    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    assert!(s.pump(|s| s.copied().is_empty()), "nothing copied yet");

    // Top of the transcript, down past the word — six rows of it, under a blank — onto the
    // tagline, select to the end of it, yank.
    s.ch("g");
    s.ch("g");
    for _ in 0..7 {
        s.ch("j");
    }
    s.ch("v");
    s.ch("$");
    s.ch("y");

    assert!(
        s.pump(|s| s.copied().iter().any(|t| t.contains("terminal-first"))),
        "the greeting line came out: {:?}",
        s.copied()
    );
}

#[test]
fn select_all_while_reading_takes_the_whole_transcript() {
    let sb = Sandbox::new("readall");
    let mut s = sb.start();
    s.ready();
    assert!(s.pump(|s| s.chat().iter().any(|l| l.contains("neosh"))));

    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    s.ch("y");
    s.ch("a");
    assert!(
        s.pump(|s| s.copied().iter().any(|t| t.contains("neosh"))),
        "everything, not just the line under the cursor: {:?}",
        s.copied()
    );
}

#[test]
fn reading_says_so_and_escape_gives_the_keyboard_back() {
    let sb = Sandbox::new("readmode");
    let mut s = sb.start();
    s.ready();

    s.press(json!({"kind": "char", "c": "s"}), &["ctrl"]);
    assert!(
        s.pump(|s| s.buffer("[status]").iter().any(|l| l.contains("reading"))),
        "the status line says where the keyboard is: {:?}",
        s.buffer("[status]")
    );

    s.special("esc", &[]);
    // `j` is a motion while reading and a character once you are back.
    s.ch("j");
    s.wait_composer(&["j"]);
}

// ---------------------------------------------------------------------------
// Bindings
// ---------------------------------------------------------------------------

impl Sandbox {
    fn write_config(&self, contents: &str) {
        std::fs::write(self.root.join("config/config.toml"), contents).expect("config");
    }

    fn write_init(&self, contents: &str) {
        std::fs::write(self.root.join("config/init.ts"), contents).expect("init");
    }
}

#[test]
fn a_leader_binding_fires() {
    // The Neovim idiom, and the reason it matters: every binding in the program is yours to move,
    // including the ones that ship.
    let sb = Sandbox::new("leader");
    sb.write_config("[options]\nmapleader = \",\"\n");
    sb.write_init(
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("t.mark", () => neosh.notify("leader fired"));
  await neosh.keymap.set("chat", "<leader>pa", "t.mark");
  neosh.notify("bindings ready");
}
"#,
    );
    let mut s = sb.start();
    s.ready();
    // Wait for the binding, not just for a screen: `init.ts` runs after the composer exists, and a
    // key sent in between is an ordinary character.
    assert!(s.pump(|s| s.messages().iter().any(|m| m == "bindings ready")), "init.ts ran");

    s.ch(",");
    s.ch("p");
    s.ch("a");
    assert!(
        s.pump(|s| s.messages().iter().any(|m| m == "leader fired")),
        "the sequence ran the command: {:?}",
        s.messages()
    );
    // Concatenated rather than compared to a literal: an empty composer is one empty line, and
    // whether the frontend has been told about that line yet is not what this test is about.
    assert_eq!(s.composer().concat(), "", "and typed nothing");
}

#[test]
fn a_half_typed_binding_gives_up_and_types_what_you_pressed() {
    // Without this, binding a sequence whose first key is a printable character makes that
    // character untypeable: the composer swallows it and waits forever for a key you never meant
    // to press.
    let sb = Sandbox::new("timeout");
    sb.write_config("[options]\nmapleader = \",\"\ntimeoutlen = 120\n");
    sb.write_init(
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("t.mark", () => neosh.notify("leader fired"));
  await neosh.keymap.set("chat", "<leader>pa", "t.mark");
  neosh.notify("bindings ready");
}
"#,
    );
    let mut s = sb.start();
    s.ready();
    // Wait for the binding, not just for a screen: `init.ts` runs after the composer exists, and a
    // key sent in between is an ordinary character.
    assert!(s.pump(|s| s.messages().iter().any(|m| m == "bindings ready")), "init.ts ran");

    s.ch(",");
    s.wait_composer(&[","]);
    assert!(s.messages().iter().all(|m| m != "leader fired"), "and nothing was invoked");

    // Non-vacuous: the binding really is live, so the character above was genuinely being held.
    s.press(json!({"kind": "char", "c": "u"}), &["ctrl"]);
    s.wait_composer(&[""]);
    s.ch(",");
    s.ch("p");
    s.ch("a");
    assert!(
        s.pump(|s| s.messages().iter().any(|m| m == "leader fired")),
        "typed briskly, the same keys are a binding: {:?}",
        s.messages()
    );
}

#[test]
fn breaking_a_sequence_types_every_key_you_pressed_not_just_the_last() {
    // The bug this pins: with `,pa` bound, typing `,px` used to produce `x`. Two characters went
    // missing and there was nothing on screen to explain it.
    let sb = Sandbox::new("prefix");
    sb.write_config("[options]\nmapleader = \",\"\n");
    sb.write_init(
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("t.mark", () => neosh.notify("leader fired"));
  await neosh.keymap.set("chat", "<leader>pa", "t.mark");
  neosh.notify("bindings ready");
}
"#,
    );
    let mut s = sb.start();
    s.ready();
    // Wait for the binding, not just for a screen: `init.ts` runs after the composer exists, and a
    // key sent in between is an ordinary character.
    assert!(s.pump(|s| s.messages().iter().any(|m| m == "bindings ready")), "init.ts ran");

    s.ch(",");
    s.ch("p");
    s.ch("x");
    s.wait_composer(&[",px"]);

    // Non-vacuous: `,p` really was a live prefix, so those two keys were genuinely being held back
    // rather than never having been claimed at all.
    assert!(
        s.pump(|s| s.messages().iter().all(|m| m != "leader fired")),
        "and the binding did not fire on the way past"
    );
    s.press(json!({"kind": "char", "c": "u"}), &["ctrl"]);
    s.wait_composer(&[""]);
    s.ch(",");
    s.ch("p");
    s.ch("a");
    assert!(s.pump(|s| s.messages().iter().any(|m| m == "leader fired")));
}
