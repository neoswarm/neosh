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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use neosh_proto::InputEvent;
use neosh_tui::{Mirror, TerminalFrontend, to_input_event};
use tokio::sync::mpsc;

use crate::clients::{ClientId, Frame};

#[async_trait::async_trait]
pub trait Frontend: Send {
    /// Deliver one coalesced frame. It always ends with [`UiEvent::Flush`].
    ///
    /// Every event carries the view it is about, or `None` for the ones that are the workspace's —
    /// buffer contents, highlights, messages. A frontend that *is* one terminal ignores the tag and
    /// applies the lot; a workspace serving several gives each of them their share.
    async fn send(&mut self, frame: Frame) -> anyhow::Result<()>;
    async fn shutdown(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Send one viewer away and keep going.
    ///
    /// Named, because a workspace can have several and `^Q` means "close *this* terminal". The key
    /// arrives tagged with the client it was pressed in, so the answer never depends on which
    /// terminal happened to speak most recently.
    ///
    /// Nothing for a frontend that *is* the process — there is nobody to send away, and the run
    /// loop is about to end anyway. It means something only for a workspace serving terminals,
    /// which is the one place `quit` and `stop` are different words.
    async fn detach(&mut self, _client: ClientId) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Renders to the terminal.
pub struct TerminalUi {
    inner: TerminalFrontend,
    mirror: Mirror,
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

        tokio::spawn(async move {
            use futures::StreamExt;
            let mut events = crossterm::event::EventStream::new();
            while let Some(Ok(ev)) = events.next().await {
                if let Some(input) = to_input_event(ev)
                    && tx.send(input).is_err()
                {
                    break;
                }
            }
            let _ = tx.send(InputEvent::Disconnected);
        });

        Ok((
            Self {
                inner,
                mirror: Mirror::new(),
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
    async fn send(&mut self, frame: Frame) -> anyhow::Result<()> {
        let mut redraw = false;
        // The tag is nobody's business here: this process is one terminal, so every view in the
        // workspace is this one and every event in the frame is addressed to it.
        for (_, ev) in frame {
            redraw |= self.mirror.apply(ev);
        }
        // Before the draw: an escape written after ratatui has painted and moved the cursor is an
        // escape the terminal sees in the middle of a frame it has already committed.
        if let Some(text) = self.mirror.clipboard.take() {
            // A terminal that does not do OSC 52 ignores it; a broken pipe on the way out is the
            // shutdown path and not worth failing a redraw over.
            let _ = self.inner.copy(&text);
        }
        if redraw && !self.mirror.shutdown {
            let geometry = self.inner.draw(&self.mirror)?;
            self.report_geometry(&geometry);
            self.follow_motion(neosh_tui::shimmer::take_drew_animation());
        }
        Ok(())
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
    async fn send(&mut self, frame: Frame) -> anyhow::Result<()> {
        // Serialized to a buffer first, so a write failure is an `io::Error` we can classify
        // rather than a serde error that happens to wrap one.
        //
        // The view tag is dropped rather than printed: this stream is what a single frontend
        // renders, the same events a terminal would apply, and a consumer of it is one view by
        // construction. Putting a number nobody can act on into every line would make the two
        // frontends disagree about what the protocol is.
        let mut buf = Vec::new();
        for (_, ev) in frame {
            serde_json::to_writer(&mut buf, &ev)?;
            buf.push(b'\n');
        }
        // Flushed per batch rather than per event: a consumer sees whole frames, and a streaming
        // turn does not cost one syscall per token.
        self.out.write_all(&buf).and_then(|()| self.out.flush()).map_err(classify)
    }
}
