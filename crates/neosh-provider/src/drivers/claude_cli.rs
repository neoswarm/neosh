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
//! # One process per conversation, not one per turn
//!
//! The CLI is started once and kept. Each turn is another message written to the stdin it is
//! already waiting on, and the reply comes back on the stdout already being read. This is what the
//! Claude Agent SDK does with the same binary, and it is not a performance decision — it is the
//! only shape in which the *control protocol* means anything, because there is no session to
//! control when the process only exists for the length of one answer.
//!
//! What that buys, all of it otherwise impossible:
//!
//! * `^P` mid-turn — [`Control::Model`] reaches the running loop, and the next thing it thinks it
//!   thinks with the model you just picked.
//! * `⇧⇥` mid-turn — likewise, three of the four modes.
//! * `<Esc>` that *interrupts* rather than kills. A killed child takes its conversation with it and
//!   the next turn starts from a `--resume` that has never heard of the half-finished work.
//!
//! # What still costs a new process
//!
//! Everything the CLI only accepts on its command line, gathered in [`Launch`]: the directory, the
//! reasoning effort, the settings blob, and whether it was started with
//! `--dangerously-skip-permissions`. That last one is not a guess — asking for
//! `bypassPermissions` over the control protocol is refused in as many words:
//! *"Cannot set permission mode to bypassPermissions because the session was not launched with
//! --dangerously-skip-permissions"*. A replacement carries `--resume` with the session id the old
//! process reported, so the conversation survives the process that was holding it.
//!
//! # Permissions
//!
//! The CLI has its own policy, and neosh's permission mode is translated into it — see
//! [`control_mode`]. In any mode that prompts, the prompt arrives *here*, over the control
//! protocol in [`super::claude_control`], and is answered by the same person a built-in tool call
//! would reach. `--permission-prompt-tool stdio` requires `--input-format stream-json`, which is
//! the same input format a long-lived session needs, so the two fit together rather than trading
//! off. See ADR 0032.
//!
//! # The useful discovery
//!
//! `--output-format stream-json --include-partial-messages` emits the Anthropic Messages SSE
//! payloads *verbatim*, wrapped as `{"type":"stream_event","event":{…}}`. So this driver and the
//! HTTPS driver share one parser ([`crate::sse::anthropic`]) rather than each maintaining its own
//! understanding of the wire format.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use neosh_proto::{
    ContentBlock, DriverKind, InstanceConfig, Message, ModelInfo, PermissionMode, ProviderEvent,
    Role, SessionId, TurnRequest,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::approval::{PermissionAnswer, PermissionAsker};
use super::claude_control;
use crate::{Provider, ProviderError, ProviderStream, catalog, sse};

/// How neosh's permission mode is spelled to the CLI.
///
/// This has to be said explicitly. Sending nothing left the CLI on its own default in all four of
/// neosh's modes, including full access, so every write and every command came back as "requires
/// approval" whatever the setting said — the setting was a label on nothing.
///
/// The mapping, and why each one. Read alongside [`super::claude_control`]: the prompt tool is
/// passed as well, so a mode that prompts now prompts *here* rather than being a mode that refuses.
///
/// * `deny` → `plan`, the CLI's read-only mode. It may look at the workspace and say what it would
///   do, and change none of it. That is a truer reading of "deny" than refusing to start, and the
///   only one of the four that leaves the turn worth taking.
/// * `ask` → `default`: the one that prompts. Those prompts arrive over the control protocol and
///   reach a person, so "ask" means what it says.
/// * `allow_listed` → `acceptEdits`. The agent's tools are the agent's own, so its allow-list is
///   its own settings file; neosh's `allow_commands` never sees them. Anything outside it still
///   reaches the prompt.
/// * `allow` → `bypassPermissions`, which is the one that cannot be reached by asking. See
///   [`Launch::dangerous`].
pub fn control_mode(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Deny => "plan",
        PermissionMode::Ask => "default",
        PermissionMode::AllowListed => "acceptEdits",
        PermissionMode::Allow => "bypassPermissions",
    }
}

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

/// The parts of a turn the CLI will only hear on its command line.
///
/// Gathered in one place because they are exactly the set that costs a new process: there is no
/// control request for any of them. Everything *not* here — the model, three of the four
/// permission modes — is changed in place on the session that is already running.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Launch {
    /// Where the conversation is. The CLI reads files, runs commands and looks at git, all of it
    /// relative to here.
    cwd: std::path::PathBuf,
    /// Reasoning effort, which is `--effort` and nothing else.
    effort: Option<String>,
    /// The settings blob for the switches that are settings rather than flags.
    settings: Option<String>,
    /// Whether it was started with `--dangerously-skip-permissions`.
    ///
    /// The one permission mode that cannot be reached by asking. The CLI says so itself: *"Cannot
    /// set permission mode to bypassPermissions because the session was not launched with
    /// --dangerously-skip-permissions"*. So crossing into or out of full access is the one mode
    /// change that costs a process, and the other three are free.
    dangerous: bool,
    /// Whether there is anybody to answer a permission prompt. Changes only when the host wires an
    /// asker up, which happens once — but it is a flag, so it belongs here.
    prompting: bool,
}

impl Launch {
    fn of(request: &TurnRequest, mode: PermissionMode, prompting: bool) -> Self {
        Self {
            cwd: request.cwd.clone(),
            effort: request.selection.option_str("effort").map(str::to_string),
            settings: settings_arg(&request.selection),
            dangerous: mode == PermissionMode::Allow,
            prompting,
        }
    }
}

/// A `claude` that is still running, between turns as well as during them.
#[derive(Debug)]
struct Live {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    /// Kept so a failure can be reported with its actual cause rather than an exit code.
    stderr: Arc<Mutex<String>>,
    /// The stream parser's memory.
    ///
    /// Per conversation and not per turn, which is the whole reason it can exist at all: a plan
    /// made in one turn is still the plan in the next, and a parser that was thrown away with the
    /// process could only ever have known about the answer it was in the middle of.
    state: sse::ClaudeState,
    /// The CLI's own id for this conversation, to `--resume` into when the process has to be
    /// replaced.
    session: Option<String>,
    launched: Launch,
    /// What it has since been *told*, which is not what it was launched with. Tracked so a repeat
    /// is not sent: `set_model` on every turn would be a control round-trip saying nothing.
    model: String,
    mode: PermissionMode,
}

/// A change asked for while a turn is running.
///
/// Written from outside the turn — the model picker, the permission picker — and taken by the loop
/// the moment it is woken. It cannot be written straight to the CLI from there: the turn holds the
/// only handle to its stdin, and a second writer would interleave two JSON lines into one.
#[derive(Debug, Default)]
struct Tune {
    model: Option<String>,
    mode: Option<PermissionMode>,
}

/// One conversation's CLI, and the way to reach it while it is busy.
#[derive(Debug, Default)]
struct Conversation {
    /// The process. Locked for the length of a turn, which is what serialises two turns in the
    /// same conversation: they share one stdin, and interleaving them would interleave the answers.
    live: tokio::sync::Mutex<Option<Live>>,
    tune: Mutex<Tune>,
    /// Raised when [`Self::tune`] has something in it, so the running loop finds out without
    /// polling — and so a change made while the model has been thinking silently for half a minute
    /// does not wait for it to say something.
    wake: tokio::sync::Notify,
}

#[derive(Debug)]
pub struct ClaudeCliProvider {
    program: String,
    /// What `claude --version` said, asked once.
    ///
    /// `Some(None)` means we asked and could not tell — a CLI that will not report its version is
    /// treated as new enough, because hiding every recent model from somebody whose install works
    /// fine is the worse of the two mistakes.
    version: Arc<Mutex<Option<Option<(u32, u32, u32)>>>>,
    /// One live CLI per conversation.
    ///
    /// Per conversation, not per driver. There is one of these objects for the whole program and
    /// several conversations may be mid-turn at once, so a single session id here — which is what
    /// it used to be — meant the second conversation resumed into the first one's history, and
    /// nothing anywhere would have said so.
    ///
    /// The outer lock is held only long enough to look a slot up. The inner one is held for the
    /// length of a turn, which is also what serialises two turns in the same conversation: they
    /// share one process and one stdin, and interleaving them would be interleaving the answers.
    live: Arc<Mutex<HashMap<SessionId, Arc<Conversation>>>>,
    /// What neosh's permission mode is, mirrored here because the driver is told about a change
    /// out of band and the next turn has to act on it.
    mode: Arc<Mutex<PermissionMode>>,
    /// Who to ask when the CLI wants permission. `None` in a test or a headless run, and the
    /// prompt tool is then not offered at all.
    asker: Arc<Mutex<Option<Arc<dyn PermissionAsker>>>>,
}

impl Default for ClaudeCliProvider {
    fn default() -> Self {
        Self::new("claude")
    }
}

impl ClaudeCliProvider {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            version: Arc::default(),
            live: Arc::default(),
            mode: Arc::new(Mutex::new(PermissionMode::Ask)),
            asker: Arc::default(),
        }
    }

    /// Whether the CLI is on `PATH`. Used to decide if this instance is offerable at all.
    pub fn available(&self) -> bool {
        which(&self.program).is_some()
    }

    /// Let go of a conversation's CLI, so nothing is left running for a conversation nobody has.
    ///
    /// The process is asked to stop rather than killed: its stdin is dropped, which is how a
    /// `--print` session is told there are no more messages coming, and it exits on its own.
    pub fn shutdown(&self, conversation: &SessionId) {
        let Some(slot) = self.live.lock().expect("live lock poisoned").remove(conversation) else {
            return;
        };
        tokio::spawn(async move {
            if let Some(live) = slot.live.lock().await.take() {
                live.close().await;
            }
        });
    }

    /// The slot this conversation's CLI lives in, empty until the first turn puts one there.
    fn slot(&self, conversation: &SessionId) -> Arc<Conversation> {
        self.live
            .lock()
            .expect("live lock poisoned")
            .entry(conversation.clone())
            .or_default()
            .clone()
    }

    /// Ask every conversation that has a session to change something, now.
    fn tune_all(&self, f: impl Fn(&mut Tune)) {
        let all: Vec<Arc<Conversation>> =
            self.live.lock().expect("live lock poisoned").values().cloned().collect();
        for c in all {
            f(&mut c.tune.lock().expect("tune lock poisoned"));
            c.wake.notify_one();
        }
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
        let slot = self.slot(&request.conversation);
        let mode = *self.mode.lock().expect("mode lock poisoned");
        let asker = self.asker.lock().expect("asker lock poisoned").clone();

        tokio::spawn(async move {
            // Held for the whole turn. Two turns in one conversation share one process and one
            // stdin, so running them at once would interleave two answers down one pipe.
            let mut guard = slot.live.lock().await;
            let turn = Turn { request, mode, asker };
            if let Err(message) =
                run_turn(&program, &slot, &mut guard, turn, cancel, &tx).await
            {
                // A turn that failed for a reason the process cannot recover from leaves nothing
                // worth keeping: the next one starts a fresh CLI rather than writing into a pipe
                // whose other end has gone.
                *guard = None;
                let _ = tx.send(ProviderEvent::Error { message, retryable: false }).await;
            }
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

/// What is being asked, and under what policy.
///
/// A struct rather than three more arguments: they are the parts of one question and they travel
/// together everywhere.
struct Turn {
    request: TurnRequest,
    mode: PermissionMode,
    asker: Option<Arc<dyn PermissionAsker>>,
}

/// One turn, on a CLI that is either already running or about to be.
/// The `Err` case is for a failure that ends the turn *and* the process — a spawn that did not
/// work, a pipe that has gone. Everything a running CLI reports about itself, including its own
/// errors, arrives as events instead.
async fn run_turn(
    program: &str,
    conversation: &Conversation,
    slot: &mut Option<Live>,
    turn: Turn,
    cancel: CancellationToken,
    tx: &mpsc::Sender<ProviderEvent>,
) -> Result<(), String> {
    let Turn { request, mode, asker } = turn;
    // Only offered when somebody can answer it. Passing the flag with nobody behind it would route
    // every decision to a pipe nothing ever writes to, and the turn would hang instead of being
    // refused — worse than the behaviour it replaces.
    let prompting = asker.is_some();
    let launch = Launch::of(&request, mode, prompting);
    let model = model_arg(&request.selection);

    // The one thing a running session cannot be talked out of. Everything else below is a control
    // request on the process that is already there.
    if slot.as_ref().is_none_or(|live| live.launched != launch) {
        let resume = match slot.take() {
            Some(live) => {
                let id = live.session.clone();
                live.close().await;
                id
            }
            None => None,
        };
        *slot = Some(Live::spawn(program, &launch, &model, resume.as_deref()).await?);
    }
    let live = slot.as_mut().expect("a session was just put there");

    // Fired rather than awaited. Ordering on one pipe is enough: the CLI reads these before it
    // reads the message below, so the turn runs with the new setting. An answer that says the
    // request failed comes back on the same stdout as everything else and is reported there —
    // waiting for it here would mean a second reader, racing the first.
    if live.model != model {
        live.control(&json!({"subtype": "set_model", "model": model})).await?;
        live.model.clone_from(&model);
    }
    if live.mode != mode {
        live.control(&json!({"subtype": "set_permission_mode", "mode": control_mode(mode)}))
            .await?;
        live.mode = mode;
    }

    let prompt = ClaudeCliProvider::prompt_from(&request.messages);
    live.say(&prompt).await?;

    // Nothing after this point returns `Err`: the process is up and talking, so whatever happens
    // is something it said, and the turn keeps whatever it produced before it.
    let mut refused = 0usize;
    let mut interrupted = false;
    let mut finished = false;
    // Reset the moment an interrupt is asked for; until then it never fires.
    let giveup = tokio::time::sleep(Duration::from_secs(86_400));
    tokio::pin!(giveup);

    loop {
        tokio::select! {
            biased;
            // Somebody changed the model or the permission mode while this was running. Both are
            // things you reach for *because* of what the turn is doing, so they are applied to the
            // turn that is doing it.
            () = conversation.wake.notified() => {
                let tune = std::mem::take(&mut *conversation.tune.lock().expect("tune lock poisoned"));
                if let Some(model) = tune.model.filter(|m| *m != live.model) {
                    let _ = live.control(&json!({"subtype": "set_model", "model": model})).await;
                    live.model = model;
                }
                // Full access is the one that cannot be asked for — the CLI refuses it unless it
                // was launched for it — so it is left to the next turn, which relaunches. The
                // other three take effect where they were asked for.
                if let Some(mode) = tune.mode.filter(|m| *m != live.mode && *m != PermissionMode::Allow)
                    && !live.launched.dangerous
                {
                    let _ = live
                        .control(&json!({"subtype": "set_permission_mode", "mode": control_mode(mode)}))
                        .await;
                    live.mode = mode;
                }
            }
            // Asked to stop, not killed. A killed CLI takes the conversation with it, and the next
            // turn would resume into a history that has never heard of the work it interrupted.
            () = cancel.cancelled(), if !interrupted => {
                interrupted = true;
                let _ = live.control(&json!({"subtype": "interrupt"})).await;
                giveup.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(5));
            }
            // It was asked politely and did not stop. Now it is killed, and the slot is emptied so
            // the next turn does not write into a pipe with nothing on the other end.
            () = &mut giveup, if interrupted => {
                let _ = live.child.start_kill();
                *slot = None;
                return Ok(());
            }
            line = live.lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    // The process is gone. Whatever it managed to say is already in the turn.
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(ProviderEvent::Error {
                            message: format!("reading {program} output: {e}"),
                            retryable: false,
                        }).await;
                        *slot = None;
                        return Ok(());
                    }
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue; // non-JSON chatter on stdout is not fatal
                };
                // Every line carries it, and it is what a replacement process resumes into.
                if live.session.is_none()
                    && let Some(id) = v.get("session_id").and_then(|s| s.as_str())
                {
                    live.session = Some(id.to_string());
                }
                // A question, which the CLI is now blocked on. Answered inline: the turn genuinely
                // is waiting on a person, and the transcript's spinner says so.
                if let Some(ask) = claude_control::can_use_tool(&v, &request.cwd) {
                    // Raced against the cancellation, not awaited on its own. A prompt with
                    // nobody at the keyboard is exactly when somebody reaches for `<Esc>`, and a
                    // turn that could only be interrupted between questions could not be
                    // interrupted at all while one was open.
                    let answer = match &asker {
                        Some(a) => tokio::select! {
                            biased;
                            () = cancel.cancelled() => PermissionAnswer::Deny,
                            answer = a.ask(ask.request.clone()) => answer,
                        },
                        None => PermissionAnswer::Deny,
                    };
                    if answer == PermissionAnswer::Deny {
                        refused += 1;
                    }
                    let reply = claude_control::response(&ask, &answer);
                    if write_line(&mut live.stdin, &reply).await.is_err() {
                        *slot = None;
                        break;
                    }
                    continue;
                }
                for ev in control_failure(&v).into_iter().chain(sse::claude_cli_line(&v, &mut live.state)) {
                    if tx.send(ev).await.is_err() {
                        // The receiver is gone: the turn was abandoned. The process is not — it
                        // holds the conversation, and there is every chance the next turn is about
                        // to ask it something.
                        return Ok(());
                    }
                }
                // The turn is over, and the process is not. It goes back to waiting on stdin for
                // whatever is asked next, which is the whole point of keeping it.
                if v.get("type").and_then(|t| t.as_str()) == Some("result") {
                    finished = true;
                    break;
                }
            }
        }
    }

    // Said once, at the end, rather than per refusal: the transcript already shows each refused
    // call, and what is worth saying is that the turn stopped short because of them.
    if refused > 0 {
        let _ = tx
            .send(ProviderEvent::Error {
                message: format!("{refused} request(s) were refused"),
                retryable: false,
            })
            .await;
    }
    if !finished && !cancel.is_cancelled() {
        // Only reachable when the process died mid-turn, so its stderr is the interesting part and
        // the slot has to be emptied whatever it says.
        let detail = slot
            .as_ref()
            .map(|l| l.stderr.lock().expect("stderr lock poisoned").trim().to_string())
            .unwrap_or_default();
        let code = match slot.take() {
            Some(mut live) => live.child.wait().await.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1),
            None => -1,
        };
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
    Ok(())
}

/// A control request the CLI refused, as something the transcript can say.
///
/// These are answers to requests neosh made on the user's behalf — "use this model now", "stop" —
/// and a refusal means the thing they just asked for did not happen. Swallowing it would leave a
/// turn running on the old model with nothing anywhere to say why.
fn control_failure(v: &serde_json::Value) -> Option<ProviderEvent> {
    if v.get("type").and_then(Value::as_str) != Some("control_response") {
        return None;
    }
    let response = v.get("response")?;
    if response.get("subtype").and_then(Value::as_str) != Some("error") {
        return None;
    }
    Some(ProviderEvent::Error {
        message: response.get("error").and_then(Value::as_str).unwrap_or("control request failed").to_string(),
        retryable: false,
    })
}

impl Live {
    /// Start one, resuming a conversation a previous process was holding.
    async fn spawn(
        program: &str,
        launch: &Launch,
        model: &str,
        resume: Option<&str>,
    ) -> Result<Self, String> {
        let mut cmd = Command::new(program);
        if !launch.cwd.as_os_str().is_empty() {
            cmd.current_dir(&launch.cwd);
        }
        cmd.arg("--print")
            .args(["--output-format", "stream-json"])
            .arg("--include-partial-messages")
            // The CLI requires --verbose to emit stream-json on stdout.
            .arg("--verbose")
            // Messages arrive on stdin for the length of the session rather than as one positional
            // argument, which is what makes this a session at all — and is also the precondition
            // for `--permission-prompt-tool`.
            .args(["--input-format", "stream-json"])
            .args(["--model", model]);
        if launch.prompting {
            // The hidden flag that turns "there is nobody to ask" into a question on stdout. See
            // [`super::claude_control`].
            cmd.args(["--permission-prompt-tool", "stdio"]);
        }
        if launch.dangerous {
            cmd.arg("--dangerously-skip-permissions");
        }
        if let Some(effort) = &launch.effort {
            cmd.args(["--effort", effort]);
        }
        if let Some(settings) = &launch.settings {
            cmd.args(["--settings", settings]);
        }
        if let Some(id) = resume {
            cmd.args(["--resume", id]);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child =
            cmd.spawn().map_err(|e| format!("could not run {program:?}: {e}"))?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let stdin = child.stdin.take().expect("stdin was piped");

        // Kept so a failure can be reported with its actual cause rather than just an exit code.
        let buf = Arc::new(Mutex::new(String::new()));
        {
            let buf = buf.clone();
            tokio::spawn(async move {
                let mut l = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = l.next_line().await {
                    let mut b = buf.lock().expect("stderr lock poisoned");
                    b.push_str(&line);
                    b.push('\n');
                }
            });
        }

        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            stderr: buf,
            state: sse::ClaudeState::default(),
            session: resume.map(str::to_string),
            launched: launch.clone(),
            model: model.to_string(),
            // What it *is*, not what neosh wants: a fresh process is on the CLI's own default
            // unless the dangerous flag put it somewhere else, and saying otherwise here would
            // skip the control request that gets it there.
            mode: if launch.dangerous { PermissionMode::Allow } else { PermissionMode::Ask },
        })
    }

    /// Ask the running session to change something about itself.
    async fn control(&mut self, request: &Value) -> Result<(), String> {
        let message = json!({
            "type": "control_request",
            "request_id": neosh_proto::RequestId::new().to_string(),
            "request": request,
        });
        write_line(&mut self.stdin, &message)
            .await
            .map_err(|e| format!("could not reach the running claude session: {e}"))
    }

    /// Put a question to it.
    async fn say(&mut self, prompt: &str) -> Result<(), String> {
        write_line(&mut self.stdin, &claude_control::user_message(prompt))
            .await
            .map_err(|e| format!("could not send the prompt to claude: {e}"))
    }

    /// Tell it there is nothing more coming, and wait for it to go.
    ///
    /// Dropping stdin is how a `--print` session is told the conversation is over; it exits on its
    /// own. Killed only if it does not, because a process that is mid-write to a file is better
    /// asked than shot.
    async fn close(mut self) {
        drop(self.stdin);
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await.is_err() {
            let _ = self.child.start_kill();
        }
    }
}

impl super::AgentDriver for ClaudeCliProvider {
    fn set_permission_mode(&self, mode: PermissionMode) {
        *self.mode.lock().expect("mode lock poisoned") = mode;
        // And to every session that is already running, rather than only to the next turn.
        self.tune_all(|t| t.mode = Some(mode));
    }

    fn set_model(&self, conversation: &SessionId, selection: &neosh_proto::ModelSelection) {
        let slot = self.slot(conversation);
        slot.tune.lock().expect("tune lock poisoned").model = Some(model_arg(selection));
        slot.wake.notify_one();
    }

    fn close_conversation(&self, conversation: &SessionId) {
        self.shutdown(conversation);
    }

    fn set_permission_asker(&self, asker: Arc<dyn PermissionAsker>) {
        *self.asker.lock().expect("asker lock poisoned") = Some(asker);
    }
}

/// Write one JSON line to the child's stdin, flushed, because it is waiting on it.
async fn write_line(
    stdin: &mut tokio::process::ChildStdin,
    value: &serde_json::Value,
) -> std::io::Result<()> {
    let mut line = value.to_string();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
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
            conversation: neosh_proto::SessionId::from("test"),
            cwd: std::path::PathBuf::new(),
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

    /// The child is spawned in the conversation's directory.
    ///
    /// Worth a real process rather than a mock: the whole failure this fixes was a `Command` that
    /// looked complete and inherited a directory nobody had chosen, and nothing short of asking a
    /// child where it woke up would have caught it.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_child_is_started_in_the_conversations_directory() {
        use futures::StreamExt;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("neosh-cwd-{}", std::process::id()));
        let work = dir.join("work");
        std::fs::create_dir_all(&work).expect("dirs");
        let script = dir.join("fake-claude");
        std::fs::write(&script, "#!/bin/sh\npwd > \"$(dirname \"$0\")/where\"\n").expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let p = ClaudeCliProvider::new(script.display().to_string());
        let mut req = req("claude-opus-5");
        req.cwd = work.clone();
        let _: Vec<_> = p.stream(&inst(), req, CancellationToken::new()).collect().await;

        let saw = std::fs::read_to_string(dir.join("where")).expect("the child ran");
        // Compared canonically: the temp directory is a symlink on some systems, and `pwd` in a
        // shell reports the logical path.
        assert_eq!(
            std::fs::canonicalize(saw.trim()).ok(),
            std::fs::canonicalize(&work).ok(),
            "spawned in {saw:?}, wanted {}",
            work.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty directory means "wherever the host is", which is what a driver with no opinion did
    /// before the field existed — so a provider plugin written against the old shape still works.
    #[test]
    fn an_empty_directory_is_left_to_the_process() {
        let r = req("claude-opus-5");
        assert!(r.cwd.as_os_str().is_empty());
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


    #[test]
    fn every_mode_says_something_different_to_the_cli() {
        // The bug this replaces: nothing was passed at all, so all four of neosh's modes ran the
        // CLI on its own default and "full access" refused writes exactly like "deny" did.
        use PermissionMode::*;
        assert_eq!(control_mode(Deny), "plan");
        assert_eq!(control_mode(Ask), "default");
        assert_eq!(control_mode(AllowListed), "acceptEdits");
        assert_eq!(control_mode(Allow), "bypassPermissions");
        let all: std::collections::HashSet<_> =
            [Deny, Ask, AllowListed, Allow].map(control_mode).into_iter().collect();
        assert_eq!(all.len(), 4, "four modes, four behaviours");
    }

    #[test]
    fn only_full_access_is_launched_dangerously() {
        // The CLI refuses `bypassPermissions` over the control protocol unless it was started with
        // the flag — in those words — so this is the one mode change that costs a process, and the
        // other three must not drag the flag along with them.
        let launch = |mode| Launch::of(&req("opus"), mode, true).dangerous;
        for m in [PermissionMode::Deny, PermissionMode::Ask, PermissionMode::AllowListed] {
            assert!(!launch(m), "{m:?} must not bypass anything");
        }
        assert!(launch(PermissionMode::Allow));
    }

    #[test]
    fn changing_the_model_does_not_cost_a_process_and_changing_the_directory_does() {
        // What separates the two lists is whether the CLI has a control request for it. Getting
        // this wrong in the cheap direction is the worse mistake: a session restarted on every
        // model switch is one that cannot be switched mid-turn at all, which is the whole reason
        // it is kept alive.
        let mut here = req("opus");
        here.cwd = std::path::PathBuf::from("/work");
        let base = Launch::of(&here, PermissionMode::Ask, true);

        let mut other_model = here.clone();
        other_model.selection.model = "claude-sonnet-5".into();
        assert_eq!(Launch::of(&other_model, PermissionMode::Ask, true), base, "a control request");

        let mut elsewhere = here.clone();
        elsewhere.cwd = std::path::PathBuf::from("/elsewhere");
        assert_ne!(Launch::of(&elsewhere, PermissionMode::Ask, true), base, "a command line");

        let mut harder = here.clone();
        harder.selection.options = vec![neosh_proto::OptionSelection {
            id: "effort".into(),
            value: neosh_proto::ProviderOptionValue::Text("xhigh".into()),
        }];
        assert_ne!(
            Launch::of(&harder, PermissionMode::Ask, true),
            base,
            "`--effort` and nothing else"
        );
    }
}
