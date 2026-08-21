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
//! # Approvals
//!
//! An ACP agent asks its *client* for permission — `session/request_permission` — which means neosh
//! has to answer. It used to answer from the permission mode and nothing else: take the allow
//! option under `full access`, refuse otherwise. Which made `ask`, the default, mean "refuse
//! everything without asking anybody".
//!
//! Now the request goes to a [`crate::approval::PermissionAsker`], which is the same prompt a
//! built-in tool call reaches, and the agent's own options travel with it — an agent that offers
//! "allow, and don't ask again for `cargo`" is offering something no client-side yes/no could
//! reproduce, so it is offered verbatim. See ADR 0032.
//!
//! With no asker installed the old behaviour is exactly what remains, which is what a test or a
//! headless run gets: policy answers, and where policy wanted a person, the answer is no.
//!
//! # Sessions
//!
//! One child process per turn. The agent's session id is kept and `session/load` is tried first so
//! the conversation continues; when the agent has no `loadSession` capability that fails, a new
//! session is made and the whole conversation is sent as the prompt. Both paths are correct; the
//! first is merely cheaper.
//!
//! Unlike [`super::claude_cli`] and [`super::codex_cli`], which now hold their process open, this
//! one still spawns per turn — and can afford to, because ACP's session lives on the *agent's*
//! side and `session/load` puts it back. What the process being short-lived does cost is a place
//! to send `session/set_mode` between turns, which is why the mode is still passed at session
//! setup rather than when it changes.

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use neosh_proto::{
    Activity, BlockStartKind, Capability, ContentBlock, DriverCommand, DriverKind, InstanceConfig,
    Message, PermissionMode, PermissionOption, PermissionOptionKind, PlanState, PlanStep,
    ProviderEvent, Role, StopReason, ToolCallId, TurnRequest, Usage,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::approval::{PermissionAnswer, PermissionAsker, PermissionRequest};
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
    /// What neosh's permission mode is, mirrored here for the case where nobody can be asked.
    mode: Arc<Mutex<PermissionMode>>,
    /// Who to ask when the agent wants permission. `None` in a test or a headless run, and the
    /// driver then falls back to answering from the mode alone.
    asker: Arc<Mutex<Option<Arc<dyn PermissionAsker>>>>,
}

impl AcpProvider {
    pub fn new(kind: &str, program: &str, args: &[&str]) -> Self {
        Self {
            kind: DriverKind::from(kind),
            program: program.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            session: Arc::default(),
            mode: Arc::new(Mutex::new(PermissionMode::Ask)),
            asker: Arc::default(),
        }
    }

    /// Whether the CLI is on `PATH`. Used to decide if this instance is offerable at all.
    pub fn available(&self) -> bool {
        super::claude_cli::which(&self.program).is_some()
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
            let mut input = update
                .get("rawInput")
                .cloned()
                .unwrap_or_else(|| json!({ "query": title }));
            fold_diff(&mut input, update);
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
        // The other half of a tool call. The block is closed by now and re-opening it would draw
        // the call twice — but a result is not a block, and without one every ACP tool card sat
        // there with a pulsing dot over a call that had come back minutes ago.
        "tool_call_update" => {
            let Some(id) = update.get("toolCallId").and_then(|t| t.as_str()) else {
                return Vec::new();
            };
            let status = update.get("status").and_then(|s| s.as_str()).unwrap_or_default();
            // `in_progress` and `pending` are not answers. Only the two endings are.
            if !matches!(status, "completed" | "failed") {
                return Vec::new();
            }
            vec![ProviderEvent::ToolResult {
                id: ToolCallId::from(id),
                content: content_text(update),
                is_error: status == "failed",
            }]
        }

        // The agent's own checklist. ACP calls the steps `entries` and neosh calls them steps;
        // everything else about them is the same idea, whole list every time.
        "plan" => {
            let steps = update
                .get("entries")
                .and_then(|e| e.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            let text = e.get("content").and_then(|c| c.as_str())?.trim();
                            (!text.is_empty()).then(|| PlanStep {
                                text: text.to_string(),
                                state: match e.get("status").and_then(|s| s.as_str()) {
                                    Some("completed") => PlanState::Done,
                                    Some("in_progress") => PlanState::Active,
                                    _ => PlanState::Pending,
                                },
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            vec![ProviderEvent::Activity { activity: Activity::Plan { steps } }]
        }

        // What this agent accepts as a slash command, which is a fact about the agent and its
        // install rather than about ACP — a vendor ships some, a project adds more.
        "available_commands_update" => {
            let commands = update
                .get("availableCommands")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|c| {
                            Some(DriverCommand {
                                name: c.get("name").and_then(|n| n.as_str())?.to_string(),
                                description: c
                                    .get("description")
                                    .and_then(|d| d.as_str())
                                    .map(str::to_string),
                                argument_hint: c
                                    .get("input")
                                    .and_then(|i| i.get("hint"))
                                    .and_then(|h| h.as_str())
                                    .map(str::to_string),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            vec![ProviderEvent::Activity { activity: Activity::Commands { commands } }]
        }

        // Mode changes and usage are about the agent's own UI. Ignored rather than guessed at: an
        // unknown update rendered as an empty block reads as a bug in neosh.
        _ => Vec::new(),
    }
}

/// The text of an update's `content`, in either of the two shapes agents send it in.
///
/// ACP wraps each item as `{ type: "content", content: { type: "text", text } }`, and several
/// agents send the inner object directly. Both are read, because a result that arrived and was not
/// understood looks exactly like one that never arrived.
fn content_text(update: &Value) -> String {
    let Some(items) = update.get("content").and_then(|c| c.as_array()) else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let inner = item.get("content").unwrap_or(item);
            match inner.get("type").and_then(|t| t.as_str()) {
                Some("text") | None => {
                    inner.get("text").and_then(|t| t.as_str()).map(str::to_string)
                }
                Some("diff") => Some(format!(
                    "diff {}",
                    inner.get("path").and_then(|p| p.as_str()).unwrap_or_default()
                )),
                Some(other) => Some(format!("[{other}]")),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Put the diff an ACP agent reports for an edit where the transcript looks for one.
///
/// ACP keeps the change out of `rawInput` — that is whatever the agent's own tool took, in its own
/// vocabulary — and reports it separately as `content: [{ type: "diff", path, oldText, newText }]`.
/// neosh reads edits off a call's input, in the handful of shapes editing tools use, so the diff
/// goes in there under `old_text`/`new_text`/`path`, and only where the input has nothing of its
/// own to say. An agent that sends the diff later, in a `tool_call_update`, is still out of reach:
/// the block is closed by then. See ADR 0033.
fn fold_diff(input: &mut Value, update: &Value) {
    let Some(items) = update.get("content").and_then(|c| c.as_array()) else { return };
    let Some(diff) = items.iter().find(|c| c.get("type").and_then(|t| t.as_str()) == Some("diff"))
    else {
        return;
    };
    if !input.is_object() {
        *input = json!({});
    }
    let Some(map) = input.as_object_mut() else { return };
    for (from, to) in [("oldText", "old_text"), ("newText", "new_text"), ("path", "path")] {
        if let Some(v) = diff.get(from).and_then(|v| v.as_str())
            && !map.contains_key(to)
        {
            map.insert(to.to_string(), Value::String(v.to_string()));
        }
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

/// What to do about something the agent sent while we were waiting for an answer.
///
/// The middle variant is the whole reason this is an enum rather than an `Option<Value>`. A
/// notification handler is a plain closure — it cannot await, so it cannot ask anybody anything —
/// and the one message that has to be answered by a person is the one it therefore could never
/// answer. So it hands the question back to the pump, which can.
enum Note {
    /// A notification. Nothing to send.
    Ignore,
    /// A request, with the answer already known.
    Reply(Value),
    /// A request that needs somebody asked first.
    Ask(PermissionRequest),
}

/// One JSON-RPC conversation over a child's stdio.
struct Rpc {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next: i64,
    /// Who answers a permission request, and what to fall back to when that is nobody.
    asker: Option<Arc<dyn PermissionAsker>>,
    mode: PermissionMode,
    /// Counted so a turn that got nowhere can say why rather than ending in silence.
    refusals: usize,
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
        on_note: &mut impl FnMut(&str, &Value) -> Note,
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
            let reply = match on_note(m, &params) {
                Note::Ignore => None,
                Note::Reply(v) => Some(v),
                // The agent is blocked on this and so is the turn, which is correct: it is waiting
                // on a person, and the spinner in the transcript says so.
                Note::Ask(req) => Some(self.answer_permission(req, &params).await),
            };
            // A request from the agent has an id and is waiting for an answer; a notification does
            // not and is not.
            if let (Some(their_id), Some(result)) = (msg.get("id").cloned(), reply) {
                self.send(&json!({ "jsonrpc": "2.0", "id": their_id, "result": result })).await?;
            }
        }
    }

    /// Resolve one `session/request_permission` into the outcome object the agent is waiting for.
    async fn answer_permission(&mut self, req: PermissionRequest, params: &Value) -> Value {
        let (outcome, refused) =
            permission_outcome(self.asker.as_ref(), self.mode, req, params).await;
        if refused {
            self.refusals += 1;
        }
        outcome
    }
}

/// The answer to one permission request, and whether it was a refusal.
///
/// Free rather than a method so it can be tested without a child process: everything it needs is
/// an asker, a mode and the request, and none of those involve a pipe.
async fn permission_outcome(
    asker: Option<&Arc<dyn PermissionAsker>>,
    mode: PermissionMode,
    req: PermissionRequest,
    params: &Value,
) -> (Value, bool) {
    let answer = match asker {
        Some(a) => a.ask(req).await,
        // Nobody to ask. The mode is all there is, and where the mode wanted a person the answer
        // is no — the same direction a blocking hook times out in, per ADR 0009.
        None => match permission_answer(params, mode) {
            Some(id) => PermissionAnswer::Option(id),
            None => PermissionAnswer::Deny,
        },
    };
    let chosen = match answer {
        PermissionAnswer::Option(id) => Some(id),
        // Approved without naming one of the agent's options, so take the narrowest yes it
        // offered. "Once" rather than "always": a wrapper choosing "always" on somebody's behalf
        // is choosing something they cannot see.
        PermissionAnswer::Allow => narrowest_allow(params),
        PermissionAnswer::Deny => None,
    };
    match chosen {
        Some(id) => (json!({ "outcome": { "outcome": "selected", "optionId": id } }), false),
        None => (json!({ "outcome": { "outcome": "cancelled" } }), true),
    }
}

/// How to answer `session/request_permission` with no one to ask.
///
/// Returns the option id to take, or `None` to refuse. Refusing unless the mode is `full access` is
/// the fallback rather than the design — see this module's header — and it is what a test or a
/// headless run gets.
pub fn permission_answer(params: &Value, mode: PermissionMode) -> Option<String> {
    if mode != PermissionMode::Allow {
        return None;
    }
    narrowest_allow(params)
}

/// The least permissive "yes" among the options the agent offered.
fn narrowest_allow(params: &Value) -> Option<String> {
    let options = params.get("options").and_then(|o| o.as_array())?;
    let pick = |want: &str| {
        options
            .iter()
            .find(|o| o.get("kind").and_then(|k| k.as_str()) == Some(want))
            .and_then(|o| o.get("optionId").and_then(|i| i.as_str()))
            .map(str::to_string)
    };
    pick("allow_once").or_else(|| pick("allow_always"))
}

/// Read a `session/request_permission` into something a person and a policy can both answer.
///
/// The agent's own wording is kept for the person and a [`Capability`] is derived for the policy,
/// which is what lets "read a file in this workspace" be answered without anybody being disturbed —
/// the same answer the built-in `read_file` gets, for the same reason.
///
/// A request whose shape is unrecognised becomes [`Capability::Exec`] with the title as the
/// command. Deliberately the strictest of the four: an unknown effect gated as if it were arbitrary
/// execution is a prompt too many, and the alternative is a prompt too few.
pub fn permission_request(params: &Value, cwd: &std::path::Path) -> PermissionRequest {
    let call = params.get("toolCall").unwrap_or(&Value::Null);
    let title = call
        .get("title")
        .and_then(|t| t.as_str())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or("the agent wants permission")
        .to_string();
    let raw = call.get("rawInput").unwrap_or(&Value::Null);
    let str_at = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    // `locations` is where ACP says a tool call will have its effect, which is a better answer than
    // guessing which key of `rawInput` a given agent calls its path.
    let path = call
        .get("locations")
        .and_then(|l| l.as_array())
        .and_then(|l| l.first())
        .and_then(|l| l.get("path"))
        .and_then(|p| p.as_str())
        .map(str::to_string)
        .or_else(|| str_at(raw, "path"))
        .or_else(|| str_at(raw, "file_path"))
        .or_else(|| str_at(raw, "abs_path"));
    let capability = match call.get("kind").and_then(|k| k.as_str()).unwrap_or("other") {
        "read" | "search" => Capability::ReadFile { path: path.unwrap_or_else(|| ".".into()) },
        "edit" | "delete" | "move" => {
            Capability::WriteFile { path: path.unwrap_or_else(|| ".".into()) }
        }
        "fetch" => Capability::Network {
            host: str_at(raw, "url")
                .as_deref()
                .and_then(host_of)
                .or_else(|| str_at(raw, "host"))
                .unwrap_or_else(|| title.clone()),
        },
        // `execute`, `think`, `other`, and anything a later revision of ACP adds.
        _ => Capability::Exec {
            command: str_at(raw, "command").unwrap_or_else(|| title.clone()),
        },
    };
    PermissionRequest {
        title,
        capability,
        options: params
            .get("options")
            .and_then(|o| o.as_array())
            .map(|o| o.iter().filter_map(permission_option).collect())
            .unwrap_or_default(),
        cwd: cwd.to_path_buf(),
    }
}

/// One entry of the `options` array, or `None` if it is missing the parts that make it answerable.
fn permission_option(v: &Value) -> Option<PermissionOption> {
    let id = v.get("optionId").and_then(|i| i.as_str())?.to_string();
    let kind = match v.get("kind").and_then(|k| k.as_str())? {
        "allow_once" => PermissionOptionKind::AllowOnce,
        "allow_always" => PermissionOptionKind::AllowAlways,
        "reject_once" => PermissionOptionKind::RejectOnce,
        "reject_always" => PermissionOptionKind::RejectAlways,
        _ => return None,
    };
    Some(PermissionOption {
        // The agent's wording, falling back to the kind — never the id, which is opaque and
        // sometimes a uuid.
        label: v
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|n| !n.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                match kind {
                    PermissionOptionKind::AllowOnce => "Allow once",
                    PermissionOptionKind::AllowAlways => "Allow always",
                    PermissionOptionKind::RejectOnce => "Deny",
                    PermissionOptionKind::RejectAlways => "Deny always",
                }
                .to_string()
            }),
        id,
        kind,
    })
}

/// The host part of a URL, without pulling in a URL parser for one field.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = host.split_once(':').map(|(h, _)| h).unwrap_or(host);
    (!host.is_empty()).then(|| host.to_string())
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
        let asker = self.asker.lock().expect("asker lock poisoned").clone();
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

            let mut rpc = Rpc {
                stdin,
                lines: BufReader::new(stdout).lines(),
                next: 1,
                asker,
                mode,
                refusals: 0,
            };

            // Everything the agent says while we are waiting for an answer, and the parser state
            // that goes with it. One lock over the lot, because the notification handler is a
            // plain closure — it cannot await, so it can only *collect*, and the collection has to
            // survive across four separate requests.
            let notes = Arc::new(Mutex::new(Notes::default()));
            let mut on_note = {
                let notes = notes.clone();
                let workdir = workdir.clone();
                move |method: &str, params: &Value| -> Note {
                    match method {
                        "session/update" => {
                            notes.lock().expect("notes lock poisoned").push(params);
                            Note::Ignore
                        }
                        // Handed to the pump rather than answered here: answering means asking a
                        // person, asking a person means awaiting, and a closure cannot await.
                        "session/request_permission" => {
                            Note::Ask(permission_request(params, &workdir))
                        }
                        // Everything else the agent might ask a client for — reading files, running
                        // terminals — was declined in `clientCapabilities`, so an answer of
                        // "nothing" is consistent rather than surprising.
                        _ => Note::Reply(Value::Null),
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
            let mut answered = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                r = rpc.request(
                    "session/prompt",
                    json!({ "sessionId": session_id, "prompt": prompt }),
                    &mut on_note,
                ) => Some(r),
            };
            if answered.is_none() {
                // Asked to stop, not killed. ACP has a notification for exactly this, and the
                // agent answers the outstanding `session/prompt` with `cancelled` — which is what
                // lets it finish the file it was halfway through writing and record the turn in
                // its own session, so `session/load` next time knows this happened.
                //
                // Killing is the fallback, not the plan. An agent that ignores the notification
                // costs two seconds and then gets what it would have got anyway.
                let _ = rpc
                    .send(&json!({
                        "jsonrpc": "2.0",
                        "method": "session/cancel",
                        "params": { "sessionId": session_id },
                    }))
                    .await;
                answered = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    rpc.request(
                        "session/prompt",
                        json!({ "sessionId": session_id, "prompt": Vec::<Value>::new() }),
                        &mut on_note,
                    ),
                )
                .await
                .ok();
            }
            let Some(answered) = answered else {
                let _ = child.start_kill();
                return;
            };

            let (drained, opened) = {
                let mut n = notes.lock().expect("notes lock poisoned");
                (std::mem::take(&mut n.events), n.opened.clone())
            };
            let refusals = rpc.refusals;
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
                    // Said once, at the end, rather than per refusal: the transcript already
                    // shows each refused call, and what is worth saying is that the turn stopped
                    // short because of them.
                    if refusals > 0 {
                        let _ = tx
                            .send(ProviderEvent::Error {
                                message: format!(
                                    "{program} asked for permission {refusals} time(s) and was \
                                     refused"
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

impl super::AgentDriver for AcpProvider {
    fn set_permission_mode(&self, mode: PermissionMode) {
        *self.mode.lock().expect("mode lock poisoned") = mode;
    }

    fn set_permission_asker(&self, asker: Arc<dyn PermissionAsker>) {
        *self.asker.lock().expect("asker lock poisoned") = Some(asker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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

        // The update is not a second card — but it is the *answer*, and dropping it left every ACP
        // card pulsing over a call that had come back.
        let updated = acp_events(
            &note(
                r#"{"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"fn main() {}"}}]}}"#,
            ),
            &mut i,
            &mut blocks,
        );
        assert_eq!(updated, vec![ProviderEvent::ToolResult {
            id: ToolCallId::from("t1"),
            content: "fn main() {}".into(),
            is_error: false,
        }]);
        assert!(
            !updated.iter().any(|e| matches!(e, ProviderEvent::BlockStart { .. })),
            "and still one card"
        );
    }

    #[test]
    fn a_call_that_is_only_getting_on_with_it_has_not_answered() {
        // `in_progress` arrives repeatedly while a long call runs. Read as an ending, each one
        // would settle the card and the next would find it already answered.
        let mut i = 0;
        let mut blocks = Default::default();
        for status in ["pending", "in_progress"] {
            let events = acp_events(
                &note(&format!(
                    r#"{{"update":{{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"{status}"}}}}"#
                )),
                &mut i,
                &mut blocks,
            );
            assert!(events.is_empty(), "{status} is not an answer");
        }
        let failed = acp_events(
            &note(
                r#"{"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"failed","content":[{"type":"text","text":"no such file"}]}}"#,
            ),
            &mut i,
            &mut blocks,
        );
        assert_eq!(failed, vec![ProviderEvent::ToolResult {
            id: ToolCallId::from("t1"),
            content: "no such file".into(),
            is_error: true,
        }]);
    }

    #[test]
    fn a_plan_and_a_command_list_are_no_longer_dropped() {
        // Both were read past, because there was nowhere to put something that is not a content
        // block. See ADR 0034.
        let mut i = 0;
        let mut blocks = Default::default();
        let plan = acp_events(
            &note(
                r#"{"update":{"sessionUpdate":"plan","entries":[{"content":"read the code","status":"completed"},{"content":"change it","status":"in_progress"},{"content":"test it","status":"pending"}]}}"#,
            ),
            &mut i,
            &mut blocks,
        );
        assert_eq!(plan, vec![ProviderEvent::Activity {
            activity: Activity::Plan {
                steps: vec![
                    PlanStep { text: "read the code".into(), state: PlanState::Done },
                    PlanStep { text: "change it".into(), state: PlanState::Active },
                    PlanStep { text: "test it".into(), state: PlanState::Pending },
                ],
            },
        }]);

        let commands = acp_events(
            &note(
                r#"{"update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"compact","description":"Summarise the conversation"},{"name":"review","input":{"hint":"[path]"}}]}}"#,
            ),
            &mut i,
            &mut blocks,
        );
        assert_eq!(commands, vec![ProviderEvent::Activity {
            activity: Activity::Commands {
                commands: vec![
                    DriverCommand {
                        name: "compact".into(),
                        description: Some("Summarise the conversation".into()),
                        argument_hint: None,
                    },
                    DriverCommand {
                        name: "review".into(),
                        description: None,
                        argument_hint: Some("[path]".into()),
                    },
                ],
            },
        }]);
        assert_eq!(i, 0, "neither opened a block, because neither is one");
    }

    #[test]
    fn a_diff_an_agent_reports_lands_where_the_transcript_reads_edits() {
        let mut i = 0;
        let mut blocks = Default::default();
        let events = acp_events(
            &note(
                r#"{"update":{"sessionUpdate":"tool_call","toolCallId":"t9","title":"Edit main.rs","kind":"edit","rawInput":{"file_path":"/w/main.rs"},"content":[{"type":"diff","path":"/w/main.rs","oldText":"a\nb","newText":"a\nc"}]}}"#,
            ),
            &mut i,
            &mut blocks,
        );
        let input = events
            .iter()
            .find_map(|e| match e {
                ProviderEvent::ToolInputDelta { partial_json, .. } => Some(partial_json.clone()),
                _ => None,
            })
            .expect("the call's input");
        let v: Value = serde_json::from_str(&input).expect("json");
        assert_eq!(v["old_text"], "a\nb");
        assert_eq!(v["new_text"], "a\nc");
        assert_eq!(v["path"], "/w/main.rs");
        assert_eq!(v["file_path"], "/w/main.rs", "what the agent's own tool took is untouched");
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
            conversation: neosh_proto::SessionId::from("test"),
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

    // ---- permission requests ------------------------------------------

    fn ask(kind: &str, extra: Value) -> Value {
        let mut call = json!({ "toolCallId": "c1", "title": "Run cargo test", "kind": kind });
        if let (Some(o), Some(e)) = (call.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                o.insert(k.clone(), v.clone());
            }
        }
        json!({
            "sessionId": "s1",
            "toolCall": call,
            "options": [
                { "optionId": "o-yes", "name": "Yes, and don't ask again for cargo", "kind": "allow_always" },
                { "optionId": "o-once", "name": "Yes", "kind": "allow_once" },
                { "optionId": "o-no", "name": "No, and tell Claude what to do differently", "kind": "reject_once" },
            ],
        })
    }

    #[test]
    fn a_request_keeps_the_agents_own_wording_and_its_own_options() {
        let r = permission_request(&ask("execute", json!({ "rawInput": { "command": "cargo test" } })), Path::new("/w"));
        assert_eq!(r.title, "Run cargo test");
        assert_eq!(r.capability, Capability::Exec { command: "cargo test".into() });
        assert_eq!(r.cwd, Path::new("/w"));
        assert_eq!(
            r.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            ["o-yes", "o-once", "o-no"],
            "in the agent's order, so a UI can present them as offered"
        );
        assert_eq!(r.options[0].label, "Yes, and don't ask again for cargo");
        assert_eq!(r.options[0].kind, PermissionOptionKind::AllowAlways);
    }

    #[test]
    fn the_kind_of_call_becomes_the_capability_policy_understands() {
        let at = |k, extra| permission_request(&ask(k, extra), Path::new("/w")).capability;
        let loc = json!({ "locations": [{ "path": "src/main.rs" }] });
        assert_eq!(at("read", loc.clone()), Capability::ReadFile { path: "src/main.rs".into() });
        assert_eq!(at("edit", loc.clone()), Capability::WriteFile { path: "src/main.rs".into() });
        assert_eq!(at("delete", loc), Capability::WriteFile { path: "src/main.rs".into() });
        assert_eq!(
            at("fetch", json!({ "rawInput": { "url": "https://example.com:8443/a/b?c" } })),
            Capability::Network { host: "example.com".into() }
        );
    }

    #[test]
    fn a_shape_nobody_recognises_is_gated_as_if_it_were_arbitrary_execution() {
        // The strictest of the four, deliberately: an unknown effect waved through is the mistake
        // that cannot be taken back.
        let r = permission_request(&ask("something-acp-adds-in-2027", Value::Null), Path::new("/w"));
        assert_eq!(r.capability, Capability::Exec { command: "Run cargo test".into() });
    }

    #[test]
    fn a_path_is_taken_from_where_acp_says_the_effect_lands() {
        // `locations` beats guessing which key of `rawInput` this particular agent uses.
        let params = ask(
            "edit",
            json!({ "locations": [{ "path": "a.rs" }], "rawInput": { "file_path": "b.rs" } }),
        );
        assert_eq!(
            permission_request(&params, Path::new("/w")).capability,
            Capability::WriteFile { path: "a.rs".into() }
        );
    }

    #[test]
    fn with_nobody_to_ask_only_full_access_answers_yes() {
        let params = ask("execute", Value::Null);
        for m in [PermissionMode::Ask, PermissionMode::AllowListed, PermissionMode::Deny] {
            assert_eq!(permission_answer(&params, m), None, "{m:?} has no one to ask");
        }
        assert_eq!(
            permission_answer(&params, PermissionMode::Allow).as_deref(),
            Some("o-once"),
            "and takes the narrowest yes rather than the agent's 'always'"
        );
    }

    #[test]
    fn an_option_neosh_cannot_read_is_dropped_rather_than_guessed_at() {
        let params = json!({
            "toolCall": { "title": "Do a thing", "kind": "other" },
            "options": [
                { "optionId": "a", "name": "Sure", "kind": "allow_once" },
                { "optionId": "b", "name": "Maybe", "kind": "invent_a_kind" },
                { "name": "No id at all", "kind": "reject_once" },
            ],
        });
        let r = permission_request(&params, Path::new("/w"));
        assert_eq!(r.options.len(), 1);
        assert_eq!(r.options[0].id, "a");
    }

    #[derive(Debug)]
    struct Fake(PermissionAnswer, Arc<Mutex<Vec<PermissionRequest>>>);

    #[async_trait::async_trait]
    impl PermissionAsker for Fake {
        async fn ask(&self, request: PermissionRequest) -> PermissionAnswer {
            self.1.lock().expect("seen lock poisoned").push(request);
            self.0.clone()
        }
    }

    fn faking(answer: PermissionAnswer) -> (Arc<dyn PermissionAsker>, Arc<Mutex<Vec<PermissionRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (Arc::new(Fake(answer, seen.clone())), seen)
    }

    async fn answered(
        asker: Option<&Arc<dyn PermissionAsker>>,
        mode: PermissionMode,
        params: &Value,
    ) -> (Value, bool) {
        permission_outcome(asker, mode, permission_request(params, Path::new("/w")), params).await
    }

    #[tokio::test]
    async fn the_request_reaches_whoever_is_listening_rather_than_the_mode() {
        let params = ask("execute", Value::Null);
        let (asker, seen) = faking(PermissionAnswer::Option("o-yes".into()));
        let (outcome, refused) = answered(Some(&asker), PermissionMode::Ask, &params).await;
        assert!(!refused);
        assert_eq!(outcome["outcome"]["outcome"], "selected");
        assert_eq!(
            outcome["outcome"]["optionId"], "o-yes",
            "the option the person actually picked, not one reconstructed from a yes"
        );
        let seen = seen.lock().expect("seen lock poisoned");
        assert_eq!(seen.len(), 1, "asked once, in `ask` mode, where it used to refuse silently");
        assert_eq!(seen[0].title, "Run cargo test");
    }

    #[tokio::test]
    async fn a_yes_that_names_no_option_takes_the_narrowest_one() {
        let params = ask("execute", Value::Null);
        let (asker, _) = faking(PermissionAnswer::Allow);
        let (outcome, _) = answered(Some(&asker), PermissionMode::Ask, &params).await;
        assert_eq!(outcome["outcome"]["optionId"], "o-once");
    }

    #[tokio::test]
    async fn a_refusal_is_a_cancellation_the_agent_can_act_on() {
        let params = ask("execute", Value::Null);
        let (asker, _) = faking(PermissionAnswer::Deny);
        let (outcome, refused) = answered(Some(&asker), PermissionMode::Allow, &params).await;
        assert!(refused, "counted, so the turn can say why it stopped short");
        assert_eq!(outcome["outcome"]["outcome"], "cancelled");
    }

    #[tokio::test]
    async fn with_no_asker_at_all_it_is_the_old_behaviour_exactly() {
        let params = ask("execute", Value::Null);
        let (outcome, refused) = answered(None, PermissionMode::Ask, &params).await;
        assert!(refused);
        assert_eq!(outcome["outcome"]["outcome"], "cancelled");
        let (outcome, refused) = answered(None, PermissionMode::Allow, &params).await;
        assert!(!refused);
        assert_eq!(outcome["outcome"]["optionId"], "o-once");
    }
}
