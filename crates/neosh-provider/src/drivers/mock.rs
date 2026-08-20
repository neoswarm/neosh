//! A provider that replays recorded events.
//!
//! This is what makes the Phase 0 definition of done testable in CI: the streaming, tool-call and
//! hook-veto paths all run end to end with no API key, no network and no wall-clock flakiness.
//! Fixtures are JSONL of [`ProviderEvent`], so a real session can be recorded and replayed.

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use neosh_proto::{
    BlockStartKind, DriverKind, InstanceConfig, ProviderEvent, StopReason, ToolCallId, TurnRequest,
    Usage,
};
use tokio_util::sync::CancellationToken;

use crate::{Provider, ProviderStream};

#[derive(Debug, Default)]
pub struct MockProvider {
    /// Scripts are popped in order, one per turn, so a fixture can drive a multi-turn tool loop.
    scripts: Mutex<Vec<Vec<ProviderEvent>>>,
    /// Every request the host made, for assertions.
    pub seen: Arc<Mutex<Vec<TurnRequest>>>,
}

impl MockProvider {
    pub fn new(scripts: Vec<Vec<ProviderEvent>>) -> Self {
        // Reversed so `pop` yields them in the order given.
        Self { scripts: Mutex::new(scripts.into_iter().rev().collect()), seen: Arc::default() }
    }

    /// A single assistant turn that streams `chunks` as text and ends cleanly.
    pub fn text_turn(chunks: &[&str]) -> Vec<ProviderEvent> {
        let mut v = vec![
            ProviderEvent::MessageStart { model: Some("mock".into()), usage: Usage::default() },
            ProviderEvent::BlockStart { index: 0, block: BlockStartKind::Text },
        ];
        v.extend(
            chunks
                .iter()
                .map(|c| ProviderEvent::TextDelta { index: 0, text: (*c).to_string() }),
        );
        v.push(ProviderEvent::BlockStop { index: 0 });
        v.push(ProviderEvent::MessageDelta {
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage { output_tokens: chunks.len() as u64, ..Default::default() },
        });
        v.push(ProviderEvent::MessageStop);
        v
    }

    /// A turn that calls one tool. `input` is split into two deltas so the consumer is forced to
    /// accumulate rather than parse the first fragment.
    pub fn tool_turn(id: &str, name: &str, input: &str) -> Vec<ProviderEvent> {
        let (a, b) = input.split_at(input.len() / 2);
        vec![
            ProviderEvent::MessageStart { model: Some("mock".into()), usage: Usage::default() },
            ProviderEvent::BlockStart {
                index: 0,
                block: BlockStartKind::ToolUse {
                    id: ToolCallId(id.into()),
                    name: name.into(),
                },
            },
            ProviderEvent::ToolInputDelta { index: 0, partial_json: a.into() },
            ProviderEvent::ToolInputDelta { index: 0, partial_json: b.into() },
            ProviderEvent::BlockStop { index: 0 },
            ProviderEvent::MessageDelta {
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage::default(),
            },
            ProviderEvent::MessageStop,
        ]
    }

    /// Load newline-delimited [`ProviderEvent`] JSON as one turn.
    pub fn from_jsonl(text: &str) -> Result<Vec<ProviderEvent>, serde_json::Error> {
        text.lines().filter(|l| !l.trim().is_empty()).map(serde_json::from_str).collect()
    }
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    fn driver(&self) -> DriverKind {
        DriverKind::from("mock")
    }

    fn stream(
        &self,
        _instance: &InstanceConfig,
        request: TurnRequest,
        cancel: CancellationToken,
    ) -> ProviderStream {
        self.seen.lock().expect("mock lock poisoned").push(request);
        let script = self
            .scripts
            .lock()
            .expect("mock lock poisoned")
            .pop()
            .unwrap_or_else(|| Self::text_turn(&["(mock provider has no script left)"]));

        // Honour cancellation the same way a real driver does, so tests exercise the real path.
        Box::pin(futures::stream::iter(script).take_while(move |_| {
            let cancelled = cancel.is_cancelled();
            async move { !cancelled }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use neosh_proto::{AuthRef, InstanceId, ModelSelection};

    fn inst() -> InstanceConfig {
        InstanceConfig {
            id: InstanceId::from("mock"),
            driver: DriverKind::from("mock"),
            display_name: "Mock".into(),
            base_url: None,
            brand: None,
            auth: AuthRef::None,
            models: vec![],
            extra_headers: vec![],
        }
    }

    fn req() -> TurnRequest {
        TurnRequest {
            cwd: std::path::PathBuf::new(),
            selection: ModelSelection {
                instance: InstanceId::from("mock"),
                model: "mock".into(),
                options: vec![],
            },
            system: None,
            messages: vec![],
            tools: vec![],
            max_output_tokens: None,
        }
    }

    #[tokio::test]
    async fn scripts_replay_in_order() {
        let p = MockProvider::new(vec![
            MockProvider::text_turn(&["first"]),
            MockProvider::text_turn(&["second"]),
        ]);
        for expected in ["first", "second"] {
            let got: Vec<_> = p.stream(&inst(), req(), CancellationToken::new()).collect().await;
            assert!(got.iter().any(|e| matches!(
                e, ProviderEvent::TextDelta { text, .. } if text == expected
            )));
        }
    }

    #[tokio::test]
    async fn a_cancelled_token_yields_nothing() {
        let p = MockProvider::new(vec![MockProvider::text_turn(&["hi"])]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let got: Vec<_> = p.stream(&inst(), req(), cancel).collect().await;
        assert!(got.is_empty());
    }

    #[test]
    fn tool_turn_splits_input_so_consumers_must_accumulate() {
        let ev = MockProvider::tool_turn("t1", "read_file", r#"{"path":"a.txt"}"#);
        let deltas: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ToolInputDelta { partial_json, .. } => Some(partial_json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 2);
        assert!(serde_json::from_str::<serde_json::Value>(deltas[0]).is_err());
        let joined: String = deltas.concat();
        assert_eq!(joined, r#"{"path":"a.txt"}"#);
    }

    #[test]
    fn jsonl_fixtures_round_trip() {
        let turn = MockProvider::text_turn(&["a", "b"]);
        let jsonl: String = turn
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(MockProvider::from_jsonl(&jsonl).unwrap(), turn);
    }
}
