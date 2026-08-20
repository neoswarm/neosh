//! The user configuration, end to end through the real binary.
//!
//! `init.ts` is not a special case in the host — it is a plugin with a reserved name, loaded first.
//! These tests prove the parts of that claim that are easy to get subtly wrong: that it runs before
//! plugin discovery, that `rtp.add` from it actually decides what loads, that the declarative and
//! imperative layers reach the same options, and that a repository you cloned cannot run code.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

const BUDGET: Duration = Duration::from_secs(30);

/// A scratch config directory plus workspace, torn down on drop.
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
        let root = std::env::temp_dir().join(format!("neosh-usercfg-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["config", "state", "work"] {
            std::fs::create_dir_all(root.join(d)).expect("sandbox dirs");
        }
        Self { root }
    }

    fn config(&self) -> PathBuf {
        self.root.join("config")
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn write(&self, rel: &str, contents: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().expect("has parent")).expect("mkdir");
        std::fs::write(p, contents).expect("write");
    }

    /// A plugin directory with a manifest, ready to be picked up by discovery.
    fn write_plugin(&self, rel: &str, name: &str, body: &str) {
        self.write(&format!("{rel}/{name}/main.ts"), body);
        self.write(
            &format!("{rel}/{name}/plugin.toml"),
            &format!("name = \"{name}\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n"),
        );
    }

    fn start(&self) -> Session {
        let child = Command::new(env!("CARGO_BIN_EXE_neosh"))
            .args(["--ui-protocol", "stdio"])
            .arg("--config-dir")
            .arg(self.config())
            .arg("--cwd")
            .arg(self.work())
            // No provider: these tests are about configuration, not turns.
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
    /// Lines arrive on a channel rather than being read inline.
    ///
    /// A blocking `read_line` on the child's stdout ignores the deadline entirely: if neosh stops
    /// producing output the test hangs forever instead of failing with a transcript. Reading on a
    /// thread makes the timeout real, which is the difference between a diagnosable failure and a
    /// CI job that has to be killed.
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

    fn key_char(&mut self, c: char) {
        self.send(&format!(
            r#"{{"type":"key","key":{{"code":{{"kind":"char","c":"{c}"}},"mods":{{}}}}}}"#
        ));
    }

    fn read_until(&mut self, what: &str, pred: impl Fn(&Session) -> bool) {
        let deadline = Instant::now() + BUDGET;
        loop {
            if pred(self) {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        self.events.push(v);
                    }
                }
                // The child exited, or the deadline passed. Either way, report what was seen.
                Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout) => break,
            }
        }
        if !pred(self) {
            panic!("timed out waiting for {what}\n{}", self.transcript());
        }
    }

    /// Every line of text the UI has shown, plus every transient message.
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

    /// Collect events for a fixed period. Unlike `read_until`, quiet is a valid outcome — this is
    /// for asserting that something *stopped* happening.
    fn drain_for(&mut self, how_long: Duration) {
        let deadline = Instant::now() + how_long;
        while let Ok(line) = self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                self.events.push(v);
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }

    fn wait_for(&mut self, needle: &str) {
        let want = needle.to_string();
        self.read_until(&want.clone(), move |s| s.saw(&want));
    }

    fn transcript(&self) -> String {
        let mut s = String::from("--- text seen ---\n");
        for l in self.texts() {
            s.push_str(&format!("  {l}\n"));
        }
        s
    }

    /// Whether the process ends within `budget`.
    ///
    /// Checked by waiting on the child rather than on the stream: a frontend that stopped emitting
    /// looks identical to one that exited, and only one of those is quitting.
    fn exits_within(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {}
                Err(_) => return false,
            }
            // Keep reading, both so the pipe cannot fill and stall the shutdown we are waiting
            // for, and so the events emitted on the way out are still assertable afterwards.
            if let Ok(line) = self.lines.recv_timeout(Duration::from_millis(50))
                && let Ok(v) = serde_json::from_str::<Value>(&line)
            {
                self.events.push(v);
            }
        }
        false
    }

    fn signal(&mut self, sig: i32) {
        // Safe: the pid is one this process spawned and has not reaped.
        let pid = self.child.id() as i32;
        unsafe {
            libc_kill(pid, sig);
        }
    }

    fn saw_shutdown(&self) -> bool {
        self.events.iter().any(|e| e["type"] == "shutdown")
    }
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// Report progress by writing into a buffer, which is what the assertions read. Same trick as the
/// hello-plugin test: everything asserted on is something a user could see.
const REPORTER: &str = r#"
import type { PluginContext } from "@neosh/api";
export async function report({ neosh }: PluginContext, line: string) {
  const buf = await neosh.buf.create({ name: "[report]", scratch: true });
  await neosh.buf.setLines(buf, 0, -1, [line]);
}
"#;

fn init_ts(body: &str) -> String {
    format!(
        r#"import type {{ PluginContext }} from "@neosh/api";
{REPORTER}
export async function activate(ctx: PluginContext) {{
  const neosh = ctx.neosh;
{body}
}}
"#
    )
}

#[test]
fn init_ts_runs_at_startup_with_the_full_api() {
    let s = Sandbox::new("runs");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  await neosh.cmd.register("cfg.hello", () => {}, { desc: "from init.ts" });
  await neosh.keymap.set("chat", "<C-y>", "cfg.hello");
  await report(ctx, "init.ts ran");"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("init.ts ran");
}

#[test]
fn config_toml_and_init_ts_reach_the_same_options() {
    // The declarative and imperative layers are two spellings of one mechanism. If they were two
    // mechanisms, every option would need implementing twice and one of them would rot.
    let s = Sandbox::new("options");
    s.write("config/config.toml", "[options]\n\"chat.show_thinking\" = true\n");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  const fromToml = await neosh.opt.get<boolean>("chat.show_thinking");
  await neosh.opt.set("chat.show_tools", false);
  const entry = await neosh.opt.entry("chat.show_tools");
  await report(ctx, `toml=${fromToml} set=${entry?.value} modified=${entry?.modified}`);"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("toml=true set=false modified=true");
}

#[test]
fn a_misspelled_option_is_reported_rather_than_ignored() {
    // The failure mode a config file must not have: looking like it worked.
    let s = Sandbox::new("typo");
    s.write("config/config.toml", "[options]\n\"chat.show_thinkng\" = true\n");
    let mut sess = s.start();
    sess.wait_for("chat.show_thinkng");
    assert!(
        sess.saw("not declared") || sess.saw("did you mean"),
        "the user must be told why nothing happened\n{}",
        sess.transcript()
    );
}

#[test]
fn init_ts_decides_which_plugins_load() {
    // The reason config activates before discovery. Without this ordering `rtp.add` is decoration.
    let s = Sandbox::new("rtp");
    s.write_plugin(
        "elsewhere",
        "late",
        r#"import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  const buf = await neosh.buf.create({ name: "[late]", scratch: true });
  await neosh.buf.setLines(buf, 0, -1, ["plugin from rtp.add loaded"]);
}
"#,
    );
    s.write(
        "config/init.ts",
        &init_ts(&format!(
            r#"  await neosh.rtp.add({:?});
  await report(ctx, "rtp configured");"#,
            s.root.join("elsewhere").display().to_string()
        )),
    );

    let mut sess = s.start();
    sess.wait_for("plugin from rtp.add loaded");
}

#[test]
fn a_plugins_own_option_is_settable_from_config_toml() {
    // The value is written before the plugin that declares it exists. Holding it until the
    // declaration arrives is what lets a user configure a plugin in the declarative file at all.
    let s = Sandbox::new("deferred");
    s.write_plugin(
        "config/plugins",
        "greeter",
        r#"import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  const buf = await neosh.buf.create({ name: "[greeter]", scratch: true });
  // The held value arrives after activation, as an ordinary option change — which is also how a
  // plugin would learn about the setting being changed later.
  neosh.opt.onChange((e) => {
    if (e.name === "greeter.greeting") {
      void neosh.buf.setLines(buf, 0, -1, [`greeting=${e.value}`]);
    }
  });
  await neosh.opt.declare({
    name: "greeter.greeting",
    type: { type: "str" },
    default: "hello",
  });
}
"#,
    );
    s.write("config/config.toml", "[options]\n\"greeter.greeting\" = \"bonjour\"\n");

    let mut sess = s.start();
    sess.wait_for("greeting=bonjour");
}

#[test]
fn a_broken_init_ts_does_not_stop_neosh_from_starting() {
    // Locking someone out of their editor because of a typo in their config is unforgivable: the
    // editor is how they would fix it.
    let s = Sandbox::new("broken");
    s.write("config/init.ts", "export function activate() { throw new Error('boom'); }\n");
    s.write_plugin(
        "config/plugins",
        "survivor",
        r#"import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  const buf = await neosh.buf.create({ name: "[survivor]", scratch: true });
  await neosh.buf.setLines(buf, 0, -1, ["plugins still loaded"]);
}
"#,
    );

    let mut sess = s.start();
    sess.wait_for("boom");
    // And startup continued past the failure rather than stalling on it.
    sess.wait_for("plugins still loaded");
}

#[test]
fn keys_bound_in_init_ts_actually_fire() {
    let s = Sandbox::new("keymap");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  const buf = await neosh.buf.create({ name: "[k]", scratch: true });
  await neosh.cmd.register("cfg.mark", async () => {
    await neosh.buf.setLines(buf, 0, -1, ["key handler ran"]);
  });
  await neosh.keymap.set("chat", "<C-y>", "cfg.mark");
  await report(ctx, "keymap set");"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("keymap set");
    sess.send(r#"{"type":"key","key":{"code":{"kind":"char","c":"y"},"mods":{"ctrl":true}}}"#);
    sess.wait_for("key handler ran");
}

// ---- project configuration ------------------------------------------------

#[test]
fn an_untrusted_project_init_is_not_run() {
    // A repository you cloned must not execute code on `cd`. This is the whole reason for trust.
    let s = Sandbox::new("untrusted");
    s.write(
        "work/.neosh/init.ts",
        r#"import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  const buf = await neosh.buf.create({ name: "[evil]", scratch: true });
  await neosh.buf.setLines(buf, 0, -1, ["PROJECT CODE RAN"]);
}
"#,
    );

    let mut sess = s.start();
    sess.wait_for("neosh trust");
    assert!(!sess.saw("PROJECT CODE RAN"), "untrusted project code executed\n{}", sess.transcript());
}

#[test]
fn a_trusted_project_init_runs() {
    let s = Sandbox::new("trusted");
    s.write(
        "work/.neosh/init.ts",
        r#"import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  const buf = await neosh.buf.create({ name: "[proj]", scratch: true });
  await neosh.buf.setLines(buf, 0, -1, ["project config ran"]);
}
"#,
    );

    let status = Command::new(env!("CARGO_BIN_EXE_neosh"))
        .arg("--config-dir")
        .arg(s.config())
        .arg("--cwd")
        .arg(s.work())
        .env("NEOSH_STATE_DIR", s.root.join("state"))
        .args(["trust"])
        .stdout(Stdio::null())
        .status()
        .expect("neosh trust should run");
    assert!(status.success());

    let mut sess = s.start();
    sess.wait_for("project config ran");
}

#[test]
fn editing_a_trusted_project_init_stops_it_running_again() {
    // `git pull` must not be able to change what runs on your machine.
    let s = Sandbox::new("stale");
    s.write(
        "work/.neosh/init.ts",
        "export function activate() {}\n",
    );
    Command::new(env!("CARGO_BIN_EXE_neosh"))
        .arg("--config-dir")
        .arg(s.config())
        .arg("--cwd")
        .arg(s.work())
        .env("NEOSH_STATE_DIR", s.root.join("state"))
        .args(["trust"])
        .stdout(Stdio::null())
        .status()
        .expect("trust");

    s.write(
        "work/.neosh/init.ts",
        r#"import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  const buf = await neosh.buf.create({ name: "[evil]", scratch: true });
  await neosh.buf.setLines(buf, 0, -1, ["PROJECT CODE RAN"]);
}
"#,
    );

    let mut sess = s.start();
    sess.wait_for("changed since you trusted it");
    assert!(!sess.saw("PROJECT CODE RAN"), "edited project code executed\n{}", sess.transcript());
}

#[test]
fn clean_ignores_the_config_entirely() {
    let s = Sandbox::new("clean");
    s.write(
        "config/init.ts",
        &init_ts(r#"  await report(ctx, "init.ts ran");"#),
    );

    let child = Command::new(env!("CARGO_BIN_EXE_neosh"))
        .args(["--ui-protocol", "stdio", "--clean"])
        .arg("--config-dir")
        .arg(s.config())
        .arg("--cwd")
        .arg(s.work())
        .args(["--mock-script", &fixture().display().to_string()])
        .args(["--model", "mock/mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start");
    let mut sess = Session::wrap(child);

    // Something the host always emits, so we know startup got past config.
    sess.wait_for("--clean");
    // Drive a key so any pending config work would have had a chance to appear.
    sess.key_char('x');
    sess.read_until("the composer to echo the keypress", |s| s.saw("x"));
    assert!(!sess.saw("init.ts ran"), "--clean must not read the config\n{}", sess.transcript());
}

// ---- output hygiene -------------------------------------------------------

/// Run a subcommand, read one line, then close the pipe and see how neosh takes it.
fn exits_cleanly_when_the_reader_goes_away(args: &[&str]) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_neosh"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    {
        let out = child.stdout.take().expect("stdout piped");
        let mut r = BufReader::new(out);
        let mut first = String::new();
        let _ = r.read_line(&mut first);
        // `r` drops here, closing the read end: the next write gets EPIPE.
    }

    let out = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stderr)
}

#[test]
fn piping_command_output_into_head_does_not_panic() {
    // `println!` panics on EPIPE. Left unhandled, `neosh paths | head` prints a crash report where
    // every other unix tool exits quietly.
    for args in [
        vec!["paths"],
        vec!["--clean", "paths"],
        vec!["--clean", "--list-models"],
    ] {
        let (ok, stderr) = exits_cleanly_when_the_reader_goes_away(&args);
        assert!(!stderr.contains("panicked"), "`neosh {}` panicked:\n{stderr}", args.join(" "));
        assert!(ok, "`neosh {}` exited non-zero:\n{stderr}", args.join(" "));
    }
}

#[test]
fn a_closed_ui_protocol_pipe_is_a_disconnect_not_a_crash() {
    let s = Sandbox::new("epipe-ui");
    let (ok, stderr) = {
        let mut child = Command::new(env!("CARGO_BIN_EXE_neosh"))
            .args(["--ui-protocol", "stdio", "--clean"])
            .arg("--cwd")
            .arg(s.work())
            .args(["--mock-script", &fixture().display().to_string()])
            .args(["--model", "mock/mock"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        {
            let out = child.stdout.take().expect("stdout piped");
            let mut r = BufReader::new(out);
            let mut first = String::new();
            let _ = r.read_line(&mut first);
            assert!(first.contains("init"), "expected the protocol to open with init: {first:?}");
        }
        // Nudge it into writing again now that nobody is reading.
        let mut stdin = child.stdin.take().expect("stdin piped");
        let _ = writeln!(stdin, r#"{{"type":"ready","width":80,"height":24}}"#);
        let _ = stdin.flush();
        drop(stdin);

        let out = child.wait_with_output().expect("wait");
        (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    assert!(!stderr.contains("panicked"), "panicked on a closed pipe:\n{stderr}");
    assert!(!stderr.contains("Broken pipe"), "reported a disconnect as a failure:\n{stderr}");
    assert!(ok, "exited non-zero on a clean disconnect:\n{stderr}");
}

// ---- reload ---------------------------------------------------------------

impl Session {
    fn key_ctrl(&mut self, c: char) {
        self.send(&format!(
            r#"{{"type":"key","key":{{"code":{{"kind":"char","c":"{c}"}},"mods":{{"ctrl":true}}}}}}"#
        ));
    }
}

#[test]
fn reload_re_reads_the_config_from_disk() {
    let s = Sandbox::new("reload");
    s.write("config/init.ts", &init_ts(r#"  neosh.notify("generation ONE");"#));

    let mut sess = s.start();
    sess.wait_for("generation ONE");

    s.write("config/init.ts", &init_ts(r#"  neosh.notify("generation TWO");"#));
    sess.key_ctrl('r');
    sess.wait_for("generation TWO");
}

#[test]
fn reload_reaches_modules_the_config_imports() {
    // deno_core caches modules by URL. Busting only the entry point produces the worst kind of
    // reload: one that reports success and keeps running the previous code.
    let s = Sandbox::new("reload-deps");
    s.write("config/helper.ts", "export const label = () => \"ONE\";\n");
    s.write(
        "config/init.ts",
        &format!(
            "import type {{ PluginContext }} from \"@neosh/api\";\n\
             import {{ label }} from \"./helper.ts\";\n\
             export async function activate({{ neosh }}: PluginContext) {{\n\
             \x20 neosh.notify(`helper says ${{label()}}`);\n\
             }}\n"
        ),
    );

    let mut sess = s.start();
    sess.wait_for("helper says ONE");

    // Only the helper changes; the entry point is byte-identical.
    s.write("config/helper.ts", "export const label = () => \"TWO\";\n");
    sess.key_ctrl('r');
    sess.wait_for("helper says TWO");
}

#[test]
fn reload_undoes_a_setting_you_deleted() {
    // The failure that makes people restart instead of reloading: a config that only ever adds.
    let s = Sandbox::new("reload-opt");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  await neosh.opt.set("chat.show_thinking", true);
  neosh.notify(`thinking=${await neosh.opt.get("chat.show_thinking")}`);"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("thinking=true");

    // The `set` line is gone, so the option must go back to its default.
    s.write(
        "config/init.ts",
        &init_ts(r#"  neosh.notify(`thinking=${await neosh.opt.get("chat.show_thinking")}`);"#),
    );
    sess.key_ctrl('r');
    sess.wait_for("thinking=false");
}

#[test]
fn reload_drops_a_command_you_deleted() {
    let s = Sandbox::new("reload-cmd");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  await neosh.cmd.register("cfg.gone", () => {}, { desc: "temporary" });
  const names = (await neosh.cmd.list()).map((c) => c.name);
  neosh.notify(`commands: ${names.filter((n) => n.startsWith("cfg.")).join(",") || "none"}`);"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("commands: cfg.gone");

    s.write(
        "config/init.ts",
        &init_ts(
            r#"  const names = (await neosh.cmd.list()).map((c) => c.name);
  neosh.notify(`commands: ${names.filter((n) => n.startsWith("cfg.")).join(",") || "none"}`);"#,
        ),
    );
    sess.key_ctrl('r');
    sess.wait_for("commands: none");
}

#[test]
fn a_config_that_stops_parsing_leaves_the_running_one_alone() {
    // Tearing down a working configuration and having nothing to put back is the one outcome a
    // reload must never produce.
    let s = Sandbox::new("reload-broken");
    s.write("config/init.ts", &init_ts(r#"  neosh.notify("good config");"#));

    let mut sess = s.start();
    sess.wait_for("good config");

    s.write("config/config.toml", "this is not = = toml\n");
    sess.key_ctrl('r');
    sess.wait_for("keeping the current config");
    assert!(!sess.saw("configuration reloaded"), "it must not claim success\n{}", sess.transcript());

    // Still alive and still the old config: fix the file and reload again.
    s.write("config/config.toml", "");
    s.write("config/init.ts", &init_ts(r#"  neosh.notify("recovered");"#));
    sess.key_ctrl('r');
    sess.wait_for("recovered");
}

#[test]
fn reload_picks_up_a_project_trusted_since_launch() {
    // Otherwise `neosh trust` in another terminal needs a restart to take effect.
    let s = Sandbox::new("reload-trust");
    s.write(
        "work/.neosh/init.ts",
        r#"import type { PluginContext } from "@neosh/api";
export async function activate({ neosh }: PluginContext) {
  neosh.notify("project config ran");
}
"#,
    );

    let mut sess = s.start();
    sess.wait_for("neosh trust");
    assert!(!sess.saw("project config ran"));

    let status = Command::new(env!("CARGO_BIN_EXE_neosh"))
        .arg("--config-dir")
        .arg(s.config())
        .arg("--cwd")
        .arg(s.work())
        .env("NEOSH_STATE_DIR", s.root.join("state"))
        .arg("trust")
        .stdout(Stdio::null())
        .status()
        .expect("trust");
    assert!(status.success());

    sess.key_ctrl('r');
    sess.wait_for("project config ran");
}

#[test]
fn builtin_commands_are_ordinary_registry_entries() {
    // If the host's own commands lived somewhere private, `cmd.list()` would be a partial view and
    // rebinding them would be impossible.
    let s = Sandbox::new("builtins");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  const all = await neosh.cmd.list();
  const reload = all.find((c) => c.name === "config.reload");
  neosh.notify(`reload owner=${reload?.plugin} desc=${reload ? "yes" : "no"}`);"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("reload owner=neosh desc=yes");
}

#[test]
fn a_config_can_rebind_a_builtin_key() {
    // The default bindings are ordinary bindings, not reserved keys.
    let s = Sandbox::new("rebind");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  const buf = await neosh.buf.create({ name: "[mine]", scratch: true });
  await neosh.cmd.register("cfg.mine", async () => {
    await neosh.buf.setLines(buf, 0, -1, ["my binding won"]);
  });
  await neosh.keymap.set("chat", "<C-r>", "cfg.mine");
  neosh.notify("rebound");"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("rebound");
    sess.key_ctrl('r');
    sess.wait_for("my binding won");
    assert!(!sess.saw("configuration reloaded"), "the built-in should not have run too");
}

#[test]
fn quit_closes_neosh() {
    // Before this there was no way out of the terminal UI but SIGINT.
    let s = Sandbox::new("quit");
    let mut sess = s.start();
    // Wait for the first frame, so the key is not delivered before the host is listening.
    sess.read_until("the first frame", |s| {
        s.events.iter().any(|e| e["type"] == "flush")
    });
    sess.key_ctrl('q');
    let status = sess.child.wait().expect("wait");
    assert!(status.success(), "quit should exit cleanly, got {status}");
}

// ---- timers ---------------------------------------------------------------

#[test]
fn timers_fire_in_a_session_with_no_input() {
    // The case that deadlocks if the drive loop only wakes on host messages: nothing but the timer
    // itself can move this session forward.
    let s = Sandbox::new("timers");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  const buf = await neosh.buf.create({ name: "[t]", scratch: true });
  const seen: string[] = [];
  const show = () => void neosh.buf.setLines(buf, 0, -1, [seen.join(" ")]);

  setTimeout(() => { seen.push("timeout"); show(); }, 40);

  let n = 0;
  const stop = neosh.timer.every(20, () => {
    n += 1;
    seen.push(`tick${n}`);
    show();
    if (n === 3) stop.dispose();
  });

  const never = setTimeout(() => { seen.push("CLEARED-FIRED"); show(); }, 30);
  clearTimeout(never);

  const d = neosh.timer.debounce(35, (which: string) => { seen.push(`debounced:${which}`); show(); });
  d("a"); d("b"); d("c");"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("tick3");
    sess.wait_for("timeout");
    sess.wait_for("debounced:c");

    let seen = sess.texts().join(" ");
    assert!(!seen.contains("CLEARED-FIRED"), "a cleared timer fired\n{}", sess.transcript());
    assert!(!seen.contains("tick4"), "a disposed interval kept running\n{}", sess.transcript());
    assert!(
        !seen.contains("debounced:a") && !seen.contains("debounced:b"),
        "debounce did not coalesce the burst\n{}",
        sess.transcript()
    );
}

#[test]
fn a_repeating_timer_does_not_hold_shutdown_open() {
    // Pending work in the JS event loop must not outlive the session. A `setInterval` that kept the
    // runtime alive would mean neosh never exits for anyone who installed a polling plugin.
    let s = Sandbox::new("timer-shutdown");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  setInterval(() => {}, 10);
  setTimeout(() => {}, 60 * 60 * 1000);
  neosh.notify("armed");"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("armed");

    let began = Instant::now();
    sess.key_ctrl('q');
    let status = sess.child.wait().expect("wait");
    assert!(status.success(), "did not exit cleanly: {status}");
    assert!(began.elapsed() < Duration::from_secs(5), "shutdown took {:?}", began.elapsed());
}

#[test]
fn unloading_a_plugin_cancels_the_timers_it_owns() {
    // An interval that outlives its plugin would keep calling into a torn-down context, and would
    // double up on every reload.
    let s = Sandbox::new("timer-unload");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  const buf = await neosh.buf.create({ name: "[gen]", scratch: true });
  neosh.timer.every(15, () => { void neosh.buf.setLines(buf, 0, -1, ["alive"]); });
  neosh.notify("armed");"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("armed");
    sess.wait_for("alive");

    // Reload with a config that arms nothing. If teardown missed the interval, the old callback
    // keeps writing — many times over, at 15ms.
    s.write("config/init.ts", &init_ts(r#"  neosh.notify("quiet config");"#));
    sess.key_ctrl('r');
    sess.wait_for("quiet config");

    let before = sess.texts().len();
    sess.drain_for(Duration::from_millis(500));
    let after = sess.texts().into_iter().skip(before).filter(|t| t == "alive").count();
    assert!(after == 0, "the old interval survived its plugin: {after} more writes in 500ms");
}

// ---- reload applies the whole configuration, not part of it -----------------
//
// Every test below is a regression: the first version of `config.reload` re-ran options and config
// scripts only, and silently ignored the rest while reporting success.

#[test]
fn reload_applies_changed_permissions() {
    // `exec` rather than `read_file`: reads inside the workspace are allowed in every mode but
    // `deny`, so they cannot tell an applied permission change from an ignored one.
    //
    // And not `ask`: `neosh.permit` no longer *returns* "prompt" — it asks whoever can answer and
    // comes back with a decision, which is the point of the approval flow. A config-reload test
    // should not depend on a human, so both ends of this are modes that decide by themselves.
    let s = Sandbox::new("reload-perms");
    s.write(
        "config/config.toml",
        "[permissions]\nmode = \"allow_listed\"\nallow_commands = [\"cargo\"]\n",
    );
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  const d = await neosh.permit({ kind: "exec", command: "cargo build" });
  neosh.notify(`permit=${d.kind}`);"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("permit=allow");

    // Both the mode and the allow-list have to reach the running layer.
    s.write("config/config.toml", "[permissions]\nmode = \"deny\"\n");
    sess.key_ctrl('r');
    sess.wait_for("permit=deny");

    // And back, so this cannot pass by permissions only ever getting stricter.
    s.write(
        "config/config.toml",
        "[permissions]\nmode = \"allow_listed\"\nallow_commands = [\"cargo\"]\n",
    );
    sess.key_ctrl('r');
    sess.wait_for("permit=allow");
}

#[test]
fn reload_applies_provider_changes_in_both_directions() {
    let s = Sandbox::new("reload-providers");
    let report = r#"  const ids = (await neosh.agent.listInstances()).map((i) => i.id);
  neosh.notify(`instances: ${ids.includes("scratch") ? "has-scratch" : "no-scratch"}`);"#;
    s.write("config/init.ts", &init_ts(report));

    let mut sess = s.start();
    sess.wait_for("instances: no-scratch");

    // Added.
    // Driver `mock`, because these sessions run with `--mock-script` and only that driver is
    // registered — an instance whose driver is missing is correctly not listed as usable.
    s.write(
        "config/config.toml",
        "[[providers]]\nid = \"scratch\"\ndriver = \"mock\"\n\
         display_name = \"Scratch\"\nauth = { kind = \"none\" }\n",
    );
    sess.key_ctrl('r');
    sess.wait_for("instances: has-scratch");

    // And removed again — rebuilding from a base is the only way deleting a block takes effect.
    s.write("config/config.toml", "");
    sess.key_ctrl('r');
    sess.read_until("the instance to disappear", |s| {
        s.texts().iter().rev().take(4).any(|t| t.contains("no-scratch"))
    });
}

#[test]
fn reload_restores_a_builtin_binding_a_previous_config_had_taken() {
    // Generation 1 rebinds <C-r>; generation 2 does not. Without re-registering the built-ins on
    // every application, <C-r> would be left bound to nothing — and <C-r> is how you reload.
    let s = Sandbox::new("reload-rebind");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  await neosh.cmd.register("cfg.reload", async () => {
    await neosh.cmd.exec("config.reload");
  });
  await neosh.keymap.set("chat", "<C-y>", "cfg.reload");
  await neosh.cmd.register("cfg.mine", () => neosh.notify("my binding ran"));
  await neosh.keymap.set("chat", "<C-r>", "cfg.mine");
  neosh.notify("generation one");"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("generation one");
    sess.key_ctrl('r');
    sess.wait_for("my binding ran");

    // Reload through the config's own key, then drop the rebinding.
    s.write("config/init.ts", &init_ts(r#"  neosh.notify("generation two");"#));
    sess.key_ctrl('y');
    sess.wait_for("generation two");

    // <C-r> must be the built-in again rather than dangling.
    sess.key_ctrl('r');
    sess.wait_for("configuration reloaded");
}

#[test]
fn a_config_that_throws_is_still_torn_down() {
    // A plugin that fails to activate never entered the loaded list, so reload skipped it — and
    // anything it registered before throwing survived every reload, stacking one more copy each
    // time.
    let s = Sandbox::new("throw-teardown");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  neosh.timer.every(20, () => neosh.notify("zombie"));
  neosh.notify("armed");
  throw new Error("boom");"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("armed");
    sess.wait_for("boom");

    // Teardown runs on the failure path, so the interval it armed is already gone.
    sess.drain_for(Duration::from_millis(300));
    let before = sess.texts().iter().filter(|t| t.contains("zombie")).count();

    s.write("config/init.ts", &init_ts(r#"  neosh.notify("quiet");"#));
    sess.key_ctrl('r');
    sess.wait_for("quiet");
    sess.drain_for(Duration::from_millis(400));

    let after = sess.texts().iter().filter(|t| t.contains("zombie")).count();
    assert_eq!(after, before, "the failed config's interval survived: {} new firings", after - before);
}

#[test]
fn a_zero_delay_interval_does_not_wedge_the_runtime() {
    // Regression: rescheduling at `now + 0` made the deadline drain loop spin, hanging the process.
    // One line of ordinary plugin JavaScript could do it.
    let s = Sandbox::new("zero-interval");
    s.write(
        "config/init.ts",
        &init_ts(
            r#"  let n = 0;
  const stop = neosh.timer.every(0, () => {
    n += 1;
    if (n === 3) { stop.dispose(); neosh.notify("ticked three times"); }
  });
  neosh.notify("armed");"#,
        ),
    );

    let mut sess = s.start();
    sess.wait_for("armed");
    sess.wait_for("ticked three times");

    let began = Instant::now();
    sess.key_ctrl('q');
    let status = sess.child.wait().expect("wait");
    assert!(status.success(), "did not exit cleanly: {status}");
    assert!(began.elapsed() < Duration::from_secs(5), "shutdown took {:?}", began.elapsed());
}

#[test]
fn a_reload_asked_for_during_startup_happens_when_startup_settles() {
    // Startup is asynchronous and loads several bundled plugins after the config, so pressing
    // `<C-r>` a moment after launch lands inside a real window where nothing is ready yet.
    // Refusing with "try again" made the user press the key twice for no reason they could see.
    let s = Sandbox::new("reload-during-startup");
    s.write(
        "config/init.ts",
        "import type { PluginContext } from \"@neosh/api\";\n\
         export async function activate({ neosh }: PluginContext) {\n\
         \x20 neosh.notify(`config says ONE`);\n\
         }\n",
    );

    let mut sess = s.start();
    sess.wait_for("config says ONE");

    // The config has activated but the bundled plugins have not finished, so this arrives early.
    s.write(
        "config/init.ts",
        "import type { PluginContext } from \"@neosh/api\";\n\
         export async function activate({ neosh }: PluginContext) {\n\
         \x20 neosh.notify(`config says TWO`);\n\
         }\n",
    );
    sess.key_ctrl('r');

    // No second keypress. If the reload were dropped rather than queued, this times out.
    sess.wait_for("config says TWO");
}

// ---------------------------------------------------------------------------
// Getting out
// ---------------------------------------------------------------------------

#[test]
fn ctrl_c_clears_a_draft_before_it_considers_quitting() {
    // The cost of getting this wrong is someone's unsent message. `<C-c>` is muscle memory, so it
    // has to deal with the most local thing first and only ever leave as a last resort.
    let s = Sandbox::new("ctrlc-draft");
    let mut sess = s.start();
    sess.wait_for("agent workspace");

    for c in "hello there".chars() {
        sess.key_char(c);
    }
    sess.read_until("the draft", |s| s.saw("hello there"));

    sess.key_ctrl('c');
    // Cleared, and no talk of quitting.
    sess.drain_for(Duration::from_millis(600));
    assert!(
        !sess.saw("press ^C again"),
        "a draft was there to clear, so quitting is not on the table yet\n{}",
        sess.transcript()
    );
}

#[test]
fn ctrl_c_twice_on_an_empty_composer_quits() {
    let s = Sandbox::new("ctrlc-quit");
    let mut sess = s.start();
    sess.wait_for("agent workspace");

    sess.key_ctrl('c');
    sess.wait_for("press ^C again to quit");

    sess.key_ctrl('c');
    // The process ends, so the stream closes. That is the observable.
    assert!(sess.exits_within(Duration::from_secs(10)), "second ^C should quit");
}

#[test]
fn ctrl_q_quits_immediately() {
    let s = Sandbox::new("ctrlq");
    let mut sess = s.start();
    sess.wait_for("agent workspace");
    sess.key_ctrl('q');
    assert!(sess.exits_within(Duration::from_secs(10)), "^Q is the no-questions exit");
}

#[test]
fn a_termination_signal_leaves_through_the_ordinary_quit_path() {
    // Not `exit(1)` from a handler: shutting down has to flush, tear plugins down and restore the
    // terminal, or a `kill` leaves the user's shell in raw mode with the alternate screen on.
    let s = Sandbox::new("sigterm");
    let mut sess = s.start();
    sess.wait_for("agent workspace");
    sess.signal(libc_sigterm());
    assert!(sess.exits_within(Duration::from_secs(10)), "SIGTERM should shut down cleanly");
    assert!(sess.saw_shutdown(), "the frontend was told to shut down\n{}", sess.transcript());
}

fn libc_sigterm() -> i32 {
    15
}
