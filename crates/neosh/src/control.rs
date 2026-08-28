//! Talking to a workspace with no screen in the way.
//!
//! # Why there is no second vocabulary here
//!
//! Everything below sends an [`ApiCall`] — the same call a plugin makes — and reads back an
//! [`ApiResponse`]. There is no `ControlRequest` enum listing what a script is allowed to do,
//! because every one of those verbs already exists on the plugin API and a second list of them is
//! a second list to keep in step. What `neosh agent` can do is therefore exactly what a plugin can
//! do, and a capability added for one arrives in the other with nobody writing a line.
//!
//! # This is not a terminal
//!
//! A control connection is not in [`crate::clients::Clients`], is never sent a frame, never takes
//! a terminal over, and is not what `neosh status` means by "a terminal is attached" — which
//! matters, because whether anybody is watching is what decides where a notification goes. Ten
//! scripts polling a workspace must not make it believe somebody is sitting at it.
//!
//! # One connection, two streams
//!
//! A connection may [`Control::subscribe`] and go on making calls. Answers and events are
//! interleaved on the wire, so [`Control::call`] holds any event it walks past and
//! [`Control::event`] hands them back in order. That is what lets a caller subscribe *first* and
//! then start a turn — the other order is a race that a short turn wins.

use std::collections::VecDeque;
use std::path::Path;

use neosh_proto::{ApiCall, ApiOk, ApiResponse, ClientMessage, PluginEvent, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// How long to wait for the answer to the *first* call before deciding nobody is going to send
/// one.
///
/// Only the first. A workspace from before this protocol existed does not fail the call, it
/// ignores the line — the accept loop skips what it cannot parse rather than disconnecting,
/// which is right for a person driving the socket by hand and is indistinguishable, from here,
/// from a workspace thinking hard. Every call after the first has been answered by a workspace
/// that has already proved it can, so it waits as long as the work takes: `git status` on a large
/// repository, a model listing that is a network round trip per provider.
const FIRST_ANSWER: std::time::Duration = std::time::Duration::from_secs(20);

/// A workspace, reachable and answering.
pub struct Control {
    write: OwnedWriteHalf,
    lines: Lines<BufReader<OwnedReadHalf>>,
    /// Events read while waiting for an answer. See the module docs.
    held: VecDeque<PluginEvent>,
    next: u64,
    asked: bool,
}

impl Control {
    /// Connect to the workspace for these paths, starting one if there is none.
    ///
    /// `args` are what to hand a workspace we start ourselves, exactly as a terminal would — see
    /// [`crate::client::workspace_args`]. A workspace that is already running is not told any of
    /// it: its configuration is its own, and a script is no more entitled to change the model
    /// under somebody's running turn than a second terminal is.
    pub async fn connect(socket: &Path, args: &[String]) -> anyhow::Result<Self> {
        let stream = crate::client::connect_or_start(socket, args).await?;
        Ok(Self::over(stream))
    }

    /// Connect only if something is already there.
    ///
    /// For the verbs that are questions about a workspace rather than work to do in one: starting
    /// a whole workspace — plugins, catalogues, a JS heap — to answer `neosh agent ls` with an
    /// empty list would be a strange thing to have done.
    pub async fn existing(socket: &Path) -> anyhow::Result<Option<Self>> {
        match UnixStream::connect(socket).await {
            Ok(s) => Ok(Some(Self::over(s))),
            Err(_) => Ok(None),
        }
    }

    fn over(stream: UnixStream) -> Self {
        let (read, write) = stream.into_split();
        Self {
            write,
            lines: BufReader::new(read).lines(),
            held: VecDeque::new(),
            next: 0,
            asked: false,
        }
    }

    /// Make one call and wait for its answer.
    ///
    /// An [`neosh_proto::ApiError`] comes back as an `Err` carrying the sentence the API wrote:
    /// those messages are written to be read by a person — they say the fix and not just the
    /// refusal — and re-wording them here would lose that.
    pub async fn call(&mut self, call: ApiCall) -> anyhow::Result<ApiOk> {
        self.next += 1;
        let id = self.next.to_string();
        self.send(&ClientMessage::Call { id: id.clone(), call }).await?;
        let first = !std::mem::replace(&mut self.asked, true);
        let waiting = self.answer(&id);
        let response = match first {
            false => waiting.await?,
            true => match tokio::time::timeout(FIRST_ANSWER, waiting).await {
                Ok(r) => r?,
                Err(_) => anyhow::bail!(
                    "the workspace did not answer. It has been running since before `neosh \
                     agent` existed, so it does not know what was asked — finish or stop what is \
                     running in it, then `neosh stop` and try again."
                ),
            },
        };
        match response {
            ApiResponse::Ok { value } => Ok(value),
            ApiResponse::Err { error } => Err(anyhow::anyhow!("{error}")),
        }
    }

    /// Read until the answer to `id` arrives, holding on to anything else on the way.
    async fn answer(&mut self, id: &str) -> anyhow::Result<ApiResponse> {
        loop {
            let Some(line) = self.lines.next_line().await? else {
                anyhow::bail!("the workspace closed the connection without answering");
            };
            match serde_json::from_str::<ServerMessage>(&line) {
                Ok(ServerMessage::Called { id: got, response }) if got == id => {
                    return Ok(response);
                }
                Ok(ServerMessage::Event { event }) => self.held.push_back(event),
                // An answer to a call we are no longer waiting for, or a frame for a terminal that
                // is not us. Neither is a reason to stop reading.
                _ => {}
            }
        }
    }

    /// Ask for everything this workspace broadcasts, from now until the connection closes.
    pub async fn subscribe(&mut self) -> anyhow::Result<()> {
        self.send(&ClientMessage::Subscribe).await
    }

    /// The next event, or `None` when the workspace has gone.
    pub async fn event(&mut self) -> anyhow::Result<Option<PluginEvent>> {
        if let Some(held) = self.held.pop_front() {
            return Ok(Some(held));
        }
        loop {
            let Some(line) = self.lines.next_line().await? else { return Ok(None) };
            if let Ok(ServerMessage::Event { event }) = serde_json::from_str::<ServerMessage>(&line)
            {
                return Ok(Some(event));
            }
        }
    }

    async fn send(&mut self, msg: &ClientMessage) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        self.write.write_all(&line).await?;
        self.write.flush().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reading answers
// ---------------------------------------------------------------------------

/// Unwrap one [`ApiOk`] variant, saying what came instead when it is the wrong one.
///
/// A wrong variant is this binary and the host disagreeing about the API, which is a bug rather
/// than something a user did — so it says both what it wanted and what arrived, because that pair
/// is the whole of the bug report.
macro_rules! unwrap_ok {
    ($value:expr, $pat:pat => $out:expr, $what:literal) => {
        match $value {
            $pat => Ok($out),
            other => Err(anyhow::anyhow!("expected {} from the workspace, got {other:?}", $what)),
        }
    };
}

pub(crate) use unwrap_ok;
