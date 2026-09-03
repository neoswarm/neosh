//! Google Gemini via `streamGenerateContent`.
//!
//! Gemini's shape diverges more than the OpenAI-compatible family: roles are `user`/`model`, tool
//! results are `functionResponse` parts rather than messages, and reasoning arrives as ordinary
//! text parts flagged `thought: true`. Everything is normalized into [`ProviderEvent`] by
//! [`crate::sse::gemini`] so the agent loop sees one shape.

use std::collections::HashMap;

use neosh_proto::{
    ContentBlock, DriverKind, InstanceConfig, Message, ModelInfo, ProviderEvent, Role,
    ToolCallId, ToolDef, TurnRequest,
};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::drivers::http;
use crate::{Provider, ProviderError, ProviderStream, resolve_auth, sse};

const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

/// A tool's parameters, in the only schema dialect Gemini will accept.
///
/// `functionDeclarations[].parameters` is an **OpenAPI 3.0 `Schema`**, not JSON Schema, and the
/// endpoint rejects the whole request over a field it does not know rather than ignoring it:
/// `Invalid JSON payload received. Unknown name "additionalProperties" at
/// 'tools[0].function_declarations[0].parameters'`. That is a provider that cannot run a single
/// turn, not a tool that degrades — and it needs no unusual setup to hit, because `read_file`, the
/// built-in, ends its schema with `additionalProperties: false`.
///
/// So the wire shape is built by **allowing** the fields Gemini documents rather than by stripping
/// the ones we have watched it reject. The schemas arriving here are not ours: a plugin's tool and
/// an MCP server's tool land in this field too, and a deny-list is a list somebody has to extend
/// the first time one of them uses `$ref`, `propertyNames` or `patternProperties` — where the cost
/// of having forgotten is the 400 above rather than a slightly loose parameter.
///
/// Dropping a constraint loosens what the model may send, which the tool validates on arrival
/// anyway. Keeping one fails the request outright. Only one direction of that trade is recoverable.
fn schema(v: &Value) -> Value {
    // JSON Schema's boolean form (`true`/`false` in place of a schema object) has no spelling here
    // at all, so it becomes the empty schema rather than a `parameters` of `true`.
    let Some(obj) = v.as_object() else { return json!({}) };
    let mut out = serde_json::Map::new();
    for (k, val) in obj {
        match k.as_str() {
            // Taken as written.
            "description" | "title" | "format" | "pattern" | "example" | "default" | "nullable"
            | "enum" | "required" | "minimum" | "maximum" | "minLength" | "maxLength"
            | "minItems" | "maxItems" | "minProperties" | "maxProperties" | "propertyOrdering" => {
                out.insert(k.clone(), val.clone());
            }
            // `type` is an enum on Gemini's side, so it is spelled in the enum's own case. A JSON
            // Schema *union* — `["string", "null"]`, which is how most generators write an optional
            // field — is not a value that enum has; the nullable half of it is a separate field
            // here, and the rest of the union is information this dialect cannot carry.
            "type" => match val {
                Value::String(t) => {
                    out.insert("type".into(), json!(t.to_uppercase()));
                }
                Value::Array(types) => {
                    if types.iter().any(|t| t.as_str() == Some("null")) {
                        out.insert("nullable".into(), json!(true));
                    }
                    if let Some(t) =
                        types.iter().filter_map(Value::as_str).find(|t| *t != "null")
                    {
                        out.insert("type".into(), json!(t.to_uppercase()));
                    }
                }
                _ => {}
            },
            "properties" => {
                let props: serde_json::Map<_, _> = val
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(name, p)| (name.clone(), schema(p)))
                    .collect();
                out.insert("properties".into(), props.into());
            }
            "items" => {
                out.insert("items".into(), schema(val));
            }
            "anyOf" => {
                let any: Vec<Value> =
                    val.as_array().into_iter().flatten().map(schema).collect();
                out.insert("anyOf".into(), any.into());
            }
            // `const` is JSON Schema's single-value enum, and an enum is exactly how Gemini spells
            // it — so this one constraint survives translation instead of being dropped.
            "const" => {
                out.insert("enum".into(), json!([val]));
            }
            // Everything else. `$schema`, `$ref`, `$defs`, `definitions`, `allOf`, `oneOf`, `not`,
            // `additionalProperties`, `exclusiveMinimum`, `exclusiveMaximum`, `multipleOf`,
            // `patternProperties`, `propertyNames`, `uniqueItems`, `additionalItems` and
            // `if`/`then`/`else` have no field on Gemini's `Schema`, and a keyword invented after
            // this was written lands here too rather than in a 400.
            _ => {}
        }
    }
    out.into()
}

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
        // Gemini keys a tool result by the **function's name**, not by the id of the call it
        // answers, so the name has to be carried forward from the `functionCall` that asked for it.
        // Sending a literal `"tool"` — which is what this did — tells the model that a function it
        // never called has returned, and the reply it writes is about nothing that happened. A
        // parallel call to the same tool still cannot be disambiguated, because the wire format has
        // no id to disambiguate it with; naming the function is as close as the shape allows.
        let names: HashMap<&ToolCallId, &str> = msgs
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, .. } => Some((id, name.as_str())),
                _ => None,
            })
            .collect();
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
                        ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                            Some(json!({
                                "functionResponse": {
                                    "name": names.get(tool_use_id).copied().unwrap_or("tool"),
                                    // A `functionResponse` has no error channel, so a failure that
                                    // is not said in the payload is a stack trace the model reads
                                    // as the answer.
                                    "response": if *is_error {
                                        json!({"error": content})
                                    } else {
                                        json!({"content": content})
                                    },
                                }
                            }))
                        }
                        ContentBlock::Image { path, media_type } => Some(json!({
                            "inlineData": {
                                "mimeType": media_type,
                                "data": crate::image::base64_at(path)?,
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
            "functionDeclarations": tools.iter().map(|t| {
                let mut decl = json!({"name": t.name, "description": t.description});
                let params = schema(&t.input_schema);
                // A tool that takes nothing omits `parameters` rather than sending an object with
                // no properties in it — which is what Google's guidance says, and what a schema of
                // `{"type": "object"}` becomes once there is nothing else in it.
                let has_params = params
                    .get("properties")
                    .and_then(Value::as_object)
                    .is_some_and(|p| !p.is_empty());
                if has_params {
                    decl["parameters"] = params;
                }
                decl
            }).collect::<Vec<_>>()
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
        let mut thinking = json!({"includeThoughts": true});
        // Two spellings of one idea, because the API changed shape between generations: the 3 line
        // takes a named rung, the 2.5 line takes a token budget where the only number worth a
        // control is zero. Whichever knob the model advertised is the one that arrives here; a
        // model that advertised neither sends neither, and gets the endpoint's own default.
        if let Some(level) = request.selection.option_str("thinking_level") {
            thinking["thinkingLevel"] = json!(level);
        }
        if let Some(on) = request.selection.option_flag("thinking") {
            // `-1` is "decide for yourself", which is the default and the only honest value for
            // "on": any fixed budget we chose here would be a guess about a turn we cannot see.
            thinking["thinkingBudget"] = json!(if on { -1 } else { 0 });
        }
        gen_cfg["thinkingConfig"] = thinking;
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
                        // Keyed off the id, which is the one thing `models.list` is certain to
                        // return and the one thing Google's own knob follows from: the 3 line takes
                        // a thinking *level*, the 2.5 line a *budget*. Done here as well as in the
                        // catalogue so a model released this morning arrives with its ladder rather
                        // than waiting for a seed entry to be written for it.
                        let descriptors = crate::catalog::gemini_knobs(id);
                        info.capabilities = neosh_proto::ModelCapabilities {
                            tools: true,
                            vision: true,
                            streaming: true,
                            thinking: true,
                            prompt_caching: false,
                            option_descriptors: descriptors,
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
            resume: None,
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

    fn tool(input_schema: Value) -> ToolDef {
        ToolDef {
            name: "read_file".into(),
            description: "d".into(),
            input_schema,
            source: neosh_proto::ToolSource::Builtin,
        }
    }

    fn params(input_schema: Value) -> Value {
        let mut r = req(vec![]);
        r.tools = vec![tool(input_schema)];
        GoogleProvider::body(&r)["tools"][0]["functionDeclarations"][0].clone()
    }

    #[test]
    fn a_schema_gemini_does_not_know_a_field_of_is_not_sent_verbatim() {
        // The reported bug, end to end: `read_file`'s own schema, which ends in
        // `additionalProperties: false`, used to reach the wire and 400 the whole turn.
        let d = params(json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "where"}},
            "required": ["path"],
            "additionalProperties": false,
            "$schema": "https://json-schema.org/draft/2020-12/schema",
        }));
        let p = &d["parameters"];
        assert!(p.get("additionalProperties").is_none(), "the field that 400s the request");
        assert!(p.get("$schema").is_none());
        assert_eq!(p["type"], "OBJECT");
        assert_eq!(p["properties"]["path"]["type"], "STRING");
        assert_eq!(p["properties"]["path"]["description"], "where");
        assert_eq!(p["required"][0], "path");
    }

    #[test]
    fn a_keyword_nobody_here_has_heard_of_is_dropped_rather_than_forwarded() {
        // The allow-list is the point: an MCP server's schema is not ours to predict, and the cost
        // of guessing wrong has to be a loose parameter rather than a provider that cannot answer.
        let p = &params(json!({
            "type": "object",
            "properties": {
                "a": {"type": "string", "patternProperties": {"^x": {}}, "propertyNames": {}},
            },
            "unevaluatedProperties": false,
            "$defs": {"x": {"type": "string"}},
            "allOf": [{"type": "object"}],
        }))["parameters"];
        for gone in ["unevaluatedProperties", "$defs", "allOf"] {
            assert!(p.get(gone).is_none(), "{gone} reached the wire");
        }
        assert_eq!(p["properties"]["a"].as_object().unwrap().len(), 1, "only `type` survives");
    }

    #[test]
    fn an_optional_field_written_as_a_union_becomes_a_nullable_one() {
        // `["string", "null"]` is how most generators spell optional, and `type` is an enum here —
        // so the array is not a value it has, and the request fails on the field before this one
        // even gets looked at.
        let p = &params(json!({
            "type": "object",
            "properties": {"note": {"type": ["string", "null"]}},
        }))["parameters"];
        assert_eq!(p["properties"]["note"]["type"], "STRING");
        assert_eq!(p["properties"]["note"]["nullable"], true);
    }

    #[test]
    fn nesting_is_walked_all_the_way_down() {
        let p = &params(json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"line": {"type": "integer", "exclusiveMinimum": 0}},
                        "additionalProperties": false,
                    },
                },
            },
        }))["parameters"];
        let item = &p["properties"]["edits"]["items"];
        assert_eq!(item["type"], "OBJECT");
        assert!(item.get("additionalProperties").is_none());
        assert_eq!(item["properties"]["line"]["type"], "INTEGER");
        assert!(item["properties"]["line"].get("exclusiveMinimum").is_none());
    }

    #[test]
    fn a_single_valued_constraint_survives_as_the_enum_it_is() {
        let p = &params(json!({
            "type": "object",
            "properties": {"mode": {"const": "read"}},
        }))["parameters"];
        assert_eq!(p["properties"]["mode"]["enum"], json!(["read"]));
    }

    #[test]
    fn a_tool_that_takes_nothing_sends_no_parameters_at_all() {
        // Rather than an OBJECT with an empty `properties`, which is what a bare `{"type":
        // "object"}` would otherwise become.
        let d = params(json!({"type": "object"}));
        assert!(d.get("parameters").is_none());
        assert_eq!(d["name"], "read_file");
    }

    #[test]
    fn a_tool_result_is_named_by_the_call_it_answers() {
        // Gemini keys a response by function name. A literal `"tool"` is a name nothing declared,
        // so the model was being told that a function it never called had returned.
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
        assert_eq!(b["contents"][1]["parts"][0]["functionResponse"]["name"], "read_file");
    }

    #[test]
    fn a_failed_call_says_so_in_the_payload() {
        // There is no error flag on a `functionResponse`, so a failure not said here is a stack
        // trace the model reads as the answer.
        let b = GoogleProvider::body(&req(vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: ToolCallId("c".into()),
                content: "no such file".into(),
                is_error: true,
            }],
        }]));
        let r = &b["contents"][0]["parts"][0]["functionResponse"]["response"];
        assert_eq!(r["error"], "no such file");
        assert!(r.get("content").is_none());
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
