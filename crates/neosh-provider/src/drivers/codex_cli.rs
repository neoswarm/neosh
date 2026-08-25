//! Drive `codex app-server` as a provider.
//!
//! # Why this exists
//!
//! The same reason [`crate::drivers::claude_cli`] does: a Codex subscription is a plan somebody is
//! already paying for, and using it needs no API key and no trip to a billing console. Between the
//! two, the two most common plans a terminal agent's user already has are both reachable with no
//! setup at all.
//!
//! # Why `app-server` and not `exec`
//!
//! Both are the same binary. `codex exec --json` is the one-shot: it takes a prompt, prints a line
//! per thing that happened, and exits. It is simpler, and it cannot do four things that matter:
//!
//! 1. **Stream.** `exec` reports an assistant message *whole, on completion*. `app-server` sends
//!    `item/agentMessage/delta` and `item/reasoning/textDelta`, so an answer arrives as it is
//!    written rather than all at once at the end.
//! 2. **Ask.** `exec` is non-interactive — there is nobody for it to ask — so neosh's `ask` mode
//!    could only fail closed, stopping at the first write. `app-server` sends
//!    `item/commandExecution/requestApproval` and `item/fileChange/requestApproval` as JSON-RPC
//!    requests and blocks on the reply, which is the same shape ACP uses and reaches the same
//!    person.
//! 3. **Stop.** `turn/interrupt` asks the turn to end. Killing `exec` ends the process holding the
//!    thread.
//! 4. **Say what it is doing.** `turn/plan/updated` is a real checklist, `thread/tokenUsage/updated`
//!    carries the context window, and `collabAgentToolCall` / `subAgentActivity` are the swarm.
//!    None of it exists in `exec`'s output. See [`neosh_proto::Activity`].
//!
//! And it costs nothing: `model/list` already needed an `app-server`, so this is one transport
//! where there were two.
//!
//! # What it actually is
//!
//! An **agent driver**. Codex runs its own loop, with its own tools and its own sandbox — so
//! neosh's tool registry is not in play and `tool.pre` hooks observe rather than gate.
//! [`Provider::delegates_agent_loop`] is how that is said out loud rather than quietly assumed.
//!
//! # One process per conversation
//!
//! Started once and kept, like `claude_cli` — but simpler, because there is nothing here that has
//! to be said on a command line. The model, the effort, the sandbox, the approval policy and even
//! the directory are all parameters of `turn/start`, so **nothing ever needs a new process**: a
//! change to any of them takes effect on the next turn with no relaunch at all. What the process
//! holds is the thread, and `thread/resume` puts it back if it ever has to be replaced.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use neosh_proto::{
    Activity, BlockStartKind, Capability, ContentBlock, DriverKind, InstanceConfig, Message,
    ModelCapabilities, ModelInfo, ModelTier, OptionChoice, PermissionMode, PermissionOption,
    PermissionOptionKind, PlanState, PlanStep, ProviderEvent, ProviderOptionDescriptor, Role,
    SessionId, StopReason, TaskId, TaskStatus, ToolCallId, TurnRequest, Usage,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::approval::{PermissionAnswer, PermissionAsker, PermissionRequest};
use crate::{Provider, ProviderError, ProviderStream, catalog};

/// How much the sandbox lets through, for neosh's mode.
///
/// * `deny` → `readOnly`. It may look and reason; it may not touch anything.
/// * `ask` → `readOnly` as well, but paired with an approval policy that *asks* — which is the
///   whole difference this transport makes. Under `codex exec` there was nobody to ask, so `ask`
///   meant a turn that stopped at the first write; now it means a turn that stops and enquires.
/// * `allow_listed` → `workspaceWrite`, which is codex's own idea of the same bargain neosh's
///   allow-list makes: effects are fine inside the tree you are working in.
/// * `allow` → `dangerFullAccess`. The name is the protocol's and it is not being softened here;
///   it is what "full access" means, and it happens only because somebody went and chose it.
pub fn sandbox_policy(mode: PermissionMode) -> Value {
    match mode {
        PermissionMode::Deny | PermissionMode::Ask => json!({ "type": "readOnly" }),
        PermissionMode::AllowListed => json!({ "type": "workspaceWrite" }),
        PermissionMode::Allow => json!({ "type": "dangerFullAccess" }),
    }
}

/// Whether codex should stop and ask before stepping outside its sandbox.
///
/// `on-request` only where there is both something to ask about and somebody to ask. `deny` never
/// asks because there is nothing to grant — read-only is the answer, not a starting position — and
/// `allow` never asks because being asked is precisely what it opted out of. Without an asker every
/// mode is `never`: a policy that routes decisions to a pipe nobody reads turns a turn into a hang,
/// which is worse than the refusal it replaced.
pub fn approval_policy(mode: PermissionMode, asking: bool) -> &'static str {
    match mode {
        PermissionMode::Ask | PermissionMode::AllowListed if asking => "on-request",
        _ => "never",
    }
}

/// A change asked for while a turn is running.
///
/// Thinner than [`super::claude_cli`]'s, and for a good reason: the model, the effort and the
/// sandbox are all parameters of `turn/start`, so the only way to change them is to start the next
/// turn — there is nothing to send mid-flight and nothing to keep here.
#[derive(Debug, Default)]
struct Tune {
    /// The turn to interrupt, once one is running. Written by the reader when `turn/start` answers.
    turn: Option<String>,
}

/// One conversation's app-server, and the thread it is holding.
#[derive(Debug, Default)]
struct Conversation {
    /// Locked for the length of a turn. One thread, one stdin: two turns at once would interleave
    /// two conversations down one pipe.
    live: tokio::sync::Mutex<Option<Live>>,
    tune: Mutex<Tune>,
}

/// A `codex app-server` that is still running, between turns as well as during them.
#[derive(Debug)]
struct Live {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    /// Kept so a failure can be reported with its cause. `codex` logs here in normal operation, so
    /// its contents are not evidence of failure on their own — only of what went wrong when
    /// something did.
    stderr: Arc<Mutex<String>>,
    /// The thread this conversation is, once one has been started.
    thread: Option<String>,
    /// Where the thread was started. `turn/start` takes a directory too, but the thread's own is
    /// what the agent's tools resolve against, so a conversation that moved needs a new one.
    cwd: std::path::PathBuf,
    /// Request ids, which have to be unique within the connection.
    next_id: u64,
}

#[derive(Debug)]
pub struct CodexCliProvider {
    program: String,
    /// One live app-server per conversation.
    ///
    /// Per conversation, not per driver. There is one of these objects for the whole program and
    /// several conversations may be mid-turn at once, so a single thread id here — which is what
    /// it used to be — meant the second conversation resumed into the first one's history, and
    /// nothing anywhere would have said so.
    live: Arc<Mutex<HashMap<SessionId, Arc<Conversation>>>>,
    /// What neosh's permission mode is, mirrored here because the driver is told out of band and
    /// the next turn has to act on it.
    mode: Arc<Mutex<PermissionMode>>,
    /// Who to ask when codex wants to step outside its sandbox. `None` in a test or a headless
    /// run, and the approval policy is then `never` rather than a question nobody hears.
    asker: Arc<Mutex<Option<Arc<dyn PermissionAsker>>>>,
}

impl Default for CodexCliProvider {
    fn default() -> Self {
        Self::new("codex")
    }
}

impl CodexCliProvider {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            live: Arc::default(),
            mode: Arc::new(Mutex::new(PermissionMode::Ask)),
            asker: Arc::default(),
        }
    }

    /// Whether the CLI is on `PATH`. Used to decide if this instance is offerable at all.
    pub fn available(&self) -> bool {
        super::claude_cli::which(&self.program).is_some()
    }

    /// Let go of a conversation's app-server, so nothing is left running for a conversation nobody
    /// has.
    pub fn shutdown(&self, conversation: &SessionId) {
        let Some(slot) = self.live.lock().expect("live lock poisoned").remove(conversation) else {
            return;
        };
        tokio::spawn(async move {
            if let Some(live) = slot.live.lock().await.take() {
                live.close().await;
            }
        });
    }

    fn slot(&self, conversation: &SessionId) -> Arc<Conversation> {
        self.live
            .lock()
            .expect("live lock poisoned")
            .entry(conversation.clone())
            .or_default()
            .clone()
    }

    /// The newest user message, as the input items a turn takes. The thread supplies the history.
    ///
    /// A picture goes as `localImage` — the *path*, not the bytes. The server is a process on this
    /// machine reading a file this machine wrote, so base64 down a pipe would be a megabyte of
    /// encoding to save nothing, and codex resizes and encodes it itself on the way to the model.
    fn input_from(messages: &[Message]) -> Vec<Value> {
        let Some(m) = messages.iter().rev().find(|m| m.role == Role::User) else {
            return vec![json!({"type": "text", "text": ""})];
        };
        let text = m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut items: Vec<Value> = m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Image { path, .. } => Some(json!({
                    "type": "localImage",
                    "path": path,
                })),
                _ => None,
            })
            .collect();
        items.push(json!({"type": "text", "text": text}));
        items
    }
}

/// What reading the app-server's notification stream needs to remember between lines.
///
/// The protocol identifies everything by an opaque item id; [`ProviderEvent`] addresses blocks by
/// position. This is the translation, plus the two things that can only be known by having watched:
/// whether an item streamed before it completed, and what the connection has been complaining about.
#[derive(Debug, Default)]
pub struct CodexState {
    next_index: u32,
    /// Which block index an item was given.
    blocks: HashMap<String, u32>,
    /// Items that streamed at least one delta. Their `item/completed` carries the whole text again,
    /// and emitting that would print the answer twice.
    streamed: std::collections::HashSet<String>,
    /// The last complaint. See [`codex_complaint`].
    complaint: Option<String>,
}

impl CodexState {
    /// A new turn on the same thread. Block indices restart, because a `message_start` went with it.
    fn begin(&mut self) {
        self.next_index = 0;
        self.blocks.clear();
        self.streamed.clear();
        self.complaint = None;
    }

    /// The block an item writes into, opening one the first time it is seen.
    fn index(&mut self, id: &str) -> u32 {
        if let Some(i) = self.blocks.get(id) {
            return *i;
        }
        let i = self.next_index;
        self.next_index += 1;
        self.blocks.insert(id.to_string(), i);
        i
    }

    /// What the turn should say went wrong, if it ended without an answer.
    pub fn complaint(&self) -> Option<&str> {
        self.complaint.as_deref()
    }
}

fn activity(a: Activity) -> ProviderEvent {
    ProviderEvent::Activity { activity: a }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// Turn one app-server notification into provider events.
///
/// A free function taking already-parsed JSON, so the mapping is testable against recorded traffic
/// without a `codex` binary anywhere near it — which matters, because the CLI is exactly the kind
/// of dependency CI does not have, and because a logged-out one cannot produce an answer to record.
pub fn app_server_event(v: &Value, state: &mut CodexState) -> Vec<ProviderEvent> {
    let Some(method) = str_at(v, "method") else { return Vec::new() };
    let p = v.get("params").unwrap_or(&Value::Null);
    match method {
        "turn/started" => {
            state.begin();
            vec![ProviderEvent::MessageStart { model: None, usage: Usage::default() }]
        }
        "item/started" => p.get("item").map(|i| item_started(i, state)).unwrap_or_default(),
        "item/completed" => p.get("item").map(|i| item_completed(i, state)).unwrap_or_default(),
        // The answer, as it is written. This is the whole reason for this transport.
        "item/agentMessage/delta" => stream_delta(p, state, false),
        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => stream_delta(p, state, true),
        // The checklist, whole every time — which is how a plan should arrive. See
        // [`neosh_proto::Activity::Plan`].
        "turn/plan/updated" => {
            let steps = p
                .get("plan")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|s| {
                            let text = str_at(s, "step")?.trim();
                            (!text.is_empty()).then(|| PlanStep {
                                text: text.to_string(),
                                state: match str_at(s, "status") {
                                    Some("completed") => PlanState::Done,
                                    Some("inProgress") => PlanState::Active,
                                    _ => PlanState::Pending,
                                },
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            vec![activity(Activity::Plan { steps })]
        }
        // Both at once, from one message: what the turn has spent, and how full the window is. The
        // second is a number nothing else in neosh could have known — a context meter over a vendor
        // CLI is otherwise a guess about a window whose size is the vendor's business.
        "thread/tokenUsage/updated" => {
            let usage = p.get("tokenUsage");
            let n = |side: &str, key: &str| {
                usage
                    .and_then(|u| u.get(side))
                    .and_then(|b| b.get(key))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            };
            let mut out = vec![ProviderEvent::MessageDelta {
                stop_reason: None,
                usage: Usage {
                    input_tokens: n("last", "inputTokens"),
                    output_tokens: n("last", "outputTokens"),
                    cache_read_tokens: n("last", "cachedInputTokens"),
                    cache_write_tokens: n("last", "cacheWriteInputTokens"),
                    thinking_tokens: n("last", "reasoningOutputTokens"),
                },
            }];
            let window = usage.and_then(|u| u.get("modelContextWindow")).and_then(Value::as_u64);
            if let Some(total) = window.filter(|t| *t > 0) {
                out.push(activity(Activity::Context { used: n("total", "totalTokens"), total }));
            }
            out
        }
        // What the plan has left, pushed unprompted whenever it changes. Whole, unlike `claude`'s
        // single-window line — every limit this account has, every time — so the store may replace
        // rather than patch on it.
        "account/rateLimits/updated" => {
            let q = crate::quota::parse_codex_rate_limits(
                p.get("rateLimits").filter(|r| !r.is_null()).unwrap_or(p),
            );
            if q.is_empty() {
                Vec::new()
            } else {
                vec![activity(Activity::Quota {
                    plan: q.plan,
                    windows: q.windows,
                    credits: q.credits,
                })]
            }
        }
        // No token counts on this one, unlike `claude`'s boundary. Saying it happened is still worth
        // a row: the conversation the agent is holding was just replaced by a summary of itself.
        "context/compacted" => vec![activity(Activity::Compacted { before: None, after: None })],
        "turn/completed" => {
            let turn = p.get("turn").unwrap_or(&Value::Null);
            let stop = match str_at(turn, "status") {
                Some("failed") => StopReason::Error {
                    message: turn
                        .get("error")
                        .and_then(|e| str_at(e, "message"))
                        .or_else(|| state.complaint())
                        .unwrap_or("the turn failed")
                        .to_string(),
                },
                // `cancelled` is what an interrupt produces, and the turn keeps whatever it wrote.
                _ => StopReason::EndTurn,
            };
            vec![
                ProviderEvent::MessageDelta { stop_reason: Some(stop), usage: Usage::default() },
                ProviderEvent::MessageStop,
            ]
        }
        // Collected, not emitted. See [`codex_complaint`].
        "error" | "warning" => {
            if let Some(text) = codex_complaint(p) {
                state.complaint = Some(text);
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// One `item/*Delta` notification, on the block its item opened.
fn stream_delta(p: &Value, state: &mut CodexState, thinking: bool) -> Vec<ProviderEvent> {
    let (Some(id), Some(text)) = (str_at(p, "itemId"), str_at(p, "delta")) else {
        return Vec::new();
    };
    state.streamed.insert(id.to_string());
    let index = state.index(id);
    let text = text.to_string();
    vec![if thinking {
        ProviderEvent::ThinkingDelta { index, text }
    } else {
        ProviderEvent::TextDelta { index, text }
    }]
}

/// A tool call, complete on arrival.
///
/// Codex names an item once and never revises its arguments, so the block opens and closes in one
/// go: the card appears the moment the call starts, with its subject already on it, and the answer
/// arrives later as a [`ProviderEvent::ToolResult`].
fn call(id: &str, name: &str, input: Value, state: &mut CodexState) -> Vec<ProviderEvent> {
    let index = state.index(id);
    vec![
        ProviderEvent::BlockStart {
            index,
            block: BlockStartKind::ToolUse { id: ToolCallId::from(id), name: name.to_string() },
        },
        ProviderEvent::ToolInputDelta { index, partial_json: input.to_string() },
        ProviderEvent::BlockStop { index },
    ]
}

fn item_started(item: &Value, state: &mut CodexState) -> Vec<ProviderEvent> {
    let (Some(kind), Some(id)) = (str_at(item, "type"), str_at(item, "id")) else {
        return Vec::new();
    };
    match kind {
        // Opened empty; the deltas fill it. `item/completed` carries the whole text as well, which
        // is what makes this safe: an item that never streamed still says everything it had.
        "agentMessage" => {
            let index = state.index(id);
            vec![ProviderEvent::BlockStart { index, block: BlockStartKind::Text }]
        }
        "reasoning" => {
            let index = state.index(id);
            vec![ProviderEvent::BlockStart { index, block: BlockStartKind::Thinking }]
        }
        "commandExecution" => {
            let input = json!({
                "command": str_at(item, "command").unwrap_or_default(),
                "cwd": str_at(item, "cwd").unwrap_or_default(),
            });
            call(id, "shell", input, state)
        }
        // The patch, as codex computed it before applying it — so the card has the diff from the
        // moment the call appears rather than after the fact. `cards::edits_of` reads the unified
        // form.
        "fileChange" => {
            let input = json!({ "changes": item.get("changes").cloned().unwrap_or(json!([])) });
            call(id, "apply_patch", input, state)
        }
        "mcpToolCall" => {
            let name = format!(
                "{}/{}",
                str_at(item, "server").unwrap_or("mcp"),
                str_at(item, "tool").unwrap_or("call")
            );
            call(id, &name, item.get("arguments").cloned().unwrap_or(json!({})), state)
        }
        "dynamicToolCall" => {
            let name = str_at(item, "tool").unwrap_or("tool").to_string();
            call(id, &name, item.get("arguments").cloned().unwrap_or(json!({})), state)
        }
        "webSearch" => {
            call(id, "web_search", json!({ "query": str_at(item, "query").unwrap_or_default() }), state)
        }
        // A sub-agent. Two shapes, because codex has two: one it spawned as a tool call, and one
        // running against its own thread.
        "collabAgentToolCall" => vec![activity(Activity::TaskStarted {
            task: TaskId::from(id),
            title: str_at(item, "prompt").unwrap_or("sub-agent").to_string(),
            role: str_at(item, "tool").map(str::to_string),
            parent_call: None,
            model: str_at(item, "model").map(str::to_string),
            // codex blocks the turn on both of its sub-agent shapes; nothing here is let go of.
            backgrounded: false,
        })],
        "subAgentActivity" => vec![activity(Activity::TaskStarted {
            task: TaskId::from(str_at(item, "agentThreadId").unwrap_or(id)),
            title: str_at(item, "agentPath").unwrap_or("sub-agent").to_string(),
            role: None,
            parent_call: None,
            model: None,
            backgrounded: false,
        })],
        "contextCompaction" => vec![activity(Activity::Compacted { before: None, after: None })],
        _ => Vec::new(),
    }
}

fn item_completed(item: &Value, state: &mut CodexState) -> Vec<ProviderEvent> {
    let (Some(kind), Some(id)) = (str_at(item, "type"), str_at(item, "id")) else {
        return Vec::new();
    };
    // A result belongs to the call, whether or not this stream ever mentioned the call — a turn
    // resumed mid-flight can meet the second half of one.
    let result = |content: String, is_error: bool| {
        vec![ProviderEvent::ToolResult { id: ToolCallId::from(id), content, is_error }]
    };
    match kind {
        "agentMessage" | "reasoning" => {
            let index = state.index(id);
            let mut out = Vec::new();
            // Only for an item that never streamed. Otherwise this is a second copy of the answer.
            if !state.streamed.contains(id) {
                let text = match kind {
                    "agentMessage" => str_at(item, "text").unwrap_or_default().to_string(),
                    _ => item
                        .get("content")
                        .or_else(|| item.get("summary"))
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")
                        })
                        .unwrap_or_default(),
                };
                if !text.is_empty() {
                    out.push(if kind == "agentMessage" {
                        ProviderEvent::TextDelta { index, text }
                    } else {
                        ProviderEvent::ThinkingDelta { index, text }
                    });
                }
            }
            out.push(ProviderEvent::BlockStop { index });
            out
        }
        "commandExecution" => {
            let code = item.get("exitCode").and_then(Value::as_i64);
            let output = str_at(item, "aggregatedOutput").unwrap_or_default();
            let failed = code.is_some_and(|c| c != 0);
            // The convention `cards::exit_code` reads, so a failed command says what it exited
            // with rather than only that it did.
            let content = match (failed, code) {
                (true, Some(c)) => format!("Exit code {c}\n{output}"),
                _ => output.to_string(),
            };
            result(content, failed)
        }
        "fileChange" => {
            let failed = str_at(item, "status").is_some_and(|s| s != "completed" && s != "applied");
            let files: Vec<&str> = item
                .get("changes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|c| str_at(c, "path")).collect())
                .unwrap_or_default();
            result(
                if failed {
                    format!("patch {}", str_at(item, "status").unwrap_or("failed"))
                } else {
                    files.join("\n")
                },
                failed,
            )
        }
        "mcpToolCall" | "dynamicToolCall" => {
            let error = item.get("error").and_then(|e| str_at(e, "message"));
            match error {
                Some(message) => result(message.to_string(), true),
                None => result(
                    item.get("result")
                        .map(|r| {
                            str_at(r, "text").map(str::to_string).unwrap_or_else(|| r.to_string())
                        })
                        .unwrap_or_default(),
                    false,
                ),
            }
        }
        "webSearch" => {
            let n = item.get("results").and_then(Value::as_array).map_or(0, |a| a.len());
            result(format!("{n} result(s)"), false)
        }
        "collabAgentToolCall" => vec![activity(Activity::TaskEnded {
            task: TaskId::from(id),
            status: match str_at(item, "status") {
                Some("failed") => TaskStatus::Failed,
                _ => TaskStatus::Done,
            },
            summary: None,
        })],
        "subAgentActivity" => vec![activity(Activity::TaskEnded {
            task: TaskId::from(str_at(item, "agentThreadId").unwrap_or(id)),
            status: TaskStatus::Done,
            summary: None,
        })],
        _ => Vec::new(),
    }
}

/// A complaint that is not, on its own, a failure.
///
/// Learned from real output: a 401 produces eleven `error` notifications reading
/// `"Reconnecting... 2/5"` before codex gives up, and a `warning` for things like falling back from
/// WebSockets to HTTPS. Treating any of those as terminal would abort a turn that was about to
/// succeed on the third attempt.
///
/// So they are collected rather than emitted, and the last one becomes the *reason* if the turn
/// ends without an answer — which is the message worth showing, and much better than the raw
/// tracing lines on stderr that would otherwise be reported instead.
pub fn codex_complaint(p: &Value) -> Option<String> {
    p.get("error")
        .and_then(|e| str_at(e, "message"))
        .or_else(|| str_at(p, "message"))
        .map(str::to_string)
}

/// Ask a running `codex app-server` what models it serves.
///
/// # Why a second process rather than a flag
///
/// `codex exec` has no way to list anything — it takes a prompt and runs a turn. `codex
/// app-server` is the same binary speaking JSON-RPC on stdio, and `model/list` is a documented
/// method on it that answers from the account's own catalogue. So the list is *the account's*: a
/// model somebody has early access to appears without neosh knowing it exists, and one they have
/// lost access to stops being offered.
///
/// # What comes back that a written list cannot have
///
/// Each entry carries its own reasoning-effort ladder and its own default — which differ per
/// model, and gained a level (`ultra`) that no list written by hand would have. Those become the
/// option descriptors, so the effort picker offers exactly what this model accepts.
///
/// Hidden models are asked for too. `hidden` is the CLI's own "not in the default picker" flag,
/// which is very nearly what `legacy` means here: reachable, out of the way, and the reason you
/// went looking is usually that you are reproducing something.
async fn discover(program: &str) -> Result<Vec<ModelInfo>, ProviderError> {
    let mut child = Command::new(program)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ProviderError::Transport(format!("could not run {program} app-server: {e}")))?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut lines = BufReader::new(stdout).lines();

    let hello = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "clientInfo": { "name": "neosh", "title": "neosh", "version": env!("CARGO_PKG_VERSION") } },
    });
    let ready = serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} });
    let ask = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "model/list",
        "params": { "includeHidden": true },
    });
    for msg in [hello, ready, ask] {
        let line = format!("{msg}\n");
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| ProviderError::Transport(format!("writing to {program}: {e}")))?;
    }
    stdin.flush().await.ok();

    // Notifications are interleaved with replies — a remote-control status arrives unprompted
    // between the two — so this reads until the id it asked about rather than taking line two.
    let mut out = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if v.get("id").and_then(serde_json::Value::as_u64) != Some(2) {
            continue;
        }
        if let Some(e) = v.get("error") {
            return Err(ProviderError::BadResponse(format!("{program} model/list: {e}")));
        }
        let Some(data) = v.pointer("/result/data").and_then(|d| d.as_array()) else { break };
        out.extend(data.iter().filter_map(codex_model));
        break;
    }
    // Dropping the child kills it (`kill_on_drop`), but only once it is polled; this makes the
    // reaping happen here rather than whenever the runtime next gets round to it.
    let _ = child.kill().await;

    if out.is_empty() {
        return Err(ProviderError::BadResponse(format!("{program} listed no models")));
    }
    Ok(out)
}

/// One entry of a `model/list` reply.
///
/// `pub` because the shape is worth testing against a recorded reply without a `codex` on `PATH`,
/// which is the only way this parser gets exercised in CI.
pub fn codex_model(v: &serde_json::Value) -> Option<ModelInfo> {
    let id = v.get("model").or_else(|| v.get("id"))?.as_str()?;
    let name = v.get("displayName").and_then(|d| d.as_str()).unwrap_or(id);
    let levels: Vec<&str> = v
        .get("supportedReasoningEfforts")
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|o| o.get("reasoningEffort")?.as_str()).collect())
        .unwrap_or_default();
    let default = v.get("defaultReasoningEffort").and_then(|d| d.as_str()).unwrap_or("medium");

    let mut descriptors = Vec::new();
    if !levels.is_empty() {
        descriptors.push(ProviderOptionDescriptor::Select {
            id: "effort".into(),
            label: "Effort".into(),
            description: Some("How much reasoning the model spends before answering.".into()),
            options: levels
                .iter()
                .map(|l| OptionChoice {
                    id: (*l).into(),
                    label: catalog::level_label(l),
                    // The CLI writes a sentence for each level. It is better than anything we
                    // would invent, and it is per model rather than per ladder.
                    description: v
                        .get("supportedReasoningEfforts")
                        .and_then(|e| e.as_array())
                        .and_then(|a| {
                            a.iter().find(|o| o.get("reasoningEffort").and_then(|r| r.as_str()) == Some(*l))
                        })
                        .and_then(|o| o.get("description")?.as_str())
                        .map(str::to_string),
                    is_default: *l == default,
                })
                .collect(),
            current_value: Some(default.into()),
            prompt_injected_values: Vec::new(),
        });
    }

    let hidden = v.get("hidden").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let mut m = ModelInfo::undescribed(id, name);
    m.capabilities = ModelCapabilities {
        tools: true,
        vision: v
            .get("inputModalities")
            .and_then(|i| i.as_array())
            .is_some_and(|a| a.iter().any(|m| m.as_str() == Some("image"))),
        streaming: true,
        thinking: !levels.is_empty(),
        prompt_caching: true,
        option_descriptors: descriptors,
    };
    // The family is the generation, so `model.line gpt-5.6` means "whichever Sol/Terra/Luna is
    // current" — the same trick the Claude aliases play, from a naming scheme rather than a table.
    m.family = Some(id.rsplit_once('-').map(|(head, _)| head).unwrap_or(id).to_string());
    m.tier = Some(codex_tier(id));
    m.tagline = v.get("description").and_then(|d| d.as_str()).map(str::to_string);
    m.legacy = hidden;
    Some(m)
}

/// Which rung a codex model sits on.
///
/// The ladder is in the name — Sol, Terra, Luna, in that order, and a `-mini` is always the small
/// one. Everything else is called balanced, which is where an unknown model is least wrong: it
/// will not be offered as "the most capable thing here" by an upgrade, nor as the cheap one.
fn codex_tier(id: &str) -> ModelTier {
    if id.ends_with("-sol") {
        ModelTier::Frontier
    } else if id.ends_with("-luna") || id.ends_with("-mini") {
        ModelTier::Fast
    } else {
        ModelTier::Balanced
    }
}

#[async_trait::async_trait]
impl Provider for CodexCliProvider {
    fn driver(&self) -> DriverKind {
        DriverKind::from("codex-cli")
    }

    /// What this account can reach, asked of the CLI; the written catalogue if it cannot say.
    ///
    /// The fallback is not a formality. `codex app-server` needs the same login `codex exec` does,
    /// so on a machine that has the binary but has not signed in, discovery fails — and that is
    /// exactly the machine whose owner is deciding whether signing in is worth it. An empty pane
    /// answers that with silence.
    async fn list_models(&self, instance: &InstanceConfig) -> Result<Vec<ModelInfo>, ProviderError> {
        match discover(&self.program).await {
            Ok(models) => Ok(models),
            Err(e) => {
                tracing::debug!(program = %self.program, "codex model discovery failed: {e}");
                Ok(instance.models.clone())
            }
        }
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
        // A one-shot — a branch name, a thread title — belongs to no conversation, so it gets a
        // slot nobody else can find and the app-server is let go of at the end of it. Keyed into
        // the map instead, every one of them shared a process that nothing ever closed.
        let one_shot = request.is_one_shot();
        let slot = match one_shot {
            true => Arc::new(Conversation::default()),
            false => self.slot(&request.conversation),
        };
        let mode = *self.mode.lock().expect("mode lock poisoned");
        let asker = self.asker.lock().expect("asker lock poisoned").clone();

        tokio::spawn(async move {
            let mut guard = slot.live.lock().await;
            if let Err(message) =
                run_turn(&program, &slot, &mut guard, request, mode, asker, cancel, &tx).await
            {
                // A failure the process cannot recover from leaves nothing worth keeping: the next
                // turn starts a fresh app-server rather than writing into a pipe whose other end
                // has gone.
                *guard = None;
                let _ = tx.send(ProviderEvent::Error { message, retryable: false }).await;
            }
            // Nothing is coming back to this one.
            if one_shot && let Some(live) = guard.take() {
                live.close().await;
            }
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

/// One turn, on an app-server that is either already running or about to be.
///
/// The `Err` case is for a failure that ends the turn *and* the process. Everything a running
/// server reports about itself, including its own errors, arrives as events instead.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    program: &str,
    conversation: &Conversation,
    slot: &mut Option<Live>,
    request: TurnRequest,
    mode: PermissionMode,
    asker: Option<Arc<dyn PermissionAsker>>,
    cancel: CancellationToken,
    tx: &mpsc::Sender<ProviderEvent>,
) -> Result<(), String> {
    // The thread's directory is what the agent's tools resolve against, so a conversation that has
    // moved needs a new thread — the one thing here that a running server cannot be talked out of.
    // Even that keeps the process.
    if slot.as_ref().is_some_and(|l| l.cwd != request.cwd)
        && let Some(live) = slot.take()
    {
        live.close().await;
    }
    if slot.is_none() {
        *slot = Some(Live::spawn(program, &request.cwd).await?);
    }
    let live = slot.as_mut().expect("a server was just put there");
    if live.thread.is_none() {
        live.start_thread(&request.cwd, mode).await?;
    }

    // Everything that would have been a command-line argument is a parameter of this one call, so
    // a model, an effort or a sandbox that changed since the last turn needs nothing but this.
    let asking = asker.is_some();
    let mut params = json!({
        "threadId": live.thread.clone().unwrap_or_default(),
        "input": CodexCliProvider::input_from(&request.messages),
        "model": request.selection.model.as_ref(),
        "sandboxPolicy": sandbox_policy(mode),
        "approvalPolicy": approval_policy(mode, asking),
    });
    if let Some(effort) = request.selection.option_str("effort")
        && let Some(map) = params.as_object_mut()
    {
        map.insert("effort".into(), Value::String(effort.to_string()));
    }
    let started = live.request("turn/start", params).await?;

    let mut state = CodexState::default();
    let mut turn: Option<String> = None;
    let mut interrupted = false;
    let mut finished = false;
    let giveup = tokio::time::sleep(Duration::from_secs(86_400));
    tokio::pin!(giveup);

    loop {
        tokio::select! {
            biased;
            // Asked to stop, not killed. A killed server takes the thread with it, and the next
            // turn would resume into a history that never heard of the work it interrupted.
            () = cancel.cancelled(), if !interrupted => {
                interrupted = true;
                if let Some(id) = &turn {
                    let params = json!({ "threadId": live.thread, "turnId": id });
                    let _ = live.notify_request("turn/interrupt", params).await;
                }
                giveup.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(5));
            }
            () = &mut giveup, if interrupted => {
                let _ = live.child.start_kill();
                *slot = None;
                return Ok(());
            }
            line = live.lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(ProviderEvent::Error {
                            message: format!("reading {program} app-server: {e}"),
                            retryable: false,
                        }).await;
                        *slot = None;
                        return Ok(());
                    }
                };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };

                // The reply to `turn/start`, which is the only place the turn's own id appears —
                // and `turn/interrupt` needs it.
                if v.get("id").and_then(Value::as_u64) == Some(started)
                    && let Some(id) = v.pointer("/result/turn/id").and_then(Value::as_str)
                {
                    turn = Some(id.to_string());
                    conversation.tune.lock().expect("tune lock poisoned").turn = turn.clone();
                    continue;
                }
                // A question codex is blocked on. This is the whole reason `ask` mode means
                // something here: `codex exec` had nobody to ask and could only refuse.
                if let Some(ask) = approval_request(&v, &request.cwd) {
                    // Raced against the cancellation, not awaited on its own. A prompt with
                    // nobody at the keyboard is exactly when somebody reaches for `<Esc>`, and a
                    // turn that could only be interrupted between questions could not be
                    // interrupted at all while one was open.
                    let answer = match &asker {
                        Some(a) => tokio::select! {
                            biased;
                            () = cancel.cancelled() => PermissionAnswer::Deny,
                            answer = a.ask(PermissionRequest {
                                conversation: Some(request.conversation.clone()),
                                ..ask.request.clone()
                            }) => answer,
                        },
                        None => PermissionAnswer::Deny,
                    };
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": ask.id,
                        "result": { "decision": decision(&ask, &answer) },
                    });
                    if write_line(&mut live.stdin, &reply).await.is_err() {
                        *slot = None;
                        break;
                    }
                    continue;
                }
                for ev in app_server_event(&v, &mut state) {
                    finished |= matches!(ev, ProviderEvent::MessageStop);
                    if tx.send(ev).await.is_err() {
                        // The receiver is gone: the turn was abandoned. The process is not — it
                        // holds the thread, and the next turn is probably about to use it.
                        return Ok(());
                    }
                }
                if finished {
                    break;
                }
            }
        }
    }

    if !finished && !cancel.is_cancelled() {
        // Only reachable when the process died mid-turn. Its own complaint beats its stderr, which
        // is full of ordinary tracing.
        let detail = state
            .complaint()
            .map(str::to_string)
            .or_else(|| {
                slot.as_ref().and_then(|l| {
                    let b = l.stderr.lock().expect("stderr lock poisoned");
                    (!b.trim().is_empty()).then(|| b.trim().to_string())
                })
            })
            .unwrap_or_else(|| format!("{program} app-server stopped before finishing the turn"));
        *slot = None;
        let _ = tx.send(ProviderEvent::Error { message: detail, retryable: false }).await;
    }
    Ok(())
}

/// One approval request, read into something a person can be asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    /// The JSON-RPC id to answer on.
    pub id: u64,
    /// Whether this is about running something or about writing something, which is the only thing
    /// that changes about the answer: the two decisions are different enums in the protocol.
    pub file_change: bool,
    pub request: PermissionRequest,
}

/// The option id for "yes, and stop asking for the rest of this conversation".
const FOR_SESSION: &str = "accept-for-session";
/// The option id for "yes, this once".
const ONCE: &str = "accept";
/// The option id for "no".
const DECLINE: &str = "decline";

/// Read a server-to-client approval request, if that is what this is.
///
/// The two the protocol sends are `item/commandExecution/requestApproval` and
/// `item/fileChange/requestApproval`. Both block the turn until they are answered.
pub fn approval_request(v: &Value, cwd: &std::path::Path) -> Option<Approval> {
    let id = v.get("id").and_then(Value::as_u64)?;
    let method = str_at(v, "method")?;
    let p = v.get("params")?;
    let file_change = match method {
        "item/commandExecution/requestApproval" => false,
        "item/fileChange/requestApproval" => true,
        _ => return None,
    };
    let (title, capability) = if file_change {
        let root = str_at(p, "grantRoot").unwrap_or_default();
        (
            format!("Write to {}", if root.is_empty() { "the workspace" } else { root }),
            Capability::WriteFile { path: root.to_string() },
        )
    } else {
        let command = str_at(p, "command").unwrap_or_default();
        (format!("Run {command}"), Capability::Exec { command: command.to_string() })
    };
    // Worded here rather than by the agent, because unlike ACP and the `claude` control protocol,
    // codex sends the *decisions it accepts* as a schema and no sentences to go with them. These
    // are the three that mean something in every case; the amendment-carrying ones are codex
    // negotiating with its own policy files and are not a question for a person.
    let options = vec![
        PermissionOption {
            id: ONCE.into(),
            label: "Yes".into(),
            kind: PermissionOptionKind::AllowOnce,
        },
        PermissionOption {
            id: FOR_SESSION.into(),
            label: "Yes, and don't ask again this session".into(),
            kind: PermissionOptionKind::AllowAlways,
        },
        PermissionOption {
            id: DECLINE.into(),
            label: "No".into(),
            kind: PermissionOptionKind::RejectOnce,
        },
    ];
    let reason = str_at(p, "reason").filter(|r| !r.is_empty());
    Some(Approval {
        id,
        file_change,
        request: PermissionRequest {
            title: match reason {
                Some(r) => format!("{title} — {r}"),
                None => title,
            },
            capability,
            options,
            cwd: cwd.to_path_buf(),
            // Filled in by the caller, which is the only thing here that knows which conversation
            // this driver is running.
            conversation: None,
        },
    })
}

/// The decision string for an answer. Both enums share these three spellings.
pub fn decision(ask: &Approval, answer: &PermissionAnswer) -> &'static str {
    let _ = ask;
    match answer {
        PermissionAnswer::Option(id) if id == FOR_SESSION => "acceptForSession",
        PermissionAnswer::Option(id) if id == DECLINE => "decline",
        // Anything else that is an answer at all is a yes, taken as the narrowest one — including
        // a policy answer, which is a yes to the question rather than a choice between the ways of
        // saying it.
        PermissionAnswer::Option(_) | PermissionAnswer::Allow => "accept",
        PermissionAnswer::Deny => "decline",
    }
}

impl Live {
    async fn spawn(program: &str, cwd: &std::path::Path) -> Result<Self, String> {
        let mut cmd = Command::new(program);
        if !cwd.as_os_str().is_empty() {
            cmd.current_dir(cwd);
        }
        cmd.arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child =
            cmd.spawn().map_err(|e| format!("could not run {program} app-server: {e}"))?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let stdin = child.stdin.take().expect("stdin was piped");

        let buf = Arc::new(Mutex::new(String::new()));
        {
            let buf = buf.clone();
            tokio::spawn(async move {
                let mut l = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = l.next_line().await {
                    let mut b = buf.lock().expect("stderr lock poisoned");
                    b.push_str(&line);
                    b.push('\n');
                }
            });
        }

        let mut live = Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            stderr: buf,
            thread: None,
            cwd: cwd.to_path_buf(),
            next_id: 1,
        };
        live.handshake().await?;
        Ok(live)
    }

    /// `initialize`, then `initialized`. Both are required before anything else is answered.
    async fn handshake(&mut self) -> Result<(), String> {
        let id = self
            .request(
                "initialize",
                json!({ "clientInfo": {
                    "name": "neosh", "title": "neosh", "version": env!("CARGO_PKG_VERSION"),
                } }),
            )
            .await?;
        self.await_reply(id).await?;
        write_line(
            &mut self.stdin,
            &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        )
        .await
        .map_err(|e| format!("codex app-server: {e}"))
    }

    async fn start_thread(&mut self, cwd: &std::path::Path, mode: PermissionMode) -> Result<(), String> {
        let id = self
            .request(
                "thread/start",
                json!({
                    "cwd": cwd.display().to_string(),
                    "sandbox": match mode {
                        PermissionMode::Deny | PermissionMode::Ask => "read-only",
                        PermissionMode::AllowListed => "workspace-write",
                        PermissionMode::Allow => "danger-full-access",
                    },
                }),
            )
            .await?;
        let reply = self.await_reply(id).await?;
        self.thread = reply
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "codex app-server started no thread".to_string())?
            .into();
        Ok(())
    }

    /// Send a request and hand back its id.
    async fn request(&mut self, method: &str, params: Value) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        write_line(&mut self.stdin, &message)
            .await
            .map(|()| id)
            .map_err(|e| format!("could not reach the running codex app-server: {e}"))
    }

    /// Send a request whose reply nothing is waiting for. `turn/interrupt` is one: what it does is
    /// visible as a `turn/completed`, which the turn loop is already reading.
    async fn notify_request(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.request(method, params).await.map(|_| ())
    }

    /// Read until the reply to `id`, which is the only safe way to wait for one: notifications are
    /// interleaved with replies from the first message onward.
    ///
    /// Only used before a turn starts, when nothing else is listening. Once one has, the turn loop
    /// owns the stream.
    async fn await_reply(&mut self, id: u64) -> Result<Value, String> {
        while let Ok(Some(line)) = self.lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(e) = v.get("error") {
                return Err(format!("codex app-server: {e}"));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
        Err("codex app-server stopped before answering".into())
    }

    /// Tell it there is nothing more coming, and wait for it to go.
    async fn close(mut self) {
        drop(self.stdin);
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await.is_err() {
            let _ = self.child.start_kill();
        }
    }
}

/// Write one JSON line to the server's stdin, flushed, because it is waiting on it.
async fn write_line(
    stdin: &mut tokio::process::ChildStdin,
    value: &Value,
) -> std::io::Result<()> {
    let mut line = value.to_string();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

impl super::AgentDriver for CodexCliProvider {
    fn set_permission_mode(&self, mode: PermissionMode) {
        // Nothing to send: the sandbox and the approval policy are parameters of the next
        // `turn/start`, so the mode is applied by asking the next question rather than by telling
        // the server about it now.
        *self.mode.lock().expect("mode lock poisoned") = mode;
    }

    fn set_permission_asker(&self, asker: Arc<dyn PermissionAsker>) {
        *self.asker.lock().expect("asker lock poisoned") = Some(asker);
    }

    fn close_conversation(&self, conversation: &SessionId) {
        self.shutdown(conversation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{AuthRef, InstanceId, ModelSelection};

    /// Every line in order, through one parser state, the way the driver reads them.
    fn replay(lines: &[&str]) -> Vec<ProviderEvent> {
        let mut state = CodexState::default();
        lines
            .iter()
            .flat_map(|l| app_server_event(&serde_json::from_str(l).expect("fixture parses"), &mut state))
            .collect()
    }

    fn only_activity(events: &[ProviderEvent]) -> Vec<&Activity> {
        events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::Activity { activity } => Some(activity),
                _ => None,
            })
            .collect()
    }

    /// A turn as the app-server reports it: the handshake shapes are a real capture from
    /// `codex-cli 0.148.0`; the answer path follows the protocol schema, since a logged-out codex
    /// cannot produce one to record.
    const TURN: &[&str] = &[
        r#"{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"t1","turn":{"id":"u1","items":[],"status":"inProgress"}}}"#,
        r#"{"jsonrpc":"2.0","method":"item/started","params":{"threadId":"t1","turnId":"u1","startedAtMs":1,"item":{"type":"reasoning","id":"r1","content":[]}}}"#,
        r#"{"jsonrpc":"2.0","method":"item/reasoning/textDelta","params":{"threadId":"t1","turnId":"u1","itemId":"r1","contentIndex":0,"delta":"weighing it"}}"#,
        r#"{"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"t1","turnId":"u1","item":{"type":"reasoning","id":"r1","content":["weighing it"]}}}"#,
        r#"{"jsonrpc":"2.0","method":"item/started","params":{"threadId":"t1","turnId":"u1","startedAtMs":2,"item":{"type":"commandExecution","id":"c1","command":"cargo test","cwd":"/work","commandActions":[],"status":"inProgress"}}}"#,
        r#"{"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"t1","turnId":"u1","item":{"type":"commandExecution","id":"c1","command":"cargo test","cwd":"/work","commandActions":[],"status":"failed","exitCode":101,"aggregatedOutput":"one test failed"}}}"#,
        r#"{"jsonrpc":"2.0","method":"turn/plan/updated","params":{"threadId":"t1","turnId":"u1","plan":[{"step":"read the test","status":"completed"},{"step":"fix it","status":"inProgress"}]}}"#,
        r#"{"jsonrpc":"2.0","method":"item/started","params":{"threadId":"t1","turnId":"u1","startedAtMs":3,"item":{"type":"agentMessage","id":"a1","text":""}}}"#,
        r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"t1","turnId":"u1","itemId":"a1","delta":"It fails "}}"#,
        r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"t1","turnId":"u1","itemId":"a1","delta":"on line 3."}}"#,
        r#"{"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"t1","turnId":"u1","item":{"type":"agentMessage","id":"a1","text":"It fails on line 3."}}}"#,
        r#"{"jsonrpc":"2.0","method":"thread/tokenUsage/updated","params":{"threadId":"t1","turnId":"u1","tokenUsage":{"last":{"inputTokens":120,"outputTokens":40,"cachedInputTokens":80,"reasoningOutputTokens":12,"totalTokens":160},"total":{"inputTokens":300,"outputTokens":90,"cachedInputTokens":80,"reasoningOutputTokens":12,"totalTokens":390},"modelContextWindow":272000}}}"#,
        r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u1","items":[],"status":"completed"}}}"#,
    ];

    #[test]
    fn an_answer_arrives_as_it_is_written() {
        // The whole reason for this transport. `codex exec` reports an assistant message once, on
        // completion; here it streams, and the `item/completed` that repeats it must not print it
        // a second time.
        let events = replay(TURN);
        let text: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, ["It fails ", "on line 3."], "streamed once, not streamed and repeated");
    }

    #[test]
    fn an_item_that_never_streamed_still_says_everything_it_had() {
        // The other half of the same rule. Not every item streams — a short one can complete
        // before a delta is sent — and dropping its text on that basis loses the answer entirely.
        let events = replay(&[
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"t1","turn":{"id":"u1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"item/started","params":{"item":{"type":"agentMessage","id":"a1","text":""}}}"#,
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"type":"agentMessage","id":"a1","text":"done"}}}"#,
        ]);
        assert!(
            events.iter().any(|e| matches!(e, ProviderEvent::TextDelta { text, .. } if text == "done")),
            "{events:?}"
        );
    }

    #[test]
    fn a_command_is_a_card_before_it_is_an_answer() {
        let events = replay(TURN);
        let call = events.iter().find_map(|e| match e {
            ProviderEvent::BlockStart { block: BlockStartKind::ToolUse { id, name }, .. } => {
                Some((id.clone(), name.clone()))
            }
            _ => None,
        });
        assert_eq!(call, Some((ToolCallId::from("c1"), "shell".into())));
        // The exit code is not in the protocol's idea of a result, so it is written into the text
        // in the convention the card reads. See `cards::exit_code`.
        assert!(
            events.iter().any(|e| matches!(
                e,
                ProviderEvent::ToolResult { content, is_error: true, .. }
                    if content.starts_with("Exit code 101")
            )),
            "{events:?}"
        );
    }

    #[test]
    fn the_plan_and_the_context_window_come_off_the_same_stream() {
        let events = replay(TURN);
        let acts = only_activity(&events);
        assert!(acts.contains(&&Activity::Plan {
            steps: vec![
                PlanStep { text: "read the test".into(), state: PlanState::Done },
                PlanStep { text: "fix it".into(), state: PlanState::Active },
            ],
        }));
        // A number nothing else in neosh could have known: how big this model's window is, is the
        // vendor's business and codex is the one being told.
        assert!(acts.contains(&&Activity::Context { used: 390, total: 272_000 }));
    }

    #[test]
    fn a_turn_ends_once_and_says_why() {
        let events = replay(TURN);
        assert_eq!(events.iter().filter(|e| **e == ProviderEvent::MessageStop).count(), 1);
        assert!(events.iter().any(|e| matches!(
            e,
            ProviderEvent::MessageDelta { stop_reason: Some(StopReason::EndTurn), .. }
        )));
    }

    /// A real capture: this is what a logged-out `codex app-server` does with a turn, eleven
    /// complaints deep and then a failure.
    #[test]
    fn reconnecting_is_not_a_failure_until_it_is() {
        let events = replay(&[
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"t1","turn":{"id":"u1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"error","params":{"error":{"message":"Reconnecting... 2/5"}}}"#,
            r#"{"jsonrpc":"2.0","method":"warning","params":{"threadId":"t1","message":"Falling back from WebSockets to HTTPS transport."}}"#,
            r#"{"jsonrpc":"2.0","method":"error","params":{"error":{"message":"unexpected status 401 Unauthorized"}}}"#,
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"t1","turn":{"id":"u1","status":"failed"}}}"#,
        ]);
        assert!(
            !events.iter().any(|e| matches!(e, ProviderEvent::Error { .. })),
            "a complaint mid-turn is not the turn's answer\n{events:?}"
        );
        let stop = events.iter().find_map(|e| match e {
            ProviderEvent::MessageDelta { stop_reason: Some(StopReason::Error { message }), .. } => {
                Some(message.clone())
            }
            _ => None,
        });
        assert_eq!(
            stop.as_deref(),
            Some("unexpected status 401 Unauthorized"),
            "and the last one is the reason, not the first"
        );
    }

    #[test]
    fn an_approval_reaches_a_person_with_the_command_on_it() {
        // The thing `codex exec` could not do at all: it was non-interactive, so `ask` mode could
        // only fail closed at the first write.
        let v: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":7,"method":"item/commandExecution/requestApproval","params":{"threadId":"t1","turnId":"u1","itemId":"c9","startedAtMs":1,"command":"rm -rf build","reason":"outside the sandbox"}}"#,
        )
        .unwrap();
        let ask = approval_request(&v, std::path::Path::new("/work")).expect("an approval");
        assert_eq!(ask.id, 7);
        assert!(!ask.file_change);
        assert_eq!(ask.request.capability, Capability::Exec { command: "rm -rf build".into() });
        assert!(ask.request.title.contains("rm -rf build"));
        assert!(ask.request.title.contains("outside the sandbox"), "{}", ask.request.title);

        assert_eq!(decision(&ask, &PermissionAnswer::Deny), "decline");
        assert_eq!(decision(&ask, &PermissionAnswer::Allow), "accept");
        assert_eq!(
            decision(&ask, &PermissionAnswer::Option(FOR_SESSION.into())),
            "acceptForSession"
        );
    }

    #[test]
    fn an_ordinary_notification_is_not_an_approval() {
        // Notifications have no `id`, which is the whole difference — and answering one would be
        // sending a reply to a message nobody is waiting on.
        let v: Value =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"turn/started","params":{}}"#).unwrap();
        assert_eq!(approval_request(&v, std::path::Path::new("/work")), None);
    }

    #[test]
    fn every_mode_says_something_different_to_the_sandbox() {
        use PermissionMode::*;
        assert_eq!(sandbox_policy(Deny), json!({"type": "readOnly"}));
        assert_eq!(sandbox_policy(Ask), json!({"type": "readOnly"}));
        assert_eq!(sandbox_policy(AllowListed), json!({"type": "workspaceWrite"}));
        assert_eq!(sandbox_policy(Allow), json!({"type": "dangerFullAccess"}));
    }

    #[test]
    fn nothing_is_asked_when_there_is_nobody_to_ask() {
        // A policy that routes decisions to a pipe nobody reads turns a turn into a hang, which is
        // worse than the refusal it replaced.
        for m in [
            PermissionMode::Deny,
            PermissionMode::Ask,
            PermissionMode::AllowListed,
            PermissionMode::Allow,
        ] {
            assert_eq!(approval_policy(m, false), "never", "{m:?} with no asker");
        }
        assert_eq!(approval_policy(PermissionMode::Ask, true), "on-request");
        assert_eq!(approval_policy(PermissionMode::AllowListed, true), "on-request");
        // Neither end of the range asks: `deny` has nothing to grant, and `allow` opted out.
        assert_eq!(approval_policy(PermissionMode::Deny, true), "never");
        assert_eq!(approval_policy(PermissionMode::Allow, true), "never");
    }

    fn inst() -> InstanceConfig {
        InstanceConfig {
            id: InstanceId::from("codex-cli"),
            driver: DriverKind::from("codex-cli"),
            display_name: "Codex".into(),
            base_url: None,
            brand: None,
            auth: AuthRef::Inherited,
            models: vec![],
            extra_headers: vec![],
        }
    }

    #[test]
    fn it_declares_that_it_owns_the_agent_loop() {
        assert!(CodexCliProvider::default().delegates_agent_loop());
    }

    #[tokio::test]
    async fn a_missing_binary_reports_an_error_event() {
        use futures::StreamExt;
        let p = CodexCliProvider::new("definitely-not-a-real-binary-xyzzy");
        assert!(!p.available());
        let req = TurnRequest {
            resume: None,
            conversation: SessionId::from("test"),
            cwd: std::path::PathBuf::new(),
            selection: ModelSelection {
                instance: InstanceId::from("codex-cli"),
                model: "gpt-5.6".into(),
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
        let got: Vec<_> = p.stream(&inst(), req, CancellationToken::new()).collect().await;
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], ProviderEvent::Error { retryable: false, .. }));
    }
}
