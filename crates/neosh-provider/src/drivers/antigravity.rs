//! Google's Antigravity, through the `agy` CLI.
//!
//! # Why this and not the ACP runtime
//!
//! Antigravity ships two headless surfaces. One is an ACP server published to the ACP registry —
//! a ~540 MB download of `agy_acp_server.par` plus a `localharness_external` helper, which a
//! client is expected to fetch, verify and keep updated itself. The other is `agy`, the ordinary
//! CLI, which a person installs the way they installed `claude` and `codex`.
//!
//! neosh takes the CLI, for the same reason it takes `claude` in stream-json mode rather than
//! `claude -p`: **take the transport that can do the most**, and prefer the one that is already
//! how the user signs in. `agy` carries its own Google login — including the Antigravity access
//! that comes with a Google AI subscription — so a workspace that can see `agy` on `PATH` can use
//! that plan with no API key and nothing to paste. A managed 540 MB runtime download would also
//! make neosh the updater for somebody else's binary, which is a job it has declined everywhere
//! else: `AuthRef::Cli` exists precisely so a vendor CLI stays the vendor's to install.
//!
//! # The transport
//!
//! `agy -p= --input-format stream-json --output-format stream-json` is NDJSON both ways over one
//! long-lived process — one turn per line in, a stream of events per turn out, and the whole thing
//! is *one conversation*, which is what lets a driver hold a session open. That is the same shape
//! [`super::claude_cli`] drives, and this driver is deliberately its sibling.
//!
//! Every frame is wrapped in an `event` envelope whose payload sits under a key of the same name:
//!
//! ```text
//! {"event":"init","conversation_id":"…","init":{"cwd":"…","tools":[…],"permission_mode":"…"}}
//! {"event":"step_update","step_update":{"step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"PONG"}}
//! {"event":"result","result":{"status":"SUCCESS","response":"PONG\n","num_turns":1,"usage":{…}}}
//! ```
//!
//! Going the other way, `"user"` is the **only** event `agy` accepts — every other name is
//! answered with `warning: ignoring unsupported stream input message event`, on stdout, in plain
//! text. Its body is the message shape the Anthropic API uses, which is a coincidence worth not
//! relying on and so is written out here rather than shared with a neighbour.
//!
//! # Non-JSON on stdout is normal, and one line of it is load-bearing
//!
//! `agy` writes warnings and notices to stdout interleaved with the NDJSON, so a line that does
//! not parse is skipped rather than fatal. One of them is not noise, though, and dropping it is
//! how this driver would have shipped with a silent failure mode — see below.
//!
//! # Permissions, honestly
//!
//! **`agy` headless cannot ask.** There is no answer channel: `"user"` is the only input event,
//! so a permission request has nowhere to go. What it does instead is auto-deny and say so, in
//! plain text on stdout:
//!
//! ```text
//! jetski: no output produced — a tool required the "command" permission that headless mode
//! cannot prompt for, so it was auto-denied. …
//! ```
//!
//! and then finish the turn `SUCCESS` with an empty `response`. A driver that only read the JSON
//! would report a turn that did nothing, as a success, with no reason anywhere on screen — which
//! is the worst answer available and the exact failure `acp.rs` counts refusals to avoid. So the
//! notice is lifted out of the text stream and re-emitted as a [`ProviderEvent::Error`] naming the
//! mode that caused it.
//!
//! That leaves the mode mapping, which is a statement about what the transport can do:
//!
//! | neosh mode | `agy` | what happens |
//! |---|---|---|
//! | full access | `--dangerously-skip-permissions` | everything runs; this is neosh's default |
//! | ask, allow-listed, deny | nothing | `agy` uses its own `request-review` and auto-denies what its own `permissions.allow` does not cover |
//!
//! Three of the four cannot be honoured as neosh means them, and pretending otherwise would be a
//! setting that changes nothing. What the driver can do is refuse to be silent about it, which is
//! what the notice above buys.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use neosh_proto::{
    Activity, BlockStartKind, ContentBlock, DriverKind, InstanceConfig, Message, ModelInfo,
    PermissionMode, ProviderEvent, Role, SessionId, StopReason, ToolCallId, TurnRequest, Usage,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{Provider, ProviderError, ProviderStream};

/// The prefix `agy` puts on the one plain-text line that explains an empty turn.
///
/// Matched on rather than parsed: it is a sentence written for a person, and the only thing this
/// needs from it is "this turn produced nothing, and here is why".
const DENIED: &str = "jetski: no output produced";

/// What a conversation's process is, once it has one.
#[derive(Debug)]
struct Live {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    child: tokio::process::Child,
    /// `agy`'s own id for this conversation, off the `init` frame. Travels back to the workspace
    /// as [`Activity::Resume`] so reopening a conversation after a restart resumes it rather than
    /// starting an agent that has never heard of it.
    conversation: Option<String>,
    /// The flags this process was started with. A mode or model change has to restart it, because
    /// both are launch flags and `agy` has no control channel to be told mid-session.
    launched: Launch,
}

/// The launch-time facts a running process cannot be talked out of.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Launch {
    model: String,
    /// `--dangerously-skip-permissions`, which is the only mode `agy` headless can actually honour.
    full_access: bool,
    /// `--mode plan`.
    planning: bool,
    cwd: std::path::PathBuf,
    resume: Option<String>,
}

impl Launch {
    fn of(request: &TurnRequest, mode: PermissionMode) -> Self {
        Self {
            model: request.selection.model.as_ref().to_string(),
            full_access: mode == PermissionMode::Allow,
            // Plan mode is a knob the driver advertises, not a permission and not a mode of
            // neosh's own: `--mode plan` is how Antigravity spells "work it out and tell me,
            // do not do it", and `^E` is where every other thing a model can be told already
            // lives. Making it a `PermissionMode` would have been a fifth answer to "may this
            // happen" that is not about permission at all.
            planning: request.selection.option_str("mode") == Some("plan"),
            cwd: request.cwd.clone(),
            resume: request.resume.clone(),
        }
    }

    fn args(&self) -> Vec<String> {
        // `-p=` and not `-p`: the flag takes a value, so a bare `-p` swallows whatever follows it
        // as the prompt — `agy` says so itself, and with `--input-format` next on the line it
        // would swallow that. An empty value is how you say "the prompts arrive on stdin".
        let mut args = vec![
            "-p=".to_string(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
        ];
        if !self.model.is_empty() {
            args.push("--model".into());
            args.push(self.model.clone());
        }
        if self.full_access {
            args.push("--dangerously-skip-permissions".into());
        }
        if self.planning {
            args.push("--mode".into());
            args.push("plan".into());
        }
        if let Some(id) = &self.resume {
            args.push("--conversation".into());
            args.push(id.clone());
        }
        args
    }
}

/// One conversation's process, and the lock that makes it single-writer.
#[derive(Debug, Default)]
struct Conversation {
    live: AsyncMutex<Option<Live>>,
}

#[derive(Debug)]
pub struct AntigravityProvider {
    program: String,
    /// One process per conversation, exactly as [`super::claude_cli`] keeps one — and for the
    /// reason stated there: several conversations run at once, and a driver holding a single
    /// session resumes the second into the first one's history.
    slots: Mutex<HashMap<SessionId, Arc<Conversation>>>,
    mode: Mutex<PermissionMode>,
}

impl Default for AntigravityProvider {
    fn default() -> Self {
        Self::new("agy")
    }
}

impl AntigravityProvider {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            slots: Mutex::default(),
            mode: Mutex::new(PermissionMode::Ask),
        }
    }

    /// Whether the CLI is on `PATH`, which is what decides if this provider is offered at all.
    pub fn available(&self) -> bool {
        super::claude_cli::which(&self.program).is_some()
    }

    /// The slot for a conversation, made on first use.
    ///
    /// A one-shot turn — a branch name, a commit message; [`TurnRequest::is_one_shot`] — gets a
    /// slot nobody else can find, because the empty conversation id is an ordinary map key and
    /// every one of them would otherwise share a process that nothing ever emptied.
    fn slot(&self, conversation: &SessionId) -> Arc<Conversation> {
        let key = if conversation.0.is_empty() {
            SessionId(format!("\u{0}one-shot-{:p}", self))
        } else {
            conversation.clone()
        };
        self.slots
            .lock()
            .expect("slots lock poisoned")
            .entry(key)
            .or_default()
            .clone()
    }
}

/// The one message shape `agy` accepts on stdin.
///
/// Images travel as the API's own image block, base64 inline: the picture lives once in the state
/// directory and is read here when the request is built, never carried through the transcript.
fn user_frame(messages: &[Message]) -> Value {
    let latest = messages.iter().rev().find(|m| m.role == Role::User);
    let mut content: Vec<Value> = Vec::new();
    if let Some(m) = latest {
        for block in &m.content {
            match block {
                ContentBlock::Image { path, media_type } => {
                    // A picture whose file has gone is dropped rather than failing the turn: the
                    // words are still worth sending.
                    if let Some(data) = crate::image::base64_at(path) {
                        content.push(json!({
                            "type": "image",
                            "source": { "type": "base64", "media_type": media_type, "data": data },
                        }));
                    }
                }
                ContentBlock::Text { text } if !text.trim().is_empty() => {
                    content.push(json!({ "type": "text", "text": text }));
                }
                _ => {}
            }
        }
    }
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }
    json!({ "event": "user", "message": { "role": "user", "content": content } })
}

/// `agy`'s token accounting, which happens to line up with neosh's field for field.
fn usage_of(v: &Value) -> Usage {
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        cache_read_tokens: n("cache_read_tokens"),
        cache_write_tokens: 0,
        thinking_tokens: n("thinking_tokens"),
    }
}

/// How a finished turn ended.
///
/// Takes the whole `result` body rather than just the status, because a failure carries its reason
/// in a sibling field and a stop reason that says only "error" is a turn that ended for no stated
/// cause.
fn stop_of(body: &Value) -> StopReason {
    let status = body.get("status").and_then(Value::as_str).unwrap_or("SUCCESS");
    match status {
        "SUCCESS" => StopReason::EndTurn,
        "CANCELLED" | "INTERRUPTED" => StopReason::Cancelled,
        _ => StopReason::Error {
            message: body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or(status)
                .to_string(),
        },
    }
}

/// Everything one turn needs to remember while it reads.
#[derive(Default)]
struct Reading {
    next_index: u32,
    /// `agy` addresses steps by index and neosh addresses blocks by position, so a step that opens
    /// a block has to be remembered until it closes.
    blocks: HashMap<i64, u32>,
    /// Text arrives as deltas on one logical answer; opened lazily so a turn that only ran tools
    /// does not draw an empty paragraph.
    text: Option<u32>,
    /// The plain-text notice explaining an empty turn, if one arrived.
    denied: Option<String>,
}

/// Turn one output frame into provider events.
///
/// A free function over parsed JSON so every shape here is testable with no `agy` on the machine —
/// which matters, because it is a 180 MB download nobody should need in order to run the tests.
fn events(frame: &Value, state: &mut Reading) -> Vec<ProviderEvent> {
    let Some(kind) = frame.get("event").and_then(Value::as_str) else { return Vec::new() };
    let body = frame.get(kind).unwrap_or(&Value::Null);
    let mut out = Vec::new();

    match kind {
        "init" => {}
        "step_update" => {
            let step = body.get("step_index").and_then(Value::as_i64).unwrap_or(-1);
            let state_of = body.get("state").and_then(Value::as_str).unwrap_or("");
            match body.get("step_type").and_then(Value::as_str).unwrap_or("") {
                "agent_response" => {
                    if let Some(text) =
                        body.get("text_delta").and_then(Value::as_str).filter(|t| !t.is_empty())
                    {
                        let index = *state.text.get_or_insert_with(|| {
                            let i = state.next_index;
                            state.next_index += 1;
                            out.push(ProviderEvent::BlockStart { index: i, block: BlockStartKind::Text });
                            i
                        });
                        out.push(ProviderEvent::TextDelta { index, text: text.to_string() });
                    }
                }
                "tool" => {
                    let info = body.get("tool_info").unwrap_or(&Value::Null);
                    let name = info
                        .get("name")
                        .or_else(|| body.get("tool_name"))
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    if state_of == "ACTIVE" {
                        // A run of text is finished by a tool starting: the answer continues in a
                        // new block afterwards rather than having a call spliced into the middle
                        // of a sentence.
                        if let Some(i) = state.text.take() {
                            out.push(ProviderEvent::BlockStop { index: i });
                        }
                        let index = state.next_index;
                        state.next_index += 1;
                        state.blocks.insert(step, index);
                        out.push(ProviderEvent::BlockStart {
                            index,
                            block: BlockStartKind::ToolUse {
                                id: ToolCallId(format!("agy-{step}")),
                                name,
                            },
                        });
                        if let Some(params) = info.get("parameters") {
                            out.push(ProviderEvent::ToolInputDelta {
                                index,
                                partial_json: params.to_string(),
                            });
                        }
                    } else if let Some(index) = state.blocks.remove(&step) {
                        out.push(ProviderEvent::BlockStop { index });
                        let error = info.get("error");
                        let content = error
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .or_else(|| {
                                info.get("output").and_then(Value::as_str).map(str::to_string)
                            })
                            .unwrap_or_default();
                        out.push(ProviderEvent::ToolResult {
                            id: ToolCallId(format!("agy-{step}")),
                            content,
                            // `ERROR` is the step's own word for it, and an `error` on `tool_info`
                            // is the same thing said in the payload. Either is enough.
                            is_error: state_of == "ERROR" || error.is_some(),
                        });
                    }
                }
                // The driver's account of its own loop, which is never a content block.
                "system_message" => {
                    if let Some(text) =
                        body.get("text").and_then(Value::as_str).filter(|t| !t.trim().is_empty())
                    {
                        out.push(ProviderEvent::Activity {
                            activity: Activity::Waiting { what: text.to_string(), retry_in_ms: None },
                        });
                    }
                }
                // `user_input` is our own message echoed back, and anything a later release adds
                // lands here rather than in a panic.
                _ => {}
            }
        }
        "result" => {
            if let Some(i) = state.text.take() {
                out.push(ProviderEvent::BlockStop { index: i });
            }
            // Any tool still open is closed rather than left spinning: a turn that ended with a
            // call in flight is a card that would otherwise animate forever.
            for (_, index) in std::mem::take(&mut state.blocks) {
                out.push(ProviderEvent::BlockStop { index });
            }
            let usage = body.get("usage").map(usage_of).unwrap_or_default();
            // The turn said nothing and the CLI told us why in prose. Reported as an error rather
            // than dropped: a `SUCCESS` with an empty answer and no reason on screen is
            // indistinguishable from the agent having nothing to say.
            if let Some(notice) = state.denied.take() {
                out.push(ProviderEvent::Error { message: notice, retryable: false });
            } else if let Some(message) = body.get("error").and_then(Value::as_str) {
                out.push(ProviderEvent::Error { message: message.to_string(), retryable: false });
            }
            out.push(ProviderEvent::MessageDelta { stop_reason: Some(stop_of(body)), usage });
            out.push(ProviderEvent::MessageStop);
        }
        _ => {}
    }
    out
}

/// What to do with a stdout line that is not JSON.
///
/// Most of it is `agy` talking to a person and is dropped. The auto-deny notice is not: it is the
/// only explanation a turn that produced nothing will ever get.
fn plain(line: &str, state: &mut Reading, mode: PermissionMode) -> Option<ProviderEvent> {
    let line = line.trim();
    if !line.starts_with(DENIED) {
        return None;
    }
    state.denied = Some(format!(
        "antigravity: {line}\n\nneosh is in {mode:?} mode. `agy` headless has no way to ask you, \
         so it denies what its own permissions.allow does not cover — switch this conversation to \
         full access with ⇧⇥ to let it act."
    ));
    None
}

#[async_trait::async_trait]
impl Provider for AntigravityProvider {
    fn driver(&self) -> DriverKind {
        DriverKind::from("antigravity")
    }

    fn delegates_agent_loop(&self) -> bool {
        true
    }

    /// What this account is actually offered, asked of the CLI rather than written down here.
    ///
    /// `agy models` prints `id<TAB>name`, and the id carries the reasoning effort — Antigravity
    /// publishes `gemini-3.8-flash-high` and `gemini-3.8-flash-low` as two models rather than one
    /// model with a knob, so that is how they are listed.
    async fn list_models(&self, _instance: &InstanceConfig) -> Result<Vec<ModelInfo>, ProviderError> {
        let out = Command::new(&self.program)
            .arg("models")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| ProviderError::Transport(format!("could not run `agy models`: {e}")))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let models: Vec<ModelInfo> = text
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(id, name)| ModelInfo::undescribed(id.trim(), name.trim()))
            .collect();
        if models.is_empty() {
            // The usual reason, and it names the fix rather than reporting an empty catalogue.
            return Err(ProviderError::MissingCredentials(
                "antigravity: `agy models` listed nothing — run `agy` once to sign in".into(),
            ));
        }
        Ok(models)
    }

    fn stream(
        &self,
        _instance: &InstanceConfig,
        request: TurnRequest,
        cancel: CancellationToken,
    ) -> ProviderStream {
        let (tx, rx) = mpsc::channel::<ProviderEvent>(256);
        let program = self.program.clone();
        let mode = *self.mode.lock().expect("mode lock poisoned");
        let slot = self.slot(&request.conversation);
        let one_shot = request.is_one_shot();
        let want = Launch::of(&request, mode);
        let frame = user_frame(&request.messages);
        let model = want.model.clone();

        tokio::spawn(async move {
            let mut held = slot.live.lock().await;

            // A launch flag changed, so the process cannot be told — it has to be replaced. Only
            // when it actually differs: restarting on every turn would throw away the session that
            // holding the process open exists to keep.
            if held.as_ref().is_some_and(|l| l.launched != want) {
                if let Some(live) = held.take() {
                    live.close().await;
                }
            }

            if held.is_none() {
                match Live::spawn(&program, &want).await {
                    Ok(live) => *held = Some(live),
                    Err(e) => {
                        let _ = tx.send(ProviderEvent::Error { message: e, retryable: false }).await;
                        return;
                    }
                }
            }
            let live = held.as_mut().expect("just spawned");

            let _ = tx
                .send(ProviderEvent::MessageStart { model: Some(model), usage: Usage::default() })
                .await;

            if let Err(e) = live.say(&frame).await {
                let _ = tx.send(ProviderEvent::Error { message: e, retryable: false }).await;
                *held = None;
                return;
            }

            let mut state = Reading::default();
            loop {
                let line = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        // Nothing to interrupt with: `agy` headless takes only `user` frames, so
                        // the process is closed and the conversation resumed by id next time —
                        // which is what `--conversation` is for, and why the id is kept.
                        if let Some(live) = held.take() { live.close().await; }
                        return;
                    }
                    r = live.lines.next_line() => r,
                };
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) | Err(_) => {
                        let _ = tx
                            .send(ProviderEvent::Error {
                                message: "antigravity: `agy` closed its output".into(),
                                retryable: false,
                            })
                            .await;
                        *held = None;
                        return;
                    }
                };

                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    if let Some(ev) = plain(&line, &mut state, mode) {
                        let _ = tx.send(ev).await;
                    }
                    continue;
                };

                // The id `agy` gave this conversation, said once so the workspace can resume it
                // after a restart. Kept on the process too, so a relaunch mid-conversation
                // continues rather than starting over.
                if v.get("event").and_then(Value::as_str) == Some("init") {
                    if let Some(id) = v.get("conversation_id").and_then(Value::as_str) {
                        if live.conversation.as_deref() != Some(id) {
                            live.conversation = Some(id.to_string());
                            live.launched.resume = Some(id.to_string());
                            let _ = tx
                                .send(ProviderEvent::Activity {
                                    activity: Activity::Resume { token: id.to_string() },
                                })
                                .await;
                        }
                    }
                }

                let done = v.get("event").and_then(Value::as_str) == Some("result");
                for ev in events(&v, &mut state) {
                    if tx.send(ev).await.is_err() {
                        return;
                    }
                }
                if done {
                    break;
                }
            }

            // A one-shot turn's process is closed with the turn: it belongs to no conversation, so
            // nothing will ever come back for it, and leaving it running is a second `agy` beside
            // the one you are talking to.
            if one_shot {
                if let Some(live) = held.take() {
                    live.close().await;
                }
            }
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

impl Live {
    async fn spawn(program: &str, launch: &Launch) -> Result<Self, String> {
        let workdir = if launch.cwd.as_os_str().is_empty() {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            launch.cwd.clone()
        };
        let mut child = Command::new(program)
            .args(launch.args())
            .current_dir(&workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited rather than piped: `agy` writes progress there, nothing here reads it, and
            // a pipe nobody drains is a child that stops when the buffer fills.
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not run {program:?}: {e}"))?;
        let stdin = child.stdin.take().ok_or("agy: stdin was not piped")?;
        let stdout = child.stdout.take().ok_or("agy: stdout was not piped")?;
        Ok(Self {
            stdin,
            lines: BufReader::new(stdout).lines(),
            child,
            conversation: None,
            launched: launch.clone(),
        })
    }

    async fn say(&mut self, frame: &Value) -> Result<(), String> {
        let mut line = frame.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
        self.stdin.flush().await.map_err(|e| e.to_string())
    }

    async fn close(mut self) {
        // Closing stdin is how `agy` is asked to finish: it reads one turn per line and stops at
        // end of input. Killing is the fallback, not the plan.
        drop(self.stdin);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), self.child.wait()).await;
        let _ = self.child.start_kill();
    }
}

impl super::AgentDriver for AntigravityProvider {
    fn set_permission_mode(&self, mode: PermissionMode) {
        *self.mode.lock().expect("mode lock poisoned") = mode;
    }

    fn close_conversation(&self, conversation: &SessionId) {
        let slot = self.slots.lock().expect("slots lock poisoned").remove(conversation);
        if let Some(slot) = slot {
            tokio::spawn(async move {
                if let Some(live) = slot.live.lock().await.take() {
                    live.close().await;
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(text: &str) -> Value {
        serde_json::from_str(text).expect("test fixture parses")
    }

    #[test]
    fn an_answer_streams_as_deltas_on_one_block() {
        let mut s = Reading::default();
        let a = events(
            &frame(
                r#"{"event":"step_update","step_update":{"step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"PO"}}"#,
            ),
            &mut s,
        );
        let b = events(
            &frame(
                r#"{"event":"step_update","step_update":{"step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"NG"}}"#,
            ),
            &mut s,
        );
        assert!(matches!(a[0], ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text }));
        assert!(matches!(&a[1], ProviderEvent::TextDelta { index: 0, text } if text == "PO"));
        // The second delta opens nothing: one answer is one block.
        assert_eq!(b.len(), 1);
        assert!(matches!(&b[0], ProviderEvent::TextDelta { index: 0, text } if text == "NG"));
    }

    #[test]
    fn a_tool_call_opens_on_active_and_answers_on_done() {
        let mut s = Reading::default();
        let open = events(
            &frame(
                r#"{"event":"step_update","step_update":{"step_index":2,"state":"ACTIVE","step_type":"tool","tool_name":"view_file","tool_info":{"name":"view_file","parameters":{"AbsolutePath":"/tmp/note.txt"}}}}"#,
            ),
            &mut s,
        );
        assert!(matches!(
            &open[0],
            ProviderEvent::BlockStart { index: 0, block: BlockStartKind::ToolUse { name, .. } }
                if name == "view_file"
        ));
        assert!(matches!(&open[1], ProviderEvent::ToolInputDelta { index: 0, partial_json }
            if partial_json.contains("/tmp/note.txt")));

        let done = events(
            &frame(
                r#"{"event":"step_update","step_update":{"step_index":2,"state":"DONE","step_type":"tool","tool_info":{"name":"view_file"}}}"#,
            ),
            &mut s,
        );
        assert!(matches!(done[0], ProviderEvent::BlockStop { index: 0 }));
        assert!(matches!(&done[1], ProviderEvent::ToolResult { is_error: false, .. }));
    }

    #[test]
    fn a_failed_tool_carries_the_cli_s_own_reason() {
        // The shape `agy` actually sends: `state: ERROR`, and the message under `tool_info.error`.
        let mut s = Reading::default();
        events(
            &frame(
                r#"{"event":"step_update","step_update":{"step_index":6,"state":"ACTIVE","step_type":"tool","tool_info":{"name":"list_dir"}}}"#,
            ),
            &mut s,
        );
        let done = events(
            &frame(
                r#"{"event":"step_update","step_update":{"step_index":6,"state":"ERROR","step_type":"tool","tool_info":{"name":"list_dir","error":{"type":"TOOL_ERROR","message":"user denied permission for read_file(/Users/x)"}}}}"#,
            ),
            &mut s,
        );
        let ProviderEvent::ToolResult { content, is_error, .. } = &done[1] else {
            panic!("expected a tool result, got {done:?}");
        };
        assert!(*is_error);
        assert!(content.contains("denied permission"), "{content}");
    }

    #[test]
    fn a_turn_that_only_ran_tools_draws_no_empty_paragraph() {
        let mut s = Reading::default();
        events(
            &frame(
                r#"{"event":"step_update","step_update":{"step_index":1,"state":"ACTIVE","step_type":"tool","tool_info":{"name":"run_command"}}}"#,
            ),
            &mut s,
        );
        let end = events(
            &frame(r#"{"event":"result","result":{"status":"SUCCESS","usage":{}}}"#),
            &mut s,
        );
        assert!(
            !end.iter().any(|e| matches!(e, ProviderEvent::BlockStart { .. })),
            "a text block was opened for a turn with no text in it: {end:?}"
        );
    }

    #[test]
    fn a_call_still_open_when_the_turn_ends_is_closed_rather_than_left_spinning() {
        let mut s = Reading::default();
        events(
            &frame(
                r#"{"event":"step_update","step_update":{"step_index":4,"state":"ACTIVE","step_type":"tool","tool_info":{"name":"run_command"}}}"#,
            ),
            &mut s,
        );
        let end = events(
            &frame(r#"{"event":"result","result":{"status":"SUCCESS","usage":{}}}"#),
            &mut s,
        );
        assert!(
            end.iter().any(|e| matches!(e, ProviderEvent::BlockStop { index: 0 })),
            "the open call was never closed: {end:?}"
        );
    }

    #[test]
    fn usage_comes_off_the_result_field_for_field() {
        let mut s = Reading::default();
        let end = events(
            &frame(
                r#"{"event":"result","result":{"status":"SUCCESS","usage":{"input_tokens":13694,"output_tokens":33,"thinking_tokens":31,"cache_read_tokens":7,"total_tokens":13727}}}"#,
            ),
            &mut s,
        );
        let ProviderEvent::MessageDelta { usage, stop_reason } = &end[0] else {
            panic!("expected a message delta, got {end:?}");
        };
        assert_eq!(usage.input_tokens, 13694);
        assert_eq!(usage.thinking_tokens, 31);
        assert_eq!(usage.cache_read_tokens, 7);
        assert_eq!(*stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn an_empty_turn_says_why_instead_of_reporting_success() {
        // The regression this driver exists to avoid: `agy` auto-denies in headless mode, says so
        // in prose on stdout, and then finishes the turn SUCCESS with an empty answer. Read as
        // JSON alone that is a turn which did nothing, reported as having worked.
        let mut s = Reading::default();
        let notice = "jetski: no output produced — a tool required the \"command\" permission that \
                      headless mode cannot prompt for, so it was auto-denied.";
        assert!(plain(notice, &mut s, PermissionMode::Ask).is_none());
        let end = events(
            &frame(r#"{"event":"result","result":{"status":"SUCCESS","response":"","usage":{}}}"#),
            &mut s,
        );
        let ProviderEvent::Error { message, .. } = &end[0] else {
            panic!("an empty turn reported nothing: {end:?}");
        };
        assert!(message.contains("auto-denied"), "{message}");
        assert!(message.contains("full access"), "the fix is not named: {message}");
    }

    #[test]
    fn ordinary_chatter_on_stdout_is_dropped() {
        let mut s = Reading::default();
        assert!(plain("warning: ignoring unsupported thing", &mut s, PermissionMode::Ask).is_none());
        assert!(s.denied.is_none(), "an unrelated line was kept as a denial notice");
    }

    #[test]
    fn a_frame_nobody_recognises_is_ignored() {
        let mut s = Reading::default();
        assert!(events(&frame(r#"{"event":"tomorrows_event","tomorrows_event":{}}"#), &mut s).is_empty());
        assert!(events(&frame(r#"{"no_event":true}"#), &mut s).is_empty());
    }

    #[test]
    fn the_prompt_flag_carries_its_empty_value() {
        // `-p` on its own swallows `--input-format` as the prompt, which `agy` reports as an error
        // and which cost an afternoon the first time.
        let launch = Launch {
            model: "gemini-3.8-flash-high".into(),
            full_access: false,
            planning: false,
            cwd: std::path::PathBuf::new(),
            resume: None,
        };
        let args = launch.args();
        assert_eq!(args[0], "-p=");
        assert!(args.windows(2).any(|w| w[0] == "--model" && w[1] == "gemini-3.8-flash-high"));
        assert!(!args.iter().any(|a| a == "--dangerously-skip-permissions"));
    }

    #[test]
    fn full_access_is_the_only_mode_agy_can_honour_and_plan_mode_is_a_knob() {
        let base = Launch {
            model: String::new(),
            full_access: false,
            planning: false,
            cwd: std::path::PathBuf::new(),
            resume: None,
        };
        let allow = Launch { full_access: true, ..base.clone() };
        assert!(allow.args().iter().any(|a| a == "--dangerously-skip-permissions"));

        let planning = Launch { planning: true, ..base.clone() };
        let args = planning.args();
        assert!(args.windows(2).any(|w| w[0] == "--mode" && w[1] == "plan"));
        // Plan mode is not a permission: it must not quietly grant one.
        assert!(!args.iter().any(|a| a == "--dangerously-skip-permissions"));
    }

    #[test]
    fn plan_mode_arrives_as_the_option_the_sheet_sets() {
        use neosh_proto::{InstanceId, ModelId, ModelSelection, OptionSelection, ProviderOptionValue};
        let mut request = TurnRequest {
            selection: ModelSelection {
                instance: InstanceId::from("antigravity"),
                model: ModelId::from("gemini-3.8-flash-high"),
                options: vec![OptionSelection {
                    id: "mode".into(),
                    value: ProviderOptionValue::Text("plan".into()),
                }],
            },
            conversation: SessionId(String::new()),
            cwd: std::path::PathBuf::new(),
            resume: None,
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: None,
        };
        assert!(Launch::of(&request, PermissionMode::Ask).planning);

        request.selection.options.clear();
        assert!(!Launch::of(&request, PermissionMode::Ask).planning);
    }

    #[test]
    fn a_resumed_conversation_is_named_on_the_command_line() {
        let launch = Launch {
            model: String::new(),
            full_access: false,
            planning: false,
            cwd: std::path::PathBuf::new(),
            resume: Some("24acd8ab-0b58-4d66-b8d1-16dbddf1ce21".into()),
        };
        assert!(
            launch.args().windows(2).any(|w| w[0] == "--conversation"
                && w[1] == "24acd8ab-0b58-4d66-b8d1-16dbddf1ce21")
        );
    }

    #[test]
    fn the_message_sent_is_the_one_shape_agy_accepts() {
        // Every other event name is answered with "ignoring unsupported stream input message
        // event", on stdout, in plain text — so a wrong envelope here is a turn that hangs.
        let msg = Message { role: Role::User, content: vec![ContentBlock::Text { text: "hi".into() }] };
        let v = user_frame(&[msg]);
        assert_eq!(v["event"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"][0]["type"], "text");
        assert_eq!(v["message"]["content"][0]["text"], "hi");
    }

    #[test]
    fn only_the_newest_question_is_sent_because_the_session_holds_the_rest() {
        let msgs = vec![
            Message { role: Role::User, content: vec![ContentBlock::Text { text: "first".into() }] },
            Message { role: Role::Assistant, content: vec![ContentBlock::Text { text: "ok".into() }] },
            Message { role: Role::User, content: vec![ContentBlock::Text { text: "second".into() }] },
        ];
        let v = user_frame(&msgs);
        let content = v["message"]["content"].as_array().expect("content is an array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "second");
    }

    #[test]
    fn a_status_that_is_not_success_is_not_reported_as_the_end_of_a_turn() {
        for (status, want) in [
            ("SUCCESS", StopReason::EndTurn),
            ("CANCELLED", StopReason::Cancelled),
            // A failure keeps the CLI's own words: a stop reason that says only "error" is a turn
            // that ended for no stated cause.
            ("ERROR", StopReason::Error { message: "stream input message is missing".into() }),
        ] {
            let mut s = Reading::default();
            let end = events(
                &frame(&format!(
                    r#"{{"event":"result","result":{{"status":"{status}","error":"stream input message is missing"}}}}"#
                )),
                &mut s,
            );
            let ProviderEvent::MessageDelta { stop_reason, .. } =
                end.iter().find(|e| matches!(e, ProviderEvent::MessageDelta { .. })).expect("a delta")
            else {
                unreachable!()
            };
            assert_eq!(*stop_reason, Some(want), "{status}");
        }
    }
}
