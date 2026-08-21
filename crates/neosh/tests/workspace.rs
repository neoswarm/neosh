//! The workspace outliving the terminal, driven as two real processes over a real socket.
//!
//! There is no way to fake this one. The thing being asserted is that a *process* survives another
//! process going away, so both of them have to exist: `neosh --serve` on one side, a socket client
//! standing in for the terminal on the other. What the client sends and receives here is exactly
//! what `crate::client` sends and receives — the same envelope, the same `InputEvent`s — with a
//! `Mirror` in place of a screen, because a screen is the one part of a terminal a test cannot
//! usefully have.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use neosh_proto::{ClientMessage, DetachReason, InputEvent, ServerMessage, UiEvent};

/// How long any "wait until" in here is allowed to take.
///
/// Generous, because a cold workspace boots deno and discovers plugins before it binds, and CI is
/// slower than a laptop by more than the margin a tight bound would leave.
const PATIENCE: Duration = Duration::from_secs(30);

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// A sandbox with its own config, state and — the point — its own socket.
///
/// The socket path is derived from the config directory, so a test gets a workspace of its own
/// without any further arrangement. That is not incidental: if it were keyed on the user, running
/// the suite would attach to the developer's real workspace and stop it.
struct Sandbox {
    root: PathBuf,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Stop the workspace before removing the directory it is writing into. It is a background
        // process by construction: nothing else will.
        let _ = self.neosh(&["stop"]).output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("neosh-workspace-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["config", "state", "work", "run"] {
            std::fs::create_dir_all(root.join(d)).expect("sandbox dirs");
        }
        Self { root }
    }

    fn neosh(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_neosh"));
        c.arg("--config-dir")
            .arg(self.root.join("config"))
            .arg("--cwd")
            .arg(self.root.join("work"))
            .args(args)
            .env("NEOSH_STATE_DIR", self.root.join("state"))
            .env("NEOSH_DATA_DIR", self.root.join("state"))
            .env("NEOSH_CACHE_DIR", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"));
        c
    }

    /// Where this sandbox's workspace listens.
    ///
    /// Recomputed here rather than asked of the binary: a test that derived the path by calling
    /// the code under test would pass just as happily if both were wrong about it.
    fn socket(&self) -> PathBuf {
        let key = fnv(self.root.join("config").as_os_str().as_encoded_bytes());
        self.root.join("run/neosh").join(format!("{key}.sock"))
    }

    fn serve(&self, script: &str) -> Workspace {
        self.serve_paced(script, 0)
    }

    /// Start a workspace and wait for it to answer.
    ///
    /// `pace_ms` slows the mock provider down, for the tests that need a turn to still be running
    /// while something happens to the terminal watching it.
    fn serve_paced(&self, script: &str, pace_ms: u64) -> Workspace {
        let child = self
            .neosh(&["--serve", "--mock-script"])
            .arg(fixture(script))
            .args(["--model", "mock/mock"])
            .env("NEOSH_MOCK_DELAY_MS", pace_ms.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn workspace");
        let socket = self.socket();
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if UnixStream::connect(&socket).is_ok() {
                return Workspace { child, socket };
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the workspace never came up on {}", socket.display());
    }
}

/// FNV-1a, matching `paths::key_for`. Duplicated on purpose: a test that computed the path by
/// calling the code under test would pass just as happily if both were wrong.
fn fnv(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

struct Workspace {
    child: Child,
    socket: PathBuf,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Workspace {
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// A terminal, minus the terminal.
///
/// Dropping one has to look to the workspace exactly like a window being closed, which means the
/// socket must actually close. It does not close by itself: the reader lives on its own thread
/// with a cloned descriptor, and that clone keeps the connection open long after the struct is
/// gone — so a test that "closed the terminal" would in fact still be attached, and would then
/// assert the wrong thing about a workspace that was behaving correctly.
struct Client {
    write: UnixStream,
    lines: std::sync::mpsc::Receiver<String>,
    mirror: neosh_tui::Mirror,
    ended: Option<DetachReason>,
    /// Why an attach was turned away, when it was.
    refusal: Option<String>,
    stopped: bool,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.write.shutdown(std::net::Shutdown::Both);
    }
}

impl Client {
    fn attach(socket: &Path, protocol: u32) -> Self {
        let stream = UnixStream::connect(socket).expect("connect");
        let read = stream.try_clone().expect("clone");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(read).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut c = Self {
            write: stream,
            lines: rx,
            mirror: neosh_tui::Mirror::new(),
            ended: None,
            refusal: None,
            stopped: false,
        };
        c.send(ClientMessage::Attach { protocol_version: protocol, width: 100, height: 34 });
        c
    }

    fn send(&mut self, msg: ClientMessage) {
        let mut line = serde_json::to_vec(&msg).expect("encode");
        line.push(b'\n');
        let _ = self.write.write_all(&line);
        let _ = self.write.flush();
    }

    fn key(&mut self, text: &str) {
        for ch in text.chars() {
            self.send(ClientMessage::Input {
                event: InputEvent::Key {
                    key: neosh_proto::KeyPress {
                        code: neosh_proto::KeyCode::Char { c: ch.to_string() },
                        mods: neosh_proto::KeyMods::default(),
                    },
                },
            });
        }
    }

    fn enter(&mut self) {
        self.send(ClientMessage::Input {
            event: InputEvent::Key {
                key: neosh_proto::KeyPress {
                    code: neosh_proto::KeyCode::Enter,
                    mods: neosh_proto::KeyMods::default(),
                },
            },
        });
    }

    /// Fold in whatever has arrived. Returns false once the connection is over.
    fn pump_once(&mut self) -> bool {
        while let Ok(line) = self.lines.try_recv() {
            match serde_json::from_str::<ServerMessage>(&line) {
                Ok(ServerMessage::Events { batch }) => {
                    for ev in batch {
                        if matches!(ev, UiEvent::Shutdown) {
                            self.stopped = true;
                        }
                        self.mirror.apply(ev);
                    }
                }
                Ok(ServerMessage::Detached { reason }) => self.ended = Some(reason),
                Ok(ServerMessage::Refused { reason, .. }) => {
                    self.ended = Some(DetachReason::Stopping);
                    self.refusal = Some(reason);
                }
                _ => {}
            }
        }
        self.ended.is_none() && !self.stopped
    }

    /// Pump until `f` is happy, or give up.
    fn until(&mut self, what: &str, f: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            self.pump_once();
            if f(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}:\n{:#?}", self.text());
    }

    /// Every line of every buffer, which is what "is it on screen" means here.
    fn text(&self) -> Vec<String> {
        self.mirror
            .buffers
            .values()
            .flat_map(|b| b.lines.iter().map(|l| l.text.clone()))
            .collect()
    }

    fn shows(&self, needle: &str) -> bool {
        self.text().iter().any(|l| l.contains(needle))
    }

    /// Wait until the workspace is actually ready to be typed at.
    ///
    /// Not the welcome screen, which is drawn before the plugins are up — the sidebar's heading,
    /// which only exists once a plugin has activated and drawn. A key sent before that lands
    /// nowhere, and the test that sent it fails with "no turn ran" a long way from the reason.
    fn ready(&mut self) {
        self.until("the workspace to finish starting", |c| c.shows("PROJECTS"));
    }

    /// Type a question and send it, and wait until it is a question rather than a draft.
    ///
    /// The wait is on the *transcript*, not on "is this text anywhere": what you have typed is in
    /// the composer from the first keystroke, so a check across every buffer is satisfied before
    /// `Enter` has been read, and the test races on from there.
    fn ask(&mut self, question: &str) {
        self.key(question);
        self.enter();
        self.until("the question to land in the transcript", |c| {
            c.transcript().iter().any(|l| l.contains(question))
        });
    }

    /// The chat buffer, which is the one named for it.
    fn transcript(&self) -> Vec<String> {
        self.mirror
            .buffers
            .values()
            .filter(|b| b.name.contains("chat"))
            .flat_map(|b| b.lines.iter().map(|l| l.text.clone()))
            .collect()
    }
}

fn status(sb: &Sandbox) -> String {
    let out = sb.neosh(&["status"]).output().expect("neosh status");
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn a_turn_keeps_running_when_the_terminal_that_started_it_goes_away() {
    // The whole point. A terminal is a window onto the work; closing a window is not a decision
    // about the work.
    let sb = Sandbox::new("survives");
    let ws = sb.serve_paced("hello_turn.jsonl", 900);

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.ask("keep at it");

    // The terminal is gone — not asked to leave, *gone*, the way a closed window or a dropped ssh
    // connection goes.
    drop(c);

    let text = status(&sb);
    assert!(text.contains("running"), "the workspace is still there: {text}");
    assert!(text.contains("keep at it"), "and the turn is still in it: {text}");
    let mut ws = ws;
    assert!(ws.alive());
}

#[test]
fn reattaching_shows_what_happened_while_nobody_was_looking() {
    let sb = Sandbox::new("reattach");
    let ws = sb.serve("hello_turn.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    first.ask("ask me something");
    drop(first);

    // Long enough for the turn to have finished with nobody attached at all.
    std::thread::sleep(Duration::from_millis(600));

    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.until("the answer the turn produced", |c| c.shows("All done."));
    assert!(second.shows("ask me something"), "and the question: {:?}", second.text());
}

#[test]
fn quitting_a_terminal_leaves_the_workspace_running_and_stop_does_not() {
    // Two words for two different things. `quit` used to mean both because there was only one
    // place to be.
    let sb = Sandbox::new("quitstop");
    let mut ws = sb.serve("hello_turn.jsonl");

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.send(ClientMessage::Input {
        event: InputEvent::Command { name: "quit".into(), args: Vec::new() },
    });
    c.until("to be sent away", |c| c.ended == Some(DetachReason::Asked));
    assert!(ws.alive(), "quit closed the terminal, not the workspace");
    assert!(status(&sb).contains("running"));

    let out = sb.neosh(&["stop"]).output().expect("stop");
    assert!(String::from_utf8_lossy(&out.stdout).contains("stopped"));
    let deadline = Instant::now() + PATIENCE;
    while ws.alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!ws.alive(), "stop stopped it");
    assert!(status(&sb).contains("no workspace"));
}

#[test]
fn a_second_terminal_takes_over_and_the_first_is_told_why() {
    // Rather than being refused. A client that died without saying so looks exactly like one that
    // is merely quiet, so refusing would let a crashed terminal lock you out of your own work.
    let sb = Sandbox::new("takeover");
    let ws = sb.serve("hello_turn.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();

    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();

    first.until("the first to be told", |c| c.ended == Some(DetachReason::TakenOver));
    assert!(second.ended.is_none(), "and the second is the one attached now");
}

#[test]
fn a_client_from_a_different_neosh_is_refused_in_a_sentence() {
    // The failure this exists for: neosh is upgraded while a workspace from the old one is still
    // running. Rendering a partial UI would be a worse answer than saying so.
    let sb = Sandbox::new("protocol");
    let ws = sb.serve("hello_turn.jsonl");

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION + 1);
    c.until("a refusal", |c| c.refusal.is_some());
    let reason = c.refusal.clone().expect("a reason");
    assert!(reason.contains("neosh stop"), "and it says what to do about it: {reason}");
}

#[test]
fn a_second_workspace_will_not_start_on_a_socket_that_answers() {
    let sb = Sandbox::new("onlyone");
    let _ws = sb.serve("hello_turn.jsonl");

    let out = sb
        .neosh(&["--serve", "--model", "mock/mock", "--mock-script"])
        .arg(fixture("hello_turn.jsonl"))
        .output()
        .expect("second workspace");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("already running"), "{text}");
    assert!(text.contains("neosh stop"), "and says how to deal with it: {text}");
}

#[test]
fn a_socket_left_behind_by_a_workspace_that_died_is_not_a_workspace() {
    // A socket file outlives the process that made it, so a crash leaves one. Treating its
    // presence as "something is running" would mean one hard kill locks the directory forever.
    let sb = Sandbox::new("stale");
    let socket = sb.socket();
    std::fs::create_dir_all(socket.parent().expect("dir")).expect("mkdir");
    std::fs::write(&socket, b"not a socket").expect("leftover");

    let mut ws = sb.serve("hello_turn.jsonl");
    assert!(ws.alive(), "it bound over the leftover");
    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
}

#[test]
fn status_answers_while_a_turn_is_running_which_is_when_it_is_asked() {
    let sb = Sandbox::new("statusbusy");
    let ws = sb.serve_paced("hello_turn.jsonl", 900);
    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.ask("think about it");

    let deadline = Instant::now() + PATIENCE;
    loop {
        let text = status(&sb);
        if text.contains("think about it") {
            assert!(text.contains("a terminal is attached"), "{text}");
            break;
        }
        assert!(Instant::now() < deadline, "status never reported the turn: {text}");
        std::thread::sleep(Duration::from_millis(100));
    }
}
