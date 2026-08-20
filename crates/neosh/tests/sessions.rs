//! Conversations, end to end through the real binary.
//!
//! A workspace holds several conversations at once and must not lose any of them: switching parks
//! one and restores another, closing lands you somewhere sensible, and quitting and starting again
//! puts you back where you were.

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
        let root = std::env::temp_dir().join(format!("neosh-sess-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["config", "state", "work", "other"] {
            std::fs::create_dir_all(root.join(d)).expect("sandbox dirs");
        }
        Self { root }
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    /// Bundled plugins are off: these tests are about the session layer, and a sidebar redrawing on
    /// a timer only adds noise to the transcript they assert on.
    fn start(&self) -> Session {
        std::fs::write(
            self.root.join("config/config.toml"),
            "[options]\n\"plugins.disabled\" = [\"sidebar\", \"model\", \"git\"]\n",
        )
        .expect("write config");
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

    fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.send(&format!(
                r#"{{"type":"key","key":{{"code":{{"kind":"char","c":"{ch}"}},"mods":{{}}}}}}"#
            ));
        }
    }

    fn enter(&mut self) {
        self.send(r#"{"type":"key","key":{"code":{"kind":"enter"},"mods":{}}}"#);
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

    fn saw(&self, needle: &str) -> bool {
        self.texts().iter().any(|l| l.contains(needle))
    }

    /// The chat buffer as it stands now, by folding every splice the way a frontend does.
    ///
    /// Asserting on "was this text ever emitted" would pass for a switch that never cleared the
    /// previous conversation, which is exactly the bug worth catching.
    fn chat_now(&self) -> Vec<String> {
        let chat = self.events.iter().find_map(|e| {
            (e["type"] == "buffer_opened"
                && e["name"].as_str().is_some_and(|n| n.starts_with("[chat]")))
            .then(|| e["buf"].as_u64())?
        });
        let Some(chat) = chat else { return Vec::new() };
        let mut lines: Vec<String> = Vec::new();
        for e in &self.events {
            if e["type"] != "buffer_lines" || e["buf"].as_u64() != Some(chat) {
                continue;
            }
            let start = e["start"].as_i64().unwrap_or(0);
            let old_end = e["old_end"].as_i64().unwrap_or(start);
            let new: Vec<String> = e["lines"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l["text"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let s = start.clamp(0, lines.len() as i64) as usize;
            let en = if old_end < 0 { lines.len() } else { (old_end as usize).clamp(s, lines.len()) };
            lines.splice(s..en, new);
        }
        lines
    }

    fn transcript(&self) -> String {
        let mut s = String::from("--- text seen ---\n");
        for l in self.texts() {
            s.push_str(&format!("  {l}\n"));
        }
        s
    }

    fn exits_within(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            let _ = self.lines.recv_timeout(Duration::from_millis(50));
        }
        false
    }
}

/// A plugin that drives the session API and reports what it sees, so every assertion is on
/// something a plugin could actually observe.
const DRIVER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("t.new", async () => {
    await neosh.session.create();
    neosh.notify("created");
  });
  await neosh.cmd.register("t.newElsewhere", async () => {
    const s = await neosh.session.create({ cwd: CWD_OTHER, title: "Elsewhere" });
    neosh.notify(`created at ${s.cwd}`);
  });
  await neosh.cmd.register("t.oldest", async () => {
    const all = await neosh.session.list();
    const oldest = all[all.length - 1];
    if (!oldest) return;
    try {
      await neosh.session.switch(oldest.id);
    } catch (e) {
      neosh.notify(`switch failed: ${e}`);
    }
  });
  await neosh.cmd.register("t.other", async () => {
    const all = await neosh.session.list();
    const other = all.find((s) => !s.is_active);
    if (!other) { neosh.notify("nowhere else to go"); return; }
    try {
      await neosh.session.switch(other.id);
      neosh.notify(`switched to ${other.label}`);
    } catch (e) {
      neosh.notify(`switch failed: ${e}`);
    }
  });
  await neosh.cmd.register("t.list", async () => {
    const all = await neosh.session.list();
    neosh.notify(`sessions: ${all.map((s) => `${s.is_active ? "*" : ""}${s.label}`).join(" | ")}`);
  });
  await neosh.cmd.register("t.closeActive", async () => {
    const cur = await neosh.session.current();
    try {
      await neosh.session.close(cur.id);
      neosh.notify("closed");
    } catch (e) {
      neosh.notify(`close failed: ${e}`);
    }
  });
  await neosh.cmd.register("t.listAll", async () => {
    const all = await neosh.session.list({ includeArchived: true });
    neosh.notify(
      `all: ${all.map((s) => `${s.archived ? "~" : ""}${s.is_active ? "*" : ""}${s.label}`).join(" | ")}`,
    );
  });
  await neosh.cmd.register("t.archiveOldest", async () => {
    const all = await neosh.session.list();
    const oldest = all[all.length - 1];
    if (!oldest) return;
    try {
      await neosh.session.archive(oldest.id);
      neosh.notify("archived");
    } catch (e) {
      neosh.notify(`archive failed: ${e}`);
    }
  });
  await neosh.cmd.register("t.unarchiveAll", async () => {
    for (const s of await neosh.session.list({ includeArchived: true })) {
      if (s.archived) await neosh.session.archive(s.id, false);
    }
    neosh.notify("unarchived");
  });
  await neosh.cmd.register("t.archiveActive", async () => {
    const cur = await neosh.session.current();
    try {
      await neosh.session.archive(cur.id);
      neosh.notify("archived the active one");
    } catch (e) {
      neosh.notify(`archive failed: ${e}`);
    }
  });
  await neosh.cmd.register("t.rename", async () => {
    const cur = await neosh.session.current();
    await neosh.session.rename(cur.id, "Renamed thread");
    neosh.notify("renamed");
  });
  neosh.notify("driver ready");
}
"#;

fn install_driver(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/driver");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"driver\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    let other = sb.root.join("other").display().to_string();
    std::fs::write(dir.join("main.ts"), DRIVER.replace("CWD_OTHER", &format!("{other:?}")))
        .expect("plugin");
}

#[test]
fn a_fresh_start_has_exactly_one_conversation() {
    let sb = Sandbox::new("one");
    install_driver(&sb);
    let mut s = sb.start();
    s.wait_for("driver ready");
    s.command("t.list");
    s.wait_for("sessions: *New conversation");
}

#[test]
fn switching_parks_one_conversation_and_restores_the_other() {
    let sb = Sandbox::new("switch");
    install_driver(&sb);
    let mut s = sb.start();
    s.wait_for("driver ready");

    s.type_text("first question");
    s.enter();
    s.wait_for("first question");

    s.command("t.new");
    s.wait_for("created");
    assert!(
        s.pump(|s| !s.chat_now().iter().any(|l| l.contains("first question"))),
        "a new conversation starts empty\n{:?}",
        s.chat_now()
    );

    s.type_text("second question");
    s.enter();
    s.wait_for("second question");

    s.command("t.oldest");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("first question"))),
        "the parked conversation came back\n{:?}",
        s.chat_now()
    );
    assert!(
        !s.chat_now().iter().any(|l| l.contains("second question")),
        "and the other one's text is not mixed into it\n{:?}",
        s.chat_now()
    );
}

#[test]
fn the_list_is_most_recently_used_first_and_labelled_by_the_first_message() {
    let sb = Sandbox::new("labels");
    install_driver(&sb);
    let mut s = sb.start();
    s.wait_for("driver ready");

    s.type_text("about the parser");
    s.enter();
    s.wait_for("about the parser");
    s.command("t.new");
    s.wait_for("created");
    s.type_text("about the renderer");
    s.enter();
    s.wait_for("about the renderer");

    s.command("t.list");
    s.wait_for("sessions: *about the renderer | about the parser");
}

#[test]
fn renaming_replaces_the_first_message_label() {
    let sb = Sandbox::new("rename");
    install_driver(&sb);
    let mut s = sb.start();
    s.wait_for("driver ready");
    s.type_text("something");
    s.enter();
    s.wait_for("something");

    s.command("t.rename");
    s.wait_for("renamed");
    s.command("t.list");
    s.wait_for("sessions: *Renamed thread");
}

#[test]
fn a_conversation_in_another_directory_is_how_a_second_project_appears() {
    let sb = Sandbox::new("project");
    install_driver(&sb);
    let mut s = sb.start();
    s.wait_for("driver ready");
    s.command("t.newElsewhere");
    // The cwd is what a sidebar groups by, so it has to survive the round trip.
    s.wait_for("/other");
}

#[test]
fn closing_the_last_conversation_is_refused() {
    // There is always somewhere for the next thing you type.
    let sb = Sandbox::new("last");
    install_driver(&sb);
    let mut s = sb.start();
    s.wait_for("driver ready");
    s.command("t.closeActive");
    s.wait_for("close failed:");
}

#[test]
fn closing_the_active_conversation_lands_on_another_one() {
    let sb = Sandbox::new("close");
    install_driver(&sb);
    let mut s = sb.start();
    s.wait_for("driver ready");

    s.type_text("keep me");
    s.enter();
    s.wait_for("keep me");
    s.command("t.new");
    s.wait_for("created");

    s.command("t.closeActive");
    s.wait_for("closed");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("keep me"))),
        "landed back in the surviving conversation\n{:?}",
        s.chat_now()
    );
}

#[test]
fn an_archived_conversation_leaves_the_list_without_leaving_the_store() {
    let sb = Sandbox::new("archived");
    install_driver(&sb);
    let mut s = sb.start();
    s.wait_for("driver ready");

    s.type_text("put me away");
    s.enter();
    s.wait_for("put me away");
    s.command("t.new");
    s.wait_for("created");

    s.command("t.archiveOldest");
    s.wait_for("archived");
    s.command("t.list");
    s.wait_for("sessions: *New conversation");
    s.command("t.listAll");
    s.wait_for("~put me away");

    s.command("t.unarchiveAll");
    s.wait_for("unarchived");
    s.command("t.list");
    // Back, and at the top: you unarchived it in order to look at it.
    s.wait_for("sessions: put me away | *New conversation");
}

#[test]
fn archiving_the_only_conversation_lands_you_in_a_fresh_one_rather_than_nowhere() {
    // The store refuses to leave you with nothing; the host answers by starting something empty,
    // because "there is nowhere to go" is a bad answer to "I am done with this".
    let sb = Sandbox::new("archivelast");
    install_driver(&sb);
    let mut s = sb.start();
    s.wait_for("driver ready");

    s.type_text("the only one");
    s.enter();
    s.wait_for("the only one");

    s.command("t.archiveActive");
    s.wait_for("archived the active one");
    s.command("t.list");
    s.wait_for("sessions: *New conversation");
    s.command("t.listAll");
    s.wait_for("~the only one");
}

#[test]
fn archiving_survives_a_restart() {
    let sb = Sandbox::new("archiverestart");
    install_driver(&sb);
    {
        let mut s = sb.start();
        s.wait_for("driver ready");
        s.type_text("shelved");
        s.enter();
        s.wait_for("shelved");
        s.command("t.new");
        s.wait_for("created");
        s.command("t.archiveOldest");
        s.wait_for("archived");
        s.command("quit");
        assert!(s.exits_within(Duration::from_secs(10)), "clean exit");
    }

    let mut s = sb.start();
    s.wait_for("driver ready");
    // Not restored *into*: landing in the conversation you deliberately put away would make
    // archiving feel like it had not worked.
    assert!(
        s.pump(|s| !s.chat_now().iter().any(|l| l.contains("shelved"))),
        "started somewhere else\n{:?}",
        s.chat_now()
    );
    s.command("t.listAll");
    s.wait_for("~shelved");
}

#[test]
fn conversations_survive_a_restart() {
    let sb = Sandbox::new("restart");
    install_driver(&sb);
    {
        let mut s = sb.start();
        s.wait_for("driver ready");
        s.type_text("remember this");
        s.enter();
        s.wait_for("remember this");
        s.command("t.new");
        s.wait_for("created");
        s.type_text("and this");
        s.enter();
        s.wait_for("and this");
        s.command("quit");
        assert!(s.exits_within(Duration::from_secs(10)), "clean exit");
    }

    let mut s = sb.start();
    s.wait_for("driver ready");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("and this"))),
        "the conversation we left in came back\n{:?}",
        s.chat_now()
    );
    s.command("t.list");
    s.wait_for("sessions: *and this | remember this");
}

#[test]
fn clean_neither_reads_nor_writes_conversations() {
    let sb = Sandbox::new("clean");
    install_driver(&sb);
    {
        let mut s = sb.start();
        s.wait_for("driver ready");
        s.type_text("written");
        s.enter();
        s.wait_for("written");
        s.command("quit");
        assert!(s.exits_within(Duration::from_secs(10)));
    }
    assert!(sb.root.join("state/sessions").is_dir(), "the first run did save");

    let child = Command::new(env!("CARGO_BIN_EXE_neosh"))
        .args(["--ui-protocol", "stdio", "--clean"])
        .arg("--cwd")
        .arg(sb.work())
        .args(["--mock-script", &fixture().display().to_string()])
        .args(["--model", "mock/mock"])
        .env("NEOSH_STATE_DIR", sb.root.join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start");
    let mut s = Session::wrap(child);
    s.wait_for("--clean");
    s.drain_for(Duration::from_secs(2));
    assert!(
        !s.chat_now().iter().any(|l| l.contains("written")),
        "--clean must not restore history\n{:?}",
        s.chat_now()
    );
}

/// A conversation with a model that says one word and then never finishes, and a second one that
/// answers straight away. Between them they can prove a turn is a property of a conversation.
fn install_slow(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/slow");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"slow\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\npermissions = [\"providers\"]\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("main.ts"),
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  await neosh.provider.register("slow", [{
    id: "slow", driver: "slow", display_name: "Slow",
    models: [{ id: "slow", display_name: "Slow" }, { id: "quick", display_name: "Quick" }],
  }], async (req, emit) => {
    emit({ type: "message_start", model: req.selection.model, usage: {} });
    emit({ type: "block_start", index: 0, block: { kind: "text" } });
    if (req.selection.model === "quick") {
      emit({ type: "text_delta", index: 0, text: "answered right away" });
      emit({ type: "block_stop", index: 0 });
      emit({ type: "message_delta", stop_reason: { kind: "end_turn" }, usage: {} });
      emit({ type: "message_stop" });
      return;
    }
    // Says one word and then never finishes, so the turn is reliably still in flight.
    emit({ type: "text_delta", index: 0, text: "thinking" });
  });
  await neosh.cmd.register("t.useSlow", async () => {
    await neosh.agent.setSelection({ instance: "slow", model: "slow", options: [] });
    neosh.notify("using slow");
  });
  await neosh.cmd.register("t.useQuick", async () => {
    await neosh.agent.setSelection({ instance: "slow", model: "quick", options: [] });
    neosh.notify("using quick");
  });
  neosh.notify("slow ready");
}
"#,
    )
    .expect("plugin");
}

#[test]
fn switching_away_from_a_running_turn_leaves_it_running_where_it_belongs() {
    // Switching used to be refused while a turn ran, to stop one conversation's output landing in
    // another's transcript. The refusal was the wrong half of the fix: route the output, and
    // switching is just switching.
    let sb = Sandbox::new("switch-busy");
    install_driver(&sb);
    install_slow(&sb);

    let mut s = sb.start();
    s.wait_for("driver ready");
    s.wait_for("slow ready");
    s.command("t.new");
    s.wait_for("created");
    s.command("t.useSlow");
    s.wait_for("using slow");

    s.type_text("hang please");
    s.enter();
    s.wait_for("thinking");

    s.command("t.other");
    s.wait_for("switched to");
    assert!(
        s.pump(|s| !s.chat_now().iter().any(|l| l.contains("hang please"))),
        "the other conversation is not asked the question\n{:?}",
        s.chat_now()
    );
    assert!(
        !s.chat_now().iter().any(|l| l.contains("thinking")),
        "and does not receive its answer either\n{:?}",
        s.chat_now()
    );

    s.command("t.other");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("thinking"))),
        "coming back puts you in the middle of the answer, not before it\n{:?}",
        s.chat_now()
    );
    assert!(
        s.chat_now().iter().any(|l| l.contains("hang please")),
        "with the question that started it\n{:?}",
        s.chat_now()
    );
}

#[test]
fn a_second_conversation_answers_while_the_first_is_still_working() {
    // The point of the whole change: a model that never finishes stops one conversation, not the
    // workspace.
    let sb = Sandbox::new("concurrent");
    install_driver(&sb);
    install_slow(&sb);

    let mut s = sb.start();
    s.wait_for("driver ready");
    s.wait_for("slow ready");
    s.command("t.new");
    s.wait_for("created");
    s.command("t.useSlow");
    s.wait_for("using slow");
    s.type_text("hang please");
    s.enter();
    s.wait_for("thinking");

    s.command("t.other");
    s.wait_for("switched to");
    s.command("t.useQuick");
    s.wait_for("using quick");
    s.type_text("and this one?");
    s.enter();
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("answered right away"))),
        "the second conversation got its answer with the first still in flight\n{:?}",
        s.chat_now()
    );

    s.command("t.other");
    assert!(
        s.pump(|s| s.chat_now().iter().any(|l| l.contains("thinking"))),
        "and the first is where it was left\n{:?}",
        s.chat_now()
    );
    assert!(
        !s.chat_now().iter().any(|l| l.contains("answered right away")),
        "with none of the other one's answer in it\n{:?}",
        s.chat_now()
    );
}

// ---------------------------------------------------------------------------
// Worktrees, and git following the conversation
// ---------------------------------------------------------------------------

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

impl Sandbox {
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repository in `work/` with a second worktree beside it.
    fn repo_with_worktree(&self) -> PathBuf {
        let work = self.work();
        self.git(&work, &["init", "--initial-branch=trunk"]);
        self.git(&work, &["config", "user.email", "t@neosh.invalid"]);
        self.git(&work, &["config", "user.name", "t"]);
        self.git(&work, &["config", "commit.gpgsign", "false"]);
        std::fs::write(work.join("README.md"), "hello\n").expect("write");
        self.git(&work, &["add", "."]);
        self.git(&work, &["commit", "-m", "initial"]);

        let side = self.root.join("side");
        self.git(&work, &["worktree", "add", "-b", "side", &side.display().to_string()]);
        side
    }
}

/// Reports which branch `neosh.git` thinks it is on, so the assertion is on what a plugin sees.
const REPORTER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("t.where", async () => {
    const cur = await neosh.session.current();
    try {
      const st = await neosh.git.status();
      neosh.notify(`branch=${st.repo.branch} cwd=${cur.cwd}`);
    } catch (e) {
      neosh.notify(`no repo at ${cur.cwd}`);
    }
  });
  await neosh.cmd.register("t.openSide", async () => {
    await neosh.session.create({ cwd: SIDE });
    neosh.notify("opened side");
  });
  await neosh.cmd.register("t.worktrees", async () => {
    const t = await neosh.git.worktrees();
    neosh.notify(`worktrees: ${t.map((w) => `${w.is_current ? "*" : ""}${w.branch}`).join(" ")}`);
  });
  neosh.notify("reporter ready");
}
"#;

fn install_reporter(sb: &Sandbox, side: &Path) {
    let dir = sb.root.join("config/plugins/reporter");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"reporter\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("main.ts"),
        REPORTER.replace("SIDE", &format!("{:?}", side.display().to_string())),
    )
    .expect("plugin");
}

#[test]
fn git_answers_about_the_worktree_the_conversation_is_in() {
    if !have_git() {
        return;
    }
    // The whole point of a worktree is being able to *be* in it. A conversation carries its own
    // directory, so switching conversation switches which tree `neosh.git` reports on — without
    // that, a worktree is a directory you happen to have created and nothing more.
    let sb = Sandbox::new("worktree");
    let side = sb.repo_with_worktree();
    install_reporter(&sb, &side);

    let mut s = sb.start();
    s.wait_for("reporter ready");

    s.command("t.where");
    s.wait_for("branch=trunk");

    s.command("t.openSide");
    s.wait_for("opened side");
    s.command("t.where");
    s.wait_for("branch=side");
}

#[test]
fn the_worktree_list_marks_the_one_you_are_in() {
    if !have_git() {
        return;
    }
    let sb = Sandbox::new("wtlist");
    let side = sb.repo_with_worktree();
    install_reporter(&sb, &side);

    let mut s = sb.start();
    s.wait_for("reporter ready");
    s.command("t.openSide");
    s.wait_for("opened side");
    s.command("t.worktrees");
    s.wait_for("*side");
}

#[test]
fn a_conversation_pointed_at_nowhere_is_refused() {
    // Silently falling back to the startup directory would put the conversation somewhere the user
    // did not ask for, and they would only find out when the agent edited the wrong repository.
    let sb = Sandbox::new("nodir");
    let dir = sb.root.join("config/plugins/bad");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"bad\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("main.ts"),
        r#"
import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  try {
    await neosh.session.create({ cwd: "/definitely/not/here" });
    neosh.notify("CREATED ANYWAY");
  } catch (e) {
    neosh.notify(`refused: ${e}`);
  }
}
"#,
    )
    .expect("plugin");

    let mut s = sb.start();
    s.wait_for("refused:");
    assert!(!s.saw("CREATED ANYWAY"), "{}", s.transcript());
}

#[test]
fn two_names_for_one_directory_do_not_become_two_projects() {
    // `/tmp/x/./` and `/tmp/x` are the same place. Grouping by the literal string would show two
    // projects that are one, and the sidebar would look broken for a reason nobody could see.
    let sb = Sandbox::new("canon");
    let dir = sb.root.join("config/plugins/dup");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"dup\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    let work = sb.work();
    std::fs::write(
        dir.join("main.ts"),
        format!(
            r#"
import type * as api from "@neosh/api";
export async function activate({{ neosh }}: api.PluginContext) {{
  await neosh.session.create({{ cwd: {a:?} }});
  await neosh.session.create({{ cwd: {b:?} }});
  const dirs = new Set((await neosh.session.list()).map((s) => s.cwd));
  neosh.notify(`projects: ${{dirs.size}}`);
}}
"#,
            a = work.display().to_string(),
            b = format!("{}/.", work.display()),
        ),
    )
    .expect("plugin");

    let mut s = sb.start();
    // Two new conversations plus the original, all in the same directory.
    s.wait_for("projects: 1");
}

// ---------------------------------------------------------------------------
// Titles
// ---------------------------------------------------------------------------

impl Sandbox {
    /// Start with the bundled plugins on, and a recorded turn plus a recorded title.
    fn start_with_titles(&self, extra_config: &str) -> Session {
        // Unlike `start`, the bundled plugins stay on: the sidebar is what shows a title, and the
        // `titles` plugin is what writes one.
        std::fs::write(self.root.join("config/config.toml"), extra_config).expect("config");
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/titled_turn.jsonl");
        let child = Command::new(env!("CARGO_BIN_EXE_neosh"))
            .args(["--ui-protocol", "stdio"])
            .arg("--config-dir")
            .arg(self.root.join("config"))
            .arg("--cwd")
            .arg(self.work())
            .args(["--mock-script", &script.display().to_string()])
            .args(["--model", "mock/mock"])
            .env("NEOSH_STATE_DIR", self.root.join("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start");
        Session::wrap(child)
    }

    fn sidebar_saw(&self, s: &Session, needle: &str) -> bool {
        s.texts().iter().any(|l| l.contains(needle))
    }
}

#[test]
fn a_conversation_is_named_after_its_first_exchange() {
    // A thread list is only navigable if the rows say what they are, and the first message is often
    // "have a look at this" while the conversation turns out to be about something else.
    let sb = Sandbox::new("title");
    let mut s = sb.start_with_titles("");
    s.wait_for("PROJECTS");

    s.type_text("what is 2+2?");
    s.enter();
    s.wait_for("Four.");

    // The second recorded turn is the title request; the sidebar row changes to its answer.
    // Matched on a prefix, because the panel clips a row to its width — asserting on the whole
    // title would be asserting on the sidebar's width rather than on the title.
    s.wait_for("Simple arithmetic");
    assert!(sb.sidebar_saw(&s, "Simple arithmetic"));
}

#[test]
fn autotitling_can_be_turned_off() {
    let sb = Sandbox::new("notitle");
    let mut s = sb.start_with_titles("[options]\n\"session.autotitle\" = false\n");
    s.wait_for("PROJECTS");

    s.type_text("what is 2+2?");
    s.enter();
    s.wait_for("Four.");
    s.drain_for(Duration::from_secs(2));
    assert!(!s.saw("Simple arithmetic"), "no title was asked for\n{}", s.transcript());
    // The label falls back to the first message, which is a better placeholder than "Untitled".
    assert!(s.saw("what is 2+2?"), "{}", s.transcript());
}

#[test]
fn a_title_you_set_yourself_is_not_overwritten() {
    let sb = Sandbox::new("mytitle");
    install_driver(&sb);
    let mut s = sb.start_with_titles("");
    s.wait_for("driver ready");

    s.command("t.rename");
    s.wait_for("renamed");

    s.type_text("what is 2+2?");
    s.enter();
    s.wait_for("Four.");
    s.drain_for(Duration::from_secs(2));
    assert!(
        !s.saw("Simple arithmetic"),
        "a title someone set by hand is theirs\n{}",
        s.transcript()
    );
}
