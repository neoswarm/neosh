//! The bundled plugins, driven through the real binary.
//!
//! These exist to keep one claim honest: the sidebar and the switchers are *plugins*, written
//! against the same API a third party has. If any of them ever needed a private path into the host,
//! it would show up here as a call these tests cannot make.
//!
//! Everything is asserted on rendered text and emitted UI events — what a user would actually see.

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
        for d in ["config", "state", "work"] {
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("neosh should start");
        Session::wrap(child)
    }
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
            self.pump(|s| s.sidebar_rows() > before),
            "a conversation was started\n{:?}",
            self.sidebar_now()
        );
    }

    /// How many conversation rows the panel is showing, however they are named.
    fn sidebar_rows(&self) -> usize {
        self.sidebar_now().iter().filter(|l| l.starts_with("     ") || l.starts_with("       ")).count()
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
  neosh.notify("other ready");
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

    // Onto it — it sorts above the provider's own models — and out.
    s.special("up");
    s.ctrl("d");
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
        s.pump(|s| s.picker_named("[Go to]").iter().any(|l| l.contains("^y choose"))),
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
fn a_model_with_no_options_says_so_instead_of_opening_an_empty_picker() {
    // The mock driver exposes no `ProviderOptionDescriptor`s, so the effort switcher has nothing to
    // offer. Opening an empty list and making the user press Escape would be worse than a message.
    let sb = Sandbox::new("effort");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("e");
    s.wait_for("has no options to set");
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
    s.wait_for("on my-own-name");

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
  neosh.notify("lab ready");
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
  neosh.notify("ladder ready");
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

    // The switcher has never heard of "vibe" and does not need to.
    s.ctrl("e");
    s.wait_for("Vibe");
    assert!(s.saw("Chill"), "choices rendered\n{}", s.transcript());
    assert!(s.saw("Intense"), "choices rendered\n{}", s.transcript());

    // Move to the second entry and take it.
    s.special("down");
    s.special("enter");
    s.wait_for("Vibe: Intense");

    // And it is really on the selection the next turn would use, not just in a message.
    s.send(&command("lab.report"));
    s.wait_for(r#"selection: lab/brainy [{"id":"vibe","value":"intense"}]"#);
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
    s.wait_for("Vibe");
    s.special("down");
    s.special("enter");
    s.wait_for("Vibe: Intense");

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
    s.wait_for("on fix/the-login");

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
  neosh.notify("staller ready");
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

/// A heart on a row, meaning the project is a favourite.
const HEART: char = '\u{2665}';

/// Project rows carrying the favourite mark.
///
/// Scoped to rows naming a project, because the hint strip also prints a heart — that is the point
/// of the hint, and matching it would make these tests pass with the feature ripped out.
fn favourited(panel: &[String], project: &str) -> bool {
    panel.iter().any(|l| l.contains(HEART) && l.contains(project))
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
            let w = p.iter().position(|l| l.contains(HEART) && l.contains("work"));
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

/// A worktree is named by the repository it belongs to and the branch it is on, not by whatever
/// the directory happens to be called.
///
/// The directory is named by whoever created it, which for a worktree made by another tool is a
/// string like `wt-fe3c0d93`. A panel of those says nothing about which checkout is which, and
/// reads as somebody else's directory having wandered into your workspace.
#[test]
fn a_worktree_is_listed_by_its_repository_and_branch() {
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
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("work \u{b7} sideline"))),
        "and the worktree says which repository and which branch\n{:?}",
        s.sidebar_now()
    );
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

#[test]
fn archiving_takes_a_conversation_out_of_the_list_and_u_brings_it_back() {
    // The everyday verb. It asks nothing, because there is nothing to lose — which is the whole
    // reason it exists beside the one that does ask.
    let sb = Sandbox::new("archive");
    let mut s = sb.start();
    s.wait_for("PROJECTS");

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
    let panel_only = s.open_windows().len();
    s.key("x");
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("ARCHIVED"))),
        "an archived section appeared\n{:?}",
        s.sidebar_now()
    );
    assert_eq!(s.open_windows().len(), panel_only, "and nothing asked");

    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("keep"))),
        "and it left the everyday list\n{:?}",
        s.sidebar_now()
    );

    // Down to the archive heading. Where the cursor landed when the row under it was archived is
    // not something this test should assert, so it steps until the hints say it has arrived — the
    // hints being contextual is exactly what makes that legible.
    let on_heading =
        |s: &Session| s.sidebar_now().iter().any(|l| l.contains("show what is put away"));
    let mut found = on_heading(&s);
    for _ in 0..6 {
        if found {
            break;
        }
        // Stepping first and waiting after: waiting on a predicate that is not yet true costs the
        // whole budget, and a test that spends thirty seconds arriving is a test people delete.
        s.key("j");
        found = s.pump(on_heading);
    }
    assert!(found, "never reached the archive heading\n{:?}", s.sidebar_now());
    s.enter();
    assert!(
        s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("keep"))),
        "unfolding the archive shows what is in it\n{:?}",
        s.sidebar_now()
    );

    // And `u` on it puts it back where it was.
    s.key("j");
    assert!(s.pump(|s| s.sidebar_now().iter().any(|l| l.contains("u unarchive"))));
    s.key("u");
    assert!(
        s.pump(|s| !s.sidebar_now().iter().any(|l| l.contains("ARCHIVED"))),
        "the section goes away with the last thing in it\n{:?}",
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
fn an_empty_conversation_is_deleted_without_being_asked_about() {
    // A dialog for something with nothing to lose is friction that teaches you to dismiss dialogs.
    let sb = Sandbox::new("nodialog");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.ctrl("n");
    assert!(s.pump(|s| {
        s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count() == 2
    }));

    s.enter_panel();
    s.key("X");
    assert!(
        s.pump(|s| {
            s.sidebar_now().iter().filter(|l| l.contains("New conversation")).count() == 1
        }),
        "gone, with nothing asked\n{:?}",
        s.sidebar_now()
    );
    assert!(!s.saw("Delete \""), "and no dialog appeared\n{}", s.transcript());
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
    s.wait_for("PROJECT PANEL");
    let keys = s.buffer_named("[keys]").expect("key list buffer");
    // The text is written before the float is opened, so seeing the text is not seeing the window.
    assert!(s.pump(|s| s.windows_for(keys).len() == 1), "it is on screen\n{}", s.transcript());

    s.key("z");
    assert!(s.pump(|s| s.windows_for(keys).is_empty()), "any key closes it\n{}", s.transcript());
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
  neosh.notify("stall ready");
}
"#;

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
  neosh.notify("half ready");
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
  neosh.notify("chatty ready");
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
  neosh.notify("editor ready");
}
"#;

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
    // `read_file` tells you nothing you did not already know. `read_file  .env` is the line you
    // actually wanted, and it costs one lookup in the input the tool was given.
    let sb = Sandbox::new("toolrow");
    let mut s = sb.start();
    s.wait_for("PROJECTS");
    s.key("h");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("read_file") && l.contains(".env"))),
        "the subject is beside the name\n{:?}",
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
    // rows below, `F1` is on the row it points at, and what the duplication actually bought was
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
        s.pump(|s| s.composer_chrome().iter().any(|t| t.contains("F1 keys"))),
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
  neosh.notify("wide ready");
}
"#;

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
  neosh.notify("watcher ready");
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
  neosh.notify("reporter ready");
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
  neosh.notify("vault ready");
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
  neosh.notify("slow ready");
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
        s.composer_chrome().iter().any(|t| t.contains("edit")),
        "with the key that undoes it\n{:?}",
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
  neosh.notify("where ready");
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
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("read_file"))),
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
            let Some(at) = rows.iter().position(|l| l.contains("read_file  .env")) else {
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
    assert!(rows.iter().any(|l| l.contains("one")), "the first line is there\n{rows:?}");
    assert!(rows.iter().any(|l| l.contains("two")), "and the second\n{rows:?}");
    assert!(
        !rows.iter().any(|l| l.contains("three")),
        "and the setting stopped it there\n{rows:?}"
    );
    assert!(
        rows.iter().any(|l| l.contains("+3 more lines")),
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
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("read_file  .env"))),
        "the call is shown with its subject\n{:?}",
        s.chat_now()
    );
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains('\u{2502}')
            && l.contains("could not read"))),
        "and what it came back with, under it\n{:?}",
        s.chat_now()
    );
}

#[test]
fn an_edit_card_shows_the_change_and_how_much_of_it() {
    // `⏺ edit(main.rs)` and "edited" underneath says that something happened. The diff says what,
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
        .position(|l| l.starts_with('\u{23fa}') && l.contains("edit  ") && l.contains("main.rs"))
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
fn a_folded_card_opens_with_tab_while_reading_and_folds_again() {
    // A card shows enough to know what happened. The whole of it is one key away, in the place
    // you go to read — and the trailer says which key.
    let sb = Sandbox::new("cardfold");
    install_editor(&sb);
    sb.write_config("[options]\n\"agent.model\" = \"editor/editor\"\n\"chat.diff_lines\" = 2\n");
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
    let trailer = rows.iter().find(|l| l.contains("more lines")).expect("a trailer");
    assert!(trailer.contains("+6 more lines"), "{trailer}");
    assert!(trailer.contains("^S \u{21e5} to expand"), "it says how: {trailer}");
    assert!(!rows.iter().any(|l| l.ends_with("+ i")), "the end of the diff is folded away\n{rows:?}");
    let header = rows.iter().position(|l| l.starts_with('\u{23fa}')).expect("the header");

    // Into the transcript, to the top, and down onto the card.
    s.ctrl("s");
    s.key("g");
    s.key("g");
    for _ in 0..header {
        s.key("j");
    }
    s.special("tab");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.ends_with("+ i"))),
        "open, the whole diff is there\n{:?}",
        s.chat_now()
    );
    let rows = s.chat_now();
    assert!(!rows.iter().any(|l| l.contains("more lines")), "and nothing is held back\n{rows:?}");
    assert!(
        rows.iter().position(|l| l.contains("changed 1 file")).unwrap() > header,
        "what was below the card is still below it\n{rows:?}"
    );

    s.special("tab");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("+6 more lines"))),
        "and it folds again\n{:?}",
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
    let mut s = sb.start();
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
