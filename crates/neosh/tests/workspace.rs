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

    /// End it the way a crash does: no shutdown, no last flush, no chance to save anything.
    ///
    /// `Child::kill` is already `SIGKILL`, so this is that plus waiting for the socket to be
    /// unusable — the process is gone before the next workspace tries to bind over it.
    fn kill_hard(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
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
    /// How big this terminal says it is. A real frontend resolves layout from the declarative
    /// geometry it is sent and reports what it arrived at; this reports the whole thing, which is
    /// all the host needs to know to draw a row that has to fit.
    size: (u16, u16),
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.write.shutdown(std::net::Shutdown::Both);
    }
}

impl Client {
    fn attach(socket: &Path, protocol: u32) -> Self {
        Self::attach_sized(socket, protocol, 100, 34)
    }

    fn attach_sized(socket: &Path, protocol: u32, width: u16, height: u16) -> Self {
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
            size: (width, height),
        };
        c.send(ClientMessage::Attach { protocol_version: protocol, width, height });
        c
    }

    /// Say goodbye the way `client::run` does when the terminal it was in was closed.
    fn leave(&mut self) {
        self.send(ClientMessage::Detach);
    }

    /// Report resolved geometry, the way a frontend that has just laid itself out does.
    ///
    /// Without this the host never learns any size at all and falls back to eighty columns, so a
    /// test about width would pass or fail for reasons that have nothing to do with the terminals
    /// in it.
    fn report_viewports(&mut self) {
        let (width, height) = self.size;
        let wins: Vec<_> = self.mirror.windows.keys().copied().collect();
        for win in wins {
            self.send(ClientMessage::Input {
                event: InputEvent::ViewportChanged {
                    win,
                    width,
                    height,
                    top_line: 0,
                    rows: height as u32,
                },
            });
        }
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

    /// A chord, which is a different key from the letter it is made of.
    fn ctrl(&mut self, c: char) {
        self.send(ClientMessage::Input {
            event: InputEvent::Key {
                key: neosh_proto::KeyPress {
                    code: neosh_proto::KeyCode::Char { c: c.to_string() },
                    mods: neosh_proto::KeyMods { ctrl: true, ..Default::default() },
                },
            },
        });
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
fn a_second_terminal_joins_rather_than_taking_the_workspace_over() {
    // It used to take over and tell the first why. The reason to open a second window is that a
    // turn is running in the first, and taking it over is the exact moment you stop being able to
    // watch that turn.
    let sb = Sandbox::new("twoviews");
    let ws = sb.serve("hello_turn.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();

    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();

    first.pump_once();
    assert!(first.ended.is_none(), "the first terminal was not sent away");
    assert!(second.ended.is_none(), "and the second one is here too");
}

#[test]
fn what_happens_in_one_terminal_happens_in_the_other() {
    // "Synced everywhere" is the whole claim: there is one editor, and every terminal is a mirror
    // of it. A question asked here is a question on screen there, live and without asking for it.
    let sb = Sandbox::new("mirrored");
    let ws = sb.serve("hello_turn.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();

    first.ask("what did you do");

    second.until("the other terminal to show the question", |c| {
        c.transcript().iter().any(|l| l.contains("what did you do"))
    });
    second.until("and the answer it produced", |c| c.shows("All done."));
    first.until("in the terminal that asked, too", |c| c.shows("All done."));
}

#[test]
fn closing_one_terminal_leaves_the_others_alone() {
    // `^Q` closes *this* window. The workspace carries on, and so does every other view of it —
    // which was the one thing one-viewer-at-a-time could never be asked.
    let sb = Sandbox::new("closeone");
    let ws = sb.serve("hello_turn.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();

    first.leave();
    first.until("the terminal that asked to leave to go", |c| c.ended.is_some());

    // The survivor is not merely still connected — it is still being drawn to.
    second.ask("still here");
    second.until("the workspace to answer the one that stayed", |c| c.shows("All done."));

    let text = status(&sb);
    assert!(text.contains("attached"), "somebody is still looking: {text}");
}

#[test]
fn the_last_terminal_leaving_makes_the_workspace_idle_not_gone() {
    let sb = Sandbox::new("allidle");
    let ws = sb.serve("hello_turn.jsonl");

    let mut first = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    first.ready();
    let mut second = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    second.ready();

    first.leave();
    first.until("the first to go", |c| c.ended.is_some());
    // Still attached: one terminal leaving is not "nobody is looking".
    assert!(status(&sb).contains("attached"));

    second.leave();
    second.until("the second to go", |c| c.ended.is_some());

    let text = status(&sb);
    assert!(!text.contains("no workspace"), "it is still running: {text}");
    let mut ws = ws;
    assert!(ws.alive(), "an empty workspace is idle, not over");
}

#[test]
fn a_narrow_terminal_joining_makes_the_transcript_fit_it() {
    // Every view lays out its own geometry, but a tool card is one row of buffer *content* and the
    // content is shared. Measured for the widest terminal, it wraps in the narrow one — and a card
    // that wraps is the failure `chat_width` exists to prevent. So the smallest one wins.
    let sb = Sandbox::new("smallest");
    let ws = sb.serve("wide_card.jsonl");

    let mut wide = Client::attach_sized(&ws.socket, neosh_proto::PROTOCOL_VERSION, 160, 40);
    wide.ready();
    wide.report_viewports();

    let mut narrow = Client::attach_sized(&ws.socket, neosh_proto::PROTOCOL_VERSION, 60, 20);
    narrow.ready();
    narrow.report_viewports();

    narrow.until("the welcome to be redrawn for the narrow terminal", |c| {
        c.transcript().iter().all(|l| l.chars().count() <= 60)
    });
    wide.ask("how wide");
    narrow.until("the answer", |c| c.shows("All done."));
    wide.until("the answer", |c| c.shows("All done."));

    // Identical content, and it is content the narrow terminal can print without wrapping.
    let rows = wide.transcript();
    assert_eq!(rows, narrow.transcript(), "two mirrors of one editor disagreed");
    let widest = rows.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    assert!(widest <= 60, "a row of {widest} columns does not fit the narrow terminal: {rows:#?}");
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

/// `^C` twice is a way out of a *terminal*, and a workspace is not one.
///
/// `^Q` has always known that. `^C`'s cascade — copy, leave reading, interrupt the turn, clear the
/// draft, and only then quit — ended by setting `quitting` directly, so the last rung of the key
/// people press when they want out of something took the whole workspace with it: every
/// conversation, in every project, and every turn running in any of them.
#[test]
fn pressing_ctrl_c_twice_closes_the_terminal_and_not_the_workspace() {
    let sb = Sandbox::new("ctrlc");
    let mut ws = sb.serve("hello_turn.jsonl");

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    // Nothing selected, nothing being read, no turn, no draft: the cascade is empty, so the first
    // press arms and the second one is the way out.
    c.ctrl('c');
    c.ctrl('c');
    c.until("to be sent away", |c| c.ended == Some(DetachReason::Asked));

    assert!(ws.alive(), "^C^C closed the terminal, not the workspace");
    assert!(status(&sb).contains("running"), "{}", status(&sb));

    // And it is still a workspace you can come back to, which is the part that matters.
    let mut again = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    again.ready();
}

/// What a turn has already done is on disk while it is still doing the rest.
///
/// A turn is minutes of work and a dozen tool rounds, and the only saves used to be at either end
/// of it — so for the whole time it ran, the conversation on disk held the message you typed and
/// nothing after it. Anything that took the workspace out without warning lost the entire answer
/// and left the transcript ending on your own question.
#[test]
fn what_a_turn_has_done_is_on_disk_before_the_turn_ends() {
    let sb = Sandbox::new("midturn");
    let mut ws = sb.serve_paced("hello_turn.jsonl", 700);

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.ask("read it for me");

    // The first round's tool call, written down while the second round is still going. Waited for
    // rather than slept on, and the turn is asserted to be still in flight when it arrives — a
    // save that only happened because the turn had quietly finished would prove nothing.
    let deadline = Instant::now() + PATIENCE;
    loop {
        let saved = sessions_on_disk(&sb);
        if saved.contains("hello_greet") {
            assert!(
                status(&sb).contains("read it for me"),
                "the turn was already over, so this says nothing about saving during one"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the turn's first round never reached the disk:\n{saved}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // And the real test of it: take the workspace out the way a crash would, with no chance to
    // flush anything, and see what a new one finds.
    ws.kill_hard();
    drop(c);
    let ws2 = sb.serve_paced("hello_turn.jsonl", 0);
    let mut c2 = Client::attach(&ws2.socket, neosh_proto::PROTOCOL_VERSION);
    c2.ready();
    c2.until("the killed turn's work to come back", |c| {
        c.transcript().iter().any(|l| l.contains("read it for me"))
    });
    let back = c2.transcript().join("\n");
    assert!(back.contains("hello_greet"), "the tool round the turn had finished is gone:\n{back}");
}

/// Every session file the sandbox has written, as one string to look for things in.
fn sessions_on_disk(sb: &Sandbox) -> String {
    let dir = sb.root.join("state/sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else { return String::new() };
    entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A turn killed with the workspace says so, in the panel and in the transcript.
///
/// The one thing a conversation cannot notice about itself. Someone shuts the computer down mid
/// turn; the process does not get to write down that it died, so the conversation comes back
/// ending on a half-finished piece of work with nothing anywhere saying that is what it is.
#[test]
fn a_turn_killed_with_the_workspace_is_marked_when_it_comes_back() {
    let sb = Sandbox::new("cutoff");
    let mut ws = sb.serve_paced("hello_turn.jsonl", 700);

    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.ask("do the long thing");
    // Wait until the turn is genuinely in flight before pulling the plug, so what is being tested
    // is a turn that was killed rather than one that had never started.
    let deadline = Instant::now() + PATIENCE;
    while !status(&sb).contains("do the long thing") {
        assert!(Instant::now() < deadline, "the turn never started");
        std::thread::sleep(Duration::from_millis(50));
    }

    // No shutdown, no last flush: the machine went away.
    ws.kill_hard();
    drop(c);

    let ws2 = sb.serve_paced("hello_turn.jsonl", 0);
    let mut c2 = Client::attach(&ws2.socket, neosh_proto::PROTOCOL_VERSION);
    c2.ready();
    c2.until("the cut-off turn to be accounted for", |c| {
        c.transcript().iter().any(|l| l.contains("the workspace stopped while this turn was running"))
    });

    // And it survives a second start. The note the mark is derived from is cleared by the load that
    // reads it, so a mark that was not itself written down would last exactly one restart — and the
    // restart after that would quietly say everything was fine.
    drop(c2);
    let out = sb.neosh(&["stop"]).output().expect("stop");
    assert!(String::from_utf8_lossy(&out.stdout).contains("stopped"));
    let ws3 = sb.serve_paced("hello_turn.jsonl", 0);
    let mut c3 = Client::attach(&ws3.socket, neosh_proto::PROTOCOL_VERSION);
    c3.ready();
    c3.until("the mark to survive a second restart", |c| {
        c.transcript().iter().any(|l| l.contains("the workspace stopped while this turn was running"))
    });
}

/// And it goes away when a new turn starts there, because then it is not the last turn any more.
#[test]
fn asking_again_clears_the_mark_a_killed_turn_left() {
    let sb = Sandbox::new("cutoffclear");
    let mut ws = sb.serve_paced("hello_turn.jsonl", 700);
    let mut c = Client::attach(&ws.socket, neosh_proto::PROTOCOL_VERSION);
    c.ready();
    c.ask("the one that dies");
    let deadline = Instant::now() + PATIENCE;
    while !status(&sb).contains("the one that dies") {
        assert!(Instant::now() < deadline, "the turn never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    ws.kill_hard();
    drop(c);

    let ws2 = sb.serve_paced("hello_turn.jsonl", 0);
    let mut c2 = Client::attach(&ws2.socket, neosh_proto::PROTOCOL_VERSION);
    c2.ready();
    c2.until("the mark", |c| {
        c.transcript().iter().any(|l| l.contains("the workspace stopped while this turn was running"))
    });
    c2.ask("and the one that does not");
    c2.until("the new turn to finish", |c| {
        c.transcript().iter().any(|l| l.contains("All done."))
    });
    let after = c2.transcript().join("\n");
    assert!(
        !after.contains("the workspace stopped while this turn was running"),
        "a finished turn left the previous turn's mark behind:\n{after}"
    );
}
