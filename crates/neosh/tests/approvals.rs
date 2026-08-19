//! Approvals, end to end.
//!
//! `permissions.mode = "ask"` is the default. Without something answering, "ask" means "refuse":
//! every gated tool call comes back as an error the model cannot act on and the user is never asked
//! anything. These tests are about the difference.

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
        let root = std::env::temp_dir().join(format!("neosh-approve-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["config", "state", "work"] {
            std::fs::create_dir_all(root.join(d)).expect("dirs");
        }
        std::fs::write(root.join("work/secret.txt"), "the contents\n").expect("write");
        Self { root }
    }

    fn write_config(&self, contents: &str) {
        std::fs::write(self.root.join("config/config.toml"), contents).expect("config");
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

/// Any recorded turn; these tests drive the permission API from a plugin rather than through a
/// tool call, because the decision point is shared and a plugin can exercise it directly.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello_turn.jsonl")
}

/// Asks for a capability that `ask` mode genuinely prompts about.
///
/// `read_file` inside the workspace is allowed outright in `ask` mode — the point of that mode is
/// running commands and reaching the network, not reading the project you opened. `exec` is what
/// asking is for.
const ASKER: &str = r#"
import type { PluginContext } from "@neosh/api";

export async function activate({ neosh }: PluginContext) {
  await neosh.cmd.register("t.exec", async () => {
    const d = await neosh.permit({ kind: "exec", command: "rm -rf /tmp/nope" });
    neosh.notify(`decision: ${d.kind}${d.kind === "deny" ? ` (${d.reason})` : ""}`);
  });
  neosh.notify("asker ready");
}
"#;

fn install_asker(sb: &Sandbox) {
    let dir = sb.root.join("config/plugins/asker");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("plugin.toml"),
        "name = \"asker\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("main.ts"), ASKER).expect("plugin");
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

    fn saw(&self, needle: &str) -> bool {
        self.texts().iter().any(|l| l.contains(needle))
    }

    fn transcript(&self) -> String {
        let mut s = String::from("--- text seen ---\n");
        for l in self.texts() {
            s.push_str(&format!("  {l}\n"));
        }
        s
    }
}

#[test]
fn a_gated_tool_call_asks_before_it_runs() {
    // The default mode is `ask`, and this is what asking looks like.
    let sb = Sandbox::new("ask");
    sb.write_config("[permissions]\nmode = \"ask\"\n");
    install_asker(&sb);
    let mut s = sb.start();
    s.wait_for("asker ready");

    s.command("t.exec");
    s.wait_for("Run rm -rf /tmp/nope?");
    assert!(s.saw("Allow once"), "the choices are offered\n{}", s.transcript());
}

#[test]
fn allowing_lets_the_tool_run() {
    let sb = Sandbox::new("allow");
    sb.write_config("[permissions]\nmode = \"ask\"\n");
    install_asker(&sb);
    let mut s = sb.start();
    s.wait_for("asker ready");

    s.command("t.exec");
    s.wait_for("Allow once");
    s.special("enter"); // the cursor starts on "Allow once"
    s.wait_for("decision: allow");
}

#[test]
fn dismissing_the_prompt_is_a_refusal_not_an_allow() {
    // A prompt you can escape into an allow is not a prompt.
    let sb = Sandbox::new("escape");
    sb.write_config("[permissions]\nmode = \"ask\"\n");
    install_asker(&sb);
    let mut s = sb.start();
    s.wait_for("asker ready");

    s.command("t.exec");
    s.wait_for("Allow once");
    s.special("esc");
    s.wait_for("decision: deny (not approved)");
}

#[test]
fn choosing_deny_refuses() {
    let sb = Sandbox::new("deny");
    sb.write_config("[permissions]\nmode = \"ask\"\n");
    install_asker(&sb);
    let mut s = sb.start();
    s.wait_for("asker ready");

    s.command("t.exec");
    s.wait_for("Allow once");
    // Down twice: "Allow once", "Allow for this session", "Deny".
    s.special("down");
    s.special("down");
    s.special("enter");
    s.wait_for("decision: deny (denied)");
}

#[test]
fn an_allow_listed_mode_never_asks() {
    // Policy that already answered must not ask again; a prompt for something the config allows is
    // a prompt the user cannot act on meaningfully.
    let sb = Sandbox::new("allowlisted");
    sb.write_config("[permissions]\nmode = \"allow_listed\"\n");
    install_asker(&sb);
    let mut s = sb.start();
    s.wait_for("asker ready");

    s.command("t.exec");
    s.drain_for(Duration::from_secs(3));
    assert!(!s.saw("Allow once"), "no prompt was needed\n{}", s.transcript());
    assert!(s.saw("decision: deny"), "allow_listed still refuses an unlisted command\n{}", s.transcript());
}

#[test]
fn deny_mode_refuses_without_asking() {
    let sb = Sandbox::new("denymode");
    sb.write_config("[permissions]\nmode = \"deny\"\n");
    install_asker(&sb);
    let mut s = sb.start();
    s.wait_for("asker ready");

    s.command("t.exec");
    s.wait_for("decision: deny");
    assert!(!s.saw("Allow once"), "asking would be pointless\n{}", s.transcript());
}

#[test]
fn with_the_approvals_plugin_off_a_gated_call_is_refused_rather_than_allowed() {
    // Fail closed. An unanswerable request that ran anyway would make `ask` mode a lie.
    let sb = Sandbox::new("noplugin");
    sb.write_config(
        "[permissions]\nmode = \"ask\"\n\n[options]\n\"plugins.disabled\" = [\"approvals\"]\n",
    );
    install_asker(&sb);
    let mut s = sb.start();
    s.wait_for("asker ready");

    s.command("t.exec");
    s.wait_for("needs approval, and nothing is able to ask");
}
