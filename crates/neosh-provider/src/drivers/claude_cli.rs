//! Drive the `claude` CLI as a provider.
//!
//! # Why this exists
//!
//! It needs no API key. It reuses whatever login the user already has, which means a fresh install
//! of neosh is useful immediately instead of after a trip to a billing console. That is worth a
//! driver on its own.
//!
//! # What it actually is
//!
//! An **agent driver**, not a model driver: the CLI runs its own loop with its own tools, so
//! neosh's tool registry is not in play and `tool.pre` hooks observe rather than gate. The host
//! surfaces that distinction via [`Provider::delegates_agent_loop`] rather than quietly pretending
//! the two are equivalent. Wiring neosh's own tool registry in through `--mcp-config` is the
//! natural next step, and needs no change to this file's shape.
//!
//! # The useful discovery
//!
//! `--output-format stream-json --include-partial-messages` emits the Anthropic Messages SSE
//! payloads *verbatim*, wrapped as `{"type":"stream_event","event":{…}}`. So this driver and the
//! HTTPS driver share one parser ([`crate::sse::anthropic`]) rather than each maintaining its own
//! understanding of the wire format.

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use neosh_proto::{
    ContentBlock, DriverKind, InstanceConfig, Message, ProviderEvent, Role, TurnRequest,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{Provider, ProviderStream, sse};

#[derive(Debug)]
pub struct ClaudeCliProvider {
    program: String,
    /// The CLI owns conversation history; we hold its session id and resume into it.
    ///
    /// One conversation per instance, which matches one neosh session. Multiple concurrent
    /// conversations against this driver would need a session id threaded through `TurnRequest`.
    session: Arc<Mutex<Option<String>>>,
}

impl Default for ClaudeCliProvider {
    fn default() -> Self {
        Self::new("claude")
    }
}

impl ClaudeCliProvider {
    pub fn new(program: impl Into<String>) -> Self {
        Self { program: program.into(), session: Arc::default() }
    }

    /// Whether the CLI is on `PATH`. Used to decide if this instance is offerable at all.
    pub fn available(&self) -> bool {
        which(&self.program).is_some()
    }

    /// Forget the CLI-side conversation, so the next turn starts fresh.
    pub fn reset_session(&self) {
        *self.session.lock().expect("session lock poisoned") = None;
    }

    /// The CLI takes a single prompt, not a message array, so we send only the newest user turn
    /// and let `--resume` supply the history it already has.
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

fn which(program: &str) -> Option<std::path::PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) {
        let p = std::path::PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join(program)).find(|p| p.is_file())
    })
}

#[async_trait::async_trait]
impl Provider for ClaudeCliProvider {
    fn driver(&self) -> DriverKind {
        DriverKind::from("claude-cli")
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
        let session = self.session.clone();

        tokio::spawn(async move {
            let mut cmd = Command::new(&program);
            cmd.arg("-p")
                .arg(Self::prompt_from(&request.messages))
                .args(["--output-format", "stream-json"])
                .arg("--include-partial-messages")
                // The CLI requires --verbose to emit stream-json on stdout.
                .arg("--verbose")
                .args(["--model", request.selection.model.as_ref()]);

            if let Some(effort) = request.selection.option_str("effort") {
                cmd.args(["--effort", effort]);
            }
            if let Some(prev) = session.lock().expect("session lock poisoned").clone() {
                cmd.args(["--resume", &prev]);
            }

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

            // Keep stderr so a failure can be reported with its actual cause rather than just an
            // exit code.
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

            let mut saw_stop = false;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        // Cancellation must actually reap the child; otherwise an interrupted turn
                        // leaves a process burning tokens in the background.
                        let _ = child.start_kill();
                        break;
                    }
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                                    continue; // non-JSON chatter on stdout is not fatal
                                };
                                if v.get("type").and_then(|t| t.as_str()) == Some("system")
                                    && v.get("subtype").and_then(|t| t.as_str()) == Some("init")
                                    && let Some(id) = v.get("session_id").and_then(|s| s.as_str())
                                {
                                    *session.lock().expect("session lock poisoned") =
                                        Some(id.to_string());
                                }
                                if let Some(ev) = sse::claude_cli_line(&v) {
                                    saw_stop |= matches!(ev, ProviderEvent::MessageStop);
                                    if tx.send(ev).await.is_err() {
                                        // Receiver dropped: the turn was abandoned.
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
                let detail = stderr_buf.lock().expect("stderr lock poisoned").trim().to_string();
                let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                let _ = tx
                    .send(ProviderEvent::Error {
                        message: if detail.is_empty() {
                            format!("{program} exited with status {code} before finishing the turn")
                        } else {
                            detail
                        },
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

    fn inst() -> InstanceConfig {
        InstanceConfig {
            id: InstanceId::from("claude-cli"),
            driver: DriverKind::from("claude-cli"),
            display_name: "Claude CLI".into(),
            base_url: None,
            brand: None,
            auth: AuthRef::Inherited,
            models: vec![],
            extra_headers: vec![],
        }
    }

    fn req(model: &str) -> TurnRequest {
        TurnRequest {
            selection: ModelSelection {
                instance: InstanceId::from("claude-cli"),
                model: model.into(),
                options: vec![],
            },
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hello".into() }],
            }],
            tools: vec![],
            max_output_tokens: None,
        }
    }

    #[test]
    fn prompt_uses_the_latest_user_message() {
        let msgs = vec![
            Message { role: Role::User, content: vec![ContentBlock::Text { text: "old".into() }] },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "reply".into() }],
            },
            Message { role: Role::User, content: vec![ContentBlock::Text { text: "new".into() }] },
        ];
        assert_eq!(ClaudeCliProvider::prompt_from(&msgs), "new");
    }

    #[test]
    fn it_declares_that_it_owns_the_agent_loop() {
        assert!(ClaudeCliProvider::default().delegates_agent_loop());
    }

    /// A missing binary must surface as a provider error on the stream, not a panic or a hang.
    #[tokio::test]
    async fn a_missing_binary_reports_an_error_event() {
        use futures::StreamExt;
        let p = ClaudeCliProvider::new("definitely-not-a-real-binary-xyzzy");
        assert!(!p.available());
        let got: Vec<_> =
            p.stream(&inst(), req("opus"), CancellationToken::new()).collect().await;
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], ProviderEvent::Error { retryable: false, .. }));
    }
}
