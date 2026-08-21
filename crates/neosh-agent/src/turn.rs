//! Folding a provider event stream into a message.
//!
//! Partial content is a first-class state here, not an edge case: a turn is renderable at every
//! point in its life, and the same structure that streams into the UI is the one replayed to the
//! provider next turn.
//!
//! The one rule that matters: **tool arguments are accumulated as text and parsed exactly once**,
//! at block close. Parsing or matching on a fragment is the classic way to get a tool call that
//! silently does the wrong thing, and providers differ in how they escape and chunk that JSON.
//!
//! The second rule, learned the hard way: **a block index is only unique within one message.** An
//! agent driver replays a whole CLI session down a single stream — several `message_start`s, each
//! numbering its blocks from zero — so keying blocks by index alone means the second message's
//! text block silently overwrites the first message's tool call, and the transcript loses the
//! tool. Blocks are keyed by `(message, index)` and held in the order they opened.

use std::collections::BTreeMap;

use neosh_proto::{
    Activity, BlockStartKind, ContentBlock, Message, ProviderEvent, Role, StopReason, ToolCallId,
    Usage,
};

#[derive(Debug, Clone, PartialEq)]
enum Partial {
    Text { text: String },
    Thinking { text: String, signature: Option<String> },
    ToolUse { id: ToolCallId, name: String, json: String },
    /// What a tool came back with, reported by a driver that ran it itself.
    Result { id: ToolCallId, content: String, is_error: bool },
}

/// Something the UI or the agent loop should react to as it happens.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnUpdate {
    /// Assistant text arrived. Append it; never re-render the whole message.
    Text { text: String },
    Thinking { text: String },
    /// A tool call is complete and its arguments parsed.
    ToolReady { id: ToolCallId, name: String, input: serde_json::Value },
    /// A tool call closed with unparseable arguments. Surfaced rather than dropped, because the
    /// right response is to tell the model its arguments were malformed.
    ToolMalformed { id: ToolCallId, name: String, raw: String, error: String },
    /// A tool call a delegating driver ran for itself has come back.
    ToolResult { id: ToolCallId, content: String, is_error: bool },
    /// The driver's own loop said something about itself. Passed straight through: none of it is
    /// part of the message, so the assembler has nothing to fold it into and nothing to decide.
    Activity { activity: Activity },
    Error { message: String, retryable: bool },
}

#[derive(Debug, Default)]
pub struct TurnAssembler {
    /// Every block, in the order it opened. Order is the whole point: a transcript that reordered
    /// a tool call and the sentence explaining it would be describing a different turn.
    slots: Vec<Partial>,
    /// Which slot a streaming block writes into, keyed by `(message, index)`.
    at: BTreeMap<(u32, u32), usize>,
    /// Which message the stream is in. Bumped by each `message_start` after the first, because an
    /// agent driver sends several down one stream and each restarts its indices at zero.
    seq: u32,
    stop_reason: Option<StopReason>,
    usage: Usage,
    model: Option<String>,
}

impl TurnAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Whether the stream has reported why it stopped.
    pub fn stop_reason(&self) -> Option<&StopReason> {
        self.stop_reason.as_ref()
    }

    /// The slot a `(message, index)` block writes into, opening one if the stream never announced
    /// it. A delta for a block we never saw open is still real output, and discarding the model's
    /// words over a protocol quirk is the worse failure.
    fn slot(&mut self, index: u32, make: impl FnOnce() -> Partial) -> usize {
        let key = (self.seq, index);
        if let Some(i) = self.at.get(&key) {
            return *i;
        }
        self.slots.push(make());
        let i = self.slots.len() - 1;
        self.at.insert(key, i);
        i
    }

    fn open(&self, index: u32) -> Option<&Partial> {
        self.at.get(&(self.seq, index)).and_then(|i| self.slots.get(*i))
    }

    pub fn push(&mut self, ev: ProviderEvent) -> Vec<TurnUpdate> {
        match ev {
            ProviderEvent::MessageStart { model, usage } => {
                // Not the first: an agent driver sends a whole session down one stream, and the
                // next message numbers its blocks from zero again.
                if !self.slots.is_empty() {
                    self.seq += 1;
                }
                self.model = model;
                self.usage.merge(&usage);
                vec![]
            }
            ProviderEvent::BlockStart { index, block } => {
                let p = match block {
                    BlockStartKind::Text => Partial::Text { text: String::new() },
                    BlockStartKind::Thinking => {
                        Partial::Thinking { text: String::new(), signature: None }
                    }
                    BlockStartKind::ToolUse { id, name } => {
                        Partial::ToolUse { id, name, json: String::new() }
                    }
                };
                let key = (self.seq, index);
                match self.at.get(&key) {
                    Some(i) => self.slots[*i] = p,
                    None => {
                        self.slots.push(p);
                        self.at.insert(key, self.slots.len() - 1);
                    }
                }
                vec![]
            }
            ProviderEvent::TextDelta { index, text } => {
                let i = self.slot(index, || Partial::Text { text: String::new() });
                match &mut self.slots[i] {
                    Partial::Text { text: t } => t.push_str(&text),
                    other => *other = Partial::Text { text: text.clone() },
                }
                vec![TurnUpdate::Text { text }]
            }
            ProviderEvent::ThinkingDelta { index, text } => {
                let i =
                    self.slot(index, || Partial::Thinking { text: String::new(), signature: None });
                match &mut self.slots[i] {
                    Partial::Thinking { text: t, .. } => t.push_str(&text),
                    other => *other = Partial::Thinking { text: text.clone(), signature: None },
                }
                vec![TurnUpdate::Thinking { text }]
            }
            ProviderEvent::SignatureDelta { index, signature } => {
                if let Some(i) = self.at.get(&(self.seq, index)).copied()
                    && let Partial::Thinking { signature: s, .. } = &mut self.slots[i]
                {
                    s.get_or_insert_with(String::new).push_str(&signature);
                }
                vec![]
            }
            ProviderEvent::ToolInputDelta { index, partial_json } => {
                if let Some(i) = self.at.get(&(self.seq, index)).copied()
                    && let Partial::ToolUse { json, .. } = &mut self.slots[i]
                {
                    json.push_str(&partial_json);
                }
                vec![]
            }
            ProviderEvent::ToolResult { id, content, is_error } => {
                self.slots.push(Partial::Result {
                    id: id.clone(),
                    content: content.clone(),
                    is_error,
                });
                vec![TurnUpdate::ToolResult { id, content, is_error }]
            }
            ProviderEvent::BlockStop { index } => {
                // Only now is a tool call's JSON complete.
                match self.open(index) {
                    Some(Partial::ToolUse { id, name, json }) => {
                        let raw = if json.trim().is_empty() { "{}" } else { json.as_str() };
                        match serde_json::from_str::<serde_json::Value>(raw) {
                            Ok(input) => vec![TurnUpdate::ToolReady {
                                id: id.clone(),
                                name: name.clone(),
                                input,
                            }],
                            Err(e) => vec![TurnUpdate::ToolMalformed {
                                id: id.clone(),
                                name: name.clone(),
                                raw: json.clone(),
                                error: e.to_string(),
                            }],
                        }
                    }
                    _ => vec![],
                }
            }
            ProviderEvent::MessageDelta { stop_reason, usage } => {
                if let Some(sr) = stop_reason {
                    self.stop_reason = Some(sr);
                }
                self.usage.merge(&usage);
                vec![]
            }
            ProviderEvent::MessageStop => vec![],
            ProviderEvent::Activity { activity } => vec![TurnUpdate::Activity { activity }],
            ProviderEvent::Error { message, retryable } => {
                self.stop_reason = Some(StopReason::Error { message: message.clone() });
                vec![TurnUpdate::Error { message, retryable }]
            }
        }
    }

    /// The conversation this stream produced, in order.
    ///
    /// Usually one assistant message. A driver that runs its own loop produces the real shape —
    /// assistant, the tool results as their own user message, assistant again — because that is
    /// what happened, and because it is the only shape that replays to a *different* provider if
    /// you switch model mid-conversation. Squashing a tool result into the assistant message that
    /// asked for it produces an array no API will accept.
    pub fn messages(&self) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::new();
        for p in &self.slots {
            let (role, block) = match p {
                Partial::Text { text } if text.is_empty() => continue,
                Partial::Text { text } => {
                    (Role::Assistant, ContentBlock::Text { text: text.clone() })
                }
                Partial::Thinking { text, signature } => (Role::Assistant, ContentBlock::Thinking {
                    text: text.clone(),
                    signature: signature.clone(),
                }),
                Partial::ToolUse { id, name, json } => (Role::Assistant, ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: serde_json::from_str(if json.trim().is_empty() { "{}" } else { json })
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                }),
                Partial::Result { id, content, is_error } => (Role::User, ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                }),
            };
            match out.last_mut() {
                Some(m) if m.role == role => m.content.push(block),
                _ => out.push(Message { role, content: vec![block] }),
            }
        }
        out
    }

    /// Everything the assistant said, as one message. Safe to call mid-stream.
    ///
    /// Loses the ordering against tool results, so it is for callers that only want the words —
    /// [`Self::messages`] is what goes into a conversation.
    pub fn message(&self) -> Message {
        let content = self
            .messages()
            .into_iter()
            .filter(|m| m.role == Role::Assistant)
            .flat_map(|m| m.content)
            .collect();
        Message { role: Role::Assistant, content }
    }

    /// Tool calls the model made, in the order it made them.
    pub fn tool_calls(&self) -> Vec<(ToolCallId, String, serde_json::Value)> {
        self.slots
            .iter()
            .filter_map(|p| match p {
                Partial::ToolUse { id, name, json } => Some((
                    id.clone(),
                    name.clone(),
                    serde_json::from_str(if json.trim().is_empty() { "{}" } else { json })
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                )),
                _ => None,
            })
            .collect()
    }

    pub fn finish(self) -> (Vec<Message>, StopReason, Usage) {
        let msgs = self.messages();
        // A stream that ends without saying why is treated as a clean end rather than an error;
        // several endpoints simply close the connection after the last chunk.
        let stop = self.stop_reason.unwrap_or(StopReason::EndTurn);
        (msgs, stop, self.usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_stream() -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::MessageStart { model: Some("m".into()), usage: Usage::default() },
            ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text },
            ProviderEvent::TextDelta { index: 0, text: "Hello".into() },
            ProviderEvent::TextDelta { index: 0, text: ", world".into() },
            ProviderEvent::BlockStop { index: 0 },
            ProviderEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage { output_tokens: 3, ..Default::default() },
            },
            ProviderEvent::MessageStop,
        ]
    }

    #[test]
    fn text_deltas_accumulate_and_stream_incrementally() {
        let mut a = TurnAssembler::new();
        let updates: Vec<_> = text_stream().into_iter().flat_map(|e| a.push(e)).collect();
        assert_eq!(updates, vec![
            TurnUpdate::Text { text: "Hello".into() },
            TurnUpdate::Text { text: ", world".into() },
        ]);
        let (msgs, stop, usage) = a.finish();
        assert_eq!(msgs[0].content, vec![ContentBlock::Text { text: "Hello, world".into() }]);
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(usage.output_tokens, 3);
    }

    #[test]
    fn the_message_is_readable_mid_stream() {
        let mut a = TurnAssembler::new();
        for e in text_stream().into_iter().take(3) {
            a.push(e);
        }
        assert_eq!(a.message().content, vec![ContentBlock::Text { text: "Hello".into() }]);
    }

    #[test]
    fn tool_arguments_are_parsed_once_at_block_close() {
        let mut a = TurnAssembler::new();
        let mut updates = Vec::new();
        for e in [
            ProviderEvent::BlockStart {
                index: 0,
                block: BlockStartKind::ToolUse {
                    id: ToolCallId("t1".into()),
                    name: "read_file".into(),
                },
            },
            // Neither fragment is valid JSON on its own.
            ProviderEvent::ToolInputDelta { index: 0, partial_json: r#"{"path":"#.into() },
            ProviderEvent::ToolInputDelta { index: 0, partial_json: r#""a.txt"}"#.into() },
            ProviderEvent::BlockStop { index: 0 },
        ] {
            updates.extend(a.push(e));
        }
        assert_eq!(updates, vec![TurnUpdate::ToolReady {
            id: ToolCallId("t1".into()),
            name: "read_file".into(),
            input: json!({"path": "a.txt"}),
        }]);
    }

    #[test]
    fn a_tool_call_with_no_arguments_yields_an_empty_object() {
        let mut a = TurnAssembler::new();
        a.push(ProviderEvent::BlockStart {
            index: 0,
            block: BlockStartKind::ToolUse { id: ToolCallId("t".into()), name: "now".into() },
        });
        let u = a.push(ProviderEvent::BlockStop { index: 0 });
        assert_eq!(u, vec![TurnUpdate::ToolReady {
            id: ToolCallId("t".into()),
            name: "now".into(),
            input: json!({}),
        }]);
    }

    #[test]
    fn malformed_tool_arguments_are_reported_not_silently_dropped() {
        let mut a = TurnAssembler::new();
        a.push(ProviderEvent::BlockStart {
            index: 0,
            block: BlockStartKind::ToolUse { id: ToolCallId("t".into()), name: "f".into() },
        });
        a.push(ProviderEvent::ToolInputDelta { index: 0, partial_json: "{not json".into() });
        let u = a.push(ProviderEvent::BlockStop { index: 0 });
        assert!(matches!(u.as_slice(), [TurnUpdate::ToolMalformed { .. }]));
    }

    #[test]
    fn thinking_keeps_its_signature_for_replay() {
        let mut a = TurnAssembler::new();
        a.push(ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Thinking });
        a.push(ProviderEvent::ThinkingDelta { index: 0, text: "considering".into() });
        a.push(ProviderEvent::SignatureDelta { index: 0, signature: "sig".into() });
        a.push(ProviderEvent::BlockStop { index: 0 });
        assert_eq!(a.message().content, vec![ContentBlock::Thinking {
            text: "considering".into(),
            signature: Some("sig".into()),
        }]);
    }

    #[test]
    fn blocks_are_ordered_by_when_the_model_opened_them() {
        let mut a = TurnAssembler::new();
        a.push(ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Thinking });
        a.push(ProviderEvent::ThinkingDelta { index: 0, text: "t".into() });
        a.push(ProviderEvent::BlockStart { index: 1, block: BlockStartKind::Text });
        a.push(ProviderEvent::TextDelta { index: 1, text: "answer".into() });
        let c = a.message().content;
        assert!(matches!(c[0], ContentBlock::Thinking { .. }));
        assert!(matches!(c[1], ContentBlock::Text { .. }));
    }

    #[test]
    fn a_stream_error_becomes_the_stop_reason() {
        let mut a = TurnAssembler::new();
        a.push(ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text });
        a.push(ProviderEvent::TextDelta { index: 0, text: "partial".into() });
        let u = a.push(ProviderEvent::Error { message: "boom".into(), retryable: true });
        assert_eq!(u, vec![TurnUpdate::Error { message: "boom".into(), retryable: true }]);
        let (msgs, stop, _) = a.finish();
        // The text produced before the failure is kept, not discarded.
        assert_eq!(msgs[0].content, vec![ContentBlock::Text { text: "partial".into() }]);
        assert!(matches!(stop, StopReason::Error { .. }));
    }

    #[test]
    fn a_stream_that_ends_without_a_stop_reason_counts_as_a_clean_end() {
        let mut a = TurnAssembler::new();
        a.push(ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text });
        a.push(ProviderEvent::TextDelta { index: 0, text: "x".into() });
        assert_eq!(a.finish().1, StopReason::EndTurn);
    }

    #[test]
    fn empty_text_blocks_do_not_appear_in_the_message() {
        let mut a = TurnAssembler::new();
        a.push(ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text });
        a.push(ProviderEvent::BlockStop { index: 0 });
        assert!(a.message().content.is_empty());
    }

    #[test]
    fn usage_accumulates_across_message_start_and_delta() {
        let mut a = TurnAssembler::new();
        a.push(ProviderEvent::MessageStart {
            model: None,
            usage: Usage { input_tokens: 100, cache_read_tokens: 50, ..Default::default() },
        });
        a.push(ProviderEvent::MessageDelta {
            stop_reason: None,
            usage: Usage { output_tokens: 20, ..Default::default() },
        });
        let u = a.usage();
        assert_eq!((u.input_tokens, u.cache_read_tokens, u.output_tokens), (100, 50, 20));
    }
}
