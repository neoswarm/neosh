//! Agents that speak the Agent Client Protocol.
//!
//! # Why one driver for several vendors
//!
//! ACP is JSON-RPC 2.0 over stdio, newline-delimited, and it is what Cursor's `cursor-agent`,
//! xAI's `grok` and Google's `gemini --experimental-acp` all speak. One driver, three plans, and
//! the fourth costs a line in the catalogue rather than a file here. That is the same bet ADR 0010
//! makes about MCP: a protocol worth adopting is one that turns "support vendor X" into
//! configuration.
//!
//! # What it actually is
//!
//! An **agent driver**, like [`super::claude_cli`] and [`super::codex_cli`]. The agent owns the
//! loop, the tools and the file edits; neosh's tool registry is not in play.
//!
//! # Approvals, and the honest limitation
//!
//! An ACP agent asks its *client* for permission — `session/request_permission` — which means
//! neosh has to answer. It currently answers from the permission mode and nothing else:
//!
//! - `full access`: take the agent's allow option.
//! - anything else: refuse, and say why.
//!
//! That is not the end state. The end state is the request arriving at the same approval prompt a
//! built-in tool call goes through, so `tool.pre` hooks see it and the answer is a person's rather
//! than a setting's. That needs the request to travel out of the driver and an answer to travel
//! back, which is a protocol change and a second approval path if done carelessly. Until then the
//! behaviour is conservative and stated out loud rather than quietly permissive.
//!
//! # Sessions
//!
//! One child process per turn. The agent's session id is kept and `session/load` is tried first so
//! the conversation continues; when the agent has no `loadSession` capability that fails, a new
//! session is made and the whole conversation is sent as the prompt. Both paths are correct; the
//! first is merely cheaper.

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use neosh_proto::{
    BlockStartKind, ContentBlock, DriverKind, InstanceConfig, Message, PermissionMode,
    ProviderEvent, Role, StopReason, ToolCallId, TurnRequest, Usage,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{Provider, ProviderStream};

/// The protocol version this client implements.
const PROTOCOL_VERSION: i64 = 1;

#[derive(Debug)]
pub struct AcpProvider {
    kind: DriverKind,
    program: String,
    args: Vec<String>,
    /// The agent's session id, so the next turn continues the conversation.
    session: Arc<Mutex<Option<String>>>,
    /// What neosh's permission mode is, mirrored here because the driver has to answer
    /// `session/request_permission` synchronously and cannot reach the permission layer.
    mode: Arc<Mutex<PermissionMode>>,
}

impl AcpProvider {
    pub fn new(kind: &str, program: &str, args: &[&str]) -> Self {
        Self {
            kind: DriverKind::from(kind),
            program: program.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            session: Arc::default(),
            mode: Arc::new(Mutex::new(PermissionMode::Ask)),
        }
    }

    /// Whether the CLI is on `PATH`. Used to decide if this instance is offerable at all.
    pub fn available(&self) -> bool {
        super::claude_cli::which(&self.program).is_some()
    }

    /// Tell the driver what neosh's permission mode is now.
    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock().expect("mode lock poisoned") = mode;
    }

    /// Forget the agent-side session, so the next turn starts a new one.
    pub fn reset_session(&self) {
        *self.session.lock().expect("session lock poisoned") = None;
    }
}

/// The whole conversation as ACP content blocks, for a session that has no history of its own.
fn prompt_blocks(messages: &[Message], only_latest: bool) -> Vec<Value> {
    let render = |m: &Message| -> Option<String> {
        let text: String = m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        (!text.trim().is_empty()).then_some(text)
    };

    if only_latest {
        return messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(render)
            .map(|text| vec![json!({ "type": "text", "text": text })])
            .unwrap_or_default();
    }

    // Roles are spelled out, because a flattened transcript with no speakers reads as one very
    // confused person talking to themselves.
    messages
        .iter()
        .filter_map(|m| {
            let who = match m.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            render(m).map(|text| json!({ "type": "text", "text": format!("{who}: {text}") }))
        })
        .collect()
}

/// Turn one `session/update` notification into provider events.
///
/// A free function over already-parsed JSON so the mapping is testable without any agent binary
/// present — which matters, because none of the three is a dependency anyone should need in order
/// to run the tests.
///
/// `blocks` maps a tool call id to the block index it was opened on, because ACP identifies tool
/// calls by an opaque id and `ProviderEvent` addresses blocks by position.
pub fn acp_events(
    params: &Value,
    next_index: &mut u32,
    blocks: &mut std::collections::HashMap<String, u32>,
) -> Vec<ProviderEvent> {
    let update = params.get("update").unwrap_or(&Value::Null);
    let kind = update.get("sessionUpdate").and_then(|k| k.as_str()).unwrap_or_default();

    let text_of = |field: &str| -> String {
        update
            .get(field)
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string()
    };

    match kind {
        // Answer and reasoning, both as deltas: ACP streams them, so this is the one driver where
        // the text really does arrive a piece at a time.
        "agent_message_chunk" => {
            let text = text_of("content");
            if text.is_empty() {
                return Vec::new();
            }
            let index = open(next_index, blocks, "agent_message", BlockStartKind::Text);
            vec![ProviderEvent::TextDelta { index, text }]
        }
        "agent_thought_chunk" => {
            let text = text_of("content");
            if text.is_empty() {
                return Vec::new();
            }
            let index = open(next_index, blocks, "agent_thought", BlockStartKind::Thinking);
            vec![ProviderEvent::ThinkingDelta { index, text }]
        }

        "tool_call" => {
            let Some(id) = update.get("toolCallId").and_then(|t| t.as_str()) else {
                return Vec::new();
            };
            if blocks.contains_key(id) {
                return Vec::new();
            }
            // The agent's own title is the best description there is: it wrote it for a human.
            let title = update
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or_else(|| update.get("kind").and_then(|k| k.as_str()).unwrap_or("tool"));
            let index = *next_index;
            *next_index += 1;
            blocks.insert(id.to_string(), index);
            let input = update
                .get("rawInput")
                .cloned()
                .unwrap_or_else(|| json!({ "query": title }));
            vec![
                ProviderEvent::BlockStart {
                    index,
                    block: BlockStartKind::ToolUse {
                        id: ToolCallId::from(id),
                        name: tool_name(update),
                    },
                },
                ProviderEvent::ToolInputDelta { index, partial_json: input.to_string() },
                ProviderEvent::BlockStop { index },
            ]
        }
        // Updates carry status and output. Nothing to add to a block that is already closed, and
        // re-opening one would draw the same call twice.
        "tool_call_update" => Vec::new(),

        // Plans, mode changes, command lists and usage are about the agent's own UI. Ignored rather
        // than guessed at: an unknown update rendered as an empty block reads as a bug in neosh.
        _ => Vec::new(),
    }
}

/// The block index for a streaming kind, opening one the first time.
fn open(
    next_index: &mut u32,
    blocks: &mut std::collections::HashMap<String, u32>,
    key: &str,
    _kind: BlockStartKind,
) -> u32 {
    if let Some(index) = blocks.get(key) {
        return *index;
    }
    let index = *next_index;
    *next_index += 1;
    blocks.insert(key.to_string(), index);
    index
}

/// A name for a tool call, from the agent's `kind` where it gave one.
fn tool_name(update: &Value) -> String {
    update
        .get("kind")
        .and_then(|k| k.as_str())
        .map(|k| match k {
            "read" => "read_file",
            "edit" => "edit_file",
            "delete" => "delete_file",
            "move" => "move_file",
            "search" => "search",
            "execute" => "shell",
            "fetch" => "fetch",
            other => other,
        })
        .unwrap_or("tool")
        .to_string()
}

/// Usage from a `PromptResponse`, in neosh's shape.
fn usage_of(v: &Value) -> Usage {
    let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    Usage {
        input_tokens: n("inputTokens"),
        output_tokens: n("outputTokens"),
        cache_read_tokens: n("cachedReadTokens"),
        cache_write_tokens: n("cachedWriteTokens"),
        thinking_tokens: n("thoughtTokens"),
    }
}

/// ACP's stop reasons, in neosh's shape.
pub fn stop_of(reason: &str) -> StopReason {
    match reason {
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal { category: None, explanation: None },
        "cancelled" => StopReason::Cancelled,
        _ => StopReason::EndTurn,
    }
}

/// What the agent said while we were waiting, and the parser state that made sense of it.
///
/// The notification handler runs inside a synchronous closure — it cannot send on a channel,
/// because sending is asynchronous — so it collects here and the caller drains it once the request
/// it was waiting for has come back.
#[derive(Default)]
struct Notes {
    events: Vec<ProviderEvent>,
    /// Block index per streaming kind and per tool call id.
    blocks: std::collections::HashMap<String, u32>,
    /// Blocks that have had their `BlockStart` emitted, so each gets exactly one — and so the
    /// matching `BlockStop`s can be emitted at the end.
    opened: Vec<u32>,
    next_index: u32,
    refusals: u32,
}

impl Notes {
    fn push(&mut self, params: &Value) {
        for ev in acp_events(params, &mut self.next_index, &mut self.blocks) {
            // A streaming block's `BlockStart` is emitted the first time its index is used, which
            // is the only moment we know it needs one.
            let opening_kind = match &ev {
                ProviderEvent::TextDelta { index, .. } => Some((*index, BlockStartKind::Text)),
                ProviderEvent::ThinkingDelta { index, .. } => {
                    Some((*index, BlockStartKind::Thinking))
                }
                _ => None,
            };
            if let Some((i, kind)) = opening_kind
                && !self.opened.contains(&i)
            {
                self.opened.push(i);
                self.events.push(ProviderEvent::BlockStart { index: i, block: kind });
            }
            self.events.push(ev);
        }
    }
}

/// One JSON-RPC conversation over a child's stdio.
struct Rpc {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next: i64,
}

impl Rpc {
    async fn send(&mut self, value: &Value) -> Result<(), String> {
        let mut line = value.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
        self.stdin.flush().await.map_err(|e| e.to_string())
    }

    /// Make a request and pump everything that arrives before its answer.
    ///
    /// `on_note` sees every notification, and every request *from* the agent, because an agent that
    /// asks a question and never hears back simply stops — a hang with no error, which is the worst
    /// failure mode a subprocess protocol has.
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        on_note: &mut impl FnMut(&str, &Value) -> Option<Value>,
    ) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;

        loop {
            let Some(line) = self.lines.next_line().await.map_err(|e| e.to_string())? else {
                return Err(format!("{method}: the agent closed its output"));
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue; // non-JSON chatter on stdout is not fatal
            };

            // Our answer.
            if msg.get("id").and_then(|i| i.as_i64()) == Some(id) && msg.get("method").is_none() {
                if let Some(err) = msg.get("error") {
                    let text = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("the agent refused the request");
                    return Err(format!("{method}: {text}"));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }

            let Some(m) = msg.get("method").and_then(|m| m.as_str()) else { continue };
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let reply = on_note(m, &params);
            // A request from the agent has an id and is waiting for an answer; a notification does
            // not and is not.
            if let (Some(their_id), Some(result)) = (msg.get("id").cloned(), reply) {
                self.send(&json!({ "jsonrpc": "2.0", "id": their_id, "result": result })).await?;
            }
        }
    }
}

/// How to answer `session/request_permission`, given the mode.
///
/// Returns the option id to take, or `None` to refuse. Refusing is the default because the driver
/// has no way to ask a person: see this module's header for why that is a stated limitation rather
/// than a design.
pub fn permission_answer(params: &Value, mode: PermissionMode) -> Option<String> {
    if mode != PermissionMode::Allow {
        return None;
    }
    let options = params.get("options").and_then(|o| o.as_array())?;
    // Once, not always: "allow this" is the narrowest thing that lets the turn continue, and a
    // wrapper choosing "always" on somebody's behalf is choosing something they cannot see.
    let pick = |want: &str| {
        options
            .iter()
            .find(|o| o.get("kind").and_then(|k| k.as_str()) == Some(want))
            .and_then(|o| o.get("optionId").and_then(|i| i.as_str()))
            .map(str::to_string)
    };
    pick("allow_once").or_else(|| pick("allow_always"))
}

#[async_trait::async_trait]
impl Provider for AcpProvider {
    fn driver(&self) -> DriverKind {
        self.kind.clone()
    }

    fn delegates_agent_loop(&self) -> bool {
        true
    }

    fn stream(
        &self,
        _instance: &InstanceConfig,
        request: TurnRequest,
        cancel: CancellationToken,
    ) -> ProviderStream {
        let (tx, rx) = mpsc::channel::<ProviderEvent>(256);
        let program = self.program.clone();
        let args = self.args.clone();
        let session_slot = self.session.clone();
        let mode = *self.mode.lock().expect("mode lock poisoned");
        let model = request.selection.model.as_ref().to_string();
        // The conversation's directory. ACP makes this explicit — `cwd` is a parameter of
        // `session/new` — so unlike the other two drivers this one was always going to send
        // *something*; it just used to send whatever directory neosh was launched from.
        let workdir = if request.cwd.as_os_str().is_empty() {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            request.cwd.clone()
        };
        let cwd = workdir.display().to_string();

        tokio::spawn(async move {
            let fail = |tx: mpsc::Sender<ProviderEvent>, message: String| async move {
                let _ = tx.send(ProviderEvent::Error { message, retryable: false }).await;
            };

            let mut cmd = Command::new(&program);
            // Spawned there as well as told about it: an agent reads its own configuration
            // relative to where it starts, and `session/new` is too late for that.
            cmd.current_dir(&workdir)
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => return fail(tx, format!("could not run {program:?}: {e}")).await,
            };

            let stdin = child.stdin.take().expect("stdin was piped");
            let stdout = child.stdout.take().expect("stdout was piped");
            let stderr = child.stderr.take().expect("stderr was piped");
            let stderr_buf = Arc::new(Mutex::new(String::new()));
            {
                let buf = stderr_buf.clone();
                tokio::spawn(async move {
                    let mut l = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = l.next_line().await {
                        let mut b = buf.lock().expect("stderr lock poisoned");
                        b.push_str(&line);
                        b.push('\n');
                    }
                });
            }

            let mut rpc = Rpc { stdin, lines: BufReader::new(stdout).lines(), next: 1 };

            // Everything the agent says while we are waiting for an answer, and the parser state
            // that goes with it. One lock over the lot, because the notification handler is a
            // plain closure — it cannot await, so it can only *collect*, and the collection has to
            // survive across four separate requests.
            let notes = Arc::new(Mutex::new(Notes::default()));
            let mut on_note = {
                let notes = notes.clone();
                move |method: &str, params: &Value| -> Option<Value> {
                    let mut n = notes.lock().expect("notes lock poisoned");
                    match method {
                        "session/update" => {
                            n.push(params);
                            None
                        }
                        "session/request_permission" => match permission_answer(params, mode) {
                            Some(id) => Some(json!({
                                "outcome": { "outcome": "selected", "optionId": id }
                            })),
                            None => {
                                n.refusals += 1;
                                Some(json!({ "outcome": { "outcome": "cancelled" } }))
                            }
                        },
                        // Everything else the agent might ask a client for — reading files, running
                        // terminals — was declined in `clientCapabilities`, so an answer of
                        // "nothing" is consistent rather than surprising.
                        _ => Some(Value::Null),
                    }
                }
            };

            // 1. initialize
            let init = json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientInfo": { "name": "neosh", "version": env!("CARGO_PKG_VERSION") },
                // Declined deliberately: neosh has its own file and terminal tools behind its own
                // permission layer, and lending them to an agent that runs its own loop would put
                // effects outside that layer.
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false
                }
            });
            if let Err(e) = rpc.request("initialize", init, &mut on_note).await {
                let _ = child.start_kill();
                return fail(tx, e).await;
            }

            // 2. a session: continue the old one if the agent can, else start a fresh one and send
            //    the whole conversation.
            let previous = session_slot.lock().expect("session lock poisoned").clone();
            let mut resumed = false;
            let mut session_id = None;
            if let Some(id) = previous {
                let loaded = rpc
                    .request(
                        "session/load",
                        json!({ "sessionId": id, "cwd": cwd, "mcpServers": [] }),
                        &mut on_note,
                    )
                    .await;
                if loaded.is_ok() {
                    resumed = true;
                    session_id = Some(id);
                }
            }
            if session_id.is_none() {
                match rpc
                    .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }), &mut on_note)
                    .await
                {
                    Ok(v) => {
                        session_id = v.get("sessionId").and_then(|s| s.as_str()).map(str::to_string)
                    }
                    Err(e) => {
                        let _ = child.start_kill();
                        return fail(tx, e).await;
                    }
                }
            }
            let Some(session_id) = session_id else {
                let _ = child.start_kill();
                return fail(tx, "the agent did not give this session an id".into()).await;
            };
            *session_slot.lock().expect("session lock poisoned") = Some(session_id.clone());

            // 3. the model, where the agent lets one be chosen. A failure here is not fatal: an
            //    agent with one model refuses the call and is still perfectly usable.
            if !model.is_empty() {
                let _ = rpc
                    .request(
                        "session/set_model",
                        json!({ "sessionId": session_id, "modelId": model }),
                        &mut on_note,
                    )
                    .await;
            }

            let _ = tx.send(ProviderEvent::MessageStart { model: Some(model), usage: Usage::default() }).await;

            // 4. the prompt, which does not return until the turn is over.
            let prompt = prompt_blocks(&request.messages, resumed);
            let answered = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    let _ = child.start_kill();
                    return;
                }
                r = rpc.request(
                    "session/prompt",
                    json!({ "sessionId": session_id, "prompt": prompt }),
                    &mut on_note,
                ) => r,
            };

            let (drained, opened, refusals) = {
                let mut n = notes.lock().expect("notes lock poisoned");
                (std::mem::take(&mut n.events), n.opened.clone(), n.refusals)
            };
            for ev in drained {
                if tx.send(ev).await.is_err() {
                    let _ = child.start_kill();
                    return;
                }
            }

            match answered {
                Ok(v) => {
                    for i in opened {
                        let _ = tx.send(ProviderEvent::BlockStop { index: i }).await;
                    }
                    if refusals > 0 {
                        let _ = tx
                            .send(ProviderEvent::Error {
                                message: format!(
                                    "{program} asked for permission {refusals} time(s) and neosh \
                                     refused: this driver can only approve on your behalf under \
                                     `full access` (^Y)"
                                ),
                                retryable: false,
                            })
                            .await;
                    }
                    let stop =
                        v.get("stopReason").and_then(|s| s.as_str()).map(stop_of).unwrap_or(StopReason::EndTurn);
                    let usage = v.get("usage").map(usage_of).unwrap_or_default();
                    let _ = tx.send(ProviderEvent::MessageDelta { stop_reason: Some(stop), usage }).await;
                    let _ = tx.send(ProviderEvent::MessageStop).await;
                }
                Err(e) => {
                    let logged = stderr_buf.lock().expect("stderr lock poisoned").trim().to_string();
                    let message = if logged.is_empty() { e } else { format!("{e}\n{logged}") };
                    let _ = tx.send(ProviderEvent::Error { message, retryable: false }).await;
                }
            }
            let _ = child.start_kill();
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(json_text: &str) -> Value {
        serde_json::from_str(json_text).expect("fixture parses")
    }

    #[test]
    fn an_answer_streams_as_deltas_on_one_block() {
        // ACP is the one CLI protocol here that really does deliver text a piece at a time, so the
        // pieces have to land on the same block rather than opening a new one each.
        let mut i = 0;
        let mut blocks = Default::default();
        let a = acp_events(
            &note(r#"{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"one "}}}"#),
            &mut i,
            &mut blocks,
        );
        let b = acp_events(
            &note(r#"{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"two"}}}"#),
            &mut i,
            &mut blocks,
        );
        assert!(matches!(&a[0], ProviderEvent::TextDelta { index: 0, text } if text == "one "));
        assert!(matches!(&b[0], ProviderEvent::TextDelta { index: 0, text } if text == "two"));
        assert_eq!(i, 1, "one block, not two");
    }

    #[test]
    fn reasoning_gets_a_block_of_its_own() {
        let mut i = 0;
        let mut blocks = Default::default();
        acp_events(
            &note(r#"{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}"#),
            &mut i,
            &mut blocks,
        );
        let t = acp_events(
            &note(r#"{"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hm"}}}"#),
            &mut i,
            &mut blocks,
        );
        assert!(matches!(t[0], ProviderEvent::ThinkingDelta { index: 1, .. }));
    }

    #[test]
    fn a_tool_call_is_drawn_once_however_often_it_is_updated() {
        // ACP reports a call and then updates it — with output, with a status. Re-opening a block
        // on every update would draw the same call four times.
        let mut i = 0;
        let mut blocks = Default::default();
        let started = acp_events(
            &note(
                r#"{"update":{"sessionUpdate":"tool_call","toolCallId":"t1","kind":"read",
                    "title":"Read src/main.rs","rawInput":{"path":"src/main.rs"},"status":"pending"}}"#,
            ),
            &mut i,
            &mut blocks,
        );
        match &started[0] {
            ProviderEvent::BlockStart { block: BlockStartKind::ToolUse { id, name }, .. } => {
                assert_eq!(id.0, "t1");
                assert_eq!(name, "read_file", "the agent's kind, in neosh's vocabulary");
            }
            other => panic!("expected a tool block, got {other:?}"),
        }
        assert!(
            matches!(&started[1], ProviderEvent::ToolInputDelta { partial_json, .. }
                if partial_json.contains("src/main.rs")),
            "and its subject"
        );

        let again = acp_events(
            &note(r#"{"update":{"sessionUpdate":"tool_call","toolCallId":"t1","kind":"read","title":"Read src/main.rs"}}"#),
            &mut i,
            &mut blocks,
        );
        assert!(again.is_empty(), "the same call twice is one card");
        let updated = acp_events(
            &note(r#"{"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed"}}"#),
            &mut i,
            &mut blocks,
        );
        assert!(updated.is_empty());
    }

    #[test]
    fn an_update_we_have_never_heard_of_is_ignored() {
        let mut i = 0;
        let mut blocks = Default::default();
        assert!(
            acp_events(&note(r#"{"update":{"sessionUpdate":"telepathy"}}"#), &mut i, &mut blocks)
                .is_empty()
        );
        assert_eq!(i, 0, "and does not consume a block index");
    }

    #[test]
    fn permission_is_refused_unless_you_have_said_not_to_ask() {
        let ask = note(
            r#"{"options":[{"optionId":"y","kind":"allow_once"},{"optionId":"n","kind":"reject_once"}]}"#,
        );
        assert_eq!(permission_answer(&ask, PermissionMode::Allow).as_deref(), Some("y"));
        for mode in [PermissionMode::Ask, PermissionMode::AllowListed, PermissionMode::Deny] {
            assert_eq!(
                permission_answer(&ask, mode),
                None,
                "the driver cannot ask a person, so it does not answer for one"
            );
        }
    }

    #[test]
    fn allow_once_is_preferred_over_allow_always() {
        // "Always" is a decision with a lifetime, and a wrapper taking it on somebody's behalf is
        // taking one they cannot see.
        let ask = note(
            r#"{"options":[{"optionId":"a","kind":"allow_always"},{"optionId":"o","kind":"allow_once"}]}"#,
        );
        assert_eq!(permission_answer(&ask, PermissionMode::Allow).as_deref(), Some("o"));
    }

    #[test]
    fn a_resumed_session_sends_only_the_new_question() {
        let msgs = vec![
            Message { role: Role::User, content: vec![ContentBlock::Text { text: "old".into() }] },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "reply".into() }],
            },
            Message { role: Role::User, content: vec![ContentBlock::Text { text: "new".into() }] },
        ];
        let latest = prompt_blocks(&msgs, true);
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0]["text"], "new");

        // And a fresh one sends the lot, with who said what — a flattened transcript with no
        // speakers reads as one very confused person talking to themselves.
        let all = prompt_blocks(&msgs, false);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0]["text"], "User: old");
        assert_eq!(all[1]["text"], "Assistant: reply");
    }

    #[test]
    fn stop_reasons_map_onto_ours() {
        assert_eq!(stop_of("end_turn"), StopReason::EndTurn);
        assert_eq!(stop_of("max_tokens"), StopReason::MaxTokens);
        assert!(matches!(stop_of("refusal"), StopReason::Refusal { .. }));
        assert_eq!(stop_of("cancelled"), StopReason::Cancelled);
        assert_eq!(stop_of("something-new"), StopReason::EndTurn, "and an unknown one ends the turn");
    }

    #[tokio::test]
    async fn a_missing_binary_reports_an_error_event() {
        use futures::StreamExt;
        use neosh_proto::{AuthRef, InstanceId, ModelSelection};
        let p = AcpProvider::new("ghost", "definitely-not-a-real-binary-xyzzy", &[]);
        assert!(!p.available());
        let inst = InstanceConfig {
            id: InstanceId::from("ghost"),
            driver: DriverKind::from("ghost"),
            display_name: "Ghost".into(),
            base_url: None,
            brand: None,
            auth: AuthRef::Inherited,
            models: vec![],
            extra_headers: vec![],
        };
        let req = TurnRequest {
            cwd: std::path::PathBuf::new(),
            selection: ModelSelection {
                instance: InstanceId::from("ghost"),
                model: "m".into(),
                options: vec![],
            },
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hello".into() }],
            }],
            tools: vec![],
            max_output_tokens: None,
        };
        let got: Vec<_> = p.stream(&inst, req, CancellationToken::new()).collect().await;
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], ProviderEvent::Error { retryable: false, .. }));
    }
}
