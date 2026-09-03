//! ASCP — the Agent Swarm Control Protocol.
//!
//! One person, several computers, agents running on all of them. ASCP is what makes that one
//! workspace: every node knows what every other node is running, and can ask a node to do something
//! with an agent it owns.
//!
//! # What it is not
//!
//! **It does not move agents.** An agent belongs to the node it was started on, for the whole of
//! its life. Its files are there, its shell is there, its credentials are there — a conversation
//! that could migrate would be a conversation whose `cwd` means something different afterwards.
//! What travels is a *description* of the agent and *requests* addressed to its owner, and the
//! owner may always refuse.
//!
//! **It does not do NAT traversal.** ASCP is an ordinary TCP protocol; on a LAN it needs nothing,
//! and across the internet you run an overlay network — Tailscale, Nebula, NetBird, ZeroTier —
//! underneath it. Hole-punching is a hard problem that several mature projects have already solved,
//! and an agent protocol that reimplemented it would be doing their job badly instead of its own.
//!
//! **It does not trust the network.** An overlay gets packets to you; it does not decide who may
//! steer your agent. Every node has an ed25519 keypair, the handshake is a mutual challenge, and a
//! peer is authorised by its public key being in a list you wrote.
//!
//! # Shape
//!
//! A connection is symmetric once established: either side may send anything. Four families, and
//! they are deliberately different in cost.
//!
//! | Family | Volume | Always on |
//! |---|---|---|
//! | [`Presence`](AscpMessage::Presence) | one per node per heartbeat | yes |
//! | [`Inventory`](AscpMessage::Inventory) | one per change | yes |
//! | [`Command`](AscpMessage::Command) | one per keystroke that means something | on demand |
//! | [`Stream`](AscpMessage::Stream) | one per token | **only while subscribed** |
//!
//! That last row is the whole performance argument. Twenty machines streaming every token to
//! everyone is a great deal of traffic to render a board nobody is reading. Inventory is cheap and
//! unconditional; a stream is something you ask for when you open a conversation and drop when you
//! leave it.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::agent::{Message, PermissionDecision, StopReason, Usage};
use crate::ids::SessionId;
use crate::provider::Activity;

/// The version of these message types.
///
/// Bumped on any change that an older peer could misread. The handshake exchanges it and refuses
/// rather than negotiating down: a protocol that silently degrades is one where a missing feature
/// looks like a bug in the other machine.
pub const ASCP_VERSION: u32 = 0;

/// The default port. `77` for the two letters, `17` because something had to be.
pub const ASCP_PORT: u16 = 7717;

/// A node's public identity: the hex ed25519 public key.
///
/// The key *is* the name. There is no registry, no certificate authority and nothing to revoke
/// centrally — you authorise a peer by putting its id in your own list, and you de-authorise it by
/// taking it out. For a workspace that is one person's several computers, a CA would be
/// infrastructure to run in exchange for nothing.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
#[ts(export)]
pub struct NodeId(pub String);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl NodeId {
    /// The first eight characters, for anywhere a full key would be noise.
    ///
    /// Never for comparison. Two nodes with a colliding prefix is a thing you cannot rule out, and
    /// "the short form was enough" is how that becomes a security property rather than a display
    /// detail.
    pub fn short(&self) -> &str {
        let n = self.0.len().min(8);
        &self.0[..n]
    }
}

/// What a node says about itself.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct NodeInfo {
    pub id: NodeId,
    /// What to call it on screen — the hostname unless the user chose something.
    ///
    /// Advisory, and not unique: two machines may both be called `laptop`. Anything that has to be
    /// right uses [`NodeId`].
    pub name: String,
    /// `macos`, `linux`, `windows`. For the icon and for "why is that one behaving differently".
    pub os: String,
    /// The neosh version, so a board can say which machine is behind.
    pub version: String,
}

/// How two machines recognise the same project.
///
/// The hard part of showing "this project is on three computers" is that the path is not the same
/// on any of them: `/Users/me/dev/neosh`, `/home/me/src/neosh`, `C:\code\neosh`. Comparing paths
/// answers the wrong question, and comparing directory names says two unrelated `api` directories
/// are one project.
///
/// So the key is the repository's origin URL, normalised — `git:github.com/neoswarm/neosh` — which
/// is the same string on every machine that cloned it, including through different protocols
/// (`git@github.com:…`, `https://github.com/…`). A directory that is not a checkout falls back to
/// `dir:<basename>`, which is weak on purpose: it is a guess, and a guess about identity should be
/// visibly a guess rather than quietly wrong.
///
/// Worktrees of one repository share a key. That is deliberate — they are the same project, checked
/// out twice — and it is what lets a board say "neosh: on mac-studio and on linux-box" rather than
/// listing four rows that are all really one thing.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[ts(export)]
pub struct ProjectKey(pub String);

impl std::fmt::Display for ProjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl ProjectKey {
    /// From a git remote URL. `None` when there is no remote to go on.
    ///
    /// Normalisation strips the transport, any credentials, the port, and a trailing `.git`, so
    /// every way of writing the same repository lands on one string. It does *not* lowercase the
    /// path: git forges differ on whether they care, and folding case would merge two repositories
    /// that a case-sensitive host considers separate.
    pub fn from_remote(url: &str) -> Option<Self> {
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        // `git@host:owner/repo` — scp-like syntax, which is not a URL and has to be handled first.
        let rest = if let Some((head, tail)) = url.split_once(':')
            && !head.contains('/')
            && !tail.starts_with("//")
        {
            let host = head.rsplit('@').next().unwrap_or(head);
            format!("{host}/{}", tail.trim_start_matches('/'))
        } else {
            let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
            // Credentials, if somebody's remote carries them. They are not part of the identity,
            // and keeping them would make one clone a different project from another.
            let no_creds = after_scheme.rsplit('@').next().unwrap_or(after_scheme);
            // A port is not part of the identity either: the same repository reached on 22 and on
            // 2222 is the same repository.
            match no_creds.split_once('/') {
                Some((host, path)) => {
                    let host = host.split_once(':').map(|(h, _)| h).unwrap_or(host);
                    format!("{host}/{path}")
                }
                None => no_creds.to_string(),
            }
        };
        let cleaned = rest.trim_end_matches('/').trim_end_matches(".git").trim_end_matches('/');
        if cleaned.is_empty() {
            return None;
        }
        Some(Self(format!("git:{cleaned}")))
    }

    /// The fallback for a directory that is not a checkout.
    pub fn from_dir_name(name: &str) -> Self {
        Self(format!("dir:{name}"))
    }

    /// Whether this key is a real repository identity rather than a guess from a directory name.
    ///
    /// Worth asking before telling somebody two machines have the same project: for a `git:` key
    /// that is a fact, and for a `dir:` key it is a coincidence of naming that may well be wrong.
    pub fn is_certain(&self) -> bool {
        self.0.starts_with("git:")
    }
}

/// What an agent is doing, coarsely enough to be worth sending on every change.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AgentState {
    #[default]
    Idle,
    /// A turn is in flight.
    Running,
    /// Stopped, waiting for somebody to answer a permission prompt. Distinct from `Running`
    /// because it is the state that needs a person, and a board that showed it as "working" would
    /// be a board you learn to ignore while it waits for you.
    Blocked,
    /// The last turn ended badly. Cleared when the next one starts.
    Failed,
}

/// One agent, as another node sees it.
///
/// A description, not a handle. Everything here is for showing and for deciding what to ask; acting
/// on it means sending a [`Command`](AscpMessage::Command) to the node that owns it.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct AgentSummary {
    /// The conversation id, which is unique on its own node and nowhere else. Address an agent as
    /// `(node, id)`; two machines restoring from the same backup would otherwise be one collision
    /// away from steering each other's work.
    pub session: SessionId,
    pub project: ProjectKey,
    /// What that node calls the project — `neosh`, or `neosh · fix/thing` for a worktree.
    pub project_name: String,
    /// The working directory **on the owning node**. Shown, never opened: it is a path in somebody
    /// else's filesystem and may not exist here at all.
    pub cwd: String,
    /// The title, or the opening of the first message. Computed by the owner so every node agrees.
    pub label: String,
    pub state: AgentState,
    pub message_count: u32,
    /// The model the owner will use for the next turn, for a board that says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Seconds since the epoch, when the running turn began. Only meaningful while `Running`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null")]
    pub turn_started_at: Option<i64>,
    #[ts(type = "number")]
    pub updated_at: i64,
    #[serde(default)]
    pub usage: Usage,
}

/// What a node can be asked to do with an agent it owns.
///
/// Every one is a *request*. The owner decides — it may be busy, the conversation may have been
/// deleted a moment ago, or the person sitting at it may have said no to remote control entirely.
/// A command that could not be refused would make every node an administrator of every other one.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "command", rename_all = "snake_case")]
#[ts(export)]
pub enum AgentCommand {
    /// Say something to the agent. While a turn is running this steers it, exactly as typing here
    /// would: the message is held and taken in at the next gap.
    Send { text: String },
    /// Ask the turn to stop. The conversation survives it.
    Interrupt,
    /// Answer a permission prompt.
    ///
    /// The sharpest thing in this protocol, and the reason [`NodeCapabilities::accepts_approvals`]
    /// exists: approving a tool call on another machine authorises a write to *that machine's*
    /// filesystem. Sound when the machines are all yours, and something to be able to say no to.
    Approve { decision: PermissionDecision },
    /// Change the model for the next turn, and tell a running one.
    SetModel { instance: String, model: String },
    Rename { title: Option<String> },
    Archive { archived: bool },
    /// Start a conversation on that node. `cwd` is a path **on the owner**, which the caller learnt
    /// from an [`AgentSummary`] or from [`NodeCapabilities::projects`] rather than inventing.
    NewSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
}

/// One checkout a node has, for "start something over there".
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct RemoteProject {
    pub key: ProjectKey,
    pub name: String,
    /// The path on the owning node. Opaque here: it is what you hand back in
    /// [`AgentCommand::NewSession`], not something to look at locally.
    pub cwd: String,
    /// Whether this node has a conversation open in it now.
    ///
    /// Meant something for exactly as long as this list was derived from live conversations: every
    /// project on it had one, so every one of them said `true` and a board drawing the flag drew
    /// one colour. A node now advertises the places it *works in* — which is the list its own panel
    /// shows, and includes the project you cleared out this morning — so the two states exist and
    /// the flag is worth reading.
    #[serde(default)]
    pub active: bool,
    /// How many conversations are open in it there.
    #[serde(default)]
    pub sessions: u32,
    /// How many of those are mid-turn.
    ///
    /// Separate from `sessions` because they are different questions and a board answers both in
    /// one row: `3 conversations · 1 working` is what the sidebar says about a local project, and a
    /// remote one should not have to say less.
    #[serde(default)]
    pub running: u32,
}

/// What a node is willing to have done to it.
///
/// Sent in the handshake, so a board can grey out a verb rather than offering one that will be
/// refused. Advisory to the *caller*; the owner enforces regardless, because a capability list is
/// something the other side could have lied about.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct NodeCapabilities {
    /// Whether [`AgentCommand::Send`] and friends are accepted at all. `false` makes this node
    /// visible but read-only — a perfectly reasonable thing for a build machine to be.
    #[serde(default)]
    pub accepts_commands: bool,
    /// Whether [`AgentCommand::Approve`] is accepted. Separate from `accepts_commands` because it
    /// is a different kind of trust: steering an agent is a message, approving one is a write.
    #[serde(default)]
    pub accepts_approvals: bool,
    /// Whether [`AscpMessage::Subscribe`] will produce anything.
    #[serde(default)]
    pub streams: bool,
    /// Whether [`AscpMessage::Browse`] will be answered.
    ///
    /// Not a permission — it follows `accepts_commands`, for the reason written on `Browse` — but a
    /// *compatibility* flag, and that is why it is here rather than derived. A node built before
    /// `Browse` existed cannot skip a message it has never heard of: the frame parses or the
    /// connection fails, so a new node that sent one on spec would drop an old peer's link every
    /// time somebody opened a directory picker. It defaults to `false`, which is exactly what an
    /// older node's handshake decodes to, so "does not say" and "cannot" are the same answer.
    #[serde(default)]
    pub browse: bool,
    /// The checkouts this node has, for starting something on it.
    #[serde(default)]
    pub projects: Vec<RemoteProject>,
}

/// What a subscriber is told while watching one agent.
///
/// The same events the owning node's own frontend gets, minus anything a viewer cannot act on.
/// Enough to render a live transcript: that is the point, because "it feels like the agent is on
/// this computer" is a claim about what you can see, not about where the process is.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "event", rename_all = "snake_case")]
#[ts(export)]
pub enum StreamEvent {
    /// Everything said so far, sent once when the subscription opens.
    ///
    /// A subscriber that only received deltas would show an empty conversation until the next token
    /// arrived, which for an idle agent is never.
    History { messages: Vec<Message> },
    TurnStarted { turn: String },
    Token { turn: String, text: String },
    Thinking { turn: String, text: String },
    /// A driver's account of its own loop — sub-agents, plans, compaction. Carried separately from
    /// `Token` here for the same reason it is locally: it is not a content block.
    Activity { turn: String, activity: Activity },
    TurnEnded { turn: String, stop_reason: StopReason, usage: Usage },
    /// The agent is waiting for a person. Carries what is being asked so a remote viewer can answer
    /// it, if the owner accepts approvals.
    Blocked { turn: String, prompt: String },
}

/// Why a command was not carried out.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[ts(export)]
pub enum Refusal {
    /// No such agent here — deleted, or never was.
    NoSuchAgent,
    /// This node does not accept that kind of command. Not an error: it is the answer.
    NotPermitted { what: String },
    /// The owner could not do it right now.
    Busy { message: String },
    Failed { message: String },
}

/// Everything that goes over the wire.
///
/// One enum rather than a request and a response type, because the connection is symmetric: after
/// the handshake either side may send any of these, and a node is a peer rather than a client or a
/// server. Which side dialled is an accident of who booted first.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum AscpMessage {
    /// Opens a connection. The dialler goes first.
    ///
    /// `nonce` is random and is what the other side has to sign, which is what makes this an
    /// authentication rather than an announcement: anybody can claim a [`NodeId`], and only the
    /// holder of the key can sign a number they did not choose.
    Hello {
        version: u32,
        node: NodeInfo,
        capabilities: NodeCapabilities,
        /// Hex, 32 bytes.
        nonce: String,
    },
    /// The answer: who I am, proof I hold my key, and a challenge of my own.
    Welcome {
        version: u32,
        node: NodeInfo,
        capabilities: NodeCapabilities,
        /// Hex ed25519 signature over the dialler's `nonce`.
        signature: String,
        nonce: String,
    },
    /// The dialler's half of the proof. After this both sides are authenticated.
    Proof { signature: String },
    /// I am still here. Also carries anything about the node that can change while it runs.
    Presence {
        node: NodeInfo,
        capabilities: NodeCapabilities,
        /// Monotonic per node, so a board can drop a message that overtook another.
        ///
        /// Not a timestamp: clocks on different machines disagree by more than the interval
        /// between two of these, and "the newer one wins" decided by wall clock is how a stale
        /// inventory permanently wins over a fresh one.
        seq: u64,
    },
    /// What this node is running.
    ///
    /// `full` marks a complete snapshot, which replaces whatever the receiver had; otherwise this
    /// is a delta and `agents` are additions-or-updates while `gone` are removals. A node sends a
    /// full snapshot on connect and after anything it could not express as a delta.
    Inventory {
        seq: u64,
        #[serde(default)]
        full: bool,
        #[serde(default)]
        agents: Vec<AgentSummary>,
        #[serde(default)]
        gone: Vec<SessionId>,
    },
    /// Start sending me [`StreamEvent`]s for this agent. Idempotent.
    Subscribe { session: SessionId },
    Unsubscribe { session: SessionId },
    Stream { session: SessionId, event: StreamEvent },
    /// Do something with an agent you own.
    ///
    /// `id` correlates the answer. Every command is answered exactly once, with [`Self::Ack`] or
    /// [`Self::Refused`] — a command that could time out silently is a keystroke you cannot tell
    /// from a dropped connection.
    Command { id: String, session: SessionId, command: AgentCommand },
    Ack {
        id: String,
        /// The conversation a [`AgentCommand::NewSession`] created, when that is what was asked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
    },
    Refused { id: String, refusal: Refusal },
    /// Which directories on that machine start with `prefix`.
    ///
    /// [`NodeCapabilities::projects`] is what a node *offers*, and it is a short list of places it
    /// already works in. This is the other half: the directory over there that neither machine has
    /// ever opened, which without this can only be reached by typing a path from memory and finding
    /// out it was wrong when the conversation fails to start.
    ///
    /// Gated on [`NodeCapabilities::accepts_commands`], and deliberately not on a knob of its own.
    /// A node that accepts commands already accepts [`AgentCommand::NewSession`] with an arbitrary
    /// `cwd` — it will *start an agent* anywhere on its disk you name — so declining to say which
    /// directories exist would withhold strictly less than it has already given away, at the cost
    /// of a third permission for people to reason about. Directory names only: never files, never
    /// contents, never anything a `read_dir` cannot answer.
    ///
    /// Answered with [`Self::Browsed`] or [`Self::Refused`], exactly once, like every other
    /// request here.
    Browse { id: String, prefix: String },
    /// The answer to [`Self::Browse`] — each entry as the asker would have typed it, trailing
    /// separator and all, so it can go straight back into a field.
    Browsed { id: String, paths: Vec<String> },
    /// Closing cleanly, so a peer can say "went away" rather than waiting out a timeout.
    Goodbye {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every way of writing one repository has to land on one key, or a project is "on three
    /// computers" only when all three happened to clone it the same way.
    #[test]
    fn every_spelling_of_a_remote_is_the_same_project() {
        let want = ProjectKey("git:github.com/neoswarm/neosh".into());
        for url in [
            "git@github.com:neoswarm/neosh.git",
            "git@github.com:neoswarm/neosh",
            "https://github.com/neoswarm/neosh.git",
            "https://github.com/neoswarm/neosh",
            "ssh://git@github.com/neoswarm/neosh.git",
            "ssh://git@github.com:22/neoswarm/neosh.git",
            "https://someone:token@github.com/neoswarm/neosh.git",
            "https://github.com/neoswarm/neosh/",
        ] {
            assert_eq!(ProjectKey::from_remote(url).as_ref(), Some(&want), "{url}");
        }
    }

    /// Case is left alone. Folding it would merge two repositories that a case-sensitive forge
    /// considers separate, which is a wrong answer rather than an untidy one.
    #[test]
    fn case_is_not_folded() {
        assert_ne!(
            ProjectKey::from_remote("https://example.com/Ann/Repo"),
            ProjectKey::from_remote("https://example.com/ann/repo"),
        );
    }

    #[test]
    fn a_remote_that_says_nothing_is_no_key_at_all() {
        assert_eq!(ProjectKey::from_remote(""), None);
        assert_eq!(ProjectKey::from_remote("   "), None);
    }

    /// A directory name is a guess and says so, because "these two machines have the same project"
    /// derived from two directories both called `api` is a wrong answer stated confidently.
    #[test]
    fn a_directory_name_is_a_guess_and_admits_it() {
        let dir = ProjectKey::from_dir_name("api");
        assert!(!dir.is_certain());
        assert!(ProjectKey::from_remote("git@github.com:me/api.git").unwrap().is_certain());
    }

    /// The short form is for showing. Nothing may compare on it.
    #[test]
    fn a_short_id_is_a_prefix_and_survives_a_short_key() {
        assert_eq!(NodeId("0123456789abcdef".into()).short(), "01234567");
        assert_eq!(NodeId("abc".into()).short(), "abc");
        assert_eq!(NodeId(String::new()).short(), "");
    }

    /// A machine that predates [`AscpMessage::Browse`] has to decode as one that cannot answer it.
    ///
    /// This is the whole of what keeps a directory picker from knocking an older peer off the
    /// board. An unknown message tag fails the frame and fails the connection with it, so
    /// [`NodeCapabilities::browse`] is what a new node checks before ever sending one — and the
    /// handshake of a node that has never heard of the field says nothing about it. "Did not say"
    /// and "cannot" must therefore be the same answer, which is what `#[serde(default)]` on a
    /// `bool` buys and what this pins down.
    #[test]
    fn a_handshake_from_before_browse_existed_says_it_cannot_browse() {
        let older = r#"{
            "accepts_commands": true,
            "accepts_approvals": false,
            "streams": true,
            "projects": [
                {"key":"git:example.com/me/thing","name":"thing","cwd":"/home/me/thing","active":true}
            ]
        }"#;
        let caps: NodeCapabilities = serde_json::from_str(older).expect("an older handshake parses");
        assert!(!caps.browse, "and it is not asked to browse");
        assert!(caps.accepts_commands, "while everything it did say survives");
        // The counts are the same bargain one type down: absent is zero, not a parse failure.
        let p = &caps.projects[0];
        assert_eq!((p.sessions, p.running), (0, 0));
        assert!(p.active, "and the flag it did send is believed");
    }

    /// And the other direction: an older node is handed fields it has never heard of.
    ///
    /// Serde ignores unknown keys on a struct by default, which is the half of the bargain that
    /// lets a new node advertise `browse` at all — but "by default" is a setting somebody could
    /// change to `deny_unknown_fields` on a tidying pass, and that would break every mixed-version
    /// swarm at the handshake. Pinned here so the tidying pass fails a test instead.
    #[test]
    fn a_handshake_carrying_fields_an_older_node_lacks_still_parses() {
        let newer = r#"{
            "accepts_commands": true,
            "accepts_approvals": false,
            "streams": true,
            "browse": true,
            "something_from_next_year": 7,
            "projects": [
                {"key":"git:example.com/me/thing","name":"thing","cwd":"/t","active":false,
                 "sessions":3,"running":1,"another_new_one":"x"}
            ]
        }"#;
        let caps: NodeCapabilities = serde_json::from_str(newer).expect("a newer handshake parses");
        assert!(caps.browse);
        assert_eq!((caps.projects[0].sessions, caps.projects[0].running), (3, 1));
    }
}
