//! Asking a person, from inside a driver.
//!
//! An agent driver runs its own loop, so the calls it wants to make never reach neosh's tool
//! registry and never pass [`neosh_agent::tools::ToolCtx::permit`]. What they pass instead is a
//! question the *agent* asks its client — ACP's `session/request_permission` — and until this
//! existed, the only answers a driver could give were "yes, because the mode says full access" and
//! "no". So `ask` mode, the default, meant every write and every command was refused without
//! anybody being asked anything.
//!
//! The shape here is deliberately thin: a request, a set of answers the asker itself worded, and a
//! reply. Nothing about hooks, plugins or pickers, because a driver has no business knowing which
//! of those is on the other end — and a test can put a closure there instead.

use std::path::PathBuf;

use neosh_proto::{Capability, PermissionOption};

/// A question an agent driver is waiting on the answer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// What is about to happen, in the agent's own words. Shown as-is: the agent knows what it is
    /// doing and a paraphrase generated from the capability is always the worse sentence.
    pub title: String,
    /// The same thing as policy sees it, so [`neosh_agent::PermissionLayer`] can answer without a
    /// person where it already has an answer.
    pub capability: Capability,
    /// The answers the agent offers, in its order. Never invented here: an agent that offers
    /// "allow always for this command" means something specific by it, and a client that
    /// substituted its own two buttons would be answering a different question.
    pub options: Vec<PermissionOption>,
    /// Where the turn is running, so a relative path in the capability resolves against the right
    /// tree. Several conversations can be mid-turn in different projects at once.
    pub cwd: PathBuf,
    /// Which conversation is blocked on this.
    ///
    /// What [`crate::ask::QuestionRequest`] has always carried, and for the same reason: a
    /// workspace runs several turns at once, only one of them is on screen, and "somebody is
    /// waiting on you" is a fact about a conversation rather than about whichever panel drew the
    /// prompt. `None` where a plugin asked outside any turn.
    #[doc(alias = "session")]
    pub conversation: Option<neosh_proto::SessionId>,
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAnswer {
    /// Take this exact option, by its `id`.
    Option(String),
    /// Approved without naming an option — the caller takes the narrowest allow on offer.
    ///
    /// This is what a policy answer looks like: "full access" is a yes to the question, not a
    /// choice between the agent's four ways of saying yes.
    Allow,
    Deny,
}

/// Somebody who can answer a [`PermissionRequest`].
///
/// Fail closed is the caller's job, not this trait's: a driver with no asker refuses, and an asker
/// that hangs is bounded by whatever timeout it imposes on itself.
#[async_trait::async_trait]
pub trait PermissionAsker: Send + Sync + std::fmt::Debug {
    async fn ask(&self, request: PermissionRequest) -> PermissionAnswer;
}
