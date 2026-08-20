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

use neosh_proto::{BlockStartKind, ProviderEvent, StopReason, ToolCallId, Usage};
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
pub fn claude_cli_line(v: &Value) -> Vec<ProviderEvent> {
    let Some(kind) = v.get("type").and_then(Value::as_str) else { return Vec::new() };
    match kind {
        "stream_event" => v.get("event").and_then(anthropic).into_iter().collect(),
        // The CLI runs its own tools, and reports each result as a user message carrying
        // `tool_result` blocks. Dropping these leaves a transcript full of calls and no answers,
        // which reads as a tool that hung — so they are the other half of the stream, not chatter.
        "user" => tool_results(v.get("message").unwrap_or(&Value::Null)),
        // A `result` with is_error signals the CLI itself failed (auth, spawn, quota).
        "result" if v.get("is_error").and_then(Value::as_bool) == Some(true) => {
            vec![ProviderEvent::Error {
                message: v
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or("claude CLI reported an error")
                    .to_string(),
                retryable: false,
            }]
        }
        _ => Vec::new(),
    }
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

    #[test]
    fn parses_real_claude_cli_output() {
        let events: Vec<ProviderEvent> = REAL_CLI_LINES
            .iter()
            .flat_map(|l| claude_cli_line(&serde_json::from_str(l).unwrap()))
            .collect();

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
        assert_eq!(events.len(), 7, "non-model event families must be ignored, not mapped");
    }

    #[test]
    fn message_start_carries_cache_usage() {
        let v: Value = serde_json::from_str(REAL_CLI_LINES[0]).unwrap();
        let [ProviderEvent::MessageStart { usage, model }] = &claude_cli_line(&v)[..] else {
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
