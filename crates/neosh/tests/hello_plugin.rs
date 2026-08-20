//! The Phase 0 definition of done, end to end, through the real binary.
//!
//! Runs `neosh` with the stdio frontend and a recorded provider, loads `examples/hello-plugin`
//! from disk with no core changes, and asserts on the emitted UI event stream — the same events
//! the terminal frontend renders. No API key, no network, no terminal.
//!
//! Asserting on the protocol rather than on internals is the point: if the core ever grew a path
//! that reached past the protocol to draw something, these assertions would stop seeing it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

const BUDGET: Duration = Duration::from_secs(90);

struct Session {
    child: Child,
    /// Lines arrive on a channel rather than being read inline: a blocking `read_line` ignores
    /// the deadline, so a stalled host would hang the test rather than failing with a transcript.
    lines: Receiver<String>,
    events: Vec<Value>,
}

impl Session {
    fn start() -> Self {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf();

        let mut child = Command::new(env!("CARGO_BIN_EXE_neosh"))
            .args(["--ui-protocol", "stdio"])
            // `--clean` isolates the test from whatever config the developer happens to have:
            // no config.toml, no init.ts, no plugin directories but the one named here.
            .arg("--clean")
            .arg("--plugin-dir")
            .arg(root.join("examples"))
            .arg("--mock-script")
            .arg(root.join("crates/neosh/tests/fixtures/hello_turn.jsonl"))
            .args(["--model", "mock/mock"])
            .arg("--cwd")
            .arg(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("neosh should start");

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
        Self { child, lines, events: Vec::new() }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin piped");
        writeln!(stdin, "{line}").expect("write input");
        stdin.flush().expect("flush input");
    }

    fn key_char(&mut self, c: char) {
        self.send(&format!(
            r#"{{"type":"key","key":{{"code":{{"kind":"char","c":"{c}"}},"mods":{{}}}}}}"#
        ));
    }

    fn key_enter(&mut self) {
        self.send(r#"{"type":"key","key":{"code":{"kind":"enter"},"mods":{}}}"#);
    }

    /// Read events until `pred` matches something, or the budget runs out.
    fn read_until(&mut self, what: &str, pred: impl Fn(&[Value]) -> bool) {
        let deadline = Instant::now() + BUDGET;
        loop {
            if pred(&self.events) {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        self.events.push(v);
                    }
                }
                Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout) => break,
            }
        }
        if !pred(&self.events) {
            panic!("timed out waiting for {what}\n{}", self.transcript());
        }
    }

    /// Every line of text the UI has shown, in order. This is what a user would see.
    fn lines(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| e["type"] == "buffer_lines")
            .flat_map(|e| e["lines"].as_array().cloned().unwrap_or_default())
            .filter_map(|l| l["text"].as_str().map(str::to_string))
            .collect()
    }

    fn has_line_containing(&self, needle: &str) -> bool {
        self.lines().iter().any(|l| l.contains(needle))
    }

    fn transcript(&self) -> String {
        let mut s = String::from("--- UI lines seen ---\n");
        for l in self.lines() {
            s.push_str(&format!("  {l}\n"));
        }
        s
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// All six definition-of-done items, in one run of the real binary.
#[test]
fn hello_plugin_satisfies_the_phase_0_definition_of_done() {
    let mut s = Session::start();
    s.send(r#"{"type":"ready","width":100,"height":30}"#);

    // The plugin reports its own progress into a buffer, so the assertions below read the same
    // text a user would see rather than reaching into the host.
    s.read_until("the plugin to finish activating", |e| {
        e.iter().filter(|v| v["type"] == "buffer_lines").any(|v| {
            v["lines"]
                .as_array()
                .map(|a| a.iter().any(|l| l["text"].as_str() == Some("hello-plugin: ready")))
                .unwrap_or(false)
        })
    });

    // --- 1. a command bound to a key ------------------------------------
    assert!(
        s.has_line_containing("1. command hello.open registered and bound to <C-h>"),
        "DoD 1 (command + keybinding) not reported\n{}",
        s.transcript()
    );

    // --- 2. a float, opened by the plugin, with content ------------------
    assert!(s.has_line_containing("2. float opened"), "DoD 2 not reported");
    let float_opened = s.events.iter().any(|e| {
        e["type"] == "window_opened" && e["layout"]["kind"] == "float"
    });
    assert!(float_opened, "no float window reached the frontend\n{}", s.transcript());
    assert!(
        s.has_line_containing("hello from a TypeScript plugin"),
        "the float's content never reached the frontend"
    );

    // --- 3. an extmark that survives an append above it ------------------
    assert!(
        s.has_line_containing("3. extmark survived the insert above (row 1 -> 2)"),
        "DoD 3 (extmark survival) failed\n{}",
        s.transcript()
    );
    // And the annotation is actually on the wire, attached to its line.
    let mark_rendered = s.events.iter().any(|e| {
        e["type"] == "buffer_lines"
            && e["lines"].as_array().is_some_and(|ls| {
                ls.iter().any(|l| {
                    l["marks"].as_array().is_some_and(|ms| {
                        ms.iter().any(|m| {
                            m["virt_text"][0]["text"]
                                .as_str()
                                .is_some_and(|t| t.contains("still attached"))
                        })
                    })
                })
            })
    });
    assert!(mark_rendered, "the virtual text never reached the frontend\n{}", s.transcript());

    // --- drive a turn ----------------------------------------------------
    for c in "hi".chars() {
        s.key_char(c);
    }
    s.key_enter();

    // --- 4. the agent calls a plugin-registered tool ----------------------
    s.read_until("the plugin's tool to run", |e| {
        contains(e, "4. tool hello_greet ran for neosh")
    });

    // --- 5. a blocking pre-hook vetoes a tool call ------------------------
    s.read_until("the veto", |e| contains(e, "5. vetoed read_file .env"));

    // --- 6. streamed tokens land in a buffer incrementally ----------------
    s.read_until("the streamed turn to end", |e| contains(e, "6. streamed"));
    let streamed = s
        .lines()
        .into_iter()
        .find(|l| l.contains("6. streamed"))
        .expect("checked above");
    assert!(
        streamed.contains("streamed 3 chunks"),
        "expected three separate chunks, got {streamed:?} — tokens were not delivered incrementally"
    );

    // The plugin saw three separate chunks, and the frontend must never see more than that: an
    // append can only ever produce one frame, and mutations inside one ~16 ms window collapse into
    // a single one.
    //
    // *How many* it collapses to is wall clock, not code — on a loaded machine the window genuinely
    // closes between tokens and three frames is the correct answer. The merge itself is asserted in
    // `neosh-core/tests/coalescing.rs`, where it is deterministic. Here we check the half that
    // cannot flap: nothing is duplicated, and the finished line arrives.
    //
    // Counted **per buffer**. Two of them carry this text — the transcript, and the plugin's own
    // `[hello-stream]` — and adding them together measures nothing: it says three tokens produced
    // six frames when each buffer saw three, which is exactly right.
    let mut per_buffer: std::collections::BTreeMap<u64, Vec<&str>> = Default::default();
    for e in s.events.iter().filter(|e| e["type"] == "buffer_lines") {
        let Some(text) = e["lines"][0]["text"].as_str() else { continue };
        if !text.starts_with("All") {
            continue;
        }
        per_buffer.entry(e["buf"].as_u64().unwrap_or_default()).or_default().push(text);
    }
    assert!(
        per_buffer.values().any(|v| v.contains(&"All done.")),
        "the finished assistant line should reach the frontend: {per_buffer:?}"
    );
    for (buf, frames) in &per_buffer {
        assert!(
            frames.len() <= 3,
            "buffer {buf} saw more frames than there were tokens: {frames:?}"
        );
    }
}

fn contains(events: &[Value], needle: &str) -> bool {
    events
        .iter()
        .filter(|e| e["type"] == "buffer_lines")
        .filter_map(|e| e["lines"].as_array())
        .flatten()
        .filter_map(|l| l["text"].as_str())
        .any(|t| t.contains(needle))
}

/// The protocol is complete enough to render from: nothing the frontend needs is missing.
#[test]
fn the_event_stream_is_self_describing() {
    let mut s = Session::start();
    s.send(r#"{"type":"ready","width":100,"height":30}"#);
    s.read_until("startup", |e| {
        e.iter().any(|v| v["type"] == "flush") && e.len() > 5
    });

    assert_eq!(s.events[0]["type"], "init", "a frontend must learn the protocol version first");
    assert!(
        s.events.iter().any(|e| e["type"] == "highlight_defined"),
        "highlight groups must be declared before anything references them"
    );
    assert!(
        s.events.iter().any(|e| e["type"] == "window_opened"),
        "the frontend needs windows to draw into"
    );
    // Batches end with a flush, so a frontend draws once per frame rather than per event.
    assert_eq!(
        s.events.last().map(|e| e["type"].clone()),
        Some(Value::String("flush".into())),
        "every batch must end with a flush"
    );
}
