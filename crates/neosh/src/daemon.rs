//! The workspace, in a process of its own.
//!
//! # Why a terminal is the wrong place to keep an agent
//!
//! Everything expensive in neosh outlives the reason you opened a terminal. A turn takes minutes;
//! a plugin runtime takes a second to start and holds a model catalogue and a JS heap; a
//! conversation is the artefact. Tying all of it to a tty means closing the window kills the work,
//! and the one thing you can be sure of about a window is that it will be closed.
//!
//! So the workspace runs here and the terminal is a viewer. What survives is *everything*: the
//! turns in flight, the plugins, the sessions, the catalogue. What the terminal owns is the screen
//! and the keyboard.
//!
//! # This is not a new protocol
//!
//! The boundary already existed. [`crate::frontend::Frontend`] takes `UiEvent`s and something else
//! sends `InputEvent`s back, and `StdioFrontend` has been writing exactly that over a pipe since
//! before there was a daemon. All this adds is a socket in the middle and an envelope
//! ([`ClientMessage`] / [`ServerMessage`]) for the things a *connection* has to say and a view does
//! not: attaching, detaching, and being told somebody else took over.
//!
//! # However many terminals you open
//!
//! A second `neosh` used to take the workspace over and tell the first why, on the grounds that
//! there is one `Editor` and therefore one cursor, one scroll position and one size. That is still
//! true, and it turns out to be an argument for *mirroring* rather than for refusing: every
//! attached terminal shows the same thing, live, and closing one leaves the others alone. The
//! reason to open a second window is that a turn is running in the first, and taking it over is
//! exactly the moment you lose sight of that turn.
//!
//! What a connection has to reconcile is size, because several terminals report several of them.
//! See [`crate::clients`], which owns that and says why it is the smallest one.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use neosh_proto::{
    ClientMessage, DetachReason, InputEvent, RunningTurn, ServerMessage, WorkspaceStatus,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};

use crate::clients::{ClientId, Clients, Frame, Source};
use crate::frontend::Frontend;

/// What the workspace is doing, for anyone who asks without attaching.
///
/// Filled in by the host — which is the only thing that knows — and read by the accept loop, which
/// answers `neosh status` without disturbing the host at all. A snapshot behind a lock rather than
/// a request into the run loop, because a status query must be answerable while the host is busy;
/// that is precisely when it is asked.
#[derive(Default)]
pub struct Live {
    status: std::sync::Mutex<WorkspaceStatus>,
    /// When the last client detached, as seconds since the epoch. Zero while one is attached.
    detached_at: AtomicU64,
    started: AtomicU64,
}

/// Seconds since the epoch, as an unsigned count that only ever goes forward.
///
/// `host::now_secs` is `i64` because it stamps conversations, where a clock set backwards is worth
/// representing. Here it measures uptime, and an uptime that went negative would be a bug in the
/// arithmetic rather than news about the clock.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Live {
    pub fn new() -> Arc<Self> {
        let l = Arc::new(Self::default());
        l.started.store(now(), Ordering::Relaxed);
        l.detached_at.store(now(), Ordering::Relaxed);
        l
    }

    /// The host's periodic report of what it is holding.
    pub fn report(&self, conversations: usize, running: Vec<RunningTurn>) {
        let mut s = self.status.lock().expect("status lock poisoned");
        s.conversations = conversations;
        s.running = running;
    }

    fn snapshot(&self, attached: bool) -> WorkspaceStatus {
        let now = now();
        let mut s = self.status.lock().expect("status lock poisoned").clone();
        s.attached = attached;
        s.uptime_secs = now.saturating_sub(self.started.load(Ordering::Relaxed));
        s.idle_secs = match attached {
            true => 0,
            false => now.saturating_sub(self.detached_at.load(Ordering::Relaxed)),
        };
        // Captured at startup, so this is the build that is running rather than the one on disk —
        // which after a rebuild is a different program with the same path.
        s.build = crate::build::capture().clone();
        s
    }
}

/// The frontend of a workspace that may have several terminals looking at it, or none.
///
/// A frame with nobody to draw it goes nowhere, and that is correct rather than lossy: the events
/// are deltas against a mirror, every one of them is already reflected in the editor's own state,
/// and the next client to attach is told the state rather than the history. Dropping them is also
/// what keeps a headless workspace from growing a queue for the rest of its life.
pub struct Viewer {
    clients: Arc<Mutex<Clients>>,
}

impl Viewer {
    fn new(clients: Arc<Mutex<Clients>>) -> Self {
        Self { clients }
    }
}

#[async_trait::async_trait]
impl Frontend for Viewer {
    /// One viewer asked to leave — `^Q` in that terminal, and only that one. Tell it so, and let
    /// go of it.
    ///
    /// Dropped here rather than left for the reader task to notice, so the very next frame the
    /// host draws goes to the terminals that are still there instead of racing a socket that is
    /// closing.
    async fn detach(&mut self, client: ClientId) -> anyhow::Result<()> {
        let mut clients = self.clients.lock().await;
        clients.tell(client, ServerMessage::Detached { reason: DetachReason::Asked });
        clients.detach(client);
        Ok(())
    }

    async fn send(&mut self, frame: Frame) -> anyhow::Result<()> {
        // A send failure is a client that went away between the lock and the write, which the
        // reader task notices and cleans up. Not the host's problem, and certainly not a reason to
        // end a turn.
        self.clients.lock().await.send(&frame);
        Ok(())
    }

    /// Nothing attached is `Detached`, which is a different answer from `Away` and needs a
    /// different channel: there is no stream to write an escape to.
    async fn presence(&mut self, idle_after: std::time::Duration) -> crate::frontend::Presence {
        let clients = self.clients.lock().await;
        if clients.is_empty() {
            return crate::frontend::Presence::Detached;
        }
        match clients.away(idle_after) {
            true => crate::frontend::Presence::Away,
            false => crate::frontend::Presence::Watching,
        }
    }
}

/// A workspace listening for viewers.
pub struct Daemon {
    listener: UnixListener,
    clients: Arc<Mutex<Clients>>,
    to_host: mpsc::UnboundedSender<(Source, InputEvent)>,
    live: Arc<Live>,
}

impl Daemon {
    /// Take the socket, or explain who already has it.
    ///
    /// A socket file is not a lock: it survives the process that made it, so a crash leaves one
    /// behind and binding over it would let a second workspace start while the first is running.
    /// The test is therefore behavioural — *connect* to it. Something answers, and this is not our
    /// socket to take; nothing answers, and it is a leftover, which is removed and rebound.
    pub async fn bind(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            // The socket is a way into a process that can read your files and spend your API
            // credits. On a shared machine, `/tmp` fallback and all, it is nobody else's business.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        if path.exists() {
            if UnixStream::connect(path).await.is_ok() {
                anyhow::bail!(
                    "a workspace is already running on {} — attach with `neosh`, or stop it with \
                     `neosh stop`",
                    path.display()
                );
            }
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        let (to_host, _) = mpsc::unbounded_channel();
        Ok(Self {
            listener,
            clients: Arc::new(Mutex::new(Clients::default())),
            to_host,
            live: Live::new(),
        })
    }

    pub fn live(&self) -> Arc<Live> {
        self.live.clone()
    }

    /// The frontend to hand the host, and the input channel to run it on.
    ///
    /// Called once, before [`Self::serve`]. The host then runs forever, with clients coming and
    /// going underneath it — which is the whole point: the run loop never learns that anybody
    /// attached, beyond one [`InputEvent::Attached`] telling it to say everything again.
    pub fn wire(&mut self) -> (Box<dyn Frontend>, mpsc::UnboundedReceiver<(Source, InputEvent)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.to_host = tx;
        (Box::new(Viewer::new(self.clients.clone())), rx)
    }

    /// Accept clients until the process ends. Runs alongside the host, not instead of it.
    pub async fn serve(self) {
        loop {
            let Ok((stream, _)) = self.listener.accept().await else { continue };
            let this = Handle {
                clients: self.clients.clone(),
                to_host: self.to_host.clone(),
                live: self.live.clone(),
            };
            tokio::spawn(async move { this.client(stream).await });
        }
    }

    /// Take the socket file with us. A workspace that is gone should not leave something for the
    /// next `neosh` to try to connect to.
    pub fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

#[derive(Clone)]
struct Handle {
    clients: Arc<Mutex<Clients>>,
    to_host: mpsc::UnboundedSender<(Source, InputEvent)>,
    live: Arc<Live>,
}

impl Handle {
    /// One connection, from its first line to its last.
    async fn client(self, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut lines = tokio::io::BufReader::new(read).lines();

        // The writer is its own task so the host never blocks on a socket. A client that has
        // stopped reading — suspended, or on the far end of a wedged ssh — must cost a queue and
        // not a stalled turn.
        let (out, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let writer = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let Ok(mut line) = serde_json::to_vec(&msg) else { continue };
                line.push(b'\n');
                if write.write_all(&line).await.is_err() || write.flush().await.is_err() {
                    break;
                }
            }
        });

        let mut source: Option<Source> = None;
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let msg = match serde_json::from_str::<ClientMessage>(&line) {
                Ok(m) => m,
                // Reported and skipped, exactly as the stdio frontend does: a client driving this
                // by hand should be told which line was wrong, not disconnected.
                Err(e) => {
                    tracing::warn!("ignoring unparseable client message: {e}");
                    continue;
                }
            };
            match msg {
                ClientMessage::Status => {
                    let attached_now = !self.clients.lock().await.is_empty();
                    let _ = out.send(ServerMessage::Status {
                        status: self.live.snapshot(attached_now),
                    });
                    break;
                }
                ClientMessage::Stop => {
                    // `stop` and not `quit`: in a workspace those are different words, and this
                    // is the one that means it. It goes through the command registry like any
                    // other, so the workspace leaves the way it always has — conversations
                    // flushed to disk, plugins torn down.
                    let _ = self.to_host.send((
                        Source::LOCAL,
                        InputEvent::Command { name: "stop".into(), args: Vec::new() },
                    ));
                    // And the connection stays open until the host has actually gone: `neosh
                    // stop` reads "stopped" off this socket closing, and closing it here, with
                    // the host still writing conversations to disk and saying goodbye to its
                    // plugins, made `neosh stop && neosh --serve` find the old socket answering.
                    self.to_host.closed().await;
                    break;
                }
                ClientMessage::Attach { protocol_version, width, height, build } => {
                    if protocol_version != neosh_proto::PROTOCOL_VERSION {
                        let _ = out.send(ServerMessage::Refused {
                            reason: format!(
                                "this workspace speaks protocol {} and you speak {protocol_version} \
                                 — it has been running since before neosh was upgraded. Finish or \
                                 stop what it is doing, then `neosh stop`.",
                                neosh_proto::PROTOCOL_VERSION
                            ),
                            protocol_version: neosh_proto::PROTOCOL_VERSION,
                        });
                        break;
                    }
                    // Alongside whoever is already here, rather than instead of them — and in a
                    // place of its own rather than as a copy of theirs. The host hears about the
                    // view on the `Attached` below and builds it a screen.
                    let from = self.clients.lock().await.attach(out.clone(), (width, height));
                    source = Some(from);
                    // Ours, whatever the terminal's turned out to be. Which of the two is
                    // behind is the terminal's to work out and to say — it is the thing with a
                    // screen, and it is the one that can still be rebuilt without anybody having
                    // to stop what is running here.
                    if !build.same_as(crate::build::capture()) {
                        tracing::info!(
                            workspace = ?crate::build::capture(),
                            terminal = ?build,
                            "a terminal attached from a different build"
                        );
                    }
                    let _ = out.send(ServerMessage::Attached {
                        protocol_version: neosh_proto::PROTOCOL_VERSION,
                        build: crate::build::capture().clone(),
                    });
                    // Its own size. It used to be the smallest attached terminal's, because every
                    // view shared one transcript; a view has its own now, so a wide window is
                    // furnished wide and nobody else's screen is involved.
                    let _ = self.to_host.send((from, InputEvent::Attached { width, height }));
                }
                ClientMessage::Input { event } => {
                    let Some(from) = source else {
                        tracing::warn!("input before attach, ignored");
                        continue;
                    };
                    let id = from.client;
                    // Whether somebody is at *this* terminal, which the workspace is away only if
                    // every terminal says no to. Recorded here beside the other per-client facts,
                    // and not forwarded: the host asks the frontend the merged question rather
                    // than tracking a set of terminals it cannot see.
                    if let InputEvent::Key { .. } | InputEvent::Paste { .. } = event {
                        self.clients.lock().await.typed(id);
                    }
                    // A resize is about this terminal's shape and nobody else's, now that a
                    // transcript belongs to a view — so it is recorded and forwarded as its own.
                    let forward = match event {
                        InputEvent::Focus { on } => {
                            self.clients.lock().await.focused(id, on);
                            // Nothing downstream acts on it, and forwarding it would arm a redraw
                            // for an event that changes not one cell.
                            None
                        }
                        InputEvent::Resize { width, height } => {
                            match self.clients.lock().await.resized(id, (width, height)) {
                                Some((width, height)) => Some(InputEvent::Resize { width, height }),
                                // Somebody smaller is still attached, so nothing the host draws
                                // changes. The view re-lays-out its own geometry regardless.
                                None => None,
                            }
                        }
                        InputEvent::ViewportChanged { win, width, height, top_line, rows } => {
                            self.clients.lock().await.viewport(id, win, (width, height, top_line, rows))
                        }
                        other => Some(other),
                    };
                    let Some(event) = forward else { continue };
                    if self.to_host.send((from, event)).is_err() {
                        break;
                    }
                }
                ClientMessage::Detach => {
                    let _ = out.send(ServerMessage::Detached { reason: DetachReason::Asked });
                    break;
                }
            }
        }

        // However this connection ended — asked to leave, took a signal, or the terminal it was in
        // was closed — the workspace keeps going, and so does every other terminal looking at it.
        // That is the entire point of it being here.
        if let Some(from) = source {
            let (remerged, empty, orphaned) = {
                let mut clients = self.clients.lock().await;
                // `detach` may already have happened: `^Q` is answered by the host, which lets go
                // of the view before this connection has finished winding down.
                clients.detach(from.client);
                let orphaned = !clients.watching(from.view);
                // Nobody reports a size when somebody *else* closes a window, so a view two
                // terminals were sharing — pinned narrow by the one that has just gone — has to be
                // given its width back from here or it stays narrow for as long as it runs.
                (clients.remerge(), clients.is_empty(), orphaned)
            };
            // Nobody is looking at that screen any more, so the host can take it down. Said from
            // here because a client cannot know whether it was the last one on its view, and a
            // client whose socket died says nothing at all.
            if orphaned {
                let _ = self.to_host.send((from, InputEvent::Detached));
            }
            for ev in remerged {
                let _ = self.to_host.send((from, ev));
            }
            // Stamped from what is true now rather than from having just removed ourselves: the
            // question `neosh status` asks is whether *anybody* is looking.
            if empty {
                self.live.detached_at.store(now(), Ordering::Relaxed);
            }
        }
        drop(out);
        let _ = writer.await;
    }
}
