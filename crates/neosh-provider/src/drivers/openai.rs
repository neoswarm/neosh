//! One driver for every endpoint that speaks the OpenAI chat-completions shape.
//!
//! OpenAI, OpenRouter, Groq, DeepSeek, xAI, Mistral, Together, Fireworks, Cerebras, Ollama,
//! llama.cpp, LM Studio and vLLM all serve this format. They differ only in base URL and
//! credentials, which are [`InstanceConfig`] fields — so supporting another one of them is a config
//! entry, never code. This is the bulk of "support every model".

use neosh_proto::{
    ContentBlock, DriverKind, InstanceConfig, Message, ModelInfo, ProviderEvent, Role,
    ToolDef, TurnRequest,
};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::catalog;
use crate::drivers::http;
use crate::{Provider, ProviderError, ProviderStream, resolve_auth, sse};

#[derive(Debug, Default)]
pub struct OpenAiCompatProvider;

impl OpenAiCompatProvider {
    fn base(instance: &InstanceConfig) -> Result<String, ProviderError> {
        instance
            .base_url
            .clone()
            .map(|u| u.trim_end_matches('/').to_string())
            .ok_or_else(|| {
                ProviderError::Unsupported(format!(
                    "instance {} has no base_url; OpenAI-compatible endpoints require one",
                    instance.id
                ))
            })
    }

    /// Flatten our block model into OpenAI's message model.
    ///
    /// The shapes do not correspond one-to-one: tool results are their own messages rather than
    /// blocks inside a user message, and tool arguments are a JSON *string* rather than an object.
    /// Getting the second one wrong is accepted by permissive local servers and rejected by
    /// OpenAI, which makes it a bug that only shows up in production.
    pub fn messages(system: Option<&str>, msgs: &[Message]) -> Vec<Value> {
        let mut out = Vec::new();
        if let Some(s) = system {
            out.push(json!({"role": "system", "content": s}));
        }
        for m in msgs {
            match m.role {
                Role::User => {
                    let text: String = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    // A picture only fits in the *array* form of `content`. A string and a
                    // one-element array are the same message to the server, so the array is only
                    // reached for when there is something in it that a string cannot hold.
                    let images: Vec<Value> = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Image { path, media_type } => Some(json!({
                                "type": "image_url",
                                "image_url": { "url": crate::image::data_url(path, media_type)? },
                            })),
                            _ => None,
                        })
                        .collect();
                    if !images.is_empty() {
                        let mut parts = Vec::new();
                        if !text.is_empty() {
                            parts.push(json!({"type": "text", "text": text}));
                        }
                        parts.extend(images);
                        out.push(json!({"role": "user", "content": parts}));
                    } else if !text.is_empty() {
                        out.push(json!({"role": "user", "content": text}));
                    }
                    for b in &m.content {
                        if let ContentBlock::ToolResult { tool_use_id, content, .. } = b {
                            out.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id.0,
                                "content": content,
                            }));
                        }
                    }
                }
                Role::Assistant => {
                    let text: String = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    let calls: Vec<Value> = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolUse { id, name, input } => Some(json!({
                                "id": id.0,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    // Arguments are a string here, not an object.
                                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                                },
                            })),
                            _ => None,
                        })
                        .collect();
                    if text.is_empty() && calls.is_empty() {
                        continue;
                    }
                    let mut msg = json!({"role": "assistant"});
                    msg["content"] = if text.is_empty() { Value::Null } else { json!(text) };
                    if !calls.is_empty() {
                        msg["tool_calls"] = json!(calls);
                    }
                    out.push(msg);
                }
            }
        }
        out
    }

    fn tools(tools: &[ToolDef]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                })
            })
            .collect()
    }

    /// The knobs an entry in `/models` says it takes.
    ///
    /// # Why an aggregator can answer this and a vendor cannot
    ///
    /// `GET /v1/models` is barely specified: OpenAI returns four fields and none of them is about
    /// parameters. An aggregator has to do better, because it fronts hundreds of models from dozens
    /// of vendors and nobody could otherwise tell which of them reason — so OpenRouter puts
    /// `supported_parameters` on every row, and that list is the only place in this entire family of
    /// endpoints where "does this model take a reasoning effort" is a question with a real answer.
    ///
    /// So this is discovery where discovery exists, and [`catalog::seed_models`] is the written-down
    /// answer for the endpoints that will not say. Four hundred OpenRouter models arrive with the
    /// right ladder each and no catalogue entry between them.
    ///
    /// Deliberately narrow: only parameters this driver actually sends are turned into knobs. A
    /// picker offering a control that goes nowhere is worse than one that is short.
    fn advertised_knobs(m: &Value) -> Vec<neosh_proto::ProviderOptionDescriptor> {
        let Some(params) = m.get("supported_parameters").and_then(Value::as_array) else {
            return Vec::new();
        };
        let takes = |name: &str| params.iter().any(|p| p.as_str() == Some(name));
        let mut out = Vec::new();
        if takes("reasoning") || takes("reasoning_effort") {
            // Three rungs, because that is what the aggregator normalises every vendor's ladder
            // onto before passing it on. A fourth invented here would be translated into one of
            // these anyway, or rejected.
            out.push(catalog::effort_levels(&["low", "medium", "high"], "medium"));
        }
        if takes("verbosity") {
            out.push(catalog::verbosity_levels());
        }
        out
    }

    pub fn body(request: &TurnRequest) -> Value {
        let mut body = json!({
            "model": request.selection.model.0,
            "stream": true,
            // Without this, most endpoints report no usage at all for a streamed turn, and the
            // token budget display silently reads zero.
            "stream_options": {"include_usage": true},
            "messages": Self::messages(request.system.as_deref(), &request.messages),
        });
        if !request.tools.is_empty() {
            body["tools"] = json!(Self::tools(&request.tools));
        }
        if let Some(max) = request.max_output_tokens {
            body["max_completion_tokens"] = json!(max);
        }
        if let Some(effort) = request.selection.option_str("effort") {
            body["reasoning_effort"] = json!(effort);
        }
        // How long the answer is, which is a separate question from how hard it thought — and the
        // one people actually reach for. Sent only when a model advertised it, so an endpoint that
        // has never heard of the parameter never sees it.
        if let Some(verbosity) = request.selection.option_str("verbosity") {
            body["verbosity"] = json!(verbosity);
        }
        body
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiCompatProvider {
    fn driver(&self) -> DriverKind {
        DriverKind::from("openai-compat")
    }

    /// Every endpoint in this family answers `GET /models`, which is why the catalog ships base
    /// URLs instead of model lists that would be stale within weeks.
    async fn list_models(&self, instance: &InstanceConfig) -> Result<Vec<ModelInfo>, ProviderError> {
        let base = Self::base(instance)?;
        let mut req = http::client().get(format!("{base}/models"));
        if let Some(key) = resolve_auth(instance).unwrap_or(None) {
            req = req.bearer_auth(key.expose_secret());
        }
        let resp = req.send().await.map_err(|e| ProviderError::Transport(e.to_string()))?;
        let v: Value = http::error_for_status(resp)
            .await?
            .json()
            .await
            .map_err(|e| ProviderError::BadResponse(e.to_string()))?;

        let mut out: Vec<ModelInfo> = v
            .get("data")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m.get("id").and_then(Value::as_str)?;
                        let mut info = ModelInfo::undescribed(
                            id,
                            m.get("name").and_then(Value::as_str).unwrap_or(id),
                        );
                        info.context_window = m
                            .get("context_length")
                            .or(m.get("context_window"))
                            .and_then(Value::as_u64);
                        let descriptors = Self::advertised_knobs(m);
                        info.capabilities = neosh_proto::ModelCapabilities {
                            tools: true,
                            vision: false,
                            streaming: true,
                            thinking: descriptors.iter().any(|d| d.id() == "effort"),
                            prompt_caching: false,
                            option_descriptors: descriptors,
                        };
                        Some(info)
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        if out.is_empty() { Ok(instance.models.clone()) } else { Ok(out) }
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

        tokio::spawn(async move {
            let base = match base {
                Ok(b) => b,
                Err(e) => {
                    let _ = tx
                        .send(ProviderEvent::Error { message: e.to_string(), retryable: false })
                        .await;
                    return;
                }
            };

            let mut req = http::client()
                .post(format!("{base}/chat/completions"))
                .header("content-type", "application/json");
            match resolve_auth(&config) {
                Ok(Some(key)) => req = req.bearer_auth(key.expose_secret()),
                Ok(None) => {} // local endpoints legitimately need no credentials
                Err(e) => {
                    let _ = tx
                        .send(ProviderEvent::Error { message: e.to_string(), retryable: false })
                        .await;
                    return;
                }
            }
            for (k, v) in &extra {
                req = req.header(k, v);
            }

            let resp = match req.json(&Self::body(&request)).send().await {
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
                        for ev in sse::openai(&v, &mut state) {
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
            // This shape has no terminal event of its own; synthesize one so the agent loop's
            // state machine always sees a well-formed end of turn.
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
    use neosh_proto::{InstanceId, ModelSelection, OptionSelection, ProviderOptionValue, ToolCallId};

    fn msg(role: Role, content: Vec<ContentBlock>) -> Message {
        Message { role, content }
    }

    #[test]
    fn tool_arguments_are_serialized_as_a_string_not_an_object() {
        let out = OpenAiCompatProvider::messages(None, &[msg(Role::Assistant, vec![
            ContentBlock::ToolUse {
                id: ToolCallId("c1".into()),
                name: "read_file".into(),
                input: json!({"path": "a.txt"}),
            },
        ])]);
        let args = &out[0]["tool_calls"][0]["function"]["arguments"];
        assert!(args.is_string(), "OpenAI rejects an object here");
        assert_eq!(args.as_str().unwrap(), r#"{"path":"a.txt"}"#);
    }

    #[test]
    fn tool_results_become_their_own_messages() {
        let out = OpenAiCompatProvider::messages(None, &[msg(Role::User, vec![
            ContentBlock::ToolResult {
                tool_use_id: ToolCallId("c1".into()),
                content: "hello".into(),
                is_error: false,
            },
        ])]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "c1");
    }

    #[test]
    fn system_prompt_leads_the_message_list() {
        let out = OpenAiCompatProvider::messages(Some("be brief"), &[msg(Role::User, vec![
            ContentBlock::Text { text: "hi".into() },
        ])]);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[1]["role"], "user");
    }

    #[test]
    fn thinking_blocks_are_not_replayed() {
        let out = OpenAiCompatProvider::messages(None, &[msg(Role::Assistant, vec![
            ContentBlock::Thinking { text: "hmm".into(), signature: Some("s".into()) },
            ContentBlock::Text { text: "answer".into() },
        ])]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"], "answer");
        assert!(!out[0].to_string().contains("hmm"));
    }

    #[test]
    fn usage_reporting_is_requested_explicitly() {
        let r = TurnRequest {
            resume: None,
            conversation: neosh_proto::SessionId::from("test"),
            cwd: std::path::PathBuf::new(),
            selection: ModelSelection {
                instance: InstanceId::from("openai"),
                model: neosh_proto::ModelId::from("some-model"),
                options: vec![OptionSelection {
                    id: "effort".into(),
                    value: ProviderOptionValue::Text("high".into()),
                }],
            },
            system: None,
            messages: vec![],
            tools: vec![],
            max_output_tokens: Some(1000),
        };
        let b = OpenAiCompatProvider::body(&r);
        assert_eq!(b["stream_options"]["include_usage"], true);
        assert_eq!(b["reasoning_effort"], "high");
        assert_eq!(b["max_completion_tokens"], 1000);
    }

    #[test]
    fn a_missing_base_url_is_an_error_not_a_silent_default() {
        let inst = InstanceConfig {
            id: InstanceId::from("x"),
            driver: DriverKind::from("openai-compat"),
            display_name: "x".into(),
            base_url: None,
            brand: None,
            auth: neosh_proto::AuthRef::None,
            models: vec![],
            extra_headers: vec![],
        };
        assert!(OpenAiCompatProvider::base(&inst).is_err());
    }
}
