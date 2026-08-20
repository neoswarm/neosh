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
    ContentBlock, DriverKind, InstanceConfig, Message, ModelInfo, ProviderEvent, Role, TurnRequest,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{Provider, ProviderError, ProviderStream, catalog, sse};

/// The oldest `claude` that will accept a given model.
///
/// A CLI that has not heard of a model does not degrade — it exits with "unknown model" after you
/// have typed your question. So the picker is filtered rather than the failure explained, and
/// anything not listed here has been around long enough that no installed version lacks it.
///
/// Sourced from the release each model shipped in. Wrong in the safe direction if it drifts: a
/// too-high floor hides a model until the next upgrade, a too-low one is the behaviour we have
/// today.
const MIN_VERSION: &[(&str, (u32, u32, u32))] = &[
    ("claude-opus-5", (2, 1, 219)),
    ("claude-fable-5", (2, 1, 169)),
    ("claude-sonnet-5", (2, 1, 219)),
    ("claude-opus-4-8", (2, 1, 154)),
    ("claude-opus-4-7", (2, 1, 111)),
];

#[derive(Debug)]
pub struct ClaudeCliProvider {
    program: String,
    /// What `claude --version` said, asked once.
    ///
    /// `Some(None)` means we asked and could not tell — a CLI that will not report its version is
    /// treated as new enough, because hiding every recent model from somebody whose install works
    /// fine is the worse of the two mistakes.
    version: Arc<Mutex<Option<Option<(u32, u32, u32)>>>>,
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
        Self { program: program.into(), version: Arc::default(), session: Arc::default() }
    }

    /// Whether the CLI is on `PATH`. Used to decide if this instance is offerable at all.
    pub fn available(&self) -> bool {
        which(&self.program).is_some()
    }

    /// Forget the CLI-side conversation, so the next turn starts fresh.
    pub fn reset_session(&self) {
        *self.session.lock().expect("session lock poisoned") = None;
    }

    /// The installed version, asked once and remembered.
    ///
    /// Blocking, deliberately: this is one `--version` on a program already on `PATH`, and the
    /// alternative is an async lock held across a spawn in a function whose only caller is itself
    /// async and already awaiting a list.
    fn version(&self) -> Option<(u32, u32, u32)> {
        let mut slot = self.version.lock().expect("version lock poisoned");
        *slot.get_or_insert_with(|| {
            let out = std::process::Command::new(&self.program).arg("--version").output().ok()?;
            parse_version(&String::from_utf8_lossy(&out.stdout))
        })
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

/// Where a program is on `PATH`, or nothing.
///
/// Shared with the other CLI-backed drivers: whether a vendor CLI is installed is the same
/// question every one of them has to answer before it can be offered.
pub(crate) fn which(program: &str) -> Option<std::path::PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) {
        let p = std::path::PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join(program)).find(|p| p.is_file())
    })
}

/// Pull `2.1.237` out of `2.1.237 (Claude Code)`.
///
/// Leading-number-triple rather than a semver parse: the string is a version *followed by* a
/// product name, which no semver crate will accept, and the part we compare is the triple.
fn parse_version(out: &str) -> Option<(u32, u32, u32)> {
    let field = out.split_whitespace().next()?;
    let mut parts = field.split('.').map(|p| p.trim_end_matches(|c: char| !c.is_ascii_digit()));
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor, patch))
}

/// Whether an installed version can serve a model.
///
/// Unknown version means yes: see [`ClaudeCliProvider::version`].
fn serves(model: &str, installed: Option<(u32, u32, u32)>) -> bool {
    let Some(need) = MIN_VERSION.iter().find(|(id, _)| *id == model).map(|(_, v)| *v) else {
        return true;
    };
    installed.is_none_or(|have| have >= need)
}

/// The model string `--model` should be given.
///
/// The long context window is a suffix on the id rather than a flag, which is how the CLI spells
/// it: `claude-opus-5[1m]`. Only ever appended for a model whose catalogue entry offers the
/// choice, so a selection carried over from a model that had the knob cannot smuggle it onto one
/// that does not.
fn model_arg(selection: &neosh_proto::ModelSelection) -> String {
    match selection.option_str("context") {
        Some("1m") => format!("{}[1m]", selection.model),
        _ => selection.model.to_string(),
    }
}

/// The `--settings` payload for the switches that are settings rather than flags.
///
/// Fast mode and always-on thinking have no command-line flag; the CLI reads them from its
/// settings, which `--settings` will take as a JSON string. Returns nothing when neither is on, so
/// the usual case adds no argument at all.
fn settings_arg(selection: &neosh_proto::ModelSelection) -> Option<String> {
    let mut map = serde_json::Map::new();
    if selection.option_flag("fast_mode") == Some(true) {
        map.insert("fastMode".into(), true.into());
    }
    if let Some(on) = selection.option_flag("thinking") {
        map.insert("alwaysThinkingEnabled".into(), on.into());
    }
    (!map.is_empty()).then(|| serde_json::Value::Object(map).to_string())
}

#[async_trait::async_trait]
impl Provider for ClaudeCliProvider {
    fn driver(&self) -> DriverKind {
        DriverKind::from("claude-cli")
    }

    /// The catalogue, minus anything this install is too old to accept.
    ///
    /// Not a network call and not cached upstream by accident: `--version` is asked once per
    /// process, and the filtering is a comparison over nine entries.
    async fn list_models(&self, _instance: &InstanceConfig) -> Result<Vec<ModelInfo>, ProviderError> {
        let installed = self.version();
        Ok(catalog::claude_cli_models()
            .into_iter()
            .filter(|m| serves(m.id.as_ref(), installed))
            .collect())
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
                .args(["--model", &model_arg(&request.selection)]);

            if let Some(effort) = request.selection.option_str("effort") {
                cmd.args(["--effort", effort]);
            }
            if let Some(settings) = settings_arg(&request.selection) {
                cmd.args(["--settings", &settings]);
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

    fn sel(model: &str, options: &[(&str, neosh_proto::ProviderOptionValue)]) -> ModelSelection {
        ModelSelection {
            instance: InstanceId::from("claude-cli"),
            model: model.into(),
            options: options
                .iter()
                .map(|(id, v)| neosh_proto::OptionSelection { id: (*id).into(), value: v.clone() })
                .collect(),
        }
    }

    #[test]
    fn the_version_is_the_number_in_front_of_the_product_name() {
        assert_eq!(parse_version("2.1.237 (Claude Code)\n"), Some((2, 1, 237)));
        assert_eq!(parse_version("2.1 (Claude Code)"), Some((2, 1, 0)));
        assert_eq!(parse_version("who knows"), None);
    }

    /// The ordering that matters is patch-level: the releases these models shipped in differ only
    /// there, so a comparison that stopped at minor would offer every one of them to every install.
    #[test]
    fn an_older_cli_is_not_offered_a_model_it_will_reject() {
        assert!(!serves("claude-opus-5", Some((2, 1, 218))));
        assert!(serves("claude-opus-5", Some((2, 1, 219))));
        assert!(serves("claude-opus-5", Some((2, 2, 0))));
        // Nothing recorded means it has always been there.
        assert!(serves("claude-haiku-4-5", Some((1, 0, 0))));
    }

    /// A CLI that will not say what it is gets the benefit of the doubt: hiding everything recent
    /// from somebody whose install works is worse than offering one model too many.
    #[test]
    fn an_unknown_version_is_offered_everything() {
        assert!(serves("claude-opus-5", None));
    }

    #[test]
    fn the_long_context_window_is_a_suffix_on_the_model_not_a_flag() {
        use neosh_proto::ProviderOptionValue::Text;
        assert_eq!(model_arg(&sel("claude-opus-5", &[("context", Text("1m".into()))])), "claude-opus-5[1m]");
        assert_eq!(model_arg(&sel("claude-opus-5", &[("context", Text("200k".into()))])), "claude-opus-5");
        assert_eq!(model_arg(&sel("claude-opus-5", &[])), "claude-opus-5");
    }

    /// Fast mode and always-on thinking have no flag; they are settings, and `--settings` takes
    /// JSON. Neither on means no argument at all, so the ordinary turn is unchanged.
    #[test]
    fn the_switches_without_flags_travel_as_settings_json() {
        use neosh_proto::ProviderOptionValue::Flag;
        assert_eq!(settings_arg(&sel("claude-opus-5", &[])), None);
        assert_eq!(
            settings_arg(&sel("claude-opus-5", &[("fast_mode", Flag(false))])),
            None,
            "off is the default, and saying so would be a setting written for no reason"
        );
        assert_eq!(
            settings_arg(&sel("claude-opus-5", &[("fast_mode", Flag(true))])).as_deref(),
            Some(r#"{"fastMode":true}"#)
        );
        assert_eq!(
            settings_arg(&sel("claude-haiku-4-5", &[("thinking", Flag(false))])).as_deref(),
            Some(r#"{"alwaysThinkingEnabled":false}"#),
            "thinking off is a real instruction — the CLI's own default is not necessarily off"
        );
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
