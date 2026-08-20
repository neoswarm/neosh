//! Drive the `codex` CLI as a provider.
//!
//! # Why this exists
//!
//! The same reason [`crate::drivers::claude_cli`] does: a Codex subscription is a plan somebody is
//! already paying for, and using it needs no API key and no trip to a billing console. Between the
//! two, the two most common plans a terminal agent's user already has are both reachable with no
//! setup at all.
//!
//! # What it actually is
//!
//! An **agent driver**. `codex exec` runs its own loop, with its own tools, its own sandbox and its
//! own approval policy — so neosh's tool registry is not in play and `tool.pre` hooks observe
//! rather than gate. [`Provider::delegates_agent_loop`] is how that is said out loud rather than
//! quietly assumed.
//!
//! The sandbox is deliberately not overridden here. `codex` has a policy, the user configured it,
//! and a wrapper that silently passed `--dangerously-bypass-approvals-and-sandbox` to make turns
//! run more smoothly would be making a security decision on somebody else's behalf. What that
//! means in practice: a turn that wants to write a file may stop and say it needs approval, and
//! the answer is to configure `codex`, not neosh.
//!
//! # The wire format
//!
//! `codex exec --json` emits one JSON object per line, documented as `ThreadEvent`: a
//! `thread.started`, then `item.started` / `item.updated` / `item.completed` for each thing the
//! agent does, then `turn.completed`. Unlike the Anthropic SSE that `claude_cli` re-uses verbatim,
//! there are **no token deltas** — an assistant message arrives whole, on completion. So a turn
//! through this driver shows its tool activity live and then its answer all at once. That is a
//! property of the CLI, not a shortcut taken here.

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use neosh_proto::{
    BlockStartKind, ContentBlock, DriverKind, InstanceConfig, Message, ModelCapabilities, ModelInfo,
    ModelTier, OptionChoice, ProviderEvent, ProviderOptionDescriptor, Role, StopReason, ToolCallId,
    TurnRequest, Usage,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{Provider, ProviderError, ProviderStream, catalog};

#[derive(Debug)]
pub struct CodexCliProvider {
    program: String,
    /// The CLI owns conversation history; we hold its thread id and resume into it.
    thread: Arc<Mutex<Option<String>>>,
}

impl Default for CodexCliProvider {
    fn default() -> Self {
        Self::new("codex")
    }
}

impl CodexCliProvider {
    pub fn new(program: impl Into<String>) -> Self {
        Self { program: program.into(), thread: Arc::default() }
    }

    /// Whether the CLI is on `PATH`. Used to decide if this instance is offerable at all.
    pub fn available(&self) -> bool {
        super::claude_cli::which(&self.program).is_some()
    }

    /// Forget the CLI-side thread, so the next turn starts a new one.
    pub fn reset_session(&self) {
        *self.thread.lock().expect("thread lock poisoned") = None;
    }

    /// The newest user message. `codex exec` takes one prompt; `resume` supplies the history.
    fn prompt_from(messages: &[Message]) -> String {
        messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
}

/// Turn one `ThreadEvent` line into provider events.
///
/// A free function taking already-parsed JSON, so the mapping is testable against recorded output
/// without a `codex` binary anywhere near it — which matters, because the CLI is exactly the kind
/// of dependency CI does not have.
///
/// `index` is the caller's block counter: this protocol identifies items by an opaque id, and
/// `ProviderEvent` addresses blocks by position, so the translation has to keep the count.
pub fn codex_events(v: &serde_json::Value, next_index: &mut u32) -> Vec<ProviderEvent> {
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or_default();
    let item = v.get("item");
    let detail = |name: &str| item.and_then(|i| i.get(name)).and_then(|x| x.as_str());
    let item_type = item.and_then(|i| i.get("type")).and_then(|t| t.as_str()).unwrap_or_default();
    let item_id = item.and_then(|i| i.get("id")).and_then(|x| x.as_str()).unwrap_or("item");

    let mut block = |kind: BlockStartKind, body: Vec<ProviderEvent>| {
        let index = *next_index;
        *next_index += 1;
        let mut out = vec![ProviderEvent::BlockStart { index, block: kind }];
        out.extend(body.into_iter().map(|e| reindex(e, index)));
        out.push(ProviderEvent::BlockStop { index });
        out
    };

    match (kind, item_type) {
        ("thread.started", _) => vec![ProviderEvent::MessageStart { model: None, usage: Usage::default() }],

        // Answers and reasoning arrive whole, on completion — there is no delta event in this
        // protocol. Emitting them on `item.started` would print an empty block.
        ("item.completed", "agent_message") => {
            let text = detail("text").unwrap_or_default().to_string();
            block(BlockStartKind::Text, vec![ProviderEvent::TextDelta { index: 0, text }])
        }
        ("item.completed", "reasoning") => {
            let text = detail("text").unwrap_or_default().to_string();
            block(BlockStartKind::Thinking, vec![ProviderEvent::ThinkingDelta { index: 0, text }])
        }

        // Tool activity is shown as it starts, because that is the part of an agent turn worth
        // watching. The result is not fed back to neosh's tool layer — codex already ran it.
        ("item.started", "command_execution") => {
            let command = detail("command").unwrap_or_default();
            block(
                BlockStartKind::ToolUse { id: ToolCallId::from(item_id), name: "shell".into() },
                vec![ProviderEvent::ToolInputDelta {
                    index: 0,
                    partial_json: serde_json::json!({ "command": command }).to_string(),
                }],
            )
        }
        ("item.started", "mcp_tool_call") => {
            let server = detail("server").unwrap_or_default();
            let tool = detail("tool").unwrap_or_default();
            let arguments = item
                .and_then(|i| i.get("arguments"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            block(
                BlockStartKind::ToolUse {
                    id: ToolCallId::from(item_id),
                    name: format!("{server}.{tool}"),
                },
                vec![ProviderEvent::ToolInputDelta {
                    index: 0,
                    partial_json: arguments.to_string(),
                }],
            )
        }
        ("item.started", "web_search") => {
            let query = detail("query").unwrap_or_default();
            block(
                BlockStartKind::ToolUse { id: ToolCallId::from(item_id), name: "web_search".into() },
                vec![ProviderEvent::ToolInputDelta {
                    index: 0,
                    partial_json: serde_json::json!({ "query": query }).to_string(),
                }],
            )
        }
        // A patch is only reported once, on completion, and the interesting part is which files.
        ("item.completed", "file_change") => {
            let paths: Vec<&str> = item
                .and_then(|i| i.get("changes"))
                .and_then(|c| c.as_array())
                .map(|a| a.iter().filter_map(|c| c.get("path").and_then(|p| p.as_str())).collect())
                .unwrap_or_default();
            block(
                BlockStartKind::ToolUse { id: ToolCallId::from(item_id), name: "apply_patch".into() },
                vec![ProviderEvent::ToolInputDelta {
                    index: 0,
                    partial_json: serde_json::json!({ "path": paths.join(", ") }).to_string(),
                }],
            )
        }

        ("turn.completed", _) => {
            let usage = v.get("usage");
            let n = |k: &str| usage.and_then(|u| u.get(k)).and_then(|x| x.as_u64()).unwrap_or(0);
            vec![
                ProviderEvent::MessageDelta {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Usage {
                        input_tokens: n("input_tokens"),
                        output_tokens: n("output_tokens"),
                        cache_read_tokens: n("cached_input_tokens"),
                        cache_write_tokens: n("cache_write_input_tokens"),
                        thinking_tokens: n("reasoning_output_tokens"),
                    },
                },
                ProviderEvent::MessageStop,
            ]
        }

        // The one terminal failure. A bare `error` line is *not* one — see [`codex_complaint`].
        ("turn.failed", _) => vec![ProviderEvent::Error {
            message: v
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .unwrap_or("the turn failed")
                .to_string(),
            retryable: false,
        }],

        // Everything else — `turn.started`, `item.updated`, todo lists, collab tools — is either
        // covered by an event we already emit or has nothing to show. Ignored rather than guessed
        // at: an unknown item type appearing in the transcript as a blank block is worse than not
        // appearing at all.
        _ => Vec::new(),
    }
}

/// A complaint that is not, on its own, a failure.
///
/// Learned from real output: a 401 produces eleven `{"type":"error","message":"Reconnecting… 2/5"}`
/// lines before the CLI gives up, and an `item.completed` carrying an `error` item for things like
/// "falling back from WebSockets to HTTPS". Treating any of those as terminal would abort a turn
/// that was about to succeed on the third attempt.
///
/// So they are collected rather than emitted, and the last one becomes the *reason* if the turn
/// ends without an answer — which is the message worth showing, and much better than the raw
/// tracing lines on stderr that would otherwise be reported instead.
pub fn codex_complaint(v: &serde_json::Value) -> Option<String> {
    let kind = v.get("type").and_then(|t| t.as_str())?;
    match kind {
        "error" => v.get("message").and_then(|m| m.as_str()).map(str::to_string),
        "item.completed"
            if v.pointer("/item/type").and_then(|t| t.as_str()) == Some("error") =>
        {
            v.pointer("/item/message").and_then(|m| m.as_str()).map(str::to_string)
        }
        _ => None,
    }
}

/// Put a block-body event on the block it belongs to.
fn reindex(ev: ProviderEvent, index: u32) -> ProviderEvent {
    match ev {
        ProviderEvent::TextDelta { text, .. } => ProviderEvent::TextDelta { index, text },
        ProviderEvent::ThinkingDelta { text, .. } => ProviderEvent::ThinkingDelta { index, text },
        ProviderEvent::ToolInputDelta { partial_json, .. } => {
            ProviderEvent::ToolInputDelta { index, partial_json }
        }
        other => other,
    }
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
                    label: catalog::title_case(l),
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
        let thread = self.thread.clone();

        tokio::spawn(async move {
            let prompt = Self::prompt_from(&request.messages);
            let previous = thread.lock().expect("thread lock poisoned").clone();

            let mut cmd = Command::new(&program);
            if !request.cwd.as_os_str().is_empty() {
                cmd.current_dir(&request.cwd);
            }
            cmd.arg("exec");
            if let Some(id) = &previous {
                cmd.arg("resume").arg(id);
            }
            cmd.arg("--json")
                // The workspace is wherever neosh was started, which is not always a repository.
                // Without this, `codex exec` refuses to run there at all.
                .arg("--skip-git-repo-check")
                .args(["--model", request.selection.model.as_ref()]);
            if let Some(effort) = request.selection.option_str("effort") {
                cmd.args(["-c", &format!("model_reasoning_effort=\"{effort}\"")]);
            }
            // Last, so it is unambiguously the positional prompt and not the value of a flag.
            cmd.arg(&prompt);

            cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx
                        .send(ProviderEvent::Error {
                            message: format!("could not run {program:?}: {e}"),
                            retryable: false,
                        })
                        .await;
                    return;
                }
            };

            let stdout = child.stdout.take().expect("stdout was piped");
            let stderr = child.stderr.take().expect("stderr was piped");
            let mut lines = BufReader::new(stdout).lines();

            // Keep stderr so a failure can be reported with its actual cause rather than an exit
            // code. `codex` logs to stderr in normal operation, so this is not evidence of failure
            // on its own — only of what went wrong when something did.
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

            let mut index = 0u32;
            let mut saw_stop = false;
            let mut last_complaint: Option<String> = None;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        // Cancellation must reap the child, or an interrupted turn leaves an agent
                        // running in the background — still editing files, still spending money.
                        let _ = child.start_kill();
                        break;
                    }
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                                    continue; // non-JSON chatter on stdout is not fatal
                                };
                                if v.get("type").and_then(|t| t.as_str()) == Some("thread.started")
                                    && let Some(id) =
                                        v.get("thread_id").and_then(|s| s.as_str())
                                {
                                    *thread.lock().expect("thread lock poisoned") =
                                        Some(id.to_string());
                                }
                                if let Some(reason) = codex_complaint(&v) {
                                    last_complaint = Some(reason);
                                    continue;
                                }
                                for ev in codex_events(&v, &mut index) {
                                    saw_stop |= matches!(ev, ProviderEvent::MessageStop);
                                    if tx.send(ev).await.is_err() {
                                        let _ = child.start_kill();
                                        return;
                                    }
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                let _ = tx.send(ProviderEvent::Error {
                                    message: format!("reading {program} output: {e}"),
                                    retryable: false,
                                }).await;
                                break;
                            }
                        }
                    }
                }
            }

            let status = child.wait().await;
            if !saw_stop && !cancel.is_cancelled() {
                // What the CLI said, in preference to what it logged: `codex` writes tracing
                // lines to stderr in normal operation, and the last thing it complained about on
                // the protocol is both shorter and actually about this turn.
                let logged = stderr_buf.lock().expect("stderr lock poisoned").trim().to_string();
                let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                let message = last_complaint.or_else(|| (!logged.is_empty()).then_some(logged));
                let _ = tx
                    .send(ProviderEvent::Error {
                        message: message.unwrap_or_else(|| {
                            format!("{program} exited with status {code} before finishing the turn")
                        }),
                        retryable: false,
                    })
                    .await;
            }
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{AuthRef, InstanceId, ModelSelection};

    fn ev(line: &str, index: &mut u32) -> Vec<ProviderEvent> {
        codex_events(&serde_json::from_str(line).expect("fixture parses"), index)
    }

    #[test]
    fn an_answer_arrives_as_one_text_block() {
        // No token deltas in this protocol: the message is complete when it is reported. A driver
        // that emitted on `item.started` would open a block and put nothing in it.
        let mut i = 0;
        assert!(ev(r#"{"type":"item.started","item":{"id":"a","type":"agent_message","text":""}}"#, &mut i).is_empty());
        let got = ev(
            r#"{"type":"item.completed","item":{"id":"a","type":"agent_message","text":"done"}}"#,
            &mut i,
        );
        assert_eq!(got.len(), 3);
        assert!(matches!(got[0], ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text }));
        assert!(matches!(&got[1], ProviderEvent::TextDelta { index: 0, text } if text == "done"));
        assert!(matches!(got[2], ProviderEvent::BlockStop { index: 0 }));
    }

    #[test]
    fn a_command_the_agent_ran_is_shown_with_what_it_ran() {
        let mut i = 0;
        let got = ev(
            r#"{"type":"item.started","item":{"id":"c1","type":"command_execution",
                "command":"ls -la","aggregated_output":"","status":"in_progress"}}"#,
            &mut i,
        );
        match &got[0] {
            ProviderEvent::BlockStart { block: BlockStartKind::ToolUse { id, name }, .. } => {
                assert_eq!(name, "shell");
                assert_eq!(id.0, "c1", "the CLI's own id, so an update can find its block");
            }
            other => panic!("expected a tool block, got {other:?}"),
        }
        assert!(
            matches!(&got[1], ProviderEvent::ToolInputDelta { partial_json, .. }
                if partial_json.contains("ls -la")),
            "and the command is in the input, where the transcript looks for a subject"
        );
    }

    #[test]
    fn blocks_are_numbered_in_the_order_they_open() {
        let mut i = 0;
        let a = ev(r#"{"type":"item.completed","item":{"id":"a","type":"reasoning","text":"hm"}}"#, &mut i);
        let b = ev(r#"{"type":"item.completed","item":{"id":"b","type":"agent_message","text":"hi"}}"#, &mut i);
        assert!(matches!(a[0], ProviderEvent::BlockStart { index: 0, .. }));
        assert!(matches!(b[0], ProviderEvent::BlockStart { index: 1, .. }));
        assert!(matches!(b[1], ProviderEvent::TextDelta { index: 1, .. }), "and so is the body");
    }

    #[test]
    fn a_finished_turn_carries_what_it_cost() {
        let mut i = 0;
        let got = ev(
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":4,
                "cache_write_input_tokens":2,"output_tokens":7,"reasoning_output_tokens":3}}"#,
            &mut i,
        );
        match &got[0] {
            ProviderEvent::MessageDelta { usage, stop_reason } => {
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 7);
                assert_eq!(usage.cache_read_tokens, 4);
                assert_eq!(usage.thinking_tokens, 3);
                assert_eq!(*stop_reason, Some(StopReason::EndTurn));
            }
            other => panic!("expected a message delta, got {other:?}"),
        }
        assert!(matches!(got[1], ProviderEvent::MessageStop));
    }

    #[test]
    fn a_failed_turn_becomes_an_error_event() {
        let mut i = 0;
        let got = ev(r#"{"type":"turn.failed","error":{"message":"rate limited"}}"#, &mut i);
        assert!(matches!(&got[0], ProviderEvent::Error { message, .. } if message == "rate limited"));
    }

    #[test]
    fn a_retry_notice_does_not_end_the_turn() {
        // Taken from real output: an unauthenticated `codex exec` emits eleven of these before it
        // gives up. Treating the first as terminal would abort a turn that was about to succeed on
        // the third attempt.
        let line = r#"{"type":"error","message":"Reconnecting... 2/5 (401 Unauthorized)"}"#;
        let mut i = 0;
        assert!(ev(line, &mut i).is_empty(), "not a stream event");
        assert_eq!(
            codex_complaint(&serde_json::from_str(line).unwrap()).as_deref(),
            Some("Reconnecting... 2/5 (401 Unauthorized)"),
            "but kept, because it is the reason if nothing else arrives"
        );
    }

    #[test]
    fn a_non_fatal_error_item_is_kept_as_a_reason_rather_than_drawn() {
        // Also from real output. "Falling back from WebSockets to HTTPS" is a thing that happened,
        // not a thing the user asked about.
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"error",
            "message":"Falling back from WebSockets to HTTPS transport."}}"#;
        let mut i = 0;
        assert!(ev(line, &mut i).is_empty());
        assert!(
            codex_complaint(&serde_json::from_str(line).unwrap())
                .is_some_and(|m| m.contains("Falling back")),
        );
    }

    #[test]
    fn an_item_type_we_have_never_heard_of_is_ignored() {
        // Rather than rendered as an empty block. The CLI adds item types faster than this file
        // will be updated, and a blank card in the transcript reads as a bug in neosh.
        let mut i = 0;
        assert!(ev(r#"{"type":"item.started","item":{"id":"x","type":"telepathy"}}"#, &mut i).is_empty());
        assert_eq!(i, 0, "and it does not consume a block index");
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
        let inst = InstanceConfig {
            id: InstanceId::from("codex-cli"),
            driver: DriverKind::from("codex-cli"),
            display_name: "Codex".into(),
            base_url: None,
            brand: None,
            auth: AuthRef::Inherited,
            models: vec![],
            extra_headers: vec![],
        };
        let req = TurnRequest {
            cwd: std::path::PathBuf::new(),
            selection: ModelSelection {
                instance: InstanceId::from("codex-cli"),
                model: "gpt-5.6-terra".into(),
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

    /// A real `model/list` entry, copied verbatim from `codex app-server` on a signed-in machine.
    ///
    /// Recorded rather than hand-written because the point of discovery is that it answers
    /// questions no catalogue of ours could — the `ultra` effort level here is one nobody wrote
    /// down, and a fixture I invented would not have it.
    const SOL: &str = include_str!("../../tests/fixtures/codex_model_sol.json");
    const MINI: &str = include_str!("../../tests/fixtures/codex_model_mini.json");

    fn parse(raw: &str) -> ModelInfo {
        codex_model(&serde_json::from_str(raw).expect("fixture is JSON")).expect("a model")
    }

    #[test]
    fn a_discovered_model_keeps_the_name_and_blurb_the_cli_gave_it() {
        let m = parse(SOL);
        assert_eq!(m.id.as_ref(), "gpt-5.6-sol");
        assert_eq!(m.display_name, "GPT-5.6-Sol");
        assert_eq!(m.tagline.as_deref(), Some("Latest frontier agentic coding model."));
        assert!(!m.legacy);
    }

    /// The whole reason to ask rather than to write a list down: this ladder has a level our own
    /// `EFFORT_CODEX` never had, and it arrives with the CLI's own description of each rung.
    #[test]
    fn the_effort_ladder_is_the_one_this_model_actually_accepts() {
        let m = parse(SOL);
        let ProviderOptionDescriptor::Select { options, current_value, .. } =
            &m.capabilities.option_descriptors[0]
        else {
            panic!("effort should be a select");
        };
        let ids: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["low", "medium", "high", "xhigh", "max", "ultra"]);
        assert_eq!(current_value.as_deref(), Some("low"));
        assert!(options.iter().find(|o| o.id == "low").expect("low").is_default);
        assert_eq!(
            options[0].description.as_deref(),
            Some("Fast responses with lighter reasoning")
        );
    }

    /// `hidden` is the CLI's "not in the default picker" flag, which is what `legacy` means here:
    /// reachable, folded away, and the reason you went looking is that you are reproducing
    /// something.
    #[test]
    fn a_model_the_cli_hides_is_offered_as_superseded_rather_than_dropped() {
        let m = parse(MINI);
        assert!(m.legacy);
        assert_eq!(m.tier, Some(ModelTier::Fast));
    }

    #[test]
    fn the_rung_comes_from_the_name_and_an_unknown_one_lands_in_the_middle() {
        assert_eq!(codex_tier("gpt-5.6-sol"), ModelTier::Frontier);
        assert_eq!(codex_tier("gpt-5.6-luna"), ModelTier::Fast);
        assert_eq!(codex_tier("gpt-5.4-mini"), ModelTier::Fast);
        assert_eq!(codex_tier("gpt-5.6-terra"), ModelTier::Balanced);
        assert_eq!(codex_tier("something-new"), ModelTier::Balanced);
    }
}
