//! The bundled plugins, driven through the real binary.
//!
//! These exist to keep one claim honest: the sidebar and the switchers are *plugins*, written
//! against the same API a third party has. If any of them ever needed a private path into the host,
//! it would show up here as a call these tests cannot make.
//!
//! Everything is asserted on rendered text and emitted UI events — what a user would actually see.
//!
//! **The mock plugins here announce themselves from `neosh.ready`, never from the end of their own
//! `activate`**, and every `wait_for("… ready")` below is relying on that. A plugin's own
//! activation is the wrong moment to type at: the ten bundled plugins are still loading alongside
//! it, so `^E`, `^L` and `^T` are keys nothing has bound yet, `session.new` is a name nothing
//! answers to, and a configured `agent.model` naming this very plugin may still be held. A test
//! that pressed a key there was racing ten module loads — which is a race an idle laptop wins and
//! a busy CI runner loses, and losing it is what turned twenty of these red at once.

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
        let root = std::env::temp_dir().join(format!("neosh-builtin-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // `claude` and `codex` are the vendors' own homes, empty unless a test fills one. See
        // `Sandbox::write_usage_history`.
        for d in ["config", "state", "work", "claude/projects", "codex/sessions"] {
            std::fs::create_dir_all(root.join(d)).expect("sandbox dirs");
        }
        Self { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn write_config(&self, contents: &str) {
        std::fs::write(self.root.join("config/config.toml"), contents).expect("write config");
    }

    /// Make `work/` a git repository, so the git-dependent plugins have something to report.
    fn git_init(&self) {
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(self.work())
                .args(args)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "--initial-branch=trunk"]);
        run(&["config", "user.email", "test@neosh.invalid"]);
        run(&["config", "user.name", "neosh test"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(self.work().join("README.md"), "hello\n").expect("write");
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
    }

    /// One answer in a `claude` transcript, `hours_ago` old.
    ///
    /// The last-30-days block is read from `~/.claude/projects` and `~/.codex/sessions`, which are
    /// files another program wrote — so a test that asserts on it has to bring its own. Without
    /// this the assertion is about whatever the developer happened to have spent that month, which
    /// is a test that passes on a laptop and says "nothing in this span" on a fresh runner.
    fn write_usage_history(&self, model: &str, hours_ago: i64) {
        let at = now_secs() - hours_ago * 3_600;
        let dir = self.root.join("claude/projects/-tmp-neosh");
        std::fs::create_dir_all(&dir).expect("transcript dir");
        let line = serde_json::json!({
            "type": "assistant",
            "timestamp": iso_utc(at),
            "requestId": "req-1",
            "message": {
                "id": "msg-1",
                "model": model,
                "usage": {
                    "input_tokens": 12_000,
                    "output_tokens": 3_000,
                    "cache_read_input_tokens": 40_000,
                    "cache_creation_input_tokens": 1_000,
                },
            },
        });
        std::fs::write(dir.join("session.jsonl"), format!("{line}\n")).expect("transcript");
    }

    fn start(&self) -> Session {
        let child = Command::new(env!("CARGO_BIN_EXE_neosh"))
            .args(["--ui-protocol", "stdio"])
            .arg("--config-dir")
            .arg(self.root.join("config"))
            .arg("--cwd")
            .arg(self.work())
            .args(["--mock-script", &fixture().display().to_string()])
            .args(["--model", "mock/mock"])
            .env("NEOSH_STATE_DIR", self.root.join("state"))
            // The history block reads the *vendors'* transcripts rather than ours — see
            // `crate::usage`. Pointed at this sandbox, because the alternative is a hundred and
            // forty-seven neoshes scanning a real month of somebody's work and asserting on it.
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("neosh should start");
        Session::wrap(child)
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// A unix second as `YYYY-MM-DDTHH:MM:SSZ`, which is what a transcript timestamp looks like.
///
/// Hinnant's `civil_from_days`, the companion to the `days_from_civil` in `crate::usage` that
/// reads it back. Written out rather than pulled in: one date format in one direction is not worth
/// a dependency, and the two halves failing together is easier to see when they are both here.
fn iso_utc(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let (h, mi, sec) = (rest / 3_600, (rest % 3_600) / 60, rest % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{sec:02}Z")
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello_turn.jsonl")
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
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
        let out = BufReader::new(child.stdout.take().expect("stdout piped"));
        let (tx, lines) = channel();
        // On a thread, so the deadline is real: a blocking read ignores it and hangs the suite.
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
        let stdin = self.child.stdin.as_mut().expect("stdin piped");
        writeln!(stdin, "{line}").expect("write");
        stdin.flush().expect("flush");
    }

    fn key(&mut self, c: &str) {
        self.send(&format!(
            r#"{{"type":"key","key":{{"code":{{"kind":"char","c":"{c}"}},"mods":{{}}}}}}"#
        ));
    }

    /// `^N`, and wait until the conversation it starts is actually in the list.
    ///
    /// Worth a helper: starting one is now several round trips — it asks git whether there are
    /// worktrees to offer before deciding whether to ask you anything — so a test that pressed the
    /// key and immediately moved the cursor was moving it over a list about to gain a row.
    fn new_conversation(&mut self) {
        let before = self.sidebar_rows();
        self.ctrl("n");
        assert!(
            self.pump(move |s| s.sidebar_rows() > before),
            "a conversation was started\n{:?}",
            self.sidebar_now()
        );
    }

    /// How many conversation rows the panel is showing, however they are named.
    ///
    /// Counted by the age in the right-hand column rather than by the indent. A row the panel has
    /// unfolded is several lines, and a line of somebody's title is indented exactly as far as the
    /// row it belongs to — so the text alone cannot tell a second line of a title from an idle
    /// conversation. The age can: it is virtual text on the row, and a continuation has none.

    fn sidebar_rows(&self) -> usize {
        let text = self.sidebar_now();
        let virt = self.sidebar_virt_now();
        text.iter()
            .zip(virt.iter())
            .filter(|(l, v)| l.starts_with(CONVERSATION_INDENT) && !v.trim().is_empty())
            .count()
    }

    fn ctrl(&mut self, c: &str) {
        self.send(&format!(
            r#"{{"type":"key","key":{{"code":{{"kind":"char","c":"{c}"}},"mods":{{"ctrl":true}}}}}}"#
        ));
    }

    fn special(&mut self, kind: &str) {
        self.send(&format!(r#"{{"type":"key","key":{{"code":{{"kind":"{kind}"}},"mods":{{}}}}}}"#));
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

    fn saw(&self, needle: &str) -> bool {
        self.texts().iter().any(|l| l.contains(needle))
    }

    /// Buffers opened so far, by name. Used to tell "the sidebar exists" from "it is on screen".
    fn buffer_named(&self, name: &str) -> Option<u64> {
        self.events.iter().find_map(|e| {
            (e["type"] == "buffer_opened" && e["name"] == name).then(|| e["buf"].as_u64())?
        })
    }

    fn open_windows(&self) -> Vec<u64> {
        let mut open = Vec::new();
        for e in &self.events {
            match e["type"].as_str() {
                Some("window_opened") => {
                    if let Some(w) = e["win"].as_u64() {
                        open.push(w);
                    }
                }
                Some("window_closed") => {
                    if let Some(w) = e["win"].as_u64() {
                        open.retain(|x| *x != w);
                    }
                }
                _ => {}
            }
        }
        open
    }

    /// Every buffer that has ever been opened under `name`, newest last.
    ///
    /// A panel that is opened, closed and opened again is two buffers with one name, so a helper
    /// that took the first would be watching the one from last time forever.
    fn buffers_named(&self, name: &str) -> Vec<u64> {
        self.events
            .iter()
            .filter(|e| e["type"] == "buffer_opened" && e["name"] == name)
            .filter_map(|e| e["buf"].as_u64())
            .collect()
    }

    /// Wait until something on screen is showing a buffer called `name`.
    ///
    /// Not `wait_for` on its text: that scans everything the screen has *ever* said, so the second
    /// time a panel is opened it returns immediately — before the panel is there — and the keys
    /// meant for it go to the composer instead.
    fn wait_open(&mut self, name: &str) {
        let there = |s: &Session| s.buffers_named(name).iter().any(|b| !s.windows_for(*b).is_empty());
        assert!(self.pump(there), "{name} never opened\n{}", self.transcript());
    }

    /// Wait until nothing on screen is showing the buffer called `name`.
    ///
    /// The panels are plugin-drawn, so closing one is a round trip: the key resolves against the
    /// window that is still open, the plugin is told, and the window goes away a moment later. A
    /// test that types the next key immediately is typing it *into the panel* — which no person is
    /// fast enough to do, and every test is.
    fn wait_closed(&mut self, name: &str) {
        let gone = |s: &Session| {
            let bufs = s.buffers_named(name);
            !bufs.is_empty() && bufs.iter().all(|b| s.windows_for(*b).is_empty())
        };
        assert!(self.pump(gone), "{name} never closed\n{}", self.transcript());
    }

    /// Windows currently showing `buf`.
    fn windows_for(&self, buf: u64) -> Vec<u64> {
        let open = self.open_windows();
        self.events
            .iter()
            .filter(|e| e["type"] == "window_opened" && e["buf"].as_u64() == Some(buf))
            .filter_map(|e| e["win"].as_u64())
            .filter(|w| open.contains(w))
            .collect()
    }

    /// Every event this frontend has been sent, verbatim.
    ///
    /// The only way to assert a *negative* about the wire: that something the host knows never
    /// appears in what it sends. Rendered text is not enough — a value could be in a field nothing
    /// draws and still be in the JSON a frontend logs.
    fn wire(&self) -> String {
        self.events.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n")
    }

    fn transcript(&self) -> String {
        let mut s = String::from("--- text seen ---\n");
        for l in self.texts() {
            s.push_str(&format!("  {l}\n"));
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

#[test]
fn the_bundled_plugins_load_without_a_config_or_a_plugin_directory() {
    // They ship inside the binary, so a first run with an empty config directory still has them.
    let sb = Sandbox::new("load");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(s.saw("PROJECTS"), "the sidebar rendered\n{}", s.transcript());
}

#[test]
fn a_bundled_plugin_can_be_switched_off_from_config() {
    let sb = Sandbox::new("disable");
    sb.write_config("[options]\n\"plugins.disabled\" = [\"sidebar\"]\n");
    let mut s = sb.start();
    // Wait for something the *other* bundled plugin does, so this is not just a race with startup.
    s.drain_for(Duration::from_secs(3));
    assert!(
        s.buffer_named("[sidebar]").is_none(),
        "the sidebar should not have been created\n{}",
        s.transcript()
    );
    // And the rest still loaded: disabling one plugin must not disable them all.
    s.ctrl("p");
    s.wait_for("Model");
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

#[test]
fn the_branch_you_are_on_is_on_screen() {
    // In the footer rather than the panel: it is one line, it is true of the whole workspace, and
    // the footer is the part you can still see with the sidebar hidden.
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branch");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("trunk");
}

#[test]
fn the_sidebar_toggles_and_the_second_toggle_puts_it_back() {
    let sb = Sandbox::new("toggle");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let buf = s.buffer_named("[sidebar]").expect("sidebar buffer");
    assert_eq!(s.windows_for(buf).len(), 1, "open at startup");

    s.ctrl("b");
    assert!(s.pump(|s| s.windows_for(buf).is_empty()), "closes\n{}", s.transcript());

    s.ctrl("b");
    assert!(s.pump(|s| !s.windows_for(buf).is_empty()), "reopens\n{}", s.transcript());
}

#[test]
fn the_sidebar_stays_shut_when_config_says_so() {
    let sb = Sandbox::new("closed");
    sb.write_config("[options]\n\"sidebar.open\" = false\n");
    let mut s = sb.start();
    // The plugin still loads and still creates its buffer — it just does not open a window.
    assert!(s.pump(|s| s.buffer_named("[sidebar]").is_some()), "plugin loaded\n{}", s.transcript());
    let buf = s.buffer_named("[sidebar]").expect("buffer");
    s.drain_for(Duration::from_secs(2));
    assert!(s.windows_for(buf).is_empty(), "no window\n{}", s.transcript());
}

#[test]
fn outside_a_repository_the_sidebar_still_lists_the_project() {
    // `work/` is deliberately not a git repository here. Working outside version control is an
    // ordinary state, and a panel that renders an exception instead of your conversations would be
    // worse than one that renders less.
    let sb = Sandbox::new("norepo");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.drain_for(Duration::from_secs(1));
    let panel = s.sidebar_now();
    assert!(
        panel.iter().any(|l| l.contains("New conversation")),
        "the conversation is listed even with no repository\n{panel:?}"
    );
}

// ---------------------------------------------------------------------------
// Model and effort switchers
// ---------------------------------------------------------------------------

#[test]
fn the_model_picker_opens_and_lists_what_is_reachable() {
    let sb = Sandbox::new("modelpick");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");
    let rows = s.picker_now();
    // Two panes: providers on the left under the heading for what they cost, their models on the
    // right. The heading is the whole point — a plan and a key are different things to spend, and
    // one flat list could not say which a row was.
    assert!(rows.iter().any(|l| l.contains("LOCAL")), "grouped by account\n{rows:?}");
    assert!(
        rows.iter().any(|l| l.contains("Mock") && l.contains('\u{2502}')),
        "a rail entry and a model row on the same line\n{rows:?}"
    );
    // The model id is still there: it is unique per instance and not globally, so a picker that
    // dropped it would have to guess which endpoint a row means.
    assert!(rows.iter().any(|l| l.contains("mock")), "{rows:?}");
}

/// A second provider with two models, so the rail has somewhere to move to and the pane has
/// somewhere to move *within* — which is how "did the focus actually change panes" is answerable.
const OTHER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("other", [{
    id: "other", driver: "other", display_name: "Other",
    models: [
      { id: "other-1", display_name: "Other One" },
      { id: "other-2", display_name: "Other Two" },
    ],
  }], async (_req, emit) => { emit({ type: "message_stop" }); });
  neosh.event.on("neosh.ready", () => neosh.notify("other ready"));
}
"#;

fn install_other(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/other");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"other\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), OTHER).expect("plugin");
}

/// Moving into the rail and down it changes which provider's models are on the right.
///
/// Regression, and a nasty one: `<Tab>` is deliberately shared between `complete` and `pane_next`
/// on the reasoning that no widget has both. But the key was resolved against every action in a
/// fixed order regardless of which widget was asking, so `<Tab>` in the model picker came back as
/// `complete` — an action a rail picker has no case for — and did nothing at all. The second pane
/// was on screen, named in the shortcut row, and unreachable.
#[test]
fn the_rail_is_reachable_and_choosing_a_provider_lists_its_models() {
    let sb = Sandbox::new("railmove");
    install_other(&sb);
    let mut s = sb.start();
    s.wait_for("other ready");
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");

    // The shortcut row says how to get there, so it had better be true.
    assert!(
        s.picker_now().iter().any(|l| l.contains("providers")),
        "the picker says the rail is reachable\n{:?}",
        s.picker_now()
    );

    // Back a pane, into the rail; down it, onto the other provider; then *forward* a pane with
    // `<Tab>` and down again. If `<Tab>` reached the pane, that last press moved the model cursor.
    // If it did not, it moved the rail instead — off the end, leaving the cursor on the first
    // model of a provider whose list nobody asked to change.
    s.special("back_tab");
    s.special("down");
    assert!(
        s.pump(|s| s.picker_now().iter().any(|l| l.contains("Other One"))),
        "moving down the rail listed the other provider\n{:?}",
        s.picker_now()
    );
    s.special("tab");
    s.special("down");
    assert!(
        s.pump(|s| s
            .picker_now()
            .iter()
            .any(|l| l.contains("Other Two") && l.contains('\u{276f}'))),
        "and the cursor is on the second model, so <Tab> reached the pane\n{:?}",
        s.picker_now()
    );
}

/// A model you named yourself can be taken back out.
///
/// `n` existed and `d` did not, which meant a mistyped id was a row you had to read past forever —
/// and the most likely thing to be in a hand-typed list of model ids is a typo.
#[test]
fn a_model_you_added_yourself_can_be_removed_again() {
    let sb = Sandbox::new("customdel");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");

    // `^A`, then the id, then the name it shows as.
    s.ctrl("a");
    assert!(s.pump(|s| !s.prompt_now().is_empty()), "the id prompt opened");
    s.type_text("acme-1");
    s.enter();
    assert!(
        s.pump(|s| s.prompt_now().iter().any(|l| l.contains("Show it as"))),
        "then it asks what to call it\n{:?}",
        s.prompt_now()
    );
    s.type_text("!");
    s.enter();
    assert!(
        s.pump(|s| s.picker_now().iter().any(|l| l.contains("acme-1") && l.contains("yours"))),
        "and it is in the list, marked as yours\n{:?}",
        s.picker_now()
    );

    // Onto it — it sorts above the provider's own models — and out. Asked about first: you named
    // this one yourself, so nothing will offer it back.
    s.special("up");
    s.ctrl("d");
    assert!(
        s.pump(|s| s.saw("Remove acme-1")),
        "it asks before taking it away\n{}",
        s.transcript()
    );
    s.key("y");
    assert!(
        s.pump(|s| !s.picker_now().iter().any(|l| l.contains("acme-1"))),
        "and out again\n{:?}",
        s.picker_now()
    );
    assert!(
        s.saw("removed"),
        "and it says so"
    );
}

/// The filter can contain any letter, including the ones the picker's own actions used to take.
///
/// Regression: sign-in, refresh, add and remove were on bare `s`, `r`, `n` and `d`, checked before
/// the filter ever saw the key. So typing "sonnet" opened a sign-in prompt on the `s`, and there
/// was no way to search a long model list for anything whose name contained one of four very
/// common letters.
#[test]
fn typing_a_model_name_filters_rather_than_firing_a_command() {
    let sb = Sandbox::new("filterletters");
    install_other(&sb);
    let mut s = sb.start();
    s.wait_for("other ready");
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");
    // Onto the provider with two models to tell apart.
    s.special("back_tab");
    s.special("down");
    assert!(s.pump(|s| s.picker_now().iter().any(|l| l.contains("Other Two"))));
    s.special("tab");

    // Every letter here is one that used to be a command.
    s.type_text("nd");
    assert!(
        s.pump(|s| {
            let rows = s.picker_now();
            rows.iter().any(|l| l.contains("> nd"))
        }),
        "the letters went into the filter\n{:?}",
        s.picker_now()
    );
    assert!(
        s.prompt_now().is_empty() && !s.saw("asking again"),
        "and fired nothing: {:?}",
        s.prompt_now()
    );
}

/// `d` on a model the provider serves is not a deletion. It is not ours to delete, and saying
/// which of the two a row is beats doing nothing at all.
#[test]
fn d_on_a_model_the_provider_serves_explains_rather_than_deleting() {
    let sb = Sandbox::new("customdel2");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");
    let before = s.picker_now();

    s.ctrl("d");
    assert!(
        s.pump(|s| s.saw("nothing of yours to remove")),
        "it says which kind of row this is"
    );
    assert_eq!(s.picker_now(), before, "and nothing moved");
}

/// A picker says which keys it answers to.
///
/// Every one of them is a list you arrived at by pressing something, and none of them used to say
/// what to press next — which makes a modal you have to guess your way out of.
#[test]
fn a_picker_says_how_to_use_it() {
    let sb = Sandbox::new("pickerhints");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("k");
    assert!(
        s.pump(|s| s.picker_named("[Go to]").iter().any(|l| l.contains("choose"))),
        "the key strip is there\n{:?}",
        s.picker_named("[Go to]")
    );
    let rows = s.picker_named("[Go to]");
    let strip = rows.iter().find(|l| l.contains("choose")).expect("found above");
    assert!(strip.contains("close"), "and how to get out again: {strip:?}");
}

/// Written from the bindings, not from a string, so rebinding one does not make the strip a lie.
#[test]
fn the_key_strip_says_the_keys_that_are_actually_bound() {
    let sb = Sandbox::new("pickerhintsbound");
    sb.write_config("[options]\n\"ui.keys.accept\" = \"<C-y>\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");
    s.ctrl("k");
    assert!(
        // `^Y`, capitalised: nobody presses shift to send it, and the capital is how a terminal
        // has spelled a chord since curses.
        s.pump(|s| s.picker_named("[Go to]").iter().any(|l| l.contains("^Y choose"))),
        "the strip follows the setting\n{:?}",
        s.picker_named("[Go to]")
    );
}

#[test]
fn a_picker_owns_its_navigation_keys_while_it_is_open() {
    // `<C-n>` is bound globally to "new conversation". A raw capture only receives keys that no
    // binding claimed, so without window-scoped bindings, pressing it inside a picker started a
    // new conversation and took the picker off screen — while you were in the middle of typing a
    // filter. This is the fix, and the reason the fix has to be scoped rather than global.
    let sb = Sandbox::new("pickerkeys");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    let before = s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count();
    let windows = s.open_windows().len();
    s.ctrl("p");
    s.wait_for("Model");
    s.ctrl("n");
    s.drain_for(Duration::from_secs(1));
    assert_eq!(
        s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count(),
        before,
        "no conversation was created\n{:?}",
        s.sidebar_now()
    );

    // And the binding comes back the moment the picker is gone — the scope is the window, not the
    // session, so nothing is left holding a key it no longer needs. Waiting for the window to
    // actually close first: closing is a round trip to the plugin thread, and a key sent before it
    // lands is still the picker's.
    s.special("esc");
    assert!(s.pump(|s| s.open_windows().len() == windows), "the picker closed");
    s.ctrl("n");
    assert!(
        s.pump(|s| {
            s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count() > before
        }),
        "the global binding is back\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn the_navigation_keys_can_be_rebound() {
    let sb = Sandbox::new("rebind");
    sb.write_config("[options]\n\"ui.keys.next\" = \"<C-f>\"\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.ctrl("p");
    s.wait_for("Model");
    // `<C-f>` now moves. There is one model in the mock catalogue, so what this proves is that the
    // key reaches the picker at all rather than falling through to the composer.
    s.ctrl("f");
    s.drain_for(Duration::from_secs(1));
    assert!(
        !s.composer_now().iter().any(|l| l.contains('\u{6}')),
        "the rebound key was consumed by the picker\n{:?}",
        s.composer_now()
    );
    s.special("esc");
}

#[test]
fn typing_filters_the_picker_and_escape_leaves_the_model_alone() {
    let sb = Sandbox::new("filter");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Mock");

    // A query that cannot match anything must say so rather than silently showing everything.
    for c in ["z", "z", "z"] {
        s.key(c);
    }
    s.wait_for("no matches");

    s.special("esc");
    s.drain_for(Duration::from_secs(1));
    assert!(!s.saw("model: "), "dismissing must not change the selection\n{}", s.transcript());
}

#[test]
fn choosing_a_model_reports_the_new_selection() {
    let sb = Sandbox::new("choose");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Mock");
    s.special("enter");
    s.wait_for("model: mock/Mock");
}

#[test]
fn a_model_with_no_options_says_so_instead_of_opening_an_empty_panel() {
    // The mock driver exposes no `ProviderOptionDescriptor`s, so the switcher has nothing to offer.
    // Opening an empty panel and making the user press Escape would be worse than a message — and
    // the message names the model, because the answer to "why not" is almost always "not that one".
    let sb = Sandbox::new("effort");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("e");
    s.wait_for("has nothing to set");
}

// ---------------------------------------------------------------------------
// Git
// ---------------------------------------------------------------------------

#[test]
fn git_status_renders_the_working_tree() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("gitstatus");
    sb.git_init();
    std::fs::write(sb.work().join("new.txt"), "x\n").expect("write");
    std::fs::write(sb.work().join("README.md"), "changed\n").expect("write");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("g");
    // The two-column `XY` prefix git itself prints: staged state, then working tree. `?` for
    // untracked and `M` for modified — git's own letters. `U` here would claim a conflict.
    s.wait_for(" ?  new.txt");
    assert!(s.saw(" M  README.md"), "modified file listed\n{}", s.transcript());
}

#[test]
fn outside_a_repository_git_status_warns_rather_than_throwing() {
    let sb = Sandbox::new("gitnorepo");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("g");
    s.wait_for("no git repository");
}

// ---------------------------------------------------------------------------
// Generated branch names — the whole point of `neosh.gen`
// ---------------------------------------------------------------------------

impl Sandbox {
    /// Start with a specific recorded turn, for the generation tests.
    fn start_with(&self, fixture: &Path) -> Session {
        self.spawn(fixture, Some("mock/mock"))
    }

    /// Start without `--model`, so `agent.model` from config actually decides.
    ///
    /// `--model` is applied last on purpose — a flag that lost to a config file is an hour of
    /// someone's life — which means a test about config-chosen models must not pass it.
    fn start_letting_config_choose(&self) -> Session {
        self.spawn(&fixture(), None)
    }

    fn spawn(&self, script: &Path, model: Option<&str>) -> Session {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_neosh"));
        cmd.args(["--ui-protocol", "stdio"])
            .arg("--config-dir")
            .arg(self.root.join("config"))
            .arg("--cwd")
            .arg(self.work())
            .args(["--mock-script", &script.display().to_string()])
            .env("NEOSH_STATE_DIR", self.root.join("state"))
            // The history block reads the *vendors'* transcripts rather than ours — see
            // `crate::usage`. Pointed at this sandbox, because the alternative is a hundred and
            // forty-seven neoshes scanning a real month of somebody's work and asserting on it.
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env("CODEX_HOME", self.root.join("codex"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(m) = model {
            cmd.args(["--model", m]);
        }
        Session::wrap(cmd.spawn().expect("neosh should start"))
    }
}

fn branch_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/branch_name.jsonl")
}

impl Session {
    /// Type a string one key at a time, as a user would.
    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(&c.to_string());
        }
    }
}

#[test]
fn a_branch_name_is_generated_slugged_and_prefixed_from_config() {
    if !have_git() {
        return;
    }
    // The fixture answers with `"Fix the Login Redirect!"` wrapped in a ```json fence and a
    // preamble — which is what models actually do. Three separate things have to work for this to
    // pass: the host finding JSON inside prose, the plugin slugging it into a legal refname, and
    // the configured prefix surviving that slugging.
    let sb = Sandbox::new("branchgen");
    sb.git_init();
    sb.write_config("[options]\n\"git.branch.prefix\" = \"feature/\"\n");

    let mut s = sb.start_with(&branch_fixture());
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.type_text("the login page bounces");
    s.special("enter");

    // The proposed name is shown for approval before the branch exists.
    s.wait_for("feature/fix-the-login-redirect");
}

/// A prefix the user configured and a type the model chose are the same decision, made twice.
///
/// The built-in prompt asks for `fix/`, `feature/`, `chore/` and the rest, so a
/// `git.branch.prefix` bolted on unconditionally reads `feature/fix/the-login-redirect` — a
/// namespace nobody meant, under a type that is now wrong. The prefix is for a name that arrived
/// without one, which is what it does here and what it still does in the test above.
#[test]
fn a_configured_prefix_does_not_stack_on_a_type_the_model_chose() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branchtyped");
    sb.git_init();
    sb.write_config("[options]\n\"git.branch.prefix\" = \"feature/\"\n");

    let mut s = sb.start_with(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/branch_name_typed.jsonl"),
    );
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.type_text("the login page bounces");
    s.special("enter");

    s.wait_for("fix/the-login-redirect");
    assert!(
        !s.saw("feature/fix/the-login-redirect"),
        "and the configured prefix was not stacked on top of it\n{}",
        s.transcript()
    );
}

/// `git.branch.model` is a model, and an unset one is the fallback rather than a guess.
///
/// The option is `instance/model`, parsed in the plugin and handed to `gen.complete` as a
/// selection — so a malformed or absent one has to mean "you decide" all the way down to the host,
/// which then tries `gen.model` and finally the conversation's own. A plugin that filled in a
/// default here would be a third place the precedence is written, and the first to disagree.
#[test]
fn the_model_that_names_a_branch_is_a_setting() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branchmodel");
    sb.git_init();
    sb.write_config("[options]\n\"git.branch.model\" = \"mock/mock\"\n");

    let mut s = sb.start_with(&branch_fixture());
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.type_text("the login page bounces");
    s.special("enter");

    // Named at all is the assertion: a selection the host could not resolve fails the call, and
    // the plugin says so instead of proposing a name.
    s.wait_for("fix-the-login-redirect");
}

/// A scratch worktree is named by the first thing asked in it, and the panel says so.
///
/// The whole shape of this: `git.worktree.new.inside` gives you `brisk-otter` because naming a
/// branch before the work is a decision made at the worst possible moment — and the first message
/// *is* that decision, arriving on its own. Three things have to happen and the third is the one
/// that was missing: git renames the branch, the host notices that the label it wrote down when it
/// first saw the directory is now wrong, and the sidebar row — which is that label — redraws.
#[test]
fn a_scratch_worktree_is_named_by_the_first_message_and_the_sidebar_follows() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branchauto");
    sb.git_init();

    let mut s = sb.start_with(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/branch_name_typed.jsonl"),
    );
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new.inside"));
    let under = sb.work().join(".worktrees");
    assert!(
        s.pump(|_| std::fs::read_dir(&under).is_ok_and(|d| d.count() > 0)),
        "the worktree exists, on a name nobody chose"
    );
    let tree = std::fs::read_dir(&under)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .next()
        .expect("one worktree");

    // Whatever two words it landed on. Named for us, which is exactly what makes it replaceable.
    let scratch = head_of(&tree);
    assert!(scratch.contains('-'), "a two-word scratch name, not {scratch:?}");

    // Waited for rather than assumed: creating the tree also starts a conversation in it and
    // switches to it, and a test that types before that lands is typing into the conversation in
    // the *main* checkout — where there is no scratch branch and nothing to rename.
    let row_says = |s: &Session, want: &str| s.sidebar_now().iter().any(|l| l.contains(want));
    let named_scratch = scratch.clone();
    assert!(
        s.pump(move |s| row_says(s, &named_scratch)),
        "the worktree is a row in the panel, under the name nobody chose\n{:?}",
        s.sidebar_now()
    );

    // The first thing asked in it. The turn and the naming call both read the same fixture, so
    // which of them the mock answers first does not decide the test.
    s.type_text("the login page bounces");
    s.special("enter");

    assert!(
        s.pump(|_| head_of(&tree) == "fix/the-login-redirect"),
        "git renamed the branch — it is on {:?}\n{}",
        head_of(&tree),
        s.transcript()
    );
    // And the row followed. Without the host putting right the label it wrote down the first time
    // it saw this directory, the panel says the scratch name until the workspace restarts.
    //
    // Matched on the start of the name, because the row is a branch in a narrow column and the
    // panel elides the end of it — `⎇ fix/the-login-redire…`. Asserting the whole string would be
    // asserting the sidebar's width.
    assert!(
        s.pump(move |s| row_says(s, "⎇ fix/the-login")),
        "the sidebar row followed it\n{:?}",
        s.sidebar_now()
    );
    let gone = scratch.clone();
    assert!(
        !s.sidebar_now().iter().any(|l| l.contains(&gone)),
        "and the scratch name is not still a row beside it\n{:?}",
        s.sidebar_now()
    );
}

/// Which branch a checkout is on, asked of git rather than of anything under test.
fn head_of(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn the_generated_name_is_editable_before_the_branch_is_created() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branchedit");
    sb.git_init();
    let mut s = sb.start_with(&branch_fixture());
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.type_text("x");
    s.special("enter");
    s.wait_for("fix-the-login-redirect");

    // Clear the field and type our own, then accept.
    s.ctrl("u");
    s.type_text("my-own-name");
    s.special("enter");
    // The footer, which is where the branch lives. It used to be a toast saying the same thing,
    // and a message restating what is already on screen is exactly what ADR 0057 took away — so
    // the branch segment is now refreshed on the spot instead, and this waits on that.
    s.wait_for("my-own-name  mock");

    let branch = Command::new("git")
        .current_dir(sb.work())
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git runs");
    assert_eq!(
        String::from_utf8_lossy(&branch.stdout).trim(),
        "my-own-name",
        "the branch git actually created"
    );
}

#[test]
fn dismissing_the_description_creates_nothing() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("branchcancel");
    sb.git_init();
    let mut s = sb.start_with(&branch_fixture());
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.special("esc");
    s.drain_for(Duration::from_secs(2));

    let branches = Command::new("git")
        .current_dir(sb.work())
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .output()
        .expect("git runs");
    let names = String::from_utf8_lossy(&branches.stdout);
    assert_eq!(names.trim(), "trunk", "no branch was created");
}

/// Invoke a command the way a menu or a palette entry would.
///
/// Not a synthesised keystroke: a command with no default binding has no key to synthesise, and one
/// that does would break the moment a user rebound it.
fn command(name: &str) -> String {
    format!(r#"{{"type":"command","name":"{name}"}}"#)
}

fn command_with(name: &str, arg: &str) -> String {
    format!(r#"{{"type":"command","name":"{name}","args":["{arg}"]}}"#)
}

#[test]
fn a_plugin_that_did_not_declare_vcs_write_is_refused() {
    if !have_git() {
        return;
    }
    // Enforcement is per plugin, from the manifest — not the agent's permission mode. This proves
    // the check is real: identical code, one word missing from `plugin.toml`.
    let sb = Sandbox::new("nodeclare");
    sb.git_init();
    let dir = sb.root.join("config/plugins/meddler");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"meddler\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("write manifest");
    std::fs::write(
        dir.join("main.ts"),
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  // Reading is fine and needs nothing declared.
  const status = await neosh.git.status();
  neosh.notify(`read ok: ${status.repo.branch}`);
  try {
    await neosh.git.createBranch("sneaky");
    neosh.notify("WROTE ANYWAY");
  } catch (e) {
    neosh.notify(`refused: ${e}`);
  }
}
"#,
    )
    .expect("write plugin");

    let mut s = sb.start();
    s.wait_for("read ok: trunk");
    s.wait_for("refused:");
    assert!(s.saw("vcs_write"), "the refusal names what to declare\n{}", s.transcript());
    assert!(!s.saw("WROTE ANYWAY"), "the write must not have happened\n{}", s.transcript());

    let branches = Command::new("git")
        .current_dir(sb.work())
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .output()
        .expect("git runs");
    assert_eq!(String::from_utf8_lossy(&branches.stdout).trim(), "trunk");
}

#[test]
fn clean_drops_user_config_but_keeps_the_bundled_plugins() {
    // `--clean` answers "is this neosh, or is it my setup". The sidebar and the switchers *are*
    // neosh — starting with no interface at all would answer a question nobody asked. To suspect
    // one of them, `plugins.disabled` names it.
    let sb = Sandbox::new("clean");
    let child = Command::new(env!("CARGO_BIN_EXE_neosh"))
        .args(["--ui-protocol", "stdio", "--clean"])
        .arg("--cwd")
        .arg(sb.work())
        .args(["--mock-script", &fixture().display().to_string()])
        .args(["--model", "mock/mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("neosh starts");
    let mut s = Session::wrap(child);

    s.wait_for("--clean");
    s.wait_for("PROJECTS");
    assert!(s.buffer_named("[sidebar]").is_some(), "the bundled UI is still there");
}

/// A provider plugin whose model exposes a knob the host has never heard of.
///
/// The claim under test is the strong form: a driver invents an option, and the bundled switcher
/// renders and applies it with **no change to the switcher**. That is the whole reason
/// `ProviderOptionDescriptor` exists rather than a hard-coded reasoning-effort enum.
const INVENTED_KNOB: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("invented", [{
    id: "lab",
    driver: "invented",
    display_name: "Lab",
    models: [{
      id: "brainy",
      display_name: "Brainy",
      capabilities: {
        tools: false, vision: false, streaming: true, thinking: true, prompt_caching: false,
        option_descriptors: [{
          type: "select",
          id: "vibe",
          label: "Vibe",
          options: [
            { id: "chill", label: "Chill", is_default: true },
            { id: "intense", label: "Intense", description: "more of it", is_default: false },
          ],
        }],
      },
    }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "brainy", usage: {} });
    emit({ type: "message_stop" });
  });

  await neosh.cmd.register("lab.report", async () => {
    const sel = await neosh.agent.selection();
    neosh.notify(`selection: ${sel?.instance}/${sel?.model} ${JSON.stringify(sel?.options ?? [])}`);
  });
  neosh.event.on("neosh.ready", () => neosh.notify("lab ready"));
}
"#;

/// A driver whose top level is a *word* rather than a parameter.
///
/// The shape a vendor harness has: a ladder that can be sent, plus a level you buy by saying it. The
/// handler reads back both halves of what arrived, which is the only way to tell the difference
/// between "the word was injected" and "the word was sent as the effort, and the endpoint would have
/// rejected it".
const SPOKEN_KNOB: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("spoken", [{
    id: "lab",
    driver: "spoken",
    display_name: "Lab",
    models: [{
      id: "wordy",
      display_name: "Wordy",
      capabilities: {
        tools: false, vision: false, streaming: true, thinking: true, prompt_caching: false,
        option_descriptors: [{
          type: "select",
          id: "effort",
          label: "Effort",
          options: [
            { id: "low", label: "Low", is_default: false },
            { id: "high", label: "High", is_default: true },
            { id: "abracadabra", label: "Abracadabra", is_default: false },
          ],
          prompt_injected_values: ["abracadabra"],
        }],
      },
    }],
  }], async (req, emit) => {
    const last = req.messages[req.messages.length - 1];
    const text = last?.content.map((b) => (b.type === "text" ? b.text : "")).join("") ?? "";
    const effort = req.selection.options?.find((o) => o.id === "effort")?.value ?? "none";
    emit({ type: "message_start", model: "wordy", usage: {} });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: `got[${text.replace(/\n/g, "|")}] effort[${effort}]` });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("spoken ready"));
}
"#;

/// A driver with a full ladder in it: three rungs, plus one superseded model.
const LADDER: &str = r#"
import type { PluginContext } from "@neosh/api";

const model = (id: string, name: string, tier: string, legacy: boolean) => ({
  id,
  display_name: name,
  family: "rung",
  tier,
  legacy,
  tagline: `the ${tier} one`,
  capabilities: {
    tools: false, vision: false, streaming: true, thinking: false, prompt_caching: false,
    option_descriptors: [],
  },
});

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("ladder", [{
    id: "lab",
    driver: "ladder",
    display_name: "Lab",
    models: [
      model("big", "Big", "frontier", false),
      model("mid", "Mid", "balanced", false),
      model("small", "Small", "fast", false),
      model("old", "Old", "balanced", true),
    ],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "mid", usage: {} });
    emit({ type: "message_stop" });
  });

  await neosh.cmd.register("lab.report", async () => {
    const sel = await neosh.agent.selection();
    neosh.notify(`selection: ${sel?.instance}/${sel?.model}`);
  });
  neosh.event.on("neosh.ready", () => neosh.notify("ladder ready"));
}
"#;

fn install_ladder(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), LADDER).expect("write plugin");
}

#[test]
fn one_step_down_is_a_rung_not_a_list() {
    // The whole reason models carry a tier. "Give me something cheaper" should be a keystroke, and
    // it should stay on the provider you are already paying for.
    let sb = Sandbox::new("ladder");
    install_ladder(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"lab/big\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("ladder ready");

    s.send(&command("model.downgrade"));
    s.wait_for("model: lab/Mid");
    s.send(&command("model.downgrade"));
    s.wait_for("model: lab/Small");

    // No wrapping. A downgrade that teleports you to the most expensive thing in the catalogue is
    // a keystroke you learn not to press.
    s.send(&command("model.downgrade"));
    s.wait_for("already the quickest one here");
    s.send(&command("lab.report"));
    s.wait_for("selection: lab/small");

    s.send(&command("model.upgrade"));
    s.wait_for("model: lab/Mid");
}

#[test]
fn a_line_resolves_to_the_one_that_has_not_been_superseded() {
    // "Use Opus" is a statement about a product line, not about a pinned id that goes stale. The
    // superseded entry sits on the same rung as the current one, so picking wrongly is possible
    // and would be silent.
    let sb = Sandbox::new("line");
    install_ladder(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"lab/small\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("ladder ready");

    s.send(&command_with("model.line", "rung"));
    s.wait_for("model: lab/Big");
    s.send(&command_with("model.line", "nonesuch"));
    s.wait_for(r#"nothing reachable is in the "nonesuch" line"#);
}

#[test]
fn superseded_models_are_folded_behind_a_count() {
    // Reachable and out of the way. You go looking for last year's model to reproduce something,
    // which is exactly when hiding it outright would be worst.
    let sb = Sandbox::new("legacy");
    install_ladder(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"lab/big\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("ladder ready");

    s.ctrl("p");
    s.wait_for("Model");
    let rows = s.picker_now();
    assert!(rows.iter().any(|l| l.contains("Big")), "the current models are listed\n{rows:?}");
    assert!(
        rows.iter().any(|l| l.contains("1 superseded")),
        "and the old one is behind a count\n{rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("Old")),
        "which means it is not in the way\n{rows:?}"
    );
}

#[test]
fn the_rail_says_what_a_provider_costs_before_you_pick_from_it() {
    // A plan you already pay for and a key billed per token are different decisions. The old flat
    // list could not say which a row was, which is why "it says I have no API key" was reasonable.
    let sb = Sandbox::new("accounts");
    sb.write_config(NEEDS_A_KEY);
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");
    let rows = s.picker_now();
    assert!(rows.iter().any(|l| l.contains("API KEYS")), "grouped by account\n{rows:?}");
    assert!(rows.iter().any(|l| l.contains("LOCAL")), "grouped by account\n{rows:?}");
    assert!(rows.iter().any(|l| l.contains("Acme")), "the configured instance is on the rail\n{rows:?}");
}

#[test]
fn a_driver_can_invent_an_option_and_the_switcher_renders_it() {
    let sb = Sandbox::new("invented");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), INVENTED_KNOB).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/brainy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("lab ready");

    // The switcher has never heard of "vibe" and does not need to. Every value is on screen at
    // once — that is what the panel is for — so both of them are visible before anything is chosen.
    s.ctrl("e");
    s.wait_open("[model options]");
    s.wait_for("Vibe");
    assert!(s.saw("Chill"), "choices rendered\n{}", s.transcript());
    assert!(s.saw("Intense"), "choices rendered\n{}", s.transcript());

    // Along the row, then out. Right rather than down: the values of one knob run across, and the
    // knobs run down — which is the difference between this and the two nested lists it replaced.
    s.special("right");
    s.special("enter");
    s.wait_closed("[model options]");

    // And it is really on the selection the next turn would use. Asserted here rather than from a
    // notification, because there is no longer one to assert: the panel applies as you move, so the
    // *state* is the acknowledgement.
    s.send(&command("lab.report"));
    s.wait_for(r#"selection: lab/brainy [{"id":"vibe","value":"intense"}]"#);
}

#[test]
fn a_level_that_is_a_word_is_said_rather_than_sent() {
    // The half of `prompt_injected_values` that never existed. The field has been on the wire since
    // ADR 0011 and nothing acted on it, so choosing such a level put it where a parameter goes —
    // `--effort abracadabra`, which is not a level, and the turn fails.
    let sb = Sandbox::new("spoken");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), SPOKEN_KNOB).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/wordy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("spoken ready");
    s.ctrl("e");
    s.wait_open("[model options]");
    // Past the top of the sendable ladder: low, high, and then the word.
    s.special("right");
    s.special("right");
    s.special("enter");
    s.wait_closed("[model options]");

    for c in ["h", "e", "l", "l", "o"] {
        s.key(c);
    }
    s.special("enter");

    // Both halves at once. The word arrived at the top of the message, and what went in the
    // parameter is the ladder's own default rather than the word — so a driver that would have
    // rejected it never sees it, and a driver whose default is lower does not quietly get its way.
    s.wait_for("got[abracadabra||hello] effort[high]");
}

#[test]
fn nothing_else_reaches_the_keyboard_while_the_option_sheet_is_open() {
    // Every global key used to fall straight through it. `^N` opened a new conversation *behind*
    // the panel, which stayed on screen; `^T` opened the project panel underneath and moved focus
    // into it. Shadowing the keys the panel wants could never have fixed this — a panel can name
    // the keys it wants and never the ones it merely does not want to happen. See ADR 0047.
    let sb = Sandbox::new("modalsheet");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), INVENTED_KNOB).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/brainy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("lab ready");
    s.ctrl("e");
    s.wait_open("[model options]");

    s.ctrl("n");
    s.ctrl("t");
    // A letter, too: a modal that let characters through would be typing into the composer behind
    // the float, where you cannot see what you are writing.
    s.key("z");
    // Nothing to wait *for* — the assertion is that none of those three did anything — so this is
    // the one case that has to spend real time before it can say so.
    s.drain_for(Duration::from_millis(1200));

    assert!(
        s.buffers_named("[New conversation]").is_empty(),
        "`^N` opened a conversation behind the panel\n{}",
        s.transcript()
    );
    let sheet = s.buffers_named("[model options]");
    assert!(
        sheet.iter().any(|b| !s.windows_for(*b).is_empty()),
        "the panel that ate the keys is not even on screen\n{}",
        s.transcript()
    );

    // And the key that opened it closes it, which is the obligation modality creates: the global
    // `^E` no longer arrives to toggle it off.
    s.ctrl("e");
    s.wait_closed("[model options]");
}

#[test]
fn a_knob_row_says_it_is_a_control_before_you_press_anything() {
    // A ladder with one rung lit reads identically whether or not `h`/`l` do anything to it — which
    // is how a live control came to look like a summary, and why `↵` on it (which means "done")
    // felt like a key that does nothing at all.
    let sb = Sandbox::new("railsheet");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), INVENTED_KNOB).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/brainy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("lab ready");
    s.ctrl("e");
    s.wait_open("[model options]");
    s.wait_for("Vibe");

    assert!(
        s.texts().iter().any(|l| l.contains("‹") && l.contains("›") && l.contains("Chill")),
        "the row is drawn without the rail that says it is one\n{}",
        s.transcript()
    );
    // And the key that changes something is named before the key that leaves.
    // Letters, not arrows: the arrows are bound too, and the row is a promise about a keyboard.
    assert!(s.saw("h l change"), "the hints lead with the key that does the thing\n{}", s.transcript());
}

#[test]
fn what_you_typed_is_what_the_transcript_keeps() {
    // The word belongs to the *request*, not to the conversation. Written into the stored message it
    // would be replayed on every later turn — so turning the level off would not turn it off — and
    // it would be sitting in the transcript above an answer, as something you never said.
    let sb = Sandbox::new("spokenkeep");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), SPOKEN_KNOB).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/wordy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("spoken ready");
    s.ctrl("e");
    s.wait_open("[model options]");
    s.special("right");
    s.special("right");
    s.special("enter");
    s.wait_closed("[model options]");
    for c in ["h", "i"] {
        s.key(c);
    }
    s.special("enter");
    s.wait_for("got[abracadabra||hi]");

    // Back down the ladder, and ask again. If the first turn's word had been kept, this one would
    // carry it too.
    s.ctrl("e");
    s.wait_open("[model options]");
    s.special("left");
    s.special("left");
    s.special("enter");
    s.wait_closed("[model options]");
    for c in ["y", "o"] {
        s.key(c);
    }
    s.special("enter");
    s.wait_for("got[yo] effort[low]");
}

#[test]
fn switching_models_keeps_an_option_the_new_one_also_has() {
    // Switching between two thinking models should not silently reset your effort level; switching
    // to one without the knob must drop it rather than send a value the driver would reject.
    let sb = Sandbox::new("carryover");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(
        dir.join("main.ts"),
        &INVENTED_KNOB.replace(
            "models: [{",
            "models: [{ id: \"plain\", display_name: \"Plain\", capabilities: { tools: false, \
             vision: false, streaming: true, thinking: false, prompt_caching: false, \
             option_descriptors: [] } }, {",
        ),
    )
    .expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"lab/brainy\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("lab ready");
    s.ctrl("e");
    s.wait_open("[model options]");
    s.wait_for("Vibe");
    s.special("right");
    s.special("enter");
    s.wait_closed("[model options]");

    // Move to "Plain", which has no options at all.
    s.ctrl("p");
    s.wait_for("Plain");
    for c in ["P", "l", "a", "i", "n"] {
        s.key(c);
    }
    s.special("enter");
    s.wait_for("model: lab/Plain");

    s.send(&command("lab.report"));
    s.wait_for("selection: lab/plain []");
}

#[test]
fn a_model_answer_git_would_reject_is_made_into_a_legal_refname() {
    if !have_git() {
        return;
    }
    // `git check-ref-format` refuses every part of this: leading dashes, `//`, a `.lock` suffix,
    // spaces, and trailing punctuation. The slug runs in the plugin rather than the host so the
    // name shown for approval is exactly the name that gets created.
    let sb = Sandbox::new("nastyname");
    sb.git_init();
    let mut s = sb.start_with(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/branch_name_nasty.jsonl"),
    );
    s.wait_for("PROJECTS");

    s.send(&command("git.branch.new"));
    s.wait_for("What are you about to work on?");
    s.type_text("x");
    s.special("enter");
    s.wait_for("fix/the-login");
    s.special("enter");
    // The footer rather than a toast — see ADR 0057.
    s.wait_for("fix/the-login  mock");

    // The real proof: git accepted it.
    let branch = Command::new("git")
        .current_dir(sb.work())
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git runs");
    assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), "fix/the-login");
}

// ---------------------------------------------------------------------------
// Getting out
// ---------------------------------------------------------------------------

#[test]
fn ctrl_c_closes_a_picker_rather_than_arming_a_quit() {
    // `<C-c>` is bound globally to `interrupt`. While a picker is on screen it obviously means
    // "close this", so the picker binds it window-scoped, which outranks the global binding.
    let sb = Sandbox::new("ctrlc-picker");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Model");

    let picker_buf = s.buffer_named("[Model]").expect("picker buffer");
    assert_eq!(s.windows_for(picker_buf).len(), 1);

    s.ctrl("c");
    assert!(
        s.pump(|s| s.windows_for(picker_buf).is_empty()),
        "the picker should close\n{}",
        s.transcript()
    );
    assert!(
        !s.saw("press ^C again"),
        "closing a picker must not also arm a quit\n{}",
        s.transcript()
    );
}

// ---------------------------------------------------------------------------
// The thread list, driven from the keyboard
// ---------------------------------------------------------------------------

impl Session {
    fn down(&mut self) {
        self.send(r#"{"type":"key","key":{"code":{"kind":"down"},"mods":{}}}"#);
    }

    fn enter(&mut self) {
        self.send(r#"{"type":"key","key":{"code":{"kind":"enter"},"mods":{}}}"#);
    }

    /// Focus the panel and wait until it has the keyboard.
    ///
    /// `<C-t>` reaches the plugin thread asynchronously, so a key sent straight afterwards races it
    /// and lands in the composer instead. The hint strip switching to the in-panel verbs is the
    /// signal that focus and the key capture are both in place.
    fn enter_panel(&mut self) {
        self.ctrl("t");
        assert!(
            self.pump(|s| s.sidebar_now().iter().any(|l| l.contains("? keys"))),
            "the panel took the keyboard\n{:?}",
            self.sidebar_now()
        );
    }

    /// Virtual text attached to the sidebar's lines.
    ///
    /// Right-aligned columns — ages, counts, elapsed times — are extmarks rather than buffer text,
    /// because only the frontend knows how wide the pane is or how many columns a character takes.
    fn sidebar_virt(&self) -> Vec<String> {
        let Some(buf) = self.buffer_named("[sidebar]") else { return Vec::new() };
        self.events
            .iter()
            .filter(|e| e["type"] == "buffer_lines" && e["buf"].as_u64() == Some(buf))
            .filter_map(|e| e["lines"].as_array())
            .flatten()
            .filter_map(|l| l["marks"].as_array())
            .flatten()
            .filter_map(|m| m["virt_text"].as_array())
            .flatten()
            .filter_map(|c| c["text"].as_str().map(str::to_string))
            .collect()
    }

    /// The sidebar buffer's current contents, folded the way a frontend folds splices.
    /// The lines of the model picker, whatever pane they belong to.
    fn picker_now(&self) -> Vec<String> {
        self.picker_named("[Model]")
    }

    /// The newest buffer with a name, for widgets that open one per use.
    ///
    /// A prompt asks two questions by opening two buffers both called `[prompt]`, so matching the
    /// first one means reading the answer to a question that has already been answered.
    fn latest_named(&self, name: &str) -> Option<u64> {
        self.events.iter().rev().find_map(|e| {
            (e["type"] == "buffer_opened" && e["name"] == name).then(|| e["buf"].as_u64())?
        })
    }

    fn prompt_now(&self) -> Vec<String> {
        match self.latest_named("[prompt]") {
            Some(b) => self.lines_of(b),
            None => Vec::new(),
        }
    }

    /// The contents of a picker window, by the title it was opened with.
    fn picker_named(&self, name: &str) -> Vec<String> {
        match self.buffer_named(name) {
            Some(buf) => self.lines_of(buf),
            None => Vec::new(),
        }
    }

    /// The highlight groups on each row of the transcript.
    fn chat_groups(&self) -> Vec<Vec<String>> {
        let name = self.events.iter().find_map(|e| {
            let n = e["name"].as_str()?;
            (e["type"] == "buffer_opened" && n.starts_with("[chat]")).then(|| n.to_string())
        });
        match name.and_then(|n| self.buffer_named(&n)) {
            Some(b) => self.groups_of(b),
            None => Vec::new(),
        }
    }

    fn sidebar_now(&self) -> Vec<String> {
        match self.buffer_named("[sidebar]") {
            Some(b) => self.lines_of(b),
            None => Vec::new(),
        }
    }

    /// Replay every edit to one buffer, so what comes back is what is on screen now rather than
    /// everything that was ever drawn there.
    /// The highlight groups on each row of a buffer, folded the same way its text is.
    ///
    /// Needed because a mark is not text: inserting a row above a marked one moves the mark with
    /// it, and an off-by-one there is invisible in the transcript's contents and glaring on screen.
    fn groups_of(&self, buf: u64) -> Vec<Vec<String>> {
        let mut rows: Vec<Vec<String>> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = e["start"].as_i64().unwrap_or(0);
            let old_end = e["old_end"].as_i64().unwrap_or(start);
            let new: Vec<Vec<String>> = e["lines"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|l| {
                            l["marks"]
                                .as_array()
                                .map(|ms| {
                                    ms.iter()
                                        .filter_map(|m| m["hl_group"].as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let s = start.clamp(0, rows.len() as i64) as usize;
            let en = if old_end < 0 { rows.len() } else { (old_end as usize).clamp(s, rows.len()) };
            rows.splice(s..en, new);
        }
        rows
    }

    fn lines_of(&self, buf: u64) -> Vec<String> {
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
}

#[test]
fn the_sidebar_lists_conversations_under_their_project() {
    let sb = Sandbox::new("threads");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["a", "b", "c"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("abc");

    // The conversation appears in the list, labelled by what was said in it.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("abc"))),
        "the conversation is listed\n{:?}",
        s.sidebar_now()
    );
    // And under a project heading, not loose.
    let panel = s.sidebar_now();
    // Trimmed: rows carry a left margin, and this test is about ordering rather than indentation.
    let projects = panel.iter().position(|l| l.trim() == "PROJECTS").expect("heading");
    let thread = panel.iter().position(|l| l.contains("abc")).expect("row");
    assert!(thread > projects, "the row sits under the heading\n{panel:?}");
}

#[test]
fn a_new_conversation_appears_in_the_list_and_becomes_the_active_one() {
    let sb = Sandbox::new("newthread");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["o", "n", "e"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("one");

    s.ctrl("n");
    assert!(
        s.pump(|s| {
            let p = s.sidebar_now();
            p.iter().filter(|l| l.contains("New conversation")).count() == 1
                && p.iter().any(|l| l.contains("one"))
        }),
        "both conversations listed\n{:?}",
        s.sidebar_now()
    );
    // The new one is the active one: exactly one row carries the active marker.
    let active = s.sidebar_now().iter().filter(|l| l.trim_start().starts_with("▸ ")).count();
    assert_eq!(active, 1, "one active row\n{:?}", s.sidebar_now());
}

#[test]
fn moving_and_selecting_in_the_thread_list_switches_conversation() {
    let sb = Sandbox::new("select");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["f", "i", "r", "s", "t"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("first");

    s.new_conversation(); // a second conversation, now active
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("New conversation"))));

    // Into the list, down to the other row, select it.
    s.enter_panel();
    s.down();
    s.enter();

    // The chat buffer comes back with the first conversation's text.
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("first"))),
        "switched back to the first conversation\n{:?}",
        s.chat_now()
    );
}

// ---------------------------------------------------------------------------
// Favourites, order and folding
// ---------------------------------------------------------------------------

/// Install a plugin that stalls the agent inside `turn.pre`.
///
/// The mock provider answers instantly, which makes "a turn is running" impossible to observe from
/// the outside. A blocking hook that never resolves holds the turn open for as long as the test
/// needs, without any sleeping or racing.
fn install_staller(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/staller");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"staller\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"hooks_blocking\"]\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("main.ts"),
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.hook.register("turn_pre", () => new Promise(() => {}), {
    blocking: true,
    timeoutMs: 25000,
  });
  neosh.event.on("neosh.ready", () => neosh.notify("staller ready"));
}
"#,
    )
    .expect("plugin");
}

#[test]
fn a_running_turn_says_how_long_it_has_been_running() {
    // "Working" with no clock is indistinguishable from wedged, which is the moment you reach for
    // `^C` and throw away the answer.
    let sb = Sandbox::new("elapsed");
    install_staller(&sb);
    let mut s = sb.start();
    s.wait_for("staller ready");
    s.wait_for("PROJECTS");

    for c in ["h", "i"] {
        s.key(c);
    }
    s.enter();

    assert!(
        s.pump(|s| {
            s.sidebar_now().iter().any(|l| l.contains("hi"))
                && s.sidebar_virt().iter().any(|t| t.trim_end().ends_with('s'))
        }),
        "a duration appears beside the working conversation\n{:?}\n{:?}",
        s.sidebar_now(),
        s.sidebar_virt()
    );
}

/// A star on a row, meaning the project is a favourite.
///
/// U+2605 rather than a heart, and not because it looks better: `♥` is a codepoint Unicode calls
/// an emoji, so a terminal with a colour-emoji font draws it from there and hands back an outline.
const STAR: char = '\u{2605}';

/// What a conversation's row is indented by, and therefore what tells one from a project's.
///
/// A project sits one column in and a conversation three, which is the same two-column step a
/// worktree takes from the repository it is a tree of.
const CONVERSATION_INDENT: &str = "   ";

/// Project rows carrying the favourite mark.
///
/// Scoped to rows naming a project, because the hint strip also prints a star — that is the point
/// of the hint, and matching it would make these tests pass with the feature ripped out.
fn favourited(panel: &[String], project: &str) -> bool {
    panel.iter().any(|l| l.contains(STAR) && l.contains(project))
}

#[test]
fn favouriting_marks_a_project_and_it_is_still_marked_next_time() {
    let sb = Sandbox::new("fav");
    {
        let mut s = sb.start();
        s.wait_for("PROJECTS");
        s.enter_panel();
        s.key("f");
        assert!(
            s.pump(|s| favourited(&s.sidebar_now(), "work")),
            "the project is marked\n{:?}",
            s.sidebar_now()
        );
        // One list, not two: there is no second heading to go and look under.
        assert_eq!(
            s.sidebar_now().iter().filter(|l| l.trim() == "PROJECTS").count(),
            1,
            "favourites stay in the same list\n{:?}",
            s.sidebar_now()
        );
    }

    // A favourite you have to re-mark every morning is not a favourite.
    let mut s = sb.start();
    assert!(
        s.pump(|s| favourited(&s.sidebar_now(), "work")),
        "still marked after a restart\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn a_favourite_sorts_above_a_project_that_was_used_more_recently() {
    let sb = Sandbox::new("favtop");
    let other = sb.root.join("elsewhere");
    std::fs::create_dir_all(&other).expect("mkdir");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&serde_json::json!({
        "type": "command",
        "name": "project.open",
        "args": [other.display().to_string()],
    })
    .to_string());
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("elsewhere"))));

    // `work` is now the older project. Favourite it and it goes above the newer one anyway.
    s.enter_panel();
    s.down();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("JK move"))),
        "the cursor is on a project\n{:?}",
        s.sidebar_now()
    );
    s.key("f");
    assert!(
        s.pump(|s| {
            let p = s.sidebar_now();
            let w = p.iter().position(|l| l.contains(STAR) && l.contains("work"));
            let e = p.iter().position(|l| l.contains("elsewhere"));
            matches!((w, e), (Some(w), Some(e)) if w < e)
        }),
        "the favourite leads\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn unfavouriting_takes_the_mark_away() {
    let sb = Sandbox::new("unfav");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.enter_panel();
    s.key("f");
    assert!(s.pump(|s| favourited(&s.sidebar_now(), "work")));
    s.key("f");
    assert!(
        s.pump(|s| !favourited(&s.sidebar_now(), "work")),
        "the mark is gone\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn adding_a_project_is_a_row_you_can_see_rather_than_a_key_you_have_to_know() {
    let sb = Sandbox::new("addrow");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("+ Add project"))),
        "the panel offers it\n{:?}",
        s.sidebar_now()
    );

    // And it does something: with no other worktree to offer, it asks for a path.
    s.enter_panel();
    for _ in 0..8 {
        s.down();
        if s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("add project"))) {
            break;
        }
    }
    s.enter();
    s.wait_for("Add project");
}

// ---------------------------------------------------------------------------
// Worktrees
// ---------------------------------------------------------------------------

/// `^N` in a repository asks where the conversation goes, because there is a real answer that is
/// not always "here".
///
/// Outside a repository it does not ask — the question would have one answer — so this also pins
/// down that `session.new` stays a single key everywhere the choice would be theatre.
#[test]
fn a_new_conversation_in_a_repository_offers_a_worktree() {
    let sb = Sandbox::new("newwhere");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("n");
    // Waiting on the picker's own buffer, not on the words in it: "New conversation" is also what
    // an unnamed conversation is called in the panel, so a text match passes before it opens.
    assert!(
        s.pump(|s| !s.picker_named("[New conversation]").is_empty()),
        "the picker opened\n{:?}",
        s.sidebar_now()
    );
    let rows = s.picker_named("[New conversation]");
    assert!(rows.iter().any(|l| l.contains("Here")), "the default\n{rows:?}");
    assert!(rows.iter().any(|l| l.contains("new worktree")), "and the other one\n{rows:?}");
    // The in-project variant is a visible row, not a setting you have to know about: a choice
    // that only exists after editing config.toml is a choice nobody discovers.
    assert!(
        rows.iter().any(|l| l.contains("in this project")),
        "and the one that stays in the repository\n{rows:?}"
    );
}

/// The trees it offers under "an existing one" are the ones on the panel's list, not every
/// checkout `git worktree list` can see.
///
/// A scratch branch you finished with in March is still a worktree on disk long after `X` took it
/// off the panel, so this picker was handing back every project you had ever removed — twenty rows
/// of finished work above the three you are in — and choosing one put it straight into the sidebar
/// again. The list is the workspace's answer to *which places do I work in* (ADR 0039), and this
/// question asks exactly that.
///
/// Two worktrees, alike in every way git can see and differing only in whether anything was ever
/// started in one of them, which is what makes this a statement about the list rather than about
/// worktrees.
#[test]
fn a_worktree_that_is_not_on_the_list_is_not_offered() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("newwherelist");
    sb.git_init();
    let add = |branch: &str| {
        let at = sb.root.join(branch);
        let out = Command::new("git")
            .current_dir(sb.work())
            .args(["worktree", "add", "-b", branch, &at.display().to_string()])
            .output()
            .expect("git runs");
        assert!(out.status.success(), "worktree add: {}", String::from_utf8_lossy(&out.stderr));
        at
    };
    let onlist = add("onlist");
    add("offlist");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    // A conversation started in a directory is how it joins the list. The other tree gets nothing,
    // which is the state `X` leaves behind: on the disk, in git, on nobody's list.
    s.send(&command_with("project.open", &onlist.display().to_string()));
    assert!(s.pump(|s| s.sidebar_rows() >= 2), "it is a project now\n{:?}", s.sidebar_now());
    // Asked from the checkout rather than from inside the new tree — the tree you are standing in
    // is never one of the answers, so asking there could not tell the two rules apart.
    s.send(&command_with("project.open", &sb.work().display().to_string()));
    assert!(s.pump(|s| s.sidebar_rows() >= 3), "and we are back\n{:?}", s.sidebar_now());

    s.ctrl("n");
    assert!(
        s.pump(|s| s.picker_named("[New conversation]").iter().any(|l| l.contains("onlist"))),
        "the tree on the list is a row\n{:?}",
        s.picker_named("[New conversation]")
    );
    let rows = s.picker_named("[New conversation]");
    assert!(rows.iter().any(|l| l.contains("Here")), "the question is still asked\n{rows:?}");
    assert!(
        !rows.iter().any(|l| l.contains("offlist")),
        "and the one nobody kept is not\n{rows:?}"
    );
}

/// The picker's in-project row works with nothing configured: the tree lands in `.worktrees/`
/// and `.gitignore` keeps it out of `git status` — no `worktree.root` edit required.
///
/// The ignore line goes in the *tracked* file, not `.git/info/exclude`, because the exclude file
/// is per clone: a colleague pulling the layout would rediscover the untracked-directory noise
/// this exists to prevent. And it names the worktrees' directory, not the tree — one line that
/// covers every branch after it, in a file everyone shares.
#[test]
fn a_worktree_inside_the_project_needs_no_configuration() {
    let sb = Sandbox::new("wtinsidecmd");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new.inside"));

    let under = sb.work().join(".worktrees");
    assert!(
        s.pump(|_| std::fs::read_dir(&under).is_ok_and(|d| d.count() > 0)),
        "a worktree appeared under {} with nothing asked and nothing configured",
        under.display()
    );
    let gitignore = sb.work().join(".gitignore");
    assert!(
        s.pump(|_| std::fs::read_to_string(&gitignore)
            .is_ok_and(|t| t.lines().any(|l| l.trim() == "/.worktrees/"))),
        "and .gitignore keeps the whole directory out of git status, for every clone\n{:?}",
        std::fs::read_to_string(&gitignore)
    );
}

/// `git.worktree.remove <path>` takes the checkout off the disk without a picker — the sidebar's
/// `d` is a row that already points at one, and a picker opened from it asks you to point twice.
///
/// The removal runs from the main checkout regardless of where the active conversation is,
/// because `git worktree remove` refuses to saw off the branch it is sitting on.
#[test]
fn a_worktree_is_removed_by_its_path_without_a_picker() {
    let sb = Sandbox::new("wtremove");
    sb.git_init();
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new.inside"));
    // Waited for by the notification the verb ends with, not by the directory appearing. Those are
    // the two halves of it: `git worktree add` puts the checkout on disk, and the conversation is
    // created in it afterwards. The refusal asserted below is about the second half — so a test
    // that moved as soon as the directory existed was asking to remove a tree nothing was standing
    // in yet, which is a removal that succeeds.
    assert!(s.pump(|s| s.saw("branched ")), "the tree was made and entered\n{}", s.transcript());
    let under = sb.work().join(".worktrees");
    assert!(
        s.pump(|_| std::fs::read_dir(&under).is_ok_and(|d| d.count() > 0)),
        "the worktree exists to begin with"
    );
    // Canonicalised, because the worktree list reports real paths and `$TMPDIR` on macOS is a
    // symlink — `/var/folders/…` names a directory git will only ever call `/private/var/…`.
    let made = std::fs::read_dir(&under)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .next()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .expect("one worktree");

    // Landing in the new tree is the point of creating one — which means removing it has to be
    // refused right now: the conversation is standing in it.
    s.send(&command_with("git.worktree.remove", &made.display().to_string()));
    assert!(
        s.pump(|s| s.saw("switch to another conversation")),
        "removing the tree you are in is refused with a reason"
    );

    // From the main checkout it goes.
    s.send(&command_with("project.open", &sb.work().display().to_string()));
    s.send(&command_with("git.worktree.remove", &made.display().to_string()));
    assert!(s.pump(|_| !made.exists()), "the checkout is gone from the disk");
}

/// `git.pull` brings the remote's commits into the conversation's checkout, and what git said
/// about it is the answer rather than silence.
#[test]
fn pulling_brings_the_remote_changes_into_the_checkout() {
    let sb = Sandbox::new("gitpull");
    sb.git_init();
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = Command::new("git").current_dir(dir).args(args).output().expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    // A bare "origin" beside the checkout, and a second clone playing the colleague who pushed.
    git(&sb.root, &["clone", "--bare", "--quiet", "work", "origin.git"]);
    git(&sb.work(), &["remote", "add", "origin", "../origin.git"]);
    git(&sb.work(), &["fetch", "--quiet", "origin"]);
    git(&sb.work(), &["branch", "--set-upstream-to=origin/trunk", "trunk"]);
    git(&sb.root, &["clone", "--quiet", "origin.git", "peer"]);
    let peer = sb.root.join("peer");
    git(&peer, &["config", "user.email", "peer@neosh.invalid"]);
    git(&peer, &["config", "user.name", "peer"]);
    git(&peer, &["config", "commit.gpgsign", "false"]);
    std::fs::write(peer.join("news.txt"), "from the remote\n").expect("write");
    git(&peer, &["add", "."]);
    git(&peer, &["commit", "--quiet", "-m", "news"]);
    git(&peer, &["push", "--quiet", "origin", "trunk"]);

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&command("git.pull"));
    assert!(
        s.pump(|_| sb.work().join("news.txt").is_file()),
        "the remote's commit arrived in the working tree"
    );
}

#[test]
fn a_new_conversation_outside_a_repository_does_not_ask() {
    let sb = Sandbox::new("newnoask");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let before = s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count();
    s.ctrl("n");
    assert!(
        s.pump(|s| {
            s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count() > before
        }),
        "it just started one\n{:?}",
        s.sidebar_now()
    );
    assert!(
        s.picker_named("[New conversation]").is_empty(),
        "without asking a question with one answer"
    );
}

/// A worktree goes under `worktree.root`, as `<root>/<repo>/<branch>`, and you land in it.
///
/// The landing is the point. Creating a directory you then have to go and find is not a feature,
/// it is a chore with extra steps — and `^N` offering it only makes sense if choosing it puts you
/// where you asked to be.
#[test]
fn a_worktree_is_created_under_the_configured_root_and_you_land_in_it() {
    let sb = Sandbox::new("wtnew");
    sb.git_init();
    let root = sb.root.join("trees");
    sb.write_config(&format!(
        "[options]\n\"worktree.root\" = \"{}\"\n",
        root.display()
    ));
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new"));
    s.wait_for("Branch for the new worktree");
    s.type_text("feat/thing");
    s.enter();

    // The layout says both halves: the repository, then the branch with its slash flattened,
    // because a directory is a name and not a path.
    let want = format!("{}/work/feat-thing", root.display());
    assert!(
        s.pump(|_| std::path::Path::new(&want).is_dir()),
        "the worktree is on disk at {want}"
    );
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("feat-thing"))),
        "and the conversation is in it\n{:?}",
        s.chat_now()
    );
}

/// A worktree is a row *inside* the repository it belongs to, named by its branch.
///
/// It used to be a top-level project named `work · sideline` — correct in that a worktree is a
/// place conversations live, and wrong as a list: four scratch trees of one repository read as
/// four unrelated projects, and the name carried the relationship precisely because the structure
/// did not. Nested, the repository's name is said once by the row above, and what is left to say
/// about the tree is the branch.
#[test]
fn a_worktree_nests_under_the_repository_it_belongs_to() {
    let sb = Sandbox::new("wtname");
    sb.git_init();
    let root = sb.root.join("trees");
    sb.write_config(&format!("[options]\n\"worktree.root\" = \"{}\"\n", root.display()));
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");
    // The original checkout is just the repository — adding its branch would put a word that
    // changes on every `git switch` beside a name that never does.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("work") && !l.contains('\u{b7}'))),
        "the main checkout is named plainly\n{:?}",
        s.sidebar_now()
    );

    s.send(&command_with("git.worktree.new", "sideline"));
    assert!(
        s.pump(|s| {
            let rows = s.sidebar_now();
            let repo = rows.iter().position(|l| l.contains("work") && !l.contains("sideline"));
            let tree = rows.iter().position(|l| l.contains("sideline"));
            match (repo, tree) {
                // Below its repository, indented past the repository's own arrow, and the branch
                // alone — `work · sideline` is the flat list this replaced.
                (Some(r), Some(t)) => {
                    t > r
                        && rows[t].starts_with(CONVERSATION_INDENT)
                        && !rows[t].contains('\u{b7}')
                }
                _ => false,
            }
        }),
        "the worktree is a nested row named by its branch\n{:?}",
        s.sidebar_now()
    );
}

/// And it is still inside it once you have deleted everything you were doing in there.
///
/// Which tree belongs to which repository is read off a conversation's `repo_root` — so an emptied
/// worktree has nobody left to say it, and a project that outlives its conversations would jump out
/// of the repository it belongs to and land at the top level as a project of its own. A tree does
/// not become a project by being idle: the relationship is written down (`sidebar.root`) while
/// something still knows it.
#[test]
fn an_emptied_worktree_is_still_inside_its_repository() {
    let sb = Sandbox::new("wtempty");
    sb.git_init();
    let root = sb.root.join("trees");
    sb.write_config(&format!(
        "[options]\n\"worktree.root\" = \"{}\"\n\"ui.confirm_destructive\" = false\n",
        root.display()
    ));
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command_with("git.worktree.new", "sideline"));
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("sideline"))),
        "the worktree is there to begin with\n{:?}",
        s.sidebar_now()
    );

    // The conversation the worktree was made for is the active one, so this empties the tree. Wait
    // for it to actually go: every claim below is true of the panel *before* the delete as well, so
    // a test that only asserted the shape would pass without waiting for anything to happen.
    s.send(&command("session.close"));
    assert!(
        s.pump(|s| s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count() == 1),
        "the tree's only conversation is gone\n{:?}",
        s.sidebar_now()
    );
    assert!(
        s.pump(|s| {
            let rows = s.sidebar_now();
            let repo = rows.iter().position(|l| l.contains("work") && !l.contains("sideline"));
            let tree = rows.iter().position(|l| l.contains("sideline"));
            match (repo, tree) {
                (Some(r), Some(t)) => t > r && rows[t].starts_with(CONVERSATION_INDENT),
                _ => false,
            }
        }),
        "the emptied tree is still nested under its repository\n{:?}",
        s.sidebar_now()
    );
}

/// No questions at all. A branch named before the work is a decision made at the worst possible
/// moment, and needing to make it is what stops people reaching for a clean tree in the first
/// place.
///
/// The generated name has to be a *name* — two words you can say out loud and find again in a list
/// of eight of them — which is why this asserts the shape rather than merely that a directory
/// appeared: `wt-3` and a timestamp would both pass "it exists".
#[test]
fn a_worktree_can_be_had_without_answering_anything() {
    let sb = Sandbox::new("wtauto");
    sb.git_init();
    let root = sb.root.join("trees");
    sb.write_config(&format!("[options]\n\"worktree.root\" = \"{}\"\n", root.display()));
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new.auto"));

    let under = root.join("work");
    assert!(
        s.pump(|_| std::fs::read_dir(&under).is_ok_and(|d| d.count() > 0)),
        "a worktree appeared under {} with nothing asked",
        under.display()
    );
    assert!(s.prompt_now().is_empty(), "and no field was opened\n{:?}", s.prompt_now());

    let made = std::fs::read_dir(&under)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .next()
        .expect("one worktree");
    let words: Vec<&str> = made.split('-').collect();
    assert!(
        words.len() >= 2 && words.iter().all(|w| w.len() > 2 && w.chars().all(|c| c.is_ascii_lowercase())),
        "it is words rather than an identifier: {made}"
    );
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains(&made))),
        "and the conversation landed in it\n{:?}",
        s.chat_now()
    );
}

/// `n` in the panel asks the same question `^N` does.
///
/// They used to differ: `^N` opened the picker and `n` created one outright. One letter and a
/// modifier apart, doing visibly different things, which makes "which one did I want" a thing you
/// can only answer by having already been surprised once.
#[test]
fn the_panel_key_for_a_new_conversation_asks_what_the_chat_key_asks() {
    let sb = Sandbox::new("panelnew");
    sb.git_init();
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.enter_panel();
    s.key("n");
    assert!(
        s.pump(|s| !s.picker_named("[New conversation]").is_empty()),
        "the same picker\n{:?}",
        s.sidebar_now()
    );
    let rows = s.picker_named("[New conversation]");
    assert!(rows.iter().any(|l| l.contains("Here")), "with Here first\n{rows:?}");
}

/// One question. The location is a setting; asking again is asking somebody to repeat themselves.
#[test]
fn making_a_worktree_asks_only_for_the_branch() {
    let sb = Sandbox::new("wtoneq");
    sb.git_init();
    let root = sb.root.join("trees");
    sb.write_config(&format!("[options]\n\"worktree.root\" = \"{}\"\n", root.display()));
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new"));
    s.wait_for("Branch for the new worktree");
    s.type_text("quick");
    s.enter();

    let want = format!("{}/work/quick", root.display());
    assert!(
        s.pump(|_| std::path::Path::new(&want).is_dir()),
        "it went straight to {want} without a second prompt"
    );
    assert!(
        s.prompt_now().is_empty() || !s.prompt_now().iter().any(|l| l.contains("Create worktree at")),
        "nothing asked where\n{:?}",
        s.prompt_now()
    );
}

/// An empty root restores the old sibling layout, for anyone who wants their trees beside the
/// thing they are trees of.
#[test]
fn an_empty_worktree_root_puts_them_beside_the_repository() {
    let sb = Sandbox::new("wtsibling");
    sb.git_init();
    sb.write_config("[options]\n\"worktree.root\" = \"\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new"));
    s.wait_for("Branch for the new worktree");
    s.type_text("side");
    s.enter();
    let want = format!("{}/work-worktrees/side", sb.root.display());
    assert!(
        s.pump(|_| std::path::Path::new(&want).is_dir()),
        "the worktree went beside the repository, at {want}"
    );
}

/// A relative root is inside the repository itself — `<repo>/.worktrees/<branch>`, with no
/// `<repo>` level, because inside the repository nothing else's worktrees can collide with yours.
///
/// The ignore entry is the half that makes the layout livable: git happily creates a worktree
/// inside its own repository and then reports the entire checkout as one untracked directory, in
/// every status, forever. It goes in the repository's `.gitignore` — tracked, so every clone gets
/// it — and names the worktrees' directory once rather than growing a line per branch.
#[test]
fn a_relative_worktree_root_lives_inside_the_repository_and_stays_out_of_status() {
    let sb = Sandbox::new("wtinside");
    sb.git_init();
    sb.write_config("[options]\n\"worktree.root\" = \".worktrees\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("PROJECTS");

    s.send(&command("git.worktree.new"));
    s.wait_for("Branch for the new worktree");
    s.type_text("inside");
    s.enter();

    let repo = sb.root.join("work");
    let want = repo.join(".worktrees/inside");
    assert!(
        s.pump(|_| want.is_dir()),
        "the worktree is inside the repository, at {}",
        want.display()
    );
    let gitignore = repo.join(".gitignore");
    assert!(
        s.pump(|_| std::fs::read_to_string(&gitignore)
            .is_ok_and(|t| t.lines().any(|l| l.trim() == "/.worktrees/"))),
        "and .gitignore keeps it out of git status\n{:?}",
        std::fs::read_to_string(&gitignore)
    );
}

#[test]
fn folding_a_project_hides_its_conversations_and_keeps_the_count() {
    let sb = Sandbox::new("fold");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["t", "a", "l", "k"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("talk");

    s.enter_panel();
    // From the conversation the panel lands on, up onto the project it belongs to.
    s.special("up");
    s.key(" ");
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("talk"))),
        "the conversation is folded away\n{:?}",
        s.sidebar_now()
    );

    s.key(" ");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("talk"))),
        "and unfolds again\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn a_second_project_can_be_opened_and_dragged_above_the_first() {
    let sb = Sandbox::new("reorder");
    let other = sb.root.join("elsewhere");
    std::fs::create_dir_all(&other).expect("mkdir");

    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&serde_json::json!({
        "type": "command",
        "name": "project.open",
        "args": [other.display().to_string()],
    })
    .to_string());
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("elsewhere"))),
        "the second project appears\n{:?}",
        s.sidebar_now()
    );

    // Most recently used first, so the new one leads.
    let before = s.sidebar_now();
    let elsewhere = before.iter().position(|l| l.contains("elsewhere")).expect("row");
    let work = before.iter().position(|l| l.contains("work")).expect("row");
    assert!(elsewhere < work, "the newest project leads to begin with\n{before:?}");

    // Onto `work` — one row down from the conversation the panel lands on — then move it up past
    // `elsewhere`. Each step waits: keys reach the plugin thread asynchronously, so a burst races
    // itself and half of it lands in the composer.
    s.enter_panel();
    s.down();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("JK move"))),
        "the cursor is on a project\n{:?}",
        s.sidebar_now()
    );
    s.key("K");
    assert!(
        s.pump(|s| {
            let p = s.sidebar_now();
            let e = p.iter().position(|l| l.contains("elsewhere"));
            let w = p.iter().position(|l| l.contains(" work"));
            matches!((w, e), (Some(w), Some(e)) if w < e)
        }),
        "the project moved above the one that was newer\n{:?}",
        s.sidebar_now()
    );
}

/// A project is a place you work, not a property of the work in it.
///
/// The list used to be derived from where the conversations were, so emptying a project deleted it:
/// you cleared out a month of threads and the directory you had been working in all month went with
/// them, leaving nothing to start the next one from.
#[test]
fn a_project_outlives_the_conversations_in_it() {
    let sb = Sandbox::new("emptyproject");
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let other = sb.root.join("elsewhere");
    std::fs::create_dir_all(&other).expect("mkdir");

    {
        let mut s = sb.start();
        s.wait_for("PROJECTS");
        s.send(&command_with("project.open", &other.display().to_string()));
        assert!(
            s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("elsewhere"))),
            "the second project appears\n{:?}",
            s.sidebar_now()
        );

        // The conversation `project.open` started is the active one, so this is the last thing in
        // that directory going away.
        s.send(&command("session.close"));
        assert!(
            s.pump(|s| {
                let p = s.sidebar_now();
                p.iter().any(|l| l.contains("elsewhere"))
                    && p.iter().any(|l| l.contains("nothing here yet"))
            }),
            "the project is still a row, with nothing in it\n{:?}",
            s.sidebar_now()
        );
    }

    // And still there next time: the list is written down rather than worked out again from
    // whatever happens to be in it.
    let mut s = sb.start();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("elsewhere"))),
        "the project survived a restart\n{:?}",
        s.sidebar_now()
    );
}

/// Which means there has to be a way off the list, and it is the key that already means "for good".
#[test]
fn a_project_is_removed_by_the_key_on_its_heading() {
    let sb = Sandbox::new("removeproject");
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let other = sb.root.join("elsewhere");
    std::fs::create_dir_all(&other).expect("mkdir");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.send(&command_with("project.open", &other.display().to_string()));
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("elsewhere"))));
    s.send(&command("session.close"));
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("nothing here yet"))));

    // The cursor lands on the conversation you are in — the one left in `work` — and the row under
    // it is the empty project's heading.
    s.enter_panel();
    s.down();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("X remove"))),
        "the cursor is on a project, and the strip says the verb\n{:?}",
        s.sidebar_now()
    );

    s.key("X");
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("elsewhere"))),
        "the project is off the list\n{:?}",
        s.sidebar_now()
    );
    // And the one it was sitting next to is untouched.
    assert!(
        s.sidebar_now().iter().any(|l| l.contains("work")),
        "the other project is still here\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn the_key_hints_follow_what_the_cursor_is_on() {
    // A terminal UI that hides its verbs is a terminal UI you use three of.
    let sb = Sandbox::new("hints");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("^T projects"))),
        "unfocused, it says how to get in\n{:?}",
        s.sidebar_now()
    );

    s.enter_panel();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("x archive"))),
        "on a conversation, it offers the conversation verbs\n{:?}",
        s.sidebar_now()
    );

    s.special("up");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("JK move"))),
        "on a project, it offers the project verbs\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn the_hint_strip_can_be_switched_off() {
    let sb = Sandbox::new("nohints");
    sb.write_config("[options]\n\"sidebar.hints\" = false\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.drain_for(Duration::from_secs(1));
    assert!(
        !s.sidebar_now().iter().any(|l| l.contains("^T projects")),
        "no hint strip\n{:?}",
        s.sidebar_now()
    );
}

/// The float a widget opened, folded the way a frontend folds splices.
impl Session {
    fn float_named(&self, prefix: &str) -> Vec<String> {
        let name = self.events.iter().find_map(|e| {
            let n = e["name"].as_str()?;
            (e["type"] == "buffer_opened" && n.starts_with(prefix)).then(|| n.to_string())
        });
        let Some(name) = name else { return Vec::new() };
        let Some(buf) = self.buffer_named(&name) else { return Vec::new() };
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
}

impl Session {
    /// Where the terminal caret was last put in a window.
    fn caret_in(&self, win: u64) -> Option<(u64, u64)> {
        self.events
            .iter()
            .filter(|e| e["type"] == "cursor_moved" && e["win"].as_u64() == Some(win))
            .filter_map(|e| Some((e["row"].as_u64()?, e["col"].as_u64()?)))
            .next_back()
    }

    fn window_showing(&self, buffer_prefix: &str) -> Option<u64> {
        let name = self.events.iter().find_map(|e| {
            let n = e["name"].as_str()?;
            (e["type"] == "buffer_opened" && n.starts_with(buffer_prefix)).then(|| n.to_string())
        })?;
        let buf = self.buffer_named(&name)?;
        self.windows_for(buf).first().copied()
    }
}

/// Start a conversation called `keep`, then archive it from the panel.
///
/// Shared by both archive tests: getting there is six keystrokes of setup and neither test is about
/// the setup.
fn archive_one(s: &mut Session) {
    for c in ["k", "e", "e", "p"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("keep");
    s.new_conversation(); // a second conversation, so the first is not the only one left

    s.enter_panel();
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("keep"))));
    s.down();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("x archive"))),
        "the cursor is on a conversation\n{:?}",
        s.sidebar_now()
    );
    s.key("x");
}

#[test]
fn archiving_takes_a_conversation_out_of_the_panel_altogether() {
    // The everyday verb, and it asks nothing, because there is nothing to lose. What it leaves
    // behind is one row saying how many — not a section of dim rows in the column you work in.
    let sb = Sandbox::new("archive");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    let panel_only = s.open_windows().len();
    archive_one(&mut s);
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("Archived"))),
        "a door to the archive appeared\n{:?}",
        s.sidebar_now()
    );
    assert_eq!(s.open_windows().len(), panel_only, "and nothing asked");

    // Not folded away, not dimmed at the foot: gone. Every row of the panel is drained first, so
    // this is about what the panel says *now* rather than what it ever said.
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("keep"))),
        "and it left the panel entirely\n{:?}",
        s.sidebar_now()
    );
    // Including with everything unfolded — there is no state of this panel that shows it.
    s.key("j");
    s.key(" ");
    s.drain_for(Duration::from_millis(400));
    assert!(
        !s.sidebar_now().iter().any(|l| l.contains("keep")),
        "no fold brings it back\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn the_archive_is_a_place_you_go_and_come_back_from() {
    // `a` in the panel — and `^F` from anywhere — opens what you have put away, as a list you can
    // filter. `^U` on a row puts it back without switching to it, because tidying and going
    // somewhere are different things.
    let sb = Sandbox::new("browse");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    archive_one(&mut s);
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("Archived"))));

    s.key("a");
    assert!(
        s.pump(|s| s.picker_named("[Archived conversations]").iter().any(|l| l.contains("keep"))),
        "the archive opened with it in\n{:?}",
        s.picker_named("[Archived conversations]")
    );

    s.ctrl("u");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("keep"))),
        "`^U` put it back in the list\n{:?}",
        s.sidebar_now()
    );
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("Archived"))),
        "and the door went with the last thing behind it\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn deleting_a_conversation_with_something_in_it_asks_first() {
    // Deleting removes the file from disk. There is no undo, so this is the one verb in the panel
    // that stops and asks — and it is shifted, so it is not the one your fingers reach for.
    let sb = Sandbox::new("confirmdel");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["k", "e", "e", "p"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("keep");
    s.new_conversation(); // a second conversation, so deleting the first is even allowed

    s.enter_panel();
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("keep"))));
    s.down();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("X delete"))),
        "the cursor is on a conversation\n{:?}",
        s.sidebar_now()
    );
    let panel_only = s.open_windows().len();
    s.key("X");
    assert!(s.pump(|s| s.open_windows().len() > panel_only), "the dialog opened");

    // What is at stake, in the dialog, rather than a bare "are you sure?" you learn to clear.
    assert!(
        s.pump(|s| s.saw("messages, in")),
        "it says what there is to lose\n{}",
        s.transcript()
    );
    // And the answer that cannot be undone is not drawn like the one beside it.
    let dialog = s.buffer_named("[confirm]").expect("the dialog buffer");
    assert!(
        s.groups_of(dialog).iter().any(|row| row.iter().any(|g| g == "Diagnostic.Error")),
        "the destructive answer is coloured as one\n{:?}",
        s.groups_of(dialog)
    );
    // The row under the cursor is banded rather than repainted, which is what leaves the colour
    // above intact. A ranged group across the same bytes would take it.
    assert!(
        s.wire().contains(r#""line_hl_group":"Picker.Selected""#),
        "the cursor row is a band under the text, not a group over it"
    );

    // The cursor starts on the answer that changes nothing: `<CR>` is reflex by the second time you
    // have seen a dialog, and a reflex must not delete anything.
    s.enter();
    assert!(s.pump(|s| s.open_windows().len() == panel_only), "and it closed again");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("keep"))),
        "accepting the default kept it\n{:?}",
        s.sidebar_now()
    );

    // And saying yes really does delete it, so the dialog is a question rather than a speed bump.
    s.key("X");
    assert!(s.pump(|s| s.open_windows().len() > panel_only), "asked a second time");
    s.special("up");
    s.enter();
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("keep"))),
        "choosing Delete removed it\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn deleting_asks_even_when_there_is_nothing_in_it_to_lose() {
    // It used to go straight through for an empty conversation, on the grounds that a dialog for
    // something with nothing to lose is friction. What that actually bought was a key whose
    // behaviour depended on state you cannot see from the row it is pointed at — so the one time it
    // did stop and ask was the one time your fingers were already through it.
    let sb = Sandbox::new("nodialog");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("n");
    assert!(s.pump(|s| {
        s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count() == 2
    }));

    s.enter_panel();
    let panel_only = s.open_windows().len();
    s.key("X");
    assert!(s.pump(|s| s.open_windows().len() > panel_only), "it asked anyway");
    assert!(
        s.pump(|s| s.saw("Nothing has been said in it yet")),
        "and said what there was to lose\n{}",
        s.transcript()
    );

    // `y` answers it outright, which is the point of a dialog with two answers and no filter.
    s.key("y");
    assert!(
        s.pump(|s| {
            s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count() == 1
        }),
        "answering yes deleted it\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn confirmation_can_be_switched_off() {
    let sb = Sandbox::new("noconfirm");
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["g", "o", "n", "e"] {
        s.key(c);
    }
    s.enter();
    s.wait_for("gone");
    s.new_conversation();

    s.enter_panel();
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("gone"))));
    s.down();
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("X delete"))));
    s.key("X");
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("gone"))),
        "deleted without asking\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn the_caret_follows_what_you_type_into_a_picker() {
    // Left at the origin the caret sits on the title, and the field reads as inert: you type, the
    // text appears, and the cursor is somewhere else entirely — so it looks like nothing happened.
    let sb = Sandbox::new("caret");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.ctrl("p");
    s.wait_for("Model");
    let win = s.window_showing("[Model]").expect("the picker window");
    for c in ["m", "o"] {
        s.key(c);
    }
    assert!(
        s.pump(|s| s.caret_in(win).is_some_and(|(row, col)| row == 1 && col == 4)),
        "the caret is at the end of `> mo`, got {:?}",
        s.caret_in(win)
    );
    s.special("esc");
}

#[test]
fn typing_a_path_offers_the_directories_that_match_it() {
    // A path field with no completion is a path field you type wrong. The list under it is the
    // directories matching what you have typed, refreshed on every keystroke.
    let sb = Sandbox::new("pathpick");
    for name in ["alpha", "beta"] {
        std::fs::create_dir_all(sb.work().join(name)).expect("mkdir");
    }
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("project.open"));
    // The float, not the sidebar's `+ Add project` row — which is on screen from the start and
    // would let the test type into the composer instead.
    assert!(s.pump(|s| !s.float_named("[Add project").is_empty()), "the field opened");

    for c in sb.work().display().to_string().chars() {
        s.key(&c.to_string());
    }
    s.key("/");

    assert!(
        s.pump(|s| {
            let f = s.float_named("[Add project");
            f.iter().any(|l| l.ends_with("alpha/")) && f.iter().any(|l| l.ends_with("beta/"))
        }),
        "both directories are offered\n{:?}",
        s.float_named("[Add project")
    );
}

#[test]
fn tab_takes_the_highlighted_completion_into_the_field() {
    let sb = Sandbox::new("pathtab");
    std::fs::create_dir_all(sb.work().join("alpha")).expect("mkdir");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("project.open"));
    // The float, not the sidebar's `+ Add project` row — which is on screen from the start and
    // would let the test type into the composer instead.
    assert!(s.pump(|s| !s.float_named("[Add project").is_empty()), "the field opened");
    for c in sb.work().display().to_string().chars() {
        s.key(&c.to_string());
    }
    s.key("/");
    assert!(s.pump(|s| s.float_named("[Add project").iter().any(|l| l.ends_with("alpha/"))));

    s.special("tab");
    assert!(
        s.pump(|s| {
            s.float_named("[Add project").iter().any(|l| l.starts_with("> ") && l.ends_with("alpha/"))
        }),
        "the field now holds the completed path\n{:?}",
        s.float_named("[Add project")
    );
}

#[test]
fn a_path_you_typed_in_full_is_accepted_even_though_it_was_never_offered() {
    // A completion list that refuses a path it did not think of is a list that gets in the way.
    // The last segment is typed but not completed, so no row matches it.
    let sb = Sandbox::new("pathfree");
    let target = sb.work().join("chosen");
    std::fs::create_dir_all(&target).expect("mkdir");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command("project.open"));
    // The float, not the sidebar's `+ Add project` row — which is on screen from the start and
    // would let the test type into the composer instead.
    assert!(s.pump(|s| !s.float_named("[Add project").is_empty()), "the field opened");
    for c in target.display().to_string().chars() {
        s.key(&c.to_string());
    }
    s.enter();

    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("chosen"))),
        "the project was added\n{:?}",
        s.sidebar_now()
    );
}

#[test]
fn the_key_list_opens_from_the_panel_and_any_key_dismisses_it() {
    let sb = Sandbox::new("keylist");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.enter_panel();
    s.key("?");
    // The panel's section is named after its buffer kind and is generated from the live registry —
    // it used to be a hand-written list in the palette, which is exactly the drift this replaced.
    // It comes first because `?` was pressed *in* the panel.
    s.wait_for("neosh.sidebar");
    let keys = s.buffer_named("[keys]").expect("key list buffer");
    assert!(
        s.pump(|s| {
            let lines = s.lines_of(keys);
            let first = lines.iter().position(|l| l.contains("neosh.sidebar"));
            let chat = lines.iter().position(|l| l.trim() == "CHAT");
            matches!((first, chat), (Some(f), Some(c)) if f < c)
        }),
        "the panel you are standing in is listed before the global bindings\n{:?}",
        s.lines_of(keys)
    );
    assert!(
        s.lines_of(keys).iter().any(|l| l.contains("Archive this conversation")),
        "and its verbs are there, described by the command that owns them\n{:?}",
        s.lines_of(keys)
    );
    // The text is written before the float is opened, so seeing the text is not seeing the window.
    assert!(s.pump(|s| s.windows_for(keys).len() == 1), "it is on screen\n{}", s.transcript());

    s.key("z");
    assert!(s.pump(|s| s.windows_for(keys).is_empty()), "any key closes it\n{}", s.transcript());
}

/// The key list is reachable from the composer, and by a chord rather than by `F1`. Apple's top
/// row is brightness until somebody finds the setting, so the key whose job is teaching every
/// other key was the one key a new Mac could not press. See ADR 0048.
#[test]
fn the_key_list_opens_on_a_chord_from_the_composer() {
    let sb = Sandbox::new("keylistchord");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.ctrl("z");
    s.wait_for("CHAT");
    let keys = s.buffer_named("[keys]").expect("key list buffer");
    assert!(
        s.pump(|s| s.lines_of(keys).iter().any(|l| l.contains("Command palette"))),
        "the live registry, not a written-down list\n{:?}",
        s.lines_of(keys)
    );
    assert!(s.pump(|s| s.windows_for(keys).len() == 1), "it is on screen\n{}", s.transcript());
}

#[test]
fn escape_leaves_the_thread_list_and_typing_goes_back_to_the_composer() {
    let sb = Sandbox::new("leave");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.enter_panel();
    s.special("esc");
    // Leaving is a round trip to the plugin thread as well; the hint strip going back to its
    // out-of-panel form is the signal that the keyboard has been handed back.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("^T projects"))),
        "the panel gave the keyboard back\n{:?}",
        s.sidebar_now()
    );
    // A key that would move the list instead lands in the composer.
    s.key("z");
    assert!(
        s.pump(|s| s.composer_now().iter().any(|l| l.contains('z'))),
        "typing resumed in the composer\n{:?}",
        s.composer_now()
    );
}

impl Session {
    /// The composer buffer's current contents.
    fn composer_now(&self) -> Vec<String> {
        let buf = match self.buffer_named("[composer]") {
            Some(b) => b,
            None => return Vec::new(),
        };
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

    fn chat_now(&self) -> Vec<String> {
        let buf = self.events.iter().find_map(|e| {
            (e["type"] == "buffer_opened"
                && e["name"].as_str().is_some_and(|n| n.starts_with("[chat]")))
            .then(|| e["buf"].as_u64())?
        });
        let Some(buf) = buf else { return Vec::new() };
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
}

#[test]
fn motion_can_be_turned_off_without_turning_off_the_sidebar() {
    let sb = Sandbox::new("nomotion");
    sb.write_config("[options]\n\"ui.motion\" = false\n\"ui.ascii_only\" = true\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.drain_for(Duration::from_secs(2));
    // No braille anywhere: with ascii_only the glyph set is plain, and with motion off nothing
    // cycles at all.
    let panel = s.sidebar_now().join("\n");
    assert!(
        !panel.contains('⠋') && !panel.contains('⠙'),
        "no spinner frames with motion off\n{panel}"
    );
    assert!(
        !panel.contains('\u{25be}') && !panel.contains('\u{25b8}'),
        "ascii_only means the fold arrows are ascii too\n{panel}"
    );
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

impl Session {
    /// Highlight groups the host has announced, by name.
    fn highlight_groups(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| e["type"] == "highlight_defined")
            .filter_map(|e| e["name"].as_str().map(str::to_string))
            .collect()
    }

    /// The most recent definition of a group.
    fn highlight(&self, name: &str) -> Option<&Value> {
        self.events
            .iter()
            .rev()
            .find(|e| e["type"] == "highlight_defined" && e["name"] == name)
            .map(|e| &e["def"])
    }
}

#[test]
fn every_group_a_widget_links_to_is_announced() {
    // The bug this guards: `Picker.Selected` used to link to `CursorLine`, which nothing defined,
    // so the chain dead-ended and the selected row in every picker rendered plain. A dead link does
    // not error — it just quietly has no colour.
    let sb = Sandbox::new("palette");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let groups = s.highlight_groups();
    for required in [
        "Normal",
        "Comment",
        "Title",
        "CursorLine",
        "Search",
        "Picker.Selected",
        "Picker.Match",
        "Sidebar.Heading",
        "Status.Working",
        "Meter.Fill",
        "Git.Added",
        "Diff.Add",
        "Agent.Thinking",
    ] {
        assert!(groups.iter().any(|g| g == required), "{required} was never defined; have {groups:?}");
    }
}

#[test]
fn the_selected_row_of_a_picker_has_a_visible_highlight() {
    let sb = Sandbox::new("selection");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("p");
    s.wait_for("Mock");

    // The mark is emitted with the text, so a row carrying `Picker.Selected` proves both that the
    // widget applied it and that the group exists to be applied.
    let marked = s.events.iter().any(|e| {
        e["type"] == "buffer_lines"
            && e["lines"].as_array().is_some_and(|lines| {
                lines.iter().any(|l| {
                    l["marks"].as_array().is_some_and(|marks| {
                        marks.iter().any(|m| m["hl_group"] == "Picker.Selected")
                    })
                })
            })
    });
    assert!(marked, "the cursor row is highlighted\n{}", s.transcript());
    assert!(
        s.highlight("Picker.Selected").is_some(),
        "and the group resolves rather than dead-ending"
    );
}

#[test]
fn switching_theme_re_announces_the_groups() {
    // Highlights are pushed, not pulled: a frontend holds a copy and cannot ask for a new one, so a
    // theme switch that did not re-emit would change nothing on screen.
    let sb = Sandbox::new("theme");
    sb.write_config("[options]\n\"ui.theme\" = \"light\"\n");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    let dark = Sandbox::new("theme-dark");
    let mut d = dark.start();
    d.wait_for("PROJECTS");

    assert_ne!(
        s.highlight("Comment"),
        d.highlight("Comment"),
        "the light theme is not the dark one under another name"
    );
}

#[test]
fn an_unknown_theme_is_rejected_by_the_option_registry() {
    let sb = Sandbox::new("badtheme");
    sb.write_config("[options]\n\"ui.theme\" = \"solarized\"\n");
    let mut s = sb.start();
    // Typed options make this an error naming the problem, not a silent fallback.
    s.wait_for("ui.theme");
    assert!(s.saw("solarized"), "the message names the value\n{}", s.transcript());
}

// ---------------------------------------------------------------------------
// Diffs
// ---------------------------------------------------------------------------

#[test]
fn the_diff_view_shows_a_files_patch() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("diff");
    sb.git_init();
    std::fs::write(sb.work().join("README.md"), "hello\nsecond line\n").expect("write");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("d");
    s.wait_for("Changed files");
    s.enter();
    s.wait_for("+second line");
    assert!(s.saw("── unstaged ──"), "labelled by which half it came from\n{}", s.transcript());
}

#[test]
fn an_untracked_file_says_so_rather_than_showing_an_empty_diff() {
    if !have_git() {
        return;
    }
    // A blank pane would look like a bug. There is genuinely nothing to diff against.
    let sb = Sandbox::new("untracked");
    sb.git_init();
    std::fs::write(sb.work().join("brand-new.txt"), "x\n").expect("write");

    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("d");
    // The picker's title, not the filename: the sidebar already lists the file, so waiting on that
    // would send Enter into the composer before anything opened.
    s.wait_for("Changed files");
    s.enter();
    s.wait_for("is untracked");
}

// ---------------------------------------------------------------------------
// Entering an API key
// ---------------------------------------------------------------------------

/// The literal a test types in. Distinctive so a search for it means something.
const FAKE_KEY: &str = "sk-neosh-test-DO-NOT-LOG-8f3a";

/// A configured instance that wants a key.
///
/// The mock provider needs none, which is exactly right for every other test here and no use at
/// all for this one. The variable is never set, so this instance starts out with nothing.
const NEEDS_A_KEY: &str = r#"
[[providers]]
id = "acme"
driver = "openai-compat"
display_name = "Acme"
base_url = "https://acme.invalid/v1"
auth = { kind = "env", var = "NEOSH_TEST_ACME_KEY_NEVER_SET" }
"#;

#[test]
fn a_key_typed_in_never_reaches_the_frontend() {
    // The load-bearing test for the whole credential path. The host reads these keystrokes itself
    // precisely so the value never enters a buffer — because a buffer is drawn, and drawing it
    // would put the key in a `UiEvent`, across a process boundary, into whatever the frontend logs.
    let sb = Sandbox::new("secretwire");
    sb.write_config(NEEDS_A_KEY);
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    s.send(&command_with("provider.key", "acme"));
    s.wait_for("Paste or type the key");

    for c in FAKE_KEY.chars() {
        s.key(&c.to_string());
    }
    assert!(
        s.pump(|s| s.saw("\u{2022}\u{2022}\u{2022}")),
        "the field shows bullets, so a paste that did not arrive is visible\n{}",
        s.transcript()
    );
    s.enter();
    // Something happened — either it was stored, or the keychain was unavailable and it is held for
    // the run. Both are fine here; what is not fine is the value being anywhere in the JSON.
    assert!(s.pump(|s| s.saw("key saved") || s.saw("kept for this run")), "{}", s.transcript());

    assert!(
        !s.wire().contains(FAKE_KEY),
        "the key reached the frontend, which is the one thing this design exists to prevent"
    );
    assert!(!s.wire().contains("sk-neosh-test"), "not even a prefix of it");
}

#[test]
fn the_key_prompt_gives_the_composer_back_with_the_draft_still_in_it() {
    // It borrows the composer. Borrowing is only acceptable if it gives it back — losing an unsent
    // question because you stopped to paste a key would be its own reason never to stop.
    let sb = Sandbox::new("secretdraft");
    sb.write_config(NEEDS_A_KEY);
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    for c in ["h", "a", "l", "f"] {
        s.key(c);
    }
    assert!(s.pump(|s| s.saw("half")), "{}", s.transcript());

    s.send(&command_with("provider.key", "acme"));
    s.wait_for("Paste or type the key");
    for c in ["a", "b", "c"] {
        s.key(c);
    }
    s.send(r#"{"type":"key","key":{"code":{"kind":"esc"},"mods":{}}}"#);

    assert!(
        s.pump(|s| s.texts().iter().rev().take(4).any(|l| l == "half")),
        "the draft came back\n{}",
        s.transcript()
    );
    assert!(!s.wire().contains("abc"), "and what was typed into the prompt went nowhere");
}

#[test]
fn while_a_key_is_being_typed_no_binding_fires() {
    // Every base64 key contains an `n`, and `<C-n>` is a new conversation. Routing these keys
    // through the keymaps would mean pasting a key sometimes started a conversation halfway
    // through — which is also how half a key ends up stored.
    let sb = Sandbox::new("secretkeys");
    sb.write_config(NEEDS_A_KEY);
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    let before = s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count();

    s.send(&command_with("provider.key", "acme"));
    s.wait_for("Paste or type the key");
    s.ctrl("n");
    s.ctrl("t");
    s.key("n");
    assert!(s.pump(|s| s.saw("\u{2022}")), "the `n` went into the key\n{}", s.transcript());

    s.send(r#"{"type":"key","key":{"code":{"kind":"esc"},"mods":{}}}"#);
    s.drain_for(Duration::from_millis(200));
    assert_eq!(
        s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count(),
        before,
        "no conversation was created\n{:?}",
        s.sidebar_now()
    );
}

// ---------------------------------------------------------------------------
// The transcript
// ---------------------------------------------------------------------------

/// A driver that starts a turn and never finishes it, so the working line is reliably on screen.
const STALLS: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("stall", [{
    id: "stall", driver: "stall", display_name: "Stall",
    models: [{ id: "stall", display_name: "Stall" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "stall", usage: {} });
    // Nothing else, ever. The turn stays in flight.
  });
  neosh.event.on("neosh.ready", () => neosh.notify("stall ready"));
}
"#;

/// A provider that reports it is waiting and then never finishes — the shape of a vendor CLI sitting
/// in retry backoff after a 529.
///
/// The reason this needs a test at all is that the failure mode is *nothing on screen*: several
/// minutes with no tokens, no card and no card body, which is indistinguishable from a wedged
/// process and is what sends people to `^C` on a turn that was about to come back.
const WAITER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("waiter", [{
    id: "waiter", driver: "waiter", display_name: "Waiter",
    models: [{ id: "waiter", display_name: "Waiter" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "waiter", usage: {} });
    emit({ type: "activity", activity: {
      kind: "waiting",
      what: "Retrying after 529 · attempt 2 of 5",
      retry_in_ms: 4200,
    } });
    // And then nothing. The wait is the whole turn.
  });
  neosh.event.on("neosh.ready", () => neosh.notify("waiter ready"));
}
"#;

fn install_waiter(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/waiter");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"waiter\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), WAITER).expect("plugin");
}

#[test]
fn a_turn_that_is_waiting_says_what_it_is_waiting_for() {
    // "Working… 4m" and "Retrying after 529 · attempt 2 of 5 · in 5s … 4m" are the same turn. Only
    // one of them is a reason to keep waiting rather than to interrupt. See ADR 0054.
    let sb = Sandbox::new("waiting");
    install_waiter(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"waiter/waiter\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("waiter ready");

    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Retrying after 529")
            && l.contains("attempt 2 of 5")
            && l.contains("esc to interrupt"))),
        "the working line says why nothing is happening\n{:?}",
        s.chat_now()
    );
    // Rounded up rather than truncated: a wait reported as `in 0s` reads as a lie.
    assert!(
        s.chat_now().iter().any(|l| l.contains("in 5s")),
        "and how long it expects to be\n{:?}",
        s.chat_now()
    );
}

/// A provider that backgrounds something, answers, and ends — the shape of `run_in_background`.
const LEAVER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("leaver", [{
    id: "leaver", driver: "leaver", display_name: "Leaver",
    models: [{ id: "leaver", display_name: "Leaver" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "leaver", usage: {} });
    emit({ type: "activity", activity: { kind: "background", tasks: [
      { id: "bg1", title: "npm run build", kind: "local_bash" },
    ] } });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Started the build." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("leaver ready"));
}
"#;

fn install_leaver(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/leaver");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"leaver\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), LEAVER).expect("plugin");
}

#[test]
fn a_finished_turn_says_what_it_walked_away_from() {
    // The turn ends and the footer listing what was out goes away with the working line. Without
    // this the one case people actually ask about — "it says it is done, is it?" — is the one that
    // leaves no trace at all. See ADR 0054.
    let sb = Sandbox::new("leftrunning");
    install_leaver(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"leaver/leaver\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("leaver ready");

    s.type_text("build it");
    s.enter();
    s.wait_for("Started the build.");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Still running in the background"))),
        "the answer does not get to be the last word\n{:?}",
        s.chat_now()
    );
    assert!(
        s.chat_now().iter().any(|l| l.contains("npm run build") && l.contains("local_bash")),
        "and it says what, in the driver's own words for the kind of thing it is\n{:?}",
        s.chat_now()
    );
    // The conversation is what carries this, not the turn — so the row in the panel knows too, and
    // knows it after the turn that started it is over.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("\u{25cb} build it"))),
        "the panel marks a conversation that is still doing something\n{:?}",
        s.sidebar_now()
    );
}

/// A provider that says something and then never finishes — the shape of a real turn that has
/// written a sentence and gone off to run tools.
const HALFWAY: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("half", [{
    id: "half", driver: "half", display_name: "Half",
    models: [{ id: "half", display_name: "Half" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "half", usage: {} });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Here is what I found so far." });
    // And then nothing, ever. The turn stays in flight with an answer already on screen.
  });
  neosh.event.on("neosh.ready", () => neosh.notify("half ready"));
}
"#;

/// A tool that succeeds with several lines, and a model that calls it once.
///
/// The built-in fixture's tools all fail, and a failure is the one case that ignores the limit —
/// so proving the limit does anything needs a call that works.
const CHATTY: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.tool.register(
    { name: "count", description: "Count", inputSchema: { type: "object" } },
    async () => ({ content: "one\ntwo\nthree\nfour\nfive" }),
  );
  let asked = false;
  await neosh.provider.register("chatty", [{
    id: "chatty", driver: "chatty", display_name: "Chatty",
    models: [{ id: "chatty", display_name: "Chatty" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "chatty", usage: {} });
    if (!asked) {
      asked = true;
      emit({ type: "block_start", index: 0, block: { kind: "tool_use", id: "c1", name: "count" } });
      emit({ type: "tool_input_delta", index: 0, partial_json: "{}" });
      emit({ type: "block_stop", index: 0 });
      emit({ type: "message_delta", stop_reason: { kind: "tool_use" }, usage: {} });
      emit({ type: "message_stop" });
      return;
    }
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Counted." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("chatty ready"));
}
"#;

/// A tool whose call is an edit — `old_string`/`new_string` at a path — and a model that makes one.
///
/// What the card shows for an edit is read off those arguments, so this is the shape that proves
/// the diff, the `+N -N`, and the summary at the end of the turn.
const EDITOR: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.tool.register(
    { name: "edit", description: "Edit", inputSchema: { type: "object" } },
    async () => ({ content: "edited" }),
  );
  let asked = false;
  await neosh.provider.register("editor", [{
    id: "editor", driver: "editor", display_name: "Editor",
    models: [{ id: "editor", display_name: "Editor" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "editor", usage: {} });
    if (!asked) {
      asked = true;
      const input = {
        file_path: "/work/src/main.rs",
        old_string: "a\nb\nc\nd\ne\nf\ng\nh",
        new_string: "a\nb\nc\nd\nE\nf\ng\nh\ni",
      };
      emit({ type: "block_start", index: 0, block: { kind: "tool_use", id: "e1", name: "edit" } });
      emit({ type: "tool_input_delta", index: 0, partial_json: JSON.stringify(input) });
      emit({ type: "block_stop", index: 0 });
      emit({ type: "message_delta", stop_reason: { kind: "tool_use" }, usage: {} });
      emit({ type: "message_stop" });
      return;
    }
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Edited." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("editor ready"));
}
"#;

const LOOKER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  const lines = (n: number) => Array.from({ length: n }, (_, i) => `line ${i + 1}`).join("\n");
  await neosh.tool.register(
    { name: "look", description: "Read", inputSchema: { type: "object" } },
    async () => ({ content: lines(9) }),
  );
  await neosh.tool.register(
    { name: "hunt", description: "Grep", inputSchema: { type: "object" } },
    async () => ({ content: lines(4) }),
  );
  await neosh.tool.register(
    { name: "edit", description: "Edit", inputSchema: { type: "object" } },
    async () => ({ content: "edited" }),
  );
  let turn = 0;
  await neosh.provider.register("looker", [{
    id: "looker", driver: "looker", display_name: "Looker",
    models: [{ id: "looker", display_name: "Looker" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "looker", usage: {} });
    const call = (i: number, name: string, input: unknown) => {
      emit({ type: "block_start", index: i, block: { kind: "tool_use", id: `t${turn}_${i}`, name } });
      emit({ type: "tool_input_delta", index: i, partial_json: JSON.stringify(input) });
      emit({ type: "block_stop", index: i });
    };
    turn += 1;
    if (turn === 1) {
      call(0, "look", { file_path: "/work/src/one.rs" });
      call(1, "look", { file_path: "/work/src/two.rs" });
      call(2, "hunt", { pattern: "fn main", path: "/work/src" });
      call(3, "edit", { file_path: "/work/src/one.rs", old_string: "a", new_string: "b" });
      emit({ type: "message_delta", stop_reason: { kind: "tool_use" }, usage: {} });
      emit({ type: "message_stop" });
      return;
    }
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Looked." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("looker ready"));
}
"#;

fn install_looker(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/looker");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"looker\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\", \"tools\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), LOOKER).expect("plugin");
}

fn install_editor(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/editor");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"editor\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\", \"tools\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), EDITOR).expect("plugin");
}

fn install_chatty(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/chatty");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"chatty\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\", \"tools\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), CHATTY).expect("plugin");
}

fn install_halfway(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/half");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"half\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), HALFWAY).expect("plugin");
}

fn install_stall(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/stall");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"stall\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), STALLS).expect("plugin");
}

#[test]
fn a_question_is_framed_so_you_can_find_where_a_turn_began() {
    // A bar down the left rather than a `>` prefix: the frontend wraps long lines, a prefix
    // survives only on the first of them, and what you scan for when scrolling back is exactly
    // where the turns start.
    let sb = Sandbox::new("framing");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    for c in ["h", "i"] {
        s.key(c);
    }
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l == "\u{258c} hi")),
        "the question is framed\n{:?}",
        s.chat_now()
    );
}

#[test]
fn a_tool_call_says_what_it_is_about_not_only_its_name() {
    // The tool's name tells you nothing you did not already know. `Read  .env` is the line you
    // actually wanted, and both halves of it cost one lookup in the input the tool was given.
    let sb = Sandbox::new("toolrow");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.key("h");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Read") && l.contains(".env"))),
        "the subject is beside what was done\n{:?}",
        s.chat_now()
    );
}

#[test]
fn a_turn_in_flight_says_so_and_says_for_how_long() {
    // The gap between pressing Enter and the first token is the longest silence in the program,
    // and a still screen and a wedged process look identical. The clock is the difference.
    let sb = Sandbox::new("working");
    install_stall(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"stall/stall\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("stall ready");

    for c in ["h", "a", "n", "g"] {
        s.key(c);
    }
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains('\u{2026}') && l.contains("esc to interrupt"))),
        "the working line is up\n{:?}",
        s.chat_now()
    );
    // And it is the last line: it says what is happening *now*, so anything already reported
    // belongs above it.
    let rows = s.chat_now();
    let last = rows.iter().rposition(|l| !l.trim().is_empty()).unwrap_or(0);
    assert!(rows[last].contains("esc to interrupt"), "pinned to the bottom\n{rows:?}");
}

#[test]
fn the_working_line_is_gone_the_moment_there_is_an_answer() {
    // Leaving it above the reply would say a turn is in flight while you are reading its result.
    let sb = Sandbox::new("workingends");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.key("h");
    s.enter();
    s.wait_for("All done.");
    s.drain_for(Duration::from_millis(300));
    assert!(
        !s.chat_now().iter().any(|l| l.contains("esc to interrupt")),
        "no working line survives the turn\n{:?}",
        s.chat_now()
    );
}


// ---------------------------------------------------------------------------
// The composer: what it says, and what the keys under it do
// ---------------------------------------------------------------------------

impl Session {
    /// Everything drawn *around* the composer that is not in the composer: the prompt, the
    /// placeholder, the rule, the shortcut row. All of it is virtual text, so none of it can be
    /// read back out of the buffer — which is the point, since the buffer's contents get sent.
    fn composer_chrome(&self) -> Vec<String> {
        let buf = self
            .events
            .iter()
            .find_map(|e| {
                (e["type"] == "buffer_opened" && e["name"] == "[composer]")
                    .then(|| e["buf"].as_u64())?
            })
            .unwrap_or(u64::MAX);
        let mut latest: Vec<String> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let mut found = Vec::new();
            for line in e["lines"].as_array().into_iter().flatten() {
                for m in line["marks"].as_array().into_iter().flatten() {
                    let text: String = m["virt_text"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c["text"].as_str())
                        .collect();
                    if !text.trim().is_empty() {
                        found.push(text);
                    }
                }
            }
            if !found.is_empty() {
                latest = found;
            }
        }
        latest
    }
}

#[test]
fn the_shortcut_row_is_off_because_the_sidebar_already_says_all_of_it() {
    // It read as a good idea and was not. `^T`, `^N` and `^K` are in the sidebar's own footer two
    // rows below, `^Z` is on the row it points at, and what the duplication actually bought was
    // one fewer line of transcript and a composer pressed against the status strip.
    let sb = Sandbox::new("nohints");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("^K palette"))),
        "the sidebar carries the keys\n{:?}",
        s.sidebar_now()
    );
    assert!(
        !s.composer_chrome().join(" ").contains("palette"),
        "and the composer does not say them again\n{:?}",
        s.composer_chrome()
    );
}

#[test]
fn the_shortcut_row_comes_back_if_you_ask_for_it() {
    let sb = Sandbox::new("hints");
    sb.write_config("[options]\n\"ui.hints\" = true\n");
    let mut s = sb.start_letting_config_choose();
    // Waiting on a plugin's entry, not the host's: the host seeds `⏎ send` before any plugin has
    // loaded, so asserting on that alone would pass before the row was finished.
    assert!(
        s.pump(|s| s.composer_chrome().iter().any(|t| t.contains("^Z keys"))),
        "the way to the keys that did not fit\n{:?}",
        s.composer_chrome()
    );
    let chrome = s.composer_chrome().join(" ");
    assert!(chrome.contains("send"), "and the one key everybody needs: {chrome}");
}

/// A provider whose model says how big its window is, and which reports having filled a third of
/// it. Enough for the meter to be a real number rather than a shape.
const WIDE: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("wide", [{
    id: "wide", driver: "wide", display_name: "Wide",
    models: [{ id: "wide-1", display_name: "Wide One", context_window: 300000 }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "wide-1", usage: { input_tokens: 100000 } });
    emit({ type: "text_delta", index: 0, text: "ok" });
    emit({
      type: "message_delta",
      stop_reason: { kind: "end_turn" },
      usage: { input_tokens: 100000, output_tokens: 20 },
    });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("wide ready"));
}
"#;

/// A driver that fills its context and then compacts it, reporting both the way `claude` does.
///
/// `Activity::Context` is the driver answering "how full is it"; `Activity::Compacted` carries the
/// count either side of a compaction. The second one is the whole subject of the test: it is the
/// only moment the number moves without a request being made.
const COMPACTOR: &str = r#"
import type { PluginContext } from "@neosh/api";
const nap = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("compactor", [{
    id: "compactor", driver: "compactor", display_name: "Compactor",
    models: [{ id: "compactor", display_name: "Compactor" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "compactor", usage: {} });
    emit({ type: "activity", activity: { kind: "context", used: 180000, total: 200000 } });
    await nap(400);
    emit({ type: "activity", activity: { kind: "compacted", before: 180000, after: 12000 } });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Carrying on." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  }, { agentLoop: true });
  neosh.event.on("neosh.ready", () => neosh.notify("compactor ready"));
}
"#;

fn install_compactor(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/compactor");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"compactor\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), COMPACTOR).expect("plugin");
}

fn install_wide(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/wide");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"wide\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), WIDE).expect("plugin");
}

/// How full the window is, drawn as a bar and said as a percentage — and drawn *before* anything
/// has been spent.
///
/// It used to appear only once a turn had gone through, so the one moment you could not see how
/// much room a model has was before deciding what to do with it. A 200k window and a 1M window are
/// different tools and the difference matters most at the start.
#[test]
fn the_context_meter_is_there_before_the_first_turn() {
    let sb = Sandbox::new("ctxmeter");
    install_wide(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"wide/wide-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("wide ready");

    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("of 300k"))),
        "the window is on the strip before a single token has been spent\n{:?}",
        s.status_now()
    );
    assert!(
        s.status_now().iter().any(|l| l.contains("0% of 300k")),
        "and it reads empty rather than being absent\n{:?}",
        s.status_now()
    );
    assert!(
        s.status_now().iter().any(|l| l.contains('\u{2591}')),
        "as a bar, not only as a number\n{:?}",
        s.status_now()
    );

    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("33% of 300k"))),
        "and it fills\n{:?}",
        s.status_now()
    );
    assert!(
        s.status_now().iter().any(|l| l.contains('\u{2588}')),
        "with filled cells\n{:?}",
        s.status_now()
    );
}

/// A plugin that writes down every draft it is told about.
///
/// Which is the whole of what a completion menu does before it decides whether to open — so this is
/// the contract, with nothing in the way of reading it.
const WATCHER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  const buf = await neosh.buf.create({ name: "[drafts]", scratch: true });
  const seen: string[] = [];
  neosh.agent.onComposerChange(({ text }) => {
    seen.push(`draft<${text}>`);
    void neosh.buf.setLines(buf, 0, -1, seen);
  });
  neosh.event.on("neosh.ready", () => neosh.notify("watcher ready"));
}
"#;

// ---------------------------------------------------------------------------
// The slash menu
// ---------------------------------------------------------------------------

/// Put a lone `/` in the composer and wait for the menu it opens.
///
/// Retried, because plugins load asynchronously and nothing in the protocol announces "every
/// builtin is up" — a `/` typed a millisecond too early is a `/` in the composer and no menu.
/// Backspaced between attempts so the field is left holding exactly one slash whichever attempt
/// won, which is what the tests below then type into.
fn open_slash_menu(s: &mut Session) {
    for _ in 0..20 {
        s.type_text("/");
        s.drain_for(Duration::from_millis(150));
        if s.buffer_named("[Run]").is_some() {
            return;
        }
        s.special("backspace");
        s.drain_for(Duration::from_millis(50));
    }
    panic!("the slash menu never opened\n{}", s.transcript());
}

/// Typing `/compact` and pressing enter must send `/compact`.
///
/// It did not. The menu opened on the `/`, kept the keyboard, fuzzy-matched `compact` over every
/// command's *description* — six letters that occur in that order in a great deal of English — and
/// `↵` ran whichever unrelated command that put under the cursor. Meanwhile `/compact` itself was
/// not in the list, because a driver only reports its commands after a turn has run and this
/// conversation had not run one. So the one thing the user typed was the one thing that could not
/// happen, and what happened instead was silent.
#[test]
fn a_command_the_menu_has_never_heard_of_is_sent_to_the_agent_as_typed() {
    let sb = Sandbox::new("slashunknown");
    let mut s = sb.start();

    open_slash_menu(&mut s);

    s.type_text("compact");
    // The composer is the field: what was typed is in it, whatever the menu is doing.
    assert!(
        s.pump(|s| s.composer_now().iter().any(|l| l == "/compact")),
        "and the composer says what was typed\n{:?}",
        s.composer_now()
    );

    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("/compact"))),
        "and enter sends it\n{:?}",
        s.chat_now()
    );
    // The failure mode this replaces: something else ran instead. `chat.read` was the usual
    // winner, and it leaves the mode line saying so.
    assert!(
        !s.status_now().iter().any(|l| l.contains("reading")),
        "rather than running whatever happened to match\n{:?}",
        s.status_now()
    );
}

/// A command the *driver* named is one row of the same list, and choosing it sends it.
///
/// The two vocabularies differ in exactly one place — what accepting does. A neosh command runs and
/// the draft is cleared, because it was never a message. A driver command *is* a message and, if it
/// is finished being typed, it goes: asking for a second confirmation of a row you picked out of a
/// list is a keystroke that answers nothing.
#[test]
fn a_command_the_driver_named_is_offered_and_sent_as_a_message() {
    let sb = Sandbox::new("slashdriver");
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/driver_commands.jsonl");
    let mut s = sb.spawn(&script, Some("mock/mock"));

    // Nothing knows the driver's commands until a turn has run — the handshake is what says.
    s.type_text("hi");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("Hello."))), "a turn ran");

    open_slash_menu(&mut s);
    s.type_text("comp");
    assert!(
        s.pump(|s| s.lines_of(s.buffer_named("[Run]").unwrap_or(u64::MAX))
            .iter()
            .any(|l| l.contains("Summarise the conversation so far"))),
        "the driver's own command, in the driver's own words\n{:?}",
        s.buffer_named("[Run]").map(|b| s.lines_of(b))
    );

    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("/compact"))),
        "and choosing it asks for it\n{:?}",
        s.chat_now()
    );
}

/// Escape leaves what was typed where it was being typed.
#[test]
fn dismissing_the_slash_menu_keeps_the_draft() {
    let sb = Sandbox::new("slashescape");
    let mut s = sb.start();
    open_slash_menu(&mut s);
    s.type_text("git");
    s.special("esc");
    assert!(
        s.pump(|s| s.composer_now().iter().any(|l| l == "/git")),
        "still there, mid-word\n{:?}",
        s.composer_now()
    );
}

/// A search borrows the composer, and what is in it then is not a draft.
///
/// `/` in the transcript types into the same field the composer uses, which is deliberate — a float
/// over the thing you are searching would hide the answer. The cost is that anything watching the
/// draft sees `/`, then `/n`, then `/ne`. For the slash-command menu, which opens on exactly a lone
/// `/`, that means opening a picker on top of the search the moment it starts and taking the
/// keyboard off it.
///
/// So the event is about the *draft*, and a borrowed field has none.
fn drafts(s: &Session) -> Vec<String> {
    s.buffer_named("[drafts]").map(|b| s.lines_of(b)).unwrap_or_default()
}

#[test]
fn a_search_is_not_broadcast_as_a_draft() {
    let sb = Sandbox::new("searchdraft");
    let dir = sb.root.join("config/plugins/watcher");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"watcher\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), WATCHER).expect("plugin");

    let mut s = sb.start();
    s.wait_for("watcher ready");

    // An ordinary draft is broadcast, or the event would be useless.
    s.type_text("hi");
    assert!(
        s.pump(|s| drafts(s).iter().any(|l| l.contains("draft<hi>"))),
        "typing in the composer is a draft\n{:?}",
        drafts(&s)
    );

    // Now the search. Every one of these keystrokes lands in the same buffer.
    s.ctrl("s");
    s.type_text("/neosh");
    assert!(
        s.pump(|s| s.composer_now().iter().any(|l| l.contains("/neosh"))),
        "the search is being typed\n{:?}",
        s.composer_now()
    );

    // Back to the composer, and one more character. Waiting for *that* to be recorded is what makes
    // the negative below mean anything: events reach a plugin in order, so a draft recorded after
    // the search proves every keystroke of the search was already handled — or dropped.
    s.special("esc");
    s.special("esc");
    s.type_text("z");
    assert!(
        s.pump(|s| drafts(s).iter().any(|l| l.contains("draft<hiz>"))),
        "the composer is the draft again\n{:?}",
        drafts(&s)
    );

    let seen = drafts(&s);
    assert!(
        !seen.iter().any(|l| l.contains("draft</")),
        "and not one keystroke of the search was announced as a draft\n{seen:?}"
    );
}

/// A driver that reports its own context occupancy while it works, the way a vendor CLI does.
///
/// The catalogue says this model has a 300k window. The driver says the window is 128k and that it
/// has 64k in it — which is the situation that matters, because an agent driver's window is
/// whatever *it* decided and it is the only thing that knows what it is still holding.
const REPORTER: &str = r#"
import type { PluginContext } from "@neosh/api";
const nap = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("reporter", [{
    id: "reporter", driver: "reporter", display_name: "Reporter",
    models: [{ id: "reporter-1", display_name: "Reporter One", context_window: 300000 }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "reporter-1", usage: {} });
    // Mid-turn, before a single token of answer and long before the turn ends.
    emit({ type: "activity", activity: { kind: "context", used: 64000, total: 128000 } });
    await nap(900);
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "ok" });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  }, { agentLoop: true });
  neosh.event.on("neosh.ready", () => neosh.notify("reporter ready"));
}
"#;

fn install_reporter(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/reporter");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"reporter\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), REPORTER).expect("plugin");
}

/// The meter believes the driver, and moves while the turn is still running.
///
/// Two faults, one symptom. The number came from the usage of the last *request*, and the window
/// came from neosh's catalogue — so for an agent driver, which holds the conversation on its own
/// side and prunes and compacts it without telling anybody, both halves were guesses. And it was
/// only ever recomputed when a turn *ended*, which for an agent driver is minutes: a fresh
/// conversation read `0%` and stayed there for the whole of its first turn, which is exactly when
/// somebody looks at it.
#[test]
fn the_context_meter_believes_the_driver_and_moves_while_it_works() {
    let sb = Sandbox::new("ctxreport");
    install_reporter(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"reporter/reporter-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("reporter ready");

    // Before the turn: nothing has been reported, so the catalogue is the best guess there is.
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("0% of 300k"))),
        "the catalogue is the fallback, not a lie\n{:?}",
        s.status_now()
    );

    s.type_text("go");
    s.enter();
    // 64k of 128k. Both halves are the driver's: the catalogue would have said 21% of 300k.
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("50% of 128k"))),
        "the driver's own numerator and its own denominator\n{:?}",
        s.status_now()
    );
    assert!(
        !s.status_now().iter().any(|l| l.contains("of 300k")),
        "and not the catalogue's window once it has been told\n{:?}",
        s.status_now()
    );
    // The driver sent that before it wrote a word of the answer, so seeing it here means the meter
    // did not wait for the turn to be over.
    assert!(
        !s.saw("ok"),
        "and it arrived before the answer did, not after the turn\n{}",
        s.transcript()
    );
}

/// One gauge, not two. The sidebar drew a context meter and so did the usage plugin; for as long
/// as no model in the catalogue reported a window, only one of them ever appeared. The moment one
/// did, the footer had two bars in it that disagreed about what "used" meant.
#[test]
fn there_is_exactly_one_context_meter() {
    let sb = Sandbox::new("onemeter");
    install_wide(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"wide/wide-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("wide ready");
    s.type_text("go");
    s.enter();
    assert!(s.pump(|s| s.status_now().iter().any(|l| l.contains("33% of 300k"))));

    let line = s.status_now().join(" ");
    let bars = line.match_indices('\u{2588}').count();
    assert!(bars > 0 && bars <= 8, "one bar of at most eight cells, not two: {line:?}");
}

/// Compaction moves the meter, because compaction is what moved the context.
///
/// The card said `180k → 12k` and the meter under it went on reporting 180k — for the rest of the
/// conversation, because a driver that has once answered "how full is it" turns off the estimate
/// derived from usage, and the next answer only arrives with the next request. The driver said
/// `after`; that is the number.
#[test]
fn compacting_updates_the_context_meter() {
    let sb = Sandbox::new("compactctx");
    install_compactor(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"compactor/compactor\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("compactor ready");
    s.type_text("go");
    s.enter();

    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Compacted the conversation"))),
        "it compacted, and said so\n{:?}",
        s.chat_now()
    );
    // The card carries both numbers, so the one the meter has to agree with is on screen beside
    // it. 12k of the 200k window the driver reported is 6%; 180k is 90%, which is what the meter
    // went on saying.
    assert!(
        s.chat_now().iter().any(|l| l.contains("180.0k") && l.contains("12.0k")),
        "with the count either side of it\n{:?}",
        s.chat_now()
    );
    assert!(
        s.pump(|s| {
            let line = s.status_now().join("");
            line.contains("6% of 200k") && !line.contains("90%")
        }),
        "and the meter says what the window holds now\n{:?}",
        s.status_now()
    );
}

/// A strip too narrow for everything drops whole segments, least important first, rather than
/// writing the right half over the end of the left one.
#[test]
fn a_narrow_status_strip_drops_segments_rather_than_overlapping_them() {
    let sb = Sandbox::new("narrowstatus");
    install_wide(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"wide/wide-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("wide ready");
    assert!(s.pump(|s| s.status_now().iter().any(|l| l.contains("of 300k"))));

    s.viewport("[status]", 40, 1);
    assert!(
        s.pump(|s| {
            let line = s.status_now().join("");
            !line.contains("of 300k") && line.contains("chat")
        }),
        "the meter went and the mode stayed\n{:?}",
        s.status_now()
    );
    let line = s.status_now().join("");
    assert!(
        line.chars().count() <= 41,
        "and the line still fits the window: {} chars of 40\n{line:?}",
        line.chars().count()
    );
}

#[test]
fn a_key_shown_beside_what_it_changes_is_not_repeated_on_the_shortcut_row() {
    // The rule the footer exists for. Saying it twice costs a row and teaches nothing the first
    // place did not — and the first place is better, because the thing being changed is right
    // there being read.
    let sb = Sandbox::new("nodupekeys");
    let mut s = sb.start();
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("^P"))),
        "the model carries its key\n{:?}",
        s.status_now()
    );
    assert!(
        !s.composer_chrome().join(" ").contains("^P"),
        "and the shortcut row does not say it again\n{:?}",
        s.composer_chrome()
    );
}

#[test]
fn an_empty_composer_says_what_to_do_with_it() {
    let sb = Sandbox::new("placeholder");
    let mut s = sb.start();
    assert!(
        s.pump(|s| s.composer_chrome().iter().any(|t| t.contains("Ask anything"))),
        "a prompt for the one field there is\n{:?}",
        s.composer_chrome()
    );
    // And it goes away once there is something to send, or it would look like part of the message.
    s.type_text("hi");
    assert!(
        s.pump(|s| !s.composer_chrome().iter().any(|t| t.contains("Ask anything"))),
        "the placeholder steps aside\n{:?}",
        s.composer_chrome()
    );
}

#[test]
fn the_welcome_says_which_account_answers() {
    // The whole complaint the account split exists to answer: a fresh screen that says "no API
    // key" tells you nothing about the plan you are actually on. This says which it is, before you
    // have typed anything.
    let sb = Sandbox::new("welcome");
    let mut s = sb.start();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("model      mock/mock"))),
        "the welcome names the model\n{:?}",
        s.chat_now()
    );
}

#[test]
fn the_welcome_does_not_land_on_top_of_a_conversation() {
    // Regression: the greeting was appended at startup unconditionally, so restoring a session
    // printed "type a message and press Enter" *underneath* the last thing that was said.
    let sb = Sandbox::new("welcomeconv");
    let mut s = sb.start();
    s.pump(|s| s.chat_now().iter().any(|l| l.contains("neosh")));
    s.type_text("a question");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("a question"))), "it was asked");
    // It may stay at the top — that is where a header belongs. What it must never do is appear
    // *below* the exchange, which is what happened when it was appended at startup regardless of
    // what had already been restored into the buffer.
    let chat = s.chat_now();
    let welcome = chat.iter().position(|l| l.contains("agent workspace"));
    let question = chat.iter().position(|l| l.contains("a question")).expect("the question is there");
    if let Some(w) = welcome {
        assert!(w < question, "the welcome stays above the conversation\n{chat:?}");
    }
}


// ---------------------------------------------------------------------------
// A model that has stopped working
// ---------------------------------------------------------------------------

impl Sandbox {
    /// Put a conversation on disk that remembers a model needing an API key nobody has.
    ///
    /// Written as a file rather than produced by driving the UI, because the situation being tested
    /// is specifically "this was chosen a long time ago, on a machine that had the key".
    fn remember_a_conversation_using(&self, instance: &str, model: &str) {
        let dir = self.root.join("state/sessions");
        std::fs::create_dir_all(&dir).expect("session dir");
        let cwd = self.work().display().to_string();
        std::fs::write(
            dir.join("aaaaaaaa-1111-2222-3333-444444444444.json"),
            format!(
                r#"{{"id":"aaaaaaaa-1111-2222-3333-444444444444","cwd":"{cwd}",
                    "created_at":1,"updated_at":2,"messages":[],
                    "selection":{{"instance":"{instance}","model":"{model}","options":[]}},
                    "archived":false}}"#
            ),
        )
        .expect("write session");
    }
}

#[test]
fn a_remembered_model_that_cannot_authenticate_gives_way_to_one_that_can() {
    // The complaint this exists for: a conversation started against an API-key provider, reopened
    // on a machine with no key, greeting you with "no key" over a model you did not choose today.
    // A stored selection is a record of what was used, not an instruction.
    let sb = Sandbox::new("staleselection");
    sb.remember_a_conversation_using("anthropic", "claude-opus-5");
    let mut s = sb.start_letting_config_choose();
    assert!(
        s.pump(|s| s.texts().iter().any(|t| t.contains("instead"))),
        "it said what it swapped and what for\n{}",
        s.transcript()
    );
    assert!(
        s.texts().iter().any(|t| t.contains("anthropic/claude-opus-5 cannot authenticate")),
        "and named the one that stopped working\n{}",
        s.transcript()
    );
}

#[test]
fn a_model_you_asked_for_is_not_swapped_out_from_under_you() {
    // The other half of the rule. `agent.model` is somebody saying which model to use; answering
    // that with a different one would be worse than the error they get when they send.
    let sb = Sandbox::new("pinnedselection");
    let dir = sb.root.join("config/plugins/locked");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"locked\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), WANTS_A_KEY).expect("write plugin");
    sb.write_config("[options]\n\"agent.model\" = \"vault/sealed\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("vault ready");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("vault/sealed"))),
        "the welcome still names the model that was asked for\n{:?}",
        s.chat_now()
    );
    // The welcome still warns that it cannot authenticate — that is the honest thing to say. What
    // must not happen is a substitution, which is the "instead" this looks for.
    assert!(
        !s.texts().iter().any(|t| t.contains("instead")),
        "the choice stood\n{}",
        s.transcript()
    );
}

/// A driver whose instance wants a key from a variable that is never set.
///
/// Registered by a plugin rather than written in `config.toml`, because the point is an instance
/// that *resolves* — there is a driver behind it — and still cannot authenticate. A configured
/// instance with no driver loaded fails a different way, one step earlier.
const WANTS_A_KEY: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("vault", [{
    id: "vault",
    driver: "vault",
    display_name: "Vault",
    auth: { kind: "env", var: "NEOSH_TEST_VAULT_KEY_NEVER_SET" },
    models: [{
      id: "sealed",
      display_name: "Sealed",
      capabilities: {
        tools: false, vision: false, streaming: true, thinking: false, prompt_caching: false,
        option_descriptors: [],
      },
    }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "sealed", usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("vault ready"));
}
"#;


/// A two-pane picker with a slow provider between two quick ones.
///
/// The shape of the bug this reproduces: holding a movement key starts one lookup per row it passes
/// through, and they finish in whatever order the network allows.
const SLOW_RAIL: &str = r#"
import type { PluginContext } from "@neosh/api";
import { railPicker } from "@neosh/api/ui";

const after = (neosh: any, ms: number) =>
  new Promise<void>((done) => neosh.timer.after(ms, () => done()));

export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("slow.pick", async () => {
    await railPicker<string, string>(neosh, {
      title: "Slow",
      rail: [
        { label: "First", value: "first" },
        { label: "Dawdler", value: "dawdler" },
        { label: "Last", value: "last" },
      ],
      async items(which) {
        if (which === "dawdler") {
          await after(neosh, 600);
          return [];
        }
        return [{ label: `${which} arrived`, value: which }];
      },
    });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("slow ready"));
}
"#;

#[test]
fn a_slow_provider_cannot_clobber_the_one_you_moved_on_to() {
    let sb = Sandbox::new("railrace");
    let dir = sb.root.join("config/plugins/slowrail");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"slowrail\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("write manifest");
    std::fs::write(dir.join("main.ts"), SLOW_RAIL).expect("write plugin");

    let mut s = sb.start();
    s.wait_for("slow ready");
    s.send(&command("slow.pick"));
    s.wait_for("first arrived");

    // Two moves in a row, without waiting: straight through the slow one and out the other side,
    // which is what holding a movement key does.
    s.special("left");
    s.special("down");
    s.special("down");
    assert!(
        s.pump(|s| s.picker_named("[Slow]").iter().any(|l| l.contains("last arrived"))),
        "the provider you ended on decides what is listed\n{:?}",
        s.picker_named("[Slow]")
    );

    // And it stays that way once the one you passed through finally replies. Pumping keeps events
    // flowing while that happens, which is what gives the late answer its chance to land.
    let until = Instant::now() + Duration::from_millis(1200);
    while Instant::now() < until {
        s.pump(|_| false);
    }
    assert!(
        s.picker_named("[Slow]").iter().any(|l| l.contains("last arrived")),
        "a late answer to a question you have moved on from is dropped\n{:?}",
        s.picker_named("[Slow]")
    );
}


// ---------------------------------------------------------------------------
// Markdown in the transcript
// ---------------------------------------------------------------------------

fn markdown_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/markdown_turn.jsonl")
}

#[test]
fn an_answer_is_rendered_as_markdown_while_it_streams() {
    // The fixture arrives three characters at a time, so what this checks is the incremental path:
    // headings, lists and fences all have to survive being cut in half by a token boundary.
    let sb = Sandbox::new("mdstream");
    let mut s = sb.spawn(&markdown_fixture(), Some("mock/mock"));
    s.wait_for("PROJECTS");
    s.type_text("tell me");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l == "That is all.")),
        "the answer finished\n{:?}",
        s.chat_now()
    );

    let chat = s.chat_now();
    let has = |needle: &str| chat.iter().any(|l| l == needle);
    assert!(has("What I found"), "the heading lost its hashes\n{chat:?}");
    assert!(has("  \u{2022} the rule is drawn above the composer"), "bullets and bold\n{chat:?}");
    assert!(has("  \u{2022} ui.hints turns the row off"), "inline code\n{chat:?}");
    assert!(has("  \u{2022} links like the docs keep their words"), "link labels\n{chat:?}");
    assert!(has("  rust"), "the fence says what language it is\n{chat:?}");
    assert!(has("  fn main() {"), "and its contents are indented\n{chat:?}");
    assert!(
        has("      // ## not a heading"),
        "nothing inside a fence is markup\n{chat:?}"
    );
    assert!(
        !chat.iter().any(|l| l.contains("```") || l.contains("**")),
        "no markers survive into the transcript\n{chat:?}"
    );
}

#[test]
fn a_rendered_answer_looks_the_same_after_switching_away_and_back() {
    // Two renderers is how a reopened conversation stops looking like the one you were just in.
    let sb = Sandbox::new("mdreplay");
    // Not asked before closing: these use `session.close` as a way to *leave* a conversation and
    // come back, and the confirmation is a different test's subject.
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let mut s = sb.spawn(&markdown_fixture(), Some("mock/mock"));
    s.wait_for("PROJECTS");
    s.type_text("tell me");
    s.enter();
    // Not just "the last words arrived" — the working line is still down there until the turn
    // actually ends, and it is not part of the answer being compared.
    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            rows.iter().any(|l| l == "That is all.")
                && !rows.iter().any(|l| l.contains("esc to interrupt"))
        }),
        "the turn finished"
    );
    let live: Vec<String> = s.chat_now().into_iter().filter(|l| !l.trim().is_empty()).collect();

    s.send(&command("session.new"));
    assert!(s.pump(|s| !s.chat_now().iter().any(|l| l == "That is all.")), "somewhere else now");
    // Closing the empty one lands back in the conversation it was opened from.
    s.send(&command("session.close"));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l == "That is all.")),
        "and back\n{:?}",
        s.chat_now()
    );
    let replayed: Vec<String> = s.chat_now().into_iter().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        replayed.ends_with(&live[live.len().saturating_sub(6)..]),
        "the replay ends the way the live stream did\nlive: {live:?}\nback: {replayed:?}"
    );
}

#[test]
fn markdown_can_be_turned_off_for_when_you_want_what_was_actually_sent() {
    let sb = Sandbox::new("mdoff");
    sb.write_config("[options]\n\"chat.markdown\" = false\n");
    let mut s = sb.spawn(&markdown_fixture(), Some("mock/mock"));
    s.wait_for("PROJECTS");
    s.type_text("tell me");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("## What I found"))),
        "the hashes are still there\n{:?}",
        s.chat_now()
    );
}


// ---------------------------------------------------------------------------
// Steering: saying something to a turn that is already running
// ---------------------------------------------------------------------------

#[test]
fn typing_while_it_works_is_steering_rather_than_an_error() {
    // It used to say "a turn is already running" and throw the sentence away. That is the one
    // moment in the program where you know exactly what you want to say and cannot say it.
    let sb = Sandbox::new("steer");
    install_stall(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"stall/stall\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("stall ready");

    s.type_text("first");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("first"))), "the turn started");

    s.type_text("actually, do it differently");
    s.enter();
    assert!(
        s.pump(|s| s.composer_chrome().iter().any(|t| t.contains("queued"))),
        "it is held, and says so\n{:?}",
        s.composer_chrome()
    );
    assert!(
        s.composer_chrome().iter().any(|t| t.contains("do it differently")),
        "and says what is waiting\n{:?}",
        s.composer_chrome()
    );
    assert!(
        !s.texts().iter().any(|t| t.contains("already running")),
        "and does not refuse it\n{}",
        s.transcript()
    );
    // Not in the transcript yet: the model has not been told, and a transcript showing a question
    // nobody has been asked is a transcript that is lying.
    assert!(
        !s.chat_now().iter().any(|l| l.contains("do it differently")),
        "not shown as asked until it has been\n{:?}",
        s.chat_now()
    );
}

#[test]
fn a_message_queued_past_the_last_gap_becomes_the_next_turn() {
    // Steering is taken into the running turn at the next gap. If the turn ends before there is
    // one — interrupted, or simply finished — the question was still asked, and dropping it
    // silently would be the worst possible reading of "queued".
    let sb = Sandbox::new("steerleftover");
    install_stall(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"stall/stall\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("stall ready");

    s.type_text("first");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("esc to interrupt"))), "working");
    s.type_text("second");
    s.enter();
    assert!(s.pump(|s| s.composer_chrome().iter().any(|t| t.contains("queued"))), "held");

    s.special("esc");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("second"))),
        "it was asked once there was room to ask it\n{:?}",
        s.chat_now()
    );
}

#[test]
fn the_working_line_says_how_much_is_waiting() {
    let sb = Sandbox::new("steercount");
    install_stall(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"stall/stall\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("stall ready");
    s.type_text("go");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("esc to interrupt"))), "working");

    s.type_text("one");
    s.enter();
    s.type_text("two");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("2 queued"))),
        "the line that says a turn is in flight also says what is behind it\n{:?}",
        s.chat_now()
    );
}

/// A driver that answers with the message it was actually handed.
///
/// The point of view that matters here. Everything else in this file can see what was drawn, and
/// what was drawn is not what was asked — the transcript writes a queued message down the moment
/// it enters the conversation, whether or not anything ever went out with it. Only the driver
/// knows, so the driver says.
///
/// It naps first, so there is a window in which to queue something while it is out.
const PARROT: &str = r#"
import type { PluginContext } from "@neosh/api";
const nap = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("parrot", [{
    id: "parrot", driver: "parrot", display_name: "Parrot",
    models: [{ id: "parrot", display_name: "Parrot" }],
  }], async (req, emit) => {
    await nap(900);
    const last = [...req.messages].reverse().find((m) => m.role === "user");
    const said = (last?.content ?? [])
      .map((b: { type: string; text?: string }) => (b.type === "text" ? b.text ?? "" : ""))
      .join(" ")
      .split("\n")
      .map((l: string) => l.trim())
      .filter(Boolean)
      .join(" + ");
    emit({ type: "message_start", model: "parrot", usage: {} });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "asked: " + said });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("parrot ready"));
}
"#;

fn install_parrot(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/parrot");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"parrot\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), PARROT).expect("plugin");
}

/// Two things queued into the same gap are two things that were asked.
///
/// They used to become two user *messages*, and a driver that keeps the conversation on its own
/// side is handed the newest one and nothing else — so the older question was drawn in the
/// transcript as asked and then never put to anybody. From the outside that is indistinguishable
/// from an agent that read your message and ignored it, which is exactly what it looked like.
///
/// Asserted on what the driver says it was handed, because the transcript cannot tell the
/// difference: it writes a queued message down when it enters the conversation, whether or not
/// anything went out with it.
#[test]
fn everything_queued_into_one_gap_is_asked_and_not_just_the_last_of_it() {
    let sb = Sandbox::new("steerboth");
    install_parrot(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"parrot/parrot\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("parrot ready");

    s.type_text("go");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("esc to interrupt"))), "working");

    s.type_text("also the tests");
    s.enter();
    s.type_text("and the docs");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("2 queued"))), "both held");

    assert!(
        s.pump(|s| s
            .chat_now()
            .iter()
            .any(|l| l.contains("asked: also the tests + and the docs"))),
        "both reached the driver, in the order they were typed\n{:?}",
        s.chat_now()
    );
}

/// The queue is unsent conversation, so it reads with the conversation and not with the furniture.
///
/// Under the field it sat below the shortcut row and one line above the status bar, in the strip
/// the eye has been trained to skip. Above it, it is the last thing between what has been said and
/// what you are saying — which is what it is.
#[test]
fn what_is_queued_sits_above_the_composer_and_says_how_to_take_it_back() {
    let sb = Sandbox::new("steerplace");
    install_stall(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"stall/stall\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("stall ready");
    s.type_text("go");
    s.enter();
    assert!(s.pump(|s| s.chat_now().iter().any(|l| l.contains("esc to interrupt"))), "working");

    s.type_text("and check the tests");
    s.enter();
    assert!(
        s.pump(|s| s.composer_chrome().iter().any(|t| t.contains("and check the tests"))),
        "held, and shown\n{:?}",
        s.composer_chrome()
    );
    assert!(
        s.composer_chrome().iter().any(|t| t.contains("take back")),
        "with the key that undoes it, saying what it does rather than naming the operation\n{:?}",
        s.composer_chrome()
    );
    // And the field itself says what `\u{23ce}` does now. "Ask anything" while a turn is running
    // invites the one thing you cannot do and says nothing about the thing you can, which is how a
    // sentence disappears into a queue nobody knew was there.
    assert!(
        s.composer_chrome().iter().any(|t| t.contains("Steer it")),
        "and the empty field says what sending does now\n{:?}",
        s.composer_chrome()
    );
    assert!(
        !s.composer_now().iter().any(|l| l.contains("and check the tests")),
        "and it is chrome, not something that would be sent twice: {:?}",
        s.composer_now()
    );

    // Enter is a decision made in half a second about a sentence you were still writing. This is
    // the other half of it: the message comes back to be changed rather than being lost to a typo.
    s.send(&command("chat.queue.edit"));
    assert!(
        s.pump(|s| s.composer_now().iter().any(|l| l.contains("and check the tests"))),
        "back where it can be edited\n{:?}",
        s.composer_now()
    );
    assert!(
        s.pump(|s| !s.composer_chrome().iter().any(|t| t.contains("and check the tests"))),
        "and out of the queue, not in both places\n{:?}",
        s.composer_chrome()
    );
}

/// A question butted against whatever came before it reads as another line of that answer.
///
/// The case this exists for is a tool card, which has no trailing blank of its own: the previous
/// turn ends with `└ 3 files`, and the next thing you said starts on the very next row.
// ---------------------------------------------------------------------------
// A conversation happens where its project is
// ---------------------------------------------------------------------------

/// A provider that answers with the directory it was told the turn happens in.
///
/// Which is the only way to check this from outside: the thing that was wrong was what a *driver*
/// received, and a driver is the far side of the provider boundary.
const WHERE: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("where", [{
    id: "where", driver: "where", display_name: "Where",
    models: [{ id: "where-1", display_name: "Where One" }],
  }], async (req, emit) => {
    emit({ type: "text_delta", index: 0, text: `cwd=${req.cwd}` });
    emit({ type: "message_stop" });
  });
  // Driven through the same calls a real plugin uses, so the test exercises the public path
  // rather than a back door the product does not have.
  await neosh.cmd.register("where.at", async (args) => {
    await neosh.session.create({ cwd: args[0], activate: true });
  }, { desc: "open a conversation in a directory" });
  await neosh.cmd.register("where.goto", async (args) => {
    const all = await neosh.session.list();
    const found = all.find((s) => s.cwd === args[0]);
    if (found) await neosh.session.switch(found.id);
    else neosh.notify(`no conversation in ${args[0]}`, "warn");
  }, { desc: "switch to the conversation in a directory" });
  neosh.event.on("neosh.ready", () => neosh.notify("where ready"));
}
"#;

fn install_where(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/where");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"where\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), WHERE).expect("plugin");
}

/// A turn happens in the conversation's project, not in whatever directory neosh was started from.
///
/// This was the whole bug. A conversation carried a `cwd` and the sidebar grouped by it, but the
/// thing that actually does the work — a vendor CLI, which reads files and runs commands relative
/// to where it was spawned — was never told. So every project's conversations ran in the same
/// directory, and the grouping was decoration.
#[test]
fn a_turn_runs_in_the_project_the_conversation_belongs_to() {
    let sb = Sandbox::new("turncwd");
    install_where(&sb);
    let elsewhere = sb.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    sb.write_config("[options]\n\"agent.model\" = \"where/where-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("where ready");
    s.wait_for("PROJECTS");

    // A conversation in a second directory, which is what adding a project amounts to.
    s.send(&command_with("where.at", &elsewhere.display().to_string()));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("elsewhere"))),
        "the banner names the project this conversation is in\n{:?}",
        s.chat_now()
    );

    s.type_text("go");
    s.enter();
    let want = format!("cwd={}", elsewhere.display());
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains(&want))),
        "and the driver was told to work there\n{:?}",
        s.chat_now()
    );
}

/// Switching back moves everything back with it.
#[test]
fn switching_conversations_moves_the_working_directory_too() {
    let sb = Sandbox::new("switchcwd");
    install_where(&sb);
    let elsewhere = sb.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");
    sb.write_config("[options]\n\"agent.model\" = \"where/where-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("where ready");
    s.wait_for("PROJECTS");
    s.send(&command_with("where.at", &elsewhere.display().to_string()));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("elsewhere"))),
        "in the second project\n{:?}",
        s.chat_now()
    );

    // Back to the first, and a turn there must run in the original workspace again.
    let home = format!("directory  {}", sb.work().display());
    s.send(&command_with("where.goto", &sb.work().display().to_string()));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains(&home))),
        "back in the first\n{:?}",
        s.chat_now()
    );
    s.type_text("go");
    s.enter();
    let want = format!("cwd={}", sb.work().display());
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains(&want))),
        "back in the first project\n{:?}",
        s.chat_now()
    );
}

#[test]
fn a_question_gets_a_gap_above_it_and_keeps_its_bar() {
    let sb = Sandbox::new("questiongap");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Read  .env"))),
        "the turn ran\n{:?}",
        s.chat_now()
    );
    s.type_text("again");
    s.enter();

    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            rows.iter().position(|l| l.contains("again")).is_some_and(|at| {
                at > 0 && rows[at - 1].trim().is_empty()
            })
        }),
        "there is a blank row above it\n{:?}",
        s.chat_now()
    );
    // And the bar is *coloured* on the question rather than on the blank row above it. The gap
    // shifts every row after it, which is exactly the sort of off-by-one that leaves the text
    // looking perfectly correct and the colour one row out.
    let rows = s.chat_now();
    let at = rows.iter().position(|l| l.contains("again")).expect("found above");
    let groups = s.chat_groups();
    assert!(
        groups.get(at).is_some_and(|g| g.iter().any(|h| h == "Agent.User")),
        "the bar is coloured on the question, not on the gap: {:?}",
        &groups[at.saturating_sub(1)..(at + 1).min(groups.len())]
    );
}

#[test]
fn a_tool_calls_dot_says_whether_it_is_still_going() {
    // The only thing in the transcript that says whether something is still happening. Colour
    // rather than shape, because a glyph that changes to mean "finished" makes the column
    // impossible to scan — and a dot that never stops pulsing is a tool that looks hung.
    let sb = Sandbox::new("tooldot");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.key("h");
    s.enter();
    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            let Some(at) = rows.iter().position(|l| l.contains("Read  .env")) else {
                return false;
            };
            // Failed, because `.env` is not there — which is the point: the dot reports how it
            // went, not merely that it is over.
            s.chat_groups().get(at).is_some_and(|g| g.iter().any(|h| h == "Agent.ToolFailed"))
        }),
        "the dot beside a finished call says how it went\n{:?}\n{:?}",
        s.chat_now(),
        s.chat_groups()
    );
}

#[test]
fn a_turn_that_has_already_said_something_still_says_it_is_working() {
    // The bug this fixes: the first token took the working line away and nothing put it back, so
    // a turn that had written a sentence and then went off to run tools looked finished in the
    // chat while the footer and the sidebar both said it was still going.
    let sb = Sandbox::new("stillworking");
    install_halfway(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"half/half\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("half ready");

    s.type_text("go");
    s.enter();

    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            rows.iter().any(|l| l.contains("what I found so far"))
                && rows.iter().any(|l| l.contains("esc to interrupt"))
        }),
        "the answer is on screen and so is the fact that it is not finished\n{:?}",
        s.chat_now()
    );
    // And in that order: a spinner above the sentence it is producing says the wrong thing about
    // which of them is still happening.
    let rows = s.chat_now();
    let said = rows.iter().position(|l| l.contains("what I found so far")).expect("the answer");
    let spinner = rows.iter().position(|l| l.contains("esc to interrupt")).expect("the line");
    assert!(spinner > said, "the working line is under the answer\n{rows:?}");
}

#[test]
fn how_much_of_a_tool_result_to_show_is_a_setting() {
    // Three lines is a default, not a law. `0` is the other end of it: how much came back, and
    // nothing else.
    let sb = Sandbox::new("toollines");
    install_chatty(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"chatty/chatty\"\n\"chat.tool_output_lines\" = 2\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("chatty ready");
    s.type_text("go");
    s.enter();

    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Counted."))),
        "the turn finished\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    assert!(rows.iter().any(|l| l.contains("one")), "one line of head is there\n{rows:?}");
    // Folded from the *middle*: what a command decided is at the end of what it printed, and a
    // card that keeps the first two lines of `cargo test` keeps `Compiling` and `Finished`.
    assert!(rows.iter().any(|l| l.contains("five")), "and the line it ended on\n{rows:?}");
    assert!(
        !rows.iter().any(|l| l.contains("three")),
        "and the setting stopped it there\n{rows:?}"
    );
    assert!(
        rows.iter().any(|l| l.contains("+3 lines")),
        "saying how much was left out, because a result that is silently cut is a result you \
         would believe\n{rows:?}"
    );
}

#[test]
fn a_tool_call_says_what_came_back_not_only_that_it_ran() {
    // A card that names the tool and nothing else is a card you have to take on trust, and "it
    // ran" is not the thing you wanted to know.
    let sb = Sandbox::new("toolresult");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Read  .env"))),
        "the call is shown with its subject\n{:?}",
        s.chat_now()
    );
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains('\u{2514}')
            && l.contains("could not read"))),
        "and what it came back with, under it and joined to it\n{:?}",
        s.chat_now()
    );
}

#[test]
fn an_edit_card_shows_the_change_and_how_much_of_it() {
    // `edit(main.rs)` and "edited" underneath says that something happened. The diff says what,
    // the `+2 -1` says how much, and the row at the end of the turn adds it up — which is the
    // difference between a transcript you can review and one you have to take on trust.
    let sb = Sandbox::new("editcard");
    install_editor(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"editor/editor\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("editor ready");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("changed 1 file"))),
        "the turn finished and was summed up\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    let header = rows
        .iter()
        .position(|l| l.starts_with("  Edited  ") && l.contains("main.rs"))
        .expect("the card's header");
    assert!(
        rows[header].ends_with("+2 -1"),
        "the stats are on the header, and a call this fast is not timed\n{rows:?}"
    );
    let del = rows.iter().position(|l| l.ends_with("- e")).expect("the removed line");
    let add = rows.iter().position(|l| l.ends_with("+ E")).expect("the added line");
    assert!(header < del && del < add, "the diff is under its header, in order\n{rows:?}");
    assert!(rows.iter().any(|l| l.ends_with("+ i")), "{rows:?}");
    assert!(
        !rows.iter().any(|l| l.trim() == "a" || l.trim() == "b"),
        "not every unchanged line: two either side of a change is context, eight is a file\n{rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("more lines")),
        "every change is on screen, so there is no 'more'\n{rows:?}"
    );
    let total = rows.iter().position(|l| l.contains("changed 1 file  +2 -1")).expect("the total");
    assert!(rows[total + 1].contains("main.rs") && rows[total + 1].ends_with("+2 -1"), "{rows:?}");
    let answer = rows.iter().position(|l| l.contains("Edited.")).expect("the answer");
    assert!(answer < total, "the summary closes the turn, under the answer\n{rows:?}");
}

#[test]
fn calls_that_only_looked_at_things_share_one_row_and_open_into_several() {
    // A turn that reads three files used to be three cards, each with a mark down the left and a
    // slice of the file under it. What you wanted was the *list of what it looked at*, which is
    // exactly what those bodies were holding apart. So the run is one row — and the edit that
    // follows is not part of it, because a diff is an answer and not a name.
    let sb = Sandbox::new("looks");
    install_looker(&sb);
    // Not asked before closing: these use `session.close` as a way to *leave* a conversation and
    // come back, and the confirmation is a different test's subject.
    sb.write_config(
        "[options]\n\"agent.model\" = \"looker/looker\"\n\"ui.confirm_destructive\" = false\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("looker ready");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            rows.iter().any(|l| l.contains("Looked."))
                && !rows.iter().any(|l| l.contains("esc to interrupt"))
        }),
        "the turn finished\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    let run = rows
        .iter()
        .position(|l| l.starts_with("  Explored  "))
        .unwrap_or_else(|| panic!("the run is one row\n{rows:?}"));
    assert_eq!(
        rows[run], "  Explored  one.rs, two.rs, fn main",
        "named, shortest first-glance form\n{rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("line 1")),
        "and nothing any of them came back with\n{rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.starts_with('\u{23fa}')),
        "no card carries the old dot\n{rows:?}"
    );
    // The edit is its own card, on the very next row: a card is a header at column zero with its
    // body indented under a corner, which is what says where one action ends and the next begins —
    // a blank row between every two of them was a row spent saying it again.
    assert!(rows[run + 1].starts_with("  Edited  "), "directly under the run\n{rows:?}");
    assert!(rows.iter().any(|l| l.ends_with("- a")), "the edit kept its diff\n{rows:?}");

    // Opened, one row per call, with the whole subject and how much came back.
    s.ctrl("s");
    s.key("g");
    s.key("g");
    for _ in 0..run {
        s.key("j");
    }
    s.special("tab");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Read  /work/src/one.rs"))),
        "open, every call is named in full\n{:?}",
        s.chat_now()
    );
    let open = s.chat_now();
    assert!(open.iter().any(|l| l.contains("Read  /work/src/two.rs  9 lines")), "{open:?}");
    assert!(open.iter().any(|l| l.contains("Searched  fn main  4 lines")), "{open:?}");
    assert_eq!(open[run], rows[run], "the header did not move or change\n{open:?}");

    // And the same rows come back after switching away and back, which is the whole reason the
    // live path and the replay go through one renderer.
    s.special("esc");
    let live: Vec<String> = s.chat_now().into_iter().filter(|l| !l.trim().is_empty()).collect();
    s.send(&command("session.new"));
    assert!(s.pump(|s| !s.chat_now().iter().any(|l| l.contains("Looked."))), "somewhere else");
    s.send(&command("session.close"));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Looked."))),
        "and back\n{:?}",
        s.chat_now()
    );
    let back = s.chat_now();
    assert!(
        back.iter().any(|l| l == "  Explored  one.rs, two.rs, fn main"),
        "the replay folds the run exactly as the live stream did\nlive: {live:?}\nback: {back:?}"
    );
    assert!(
        !back.iter().any(|l| l.contains("line 1")),
        "and does not unfold it\n{back:?}"
    );
}

/// A provider that runs three commands in one turn, and a `run` tool with canned output for each.
///
/// The shell neosh does not have: what matters to a card is that the call was handed a `command`,
/// which is how a card decides what a call is in the first place — off the arguments, never off a
/// list of tool names.
const RUNNER: &str = r#"
import type { PluginContext } from "@neosh/api";

const SAID: Record<string, string> = {
  "git add -A": "",
  "git commit -m 'stack the cards'": "[main 6759f10] stack the cards\n 3 files changed",
  "git push origin main": "To github.com:neoswarm/neosh\n   6759f10..c280938  main -> main",
};

export async function activate({ neosh }: PluginContext) {
  await neosh.tool.register(
    { name: "run", description: "Run", inputSchema: { type: "object" } },
    async (input) => ({ content: SAID[(input as { command: string }).command] ?? "ran" }),
  );
  let turn = 0;
  await neosh.provider.register("runner", [{
    id: "runner", driver: "runner", display_name: "Runner",
    models: [{ id: "runner", display_name: "Runner" }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "runner", usage: {} });
    turn += 1;
    if (turn === 1) {
      Object.keys(SAID).forEach((command, i) => {
        emit({ type: "block_start", index: i, block: { kind: "tool_use", id: `r${i}`, name: "run" } });
        emit({ type: "tool_input_delta", index: i, partial_json: JSON.stringify({ command }) });
        emit({ type: "block_stop", index: i });
      });
      emit({ type: "message_delta", stop_reason: { kind: "tool_use" }, usage: {} });
      emit({ type: "message_stop" });
      return;
    }
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "Pushed." });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("runner ready"));
}
"#;

fn install_runner(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/runner");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"runner\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\", \"tools\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), RUNNER).expect("plugin");
}

/// ADR 0051. A stretch of a turn spent running things is one row, and the answer is the last one's.
#[test]
fn a_run_of_commands_is_one_row_that_keeps_the_last_answer() {
    let sb = Sandbox::new("stack");
    install_runner(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"runner/runner\"\n\"ui.confirm_destructive\" = false\n",
    );
    let mut s = sb.start_letting_config_choose();
    // The splash names the active model, and the model the config asked for only becomes active
    // once the plugin that provides it has finished loading. "runner ready" is the plugin saying
    // it registered; this is the host saying it took.
    s.wait_for("runner/runner");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| {
            let rows = s.chat_now();
            rows.iter().any(|l| l.contains("Pushed."))
                && !rows.iter().any(|l| l.contains("esc to interrupt"))
        }),
        "the turn finished\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    let run = rows
        .iter()
        .position(|l| l.starts_with("  Ran  "))
        .unwrap_or_else(|| panic!("the stack is one row\n{rows:?}"));
    assert_eq!(
        rows[run], "  Ran  git add, git commit, git push origin",
        "each command named by what it is rather than how it was spelled\n{rows:?}"
    );
    // The last one's output, because that is the one the other two were getting to.
    assert!(
        rows.iter().any(|l| l.contains("6759f10..c280938")),
        "the answer is under it\n{rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("3 files changed")),
        "and the ones before it folded into their names\n{rows:?}"
    );

    // Opened, every command in full with the whole of what it printed.
    s.ctrl("s");
    s.key("g");
    s.key("g");
    for _ in 0..run {
        s.key("j");
    }
    s.special("tab");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("3 files changed"))),
        "open, nothing the fold hid is out of reach\n{:?}",
        s.chat_now()
    );
    let open = s.chat_now();
    assert!(
        open.iter().any(|l| l.contains("Ran  git commit -m 'stack the cards'")),
        "and every command is there in full\n{open:?}"
    );
    assert_eq!(open[run], rows[run], "the header did not move or change\n{open:?}");

    // And the replay folds it exactly as the live stream did, which is the whole reason the two
    // go through one renderer.
    s.special("tab");
    s.special("esc");
    s.send(&command("session.new"));
    assert!(s.pump(|s| !s.chat_now().iter().any(|l| l.contains("Pushed."))), "somewhere else");
    s.send(&command("session.close"));
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("Pushed."))),
        "and back\n{:?}",
        s.chat_now()
    );
    let back = s.chat_now();
    assert!(
        back.iter().any(|l| l == "  Ran  git add, git commit, git push origin"),
        "one row again\n{back:?}"
    );
    assert!(
        back.iter().any(|l| l.contains("6759f10..c280938"))
            && !back.iter().any(|l| l.contains("3 files changed")),
        "with the last answer and no other\n{back:?}"
    );
}

/// The three states a card can be in, and the two things that move it between them. See ADR 0057.
///
/// Folded is what a card costs when nobody is looking at it. The card the *cursor* is in shows
/// more — bounded, because nine hundred rows appearing under `j` is what the fold exists to
/// prevent — and `\u{21e5}` is all of it. The trailer says which key at every step where something
/// is still being held back.
#[test]
fn a_card_opens_as_the_cursor_arrives_and_folds_as_it_leaves() {
    let sb = Sandbox::new("cardfold");
    install_editor(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"editor/editor\"\n\"chat.diff_lines\" = 2\n\
         \"chat.preview_lines\" = 4\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("editor ready");
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("changed 1 file"))),
        "the turn finished\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    let trailer = rows.iter().find(|l| l.contains(" lines (")).expect("a trailer");
    assert!(trailer.contains("+6 lines"), "{trailer}");
    assert!(trailer.contains("^S \u{21e5} to expand"), "it says how: {trailer}");
    assert!(!rows.iter().any(|l| l.ends_with("+ i")), "the end of the diff is folded away\n{rows:?}");
    let header = rows.iter().position(|l| l.starts_with("  Edited  ")).expect("the header");

    // Into the transcript, to the top, and down onto the card. Arriving is what opens it — no key.
    s.ctrl("s");
    s.key("g");
    s.key("g");
    for _ in 0..header {
        s.key("j");
    }
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("+4 lines"))),
        "the card the cursor is in is worth more rows, and still bounded\n{:?}",
        s.chat_now()
    );
    assert!(
        !s.chat_now().iter().any(|l| l.ends_with("+ i")),
        "a preview is a budget, not an opening\n{:?}",
        s.chat_now()
    );

    s.special("tab");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.ends_with("+ i"))),
        "open, the whole diff is there\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    assert!(!rows.iter().any(|l| l.contains(" lines (")), "and nothing is held back\n{rows:?}");
    assert!(
        rows.iter().position(|l| l.contains("changed 1 file")).unwrap() > header,
        "what was below the card is still below it\n{rows:?}"
    );

    // `\u{21e5}` again gives back what the cursor was already buying — never less than that, which
    // would be a key that answers "show me more" with a smaller card.
    s.special("tab");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("+4 lines"))),
        "back to the preview\n{:?}",
        s.chat_now()
    );

    // And off the card entirely, which is the only thing that folds it.
    s.key("k");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("+6 lines"))),
        "the rows go back when the cursor does\n{:?}",
        s.chat_now()
    );
}

#[test]
fn the_welcome_says_how_to_read_the_transcript() {
    // `^S` existed for a long time before anything on screen mentioned it. The welcome is the one
    // place everybody looks once.
    let sb = Sandbox::new("welcomekeys");
    let mut s = sb.start();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("^S browse & copy"))),
        "the key row\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    // The frontend in these tests is 100 columns wide, which is room for the word.
    assert!(
        rows.iter().any(|l| l.contains("\u{2588}\u{2588}\u{2588}\u{2557}")),
        "and the word above it\n{rows:?}"
    );
}

#[test]
fn the_footer_says_what_the_agent_may_do_and_which_key_changes_it() {
    let sb = Sandbox::new("permmode");
    let mut s = sb.start();
    // Full access out of the box — see `PermissionsConfig::default` — which is precisely why the
    // row has to be there in every mode rather than only the surprising ones.
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("full access \u{21e7}\u{21e5}"))),
        "the mode, and the key beside it\n{:?}",
        s.status_now()
    );
    s.send(&command("permission.cycle"));
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("deny"))),
        "and it changes\n{:?}",
        s.status_now()
    );
}

/// The mode belongs to the conversation, not to the terminal it is read in.
///
/// The failure this is here to catch is silent and in the dangerous direction: a mode kept in one
/// shared place means switching away from a conversation you had put in `deny` and into any other
/// one leaves that other one in `deny` too — and, worse, coming back the other way carries full
/// access into a conversation somebody had deliberately locked down.
#[test]
fn a_permission_mode_belongs_to_the_conversation_it_was_chosen_in() {
    let sb = Sandbox::new("permsession");
    // Not asked before closing: these use `session.close` as a way to *leave* a conversation and
    // come back, and the confirmation is a different test's subject.
    sb.write_config("[options]\n\"ui.confirm_destructive\" = false\n");
    let mut s = sb.start_letting_config_choose();
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("full access"))),
        "the configured default to start from\n{:?}",
        s.status_now()
    );

    s.send(&command("permission.cycle")); // full access -> deny
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("deny"))),
        "this one is set\n{:?}",
        s.status_now()
    );

    // A conversation that has never been given a mode takes the configured one, not whatever the
    // conversation you came from happened to be left in.
    s.send(&command("session.new"));
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("full access"))),
        "a new one is its own\n{:?}",
        s.status_now()
    );

    s.send(&command("session.close"));
    assert!(
        s.pump(|s| s.status_now().iter().any(|l| l.contains("deny"))),
        "and going back is going back to it\n{:?}",
        s.status_now()
    );
}

impl Session {
    /// The window showing a named buffer, if the frontend was told about one.
    fn window_of(&self, name: &str) -> Option<u64> {
        let buf = self.buffer_named(name)?;
        self.events.iter().find_map(|e| {
            (e["type"] == "window_opened" && e["buf"].as_u64() == Some(buf))
                .then(|| e["win"].as_u64())?
        })
    }

    /// Tell the core how big a window really is.
    ///
    /// A stdio frontend has no geometry of its own, so nothing reports this unless a test does —
    /// which means anything that depends on real width is untested until it is said out loud here.
    fn viewport(&mut self, name: &str, width: u16, height: u16) {
        let Some(win) = self.window_of(name) else { return };
        self.send(&format!(
            r#"{{"type":"viewport_changed","win":{win},"width":{width},"height":{height},"top_line":0}}"#
        ));
    }

    fn status_now(&self) -> Vec<String> {
        match self.buffer_named("[status]") {
            Some(b) => self.lines_of(b),
            None => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Extending a bundled panel from outside it
// ---------------------------------------------------------------------------

/// A plugin that has never heard of the sidebar's source, adding to the sidebar.
///
/// Everything it does is on the public surface: a contribution point for rows, a contribution point
/// for a verb, and a key bound at the *buffer kind* rather than at a window it cannot name. If any
/// of that ever needs a private path into the panel, these tests stop compiling against the API a
/// third party has.
const EXTENDER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh, subscriptions }: PluginContext) {
  subscriptions.push(await neosh.cmd.register("acme.touched", (args) => {
    // The row under the cursor arrives as arguments, which is the whole reason the panel binds the
    // key rather than the contributor doing it.
    neosh.notify(`acme touched ${args.join(" ")}`);
  }, { desc: "Touch the row" }));

  subscriptions.push(await neosh.ext.contribute("sidebar.section", "todo", {
    title: "ACME",
    at: "below",
    rows: [{ text: "Ship the thing", command: "acme.touched", args: ["from-row"] }],
  }));

  subscriptions.push(await neosh.ext.contribute("sidebar.action", "touch", {
    key: "t",
    label: "touch",
    command: "acme.touched",
    on: "any",
  }));

  neosh.event.on("neosh.ready", () => neosh.notify("acme ready"));
}
"#;

fn install_extender(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/acme");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"acme\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), EXTENDER).expect("plugin");
}

/// Rows somebody else contributed are in the panel, and pressing `↵` on one runs their command.
#[test]
fn a_third_party_can_put_rows_in_the_sidebar() {
    let sb = Sandbox::new("extendrows");
    install_extender(&sb);
    let mut s = sb.start();
    s.wait_for("acme ready");
    s.wait_for("PROJECTS");

    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("Ship the thing"))),
        "the contributed row is drawn\n{:?}",
        s.sidebar_now()
    );

    // Into the panel, then down to their row and press it. Walked rather than jumped: the cursor
    // wraps, so a fixed number of presses would depend on how many rows the panel happens to have.
    s.ctrl("t");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("↵ open"))),
        "in the panel\n{:?}",
        s.sidebar_now()
    );
    let mut landed = false;
    for _ in 0..12 {
        if s.sidebar_cursor().is_some_and(|r| r.contains("Ship the thing")) {
            landed = true;
            break;
        }
        s.key("j");
        s.drain_for(Duration::from_millis(120));
    }
    assert!(landed, "the cursor can be put on a contributed row\n{:?}", s.sidebar_now());

    s.special("enter");
    assert!(
        s.pump(|s| s.saw("acme touched from-row")),
        "the contributed command ran with the arguments it asked for\n{:?}",
        s.sidebar_now()
    );
}

/// A verb contributed by somebody else is a real binding on the panel: it fires on its key, and the
/// row under the cursor is handed to it.
///
/// The key is bound at `buf_kind` scope, which is the part that could not be done before — the
/// panel's window id is private to the panel and changes every time it is toggled, so a third party
/// had no name to bind against.
#[test]
fn a_third_party_verb_fires_on_its_key_with_the_row_it_was_pointed_at() {
    let sb = Sandbox::new("extendverb");
    install_extender(&sb);
    let mut s = sb.start();
    s.wait_for("acme ready");
    s.wait_for("PROJECTS");

    s.ctrl("t");
    // The panel lands on the conversation you are in, so the row under the cursor is a session.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("↵ open"))),
        "the cursor is on a conversation\n{:?}",
        s.sidebar_now()
    );

    s.key("t");
    assert!(
        s.pump(|s| s.saw("acme touched session")),
        "the contributed key fired, and was told which row\n{}",
        s.transcript()
    );
}

/// The panel's own keys are ordinary bindings, so the registry knows them — and so anybody can
/// replace one.
///
/// This is the claim the whole rewrite rests on. Before it, the panel resolved keys in a private
/// switch statement: `^Z` could not list them, `^K` could not run them, and rebinding `x` meant
/// forking the plugin.
#[test]
fn the_panels_keys_are_in_the_registry_and_a_user_can_rebind_them() {
    let sb = Sandbox::new("panelkeys");
    std::fs::write(
        sb.root.join("config/init.ts"),
        r#"import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("mine.shout", () => neosh.notify("mine ran"), { desc: "Shout" });
  // Exactly what a third party writes to take a key inside somebody else's panel.
  await neosh.keymap.set("chat", "x", "mine.shout", {
    scope: { kind: "buf_kind", name: "neosh.sidebar" },
    desc: "Shout instead of archiving",
  });
  neosh.notify("init done");
}
"#,
    )
    .expect("init.ts");

    let mut s = sb.start();
    s.wait_for("init done");
    s.wait_for("PROJECTS");

    s.ctrl("t");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("↵ open"))),
        "in the panel, on a conversation\n{:?}",
        s.sidebar_now()
    );

    // `x` archives a conversation by default. Bound by the user, it does not.
    let before = s.sidebar_rows();
    s.key("x");
    assert!(
        s.pump(|s| s.saw("mine ran")),
        "the user's binding won\n{}",
        s.transcript()
    );
    assert_eq!(
        s.sidebar_rows(),
        before,
        "and the default it replaced did not also happen\n{:?}",
        s.sidebar_now()
    );
}

/// A count moves that many rows you can land on, and the panel says one is being typed.
///
/// The keys are Vim's because this is a cursor over a column in a terminal, and a list of thirty
/// conversations reached one `j` at a time is a list nobody moves around in. Counted in rows the
/// cursor can *land* on rather than lines, so the headings and rules it already skips do not
/// silently eat two of the five. See ADR 0049.
#[test]
fn a_count_before_a_motion_moves_that_many_rows() {
    let sb = Sandbox::new("counts");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    // Three conversations, so there is somewhere to count to. Named by what was typed into them,
    // which is what a conversation with no title is called in a list.
    let named = |s: &mut Session, name: &str| {
        s.type_text(name);
        s.enter();
        // Waited for in the *panel*, not in the transcript: the next thing this test does is start
        // another conversation, and a row still called "New conversation" because the rename has
        // not arrived yet is a row that makes counting them meaningless.
        let want = name.to_string();
        assert!(
            s.pump(move |s| s.sidebar_now().iter().any(|l| l.contains(&want))),
            "the panel says what this conversation is about"
        );
    };
    named(&mut s, "alpha");
    s.new_conversation();
    named(&mut s, "beta");
    s.new_conversation();
    named(&mut s, "gamma");

    // The panel lands on the conversation you are in, and the list is newest first — so counting
    // down from `gamma` two rows is `alpha`, with `beta` in between.
    s.enter_panel();
    assert!(
        s.pump(|s| s.sidebar_cursor().is_some_and(|l| l.contains("gamma"))),
        "the cursor starts where you are\n{:?}",
        s.sidebar_now()
    );

    // Half of it: a digit on its own moves nothing and says so, because a first press with no
    // visible effect is indistinguishable from a key that does nothing.
    s.key("2");
    assert!(
        s.pump(|s| s.sidebar_virt_now().iter().any(|v| v.trim() == "2")),
        "the count being typed is on screen\n{:?}",
        s.sidebar_virt_now()
    );
    assert!(
        s.sidebar_cursor().is_some_and(|l| l.contains("gamma")),
        "and nothing has moved yet\n{:?}",
        s.sidebar_now()
    );

    s.key("j");
    assert!(
        s.pump(|s| s.sidebar_cursor().is_some_and(|l| l.contains("alpha"))),
        "two rows down, not two lines\n{:?}",
        s.sidebar_now()
    );
    assert!(
        !s.sidebar_virt_now().iter().any(|v| v.trim() == "2"),
        "and the count is spent\n{:?}",
        s.sidebar_virt_now()
    );

    // `gg` is the top of the list, which is the project heading — the first row anything can land
    // on. `G` is the far end.
    s.key("g");
    s.key("g");
    assert!(
        s.pump(|s| s.sidebar_cursor().is_some_and(|l| l.contains("work"))),
        "gg is the first row you can land on\n{:?}",
        s.sidebar_now()
    );
    s.key("2");
    s.key("G");
    assert!(
        s.pump(|s| s.sidebar_cursor().is_some_and(|l| l.contains("gamma"))),
        "2G is the second row, not the end\n{:?}",
        s.sidebar_now()
    );
}

/// The column is as wide as you want it, and resizing it does not take the keyboard away.
///
/// Reopening the dock was how this used to be done, and it gives the window a new id: the panel
/// resized from inside itself threw the cursor back to the composer on every press. The width is
/// the `sidebar.width` setting either way, so a key and a line of `config.toml` say one number.
#[test]
fn the_panel_is_resized_in_place_and_keeps_the_cursor() {
    let sb = Sandbox::new("width");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.enter_panel();

    let rule = |s: &Session| {
        s.sidebar_now().iter().filter(|l| l.starts_with('\u{2500}')).map(|l| l.chars().count()).max()
    };
    let before = rule(&s).expect("the panel has a rule under its heading");

    s.key("3");
    s.key(">");
    assert!(
        s.pump(move |s| rule(s).is_some_and(|w| w > before)),
        "the column got wider\n{:?}",
        s.sidebar_now()
    );
    // Still in the panel: the hint strip is the panel's own, not the composer's.
    assert!(
        s.sidebar_now().iter().any(|l| l.contains("? keys")),
        "and the panel still has the keyboard\n{:?}",
        s.sidebar_now()
    );

    s.key("=");
    assert!(
        s.pump(move |s| rule(s) == Some(before)),
        "and back to the default\n{:?}",
        s.sidebar_now()
    );
}

impl Session {
    /// The right-hand column of every panel row, as it is *now*.
    ///
    /// [`Session::sidebar_virt`] is every one ever drawn, which answers a different question: a
    /// count that was on screen and has since been spent is in that list for the rest of the
    /// session, so "it is gone" is not a thing it can be asked.
    fn sidebar_virt_now(&self) -> Vec<String> {
        let Some(buf) = self.buffer_named("[sidebar]") else { return Vec::new() };
        let mut rows: Vec<String> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(buf) {
                continue;
            }
            let start = e["start"].as_i64().unwrap_or(0);
            let old_end = e["old_end"].as_i64().unwrap_or(start);
            let new: Vec<String> = e["lines"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|l| {
                            l["marks"]
                                .as_array()
                                .map(|ms| {
                                    ms.iter()
                                        .filter_map(|m| m["virt_text"].as_array())
                                        .flatten()
                                        .filter_map(|c| c["text"].as_str())
                                        .collect::<Vec<_>>()
                                        .join("")
                                })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let s = start.clamp(0, rows.len() as i64) as usize;
            let en = if old_end < 0 { rows.len() } else { (old_end as usize).clamp(s, rows.len()) };
            rows.splice(s..en, new);
        }
        rows
    }

    /// The panel row the cursor is on, read from the highlight rather than from a count.
    ///
    /// A count would be a test that passes for the wrong reason the moment the panel gains a row.
    fn sidebar_cursor(&self) -> Option<String> {
        let buf = self.buffer_named("[sidebar]")?;
        let text = self.lines_of(buf);
        let groups = self.groups_of(buf);
        groups
            .iter()
            .position(|row| row.iter().any(|g| g == "Sidebar.Selected"))
            .and_then(|i| text.get(i).cloned())
    }
}

/* -------------------------------------------------------------------------- */
/* What the plan has left — ADR 0044                                          */
/* -------------------------------------------------------------------------- */

/// A provider written as a plugin, reporting its own vendor's allowance two ways.
///
/// Both paths at once because they are the two halves of the rule: the mid-turn `Activity::Quota`
/// carries one window and must *patch*, and `quota.report` is complete and must *replace*. A
/// fixture that only exercised one of them would pass with the other wired backwards.
const PLANNER: &str = r#"
import type { PluginContext } from "@neosh/api";
const nap = (ms: number) => new Promise((r) => setTimeout(r, ms));

const win = (id: string, label: string, pct: number, active = false) => ({
  id, label, used_percent: pct, severity: pct >= 90 ? "critical" : "normal",
  active, resets_at: null, window_minutes: null, scope: null,
});

export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("planner", [{
    id: "planner", driver: "planner", display_name: "Planner",
    // How it is drawn, declared by the plugin exactly as a bundled provider declares it. A group
    // name, never a colour: the theme owns what `Brand.Anthropic` looks like.
    brand: { mark: "\u2733", ascii: "A", hl: "Brand.Anthropic" },
    models: [{ id: "planner-1", display_name: "Planner One", context_window: 300000 }],
  }], async (_req, emit) => {
    emit({ type: "message_start", model: "planner-1", usage: {} });
    // One window, mid-turn, before a word of the answer. Everything else must survive it.
    emit({ type: "activity", activity: {
      kind: "quota", windows: [win("weekly", "Weekly", 93, true)],
    } });
    await nap(600);
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "ok" });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  }, { agentLoop: true });

  // The complete picture, published the way a plugin provider does it.
  await neosh.cmd.register("planner.publish", async () => {
    await neosh.quota.report({
      instance: "planner",
      plan: "flightplan",
      windows: [win("session", "Session", 12, true), win("weekly", "Weekly", 40)],
      credits: null,
      observed_at: Math.floor(Date.now() / 1000),
      source: "plugin",
    });
    neosh.notify("published");
  });

  neosh.event.on("neosh.ready", () => neosh.notify("planner ready"));
}
"#;

fn install_planner(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/planner");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"planner\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), PLANNER).expect("plugin");
}

/// A provider written as a plugin gets its plan drawn like anybody's.
///
/// The whole test of the extensibility claim for this feature: nothing in the sidebar has heard of
/// `planner`, and the strip draws its windows anyway — because it listens for the event rather than
/// knowing which vendors exist.
#[test]
fn a_plugins_own_provider_appears_in_the_plan_strip() {
    let sb = Sandbox::new("planstrip");
    install_planner(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"planner/planner-1\"\n\"usage.sidebar.style\" = \"full\"\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");

    s.send(&command("planner.publish"));
    s.wait_for("published");

    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("PLAN"))),
        "the strip is a section like any other\n{:?}",
        s.sidebar_now()
    );
    let strip = s.sidebar_now().join("\n");
    assert!(strip.contains("Session"), "both windows, not just the worst:\n{strip}");
    assert!(strip.contains("Weekly"), "both windows, not just the worst:\n{strip}");
    assert!(strip.contains("12%"), "the vendor's own figures:\n{strip}");
    assert!(strip.contains("40%"), "the vendor's own figures:\n{strip}");

    // And it is on disk, because a percentage is only ever reported: a workspace that forgot the
    // last one opens with empty gauges and stays that way until something reports again — which on
    // a machine that is offline, signed out, or being told to ask less often is indefinitely.
    let saved = sb.root.join("state/quota.json");
    assert!(saved.is_file(), "the snapshot is kept, not only forwarded");
    let body = std::fs::read_to_string(&saved).expect("readable");
    assert!(body.contains("flightplan"), "with the plan it belongs to:\n{body}");
    assert!(
        std::fs::read_to_string(sb.root.join("state/quota.jsonl"))
            .is_ok_and(|h| h.contains("session")),
        "and the samples beside it, so a gauge can be a line"
    );
}

/// The plan is one row until you ask for more of it.
///
/// Three bars, a name and a sentence is five rows of a column whose job is your conversations —
/// and four of them answer a question you did not ask. The default is the limit that would refuse
/// the next request, plus anything else already critical; `<Tab>` on the row steps up to every
/// window and then to the whole block, and `^L` is all of it whichever is showing. See ADR 0049.
#[test]
fn the_plan_strip_is_one_row_until_you_ask_for_more() {
    let sb = Sandbox::new("plandense");
    install_planner(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"planner/planner-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");
    s.send(&command("planner.publish"));
    s.wait_for("published");

    // The row is the account wearing the bar of the window the provider marked active — the one
    // that would refuse the next request. Its own label waits behind `⇥`: `Session` on a strip
    // with no account row above it says whose session to nobody. And `weekly` at 40% is not
    // news, so it is not a row.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("Planner"))),
        "the account, wearing the limit that binds\n{:?}",
        s.sidebar_now()
    );
    let strip = s.sidebar_now().join("\n");
    assert!(!strip.contains("Session"), "the window's own label waits behind ⇥:\n{strip}");
    assert!(!strip.contains("Weekly"), "and a limit at 40% is not news:\n{strip}");
    assert!(!strip.contains("% left ·"), "nor is the sentence that explains it:\n{strip}");

    // Onto a plan row and `<Tab>`: every window, contributed as a verb on the rows it belongs to
    // rather than bound over every conversation in the panel.
    s.enter_panel();
    s.key("G");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("plan detail"))),
        "the key is advertised on the rows it applies to\n{:?}",
        s.sidebar_now()
    );
    s.special("tab");
    assert!(
        s.pump(|s| s.sidebar_now().join("\n").contains("Weekly")),
        "every window, one step up\n{:?}",
        s.sidebar_now()
    );
    s.special("tab");
    assert!(
        s.pump(|s| s.sidebar_now().join("\n").contains("88% left")),
        "and the account and the sentence at the top of the ladder\n{:?}",
        s.sidebar_now()
    );
    s.special("tab");
    assert!(
        s.pump(|s| !s.sidebar_now().join("\n").contains("Weekly")),
        "and round to one row again\n{:?}",
        s.sidebar_now()
    );
}

/// The provider's mark keeps the provider's colour, on a row graded by something else.
///
/// The bar is coloured by how close the limit is, which is what a bar is for — and the one thing on
/// the row that says *whose* allowance it is has a colour of its own. Two accounts are then two
/// rows you tell apart without reading a word. It needs a span rather than a row highlight, so
/// `sidebar.section` rows carry spans: a contributed row that can only be one colour is a row a
/// plugin cannot put a mark on.
#[test]
fn the_plan_row_keeps_the_providers_own_colour() {
    let sb = Sandbox::new("planbrand");
    install_planner(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"planner/planner-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");
    s.send(&command("planner.publish"));
    s.wait_for("published");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("Planner"))),
        "the strip is drawn\n{:?}",
        s.sidebar_now()
    );

    let buf = s.buffer_named("[sidebar]").expect("the panel buffer");
    let at = s
        .sidebar_now()
        .iter()
        .position(|l| l.contains("Planner"))
        .expect("the row with the binding limit on it");
    let groups = s.groups_of(buf);
    let on_row = groups.get(at).cloned().unwrap_or_default();
    assert!(
        on_row.iter().any(|g| g == "Brand.Anthropic"),
        "the mark carries the provider's group\n{on_row:?}"
    );
    assert!(
        on_row.iter().any(|g| g != "Brand.Anthropic"),
        "and the rest of the row is graded by how close the limit is\n{on_row:?}"
    );
}

/// Whose allowance it is, said on the strip.
///
/// Three bars and no name answers "how much is left" without ever saying *of what* — and on a
/// machine with a Claude plan and a Codex one it was two identical-looking stacks with nothing to
/// tell them apart. The account row carries the provider's display name and its plan, and the
/// provider here is one a plugin registered, so nothing in the sidebar or the usage plugin has
/// heard of it: the name is read off the credential list, like the model picker reads it.
#[test]
fn the_plan_strip_says_which_account_it_is_about() {
    let sb = Sandbox::new("planwho");
    install_planner(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"planner/planner-1\"\n\"usage.sidebar.style\" = \"full\"\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");

    s.send(&command("planner.publish"));
    s.wait_for("published");

    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("Planner"))),
        "the account is named, not left as an instance id\n{:?}",
        s.sidebar_now()
    );

    // What the vendor calls the plan, and the way into the panel. Both are right-aligned virtual
    // text rather than buffer content, because only the frontend knows how wide the column is.
    let virt = s.sidebar_virt().join("|");
    assert!(virt.contains("flightplan"), "the plan's own name sits on the account row:\n{virt}");
    assert!(
        virt.contains("^L"),
        "and the heading says how to see the rest of it:\n{virt}"
    );

    // The one row here that is a sentence. Every other row is a measurement whose direction you
    // have to already know: a bar that fills as an allowance is *spent* reads as "how much is left"
    // to about half of everyone. `session` is at 12% and is the window the provider marked active,
    // so the strip says 88 — the number in the direction people ask the question.
    let strip = s.sidebar_now().join("\n");
    assert!(
        strip.contains("88% left"),
        "the binding window, in words and the right way round:\n{strip}"
    );
}

/// A mid-turn report patches; it does not replace.
///
/// `claude`'s `rate_limit_event` carries exactly one window and says nothing at all about the
/// others. Treated as a snapshot it deletes every other row — so the session limit vanishes from
/// the strip the first time the weekly one moves, which is precisely when somebody is watching it.
#[test]
fn a_mid_turn_report_does_not_erase_the_other_windows() {
    let sb = Sandbox::new("planpatch");
    install_planner(&sb);
    sb.write_config(
        "[options]\n\"agent.model\" = \"planner/planner-1\"\n\"usage.sidebar.style\" = \"full\"\n",
    );
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");

    s.send(&command("planner.publish"));
    s.wait_for("published");
    assert!(s.pump(|s| s.sidebar_now().join("\n").contains("40%")), "the published weekly");

    // The turn reports the weekly window alone, at a different figure.
    s.type_text("go");
    s.enter();
    assert!(
        s.pump(|s| s.sidebar_now().join("\n").contains("93%")),
        "the driver's mid-turn figure lands\n{:?}",
        s.sidebar_now()
    );

    let strip = s.sidebar_now().join("\n");
    assert!(
        strip.contains("Session") && strip.contains("12%"),
        "and the window it said nothing about is still there:\n{strip}"
    );
    assert!(!strip.contains("40%"), "with the stale weekly figure replaced:\n{strip}");
}

/// The panel opens on its own key, with both halves in it.
///
/// Two blocks and never one axis: an opaque percentage and a token count do not convert into each
/// other, and the panel that drew them together would be inventing an exchange rate.
#[test]
fn the_usage_panel_opens_with_the_plan_above_the_history() {
    let sb = Sandbox::new("planpanel");
    install_planner(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"planner/planner-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");
    s.send(&command("planner.publish"));
    s.wait_for("published");

    s.ctrl("l");
    assert!(
        s.pump(|s| s.buffer_named("[usage]").is_some()),
        "^L opens a buffer of its own\n{}",
        s.transcript()
    );
    let buf = s.buffer_named("[usage]").expect("the panel buffer");
    // Pumped on the *plan name*, not on the heading. The heading is drawn immediately, with
    // "nothing has reported an allowance yet" under it, while the transcript scan runs — so a test
    // that stopped at the heading would be reading the panel mid-load and asserting on the
    // placeholder.
    assert!(
        s.pump(|s| s.lines_of(buf).iter().any(|l| l.contains("flightplan"))),
        "the plan's own name, where the vendor gives one\n{:?}",
        s.lines_of(buf)
    );
    let text = s.lines_of(buf).join("\n");
    assert!(text.contains("YOUR PLAN"), "the gauges are the first thing in it:\n{text}");
    assert!(text.contains("Session") && text.contains("Weekly"), "every window:\n{text}");
    assert!(
        text.contains("THE LAST"),
        "and the history under it, as a separate block:\n{text}"
    );
}

/// The panel's keys are the panel's keys, bound to named commands.
///
/// `t` and `c` are ordinary letters. Bound at buffer-kind scope they only fire here — and because
/// they point at named commands rather than at a `switch` inside a handler, `^Z` lists them and an
/// `init.ts` can move them. See ADR 0040.
#[test]
fn the_usage_panel_swaps_metric_on_its_own_key() {
    let sb = Sandbox::new("planmetric");
    install_planner(&sb);
    // A month with exactly one answer in it, so the breakdown below has a row to draw. Read off a
    // transcript this test wrote, never off the developer's own.
    sb.write_usage_history("claude-opus-4-8", 2);
    sb.write_config("[options]\n\"agent.model\" = \"planner/planner-1\"\n");
    let mut s = sb.start_letting_config_choose();
    s.wait_for("planner ready");

    s.ctrl("l");
    assert!(s.pump(|s| s.buffer_named("[usage]").is_some()), "the panel opens");
    let buf = s.buffer_named("[usage]").expect("the panel buffer");
    // Asserted on the breakdown's own figures, not on the heading: the metric is drawn there as
    // right-aligned virtual text, which is a mark rather than buffer text and so is not something
    // this harness can read back. The `$` is what a reader is actually looking at.
    assert!(
        s.pump(|s| s.lines_of(buf).iter().any(|l| l.contains("calls") && l.contains('$'))),
        "it opens on cost\n{:?}",
        s.lines_of(buf)
    );

    s.key("t");
    assert!(
        s.pump(|s| s.lines_of(buf).iter().any(|l| l.contains("calls") && !l.contains('$'))),
        "`t` is tokens\n{:?}",
        s.lines_of(buf)
    );
    s.key("c");
    assert!(
        s.pump(|s| s.lines_of(buf).iter().any(|l| l.contains("calls") && l.contains('$'))),
        "`c` is back to cost\n{:?}",
        s.lines_of(buf)
    );
}

/// A plugin may only publish for a driver it registered.
///
/// The strip is a thing people look at before deciding whether to spend an afternoon on something.
/// A figure any plugin could put under `claude-cli` is not one to draw as fact.
#[test]
fn a_plugin_cannot_publish_a_quota_for_somebody_elses_provider() {
    const LIAR: &str = r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("liar.try", async () => {
    try {
      await neosh.quota.report({
        instance: "claude-cli", plan: "free", windows: [], credits: null,
        observed_at: 1, source: "plugin",
      });
      neosh.notify("liar accepted");
    } catch (e) {
      neosh.notify(`liar refused: ${e}`);
    }
  });
  neosh.event.on("neosh.ready", () => neosh.notify("liar ready"));
}
"#;
    let sb = Sandbox::new("planliar");
    let dir = sb.root.join("config/plugins/liar");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"liar\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), LIAR).expect("plugin");

    let mut s = sb.start();
    s.wait_for("liar ready");
    s.send(&command("liar.try"));
    assert!(s.pump(|s| s.saw("liar refused")), "refused, by name\n{}", s.transcript());
    assert!(!s.saw("liar accepted"), "and not accepted\n{}", s.transcript());
}

#[test]
fn a_conversation_waiting_on_an_answer_is_marked_in_the_panel() {
    // A conversation that has asked you something looks *exactly* like one that is thinking: the
    // turn is still in flight, blocked on the hook, so the row is a spinner and the only way to
    // find it is to open conversations until one of them asks. The footer can only say how many —
    // it is one line and it is shared — so the panel is the only place that can say which.
    //
    // The two halves are both under test: the `questions` plugin writing `question.asking`, and
    // this panel reading it. They are separate plugins and neither imports the other, which is the
    // point — a question panel of your own writes the same var and this row still lights up.
    const ASKER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("t.ask", async () => {
    const answers = await neosh.ask([{
      question: "Tabs or spaces?",
      header: "Indentation",
      multi_select: false,
      options: [
        { label: "Tabs", description: "tab characters" },
        { label: "Spaces", description: "space characters" },
      ],
    }]);
    neosh.notify(`answered: ${answers === null ? "nothing" : JSON.stringify(answers)}`);
  });
  // To the other conversation if there is one, to a new one if there is not.
  await neosh.cmd.register("t.switch", async () => {
    const list = await neosh.session.list();
    const other = list.find((x) => !x.is_active);
    if (other) await neosh.session.switch(other.id);
    else await neosh.session.create({ activate: true });
  });
  neosh.event.on("neosh.ready", () => neosh.notify("asker ready"));
}
"#;
    let sb = Sandbox::new("asking-row");
    let dir = sb.root.join("config/plugins/asker");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"asker\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), ASKER).expect("plugin");

    let mut s = sb.start();
    s.wait_for("asker ready");
    s.wait_for("PROJECTS");

    s.send(&command("t.ask"));
    s.wait_for("Tabs or spaces?");
    // Away, so the question is one you cannot see. This is the whole case: on screen it is a panel
    // over the composer and nothing needs saying twice.
    s.send(&command("t.switch"));
    s.wait_for("conversation is asking");

    let marked = |s: &Session| {
        s.sidebar_now().iter().filter(|l| l.trim_start().starts_with("? ")).count()
    };
    assert!(
        s.pump(|s| marked(s) == 1),
        "one row says a question is waiting on it\n{:?}",
        s.sidebar_now()
    );
    // And in the colour the palette keeps for waiting on something outside the program — the same
    // one the footer's `Question.Waiting` links to. A glyph nobody can pick out of a column of
    // thirty is a glyph that is not there.
    let buf = s.buffer_named("[sidebar]").expect("sidebar buffer");
    assert!(
        s.groups_of(buf).iter().flatten().any(|g| g == "Status.Pending"),
        "the row is coloured, not only marked\n{:?}",
        s.groups_of(buf)
    );

    // Answering is what takes it off. Nothing to dismiss, and nothing left behind on a row that is
    // not waiting on anybody any more.
    //
    // Counted rather than waited for: the question has already been on screen once, so anything
    // scanning everything ever drawn is satisfied before the panel has come back — and the key
    // would land in the composer of the conversation being switched to.
    let drawn = |s: &Session| s.texts().iter().filter(|l| l.contains("Tabs or spaces?")).count();
    let quiet = drawn(&s);
    s.send(&command("t.switch"));
    assert!(
        s.pump(move |x| drawn(x) > quiet + 1),
        "the question is in front of you again\n{}",
        s.transcript()
    );
    s.key("2");
    s.wait_for(r#""answers":["Spaces"]"#);
    assert!(
        s.pump(|s| marked(s) == 0),
        "and the mark goes with the answer\n{:?}",
        s.sidebar_now()
    );
}

/// The row you are in says all of its title; every other row waits for the cursor.
///
/// A conversation's title is the only thing telling two of them in one project apart, and a
/// generated title is a sentence rather than a word — so a 34-column panel cuts it at about the
/// point where it was going to say which of them this is. The conversation you are *in* is not a
/// row you are considering, it is where you are, so it stays open with nothing on it. Everywhere
/// else the cursor is what asks, and leaving folds the row back — which is what keeps the column
/// something you read down.
#[test]
fn the_row_you_are_in_says_all_of_a_title_the_column_had_to_cut() {
    let sb = Sandbox::new("unfold");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

    let title = "a conversation whose title is longer than the panel it has to fit inside";
    for c in title.chars() {
        s.key(&c.to_string());
    }
    s.enter();
    s.wait_for("longer than the panel");

    // Cut, first of all — otherwise this test would pass on a panel that never had the problem.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains('…'))),
        "the title does not fit the column\n{:?}",
        s.sidebar_now()
    );
    // And open, with the cursor nowhere near it and the panel not even focused: this is the
    // conversation on screen, and abbreviating the one row you are certain about is the one place
    // the cursor rule is wrong.
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("has to fit inside"))),
        "the conversation you are in says its whole name\n{:?}",
        s.sidebar_now()
    );

    // Somewhere else. The row that was open is now an ordinary one, and goes back to a line.
    s.new_conversation();
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("has to fit inside"))),
        "a row nobody is in or on stays one row\n{:?}",
        s.sidebar_now()
    );

    // Onto it with the cursor, and it says the rest of itself again. The panel keeps the cursor on
    // the conversation you are in, and the list is newest first — so the older one is the row
    // below where the cursor lands.
    s.enter_panel();
    s.key("j");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("has to fit inside"))),
        "the row under the cursor says the rest of itself\n{:?}",
        s.sidebar_now()
    );

    // Off it again, and it folds away.
    s.key("k");
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("has to fit inside"))),
        "and folds back when the cursor leaves\n{:?}",
        s.sidebar_now()
    );
}

/// A plugin can drive a conversation it is not looking at.
///
/// The vocabulary for doing something *to* a conversation — steer it, interrupt it, re-model it,
/// start one — existed only over the swarm: `swarm.command` names a node and a session, so a
/// plugin could fan work out over every other machine and, on this one, had `agent.send`, which
/// takes no session and acts on whatever is on screen. Watching was already per-conversation, so
/// the local API could see every conversation in the workspace and drive exactly one of them.
///
/// That is the shape of an orchestrator, so it is asserted as one: two conversations are given
/// different work by id, and the screen never moves. `sessions.switch` before each message is the
/// thing this replaced — it works, and it drags the transcript out from under whoever is reading.
#[test]
fn a_plugin_can_run_a_conversation_it_is_not_looking_at() {
    const FANOUT: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("lab.fanout", async () => {
    try {
    const here = (await neosh.session.current()).id;
    // Started by name, and the answer is the one that was made — not whatever is on screen.
    const made = await neosh.agent.command({ command: "new_session", title: "over there" });
    if (!made) { neosh.notify("no session came back"); return; }
    if (made === here) { neosh.notify("it answered with the conversation on screen"); return; }
    await neosh.agent.command({ command: "send", text: "work for the second" }, made);
    await neosh.agent.command({ command: "send", text: "work for the first" }, here);
    neosh.notify("fanned out");
    } catch (e) { neosh.notify("fanout failed: " + e); }
  });
  neosh.event.on("neosh.ready", () => neosh.notify("lab ready"));
}
"#;
    let sb = Sandbox::new("fanout");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), FANOUT).expect("plugin");
    install_parrot(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"parrot/parrot\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("parrot ready");
    s.wait_for("lab ready");

    // The conversation on screen, so there is something to have stayed on.
    s.type_text("first");
    s.enter();
    assert!(s.pump(|s| s.saw("asked: first")), "the conversation on screen answers\n{}", s.transcript());

    s.send(&command("lab.fanout"));
    s.wait_for("fanned out");

    // Both turns ran. The second conversation is not on screen, so its answer is asserted through
    // the driver rather than the transcript — which is the point: nothing about it was drawn.
    assert!(
        s.pump(|s| s.saw("asked: work for the first")),
        "the conversation on screen was steered by id\n{}",
        s.transcript()
    );
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("over there"))),
        "the conversation that was started is in the list\n{:?}",
        s.sidebar_now()
    );
    // And the screen never moved: what is in the chat buffer is still the first conversation's.
    assert!(
        s.chat_now().iter().any(|l| l.contains("first")),
        "the transcript on screen is still the one that was there\n{}",
        s.transcript()
    );
    assert!(
        !s.chat_now().iter().any(|l| l.contains("work for the second")),
        "the other conversation's work was drawn into this one's transcript\n{}",
        s.transcript()
    );
}


/// A plugin decides which model answers.
///
/// Every other hook is about a turn already under way — what the prompt says, which tool may run,
/// what it cost. None of them could answer the question an orchestrator exists to answer: *who
/// should do this one*. Routing policy could only be written in Rust, in a program whose whole
/// claim is that a third party could have written the feature.
///
/// Asserted through the driver, because that is the only witness that cannot be faked by the UI:
/// the turn is sent to an instance the conversation was never pointed at.
#[test]
fn a_plugin_can_choose_which_model_answers_a_turn() {
    const ROUTER: &str = r#"
import type { PluginContext, HookPayload } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  const say = (who: string) => async (_req: unknown, emit: (e: unknown) => void) => {
    emit({ type: "message_start", model: who, usage: {} });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    emit({ type: "text_delta", index: 0, text: "answered by " + who });
    emit({ type: "block_stop", index: 0 });
    emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
    emit({ type: "message_stop" });
  };
  await neosh.provider.register("small", [{
    id: "small", driver: "small", display_name: "Small",
    models: [{ id: "small", display_name: "Small" }],
  }], say("small"));
  await neosh.provider.register("big", [{
    id: "big", driver: "big", display_name: "Big",
    models: [{ id: "big", display_name: "Big" }],
  }], say("big"));

  await neosh.hook.register("turn_route", (p: HookPayload) => {
    if (p.hook !== "turn_route") return { action: "continue" };
    if (p.text.includes("too expensive")) {
      return { action: "veto", reason: "over the budget for today" };
    }
    if (!p.text.includes("urgent")) return { action: "continue" };
    // Re-point this one turn. The conversation was never switched to it.
    return {
      action: "modify",
      payload: { ...p, selection: { instance: "big", model: "big", options: [] } },
    };
  }, { blocking: true });

  neosh.event.on("neosh.ready", () => neosh.notify("router ready"));
}
"#;
    let sb = Sandbox::new("router");
    let dir = sb.root.join("config/plugins/lab");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"lab\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\", \"hooks_blocking\"]\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), ROUTER).expect("plugin");
    sb.write_config("[options]\n\"agent.model\" = \"small/small\"\n");

    let mut s = sb.start_letting_config_choose();
    s.wait_for("router ready");

    // Ordinary work goes where the conversation points.
    s.type_text("something ordinary");
    s.enter();
    assert!(s.pump(|s| s.saw("answered by small")), "the unrouted turn\n{}", s.transcript());

    // And the router sends this one somewhere else, without the conversation being re-pointed by
    // hand and without anything on screen having been switched.
    s.type_text("this one is urgent");
    s.enter();
    assert!(s.pump(|s| s.saw("answered by big")), "the routed turn\n{}", s.transcript());

    // And a router that says no says why, *in the transcript*, under the question it refused.
    // A veto used to end the turn as `Cancelled` — the value `<Esc>` produces — so a question a
    // plugin had blocked was indistinguishable from one the person had interrupted, and the reason
    // the plugin gave lived for a few seconds in a toast and then nowhere at all.
    s.type_text("this one is too expensive");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("over the budget for today"))),
        "the reason is in the transcript\n{}",
        s.transcript()
    );
}

/// The rest of the declared permissions are enforced too.
///
/// `vcs_write` was the only one ever checked, which made the other seven documentation shaped like
/// a boundary: `neosh plugins` printed a manifest's permissions, and a plugin that declared none of
/// them could still register a tool the model calls, take a blocking hook and refuse every turn, or
/// stand in as a model provider. A workspace that runs other people's code — and, on the swarm,
/// takes commands from other machines — cannot have a permission list that is only a description.
///
/// One plugin, three attempts, nothing declared. Reading and drawing stay free, because they are no
/// more privileged than what the person sitting there can already do.
#[test]
fn a_plugin_that_declared_nothing_cannot_register_a_tool_a_provider_or_a_blocking_hook() {
    let sb = Sandbox::new("undeclared");
    let dir = sb.root.join("config/plugins/greedy");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"greedy\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("main.ts"),
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  const tried = async (what: string, f: () => Promise<unknown>) => {
    try { await f(); neosh.notify(`ALLOWED ${what}`); }
    catch (e) { neosh.notify(`refused ${what}: ${e}`); }
  };
  await tried("tool", () =>
    neosh.tool.register({ name: "t", description: "d", inputSchema: {} }, async () => ({
      content: "x", is_error: false,
    })));
  await tried("provider", () =>
    neosh.provider.register("x", [{
      id: "x", driver: "x", display_name: "X", models: [{ id: "x", display_name: "X" }],
    }], async () => {}));
  await tried("blocking hook", () =>
    neosh.hook.register("turn_pre", () => ({ action: "continue" }), { blocking: true }));
  // An observer cannot refuse anything, which is exactly what `hooks_blocking` distinguishes — so
  // this one needs nothing declared and must still work.
  await tried("observing hook", () =>
    neosh.hook.register("turn_post", () => ({ action: "continue" })));
  neosh.notify("greedy done");
}
"#,
    )
    .expect("plugin");

    let mut s = sb.start();
    s.wait_for("greedy done");
    assert!(!s.saw("ALLOWED tool"), "a tool was registered undeclared\n{}", s.transcript());
    assert!(!s.saw("ALLOWED provider"), "a provider was registered undeclared\n{}", s.transcript());
    assert!(
        !s.saw("ALLOWED blocking hook"),
        "a blocking hook was taken undeclared\n{}",
        s.transcript()
    );
    assert!(
        s.saw("ALLOWED observing hook"),
        "an observer needs nothing declared and was refused anyway\n{}",
        s.transcript()
    );
    // And each refusal says the word to put in the manifest, so the fix is one line rather than a
    // capability name and a guess about where it goes.
    for want in ["tools", "providers", "hooks_blocking"] {
        assert!(s.saw(want), "the refusal names {want}\n{}", s.transcript());
    }
}
