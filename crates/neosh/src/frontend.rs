//! Frontends.
//!
//! Two implementations of one trait. The terminal is what users run;
//! [`StdioFrontend`] emits the identical event stream as JSON lines.
//!
//! The stdio frontend is not a debugging afterthought — it is what keeps the boundary honest. If
//! the core ever reached past the protocol to touch ratatui, the JSON stream would immediately stop
//! containing everything needed to render, and the end-to-end tests that assert on it would fail.
//! It is also the shape a web or mobile frontend would connect over.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use neosh_proto::{InputEvent, UiEvent};
use neosh_tui::{Mirror, TerminalFrontend, to_input_event};
use tokio::sync::mpsc;

/// Whether anybody can see this workspace, which is what decides how a notification is delivered.
///
/// Three states rather than a boolean, because the two ways of not being seen need different
/// channels: a terminal that exists but is behind another window can be written an escape
/// sequence, and one that does not exist cannot. See ADR 0057.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// A terminal is attached and in front of somebody. Nothing needs to leave it.
    Watching,
    /// Attached, but no terminal has focus — or none has seen a keystroke in long enough that we
    /// assume nobody is at it. The view raises the notification itself.
    Away,
    /// Nothing is attached at all. There is no stream to write to, so the workspace's own machine
    /// is the only place left to raise it.
    Detached,
}

#[async_trait::async_trait]
pub trait Frontend: Send {
    /// Deliver one coalesced batch. The batch always ends with [`UiEvent::Flush`].
    async fn send(&mut self, batch: Vec<UiEvent>) -> anyhow::Result<()>;

    /// Whether anybody is looking.
    ///
    /// Asked of the frontend because the frontend is the only thing that knows: focus is a fact
    /// about somebody else's window manager, reported by a terminal that may or may not implement
    /// mode 1004, and a workspace serving several has to consider all of them.
    ///
    /// `idle_after` is the fallback for a terminal that cannot report focus — how long without a
    /// keystroke counts as nobody being there.
    async fn presence(&mut self, _idle_after: Duration) -> Presence {
        Presence::Watching
    }
    async fn shutdown(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Send one viewer away and keep going.
    ///
    /// Named, because a workspace can have several and `^Q` means "close *this* terminal". The key
    /// arrives tagged with the view it was pressed in, so the answer never depends on which
    /// terminal happened to speak most recently.
    ///
    /// Nothing for a frontend that *is* the process — there is nobody to send away, and the run
    /// loop is about to end anyway. It means something only for a workspace serving terminals,
    /// which is the one place `quit` and `stop` are different words.
    async fn detach(&mut self, _view: crate::views::ViewId) -> anyhow::Result<()> {
        Ok(())
    }

    /// Tell a terminal we own how to raise a notification.
    ///
    /// Only means anything for a frontend the workspace *is* — a `--no-daemon` neosh, where the
    /// terminal and the configuration are on one machine. A client terminal detects its own and is
    /// overridden by `NEOSH_NOTIFY_OSC`, because a workspace's configuration is its own and a
    /// second terminal is a viewer rather than a second opinion.
    fn configure_alerts(&mut self, _osc: neosh_tui::terminal::AlertOsc, _bell: bool) {}
}

/// Whether somebody is at this terminal, shared with the task that reads its events.
///
/// Atomics rather than a lock: it is written from the reader task on every keystroke and read from
/// the host loop, and a mutex between those two for two integers is a lock nobody needs. Focus is a
/// tri-state — see [`Watch::focused`].
#[derive(Debug, Default)]
struct Watch {
    /// `1` focused, `0` not, `-1` never said — which is what a terminal without mode 1004 leaves
    /// it at, and is why this is not a `bool`. See ADR 0057.
    focused: AtomicI8,
    /// Milliseconds since the process started, at the last keystroke.
    ///
    /// Relative to a fixed origin rather than an `Instant`, because an `Instant` is not something
    /// two threads can share in an atomic and the only question asked of it is a difference.
    typed_at: AtomicU64,
}

impl Watch {
    fn typed(&self, origin: Instant) {
        self.typed_at.store(origin.elapsed().as_millis() as u64, Ordering::Relaxed);
        // A terminal being typed into is a terminal somebody is looking at, whatever it last said
        // about focus. Window managers do drop the focus-in coming back from a workspace switch,
        // and a view stuck on `false` is one that notifies about things you are staring at.
        if self.focused.load(Ordering::Relaxed) == 0 {
            self.focused.store(1, Ordering::Relaxed);
        }
    }

    fn away(&self, origin: Instant, idle_after: Duration) -> bool {
        match self.focused.load(Ordering::Relaxed) {
            1 => false,
            0 => true,
            // Cannot say. Idleness is the honest proxy.
            _ => {
                let last = Duration::from_millis(self.typed_at.load(Ordering::Relaxed));
                origin.elapsed().saturating_sub(last) >= idle_after
            }
        }
    }
}

/// Renders to the terminal.
pub struct TerminalUi {
    inner: TerminalFrontend,
    mirror: Mirror,
    /// Whether somebody is at this terminal.
    watch: Arc<Watch>,
    /// What `Watch`'s millisecond counts are measured from.
    origin: Instant,
    /// Which escape this terminal understands as "raise a notification", and whether to ring the
    /// bell as well. Guessed at startup; `notify.osc` and `notify.bell` replace it once the
    /// configuration has loaded, which is long after the terminal has to be in the right mode.
    alert_osc: neosh_tui::terminal::AlertOsc,
    alert_bell: bool,
    /// Where input events go, so resolved geometry can be reported back as `ViewportChanged`.
    input: mpsc::UnboundedSender<InputEvent>,
    /// Whether a repaint ticker is running, and the switch that stops it.
    animating: Arc<AtomicBool>,
    /// The last geometry we told the core about, per window.
    ///
    /// Only *changes* are sent. Emitting one per window per draw is a feedback loop: the event arms
    /// the redraw deadline, which draws, which emits the event again, forever.
    reported: std::collections::HashMap<neosh_proto::WindowId, (u16, u16, u32, u32)>,
}

impl TerminalUi {
    pub fn enter() -> anyhow::Result<(Self, mpsc::UnboundedReceiver<InputEvent>)> {
        // Before taking the screen, arrange for a panic to be readable. Otherwise the message is
        // printed into the alternate screen, `Drop` tears that screen down, and the user is left
        // looking at their shell prompt with no indication anything happened at all.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            neosh_tui::restore_terminal();
            previous(info);
        }));

        let inner = TerminalFrontend::enter()?;
        let (w, h) = inner.size()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(InputEvent::Ready { width: w, height: h });
        let tx_geometry = tx.clone();
        let watch = Arc::new(Watch::default());
        // `-1` is "this terminal has never said". `Default` gives zero, which is the one value
        // that means something else — not focused — so it has to be set rather than assumed.
        watch.focused.store(-1, Ordering::Relaxed);
        let origin = Instant::now();
        let seen = watch.clone();

        tokio::spawn(async move {
            use futures::StreamExt;
            let mut events = crossterm::event::EventStream::new();
            while let Some(Ok(ev)) = events.next().await {
                let Some(input) = to_input_event(ev) else { continue };
                // Recorded here rather than at the host, because a frontend that *is* the process
                // has no view id to be tagged with and the host would have nowhere to put it.
                match &input {
                    InputEvent::Focus { on } => {
                        seen.focused.store(i8::from(*on), Ordering::Relaxed);
                        // Nothing downstream acts on it, and forwarding it would arm a redraw for
                        // an event that changes not one cell.
                        continue;
                    }
                    InputEvent::Key { .. } | InputEvent::Paste { .. } => seen.typed(origin),
                    _ => {}
                }
                if tx.send(input).is_err() {
                    break;
                }
            }
            let _ = tx.send(InputEvent::Disconnected);
        });

        Ok((
            Self {
                inner,
                mirror: Mirror::new(),
                watch,
                origin,
                alert_osc: neosh_tui::terminal::AlertOsc::detect(),
                alert_bell: false,
                input: tx_geometry,
                reported: Default::default(),
                animating: Arc::new(AtomicBool::new(false)),
            },
            rx,
        ))
    }
}

#[async_trait::async_trait]
impl Frontend for TerminalUi {
    async fn send(&mut self, batch: Vec<UiEvent>) -> anyhow::Result<()> {
        let mut redraw = false;
        for ev in batch {
            redraw |= self.mirror.apply(ev);
        }
        // Before the draw: an escape written after ratatui has painted and moved the cursor is an
        // escape the terminal sees in the middle of a frame it has already committed.
        if let Some(text) = self.mirror.clipboard.take() {
            // A terminal that does not do OSC 52 ignores it; a broken pipe on the way out is the
            // shutdown path and not worth failing a redraw over.
            let _ = self.inner.copy(&text);
        }
        // The host only ever sends one of these once it has established nobody can see the thing
        // it is about, so there is nothing left to decide here — just which escape this terminal
        // speaks, which is the one part of it the host cannot know. See ADR 0057.
        for (level, title, body) in std::mem::take(&mut self.mirror.alerts) {
            let _ = self.inner.alert(self.alert_osc, self.alert_bell, &title, &body);
            let _ = level;
        }
        if redraw && !self.mirror.shutdown {
            let geometry = self.inner.draw(&self.mirror)?;
            self.report_geometry(&geometry);
            self.follow_motion(neosh_tui::shimmer::take_drew_animation());
        }
        Ok(())
    }

    async fn presence(&mut self, idle_after: Duration) -> Presence {
        match self.watch.away(self.origin, idle_after) {
            true => Presence::Away,
            false => Presence::Watching,
        }
    }

    fn configure_alerts(&mut self, osc: neosh_tui::terminal::AlertOsc, bell: bool) {
        self.alert_osc = osc;
        self.alert_bell = bell;
    }

    async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.inner.leave()?;
        Ok(())
    }
}

/// How often to redraw while something on screen is moving.
///
/// 20 fps. Fast enough that a sweep reads as continuous, slow enough that it is a rounding error
/// next to what streaming a response already costs — and it only runs while there is motion, so an
/// idle workspace is still a process asleep on a `select!`.
const MOTION_FRAME: Duration = Duration::from_millis(50);

impl TerminalUi {
    /// Start or stop asking for frames.
    ///
    /// The frontend drives this, not the core, because the frontend is the only thing that knows
    /// whether the animated row is on screen or has been scrolled past. The ticker exists for
    /// exactly as long as motion is visible: it is armed by a frame that had some, and the first
    /// frame without any takes it down.
    fn follow_motion(&mut self, moving: bool) {
        if moving == self.animating.load(Ordering::Relaxed) {
            return;
        }
        self.animating.store(moving, Ordering::Relaxed);
        if !moving {
            return;
        }
        let flag = self.animating.clone();
        let tx = self.input.clone();
        tokio::spawn(async move {
            // Re-checked each tick rather than held for the lifetime of the task, so the ticker
            // stops itself the moment a frame comes back still.
            while flag.load(Ordering::Relaxed) {
                tokio::time::sleep(MOTION_FRAME).await;
                if tx.send(InputEvent::Repaint).is_err() {
                    return;
                }
            }
        });
    }

    /// Tell the core where each window ended up, but only when it moved.
    fn report_geometry(&mut self, geometry: &[neosh_tui::Geometry]) {
        for g in geometry {
            // The first buffer row that was *drawn*, which the frontend is the only thing that
            // knows. It used to report back whatever the core had asked for, with a zero standing
            // in for a tail-following window — but a rendered row does come from a buffer row, and
            // a window whose scroll bent to keep its cursor on screen is showing a row nobody
            // asked for. Everything paging by a screenful reads this number.
            let now = (g.rect.width, g.rect.height, g.top_line, g.rows);
            if self.reported.get(&g.win) == Some(&now) {
                continue;
            }
            self.reported.insert(g.win, now);
            let _ = self.input.send(InputEvent::ViewportChanged {
                win: g.win,
                width: g.rect.width,
                height: g.rect.height,
                top_line: g.top_line,
                rows: g.rows,
            });
        }
        // A window that closed should not keep a stale entry, or reopening it at the same size
        // would report nothing and leave every plugin with the old geometry.
        self.reported.retain(|win, _| geometry.iter().any(|g| g.win == *win));
    }
}

/// The frontend went away mid-write.
///
/// Not a failure: `neosh --ui-protocol=stdio | head` closing the pipe is the consumer's decision,
/// and reporting it as an error would print a crash where a clean exit belongs.
#[derive(Debug, thiserror::Error)]
#[error("frontend disconnected")]
pub struct Disconnected;

/// Writes the protocol to stdout as JSON lines and reads input as JSON lines from stdin.
pub struct StdioFrontend {
    out: std::io::Stdout,
}

impl StdioFrontend {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<InputEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<InputEvent>(&line) {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                    // A malformed line is reported and skipped rather than killing the session,
                    // which matters when a human is driving this by hand.
                    Err(e) => eprintln!("neosh: ignoring unparseable input: {e}"),
                }
            }
            let _ = tx.send(InputEvent::Disconnected);
        });
        (Self { out: std::io::stdout() }, rx)
    }
}

/// A closed pipe is a disconnect; anything else is a real I/O failure worth reporting.
fn classify(e: std::io::Error) -> anyhow::Error {
    match e.kind() {
        std::io::ErrorKind::BrokenPipe => anyhow::Error::new(Disconnected),
        _ => anyhow::Error::new(e),
    }
}

#[async_trait::async_trait]
impl Frontend for StdioFrontend {
    async fn send(&mut self, batch: Vec<UiEvent>) -> anyhow::Result<()> {
        // Serialized to a buffer first, so a write failure is an `io::Error` we can classify
        // rather than a serde error that happens to wrap one.
        let mut buf = Vec::new();
        for ev in batch {
            serde_json::to_writer(&mut buf, &ev)?;
            buf.push(b'\n');
        }
        // Flushed per batch rather than per event: a consumer sees whole frames, and a streaming
        // turn does not cost one syscall per token.
        self.out.write_all(&buf).and_then(|()| self.out.flush()).map_err(classify)
    }
}
