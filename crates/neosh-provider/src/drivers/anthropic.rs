//! Anthropic Messages API over HTTPS.
//!
//! Three things here are easy to get wrong and expensive when you do:
//!
//! * **`budget_tokens` is gone.** Sending it to the current generation is a 400, not a warning.
//!   Reasoning depth is `output_config.effort` and thinking is `{"type":"adaptive"}`.
//! * **Assistant prefill is rejected.** Nothing here may append a trailing assistant message to
//!   steer format.
//! * **A refusal is HTTP 200.** It arrives as `stop_reason: "refusal"`, so a client that only
//!   inspects transport failures shows an empty turn and looks like a hang.

use neosh_proto::{
    ContentBlock, DriverKind, InstanceConfig, Message, ModelId, ModelInfo, ProviderEvent, Role,
    ToolDef, TurnRequest,
};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::drivers::http;
use crate::{Provider, ProviderError, ProviderStream, catalog, resolve_auth, sse};

const API_VERSION: &str = "2023-06-01";
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";
const DEFAULT_MAX_TOKENS: u64 = 64_000;

#[derive(Debug, Default)]
pub struct AnthropicProvider;

impl AnthropicProvider {
    fn base(instance: &InstanceConfig) -> String {
        instance
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com".into())
            .trim_end_matches('/')
            .to_string()
    }

    fn content_block(b: &ContentBlock) -> Option<Value> {
        Some(match b {
            ContentBlock::Text { text } => json!({"type": "text", "text": text}),
            ContentBlock::Thinking { text, signature } => {
                // Thinking blocks must be replayed byte-identically or the turn is rejected. One
                // without a signature came from a different model and is dropped rather than sent.
                let sig = signature.as_ref()?;
                json!({"type": "thinking", "thinking": text, "signature": sig})
            }
            ContentBlock::ToolUse { id, name, input } => {
                json!({"type": "tool_use", "id": id.0, "name": name, "input": input})
            }
            ContentBlock::ToolResult { tool_use_id, content, is_error } => json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id.0,
                "content": content,
                "is_error": is_error,
            }),
        })
    }

    fn messages(msgs: &[Message]) -> Vec<Value> {
        msgs.iter()
            .map(|m| {
                json!({
                    "role": match m.role { Role::User => "user", Role::Assistant => "assistant" },
                    "content": m.content.iter().filter_map(Self::content_block).collect::<Vec<_>>(),
                })
            })
            .filter(|m| !m["content"].as_array().is_some_and(Vec::is_empty))
            .collect()
    }

    fn tools(tools: &[ToolDef]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect()
    }

    /// Build the request body.
    ///
    /// Split out from [`Provider::stream`] so the option-to-parameter mapping — the part that
    /// produces 400s when wrong — is unit-testable without a network.
    pub fn body(request: &TurnRequest, model_supports_thinking: bool) -> Value {
        let sel = &request.selection;
        let mut body = json!({
            "model": sel.model.0,
            "max_tokens": request.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            "stream": true,
            "messages": Self::messages(&request.messages),
        });

        if let Some(system) = &request.system {
            // Cache the system prefix: it is the largest stable span in an agent conversation.
            body["system"] = json!([{
                "type": "text",
                "text": system,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(Self::tools(&request.tools));
        }
        if model_supports_thinking {
            // `summarized` rather than the default `omitted`, so a streaming UI can show reasoning
            // instead of a long silence.
            body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
        }
        if let Some(effort) = sel.option_str("effort") {
            body["output_config"] = json!({"effort": effort});
        }
        if sel.option_flag("fast_mode") == Some(true) {
            body["speed"] = json!("fast");
        }
        body
    }

    fn thinking_supported(instance: &InstanceConfig, model: &ModelId) -> bool {
        instance
            .models
            .iter()
            .chain(catalog::anthropic_models().iter())
            .find(|m| &m.id == model)
            .map(|m| m.capabilities.thinking)
            .unwrap_or(true)
    }
}

#[async_trait::async_trait]
impl Provider for AnthropicProvider {
    fn driver(&self) -> DriverKind {
        DriverKind::from("anthropic")
    }

    async fn list_models(&self, instance: &InstanceConfig) -> Result<Vec<ModelInfo>, ProviderError> {
        let Some(key) = resolve_auth(instance)? else {
            return Ok(instance.models.clone());
        };
        let resp = http::client()
            .get(format!("{}/v1/models?limit=100", Self::base(instance)))
            .header("x-api-key", key.expose_secret())
            .header("anthropic-version", API_VERSION)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let v: Value = http::error_for_status(resp)
            .await?
            .json()
            .await
            .map_err(|e| ProviderError::BadResponse(e.to_string()))?;

        // Discovery gives the authoritative id list; the static catalog supplies the option
        // descriptors and pricing that no endpoint reports.
        let known = catalog::anthropic_models();
        let mut out = Vec::new();
        for m in v.get("data").and_then(Value::as_array).unwrap_or(&vec![]) {
            let Some(id) = m.get("id").and_then(Value::as_str) else { continue };
            let mut info = known.iter().find(|k| k.id.0 == id).cloned().unwrap_or_else(|| {
                // Not in the catalogue: a model that shipped after this build. It gets a name and
                // nothing editorial, which is honest — the alternative is guessing its tier from
                // its id and putting it on the wrong rung of the ladder.
                ModelInfo::undescribed(
                    id,
                    m.get("display_name").and_then(Value::as_str).unwrap_or(id),
                )
            });
            if let Some(ctx) = m.get("max_input_tokens").and_then(Value::as_u64) {
                info.context_window = Some(ctx);
            }
            if let Some(max) = m.get("max_tokens").and_then(Value::as_u64) {
                info.max_output_tokens = Some(max);
            }
            out.push(info);
        }
        if out.is_empty() { Ok(known) } else { Ok(out) }
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
        let extra = instance.extra_headers.clone();
        let thinking = Self::thinking_supported(instance, &request.selection.model);
        let fast = request.selection.option_flag("fast_mode") == Some(true);

        tokio::spawn(async move {
            let key = match resolve_auth(&config) {
                Ok(Some(k)) => k,
                Ok(None) => {
                    let _ = tx
                        .send(ProviderEvent::Error {
                            message: "the Anthropic driver requires an API key".into(),
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

            let mut req = http::client()
                .post(format!("{base}/v1/messages"))
                .header("x-api-key", key.expose_secret())
                .header("anthropic-version", API_VERSION)
                .header("content-type", "application/json");
            if fast {
                req = req.header("anthropic-beta", FAST_MODE_BETA);
            }
            for (k, v) in &extra {
                req = req.header(k, v);
            }

            let resp = match req.json(&Self::body(&request, thinking)).send().await {
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

            let mut data = http::sse_data(resp, cancel);
            while let Some(item) = data.recv().await {
                match item {
                    Ok(payload) => {
                        let Ok(v) = serde_json::from_str::<Value>(&payload) else { continue };
                        if let Some(ev) = sse::anthropic(&v)
                            && tx.send(ev).await.is_err()
                        {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(ProviderEvent::Error {
                                message: e.to_string(),
                                retryable: true,
                            })
                            .await;
                        return;
                    }
                }
            }
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{InstanceId, ModelSelection, OptionSelection, ProviderOptionValue, ToolCallId, ToolSource};

    fn request(options: Vec<OptionSelection>) -> TurnRequest {
        TurnRequest {
            cwd: std::path::PathBuf::new(),
            selection: ModelSelection {
                instance: InstanceId::from("anthropic"),
                model: ModelId::from("claude-opus-5"),
                options,
            },
            system: Some("be concise".into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            tools: vec![],
            max_output_tokens: None,
        }
    }

    fn text(id: &str) -> OptionSelection {
        OptionSelection { id: id.into(), value: ProviderOptionValue::Text("xhigh".into()) }
    }

    #[test]
    fn never_sends_budget_tokens() {
        let b = AnthropicProvider::body(&request(vec![text("effort")]), true);
        assert!(b["thinking"]["budget_tokens"].is_null());
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert_eq!(b.to_string().contains("budget_tokens"), false);
    }

    #[test]
    fn effort_maps_into_output_config_not_top_level() {
        let b = AnthropicProvider::body(&request(vec![text("effort")]), true);
        assert_eq!(b["output_config"]["effort"], "xhigh");
        assert!(b["effort"].is_null(), "effort is not a top-level parameter");
    }

    #[test]
    fn fast_mode_sets_speed_only_when_enabled() {
        let on = AnthropicProvider::body(
            &request(vec![OptionSelection {
                id: "fast_mode".into(),
                value: ProviderOptionValue::Flag(true),
            }]),
            true,
        );
        assert_eq!(on["speed"], "fast");
        assert!(AnthropicProvider::body(&request(vec![]), true)["speed"].is_null());
    }

    #[test]
    fn thinking_is_omitted_for_models_without_it() {
        assert!(AnthropicProvider::body(&request(vec![]), false)["thinking"].is_null());
    }

    #[test]
    fn system_prompt_is_cached() {
        let b = AnthropicProvider::body(&request(vec![]), true);
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn thinking_blocks_without_a_signature_are_dropped_not_sent() {
        let mut r = request(vec![]);
        r.messages.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking { text: "hm".into(), signature: None },
                ContentBlock::Text { text: "answer".into() },
            ],
        });
        let b = AnthropicProvider::body(&r, true);
        let blocks = b["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "unsigned thinking must not be replayed");
        assert_eq!(blocks[0]["type"], "text");
    }

    #[test]
    fn tool_use_and_results_round_trip_into_wire_shape() {
        let mut r = request(vec![]);
        r.tools = vec![ToolDef {
            name: "read_file".into(),
            description: "read".into(),
            input_schema: json!({"type": "object"}),
            source: ToolSource::Builtin,
        }];
        r.messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: ToolCallId("t1".into()),
                name: "read_file".into(),
                input: json!({"path": "a.txt"}),
            }],
        });
        r.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: ToolCallId("t1".into()),
                content: "contents".into(),
                is_error: false,
            }],
        });
        let b = AnthropicProvider::body(&r, true);
        assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(b["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(b["messages"][2]["content"][0]["tool_use_id"], "t1");
    }

    #[test]
    fn streaming_is_always_on() {
        assert_eq!(AnthropicProvider::body(&request(vec![]), true)["stream"], true);
    }
}
