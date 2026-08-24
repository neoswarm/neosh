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
    Activity, ContentBlock, DriverKind, InstanceConfig, Message, ModelInfo, PermissionMode,
    ProviderEvent, Role, SessionId, TurnRequest,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::approval::{PermissionAnswer, PermissionAsker, PermissionRequest};
use crate::ask::{QuestionAnswers, QuestionAsker, QuestionRequest};
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
    /// Everything the CLI has said that nothing has read yet.
    ///
    /// A channel rather than the pipe itself, because the reader has to outlive the turn for the
    /// same reason the process does. `claude` answers things nobody asked it: a backgrounded
    /// command finishing is enqueued as a message and replied to on its own, with a whole turn of
    /// `init`, thinking, text and `result` on stdout. Read only while a turn is in flight, that
    /// turn is invisible until somebody happens to type — and then it is worse than invisible,
    /// because its `result` is the first one the next turn sees and the next turn ends on it. See
    /// [`run_turn`].
    ///
    /// Unbounded on purpose: the alternative to holding it is a full pipe, and a full pipe stops
    /// the CLI mid-sentence with nothing anywhere to say why.
    lines: mpsc::UnboundedReceiver<String>,
    /// Whether the CLI is in the middle of a turn — `system`/`init` opens one, `result` closes it.
    ///
    /// On the process rather than in the turn, because the turn it describes need not be ours.
    busy: bool,
    /// Shared with the reader: whether a turn has begun that nothing has read. Cleared here, by
    /// whoever reads the `init` that set it. See [`Unread`].
    unread: Arc<Unread>,
    /// Lines read after a turn was already over, kept for whoever reads next.
    ///
    /// There is exactly one moment a turn reads past its own ending: having seen `result` it asks
    /// how full the context is and waits for the answer. Whatever else arrives in that window is
    /// the *next* turn's — the CLI answering something nobody asked, most often — and folding it
    /// into an answer that has already been given puts half of one turn under another.
    pushed_back: std::collections::VecDeque<String>,
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

/// Whether the CLI has begun a turn that nothing has read — the one piece of state the reader task
/// and the turn loop both touch.
///
/// The reader is the only thing looking at the pipe between turns, so it is the only thing that can
/// notice a turn nobody asked for. It cannot simply report every `init` it sees: an ordinary turn
/// opens with one too, and that one is being read. So it reports only what nothing is reading —
/// and, when something *is*, leaves a note that whoever it was clears by reading the line. A note
/// still there when the last reader goes is a turn that was begun and never heard, and it is
/// reported on the way out. Without that, an `init` landing in the moment between a turn's last
/// read and its bookkeeping was a turn nobody would ever hear about.
#[derive(Debug, Default)]
struct Unread {
    /// How many turns are reading. A count rather than a flag: a turn that starts while the last
    /// one is still winding down would otherwise be un-marked by the older one's way out.
    attached: std::sync::atomic::AtomicUsize,
    /// An `init` seen while somebody was reading, and not yet read by them.
    pending: std::sync::atomic::AtomicBool,
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
    /// Whether the CLI has begun a turn that nothing has read. See [`Unread`].
    unread: Arc<Unread>,
    /// Set by the host just before it opens a turn to hold something the agent is already saying.
    /// Taken by that turn, which then says nothing and only reads. See
    /// [`super::AgentDriver::listen_only`].
    listen: Arc<std::sync::atomic::AtomicBool>,
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
    /// Where a question goes. Separate from `asker` because a question is not a permission — see
    /// [`crate::ask`].
    questioner: Arc<Mutex<Option<Arc<dyn QuestionAsker>>>>,
    /// Where to say that the CLI has started a turn nobody asked for. See [`super::Unasked`].
    unasked: Arc<Mutex<Option<Arc<dyn super::Unasked>>>>,
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
            questioner: Arc::default(),
            unasked: Arc::default(),
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
    ///
    /// A string when that is all there is, and the Anthropic block array when the message carries
    /// a picture — the CLI's stdin takes the same `content` the API does, so this is the API's
    /// answer rather than a shape of the CLI's own. Plain text stays a plain string on purpose:
    /// it is what every existing test and every reader expects to see going down that pipe.
    fn prompt_from(messages: &[Message]) -> Value {
        let Some(m) = messages.iter().rev().find(|m| m.role == Role::User) else {
            return Value::String(String::new());
        };
        let text = m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let images: Vec<Value> = m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Image { path, media_type } => Some(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": crate::image::base64_at(path)?,
                    },
                })),
                _ => None,
            })
            .collect();
        if images.is_empty() {
            return Value::String(text);
        }
        // The picture first. A question about an image reads as one when the image is what the
        // sentence is pointing at, and the CLI passes the order through.
        let mut blocks = images;
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
        Value::Array(blocks)
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
        // A one-shot belongs to nobody, so it gets a slot nobody else can find and the process is
        // let go of at the end of it. Keyed into the map instead — which is what an empty id does
        // if you do not ask — every branch name and thread title in the workspace shared one CLI,
        // and that CLI was never closed: a second `claude` running beside the conversation you
        // were in, holding a full-access session open for as long as the workspace lived.
        let one_shot = request.is_one_shot();
        let slot = match one_shot {
            true => Arc::new(Conversation::default()),
            false => self.slot(&request.conversation),
        };
        let mode = *self.mode.lock().expect("mode lock poisoned");
        let asker = self.asker.lock().expect("asker lock poisoned").clone();
        let questioner = self.questioner.lock().expect("questioner lock poisoned").clone();
        let unasked = self.unasked.lock().expect("unasked lock poisoned").clone();
        let here = request.conversation.clone();

        tokio::spawn(async move {
            // Before the lock, not after: a turn queued behind another one is still a turn, and
            // the reader must not report what it is waiting for as something nobody asked for.
            let watch =
                Watch { unread: slot.unread.clone(), conversation: here, sink: unasked.clone() };
            let attached = Attached::of(watch.clone());
            // Held for the whole turn. Two turns in one conversation share one process and one
            // stdin, so running them at once would interleave two answers down one pipe.
            let mut guard = slot.live.lock().await;
            let mut turn = Turn { request, mode, asker, questioner, unasked };
            // Retried exactly once, and only for the one failure a retry can fix: the conversation
            // this driver was told to pick up is not there any more — the CLI's own history was
            // cleared, or the project directory moved out from under it. Starting fresh loses what
            // the agent remembered, which is already lost; reporting it instead would fail a turn
            // over a stale value neither side can do anything about.
            let mut result = run_turn(&program, &slot, &mut guard, turn.clone(), cancel.clone(), &tx).await;
            if matches!(result, Ok(Outcome::Stale)) {
                *guard = None;
                turn.request.resume = None;
                result = run_turn(&program, &slot, &mut guard, turn, cancel, &tx).await;
            }
            if let Err(message) = result
            {
                // A turn that failed for a reason the process cannot recover from leaves nothing
                // worth keeping: the next one starts a fresh CLI rather than writing into a pipe
                // whose other end has gone.
                *guard = None;
                let _ = tx.send(ProviderEvent::Error { message, retryable: false }).await;
            }
            // Nothing is coming back to this one, so it is asked to stop rather than left holding
            // a login and a JS heap until the workspace ends.
            if one_shot && let Some(live) = guard.take() {
                live.close().await;
            }
            drop(attached);
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

/// What is being asked, and under what policy.
///
/// A struct rather than three more arguments: they are the parts of one question and they travel
/// together everywhere.
#[derive(Clone)]
struct Turn {
    request: TurnRequest,
    mode: PermissionMode,
    asker: Option<Arc<dyn PermissionAsker>>,
    questioner: Option<Arc<dyn QuestionAsker>>,
    unasked: Option<Arc<dyn super::Unasked>>,
}

/// Says a turn is reading this conversation's stdout, for exactly as long as one is.
///
/// A guard rather than two assignments, because a turn has several ways out — a spawn that failed,
/// a stale resume that retries, a cancellation — and a count left raised is a conversation whose
/// agent can never again be heard talking on its own.
struct Attached(Watch);

impl Attached {
    fn of(watch: Watch) -> Self {
        watch.unread.attached.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(watch)
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.0.unread.attached.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        // The last one out takes any note with it. A turn that began while this one was reading
        // and that this one never got to is a turn nothing has heard, and now nothing else will.
        if !self.0.reading()
            && self.0.unread.pending.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.0.report();
        }
    }
}

/// How a turn ended, for the one caller that can do something about it.
enum Outcome {
    /// The turn ran. Whatever the CLI made of it is already in the stream.
    Done,
    /// It never started: the conversation this driver was asked to resume is gone. Nothing has
    /// been forwarded, so the caller may simply try again without it.
    Stale,
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
) -> Result<Outcome, String> {
    let Turn { request, mode, asker, questioner, unasked } = turn;
    // A turn opened to hold something the agent is already saying. It has nothing of its own to
    // ask, so it says nothing and reads until whatever is running stops. Taken rather than read:
    // it describes this turn and must not stick to the next one.
    let listen_only = conversation.listen.swap(false, std::sync::atomic::Ordering::Relaxed);
    // Only offered when somebody can answer it. Passing the flag with nobody behind it would route
    // every decision to a pipe nothing ever writes to, and the turn would hang instead of being
    // refused — worse than the behaviour it replaces.
    // Either kind of asking needs the hidden flag: without it the CLI has nowhere to send a
    // permission request *or* a question, and `AskUserQuestion` silently answers itself.
    let prompting = asker.is_some() || questioner.is_some();
    let launch = Launch::of(&request, mode, prompting);
    let model = model_arg(&request.selection);

    // The one thing a running session cannot be talked out of. Everything else below is a control
    // request on the process that is already there.
    // Whether this process is being started on a handle that outlived a workspace, which is the
    // only kind that can be stale: one taken from a process this one is replacing was valid a
    // moment ago.
    let mut borrowed = false;
    // Whether the process below was started by *this* call. A fresh one has nothing left over from
    // a previous turn by definition, and everything it has said so far belongs to the turn about to
    // run — including the `result` that says it would not resume. See [`drain_between_turns`].
    let mut fresh = false;
    if slot.as_ref().is_none_or(|live| live.launched != launch) {
        let resume = match slot.take() {
            Some(live) => {
                let id = live.session.clone();
                live.close().await;
                id
            }
            // Nothing is running for this conversation, which after a restart is every
            // conversation. What the driver called it last time is the whole of its memory.
            None => {
                borrowed = request.resume.is_some();
                request.resume.clone()
            }
        };
        *slot = Some(
            Live::spawn(program, &launch, &model, resume.as_deref(), Watch {
                unread: conversation.unread.clone(),
                conversation: request.conversation.clone(),
                sink: unasked.clone(),
            })
            .await?,
        );
        fresh = true;
    }
    let live = slot.as_mut().expect("a session was just put there");

    // Whatever the CLI said while nobody was reading.
    //
    // A turn stops being read at its `result`, and the process does not stop talking there. A
    // shell the agent put in the background finishes minutes later, and the CLI says so —
    // `background_tasks_changed`, `task_updated`, `task_notification` — and then *starts a turn of
    // its own* to react to it: a fresh `init`, an assistant message, and a `result`, with nobody
    // having asked it anything. All of it is still sitting in the pipe when the next question is
    // asked, and the loop below reads it as the answer to that question. The stray `result` ends
    // the turn before the CLI has even seen the prompt, so the reply drawn under what you typed is
    // the model's reaction to a shell exiting, and your actual answer arrives one turn late.
    //
    // So it is read *here*, before the prompt goes in, and split: what the CLI said about the
    // conversation is forwarded — those are level and edge facts, idempotent and keyed by task, and
    // the whole reason the indicator can be put right at all — and the unasked turn's own content
    // is dropped. Dropped rather than drawn: it is an answer to a question that was never put, and
    // showing it under the one that was is the mis-attribution this exists to stop. The CLI keeps
    // it in its own history either way, so the model has not forgotten it; only the transcript has.
    if fresh {
        // A process that has just started is running nothing, and it will not say so: the live set
        // is only ever reported when it *changes*, so a receiver that was told about two background
        // shells by the process this one replaced would go on drawing them until something else
        // happened to change the set — which, for shells that died with the old process, is never.
        // The vendor's own rule for this signal, said on our wire: reset to empty on (re)start.
        let _ = tx
            .send(ProviderEvent::Activity {
                activity: Activity::Background { tasks: Vec::new() },
            })
            .await;
    } else if !listen_only {
        // Never for a turn opened to *hear* one of these. Dropping the unasked turn's content is
        // right when it would land under a question it does not answer, and exactly wrong when the
        // turn was opened to put it somewhere of its own. See [`super::AgentDriver::listen_only`].
        drain_between_turns(live, tx).await;
    }

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

    // Whether the prompt has gone out, and how many turn endings stand between here and the end of
    // this one.
    //
    // `drain_between_turns` above took what had already arrived, but a turn nobody asked for that
    // is *still going* is not over: the CLI queues the prompt behind it, and the first `result`
    // after that is theirs. Ending on it is how a message that had not been read yet came back as
    // a turn that said nothing.
    let mut said = false;
    let mut owed = 0usize;
    if !listen_only {
        live.say(prompt).await?;
        said = true;
        owed = if live.busy { 2 } else { 1 };
    }

    // Nothing after this point returns `Err`: the process is up and talking, so whatever happens is
    // something it said, and the turn keeps whatever it produced before it.
    let mut refused = 0usize;
    let mut interrupted = false;
    let mut finished = false;
    // Whether anybody is still listening. Once nobody is, the turn is not over — the *process* is
    // still mid-answer — so this keeps reading and stops forwarding. See where it is set.
    let mut abandoned = false;
    // Whether the context question in flight is the one that ends the turn.
    let mut ending = false;
    // Lines read during that wait, which belong to whatever comes next. Held here rather than put
    // straight back where the next turn will find them, because this loop reads from there too —
    // put back, the same line is read, put back and read again, forever.
    let mut held: Vec<String> = Vec::new();
    // How full the CLI's context is, asked while it works rather than once at the end.
    //
    // This is what the meter is for, and it only means anything if it moves: "should I start a new
    // conversation?" is a question about the turn you are in, and an answer that arrives when that
    // turn finishes has arrived too late to act on. The CLI answers this mid-turn — it is its own
    // accounting rather than a request to the model — so it is asked on a clock.
    let mut clock = tokio::time::interval(Duration::from_secs(5));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Reset the moment an interrupt is asked for; until then it never fires.
    let giveup = tokio::time::sleep(Duration::from_secs(86_400));
    tokio::pin!(giveup);

    loop {
        // A turn opened to hold what the agent is already saying — see `listen_only`. It has
        // nothing to ask, so it says nothing and reads until that turn is over. Nothing was drained
        // on the way in, because dropping the very thing this turn exists to show would be the one
        // mistake it cannot make; so it reads what has arrived first and then decides. An empty
        // pipe with nothing running means it has all been read, and there is nothing to wait for.
        if listen_only && !said && live.pushed_back.is_empty() && live.lines.is_empty() {
            if !live.busy {
                finished = true;
                break;
            }
            said = true;
            owed = 1;
        }
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
            _ = clock.tick(), if !interrupted && !ending => {
                let _ = live.control(&json!({"subtype": "get_context_usage"})).await;
            }
            // Asked to stop, not killed. A killed CLI takes the conversation with it, and the next
            // turn would resume into a history that has never heard of the work it interrupted.
            () = cancel.cancelled(), if !interrupted => {
                interrupted = true;
                let _ = live.control(&json!({"subtype": "interrupt"})).await;
                giveup.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(5));
            }
            // The deadline, which two different waits arm and which therefore means two things.
            // After an interrupt: it was asked politely and did not stop, so now it is killed and
            // the slot emptied, or the next turn writes into a pipe with nothing behind it. After
            // the context question: nothing at all went wrong — the turn is already finished and
            // that was an extra — so it is simply given up on.
            () = &mut giveup, if interrupted || ending => {
                // `ending` is asked first, because it is armed *last* and only by a turn that has
                // already finished. Reading `interrupted` first meant the two-second wait for one
                // number was answered by killing a process that had done everything asked of it.
                if ending {
                    break;
                }
                let _ = live.child.start_kill();
                *slot = None;
                return Ok(Outcome::Done);
            }
            line = async {
                // What a previous turn read past its own ending comes first, in the order it was
                // said. See [`Live::pushed_back`].
                match live.pushed_back.pop_front() {
                    Some(line) => Some(line),
                    None => live.lines.recv().await,
                }
            } => {
                // The process is gone. Whatever it managed to say is already in the turn.
                let Some(line) = line else { break };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue; // non-JSON chatter on stdout is not fatal
                };
                // This turn is over and this is the wait for one last number. Anything the CLI
                // says that is not that number belongs to what comes next.
                if ending && v.get("type").and_then(Value::as_str) != Some("control_response") {
                    held.push(line);
                    continue;
                }
                // Whether a turn is running, ours or one of its own. Read for the boundary rather
                // than for the handshake — what an `init` *carries* is forwarded below with
                // everything else, and a `result` is acted on where it is read.
                live.note_boundary(&v);
                // The CLI declined to pick up where it left off. Nothing has been forwarded and
                // the turn has not begun, so this is not a failure to report — it is a token to
                // throw away, said to the caller who is holding it.
                if borrowed && stale_resume(&v) {
                    return Ok(Outcome::Stale);
                }
                // Every line carries it, and it is what a replacement process resumes into — this
                // one, and, once it has been said out loud, one started a week from now. Compared
                // rather than set once: resuming does not always keep the id it was given, and a
                // conversation stored under the id the CLI has stopped using is one that resumes
                // into a history missing everything since.
                if let Some(id) = v.get("session_id").and_then(|s| s.as_str()).filter(|s| !s.is_empty())
                    && live.session.as_deref() != Some(id)
                {
                    live.session = Some(id.to_string());
                    let _ = tx
                        .send(ProviderEvent::Activity {
                            activity: Activity::Resume { token: id.to_string() },
                        })
                        .await;
                }
                // The model asking *you* something. Handled before the permission path and never
                // by it: policy has no answer to "which of these", and the yes it would give
                // carries none — which the CLI reads as nobody having answered at all.
                if let Some(ask) = claude_control::ask_user_question(&v) {
                    let answers = match &questioner {
                        Some(q) if !abandoned => {
                            let req = QuestionRequest {
                                questions: ask.questions.clone(),
                                conversation: request.conversation.clone(),
                                cwd: request.cwd.clone(),
                            };
                            tokio::select! {
                                biased;
                                // Raced against the cancellation for the same reason a permission
                                // prompt is: a question on screen with nobody at the keyboard is
                                // exactly when somebody reaches for `<Esc>`.
                                () = cancel.cancelled() => QuestionAnswers::dismissed(),
                                answers = q.ask(req) => answers,
                            }
                        }
                        _ => QuestionAnswers::nobody(),
                    };
                    let reply = claude_control::answer(&ask, &answers);
                    if write_line(&mut live.stdin, &reply).await.is_err() {
                        *slot = None;
                        break;
                    }
                    continue;
                }
                // A question, which the CLI is now blocked on. Answered inline: the turn genuinely
                // is waiting on a person, and the transcript's spinner says so.
                if let Some(ask) = claude_control::can_use_tool(&v, &request.cwd) {
                    // Raced against the cancellation, not awaited on its own. A prompt with
                    // nobody at the keyboard is exactly when somebody reaches for `<Esc>`, and a
                    // turn that could only be interrupted between questions could not be
                    // interrupted at all while one was open.
                    let answer = match &asker {
                        // Nobody to ask once the turn has been abandoned, and a question with
                        // nobody behind it is a drain that never finishes.
                        Some(a) if !abandoned => tokio::select! {
                            biased;
                            () = cancel.cancelled() => PermissionAnswer::Deny,
                            answer = a.ask(PermissionRequest {
                                conversation: Some(request.conversation.clone()),
                                ..ask.request.clone()
                            }) => answer,
                        },
                        _ => PermissionAnswer::Deny,
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
                // Anything else the CLI asked for and is now waiting on. Last, so every handler
                // above has had its go, and unconditional, so there is no line it can send that
                // leaves the turn hanging. See [`claude_control::unhandled`].
                if let Some(reply) = claude_control::unhandled(&v) {
                    if write_line(&mut live.stdin, &reply).await.is_err() {
                        *slot = None;
                        break;
                    }
                    continue;
                }
                // How full the context is. Matched by *shape* rather than by request id: several
                // of these are asked over a long turn, they all answer the same question, and the
                // only one that matters is the newest.
                if let Some(ev) = context_usage(&v) {
                    let _ = tx.send(ev).await;
                    if ending {
                        break;
                    }
                    continue;
                }
                for ev in control_failure(&v).into_iter().chain(sse::claude_cli_line(&v, &mut live.state)) {
                    if abandoned {
                        break;
                    }
                    if tx.send(ev).await.is_err() {
                        // The receiver is gone: the turn was abandoned — `<Esc>`, or a switch away
                        // from a conversation whose turn was still running. The *process* is not
                        // abandoned; it holds the conversation and the next turn is about to ask
                        // it something. Which is exactly why this cannot return here.
                        //
                        // The CLI is mid-answer, and everything it has not written yet is still in
                        // the pipe. Returning leaves it there, and `live.lines` outlives the turn —
                        // so the *next* turn reads the rest of this one as its own. That is how an
                        // interrupted `/compact` came back as the answer to the question typed
                        // after it: `Compaction canceled.` under a message about something else,
                        // arriving before the reply it was actually waiting for.
                        //
                        // So the turn stops being forwarded and starts being drained: ask it to
                        // stop, then read to the `result` that ends it, and leave the process at a
                        // turn boundary where the next prompt starts clean. The 5s deadline the
                        // interrupt arms is the bound — past that it is killed, as it already was.
                        abandoned = true;
                        if !interrupted {
                            interrupted = true;
                            let _ = live.control(&json!({"subtype": "interrupt"})).await;
                            giveup
                                .as_mut()
                                .reset(tokio::time::Instant::now() + Duration::from_secs(5));
                        }
                        break;
                    }
                }
                // A turn is over, and the process is not.
                //
                // Not necessarily *this* turn: a `result` closing something the CLI started for
                // itself, or closing the turn ours is queued behind, is one more line to forward
                // and not the end of the answer. Ending here is how a message that had not even
                // been sent yet came back as a turn that said nothing.
                if v.get("type").and_then(|t| t.as_str()) == Some("result") {
                    owed = owed.saturating_sub(1);
                    // Abandoned is the exception, and the reason is the one draining exists for:
                    // there is nobody to give the rest to, so the process is left at the first turn
                    // boundary going rather than the one this turn was promised.
                    if !abandoned && (!said || owed > 0) {
                        continue;
                    }
                    finished = true;
                    // Not when it has been abandoned: there is nobody to tell, and asking would
                    // re-arm the deadline that an interrupt has already pointed at killing the
                    // process — which is the one thing draining exists to avoid.
                    //
                    // And not when it has been interrupted, which is the same sentence about the
                    // same deadline and was the half that was missing. `<Esc>` on a turn the CLI
                    // then stopped politely still left `interrupted` set; this asked one last
                    // question, re-armed the deadline at two seconds, and the branch below read
                    // that expiry as "asked to stop and did not" — so every interrupt ended with
                    // the conversation's CLI killed and the next turn resuming into a fresh one.
                    if !abandoned
                        && !interrupted
                        && !ending
                        && live.control(&json!({"subtype": "get_context_usage"})).await.is_ok()
                    {
                        ending = true;
                        // Bounded, because an install that will not answer must not hold the turn
                        // open. Two seconds is far more than a local answer takes.
                        giveup.as_mut().reset(
                            tokio::time::Instant::now() + Duration::from_secs(2),
                        );
                        continue;
                    }
                    break;
                }
            }
        }
    }

    // Whatever arrived after the answer was already given, left where the next turn reads first.
    if let Some(live) = slot.as_mut() {
        live.pushed_back.extend(held);
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
    Ok(Outcome::Done)
}

/// The CLI refusing to resume a conversation it no longer has.
///
/// It says so in the ordinary way — a `result` line with `is_error` — which is also how it reports
/// every other failure, so the sentence is what tells them apart. Matched loosely on purpose: this
/// only decides whether to try once more without the handle, and being wrong in either direction
/// costs a turn that starts fresh instead of one that fails.
fn stale_resume(v: &Value) -> bool {
    if v.get("type").and_then(Value::as_str) != Some("result")
        || v.get("is_error").and_then(Value::as_bool) != Some(true)
    {
        return false;
    }
    let gone = |s: &str| s.contains("No conversation found");
    v.get("errors")
        .and_then(Value::as_array)
        .is_some_and(|e| e.iter().filter_map(Value::as_str).any(gone))
        || v.get("result").and_then(Value::as_str).is_some_and(gone)
}

/// Read everything the CLI already said while no turn was listening, and keep only the part that is
/// about the conversation rather than about a turn nobody asked for.
///
/// Bounded twice, because this runs on the path that sends every message. Nothing is *waited* for —
/// a zero timeout takes what has already arrived and stops the moment the pipe would block, so a
/// quiet process costs one poll — and a process that has been talking to itself for an hour cannot
/// hold a keystroke open: past the cap the rest is left where it is and read as part of this turn,
/// which is the behaviour this replaces and no worse than it.
///
/// A partly-arrived line is not lost by stopping: `Lines` keeps what it has buffered and the turn
/// loop picks the rest up.
///
/// Never called on a process this turn started. A fresh one has no previous turn to have leftovers
/// from, and it has one thing to say that only the turn loop can act on: a `result` refusing to
/// `--resume`, which is read there as a token to throw away rather than as an answer. Draining that
/// would drop it, and the turn would then wait on a process that had already given up.
async fn drain_between_turns(live: &mut Live, tx: &mpsc::Sender<ProviderEvent>) {
    /// Far more than a turn boundary ever leaves behind, and small enough to be free.
    const CAP: usize = 512;
    for _ in 0..CAP {
        // What a previous turn read past its own ending first, then whatever the reader has
        // collected since. Nothing is *waited* for: both are what has already arrived.
        let line = match live.pushed_back.pop_front() {
            Some(line) => line,
            None => match live.lines.try_recv() {
                Ok(line) => line,
                // Nothing more has arrived, or the process is gone. Either way the turn below is
                // the thing that reports it — it has a `tx` nobody has closed and an error path
                // already.
                Err(_) => return,
            },
        };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        // Counted even though most of it is dropped. A turn nobody asked for that has *ended* is
        // one the prompt below is not queued behind; one still running is, and the turn loop has to
        // wait for its ending as well as its own. See `owed`.
        live.note_boundary(&v);
        // Only what the CLI says about the *conversation*. `type: "system"` is the family that
        // carries it — the live set of background tasks, a task's status, the report a finished one
        // files — and `sse::claude_cli_line` already knows which of those are worth an event and
        // which are noise. Everything else here belongs to the unasked turn: its `init`, its
        // assistant blocks, the tool round it ran, and the `result` that would otherwise end this
        // one before it started.
        if v.get("type").and_then(Value::as_str) != Some("system") {
            continue;
        }
        // Kept for the same reason the turn loop keeps it, and it is why this cannot simply skip
        // what it does not forward: an unasked turn opens with an `init`, and `init` is where a new
        // id for this conversation is announced. Missed here, the next process `--resume`s into a
        // history that stops at the last turn somebody asked for.
        if let Some(id) = v.get("session_id").and_then(Value::as_str).filter(|s| !s.is_empty())
            && live.session.as_deref() != Some(id)
        {
            live.session = Some(id.to_string());
            let _ = tx
                .send(ProviderEvent::Activity {
                    activity: Activity::Resume { token: id.to_string() },
                })
                .await;
        }
        for ev in sse::claude_cli_line(&v, &mut live.state) {
            if tx.send(ev).await.is_err() {
                return;
            }
        }
    }
}

/// How full the CLI says its context is.
///
/// The one number neosh could not have worked out for itself, and the reason for asking. What a
/// turn's usage reports is the size of *one request*; what fills a window is the conversation the
/// agent is holding — which it prunes, compacts and reorders on its own, and about which a client
/// counting its own messages is guessing.
///
/// Recognised by shape rather than by request id, because over a long turn several are asked and
/// any of their answers is a good one.
///
/// The window comes with it, and that half matters just as much. A vendor CLI's window is whatever
/// that CLI decided: `claude` running `claude-haiku-4-5` answers `maxTokens: 200000`, while neosh's
/// own catalogue entry for the selected model said a million. Dividing a half-known numerator by an
/// invented denominator is how a meter reads `2%` where the agent itself would say `8%`.
fn context_usage(v: &Value) -> Option<ProviderEvent> {
    if v.get("type").and_then(Value::as_str) != Some("control_response") {
        return None;
    }
    let r = v.pointer("/response/response")?;
    let used = r.get("totalTokens").and_then(Value::as_u64)?;
    let total = r.get("maxTokens").and_then(Value::as_u64).filter(|t| *t > 0)?;
    Some(ProviderEvent::Activity { activity: Activity::Context { used, total } })
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

/// What the reader task needs in order to notice a turn nobody asked for.
///
/// It reads the pipe for the life of the process, which means it is the only thing looking at it
/// between turns — and between turns is exactly when the CLI answers its own backgrounded work.
/// See [`super::Unasked`].
#[derive(Clone)]
struct Watch {
    unread: Arc<Unread>,
    conversation: SessionId,
    sink: Option<Arc<dyn super::Unasked>>,
}

impl Watch {
    /// One line off the pipe, looked at only when nobody else is looking.
    ///
    /// `system`/`init` is what the CLI says at the top of every turn it starts. Parsed rather than
    /// matched as a substring, and only in the detached case, so an attached turn pays nothing.
    fn opens_a_turn(&self, line: &str) -> bool {
        if self.sink.is_none() {
            return false;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { return false };
        v.get("type").and_then(Value::as_str) == Some("system")
            && v.get("subtype").and_then(Value::as_str) == Some("init")
    }

    fn reading(&self) -> bool {
        self.unread.attached.load(std::sync::atomic::Ordering::SeqCst) > 0
    }

    /// The live set of background tasks, if that is what this line carries.
    ///
    /// Read in the detached case only, so an attached turn pays nothing for it — the same trade
    /// [`Self::opens_a_turn`] makes.
    ///
    /// The substring is a gate and not the decision: what a line *is* still comes from the parser
    /// below, and all this says is that a line without those characters in it cannot be one. It
    /// earns its place because this sits between the CLI's stdout and the channel every turn reads
    /// from — a second whole-line parse per line, on the one path where added latency shifts which
    /// side of a turn boundary a line lands on.
    fn level_in(&self, line: &str) -> Option<Vec<neosh_proto::BackgroundTask>> {
        self.sink.as_ref()?;
        if !line.contains("background_tasks_changed") {
            return None;
        }
        crate::sse::background_tasks(&serde_json::from_str::<Value>(line).ok()?)
    }

    /// Say what is running, when nothing is reading the pipe it was said on.
    ///
    /// A turn's `result` is not the end of the process, and a detached shell is the whole reason:
    /// it finishes minutes later, the CLI says so, and until now the only thing that could carry
    /// that to the workspace was the turn the CLI usually — but not always — runs at itself
    /// afterwards. Said here it does not depend on that. The set is a level, so saying it twice is
    /// saying it once, and the turn loop goes on forwarding its own copy in order.
    ///
    /// Nothing is said while somebody is reading, and the check is after the line has gone into the
    /// pipe so that a turn which attached in between is the one that reports it: two paths into one
    /// field can arrive out of order, and out of order here means the *older* set wins and the mark
    /// is wrong until the next change — which, for a conversation nobody goes back to, is forever.
    fn background(&self, tasks: Vec<neosh_proto::BackgroundTask>) {
        if self.reading() {
            return;
        }
        if let Some(sink) = &self.sink {
            sink.background(&self.conversation, tasks);
        }
    }

    /// The process is gone, so nothing it was holding is running any more.
    ///
    /// Whatever it had reported dies with it: the shells it detached are its children, the account
    /// of them was only ever in its head, and nothing that comes after can ask. Without this a
    /// conversation whose CLI was killed — crashed, OOM, `kill -9` — wears a "still working on
    /// something" mark until somebody happens to send it another message, which for the one it was
    /// killed in the middle of is exactly the thing they are not about to do.
    ///
    /// Said even when the process is being replaced on purpose, where the fresh one says the same
    /// thing a moment later. Empty twice is empty.
    fn ended(&self) {
        if let Some(sink) = &self.sink {
            sink.background(&self.conversation, Vec::new());
        }
    }

    /// A turn has begun. Report it if nothing is reading, and otherwise leave the note that
    /// whoever is reading clears by getting to it. See [`Unread`].
    fn began(&self) {
        if self.reading() {
            self.unread.pending.store(true, std::sync::atomic::Ordering::SeqCst);
            // Checked again, because the last reader may have gone between the two — and it takes
            // what is pending with it on the way out, so a note left after that lands nowhere.
            if self.reading() {
                return;
            }
            if !self.unread.pending.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
        }
        self.report();
    }

    fn report(&self) {
        if let Some(sink) = &self.sink {
            sink.began(&self.conversation);
        }
    }
}

impl Live {
    /// Start one, resuming a conversation a previous process was holding.
    async fn spawn(
        program: &str,
        launch: &Launch,
        model: &str,
        resume: Option<&str>,
        watch: Watch,
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

        // Stdout is read for the life of the *process*, not the length of a turn. See
        // [`Live::lines`] for what is on the other end of that distinction. A read error is left
        // to the `None` it is immediately followed by: the interesting half of a broken pipe is
        // the exit status and the stderr, and both are collected where the turn ends.
        let (line_tx, lines) = mpsc::unbounded_channel();
        let unread = watch.unread.clone();
        tokio::spawn(async move {
            let mut l = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = l.next_line().await {
                let opens = watch.opens_a_turn(&line);
                // Read before the send and reported after it, because the send is what moves the
                // line and what gives a turn the chance to answer for it instead.
                let level = watch.level_in(&line);
                if line_tx.send(line).is_err() {
                    break;
                }
                if let Some(tasks) = level {
                    watch.background(tasks);
                }
                // After the send, never before: the workspace answers this by opening a turn to
                // read, and a turn that arrives before the line it was opened for finds an empty
                // pipe and closes again.
                if opens {
                    watch.began();
                }
            }
            // Stdout closed, which for a process reading stdin forever means the process is over.
            watch.ended();
        });

        Ok(Self {
            child,
            stdin,
            lines,
            busy: false,
            unread,
            pushed_back: std::collections::VecDeque::new(),
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

    /// Note what one line says about whether a turn is running.
    ///
    /// `system`/`init` opens a turn and `result` closes one, for turns the CLI starts of its own
    /// accord as much as for ours. A flag rather than a count on purpose: a count that drifts
    /// upward is a turn waiting for an ending that never comes, where a flag is wrong by at most
    /// one ending and an install that stopped emitting `init` would simply put this back where it
    /// was.
    fn note_boundary(&mut self, v: &Value) {
        match v.get("type").and_then(Value::as_str) {
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
                self.busy = true;
                // Somebody is reading it, so it is not news. See [`Unread::pending`].
                self.unread.pending.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            Some("result") => self.busy = false,
            _ => {}
        }
    }

    /// Ask the running session to change something about itself, or to say something about itself.
    ///
    /// Hands back the id it used. Most of these are fire-and-forget — ordering on one pipe is
    /// enough, and the answer to `set_model` is only interesting when it is a refusal, which
    /// arrives on the same stdout as everything else. The one caller that keeps the id is the one
    /// waiting for an answer with a number in it.
    async fn control(&mut self, request: &Value) -> Result<String, String> {
        let id = neosh_proto::RequestId::new().to_string();
        let message = json!({
            "type": "control_request",
            "request_id": id,
            "request": request,
        });
        write_line(&mut self.stdin, &message)
            .await
            .map(|()| id)
            .map_err(|e| format!("could not reach the running claude session: {e}"))
    }

    /// Put a question to it.
    async fn say(&mut self, prompt: Value) -> Result<(), String> {
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

    fn set_question_asker(&self, asker: Arc<dyn QuestionAsker>) {
        *self.questioner.lock().expect("questioner lock poisoned") = Some(asker);
    }

    fn set_permission_asker(&self, asker: Arc<dyn PermissionAsker>) {
        *self.asker.lock().expect("asker lock poisoned") = Some(asker);
    }

    fn set_unasked(&self, sink: Arc<dyn super::Unasked>) {
        *self.unasked.lock().expect("unasked lock poisoned") = Some(sink);
    }

    fn listen_only(&self, conversation: &SessionId) {
        self.slot(conversation).listen.store(true, std::sync::atomic::Ordering::Relaxed);
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
            resume: None,
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

    /// Plain text stays a plain *string* on the wire, not a one-element array. Both are the same
    /// message to the CLI, and this is the one every reader of that pipe expects to see.
    #[test]
    fn a_message_with_no_picture_in_it_goes_as_it_always_did() {
        let msgs =
            vec![Message { role: Role::User, content: vec![ContentBlock::Text { text: "hi".into() }] }];
        assert!(ClaudeCliProvider::prompt_from(&msgs).is_string());
    }

    #[test]
    fn a_picture_goes_as_the_block_the_api_takes_and_goes_first() {
        let dir = std::env::temp_dir().join(format!("neosh-cli-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let png = dir.join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n-not-really").expect("write");

        let msgs = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Image {
                    path: png.display().to_string(),
                    media_type: "image/png".into(),
                },
                ContentBlock::Text { text: "what is this".into() },
            ],
        }];
        let v = ClaudeCliProvider::prompt_from(&msgs);
        let blocks = v.as_array().expect("an array once there is more than text in it");
        assert_eq!(blocks[0]["type"], "image", "the picture the sentence points at comes first");
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert!(
            blocks[0]["source"]["data"].as_str().is_some_and(|d| d.starts_with("iVBORw0KGgo")),
            "and it is the actual bytes, base64\n{v}"
        );
        assert_eq!(blocks[1]["text"], "what is this");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A workspace directory can be cleared out between the day a conversation was had and the day
    /// it is reopened. The rest of the message is still a question worth asking.
    #[test]
    fn an_image_whose_file_has_gone_is_dropped_rather_than_failing_the_turn() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Image {
                    path: "/definitely/not/here.png".into(),
                    media_type: "image/png".into(),
                },
                ContentBlock::Text { text: "still a question".into() },
            ],
        }];
        assert_eq!(
            ClaudeCliProvider::prompt_from(&msgs),
            Value::String("still a question".into()),
        );
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

        let dir = std::env::temp_dir().join(format!("neosh-cwd-{}", std::process::id()));
        let work = dir.join("work");
        std::fs::create_dir_all(&work).expect("dirs");
        let script = dir.join("fake-claude");
        // Written by a child rather than by us, and the reason is the whole of
        // `tests/support/mod.rs`: a descriptor this process holds open for writing is a descriptor
        // a sibling test's `spawn` forks a copy of, and the `execve` here then fails with
        // `ETXTBSY`. Nothing we never opened can be inherited.
        let wrote = std::process::Command::new("/bin/sh")
            .args(["-c", r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#, "neosh-test-write"])
            .arg("#!/bin/sh\npwd > \"$(dirname \"$0\")/where\"\n")
            .arg(&script)
            .status()
            .expect("a shell to write the script");
        assert!(wrote.success(), "writing the stand-in failed: {wrote}");

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

    /// A turn the CLI started for itself must not end the turn neosh asked for.
    ///
    /// The failure, from a real transcript: the agent backgrounds a command and says it will
    /// report back. The command finishes, `claude` enqueues a notification to itself and answers
    /// it — a whole turn of `init`, text and `result` on a stdout nothing was reading. Type
    /// anything and two things went wrong at once: that turn appeared all at once as though it
    /// were the reply, and its `result` ended the turn before the CLI had even taken the message
    /// off its own queue. What you typed was never answered and nothing said so.
    ///
    /// A real child rather than a mock, for the same reason the directory test uses one: the whole
    /// bug lives in what is on the pipe and when, and nothing short of a process writing to one
    /// would have caught it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_turn_the_cli_started_for_itself_does_not_end_the_one_we_asked_for() {
        use futures::StreamExt;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("neosh-unasked-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dirs");
        let script = dir.join("fake-claude");
        // Turn one is answered, and then — with nobody having asked — a turn of its own *begins*
        // and does not finish: `init`, and the rest of it only once the next prompt has been read.
        // That is the case the drain cannot cover, because the turn is still going when the prompt
        // goes in and the CLI queues it behind them.
        std::fs::write(
            &script,
            r#"#!/bin/sh
n=0
spoke=
pending=
while IFS= read -r line; do
  case "$line" in
    *control_request*)
      id=`printf '%s' "$line" | sed 's/.*"request_id":"\([^"]*\)".*/\1/'`
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"totalTokens":10,"maxTokens":100}}}\n' "$id"
      if [ "$n" = 1 ] && [ -z "$spoke" ]; then
        spoke=1
        pending=1
        printf '{"type":"system","subtype":"init","session_id":"s","slash_commands":[]}\n'
      fi
      continue
      ;;
  esac
  if [ -n "$pending" ]; then
    pending=
    printf '{"type":"result","subtype":"success","is_error":false,"session_id":"s","result":"the background command finished"}\n'
  fi
  n=`expr $n + 1`
  printf '{"type":"system","subtype":"init","session_id":"s","slash_commands":[]}\n'
  printf '{"type":"result","subtype":"success","is_error":false,"session_id":"s","result":"answer %s"}\n' "$n"
done
"#,
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let p = ClaudeCliProvider::new(script.display().to_string());
        let said = |evs: Vec<ProviderEvent>| {
            evs.into_iter()
                .filter_map(|e| match e {
                    ProviderEvent::TextDelta { text, .. } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" / ")
        };

        let first: Vec<_> =
            p.stream(&inst(), req("claude-opus-5"), CancellationToken::new()).collect().await;
        assert_eq!(said(first), "answer 1");

        // The same conversation, so the same process — and a turn of the CLI's own still running on
        // it when the prompt goes in.
        let second: Vec<_> =
            p.stream(&inst(), req("claude-opus-5"), CancellationToken::new()).collect().await;
        assert_eq!(
            said(second),
            "the background command finished / answer 2",
            "the ending that arrives first belongs to the turn this one was queued behind, and the \
             message that was actually sent is still answered after it"
        );

        p.shutdown(&neosh_proto::SessionId::from("test"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nobody asked, so the workspace is told — and the turn it opens listens without speaking.
    ///
    /// The other half of the failure above. Reading the unasked-for turn at the top of the *next*
    /// message stops it being lost and stops it eating that message's answer, but it is still
    /// invisible until somebody types: the agent says it will report back when the build lands,
    /// the build lands, and the screen never changes. So the driver says so the moment it happens,
    /// and the turn opened for it says nothing of its own — one that spoke would be asking a
    /// second question on top of the one being answered.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_agent_that_speaks_unprompted_is_reported_and_then_listened_to() {
        use futures::StreamExt;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug, Default)]
        struct Heard(AtomicUsize);
        impl super::super::Unasked for Heard {
            fn began(&self, _conversation: &SessionId) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn background(&self, _conversation: &SessionId, _tasks: Vec<neosh_proto::BackgroundTask>) {}
        }

        let dir = std::env::temp_dir().join(format!("neosh-unprompted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dirs");
        let script = dir.join("fake-claude");
        // Turn one is answered and the context question that closes it is answered too — and only
        // then, with the turn over and nothing reading, does a turn nobody asked for begin. The
        // second half of it arrives a moment later, so the listening turn has something to wait
        // for rather than a pipe that is already empty.
        std::fs::write(
            &script,
            r#"#!/bin/sh
n=0
spoke=
while IFS= read -r line; do
  case "$line" in
    *control_request*)
      id=`printf '%s' "$line" | sed 's/.*"request_id":"\([^"]*\)".*/\1/'`
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"totalTokens":10,"maxTokens":100}}}\n' "$id"
      if [ "$n" = 1 ] && [ -z "$spoke" ]; then
        spoke=1
        printf '{"type":"system","subtype":"init","session_id":"s","slash_commands":[]}\n'
        sleep 1
        printf '{"type":"result","subtype":"success","is_error":false,"session_id":"s","result":"the background command finished"}\n'
      fi
      continue
      ;;
  esac
  n=`expr $n + 1`
  printf '{"type":"system","subtype":"init","session_id":"s","slash_commands":[]}\n'
  printf '{"type":"result","subtype":"success","is_error":false,"session_id":"s","result":"answer %s"}\n' "$n"
done
"#,
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let p = ClaudeCliProvider::new(script.display().to_string());
        let heard = Arc::new(Heard::default());
        crate::drivers::AgentDriver::set_unasked(&p, heard.clone());
        let conversation = SessionId::from("test");
        let said = |evs: Vec<ProviderEvent>| {
            evs.into_iter()
                .filter_map(|e| match e {
                    ProviderEvent::TextDelta { text, .. } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" / ")
        };

        let first: Vec<_> =
            p.stream(&inst(), req("claude-opus-5"), CancellationToken::new()).collect().await;
        assert_eq!(said(first), "answer 1");

        // Nothing is attached now, so the next thing the CLI says on its own is news.
        for _ in 0..200 {
            if heard.0.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(heard.0.load(Ordering::SeqCst), 1, "the workspace was never told");

        // What the host does with that: mark the next turn as one that only listens, and open it.
        crate::drivers::AgentDriver::listen_only(&p, &conversation);
        let mut listening = req("claude-opus-5");
        listening.messages.clear();
        let heard_out: Vec<_> =
            p.stream(&inst(), listening, CancellationToken::new()).collect().await;
        assert_eq!(
            said(heard_out),
            "the background command finished",
            "a turn opened to listen hears the rest of what was already being said"
        );
        // And says nothing while doing it: a prompt down that pipe would have come back "answer 2".
        assert_eq!(heard.0.load(Ordering::SeqCst), 1, "and started nothing of its own");

        p.shutdown(&conversation);
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

    /// The user's sequence: a turn is interrupted mid-answer, and the next question is asked on
    /// the same conversation.
    ///
    /// Two things have to hold, and only one of them used to. Whatever the CLI was still writing
    /// must not arrive under the new question — that is the drain, and it worked. And the
    /// conversation must still be *the same CLI*: an interrupt asks the process to stop, it stops,
    /// and a turn that stopped politely is not a turn to kill. It was killed, every time, because
    /// the last thing an ending turn does is ask for one more number and the deadline that arms is
    /// the same one the interrupt armed.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_interrupted_answer_does_not_arrive_under_the_next_question() {
        use futures::StreamExt;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("neosh-cut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dirs");
        let script = dir.join("fake-claude");
        // A CLI that answers slowly, in a writer of its own, so a control request can arrive in
        // the middle of an answer the way it does against the real one. `interrupt` stops the
        // writer, and the turn is closed with a `result` — which is what `claude` does.
        std::fs::write(
            &script,
            r#"#!/bin/sh
d=`dirname "$0"`
echo "$$" >> "$d/spawns"
n=0
while IFS= read -r line; do
  case "$line" in
    *'"subtype":"interrupt"'*)
      : > "$d/stop"
      id=`printf '%s' "$line" | sed 's/.*"request_id":"\([^"]*\)".*/\1/'`
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      continue
      ;;
    *control_request*)
      id=`printf '%s' "$line" | sed 's/.*"request_id":"\([^"]*\)".*/\1/'`
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"totalTokens":10,"maxTokens":100}}}\n' "$id"
      continue
      ;;
  esac
  n=`expr $n + 1`
  rm -f "$d/stop"
  printf '{"type":"system","subtype":"init","session_id":"s","slash_commands":[]}\n'
  m=$n
  (
    i=1
    while [ $i -le 6 ]; do
      if [ -f "$d/stop" ]; then break; fi
      printf '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"T%s-%s "}}}\n' "$m" "$i"
      sleep 1
      i=`expr $i + 1`
    done
    printf '{"type":"result","subtype":"success","is_error":false,"session_id":"s","result":""}\n'
  ) &
done
"#,
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let p = ClaudeCliProvider::new(script.display().to_string());
        let said = |evs: &[ProviderEvent]| {
            evs.iter()
                .filter_map(|e| match e {
                    ProviderEvent::TextDelta { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        };

        // Turn one, cut off the way `<Esc>` cuts one off: the token is cancelled and the consumer
        // lets go of the stream at once.
        let cancel = CancellationToken::new();
        let mut first = p.stream(&inst(), req("claude-opus-5"), cancel.clone());
        let mut got = Vec::new();
        while let Some(ev) = first.next().await {
            let stop = matches!(&ev, ProviderEvent::TextDelta { text, .. } if text.starts_with("T1-2"));
            got.push(ev);
            if stop {
                break;
            }
        }
        assert!(said(&got).contains("T1-1"), "the first turn said something: {:?}", said(&got));
        cancel.cancel();
        drop(first);

        // Long enough for the interrupted turn to have been drained, and for anything it was
        // still writing to have been written.
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Turn two, on the same conversation and therefore the same process.
        let second: Vec<_> =
            p.stream(&inst(), req("claude-opus-5"), CancellationToken::new()).collect().await;
        let text = said(&second);
        assert!(
            !text.contains("T1-"),
            "the interrupted answer arrived under the next question: {text:?}"
        );
        assert!(text.contains("T2-"), "and the next question was answered: {text:?}");
        let spawned = std::fs::read_to_string(dir.join("spawns")).unwrap_or_default();
        assert_eq!(
            spawned.lines().count(),
            1,
            "the conversation kept the CLI it started with: {spawned:?}"
        );

        p.shutdown(&neosh_proto::SessionId::from("test"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Interrupting a turn does not cost the conversation its agent.
    ///
    /// The last thing an ending turn does is ask the CLI how full its context is, on a two-second
    /// deadline — an extra, and one an install is perfectly entitled to answer with nothing useful
    /// or not at all. On an *interrupted* turn that deadline was the same timer the interrupt had
    /// armed, and its expiry was read as "asked to stop and did not": the process was killed, and
    /// the next question in that conversation started a second `claude` and resumed into it.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_interrupt_does_not_kill_a_cli_that_stopped_when_it_was_asked() {
        use futures::StreamExt;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("neosh-keepcli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dirs");
        let script = dir.join("fake-claude");
        // It stops the moment it is asked to and says so — and it answers control requests without
        // token counts, which is all an install that does not report context usage can do. That is
        // what leaves the closing question unanswered and the deadline to expire.
        std::fs::write(
            &script,
            r#"#!/bin/sh
d=`dirname "$0"`
echo "$$" >> "$d/spawns"
n=0
while IFS= read -r line; do
  case "$line" in
    *'"subtype":"interrupt"'*)
      : > "$d/stop"
      id=`printf '%s' "$line" | sed 's/.*"request_id":"\([^"]*\)".*/\1/'`
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      continue
      ;;
    *control_request*)
      id=`printf '%s' "$line" | sed 's/.*"request_id":"\([^"]*\)".*/\1/'`
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$id"
      continue
      ;;
  esac
  n=`expr $n + 1`
  rm -f "$d/stop"
  printf '{"type":"system","subtype":"init","session_id":"s","slash_commands":[]}\n'
  m=$n
  (
    i=1
    while [ $i -le 40 ]; do
      if [ -f "$d/stop" ]; then break; fi
      printf '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"T%s-%s "}}}\n' "$m" "$i"
      sleep 0.2
      i=`expr $i + 1`
    done
    printf '{"type":"result","subtype":"success","is_error":false,"session_id":"s","result":""}\n'
  ) &
done
"#,
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let p = ClaudeCliProvider::new(script.display().to_string());
        let cancel = CancellationToken::new();
        let mut first = p.stream(&inst(), req("claude-opus-5"), cancel.clone());
        while let Some(ev) = first.next().await {
            if matches!(&ev, ProviderEvent::TextDelta { text, .. } if text.starts_with("T1-1")) {
                break;
            }
        }
        // `<Esc>`: the token is cancelled and the consumer lets go, exactly as the turn loop does.
        cancel.cancel();
        drop(first);

        // Longer than either deadline the turn can arm, so a process that was going to be killed
        // has been.
        tokio::time::sleep(Duration::from_secs(6)).await;

        let second: Vec<_> =
            p.stream(&inst(), req("claude-opus-5"), CancellationToken::new()).collect().await;
        assert!(
            second.iter().any(|e| matches!(e, ProviderEvent::TextDelta { text, .. } if text.starts_with("T2-"))),
            "the next question is answered by the CLI that already knows this conversation"
        );
        let spawned = std::fs::read_to_string(dir.join("spawns")).unwrap_or_default();
        assert_eq!(
            spawned.lines().count(),
            1,
            "and it is the same one: {spawned:?}"
        );

        p.shutdown(&neosh_proto::SessionId::from("test"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A one-shot generation borrows the model and gives it back.
    ///
    /// A branch name or a thread title is nobody's conversation — [`TurnRequest::conversation`] is
    /// empty and says so. Keyed into the live map by that empty id, every one of them shared a
    /// process that nothing ever closed: a second `claude`, with the same flags as the one
    /// answering you, running for as long as the workspace did.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_one_shot_generation_does_not_leave_a_cli_behind() {
        use futures::StreamExt;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("neosh-oneshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dirs");
        let script = dir.join("fake-claude");
        // It answers, and then reports when its stdin is closed — which is how a `--print` session
        // is told there are no more messages coming, and the only way to see that it was told.
        std::fs::write(
            &script,
            r#"#!/bin/sh
d=`dirname "$0"`
while IFS= read -r line; do
  case "$line" in
    *control_request*)
      id=`printf '%s' "$line" | sed 's/.*"request_id":"\([^"]*\)".*/\1/'`
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"totalTokens":10,"maxTokens":100}}}\n' "$id"
      continue
      ;;
  esac
  printf '{"type":"system","subtype":"init","session_id":"s","slash_commands":[]}\n'
  printf '{"type":"result","subtype":"success","is_error":false,"session_id":"s","result":"a-name"}\n'
done
echo done > "$d/closed"
"#,
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let p = ClaudeCliProvider::new(script.display().to_string());
        let mut one_shot = req("claude-opus-5");
        one_shot.conversation = neosh_proto::SessionId::from("");
        assert!(one_shot.is_one_shot(), "that is what an empty conversation means");
        let evs: Vec<_> = p.stream(&inst(), one_shot, CancellationToken::new()).collect().await;
        assert!(
            evs.iter().any(|e| matches!(e, ProviderEvent::TextDelta { text, .. } if text == "a-name")),
            "it still answers"
        );

        for _ in 0..200 {
            if dir.join("closed").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(dir.join("closed").exists(), "and the process it borrowed was given back");
        let _ = std::fs::remove_dir_all(&dir);
    }

}
