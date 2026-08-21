//! The terminal, as a viewer of a workspace running somewhere else.
//!
//! Everything here is screen and keyboard and nothing else. It holds no buffers, no sessions, no
//! plugin runtime and no opinion about any of them — it folds `UiEvent`s into a [`Mirror`] and
//! draws, exactly as the in-process terminal frontend always did, because it *is* that frontend
//! with a socket where the function call used to be.
//!
//! That is the test of whether the protocol boundary was real. It was: [`TerminalUi`] needed no
//! changes at all.

use std::path::{Path, PathBuf};

use neosh_proto::{ClientMessage, DetachReason, InputEvent, ServerMessage, UiEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::frontend::{Frontend, TerminalUi};

/// How long to keep trying the socket of a workspace we have just started.
///
/// A cold start reads config, boots deno and discovers plugins before it binds. Polling rather
/// than sleeping a fixed time so the common case — a warm workspace, or a fast machine — costs a
/// few milliseconds rather than a fixed pessimistic wait.
const SPAWN_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
const SPAWN_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Why the terminal stopped drawing.
pub enum Ended {
    /// The viewer left; the workspace carries on.
    Detached(DetachReason),
    /// The workspace itself is gone.
    Stopped,
}

/// Connect to the workspace for these paths, starting one if there is none.
///
/// `args` are the arguments to hand a workspace we start ourselves — the model, the plugin
/// directories, the working directory. They are *not* sent to one that is already running: a
/// workspace's configuration is its own, and a second terminal is a viewer rather than a second
/// opinion about which model to use.
pub async fn connect_or_start(socket: &Path, args: &[String]) -> anyhow::Result<UnixStream> {
    if let Ok(s) = UnixStream::connect(socket).await {
        return Ok(s);
    }
    // Nothing answering. A leftover file from a workspace that died is removed by the one we are
    // about to start, which connects before it binds for exactly this reason.
    start(args)?;

    let deadline = std::time::Instant::now() + SPAWN_WAIT;
    let mut last: Option<std::io::Error> = None;
    while std::time::Instant::now() < deadline {
        match UnixStream::connect(socket).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(SPAWN_POLL).await;
    }
    let why = last.map(|e| e.to_string()).unwrap_or_else(|| "no reason given".into());
    anyhow::bail!(
        "started a workspace but it never came up on {} ({why}). Run `neosh --serve` in another \
         terminal to see why.",
        socket.display()
    )
}

/// Start a workspace in the background, detached from this terminal.
///
/// Detached is the point: it must survive the shell that started it, the terminal that shell is
/// in, and the ssh session that terminal is in. Its own session (`setsid`) is what stops a
/// `SIGHUP` on the way out from taking the workspace with it.
fn start(args: &[String]) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--serve")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Safety: `setsid` is async-signal-safe and this runs between fork and exec, which is the
        // only place it is allowed to run at all.
        unsafe {
            cmd.pre_exec(|| {
                libc_setsid();
                Ok(())
            });
        }
    }
    cmd.spawn()?;
    Ok(())
}

#[cfg(unix)]
fn libc_setsid() {
    // Declared here rather than taking a dependency on `libc` for one call. A failure means we are
    // already a session leader, which is fine and is the only way it can fail.
    unsafe extern "C" {
        fn setsid() -> i32;
    }
    unsafe {
        setsid();
    }
}

#[cfg(not(unix))]
fn libc_setsid() {}

/// Ask a workspace what it is holding, without attaching to it.
pub async fn status(socket: &Path) -> anyhow::Result<Option<neosh_proto::WorkspaceStatus>> {
    let Ok(stream) = UnixStream::connect(socket).await else { return Ok(None) };
    let (read, mut write) = stream.into_split();
    let mut lines = tokio::io::BufReader::new(read).lines();
    write.write_all(&line(&ClientMessage::Status)?).await?;
    write.flush().await?;
    while let Some(l) = lines.next_line().await? {
        if let Ok(ServerMessage::Status { status }) = serde_json::from_str(&l) {
            return Ok(Some(status));
        }
    }
    Ok(None)
}

/// Ask a workspace to stop. Returns false when there was nothing running.
pub async fn stop(socket: &Path) -> anyhow::Result<bool> {
    let Ok(stream) = UnixStream::connect(socket).await else { return Ok(false) };
    let (read, mut write) = stream.into_split();
    write.write_all(&line(&ClientMessage::Stop)?).await?;
    write.flush().await?;
    // Wait for the far end to close, which is the workspace having finished writing its
    // conversations to disk. Returning before that would make `neosh stop && neosh` a race that
    // loses the last turn about one time in twenty.
    let mut lines = tokio::io::BufReader::new(read).lines();
    while lines.next_line().await?.is_some() {}
    Ok(true)
}

fn line(msg: &ClientMessage) -> anyhow::Result<Vec<u8>> {
    let mut v = serde_json::to_vec(msg)?;
    v.push(b'\n');
    Ok(v)
}

/// Attach this terminal to `stream` and draw until something ends it.
pub async fn attach(stream: UnixStream) -> anyhow::Result<Ended> {
    let (mut ui, mut input_rx) = TerminalUi::enter()?;
    let result = run(stream, &mut ui, &mut input_rx).await;
    // The terminal comes back whatever happened, including a failure: leaving somebody in the
    // alternate screen with no program running is worse than any error message.
    let _ = ui.shutdown().await;
    result
}

async fn run(
    stream: UnixStream,
    ui: &mut TerminalUi,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
) -> anyhow::Result<Ended> {
    let (read, mut write) = stream.into_split();
    let mut lines = tokio::io::BufReader::new(read).lines();

    // The size goes with the attach: a workspace that has been headless has no idea how big the
    // terminal now looking at it is, and one frame drawn at the wrong size is a visible flash of
    // the wrong layout.
    //
    // Taken from the terminal frontend's own first event rather than measured again here. It sends
    // exactly one `Ready` the moment it takes the screen, before it starts reading keys, so this
    // is both the first thing in the channel and the size it will actually draw at.
    let (width, height) = match input_rx.recv().await {
        Some(InputEvent::Ready { width, height }) => (width, height),
        other => anyhow::bail!("the terminal did not report its size (got {other:?})"),
    };
    write
        .write_all(&line(&ClientMessage::Attach {
            protocol_version: neosh_proto::PROTOCOL_VERSION,
            width,
            height,
        })?)
        .await?;
    write.flush().await?;

    loop {
        tokio::select! {
            Some(ev) = input_rx.recv() => {
                // A repaint is the terminal asking to draw the same state again while something on
                // it moves. Answered here rather than at the far end: it changes nothing the
                // workspace owns, and a round trip per animation frame would put the socket in the
                // path of a twenty-hertz shimmer.
                if matches!(ev, InputEvent::Repaint) {
                    ui.send(vec![UiEvent::Flush]).await?;
                    continue;
                }
                if matches!(ev, InputEvent::Disconnected) {
                    // The terminal went away — the window was closed, the ssh session dropped.
                    // Say so, so the workspace releases the slot rather than waiting for a write
                    // to fail, and leave.
                    let _ = write.write_all(&line(&ClientMessage::Detach)?).await;
                    let _ = write.flush().await;
                    return Ok(Ended::Detached(DetachReason::Asked));
                }
                let msg = ClientMessage::Input { event: ev };
                if write.write_all(&line(&msg)?).await.is_err() {
                    return Ok(Ended::Stopped);
                }
                let _ = write.flush().await;
            }
            line = lines.next_line() => {
                let Ok(Some(l)) = line else {
                    // The socket closed under us, which is the workspace having stopped.
                    return Ok(Ended::Stopped);
                };
                match serde_json::from_str::<ServerMessage>(&l) {
                    Ok(ServerMessage::Events { batch }) => {
                        let leaving = batch.iter().any(|e| matches!(e, UiEvent::Shutdown));
                        ui.send(batch).await?;
                        if leaving {
                            return Ok(Ended::Stopped);
                        }
                    }
                    Ok(ServerMessage::Detached { reason }) => return Ok(Ended::Detached(reason)),
                    Ok(ServerMessage::Refused { reason, .. }) => anyhow::bail!("{reason}"),
                    Ok(ServerMessage::Attached { .. }) | Ok(ServerMessage::Status { .. }) => {}
                    Err(e) => tracing::warn!("ignoring unparseable workspace message: {e}"),
                }
            }
            else => return Ok(Ended::Stopped),
        }
    }
}

/// The arguments a workspace we start should be given.
///
/// Deliberately not "everything on this command line": `--serve` and the client-only flags would
/// be nonsense to pass on, and a flag that only makes sense once must not be handed to a process
/// that may already be running with a different answer.
pub fn workspace_args(
    config_dir: &Option<PathBuf>,
    cwd: &Path,
    model: &Option<String>,
    plugin_dirs: &[PathBuf],
    mock_script: &Option<PathBuf>,
) -> Vec<String> {
    let mut out = vec!["--cwd".into(), cwd.display().to_string()];
    if let Some(c) = config_dir {
        out.push("--config-dir".into());
        out.push(c.display().to_string());
    }
    if let Some(m) = model {
        out.push("--model".into());
        out.push(m.clone());
    }
    for d in plugin_dirs {
        out.push("--plugin-dir".into());
        out.push(d.display().to_string());
    }
    if let Some(m) = mock_script {
        out.push("--mock-script".into());
        out.push(m.display().to_string());
    }
    out
}
