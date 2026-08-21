//! Google Gemini via `streamGenerateContent`.
//!
//! Gemini's shape diverges more than the OpenAI-compatible family: roles are `user`/`model`, tool
//! results are `functionResponse` parts rather than messages, and reasoning arrives as ordinary
//! text parts flagged `thought: true`. Everything is normalized into [`ProviderEvent`] by
//! [`crate::sse::gemini`] so the agent loop sees one shape.

use neosh_proto::{
    ContentBlock, DriverKind, InstanceConfig, Message, ModelInfo, ProviderEvent, Role,
    ToolDef, TurnRequest,
};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::drivers::http;
use crate::{Provider, ProviderError, ProviderStream, resolve_auth, sse};

const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

#[derive(Debug, Default)]
pub struct GoogleProvider;

impl GoogleProvider {
    fn base(instance: &InstanceConfig) -> String {
        instance
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE.into())
            .trim_end_matches('/')
            .to_string()
    }

    pub fn contents(msgs: &[Message]) -> Vec<Value> {
        msgs.iter()
            .filter_map(|m| {
                let parts: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(json!({"text": text})),
                        ContentBlock::ToolUse { name, input, .. } => {
                            Some(json!({"functionCall": {"name": name, "args": input}}))
                        }
                        ContentBlock::ToolResult { content, .. } => Some(json!({
                            "functionResponse": {
                                // Gemini keys responses by function name, not call id, so a
                                // parallel call to the same tool cannot be disambiguated here.
                                "name": "tool",
                                "response": {"content": content},
                            }
                        })),
                        // Reasoning is never replayed to Gemini.
                        ContentBlock::Thinking { .. } => None,
                    })
                    .collect();
                if parts.is_empty() {
                    return None;
                }
                Some(json!({
                    "role": match m.role { Role::User => "user", Role::Assistant => "model" },
                    "parts": parts,
                }))
            })
            .collect()
    }

    fn tools(tools: &[ToolDef]) -> Value {
        json!([{
            "functionDeclarations": tools.iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
            })).collect::<Vec<_>>()
        }])
    }

    pub fn body(request: &TurnRequest) -> Value {
        let mut body = json!({ "contents": Self::contents(&request.messages) });
        if let Some(s) = &request.system {
            body["systemInstruction"] = json!({"parts": [{"text": s}]});
        }
        if !request.tools.is_empty() {
            body["tools"] = Self::tools(&request.tools);
        }
        let mut gen_cfg = json!({});
        if let Some(max) = request.max_output_tokens {
            gen_cfg["maxOutputTokens"] = json!(max);
        }
        // Ask for reasoning to be surfaced rather than silently discarded.
        gen_cfg["thinkingConfig"] = json!({"includeThoughts": true});
        body["generationConfig"] = gen_cfg;
        body
    }
}

#[async_trait::async_trait]
impl Provider for GoogleProvider {
    fn driver(&self) -> DriverKind {
        DriverKind::from("google")
    }

    async fn list_models(&self, instance: &InstanceConfig) -> Result<Vec<ModelInfo>, ProviderError> {
        let Some(key) = resolve_auth(instance)? else {
            return Ok(instance.models.clone());
        };
        let resp = http::client()
            .get(format!("{}/models?pageSize=200", Self::base(instance)))
            .header("x-goog-api-key", key.expose_secret())
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let v: Value = http::error_for_status(resp)
            .await?
            .json()
            .await
            .map_err(|e| ProviderError::BadResponse(e.to_string()))?;

        Ok(v.get("models")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter(|m| {
                        // Embedding-only models cannot serve a turn; offering them is a trap.
                        m.get("supportedGenerationMethods")
                            .and_then(Value::as_array)
                            .map(|ms| ms.iter().any(|s| s.as_str() == Some("streamGenerateContent")))
                            .unwrap_or(true)
                    })
                    .filter_map(|m| {
                        let full = m.get("name").and_then(Value::as_str)?;
                        let id = full.strip_prefix("models/").unwrap_or(full);
                        let mut info = ModelInfo::undescribed(
                            id,
                            m.get("displayName").and_then(Value::as_str).unwrap_or(id),
                        );
                        info.context_window = m.get("inputTokenLimit").and_then(Value::as_u64);
                        info.max_output_tokens = m.get("outputTokenLimit").and_then(Value::as_u64);
                        info.capabilities = neosh_proto::ModelCapabilities {
                            tools: true,
                            vision: true,
                            streaming: true,
                            thinking: true,
                            prompt_caching: false,
                            option_descriptors: vec![],
                        };
                        Some(info)
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn stream(
        &self,
        instance: &InstanceConfig,
        request: TurnRequest,
        cancel: CancellationToken,
    ) -> ProviderStream {
        let (tx, rx) = mpsc::channel::<ProviderEvent>(256);
        let base = Self::base(instance);
        let config = instance.clone();
        let model = request.selection.model.0.clone();

        tokio::spawn(async move {
            let key = match resolve_auth(&config) {
                Ok(Some(k)) => k,
                Ok(None) => {
                    let _ = tx
                        .send(ProviderEvent::Error {
                            message: "the Google driver requires GEMINI_API_KEY".into(),
                            retryable: false,
                        })
                        .await;
                    return;
                }
                Err(e) => {
                    let _ = tx
                        .send(ProviderEvent::Error { message: e.to_string(), retryable: false })
                        .await;
                    return;
                }
            };

            let url = format!("{base}/models/{model}:streamGenerateContent?alt=sse");
            let resp = match http::client()
                .post(url)
                // The key goes in a header, not the query string, so it cannot leak into logs.
                .header("x-goog-api-key", key.expose_secret())
                .header("content-type", "application/json")
                .json(&Self::body(&request))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx
                        .send(ProviderEvent::Error {
                            message: e.to_string(),
                            retryable: e.is_timeout() || e.is_connect(),
                        })
                        .await;
                    return;
                }
            };
            let status = resp.status().as_u16();
            let resp = match http::error_for_status(resp).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx
                        .send(ProviderEvent::Error {
                            message: e.to_string(),
                            retryable: http::retryable(status),
                        })
                        .await;
                    return;
                }
            };

            let mut state = sse::OpenAiState::default();
            let mut data = http::sse_data(resp, cancel);
            let mut saw_stop = false;
            while let Some(item) = data.recv().await {
                match item {
                    Ok(payload) => {
                        let Ok(v) = serde_json::from_str::<Value>(&payload) else { continue };
                        for ev in sse::gemini(&v, &mut state) {
                            saw_stop |= matches!(ev, ProviderEvent::MessageDelta { .. });
                            if tx.send(ev).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(ProviderEvent::Error { message: e.to_string(), retryable: true })
                            .await;
                        return;
                    }
                }
            }
            if saw_stop {
                let _ = tx.send(ProviderEvent::MessageStop).await;
            }
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{InstanceId, ModelSelection, ToolCallId};

    fn req(msgs: Vec<Message>) -> TurnRequest {
        TurnRequest {
            conversation: neosh_proto::SessionId::from("test"),
            cwd: std::path::PathBuf::new(),
            selection: ModelSelection {
                instance: InstanceId::from("google"),
                model: neosh_proto::ModelId::from("gemini-3-pro"),
                options: vec![],
            },
            system: Some("be brief".into()),
            messages: msgs,
            tools: vec![],
            max_output_tokens: None,
        }
    }

    #[test]
    fn assistant_role_is_named_model() {
        let b = GoogleProvider::body(&req(vec![
            Message { role: Role::User, content: vec![ContentBlock::Text { text: "hi".into() }] },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "yo".into() }],
            },
        ]));
        assert_eq!(b["contents"][0]["role"], "user");
        assert_eq!(b["contents"][1]["role"], "model");
    }

    #[test]
    fn system_instruction_is_separate_from_contents() {
        let b = GoogleProvider::body(&req(vec![]));
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "be brief");
        assert!(!b["contents"].to_string().contains("be brief"));
    }

    #[test]
    fn reasoning_is_requested_so_it_is_not_silently_dropped() {
        let b = GoogleProvider::body(&req(vec![]));
        assert_eq!(b["generationConfig"]["thinkingConfig"]["includeThoughts"], true);
    }

    #[test]
    fn tool_calls_and_responses_map_to_parts() {
        let b = GoogleProvider::body(&req(vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: ToolCallId("c".into()),
                    name: "read_file".into(),
                    input: json!({"path": "a"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: ToolCallId("c".into()),
                    content: "data".into(),
                    is_error: false,
                }],
            },
        ]));
        assert_eq!(b["contents"][0]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(b["contents"][1]["parts"][0]["functionResponse"]["response"]["content"], "data");
    }

    #[test]
    fn messages_with_only_unmappable_blocks_are_dropped_not_sent_empty() {
        let b = GoogleProvider::body(&req(vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking { text: "x".into(), signature: None }],
        }]));
        assert_eq!(b["contents"].as_array().unwrap().len(), 0);
    }
}
