//! Wire-format parsers, one per upstream shape, all producing [`ProviderEvent`].
//!
//! The Anthropic Messages SSE shape is the normal form for this crate. It is the most expressive of
//! the formats we target — separate thinking blocks with signatures, incremental tool-argument
//! JSON, per-block indices — so every other format can be mapped *into* it without losing
//! information, whereas the reverse is lossy.
//!
//! [`anthropic`] is deliberately reusable by more than one transport. The `claude` CLI's
//! `--output-format stream-json --include-partial-messages` mode emits the Anthropic SSE payloads
//! verbatim inside a `{"type":"stream_event","event":{...}}` envelope, so the HTTPS driver and the
//! subprocess driver share this parser exactly rather than each growing their own.

use std::collections::{BTreeMap, HashMap};

use neosh_proto::{
    Activity, BackgroundTask, BlockStartKind, DriverCommand, PlanState, PlanStep, ProviderEvent,
    StopReason, TaskId, TaskStatus, ToolCallId, Usage,
};
use serde_json::Value;

fn u64_at(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Read a usage object in Anthropic's shape. Absent fields are zero, never an error: providers add
/// fields over time and a missing one must not fail a turn.
pub fn usage_from(v: &Value) -> Usage {
    let thinking = v
        .get("output_tokens_details")
        .and_then(|d| d.get("thinking_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens: u64_at(v, "input_tokens"),
        output_tokens: u64_at(v, "output_tokens"),
        cache_read_tokens: u64_at(v, "cache_read_input_tokens"),
        cache_write_tokens: u64_at(v, "cache_creation_input_tokens"),
        thinking_tokens: thinking,
    }
}

/// Map a provider stop reason string plus optional details.
pub fn stop_reason_from(s: &str, details: Option<&Value>) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        // A refusal arrives as HTTP 200 with this stop reason, not as an error. Code that only
        // checks transport failures renders an empty turn and looks like a hang.
        "refusal" => StopReason::Refusal {
            category: details
                .and_then(|d| d.get("category"))
                .and_then(Value::as_str)
                .map(str::to_string),
            explanation: details
                .and_then(|d| d.get("explanation"))
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        other => StopReason::Error { message: format!("unknown stop reason: {other}") },
    }
}

/// Parse one Anthropic SSE `data:` payload.
///
/// Returns `None` for events that carry no information we model (`ping`, and any future event type
/// we do not recognize). Unknown events are ignored rather than treated as errors — a provider
/// adding an event type must not break an existing client.
pub fn anthropic(v: &Value) -> Option<ProviderEvent> {
    let ty = v.get("type")?.as_str()?;
    let index = || v.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;

    Some(match ty {
        "message_start" => {
            let m = v.get("message");
            ProviderEvent::MessageStart {
                model: m
                    .and_then(|m| m.get("model"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                usage: m.and_then(|m| m.get("usage")).map(usage_from).unwrap_or_default(),
            }
        }
        "content_block_start" => {
            let b = v.get("content_block")?;
            let block = match b.get("type")?.as_str()? {
                "text" => BlockStartKind::Text,
                "thinking" | "redacted_thinking" => BlockStartKind::Thinking,
                "tool_use" => BlockStartKind::ToolUse {
                    id: ToolCallId(b.get("id")?.as_str()?.to_string()),
                    name: b.get("name")?.as_str()?.to_string(),
                },
                _ => return None,
            };
            ProviderEvent::BlockStart { index: index(), block }
        }
        "content_block_delta" => {
            let d = v.get("delta")?;
            let i = index();
            match d.get("type")?.as_str()? {
                "text_delta" => {
                    ProviderEvent::TextDelta { index: i, text: str_at(d, "text")? }
                }
                "thinking_delta" => {
                    ProviderEvent::ThinkingDelta { index: i, text: str_at(d, "thinking")? }
                }
                "signature_delta" => ProviderEvent::SignatureDelta {
                    index: i,
                    signature: str_at(d, "signature")?,
                },
                "input_json_delta" => ProviderEvent::ToolInputDelta {
                    index: i,
                    partial_json: str_at(d, "partial_json")?,
                },
                _ => return None,
            }
        }
        "content_block_stop" => ProviderEvent::BlockStop { index: index() },
        "message_delta" => {
            let d = v.get("delta");
            // Providers have moved `stop_details` between the envelope and the delta; accept both.
            let details =
                v.get("stop_details").or_else(|| d.and_then(|d| d.get("stop_details")));
            ProviderEvent::MessageDelta {
                stop_reason: d
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                    .map(|s| stop_reason_from(s, details)),
                usage: v.get("usage").map(usage_from).unwrap_or_default(),
            }
        }
        "message_stop" => ProviderEvent::MessageStop,
        "error" => {
            let e = v.get("error");
            let kind = e.and_then(|e| e.get("type")).and_then(Value::as_str).unwrap_or("");
            ProviderEvent::Error {
                message: e
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("provider error")
                    .to_string(),
                // Overload and rate limiting are worth retrying; a bad request is not.
                retryable: matches!(
                    kind,
                    "overloaded_error" | "rate_limit_error" | "api_error" | "timeout_error"
                ),
            }
        }
        _ => return None,
    })
}

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Unwrap one line of the `claude` CLI's `stream-json` output.
///
/// The CLI multiplexes several event families on one stream; only `stream_event` carries model
/// output, and its `event` field is the raw Anthropic SSE payload.
pub fn claude_cli_line(v: &Value, state: &mut ClaudeState) -> Vec<ProviderEvent> {
    // A sub-agent's traffic is stamped with the call that started it, and only half of it arrives:
    // the CLI streams `stream_event` for the top-level agent alone and reports a sub-agent as
    // whole `assistant` and `user` messages. Reading those `user` lines put a column of results
    // into the transcript whose calls had never been mentioned — answers to questions nobody saw
    // asked, with no card to sit under. A sub-agent's own account of what it did comes back as the
    // result of the `Agent` call, under that call's card, and as `task_*` lines that are *not*
    // stamped this way, which is how the swarm is followed without the transcript being invaded.
    if !v.get("parent_tool_use_id").is_none_or(Value::is_null) {
        return Vec::new();
    }
    let Some(kind) = v.get("type").and_then(Value::as_str) else { return Vec::new() };
    match kind {
        "stream_event" => {
            let events: Vec<ProviderEvent> = v.get("event").and_then(anthropic).into_iter().collect();
            state.said |= events.iter().any(|e| {
                matches!(e, ProviderEvent::TextDelta { .. } | ProviderEvent::BlockStart { .. })
            });
            events
        }
        // The CLI runs its own tools, and reports each result as a user message carrying
        // `tool_result` blocks. Dropping these leaves a transcript full of calls and no answers,
        // which reads as a tool that hung — so they are the other half of the stream, not chatter.
        //
        // The same results are also where a newly created plan step learns its number, so the
        // tracker sees them before they are turned into events.
        "user" => {
            let message = v.get("message").unwrap_or(&Value::Null);
            let mut out = state.plan.saw_results(message);
            out.extend(tool_results(message));
            out
        }
        // Whole assistant messages, which `stream_event` has already said everything about — with
        // two exceptions. A tool call arrives here with its arguments *complete*, where the stream
        // delivers them as JSON fragments that are only parsed at block close. The plan is read out
        // of ordinary tool calls, so this is the copy that can be read without reassembling
        // anything.
        //
        // The other is a message the CLI wrote itself. `<synthetic>` is its own marker for a
        // sentence no model produced — the answer to a slash command it handled locally, a refusal,
        // an explanation of why it did nothing — and those never stream, because there was no
        // request to stream. Dropping them is how `/compact` on a conversation with nothing to
        // compact arrived as a question with a blank under it: the CLI said *Error: No messages to
        // compact*, on this line and nowhere else, and the `result` line that usually rescues an
        // unsaid turn carried an empty string.
        "assistant" => {
            let message = v.get("message").unwrap_or(&Value::Null);
            let mut out = state.plan.saw_calls(message);
            out.extend(synthetic_text(message, state));
            out
        }
        "system" => system_line(v, state),
        // What the plan has left. Not a content block and never one: it is the CLI's account of
        // the circumstances it is running under, which is exactly what `Activity` is for. It is
        // also the only figure of its kind that moves *during* a turn — and an agent driver's turn
        // can be twenty minutes long, which is plenty of time to cross a threshold.
        //
        // One window, the one closest to binding. The store patches rather than replaces on this,
        // because a line saying the weekly limit is at 91% says nothing at all about the session
        // limit, and treating it as a whole snapshot would erase every other row.
        "rate_limit_event" => {
            let info = v.get("rate_limit_info").unwrap_or(&Value::Null);
            let q = crate::quota::parse_claude_rate_limit_event(info);
            if q.is_empty() {
                Vec::new()
            } else {
                vec![ProviderEvent::Activity {
                    activity: Activity::Quota {
                        plan: q.plan,
                        windows: q.windows,
                        credits: q.credits,
                    },
                }]
            }
        }
        // A `result` with is_error signals the CLI itself failed (auth, spawn, quota).
        "result" if v.get("is_error").and_then(Value::as_bool) == Some(true) => {
            state.said = false;
            vec![ProviderEvent::Error {
                message: v
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or("claude CLI reported an error")
                    .to_string(),
                retryable: false,
            }]
        }
        // A turn that said nothing, which is not the same as a turn that did nothing. The only
        // account of it is on this line, so it is turned into the message the turn never sent —
        // rather than left as a question with a blank under it.
        "result" => {
            let said = std::mem::replace(&mut state.said, false);
            let text = v.get("result").and_then(Value::as_str).unwrap_or_default().trim();
            if said || text.is_empty() {
                return Vec::new();
            }
            // A `message_start` first, and not for form's sake: block indices are only unique
            // within a message, so a bare `block_start { index: 0 }` would land on top of the
            // block 0 of whatever the *previous* turn said and quietly replace it.
            vec![
                ProviderEvent::MessageStart { model: None, usage: Usage::default() },
                ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text },
                ProviderEvent::TextDelta { index: 0, text: text.to_string() },
                ProviderEvent::BlockStop { index: 0 },
                ProviderEvent::MessageStop,
            ]
        }
        _ => Vec::new(),
    }
}

/// The text of a message the CLI wrote itself, as the message it never streamed.
///
/// Only for `<synthetic>`, which is the CLI's own word for it. Reading text off *every* whole
/// assistant message would say each ordinary answer twice, once as it arrived and again when the
/// message it was assembled from turned up.
///
/// `said` is set for the same reason the streamed path sets it: some of these are repeated on the
/// `result` line, and the fallback there exists to rescue a turn that said nothing — not to say
/// this one a second time.
fn synthetic_text(message: &Value, state: &mut ClaudeState) -> Vec<ProviderEvent> {
    if message.get("model").and_then(Value::as_str) != Some("<synthetic>") {
        return Vec::new();
    }
    let text = message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    state.said = true;
    // A message of its own, for the reason the `result` fallback opens one: block indices are
    // unique only within a message, so a bare block 0 lands on top of whatever block 0 the last
    // message had.
    vec![
        ProviderEvent::MessageStart { model: None, usage: Usage::default() },
        ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text },
        ProviderEvent::TextDelta { index: 0, text: text.to_string() },
        ProviderEvent::BlockStop { index: 0 },
        ProviderEvent::MessageStop,
    ]
}

/// What reading the CLI's stream needs to remember between lines.
///
/// Mirrors [`OpenAiState`]: the parser stays a function of one line plus what came before it,
/// rather than the driver growing a second understanding of the wire format.
#[derive(Debug, Default)]
pub struct ClaudeState {
    plan: Plan,
    /// Whether anything has been said since the last `result` line.
    ///
    /// Some turns produce no message at all. `/compact` is the clearest one: two status lines, a
    /// boundary, and a `result` whose `result` field carries the only sentence anybody wrote. With
    /// nothing watching for that, the turn arrived as a question with no answer under it.
    said: bool,
}

fn activity(a: Activity) -> ProviderEvent {
    ProviderEvent::Activity { activity: a }
}

/// The `{"type":"system"}` families — everything the CLI says about its own loop rather than about
/// the message it is producing.
///
/// These were dropped wholesale until the transcript had somewhere to put them. See
/// [`neosh_proto::Activity`].
fn system_line(v: &Value, state: &mut ClaudeState) -> Vec<ProviderEvent> {
    let str_at = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let task = || v.get("task_id").and_then(Value::as_str).map(|s| TaskId(s.to_string()));
    match v.get("subtype").and_then(Value::as_str) {
        // The handshake. Among a great deal else it lists every command this install accepts —
        // project commands, plugin commands and MCP prompts included, which is exactly why the
        // list cannot be written down anywhere in neosh.
        Some("init") => {
            let Some(names) = v.get("slash_commands").and_then(Value::as_array) else {
                return Vec::new();
            };
            let commands = names
                .iter()
                .filter_map(Value::as_str)
                .map(|name| DriverCommand {
                    name: name.to_string(),
                    description: None,
                    argument_hint: None,
                })
                .collect();
            vec![activity(Activity::Commands { commands })]
        }
        Some("task_started") => {
            let Some(task) = task() else { return Vec::new() };
            vec![activity(Activity::TaskStarted {
                task,
                title: str_at("description").unwrap_or_else(|| "sub-agent".into()),
                role: str_at("subagent_type"),
                parent_call: str_at("tool_use_id").map(ToolCallId),
                model: str_at("model"),
                backgrounded: v.get("is_backgrounded").and_then(Value::as_bool).unwrap_or(false),
            })]
        }
        Some("task_progress") => {
            let Some(task) = task() else { return Vec::new() };
            vec![activity(Activity::TaskProgress {
                task,
                summary: str_at("summary").or_else(|| str_at("description")),
                last_tool: str_at("last_tool_name"),
                usage: task_usage(v.get("usage")),
            })]
        }
        // The status patch. Only worth an event when it says the task stopped being ours: a patch
        // that renames it is not news, and a stream of non-events would push everything else off
        // the screen.
        Some("task_updated") => {
            let (Some(task), Some(patch)) = (task(), v.get("patch")) else { return Vec::new() };
            let status = patch
                .get("status")
                .and_then(Value::as_str)
                .and_then(task_status)
                .or_else(|| {
                    (patch.get("is_backgrounded").and_then(Value::as_bool) == Some(true))
                        .then_some(TaskStatus::Backgrounded)
                });
            match status {
                Some(TaskStatus::Running) | None => Vec::new(),
                Some(status) => vec![activity(Activity::TaskEnded {
                    task,
                    status,
                    summary: patch.get("error").and_then(Value::as_str).map(str::to_string),
                })],
            }
        }
        // The report, which is the one that carries what the sub-agent actually concluded.
        Some("task_notification") => {
            let Some(task) = task() else { return Vec::new() };
            vec![activity(Activity::TaskEnded {
                task,
                status: str_at("status").as_deref().and_then(task_status).unwrap_or(TaskStatus::Done),
                summary: str_at("summary"),
            })]
        }
        // Everything running unattended, whole, whenever the set changes — a start, a completion, a
        // kill, a foreground sub-agent being let go of.
        //
        // A **level**, and it is read as one: the whole set replaces the whole set. The CLI says so
        // itself, and says why — a consumer that only wants "is background work running" must not
        // pair the `task_started`/`task_notification` bookends, because a missed bookend wedges a
        // running indicator that nothing will ever put right. Which is not hypothetical here: the
        // turn stops being read at its `result`, and a shell that was backgrounded keeps running
        // long after that. This was read as a fan-out of `TaskStarted` — additive, so an empty
        // `tasks` produced no events at all and the one payload that means "nothing is running any
        // more" was the one that did nothing.
        //
        // An absent `tasks` is not an empty one: a line we cannot read says nothing rather than
        // claiming the set is empty.
        Some("background_tasks_changed") => {
            let Some(tasks) = v.get("tasks").and_then(Value::as_array) else { return Vec::new() };
            let tasks = tasks
                .iter()
                .filter_map(|t| {
                    Some(BackgroundTask {
                        id: TaskId(t.get("task_id").and_then(Value::as_str)?.to_string()),
                        title: t
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        kind: t.get("task_type").and_then(Value::as_str).map(str::to_string),
                    })
                })
                .collect();
            vec![activity(Activity::Background { tasks })]
        }
        // A request that failed with something retryable, and the wait before it is tried again.
        //
        // The single worst-looking thing a vendor CLI does: a 529 costs several silent minutes with
        // no tokens, no card and nothing on screen, and the only way to tell it apart from a wedged
        // process is to wait longer than anybody does. The CLI has always said so on this line.
        //
        // Read from the CLI's own schema rather than from a capture — a retry is not something a
        // test can arrange on demand — so it is written to survive being wrong about the shape:
        // every field is optional, and a line with none of them still produces the one thing worth
        // saying, which is that it is waiting rather than stuck.
        Some("api_retry") => {
            let n = |k: &str| v.get(k).and_then(Value::as_u64);
            let mut what = match v.get("error_status").and_then(Value::as_u64) {
                Some(status) => format!("Retrying after {status}"),
                // Null for a connection error that never got an HTTP response, which the CLI says
                // is exactly what it means by absent here.
                None => "Retrying after a connection error".to_string(),
            };
            if let (Some(attempt), Some(max)) = (n("attempt"), n("max_retries")) {
                what.push_str(&format!(" \u{b7} attempt {attempt} of {max}"));
            }
            vec![activity(Activity::Waiting { what, retry_in_ms: n("retry_delay_ms") })]
        }
        // `status` is the CLI narrating itself. `requesting` is every turn and says nothing;
        // `compacting` is the several seconds where it answers nothing at all, which without this
        // is indistinguishable from a hang.
        Some("status") if str_at("status").as_deref() == Some("compacting") => {
            vec![activity(Activity::Compacting)]
        }
        // Compaction landed, and this is the only line with the numbers on it.
        Some("compact_boundary") => {
            let meta = v.get("compact_metadata");
            let n = |k: &str| meta.and_then(|m| m.get(k)).and_then(Value::as_u64);
            // A boundary resets the conversation the CLI is holding, so a plan built from calls
            // that are no longer in its context is a checklist for work it can no longer see.
            state.plan.clear();
            vec![activity(Activity::Compacted { before: n("pre_tokens"), after: n("post_tokens") })]
        }
        _ => Vec::new(),
    }
}

fn task_status(s: &str) -> Option<TaskStatus> {
    match s {
        "completed" | "succeeded" | "success" => Some(TaskStatus::Done),
        "failed" | "error" => Some(TaskStatus::Failed),
        "cancelled" | "canceled" | "killed" | "stopped" => Some(TaskStatus::Cancelled),
        "backgrounded" => Some(TaskStatus::Backgrounded),
        "running" | "in_progress" | "pending" | "paused" => Some(TaskStatus::Running),
        _ => None,
    }
}

/// A task's usage, which the CLI reports in its own shape rather than the Messages one — a total,
/// not a breakdown. Put in `output_tokens` because that is the field a total is least wrong in:
/// what a sub-agent cost is dominated by what it produced.
fn task_usage(v: Option<&Value>) -> Usage {
    let Some(v) = v else { return Usage::default() };
    if let Some(total) = v.get("total_tokens").and_then(Value::as_u64) {
        return Usage { output_tokens: total, ..Usage::default() };
    }
    usage_from(v)
}

/// The `tool_result` blocks of a message, if it has any.
///
/// `content` is a string for most tools and a block array for the ones that return images, so both
/// are read; a result nobody can render as text still counts as a result that arrived.
fn tool_results(message: &Value) -> Vec<ProviderEvent> {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| {
            let id = b.get("tool_use_id").and_then(Value::as_str)?;
            Some(ProviderEvent::ToolResult {
                id: ToolCallId(id.to_string()),
                content: result_text(b.get("content")),
                is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_string),
                Some(other) => Some(format!("[{other}]")),
                None => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The agent's own checklist, rebuilt from the calls that maintain it.
///
/// There is no plan channel. A plan is not something the CLI announces — it is a tool the agent
/// calls, and the list is whatever those calls have added up to. So this reads them: `TodoWrite`,
/// which carries the whole list every time, and the `TaskCreate` / `TaskUpdate` pair that replaced
/// it in recent versions and carries one step at a time. Both are supported because which one an
/// install has is not something neosh gets to decide.
///
/// The awkward part is that `TaskCreate` does not say which number it got — the *result* does
/// (`"Task #2 created successfully: …"`). So a create is held until its result comes back. That is
/// the CLI's own numbering rather than an invented one, which matters because `TaskUpdate` refers
/// to steps by it.
#[derive(Debug, Default)]
struct Plan {
    /// Steps by the number the CLI gave them. Ordered: a checklist that reshuffles between two
    /// draws is one nobody can read.
    steps: BTreeMap<u32, PlanStep>,
    /// Creates waiting for the result that names them, by tool call id.
    naming: HashMap<String, String>,
}

impl Plan {
    fn clear(&mut self) {
        self.steps.clear();
        self.naming.clear();
    }

    fn emit(&self) -> Vec<ProviderEvent> {
        vec![activity(Activity::Plan { steps: self.steps.values().cloned().collect() })]
    }

    /// Tool calls, whole, from an `assistant` line.
    fn saw_calls(&mut self, message: &Value) -> Vec<ProviderEvent> {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut changed = false;
        for b in blocks.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        {
            let (Some(name), Some(input)) =
                (b.get("name").and_then(Value::as_str), b.get("input"))
            else {
                continue;
            };
            match name {
                // The whole list, every time. Replacing rather than merging is the point: this
                // call *is* the plan, including the steps it silently dropped.
                "TodoWrite" => {
                    let Some(todos) = input.get("todos").and_then(Value::as_array) else { continue };
                    self.steps = todos
                        .iter()
                        .enumerate()
                        .filter_map(|(i, t)| {
                            let text = t.get("content").and_then(Value::as_str)?.trim();
                            (!text.is_empty()).then(|| {
                                (i as u32, PlanStep {
                                    text: text.to_string(),
                                    state: plan_state(t.get("status").and_then(Value::as_str)),
                                })
                            })
                        })
                        .collect();
                    self.naming.clear();
                    changed = true;
                }
                "TaskCreate" => {
                    if let (Some(id), Some(subject)) = (
                        b.get("id").and_then(Value::as_str),
                        input.get("subject").and_then(Value::as_str),
                    ) {
                        self.naming.insert(id.to_string(), subject.to_string());
                    }
                }
                "TaskUpdate" => {
                    let Some(n) = input.get("taskId").and_then(number) else { continue };
                    if let Some(step) = self.steps.get_mut(&n) {
                        step.state = plan_state(input.get("status").and_then(Value::as_str));
                        if let Some(subject) = input.get("subject").and_then(Value::as_str) {
                            step.text = subject.to_string();
                        }
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        if changed { self.emit() } else { Vec::new() }
    }

    /// The results of those calls, from a `user` line. Only a held `TaskCreate` is looking for one.
    fn saw_results(&mut self, message: &Value) -> Vec<ProviderEvent> {
        if self.naming.is_empty() {
            return Vec::new();
        }
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut changed = false;
        for b in
            blocks.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            let Some(id) = b.get("tool_use_id").and_then(Value::as_str) else { continue };
            let Some(text) = self.naming.remove(id) else { continue };
            // `Task #2 created successfully: …`. A create that failed says something else entirely,
            // and a step with no number is a step `TaskUpdate` could never reach again.
            if let Some(n) = result_text(b.get("content"))
                .split_once('#')
                .and_then(|(_, rest)| number(&Value::String(rest.to_string())))
            {
                self.steps.insert(n, PlanStep { text, state: PlanState::Pending });
                changed = true;
            }
        }
        if changed { self.emit() } else { Vec::new() }
    }
}

fn plan_state(s: Option<&str>) -> PlanState {
    match s {
        Some("completed" | "done") => PlanState::Done,
        Some("in_progress" | "active") => PlanState::Active,
        _ => PlanState::Pending,
    }
}

/// A step number, however it was spelled. `TaskUpdate` sends `"1"`; nothing says it always will.
fn number(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => {
            let digits: String = s.trim().chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        }
        _ => None,
    }
}

/// Parse an OpenAI-compatible chat-completions SSE payload.
///
/// One parser serves OpenAI, OpenRouter, Groq, Together, DeepSeek, xAI, Fireworks, vLLM, Ollama and
/// llama.cpp, because they all speak this shape. That is what makes "support every model" a
/// configuration problem rather than a code problem.
///
/// `state` tracks which content block index each streaming tool call occupies, since this format
/// indexes tool calls separately from content.
pub fn openai(v: &Value, state: &mut OpenAiState) -> Vec<ProviderEvent> {
    let mut out = Vec::new();

    if let Some(err) = v.get("error") {
        out.push(ProviderEvent::Error {
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider error")
                .to_string(),
            retryable: false,
        });
        return out;
    }

    if !state.started {
        state.started = true;
        out.push(ProviderEvent::MessageStart {
            model: v.get("model").and_then(Value::as_str).map(str::to_string),
            usage: Usage::default(),
        });
    }

    let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else {
        // A usage-only chunk (`stream_options: {include_usage: true}`) has no choices.
        if let Some(u) = v.get("usage") {
            out.push(ProviderEvent::MessageDelta {
                stop_reason: None,
                usage: openai_usage(u),
            });
        }
        return out;
    };

    if let Some(delta) = choice.get("delta") {
        // Reasoning models spell this differently; accept every spelling we have seen rather than
        // silently dropping the model's thinking.
        for key in ["reasoning_content", "reasoning"] {
            if let Some(t) = delta.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                let idx = state.open_block(BlockKind::Thinking, &mut out);
                out.push(ProviderEvent::ThinkingDelta { index: idx, text: t.to_string() });
            }
        }
        if let Some(t) = delta.get("content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            let idx = state.open_block(BlockKind::Text, &mut out);
            out.push(ProviderEvent::TextDelta { index: idx, text: t.to_string() });
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let slot = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let f = call.get("function");
                if let Some(name) = f.and_then(|f| f.get("name")).and_then(Value::as_str) {
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("call_{slot}"));
                    let idx = state.open_tool(slot, &mut out);
                    out.push(ProviderEvent::BlockStart {
                        index: idx,
                        block: BlockStartKind::ToolUse { id: ToolCallId(id), name: name.into() },
                    });
                }
                if let Some(args) =
                    f.and_then(|f| f.get("arguments")).and_then(Value::as_str).filter(|s| !s.is_empty())
                {
                    if let Some(idx) = state.tool_slots.get(&slot).copied() {
                        out.push(ProviderEvent::ToolInputDelta {
                            index: idx,
                            partial_json: args.to_string(),
                        });
                    }
                }
            }
        }
    }

    if let Some(fin) = choice.get("finish_reason").and_then(Value::as_str) {
        state.close_all(&mut out);
        let stop = match fin {
            "stop" => StopReason::EndTurn,
            "tool_calls" | "function_call" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            "content_filter" => {
                StopReason::Refusal { category: Some("content_filter".into()), explanation: None }
            }
            other => StopReason::Error { message: format!("unknown finish reason: {other}") },
        };
        out.push(ProviderEvent::MessageDelta {
            stop_reason: Some(stop),
            usage: v.get("usage").map(openai_usage).unwrap_or_default(),
        });
    }
    out
}

fn openai_usage(v: &Value) -> Usage {
    Usage {
        input_tokens: u64_at(v, "prompt_tokens"),
        output_tokens: u64_at(v, "completion_tokens"),
        cache_read_tokens: v
            .get("prompt_tokens_details")
            .map(|d| u64_at(d, "cached_tokens"))
            .unwrap_or(0),
        cache_write_tokens: 0,
        thinking_tokens: v
            .get("completion_tokens_details")
            .map(|d| u64_at(d, "reasoning_tokens"))
            .unwrap_or(0),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

/// Per-stream bookkeeping for the OpenAI shape, which has no explicit block lifecycle.
#[derive(Debug, Default)]
pub struct OpenAiState {
    started: bool,
    next_index: u32,
    current: Option<(BlockKind, u32)>,
    tool_slots: std::collections::HashMap<usize, u32>,
}

impl OpenAiState {
    fn open_block(&mut self, kind: BlockKind, out: &mut Vec<ProviderEvent>) -> u32 {
        if let Some((k, i)) = self.current {
            if k == kind {
                return i;
            }
            out.push(ProviderEvent::BlockStop { index: i });
        }
        let index = self.next_index;
        self.next_index += 1;
        self.current = Some((kind, index));
        out.push(ProviderEvent::BlockStart {
            index,
            block: match kind {
                BlockKind::Text => BlockStartKind::Text,
                BlockKind::Thinking => BlockStartKind::Thinking,
            },
        });
        index
    }

    fn open_tool(&mut self, slot: usize, out: &mut Vec<ProviderEvent>) -> u32 {
        if let Some(i) = self.tool_slots.get(&slot) {
            return *i;
        }
        if let Some((_, i)) = self.current.take() {
            out.push(ProviderEvent::BlockStop { index: i });
        }
        let index = self.next_index;
        self.next_index += 1;
        self.tool_slots.insert(slot, index);
        index
    }

    fn close_all(&mut self, out: &mut Vec<ProviderEvent>) {
        if let Some((_, i)) = self.current.take() {
            out.push(ProviderEvent::BlockStop { index: i });
        }
        let mut slots: Vec<u32> = self.tool_slots.values().copied().collect();
        slots.sort_unstable();
        for i in slots {
            out.push(ProviderEvent::BlockStop { index: i });
        }
        self.tool_slots.clear();
    }
}

/// Parse a Google Gemini `streamGenerateContent` chunk.
pub fn gemini(v: &Value, state: &mut OpenAiState) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    if !state.started {
        state.started = true;
        out.push(ProviderEvent::MessageStart {
            model: v.get("modelVersion").and_then(Value::as_str).map(str::to_string),
            usage: Usage::default(),
        });
    }
    let Some(cand) = v.get("candidates").and_then(Value::as_array).and_then(|c| c.first()) else {
        return out;
    };
    if let Some(parts) = cand.get("content").and_then(|c| c.get("parts")).and_then(Value::as_array) {
        for part in parts {
            // Gemini marks reasoning with a `thought` flag on an otherwise normal text part.
            let is_thought = part.get("thought").and_then(Value::as_bool).unwrap_or(false);
            if let Some(t) = part.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                let kind = if is_thought { BlockKind::Thinking } else { BlockKind::Text };
                let idx = state.open_block(kind, &mut out);
                out.push(if is_thought {
                    ProviderEvent::ThinkingDelta { index: idx, text: t.to_string() }
                } else {
                    ProviderEvent::TextDelta { index: idx, text: t.to_string() }
                });
            }
            if let Some(fc) = part.get("functionCall") {
                let name = fc.get("name").and_then(Value::as_str).unwrap_or_default();
                let idx = state.open_tool(state.tool_slots.len(), &mut out);
                out.push(ProviderEvent::BlockStart {
                    index: idx,
                    block: BlockStartKind::ToolUse {
                        id: ToolCallId(format!("{name}_{idx}")),
                        name: name.to_string(),
                    },
                });
                if let Some(args) = fc.get("args") {
                    out.push(ProviderEvent::ToolInputDelta {
                        index: idx,
                        partial_json: args.to_string(),
                    });
                }
                out.push(ProviderEvent::BlockStop { index: idx });
                state.tool_slots.remove(&idx.saturating_sub(0).try_into().unwrap_or(0));
            }
        }
    }
    if let Some(reason) = cand.get("finishReason").and_then(Value::as_str) {
        state.close_all(&mut out);
        let usage = v
            .get("usageMetadata")
            .map(|u| Usage {
                input_tokens: u64_at(u, "promptTokenCount"),
                output_tokens: u64_at(u, "candidatesTokenCount"),
                thinking_tokens: u64_at(u, "thoughtsTokenCount"),
                ..Default::default()
            })
            .unwrap_or_default();
        out.push(ProviderEvent::MessageDelta {
            stop_reason: Some(match reason {
                "STOP" => StopReason::EndTurn,
                "MAX_TOKENS" => StopReason::MaxTokens,
                "SAFETY" | "PROHIBITED_CONTENT" => StopReason::Refusal {
                    category: Some(reason.to_ascii_lowercase()),
                    explanation: None,
                },
                other => StopReason::Error { message: format!("finish reason {other}") },
            }),
            usage,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// These payloads were captured from a real `claude -p --output-format stream-json
    /// --include-partial-messages` run, so this test pins the actual contract rather than a
    /// remembered one.
    const REAL_CLI_LINES: &[&str] = &[
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-haiku-4-5-20251001","id":"msg_01","type":"message","role":"assistant","content":[],"usage":{"input_tokens":10,"cache_read_input_tokens":12078}}},"session_id":"s","uuid":"u"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}},"session_id":"s","uuid":"u"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"The","estimated_tokens":null}},"session_id":"s","uuid":"u"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0},"session_id":"s"}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"1, 2, 3"}},"session_id":"s"}"#,
        r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":38}}}"#,
        r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
        r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":5}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
    ];

    /// Every line in order, through one parser state, the way the driver reads them.
    fn cli(lines: &[&str]) -> Vec<ProviderEvent> {
        let mut state = ClaudeState::default();
        lines
            .iter()
            .flat_map(|l| claude_cli_line(&serde_json::from_str(l).unwrap(), &mut state))
            .collect()
    }

    /// The one activity of a kind, or a panic naming what was there instead.
    fn only_activity(events: &[ProviderEvent]) -> Vec<&Activity> {
        events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::Activity { activity } => Some(activity),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn parses_real_claude_cli_output() {
        let events = cli(REAL_CLI_LINES);

        assert!(matches!(events[0], ProviderEvent::MessageStart { .. }));
        assert!(matches!(
            events[1],
            ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Thinking }
        ));
        assert_eq!(events[2], ProviderEvent::ThinkingDelta { index: 0, text: "The".into() });
        assert_eq!(events[3], ProviderEvent::BlockStop { index: 0 });
        assert_eq!(events[4], ProviderEvent::TextDelta { index: 1, text: "1, 2, 3".into() });
        assert!(matches!(
            events[5],
            ProviderEvent::MessageDelta { stop_reason: Some(StopReason::EndTurn), .. }
        ));
        assert_eq!(events[6], ProviderEvent::MessageStop);
        assert_eq!(
            events.len(),
            7,
            "a line that says nothing about the loop or the message stays unmapped"
        );
    }

    /// A real capture of a turn that ran the `Agent` tool. The sub-agent's three calls arrive as
    /// whole `assistant` messages the CLI never streams, so reading its `user` lines put results
    /// in the transcript with no card to sit under; its report comes back on the parent's own
    /// call, which is the line worth keeping.
    const REAL_SUBAGENT_LINES: &[&str] = &[
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_inner","type":"tool_result","content":"drwxr-xr-x - meszmate .git\n","is_error":false}]},"parent_tool_use_id":"toolu_agent","session_id":"s"}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_inner2","name":"Bash","input":{"command":"ls"}}]},"parent_tool_use_id":"toolu_agent","session_id":"s"}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_agent","type":"tool_result","content":[{"type":"text","text":"Full listing of /tmp/cwdprobe"}]}]},"parent_tool_use_id":null,"session_id":"s"}"#,
    ];

    #[test]
    fn a_sub_agents_results_are_not_the_conversations_results() {
        let events = cli(REAL_SUBAGENT_LINES);
        assert_eq!(
            events,
            vec![ProviderEvent::ToolResult {
                id: ToolCallId("toolu_agent".into()),
                content: "Full listing of /tmp/cwdprobe".into(),
                is_error: false,
            }],
            "only the `Agent` call's own result belongs to this conversation"
        );
    }

    /// A real capture of a turn that made a three-step plan and ran one sub-agent, on
    /// `claude 2.1.237`. Everything the loop said about itself is here and nothing else is: no
    /// `stream_event`, so what this pins is exactly the families that used to be dropped.
    const REAL_ACTIVITY_LINES: &[&str] = &[
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01NVuDGUeC1xTjRgpWsTpQza","name":"TaskCreate","input":{"subject":"Clear desk surface","description":"Remove all items from desk and organize them"},"caller":{"type":"direct"}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01NVuDGUeC1xTjRgpWsTpQza","type":"tool_result","content":"Task #1 created successfully: Clear desk surface"}]}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01EWUHzrGFrNezWY5iANJvb5","name":"TaskCreate","input":{"subject":"Organize desk drawers","description":"Sort and arrange items in desk drawers"},"caller":{"type":"direct"}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01EWUHzrGFrNezWY5iANJvb5","type":"tool_result","content":"Task #2 created successfully: Organize desk drawers"}]}}"#,
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a0618fd73e2bcb9ba","task_type":"local_agent","description":"Say hello"}]}"#,
        r#"{"type":"system","subtype":"task_started","task_id":"a0618fd73e2bcb9ba","tool_use_id":"toolu_01VERa8BPA1ye6fSEenrQ1AB","description":"Say hello","subagent_type":"general-purpose","task_type":"local_agent","prompt":"Reply with only the word hello."}"#,
        r#"{"type":"system","subtype":"task_updated","task_id":"a0618fd73e2bcb9ba","patch":{"status":"completed","end_time":1787281698599}}"#,
        r#"{"type":"system","subtype":"task_notification","task_id":"a0618fd73e2bcb9ba","tool_use_id":"toolu_01VERa8BPA1ye6fSEenrQ1AB","status":"completed","summary":"hello","usage":{"total_tokens":6656,"tool_uses":0,"duration_ms":1289}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01UFmF7pB2UFv1rp4QF1pY2h","name":"TaskUpdate","input":{"taskId":"1","status":"in_progress"},"caller":{"type":"direct"}}]}}"#,
    ];

    #[test]
    fn a_plan_is_rebuilt_from_the_calls_that_maintain_it() {
        let events = cli(REAL_ACTIVITY_LINES);
        let plans: Vec<&Vec<PlanStep>> = only_activity(&events)
            .into_iter()
            .filter_map(|a| match a {
                Activity::Plan { steps } => Some(steps),
                _ => None,
            })
            .collect();

        // One per change, and the first arrives with the *result* of the create — not the call,
        // which does not yet know what number it got.
        assert_eq!(plans.len(), 3, "a plan is reported when it changes, not when it is asked for");
        assert_eq!(plans[0].len(), 1);
        assert_eq!(
            plans[2],
            &vec![
                PlanStep { text: "Clear desk surface".into(), state: PlanState::Active },
                PlanStep { text: "Organize desk drawers".into(), state: PlanState::Pending },
            ],
            "`TaskUpdate` reaches step 1 by the CLI's own number"
        );
    }

    #[test]
    fn a_whole_todo_list_replaces_the_plan_rather_than_adding_to_it() {
        // The older shape, still what an install one version back sends. Two calls, and the second
        // drops a step: a plan that merged them would keep showing work that is no longer planned.
        let events = cli(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"TodoWrite","input":{"todos":[{"content":"one","status":"completed"},{"content":"two","status":"in_progress"},{"content":"three","status":"pending"}]}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"TodoWrite","input":{"todos":[{"content":"one","status":"completed"}]}}]}}"#,
        ]);
        let plans: Vec<&Vec<PlanStep>> = only_activity(&events)
            .into_iter()
            .filter_map(|a| match a {
                Activity::Plan { steps } => Some(steps),
                _ => None,
            })
            .collect();
        assert_eq!(plans[0].len(), 3);
        assert_eq!(plans[0][1].state, PlanState::Active);
        assert_eq!(plans[1].len(), 1, "the newest call is the plan, not an addition to it");
    }

    /// Recorded from a real `claude --print --input-format stream-json` run: `/compact` on a
    /// conversation with nothing in it yet. The sentence exists once, on the `assistant` line, and
    /// the `result` that usually rescues a turn that said nothing carries an empty string.
    #[test]
    fn a_sentence_the_cli_wrote_itself_is_said_rather_than_dropped() {
        let events = cli(&[
            r#"{"type":"assistant","message":{"id":"6e7aacf5","model":"<synthetic>","role":"assistant","stop_reason":"end_turn","type":"message","content":[{"type":"text","text":"Error: No messages to compact"}]},"parent_tool_use_id":null,"session_id":"s"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":0,"result":"","session_id":"s"}"#,
        ]);
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "Error: No messages to compact",
            "without this the turn is a question with a blank under it"
        );
    }

    /// The same sentence on both lines, which is the other way the CLI reports one of these.
    #[test]
    fn a_sentence_the_cli_repeats_on_the_result_line_is_still_said_once() {
        let events = cli(&[
            r#"{"type":"assistant","message":{"model":"<synthetic>","role":"assistant","type":"message","content":[{"type":"text","text":"Not enough messages to compact."}]},"session_id":"s"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Not enough messages to compact.","session_id":"s"}"#,
        ]);
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Not enough messages to compact."]);
    }

    /// The guard that keeps the above from saying every ordinary answer twice.
    #[test]
    fn a_streamed_answer_is_not_repeated_by_the_message_it_came_from() {
        let events = cli(&[
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"1, 2, 3"}},"session_id":"s"}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus-5","role":"assistant","type":"message","content":[{"type":"text","text":"1, 2, 3"}]},"session_id":"s"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"1, 2, 3","session_id":"s"}"#,
        ]);
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["1, 2, 3"], "said as it arrived, and not again afterwards");
    }

    #[test]
    fn a_sub_agent_is_followed_from_start_to_report() {
        let events = cli(REAL_ACTIVITY_LINES);
        let tasks: Vec<&Activity> = only_activity(&events)
            .into_iter()
            .filter(|a| !matches!(a, Activity::Plan { .. }))
            .collect();
        let id = TaskId("a0618fd73e2bcb9ba".into());

        assert_eq!(
            tasks[0],
            &Activity::Background {
                tasks: vec![BackgroundTask {
                    id: id.clone(),
                    title: "Say hello".into(),
                    kind: Some("local_agent".into()),
                }],
            },
            "the live set, said whole — which is also how a resumed conversation hears about work \
             it never saw start"
        );
        assert_eq!(
            tasks[1],
            &Activity::TaskStarted {
                task: id.clone(),
                title: "Say hello".into(),
                role: Some("general-purpose".into()),
                // The call whose card it belongs under. Without this the swarm is a flat list
                // beside the transcript rather than part of it.
                parent_call: Some(ToolCallId("toolu_01VERa8BPA1ye6fSEenrQ1AB".into())),
                model: None,
                backgrounded: false,
            }
        );
        assert_eq!(
            tasks[2],
            &Activity::TaskEnded { task: id.clone(), status: TaskStatus::Done, summary: None },
            "the status patch"
        );
        assert_eq!(
            tasks[3],
            &Activity::TaskEnded {
                task: id,
                status: TaskStatus::Done,
                summary: Some("hello".into()),
            },
            "and the report, which is the one that says what it concluded"
        );
    }

    /// A shell the agent put in the background, captured on `claude 2.1.240` — the whole life of
    /// one, in the order it actually arrives.
    ///
    /// The last three lines are on the far side of the turn's `result`. That is not an artefact of
    /// how this was captured: a backgrounded shell outlives the turn that started it by definition,
    /// so the only place it can report is after the turn has ended.
    const REAL_BACKGROUND_SHELL: &[&str] = &[
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bcuwuj1ft","task_type":"local_bash","description":"Sleep for 60 seconds then echo done"}]}"#,
        r#"{"type":"system","subtype":"task_started","task_id":"bcuwuj1ft","tool_use_id":"toolu_01Hp7uCzHCMEknR27ZAyyrYE","description":"Sleep for 60 seconds then echo done","is_backgrounded":true,"task_type":"local_bash"}"#,
        r#"{"type":"result","subtype":"success","duration_ms":3463}"#,
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#,
        r#"{"type":"system","subtype":"task_updated","task_id":"bcuwuj1ft","patch":{"status":"completed","end_time":1787457600637}}"#,
        r#"{"type":"system","subtype":"task_notification","task_id":"bcuwuj1ft","status":"completed","summary":"Sleep for 60 seconds then echo done"}"#,
    ];

    #[test]
    fn the_live_set_is_a_level_and_an_empty_one_says_nothing_is_running() {
        let events = cli(REAL_BACKGROUND_SHELL);
        let sets: Vec<&Vec<BackgroundTask>> = only_activity(&events)
            .into_iter()
            .filter_map(|a| match a {
                Activity::Background { tasks } => Some(tasks),
                _ => None,
            })
            .collect();
        assert_eq!(sets.len(), 2, "both payloads are events, including the empty one");
        assert_eq!(sets[0].len(), 1);
        assert_eq!(sets[0][0].id, TaskId("bcuwuj1ft".into()));
        assert_eq!(sets[0][0].kind.as_deref(), Some("local_bash"));
        // The one that used to produce nothing at all. Read as a fan-out of starts it was purely
        // additive, so the payload whose entire meaning is "the set is now empty" was silent — and
        // an indicator switched on by the first line had nothing that could ever switch it off.
        assert!(sets[1].is_empty(), "an empty set is a value, not an absence of news");
    }

    #[test]
    fn a_shell_that_was_launched_detached_says_so_at_the_start() {
        // The later move to the background arrives as a status patch and was already read. This is
        // the commoner half — `run_in_background: true` — and it is only ever said here.
        let events = cli(REAL_BACKGROUND_SHELL);
        let started = only_activity(&events)
            .into_iter()
            .find_map(|a| match a {
                Activity::TaskStarted { backgrounded, .. } => Some(*backgrounded),
                _ => None,
            })
            .expect("the start is reported");
        assert!(started, "nothing is waiting on it, from the first frame it is drawn in");
    }

    #[test]
    fn a_line_that_does_not_say_what_is_running_does_not_claim_nothing_is() {
        // Absent is not empty. A payload we cannot read has to be silent, or one malformed line
        // clears a mark that was telling the truth.
        let events = cli(&[r#"{"type":"system","subtype":"background_tasks_changed"}"#]);
        assert!(
            only_activity(&events).into_iter().all(|a| !matches!(a, Activity::Background { .. })),
            "no tasks field is no news"
        );
    }

    #[test]
    fn a_retry_says_what_it_is_waiting_for_and_how_long() {
        let events = cli(&[
            r#"{"type":"system","subtype":"api_retry","error_status":529,"attempt":2,"max_retries":5,"retry_delay_ms":4200}"#,
        ]);
        assert_eq!(
            only_activity(&events).first().copied(),
            Some(&Activity::Waiting {
                what: "Retrying after 529 \u{b7} attempt 2 of 5".into(),
                retry_in_ms: Some(4200),
            }),
        );
    }

    #[test]
    fn a_retry_that_says_almost_nothing_still_says_it_is_waiting() {
        // Read out of the CLI's schema rather than off a capture, so the shape is not something
        // this can be certain of. The one thing worth saying survives every field being missing:
        // a null `error_status` is the CLI's own way of spelling "no HTTP response at all".
        let events = cli(&[r#"{"type":"system","subtype":"api_retry"}"#]);
        assert_eq!(
            only_activity(&events).first().copied(),
            Some(&Activity::Waiting {
                what: "Retrying after a connection error".into(),
                retry_in_ms: None,
            }),
            "a wait nobody can describe is still not a hang"
        );
    }

    /// A real `/compact`, captured on `claude 2.1.237`. Three lines, no message and no
    /// `message_stop` at all — which is why a turn used to be reported as having crashed.
    const REAL_COMPACT_LINES: &[&str] = &[
        r#"{"type":"system","subtype":"status","status":"compacting"}"#,
        r#"{"type":"system","subtype":"status","status":null,"compact_result":"success"}"#,
        r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":20317,"post_tokens":1597,"cumulative_dropped_tokens":18720,"duration_ms":19927}}"#,
    ];

    #[test]
    fn a_turn_that_produced_no_message_still_says_what_happened() {
        // `/compact` is the case: two status lines, a boundary, and a `result` carrying the only
        // sentence anybody wrote. Read as "no message", the turn arrived as a question with a
        // blank under it — which is what a crash looks like.
        let mut lines = REAL_COMPACT_LINES.to_vec();
        lines.push(
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":0,"result":"Compacted the conversation."}"#,
        );
        let events = cli(&lines);
        let text: Vec<&ProviderEvent> = events
            .iter()
            .filter(|e| !matches!(e, ProviderEvent::Activity { .. }))
            .collect();
        assert_eq!(
            text,
            vec![
                // The `message_start` matters: without it this block 0 lands on top of the last
                // thing the previous turn said.
                &ProviderEvent::MessageStart { model: None, usage: Usage::default() },
                &ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text },
                &ProviderEvent::TextDelta { index: 0, text: "Compacted the conversation.".into() },
                &ProviderEvent::BlockStop { index: 0 },
                &ProviderEvent::MessageStop,
            ]
        );
    }

    #[test]
    fn a_turn_that_did_say_something_does_not_say_it_twice() {
        // The same `result` line carries a copy of the answer that was just streamed. Emitting it
        // unconditionally would print every answer twice, which is a worse bug than the one above.
        let mut lines = REAL_CLI_LINES.to_vec();
        lines.push(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"1, 2, 3"}"#,
        );
        let events = cli(&lines);
        assert_eq!(
            events.iter().filter(|e| matches!(e, ProviderEvent::TextDelta { .. })).count(),
            1,
            "the streamed answer is the answer"
        );
    }

    #[test]
    fn compaction_is_said_out_loud_at_both_ends() {
        let events = cli(REAL_COMPACT_LINES);
        assert_eq!(
            only_activity(&events),
            vec![
                &Activity::Compacting,
                &Activity::Compacted { before: Some(20317), after: Some(1597) },
            ],
            "twenty seconds of silence needs a start, and the numbers only exist on the boundary"
        );
    }

    #[test]
    fn a_compaction_throws_away_a_plan_the_cli_can_no_longer_see() {
        let mut lines: Vec<&str> = vec![
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"TodoWrite","input":{"todos":[{"content":"one","status":"pending"}]}}]}}"#,
        ];
        lines.extend_from_slice(REAL_COMPACT_LINES);
        lines.push(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"TaskUpdate","input":{"taskId":"0","status":"completed"}}]}}"#,
        );
        let events = cli(&lines);
        let plans = only_activity(&events)
            .into_iter()
            .filter(|a| matches!(a, Activity::Plan { .. }))
            .count();
        assert_eq!(plans, 1, "the update lands on a step that no longer exists, and says nothing");
    }

    #[test]
    fn the_handshake_says_which_commands_this_install_accepts() {
        let events = cli(&[
            r#"{"type":"system","subtype":"init","session_id":"s","model":"claude-opus-5","slash_commands":["compact","context","init","security-review"]}"#,
        ]);
        let [Activity::Commands { commands }] = only_activity(&events)[..] else {
            panic!("expected the command list");
        };
        assert_eq!(commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), [
            "compact",
            "context",
            "init",
            "security-review"
        ]);
    }

    #[test]
    fn message_start_carries_cache_usage() {
        let v: Value = serde_json::from_str(REAL_CLI_LINES[0]).unwrap();
        let mut state = ClaudeState::default();
        let [ProviderEvent::MessageStart { usage, model }] = &claude_cli_line(&v, &mut state)[..]
        else {
            panic!("expected MessageStart");
        };
        assert_eq!(usage.cache_read_tokens, 12078);
        assert_eq!(model.as_deref(), Some("claude-haiku-4-5-20251001"));
    }

    #[test]
    fn tool_use_blocks_and_incremental_json() {
        let start = anthropic(&json!({
            "type":"content_block_start","index":2,
            "content_block":{"type":"tool_use","id":"toolu_1","name":"read_file"}
        }))
        .unwrap();
        assert_eq!(start, ProviderEvent::BlockStart {
            index: 2,
            block: BlockStartKind::ToolUse {
                id: ToolCallId("toolu_1".into()),
                name: "read_file".into()
            }
        });

        let d = anthropic(&json!({
            "type":"content_block_delta","index":2,
            "delta":{"type":"input_json_delta","partial_json":"{\"path\":"}
        }))
        .unwrap();
        assert_eq!(d, ProviderEvent::ToolInputDelta {
            index: 2,
            partial_json: "{\"path\":".into()
        });
    }

    #[test]
    fn refusal_is_a_stop_reason_not_a_transport_error() {
        let e = anthropic(&json!({
            "type":"message_delta",
            "delta":{"stop_reason":"refusal","stop_details":{"category":"cyber"}},
            "usage":{"output_tokens":3}
        }))
        .unwrap();
        match e {
            ProviderEvent::MessageDelta { stop_reason: Some(StopReason::Refusal { category, .. }), .. } => {
                assert_eq!(category.as_deref(), Some("cyber"));
            }
            other => panic!("expected a refusal stop reason, got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_types_are_ignored_not_fatal() {
        assert!(anthropic(&json!({"type":"ping"})).is_none());
        assert!(anthropic(&json!({"type":"some_future_event","data":1})).is_none());
    }

    #[test]
    fn overload_is_retryable_but_bad_request_is_not() {
        let retry = anthropic(&json!({
            "type":"error","error":{"type":"overloaded_error","message":"busy"}
        }))
        .unwrap();
        assert_eq!(retry, ProviderEvent::Error { message: "busy".into(), retryable: true });

        let no_retry = anthropic(&json!({
            "type":"error","error":{"type":"invalid_request_error","message":"bad"}
        }))
        .unwrap();
        assert!(matches!(no_retry, ProviderEvent::Error { retryable: false, .. }));
    }

    #[test]
    fn openai_text_stream_maps_to_blocks() {
        let mut st = OpenAiState::default();
        let mut all = Vec::new();
        for chunk in [
            json!({"model":"gpt-5","choices":[{"delta":{"content":"Hel"},"index":0}]}),
            json!({"choices":[{"delta":{"content":"lo"},"index":0}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}}),
        ] {
            all.extend(openai(&chunk, &mut st));
        }
        assert!(matches!(all[0], ProviderEvent::MessageStart { .. }));
        assert!(matches!(all[1], ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text }));
        assert_eq!(all[2], ProviderEvent::TextDelta { index: 0, text: "Hel".into() });
        assert_eq!(all[3], ProviderEvent::TextDelta { index: 0, text: "lo".into() });
        assert_eq!(all[4], ProviderEvent::BlockStop { index: 0 });
        match &all[5] {
            ProviderEvent::MessageDelta { stop_reason, usage } => {
                assert_eq!(*stop_reason, Some(StopReason::EndTurn));
                assert_eq!(usage.input_tokens, 5);
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn openai_tool_calls_accumulate_arguments() {
        let mut st = OpenAiState::default();
        let mut all = Vec::new();
        for chunk in [
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read_file","arguments":""}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ] {
            all.extend(openai(&chunk, &mut st));
        }
        let json: String = all
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ToolInputDelta { partial_json, .. } => Some(partial_json.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(json, r#"{"path":"a.txt"}"#);
        assert!(serde_json::from_str::<Value>(&json).is_ok(), "must parse once complete");
        assert!(all.iter().any(|e| matches!(
            e,
            ProviderEvent::MessageDelta { stop_reason: Some(StopReason::ToolUse), .. }
        )));
    }

    #[test]
    fn openai_reasoning_field_variants_both_map_to_thinking() {
        for key in ["reasoning_content", "reasoning"] {
            let mut st = OpenAiState::default();
            let out = openai(&json!({"choices":[{"delta":{key:"hmm"}}]}), &mut st);
            assert!(
                out.iter().any(|e| matches!(e, ProviderEvent::ThinkingDelta { .. })),
                "{key} should map to thinking"
            );
        }
    }

    #[test]
    fn gemini_separates_thought_parts_from_text() {
        let mut st = OpenAiState::default();
        let mut all = Vec::new();
        all.extend(gemini(
            &json!({"modelVersion":"gemini-3-pro","candidates":[{"content":{"parts":[{"text":"pondering","thought":true}]}}]}),
            &mut st,
        ));
        all.extend(gemini(
            &json!({"candidates":[{"content":{"parts":[{"text":"answer"}]},"finishReason":"STOP"}],
                    "usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4}}),
            &mut st,
        ));
        assert!(all.iter().any(|e| matches!(e, ProviderEvent::ThinkingDelta { .. })));
        assert!(all.iter().any(|e| matches!(e, ProviderEvent::TextDelta { .. })));
        match all.last().unwrap() {
            ProviderEvent::MessageDelta { stop_reason, usage } => {
                assert_eq!(*stop_reason, Some(StopReason::EndTurn));
                assert_eq!(usage.output_tokens, 4);
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }
}
